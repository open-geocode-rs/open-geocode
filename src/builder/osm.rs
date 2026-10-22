//! Streaming OSM build.
//!
//! Memory stays bounded whatever the input size. Ways get their geometry
//! from a node store: the scan marks the nodes kept ways need in a bitmap
//! over the id space, the node pass keeps only their coordinates, in id
//! order, and the way pass reads the kept ways again and finds each node by
//! its rank among the marked ids. Finished records flow through scratch
//! files and external sorts. A region's sorts fit in memory, reaching
//! scratch only once, compressed, as the checkpoints write them; a planet
//! maps its coordinates from scratch when the budget cannot hold them, and
//! spills and merges records through the same code. The budget
//! and the worker threads are sized to the machine before the build starts,
//! and the rest of the build to what each scan counted (see [`plan_build`]).
//!
//! One Pack can cover several extracts. The four PBF passes below run once
//! per input, because the node pass stores coordinates in id order, which
//! only holds within one sorted PBF. Their records all land in the same
//! sorters, so the stages after them -- boundaries, the record write, the
//! seal and the parser training -- run once over everything. That keeps one
//! text index with one set of collection statistics, so BM25 scores stay
//! comparable across regions, which is what searching several countries as
//! one requires.
//!
//! ```text
//! per input:
//! pass 1  scan every block (parallel)     -> records for nodes, needed node ids,
//!                                            boundary relations
//! pass 2  read way blocks                 -> boundary relation member ways
//! pass 3  read node blocks (parallel)     -> coordinates of needed nodes
//! pass 4  reread kept way blocks          -> address, POI, street, interpolation
//!         (parallel)                         records, boundary ways
//!
//! once:
//! order   context records, given their ids; streets joined; addresses
//!         written to the address book, repeated houses found, addresses
//!         placed
//! write   context records, then the rest, each in Hilbert order,
//!         drawing a sample of each country's records
//! seal    text, spatial and parser dictionary indexes into the Pack file
//! train   the address parser on the sample, its weights added to the Pack
//! ```
//!
//! Every step above ends in a checkpoint ([`checkpoint::Step`]): its
//! outputs are durable in scratch and a manifest names them, so a build
//! that stops goes on from the last one (see [`crate::builder::resume`]).
//! Stopping costs the step it stopped in, never the steps before.

mod address;
pub(crate) use address::LOCALITY_KEYS;
mod admin_levels;
mod book;
mod boundary;
mod checkpoint;
mod containment;
mod emit;
mod emitted;
mod geometry;
mod interpolation;
mod locations;
mod pbf;
mod place;
mod poi;
mod postcode;
mod scan;
mod slash;
mod spill;
mod street;
mod tags;

#[cfg(test)]
mod test_pbf;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    fs::{self, File},
    io::{BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use crate::error::{Error, Result};
use rayon::prelude::*;

use crate::{
    alloc,
    builder::{
        addresses::{self, AddressFile, AddressSource},
        plan::{BuildPlan, ItemCounts, Scanned, plan_build_holding},
        progress::{BuildObserver, BuildStage, Progress, TerminalObserver},
        report::{
            BUILD_REPORT_SCHEMA_VERSION, BuilderReport, CheckpointReport, ReportInput,
            ResumeReport, SlashNumberCounts, SorterReport, StageReport,
        },
        resume::{self, FORMAT_VERSION, FileStamp, Fingerprint, Found, Leftover, Manifest},
    },
    extsort::{
        ExternalSorter, IndexedSpool, RunState, Scratch, ScratchFile, Sorted, SorterState, Spill,
        Spool, SpoolFile,
    },
    memory::{Budget, Reservation},
    pack::{
        AUDIT_DIR, BUILD_REPORT_FILE, BuildStats, Generation, REJECTIONS_FILE, RecordContext,
        RecordId, SCRATCH_POINTER_FILE, SealedPack, Sealing, Writer, WriterOptions,
    },
    parse::lattice::{
        CountryCode,
        train::{self, Sampler},
        weights::{WEIGHTS_SECTION, WEIGHTS_VERSION},
    },
    record::{OSM_DATASET, ObjectType, Record, RejectedRecord, StreetRecord},
    records::quantize,
    resources::Resources,
    text_index::PostcodeAreas,
    util::{fold::search_key, geo::point_lon_lat, hilbert::hilbert_key},
};
use book::{Book, BookFile};
pub(crate) use boundary::BOUNDARY_SHAPE_BYTES_PER_COORD;
use boundary::{
    BoundaryCandidate, BoundaryIndex, BoundaryRelationStub, BoundarySet, ContextOrigin, SetItem,
    SourceContext, Vertex, build_boundaries, record_context,
};
use checkpoint::{
    AddressState, AuditState, ContextState, GeometryItem, Id, InputState, KeptWays, Pass,
    PostcodePoint, ScanCounts, State, Step,
};
use emitted::Emitted;
pub(crate) use locations::{
    CHUNK_BYTES as NODE_SET_CHUNK_BYTES, LOCATION_BYTES as NODE_SLOT_BYTES, LoadStats,
    TABLE_BYTES as NODE_SET_TABLE_BYTES,
};
use locations::{ChunkBits, Locations, NodeSet, Slots};
use pbf::for_each_block;
pub use pbf::replication_timestamp;
use scan::scan_block;
use spill::{
    Body, Datasets, House, PendingAddress, PendingRecord, PendingStreet, PlacedAddress, Postcodes,
    WayKind,
};

/// Records written per parallel batch.
const WRITE_BATCH_RECORDS: usize = 16_384;

#[derive(Clone)]
pub struct BuildOsmOptions {
    /// The extracts to build from, in the order they are read. Several make
    /// one Pack covering them all: the passes below run per input and their
    /// records accumulate into one set of indexes, so the Tantivy index and
    /// its collection statistics stay global and scores stay comparable
    /// across every region in the Pack.
    pub inputs: Vec<PathBuf>,
    pub pack: PathBuf,
    /// Memory for the build's buffers; inputs that need more spill to
    /// scratch files. Derived from the memory the process can use when not
    /// set.
    pub memory_budget_bytes: Option<usize>,
    /// Directory for scratch files; defaults to inside the new Pack generation.
    pub scratch_dir: Option<PathBuf>,
    /// The most scratch disk the build may hold at once. The scratch
    /// volume's free space caps it whether set or not.
    pub scratch_budget_bytes: Option<u64>,
    /// Build even when the pre-flight check finds too little memory or disk.
    pub force: bool,
    /// What to do with a build of the same Pack that stopped: refuse, go on
    /// from its last checkpoint, or clear it.
    pub leftover: Leftover,
    /// What the inputs are, for example region ids, kept in the build
    /// report. One per input, in the same order.
    pub input_sources: Vec<String>,
    /// Told about each stage as it starts, advances and finishes, and about
    /// the plan's warnings.
    pub observer: Arc<dyn BuildObserver>,
    /// Set to stop the build: it then fails with
    /// [`crate::builder::BuildCancelled`] and publishes nothing, keeping its
    /// last checkpoint to resume from.
    pub cancel: Arc<AtomicBool>,
    /// Address points to add to the inputs' own, such as OpenAddresses.
    pub addresses: Option<Arc<dyn AddressSource>>,
    /// How many addresses `addresses` is expected to add, for the plan to
    /// size disk by; 0 when not known before they are fetched.
    pub imported_rows: u64,
}

impl Default for BuildOsmOptions {
    fn default() -> Self {
        Self {
            inputs: Vec::new(),
            pack: PathBuf::new(),
            memory_budget_bytes: None,
            scratch_dir: None,
            scratch_budget_bytes: None,
            force: false,
            leftover: Leftover::default(),
            input_sources: Vec::new(),
            observer: Arc::new(TerminalObserver::new()),
            cancel: Arc::new(AtomicBool::new(false)),
            addresses: None,
            imported_rows: 0,
        }
    }
}

impl fmt::Debug for BuildOsmOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BuildOsmOptions")
            .field("inputs", &self.inputs)
            .field("pack", &self.pack)
            .field("memory_budget_bytes", &self.memory_budget_bytes)
            .field("scratch_dir", &self.scratch_dir)
            .field("scratch_budget_bytes", &self.scratch_budget_bytes)
            .field("force", &self.force)
            .field("leftover", &self.leftover)
            .field("input_sources", &self.input_sources)
            .field("cancel", &self.cancel)
            .field("addresses", &self.addresses)
            .finish_non_exhaustive()
    }
}

/// Plan a build of `options` on this machine: probe it, size the budget and
/// the threads, and check the memory and disk the input needs. A build of
/// the Pack that stopped is refused, cleared or counted toward the disk the
/// build has, as `options.leftover` says.
pub fn plan_osm_build(options: &BuildOsmOptions) -> Result<BuildPlan> {
    if options.inputs.is_empty() {
        return Err(Error::BadInput(
            "a build needs at least one input".to_string(),
        ));
    }
    for input in &options.inputs {
        if !input.is_file() {
            return Err(Error::BadInput(format!(
                "input {} does not exist",
                input.display()
            )));
        }
    }
    let held = settle_leftovers(options)?.map_or(0, |found| found.bytes);
    let scratch = options.scratch_dir.as_deref().unwrap_or(&options.pack);
    // The passes run per input but the sorters, the heap the boundary stage
    // holds and the Pack are shared, so the build is sized to every input
    // together.
    plan_build_holding(
        total_input_bytes(&options.inputs)?,
        options.imported_rows,
        scratch,
        &options.pack,
        options.memory_budget_bytes,
        options.scratch_budget_bytes,
        options.force,
        held,
    )
}

/// Refuse or clear the stopped builds of the Pack, as `options.leftover`
/// says: the one to resume, if any.
fn settle_leftovers(options: &BuildOsmOptions) -> Result<Option<Found>> {
    let found = resume::find(&options.pack)?;
    match options.leftover {
        Leftover::Refuse if !found.is_empty() => Err(resume::refusal(&options.pack, &found)),
        Leftover::Refuse => Ok(None),
        Leftover::Clear => {
            for found in &found {
                resume::clear(found)?;
            }
            Ok(None)
        }
        Leftover::Resume => {
            // Nothing can go on from a build that reached no checkpoint, and
            // what it left only takes the disk this one needs.
            let (resumable, unusable): (Vec<_>, Vec<_>) =
                found.into_iter().partition(Found::resumable);
            for found in &unusable {
                resume::clear(found)?;
            }
            for older in resumable.iter().skip(1) {
                options.observer.warned(&format!(
                    "{older} is left as it is: only the last checkpointed build is resumed"
                ));
            }
            Ok(resumable.into_iter().next())
        }
    }
}

/// The bytes of every input together.
fn total_input_bytes(inputs: &[PathBuf]) -> Result<u64> {
    inputs
        .iter()
        .try_fold(0, |total, input| Ok(total + pbf::input_bytes(input)?))
}

pub fn build_osm_pack(options: BuildOsmOptions) -> Result<BuilderReport> {
    let plan = plan_osm_build(&options)?;
    build_planned_osm_pack(options, plan)
}

#[cfg(test)]
thread_local! {
    /// Fails a build once the step's work is done but before its checkpoint
    /// is written, as a crash there would.
    static FAIL_BEFORE_CHECKPOINT: std::cell::Cell<Option<Step>> =
        const { std::cell::Cell::new(None) };
    /// Fails a build after its last checkpoint, as the parser's training
    /// starts.
    static FAIL_BEFORE_TRAINING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Build with a plan made by [`plan_osm_build`]. The caller sizes the
/// process's rayon pool to `plan.threads` before any parallel work, since
/// one pool serves every stage.
pub fn build_planned_osm_pack(options: BuildOsmOptions, plan: BuildPlan) -> Result<BuilderReport> {
    for warning in plan.shortfalls.iter().chain(&plan.warnings) {
        options.observer.warned(warning);
    }
    let mut run = Run::start(&options, plan)?;
    for step in Step::all(options.inputs.len()) {
        if run.done.is_some_and(|done| step <= done) {
            continue;
        }
        run.step(step)?;
        #[cfg(test)]
        if FAIL_BEFORE_CHECKPOINT.get() == Some(step) {
            return Err(Error::Inconsistent(format!(
                "injected failure before the checkpoint of {step:?}"
            )));
        }
        run.checkpoint(step)?;
    }
    run.finish()
}

/// One run of a build: every step from the first, or from the checkpoint
/// a run before this one left.
struct Run<'a> {
    options: &'a BuildOsmOptions,
    plan: BuildPlan,
    /// The most one sorter may hold.
    budget: usize,
    memory: Arc<Budget>,
    scratch: Arc<Scratch>,
    progress: Progress,
    fingerprint: Fingerprint,
    started: Instant,
    /// Build time the runs before this one spent up to its checkpoint.
    elapsed_before: Duration,
    /// The last step a run before this one completed.
    done: Option<Step>,
    /// What every stage of the runs before this one cost.
    stages_before: Vec<StageReport>,
    /// Until the record write takes it.
    generation: Option<Generation>,
    build: Build,
    /// Sorters already read, as the report counts them.
    sorted: BTreeMap<String, SorterReport>,
    scanned: Option<ScanCounts>,
    /// Boundary geometry gathered across inputs, for the boundary stage: a
    /// relation and the ways it is made of can only be assembled once all
    /// of them are known. Each input's part is saved beside it.
    geometry: Geometry,
    geometry_saved: Vec<SpoolFile<GeometryItem>>,
    input: Option<InputPasses>,
    /// From the boundary stage until context ordering consumes it.
    boundaries: Option<BoundarySet>,
    /// The boundary set as saved, until the record write: what the context
    /// index is derived from again.
    boundaries_saved: Option<SpoolFile<SetItem>>,
    /// The boundary shapes' hold on the memory budget, for as long as they
    /// are held: they cannot spill.
    boundary_memory: Option<Reservation>,
    imports: Option<Vec<AddressFile>>,
    contexts: Option<Contexts>,
    /// The addresses in street order, when imported rows are matched
    /// against them before they are placed.
    address_book: Option<BookFile<PendingAddress>>,
    /// What matching them decided, sorted by position in the book.
    verdicts: Option<ExternalSorter<(u64, u64)>>,
    /// The books the placed addresses are read back from as they are
    /// written.
    books: Option<AddressBook>,
    postcodes: Option<SpoolFile<PostcodePoint>>,
    sample: Option<Sampler>,
    sample_saved: Option<ScratchFile>,
    sealing: Option<Sealing>,
    sealed: Option<(SealedPack, BuildStats)>,
    /// When the Pack's indexes started, in this run.
    indexing: Option<Instant>,
    checkpoints: CheckpointReport,
}

/// Context records in their order, the ids they were given, and what
/// the index derived from them answers.
struct Contexts {
    ordered: SpoolFile<PendingRecord>,
    boundary_ids: Vec<RecordId>,
    country_ids: HashMap<String, RecordId>,
    /// The areas that are settlements, by their own tags or by holding a
    /// settlement of their name.
    settled: HashSet<RecordId>,
    index: BoundaryIndex,
    area_names: HashMap<RecordId, Vec<String>>,
    country_codes: HashMap<RecordId, CountryCode>,
    region_groups: HashMap<RecordId, Vec<RecordId>>,
}

impl Contexts {
    /// What the boundary set answers once its records have ids. Cheaper to
    /// derive again from the saved set than to save: it is the polygons
    /// indexed, and names and codes looked up.
    fn derive(
        boundaries: BoundarySet,
        ordered: SpoolFile<PendingRecord>,
        boundary_ids: Vec<RecordId>,
        country_ids: HashMap<String, RecordId>,
        settled: HashSet<RecordId>,
    ) -> Self {
        let country_codes = boundaries.country_codes(&boundary_ids, &country_ids);
        let area_names = boundaries.context_names(&boundary_ids, &country_ids);
        let region_groups = boundaries.region_groups(&boundary_ids);
        let index = boundaries.into_index(&boundary_ids, &country_ids);
        Self {
            ordered,
            boundary_ids,
            country_ids,
            settled,
            index,
            area_names,
            country_codes,
            region_groups,
        }
    }
}

impl<'a> Run<'a> {
    /// A run from the first step, or from the last checkpoint of a build
    /// of the Pack that stopped, when `options.leftover` asks for that.
    fn start(options: &'a BuildOsmOptions, plan: BuildPlan) -> Result<Self> {
        let fingerprint = Fingerprint::of(
            &options.inputs,
            &options.input_sources,
            options
                .addresses
                .as_ref()
                .map(|source| format!("{source:?}")),
        )?;
        match settle_leftovers(options)? {
            Some(found) => Self::resume(options, plan, fingerprint, &found),
            None => {
                if options.leftover == Leftover::Resume {
                    options.observer.warned(&format!(
                        "no stopped build of {} to resume: starting over",
                        options.pack.display()
                    ));
                }
                Self::fresh(options, plan, fingerprint)
            }
        }
    }

    /// The parts every run shares: the memory budget, the scratch it
    /// streams to and the meter.
    fn new(
        options: &'a BuildOsmOptions,
        plan: BuildPlan,
        fingerprint: Fingerprint,
        generation: Generation,
        scratch: Arc<Scratch>,
        build: Build,
    ) -> Self {
        // The sorters and the parser's lattice cache split what the others
        // leave of the pool by fair shares, each at most `budget`; the node
        // store takes what it needs of the rest. While records are written
        // the text index is what every record waits on, so it takes half a
        // sorter's: more indexing threads, fewer segments. The context
        // record and spatial sorters hold a few times less than the others,
        // and are capped to stay so.
        let budget = usize::try_from(plan.sorter_bytes).unwrap_or(usize::MAX);
        scratch.cancel_on(Arc::clone(&options.cancel));
        // Spill files stay in the page cache while they fit in a quarter of
        // what the build leaves free: the rest keeps the Pack and the input
        // it rereads, and dirty pages the lazy writer holds back are not
        // free.
        scratch.set_cache_bytes(
            plan.resources
                .available_memory_bytes
                .saturating_sub(plan.memory_budget_bytes + plan.untracked_bytes)
                / 4,
        );
        let memory = Arc::clone(build.memory());
        let progress = Progress::new(Arc::clone(&options.observer), Arc::clone(&options.cancel))
            .metered(&scratch, &memory);
        Self {
            options,
            plan,
            budget,
            memory,
            scratch,
            progress,
            fingerprint,
            started: Instant::now(),
            elapsed_before: Duration::ZERO,
            done: None,
            stages_before: Vec::new(),
            generation: Some(generation),
            build,
            sorted: BTreeMap::new(),
            scanned: None,
            geometry: Geometry::default(),
            geometry_saved: Vec::new(),
            input: None,
            boundaries: None,
            boundaries_saved: None,
            boundary_memory: None,
            imports: None,
            contexts: None,
            address_book: None,
            verdicts: None,
            books: None,
            postcodes: None,
            sample: None,
            sample_saved: None,
            sealing: None,
            sealed: None,
            indexing: None,
            checkpoints: CheckpointReport::default(),
        }
    }

