//! The on-disk form of runs and spools: compressed blocks in chunk files.
//!
//! Items are coded into blocks of about [`BLOCK_BYTES`], each against the
//! item before it in the block ([`Spill::encode_after`]), so a block decodes
//! on its own. Each block is one zstd frame behind a header:
//!
//! ```text
//! frame  = compressed_len u32 LE | raw_len u32 LE | items u32 LE | zstd frame
//! ```
//!
//! Blocks are compressed in parallel and written in order to chunk files of
//! about [`CHUNK_BYTES`]. A run read once deletes each chunk as soon as it
//! is read: exFAT cannot punch holes, so this is how reading frees disk
//! while a merge is still going. Readers decompress ahead of the consumer
//! on their own threads.

use std::{
    collections::VecDeque,
    fs::File,
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread::JoinHandle,
    time::Instant,
};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use super::{
    Spill,
    scratch::{Owner, Scratch, Shortfall},
};
use crate::{
    error::{Error, Result},
    util::codec::Delta,
};

/// Raw bytes a block holds before it is compressed: large enough for zstd
/// to find its matches, small enough that thousands of runs can each hold
/// two while they are merged. A sorter with a small share writes smaller
/// ones.
pub(super) const BLOCK_BYTES: usize = 256 << 10;
pub(super) const MIN_BLOCK_BYTES: usize = 16 << 10;
/// Bytes on disk a chunk file holds before the next one starts. A merge
/// frees its inputs a chunk at a time, so it holds up to one partly read
/// chunk per run on top of what it writes; small chunks keep that low, and
/// large ones keep the directory short for exFAT, which searches it in
/// order on every create.
const CHUNK_BYTES: u64 = 8 << 20;
const HEADER_BYTES: usize = 12;
/// Raw blocks a stream gathers before compressing them side by side.
const STREAM_BATCH_BLOCKS: usize = 4;

/// Why a write stopped: the ledger refused it, or something failed.
#[derive(Debug)]
pub(super) enum Failure {
    Full(Shortfall),
    Error(Error),
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Error(error)
    }
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self::Error(error.into())
    }
}

impl From<Failure> for Error {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Full(shortfall) => shortfall.into(),
            Failure::Error(error) => error,
        }
    }
}

/// Code `items` into one block, each against the one before.
pub(super) fn encode_block<T: Spill>(items: &[T], raw: &mut Vec<u8>) {
    let mut delta = Delta::default();
    for item in items {
        item.encode_after(&mut delta, raw);
    }
}

/// A block, compressed and ready to write. Keeps its raw bytes so it can be
/// compressed again at a higher level when the ledger refuses it.
#[derive(Debug)]
pub(super) struct Frame {
    raw: Vec<u8>,
    items: u32,
    level: i32,
    bytes: Vec<u8>,
}

impl Frame {
    pub(super) fn compress(raw: Vec<u8>, items: usize, level: i32) -> Result<Self> {
        let items = u32::try_from(items)
            .map_err(|_| Error::Inconsistent("scratch block of over 4G items".into()))?;
        let mut frame = Self {
            raw,
            items,
            level,
            bytes: Vec::new(),
        };
        frame.recompress(level)?;
        Ok(frame)
    }

    fn recompress(&mut self, level: i32) -> Result<()> {
        let compressed = zstd::bulk::compress(&self.raw, level)?;
        let len = |value: usize| {
            u32::try_from(value)
                .map_err(|_| Error::Inconsistent("scratch block of over 4 GiB".into()))
        };
        self.bytes.clear();
        self.bytes.reserve(HEADER_BYTES + compressed.len());
        self.bytes
            .extend_from_slice(&len(compressed.len())?.to_le_bytes());
        self.bytes
            .extend_from_slice(&len(self.raw.len())?.to_le_bytes());
        self.bytes.extend_from_slice(&self.items.to_le_bytes());
        self.bytes.extend_from_slice(&compressed);
        self.level = level;
        Ok(())
    }

    pub(super) fn raw_len(&self) -> usize {
        self.raw.len()
    }

    pub(super) fn items(&self) -> u32 {
        self.items
    }
}

