//! The host layout every measurement runs on.
//!
//! The VM also serves production, so the benchmark carves it up instead of
//! owning it: cores 0-7 keep everything that was already running, cores 8-15
//! run the load generator, and cores 16-31 belong to the engine under test in
//! `sut.slice`. The engine's memory is enforced through `memory.max`, which
//! counts page cache, so the memory figure for a box is a limit the engine
//! lived within rather than an RSS reading.
//!
//! Every change to a slice we did not create is `--runtime` and is undone on
//! exit, on error, and on Ctrl-C, SIGTERM or SIGHUP, through one cleanup
//! routine that also brings down any engine still up.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

pub const OTHER_CPUS: &str = "0-7";
pub const LOAD_CPUS: &str = "8-15";
pub const SUT_FIRST_CPU: u32 = 16;
pub const SUT_MAX_CPUS: u32 = 16;
const PINNED_UNITS: [&str; 3] = ["system.slice", "user.slice", "init.scope"];
const SUT_SLICE: &str = "sut.slice";
const SUT_CGROUP: &str = "/sys/fs/cgroup/sut.slice";

/// The CPUs and memory the engine under test gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BoxSize {
    pub cpus: u32,
    pub mem_mib: u64,
}

impl BoxSize {
    pub const BIG: BoxSize = BoxSize {
        cpus: 16,
        mem_mib: 64 * 1024,
    };

    pub fn cpu_list(&self) -> String {
        if self.cpus == 1 {
            SUT_FIRST_CPU.to_string()
        } else {
            format!("{}-{}", SUT_FIRST_CPU, SUT_FIRST_CPU + self.cpus - 1)
        }
    }

    pub fn label(&self) -> String {
        format!("{}cpu-{}", self.cpus, mib_label(self.mem_mib))
    }
}

pub fn mib_label(mib: u64) -> String {
    if mib.is_multiple_of(1024) {
        format!("{}GiB", mib / 1024)
    } else {
        format!("{mib}MiB")
    }
}

/// What has to be undone if the process stops early.
struct Cleanup {
    pinned: bool,
    engines: Vec<PathBuf>,
    children: Vec<u32>,
}

static CLEANUP: Mutex<Cleanup> = Mutex::new(Cleanup {
    pinned: false,
    engines: Vec::new(),
    children: Vec::new(),
});

