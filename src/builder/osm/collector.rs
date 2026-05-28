use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};

use anyhow::{Context, Result};
use osmpbf::{Element, RelMemberType};

use crate::{
    builder::report::{BuilderReport, CandidateIssue, ScannedCounts},
    record::{LocationPrecision, OsmObjectType},
};

use super::{
    address::{
        AddressCandidate, collect_addr_tags_from_map, collect_clean_tags, validate_address_tags,
    },
    boundary::{
        BoundaryRelationMember, BoundaryRelationStub, BoundaryWayStub, has_admin_boundary_tags,
        relation_member_role,
    },
    interpolation::{InterpolationWayStub, has_interpolation_tag},
    pbf::{element_reader_with_progress, input_bytes},
    place::has_place_tag,
    street::{StreetWayStub, has_highway_tag, missing_street_name_issue, street_name},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddressWayStub {
    pub object_id: i64,
    pub node_refs: Vec<i64>,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlaceNodeCandidate {
    pub object_id: i64,
    pub lat: f64,
    pub lon: f64,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CollectedRejection {
    pub issue: CandidateIssue,
    pub object_type: OsmObjectType,
    pub object_id: i64,
    pub tags: BTreeMap<String, String>,
    pub addr_tags: Option<BTreeMap<String, String>>,
    pub layer_hint: Option<&'static str>,
    pub write_record: bool,
}

#[derive(Debug)]
pub(crate) struct DiscoveryResult {
    pub report: BuilderReport,
    pub place_node_candidates: Vec<PlaceNodeCandidate>,
    pub address_node_candidates: Vec<AddressCandidate>,
    pub way_stubs: Vec<AddressWayStub>,
    pub interpolation_way_stubs: Vec<InterpolationWayStub>,
    pub street_way_stubs: Vec<StreetWayStub>,
    pub boundary_way_stubs: Vec<BoundaryWayStub>,
    pub boundary_relation_stubs: Vec<BoundaryRelationStub>,
    pub rejections: Vec<CollectedRejection>,
    pub address_node_tags: HashMap<i64, BTreeMap<String, String>>,
    pub required_node_ids: HashSet<i64>,
}

#[derive(Debug, Default)]
struct DiscoveryChunk {
    scanned: ScannedCounts,
    place_node_candidates: Vec<PlaceNodeCandidate>,
    address_node_candidates: Vec<AddressCandidate>,
    way_stubs: Vec<AddressWayStub>,
    interpolation_way_stubs: Vec<InterpolationWayStub>,
    street_way_stubs: Vec<StreetWayStub>,
    boundary_way_stubs: Vec<BoundaryWayStub>,
    boundary_relation_stubs: Vec<BoundaryRelationStub>,
    rejections: Vec<CollectedRejection>,
    address_node_tags: HashMap<i64, BTreeMap<String, String>>,
    required_node_ids: HashSet<i64>,
}

impl DiscoveryChunk {
    fn merge(mut self, other: Self) -> Self {
        self.scanned.nodes += other.scanned.nodes;
        self.scanned.dense_nodes += other.scanned.dense_nodes;
        self.scanned.ways += other.scanned.ways;
        self.scanned.relations += other.scanned.relations;
        self.place_node_candidates
            .extend(other.place_node_candidates);
        self.address_node_candidates
            .extend(other.address_node_candidates);
        self.way_stubs.extend(other.way_stubs);
        self.interpolation_way_stubs
            .extend(other.interpolation_way_stubs);
        self.street_way_stubs.extend(other.street_way_stubs);
        self.boundary_way_stubs.extend(other.boundary_way_stubs);
        self.boundary_relation_stubs
            .extend(other.boundary_relation_stubs);
        self.rejections.extend(other.rejections);
        self.address_node_tags.extend(other.address_node_tags);
        self.required_node_ids.extend(other.required_node_ids);
        self
    }
}

pub(crate) fn discover_address_features(input: &Path) -> Result<DiscoveryResult> {
    let (reader, progress) = element_reader_with_progress(input, "1/7 scan OSM features")?;

    let mut chunk = reader
        .par_map_reduce(
            collect_element,
            DiscoveryChunk::default,
            DiscoveryChunk::merge,
        )
        .with_context(|| format!("failed to parse {}", input.display()))?;
    progress.finish_with_message("1/7 scan OSM features complete");

    sort_discovery_chunk(&mut chunk);

    let mut report = BuilderReport {
        schema_version: 12,
        input: input.display().to_string(),
        input_bytes: input_bytes(input)?,
        scanned: chunk.scanned.clone(),
        ..BuilderReport::default()
    };
    for rejection in &chunk.rejections {
        report.reject_with_context(
            rejection.issue,
            rejection.object_type,
            rejection.object_id,
            &rejection.tags,
            rejection.addr_tags.as_ref(),
            rejection.layer_hint,
            rejection.write_record,
        );
    }

    Ok(DiscoveryResult {
        report,
        place_node_candidates: chunk.place_node_candidates,
        address_node_candidates: chunk.address_node_candidates,
        way_stubs: chunk.way_stubs,
        interpolation_way_stubs: chunk.interpolation_way_stubs,
        street_way_stubs: chunk.street_way_stubs,
        boundary_way_stubs: chunk.boundary_way_stubs,
        boundary_relation_stubs: chunk.boundary_relation_stubs,
        rejections: chunk
            .rejections
            .into_iter()
            .filter(|rejection| rejection.write_record)
            .collect(),
        address_node_tags: chunk.address_node_tags,
        required_node_ids: chunk.required_node_ids,
    })
}

fn collect_element(element: Element<'_>) -> DiscoveryChunk {
    let mut chunk = DiscoveryChunk::default();
    match element {
        Element::DenseNode(node) => {
            chunk.scanned.dense_nodes += 1;
            collect_node(
                &mut chunk,
                node.id(),
                node.lat(),
                node.lon(),
                collect_clean_tags(node.tags()),
            );
        }
        Element::Node(node) => {
            chunk.scanned.nodes += 1;
            collect_node(
                &mut chunk,
                node.id(),
                node.lat(),
                node.lon(),
                collect_clean_tags(node.tags()),
            );
        }
        Element::Way(way) => {
            chunk.scanned.ways += 1;
            collect_way(&mut chunk, way.id(), collect_clean_tags(way.tags()), || {
                way.refs().collect::<Vec<_>>()
            });
        }
        Element::Relation(relation) => {
            chunk.scanned.relations += 1;
            let members = relation
                .members()
                .filter_map(|member| {
                    if member.member_type != RelMemberType::Way {
                        return None;
                    }
                    let role = relation_member_role(member.role().unwrap_or_default())?;
                    Some(BoundaryRelationMember {
                        way_id: member.member_id,
                        role,
                    })
                })
                .collect::<Vec<_>>();
            collect_relation(
                &mut chunk,
                relation.id(),
                collect_clean_tags(relation.tags()),
                members,
            );
        }
    }
    chunk
}

fn collect_node(
    chunk: &mut DiscoveryChunk,
    object_id: i64,
    lat: f64,
    lon: f64,
    all_tags: BTreeMap<String, String>,
) {
    if has_place_tag(&all_tags) {
        chunk.place_node_candidates.push(PlaceNodeCandidate {
            object_id,
            lat,
            lon,
            tags: all_tags.clone(),
        });
    }

    let tags = collect_addr_tags_from_map(&all_tags);
    if tags.is_empty() {
        return;
    }
    chunk.address_node_tags.insert(object_id, tags.clone());

    if has_interpolation_tag(&tags) {
        collect_audited_rejection(
            chunk,
            CandidateIssue::InterpolationUnsupportedObject,
            OsmObjectType::Node,
            object_id,
            &all_tags,
            &tags,
            Some("interpolation"),
        );
        return;
    }

    if let Err(issue) = validate_address_tags(&tags) {
        collect_audited_rejection(
            chunk,
            issue,
            OsmObjectType::Node,
            object_id,
            &all_tags,
            &tags,
            Some("address"),
        );
        return;
    }

    chunk.address_node_candidates.push(AddressCandidate {
        object_type: OsmObjectType::Node,
        object_id,
        lat,
        lon,
        location_precision: LocationPrecision::Point,
        tags,
    });
}

fn collect_way(
    chunk: &mut DiscoveryChunk,
    object_id: i64,
    all_tags: BTreeMap<String, String>,
    refs: impl Fn() -> Vec<i64>,
) {
    if has_admin_boundary_tags(&all_tags) {
        let node_refs = refs();
        if !node_refs.is_empty() {
            chunk.required_node_ids.extend(node_refs.iter().copied());
            chunk.boundary_way_stubs.push(BoundaryWayStub {
                object_id,
                node_refs,
                tags: all_tags.clone(),
            });
        }
    }

    if has_highway_tag(&all_tags) {
        if street_name(&all_tags).is_some() {
            let node_refs = refs();
            if node_refs.is_empty() {
                let empty_addr_tags = BTreeMap::new();
                collect_audited_rejection(
                    chunk,
                    CandidateIssue::StreetUnresolvedGeometry,
                    OsmObjectType::Way,
                    object_id,
                    &all_tags,
                    &empty_addr_tags,
                    Some("street"),
                );
            } else {
                chunk.required_node_ids.extend(node_refs.iter().copied());
                chunk.street_way_stubs.push(StreetWayStub {
                    object_id,
                    node_refs,
                    tags: all_tags.clone(),
                });
            }
        } else {
            collect_report_only_rejection(
                chunk,
                missing_street_name_issue(&all_tags),
                OsmObjectType::Way,
                object_id,
                &all_tags,
            );
        }
    }

    let tags = collect_addr_tags_from_map(&all_tags);
    if tags.is_empty() {
        return;
    }

    if has_interpolation_tag(&tags) {
        let node_refs = refs();
        if node_refs.is_empty() {
            collect_audited_rejection(
                chunk,
                CandidateIssue::InterpolationWayWithoutNodes,
                OsmObjectType::Way,
                object_id,
                &all_tags,
                &tags,
                Some("interpolation"),
            );
            return;
        }

        chunk.required_node_ids.extend(node_refs.iter().copied());
        chunk.interpolation_way_stubs.push(InterpolationWayStub {
            object_id,
            node_refs,
            tags: all_tags,
        });
        return;
    }

    if let Err(issue) = validate_address_tags(&tags) {
        collect_audited_rejection(
            chunk,
            issue,
            OsmObjectType::Way,
            object_id,
            &all_tags,
            &tags,
            Some("address"),
        );
        return;
    }

    let node_refs = refs();
    if node_refs.is_empty() {
        collect_rejection_record(
            chunk,
            CandidateIssue::WayWithoutResolvedNodes,
            OsmObjectType::Way,
            object_id,
            &all_tags,
            Some("address"),
        );
        return;
    }

    chunk.required_node_ids.extend(node_refs.iter().copied());
    chunk.way_stubs.push(AddressWayStub {
        object_id,
        tags,
        node_refs,
    });
}

fn collect_relation(
    chunk: &mut DiscoveryChunk,
    object_id: i64,
    all_tags: BTreeMap<String, String>,
    boundary_members: Vec<BoundaryRelationMember>,
) {
    if has_admin_boundary_tags(&all_tags) {
        if !boundary_members.is_empty() {
            chunk.boundary_relation_stubs.push(BoundaryRelationStub {
                object_id,
                members: boundary_members,
                tags: all_tags,
            });
        }
        return;
    }

    let tags = collect_addr_tags_from_map(&all_tags);
    if tags.is_empty() {
        return;
    }

    let (issue, layer_hint) = if has_interpolation_tag(&tags) {
        (
            CandidateIssue::InterpolationUnsupportedObject,
            "interpolation",
        )
    } else {
        (CandidateIssue::UnsupportedRelation, "address")
    };
    collect_audited_rejection(
        chunk,
        issue,
        OsmObjectType::Relation,
        object_id,
        &all_tags,
        &tags,
        Some(layer_hint),
    );
}

fn collect_audited_rejection(
    chunk: &mut DiscoveryChunk,
    issue: CandidateIssue,
    object_type: OsmObjectType,
    object_id: i64,
    tags: &BTreeMap<String, String>,
    addr_tags: &BTreeMap<String, String>,
    layer_hint: Option<&'static str>,
) {
    chunk.rejections.push(CollectedRejection {
        issue,
        object_type,
        object_id,
        tags: tags.clone(),
        addr_tags: Some(addr_tags.clone()),
        layer_hint,
        write_record: true,
    });
}

fn collect_rejection_record(
    chunk: &mut DiscoveryChunk,
    issue: CandidateIssue,
    object_type: OsmObjectType,
    object_id: i64,
    tags: &BTreeMap<String, String>,
    layer_hint: Option<&'static str>,
) {
    chunk.rejections.push(CollectedRejection {
        issue,
        object_type,
        object_id,
        tags: tags.clone(),
        addr_tags: None,
        layer_hint,
        write_record: true,
    });
}

fn collect_report_only_rejection(
    chunk: &mut DiscoveryChunk,
    issue: CandidateIssue,
    object_type: OsmObjectType,
    object_id: i64,
    tags: &BTreeMap<String, String>,
) {
    chunk.rejections.push(CollectedRejection {
        issue,
        object_type,
        object_id,
        tags: tags.clone(),
        addr_tags: None,
        layer_hint: None,
        write_record: false,
    });
}

fn sort_discovery_chunk(chunk: &mut DiscoveryChunk) {
    chunk
        .place_node_candidates
        .sort_by_key(|candidate| candidate.object_id);
    chunk
        .address_node_candidates
        .sort_by_key(|candidate| candidate.object_id);
    chunk.way_stubs.sort_by_key(|stub| stub.object_id);
    chunk
        .interpolation_way_stubs
        .sort_by_key(|stub| stub.object_id);
    chunk.street_way_stubs.sort_by_key(|stub| stub.object_id);
    chunk.boundary_way_stubs.sort_by_key(|stub| stub.object_id);
    chunk
        .boundary_relation_stubs
        .sort_by_key(|stub| stub.object_id);
    chunk.rejections.sort_by_key(|rejection| {
        (
            object_type_rank(rejection.object_type),
            rejection.object_id,
            rejection.issue.as_str(),
            rejection.layer_hint,
            rejection.write_record,
        )
    });
}

const fn object_type_rank(object_type: OsmObjectType) -> u8 {
    match object_type {
        OsmObjectType::Node => 0,
        OsmObjectType::Way => 1,
        OsmObjectType::Relation => 2,
    }
}