#[derive(Debug)]
struct Chunk {
    path: PathBuf,
    bytes: u64,
}

/// A sorted run or a spool on disk. Its chunk files are removed, and their
/// bytes returned to the ledger, when it is dropped or as it is read.
#[derive(Debug)]
pub(super) struct Run {
    scratch: Arc<Scratch>,
    owner: Arc<Owner>,
    chunks: VecDeque<Chunk>,
    pub(super) items: u64,
    pub(super) bytes: u64,
    /// Largest block, raw and as on disk: what reading it holds at once.
    max_raw: usize,
    max_frame: usize,
    /// The lowest zstd level of its blocks.
    pub(super) level: i32,
    /// How often its items have been merged.
    pub(super) depth: u64,
}

/// A run or a spool as a checkpoint names it: its chunk files, by name in
/// the scratch directory, and what reading it back needs to know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    pub label: String,
    pub chunks: Vec<(String, u64)>,
    pub items: u64,
    pub bytes: u64,
    pub max_raw: usize,
    pub max_frame: usize,
    pub level: i32,
    pub depth: u64,
}

impl RunState {
    /// The chunk files, as paths in `scratch`.
    pub fn files<'a>(&'a self, scratch: &'a Scratch) -> impl Iterator<Item = PathBuf> + 'a {
        self.chunks
            .iter()
            .map(|(name, _)| scratch.path().join(name))
    }
}

impl Run {
    /// Memory a reader of this run holds: the block being read, the one
    /// read ahead and the compressed bytes behind it.
    pub(super) fn read_bytes(&self) -> usize {
        2 * self.max_raw + self.max_frame
    }

    pub(super) fn state(&self) -> RunState {
        RunState {
            label: self.owner.label().to_string(),
            chunks: self
                .chunks
                .iter()
                .map(|chunk| {
                    let name = chunk
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    (name, chunk.bytes)
                })
                .collect(),
            items: self.items,
            bytes: self.bytes,
            max_raw: self.max_raw,
            max_frame: self.max_frame,
            level: self.level,
            depth: self.depth,
        }
    }

    /// The run a checkpoint named, its chunks counted on the ledger again.
    pub(super) fn restore(scratch: &Arc<Scratch>, state: &RunState) -> Result<Self> {
        let owner = scratch.owner(&state.label);
        let mut chunks = VecDeque::new();
        for (name, bytes) in &state.chunks {
            let path = scratch.path().join(name);
            let found = std::fs::metadata(&path).map(|metadata| metadata.len());
            if found.as_ref().ok() != Some(bytes) {
                return Err(Error::BadScratch {
                    path,
                    detail: "a checkpoint names a chunk that is missing or changed",
                });
            }
            scratch.adopt(&owner, *bytes);
            chunks.push_back(Chunk {
                path,
                bytes: *bytes,
            });
        }
        Ok(Self {
            scratch: Arc::clone(scratch),
            owner,
            chunks,
            items: state.items,
            bytes: state.bytes,
            max_raw: state.max_raw,
            max_frame: state.max_frame,
            level: state.level,
            depth: state.depth,
        })
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        for chunk in self.chunks.drain(..) {
            self.scratch.remove(&self.owner, &chunk.path, chunk.bytes);
        }
    }
}

/// Writes frames, in order, into a new run.
#[derive(Debug)]
pub(super) struct RunWriter {
    label: String,
    run: Run,
    file: Option<File>,
}

impl RunWriter {
    pub(super) fn new(scratch: &Arc<Scratch>, owner: &Arc<Owner>) -> Self {
        Self {
            label: owner.label().to_string(),
            run: Run {
                scratch: Arc::clone(scratch),
                owner: Arc::clone(owner),
                chunks: VecDeque::new(),
                items: 0,
                bytes: 0,
                max_raw: 0,
                max_frame: 0,
                level: i32::MAX,
                depth: 0,
            },
            file: None,
        }
    }

    pub(super) fn set_depth(&mut self, depth: u64) {
        self.run.depth = depth;
    }

