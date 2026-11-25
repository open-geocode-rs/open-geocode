use std::collections::{BTreeMap, HashSet};

use geo::MultiPolygon;
use osmpbf::{Element, PrimitiveBlock};

use crate::{
    builder::report::CandidateIssue,
    record::{Entrance, LocationPrecision, ObjectType},
};

use super::{
    address::{AddressCandidate, collect_clean_tags, emit_address},
    boundary::{BoundaryWay, Vertex},
    emitted::Emitted,
    geometry::{centroid, closed_way_polygon},
    interpolation::{InterpolationNode, emit_interpolation},
    locations::{Location, Locations},
    poi::{PoiCandidate, emit_poi, outline},
    scan::{may_matter, way_features},
    spill::{WayFeature, WayKind},
    street::emit_street,
};

#[derive(Debug, Default)]
pub(crate) struct FeatureOutput {
    pub emitted: Emitted,
    pub boundary_ways: Vec<BoundaryWay>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WayNode {
    pub id: i64,
    pub location: Location,
    pub tags: Option<BTreeMap<String, String>>,
    pub entrance: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct Assembled {
    pub output: FeatureOutput,
    pub members: Vec<(i64, Option<Vec<Vertex>>, u64)>,
    pub resolved: u64,
    pub entrances: Vec<(i64, String)>,
}

fn resolve(
    refs: &[i64],
    locations: &Locations,
    tags: bool,
    entrances: bool,
) -> (Option<Vec<WayNode>>, u64) {
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
                        entrance: if entrances {
                            locations.entrance(*id).map(str::to_string)
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
        let tags = if may_matter(way.tags().map(|(key, _)| key)) {
            collect_clean_tags(way.tags())
        } else {
            BTreeMap::new()
        };
        if !tags.is_empty() {
            for (feature, way_refs) in way_features(way.id(), &tags, refs) {
                let (nodes, found) = resolve(
                    &way_refs,
                    locations,
                    feature.kind == WayKind::Interpolation,
                    matches!(feature.kind, WayKind::Address | WayKind::Poi),
                );
                assembled.resolved += found;
                emit_feature(feature, nodes, &mut assembled.output);
            }
        }
        if members.contains(&way.id()) {
            let (nodes, found) = resolve(refs(), locations, false, true);
            assembled.entrances.extend(
                nodes
                    .iter()
                    .flatten()
                    .filter_map(|node| Some((node.id, node.entrance.clone()?))),
            );
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
    let entrances = |nodes: &[WayNode]| {
        let mut seen = HashSet::new();
        nodes
            .iter()
            .filter(|node| node.entrance.is_some() && seen.insert(node.id))
            .filter_map(|node| {
                Entrance::of_tag(
                    node.entrance.as_deref(),
                    node.location.lon(),
                    node.location.lat(),
                )
            })
            .collect::<Vec<_>>()
    };
    match feature.kind {
        WayKind::Address => {
            let center = nodes.as_deref().and_then(|nodes| centroid(&points(nodes)));
            match center {
                Some((lat, lon)) => emit_address(
                    &AddressCandidate {
                        object_type: ObjectType::Way,
                        object_id: feature.way_id,
                        lat,
                        lon,
                        location_precision: LocationPrecision::Centroid,
                        tags: feature.tags,
                        entrances: nodes.as_deref().map(entrances).unwrap_or_default(),
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
            match (center, nodes.as_deref()) {
                (Some((lat, lon)), Some(nodes)) => emit_poi(
                    &PoiCandidate {
                        object_type: ObjectType::Way,
                        object_id: feature.way_id,
                        lat,
                        lon,
                        location_precision: LocationPrecision::Centroid,
                        tags: feature.tags,
                        outline: closed_way_polygon(&points(nodes))
                            .map(|polygon| outline(&MultiPolygon::new(vec![polygon]))),
                        nodes: nodes.iter().map(|node| node.id).collect(),
                        entrances: entrances(nodes),
                    },
                    out,
                ),
                _ => out.reject(
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
