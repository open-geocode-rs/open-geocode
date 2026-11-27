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
pub use super::places::overture::Report as OvertureReport;
#[cfg(test)]
pub(super) use poi::resolve as resolve_pois;
mod postcode;
mod scan;
mod slash;
mod spill;
mod street;
mod tags;
mod towns;

#[cfg(test)]
pub(crate) mod test_pbf;
mod tiger;
mod timezone;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt,
    fs::{self, File},
    io::{BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use crate::error::{Error, Result};
use rayon::prelude::*;

use crate::{
    alloc,
    builder::{
        addresses::{self, AddressFile, AddressSource},
        places::{Identity, Place, overture},
        plan::{BuildPlan, ItemCounts, Scanned, plan_build_holding},
        progress::{BuildObserver, BuildStage, Progress, TerminalObserver},
        qrank::QRank,
        report::{
            BUILD_REPORT_SCHEMA_VERSION, BuilderReport, CheckpointReport, QRankReport, ReportInput,
            ResumeReport, SlashNumberCounts, SorterReport, StageReport, TimezoneReport,
        },
        resume::{self, FORMAT_VERSION, FileStamp, Fingerprint, Found, Leftover, Manifest},
    },
    enclosing::AreaTags,
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
        CountryCode, dictionary_key,
        train::{self, Sampler},
        weights::{WEIGHTS_SECTION, WEIGHTS_VERSION},
    },
    record::{
        Extent, OSM_DATASET, ObjectType, PlaceLayer, PlaceRecord, Record, RejectedRecord,
        StreetRecord,
    },
    records::quantize,
    resources::Resources,
    text_index::PostcodeAreas,
    util::{geo::point_lon_lat, hilbert::hilbert_key},
};
use book::{Book, BookFile};
use boundary::{
    BoundaryBuilder, BoundaryIndex, BoundaryRelationStub, BoundarySet, ContextOrigin, SetItem,
    Settlement, SourceContext, TownDot, Vertex, record_context,
};
use checkpoint::{
    AddressState, AuditState, ContextState, GeometryItem, Id, InputState, KeptWays, NamedPoint,
    Pass, ScanCounts, State, Step,
};
use containment::Shapes;
use emitted::{Emitted, Ready};
pub(crate) use locations::{
    CHUNK_BYTES as NODE_SET_CHUNK_BYTES, LOCATION_BYTES as NODE_SLOT_BYTES, LoadStats,
    TABLE_BYTES as NODE_SET_TABLE_BYTES,
};
use locations::{ChunkBits, Locations, NodeSet, Slots};
use pbf::for_each_block;
pub use pbf::replication_timestamp;
use scan::scan_block;
use spill::{
    Body, Datasets, House, Lent, PendingAddress, PendingNamed, PendingRecord, PlacedAddress,
    WayKind,
};
use timezone::Timezones;

const WRITE_BATCH_RECORDS: usize = 16_384;

pub(crate) const BOUNDARY_BUILD_BYTES_PER_VERTEX: usize = 160;
pub(crate) const MIN_BOUNDARY_BATCH_BYTES: usize = 128 << 20;
const MAX_BOUNDARY_BATCH_BYTES: usize = 1 << 30;

#[derive(Clone)]
pub struct BuildOsmOptions {
    pub inputs: Vec<PathBuf>,
    pub pack: PathBuf,
    pub memory_budget_bytes: Option<usize>,
    pub scratch_dir: Option<PathBuf>,
    pub scratch_budget_bytes: Option<u64>,
    pub force: bool,
    pub leftover: Leftover,
    pub input_sources: Vec<String>,
    pub observer: Arc<dyn BuildObserver>,
    /// Set to stop the build: it then fails with
    /// [`crate::builder::BuildCancelled`] and publishes nothing, keeping its
    /// last checkpoint to resume from.
    pub cancel: Arc<AtomicBool>,
    pub addresses: Option<Arc<dyn AddressSource>>,
    pub imported_rows: u64,
    pub qrank: Option<PathBuf>,
    pub timezones: Option<PathBuf>,
    pub overture: bool,
    pub overture_verified: Option<PathBuf>,
    pub tiger: bool,
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
            qrank: None,
            timezones: None,
            overture: false,
            overture_verified: None,
            tiger: false,
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
            .field("qrank", &self.qrank)
            .field("timezones", &self.timezones)
            .field("overture", &self.overture)
            .field("overture_verified", &self.overture_verified)
            .field("tiger", &self.tiger)
            .finish_non_exhaustive()
    }
}

pub fn plan_osm_build(options: &BuildOsmOptions) -> Result<BuildPlan> {
    if options.overture_verified.is_some() && !options.overture {
        return Err(Error::BadInput(
            "--overture-verified requires --overture".into(),
        ));
    }
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

fn total_input_bytes(inputs: &[PathBuf]) -> Result<u64> {
    inputs
        .iter()
        .try_fold(0, |total, input| Ok(total + pbf::input_bytes(input)?))
}

const CHECKPOINT_EVERY: Duration = if cfg!(test) {
    Duration::ZERO
} else {
    Duration::from_secs(300)
};

pub fn build_osm_pack(options: &BuildOsmOptions) -> Result<BuilderReport> {
    let plan = plan_osm_build(options)?;
    build_planned_osm_pack(options, plan)
}

#[cfg(test)]
thread_local! {
    static FAIL_BEFORE_CHECKPOINT: std::cell::Cell<Option<Step>> =
        const { std::cell::Cell::new(None) };
    static FAIL_BEFORE_TRAINING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Build with a plan made by [`plan_osm_build`]. The caller sizes the
/// process's rayon pool to `plan.threads` before any parallel work, since
/// one pool serves every stage.
pub fn build_planned_osm_pack(options: &BuildOsmOptions, plan: BuildPlan) -> Result<BuilderReport> {
    for warning in plan.shortfalls.iter().chain(&plan.warnings) {
        options.observer.warned(warning);
    }
    let mut run = Run::start(options, plan)?;
    let mut checkpointed = Instant::now();
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
        if checkpointed.elapsed() >= CHECKPOINT_EVERY {
            run.checkpoint(step)?;
            checkpointed = Instant::now();
        }
    }
    run.finish()
}

struct Run<'a> {
    options: &'a BuildOsmOptions,
    plan: BuildPlan,
    budget: usize,
    memory: Arc<Budget>,
    scratch: Arc<Scratch>,
    progress: Progress,
    fingerprint: Fingerprint,
    started: Instant,
    elapsed_before: Duration,
    done: Option<Step>,
    stages_before: Vec<StageReport>,
    build: Build,
    sorted: BTreeMap<String, SorterReport>,
    scanned: Option<ScanCounts>,
    geometry: Geometry,
    geometry_saved: Vec<SpoolFile<GeometryItem>>,
    input: Option<InputPasses>,
    boundaries: Option<BoundarySet>,
    boundaries_saved: Option<SpoolFile<SetItem>>,
    boundary_shapes: Option<Arc<Shapes>>,
    boundary_memory: Option<Reservation>,
    imports: Option<Vec<AddressFile>>,
    contexts: Option<Contexts>,
    address_book: Option<BookFile<PendingAddress>>,
    verdicts: Option<ExternalSorter<(u64, u64)>>,
    books: Option<AddressBook>,
    postcodes: Option<SpoolFile<NamedPoint>>,
    sample: Option<Sampler>,
    sample_saved: Option<ScratchFile>,
    sealing: Option<Sealing>,
    sealed: Option<(SealedPack, BuildStats)>,
    indexing: Option<Instant>,
    checkpoints: CheckpointReport,
    generation: Option<Generation>,
}

struct Contexts {
    ordered: SpoolFile<PendingRecord>,
    boundary_ids: Vec<RecordId>,
    country_ids: HashMap<String, RecordId>,
    settled: HashSet<RecordId>,
    index: BoundaryIndex,
    area_names: HashMap<RecordId, Vec<String>>,
    area_tags: BTreeMap<RecordId, AreaTags>,
    country_codes: HashMap<RecordId, CountryCode>,
    subdivision_codes: HashMap<RecordId, String>,
    plate_codes: HashMap<RecordId, String>,
    default_languages: HashMap<RecordId, String>,
    language_names: HashMap<RecordId, crate::pack::LanguageNames>,
    extents: Vec<(RecordId, Extent)>,
    region_groups: HashMap<RecordId, Vec<RecordId>>,
}

enum TownReach {
    Votes(Vec<SpoolFile<NamedPoint>>),
    Measured(HashMap<RecordId, u32>),
}

impl Contexts {
    fn derive(
        boundaries: BoundarySet,
        ordered: SpoolFile<PendingRecord>,
        boundary_ids: Vec<RecordId>,
        country_ids: HashMap<String, RecordId>,
        mut settled: HashSet<RecordId>,
        reach: TownReach,
    ) -> Result<Self> {
        let country_codes = boundaries.country_codes(&boundary_ids, &country_ids);
        let subdivision_codes = boundaries.subdivision_codes(&boundary_ids);
        let plate_codes = boundaries.plate_codes(&boundary_ids);
        let default_languages = boundaries.default_languages(&boundary_ids);
        let language_names = boundaries.language_names(&boundary_ids);
        let extents = boundaries.extents(&boundary_ids);
        let area_names = boundaries.context_names(&boundary_ids, &country_ids);
        let area_tags = boundaries.area_tags(&boundary_ids);
        let region_groups = boundaries.region_groups(&boundary_ids);
        let mut index = boundaries.into_index(&boundary_ids, &country_ids);
        // Only the own name counts; other names may translate a town, a district shares its seat's.
        let mut points = Vec::new();
        for (record_id, item) in ordered.reader()?.enumerate() {
            let item = item?;
            if item.origin.is_none()
                && let Body::Record(record) = &item.body
                && let Record::Place(_, place) = record.as_ref()
                && street::is_settlement(&place.place_type)
                && let Some(point) = point_lon_lat(&place.geometry)
            {
                points.push((record_id as RecordId, place.clone(), point));
            }
        }
        let holders = points
            .par_iter()
            .map(|(_, place, [lon, lat])| {
                let names = std::iter::once(&place.name)
                    .chain(&place.alt_names)
                    .filter_map(|name| dictionary_key(name))
                    .collect::<HashSet<_>>();
                index.smallest_area_at(*lon, *lat, |area| {
                    area_names
                        .get(&area)
                        .and_then(|names| names.first())
                        .and_then(|name| dictionary_key(name))
                        .is_some_and(|name| names.contains(&name))
                })
            })
            .collect::<Vec<_>>();
        let held = holders.iter().flatten().copied().collect::<HashSet<_>>();
        let mut towns_in = points
            .par_iter()
            .enumerate()
            .filter_map(|(at, (_, place, [lon, lat]))| {
                let rank = boundary::town_rank(&place.place_type)?;
                Some(
                    index
                        .areas_at(*lon, *lat)
                        .filter(|area| held.contains(area))
                        .map(|area| (area, (at, rank)))
                        .collect::<Vec<_>>(),
                )
            })
            .flatten_iter()
            .fold(
                HashMap::<RecordId, Vec<(usize, u8)>>::new,
                |mut towns, (area, town)| {
                    towns.entry(area).or_default().push(town);
                    towns
                },
            )
            .reduce(HashMap::new, |mut left, right| {
                for (area, towns) in right {
                    left.entry(area).or_default().extend(towns);
                }
                left
            });
        for (own, rank, [lon, lat]) in index.settlement_areas() {
            for area in index
                .areas_at(lon, lat)
                .filter(|area| *area != own && held.contains(area))
            {
                towns_in.entry(area).or_default().push((usize::MAX, rank));
            }
        }
        let holders = holders
            .into_iter()
            .zip(&points)
            .enumerate()
            .map(|(at, (holder, (_, place, _)))| {
                let rank = boundary::town_rank(&place.place_type).unwrap_or(u8::MAX);
                holder.filter(|area| {
                    towns_in.get(area).is_none_or(|towns| {
                        towns
                            .iter()
                            .all(|(other, other_rank)| *other == at || *other_rank > rank)
                    })
                })
            })
            .collect::<Vec<_>>();
        let mut named = Vec::new();
        let mut dots = Vec::new();
        let mut dot_names = Vec::new();
        for ((record_id, place, point), holder) in points.into_iter().zip(holders) {
            let settlement = boundary::town_rank(&place.place_type).map(|rank| Settlement {
                rank,
                prominence: place.prominence,
            });
            match (holder, settlement) {
                (Some(holder), settlement) => {
                    settled.insert(holder);
                    named.extend(settlement.map(|settlement| (holder, settlement)));
                }
                (None, Some(settlement)) => {
                    dot_names.push(
                        std::iter::once(&place.name)
                            .chain(&place.alt_names)
                            .filter_map(|name| dictionary_key(name))
                            .collect::<Vec<_>>(),
                    );
                    dots.push(TownDot::new(record_id, point, place.name, settlement));
                }
                (None, None) => {}
            }
        }
        match reach {
            TownReach::Votes(votes) => towns::measure_reach(
                &mut dots,
                &dot_names,
                votes.into_iter().flat_map(SpoolFile::into_reader),
            )?,
            TownReach::Measured(measured) => {
                for dot in &mut dots {
                    dot.reach_m = measured.get(&dot.record_id).copied().map_or(0.0, f64::from);
                }
            }
        }
        index.add_towns(named, dots);
        Ok(Self {
            ordered,
            boundary_ids,
            country_ids,
            settled,
            index,
            area_names,
            area_tags,
            country_codes,
            subdivision_codes,
            plate_codes,
            default_languages,
            language_names,
            extents,
            region_groups,
        })
    }
}

