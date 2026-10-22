use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    builder::{osm::LOCALITY_KEYS, plan::BuildPlan},
    extsort::Traffic,
    parse::lattice::train::Report,
    record::{AddressRecord, LocationPrecision, ObjectType, PlaceLayer, PoiRecord},
    resources::Resources,
    util::geo::point_lon_lat,
};

pub const BUILD_REPORT_SCHEMA_VERSION: u32 = 22;

/// One extract a build read.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ReportInput {
    pub path: String,
    /// What the input is, as the caller named it: a region id for a Pack
    /// built from a catalog region.
    pub source: Option<String>,
    /// When the input's data was taken from OpenStreetMap, in seconds since
    /// the Unix epoch, as the PBF header states it.
    pub replication_timestamp: Option<i64>,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct BuilderReport {
    pub schema_version: u32,
    /// The extracts the Pack was built from, in the order they were read.
    /// A Pack covering several regions has one entry per region.
    pub inputs: Vec<ReportInput>,
    pub pack: String,
    /// The bytes of every input together.
    pub input_bytes: u64,
    /// The datasets the Pack's records come from, by the id their
    /// provenance names ([`crate::record::OSM_DATASET`]).
    pub datasets: Vec<String>,
    pub scanned: ScannedCounts,
    pub accepted: AcceptedCounts,
    pub rejected: RejectedCounts,
    pub disposition: CandidateDispositionCounts,
    pub validation: ValidationAuditCounts,
    pub quality: AcceptedQualityReport,
    pub triage: RejectionTriageReport,
    pub geometry_resolution: GeometryResolutionCounts,
    pub geometry_passes: GeometryPassReport,
    pub completeness: CompletenessCounts,
    pub phases: PhaseTimings,
    pub throughput: Throughput,
    pub resources: ResourceReport,
    pub scratch: ScratchReport,
    pub output: PackOutputReport,
    /// The address parser trained on the Pack's records.
    pub parser_training: Report,
    /// Address files added from outside OSM, such as OpenAddresses, one
    /// entry per file.
    pub imports: Vec<crate::builder::addresses::ImportReport>,
    /// Ways and relations libpostal's admin level table names for the
    /// input's countries that the input lacks: rules that apply to nothing.
    pub unmatched_admin_level_ids: Vec<String>,
    pub slash_numbers: SlashNumberCounts,
    /// What each stage cost, in the order they ran; a stage run once per
    /// input has one entry per input.
    pub stages: Vec<StageReport>,
    /// The checkpoints the build wrote, every run's together.
    #[serde(default)]
    pub checkpoints: CheckpointReport,
    /// Each time the build went on from a checkpoint a run before left.
    #[serde(default)]
    pub resumes: Vec<ResumeReport>,
}

/// What checkpointing cost.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CheckpointReport {
    pub count: u64,
    /// Time spent writing them: spilling sorter buffers, saving state and
    /// syncing the files they name.
    pub wall_ms: u64,
    /// The stage the last one completed.
    pub last: Option<String>,
    /// Most bytes held on disk at once for a checkpoint that the build was
    /// done with: inputs a stage consumed, kept until the stage's own
    /// checkpoint replaced the one before. Not live: given up first when
    /// the scratch disk runs short.
    pub peak_retained_bytes: u64,
    /// The stage in which retained files were given up to make room, so
    /// the build could not have gone on from the checkpoint before it.
    pub voided: Option<String>,
}

/// A run that went on from the checkpoint another left.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ResumeReport {
    /// The last stage the stopped run completed.
    pub after: String,
    /// Scratch bytes it went on with, by owner: sorters, spools, the node
    /// store, the address books, the Pack writer's parts.
    pub reused: BTreeMap<String, u64>,
    pub reused_bytes: u64,
    /// Build time up to the checkpoint, not spent again.
    pub saved_ms: u64,
}

/// How the build read the house numbers written "N/M".
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SlashNumberCounts {
    /// Read as UNIT/NUMBER: stored as the number, with the unit.
    pub unit_first: u64,
    /// Read as a number first: a range, a corner or a building and its flat.
    pub number_first: u64,
    /// No neighbours to read them by: kept as written.
    pub unresolved: u64,
}

impl BuilderReport {
    /// Derive build throughput from the final wall-clock and record counts.
    /// Call once `phases.total_ms` is final.
    pub(crate) fn finalize_throughput(&mut self) {
        let secs = self.phases.total_ms as f64 / 1000.0;
        if secs <= 0.0 {
            return;
        }
        self.throughput.records_per_sec = self.output.record_count as f64 / secs;
        self.throughput.addresses_per_sec =
            *self.accepted.by_layer.get("address").unwrap_or(&0) as f64 / secs;
    }

    /// Fold a report produced by a parallel worker into this one. Workers cover
    /// consecutive input ranges and are merged in input order, so capped sample
    /// lists keep the same entries a sequential build would.
    pub(crate) fn merge(&mut self, other: BuilderReport) {
        let BuilderReport {
            scanned,
            accepted,
            rejected,
            disposition,
            validation,
            quality,
            triage,
            geometry_resolution,
            completeness,
            ..
        } = other;
        self.scanned.nodes += scanned.nodes;
        self.scanned.dense_nodes += scanned.dense_nodes;
        self.scanned.ways += scanned.ways;
        self.scanned.relations += scanned.relations;

        self.accepted.total += accepted.total;
        merge_counts(&mut self.accepted.by_layer, accepted.by_layer);
        self.accepted.node_addresses += accepted.node_addresses;
        self.accepted.way_centroid_addresses += accepted.way_centroid_addresses;
        self.accepted.interpolation_ranges += accepted.interpolation_ranges;
        self.accepted.street_segments += accepted.street_segments;
        self.accepted.postcode_records += accepted.postcode_records;
        self.accepted.place_nodes += accepted.place_nodes;
        self.accepted.place_areas += accepted.place_areas;
        self.accepted.poi_nodes += accepted.poi_nodes;
        self.accepted.poi_way_centroids += accepted.poi_way_centroids;
        self.accepted.pois_with_address += accepted.pois_with_address;
        merge_counts(&mut self.accepted.poi_categories, accepted.poi_categories);

        self.rejected.total += rejected.total;
        merge_counts(&mut self.rejected.by_reason, rejected.by_reason);

        self.disposition.invalid += disposition.invalid;
        self.disposition.out_of_scope += disposition.out_of_scope;
        self.disposition.unsupported += disposition.unsupported;
        self.disposition.unresolved_geometry += disposition.unresolved_geometry;
        merge_counts(&mut self.disposition.by_reason, disposition.by_reason);

        self.validation
            .missing_housenumber
            .merge(validation.missing_housenumber);
        self.validation
            .missing_street_or_place
            .merge(validation.missing_street_or_place);
        self.validation
            .unsupported_relation
            .merge(validation.unsupported_relation);
        self.validation
            .unresolved_geometry
            .merge(validation.unresolved_geometry);
        self.validation
            .interpolation
            .merge(validation.interpolation);

        self.quality.addresses.merge(quality.addresses);
        merge_samples(
            &mut self.quality.samples,
            quality.samples,
            MAX_ACCEPTED_QUALITY_SAMPLES_PER_BUCKET,
        );

        merge_counts(&mut self.triage.by_bucket, triage.by_bucket);
        for (reason, buckets) in triage.by_reason_and_bucket {
            merge_counts(
                self.triage.by_reason_and_bucket.entry(reason).or_default(),
                buckets,
            );
        }
        let gaps = triage.street_name_gaps;
        merge_counts(
            &mut self.triage.street_name_gaps.by_highway,
            gaps.by_highway,
        );
        merge_counts(
            &mut self.triage.street_name_gaps.by_service,
            gaps.by_service,
        );
        merge_counts(&mut self.triage.street_name_gaps.by_bucket, gaps.by_bucket);
        merge_samples(
            &mut self.triage.samples,
            triage.samples,
            MAX_REJECTION_SAMPLES_PER_REASON_BUCKET,
        );

        let geometry = geometry_resolution;
        self.geometry_resolution.address_way_stubs += geometry.address_way_stubs;
        self.geometry_resolution.interpolation_way_stubs += geometry.interpolation_way_stubs;
        self.geometry_resolution.street_way_stubs += geometry.street_way_stubs;
        self.geometry_resolution.boundary_way_stubs += geometry.boundary_way_stubs;
        self.geometry_resolution.poi_way_stubs += geometry.poi_way_stubs;
        self.geometry_resolution.required_node_refs += geometry.required_node_refs;
        self.geometry_resolution.resolved_node_refs += geometry.resolved_node_refs;

        self.completeness.city += completeness.city;
        self.completeness.postcode += completeness.postcode;
        self.completeness.state += completeness.state;
        self.completeness.country += completeness.country;
    }
}

