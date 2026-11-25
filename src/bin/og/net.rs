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
use serde::Deserialize;
use ureq::{Agent, ResponseExt, http::Response};

use crate::format;

const CHUNK_BYTES: usize = 1 << 20;
/// How long a download may receive nothing before it is given up as stalled.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the index body may take to arrive once its headers are in.
const INDEX_BODY_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(60)
};
/// How often a download waiting for data looks at its cancel flag.
const POLL: Duration = Duration::from_millis(200);
/// Geofabrik's list of every extract it serves and where each one is now.
pub(crate) const GEOFABRIK_INDEX: &str = "https://download.geofabrik.de/index-v1-nogeom.json";
/// How many times a request is repeated after a timeout or a 5xx answer.
const RETRIES: u32 = 3;

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

/// The wait before retry number `attempt` (from 0), doubling each time.
fn backoff(attempt: u32) -> Duration {
    let first = if cfg!(test) {
        Duration::from_millis(1)
    } else {
        Duration::from_secs(1)
    };
    first * 2u32.pow(attempt)
}

/// A timeout or a server error may pass. A 404 or any other answer will not,
/// so it is never repeated.
fn is_transient(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::Timeout(_) => true,
        ureq::Error::StatusCode(status) => *status >= 500,
        _ => false,
    }
}

