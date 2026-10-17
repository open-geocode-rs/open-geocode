//! Disk-backed sorting and spooling for builds larger than memory.
//!
//! [`ExternalSorter`] buffers items until its fair share of the build
//! [`Budget`] is used (or the shared pool runs out), sorts the buffer,
//! and writes it to a run file in the build scratch directory. Reading merges
//! every run with a k-way heap. When everything fits no run is written and the
//! sort is an ordinary in-memory sort, so a city extract and a planet build
//! share one code path.
//!
//! [`Spool`] is the unsorted sibling: an append-only scratch file replayed in
//! write order, once or, closed into a [`SpoolFile`], as often as needed.
//! [`IndexedSpool`] is read back by index, in any order.

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crate::error::{Error, Result};
use rayon::prelude::*;

use crate::{
    builder::BuildCancelled,
    memory::{Budget, Reservation, Spillable},
    resources,
    util::codec::{get_u64, put_u64},
};

const IO_BUFFER_BYTES: usize = 1 << 20;
/// Runs merged at once. Beyond this, runs are merged in rounds so the number of
/// open files and read buffers stays bounded.
const MAX_MERGE_FAN_IN: usize = 64;
/// When the shared pool is full, a sorter spills only once its buffer holds at
/// least this fraction of its share; smaller runs would cost more than they free.
const MIN_RUN_FRACTION: usize = 16;
/// Smallest buffer growth step, in items.
const MIN_CAPACITY: usize = 1_024;
/// Smallest read buffer per run during a merge.
const MIN_MERGE_BUFFER_BYTES: usize = 64 << 10;
/// Items a merge round writes between two checks of whether the build goes
/// on.
const CHECK_EVERY_ITEMS: usize = 1 << 16;

/// A value that can be written to and read back from scratch files.
pub trait Spill: Sized {
    fn encode(&self, out: &mut Vec<u8>);
    fn decode(input: &mut &[u8]) -> Result<Self>;
    /// Approximate bytes this value occupies in memory, counted against the
    /// sorter budget. Values that own heap data must include it.
    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

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
    spilled_bytes: AtomicU64,
    /// Spilled bytes up to which new files are marked temporary.
    cache_bytes: AtomicU64,
    /// Set to stop the build this scratch serves.
    cancel: OnceLock<Arc<AtomicBool>>,
}

