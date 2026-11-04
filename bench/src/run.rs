//! The measurements: one engine, one box, one session at a time.
//!
//! A session is an engine brought up cold in a box and taken down again.
//! Every timed step inside it is bracketed by noise samples, and a step that
//! lost more than 1% of the engine cores' time to steal is run again, up to
//! twice, with the reason kept.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::correctness::{self, Score};
use crate::engine::{Abort, Engine, Image, NotConfigurable};
use crate::host::{self, BoxSize, Evicted, Noise, SutMemory, mib_label};
use crate::log;
use crate::oha::{Oha, OhaRun, Shape};
use crate::queries::{Forward, Reverse};
use crate::search::{self, CeilingSearch, MemorySearch};

/// Steal above this share of the SUT cores' time invalidates a step.
pub const STEAL_LIMIT: f64 = 0.01;
pub const MAX_RETRIES: usize = 2;
/// The job the smallest box must do.
pub const JOB_RPS: f64 = 100.0;
/// The memory bisection's lower bound.
pub const FLOOR_MIN_MIB: u64 = 256;
/// CPU counts for og's scaling curve.
pub const SCALE_CPUS: [u32; 5] = [1, 2, 4, 8, 16];
/// The smoke profile's scale points.
pub const SMOKE_SCALE_CPUS: [u32; 2] = [1, 16];
/// The smoke profile's floor boxes: one every engine should pass, one below
/// what most can be configured for or serve in.
pub const SMOKE_FLOOR_BOXES: [BoxSize; 2] = [
    BoxSize {
        cpus: 4,
        mem_mib: 16384,
    },
    BoxSize {
        cpus: 1,
        mem_mib: 512,
    },
];

/// How long and how large each step is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    /// Queries per direction for correctness, warm-up and service time.
    pub queries: usize,
    pub step: Duration,
    pub lead_in: Duration,
    pub confirm_runs: u32,
    pub start_timeout: Duration,
    /// A plumbing check: every code path once, fixed rates and boxes instead
    /// of searches.
    #[serde(default)]
    pub smoke: bool,
    /// How close the ceiling bisection gets, as a share of the last pass.
    #[serde(default = "default_tolerance")]
    pub ceiling_tolerance: f64,
    /// A fixed memory ladder for the floor, walked down to the first fail
    /// with no confirmation runs, in place of the bisection.
    #[serde(default)]
    pub memory_ladder: Option<Vec<u64>>,
}

fn default_tolerance() -> f64 {
    search::CEILING_TOLERANCE
}

impl Profile {
    pub fn full() -> Profile {
        Profile {
            name: "full".into(),
            queries: 10_000,
            step: Duration::from_secs(60),
            lead_in: Duration::from_secs(10),
            confirm_runs: 3,
            start_timeout: Duration::from_secs(1800),
            smoke: false,
            ceiling_tolerance: search::CEILING_TOLERANCE,
            memory_ladder: None,
        }
    }

    /// Real numbers in hours rather than a day: one repeat, 30 s steps after
    /// a 5 s lead-in, the full query sets, ceilings to within 10%, and the
    /// floor's memory on a fixed ladder with no confirmation runs.
    pub fn quick() -> Profile {
        Profile {
            name: "quick".into(),
            queries: 10_000,
            step: Duration::from_secs(30),
            lead_in: Duration::from_secs(5),
            confirm_runs: 0,
            start_timeout: Duration::from_secs(1800),
            smoke: false,
            ceiling_tolerance: 0.10,
            memory_ladder: Some(vec![16384, 8192, 4096, 2048, 1024, 512, 256]),
        }
    }

