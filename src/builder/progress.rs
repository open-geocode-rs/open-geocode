//! What a build reports while it runs, and how it is stopped.
//!
//! The builder names each stage as it starts, says how much work the stage
//! has when that is known, and counts the work as it is done. A
//! [`BuildObserver`] turns that into whatever its caller shows: bars on a
//! terminal, lines in a log, or the screens of the interactive interface.
//!
//! A [`Meter`] measures what each stage costs: wall and CPU time, bytes read
//! from each input, scratch written and read by owner, and the peaks of
//! scratch, RAM and the memory budget. From the shares of time each
//! resource was busy it names the one that bound the stage ([`Bound`]),
//! live every second and in the build report at the end.

use std::{
    collections::BTreeMap,
    fmt,
    io::IsTerminal,
    path::Path,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    builder::report::{Bound, BoundShares, InputRead, StageReport},
    error::Result,
    extsort::{IoTimes, Scratch, Traffic},
    memory::Budget,
    resources,
};
use indicatif::{ProgressBar, ProgressStyle};

/// How often a running stage is measured for live progress.
const SAMPLE_EVERY: Duration = Duration::from_secs(1);

/// The stages of a build, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildStage {
    /// Pass 1: classify every OSM object. Measured in bytes of the input.
    Scan,
    /// Pass 2: read the ways boundary relations are made of. Measured in
    /// blocks of the input.
    BoundaryMembers,
    /// Pass 3: join node coordinates onto way references. Measured in blocks
    /// of the input.
    NodeJoin,
    /// Turn ways into records. Measured in ways.
    AssembleWays,
    /// Build the admin boundary polygons. No measure.
    Boundaries,
    /// Read address files from outside OSM, such as OpenAddresses, fetching
    /// them first when the source downloads. Measured in bytes of the files.
    ImportAddresses,
    /// Write places and boundaries. Measured in records.
    ContextRecords,
    /// Write addresses, POIs, interpolations, streets and postcodes. Measured
    /// in records.
    Records,
    /// Finish the text and spatial indexes and write the Pack file. No
    /// measure.
    Indexes,
    /// Train the address parser on the Pack. No measure.
    Parser,
}

/// What a stage's work is counted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageUnit {
    Bytes,
    Blocks,
    Ways,
    Records,
    /// The stage has no measure: only its time is known.
    None,
}

impl BuildStage {
    pub const ALL: [BuildStage; 10] = [
        BuildStage::Scan,
        BuildStage::BoundaryMembers,
        BuildStage::NodeJoin,
        BuildStage::AssembleWays,
        BuildStage::Boundaries,
        BuildStage::ImportAddresses,
        BuildStage::ContextRecords,
        BuildStage::Records,
        BuildStage::Indexes,
        BuildStage::Parser,
    ];

    /// What the stage's work is counted in, as the build report names it.
    pub const fn unit_name(self) -> &'static str {
        match self.unit() {
            StageUnit::Bytes => "bytes",
            StageUnit::Blocks => "blocks",
            StageUnit::Ways => "ways",
            StageUnit::Records => "records",
            StageUnit::None => "none",
        }
    }

    pub const fn unit(self) -> StageUnit {
        match self {
            Self::Scan | Self::ImportAddresses => StageUnit::Bytes,
            Self::BoundaryMembers | Self::NodeJoin => StageUnit::Blocks,
            Self::AssembleWays => StageUnit::Ways,
            Self::ContextRecords | Self::Records => StageUnit::Records,
            Self::Boundaries | Self::Indexes | Self::Parser => StageUnit::None,
        }
    }

    /// One line naming the stage, for logs and terminal bars.
    pub const fn description(self) -> &'static str {
        match self {
            Self::Scan => "scan OSM objects",
            Self::BoundaryMembers => "read boundary member ways",
            Self::NodeJoin => "join node coordinates",
            Self::AssembleWays => "assemble ways",
            Self::Boundaries => "build admin boundaries",
            Self::ImportAddresses => "import addresses",
            Self::ContextRecords => "write context records",
            Self::Records => "write records",
            Self::Indexes => "finish Pack indexes",
            Self::Parser => "train address parser",
        }
    }

    /// The stage's place in the build, from 1.
    pub fn number(self) -> usize {
        Self::ALL
            .iter()
            .position(|stage| *stage == self)
            .map_or(0, |index| index + 1)
    }
}

