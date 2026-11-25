use std::{
    fs::{self, File},
    ops::Deref,
    path::{Path, PathBuf},
    sync::LazyLock,
};

const ROOT_PREFIX: &str = "open-geocode-tests-";
const LOCK_FILE: &str = "lock";

struct Root {
    path: PathBuf,
    _lock: File,
}

// Statics never drop. A later run's sweep frees this root once the lock dies with us.
static ROOT: LazyLock<Root> = LazyLock::new(|| {
    let temp = std::env::temp_dir();
    sweep_dead_roots(&temp);
    let path = temp.join(format!(
        "{ROOT_PREFIX}{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&path).expect("create the test root");
    let staged = path.join(format!("{LOCK_FILE}.new"));
    let lock = File::create(&staged).expect("create the test root lock");
    lock.try_lock().expect("lock the test root");
    // A sweep opens only the final name, so it never sees the lock unheld.
    fs::rename(&staged, path.join(LOCK_FILE)).expect("publish the test root lock");
    Root { path, _lock: lock }
});

fn sweep_dead_roots(temp: &Path) {
    let Ok(entries) = fs::read_dir(temp) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(ROOT_PREFIX) {
            continue;
        }
        let root = entry.path();
        let Ok(lock) = File::open(root.join(LOCK_FILE)) else {
            continue;
        };
        if lock.try_lock().is_err() {
            continue;
        }
        // Windows refuses to remove a directory holding an open file.
        drop(lock);
        let _ = fs::remove_dir_all(root);
    }
}

pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(name: &str) -> Self {
        let path = ROOT.path.join(format!("{name}-{}", uuid::Uuid::new_v4()));
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
