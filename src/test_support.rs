use std::{
    fs,
    ops::Deref,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{Duration, SystemTime},
};

/// A killed run leaves its root behind. No run lasts six hours.
const STALE_ROOT_AGE: Duration = Duration::from_secs(6 * 60 * 60);

static ROOT: LazyLock<PathBuf> = LazyLock::new(|| {
    let parent = std::env::temp_dir().join("open-geocode-tests");
    sweep_stale_roots(&parent);
    let root = parent.join(format!("{}-{}", std::process::id(), uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).expect("create the test root");
    root
});

fn sweep_stale_roots(parent: &Path) {
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > STALE_ROOT_AGE);
        if stale {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(name: &str) -> Self {
        let path = ROOT.join(format!("{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create a test directory");
        Self(path)
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
