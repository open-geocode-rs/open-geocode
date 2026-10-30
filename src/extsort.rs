//! Disk-backed sorting and spooling for builds larger than memory.
//!
//! [`ExternalSorter`] buffers items until part of its fair share of the
//! build [`Budget`] is used (or the shared pool runs out), then hands the
//! buffer to a spill thread that sorts it in place, codes and compresses it
//! in parallel and writes it as a run, while the producer fills the next
//! buffer. Reading merges every run in one pass when memory allows (see
//! [`merge`]). When everything fits no run is written and the sort is an
//! ordinary in-memory sort, so a city extract and a planet build share one
//! code path.
//!
//! [`Spool`] is the unsorted sibling: an append-only scratch file replayed in
//! write order, once or, closed into a [`SpoolFile`], as often as needed.
//! [`IndexedSpool`] is read back by index, in any order, so it is not
//! compressed.
//!
//! Every byte is reserved in the [`Scratch`] ledger before it is written;
//! see [`scratch`] for what happens when it runs out.
//!
//! A sorter or a closed spool can be saved at a checkpoint and restored
//! from it ([`SorterState`], [`RunState`]): a build that stops goes on with
//! the runs already on disk.

mod block;
mod merge;
mod scratch;

use std::{
    fs::File,
    io::{BufWriter, Write},
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::JoinHandle,
    time::Instant,
};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

pub use crate::util::codec::Delta;
use crate::{
    error::{Error, Result},
    memory::{Budget, Reservation, Spillable},
    util::codec::{get_delta, get_u64, put_delta, put_u64},
};
pub use block::RunState;
use block::{
    BLOCK_BYTES, Block, Cursor, Failure, Frame, MIN_BLOCK_BYTES, Prefetch, Run, RunWriter, Stream,
};
pub use merge::Merge;
pub use scratch::{
    IoTimes, Ledger, MANIFEST_FILE, Owner, Scratch, ScratchFile, Shortfall, Traffic,
};

const IO_BUFFER_BYTES: usize = 1 << 20;
/// When the shared pool is full, a sorter spills only once its buffer holds at
/// least this fraction of its share; smaller runs would cost more than they free.
const MIN_RUN_FRACTION: usize = 16;
/// Smallest buffer growth step, in items.
const MIN_CAPACITY: usize = 1_024;
/// Items a merge writes between two checks of whether the build goes on.
const CHECK_EVERY_ITEMS: usize = 1 << 16;

/// A value that can be written to and read back from scratch files.
///
/// Sorted values must be totally ordered: values that compare equal must be
/// interchangeable, since runs are sorted in place, not stably. A type
/// whose key can tie carries a sequence number in its order.
pub trait Spill: Sized {
    fn encode(&self, out: &mut Vec<u8>);
    fn decode(input: &mut &[u8]) -> Result<Self>;
    /// Approximate bytes this value occupies in memory, counted against the
    /// sorter budget. Values that own heap data must include it.
    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
    }
    /// Encode after the value before it in the same scratch block, which
    /// left `delta`. A type whose sort key leads with an integer or shares
    /// prefixes with its neighbours codes the key against it
    /// ([`put_delta`], [`crate::util::codec::put_front`]).
    fn encode_after(&self, delta: &mut Delta, out: &mut Vec<u8>) {
        let _ = delta;
        self.encode(out);
    }
    /// Decode what [`Spill::encode_after`] wrote.
    fn decode_after(delta: &mut Delta, input: &mut &[u8]) -> Result<Self> {
        let _ = delta;
        Self::decode(input)
    }
}

/// Spill statistics for one sorter, surfaced in the build report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SortStats {
    pub items: u64,
    pub runs: u64,
}

impl SortStats {
    /// Adds another sorter's work to this total. A build with several inputs
    /// sorts once per input, and the report counts them together.
    pub fn merge(&mut self, other: Self) {
        self.items += other.items;
        self.runs += other.runs;
    }
}

/// How a sorter's share is split: the buffer it fills, the one being
/// written (as much again) and the blocks that buffer is coded into.
fn buffer_bytes(share: usize) -> usize {
    (share / 8 * 3).max(1)
}

/// Raw bytes of a run's blocks for a sorter with `share` bytes.
fn block_bytes(share: usize) -> usize {
    (share / 32).clamp(MIN_BLOCK_BYTES, BLOCK_BYTES)
}

/// A sorter as a checkpoint names it: every item it was given is in its
/// runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SorterState {
    pub label: String,
    pub items: u64,
    pub runs_written: u64,
    pub runs: Vec<RunState>,
}

impl SorterState {
    /// The files its runs are in.
    pub fn files<'a>(
        &'a self,
        scratch: &'a Scratch,
    ) -> impl Iterator<Item = std::path::PathBuf> + 'a {
        self.runs.iter().flat_map(|run| run.files(scratch))
    }

    /// Bytes its runs hold on disk.
    pub fn bytes(&self) -> u64 {
        self.runs.iter().map(|run| run.bytes).sum()
    }
}

#[derive(Debug)]
pub struct ExternalSorter<T> {
    scratch: Arc<Scratch>,
    budget: Arc<Budget>,
    owner: Arc<Owner>,
    /// The most bytes this sorter may hold before it spills, whatever its
    /// fair share of the pool.
    share_bytes: usize,
    /// Buffer capacity plus the heap bytes of buffered items.
    reservation: Spillable,
    buffer: Vec<T>,
    /// Runs written on the producer's own thread (see
    /// [`ExternalSorter::spill`]).
    runs: Vec<Run>,
    spiller: Option<Spiller<T>>,
    items: u64,
    runs_written: Arc<AtomicU64>,
}

