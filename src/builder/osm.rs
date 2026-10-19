//! Streaming OSM build.
//!
//! Memory stays bounded by the sorter budget whatever the input size: features,
//! node references, resolved coordinates and finished records all flow through
//! scratch files and external sorts. A region fits in the budget and never
//! touches disk; a planet spills and merges through the same code. The budget
//! and the worker threads are sized to what the machine can give before the
//! build starts (see [`plan_build`]).
//!
//! One Pack can cover several extracts. The three PBF passes and the feature
//! emission below run once per input, because each pass reads its input in
//! file order and the node join merges against a request stream sorted by
//! node id, which only holds within one sorted PBF. Their records all land in
//! the same sorters, so the stages after them -- boundaries, the record
//! write, the seal and the parser training -- run once over everything. That
//! keeps one text index with one set of collection statistics, so BM25 scores
//! stay comparable across regions, which is what searching several countries
//! as one requires.
//!
//! ```text
//! per inputs: vec![//! pass 1  scan every block (parallel)    -> records for nodes], way features,
//!                                           node references, boundary relations
//! pass 2  read way blocks                 -> boundary relation member ways
//! pass 3  read node blocks, merge-join    -> coordinates per way position
//! emit    ways + coordinates (parallel)   -> address, POI, street, interpolation records
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
mod nodes;
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
        plan::{BuildPlan, plan_build},
        progress::{BuildObserver, BuildStage, Progress, TerminalObserver},
        report::{
            BUILD_REPORT_SCHEMA_VERSION, BuilderReport, ReportInput, SlashNumberCounts,
            SorterReport,
        },
    },
    extsort::{ExternalSorter, SortStats, Spool},
    memory::Budget,
    pack::{AUDIT_DIR, BUILD_REPORT_FILE, REJECTIONS_FILE, RecordContext, Writer, WriterOptions},
    parse::lattice::{
        train::{self, Sampler},
        weights::{WEIGHTS_SECTION, WEIGHTS_VERSION},
    },
    record::{OSM_DATASET, Record, RejectedRecord},
    records::quantize,
    resources::Resources,
    text_index::PostcodeAreas,
    util::{geo::point_lon_lat, hilbert::hilbert_key},
};
use boundary::{BoundaryIndex, ContextOrigin, Vertex, build_boundaries, record_context};
use emit::{FeatureOutput, emit_features};
use emitted::Emitted;
use nodes::join_nodes;
pub use pbf::replication_timestamp;
use pbf::{for_each_block, input_bytes};
use postcode::PostcodeAccumulator;
use scan::scan_block;
use spill::{NodeRequest, PendingAddress, PendingRecord, ResolvedRef, WayFeature, WayKind};

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
        options.force,
    )
}

/// The bytes of every input together.
fn total_input_bytes(inputs: &[PathBuf]) -> Result<u64> {
    inputs
        .iter()
        .try_fold(0, |total, input| Ok(total + input_bytes(input)?))
}

pub fn build_osm_pack(options: BuildOsmOptions) -> Result<BuilderReport> {
    let plan = plan_osm_build(&options)?;
    build_planned_osm_pack(options, plan)
}

