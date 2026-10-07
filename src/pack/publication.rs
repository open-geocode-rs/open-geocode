use std::{
    fs::{self, File},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use uuid::Uuid;

use super::{PackManifest, PackReader};

const GENERATIONS: &str = "generations";
const CURRENT: &str = "CURRENT";

/// Resolve once per operation so records and indexes come from the same build.
/// Packs built before generation publication remain readable at their original path.
pub fn resolve_pack_path(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    let current = match fs::read_to_string(path.join(CURRENT)) {
        Ok(current) => current,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(path.to_path_buf()),
        Err(error) => return Err(error).context("failed to read pack CURRENT pointer"),
    };
    let id = Uuid::parse_str(current.trim()).context("invalid pack generation in CURRENT")?;
    let generation = path.join(GENERATIONS).join(id.to_string());
    if !generation.join("manifest.json").is_file() {
        bail!(
            "pack generation {} is incomplete or missing",
            generation.display()
        );
    }
    Ok(generation)
}

pub(super) fn create_generation(destination: &Path) -> Result<PathBuf> {
    let generations = destination.join(GENERATIONS);
    fs::create_dir_all(&generations)
        .with_context(|| format!("failed to create {}", generations.display()))?;
    let generation = generations.join(Uuid::new_v4().to_string());
    fs::create_dir(&generation)?;
    Ok(generation)
}

pub(super) fn publish(
    destination: &Path,
    generation: &Path,
    manifest: &PackManifest,
) -> Result<()> {
    // Exercise the same readers used for serving before making the build visible.
    let reader = PackReader::open(generation)?;
    let text = crate::text_index::open_text_index(generation)?;
    let index_reader = text.reader()?;
    if index_reader.searcher().num_docs() != manifest.record_count {
        bail!("text index document count does not match pack records");
    }
    let spatial = crate::spatial_index::PackSpatialIndexReader::open(generation)?;
    drop((reader, index_reader, text, spatial));

    for entry in manifest.files.values() {
        let path = generation.join(&entry.path);
        let file = File::open(&path)?;
        if file.metadata()?.len() != entry.bytes {
            bail!("pack file {} does not match manifest size", path.display());
        }
    }
    sync_tree(generation)?;
    sync_directory(&destination.join(GENERATIONS))?;

    // A unique temporary pointer allows simultaneous builders without sharing writes.
    // Keep old and failed generations: existing mappings and lazy readers may use them.
    let pending = destination.join(format!(".CURRENT-{}", Uuid::new_v4()));
    let mut file = File::create_new(&pending)?;
    writeln!(
        file,
        "{}",
        generation
            .file_name()
            .context("missing generation name")?
            .to_string_lossy()
    )?;
    file.sync_all()?;
    drop(file);
    fs::rename(&pending, destination.join(CURRENT))
        .context("failed to publish pack CURRENT pointer")?;
    sync_directory(destination)?;
    Ok(())
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