impl<T: Spill + Ord + Send + Sync + 'static> ExternalSorter<T> {
    pub fn new(
        scratch: &Arc<Scratch>,
        label: &'static str,
        budget: &Arc<Budget>,
        share_bytes: usize,
    ) -> Self {
        Self {
            scratch: Arc::clone(scratch),
            budget: Arc::clone(budget),
            owner: scratch.owner(label),
            share_bytes: share_bytes.max(1),
            reservation: Spillable::new(budget),
            buffer: Vec::new(),
            runs: Vec::new(),
            spiller: None,
            items: 0,
            runs_written: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn push(&mut self, item: T) -> Result<()> {
        let heap = item
            .resident_bytes()
            .saturating_sub(std::mem::size_of::<T>());
        let share = buffer_bytes(self.reservation.share(self.share_bytes));
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
                    self.owner.label()
                ))
            })?;
        }
        self.buffer.push(item);
        self.items += 1;
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
        SortStats {
            items: self.items,
            runs: self.runs_written.load(Ordering::Relaxed),
        }
    }

    /// Write what the buffer holds as a run and wait for every run to be
    /// on disk, for a checkpoint to name them. The buffer's memory is free
    /// again after.
    pub fn checkpoint(&mut self) -> Result<SorterState> {
        self.spill()?;
        if let Some(spiller) = self.spiller.take() {
            self.runs.extend(spiller.join()?);
        }
        Ok(SorterState {
            label: self.owner.label().to_string(),
            items: self.items,
            runs_written: self.runs_written.load(Ordering::Relaxed),
            runs: self.runs.iter().map(Run::state).collect(),
        })
    }

    /// A sorter holding the runs `state` names, as [`ExternalSorter::new`]
    /// makes an empty one.
    pub fn restore(
        scratch: &Arc<Scratch>,
        budget: &Arc<Budget>,
        share_bytes: usize,
        state: &SorterState,
    ) -> Result<Self> {
        let mut sorter = Self {
            scratch: Arc::clone(scratch),
            budget: Arc::clone(budget),
            owner: scratch.owner(&state.label),
            share_bytes: share_bytes.max(1),
            reservation: Spillable::new(budget),
            buffer: Vec::new(),
            runs: Vec::new(),
            spiller: None,
            items: state.items,
            runs_written: Arc::new(AtomicU64::new(state.runs_written)),
        };
        for run in &state.runs {
            sorter.runs.push(Run::restore(scratch, run)?);
        }
        Ok(sorter)
    }

    /// Sorted items, ascending.
    pub fn finish(mut self) -> Result<Sorted<T>> {
        if self.runs.is_empty() && self.spiller.is_none() {
            self.buffer.par_sort_unstable();
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
        let mut runs = std::mem::take(&mut self.runs);
        if let Some(spiller) = self.spiller.take() {
            runs.extend(spiller.join()?);
        }
        let share = self.reservation.share(self.share_bytes);
        let fan_in = merge::fan_in(&runs, share);
        let runs = merge::premerge::<T>(&self.scratch, &self.owner, &self.budget, runs, fan_in)?;
        let passes = runs.iter().map(|run| run.depth).max().unwrap_or(0) + 1;
        self.owner.merged(runs.len(), passes);
        Ok(Sorted::Merge(Merge::open(
            &self.scratch,
            runs,
            &self.budget,
        )?))
    }

    /// Hand the buffer to the spill thread, waiting while it still writes
    /// the one before.
    fn spill(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let batch = Batch {
            items: std::mem::take(&mut self.buffer),
            reservation: self.reservation.reservation().take(),
        };
        let writer = RunSpill {
            scratch: Arc::clone(&self.scratch),
            owner: Arc::clone(&self.owner),
            budget: Arc::clone(&self.budget),
            share_bytes: self.reservation.share(self.share_bytes),
            runs_written: Arc::clone(&self.runs_written),
        };
        if rayon::current_thread_index().is_some() {
            // A pool worker waiting on the spill thread could leave that
            // thread no worker to sort on: write here instead.
            return writer.write(batch, &mut self.runs);
        }
        let spiller = self.spiller.get_or_insert_with(|| Spiller::start(writer));
        let started = Instant::now();
        let sent = spiller.sender.as_ref().map(|sender| sender.send(batch));
        self.scratch.add_spill_wait(started.elapsed());
        if !matches!(sent, Some(Ok(()))) {
            let spiller = self.spiller.take().expect("spiller");
            return Err(spiller.join().err().unwrap_or_else(|| {
                Error::Inconsistent(format!("the {} spill thread stopped", self.owner.label()))
            }));
        }
        Ok(())
    }
}

/// A full buffer and the memory it holds until it is written.
struct Batch<T> {
    items: Vec<T>,
    reservation: Reservation,
}

impl<T> std::fmt::Debug for Batch<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Batch")
            .field("items", &self.items.len())
            .field("bytes", &self.reservation.bytes())
            .finish()
    }
}

/// What a spill needs, on whichever thread it runs.
struct RunSpill {
    scratch: Arc<Scratch>,
    owner: Arc<Owner>,
    budget: Arc<Budget>,
    share_bytes: usize,
    runs_written: Arc<AtomicU64>,
}