impl Scratch {
    pub fn create(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let path = path.into();
        fs::create_dir_all(&path).map_err(|source| Error::IoPath {
            action: "failed to create scratch directory",
            path: path.clone(),
            source,
        })?;
        Ok(Arc::new(Self {
            path,
            next_file: AtomicU64::new(0),
            spilled_bytes: AtomicU64::new(0),
            cache_bytes: AtomicU64::new(0),
            cancel: OnceLock::new(),
        }))
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

    /// Fail with a clear message when about `bytes` more would not fit in
    /// 90% of the scratch volume's free space, as DuckDB caps its spill,
    /// instead of failing in the middle of a write.
    pub fn check_room(&self, bytes: u64) -> Result<()> {
        if let Some(free) = resources::free_disk(&self.path)
            && bytes > free / 10 * 9
        {
            return Err(Error::OutOfResources(format!(
                "scratch volume of {} has {} MiB free, and the build needs about {} MiB more; free space there or pass --scratch-dir on a larger volume",
                self.path.display(),
                free >> 20,
                bytes >> 20
            )));
        }
        Ok(())
    }

    /// Create a scratch file for writing.
    fn create_file(&self, path: &Path) -> std::io::Result<File> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(windows)]
        if self.spilled_bytes() < self.cache_bytes.load(Ordering::Relaxed) {
            use std::os::windows::fs::OpenOptionsExt;
            options.attributes(windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_TEMPORARY);
        }
        options.open(path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Total bytes written to scratch files so far.
    pub fn spilled_bytes(&self) -> u64 {
        self.spilled_bytes.load(Ordering::Relaxed)
    }

    fn new_file(&self, label: &str) -> PathBuf {
        let index = self.next_file.fetch_add(1, Ordering::Relaxed);
        self.path.join(format!("{label}-{index:06}.bin"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Spill statistics for one sorter, surfaced in the build report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SortStats {
    pub items: u64,
    pub runs: u64,
}

#[derive(Debug)]
pub struct ExternalSorter<T> {
    scratch: Arc<Scratch>,
    budget: Arc<Budget>,
    label: &'static str,
    /// The most bytes this sorter may buffer before it spills, whatever its
    /// fair share of the pool.
    share_bytes: usize,
    /// Buffer capacity plus the heap bytes of buffered items.
    reservation: Spillable,
    buffer: Vec<T>,
    runs: Vec<PathBuf>,
    stats: SortStats,
}

impl<T: Spill + Ord + Send> ExternalSorter<T> {
    pub fn new(
        scratch: &Arc<Scratch>,
        label: &'static str,
        budget: &Arc<Budget>,
        share_bytes: usize,
    ) -> Self {
        Self {
            scratch: Arc::clone(scratch),
            budget: Arc::clone(budget),
            label,
            share_bytes: share_bytes.max(1),
            reservation: Spillable::new(budget),
            buffer: Vec::new(),
            runs: Vec::new(),
            stats: SortStats::default(),
        }
    }

    pub fn push(&mut self, item: T) -> Result<()> {
        let heap = item
            .resident_bytes()
            .saturating_sub(std::mem::size_of::<T>());
        let share = self.reservation.share(self.share_bytes);
        let (mut growth, mut needed) = self.needed(heap, share);
        let over_share = self.reservation.bytes() + needed > share;
        if over_share || !self.reservation.reservation().try_grow(needed) {
            // Spill when the share is used up, or when the pool is full and
            // the buffer is big enough to be worth a run.
            if over_share || self.reservation.bytes() >= share / MIN_RUN_FRACTION {
                self.spill()?;
                (growth, needed) = self.needed(heap, share);
            }
            if !self.reservation.reservation().try_grow(needed) {
                self.reservation.reservation().force_grow(needed);
            }
        }
        if self.buffer.try_reserve_exact(growth).is_err() {
            // The allocator refused to grow the buffer: spill it and grow
            // from empty, as GNU sort halves its buffer rather than fail.
            self.spill()?;
            (growth, needed) = self.needed(heap, share);
            self.reservation.reservation().force_grow(needed);
            self.buffer.try_reserve_exact(growth).map_err(|_| {
                Error::OutOfResources(format!(
                    "out of memory: the {} sorter cannot buffer one item after spilling",
                    self.label
                ))
            })?;
        }
        self.buffer.push(item);
        self.stats.items += 1;
        Ok(())
    }

    /// Items the buffer must grow by to take one more item, and the bytes that
    /// costs including the item's own heap data.
    fn needed(&self, heap: usize, share: usize) -> (usize, usize) {
        let growth = if self.buffer.len() == self.buffer.capacity() {
            // Double, but never past what the share can hold.
            let share_items = (share / std::mem::size_of::<T>().max(1)).max(1);
            self.buffer
                .capacity()
                .max(MIN_CAPACITY)
                .min(share_items.saturating_sub(self.buffer.capacity()))
                .max(1)
        } else {
            0
        };
        (growth, heap + growth * std::mem::size_of::<T>())
    }

    pub fn stats(&self) -> SortStats {
        self.stats
    }

    /// Sorted items, ascending, with ties in insertion order.
    pub fn finish(mut self) -> Result<Sorted<T>> {
        if self.runs.is_empty() {
            // Stable sort plus run-order tie-breaking keeps equal items in push order.
            self.buffer.par_sort();
            let reservation =
                std::mem::replace(&mut self.reservation, Spillable::new(&self.budget))
                    .into_reservation();
            return Ok(Sorted::Memory(
                std::mem::take(&mut self.buffer).into_iter(),
                reservation,
            ));
        }
        // Once anything spilled, the tail is spilled too, so a finished sorter
        // holds only merge read buffers while later phases fill their own.
        self.spill()?;
        let share = self.reservation.share(self.share_bytes);
        while self.runs.len() > MAX_MERGE_FAN_IN {
            let batch = self.runs.drain(..MAX_MERGE_FAN_IN).collect::<Vec<_>>();
            let merged = self.scratch.new_file(self.label);
            let mut writer = SpoolWriter::<T>::create(&self.scratch, &merged)?;
            for (written, item) in Merge::<T>::open(&batch, &self.budget, share)?.enumerate() {
                if written % CHECK_EVERY_ITEMS == 0 {
                    self.scratch.check_cancelled()?;
                }
                writer.write(&item?)?;
            }
            writer.finish()?;
            self.runs.insert(0, merged);
        }
        Ok(Sorted::Merge(Merge::open(&self.runs, &self.budget, share)?))
    }

    fn spill(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.scratch.check_room(self.reservation.bytes() as u64)?;
        self.buffer.par_sort();
        let path = self.scratch.new_file(self.label);
        let mut writer = SpoolWriter::create(&self.scratch, &path)?;
        for item in self.buffer.drain(..) {
            writer.write(&item)?;
        }
        writer.finish()?;
        // Return the memory to the pool, not just the items.
        self.buffer = Vec::new();
        self.reservation.reservation().release_all();
        self.runs.push(path);
        self.stats.runs += 1;
        Ok(())
    }
}

/// Sorted output. The in-memory form keeps its memory reserved until dropped.
#[derive(Debug)]
pub enum Sorted<T> {
    Memory(std::vec::IntoIter<T>, Reservation),
    Merge(Merge<T>),
}

impl<T: Spill + Ord> Iterator for Sorted<T> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Memory(items, _) => items.next().map(Ok),
            Self::Merge(merge) => merge.next(),
        }
    }
}

/// K-way merge over sorted run files.
#[derive(Debug)]
pub struct Merge<T> {
    sources: Vec<SpoolReader<T>>,
    heap: BinaryHeap<Reverse<HeapEntry<T>>>,
    failed: bool,
    /// Read buffers of the open runs.
    _buffers: Reservation,
}

#[derive(Debug)]
struct HeapEntry<T> {
    item: T,
    source: usize,
}

impl<T: Ord> PartialEq for HeapEntry<T> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl<T: Ord> Eq for HeapEntry<T> {}

impl<T: Ord> PartialOrd for HeapEntry<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T: Ord> Ord for HeapEntry<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.item
            .cmp(&other.item)
            .then_with(|| self.source.cmp(&other.source))
    }
}