/// Undoes everything registered: stops children, brings engines down,
/// restores the pinned slices. Safe to call more than once.
pub fn cleanup() {
    let (pinned, engines, children) = {
        let mut c = CLEANUP.lock().unwrap_or_else(|e| e.into_inner());
        let out = (
            c.pinned,
            std::mem::take(&mut c.engines),
            std::mem::take(&mut c.children),
        );
        c.pinned = false;
        out
    };
    for pid in children {
        // The child is `sudo`, which relays SIGTERM to the command it runs.
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
    for dir in engines {
        if let Err(e) = crate::engine::compose_down(&dir) {
            eprintln!("cleanup: compose down in {}: {e:#}", dir.display());
        }
    }
    if pinned {
        for unit in PINNED_UNITS {
            if let Err(e) = sudo(&[
                "systemctl",
                "set-property",
                "--runtime",
                unit,
                "AllowedCPUs=",
            ]) {
                eprintln!("cleanup: restoring {unit}: {e:#}");
            }
        }
        eprintln!(
            "host restored: {} back on all CPUs",
            PINNED_UNITS.join(", ")
        );
    }
}

/// Installs the handler that restores the host on Ctrl-C, SIGTERM and SIGHUP.
pub fn install_signal_handler() -> Result<()> {
    ctrlc::set_handler(|| {
        eprintln!("\ninterrupted: restoring the host");
        cleanup();
        std::process::exit(130);
    })
    .context("installing the signal handler")
}

pub fn register_engine(dir: &Path) {
    let mut c = CLEANUP.lock().unwrap_or_else(|e| e.into_inner());
    if !c.engines.iter().any(|d| d == dir) {
        c.engines.push(dir.to_path_buf());
    }
}

pub fn unregister_engine(dir: &Path) {
    CLEANUP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .engines
        .retain(|d| d != dir);
}

pub fn register_child(pid: u32) {
    CLEANUP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .children
        .push(pid);
}

pub fn unregister_child(pid: u32) {
    CLEANUP
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .children
        .retain(|p| *p != pid);
}

/// Holds the host layout for as long as it lives; dropping it restores the
/// host.
pub struct HostGuard(());

impl Drop for HostGuard {
    fn drop(&mut self) {
        cleanup();
    }
}

/// Moves everything already running onto cores 0-7.
pub fn pin() -> Result<HostGuard> {
    // Registered before the first change so a failure halfway is undone too.
    CLEANUP.lock().unwrap_or_else(|e| e.into_inner()).pinned = true;
    let guard = HostGuard(());
    for unit in PINNED_UNITS {
        sudo(&[
            "systemctl",
            "set-property",
            "--runtime",
            unit,
            &format!("AllowedCPUs={OTHER_CPUS}"),
        ])?;
    }
    Ok(guard)
}

/// Runs a command under `sudo -n`, failing instead of prompting.
pub fn sudo(args: &[&str]) -> Result<()> {
    let out = Command::new("sudo")
        .arg("-n")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running sudo {}", args.join(" ")))?;
    ensure!(
        out.status.success(),
        "sudo {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Resizes `sut.slice` to `b`. The slice is stopped and started again so its
/// `memory.peak` starts from zero for the new engine run; it must be empty
/// when this is called.
pub fn set_sut(b: BoxSize) -> Result<()> {
    ensure!(
        (1..=SUT_MAX_CPUS).contains(&b.cpus),
        "a box has 1-16 CPUs, not {}",
        b.cpus
    );
    if sut_active() {
        sudo(&["systemctl", "stop", SUT_SLICE])?;
    }
    let mem = format!("{}M", b.mem_mib);
    sudo(&[
        "systemctl",
        "set-property",
        "--runtime",
        SUT_SLICE,
        &format!("AllowedCPUs={}", b.cpu_list()),
        &format!("MemoryMax={mem}"),
        "MemorySwapMax=0",
        &format!("MemoryLow={mem}"),
        "IOWeight=1000",
    ])?;
    sudo(&["systemctl", "start", SUT_SLICE])?;
    let cpus = fs::read_to_string(format!("{SUT_CGROUP}/cpuset.cpus")).unwrap_or_default();
    ensure!(
        cpus.trim() == b.cpu_list(),
        "sut.slice cpuset is {:?}, expected {}",
        cpus.trim(),
        b.cpu_list()
    );
    Ok(())
}

fn sut_active() -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", SUT_SLICE])
        .status()
        .is_ok_and(|s| s.success())
}

/// Memory use of everything in `sut.slice`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SutMemory {
    /// The high-water mark of the slice since the engine started, page cache
    /// included.
    pub peak_bytes: u64,
    pub anon_bytes: u64,
    pub file_bytes: u64,
    /// The whole of `memory.stat` at the time of reading.
    pub stat: BTreeMap<String, u64>,
}

pub fn sut_memory() -> Result<SutMemory> {
    let peak = read_u64(&format!("{SUT_CGROUP}/memory.peak"))?;
    let stat = read_keyed(&format!("{SUT_CGROUP}/memory.stat"))?;
    Ok(SutMemory {
        peak_bytes: peak,
        anon_bytes: stat.get("anon").copied().unwrap_or(0),
        file_bytes: stat.get("file").copied().unwrap_or(0),
        stat,
    })
}

/// How many processes the kernel has OOM-killed in `sut.slice` since it was
/// last started.
pub fn sut_oom_kills() -> u64 {
    read_keyed(&format!("{SUT_CGROUP}/memory.events"))
        .ok()
        .and_then(|m| m.get("oom_kill").copied())
        .unwrap_or(0)
}

fn read_u64(path: &str) -> Result<u64> {
    let s = fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    s.trim().parse().with_context(|| format!("parsing {path}"))
}

fn read_keyed(path: &str) -> Result<BTreeMap<String, u64>> {
    let s = fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    Ok(s.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(' ')?;
            Some((k.to_string(), v.trim().parse().ok()?))
        })
        .collect())
}

/// A command that runs `program` as us inside a transient scope. sudo is only
/// needed to create the scope; `--uid/--gid` drop back to our own user.
fn scoped(slice: &str, cpus: Option<&str>, program: &OsStr) -> Command {
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let mut c = Command::new("sudo");
    c.args(["-n", "systemd-run", "--scope", "--quiet", "--collect"])
        .arg(format!("--slice={slice}"));
    if let Some(cpus) = cpus {
        c.arg("-p").arg(format!("AllowedCPUs={cpus}"));
    }
    c.arg(format!("--uid={uid}"))
        .arg(format!("--gid={gid}"))
        .arg("--")
        .arg(program);
    c
}

/// A command for the load generator, on cores 8-15.
pub fn load_command(program: &Path) -> Command {
    scoped("load.slice", Some(LOAD_CPUS), program.as_os_str())
}

/// A command for a process that stands in for an engine, inside `sut.slice`.
pub fn sut_command(program: &Path) -> Command {
    scoped(SUT_SLICE, None, program.as_os_str())
}

/// Files and bytes a cache eviction went over.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Evicted {
    pub files: u64,
    pub bytes: u64,
}

