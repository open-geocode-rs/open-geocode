use std::{
    fs::{self, File},
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::Path,
    sync::mpsc,
};

use anyhow::{Context, Result, anyhow};
use indicatif::ProgressBar;
use osmpbf::{Blob, BlobDecode, BlobReader, ByteOffset, PrimitiveBlock};
use rayon::prelude::*;

use crate::builder::progress::{byte_progress_bar, item_progress_bar};

const READ_BUFFER_BYTES: usize = 4 << 20;
/// Blobs decoded per parallel batch, per worker thread.
const BLOBS_PER_THREAD: usize = 4;

/// Decode PBF data blocks in parallel and hand each block's result to `sink`
/// in file order. With `offsets`, only the blobs starting at those byte
/// offsets are read, which lets later passes skip blocks they do not need.
///
/// Memory stays bounded: one batch of blobs is decoded at a time while the
/// next is read.
pub(crate) fn for_each_block<T, M, S>(
    input: &Path,
    message: &'static str,
    offsets: Option<&[u64]>,
    map: M,
    mut sink: S,
) -> Result<()>
where
    T: Send,
    M: Fn(u64, PrimitiveBlock) -> Result<T> + Sync,
    S: FnMut(T) -> Result<()>,
{
    let batch_size = rayon::current_num_threads() * BLOBS_PER_THREAD;
    let progress = match offsets {
        Some(offsets) => item_progress_bar(offsets.len() as u64, message),
        None => byte_progress_bar(input_bytes(input)?, message),
    };
    let file = File::open(input).with_context(|| format!("failed to open {}", input.display()))?;
    let reader = BufReader::with_capacity(
        READ_BUFFER_BYTES,
        ProgressReader {
            inner: file,
            progress: offsets.is_none().then(|| progress.clone()),
        },
    );
    let mut blobs = BlobReader::new_seekable(reader)
        .with_context(|| format!("failed to read {}", input.display()))?;

    std::thread::scope(|scope| -> Result<()> {
        let (sender, receiver) = mpsc::sync_channel::<Result<Vec<Blob>>>(2);
        let progress_for_reader = progress.clone();
        scope.spawn(move || {
            let mut batch = Vec::with_capacity(batch_size);
            let mut next = |index: usize| -> Option<osmpbf::Result<Blob>> {
                match offsets {
                    Some(offsets) => {
                        let offset = *offsets.get(index)?;
                        progress_for_reader.inc(1);
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
                        let _ = sender.send(Err(anyhow!(error)));
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
                if let Some(output) =
                    output.with_context(|| format!("failed to parse {}", input.display()))?
                {
                    sink(output)?;
                }
            }
        }
        Ok(())
    })?;
    progress.finish_with_message(format!("{message} complete"));
    Ok(())
}

pub(crate) fn input_bytes(input: &Path) -> Result<u64> {
    Ok(fs::metadata(input)
        .with_context(|| format!("failed to stat {}", input.display()))?
        .len())
}

struct ProgressReader<R> {
    inner: R,
    progress: Option<ProgressBar>,
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let bytes_read = self.inner.read(buffer)?;
        if let Some(progress) = &self.progress {
            progress.inc(bytes_read as u64);
        }
        Ok(bytes_read)
    }
}

impl<R: Seek> Seek for ProgressReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}
