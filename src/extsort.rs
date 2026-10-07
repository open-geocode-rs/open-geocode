//! Disk-backed sorting and spooling for builds larger than memory.
//!
//! [`ExternalSorter`] buffers items until a byte budget is reached, sorts the
//! buffer, and writes it to a run file in the build scratch directory. Reading
//! merges every run with a k-way heap. When everything fits in the budget no run
//! is written and the sort is an ordinary in-memory sort, so a city extract and
//! a planet build share one code path.
//!
//! [`Spool`] is the unsorted sibling: an append-only scratch file replayed in
//! write order.

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    fs::{self, File},
    io::{BufReader, BufWriter, Read, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;

use crate::util::codec::{get_u64, put_u64};

const IO_BUFFER_BYTES: usize = 1 << 20;
/// Runs merged at once. Beyond this, runs are merged in rounds so the number of
/// open files and read buffers stays bounded.
const MAX_MERGE_FAN_IN: usize = 64;

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
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
    next_file: AtomicU64,
    spilled_bytes: AtomicU64,
}

impl Scratch {
    pub fn create(path: impl Into<PathBuf>) -> Result<Arc<Self>> {
        let path = path.into();
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create scratch directory {}", path.display()))?;
        Ok(Arc::new(Self {
            path,
            next_file: AtomicU64::new(0),
            spilled_bytes: AtomicU64::new(0),
        }))
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

pub struct ExternalSorter<T> {
    scratch: Arc<Scratch>,
    label: &'static str,
    budget_bytes: usize,
    buffer: Vec<T>,
    buffered_bytes: usize,
    runs: Vec<PathBuf>,
    stats: SortStats,
}

impl<T: Spill + Ord + Send> ExternalSorter<T> {
    pub fn new(scratch: &Arc<Scratch>, label: &'static str, budget_bytes: usize) -> Self {
        Self {
            scratch: Arc::clone(scratch),
            label,
            budget_bytes: budget_bytes.max(1),
            buffer: Vec::new(),
            buffered_bytes: 0,
            runs: Vec::new(),
            stats: SortStats::default(),
        }
    }

    pub fn push(&mut self, item: T) -> Result<()> {
        self.buffered_bytes += item.resident_bytes();
        self.buffer.push(item);
        self.stats.items += 1;
        if self.buffered_bytes >= self.budget_bytes {
            self.spill()?;
        }
        Ok(())
    }

    pub fn stats(&self) -> SortStats {
        self.stats
    }

    /// Sorted items, ascending, with ties in insertion order.
    pub fn finish(mut self) -> Result<Sorted<T>> {
        // Stable sort plus run-order tie-breaking keeps equal items in push order.
        self.buffer.par_sort();
        if self.runs.is_empty() {
            return Ok(Sorted::Memory(std::mem::take(&mut self.buffer).into_iter()));
        }
        while self.runs.len() + 1 > MAX_MERGE_FAN_IN {
            let batch = self.runs.drain(..MAX_MERGE_FAN_IN).collect::<Vec<_>>();
            let merged = self.scratch.new_file(self.label);
            let mut writer = SpoolWriter::<T>::create(&self.scratch, &merged)?;
            for item in Merge::<T>::open(&batch, Vec::new())? {
                writer.write(&item?)?;
            }
            writer.finish()?;
            self.runs.insert(0, merged);
        }
        let memory = std::mem::take(&mut self.buffer);
        Ok(Sorted::Merge(Merge::open(&self.runs, memory)?))
    }

    fn spill(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.buffer.par_sort();
        let path = self.scratch.new_file(self.label);
        let mut writer = SpoolWriter::create(&self.scratch, &path)?;
        for item in self.buffer.drain(..) {
            writer.write(&item)?;
        }
        writer.finish()?;
        self.buffered_bytes = 0;
        self.runs.push(path);
        self.stats.runs += 1;
        Ok(())
    }
}

pub enum Sorted<T> {
    Memory(std::vec::IntoIter<T>),
    Merge(Merge<T>),
}

impl<T: Spill + Ord> Iterator for Sorted<T> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Memory(items) => items.next().map(Ok),
            Self::Merge(merge) => merge.next(),
        }
    }
}

