//! The batch geocoding flow: a file of addresses is read, the columns that
//! make up the address are picked, and the file is geocoded by the engine's
//! batch run, the same `open-geocode batch-geocode` does, into a new file
//! beside it.
//!
//! A row's address is its picked cells joined in file order into one line,
//! which the engine's parser reads like any query: there is no telling it
//! which cell is the street or the postcode.

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use anyhow::Result;
use open_geocode::batch::{
    BatchGeocodeOptions, BatchObserver, BatchRow, cells, detect_delimiter, header_names,
    open_delimited, run_batch_geocode, unnamed_columns,
};

use crate::engine::{Engine, Hit, name_of};

/// Rows of the file shown, and geocoded, before the run starts.
pub const PREVIEW_ROWS: usize = 3;
/// Rows the running screen lists, newest first.
const LATEST_ROWS: usize = 8;

/// Words of a header that name part of an address, in the languages
/// address files come in, and the codes of common registers.
const ADDRESS_WORDS: &[&str] = &[
    // the address, the street and the house number
    "address", "addr", "adresse", "adress", "indirizzo", "direccion", "endereco", "street",
    "strasse", "straße", "str", "stn", "rue", "via", "calle", "rua", "road", "weg", "gasse",
    "number", "nummer", "hausnummer", "housenumber", "house", "nr", "no", "num", "numero",
    "civico", "hnr", "unit", "suite", "apt",
    // the place
    "city", "town", "locality", "municipality", "municipal", "gemeinde", "ort", "ortschaft",
    "commune", "comune", "ville", "ciudad", "cidade", "suburb", "village",
    // the postcode
    "postcode", "postal", "zip", "zipcode", "plz", "npa", "cap", "cp", "postleitzahl",
    // the region and the country
    "region", "state", "province", "prov", "canton", "kanton", "county", "district",
    "country", "land", "pays", "paese",
];

/// Words of a header that name something else, which wins over the above:
/// identifiers, coordinates, dates and flags.
const OTHER_WORDS: &[&str] = &[
    "id", "uuid", "pid", "egid", "egaid", "esid", "edid", "fosnr", "bfs", "lat", "lon", "lng",
    "latitude", "longitude", "x", "y", "easting", "northing", "date", "modified", "created",
    "updated", "status", "official", "category", "type", "phone", "email", "url",
];

