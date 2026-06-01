use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    record::{AddressRecord, LocationPrecision, OsmObjectType, PlaceLayer},
    util::geo::point_lon_lat,
};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct BuilderReport {
    pub schema_version: u32,
    pub input: String,
    pub pack: String,
    pub input_bytes: u64,
    pub scanned: ScannedCounts,
    pub accepted: AcceptedCounts,
    pub rejected: RejectedCounts,
    pub disposition: CandidateDispositionCounts,
    pub validation: ValidationAuditCounts,
    pub quality: AcceptedQualityReport,
    pub triage: RejectionTriageReport,
    pub geometry_resolution: GeometryResolutionCounts,
    pub completeness: CompletenessCounts,
    pub phases: PhaseTimings,
    pub throughput: Throughput,
    pub pack_write: PackWriteTimings,
    pub node_cache_entries: usize,
    pub record_table_bytes: u64,
    pub offset_table_bytes: u64,
    pub rejection_table_bytes: u64,
    pub rejection_offset_table_bytes: u64,
    pub text_index_path: String,
    pub text_index_schema_version: u32,
    pub text_index_document_count: u64,
    pub text_index_bytes: u64,
    pub spatial_index_path: String,
    pub spatial_index_schema_version: u32,
    pub spatial_index_point_count: u64,
    pub spatial_index_segment_count: u64,
    pub spatial_index_bytes: u64,
}

impl BuilderReport {
    /// Derive build throughput from the final wall-clock and record counts.
    /// Call once `phases.total_ms` is final (i.e. after pack finalize).
    pub(crate) fn finalize_throughput(&mut self) {
        let secs = self.phases.total_ms as f64 / 1000.0;
        if secs <= 0.0 {
            return;
        }
        self.throughput.records_per_sec = self.text_index_document_count as f64 / secs;
        self.throughput.addresses_per_sec =
            *self.accepted.by_layer.get("address").unwrap_or(&0) as f64 / secs;
    }
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
    pub postcode_records: u64,
    pub place_nodes: u64,
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
    pub object_type: OsmObjectType,
    pub object_id: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing_fields: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
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
    pub object_type: OsmObjectType,
    pub object_id: i64,
    pub source_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
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

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GeometryResolutionCounts {
    pub address_way_stubs: usize,
    pub interpolation_way_stubs: usize,
    pub street_way_stubs: usize,
    pub required_node_refs: usize,
    pub resolved_node_refs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CompletenessCounts {
    pub city: u64,
    pub postcode: u64,
    pub state: u64,
    pub country: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct PhaseTimings {
    pub pack_create_ms: u128,
    pub discovery_ms: u128,
    pub coordinate_resolution_ms: u128,
    pub record_emission_ms: u128,
    pub pack_finish_ms: u128,
    pub total_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct PackWriteTimings {
    pub record_encode_ms: u128,
    pub record_table_write_ms: u128,
    pub text_index_write_ms: u128,
    pub text_projection_ms: u128,
    pub tantivy_document_build_ms: u128,
    pub tantivy_add_document_ms: u128,
    pub spatial_index_write_ms: u128,
    pub spatial_point_pair_generation_ms: u128,
    pub spatial_segment_pair_generation_ms: u128,
    pub spatial_pair_sort_dedupe_ms: u128,
    pub spatial_cell_directory_build_ms: u128,
    pub spatial_file_write_ms: u128,
    pub rejection_encode_ms: u128,
    pub rejection_table_write_ms: u128,
    pub final_offset_header_ms: u128,
    pub table_flush_ms: u128,
    pub text_index_commit_ms: u128,
    pub text_index_size_ms: u128,
    pub spatial_index_finish_ms: u128,
    pub spatial_index_size_ms: u128,
    pub table_size_ms: u128,
    pub runtime_finalize_ms: u128,
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
        object_type: OsmObjectType,
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
        object_type: OsmObjectType,
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
            .entry(object_type_name(object_type).to_string())
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
        object_type: OsmObjectType,
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

fn source_id(object_type: OsmObjectType, object_id: i64) -> String {
    format!("osm:{}:{object_id}", object_type_name(object_type))
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
    let has_city = addr_tags.contains_key("addr:city");
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
    let has_city = addr_tags.contains_key("addr:city");
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

fn object_type_name(object_type: OsmObjectType) -> &'static str {
    match object_type {
        OsmObjectType::Node => "node",
        OsmObjectType::Way => "way",
        OsmObjectType::Relation => "relation",
    }
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
            OsmObjectType::Way,
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
            OsmObjectType::Way,
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
            OsmObjectType::Way,
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
                OsmObjectType::Way,
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
            address: AddressComponents {
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
            source: SourceProvenance::osm(OsmObjectType::Node, object_id),
        }
    }
}
