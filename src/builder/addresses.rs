//! Address points from outside OSM, such as OpenAddresses, added to a build.
//!
//! A build asks its [`AddressSource`] for files once it knows which countries
//! the Pack covers, then reads each one into the same street groups as the
//! OSM addresses. An imported address that repeats an OSM one (same street,
//! number and unit, close by) is dropped: the Pack keeps one record per house,
//! and marks it disputed when the two disagree on where it is.
//!
//! Two layouts are read, either one gzipped or not, from a file of their own
//! or from an entry of a zip archive (the collection OpenAddresses publishes):
//!
//! - OpenAddresses GeoJSON, one feature per line, with the properties
//!   `number`, `street`, `unit`, `city`, `district`, `region` and `postcode`.
//! - CSV with a header row naming `LON`, `LAT`, `NUMBER` and `STREET`, and
//!   optionally `UNIT`, `CITY`, `DISTRICT`, `REGION` and `POSTCODE`, in any
//!   case and order: the OpenAddresses CSV layout.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::File,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    error::{Error, Result},
    record::{
        AddressComponents, AddressRecord, LocationPrecision, SourceProvenance, point_geometry,
    },
};

/// What a Pack covers, from the boundaries it has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Countries covered whole, ISO 3166-1 alpha-2 in upper case: `NZ`.
    pub countries: BTreeSet<String>,
    /// Regions of the countries covered in part, ISO 3166-2 in upper case:
    /// `CA-ON` for a Pack of Ontario.
    pub subdivisions: BTreeSet<String>,
}

/// Where a build finds addresses to add to the input's own.
pub trait AddressSource: Send + Sync + fmt::Debug {
    /// The files to read for a Pack covering `coverage`, and the data sets
    /// left out. Called once, after the boundaries are built; it may
    /// download.
    fn pick(&self, coverage: &Coverage) -> Result<Picked>;
}

/// What an [`AddressSource`] gives a build: the files to read, and each data
/// set it left out with the reason, for the build to warn of.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picked {
    pub files: Vec<AddressFile>,
    pub left_out: Vec<String>,
}

/// One file of address points and the dataset its records name as their
/// source, such as `openaddresses:nz/countrywide`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AddressFile {
    pub dataset: String,
    pub path: PathBuf,
    /// The file's name inside `path` when `path` is a zip archive, read from
    /// the archive without unpacking it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    /// The ISO 3166 code of the country (`JP`) or region (`CA-ON`) the
    /// file's addresses lie in, when the source says. Its rows are checked
    /// against that area's boundary: see [`SAMPLE_ROWS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
}

/// Rows of a file with an [`AddressFile::area`] read before any is taken. A
/// file most of whose first rows lie outside its area is a broken source,
/// such as one with longitude and latitude swapped, and none of its rows is
/// taken: the few its error lands inside the area are as wrong as the rest.
/// The first rows tell, since a source is broken throughout or not at all.
pub const SAMPLE_ROWS: usize = 1_000;

impl AddressFile {
    /// Bytes read to import the file: the entry's compressed bytes inside an
    /// archive, else the file's.
    pub fn bytes(&self) -> Result<u64> {
        match &self.entry {
            Some(entry) => Ok(zip_entry(&self.path, entry)?.compressed_bytes),
            None => std::fs::metadata(&self.path)
                .map(|metadata| metadata.len())
                .map_err(|source| Error::IoPath {
                    action: "failed to read",
                    path: self.path.clone(),
                    source,
                }),
        }
    }

    /// The file as a report names it: `archive.zip!entry` for an entry.
    pub fn describe(&self) -> String {
        match &self.entry {
            Some(entry) => format!("{}!{entry}", self.path.display()),
            None => self.path.display().to_string(),
        }
    }
}

/// Where an entry's data lies in a zip archive, and how it is stored.
struct ZipEntry {
    data_start: u64,
    compressed_bytes: u64,
    deflated: bool,
}

