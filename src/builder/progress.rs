//! What a build reports while it runs, and how it is stopped.
//!
//! The builder names each stage as it starts, says how much work the stage
//! has when that is known, and counts the work as it is done. A
//! [`BuildObserver`] turns that into whatever its caller shows: bars on a
//! terminal, lines in a log, or the screens of the interactive interface.

use std::{
    fmt,
    io::IsTerminal,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};

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
    /// Build the admin boundary polygons and postcode centroids. No measure.
    Boundaries,
    /// Write places, postcodes and boundaries. Measured in records.
    ContextRecords,
    /// Write addresses, POIs, interpolations and streets. Measured in records.
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
    pub const ALL: [BuildStage; 9] = [
        BuildStage::Scan,
        BuildStage::BoundaryMembers,
        BuildStage::NodeJoin,
        BuildStage::AssembleWays,
        BuildStage::Boundaries,
        BuildStage::ContextRecords,
        BuildStage::Records,
        BuildStage::Indexes,
        BuildStage::Parser,
    ];

    pub const fn unit(self) -> StageUnit {
        match self {
            Self::Scan => StageUnit::Bytes,
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

/// The observer and the stop flag, as the passes of a build share them.
#[derive(Clone)]
pub(crate) struct Progress {
    observer: Arc<dyn BuildObserver>,
    cancel: Arc<AtomicBool>,
}

impl Progress {
    pub(crate) fn new(observer: Arc<dyn BuildObserver>, cancel: Arc<AtomicBool>) -> Self {
        Self { observer, cancel }
    }

    /// Starts a stage, unless the build was asked to stop.
    pub(crate) fn start(&self, stage: BuildStage, total: Option<u64>) -> Result<()> {
        self.check()?;
        self.observer.started(stage, total);
        Ok(())
    }

    pub(crate) fn advance(&self, units: u64) {
        self.observer.advanced(units);
    }

    pub(crate) fn finish(&self, stage: BuildStage) {
        self.observer.finished(stage);
    }

    /// Fails with [`BuildCancelled`] once the build was asked to stop.
    pub(crate) fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(BuildCancelled.into());
        }
        Ok(())
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
                "{msg:32} [{bar:40.cyan/blue}] {percent:>3}% {bytes}/{total_bytes} {bytes_per_sec} elapsed {elapsed_precise}"
            }
            (_, Some(_)) => {
                "{msg:32} [{bar:40.cyan/blue}] {percent:>3}% {pos}/{len} {per_sec} elapsed {elapsed_precise}"
            }
            (_, None) => "{msg:32} {spinner} elapsed {elapsed_precise}",
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
}
