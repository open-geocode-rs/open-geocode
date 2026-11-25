use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    builder::BuildCancelled,
    error::{Error, Result},
    resources,
};

const LEVELS: [i32; 4] = [1, 3, 6, 9];
const DISK_PROBE_BYTES: u64 = 256 << 20;
pub const MANIFEST_FILE: &str = "resume.json";
const CHECKPOINT_LABEL: &str = "checkpoint";

#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
    next_file: AtomicU64,
    cache_bytes: AtomicU64,
    cancel: OnceLock<Arc<AtomicBool>>,
    budget: AtomicU64,
    live: AtomicU64,
    peak: AtomicU64,
    stage_peak: AtomicU64,
    written: AtomicU64,
    level: AtomicI32,
    owners: Mutex<BTreeMap<String, Arc<Owner>>>,
    peak_owners: Mutex<(u64, BTreeMap<String, u64>)>,
    disk: Mutex<DiskProbe>,
    watched: Mutex<Vec<(Arc<Owner>, PathBuf)>>,
    stage: Mutex<&'static str>,
    write_nanos: AtomicU64,
    read_wait_nanos: AtomicU64,
    spill_wait_nanos: AtomicU64,
    pins: Mutex<Pins>,
    retained: AtomicU64,
    peak_retained: AtomicU64,
}

#[derive(Debug, Default)]
struct Pins {
    held: bool,
    files: HashSet<PathBuf>,
    retired: Vec<(PathBuf, u64)>,
    synced: HashSet<PathBuf>,
    voided: Option<String>,
}

#[derive(Debug, Default)]
struct DiskProbe {
    free: Option<u64>,
    live: u64,
    reserved_since: u64,
}

#[derive(Debug, Default)]
pub struct Owner {
    label: String,
    items_written: AtomicU64,
    raw_written: AtomicU64,
    written: AtomicU64,
    items_read: AtomicU64,
    raw_read: AtomicU64,
    read: AtomicU64,
    runs: AtomicU64,
    merges: AtomicU64,
    merge_passes: AtomicU64,
    max_fan_in: AtomicU64,
    live: AtomicU64,
    peak: AtomicU64,
}

impl Owner {
    pub fn label(&self) -> &str {
        &self.label
    }

