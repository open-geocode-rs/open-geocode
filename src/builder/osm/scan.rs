//! Pass 1: classify every OSM object in one parallel scan.
//!
//! Address, POI and place nodes become records immediately, because a node
//! carries its own coordinates. Ways that need geometry become [`WayFeature`]s plus
//! their node references; boundary relations are kept as stubs. Each block is
//! classified independently, so blocks are processed in parallel and merged in
//! file order. The way pass classifies the ways again ([`way_features`])
//! rather than keeping them between the passes: a reread of the way blocks
//! costs less than writing every kept way's tags to scratch and back.

use std::{cell::OnceCell, collections::BTreeMap};

use osmpbf::{Element, PrimitiveBlock, RelMemberType};

use crate::{
    builder::report::CandidateIssue,
    record::{LocationPrecision, ObjectType},
};

use super::{
    address::{
        AddressCandidate, collect_addr_tags_from_map, collect_clean_tags, emit_address,
        validate_address_tags,
    },
    boundary::{
        BoundaryRelationMember, BoundaryRelationStub, boundary_tags, has_area_tags,
        relation_member_role,
    },
    emitted::Emitted,
    interpolation::has_interpolation_tag,
    place::{emit_place_node, has_place_tag},
    poi::{Drawn, PoiCandidate, count_left_out, emit_poi, is_place_key, is_poi, poi_tags},
    spill::{WayFeature, WayKind},
    street::{has_highway_tag, is_street, missing_street_name_issue, street_name, street_tags},
};

/// Whether an element tagged with `keys` may be anything the scan keeps or
/// reports: a place, an address, a street, an area, or a POI, which needs a
/// name and a POI key. It reads the keys alone, before any tag is copied,
/// and is looser than the classifiers it guards: an element it stops is one
/// they would drop, and most tagged elements (buildings, trees, unnamed
/// paths) are.
pub(crate) fn may_matter<'a>(keys: impl Iterator<Item = &'a str>) -> bool {
    let (mut named, mut category) = (false, false);
    for key in keys {
        match key {
            "place" | "highway" | "boundary" => return true,
            // A branded place unnamed is named by its brand.
            "name" | "brand" => named = true,
            key if key.starts_with("addr:") => return true,
            key if is_place_key(key) => category = true,
            _ => {}
        }
    }
    named && category
}

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
                if may_matter(node.tags().map(|(key, _)| key)) {
                    scan_node(
                        &mut output,
                        node.id(),
                        node.lat(),
                        node.lon(),
                        &collect_clean_tags(node.tags()),
                    );
                }
            }
            Element::Node(node) => {
                output.has_nodes = true;
                output.emitted.report.scanned.nodes += 1;
                if may_matter(node.tags().map(|(key, _)| key)) {
                    scan_node(
                        &mut output,
                        node.id(),
                        node.lat(),
                        node.lon(),
                        &collect_clean_tags(node.tags()),
                    );
                }
            }
            Element::Way(way) => {
                output.has_ways = true;
                output.emitted.report.scanned.ways += 1;
                if !may_matter(way.tags().map(|(key, _)| key)) {
                    continue;
                }
                let tags = collect_clean_tags(way.tags());
                if !tags.is_empty() {
                    let refs = OnceCell::new();
                    scan_way(
                        &mut output.ways,
                        Some(&mut output.emitted),
                        way.id(),
                        &tags,
                        || {
                            refs.get_or_init(|| way.refs().collect::<Vec<_>>())
                                .as_slice()
                        },
                    );
                }
            }
            Element::Relation(relation) => {
                output.emitted.report.scanned.relations += 1;
                if !may_matter(relation.tags().map(|(key, _)| key)) {
                    continue;
                }
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
                    &collect_clean_tags(relation.tags()),
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
    all_tags: &BTreeMap<String, String>,
) {
    if all_tags.is_empty() {
        return;
    }
    let out = &mut output.emitted;
    if has_place_tag(all_tags) {
        emit_place_node(object_id, lat, lon, all_tags, out);
    }

    let address = node_address_tags(object_id, all_tags, out);
    let poi = is_poi(all_tags, Drawn::Point);
    if !poi {
        count_left_out(all_tags, Drawn::Point, out);
    }
    if poi {
        emit_poi(
            &PoiCandidate {
                object_type: ObjectType::Node,
                object_id,
                lat,
                lon,
                location_precision: LocationPrecision::Point,
                tags: poi_tags(all_tags, address.as_ref()),
            },
            out,
        );
    } else if let Some(tags) = address {
        emit_address(
            &AddressCandidate {
                object_type: ObjectType::Node,
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
            ObjectType::Node,
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
            ObjectType::Node,
            object_id,
            all_tags,
            Some(&tags),
            Some("address"),
        );
        return None;
    }
    Some(tags)
}

/// The features a way with `all_tags` (not empty) becomes, as the scan
/// classifies it, without counting what it rejects again.
pub(crate) fn way_features<'a>(
    object_id: i64,
    all_tags: &BTreeMap<String, String>,
    refs: impl Fn() -> &'a [i64],
) -> Vec<(WayFeature, Vec<i64>)> {
    let mut ways = Vec::new();
    scan_way(&mut ways, None, object_id, all_tags, refs);
    ways
}