/// Find `entry` in the archive's central directory.
fn zip_entry(path: &Path, entry: &str) -> Result<ZipEntry> {
    let file = File::open(path).map_err(|source| Error::IoPath {
        action: "failed to open",
        path: path.to_path_buf(),
        source,
    })?;
    let bad = |detail: String| Error::BadInput(format!("{}: {detail}", path.display()));
    let mut archive = zip::ZipArchive::new(BufReader::new(file))
        .map_err(|error| bad(format!("not a readable zip archive: {error}")))?;
    let found = archive
        .by_name(entry)
        .map_err(|error| bad(format!("no entry {entry}: {error}")))?;
    let deflated = match found.compression() {
        zip::CompressionMethod::Deflated => true,
        zip::CompressionMethod::Stored => false,
        other => {
            return Err(bad(format!(
                "{entry} is compressed with {other}, not deflate"
            )));
        }
    };
    let data_start = found
        .data_start()
        .ok_or_else(|| bad(format!("{entry} has no local header")))?;
    Ok(ZipEntry {
        data_start,
        compressed_bytes: found.compressed_size(),
        deflated,
    })
}

/// Fixed files, whatever the Pack covers: the `--addresses` a build is given.
#[derive(Debug, Clone, Default)]
pub struct AddressFiles(pub Vec<AddressFile>);

impl AddressSource for AddressFiles {
    fn pick(&self, _coverage: &Coverage) -> Result<Picked> {
        Ok(Picked {
            files: self.0.clone(),
            left_out: Vec::new(),
        })
    }
}

/// Several sources, read in order.
#[derive(Debug, Clone, Default)]
pub struct AddressSources(pub Vec<std::sync::Arc<dyn AddressSource>>);

impl AddressSource for AddressSources {
    fn pick(&self, coverage: &Coverage) -> Result<Picked> {
        let mut picked = Picked::default();
        for source in &self.0 {
            let one = source.pick(coverage)?;
            picked.files.extend(one.files);
            picked.left_out.extend(one.left_out);
        }
        Ok(picked)
    }
}

/// What one imported file added to the build.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportReport {
    pub dataset: String,
    pub path: String,
    pub bytes: u64,
    pub rows: u64,
    /// Rows that became address records, before duplicates were dropped.
    pub accepted: u64,
    /// Rows left out, by reason.
    pub rejected: BTreeMap<String, u64>,
    /// Accepted rows dropped for repeating an address the Pack already has,
    /// an OSM one or one an earlier row or file stated, close to where it
    /// lies.
    pub duplicates: u64,
    /// Accepted rows dropped for naming a house the Pack already has, but
    /// farther from it than sources placing one house disagree. The house
    /// kept is marked disputed.
    pub disputes: u64,
}

#[derive(Debug, Default, Deserialize)]
struct Feature {
    geometry: Option<FeatureGeometry>,
    #[serde(default)]
    properties: Properties,
}