/// Drops `paths` from the page cache, so the engine starts cold. Runs this
/// binary's `evict` subcommand under sudo because engine data is often
/// root-owned.
pub fn evict(paths: &[PathBuf]) -> Result<Evicted> {
    let exe = std::env::current_exe()?;
    let out = Command::new("sudo")
        .arg("-n")
        .arg(exe)
        .arg("evict")
        .args(paths)
        .stdin(Stdio::null())
        .output()
        .context("running the evict helper")?;
    ensure!(
        out.status.success(),
        "evict failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    serde_json::from_slice(&out.stdout).context("parsing the evict helper's output")
}

/// The body of the `evict` subcommand: `posix_fadvise(DONTNEED)` over every
/// regular file under `paths`.
///
/// Only these files are dropped. A global `drop_caches` would also empty the
/// cache of the production services sharing the machine.
pub fn evict_here(paths: &[PathBuf]) -> Result<Evicted> {
    let mut ev = Evicted::default();
    for p in paths {
        let meta = fs::metadata(p).with_context(|| format!("data path {}", p.display()))?;
        evict_walk(p, &meta, &mut ev)?;
    }
    Ok(ev)
}

fn evict_walk(p: &Path, meta: &fs::Metadata, ev: &mut Evicted) -> Result<()> {
    if meta.is_dir() {
        for entry in fs::read_dir(p).with_context(|| format!("listing {}", p.display()))? {
            let entry = entry?;
            // Symlinks inside a data dir are not followed: whatever they point
            // at is either listed in `data` itself or not the engine's.
            let m = fs::symlink_metadata(entry.path())?;
            if m.is_dir() || m.is_file() {
                evict_walk(&entry.path(), &m, ev)?;
            }
        }
    } else if meta.is_file() {
        let f = fs::File::open(p).with_context(|| format!("opening {}", p.display()))?;
        let rc = unsafe { libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        if rc != 0 {
            bail!("posix_fadvise on {}: errno {rc}", p.display());
        }
        ev.files += 1;
        ev.bytes += meta.size();
    }
    Ok(())
}

/// Every TCP port in LISTEN state, IPv4 and IPv6, on any address.
pub fn listening_ports() -> BTreeSet<u16> {
    ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|p| fs::read_to_string(p).ok())
        .flat_map(|t| parse_listening(&t))
        .collect()
}

fn parse_listening(table: &str) -> Vec<u16> {
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            // f[1] is the local "ADDR:PORT" in hex, f[3] the state; 0A is LISTEN.
            if f.get(3) != Some(&"0A") {
                return None;
            }
            let port = f.get(1)?.rsplit(':').next()?;
            u16::from_str_radix(port, 16).ok()
        })
        .collect()
}

/// One reading of the counters that say whether the machine was quiet.
#[derive(Debug, Clone)]
pub struct Sample {
    at: Instant,
    /// `(steal, total)` jiffies per CPU, indexed by CPU number.
    cpus: Vec<(u64, u64)>,
    /// Cumulative stall microseconds: cpu, io, memory; each `(some, full)`.
    psi: [(u64, u64); 3],
}

/// What happened on the machine during a timed step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Noise {
    pub wall_s: f64,
    /// Steal time as a share of the SUT cores' total time.
    pub steal_frac: f64,
    /// Stall time in microseconds over the step, machine-wide.
    pub psi_cpu_some_us: u64,
    pub psi_cpu_full_us: u64,
    pub psi_io_some_us: u64,
    pub psi_io_full_us: u64,
    pub psi_memory_some_us: u64,
    pub psi_memory_full_us: u64,
}

pub fn sample() -> Result<Sample> {
    let stat = fs::read_to_string("/proc/stat")?;
    let psi = ["cpu", "io", "memory"].map(|r| read_psi(r).unwrap_or((0, 0)));
    Ok(Sample {
        at: Instant::now(),
        cpus: parse_proc_stat(&stat),
        psi,
    })
}

/// Per-CPU `(steal, total)` from `/proc/stat`. Total is user through steal;
/// guest time is already inside user, so it is not added again.
fn parse_proc_stat(stat: &str) -> Vec<(u64, u64)> {
    let mut cpus = Vec::new();
    for line in stat.lines() {
        let Some(rest) = line.strip_prefix("cpu") else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let Some(Ok(n)) = fields.next().map(str::parse::<usize>) else {
            continue;
        };
        let v: Vec<u64> = fields.take(8).filter_map(|f| f.parse().ok()).collect();
        if v.len() < 8 {
            continue;
        }
        if cpus.len() <= n {
            cpus.resize(n + 1, (0, 0));
        }
        cpus[n] = (v[7], v.iter().sum());
    }
    cpus
}

