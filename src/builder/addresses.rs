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
    /// The files to read for a Pack covering `coverage`. Called once, after
    /// the boundaries are built; it may download.
    fn files(&self, coverage: &Coverage) -> Result<Vec<AddressFile>>;
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
}

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
    Ok(ZipEntry {
        data_start: found.data_start(),
        compressed_bytes: found.compressed_size(),
        deflated,
    })
}

/// Fixed files, whatever the Pack covers: the `--addresses` a build is given.
#[derive(Debug, Clone, Default)]
pub struct AddressFiles(pub Vec<AddressFile>);

impl AddressSource for AddressFiles {
    fn files(&self, _coverage: &Coverage) -> Result<Vec<AddressFile>> {
        Ok(self.0.clone())
    }
}

/// Several sources, read in order.
#[derive(Debug, Clone, Default)]
pub struct AddressSources(pub Vec<std::sync::Arc<dyn AddressSource>>);

impl AddressSource for AddressSources {
    fn files(&self, coverage: &Coverage) -> Result<Vec<AddressFile>> {
        let mut files = Vec::new();
        for source in &self.0 {
            files.extend(source.files(coverage)?);
        }
        Ok(files)
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
    mut accept: impl FnMut(AddressRecord) -> Result<Option<&'static str>>,
) -> Result<ImportReport> {
    let mut report = ImportReport {
        dataset: file.dataset.clone(),
        path: file.describe(),
        bytes: file.bytes()?,
        ..ImportReport::default()
    };
    let mut row_number = 0i64;
    let mut take = |row: std::result::Result<Row, &'static str>| -> Result<()> {
        row_number += 1;
        report.rows += 1;
        match row.and_then(|row| address(row, &file.dataset, row_number)) {
            Ok(record) => {
                match accept(record)? {
                    None => report.accepted += 1,
                    Some(reason) => *report.rejected.entry(reason.to_string()).or_default() += 1,
                }
                Ok(())
            }
            Err(reason) => {
                *report.rejected.entry(reason.to_string()).or_default() += 1;
                Ok(())
            }
        }
    };
    let (input, named) = match &file.entry {
        Some(entry) => (open_entry(&file.path, entry)?, Path::new(entry.as_str())),
        None => (open(&file.path)?, file.path.as_path()),
    };
    match layout(named)? {
        Layout::GeoJson => {
            for line in input.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                take(
                    serde_json::from_str::<Feature>(&line)
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
                        .map_err(|_| "malformed_row"),
                )?;
            }
        }
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
            for record in reader.records() {
                let Ok(record) = record else {
                    take(Err("malformed_row"))?;
                    continue;
                };
                let text = |index: Option<usize>| {
                    index
                        .and_then(|index| record.get(index))
                        .map(str::to_string)
                };
                take(Ok(Row {
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
                }))?;
            }
        }
    }
    Ok(report)
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
fn open_entry(path: &Path, entry: &str) -> Result<Box<dyn BufRead>> {
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
fn open(path: &Path) -> Result<Box<dyn BufRead>> {
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
            locality: text(properties.city).or(text(properties.district)),
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
                },
                |_| Ok(None)
            )
            .is_err()
        );
    }
}
