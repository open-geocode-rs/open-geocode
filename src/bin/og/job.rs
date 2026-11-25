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
    builder::{
        BuildObserver, BuildOsmOptions, BuildPlan, BuildStage, Leftover, LiveStage, StageUnit,
        build_planned_osm_pack, plan_osm_build,
    },
    openaddresses::OpenAddresses,
};

use crate::{
    format,
    home::{BuildRecord, Home, PackInfo},
    net,
    regions::{self, Region},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unit {
    Bytes,
    Count(&'static str),
    None,
}

pub(crate) struct Step {
    pub did: &'static str,
    pub doing: &'static str,
    pub object: String,
    pub explain: [&'static str; 2],
    pub unit: Unit,
    pub total: Option<u64>,
    pub started: Option<Instant>,
    pub secs: Option<f64>,
}

pub(crate) struct Progress {
    pub steps: Mutex<Vec<Step>>,
    pub current: AtomicU64,
    pub done: AtomicU64,
    pub started: Instant,
    pub plan: Mutex<Option<BuildPlan>>,
    pub warnings: Mutex<Vec<String>>,
    pub live: Mutex<Option<String>>,
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
            live: Mutex::new(None),
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

#[derive(Clone)]
pub(crate) struct BuildInput {
    pub region: &'static Region,
    pub extract: PathBuf,
    pub download: bool,
}

pub(crate) struct BuildRequest {
    pub inputs: Vec<BuildInput>,
    pub pack: PathBuf,
    pub memory_budget_bytes: Option<usize>,
    pub scratch_budget_bytes: Option<u64>,
    pub force: bool,
    pub leftover: Leftover,
    pub openaddresses: bool,
    pub tiger: bool,
}

pub(crate) struct Built {
    pub pack: PackInfo,
    pub build_secs: f64,
    pub costs: String,
}

pub(crate) struct BuildJob {
    pub progress: Arc<Progress>,
    pack: PathBuf,
    extracts: Vec<PathBuf>,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<Built>>>,
}

impl Progress {
    pub(crate) fn of(request: &BuildRequest) -> Arc<Self> {
        let mut steps = Vec::new();
        for input in request.inputs.iter().filter(|input| input.download) {
            let named = |what: &str| {
                if request.inputs.len() > 1 {
                    format!("{what} for {}", input.region.name)
                } else {
                    what.to_string()
                }
            };
            steps.push(Step {
                did: "Downloaded",
                doing: "Downloading",
                object: named(regions::dataset_name(input.region.dataset())),
                explain: [
                    "Fetching the region's extract from its publisher.",
                    "A stopped download continues where it left off next time.",
                ],
                unit: Unit::Bytes,
                total: Some(input.region.osm.bytes),
                started: None,
                secs: None,
            });
            steps.push(Step {
                did: "Checked",
                doing: "Checking",
                object: named("checksum"),
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
        Self::new(steps)
    }
}

impl BuildJob {
    pub(crate) fn start(request: BuildRequest, home: Arc<Home>) -> Self {
        let progress = Progress::of(&request);
        Self::spawn(request, home, progress)
    }

    pub(crate) fn spawn(request: BuildRequest, home: Arc<Home>, progress: Arc<Progress>) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let pack = request.pack.clone();
        let extracts = request
            .inputs
            .iter()
            .map(|input| input.extract.clone())
            .collect();
        let handle = {
            let progress = Arc::clone(&progress);
            let cancel = Arc::clone(&cancel);
            thread::spawn(move || run(&request, &home, &progress, &cancel))
        };
        Self {
            progress,
            pack,
            extracts,
            cancel,
            handle: Some(handle),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub(crate) fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    pub(crate) fn touches(&self, pack: &Path, extract: &Path) -> bool {
        self.pack == pack || self.extracts.iter().any(|own| own == extract)
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

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

impl Drop for BuildJob {
    fn drop(&mut self) {
        self.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub(crate) fn cancelled(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<open_geocode::Error>(),
        Some(open_geocode::Error::Cancelled)
    )
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
            "Stored",
            "Storing",
            "node coordinates",
            [
                "Keeping the coordinates of the points streets and buildings are drawn with.",
                "When memory cannot hold them they go to a scratch file; that is normal.",
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
                "from countries down to neighbourhoods.",
            ],
        ),
        BuildStage::ImportAddresses => (
            "Imported",
            "Importing",
            "addresses",
            [
                "Adding OpenAddresses points to the OSM addresses,",
                "leaving out the ones OSM already has.",
            ],
        ),
        BuildStage::ContextRecords => (
            "Wrote",
            "Writing",
            "context records",
            [
                "Writing places and admin areas, so every result",
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
        BuildStage::StructuredIndex => (
            "Built",
            "Building",
            "structured index",
            [
                "Keying every street and place by where it lies,",
                "so most addresses are found without a text search.",
            ],
        ),
    };
    let unit = match stage.unit() {
        StageUnit::Bytes => Unit::Bytes,
        StageUnit::Blocks => Unit::Count("blocks"),
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

struct Observer {
    progress: Arc<Progress>,
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

    fn measured(&self, live: &LiveStage) {
        *self.progress.live.lock().expect("live lock") = (!live.finished).then(|| live.summary());
    }
}

fn run(
    request: &BuildRequest,
    home: &Home,
    progress: &Arc<Progress>,
    cancel: &Arc<AtomicBool>,
) -> Result<Built> {
    let mut offset = 0;
    let mut index = None;
    for input in request.inputs.iter().filter(|input| input.download) {
        // Geofabrik's index says where the extract is now. The catalog's own
        // link is only for what the index does not hold, the planet.
        let url = match &input.region.osm.id {
            Some(id) => {
                if index.is_none() {
                    index = Some(net::Index::fetch(net::GEOFABRIK_INDEX)?);
                }
                index
                    .as_ref()
                    .context("the index was not read")?
                    .link(id)?
                    .to_string()
            }
            None => input.region.osm.url.clone(),
        };
        let url = &url;
        let (download_step, check_step) = (offset, offset + 1);
        progress.start(download_step, Some(input.region.osm.bytes));
        let published = net::published(url)?;
        let partial = net::download(&published, &input.extract, cancel, |done, total| {
            if let Some(total) = total {
                progress.steps.lock().expect("steps lock")[download_step].total = Some(total);
            }
            progress.done.store(done, Ordering::Relaxed);
        })?;
        progress.finish(download_step);
        let bytes = fs::metadata(&partial)?.len();
        progress.start(check_step, Some(bytes));
        if let Err(error) = net::check_md5(&partial, &published.md5, cancel, |done| {
            progress.done.store(done, Ordering::Relaxed);
        }) {
            if !cancel.load(Ordering::Relaxed) {
                net::discard(&partial);
            }
            return Err(error);
        }
        progress.finish(check_step);
        net::complete(&partial, &input.extract)?;
        let mut md5 = input.extract.clone().into_os_string();
        md5.push(".md5");
        let name = input
            .extract
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        fs::write(&md5, format!("{}  {name}\n", published.md5))?;
        offset += 2;
    }

    let options = BuildOsmOptions {
        inputs: request
            .inputs
            .iter()
            .map(|input| input.extract.clone())
            .collect(),
        pack: request.pack.clone(),
        memory_budget_bytes: request.memory_budget_bytes,
        scratch_budget_bytes: request.scratch_budget_bytes,
        force: request.force,
        leftover: request.leftover,
        input_sources: request
            .inputs
            .iter()
            .map(|input| input.region.id.clone())
            .collect(),
        observer: Arc::new(Observer {
            progress: Arc::clone(progress),
            offset,
        }),
        cancel: Arc::clone(cancel),
        addresses: if request.openaddresses {
            Some(Arc::new(OpenAddresses::from_env(home.openaddresses_dir())?))
        } else {
            None
        },
        imported_rows: if request.openaddresses {
            request
                .inputs
                .iter()
                .filter_map(|input| input.region.openaddresses)
                .map(|data| data.rows)
                .sum()
        } else {
            0
        },
        tiger: request.tiger,
        ..BuildOsmOptions::default()
    };
    let plan = plan_osm_build(&options)?;
    *progress.plan.lock().expect("plan lock") = Some(plan.clone());
    // The process's global pool is the batch runs', sized to every CPU the
    // first time one runs. The build gets a pool of its own, sized by the
    // plan.
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(plan.threads)
        .build()
        .context("failed to start the build's worker threads")?;
    let started = Instant::now();
    let report = pool.install(|| build_planned_osm_pack(&options, plan))?;
    let build_secs = started.elapsed().as_secs_f64();
    let pack = PackInfo::read(&request.pack)
        .with_context(|| format!("the new Pack at {} cannot be read", request.pack.display()))?;
    let finished_at = format::now();
    for (input, reported) in request.inputs.iter().zip(&report.inputs) {
        let share = if report.input_bytes > 0 {
            reported.bytes as f64 / report.input_bytes as f64
        } else {
            1.0
        };
        home.record_build(&BuildRecord {
            region: input.region.id.clone(),
            input_bytes: reported.bytes,
            build_secs: build_secs * share,
            pack_bytes: (report.output.pack_bytes as f64 * share) as u64,
            memory_budget_bytes: report.resources.plan.memory_budget_bytes,
            threads: report.resources.threads,
            finished_at,
            pack_format: open_geocode::pack::PACK_SCHEMA_VERSION,
            openaddresses: request.openaddresses,
        })?;
    }
    Ok(Built {
        pack,
        build_secs,
        costs: report.stage_table(),
    })
}
