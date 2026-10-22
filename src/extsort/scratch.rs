//! The build scratch directory and its disk ledger.
//!
//! Every byte og writes to scratch is reserved first: sorter runs, spools,
//! the Pack writer's section files and, measured at checkpoints since its
//! writes cannot be hooked, Tantivy's index directory. The ledger counts
//! what is live by owner and refuses a reservation past the cap, the
//! smaller of `--scratch-budget-mb` and 90% of what the volume had free, as
//! DuckDB caps its spill with `max_temp_directory_size`. A refused writer
//! degrades before it fails: spill blocks are compressed harder, a sorter
//! merges its older runs at that level, and only then does the build stop,
//! naming the stage, the live bytes by owner and what would fit.

use std::{
    collections::BTreeMap,
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

/// zstd levels a refused reservation steps through. Level 1 compresses at
/// several hundred MB/s per core; past 9 the gain is small and the cost
/// steep.
const LEVELS: [i32; 4] = [1, 3, 6, 9];
/// Bytes reserved between two reads of the volume's free space.
const DISK_PROBE_BYTES: u64 = 256 << 20;

/// Build scratch directory. Removed with everything in it when dropped.
///
/// Scratch files are never synced: a build that stops starts over, so only
/// the published Pack is made durable. On Windows they are marked temporary
/// while what was spilled fits the page cache the build leaves free (see
/// [`Scratch::set_cache_bytes`]): the lazy writer then keeps them in memory
/// instead of writing them out, which pays while they fit, but past that it
/// would push the Pack and the input out of the cache.
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
    next_file: AtomicU64,
    /// Spilled bytes up to which new files are marked temporary.
    cache_bytes: AtomicU64,
    /// Set to stop the build this scratch serves.
    cancel: OnceLock<Arc<AtomicBool>>,
    /// The most live bytes allowed; `u64::MAX` when only the volume caps it.
    budget: AtomicU64,
    live: AtomicU64,
    peak: AtomicU64,
    stage_peak: AtomicU64,
    /// Bytes ever written, after compression.
    written: AtomicU64,
    level: AtomicI32,
    owners: Mutex<BTreeMap<String, Arc<Owner>>>,
    /// Live bytes by owner when the peak was last recorded.
    peak_owners: Mutex<(u64, BTreeMap<String, u64>)>,
    disk: Mutex<DiskProbe>,
    /// Directories written by code that cannot reserve, measured instead.
    watched: Mutex<Vec<(Arc<Owner>, PathBuf)>>,
    stage: Mutex<&'static str>,
    write_nanos: AtomicU64,
    read_wait_nanos: AtomicU64,
    spill_wait_nanos: AtomicU64,
}

/// The volume's free space when last read, which caps the ledger as much as
/// the budget does.
#[derive(Debug, Default)]
struct DiskProbe {
    /// Free bytes, or `None` when the volume cannot be read.
    free: Option<u64>,
    /// Live bytes when `free` was read.
    live: u64,
    reserved_since: u64,
}

/// One owner of scratch disk: a sorter, a spool or a writer file, by label.
/// Sorters of one label, such as one per input, share an owner.
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

    /// A k-way merge ran; `passes` is how often the most merged item has
    /// now been merged.
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

/// What one owner wrote to and read from scratch. Raw bytes are before
/// compression, the others as on disk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Traffic {
    pub items_written: u64,
    pub raw_written_bytes: u64,
    pub written_bytes: u64,
    pub items_read: u64,
    pub raw_read_bytes: u64,
    pub read_bytes: u64,
    /// Sorted runs and spools written, merged runs included.
    pub runs: u64,
    /// K-way merges, the last one included.
    pub merges: u64,
    /// How often the most merged item was merged: 1 for one merge of every
    /// run, 2 when some were merged first.
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

/// Time the build's threads spent on scratch I/O and handing work over.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IoTimes {
    /// In scratch writes, summed over writing threads.
    pub write: Duration,
    /// Readers waiting for a block of scratch to be read ahead.
    pub read_wait: Duration,
    /// Producers waiting for a sorter's previous buffer to be written.
    pub spill_wait: Duration,
}

