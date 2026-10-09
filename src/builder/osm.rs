//! Streaming OSM build.
//!
//! Memory stays bounded by the sorter budget whatever the input size: features,
//! node references, resolved coordinates and finished records all flow through
//! scratch files and external sorts. A region fits in the budget and never
//! touches disk; a planet spills and merges through the same code.
//!
//! ```text
//! pass 1  scan every block (parallel)    -> records for nodes, way features,
//!                                           node references, boundary relations
//! pass 2  read way blocks                 -> boundary relation member ways
//! pass 3  read node blocks, merge-join    -> coordinates per way position
//! emit    ways + coordinates (parallel)   -> address, POI, street, interpolation records
//! write   context records, then the rest, each in Hilbert order
//! ```

mod address;
mod boundary;
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
mod spill;
mod street;
mod tags;

#[cfg(test)]
mod test_pbf;

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;

use crate::{
    builder::{
        progress::{item_progress_bar, stage_progress},
        report::{BUILD_REPORT_SCHEMA_VERSION, BuilderReport, SorterReport},
    },
    extsort::{ExternalSorter, SortStats, Spool},
    memory::MemoryBudget,
    pack::{
        AUDIT_DIR, BUILD_REPORT_FILE, PackWriter, PackWriterOptions, REJECTIONS_FILE, RecordContext,
    },
    record::{Record, RejectedRecord},
    records::quantize,
    text_index::PostcodeAreas,
    util::{geo::point_lon_lat, hilbert::hilbert_key},
};
use boundary::{BoundaryIndex, ContextOrigin, Vertex, build_boundaries, record_context};
use emit::{FeatureOutput, emit_features};
use emitted::Emitted;
use nodes::join_nodes;
use pbf::{for_each_block, input_bytes};
use postcode::PostcodeAccumulator;
use scan::scan_block;
use spill::{NodeRequest, PendingRecord, ResolvedRef, WayFeature, WayKind};

/// Default memory for build sorters before they spill to disk.
pub const DEFAULT_MEMORY_BUDGET_BYTES: usize = 1 << 30;
/// Records written per parallel batch.
const WRITE_BATCH_RECORDS: usize = 16_384;

#[derive(Debug, Clone)]
pub struct BuildOsmOptions {
    pub input: PathBuf,
    pub pack: PathBuf,
    /// Memory for sort buffers. Inputs that need more spill to scratch files.
    pub memory_budget_bytes: usize,
    /// Directory for scratch files; defaults to inside the new Pack generation.
    pub scratch_dir: Option<PathBuf>,
}

impl Default for BuildOsmOptions {
    fn default() -> Self {
        Self {
            input: PathBuf::new(),
            pack: PathBuf::new(),
            memory_budget_bytes: DEFAULT_MEMORY_BUDGET_BYTES,
            scratch_dir: None,
        }
    }
}

