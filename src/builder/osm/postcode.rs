//! Postcode records and the areas that bound them.
//!
//! A postcode drawn by a `boundary=postal_code` way or relation is placed at a
//! point inside that area, and the records in the area that state no postcode
//! take it as context, as they take the admin areas they lie in. A postcode
//! without an area is placed at the medoid of a sample of the addresses that
//! state it.

use std::collections::{BTreeMap, HashMap};

use geo::{Area, BoundingRect, Centroid, Contains, InteriorPoint, MultiPolygon};
use geojson::Geometry;
use rayon::prelude::*;
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Rectangle},
};
use xxhash_rust::xxh3::xxh3_64;

use crate::{
    pack::RecordId,
    record::{
        AddressComponents, DerivedSourceProvenance, PostcodeRecord, SourceProvenance,
        point_geometry,
    },
    spatial_index::haversine_m,
    util::geo::point_lon_lat,
};

use super::{containment::AreaShape, tags::OsmTags};

/// Member points kept per postcode. The kept points are the ones with the lowest
/// hashes, so the sample is the same whatever order workers merge in, and the
/// memory stays bounded by the number of postcodes, not addresses.
const SAMPLE_POINTS: usize = 16;

/// The postcodes accepted addresses state, each with a bounded sample of
/// their points.
#[derive(Debug, Clone, Default)]
pub(crate) struct PostcodeAccumulator {
    groups: BTreeMap<String, PostcodeGroup>,
}

#[derive(Debug, Clone, Default)]
struct PostcodeGroup {
    record_count: u64,
    /// Distinct member points by ascending hash, at most [`SAMPLE_POINTS`].
    sample: Vec<(u64, [f64; 2])>,
}

impl PostcodeAccumulator {
    /// Count an accepted address: an address record or a POI that states one.
    pub(crate) fn accept(&mut self, address: &AddressComponents, geometry: &Geometry) {
        let Some(postcode) = address.postcode.as_deref().and_then(clean_postcode) else {
            return;
        };
        let Some([lon, lat]) = point_lon_lat(geometry) else {
            return;
        };
        let group = self.groups.entry(postcode).or_default();
        group.record_count += 1;
        group.offer((hash_point(lon, lat), [lon, lat]));
    }

    /// Fold in a worker's accumulator. Workers are merged in input order.
    pub(crate) fn merge(&mut self, other: PostcodeAccumulator) {
        for (postcode, other) in other.groups {
            let group = self.groups.entry(postcode).or_default();
            group.record_count += other.record_count;
            for point in other.sample {
                group.offer(point);
            }
        }
    }

    /// One record per postcode that addresses state or `areas` draw, in
    /// postcode order. A postcode with an area is placed inside it, inside
    /// the largest where several ways or relations draw it; one without is
    /// placed at the medoid of its addresses.
    pub(crate) fn into_records(self, areas: &[PostcodeArea]) -> Vec<PostcodeRecord> {
        let mut largest: BTreeMap<&str, &PostcodeArea> = BTreeMap::new();
        for area in areas {
            largest
                .entry(area.postcode.as_str())
                .and_modify(|kept| {
                    if area.area > kept.area {
                        *kept = area;
                    }
                })
                .or_insert(area);
        }
        let mut records = Vec::with_capacity(self.groups.len() + largest.len());
        for (postcode, group) in self.groups {
            records.push(match largest.remove(postcode.as_str()) {
                Some(area) => area.to_record(group.record_count),
                None => group.to_record(postcode),
            });
        }
        records.extend(largest.into_values().map(|area| area.to_record(0)));
        records.sort_by(|left, right| left.postcode.cmp(&right.postcode));
        records
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.groups.len()
    }
}

impl PostcodeGroup {
    fn offer(&mut self, point: (u64, [f64; 2])) {
        if self.sample.len() == SAMPLE_POINTS && point.0 >= self.sample[SAMPLE_POINTS - 1].0 {
            return;
        }
        let at = self.sample.partition_point(|kept| kept.0 < point.0);
        if self.sample.get(at).is_some_and(|kept| kept.0 == point.0) {
            return; // the same point again: a block of flats does not outvote a street
        }
        self.sample.insert(at, point);
        self.sample.truncate(SAMPLE_POINTS);
    }

