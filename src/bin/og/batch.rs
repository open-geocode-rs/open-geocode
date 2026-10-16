//! The batch geocoding flow: a CSV is read, each column gets a role, and the
//! file is geocoded by the engine's batch run, the same `open-geocode
//! batch-geocode` does, into a new CSV beside it.

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
    BatchGeocodeOptions, BatchObserver, BatchRow, run_batch_geocode, unnamed_columns,
};

use crate::engine::{Engine, Hit, name_of};

/// Rows of the file shown, and geocoded, before the run starts.
pub const PREVIEW_ROWS: usize = 3;
/// Rows the running screen lists, newest first.
const LATEST_ROWS: usize = 8;

/// What a column holds, as far as the query is concerned.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Address,
    City,
    Region,
    Postcode,
    Skip,
}

impl Role {
    const ALL: [Role; 5] = [
        Role::Address,
        Role::City,
        Role::Region,
        Role::Postcode,
        Role::Skip,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Address => "Address",
            Self::City => "City",
            Self::Region => "Region",
            Self::Postcode => "Postcode",
            Self::Skip => "Skip",
        }
    }

    /// The next role, or the one before it.
    pub fn cycle(self, forward: bool) -> Self {
        let at = Self::ALL.iter().position(|role| *role == self).unwrap_or(0);
        let next = if forward {
            (at + 1) % Self::ALL.len()
        } else {
            (at + Self::ALL.len() - 1) % Self::ALL.len()
        };
        Self::ALL[next]
    }
}

/// The role a header name suggests.
fn detect(header: &str) -> Role {
    let header = header.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| header.contains(word));
    if has(&["addr", "street"]) {
        Role::Address
    } else if has(&["city", "town", "locality", "municipal"]) {
        Role::City
    } else if has(&["prov", "state", "region"]) {
        Role::Region
    } else if has(&["postal", "zip", "postcode"]) {
        Role::Postcode
    } else {
        Role::Skip
    }
}

/// Whether a first row names columns instead of holding an address.
fn looks_like_header(row: &[String]) -> bool {
    const WORDS: [&str; 12] = [
        "address", "addr", "street", "city", "province", "state", "postal", "zip", "name", "id",
        "lat", "lon",
    ];
    row.iter().any(|cell| {
        let cell = cell.to_lowercase();
        WORDS
            .iter()
            .any(|word| cell == *word || cell.contains(word))
    })
}

pub struct BatchFile {
    pub path: PathBuf,
    pub name: String,
    pub bytes: u64,
    pub headers: Vec<String>,
    pub header_found: bool,
    /// Rows of data, the header row not counted.
    pub total_rows: u64,
    /// The first rows of data.
    sample: Vec<Vec<String>>,
}

impl BatchFile {
    /// Reads a CSV: its header, its first rows, and how many rows it has.
    /// None when the path is no readable CSV with at least one row.
    pub fn from_path(path: &Path) -> Option<Self> {
        let meta = path.metadata().ok()?;
        if !meta.is_file() {
            return None;
        }
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .from_path(path)
            .ok()?;
        let mut first: Vec<Vec<String>> = Vec::new();
        let mut rows = 0u64;
        for record in reader.records() {
            let record = record.ok()?;
            if first.len() <= PREVIEW_ROWS {
                first.push(record.iter().map(|cell| cell.trim().to_string()).collect());
            }
            rows += 1;
        }
        if first.is_empty() {
            return None;
        }
        let header_found = looks_like_header(&first[0]);
        let headers = if header_found {
            rows -= 1;
            first.remove(0)
        } else {
            unnamed_columns(path).ok()?
        };
        first.truncate(PREVIEW_ROWS);
        Some(Self {
            path: path.to_path_buf(),
            name: path.file_name()?.to_string_lossy().into_owned(),
            bytes: meta.len(),
            total_rows: rows,
            headers,
            header_found,
            sample: first,
        })
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
    pub roles: Vec<Role>,
    /// The column the setup cursor is on.
    pub column: usize,
    pub run: Option<Run>,
    pub summary: Option<Summary>,
}

impl BatchSession {
    pub fn new(file: BatchFile) -> Self {
        let roles: Vec<Role> = file.headers.iter().map(|header| detect(header)).collect();
        let column = roles
            .iter()
            .position(|role| *role == Role::Address)
            .unwrap_or(0);
        Self {
            file,
            roles,
            column,
            run: None,
            summary: None,
        }
    }

    fn columns(&self, role: Role) -> Vec<String> {
        self.roles
            .iter()
            .zip(&self.file.headers)
            .filter(|(own, _)| **own == role)
            .map(|(_, header)| header.clone())
            .collect()
    }

    fn cells(&self, row: &[String], role: Role) -> Option<String> {
        let cells = self
            .roles
            .iter()
            .zip(row)
            .filter(|(own, _)| **own == role)
            .map(|(_, cell)| cell.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|cell| !cell.is_empty())
            .collect::<Vec<_>>();
        (!cells.is_empty()).then(|| cells.join(", "))
    }

    /// The address searched for a row: its address columns, as the run
    /// joins them.
    pub fn query(&self, row: &[String]) -> String {
        self.cells(row, Role::Address).unwrap_or_default()
    }

    /// What the run would answer for a row.
    pub fn lookup(&self, engine: &Engine, row: &[String]) -> Option<Hit> {
        let address = self.cells(row, Role::Address)?;
        engine
            .geocode_address(
                address,
                self.cells(row, Role::City),
                self.cells(row, Role::Region),
                self.cells(row, Role::Postcode),
            )
            .ok()
            .flatten()
    }

    pub fn has_address(&self) -> bool {
        self.roles.contains(&Role::Address)
    }

    /// Starts geocoding the whole file.
    pub fn start(&mut self, pack: &Path) {
        let first = |role: Role| self.columns(role).into_iter().next();
        let output = self.file.output_path();
        let stats = Arc::new(RunStats::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let options = BatchGeocodeOptions {
            pack: Some(pack.to_path_buf()),
            input: self.file.path.clone(),
            output: output.clone(),
            header_row: self.file.header_found,
            address_field_groups: vec![self.columns(Role::Address)],
            locality_field: first(Role::City),
            region_field: first(Role::Region),
            postcode_field: first(Role::Postcode),
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
        let output_head = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .from_path(&run.output)
            .map(|mut reader| {
                reader
                    .records()
                    .take(PREVIEW_ROWS + 1)
                    .filter_map(Result::ok)
                    .map(|record| record.iter().map(str::to_string).collect())
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