    fn fresh(
        options: &'a BuildOsmOptions,
        plan: BuildPlan,
        fingerprint: Fingerprint,
    ) -> Result<Self> {
        let memory = budget_of(&plan);
        let report_inputs = options
            .inputs
            .iter()
            .enumerate()
            .map(|(index, input)| {
                Ok(ReportInput {
                    path: input.display().to_string(),
                    source: options.input_sources.get(index).cloned(),
                    replication_timestamp: replication_timestamp(input)?,
                    bytes: pbf::input_bytes(input)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let input_bytes = report_inputs.iter().map(|input| input.bytes).sum::<u64>();
        let mut generation = Generation::create(&options.pack)?;
        let scratch = generation
            .create_scratch(options.scratch_dir.as_deref(), options.scratch_budget_bytes)?;
        // Written first, so a build killed before its first checkpoint is
        // found too, and can be cleared.
        let pointer = generation.path().join(SCRATCH_POINTER_FILE);
        File::create(&pointer)
            .and_then(|mut file| {
                file.write_all(
                    std::path::absolute(scratch.path())?
                        .display()
                        .to_string()
                        .as_bytes(),
                )?;
                file.sync_all()
            })
            .map_err(|source| Error::IoPath {
                action: "failed to write",
                path: pointer,
                source,
            })?;
        let budget = usize::try_from(plan.sorter_bytes).unwrap_or(usize::MAX);
        let mut build = Build {
            report: BuilderReport {
                schema_version: BUILD_REPORT_SCHEMA_VERSION,
                inputs: report_inputs,
                pack: options.pack.display().to_string(),
                input_bytes,
                datasets: vec![OSM_DATASET.to_string()],
                ..BuilderReport::default()
            },
            audit: Audit::create(generation.path())?,
            records: Some(ExternalSorter::new(&scratch, "records", &memory, budget)),
            addresses: Some(ExternalSorter::new(&scratch, "addresses", &memory, budget)),
            streets: Some(ExternalSorter::new(&scratch, "streets", &memory, budget)),
            context_records: Some(ExternalSorter::new(
                &scratch,
                "context-records",
                &memory,
                (budget / 16).max(1),
            )),
            datasets: Datasets::default(),
            seq: 0,
            memory,
        };
        build.report.scratch.memory_budget_bytes = plan.memory_budget_bytes;
        build.report.resources.threads = rayon::current_num_threads();
        build.report.resources.plan = plan.clone();
        Ok(Self::new(
            options,
            plan,
            fingerprint,
            generation,
            scratch,
            build,
        ))
    }

    /// Go on from the checkpoint `found` left: check it was a build of the
    /// same inputs, options and og, reopen its scratch and generation, and
    /// restore what the steps after it need.
    fn resume(
        options: &'a BuildOsmOptions,
        mut plan: BuildPlan,
        mut fingerprint: Fingerprint,
        found: &Found,
    ) -> Result<Self> {
        let manifest = fs::read(found.manifest()).map_err(|source| Error::IoPath {
            action: "failed to read the build checkpoint",
            path: found.manifest(),
            source,
        })?;
        let manifest: Manifest<State> = serde_json::from_slice(&manifest)?;
        if manifest.format != FORMAT_VERSION {
            return Err(Error::BadOptions(
                format!(
                    "the build that stopped in {} wrote a checkpoint of format {}, not {FORMAT_VERSION}; pass --fresh to clear it and start over",
                    found.scratch.display(),
                    manifest.format
                )
                .into(),
            ));
        }
        let differences = manifest.fingerprint.differences(&fingerprint);
        if !differences.is_empty() {
            return Err(Error::BadOptions(
                format!(
                    "the build that stopped after \"{}\" in {} cannot go on, it was a build of something else:\n  {}\npass --fresh to clear it and start over",
                    manifest.stage,
                    found.scratch.display(),
                    differences.join("\n  ")
                )
                .into(),
            ));
        }
        fingerprint.imported = manifest.fingerprint.imported.clone();
        let state = manifest.state;
        let files = manifest
            .files
            .iter()
            .map(|name| found.scratch.join(name))
            .collect::<Vec<_>>();
        let scratch = Scratch::reopen(&found.scratch, &files, state.next_file)?;
        scratch.set_budget(options.scratch_budget_bytes);
        scratch.seed(&state.ledger);
        let mut generation = Generation::reopen(&options.pack, &found.generation);
        generation.attach(&scratch);
        let memory = budget_of(&plan);
        let budget = usize::try_from(plan.sorter_bytes).unwrap_or(usize::MAX);

        // The plan is counted again from what the scans counted, on the
        // machine as it is now.
        if let Some(scanned) = &state.scanned {
            let resources = Resources::probe();
            memory.sample(&resources);
            plan.refine(&Scanned {
                items: scanned.items.clone(),
                read_bytes: scanned.read_bytes,
                heap_bytes: alloc::live_bytes().map(|bytes| bytes as u64),
                tracked_bytes: memory.used() as u64,
                resources,
            })?;
            for warning in plan.shortfalls.iter().chain(&plan.warnings) {
                options.observer.warned(warning);
            }
        }
        let mut report = state.report;
        report.resources.threads = rayon::current_num_threads();
        report.resources.plan = plan.clone();
        report.scratch.memory_budget_bytes = plan.memory_budget_bytes;
        let sorters = &state.sorters;
        let verdicts = sorter_of(sorters, "house-verdicts", &scratch, &memory, budget)?;
        let build = Build {
            audit: Audit::reopen(generation.path(), &state.audit)?,
            records: sorter_of(sorters, "records", &scratch, &memory, budget)?,
            addresses: sorter_of(sorters, "addresses", &scratch, &memory, budget)?,
            streets: sorter_of(sorters, "streets", &scratch, &memory, budget)?,
            context_records: sorter_of(
                sorters,
                "context-records",
                &scratch,
                &memory,
                (budget / 16).max(1),
            )?,
            datasets: Datasets::from_names(state.datasets.clone()),
            seq: state.seq,
            memory,
            report,
        };
        let done = state.step;
        let stages_before = build.report.stages.clone();
        let mut run = Self::new(options, plan, fingerprint, generation, scratch, build);
        run.elapsed_before = Duration::from_millis(manifest.elapsed_ms);
        run.done = Some(done);
        run.stages_before = stages_before;
        run.sorted = state.sorted;
        run.scanned = state.scanned;
        run.imports = state.imports;
        run.checkpoints = run.build.report.checkpoints.clone();
        run.verdicts = verdicts;
        let scratch = Arc::clone(&run.scratch);
        if let Some(addresses) = &state.addresses {
            if let Some(book) = &addresses.book {
                run.address_book = Some(BookFile::restore(&scratch, book)?);
            }
            if let Some((book, postcodes)) = &addresses.placed {
                run.books = Some(AddressBook {
                    addresses: BookFile::restore(&scratch, book)?,
                    postcodes: BookFile::restore(&scratch, postcodes)?,
                    datasets: Datasets::from_names(state.datasets.clone()),
                });
            }
        }
        if done < Step::Boundaries {
            for part in &state.geometry {
                let part = SpoolFile::<GeometryItem>::restore(&scratch, part)?;
                run.geometry.add(part.reader()?)?;
                run.geometry_saved.push(part);
            }
        }
        if let Some(input) = &state.input {
            run.input = Some(InputPasses::restore(&scratch, &run.memory, input)?);
        }
        if let Some(saved) = &state.boundaries {
            let saved = SpoolFile::restore(&scratch, saved)?;
            let boundaries = BoundarySet::load(&saved)?;
            run.boundary_memory = Some(Reservation::forced(&run.memory, boundaries.shape_bytes()));
            match &state.contexts {
                Some(contexts) => {
                    run.contexts = Some(Contexts::derive(
                        boundaries,
                        SpoolFile::restore(&scratch, &contexts.ordered)?,
                        contexts.boundary_ids.clone(),
                        contexts.country_ids.clone().into_iter().collect(),
                        contexts.settled.iter().copied().collect(),
                    ));
                }
                None => run.boundaries = Some(boundaries),
            }
            run.boundaries_saved = Some(saved);
        }
        if let Some(postcodes) = &state.postcodes {
            run.postcodes = Some(SpoolFile::restore(&scratch, postcodes)?);
        }
        if let Some((name, bytes)) = &state.sample {
            let file = ScratchFile::reopen(&scratch, "sample", name, *bytes)?;
            run.sample = Some(Sampler::from_bytes(&fs::read(file.path())?)?);
            run.sample_saved = Some(file);
        }
        if let Some(sealing) = &state.sealing {
            let generation = run.generation.take().expect("a generation until the write");
            run.sealing = Some(Sealing::restore(
                generation,
                Arc::clone(&scratch),
                &run.writer_options(),
                sealing,
            )?);
        }
        if let Some(sealed) = &state.sealed {
            let generation = run.generation.take().expect("a generation until the write");
            run.sealed = Some(SealedPack::restore(generation, sealed)?);
        }
        let resumed = ResumeReport {
            after: manifest.stage,
            reused: scratch
                .traffic()
                .into_iter()
                .filter(|(_, traffic)| traffic.live_bytes > 0)
                .map(|(label, traffic)| (label, traffic.live_bytes))
                .collect(),
            reused_bytes: scratch.live_bytes(),
            saved_ms: manifest.elapsed_ms,
        };
        run.options.observer.resumed(&resumed);
        run.build.report.resumes.push(resumed);
        Ok(run)
    }

    fn writer_options(&self) -> WriterOptions {
        WriterOptions {
            memory: Arc::clone(&self.memory),
            text_index_memory_bytes: (self.budget / 2).max(1),
        }
    }

    fn step(&mut self, step: Step) -> Result<()> {
        match step {
            Step::Input { index, pass } => match pass {
                Pass::Scan => self.scan(index),
                Pass::Members => self.members(index),
                Pass::Nodes => self.nodes(index),
                Pass::Ways => self.ways(index),
            },
            Step::Boundaries => self.boundaries(),
            Step::ImportAddresses => self.import_addresses(),
            Step::Contexts => self.order_contexts(),
            Step::Streets => self.join_streets(),
            Step::AddressBook => self.write_address_book(),
            Step::Houses => self.find_repeated_houses(),
            Step::Addresses => self.place_addresses(),
            Step::Records => self.write_records(),
            Step::Seal(_) => self.seal_part(),
            Step::Sealed => self.seal(),
        }
    }

    /// Pass 1: classify everything, and mark the nodes kept ways reference.
    fn scan(&mut self, index: usize) -> Result<()> {
        let input = &self.options.inputs[index];
        let phase = Instant::now();
        let nodes = NodeSet::new(&self.memory);
        // Nodes whose address tags interpolation ways read.
        let mut tag_ids = Vec::new();
        let mut node_blobs = Vec::new();
        let mut way_blobs = Vec::new();
        // Blobs holding a kept way or a member way: all the way pass rereads.
        let mut kept_blobs = Vec::new();
        let mut kept = KeptWays::default();
        let mut part = Vec::new();
        let build = &mut self.build;
        let geometry = &mut self.geometry;
        let read = for_each_block(
            input,
            &self.progress,
            BuildStage::Scan,
            None,
            |offset, block| {
                let output = scan_block(&block);
                for (_, refs) in &output.ways {
                    nodes.mark(refs)?;
                }
                Ok((offset, output))
            },
            |(offset, output)| {
                if output.has_nodes {
                    node_blobs.push(offset);
                }
                if output.has_ways {
                    way_blobs.push(offset);
                }
                if !output.ways.is_empty() {
                    kept_blobs.push(offset);
                }
                for (feature, refs) in output.ways {
                    let resolution = &mut build.report.geometry_resolution;
                    resolution.required_node_refs += refs.len() as u64;
                    kept.all += 1;
                    kept.records += u64::from(feature.kind != WayKind::Boundary);
                    match feature.kind {
                        WayKind::Address => {
                            resolution.address_way_stubs += 1;
                            kept.addresses += 1;
                        }
                        WayKind::Poi => resolution.poi_way_stubs += 1,
                        WayKind::Street => {
                            resolution.street_way_stubs += 1;
                            kept.streets += 1;
                        }
                        WayKind::Interpolation => {
                            resolution.interpolation_way_stubs += 1;
                            tag_ids.extend_from_slice(&refs);
                        }
                        WayKind::Boundary => {
                            resolution.boundary_way_stubs += 1;
                            build.report.geometry_passes.boundary_refs += refs.len() as u64;
                            geometry.boundary_feature_ids.insert(feature.way_id);
                            part.push(GeometryItem::Feature(feature.way_id));
                        }
                    }
                }
                part.extend(output.relations.iter().cloned().map(GeometryItem::Relation));
                geometry.relations.extend(output.relations);
                build.absorb(output.emitted)
            },
        )?;
        self.build.report.geometry_passes.scan_read_bytes += read;
        self.build.report.phases.scan_ms += phase.elapsed().as_millis();
        self.save_geometry(part)?;
        self.input = Some(InputPasses {
            index,
            nodes: Some(nodes),
            tag_ids,
            node_blobs,
            way_blobs,
            kept_blobs,
            kept,
            members: HashSet::new(),
            locations: None,
            saved: InputSaved::default(),
            changed: true,
        });
        Ok(())
    }

    /// Pass 2: the ways boundary relations are made of. They often carry no
    /// tags of their own, so pass 1 could not know to keep them.
    fn members(&mut self, index: usize) -> Result<()> {
        let input_path = &self.options.inputs[index];
        let phase = Instant::now();
        let input = self.input.as_mut().ok_or_else(|| missing("the scan"))?;
        let nodes = input
            .nodes
            .as_ref()
            .ok_or_else(|| missing("the node set"))?;
        let member_ids = boundary::required_boundary_way_ids(&self.geometry.relations)
            .into_iter()
            .filter(|way_id| !self.geometry.boundary_feature_ids.contains(way_id))
            .collect::<HashSet<_>>();
        let build = &mut self.build;
        if member_ids.is_empty() {
            self.progress.start(BuildStage::BoundaryMembers, Some(0))?;
            self.progress.finish(BuildStage::BoundaryMembers);
        } else {
            let members = &mut input.members;
            let kept_blobs = &mut input.kept_blobs;
            let read = for_each_block(
                input_path,
                &self.progress,
                BuildStage::BoundaryMembers,
                Some(&input.way_blobs),
                |offset, block| Ok((offset, member_ways(&block, &member_ids))),
                |(offset, found)| {
                    if !found.is_empty() {
                        kept_blobs.push(offset);
                    }
                    for (way_id, refs) in found {
                        // A way the input repeats is read where it first
                        // appears.
                        if !members.insert(way_id) {
                            continue;
                        }
                        nodes.mark(&refs)?;
                        let refs = refs.len() as u64;
                        build.report.geometry_resolution.required_node_refs += refs;
                        build.report.geometry_passes.boundary_refs += refs;
                    }
                    Ok(())
                },
            )?;
            build.report.geometry_passes.boundary_member_read_bytes += read;
        }
        build.report.phases.boundary_member_scan_ms += phase.elapsed().as_millis();
        input.changed = true;

        // The scan has counted what the rest of the build holds, and the
        // machine has moved since the build started.
        let resources = Resources::probe();
        self.memory.sample(&resources);
        let accepted = &build.report.accepted;
        let counted = |layer: &str| accepted.by_layer.get(layer).copied().unwrap_or_default();
        let kept = &input.kept;
        let scanned = ScanCounts {
            items: ItemCounts {
                needed_nodes: nodes.len(),
                node_set_bytes: nodes.bytes(),
                kept_ways: build.report.geometry_passes.kept_ways + kept.all,
                records: accepted.total + kept.records,
                addresses: counted("address") + kept.addresses,
                streets: counted("street") + kept.streets,
                boundary_refs: build.report.geometry_passes.boundary_refs,
                imported_rows: self.plan.items.imported_rows,
            },
            read_bytes: total_input_bytes(&self.options.inputs[..=index])?,
        };
        self.plan.refine(&Scanned {
            items: scanned.items.clone(),
            read_bytes: scanned.read_bytes,
            heap_bytes: alloc::live_bytes().map(|bytes| bytes as u64),
            tracked_bytes: self.memory.used() as u64,
            resources: resources.clone(),
        })?;
        for warning in self.plan.shortfalls.iter().chain(&self.plan.warnings) {
            self.options.observer.warned(warning);
        }
        build.report.geometry_passes.kept_ways += kept.all;
        build.report.resources.plan = self.plan.clone();
        self.scanned = Some(scanned);
        Ok(())
    }

    /// Pass 3: the coordinates of every marked node.
    fn nodes(&mut self, index: usize) -> Result<()> {
        let phase = Instant::now();
        let input = self.input.as_mut().ok_or_else(|| missing("the scan"))?;
        let nodes = input.nodes.take().ok_or_else(|| missing("the node set"))?;
        let mut tag_ids = std::mem::take(&mut input.tag_ids);
        tag_ids.sort_unstable();
        tag_ids.dedup();
        let slots = Slots::new(
            nodes.len(),
            self.plan.locations_in_memory,
            &self.memory,
            &self.scratch,
        )?;
        let (locations, stored) = locations::load(
            &self.options.inputs[index],
            &input.node_blobs,
            nodes,
            &tag_ids,
            slots,
            &self.progress,
        )?;
        input.locations = Some(locations);
        // Their saved copies go at the next checkpoint: the store holds
        // what is left of them.
        input.saved.node_set = None;
        input.saved.tag_ids = None;
        self.build.report.geometry_passes.add(&stored);
        self.build.report.phases.node_join_ms += phase.elapsed().as_millis();
        Ok(())
    }

    /// Pass 4: the kept ways and member ways again, now with coordinates.
    fn ways(&mut self, index: usize) -> Result<()> {
        let phase = Instant::now();
        let mut input = self.input.take().ok_or_else(|| missing("the scan"))?;
        let locations = input
            .locations
            .as_ref()
            .ok_or_else(|| missing("the node store"))?;
        input.kept_blobs.sort_unstable();
        input.kept_blobs.dedup();
        // Boundary geometry goes to the input's spool alone: held in memory
        // across a planet's inputs it would outgrow any budget.
        let mut input_members = HashSet::new();
        let mut part = Vec::new();
        let build = &mut self.build;
        let read = for_each_block(
            &self.options.inputs[index],
            &self.progress,
            BuildStage::AssembleWays,
            Some(&input.kept_blobs),
            |_, block| Ok(emit::assemble(&block, locations, &input.members)),
            |assembled| {
                let resolution = &mut build.report.geometry_resolution;
                resolution.resolved_node_refs += assembled.resolved;
                for (way_id, line, found) in assembled.members {
                    if input_members.insert(way_id) {
                        resolution.resolved_node_refs += found;
                        part.push(GeometryItem::Member(way_id, line));
                    }
                }
                part.extend(
                    assembled
                        .output
                        .boundary_ways
                        .into_iter()
                        .map(GeometryItem::Way),
                );
                build.absorb(assembled.output.emitted)
            },
        )?;
        self.build.report.geometry_passes.way_read_bytes += read;
        self.build.report.phases.feature_emission_ms += phase.elapsed().as_millis();
        drop(input);
        self.save_geometry(part)
    }

    /// Keep an input's part of the boundary geometry on disk until the
    /// boundary stage, for a run that goes on after it.
    fn save_geometry(&mut self, part: Vec<GeometryItem>) -> Result<()> {
        let mut spool = Spool::create(&self.scratch, "geometry")?;
        for item in &part {
            spool.write(item)?;
        }
        self.geometry_saved.push(spool.finish()?);
        Ok(())
    }

    /// Admin boundaries and their place records. Every input has been read,
    /// so a relation whose ways span two extracts is assembled from all of
    /// them.
    fn boundaries(&mut self) -> Result<()> {
        let phase = Instant::now();
        self.progress.start(BuildStage::Boundaries, None)?;
        let Geometry {
            relations,
            boundary_feature_ids,
        } = std::mem::take(&mut self.geometry);
        drop(boundary_feature_ids);
        // Every boundary way's and member way's vertices go to one scratch
        // file read back by way id, so only the ids stay in memory. A way
        // with no line (a missing node, or a feature that was incomplete)
        // has none, as a relation needing it then has no shape.
        let mut lines = IndexedSpool::<GeometryItem>::create(&self.scratch, "boundary-lines")?;
        let mut line_at = HashMap::<i64, usize>::new();
        let mut candidates = Vec::new();
        for part in &self.geometry_saved {
            for item in part.reader()? {
                let (way_id, line) = match item? {
                    GeometryItem::Way(way) => {
                        candidates.extend(BoundaryCandidate::of_way(&way));
                        (way.object_id, Some(way.vertices))
                    }
                    GeometryItem::Member(way_id, line) => (way_id, line),
                    GeometryItem::Relation(_) | GeometryItem::Feature(_) => continue,
                };
                line_at.insert(way_id, line_at.len());
                lines.write(&GeometryItem::Member(way_id, line))?;
            }
        }
        let lines = lines.finish()?;
        let member_line = |way_id: i64| -> Result<Option<Vec<Vertex>>> {
            let Some(&at) = line_at.get(&way_id) else {
                return Ok(None);
            };
            match lines.get(at)? {
                GeometryItem::Member(_, line) => Ok(line),
                _ => Err(Error::Decode("a boundary line file holds only lines")),
            }
        };
        let progress = &self.progress;
        let boundaries =
            build_boundaries(candidates, &relations, member_line, || progress.check())?;
        // The shapes stay in memory through the build, so they count against
        // the budget: the sorters spill sooner instead of the build growing
        // past it.
        self.boundary_memory = Some(Reservation::forced(&self.memory, boundaries.shape_bytes()));
        self.build
            .report
            .unmatched_admin_level_ids
            .extend_from_slice(boundaries.unmatched_admin_ids());
        drop(line_at);
        drop(relations);
        for (origin, record) in boundaries.place_records(&mut self.build.report) {
            self.build.push(record, Some(origin))?;
        }
        self.boundaries_saved = Some(boundaries.save(&self.scratch)?);
        self.boundaries = Some(boundaries);
        self.geometry_saved.clear();
        self.progress.finish(BuildStage::Boundaries);
        self.build.report.phases.boundary_build_ms = phase.elapsed().as_millis();
        Ok(())
    }

    /// Addresses from outside OSM join the OSM ones before they are
    /// ordered. The source is asked only now, knowing the countries the
    /// Pack covers.
    fn import_addresses(&mut self) -> Result<()> {
        let phase = Instant::now();
        let boundaries = self
            .boundaries
            .as_ref()
            .ok_or_else(|| missing("the boundaries"))?;
        let files = match &self.options.addresses {
            Some(source) => source.files(&boundaries.coverage())?,
            None => Vec::new(),
        };
        let import_bytes = files
            .iter()
            .map(|file| std::fs::metadata(&file.path).map(|metadata| metadata.len()))
            .sum::<std::io::Result<u64>>()?;
        // A source can cover more than the extracts: a country's file for a
        // Pack of one province. Rows outside every extract's stated bounds
        // are left out; an extract that states none keeps every row.
        let extracts = self
            .options
            .inputs
            .iter()
            .map(|input| pbf::header_bbox(input))
            .collect::<Result<Option<Vec<_>>>>()?;
        let progress = &self.progress;
        progress.start(BuildStage::ImportAddresses, Some(import_bytes))?;
        let build = &mut self.build;
        for file in &files {
            let imported = addresses::read_file(file, |record| {
                progress.check()?;
                let inside = point_lon_lat(&record.geometry).is_some_and(|[lon, lat]| {
                    extracts.as_ref().is_none_or(|boxes| {
                        boxes
                            .iter()
                            .any(|b| (b[0]..=b[2]).contains(&lon) && (b[1]..=b[3]).contains(&lat))
                    })
                });
                if !inside {
                    return Ok(Some("outside_extract"));
                }
                build.push(Record::Address(record), None)?;
                Ok(None)
            })?;
            progress.advance(imported.bytes);
            progress
                .input(&file.path)
                .add(imported.bytes, std::time::Duration::ZERO);
            if !build.report.datasets.contains(&imported.dataset) {
                build.report.datasets.push(imported.dataset.clone());
            }
            build.report.imports.push(imported);
            self.fingerprint.imported.push(FileStamp::of(&file.path)?);
        }
        progress.finish(BuildStage::ImportAddresses);
        build.report.phases.address_import_ms = phase.elapsed().as_millis();
        self.imports = Some(files);
        Ok(())
    }

    /// Context records first: once their ids are known, the boundary index
    /// can describe every other record, and tell the country each address
    /// lies in.
    fn order_contexts(&mut self) -> Result<()> {
        let phase = Instant::now();
        let boundaries = self
            .boundaries
            .take()
            .ok_or_else(|| missing("the boundaries"))?;
        let context_records = self
            .build
            .context_records
            .take()
            .ok_or_else(|| missing("the context records"))?;
        self.sorted.insert(
            "context_records".to_string(),
            sorter_report(&context_records),
        );
        let mut ordered = Spool::<PendingRecord>::create(&self.scratch, "context-ordered")?;
        let mut boundary_ids = vec![0; boundaries.len()];
        let mut country_ids = HashMap::new();
        // The areas that are settlements by their own tags, and the
        // settlement points that may name the area they lie in.
        let mut settled = HashSet::new();
        let mut settlements = Vec::new();
        // Context records are the first the Pack holds.
        for (index, item) in context_records.finish()?.enumerate() {
            let item = item?;
            let record_id = index as u64;
            match &item.origin {
                Some(ContextOrigin::Boundary(boundary)) => {
                    boundary_ids[*boundary as usize] = record_id;
                }
                Some(ContextOrigin::DerivedCountry(code)) => {
                    country_ids.insert(code.clone(), record_id);
                }
                None => {}
            }
            if let Body::Record(record) = &item.body
                && let Record::Place(_, place) = record.as_ref()
                && street::is_settlement(&place.place_type)
            {
                match item.origin {
                    Some(ContextOrigin::Boundary(_)) => {
                        settled.insert(record_id);
                    }
                    Some(ContextOrigin::DerivedCountry(_)) => {}
                    None => settlements
                        .extend(search_key(&place.name).zip(point_lon_lat(&place.geometry))),
                }
            }
            ordered.write(&item)?;
        }
        let mut contexts = Contexts::derive(
            boundaries,
            ordered.finish()?,
            boundary_ids,
            country_ids,
            settled,
        );
        // An admin area is a settlement too when a settlement of its name
        // lies in it, as a town's boundary holds the town's point: Nominatim
        // links the two the same way. A ward or a local board holds none of
        // its name.
        let found = settlements
            .par_iter()
            .filter_map(|(name, [lon, lat])| {
                let locality = contexts
                    .index
                    .context_for_point(*lon, *lat, Default::default())
                    .admin_context
                    .locality_record_id()?;
                contexts
                    .area_names
                    .get(&locality)?
                    .iter()
                    .any(|area| search_key(area).as_ref() == Some(name))
                    .then_some(locality)
            })
            .collect::<Vec<_>>();
        contexts.settled.extend(found);
        self.contexts = Some(contexts);
        self.build.report.phases.context_record_write_ms = phase.elapsed().as_millis();
        Ok(())
    }

    fn join_streets(&mut self) -> Result<()> {
        let phase = Instant::now();
        let contexts = self
            .contexts
            .as_ref()
            .ok_or_else(|| missing("the contexts"))?;
        let streets = self
            .build
            .streets
            .take()
            .ok_or_else(|| missing("the streets"))?;
        self.sorted
            .insert("streets".to_string(), sorter_report(&streets));
        let records = open(&mut self.build.records, "records")?;
        self.build.report.accepted.streets = join_streets(
            streets,
            &contexts.index,
            &contexts.settled,
            &contexts.area_names,
            records,
            &mut self.build.seq,
        )?;
        self.build.report.phases.street_join_ms = phase.elapsed().as_millis();
        Ok(())
    }

    /// Whether imported rows are matched against the addresses the Pack
    /// already has before they are placed.
    fn imported(&self) -> bool {
        self.imports.as_ref().is_some_and(|files| !files.is_empty())
    }

    /// The addresses in street order, with their slash numbers read, to
    /// the address book, when imported rows are to be matched against
    /// them. Without imported rows they are written as they are placed.
    fn write_address_book(&mut self) -> Result<()> {
        if !self.imported() {
            return Ok(());
        }
        let phase = Instant::now();
        let addresses = self
            .build
            .addresses
            .take()
            .ok_or_else(|| missing("the addresses"))?;
        self.sorted
            .insert("addresses".to_string(), sorter_report(&addresses));
        self.address_book = Some(write_address_book(
            addresses,
            &self.scratch,
            &mut self.build.report.slash_numbers,
        )?);
        self.build.report.phases.address_order_ms += phase.elapsed().as_millis();
        Ok(())
    }

    fn find_repeated_houses(&mut self) -> Result<()> {
        if !self.imported() {
            return Ok(());
        }
        let phase = Instant::now();
        let book = self
            .address_book
            .as_ref()
            .ok_or_else(|| missing("the address book"))?;
        self.verdicts = Some(find_repeated_houses(
            book,
            &self.scratch,
            &self.memory,
            self.budget,
            &self.build.datasets,
            &mut self.build.report,
        )?);
        self.build.report.phases.address_order_ms += phase.elapsed().as_millis();
        Ok(())
    }

    fn place_addresses(&mut self) -> Result<()> {
        let phase = Instant::now();
        let contexts = self
            .contexts
            .as_ref()
            .ok_or_else(|| missing("the contexts"))?;
        let lent = Book::create(&self.scratch, "street-postcodes")?;
        let records = open(&mut self.build.records, "records")?;
        let seq = &mut self.build.seq;
        let (postcodes, book, lent) = match self.address_book.take() {
            None => {
                let addresses = self
                    .build
                    .addresses
                    .take()
                    .ok_or_else(|| missing("the addresses"))?;
                self.sorted
                    .insert("addresses".to_string(), sorter_report(&addresses));
                let mut runs = AddressRuns::new(
                    records,
                    seq,
                    &contexts.index,
                    &contexts.country_codes,
                    lent,
                    Some(Book::create(&self.scratch, "address-book")?),
                );
                // Slash numbers are read before the addresses are placed.
                slash::read_slash_numbers(
                    addresses.finish()?,
                    &mut self.build.report.slash_numbers,
                    |address| runs.push(address, None),
                )?;
                let (postcodes, lent, book) = runs.finish()?;
                let book = book.expect("the addresses were written to the book");
                (postcodes, book.finish()?, lent)
            }
            Some(book) => {
                let verdicts = self
                    .verdicts
                    .take()
                    .ok_or_else(|| missing("the house verdicts"))?;
                let mut runs = AddressRuns::new(
                    records,
                    seq,
                    &contexts.index,
                    &contexts.country_codes,
                    lent,
                    None,
                );
                replay_address_book(&book, verdicts.finish()?, &mut runs)?;
                let (postcodes, lent, _) = runs.finish()?;
                (postcodes, book, lent)
            }
        };
        self.books = Some(AddressBook {
            addresses: book,
            postcodes: lent.finish()?,
            datasets: Datasets::from_names(self.build.datasets.names().to_vec()),
        });
        let report = &mut self.build.report;
        // Every address is placed, so each postcode's points are all known.
        let mut points = Spool::create(&self.scratch, "postcode-points")?;
        for record in postcodes.into_records() {
            report.accept_postcode();
            let [lon, lat] = point_lon_lat(&record.geometry).ok_or_else(|| {
                Error::BadGeometry(format!("postcode {} has no point", record.postcode))
            })?;
            points.write(&PostcodePoint(record.postcode.clone(), [lon, lat]))?;
            let records = open(&mut self.build.records, "records")?;
            records.push(PendingRecord {
                key: hilbert_key(quantize(lon)?, quantize(lat)?),
                seq: self.build.seq,
                origin: None,
                body: Record::Postcode(record).into(),
            })?;
            self.build.seq += 1;
        }
        self.postcodes = Some(points.finish()?);
        self.build.report.phases.address_order_ms += phase.elapsed().as_millis();
        Ok(())
    }

    /// Write context records, then every other record, then the parts of
    /// the Pack the writer held in memory.
    fn write_records(&mut self) -> Result<()> {
        let phase = Instant::now();
        let Contexts {
            ordered,
            index,
            area_names,
            country_codes,
            region_groups,
            ..
        } = self
            .contexts
            .take()
            .ok_or_else(|| missing("the contexts"))?;
        let generation = self
            .generation
            .take()
            .ok_or_else(|| missing("the Pack generation"))?;
        let mut writer =
            Writer::create_in(generation, Arc::clone(&self.scratch), self.writer_options())?;
        writer.set_context_names(area_names);
        writer.set_country_codes(country_codes);
        writer.set_region_groups(region_groups);
        let mut sample = Sampler::default();
        let context_count = ordered.len();
        write_ordered(
            &mut writer,
            &mut sample,
            ordered.into_reader(),
            context_count,
            &index,
            None,
            &self.progress,
            BuildStage::ContextRecords,
        )?;
        let postcodes = self
            .postcodes
            .take()
            .ok_or_else(|| missing("the postcode points"))?;
        let points = postcodes
            .into_reader()
            .map(|point| point.map(|PostcodePoint(postcode, point)| (postcode, point)))
            .collect::<Result<Vec<_>>>()?;
        writer.set_postcode_areas(PostcodeAreas::new(points));
        let records = self
            .build
            .records
            .take()
            .ok_or_else(|| missing("the records"))?;
        let record_stats = sorter_report(&records);
        self.sorted
            .insert("records".to_string(), record_stats.clone());
        let books = self
            .books
            .take()
            .ok_or_else(|| missing("the address book"))?;
        write_ordered(
            &mut writer,
            &mut sample,
            records.finish()?,
            record_stats.items,
            &index,
            Some(&books),
            &self.progress,
            BuildStage::Records,
        )?;
        drop((index, books));
        self.boundaries_saved = None;
        self.build.report.phases.record_write_ms = phase.elapsed().as_millis();
        self.build.report.scratch.peak_tracked_bytes = self.memory.peak() as u64;
        self.progress.start(BuildStage::Indexes, None)?;
        self.indexing = Some(Instant::now());
        self.sealing = Some(writer.into_sealing()?);
        self.sample = Some(sample);
        Ok(())
    }

    /// Start the Pack's index stage, when a run goes on in the middle of it.
    fn indexing(&mut self) -> Result<()> {
        if self.indexing.is_none() {
            self.progress.start(BuildStage::Indexes, None)?;
            self.indexing = Some(Instant::now());
        }
        Ok(())
    }

    fn seal_part(&mut self) -> Result<()> {
        self.indexing()?;
        self.sealing
            .as_mut()
            .ok_or_else(|| missing("the sealing"))?
            .seal_part()?;
        Ok(())
    }

    fn seal(&mut self) -> Result<()> {
        self.indexing()?;
        let sealing = self.sealing.take().ok_or_else(|| missing("the sealing"))?;
        self.sealed = Some(sealing.finish()?);
        self.progress.finish(BuildStage::Indexes);
        let started = self.indexing.take().expect("indexing started");
        self.build.report.phases.pack_seal_ms += started.elapsed().as_millis();
        Ok(())
    }

    /// Save what the steps after `step` need and name it in the manifest,
    /// then make that the checkpoint a stopped build goes on from.
    fn checkpoint(&mut self, step: Step) -> Result<()> {
        let started = Instant::now();
        let scratch = Arc::clone(&self.scratch);
        let mut files = Vec::new();
        let mut sorters = Vec::new();
        save_sorter(&mut self.build.records, &mut sorters)?;
        save_sorter(&mut self.build.addresses, &mut sorters)?;
        save_sorter(&mut self.build.streets, &mut sorters)?;
        save_sorter(&mut self.build.context_records, &mut sorters)?;
        save_sorter(&mut self.verdicts, &mut sorters)?;
        for sorter in &sorters {
            files.extend(sorter.files(&scratch));
        }
        let mut addresses = AddressState::default();
        if let Some(book) = &mut self.address_book {
            addresses.book = Some(book.state(&scratch)?);
            files.extend(book.files());
        }
        if let Some(books) = &mut self.books {
            addresses.placed = Some((
                books.addresses.state(&scratch)?,
                books.postcodes.state(&scratch)?,
            ));
            files.extend(books.addresses.files());
            files.extend(books.postcodes.files());
        }
        let geometry = self
            .geometry_saved
            .iter()
            .map(SpoolFile::state)
            .collect::<Vec<_>>();
        let input = match &mut self.input {
            Some(input) => {
                let (state, input_files) = input.save(&scratch)?;
                files.extend(input_files);
                Some(state)
            }
            None => None,
        };
        let contexts = self.contexts.as_ref().map(|contexts| ContextState {
            ordered: contexts.ordered.state(),
            boundary_ids: contexts.boundary_ids.clone(),
            country_ids: contexts
                .country_ids
                .iter()
                .map(|(code, id)| (code.clone(), *id))
                .collect(),
            settled: {
                let mut settled = contexts.settled.iter().copied().collect::<Vec<_>>();
                settled.sort_unstable();
                settled
            },
        });
        if let (Some(sample), None) = (&self.sample, &self.sample_saved) {
            let mut file = ScratchFile::create(&scratch, "sample", "sample.bin")?;
            file.write_all(&sample.to_bytes())?;
            file.flush()?;
            self.sample_saved = Some(file);
        }
        let sealing = self.sealing.as_mut().map(Sealing::state).transpose()?;
        if let Some(sealing) = &self.sealing {
            files.extend(sealing.files());
        }
        for state in &geometry {
            files.extend(state.files(&scratch));
        }
        let boundaries = self.boundaries_saved.as_ref().map(SpoolFile::state);
        let postcodes = self.postcodes.as_ref().map(SpoolFile::state);
        for state in boundaries
            .iter()
            .chain(postcodes.iter())
            .chain(contexts.iter().map(|contexts| &contexts.ordered))
        {
            files.extend(state.files(&scratch));
        }
        if let Some(file) = &self.sample_saved {
            files.push(file.path().to_path_buf());
        }
        let stage = step.describe(self.options.inputs.len());
        self.checkpoints.count += 1;
        self.checkpoints.last = Some(stage.clone());
        let mut report = self.report();
        report.checkpoints = self.checkpoints.clone();
        let state = State {
            step,
            seq: self.build.seq,
            datasets: self.build.datasets.names().to_vec(),
            addresses: (addresses != AddressState::default()).then_some(addresses),
            audit: self.build.audit.state()?,
            sorters,
            sorted: self.sorted.clone(),
            next_file: scratch.next_file(),
            ledger: scratch.ledger(),
            scanned: self.scanned.clone(),
            geometry,
            input,
            boundaries,
            imports: self.imports.clone(),
            contexts,
            postcodes,
            sample: self
                .sample_saved
                .as_ref()
                .map(|file| (file.name(), file.bytes())),
            sealing,
            sealed: self
                .sealed
                .as_ref()
                .map(|(sealed, stats)| sealed.state(stats)),
            report,
        };
        let manifest = Manifest {
            format: FORMAT_VERSION,
            stage,
            fingerprint: self.fingerprint.clone(),
            elapsed_ms: self.elapsed().as_millis() as u64,
            files: files
                .iter()
                .filter_map(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .collect(),
            state,
        };
        scratch.commit(&files, &serde_json::to_vec(&manifest)?)?;
        self.checkpoints.wall_ms += started.elapsed().as_millis() as u64;
        Ok(())
    }

    /// The build's time, every run's up to this one's checkpoint together.
    fn elapsed(&self) -> Duration {
        self.elapsed_before + self.started.elapsed()
    }

    /// The report as it stands, every run's stages in it.
    fn report(&self) -> BuilderReport {
        let mut report = self.build.report.clone();
        report.stages = self.stages_before.clone();
        report.stages.extend(self.progress.stages());
        report
    }

    /// Train the parser on the sealed Pack and publish it.
    fn finish(mut self) -> Result<BuilderReport> {
        let phase = Instant::now();
        let (mut sealed, mut stats) = self.sealed.take().ok_or_else(|| missing("the Pack"))?;
        let sample = self.sample.take().ok_or_else(|| missing("the sample"))?;
        #[cfg(test)]
        if FAIL_BEFORE_TRAINING.get() {
            return Err(Error::Inconsistent(
                "injected failure before the training".into(),
            ));
        }
        // The parser learns from the sealed Pack's dictionaries and the
        // records drawn while they were written, and its weights join the
        // Pack before it is published.
        self.progress.start(BuildStage::Parser, None)?;
        // By now the build has used memory and page cache a fresh process
        // would have: sample again what is left.
        let resources = Resources::probe();
        self.memory.sample(&resources);
        self.build.report.resources.at_training = resources;
        let (weights, training) = train::train(
            &sealed.path(),
            sample,
            &self.scratch,
            &self.memory,
            self.budget,
        )?;
        if let Some(weights) = weights {
            let bytes = weights.to_bytes();
            sealed.add_section(WEIGHTS_SECTION, WEIGHTS_VERSION, &bytes)?;
            stats.bytes = sealed.bytes();
            stats
                .sections
                .insert(WEIGHTS_SECTION.to_string(), bytes.len() as u64);
        }
        let scratch = Arc::clone(&self.scratch);
        // Done with every checkpoint: a build that fails from here on goes
        // on from nothing, at the cost of training again.
        self.sample_saved = None;
        scratch.complete();
        let report = &mut self.build.report;
        report.scratch.spilled_bytes = scratch.spilled_bytes();
        report.scratch.budget_bytes = scratch.budget();
        report.scratch.peak_live_bytes = scratch.peak_bytes();
        report.scratch.peak_owners = scratch.peak_owners();
        report.scratch.zstd_level = scratch.level();
        report.scratch.spills = scratch.traffic();
        self.checkpoints.peak_retained_bytes = scratch.peak_retained_bytes();
        self.checkpoints.voided = scratch.voided();
        self.progress.finish(BuildStage::Parser);
        let mut report = self.report();
        report.checkpoints = self.checkpoints.clone();
        self.progress.check()?;
        report.parser_training = training;
        report.phases.parser_training_ms = phase.elapsed().as_millis();
        for (name, sorted) in &self.sorted {
            report.scratch.sorters.insert(name.clone(), sorted.clone());
        }
        report.output.pack_bytes = stats.bytes;
        report.output.record_count = sealed.manifest().record_count;
        report.output.text_index_bytes = stats.text_index_bytes;
        report.output.sections = stats.sections;
        report.output.rejection_count = self.build.audit.count;
        report.scratch.peak_heap_bytes = alloc::peak_bytes().unwrap_or_default() as u64;
        report.phases.total_ms = self.elapsed().as_millis();
        report.finalize_throughput();
        let Self {
            build: Build { audit, .. },
            ..
        } = self;
        audit.finish(&report)?;
        let pointer = sealed.generation().join(SCRATCH_POINTER_FILE);
        fs::remove_file(&pointer).map_err(|source| Error::IoPath {
            action: "failed to remove",
            path: pointer,
            source,
        })?;
        drop(scratch);
        sealed.publish()?;
        Ok(report)
    }
}

/// Write what `sorter` buffers as a run, and name its runs in `states`.
fn save_sorter<T: Spill + Ord + Send + Sync + 'static>(
    sorter: &mut Option<ExternalSorter<T>>,
    states: &mut Vec<SorterState>,
) -> Result<()> {
    if let Some(sorter) = sorter {
        states.push(sorter.checkpoint()?);
    }
    Ok(())
}

/// The sorter labelled `label` a checkpoint named, if it did.
fn sorter_of<T: Spill + Ord + Send + Sync + 'static>(
    states: &[SorterState],
    label: &str,
    scratch: &Arc<Scratch>,
    memory: &Arc<Budget>,
    share: usize,
) -> Result<Option<ExternalSorter<T>>> {
    states
        .iter()
        .find(|state| state.label == label)
        .map(|state| ExternalSorter::restore(scratch, memory, share, state))
        .transpose()
}

/// The memory budget a plan sizes.
fn budget_of(plan: &BuildPlan) -> Arc<Budget> {
    let memory = Budget::new(usize::try_from(plan.memory_budget_bytes).unwrap_or(usize::MAX));
    memory.sample(&plan.resources);
    memory
}

fn sorter_report<T: Spill + Ord + Send + Sync + 'static>(
    sorter: &ExternalSorter<T>,
) -> SorterReport {
    let stats = sorter.stats();
    SorterReport {
        items: stats.items,
        runs: stats.runs,
    }
}

/// The sorter in `sorter`, unless a step read it already.
fn open<'s, T>(
    sorter: &'s mut Option<ExternalSorter<T>>,
    label: &str,
) -> Result<&'s mut ExternalSorter<T>> {
    sorter
        .as_mut()
        .ok_or_else(|| Error::Inconsistent(format!("the {label} sorter was read already")))
}

