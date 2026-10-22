use std::collections::{BTreeMap, HashMap};

use crate::{
    builder::report::CandidateIssue,
    record::{ObjectType, Record, SourceProvenance, StreetRecord},
    spatial_index::haversine_m,
    util::geo::{line_geometry, line_parts},
};

use super::{
    emitted::Emitted,
    geometry::line_string_geometry,
    tags::{OsmTags, alternate_names, name_tags},
};

/// Tags a street feature keeps after the scan besides its names: what the
/// audit needs to explain a rejection.
const STREET_TAG_KEYS: [&str; 2] = ["highway", "ref"];

pub(crate) fn has_highway_tag(tags: &BTreeMap<String, String>) -> bool {
    tags.has("highway")
}

pub(crate) fn street_name(tags: &BTreeMap<String, String>) -> Option<String> {
    tags.cleaned("name")
}

pub(crate) fn street_tags(tags: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    STREET_TAG_KEYS
        .into_iter()
        .filter_map(|key| Some((key.to_string(), tags.get(key)?.clone())))
        .chain(name_tags(tags))
        .collect()
}

pub(crate) fn missing_street_name_issue(tags: &BTreeMap<String, String>) -> CandidateIssue {
    if tags.has("ref") {
        CandidateIssue::StreetRefOnlyName
    } else {
        CandidateIssue::StreetMissingName
    }
}

/// Emit a street from its resolved vertices (`None` when any node is missing).
pub(crate) fn emit_street(
    way_id: i64,
    tags: &BTreeMap<String, String>,
    points: Option<&[(f64, f64)]>,
    out: &mut Emitted,
) {
    let record = street_name(tags).zip(points).and_then(|(name, points)| {
        let built = line_string_geometry(points)?;
        Some(StreetRecord {
            alt_names: alternate_names(tags, &name),
            locality: None,
            name,
            geometry: built.geometry,
            representative_point: built.representative_point,
            source: SourceProvenance::osm(ObjectType::Way, way_id),
        })
    });
    match record {
        Some(record) => {
            out.report.accept_street();
            out.records.push(Record::Street(record));
        }
        None => out.reject(
            CandidateIssue::StreetUnresolvedGeometry,
            ObjectType::Way,
            way_id,
            tags,
            Some(&BTreeMap::new()),
            Some("street"),
        ),
    }
}

/// Ways of one name in one area this close at an end are one street: the
/// ways of a road split at its junctions share a node there, and a
/// roundabout or a bridge named otherwise leaves a gap of tens of metres.
/// Two roads of one name in one locality lie farther apart than that.
pub(crate) const JOIN_GAP_M: f64 = 50.0;

/// Metres in a degree of latitude, to size the grid ends are looked up in.
const METRES_PER_DEGREE: f64 = 111_320.0;