    /// Every code path once per engine, in about five minutes for all three:
    /// 500 queries, 5 s load steps, two rates per ceiling, two boxes per
    /// floor, og's scale at 1 and 16 CPUs only.
    pub fn smoke() -> Profile {
        Profile {
            name: "smoke".into(),
            queries: 500,
            step: Duration::from_secs(5),
            lead_in: Duration::from_secs(1),
            confirm_runs: 0,
            start_timeout: Duration::from_secs(600),
            smoke: true,
            ceiling_tolerance: search::CEILING_TOLERANCE,
            memory_ladder: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Forward,
    Reverse,
}

impl Direction {
    pub const BOTH: [Direction; 2] = [Direction::Forward, Direction::Reverse];

    pub fn name(self) -> &'static str {
        match self {
            Direction::Forward => "forward",
            Direction::Reverse => "reverse",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerDirection<T> {
    pub forward: T,
    pub reverse: T,
}

impl<T> PerDirection<T> {
    pub fn get(&self, d: Direction) -> &T {
        match d {
            Direction::Forward => &self.forward,
            Direction::Reverse => &self.reverse,
        }
    }
}

/// A step's result with the noise it ran under.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Timed<T> {
    pub value: T,
    pub noise: Noise,
    /// False if steal stayed over the limit through every retry.
    pub valid: bool,
    /// Why earlier attempts were thrown away.
    pub retries: Vec<String>,
}

/// Runs `prepare` then `run`, sampling noise around `run` only, and repeats
/// both while steal on the SUT cores is over the limit.
pub fn timed<T>(
    sut: BoxSize,
    label: &str,
    mut prepare: impl FnMut() -> Result<()>,
    mut run: impl FnMut() -> Result<T>,
) -> Result<Timed<T>> {
    let mut retries = Vec::new();
    loop {
        prepare()?;
        let before = host::sample()?;
        let value = run()?;
        let after = host::sample()?;
        let noise = host::noise(&before, &after, sut);
        if noise.steal_frac <= STEAL_LIMIT {
            return Ok(Timed {
                value,
                noise,
                valid: true,
                retries,
            });
        }
        let reason = format!(
            "{label}: steal was {:.2}% of SUT core time",
            noise.steal_frac * 100.0
        );
        log(&format!("invalid step, {reason}"));
        retries.push(reason);
        if retries.len() > MAX_RETRIES {
            return Ok(Timed {
                value,
                noise,
                valid: false,
                retries,
            });
        }
    }
}

/// Everything one run writes down, one JSON line each in `results.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Record {
    Headroom(HeadroomRecord),
    Measure(Box<MeasureRecord>),
    Ceiling(CeilingRecord),
    Floor(FloorRecord),
    Error(ErrorRecord),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeadroomRecord {
    /// The highest rate the load generator sustained against a trivial
    /// endpoint on the SUT cores.
    pub max_rps: Option<f64>,
    /// A third of that: no ceiling above it is reported.
    pub limit_rps: Option<f64>,
    pub search: CeilingSearch,
    pub steps: Vec<RateStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeasureRecord {
    pub engine: String,
    pub repeat: u32,
    pub boxed: BoxSize,
    pub cold_start_s: Timed<f64>,
    pub evicted: Evicted,
    pub correctness: PerDirection<Score>,
    pub service: PerDirection<Timed<OhaRun>>,
    pub memory: SutMemory,
    pub disk: Disk,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Disk {
    pub data_bytes: u64,
    pub images: Vec<Image>,
    pub image_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CeilingRecord {
    /// "equal-box" for the 16 CPU / 64 GiB comparison, "scale" for og's curve.
    pub context: String,
    pub engine: String,
    pub repeat: u32,
    pub boxed: BoxSize,
    pub direction: Direction,
    pub p50_ms: f64,
    pub start_rps: f64,
    pub load_limit_rps: Option<f64>,
    /// The highest passing rate; `None` if nothing passed or the load
    /// generator may have been the limit.
    pub ceiling_rps: Option<f64>,
    pub load_generator_bound: bool,
    pub search: CeilingSearch,
    pub steps: Vec<RateStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateStep {
    pub rate: f64,
    pub pass: bool,
    pub why: String,
    pub lead_in_raw: Option<String>,
    pub run: Timed<OhaRun>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FloorRecord {
    pub engine: String,
    pub direction: Direction,
    pub job_rps: f64,
    pub cpu_steps: Vec<JobStep>,
    pub memory: Option<MemorySearch>,
    pub memory_steps: Vec<JobStep>,
    pub found: Option<BoxSize>,
    pub next_smaller: Option<BoxSize>,
    pub confirm_found: Vec<JobStep>,
    pub confirm_next_smaller: Vec<JobStep>,
    pub verdict: String,
}

/// One fresh session asked to serve the job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStep {
    pub boxed: BoxSize,
    pub pass: bool,
    /// `size.sh` refused the box; a fail, but not a measured one.
    #[serde(default)]
    pub not_configurable: bool,
    pub why: String,
    pub cold_start_s: Option<f64>,
    pub step: Option<RateStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorRecord {
    pub engine: String,
    pub stage: String,
    pub message: String,
}

pub struct Ctx {
    pub bench_dir: PathBuf,
    pub out: PathBuf,
    pub profile: Profile,
    pub forward: Vec<Forward>,
    pub reverse: Vec<Reverse>,
    pub oha: Oha,
    pub http: ureq::Agent,
    pub load_limit: Option<f64>,
    results: Mutex<File>,
}

/// One engine's URL files for oha and the same URLs for the harness.
pub struct Urls {
    pub forward_file: PathBuf,
    pub reverse_file: PathBuf,
    pub forward: Vec<String>,
    pub reverse: Vec<String>,
}

impl Urls {
    pub fn file(&self, d: Direction) -> &Path {
        match d {
            Direction::Forward => &self.forward_file,
            Direction::Reverse => &self.reverse_file,
        }
    }
}

impl Ctx {
    pub fn new(
        bench_dir: PathBuf,
        out: PathBuf,
        profile: Profile,
        oha_bin: PathBuf,
    ) -> Result<Ctx> {
        let (forward, reverse) = crate::queries::load(&bench_dir.join("data/queries"))?;
        let results = OpenOptions::new()
            .create(true)
            .append(true)
            .open(out.join("results.jsonl"))?;
        let http: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(10)))
            .proxy(None)
            .build()
            .into();
        Ok(Ctx {
            oha: Oha::new(oha_bin, out.clone()),
            bench_dir,
            out,
            profile,
            forward,
            reverse,
            http,
            load_limit: None,
            results: Mutex::new(results),
        })
    }

    pub fn record(&self, r: &Record) -> Result<()> {
        let mut f = self.results.lock().unwrap_or_else(|e| e.into_inner());
        writeln!(f, "{}", serde_json::to_string(r)?)?;
        f.flush()?;
        Ok(())
    }

    /// Renders the engine's URLs for the first `profile.queries` queries and
    /// writes them to `urls/<engine>-{forward,reverse}.txt`.
    pub fn urls(&self, e: &Engine) -> Result<Urls> {
        let n = self.profile.queries;
        let forward: Vec<String> = self
            .forward
            .iter()
            .take(n)
            .map(|q| e.forward_url(&q.q))
            .collect();
        let reverse: Vec<String> = self
            .reverse
            .iter()
            .take(n)
            .map(|q| e.reverse_url(q.lat, q.lon))
            .collect();
        let dir = self.out.join("urls");
        std::fs::create_dir_all(&dir)?;
        let forward_file = dir.join(format!("{}-forward.txt", e.spec.name));
        let reverse_file = dir.join(format!("{}-reverse.txt", e.spec.name));
        std::fs::write(&forward_file, forward.join("\n") + "\n")?;
        std::fs::write(&reverse_file, reverse.join("\n") + "\n")?;
        Ok(Urls {
            forward_file,
            reverse_file,
            forward,
            reverse,
        })
    }

    fn truths(&self, d: Direction) -> Vec<(f64, f64)> {
        let n = self.profile.queries;
        match d {
            Direction::Forward => self
                .forward
                .iter()
                .take(n)
                .map(|q| (q.lon, q.lat))
                .collect(),
            Direction::Reverse => self
                .reverse
                .iter()
                .take(n)
                .map(|q| (q.truth_lon, q.truth_lat))
                .collect(),
        }
    }
}

/// An engine up in a box. Dropping it brings the engine down.
pub struct Session<'a> {
    engine: &'a Engine,
    pub cold_start: Timed<f64>,
    pub evicted: Evicted,
}

impl<'a> Session<'a> {
    /// Size the slice, evict the engine's data from the page cache, size the
    /// engine, bring it up, and time until its first correct answer.
    pub fn start(
        ctx: &Ctx,
        engine: &'a Engine,
        b: BoxSize,
        first_url: &str,
    ) -> Result<Session<'a>> {
        log(&format!(
            "{}: starting cold in {}",
            engine.spec.name,
            b.label()
        ));
        let mut evicted = Evicted::default();
        let started = timed(
            b,
            &format!("{} cold start", engine.spec.name),
            || {
                engine.down()?;
                host::set_sut(b)?;
                evicted = host::evict(&engine.spec.data)?;
                engine.size(b)
            },
            || {
                let t0 = Instant::now();
                engine.up()?;
                engine.wait_ready(&ctx.http, first_url, t0, ctx.profile.start_timeout)?;
                Ok(t0.elapsed().as_secs_f64())
            },
        );
        match started {
            Ok(cold_start) => {
                log(&format!(
                    "{}: first correct answer {:.2}s after up ({} files, {:.2} GiB evicted)",
                    engine.spec.name,
                    cold_start.value,
                    evicted.files,
                    evicted.bytes as f64 / GIB
                ));
                Ok(Session {
                    engine,
                    cold_start,
                    evicted,
                })
            }
            Err(e) => {
                if let Err(d) = engine.down() {
                    log(&format!(
                        "{}: compose down after a failed start: {d:#}",
                        engine.spec.name
                    ));
                }
                Err(e)
            }
        }
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.engine.down() {
            log(&format!("{}: compose down: {e:#}", self.engine.spec.name));
        }
    }
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// One untimed oha pass over each URL file.
/// How long one warm-up pass may take: three times what the pass would take
/// at the job's rate. An engine slower than that on 16 connections cannot
/// serve the job, and waiting longer only spends request timeouts.
fn warm_limit(queries: usize) -> Duration {
    Duration::from_secs_f64(3.0 * queries as f64 / JOB_RPS)
}

fn warm_up(ctx: &Ctx, engine: &Engine, urls: &Urls) -> Result<()> {
    for d in Direction::BOTH {
        let label = format!("{}-{}-warm", engine.spec.name, d.name());
        let r = ctx.oha.run(
            &label,
            urls.file(d),
            Shape::Warm {
                n: ctx.profile.queries,
                limit: warm_limit(ctx.profile.queries),
            },
        )?;
        if r.errors + r.non_2xx > 0 {
            log(&format!(
                "{label}: {} errors, {} non-2xx during warm-up",
                r.errors, r.non_2xx
            ));
        }
    }
    Ok(())
}

fn service_time(
    ctx: &Ctx,
    engine: &Engine,
    b: BoxSize,
    urls: &Urls,
    d: Direction,
) -> Result<Timed<OhaRun>> {
    let label = format!("{}-{}-{}-service", engine.spec.name, b.label(), d.name());
    let r = timed(
        b,
        &label,
        || Ok(()),
        || {
            ctx.oha.run(
                &label,
                urls.file(d),
                Shape::Sequential {
                    n: ctx.profile.queries,
                },
            )
        },
    )?;
    let v = &r.value;
    log(&format!(
        "{label}: p50 {} p99 {} max {} ms, {} errors, {} non-2xx",
        ms(v.p50_ms),
        ms(v.p99_ms),
        ms(v.max_ms),
        v.errors,
        v.non_2xx
    ));
    Ok(r)
}

fn ms(v: Option<f64>) -> String {
    v.map_or("-".into(), |v| format!("{v:.2}"))
}

/// An untimed lead-in at `rate`, then the timed run at `rate`, judged.
pub fn rate_step(ctx: &Ctx, b: BoxSize, label: &str, urls: &Path, rate: f64) -> Result<RateStep> {
    let mut lead_in_raw = None;
    let lead = ctx.profile.lead_in;
    let run = timed(
        b,
        label,
        || {
            if !lead.is_zero() {
                let shape = Shape::Rate {
                    rps: rate,
                    duration: lead,
                };
                lead_in_raw = Some(ctx.oha.run(&format!("{label}-leadin"), urls, shape)?.raw);
            }
            Ok(())
        },
        || {
            ctx.oha.run(
                label,
                urls,
                Shape::Rate {
                    rps: rate,
                    duration: ctx.profile.step,
                },
            )
        },
    )?;
    let (pass, why) = run.value.judge(rate);
    log(&format!(
        "{label}: {} ({why})",
        if pass { "pass" } else { "fail" }
    ));
    Ok(RateStep {
        rate,
        pass,
        why,
        lead_in_raw,
        run,
    })
}

/// One session in box `b`: correctness, warm-up and service time, footprint,
/// and then the ceiling in each direction, as asked.
pub fn session(
    ctx: &Ctx,
    engine: &Engine,
    repeat: u32,
    b: BoxSize,
    measure: bool,
    ceiling: bool,
    context: &str,
) -> Result<()> {
    let name = &engine.spec.name;
    let urls = ctx.urls(engine)?;
    let s = Session::start(ctx, engine, b, &urls.forward[0])?;

    let correctness = if measure {
        let dir = ctx.out.join("correctness");
        std::fs::create_dir_all(&dir)?;
        let mut scores = Vec::new();
        for d in Direction::BOTH {
            let list = match d {
                Direction::Forward => &urls.forward,
                Direction::Reverse => &urls.reverse,
            };
            let path = dir.join(format!("{name}-{}-r{repeat}-{}.jsonl", b.label(), d.name()));
            let sc =
                correctness::score(&ctx.http, &engine.spec.coords, list, &ctx.truths(d), &path)?;
            log(&format!(
                "{name} {}: {:.1}% within 100 m, {:.1}% any result, {} failed",
                d.name(),
                sc.share_within_100m * 100.0,
                sc.share_any_result * 100.0,
                sc.failed
            ));
            scores.push(sc);
        }
        let reverse = scores.pop().unwrap();
        let forward = scores.pop().unwrap();
        Some(PerDirection { forward, reverse })
    } else {
        None
    };

    warm_up(ctx, engine, &urls)?;
    let service = PerDirection {
        forward: service_time(ctx, engine, b, &urls, Direction::Forward)?,
        reverse: service_time(ctx, engine, b, &urls, Direction::Reverse)?,
    };

    if let Some(correctness) = correctness {
        let memory = host::sut_memory()?;
        let images = engine.images()?;
        let data_bytes = engine.data_bytes()?;
        let image_bytes = images.iter().map(|i| i.bytes).sum();
        let rec = MeasureRecord {
            engine: name.clone(),
            repeat,
            boxed: b,
            cold_start_s: s.cold_start.clone(),
            evicted: s.evicted.clone(),
            correctness,
            service: service.clone(),
            memory,
            disk: Disk {
                data_bytes,
                images,
                image_bytes,
                total_bytes: data_bytes + image_bytes,
            },
        };
        ctx.record(&Record::Measure(Box::new(rec)))?;
    }

    if ceiling {
        for d in Direction::BOTH {
            let rec = ceiling_in_session(ctx, engine, repeat, b, d, &urls, &service, context)?;
            ctx.record(&Record::Ceiling(rec))?;
        }
    }
    drop(s);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn ceiling_in_session(
    ctx: &Ctx,
    engine: &Engine,
    repeat: u32,
    b: BoxSize,
    d: Direction,
    urls: &Urls,
    service: &PerDirection<Timed<OhaRun>>,
    context: &str,
) -> Result<CeilingRecord> {
    let Some(p50_ms) = service.get(d).value.p50_ms else {
        bail!(
            "{} {}: no answers in the service-time run",
            engine.spec.name,
            d.name()
        );
    };
    let start = search::start_rate(b.cpus, p50_ms / 1000.0);
    log(&format!(
        "{} {} ceiling in {}: start at {start} req/s",
        engine.spec.name,
        d.name(),
        b.label()
    ));
    let mut steps = Vec::new();
    let mut probe = |rate: f64| {
        let label = format!("{}-{}-{}-q{rate}", engine.spec.name, b.label(), d.name());
        let step = rate_step(ctx, b, &label, urls.file(d), rate)?;
        let pass = step.pass;
        steps.push(step);
        Ok(pass)
    };
    let found = if !ctx.profile.smoke {
        search::ceiling(
            start,
            ctx.profile.ceiling_tolerance,
            ctx.load_limit,
            &mut probe,
        )?
    } else if context == "scale" {
        search::fixed(&[start], ctx.load_limit, &mut probe)?
    } else {
        // One rate well under the start, which should pass, and one well
        // over it, which should fail.
        let low = (start / 10.0).round().max(10.0);
        search::fixed(&[low, (start * 4.0).round()], ctx.load_limit, &mut probe)?
    };
    let bound = found.load_generator_bound;
    let ceiling_rps = if bound { None } else { found.ceiling };
    log(&format!(
        "{} {} ceiling in {}: {}",
        engine.spec.name,
        d.name(),
        b.label(),
        match (bound, ceiling_rps) {
            (true, _) => "load-generator bound".to_string(),
            (false, Some(c)) => format!("{c} req/s"),
            (false, None) => "no rate passed".to_string(),
        }
    ));
    Ok(CeilingRecord {
        context: context.to_string(),
        engine: engine.spec.name.clone(),
        repeat,
        boxed: b,
        direction: d,
        p50_ms,
        start_rps: start,
        load_limit_rps: ctx.load_limit,
        ceiling_rps,
        load_generator_bound: bound,
        search: found,
        steps,
    })
}

/// A fresh session in `b` asked to serve the job. An engine that cannot start
/// in the box fails the step rather than the run.
fn job_step(ctx: &Ctx, engine: &Engine, urls: &Urls, d: Direction, b: BoxSize) -> Result<JobStep> {
    let s = match Session::start(ctx, engine, b, &urls.forward[0]) {
        Ok(s) => s,
        Err(e) if e.is::<Abort>() => return Err(e),
        Err(e) => {
            let not_configurable = e.is::<NotConfigurable>();
            let why = if not_configurable {
                format!("{e:#}")
            } else {
                format!("did not start: {e:#}")
            };
            log(&format!("{} in {}: {why}", engine.spec.name, b.label()));
            return Ok(JobStep {
                boxed: b,
                pass: false,
                not_configurable,
                why,
                cold_start_s: None,
                step: None,
            });
        }
    };
    let cold = Some(s.cold_start.value);
    if let Err(e) = warm_up(ctx, engine, urls) {
        let why = format!("warm-up failed: {e:#}");
        log(&format!("{} in {}: {why}", engine.spec.name, b.label()));
        return Ok(JobStep {
            boxed: b,
            pass: false,
            not_configurable: false,
            why,
            cold_start_s: cold,
            step: None,
        });
    }
    let label = format!("{}-{}-{}-job", engine.spec.name, b.label(), d.name());
    let step = rate_step(ctx, b, &label, urls.file(d), JOB_RPS)?;
    drop(s);
    Ok(JobStep {
        boxed: b,
        pass: step.pass,
        not_configurable: false,
        why: step.why.clone(),
        cold_start_s: cold,
        step: Some(step),
    })
}

/// The smallest box that serves the job: CPUs first at 64 GiB, then memory
/// at that CPU count, then the found box and the next memory step down are
/// each run again from cold.
pub fn floor(ctx: &Ctx, engine: &Engine, d: Direction) -> Result<FloorRecord> {
    let urls = ctx.urls(engine)?;
    let big_mem = BoxSize::BIG.mem_mib;
    let mut rec = FloorRecord {
        engine: engine.spec.name.clone(),
        direction: d,
        job_rps: JOB_RPS,
        cpu_steps: Vec::new(),
        memory: None,
        memory_steps: Vec::new(),
        found: None,
        next_smaller: None,
        confirm_found: Vec::new(),
        confirm_next_smaller: Vec::new(),
        verdict: String::new(),
    };
    if ctx.profile.smoke {
        for b in SMOKE_FLOOR_BOXES {
            rec.memory_steps.push(job_step(ctx, engine, &urls, d, b)?);
        }
        rec.found = rec
            .memory_steps
            .iter()
            .rev()
            .find(|s| s.pass)
            .map(|s| s.boxed);
        rec.verdict = format!(
            "smoke: {}",
            rec.memory_steps
                .iter()
                .map(|s| format!(
                    "{} {}",
                    s.boxed.label(),
                    if s.pass {
                        "pass"
                    } else if s.not_configurable {
                        "not configurable"
                    } else {
                        "fail"
                    }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        log(&format!(
            "{} {} floor: {}",
            engine.spec.name,
            d.name(),
            rec.verdict
        ));
        return Ok(rec);
    }
    let cpus = search::cpu_floor(|cpus| {
        let step = job_step(
            ctx,
            engine,
            &urls,
            d,
            BoxSize {
                cpus,
                mem_mib: big_mem,
            },
        )?;
        let pass = step.pass;
        rec.cpu_steps.push(step);
        Ok(pass)
    })?
    .found;
    let Some(cpus) = cpus else {
        rec.verdict = "does not meet job".into();
        log(&format!(
            "{} {} floor: does not meet the job at 16 CPUs / 64 GiB",
            engine.spec.name,
            d.name()
        ));
        return Ok(rec);
    };
    let mut probe = |mem_mib: u64| {
        let step = job_step(ctx, engine, &urls, d, BoxSize { cpus, mem_mib })?;
        let pass = step.pass;
        rec.memory_steps.push(step);
        Ok(pass)
    };
    if let Some(ladder) = &ctx.profile.memory_ladder {
        let mem = search::memory_ladder(ladder, big_mem, &mut probe)?;
        let found = BoxSize {
            cpus,
            mem_mib: mem.found_mib,
        };
        rec.found = Some(found);
        rec.next_smaller = mem.below_tested.then_some(BoxSize {
            cpus,
            mem_mib: mem.below_mib,
        });
        rec.verdict = match rec.next_smaller {
            Some(n) => format!("ladder: {} fails", n.label()),
            None => format!("ladder: passes at the bottom rung, {}", found.label()),
        };
        rec.memory = Some(mem);
        log(&format!(
            "{} {} floor: {} ({})",
            engine.spec.name,
            d.name(),
            found.label(),
            rec.verdict
        ));
        return Ok(rec);
    }
    let mem = search::memory_floor(FLOOR_MIN_MIB, big_mem, &mut probe)?;
    let found = BoxSize {
        cpus,
        mem_mib: mem.found_mib,
    };
    let next = BoxSize {
        cpus,
        mem_mib: mem.below_mib,
    };
    rec.memory = Some(mem);
    rec.found = Some(found);
    rec.next_smaller = Some(next);
    for _ in 0..ctx.profile.confirm_runs {
        rec.confirm_found
            .push(job_step(ctx, engine, &urls, d, found)?);
    }
    for _ in 0..ctx.profile.confirm_runs {
        rec.confirm_next_smaller
            .push(job_step(ctx, engine, &urls, d, next)?);
    }
    let passed = |v: &[JobStep]| v.iter().filter(|s| s.pass).count();
    let (f, n, runs) = (
        passed(&rec.confirm_found),
        passed(&rec.confirm_next_smaller),
        ctx.profile.confirm_runs as usize,
    );
    let below_tested = rec.memory.as_ref().is_some_and(|m| m.below_tested);
    rec.verdict = if f == runs && n < runs {
        "confirmed".into()
    } else if f == runs && !below_tested {
        format!(
            "lower bound reached: {} also passed {n}/{runs}",
            mib_label(FLOOR_MIN_MIB)
        )
    } else {
        format!("unconfirmed: found box passed {f}/{runs}, next smaller passed {n}/{runs}")
    };
    log(&format!(
        "{} {} floor: {} ({})",
        engine.spec.name,
        d.name(),
        found.label(),
        rec.verdict
    ));
    Ok(rec)
}

/// og's scaling curve: the ceiling at each CPU count with 64 GiB.
pub fn scale(ctx: &Ctx, engine: &Engine) -> Result<()> {
    let points: &[u32] = if ctx.profile.smoke {
        &SMOKE_SCALE_CPUS
    } else {
        &SCALE_CPUS
    };
    for &cpus in points {
        let b = BoxSize {
            cpus,
            mem_mib: BoxSize::BIG.mem_mib,
        };
        guarded(
            ctx,
            &engine.spec.name,
            &format!("scale {}", b.label()),
            || session(ctx, engine, 0, b, false, true, "scale"),
        )?;
    }
    Ok(())
}

/// Runs one unit of work; an error is logged and recorded, and the run goes
/// on to the next unit. An [`Abort`], or a failure to write the record, stops
/// the run.
pub fn guarded(ctx: &Ctx, engine: &str, stage: &str, f: impl FnOnce() -> Result<()>) -> Result<()> {
    if let Err(e) = f() {
        if e.is::<Abort>() {
            return Err(e);
        }
        let message = format!("{e:#}");
        log(&format!("ERROR {engine} {stage}: {message}"));
        ctx.record(&Record::Error(ErrorRecord {
            engine: engine.into(),
            stage: stage.into(),
            message,
        }))
        .context("recording an error")?;
    }
    Ok(())
}
