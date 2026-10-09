//! Address rows from OpenAddresses-format CSV files.
//!
//! Rows are read one at a time and handed to the build in batches, so a file
//! of any size costs one batch of memory; the records then take the same
//! sorter, boundary context, postcode and text index path as OSM addresses.
//!
//! Header: `LON,LAT,NUMBER,STREET,UNIT,CITY,DISTRICT,REGION,POSTCODE`, matched
//! case-insensitively and in any order. `LON`, `LAT`, `NUMBER` and `STREET` are
//! required. `DISTRICT` and the `ID` and `HASH` columns of real OpenAddresses
//! files are accepted and ignored: a record is identified by `dataset:row`,
//! the row's 1-based position among the file's data rows.

use std::{
    collections::{BTreeMap, HashSet},
    mem,
    path::PathBuf,
    str::FromStr,
};

use anyhow::{Context, Result, bail};
use csv::ByteRecord;

use crate::{
    builder::report::CandidateIssue,
    record::{
        AddressComponents, AddressRecord, LocationPrecision, OsmObjectType, Record,
        SourceProvenance, point_geometry,
    },
    util::text::collapse_whitespace,
};

use super::emitted::Emitted;

/// Rows per batch handed to the build.
const BATCH_ROWS: u64 = 16_384;

/// A CSV file of addresses to add to the Pack, and the dataset name its
/// records carry in their ids (`dataset:row`) and provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressFile {
    pub dataset: String,
    pub path: PathBuf,
}

impl FromStr for AddressFile {
    type Err = String;

    /// `path`, or `dataset=path`. Without a dataset the file stem names it.
    fn from_str(value: &str) -> Result<Self, String> {
        let (dataset, path) = match value.split_once('=') {
            Some((dataset, path)) if is_dataset_name(dataset) => (Some(dataset), path),
            _ => (None, value),
        };
        let path = PathBuf::from(path);
        let dataset = match dataset {
            Some(dataset) => dataset.to_string(),
            None => path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .filter(|stem| is_dataset_name(stem))
                .map(str::to_string)
                .ok_or_else(|| {
                    format!(
                        "cannot derive a dataset name from {}; write it as dataset=path",
                        path.display()
                    )
                })?,
        };
        Ok(Self { dataset, path })
    }
}

fn is_dataset_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// Fail before any work on a file that is missing or a dataset name that would
/// make two records share an id.
pub(crate) fn validate(files: &[AddressFile]) -> Result<()> {
    let mut seen = HashSet::from(["osm"]);
    for file in files {
        if !file.path.is_file() {
            bail!("address file {} does not exist", file.path.display());
        }
        if !is_dataset_name(&file.dataset) || !seen.insert(file.dataset.as_str()) {
            bail!(
                "address dataset name {:?} is invalid or used twice (\"osm\" is reserved)",
                file.dataset
            );
        }
    }
    Ok(())
}

/// Column positions in the file's header.
struct Columns {
    lon: usize,
    lat: usize,
    number: usize,
    street: usize,
    unit: Option<usize>,
    city: Option<usize>,
    region: Option<usize>,
    postcode: Option<usize>,
}

impl Columns {
    fn from_header(header: &ByteRecord) -> Result<Self> {
        let find = |name: &str| {
            header.iter().position(|field| {
                let field = field.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(field);
                field.trim_ascii().eq_ignore_ascii_case(name.as_bytes())
            })
        };
        let required = |name: &str| {
            find(name).with_context(|| format!("address file has no {name} column in its header"))
        };
        Ok(Self {
            lon: required("LON")?,
            lat: required("LAT")?,
            number: required("NUMBER")?,
            street: required("STREET")?,
            unit: find("UNIT"),
            city: find("CITY"),
            region: find("REGION"),
            postcode: find("POSTCODE"),
        })
    }
}