/// K-way merge over sorted run files plus an optional sorted in-memory tail.
pub struct Merge<T> {
    sources: Vec<Source<T>>,
    heap: BinaryHeap<Reverse<HeapEntry<T>>>,
    failed: bool,
}

enum Source<T> {
    File(SpoolReader<T>),
    Memory(std::vec::IntoIter<T>),
}

impl<T: Spill> Source<T> {
    fn next(&mut self) -> Option<Result<T>> {
        match self {
            Self::File(reader) => reader.next(),
            Self::Memory(items) => items.next().map(Ok),
        }
    }
}

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
    fn open(runs: &[PathBuf], memory: Vec<T>) -> Result<Self> {
        let mut sources = runs
            .iter()
            .map(|path| {
                let mut reader = SpoolReader::open(path)?;
                reader.remove_on_drop = true;
                Ok(Source::File(reader))
            })
            .collect::<Result<Vec<_>>>()?;
        sources.push(Source::Memory(memory.into_iter()));
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
        let mut reader = SpoolReader::open(&self.path)?;
        reader.remove_on_drop = true;
        Ok(reader)
    }
}

struct SpoolWriter<T> {
    file: BufWriter<File>,
    scratch: Arc<Scratch>,
    buffer: Vec<u8>,
    items: u64,
    _marker: PhantomData<T>,
}

impl<T: Spill> SpoolWriter<T> {
    fn create(scratch: &Arc<Scratch>, path: &Path) -> Result<Self> {
        let file = File::create(path)
            .with_context(|| format!("failed to create scratch file {}", path.display()))?;
        Ok(Self {
            file: BufWriter::with_capacity(IO_BUFFER_BYTES, file),
            scratch: Arc::clone(scratch),
            buffer: Vec::new(),
            items: 0,
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
        self.scratch
            .spilled_bytes
            .fetch_add((prefix.len() + self.buffer.len()) as u64, Ordering::Relaxed);
        self.items += 1;
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.file.flush().context("failed to flush scratch file")
    }
}

pub struct SpoolReader<T> {
    file: BufReader<File>,
    path: PathBuf,
    buffer: Vec<u8>,
    remove_on_drop: bool,
    _marker: PhantomData<T>,
}

impl<T: Spill> SpoolReader<T> {
    fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("failed to open scratch file {}", path.display()))?;
        Ok(Self {
            file: BufReader::with_capacity(IO_BUFFER_BYTES, file),
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
        self.buffer.resize(len, 0);
        self.file
            .read_exact(&mut self.buffer)
            .with_context(|| format!("truncated scratch file {}", self.path.display()))?;
        let mut input = self.buffer.as_slice();
        let item = T::decode(&mut input)?;
        if !input.is_empty() {
            bail!("scratch item in {} has trailing bytes", self.path.display());
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
                bail!("truncated length prefix in {}", self.path.display());
            }
            prefix[used] = byte[0];
            used += 1;
            if byte[0] & 0x80 == 0 {
                break;
            }
            if used == prefix.len() {
                bail!("invalid length prefix in {}", self.path.display());
            }
        }
        let len = get_u64(&mut &prefix[..used])?;
        Ok(Some(
            usize::try_from(len).context("scratch item too large")?,
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
                Ok(($(<$ty>::try_from(get_u64(input)?).context("scratch value out of range")?,)+))
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
        let mut sorter = ExternalSorter::new(&scratch, "pairs", usize::MAX);
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
        let mut sorter = ExternalSorter::new(&scratch, "pairs", 50 * 16);
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
    fn run_files_are_removed_once_merged() {
        let scratch = scratch("cleanup");
        let mut sorter = ExternalSorter::new(&scratch, "pairs", 10 * 16);
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
        let mut sorter = ExternalSorter::new(&scratch, "keyed", 7 * std::mem::size_of::<Keyed>());
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
