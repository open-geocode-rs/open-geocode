//! The batch geocoding flow: a file of addresses is read, the columns that
//! make up the address are picked, and the file is geocoded by the engine's
//! batch run, the same `open-geocode batch-geocode` does, into a new file
//! beside it. Text pasted into the batch tab is read the same way.
//!
//! A row's address is its picked cells joined in file order into one line,
//! which the engine's parser reads like any query: there is no telling it
//! which cell is the street or the postcode.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
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
use open_geocode::{
    api::Accuracy,
    batch::{
        BatchGeocodeOptions, BatchObserver, BatchRow, Column, cells, delimiter_of,
        detect_delimiter, open_delimited, run_batch_geocode, unnamed_columns,
    },
};

use crate::engine::{Hit, name_of};

/// Rows of the file shown, and geocoded, before the run starts.
pub const PREVIEW_ROWS: usize = 3;
/// Rows the running screen lists, newest first.
const LATEST_ROWS: usize = 8;

/// Words of a header that name part of an address, in the languages
/// address files come in, and the codes of common registers.
const ADDRESS_WORDS: &[&str] = &[
    // the address, the street and the house number
    "address",
    "addr",
    "adresse",
    "adress",
    "indirizzo",
    "direccion",
    "endereco",
    "street",
    "strasse",
    "straße",
    "str",
    "stn",
    "rue",
    "via",
    "calle",
    "rua",
    "road",
    "weg",
    "gasse",
    "number",
    "nummer",
    "hausnummer",
    "housenumber",
    "house",
    "nr",
    "no",
    "num",
    "numero",
    "civico",
    "hnr",
    "unit",
    "suite",
    "apt",
    // the place
    "city",
    "town",
    "locality",
    "municipality",
    "municipal",
    "gemeinde",
    "ort",
    "ortschaft",
    "commune",
    "comune",
    "ville",
    "ciudad",
    "cidade",
    "suburb",
    "village",
    // the postcode
    "postcode",
    "postal",
    "zip",
    "zipcode",
    "plz",
    "npa",
    "cap",
    "cp",
    "postleitzahl",
    // the region and the country
    "region",
    "state",
    "province",
    "prov",
    "canton",
    "kanton",
    "county",
    "district",
    "country",
    "land",
    "pays",
    "paese",
    // a whole query
    "query",
    "q",
];

