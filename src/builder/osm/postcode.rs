use std::collections::BTreeMap;

use xxhash_rust::xxh3::xxh3_64;

use crate::{
    record::{AddressRecord, DerivedSourceProvenance, PostcodeRecord, point_geometry},
    spatial_index::haversine_m,
    util::geo::point_lon_lat,
};

/// Member points kept per postcode. The kept points are the ones with the lowest
/// hashes, so the sample is the same whatever order workers merge in, and the
/// memory stays bounded by the number of postcodes, not addresses.
const SAMPLE_POINTS: usize = 16;

/// Postcode centroids derived from accepted addresses.
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
    pub(crate) fn accept_address(&mut self, address: &AddressRecord) {
        let Some(postcode) = address.address.postcode.as_deref().and_then(clean_postcode) else {
            return;
        };
        let Some([lon, lat]) = point_lon_lat(&address.geometry) else {
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

    pub(crate) fn into_records(self) -> impl Iterator<Item = PostcodeRecord> {
        self.groups
            .into_iter()
            .map(|(postcode, group)| group.to_record(postcode))
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
        }
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
    use geojson::GeometryValue;

    use crate::record::{AddressComponents, LocationPrecision, OsmObjectType, SourceProvenance};

    use super::*;

    #[test]
    fn derives_postcode_record_from_accepted_addresses_across_workers() {
        let mut first = PostcodeAccumulator::default();
        let mut second = PostcodeAccumulator::default();
        first.accept_address(&address_record("m5v 2t6", -79.4, 43.6));
        second.accept_address(&address_record("M5V   2T6", -79.2, 43.8));
        first.merge(second);

        assert_eq!(first.len(), 1);
        let record = first.into_records().next().expect("record");
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
                target.accept_address(&address_record("M5V 2T6", *lon, *lat));
            }
            if reversed {
                std::mem::swap(&mut first, &mut second);
            }
            first.merge(second);
            let record = first.into_records().next().expect("record");
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
            accumulator.accept_address(&address_record(
                "M5V 2T6",
                -79.0 + step as f64 * 0.001,
                43.0,
            ));
        }
        assert_eq!(accumulator.groups["M5V 2T6"].sample.len(), SAMPLE_POINTS);
        assert_eq!(accumulator.groups["M5V 2T6"].record_count, 100);
    }

    #[test]
    fn skips_placeholder_postcodes_without_letters_or_numbers() {
        assert_eq!(clean_postcode("---"), None);
    }

    fn address_record(postcode: &str, lon: f64, lat: f64) -> AddressRecord {
        AddressRecord {
            address: AddressComponents {
                number: "1".to_string(),
                street: Some("King Street".to_string()),
                place: None,
                unit: None,
                locality: None,
                region: None,
                postcode: Some(postcode.to_string()),
                country: None,
            },
            geometry: point_geometry(lon, lat),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(OsmObjectType::Node, 1),
        }
    }
}
