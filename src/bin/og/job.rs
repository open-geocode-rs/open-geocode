//! Work that runs beside the interface: building a Pack, with the download
//! and checksum before it when the extract is not on disk yet. The job runs
//! on its own thread; the screens read its steps as they change.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

use anyhow::{Context, Result};
use open_geocode::{
    batch::Cancelled,
    builder::{
        BuildCancelled, BuildObserver, BuildOsmOptions, BuildPlan, BuildStage, StageUnit,
        build_planned_osm_pack, plan_osm_build,
    },
};

use crate::{
    format,
    home::{BuildRecord, Home, PackInfo},
    net,
    regions::{self, Region},
};

/// What a step's work is counted in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unit {
    Bytes,
    Count(&'static str),
    /// Only the step's time is known.
    None,
}

/// One line of a running job.
pub(crate) struct Step {
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
pub(crate) struct Progress {
    pub steps: Mutex<Vec<Step>>,
    /// The running step, by index.
    pub current: AtomicU64,
    /// Work done in the running step.
    pub done: AtomicU64,
    pub started: Instant,
    /// The plan the build runs with, once it is made.
    pub plan: Mutex<Option<BuildPlan>>,
    /// What the build warned about, in order.
    pub warnings: Mutex<Vec<String>>,
}

impl Progress {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps),
            current: AtomicU64::new(0),
            done: AtomicU64::new(0),
            started: Instant::now(),
            plan: Mutex::new(None),
            warnings: Mutex::new(Vec::new()),
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

    pub(crate) fn current(&self) -> usize {
        self.current.load(Ordering::Relaxed) as usize
    }

    pub(crate) fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }
}

/// A build to run: a region's extract into its Pack.
pub(crate) struct BuildRequest {
    pub region: &'static Region,
    pub extract: PathBuf,
    /// Download the extract first, also when one is on disk.
    pub download: bool,
    pub pack: PathBuf,
    /// Memory for the build's buffers; derived from what the machine can
    /// give when not set.
    pub memory_budget_bytes: Option<usize>,
    /// Build even when the plan finds too little memory or disk.
    pub force: bool,
}

/// A finished build.
pub(crate) struct Built {
    pub pack: PackInfo,
    /// The build itself, without the download.
    pub build_secs: f64,
}

pub(crate) struct BuildJob {
    pub progress: Arc<Progress>,
    /// The Pack it writes, and the extract it reads or downloads.
    pack: PathBuf,
    extract: PathBuf,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<Built>>>,
}

impl BuildJob {
    pub(crate) fn start(request: BuildRequest, home: Arc<Home>) -> Self {
        let mut steps = Vec::new();
        if request.download {
            steps.push(Step {
                did: "Downloaded",
                doing: "Downloading",
                object: regions::dataset_name(request.region.dataset()).to_string(),
                explain: [
                    "Fetching the region's extract from its publisher.",
                    "A stopped download continues where it left off next time.",
                ],
                unit: Unit::Bytes,
                total: Some(request.region.osm.bytes),
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
        let (pack, extract) = (request.pack.clone(), request.extract.clone());
        let handle = {
            let progress = Arc::clone(&progress);
            let cancel = Arc::clone(&cancel);
            thread::spawn(move || run(&request, &home, &progress, &cancel))
        };
        Self {
            progress,
            pack,
            extract,
            cancel,
            handle: Some(handle),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// The flag that stops the job, for a signal handler to set.
    pub(crate) fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// Whether the job writes this Pack, or reads or downloads this extract.
    pub(crate) fn touches(&self, pack: &Path, extract: &Path) -> bool {
        self.pack == pack || self.extract == extract
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// The job's outcome, once it is finished.
    pub(crate) fn take(&mut self) -> Option<Result<Built>> {
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
pub(crate) fn cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<BuildCancelled>().is_some() || error.downcast_ref::<Cancelled>().is_some()
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

/// Hands the builder's stages and warnings to the job's progress.
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

    fn warned(&self, message: &str) {
        self.progress
            .warnings
            .lock()
            .expect("warnings lock")
            .push(message.to_string());
    }
}

fn run(
    request: &BuildRequest,
    home: &Home,
    progress: &Arc<Progress>,
    cancel: &Arc<AtomicBool>,
) -> Result<Built> {
    let mut offset = 0;
    if request.download {
        let url = &request.region.osm.url;
        progress.start(0, Some(request.region.osm.bytes));
        let expected = net::published_md5(url)?;
        let partial = net::download(url, &request.extract, cancel, |done, total| {
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
                net::discard(&partial);
            }
            return Err(error);
        }
        progress.finish(1);
        net::complete(&partial, &request.extract)?;
        let mut md5 = request.extract.clone().into_os_string();
        md5.push(".md5");
        let name = request
            .extract
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        fs::write(&md5, format!("{expected}  {name}\n"))?;
        offset = 2;
    }

    let options = BuildOsmOptions {
        input: request.extract.clone(),
        pack: request.pack.clone(),
        memory_budget_bytes: request.memory_budget_bytes,
        force: request.force,
        input_source: Some(request.region.id.clone()),
        observer: Arc::new(Observer {
            progress: Arc::clone(progress),
            offset,
        }),
        cancel: Arc::clone(cancel),
        ..BuildOsmOptions::default()
    };
    let plan = plan_osm_build(&options)?;
    *progress.plan.lock().expect("plan lock") = Some(plan.clone());
    // The process's global pool is the batch runs', sized to every CPU the
    // first time one runs. The build gets a pool of its own, sized by the
    // plan unless RAYON_NUM_THREADS sets it.
    let threads = if std::env::var_os("RAYON_NUM_THREADS").is_some() {
        0
    } else {
        plan.threads
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .context("failed to start the build's worker threads")?;
    let started = Instant::now();
    let report = pool.install(|| build_planned_osm_pack(options, plan))?;
    let build_secs = started.elapsed().as_secs_f64();
    let pack = PackInfo::read(&request.pack)
        .with_context(|| format!("the new Pack at {} cannot be read", request.pack.display()))?;
    home.record_build(&BuildRecord {
        region: request.region.id.clone(),
        input_bytes: report.input_bytes,
        build_secs,
        pack_bytes: report.output.pack_bytes,
        memory_budget_bytes: report.resources.plan.memory_budget_bytes,
        threads: report.resources.threads,
        finished_at: format::now(),
    })?;
    Ok(Built { pack, build_secs })
}