impl<T: Spill + Ord> Merge<T> {
    /// Each run is read exactly once, so its file is deleted as soon as the
    /// merge is dropped; at planet scale this frees scratch disk between phases.
    /// Read buffers are sized so all of them fit the sorter's share, between
    /// [`MIN_MERGE_BUFFER_BYTES`] and [`IO_BUFFER_BYTES`] each. They are needed
    /// to make progress, so they are reserved even when the pool is full.
    fn open(runs: &[PathBuf], budget: &Arc<Budget>, share_bytes: usize) -> Result<Self> {
        let buffer_bytes =
            (share_bytes / runs.len().max(1)).clamp(MIN_MERGE_BUFFER_BYTES, IO_BUFFER_BYTES);
        let mut buffers = Reservation::empty(budget);
        if !buffers.try_grow(runs.len() * buffer_bytes) {
            buffers.force_grow(runs.len() * buffer_bytes);
        }
        let mut sources = runs
            .iter()
            .map(|path| {
                let mut reader = SpoolReader::open(path, buffer_bytes)?;
                reader.remove_on_drop = true;
                Ok(reader)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut heap = BinaryHeap::with_capacity(sources.len());
        for (source, input) in sources.iter_mut().enumerate() {
            if let Some(item) = input.next() {
                heap.push(Reverse(HeapEntry {
                    item: item?,
                    source,
                }));
            }
        }
        Ok(Self {
            sources,
            heap,
            failed: false,
            _buffers: buffers,
        })
    }
}

impl<T: Spill + Ord> Iterator for Merge<T> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let Reverse(entry) = self.heap.pop()?;
        match self.sources[entry.source].next() {
            Some(Ok(item)) => self.heap.push(Reverse(HeapEntry {
                item,
                source: entry.source,
            })),
            Some(Err(error)) => {
                self.failed = true;
                return Some(Err(error));
            }
            None => {}
        }
        Some(Ok(entry.item))
    }
}