    /// The sampled member with the least total distance to the others: always a
    /// real address, unmoved by a few stray ones, and safe across the antimeridian,
    /// where a mean of longitudes is not (issue #3).
    fn medoid(&self) -> [f64; 2] {
        let distance_sum = |[lon, lat]: [f64; 2]| -> f64 {
            self.sample
                .iter()
                .map(|(_, [other_lon, other_lat])| haversine_m(lon, lat, *other_lon, *other_lat))
                .sum()
        };
        self.sample
            .iter()
            .map(|(_, point)| (distance_sum(*point), *point))
            .min_by(|left, right| left.0.total_cmp(&right.0))
            .map(|(_, point)| point)
            .expect("a postcode group has at least one member")
    }

    fn to_record(&self, postcode: String) -> PostcodeRecord {
        let [lon, lat] = self.medoid();
        PostcodeRecord {
            postcode,
            geometry: point_geometry(lon, lat),
            source: DerivedSourceProvenance::osm_address_records(self.record_count),
            area: None,
        }
    }
}

/// A postcode's area, drawn by a `boundary=postal_code` way or relation.
#[derive(Debug, Clone)]
pub(crate) struct PostcodeArea {
    postcode: String,
    source: SourceProvenance,
    geometry: MultiPolygon<f64>,
    /// A point inside the area (see [`inside_point`]).
    point: [f64; 2],
    area: f64,
}

impl PostcodeArea {
    /// The area the tags draw, or `None` when they draw no postcode or the
    /// geometry has no inside.
    pub(crate) fn new(
        tags: &BTreeMap<String, String>,
        source: SourceProvenance,
        geometry: MultiPolygon<f64>,
    ) -> Option<Self> {
        Some(Self {
            postcode: postcode_area_code(tags)?,
            source,
            point: inside_point(&geometry)?,
            area: geometry.unsigned_area(),
            geometry,
        })
    }

    fn to_record(&self, record_count: u64) -> PostcodeRecord {
        PostcodeRecord {
            postcode: self.postcode.clone(),
            geometry: point_geometry(self.point[0], self.point[1]),
            source: DerivedSourceProvenance::osm_address_records(record_count),
            area: Some(self.source.clone()),
        }
    }
}

/// The postcode a `boundary=postal_code` way or relation draws: its
/// `postal_code` tag, or else its `ref`, which many carry instead.
pub(crate) fn postcode_area_code(tags: &BTreeMap<String, String>) -> Option<String> {
    if tags.cleaned("boundary").as_deref() != Some("postal_code") {
        return None;
    }
    tags.cleaned("postal_code")
        .or_else(|| tags.cleaned("ref"))
        .as_deref()
        .and_then(clean_postcode)
}

/// A point strictly inside an area: its centroid where that lies inside, so
/// a compact area is marked at its middle, else the middle of the widest
/// stretch of the area along the line across the middle of its bounds. The
/// centroid of an L, a crescent or a ring around a hole lies outside it.
fn inside_point(geometry: &MultiPolygon<f64>) -> Option<[f64; 2]> {
    let point = geometry
        .centroid()
        .filter(|centroid| geometry.contains(centroid))
        .or_else(|| geometry.interior_point())?;
    Some([point.x(), point.y()])
}

/// The postcode areas under the record ids of their postcodes, for finding
/// the one a point lies in.
#[derive(Debug, Default)]
pub(crate) struct PostcodeAreaIndex {
    areas: Vec<IndexedPostcodeArea>,
    tree: RTree<GeomWithData<Rectangle<[f64; 2]>, usize>>,
}

#[derive(Debug)]
struct IndexedPostcodeArea {
    record_id: RecordId,
    area: f64,
    shape: AreaShape,
}