impl<'a> Run<'a> {
    fn start(options: &'a BuildOsmOptions, plan: BuildPlan) -> Result<Self> {
        let mut fingerprint = Fingerprint::of(
            &options.inputs,
            &options.input_sources,
            options
                .addresses
                .as_ref()
                .map(|source| format!("{source:?}")),
        )?;
        let leftover = settle_leftovers(options)?;
        fingerprint.tiger = options.tiger;
        fingerprint.overture_policy = options.overture.then_some(overture::PUBLICATION_POLICY);
        fingerprint.overture_verified = options
            .overture_verified
            .as_deref()
            .map(crate::builder::resume::FileStamp::of)
            .transpose()?;
        if options.overture && leftover.is_none() {
            pbf::overture_bounds(&options.inputs)?;
            fingerprint.overture = Some(overture::latest_release()?);
        }
        fingerprint.timezones = options
            .timezones
            .as_deref()
            .map(crate::builder::resume::FileStamp::of)
            .transpose()?;
        match leftover {
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

    fn new(
        options: &'a BuildOsmOptions,
        plan: BuildPlan,
        fingerprint: Fingerprint,
        generation: Generation,
        scratch: Arc<Scratch>,
        build: Build,
    ) -> Self {
        let budget = usize::try_from(plan.sorter_bytes).unwrap_or(usize::MAX);
        scratch.cancel_on(Arc::clone(&options.cancel));
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
            boundary_shapes: None,
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
            places: Some(ExternalSorter::new(&scratch, "places", &memory, budget)),
            context_records: Some(context_sorter(&scratch, &memory, budget)),
            town_votes: Some(Spool::create(&scratch, TOWN_VOTES)?),
            town_votes_saved: Vec::new(),
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
        let manifest: Manifest<Box<State>> = serde_json::from_slice(&manifest)?;
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
        if options.overture {
            fingerprint.overture =
                Some(manifest.fingerprint.overture.clone().ok_or_else(|| {
                    Error::BadOptions(
                        "the stopped build did not use --overture; pass --fresh to change it"
                            .into(),
                    )
                })?);
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
        fingerprint
            .imported
            .clone_from(&manifest.fingerprint.imported);
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
            places: sorter_of(sorters, "places", &scratch, &memory, budget)?,
            context_records: sorter_of(
                sorters,
                CONTEXT_RECORDS,
                &scratch,
                &memory,
                context_share(budget),
            )?,
            town_votes: if state.contexts.is_none() {
                Some(Spool::create(&scratch, TOWN_VOTES)?)
            } else {
                None
            },
            town_votes_saved: state
                .town_votes
                .iter()
                .map(|part| SpoolFile::restore(&scratch, part))
                .collect::<Result<Vec<_>>>()?,
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
            if let Some((book, lent)) = &addresses.placed {
                run.books = Some(AddressBook {
                    addresses: BookFile::restore(&scratch, book)?,
                    lent: BookFile::restore(&scratch, lent)?,
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
            let Some(shapes) = &state.boundary_shapes else {
                return Err(Error::BadOptions(
                    format!(
                        "the build that stopped in {} saved its boundaries without their shape file, as an older og did; pass --fresh to clear it and start over",
                        found.scratch.display()
                    )
                    .into(),
                ));
            };
            let shapes = Arc::new(Shapes::restore(&scratch, shapes)?);
            let saved = SpoolFile::restore(&scratch, saved)?;
            let boundaries = BoundarySet::load(&saved, Arc::clone(&shapes))?;
            run.boundary_memory = Some(Reservation::forced(&run.memory, boundaries.heap_bytes()));
            run.boundary_shapes = Some(shapes);
            match &state.contexts {
                Some(contexts) => {
                    run.contexts = Some(Contexts::derive(
                        boundaries,
                        SpoolFile::restore(&scratch, &contexts.ordered)?,
                        contexts.boundary_ids.clone(),
                        contexts.country_ids.clone().into_iter().collect(),
                        contexts.settled.iter().copied().collect(),
                        TownReach::Measured(contexts.town_reach.iter().copied().collect()),
                    )?);
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
            sorter_bytes: self.budget,
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
            Step::Streets => {
                self.join_streets()?;
                self.import_overture()?;
                self.resolve_places()
            }
            Step::AddressBook => self.write_address_book(),
            Step::Houses => self.find_repeated_houses(),
            Step::Addresses => self.place_addresses(),
            Step::Records => self.write_records(),
            Step::Seal(_) => self.seal_part(),
            Step::Sealed => self.seal(),
        }
    }

    fn scan(&mut self, index: usize) -> Result<()> {
        let input = &self.options.inputs[index];
        let phase = Instant::now();
        let nodes = NodeSet::new(&self.memory);
        let mut interpolation_anchor_ids = Vec::new();
        let mut node_blobs = Vec::new();
        let mut way_blobs = Vec::new();
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
                let mut output = scan_block(&block);
                output.emitted.prepare()?;
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
                            interpolation_anchor_ids.extend_from_slice(&refs);
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
        self.save_geometry(&part)?;
        self.input = Some(InputPasses {
            index,
            nodes: Some(nodes),
            tag_ids: interpolation_anchor_ids,
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
            resources,
        })?;
        for warning in self.plan.shortfalls.iter().chain(&self.plan.warnings) {
            self.options.observer.warned(warning);
        }
        build.report.geometry_passes.kept_ways += kept.all;
        build.report.resources.plan = self.plan.clone();
        self.scanned = Some(scanned);
        Ok(())
    }

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
        input.saved.node_set = None;
        input.saved.tag_ids = None;
        self.build.report.geometry_passes.add(&stored);
        self.build.report.phases.node_join_ms += phase.elapsed().as_millis();
        Ok(())
    }

    fn ways(&mut self, index: usize) -> Result<()> {
        let phase = Instant::now();
        let mut input = self.input.take().ok_or_else(|| missing("the scan"))?;
        let locations = input
            .locations
            .as_ref()
            .ok_or_else(|| missing("the node store"))?;
        input.kept_blobs.sort_unstable();
        input.kept_blobs.dedup();
        let mut input_members = HashSet::new();
        let mut part = Spool::create(&self.scratch, "geometry")?;
        let build = &mut self.build;
        let read = for_each_block(
            &self.options.inputs[index],
            &self.progress,
            BuildStage::AssembleWays,
            Some(&input.kept_blobs),
            |_, block| {
                let mut assembled = emit::assemble(&block, locations, &input.members);
                assembled.output.emitted.prepare()?;
                Ok(assembled)
            },
            |assembled| {
                let resolution = &mut build.report.geometry_resolution;
                resolution.resolved_node_refs += assembled.resolved;
                for (way_id, line, found) in assembled.members {
                    if input_members.insert(way_id) {
                        resolution.resolved_node_refs += found;
                        part.write(&GeometryItem::Member(way_id, line))?;
                    }
                }
                for (node, kind) in assembled.entrances {
                    part.write(&GeometryItem::Entrance(node, kind))?;
                }
                for way in assembled.output.boundary_ways {
                    part.write(&GeometryItem::Way(way))?;
                }
                build.absorb(assembled.output.emitted)
            },
        )?;
        self.build.report.geometry_passes.way_read_bytes += read;
        self.build.report.phases.feature_emission_ms += phase.elapsed().as_millis();
        drop(input);
        self.geometry_saved.push(part.finish()?);
        Ok(())
    }

    fn save_geometry(&mut self, part: &[GeometryItem]) -> Result<()> {
        let mut spool = Spool::create(&self.scratch, "geometry")?;
        for item in part {
            spool.write(item)?;
        }
        self.geometry_saved.push(spool.finish()?);
        Ok(())
    }

    fn boundaries(&mut self) -> Result<()> {
        let phase = Instant::now();
        self.progress.start(BuildStage::Boundaries, None)?;
        let Geometry {
            relations,
            boundary_feature_ids,
        } = std::mem::take(&mut self.geometry);
        drop(boundary_feature_ids);
        let batch_bytes = self
            .memory
            .limit()
            .saturating_sub(self.memory.used())
            .clamp(MIN_BOUNDARY_BATCH_BYTES, MAX_BOUNDARY_BATCH_BYTES);
        let mut batch_memory = Reservation::empty(&self.memory);
        if !batch_memory.try_grow(batch_bytes) {
            batch_memory.force_grow(MIN_BOUNDARY_BATCH_BYTES);
        }
        let batch_vertices = batch_memory.bytes() / BOUNDARY_BUILD_BYTES_PER_VERTEX;
        let mut builder = BoundaryBuilder::new(&self.scratch)?;
        let mut lines = IndexedSpool::<GeometryItem>::create(&self.scratch, "boundary-lines")?;
        struct BoundaryLine {
            way_id: i64,
            spool_index: u32,
            vertex_count: u32,
        }
        let mut line_at = Vec::<BoundaryLine>::new();
        let mut ways = Vec::new();
        let mut way_vertices = 0;
        let mut entrances = HashMap::new();
        for part in &self.geometry_saved {
            for item in part.reader()? {
                let (way_id, line) = match item? {
                    GeometryItem::Way(mut way) => {
                        let way_id = way.object_id;
                        let vertices = if boundary::may_be_area(&way) {
                            way_vertices += way.vertices.len();
                            let vertices = way.vertices.clone();
                            ways.push(way);
                            vertices
                        } else {
                            std::mem::take(&mut way.vertices)
                        };
                        if way_vertices >= batch_vertices {
                            self.progress.check()?;
                            builder.add_ways(&std::mem::take(&mut ways))?;
                            way_vertices = 0;
                        }
                        (way_id, Some(vertices))
                    }
                    GeometryItem::Member(way_id, line) => (way_id, line),
                    GeometryItem::Entrance(node, kind) => {
                        entrances.insert(node, kind);
                        continue;
                    }
                    GeometryItem::Relation(_) | GeometryItem::Feature(_) => continue,
                };
                let vertices = line.as_ref().map_or(0, Vec::len);
                line_at.push(BoundaryLine {
                    way_id,
                    spool_index: u32::try_from(line_at.len())
                        .map_err(|_| Error::Decode("more boundary lines than a u32 counts"))?,
                    vertex_count: u32::try_from(vertices).unwrap_or(u32::MAX),
                });
                lines.write(&GeometryItem::Member(way_id, line))?;
            }
        }
        builder.add_ways(&ways)?;
        drop(ways);
        let lines = lines.finish()?;
        line_at.sort_unstable_by_key(|line| line.way_id);
        let line_memory = Reservation::forced(
            &self.memory,
            line_at.capacity() * std::mem::size_of::<BoundaryLine>(),
        );
        let find = |way_id: i64| {
            line_at
                .binary_search_by_key(&way_id, |line| line.way_id)
                .ok()
                .map(|at| &line_at[at])
        };
        let member_line = |way_id: i64| -> Result<Option<Vec<Vertex>>> {
            let Some(line) = find(way_id) else {
                return Ok(None);
            };
            match lines.get(line.spool_index as usize)? {
                GeometryItem::Member(_, line) => Ok(line),
                _ => Err(Error::Decode("a boundary line file holds only lines")),
            }
        };
        let mut held = Reservation::empty(&self.memory);
        let mut start = 0;
        let mut vertices = 0;
        for (at, stub) in relations.iter().enumerate() {
            vertices += stub
                .members
                .iter()
                .filter_map(|member| find(member.way_id))
                .map(|line| line.vertex_count as usize)
                .sum::<usize>();
            if vertices >= batch_vertices || at + 1 == relations.len() {
                self.progress.check()?;
                builder.add_relations(&relations[start..=at], member_line)?;
                held.force_grow(builder.heap_bytes().saturating_sub(held.bytes()));
                start = at + 1;
                vertices = 0;
            }
        }
        self.progress.check()?;
        let mut places = Emitted::default();
        for candidate in boundary::place_relations(&relations, member_line, &entrances)? {
            poi::emit_poi(&candidate, &mut places);
        }
        self.build.absorb(places)?;
        drop((line_memory, line_at, lines, relations, batch_memory));
        self.build.report.geometry_passes.boundary_shape_vertices = builder.vertices();
        let mut boundaries = builder.finish()?;
        self.merge_place_points(&mut boundaries)?;
        let passes = &mut self.build.report.geometry_passes;
        passes.boundary_shape_bytes = boundaries.shapes().bytes();
        passes.boundary_heap_bytes = boundaries.heap_bytes() as u64;
        drop(held);
        self.boundary_memory = Some(Reservation::forced(&self.memory, boundaries.heap_bytes()));
        self.build
            .report
            .unmatched_admin_level_ids
            .extend_from_slice(boundaries.unmatched_admin_ids());
        for (origin, record) in boundaries.place_records(&mut self.build.report) {
            self.build.push(record, Some(origin))?;
        }
        self.boundaries_saved = Some(boundaries.save(&self.scratch)?);
        self.boundary_shapes = Some(Arc::clone(boundaries.shapes()));
        self.boundaries = Some(boundaries);
        self.geometry_saved.clear();
        self.progress.finish(BuildStage::Boundaries);
        self.build.report.phases.boundary_build_ms = phase.elapsed().as_millis();
        Ok(())
    }

    fn merge_place_points(&mut self, boundaries: &mut BoundarySet) -> Result<()> {
        let mut links = boundaries.place_links();
        if links.is_empty() {
            return Ok(());
        }
        let mut link_memory = Reservation::forced(&self.memory, links.heap_bytes());
        let Some(points) = self.build.context_records.take() else {
            return Ok(());
        };
        self.sorted
            .insert("place_points".to_string(), sorter_report(&points));
        let mut staged = Spool::<PendingRecord>::create(&self.scratch, "place-points")?;
        for (at, pending) in points.finish()?.enumerate() {
            let pending = pending?;
            if let Some((layer, point)) = place_point(&pending) {
                boundaries.match_place_point(&mut links, at as u64, layer, point);
                link_memory.force_grow(links.heap_bytes().saturating_sub(link_memory.bytes()));
            }
            staged.write(&pending)?;
        }
        boundaries.group_place_points(&mut links);
        let mut kept = context_sorter(&self.scratch, &self.memory, self.budget);
        let mut merged = 0;
        for (at, pending) in staged.into_reader()?.enumerate() {
            let pending = pending?;
            if place_point(&pending).is_some_and(|(_, point)| {
                boundaries.merge_place_point(&mut links, at as u64, point)
            }) {
                merged += 1;
            } else {
                kept.push(pending)?;
            }
        }
        boundaries.finish_place_merges(links);
        drop(link_memory);
        self.build.context_records = Some(kept);
        self.build.report.accepted.place_points_merged = merged;
        Ok(())
    }

    fn import_addresses(&mut self) -> Result<()> {
        let phase = Instant::now();
        let boundaries = self
            .boundaries
            .as_ref()
            .ok_or_else(|| missing("the boundaries"))?;
        let picked = match &self.options.addresses {
            Some(source) => source.pick(&boundaries.coverage())?,
            None => addresses::Picked::default(),
        };
        for why in &picked.left_out {
            self.options.observer.warned(why);
        }
        let files = picked.files;
        let import_bytes = files
            .iter()
            .map(addresses::AddressFile::bytes)
            .sum::<Result<u64>>()?;
        let extracts = self
            .options
            .inputs
            .iter()
            .map(|input| pbf::header_bbox(input))
            .collect::<Result<Option<Vec<_>>>>()?;
        let progress = &self.progress;
        progress.start(BuildStage::ImportAddresses, Some(import_bytes))?;
        let build = &mut self.build;
        let areas = files
            .iter()
            .filter_map(|file| file.area.as_deref())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|code| (code, boundaries.area(code)))
            .collect::<BTreeMap<_, _>>();
        for file in &files {
            let area = file
                .area
                .as_deref()
                .and_then(|code| areas.get(code)?.as_ref());
            let broken = match area {
                Some(area) => {
                    let points = addresses::first_points(file, addresses::SAMPLE_ROWS)?;
                    let outside = points.iter().filter(|point| !area(**point)).count();
                    outside * 2 > points.len()
                }
                None => false,
            };
            let mut dataset = None;
            let imported = addresses::read_file_prepared(
                file,
                |record| {
                    progress.check()?;
                    if broken {
                        return Ok(Err("source_outside_area"));
                    }
                    let Some(point) = point_lon_lat(&record.geometry) else {
                        return Ok(Err("outside_extract"));
                    };
                    let [lon, lat] = point;
                    let inside = extracts.as_ref().is_none_or(|boxes| {
                        boxes
                            .iter()
                            .any(|b| (b[0]..=b[2]).contains(&lon) && (b[1]..=b[3]).contains(&lat))
                    });
                    if !inside {
                        return Ok(Err("outside_extract"));
                    }
                    if area.as_ref().is_some_and(|area| !area(point)) {
                        return Ok(Err("outside_area"));
                    }
                    Ok(Ok(PendingAddress::new(&record, 0, 0)?))
                },
                |address| {
                    let id = *dataset.get_or_insert_with(|| build.datasets.id(&file.dataset));
                    build.push_address(address, id)?;
                    Ok(None)
                },
            )?;
            progress.advance(imported.bytes);
            progress
                .input(&file.path)
                .add(imported.bytes, std::time::Duration::ZERO);
            if !build.report.datasets.contains(&imported.dataset) {
                build.report.datasets.push(imported.dataset.clone());
            }
            build.report.imports.push(imported);
            let stamp = FileStamp::of(&file.path)?;
            if !self.fingerprint.imported.contains(&stamp) {
                self.fingerprint.imported.push(stamp);
            }
        }
        progress.finish(BuildStage::ImportAddresses);
        build.report.phases.address_import_ms = phase.elapsed().as_millis();
        self.imports = Some(files);
        Ok(())
    }

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
        let mut settled = HashSet::new();
        for (index, item) in context_records.finish()?.enumerate() {
            let item = item?;
            let record_id = index as u64;
            match &item.origin {
                Some(ContextOrigin::Boundary(boundary)) => {
                    boundary_ids[*boundary as usize] = record_id;
                    if boundaries.is_settled(*boundary as usize) {
                        settled.insert(record_id);
                    }
                }
                Some(ContextOrigin::DerivedCountry(code)) => {
                    country_ids.insert(code.clone(), record_id);
                }
                None => {}
            }
            if let Some(ContextOrigin::Boundary(_)) = item.origin
                && let Body::Record(record) = &item.body
                && let Record::Place(_, place) = record.as_ref()
                && street::is_settlement(&place.place_type)
            {
                settled.insert(record_id);
            }
            ordered.write(&item)?;
        }
        let open = self
            .build
            .town_votes
            .take()
            .ok_or_else(|| missing("the towns the addresses name"))?;
        let mut votes = std::mem::take(&mut self.build.town_votes_saved);
        if open.written() > 0 {
            votes.push(open.finish()?);
        }
        self.contexts = Some(Contexts::derive(
            boundaries,
            ordered.finish()?,
            boundary_ids,
            country_ids,
            settled,
            TownReach::Votes(votes),
        )?);
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

    fn import_overture(&mut self) -> Result<()> {
        let Some(release) = self.fingerprint.overture.as_deref() else {
            return Ok(());
        };
        let bounds = pbf::overture_bounds(&self.options.inputs)?;
        let verified = if let Some(stamp) = &self.fingerprint.overture_verified {
            let verified = overture::VerifiedPlaces::load(&stamp.path, release)?;
            if *stamp != crate::builder::resume::FileStamp::of(&stamp.path)? {
                return Err(Error::BadInput(
                    "the Overture verification file changed during the build".into(),
                ));
            }
            verified
        } else {
            overture::VerifiedPlaces::default()
        };
        let progress = &self.progress;
        let build = &mut self.build;
        let report = overture::import(
            release,
            &bounds,
            self.scratch.path(),
            &verified,
            || progress.check(),
            |place| build.push_place(place),
        )?;
        if report.unknown_licence > 0 {
            self.options.observer.warned(&format!(
                "Overture {release}: left out {} Places rows without approved source licences",
                report.unknown_licence
            ));
        }
        build.report.overture = Some(report);
        Ok(())
    }

    fn resolve_places(&mut self) -> Result<()> {
        let phase = Instant::now();
        let places = self
            .build
            .places
            .take()
            .ok_or_else(|| missing("the places"))?;
        self.sorted
            .insert("places".to_string(), sorter_report(&places));
        let records = open(&mut self.build.records, "records")?;
        let merged = resolve_places(places, records, &mut self.build.seq, &mut self.build.report)?;
        self.build.report.accepted.places_merged = merged;
        self.build.report.phases.street_join_ms += phase.elapsed().as_millis();
        Ok(())
    }

    fn imported(&self) -> bool {
        self.imports.as_ref().is_some_and(|files| !files.is_empty())
    }

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
        let lent = Book::create(&self.scratch, "street-lending")?;
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
                let slash_numbers = &mut self.build.report.slash_numbers;
                overlapped(
                    |emit| slash::read_slash_numbers(addresses.finish()?, slash_numbers, emit),
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
            lent: lent.finish()?,
            datasets: Datasets::from_names(self.build.datasets.names().to_vec()),
        });
        let report = &mut self.build.report;
        let mut points = Spool::create(&self.scratch, "postcode-points")?;
        for record in postcodes.into_records() {
            report.accept_postcode();
            let [lon, lat] = point_lon_lat(&record.geometry).ok_or_else(|| {
                Error::BadGeometry(format!("postcode {} has no point", record.postcode))
            })?;
            points.write(&NamedPoint {
                name: record.postcode.clone(),
                point: [lon, lat],
            })?;
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

    fn write_records(&mut self) -> Result<()> {
        let phase = Instant::now();
        let Contexts {
            ordered,
            index,
            area_names,
            area_tags,
            country_codes,
            subdivision_codes,
            plate_codes,
            default_languages,
            language_names,
            extents,
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
        let mut writer = Writer::create_in(
            generation,
            Arc::clone(&self.scratch),
            &self.writer_options(),
        )?;
        writer.set_area_tags(area_tags);
        writer.set_country_codes(country_codes);
        if self.options.tiger
            && subdivision_codes
                .values()
                .any(|code| code.starts_with("US-"))
        {
            let gazetteer = tiger::Gazetteer::load(&self.options.pack)?;
            let codes = index.census_codes(&subdivision_codes, &area_names, &gazetteer);
            if !codes.is_empty() {
                writer.set_census_codes(codes);
                self.build.report.datasets.push(tiger::DATASET.to_string());
            }
        }
        writer.set_context_names(area_names);
        writer.set_subdivision_codes(subdivision_codes);
        writer.set_plate_codes(plate_codes);
        writer.set_default_languages(default_languages);
        writer.set_language_names(language_names);
        writer.set_extents(extents);
        writer.set_region_groups(region_groups);
        let mut sample = Sampler::default();
        let context_count = ordered.len();
        let qrank = match &self.options.qrank {
            Some(path) => Some(QRank::load(path, &self.memory)?),
            None => None,
        };
        let zoning = match &self.options.timezones {
            Some(path) => {
                let zones = Timezones::load(path, &self.scratch)?;
                let (areas, area_count) = index.timezones(&zones);
                writer.set_timezones(zones.names().to_vec(), areas.clone());
                Some(Zoning {
                    zones,
                    areas: areas.into_iter().collect(),
                    area_count,
                    by_area: AtomicU64::new(0),
                    by_point: AtomicU64::new(0),
                    without: AtomicU64::new(0),
                })
            }
            None => None,
        };
        let ranked_contexts = write_ordered(
            &mut writer,
            &mut sample,
            ordered.into_reader(),
            context_count,
            &index,
            None,
            qrank.as_ref(),
            zoning.as_ref(),
            &self.progress,
            BuildStage::ContextRecords,
        )?;
        let postcodes = self
            .postcodes
            .take()
            .ok_or_else(|| missing("the postcode points"))?;
        let points = postcodes
            .into_reader()
            .map(|point| point.map(|NamedPoint { name, point }| (name, point)))
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
        let ranked = write_ordered(
            &mut writer,
            &mut sample,
            records.finish()?,
            record_stats.items,
            &index,
            Some(&books),
            qrank.as_ref(),
            zoning.as_ref(),
            &self.progress,
            BuildStage::Records,
        )?;
        if let (Some(path), Some(zoning)) = (&self.options.timezones, &zoning) {
            let metadata = fs::metadata(path)?;
            self.build.report.timezones = Some(TimezoneReport {
                path: path.display().to_string(),
                bytes: metadata.len(),
                modified_unix: metadata
                    .modified()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs()),
                zones: zoning.zones.names().len() as u64,
                areas: zoning.area_count,
                areas_in_one_zone: zoning.areas.len() as u64,
                records_zoned_by_area: zoning.by_area.load(Ordering::Relaxed),
                records_zoned_by_point: zoning.by_point.load(Ordering::Relaxed),
                records_without_zone: zoning.without.load(Ordering::Relaxed),
            });
        }
        drop(zoning);
        if let (Some(path), Some(qrank)) = (&self.options.qrank, &qrank) {
            let metadata = fs::metadata(path)?;
            self.build.report.qrank = Some(QRankReport {
                path: path.display().to_string(),
                bytes: metadata.len(),
                modified_unix: metadata
                    .modified()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs()),
                items: qrank.len() as u64,
                ranked_places: ranked_contexts + ranked,
            });
        }
        drop(qrank);
        drop((index, books));
        self.boundaries_saved = None;
        self.boundary_shapes = None;
        self.build.report.phases.record_write_ms = phase.elapsed().as_millis();
        self.build.report.scratch.peak_tracked_bytes = self.memory.peak() as u64;
        self.progress.start(BuildStage::Indexes, None)?;
        self.indexing = Some(Instant::now());
        self.sealing = Some(writer.into_sealing()?);
        self.sample = Some(sample);
        Ok(())
    }

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

    fn checkpoint(&mut self, step: Step) -> Result<()> {
        let started = Instant::now();
        let scratch = Arc::clone(&self.scratch);
        let mut files = Vec::new();
        let mut sorters = Vec::new();
        save_sorter(&mut self.build.records, &mut sorters)?;
        save_sorter(&mut self.build.addresses, &mut sorters)?;
        save_sorter(&mut self.build.streets, &mut sorters)?;
        save_sorter(&mut self.build.places, &mut sorters)?;
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
                books.lent.state(&scratch)?,
            ));
            files.extend(books.addresses.files());
            files.extend(books.lent.files());
        }
        let geometry = self
            .geometry_saved
            .iter()
            .map(SpoolFile::state)
            .collect::<Vec<_>>();
        if let Some(open) = &mut self.build.town_votes
            && open.written() > 0
        {
            let next = Spool::create(&scratch, TOWN_VOTES)?;
            let closed = std::mem::replace(open, next);
            self.build.town_votes_saved.push(closed.finish()?);
        }
        let town_votes = self
            .build
            .town_votes_saved
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
            town_reach: contexts.index.town_reach(),
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
        for state in geometry.iter().chain(&town_votes) {
            files.extend(state.files(&scratch));
        }
        let boundaries = self.boundaries_saved.as_ref().map(SpoolFile::state);
        let boundary_shapes = self.boundary_shapes.as_ref().map(|shapes| {
            files.push(shapes.path().to_path_buf());
            shapes.state()
        });
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
            boundary_shapes,
            imports: self.imports.clone(),
            contexts,
            postcodes,
            town_votes,
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

    fn elapsed(&self) -> Duration {
        self.elapsed_before + self.started.elapsed()
    }

    fn report(&self) -> BuilderReport {
        let mut report = self.build.report.clone();
        report.stages.clone_from(&self.stages_before);
        report.stages.extend(self.progress.stages());
        report
    }

    fn record_scratch(&mut self, scratch: &Scratch) {
        let report = &mut self.build.report;
        report.scratch.spilled_bytes = scratch.spilled_bytes();
        report.scratch.budget_bytes = scratch.budget();
        report.scratch.peak_live_bytes = scratch.peak_bytes();
        report.scratch.peak_owners = scratch.peak_owners();
        report.scratch.zstd_level = scratch.level();
        report.scratch.spills = scratch.traffic();
        self.checkpoints.peak_retained_bytes = scratch.peak_retained_bytes();
        self.checkpoints.voided = scratch.voided();
    }

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
        self.progress.start(BuildStage::Parser, None)?;
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
        self.sample_saved = None;
        scratch.complete();
        self.record_scratch(&scratch);
        self.progress.finish(BuildStage::Parser);
        let parser_training_ms = phase.elapsed().as_millis();
        self.progress.start(BuildStage::StructuredIndex, None)?;
        let structured = Instant::now();
        crate::structured::add_to(&mut sealed, &scratch, &self.memory, self.budget)?;
        crate::ids::add_to(&mut sealed, &scratch, &self.memory, self.budget)?;
        if self
            .build
            .report
            .overture
            .as_ref()
            .is_some_and(|report| report.kept > 0)
        {
            let notices = crate::licences::OVERTURE_NOTICES.as_bytes();
            let section = crate::pack::OVERTURE_NOTICES_SECTION;
            sealed.add_section(section, 1, notices)?;
            stats.sections.insert(section.into(), notices.len() as u64);
        }
        if self.options.tiger {
            let counties = tiger::counties_for_pack(&sealed.path(), &self.options.pack)?;
            let sections =
                tiger::add_ranges(&mut sealed, &self.options.pack, &counties, &self.progress)?;
            if !sections.is_empty() {
                self.build
                    .report
                    .datasets
                    .push(crate::tiger::DATASET.to_string());
                if sections
                    .iter()
                    .any(|(name, _)| *name == crate::tiger::SIDE_BLOCKS_SECTION)
                {
                    self.build.report.datasets.push("tiger:2026:faces".into());
                }
                stats.sections.extend(
                    sections
                        .into_iter()
                        .map(|(name, len)| (name.to_string(), len)),
                );
            }
            for (historical, section, dataset) in [
                (
                    false,
                    crate::tiger::BLOCKS_SECTION,
                    crate::tiger::BLOCKS_DATASET,
                ),
                (
                    true,
                    crate::tiger::HISTORICAL_SECTION,
                    "tiger:2010:tabblock10",
                ),
            ] {
                let bytes = tiger::add_blocks(
                    &mut sealed,
                    &self.options.pack,
                    &counties,
                    &self.progress,
                    historical,
                )?;
                if bytes > 0 {
                    self.build.report.datasets.push(dataset.to_string());
                    stats.sections.insert(section.to_string(), bytes);
                }
            }
            let bytes =
                tiger::add_zcta(&mut sealed, &self.options.pack, &counties, &self.progress)?;
            if bytes > 0 {
                stats
                    .sections
                    .insert(crate::tiger::ZCTA_SECTION.into(), bytes);
            }
        }
        stats.bytes = sealed.bytes();
        self.progress.finish(BuildStage::StructuredIndex);
        self.record_scratch(&scratch);
        let mut report = self.report();
        report.checkpoints = self.checkpoints.clone();
        self.progress.check()?;
        report.parser_training = training;
        report.phases.parser_training_ms = parser_training_ms;
        report.phases.structured_index_ms = structured.elapsed().as_millis();
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

fn save_sorter<T: Spill + Ord + Send + Sync + 'static>(
    sorter: &mut Option<ExternalSorter<T>>,
    states: &mut Vec<SorterState>,
) -> Result<()> {
    if let Some(sorter) = sorter {
        states.push(sorter.checkpoint()?);
    }
    Ok(())
}

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

fn budget_of(plan: &BuildPlan) -> Arc<Budget> {
    let memory = Budget::new(usize::try_from(plan.memory_budget_bytes).unwrap_or(usize::MAX));
    memory.limit_heap(usize::try_from(plan.heap_limit_bytes()).unwrap_or(usize::MAX));
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

fn open<'s, T>(
    sorter: &'s mut Option<ExternalSorter<T>>,
    label: &str,
) -> Result<&'s mut ExternalSorter<T>> {
    sorter
        .as_mut()
        .ok_or_else(|| Error::Inconsistent(format!("the {label} sorter was read already")))
}

const CONTEXT_RECORDS: &str = "context-records";

fn context_share(budget: usize) -> usize {
    (budget / 16).max(1)
}

fn place_point(pending: &PendingRecord) -> Option<(PlaceLayer, &PlaceRecord)> {
    match &pending.body {
        Body::Record(record) => match record.as_ref() {
            Record::Place(layer, place) => Some((*layer, place)),
            _ => None,
        },
        _ => None,
    }
}

fn context_sorter(
    scratch: &Arc<Scratch>,
    memory: &Arc<Budget>,
    budget: usize,
) -> ExternalSorter<PendingRecord> {
    ExternalSorter::new(scratch, CONTEXT_RECORDS, memory, context_share(budget))
}

const TOWN_VOTES: &str = "town-votes";

fn missing(what: &str) -> Error {
    Error::Inconsistent(format!("{what} is missing: a build step ran out of order"))
}

struct Build {
    report: BuilderReport,
    audit: Audit,
    records: Option<ExternalSorter<PendingRecord>>,
    addresses: Option<ExternalSorter<PendingAddress>>,
    streets: Option<ExternalSorter<PendingNamed>>,
    places: Option<ExternalSorter<PendingNamed>>,
    context_records: Option<ExternalSorter<PendingRecord>>,
    town_votes: Option<Spool<NamedPoint>>,
    town_votes_saved: Vec<SpoolFile<NamedPoint>>,
    datasets: Datasets,
    seq: u64,
    memory: Arc<Budget>,
}

impl Build {
    fn memory(&self) -> &Arc<Budget> {
        &self.memory
    }

