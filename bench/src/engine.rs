//! One geocoder under test, as its `bench/engines/<name>/` directory
//! describes it.
//!
//! The harness only ever touches an engine through that directory's contract:
//! `engine.toml` for ports, URL shapes and on-disk paths, `size.sh` to size it
//! for a box, and `compose.yml` to bring it up and down. Anything specific to
//! one engine lives there, never here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::geo::{coords_at, encode_component};
use crate::host::{self, BoxSize};

pub const NAMES: [&str; 3] = ["og", "photon", "pelias"];

/// `engine.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub name: String,
    pub port: u16,
    /// Path and query for a forward search; `{q}` is replaced by the
    /// URL-encoded query text.
    pub forward: String,
    /// Path and query for a reverse search; `{lat}` and `{lon}` are replaced.
    pub reverse: String,
    /// JSON pointer to the `[lon, lat]` of the first result.
    pub coords: String,
    /// Every on-disk path the server reads.
    pub data: Vec<PathBuf>,
    #[serde(default)]
    pub images: Vec<String>,
}

pub struct Engine {
    pub spec: Spec,
    pub dir: PathBuf,
    /// `engine.toml` as written, for the manifest.
    pub toml: String,
    /// Listening ports just before the last `up`.
    baseline: Mutex<Option<BTreeSet<u16>>>,
}

/// How long to wait for a port or container to go away.
const PORT_WAIT: Duration = Duration::from_secs(60);

fn wait_until(limit: Duration, mut ok: impl FnMut() -> bool) -> Result<()> {
    let t0 = Instant::now();
    while !ok() {
        if t0.elapsed() > limit {
            bail!("still waiting after {}s", limit.as_secs());
        }
        sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// `size.sh` exited 1: the box is below what the engine can be set up for.
/// A floor search counts it as a fail, recorded apart from measured fails.
#[derive(Debug)]
pub struct NotConfigurable(pub String);

/// An error no other box or engine would avoid; it stops the whole run.
#[derive(Debug)]
pub struct Abort(pub String);

impl std::fmt::Display for NotConfigurable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "not configurable: {}", self.0)
    }
}

impl std::fmt::Display for Abort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotConfigurable {}
impl std::error::Error for Abort {}

/// An image's identity and size, from `docker image inspect`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub reference: String,
    pub id: String,
    pub repo_digests: Vec<String>,
    pub bytes: u64,
}

impl Engine {
    pub fn load(bench_dir: &Path, name: &str) -> Result<Engine> {
        let dir = bench_dir.join("engines").join(name);
        let path = dir.join("engine.toml");
        let toml = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let spec: Spec =
            toml::from_str(&toml).with_context(|| format!("parsing {}", path.display()))?;
        ensure!(
            spec.name == name,
            "{} names itself {:?}",
            path.display(),
            spec.name
        );
        for f in ["compose.yml", "size.sh"] {
            ensure!(dir.join(f).is_file(), "{} has no {f}", dir.display());
        }
        ensure!(spec.forward.contains("{q}"), "{name}: forward has no {{q}}");
        ensure!(
            spec.reverse.contains("{lat}") && spec.reverse.contains("{lon}"),
            "{name}: reverse needs {{lat}} and {{lon}}"
        );
        Ok(Engine {
            spec,
            dir,
            toml,
            baseline: Mutex::new(None),
        })
    }

    pub fn forward_url(&self, q: &str) -> String {
        render(
            self.spec.port,
            &self.spec.forward,
            &[("{q}", &encode_component(q))],
        )
    }

    pub fn reverse_url(&self, lat: f64, lon: f64) -> String {
        render(
            self.spec.port,
            &self.spec.reverse,
            &[("{lat}", &lat.to_string()), ("{lon}", &lon.to_string())],
        )
    }