impl PostcodeAreaIndex {
    /// Index `areas` under the record ids of their postcodes. Edge indexes of
    /// large areas are built in parallel.
    pub(crate) fn new(areas: Vec<PostcodeArea>, record_ids: &HashMap<String, RecordId>) -> Self {
        let areas = areas
            .into_par_iter()
            .filter_map(|area| {
                let bounds = area.geometry.bounding_rect()?;
                let rectangle = Rectangle::from_corners(
                    [bounds.min().x, bounds.min().y],
                    [bounds.max().x, bounds.max().y],
                );
                Some((
                    rectangle,
                    IndexedPostcodeArea {
                        record_id: *record_ids.get(&area.postcode)?,
                        area: area.area,
                        shape: AreaShape::new(area.geometry),
                    },
                ))
            })
            .collect::<Vec<_>>();
        let (rectangles, areas): (Vec<_>, Vec<_>) = areas.into_iter().unzip();
        let tree = RTree::bulk_load(
            rectangles
                .into_iter()
                .enumerate()
                .map(|(index, rectangle)| GeomWithData::new(rectangle, index))
                .collect(),
        );
        Self { areas, tree }
    }

    /// The postcode whose area covers a point, the smallest where areas
    /// overlap: a postcode drawn inside a larger one is the finer answer.
    pub(crate) fn postcode_at(&self, lon: f64, lat: f64) -> Option<RecordId> {
        if !lon.is_finite() || !lat.is_finite() {
            return None;
        }
        self.tree
            .locate_in_envelope_intersecting(&AABB::from_point([lon, lat]))
            .map(|entry| &self.areas[entry.data])
            .filter(|area| area.shape.covers(lon, lat))
            .min_by(|left, right| {
                left.area
                    .total_cmp(&right.area)
                    .then(left.record_id.cmp(&right.record_id))
            })
            .map(|area| area.record_id)
    }
}

fn hash_point(lon: f64, lat: f64) -> u64 {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&lon.to_le_bytes());
    bytes[8..].copy_from_slice(&lat.to_le_bytes());
    xxh3_64(&bytes)
}

fn clean_postcode(value: &str) -> Option<String> {
    let cleaned = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase();
    if cleaned.is_empty()
        || !cleaned
            .chars()
            .any(|character| character.is_ascii_alphanumeric())
    {
        None
    } else {
        Some(cleaned)
    }
}

#[cfg(test)]
mod tests {
    use geo::{LineString, Polygon};
    use geojson::GeometryValue;

    use crate::record::OsmObjectType;

    use super::*;

    #[test]
    fn derives_postcode_record_from_accepted_addresses_across_workers() {
        let mut first = PostcodeAccumulator::default();
        let mut second = PostcodeAccumulator::default();
        first.accept(&address("m5v 2t6"), &point_geometry(-79.4, 43.6));
        second.accept(&address("M5V   2T6"), &point_geometry(-79.2, 43.8));
        first.merge(second);

        assert_eq!(first.len(), 1);
        let record = first.into_records(&[]).into_iter().next().expect("record");
        assert_eq!(record.id(), "derived:osm:postcode:M5V%202T6");
        assert_eq!(record.label(), "M5V 2T6");
        assert_eq!(record.source.derived_from, "accepted_address_records");
        assert_eq!(record.source.record_count, 2);
        match record.geometry.value {
            GeometryValue::Point { coordinates } => {
                assert!([[-79.4, 43.6], [-79.2, 43.8]].contains(&[coordinates[0], coordinates[1]]));
            }
            other => panic!("expected Point, got {}", other.type_name()),
        }
    }

