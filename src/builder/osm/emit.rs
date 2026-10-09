//! Assemble way geometry from the joined coordinates and emit records.
//!
//! Way features (in owner order) and resolved coordinates (sorted by owner and
//! position) are read in lockstep, so each way's vertices arrive together.
//! Chunks of ways are assembled in parallel and merged in order.

use std::collections::HashMap;

use anyhow::Result;
use rayon::prelude::*;

use crate::{
    builder::report::CandidateIssue,
    extsort::{Sorted, SpoolReader},
    record::{LocationPrecision, OsmObjectType},
};

use super::{
    address::{AddressCandidate, emit_address},
    boundary::{BoundaryWay, Vertex},
    emitted::Emitted,
    geometry::centroid,
    interpolation::{InterpolationNode, emit_interpolation},
    poi::{PoiCandidate, emit_poi},
    spill::{ResolvedRef, WayFeature, WayKind},
    street::emit_street,
};

const FEATURES_PER_CHUNK: usize = 16_384;

#[derive(Debug, Default)]
pub(crate) struct FeatureOutput {
    pub emitted: Emitted,
    pub boundary_ways: Vec<BoundaryWay>,
}

/// Emit every way feature, then return the resolved vertices of the remaining
/// owners (boundary relation members, numbered after the features).
pub(crate) fn emit_features(
    features: SpoolReader<WayFeature>,
    resolved: Sorted<ResolvedRef>,
    mut sink: impl FnMut(FeatureOutput) -> Result<()>,
) -> Result<HashMap<u64, Vec<Vertex>>> {
    let mut resolved = resolved.peekable();
    let mut owner = 0u64;
    let mut features = features.peekable();
    while features.peek().is_some() {
        let mut chunk = Vec::with_capacity(FEATURES_PER_CHUNK);
        for feature in features.by_ref().take(FEATURES_PER_CHUNK) {
            let mut refs = Vec::new();
            while let Some(next) =
                resolved.next_if(|next| next.as_ref().map_or(true, |next| next.owner <= owner))
            {
                refs.push(next?);
            }
            chunk.push((feature?, refs));
            owner += 1;
        }
        let outputs = chunk
            .into_par_iter()
            .map(|(feature, refs)| {
                let mut output = FeatureOutput::default();
                emit_feature(feature, refs, &mut output);
                output
            })
            .collect::<Vec<_>>();
        for output in outputs {
            sink(output)?;
        }
    }

    let mut members: HashMap<u64, Vec<Vertex>> = HashMap::new();
    for item in resolved {
        let item = item?;
        members.entry(item.owner).or_default().push(Vertex {
            node_id: item.node_id,
            lat: item.lat(),
            lon: item.lon(),
        });
    }
    Ok(members)
}

fn emit_feature(feature: WayFeature, refs: Vec<ResolvedRef>, output: &mut FeatureOutput) {
    // Positions are unique per way, so every vertex resolved iff the counts match.
    let complete = refs.len() == feature.node_count as usize;
    let out = &mut output.emitted;
    let points = || {
        refs.iter()
            .map(|node| (node.lat(), node.lon()))
            .collect::<Vec<_>>()
    };
    match feature.kind {
        WayKind::Address => {
            let center = if complete { centroid(&points()) } else { None };
            match center {
                Some((lat, lon)) => emit_address(
                    AddressCandidate {
                        object_type: OsmObjectType::Way,
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
                    OsmObjectType::Way,
                    feature.way_id,
                    &feature.tags,
                    Some(&feature.tags),
                    Some("address"),
                ),
            }
        }
        WayKind::Poi => {
            let center = if complete { centroid(&points()) } else { None };
            match center {
                Some((lat, lon)) => emit_poi(
                    PoiCandidate {
                        object_type: OsmObjectType::Way,
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
                    OsmObjectType::Way,
                    feature.way_id,
                    &feature.tags,
                    None,
                    Some("poi"),
                ),
            }
        }
        WayKind::Street => {
            let points = complete.then(points);
            emit_street(feature.way_id, &feature.tags, points.as_deref(), out);
        }
        WayKind::Interpolation => {
            let nodes = complete.then(|| {
                refs.into_iter()
                    .map(|node| InterpolationNode {
                        node_id: node.node_id,
                        lat: node.lat(),
                        lon: node.lon(),
                        addr_tags: node.tags,
                    })
                    .collect::<Vec<_>>()
            });
            emit_interpolation(feature.way_id, &feature.tags, nodes.as_deref(), out);
        }
        WayKind::Boundary => {
            if complete {
                output.boundary_ways.push(BoundaryWay {
                    object_id: feature.way_id,
                    tags: feature.tags,
                    vertices: refs
                        .iter()
                        .map(|node| Vertex {
                            node_id: node.node_id,
                            lat: node.lat(),
                            lon: node.lon(),
                        })
                        .collect(),
                });
            }
        }
    }
}
