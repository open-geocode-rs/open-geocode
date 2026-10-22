//! Streaming OSM build.
//!
//! Memory stays bounded whatever the input size. Ways get their geometry
//! from a node store: the scan marks the nodes kept ways need in a bitmap
//! over the id space, the node pass keeps only their coordinates, in id
//! order, and the way pass reads the kept ways again and finds each node by
//! its rank among the marked ids. Finished records flow through scratch
//! files and external sorts. A region fits in memory and never touches disk;
//! a planet maps its coordinates from scratch when the budget cannot hold
//! them, and spills and merges records through the same code. The budget
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
//! write   context records, then the rest, each in Hilbert order,
//!         drawing a sample of each country's records
//! seal    text, spatial and parser dictionary indexes into the Pack file
//! train   the address parser on the sample, its weights added to the Pack
//! ```

mod address;
pub(crate) use address::LOCALITY_KEYS;
mod admin_levels;
mod boundary;
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
    collections::{HashMap, HashSet},
    fmt,
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

use crate::error::{Error, Result};
use rayon::prelude::*;

use crate::{
    alloc,
    builder::{
        addresses::{self, AddressSource},
        plan::{BuildPlan, ItemCounts, Scanned, plan_build},
        progress::{BuildObserver, BuildStage, Progress, TerminalObserver},
        report::{
            BUILD_REPORT_SCHEMA_VERSION, BuilderReport, ReportInput, SlashNumberCounts,
            SorterReport,
        },
    },
    extsort::{ExternalSorter, Scratch, Spool},
    memory::Budget,
    pack::{
        AUDIT_DIR, BUILD_REPORT_FILE, REJECTIONS_FILE, RecordContext, RecordId, Writer,
        WriterOptions,
    },
    parse::lattice::{
        CountryCode,
        train::{self, Sampler},
        weights::{WEIGHTS_SECTION, WEIGHTS_VERSION},
    },
    record::{
        AddressComponents, AddressRecord, OSM_DATASET, ObjectType, Record, RejectedRecord,
        StreetRecord,
    },
    records::quantize,
    resources::Resources,
    text_index::PostcodeAreas,
    util::{fold::search_key, geo::point_lon_lat, hilbert::hilbert_key},
};
use boundary::{
    BoundaryIndex, BoundaryRelationStub, BoundaryWay, ContextOrigin, Vertex, build_boundaries,
    record_context,
};
use emitted::Emitted;
pub(crate) use locations::{
    CHUNK_BYTES as NODE_SET_CHUNK_BYTES, LOCATION_BYTES as NODE_SLOT_BYTES, LoadStats,
    TABLE_BYTES as NODE_SET_TABLE_BYTES,
};
use locations::{NodeSet, Slots};
use pbf::for_each_block;
pub use pbf::replication_timestamp;
use scan::scan_block;
use spill::{PendingAddress, PendingRecord, PendingStreet, WayKind};

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
    /// The most scratch disk the build may use; what is free when not set.
    pub scratch_budget_bytes: Option<u64>,
    /// Build even when the pre-flight check finds too little memory or disk.
    pub force: bool,
    /// What the inputs are, for example region ids, kept in the build
    /// report. One per input, in the same order.
    pub input_sources: Vec<String>,
    /// Told about each stage as it starts, advances and finishes, and about
    /// the plan's warnings.
    pub observer: Arc<dyn BuildObserver>,
    /// Set to stop the build: it then fails with
    /// [`crate::builder::BuildCancelled`] and publishes nothing.
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
            .field("input_sources", &self.input_sources)
            .field("cancel", &self.cancel)
            .field("addresses", &self.addresses)
            .finish_non_exhaustive()
    }
}

/// Plan a build of `options` on this machine: probe it, size the budget and
/// the threads, and check the memory and disk the input needs.
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
    let scratch = options.scratch_dir.as_deref().unwrap_or(&options.pack);
    // The passes run per input but the sorters, the heap the boundary stage
    // holds and the Pack are shared, so the build is sized to every input
    // together.
    plan_build(
        total_input_bytes(&options.inputs)?,
        options.imported_rows,
        scratch,
        &options.pack,
        options.memory_budget_bytes,
        options.scratch_budget_bytes,
        options.force,
    )
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