    fn absorb(&mut self, emitted: Emitted) -> Result<()> {
        let Emitted {
            report,
            records,
            ready,
            places,
            rejections,
        } = emitted;
        self.report.merge(report);
        for rejection in &rejections {
            self.audit.write(rejection)?;
        }
        for item in ready {
            match item {
                Ready::Address(address) => self.push_address(address, 0)?,
                Ready::Record(record) => self.push(*record, None)?,
            }
        }
        for record in records {
            self.push(record, None)?;
        }
        for place in places {
            self.push_place(place)?;
        }
        Ok(())
    }

    fn push_place(&mut self, place: Place) -> Result<()> {
        self.seq += 1;
        open(&mut self.places, "places")?.push(PendingNamed::place(place, self.seq - 1)?)
    }

    fn push_address(&mut self, address: PendingAddress, dataset: u32) -> Result<()> {
        let fields = address.fields();
        for name in [fields.locality, fields.place].into_iter().flatten() {
            self.town_votes
                .as_mut()
                .ok_or_else(|| missing("the towns the addresses name"))?
                .write(&NamedPoint {
                    name: name.to_string(),
                    point: address.point,
                })?;
        }
        self.seq += 1;
        open(&mut self.addresses, "addresses")?.push(address.numbered(self.seq - 1, dataset))
    }

    fn push(&mut self, record: Record, origin: Option<ContextOrigin>) -> Result<()> {
        if let Record::Address(address) = record {
            let dataset = match address.source.object_type {
                ObjectType::Row => self.datasets.id(&address.source.dataset),
                _ => 0,
            };
            return self.push_address(PendingAddress::new(&address, 0, 0)?, dataset);
        }
        if let Record::Street(street) = record {
            self.seq += 1;
            return open(&mut self.streets, "streets")?
                .push(PendingNamed::new(street, self.seq - 1)?);
        }
        if let Record::Poi(record) = record {
            return self.push_place(Place {
                record,
                identity: Identity::default(),
            });
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

struct AddressRuns<'a> {
    records: &'a mut ExternalSorter<PendingRecord>,
    seq: &'a mut u64,
    index: &'a BoundaryIndex,
    country_codes: &'a HashMap<RecordId, CountryCode>,
    postcodes: postcode::Accumulator,
    lending: StreetLending,
    book: Option<Book<PendingAddress>>,
    batch: Vec<(PendingAddress, Option<(u64, bool)>)>,
    group: Option<(Vec<u8>, u64, Option<CountryCode>)>,
}

impl<'a> AddressRuns<'a> {
    fn new(
        records: &'a mut ExternalSorter<PendingRecord>,
        seq: &'a mut u64,
        index: &'a BoundaryIndex,
        country_codes: &'a HashMap<RecordId, CountryCode>,
        lent: Book<Lent>,
        book: Option<Book<PendingAddress>>,
    ) -> Self {
        Self {
            records,
            seq,
            index,
            country_codes,
            postcodes: postcode::Accumulator::default(),
            lending: StreetLending::new(lent),
            book,
            batch: Vec::new(),
            group: None,
        }
    }

    fn push(&mut self, address: PendingAddress, shelved: Option<(u64, bool)>) -> Result<()> {
        self.batch.push((address, shelved));
        if self.batch.len() == WRITE_BATCH_RECORDS {
            self.place_batch()?;
        }
        Ok(())
    }

    fn finish(
        mut self,
    ) -> Result<(
        postcode::Accumulator,
        Book<Lent>,
        Option<Book<PendingAddress>>,
    )> {
        self.place_batch()?;
        self.lending.close_street()?;
        Ok((self.postcodes, self.lending.book, self.book))
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
                let context = self.index.context_for_point(lon, lat, &source);
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
            let lent = self.lending.offer(address.street_key(), &fields)?;
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
                body: Body::Address(PlacedAddress { at, lent, disputed }),
            })?;
            *self.seq += 1;
        }
        Ok(())
    }
}

struct StreetLending {
    book: Book<Lent>,
    key: Vec<u8>,
    postcodes: postcode::Street,
    spellings: BTreeSet<String>,
    lend_postcodes: bool,
    lend_spellings: bool,
}

impl StreetLending {
    fn new(book: Book<Lent>) -> Self {
        Self {
            book,
            key: Vec::new(),
            postcodes: postcode::Street::default(),
            spellings: BTreeSet::new(),
            lend_postcodes: false,
            lend_spellings: false,
        }
    }

    fn offer(&mut self, key: &[u8], fields: &spill::Fields<'_>) -> Result<Option<u64>> {
        if self.key != key {
            self.close_street()?;
            self.key.clear();
            self.key.extend_from_slice(key);
        }
        let postcode = self.postcodes.lends_to(fields);
        let spelling = fields.street.is_some_and(|name| {
            if spill::in_capitals(name) {
                return true;
            }
            if !self.spellings.contains(name) {
                self.spellings.insert(name.to_owned());
            }
            false
        });
        self.lend_postcodes |= postcode;
        self.lend_spellings |= spelling;
        Ok((postcode || spelling).then(|| self.book.next_position()))
    }

    fn close_street(&mut self) -> Result<()> {
        let postcodes = std::mem::take(&mut self.postcodes).postcodes;
        let spellings = std::mem::take(&mut self.spellings);
        let lend_postcodes = std::mem::take(&mut self.lend_postcodes);
        let lend_spellings = std::mem::take(&mut self.lend_spellings);
        if lend_postcodes || lend_spellings {
            self.book.push(Lent {
                postcodes: if lend_postcodes {
                    postcodes
                } else {
                    Vec::new()
                },
                spellings: if lend_spellings {
                    spellings.into_iter().collect()
                } else {
                    Vec::new()
                },
            })?;
        }
        Ok(())
    }
}

struct AddressBook {
    addresses: BookFile<PendingAddress>,
    lent: BookFile<Lent>,
    datasets: Datasets,
}

impl AddressBook {
    fn records(&self, batch: Vec<PendingRecord>) -> Result<Vec<(Record, Option<ContextOrigin>)>> {
        let mut address_chunks = Vec::new();
        let mut lending_chunks = Vec::new();
        for pending in &batch {
            if let Body::Address(placed) = &pending.body {
                address_chunks.push(BookFile::<PendingAddress>::chunk_of(placed.at));
                lending_chunks.extend(placed.lent.map(BookFile::<Lent>::chunk_of));
            }
        }
        let addresses = read_chunks(&self.addresses, address_chunks)?;
        let lending = read_chunks(&self.lent, lending_chunks)?;
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
                        if let Some(at) = placed.lent {
                            let chunk = &lending[&BookFile::<Lent>::chunk_of(at)];
                            let lent = BookFile::pick(chunk, at)?;
                            if record.address.postcode.is_none() {
                                record.street_postcodes.clone_from(&lent.postcodes);
                            }
                            if let Some(street) = record.address.street.as_mut()
                                && !lent.spellings.is_empty()
                                && spill::in_capitals(street)
                            {
                                let lowercase = street.to_lowercase();
                                if let Some(spelling) = lent
                                    .spellings
                                    .iter()
                                    .find(|spelling| spelling.to_lowercase() == lowercase)
                                {
                                    street.clone_from(spelling);
                                }
                            }
                        }
                        Record::Address(record)
                    }
                };
                Ok((record, pending.origin))
            })
            .collect()
    }
}

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

fn resolve_places(
    places: ExternalSorter<PendingNamed>,
    records: &mut ExternalSorter<PendingRecord>,
    seq: &mut u64,
    report: &mut BuilderReport,
) -> Result<u64> {
    let mut merged = 0;
    let mut retained = 0;
    let mut imported = 0;
    let mut flush = |group: &mut Vec<Place>| -> Result<()> {
        let count = group.len();
        if report.overture.is_some() {
            imported += group
                .iter()
                .filter(|place| place.record.source.gers().is_some())
                .count() as u64;
        }
        let resolved = poi::resolve(std::mem::take(group));
        merged += (count - resolved.len()) as u64;
        for place in resolved {
            if place.source.gers().is_some() {
                retained += 1;
                if !report.datasets.contains(&place.source.dataset) {
                    report.datasets.push(place.source.dataset.clone());
                }
                report.accept_poi(&place);
            }
            let [lon, lat] = Record::Poi(place.clone())
                .display_point()
                .ok_or_else(|| Error::BadGeometry(format!("place {} has no point", place.id())))?;
            records.push(PendingRecord {
                key: hilbert_key(quantize(lon)?, quantize(lat)?),
                seq: *seq,
                origin: None,
                body: Record::Poi(place).into(),
            })?;
            *seq += 1;
        }
        Ok(())
    };
    let mut group = Vec::new();
    let mut name = None::<Vec<u8>>;
    for pending in places.finish()? {
        let pending = pending?;
        if name.as_deref() != Some(pending.name_key()) {
            flush(&mut group)?;
            name = Some(pending.name_key().to_vec());
        }
        group.push(pending.into_place());
    }
    flush(&mut group)?;
    if let Some(overture) = report.overture.as_mut() {
        overture.kept = retained;
        overture.duplicates = imported - retained;
    }
    Ok(merged)
}

fn join_streets(
    streets: ExternalSorter<PendingNamed>,
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
                    let context = index.context_for_point(lon, lat, &Default::default());
                    let tuple = &context.admin_context;
                    let locality = tuple.locality_record_id();
                    let area = locality
                        .filter(|locality| settled.contains(locality))
                        .or_else(|| tuple.district_record_id())
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
    let mut runs = Vec::<Vec<StreetRecord>>::new();
    let mut ways = 0;
    let mut name = None::<Vec<u8>>;
    overlapped(
        |emit| streets.finish()?.try_for_each(|pending| emit(pending?)),
        |pending: PendingNamed| {
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
            Ok(())
        },
    )?;
    flush(&mut runs)?;
    Ok(count)
}

const REPEATED_HOUSE_RADIUS_M: f64 = 100.0;

pub(crate) const DISPUTED_HOUSE_RADIUS_M: f64 = 1_000.0;

const DROPPED: u64 = 0;
const DISPUTED: u64 = 1;

type Verdicts = Sorted<(u64, u64)>;

fn write_address_book(
    addresses: ExternalSorter<PendingAddress>,
    scratch: &Arc<Scratch>,
    slash_numbers: &mut SlashNumberCounts,
) -> Result<BookFile<PendingAddress>> {
    let mut book = Book::create(scratch, "address-book")?;
    overlapped(
        |emit| slash::read_slash_numbers(addresses.finish()?, slash_numbers, emit),
        |address| book.push(address).map(drop),
    )?;
    book.finish()
}

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

fn find_repeated_houses(
    book: &BookFile<PendingAddress>,
    scratch: &Arc<Scratch>,
    memory: &Arc<Budget>,
    budget: usize,
    datasets: &Datasets,
    report: &mut BuilderReport,
) -> Result<ExternalSorter<(u64, u64)>> {
    let mut houses = ExternalSorter::<House>::new(scratch, "houses", memory, budget);
    for (at, address) in book.items().enumerate() {
        houses.push(address?.house(at as u64))?;
    }
    let mut verdicts = ExternalSorter::new(scratch, "house-verdicts", memory, budget);
    let import_of = datasets
        .names()
        .iter()
        .map(|name| {
            report
                .imports
                .iter()
                .position(|import| import.dataset == *name)
        })
        .collect::<Vec<_>>();
    let mut kept: Vec<House> = Vec::new();
    for house in houses.finish()? {
        let house = house?;
        if kept.first().is_some_and(|first| first.key != house.key) {
            kept.clear();
        }
        let distance = |kept: &House| {
            crate::util::geo::haversine_m(
                kept.point[0],
                kept.point[1],
                house.point[0],
                house.point[1],
            )
        };
        let repeated = house.imported
            && kept.iter().any(|kept| {
                distance(kept) <= REPEATED_HOUSE_RADIUS_M
                    || (kept.same_source(&house)
                        && distance(kept) <= DISPUTED_HOUSE_RADIUS_M
                        && !kept.other_house(&house))
            });
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
        let import = *import_of
            .get(house.dataset as usize)
            .ok_or(Error::Decode("unknown address dataset"))?;
        if let Some(import) = import.and_then(|at| report.imports.get_mut(at)) {
            if repeated {
                import.duplicates += 1;
            } else {
                import.disputes += 1;
            }
        }
    }
    Ok(verdicts)
}

#[derive(Debug, Default)]
struct Geometry {
    relations: Vec<BoundaryRelationStub>,
    boundary_feature_ids: HashSet<i64>,
}

impl Geometry {
    fn add(&mut self, part: impl Iterator<Item = Result<GeometryItem>>) -> Result<()> {
        for item in part {
            match item? {
                GeometryItem::Relation(relation) => self.relations.push(relation),
                GeometryItem::Feature(way_id) => {
                    self.boundary_feature_ids.insert(way_id);
                }
                GeometryItem::Way(_) | GeometryItem::Member(..) | GeometryItem::Entrance(..) => {}
            }
        }
        Ok(())
    }
}

struct InputPasses {
    index: usize,
    nodes: Option<NodeSet>,
    tag_ids: Vec<i64>,
    node_blobs: Vec<u64>,
    way_blobs: Vec<u64>,
    kept_blobs: Vec<u64>,
    kept: KeptWays,
    members: HashSet<i64>,
    locations: Option<Locations>,
    saved: InputSaved,
    changed: bool,
}

#[derive(Default)]
struct InputSaved {
    node_blobs: Option<SpoolFile<Id>>,
    way_blobs: Option<SpoolFile<Id>>,
    kept_blobs: Option<SpoolFile<Id>>,
    tag_ids: Option<SpoolFile<Id>>,
    node_set: Option<SpoolFile<ChunkBits>>,
    members: Option<SpoolFile<Id>>,
}

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

fn load_ids(saved: &SpoolFile<Id>) -> Result<Vec<i64>> {
    saved.reader()?.map(|id| id.map(|Id(id)| id)).collect()
}