/// Receives a build's progress. Calls come from the build's threads; a stage
/// is started, advanced any number of times, then finished, and stages never
/// overlap. A stage the input does not need is started and finished at once.
pub trait BuildObserver: Send + Sync {
    /// A stage starts. `total` is its work in the stage's unit when known up
    /// front.
    fn started(&self, stage: BuildStage, total: Option<u64>);
    /// `units` more of the running stage's work is done.
    fn advanced(&self, units: u64);
    fn finished(&self, stage: BuildStage);
    /// Something the build should be told about but goes on despite, such
    /// as the pre-flight check finding memory or disk short of what it needs.
    fn warned(&self, message: &str);
    /// What the running stage costs so far, about every second while it
    /// runs and once when it ends.
    fn measured(&self, live: &LiveStage) {
        let _ = live;
    }
}

/// The running stage's throughput and what bounds it.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveStage {
    pub stage: BuildStage,
    pub elapsed: Duration,
    pub threads: usize,
    /// The stage's work per second, in its unit.
    pub units_per_sec: f64,
    /// Input bytes read per second.
    pub input_bytes_per_sec: f64,
    /// Scratch bytes written per second, as on disk.
    pub scratch_bytes_per_sec: f64,
    pub live_scratch_bytes: u64,
    pub shares: BoundShares,
    pub bound: Bound,
    /// The stage has ended: this is its final measure.
    pub finished: bool,
}

impl LiveStage {
    /// One line for a log or a progress bar.
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("{:.1}/{} cores", self.shares.cores, self.threads)];
        if self.input_bytes_per_sec > 0.0 {
            parts.push(format!("in {:.0} MB/s", self.input_bytes_per_sec / 1e6));
        }
        if self.scratch_bytes_per_sec > 0.0 {
            parts.push(format!(
                "spill {:.0} MB/s",
                self.scratch_bytes_per_sec / 1e6
            ));
        }
        parts.push(format!(
            "scratch {:.0} MiB",
            self.live_scratch_bytes as f64 / f64::from(1 << 20)
        ));
        parts.push(format!("bound {}", self.bound.as_str()));
        parts.join(", ")
    }
}

/// The error a build returns when it was asked to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildCancelled;

impl fmt::Display for BuildCancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("build cancelled")
    }
}

impl std::error::Error for BuildCancelled {}

/// The observer and the stop flag, as the passes of a build share them,
/// and the meter that measures each stage.
#[derive(Clone)]
pub(crate) struct Progress {
    observer: Arc<dyn BuildObserver>,
    cancel: Arc<AtomicBool>,
    meter: Option<Arc<Meter>>,
}

impl Progress {
    pub(crate) fn new(observer: Arc<dyn BuildObserver>, cancel: Arc<AtomicBool>) -> Self {
        Self {
            observer,
            cancel,
            meter: None,
        }
    }

    /// Measure each stage against `scratch` and `budget`, sampling the
    /// running one every second for the observer.
    pub(crate) fn metered(mut self, scratch: &Arc<Scratch>, budget: &Arc<Budget>) -> Self {
        let meter = Arc::new(Meter {
            scratch: Arc::clone(scratch),
            budget: Arc::clone(budget),
            threads: rayon::current_num_threads(),
            inputs: Mutex::new(Vec::new()),
            running: Mutex::new(None),
            stages: Mutex::new(Vec::new()),
            units: AtomicU64::new(0),
            peak_rss: AtomicU64::new(0),
        });
        let weak = Arc::downgrade(&meter);
        let observer = Arc::clone(&self.observer);
        std::thread::spawn(move || sample(&weak, &*observer));
        self.meter = Some(meter);
        self
    }