/// Words of a header that name something else, which wins over the above:
/// identifiers, coordinates, dates and flags.
const OTHER_WORDS: &[&str] = &[
    "id",
    "uuid",
    "pid",
    "egid",
    "egaid",
    "esid",
    "edid",
    "fosnr",
    "bfs",
    "lat",
    "lon",
    "lng",
    "latitude",
    "longitude",
    "x",
    "y",
    "easting",
    "northing",
    "date",
    "modified",
    "created",
    "updated",
    "status",
    "official",
    "category",
    "type",
    "phone",
    "email",
    "url",
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
    let place = words
        .iter()
        .any(|word| matches!(word.as_str(), "com" | "gem" | "mun"))
        && words
            .iter()
            .any(|word| matches!(word.as_str(), "name" | "label"));
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

/// The columns that make up the address, for columns named `headers`: those
/// whose name says so, or all of them when no name does, as in a file
/// without a header row.
fn address_columns(headers: &[String]) -> Vec<bool> {
    let used: Vec<bool> = headers.iter().map(|header| names_address(header)).collect();
    if used.contains(&true) {
        used
    } else {
        vec![true; headers.len()]
    }
}

/// The address of a row: the cells of the `used` columns with their
/// whitespace collapsed, in file order, joined as the run joins them.
fn address(used: &[bool], row: &[String]) -> String {
    used.iter()
        .zip(row)
        .filter(|(on, _)| **on)
        .map(|(_, cell)| cell.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|cell| !cell.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Reads pasted text as a file would be read: a first row that names its
/// columns picks the address columns, and every other row is one address.
/// Returns the addresses and a short note on how they were read.
pub fn pasted_addresses(text: &str) -> (Vec<String>, String) {
    let records: Vec<Vec<String>> = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(delimiter_of(text.as_bytes()))
        .from_reader(text.as_bytes())
        .byte_records()
        .filter_map(Result::ok)
        .map(|record| cells(&record))
        .collect();
    let header = records
        .first()
        .filter(|first| looks_like_header(first, records.get(1)));
    let (used, rows, note) = match header {
        Some(header) => {
            let used = address_columns(header);
            let picked: Vec<&str> = header
                .iter()
                .zip(&used)
                .filter(|(_, on)| **on)
                .map(|(name, _)| name.as_str())
                .collect();
            let note = format!("address from {}", picked.join(", "));
            (used, &records[1..], note)
        }
        None => {
            let width = records.iter().map(Vec::len).max().unwrap_or(0);
            (
                vec![true; width],
                &records[..],
                "one address per row".to_string(),
            )
        }
    };
    let addresses = rows
        .iter()
        .map(|row| address(&used, row))
        .filter(|address| !address.is_empty())
        .collect();
    (addresses, note)
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
    /// Set to stop the count when the file is let go of.
    stop: Arc<AtomicBool>,
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
            first.push(
                cells(&record.ok()?)
                    .iter()
                    .map(|cell| cell.trim().to_string())
                    .collect(),
            );
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
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (rows, counted, stop, path) = (
                Arc::clone(&rows),
                Arc::clone(&counted),
                Arc::clone(&stop),
                path.to_path_buf(),
            );
            let skip = u64::from(header_found);
            thread::spawn(move || {
                let Ok(mut reader) = open_delimited(&path, delimiter) else {
                    return;
                };
                let mut record = csv::ByteRecord::new();
                let mut seen = 0u64;
                while reader.read_byte_record(&mut record).unwrap_or(false) {
                    seen += 1;
                    if seen.is_multiple_of(65_536) {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
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
            stop,
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

    /// Where a run writes: `<stem>.geocoded.csv` beside the file, numbered
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

/// The count stops with the file: a path typed over is not read to its end.
impl Drop for BatchFile {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// One row of the running screen.
pub struct Latest {
    pub query: String,
    pub hit: Option<(String, f64, f64)>,
    pub millis: f64,
}

/// Sub-buckets per power of two of [`Latencies`]: each bucket is at most
/// an eighth wider than its lower bound.
const SUB_BUCKETS: u32 = 8;
/// Bits of the slowest time [`Latencies`] tells apart, in microseconds:
/// longer ones count as this long, about 19 hours.
const LATENCY_BITS: u32 = 36;
const LATENCY_BUCKETS: usize =
    (SUB_BUCKETS + (LATENCY_BITS - SUB_BUCKETS.ilog2()) * SUB_BUCKETS) as usize;

/// How long rows took, in buckets of fixed bounds: memory that does not
/// grow with the rows, and quantiles to within an eighth.
pub struct Latencies {
    buckets: [AtomicU64; LATENCY_BUCKETS],
    slowest_micros: AtomicU64,
}

impl Default for Latencies {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            slowest_micros: AtomicU64::new(0),
        }
    }
}

impl Latencies {
    /// The bucket of a time: one per microsecond below [`SUB_BUCKETS`],
    /// then [`SUB_BUCKETS`] for each power of two.
    fn bucket(micros: u64) -> usize {
        let micros = micros.min((1 << LATENCY_BITS) - 1);
        if micros < u64::from(SUB_BUCKETS) {
            return micros as usize;
        }
        let power = micros.ilog2();
        let shift = power - SUB_BUCKETS.ilog2();
        let within = (micros >> shift) - u64::from(SUB_BUCKETS);
        (SUB_BUCKETS + shift * SUB_BUCKETS) as usize + within as usize
    }

    /// The smallest time in a bucket.
    fn lower(bucket: usize) -> u64 {
        let bucket = bucket as u64;
        let sub = u64::from(SUB_BUCKETS);
        if bucket < sub {
            return bucket;
        }
        let shift = (bucket - sub) / sub;
        (sub + (bucket - sub) % sub) << shift
    }

    pub fn record(&self, millis: f64) {
        let micros = (millis * 1_000.0) as u64;
        self.buckets[Self::bucket(micros)].fetch_add(1, Ordering::Relaxed);
        self.slowest_micros.fetch_max(micros, Ordering::Relaxed);
    }

    /// The time below which a share `q` of the rows took, in milliseconds:
    /// the middle of its bucket.
    pub fn quantile(&self, q: f64) -> f64 {
        let counts: Vec<u64> = self
            .buckets
            .iter()
            .map(|bucket| bucket.load(Ordering::Relaxed))
            .collect();
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return 0.0;
        }
        let rank = ((total - 1) as f64 * q).round() as u64;
        let mut seen = 0;
        for (bucket, count) in counts.iter().enumerate() {
            seen += count;
            if seen > rank {
                let (low, high) = (Self::lower(bucket), Self::lower(bucket + 1));
                return (low + high) as f64 / 2.0 / 1_000.0;
            }
        }
        self.slowest()
    }

    pub fn slowest(&self) -> f64 {
        self.slowest_micros.load(Ordering::Relaxed) as f64 / 1_000.0
    }
}

/// What a run has done so far, shared with its thread.
#[derive(Default)]
pub struct RunStats {
    done: AtomicU64,
    matched: AtomicU64,
    accuracy: Mutex<BTreeMap<Accuracy, u64>>,
    pub latencies: Latencies,
    pub latest: Mutex<VecDeque<Latest>>,
}

impl BatchObserver for RunStats {
    fn row(&self, row: &BatchRow) {
        let millis = row.elapsed.as_secs_f64() * 1_000.0;
        // A row is done before it is matched, so `matched` read before
        // `done` is never more.
        self.done.fetch_add(1, Ordering::SeqCst);
        if let Some(feature) = row.feature {
            self.matched.fetch_add(1, Ordering::SeqCst);
            *self
                .accuracy
                .lock()
                .expect("accuracy lock")
                .entry(feature.properties.common.accuracy)
                .or_default() += 1;
        }
        self.latencies.record(millis);
        let mut latest = self.latest.lock().expect("latest lock");
        latest.push_front(Latest {
            query: row.query.to_string(),
            hit: row.feature.map(|feature| {
                (
                    feature.properties.common.name.clone(),
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
    /// Rows done and rows matched so far.
    pub fn counts(&self) -> (u64, u64) {
        let matched = self.matched.load(Ordering::SeqCst);
        let done = self.done.load(Ordering::SeqCst);
        (done, matched.min(done))
    }
}

/// What a finished run found.
pub struct Summary {
    pub rows: u64,
    pub matched: u64,
    /// Matches by accuracy, the most precise first.
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
    /// What a run would answer for the sample rows, by the columns picked.
    pub previews: HashMap<Vec<bool>, Vec<Option<Hit>>>,
    /// Where the run writes, as it would be named now.
    pub output: PathBuf,
    pub run: Option<Run>,
    pub summary: Option<Summary>,
}

impl BatchSession {
    pub fn new(file: BatchFile) -> Self {
        let used = address_columns(&file.headers);
        let column = used.iter().position(|on| *on).unwrap_or(0);
        Self {
            output: file.output_path(),
            file,
            used,
            column,
            previews: HashMap::new(),
            run: None,
            summary: None,
        }
    }

    /// The address searched for a row.
    pub fn query(&self, row: &[String]) -> String {
        address(&self.used, row)
    }

    pub fn has_address(&self) -> bool {
        self.used.contains(&true)
    }

    /// Starts geocoding the whole file.
    pub fn start(&mut self, pack: &Path) {
        self.output = self.file.output_path();
        let output = self.output.clone();
        let stats = Arc::new(RunStats::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let options = BatchGeocodeOptions {
            pack: Some(pack.to_path_buf()),
            input: self.file.path.clone(),
            output: output.clone(),
            header_row: self.file.header_found,
            // By place: two columns may share a name, or have none.
            address_field_groups: vec![
                self.used
                    .iter()
                    .enumerate()
                    .filter(|(_, on)| **on)
                    .map(|(index, _)| Column::At(index))
                    .collect(),
            ],
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
        let (rows, matched) = stats.counts();
        let by_accuracy = stats
            .accuracy
            .lock()
            .expect("accuracy lock")
            .iter()
            .map(|(accuracy, count)| (name_of(accuracy), *count))
            .collect();
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
            rows,
            matched,
            by_accuracy,
            p50: stats.latencies.quantile(0.50),
            p95: stats.latencies.quantile(0.95),
            p99: stats.latencies.quantile(0.99),
            slowest: stats.latencies.slowest(),
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
            "ADR_EGAID",
            "STR_ESID",
            "BDG_EGID",
            "ADR_EDID",
            "STN_LABEL",
            "ADR_NUMBER",
            "BDG_CATEGORY",
            "BDG_NAME",
            "ZIP_LABEL",
            "COM_FOSNR",
            "COM_NAME",
            "COM_CANTON",
            "ADR_STATUS",
            "ADR_OFFICIAL",
            "ADR_MODIFIED",
            "ADR_EASTING",
            "ADR_NORTHING",
        ];
        let picked: Vec<&str> = headers
            .into_iter()
            .filter(|header| names_address(header))
            .collect();
        assert_eq!(
            picked,
            [
                "STN_LABEL",
                "ADR_NUMBER",
                "ZIP_LABEL",
                "COM_NAME",
                "COM_CANTON"
            ]
        );
        for header in [
            "address",
            "Street Address",
            "city",
            "Postal Code",
            "id",
            "lat",
        ] {
            assert_eq!(
                names_address(header),
                !matches!(header, "id" | "lat"),
                "{header}"
            );
        }
    }

    #[test]
    fn tells_a_header_from_data() {
        let row = |cells: &[&str]| {
            cells
                .iter()
                .map(|cell| cell.to_string())
                .collect::<Vec<_>>()
        };
        let codes = row(&["ADR_EGAID", "BDG_EGID"]);
        assert!(looks_like_header(
            &codes,
            Some(&row(&["102435015", "243056020"]))
        ));
        let addresses = row(&["100 queen st w", "toronto"]);
        assert!(!looks_like_header(
            &addresses,
            Some(&row(&["24 sussex dr", "ottawa"]))
        ));
    }

    #[test]
    fn reads_pasted_text_as_a_file() {
        let (addresses, note) =
            pasted_addresses("id;street;city\n1;24 Sussex Dr;Ottawa\n2;;\n3;1 Main St;\n");
        assert_eq!(addresses, ["24 Sussex Dr, Ottawa", "1 Main St"]);
        assert_eq!(note, "address from street, city");
        let (addresses, note) = pasted_addresses("100 queen st w\n24 sussex dr");
        assert_eq!(addresses, ["100 queen st w", "24 sussex dr"]);
        assert_eq!(note, "one address per row");
    }

    #[test]
    fn latency_quantiles_fall_in_their_bucket() {
        for micros in [0, 1, 7, 8, 9, 15, 16, 1_000, 123_456, 1 << 40] {
            let bucket = Latencies::bucket(micros);
            let micros = micros.min((1 << LATENCY_BITS) - 1);
            assert!(Latencies::lower(bucket) <= micros, "{micros}");
            assert!(micros < Latencies::lower(bucket + 1), "{micros}");
            assert!(bucket < LATENCY_BUCKETS, "{micros}");
        }
        let latencies = Latencies::default();
        for millis in 1..=100 {
            latencies.record(f64::from(millis));
        }
        let p50 = latencies.quantile(0.5);
        assert!((p50 - 50.0).abs() / 50.0 < 0.07, "{p50}");
        assert!((latencies.quantile(0.99) - 99.0).abs() / 99.0 < 0.07);
        assert_eq!(latencies.slowest(), 100.0);
    }
}
