use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::record::{AddressRecord, LocationPrecision, OsmObjectType};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct BuilderReport {
    pub schema_version: u32,
    pub input: String,
    pub output: String,
    pub input_bytes: u64,
    pub scanned: ScannedCounts,
    pub accepted: AcceptedCounts,
    pub rejected: RejectedCounts,
    pub disposition: CandidateDispositionCounts,
    pub validation: ValidationAuditCounts,
    pub geometry_resolution: GeometryResolutionCounts,
    pub completeness: CompletenessCounts,
    pub phases: PhaseTimings,
    pub node_cache_entries: usize,
    pub output_ndjson_bytes: u64,
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
    pub node_addresses: u64,
    pub way_centroid_addresses: u64,
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
    pub discovery_ms: u128,
    pub coordinate_resolution_ms: u128,
    pub record_emission_ms: u128,
    pub total_ms: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateIssue {
    MissingHouseNumber,
    MissingStreetOrPlace,
    UnsupportedRelation,
    WayWithoutResolvedNodes,
}

impl CandidateIssue {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::MissingHouseNumber => "missing_housenumber",
            Self::MissingStreetOrPlace => "missing_street_or_place",
            Self::UnsupportedRelation => "unsupported_relation",
            Self::WayWithoutResolvedNodes => "way_without_resolved_nodes",
        }
    }

    const fn disposition(self) -> CandidateDisposition {
        match self {
            Self::MissingHouseNumber | Self::MissingStreetOrPlace => CandidateDisposition::Invalid,
            Self::UnsupportedRelation => CandidateDisposition::Unsupported,
            Self::WayWithoutResolvedNodes => CandidateDisposition::UnresolvedGeometry,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateDisposition {
    Invalid,
    Unsupported,
    UnresolvedGeometry,
}

impl BuilderReport {
    pub(crate) fn accept(&mut self, record: &AddressRecord) {
        self.accepted.total += 1;
        match record.location_precision {
            LocationPrecision::Point => self.accepted.node_addresses += 1,
            LocationPrecision::Centroid => self.accepted.way_centroid_addresses += 1,
        }

        if record.city.is_some() {
            self.completeness.city += 1;
        }
        if record.postcode.is_some() {
            self.completeness.postcode += 1;
        }
        if record.state.is_some() {
            self.completeness.state += 1;
        }
        if record.country.is_some() {
            self.completeness.country += 1;
        }
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
            CandidateIssue::WayWithoutResolvedNodes => &mut self.validation.unresolved_geometry,
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