/// A step ran before the one that makes what it needs.
fn missing(what: &str) -> Error {
    Error::Inconsistent(format!("{what} is missing: a build step ran out of order"))
}

/// Build state shared by the passes.
struct Build {
    report: BuilderReport,
    audit: Audit,
    /// POIs, interpolations and streets, then the addresses.
    records: Option<ExternalSorter<PendingRecord>>,
    /// Addresses, until every one is known and they can join `records` a
    /// street at a time.
    addresses: Option<ExternalSorter<PendingAddress>>,
    /// The ways of streets, until every one is known and the ways of one
    /// street can join (see [`join_streets`]).
    streets: Option<ExternalSorter<PendingStreet>>,
    /// Places: everything a context tuple can point at.
    context_records: Option<ExternalSorter<PendingRecord>>,
    /// The datasets of the imported addresses.
    datasets: Datasets,
    seq: u64,
    memory: Arc<Budget>,
}

impl Build {
    fn memory(&self) -> &Arc<Budget> {
        &self.memory
    }

    /// Merge one worker's output, in input order.
    fn absorb(&mut self, emitted: Emitted) -> Result<()> {
        let Emitted {
            report,
            records,
            rejections,
        } = emitted;
        self.report.merge(report);
        for rejection in &rejections {
            self.audit.write(rejection)?;
        }
        for record in records {
            self.push(record, None)?;
        }
        Ok(())
    }