fn merge_counts(target: &mut BTreeMap<String, u64>, source: BTreeMap<String, u64>) {
    for (key, count) in source {
        *target.entry(key).or_default() += count;
    }
}

fn merge_samples<T>(
    target: &mut BTreeMap<String, Vec<T>>,
    source: BTreeMap<String, Vec<T>>,
    cap: usize,
) {
    for (bucket, samples) in source {
        let existing = target.entry(bucket).or_default();
        let room = cap.saturating_sub(existing.len());
        existing.extend(samples.into_iter().take(room));
    }
}

/// What the machine gave the build.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ResourceReport {
    /// The probe, the sizes chosen from it and the pre-flight check.
    pub plan: BuildPlan,
    /// Worker threads the build ran on.
    pub threads: usize,
    /// The probe again when the parser's training started.
    pub at_training: Resources,
}

/// Disk used for build scratch files and how often each sorter spilled.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ScratchReport {
    pub memory_budget_bytes: u64,
    /// Most memory held at once by sort, merge, text indexing and lattice
    /// buffers.
    pub peak_tracked_bytes: u64,
    /// Most bytes the heap held at once, as the global allocator counts
    /// them; 0 when it does not.
    pub peak_heap_bytes: u64,
    /// Bytes written to scratch files, as on disk.
    pub spilled_bytes: u64,
    pub sorters: BTreeMap<String, SorterReport>,
    /// The `--scratch-budget-mb` cap; `None` when only the scratch volume's
    /// free space capped the build.
    pub budget_bytes: Option<u64>,
    /// Most scratch bytes on disk at once, every owner together.
    pub peak_live_bytes: u64,
    /// What each owner held when that peak was reached.
    pub peak_owners: BTreeMap<String, u64>,
    /// The zstd level spill blocks ended at: above 1 when the ledger ran
    /// short and blocks were compressed harder.
    pub zstd_level: i32,
    /// Every scratch owner's traffic: sorters and spools by label, the Pack
    /// writer's files as `pack-*`.
    pub spills: BTreeMap<String, Traffic>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SorterReport {
    pub items: u64,
    pub runs: u64,
}

/// Which resource held a stage back, judged from what it measured.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Bound {
    /// The stage kept the threads busy, or one thread on its serial path.
    Cpu,
    /// Waiting on the input or on scratch being read.
    ReadIo,
    /// Writing scratch.
    WriteIo,
    /// None of these was saturated: the stage waited on another stage, a
    /// lock or the machine.
    #[default]
    Waiting,
}

impl Bound {
    /// A share past which a resource counts as what held the stage back.
    const SATURATED: f64 = 0.5;

    /// The resource with the largest share, if it is saturated. A stage
    /// whose driving thread was busy all along is bound by that thread's
    /// CPU, however idle the others were.
    pub fn classify(shares: &BoundShares) -> Self {
        let cpu = shares.cpu.max(shares.driver);
        [
            (Self::Cpu, cpu),
            (Self::ReadIo, shares.read),
            (Self::WriteIo, shares.write),
        ]
        .into_iter()
        .filter(|(_, share)| *share >= Self::SATURATED)
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .map_or(Self::Waiting, |(bound, _)| bound)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::ReadIo => "read-io",
            Self::WriteIo => "write-io",
            Self::Waiting => "waiting",
        }
    }
}

/// The shares of a stage's wall time each resource was busy, from 0 to 1.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
pub struct BoundShares {
    /// CPU time over wall time and threads.
    pub cpu: f64,
    /// CPU time over wall time: threads kept busy on average.
    pub cores: f64,
    /// CPU time of the thread that drives the stage, its sink, over wall
    /// time: near 1 when the stage waits on that one thread.
    pub driver: f64,
    /// Time reading the input or waiting for scratch to be read ahead.
    pub read: f64,
    /// Time writing scratch, over every writing thread.
    pub write: f64,
    /// Time a producer waited for its previous buffer to be written.
    pub spill_wait: f64,
}

/// Bytes a stage read from one input.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct InputRead {
    pub bytes: u64,
    /// Time spent in reads, on the thread that reads.
    pub wait_ms: u64,
}

/// What one stage of the build cost.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct StageReport {
    /// The stage, as progress names it.
    pub stage: String,
    pub wall_ms: u64,
    /// Process CPU time, user and kernel, over every thread.
    pub cpu_ms: u64,
    pub threads: usize,
    /// The stage's work in its own unit ("bytes", "blocks", "ways",
    /// "records"), or none.
    pub unit: String,
    pub units: u64,
    /// Bytes read from each input file.
    pub inputs: BTreeMap<String, InputRead>,
    /// Scratch traffic by owner, of owners that wrote or read in the stage.
    pub spills: BTreeMap<String, Traffic>,
    pub items_written: u64,
    pub items_read: u64,
    pub scratch_raw_written_bytes: u64,
    pub scratch_written_bytes: u64,
    pub scratch_raw_read_bytes: u64,
    pub scratch_read_bytes: u64,
    pub peak_scratch_bytes: u64,
    /// Most of the process in RAM, sampled every second.
    pub peak_rss_bytes: u64,
    /// Most of the memory budget reserved at once.
    pub peak_budget_bytes: u64,
    pub shares: BoundShares,
    pub bound: Bound,
}

impl StageReport {
    fn input_bytes(&self) -> u64 {
        self.inputs.values().map(|input| input.bytes).sum()
    }
}

impl BuilderReport {
    /// What each stage cost, as a table for a person to read at the end of
    /// a build.
    pub fn stage_table(&self) -> String {
        let mib = |bytes: u64| bytes as f64 / f64::from(1 << 20);
        let mut table = format!(
            "{:<28} {:>8} {:>6} {:>9} {:>17} {:>17} {:>9} {:>8} {:>8}  {}\n",
            "stage",
            "wall s",
            "cores",
            "in MB/s",
            "spill w MiB (raw)",
            "spill r MiB (raw)",
            "scratch",
            "RSS",
            "budget",
            "bound"
        );
        for stage in &self.stages {
            let secs = stage.wall_ms as f64 / 1000.0;
            let rate = if secs > 0.0 {
                stage.input_bytes() as f64 / 1e6 / secs
            } else {
                0.0
            };
            table.push_str(&format!(
                "{:<28} {:>8.1} {:>6.1} {:>9.0} {:>8.0} ({:>6.0}) {:>8.0} ({:>6.0}) {:>8.0}M {:>7.0}M {:>7.0}M  {}\n",
                stage.stage,
                secs,
                stage.shares.cores,
                rate,
                mib(stage.scratch_written_bytes),
                mib(stage.scratch_raw_written_bytes),
                mib(stage.scratch_read_bytes),
                mib(stage.scratch_raw_read_bytes),
                mib(stage.peak_scratch_bytes),
                mib(stage.peak_rss_bytes),
                mib(stage.peak_budget_bytes),
                stage.bound.as_str(),
            ));
        }
        let scratch = &self.scratch;
        let owners = scratch
            .peak_owners
            .iter()
            .map(|(owner, bytes)| format!("{owner} {:.0} MiB", mib(*bytes)))
            .collect::<Vec<_>>()
            .join(", ");
        table.push_str(&format!(
            "peak scratch {:.0} MiB ({owners}); {:.0} MiB written; zstd level {}\n",
            mib(scratch.peak_live_bytes),
            mib(scratch.spilled_bytes),
            scratch.zstd_level,
        ));
        let checkpoints = &self.checkpoints;
        table.push_str(&format!(
            "{} checkpoints, {:.1} s; at most {:.0} MiB kept for them past use{}
",
            checkpoints.count,
            checkpoints.wall_ms as f64 / 1000.0,
            mib(checkpoints.peak_retained_bytes),
            checkpoints
                .voided
                .as_ref()
                .map(|stage| format!(", given up for room in stage {stage}"))
                .unwrap_or_default(),
        ));
        for resume in &self.resumes {
            table.push_str(&format!(
                "resumed after \"{}\": {:.0} MiB reused, {:.1} s not run again
",
                resume.after,
                mib(resume.reused_bytes),
                resume.saved_ms as f64 / 1000.0,
            ));
        }
        table
    }
}

