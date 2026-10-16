//! Work that runs beside the interface: building a Pack, with the download
//! and checksum before it when the extract is not on disk yet. The job runs
//! on its own thread; the screens read its steps as they change.

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use open_geocode::{
    batch::BatchCancelled,
    builder::{
        BuildCancelled, BuildObserver, BuildOsmOptions, BuildStage, StageUnit, build_osm_pack,
    },
};

use crate::{
    format,
    home::{BuildRecord, Home, PackInfo},
    machine, net,
    regions::Region,
};

/// What a step's work is counted in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Bytes,
    Count(&'static str),
    /// Only the step's time is known.
    None,
}

/// One line of a running job.
pub struct Step {
    /// What it did, once finished: `Scanned`.
    pub did: &'static str,
    /// What it does while it runs: `Scanning`.
    pub doing: &'static str,
    /// What it works on: `OSM objects`.
    pub object: String,
    /// Two lines on what the step does, for the details view.
    pub explain: [&'static str; 2],
    pub unit: Unit,
    pub total: Option<u64>,
    pub started: Option<Instant>,
    /// Seconds the step took, once finished.
    pub secs: Option<f64>,
}

/// A job's steps and where it is, shared with its thread.
pub struct Progress {
    pub steps: Mutex<Vec<Step>>,
    /// The running step, by index.
    pub current: AtomicU64,
    /// Work done in the running step.
    pub done: AtomicU64,
    pub started: Instant,
}

impl Progress {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps),
            current: AtomicU64::new(0),
            done: AtomicU64::new(0),
            started: Instant::now(),
        })
    }

    fn start(&self, index: usize, total: Option<u64>) {
        let mut steps = self.steps.lock().expect("steps lock");
        steps[index].total = total;
        steps[index].started = Some(Instant::now());
        self.done.store(0, Ordering::Relaxed);
        self.current.store(index as u64, Ordering::Relaxed);
    }

    fn finish(&self, index: usize) {
        let mut steps = self.steps.lock().expect("steps lock");
        let step = &mut steps[index];
        step.secs = Some(step.started.map_or(0.0, |at| at.elapsed().as_secs_f64()));
        // A step with a total ends with all of it done, which the last
        // advance may not have said.
        if let Some(total) = step.total {
            self.done.store(total, Ordering::Relaxed);
        }
    }

    pub fn current(&self) -> usize {
        self.current.load(Ordering::Relaxed) as usize
    }

    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }
}

pub struct BuildPlan {
    pub region: &'static Region,
    pub extract: PathBuf,
    /// Download the extract first, also when one is on disk.
    pub download: bool,
    pub pack: PathBuf,
    pub memory_budget_bytes: u64,
}

/// A finished build.
pub struct Built {
    pub pack: PackInfo,
    /// The build itself, without the download.
    pub build_secs: f64,
}

pub struct BuildJob {
    pub progress: Arc<Progress>,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<Built>>>,
}

impl BuildJob {
    pub fn start(plan: BuildPlan, home: Arc<Home>) -> Self {
        let mut steps = Vec::new();
        if plan.download {
            steps.push(Step {
                did: "Downloaded",
                doing: "Downloading",
                object: "OpenStreetMap".to_string(),
                explain: [
                    "Fetching the region's extract from its publisher.",
                    "A stopped download continues where it left off next time.",
                ],
                unit: Unit::Bytes,
                total: Some(plan.region.osm.bytes),
                started: None,
                secs: None,
            });
            steps.push(Step {
                did: "Checked",
                doing: "Checking",
                object: "checksum".to_string(),
                explain: [
                    "Comparing the file with the MD5 sum it is published with,",
                    "so a broken download is never built.",
                ],
                unit: Unit::Bytes,
                total: None,
                started: None,
                secs: None,
            });
        }
        steps.extend(BuildStage::ALL.into_iter().map(build_step));
        let progress = Progress::new(steps);
        let cancel = Arc::new(AtomicBool::new(false));
        let handle = {
            let progress = Arc::clone(&progress);
            let cancel = Arc::clone(&cancel);
            thread::spawn(move || run(&plan, &home, &progress, &cancel))
        };
        Self {
            progress,
            cancel,
            handle: Some(handle),
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// The job's outcome, once it is finished.
    pub fn take(&mut self) -> Option<Result<Built>> {
        if !self.is_finished() {
            return None;
        }
        let handle = self.handle.take()?;
        Some(
            handle
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("the build stopped unexpectedly"))),
        )
    }
}