impl InputPasses {
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

fn overlapped<T: Send>(
    read: impl FnOnce(&mut dyn FnMut(T) -> Result<()>) -> Result<()> + Send,
    mut take: impl FnMut(T) -> Result<()>,
) -> Result<()> {
    let stopped = || Error::Inconsistent("a merge reader lost the thread taking its items".into());
    std::thread::scope(|scope| {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<T>>(1);
        let reader = scope.spawn(move || -> Result<()> {
            let mut batch = Vec::with_capacity(WRITE_BATCH_RECORDS);
            read(&mut |item| {
                batch.push(item);
                if batch.len() == WRITE_BATCH_RECORDS {
                    sender
                        .send(std::mem::take(&mut batch))
                        .map_err(|_| stopped())?;
                }
                Ok(())
            })?;
            if !batch.is_empty() {
                sender.send(batch).map_err(|_| stopped())?;
            }
            Ok(())
        });
        for batch in receiver {
            batch.into_iter().try_for_each(&mut take)?;
        }
        reader
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

struct Zoning {
    zones: Timezones,
    areas: HashMap<RecordId, u16>,
    area_count: u64,
    by_area: AtomicU64,
    by_point: AtomicU64,
    without: AtomicU64,
}

impl Zoning {
    fn stamp(&self, record: &Record, context: &mut RecordContext) {
        let chain = context.admin_context.entries();
        let count = if chain
            .iter()
            .any(|entry| self.areas.contains_key(&entry.record_id))
        {
            &self.by_area
        } else {
            context.zone = record
                .display_point()
                .and_then(|point| self.zones.at(point));
            if context.zone.is_some() {
                &self.by_point
            } else {
                &self.without
            }
        };
        count.fetch_add(1, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
fn write_ordered(
    writer: &mut Writer,
    sample: &mut Sampler,
    records: impl Iterator<Item = Result<PendingRecord>> + Send,
    total: u64,
    index: &BoundaryIndex,
    book: Option<&AddressBook>,
    qrank: Option<&QRank>,
    zoning: Option<&Zoning>,
    progress: &Progress,
    stage: BuildStage,
) -> Result<u64> {
    let ranked = AtomicU64::new(0);
    let ranked_ref = &ranked;
    progress.start(stage, Some(total))?;
    let mut preparer = writer.preparer();
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
                            .map(|(mut record, origin)| {
                                let known = match &mut record {
                                    Record::Poi(poi) => Some((&mut poi.prominence, poi.wikidata)),
                                    Record::Place(_, place) => {
                                        Some((&mut place.prominence, place.wikidata))
                                    }
                                    _ => None,
                                };
                                if let (Some(qrank), Some((prominence, Some(item)))) =
                                    (qrank, known)
                                {
                                    prominence.importance = qrank.importance(item);
                                    if prominence.importance > 0 {
                                        ranked_ref.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                                let own = match &origin {
                                    Some(ContextOrigin::Boundary(boundary)) => {
                                        index.own_area(*boundary)
                                    }
                                    _ => None,
                                };
                                let located = record_context(index, &record, own);
                                let country = located
                                    .as_ref()
                                    .and_then(|context| context.admin_context.country_record_id());
                                let mut context = match origin {
                                    Some(ContextOrigin::DerivedCountry(_)) => None,
                                    Some(ContextOrigin::Boundary(_)) => {
                                        located.and_then(|located| {
                                            let Record::Place(tier, place) = &record else {
                                                return Some(located);
                                            };
                                            index.area_context(
                                                *tier,
                                                &record,
                                                &place.source,
                                                own,
                                                located,
                                            )
                                        })
                                    }
                                    None => located,
                                };
                                if let (Some(zoning), Some(context)) = (zoning, &mut context)
                                    && !matches!(record, Record::Place(..))
                                {
                                    zoning.stamp(&record, context);
                                }
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
    Ok(ranked.into_inner())
}

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
        parse::lattice::dictionary::Dictionaries,
        record::{PlaceLayer, Record},
        reverse::{GeocodeOptions, MatchKind, PackReverseGeocoder},
        search::{TextAutocompleteOptions, TextSearchHit, TextSearchOptions, TextSearcher},
        test_support::TempDir,
    };

    use std::sync::LazyLock;

    use super::{
        test_pbf::{TestElement, moved, node, relation, renumbered, way, write_pbf},
        *,
    };

    fn temp_dir(name: &str) -> TempDir {
        TempDir::new(&format!("build-{name}"))
    }

    struct Built {
        dir: TempDir,
        inputs: Vec<PathBuf>,
        report: BuilderReport,
        events: Vec<(BuildStage, &'static str, Option<u64>)>,
    }

    impl Built {
        fn new(name: &str, extracts: Vec<(&str, Vec<Vec<TestElement>>)>) -> Self {
            Self::with(name, extracts, |_| BuildOsmOptions::default())
        }

        fn with(
            name: &str,
            extracts: Vec<(&str, Vec<Vec<TestElement>>)>,
            options: impl FnOnce(&Path) -> BuildOsmOptions,
        ) -> Self {
            let dir = temp_dir(name);
            let options = options(&dir);
            let mut inputs = Vec::new();
            let mut sources = Vec::new();
            for (source, blocks) in extracts {
                let input = dir.join(format!("{source}.osm.pbf"));
                write_pbf(&input, &blocks);
                inputs.push(input);
                sources.push(source.to_string());
            }
            let recorder = Arc::new(Recorder::default());
            let report = build_osm_pack(&BuildOsmOptions {
                inputs: inputs.clone(),
                pack: dir.join("pack"),
                input_sources: sources,
                observer: Arc::clone(&recorder) as Arc<dyn BuildObserver>,
                ..options
            })
            .expect("build");
            let events = recorder.0.lock().expect("events").clone();
            Self {
                dir,
                inputs,
                report,
                events,
            }
        }

        fn pack(&self) -> PathBuf {
            self.dir.join("pack")
        }
    }

    fn one_extract(
        cells: Vec<(&'static str, Vec<Vec<TestElement>>)>,
    ) -> Vec<(&'static str, Vec<Vec<TestElement>>)> {
        let mut blocks = [Vec::new(), Vec::new(), Vec::new()];
        for element in cells.into_iter().flat_map(|(_, cell)| cell).flatten() {
            let kind = match element {
                TestElement::Node { .. } => 0,
                TestElement::Way { .. } => 1,
                TestElement::Relation { .. } => 2,
            };
            blocks[kind].push(element);
        }
        vec![("cells", blocks.into())]
    }

    static TOWN: LazyLock<Built> = LazyLock::new(|| Built::new("town", vec![("town", town())]));

    static TWO_COUNTRIES: LazyLock<Built> = LazyLock::new(|| {
        Built::new(
            "two-countries",
            vec![("town", town_with_pois()), ("other", other_country())],
        )
    });

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

    #[test]
    fn builds_one_pack_covering_two_extracts() {
        let both = &TWO_COUNTRIES.report;
        assert_eq!(both.inputs.len(), 2, "the report names every input");
        assert_eq!(
            both.input_bytes,
            both.inputs.iter().map(|i| i.bytes).sum::<u64>()
        );

        let reader = Arc::new(Reader::open(TWO_COUNTRIES.pack()).expect("reader"));
        let searcher = TextSearcher::from_pack(Arc::clone(&reader)).expect("searcher");
        let search = |countries: Vec<String>| {
            searcher
                .search(&TextSearchOptions {
                    query: "King Street".to_string(),
                    limit: 20,
                    layer: None,
                    countries,
                    focus: None,
                    bbox: None,
                })
                .expect("search")
        };

        let any = search(Vec::new());
        assert!(any.len() >= 2, "both King Streets are indexed: {any:?}");

        let ca = search(vec!["CA".to_string()]);
        let nz = search(vec!["NZ".to_string()]);
        assert!(!ca.is_empty(), "Canada has a King Street");
        assert!(!nz.is_empty(), "New Zealand has one too");
        assert_eq!(
            ca.len() + nz.len(),
            any.len(),
            "every hit lies in exactly one of the two countries"
        );

        assert_eq!(
            search(vec!["CA".to_string(), "NZ".to_string()]).len(),
            any.len()
        );
        assert!(search(vec!["FR".to_string()]).is_empty());

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

    fn build_all(
        root: &Path,
        inputs: &[&Path],
        memory_budget_bytes: Option<usize>,
    ) -> BuilderReport {
        build_osm_pack(&BuildOsmOptions {
            inputs: inputs.iter().map(|input| input.to_path_buf()).collect(),
            pack: root.to_path_buf(),
            memory_budget_bytes,
            force: memory_budget_bytes.is_some(),
            ..BuildOsmOptions::default()
        })
        .expect("build")
    }

    #[derive(Debug, Default)]
    struct RecordingSource {
        files: Vec<addresses::AddressFile>,
        asked: std::sync::Mutex<Vec<addresses::Coverage>>,
    }

    impl AddressSource for RecordingSource {
        fn pick(&self, coverage: &addresses::Coverage) -> Result<addresses::Picked> {
            self.asked.lock().expect("lock").push(coverage.clone());
            Ok(addresses::Picked {
                files: self.files.clone(),
                left_out: Vec::new(),
            })
        }
    }

    struct Imported {
        built: Built,
        source: Arc<RecordingSource>,
    }

    static TOWN_IMPORTS: LazyLock<Imported> = LazyLock::new(|| {
        let mut source = None;
        let built = Built::with(
            "town-imports",
            vec![("town", town()), ("streets", imported_streets())],
            |dir| {
                let file = |dataset: &str, area: Option<&str>, text: String| {
                    let path = dir.join(format!("{}.csv", dataset.replace('/', "-")));
                    fs::write(&path, text).expect("csv");
                    addresses::AddressFile {
                        dataset: format!("openaddresses:{dataset}"),
                        path,
                        entry: None,
                        area: area.map(str::to_string),
                    }
                };
                let mut files = vec![file(
                    "test",
                    None,
                    "LON,LAT,NUMBER,STREET,CITY,POSTCODE\n\
                     -79.40005,43.60003,10,KING ST,Toronto,M5V 1A1\n\
                     -79.4003,43.6003,16,King Street,Toronto,M5V 1A1\n\
                     -79.40031,43.60031,16,King Street,,\n\
                     -79.45,43.65,3,Queen Street,Toronto,\n\
                     -79.45,43.65,,Queen Street,Toronto,\n"
                        .to_string(),
                )];
                let outside_area = [
                    (
                        "sound",
                        "-79.47,43.64,3,Bloor Street\n-78.50,43.65,7,Lake Road\n",
                    ),
                    (
                        "swapped",
                        "43.62,-79.41,8,Bloor Street\n43.66,-79.46,9,Bloor Street\n\
                         -79.46,43.66,11,Bloor Street\n",
                    ),
                ];
                for (name, rows) in outside_area {
                    files.push(file(
                        &format!("ca/on/{name}"),
                        Some("CA-ON"),
                        format!("LON,LAT,NUMBER,STREET\n{rows}"),
                    ));
                }
                files.push(file(
                    "disputed",
                    None,
                    "LON,LAT,NUMBER,STREET,CITY,POSTCODE\n\
                     -79.35,43.604,10,Front Street,Toronto,M5J 1A1\n\
                     -79.35,43.70,10,Front Street,Toronto,\n\
                     -79.35,43.596,10,Front Street,Toronto,K7L 1A1\n"
                        .to_string(),
                ));
                files.push(file(
                    "capitals",
                    None,
                    "LON,LAT,NUMBER,STREET,CITY,POSTCODE\n\
                     -79.33,43.600,40,WELLINGTON STREET,Toronto,\n\
                     -79.33,43.601,42,RICHMOND STREET,Toronto,\n\
                     -79.33,43.601,50,ADELAIDE STREET,Toronto,M5G 1H2\n\
                     -79.33,43.601,52,Adelaide Street,Toronto,\n\
                     -79.33,43.601,54,ADELAIDE ST,Toronto,\n"
                        .to_string(),
                ));
                let rows = |rows: &str| format!("LON,LAT,NUMBER,STREET,CITY\n{rows}");
                files.extend([
                    file(
                        "apart-lots",
                        None,
                        rows(
                            "-79.38,43.600,20,Dundas Street,Toronto\n\
                              -79.38,43.604,20,Dundas Street,Toronto\n\
                              -79.38,43.600,30,Dundas Street,Toronto\n",
                        ),
                    ),
                    file(
                        "apart-register",
                        None,
                        rows("-79.38,43.604,30,Dundas Street,Toronto\n"),
                    ),
                    file(
                        "dropped-empty",
                        None,
                        rows("-79.36,43.600,,College Street,Toronto\n"),
                    ),
                    file(
                        "dropped-lots",
                        None,
                        rows(
                            "-79.36,43.600,20,College Street,Toronto\n\
                              -79.36,43.604,20,College Street,Toronto\n\
                              -79.36,43.600,30,College Street,Toronto\n",
                        ),
                    ),
                    file(
                        "dropped-register",
                        None,
                        rows("-79.36,43.604,30,College Street,Toronto\n"),
                    ),
                ]);
                let mut again = file(
                    "dropped-lots-again",
                    None,
                    rows("-79.36,43.600,20,College Street,Toronto\n"),
                );
                again.dataset = "openaddresses:dropped-lots".to_string();
                files.push(again);
                let (inside, outside, swapped) = ("-79.43,43.66", "-78.50,43.66", "43.66,-79.43");
                let sample = addresses::SAMPLE_ROWS;
                for (name, runs) in [
                    ("cutoff", vec![(sample, inside), (1, outside)]),
                    ("late", vec![(sample, swapped), (1, inside)]),
                    ("half", vec![(sample / 2, outside), (sample / 2, inside)]),
                    (
                        "most",
                        vec![(sample / 2 + 1, outside), (sample / 2 - 1, inside)],
                    ),
                    ("unplaced", vec![(sample, "0,0"), (1, inside)]),
                    ("empty", vec![]),
                ] {
                    let mut text = String::from("LON,LAT,NUMBER,STREET\n");
                    let mut number = 0;
                    for (rows, point) in runs {
                        for _ in 0..rows {
                            number += 1;
                            text.push_str(&format!("{point},{number},{name} Street\n"));
                        }
                    }
                    files.push(file(&format!("ca/on/{name}"), Some("CA-ON"), text));
                }
                let recording = Arc::new(RecordingSource {
                    files,
                    ..RecordingSource::default()
                });
                source = Some(Arc::clone(&recording));
                BuildOsmOptions {
                    addresses: Some(recording as Arc<dyn AddressSource>),
                    ..BuildOsmOptions::default()
                }
            },
        );
        Imported {
            built,
            source: source.expect("a source"),
        }
    });

    fn imported_streets() -> Vec<Vec<TestElement>> {
        let house = |id, lat, lon, tags: &[(&'static str, &'static str)]| {
            node(id, lat, lon, &[&[("addr:city", "Toronto")], tags].concat())
        };
        vec![
            vec![
                house(
                    20001,
                    43.60,
                    -79.35,
                    &[
                        ("addr:housenumber", "10"),
                        ("addr:street", "Front Street"),
                        ("addr:postcode", "M5J 1A1"),
                    ],
                ),
                house(
                    20011,
                    43.60,
                    -79.33,
                    &[
                        ("addr:housenumber", "10"),
                        ("addr:street", "Wellington Street"),
                        ("addr:postcode", "M5H 1A1"),
                    ],
                ),
                house(
                    20012,
                    43.630,
                    -79.37,
                    &[
                        ("addr:housenumber", "2"),
                        ("addr:street", "Adelaide Street"),
                    ],
                ),
                house(
                    20013,
                    43.631,
                    -79.37,
                    &[
                        ("addr:housenumber", "10"),
                        ("addr:street", "Adelaide Street"),
                    ],
                ),
                node(20020, 43.61, -79.35, &[]),
                node(20021, 43.61, -79.34, &[]),
            ],
            vec![way(
                20600,
                &[20020, 20021],
                &[("highway", "residential"), ("name", "Richmond Street")],
            )],
        ]
    }

    fn imports_of(dataset: &str) -> Vec<&'static addresses::ImportReport> {
        TOWN_IMPORTS
            .built
            .report
            .imports
            .iter()
            .filter(|import| import.dataset == format!("openaddresses:{dataset}"))
            .collect()
    }

    fn imported_address(id: &str) -> Option<crate::record::AddressRecord> {
        let reader = Reader::open(TOWN_IMPORTS.built.pack()).expect("reader");
        let record_id = reader.find_by_source_id(id).expect("lookup")?;
        match reader.records().record(record_id).expect("record") {
            Record::Address(address) => Some(address),
            other => panic!("{id} is an address: {other:?}"),
        }
    }

    #[test]
    fn imports_addresses_and_leaves_out_the_ones_osm_has() {
        assert_eq!(
            *TOWN_IMPORTS.source.asked.lock().expect("lock"),
            vec![addresses::Coverage {
                countries: Default::default(),
                subdivisions: std::collections::BTreeSet::from(["CA-ON".to_string()]),
            }],
            "asked once, for the province: the extract has no Canada boundary"
        );
        let [imported] = imports_of("test")[..] else {
            panic!("one file");
        };
        assert_eq!((imported.rows, imported.accepted), (5, 4));
        assert_eq!(imported.rejected.get("missing_number"), Some(&1));
        assert_eq!(
            imported.duplicates, 2,
            "10 King St repeats the OSM address, the second 16 the first"
        );
        let report = &TOWN_IMPORTS.built.report;
        assert!(report.datasets.contains(&"openaddresses:test".to_string()));

        let kept = (1..=5)
            .map(|row| imported_address(&format!("openaddresses:test:row:{row}")).is_some())
            .collect::<Vec<_>>();
        assert_eq!(kept, [false, true, false, true, false]);
        let searcher = TextSearcher::open(TOWN_IMPORTS.built.pack()).expect("searcher");
        let top = |query: &str| {
            searcher
                .search(&TextSearchOptions {
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
    }

    #[test]
    fn leaves_out_the_rows_of_a_source_outside_its_area() {
        let rejected = |dataset: &str| {
            let [import] = imports_of(dataset)[..] else {
                panic!("one file of {dataset}");
            };
            (import.rows, import.accepted, import.rejected.clone())
        };
        assert_eq!(
            rejected("ca/on/sound"),
            (2, 1, BTreeMap::from([("outside_area".to_string(), 1)])),
            "Lake Road lies east of Ontario"
        );
        assert_eq!(
            rejected("ca/on/swapped"),
            (
                3,
                0,
                BTreeMap::from([("source_outside_area".to_string(), 3)])
            ),
            "11 Bloor Street lies inside, in a source that cannot be trusted"
        );
        assert!(imported_address("openaddresses:ca/on/sound:row:1").is_some());
        assert!(imported_address("openaddresses:ca/on/sound:row:2").is_none());
        assert!(imported_address("openaddresses:ca/on/swapped:row:3").is_none());
    }

    #[test]
    fn keeps_a_source_whose_area_left_a_ring_open() {
        let dir = temp_dir("open-ring-area");
        let input = dir.join("town.osm.pbf");
        let mut elements = town();
        elements[1].extend([
            node(1021, 45.0, -75.0, &[]),
            node(1022, 45.0, -74.9, &[]),
            node(1023, 45.1, -74.9, &[]),
            node(1024, 45.1, -75.0, &[]),
        ]);
        for element in elements.iter_mut().flatten() {
            match element {
                TestElement::Way { id: 502, refs, .. } => *refs = vec![1011, 1012, 1013, 1014],
                TestElement::Relation {
                    id: 901, members, ..
                } => *members = vec![(502, "outer"), (503, "outer")],
                _ => {}
            }
        }
        elements[2].push(way(503, &[1021, 1022, 1023, 1024, 1021], &[]));
        write_pbf(&input, &elements);
        let path = dir.join("sound.csv");
        fs::write(
            &path,
            "LON,LAT,NUMBER,STREET\n-79.45,43.65,3,Queen Street\n-79.46,43.66,11,Queen Street\n",
        )
        .expect("csv");
        let report = build_osm_pack(&BuildOsmOptions {
            inputs: vec![input],
            pack: dir.join("pack"),
            addresses: Some(Arc::new(addresses::AddressFiles(vec![
                addresses::AddressFile {
                    dataset: "openaddresses:ca/on/sound".to_string(),
                    path,
                    entry: None,
                    area: Some("CA-ON".to_string()),
                },
            ]))),
            ..BuildOsmOptions::default()
        })
        .expect("build");

        let import = &report.imports[0];
        assert_eq!(
            (import.rows, import.accepted, import.rejected.clone()),
            (2, 2, BTreeMap::new()),
            "both rows lie on the mainland, outside the one ring that closed"
        );
        let reader = Reader::open(dir.join("pack")).expect("reader");
        assert_eq!(reader.manifest().layer_counts["address"], 5 + 2);
    }

    #[test]
    fn keeps_one_record_of_a_house_two_sources_place_apart() {
        let [imported] = imports_of("disputed")[..] else {
            panic!("one file");
        };
        assert_eq!((imported.duplicates, imported.disputes), (0, 1));
        let disputed = |id: &str| {
            imported_address(id)
                .unwrap_or_else(|| panic!("{id} is in the Pack"))
                .disputed
        };
        assert!(disputed("osm:node:20001"), "OSM's point is kept, disputed");
        assert_eq!(
            imported_address("openaddresses:disputed:row:1"),
            None,
            "the row 445 m away is dropped"
        );
        assert!(
            !disputed("openaddresses:disputed:row:2"),
            "11 km away: another house"
        );
        assert!(
            !disputed("openaddresses:disputed:row:3"),
            "another postcode: another house"
        );
    }

    #[test]
    fn an_address_in_capitals_takes_the_spelling_its_street_writes() {
        for (row, (street, postcodes)) in [
            ("Wellington Street", &["M5H 1A1"][..]),
            ("RICHMOND STREET", &[][..]),
            ("Adelaide Street", &[][..]),
            ("Adelaide Street", &["M5G 1H2"][..]),
            ("ADELAIDE ST", &["M5G 1H2"][..]),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("openaddresses:capitals:row:{}", row + 1);
            let address = imported_address(&id).expect(&id);
            assert_eq!(address.address.street.as_deref(), Some(street), "{id}");
            assert_eq!(address.street_postcodes, postcodes, "{id}");
        }
    }

    #[test]
    fn a_house_one_source_places_apart_is_repeated_not_disputed() {
        let counts = ["apart-lots", "apart-register"]
            .into_iter()
            .flat_map(imports_of)
            .map(|import| (import.duplicates, import.disputes))
            .fold((0, 0), |(duplicates, disputes), (more, also)| {
                (duplicates + more, disputes + also)
            });
        assert_eq!(counts, (1, 1), "20 repeats within lots; 30 is disputed");
        let kept = |ids: &[&str]| {
            ids.iter()
                .filter_map(|id| imported_address(id).map(|address| address.disputed))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            kept(&[
                "openaddresses:apart-lots:row:1",
                "openaddresses:apart-lots:row:2"
            ]),
            [false],
            "one record of 20, not disputed"
        );
        assert_eq!(
            kept(&[
                "openaddresses:apart-lots:row:3",
                "openaddresses:apart-register:row:1"
            ]),
            [true],
            "one record of 30, disputed"
        );
    }

    #[test]
    fn a_dropped_row_counts_on_the_first_file_of_its_dataset() {
        let counts = TOWN_IMPORTS
            .built
            .report
            .imports
            .iter()
            .filter(|import| import.dataset.starts_with("openaddresses:dropped-"))
            .map(|import| (import.dataset.as_str(), import.duplicates, import.disputes))
            .collect::<Vec<_>>();
        assert_eq!(
            counts,
            [
                ("openaddresses:dropped-empty", 0, 0),
                ("openaddresses:dropped-lots", 2, 0),
                ("openaddresses:dropped-register", 0, 1),
                ("openaddresses:dropped-lots", 0, 0),
            ]
        );
    }

    #[test]
    fn judges_a_source_by_its_first_rows_alone() {
        let judged = ["cutoff", "late", "half", "most", "unplaced", "empty"]
            .into_iter()
            .map(|name| {
                let [import] = imports_of(&format!("ca/on/{name}"))[..] else {
                    panic!("one file of {name}");
                };
                (
                    name,
                    import.rows,
                    import.accepted,
                    import
                        .rejected
                        .iter()
                        .map(|(reason, rows)| (reason.as_str(), *rows))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            judged,
            [
                ("cutoff", 1001, 1000, vec![("outside_area", 1)]),
                ("late", 1001, 0, vec![("source_outside_area", 1001)]),
                ("half", 1000, 500, vec![("outside_area", 500)]),
                ("most", 1000, 0, vec![("source_outside_area", 1000)]),
                ("unplaced", 1001, 1, vec![("bad_coordinates", 1000)]),
                ("empty", 0, 0, vec![]),
            ]
        );
    }

    fn slash_numbers() -> Vec<Vec<TestElement>> {
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
        vec![vec![
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
        ]]
    }

    #[test]
    fn reads_a_slash_number_as_a_flat_of_its_building_never_a_house() {
        let searcher = TextSearcher::open(ADMIN.built.pack()).expect("searcher");
        let top = |query: &str| {
            searcher
                .search(&TextSearchOptions {
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
                .geocode_address(&crate::search::AddressGeocodeOptions {
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
        assert_eq!(top("1367 Eruera Street"), vec!["osm:node:300014"]);
        assert_eq!(top("10/1367 Eruera Street"), vec!["osm:node:300015"]);
        for query in ["10 Eruera Street", "2 Eruera Street"] {
            assert_eq!(top(query), vec!["osm:way:300100"], "{query}");
            assert_eq!(geocode(query).as_deref(), Some("osm:way:300100"), "{query}");
        }
        assert_eq!(
            top("1300 Eruera Street"),
            vec!["derived:estimate:1300:osm:node:300011:osm:node:300013"]
        );
    }

    #[test]
    fn builds_a_searchable_reverse_geocodable_pack_from_pbf() {
        let report = &TOWN.report;
        let input = &TOWN.inputs[0];
        let reader = Reader::open(TOWN.pack()).expect("reader");
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
        assert_eq!(report.geometry_resolution.required_node_refs, 27);
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
        let input_bytes = fs::metadata(input).expect("input").len();
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
        assert!(report.scratch.peak_tracked_bytes > 0);
        assert!(report.scratch.peak_tracked_bytes <= report.scratch.memory_budget_bytes);

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

        let searcher = TextSearcher::open(TOWN.pack()).expect("searcher");
        let hits = searcher
            .search(&TextSearchOptions {
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

        let reverse = PackReverseGeocoder::open(TOWN.pack()).expect("reverse");
        let result = |lon, lat| {
            reverse
                .reverse(GeocodeOptions::at(lon, lat))
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

        let bench = crate::bench::benchmark_pack(&crate::bench::PackBenchmarkOptions {
            pack: TOWN.pack(),
            queries: None,
            iterations: 1,
            warmup: 0,
        })
        .expect("bench");
        let build_metrics = bench.pack.build.expect("build metrics");
        assert_eq!(build_metrics.accepted_records, report.accepted.total);
        assert_eq!(build_metrics.scratch, report.scratch);
    }

    fn ward() -> Vec<Vec<TestElement>> {
        vec![
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
        ]
    }

    #[test]
    fn indexes_every_name_and_accepts_block_scheme_addresses() {
        let reader = ADMIN.pack();
        let address = reader
            .find_by_source_id("osm:node:100010")
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
            .find_by_source_id("osm:way:100500")
            .expect("lookup")
            .expect("ward");
        assert_eq!(
            reader.records().names(ward).expect("names"),
            vec!["千代田区", "Chiyoda", "ちよだく", "chiyodaku"]
        );

        let searcher = TextSearcher::open(ADMIN.built.pack()).expect("searcher");
        let first = |query: &str, layer: &str| {
            searcher
                .search(&TextSearchOptions {
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
            Some("osm:way:100600")
        );
        assert_eq!(
            first("Chiyoda", "locality").as_deref(),
            Some("osm:way:100500")
        );
        assert_eq!(
            first("ちよだく", "locality").as_deref(),
            Some("osm:way:100500")
        );
        assert_eq!(
            first("chiyodaku", "locality").as_deref(),
            Some("osm:way:100500"),
            "the Latin form of a kana name"
        );

        let tagger =
            crate::parse::lattice::LatticeTagger::open(&ADMIN.built.pack()).expect("tagger");
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

    fn honshu() -> Vec<Vec<TestElement>> {
        vec![
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
        ]
    }

    #[test]
    fn places_drawn_as_areas_are_places_and_context() {
        let reader = ADMIN.pack();
        let island = reader
            .find_by_source_id("osm:relation:200900")
            .expect("lookup")
            .expect("island");
        let suburb = reader
            .find_by_source_id("osm:way:200502")
            .expect("lookup")
            .expect("suburb");
        assert!(
            reader
                .find_by_source_id("osm:relation:200901")
                .expect("lookup")
                .is_none()
        );
        let address = reader
            .find_by_source_id("osm:node:200010")
            .expect("lookup")
            .expect("address");
        let context = reader
            .boundary_context(address)
            .expect("context")
            .expect("address context")
            .admin_context;
        assert_eq!(context.place_record_id(), Some(island));
        let layer = |record| reader.record_summary(record).expect("summary").layer;
        assert_eq!(
            (layer(island), layer(suburb)),
            ("place".into(), "neighbourhood".into())
        );
        assert_eq!(context.neighbourhood_record_id(), Some(suburb));

        let tagger =
            crate::parse::lattice::LatticeTagger::open(&ADMIN.built.pack()).expect("tagger");
        use crate::parse::lattice::dictionary::Field;
        for key in ["本州", "honshu", "港北"] {
            assert!(
                tagger.dictionaries().contains(Field::Locality, key),
                "{key}"
            );
        }
    }

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
        let report = &TWO_COUNTRIES.report;
        let reader = Reader::open(TWO_COUNTRIES.pack()).expect("reader");
        assert_eq!(report.accepted.pois_with_address, 1);
        assert_eq!(report.accepted.poi_nodes, 3);
        assert_eq!(report.accepted.poi_way_centroids, 1);
        assert_eq!(report.accepted.poi_categories.get("amenity"), Some(&2));
        assert_eq!(report.geometry_resolution.poi_way_stubs, 1);

        let summaries = (0..reader.manifest().record_count)
            .map(|id| (id, reader.record_summary(id).expect("summary")))
            .collect::<Vec<_>>();
        let mut pois = summaries
            .iter()
            .filter(|(_, summary)| summary.layer == "poi")
            .map(|(_, summary)| summary.id.as_str())
            .collect::<Vec<_>>();
        pois.sort_unstable();
        assert_eq!(
            pois,
            [
                "osm:node:2001",
                "osm:node:2002",
                "osm:node:2009",
                "osm:way:700"
            ]
        );
        let layers = |id: &str| {
            summaries
                .iter()
                .filter(|(_, summary)| summary.id == id)
                .map(|(_, summary)| summary.layer.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            layers("osm:node:2004"),
            ["address"],
            "the unnamed cafe stays an address"
        );
        let mut cafe = summaries
            .iter()
            .filter(|(_, summary)| summary.id == "osm:node:2001")
            .map(|(id, summary)| (summary.layer.as_str(), *id))
            .collect::<Vec<_>>();
        cafe.sort_unstable();
        assert_eq!(
            cafe.iter().map(|(layer, _)| *layer).collect::<Vec<_>>(),
            ["address", "poi"]
        );
        assert!(
            !summaries
                .iter()
                .any(|(_, summary)| summary.id == "osm:node:2003"),
            "bench"
        );
        let Record::Poi(poi) = reader.records().record(cafe[1].1).expect("record") else {
            panic!("the cafe is a POI");
        };
        assert_eq!(poi.category, "amenity:cafe");
        assert_eq!(
            poi.address.expect("address").street.as_deref(),
            Some("King Street West")
        );

        let addresses = TextSearcher::open(TWO_COUNTRIES.pack()).expect("searcher");
        let places = addresses.places();
        let search = |query: &str, layer: Option<&str>| -> Vec<TextSearchHit> {
            let searcher = match layer {
                Some("poi") | None => &places,
                Some(_) => &addresses,
            };
            searcher
                .search(&TextSearchOptions {
                    query: query.into(),
                    limit: 5,
                    layer: layer.filter(|layer| *layer != "poi").map(str::to_string),
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
        let address = search("123 King Street West", Some("address"));
        assert_eq!(ids(&address), vec!["osm:node:2001"]);
        assert_eq!(address[0].record.layer, "address");
        assert!(search("Riverdale Farm", Some("address")).is_empty());
        let station = search("Union Station Toronto", Some("poi"));
        assert_eq!(ids(&station), vec!["osm:way:700"]);
        assert_eq!(
            station[0].record.point.expect("point").precision,
            PointPrecision::Centroid
        );
        assert!(search("Memorial Bench", None).is_empty());

        let suggestions = places
            .autocomplete(&TextAutocompleteOptions {
                query: "riverdale fa".into(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("autocomplete");
        assert_eq!(ids(&suggestions), vec!["osm:node:2002"]);

        let reverse = PackReverseGeocoder::open(TWO_COUNTRIES.pack()).expect("reverse");
        let result = reverse
            .reverse(GeocodeOptions::at(-79.38, 43.65))
            .expect("reverse")
            .result
            .expect("result");
        assert_eq!(result.match_kind, MatchKind::ExplicitAddress);
        assert_eq!(result.id.as_deref(), Some("osm:node:2001"));
        assert_eq!(result.label, "123 King Street West, M5V 1A1");
        assert_eq!(result.context.locality.as_deref(), Some("Toronto"));
    }

    static NL_PLACES: LazyLock<Built> = LazyLock::new(|| {
        Built::with(
            "nl-places",
            one_extract(vec![
                ("cafes", renumbered(cafes(), 1000)),
                ("stadium", renumbered(stadium(), 2000)),
                ("brands", renumbered(brands(), 3000)),
                ("contact", moved(renumbered(contacts(), 4000), 1.0, 0.0)),
                ("bbox", moved(renumbered(outlines(), 5000), 2.0, 0.0)),
                ("zones", renumbered(zoned_town(), 6000)),
                ("island", moved(renumbered(island_town(), 7000), 4.0, 0.0)),
                (
                    "entrances",
                    moved(renumbered(buildings_with_entrances(), 8000), 6.0, 0.0),
                ),
                ("ranked", renumbered(ranked_cafes(), 9000)),
            ]),
            |dir| {
                let qrank = dir.join("qrank.csv");
                fs::write(&qrank, "Entity,QRank\nQ11,40\nQ22,90000\n").expect("qrank");
                BuildOsmOptions {
                    timezones: Some(timezones(dir)),
                    qrank: Some(qrank),
                    ..BuildOsmOptions::default()
                }
            },
        )
    });

    fn place_search(query: &str) -> Vec<TextSearchHit> {
        TextSearcher::open(NL_PLACES.pack())
            .expect("searcher")
            .places()
            .search(&TextSearchOptions {
                query: query.into(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search")
    }

    fn cafes() -> Vec<Vec<TestElement>> {
        let cafe = [("name", "Cafetaria De Pindol"), ("amenity", "cafe")];
        vec![
            vec![
                node(
                    1,
                    51.41,
                    5.40000,
                    &[
                        cafe[0],
                        cafe[1],
                        ("addr:housenumber", "82"),
                        ("addr:street", "Dorpstraat"),
                    ],
                ),
                node(2, 51.41, 5.40030, &cafe),
                node(20, 51.40990, 5.39985, &[]),
                node(21, 51.40990, 5.40015, &[]),
                node(22, 51.41010, 5.40015, &[]),
                node(23, 51.41010, 5.39985, &[]),
            ],
            vec![way(
                10,
                &[20, 21, 22, 23, 20],
                &[cafe[0], cafe[1], ("building", "yes")],
            )],
            vec![],
        ]
    }

    #[test]
    fn one_cafe_mapped_twice_is_one_place_found_by_either_id() {
        let reader = Reader::open(NL_PLACES.pack()).expect("reader");
        let catalogue = crate::ids::Catalogue::open(reader.container())
            .expect("open")
            .expect("a catalogue");
        let find = |object_type, id| {
            catalogue
                .find(reader.records(), object_type, id)
                .expect("find")
        };
        let first = find(ObjectType::Way, 1010);
        assert_eq!(first.len(), 1);
        assert_eq!(
            first,
            find(ObjectType::Node, 1001),
            "the merged-away id finds the place"
        );
        assert_ne!(find(ObjectType::Node, 1002), first);
        let Record::Poi(cafe) = reader.records().record(first[0]).expect("record") else {
            panic!("a place");
        };
        assert_eq!(
            cafe.address.expect("the address one copy stated").number,
            "82"
        );
    }

    fn stadium() -> Vec<Vec<TestElement>> {
        let nodes = vec![
            node(1, 52.300, 4.930, &[]),
            node(2, 52.300, 4.950, &[]),
            node(3, 52.320, 4.950, &[]),
            node(4, 52.320, 4.930, &[]),
            node(5, 52.305, 4.935, &[]),
            node(6, 52.305, 4.945, &[]),
            node(7, 52.315, 4.945, &[]),
            node(8, 52.315, 4.935, &[]),
        ];
        let ways = vec![
            way(10, &[1, 2, 3], &[]),
            way(11, &[3, 4, 1], &[]),
            way(12, &[5, 6, 7, 8, 5], &[]),
        ];
        let relations = vec![relation(
            100,
            &[(10, "outer"), (11, "outer"), (12, "inner")],
            &[
                ("type", "multipolygon"),
                ("leisure", "stadium"),
                ("name", "Johan Cruijff ArenA"),
            ],
        )];
        vec![nodes, ways, relations]
    }

    #[test]
    fn a_place_drawn_as_a_multipolygon_is_a_place_inside_its_outline() {
        let hit = place_search("Johan Cruijff ArenA")
            .into_iter()
            .next()
            .expect("the stadium");
        assert_eq!(hit.record.id, "osm:relation:2100");
        assert_eq!(hit.record.category.as_deref(), Some("leisure:stadium"));
        let point = hit.record.point.expect("point");
        let inside = |(low, high): (f64, f64), value: f64| low < value && value < high;
        assert!(inside((4.930, 4.950), point.lon) && inside((52.300, 52.320), point.lat));
        assert!(
            !(inside((4.935, 4.945), point.lon) && inside((52.305, 52.315), point.lat)),
            "its point is not in the hole: {point:?}"
        );
    }

    fn brands() -> Vec<Vec<TestElement>> {
        vec![
            vec![
                node(
                    1,
                    52.37,
                    4.89,
                    &[
                        ("name", "Tankstation Zanoli"),
                        ("amenity", "fuel"),
                        ("brand", "Esso"),
                        ("brand:wikidata", "Q867662"),
                    ],
                ),
                node(
                    2,
                    52.38,
                    4.90,
                    &[("amenity", "cafe"), ("brand", "Starbucks")],
                ),
                node(
                    3,
                    52.31,
                    4.76,
                    &[
                        ("name", "Amsterdam Airport Schiphol"),
                        ("aeroway", "aerodrome"),
                        ("iata", "AMS"),
                    ],
                ),
                node(4, 52.36, 4.88, &[("name", "Bench"), ("amenity", "bench")]),
            ],
            vec![],
            vec![],
        ]
    }

    #[test]
    fn a_place_is_found_by_its_brand_and_codes_and_drops_are_counted() {
        assert_eq!(
            NL_PLACES
                .report
                .accepted
                .places_left_out
                .get("amenity=bench"),
            Some(&1)
        );
        let reader = Reader::open(NL_PLACES.pack()).expect("reader");
        assert!(absent(&reader, "osm:node:3004"), "a bench is no place");
        let top = |query: &str| {
            place_search(query)
                .into_iter()
                .next()
                .expect("a place")
                .record
        };
        let esso = top("Esso");
        assert_eq!(esso.name, "Tankstation Zanoli");
        let brand = esso.brand.expect("a brand");
        assert_eq!(
            (brand.name.as_str(), brand.wikidata),
            ("Esso", Some(867_662))
        );
        assert_eq!(top("Starbucks").id, "osm:node:3002");
        assert_eq!(top("AMS").id, "osm:node:3003");
    }

    fn contacts() -> Vec<Vec<TestElement>> {
        vec![
            vec![
                node(
                    1,
                    52.37,
                    4.89,
                    &[
                        ("name", "Cafe Zanoli"),
                        ("amenity", "cafe"),
                        ("phone", "+31 20 555 0111;+31 20 555 0112"),
                        ("contact:website", "https://Zanoli.example/"),
                        ("email", "n/a"),
                        ("opening_hours", "Mo-Fr 08:00-18:00; Sa 09:00-13:00"),
                    ],
                ),
                node(
                    2,
                    52.38,
                    4.90,
                    &[("name", "Cafe Blauw"), ("amenity", "cafe")],
                ),
                node(
                    3,
                    52.39,
                    4.91,
                    &[
                        ("name", "Cafe Oud"),
                        ("disused:amenity", "cafe"),
                        ("phone", "+31 20 555 0113"),
                    ],
                ),
            ],
            vec![],
            vec![],
        ]
    }

    #[test]
    fn a_built_place_answers_with_the_contact_and_hours_its_tags_state() {
        let found = |query: &str| {
            place_search(query)
                .into_iter()
                .map(|hit| hit.record)
                .map(|record| (record.id, record.contact, record.opening_hours))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            found("Cafe Zanoli")[0],
            (
                "osm:node:4001".to_string(),
                Some(crate::record::Contact {
                    fax: None,
                    phone: Some("+31 20 555 0111".to_string()),
                    website: Some("https://Zanoli.example/".to_string()),
                    email: None,
                }),
                Some("Mo-Fr 08:00-18:00; Sa 09:00-13:00".to_string()),
            )
        );
        assert_eq!(
            found("Cafe Blauw")[0],
            ("osm:node:4002".to_string(), None, None)
        );
        assert!(
            found("Cafe Oud")
                .iter()
                .all(|(id, _, _)| id != "osm:node:4003")
        );
    }

    fn outlines() -> Vec<Vec<TestElement>> {
        vec![
            vec![
                node(1, 52.0, 4.0, &[]),
                node(2, 52.0, 4.2, &[]),
                node(3, 52.1, 4.2, &[]),
                node(4, 52.1, 4.0, &[]),
                node(10, 52.03, 4.05, &[]),
                node(11, 52.04, 4.07, &[]),
                node(12, 52.035, 4.11, &[]),
                node(20, 52.0500, 4.1000, &[]),
                node(21, 52.0500, 4.1004, &[]),
                node(22, 52.0503, 4.1004, &[]),
                node(23, 52.0503, 4.1000, &[]),
                node(
                    30,
                    52.06,
                    4.12,
                    &[("name", "Cafe Kade"), ("amenity", "cafe")],
                ),
                node(
                    31,
                    52.07,
                    4.13,
                    &[("addr:housenumber", "8"), ("addr:street", "Kadeweg")],
                ),
                node(40, -17.0, 179.0, &[]),
                node(41, -17.0, 180.0, &[]),
                node(42, -16.0, 180.0, &[]),
                node(43, -16.0, 179.0, &[]),
                node(44, -17.0, -180.0, &[]),
                node(45, -17.0, -179.5, &[]),
                node(46, -16.5, -179.5, &[]),
                node(47, -16.5, -180.0, &[]),
            ],
            vec![
                way(100, &[1, 2, 3, 4, 1], &[]),
                way(
                    101,
                    &[10, 11, 12],
                    &[("highway", "residential"), ("name", "Kadeweg")],
                ),
                way(
                    102,
                    &[20, 21, 22, 23, 20],
                    &[("name", "Havenhal"), ("amenity", "theatre")],
                ),
                way(103, &[40, 41, 42, 43, 40], &[]),
                way(104, &[44, 45, 46, 47, 44], &[]),
            ],
            vec![
                relation(
                    900,
                    &[(100, "outer")],
                    &[
                        ("boundary", "administrative"),
                        ("admin_level", "8"),
                        ("name", "Kadestad"),
                    ],
                ),
                relation(
                    901,
                    &[(103, "outer"), (104, "outer")],
                    &[
                        ("type", "multipolygon"),
                        ("place", "island"),
                        ("name", "Datumeiland"),
                    ],
                ),
            ],
        ]
    }

    #[test]
    fn a_built_area_street_and_outlined_place_answer_with_their_bbox() {
        let reader = Reader::open(NL_PLACES.pack()).expect("reader");
        let bbox = |id: &str| {
            let record = reader
                .find_by_source_id(id)
                .expect("lookup")
                .unwrap_or_else(|| panic!("{id} is in the Pack"));
            reader.record_summary(record).expect("summary").bbox
        };
        assert_eq!(bbox("osm:relation:5900"), Some([4.0, 54.0, 4.2, 54.1]));
        assert_eq!(bbox("osm:way:5101"), Some([4.05, 54.03, 4.11, 54.04]));
        assert_eq!(bbox("osm:way:5102"), Some([4.1, 54.05, 4.1004, 54.0503]));
        assert_eq!(
            bbox("osm:relation:5901"),
            Some([179.0, -15.0, -179.5, -14.0])
        );
        assert_eq!(bbox("osm:node:5030"), None);
        assert_eq!(bbox("osm:node:5031"), None);
    }

    fn timezones(dir: &Path) -> PathBuf {
        let zones = dir.join("zones.json");
        let zone = |tzid: &str, west: f64, east: f64| {
            format!(
                r#"{{"type":"Feature","properties":{{"tzid":"{tzid}"}},"geometry":{{"type":"Polygon","coordinates":[[[{west},51.9],[{east},51.9],[{east},52.2],[{west},52.2],[{west},51.9]]]}}}}"#
            )
        };
        fs::write(
            &zones,
            format!(
                r#"{{"type":"FeatureCollection","features":[{},{}]}}"#,
                zone("Test/West", 3.9, 4.1),
                zone("Test/East", 4.1, 4.3)
            ),
        )
        .expect("zones");
        zones
    }

    static ZONELESS: LazyLock<Built> =
        LazyLock::new(|| Built::new("zoneless", vec![("zones", zoned_town())]));

    fn zoned_town() -> Vec<Vec<TestElement>> {
        let house = |id: i64, lat: f64, lon: f64| {
            node(
                id,
                lat,
                lon,
                &[("addr:housenumber", "8"), ("addr:street", "Kadeweg")],
            )
        };
        vec![
            vec![
                node(1, 52.0, 4.0, &[]),
                node(2, 52.0, 4.2, &[]),
                node(3, 52.1, 4.2, &[]),
                node(4, 52.1, 4.0, &[]),
                node(5, 52.02, 4.02, &[]),
                node(6, 52.02, 4.06, &[]),
                node(7, 52.06, 4.06, &[]),
                node(8, 52.06, 4.02, &[]),
                house(10, 52.04, 4.04),
                house(11, 52.08, 4.08),
                house(12, 52.05, 4.15),
                node(
                    13,
                    52.05,
                    4.25,
                    &[("name", "Cafe Oost"), ("amenity", "cafe")],
                ),
                house(14, 52.05, 4.5),
            ],
            vec![
                way(100, &[1, 2, 3, 4, 1], &[]),
                way(
                    101,
                    &[5, 6, 7, 8, 5],
                    &[("place", "suburb"), ("name", "Westwijk")],
                ),
            ],
            vec![relation(
                900,
                &[(100, "outer")],
                &[
                    ("boundary", "administrative"),
                    ("admin_level", "8"),
                    ("name", "Havenstad"),
                ],
            )],
        ]
    }

    #[test]
    fn a_built_area_lies_in_the_areas_broader_than_it() {
        let reader = Reader::open(ZONELESS.pack()).expect("reader");
        let record = |id: &str| {
            reader
                .find_by_source_id(id)
                .expect("lookup")
                .unwrap_or_else(|| panic!("{id} is in the Pack"))
        };
        let chain = |id: &str| {
            reader
                .boundary_context(record(id))
                .expect("context")
                .map(|context| {
                    context
                        .admin_context
                        .entries()
                        .iter()
                        .map(|entry| entry.record_id)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let (town, suburb) = (record("osm:relation:900"), record("osm:way:101"));
        assert_eq!(chain("osm:way:101"), [town]);
        assert_eq!(chain("osm:relation:900"), Vec::<RecordId>::new());
        assert!(chain("osm:node:10").contains(&suburb));
    }

    fn section_formats(pack: &Path) -> Vec<(String, u32)> {
        let reader = Reader::open(pack).expect("reader");
        let mut formats = reader
            .container()
            .sections()
            .iter()
            .map(|(name, info)| {
                let name = match name.split_once('/') {
                    Some((group, _)) => format!("{group}/*"),
                    None => name.clone(),
                };
                (name, info.version)
            })
            .collect::<Vec<_>>();
        formats.dedup();
        formats
    }

    #[test]
    fn the_schema_moves_with_every_section() {
        let formats = section_formats(&TOWN.pack());
        let expected = [
            ("context/*", 1),
            ("context/*", 4),
            ("country_codes", 1),
            ("default_languages", 1),
            ("extents", 1),
            ("gazetteer/*", 1),
            ("language_names", 1),
            ("manifest", 1),
            ("parse/*", 4),
            ("places/*", 3),
            ("plate_codes", 1),
            ("postcode_addresses", 1),
            ("records/*", 15),
            ("spatial/*", 3),
            ("streets/*", 1),
            ("subdivision_codes", 1),
            ("text/*", 15),
        ]
        .map(|(name, version)| (name.to_string(), version));
        assert_eq!(
            (crate::pack::PACK_SCHEMA_VERSION, formats),
            (11, expected.to_vec())
        );
    }

    fn island_town() -> Vec<Vec<TestElement>> {
        vec![
            vec![
                node(1, 52.0, 4.0, &[]),
                node(2, 52.0, 4.2, &[]),
                node(3, 52.1, 4.2, &[]),
                node(4, 52.1, 4.0, &[]),
                node(5, 51.99, 3.95, &[]),
                node(6, 51.99, 4.3, &[]),
                node(7, 52.11, 4.3, &[]),
                node(8, 52.11, 3.95, &[]),
            ],
            vec![
                way(100, &[1, 2, 3, 4, 1], &[]),
                way(
                    101,
                    &[5, 6, 7, 8, 5],
                    &[("place", "island"), ("name", "Groot Eiland")],
                ),
            ],
            vec![relation(
                900,
                &[(100, "outer")],
                &[
                    ("boundary", "administrative"),
                    ("admin_level", "8"),
                    ("name", "Eilandstad"),
                ],
            )],
        ]
    }

    #[test]
    fn an_area_lies_only_in_the_areas_that_hold_it() {
        let reader = Reader::open(NL_PLACES.pack()).expect("reader");
        let island = reader
            .find_by_source_id("osm:way:7101")
            .expect("lookup")
            .expect("the island is in the Pack");
        let point = reader
            .record_summary(island)
            .expect("summary")
            .point
            .expect("point");
        assert!(
            (4.0..4.2).contains(&point.lon) && (56.0..56.1).contains(&point.lat),
            "the island's point is in the town"
        );
        let chain = reader
            .boundary_context(island)
            .expect("context")
            .map_or(0, |context| context.admin_context.entries().len());
        assert_eq!(chain, 0, "the town does not hold the island");
    }

    #[test]
    fn a_built_record_answers_with_the_timezone_it_lies_in() {
        let zoned = NL_PLACES
            .report
            .timezones
            .as_ref()
            .expect("the timezone file read");
        assert_eq!(
            (zoned.path.clone(), zoned.zones),
            (NL_PLACES.dir.join("zones.json").display().to_string(), 2)
        );

        let reader = Reader::open(NL_PLACES.pack()).expect("reader");
        let record = |id: &str| {
            reader
                .find_by_source_id(id)
                .expect("lookup")
                .unwrap_or_else(|| panic!("{id} is in the Pack"))
        };
        let timezone = |id: &str| {
            let record = record(id);
            let zone = reader
                .timezone(record)
                .expect("timezone")
                .map(str::to_string);
            assert_eq!(
                reader.record_summary(record).expect("summary").timezone,
                zone
            );
            zone
        };
        let stamped = |id: &str| {
            reader
                .boundary_context(record(id))
                .expect("context")
                .and_then(|context| context.zone)
        };
        let (west, east) = (Some("Test/West".to_string()), Some("Test/East".to_string()));
        assert_eq!(timezone("osm:way:6101"), west, "a suburb in one zone");
        assert_eq!(timezone("osm:relation:6900"), None, "a town in two");
        assert_eq!(
            (timezone("osm:node:6010"), stamped("osm:node:6010")),
            (west.clone(), None)
        );
        assert_eq!(
            (timezone("osm:node:6011"), stamped("osm:node:6011")),
            (west, Some(0))
        );
        assert_eq!(
            (timezone("osm:node:6012"), stamped("osm:node:6012")),
            (east.clone(), Some(1))
        );
        assert_eq!(
            (timezone("osm:node:6013"), stamped("osm:node:6013")),
            (east, Some(1))
        );
        assert_eq!(
            (timezone("osm:node:6014"), stamped("osm:node:6014")),
            (None, None)
        );
        assert_eq!(
            reader
                .boundary_context(record("osm:node:6013"))
                .expect("context"),
            Some(RecordContext {
                zone: Some(1),
                ..RecordContext::default()
            }),
            "a zone alone is a context"
        );
    }

    #[test]
    fn a_build_given_no_timezones_or_a_bad_file_names_no_zone() {
        assert_eq!(ZONELESS.report.timezones, None);
        let reader = Reader::open(ZONELESS.pack()).expect("reader");
        for id in [
            "osm:way:101",
            "osm:relation:900",
            "osm:node:10",
            "osm:node:13",
        ] {
            let record = reader.find_by_source_id(id).expect("lookup").expect(id);
            assert_eq!(reader.timezone(record).expect("timezone"), None, "{id}");
            assert_eq!(
                reader.record_summary(record).expect("summary").timezone,
                None,
                "{id}"
            );
        }
        assert_eq!(
            reader
                .boundary_context(
                    reader
                        .find_by_source_id("osm:node:13")
                        .expect("lookup")
                        .expect("cafe")
                )
                .expect("context"),
            None
        );

        let dir = temp_dir("bad-timezones");
        let zones = dir.join("zones.json");
        fs::write(&zones, r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"tzid":"Test/Line"},"geometry":{"type":"LineString","coordinates":[[4.0,52.0],[4.1,52.0]]}}]}"#)
            .expect("zones");
        let error = build_osm_pack(&BuildOsmOptions {
            inputs: ZONELESS.inputs.clone(),
            pack: dir.join("bad"),
            timezones: Some(zones.clone()),
            ..BuildOsmOptions::default()
        })
        .expect_err("a file of lines");
        assert_eq!(
            error.to_string(),
            Error::BadInput(format!(
                "timezone file {} is not usable (zone Test/Line is a LineString, not a Polygon \
                 or a MultiPolygon at line 1 column 164). It must be the GeoJSON of \
                 timezone-boundary-builder, one feature per zone, each a Polygon or a \
                 MultiPolygon with a `tzid` property",
                zones.display()
            ))
            .to_string()
        );
        assert!(
            Reader::open(dir.join("bad")).is_err(),
            "nothing is published"
        );
    }

    fn buildings_with_entrances() -> Vec<Vec<TestElement>> {
        let house =
            |number: &'static str| [("addr:housenumber", number), ("addr:street", "Kadeweg")];
        vec![
            vec![
                node(1, 52.0000, 4.0000, &[("entrance", "main")]),
                node(2, 52.0000, 4.0004, &[]),
                node(3, 52.0003, 4.0004, &[("entrance", "service")]),
                node(4, 52.0003, 4.0000, &[("entrance", " ")]),
                node(10, 52.0010, 4.0010, &[("entrance", "yes")]),
                node(11, 52.0010, 4.0014, &[]),
                node(12, 52.0013, 4.0014, &[]),
                node(13, 52.0013, 4.0010, &[]),
                node(
                    20,
                    52.0020,
                    4.0020,
                    &[
                        ("addr:housenumber", "8"),
                        ("addr:street", "Kadeweg"),
                        ("entrance", "main"),
                    ],
                ),
                node(21, 52.0021, 4.0021, &house("9")),
                node(
                    22,
                    52.0022,
                    4.0022,
                    &[
                        ("name", "Cafe Deur"),
                        ("amenity", "cafe"),
                        ("entrance", "staircase"),
                    ],
                ),
                node(30, 52.0030, 4.0030, &[]),
                node(31, 52.0030, 4.0040, &[("entrance", "main")]),
                node(32, 52.0040, 4.0040, &[]),
                node(33, 52.0040, 4.0030, &[]),
                node(34, 52.0033, 4.0033, &[]),
                node(35, 52.0033, 4.0037, &[("entrance", "emergency")]),
                node(36, 52.0037, 4.0037, &[]),
                node(37, 52.0037, 4.0033, &[]),
                node(40, 52.0050, 4.0050, &[]),
                node(41, 52.0050, 4.0054, &[]),
                node(42, 52.0053, 4.0054, &[]),
                node(43, 52.0053, 4.0050, &[]),
            ],
            vec![
                way(
                    100,
                    &[1, 2, 3, 4, 1],
                    &[
                        ("building", "yes"),
                        ("addr:housenumber", "5"),
                        ("addr:street", "Kadeweg"),
                    ],
                ),
                way(
                    101,
                    &[10, 11, 12, 13, 10],
                    &[("name", "Havenhal"), ("amenity", "theatre")],
                ),
                way(102, &[30, 31, 32], &[]),
                way(103, &[32, 33, 30], &[]),
                way(104, &[34, 35, 36, 37, 34], &[]),
                way(
                    105,
                    &[40, 41, 42, 43, 40],
                    &[("name", "Lege Hal"), ("amenity", "theatre")],
                ),
            ],
            vec![relation(
                900,
                &[(102, "outer"), (103, "outer"), (104, "inner")],
                &[
                    ("type", "multipolygon"),
                    ("name", "Havenschool"),
                    ("amenity", "school"),
                ],
            )],
        ]
    }

    #[test]
    fn a_built_record_answers_with_the_entrances_mapped_for_it() {
        use crate::record::AccessPoint;
        let reader = Reader::open(NL_PLACES.pack()).expect("reader");
        let access = |id: &str| {
            let record = reader
                .find_by_source_id(id)
                .expect("lookup")
                .unwrap_or_else(|| panic!("{id} is in the Pack"));
            reader.record_summary(record).expect("summary").access
        };
        let point = |lon: f64, lat: f64, kind: &str| AccessPoint {
            lon,
            lat,
            kind: kind.to_string(),
        };
        assert_eq!(
            access("osm:way:8100"),
            [point(4.0, 58.0, "main"), point(4.0004, 58.0003, "service")]
        );
        assert_eq!(access("osm:way:8101"), [point(4.001, 58.001, "yes")]);
        assert_eq!(access("osm:node:8020"), [point(4.002, 58.002, "main")]);
        assert_eq!(access("osm:node:8021"), []);
        assert_eq!(
            access("osm:node:8022"),
            [point(4.0022, 58.0022, "staircase")]
        );
        assert_eq!(
            access("osm:relation:8900"),
            [
                point(4.004, 58.003, "main"),
                point(4.0037, 58.0033, "emergency")
            ]
        );
        assert_eq!(access("osm:way:8105"), []);
    }

    fn ranked_cafes() -> Vec<Vec<TestElement>> {
        let cafe = |id: i64, item: &'static str| {
            node(
                id,
                43.65,
                -79.38,
                &[
                    ("name", "Blue Door"),
                    ("amenity", "cafe"),
                    ("wikidata", item),
                ],
            )
        };
        vec![vec![cafe(1, "Q11"), cafe(2, "Q22")], vec![], vec![]]
    }

    #[test]
    fn a_place_whose_wikidata_item_is_read_more_ranks_first() {
        let ranked = NL_PLACES
            .report
            .qrank
            .as_ref()
            .expect("the QRank file read");
        assert_eq!(ranked.items, 2);
        let hits = place_search("Blue Door");
        let ids = hits
            .iter()
            .map(|hit| hit.record.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["osm:node:9002", "osm:node:9001"]);
    }

    fn streets() -> Vec<Vec<TestElement>> {
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
            way(700, &[201, 202], &road("Ponsonby Road")),
            way(701, &[202, 203], &road("Ponsonby Road")),
            way(702, &[203, 204], &road("Ponsonby Road")),
            way(703, &[205, 206], &road("Ponsonby Road")),
            way(710, &[211, 212], &road("Main Street")),
            way(711, &[212, 213], &road("Main Street")),
            way(
                720,
                &[205, 206],
                &[("highway", "platform"), ("name", "Kingston Station")],
            ),
        ]);
        vec![nodes, ways, vec![district, board, town]]
    }

    #[test]
    fn joins_the_ways_of_a_street_and_labels_it_with_its_town() {
        let reader = Reader::open(MERGED_PLACES.pack()).expect("reader");
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
            .filter(|(id, _, _)| id.starts_with("osm:way:67"))
            .collect::<Vec<_>>();
        streets.sort();
        let street = |id: &str, label: &str, kind: &str| {
            (id.to_string(), label.to_string(), kind.to_string())
        };
        assert_eq!(
            streets,
            vec![
                street("osm:way:6700", "Ponsonby Road, Auckland", "MultiLineString"),
                street("osm:way:6703", "Ponsonby Road, Auckland", "LineString"),
                street("osm:way:6710", "Main Street, Kingston", "MultiLineString"),
            ],
            "a street in a local board is named by its district, one in a town by the town"
        );
        let catalogue = crate::ids::Catalogue::open(reader.container())
            .expect("open")
            .expect("a catalogue");
        let found = |way: i64| {
            catalogue
                .find(reader.records(), ObjectType::Way, way)
                .expect("find")
                .into_iter()
                .map(|id| reader.record_summary(id).expect("summary").id)
                .collect::<Vec<_>>()
        };
        for way in [6700, 6701, 6702] {
            assert_eq!(found(way), ["osm:way:6700"], "way {way}");
        }
        assert_eq!(found(6703), ["osm:way:6703"], "the far road stays its own");
        assert_eq!(found(6711), ["osm:way:6710"]);
        assert!(found(6720).is_empty(), "a platform is no street");
    }

    fn communes() -> Vec<Vec<TestElement>> {
        let mut nodes = vec![
            node(1, 2.5, 2.5, &[]),
            node(2, 2.5, 2.501, &[]),
            node(3, 3.5, 3.5, &[]),
            node(4, 3.5, 3.501, &[]),
            node(5, 2.6, 2.6, &[("place", "city"), ("name", "Poitiers")]),
            node(6, 3.6, 3.6, &[("place", "village"), ("name", "Ligugé")]),
        ];
        let mut ways = Vec::new();
        let mut relations = Vec::new();
        type Area<'a> = (&'a str, f64, f64, &'a [(&'a str, &'a str)]);
        let areas: [Area; 8] = [
            ("3", 0.5, 9.5, &[("name", "France métropolitaine")]),
            (
                "4",
                1.0,
                5.0,
                &[("name", "Nouvelle-Aquitaine"), ("ISO3166-2", "FR-NAQ")],
            ),
            (
                "5",
                6.0,
                9.0,
                &[("name", "Collectivité européenne d'Alsace")],
            ),
            ("6", 1.5, 4.5, &[("name", "Vienne"), ("ISO3166-2", "FR-86")]),
            ("7", 2.0, 4.0, &[("name", "Poitiers")]),
            ("8", 2.1, 2.9, &[("name", "Poitiers")]),
            ("8", 3.1, 3.9, &[("name", "Ligugé")]),
            ("8", 6.5, 7.5, &[("name", "Strasbourg")]),
        ];
        for (index, (level, low, high, tags)) in areas.into_iter().enumerate() {
            let id = 100 + 10 * index as i64;
            nodes.extend([
                node(id, low, low, &[]),
                node(id + 1, low, high, &[]),
                node(id + 2, high, high, &[]),
                node(id + 3, high, low, &[]),
            ]);
            ways.push(way(id, &[id, id + 1, id + 2, id + 3, id], &[]));
            let mut tags = tags.to_vec();
            tags.extend([("boundary", "administrative"), ("admin_level", level)]);
            relations.push(relation(id, &[(id, "outer")], &tags));
        }
        let road = [("highway", "residential"), ("name", "Grande Rue")];
        ways.extend([way(700, &[1, 2], &road), way(701, &[3, 4], &road)]);
        vec![nodes, ways, relations]
    }

    #[test]
    fn a_french_street_is_labelled_and_found_by_its_commune_not_its_arrondissement() {
        let reader = ADMIN.pack();
        let mut streets = reader
            .records_json_by_layer("street", 0)
            .expect("streets")
            .into_iter()
            .map(|street| {
                (
                    street["id"].as_str().expect("id").to_string(),
                    street["label"].as_str().expect("label").to_string(),
                )
            })
            .filter(|(id, _)| id.starts_with("osm:way:4007"))
            .collect::<Vec<_>>();
        streets.sort();
        assert_eq!(
            streets,
            [
                (
                    "osm:way:400700".to_string(),
                    "Grande Rue, Poitiers".to_string()
                ),
                (
                    "osm:way:400701".to_string(),
                    "Grande Rue, Ligugé".to_string()
                ),
            ]
        );
        let searcher = TextSearcher::open(ADMIN.built.pack()).expect("searcher");
        let found = searcher
            .search(&TextSearchOptions {
                query: "Grande Rue, Poitiers".to_string(),
                limit: 1,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search")
            .into_iter()
            .map(|hit| hit.record.id)
            .collect::<Vec<_>>();
        assert_eq!(found, ["osm:way:400700"]);
    }

    #[test]
    fn spilling_to_disk_builds_the_same_pack() {
        let dir = temp_dir("spill");
        let input = dir.join("town.osm.pbf");
        write_pbf(&input, &town_with_pois());
        let in_memory = build(&dir.join("memory"), &input, None);
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
        for owner in [
            "dictionary-streets",
            "dictionary-pois",
            "dictionary-words",
            "dictionary-aliases",
        ] {
            let spill = ledger.spills.get(owner);
            assert!(
                spill.is_some_and(|spill| spill.merges > 0),
                "{owner}: {:?}",
                ledger.spills
            );
        }
        let records = &ledger.spills["records"];
        assert_eq!(records.items_read, records.items_written);
        assert!(records.merge_passes >= 1);
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
        let structured = spilled.stages.last().expect("structured index stage");
        assert_eq!(structured.stage, BuildStage::StructuredIndex.description());
        for owner in ["structured-keys", "places-ids"] {
            assert!(
                structured
                    .spills
                    .get(owner)
                    .is_some_and(|spill| spill.merges > 0),
                "{owner}: {:?}",
                structured.spills
            );
        }

        let memory = Reader::open(dir.join("memory")).expect("memory pack");
        let spilled = Reader::open(dir.join("spilled")).expect("spilled pack");
        for (section, _) in crate::structured::SECTIONS {
            let section_of = |pack: &Reader| {
                let bytes = pack.container().raw_section(section).expect(section);
                if section != "streets/meta" {
                    return bytes.as_slice().to_vec();
                }
                let mut meta: serde_json::Value =
                    serde_json::from_slice(bytes.as_slice()).expect("meta");
                meta["segments"].take();
                meta.to_string().into_bytes()
            };
            assert_eq!(section_of(&memory), section_of(&spilled), "{section}");
        }
        let sorted = memory
            .container()
            .sections()
            .keys()
            .filter(|name| name.starts_with("parse/") || *name == crate::ids::SECTION)
            .collect::<Vec<_>>();
        assert!(
            sorted.contains(
                &&crate::parse::lattice::dictionary::Field::Street
                    .section()
                    .to_string()
            )
        );
        assert!(sorted.contains(&&crate::ids::SECTION.to_string()));
        for section in sorted {
            assert_eq!(
                memory
                    .container()
                    .raw_section(section)
                    .expect(section)
                    .as_slice(),
                spilled
                    .container()
                    .raw_section(section)
                    .expect(section)
                    .as_slice(),
                "{section}"
            );
        }
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
        let refused = build_osm_pack(&options);
        let Err(Error::OutOfResources(message)) = refused else {
            panic!("expected the plan to refuse the scratch budget, got {refused:?}");
        };
        assert!(message.contains("--scratch-budget-mb"), "{message}");
        let failed = build_osm_pack(&BuildOsmOptions {
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
        let error = build_osm_pack(&BuildOsmOptions {
            inputs: vec![input],
            pack: dir.join("pack"),
            ..BuildOsmOptions::default()
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("osmium sort"), "{error:#}");
        assert!(!dir.join("pack").join("CURRENT").exists());
    }

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
        let report = &TOWN.report;
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

        let events = &TOWN.events;
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
            Some(fs::metadata(&TOWN.inputs[0]).expect("input").len()),
            "the scan is measured in bytes of the input"
        );
    }

    #[test]
    fn a_cancelled_build_publishes_nothing() {
        let dir = temp_dir("cancelled");
        let input = dir.join("town.osm.pbf");
        write_pbf(&input, &town());
        let error = build_osm_pack(&BuildOsmOptions {
            inputs: vec![input],
            pack: dir.join("pack"),
            cancel: Arc::new(AtomicBool::new(true)),
            ..BuildOsmOptions::default()
        })
        .unwrap_err();
        assert!(matches!(error, Error::Cancelled));
        assert!(!dir.join("pack").join("CURRENT").exists());
    }

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
                entry: None,
                area: None,
            },
            addresses::AddressFile {
                dataset: "openaddresses:second".to_string(),
                path: second,
                entry: None,
                area: None,
            },
        ]
    }

    fn pack_contents(
        root: &Path,
        inputs: &[&Path],
        files: Vec<addresses::AddressFile>,
        budget: Option<usize>,
    ) -> (Vec<String>, BuilderReport) {
        let report = build_osm_pack(&BuildOsmOptions {
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
        (contents, report)
    }

    #[test]
    fn imported_rows_repeat_dispute_and_lend_by_their_place_in_the_address_book() {
        let dir = temp_dir("address-book");
        let input = dir.join("flats.osm.pbf");
        write_pbf(&input, &flats());
        let files = flat_rows(&dir);
        let (in_memory, report) =
            pack_contents(&dir.join("memory"), &[&input], files.clone(), None);
        let (spilled, _) = pack_contents(&dir.join("spilled"), &[&input], files, Some(4));
        assert_eq!(in_memory, spilled);
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

        let reader = Reader::open(dir.join("memory")).expect("reader");
        let address = |id: &str| {
            let record_id = reader.find_by_source_id(id).expect("lookup")?;
            match reader.records().record(record_id).expect("record") {
                Record::Address(address) => Some(address),
                other => panic!("{id} is an address: {other:?}"),
            }
        };
        for dropped in [
            "openaddresses:first:row:1",
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
    }

    #[test]
    fn a_pack_lists_the_areas_a_record_lies_in_with_their_raw_tags() {
        let dir = temp_dir("enclosing");
        let input = dir.join("enclosing.osm.pbf");
        let mut blocks = town();
        let corners = [
            (43.55, -79.55),
            (43.55, -79.25),
            (43.8, -79.25),
            (43.8, -79.55),
        ];
        let nodes = (2001..)
            .zip(corners)
            .map(|(id, (lat, lon))| node(id, lat, lon, &[]));
        blocks[1].extend(nodes);
        let south = [("addr:housenumber", "7"), ("addr:street", "King Street")];
        blocks[1].push(node(2010, 43.52, -79.4, &south));
        let village = [("place", "village"), ("name", "Lonely")];
        blocks[1].push(node(2011, 10.0, 10.0, &village));
        let city = [
            ("place", "city"),
            ("boundary", "administrative"),
            ("name", "Metro"),
        ];
        blocks[2].push(way(700, &[2001, 2002, 2003, 2004, 2001], &city));
        let hook = [
            (43.85, -79.95),
            (43.85, -79.75),
            (43.87, -79.75),
            (43.87, -79.93),
        ];
        let hook = [
            &hook[..],
            &[
                (43.93, -79.93),
                (43.93, -79.75),
                (43.95, -79.75),
                (43.95, -79.95),
            ],
        ]
        .concat();
        let pocket = [
            (43.871, -79.929),
            (43.871, -79.751),
            (43.929, -79.751),
            (43.929, -79.929),
        ];
        for (way_id, first, level, name, ring) in [
            (710, 3000, "9", "Hook", &hook[..]),
            (711, 3100, "10", "Pocket", &pocket[..]),
        ] {
            let ids = (first..).take(ring.len()).collect::<Vec<_>>();
            blocks[1].extend(
                ids.iter()
                    .zip(ring)
                    .map(|(id, (lat, lon))| node(*id, *lat, *lon, &[])),
            );
            let tags = [
                ("boundary", "administrative"),
                ("admin_level", level),
                ("name", name),
            ];
            blocks[2].push(way(way_id, &[&ids[..], &ids[..1]].concat(), &tags));
        }
        write_pbf(&input, &blocks);
        build(&dir.join("pack"), &input, None);
        let pack = Reader::open(dir.join("pack")).expect("reader");

        let id = |source: &str| pack.find_by_source_id(source).unwrap().expect(source);
        let [hook, pocket] = ["way:710", "way:711"].map(|source| id(&format!("osm:{source}")));
        let [toronto, metro, ontario] =
            ["relation:900", "way:700", "relation:901"].map(|source| id(&format!("osm:{source}")));
        let (king, south, bay) = (id("osm:node:1"), id("osm:node:2010"), id("osm:way:602"));
        let (ward, ward_relation) = (id("osm:way:605"), id("osm:relation:902"));
        let context = pack.boundary_context(king).unwrap().expect("context");
        assert_eq!(context.admin_context.locality_record_id(), Some(metro));
        assert_eq!(
            context.admin_context.get(PlaceLayer::SubLocality),
            Some(toronto)
        );
        let canada = context.admin_context.country_record_id().expect("country");

        let lists = [
            (king, vec![toronto, metro, ontario, canada]),
            (south, vec![toronto, ontario, canada]),
            (
                bay,
                vec![ward_relation, ward, toronto, metro, ontario, canada],
            ),
            (toronto, vec![metro, ontario, canada]),
            (metro, vec![ontario, canada]),
            (canada, vec![]),
            (hook, vec![ontario, canada]),
            (pocket, vec![ontario, canada]),
        ];
        for (record, list) in lists {
            assert_eq!(pack.enclosing_areas(record).unwrap(), list, "{record}");
        }
        let dictionaries = Dictionaries::open(pack.container()).expect("dictionaries");
        assert_eq!(dictionaries.areas("metro"), [metro as u32]);
        assert_eq!(dictionaries.areas("lonely"), [id("osm:node:2011") as u32]);
        let admin = Some("administrative");
        for (area, tags) in [
            (toronto, Some(AreaTags::of(None, admin, Some("8")))),
            (metro, Some(AreaTags::of(Some("city"), admin, None))),
            (canada, None),
        ] {
            assert_eq!(pack.area_tags(area).unwrap(), tags, "{area}");
            if let Some(context) = pack.boundary_context(area).unwrap() {
                let enclosing = pack.enclosing_areas(area).unwrap();
                assert!(
                    context
                        .admin_context
                        .parent_record_ids()
                        .all(|parent| enclosing.contains(&parent))
                );
            }
        }
    }

    struct Map {
        nodes: Vec<TestElement>,
        ways: Vec<TestElement>,
        relations: Vec<TestElement>,
        next: i64,
        offset: (f64, f64),
        kept: HashMap<&'static str, i64>,
    }

    impl Map {
        fn new() -> Self {
            Self {
                nodes: Vec::new(),
                ways: Vec::new(),
                relations: Vec::new(),
                next: 1,
                offset: (0.0, 0.0),
                kept: HashMap::new(),
            }
        }

        fn id(&mut self) -> i64 {
            self.next += 1;
            self.next
        }

        fn at(&mut self, north: f64, east: f64) {
            self.offset = (north, east);
        }

        fn keep(&mut self, name: &'static str, id: i64) {
            assert!(self.kept.insert(name, id).is_none(), "{name}");
        }

        fn point(&mut self, lat: f64, lon: f64, tags: &[(&'static str, &'static str)]) -> i64 {
            let id = self.id();
            let (north, east) = self.offset;
            self.nodes.push(node(id, lat + north, lon + east, tags));
            id
        }

        fn area(
            &mut self,
            [south, west, north, east]: [f64; 4],
            tags: &[(&'static str, &'static str)],
        ) -> Vec<i64> {
            let ring = [(south, west), (south, east), (north, east), (north, west)]
                .map(|(lat, lon)| self.point(lat, lon, &[]));
            let ring = [&ring[..], &ring[..1]].concat();
            self.over(&ring, tags);
            ring
        }

        fn relation(&mut self, id: i64, bounds: [f64; 4], tags: &[(&'static str, &'static str)]) {
            self.area(bounds, &[]);
            let TestElement::Way { id: outer, .. } = self.ways.last().expect("way") else {
                unreachable!("an area is a way");
            };
            self.relations
                .push(relation(id, &[(*outer, "outer")], tags));
        }

        fn over(&mut self, ring: &[i64], tags: &[(&'static str, &'static str)]) {
            let id = self.id();
            self.ways.push(way(id, ring, tags));
        }

        fn blocks(self) -> Vec<Vec<TestElement>> {
            vec![self.nodes, self.ways, self.relations]
        }

        fn build(self, name: &str) -> (TempDir, Reader) {
            let dir = TempDir::new(name);
            let input = dir.join("map.osm.pbf");
            write_pbf(&input, &self.blocks());
            build(&dir.join("pack"), &input, None);
            let pack = Reader::open(dir.join("pack")).expect("reader");
            (dir, pack)
        }
    }

    struct Admin {
        built: Built,
        kept: HashMap<&'static str, i64>,
    }

    impl Admin {
        fn pack(&self) -> Reader {
            Reader::open(self.built.pack()).expect("reader")
        }

        fn tiers(&self, kept: &str) -> Vec<(PlaceLayer, String)> {
            tiers(&self.pack(), self.kept[kept])
        }
    }

    static ADMIN: LazyLock<Admin> = LazyLock::new(|| {
        let mut map = Map::new();
        map.area([50.2, -2.0, 53.0, 1.0], &country("United Kingdom", "GB"));
        map.area([50.5, -1.5, 52.5, 0.5], &admin("4", "England"));
        town_dot(&mut map);
        villages(&mut map);
        inferred_country(&mut map);
        let [boundary, level, name, code] = country("Deutschland", "DE");
        map.area(
            [44.0, 9.0, 50.0, 30.0],
            &[boundary, level, name, code, ("default_language", "de")],
        );
        city_district(&mut map);
        map.at(0.0, 6.0);
        state_hamlet(&mut map);
        map.at(0.0, 12.0);
        state_hamlet_city_area(&mut map);
        map.at(0.0, 0.0);
        plate_codes(&mut map);
        map.area([40.0, -75.0, 42.0, -73.0], &country("United States", "US"));
        borough(&mut map);
        map.area([54.0, 36.0, 57.0, 45.0], &country("Россия", "RU"));
        district(&mut map);
        map.at(0.0, 4.0);
        district_centre(&mut map);
        map.at(0.0, 0.0);
        map.area([40.0, -17.5, 50.1, 8.0], &country("France", "FR"));
        tiered_town(&mut map);
        map.area([-47.5, 166.0, -40.0, 174.0], &country("New Zealand", "NZ"));
        nickname(&mut map);
        seat(&mut map);
        map.area(
            [50.2, 2.5, 51.5, 6.4],
            &country("België / Belgique / Belgien", "BE"),
        );
        inside_tiered(&mut map);
        let kept = std::mem::take(&mut map.kept);
        let built = Built::new(
            "admin-cells",
            one_extract(vec![
                ("map", map.blocks()),
                ("ward", renumbered(ward(), 100_000)),
                ("honshu", renumbered(honshu(), 200_000)),
                ("slash", renumbered(slash_numbers(), 300_000)),
                (
                    "communes",
                    moved(renumbered(communes(), 400_000), 40.5, -17.0),
                ),
            ]),
        );
        Admin { built, kept }
    });

    fn admin(level: &'static str, name: &'static str) -> [(&'static str, &'static str); 3] {
        [
            ("boundary", "administrative"),
            ("admin_level", level),
            ("name", name),
        ]
    }

    fn country(name: &'static str, code: &'static str) -> [(&'static str, &'static str); 4] {
        let [boundary, level, name] = admin("2", name);
        [boundary, level, name, ("ISO3166-1:alpha2", code)]
    }

    fn tiers(pack: &Reader, node: i64) -> Vec<(PlaceLayer, String)> {
        let record = pack
            .find_by_source_id(&format!("osm:node:{node}"))
            .unwrap()
            .expect("record");
        let context = pack.boundary_context(record).unwrap().expect("context");
        PlaceLayer::ALL
            .into_iter()
            .filter_map(|tier| {
                let area = context.admin_context.get(tier)?;
                Some((tier, pack.context_record(area).unwrap()?.name))
            })
            .collect()
    }

    fn named(tiers: &[(PlaceLayer, &str)]) -> Vec<(PlaceLayer, String)> {
        tiers
            .iter()
            .map(|(tier, name)| (*tier, name.to_string()))
            .collect()
    }

    fn house(map: &mut Map, lat: f64, lon: f64) -> i64 {
        map.point(
            lat,
            lon,
            &[("addr:housenumber", "10"), ("addr:street", "High Street")],
        )
    }

    fn town_dot(map: &mut Map) {
        map.area([51.3, -0.5, 51.7, 0.3], &admin("5", "Greater London"));
        map.area(
            [51.49, -0.16, 51.52, -0.11],
            &admin("8", "City of Westminster"),
        );
        map.point(
            51.497,
            -0.137,
            &[("place", "city"), ("name", "City of Westminster")],
        );
        map.relation(
            51800,
            [51.51, -0.11, 51.52, -0.08],
            &admin("6", "City of London"),
        );
        map.point(
            51.507,
            -0.128,
            &[
                ("place", "city"),
                ("name", "London"),
                ("name:de", "London"),
                ("name:fr", "Londres"),
                ("name:ru", "Лондон"),
                ("wikidata", "Q84"),
            ],
        );
        map.point(
            51.536,
            -0.103,
            &[
                ("addr:housenumber", "1"),
                ("addr:street", "Upper Street"),
                ("addr:city", "London"),
            ],
        );
        let downing = house(map, 51.5034, -0.1276);
        map.keep("downing", downing);
    }

    #[test]
    fn a_town_point_is_the_locality_and_a_city_tagged_borough_yields_to_its_city() {
        assert_eq!(
            ADMIN.tiers("downing"),
            named(&[
                (PlaceLayer::Country, "United Kingdom"),
                (PlaceLayer::Region, "England"),
                (PlaceLayer::District, "Greater London"),
                (PlaceLayer::Locality, "London"),
                (PlaceLayer::SubLocality, "City of Westminster"),
            ])
        );
    }

    fn villages(map: &mut Map) {
        map.point(51.0, -1.0, &[("place", "village"), ("name", "Ashby")]);
        map.point(
            51.0,
            -0.98,
            &[
                ("place", "village"),
                ("name", "Bourne"),
                ("name:de", "Bourne"),
                ("wikidata", "Q1"),
            ],
        );
        let farm = house(map, 51.0005, -0.9995);
        map.keep("ashby farm", farm);
    }

    #[test]
    fn a_house_by_a_village_point_is_in_that_village_over_a_better_known_one() {
        assert_eq!(
            ADMIN.tiers("ashby farm"),
            named(&[
                (PlaceLayer::Country, "United Kingdom"),
                (PlaceLayer::Region, "England"),
                (PlaceLayer::Locality, "Ashby"),
            ])
        );
    }

    fn inferred_country(map: &mut Map) {
        map.area(
            [42.5, -85.0, 49.0, -76.0],
            &[
                ("boundary", "administrative"),
                ("admin_level", "4"),
                ("name", "Ontario"),
                ("ISO3166-2", "CA-ON"),
            ],
        );
        map.point(45.0, -80.0, &[("place", "village"), ("name", "Elmvale")]);
        map.point(45.0, -79.9, &[("place", "village"), ("name", "Minesing")]);
        let farm = house(map, 45.0005, -80.0005);
        map.keep("elmvale farm", farm);
    }

    #[test]
    fn a_village_point_names_a_house_in_a_region_with_no_country_boundary() {
        let tiers = ADMIN.tiers("elmvale farm");
        assert!(
            tiers.contains(&(PlaceLayer::Locality, "Elmvale".to_string())),
            "{tiers:?}"
        );
    }

    #[test]
    fn a_village_point_names_a_house_in_an_extract_with_no_boundary() {
        let mut map = Map::new();
        map.point(51.0, -1.0, &[("place", "village"), ("name", "Ashby")]);
        map.point(51.0, -0.9, &[("place", "village"), ("name", "Bourne")]);
        let farm = house(&mut map, 51.0005, -0.9995);
        let (_dir, pack) = map.build("town-no-boundary");
        assert_eq!(
            tiers(&pack, farm),
            named(&[(PlaceLayer::Locality, "Ashby")])
        );
    }

    fn city_district(map: &mut Map) {
        map.area([47.5, 10.0, 49.5, 12.5], &admin("4", "Bayern"));
        map.area([47.8, 11.0, 48.5, 12.3], &admin("5", "Oberbayern"));
        map.area([48.06, 11.36, 48.25, 11.72], &admin("6", "München"));
        map.area([48.13, 11.57, 48.15, 11.6], &admin("9", "Altstadt-Lehel"));
        map.point(48.137, 11.575, &[("place", "city"), ("name", "München")]);
        map.area(
            [48.1365, 11.574, 48.1372, 11.5752],
            &admin("11", "Stadt München"),
        );
        let marienplatz = house(map, 48.1374, 11.5755);
        map.keep("marienplatz", marienplatz);
        let ends = [11.57, 11.574].map(|lon| map.point(48.139, lon, &[]));
        let street = map.id();
        map.ways.push(way(
            street,
            &ends,
            &[("highway", "residential"), ("name", "Kaufingerstraße")],
        ));
        map.keep("kaufingerstraße", street);
    }

    #[test]
    fn a_city_boundary_above_the_locality_tier_is_the_locality_over_its_boroughs() {
        let pack = ADMIN.pack();
        let street = pack
            .find_by_source_id(&format!("osm:way:{}", ADMIN.kept["kaufingerstraße"]))
            .unwrap()
            .expect("street");
        assert_eq!(
            pack.record_json(street).unwrap()["locality"],
            "München",
            "a street is named by its town"
        );
        assert_eq!(
            ADMIN.tiers("marienplatz"),
            named(&[
                (PlaceLayer::Country, "Deutschland"),
                (PlaceLayer::Region, "Bayern"),
                (PlaceLayer::SubRegion, "Oberbayern"),
                (PlaceLayer::Locality, "München"),
                (PlaceLayer::SubLocality, "Altstadt-Lehel"),
            ])
        );
    }

    fn borough(map: &mut Map) {
        map.area([40.2, -74.8, 41.8, -73.2], &admin("4", "New York"));
        map.area(
            [40.5, -74.3, 40.95, -73.7],
            &[("place", "city"), ("name", "City of New York")],
        );
        let ring = map.area([40.57, -74.04, 40.74, -73.83], &admin("6", "Kings County"));
        map.over(&ring, &admin("7", "Brooklyn"));
        let parkway = house(map, 40.6712, -73.9636);
        map.keep("parkway", parkway);
    }

    #[test]
    fn a_borough_and_its_county_inside_a_city_keep_the_borough_tier_and_the_county_area() {
        assert_eq!(
            ADMIN.tiers("parkway"),
            named(&[
                (PlaceLayer::Country, "United States"),
                (PlaceLayer::Region, "New York"),
                (PlaceLayer::Locality, "City of New York"),
                (PlaceLayer::SubLocality, "Brooklyn"),
            ])
        );
        let pack = ADMIN.pack();
        let record = pack
            .find_by_source_id(&format!("osm:node:{}", ADMIN.kept["parkway"]))
            .unwrap()
            .expect("record");
        let names = pack
            .enclosing_areas(record)
            .unwrap()
            .into_iter()
            .map(|area| pack.context_record(area).unwrap().expect("area").name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"Kings County".to_string()), "{names:?}");
    }

    fn tier_names(kept: &str) -> impl Fn(PlaceLayer) -> Option<String> {
        let context = ADMIN.tiers(kept);
        move |tier| {
            context
                .iter()
                .find(|(held, _)| *held == tier)
                .map(|(_, name)| name.clone())
        }
    }

    fn district(map: &mut Map) {
        map.area([55.4, 37.1, 56.0, 38.0], &admin("4", "Москва"));
        map.area(
            [55.57, 37.3, 55.92, 37.9],
            &[("place", "city"), ("name", "Москва")],
        );
        map.area(
            [55.7, 37.7, 55.85, 37.85],
            &admin("5", "Восточный административный округ"),
        );
        map.area([55.78, 37.75, 55.8, 37.8], &admin("8", "район Измайлово"));
        let boulevard = house(map, 55.795, 37.79);
        map.keep("boulevard", boulevard);
    }

    #[test]
    fn a_district_inside_a_city_area_lies_below_the_city_under_its_okrug() {
        let at = tier_names("boulevard");
        assert_eq!(at(PlaceLayer::Region).as_deref(), Some("Москва"));
        assert_eq!(at(PlaceLayer::Locality).as_deref(), Some("Москва"));
        assert_eq!(
            at(PlaceLayer::SubLocality).as_deref(),
            Some("Восточный административный округ")
        );
        assert_eq!(
            at(PlaceLayer::Neighbourhood).as_deref(),
            Some("район Измайлово")
        );
    }

    fn district_centre(map: &mut Map) {
        map.area([55.4, 37.1, 56.0, 38.0], &admin("4", "Татарстан"));
        map.area(
            [55.57, 37.3, 55.92, 37.9],
            &[("place", "city"), ("name", "Казань")],
        );
        map.area(
            [55.7, 37.7, 55.85, 37.85],
            &admin("5", "Западный административный округ"),
        );
        map.area(
            [55.76, 37.76, 55.8, 37.8],
            &admin("8", "район Соколиная Гора"),
        );
        let house = house(map, 55.775, 37.775);
        map.keep("okrug centre", house);
    }

    #[test]
    fn a_district_holding_its_okrugs_inner_point_still_lies_inside_it() {
        let at = tier_names("okrug centre");
        let context = ADMIN.tiers("okrug centre");
        assert_eq!(
            at(PlaceLayer::Locality).as_deref(),
            Some("Казань"),
            "{context:?}"
        );
        assert_eq!(
            at(PlaceLayer::SubLocality).as_deref(),
            Some("Западный административный округ"),
            "{context:?}"
        );
        assert_eq!(
            at(PlaceLayer::Neighbourhood).as_deref(),
            Some("район Соколиная Гора"),
            "{context:?}"
        );
    }

    fn tiered_town(map: &mut Map) {
        map.area([48.1, 1.4, 49.2, 3.6], &admin("4", "Île-de-France"));
        map.area([48.81, 2.22, 48.91, 2.47], &admin("6", "Paris"));
        map.area([48.815, 2.224, 48.902, 2.469], &admin("8", "Paris"));
        map.point(48.8567, 2.3508, &[("place", "city"), ("name", "Paris")]);
        map.area(
            [48.85, 2.35, 48.862, 2.369],
            &admin("9", "Paris 4e Arrondissement"),
        );
        let rivoli = house(map, 48.8553, 2.3601);
        map.keep("rivoli", rivoli);
        map.area([47.85, -1.7, 47.9, -1.62], &admin("8", "Pancé"));
        map.point(47.88, -1.66, &[("place", "hamlet"), ("name", "La Touche")]);
        let farm = house(map, 47.8818, -1.6613);
        map.keep("pancé farm", farm);
    }

    #[test]
    fn a_town_the_tiers_name_keeps_its_context_over_a_hamlet_point_inside() {
        assert_eq!(
            ADMIN.tiers("rivoli"),
            named(&[
                (PlaceLayer::Country, "France"),
                (PlaceLayer::Region, "Île-de-France"),
                (PlaceLayer::District, "Paris"),
                (PlaceLayer::Locality, "Paris"),
                (PlaceLayer::Neighbourhood, "Paris 4e Arrondissement"),
            ])
        );
        assert_eq!(
            ADMIN.tiers("pancé farm"),
            named(&[
                (PlaceLayer::Country, "France"),
                (PlaceLayer::Locality, "Pancé"),
            ])
        );
    }

    fn nickname(map: &mut Map) {
        map.area([-46.8, 169.0, -45.9, 170.3], &admin("4", "Otago"));
        map.area([-46.7, 169.2, -46.0, 170.2], &admin("6", "Clutha District"));
        map.point(
            -46.2378,
            169.7399,
            &[
                ("place", "town"),
                ("name", "Balclutha"),
                ("loc_name", "Clutha"),
            ],
        );
        map.point(
            -46.30,
            169.70,
            &[("place", "village"), ("name", "Stirling")],
        );
        let road = house(map, -46.239, 169.742);
        map.keep("balclutha road", road);
    }

    #[test]
    fn a_district_named_like_a_town_nickname_is_no_town_and_the_town_point_stays() {
        let context = ADMIN.tiers("balclutha road");
        assert!(
            context.contains(&(PlaceLayer::Locality, "Balclutha".to_string())),
            "{context:?}"
        );
        assert!(
            !context.contains(&(PlaceLayer::Locality, "Clutha District".to_string())),
            "{context:?}"
        );
    }

    fn seat(map: &mut Map) {
        map.area([-44.6, 170.6, -43.9, 171.5], &admin("4", "Canterbury"));
        map.area([-44.5, 170.8, -44.0, 171.4], &admin("6", "Timaru District"));
        map.point(-44.396, 171.254, &[("place", "town"), ("name", "Timaru")]);
        let temuka = map.point(-44.244, 171.278, &[("place", "town"), ("name", "Temuka")]);
        map.keep("temuka", temuka);
        let seadown = map.point(-44.33, 171.25, &[("place", "village"), ("name", "Seadown")]);
        map.keep("seadown", seadown);
        map.point(
            -44.33,
            171.2,
            &[
                ("addr:housenumber", "1"),
                ("addr:street", "Main North Road"),
                ("addr:city", "Timaru"),
            ],
        );
        let near = house(map, -44.397, 171.255);
        map.keep("timaru house", near);
    }

    #[test]
    fn a_district_named_for_its_seat_is_no_town_and_town_points_never_nest() {
        assert_eq!(
            ADMIN.tiers("timaru house"),
            named(&[
                (PlaceLayer::Country, "New Zealand"),
                (PlaceLayer::Region, "Canterbury"),
                (PlaceLayer::District, "Timaru District"),
                (PlaceLayer::Locality, "Timaru"),
            ])
        );
        for town in ["temuka", "seadown"] {
            let context = ADMIN.tiers(town);
            assert!(
                !context
                    .iter()
                    .any(|(tier, _)| *tier == PlaceLayer::Locality),
                "{context:?}"
            );
        }
    }

    fn state_hamlet(map: &mut Map) {
        map.area([47.5, 10.0, 49.5, 12.9], &admin("4", "Hessen"));
        map.area([47.7, 12.4, 47.9, 12.7], &admin("8", "Ruhpolding"));
        map.point(47.794, 12.534, &[("place", "hamlet"), ("name", "Hessen")]);
        map.point(
            47.761,
            12.645,
            &[("place", "village"), ("name", "Ruhpolding")],
        );
        map.point(
            48.137,
            11.575,
            &[("place", "city"), ("name", "Frankfurt am Main")],
        );
        let farm = house(map, 47.795, 12.535);
        map.keep("ruhpolding farm", farm);
        let outside = house(map, 48.5, 11.0);
        map.keep("hessen house", outside);
    }

    #[test]
    fn a_state_named_like_a_hamlet_in_it_is_no_town() {
        assert_eq!(
            ADMIN.tiers("ruhpolding farm"),
            named(&[
                (PlaceLayer::Country, "Deutschland"),
                (PlaceLayer::Region, "Hessen"),
                (PlaceLayer::Locality, "Ruhpolding"),
            ])
        );
        let context = ADMIN.tiers("hessen house");
        assert!(
            !context.contains(&(PlaceLayer::Locality, "Hessen".to_string())),
            "{context:?}"
        );
    }

    fn state_hamlet_city_area(map: &mut Map) {
        map.area([47.5, 10.0, 49.5, 12.9], &admin("4", "Sachsen"));
        map.area([48.0, 11.4, 48.3, 11.7], &admin("8", "Dresden"));
        map.point(47.794, 12.534, &[("place", "hamlet"), ("name", "Sachsen")]);
        map.point(48.137, 11.575, &[("place", "city"), ("name", "Dresden")]);
        let _bounds_the_hamlets_reach =
            map.point(47.794, 13.0, &[("place", "village"), ("name", "Görlitz")]);
        let outside = house(map, 48.5, 11.0);
        map.keep("sachsen house", outside);
    }

    #[test]
    fn a_state_named_like_a_hamlet_in_it_is_no_town_with_the_city_an_area() {
        assert_eq!(
            ADMIN.tiers("sachsen house"),
            named(&[
                (PlaceLayer::Country, "Deutschland"),
                (PlaceLayer::Region, "Sachsen"),
            ])
        );
    }

    fn inside_tiered(map: &mut Map) {
        map.area([50.6, 2.5, 51.4, 5.9], &admin("4", "Vlaanderen"));
        map.area([50.73, 3.43, 50.8, 3.58], &admin("8", "Kluisbergen"));
        map.area([50.73, 3.47, 50.8, 3.58], &admin("9", "Kwaremont"));
        map.point(
            50.755,
            3.525,
            &[("place", "village"), ("name", "Kwaremont")],
        );
        let kraaistraat = house(map, 50.756, 3.526);
        map.keep("kraaistraat", kraaistraat);
    }

    #[test]
    fn a_village_inside_the_tiered_town_stays_below_it() {
        assert_eq!(
            ADMIN.tiers("kraaistraat"),
            named(&[
                (PlaceLayer::Country, "België / Belgique / Belgien"),
                (PlaceLayer::Region, "Vlaanderen"),
                (PlaceLayer::Locality, "Kluisbergen"),
                (PlaceLayer::SubLocality, "Kwaremont"),
            ])
        );
    }

    fn plate_codes(map: &mut Map) {
        let [boundary, level, name] = admin("6", "Rhein-Hunsrück-Kreis");
        map.relation(
            61_000,
            [44.5, 26.0, 46.0, 28.0],
            &[boundary, level, name, ("license_plate_code", "SIM")],
        );
        map.relation(
            61_010,
            [46.5, 26.0, 48.0, 28.0],
            &admin("6", "Kreis ohne Kennzeichen"),
        );
    }

    #[test]
    fn a_boundary_keeps_its_plate_code_and_default_language() {
        let pack = ADMIN.pack();
        let kept = |id: &str| {
            let record = pack.find_by_source_id(id).expect("lookup").expect(id);
            (
                pack.plate_code(record).map(str::to_string),
                pack.default_languages(record).collect::<Vec<_>>().join(";"),
            )
        };
        assert_eq!(
            kept("osm:relation:61000"),
            (Some("SIM".to_string()), String::new())
        );
        assert_eq!(kept("osm:relation:61010"), (None, String::new()));
        let germany = (0..pack.records().record_count())
            .find(|record| pack.record_summary(*record).expect("summary").name == "Deutschland")
            .expect("Deutschland");
        assert_eq!(pack.plate_code(germany), None);
        assert_eq!(pack.default_languages(germany).collect::<Vec<_>>(), ["de"]);
    }

    fn records_and_contexts(pack: &Path) -> Vec<(serde_json::Value, Option<RecordContext>)> {
        let reader = Reader::open(pack).expect("reader");
        (0..reader.manifest().record_count)
            .map(|id| {
                let record = serde_json::json!({
                    "record": reader.record_json(id).expect("record"),
                    "enclosing": reader.enclosing_areas(id).expect("areas"),
                    "tags": reader.area_tags(id).expect("tags"),
                });
                (record, reader.boundary_context(id).expect("context"))
            })
            .collect()
    }

    fn resumable_inputs(dir: &Path) -> BuildOsmOptions {
        let (first, second) = (dir.join("town.osm.pbf"), dir.join("other.osm.pbf"));
        let mut town = town_with_pois();
        let [nodes, ways, relations] = renumbered(buildings_with_entrances(), 3000)
            .try_into()
            .unwrap_or_else(|_| panic!("nodes, ways and relations"));
        town[2].extend(nodes);
        town[3].extend(ways);
        town[4].extend(relations);
        write_pbf(&first, &town);
        write_pbf(&second, &other_country());
        let csv = dir.join("addresses.csv");
        fs::write(
            &csv,
            "LON,LAT,NUMBER,STREET,CITY,POSTCODE\n\
             -79.40005,43.60003,10,KING ST,Toronto,M5V 1A1\n\
             -79.4003,43.6003,16,King Street,Toronto,M5V 1A1\n\
             -79.40,43.604,10,King Street,Toronto,M5V 1A1\n\
             -79.45,43.65,3,QUEEN STREET,Toronto,M5V 2B1\n\
             -79.45,43.65,5,Queen Street,Toronto,\n",
        )
        .expect("csv");
        BuildOsmOptions {
            inputs: vec![first, second],
            addresses: Some(Arc::new(addresses::AddressFiles(vec![
                addresses::AddressFile {
                    dataset: "openaddresses:test".to_string(),
                    path: csv,
                    entry: None,
                    area: None,
                },
            ]))),
            ..BuildOsmOptions::default()
        }
    }

    fn fail_before(options: &BuildOsmOptions, step: Step) -> Error {
        FAIL_BEFORE_CHECKPOINT.set(Some(step));
        let failed = build_osm_pack(options);
        FAIL_BEFORE_CHECKPOINT.set(None);
        failed.expect_err("the injected failure stops the build")
    }

    fn copy_stopped_build_and_repoint_its_scratch(pack: &Path, copy: &Path) {
        fn copy_tree(from: &Path, to: &Path) {
            fs::create_dir_all(to).expect("create the copy");
            for entry in fs::read_dir(from).expect("read the stopped build") {
                let entry = entry.expect("entry");
                let target = to.join(entry.file_name());
                if entry.file_type().expect("type").is_dir() {
                    copy_tree(&entry.path(), &target);
                } else {
                    fs::copy(entry.path(), &target).expect("copy");
                }
            }
        }
        copy_tree(pack, copy);
        for found in resume::find(copy).expect("find") {
            let scratch = found
                .scratch
                .strip_prefix(pack)
                .expect("scratch in the Pack");
            fs::write(
                found.generation.join(SCRATCH_POINTER_FILE),
                copy.join(scratch).display().to_string(),
            )
            .expect("point at the copy's scratch");
        }
    }

    fn resumed_after_any_step_makes_the_same_pack(name: &str, budget: Option<usize>) {
        let dir = TempDir::new(name);
        let steps = Step::all(2);
        let after = |step: &Step| step.describe(2);
        let options = BuildOsmOptions {
            memory_budget_bytes: budget,
            force: budget.is_some(),
            ..resumable_inputs(&dir)
        };
        let run = dir.join("run");
        let whole = build_osm_pack(&BuildOsmOptions {
            pack: run.join("whole"),
            ..options.clone()
        })
        .expect("uninterrupted build");
        assert_eq!(whole.checkpoints.count, steps.len() as u64);
        assert!(whole.resumes.is_empty());
        let expected = records_and_contexts(&run.join("whole"));
        let with_access = expected
            .iter()
            .filter(|(record, _)| record["record"].get("access").is_some())
            .count();
        assert_eq!(with_access, 5, "the entrances are in the Pack");

        let pack = run.join("stopped");
        let options = BuildOsmOptions {
            pack: pack.clone(),
            leftover: Leftover::Resume,
            ..options
        };
        let node_location_and_mid_seal_steps = [
            steps
                .iter()
                .position(|step| {
                    *step
                        == Step::Input {
                            index: 0,
                            pass: Pass::Ways,
                        }
                })
                .expect("the first input's ways"),
            steps
                .iter()
                .position(|step| *step == Step::Seal(2))
                .expect("a mid-seal part"),
        ];
        let mut copies = Vec::new();
        for (at, step) in steps.iter().enumerate() {
            let error = fail_before(&options, *step);
            assert!(format!("{error}").contains("injected"), "{error}");
            let found = resume::find(&pack).expect("find");
            if at == 0 {
                assert!(found.is_empty(), "nothing is left before a checkpoint");
            } else {
                assert_eq!(found.len(), 1, "{step:?}");
                assert_eq!(found[0].stage, Some(after(&steps[at - 1])));
                assert!(found[0].bytes > 0);
                assert!(!pack.join("CURRENT").exists());
            }
            if budget.is_some() && node_location_and_mid_seal_steps.contains(&at) {
                let copy = run.join(format!("stopped-{at}"));
                copy_stopped_build_and_repoint_its_scratch(&pack, &copy);
                copies.push((at, copy));
            }
        }
        FAIL_BEFORE_TRAINING.set(true);
        let failed = build_osm_pack(&options);
        FAIL_BEFORE_TRAINING.set(false);
        assert!(
            matches!(
                &failed,
                Err(Error::Inconsistent(message)) if message == "injected failure before the training"
            ),
            "{:?}",
            failed.map(|_| ())
        );
        let chained = build_osm_pack(&options).expect("resumed before the training");
        assert_eq!(
            chained
                .resumes
                .iter()
                .map(|resume| resume.after.as_str())
                .collect::<Vec<_>>(),
            steps.iter().map(after).collect::<Vec<_>>(),
            "every checkpoint restored once"
        );
        assert_eq!(records_and_contexts(&pack), expected);
        assert_eq!(chained.checkpoints.count, whole.checkpoints.count);
        assert_eq!(
            chained.parser_training.trained,
            whole.parser_training.trained
        );
        assert_eq!(chained.accepted, whole.accepted);
        assert_eq!(chained.imports, whole.imports);
        assert_eq!(chained.output.record_count, whole.output.record_count);
        assert_eq!(chained.output.rejection_count, whole.output.rejection_count);

        for (at, copy) in copies {
            let resumed = build_osm_pack(&BuildOsmOptions {
                pack: copy.clone(),
                ..options.clone()
            })
            .unwrap_or_else(|error| panic!("resumed before {:?}: {error}", steps[at]));
            assert_eq!(records_and_contexts(&copy), expected, "{:?}", steps[at]);
            assert_eq!(resumed.accepted, whole.accepted, "{:?}", steps[at]);
            assert_eq!(resumed.imports, whole.imports, "{:?}", steps[at]);
            assert_eq!(resumed.output.record_count, whole.output.record_count);
            assert_eq!(resumed.output.rejection_count, whole.output.rejection_count);
            assert_eq!(resumed.checkpoints.count, whole.checkpoints.count);
            let resume = resumed.resumes.last().expect("resumed");
            assert_eq!(resume.after, after(&steps[at - 1]));
            assert!(resume.reused_bytes > 0, "{resume:?}");
            assert!(
                resume::find(&copy).expect("find").is_empty(),
                "a completed build leaves nothing to resume"
            );
        }
    }

    #[test]
    fn a_build_resumed_after_any_step_makes_the_same_pack() {
        resumed_after_any_step_makes_the_same_pack("resume", None);
    }

    #[test]
    fn a_build_spilling_coordinates_to_scratch_resumed_after_any_step_makes_the_same_pack() {
        resumed_after_any_step_makes_the_same_pack("resume-spilling", Some(4));
    }

    #[test]
    fn an_older_checkpoint_is_refused_and_kept() {
        let dir = temp_dir("old-checkpoint");
        let options = BuildOsmOptions {
            pack: dir.join("pack"),
            ..resumable_inputs(&dir)
        };
        fail_before(&options, Step::Contexts);
        let found = resume::find(&options.pack).expect("find");
        let manifest = found[0].manifest();
        let mut prior: Manifest<State> =
            serde_json::from_slice(&fs::read(&manifest).expect("read")).expect("manifest");
        prior.format = FORMAT_VERSION - 1;
        let bytes = serde_json::to_vec(&prior).expect("serialize");
        fs::write(&manifest, &bytes).expect("write");

        let failure = build_osm_pack(&BuildOsmOptions {
            leftover: Leftover::Resume,
            ..options
        })
        .expect_err("old checkpoint is refused");
        let expected = format!(
            "checkpoint of format {}, not {FORMAT_VERSION}",
            prior.format
        );
        assert!(failure.to_string().contains(&expected), "{failure}");
        assert_eq!(fs::read(&manifest).expect("checkpoint kept"), bytes);
    }

    #[test]
    fn a_stopped_build_is_reported_and_refuses_changed_inputs() {
        let dir = temp_dir("resume-refused");
        let options = BuildOsmOptions {
            pack: dir.join("pack"),
            ..resumable_inputs(&dir)
        };
        fail_before(&options, Step::Contexts);

        let refused = build_osm_pack(&options).expect_err("refused");
        let message = refused.to_string();
        assert!(message.contains("import addresses"), "{message}");
        assert!(
            message.contains("--resume") && message.contains("--fresh"),
            "{message}"
        );

        write_pbf(&options.inputs[1], &town());
        let changed = build_osm_pack(&BuildOsmOptions {
            leftover: Leftover::Resume,
            ..options.clone()
        })
        .expect_err("a changed input");
        let message = changed.to_string();
        assert!(message.contains("input 2"), "{message}");
        assert!(message.contains("--fresh"), "{message}");
        assert_eq!(resume::find(&options.pack).expect("find").len(), 1);

        let fresh = build_osm_pack(&BuildOsmOptions {
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
    }

    static MERGED_PLACES: LazyLock<Built> = LazyLock::new(|| {
        let item_layouts = [(8000, 80.0, "alpha"), (9000, 90.0, "beta")];
        let link_layouts = [(10_000, 100.0, "east"), (11_000, 110.0, "west")];
        let mut cells = vec![
            ("cities", renumbered(two_cities(), 1000)),
            ("county", moved(renumbered(county_seat(), 2000), 0.0, 10.0)),
            ("item", moved(renumbered(shared_item(), 3000), 0.0, 20.0)),
            ("ballard", moved(renumbered(ballard(), 4000), 0.0, 40.0)),
            ("riverton", moved(renumbered(riverton(), 5000), 0.0, 50.0)),
            ("streets", moved(renumbered(streets(), 6000), 0.0, 60.0)),
            (
                "springfield",
                moved(renumbered(springfield(), 7000), 0.0, 70.0),
            ),
        ];
        for (by, east, layout) in item_layouts {
            cells.push((
                layout,
                moved(renumbered(named_items(layout), by), 0.0, east),
            ));
        }
        for (by, east, layout) in link_layouts {
            cells.push((
                layout,
                moved(renumbered(strongest_links(layout), by), 0.0, east),
            ));
        }
        Built::new("merged-places", one_extract(cells))
    });

    fn place_coordinates(
        reader: &Reader,
        layer: &str,
        ids: &[&str],
    ) -> Vec<(String, serde_json::Value)> {
        let mut places = reader
            .records_json_by_layer(layer, 0)
            .expect("places")
            .into_iter()
            .map(|place| {
                (
                    place["id"].as_str().expect("id").to_string(),
                    place["geometry"]["coordinates"].clone(),
                )
            })
            .filter(|(id, _)| ids.contains(&id.as_str()))
            .collect::<Vec<_>>();
        places.sort_by(|left, right| left.0.cmp(&right.0));
        places
    }

    fn absent(reader: &Reader, id: &str) -> bool {
        reader.find_by_source_id(id).expect("lookup").is_none()
    }

    fn two_cities() -> Vec<Vec<TestElement>> {
        let city = |name, item| {
            [
                ("boundary", "administrative"),
                ("admin_level", "8"),
                ("name", name),
                ("wikidata", item),
            ]
        };
        vec![
            vec![
                node(
                    10,
                    0.2,
                    0.3,
                    &[
                        ("place", "city"),
                        ("name", "Seattle"),
                        ("alt_name", "Emerald City"),
                        ("name:lut", "dᶻidᶻəlal̓ič"),
                    ],
                ),
                node(
                    11,
                    2.4,
                    2.6,
                    &[
                        ("place", "city"),
                        ("name", "Bellevue"),
                        ("wikidata", "Q214164"),
                    ],
                ),
                node(100, 0.0, 0.0, &[]),
                node(101, 0.0, 1.0, &[]),
                node(102, 1.0, 1.0, &[]),
                node(103, 1.0, 0.0, &[]),
                node(110, 2.0, 2.0, &[]),
                node(111, 2.0, 3.0, &[]),
                node(112, 3.0, 3.0, &[]),
                node(113, 3.0, 2.0, &[]),
            ],
            vec![
                way(100, &[100, 101, 102, 103, 100], &[]),
                way(110, &[110, 111, 112, 113, 110], &[]),
            ],
            vec![
                TestElement::Relation {
                    id: 900,
                    members: vec![(100, "outer")],
                    nodes: vec![(10, "label")],
                    tags: city("Seattle", "Q5083").to_vec(),
                },
                relation(901, &[(110, "outer")], &city("Bellevue", "Q214164")),
            ],
        ]
    }

    #[test]
    fn cities_merge_into_one_record_at_their_label_or_wikidata_nodes_point() {
        let reader = Reader::open(MERGED_PLACES.pack()).expect("reader");
        assert!(absent(&reader, "osm:node:1010") && absent(&reader, "osm:node:1011"));
        let mut cities = reader
            .records_json_by_layer("locality", 0)
            .expect("cities")
            .into_iter()
            .map(|city| {
                (
                    city["id"].as_str().expect("id").to_string(),
                    city["geometry"]["coordinates"].clone(),
                    city["alt_names"].clone(),
                )
            })
            .filter(|(id, _, _)| id.starts_with("osm:relation:190"))
            .collect::<Vec<_>>();
        cities.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            cities,
            [
                (
                    "osm:relation:1900".to_string(),
                    serde_json::json!([0.3, 0.2]),
                    serde_json::json!(["Emerald City", "dᶻidᶻəlal̓ič"]),
                ),
                (
                    "osm:relation:1901".to_string(),
                    serde_json::json!([2.6, 2.4]),
                    serde_json::Value::Null,
                ),
            ]
        );
    }

    fn county_seat() -> Vec<Vec<TestElement>> {
        vec![
            vec![
                node(10, 0.5, 0.5, &[("place", "town"), ("name", "Kent")]),
                node(100, 0.0, 0.0, &[]),
                node(101, 0.0, 1.0, &[]),
                node(102, 1.0, 1.0, &[]),
                node(103, 1.0, 0.0, &[]),
            ],
            vec![way(100, &[100, 101, 102, 103, 100], &[])],
            vec![TestElement::Relation {
                id: 900,
                members: vec![(100, "outer")],
                nodes: vec![(10, "label"), (10, "admin_centre")],
                tags: vec![
                    ("boundary", "administrative"),
                    ("admin_level", "6"),
                    ("name", "King County"),
                ],
            }],
        ]
    }

    #[test]
    fn a_countys_seat_stays_its_own_place() {
        let reader = Reader::open(MERGED_PLACES.pack()).expect("reader");
        for id in ["osm:node:2010", "osm:relation:2900"] {
            assert!(!absent(&reader, id), "{id}");
        }
    }

    fn shared_item() -> Vec<Vec<TestElement>> {
        vec![
            vec![
                node(
                    10,
                    0.5,
                    0.5,
                    &[
                        ("place", "suburb"),
                        ("name", "Downtown Redmond"),
                        ("wikidata", "Q234332"),
                    ],
                ),
                node(
                    11,
                    5.0,
                    5.0,
                    &[
                        ("place", "city"),
                        ("name", "Redmond"),
                        ("wikidata", "Q234332"),
                    ],
                ),
                node(100, 0.0, 0.0, &[]),
                node(101, 0.0, 1.0, &[]),
                node(102, 1.0, 1.0, &[]),
                node(103, 1.0, 0.0, &[]),
            ],
            vec![way(100, &[100, 101, 102, 103, 100], &[])],
            vec![relation(
                900,
                &[(100, "outer")],
                &[
                    ("boundary", "administrative"),
                    ("admin_level", "8"),
                    ("name", "Redmond"),
                    ("wikidata", "Q234332"),
                ],
            )],
        ]
    }

    #[test]
    fn a_shared_item_merges_only_on_its_layer_and_within_bounds() {
        let reader = Reader::open(MERGED_PLACES.pack()).expect("reader");
        for id in ["osm:node:3010", "osm:node:3011", "osm:relation:3900"] {
            assert!(!absent(&reader, id), "{id}");
        }
    }

    fn ballard() -> Vec<Vec<TestElement>> {
        vec![
            vec![
                node(
                    10,
                    0.5,
                    0.5,
                    &[
                        ("place", "suburb"),
                        ("name", "Ballard"),
                        ("wikidata", "Q3428047"),
                    ],
                ),
                node(11, 5.0, 5.0, &[("place", "suburb"), ("name", "Ballard")]),
                node(
                    12,
                    0.6,
                    0.6,
                    &[("place", "neighbourhood"), ("name", "Loyal Heights")],
                ),
                node(100, 0.0, 0.0, &[]),
                node(101, 0.0, 1.0, &[]),
                node(102, 1.0, 1.0, &[]),
                node(103, 1.0, 0.0, &[]),
                node(200, -1.0, -1.0, &[]),
                node(201, -1.0, 6.0, &[]),
                node(202, 6.0, 6.0, &[]),
                node(203, 6.0, -1.0, &[]),
            ],
            vec![
                way(
                    100,
                    &[100, 101, 102, 103, 100],
                    &[
                        ("boundary", "administrative"),
                        ("admin_level", "10"),
                        ("name", "Ballard"),
                    ],
                ),
                way(200, &[200, 201, 202, 203, 200], &[]),
            ],
            vec![relation(
                900,
                &[(200, "outer")],
                &[
                    ("boundary", "administrative"),
                    ("admin_level", "8"),
                    ("name", "Lake City"),
                ],
            )],
        ]
    }

    #[test]
    fn a_place_node_merges_into_the_boundary_of_its_name_around_it() {
        let reader = Reader::open(MERGED_PLACES.pack()).expect("reader");
        let ids = [
            "osm:node:4010",
            "osm:node:4011",
            "osm:node:4012",
            "osm:way:4100",
        ];
        assert_eq!(
            place_coordinates(&reader, "neighbourhood", &ids),
            [
                ("osm:node:4011".to_string(), serde_json::json!([45.0, 5.0])),
                ("osm:node:4012".to_string(), serde_json::json!([40.6, 0.6])),
                ("osm:way:4100".to_string(), serde_json::json!([40.5, 0.5])),
            ]
        );
        let ballard = reader
            .find_by_source_id("osm:way:4100")
            .expect("lookup")
            .expect("Ballard");
        assert_eq!(
            reader.records().wikidata(ballard).expect("wikidata"),
            Some(3428047)
        );
    }

    fn riverton() -> Vec<Vec<TestElement>> {
        let admin = |level, name| {
            vec![
                ("boundary", "administrative"),
                ("admin_level", level),
                ("name", name),
            ]
        };
        vec![
            vec![
                node(10, 0.75, 0.75, &[("place", "town"), ("name", "Riverton")]),
                node(20, 0.7, 0.7, &[]),
                node(21, 0.7, 0.701, &[]),
                node(100, 0.0, 0.0, &[]),
                node(101, 0.0, 1.0, &[]),
                node(102, 1.0, 1.0, &[]),
                node(103, 1.0, 0.0, &[]),
                node(110, 0.6, 0.6, &[]),
                node(111, 0.6, 0.9, &[]),
                node(112, 0.9, 0.9, &[]),
                node(113, 0.9, 0.6, &[]),
            ],
            vec![
                way(100, &[100, 101, 102, 103, 100], &[]),
                way(110, &[110, 111, 112, 113, 110], &[]),
                way(
                    700,
                    &[20, 21],
                    &[("highway", "residential"), ("name", "Palmer Street")],
                ),
            ],
            vec![
                relation(900, &[(100, "outer")], &admin("6", "Southland")),
                TestElement::Relation {
                    id: 910,
                    members: vec![(110, "outer")],
                    nodes: vec![(10, "label")],
                    tags: admin("8", "Riverton"),
                },
            ],
        ]
    }

    #[test]
    fn a_boundary_that_absorbs_a_town_node_names_its_streets_as_a_settlement() {
        let reader = Reader::open(MERGED_PLACES.pack()).expect("reader");
        assert!(absent(&reader, "osm:node:5010"));
        let street = reader
            .find_by_source_id("osm:way:5700")
            .expect("lookup")
            .expect("street");
        assert_eq!(
            reader.record_json(street).expect("json")["label"],
            "Palmer Street, Riverton"
        );
    }

    fn unit_square(first: i64, west: f64) -> (Vec<TestElement>, TestElement) {
        let corners = [
            (west, 0.0),
            (west, 1.0),
            (west + 1.0, 1.0),
            (west + 1.0, 0.0),
        ];
        let nodes = (first..)
            .zip(corners)
            .map(|(id, (lon, lat))| node(id, lat, lon, &[]))
            .collect();
        let ring = [first, first + 1, first + 2, first + 3, first];
        (nodes, way(first, &ring, &[]))
    }

    fn merged_towns(
        towns: Vec<TestElement>,
        boundaries: Vec<(f64, TestElement)>,
    ) -> Vec<Vec<TestElement>> {
        let (mut nodes, mut ways) = (towns, Vec::new());
        for (at, (west, _)) in boundaries.iter().enumerate() {
            let (corners, ring) = unit_square(100 + 10 * at as i64, *west);
            nodes.extend(corners);
            ways.push(ring);
        }
        let relations = boundaries
            .into_iter()
            .map(|(_, relation)| relation)
            .collect();
        vec![nodes, ways, relations]
    }

    fn town_boundary(
        id: i64,
        ring: i64,
        label: Option<i64>,
        tags: &[(&'static str, &'static str)],
    ) -> TestElement {
        TestElement::Relation {
            id,
            members: vec![(ring, "outer")],
            nodes: label.map(|node| (node, "label")).into_iter().collect(),
            tags: [("boundary", "administrative"), ("admin_level", "8")]
                .iter()
                .chain(tags)
                .copied()
                .collect(),
        }
    }

    fn point_and_item(reader: &Reader, id: &str) -> (serde_json::Value, Option<u32>) {
        let record = reader.find_by_source_id(id).expect("lookup").expect(id);
        let point = reader
            .records_json_by_layer("locality", 0)
            .expect("towns")
            .into_iter()
            .find(|town| town["id"] == id)
            .expect("a town")["geometry"]["coordinates"]
            .clone();
        (point, reader.records().wikidata(record).expect("wikidata"))
    }

    fn springfield() -> Vec<Vec<TestElement>> {
        merged_towns(
            vec![node(
                10,
                0.5,
                2.5,
                &[
                    ("place", "town"),
                    ("name", "Springfield Centre"),
                    ("wikidata", "Q100"),
                ],
            )],
            vec![
                (
                    0.0,
                    town_boundary(
                        900,
                        100,
                        None,
                        &[("name", "Springfield"), ("wikidata", "Q100")],
                    ),
                ),
                (
                    2.0,
                    town_boundary(
                        910,
                        110,
                        None,
                        &[("name", "Springfield East"), ("wikidata", "Q100")],
                    ),
                ),
            ],
        )
    }

    #[test]
    fn a_place_node_merges_into_the_boundary_of_its_item_that_holds_it() {
        let reader = Reader::open(MERGED_PLACES.pack()).expect("reader");
        assert!(absent(&reader, "osm:node:7010"));
        assert_eq!(
            point_and_item(&reader, "osm:relation:7910").0,
            serde_json::json!([72.5, 0.5])
        );
    }

    fn named_items(layout: &str) -> Vec<Vec<TestElement>> {
        let (first, second, item) = match layout {
            "alpha" => ("Alpha", "Beta", "Q110"),
            _ => ("Delta", "Gamma", "Q111"),
        };
        let named = if layout == "alpha" { first } else { second };
        let town = |id, lat, name| {
            node(
                id,
                lat,
                lat,
                &[("place", "town"), ("name", name), ("wikidata", item)],
            )
        };
        merged_towns(
            vec![town(10, 0.3, first), town(11, 0.6, second)],
            vec![(0.0, town_boundary(900, 100, None, &[("name", named)]))],
        )
    }

    #[test]
    fn a_place_node_joins_the_boundary_another_node_of_its_item_named_in_either_order() {
        let reader = Reader::open(MERGED_PLACES.pack()).expect("reader");
        for (by, item) in [(8000, 110), (9000, 111)] {
            for node in [10, 11] {
                assert!(absent(&reader, &format!("osm:node:{}", by + node)), "{by}");
            }
            assert_eq!(
                point_and_item(&reader, &format!("osm:relation:{}", by + 900)).1,
                Some(item),
                "{by}"
            );
        }
    }

    fn strongest_links(layout: &str) -> Vec<Vec<TestElement>> {
        let (label_at, named_at, names, items) = match layout {
            "east" => (
                2.5,
                2.3,
                ["Westport", "Hastings", "Hastings Town"],
                ["Q120", "Q200", "Q300", "Q400"],
            ),
            _ => (
                2.3,
                2.5,
                ["Greymouth", "Napier", "Napier Town"],
                ["Q121", "Q201", "Q301", "Q401"],
            ),
        };
        let town = |id, lon, name, item| {
            node(
                id,
                0.5,
                lon,
                &[("place", "town"), ("name", name), ("wikidata", item)],
            )
        };
        merged_towns(
            vec![
                town(10, 0.5, names[0], items[1]),
                town(20, named_at, names[1], items[2]),
                town(21, label_at, names[2], items[3]),
            ],
            vec![
                (
                    0.0,
                    town_boundary(
                        900,
                        100,
                        Some(10),
                        &[("name", names[0]), ("wikidata", items[0])],
                    ),
                ),
                (
                    2.0,
                    town_boundary(910, 110, Some(21), &[("name", names[1])]),
                ),
            ],
        )
    }

    #[test]
    fn a_boundary_takes_its_strongest_links_point_and_its_item_when_it_has_none() {
        let reader = Reader::open(MERGED_PLACES.pack()).expect("reader");
        for (by, items, points) in [
            (10_000, (120, 400), ([100.5, 0.5], [102.5, 0.5])),
            (11_000, (121, 401), ([110.5, 0.5], [112.3, 0.5])),
        ] {
            for node in [10, 20, 21] {
                assert!(absent(&reader, &format!("osm:node:{}", by + node)), "{by}");
            }
            assert_eq!(
                point_and_item(&reader, &format!("osm:relation:{}", by + 900)),
                (serde_json::json!(points.0), Some(items.0)),
                "{by}"
            );
            assert_eq!(
                point_and_item(&reader, &format!("osm:relation:{}", by + 910)),
                (serde_json::json!(points.1), Some(items.1)),
                "{by}"
            );
        }
    }
}
