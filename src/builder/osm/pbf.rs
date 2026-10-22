use std::{
    fs::{self, File},
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::Instant,
};

use crate::error::{Error, Result};
use osmpbf::{Blob, BlobDecode, BlobReader, ByteOffset, PrimitiveBlock};
use rayon::prelude::*;

use crate::builder::progress::{BuildStage, InputCounter, Progress};

const READ_BUFFER_BYTES: usize = 4 << 20;
/// Blobs decoded per parallel batch, per worker thread.
const BLOBS_PER_THREAD: usize = 4;

/// Decode PBF data blocks in parallel and hand each block's result to `sink`
/// in file order. With `offsets`, only the blobs starting at those byte
/// offsets are read, which lets later passes skip blocks they do not need.
///
/// Memory stays bounded: one batch of blobs is decoded at a time while the
/// next is read. The stage is measured in bytes when the whole file is read
/// and in blocks when only `offsets` are. Returns the bytes read from the
/// file, which on a disk-bound input is what a pass costs.
pub(crate) fn for_each_block<T, M, S>(
    input: &Path,
    progress: &Progress,
    stage: BuildStage,
    offsets: Option<&[u64]>,
    map: M,
    mut sink: S,
) -> Result<u64>
where
    T: Send,
    M: Fn(u64, PrimitiveBlock) -> Result<T> + Sync,
    S: FnMut(T) -> Result<()>,
{
    let batch_size = rayon::current_num_threads() * BLOBS_PER_THREAD;
    let total = match offsets {
        Some(offsets) => offsets.len() as u64,
        None => input_bytes(input)?,
    };
    progress.start(stage, Some(total))?;
    let file = File::open(input).map_err(|source| Error::IoPath {
        action: "failed to open",
        path: input.to_path_buf(),
        source,
    })?;
    let read_bytes = Arc::new(AtomicU64::new(0));
    let reader = Buffered {
        inner: BufReader::with_capacity(
            READ_BUFFER_BYTES,
            ProgressReader {
                inner: file,
                progress: offsets.is_none().then(|| progress.clone()),
                read_bytes: Arc::clone(&read_bytes),
                read: progress.input(input),
            },
        ),
        position: 0,
    };
    let mut blobs = BlobReader::new_seekable(reader)?;

    std::thread::scope(|scope| -> Result<()> {
        let (sender, receiver) = mpsc::sync_channel::<Result<Vec<Blob>>>(2);
        let progress_for_reader = progress.clone();
        scope.spawn(move || {
            let mut batch = Vec::with_capacity(batch_size);
            let mut next = |index: usize| -> Option<osmpbf::Result<Blob>> {
                match offsets {
                    Some(offsets) => {
                        let offset = *offsets.get(index)?;
                        progress_for_reader.advance(1);
                        Some(blobs.blob_from_offset(ByteOffset(offset)))
                    }
                    None => blobs.next(),
                }
            };
            let mut index = 0;
            loop {
                let blob = next(index);
                index += 1;
                match blob {
                    Some(Ok(blob)) => {
                        batch.push(blob);
                        if batch.len() == batch_size
                            && sender.send(Ok(std::mem::take(&mut batch))).is_err()
                        {
                            return;
                        }
                    }
                    Some(Err(error)) => {
                        let _ = sender.send(Err(Error::from(error)));
                        return;
                    }
                    None => {
                        if !batch.is_empty() {
                            let _ = sender.send(Ok(batch));
                        }
                        return;
                    }
                }
            }
        });

        // Dropping the receiver on an early return stops the reader thread.
        for batch in receiver {
            progress.check()?;
            let outputs = batch?
                .par_iter()
                .map(|blob| {
                    let offset = blob.offset().map_or(0, |offset| offset.0);
                    match blob.decode()? {
                        BlobDecode::OsmData(block) => map(offset, block).map(Some),
                        BlobDecode::OsmHeader(_) | BlobDecode::Unknown(_) => Ok(None),
                    }
                })
                .collect::<Vec<Result<Option<T>>>>();
            for output in outputs {
                if let Some(output) = output? {
                    sink(output)?;
                }
            }
        }
        Ok(())
    })?;
    progress.finish(stage);
    Ok(read_bytes.load(Ordering::Relaxed))
}

