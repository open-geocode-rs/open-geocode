//! Driving the load generator and reading what it measured.
//!
//! Every run goes through `oha` on cores 8-15 and leaves its JSON under
//! `raw/`, so any number in a report can be traced back to the file it came
//! from.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::host;

/// The p99 a rate must stay under to pass.
pub const P99_LIMIT_MS: f64 = 100.0;
/// The share of the asked-for rate that must actually be served.
pub const ACHIEVED_SHARE: f64 = 0.99;
/// The share of requests that must get a 2xx answer. A server may answer
/// what it cannot take on at once with an immediate refusal rather than make
/// everyone wait; up to one request in a thousand counts as served traffic
/// shed, not as a failed rate.
pub const ANSWERED_SHARE: f64 = 0.999;
/// Connections for rate runs: enough that a slow engine shows up as latency
/// and lost rate rather than as an idle load generator.
pub const RATE_CONNECTIONS: u32 = 256;
/// Connections for the untimed warm-up pass.
pub const WARM_CONNECTIONS: u32 = 16;
/// Matches the eight load-generator cores; more threads would only contend.
const WORKER_THREADS: &str = "8";
/// A request this slow has failed any criterion here already; the timeout
/// keeps one stuck connection from hanging a step forever.
const REQUEST_TIMEOUT: &str = "10s";

/// What to ask of oha.
#[derive(Debug, Clone, Copy)]
pub enum Shape {
    /// One connection, `n` requests back to back: service time.
    Sequential { n: usize },
    /// An untimed pass to fill caches, abandoned if it is not done within
    /// `limit`: an engine stalling on every request would otherwise hold
    /// the run for `n` request timeouts.
    Warm { n: usize, limit: Duration },
    /// A fixed arrival rate for a duration, with latency measured from when
    /// each request was due, not when it was sent.
    Rate { rps: f64, duration: Duration },
}

pub struct Oha {
    pub bin: PathBuf,
    run_dir: PathBuf,
    seq: AtomicU32,
}

/// The numbers a report needs from one oha JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OhaRun {
    /// The raw JSON, relative to the run directory.
    pub raw: String,
    pub requests: u64,
    pub ok: u64,
    pub non_2xx: u64,
    pub errors: u64,
    pub status: BTreeMap<String, u64>,
    pub error_kinds: BTreeMap<String, u64>,
    pub elapsed_s: f64,
    /// 2xx responses per second over the run.
    pub achieved_rps: f64,
    pub p50_ms: Option<f64>,
    pub p90_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    pub p999_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

#[derive(Deserialize)]
struct Raw {
    summary: RawSummary,
    #[serde(rename = "latencyPercentiles")]
    latency: BTreeMap<String, Option<f64>>,
    #[serde(rename = "statusCodeDistribution", default)]
    status: BTreeMap<String, u64>,
    #[serde(rename = "errorDistribution", default)]
    errors: BTreeMap<String, u64>,
}

#[derive(Deserialize)]
struct RawSummary {
    total: f64,
    slowest: Option<f64>,
}

impl OhaRun {
    pub fn parse(json: &str, raw: String) -> Result<OhaRun> {
        let r: Raw = serde_json::from_str(json).context("parsing oha JSON")?;
        let ok: u64 = r
            .status
            .iter()
            .filter(|(c, _)| c.starts_with('2'))
            .map(|(_, n)| n)
            .sum();
        let all: u64 = r.status.values().sum();
        let errors: u64 = r.errors.values().sum();
        let ms = |k: &str| r.latency.get(k).copied().flatten().map(|s| s * 1000.0);
        Ok(OhaRun {
            raw,
            requests: all + errors,
            ok,
            non_2xx: all - ok,
            errors,
            elapsed_s: r.summary.total,
            achieved_rps: if r.summary.total > 0.0 {
                ok as f64 / r.summary.total
            } else {
                0.0
            },
            p50_ms: ms("p50"),
            p90_ms: ms("p90"),
            p99_ms: ms("p99"),
            p999_ms: ms("p99.9"),
            max_ms: r.summary.slowest.map(|s| s * 1000.0),
            status: r.status,
            error_kinds: r.errors,
        })
    }

    /// Whether this run served `rate`: p99 under 100 ms, at least 99.9% of
    /// requests answered 2xx, and at least 99% of the rate actually served.
    /// The string says why, either way.
    pub fn judge(&self, rate: f64) -> (bool, String) {
        let p99 = self.p99_ms.unwrap_or(f64::INFINITY);
        let share = self.achieved_rps / rate;
        let unanswered = self.errors + self.non_2xx;
        let answered = self.requests > 0
            && unanswered as f64 <= (1.0 - ANSWERED_SHARE) * self.requests as f64;
        let pass = p99 < P99_LIMIT_MS && answered && share >= ACHIEVED_SHARE;
        let why = format!(
            "p99 {p99:.1} ms, {} errors, {} non-2xx, achieved {:.1} of {rate} req/s ({:.1}%)",
            self.errors,
            self.non_2xx,
            self.achieved_rps,
            share * 100.0
        );
        (pass, why)
    }
}