    #[test]
    fn postcode_point_is_a_member_and_ignores_a_stray_address_whatever_the_merge_order() {
        let mut points: Vec<(f64, f64)> = (0..9)
            .map(|step| (-79.39 + step as f64 * 0.001, 43.65))
            .collect();
        points.push((151.2, -33.9)); // one address mistagged with a Toronto postcode
        let point_of = |split: usize, reversed: bool| {
            let (mut first, mut second) = (
                PostcodeAccumulator::default(),
                PostcodeAccumulator::default(),
            );
            for (index, (lon, lat)) in points.iter().enumerate() {
                let target = if index < split {
                    &mut first
                } else {
                    &mut second
                };
                target.accept(&address("M5V 2T6"), &point_geometry(*lon, *lat));
            }
            if reversed {
                std::mem::swap(&mut first, &mut second);
            }
            first.merge(second);
            let record = first.into_records(&[]).into_iter().next().expect("record");
            assert_eq!(record.source.record_count, 10);
            match record.geometry.value {
                GeometryValue::Point { coordinates } => (coordinates[0], coordinates[1]),
                other => panic!("expected Point, got {}", other.type_name()),
            }
        };

        let point = point_of(3, false);
        assert!(points.contains(&point));
        assert!((point.0 - -79.386).abs() < 0.002 && point.1 == 43.65);
        assert_eq!(point_of(7, true), point);
    }

    #[test]
    fn keeps_a_bounded_sample_per_postcode() {
        let mut accumulator = PostcodeAccumulator::default();
        for step in 0..100 {
            accumulator.accept(
                &address("M5V 2T6"),
                &point_geometry(-79.0 + step as f64 * 0.001, 43.0),
            );
        }
        assert_eq!(accumulator.groups["M5V 2T6"].sample.len(), SAMPLE_POINTS);
        assert_eq!(accumulator.groups["M5V 2T6"].record_count, 100);
    }

    #[test]
    fn skips_placeholder_postcodes_without_letters_or_numbers() {
        assert_eq!(clean_postcode("---"), None);
    }

    #[test]
    fn places_an_l_shaped_area_inside_it_and_a_square_at_its_middle() {
        // The centroid of the L, (1.1, 1.1), lies in the notch outside it.
        let l_shape = polygon(&[
            (0.0, 0.0),
            (3.0, 0.0),
            (3.0, 1.0),
            (1.0, 1.0),
            (1.0, 3.0),
            (0.0, 3.0),
        ]);
        let centroid = l_shape.centroid().expect("centroid");
        assert!(!l_shape.contains(&centroid));
        let [lon, lat] = inside_point(&l_shape).expect("inside point");
        assert!(l_shape.contains(&geo::Point::new(lon, lat)), "{lon} {lat}");

        let square = polygon(&[(0.0, 0.0), (2.0, 0.0), (2.0, 2.0), (0.0, 2.0)]);
        assert_eq!(inside_point(&square), Some([1.0, 1.0]));
    }