/// `refs` decodes the node list on first use; most tagged ways are not kept.
/// Rejections go to `out` when given.
fn scan_way<'a>(
    ways: &mut Vec<(WayFeature, Vec<i64>)>,
    mut out: Option<&mut Emitted>,
    object_id: i64,
    all_tags: &BTreeMap<String, String>,
    refs: impl Fn() -> &'a [i64],
) {
    let feature = |kind, tags| {
        (
            WayFeature {
                kind,
                way_id: object_id,
                tags,
            },
            refs().to_vec(),
        )
    };

    if has_area_tags(all_tags) && !refs().is_empty() {
        ways.push(feature(WayKind::Boundary, boundary_tags(all_tags)));
    }

    if has_highway_tag(all_tags) && !is_street(all_tags) {
        if let Some(out) = out.as_deref_mut() {
            out.reject_in_report(
                CandidateIssue::HighwayNotAStreet,
                ObjectType::Way,
                object_id,
                all_tags,
                None,
                None,
            );
        }
    } else if has_highway_tag(all_tags) {
        if street_name(all_tags).is_some() {
            if refs().is_empty() {
                if let Some(out) = out.as_deref_mut() {
                    out.reject(
                        CandidateIssue::StreetUnresolvedGeometry,
                        ObjectType::Way,
                        object_id,
                        all_tags,
                        Some(&BTreeMap::new()),
                        Some("street"),
                    );
                }
            } else {
                ways.push(feature(WayKind::Street, street_tags(all_tags)));
            }
        } else if let Some(out) = out.as_deref_mut() {
            out.reject_in_report(
                missing_street_name_issue(all_tags),
                ObjectType::Way,
                object_id,
                all_tags,
                None,
                None,
            );
        }
    }

    let tags = collect_addr_tags_from_map(all_tags);
    if has_interpolation_tag(&tags) {
        if refs().is_empty() {
            if let Some(out) = out.as_deref_mut() {
                out.reject(
                    CandidateIssue::InterpolationWayWithoutNodes,
                    ObjectType::Way,
                    object_id,
                    all_tags,
                    Some(&tags),
                    Some("interpolation"),
                );
            }
        } else {
            ways.push(feature(WayKind::Interpolation, tags));
        }
        return;
    }

    let address = if tags.is_empty() {
        None
    } else if let Err(issue) = validate_address_tags(&tags) {
        if let Some(out) = out.as_deref_mut() {
            out.reject(
                issue,
                ObjectType::Way,
                object_id,
                all_tags,
                Some(&tags),
                Some("address"),
            );
        }
        None
    } else {
        Some(tags)
    };

    let poi = is_poi(all_tags, Drawn::Area);
    if !poi && let Some(out) = out.as_deref_mut() {
        count_left_out(all_tags, Drawn::Area, out);
    }
    let (kind, tags, layer_hint) = if poi {
        (WayKind::Poi, poi_tags(all_tags, address.as_ref()), "poi")
    } else if let Some(tags) = address {
        (WayKind::Address, tags, "address")
    } else {
        return;
    };
    if refs().is_empty() {
        if let Some(out) = out {
            out.reject(
                CandidateIssue::WayWithoutResolvedNodes,
                ObjectType::Way,
                object_id,
                all_tags,
                None,
                Some(layer_hint),
            );
        }
        return;
    }
    ways.push(feature(kind, tags));
}

/// Whether a relation's members draw its area: an admin boundary, or a
/// multipolygon, not a route, site or collection that happens to carry a
/// `place` tag.
fn is_area_relation(tags: &BTreeMap<String, String>) -> bool {
    tags.get("boundary").map(String::as_str) == Some("administrative")
        || matches!(
            tags.get("type").map(String::as_str),
            None | Some("multipolygon" | "boundary")
        )
}

fn scan_relation(
    output: &mut ScanOutput,
    object_id: i64,
    all_tags: &BTreeMap<String, String>,
    members: Vec<BoundaryRelationMember>,
) {
    if has_area_tags(all_tags) && is_area_relation(all_tags) {
        if !members.is_empty() {
            output.relations.push(BoundaryRelationStub {
                object_id,
                members,
                tags: boundary_tags(all_tags),
            });
        }
        return;
    }
    // A place drawn as several outlines: a stadium, a campus, a castle and
    // its moat. Its rings are assembled with the boundaries', and it becomes
    // a place at a point inside them (see `boundary::place_relations`).
    if is_area_relation(all_tags) && is_poi(all_tags, Drawn::Area) {
        if !members.is_empty() {
            let address = collect_addr_tags_from_map(all_tags);
            let address =
                (!address.is_empty() && validate_address_tags(&address).is_ok()).then_some(address);
            output.relations.push(BoundaryRelationStub {
                object_id,
                members,
                tags: poi_tags(all_tags, address.as_ref()),
            });
        }
        return;
    }

    let tags = collect_addr_tags_from_map(all_tags);
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
        ObjectType::Relation,
        object_id,
        all_tags,
        Some(&tags),
        Some(layer_hint),
    );
}