    /// Runs `size.sh CPUS MEM_MIB`, which writes the `.env` compose reads.
    ///
    /// Exit 1 means the engine's documented setup cannot be configured for
    /// this budget ([`NotConfigurable`]); exit 2 is a usage error, which no
    /// later box can fix ([`Abort`]).
    pub fn size(&self, b: BoxSize) -> Result<()> {
        let out = Command::new("bash")
            .arg("size.sh")
            .arg(b.cpus.to_string())
            .arg(b.mem_mib.to_string())
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .output()
            .context("running size.sh")?;
        let what = format!(
            "{}/size.sh {} {}: {}",
            self.spec.name,
            b.cpus,
            b.mem_mib,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        match out.status.code() {
            Some(0) => Ok(()),
            Some(1) => Err(NotConfigurable(what).into()),
            Some(2) => Err(Abort(format!("usage error from {what}")).into()),
            _ => bail!("{what} ({})", out.status),
        }
    }

    /// Brings the engine up after checking its port is free, and remembers
    /// which ports were listening beforehand so `down` can tell which ones
    /// the engine opened.
    pub fn up(&self) -> Result<()> {
        wait_until(PORT_WAIT, || {
            !host::listening_ports().contains(&self.spec.port)
        })
        .with_context(|| {
            format!(
                "port {} is still in use before starting {}",
                self.spec.port, self.spec.name
            )
        })?;
        *self.baseline.lock().unwrap_or_else(|e| e.into_inner()) = Some(host::listening_ports());
        host::register_engine(&self.dir);
        compose(&self.dir, &["up", "-d", "--quiet-pull"])
    }

    /// `compose down`, then waits until every container is gone and every
    /// port the engine opened is closed again. Engines share ports beyond
    /// their HTTP one (search nodes, transport ports), so the next engine
    /// must not start while any of them is still held.
    pub fn down(&self) -> Result<()> {
        compose_down(&self.dir)?;
        host::unregister_engine(&self.dir);
        let baseline = self
            .baseline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(before) = baseline {
            wait_until(PORT_WAIT, || self.containers().is_ok_and(|c| c.is_empty())).with_context(
                || {
                    format!(
                        "{}: containers still present after compose down",
                        self.spec.name
                    )
                },
            )?;
            let opened = || -> Vec<u16> {
                host::listening_ports()
                    .difference(&before)
                    .copied()
                    .collect()
            };
            if wait_until(PORT_WAIT, || opened().is_empty()).is_err() {
                bail!(
                    "{}: ports still listening after compose down: {:?}",
                    self.spec.name,
                    opened()
                );
            }
        }
        Ok(())
    }

    fn containers(&self) -> Result<Vec<String>> {
        if !self.dir.join(".env").exists() {
            return Ok(Vec::new());
        }
        let out = Command::new("docker")
            .args([
                "compose",
                "--env-file",
                ".env",
                "-f",
                "compose.yml",
                "ps",
                "-a",
                "-q",
            ])
            .current_dir(&self.dir)
            .output()?;
        ensure!(out.status.success(), "docker compose ps failed");
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect())
    }

    /// Polls `url` until it answers 200 with coordinates at the engine's
    /// pointer. Fails early if the slice saw an OOM kill or a container died,
    /// so a box too small to start in costs seconds, not the whole timeout.
    pub fn wait_ready(
        &self,
        http: &ureq::Agent,
        url: &str,
        t0: Instant,
        timeout: Duration,
    ) -> Result<()> {
        let mut last_check = Instant::now();
        loop {
            if let Ok((200, Some(_))) = probe(http, url, &self.spec.coords) {
                return Ok(());
            }
            if t0.elapsed() > timeout {
                bail!(
                    "{} gave no correct answer within {}s",
                    self.spec.name,
                    timeout.as_secs()
                );
            }
            if last_check.elapsed() > Duration::from_secs(2) {
                last_check = Instant::now();
                let ooms = host::sut_oom_kills();
                ensure!(
                    ooms == 0,
                    "{}: {ooms} OOM kill(s) while starting",
                    self.spec.name
                );
                let dead = self.dead_services()?;
                ensure!(
                    dead.is_empty(),
                    "{}: service(s) stopped while starting: {}",
                    self.spec.name,
                    dead.join(", ")
                );
            }
            sleep(Duration::from_millis(100));
        }
    }