/// Build with a plan made by [`plan_osm_build`]. The caller sizes the
/// process's rayon pool to `plan.threads` before any parallel work, since
/// one pool serves every stage.
pub fn build_planned_osm_pack(options: BuildOsmOptions, plan: BuildPlan) -> Result<BuilderReport> {
    let started = Instant::now();
    for warning in plan.shortfalls.iter().chain(&plan.warnings) {
        options.observer.warned(warning);
    }
    let budget = usize::try_from(plan.memory_budget_bytes).unwrap_or(usize::MAX);
    // The sorters and the parser's lattice cache split what the others leave
    // of the pool by fair shares. While records are written the text index
    // is what every record waits on, so it takes half: more indexing
    // threads, fewer segments. The context record and spatial sorters hold
    // a few times less than the others, and are capped to stay so.
    let memory = Budget::new(budget);
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
                bytes: input_bytes(input)?,
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
            .saturating_sub(plan.memory_budget_bytes + plan.untracked_bytes(input_bytes))
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
        postcodes: PostcodeAccumulator::default(),
        records: ExternalSorter::new(&scratch, "records", &memory, budget),
        addresses: ExternalSorter::new(&scratch, "addresses", &memory, budget),
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
    build.report.resources.plan = plan;

    // Boundary geometry accumulates across every input, because a boundary
    // relation and the ways it is made of can only be assembled once all of
    // them are known. Everything else each pass needs is per input.
    let mut relations = Vec::new();
    // Boundary ways kept by pass 1. When one is also a relation member, its
    // vertices are reused instead of being requested a second time.
    let mut boundary_feature_ids = HashSet::new();
    let mut boundary_ways = Vec::new();
    let mut member_vertices: HashMap<i64, Option<Vec<Vertex>>> = HashMap::new();
    let mut request_stats = SortStats::default();
    let mut resolved_stats = SortStats::default();

    // Each input is read on its own: a pass reads its blocks in file order,
    // and the node join merges a request stream sorted by node id against
    // nodes that ascend only within one sorted PBF.
    for input in &options.inputs {
        // Pass 1: classify everything.
        let phase = Instant::now();
        let mut requests = ExternalSorter::new(&scratch, "node-requests", &memory, budget);
        let mut features = Spool::<WayFeature>::create(&scratch, "way-features")?;
        let mut node_blobs = Vec::new();
        let mut way_blobs = Vec::new();
        for_each_block(
            input,
            &progress,
            BuildStage::Scan,
            None,
            |offset, block| Ok((offset, scan_block(&block))),
            |(offset, output)| {
                if output.has_nodes {
                    node_blobs.push(offset);
                }
                if output.has_ways {
                    way_blobs.push(offset);
                }
                for (feature, refs) in output.ways {
                    let resolution = &mut build.report.geometry_resolution;
                    match feature.kind {
                        WayKind::Address => resolution.address_way_stubs += 1,
                        WayKind::Poi => resolution.poi_way_stubs += 1,
                        WayKind::Street => resolution.street_way_stubs += 1,
                        WayKind::Interpolation => resolution.interpolation_way_stubs += 1,
                        WayKind::Boundary => {
                            resolution.boundary_way_stubs += 1;
                            boundary_feature_ids.insert(feature.way_id);
                        }
                    }
                    request_nodes(
                        &mut requests,
                        features.written(),
                        &refs,
                        feature.kind == WayKind::Interpolation,
                    )?;
                    features.write(&feature)?;
                }
                relations.extend(output.relations);
                build.absorb(output.emitted)
            },
        )?;
        build.report.phases.scan_ms += phase.elapsed().as_millis();

        // Pass 2: the ways boundary relations are made of. They often carry no
        // tags of their own, so pass 1 could not know to keep them.
        let phase = Instant::now();
        let member_base = features.written();
        let member_ids = boundary::required_boundary_way_ids(&relations)
            .into_iter()
            .filter(|way_id| !boundary_feature_ids.contains(way_id))
            .collect::<HashSet<_>>();
        let mut members: HashMap<i64, (u64, usize)> = HashMap::new();
        if member_ids.is_empty() {
            progress.start(BuildStage::BoundaryMembers, Some(0))?;
            progress.finish(BuildStage::BoundaryMembers);
        } else {
            for_each_block(
                input,
                &progress,
                BuildStage::BoundaryMembers,
                Some(&way_blobs),
                |_, block| Ok(member_ways(&block, &member_ids)),
                |found| {
                    for (way_id, refs) in found {
                        if members.contains_key(&way_id) {
                            continue;
                        }
                        let owner = member_base + members.len() as u64;
                        request_nodes(&mut requests, owner, &refs, false)?;
                        members.insert(way_id, (owner, refs.len()));
                    }
                    Ok(())
                },
            )?;
        }
        build.report.phases.boundary_member_scan_ms += phase.elapsed().as_millis();

        // Pass 3: coordinates for every requested node.
        let phase = Instant::now();
        request_stats.merge(requests.stats());
        build.report.geometry_resolution.required_node_refs = request_stats.items;
        let mut resolved =
            ExternalSorter::<ResolvedRef>::new(&scratch, "resolved-nodes", &memory, budget);
        let join = join_nodes(
            input,
            &node_blobs,
            requests.finish()?,
            &mut resolved,
            &progress,
        )?;
        build.report.geometry_resolution.resolved_node_refs = join.resolved;
        build.report.phases.node_join_ms += phase.elapsed().as_millis();

        // Assemble ways into records.
        let phase = Instant::now();
        resolved_stats.merge(resolved.stats());
        progress.start(BuildStage::AssembleWays, Some(features.written()))?;
        let input_member_vertices = emit_features(
            features.into_reader()?,
            resolved.finish()?,
            |output: FeatureOutput| {
                progress.advance(1);
                progress.check()?;
                boundary_ways.extend(output.boundary_ways);
                build.absorb(output.emitted)
            },
        )?;
        progress.finish(BuildStage::AssembleWays);
        build.report.phases.feature_emission_ms += phase.elapsed().as_millis();

        // This input's member ways, keyed by way id so the boundary stage
        // can look them up once every input has been read. The owner ids
        // `emit_features` returns are this input's own, so they are resolved
        // here rather than kept.
        let mut input_member_vertices = input_member_vertices;
        for (way_id, (owner, count)) in members {
            let line = input_member_vertices
                .remove(&owner)
                .filter(|vertices| vertices.len() == count);
            member_vertices.insert(way_id, line);
        }
    }

    // Admin boundaries, their place records and postcode centroids. Every
    // input has been read, so a relation whose ways span two extracts is
    // assembled from all of them.
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
    let mut postcode_centroids = Vec::new();
    for record in std::mem::take(&mut build.postcodes).into_records() {
        build.report.accept_postcode();
        if let Some(point) = point_lon_lat(&record.geometry) {
            postcode_centroids.push((record.postcode.clone(), point));
        }
        build.push(Record::Postcode(record), None)?;
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

    let phase = Instant::now();
    let Build {
        mut report,
        audit,
        mut records,
        addresses,
        context_records,
        mut seq,
        ..
    } = build;
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
    // Each street's addresses join the records as one run, in number order,
    // placed in the Hilbert order by its first address: the run stays whole
    // for the record store and the text index to share what its addresses
    // state.
    let mut anchor = None;
    let mut previous: Option<PendingAddress> = None;
    let mut push = |address: PendingAddress| -> Result<()> {
        if previous
            .as_ref()
            .is_none_or(|previous| !previous.same_group(&address))
        {
            let [lon, lat] = address.record.display_point().ok_or_else(|| {
                Error::BadGeometry(format!(
                    "record {} has no display point",
                    address.record.id()
                ))
            })?;
            anchor = Some(hilbert_key(quantize(lon)?, quantize(lat)?));
        }
        records.push(PendingRecord {
            key: anchor.expect("a group has an anchor"),
            seq,
            origin: None,
            record: address.record.clone(),
        })?;
        seq += 1;
        previous = Some(address);
        Ok(())
    };
    if files.is_empty() {
        slash::read_slash_numbers(addresses.finish()?, &mut slash_numbers, &mut push)?;
    } else {
        for address in addresses.finish()? {
            push(address?)?;
        }
    }
    report.slash_numbers = slash_numbers;
    report.phases.address_order_ms = phase.elapsed().as_millis();

    // Context records first: once their ids are known, the boundary index can
    // describe every other record.
    let phase = Instant::now();
    let context_stats = context_records.stats();
    let record_stats = records.stats();
    let mut ordered = Spool::<PendingRecord>::create(&scratch, "context-ordered")?;
    let mut boundary_ids = vec![0; boundaries.len()];
    let mut country_ids = HashMap::new();
    let first_id = writer.record_count();
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
        ordered.write(&item)?;
    }
    writer.set_context_names(boundaries.context_names(&boundary_ids, &country_ids));
    writer.set_country_codes(boundaries.country_codes(&boundary_ids, &country_ids));
    writer.set_region_groups(boundaries.region_groups(&boundary_ids));
    writer.set_postcode_areas(PostcodeAreas::new(postcode_centroids));
    let index = boundaries.into_index(&boundary_ids, &country_ids);
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
        ("node_requests", request_stats),
        ("resolved_nodes", resolved_stats),
        ("addresses", address_stats),
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
    postcodes: PostcodeAccumulator,
    /// POIs, interpolations and streets, then the addresses.
    records: ExternalSorter<PendingRecord>,
    /// Addresses, until every one is known and they can join `records` a
    /// street at a time.
    addresses: ExternalSorter<PendingAddress>,
    /// Places and postcodes: everything a context tuple can point at.
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
            postcodes,
        } = emitted;
        self.report.merge(report);
        self.postcodes.merge(postcodes);
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