/// The words of a header: `ADR_NUMBER` is `adr` and `number`.
fn words(header: &str) -> Vec<String> {
    header
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether a header names part of an address. A municipality's or
/// commune's name counts; its number does not.
fn names_address(header: &str) -> bool {
    let words = words(header);
    let has = |list: &[&str]| words.iter().any(|word| list.contains(&word.as_str()));
    if has(OTHER_WORDS) {
        return false;
    }
    let place = words.iter().any(|word| matches!(word.as_str(), "com" | "gem" | "mun"))
        && words.iter().any(|word| matches!(word.as_str(), "name" | "label"));
    place || has(ADDRESS_WORDS)
}

fn is_number(cell: &str) -> bool {
    let cell = cell.trim();
    !cell.is_empty() && cell.parse::<f64>().is_ok()
}

/// Whether a first row names columns instead of holding data: some column
/// is a number in the second row but not in the first, or a cell of the
/// first row names part of an address.
fn looks_like_header(first: &[String], second: Option<&Vec<String>>) -> bool {
    let typed = second.is_some_and(|second| {
        first
            .iter()
            .zip(second)
            .any(|(name, value)| !is_number(name) && !name.is_empty() && is_number(value))
    });
    typed || first.iter().any(|cell| names_address(cell))
}

pub struct BatchFile {
    pub path: PathBuf,
    pub name: String,
    pub bytes: u64,
    pub headers: Vec<String>,
    pub header_found: bool,
    /// Rows of data, the header row not counted, as far as the count got.
    rows: Arc<AtomicU64>,
    /// Whether the count reached the end of the file.
    counted: Arc<AtomicBool>,
    /// The first rows of data.
    sample: Vec<Vec<String>>,
}

impl BatchFile {
    /// Reads a delimited file's header and first rows, and starts counting
    /// its rows in the background. None when the path is no file with at
    /// least one row.
    pub fn from_path(path: &Path) -> Option<Self> {
        let meta = path.metadata().ok()?;
        if !meta.is_file() {
            return None;
        }
        let delimiter = detect_delimiter(path).ok()?;
        let mut reader = open_delimited(path, delimiter).ok()?;
        let mut first: Vec<Vec<String>> = Vec::new();
        for record in reader.byte_records().take(PREVIEW_ROWS + 1) {
            let record = record.ok()?;
            let row = if first.is_empty() {
                header_names(&record)
            } else {
                cells(&record)
            };
            first.push(row.iter().map(|cell| cell.trim().to_string()).collect());
        }
        if first.is_empty() {
            return None;
        }
        let header_found = looks_like_header(&first[0], first.get(1));
        let headers = if header_found {
            first.remove(0)
        } else {
            unnamed_columns(path).ok()?
        };
        first.truncate(PREVIEW_ROWS);
        let rows = Arc::new(AtomicU64::new(0));
        let counted = Arc::new(AtomicBool::new(false));
        {
            let (rows, counted, path) = (Arc::clone(&rows), Arc::clone(&counted), path.to_path_buf());
            let skip = u64::from(header_found);
            thread::spawn(move || {
                let Ok(mut reader) = open_delimited(&path, delimiter) else {
                    return;
                };
                let mut record = csv::ByteRecord::new();
                let mut seen = 0u64;
                while reader.read_byte_record(&mut record).unwrap_or(false) {
                    seen += 1;
                    if seen % 65_536 == 0 {
                        rows.store(seen.saturating_sub(skip), Ordering::Relaxed);
                    }
                }
                rows.store(seen.saturating_sub(skip), Ordering::Relaxed);
                counted.store(true, Ordering::Relaxed);
            });
        }
        Some(Self {
            path: path.to_path_buf(),
            name: path.file_name()?.to_string_lossy().into_owned(),
            bytes: meta.len(),
            headers,
            header_found,
            rows,
            counted,
            sample: first,
        })
    }

    /// Rows of data counted so far.
    pub fn rows(&self) -> u64 {
        self.rows.load(Ordering::Relaxed)
    }

    /// Whether [`Self::rows`] is the whole file.
    pub fn counted(&self) -> bool {
        self.counted.load(Ordering::Relaxed)
    }

    /// The first rows, each with one cell per header.
    pub fn sample(&self) -> Vec<Vec<String>> {
        self.sample
            .iter()
            .map(|row| {
                let mut row = row.clone();
                row.resize(self.headers.len(), String::new());
                row
            })
            .collect()
    }

    /// The file name without its extension.
    pub fn stem(&self) -> &str {
        self.name
            .rsplit_once('.')
            .map_or(&self.name, |(stem, _)| stem)
    }

    /// Where the run writes: `<stem>.geocoded.csv` beside the file, numbered
    /// when that name is taken, so nothing is written over.
    fn output_path(&self) -> PathBuf {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let mut path = dir.join(format!("{}.geocoded.csv", self.stem()));
        let mut number = 2;
        while path.exists() {
            path = dir.join(format!("{}.geocoded-{number}.csv", self.stem()));
            number += 1;
        }
        path
    }
}

/// One row of the running screen.
pub struct Latest {
    pub query: String,
    pub hit: Option<(String, f64, f64)>,
    pub millis: f64,
}

/// What a run has done so far, shared with its thread.
#[derive(Default)]
pub struct RunStats {
    pub done: AtomicU64,
    pub matched: AtomicU64,
    pub accuracy: Mutex<BTreeMap<String, u64>>,
    pub latencies: Mutex<Vec<f32>>,
    pub latest: Mutex<VecDeque<Latest>>,
}

impl BatchObserver for RunStats {
    fn row(&self, row: &BatchRow) {
        let millis = row.elapsed.as_secs_f64() * 1_000.0;
        self.done.fetch_add(1, Ordering::Relaxed);
        if let Some(feature) = row.feature {
            self.matched.fetch_add(1, Ordering::Relaxed);
            *self
                .accuracy
                .lock()
                .expect("accuracy lock")
                .entry(name_of(&feature.properties.accuracy))
                .or_default() += 1;
        }
        self.latencies
            .lock()
            .expect("latencies lock")
            .push(millis as f32);
        let mut latest = self.latest.lock().expect("latest lock");
        latest.push_front(Latest {
            query: row.query.to_string(),
            hit: row.feature.map(|feature| {
                (
                    feature.properties.name.clone(),
                    feature.lat(),
                    feature.lon(),
                )
            }),
            millis,
        });
        latest.truncate(LATEST_ROWS);
    }
}

impl RunStats {
    /// Median, 95th and 99th percentile and the slowest, in milliseconds.
    pub fn latency(&self) -> (f64, f64, f64, f64) {
        let mut latencies = self.latencies.lock().expect("latencies lock").clone();
        latencies.sort_by(f32::total_cmp);
        let at = |q: f64| {
            if latencies.is_empty() {
                0.0
            } else {
                f64::from(latencies[((latencies.len() - 1) as f64 * q).round() as usize])
            }
        };
        (at(0.50), at(0.95), at(0.99), at(1.0))
    }
}

/// What a finished run found.
pub struct Summary {
    pub rows: u64,
    pub matched: u64,
    pub by_accuracy: Vec<(String, u64)>,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub slowest: f64,
    pub seconds: f64,
    pub output: PathBuf,
    pub output_bytes: u64,
    /// The output's first rows, header first.
    pub output_head: Vec<Vec<String>>,
}

pub struct Run {
    pub stats: Arc<RunStats>,
    pub started: Instant,
    pub output: PathBuf,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<()>>>,
}

impl Run {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// The run's outcome, once it is finished.
    pub fn take(&mut self) -> Option<Result<()>> {
        if !self.is_finished() {
            return None;
        }
        let handle = self.handle.take()?;
        Some(
            handle
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("the run stopped unexpectedly"))),
        )
    }
}