impl Oha {
    pub fn new(bin: PathBuf, run_dir: PathBuf) -> Oha {
        Oha {
            bin,
            run_dir,
            seq: AtomicU32::new(0),
        }
    }

    pub fn version(&self) -> Result<String> {
        let out = Command::new(&self.bin).arg("--version").output()?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Runs oha in the load slice over the URLs in `urls` and parses its JSON.
    /// `label` names the raw file.
    pub fn run(&self, label: &str, urls: &Path, shape: Shape) -> Result<OhaRun> {
        let n = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let rel = format!("raw/{n:04}-{label}.json");
        let raw = self.run_dir.join(&rel);
        std::fs::create_dir_all(raw.parent().unwrap())?;
        let mut cmd = host::load_command(&self.bin);
        cmd.args([
            "--no-tui",
            "--output-format",
            "json",
            "--worker-threads",
            WORKER_THREADS,
        ])
        .args(["-t", REQUEST_TIMEOUT])
        .arg("-o")
        .arg(&raw);
        match shape {
            Shape::Sequential { n } => {
                cmd.args(["-c", "1", "-n", &n.to_string()]);
            }
            Shape::Warm { n, .. } => {
                cmd.args(["-c", &WARM_CONNECTIONS.to_string(), "-n", &n.to_string()]);
            }
            Shape::Rate { rps, duration } => {
                // `-w` waits for requests in flight at the deadline instead
                // of counting them as errors, which would fail every rate.
                cmd.args([
                    "-q",
                    &format!("{rps}"),
                    "-z",
                    &format!("{}s", duration.as_secs()),
                ])
                .args([
                    "-c",
                    &RATE_CONNECTIONS.to_string(),
                    "--latency-correction",
                    "-w",
                ]);
            }
        }
        cmd.arg("--urls-from-file")
            .arg(urls)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        let limit = match shape {
            Shape::Warm { limit, .. } => Some(limit),
            _ => None,
        };
        let mut child = cmd.spawn().context("starting oha")?;
        host::register_child(child.id());
        let t0 = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break Ok(st),
                Ok(None) if limit.is_some_and(|l| t0.elapsed() > l) => {
                    // The child is sudo, which relays SIGTERM to oha.
                    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
                    let _ = child.wait();
                    host::unregister_child(child.id());
                    anyhow::bail!(
                        "oha {label} did not finish within {}s",
                        limit.unwrap_or_default().as_secs()
                    );
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => break Err(e),
            }
        };
        host::unregister_child(child.id());
        let status = status?;
        ensure!(status.success(), "oha {label} exited with {status}");
        let json =
            std::fs::read_to_string(&raw).with_context(|| format!("reading {}", raw.display()))?;
        OhaRun::parse(&json, rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mixed() -> OhaRun {
        OhaRun::parse(include_str!("testdata/oha-mixed.json"), "x".into()).unwrap()
    }

    #[test]
    fn parses_percentiles_and_status_codes() {
        let r = mixed();
        assert_eq!((r.requests, r.ok, r.non_2xx, r.errors), (200, 104, 96, 0));
        assert!((r.p50_ms.unwrap() - 342.533615).abs() < 1e-9);
        assert!((r.p90_ms.unwrap() - 564.571869).abs() < 1e-9);
        assert!((r.p99_ms.unwrap() - 664.450589).abs() < 1e-9);
        assert!((r.p999_ms.unwrap() - 674.979917).abs() < 1e-9);
        assert!((r.max_ms.unwrap() - 674.979917).abs() < 1e-9);
        assert!((r.achieved_rps - 104.0 / 12.784117963).abs() < 1e-9);
        let (pass, why) = r.judge(10.0);
        assert!(!pass, "{why}");
    }

    #[test]
    fn parses_a_run_with_no_responses() {
        let r = OhaRun::parse(include_str!("testdata/oha-refused.json"), "x".into()).unwrap();
        assert_eq!((r.ok, r.errors), (0, 20));
        assert_eq!(r.p50_ms, None);
        assert_eq!(r.max_ms, None);
        assert_eq!(
            r.error_kinds.get("Connection refused (os error 111)"),
            Some(&20)
        );
        assert!(!r.judge(1.0).0);
    }

    #[test]
    fn judge_each_criterion() {
        let mut r = mixed();
        r.non_2xx = 0;
        r.status.clear();
        r.p99_ms = Some(99.9);
        r.achieved_rps = 99.0;
        assert!(r.judge(100.0).0);
        r.achieved_rps = 98.9;
        assert!(!r.judge(100.0).0);
        r.achieved_rps = 100.0;
        r.p99_ms = Some(100.0);
        assert!(!r.judge(100.0).0);
        r.p99_ms = Some(5.0);
        r.errors = 1;
        assert!(!r.judge(100.0).0);
        r.errors = 0;
        r.non_2xx = 1;
        assert!(!r.judge(100.0).0);
        r.requests = 1000;
        assert!(r.judge(100.0).0, "one refusal in a thousand is shed traffic");
        r.non_2xx = 2;
        assert!(!r.judge(100.0).0);
        r.non_2xx = 0;
        r.errors = 2;
        assert!(!r.judge(100.0).0);
    }
}
