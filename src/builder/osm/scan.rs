//! Pass 1: classify every OSM object in one parallel scan.
//!
//! Address, POI and place nodes become records immediately, because a node
//! carries its own coordinates. Ways that need geometry become [`WayFeature`]s plus
//! their node references; boundary relations are kept as stubs. Each block is
//! classified independently, so blocks are processed in parallel and merged in
//! file order.

use std::{cell::OnceCell, collections::BTreeMap};

use osmpbf::{Element, PrimitiveBlock, RelMemberType};

use crate::{
    builder::report::CandidateIssue,
    record::{LocationPrecision, OsmObjectType},
};

use super::{
    address::{
        AddressCandidate, collect_addr_tags_from_map, collect_clean_tags, emit_address,
        validate_address_tags,
    },
    boundary::{
        BoundaryRelationMember, BoundaryRelationStub, boundary_tags, has_admin_boundary_tags,
        relation_member_role,
    },
    emitted::Emitted,
    interpolation::has_interpolation_tag,
    place::{emit_place_node, has_place_tag},
    poi::{PoiCandidate, emit_poi, is_poi, poi_tags},
    spill::{WayFeature, WayKind},
    street::{has_highway_tag, missing_street_name_issue, street_name, street_tags},
};

/// Classification of one PBF block.
#[derive(Debug, Default)]
pub(crate) struct ScanOutput {
    pub emitted: Emitted,
    /// Kept ways with their node references.
    pub ways: Vec<(WayFeature, Vec<i64>)>,
    pub relations: Vec<BoundaryRelationStub>,
    pub has_nodes: bool,
    pub has_ways: bool,
}

pub(crate) fn scan_block(block: &PrimitiveBlock) -> ScanOutput {
    let mut output = ScanOutput::default();
    for element in block.elements() {
        match element {
            Element::DenseNode(node) => {
                output.has_nodes = true;
                output.emitted.report.scanned.dense_nodes += 1;
                scan_node(
                    &mut output,
                    node.id(),
                    node.lat(),
                    node.lon(),
                    collect_clean_tags(node.tags()),
                );
            }
            Element::Node(node) => {
                output.has_nodes = true;
                output.emitted.report.scanned.nodes += 1;
                scan_node(
                    &mut output,
                    node.id(),
                    node.lat(),
                    node.lon(),
                    collect_clean_tags(node.tags()),
                );
            }
            Element::Way(way) => {
                output.has_ways = true;
                output.emitted.report.scanned.ways += 1;
                let tags = collect_clean_tags(way.tags());
                if !tags.is_empty() {
                    let refs = OnceCell::new();
                    scan_way(&mut output, way.id(), tags, || {
                        refs.get_or_init(|| way.refs().collect::<Vec<_>>())
                            .as_slice()
                    });
                }
            }
            Element::Relation(relation) => {
                output.emitted.report.scanned.relations += 1;
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
                    .collect();
                scan_relation(
                    &mut output,
                    relation.id(),
                    collect_clean_tags(relation.tags()),
                    members,
                );
            }
        }
    }
    output
}

fn scan_node(
    output: &mut ScanOutput,
    object_id: i64,
    lat: f64,
    lon: f64,
    all_tags: BTreeMap<String, String>,
) {
    if all_tags.is_empty() {
        return;
    }
    let out = &mut output.emitted;
    if has_place_tag(&all_tags) {
        emit_place_node(object_id, lat, lon, &all_tags, out);
    }

    let address = node_address_tags(object_id, &all_tags, out);
    if is_poi(&all_tags) {
        emit_poi(
            PoiCandidate {
                object_type: OsmObjectType::Node,
                object_id,
                lat,
                lon,
                location_precision: LocationPrecision::Point,
                tags: poi_tags(&all_tags, address.as_ref()),
            },
            out,
        );
    } else if let Some(tags) = address {
        emit_address(
            AddressCandidate {
                object_type: OsmObjectType::Node,
                object_id,
                lat,
                lon,
                location_precision: LocationPrecision::Point,
                tags,
            },
            out,
        );
    }
}