    fn push(&mut self, record: Record, origin: Option<ContextOrigin>) -> Result<()> {
        if let Record::Address(address) = record {
            let dataset = match address.source.object_type {
                ObjectType::Row => self.datasets.id(&address.source.dataset),
                _ => 0,
            };
            self.seq += 1;
            return open(&mut self.addresses, "addresses")?.push(PendingAddress::new(
                address,
                self.seq - 1,
                dataset,
            )?);
        }
        if let Record::Street(street) = record {
            self.seq += 1;
            return open(&mut self.streets, "streets")?
                .push(PendingStreet::new(street, self.seq - 1)?);
        }
        let [lon, lat] = record.display_point().ok_or_else(|| {
            Error::BadGeometry(format!("record {} has no display point", record.id()))
        })?;
        let context = record.layer().is_context();
        let pending = PendingRecord {
            key: hilbert_key(quantize(lon)?, quantize(lat)?),
            seq: self.seq,
            origin,
            body: record.into(),
        };
        self.seq += 1;
        if context {
            open(&mut self.context_records, "context records")?.push(pending)
        } else {
            open(&mut self.records, "records")?.push(pending)
        }
    }
}

/// Addresses on their way into the record sorter, in street order: each
/// group placed in the Hilbert order by its first address, so the group
/// stays one run for the record store and the text index to share what its
/// addresses state; lent its street's postcodes when it states none; and
/// counted toward its postcode's point in the country it lies in. The record
/// sorter takes where each waits in the address book, not the address.
struct AddressRuns<'a> {
    records: &'a mut ExternalSorter<PendingRecord>,
    seq: &'a mut u64,
    index: &'a BoundaryIndex,
    country_codes: &'a HashMap<RecordId, CountryCode>,
    street: postcode::Street,
    postcodes: postcode::Accumulator,
    /// The postcodes lent to each street, and the street key and position
    /// of the last street's.
    lent: Book<Postcodes>,
    lent_street: Option<(Vec<u8>, u64)>,
    /// Where the addresses not yet in an address book are written as they
    /// are placed.
    book: Option<Book<PendingAddress>>,
    /// Each address, and where it is in the book and whether it is disputed
    /// when it is already there.
    batch: Vec<(PendingAddress, Option<(u64, bool)>)>,
    /// The group key of the last address placed, its Hilbert key, and the
    /// country it lies in.
    group: Option<(Vec<u8>, u64, Option<CountryCode>)>,
}

impl<'a> AddressRuns<'a> {
    fn new(
        records: &'a mut ExternalSorter<PendingRecord>,
        seq: &'a mut u64,
        index: &'a BoundaryIndex,
        country_codes: &'a HashMap<RecordId, CountryCode>,
        lent: Book<Postcodes>,
        book: Option<Book<PendingAddress>>,
    ) -> Self {
        Self {
            records,
            seq,
            index,
            country_codes,
            street: postcode::Street::default(),
            postcodes: postcode::Accumulator::default(),
            lent,
            lent_street: None,
            book,
            batch: Vec::new(),
            group: None,
        }
    }

    /// Place `address`: `shelved` says where it is in the address book and
    /// whether it is disputed, or it is written to this one's book.
    fn push(&mut self, address: PendingAddress, shelved: Option<(u64, bool)>) -> Result<()> {
        self.batch.push((address, shelved));
        if self.batch.len() == WRITE_BATCH_RECORDS {
            self.place_batch()?;
        }
        Ok(())
    }

    /// The postcode points, the street postcodes lent, and the book the
    /// addresses were written to, if they were.
    fn finish(
        mut self,
    ) -> Result<(
        postcode::Accumulator,
        Book<Postcodes>,
        Option<Book<PendingAddress>>,
    )> {
        self.place_batch()?;
        Ok((self.postcodes, self.lent, self.book))
    }

    fn place_batch(&mut self) -> Result<()> {
        let batch = std::mem::take(&mut self.batch);
        let starts = batch
            .iter()
            .enumerate()
            .map(|(at, (address, _))| match at {
                0 => self
                    .group
                    .as_ref()
                    .is_none_or(|(key, ..)| key[..] != *address.group_key()),
                _ => !batch[at - 1].0.same_group(address),
            })
            .collect::<Vec<_>>();
        // One lookup per group, not per address: a group is one street in
        // one cell, which a border runs through too rarely to matter. Only
        // a group stating a postcode needs its country.
        let countries = batch
            .par_iter()
            .zip(&starts)
            .map(|((address, _), start)| {
                let fields = address.fields();
                if !*start || fields.postcode.is_none() {
                    return None;
                }
                let [lon, lat] = address.point;
                let source = SourceContext {
                    country: fields.country,
                    region: fields.region,
                    locality: fields.locality,
                    neighbourhood: fields.areas.last().copied(),
                    place: fields.place,
                    ..SourceContext::default()
                };
                let context = self.index.context_for_point(lon, lat, source);
                let country = context.admin_context.country_record_id()?;
                self.country_codes.get(&country).copied()
            })
            .collect::<Vec<_>>();
        for (((address, shelved), start), country) in batch.into_iter().zip(starts).zip(countries) {
            if start {
                let [lon, lat] = address.point;
                self.group = Some((
                    address.group_key().to_vec(),
                    hilbert_key(quantize(lon)?, quantize(lat)?),
                    country,
                ));
            }
            let (_, key, country) = self.group.as_ref().expect("a group has a start");
            let (key, country) = (*key, *country);
            let fields = address.fields();
            let postcodes = match self.street.lend(address.street_key(), &fields) {
                Some(lent) => Some(match &self.lent_street {
                    Some((street, at)) if street[..] == *address.street_key() => *at,
                    _ => {
                        let at = self.lent.push(Postcodes(lent.to_vec()))?;
                        self.lent_street = Some((address.street_key().to_vec(), at));
                        at
                    }
                }),
                None => None,
            };
            self.postcodes
                .accept(country, fields.postcode, address.point);
            let (at, disputed) = match shelved {
                Some(shelved) => shelved,
                None => (
                    self.book
                        .as_mut()
                        .expect("an address not in a book has one to go to")
                        .push(address)?,
                    false,
                ),
            };
            self.records.push(PendingRecord {
                key,
                seq: *self.seq,
                origin: None,
                body: Body::Address(PlacedAddress {
                    at,
                    postcodes,
                    disputed,
                }),
            })?;
            *self.seq += 1;
        }
        Ok(())
    }
}

/// The addresses in street order and the postcodes lent to their streets,
/// read back as the records standing for them are written.
struct AddressBook {
    addresses: BookFile<PendingAddress>,
    postcodes: BookFile<Postcodes>,
    datasets: Datasets,
}

impl AddressBook {
    /// The records of `batch`, each address read from the book. A batch
    /// comes in record order, where an address group is one run, so each
    /// chunk it needs is read once.
    fn records(&self, batch: Vec<PendingRecord>) -> Result<Vec<(Record, Option<ContextOrigin>)>> {
        let mut address_chunks = Vec::new();
        let mut postcode_chunks = Vec::new();
        for pending in &batch {
            if let Body::Address(placed) = &pending.body {
                address_chunks.push(BookFile::<PendingAddress>::chunk_of(placed.at));
                postcode_chunks.extend(placed.postcodes.map(BookFile::<Postcodes>::chunk_of));
            }
        }
        let addresses = read_chunks(&self.addresses, address_chunks)?;
        let postcodes = read_chunks(&self.postcodes, postcode_chunks)?;
        batch
            .into_par_iter()
            .map(|pending| {
                let record = match pending.body {
                    Body::Record(record) => *record,
                    Body::Address(placed) => {
                        let chunk = &addresses[&BookFile::<PendingAddress>::chunk_of(placed.at)];
                        let mut record =
                            BookFile::pick(chunk, placed.at)?.record(&self.datasets)?;
                        record.disputed = placed.disputed;
                        if let Some(at) = placed.postcodes {
                            let chunk = &postcodes[&BookFile::<Postcodes>::chunk_of(at)];
                            record.street_postcodes = BookFile::pick(chunk, at)?.0.clone();
                        }
                        Record::Address(record)
                    }
                };
                Ok((record, pending.origin))
            })
            .collect()
    }
}

/// The chunks of `book` numbered in `chunks`, each read once.
fn read_chunks<T: book::Entry + Send + Sync>(
    book: &BookFile<T>,
    mut chunks: Vec<usize>,
) -> Result<HashMap<usize, Vec<T>>> {
    chunks.sort_unstable();
    chunks.dedup();
    chunks
        .into_par_iter()
        .map(|index| Ok((index, book.chunk(index)?)))
        .collect()
}

/// The ways of each street name, joined into one street record per street
/// of each area and pushed to `records`; how many streets that makes. OSM
/// draws a road as a way per stretch between its junctions, so one road was
/// as many answers as it has stretches, told apart only by their ids.
///
/// Ways join within the area their street is labelled with: the locality
/// they lie in when that is a settlement (see `settled`), else its
/// district, so a street in a city's ward or local board is named by the
/// city and joins across the wards; else the locality. One name is many
/// streets across a country, and a road crossing a town line is a street in
/// each town. `area_names` are the names of each area, its own first.
fn join_streets(
    streets: ExternalSorter<PendingStreet>,
    index: &BoundaryIndex,
    settled: &HashSet<RecordId>,
    area_names: &HashMap<RecordId, Vec<String>>,
    records: &mut ExternalSorter<PendingRecord>,
    seq: &mut u64,
) -> Result<u64> {
    let mut count = 0;
    let mut flush = |runs: &mut Vec<Vec<StreetRecord>>| -> Result<()> {
        let joined = runs
            .par_drain(..)
            .flat_map_iter(|run| {
                let mut areas = Vec::<(Option<RecordId>, Option<String>, Vec<StreetRecord>)>::new();
                for way in run {
                    let [lon, lat] = way.representative_point;
                    let context = index.context_for_point(lon, lat, Default::default());
                    let tuple = &context.admin_context;
                    let locality = tuple.locality_record_id();
                    let area = locality
                        .filter(|locality| settled.contains(locality))
                        .or(tuple.district_record_id())
                        .or(locality);
                    match areas.iter_mut().find(|(at, ..)| *at == area) {
                        Some((.., members)) => members.push(way),
                        None => {
                            let named =
                                area.and_then(|area| area_names.get(&area)?.first().cloned());
                            areas.push((area, named, vec![way]));
                        }
                    }
                }
                areas.into_iter().flat_map(|(_, named, members)| {
                    street::join_ways(members)
                        .into_iter()
                        .map(move |mut street| {
                            street.locality.clone_from(&named);
                            street
                        })
                })
            })
            .collect::<Vec<_>>();
        for street in joined {
            let [lon, lat] = street.representative_point;
            records.push(PendingRecord {
                key: hilbert_key(quantize(lon)?, quantize(lat)?),
                seq: *seq,
                origin: None,
                body: Record::Street(street).into(),
            })?;
            *seq += 1;
            count += 1;
        }
        Ok(())
    };
    // Whole names at a time, a batch of ways or so per parallel join.
    let mut runs = Vec::<Vec<StreetRecord>>::new();
    let mut ways = 0;
    let mut name = None::<Vec<u8>>;
    for pending in streets.finish()? {
        let pending = pending?;
        if name.as_deref() != Some(pending.name_key()) {
            if ways >= WRITE_BATCH_RECORDS {
                flush(&mut runs)?;
                ways = 0;
            }
            name = Some(pending.name_key().to_vec());
            runs.push(Vec::new());
        }
        ways += 1;
        runs.last_mut()
            .expect("a run was started")
            .push(pending.into_street());
    }
    flush(&mut runs)?;
    Ok(count)
}

/// An imported address within this distance of an earlier address of the
/// same house (street, number and unit) repeats it. Sources place one house a
/// few metres apart, at the door, the parcel or the building's middle.
const REPEATED_HOUSE_RADIUS_M: f64 = 100.0;

/// An imported address farther than [`REPEATED_HOUSE_RADIUS_M`] from a kept
/// address of the same house, but within this, still names that house: the
/// sources disagree on where it is. Below half the 2 km street-group cell, so
/// it never joins what the cell grid keeps apart as two runs of one street,
/// and far below the spacing of the places a street name repeats in (two
/// villages' Hauptstrasse, a rural road crossing a county line where the
/// numbers start over). A large rural parcel's centroid lies a few hundred
/// metres from its house; a row geocoded to the wrong end of a street or to
/// another town does not, and is kept as the house it says it is.
const DISPUTED_HOUSE_RADIUS_M: f64 = 1_000.0;

/// What finding repeated houses decided about an address: dropped, or kept
/// and disputed. Sorted after its position in the address book.
const DROPPED: u64 = 0;
const DISPUTED: u64 = 1;

/// Positions in the address book, each with what was decided about it.
type Verdicts = Sorted<(u64, u64)>;

/// The addresses in street order, with their slash numbers read, written
/// to an address book.
fn write_address_book(
    addresses: ExternalSorter<PendingAddress>,
    scratch: &Arc<Scratch>,
    slash_numbers: &mut SlashNumberCounts,
) -> Result<BookFile<PendingAddress>> {
    let mut book = Book::create(scratch, "address-book")?;
    slash::read_slash_numbers(addresses.finish()?, slash_numbers, |address| {
        book.push(address).map(drop)
    })?;
    book.finish()
}

/// Place the addresses of `book` that `verdicts` keeps, in street order.
fn replay_address_book(
    book: &BookFile<PendingAddress>,
    verdicts: Verdicts,
    runs: &mut AddressRuns,
) -> Result<()> {
    let mut verdicts = verdicts.peekable();
    for (at, address) in book.items().enumerate() {
        let (address, at) = (address?, at as u64);
        let (mut dropped, mut disputed) = (false, false);
        while let Some(verdict) =
            verdicts.next_if(|verdict| !matches!(verdict, Ok((position, _)) if *position != at))
        {
            match verdict?.1 {
                DROPPED => dropped = true,
                DISPUTED => disputed = true,
                _ => return Err(Error::Decode("unknown house verdict")),
            }
        }
        if !dropped {
            runs.push(address, Some((at, disputed)))?;
        }
    }
    Ok(())
}

