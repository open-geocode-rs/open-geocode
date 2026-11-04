//! `bench`: drives every measurement of the speed benchmark.
//!
//! Three geocoders serve the same country extract, each in turn, inside a
//! fixed slice of this machine. The harness owns the host layout, the query
//! sets, the load generator, the searches for ceilings and smallest boxes,
//! and the report. It reaches each engine only through the contract in
//! `bench/engines/<name>/`.

mod correctness;
mod engine;
mod geo;
mod host;
mod manifest;
mod oha;
mod queries;
mod report;
mod run;
mod search;
mod trivial;

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use engine::Engine;
use host::BoxSize;
use run::{Ctx, Direction, Profile, Record, guarded};

#[derive(Parser)]
#[command(
    name = "bench",
    about = "Speed benchmark harness for the geocoders under test"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Extract every address from the pbf and write the seeded query sets
    /// into bench/data/queries.
    Queries(QueriesArgs),
    /// Footprint, cold start, correctness and service time at 16 CPUs / 64 GiB.
    Measure(RunArgs),
    /// The highest rate each engine serves at 16 CPUs / 64 GiB.
    Ceiling(RunArgs),
    /// The smallest box that serves 100 req/s, forward and reverse. Runs once;
    /// the found box is confirmed three times instead.
    Floor(RunArgs),
    /// og's ceiling at 1, 2, 4, 8 and 16 CPUs with 64 GiB. Runs once.
    Scale(RunArgs),
    /// Everything: measure and ceiling for each repeat in rotated engine
    /// order, then floor, then og's scaling, then the report.
    All(RunArgs),
    /// A plumbing check of every code path, about five minutes for all three
    /// engines: 500 queries, 5 s steps, two rates per ceiling, two boxes per
    /// floor, og's scale at 1 and 16 CPUs.
    Smoke(RunArgs),
    /// Rebuild summary.json and report.md for a run.
    Report {
        #[arg(long)]
        out: PathBuf,
    },
    /// Drop files from the page cache (run under sudo by the harness).
    #[command(hide = true)]
    Evict { paths: Vec<PathBuf> },
    /// Serve 200 to everything (run in sut.slice by the harness).
    #[command(hide = true)]
    TrivialServer {
        #[arg(long)]
        port: u16,
    },
}

#[derive(Args)]
struct QueriesArgs {
    /// Defaults to ~/og-bench/data/pbf/netherlands-261101.osm.pbf.
    #[arg(long)]
    pbf: Option<PathBuf>,
    /// Where osmium writes its intermediate files. Defaults to
    /// ~/og-bench/data/queries-work.
    #[arg(long)]
    work: Option<PathBuf>,
}

#[derive(Args)]
struct RunArgs {
    /// og, photon, pelias, or all.
    #[arg(long, default_value = "all")]
    engine: String,
    #[arg(long, default_value_t = 3)]
    repeats: u32,
    /// Defaults to bench/results/<UTC date-time>.
    #[arg(long)]
    out: Option<PathBuf>,
    /// The load generator. Defaults to ~/og-bench/bin/oha.
    #[arg(long)]
    oha: Option<PathBuf>,
    /// full: as specified. quick: 1 repeat, 30 s steps with a 5 s lead-in,
    /// ceilings to within 10%, the floor's memory on a fixed ladder
    /// (16384 down to 256 MiB) with no confirmation runs. Ignored by smoke.
    #[arg(long, default_value = "full")]
    profile: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Measure,
    Ceiling,
    Floor,
    Scale,
    All,
    Smoke,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Queries(a) => make_queries(a),
        Cmd::Measure(a) => run_mode(Mode::Measure, a),
        Cmd::Ceiling(a) => run_mode(Mode::Ceiling, a),
        Cmd::Floor(a) => run_mode(Mode::Floor, a),
        Cmd::Scale(a) => run_mode(Mode::Scale, a),
        Cmd::All(a) => run_mode(Mode::All, a),
        Cmd::Smoke(a) => run_mode(Mode::Smoke, a),
        Cmd::Report { out } => {
            report::write(&out).map(|_| println!("{}", out.join("report.md").display()))
        }
        Cmd::Evict { paths } => host::evict_here(&paths).and_then(|ev| {
            println!("{}", serde_json::to_string(&ev)?);
            Ok(())
        }),
        Cmd::TrivialServer { port } => trivial::serve(port),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        host::cleanup();
        std::process::exit(1);
    }
}

fn bench_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
}

fn make_queries(a: QueriesArgs) -> Result<()> {
    let pbf = a
        .pbf
        .unwrap_or_else(|| home().join("og-bench/data/pbf/netherlands-261101.osm.pbf"));
    let work = a
        .work
        .unwrap_or_else(|| home().join("og-bench/data/queries-work"));
    let out = bench_dir().join("data/queries");
    let meta = queries::make(&pbf, &work, &out)?;
    println!(
        "{} eligible addresses; wrote {}",
        meta.eligible_addresses,
        out.display()
    );
    Ok(())
}