    pub(super) fn wrote(&self, items: u64, raw: u64, bytes: u64) {
        self.items_written.fetch_add(items, Ordering::Relaxed);
        self.raw_written.fetch_add(raw, Ordering::Relaxed);
        self.written.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(super) fn read(&self, items: u64, raw: u64, bytes: u64) {
        self.items_read.fetch_add(items, Ordering::Relaxed);
        self.raw_read.fetch_add(raw, Ordering::Relaxed);
        self.read.fetch_add(bytes, Ordering::Relaxed);
    }

    pub(super) fn add_run(&self) {
        self.runs.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn merged(&self, fan_in: usize, passes: u64) {
        self.merges.fetch_add(1, Ordering::Relaxed);
        self.max_fan_in.fetch_max(fan_in as u64, Ordering::Relaxed);
        self.merge_passes.fetch_max(passes, Ordering::Relaxed);
    }

    pub fn traffic(&self) -> Traffic {
        let load = |value: &AtomicU64| value.load(Ordering::Relaxed);
        Traffic {
            items_written: load(&self.items_written),
            raw_written_bytes: load(&self.raw_written),
            written_bytes: load(&self.written),
            items_read: load(&self.items_read),
            raw_read_bytes: load(&self.raw_read),
            read_bytes: load(&self.read),
            runs: load(&self.runs),
            merges: load(&self.merges),
            merge_passes: load(&self.merge_passes),
            max_fan_in: load(&self.max_fan_in),
            live_bytes: load(&self.live),
            peak_bytes: load(&self.peak),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Traffic {
    pub items_written: u64,
    pub raw_written_bytes: u64,
    pub written_bytes: u64,
    pub items_read: u64,
    pub raw_read_bytes: u64,
    pub read_bytes: u64,
    pub runs: u64,
    pub merges: u64,
    pub merge_passes: u64,
    pub max_fan_in: u64,
    pub live_bytes: u64,
    pub peak_bytes: u64,
}

impl Traffic {
    /// The traffic between `earlier` and this. Live and peak bytes, merge
    /// passes and fan-in stay as they are now: they are maxima, not sums.
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            items_written: self.items_written - earlier.items_written,
            raw_written_bytes: self.raw_written_bytes - earlier.raw_written_bytes,
            written_bytes: self.written_bytes - earlier.written_bytes,
            items_read: self.items_read - earlier.items_read,
            raw_read_bytes: self.raw_read_bytes - earlier.raw_read_bytes,
            read_bytes: self.read_bytes - earlier.read_bytes,
            runs: self.runs - earlier.runs,
            merges: self.merges - earlier.merges,
            ..*self
        }
    }

    pub fn is_idle(&self) -> bool {
        self.written_bytes == 0 && self.read_bytes == 0 && self.merges == 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IoTimes {
    pub write: Duration,
    pub read_wait: Duration,
    pub spill_wait: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortfall {
    pub message: String,
}

impl From<Shortfall> for Error {
    fn from(shortfall: Shortfall) -> Self {
        Error::OutOfResources(shortfall.message)
    }
}

impl Scratch {
    pub fn create(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let path = path.into();
        fs::create_dir_all(&path).map_err(|source| Error::IoPath {
            action: "failed to create scratch directory",
            path: path.clone(),
            source,
        })?;
        Ok(Arc::new(Self::with_counter(path, 0)))
    }

    /// The scratch directory of a build that stopped, to go on from its
    /// checkpoint: everything `files` (the files and directories the
    /// checkpoint names) leaves out is removed, and `files` are pinned
    /// again. Each restored owner counts its own files with
    /// [`Scratch::adopt`].
    pub fn reopen(
        path: impl Into<PathBuf>,
        files: &[PathBuf],
        next_file: u64,
    ) -> Result<Arc<Self>> {
        let path = path.into();
        let named = files.iter().collect::<HashSet<_>>();
        let io = |action: &'static str, path: &Path| {
            let path = path.to_path_buf();
            move |source| Error::IoPath {
                action,
                path,
                source,
            }
        };
        for entry in fs::read_dir(&path).map_err(io("failed to read scratch directory", &path))? {
            let entry = entry?;
            let found = entry.path();
            if entry.file_name() == MANIFEST_FILE || named.contains(&found) {
                continue;
            }
            let removed = if entry.file_type()?.is_dir() {
                fs::remove_dir_all(&found)
            } else {
                fs::remove_file(&found)
            };
            removed.map_err(io("failed to remove leftover scratch", &found))?;
        }
        let scratch = Self::with_counter(path, next_file);
        {
            let mut pins = scratch.pins.lock().expect("pins lock");
            pins.held = true;
            let pins = &mut *pins;
            pins.files = files.iter().cloned().collect();
            pins.synced.clone_from(&pins.files);
        }
        Ok(Arc::new(scratch))
    }

    fn with_counter(path: PathBuf, next_file: u64) -> Self {
        let disk = DiskProbe {
            free: resources::free_disk(&path),
            ..DiskProbe::default()
        };
        Self {
            path,
            next_file: AtomicU64::new(next_file),
            cache_bytes: AtomicU64::new(0),
            cancel: OnceLock::new(),
            budget: AtomicU64::new(u64::MAX),
            live: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            stage_peak: AtomicU64::new(0),
            written: AtomicU64::new(0),
            level: AtomicI32::new(LEVELS[0]),
            owners: Mutex::new(BTreeMap::new()),
            peak_owners: Mutex::new((0, BTreeMap::new())),
            disk: Mutex::new(disk),
            watched: Mutex::new(Vec::new()),
            stage: Mutex::new("start"),
            write_nanos: AtomicU64::new(0),
            read_wait_nanos: AtomicU64::new(0),
            spill_wait_nanos: AtomicU64::new(0),
            pins: Mutex::new(Pins::default()),
            retained: AtomicU64::new(0),
            peak_retained: AtomicU64::new(0),
        }
    }

    pub fn set_budget(&self, bytes: Option<u64>) {
        self.budget
            .store(bytes.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    pub fn budget(&self) -> Option<u64> {
        Some(self.budget.load(Ordering::Relaxed)).filter(|budget| *budget != u64::MAX)
    }

    pub fn cancel_on(&self, cancel: Arc<AtomicBool>) {
        let _ = self.cancel.set(cancel);
    }

    /// Fails with [`BuildCancelled`] once the flag given to
    /// [`Scratch::cancel_on`] is set.
    pub fn check_cancelled(&self) -> Result<()> {
        if self
            .cancel
            .get()
            .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
        {
            return Err(BuildCancelled.into());
        }
        Ok(())
    }

    pub fn set_cache_bytes(&self, bytes: u64) {
        self.cache_bytes.store(bytes, Ordering::Relaxed);
    }

    pub fn set_stage(&self, stage: &'static str) {
        *self.stage.lock().expect("stage lock") = stage;
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn spilled_bytes(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    pub fn live_bytes(&self) -> u64 {
        self.live.load(Ordering::Relaxed)
    }

    pub fn peak_bytes(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    pub fn peak_owners(&self) -> BTreeMap<String, u64> {
        self.peak_owners.lock().expect("peak lock").1.clone()
    }

    pub fn stage_peak_bytes(&self) -> u64 {
        self.stage_peak.load(Ordering::Relaxed)
    }

    pub fn reset_stage_peak(&self) {
        self.stage_peak.store(self.live_bytes(), Ordering::Relaxed);
    }

    pub fn level(&self) -> i32 {
        self.level.load(Ordering::Relaxed)
    }

    pub(super) fn raise_level(&self, from: i32) -> bool {
        let Some(next) = LEVELS.iter().copied().find(|level| *level > from) else {
            return false;
        };
        self.level.fetch_max(next, Ordering::Relaxed);
        true
    }

    pub fn io_times(&self) -> IoTimes {
        let load = |value: &AtomicU64| Duration::from_nanos(value.load(Ordering::Relaxed));
        IoTimes {
            write: load(&self.write_nanos),
            read_wait: load(&self.read_wait_nanos),
            spill_wait: load(&self.spill_wait_nanos),
        }
    }

    pub(super) fn add_write_time(&self, time: Duration) {
        self.write_nanos.fetch_add(nanos(time), Ordering::Relaxed);
    }

    pub(super) fn add_read_wait(&self, time: Duration) {
        self.read_wait_nanos
            .fetch_add(nanos(time), Ordering::Relaxed);
    }

    pub(super) fn add_spill_wait(&self, time: Duration) {
        self.spill_wait_nanos
            .fetch_add(nanos(time), Ordering::Relaxed);
    }

    pub fn owner(&self, label: &str) -> Arc<Owner> {
        let mut owners = self.owners.lock().expect("owners lock");
        if let Some(owner) = owners.get(label) {
            return Arc::clone(owner);
        }
        let owner = Arc::new(Owner {
            label: label.to_string(),
            ..Owner::default()
        });
        owners.insert(label.to_string(), Arc::clone(&owner));
        owner
    }

    pub fn traffic(&self) -> BTreeMap<String, Traffic> {
        self.owners
            .lock()
            .expect("owners lock")
            .iter()
            .map(|(label, owner)| (label.clone(), owner.traffic()))
            .collect()
    }

    /// Reserve `bytes` for `owner` before writing them, or say why not.
    /// Retired files are removed to make room before the ledger refuses.
    pub fn reserve(&self, owner: &Owner, bytes: u64) -> std::result::Result<(), Shortfall> {
        loop {
            match self.try_reserve(owner, bytes) {
                Err(shortfall) if !self.reclaim() => return Err(shortfall),
                Err(_) => {}
                Ok(()) => return Ok(()),
            }
        }
    }

    fn try_reserve(&self, owner: &Owner, bytes: u64) -> std::result::Result<(), Shortfall> {
        let cap = self.cap(bytes);
        let mut live = self.live.load(Ordering::Relaxed);
        let next = loop {
            let next = live.saturating_add(bytes);
            if next > cap {
                return Err(self.shortfall(owner, bytes, cap));
            }
            match self
                .live
                .compare_exchange_weak(live, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break next,
                Err(current) => live = current,
            }
        };
        let owned = owner.live.fetch_add(bytes, Ordering::Relaxed) + bytes;
        raise(&owner.peak, owned);
        self.record_peak(next);
        Ok(())
    }

    /// Count `bytes` already on disk as `owner`'s, as a restored checkpoint
    /// finds them. Never refused: they are there whether counted or not.
    pub fn adopt(&self, owner: &Owner, bytes: u64) {
        let live = self.live.fetch_add(bytes, Ordering::Relaxed) + bytes;
        let owned = owner.live.fetch_add(bytes, Ordering::Relaxed) + bytes;
        owner.peak.fetch_max(owned, Ordering::Relaxed);
        self.record_peak(live);
    }

    pub fn release(&self, owner: &Owner, bytes: u64) {
        self.live.fetch_sub(bytes, Ordering::Relaxed);
        owner.live.fetch_sub(bytes, Ordering::Relaxed);
    }

    pub fn remove(&self, owner: &Owner, path: &Path, bytes: u64) {
        if !self.retire(path, bytes) {
            let _ = fs::remove_file(path);
        }
        self.release(owner, bytes);
    }

    pub fn remove_dir(&self, path: &Path) -> Result<()> {
        if !self.retire(path, directory_bytes(path)) {
            fs::remove_dir_all(path).map_err(|source| Error::IoPath {
                action: "failed to remove",
                path: path.to_path_buf(),
                source,
            })?;
        }
        self.unwatch(path);
        Ok(())
    }

    fn retire(&self, path: &Path, bytes: u64) -> bool {
        let mut pins = self.pins.lock().expect("pins lock");
        if !pins.held || !pins.files.contains(path) {
            return false;
        }
        pins.retired.push((path.to_path_buf(), bytes));
        let retained = self.retained.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak_retained.fetch_max(retained, Ordering::Relaxed);
        true
    }

    fn reclaim(&self) -> bool {
        let mut pins = self.pins.lock().expect("pins lock");
        if pins.retired.is_empty() {
            return false;
        }
        for (path, bytes) in pins.retired.drain(..) {
            remove_path(&path);
            self.retained.fetch_sub(bytes, Ordering::Relaxed);
        }
        pins.held = false;
        pins.files.clear();
        pins.voided = Some(self.stage.lock().expect("stage lock").to_string());
        let _ = fs::remove_file(self.path.join(MANIFEST_FILE));
        drop(pins);
        let mut disk = self.disk.lock().expect("disk lock");
        disk.free = resources::free_disk(&self.path);
        disk.live = self.live_bytes();
        disk.reserved_since = 0;
        true
    }

    /// Make the checkpoint `manifest` describes durable: sync every one of
    /// `files` (files and directories in the scratch directory, all it
    /// names) not synced before, then replace the manifest atomically.
    /// Retired files it no longer names are removed.
    pub fn commit(&self, files: &[PathBuf], manifest: &[u8]) -> Result<()> {
        let synced = self.pins.lock().expect("pins lock").synced.clone();
        let mut newly = Vec::new();
        for path in files {
            if !synced.contains(path) {
                sync_path(path)?;
                newly.push(path.clone());
            }
        }
        let pending = self.path.join(format!("{MANIFEST_FILE}.pending"));
        let target = self.path.join(MANIFEST_FILE);
        let io = |source| Error::IoPath {
            action: "failed to write the build checkpoint",
            path: target.clone(),
            source,
        };
        let mut file = File::create(&pending).map_err(io)?;
        file.write_all(manifest).map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);
        fs::rename(&pending, &target).map_err(io)?;
        sync_directory(&self.path);
        let owner = self.owner(CHECKPOINT_LABEL);
        self.release(&owner, owner.live.load(Ordering::Relaxed));
        self.adopt(&owner, manifest.len() as u64);
        let mut pins = self.pins.lock().expect("pins lock");
        pins.held = true;
        pins.files = files.iter().cloned().collect();
        pins.synced.extend(newly);
        let (kept, gone): (Vec<_>, Vec<_>) = std::mem::take(&mut pins.retired)
            .into_iter()
            .partition(|(path, _)| pins.files.contains(path));
        pins.retired = kept;
        for (path, bytes) in gone {
            remove_path(&path);
            self.retained.fetch_sub(bytes, Ordering::Relaxed);
        }
        Ok(())
    }

    pub fn resumable(&self) -> bool {
        self.pins.lock().expect("pins lock").held
    }

    pub fn complete(&self) {
        let mut pins = self.pins.lock().expect("pins lock");
        pins.held = false;
        pins.files.clear();
        let _ = fs::remove_file(self.path.join(MANIFEST_FILE));
        let owner = self.owner(CHECKPOINT_LABEL);
        self.release(&owner, owner.live.load(Ordering::Relaxed));
    }

    pub fn voided(&self) -> Option<String> {
        self.pins.lock().expect("pins lock").voided.clone()
    }

    pub fn retained_bytes(&self) -> u64 {
        self.retained.load(Ordering::Relaxed)
    }

    pub fn peak_retained_bytes(&self) -> u64 {
        self.peak_retained.load(Ordering::Relaxed)
    }

    pub fn next_file(&self) -> u64 {
        self.next_file.load(Ordering::Relaxed)
    }

    pub fn ledger(&self) -> Ledger {
        Ledger {
            written: self.spilled_bytes(),
            peak: self.peak_bytes(),
            peak_owners: self.peak_owners(),
            peak_retained: self.peak_retained_bytes(),
            owners: self.traffic(),
        }
    }

    pub fn seed(&self, ledger: &Ledger) {
        self.written.fetch_add(ledger.written, Ordering::Relaxed);
        self.peak_retained
            .fetch_max(ledger.peak_retained, Ordering::Relaxed);
        if self.peak.fetch_max(ledger.peak, Ordering::Relaxed) < ledger.peak {
            *self.peak_owners.lock().expect("peak lock") =
                (ledger.peak, ledger.peak_owners.clone());
        }
        for (label, traffic) in &ledger.owners {
            let owner = self.owner(label);
            let add = |counter: &AtomicU64, value: u64| {
                counter.fetch_add(value, Ordering::Relaxed);
            };
            add(&owner.items_written, traffic.items_written);
            add(&owner.raw_written, traffic.raw_written_bytes);
            add(&owner.written, traffic.written_bytes);
            add(&owner.items_read, traffic.items_read);
            add(&owner.raw_read, traffic.raw_read_bytes);
            add(&owner.read, traffic.read_bytes);
            add(&owner.runs, traffic.runs);
            add(&owner.merges, traffic.merges);
            owner
                .merge_passes
                .fetch_max(traffic.merge_passes, Ordering::Relaxed);
            owner
                .max_fan_in
                .fetch_max(traffic.max_fan_in, Ordering::Relaxed);
            owner.peak.fetch_max(traffic.peak_bytes, Ordering::Relaxed);
        }
    }

    fn cap(&self, bytes: u64) -> u64 {
        let budget = self
            .budget
            .load(Ordering::Relaxed)
            .saturating_sub(self.retained_bytes());
        let mut disk = self.disk.lock().expect("disk lock");
        disk.reserved_since += bytes;
        if disk.reserved_since >= DISK_PROBE_BYTES {
            disk.free = resources::free_disk(&self.path);
            disk.live = self.live_bytes();
            disk.reserved_since = 0;
        }
        match disk.free {
            Some(free) => budget.min(disk.live.saturating_add(free / 10 * 9)),
            None => budget,
        }
    }

    fn record_peak(&self, live: u64) {
        raise(&self.stage_peak, live);
        if self.peak.load(Ordering::Relaxed) >= live
            || self.peak.fetch_max(live, Ordering::Relaxed) >= live
        {
            return;
        }
        let mut peak = self.peak_owners.lock().expect("peak lock");
        if live < peak.0.saturating_add((peak.0 / 64).max(1 << 20)) && peak.0 != 0 {
            return;
        }
        *peak = (live, self.live_by_owner());
    }

    fn live_by_owner(&self) -> BTreeMap<String, u64> {
        self.owners
            .lock()
            .expect("owners lock")
            .iter()
            .map(|(label, owner)| (label.clone(), owner.live.load(Ordering::Relaxed)))
            .filter(|(_, live)| *live > 0)
            .collect()
    }

    fn shortfall(&self, owner: &Owner, bytes: u64, cap: u64) -> Shortfall {
        let live = self.live_bytes();
        let owners = self
            .live_by_owner()
            .into_iter()
            .map(|(label, bytes)| format!("{label} {} MiB", mib(bytes)))
            .collect::<Vec<_>>()
            .join(", ");
        let limit = match self.budget() {
            Some(budget) if budget.saturating_sub(self.retained_bytes()) <= cap => {
                format!("the scratch budget of {} MiB", mib(budget))
            }
            _ => format!(
                "the {} MiB the scratch volume of {} leaves",
                mib(cap),
                self.path.display()
            ),
        };
        Shortfall {
            message: format!(
                "scratch disk is full in stage {}: {} needs {} MiB more, and {} MiB is live ({owners}) against {limit}, compressed at zstd level {}; it would fit in {} MiB: pass a larger --scratch-budget-mb or --scratch-dir on a larger volume",
                self.stage.lock().expect("stage lock"),
                owner.label,
                mib(bytes).max(1),
                mib(live),
                self.level(),
                mib(live.saturating_add(bytes)).max(1),
            ),
        }
    }

    pub fn watch(&self, label: &str, path: &Path) {
        let owner = self.owner(label);
        self.watched
            .lock()
            .expect("watched lock")
            .push((owner, path.to_path_buf()));
        self.measure();
    }

    pub fn unwatch(&self, path: &Path) {
        let mut watched = self.watched.lock().expect("watched lock");
        watched.retain(|(owner, watched)| {
            if watched != path {
                return true;
            }
            let bytes = owner.live.swap(0, Ordering::Relaxed);
            self.live.fetch_sub(bytes, Ordering::Relaxed);
            false
        });
    }

    pub fn measure(&self) {
        for (owner, path) in self.watched.lock().expect("watched lock").iter() {
            let bytes = directory_bytes(path);
            let before = owner.live.swap(bytes, Ordering::Relaxed);
            owner.peak.fetch_max(bytes, Ordering::Relaxed);
            if bytes >= before {
                let grown = bytes - before;
                owner.written.fetch_add(grown, Ordering::Relaxed);
                owner.raw_written.fetch_add(grown, Ordering::Relaxed);
                let live = self.live.fetch_add(grown, Ordering::Relaxed) + grown;
                self.record_peak(live);
            } else {
                self.live.fetch_sub(before - bytes, Ordering::Relaxed);
            }
        }
        let mut disk = self.disk.lock().expect("disk lock");
        disk.free = resources::free_disk(&self.path);
        disk.live = self.live_bytes();
        disk.reserved_since = 0;
    }

    pub(super) fn create_file(&self, path: &Path) -> std::io::Result<File> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(windows)]
        if self.spilled_bytes() < self.cache_bytes.load(Ordering::Relaxed) {
            use std::os::windows::fs::OpenOptionsExt;
            options.attributes(windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TEMPORARY);
        }
        options.open(path)
    }

    pub(crate) fn new_file(&self, label: &str) -> PathBuf {
        let index = self.next_file.fetch_add(1, Ordering::Relaxed);
        self.path.join(format!("{label}-{index:06}.bin"))
    }

    pub(super) fn count_written(&self, bytes: u64) {
        self.written.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.resumable() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    pub written: u64,
    pub peak: u64,
    pub peak_owners: BTreeMap<String, u64>,
    pub peak_retained: u64,
    pub owners: BTreeMap<String, Traffic>,
}

fn remove_path(path: &Path) {
    let _ = if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
}

fn sync_path(path: &Path) -> Result<()> {
    let io = |source| Error::IoPath {
        action: "failed to sync scratch file",
        path: path.to_path_buf(),
        source,
    };
    if path.is_dir() {
        for entry in fs::read_dir(path).map_err(io)? {
            sync_path(&entry.map_err(io)?.path())?;
        }
        sync_directory(path);
        return Ok(());
    }
    // Windows flushes a file only through a handle that may write it.
    File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(io)
}

fn sync_directory(path: &Path) {
    #[cfg(unix)]
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn nanos(time: Duration) -> u64 {
    u64::try_from(time.as_nanos()).unwrap_or(u64::MAX)
}

fn mib(bytes: u64) -> u64 {
    bytes.div_ceil(1 << 20)
}

fn directory_bytes(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| match entry.metadata() {
            Ok(metadata) if metadata.is_dir() => fs::read_dir(entry.path())
                .map(|inner| {
                    inner
                        .filter_map(|entry| entry.ok()?.metadata().ok())
                        .filter(|metadata| metadata.is_file())
                        .map(|metadata| metadata.len())
                        .sum()
                })
                .unwrap_or(0),
            Ok(metadata) => metadata.len(),
            Err(_) => 0,
        })
        .sum()
}

#[derive(Debug)]
pub struct ScratchFile {
    /// Taken before the file is removed: Windows cannot remove an open file.
    file: Option<File>,
    path: PathBuf,
    scratch: Arc<Scratch>,
    owner: Arc<Owner>,
    bytes: u64,
}

impl ScratchFile {
    pub fn create(scratch: &Arc<Scratch>, label: &str, name: &str) -> Result<Self> {
        let path = scratch.path().join(name);
        let file = scratch.create_file(&path).map_err(|source| Error::IoPath {
            action: "failed to create scratch file",
            path: path.clone(),
            source,
        })?;
        Ok(Self {
            file: Some(file),
            path,
            scratch: Arc::clone(scratch),
            owner: scratch.owner(label),
            bytes: 0,
        })
    }

    pub fn reopen(scratch: &Arc<Scratch>, label: &str, name: &str, bytes: u64) -> Result<Self> {
        let path = scratch.path().join(name);
        let failed = |source| Error::IoPath {
            action: "failed to reopen scratch file",
            path: path.clone(),
            source,
        };
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(failed)?;
        if file.metadata().map_err(failed)?.len() < bytes {
            return Err(Error::BadScratch {
                path,
                detail: "a checkpoint names more bytes than the file holds",
            });
        }
        file.set_len(bytes).map_err(failed)?;
        io::Seek::seek(&mut file, io::SeekFrom::End(0)).map_err(failed)?;
        let owner = scratch.owner(label);
        scratch.adopt(&owner, bytes);
        Ok(Self {
            file: Some(file),
            path,
            scratch: Arc::clone(scratch),
            owner,
            bytes,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn label(&self) -> &str {
        self.owner.label()
    }

    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

fn raise(peak: &AtomicU64, value: u64) {
    if peak.load(Ordering::Relaxed) < value {
        peak.fetch_max(value, Ordering::Relaxed);
    }
}

impl Write for ScratchFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let asked = buffer.len() as u64;
        self.scratch
            .reserve(&self.owner, asked)
            .map_err(|shortfall| io::Error::other(shortfall.message))?;
        let started = Instant::now();
        let written = self
            .file
            .as_mut()
            .expect("open until dropped")
            .write(buffer);
        self.scratch.add_write_time(started.elapsed());
        let done = written.as_ref().map_or(0, |written| *written as u64);
        self.scratch.release(&self.owner, asked - done);
        self.bytes += done;
        self.owner.wrote(0, done, done);
        self.scratch.count_written(done);
        written
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.as_mut().expect("open until dropped").flush()
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        drop(self.file.take());
        self.scratch.remove(&self.owner, &self.path, self.bytes);
    }
}
