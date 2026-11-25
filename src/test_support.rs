use fs4::fs_std::FileExt;
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

static ROOT: LazyLock<Root> = LazyLock::new(|| {
    let temp = std::env::temp_dir();
    sweep_dead_roots(&temp);
    let path = temp.join(format!(
        "{ROOT_PREFIX}{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    fs::create_dir(&path).expect("create the test root");
    let unswept_lock_path = path.join(format!("{LOCK_FILE}.new"));
    let lock = File::create(&unswept_lock_path).expect("create the test root lock");
    assert!(
        lock.try_lock_exclusive().expect("lock the test root"),
        "the new test root's lock is free"
    );
    fs::rename(&unswept_lock_path, path.join(LOCK_FILE)).expect("publish the test root lock");
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
        if lock_is_free(&root) {
            let _ = fs::remove_dir_all(root);
        }
    }
}

fn lock_is_free(root: &Path) -> bool {
    File::open(root.join(LOCK_FILE)).is_ok_and(|lock| matches!(lock.try_lock_exclusive(), Ok(true)))
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