/// An imported address within this distance of an earlier address of the
/// same house (street, number and unit) repeats it. Sources place one house a
/// few metres apart, at the door, the parcel or the building's middle.
const REPEATED_HOUSE_RADIUS_M: f64 = 100.0;

/// The addresses again, without the imported rows that repeat an address
/// the Pack already has, OSM ones first: read in house order, then sorted
/// back into street groups. Each dropped row is counted on its file.
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
    let mut house: Option<PendingAddress> = None;
    let mut kept: Vec<[f64; 2]> = Vec::new();
    for address in houses.finish()? {
        let address = address?;
        if house
            .as_ref()
            .is_none_or(|house| !house.same_group(&address))
        {
            kept.clear();
        }
        let point = address.record.display_point().ok_or_else(|| {
            Error::BadGeometry(format!(
                "record {} has no display point",
                address.record.id()
            ))
        })?;
        let repeated = kept.iter().any(|[lon, lat]| {
            crate::spatial_index::haversine_m(*lon, *lat, point[0], point[1])
                <= REPEATED_HOUSE_RADIUS_M
        });
        let record = address.clone().into_address();
        if repeated && record.source.object_type == crate::record::ObjectType::Row {
            if let Some(import) = report
                .imports
                .iter_mut()
                .find(|import| import.dataset == record.source.dataset)
            {
                import.duplicates += 1;
            }
        } else {
            kept.push(point);
            grouped.push(PendingAddress::new(record, address.seq)?)?;
        }
        house = Some(address);
    }
    Ok(grouped)
}