/// When the extract's data was taken from OpenStreetMap, as its header
/// states it, in seconds since the Unix epoch. `None` when the header does
/// not say; Geofabrik and planet extracts do.
pub fn replication_timestamp(input: &Path) -> Result<Option<i64>> {
    let file = File::open(input).map_err(|source| Error::IoPath {
        action: "failed to open",
        path: input.to_path_buf(),
        source,
    })?;
    let mut blobs = BlobReader::new(BufReader::new(file));
    let Some(blob) = blobs.next() else {
        return Ok(None);
    };
    let blob = blob?;
    match blob.decode()? {
        BlobDecode::OsmHeader(header) => Ok(header.osmosis_replication_timestamp()),
        BlobDecode::OsmData(_) | BlobDecode::Unknown(_) => Ok(None),
    }
}

/// The bounding box the extract's header states, `[min_lon, min_lat, max_lon,
/// max_lat]`; Geofabrik sets it to the bounds of the extract's polygon.
pub(crate) fn header_bbox(input: &Path) -> Result<Option<[f64; 4]>> {
    let file = File::open(input).map_err(|source| Error::IoPath {
        action: "failed to open",
        path: input.to_path_buf(),
        source,
    })?;
    let mut blobs = BlobReader::new(BufReader::new(file));
    let Some(blob) = blobs.next() else {
        return Ok(None);
    };
    match blob?.decode()? {
        BlobDecode::OsmHeader(header) => Ok(header.bbox().map(|bbox| {
            [
                bbox.left,
                bbox.bottom.min(bbox.top),
                bbox.right,
                bbox.bottom.max(bbox.top),
            ]
        })),
        BlobDecode::OsmData(_) | BlobDecode::Unknown(_) => Ok(None),
    }
}

pub(crate) fn input_bytes(input: &Path) -> Result<u64> {
    Ok(fs::metadata(input)?.len())
}

/// A buffered reader that keeps its buffer across a seek to where it already
/// holds the bytes. `BlobReader` seeks to every blob it is asked for, and
/// `BufReader` drops its whole buffer on any absolute seek, so reading the
/// blobs of one kind, which mostly follow one another, read the file again
/// buffer by buffer for each.
struct Buffered<R> {
    inner: BufReader<R>,
    position: u64,
}

impl<R: Read> Read for Buffered<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.position += read as u64;
        Ok(read)
    }
}

impl<R: Seek> Seek for Buffered<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let out_of_range = || io::Error::other("seek offset out of range");
        let delta = match position {
            SeekFrom::Start(target) => i64::try_from(target)
                .ok()
                .zip(i64::try_from(self.position).ok())
                .map(|(target, current)| target - current)
                .ok_or_else(out_of_range)?,
            SeekFrom::Current(delta) => delta,
            SeekFrom::End(_) => {
                self.position = self.inner.seek(position)?;
                return Ok(self.position);
            }
        };
        let target = self
            .position
            .checked_add_signed(delta)
            .ok_or_else(out_of_range)?;
        self.inner.seek_relative(delta)?;
        self.position = target;
        Ok(target)
    }
}

/// Counts what is read from the input, and the time it takes, for the
/// build report; and advances the stage when it is measured in bytes.
struct ProgressReader<R> {
    inner: R,
    progress: Option<Progress>,
    read_bytes: Arc<AtomicU64>,
    read: Arc<InputCounter>,
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let started = Instant::now();
        let bytes_read = self.inner.read(buffer)?;
        self.read_bytes
            .fetch_add(bytes_read as u64, Ordering::Relaxed);
        self.read.add(bytes_read as u64, started.elapsed());
        if let Some(progress) = &self.progress {
            progress.advance(bytes_read as u64);
        }
        Ok(bytes_read)
    }
}

impl<R: Seek> Seek for ProgressReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seek_within_the_buffer_reads_the_file_once() {
        let data = (0..=255u8).cycle().take(10_000).collect::<Vec<_>>();
        let read_bytes = Arc::new(AtomicU64::new(0));
        let mut reader = Buffered {
            inner: BufReader::with_capacity(
                4_096,
                ProgressReader {
                    inner: io::Cursor::new(data.clone()),
                    progress: None,
                    read_bytes: Arc::clone(&read_bytes),
                    read: Arc::default(),
                },
            ),
            position: 0,
        };
        let mut read_at = |offset: u64| {
            assert_eq!(reader.seek(SeekFrom::Start(offset)).expect("seek"), offset);
            let mut byte = [0];
            reader.read_exact(&mut byte).expect("read");
            byte[0]
        };
        for offset in [0, 10, 100, 4_000, 3_000, 9_999] {
            assert_eq!(read_at(offset), data[offset as usize]);
        }
        // The first buffer serves every offset below 4,096, back to 3,000
        // after 4,000 too; only 9,999 is read again.
        assert_eq!(read_bytes.load(Ordering::Relaxed), 4_096 + 1);
        assert_eq!(reader.seek(SeekFrom::Current(-1)).expect("seek"), 9_999);
    }
}
