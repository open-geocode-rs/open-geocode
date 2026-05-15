use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::record::{AddressRecord, LocationPrecision};

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
}
