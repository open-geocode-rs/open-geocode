//! The way pass: read the way blocks again, give each kept way its nodes'
//! coordinates from the node store, and emit its records.
//!
//! Each block is assembled on its own, so blocks run in parallel and their
//! records are merged in file order, the order the scan kept the ways in.

use std::collections::{BTreeMap, HashSet};

use osmpbf::{Element, PrimitiveBlock};

use crate::{
    builder::report::CandidateIssue,
    record::{LocationPrecision, ObjectType},
};

use super::{
    address::{AddressCandidate, collect_clean_tags, emit_address},
    boundary::{BoundaryWay, Vertex},
    emitted::Emitted,
    geometry::centroid,
    interpolation::{InterpolationNode, emit_interpolation},
    locations::{Location, Locations},
    poi::{PoiCandidate, emit_poi},
    scan::way_features,
    spill::{WayFeature, WayKind},
    street::emit_street,
};

#[derive(Debug, Default)]
pub(crate) struct FeatureOutput {
    pub emitted: Emitted,
    pub boundary_ways: Vec<BoundaryWay>,
}

/// One node of a way, with its address tags when the way reads them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WayNode {
    pub id: i64,
    pub location: Location,
    pub tags: Option<BTreeMap<String, String>>,
}

/// What one way block gave.
#[derive(Debug, Default)]
pub(crate) struct Assembled {
    pub output: FeatureOutput,
    /// Boundary relation member ways, in file order: each with its vertices
    /// when every one is stored, and how many are.
    pub members: Vec<(i64, Option<Vec<Vertex>>, u64)>,
    /// Node references of the block's kept ways that have coordinates.
    pub resolved: u64,
}

/// The nodes of `refs`, when `locations` has every one, and how many it has.
fn resolve(refs: &[i64], locations: &Locations, tags: bool) -> (Option<Vec<WayNode>>, u64) {
    let mut found = 0;
    let mut nodes = Some(Vec::with_capacity(refs.len()));
    for id in refs {
        match locations.get(*id) {
            Some(location) => {
                found += 1;
                if let Some(nodes) = &mut nodes {
                    nodes.push(WayNode {
                        id: *id,
                        location,
                        tags: if tags {
                            locations.tags(*id).cloned()
                        } else {
                            None
                        },
                    });
                }
            }
            None => nodes = None,
        }
    }
    (nodes, found)
}

/// Emit the kept ways of `block` and gather the vertices of the boundary
/// relation members among its ways (`members`).
pub(crate) fn assemble(
    block: &PrimitiveBlock,
    locations: &Locations,
    members: &HashSet<i64>,
) -> Assembled {
    let mut assembled = Assembled::default();
    for element in block.elements() {
        let Element::Way(way) = element else {
            continue;
        };
        let refs = std::cell::OnceCell::new();
        let refs = || {
            refs.get_or_init(|| way.refs().collect::<Vec<_>>())
                .as_slice()
        };
        let tags = collect_clean_tags(way.tags());
        if !tags.is_empty() {
            for (feature, way_refs) in way_features(way.id(), tags, refs) {
                let (nodes, found) =
                    resolve(&way_refs, locations, feature.kind == WayKind::Interpolation);
                assembled.resolved += found;
                emit_feature(feature, nodes, &mut assembled.output);
            }
        }
        if members.contains(&way.id()) {
            let (nodes, found) = resolve(refs(), locations, false);
            let vertices = nodes.map(|nodes| nodes.iter().map(vertex).collect());
            assembled.members.push((way.id(), vertices, found));
        }
    }
    assembled
}

fn vertex(node: &WayNode) -> Vertex {
    Vertex {
        node_id: node.id,
        lat: node.location.lat(),
        lon: node.location.lon(),
    }
}

/// Emit the records of a kept way whose nodes are `nodes`, `None` when any
/// lacks coordinates.
pub(crate) fn emit_feature(
    feature: WayFeature,
    nodes: Option<Vec<WayNode>>,
    output: &mut FeatureOutput,
) {
    let out = &mut output.emitted;
    let points = |nodes: &[WayNode]| {
        nodes
            .iter()
            .map(|node| (node.location.lat(), node.location.lon()))
            .collect::<Vec<_>>()
    };
    match feature.kind {
        WayKind::Address => {
            let center = nodes.as_deref().and_then(|nodes| centroid(&points(nodes)));
            match center {
                Some((lat, lon)) => emit_address(
                    AddressCandidate {
                        object_type: ObjectType::Way,
                        object_id: feature.way_id,
                        lat,
                        lon,
                        location_precision: LocationPrecision::Centroid,
                        tags: feature.tags,
                    },
                    out,
                ),
                None => out.reject(
                    CandidateIssue::WayWithoutResolvedNodes,
                    ObjectType::Way,
                    feature.way_id,
                    &feature.tags,
                    Some(&feature.tags),
                    Some("address"),
                ),
            }
        }
        WayKind::Poi => {
            let center = nodes.as_deref().and_then(|nodes| centroid(&points(nodes)));
            match center {
                Some((lat, lon)) => emit_poi(
                    PoiCandidate {
                        object_type: ObjectType::Way,
                        object_id: feature.way_id,
                        lat,
                        lon,
                        location_precision: LocationPrecision::Centroid,
                        tags: feature.tags,
                    },
                    out,
                ),
                None => out.reject(
                    CandidateIssue::WayWithoutResolvedNodes,
                    ObjectType::Way,
                    feature.way_id,
                    &feature.tags,
                    None,
                    Some("poi"),
                ),
            }
        }
        WayKind::Street => {
            let points = nodes.as_deref().map(points);
            emit_street(feature.way_id, &feature.tags, points.as_deref(), out);
        }
        WayKind::Interpolation => {
            let nodes = nodes.map(|nodes| {
                nodes
                    .into_iter()
                    .map(|node| InterpolationNode {
                        node_id: node.id,
                        lat: node.location.lat(),
                        lon: node.location.lon(),
                        addr_tags: node.tags,
                    })
                    .collect::<Vec<_>>()
            });
            emit_interpolation(feature.way_id, &feature.tags, nodes.as_deref(), out);
        }
        WayKind::Boundary => {
            if let Some(nodes) = nodes {
                output.boundary_ways.push(BoundaryWay {
                    object_id: feature.way_id,
                    tags: feature.tags,
                    vertices: nodes.iter().map(vertex).collect(),
                });
            }
        }
    }
}