    /// Starts a stage, unless the build was asked to stop.
    pub(crate) fn start(&self, stage: BuildStage, total: Option<u64>) -> Result<()> {
        self.check()?;
        if let Some(meter) = &self.meter {
            meter.begin(stage);
        }
        self.observer.started(stage, total);
        Ok(())
    }

    pub(crate) fn advance(&self, units: u64) {
        if let Some(meter) = &self.meter {
            meter.units.fetch_add(units, Ordering::Relaxed);
        }
        self.observer.advanced(units);
    }

    pub(crate) fn finish(&self, stage: BuildStage) {
        if let Some(meter) = &self.meter
            && let Some(report) = meter.end(stage)
        {
            self.observer.measured(&report.1);
            meter.stages.lock().expect("stages lock").push(report.0);
        }
        self.observer.finished(stage);
    }

    /// The counter of what is read from `input`.
    pub(crate) fn input(&self, input: &Path) -> Arc<InputCounter> {
        match &self.meter {
            Some(meter) => meter.input(input),
            None => Arc::default(),
        }
    }

    /// What every finished stage cost, in order.
    pub(crate) fn stages(&self) -> Vec<StageReport> {
        self.meter.as_ref().map_or_else(Vec::new, |meter| {
            meter.stages.lock().expect("stages lock").clone()
        })
    }

    /// Fails with [`BuildCancelled`] once the build was asked to stop.
    pub(crate) fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(BuildCancelled.into());
        }
        Ok(())
    }
}

/// Bytes read from one input and the time spent reading them.
#[derive(Debug, Default)]
pub(crate) struct InputCounter {
    bytes: AtomicU64,
    nanos: AtomicU64,
}