/// Build with a plan made by [`plan_osm_build`]. The caller sizes the
/// process's rayon pool to `plan.threads` before any parallel work, since
/// one pool serves every stage.
pub fn build_planned_osm_pack(
    options: BuildOsmOptions,
    mut plan: BuildPlan,
) -> Result<BuilderReport> {
    let started = Instant::now();
    for warning in plan.shortfalls.iter().chain(&plan.warnings) {
        options.observer.warned(warning);
    }
    // The sorters and the parser's lattice cache split what the others leave
    // of the pool by fair shares, each at most `budget`; the node store
    // takes what it needs of the rest. While records are written the text
    // index is what every record waits on, so it takes half a sorter's: more
    // indexing threads, fewer segments. The context record and spatial
    // sorters hold a few times less than the others, and are capped to stay
    // so.
    let budget = usize::try_from(plan.sorter_bytes).unwrap_or(usize::MAX);
    let memory = Budget::new(usize::try_from(plan.memory_budget_bytes).unwrap_or(usize::MAX));
    memory.sample(&plan.resources);
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

    let mut writer = Writer::create_with(
        &options.pack,
        WriterOptions {
            scratch_dir: options.scratch_dir.clone(),
            memory: Arc::clone(&memory),
            text_index_memory_bytes: (budget / 2).max(1),
        },
    )?;
    let scratch = Arc::clone(writer.scratch());
    scratch.cancel_on(Arc::clone(&options.cancel));
    let progress = Progress::new(Arc::clone(&options.observer), Arc::clone(&options.cancel));
    // Spill files stay in the page cache while they fit in a quarter of what
    // the build leaves free: the rest keeps the Pack and the input it
    // rereads, and dirty pages the lazy writer holds back are not free.
    scratch.set_cache_bytes(
        plan.resources
            .available_memory_bytes
            .saturating_sub(plan.memory_budget_bytes + plan.untracked_bytes)
            / 4,
    );
    let mut build = Build {
        report: BuilderReport {
            schema_version: BUILD_REPORT_SCHEMA_VERSION,
            inputs: report_inputs,
            pack: options.pack.display().to_string(),
            input_bytes,
            datasets: vec![OSM_DATASET.to_string()],
            ..BuilderReport::default()
        },
        audit: Audit::create(writer.generation())?,
        records: ExternalSorter::new(&scratch, "records", &memory, budget),
        addresses: ExternalSorter::new(&scratch, "addresses", &memory, budget),
        streets: ExternalSorter::new(&scratch, "streets", &memory, budget),
        context_records: ExternalSorter::new(
            &scratch,
            "context-records",
            &memory,
            (budget / 16).max(1),
        ),
        seq: 0,
    };
    build.report.scratch.memory_budget_bytes = plan.memory_budget_bytes;
    build.report.resources.threads = rayon::current_num_threads();
    build.report.resources.plan = plan.clone();

    // Boundary geometry accumulates across every input, because a boundary
    // relation and the ways it is made of can only be assembled once all of
    // them are known. Everything else each pass needs is per input.
    let mut geometry = Geometry::default();
    let passes = Passes {
        scratch: &scratch,
        memory: &memory,
        progress: &progress,
        observer: options.observer.as_ref(),
    };
    let mut scanned_bytes = 0;
    // Each input is read on its own: the node pass stores coordinates in id
    // order, and ids ascend only within one sorted PBF.
    for input in &options.inputs {
        scanned_bytes += pbf::input_bytes(input)?;
        read_input(
            input,
            &mut build,
            &mut geometry,
            &mut plan,
            scanned_bytes,
            &passes,
        )?;
        build.report.resources.plan = plan.clone();
    }
    let Geometry {
        relations,
        boundary_feature_ids,
        boundary_ways,
        member_vertices,
    } = geometry;

    // Admin boundaries and their place records. Every input has been read,
    // so a relation whose ways span two extracts is assembled from all of
    // them.
    let phase = Instant::now();
    progress.start(BuildStage::Boundaries, None)?;
    // Boundary features that were incomplete have no entry in `boundary_ways`
    // and resolve to `None`, like an incomplete member.
    let mut member_lines: HashMap<i64, Option<&[Vertex]>> = boundary_feature_ids
        .iter()
        .map(|way_id| (*way_id, None))
        .collect();
    for way in &boundary_ways {
        member_lines.insert(way.object_id, Some(way.vertices.as_slice()));
    }
    for (way_id, line) in &member_vertices {
        member_lines.insert(*way_id, line.as_deref());
    }
    let boundaries = build_boundaries(&boundary_ways, &relations, &member_lines, || {
        progress.check()
    })?;
    build
        .report
        .unmatched_admin_level_ids
        .extend_from_slice(boundaries.unmatched_admin_ids());
    drop(member_lines);
    drop((
        boundary_ways,
        member_vertices,
        relations,
        boundary_feature_ids,
    ));
    for (origin, record) in boundaries.place_records(&mut build.report) {
        build.push(record, Some(origin))?;
    }
    progress.finish(BuildStage::Boundaries);
    build.report.phases.boundary_build_ms = phase.elapsed().as_millis();

    // Addresses from outside OSM join the OSM ones before they are ordered.
    // The source is asked only now, knowing the countries the Pack covers.
    let phase = Instant::now();
    let files = match &options.addresses {
        Some(source) => source.files(&boundaries.coverage())?,
        None => Vec::new(),
    };
    let import_bytes = files
        .iter()
        .map(|file| std::fs::metadata(&file.path).map(|metadata| metadata.len()))
        .sum::<std::io::Result<u64>>()?;
    // A source can cover more than the extracts: a country's file for a Pack
    // of one province. Rows outside every extract's stated bounds are left
    // out; an extract that states none keeps every row.
    let extracts = options
        .inputs
        .iter()
        .map(|input| pbf::header_bbox(input))
        .collect::<Result<Option<Vec<_>>>>()?;
    progress.start(BuildStage::ImportAddresses, Some(import_bytes))?;
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
        if !build.report.datasets.contains(&imported.dataset) {
            build.report.datasets.push(imported.dataset.clone());
        }
        build.report.imports.push(imported);
    }
    progress.finish(BuildStage::ImportAddresses);
    build.report.phases.address_import_ms = phase.elapsed().as_millis();

    let Build {
        mut report,
        audit,
        mut records,
        addresses,
        streets,
        context_records,
        mut seq,
        ..
    } = build;

    // Context records first: once their ids are known, the boundary index can
    // describe every other record, and tell the country each address lies in.
    let phase = Instant::now();
    let context_stats = context_records.stats();
    let mut ordered = Spool::<PendingRecord>::create(&scratch, "context-ordered")?;
    let mut boundary_ids = vec![0; boundaries.len()];
    let mut country_ids = HashMap::new();
    let first_id = writer.record_count();
    // The areas that are settlements by their own tags, and the settlement
    // points that may name the area they lie in.
    let mut settled = HashSet::new();
    let mut settlements = Vec::new();
    for (index, item) in context_records.finish()?.enumerate() {
        let item = item?;
        let record_id = first_id + index as u64;
        match &item.origin {
            Some(ContextOrigin::Boundary(boundary)) => boundary_ids[*boundary as usize] = record_id,
            Some(ContextOrigin::DerivedCountry(code)) => {
                country_ids.insert(code.clone(), record_id);
            }
            None => {}
        }
        if let Record::Place(_, place) = &item.record
            && street::is_settlement(&place.place_type)
        {
            match item.origin {
                Some(ContextOrigin::Boundary(_)) => {
                    settled.insert(record_id);
                }
                Some(ContextOrigin::DerivedCountry(_)) => {}
                None => {
                    settlements.extend(search_key(&place.name).zip(point_lon_lat(&place.geometry)))
                }
            }
        }
        ordered.write(&item)?;
    }
    let country_codes = boundaries.country_codes(&boundary_ids, &country_ids);
    let area_names = boundaries.context_names(&boundary_ids, &country_ids);
    writer.set_context_names(area_names.clone());
    writer.set_country_codes(country_codes.clone());
    writer.set_region_groups(boundaries.region_groups(&boundary_ids));
    let index = boundaries.into_index(&boundary_ids, &country_ids);
    // An admin area is a settlement too when a settlement of its name lies
    // in it, as a town's boundary holds the town's point: Nominatim links
    // the two the same way. A ward or a local board holds none of its name.
    settled.extend(
        settlements
            .par_iter()
            .filter_map(|(name, [lon, lat])| {
                let locality = index
                    .context_for_point(*lon, *lat, Default::default())
                    .admin_context
                    .locality_record_id()?;
                area_names
                    .get(&locality)?
                    .iter()
                    .any(|area| search_key(area).as_ref() == Some(name))
                    .then_some(locality)
            })
            .collect::<Vec<_>>(),
    );
    drop(settlements);
    let context_count = ordered.written();
    let mut sample = Sampler::default();
    write_ordered(
        &mut writer,
        &mut sample,
        ordered.into_reader()?,
        context_count,
        &index,
        &progress,
        BuildStage::ContextRecords,
    )?;
    report.phases.context_record_write_ms = phase.elapsed().as_millis();

    let phase = Instant::now();
    let street_stats = streets.stats();
    report.accepted.streets = join_streets(
        streets,
        &index,
        &settled,
        &area_names,
        &mut records,
        &mut seq,
    )?;
    drop(area_names);
    report.phases.street_join_ms = phase.elapsed().as_millis();

    let phase = Instant::now();
    let address_stats = addresses.stats();
    // Slash numbers are read before imported rows are matched against OSM
    // ones, so an OSM flat "10/1367" repeats an imported 1367 unit 10.
    let mut slash_numbers = SlashNumberCounts::default();
    let addresses = if files.is_empty() {
        addresses
    } else {
        drop_repeated_houses(
            addresses,
            &scratch,
            &memory,
            budget,
            &mut report,
            &mut slash_numbers,
        )?
    };
    let mut runs = AddressRuns::new(&mut records, &mut seq, &index, &country_codes);
    if files.is_empty() {
        slash::read_slash_numbers(addresses.finish()?, &mut slash_numbers, |address| {
            runs.push(address)
        })?;
    } else {
        for address in addresses.finish()? {
            runs.push(address?)?;
        }
    }
    let postcodes = runs.finish()?;
    report.slash_numbers = slash_numbers;
    // Every address is placed, so each postcode's points are all known.
    let mut postcode_points = Vec::new();
    for record in postcodes.into_records() {
        report.accept_postcode();
        let [lon, lat] = point_lon_lat(&record.geometry).ok_or_else(|| {
            Error::BadGeometry(format!("postcode {} has no point", record.postcode))
        })?;
        postcode_points.push((record.postcode.clone(), [lon, lat]));
        records.push(PendingRecord {
            key: hilbert_key(quantize(lon)?, quantize(lat)?),
            seq,
            origin: None,
            record: Record::Postcode(record),
        })?;
        seq += 1;
    }
    writer.set_postcode_areas(PostcodeAreas::new(postcode_points));
    report.phases.address_order_ms = phase.elapsed().as_millis();

    let phase = Instant::now();
    let record_stats = records.stats();
    write_ordered(
        &mut writer,
        &mut sample,
        records.finish()?,
        record_stats.items,
        &index,
        &progress,
        BuildStage::Records,
    )?;
    drop(index);
    report.phases.record_write_ms = phase.elapsed().as_millis();

    let phase = Instant::now();
    report.scratch.spilled_bytes = scratch.spilled_bytes();
    report.scratch.peak_tracked_bytes = memory.peak() as u64;
    progress.start(BuildStage::Indexes, None)?;
    let (mut sealed, mut stats) = writer.seal()?;
    progress.finish(BuildStage::Indexes);
    report.phases.pack_seal_ms = phase.elapsed().as_millis();

    // The parser learns from the sealed Pack's dictionaries and the records
    // drawn while they were written, and its weights join the Pack before it
    // is published.
    let phase = Instant::now();
    progress.start(BuildStage::Parser, None)?;
    // By now the build has used memory and page cache a fresh process would
    // have: sample again what is left.
    let resources = Resources::probe();
    memory.sample(&resources);
    report.resources.at_training = resources;
    let (weights, training) = train::train(&sealed.path(), sample, &scratch, &memory, budget)?;
    if let Some(weights) = weights {
        let bytes = weights.to_bytes();
        sealed.add_section(WEIGHTS_SECTION, WEIGHTS_VERSION, &bytes)?;
        stats.bytes = sealed.bytes();
        stats
            .sections
            .insert(WEIGHTS_SECTION.to_string(), bytes.len() as u64);
    }
    drop(scratch);
    progress.finish(BuildStage::Parser);
    progress.check()?;
    report.parser_training = training;
    report.phases.parser_training_ms = phase.elapsed().as_millis();

    for (name, stats) in [
        ("addresses", address_stats),
        ("streets", street_stats),
        ("records", record_stats),
        ("context_records", context_stats),
    ] {
        report.scratch.sorters.insert(
            name.to_string(),
            SorterReport {
                items: stats.items,
                runs: stats.runs,
            },
        );
    }
    report.output.pack_bytes = stats.bytes;
    report.output.record_count = sealed.manifest().record_count;
    report.output.text_index_bytes = stats.text_index_bytes;
    report.output.sections = stats.sections;
    report.output.rejection_count = audit.count;
    report.scratch.peak_heap_bytes = alloc::peak_bytes().unwrap_or_default() as u64;
    report.phases.total_ms = started.elapsed().as_millis();
    report.finalize_throughput();
    audit.finish(&report)?;
    sealed.publish()?;
    Ok(report)
}