/// Read every row of `file` and pass the resulting records and rejections to
/// `sink`, one batch at a time and in file order.
pub(crate) fn import_addresses(
    file: &AddressFile,
    mut sink: impl FnMut(Emitted) -> Result<()>,
) -> Result<()> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(&file.path)
        .with_context(|| format!("failed to open {}", file.path.display()))?;
    let columns = Columns::from_header(reader.byte_headers()?)
        .with_context(|| format!("failed to read {}", file.path.display()))?;
    let mut out = Emitted::default();
    let mut record = ByteRecord::new();
    let mut row = 0i64;
    while reader
        .read_byte_record(&mut record)
        .with_context(|| format!("failed to read row {} of {}", row + 1, file.path.display()))?
    {
        row += 1;
        out.report.scanned.address_rows += 1;
        // A stray quote opens a field that runs on over the following rows;
        // none of their data can be trusted, so the whole span is one rejection.
        if record.iter().any(|field| field.contains(&b'\n')) {
            let tags = BTreeMap::new();
            out.reject_in_report(
                CandidateIssue::MalformedRow,
                OsmObjectType::Row,
                row,
                &tags,
                Some(&tags),
                Some("address"),
            );
        } else {
            emit_row(&file.dataset, row, &columns, &record, &mut out);
        }
        if row as u64 % BATCH_ROWS == 0 {
            sink(mem::take(&mut out))?;
        }
    }
    sink(out)
}

fn emit_row(dataset: &str, row: i64, columns: &Columns, record: &ByteRecord, out: &mut Emitted) {
    let field =
        |column: Option<usize>| collapse_whitespace(&String::from_utf8_lossy(record.get(column?)?));
    let number = field(Some(columns.number));
    let street = field(Some(columns.street));
    let address = AddressComponents {
        number: number.clone().unwrap_or_default(),
        street: street.clone(),
        place: None,
        unit: field(columns.unit),
        locality: field(columns.city),
        region: field(columns.region),
        postcode: field(columns.postcode),
        country: None,
    };
    let point = coordinate(record, columns.lon, 180.0).zip(coordinate(record, columns.lat, 90.0));
    let issue = if number.is_none() {
        Some(CandidateIssue::MissingHouseNumber)
    } else if street.is_none() {
        Some(CandidateIssue::MissingStreetOrPlace)
    } else if point.is_none() {
        Some(CandidateIssue::InvalidCoordinates)
    } else {
        None
    };
    if let Some(issue) = issue {
        let tags = rejected_tags(&address);
        out.reject_in_report(
            issue,
            OsmObjectType::Row,
            row,
            &tags,
            Some(&tags),
            Some("address"),
        );
        return;
    }
    let (lon, lat) = point.expect("rows without coordinates were rejected");
    let record = AddressRecord {
        address,
        geometry: point_geometry(lon, lat),
        location_precision: LocationPrecision::Point,
        source: SourceProvenance::row(dataset, row),
    };
    out.report.accept_address_with_tags(&record, None);
    out.postcodes.accept_address(&record);
    out.records.push(Record::Address(record));
}

/// A finite coordinate within `[-limit, limit]`.
fn coordinate(record: &ByteRecord, column: usize, limit: f64) -> Option<f64> {
    let value = std::str::from_utf8(record.get(column)?)
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()?;
    (value.is_finite() && value.abs() <= limit).then_some(value)
}