pub fn build_osm_pack(options: BuildOsmOptions) -> Result<BuilderReport> {
    let started = Instant::now();
    if !options.input.is_file() {
        bail!("input {} does not exist", options.input.display());
    }
    // Up to four sorters hold memory at the same time (node references or
    // resolved coordinates, the two record sorters, the spatial pairs); the text
    // index buffers take the same share as one sorter while records are written.
    let sorter_budget = (options.memory_budget_bytes / 4).max(1);
    let memory = MemoryBudget::new(options.memory_budget_bytes);
    let mut writer = PackWriter::create_with(
        &options.pack,
        PackWriterOptions {
            scratch_dir: options.scratch_dir.clone(),
            memory: Arc::clone(&memory),
            sort_budget_bytes: sorter_budget,
            text_index_memory_bytes: sorter_budget,
        },
    )?;
    let scratch = Arc::clone(writer.scratch());
    let mut build = Build {
        report: BuilderReport {
            schema_version: BUILD_REPORT_SCHEMA_VERSION,
            input: options.input.display().to_string(),
            pack: options.pack.display().to_string(),
            input_bytes: input_bytes(&options.input)?,
            ..BuilderReport::default()
        },
        audit: Audit::create(writer.generation())?,
        postcodes: PostcodeAccumulator::default(),
        records: ExternalSorter::new(&scratch, "records", &memory, sorter_budget),
        context_records: ExternalSorter::new(
            &scratch,
            "context-records",
            &memory,
            sorter_budget / 4,
        ),
        seq: 0,
    };
    build.report.scratch.memory_budget_bytes = options.memory_budget_bytes as u64;

    // Pass 1: classify everything.
    let phase = Instant::now();
    let mut requests = ExternalSorter::new(&scratch, "node-requests", &memory, sorter_budget);
    let mut features = Spool::<WayFeature>::create(&scratch, "way-features")?;
    let mut relations = Vec::new();
    // Boundary ways kept by pass 1. When one is also a relation member, its
    // vertices are reused instead of being requested a second time.
    let mut boundary_feature_ids = HashSet::new();
    let mut node_blobs = Vec::new();
    let mut way_blobs = Vec::new();
    for_each_block(
        &options.input,
        "1/7 scan OSM objects",
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
    build.report.phases.scan_ms = phase.elapsed().as_millis();

    // Pass 2: the ways boundary relations are made of. They often carry no
    // tags of their own, so pass 1 could not know to keep them.
    let phase = Instant::now();
    let member_base = features.written();
    let member_ids = boundary::required_boundary_way_ids(&relations)
        .into_iter()
        .filter(|way_id| !boundary_feature_ids.contains(way_id))
        .collect::<HashSet<_>>();
    let mut members: HashMap<i64, (u64, usize)> = HashMap::new();
    if !member_ids.is_empty() {
        for_each_block(
            &options.input,
            "2/7 read boundary member ways",
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
    build.report.phases.boundary_member_scan_ms = phase.elapsed().as_millis();

    // Pass 3: coordinates for every requested node.
    let phase = Instant::now();
    let request_stats = requests.stats();
    build.report.geometry_resolution.required_node_refs = request_stats.items;
    let mut resolved =
        ExternalSorter::<ResolvedRef>::new(&scratch, "resolved-nodes", &memory, sorter_budget);
    let join = join_nodes(
        &options.input,
        &node_blobs,
        requests.finish()?,
        &mut resolved,
    )?;
    build.report.geometry_resolution.resolved_node_refs = join.resolved;
    build.report.phases.node_join_ms = phase.elapsed().as_millis();

    // Assemble ways into records.
    let phase = Instant::now();
    let resolved_stats = resolved.stats();
    let mut boundary_ways = Vec::new();
    let progress = item_progress_bar(features.written(), "4/7 assemble ways");
    let mut member_vertices = emit_features(
        features.into_reader()?,
        resolved.finish()?,
        |output: FeatureOutput| {
            progress.inc(1);
            boundary_ways.extend(output.boundary_ways);
            build.absorb(output.emitted)
        },
    )?;
    progress.finish_with_message("4/7 assemble ways complete");
    build.report.phases.feature_emission_ms = phase.elapsed().as_millis();

    // Admin boundaries, their place records and postcode centroids.
    let phase = Instant::now();
    let member_vertices: HashMap<i64, Option<Vec<Vertex>>> = members
        .into_iter()
        .map(|(way_id, (owner, count))| {
            let line = member_vertices
                .remove(&owner)
                .filter(|vertices| vertices.len() == count);
            (way_id, line)
        })
        .collect();
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
    let boundaries = build_boundaries(&boundary_ways, &relations, &member_lines);
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
    build.report.phases.boundary_build_ms = phase.elapsed().as_millis();

    // Context records first: once their ids are known, the boundary index can
    // describe every other record.
    let phase = Instant::now();
    let Build {
        mut report,
        audit,
        records,
        context_records,
        ..
    } = build;
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
    writer.set_postcode_areas(PostcodeAreas::new(postcode_centroids));
    let index = boundaries.into_index(&boundary_ids, &country_ids);
    let context_count = ordered.written();
    write_ordered(
        &mut writer,
        ordered.into_reader()?,
        context_count,
        &index,
        "5/7 write context records",
    )?;
    report.phases.context_record_write_ms = phase.elapsed().as_millis();

    let phase = Instant::now();
    write_ordered(
        &mut writer,
        records.finish()?,
        record_stats.items,
        &index,
        "6/7 write records",
    )?;
    drop(index);
    report.phases.record_write_ms = phase.elapsed().as_millis();

    let phase = Instant::now();
    report.scratch.spilled_bytes = scratch.spilled_bytes();
    report.scratch.peak_tracked_bytes = memory.peak() as u64;
    drop(scratch);
    let progress = stage_progress("7/7 finish Pack indexes");
    let (sealed, stats) = writer.seal()?;
    progress.finish_with_message("7/7 finish Pack indexes complete");
    report.phases.pack_seal_ms = phase.elapsed().as_millis();

    for (name, stats) in [
        ("node_requests", request_stats),
        ("resolved_nodes", resolved_stats),
        ("records", record_stats),
        ("context_records", context_stats),
        (
            "spatial_pairs",
            SortStats {
                items: 0,
                runs: stats.spatial_pair_runs,
            },
        ),
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
    /// Addresses, POIs, interpolations and streets.
    records: ExternalSorter<PendingRecord>,
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
        let [lon, lat] = record
            .display_point()
            .with_context(|| format!("record {} has no display point", record.id()))?;
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
            pos: u32::try_from(pos).context("way has too many nodes")?,
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

/// Write sorted records in batches, computing admin context in parallel.
/// Boundary and derived-country records describe context and get none.
fn write_ordered(
    writer: &mut PackWriter,
    records: impl Iterator<Item = Result<PendingRecord>>,
    total: u64,
    index: &BoundaryIndex,
    message: &'static str,
) -> Result<()> {
    let progress = item_progress_bar(total, message);
    let mut records = records.peekable();
    while records.peek().is_some() {
        let batch = records
            .by_ref()
            .take(WRITE_BATCH_RECORDS)
            .collect::<Result<Vec<_>>>()?;
        let batch = batch
            .into_par_iter()
            .map(|pending| {
                let context: Option<RecordContext> = match pending.origin {
                    Some(_) => None,
                    None => record_context(index, &pending.record),
                };
                (pending.record, context)
            })
            .collect::<Vec<_>>();
        writer.write_batch(&batch)?;
        progress.inc(batch.len() as u64);
    }
    progress.finish_with_message(format!("{message} complete"));
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
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let path = dir.join(REJECTIONS_FILE);
        let file =
            File::create(&path).with_context(|| format!("failed to create {}", path.display()))?;
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
        let file =
            File::create(&path).with_context(|| format!("failed to create {}", path.display()))?;
        serde_json::to_writer_pretty(BufWriter::new(file), report)
            .with_context(|| format!("failed to write {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        pack::{PackReader, RecordPointPrecision},
        record::Record,
        reverse::{PackReverseGeocoder, ReverseGeocodeOptions, ReverseMatchKind},
        search::{PackTextSearcher, TextAutocompleteOptions, TextSearchHit, TextSearchOptions},
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

    fn build(root: &Path, input: &Path, memory_budget_bytes: usize) -> BuilderReport {
        build_osm_pack(BuildOsmOptions {
            input: input.to_path_buf(),
            pack: root.to_path_buf(),
            memory_budget_bytes,
            scratch_dir: None,
        })
        .expect("build")
    }

    #[test]
    fn builds_a_searchable_reverse_geocodable_pack_from_pbf() {
        let dir = temp_dir("town");
        let input = dir.join("town.osm.pbf");
        write_pbf(&input, &town());
        let report = build(&dir.join("pack"), &input, DEFAULT_MEMORY_BUDGET_BYTES);

        let reader = PackReader::open(dir.join("pack")).expect("reader");
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

        // Context records come first, and each group is in Hilbert order.
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
        for group in [&headers[..first_other], &headers[first_other..]] {
            let keys = group
                .iter()
                .map(|header| hilbert_key(header.lon_e7, header.lat_e7))
                .collect::<Vec<_>>();
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
        assert_eq!(name(context.admin_context.locality_record_id), "Toronto");
        assert_eq!(name(context.admin_context.region_record_id), "Ontario");
        assert_eq!(name(context.admin_context.country_record_id), "Canada");

        let searcher = PackTextSearcher::open(dir.join("pack")).expect("searcher");
        let hits = searcher
            .search(TextSearchOptions {
                query: "10 King Street".into(),
                limit: 1,
                layer: Some("address".into()),
            })
            .expect("search");
        assert_eq!(hits[0].record.id, "osm:node:1");
        assert_eq!(hits[0].record.label, "10 King Street, Toronto, M5V 1A1");

        let reverse = PackReverseGeocoder::open(dir.join("pack")).expect("reverse");
        let result = |lon, lat| {
            reverse
                .reverse(ReverseGeocodeOptions { lon, lat })
                .expect("reverse")
                .result
                .expect("result")
        };
        let building = result(-79.4299, 43.6201);
        assert_eq!(building.match_kind, ReverseMatchKind::ExplicitAddress);
        assert_eq!(building.id.as_deref(), Some("osm:way:602"));
        let estimated = result(-79.44001, 43.6305);
        assert_eq!(estimated.match_kind, ReverseMatchKind::EstimatedAddress);
        assert!(
            estimated.label.starts_with("6 Elm Street"),
            "{}",
            estimated.label
        );
        let street = result(-79.415, 43.61001);
        assert_eq!(street.match_kind, ReverseMatchKind::NearestStreet);
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
        let report = build(&dir.join("pack"), &input, DEFAULT_MEMORY_BUDGET_BYTES);

        let reader = PackReader::open(dir.join("pack")).expect("reader");
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

        let searcher = PackTextSearcher::open(dir.join("pack")).expect("searcher");
        let search = |query: &str, layer: Option<&str>| -> Vec<TextSearchHit> {
            searcher
                .search(TextSearchOptions {
                    query: query.into(),
                    limit: 5,
                    layer: layer.map(str::to_string),
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
            RecordPointPrecision::Centroid
        );
        assert!(search("Memorial Bench", None).is_empty());

        let suggestions = searcher
            .autocomplete(TextAutocompleteOptions {
                query: "riverdale fa".into(),
                limit: 5,
                layer: None,
            })
            .expect("autocomplete");
        assert_eq!(ids(&suggestions), vec!["osm:node:2002"]);

        // Reverse answers with the cafe's address, as it did when the cafe was
        // an address record.
        let reverse = PackReverseGeocoder::open(dir.join("pack")).expect("reverse");
        let result = reverse
            .reverse(ReverseGeocodeOptions {
                lon: -79.38,
                lat: 43.65,
            })
            .expect("reverse")
            .result
            .expect("result");
        assert_eq!(result.match_kind, ReverseMatchKind::ExplicitAddress);
        assert_eq!(result.id.as_deref(), Some("osm:node:2001"));
        assert_eq!(result.label, "123 King Street West, M5V 1A1");
        assert_eq!(result.context.locality.as_deref(), Some("Toronto"));
    }

    #[test]
    fn spilling_to_disk_builds_the_same_pack() {
        let dir = temp_dir("spill");
        let input = dir.join("town.osm.pbf");
        write_pbf(&input, &town_with_pois());
        let in_memory = build(&dir.join("memory"), &input, DEFAULT_MEMORY_BUDGET_BYTES);
        // A 4-byte budget leaves every sorter one item of memory, so everything
        // goes through run files and multi-round merges.
        let spilled = build(&dir.join("spilled"), &input, 4);
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

        let memory = PackReader::open(dir.join("memory")).expect("memory pack");
        let spilled = PackReader::open(dir.join("spilled")).expect("spilled pack");
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
            input,
            pack: dir.join("pack"),
            ..BuildOsmOptions::default()
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("osmium sort"), "{error:#}");
        assert!(!dir.join("pack").join("CURRENT").exists());
    }
}
