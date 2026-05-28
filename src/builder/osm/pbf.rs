use std::{
    fs::{self, File},
    io::{self, BufReader, Read},
    path::Path,
};

use anyhow::{Context, Result};
use indicatif::ProgressBar;
use osmpbf::ElementReader;

use crate::builder::progress::byte_progress_bar;

pub(crate) fn element_reader_with_progress(
    input: &Path,
    message: &'static str,
) -> Result<(ElementReader<BufReader<ProgressReader<File>>>, ProgressBar)> {
    let input_bytes = fs::metadata(input)
        .with_context(|| format!("failed to stat {}", input.display()))?
        .len();
    let progress = byte_progress_bar(input_bytes, message);
    let file = File::open(input).with_context(|| format!("failed to open {}", input.display()))?;
    let reader = ElementReader::new(BufReader::new(ProgressReader {
        inner: file,
        progress: progress.clone(),
    }));
    Ok((reader, progress))
}

pub(crate) fn input_bytes(input: &Path) -> Result<u64> {
    Ok(fs::metadata(input)
        .with_context(|| format!("failed to stat {}", input.display()))?
        .len())
}

pub(crate) struct ProgressReader<R> {
    inner: R,
    progress: ProgressBar,
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let bytes_read = self.inner.read(buffer)?;
        self.progress.inc(bytes_read as u64);
        Ok(bytes_read)
    }
}
