//! Talking to the servers extracts come from: their size and date, the
//! extract itself, and the checksum it is published with.

use std::{
    fmt::Write as _,
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
use ureq::{Agent, ResponseExt, http::Response};

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
pub(crate) struct Remote {
    pub bytes: Option<u64>,
}

pub(crate) fn head(url: &str) -> Result<Remote> {
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
pub(crate) fn state_timestamp(url: &str) -> Result<i64> {
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

/// An extract where its server keeps it now, and the MD5 sum published
/// beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Published {
    pub url: String,
    pub md5: String,
}

/// Follows `url`'s redirects to the file they land on and reads its
/// `<file>.md5`. Geofabrik sends `-latest` to a dated file (`monaco-261109`),
/// or for its biggest extracts to a mirror that alone has the `.md5`. The sum
/// and the download both use that one file, so a new snapshot published
/// between them cannot split them.
pub(crate) fn published(url: &str) -> Result<Published> {
    let response = agent()
        .head(url)
        .call()
        .with_context(|| format!("failed to reach {url}"))?;
    let url = response.get_uri().to_string();
    let md5_url = format!("{url}.md5");
    let text = agent()
        .get(&md5_url)
        .call()
        .with_context(|| format!("failed to reach {md5_url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("failed to read {md5_url}"))?;
    let md5 = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    if md5.len() != 32 || !md5.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("{md5_url} holds no MD5 sum");
    }
    Ok(Published { url, md5 })
}

/// The file a download is written to until it is complete and checked.
pub(crate) fn partial_path(path: &Path) -> PathBuf {
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
pub(crate) fn complete(partial: &Path, path: &Path) -> Result<()> {
    fs::rename(partial, path)
        .with_context(|| format!("failed to move {} into place", path.display()))?;
    let _ = fs::remove_file(version_path(partial));
    Ok(())
}

/// Removes a partial file there is no use continuing from.
pub(crate) fn discard(partial: &Path) {
    let _ = fs::remove_file(partial);
    let _ = fs::remove_file(version_path(partial));
}

/// Downloads the published file to the partial file of `path`, continuing
/// an earlier download that stopped while the server still has the same
/// version of the file, and starting over when it does not. `progress` is told the
/// bytes on disk so far and the whole size, when the server says it.
pub(crate) fn download(
    published: &Published,
    path: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let url = &published.url;
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
    let total = total.or_else(|| length.map(|length| length + start));
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
pub(crate) fn check_md5(
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
    let sum = hasher
        .finalize()
        .iter()
        .fold(String::new(), |mut sum, byte| {
            let _ = write!(sum, "{byte:02x}");
            sum
        });
    if sum != expected {
        bail!(
            "{} does not match its published checksum ({sum}, expected {expected})",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        sync::{Arc, Mutex},
    };

    use super::*;

    /// What a route answers: a body, or a redirect to a path or a full URL.
    #[derive(Clone)]
    enum Answer {
        Body(Vec<u8>),
        Redirect(String),
    }

    /// A local HTTP server whose routes a test can change while it runs.
    /// Unknown paths answer 404.
    struct Server {
        base: String,
        routes: Arc<Mutex<Vec<(String, Answer)>>>,
    }

    impl Server {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let routes: Arc<Mutex<Vec<(String, Answer)>>> = Arc::default();
            let served = Arc::clone(&routes);
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let path = parts.next().unwrap_or_default().to_string();
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap() <= 2 {
                            break;
                        }
                    }
                    let answer = served
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|(route, _)| *route == path)
                        .map(|(_, answer)| answer.clone());
                    let (head, body) = match answer {
                        Some(Answer::Body(body)) => (
                            format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n", body.len()),
                            body,
                        ),
                        Some(Answer::Redirect(to)) => (
                            format!(
                                "HTTP/1.1 307 Temporary Redirect\r\nlocation: {to}\r\ncontent-length: 0\r\n"
                            ),
                            Vec::new(),
                        ),
                        None => (
                            "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n".to_string(),
                            Vec::new(),
                        ),
                    };
                    let _ = stream.write_all(format!("{head}connection: close\r\n\r\n").as_bytes());
                    if method != "HEAD" {
                        let _ = stream.write_all(&body);
                    }
                }
            });
            Self { base, routes }
        }

        fn route(&self, path: &str, answer: Answer) {
            let mut routes = self.routes.lock().unwrap();
            routes.retain(|(route, _)| route != path);
            routes.push((path.to_string(), answer));
        }

        fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base)
        }
    }

    fn md5_hex(bytes: &[u8]) -> String {
        Md5::digest(bytes)
            .iter()
            .fold(String::new(), |mut sum, byte| {
                let _ = write!(sum, "{byte:02x}");
                sum
            })
    }

    fn md5_file(sum: &str, name: &str) -> Answer {
        Answer::Body(format!("{sum}  {name}\n").into_bytes())
    }

    #[test]
    fn the_sum_is_read_beside_an_extract_served_where_it_is_asked_for() {
        let server = Server::start();
        let sum = md5_hex(b"kosovo");
        server.route("/kosovo-latest.osm.pbf", Answer::Body(b"kosovo".to_vec()));
        server.route(
            "/kosovo-latest.osm.pbf.md5",
            md5_file(&sum, "kosovo-latest.osm.pbf"),
        );

        let published = published(&server.url("/kosovo-latest.osm.pbf")).unwrap();

        assert_eq!(
            published,
            Published {
                url: server.url("/kosovo-latest.osm.pbf"),
                md5: sum,
            }
        );
    }

    #[test]
    fn the_sum_is_read_beside_the_dated_file_latest_redirects_to() {
        let server = Server::start();
        let sum = md5_hex(b"monaco 261109");
        server.route(
            "/monaco-latest.osm.pbf",
            Answer::Redirect("/monaco-261109.osm.pbf".into()),
        );
        server.route(
            "/monaco-261109.osm.pbf",
            Answer::Body(b"monaco 261109".to_vec()),
        );
        server.route(
            "/monaco-261109.osm.pbf.md5",
            md5_file(&sum, "monaco-261109.osm.pbf"),
        );

        let published = published(&server.url("/monaco-latest.osm.pbf")).unwrap();

        assert_eq!(
            published,
            Published {
                url: server.url("/monaco-261109.osm.pbf"),
                md5: sum,
            }
        );
    }

    /// Geofabrik's biggest extracts: `-latest` redirects to a mirror, and only
    /// the mirror has the `.md5`.
    #[test]
    fn the_sum_is_read_from_the_mirror_latest_redirects_to() {
        let (origin, mirror) = (Server::start(), Server::start());
        let sum = md5_hex(b"europe");
        origin.route(
            "/europe-latest.osm.pbf",
            Answer::Redirect(mirror.url("/geofabrik/europe-latest.osm.pbf")),
        );
        mirror.route(
            "/geofabrik/europe-latest.osm.pbf",
            Answer::Body(b"europe".to_vec()),
        );
        mirror.route(
            "/geofabrik/europe-latest.osm.pbf.md5",
            md5_file(&sum, "europe-261108.osm.pbf"),
        );

        let published = published(&origin.url("/europe-latest.osm.pbf")).unwrap();

        assert_eq!(
            published,
            Published {
                url: mirror.url("/geofabrik/europe-latest.osm.pbf"),
                md5: sum,
            }
        );
    }

    #[test]
    fn an_extract_with_no_sum_beside_it_is_an_error_naming_the_sum_url() {
        let server = Server::start();
        server.route("/europe-latest.osm.pbf", Answer::Body(b"europe".to_vec()));

        let error = published(&server.url("/europe-latest.osm.pbf")).unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            format!(
                "failed to reach {}: http status: 404",
                server.url("/europe-latest.osm.pbf.md5")
            )
        );
    }

    #[test]
    fn a_sum_file_without_a_sum_is_an_error() {
        let server = Server::start();
        server.route("/kosovo-latest.osm.pbf", Answer::Body(b"kosovo".to_vec()));
        server.route(
            "/kosovo-latest.osm.pbf.md5",
            Answer::Body(b"not a sum\n".to_vec()),
        );

        let error = published(&server.url("/kosovo-latest.osm.pbf")).unwrap_err();

        assert_eq!(
            error.to_string(),
            format!(
                "{} holds no MD5 sum",
                server.url("/kosovo-latest.osm.pbf.md5")
            )
        );
    }

    /// `-latest` moves on to a new snapshot after its sum was read: the
    /// download still fetches the snapshot the sum is of.
    #[test]
    fn a_snapshot_published_after_the_sum_is_read_does_not_reach_the_download() {
        let server = Server::start();
        let old = b"monaco 261109".to_vec();
        server.route(
            "/monaco-latest.osm.pbf",
            Answer::Redirect("/monaco-261109.osm.pbf".into()),
        );
        server.route("/monaco-261109.osm.pbf", Answer::Body(old.clone()));
        server.route(
            "/monaco-261109.osm.pbf.md5",
            md5_file(&md5_hex(&old), "monaco-261109.osm.pbf"),
        );

        let published = published(&server.url("/monaco-latest.osm.pbf")).unwrap();
        server.route(
            "/monaco-latest.osm.pbf",
            Answer::Redirect("/monaco-261110.osm.pbf".into()),
        );
        server.route(
            "/monaco-261110.osm.pbf",
            Answer::Body(b"monaco 261110".to_vec()),
        );

        let dir = std::env::temp_dir().join(format!("og-net-{}", std::process::id()));
        let path = dir.join("monaco.osm.pbf");
        let result =
            download(&published, &path, &AtomicBool::new(false), |_, _| {}).and_then(|partial| {
                check_md5(&partial, &published.md5, &AtomicBool::new(false), |_| {})?;
                Ok(fs::read(&partial)?)
            });
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(result.unwrap(), old);
    }
}
