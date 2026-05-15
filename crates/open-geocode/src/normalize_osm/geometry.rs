use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use anyhow::{Context, Result};
use osmpbf::Element;

use super::{collector::AddressWayStub, osm_reader::element_reader_with_progress};

pub(crate) fn resolve_required_node_locations(
    input: &Path,
    required_node_ids: &HashSet<i64>,
) -> Result<HashMap<i64, (f64, f64)>> {
    let mut node_locations = HashMap::with_capacity(required_node_ids.len());
    let (reader, progress) = element_reader_with_progress(input, "2/3 resolve required nodes")?;

    reader
        .for_each(|element| match element {
            Element::DenseNode(node) => {
                if required_node_ids.contains(&node.id()) {
                    node_locations.insert(node.id(), (node.lat(), node.lon()));
                }
            }
            Element::Node(node) => {
                if required_node_ids.contains(&node.id()) {
                    node_locations.insert(node.id(), (node.lat(), node.lon()));
                }
            }
            Element::Way(_) | Element::Relation(_) => {}
        })
        .with_context(|| format!("failed to resolve node locations from {}", input.display()))?;
    progress.finish_with_message("2/3 resolve required nodes complete");

    Ok(node_locations)
}

pub(crate) fn resolve_way_points(
    stub: &AddressWayStub,
    node_locations: &HashMap<i64, (f64, f64)>,
) -> Option<Vec<(f64, f64)>> {
    let mut points = Vec::with_capacity(stub.node_refs.len());
    for node_id in &stub.node_refs {
        points.push(*node_locations.get(node_id)?);
    }
    Some(points)
}

pub(crate) fn centroid(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    if points.is_empty() {
        return None;
    }

    if points.len() >= 4 && points.first() == points.last() {
        polygon_centroid(points).or_else(|| average_point(points))
    } else {
        average_point(points)
    }
}

fn average_point(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let count = points.len() as f64;
    let lat = points.iter().map(|(lat, _)| lat).sum::<f64>() / count;
    let lon = points.iter().map(|(_, lon)| lon).sum::<f64>() / count;
    Some((lat, lon))
}

fn polygon_centroid(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let mut signed_area = 0.0;
    let mut centroid_lon = 0.0;
    let mut centroid_lat = 0.0;

    for window in points.windows(2) {
        let (lat_a, lon_a) = window[0];
        let (lat_b, lon_b) = window[1];
        let cross = lon_a.mul_add(lat_b, -(lon_b * lat_a));
        signed_area += cross;
        centroid_lon += (lon_a + lon_b) * cross;
        centroid_lat += (lat_a + lat_b) * cross;
    }

    if signed_area.abs() < f64::EPSILON {
        return None;
    }

    let signed_area = signed_area * 0.5;
    Some((
        centroid_lat / (6.0 * signed_area),
        centroid_lon / (6.0 * signed_area),
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn computes_closed_way_centroid() {
        let points = [(0.0, 0.0), (0.0, 2.0), (2.0, 2.0), (2.0, 0.0), (0.0, 0.0)];

        let (lat, lon) = centroid(&points).expect("centroid");

        assert!((lat - 1.0).abs() < 0.000001);
        assert!((lon - 1.0).abs() < 0.000001);
    }

    #[test]
    fn resolves_only_complete_way_points() {
        let stub = AddressWayStub {
            object_id: 1,
            node_refs: vec![10, 11],
            tags: BTreeMap::new(),
        };
        let complete = HashMap::from([(10, (1.0, 2.0)), (11, (3.0, 4.0))]);
        let incomplete = HashMap::from([(10, (1.0, 2.0))]);

        assert_eq!(
            resolve_way_points(&stub, &complete),
            Some(vec![(1.0, 2.0), (3.0, 4.0)])
        );
        assert_eq!(resolve_way_points(&stub, &incomplete), None);
    }
}