/// Build state shared by the passes.
struct Build {
    report: BuilderReport,
    audit: Audit,
    /// POIs, interpolations and streets, then the addresses.
    records: ExternalSorter<PendingRecord>,
    /// Addresses, until every one is known and they can join `records` a
    /// street at a time.
    addresses: ExternalSorter<PendingAddress>,
    /// The ways of streets, until every one is known and the ways of one
    /// street can join (see [`join_streets`]).
    streets: ExternalSorter<PendingStreet>,
    /// Places: everything a context tuple can point at.
    context_records: ExternalSorter<PendingRecord>,
    seq: u64,
}

impl Build {
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
            self.seq += 1;
            return self
                .addresses
                .push(PendingAddress::new(address, self.seq - 1)?);
        }
        if let Record::Street(street) = record {
            self.seq += 1;
            return self.streets.push(PendingStreet::new(street, self.seq - 1)?);
        }
        let [lon, lat] = record.display_point().ok_or_else(|| {
            Error::BadGeometry(format!("record {} has no display point", record.id()))
        })?;
        let pending = PendingRecord {
            key: hilbert_key(quantize(lon)?, quantize(lat)?),
            seq: self.seq,
            origin,
            record,
        };
        self.seq += 1;
        if pending.record.layer().is_context() {
            self.context_records.push(pending)
        } else {
            self.records.push(pending)
        }
    }
}