fn request_nodes(
    requests: &mut ExternalSorter<NodeRequest>,
    owner: u64,
    refs: &[i64],
    want_tags: bool,
) -> Result<()> {
    for (pos, node_id) in refs.iter().enumerate() {
        requests.push(NodeRequest {
            node_id: *node_id,
            owner,
            pos: u32::try_from(pos)
                .map_err(|_| Error::BadInput("way has too many nodes".into()))?,
            want_tags,
        })?;
    }
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
        assert_eq!(report.accepted.postcode_records, 1);
        assert_eq!(report.geometry_resolution.street_way_stubs, 2);
        assert_eq!(
            report.scratch.sorters["records"].runs, 0,
            "a small input never spills"
        );
        assert!(report.scratch.peak_tracked_bytes > 0);
        assert!(report.scratch.peak_tracked_bytes <= report.scratch.memory_budget_bytes);

        // Context records come first, and each group is in Hilbert order, a
        // street's addresses placed at the first of them.
        let headers = (0..reader.manifest().record_count)
            .map(|id| reader.records().header(id).expect("header"))
            .collect::<Vec<_>>();
        let first_other = headers
            .iter()
            .position(|header| !header.layer.is_context())
            .expect("non-context records");
        assert!(
            headers[..first_other]
                .iter()
                .all(|header| header.layer.is_context())
        );
        assert!(
            headers[first_other..]
                .iter()
                .all(|header| !header.layer.is_context())
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
    fn spilling_to_disk_builds_the_same_pack() {
        let dir = temp_dir("spill");
        let input = dir.join("town.osm.pbf");
        write_pbf(&input, &town_with_pois());
        let in_memory = build(&dir.join("memory"), &input, None);
        // A 4-byte budget leaves every sorter one item of memory, so everything
        // goes through run files and multi-round merges.
        let spilled = build(&dir.join("spilled"), &input, Some(4));
        let spilling_sorters = spilled
            .scratch
            .sorters
            .values()
            .filter(|sorter| sorter.runs > 0)
            .count();
        assert!(spilling_sorters >= 4, "{:?}", spilled.scratch);
        assert!(spilled.scratch.spilled_bytes > 0);
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
