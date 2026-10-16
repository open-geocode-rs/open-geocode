//! Talking to the servers extracts come from: their size and date, the
//! extract itself, and the checksum it is published with.

use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};
use open_geocode::builder::BuildCancelled;
use ureq::Agent;

use crate::format;

const CHUNK_BYTES: usize = 1 << 20;

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
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };
    Ok(Remote {
        bytes: header("content-length").and_then(|value| value.parse().ok()),
    })
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

/// Downloads `url` to the partial file of `path`, continuing an earlier
/// download that stopped. `progress` is told the bytes on disk so far and
/// the whole size, when the server says it.
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
    let mut request = agent().get(url);
    if have > 0 {
        request = request.header("Range", format!("bytes={have}-"));
    }
    let mut response = request
        .call()
        .with_context(|| format!("failed to download {url}"))?;
    let resumed = response.status().as_u16() == 206;
    let length = response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let (mut done, total) = if resumed {
        (have, length.map(|length| length + have))
    } else {
        (0, length)
    };
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(resumed)
        .truncate(!resumed)
        .open(&partial)
        .with_context(|| format!("failed to create {}", partial.display()))?;
    progress(done, total);
    let mut body = response.body_mut().as_reader();
    let mut buffer = vec![0; CHUNK_BYTES];
    loop {
        if cancel.load(Ordering::Relaxed) {
            file.flush()?;
            return Err(BuildCancelled.into());
        }
        let read = body
            .read(&mut buffer)
            .with_context(|| format!("the download of {url} broke off"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .with_context(|| format!("failed to write {}", partial.display()))?;
        done += read as u64;
        progress(done, total);
    }
    file.flush()?;
    if let Some(total) = total
        && done != total
    {
        bail!("the download of {url} ended at {done} of {total} bytes");
    }
    Ok(partial)
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