/// The node's `addr:*` tags when they state a valid address. Invalid ones are
/// rejected whether or not the node is also a POI.
fn node_address_tags(
    object_id: i64,
    all_tags: &BTreeMap<String, String>,
    out: &mut Emitted,
) -> Option<BTreeMap<String, String>> {
    let tags = collect_addr_tags_from_map(all_tags);
    if tags.is_empty() {
        return None;
    }

    if has_interpolation_tag(&tags) {
        out.reject(
            CandidateIssue::InterpolationUnsupportedObject,
            OsmObjectType::Node,
            object_id,
            all_tags,
            Some(&tags),
            Some("interpolation"),
        );
        return None;
    }

    if let Err(issue) = validate_address_tags(&tags) {
        out.reject(
            issue,
            OsmObjectType::Node,
            object_id,
            all_tags,
            Some(&tags),
            Some("address"),
        );
        return None;
    }
    Some(tags)
}

/// `refs` decodes the node list on first use; most tagged ways are not kept.
fn scan_way<'a>(
    output: &mut ScanOutput,
    object_id: i64,
    all_tags: BTreeMap<String, String>,
    refs: impl Fn() -> &'a [i64],
) {
    let feature = |kind, tags| {
        let refs = refs();
        (
            WayFeature {
                kind,
                way_id: object_id,
                node_count: refs.len() as u32,
                tags,
            },
            refs.to_vec(),
        )
    };

    if has_admin_boundary_tags(&all_tags) && !refs().is_empty() {
        output
            .ways
            .push(feature(WayKind::Boundary, boundary_tags(&all_tags)));
    }

    if has_highway_tag(&all_tags) {
        if street_name(&all_tags).is_some() {
            if refs().is_empty() {
                output.emitted.reject(
                    CandidateIssue::StreetUnresolvedGeometry,
                    OsmObjectType::Way,
                    object_id,
                    &all_tags,
                    Some(&BTreeMap::new()),
                    Some("street"),
                );
            } else {
                output
                    .ways
                    .push(feature(WayKind::Street, street_tags(&all_tags)));
            }
        } else {
            output.emitted.reject_in_report(
                missing_street_name_issue(&all_tags),
                OsmObjectType::Way,
                object_id,
                &all_tags,
                None,
                None,
            );
        }
    }

    let tags = collect_addr_tags_from_map(&all_tags);
    if has_interpolation_tag(&tags) {
        if refs().is_empty() {
            output.emitted.reject(
                CandidateIssue::InterpolationWayWithoutNodes,
                OsmObjectType::Way,
                object_id,
                &all_tags,
                Some(&tags),
                Some("interpolation"),
            );
        } else {
            output.ways.push(feature(WayKind::Interpolation, tags));
        }
        return;
    }

    let address = if tags.is_empty() {
        None
    } else if let Err(issue) = validate_address_tags(&tags) {
        output.emitted.reject(
            issue,
            OsmObjectType::Way,
            object_id,
            &all_tags,
            Some(&tags),
            Some("address"),
        );
        None
    } else {
        Some(tags)
    };

    let (kind, tags, layer_hint) = if is_poi(&all_tags) {
        (WayKind::Poi, poi_tags(&all_tags, address.as_ref()), "poi")
    } else if let Some(tags) = address {
        (WayKind::Address, tags, "address")
    } else {
        return;
    };
    if refs().is_empty() {
        output.emitted.reject(
            CandidateIssue::WayWithoutResolvedNodes,
            OsmObjectType::Way,
            object_id,
            &all_tags,
            None,
            Some(layer_hint),
        );
        return;
    }
    output.ways.push(feature(kind, tags));
}

fn scan_relation(
    output: &mut ScanOutput,
    object_id: i64,
    all_tags: BTreeMap<String, String>,
    members: Vec<BoundaryRelationMember>,
) {
    if has_admin_boundary_tags(&all_tags) {
        if !members.is_empty() {
            output.relations.push(BoundaryRelationStub {
                object_id,
                members,
                tags: boundary_tags(&all_tags),
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
    output.emitted.reject(
        issue,
        OsmObjectType::Relation,
        object_id,
        &all_tags,
        Some(&tags),
        Some(layer_hint),
    );
}