/// The row's fields under OSM address keys, which is what the report's
/// rejection audit and samples read.
fn rejected_tags(address: &AddressComponents) -> BTreeMap<String, String> {
    [
        (
            "addr:housenumber",
            Some(&address.number).filter(|n| !n.is_empty()),
        ),
        ("addr:street", address.street.as_ref()),
        ("addr:unit", address.unit.as_ref()),
        ("addr:city", address.locality.as_ref()),
        ("addr:state", address.region.as_ref()),
        ("addr:postcode", address.postcode.as_ref()),
    ]
    .into_iter()
    .filter_map(|(key, value)| Some((key.to_string(), value?.clone())))
    .collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write};

    use super::*;

    fn import(csv: &str) -> Emitted {
        let path =
            std::env::temp_dir().join(format!("open-addresses-{}.csv", uuid::Uuid::new_v4()));
        fs::File::create(&path)
            .and_then(|mut file| file.write_all(csv.as_bytes()))
            .expect("write csv");
        let file = AddressFile {
            dataset: "linz".to_string(),
            path: path.clone(),
        };
        let mut all = Emitted::default();
        import_addresses(&file, |batch| {
            all.report.merge(batch.report);
            all.records.extend(batch.records);
            all.postcodes.merge(batch.postcodes);
            Ok(())
        })
        .expect("import");
        fs::remove_file(path).ok();
        all
    }

    #[test]
    fn reads_rows_with_any_header_case_order_and_extra_columns() {
        let out = import(
            "\u{feff}id,Street,NUMBER,lat,Lon,hash,Unit,city,district,region,postcode\n\
             a1,BARRINGER STREET,12,-43.5,172.6,h,3,Christchurch,,Canterbury,8023\n\
             a2,Elm Street,7B,-43.6,172.7,h,,,,,\n",
        );
        assert_eq!(out.report.scanned.address_rows, 2);
        let [Record::Address(first), Record::Address(second)] = out.records.as_slice() else {
            panic!("expected two addresses");
        };
        assert_eq!(first.id(), "linz:1");
        assert_eq!(first.address.number, "12");
        assert_eq!(first.address.street.as_deref(), Some("BARRINGER STREET"));
        assert_eq!(first.address.unit.as_deref(), Some("3"));
        assert_eq!(first.address.locality.as_deref(), Some("Christchurch"));
        assert_eq!(first.address.region.as_deref(), Some("Canterbury"));
        assert_eq!(first.address.postcode.as_deref(), Some("8023"));
        assert_eq!(second.id(), "linz:2");
        assert_eq!(second.address.postcode, None);
        assert_eq!(out.postcodes.len(), 1);
    }

    #[test]
    fn rejects_rows_without_number_street_or_coordinates() {
        let out = import(
            "LON,LAT,NUMBER,STREET\n\
             172.6,-43.5,,King Street\n\
             172.6,-43.5,10,\n\
             abc,-43.5,10,King Street\n\
             172.6,91,10,King Street\n\
             NaN,-43.5,10,King Street\n\
             172.6,-43.5,10,King Street\n",
        );
        assert_eq!(out.records.len(), 1);
        assert_eq!(out.report.scanned.address_rows, 6);
        assert_eq!(out.report.rejected.total, 5);
        let reasons = &out.report.rejected.by_reason;
        assert_eq!(reasons["missing_housenumber"], 1);
        assert_eq!(reasons["missing_street_or_place"], 1);
        assert_eq!(reasons["invalid_coordinates"], 3);
    }

    #[test]
    fn rejects_a_record_that_swallows_the_following_rows() {
        let out = import(
            "LON,LAT,NUMBER,STREET\n\
             1,2,5,\"Smith Rd\n\
             1,2,6,Elm St\"\n\
             \n\
             1,2,7,Oak St\n",
        );
        assert_eq!(out.report.scanned.address_rows, 2);
        assert_eq!(out.report.rejected.by_reason["malformed_row"], 1);
        assert_eq!(out.records.len(), 1, "a blank line is not a swallowed row");
    }

    #[test]
    fn requires_the_core_columns() {
        let path =
            std::env::temp_dir().join(format!("open-addresses-{}.csv", uuid::Uuid::new_v4()));
        fs::write(&path, "LON,LAT,NUMBER\n1,2,3\n").expect("write csv");
        let file = AddressFile {
            dataset: "x".to_string(),
            path: path.clone(),
        };
        let error = import_addresses(&file, |_| Ok(())).expect_err("no STREET column");
        fs::remove_file(path).ok();
        assert!(format!("{error:#}").contains("STREET"), "{error:#}");
    }

    #[test]
    fn parses_dataset_names() {
        let parse = |value: &str| value.parse::<AddressFile>().map(|file| file.dataset);
        assert_eq!(parse("gnaf=/data/au.csv"), Ok("gnaf".to_string()));
        assert_eq!(parse("/data/oa_nz.csv"), Ok("oa_nz".to_string()));
        assert_eq!(parse("/tmp/a=b/au.csv"), Ok("au".to_string()));
        assert!(parse("/data/bad name.csv").is_err());
    }
}