    #[test]
    fn reads_the_postcode_of_a_postal_code_boundary_only() {
        let tags = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            postcode_area_code(&tags(&[
                ("boundary", "postal_code"),
                ("postal_code", "10115")
            ])),
            Some("10115".to_string())
        );
        assert_eq!(
            postcode_area_code(&tags(&[("boundary", "postal_code"), ("ref", "w1a 1aa")])),
            Some("W1A 1AA".to_string()),
            "a `ref` stands in for a missing `postal_code`"
        );
        assert_eq!(
            postcode_area_code(&tags(&[
                ("boundary", "administrative"),
                ("postal_code", "10115")
            ])),
            None,
            "an admin boundary that states a postcode does not draw its area"
        );
        assert_eq!(
            postcode_area_code(&tags(&[("boundary", "postal_code")])),
            None
        );
    }

    #[test]
    fn a_postcode_with_an_area_is_placed_inside_it_and_one_without_at_its_medoid() {
        let mut accumulator = PostcodeAccumulator::default();
        // Addresses that state 10115 lie off in a corner of its area, and one
        // far outside it states it too; 10117 has no area.
        for (postcode, lon, lat) in [
            ("10115", 2.9, 0.1),
            ("10115", 2.8, 0.2),
            ("10115", 12.0, 0.0),
            ("10117", 5.0, 5.0),
        ] {
            accumulator.accept(&address(postcode), &point_geometry(lon, lat));
        }
        let small = area(
            "10115",
            OsmObjectType::Way,
            1,
            &[(0.0, 0.0), (0.5, 0.0), (0.5, 0.5), (0.0, 0.5)],
        );
        let large = area(
            "10115",
            OsmObjectType::Relation,
            2,
            &[
                (0.0, 0.0),
                (3.0, 0.0),
                (3.0, 1.0),
                (1.0, 1.0),
                (1.0, 3.0),
                (0.0, 3.0),
            ],
        );
        let alone = area(
            "10119",
            OsmObjectType::Relation,
            3,
            &[(8.0, 8.0), (9.0, 8.0), (9.0, 9.0), (8.0, 9.0)],
        );
        let records = accumulator.into_records(&[small, large.clone(), alone]);

        let postcodes = records
            .iter()
            .map(|record| record.postcode.as_str())
            .collect::<Vec<_>>();
        assert_eq!(postcodes, ["10115", "10117", "10119"]);
        let point = |record: &PostcodeRecord| point_lon_lat(&record.geometry).expect("point");

        assert_eq!(
            records[0].area,
            Some(SourceProvenance::osm(OsmObjectType::Relation, 2)),
            "the largest area draws it"
        );
        assert_eq!(records[0].source.record_count, 3);
        assert_eq!(point(&records[0]), large.point);
        assert!(
            large
                .geometry
                .contains(&geo::Point::from(point(&records[0])))
        );

        assert_eq!(records[1].area, None);
        assert_eq!(point(&records[1]), [5.0, 5.0]);

        assert_eq!(
            records[2].area,
            Some(SourceProvenance::osm(OsmObjectType::Relation, 3))
        );
        assert_eq!(records[2].source.record_count, 0, "no address states it");
        assert_eq!(point(&records[2]), [8.5, 8.5]);
    }

    #[test]
    fn finds_the_smallest_postcode_area_covering_a_point() {
        let outer = area(
            "10115",
            OsmObjectType::Relation,
            1,
            &[(0.0, 0.0), (4.0, 0.0), (4.0, 4.0), (0.0, 4.0)],
        );
        let inner = area(
            "10117",
            OsmObjectType::Relation,
            2,
            &[(1.0, 1.0), (2.0, 1.0), (2.0, 2.0), (1.0, 2.0)],
        );
        let index = PostcodeAreaIndex::new(
            vec![outer, inner],
            &HashMap::from([("10115".to_string(), 7), ("10117".to_string(), 8)]),
        );
        assert_eq!(index.postcode_at(1.5, 1.5), Some(8));
        assert_eq!(index.postcode_at(3.0, 3.0), Some(7));
        assert_eq!(index.postcode_at(5.0, 5.0), None);
        assert_eq!(PostcodeAreaIndex::default().postcode_at(1.5, 1.5), None);
    }

    /// A one-ring area; the ring closes itself.
    fn polygon(ring: &[(f64, f64)]) -> MultiPolygon<f64> {
        MultiPolygon::new(vec![Polygon::new(
            LineString::from(ring.to_vec()),
            Vec::new(),
        )])
    }

    fn area(
        postcode: &str,
        object_type: OsmObjectType,
        object_id: i64,
        ring: &[(f64, f64)],
    ) -> PostcodeArea {
        let tags = BTreeMap::from([
            ("boundary".to_string(), "postal_code".to_string()),
            ("postal_code".to_string(), postcode.to_string()),
        ]);
        PostcodeArea::new(
            &tags,
            SourceProvenance::osm(object_type, object_id),
            polygon(ring),
        )
        .expect("area")
    }

    fn address(postcode: &str) -> AddressComponents {
        AddressComponents {
            areas: Vec::new(),
            number: "1".to_string(),
            street: Some("King Street".to_string()),
            place: None,
            unit: None,
            locality: None,
            region: None,
            postcode: Some(postcode.to_string()),
            country: None,
        }
    }
}