/// What the build wrote.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct PackOutputReport {
    pub pack_bytes: u64,
    pub record_count: u64,
    pub text_index_bytes: u64,
    pub rejection_count: u64,
    pub sections: BTreeMap<String, u64>,
}

/// Build throughput derived from `phases.total_ms` (records/addresses per second).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Throughput {
    pub records_per_sec: f64,
    pub addresses_per_sec: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ScannedCounts {
    pub nodes: u64,
    pub dense_nodes: u64,
    pub ways: u64,
    pub relations: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AcceptedCounts {
    pub total: u64,
    pub by_layer: BTreeMap<String, u64>,
    pub node_addresses: u64,
    pub way_centroid_addresses: u64,
    pub interpolation_ranges: u64,
    pub street_segments: u64,
    /// Street records, the segments of each street joined into one.
    #[serde(default)]
    pub streets: u64,
    pub postcode_records: u64,
    pub place_nodes: u64,
    /// Places drawn as a closed way or a multipolygon, not admin boundaries.
    #[serde(default)]
    pub place_areas: u64,
    pub poi_nodes: u64,
    pub poi_way_centroids: u64,
    /// POIs that carry an address instead of being an address record.
    pub pois_with_address: u64,
    /// POIs by the key of their category, for example `amenity`.
    pub poi_categories: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RejectedCounts {
    pub total: u64,
    pub by_reason: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CandidateDispositionCounts {
    pub invalid: u64,
    pub out_of_scope: u64,
    pub unsupported: u64,
    pub unresolved_geometry: u64,
    pub by_reason: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ValidationAuditCounts {
    pub missing_housenumber: IssueAuditCounts,
    pub missing_street_or_place: IssueAuditCounts,
    pub unsupported_relation: IssueAuditCounts,
    pub unresolved_geometry: IssueAuditCounts,
    pub interpolation: IssueAuditCounts,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AcceptedQualityReport {
    pub addresses: AcceptedAddressQuality,
    pub samples: BTreeMap<String, Vec<AcceptedRecordSample>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AcceptedAddressQuality {
    pub total: u64,
    pub has_street: u64,
    pub has_place: u64,
    pub has_unit: u64,
    pub has_locality: u64,
    pub missing_locality: u64,
    pub has_region: u64,
    pub missing_region: u64,
    pub has_postcode: u64,
    pub missing_postcode: u64,
    pub has_country: u64,
    pub missing_country: u64,
    pub has_full_admin_context: u64,
    pub missing_any_admin_context: u64,
    pub enrichable_by_point: u64,
    pub not_enrichable_by_point: u64,
    pub by_location_precision: BTreeMap<String, u64>,
    pub by_context_shape: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptedRecordSample {
    pub bucket: String,
    pub layer: String,
    pub id: String,
    pub label: String,
    pub source_id: String,
    pub object_type: ObjectType,
    pub object_id: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_fields: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RejectionTriageReport {
    pub by_bucket: BTreeMap<String, u64>,
    pub by_reason_and_bucket: BTreeMap<String, BTreeMap<String, u64>>,
    pub street_name_gaps: StreetNameGapCounts,
    pub samples: BTreeMap<String, Vec<RejectionSample>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct StreetNameGapCounts {
    pub by_highway: BTreeMap<String, u64>,
    pub by_service: BTreeMap<String, u64>,
    pub by_bucket: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RejectionSample {
    pub reason: String,
    pub bucket: String,
    pub note: String,
    pub object_type: ObjectType,
    pub object_id: i64,
    pub source_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_hint: Option<String>,
    pub writes_rejection_record: bool,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct IssueAuditCounts {
    pub by_shape: BTreeMap<String, u64>,
    pub by_feature_context: BTreeMap<String, u64>,
    pub by_object_type: BTreeMap<String, u64>,
    pub by_addr_key: BTreeMap<String, u64>,
}

impl IssueAuditCounts {
    fn merge(&mut self, other: IssueAuditCounts) {
        merge_counts(&mut self.by_shape, other.by_shape);
        merge_counts(&mut self.by_feature_context, other.by_feature_context);
        merge_counts(&mut self.by_object_type, other.by_object_type);
        merge_counts(&mut self.by_addr_key, other.by_addr_key);
    }
}

impl AcceptedAddressQuality {
    fn merge(&mut self, other: AcceptedAddressQuality) {
        self.total += other.total;
        self.has_street += other.has_street;
        self.has_place += other.has_place;
        self.has_unit += other.has_unit;
        self.has_locality += other.has_locality;
        self.missing_locality += other.missing_locality;
        self.has_region += other.has_region;
        self.missing_region += other.missing_region;
        self.has_postcode += other.has_postcode;
        self.missing_postcode += other.missing_postcode;
        self.has_country += other.has_country;
        self.missing_country += other.missing_country;
        self.has_full_admin_context += other.has_full_admin_context;
        self.missing_any_admin_context += other.missing_any_admin_context;
        self.enrichable_by_point += other.enrichable_by_point;
        self.not_enrichable_by_point += other.not_enrichable_by_point;
        merge_counts(&mut self.by_location_precision, other.by_location_precision);
        merge_counts(&mut self.by_context_shape, other.by_context_shape);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GeometryResolutionCounts {
    pub address_way_stubs: u64,
    pub interpolation_way_stubs: u64,
    pub street_way_stubs: u64,
    pub boundary_way_stubs: u64,
    pub poi_way_stubs: u64,
    pub required_node_refs: u64,
    pub resolved_node_refs: u64,
}

/// What the passes that give ways their geometry held and read, over every
/// input.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GeometryPassReport {
    /// Ways the scan kept for their geometry.
    pub kept_ways: u64,
    /// Vertices of boundary ways and boundary relation members.
    pub boundary_refs: u64,
    /// Distinct nodes kept ways and member ways reference.
    pub needed_nodes: u64,
    /// Needed nodes the inputs have: one stored coordinate each.
    pub stored_nodes: u64,
    /// Nodes whose address tags interpolation ways read.
    pub tagged_nodes: u64,
    /// Most bytes one input's node id bitmap and rank index held.
    pub peak_node_set_bytes: u64,
    /// Most bytes one input's coordinates held.
    pub peak_location_bytes: u64,
    /// Coordinate bytes held in scratch files instead of memory, over every
    /// input.
    pub mapped_location_bytes: u64,
    /// PBF bytes each pass read: the scan reads every byte, the others only
    /// the blocks they need.
    pub scan_read_bytes: u64,
    pub boundary_member_read_bytes: u64,
    pub node_read_bytes: u64,
    pub way_read_bytes: u64,
}

impl GeometryPassReport {
    pub(crate) fn add(&mut self, stored: &crate::builder::osm::LoadStats) {
        self.needed_nodes += stored.needed;
        self.stored_nodes += stored.stored;
        self.tagged_nodes += stored.tagged;
        self.peak_node_set_bytes = self.peak_node_set_bytes.max(stored.set_bytes);
        self.peak_location_bytes = self.peak_location_bytes.max(stored.location_bytes);
        if stored.mapped {
            self.mapped_location_bytes += stored.location_bytes;
        }
        self.node_read_bytes += stored.read_bytes;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CompletenessCounts {
    pub city: u64,
    pub postcode: u64,
    pub state: u64,
    pub country: u64,
}

/// Wall-clock time per build phase.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct PhaseTimings {
    /// Pass 1: classify every OSM object.
    pub scan_ms: u128,
    /// Pass 2: read the ways that make up boundary relations.
    pub boundary_member_scan_ms: u128,
    /// Pass 3: store the coordinates of the nodes ways need.
    pub node_join_ms: u128,
    /// Pass 4: read the kept ways again and turn them into records.
    pub feature_emission_ms: u128,
    /// Build admin boundary polygons.
    pub boundary_build_ms: u128,
    /// Fetch and read address files from outside OSM.
    pub address_import_ms: u128,
    /// Order every address by street and number, drop the imported ones
    /// that repeat another, lend each street's postcodes to its addresses
    /// without one, and place each country's postcodes.
    pub address_order_ms: u128,
    /// Write places and boundaries.
    pub context_record_write_ms: u128,
    /// Join the ways of each street into one street record.
    #[serde(default)]
    pub street_join_ms: u128,
    /// Write addresses, POIs, interpolations, streets and postcodes in
    /// Hilbert order.
    pub record_write_ms: u128,
    /// Finish the text and spatial indexes and write the Pack file.
    pub pack_seal_ms: u128,
    /// Train the address parser and add its weights to the Pack file.
    pub parser_training_ms: u128,
    pub total_ms: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateIssue {
    MissingHouseNumber,
    MissingStreetOrPlace,
    UnsupportedRelation,
    WayWithoutResolvedNodes,
    InterpolationUnsupportedValue,
    InterpolationUnsupportedObject,
    InterpolationWayWithoutNodes,
    InterpolationUnresolvedGeometry,
    InterpolationMissingAnchors,
    InterpolationInsufficientNumericAnchors,
    InterpolationMissingStreetOrPlace,
    InterpolationAnchorStreetMismatch,
    InterpolationInvalidNumberRange,
    InterpolationInvalidParity,
    InterpolationNonNumericAnchor,
    StreetMissingName,
    StreetRefOnlyName,
    StreetUnresolvedGeometry,
    PlaceMissingName,
    PlaceUnsupportedValue,
}

impl CandidateIssue {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::MissingHouseNumber => "missing_housenumber",
            Self::MissingStreetOrPlace => "missing_street_or_place",
            Self::UnsupportedRelation => "unsupported_relation",
            Self::WayWithoutResolvedNodes => "way_without_resolved_nodes",
            Self::InterpolationUnsupportedValue => "interpolation_unsupported_value",
            Self::InterpolationUnsupportedObject => "interpolation_unsupported_object",
            Self::InterpolationWayWithoutNodes => "interpolation_way_without_nodes",
            Self::InterpolationUnresolvedGeometry => "interpolation_unresolved_geometry",
            Self::InterpolationMissingAnchors => "interpolation_missing_anchors",
            Self::InterpolationInsufficientNumericAnchors => {
                "interpolation_insufficient_numeric_anchors"
            }
            Self::InterpolationMissingStreetOrPlace => "interpolation_missing_street_or_place",
            Self::InterpolationAnchorStreetMismatch => "interpolation_anchor_street_mismatch",
            Self::InterpolationInvalidNumberRange => "interpolation_invalid_number_range",
            Self::InterpolationInvalidParity => "interpolation_invalid_parity",
            Self::InterpolationNonNumericAnchor => "interpolation_non_numeric_anchor",
            Self::StreetMissingName => "street_missing_name",
            Self::StreetRefOnlyName => "street_ref_only_name",
            Self::StreetUnresolvedGeometry => "street_unresolved_geometry",
            Self::PlaceMissingName => "place_missing_name",
            Self::PlaceUnsupportedValue => "place_unsupported_value",
        }
    }

    const fn disposition(self) -> CandidateDisposition {
        match self {
            Self::MissingHouseNumber | Self::MissingStreetOrPlace => CandidateDisposition::Invalid,
            Self::UnsupportedRelation
            | Self::InterpolationUnsupportedValue
            | Self::InterpolationUnsupportedObject => CandidateDisposition::Unsupported,
            Self::WayWithoutResolvedNodes
            | Self::InterpolationWayWithoutNodes
            | Self::InterpolationUnresolvedGeometry
            | Self::StreetUnresolvedGeometry => CandidateDisposition::UnresolvedGeometry,
            Self::InterpolationMissingAnchors
            | Self::InterpolationInsufficientNumericAnchors
            | Self::InterpolationMissingStreetOrPlace
            | Self::InterpolationAnchorStreetMismatch
            | Self::InterpolationInvalidNumberRange
            | Self::InterpolationInvalidParity
            | Self::InterpolationNonNumericAnchor => CandidateDisposition::Invalid,
            Self::StreetMissingName
            | Self::StreetRefOnlyName
            | Self::PlaceMissingName
            | Self::PlaceUnsupportedValue => CandidateDisposition::OutOfScope,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateDisposition {
    Invalid,
    OutOfScope,
    Unsupported,
    UnresolvedGeometry,
}

struct RejectionTriage {
    bucket: &'static str,
    note: &'static str,
}

const MAX_REJECTION_SAMPLES_PER_REASON_BUCKET: usize = 5;
const MAX_ACCEPTED_QUALITY_SAMPLES_PER_BUCKET: usize = 5;

impl BuilderReport {
    pub(crate) fn accept_address_with_tags(
        &mut self,
        address: &AddressRecord,
        source_tags: Option<&BTreeMap<String, String>>,
    ) {
        self.accept_layer("address");
        match address.location_precision() {
            LocationPrecision::Point => self.accepted.node_addresses += 1,
            LocationPrecision::Centroid => self.accepted.way_centroid_addresses += 1,
        };

        if address.address.locality.is_some() {
            self.completeness.city += 1;
        }
        if address.address.postcode.is_some() {
            self.completeness.postcode += 1;
        }
        if address.address.region.is_some() {
            self.completeness.state += 1;
        }
        if address.address.country.is_some() {
            self.completeness.country += 1;
        }
        self.record_address_quality(address, source_tags);
    }

    pub(crate) fn accept_interpolation(&mut self) {
        self.accept_layer("interpolation");
        self.accepted.interpolation_ranges += 1;
    }

    pub(crate) fn accept_street(&mut self) {
        self.accept_layer("street");
        self.accepted.street_segments += 1;
    }

    pub(crate) fn accept_postcode(&mut self) {
        self.accept_layer("postcode");
        self.accepted.postcode_records += 1;
    }

    pub(crate) fn accept_place(&mut self, layer: PlaceLayer) {
        self.accept_layer(layer.as_str());
        self.accepted.place_nodes += 1;
    }

    /// A place drawn as an area, counted with the places and on its own.
    pub(crate) fn accept_place_area(&mut self, layer: PlaceLayer) {
        self.accept_place(layer);
        self.accepted.place_areas += 1;
    }

    pub(crate) fn accept_poi(&mut self, poi: &PoiRecord) {
        self.accept_layer("poi");
        match poi.location_precision {
            LocationPrecision::Point => self.accepted.poi_nodes += 1,
            LocationPrecision::Centroid => self.accepted.poi_way_centroids += 1,
        }
        if poi.address.is_some() {
            self.accepted.pois_with_address += 1;
        }
        let key = poi.category.split(':').next().unwrap_or_default();
        *self
            .accepted
            .poi_categories
            .entry(key.to_string())
            .or_default() += 1;
    }

    fn accept_layer(&mut self, layer: &str) {
        self.accepted.total += 1;
        *self.accepted.by_layer.entry(layer.to_string()).or_default() += 1;
    }

    fn record_address_quality(
        &mut self,
        address: &AddressRecord,
        source_tags: Option<&BTreeMap<String, String>>,
    ) {
        let missing_admin_fields = missing_admin_context_fields(address);
        let not_enrichable_by_point = point_lon_lat(&address.geometry).is_none();
        let missing_postcode = address.address.postcode.is_none();
        let has_complete_source_context = address.address.locality.is_some()
            && address.address.region.is_some()
            && address.address.postcode.is_some()
            && address.address.country.is_some();

        {
            let quality = &mut self.quality.addresses;
            quality.total += 1;

            if address.address.street.is_some() {
                quality.has_street += 1;
            }
            if address.address.place.is_some() {
                quality.has_place += 1;
            }
            if address.address.unit.is_some() {
                quality.has_unit += 1;
            }
            count_option(
                address.address.locality.as_deref(),
                &mut quality.has_locality,
                &mut quality.missing_locality,
            );
            count_option(
                address.address.region.as_deref(),
                &mut quality.has_region,
                &mut quality.missing_region,
            );
            count_option(
                address.address.postcode.as_deref(),
                &mut quality.has_postcode,
                &mut quality.missing_postcode,
            );
            count_option(
                address.address.country.as_deref(),
                &mut quality.has_country,
                &mut quality.missing_country,
            );

            if missing_admin_fields.is_empty() {
                quality.has_full_admin_context += 1;
            } else {
                quality.missing_any_admin_context += 1;
            }

            if not_enrichable_by_point {
                quality.not_enrichable_by_point += 1;
            } else {
                quality.enrichable_by_point += 1;
            }

            *quality
                .by_location_precision
                .entry(location_precision_name(address.location_precision()).to_string())
                .or_default() += 1;
            *quality
                .by_context_shape
                .entry(address_context_shape(address))
                .or_default() += 1;
        }

        if not_enrichable_by_point {
            self.accepted_sample(
                "address/not_enrichable_by_point",
                address,
                source_tags,
                vec!["point_geometry".to_string()],
            );
        }

        if !missing_admin_fields.is_empty() {
            self.accepted_sample(
                "address/missing_admin_context",
                address,
                source_tags,
                missing_admin_fields,
            );
        }
        if missing_postcode {
            self.accepted_sample(
                "address/missing_postcode",
                address,
                source_tags,
                vec!["postcode".to_string()],
            );
        }
        if has_complete_source_context {
            self.accepted_sample(
                "address/complete_source_context",
                address,
                source_tags,
                Vec::new(),
            );
        }
    }

    fn accepted_sample(
        &mut self,
        bucket: &str,
        address: &AddressRecord,
        source_tags: Option<&BTreeMap<String, String>>,
        missing_fields: Vec<String>,
    ) {
        let samples = self.quality.samples.entry(bucket.to_string()).or_default();
        if samples.len() >= MAX_ACCEPTED_QUALITY_SAMPLES_PER_BUCKET {
            return;
        }

        samples.push(AcceptedRecordSample {
            bucket: bucket.to_string(),
            layer: "address".to_string(),
            id: address.id(),
            label: address.label(),
            source_id: source_id(address.source.object_type, address.source.object_id),
            object_type: address.source.object_type,
            object_id: address.source.object_id,
            missing_fields,
            tags: source_tags.cloned().unwrap_or_default(),
        });
    }

    // The rejection-audit path genuinely needs the full object/tag context to
    // triage and record a rejection; bundling it into a struct would only move
    // the arguments around.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn reject_with_context(
        &mut self,
        issue: CandidateIssue,
        object_type: ObjectType,
        object_id: i64,
        all_tags: &BTreeMap<String, String>,
        addr_tags: Option<&BTreeMap<String, String>>,
        layer_hint: Option<&str>,
        writes_rejection_record: bool,
    ) {
        self.record_rejection(issue);
        if let Some(addr_tags) = addr_tags {
            self.audit_rejection(issue, object_type, all_tags, addr_tags);
        }
        self.triage_rejection(
            issue,
            object_type,
            object_id,
            all_tags,
            layer_hint,
            writes_rejection_record,
        );
    }

    fn record_rejection(&mut self, issue: CandidateIssue) {
        self.rejected.total += 1;
        *self
            .rejected
            .by_reason
            .entry(issue.as_str().to_string())
            .or_default() += 1;

        *self
            .disposition
            .by_reason
            .entry(issue.as_str().to_string())
            .or_default() += 1;

        match issue.disposition() {
            CandidateDisposition::Invalid => self.disposition.invalid += 1,
            CandidateDisposition::OutOfScope => self.disposition.out_of_scope += 1,
            CandidateDisposition::Unsupported => self.disposition.unsupported += 1,
            CandidateDisposition::UnresolvedGeometry => self.disposition.unresolved_geometry += 1,
        }
    }

    fn audit_rejection(
        &mut self,
        issue: CandidateIssue,
        object_type: ObjectType,
        all_tags: &BTreeMap<String, String>,
        addr_tags: &BTreeMap<String, String>,
    ) {
        let shape = address_shape(issue, addr_tags);
        let audit = match issue {
            CandidateIssue::MissingHouseNumber => &mut self.validation.missing_housenumber,
            CandidateIssue::MissingStreetOrPlace => &mut self.validation.missing_street_or_place,
            CandidateIssue::UnsupportedRelation => &mut self.validation.unsupported_relation,
            CandidateIssue::WayWithoutResolvedNodes | CandidateIssue::StreetUnresolvedGeometry => {
                &mut self.validation.unresolved_geometry
            }
            CandidateIssue::InterpolationUnsupportedValue
            | CandidateIssue::InterpolationUnsupportedObject
            | CandidateIssue::InterpolationWayWithoutNodes
            | CandidateIssue::InterpolationUnresolvedGeometry
            | CandidateIssue::InterpolationMissingAnchors
            | CandidateIssue::InterpolationInsufficientNumericAnchors
            | CandidateIssue::InterpolationMissingStreetOrPlace
            | CandidateIssue::InterpolationAnchorStreetMismatch
            | CandidateIssue::InterpolationInvalidNumberRange
            | CandidateIssue::InterpolationInvalidParity
            | CandidateIssue::InterpolationNonNumericAnchor => &mut self.validation.interpolation,
            CandidateIssue::StreetMissingName | CandidateIssue::StreetRefOnlyName => {
                &mut self.validation.unresolved_geometry
            }
            CandidateIssue::PlaceMissingName | CandidateIssue::PlaceUnsupportedValue => {
                &mut self.validation.unsupported_relation
            }
        };

        *audit.by_shape.entry(shape.to_string()).or_default() += 1;
        *audit
            .by_object_type
            .entry(crate::labels::object_type_name(object_type).to_string())
            .or_default() += 1;

        for key in addr_tags.keys() {
            *audit.by_addr_key.entry(key.clone()).or_default() += 1;
        }

        let mut recorded_feature_context = false;
        for feature_context in feature_contexts(all_tags) {
            recorded_feature_context = true;
            *audit
                .by_feature_context
                .entry(feature_context.to_string())
                .or_default() += 1;
        }

        if !recorded_feature_context {
            *audit
                .by_feature_context
                .entry("no_feature_context".to_string())
                .or_default() += 1;
        }
    }

    fn triage_rejection(
        &mut self,
        issue: CandidateIssue,
        object_type: ObjectType,
        object_id: i64,
        all_tags: &BTreeMap<String, String>,
        layer_hint: Option<&str>,
        writes_rejection_record: bool,
    ) {
        let triage = rejection_triage(issue, all_tags);
        let reason = issue.as_str();
        *self
            .triage
            .by_bucket
            .entry(triage.bucket.to_string())
            .or_default() += 1;
        *self
            .triage
            .by_reason_and_bucket
            .entry(reason.to_string())
            .or_default()
            .entry(triage.bucket.to_string())
            .or_default() += 1;

        if matches!(
            issue,
            CandidateIssue::StreetMissingName | CandidateIssue::StreetRefOnlyName
        ) {
            record_street_name_gap_counts(
                &mut self.triage.street_name_gaps,
                all_tags,
                triage.bucket,
            );
        }

        let sample_key = format!("{reason}/{}", triage.bucket);
        let samples = self.triage.samples.entry(sample_key).or_default();
        if samples.len() < MAX_REJECTION_SAMPLES_PER_REASON_BUCKET {
            samples.push(RejectionSample {
                reason: reason.to_string(),
                bucket: triage.bucket.to_string(),
                note: triage.note.to_string(),
                object_type,
                object_id,
                source_id: source_id(object_type, object_id),
                layer_hint: layer_hint.map(str::to_string),
                writes_rejection_record,
                tags: all_tags.clone(),
            });
        }
    }
}

fn source_id(object_type: ObjectType, object_id: i64) -> String {
    format!(
        "osm:{}:{object_id}",
        crate::labels::object_type_name(object_type)
    )
}

fn count_option(value: Option<&str>, present: &mut u64, missing: &mut u64) {
    if value.is_some_and(|value| !value.trim().is_empty()) {
        *present += 1;
    } else {
        *missing += 1;
    }
}

fn missing_admin_context_fields(address: &AddressRecord) -> Vec<String> {
    let mut missing = Vec::new();
    if address.address.locality.is_none() {
        missing.push("locality".to_string());
    }
    if address.address.region.is_none() {
        missing.push("region".to_string());
    }
    if address.address.country.is_none() {
        missing.push("country".to_string());
    }
    missing
}

fn address_context_shape(address: &AddressRecord) -> String {
    let mut parts = vec!["number"];
    if address.address.street.is_some() {
        parts.push("street");
    }
    if address.address.place.is_some() {
        parts.push("place");
    }
    if address.address.unit.is_some() {
        parts.push("unit");
    }
    if address.address.locality.is_some() {
        parts.push("locality");
    }
    if address.address.region.is_some() {
        parts.push("region");
    }
    if address.address.postcode.is_some() {
        parts.push("postcode");
    }
    if address.address.country.is_some() {
        parts.push("country");
    }
    parts.join("+")
}

fn location_precision_name(precision: LocationPrecision) -> &'static str {
    match precision {
        LocationPrecision::Point => "point",
        LocationPrecision::Centroid => "centroid",
    }
}

fn rejection_triage(issue: CandidateIssue, tags: &BTreeMap<String, String>) -> RejectionTriage {
    match issue {
        CandidateIssue::StreetMissingName => unnamed_highway_triage(tags),
        CandidateIssue::StreetRefOnlyName => RejectionTriage {
            bucket: "needs_model_decision",
            note: "highway has ref=* but no name=*; it may be useful for route-number search, but is not a reliable address street name",
        },
        CandidateIssue::MissingHouseNumber => RejectionTriage {
            bucket: "likely_usable_missing_source_data",
            note: "object has street/place address context but no house number; needs better address source data to become a specific address",
        },
        CandidateIssue::MissingStreetOrPlace => RejectionTriage {
            bucket: "likely_usable_missing_source_data",
            note: "object has a house number but no street/place; could be recoverable only with street inference or better source data",
        },
        CandidateIssue::UnsupportedRelation | CandidateIssue::InterpolationUnsupportedObject => {
            RejectionTriage {
                bucket: "needs_parser_support",
                note: "source object may be useful, but the current builder does not support this OSM object shape for the layer",
            }
        }
        CandidateIssue::WayWithoutResolvedNodes
        | CandidateIssue::InterpolationWayWithoutNodes
        | CandidateIssue::InterpolationUnresolvedGeometry
        | CandidateIssue::StreetUnresolvedGeometry => RejectionTriage {
            bucket: "needs_geometry_resolution",
            note: "source object has useful tags but geometry could not be resolved into a usable point or line",
        },
        CandidateIssue::InterpolationUnsupportedValue
        | CandidateIssue::InterpolationMissingAnchors
        | CandidateIssue::InterpolationInsufficientNumericAnchors
        | CandidateIssue::InterpolationMissingStreetOrPlace
        | CandidateIssue::InterpolationAnchorStreetMismatch
        | CandidateIssue::InterpolationInvalidNumberRange
        | CandidateIssue::InterpolationInvalidParity
        | CandidateIssue::InterpolationNonNumericAnchor => RejectionTriage {
            bucket: "invalid_source_data",
            note: "interpolation tags are incomplete or internally inconsistent; boundaries cannot repair this",
        },
        CandidateIssue::PlaceMissingName => RejectionTriage {
            bucket: "likely_not_useful_for_geocoding",
            note: "place=* without a usable name cannot be returned as a named geocoding result",
        },
        CandidateIssue::PlaceUnsupportedValue => RejectionTriage {
            bucket: "likely_not_useful_for_geocoding",
            note: "place=* value is outside the current address-first place/admin layers",
        },
    }
}

fn unnamed_highway_triage(tags: &BTreeMap<String, String>) -> RejectionTriage {
    if tags.contains_key("addr:housenumber") {
        return RejectionTriage {
            bucket: "not_a_street_gap_address_processed_separately",
            note: "unnamed highway also carries address tags; review the address rejection or acceptance separately",
        };
    }

    let Some(highway) = tag(tags, "highway").map(normalize_tag_value) else {
        return RejectionTriage {
            bucket: "likely_not_useful_for_geocoding",
            note: "street-name rejection without a highway value is not useful as a geocoding street",
        };
    };

    if is_lifecycle_or_non_current(tags, &highway) {
        return RejectionTriage {
            bucket: "likely_not_useful_for_geocoding",
            note: "non-current or construction highway without a name is not useful for address geocoding",
        };
    }

    if highway == "service" {
        return RejectionTriage {
            bucket: "likely_not_useful_for_geocoding",
            note: "unnamed service roads, driveways, alleys, and parking aisles are usually not addressable geocoding streets",
        };
    }

    if is_minor_path_highway(&highway) {
        return RejectionTriage {
            bucket: "likely_not_useful_for_geocoding",
            note: "unnamed path-like highway is usually not useful as an address geocoding street",
        };
    }

    if is_addressable_road_highway(&highway) {
        return RejectionTriage {
            bucket: "likely_usable_missing_source_data",
            note: "unnamed addressable-looking road; likely needs an OSM fix or external road centerline source",
        };
    }

    RejectionTriage {
        bucket: "needs_review",
        note: "unnamed highway type is not clearly noise or clearly addressable; inspect samples before deciding",
    }
}

fn record_street_name_gap_counts(
    counts: &mut StreetNameGapCounts,
    tags: &BTreeMap<String, String>,
    bucket: &str,
) {
    *counts.by_bucket.entry(bucket.to_string()).or_default() += 1;
    let highway = tag(tags, "highway")
        .map(normalize_tag_value)
        .unwrap_or_else(|| "missing".to_string());
    *counts.by_highway.entry(highway).or_default() += 1;
    if let Some(service) = tag(tags, "service").map(normalize_tag_value) {
        *counts.by_service.entry(service).or_default() += 1;
    }
}

fn tag<'a>(tags: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    tags.get(key)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

fn normalize_tag_value(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn is_lifecycle_or_non_current(tags: &BTreeMap<String, String>, highway: &str) -> bool {
    matches!(highway, "construction" | "proposed")
        || tags.contains_key("construction")
        || tags.contains_key("proposed")
        || tags.contains_key("abandoned")
        || tags.contains_key("disused")
        || tags.contains_key("razed")
}

fn is_minor_path_highway(highway: &str) -> bool {
    matches!(
        highway,
        "bridleway"
            | "bus_stop"
            | "corridor"
            | "crossing"
            | "cycleway"
            | "elevator"
            | "escape"
            | "footway"
            | "give_way"
            | "path"
            | "platform"
            | "raceway"
            | "rest_area"
            | "speed_camera"
            | "steps"
            | "stop"
            | "street_lamp"
            | "track"
            | "trailhead"
            | "traffic_mirror"
            | "traffic_signals"
    )
}

fn is_addressable_road_highway(highway: &str) -> bool {
    matches!(
        highway,
        "living_street"
            | "motorway"
            | "primary"
            | "residential"
            | "road"
            | "secondary"
            | "tertiary"
            | "trunk"
            | "unclassified"
    )
}

fn address_shape(issue: CandidateIssue, addr_tags: &BTreeMap<String, String>) -> &'static str {
    match issue {
        CandidateIssue::MissingHouseNumber => missing_housenumber_shape(addr_tags),
        CandidateIssue::MissingStreetOrPlace => missing_street_or_place_shape(addr_tags),
        CandidateIssue::UnsupportedRelation => {
            if addr_tags.contains_key("addr:interpolation") {
                "interpolation_relation"
            } else {
                "relation_with_addr_tags"
            }
        }
        CandidateIssue::WayWithoutResolvedNodes => "way_without_resolved_nodes",
        CandidateIssue::StreetUnresolvedGeometry => "street_unresolved_geometry",
        CandidateIssue::StreetMissingName => "street_missing_name",
        CandidateIssue::StreetRefOnlyName => "street_ref_only_name",
        CandidateIssue::PlaceMissingName => "place_missing_name",
        CandidateIssue::PlaceUnsupportedValue => "place_unsupported_value",
        CandidateIssue::InterpolationUnsupportedValue
        | CandidateIssue::InterpolationUnsupportedObject
        | CandidateIssue::InterpolationWayWithoutNodes
        | CandidateIssue::InterpolationUnresolvedGeometry
        | CandidateIssue::InterpolationMissingAnchors
        | CandidateIssue::InterpolationInsufficientNumericAnchors
        | CandidateIssue::InterpolationMissingStreetOrPlace
        | CandidateIssue::InterpolationAnchorStreetMismatch
        | CandidateIssue::InterpolationInvalidNumberRange
        | CandidateIssue::InterpolationInvalidParity
        | CandidateIssue::InterpolationNonNumericAnchor => "interpolation",
    }
}

fn missing_housenumber_shape(addr_tags: &BTreeMap<String, String>) -> &'static str {
    if addr_tags.contains_key("addr:interpolation") {
        return "interpolation";
    }

    let has_street = addr_tags.contains_key("addr:street");
    let has_place = addr_tags.contains_key("addr:place");
    let has_city = LOCALITY_KEYS.iter().any(|key| addr_tags.contains_key(*key));
    let has_postcode = addr_tags.contains_key("addr:postcode");

    match (has_street, has_place, has_city, has_postcode) {
        (true, true, _, _) => "street_and_place_without_housenumber",
        (true, false, _, _) => "street_without_housenumber",
        (false, true, _, _) => "place_without_housenumber",
        (false, false, true, true) => "city_postcode_only",
        (false, false, true, false) => "locality_only",
        (false, false, false, true) => "postcode_only",
        _ => "other_partial_address",
    }
}

fn missing_street_or_place_shape(addr_tags: &BTreeMap<String, String>) -> &'static str {
    let has_house_number = addr_tags.contains_key("addr:housenumber");
    let has_unit = addr_tags.contains_key("addr:unit");
    let has_city = LOCALITY_KEYS.iter().any(|key| addr_tags.contains_key(*key));
    let has_postcode = addr_tags.contains_key("addr:postcode");

    match (has_house_number, has_unit, has_city, has_postcode) {
        (true, true, _, _) => "housenumber_unit_without_street_or_place",
        (true, false, true, true) => "housenumber_city_postcode_without_street_or_place",
        (true, false, true, false) => "housenumber_locality_without_street_or_place",
        (true, false, false, true) => "housenumber_postcode_without_street_or_place",
        (true, false, false, false) => "housenumber_only",
        _ => "other_missing_street_or_place",
    }
}

fn feature_contexts(tags: &BTreeMap<String, String>) -> impl Iterator<Item = &'static str> {
    let mut contexts = Vec::new();

    if tags.contains_key("building") {
        contexts.push("building");
    }
    if tags.contains_key("name") {
        contexts.push("named_feature");
    }
    if tags.contains_key("highway") {
        contexts.push("road_or_path");
    }
    if POI_CONTEXT_KEYS.iter().any(|key| tags.contains_key(*key)) {
        contexts.push("poi_or_venue");
    }

    contexts.into_iter()
}

const POI_CONTEXT_KEYS: &[&str] = &[
    "aeroway",
    "amenity",
    "club",
    "craft",
    "emergency",
    "healthcare",
    "historic",
    "leisure",
    "man_made",
    "natural",
    "office",
    "public_transport",
    "railway",
    "shop",
    "sport",
    "tourism",
];

#[cfg(test)]
mod tests {
    use crate::record::{AddressComponents, SourceProvenance, point_geometry};

    #[test]
    fn a_stage_is_bound_by_the_resource_it_kept_busiest() {
        let shares = |cpu: f64, driver: f64, read: f64, write: f64| BoundShares {
            cpu,
            cores: cpu * 24.0,
            driver,
            read,
            write,
            spill_wait: 0.0,
        };
        assert_eq!(Bound::classify(&shares(0.9, 0.3, 0.2, 0.1)), Bound::Cpu);
        // One thread busy of 24 is still CPU: the stage's serial path.
        assert_eq!(Bound::classify(&shares(0.06, 0.97, 0.1, 0.0)), Bound::Cpu);
        assert_eq!(Bound::classify(&shares(0.3, 0.4, 0.95, 0.1)), Bound::ReadIo);
        assert_eq!(Bound::classify(&shares(0.6, 0.2, 0.1, 0.8)), Bound::WriteIo);
        assert_eq!(Bound::classify(&shares(0.1, 0.3, 0.2, 0.2)), Bound::Waiting);
        assert_eq!(
            serde_json::to_value(Bound::ReadIo).expect("json"),
            serde_json::json!("read-io")
        );
    }

    #[test]
    fn the_stage_table_has_a_row_per_stage_and_the_peak() {
        let mut report = BuilderReport::default();
        report.stages.push(StageReport {
            stage: "scan OSM objects".to_string(),
            wall_ms: 2_000,
            inputs: BTreeMap::from([(
                "planet.osm.pbf".to_string(),
                InputRead {
                    bytes: 360_000_000,
                    wait_ms: 1_900,
                },
            )]),
            scratch_written_bytes: 100 << 20,
            scratch_raw_written_bytes: 400 << 20,
            bound: Bound::ReadIo,
            ..StageReport::default()
        });
        report.scratch.peak_live_bytes = 300 << 20;
        report.scratch.peak_owners = BTreeMap::from([("node-requests".to_string(), 200 << 20)]);
        report.scratch.zstd_level = 1;
        let table = report.stage_table();
        let row = table.lines().nth(1).expect("row");
        assert!(row.starts_with("scan OSM objects"), "{table}");
        assert!(row.contains(" 180 "), "input MB/s: {table}");
        assert!(row.contains("100 (   400)"), "spill: {table}");
        assert!(row.ends_with("read-io"), "{table}");
        assert!(
            table.contains("peak scratch 300 MiB (node-requests 200 MiB)"),
            "{table}"
        );
    }

    #[test]
    fn merging_worker_reports_matches_one_sequential_report() {
        let tags = BTreeMap::from([("highway".to_string(), "residential".to_string())]);
        let mut sequential = BuilderReport::default();
        let mut first = BuilderReport::default();
        let mut second = BuilderReport::default();
        for object_id in 0..8 {
            let worker = if object_id < 3 {
                &mut first
            } else {
                &mut second
            };
            for report in [&mut sequential, worker] {
                report.reject_with_context(
                    CandidateIssue::StreetMissingName,
                    ObjectType::Way,
                    object_id,
                    &tags,
                    None,
                    Some("street"),
                    false,
                );
                report.accept_address_with_tags(
                    &address_record("", object_id, Some("King Street"), None, None, None, None),
                    None,
                );
            }
        }
        let mut merged = BuilderReport::default();
        merged.merge(first);
        merged.merge(second);
        assert_eq!(merged, sequential);
    }

    use super::*;

    #[test]
    fn records_accepted_address_quality_and_samples_missing_context() {
        let address = address_record(
            "osm:node:10",
            10,
            Some("King Street"),
            None,
            None,
            None,
            None,
        );
        let tags = BTreeMap::from([
            ("addr:housenumber".to_string(), "10".to_string()),
            ("addr:street".to_string(), "King Street".to_string()),
        ]);
        let mut report = BuilderReport::default();

        report.accept_address_with_tags(&address, Some(&tags));

        assert_eq!(report.quality.addresses.total, 1);
        assert_eq!(report.quality.addresses.has_street, 1);
        assert_eq!(report.quality.addresses.missing_locality, 1);
        assert_eq!(report.quality.addresses.missing_region, 1);
        assert_eq!(report.quality.addresses.missing_postcode, 1);
        assert_eq!(report.quality.addresses.missing_country, 1);
        assert_eq!(report.quality.addresses.missing_any_admin_context, 1);
        assert_eq!(report.quality.addresses.enrichable_by_point, 1);
        assert_eq!(
            report
                .quality
                .addresses
                .by_context_shape
                .get("number+street"),
            Some(&1)
        );

        let admin_samples = report
            .quality
            .samples
            .get("address/missing_admin_context")
            .expect("missing admin sample");
        assert_eq!(admin_samples.len(), 1);
        assert_eq!(admin_samples[0].source_id, "osm:node:10");
        assert_eq!(
            admin_samples[0].missing_fields,
            vec![
                "locality".to_string(),
                "region".to_string(),
                "country".to_string()
            ]
        );
        assert_eq!(
            admin_samples[0].tags.get("addr:street"),
            Some(&"King Street".to_string())
        );
        assert!(
            report
                .quality
                .samples
                .contains_key("address/missing_postcode")
        );
    }

    #[test]
    fn records_accepted_address_quality_for_complete_source_context() {
        let address = address_record(
            "osm:node:20",
            20,
            Some("King Street"),
            Some("Toronto"),
            Some("Ontario"),
            Some("M5V"),
            Some("CA"),
        );
        let mut report = BuilderReport::default();

        report.accept_address_with_tags(&address, None);

        assert_eq!(report.quality.addresses.total, 1);
        assert_eq!(report.quality.addresses.has_locality, 1);
        assert_eq!(report.quality.addresses.has_region, 1);
        assert_eq!(report.quality.addresses.has_postcode, 1);
        assert_eq!(report.quality.addresses.has_country, 1);
        assert_eq!(report.quality.addresses.has_full_admin_context, 1);
        assert_eq!(report.quality.addresses.missing_any_admin_context, 0);
        assert_eq!(
            report.quality.addresses.by_location_precision.get("point"),
            Some(&1)
        );
        assert!(
            report
                .quality
                .samples
                .contains_key("address/complete_source_context")
        );
    }

    #[test]
    fn audits_missing_housenumber_by_shape_and_feature_context() {
        let all_tags = BTreeMap::from([
            ("addr:street".to_string(), "King Street".to_string()),
            ("amenity".to_string(), "school".to_string()),
            ("building".to_string(), "yes".to_string()),
            ("name".to_string(), "Example School".to_string()),
        ]);
        let addr_tags = BTreeMap::from([("addr:street".to_string(), "King Street".to_string())]);
        let mut report = BuilderReport::default();

        report.reject_with_context(
            CandidateIssue::MissingHouseNumber,
            ObjectType::Way,
            77,
            &all_tags,
            Some(&addr_tags),
            Some("address"),
            true,
        );

        assert_eq!(
            report
                .validation
                .missing_housenumber
                .by_shape
                .get("street_without_housenumber"),
            Some(&1)
        );
        assert_eq!(
            report
                .validation
                .missing_housenumber
                .by_feature_context
                .get("building"),
            Some(&1)
        );
        assert_eq!(
            report
                .validation
                .missing_housenumber
                .by_feature_context
                .get("poi_or_venue"),
            Some(&1)
        );
        assert_eq!(
            report
                .validation
                .missing_housenumber
                .by_feature_context
                .get("named_feature"),
            Some(&1)
        );
    }

    #[test]
    fn triages_unnamed_service_highway_as_not_useful_with_sample() {
        let tags = BTreeMap::from([
            ("highway".to_string(), "service".to_string()),
            ("service".to_string(), "parking_aisle".to_string()),
        ]);
        let mut report = BuilderReport::default();

        report.reject_with_context(
            CandidateIssue::StreetMissingName,
            ObjectType::Way,
            42,
            &tags,
            None,
            Some("street"),
            false,
        );

        assert_eq!(
            report
                .triage
                .by_reason_and_bucket
                .get("street_missing_name")
                .and_then(|counts| counts.get("likely_not_useful_for_geocoding")),
            Some(&1)
        );
        assert_eq!(
            report.triage.street_name_gaps.by_highway.get("service"),
            Some(&1)
        );
        assert_eq!(
            report
                .triage
                .street_name_gaps
                .by_service
                .get("parking_aisle"),
            Some(&1)
        );

        let samples = report
            .triage
            .samples
            .get("street_missing_name/likely_not_useful_for_geocoding")
            .expect("sample bucket");
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].source_id, "osm:way:42");
        assert_eq!(samples[0].layer_hint.as_deref(), Some("street"));
        assert!(!samples[0].writes_rejection_record);
    }

    #[test]
    fn triages_unnamed_residential_highway_as_recoverable_gap() {
        let tags = BTreeMap::from([("highway".to_string(), "residential".to_string())]);
        let mut report = BuilderReport::default();

        report.reject_with_context(
            CandidateIssue::StreetMissingName,
            ObjectType::Way,
            99,
            &tags,
            None,
            Some("street"),
            false,
        );

        assert_eq!(
            report
                .triage
                .by_bucket
                .get("likely_usable_missing_source_data"),
            Some(&1)
        );
        assert_eq!(
            report
                .triage
                .street_name_gaps
                .by_bucket
                .get("likely_usable_missing_source_data"),
            Some(&1)
        );
        assert_eq!(
            report
                .triage
                .samples
                .get("street_missing_name/likely_usable_missing_source_data")
                .and_then(|samples| samples.first())
                .map(|sample| sample.tags.get("highway")),
            Some(Some(&"residential".to_string()))
        );
    }

    #[test]
    fn caps_rejection_samples_per_reason_bucket() {
        let tags = BTreeMap::from([("highway".to_string(), "residential".to_string())]);
        let mut report = BuilderReport::default();

        for object_id in 0..10 {
            report.reject_with_context(
                CandidateIssue::StreetMissingName,
                ObjectType::Way,
                object_id,
                &tags,
                None,
                Some("street"),
                false,
            );
        }

        assert_eq!(
            report
                .triage
                .samples
                .get("street_missing_name/likely_usable_missing_source_data")
                .map(Vec::len),
            Some(MAX_REJECTION_SAMPLES_PER_REASON_BUCKET)
        );
        assert_eq!(
            report
                .triage
                .by_bucket
                .get("likely_usable_missing_source_data"),
            Some(&10)
        );
    }

    fn address_record(
        _id: &str,
        object_id: i64,
        street: Option<&str>,
        locality: Option<&str>,
        region: Option<&str>,
        postcode: Option<&str>,
        country: Option<&str>,
    ) -> AddressRecord {
        let street = street.map(str::to_string);
        AddressRecord {
            disputed: false,
            street_postcodes: Vec::new(),
            address: AddressComponents {
                areas: Vec::new(),
                number: "10".to_string(),
                street,
                place: None,
                unit: None,
                locality: locality.map(str::to_string),
                region: region.map(str::to_string),
                postcode: postcode.map(str::to_string),
                country: country.map(str::to_string),
            },
            geometry: point_geometry(-79.3832, 43.6532),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, object_id),
        }
    }
}
