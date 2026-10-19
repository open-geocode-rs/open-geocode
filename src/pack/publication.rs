use std::{
    fs::{self, File},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use crate::error::{Error, Result};
use uuid::Uuid;

use super::{Manifest, PACK_FILE, Reader};

const GENERATIONS: &str = "generations";
const CURRENT: &str = "CURRENT";

/// Resolve once per operation so every reader opens the same build. A file
/// path is a Pack file; a directory is a Pack directory with a `CURRENT`
/// pointer to its published generation.
pub fn resolve_pack_path(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let current = match fs::read_to_string(path.join(CURRENT)) {
        Ok(current) => current,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(Error::IoPath {
                action: "no Pack at",
                path: path.to_path_buf(),
                source: error,
            });
        }
        Err(error) => {
            return Err(Error::IoPath {
                action: "failed to read the CURRENT pointer of",
                path: path.to_path_buf(),
                source: error,
            });
        }
    };
    let id = Uuid::parse_str(current.trim())
        .map_err(|_| Error::Inconsistent("invalid pack generation in CURRENT".into()))?;
    let pack = path.join(GENERATIONS).join(id.to_string()).join(PACK_FILE);
    if !pack.is_file() {
        return Err(Error::Inconsistent(format!(
            "pack generation {} is incomplete or missing",
            pack.display()
        )));
    }
    Ok(pack)
}

pub(super) fn create_generation(destination: &Path) -> Result<PathBuf> {
    let generations = destination.join(GENERATIONS);
    fs::create_dir_all(&generations).map_err(|source| Error::IoPath {
        action: "failed to create",
        path: generations.clone(),
        source,
    })?;
    let generation = generations.join(Uuid::new_v4().to_string());
    fs::create_dir(&generation)?;
    Ok(generation)
}

/// Validate a generation and point `CURRENT` at it. `keep` is set the moment
/// `CURRENT` may name the generation, so a later failure never deletes the
/// Pack that servers are told to open.
pub(super) fn publish(
    destination: &Path,
    generation: &Path,
    manifest: &Manifest,
    keep: &mut bool,
) -> Result<()> {
    // Exercise the same readers used for serving before making the build visible.
    let pack = generation.join(PACK_FILE);
    let reader = Reader::open(&pack)?;
    reader.verify()?;
    if reader.manifest() != manifest {
        return Err(Error::ManifestMismatch);
    }
    let text = crate::text_index::open_text_index(reader.container())?;
    let searcher = text.reader()?.searcher();
    // A street group's document stands for its addresses, so the documents
    // cover every record once between them.
    let mut covered = 0;
    for segment in searcher.segment_readers() {
        let counts = segment
            .fast_fields()
            .u64(crate::text_index::RECORD_COUNT_FIELD)?;
        covered += (0..segment.max_doc())
            .filter_map(|doc| counts.first(doc))
            .sum::<u64>();
    }
    if searcher.num_docs() != manifest.text_document_count || covered != manifest.record_count {
        return Err(Error::Inconsistent(
            "text index documents do not cover the Pack records".into(),
        ));
    }
    drop(searcher);
    crate::spatial_index::Reader::open(reader.container(), reader.records().clone())?;
    drop((reader, text));

    sync_tree(generation)?;
    sync_directory(&destination.join(GENERATIONS))?;

    // A unique temporary pointer allows simultaneous builders without sharing writes.
    // Keep old generations: running servers may still map them.
    let pending = destination.join(format!(".CURRENT-{}", Uuid::new_v4()));
    let mut file = File::create_new(&pending)?;
    writeln!(
        file,
        "{}",
        generation
            .file_name()
            .ok_or(Error::Inconsistent("missing generation name".into()))?
            .to_string_lossy()
    )?;
    file.sync_all()?;
    drop(file);
    *keep = true;
    if let Err(error) = fs::rename(&pending, destination.join(CURRENT)) {
        // CURRENT still names the previous generation.
        *keep = false;
        let _ = fs::remove_file(&pending);
        return Err(Error::IoPath {
            action: "failed to publish the CURRENT pointer of",
            path: destination.to_path_buf(),
            source: error,
        });
    }
    #[cfg(test)]
    if FAIL_AFTER_SWITCH.get() {
        return Err(Error::Inconsistent(
            "injected failure after the CURRENT switch".into(),
        ));
    }
    sync_directory(destination)?;
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Simulates a failure after `CURRENT` already names the new generation.
    pub(super) static FAIL_AFTER_SWITCH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn sync_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        } else {
            File::options().write(true).open(entry.path())?.sync_all()?;
        }
    }
    sync_directory(path)
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