    /// Services that exited with a non-zero code or are restart-looping. A
    /// clean exit 0 is allowed: some stacks run one-shot setup containers.
    fn dead_services(&self) -> Result<Vec<String>> {
        let out = Command::new("docker")
            .args([
                "compose",
                "--env-file",
                ".env",
                "-f",
                "compose.yml",
                "ps",
                "-a",
            ])
            .args(["--format", "{{.Service}} {{.State}} {{.ExitCode}}"])
            .current_dir(&self.dir)
            .output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                let bad = match f.as_slice() {
                    [_, "restarting", ..] | [_, "dead", ..] => true,
                    [_, "exited", code] => *code != "0",
                    _ => false,
                };
                bad.then(|| l.to_string())
            })
            .collect())
    }

    /// Bytes on disk under every `data` path.
    pub fn data_bytes(&self) -> Result<u64> {
        let out = Command::new("sudo")
            .args(["-n", "du", "-s", "-b", "-c"])
            .args(&self.spec.data)
            .stdin(Stdio::null())
            .output()?;
        ensure!(
            out.status.success(),
            "du failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        let text = String::from_utf8_lossy(&out.stdout);
        let total = text
            .lines()
            .last()
            .and_then(|l| l.split_whitespace().next());
        total
            .and_then(|t| t.parse().ok())
            .context("parsing du output")
    }

    pub fn images(&self) -> Result<Vec<Image>> {
        self.spec.images.iter().map(|r| inspect_image(r)).collect()
    }
}

fn render(port: u16, template: &str, vars: &[(&str, &str)]) -> String {
    let mut path = template.to_string();
    for (k, v) in vars {
        path = path.replace(k, v);
    }
    format!("http://127.0.0.1:{port}{path}")
}

/// One request: the status and the coordinates at `pointer`, if any.
pub fn probe(http: &ureq::Agent, url: &str, pointer: &str) -> Result<(u16, Option<(f64, f64)>)> {
    let mut resp = http.get(url).call()?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string()?;
    let coords = serde_json::from_str(&body)
        .ok()
        .and_then(|v| coords_at(&v, pointer));
    Ok((status, coords))
}

fn compose(dir: &Path, args: &[&str]) -> Result<()> {
    let st = Command::new("docker")
        .args(["compose", "--env-file", ".env", "-f", "compose.yml"])
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .status()
        .context("running docker compose")?;
    ensure!(
        st.success(),
        "docker compose {} in {} failed",
        args.join(" "),
        dir.display()
    );
    Ok(())
}

/// `compose down`, if the engine was ever sized: without `.env` it cannot
/// have been brought up through the contract.
pub fn compose_down(dir: &Path) -> Result<()> {
    if !dir.join(".env").exists() {
        return Ok(());
    }
    compose(dir, &["down"])
}

fn inspect_image(reference: &str) -> Result<Image> {
    let out = Command::new("docker")
        .args([
            "image",
            "inspect",
            "--format",
            "{{.Id}}|{{json .RepoDigests}}|{{.Size}}",
            reference,
        ])
        .output()?;
    ensure!(
        out.status.success(),
        "docker image inspect {reference}: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.trim().splitn(3, '|');
    let id = parts.next().unwrap_or_default().to_string();
    let repo_digests = serde_json::from_str(parts.next().unwrap_or("[]")).unwrap_or_default();
    let bytes = parts
        .next()
        .and_then(|s| s.parse().ok())
        .context("image size")?;
    Ok(Image {
        reference: reference.to_string(),
        id,
        repo_digests,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn og() -> Engine {
        let toml = r#"
            name = "og"
            port = 7100
            forward = "/geocode?q={q}&limit=1"
            reverse = "/reverse?lat={lat}&lon={lon}"
            coords = "/features/0/geometry/coordinates"
            data = ["/srv/og"]
            images = ["og@sha256:abc"]
        "#;
        Engine {
            spec: toml::from_str(toml).unwrap(),
            dir: PathBuf::new(),
            toml: toml.into(),
            baseline: Mutex::new(None),
        }
    }

    #[test]
    fn renders_forward_and_reverse_urls() {
        let e = og();
        assert_eq!(
            e.forward_url("Damrak 1, Amsterdam"),
            "http://127.0.0.1:7100/geocode?q=Damrak%201%2C%20Amsterdam&limit=1"
        );
        assert_eq!(
            e.reverse_url(52.3731, 4.8926),
            "http://127.0.0.1:7100/reverse?lat=52.3731&lon=4.8926"
        );
        assert_eq!(
            e.reverse_url(52.0, -0.1234567),
            "http://127.0.0.1:7100/reverse?lat=52&lon=-0.1234567"
        );
    }
}
