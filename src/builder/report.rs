use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::record::{AddressRecord, LocationPrecision, OsmObjectType, PlaceLayer};

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
    pub geometry_resolution: GeometryResolutionCounts,
    pub completeness: CompletenessCounts,
    pub phases: PhaseTimings,
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
    pub text_index_prefix: TextIndexPrefixStats,
    pub spatial_index_path: String,
    pub spatial_index_schema_version: u32,
    pub spatial_index_point_count: u64,
    pub spatial_index_segment_count: u64,
    pub spatial_index_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TextIndexPrefixStats {
    pub autocomplete_prefix_terms_total: u64,
    pub autocomplete_prefix_terms_avg_per_record: f64,
    pub autocomplete_prefix_terms_p95_per_record: u64,
    pub autocomplete_prefix_terms_max_per_record: u64,
    pub autocomplete_prefix_terms_cap_hit_count: u64,
    pub autocomplete_prefix_terms_by_layer: BTreeMap<String, u64>,
    pub autocomplete_prefix_terms_by_field: BTreeMap<String, u64>,
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
    pub text_prefix_generation_ms: u128,
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

impl BuilderReport {
    pub(crate) fn accept_address(&mut self, address: &AddressRecord) {
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
        self.accept_layer(place_layer_name(layer));
        self.accepted.place_nodes += 1;
    }

    fn accept_layer(&mut self, layer: &str) {
        self.accepted.total += 1;
        *self.accepted.by_layer.entry(layer.to_string()).or_default() += 1;
    }

    pub(crate) fn reject(&mut self, issue: CandidateIssue) {
        self.record_rejection(issue);
    }

    pub(crate) fn reject_with_tags(
        &mut self,
        issue: CandidateIssue,
        object_type: OsmObjectType,
        all_tags: &BTreeMap<String, String>,
        addr_tags: &BTreeMap<String, String>,
    ) {
        self.record_rejection(issue);
        self.audit_rejection(issue, object_type, all_tags, addr_tags);
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

fn place_layer_name(layer: PlaceLayer) -> &'static str {
    match layer {
        PlaceLayer::Country => "country",
        PlaceLayer::Region => "region",
        PlaceLayer::District => "district",
        PlaceLayer::Place => "place",
        PlaceLayer::Locality => "locality",
        PlaceLayer::Neighbourhood => "neighbourhood",
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
    use super::*;

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

        report.reject_with_tags(
            CandidateIssue::MissingHouseNumber,
            OsmObjectType::Way,
            &all_tags,
            &addr_tags,
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
}