/// Sorted by position in the address book, which addresses are imported
/// rows repeating an address the Pack already has, OSM ones first, and
/// which kept ones a row disputes. The houses are found in house order,
/// from a hash of each house and its point, not the address. Each dropped
/// row is counted on its file.
///
/// One record answers for one house. A row within
/// [`REPEATED_HOUSE_RADIUS_M`] of a kept address of its house agrees with it
/// and is dropped. One beyond that, but within [`DISPUTED_HOUSE_RADIUS_M`] of
/// a kept address it does not contradict, is dropped too: the kept address
/// keeps its point, OSM's when OSM has the house, and is marked
/// [`crate::record::AddressRecord::disputed`]. Only a row farther still, or
/// one stating another postcode, locality or area than every kept address
/// near it, is a house of its own. Two houses of one street name and number
/// lie apart by the distance between the places that repeat the name, which
/// the radius stays below; where they lie closer, the postcode or the
/// locality tells them apart.
fn find_repeated_houses(
    book: &BookFile<PendingAddress>,
    scratch: &Arc<Scratch>,
    memory: &Arc<Budget>,
    budget: usize,
    datasets: &Datasets,
    report: &mut BuilderReport,
) -> Result<ExternalSorter<(u64, u64)>> {
    // Read from the book once the street order's runs are gone, so the
    // houses never share the disk with them.
    let mut houses = ExternalSorter::<House>::new(scratch, "houses", memory, budget);
    for (at, address) in book.items().enumerate() {
        houses.push(address?.house(at as u64))?;
    }
    let mut verdicts = ExternalSorter::new(scratch, "house-verdicts", memory, budget);
    let mut kept: Vec<House> = Vec::new();
    for house in houses.finish()? {
        let house = house?;
        if kept.first().is_some_and(|first| first.key != house.key) {
            kept.clear();
        }
        let distance = |kept: &House| {
            crate::spatial_index::haversine_m(
                kept.point[0],
                kept.point[1],
                house.point[0],
                house.point[1],
            )
        };
        let repeated = house.imported
            && kept
                .iter()
                .any(|kept| distance(kept) <= REPEATED_HOUSE_RADIUS_M);
        let disputed = if house.imported && !repeated {
            kept.iter()
                .filter(|kept| {
                    distance(kept) <= DISPUTED_HOUSE_RADIUS_M && !kept.other_house(&house)
                })
                .min_by(|left, right| distance(left).total_cmp(&distance(right)))
                .map(|kept| kept.at)
        } else {
            None
        };
        if !repeated && disputed.is_none() {
            kept.push(house);
            continue;
        }
        if let Some(at) = disputed {
            verdicts.push((at, DISPUTED))?;
        }
        verdicts.push((house.at, DROPPED))?;
        let dataset = datasets.name(house.dataset)?;
        if let Some(import) = report
            .imports
            .iter_mut()
            .find(|import| import.dataset == dataset)
        {
            if repeated {
                import.duplicates += 1;
            } else {
                import.disputes += 1;
            }
        }
    }
    Ok(verdicts)
}

/// Boundary geometry gathered across inputs, for the boundary stage.
#[derive(Debug, Default)]
struct Geometry {
    relations: Vec<BoundaryRelationStub>,
    /// Boundary ways kept by the scan. When one is also a relation member,
    /// its vertices are reused instead of being read a second time.
    boundary_feature_ids: HashSet<i64>,
}

impl Geometry {
    /// Add an input's part as saved, as its passes added it.
    fn add(&mut self, part: impl Iterator<Item = Result<GeometryItem>>) -> Result<()> {
        for item in part {
            match item? {
                GeometryItem::Relation(relation) => self.relations.push(relation),
                GeometryItem::Feature(way_id) => {
                    self.boundary_feature_ids.insert(way_id);
                }
                // Their vertices stay on disk, in the input's spool, until
                // the boundary stage reads them.
                GeometryItem::Way(_) | GeometryItem::Member(..) => {}
            }
        }
        Ok(())
    }
}

/// What the passes over one input hand from one to the next.
struct InputPasses {
    index: usize,
    /// The nodes kept ways need, until the node pass stores them.
    nodes: Option<NodeSet>,
    /// Nodes whose address tags interpolation ways read.
    tag_ids: Vec<i64>,
    node_blobs: Vec<u64>,
    way_blobs: Vec<u64>,
    /// Blobs holding a kept way or a member way: all the way pass rereads.
    kept_blobs: Vec<u64>,
    kept: KeptWays,
    /// Boundary relation member ways the input has.
    members: HashSet<i64>,
    /// The node store, from the node pass.
    locations: Option<Locations>,
    saved: InputSaved,
    /// The node set, the kept blobs or the members changed since they were
    /// saved.
    changed: bool,
}

/// What a checkpoint saved of an input's passes.
#[derive(Default)]
struct InputSaved {
    node_blobs: Option<SpoolFile<Id>>,
    way_blobs: Option<SpoolFile<Id>>,
    kept_blobs: Option<SpoolFile<Id>>,
    tag_ids: Option<SpoolFile<Id>>,
    node_set: Option<SpoolFile<ChunkBits>>,
    members: Option<SpoolFile<Id>>,
}

/// `values` saved to a spool.
fn save_ids(
    scratch: &Arc<Scratch>,
    label: &str,
    values: impl IntoIterator<Item = i64>,
) -> Result<SpoolFile<Id>> {
    let mut spool = Spool::create(scratch, label)?;
    for value in values {
        spool.write(&Id(value))?;
    }
    spool.finish()
}

/// The values a spool of [`save_ids`] holds.
fn load_ids(saved: &SpoolFile<Id>) -> Result<Vec<i64>> {
    saved.reader()?.map(|id| id.map(|Id(id)| id)).collect()
}

impl InputPasses {
    /// Save what changed since the last checkpoint, and name all of it.
    fn save(&mut self, scratch: &Arc<Scratch>) -> Result<(InputState, Vec<PathBuf>)> {
        let offsets = |values: &[u64]| values.iter().map(|value| *value as i64).collect::<Vec<_>>();
        let saved = &mut self.saved;
        if saved.node_blobs.is_none() {
            saved.node_blobs = Some(save_ids(scratch, "node-blobs", offsets(&self.node_blobs))?);
            saved.way_blobs = Some(save_ids(scratch, "way-blobs", offsets(&self.way_blobs))?);
        }
        if let Some(nodes) = &self.nodes {
            if saved.tag_ids.is_none() {
                saved.tag_ids = Some(save_ids(scratch, "tag-ids", self.tag_ids.iter().copied())?);
            }
            if self.changed || saved.node_set.is_none() {
                saved.node_set = Some(nodes.save(scratch)?);
            }
        }
        if self.changed || saved.kept_blobs.is_none() {
            saved.kept_blobs = Some(save_ids(scratch, "kept-blobs", offsets(&self.kept_blobs))?);
            saved.members = Some(save_ids(scratch, "members", self.members.iter().copied())?);
        }
        self.changed = false;
        let locations = self
            .locations
            .as_mut()
            .map(|locations| locations.save(scratch))
            .transpose()?;
        let required = |spool: &Option<SpoolFile<Id>>| {
            spool
                .as_ref()
                .map(SpoolFile::state)
                .ok_or_else(|| missing("a saved list of the input"))
        };
        let input = InputState {
            index: self.index,
            kept: self.kept.clone(),
            node_blobs: required(&saved.node_blobs)?,
            way_blobs: required(&saved.way_blobs)?,
            kept_blobs: required(&saved.kept_blobs)?,
            node_set: saved.node_set.as_ref().map(SpoolFile::state),
            tag_ids: saved.tag_ids.as_ref().map(SpoolFile::state),
            members: saved.members.as_ref().map(SpoolFile::state),
            locations,
        };
        let mut files = Vec::new();
        for run in [
            Some(&input.node_blobs),
            Some(&input.way_blobs),
            Some(&input.kept_blobs),
            input.node_set.as_ref(),
            input.tag_ids.as_ref(),
            input.members.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            files.extend(run.files(scratch));
        }
        if let Some(locations) = &self.locations {
            files.extend(locations.files(scratch));
        }
        Ok((input, files))
    }

    /// The passes a checkpoint saved, to go on with.
    fn restore(scratch: &Arc<Scratch>, memory: &Arc<Budget>, state: &InputState) -> Result<Self> {
        let spool = |run: &RunState| SpoolFile::<Id>::restore(scratch, run);
        let unsigned = |values: Vec<i64>| values.into_iter().map(|value| value as u64).collect();
        let node_blobs = spool(&state.node_blobs)?;
        let way_blobs = spool(&state.way_blobs)?;
        let kept_blobs = spool(&state.kept_blobs)?;
        let tag_ids = state.tag_ids.as_ref().map(spool).transpose()?;
        let members = state.members.as_ref().map(spool).transpose()?;
        let node_set = state
            .node_set
            .as_ref()
            .map(|run| SpoolFile::<ChunkBits>::restore(scratch, run))
            .transpose()?;
        Ok(Self {
            index: state.index,
            nodes: node_set
                .as_ref()
                .map(|saved| NodeSet::load(memory, saved))
                .transpose()?,
            tag_ids: tag_ids
                .as_ref()
                .map(load_ids)
                .transpose()?
                .unwrap_or_default(),
            node_blobs: unsigned(load_ids(&node_blobs)?),
            way_blobs: unsigned(load_ids(&way_blobs)?),
            kept_blobs: unsigned(load_ids(&kept_blobs)?),
            kept: state.kept.clone(),
            members: members
                .as_ref()
                .map(load_ids)
                .transpose()?
                .unwrap_or_default()
                .into_iter()
                .collect(),
            locations: state
                .locations
                .as_ref()
                .map(|locations| Locations::restore(scratch, memory, locations))
                .transpose()?,
            saved: InputSaved {
                node_blobs: Some(node_blobs),
                way_blobs: Some(way_blobs),
                kept_blobs: Some(kept_blobs),
                tag_ids,
                node_set,
                members,
            },
            changed: false,
        })
    }
}

fn member_ways(block: &osmpbf::PrimitiveBlock, member_ids: &HashSet<i64>) -> Vec<(i64, Vec<i64>)> {
    block
        .elements()
        .filter_map(|element| match element {
            osmpbf::Element::Way(way) if member_ids.contains(&way.id()) => {
                Some((way.id(), way.refs().collect()))
            }
            _ => None,
        })
        .collect()
}

/// Write sorted records in batches, computing admin context in parallel, and
/// offer each record to the parser's training sample under the country it
/// lies in. Boundary and derived-country records describe context and get
/// none, but are offered under the country their point lies in.
#[allow(clippy::too_many_arguments)]
fn write_ordered(
    writer: &mut Writer,
    sample: &mut Sampler,
    records: impl Iterator<Item = Result<PendingRecord>> + Send,
    total: u64,
    index: &BoundaryIndex,
    book: Option<&AddressBook>,
    progress: &Progress,
    stage: BuildStage,
) -> Result<()> {
    progress.start(stage, Some(total))?;
    let mut preparer = writer.preparer();
    // One thread reads and prepares the next batch while this one appends the
    // batch before: the store, the text index and the spatial pairs take
    // records in order, and would otherwise wait on the preparing.
    std::thread::scope(|scope| -> Result<()> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        scope.spawn(move || {
            let mut records = records.peekable();
            while records.peek().is_some() {
                let prepared = records
                    .by_ref()
                    .take(WRITE_BATCH_RECORDS)
                    .collect::<Result<Vec<_>>>()
                    .and_then(|batch| match book {
                        Some(book) => book.records(batch),
                        None => batch
                            .into_iter()
                            .map(|pending| match pending.body {
                                Body::Record(record) => Ok((*record, pending.origin)),
                                Body::Address(_) => Err(Error::Decode(
                                    "a placed address is written without its address book",
                                )),
                            })
                            .collect(),
                    })
                    .and_then(|batch| {
                        let (batch, countries): (Vec<_>, Vec<_>) = batch
                            .into_par_iter()
                            .map(|(record, origin)| {
                                let located = record_context(index, &record);
                                let country = located
                                    .as_ref()
                                    .and_then(|context| context.admin_context.country_record_id());
                                let context: Option<RecordContext> = match origin {
                                    Some(_) => None,
                                    None => located,
                                };
                                ((record, context), country)
                            })
                            .unzip();
                        preparer
                            .prepare(batch)
                            .map(|prepared| (prepared, countries))
                    });
                let failed = prepared.is_err();
                if sender.send(prepared).is_err() || failed {
                    break;
                }
            }
        });
        for prepared in receiver {
            let (prepared, countries) = prepared?;
            for ((id, record), country) in prepared.records().zip(countries) {
                sample.offer(id, country, record);
            }
            let len = prepared.len() as u64;
            writer.commit(prepared)?;
            progress.advance(len);
            progress.check()?;
        }
        Ok(())
    })?;
    progress.finish(stage);
    Ok(())
}

/// Build audit outputs written next to the Pack file, outside it.
struct Audit {
    dir: PathBuf,
    rejections: BufWriter<File>,
    count: u64,
}

impl Audit {
    fn create(generation: &Path) -> Result<Self> {
        let dir = generation.join(AUDIT_DIR);
        fs::create_dir_all(&dir).map_err(|source| Error::IoPath {
            action: "failed to create",
            path: dir.clone(),
            source,
        })?;
        let path = dir.join(REJECTIONS_FILE);
        let file = File::create(&path).map_err(|source| Error::IoPath {
            action: "failed to create",
            path: path.clone(),
            source,
        })?;
        Ok(Self {
            dir,
            rejections: BufWriter::with_capacity(1 << 20, file),
            count: 0,
        })
    }

    /// The audit as a checkpoint left it: rejections a failed run wrote
    /// after it are cut off.
    fn reopen(generation: &Path, state: &AuditState) -> Result<Self> {
        let dir = generation.join(AUDIT_DIR);
        let path = dir.join(REJECTIONS_FILE);
        let failed = |source| Error::IoPath {
            action: "failed to reopen",
            path: path.clone(),
            source,
        };
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(failed)?;
        file.set_len(state.bytes).map_err(failed)?;
        file.seek(SeekFrom::End(0)).map_err(failed)?;
        Ok(Self {
            dir,
            rejections: BufWriter::with_capacity(1 << 20, file),
            count: state.count,
        })
    }

    /// Flush and sync the rejections so far, for a checkpoint.
    fn state(&mut self) -> Result<AuditState> {
        self.rejections.flush()?;
        let file = self.rejections.get_mut();
        file.sync_all()?;
        Ok(AuditState {
            bytes: file.stream_position()?,
            count: self.count,
        })
    }

    fn write(&mut self, rejection: &RejectedRecord) -> Result<()> {
        serde_json::to_writer(&mut self.rejections, rejection)?;
        self.rejections.write_all(b"\n")?;
        self.count += 1;
        Ok(())
    }

    fn finish(mut self, report: &BuilderReport) -> Result<()> {
        self.rejections.flush()?;
        let path = self.dir.join(BUILD_REPORT_FILE);
        let file = File::create(&path).map_err(|source| Error::IoPath {
            action: "failed to create",
            path: path.clone(),
            source,
        })?;
        serde_json::to_writer_pretty(BufWriter::new(file), report).map_err(Error::from)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        builder::progress::StageUnit,
        pack::{PointPrecision, Reader},
        record::Record,
        reverse::{GeocodeOptions, MatchKind, PackReverseGeocoder},
        search::{TextAutocompleteOptions, TextSearchHit, TextSearchOptions, TextSearcher},
    };