fn read_psi(resource: &str) -> Result<(u64, u64)> {
    let s = fs::read_to_string(format!("/proc/pressure/{resource}"))?;
    Ok(parse_psi(&s))
}

fn parse_psi(s: &str) -> (u64, u64) {
    let total = |kind: &str| {
        s.lines()
            .find(|l| l.starts_with(kind))
            .and_then(|l| l.split_whitespace().find_map(|f| f.strip_prefix("total=")))
            .and_then(|t| t.parse().ok())
            .unwrap_or(0)
    };
    (total("some"), total("full"))
}

/// The noise between two samples, with steal counted over the SUT cores only.
pub fn noise(before: &Sample, after: &Sample, sut: BoxSize) -> Noise {
    let (mut steal, mut total) = (0u64, 0u64);
    for cpu in SUT_FIRST_CPU..SUT_FIRST_CPU + sut.cpus {
        let i = cpu as usize;
        if let (Some(a), Some(b)) = (before.cpus.get(i), after.cpus.get(i)) {
            steal += b.0.saturating_sub(a.0);
            total += b.1.saturating_sub(a.1);
        }
    }
    let d = |i: usize, full: bool| {
        let (a, b) = (before.psi[i], after.psi[i]);
        if full {
            b.1.saturating_sub(a.1)
        } else {
            b.0.saturating_sub(a.0)
        }
    };
    Noise {
        wall_s: after.at.duration_since(before.at).as_secs_f64(),
        steal_frac: if total == 0 {
            0.0
        } else {
            steal as f64 / total as f64
        },
        psi_cpu_some_us: d(0, false),
        psi_cpu_full_us: d(0, true),
        psi_io_some_us: d(1, false),
        psi_io_full_us: d(1, true),
        psi_memory_some_us: d(2, false),
        psi_memory_full_us: d(2, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_arguments() {
        assert_eq!(BoxSize::BIG.cpu_list(), "16-31");
        assert_eq!(
            BoxSize {
                cpus: 1,
                mem_mib: 256
            }
            .cpu_list(),
            "16"
        );
        assert_eq!(
            BoxSize {
                cpus: 4,
                mem_mib: 256
            }
            .cpu_list(),
            "16-19"
        );
        assert_eq!(BoxSize::BIG.label(), "16cpu-64GiB");
        assert_eq!(
            BoxSize {
                cpus: 2,
                mem_mib: 3000
            }
            .label(),
            "2cpu-3000MiB"
        );
    }

    #[test]
    fn steal_over_sut_cores_only() {
        let stat = |steal16: u64, steal0: u64, t: u64| {
            let mut s = String::from("cpu  1 2 3 4 5 6 7 8 0 0\n");
            for n in 0..32 {
                let steal = match n {
                    16 => steal16,
                    0 => steal0,
                    _ => 0,
                };
                s.push_str(&format!("cpu{n} {t} 0 0 {t} 0 0 0 {steal} 0 0\n"));
            }
            s.push_str("intr 1 2 3\n");
            s
        };
        let a = Sample {
            at: Instant::now(),
            cpus: parse_proc_stat(&stat(0, 0, 100)),
            psi: [(0, 0); 3],
        };
        let b = Sample {
            at: Instant::now(),
            cpus: parse_proc_stat(&stat(10, 500, 150)),
            psi: [(10, 5), (0, 0), (7, 0)],
        };
        let one = BoxSize {
            cpus: 1,
            mem_mib: 1024,
        };
        let n = noise(&a, &b, one);
        // cpu16: steal +10, total +100 (user, idle) +10 (steal).
        assert!((n.steal_frac - 10.0 / 110.0).abs() < 1e-12);
        assert_eq!(
            (n.psi_cpu_some_us, n.psi_cpu_full_us, n.psi_memory_some_us),
            (10, 5, 7)
        );
        // Spread over 16 cores, the same steal is under 1%.
        let n = noise(&a, &b, BoxSize::BIG);
        assert!(n.steal_frac < 0.01);
    }

    #[test]
    fn listening_ports_from_proc() {
        let t = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1 1 0 100 0 0 10 0
   1: 0100007F:1C24 0100007F:D3A2 01 00000000:00000000 00:00000000 00000000  1000        0 2 1 0 20 4 30 10 -1
   2: 00000000000000000000000001000000:2454 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 3 1 0 100 0 0 10 0
";
        assert_eq!(parse_listening(t), vec![8080, 9300]);
    }

    #[test]
    fn psi_totals() {
        let s = "some avg10=0.52 avg60=0.87 avg300=0.36 total=744763219\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=12\n";
        assert_eq!(parse_psi(s), (744763219, 12));
        assert_eq!(parse_psi("some avg10=0.00 total=5\n"), (5, 0));
    }
}
