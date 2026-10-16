//! Talking to the servers extracts come from: their size and date, the
//! extract itself, and the checksum it is published with.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};
use open_geocode::builder::BuildCancelled;
use ureq::{Agent, http::Response};

use crate::format;

const CHUNK_BYTES: usize = 1 << 20;
/// How long a download may receive nothing before it is given up as stalled.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// How often a download waiting for data looks at its cancel flag.
const POLL: Duration = Duration::from_millis(200);

fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .user_agent(concat!(
                "open-geocode/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/open-geocode-rs/open-geocode)"
            ))
            .build()
            .into()
    })
}

/// A file on a server, as its headers describe it.
#[derive(Debug, Clone, Copy)]
pub struct Remote {
    pub bytes: Option<u64>,
}

pub fn head(url: &str) -> Result<Remote> {
    let response = agent()
        .head(url)
        .call()
        .with_context(|| format!("failed to reach {url}"))?;
    Ok(Remote {
        bytes: header(&response, "content-length").and_then(|value| value.parse().ok()),
    })
}

fn header<B>(response: &Response<B>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// The time an extract's data is from, read from its replication state
/// file (`timestamp=2026-10-15T20\:21\:42Z`).
pub fn state_timestamp(url: &str) -> Result<i64> {
    let text = agent()
        .get(url)
        .call()
        .with_context(|| format!("failed to reach {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("failed to read {url}"))?;
    text.lines()
        .find_map(|line| line.strip_prefix("timestamp="))
        .and_then(format::parse_time)
        .with_context(|| format!("{url} names no timestamp"))
}

/// The MD5 sum an extract is published with, from `<url>.md5`.
pub fn published_md5(url: &str) -> Result<String> {
    let url = format!("{url}.md5");
    let text = agent()
        .get(&url)
        .call()
        .with_context(|| format!("failed to reach {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("failed to read {url}"))?;
    let sum = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    if sum.len() != 32 || !sum.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("{url} holds no MD5 sum");
    }
    Ok(sum)
}

/// The file a download is written to until it is complete and checked.
pub fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

/// Beside a partial file: the ETag, or else the Last-Modified date, of the
/// response it was written from. A download continues the partial file only
/// while the server still has that version of the file.
fn version_path(partial: &Path) -> PathBuf {
    let mut name = partial.file_name().unwrap_or_default().to_os_string();
    name.push(".version");
    partial.with_file_name(name)
}

/// Moves a complete, checked download into place.
pub fn complete(partial: &Path, path: &Path) -> Result<()> {
    fs::rename(partial, path)
        .with_context(|| format!("failed to move {} into place", path.display()))?;
    let _ = fs::remove_file(version_path(partial));
    Ok(())
}

/// Removes a partial file there is no use continuing from.
pub fn discard(partial: &Path) {
    let _ = fs::remove_file(partial);
    let _ = fs::remove_file(version_path(partial));
}

/// Downloads `url` to the partial file of `path`, continuing an earlier
/// download that stopped while the server still has the same version of
/// the file, and starting over when it does not. `progress` is told the
/// bytes on disk so far and the whole size, when the server says it.
pub fn download(
    url: &str,
    path: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let partial = partial_path(path);
    let have = fs::metadata(&partial).map_or(0, |meta| meta.len());
    let version = fs::read_to_string(version_path(&partial))
        .ok()
        .map(|version| version.trim().to_string())
        .filter(|version| !version.is_empty());
    let resume = version.filter(|_| have > 0).map(|version| (have, version));
    match fetch(url, &partial, resume.as_ref(), cancel, &mut progress)? {
        Fetched::Done => Ok(partial),
        // The partial file is no part of what the server has now.
        Fetched::Stale => {
            discard(&partial);
            match fetch(url, &partial, None, cancel, &mut progress)? {
                Fetched::Done => Ok(partial),
                Fetched::Stale => bail!("{url} cannot be downloaded from its start"),
            }
        }
    }
}

enum Fetched {
    Done,
    /// The partial file cannot be continued.
    Stale,
}

/// One request for `url`: the bytes after `resume`'s when given, else all
/// of it.
fn fetch(
    url: &str,
    partial: &Path,
    resume: Option<&(u64, String)>,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<Fetched> {
    let mut request = agent()
        .get(url)
        .config()
        .http_status_as_error(false)
        .build();
    if let Some((have, version)) = resume {
        // Without the same version the server sends the whole file again.
        request = request
            .header("Range", format!("bytes={have}-"))
            .header("If-Range", version);
    }
    let response = request
        .call()
        .with_context(|| format!("failed to download {url}"))?;
    let length = header(&response, "content-length").and_then(|value| value.parse::<u64>().ok());
    // `bytes 100-199/200` for a part, `bytes */200` for a range past the end.
    let range = header(&response, "content-range").and_then(|value| {
        let (span, total) = value.strip_prefix("bytes ")?.split_once('/')?;
        let start = span
            .split_once('-')
            .and_then(|(start, _)| start.parse().ok());
        Some((start, total.parse::<u64>().ok()))
    });
    let (start, total) = match (response.status().as_u16(), resume) {
        (200, _) => (0, length),
        (206, Some((have, _))) => match range {
            Some((Some(start), total)) if start == *have => (start, total),
            _ => return Ok(Fetched::Stale),
        },
        // Nothing past the partial file: it is the whole file when it is as
        // long as the server says, and its checksum then tells for sure.
        (416, Some((have, _))) => {
            return match range {
                Some((_, Some(total))) if total == *have => {
                    progress(*have, Some(total));
                    Ok(Fetched::Done)
                }
                _ => Ok(Fetched::Stale),
            };
        }
        (status, _) => bail!("{url} answered HTTP {status}"),
    };
    if start == 0 {
        let version = header(&response, "etag").or_else(|| header(&response, "last-modified"));
        match version {
            Some(version) => fs::write(version_path(partial), version)
                .with_context(|| format!("failed to write beside {}", partial.display()))?,
            None => {
                let _ = fs::remove_file(version_path(partial));
            }
        }
    }
    let total = total.or(length.map(|length| length + start));
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(start > 0)
        .truncate(start == 0)
        .open(partial)
        .with_context(|| format!("failed to create {}", partial.display()))?;
    let mut done = start;
    progress(done, total);
    // The body is read on a thread of its own, so a server that stops
    // sending is noticed, and a cancel is seen, while a read waits.
    let (sender, chunks) = mpsc::sync_channel::<io::Result<Vec<u8>>>(4);
    let mut body = response.into_body().into_reader();
    thread::spawn(move || {
        loop {
            let mut chunk = vec![0; CHUNK_BYTES];
            let read = match body.read(&mut chunk) {
                Ok(0) => return,
                Ok(read) => read,
                Err(error) => {
                    let _ = sender.send(Err(error));
                    return;
                }
            };
            chunk.truncate(read);
            if sender.send(Ok(chunk)).is_err() {
                return;
            }
        }
    });
    let mut heard = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            file.flush()?;
            return Err(BuildCancelled.into());
        }
        match chunks.recv_timeout(POLL) {
            Ok(Ok(chunk)) => {
                file.write_all(&chunk)
                    .with_context(|| format!("failed to write {}", partial.display()))?;
                done += chunk.len() as u64;
                progress(done, total);
                heard = Instant::now();
            }
            Ok(Err(error)) => {
                file.flush()?;
                return Err(error).with_context(|| format!("the download of {url} broke off"));
            }
            Err(RecvTimeoutError::Timeout) if heard.elapsed() >= IDLE_TIMEOUT => {
                file.flush()?;
                bail!(
                    "the download of {url} stalled: nothing arrived for {} s",
                    IDLE_TIMEOUT.as_secs()
                );
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    file.flush()?;
    if let Some(total) = total
        && done != total
    {
        bail!("the download of {url} ended at {done} of {total} bytes");
    }
    Ok(Fetched::Done)
}

/// Checks a file against an MD5 sum. `progress` is told the bytes read.
pub fn check_md5(
    path: &Path,
    expected: &str,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<()> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(CHUNK_BYTES, file);
    let mut hasher = Md5::new();
    let mut buffer = vec![0; CHUNK_BYTES];
    let mut done = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(BuildCancelled.into());
        }
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        done += read as u64;
        progress(done);
    }
    let sum: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if sum != expected {
        bail!(
            "{} does not match its published checksum ({sum}, expected {expected})",
            path.display()
        );
    }
    Ok(())
}
