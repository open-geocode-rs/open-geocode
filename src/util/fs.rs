use std::{fs, path::Path};

use anyhow::{Context, Result};

/// Total size in bytes of all files under `path`, recursing into subdirectories.
pub(crate) fn dir_size(path: impl AsRef<Path>) -> Result<u64> {
    let path = path.as_ref();
    let mut bytes = 0;
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            bytes += dir_size(entry.path())?;
        } else if metadata.is_file() {
            bytes += metadata.len();
        }
    }
    Ok(bytes)
}