/// The ways of one street name in one area, as streets: a way whose end
/// lies within [`JOIN_GAP_M`] of another's line joins it, and a street is
/// every way so joined. It keeps the name, source and id of its lowest way,
/// every other name its ways have, a line part per way, and the vertex
/// nearest the middle of its ways as its point.
pub(crate) fn join_ways(mut ways: Vec<StreetRecord>) -> Vec<StreetRecord> {
    if ways.len() < 2 {
        return ways;
    }
    ways.sort_by_key(|way| way.source.object_id);
    let lines = ways
        .iter()
        .map(|way| {
            line_parts(&way.geometry)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|position| match position.as_slice() {
                    [lon, lat, ..] if lon.is_finite() && lat.is_finite() => Some([*lon, *lat]),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    // A cell is the gap wide, so any vertex within the gap of an end lies in
    // a cell beside the end's. The ways of one area span little latitude, so
    // one longitude scale serves them all.
    let latitude = ways
        .iter()
        .map(|way| way.representative_point[1])
        .sum::<f64>()
        / ways.len() as f64;
    let cell_lat = JOIN_GAP_M / METRES_PER_DEGREE;
    let cell_lon = cell_lat / latitude.to_radians().cos().max(0.01);
    let cell = |[lon, lat]: [f64; 2]| {
        (
            (lon / cell_lon).floor() as i64,
            (lat / cell_lat).floor() as i64,
        )
    };
    let mut grid = HashMap::<(i64, i64), Vec<(usize, [f64; 2])>>::new();
    for (way, points) in lines.iter().enumerate() {
        for point in points {
            grid.entry(cell(*point)).or_default().push((way, *point));
        }
    }
    let mut parent = (0..ways.len()).collect::<Vec<_>>();
    fn root(parent: &mut [usize], mut way: usize) -> usize {
        while parent[way] != way {
            parent[way] = parent[parent[way]];
            way = parent[way];
        }
        way
    }
    for (way, points) in lines.iter().enumerate() {
        for end in [points.first(), points.last()].into_iter().flatten() {
            let (x, y) = cell(*end);
            for near in (x - 1..=x + 1).flat_map(|x| (y - 1..=y + 1).map(move |y| (x, y))) {
                for (other, point) in grid.get(&near).into_iter().flatten() {
                    if *other != way
                        && haversine_m(end[0], end[1], point[0], point[1]) <= JOIN_GAP_M
                    {
                        let (left, right) = (root(&mut parent, way), root(&mut parent, *other));
                        parent[left.max(right)] = left.min(right);
                    }
                }
            }
        }
    }
    let mut streets = Vec::<(usize, Vec<usize>)>::new();
    let mut street_of = HashMap::<usize, usize>::new();
    for way in 0..ways.len() {
        let first = root(&mut parent, way);
        match street_of.get(&first) {
            Some(&at) => streets[at].1.push(way),
            None => {
                street_of.insert(first, streets.len());
                streets.push((first, vec![way]));
            }
        }
    }
    let mut ways = ways.into_iter().map(Some).collect::<Vec<_>>();
    streets
        .into_iter()
        .map(|(_, members)| {
            join(
                members
                    .into_iter()
                    .map(|way| {
                        (
                            ways[way].take().expect("a way joins one street"),
                            lines[way].as_slice(),
                        )
                    })
                    .collect(),
            )
        })
        .collect()
}

/// One street of `members`, lowest way first, each with its vertices.
fn join(mut members: Vec<(StreetRecord, &[[f64; 2]])>) -> StreetRecord {
    if members.len() == 1 {
        return members.remove(0).0;
    }
    let length = |points: &[[f64; 2]]| {
        points
            .windows(2)
            .map(|pair| haversine_m(pair[0][0], pair[0][1], pair[1][0], pair[1][1]))
            .sum::<f64>()
    };
    let (mut total, mut lon, mut lat) = (0.0, 0.0, 0.0);
    for (way, points) in &members {
        let weight = length(points).max(f64::EPSILON);
        total += weight;
        lon += way.representative_point[0] * weight;
        lat += way.representative_point[1] * weight;
    }
    let middle = [lon / total, lat / total];
    let distance = |point: &[f64; 2]| haversine_m(middle[0], middle[1], point[0], point[1]);
    let point = members
        .iter()
        .flat_map(|(_, points)| points.iter())
        .min_by(|left, right| distance(left).total_cmp(&distance(right)))
        .copied()
        .unwrap_or(middle);
    let mut members = members.into_iter().map(|(way, _)| way);
    let mut street = members.next().expect("a street has a way");
    let mut parts = line_parts(&street.geometry)
        .into_iter()
        .flatten()
        .map(<[_]>::to_vec)
        .collect::<Vec<_>>();
    for way in members {
        parts.extend(
            line_parts(&way.geometry)
                .into_iter()
                .flatten()
                .map(<[_]>::to_vec),
        );
        for name in std::iter::once(way.name).chain(way.alt_names) {
            let known = std::iter::once(&street.name)
                .chain(&street.alt_names)
                .any(|known| known.to_lowercase() == name.to_lowercase());
            if !known {
                street.alt_names.push(name);
            }
        }
    }
    street.geometry = line_geometry(parts);
    street.representative_point = point;
    street
}

/// Whether a place of `place_type` is a settlement or a part of one, by the
/// OSM `place=` values for them. An admin area that is none (a ward, a local
/// board) is not where its streets are said to be.
pub(crate) fn is_settlement(place_type: &str) -> bool {
    matches!(
        place_type,
        "city" | "town" | "village" | "hamlet" | "suburb" | "borough" | "quarter" | "municipality"
    )
}

#[cfg(test)]
mod tests {
    use geojson::GeometryValue;

    use super::*;

    fn tags() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("highway".to_string(), "residential".to_string()),
            ("name".to_string(), " King   Street ".to_string()),
            ("surface".to_string(), "asphalt".to_string()),
        ])
    }

    #[test]
    fn builds_street_record_from_named_highway_way() {
        let mut out = Emitted::default();
        emit_street(
            99,
            &street_tags(&tags()),
            Some(&[(43.64, -79.41), (43.66, -79.36)]),
            &mut out,
        );

        let Some(Record::Street(record)) = out.records.first() else {
            panic!("expected a street record");
        };
        assert_eq!(record.id(), "osm:way:99");
        assert_eq!(record.name, "King Street");
        assert!((record.representative_point[0] - -79.385).abs() < 0.000001);
        assert!((record.representative_point[1] - 43.65).abs() < 0.000001);
        match &record.geometry.value {
            GeometryValue::LineString { coordinates } => {
                assert_eq!(coordinates.len(), 2);
                assert_eq!(coordinates[0].as_slice(), &[-79.41, 43.64]);
            }
            other => panic!("expected LineString, got {}", other.type_name()),
        }
        assert_eq!(out.report.accepted.street_segments, 1);
    }

    #[test]
    fn rejects_named_highway_without_complete_geometry() {
        let mut out = Emitted::default();
        emit_street(99, &street_tags(&tags()), None, &mut out);

        assert!(out.records.is_empty());
        assert_eq!(out.rejections.len(), 1);
        assert_eq!(out.rejections[0].reason, "street_unresolved_geometry");
        assert_eq!(out.rejections[0].layer_hint.as_deref(), Some("street"));
        assert_eq!(out.report.disposition.unresolved_geometry, 1);
    }

    #[test]
    fn keeps_only_street_tags_and_distinguishes_ref_only_highways() {
        assert_eq!(street_tags(&tags()).len(), 2);
        let ref_only = BTreeMap::from([
            ("highway".to_string(), "primary".to_string()),
            ("ref".to_string(), "401".to_string()),
        ]);
        let unnamed = BTreeMap::from([("highway".to_string(), "path".to_string())]);
        assert_eq!(
            missing_street_name_issue(&ref_only),
            CandidateIssue::StreetRefOnlyName
        );
        assert_eq!(
            missing_street_name_issue(&unnamed),
            CandidateIssue::StreetMissingName
        );
    }

    #[test]
    fn joins_the_ways_that_meet_and_keeps_a_far_one_apart() {
        let way = |id: i64, points: &[[f64; 2]], alt_names: &[&str]| {
            let coordinates = points.iter().map(|point| point.to_vec().into()).collect();
            StreetRecord {
                alt_names: alt_names.iter().map(|name| name.to_string()).collect(),
                name: "King Street".to_string(),
                locality: None,
                geometry: line_geometry(vec![coordinates]),
                representative_point: points[0],
                source: SourceProvenance::osm(ObjectType::Way, id),
            }
        };
        // 0.0003 degrees of longitude is some 30 m here: within the gap.
        let streets = join_ways(vec![
            way(3, &[[-79.002, 43.0], [-79.003, 43.0]], &["Rue King"]),
            way(1, &[[-79.0, 43.0], [-79.001, 43.0]], &[]),
            way(2, &[[-79.0013, 43.0], [-79.002, 43.0]], &["King St"]),
            way(4, &[[-79.1, 43.0], [-79.101, 43.0]], &[]),
        ]);
        assert_eq!(streets.len(), 2);
        let joined = &streets[0];
        assert_eq!(joined.id(), "osm:way:1", "named by its lowest way");
        assert_eq!(joined.alt_names, ["King St", "Rue King"]);
        let parts = line_parts(&joined.geometry).expect("a line");
        assert_eq!(parts.len(), 3);
        assert!(
            parts
                .iter()
                .flat_map(|part| part.iter())
                .any(|vertex| vertex.as_slice() == joined.representative_point),
            "its point is one of its vertices"
        );
        assert_eq!(streets[1].id(), "osm:way:4");
    }
}