impl RunSpill {
    /// Sort `batch` in place and write it as a run. When the ledger refuses
    /// it even at the highest level, the runs written at lower levels are
    /// merged at this one, freeing what the higher level saves chunk by
    /// chunk as they are read, and the run is written again.
    fn write<T: Spill + Ord + Send + Sync>(
        &self,
        mut batch: Batch<T>,
        runs: &mut Vec<Run>,
    ) -> Result<()> {
        batch.items.par_sort_unstable();
        let block = block_bytes(self.share_bytes);
        let wave = (self.share_bytes / 8 / (2 * block)).clamp(1, rayon::current_num_threads());
        let mut blocks = Reservation::empty(&self.budget);
        if !blocks.try_grow(wave * 2 * block) {
            blocks.force_grow(2 * block);
        }
        let wave = (blocks.bytes() / (2 * block)).max(1);
        loop {
            match write_run(&self.scratch, &self.owner, &batch.items, block, wave) {
                Ok(run) => {
                    runs.push(run);
                    self.runs_written.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Err(Failure::Full(shortfall)) => {
                    let level = self.scratch.level();
                    let (mut older, kept): (Vec<_>, Vec<_>) = std::mem::take(runs)
                        .into_iter()
                        .partition(|run| run.level < level);
                    *runs = kept;
                    if older.is_empty() {
                        return Err(shortfall.into());
                    }
                    let fan_in = merge::fan_in(&older, self.share_bytes);
                    while !older.is_empty() {
                        let group = older.split_off(older.len().saturating_sub(fan_in));
                        runs.push(merge::merge_into::<T>(
                            &self.scratch,
                            &self.owner,
                            &self.budget,
                            group,
                        )?);
                    }
                }
                Err(Failure::Error(error)) => return Err(error),
            }
        }
    }
}

/// Code a sorted buffer into blocks of about `block` raw bytes, `wave` at a
/// time compressed in parallel and written in order.
fn write_run<T: Spill + Sync>(
    scratch: &Arc<Scratch>,
    owner: &Arc<Owner>,
    items: &[T],
    block: usize,
    wave: usize,
) -> std::result::Result<Run, Failure> {
    let sample = &items[..items.len().min(64)];
    let mut raw = Vec::new();
    block::encode_block(sample, &mut raw);
    let mut per_block = (block * sample.len() / raw.len().max(1)).max(1);
    let mut writer = RunWriter::new(scratch, owner);
    let mut position = 0;
    while position < items.len() {
        let end = items.len().min(position + per_block * wave);
        let level = scratch.level();
        let frames = items[position..end]
            .par_chunks(per_block)
            .map(|chunk| {
                let mut raw = Vec::with_capacity(block + block / 4);
                block::encode_block(chunk, &mut raw);
                Frame::compress(raw, chunk.len(), level)
            })
            .collect::<Vec<_>>();
        let (mut raw_bytes, mut count) = (0, 0);
        for frame in frames {
            let frame = frame?;
            raw_bytes += frame.raw_len();
            count += frame.items() as usize;
            writer.write(frame)?;
        }
        per_block = (block * count / raw_bytes.max(1)).max(1);
        position = end;
    }
    Ok(writer.finish())
}

/// The thread that writes a sorter's full buffers while the producer fills
/// the next. The queue holds none: the producer waits while one is written,
/// so at most two buffers, both reserved, exist at once.
#[derive(Debug)]
struct Spiller<T> {
    sender: Option<SyncSender<Batch<T>>>,
    thread: Option<JoinHandle<Result<Vec<Run>>>>,
}

impl<T: Spill + Ord + Send + Sync + 'static> Spiller<T> {
    fn start(writer: RunSpill) -> Self {
        let (sender, batches) = mpsc::sync_channel::<Batch<T>>(0);
        let thread = std::thread::spawn(move || {
            let mut runs = Vec::new();
            for batch in batches {
                writer.write(batch, &mut runs)?;
            }
            Ok(runs)
        });
        Self {
            sender: Some(sender),
            thread: Some(thread),
        }
    }

    /// Wait for the last buffer to be written: every run, or why not.
    fn join(mut self) -> Result<Vec<Run>> {
        drop(self.sender.take());
        self.thread
            .take()
            .expect("joined once")
            .join()
            .map_err(|_| Error::Inconsistent("a spill thread panicked".into()))?
    }
}

impl<T> Drop for Spiller<T> {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
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

/// Append-only scratch file replayed in write order.
#[derive(Debug)]
pub struct Spool<T> {
    scratch: Arc<Scratch>,
    stream: Stream<T>,
}

impl<T: Spill> Spool<T> {
    pub fn create(scratch: &Arc<Scratch>, label: &str) -> Result<Self> {
        Ok(Self {
            scratch: Arc::clone(scratch),
            stream: Stream::new(scratch, &scratch.owner(label)),
        })
    }

    pub fn write(&mut self, item: &T) -> Result<()> {
        Ok(self.stream.push(item)?)
    }

    /// Items written so far.
    pub fn written(&self) -> u64 {
        self.stream.items
    }

    /// Close the spool and read it back from the start. Each chunk of the
    /// file is removed once read.
    pub fn into_reader(self) -> Result<SpoolReader<T>> {
        let run = self.stream.finish()?;
        Ok(SpoolReader::new(&self.scratch, Cursor::consume(run)))
    }

    /// Close the spool to read it back as many times as needed. The file is
    /// removed once the [`SpoolFile`] is dropped.
    pub fn finish(self) -> Result<SpoolFile<T>> {
        Ok(SpoolFile {
            run: self.stream.finish()?,
            scratch: self.scratch,
            _marker: PhantomData,
        })
    }
}

/// A closed spool, read from the start by each [`SpoolFile::reader`].
#[derive(Debug)]
pub struct SpoolFile<T> {
    run: Run,
    scratch: Arc<Scratch>,
    _marker: PhantomData<T>,
}

impl<T: Spill> SpoolFile<T> {
    pub fn reader(&self) -> Result<SpoolReader<T>> {
        Ok(SpoolReader::new(&self.scratch, Cursor::keep(&self.run)))
    }

    /// Read it once more, removing each chunk as it is read.
    pub fn into_reader(self) -> SpoolReader<T> {
        SpoolReader::new(&self.scratch, Cursor::consume(self.run))
    }

    /// The spool as a checkpoint names it.
    pub fn state(&self) -> RunState {
        self.run.state()
    }

    /// The spool a checkpoint named.
    pub fn restore(scratch: &Arc<Scratch>, state: &RunState) -> Result<Self> {
        Ok(Self {
            run: Run::restore(scratch, state)?,
            scratch: Arc::clone(scratch),
            _marker: PhantomData,
        })
    }