    /// Write `frame`, compressing it harder while the ledger refuses it and
    /// a higher level is left. A refusal raises the level for every block
    /// written after it, not only this one.
    pub(super) fn write(&mut self, mut frame: Frame) -> std::result::Result<(), Failure> {
        let scratch = Arc::clone(&self.run.scratch);
        let bytes = loop {
            let bytes = frame.bytes.len() as u64;
            match scratch.reserve(&self.run.owner, bytes) {
                Ok(()) => break bytes,
                Err(shortfall) => {
                    if !scratch.raise_level(frame.level) {
                        return Err(Failure::Full(shortfall));
                    }
                    frame.recompress(scratch.level())?;
                }
            }
        };
        let rotate = self
            .run
            .chunks
            .back()
            .is_none_or(|chunk| chunk.bytes > 0 && chunk.bytes + bytes > CHUNK_BYTES);
        if rotate || self.file.is_none() {
            self.file = None;
            let path = scratch.new_file(&self.label);
            let file = scratch.create_file(&path);
            // The chunk is listed before it is written, so a failed write
            // still removes it and returns what it held.
            self.run.chunks.push_back(Chunk { path, bytes: 0 });
            match file {
                Ok(file) => self.file = Some(file),
                Err(source) => {
                    scratch.release(&self.run.owner, bytes);
                    let path = self.run.chunks.back().expect("chunk").path.clone();
                    return Err(Failure::Error(Error::IoPath {
                        action: "failed to create scratch file",
                        path,
                        source,
                    }));
                }
            }
        }
        let chunk = self.run.chunks.back_mut().expect("chunk");
        chunk.bytes += bytes;
        let started = Instant::now();
        let written = self
            .file
            .as_mut()
            .expect("chunk open")
            .write_all(&frame.bytes);
        scratch.add_write_time(started.elapsed());
        written?;
        let run = &mut self.run;
        run.items += u64::from(frame.items);
        run.bytes += bytes;
        run.max_raw = run.max_raw.max(frame.raw.len());
        run.max_frame = run.max_frame.max(frame.bytes.len());
        run.level = run.level.min(frame.level);
        run.owner
            .wrote(u64::from(frame.items), frame.raw.len() as u64, bytes);
        scratch.count_written(bytes);
        Ok(())
    }

    pub(super) fn finish(mut self) -> Run {
        self.file = None;
        self.run.owner.add_run();
        if self.run.level == i32::MAX {
            self.run.level = self.run.scratch.level();
        }
        self.run
    }
}

/// Items written one at a time, coded into blocks that are compressed a
/// batch at a time on the pool.
#[derive(Debug)]
pub(super) struct Stream<T> {
    writer: RunWriter,
    raw: Vec<u8>,
    raw_items: usize,
    delta: Delta,
    batch: Vec<(Vec<u8>, usize)>,
    pub(super) items: u64,
    _marker: std::marker::PhantomData<T>,
}

impl<T: Spill> Stream<T> {
    pub(super) fn new(scratch: &Arc<Scratch>, owner: &Arc<Owner>) -> Self {
        Self {
            writer: RunWriter::new(scratch, owner),
            raw: Vec::new(),
            raw_items: 0,
            delta: Delta::default(),
            batch: Vec::new(),
            items: 0,
            _marker: std::marker::PhantomData,
        }
    }

    pub(super) fn set_depth(&mut self, depth: u64) {
        self.writer.set_depth(depth);
    }

