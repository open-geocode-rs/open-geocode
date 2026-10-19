//! Where `og` keeps its files: downloaded extracts, the Packs built from
//! them, and a record of each finished build.
//!
//! ```text
//! <home>/pbf/<region id>.osm.pbf    an extract, as Geofabrik names it
//! <home>/packs/<region slug>/       a Pack
//! <home>/builds.jsonl               one line per finished build
//! ```
//!
//! The home is `OG_HOME` when set, else the platform's local data directory.

use std::{
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use open_geocode::{
    builder::BuilderReport,
    pack::{AUDIT_DIR, BUILD_REPORT_FILE, Reader},
    record::Layer,
};
use serde::{Deserialize, Serialize};

use crate::regions::{self, Region};

pub(crate) struct Home {
    pub root: PathBuf,
}

impl Home {
    pub(crate) fn locate() -> Result<Self> {
        let root = match std::env::var_os("OG_HOME") {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => dirs::data_local_dir()
                .context("this system names no local data directory; set OG_HOME")?
                .join("open-geocode"),
        };
        Ok(Self { root })
    }

    pub(crate) fn packs_dir(&self) -> PathBuf {
        self.root.join("packs")
    }

    pub(crate) fn pack_dir(&self, region: &Region) -> PathBuf {
        self.packs_dir().join(region.slug())
    }

    /// Where a Pack of several regions goes: under the name it was given.
    pub(crate) fn named_pack_dir(&self, name: &str) -> PathBuf {
        self.packs_dir().join(slugify(name))
    }

    /// Where the region's extract is kept.
    pub(crate) fn extract_path(&self, region: &Region) -> PathBuf {
        let mut path = self.root.join("pbf");
        for part in region.id.split('/') {
            path.push(part);
        }
        path.set_extension("osm.pbf");
        path
    }

    /// The extract of a region when it is on disk.
    pub(crate) fn extract(&self, region: &Region) -> Option<Extract> {
        let path = self.extract_path(region);
        let bytes = fs::metadata(&path)
            .ok()
            .filter(|meta| meta.is_file())?
            .len();
        Some(Extract { path, bytes })
    }

    fn history_path(&self) -> PathBuf {
        self.root.join("builds.jsonl")
    }

    /// Every build finished on this machine, oldest first.
    pub(crate) fn history(&self) -> Vec<BuildRecord> {
        let Ok(file) = fs::File::open(self.history_path()) else {
            return Vec::new();
        };
        BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str(&line).ok())
            .collect()
    }

    pub(crate) fn record_build(&self, record: &BuildRecord) -> Result<()> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("failed to create {}", self.root.display()))?;
        let path = self.history_path();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        writeln!(file, "{}", serde_json::to_string(record)?)
            .with_context(|| format!("failed to write {}", path.display()))
    }

    /// The Packs in the home, by name.
    pub(crate) fn packs(&self) -> Vec<PackInfo> {
        let Ok(entries) = fs::read_dir(self.packs_dir()) else {
            return Vec::new();
        };
        let mut packs: Vec<PackInfo> = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().join("CURRENT").is_file())
            .filter_map(|entry| PackInfo::read(&entry.path()))
            .collect();
        packs.sort_by_key(|pack| pack.name.to_lowercase());
        packs
    }
}

pub(crate) struct Extract {
    pub path: PathBuf,
    pub bytes: u64,
}

/// A Pack on disk, as the home screen lists it.
pub(crate) struct PackInfo {
    pub name: String,
    pub dir: PathBuf,
    /// The catalog regions it was built from, when og built it. More than
    /// one is a Pack covering them all, served by one process.
    pub regions: Vec<&'static Region>,
    pub records: u64,
    pub bytes: u64,
    pub built_at: i64,
    /// When its data is from.
    pub data_at: Option<i64>,
    /// The datasets its records come from, by id, as its build report
    /// names them; empty when the report is gone.
    pub datasets: Vec<String>,
    pub layers: Vec<(Layer, u64)>,
    /// How long the build took, from its report.
    pub build_secs: Option<f64>,
}