    /// Items the spool holds.
    pub fn len(&self) -> u64 {
        self.run.items
    }

    pub fn is_empty(&self) -> bool {
        self.run.items == 0
    }
}

/// A spool read in order, its blocks read and decompressed ahead.
#[derive(Debug)]
pub struct SpoolReader<T> {
    prefetch: Prefetch,
    block: Option<Block>,
    done: bool,
    _marker: PhantomData<T>,
}

impl<T: Spill> SpoolReader<T> {
    fn new(scratch: &Arc<Scratch>, cursor: Cursor) -> Self {
        Self {
            prefetch: Prefetch::start(scratch, vec![cursor], 1),
            block: None,
            done: false,
            _marker: PhantomData,
        }
    }
}

impl<T: Spill> Iterator for SpoolReader<T> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            if let Some(block) = &mut self.block {
                match block.next() {
                    Some(Ok(item)) => return Some(Ok(item)),
                    Some(Err(error)) => {
                        self.done = true;
                        return Some(Err(error));
                    }
                    None => self.block = None,
                }
            }
            match self.prefetch.next(0) {
                Ok(Some(block)) => self.block = Some(block),
                Ok(None) => self.done = true,
                Err(error) => {
                    self.done = true;
                    return Some(Err(error));
                }
            }
        }
        None
    }
}

/// A scratch file of items read back by their index, in any order and as
/// often as needed.
#[derive(Debug)]
pub struct IndexedSpool<T> {
    file: BufWriter<ScratchFile>,
    bytes: u64,
    offsets: Vec<u64>,
    buffer: Vec<u8>,
    _marker: PhantomData<T>,
}

impl<T: Spill> IndexedSpool<T> {
    pub fn create(scratch: &Arc<Scratch>, label: &str) -> Result<Self> {
        let path = scratch.new_file(label);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(Error::Decode("scratch file name is not UTF-8"))?;
        Ok(Self {
            file: BufWriter::with_capacity(
                IO_BUFFER_BYTES,
                ScratchFile::create(scratch, label, name)?,
            ),
            bytes: 0,
            offsets: Vec::new(),
            buffer: Vec::new(),
            _marker: PhantomData,
        })
    }

    pub fn write(&mut self, item: &T) -> Result<()> {
        self.offsets.push(self.bytes);
        self.buffer.clear();
        item.encode(&mut self.buffer);
        let mut prefix = Vec::with_capacity(10);
        put_u64(&mut prefix, self.buffer.len() as u64);
        self.file.write_all(&prefix)?;
        self.file.write_all(&self.buffer)?;
        self.bytes += (prefix.len() + self.buffer.len()) as u64;
        Ok(())
    }

    /// Bytes written so far.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Close the file and map it to read items by index. The file is
    /// removed once the [`IndexedSpoolFile`] is dropped.
    pub fn finish(self) -> Result<IndexedSpoolFile<T>> {
        let file = self
            .file
            .into_inner()
            .map_err(|error| Error::Io(error.into_error()))?;
        IndexedSpoolFile::map(file, self.offsets, None)
    }
}

/// A closed [`IndexedSpool`].
#[derive(Debug)]
pub struct IndexedSpoolFile<T> {
    /// Taken before the file is removed: Windows cannot remove a mapped file.
    map: Option<memmap2::Mmap>,
    file: ScratchFile,
    offsets: Vec<u64>,
    /// The offsets, once a checkpoint wrote them out.
    saved: Option<ScratchFile>,
    _marker: PhantomData<T>,
}

/// An [`IndexedSpoolFile`] as a checkpoint names it: its file and the file
/// of its items' offsets, by name and length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedState {
    pub label: String,
    pub file: (String, u64),
    pub offsets: (String, u64),
}

impl<T> IndexedSpoolFile<T> {
    fn map(file: ScratchFile, offsets: Vec<u64>, saved: Option<ScratchFile>) -> Result<Self> {
        let path = file.path().to_path_buf();
        let read = File::open(&path).map_err(|source| Error::IoPath {
            action: "failed to open scratch file",
            path: path.clone(),
            source,
        })?;
        // SAFETY: the scratch file is this build's own and is not written
        // again while mapped.
        let map = unsafe { memmap2::Mmap::map(&read) }.map_err(|source| Error::IoPath {
            action: "failed to map scratch file",
            path,
            source,
        })?;
        Ok(Self {
            map: Some(map),
            file,
            offsets,
            saved,
            _marker: PhantomData,
        })
    }

    /// The file as a checkpoint names it, its offsets written out once.
    pub fn state(&mut self, scratch: &Arc<Scratch>) -> Result<IndexedState> {
        if self.saved.is_none() {
            let name = scratch.new_file(&format!("{}-offsets", self.file.label()));
            let name = name
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut offsets =
                BufWriter::new(ScratchFile::create(scratch, self.file.label(), &name)?);
            for offset in &self.offsets {
                offsets.write_all(&offset.to_le_bytes())?;
            }
            self.saved = Some(offsets.into_inner().map_err(|error| error.into_error())?);
        }
        let saved = self.saved.as_ref().expect("offsets saved");
        Ok(IndexedState {
            label: self.file.label().to_string(),
            file: (self.file.name(), self.file.bytes()),
            offsets: (saved.name(), saved.bytes()),
        })
    }

    /// The files a checkpoint names for it.
    pub fn files(&self) -> Vec<std::path::PathBuf> {
        std::iter::once(&self.file)
            .chain(self.saved.as_ref())
            .map(|file| file.path().to_path_buf())
            .collect()
    }