/// A reservation the ledger refused, with what the build should be told.
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
        let disk = DiskProbe {
            free: resources::free_disk(&path),
            ..DiskProbe::default()
        };
        Ok(Arc::new(Self {
            path,
            next_file: AtomicU64::new(0),
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
        }))
    }

    /// Cap the live scratch bytes, below what the volume caps them to.
    pub fn set_budget(&self, bytes: Option<u64>) {
        self.budget
            .store(bytes.unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    pub fn budget(&self) -> Option<u64> {
        Some(self.budget.load(Ordering::Relaxed)).filter(|budget| *budget != u64::MAX)
    }

    /// Stop the work that holds this scratch once `cancel` is set: merges,
    /// and the stages that check [`Scratch::check_cancelled`].
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

    /// Mark new scratch files temporary while fewer than `bytes` have been
    /// spilled.
    pub fn set_cache_bytes(&self, bytes: u64) {
        self.cache_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Name the stage that reserves next, for a refusal's message.
    pub fn set_stage(&self, stage: &'static str) {
        *self.stage.lock().expect("stage lock") = stage;
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Total bytes written to scratch files so far, as on disk.
    pub fn spilled_bytes(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    pub fn live_bytes(&self) -> u64 {
        self.live.load(Ordering::Relaxed)
    }

    pub fn peak_bytes(&self) -> u64 {
        self.peak.load(Ordering::Relaxed)
    }

    /// Live bytes by owner when the peak was reached.
    pub fn peak_owners(&self) -> BTreeMap<String, u64> {
        self.peak_owners.lock().expect("peak lock").1.clone()
    }

    /// Most live bytes since the last [`Scratch::reset_stage_peak`].
    pub fn stage_peak_bytes(&self) -> u64 {
        self.stage_peak.load(Ordering::Relaxed)
    }

    pub fn reset_stage_peak(&self) {
        self.stage_peak.store(self.live_bytes(), Ordering::Relaxed);
    }

    /// The zstd level new blocks are compressed at.
    pub fn level(&self) -> i32 {
        self.level.load(Ordering::Relaxed)
    }

    /// Compress harder from now on, if `from` is not already behind the
    /// level. False once the highest level is reached.
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

    /// The owner of everything written under `label`.
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

    /// What each owner wrote and read so far.
    pub fn traffic(&self) -> BTreeMap<String, Traffic> {
        self.owners
            .lock()
            .expect("owners lock")
            .iter()
            .map(|(label, owner)| (label.clone(), owner.traffic()))
            .collect()
    }

    /// Reserve `bytes` for `owner` before writing them, or say why not.
    pub fn reserve(&self, owner: &Owner, bytes: u64) -> std::result::Result<(), Shortfall> {
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
        owner.peak.fetch_max(owned, Ordering::Relaxed);
        self.record_peak(next);
        Ok(())
    }

    /// Return bytes a deleted file held.
    pub fn release(&self, owner: &Owner, bytes: u64) {
        self.live.fetch_sub(bytes, Ordering::Relaxed);
        owner.live.fetch_sub(bytes, Ordering::Relaxed);
    }

    /// The most live bytes allowed now: the budget, or what the volume had
    /// free when last read, whichever is less. The volume is read again
    /// every [`DISK_PROBE_BYTES`], since the Pack and other programs fill it
    /// too.
    fn cap(&self, bytes: u64) -> u64 {
        let budget = self.budget.load(Ordering::Relaxed);
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
        self.stage_peak.fetch_max(live, Ordering::Relaxed);
        if self.peak.fetch_max(live, Ordering::Relaxed) >= live {
            return;
        }
        // Owners are listed again only once the peak has grown by a step,
        // not on every block.
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
            Some(budget) if budget <= cap => format!("the scratch budget of {} MiB", mib(budget)),
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

    /// Count `path`, a directory written by code that cannot reserve, as
    /// `label`'s, measured by [`Scratch::measure`].
    pub fn watch(&self, label: &str, path: &Path) {
        let owner = self.owner(label);
        self.watched
            .lock()
            .expect("watched lock")
            .push((owner, path.to_path_buf()));
        self.measure();
    }

    /// Stop measuring `path`, once it is removed.
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

    /// Measure the watched directories, one level deep, and the volume's
    /// free space. Called at checkpoints: a watched directory past the cap
    /// fails the next reservation.
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

    /// Create a scratch file for writing.
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

    pub(super) fn new_file(&self, label: &str) -> PathBuf {
        let index = self.next_file.fetch_add(1, Ordering::Relaxed);
        self.path.join(format!("{label}-{index:06}.bin"))
    }

    pub(super) fn count_written(&self, bytes: u64) {
        self.written.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn nanos(time: Duration) -> u64 {
    u64::try_from(time.as_nanos()).unwrap_or(u64::MAX)
}

fn mib(bytes: u64) -> u64 {
    bytes.div_ceil(1 << 20)
}

/// Bytes of the files in `path` and in the directories right below it.
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

/// A plain scratch file whose every write is reserved, such as a Pack
/// section streamed before it is copied in. Removed, and its bytes
/// returned, when dropped.
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
    /// Create `name` in the scratch directory, its bytes counted as
    /// `label`'s.
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

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
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
        let _ = fs::remove_file(&self.path);
        self.scratch.release(&self.owner, self.bytes);
    }
}