    pub(super) fn push(&mut self, item: &T) -> std::result::Result<(), Failure> {
        item.encode_after(&mut self.delta, &mut self.raw);
        self.raw_items += 1;
        self.items += 1;
        if self.raw.len() >= BLOCK_BYTES {
            self.seal_block();
            if self.batch.len() >= STREAM_BATCH_BLOCKS {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn seal_block(&mut self) {
        if self.raw_items > 0 {
            let raw = std::mem::replace(&mut self.raw, Vec::with_capacity(BLOCK_BYTES));
            self.batch.push((raw, self.raw_items));
            self.raw_items = 0;
            self.delta = Delta::default();
        }
    }

    fn flush(&mut self) -> std::result::Result<(), Failure> {
        let level = self.writer.run.scratch.level();
        let frames = std::mem::take(&mut self.batch)
            .into_par_iter()
            .map(|(raw, items)| Frame::compress(raw, items, level))
            .collect::<Vec<_>>();
        for frame in frames {
            self.writer.write(frame?)?;
        }
        Ok(())
    }

    pub(super) fn finish(mut self) -> std::result::Result<Run, Failure> {
        self.seal_block();
        self.flush()?;
        Ok(self.writer.finish())
    }
}

/// One block read back, decoded item by item.
#[derive(Debug)]
pub(super) struct Block {
    raw: Vec<u8>,
    position: usize,
    left: u32,
    delta: Delta,
}

impl Block {
    pub(super) fn next<T: Spill>(&mut self) -> Option<Result<T>> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let mut input = &self.raw[self.position..];
        let before = input.len();
        let item = T::decode_after(&mut self.delta, &mut input);
        self.position += before - input.len();
        if item.is_ok() && self.left == 0 && self.position != self.raw.len() {
            return Some(Err(Error::Decode("scratch block has trailing bytes")));
        }
        Some(item)
    }
}

/// Where a reader's chunks come from.
#[derive(Debug)]
enum Chunks {
    /// A run read once: each chunk is removed once read.
    Consumed(Run),
    /// A spool read as often as needed: chunks stay.
    Kept {
        paths: Vec<PathBuf>,
        next: usize,
        owner: Arc<Owner>,
    },
}

/// Reads one run's blocks in order. The chunk is opened for each block and
/// closed after it, so a merge of thousands of runs holds no file open
/// between reads and stays under any limit on open files.
#[derive(Debug)]
pub(super) struct Cursor {
    chunks: Chunks,
    /// The chunk being read and where its next block starts.
    chunk: Option<(PathBuf, u64)>,
    compressed: Vec<u8>,
}

impl Cursor {
    pub(super) fn consume(run: Run) -> Self {
        Self {
            chunks: Chunks::Consumed(run),
            chunk: None,
            compressed: Vec::new(),
        }
    }

    pub(super) fn keep(run: &Run) -> Self {
        Self {
            chunks: Chunks::Kept {
                paths: run.chunks.iter().map(|chunk| chunk.path.clone()).collect(),
                next: 0,
                owner: Arc::clone(&run.owner),
            },
            chunk: None,
            compressed: Vec::new(),
        }
    }

    fn owner(&self) -> &Owner {
        match &self.chunks {
            Chunks::Consumed(run) => &run.owner,
            Chunks::Kept { owner, .. } => owner,
        }
    }

    /// Move to the next chunk, after removing the one read when consumed.
    fn next_chunk(&mut self) -> bool {
        let finished = self.chunk.take();
        let path = match &mut self.chunks {
            Chunks::Consumed(run) => {
                if finished.is_some()
                    && let Some(chunk) = run.chunks.pop_front()
                {
                    run.scratch.remove(&run.owner, &chunk.path, chunk.bytes);
                }
                run.chunks.front().map(|chunk| chunk.path.clone())
            }
            Chunks::Kept { paths, next, .. } => {
                let path = paths.get(*next).cloned();
                *next += 1;
                path
            }
        };
        self.chunk = path.map(|path| (path, 0));
        self.chunk.is_some()
    }

    /// The next block, or `None` past the last.
    pub(super) fn next_block(&mut self) -> Result<Option<Block>> {
        loop {
            if self.chunk.is_none() && !self.next_chunk() {
                return Ok(None);
            }
            let (path, position) = self.chunk.as_mut().expect("chunk");
            let mut file = File::open(&*path).map_err(|source| Error::IoPath {
                action: "failed to open scratch file",
                path: path.clone(),
                source,
            })?;
            file.seek(SeekFrom::Start(*position))?;
            let mut header = [0u8; HEADER_BYTES];
            match read_full(&mut file, &mut header)? {
                0 => {
                    drop(file);
                    // Removes the chunk just read when the run is consumed.
                    self.next_chunk();
                    continue;
                }
                HEADER_BYTES => {}
                _ => {
                    return Err(Error::BadScratch {
                        path: path.clone(),
                        detail: "truncated block header",
                    });
                }
            }
            let field = |index: usize| {
                u32::from_le_bytes(
                    header[index * 4..index * 4 + 4]
                        .try_into()
                        .expect("4 bytes"),
                ) as usize
            };
            let (compressed_len, raw_len, items) = (field(0), field(1), field(2));
            self.compressed.resize(compressed_len, 0);
            file.read_exact(&mut self.compressed)
                .map_err(|_| Error::BadScratch {
                    path: path.clone(),
                    detail: "truncated block",
                })?;
            *position += (HEADER_BYTES + compressed_len) as u64;
            let mut raw = Vec::new();
            raw.try_reserve_exact(raw_len).map_err(|_| {
                Error::OutOfResources(format!(
                    "scratch block of {raw_len} bytes in {} does not fit in memory",
                    path.display()
                ))
            })?;
            raw.resize(raw_len, 0);
            let decompressed = zstd::bulk::decompress_to_buffer(&self.compressed, &mut raw[..]);
            if decompressed.ok() != Some(raw_len) {
                return Err(Error::BadScratch {
                    path: path.clone(),
                    detail: "block does not decompress",
                });
            }
            self.owner().read(
                items as u64,
                raw_len as u64,
                (HEADER_BYTES + compressed_len) as u64,
            );
            return Ok(Some(Block {
                raw,
                position: 0,
                left: items as u32,
                delta: Delta::default(),
            }));
        }
    }
}

/// Fill `buffer` from `file`, or stop at its end: the bytes read.
fn read_full(file: &mut File, buffer: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(filled)
}

type Fetched = Result<Option<Block>>;

/// Reads and decompresses blocks of several runs ahead of their consumer,
/// one block ahead per run, on threads of its own: the merge that consumes
/// them may itself run on the pool.
pub(super) struct Prefetch {
    scratch: Arc<Scratch>,
    blocks: Vec<Receiver<Fetched>>,
    requests: Option<Sender<usize>>,
    threads: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for Prefetch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Prefetch")
            .field("sources", &self.blocks.len())
            .finish_non_exhaustive()
    }
}

impl Prefetch {
    pub(super) fn start(scratch: &Arc<Scratch>, cursors: Vec<Cursor>, threads: usize) -> Self {
        let (requests, queue) = mpsc::channel::<usize>();
        let (senders, blocks): (Vec<SyncSender<Fetched>>, Vec<Receiver<Fetched>>) =
            cursors.iter().map(|_| mpsc::sync_channel(1)).unzip();
        let shared = Arc::new((
            Mutex::new(queue),
            cursors.into_iter().map(Mutex::new).collect::<Vec<_>>(),
            senders,
        ));
        let threads = (0..threads.clamp(1, blocks.len().max(1)))
            .map(|_| {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || {
                    let (queue, cursors, senders) = &*shared;
                    loop {
                        let Ok(source) = queue.lock().expect("queue lock").recv() else {
                            return;
                        };
                        let block = cursors[source].lock().expect("cursor lock").next_block();
                        let _ = senders[source].send(block);
                    }
                })
            })
            .collect();
        for source in 0..blocks.len() {
            let _ = requests.send(source);
        }
        Self {
            scratch: Arc::clone(scratch),
            blocks,
            requests: Some(requests),
            threads,
        }
    }

    /// Source `source`'s next block, and a request for the one after.
    pub(super) fn next(&self, source: usize) -> Fetched {
        let started = Instant::now();
        let block = self.blocks[source]
            .recv()
            .map_err(|_| Error::Inconsistent("a scratch reader stopped".into()))?;
        self.scratch.add_read_wait(started.elapsed());
        if let Ok(Some(_)) = &block
            && let Some(requests) = &self.requests
        {
            let _ = requests.send(source);
        }
        block
    }
}

impl Drop for Prefetch {
    fn drop(&mut self) {
        // Closing the queue ends the threads; a block in flight is never
        // blocked on, since each source has at most one outstanding.
        drop(self.requests.take());
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}