    /// The file a checkpoint named.
    pub fn restore(scratch: &Arc<Scratch>, state: &IndexedState) -> Result<Self> {
        let file = ScratchFile::reopen(scratch, &state.label, &state.file.0, state.file.1)?;
        let saved = ScratchFile::reopen(scratch, &state.label, &state.offsets.0, state.offsets.1)?;
        let offsets = std::fs::read(saved.path())?
            .as_chunks::<8>()
            .0
            .iter()
            .map(|offset| u64::from_le_bytes(*offset))
            .collect();
        Self::map(file, offsets, Some(saved))
    }
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
        let truncated = || Error::BadScratch {
            path: self.file.path().to_path_buf(),
            detail: "truncated",
        };
        let mut input = usize::try_from(self.offsets[index])
            .ok()
            .and_then(|offset| bytes.get(offset..))
            .ok_or_else(truncated)?;
        let len = usize::try_from(get_u64(&mut input)?)
            .map_err(|_| Error::Decode("scratch item too large"))?;
        let mut item = input.get(..len).ok_or_else(truncated)?;
        let value = T::decode(&mut item)?;
        if !item.is_empty() {
            return Err(Error::BadScratch {
                path: self.file.path().to_path_buf(),
                detail: "item has trailing bytes",
            });
        }
        Ok(value)
    }
}

impl<T> Drop for IndexedSpoolFile<T> {
    fn drop(&mut self) {
        drop(self.map.take());
    }
}

/// Unsigned tuples, the first value coded against the one before: it leads
/// the sort.
macro_rules! spill_tuple_of_unsigned {
    ($first:ident: $first_ty:ty, $($name:ident: $ty:ty),+) => {
        impl Spill for ($first_ty, $($ty,)+) {
            fn encode(&self, out: &mut Vec<u8>) {
                let ($first, $($name,)+) = *self;
                put_u64(out, u64::from($first));
                $(put_u64(out, u64::from($name));)+
            }

            fn decode(input: &mut &[u8]) -> Result<Self> {
                Ok((
                    <$first_ty>::try_from(get_u64(input)?)
                        .map_err(|_| Error::Decode("scratch value out of range"))?,
                    $(<$ty>::try_from(get_u64(input)?)
                        .map_err(|_| Error::Decode("scratch value out of range"))?,)+
                ))
            }

            fn encode_after(&self, delta: &mut Delta, out: &mut Vec<u8>) {
                let ($first, $($name,)+) = *self;
                put_delta(out, delta, u64::from($first) as i64);
                $(put_u64(out, u64::from($name));)+
            }

            fn decode_after(delta: &mut Delta, input: &mut &[u8]) -> Result<Self> {
                Ok((
                    <$first_ty>::try_from(get_delta(input, delta)? as u64)
                        .map_err(|_| Error::Decode("scratch value out of range"))?,
                    $(<$ty>::try_from(get_u64(input)?)
                        .map_err(|_| Error::Decode("scratch value out of range"))?,)+
                ))
            }
        }
    };
}