/// Runs `request` and repeats it, with a growing wait, while it fails with a
/// timeout or a 5xx answer.
fn retrying<T>(mut request: impl FnMut() -> Result<T, ureq::Error>) -> Result<T, ureq::Error> {
    let mut attempt = 0;
    loop {
        match request() {
            Err(error) if attempt < RETRIES && is_transient(&error) => {
                thread::sleep(backoff(attempt));
                attempt += 1;
            }
            result => return result,
        }
    }
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

/// Geofabrik's index, read for where each extract is now. A link stored
/// beforehand goes stale when Geofabrik moves an extract, the index does not.
#[derive(Debug, Deserialize)]
pub(crate) struct Index {
    features: Vec<Feature>,
}

#[derive(Debug, Deserialize)]
struct Feature {
    properties: Properties,
}

#[derive(Debug, Deserialize)]
struct Properties {
    id: String,
    urls: Urls,
}

#[derive(Debug, Deserialize)]
struct Urls {
    pbf: String,
}

impl Index {
    pub(crate) fn fetch(url: &str) -> Result<Self> {
        // Only the index gets a body limit, extract downloads keep their idle timeout.
        retrying(|| {
            agent()
                .get(url)
                .config()
                .timeout_recv_body(Some(INDEX_BODY_TIMEOUT))
                .build()
                .call()
        })
        .with_context(|| format!("failed to reach {url}"))?
        .body_mut()
        .read_json()
        .with_context(|| format!("failed to read {url}"))
    }

    /// The download link of the extract the index calls `id`.
    pub(crate) fn link(&self, id: &str) -> Result<&str> {
        self.features
            .iter()
            .find(|feature| feature.properties.id == id)
            .map(|feature| feature.properties.urls.pbf.as_str())
            .with_context(|| format!("Geofabrik's index has no region {id}"))
    }
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
/// and the download both use that one URL, so for a dated file a new
/// snapshot published between them cannot split them.
pub(crate) fn published(url: &str) -> Result<Published> {
    let response =
        retrying(|| agent().head(url).call()).with_context(|| format!("failed to reach {url}"))?;
    let url = response.get_uri().to_string();
    let md5_url = format!("{url}.md5");
    let text = retrying(|| agent().get(&md5_url).call())
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
    let response = retrying(|| {
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
        let response = request.call()?;
        match response.status().as_u16() {
            status if status >= 500 => Err(ureq::Error::StatusCode(status)),
            _ => Ok(response),
        }
    })
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
        sync::{Arc, Mutex, atomic::AtomicUsize},
    };

    use super::*;

    /// What a route answers: a body, a redirect to a path or a full URL, or an
    /// error status.
    #[derive(Clone)]
    enum Answer {
        Body(Vec<u8>),
        Redirect(String),
        /// An error status with no body.
        Status(u16),
        /// Each answer once, in order, and the last one from then on.
        Sequence(Vec<Answer>),
        /// A 200 whose headers promise more body than is ever sent, then silence.
        Stall(Vec<u8>),
    }

    impl Answer {
        /// The answer for this request. A sequence moves on to its next one.
        fn next(&mut self) -> Answer {
            match self {
                Answer::Sequence(answers) if answers.len() > 1 => answers.remove(0),
                Answer::Sequence(answers) => answers[0].clone(),
                other => other.clone(),
            }
        }
    }

    /// A local HTTP server whose routes a test can change while it runs.
    /// Unknown paths answer 404.
    struct Server {
        base: String,
        routes: Arc<Mutex<Vec<(String, Answer)>>>,
        asked: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let routes: Arc<Mutex<Vec<(String, Answer)>>> = Arc::default();
            let served = Arc::clone(&routes);
            let asked: Arc<Mutex<Vec<String>>> = Arc::default();
            let record = Arc::clone(&asked);
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
                    record.lock().unwrap().push(format!("{method} {path}"));
                    let answer = served
                        .lock()
                        .unwrap()
                        .iter_mut()
                        .find(|(route, _)| *route == path)
                        .map(|(_, answer)| answer.next());
                    let (head, body) = match answer {
                        Some(Answer::Body(body)) => (
                            format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n", body.len()),
                            body,
                        ),
                        Some(Answer::Status(status)) => (
                            format!("HTTP/1.1 {status} Error\r\ncontent-length: 0\r\n"),
                            Vec::new(),
                        ),
                        Some(Answer::Redirect(to)) => (
                            format!(
                                "HTTP/1.1 307 Temporary Redirect\r\nlocation: {to}\r\ncontent-length: 0\r\n"
                            ),
                            Vec::new(),
                        ),
                        Some(Answer::Stall(sent)) => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                sent.len() + 1000
                            );
                            let _ = stream.write_all(head.as_bytes());
                            let _ = stream.write_all(&sent);
                            let _ = stream.flush();
                            thread::sleep(Duration::from_secs(10));
                            continue;
                        }
                        Some(Answer::Sequence(_)) | None => (
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
            Self {
                base,
                routes,
                asked,
            }
        }

        /// How many requests the path has had.
        fn requests(&self, path: &str) -> usize {
            let asked = self.asked.lock().unwrap();
            asked
                .iter()
                .filter(|line| line.ends_with(&format!(" {path}")))
                .count()
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

    const INDEX: &str = include_str!("fixtures/geofabrik-index.json");

    /// A fake Geofabrik. Its index lists Monaco, which `/europe/monaco-latest.osm.pbf`
    /// redirects to a dated file, and its sum is served beside that file.
    fn geofabrik(monaco: &[u8]) -> Server {
        let server = Server::start();
        server.route(
            "/index-v1-nogeom.json",
            Answer::Body(INDEX.replace("{base}", &server.base).into_bytes()),
        );
        server.route(
            "/europe/monaco-latest.osm.pbf",
            Answer::Redirect("/europe/monaco-261109.osm.pbf".into()),
        );
        server.route(
            "/europe/monaco-261109.osm.pbf",
            Answer::Body(monaco.to_vec()),
        );
        server.route(
            "/europe/monaco-261109.osm.pbf.md5",
            md5_file(&md5_hex(monaco), "monaco-261109.osm.pbf"),
        );
        server
    }

    /// What `og build` does for a region: the index's link, the sum, the
    /// download and the check. The file's bytes, or why it failed.
    fn build_download(server: &Server, id: &str) -> Result<Vec<u8>> {
        let index = Index::fetch(&server.url("/index-v1-nogeom.json"))?;
        let published = published(index.link(id)?)?;
        // One folder per call, since the tests run side by side.
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let call = CALLS.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("og-net-{}-{call}", std::process::id()));
        let path = dir.join("region.osm.pbf");
        let result =
            download(&published, &path, &AtomicBool::new(false), |_, _| {}).and_then(|partial| {
                check_md5(&partial, &published.md5, &AtomicBool::new(false), |_| {})?;
                Ok(fs::read(&partial)?)
            });
        let _ = fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn a_region_is_found_in_the_index_downloaded_and_checked() {
        let server = geofabrik(b"monaco 261109");

        assert_eq!(build_download(&server, "monaco").unwrap(), b"monaco 261109");
    }

    #[test]
    fn a_region_whose_extract_moved_is_found_where_the_index_says() {
        let server = geofabrik(b"monaco 261109");
        let moved = md5_hex(b"monaco, moved");
        server.route(
            "/index-v1-nogeom.json",
            Answer::Body(
                INDEX
                    .replace(
                        "{base}/europe/monaco-latest",
                        "{base}/europe/moved/monaco-latest",
                    )
                    .replace("{base}", &server.base)
                    .into_bytes(),
            ),
        );
        server.route(
            "/europe/moved/monaco-latest.osm.pbf",
            Answer::Body(b"monaco, moved".to_vec()),
        );
        server.route(
            "/europe/moved/monaco-latest.osm.pbf.md5",
            md5_file(&moved, "monaco-latest.osm.pbf"),
        );

        assert_eq!(build_download(&server, "monaco").unwrap(), b"monaco, moved");
    }

    #[test]
    fn a_region_the_index_does_not_have_is_an_error_naming_it() {
        let server = geofabrik(b"monaco 261109");

        let error = build_download(&server, "atlantis").unwrap_err();

        assert_eq!(
            error.to_string(),
            "Geofabrik's index has no region atlantis"
        );
    }

    #[test]
    fn an_index_names_iso_codes_as_a_list_or_joined_by_commas() {
        let server = geofabrik(b"monaco 261109");
        let index = Index::fetch(&server.url("/index-v1-nogeom.json")).unwrap();

        assert_eq!(
            index.link("monaco").unwrap(),
            server.url("/europe/monaco-latest.osm.pbf")
        );
        assert_eq!(
            index.link("haiti-and-domrep").unwrap(),
            server.url("/central-america/haiti-and-domrep-latest.osm.pbf")
        );
    }

    #[test]
    fn a_missing_extract_fails_at_once_naming_its_url_and_is_not_retried() {
        let server = geofabrik(b"monaco 261109");
        server.route("/europe/monaco-261109.osm.pbf.md5", Answer::Status(404));

        let error = build_download(&server, "monaco").unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            format!(
                "failed to reach {}: http status: 404",
                server.url("/europe/monaco-261109.osm.pbf.md5")
            )
        );
        assert_eq!(server.requests("/europe/monaco-261109.osm.pbf.md5"), 1);
    }

    #[test]
    fn a_server_that_answers_503_twice_is_retried_until_it_answers() {
        let server = geofabrik(b"monaco 261109");
        let busy = || Answer::Status(503);
        let index = INDEX.replace("{base}", &server.base).into_bytes();
        server.route(
            "/index-v1-nogeom.json",
            Answer::Sequence(vec![busy(), busy(), Answer::Body(index)]),
        );
        server.route(
            "/europe/monaco-261109.osm.pbf.md5",
            Answer::Sequence(vec![
                busy(),
                busy(),
                md5_file(&md5_hex(b"monaco 261109"), "monaco-261109.osm.pbf"),
            ]),
        );
        server.route(
            "/europe/monaco-261109.osm.pbf",
            Answer::Sequence(vec![
                busy(),
                busy(),
                Answer::Body(b"monaco 261109".to_vec()),
            ]),
        );

        assert_eq!(build_download(&server, "monaco").unwrap(), b"monaco 261109");
        assert_eq!(server.requests("/index-v1-nogeom.json"), 3);
        assert_eq!(server.requests("/europe/monaco-261109.osm.pbf.md5"), 3);
    }

    #[test]
    fn an_index_that_stalls_after_its_headers_is_given_up_on() {
        let server = Server::start();
        server.route("/index.json", Answer::Stall(br#"{"features": ["#.to_vec()));
        let started = Instant::now();

        let error = Index::fetch(&server.url("/index.json")).unwrap_err();

        assert!(started.elapsed() < Duration::from_secs(5), "{error:#}");
        assert!(format!("{error:#}").contains("timeout"), "{error:#}");
    }

    #[test]
    fn a_server_that_keeps_answering_503_is_given_up_on_after_the_retries() {
        let server = geofabrik(b"monaco 261109");
        server.route("/europe/monaco-261109.osm.pbf.md5", Answer::Status(503));

        let error = build_download(&server, "monaco").unwrap_err();

        assert!(format!("{error:#}").contains("503"), "{error:#}");
        assert_eq!(
            server.requests("/europe/monaco-261109.osm.pbf.md5"),
            1 + RETRIES as usize
        );
    }

    #[test]
    fn a_timeout_and_a_5xx_are_retried_and_nothing_else() {
        assert!(is_transient(&ureq::Error::Timeout(
            ureq::Timeout::RecvResponse
        )));
        assert!(is_transient(&ureq::Error::StatusCode(503)));
        assert!(!is_transient(&ureq::Error::StatusCode(404)));
        assert!(!is_transient(&ureq::Error::StatusCode(429)));
        assert!(!is_transient(&ureq::Error::HostNotFound));
    }

    #[test]
    fn a_file_that_does_not_match_its_sum_is_refused() {
        let server = geofabrik(b"monaco 261109");
        server.route(
            "/europe/monaco-261109.osm.pbf",
            Answer::Body(b"monaco, damaged".to_vec()),
        );

        let error = build_download(&server, "monaco").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not match its published checksum"),
            "{error:#}"
        );
    }
}