impl InputCounter {
    pub(crate) fn add(&self, bytes: u64, time: Duration) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        self.nanos.fetch_add(
            u64::try_from(time.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    fn read(&self) -> InputRead {
        InputRead {
            bytes: self.bytes.load(Ordering::Relaxed),
            wait_ms: self.nanos.load(Ordering::Relaxed) / 1_000_000,
        }
    }
}

/// Measures what each stage of a build costs.
pub(crate) struct Meter {
    scratch: Arc<Scratch>,
    budget: Arc<Budget>,
    threads: usize,
    inputs: Mutex<Vec<(String, Arc<InputCounter>)>>,
    running: Mutex<Option<Snapshot>>,
    stages: Mutex<Vec<StageReport>>,
    units: AtomicU64,
    peak_rss: AtomicU64,
}

/// The counters when a stage started.
#[derive(Debug, Clone)]
struct Snapshot {
    stage: BuildStage,
    started: Instant,
    cpu: Duration,
    /// The thread that started the stage, and its CPU time then.
    driver: Option<(Arc<resources::ThreadClock>, Duration)>,
    inputs: BTreeMap<String, InputRead>,
    traffic: BTreeMap<String, Traffic>,
    io: IoTimes,
}

impl Meter {
    fn input(&self, input: &Path) -> Arc<InputCounter> {
        let name = input.display().to_string();
        let mut inputs = self.inputs.lock().expect("inputs lock");
        if let Some((_, counter)) = inputs.iter().find(|(known, _)| *known == name) {
            return Arc::clone(counter);
        }
        let counter = Arc::<InputCounter>::default();
        inputs.push((name, Arc::clone(&counter)));
        counter
    }

    fn input_reads(&self) -> BTreeMap<String, InputRead> {
        self.inputs
            .lock()
            .expect("inputs lock")
            .iter()
            .map(|(name, counter)| (name.clone(), counter.read()))
            .collect()
    }

    fn snapshot(&self, stage: BuildStage) -> Snapshot {
        Snapshot {
            stage,
            started: Instant::now(),
            cpu: resources::process_cpu_time().unwrap_or_default(),
            driver: None,
            inputs: self.input_reads(),
            traffic: self.scratch.traffic(),
            io: self.scratch.io_times(),
        }
    }

    fn begin(&self, stage: BuildStage) {
        self.scratch.set_stage(stage.description());
        self.scratch.measure();
        self.scratch.reset_stage_peak();
        self.budget.reset_stage_peak();
        self.units.store(0, Ordering::Relaxed);
        self.peak_rss
            .store(resources::resident_bytes().unwrap_or(0), Ordering::Relaxed);
        let mut start = self.snapshot(stage);
        start.driver = resources::ThreadClock::current()
            .and_then(|clock| Some((clock.cpu_time()?, clock)))
            .map(|(time, clock)| (Arc::new(clock), time));
        *self.running.lock().expect("running lock") = Some(start);
    }

    /// The running stage's report so far, and its live view.
    fn report(&self, start: &Snapshot) -> (StageReport, LiveStage) {
        let now = self.snapshot(start.stage);
        self.peak_rss
            .fetch_max(resources::resident_bytes().unwrap_or(0), Ordering::Relaxed);
        let wall = now.started.duration_since(start.started);
        let secs = wall.as_secs_f64().max(1e-9);
        let cpu = now.cpu.saturating_sub(start.cpu);
        let inputs = now
            .inputs
            .iter()
            .filter_map(|(name, read)| {
                let before = start.inputs.get(name).copied().unwrap_or_default();
                let bytes = read.bytes - before.bytes;
                (bytes > 0).then(|| {
                    (
                        name.clone(),
                        InputRead {
                            bytes,
                            wait_ms: read.wait_ms - before.wait_ms,
                        },
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();
        let spills = now
            .traffic
            .iter()
            .map(|(label, traffic)| {
                let before = start.traffic.get(label).copied().unwrap_or_default();
                (label.clone(), traffic.since(&before))
            })
            .filter(|(_, traffic)| !traffic.is_idle())
            .collect::<BTreeMap<_, _>>();
        let total = |field: fn(&Traffic) -> u64| spills.values().map(field).sum::<u64>();
        let read_wait = inputs.values().map(|read| read.wait_ms).sum::<u64>() as f64 / 1000.0
            + (now.io.read_wait - start.io.read_wait).as_secs_f64();
        let share = |value: f64| (value / secs).min(1.0);
        let driver = start.driver.as_ref().map_or(0.0, |(clock, before)| {
            clock
                .cpu_time()
                .map_or(0.0, |now| now.saturating_sub(*before).as_secs_f64())
        });
        let shares = BoundShares {
            cpu: share(cpu.as_secs_f64() / self.threads.max(1) as f64),
            cores: cpu.as_secs_f64() / secs,
            driver: share(driver),
            read: share(read_wait),
            write: share((now.io.write - start.io.write).as_secs_f64()),
            spill_wait: share((now.io.spill_wait - start.io.spill_wait).as_secs_f64()),
        };
        let bound = Bound::classify(&shares);
        let units = self.units.load(Ordering::Relaxed);
        let input_bytes = inputs.values().map(|read| read.bytes).sum::<u64>();
        let report = StageReport {
            stage: start.stage.description().to_string(),
            wall_ms: wall.as_millis() as u64,
            cpu_ms: cpu.as_millis() as u64,
            threads: self.threads,
            unit: start.stage.unit_name().to_string(),
            units,
            items_written: total(|traffic| traffic.items_written),
            items_read: total(|traffic| traffic.items_read),
            scratch_raw_written_bytes: total(|traffic| traffic.raw_written_bytes),
            scratch_written_bytes: total(|traffic| traffic.written_bytes),
            scratch_raw_read_bytes: total(|traffic| traffic.raw_read_bytes),
            scratch_read_bytes: total(|traffic| traffic.read_bytes),
            inputs,
            spills,
            peak_scratch_bytes: self.scratch.stage_peak_bytes(),
            peak_rss_bytes: self.peak_rss.load(Ordering::Relaxed),
            peak_budget_bytes: self.budget.stage_peak() as u64,
            shares,
            bound,
        };
        let live = LiveStage {
            stage: start.stage,
            elapsed: wall,
            threads: self.threads,
            units_per_sec: units as f64 / secs,
            input_bytes_per_sec: input_bytes as f64 / secs,
            scratch_bytes_per_sec: report.scratch_written_bytes as f64 / secs,
            live_scratch_bytes: self.scratch.live_bytes(),
            shares,
            bound,
            finished: false,
        };
        (report, live)
    }

    fn end(&self, stage: BuildStage) -> Option<(StageReport, LiveStage)> {
        self.scratch.measure();
        let start = self.running.lock().expect("running lock").take()?;
        (start.stage == stage).then(|| {
            let (report, mut live) = self.report(&start);
            live.finished = true;
            (report, live)
        })
    }

    fn sample(&self) -> Option<LiveStage> {
        self.scratch.measure();
        let start = self.running.lock().expect("running lock").clone()?;
        Some(self.report(&start).1)
    }
}

/// Every [`SAMPLE_EVERY`], tell `observer` what the running stage costs so
/// far, until the meter is dropped.
fn sample(meter: &Weak<Meter>, observer: &dyn BuildObserver) {
    loop {
        std::thread::sleep(SAMPLE_EVERY);
        let Some(meter) = meter.upgrade() else {
            return;
        };
        if let Some(live) = meter.sample() {
            observer.measured(&live);
        }
    }
}

/// Draws one bar per stage on stderr when it is a terminal, and writes a
/// line per stage with the time since the build began when it is not, so a
/// build run from a script or a service logs where its time goes.
#[derive(Debug)]
pub struct TerminalObserver {
    started: Instant,
    bar: Mutex<Option<ProgressBar>>,
}

impl TerminalObserver {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            bar: Mutex::new(None),
        }
    }
}

impl Default for TerminalObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl BuildObserver for TerminalObserver {
    fn started(&self, stage: BuildStage, total: Option<u64>) {
        let message = format!(
            "{}/{} {}",
            stage.number(),
            BuildStage::ALL.len(),
            stage.description()
        );
        if !std::io::stderr().is_terminal() {
            eprintln!("[{:>7.1}s] {message}", self.started.elapsed().as_secs_f64());
        }
        let template = match (stage.unit(), total) {
            (StageUnit::Bytes, Some(_)) => {
                "{msg:32} [{bar:40.cyan/blue}] {percent:>3}% {bytes}/{total_bytes} {bytes_per_sec} elapsed {elapsed_precise} {prefix}"
            }
            (_, Some(_)) => {
                "{msg:32} [{bar:40.cyan/blue}] {percent:>3}% {pos}/{len} {per_sec} elapsed {elapsed_precise} {prefix}"
            }
            (_, None) => "{msg:32} {spinner} elapsed {elapsed_precise} {prefix}",
        };
        let bar = match total {
            Some(total) => ProgressBar::new(total),
            None => ProgressBar::new_spinner(),
        };
        bar.set_style(
            ProgressStyle::with_template(template)
                .expect("valid progress template")
                .progress_chars("=> "),
        );
        bar.set_message(message);
        if total.is_none() {
            bar.enable_steady_tick(Duration::from_millis(120));
        }
        *self.bar.lock().expect("progress bar lock") = Some(bar);
    }

    fn advanced(&self, units: u64) {
        if let Some(bar) = self.bar.lock().expect("progress bar lock").as_ref() {
            bar.inc(units);
        }
    }

    fn finished(&self, stage: BuildStage) {
        if let Some(bar) = self.bar.lock().expect("progress bar lock").take() {
            bar.finish_with_message(format!(
                "{}/{} {} complete",
                stage.number(),
                BuildStage::ALL.len(),
                stage.description()
            ));
        }
    }

    fn warned(&self, message: &str) {
        match self.bar.lock().expect("progress bar lock").as_ref() {
            Some(bar) => bar.suspend(|| eprintln!("warning: {message}")),
            None => eprintln!("warning: {message}"),
        }
    }

    fn measured(&self, live: &LiveStage) {
        if let Some(bar) = self.bar.lock().expect("progress bar lock").as_ref() {
            bar.set_prefix(format!("| {}", live.summary()));
        }
        // A log gets the cost once, as the stage ends; a terminal shows it
        // live on the bar.
        if live.finished && !std::io::stderr().is_terminal() {
            eprintln!(
                "[{:>7.1}s]   {}: {}",
                self.started.elapsed().as_secs_f64(),
                live.stage.description(),
                live.summary()
            );
        }
    }
}