    use super::{
        test_pbf::{TestElement, node, relation, way, write_pbf},
        *,
    };

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "open-geocode-build-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).expect("dir");
        dir
    }

    /// A small town: a locality inside a region, addresses, a street, a
    /// building, an interpolation range, and a few objects that must be
    /// rejected. Split over several blocks like a real extract.
    fn town() -> Vec<Vec<TestElement>> {
        vec![
            vec![
                node(
                    1,
                    43.60,
                    -79.40,
                    &[
                        ("addr:housenumber", "10"),
                        ("addr:street", "King Street"),
                        ("addr:city", "Toronto"),
                        ("addr:postcode", "M5V 1A1"),
                    ],
                ),
                node(
                    2,
                    43.6001,
                    -79.4001,
                    &[
                        ("addr:housenumber", "12"),
                        ("addr:street", "King Street"),
                        ("addr:postcode", "m5v  1a1"),
                        // Complete source context: its report sample has no missing fields.
                        ("addr:city", "Toronto"),
                        ("addr:state", "ON"),
                        ("addr:country", "CA"),
                    ],
                ),
                node(3, 43.6002, -79.4002, &[("addr:housenumber", "14")]),
                node(
                    4,
                    43.67,
                    -79.40,
                    &[("place", "neighbourhood"), ("name", "Annex")],
                ),
                node(20, 43.61, -79.42, &[]),
                node(21, 43.61, -79.41, &[]),
                node(30, 43.6200, -79.4300, &[]),
                node(31, 43.6200, -79.4298, &[]),
                node(32, 43.6202, -79.4298, &[]),
                node(33, 43.6202, -79.4300, &[]),
            ],
            vec![
                node(
                    40,
                    43.630,
                    -79.44,
                    &[("addr:housenumber", "2"), ("addr:street", "Elm Street")],
                ),
                node(
                    41,
                    43.631,
                    -79.44,
                    &[("addr:housenumber", "10"), ("addr:street", "Elm Street")],
                ),
                node(1001, 43.5, -79.5, &[]),
                node(1002, 43.5, -79.3, &[]),
                node(1003, 43.7, -79.3, &[]),
                node(1004, 43.7, -79.5, &[]),
                node(1011, 43.0, -80.0, &[]),
                node(1012, 43.0, -79.0, &[]),
                node(1013, 44.0, -79.0, &[]),
                node(1014, 44.0, -80.0, &[]),
            ],
            vec![
                way(500, &[1001, 1002, 1003], &[]),
                way(501, &[1003, 1004, 1001], &[]),
                way(502, &[1011, 1012, 1013, 1014, 1011], &[]),
                way(
                    600,
                    &[20, 21],
                    &[("highway", "residential"), ("name", "Queen Street")],
                ),
                way(601, &[20, 21], &[("highway", "service")]),
                way(
                    602,
                    &[30, 31, 32, 33, 30],
                    &[
                        ("building", "yes"),
                        ("addr:housenumber", "5"),
                        ("addr:street", "Bay Street"),
                    ],
                ),
                way(
                    603,
                    &[40, 41],
                    &[
                        ("addr:interpolation", "even"),
                        ("addr:street", "Elm Street"),
                    ],
                ),
                way(
                    604,
                    &[20, 9999],
                    &[("highway", "residential"), ("name", "Ghost Road")],
                ),
                // A named boundary way that is also a relation member.
                way(
                    605,
                    &[30, 31, 32, 33, 30],
                    &[
                        ("boundary", "administrative"),
                        ("admin_level", "10"),
                        ("name", "Bay Ward"),
                    ],
                ),
            ],
            vec![
                relation(
                    900,
                    &[(500, "outer"), (501, "outer")],
                    &[
                        ("boundary", "administrative"),
                        ("admin_level", "8"),
                        ("name", "Toronto"),
                    ],
                ),
                relation(
                    902,
                    &[(605, "outer")],
                    &[
                        ("boundary", "administrative"),
                        ("admin_level", "10"),
                        ("name", "Bay Ward Relation"),
                    ],
                ),
                relation(
                    901,
                    &[(502, "outer")],
                    &[
                        ("boundary", "administrative"),
                        ("admin_level", "4"),
                        ("name", "Ontario"),
                        ("ISO3166-2", "CA-ON"),
                    ],
                ),
            ],
        ]
    }

    /// A second country, far from `town`, so a Pack of both covers two.
    /// Its region states `NZ-WGN`, from which a New Zealand country record is
    /// derived exactly as `CA-ON` derives Canada's.
    fn other_country() -> Vec<Vec<TestElement>> {
        vec![vec![
            node(
                5001,
                -41.29,
                174.77,
                &[
                    ("addr:housenumber", "10"),
                    ("addr:street", "King Street"),
                    ("addr:city", "Wellington"),
                    ("addr:country", "NZ"),
                ],
            ),
            node(
                5002,
                -41.2901,
                174.7701,
                &[("addr:housenumber", "12"), ("addr:street", "Queen Street")],
            ),
            node(6001, -41.30, 174.76, &[]),
            node(6002, -41.28, 174.76, &[]),
            node(6003, -41.28, 174.78, &[]),
            node(6004, -41.30, 174.78, &[]),
            way(5500, &[6001, 6002, 6003, 6004, 6001], &[]),
            relation(
                5901,
                &[(5500, "outer")],
                &[
                    ("boundary", "administrative"),
                    ("admin_level", "4"),
                    ("name", "Wellington Region"),
                    ("ISO3166-2", "NZ-WGN"),
                ],
            ),
        ]]
    }

    /// One Pack from two extracts holds the records of both, in one text
    /// index: a search with no country filter finds either, and `country`
    /// narrows it to one. This is what covering several countries without a
    /// router rests on, so scores stay comparable across them.
    #[test]
    fn builds_one_pack_covering_two_extracts() {
        let dir = temp_dir("two-countries");
        let (first, second) = (dir.join("town.osm.pbf"), dir.join("other.osm.pbf"));
        write_pbf(&first, &town());
        write_pbf(&second, &other_country());

        let one = build(&dir.join("one"), &first, None);
        let both = build_all(&dir.join("both"), &[&first, &second], None);

        assert_eq!(both.inputs.len(), 2, "the report names every input");
        assert_eq!(
            both.input_bytes,
            both.inputs.iter().map(|i| i.bytes).sum::<u64>()
        );
        assert!(
            both.accepted.total > one.accepted.total,
            "the second extract's records are in the Pack too: {} vs {}",
            both.accepted.total,
            one.accepted.total
        );

        let reader = Arc::new(Reader::open(dir.join("both")).expect("reader"));
        let searcher = TextSearcher::from_pack(Arc::clone(&reader)).expect("searcher");
        let search = |countries: Vec<String>| {
            searcher
                .search(TextSearchOptions {
                    query: "King Street".to_string(),
                    limit: 20,
                    layer: None,
                    countries,
                    focus: None,
                    bbox: None,
                })
                .expect("search")
        };

        // One index over both: the query finds the street in either country.
        let any = search(Vec::new());
        assert!(any.len() >= 2, "both King Streets are indexed: {any:?}");

        // The filter narrows to one, and the two sides partition the whole.
        let ca = search(vec!["CA".to_string()]);
        let nz = search(vec!["NZ".to_string()]);
        assert!(!ca.is_empty(), "Canada has a King Street");
        assert!(!nz.is_empty(), "New Zealand has one too");
        assert_eq!(
            ca.len() + nz.len(),
            any.len(),
            "every hit lies in exactly one of the two countries"
        );

        // Several codes are a union, and an uncovered country matches nothing
        // rather than erroring: a legitimate empty answer.
        assert_eq!(
            search(vec!["CA".to_string(), "NZ".to_string()]).len(),
            any.len()
        );
        assert!(search(vec!["FR".to_string()]).is_empty());

        // The filter narrows, it does not rank: a hit keeps the score it had
        // unfiltered. This is what makes one Pack over several countries
        // worth more than a Runtime per country behind a router, where the
        // scores being merged come from different indexes and are not
        // comparable at all.
        for filtered in [&ca, &nz] {
            for hit in filtered {
                let unfiltered = any
                    .iter()
                    .find(|other| other.record_id == hit.record_id)
                    .expect("a filtered hit is also an unfiltered one");
                assert_eq!(
                    hit.score, unfiltered.score,
                    "the country filter changed the score of record {}",
                    hit.record_id
                );
            }
        }
    }

    fn build(root: &Path, input: &Path, memory_budget_bytes: Option<usize>) -> BuilderReport {
        build_all(root, &[input], memory_budget_bytes)
    }

    /// Build one Pack from several extracts, as a multi-region build does.
    fn build_all(
        root: &Path,
        inputs: &[&Path],
        memory_budget_bytes: Option<usize>,
    ) -> BuilderReport {
        build_osm_pack(BuildOsmOptions {
            inputs: inputs.iter().map(|input| input.to_path_buf()).collect(),
            pack: root.to_path_buf(),
            memory_budget_bytes,
            // A budget small enough to spill everything cannot also hold the
            // node set and the boundary shapes the plan sets inside it.
            force: memory_budget_bytes.is_some(),
            ..BuildOsmOptions::default()
        })
        .expect("build")
    }

    /// Hands out fixed files, keeping the countries it was asked for.
    #[derive(Debug, Default)]
    struct RecordingSource {
        files: Vec<addresses::AddressFile>,
        asked: std::sync::Mutex<Vec<addresses::Coverage>>,
    }

    impl AddressSource for RecordingSource {
        fn files(&self, coverage: &addresses::Coverage) -> Result<Vec<addresses::AddressFile>> {
            self.asked.lock().expect("lock").push(coverage.clone());
            Ok(self.files.clone())
        }
    }

    #[test]
    fn imports_addresses_and_leaves_out_the_ones_osm_has() {
        let dir = temp_dir("imported");
        let input = dir.join("town.osm.pbf");
        write_pbf(&input, &town());
        let csv = dir.join("addresses.csv");
        fs::write(
            &csv,
            "LON,LAT,NUMBER,STREET,CITY,POSTCODE\n\
             -79.40005,43.60003,10,KING ST,Toronto,M5V 1A1\n\
             -79.4003,43.6003,16,King Street,Toronto,M5V 1A1\n\
             -79.40031,43.60031,16,King Street,,\n\
             -79.45,43.65,3,Queen Street,Toronto,\n\
             -79.45,43.65,,Queen Street,Toronto,\n",
        )
        .expect("csv");
        let source = Arc::new(RecordingSource {
            files: vec![addresses::AddressFile {
                dataset: "openaddresses:test".to_string(),
                path: csv,
            }],
            ..RecordingSource::default()
        });
        let report = build_osm_pack(BuildOsmOptions {
            inputs: vec![input],
            pack: dir.join("pack"),
            addresses: Some(Arc::clone(&source) as Arc<dyn AddressSource>),
            ..BuildOsmOptions::default()
        })
        .expect("build");

        assert_eq!(
            *source.asked.lock().expect("lock"),
            vec![addresses::Coverage {
                countries: Default::default(),
                subdivisions: std::collections::BTreeSet::from(["CA-ON".to_string()]),
            }],
            "asked once, for the province: the extract has no Canada boundary"
        );
        let imported = &report.imports[0];
        assert_eq!((imported.rows, imported.accepted), (5, 4));
        assert_eq!(imported.rejected.get("missing_number"), Some(&1));
        assert_eq!(
            imported.duplicates, 2,
            "10 King St repeats the OSM address, the second 16 the first"
        );
        assert!(report.datasets.contains(&"openaddresses:test".to_string()));

        let reader = Reader::open(dir.join("pack")).expect("reader");
        assert_eq!(reader.manifest().layer_counts["address"], 5 + 2);
        let searcher = TextSearcher::open(dir.join("pack")).expect("searcher");
        let top = |query: &str| {
            searcher
                .search(TextSearchOptions {
                    query: query.to_string(),
                    limit: 1,
                    layer: None,
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                })
                .expect("search")
                .into_iter()
                .map(|hit| hit.record.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(top("16 King Street"), vec!["openaddresses:test:row:2"]);
        assert_eq!(top("10 King Street"), vec!["osm:node:1"]);
        assert_eq!(top("3 Queen Street"), vec!["openaddresses:test:row:4"]);
        let _ = fs::remove_dir_all(dir);
    }

    /// A row naming a house OSM has, placed a few hundred metres away, is
    /// the same house in the wrong place: one record, OSM's, marked disputed.
    /// Farther, or in another postcode, it is a house of its own.
    #[test]
    fn keeps_one_record_of_a_house_two_sources_place_apart() {
        let dir = temp_dir("disputed");
        let input = dir.join("town.osm.pbf");
        write_pbf(&input, &town());
        let csv = dir.join("addresses.csv");
        // OSM has 10 King Street, Toronto, M5V 1A1 at 43.60, -79.40.
        fs::write(
            &csv,
            "LON,LAT,NUMBER,STREET,CITY,POSTCODE\n\
             -79.40,43.604,10,King Street,Toronto,M5V 1A1\n\
             -79.40,43.70,10,King Street,Toronto,\n\
             -79.40,43.596,10,King Street,Toronto,K7L 1A1\n",
        )
        .expect("csv");
        let report = build_osm_pack(BuildOsmOptions {
            inputs: vec![input],
            pack: dir.join("pack"),
            addresses: Some(Arc::new(addresses::AddressFiles(vec![
                addresses::AddressFile {
                    dataset: "openaddresses:test".to_string(),
                    path: csv,
                },
            ]))),
            ..BuildOsmOptions::default()
        })
        .expect("build");

        let imported = &report.imports[0];
        assert_eq!((imported.duplicates, imported.disputes), (0, 1));
        let reader = Reader::open(dir.join("pack")).expect("reader");
        assert_eq!(reader.manifest().layer_counts["address"], 5 + 2);
        let disputed = |id: &str| {
            let record_id = reader
                .find_by_source_id(id)
                .expect("lookup")
                .unwrap_or_else(|| panic!("{id} is in the Pack"));
            match reader.records().record(record_id).expect("record") {
                Record::Address(address) => address.disputed,
                other => panic!("{id} is an address: {other:?}"),
            }
        };
        assert!(disputed("osm:node:1"), "OSM's point is kept, disputed");
        assert_eq!(
            reader
                .find_by_source_id("openaddresses:test:row:1")
                .expect("lookup"),
            None,
            "the row 445 m away is dropped"
        );
        assert!(
            !disputed("openaddresses:test:row:2"),
            "11 km away: another house"
        );
        assert!(
            !disputed("openaddresses:test:row:3"),
            "another postcode: another house"
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// New Zealand writes a flat "UNIT/NUMBER". Read from the plain numbers
    /// around it, "10/1367" is flat 10 of 1367: found by its building's
    /// number, never standing for house 10.
    #[test]
    fn reads_a_slash_number_as_a_flat_of_its_building() {
        let dir = temp_dir("slash");
        let input = dir.join("slash.osm.pbf");
        let address = |id, lon, number: &'static str| {
            node(
                id,
                -38.1386,
                lon,
                &[
                    ("addr:housenumber", number),
                    ("addr:street", "Eruera Street"),
                ],
            )
        };
        write_pbf(
            &input,
            &[vec![
                node(1, -38.1388, 176.2580, &[]),
                node(2, -38.1388, 176.2430, &[]),
                way(
                    100,
                    &[1, 2],
                    &[("highway", "residential"), ("name", "Eruera Street")],
                ),
                address(10, 176.2570, "1000"),
                address(11, 176.2510, "1182"),
                address(12, 176.2509, "2/1182"),
                address(13, 176.2456, "1345"),
                address(14, 176.2451, "1367"),
                address(15, 176.24505, "10/1367"),
                address(16, 176.24506, "2/1367"),
                address(17, 176.2440, "1400"),
            ]],
        );
        let report = build(&dir.join("pack"), &input, None);
        assert_eq!(
            report.slash_numbers,
            SlashNumberCounts {
                unit_first: 3,
                ..SlashNumberCounts::default()
            }
        );

        let searcher = TextSearcher::open(dir.join("pack")).expect("searcher");
        let top = |query: &str| {
            searcher
                .search(TextSearchOptions {
                    query: query.to_string(),
                    limit: 1,
                    layer: None,
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                })
                .expect("search")
                .into_iter()
                .map(|hit| hit.record.id)
                .collect::<Vec<_>>()
        };
        let geocode = |address: &str| {
            searcher
                .geocode_address(crate::search::AddressGeocodeOptions {
                    countries: Vec::new(),
                    address: address.to_string(),
                    locality: None,
                    region: None,
                    postcode: None,
                    limit: 10,
                    layer: None,
                })
                .expect("geocode")
                .map(|hit| hit.hit.record.id)
        };
        assert_eq!(top("1367 Eruera Street"), vec!["osm:node:14"]);
        assert_eq!(top("10/1367 Eruera Street"), vec!["osm:node:15"]);
        // No house 10 or 2: the street answers, not the flats.
        for query in ["10 Eruera Street", "2 Eruera Street"] {
            assert_eq!(top(query), vec!["osm:way:100"], "{query}");
            assert_eq!(geocode(query).as_deref(), Some("osm:way:100"), "{query}");
        }
        // A missing number is placed between buildings, not flats.
        assert_eq!(
            top("1300 Eruera Street"),
            vec!["derived:estimate:1300:osm:node:11:osm:node:13"]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn builds_a_searchable_reverse_geocodable_pack_from_pbf() {
        let dir = temp_dir("town");
        let input = dir.join("town.osm.pbf");
        write_pbf(&input, &town());
        let report = build(&dir.join("pack"), &input, None);

        let reader = Reader::open(dir.join("pack")).expect("reader");
        let counts = &reader.manifest().layer_counts;
        let count = |layer: &str| counts.get(layer).copied().unwrap_or_default();
        assert_eq!(
            count("address"),
            5,
            "four nodes (two are anchors) and one building"
        );
        assert_eq!(count("street"), 1);
        assert_eq!(count("interpolation"), 1);
        assert_eq!(count("postcode"), 1);
        assert_eq!(
            count("neighbourhood"),
            3,
            "Annex, the Bay Ward way and its relation"
        );
        assert_eq!(count("locality"), 1);
        assert_eq!(count("region"), 1);
        assert_eq!(count("country"), 1, "derived from ISO3166-2");
        assert_eq!(report.output.record_count, 14);
        // Each way's nodes are requested once, even when a boundary way is also
        // a relation member: 3 + 3 + 5 + 2 + 5 + 2 + 2 + 5.
        assert_eq!(report.geometry_resolution.required_node_refs, 27);
        // 17 distinct nodes, of which the extract lacks Ghost Road's 9999;
        // the interpolation's two anchors keep their address tags.
        let passes = &report.geometry_passes;
        assert_eq!(
            (
                passes.needed_nodes,
                passes.stored_nodes,
                passes.tagged_nodes
            ),
            (17, 16, 2)
        );
        assert_eq!(report.geometry_resolution.resolved_node_refs, 26);
        assert_eq!(passes.mapped_location_bytes, 0);
        let input_bytes = fs::metadata(&input).expect("input").len();
        assert_eq!(passes.scan_read_bytes, input_bytes);
        for read in [
            passes.boundary_member_read_bytes,
            passes.node_read_bytes,
            passes.way_read_bytes,
        ] {
            assert!(read > 0 && read <= input_bytes, "{passes:?}");
        }
        let plan = &report.resources.plan;
        assert!(plan.counted && plan.locations_in_memory);
        assert_eq!(plan.items.needed_nodes, 17);
        assert_eq!(plan.items.streets, 2);
        assert_eq!(report.accepted.postcode_records, 1);
        assert_eq!(report.geometry_resolution.street_way_stubs, 2);
        assert_eq!(
            report.scratch.spills["records"].merge_passes, 1,
            "a small input merges what its checkpoints wrote once"
        );
        assert!(report.scratch.peak_tracked_bytes > 0);
        assert!(report.scratch.peak_tracked_bytes <= report.scratch.memory_budget_bytes);

        // Places come first, and each group is in Hilbert order, a street's
        // addresses placed at the first of them. Postcodes are placed with
        // the rest: they wait on every address.
        let headers = (0..reader.manifest().record_count)
            .map(|id| reader.records().header(id).expect("header"))
            .collect::<Vec<_>>();
        let first_other = headers
            .iter()
            .position(|header| header.layer.place_layer().is_none())
            .expect("records other than places");
        assert!(
            headers[first_other..]
                .iter()
                .all(|header| header.layer.place_layer().is_none())
        );
        for range in [0..first_other, first_other..headers.len()] {
            let mut keys = Vec::new();
            let mut street = None;
            for id in range {
                let header = &headers[id];
                let summary = reader.records().summary(id as u64).expect("summary");
                let this_street = (header.layer == crate::record::Layer::Address)
                    .then(|| summary.address.street.clone())
                    .flatten();
                let key = match keys.last() {
                    Some(&previous) if this_street.is_some() && this_street == street => previous,
                    _ => hilbert_key(header.lon_e7, header.lat_e7),
                };
                street = this_street;
                keys.push(key);
            }
            assert!(keys.windows(2).all(|pair| pair[0] <= pair[1]));
        }

        // Boundary-derived context.
        let king = reader
            .find_by_source_id("osm:node:1")
            .expect("lookup")
            .expect("address node 1");
        let context = reader
            .boundary_context(king)
            .expect("context")
            .expect("has context");
        let name = |id: Option<u64>| {
            reader
                .context_record(id.expect("id"))
                .expect("record")
                .expect("context")
                .name
        };
        assert_eq!(name(context.admin_context.locality_record_id()), "Toronto");
        assert_eq!(name(context.admin_context.region_record_id()), "Ontario");
        assert_eq!(name(context.admin_context.country_record_id()), "Canada");

        let searcher = TextSearcher::open(dir.join("pack")).expect("searcher");
        let hits = searcher
            .search(TextSearchOptions {
                query: "10 King Street".into(),
                limit: 1,
                layer: Some("address".into()),
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search");
        assert_eq!(hits[0].record.id, "osm:node:1");
        assert_eq!(hits[0].record.label, "10 King Street, Toronto, M5V 1A1");

        let reverse = PackReverseGeocoder::open(dir.join("pack")).expect("reverse");
        let result = |lon, lat| {
            reverse
                .reverse(GeocodeOptions { lon, lat })
                .expect("reverse")
                .result
                .expect("result")
        };
        let building = result(-79.4299, 43.6201);
        assert_eq!(building.match_kind, MatchKind::ExplicitAddress);
        assert_eq!(building.id.as_deref(), Some("osm:way:602"));
        let estimated = result(-79.44001, 43.6305);
        assert_eq!(estimated.match_kind, MatchKind::EstimatedAddress);
        assert!(
            estimated.label.starts_with("6 Elm Street"),
            "{}",
            estimated.label
        );
        let street = result(-79.415, 43.61001);
        assert_eq!(street.match_kind, MatchKind::NearestStreet);
        assert!(
            street.label.starts_with("Queen Street, ") && street.label.contains("Toronto"),
            "{}",
            street.label
        );

        // Rejections go to the build audit, not the Pack.
        let rejections = reader.rejections(0).expect("rejections");
        let reasons = rejections
            .iter()
            .map(|rejection| rejection.reason.as_str())
            .collect::<Vec<_>>();
        assert!(reasons.contains(&"missing_street_or_place"), "{reasons:?}");
        assert!(
            reasons.contains(&"street_unresolved_geometry"),
            "{reasons:?}"
        );
        assert!(
            !reader
                .section_sizes()
                .keys()
                .any(|name| name.contains("reject"))
        );
        assert_eq!(report.output.rejection_count, rejections.len() as u64);
        let generation = reader.path().parent().expect("generation");
        assert!(generation.join(AUDIT_DIR).join(BUILD_REPORT_FILE).is_file());

        // The benchmark reads the build report back.
        let bench = crate::bench::benchmark_pack(crate::bench::PackBenchmarkOptions {
            pack: dir.join("pack"),
            queries: None,
            iterations: 1,
            warmup: 0,
        })
        .expect("bench");
        let build_metrics = bench.pack.build.expect("build metrics");
        assert_eq!(build_metrics.accepted_records, report.accepted.total);
        assert_eq!(build_metrics.scratch, report.scratch);
    }

    /// A ward with names in several scripts, a block scheme address in it
    /// and a street with an English name.
    #[test]
    fn indexes_every_name_and_accepts_block_scheme_addresses() {
        let dir = temp_dir("ward");
        let input = dir.join("ward.osm.pbf");
        write_pbf(
            &input,
            &[
                vec![
                    node(1, 35.67, 139.74, &[]),
                    node(2, 35.67, 139.78, &[]),
                    node(3, 35.70, 139.78, &[]),
                    node(4, 35.70, 139.74, &[]),
                    node(
                        10,
                        35.68,
                        139.76,
                        &[
                            ("addr:province", "東京都"),
                            ("addr:city", "千代田区"),
                            ("addr:neighbourhood", "丸の内一丁目"),
                            ("addr:block_number", "9"),
                            ("addr:housenumber", "1"),
                        ],
                    ),
                    node(11, 35.685, 139.75, &[]),
                    node(12, 35.686, 139.751, &[]),
                ],
                vec![
                    way(
                        500,
                        &[1, 2, 3, 4, 1],
                        &[
                            ("boundary", "administrative"),
                            ("admin_level", "8"),
                            ("name", "千代田区"),
                            ("name:en", "Chiyoda"),
                            ("name:ja-Hira", "ちよだく"),
                            ("name:etymology", "not a name"),
                        ],
                    ),
                    way(
                        600,
                        &[11, 12],
                        &[
                            ("highway", "primary"),
                            ("name", "内堀通り"),
                            ("name:en", "Uchibori-dori"),
                        ],
                    ),
                ],
            ],
        );
        let report = build(&dir.join("pack"), &input, None);
        assert_eq!(report.accepted.by_layer.get("address"), Some(&1));
        assert_eq!(report.rejected.total, 0);

        let reader = Reader::open(dir.join("pack")).expect("reader");
        let address = reader
            .find_by_source_id("osm:node:10")
            .expect("lookup")
            .expect("block address");
        let json = reader.record_json(address).expect("json");
        assert_eq!(json["address"]["number"], "9-1");
        assert_eq!(
            json["address"]["areas"],
            serde_json::json!(["丸の内一丁目"])
        );
        assert_eq!(json["address"]["region"], "東京都");
        let ward = reader
            .find_by_source_id("osm:way:500")
            .expect("lookup")
            .expect("ward");
        assert_eq!(
            reader.records().names(ward).expect("names"),
            vec!["千代田区", "Chiyoda", "ちよだく", "chiyodaku"]
        );

        let searcher = TextSearcher::open(dir.join("pack")).expect("searcher");
        let first = |query: &str, layer: &str| {
            searcher
                .search(TextSearchOptions {
                    query: query.into(),
                    limit: 1,
                    layer: Some(layer.into()),
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                })
                .expect("search")
                .first()
                .map(|hit| hit.record.id.clone())
        };
        assert_eq!(
            first("Uchibori-dori", "street").as_deref(),
            Some("osm:way:600")
        );
        assert_eq!(first("Chiyoda", "locality").as_deref(), Some("osm:way:500"));
        assert_eq!(
            first("ちよだく", "locality").as_deref(),
            Some("osm:way:500")
        );
        assert_eq!(
            first("chiyodaku", "locality").as_deref(),
            Some("osm:way:500"),
            "the Latin form of a kana name"
        );

        let tagger = crate::parse::lattice::LatticeTagger::open(&dir.join("pack")).expect("tagger");
        let dictionaries = tagger.dictionaries();
        use crate::parse::lattice::dictionary::Field;
        for (field, key) in [
            (Field::Locality, "千代田区"),
            (Field::Locality, "ちよだく"),
            (Field::Locality, "chiyoda"),
            (Field::Locality, "chiyodaku"),
            (Field::Locality, "丸の内一丁目"),
            (Field::Region, "東京都"),
            (Field::Street, "uchibori dori"),
        ] {
            assert!(dictionaries.contains(field, key), "{field:?} {key}");
        }
        assert!(!dictionaries.contains(Field::Locality, "not a name"));
    }

    /// Places drawn as areas: an island multipolygon, a suburb drawn as a
    /// closed way, and a route relation with a `place` tag that draws no
    /// area. The address inside them lies in both.
    #[test]
    fn places_drawn_as_areas_are_places_and_context() {
        let dir = temp_dir("island");
        let input = dir.join("island.osm.pbf");
        write_pbf(
            &input,
            &[
                vec![
                    node(1, 35.0, 135.0, &[]),
                    node(2, 35.0, 136.0, &[]),
                    node(3, 36.0, 136.0, &[]),
                    node(4, 36.0, 135.0, &[]),
                    node(5, 35.4, 135.4, &[]),
                    node(6, 35.4, 135.6, &[]),
                    node(7, 35.6, 135.6, &[]),
                    node(8, 35.6, 135.4, &[]),
                    node(
                        10,
                        35.5,
                        135.5,
                        &[
                            ("addr:neighbourhood", "5丁目"),
                            ("addr:block_number", "10"),
                            ("addr:housenumber", "8"),
                        ],
                    ),
                ],
                vec![
                    way(500, &[1, 2, 3], &[("natural", "coastline")]),
                    way(501, &[3, 4, 1], &[("natural", "coastline")]),
                    way(
                        502,
                        &[5, 6, 7, 8, 5],
                        &[("place", "suburb"), ("name", "港北")],
                    ),
                ],
                vec![
                    relation(
                        900,
                        &[(500, "outer"), (501, "outer")],
                        &[
                            ("type", "multipolygon"),
                            ("place", "island"),
                            ("name", "本州"),
                            ("name:en", "Honshu"),
                        ],
                    ),
                    relation(
                        901,
                        &[(500, "")],
                        &[("type", "route"), ("place", "island"), ("name", "Route")],
                    ),
                ],
            ],
        );
        let report = build(&dir.join("pack"), &input, None);
        assert_eq!(report.accepted.place_areas, 2);
        assert_eq!(report.accepted.by_layer.get("place"), Some(&1));
        assert_eq!(report.accepted.by_layer.get("neighbourhood"), Some(&1));

        let reader = Reader::open(dir.join("pack")).expect("reader");
        let island = reader
            .find_by_source_id("osm:relation:900")
            .expect("lookup")
            .expect("island");
        let suburb = reader
            .find_by_source_id("osm:way:502")
            .expect("lookup")
            .expect("suburb");
        assert!(
            reader
                .find_by_source_id("osm:relation:901")
                .expect("lookup")
                .is_none()
        );
        let address = reader
            .find_by_source_id("osm:node:10")
            .expect("lookup")
            .expect("address");
        let context = reader
            .boundary_context(address)
            .expect("context")
            .expect("address context")
            .admin_context;
        assert_eq!(context.place_record_id(), Some(island));
        assert_eq!(context.neighbourhood_record_id(), Some(suburb));

        let tagger = crate::parse::lattice::LatticeTagger::open(&dir.join("pack")).expect("tagger");
        use crate::parse::lattice::dictionary::Field;
        for key in ["本州", "honshu", "港北"] {
            assert!(
                tagger.dictionaries().contains(Field::Locality, key),
                "{key}"
            );
        }
    }

    /// The town plus named POIs: one with an address, one without, a station
    /// building, a namesake outside the town, and objects that are not POIs.
    fn town_with_pois() -> Vec<Vec<TestElement>> {
        let mut blocks = town();
        blocks.insert(
            2,
            vec![
                node(
                    2001,
                    43.65,
                    -79.38,
                    &[
                        ("name", "Tim Hortons"),
                        ("amenity", "cafe"),
                        ("addr:housenumber", "123"),
                        ("addr:street", "King Street West"),
                        ("addr:postcode", "M5V 1A1"),
                    ],
                ),
                node(
                    2002,
                    43.66,
                    -79.36,
                    &[("name", "Riverdale Farm"), ("tourism", "attraction")],
                ),
                node(
                    2003,
                    43.661,
                    -79.361,
                    &[("name", "Memorial Bench"), ("amenity", "bench")],
                ),
                node(
                    2004,
                    43.662,
                    -79.362,
                    &[
                        ("amenity", "cafe"),
                        ("addr:housenumber", "7"),
                        ("addr:street", "Mill Street"),
                    ],
                ),
                node(2005, 43.6450, -79.3810, &[]),
                node(2006, 43.6450, -79.3800, &[]),
                node(2007, 43.6456, -79.3800, &[]),
                node(2008, 43.6456, -79.3810, &[]),
                node(
                    2009,
                    43.2,
                    -79.8,
                    &[("name", "Tim Hortons"), ("amenity", "fast_food")],
                ),
            ],
        );
        blocks[3].push(way(
            700,
            &[2005, 2006, 2007, 2008, 2005],
            &[
                ("name", "Union Station"),
                ("railway", "station"),
                ("building", "train_station"),
            ],
        ));
        blocks
    }

    #[test]
    fn builds_named_pois_findable_by_name_locality_and_address() {
        let dir = temp_dir("pois");
        let input = dir.join("town.osm.pbf");
        write_pbf(&input, &town_with_pois());
        let report = build(&dir.join("pack"), &input, None);

        let reader = Reader::open(dir.join("pack")).expect("reader");
        let counts = &reader.manifest().layer_counts;
        assert_eq!(counts.get("poi"), Some(&4), "{counts:?}");
        assert_eq!(
            counts.get("address"),
            Some(&6),
            "the unnamed cafe stays an address; the named one does not"
        );
        assert_eq!(report.accepted.pois_with_address, 1);
        assert_eq!(report.accepted.poi_nodes, 3);
        assert_eq!(report.accepted.poi_way_centroids, 1);
        assert_eq!(report.accepted.poi_categories.get("amenity"), Some(&2));
        assert_eq!(report.geometry_resolution.poi_way_stubs, 1);

        // One record for the cafe, carrying its address.
        let ids = (0..reader.manifest().record_count)
            .map(|id| reader.record_summary(id).expect("summary").id)
            .collect::<Vec<_>>();
        assert_eq!(ids.iter().filter(|id| *id == "osm:node:2001").count(), 1);
        assert!(!ids.iter().any(|id| id == "osm:node:2003"), "bench");
        let cafe = reader
            .find_by_source_id("osm:node:2001")
            .expect("lookup")
            .expect("cafe");
        let Record::Poi(poi) = reader.records().record(cafe).expect("record") else {
            panic!("the cafe is a POI");
        };
        assert_eq!(poi.category, "amenity:cafe");
        assert_eq!(
            poi.address.expect("address").street.as_deref(),
            Some("King Street West")
        );

        let searcher = TextSearcher::open(dir.join("pack")).expect("searcher");
        let search = |query: &str, layer: Option<&str>| -> Vec<TextSearchHit> {
            searcher
                .search(TextSearchOptions {
                    query: query.into(),
                    limit: 5,
                    layer: layer.map(str::to_string),
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                })
                .expect("search")
        };
        let ids = |hits: &[TextSearchHit]| {
            hits.iter()
                .map(|hit| hit.record.id.clone())
                .collect::<Vec<_>>()
        };

        // By name and the locality it lies in, though its tags name none. The
        // locality leaves out the others, unless it matches nothing at all.
        let hits = search("Tim Hortons, Toronto", None);
        assert_eq!(ids(&hits), vec!["osm:node:2001"]);
        assert_eq!(search("Tim Hortons, Atlantis", None).len(), 2);
        assert_eq!(hits[0].record.layer, "poi");
        assert_eq!(hits[0].record.category.as_deref(), Some("amenity:cafe"));
        assert_eq!(
            hits[0].record.label,
            "Tim Hortons, 123 King Street West, Toronto, M5V 1A1"
        );
        assert_eq!(search("Tim Hortons", Some("poi")).len(), 2);
        assert_eq!(
            search("Riverdale Farm Toronto", Some("poi"))[0]
                .record
                .label,
            "Riverdale Farm, Toronto"
        );
        // By its address, also under the address layer filter.
        assert_eq!(
            ids(&search("123 King Street West", Some("address"))),
            vec!["osm:node:2001"]
        );
        assert!(search("Riverdale Farm", Some("address")).is_empty());
        let station = search("Union Station Toronto", Some("poi"));
        assert_eq!(ids(&station), vec!["osm:way:700"]);
        assert_eq!(
            station[0].record.point.expect("point").precision,
            PointPrecision::Centroid
        );
        assert!(search("Memorial Bench", None).is_empty());

        let suggestions = searcher
            .autocomplete(TextAutocompleteOptions {
                query: "riverdale fa".into(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("autocomplete");
        assert_eq!(ids(&suggestions), vec!["osm:node:2002"]);

        // Reverse answers with the cafe's address, as it did when the cafe was
        // an address record.
        let reverse = PackReverseGeocoder::open(dir.join("pack")).expect("reverse");
        let result = reverse
            .reverse(GeocodeOptions {
                lon: -79.38,
                lat: 43.65,
            })
            .expect("reverse")
            .result
            .expect("result");
        assert_eq!(result.match_kind, MatchKind::ExplicitAddress);
        assert_eq!(result.id.as_deref(), Some("osm:node:2001"));
        assert_eq!(result.label, "123 King Street West, M5V 1A1");
        assert_eq!(result.context.locality.as_deref(), Some("Toronto"));
    }

    #[test]
    fn joins_the_ways_of_a_street_and_labels_it_with_its_town() {
        let dir = temp_dir("street-join");
        let input = dir.join("streets.osm.pbf");
        let square = |first: i64, low: f64, high: f64| {
            [
                node(first, low, low, &[]),
                node(first + 1, low, high, &[]),
                node(first + 2, high, high, &[]),
                node(first + 3, high, low, &[]),
            ]
        };
        let area =
            |relation_id: i64, ways: i64, first: i64, level: &'static str, name: &'static str| {
                (
                    [
                        way(ways, &[first, first + 1, first + 2], &[]),
                        way(ways + 1, &[first + 2, first + 3, first], &[]),
                    ],
                    relation(
                        relation_id,
                        &[(ways, "outer"), (ways + 1, "outer")],
                        &[
                            ("boundary", "administrative"),
                            ("admin_level", level),
                            ("name", name),
                        ],
                    ),
                )
            };
        let road = |name: &'static str| [("highway", "residential"), ("name", name)];
        let mut nodes = Vec::new();
        nodes.extend(square(101, 0.0, 1.0));
        nodes.extend(square(111, 0.1, 0.4));
        nodes.extend(square(121, 0.6, 0.9));
        nodes.extend([
            // The town Kingston's point names the boundary it lies in; no
            // point names the local board.
            node(130, 0.75, 0.75, &[("place", "town"), ("name", "Kingston")]),
            node(201, 0.2, 0.2, &[]),
            node(202, 0.2, 0.201, &[]),
            node(203, 0.2, 0.202, &[]),
            node(204, 0.2005, 0.203, &[]),
            node(205, 0.3, 0.3, &[]),
            node(206, 0.3, 0.301, &[]),
            node(211, 0.7, 0.7, &[]),
            node(212, 0.7, 0.701, &[]),
            node(213, 0.7, 0.702, &[]),
        ]);
        let (district_ways, district) = area(900, 500, 101, "6", "Auckland");
        let (board_ways, board) = area(910, 510, 111, "8", "Waitematā");
        let (town_ways, town) = area(920, 520, 121, "8", "Kingston");
        let mut ways = Vec::new();
        ways.extend(district_ways);
        ways.extend(board_ways);
        ways.extend(town_ways);
        ways.extend([
            // One road drawn as three ways, then a road of its name far off.
            way(700, &[201, 202], &road("Ponsonby Road")),
            way(701, &[202, 203], &road("Ponsonby Road")),
            way(702, &[203, 204], &road("Ponsonby Road")),
            way(703, &[205, 206], &road("Ponsonby Road")),
            way(710, &[211, 212], &road("Main Street")),
            way(711, &[212, 213], &road("Main Street")),
        ]);
        write_pbf(&input, &[nodes, ways, vec![district, board, town]]);
        let report = build(&dir.join("pack"), &input, None);
        assert_eq!(report.accepted.street_segments, 6);
        assert_eq!(report.accepted.streets, 3);

        let reader = Reader::open(dir.join("pack")).expect("reader");
        let mut streets = reader
            .records_json_by_layer("street", 0)
            .expect("streets")
            .into_iter()
            .map(|street| {
                (
                    street["id"].as_str().expect("id").to_string(),
                    street["label"].as_str().expect("label").to_string(),
                    street["geometry"]["type"]
                        .as_str()
                        .expect("type")
                        .to_string(),
                )
            })
            .collect::<Vec<_>>();
        streets.sort();
        let street = |id: &str, label: &str, kind: &str| {
            (id.to_string(), label.to_string(), kind.to_string())
        };
        assert_eq!(
            streets,
            vec![
                street("osm:way:700", "Ponsonby Road, Auckland", "MultiLineString"),
                street("osm:way:703", "Ponsonby Road, Auckland", "LineString"),
                street("osm:way:710", "Main Street, Kingston", "MultiLineString"),
            ],
            "a street in a local board is named by its district, one in a town by the town"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn spilling_to_disk_builds_the_same_pack() {
        let dir = temp_dir("spill");
        let input = dir.join("town.osm.pbf");
        write_pbf(&input, &town_with_pois());
        let in_memory = build(&dir.join("memory"), &input, None);
        // A 4-byte budget leaves every sorter one item of memory, so everything
        // goes through run files and multi-round merges, and the node
        // coordinates through a scratch file.
        let spilled = build(&dir.join("spilled"), &input, Some(4));
        let spilling_sorters = spilled
            .scratch
            .sorters
            .values()
            .filter(|sorter| sorter.runs > 0)
            .count();
        assert!(spilling_sorters >= 3, "{:?}", spilled.scratch);
        assert!(spilled.scratch.spilled_bytes > 0);
        assert!(spilled.geometry_passes.mapped_location_bytes > 0);
        assert_eq!(in_memory.geometry_passes.mapped_location_bytes, 0);
        assert_eq!(in_memory.geometry_resolution, spilled.geometry_resolution);
        // Every scratch byte was counted and returned: the peak names its
        // owners, nothing is left live, and the Pack writer's files count.
        let ledger = &spilled.scratch;
        assert!(ledger.peak_live_bytes > 0 && !ledger.peak_owners.is_empty());
        assert!(
            ledger.spills.values().all(|spill| spill.live_bytes == 0),
            "{:?}",
            ledger.spills
        );
        for owner in ["records", "pack-records", "pack-spatial", "pack-text"] {
            let spill = &ledger.spills[owner];
            assert!(spill.written_bytes > 0, "{owner}: {spill:?}");
        }
        let records = &ledger.spills["records"];
        // Every run written, merged ones too, is read back once.
        assert_eq!(records.items_read, records.items_written);
        assert!(records.merge_passes >= 1);
        // Each stage that ran is in the report, in order.
        let stages = spilled
            .stages
            .iter()
            .map(|stage| stage.stage.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            stages,
            BuildStage::ALL
                .iter()
                .map(|stage| stage.description())
                .collect::<Vec<_>>()
        );
        let scan = &spilled.stages[0];
        assert_eq!(
            scan.inputs.values().map(|read| read.bytes).sum::<u64>(),
            spilled.input_bytes
        );
        assert_eq!(scan.units, spilled.input_bytes);
        assert!(scan.wall_ms <= spilled.phases.total_ms as u64);
        assert_eq!(in_memory.accepted, spilled.accepted);
        assert_eq!(in_memory.rejected, spilled.rejected);
        assert_eq!(in_memory.triage, spilled.triage);

        let memory = Reader::open(dir.join("memory")).expect("memory pack");
        let spilled = Reader::open(dir.join("spilled")).expect("spilled pack");
        assert_eq!(
            memory.manifest().record_count,
            spilled.manifest().record_count
        );
        for id in 0..memory.manifest().record_count {
            assert_eq!(
                memory.record_json(id).expect("memory record"),
                spilled.record_json(id).expect("spilled record")
            );
            assert_eq!(
                memory.boundary_context(id).expect("memory context"),
                spilled.boundary_context(id).expect("spilled context")
            );
        }
    }

    #[test]
    fn a_build_past_its_scratch_budget_stops_early_and_says_why() {
        let dir = temp_dir("scratch-budget");
        let input = dir.join("town.osm.pbf");
        write_pbf(&input, &town_with_pois());
        let options = BuildOsmOptions {
            inputs: vec![input],
            pack: dir.join("pack"),
            memory_budget_bytes: Some(4),
            scratch_budget_bytes: Some(1),
            ..BuildOsmOptions::default()
        };
        // The plan sees the budget cannot hold the build before it starts.
        let refused = build_osm_pack(options.clone());
        let Err(Error::OutOfResources(message)) = refused else {
            panic!("expected the plan to refuse the scratch budget, got {refused:?}");
        };
        assert!(message.contains("--scratch-budget-mb"), "{message}");
        // Forced past the plan, the ledger stops it at the first write.
        let failed = build_osm_pack(BuildOsmOptions {
            force: true,
            ..options
        });
        let Err(Error::OutOfResources(message)) = failed else {
            panic!("expected the scratch budget to stop the build, got {failed:?}");
        };
        assert!(
            message.contains("scratch disk is full in stage scan OSM objects"),
            "{message}"
        );
        assert!(message.contains("--scratch-budget-mb"), "{message}");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_inputs_whose_nodes_are_not_sorted() {
        let dir = temp_dir("unsorted");
        let input = dir.join("unsorted.osm.pbf");
        write_pbf(
            &input,
            &[
                vec![node(5, 43.6, -79.4, &[]), node(3, 43.6, -79.4, &[])],
                vec![way(
                    10,
                    &[3, 5],
                    &[("highway", "residential"), ("name", "Loop")],
                )],
            ],
        );
        let error = build_osm_pack(BuildOsmOptions {
            inputs: vec![input],
            pack: dir.join("pack"),
            ..BuildOsmOptions::default()
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("osmium sort"), "{error:#}");
        assert!(!dir.join("pack").join("CURRENT").exists());
    }

    /// Records what an observer is told, in order.
    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<(BuildStage, &'static str, Option<u64>)>>);

    impl BuildObserver for Recorder {
        fn started(&self, stage: BuildStage, total: Option<u64>) {
            self.0
                .lock()
                .expect("events")
                .push((stage, "started", total));
        }

        fn advanced(&self, units: u64) {
            let mut events = self.0.lock().expect("events");
            let stage = events.last().expect("a stage is running").0;
            events.push((stage, "advanced", Some(units)));
        }

        fn finished(&self, stage: BuildStage) {
            self.0
                .lock()
                .expect("events")
                .push((stage, "finished", None));
        }

        fn warned(&self, _: &str) {}
    }

    #[test]
    fn tells_the_observer_every_stage_in_order_with_its_work() {
        let dir = temp_dir("observed");
        let input = dir.join("town.osm.pbf");
        write_pbf(&input, &town());
        let recorder = Arc::new(Recorder::default());
        let report = build_osm_pack(BuildOsmOptions {
            inputs: vec![input.clone()],
            pack: dir.join("pack"),
            input_sources: vec!["town".into()],
            observer: Arc::clone(&recorder) as Arc<dyn BuildObserver>,
            ..BuildOsmOptions::default()
        })
        .expect("build");
        assert_eq!(
            report
                .inputs
                .iter()
                .map(|input| input.source.as_deref())
                .collect::<Vec<_>>(),
            [Some("town")]
        );
        assert!(report.resources.threads > 0);
        assert_eq!(report.datasets, [OSM_DATASET]);

        let events = recorder.0.lock().expect("events");
        let order: Vec<BuildStage> = events
            .iter()
            .filter(|(_, event, _)| *event == "started")
            .map(|(stage, _, _)| *stage)
            .collect();
        assert_eq!(order, BuildStage::ALL);
        for stage in BuildStage::ALL {
            let total = events
                .iter()
                .find(|(own, event, _)| *own == stage && *event == "started")
                .and_then(|(_, _, total)| *total);
            let done: u64 = events
                .iter()
                .filter(|(own, event, _)| *own == stage && *event == "advanced")
                .filter_map(|(_, _, units)| *units)
                .sum();
            match stage.unit() {
                StageUnit::None => assert_eq!(total, None, "{stage:?}"),
                _ => assert_eq!(total, Some(done), "{stage:?} counts all its work"),
            }
            assert!(
                events.contains(&(stage, "finished", None)),
                "{stage:?} finishes"
            );
        }
        assert_eq!(
            events
                .iter()
                .find(|(stage, event, _)| *stage == BuildStage::Scan && *event == "started")
                .and_then(|(_, _, total)| *total),
            Some(fs::metadata(&input).expect("input").len()),
            "the scan is measured in bytes of the input"
        );
    }

    #[test]
    fn a_cancelled_build_publishes_nothing() {
        let dir = temp_dir("cancelled");
        let input = dir.join("town.osm.pbf");
        write_pbf(&input, &town());
        let error = build_osm_pack(BuildOsmOptions {
            inputs: vec![input],
            pack: dir.join("pack"),
            cancel: Arc::new(AtomicBool::new(true)),
            ..BuildOsmOptions::default()
        })
        .unwrap_err();
        assert!(matches!(error, Error::Cancelled));
        assert!(!dir.join("pack").join("CURRENT").exists());
    }

    /// Eruera Street with flats written "N/M", some addresses stating a
    /// postcode, and a second street in another cell.
    fn flats() -> Vec<Vec<TestElement>> {
        let address = |id, lon, number: &'static str, postcode: Option<&'static str>| {
            let mut tags = vec![
                ("addr:housenumber", number),
                ("addr:street", "Eruera Street"),
            ];
            if let Some(postcode) = postcode {
                tags.push(("addr:postcode", postcode));
            }
            node(id, -38.1386, lon, &tags)
        };
        vec![vec![
            node(1, -38.1388, 176.2580, &[]),
            node(2, -38.1388, 176.2430, &[]),
            way(
                100,
                &[1, 2],
                &[("highway", "residential"), ("name", "Eruera Street")],
            ),
            address(10, 176.2570, "1000", Some("3010")),
            address(11, 176.2510, "1182", None),
            address(12, 176.2509, "2/1182", None),
            address(13, 176.2456, "1345", Some("3015")),
            address(14, 176.2451, "1367", None),
            address(15, 176.24505, "10/1367", None),
            address(16, 176.24506, "2/1367", None),
            address(17, 176.2440, "1400", None),
            node(
                18,
                -38.30,
                176.30,
                &[("addr:housenumber", "7"), ("addr:street", "Tui Road")],
            ),
        ]]
    }

    /// Rows over `flats`: repeats, disputes, other houses, slash numbers and
    /// postcodes to lend, in two files.
    fn flat_rows(dir: &Path) -> Vec<addresses::AddressFile> {
        let first = dir.join("first.csv");
        fs::write(
            &first,
            "LON,LAT,NUMBER,STREET,UNIT,CITY,POSTCODE\n\
             176.24506,-38.13861,1367,Eruera Street,10,,\n\
             176.2510,-38.1416,1182,Eruera Street,,Rotorua,\n\
             176.2510,-38.1356,1182,Eruera Street,,,3999\n\
             176.2440,-38.13862,1400,ERUERA ST,,,3015\n\
             176.24401,-38.13863,1400,Eruera Street,,,\n\
             176.2452,-38.1387,3/1345,Eruera Street,,,\n\
             176.2453,-38.1387,1351,Eruera Street,,,\n\
             176.2454,-38.1387,1353,Eruera Street,,Rotorua,3016\n\
             176.30001,-38.30001,7,Tui Road,,,3020\n\
             176.3003,-38.3003,9,Tui Road,,,\n\
             176.80,-38.80,5,Kowhai Lane,,,\n",
        )
        .expect("csv");
        let second = dir.join("second.csv");
        fs::write(
            &second,
            "LON,LAT,NUMBER,STREET,UNIT,CITY,POSTCODE\n\
             176.80002,-38.80001,5,Kowhai Lane,,,\n\
             176.8001,-38.8001,6,Kowhai Lane,,,3030\n\
             176.24506,-38.13861,1367,Eruera Street,2,,\n",
        )
        .expect("csv");
        vec![
            addresses::AddressFile {
                dataset: "openaddresses:first".to_string(),
                path: first,
            },
            addresses::AddressFile {
                dataset: "openaddresses:second".to_string(),
                path: second,
            },
        ]
    }

    /// Every record of the Pack `inputs` and `files` build into under
    /// `root`, its context, and what the report counted.
    fn pack_contents(
        root: &Path,
        inputs: &[&Path],
        files: Vec<addresses::AddressFile>,
        budget: Option<usize>,
    ) -> Vec<String> {
        let report = build_osm_pack(BuildOsmOptions {
            inputs: inputs.iter().map(|input| input.to_path_buf()).collect(),
            pack: root.to_path_buf(),
            memory_budget_bytes: budget,
            force: budget.is_some(),
            addresses: (!files.is_empty())
                .then(|| Arc::new(addresses::AddressFiles(files)) as Arc<dyn AddressSource>),
            ..BuildOsmOptions::default()
        })
        .expect("build");
        let reader = Reader::open(root).expect("pack");
        let mut imports = report.imports.clone();
        for import in &mut imports {
            import.path.clear();
        }
        let mut contents = vec![
            format!("{:?}", report.accepted),
            format!("{:?}", report.rejected),
            format!("{:?}", report.triage),
            format!("{:?}", report.quality),
            format!("{:?}", report.geometry_resolution),
            format!("{:?}", report.slash_numbers),
            format!("{:?}", report.datasets),
            format!("{imports:?}"),
            format!(
                "{:?}",
                report
                    .scratch
                    .sorters
                    .iter()
                    .map(|(name, sorter)| (name.clone(), sorter.items))
                    .collect::<Vec<_>>()
            ),
            format!("{:?}", reader.manifest().layer_counts),
        ];
        for id in 0..reader.manifest().record_count {
            contents.push(format!(
                "{} {:?} {:?}",
                reader.record_json(id).expect("record"),
                reader.records().record(id).expect("record"),
                reader.boundary_context(id).expect("context")
            ));
        }
        contents
    }

    /// Imported rows over OSM flats written "N/M": a row naming a flat by
    /// its unit repeats the OSM flat once its slash number is read, rows
    /// repeat each other across files, rows a few hundred metres off
    /// dispute OSM's house, a row's slash number is read too, and only the
    /// postcodes of the addresses kept are lent to their street. Spilling
    /// every sorter builds the same Pack.
    #[test]
    fn imported_rows_repeat_dispute_and_lend_by_their_place_in_the_address_book() {
        let dir = temp_dir("address-book");
        let input = dir.join("flats.osm.pbf");
        write_pbf(&input, &flats());
        let files = flat_rows(&dir);
        let in_memory = pack_contents(&dir.join("memory"), &[&input], files.clone(), None);
        let spilled = pack_contents(&dir.join("spilled"), &[&input], files.clone(), Some(4));
        assert_eq!(in_memory, spilled);

        let report = build_osm_pack(BuildOsmOptions {
            inputs: vec![input],
            pack: dir.join("pack"),
            addresses: Some(Arc::new(addresses::AddressFiles(files))),
            ..BuildOsmOptions::default()
        })
        .expect("build");
        let counts = report
            .imports
            .iter()
            .map(|import| (import.dataset.as_str(), import.duplicates, import.disputes))
            .collect::<Vec<_>>();
        assert_eq!(
            counts,
            [
                ("openaddresses:first", 4, 2),
                ("openaddresses:second", 2, 0)
            ]
        );
        assert_eq!(
            report.slash_numbers,
            SlashNumberCounts {
                unit_first: 4,
                ..SlashNumberCounts::default()
            },
            "three OSM flats and one imported"
        );

        let reader = Reader::open(dir.join("pack")).expect("reader");
        let address = |id: &str| {
            let record_id = reader.find_by_source_id(id).expect("lookup")?;
            match reader.records().record(record_id).expect("record") {
                Record::Address(address) => Some(address),
                other => panic!("{id} is an address: {other:?}"),
            }
        };
        for dropped in [
            // Unit 10 of 1367: OSM's "10/1367".
            "openaddresses:first:row:1",
            // 330 m from OSM's 1182, which states no locality or postcode.
            "openaddresses:first:row:2",
            "openaddresses:first:row:3",
            "openaddresses:first:row:4",
            "openaddresses:first:row:5",
            "openaddresses:first:row:9",
            "openaddresses:second:row:1",
            "openaddresses:second:row:3",
        ] {
            assert!(address(dropped).is_none(), "{dropped} repeats a house");
        }
        assert!(address("osm:node:11").expect("1182").disputed);
        assert!(!address("osm:node:14").expect("1367").disputed);
        let flat = address("openaddresses:first:row:6").expect("3/1345");
        assert_eq!(
            (flat.address.number.as_str(), flat.address.unit.as_deref()),
            ("1345", Some("3"))
        );
        assert_eq!(
            address("osm:node:14").expect("1367").street_postcodes,
            ["3010", "3015", "3016"],
            "OSM's postcodes and the kept row's 3016"
        );
        for unlent in ["osm:node:18", "openaddresses:first:row:10"] {
            assert!(
                address(unlent)
                    .expect("Tui Road")
                    .street_postcodes
                    .is_empty(),
                "{unlent}: the row stating 3020 repeats OSM's 7 Tui Road"
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    /// Every record and the context of each, in Pack order.
    fn records_and_contexts(pack: &Path) -> Vec<(serde_json::Value, Option<RecordContext>)> {
        let reader = Reader::open(pack).expect("reader");
        (0..reader.manifest().record_count)
            .map(|id| {
                (
                    reader.record_json(id).expect("record"),
                    reader.boundary_context(id).expect("context"),
                )
            })
            .collect()
    }

    /// Two extracts and an address file, so a build runs every step:
    /// both inputs' passes, the import, the address book and the houses.
    fn resumable_inputs(dir: &Path) -> BuildOsmOptions {
        let (first, second) = (dir.join("town.osm.pbf"), dir.join("other.osm.pbf"));
        write_pbf(&first, &town_with_pois());
        write_pbf(&second, &other_country());
        let csv = dir.join("addresses.csv");
        fs::write(
            &csv,
            "LON,LAT,NUMBER,STREET,CITY,POSTCODE\n\
             -79.40005,43.60003,10,KING ST,Toronto,M5V 1A1\n\
             -79.4003,43.6003,16,King Street,Toronto,M5V 1A1\n\
             -79.40,43.604,10,King Street,Toronto,M5V 1A1\n\
             -79.45,43.65,3,Queen Street,Toronto,\n",
        )
        .expect("csv");
        BuildOsmOptions {
            inputs: vec![first, second],
            addresses: Some(Arc::new(addresses::AddressFiles(vec![
                addresses::AddressFile {
                    dataset: "openaddresses:test".to_string(),
                    path: csv,
                },
            ]))),
            ..BuildOsmOptions::default()
        }
    }

    /// Build `options`, failing just before the checkpoint of `step` as a
    /// crash there would.
    fn fail_before(options: BuildOsmOptions, step: Step) -> Error {
        FAIL_BEFORE_CHECKPOINT.set(Some(step));
        let failed = build_osm_pack(options);
        FAIL_BEFORE_CHECKPOINT.set(None);
        failed.expect_err("the injected failure stops the build")
    }

    /// A build that stops at any step and is resumed makes the Pack an
    /// uninterrupted build makes: every record, in the same order, with
    /// the same context. In memory and spilling everything, coordinates
    /// mapped from scratch included.
    #[test]
    fn a_build_resumed_after_any_step_makes_the_same_pack() {
        for budget in [Some(4), None] {
            let dir = temp_dir("resume");
            let options = BuildOsmOptions {
                memory_budget_bytes: budget,
                force: budget.is_some(),
                ..resumable_inputs(&dir)
            };
            let whole = build_osm_pack(BuildOsmOptions {
                pack: dir.join("whole"),
                ..options.clone()
            })
            .expect("uninterrupted build");
            assert_eq!(whole.checkpoints.count, Step::all(2).len() as u64);
            assert!(whole.resumes.is_empty());
            let expected = records_and_contexts(&dir.join("whole"));
            let steps = Step::all(2);
            for (at, step) in steps.iter().enumerate() {
                let pack = dir.join(format!("stopped-{at}"));
                let options = BuildOsmOptions {
                    pack: pack.clone(),
                    ..options.clone()
                };
                let error = fail_before(options.clone(), *step);
                assert!(format!("{error}").contains("injected"), "{error}");
                let found = resume::find(&pack).expect("find");
                if at == 0 {
                    assert!(found.is_empty(), "nothing is left before a checkpoint");
                } else {
                    assert_eq!(found.len(), 1, "{step:?}");
                    assert_eq!(found[0].stage, Some(steps[at - 1].describe(2)));
                    assert!(found[0].bytes > 0);
                    assert!(!pack.join("CURRENT").exists());
                }
                let resumed = build_osm_pack(BuildOsmOptions {
                    leftover: Leftover::Resume,
                    ..options
                })
                .unwrap_or_else(|error| panic!("resumed after {step:?}: {error}"));
                assert_eq!(
                    records_and_contexts(&pack),
                    expected,
                    "the Pack resumed before the checkpoint of {step:?}"
                );
                assert_eq!(resumed.accepted, whole.accepted, "{step:?}");
                assert_eq!(resumed.imports, whole.imports, "{step:?}");
                assert_eq!(resumed.output.record_count, whole.output.record_count);
                assert_eq!(resumed.output.rejection_count, whole.output.rejection_count);
                assert_eq!(resumed.checkpoints.count, whole.checkpoints.count);
                if at > 0 {
                    let resume = &resumed.resumes[0];
                    assert_eq!(resume.after, steps[at - 1].describe(2));
                    assert!(resume.reused_bytes > 0, "{resume:?}");
                }
                assert!(
                    resume::find(&pack).expect("find").is_empty(),
                    "a completed build leaves nothing to resume"
                );
                let _ = fs::remove_dir_all(&pack);
            }
            // Past the last checkpoint, the sealed Pack is trained again.
            let pack = dir.join("untrained");
            let options = BuildOsmOptions {
                pack: pack.clone(),
                ..options.clone()
            };
            FAIL_BEFORE_TRAINING.set(true);
            let failed = build_osm_pack(options.clone());
            FAIL_BEFORE_TRAINING.set(false);
            assert!(failed.is_err());
            let resumed = build_osm_pack(BuildOsmOptions {
                leftover: Leftover::Resume,
                ..options
            })
            .expect("resumed before the training");
            assert_eq!(resumed.resumes[0].after, "seal the Pack");
            assert_eq!(records_and_contexts(&pack), expected);
            assert_eq!(
                resumed.parser_training.trained,
                whole.parser_training.trained
            );
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_stopped_build_is_reported_and_refuses_changed_inputs() {
        let dir = temp_dir("resume-refused");
        let options = BuildOsmOptions {
            pack: dir.join("pack"),
            ..resumable_inputs(&dir)
        };
        fail_before(options.clone(), Step::Contexts);

        // Neither resumed nor cleared, it is reported, with both commands.
        let refused = build_osm_pack(options.clone()).expect_err("refused");
        let message = refused.to_string();
        assert!(message.contains("import addresses"), "{message}");
        assert!(
            message.contains("--resume") && message.contains("--fresh"),
            "{message}"
        );

        // An input that changed since refuses to resume, naming it.
        write_pbf(&options.inputs[1], &town());
        let changed = build_osm_pack(BuildOsmOptions {
            leftover: Leftover::Resume,
            ..options.clone()
        })
        .expect_err("a changed input");
        let message = changed.to_string();
        assert!(message.contains("input 2"), "{message}");
        assert!(message.contains("--fresh"), "{message}");
        assert_eq!(resume::find(&options.pack).expect("find").len(), 1);

        // Cleared, the build starts over and nothing is left.
        let fresh = build_osm_pack(BuildOsmOptions {
            leftover: Leftover::Clear,
            ..options.clone()
        })
        .expect("fresh build");
        assert!(fresh.resumes.is_empty());
        assert!(resume::find(&options.pack).expect("find").is_empty());
        let generations = fs::read_dir(options.pack.join("generations"))
            .expect("generations")
            .count();
        assert_eq!(generations, 1, "the stopped build's generation is gone");
        let _ = fs::remove_dir_all(dir);
    }
}
