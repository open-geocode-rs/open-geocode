//! The batch geocoding flow of the preview: a CSV is read, each column gets a
//! role, and every row is looked up in the canned records. The 10,000 address
//! example has its rows made up on demand; a real file is read, but its rows
//! can only match what the canned records know.

use std::path::Path;

use crate::mock::{self, Record, Stage};

/// Rows per second the preview pretends the engine manages.
const RATE: f64 = 2_041.0;
/// Rows looked up to find the shares of a run, so a large file is not
/// searched row by row up front.
const SAMPLE: usize = 2_000;
/// Longest file the preview reads.
const MAX_BYTES: u64 = 5_000_000;

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
    pub name: String,
    pub bytes: u64,
    pub headers: Vec<String>,
    pub header_found: bool,
    pub blank_skipped: usize,
    pub total_rows: usize,
    /// The rows of a real file. The example has none: it makes them up.
    rows: Vec<Vec<String>>,
    pub example: bool,
}

impl BatchFile {
    /// A made-up file of 10,000 addresses.
    pub fn example() -> Self {
        Self {
            name: "addresses.csv".to_string(),
            bytes: 4_200_000,
            headers: ["address", "city", "province", "postal", "id"]
                .map(str::to_string)
                .to_vec(),
            header_found: true,
            blank_skipped: 14,
            total_rows: 10_000,
            rows: Vec::new(),
            example: true,
        }
    }

    /// Reads a CSV. None when the path is not a readable file of a size the
    /// preview takes.
    pub fn from_path(path: &Path) -> Option<Self> {
        let meta = path.metadata().ok()?;
        if !meta.is_file() || meta.len() > MAX_BYTES {
            return None;
        }
        let text = std::fs::read_to_string(path).ok()?;
        let blank_lines = text.lines().filter(|line| line.trim().is_empty()).count();
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .from_reader(text.as_bytes());
        let mut rows: Vec<Vec<String>> = reader
            .records()
            .filter_map(Result::ok)
            .map(|record| record.iter().map(|cell| cell.trim().to_string()).collect())
            .collect();
        let before = rows.len();
        rows.retain(|row| row.iter().any(|cell| !cell.is_empty()));
        let blank_skipped = blank_lines + (before - rows.len());
        if rows.is_empty() {
            return None;
        }
        let header_found = looks_like_header(&rows[0]);
        let headers = if header_found {
            rows.remove(0)
        } else {
            (1..=rows[0].len())
                .map(|number| format!("Column {number}"))
                .collect()
        };
        Some(Self {
            name: path.file_name()?.to_string_lossy().into_owned(),
            bytes: meta.len(),
            total_rows: rows.len(),
            headers,
            header_found,
            blank_skipped,
            rows,
            example: false,
        })
    }

    /// The cells of one row, one per header.
    pub fn row(&self, index: usize) -> Vec<String> {
        let mut row = if self.example {
            let sample = mock::SAMPLE_ROWS[index % mock::SAMPLE_ROWS.len()];
            let mut row: Vec<String> = sample.iter().map(|cell| cell.to_string()).collect();
            row.push((index + 1).to_string());
            row
        } else {
            self.rows.get(index).cloned().unwrap_or_default()
        };
        row.resize(self.headers.len(), String::new());
        row
    }

    /// The file name without its extension.
    pub fn stem(&self) -> &str {
        self.name
            .rsplit_once('.')
            .map_or(&self.name, |(stem, _)| stem)
    }
}

/// What a finished run found.
pub struct Summary {
    pub matched: usize,
    pub by_accuracy: Vec<(&'static str, usize)>,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub slowest: f64,
    pub seconds: f64,
    pub output_bytes: u64,
    pub audit_bytes: u64,
}

pub struct BatchSession {
    pub file: BatchFile,
    pub roles: Vec<Role>,
    /// The column the setup cursor is on.
    pub column: usize,
    /// Share of rows that find a match, from a sample of the rows.
    pub share: f64,
    accuracy: Vec<(&'static str, f64)>,
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
            share: 0.0,
            accuracy: Vec::new(),
            summary: None,
        }
    }

