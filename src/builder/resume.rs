//! Going on with a Pack build that stopped.
//!
//! A build checkpoints after each stage (see [`crate::builder::osm`]): its
//! outputs are on disk and a manifest in the scratch directory names them,
//! with a fingerprint of everything the result depends on. A build that
//! fails, is cancelled, runs out of disk or loses power leaves the scratch
//! directory, its unpublished generation and the last manifest behind. The
//! next build of the Pack finds them ([`find`]): asked to resume, it checks
//! the fingerprint and goes on after the last stage the manifest reached;
//! asked to start over, it clears them; asked neither, it refuses and says
//! what was left and how to do either, rather than delete hours of work or
//! fill the disk twice.

use std::{
    fmt,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::xxh3_64;

use crate::{
    error::{Error, Result},
    extsort::MANIFEST_FILE,
    pack::{self, SCRATCH_POINTER_FILE},
};

/// The manifest's own format. A build refuses a manifest of another.
pub const FORMAT_VERSION: u32 = 1;
/// Bytes of an input's start and end the fingerprint hashes: a PBF's header
/// and its last blocks, enough to tell an extract from a newer one of the
/// same size and time.
const STAMP_BYTES: u64 = 1 << 20;

/// What a build does with a build of the same Pack that stopped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Leftover {
    /// Refuse to start, saying what was left and how to resume or clear it.
    #[default]
    Refuse,
    /// Go on from its last checkpoint; start over when there is none.
    Resume,
    /// Remove it and start over.
    Clear,
}

/// A file a build reads, as a fingerprint knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub path: PathBuf,
    pub bytes: u64,
    /// Last modified, in nanoseconds since the Unix epoch.
    pub modified_ns: Option<u128>,
    /// XXH3 of its first and of its last [`STAMP_BYTES`].
    pub head: u64,
    pub tail: u64,
}

impl FileStamp {
    pub fn of(path: &Path) -> Result<Self> {
        let failed = |source| Error::IoPath {
            action: "failed to read",
            path: path.to_path_buf(),
            source,
        };
        let mut file = File::open(path).map_err(failed)?;
        let metadata = file.metadata().map_err(failed)?;
        let bytes = metadata.len();
        let mut read = |from: u64| -> Result<u64> {
            let mut buffer = Vec::new();
            file.seek(SeekFrom::Start(from)).map_err(failed)?;
            (&mut file)
                .take(STAMP_BYTES)
                .read_to_end(&mut buffer)
                .map_err(failed)?;
            Ok(xxh3_64(&buffer))
        };
        let head = read(0)?;
        let tail = read(bytes.saturating_sub(STAMP_BYTES))?;
        Ok(Self {
            path: path.to_path_buf(),
            bytes,
            modified_ns: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|since| since.as_nanos()),
            head,
            tail,
        })
    }

    /// How this file differs from `now`, the same file read again.
    fn differences(&self, now: &Self, what: &str, out: &mut Vec<String>) {
        let name = self.path.display();
        if self.path != now.path {
            out.push(format!("{what} was {name}, is {}", now.path.display()));
        } else if self.bytes != now.bytes {
            out.push(format!(
                "{what} {name} was {} bytes, is {}",
                self.bytes, now.bytes
            ));
        } else if self.modified_ns != now.modified_ns {
            out.push(format!("{what} {name} was modified since"));
        } else if (self.head, self.tail) != (now.head, now.tail) {
            out.push(format!("{what} {name} holds other bytes"));
        }
    }
}

/// Everything that decides what a build makes: its inputs, og's version,
/// the formats it writes and the options that change the Pack. The memory
/// and scratch budgets, threads and the scratch directory do not: a build
/// sorts and writes the same records whatever it spills.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub og_version: String,
    /// Each format the build writes, by name, with its version.
    pub formats: Vec<(String, u32)>,
    pub inputs: Vec<FileStamp>,
    pub input_sources: Vec<String>,
    /// The address source the build adds addresses from, as it describes
    /// itself, if any.
    pub addresses: Option<String>,
    /// The address files it imported, once it has.
    pub imported: Vec<FileStamp>,
}

impl Fingerprint {
    /// Read every input again.
    pub fn of(
        inputs: &[PathBuf],
        input_sources: &[String],
        addresses: Option<String>,
    ) -> Result<Self> {
        Ok(Self {
            og_version: env!("CARGO_PKG_VERSION").to_string(),
            formats: formats(),
            inputs: inputs
                .iter()
                .map(|input| FileStamp::of(input))
                .collect::<Result<_>>()?,
            input_sources: input_sources.to_vec(),
            addresses,
            imported: Vec::new(),
        })
    }

    /// What differs between the build this fingerprints and `now`, one line
    /// each; empty when it may go on. Imported files are read again.
    pub fn differences(&self, now: &Self) -> Vec<String> {
        let mut out = Vec::new();
        if self.og_version != now.og_version {
            out.push(format!(
                "og was version {}, is {}",
                self.og_version, now.og_version
            ));
        }
        if self.formats != now.formats {
            out.push(format!(
                "the formats written were {:?}, are {:?}",
                self.formats, now.formats
            ));
        }
        if self.inputs.len() != now.inputs.len() {
            out.push(format!(
                "the build read {} inputs, is given {}",
                self.inputs.len(),
                now.inputs.len()
            ));
        }
        for (index, (was, is)) in self.inputs.iter().zip(&now.inputs).enumerate() {
            was.differences(is, &format!("input {}", index + 1), &mut out);
        }
        if self.input_sources != now.input_sources {
            out.push(format!(
                "the inputs were named {:?}, are {:?}",
                self.input_sources, now.input_sources
            ));
        }
        if self.addresses != now.addresses {
            out.push(format!(
                "addresses were added from {}, are from {}",
                self.addresses.as_deref().unwrap_or("nothing"),
                now.addresses.as_deref().unwrap_or("nothing")
            ));
        }
        for file in &self.imported {
            match FileStamp::of(&file.path) {
                Ok(is) => file.differences(&is, "imported address file", &mut out),
                Err(_) => out.push(format!(
                    "imported address file {} is gone",
                    file.path.display()
                )),
            }
        }
        out
    }
}