/// A run let go of is stopped, and waited for, so its temporary output
/// file is removed before og exits.
impl Drop for Run {
    fn drop(&mut self) {
        self.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub struct BatchSession {
    pub file: BatchFile,
    /// Which columns make up the address.
    pub used: Vec<bool>,
    /// The column the setup cursor is on.
    pub column: usize,
    pub run: Option<Run>,
    pub summary: Option<Summary>,
}

impl BatchSession {
    pub fn new(file: BatchFile) -> Self {
        let mut used: Vec<bool> = file.headers.iter().map(|header| names_address(header)).collect();
        // A file whose headers say nothing, such as one without a header
        // row, starts with every column in the address.
        if !used.contains(&true) {
            used.iter_mut().for_each(|on| *on = true);
        }
        let column = used.iter().position(|on| *on).unwrap_or(0);
        Self {
            file,
            used,
            column,
            run: None,
            summary: None,
        }
    }

    fn columns(&self) -> Vec<String> {
        self.used
            .iter()
            .zip(&self.file.headers)
            .filter(|(on, _)| **on)
            .map(|(_, header)| header.clone())
            .collect()
    }

    /// The address searched for a row: its picked cells in file order,
    /// joined as the run joins them.
    pub fn query(&self, row: &[String]) -> String {
        self.used
            .iter()
            .zip(row)
            .filter(|(on, _)| **on)
            .map(|(_, cell)| cell.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|cell| !cell.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// What the run would answer for a row.
    pub fn lookup(&self, engine: &Engine, row: &[String]) -> Option<Hit> {
        let address = self.query(row);
        if address.is_empty() {
            return None;
        }
        engine.geocode_address(address).ok().flatten()
    }

    pub fn has_address(&self) -> bool {
        self.used.contains(&true)
    }

    /// Starts geocoding the whole file.
    pub fn start(&mut self, pack: &Path) {
        let output = self.file.output_path();
        let stats = Arc::new(RunStats::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let options = BatchGeocodeOptions {
            pack: Some(pack.to_path_buf()),
            input: self.file.path.clone(),
            output: output.clone(),
            header_row: self.file.header_found,
            address_field_groups: vec![self.columns()],
            observer: Some(Arc::clone(&stats) as Arc<dyn BatchObserver>),
            cancel: Arc::clone(&cancel),
            ..BatchGeocodeOptions::default()
        };
        let handle = thread::spawn(move || run_batch_geocode(options).map(|_| ()));
        self.summary = None;
        self.run = Some(Run {
            stats,
            started: Instant::now(),
            output,
            cancel,
            handle: Some(handle),
        });
    }

    /// Totals for the done screen, from the finished run and its output.
    pub fn summarize(&mut self) {
        let Some(run) = &self.run else {
            return;
        };
        let stats = &run.stats;
        let (p50, p95, p99, slowest) = stats.latency();
        let order = [
            "rooftop",
            "interpolated",
            "street",
            "centroid",
            "approximate",
        ];
        let mut by_accuracy: Vec<(String, u64)> = stats
            .accuracy
            .lock()
            .expect("accuracy lock")
            .iter()
            .map(|(name, count)| (name.clone(), *count))
            .collect();
        by_accuracy.sort_by_key(|(name, _)| order.iter().position(|own| own == name));
        let output_head = detect_delimiter(&run.output)
            .and_then(|delimiter| open_delimited(&run.output, delimiter))
            .map(|mut reader| {
                reader
                    .byte_records()
                    .take(PREVIEW_ROWS + 1)
                    .filter_map(Result::ok)
                    .map(|record| cells(&record))
                    .collect()
            })
            .unwrap_or_default();
        self.summary = Some(Summary {
            rows: stats.done.load(Ordering::Relaxed),
            matched: stats.matched.load(Ordering::Relaxed),
            by_accuracy,
            p50,
            p95,
            p99,
            slowest,
            seconds: run.started.elapsed().as_secs_f64(),
            output: run.output.clone(),
            output_bytes: fs::metadata(&run.output).map_or(0, |meta| meta.len()),
            output_head,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_address_columns_of_a_register() {
        let headers = [
            "ADR_EGAID", "STR_ESID", "BDG_EGID", "ADR_EDID", "STN_LABEL", "ADR_NUMBER",
            "BDG_CATEGORY", "BDG_NAME", "ZIP_LABEL", "COM_FOSNR", "COM_NAME", "COM_CANTON",
            "ADR_STATUS", "ADR_OFFICIAL", "ADR_MODIFIED", "ADR_EASTING", "ADR_NORTHING",
        ];
        let picked: Vec<&str> = headers
            .into_iter()
            .filter(|header| names_address(header))
            .collect();
        assert_eq!(
            picked,
            ["STN_LABEL", "ADR_NUMBER", "ZIP_LABEL", "COM_NAME", "COM_CANTON"]
        );
        for header in ["address", "Street Address", "city", "Postal Code", "id", "lat"] {
            assert_eq!(
                names_address(header),
                !matches!(header, "id" | "lat"),
                "{header}"
            );
        }
    }

    #[test]
    fn tells_a_header_from_data() {
        let row = |cells: &[&str]| cells.iter().map(|cell| cell.to_string()).collect::<Vec<_>>();
        let codes = row(&["ADR_EGAID", "BDG_EGID"]);
        assert!(looks_like_header(&codes, Some(&row(&["102435015", "243056020"]))));
        let addresses = row(&["100 queen st w", "toronto"]);
        assert!(!looks_like_header(&addresses, Some(&row(&["24 sussex dr", "ottawa"]))));
    }
}