/// The engines asked for, in the fixed order every rotation starts from.
fn selected(engine: &str) -> Result<Vec<&'static str>> {
    if engine == "all" {
        return Ok(engine::NAMES.to_vec());
    }
    match engine::NAMES.iter().find(|n| **n == engine) {
        Some(n) => Ok(vec![*n]),
        None => bail!("unknown engine {engine:?}: expected og, photon, pelias or all"),
    }
}

/// The engine order for a repeat: og, photon, pelias, then photon, pelias,
/// og, then pelias, og, photon, so no engine always goes first.
fn rotation(engines: &[Engine], repeat: u32) -> Vec<&Engine> {
    let mut order: Vec<&str> = engine::NAMES.to_vec();
    let n = order.len();
    order.rotate_left(repeat as usize % n);
    order
        .iter()
        .filter_map(|n| engines.iter().find(|e| e.spec.name == *n))
        .collect()
}

fn run_mode(mode: Mode, a: RunArgs) -> Result<()> {
    let started = Instant::now();
    let names = selected(&a.engine)?;
    if mode == Mode::Scale && names != ["og"] {
        bail!("scale is og only: pass --engine og");
    }
    let dir = bench_dir();
    let engines = names
        .iter()
        .map(|n| Engine::load(&dir, n))
        .collect::<Result<Vec<_>>>()?;
    let (profile, repeats) = match (mode, a.profile.as_str()) {
        (Mode::Smoke, _) => (Profile::smoke(), 1),
        (_, "full") => (Profile::full(), a.repeats.max(1)),
        (_, "quick") => (Profile::quick(), 1),
        (_, other) => bail!("unknown profile {other:?}: expected full or quick"),
    };
    let out = a
        .out
        .unwrap_or_else(|| dir.join("results").join(utc_stamp()));
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let out = out.canonicalize()?;
    open_log(&out)?;
    log(&format!(
        "run {} ({} profile) into {}",
        names.join(","),
        profile.name,
        out.display()
    ));
    let oha = a.oha.unwrap_or_else(|| home().join("og-bench/bin/oha"));

    host::install_signal_handler()?;
    let _host = host::pin()?;
    let mut ctx = Ctx::new(dir, out.clone(), profile, oha)?;
    manifest::write(&ctx, &engines)?;

    let wants_ceiling = matches!(mode, Mode::Ceiling | Mode::Scale | Mode::All | Mode::Smoke);
    if wants_ceiling {
        trivial::headroom(&mut ctx)?;
    }
    let og = engines.iter().find(|e| e.spec.name == "og");
    match mode {
        Mode::Measure | Mode::Ceiling | Mode::All | Mode::Smoke => {
            let measure = mode != Mode::Ceiling;
            let ceiling = mode != Mode::Measure;
            for repeat in 0..repeats {
                for e in rotation(&engines, repeat) {
                    let stage = format!("repeat {repeat} at {}", BoxSize::BIG.label());
                    guarded(&ctx, &e.spec.name, &stage, || {
                        run::session(&ctx, e, repeat, BoxSize::BIG, measure, ceiling, "equal-box")
                    })?;
                }
            }
            if matches!(mode, Mode::All | Mode::Smoke) {
                floors(&ctx, &engines)?;
                if let Some(og) = og {
                    run::scale(&ctx, og)?;
                }
            }
        }
        Mode::Floor => floors(&ctx, &engines)?,
        Mode::Scale => run::scale(&ctx, og.expect("checked above"))?,
    }
    report::write(&out)?;
    log(&format!("report: {}", out.join("report.md").display()));
    let t = started.elapsed().as_secs();
    log(&format!("finished in {}m {:02}s wall time", t / 60, t % 60));
    Ok(())
}

fn floors(ctx: &Ctx, engines: &[Engine]) -> Result<()> {
    for e in engines {
        for d in Direction::BOTH {
            guarded(ctx, &e.spec.name, &format!("floor {}", d.name()), || {
                let rec = run::floor(ctx, e, d)?;
                ctx.record(&Record::Floor(rec))
            })?;
        }
    }
    Ok(())
}

static LOG: OnceLock<Mutex<File>> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

fn open_log(out: &Path) -> Result<()> {
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(out.join("bench.log"))?;
    let _ = LOG.set(Mutex::new(f));
    let _ = START.set(Instant::now());
    Ok(())
}

/// A progress line on stderr, also kept in the run's `bench.log`.
pub fn log(msg: &str) {
    let t = START.get_or_init(Instant::now).elapsed().as_secs();
    let line = format!("[{:02}:{:02}:{:02}] {msg}", t / 3600, t / 60 % 60, t % 60);
    eprintln!("{line}");
    if let Some(f) = LOG.get() {
        let mut f = f.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(f, "{line}");
    }
}

/// The current UTC time as `YYYYMMDDTHHMMSSZ`.
pub fn utc_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d) = civil_from_days((secs / 86_400) as i64);
    let s = secs % 86_400;
    format!(
        "{y:04}{mo:02}{d:02}T{:02}{:02}{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_760), (2026, 11, 3));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn engine_selection() {
        assert_eq!(selected("all").unwrap(), vec!["og", "photon", "pelias"]);
        assert_eq!(selected("photon").unwrap(), vec!["photon"]);
        assert!(selected("nominatim").is_err());
    }
}