#[derive(Debug, Deserialize)]
struct FeatureGeometry {
    coordinates: Vec<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct Properties {
    number: Option<String>,
    street: Option<String>,
    unit: Option<String>,
    city: Option<String>,
    district: Option<String>,
    region: Option<String>,
    postcode: Option<String>,
}

/// One row as read, before it is checked.
#[derive(Debug, Default)]
struct Row {
    lon: Option<f64>,
    lat: Option<f64>,
    properties: Properties,
}

/// Read every row of `file`, handing each address it states to `accept`,
/// row numbers counting from 1. Rows that state no usable address are
/// counted in the report, not passed on, and so are the ones `accept` turns
/// down, under the reason it gives.
pub fn read_file(
    file: &AddressFile,
    accept: impl FnMut(AddressRecord) -> Result<Option<&'static str>>,
) -> Result<ImportReport> {
    read_rows(file, usize::MAX, |record| Ok(Ok(record)), accept)
}

/// Read every row of `file` as [`read_file`] does, with `prepare` run on each
/// address on every core before `accept` takes the rows in file order. A row
/// `prepare` turns down is counted under the reason it gives.
pub fn read_file_prepared<P: Send>(
    file: &AddressFile,
    prepare: impl Fn(AddressRecord) -> Result<std::result::Result<P, &'static str>> + Sync,
    accept: impl FnMut(P) -> Result<Option<&'static str>>,
) -> Result<ImportReport> {
    read_rows(file, usize::MAX, prepare, accept)
}

/// The points of the addresses the first `rows` rows of `file` state.
pub fn first_points(file: &AddressFile, rows: usize) -> Result<Vec<[f64; 2]>> {
    let mut points = Vec::new();
    read_rows(
        file,
        rows,
        |record| Ok(Ok(record)),
        |record| {
            points.extend(crate::util::geo::point_lon_lat(&record.geometry));
            Ok(None)
        },
    )?;
    Ok(points)
}

/// Rows read and handed to the pool at once.
const ROW_BATCH: usize = 1 << 14;

/// A row as the reading thread hands it on. A GeoJSON line is parsed on the
/// pool, a CSV record by the reader that splits it.
enum Unparsed {
    Line(Vec<u8>),
    /// Boxed, as a split row is several times a line's size.
    Row(Box<std::result::Result<Row, &'static str>>),
}

fn read_rows<P: Send>(
    file: &AddressFile,
    limit: usize,
    prepare: impl Fn(AddressRecord) -> Result<std::result::Result<P, &'static str>> + Sync,
    mut accept: impl FnMut(P) -> Result<Option<&'static str>>,
) -> Result<ImportReport> {
    let mut report = ImportReport {
        dataset: file.dataset.clone(),
        path: file.describe(),
        bytes: file.bytes()?,
        ..ImportReport::default()
    };
    let (input, named) = match &file.entry {
        Some(entry) => (open_entry(&file.path, entry)?, Path::new(entry.as_str())),
        None => (open(&file.path)?, file.path.as_path()),
    };
    let describe = file.describe();
    let rows: Box<dyn Iterator<Item = Result<Unparsed>> + Send> = match layout(named)? {
        // Split as bytes: one row of another encoding is that row's
        // rejection, not the end of the build.
        Layout::GeoJson => Box::new(input.split(b'\n').take(limit).filter_map(
            move |line| match line {
                Ok(line) if line.trim_ascii().is_empty() => None,
                Ok(line) => Some(Ok(Unparsed::Line(line))),
                Err(source) => Some(Err(Error::IoPath {
                    action: "failed to read",
                    path: PathBuf::from(describe.clone()),
                    source,
                })),
            },
        )),
        Layout::Csv => {
            let mut reader = csv::ReaderBuilder::new().flexible(true).from_reader(input);
            let headers = reader
                .headers()
                .map_err(|error| Error::BadInput(format!("{}: {error}", file.path.display())))?
                .iter()
                .map(str::to_ascii_lowercase)
                .collect::<Vec<_>>();
            let column = |name: &str| headers.iter().position(|header| header == name);
            let [lon, lat, number, street] = ["lon", "lat", "number", "street"].map(column);
            let (Some(lon), Some(lat), Some(number), Some(street)) = (lon, lat, number, street)
            else {
                return Err(Error::BadInput(format!(
                    "{} needs the columns LON, LAT, NUMBER and STREET",
                    file.path.display()
                )));
            };
            let [unit, city, district, region, postcode] =
                ["unit", "city", "district", "region", "postcode"].map(column);
            Box::new(reader.into_records().take(limit).map(move |record| {
                let record = match record {
                    Ok(record) => record,
                    Err(error) => {
                        return Ok(Unparsed::Row(Box::new(Err(match error.kind() {
                            csv::ErrorKind::Utf8 { .. } => "invalid_utf8",
                            _ => "malformed_row",
                        }))));
                    }
                };
                let text = |index: Option<usize>| {
                    index
                        .and_then(|index| record.get(index))
                        .map(str::to_string)
                };
                Ok(Unparsed::Row(Box::new(Ok(Row {
                    lon: record.get(lon).and_then(|value| value.trim().parse().ok()),
                    lat: record.get(lat).and_then(|value| value.trim().parse().ok()),
                    properties: Properties {
                        number: text(Some(number)),
                        street: text(Some(street)),
                        unit: text(unit),
                        city: text(city),
                        district: text(district),
                        region: text(region),
                        postcode: text(postcode),
                    },
                }))))
            }))
        }
    };
    let dataset = file.dataset.as_str();
    let prepare = &prepare;
    // Inflating and splitting a file is one stream, but each row parses and
    // checks on its own. One thread reads the next batch while the pool
    // readies this one, and rows are taken in file order.
    std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let reader = scope.spawn(move || -> Result<()> {
            let mut rows = rows;
            loop {
                let batch = rows.by_ref().take(ROW_BATCH).collect::<Result<Vec<_>>>()?;
                if batch.is_empty() || sender.send(batch).is_err() {
                    return Ok(());
                }
            }
        });
        // Dropping the receiver on an error here stops the reader at its
        // next batch.
        let mut row_number = 0i64;
        for batch in receiver {
            let first = row_number + 1;
            row_number += i64::try_from(batch.len())?;
            let ready = batch
                .into_par_iter()
                .enumerate()
                .map(|(at, unparsed)| {
                    let number = first + i64::try_from(at)?;
                    let row = match unparsed {
                        Unparsed::Line(line) => parse_feature(&line),
                        Unparsed::Row(row) => *row,
                    };
                    match row.and_then(|row| address(row, dataset, number)) {
                        Ok(record) => prepare(record),
                        Err(reason) => Ok(Err(reason)),
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            for row in ready {
                report.rows += 1;
                let rejected = match row {
                    Ok(prepared) => accept(prepared)?,
                    Err(reason) => Some(reason),
                };
                match rejected {
                    None => report.accepted += 1,
                    Some(reason) => *report.rejected.entry(reason.to_string()).or_default() += 1,
                }
            }
        }
        reader
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })?;
    Ok(report)
}

fn parse_feature(line: &[u8]) -> std::result::Result<Row, &'static str> {
    std::str::from_utf8(line)
        .map_err(|_| "invalid_utf8")
        .and_then(|line| serde_json::from_str::<Feature>(line).map_err(|_| "malformed_row"))
        .map(|feature| Row {
            lon: feature
                .geometry
                .as_ref()
                .and_then(|g| g.coordinates.first().copied()),
            lat: feature
                .geometry
                .as_ref()
                .and_then(|g| g.coordinates.get(1).copied()),
            properties: feature.properties,
        })
}

enum Layout {
    GeoJson,
    Csv,
}

fn layout(path: &Path) -> Result<Layout> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let name = name.strip_suffix(".gz").unwrap_or(&name);
    if name.ends_with(".geojson") || name.ends_with(".geojsonl") || name.ends_with(".json") {
        Ok(Layout::GeoJson)
    } else if name.ends_with(".csv") {
        Ok(Layout::Csv)
    } else {
        Err(Error::BadInput(format!(
            "{}: address files are .geojson or .csv, either gzipped or not",
            path.display()
        )))
    }
}

/// The text of `entry` in the zip archive at `path`, inflated as it is read.
/// The entry is read from its own offset in the archive, so a collection of
/// thousands of files is read one entry at a time and never unpacked.
fn open_entry(path: &Path, entry: &str) -> Result<Box<dyn BufRead + Send>> {
    use std::io::{Seek, SeekFrom};
    let ZipEntry {
        data_start,
        compressed_bytes,
        deflated,
    } = zip_entry(path, entry)?;
    let failed = |source| Error::IoPath {
        action: "failed to read",
        path: path.to_path_buf(),
        source,
    };
    let mut file = File::open(path).map_err(failed)?;
    file.seek(SeekFrom::Start(data_start)).map_err(failed)?;
    let data = BufReader::with_capacity(1 << 20, file).take(compressed_bytes);
    Ok(if deflated {
        Box::new(BufReader::with_capacity(
            1 << 20,
            flate2::read::DeflateDecoder::new(data),
        ))
    } else {
        Box::new(data)
    })
}

/// The file's text, gunzipped when it starts with the gzip magic.
fn open(path: &Path) -> Result<Box<dyn BufRead + Send>> {
    let failed = |source| Error::IoPath {
        action: "failed to open",
        path: path.to_path_buf(),
        source,
    };
    let mut magic = [0; 2];
    let gzipped = File::open(path)
        .map_err(failed)?
        .read(&mut magic)
        .map_err(failed)?
        == 2
        && magic == [0x1f, 0x8b];
    let file = File::open(path).map_err(failed)?;
    Ok(if gzipped {
        Box::new(BufReader::with_capacity(
            1 << 20,
            flate2::read::MultiGzDecoder::new(BufReader::new(file)),
        ))
    } else {
        Box::new(BufReader::with_capacity(1 << 20, file))
    })
}

fn address(
    row: Row,
    dataset: &str,
    row_number: i64,
) -> std::result::Result<AddressRecord, &'static str> {
    let text = |value: Option<String>| {
        value
            .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|value| !value.is_empty())
    };
    let properties = row.properties;
    let number = text(properties.number).ok_or("missing_number")?;
    let street = text(properties.street).ok_or("missing_street")?;
    let (Some(lon), Some(lat)) = (row.lon, row.lat) else {
        return Err("bad_coordinates");
    };
    if !(lon.is_finite() && lat.is_finite())
        || lon.abs() > 180.0
        || lat.abs() > 90.0
        || (lon == 0.0 && lat == 0.0)
    {
        return Err("bad_coordinates");
    }
    Ok(AddressRecord {
        disputed: false,
        street_postcodes: Vec::new(),
        address: AddressComponents {
            number,
            street: Some(street),
            place: None,
            unit: text(properties.unit),
            locality: text(properties.city).or_else(|| text(properties.district)),
            areas: Vec::new(),
            region: text(properties.region),
            postcode: text(properties.postcode),
            country: None,
        },
        geometry: point_geometry(lon, lat),
        location_precision: LocationPrecision::Point,
        source: SourceProvenance::imported(dataset, row_number),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, contents: &[u8]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("open-geocode-addresses-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(name);
        std::fs::write(&path, contents).expect("write");
        path
    }

    fn read(path: PathBuf) -> (Vec<AddressRecord>, ImportReport) {
        let mut records = Vec::new();
        let report = read_file(
            &AddressFile {
                dataset: "openaddresses:test".into(),
                path,
                entry: None,
                area: None,
            },
            |record| {
                records.push(record);
                Ok(None)
            },
        )
        .expect("read");
        (records, report)
    }

    #[test]
    fn reads_openaddresses_geojson_lines_gzipped() {
        let lines = concat!(
            r#"{"type":"Feature","properties":{"hash":"a","number":"12","street":"Queen  Street","unit":"","city":"","district":"Auckland","region":"","postcode":"1010","id":""},"geometry":{"type":"Point","coordinates":[174.76,-36.85]}}"#,
            "\n\n",
            r#"{"type":"Feature","properties":{"number":"","street":"Queen Street"},"geometry":{"type":"Point","coordinates":[174.76,-36.85]}}"#,
            "\n",
            r#"{"type":"Feature","properties":{"number":"14","street":"Queen Street"},"geometry":null}"#,
            "\n",
            "not json\n",
        );
        let mut gzipped = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gzipped, flate2::Compression::fast());
            std::io::Write::write_all(&mut encoder, lines.as_bytes()).expect("gzip");
            encoder.finish().expect("gzip");
        }
        let (records, report) = read(temp_file("source.geojson.gz", &gzipped));
        assert_eq!(records.len(), 1);
        let address = &records[0].address;
        assert_eq!(address.number, "12");
        assert_eq!(address.street.as_deref(), Some("Queen Street"));
        assert_eq!(address.unit, None);
        assert_eq!(
            address.locality.as_deref(),
            Some("Auckland"),
            "district stands in for city"
        );
        assert_eq!(address.postcode.as_deref(), Some("1010"));
        assert_eq!(
            records[0].source,
            SourceProvenance::imported("openaddresses:test", 1)
        );
        assert_eq!(report.rows, 4);
        assert_eq!(report.accepted, 1);
        assert_eq!(
            report.rejected,
            BTreeMap::from([
                ("bad_coordinates".to_string(), 1),
                ("malformed_row".to_string(), 1),
                ("missing_number".to_string(), 1),
            ])
        );
    }

    #[test]
    fn a_row_not_in_utf8_is_rejected_and_the_rows_after_it_are_read() {
        let feature = |number: &str, street: &[u8]| {
            [
                br#"{"type":"Feature","properties":{"number":""#.as_slice(),
                number.as_bytes(),
                br#"","street":""#,
                street,
                br#""},"geometry":{"type":"Point","coordinates":[6.13,49.61]}}"#,
                b"\n",
            ]
            .concat()
        };
        let geojson = [
            feature("1", b"Rue Test"),
            feature("2", b"Rue T\xe9st"),
            feature("3", b"Rue Test"),
        ]
        .concat();
        let csv = b"lon,lat,number,street\n6.13,49.61,1,Rue Test\n6.13,49.61,2,Rue T\xe9st\n6.13,49.61,3,Rue Test\n";
        for path in [
            temp_file("source.geojson", &geojson),
            temp_file("source.csv", csv),
        ] {
            let (records, report) = read(path);
            assert_eq!(
                records
                    .iter()
                    .map(|record| (record.address.number.as_str(), &record.source))
                    .collect::<Vec<_>>(),
                [
                    ("1", &SourceProvenance::imported("openaddresses:test", 1)),
                    ("3", &SourceProvenance::imported("openaddresses:test", 3)),
                ]
            );
            assert_eq!(report.rows, 3);
            assert_eq!(report.accepted, 2);
            assert_eq!(
                report.rejected,
                BTreeMap::from([("invalid_utf8".to_string(), 1)])
            );
        }
    }

    #[test]
    fn reads_openaddresses_csv_with_columns_in_any_order() {
        let csv = "street,NUMBER,Lat,lon,unit\nKing St,10,43.6,-79.4,2\nKing St,11,oops,-79.4,\n";
        let (records, report) = read(temp_file("addresses.csv", csv.as_bytes()));
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].address.unit.as_deref(), Some("2"));
        assert_eq!(
            report.rejected,
            BTreeMap::from([("bad_coordinates".to_string(), 1)])
        );

        let missing = temp_file("addresses.csv", b"lon,lat,number\n1,2,3\n");
        assert!(
            read_file(
                &AddressFile {
                    dataset: "x".into(),
                    path: missing,
                    entry: None,
                    area: None,
                },
                |_| Ok(None)
            )
            .is_err()
        );
    }

    #[test]
    fn rows_prepared_on_the_pool_are_taken_in_file_order() {
        // Past two batches, so rows from three batches must line up.
        let rows = 2 * ROW_BATCH + 3;
        let mut csv = String::from("lon,lat,number,street\n");
        for row in 1..=rows {
            let lat = if row % 7 == 0 { "oops" } else { "43.6" };
            csv.push_str(&format!("-79.4,{lat},{row},King St\n"));
        }
        let path = temp_file("addresses.csv", csv.as_bytes());
        let mut taken = Vec::new();
        let report = read_file_prepared(
            &AddressFile {
                dataset: "openaddresses:test".into(),
                path: path.clone(),
                entry: None,
                area: None,
            },
            |record| {
                let number = record.address.number.parse::<usize>().expect("number");
                Ok(if number % 3 == 0 {
                    Err("third")
                } else {
                    Ok((number, record.source))
                })
            },
            |prepared| {
                taken.push(prepared);
                Ok(None)
            },
        )
        .expect("read");
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));

        let expected = (1..=rows)
            .filter(|row| row % 7 != 0 && row % 3 != 0)
            .map(|row| {
                let id = i64::try_from(row).expect("row");
                (row, SourceProvenance::imported("openaddresses:test", id))
            })
            .collect::<Vec<_>>();
        assert_eq!(taken, expected);
        assert_eq!(report.rows, 32_771);
        assert_eq!(report.accepted, 18_727);
        assert_eq!(
            report.rejected,
            BTreeMap::from([
                ("bad_coordinates".to_string(), 4_681),
                ("third".to_string(), 9_363),
            ])
        );
    }
}