/// Append-only scratch file replayed in write order.
#[derive(Debug)]
pub struct Spool<T> {
    writer: SpoolWriter<T>,
    path: PathBuf,
}

impl<T: Spill> Spool<T> {
    pub fn create(scratch: &Arc<Scratch>, label: &str) -> Result<Self> {
        let path = scratch.new_file(label);
        Ok(Self {
            writer: SpoolWriter::create(scratch, &path)?,
            path,
        })
    }

    pub fn write(&mut self, item: &T) -> Result<()> {
        self.writer.write(item)
    }

    /// Items written so far.
    pub fn written(&self) -> u64 {
        self.writer.items
    }

    /// Close the spool and read it back from the start. The file is removed
    /// once the reader is dropped.
    pub fn into_reader(self) -> Result<SpoolReader<T>> {
        self.writer.finish()?;
        let mut reader = SpoolReader::open(&self.path, IO_BUFFER_BYTES)?;
        reader.remove_on_drop = true;
        Ok(reader)
    }

    /// Close the spool to read it back as many times as needed. The file is
    /// removed once the [`SpoolFile`] is dropped.
    pub fn finish(self) -> Result<SpoolFile<T>> {
        let items = self.writer.items;
        self.writer.finish()?;
        Ok(SpoolFile {
            path: self.path,
            items,
            _marker: PhantomData,
        })
    }
}

/// A closed spool, read from the start by each [`SpoolFile::reader`].
#[derive(Debug)]
pub struct SpoolFile<T> {
    path: PathBuf,
    items: u64,
    _marker: PhantomData<T>,
}

impl<T: Spill> SpoolFile<T> {
    pub fn reader(&self) -> Result<SpoolReader<T>> {
        SpoolReader::open(&self.path, IO_BUFFER_BYTES)
    }

    /// Items the spool holds.
    pub fn len(&self) -> u64 {
        self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items == 0
    }
}

impl<T> Drop for SpoolFile<T> {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
struct SpoolWriter<T> {
    file: BufWriter<File>,
    scratch: Arc<Scratch>,
    buffer: Vec<u8>,
    items: u64,
    /// Bytes written so far.
    bytes: u64,
    _marker: PhantomData<T>,
}

impl<T: Spill> SpoolWriter<T> {
    fn create(scratch: &Arc<Scratch>, path: &Path) -> Result<Self> {
        let file = scratch.create_file(path).map_err(|source| Error::IoPath {
            action: "failed to create scratch file",
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self {
            file: BufWriter::with_capacity(IO_BUFFER_BYTES, file),
            scratch: Arc::clone(scratch),
            buffer: Vec::new(),
            items: 0,
            bytes: 0,
            _marker: PhantomData,
        })
    }

    fn write(&mut self, item: &T) -> Result<()> {
        self.buffer.clear();
        item.encode(&mut self.buffer);
        let mut prefix = Vec::with_capacity(10);
        put_u64(&mut prefix, self.buffer.len() as u64);
        self.file.write_all(&prefix)?;
        self.file.write_all(&self.buffer)?;
        let bytes = (prefix.len() + self.buffer.len()) as u64;
        self.scratch
            .spilled_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        self.items += 1;
        self.bytes += bytes;
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        Ok(self.file.flush()?)
    }
}

/// A scratch file of items read back by their index, in any order and as
/// often as needed.
#[derive(Debug)]
pub struct IndexedSpool<T> {
    writer: SpoolWriter<T>,
    path: PathBuf,
    offsets: Vec<u64>,
}

impl<T: Spill> IndexedSpool<T> {
    pub fn create(scratch: &Arc<Scratch>, label: &str) -> Result<Self> {
        let path = scratch.new_file(label);
        Ok(Self {
            writer: SpoolWriter::create(scratch, &path)?,
            path,
            offsets: Vec::new(),
        })
    }

    pub fn write(&mut self, item: &T) -> Result<()> {
        self.offsets.push(self.writer.bytes);
        self.writer.write(item)
    }