/// A job let go of is stopped, and waited for: the builder removes its
/// unfinished Pack generation only while it still runs.
impl Drop for BuildJob {
    fn drop(&mut self) {
        self.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Whether an error is a job stopping because it was asked to.
pub fn cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<BuildCancelled>().is_some()
        || error.downcast_ref::<BatchCancelled>().is_some()
}

fn build_step(stage: BuildStage) -> Step {
    let (did, doing, object, explain): (_, _, _, [&'static str; 2]) = match stage {
        BuildStage::Scan => (
            "Scanned",
            "Scanning",
            "OSM objects",
            [
                "Reading every node, way and relation once.",
                "Places, streets, addresses and POIs are kept; the rest is skipped.",
            ],
        ),
        BuildStage::BoundaryMembers => (
            "Read",
            "Reading",
            "boundary ways",
            [
                "Collecting the outline pieces of countries, provinces and cities.",
                "These become the areas every result is placed in.",
            ],
        ),
        BuildStage::NodeJoin => (
            "Joined",
            "Joining",
            "node coordinates",
            [
                "Giving every street and building the coordinates of its points.",
                "Data larger than the memory budget spills to disk; that is normal.",
            ],
        ),
        BuildStage::AssembleWays => (
            "Assembled",
            "Assembling",
            "ways",
            [
                "Turning ways into streets, buildings and address lines.",
                "House number ranges are laid along their streets here.",
            ],
        ),
        BuildStage::Boundaries => (
            "Built",
            "Building",
            "admin areas",
            [
                "Joining boundary pieces into the outlines of each area,",
                "and placing postcode centroids.",
            ],
        ),
        BuildStage::ContextRecords => (
            "Wrote",
            "Writing",
            "context records",
            [
                "Writing places, postcodes and admin areas, so every result",
                "knows its city, region and country.",
            ],
        ),
        BuildStage::Records => (
            "Wrote",
            "Writing",
            "records",
            [
                "Writing every address, street and POI into the Pack,",
                "each with the areas it lies in.",
            ],
        ),
        BuildStage::Indexes => (
            "Built",
            "Building",
            "Pack indexes",
            [
                "Building the text index for search and autocomplete,",
                "and the spatial index for reverse geocoding.",
            ],
        ),
        BuildStage::Parser => (
            "Trained",
            "Training",
            "address parser",
            [
                "Learning this region's street words and postcode shapes,",
                "so queries are read the way people here write addresses.",
            ],
        ),
    };
    let unit = match stage.unit() {
        StageUnit::Bytes => Unit::Bytes,
        StageUnit::Blocks => Unit::Count("blocks"),
        StageUnit::Ways => Unit::Count("ways"),
        StageUnit::Records => Unit::Count("records"),
        StageUnit::None => Unit::None,
    };
    Step {
        did,
        doing,
        object: object.to_string(),
        explain,
        unit,
        total: None,
        started: None,
        secs: None,
    }
}

/// Hands the builder's stages to the job's steps.
struct Observer {
    progress: Arc<Progress>,
    /// Steps before the build's own: the download and the checksum.
    offset: usize,
}

impl BuildObserver for Observer {
    fn started(&self, stage: BuildStage, total: Option<u64>) {
        self.progress.start(self.offset + stage.number() - 1, total);
    }

    fn advanced(&self, units: u64) {
        self.progress.done.fetch_add(units, Ordering::Relaxed);
    }

    fn finished(&self, stage: BuildStage) {
        self.progress.finish(self.offset + stage.number() - 1);
    }
}

fn run(
    plan: &BuildPlan,
    home: &Home,
    progress: &Arc<Progress>,
    cancel: &Arc<AtomicBool>,
) -> Result<Built> {
    let mut offset = 0;
    if plan.download {
        let url = &plan.region.osm.url;
        progress.start(0, Some(plan.region.osm.bytes));
        let expected = net::published_md5(url)?;
        let partial = net::download(url, &plan.extract, cancel, |done, total| {
            if let Some(total) = total {
                progress.steps.lock().expect("steps lock")[0].total = Some(total);
            }
            progress.done.store(done, Ordering::Relaxed);
        })?;
        progress.finish(0);
        let bytes = fs::metadata(&partial)?.len();
        progress.start(1, Some(bytes));
        if let Err(error) = net::check_md5(&partial, &expected, cancel, |done| {
            progress.done.store(done, Ordering::Relaxed);
        }) {
            // A file that does not match is no use to continue from.
            if !cancel.load(Ordering::Relaxed) {
                let _ = fs::remove_file(&partial);
            }
            return Err(error);
        }
        progress.finish(1);
        fs::rename(&partial, &plan.extract)
            .with_context(|| format!("failed to move {} into place", plan.extract.display()))?;
        let mut md5 = plan.extract.clone().into_os_string();
        md5.push(".md5");
        let name = plan
            .extract
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        fs::write(&md5, format!("{expected}  {name}\n"))?;
        offset = 2;
    }

    // The most disk the build holds is what was free when it began less the
    // least that is free while it runs.
    fs::create_dir_all(&plan.pack)
        .with_context(|| format!("failed to create {}", plan.pack.display()))?;
    let free_at_start = machine::free_disk(&plan.pack);
    let least_free = Arc::new(AtomicU64::new(free_at_start.unwrap_or(u64::MAX)));
    let watching = Arc::new(AtomicBool::new(true));
    let watcher = {
        let (least_free, watching, pack) = (
            Arc::clone(&least_free),
            Arc::clone(&watching),
            plan.pack.clone(),
        );
        thread::spawn(move || {
            while watching.load(Ordering::Relaxed) {
                if let Some(free) = machine::free_disk(&pack) {
                    least_free.fetch_min(free, Ordering::Relaxed);
                }
                thread::sleep(Duration::from_millis(500));
            }
        })
    };
    let started = Instant::now();
    let report = build_osm_pack(BuildOsmOptions {
        input: plan.extract.clone(),
        pack: plan.pack.clone(),
        memory_budget_bytes: usize::try_from(plan.memory_budget_bytes)
            .context("the memory budget is larger than this machine can address")?,
        input_source: Some(plan.region.id.clone()),
        observer: Arc::new(Observer {
            progress: Arc::clone(progress),
            offset,
        }),
        cancel: Arc::clone(cancel),
        ..BuildOsmOptions::default()
    });
    watching.store(false, Ordering::Relaxed);
    let _ = watcher.join();
    let report = report?;
    let build_secs = started.elapsed().as_secs_f64();
    let peak_disk_bytes = free_at_start.map_or(0, |free| {
        free.saturating_sub(least_free.load(Ordering::Relaxed))
    });
    let pack = PackInfo::read(&plan.pack)
        .with_context(|| format!("the new Pack at {} cannot be read", plan.pack.display()))?;
    home.record_build(&BuildRecord {
        region: plan.region.id.clone(),
        input_bytes: report.input_bytes,
        build_secs,
        peak_disk_bytes,
        pack_bytes: report.output.pack_bytes,
        memory_budget_bytes: plan.memory_budget_bytes,
        threads: report.threads,
        finished_at: format::now(),
    })?;
    Ok(Built { pack, build_secs })
}