fn formats() -> Vec<(String, u32)> {
    [
        ("checkpoint", FORMAT_VERSION),
        ("pack", pack::PACK_SCHEMA_VERSION),
        ("records", crate::records::RECORDS_VERSION),
        ("text index", crate::text_index::TEXT_INDEX_SCHEMA_VERSION),
        (
            "build report",
            crate::builder::report::BUILD_REPORT_SCHEMA_VERSION,
        ),
    ]
    .into_iter()
    .map(|(name, version)| (name.to_string(), version))
    .collect()
}

/// A checkpoint: the build's fingerprint, the last stage it completed and
/// what it carries into the next, `S`, which the builder defines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest<S> {
    pub format: u32,
    /// The last stage completed, as people read it.
    pub stage: String,
    pub fingerprint: Fingerprint,
    /// The build's time up to the checkpoint, every run before it together.
    pub elapsed_ms: u64,
    /// Every file and directory in the scratch directory it names; the
    /// rest is removed when a build goes on from it.
    pub files: Vec<String>,
    pub state: S,
}

/// What a manifest says without knowing the builder's state.
#[derive(Debug, Deserialize)]
struct Summary {
    format: u32,
    stage: String,
}

/// A build of the Pack that stopped: its generation, its scratch directory
/// and how far it got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub generation: PathBuf,
    pub scratch: PathBuf,
    /// The last stage it completed; `None` when it reached no checkpoint,
    /// or left one of another format.
    pub stage: Option<String>,
    /// Bytes it holds on disk, scratch and unpublished Pack file together.
    pub bytes: u64,
}

impl Found {
    pub fn manifest(&self) -> PathBuf {
        self.scratch.join(MANIFEST_FILE)
    }

    /// Whether it can be resumed.
    pub fn resumable(&self) -> bool {
        self.stage.is_some()
    }
}

impl fmt::Display for Found {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let size = self.bytes as f64 / f64::from(1 << 30);
        match &self.stage {
            Some(stage) => write!(
                formatter,
                "a build that stopped after \"{stage}\", {size:.1} GiB in {}",
                self.scratch.display()
            ),
            None => write!(
                formatter,
                "a build that stopped before its first checkpoint, {size:.1} GiB in {}",
                self.scratch.display()
            ),
        }
    }
}

/// The builds of the Pack at `destination` that stopped, the last
/// checkpointed first.
pub fn find(destination: &Path) -> Result<Vec<Found>> {
    let mut found = Vec::new();
    for generation in pack::unpublished_generations(destination)? {
        let Ok(pointer) = fs::read_to_string(generation.join(SCRATCH_POINTER_FILE)) else {
            continue;
        };
        let scratch = PathBuf::from(pointer.trim());
        let summary = fs::read(scratch.join(MANIFEST_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Summary>(&bytes).ok())
            .filter(|summary| summary.format == FORMAT_VERSION);
        let modified = fs::metadata(scratch.join(MANIFEST_FILE))
            .and_then(|metadata| metadata.modified())
            .ok();
        let bytes = if scratch.starts_with(&generation) {
            tree_bytes(&generation)
        } else {
            tree_bytes(&generation) + tree_bytes(&scratch)
        };
        found.push((
            modified,
            Found {
                generation,
                scratch,
                stage: summary.map(|summary| summary.stage),
                bytes,
            },
        ));
    }
    found.sort_by(|(left, _), (right, _)| right.cmp(left));
    Ok(found.into_iter().map(|(_, found)| found).collect())
}

/// Remove a stopped build: its scratch directory and its generation.
pub fn clear(found: &Found) -> Result<()> {
    for path in [&found.scratch, &found.generation] {
        match fs::remove_dir_all(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::IoPath {
                    action: "failed to clear the stopped build at",
                    path: path.clone(),
                    source,
                });
            }
        }
    }
    Ok(())
}

/// Why a build of `destination` will not start beside `found`, and the two
/// ways to go on.
pub fn refusal(destination: &Path, found: &[Found]) -> Error {
    let listed = found
        .iter()
        .map(|found| format!("  {found}"))
        .collect::<Vec<_>>()
        .join("\n");
    let resume = if found.iter().any(Found::resumable) {
        "pass --resume to go on from its last checkpoint, or "
    } else {
        ""
    };
    Error::BadOptions(
        format!(
            "{} has {} left by a build that stopped:\n{listed}\n{resume}pass --fresh to clear it and start over",
            destination.display(),
            if found.len() == 1 { "scratch" } else { "scratch directories" },
        )
        .into(),
    )
}

/// Bytes of the files under `path`.
fn tree_bytes(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => tree_bytes(&entry.path()),
            Ok(_) => entry.metadata().map_or(0, |metadata| metadata.len()),
            Err(_) => 0,
        })
        .sum()
}