/// Addresses on their way into the record sorter, in street order: each
/// group placed in the Hilbert order by its first address, so the group
/// stays one run for the record store and the text index to share what its
/// addresses state; lent its street's postcodes when it states none; and
/// counted toward its postcode's point in the country it lies in.
struct AddressRuns<'a> {
    records: &'a mut ExternalSorter<PendingRecord>,
    seq: &'a mut u64,
    index: &'a BoundaryIndex,
    country_codes: &'a HashMap<RecordId, CountryCode>,
    street: postcode::Street,
    postcodes: postcode::Accumulator,
    batch: Vec<PendingAddress>,
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
    ) -> Self {
        Self {
            records,
            seq,
            index,
            country_codes,
            street: postcode::Street::default(),
            postcodes: postcode::Accumulator::default(),
            batch: Vec::new(),
            group: None,
        }
    }

    fn push(&mut self, address: PendingAddress) -> Result<()> {
        self.batch.push(address);
        if self.batch.len() == WRITE_BATCH_RECORDS {
            self.place_batch()?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<postcode::Accumulator> {
        self.place_batch()?;
        Ok(self.postcodes)
    }

    fn place_batch(&mut self) -> Result<()> {
        let batch = std::mem::take(&mut self.batch);
        let starts = batch
            .iter()
            .enumerate()
            .map(|(at, address)| match at {
                0 => self
                    .group
                    .as_ref()
                    .is_none_or(|(key, ..)| key[..] != address.order[..address.group_len]),
                _ => !batch[at - 1].same_group(address),
            })
            .collect::<Vec<_>>();
        // One lookup per group, not per address: a group is one street in
        // one cell, which a border runs through too rarely to matter. Only
        // a group stating a postcode needs its country.
        let countries = batch
            .par_iter()
            .zip(&starts)
            .map(|(address, start)| {
                let Record::Address(record) = &address.record else {
                    unreachable!("a pending address holds an address");
                };
                (*start && record.address.postcode.is_some())
                    .then(|| record_context(self.index, &address.record))
                    .flatten()
                    .and_then(|context| context.admin_context.country_record_id())
                    .and_then(|country| self.country_codes.get(&country).copied())
            })
            .collect::<Vec<_>>();
        for ((mut address, start), country) in batch.into_iter().zip(starts).zip(countries) {
            if start {
                let [lon, lat] = address.record.display_point().ok_or_else(|| {
                    Error::BadGeometry(format!(
                        "record {} has no display point",
                        address.record.id()
                    ))
                })?;
                self.group = Some((
                    address.order[..address.group_len].to_vec(),
                    hilbert_key(quantize(lon)?, quantize(lat)?),
                    country,
                ));
            }
            let (_, key, country) = self.group.as_ref().expect("a group has a start");
            self.street.lend(&mut address);
            let Record::Address(record) = &address.record else {
                unreachable!("a pending address holds an address");
            };
            self.postcodes
                .accept(*country, &record.address, &record.geometry);
            self.records.push(PendingRecord {
                key: *key,
                seq: *self.seq,
                origin: None,
                record: address.record,
            })?;
            *self.seq += 1;
        }
        Ok(())
    }
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
                record: Record::Street(street),
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

/// An address kept so far in one house group.
struct KeptHouse {
    point: [f64; 2],
    /// Where in the group's buffer it waits.
    at: usize,
}

/// The addresses again, without the imported rows that repeat an address
/// the Pack already has, OSM ones first: read in house order, then sorted
/// back into street groups. Each dropped row is counted on its file.
///
/// One record answers for one house. A row within
/// [`REPEATED_HOUSE_RADIUS_M`] of a kept address of its house agrees with it
/// and is dropped. One beyond that, but within [`DISPUTED_HOUSE_RADIUS_M`] of
/// a kept address it does not contradict, is dropped too: the kept address
/// keeps its point, OSM's when OSM has the house, and is marked
/// [`AddressRecord::disputed`]. Only a row farther still, or one stating
/// another postcode, locality or area than every kept address near it, is a
/// house of its own. Two houses of one street name and number lie apart by
/// the distance between the places that repeat the name, which the radius
/// stays below; where they lie closer, the postcode or the locality tells
/// them apart.
fn drop_repeated_houses(
    addresses: ExternalSorter<PendingAddress>,
    scratch: &Arc<crate::extsort::Scratch>,
    memory: &Arc<Budget>,
    budget: usize,
    report: &mut BuilderReport,
    slash_numbers: &mut SlashNumberCounts,
) -> Result<ExternalSorter<PendingAddress>> {
    let mut houses = ExternalSorter::new(scratch, "houses", memory, budget);
    slash::read_slash_numbers(addresses.finish()?, slash_numbers, |address| {
        let seq = address.seq;
        houses.push(PendingAddress::house(address.into_address(), seq)?)
    })?;
    let mut grouped = ExternalSorter::new(scratch, "addresses-kept", memory, budget);
    // A kept address is marked disputed only by the rows after it, so the
    // house waits whole before it is passed on.
    let mut house: Vec<(AddressRecord, u64)> = Vec::new();
    let mut house_key: Option<PendingAddress> = None;
    let mut kept: Vec<KeptHouse> = Vec::new();
    let flush = |house: &mut Vec<(AddressRecord, u64)>,
                 grouped: &mut ExternalSorter<PendingAddress>|
     -> Result<()> {
        for (record, seq) in house.drain(..) {
            grouped.push(PendingAddress::new(record, seq)?)?;
        }
        Ok(())
    };
    for address in houses.finish()? {
        let address = address?;
        if house_key
            .as_ref()
            .is_none_or(|house| !house.same_group(&address))
        {
            flush(&mut house, &mut grouped)?;
            kept.clear();
        }
        let point = address.record.display_point().ok_or_else(|| {
            Error::BadGeometry(format!(
                "record {} has no display point",
                address.record.id()
            ))
        })?;
        let record = address.clone().into_address();
        let imported = record.source.object_type == ObjectType::Row;
        let distance = |kept: &KeptHouse| {
            crate::spatial_index::haversine_m(kept.point[0], kept.point[1], point[0], point[1])
        };
        let repeated = imported
            && kept
                .iter()
                .any(|kept| distance(kept) <= REPEATED_HOUSE_RADIUS_M);
        let disputed = if imported && !repeated {
            kept.iter()
                .filter(|kept| {
                    distance(kept) <= DISPUTED_HOUSE_RADIUS_M
                        && !other_house(&house[kept.at].0, &record)
                })
                .min_by(|left, right| distance(left).total_cmp(&distance(right)))
                .map(|kept| kept.at)
        } else {
            None
        };
        if repeated || disputed.is_some() {
            if let Some(at) = disputed {
                house[at].0.disputed = true;
            }
            if let Some(import) = report
                .imports
                .iter_mut()
                .find(|import| import.dataset == record.source.dataset)
            {
                if repeated {
                    import.duplicates += 1;
                } else {
                    import.disputes += 1;
                }
            }
        } else {
            kept.push(KeptHouse {
                point,
                at: house.len(),
            });
            house.push((record, address.seq));
        }
        house_key = Some(address);
    }
    flush(&mut house, &mut grouped)?;
    Ok(grouped)
}

/// Whether two addresses of one house key state where they are differently:
/// both state a postcode, a locality or areas, and they differ. Then they are
/// two houses, however close.
fn other_house(kept: &AddressRecord, row: &AddressRecord) -> bool {
    fn differ<T: PartialEq>(left: Option<T>, right: Option<T>) -> bool {
        left.zip(right).is_some_and(|(left, right)| left != right)
    }
    let postcode = |address: &AddressComponents| {
        address
            .postcode
            .as_deref()
            .and_then(crate::text_index::compact_postcode)
    };
    let locality = |address: &AddressComponents| {
        address
            .locality
            .as_deref()
            .and_then(crate::util::fold::search_key)
    };
    let areas = |address: &AddressComponents| {
        Some(
            address
                .areas
                .iter()
                .filter_map(|area| crate::util::fold::search_key(area))
                .collect::<Vec<_>>(),
        )
        .filter(|areas| !areas.is_empty())
    };
    let (kept, row) = (&kept.address, &row.address);
    differ(postcode(kept), postcode(row))
        || differ(locality(kept), locality(row))
        || differ(areas(kept), areas(row))
}

/// Boundary geometry gathered across inputs, for the boundary stage.
#[derive(Debug, Default)]
struct Geometry {
    relations: Vec<BoundaryRelationStub>,
    /// Boundary ways kept by the scan. When one is also a relation member,
    /// its vertices are reused instead of being read a second time.
    boundary_feature_ids: HashSet<i64>,
    boundary_ways: Vec<BoundaryWay>,
    /// Relation member ways by id: their vertices, `None` when a node is
    /// missing.
    member_vertices: HashMap<i64, Option<Vec<Vertex>>>,
}

/// What the passes over one input share with the rest of the build.
struct Passes<'a> {
    scratch: &'a Arc<Scratch>,
    memory: &'a Arc<Budget>,
    progress: &'a Progress,
    observer: &'a dyn BuildObserver,
}

/// Kept ways one scan counted, by what they become.
#[derive(Debug, Default)]
struct KeptWays {
    addresses: u64,
    streets: u64,
    /// Ways that become records: every kind but boundaries.
    records: u64,
    all: u64,
}

/// Read one input: the scan, the boundary member ways, the node pass and
/// the way pass. `scanned_bytes` is how much of every input the scan has
/// read once this one's is done, for the plan to extrapolate from.
fn read_input(
    input: &Path,
    build: &mut Build,
    geometry: &mut Geometry,
    plan: &mut BuildPlan,
    scanned_bytes: u64,
    passes: &Passes,
) -> Result<()> {
    let progress = passes.progress;
    // Pass 1: classify everything, and mark the nodes kept ways reference.
    let phase = Instant::now();
    let nodes = NodeSet::new(passes.memory);
    // Nodes whose address tags interpolation ways read.
    let mut tag_ids = Vec::new();
    let mut node_blobs = Vec::new();
    let mut way_blobs = Vec::new();
    // Blobs holding a kept way or a member way: all the way pass rereads.
    let mut kept_blobs = Vec::new();
    let mut kept = KeptWays::default();
    let read = for_each_block(
        input,
        progress,
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
                    }
                }
            }
            geometry.relations.extend(output.relations);
            build.absorb(output.emitted)
        },
    )?;
    build.report.geometry_passes.scan_read_bytes += read;
    build.report.phases.scan_ms += phase.elapsed().as_millis();

    // Pass 2: the ways boundary relations are made of. They often carry no
    // tags of their own, so pass 1 could not know to keep them.
    let phase = Instant::now();
    let member_ids = boundary::required_boundary_way_ids(&geometry.relations)
        .into_iter()
        .filter(|way_id| !geometry.boundary_feature_ids.contains(way_id))
        .collect::<HashSet<_>>();
    let mut members = HashSet::new();
    if member_ids.is_empty() {
        progress.start(BuildStage::BoundaryMembers, Some(0))?;
        progress.finish(BuildStage::BoundaryMembers);
    } else {
        let read = for_each_block(
            input,
            progress,
            BuildStage::BoundaryMembers,
            Some(&way_blobs),
            |offset, block| Ok((offset, member_ways(&block, &member_ids))),
            |(offset, found)| {
                if !found.is_empty() {
                    kept_blobs.push(offset);
                }
                for (way_id, refs) in found {
                    // A way the input repeats is read where it first appears.
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

    // The scan has counted what the rest of the build holds.
    let accepted = &build.report.accepted;
    let counted = |layer: &str| accepted.by_layer.get(layer).copied().unwrap_or_default();
    plan.refine(&Scanned {
        items: ItemCounts {
            needed_nodes: nodes.len(),
            node_set_bytes: nodes.bytes(),
            kept_ways: build.report.geometry_passes.kept_ways + kept.all,
            records: accepted.total + kept.records,
            addresses: counted("address") + kept.addresses,
            streets: counted("street") + kept.streets,
            boundary_refs: build.report.geometry_passes.boundary_refs,
            imported_rows: plan.items.imported_rows,
        },
        read_bytes: scanned_bytes,
        heap_bytes: alloc::live_bytes().map(|bytes| bytes as u64),
        tracked_bytes: passes.memory.used() as u64,
    })?;
    for warning in plan.shortfalls.iter().chain(&plan.warnings) {
        passes.observer.warned(warning);
    }
    build.report.geometry_passes.kept_ways += kept.all;
    passes.memory.sample(&plan.resources);

    // Pass 3: the coordinates of every marked node.
    let phase = Instant::now();
    tag_ids.sort_unstable();
    tag_ids.dedup();
    let slots = Slots::new(
        nodes.len(),
        plan.locations_in_memory,
        passes.memory,
        passes.scratch,
    )?;
    let (locations, stored) =
        locations::load(input, &node_blobs, nodes, &tag_ids, slots, progress)?;
    drop(tag_ids);
    build.report.geometry_passes.add(&stored);
    build.report.phases.node_join_ms += phase.elapsed().as_millis();

    // Pass 4: the kept ways and member ways again, now with coordinates.
    let phase = Instant::now();
    kept_blobs.sort_unstable();
    kept_blobs.dedup();
    let mut input_members: HashMap<i64, Option<Vec<Vertex>>> = HashMap::new();
    let read = for_each_block(
        input,
        progress,
        BuildStage::AssembleWays,
        Some(&kept_blobs),
        |_, block| Ok(emit::assemble(&block, &locations, &members)),
        |assembled| {
            let resolution = &mut build.report.geometry_resolution;
            resolution.resolved_node_refs += assembled.resolved;
            for (way_id, line, found) in assembled.members {
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    input_members.entry(way_id)
                {
                    resolution.resolved_node_refs += found;
                    entry.insert(line);
                }
            }
            geometry
                .boundary_ways
                .extend(assembled.output.boundary_ways);
            build.absorb(assembled.output.emitted)
        },
    )?;
    build.report.geometry_passes.way_read_bytes += read;
    geometry.member_vertices.extend(input_members);
    build.report.phases.feature_emission_ms += phase.elapsed().as_millis();
    Ok(())
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
fn write_ordered(
    writer: &mut Writer,
    sample: &mut Sampler,
    records: impl Iterator<Item = Result<PendingRecord>> + Send,
    total: u64,
    index: &BoundaryIndex,
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
                    .and_then(|batch| {
                        let (batch, countries): (Vec<_>, Vec<_>) = batch
                            .into_par_iter()
                            .map(|pending| {
                                let located = record_context(index, &pending.record);
                                let country = located
                                    .as_ref()
                                    .and_then(|context| context.admin_context.country_record_id());
                                let context: Option<RecordContext> = match pending.origin {
                                    Some(_) => None,
                                    None => located,
                                };
                                ((pending.record, context), country)
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
            report.scratch.sorters["records"].runs, 0,
            "a small input never spills"
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
}