spill_tuple_of_unsigned!(a: u64, b: u64);
spill_tuple_of_unsigned!(a: u64, b: u8, c: u64);

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::util::codec::{get_string, put_str};

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

    fn sorted<T: Spill + Ord>(sorted: Sorted<T>) -> Vec<T> {
        sorted.collect::<Result<Vec<_>>>().expect("items")
    }

    fn files(scratch: &Scratch) -> usize {
        fs::read_dir(scratch.path()).expect("scratch").count()
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
        let sorted = sorted(sorter.finish().expect("finish"));
        let mut expected = input;
        expected.sort();
        assert_eq!(sorted, expected);
        assert_eq!(scratch.spilled_bytes(), 0);
    }

    #[test]
    fn a_tiny_share_spills_many_runs_and_merges_them_twice() {
        let scratch = scratch("spill");
        let item = std::mem::size_of::<(u64, u64)>();
        // Buffers of a few hundred items: well over a hundred runs, and a
        // share that reads only a few at once.
        let mut sorter = ExternalSorter::new(&scratch, "pairs", &Budget::unlimited(), 400 * item);
        let input = pseudo_random(50_000);
        for item in &input {
            sorter.push(*item).expect("push");
        }
        let sorted = {
            let finished = sorter.finish().expect("finish");
            let runs = scratch.owner("pairs").traffic().runs;
            assert!(runs > 100, "{runs} runs");
            sorted(finished)
        };
        let mut expected = input;
        expected.sort();
        assert_eq!(sorted, expected);
        let traffic = scratch.owner("pairs").traffic();
        assert!(traffic.merge_passes >= 2, "{traffic:?}");
        assert_eq!(traffic.live_bytes, 0);
        assert_eq!(files(&scratch), 0);
    }

    #[test]
    fn many_runs_merge_in_one_pass_when_their_buffers_fit() {
        let scratch = scratch("one-pass");
        let budget = Budget::unlimited();
        let owner = scratch.owner("pairs");
        let input = pseudo_random(50_000);
        let runs = input
            .chunks(250)
            .map(|chunk| {
                let mut chunk = chunk.to_vec();
                chunk.sort();
                write_run(&scratch, &owner, &chunk, MIN_BLOCK_BYTES, 1).expect("run")
            })
            .collect::<Vec<_>>();
        let fan_in = merge::fan_in(&runs, 64 << 20);
        assert!(fan_in > runs.len());
        let runs = merge::premerge::<(u64, u64)>(&scratch, &owner, &budget, runs, fan_in)
            .expect("premerge");
        assert_eq!(runs.len(), 200);
        let merged = Merge::<(u64, u64)>::open(&scratch, runs, &budget)
            .expect("merge")
            .collect::<Result<Vec<_>>>()
            .expect("items");
        let mut expected = input;
        expected.sort();
        assert_eq!(merged, expected);
        let traffic = owner.traffic();
        assert_eq!(traffic.merges, 0);
        assert_eq!(traffic.items_read, 50_000);
        assert_eq!(traffic.live_bytes, 0);
        assert_eq!(files(&scratch), 0);
    }

    #[test]
    fn a_capped_fan_in_merges_the_smallest_runs_first() {
        let scratch = scratch("capped");
        let budget = Budget::unlimited();
        let owner = scratch.owner("pairs");
        let input = pseudo_random(30_000);
        let runs = input
            .chunks(1_000)
            .map(|chunk| {
                let mut chunk = chunk.to_vec();
                chunk.sort();
                write_run(&scratch, &owner, &chunk, MIN_BLOCK_BYTES, 1).expect("run")
            })
            .collect::<Vec<_>>();
        // Three runs at once: 30 runs merge 2 first, then 3 at a time.
        let share = 3 * runs
            .iter()
            .map(Run::read_bytes)
            .max()
            .expect("runs")
            .max(64 << 10);
        let fan_in = merge::fan_in(&runs, share);
        assert_eq!(fan_in, 3);
        let runs = merge::premerge::<(u64, u64)>(&scratch, &owner, &budget, runs, fan_in)
            .expect("premerge");
        assert_eq!(runs.len(), 3);
        let traffic = owner.traffic();
        assert_eq!(traffic.merges, 14);
        assert_eq!(traffic.max_fan_in, 3);
        let merged = Merge::<(u64, u64)>::open(&scratch, runs, &budget)
            .expect("merge")
            .collect::<Result<Vec<_>>>()
            .expect("items");
        let mut expected = input;
        expected.sort();
        assert_eq!(merged, expected);
        assert_eq!(owner.traffic().live_bytes, 0);
        assert_eq!(files(&scratch), 0);
    }

    #[test]
    fn a_run_is_removed_as_soon_as_it_is_exhausted() {
        let scratch = scratch("exhausted");
        let budget = Budget::unlimited();
        let owner = scratch.owner("pairs");
        let low = (0..100u64).map(|key| (key, 0)).collect::<Vec<_>>();
        // Many blocks, more than are read ahead.
        let high = (1_000..60_000u64).map(|key| (key, 0)).collect::<Vec<_>>();
        let runs = [&low, &high, &high]
            .into_iter()
            .map(|items| write_run(&scratch, &owner, items, MIN_BLOCK_BYTES, 1).expect("run"))
            .collect::<Vec<_>>();
        assert_eq!(files(&scratch), 3);
        let mut merge = Merge::<(u64, u64)>::open(&scratch, runs, &budget).expect("merge");
        for _ in 0..100 {
            merge.next().expect("item").expect("item");
        }
        // The low run's last item came out: its file went with it.
        assert_eq!(files(&scratch), 2);
        assert_eq!(merge.count(), 2 * 59_000);
        assert_eq!(files(&scratch), 0);
        assert_eq!(scratch.live_bytes(), 0);
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
        let mut merged = sorted(first);
        merged.extend(sorted(second.finish().expect("finish")));
        merged.sort();
        let mut expected = input;
        expected.sort();
        assert_eq!(merged, expected);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn a_finished_spilled_sorter_holds_only_read_buffers() {
        let scratch = scratch("finished");
        let budget = Budget::unlimited();
        let mut sorter = ExternalSorter::new(&scratch, "pairs", &budget, 10_000);
        for item in pseudo_random(20_000) {
            sorter.push(item).expect("push");
        }
        let finished = sorter.finish().expect("finish");
        match &finished {
            Sorted::Merge(merge) => assert_eq!(budget.used(), merge.buffer_bytes()),
            Sorted::Memory(..) => panic!("spilled"),
        }
        assert_eq!(finished.count(), 20_000);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn runs_are_compressed_and_their_keys_delta_coded() {
        let scratch = scratch("compressed");
        let owner = scratch.owner("pairs");
        let items = (0..100_000u64)
            .map(|index| (1_000_000_000 + index * 3, index % 7))
            .collect::<Vec<_>>();
        let run = write_run(&scratch, &owner, &items, BLOCK_BYTES, 4).expect("run");
        let traffic = owner.traffic();
        // Plain varints take 5 + 1 bytes an item; deltas 1 + 1, but for the
        // first of each block.
        assert!(
            traffic.raw_written_bytes <= 2 * 100_000 + 4 * 8,
            "{traffic:?}"
        );
        assert!(
            traffic.written_bytes * 4 < traffic.raw_written_bytes,
            "{traffic:?}"
        );
        assert_eq!(run.items, 100_000);
        assert_eq!(scratch.spilled_bytes(), traffic.written_bytes);
        let back = Merge::<(u64, u64)>::open(&scratch, vec![run], &Budget::unlimited())
            .expect("merge")
            .collect::<Result<Vec<_>>>()
            .expect("items");
        assert_eq!(back, items);
        assert_eq!(owner.traffic().raw_read_bytes, traffic.raw_written_bytes);
    }

    #[test]
    fn spool_replays_in_write_order_and_cleans_up() {
        let scratch = scratch("spool");
        let mut spool = Spool::create(&scratch, "items").expect("spool");
        let items = pseudo_random(200_000);
        for item in &items {
            spool.write(item).expect("write");
        }
        assert_eq!(spool.written(), 200_000);
        let back = spool
            .into_reader()
            .expect("reader")
            .collect::<Result<Vec<_>>>()
            .expect("items");
        assert_eq!(back, items);
        assert_eq!(files(&scratch), 0);
        assert_eq!(scratch.live_bytes(), 0);
    }

    #[test]
    fn a_spool_file_reads_back_as_often_as_needed() {
        let scratch = scratch("spool-file");
        let mut spool = Spool::create(&scratch, "items").expect("spool");
        for item in [(3u64, 1u64), (1, 2), (2, 3)] {
            spool.write(&item).expect("write");
        }
        let file = spool.finish().expect("finish");
        for _ in 0..2 {
            let items = file
                .reader()
                .expect("reader")
                .collect::<Result<Vec<_>>>()
                .expect("items");
            assert_eq!(items, vec![(3, 1), (1, 2), (2, 3)]);
        }
        assert_eq!(file.len(), 3);
        drop(file);
        assert_eq!(files(&scratch), 0);
    }

    #[test]
    fn an_indexed_spool_reads_any_item_and_counts_its_disk() {
        let scratch = scratch("indexed");
        let mut spool = IndexedSpool::create(&scratch, "indexed").expect("spool");
        for item in pseudo_random(1_000) {
            spool.write(&item).expect("write");
        }
        let file = spool.finish().expect("finish");
        assert_eq!(file.get(999).expect("item"), pseudo_random(1_000)[999]);
        assert_eq!(scratch.live_bytes(), file.bytes());
        drop(file);
        assert_eq!(scratch.live_bytes(), 0);
        assert_eq!(files(&scratch), 0);
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

    #[test]
    fn the_ledger_refuses_past_its_budget_and_says_what_is_live() {
        let scratch = scratch("ledger");
        scratch.set_budget(Some(10 << 20));
        scratch.set_stage("write records");
        let records = scratch.owner("records");
        let text = scratch.owner("text-index");
        scratch.reserve(&records, 6 << 20).expect("fits");
        scratch.reserve(&text, 3 << 20).expect("fits");
        let refused = scratch
            .reserve(&records, 2 << 20)
            .expect_err("over budget")
            .message;
        for part in [
            "stage write records",
            "records needs 2 MiB",
            "9 MiB is live",
            "records 6 MiB",
            "text-index 3 MiB",
            "scratch budget of 10 MiB",
            "it would fit in 11 MiB",
        ] {
            assert!(refused.contains(part), "{part:?} not in {refused}");
        }
        scratch.release(&records, 6 << 20);
        assert_eq!(scratch.peak_bytes(), 9 << 20);
        assert_eq!(scratch.peak_owners().get("records"), Some(&(6 << 20)));
        scratch.reserve(&records, 2 << 20).expect("fits once freed");
        assert_eq!(records.traffic().peak_bytes, 6 << 20);
    }

    #[test]
    fn levels_rise_step_by_step_to_the_highest() {
        let scratch = scratch("levels");
        assert_eq!(scratch.level(), 1);
        let mut seen = vec![scratch.level()];
        while scratch.raise_level(scratch.level()) {
            seen.push(scratch.level());
        }
        assert_eq!(seen, [1, 3, 6, 9]);
        // A writer that saw an older level does not lower it.
        assert!(scratch.raise_level(1));
        assert_eq!(scratch.level(), 9);
    }

    #[test]
    fn a_watched_directory_counts_and_can_fail_the_next_reservation() {
        let scratch = scratch("watched");
        let directory = scratch.path().join("text");
        fs::create_dir_all(directory.join("segment")).expect("dir");
        fs::write(directory.join("meta.json"), vec![0; 1_000]).expect("file");
        fs::write(directory.join("segment").join("a.idx"), vec![0; 3_000]).expect("file");
        scratch.watch("text-index", &directory);
        assert_eq!(scratch.live_bytes(), 4_000);
        scratch.set_budget(Some(4_500));
        assert!(scratch.reserve(&scratch.owner("records"), 1_000).is_err());
        fs::remove_dir_all(&directory).expect("remove");
        scratch.unwatch(&directory);
        assert_eq!(scratch.live_bytes(), 0);
        scratch
            .reserve(&scratch.owner("records"), 1_000)
            .expect("fits");
    }

    /// Text compressing better at higher zstd levels than at level 1.
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
    struct Line {
        key: u64,
        text: String,
    }

    impl Spill for Line {
        fn encode(&self, out: &mut Vec<u8>) {
            put_u64(out, self.key);
            put_str(out, &self.text);
        }

        fn decode(input: &mut &[u8]) -> Result<Self> {
            Ok(Self {
                key: get_u64(input)?,
                text: get_string(input)?,
            })
        }

        fn resident_bytes(&self) -> usize {
            std::mem::size_of::<Self>() + self.text.capacity()
        }
    }

    fn lines(first: u64, count: u64) -> Vec<Line> {
        const WORDS: [&str; 12] = [
            "king", "street", "west", "avenue", "north", "road", "toronto", "ontario", "lane",
            "drive", "court", "place",
        ];
        let mut state = first.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        (first..first + count)
            .map(|key| {
                let text = (0..12)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        WORDS[(state % WORDS.len() as u64) as usize]
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                Line { key, text }
            })
            .collect()
    }

    fn spill(scratch: &Arc<Scratch>, label: &str) -> RunSpill {
        RunSpill {
            scratch: Arc::clone(scratch),
            owner: scratch.owner(label),
            budget: Budget::unlimited(),
            share_bytes: 64 << 20,
            runs_written: Arc::new(AtomicU64::new(0)),
        }
    }

    fn batch(items: Vec<Line>) -> Batch<Line> {
        Batch {
            items,
            reservation: Reservation::empty(&Budget::unlimited()),
        }
    }

    /// Bytes `items` take as one run at `level`.
    fn run_bytes(items: &[Line], level: i32) -> u64 {
        let scratch = scratch("measure");
        while scratch.level() < level {
            scratch.raise_level(scratch.level());
        }
        let owner = scratch.owner("measure");
        let block = block_bytes(64 << 20);
        let wave = ((64 << 20) / 8 / (2 * block)).clamp(1, rayon::current_num_threads());
        write_run(&scratch, &owner, items, block, wave)
            .expect("run")
            .bytes
    }

    #[test]
    fn a_full_ledger_compresses_harder_then_merges_older_runs_at_that_level() {
        let older = [lines(0, 4_000), lines(10_000, 4_000)];
        let newer = lines(20_000, 30_000);
        let older_fast = older.iter().map(|run| run_bytes(run, 1)).sum::<u64>();
        let older_small = run_bytes(&older.concat(), 9);
        let newer_small = run_bytes(&newer, 9);
        let saved = older_fast - older_small;
        // Room for the merged older runs while they are merged, but for the
        // newer run only once the merge has saved its bytes.
        let room = newer_small - saved / 2;
        assert!(saved > 0 && room >= older_small + older_small / 10 && room < newer_small);

        let scratch = scratch("degrade");
        let writer = spill(&scratch, "lines");
        let mut runs = Vec::new();
        for run in &older {
            writer
                .write(batch(run.clone()), &mut runs)
                .expect("older run");
        }
        assert!(runs.iter().all(|run| run.level == 1));
        scratch.set_budget(Some(scratch.live_bytes() + room));
        writer
            .write(batch(newer.clone()), &mut runs)
            .expect("fits after the merge");
        assert_eq!(scratch.level(), 9);
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().all(|run| run.level == 9));
        let traffic = scratch.owner("lines").traffic();
        assert_eq!(traffic.merges, 1);
        let mut expected = older.concat();
        expected.extend(newer);
        let merged = Merge::<Line>::open(&scratch, runs, &Budget::unlimited())
            .expect("merge")
            .collect::<Result<Vec<_>>>()
            .expect("items");
        assert_eq!(merged, expected);
    }

    #[test]
    fn a_full_ledger_fails_early_with_what_would_fit() {
        let scratch = scratch("full");
        scratch.set_stage("scan OSM objects");
        scratch.set_budget(Some(16 << 10));
        let mut sorter = ExternalSorter::new(&scratch, "lines", &Budget::unlimited(), 1 << 20);
        let failed = lines(0, 20_000)
            .into_iter()
            .try_for_each(|line| sorter.push(line))
            .and_then(|()| sorter.finish().map(drop));
        let Err(Error::OutOfResources(message)) = failed else {
            panic!("expected the ledger to stop the sort, got {failed:?}");
        };
        assert!(message.contains("stage scan OSM objects"), "{message}");
        assert!(message.contains("lines needs"), "{message}");
        assert!(message.contains("--scratch-budget-mb"), "{message}");
        assert_eq!(scratch.level(), 9);
        assert_eq!(scratch.live_bytes(), 0);
    }

    /// A sorter checkpointed and committed outlives a build that fails,
    /// and a scratch reopened from the checkpoint sorts what it held.
    #[test]
    fn a_checkpointed_sorter_survives_a_failure_and_is_restored() {
        let scratch = scratch("checkpoint");
        let path = scratch.path().to_path_buf();
        let budget = Budget::unlimited();
        let input = pseudo_random(5_000);
        let mut sorter = ExternalSorter::new(&scratch, "pairs", &budget, 1 << 20);
        for item in &input[..3_000] {
            sorter.push(*item).expect("push");
        }
        let state = sorter.checkpoint().expect("checkpoint");
        let files = state.files(&scratch).collect::<Vec<_>>();
        assert!(!files.is_empty());
        scratch.commit(&files, b"{}").expect("commit");
        // Work after the checkpoint is not the checkpoint's.
        for item in &input[3_000..] {
            sorter.push(*item).expect("push");
        }
        sorter.checkpoint().expect("later runs");
        drop(sorter);
        assert!(scratch.resumable());
        assert!(files.iter().all(|file| file.exists()));
        drop(scratch);
        assert!(path.join(MANIFEST_FILE).exists(), "kept for a resume");

        let scratch = Scratch::reopen(&path, &files, 1_000).expect("reopen");
        assert_eq!(
            fs::read_dir(&path).expect("scratch").count(),
            files.len() + 1,
            "only what the checkpoint names is left, and its manifest"
        );
        let restored = ExternalSorter::<(u64, u64)>::restore(&scratch, &budget, 1 << 20, &state)
            .expect("restore");
        assert_eq!(restored.stats().items, 3_000);
        let mut expected = input[..3_000].to_vec();
        expected.sort();
        assert_eq!(sorted(restored.finish().expect("finish")), expected);
        assert!(
            scratch.retained_bytes() > 0,
            "read runs wait for a checkpoint"
        );
        scratch.commit(&[], b"{}").expect("commit");
        assert_eq!(scratch.retained_bytes(), 0);
        assert!(files.iter().all(|file| !file.exists()));
        scratch.complete();
        drop(scratch);
        assert!(!path.exists(), "a completed build leaves nothing");
    }

    /// Files a checkpoint keeps after the build is done with them are given
    /// up, with the checkpoint, before the ledger refuses a write.
    #[test]
    fn retired_files_give_way_when_the_ledger_runs_short() {
        let scratch = scratch("reclaim");
        scratch.set_stage("write records");
        let budget = Budget::unlimited();
        let mut sorter = ExternalSorter::new(&scratch, "pairs", &budget, 1 << 20);
        for item in pseudo_random(20_000) {
            sorter.push(item).expect("push");
        }
        let state = sorter.checkpoint().expect("checkpoint");
        let files = state.files(&scratch).collect::<Vec<_>>();
        scratch.commit(&files, b"{}").expect("commit");
        assert_eq!(sorted(sorter.finish().expect("finish")).len(), 20_000);
        let retained = scratch.retained_bytes();
        assert_eq!(retained, state.bytes());
        assert_eq!(scratch.peak_retained_bytes(), retained);
        // Room for the write only once the retired runs are gone.
        scratch.set_budget(Some(retained + scratch.live_bytes()));
        let owner = scratch.owner("pack-records");
        scratch.reserve(&owner, retained).expect("room made");
        assert_eq!(scratch.retained_bytes(), 0);
        assert!(files.iter().all(|file| !file.exists()));
        assert!(!scratch.resumable());
        assert!(!scratch.path().join(MANIFEST_FILE).exists());
        assert_eq!(scratch.voided().as_deref(), Some("write records"));
        scratch.release(&owner, retained);
    }
}
