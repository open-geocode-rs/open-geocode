//! A path removed when it goes out of scope, a failed assertion included.

use std::{
    ops::Deref,
    path::{Path, PathBuf},
};

pub(crate) struct TempPath(PathBuf);

impl TempPath {
    /// Armed before anything is written to the path.
    pub(crate) fn new(name: &str) -> Self {
        Self(std::env::temp_dir().join(format!("open-geocode-{name}-{}", uuid::Uuid::new_v4())))
    }
}

impl Deref for TempPath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        let _ = std::fs::remove_file(&self.0);
        // A server's job spool sits beside its Pack.
        let _ = std::fs::remove_dir_all(self.0.with_extension("jobs"));
    }
}