impl PackInfo {
    pub(crate) fn read(dir: &Path) -> Option<Self> {
        let reader = Reader::open(dir).ok()?;
        let manifest = reader.manifest();
        let report = reader
            .path()
            .parent()
            .map(|generation| generation.join(AUDIT_DIR).join(BUILD_REPORT_FILE))
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<BuilderReport>(&bytes).ok());
        let regions: Vec<&'static Region> = report
            .as_ref()
            .map(|report| {
                report
                    .inputs
                    .iter()
                    .filter_map(|input| regions::tree().get(input.source.as_deref()?))
                    .collect()
            })
            .unwrap_or_default();
        // One region names the Pack; several leave the directory name, which
        // is what the person called the Pack when they built it.
        let name = match regions.as_slice() {
            [region] => region.name.clone(),
            _ => dir.file_name()?.to_string_lossy().into_owned(),
        };
        Some(Self {
            name,
            dir: dir.to_path_buf(),
            regions,
            records: manifest.record_count,
            bytes: reader.container().file_size(),
            built_at: manifest.built_at_unix as i64,
            // The oldest input's date: the Pack is only as fresh as the
            // least fresh region in it.
            data_at: report.as_ref().and_then(|report| {
                report
                    .inputs
                    .iter()
                    .filter_map(|input| input.replication_timestamp)
                    .min()
            }),
            datasets: report
                .as_ref()
                .map(|report| report.datasets.clone())
                .unwrap_or_default(),
            layers: manifest
                .layer_counts
                .iter()
                .filter_map(|(layer, count)| Some((Layer::parse(layer)?, *count)))
                .collect(),
            build_secs: report
                .as_ref()
                .map(|report| report.phases.total_ms as f64 / 1_000.0),
        })
    }

    /// Records in the layers `pick` takes.
    pub(crate) fn records_in(&self, pick: impl Fn(Layer) -> bool) -> u64 {
        self.layers
            .iter()
            .filter(|(layer, _)| pick(*layer))
            .map(|(_, count)| count)
            .sum()
    }
}

/// One finished build, as `builds.jsonl` keeps it: what a later build of
/// another region is estimated from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BuildRecord {
    pub region: String,
    pub input_bytes: u64,
    /// The build itself, without the download.
    pub build_secs: f64,
    pub pack_bytes: u64,
    pub memory_budget_bytes: u64,
    pub threads: usize,
    pub finished_at: i64,
}

/// Furthest apart, as a ratio of input sizes, a past build and the one it
/// estimates may be. Build time is not linear in input size over a wider
/// range: a small build is mostly fixed costs, a large one spills to disk.
const ESTIMATE_RANGE: f64 = 4.0;

/// How long a build of `input_bytes` is expected to take on this machine,
/// scaled from the finished build closest in input size. What it needs of
/// memory and disk is the build plan's to say.
pub(crate) struct Estimate {
    pub secs: f64,
    /// The build it is scaled from.
    pub from: BuildRecord,
}

/// None when no build on this machine is within [`ESTIMATE_RANGE`] of it.
pub(crate) fn estimate(history: &[BuildRecord], input_bytes: u64) -> Option<Estimate> {
    let distance =
        |record: &BuildRecord| (record.input_bytes as f64 / input_bytes as f64).ln().abs();
    let nearest = history
        .iter()
        .filter(|record| record.input_bytes > 0 && input_bytes > 0)
        .filter(|record| distance(record) <= ESTIMATE_RANGE.ln())
        .min_by(|a, b| distance(a).total_cmp(&distance(b)))?;
    let scale = input_bytes as f64 / nearest.input_bytes as f64;
    Some(Estimate {
        secs: nearest.build_secs * scale,
        from: nearest.clone(),
    })
}

/// A typed Pack name as a directory name: lower case, with runs of anything
/// but letters and digits as single dashes, so what the person typed is still
/// recognisable in the path.
pub(crate) fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() {
            slug.extend(c.to_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}