    /// Bytes written so far.
    pub fn bytes(&self) -> u64 {
        self.writer.bytes
    }

    /// Close the file and map it to read items by index. The file is
    /// removed once the [`IndexedSpoolFile`] is dropped.
    pub fn finish(self) -> Result<IndexedSpoolFile<T>> {
        self.writer.finish()?;
        let file = File::open(&self.path).map_err(|source| Error::IoPath {
            action: "failed to open scratch file",
            path: self.path.clone(),
            source,
        })?;
        // SAFETY: the scratch file is this build's own and is not written
        // again while mapped.
        let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|source| Error::IoPath {
            action: "failed to map scratch file",
            path: self.path.clone(),
            source,
        })?;
        Ok(IndexedSpoolFile {
            map: Some(map),
            path: self.path,
            offsets: self.offsets,
            _marker: PhantomData,
        })
    }
}

/// A closed [`IndexedSpool`].
#[derive(Debug)]
pub struct IndexedSpoolFile<T> {
    /// Taken before the file is removed: Windows cannot remove a mapped file.
    map: Option<memmap2::Mmap>,
    path: PathBuf,
    offsets: Vec<u64>,
    _marker: PhantomData<T>,
}

impl<T: Spill> IndexedSpoolFile<T> {
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Bytes the file holds.
    pub fn bytes(&self) -> u64 {
        self.map.as_ref().map_or(0, |map| map.len() as u64)
    }

    /// Item `index`, decoded.
    pub fn get(&self, index: usize) -> Result<T> {
        let bytes = self.map.as_deref().unwrap_or_default();
        let mut input = usize::try_from(self.offsets[index])
            .ok()
            .and_then(|offset| bytes.get(offset..))
            .ok_or(Error::BadScratch {
                path: self.path.clone(),
                detail: "truncated",
            })?;
        let len = usize::try_from(get_u64(&mut input)?)
            .map_err(|_| Error::Decode("scratch item too large"))?;
        let mut item = input.get(..len).ok_or(Error::BadScratch {
            path: self.path.clone(),
            detail: "truncated",
        })?;
        let value = T::decode(&mut item)?;
        if !item.is_empty() {
            return Err(Error::BadScratch {
                path: self.path.clone(),
                detail: "item has trailing bytes",
            });
        }
        Ok(value)
    }
}

impl<T> Drop for IndexedSpoolFile<T> {
    fn drop(&mut self) {
        drop(self.map.take());
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug)]
pub struct SpoolReader<T> {
    file: BufReader<File>,
    path: PathBuf,
    buffer: Vec<u8>,
    remove_on_drop: bool,
    _marker: PhantomData<T>,
}

impl<T: Spill> SpoolReader<T> {
    fn open(path: &Path, buffer_bytes: usize) -> Result<Self> {
        let file = File::open(path).map_err(|source| Error::IoPath {
            action: "failed to open scratch file",
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self {
            file: BufReader::with_capacity(buffer_bytes, file),
            path: path.to_path_buf(),
            buffer: Vec::new(),
            remove_on_drop: false,
            _marker: PhantomData,
        })
    }

    fn read_item(&mut self) -> Result<Option<T>> {
        let Some(len) = self.read_len()? else {
            return Ok(None);
        };
        self.buffer.clear();
        self.buffer.try_reserve_exact(len).map_err(|_| {
            Error::OutOfResources(format!(
                "scratch item of {len} bytes in {} does not fit in memory",
                self.path.display()
            ))
        })?;
        self.buffer.resize(len, 0);
        self.file
            .read_exact(&mut self.buffer)
            .map_err(|_| Error::BadScratch {
                path: self.path.clone(),
                detail: "truncated",
            })?;
        let mut input = self.buffer.as_slice();
        let item = T::decode(&mut input)?;
        if !input.is_empty() {
            return Err(Error::BadScratch {
                path: self.path.clone(),
                detail: "item has trailing bytes",
            });
        }
        Ok(Some(item))
    }

    /// Length prefix of the next item, or `None` at a clean end of file.
    fn read_len(&mut self) -> Result<Option<usize>> {
        let mut prefix = [0u8; 10];
        let mut used = 0;
        loop {
            let mut byte = [0u8; 1];
            if self.file.read(&mut byte)? == 0 {
                if used == 0 {
                    return Ok(None);
                }
                return Err(Error::BadScratch {
                    path: self.path.clone(),
                    detail: "truncated length prefix",
                });
            }
            prefix[used] = byte[0];
            used += 1;
            if byte[0] & 0x80 == 0 {
                break;
            }
            if used == prefix.len() {
                return Err(Error::BadScratch {
                    path: self.path.clone(),
                    detail: "invalid length prefix",
                });
            }
        }
        let len = get_u64(&mut &prefix[..used])?;
        Ok(Some(
            usize::try_from(len).map_err(|_| Error::Decode("scratch item too large"))?,
        ))
    }
}

impl<T: Spill> Iterator for SpoolReader<T> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.read_item().transpose()
    }
}