    /// The text searched for a row: the columns with a role, in order,
    /// address first and the region and postcode last, as one line.
    pub fn query(&self, index: usize) -> String {
        let row = self.file.row(index);
        let part = |role: Role| {
            self.roles
                .iter()
                .zip(&row)
                .filter(|(own, _)| **own == role)
                .map(|(_, cell)| cell.trim())
                .filter(|cell| !cell.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let place = [part(Role::Region), part(Role::Postcode)]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        [part(Role::Address), part(Role::City), place]
            .into_iter()
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The best answer for a row.
    pub fn lookup(&self, index: usize) -> Option<(&'static Record, f64)> {
        let query = self.query(index);
        if query.is_empty() {
            None
        } else {
            mock::search(&query).into_iter().next()
        }
    }

    /// Rows whose columns give no address at all, estimated from a sample.
    pub fn without_address(&self) -> usize {
        let sampled = self.file.total_rows.min(SAMPLE);
        if sampled == 0 {
            return 0;
        }
        let empty = (0..sampled)
            .filter(|index| self.query(*index).is_empty())
            .count();
        (empty as f64 / sampled as f64 * self.file.total_rows as f64).round() as usize
    }

    /// How long the run takes at the pretended speed.
    pub fn seconds(&self) -> f64 {
        self.file.total_rows as f64 / RATE
    }

    /// `About 5 seconds for 10,000 rows`.
    pub fn estimate(&self) -> String {
        let seconds = self.seconds();
        let time = if seconds < 1.0 {
            "Under a second".to_string()
        } else if seconds < 90.0 {
            format!("About {:.0} seconds", seconds)
        } else {
            format!("About {:.0} minutes", seconds / 60.0)
        };
        format!("{time} for {} rows", mock::thousands(self.file.total_rows))
    }

    /// Looks at a sample of the rows to know how many will match, and how
    /// accurately, before the run plays.
    pub fn prepare(&mut self) {
        let sampled = self.file.total_rows.min(SAMPLE);
        let mut matched = 0;
        let mut accuracy: Vec<(&'static str, usize)> = Vec::new();
        for index in 0..sampled {
            if let Some((record, _)) = self.lookup(index) {
                matched += 1;
                match accuracy
                    .iter_mut()
                    .find(|(name, _)| *name == record.accuracy)
                {
                    Some((_, count)) => *count += 1,
                    None => accuracy.push((record.accuracy, 1)),
                }
            }
        }
        let total = sampled.max(1) as f64;
        self.share = matched as f64 / total;
        self.accuracy = accuracy
            .into_iter()
            .map(|(name, count)| (name, count as f64 / total))
            .collect();
    }

    /// The stages of the run, played like the stages of a build.
    pub fn stages(&self) -> Vec<Stage> {
        let rows = self.file.total_rows;
        let stem = self.file.stem().to_string();
        let output = self.file.bytes + rows as u64 * 22;
        let audit = rows as u64 * 150;
        let count = mock::thousands(rows);
        vec![
            Stage {
                did: "Read",
                doing: "Reading",
                object: self.file.name.clone(),
                explain: [
                    "Reading the file.",
                    "Rows without an address are kept and left unmatched.",
                ],
                total: Some(rows as f64),
                unit: "rows",
                play_secs: 0.8,
                real_secs: 0.2,
                summary: format!("{count} rows"),
            },
            Stage {
                did: "Geocoded",
                doing: "Geocoding",
                object: stem.clone(),
                explain: [
                    "Looking up every row in the Pack.",
                    "Each row is a search on its own.",
                ],
                total: Some(rows as f64),
                unit: "rows",
                play_secs: (rows as f64 / 1_000.0).clamp(2.0, 10.0),
                real_secs: self.seconds(),
                summary: format!("{count} rows"),
            },
            Stage {
                did: "Wrote",
                doing: "Writing",
                object: "geocoded.csv".to_string(),
                explain: ["Writing your columns with lat and lon added.", ""],
                total: Some(output as f64 / 1e6),
                unit: "MB",
                play_secs: 0.8,
                real_secs: 0.3,
                summary: mock::size(output as f64 / 1e6),
            },
            Stage {
                did: "Wrote",
                doing: "Writing",
                object: "audit.csv".to_string(),
                explain: ["Writing one line per row: status, reason and score.", ""],
                total: Some(audit as f64 / 1e6),
                unit: "MB",
                play_secs: 0.8,
                real_secs: 0.4,
                summary: mock::size(audit as f64 / 1e6),
            },
        ]
    }

    /// Totals for the done screen.
    pub fn summarize(&mut self) {
        let rows = self.file.total_rows;
        let matched = (rows as f64 * self.share).round() as usize;
        let mut by_accuracy: Vec<(&'static str, usize)> = self
            .accuracy
            .iter()
            .map(|(name, share)| (*name, (rows as f64 * share).round() as usize))
            .collect();
        // Rounding must not make the parts differ from the whole.
        let parts: usize = by_accuracy.iter().map(|(_, count)| count).sum();
        if let Some(largest) = by_accuracy.iter_mut().max_by_key(|(_, count)| *count) {
            largest.1 = (largest.1 + matched).saturating_sub(parts);
        }
        let order = [
            "rooftop",
            "interpolated",
            "street",
            "centroid",
            "approximate",
        ];
        by_accuracy.sort_by_key(|(name, _)| order.iter().position(|own| own == name));
        let mut latencies: Vec<f64> = (0..rows).map(mock::latency_ms).collect();
        latencies.sort_by(f64::total_cmp);
        self.summary = Some(Summary {
            matched,
            by_accuracy,
            p50: percentile(&latencies, 0.50),
            p95: percentile(&latencies, 0.95),
            p99: percentile(&latencies, 0.99),
            slowest: latencies.last().copied().unwrap_or(0.0),
            seconds: self.seconds(),
            output_bytes: self.file.bytes + rows as u64 * 22,
            audit_bytes: rows as u64 * 150,
        });
    }
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * q).round() as usize]
}

/// Median, 95th and 99th percentile of the first `done` queries.
pub fn latency_stats(done: usize) -> (f64, f64, f64) {
    let mut latencies: Vec<f64> = (0..done).map(mock::latency_ms).collect();
    latencies.sort_by(f64::total_cmp);
    (
        percentile(&latencies, 0.50),
        percentile(&latencies, 0.95),
        percentile(&latencies, 0.99),
    )
}