impl<T> Drop for SpoolReader<T> {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

macro_rules! spill_tuple_of_unsigned {
    ($($name:ident: $ty:ty),+) => {
        impl Spill for ($($ty,)+) {
            fn encode(&self, out: &mut Vec<u8>) {
                let ($($name,)+) = *self;
                $(put_u64(out, u64::from($name));)+
            }

            fn decode(input: &mut &[u8]) -> Result<Self> {
                Ok(($(<$ty>::try_from(get_u64(input)?).map_err(|_| $crate::error::Error::Decode("scratch value out of range"))?,)+))
            }
        }
    };
}

spill_tuple_of_unsigned!(a: u64, b: u64);
spill_tuple_of_unsigned!(a: u64, b: u8, c: u64);

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> Arc<Scratch> {
        Scratch::create(std::env::temp_dir().join(format!(
            "open-geocode-extsort-{name}-{}",
            uuid::Uuid::new_v4()
        )))
        .expect("scratch")
    }

    fn pseudo_random(count: u64) -> Vec<(u64, u64)> {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        (0..count)
            .map(|index| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state % 1_000, index)
            })
            .collect()
    }

    #[test]
    fn in_memory_sort_writes_no_runs() {
        let scratch = scratch("memory");
        let mut sorter = ExternalSorter::new(&scratch, "pairs", &Budget::unlimited(), usize::MAX);
        let input = pseudo_random(10_000);
        for item in &input {
            sorter.push(*item).expect("push");
        }
        assert_eq!(sorter.stats().runs, 0);
        let sorted = sorter
            .finish()
            .expect("finish")
            .collect::<Result<Vec<_>>>()
            .expect("items");
        let mut expected = input;
        expected.sort();
        assert_eq!(sorted, expected);
        assert_eq!(scratch.spilled_bytes(), 0);
    }

    #[test]
    fn tiny_budget_spills_many_runs_and_merges_in_rounds() {
        let scratch = scratch("spill");
        // Each item counts 16 bytes, so this spills every 50 items: 400 runs,
        // more than one merge round.
        let mut sorter = ExternalSorter::new(&scratch, "pairs", &Budget::unlimited(), 50 * 16);
        let input = pseudo_random(20_000);
        for item in &input {
            sorter.push(*item).expect("push");
        }
        assert!(sorter.stats().runs as usize > MAX_MERGE_FAN_IN);
        let sorted = sorter
            .finish()
            .expect("finish")
            .collect::<Result<Vec<_>>>()
            .expect("items");
        let mut expected = input;
        expected.sort();
        assert_eq!(sorted, expected);
        assert!(scratch.spilled_bytes() > 0);
    }

    #[test]
    fn sorters_sharing_a_budget_stay_within_it() {
        let scratch = scratch("shared");
        let item = std::mem::size_of::<(u64, u64)>();
        // Each sorter's share alone would fit; together they do not.
        let budget = Budget::new(64 * 1024 * item);
        let mut first = ExternalSorter::new(&scratch, "first", &budget, 48 * 1024 * item);
        let mut second = ExternalSorter::new(&scratch, "second", &budget, 48 * 1024 * item);
        let input = pseudo_random(400_000);
        for pair in input.chunks(2) {
            first.push(pair[0]).expect("push");
            second.push(pair[1]).expect("push");
        }
        assert!(
            budget.peak() <= budget.limit(),
            "peak {} over {}",
            budget.peak(),
            budget.limit()
        );
        assert!(first.stats().runs > 0 && second.stats().runs > 0);
        let first = first.finish().expect("finish");
        // A spilled sorter writes its tail out: only read buffers stay reserved.
        assert_eq!(
            budget.used(),
            first_runs_buffers(&first) + second.reservation.bytes()
        );
        let mut merged = first.collect::<Result<Vec<_>>>().expect("first");
        merged.extend(
            second
                .finish()
                .expect("finish")
                .collect::<Result<Vec<_>>>()
                .expect("second"),
        );
        merged.sort();
        let mut expected = input;
        expected.sort();
        assert_eq!(merged, expected);
        assert_eq!(budget.used(), 0);
    }

    fn first_runs_buffers<T>(sorted: &Sorted<T>) -> usize {
        match sorted {
            Sorted::Memory(..) => 0,
            Sorted::Merge(merge) => merge._buffers.bytes(),
        }
    }

    #[test]
    fn run_files_are_removed_once_merged() {
        let scratch = scratch("cleanup");
        let mut sorter = ExternalSorter::new(&scratch, "pairs", &Budget::unlimited(), 10 * 16);
        for item in pseudo_random(1_000) {
            sorter.push(item).expect("push");
        }
        let files = || fs::read_dir(scratch.path()).expect("scratch").count();
        assert!(files() > 0);
        let sorted = sorter.finish().expect("finish");
        assert_eq!(sorted.count(), 1_000);
        assert_eq!(files(), 0);
    }

    #[test]
    fn equal_keys_keep_insertion_order_across_runs() {
        #[derive(Debug, Clone, PartialEq, Eq)]
        struct Keyed {
            key: u64,
            payload: u64,
        }
        impl PartialOrd for Keyed {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }
        impl Ord for Keyed {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                self.key.cmp(&other.key)
            }
        }
        impl Spill for Keyed {
            fn encode(&self, out: &mut Vec<u8>) {
                put_u64(out, self.key);
                put_u64(out, self.payload);
            }
            fn decode(input: &mut &[u8]) -> Result<Self> {
                Ok(Self {
                    key: get_u64(input)?,
                    payload: get_u64(input)?,
                })
            }
        }

        let scratch = scratch("stable");
        let mut sorter = ExternalSorter::new(
            &scratch,
            "keyed",
            &Budget::unlimited(),
            7 * std::mem::size_of::<Keyed>(),
        );
        for payload in 0..1_000 {
            sorter
                .push(Keyed {
                    key: payload % 3,
                    payload,
                })
                .expect("push");
        }
        let sorted = sorter
            .finish()
            .expect("finish")
            .collect::<Result<Vec<_>>>()
            .expect("items");
        for pair in sorted.windows(2) {
            if pair[0].key == pair[1].key {
                assert!(pair[0].payload < pair[1].payload);
            }
        }
    }

    #[test]
    fn spool_replays_in_write_order_and_cleans_up() {
        let scratch = scratch("spool");
        let mut spool = Spool::create(&scratch, "items").expect("spool");
        for item in [(3u64, 1u64), (1, 2), (2, 3)] {
            spool.write(&item).expect("write");
        }
        assert_eq!(spool.written(), 3);
        let reader = spool.into_reader().expect("reader");
        let path = reader.path.clone();
        let items = reader.collect::<Result<Vec<_>>>().expect("items");
        assert_eq!(items, vec![(3, 1), (1, 2), (2, 3)]);
        assert!(!path.exists());
    }

    #[test]
    fn scratch_directory_is_removed_on_drop() {
        let scratch = scratch("drop");
        let path = scratch.path().to_path_buf();
        let mut spool = Spool::create(&scratch, "items").expect("spool");
        spool.write(&(1u64, 1u64)).expect("write");
        drop(spool);
        drop(scratch);
        assert!(!path.exists());
    }
}
