use std::collections::BTreeMap;

use geojson::Geometry;
use xxhash_rust::xxh3::xxh3_64;

use crate::{
    parse::lattice::CountryCode,
    record::{AddressComponents, DerivedSourceProvenance, PostcodeRecord, Record, point_geometry},
    spatial_index::haversine_m,
    util::geo::point_lon_lat,
};

use super::spill::PendingAddress;

/// Member points kept per postcode. The kept points are the ones with the lowest
/// hashes, so the sample is the same whatever order addresses come in, and the
/// memory stays bounded by the number of postcodes, not addresses.
const SAMPLE_POINTS: usize = 16;

/// Postcode points derived from accepted addresses, one per postcode of each
/// country. Keyed by country, never by input: a planet file holds AU and NZ
/// "2110" apart, and a country split over two extracts makes one of each code.
#[derive(Debug, Clone, Default)]
pub(crate) struct Accumulator {
    groups: BTreeMap<(Option<CountryCode>, String), Group>,
}

#[derive(Debug, Clone, Default)]
struct Group {
    record_count: u64,
    /// Distinct member points by ascending hash, at most [`SAMPLE_POINTS`].
    sample: Vec<(u64, [f64; 2])>,
}

impl Accumulator {
    /// Count an accepted address in `country`.
    pub(crate) fn accept(
        &mut self,
        country: Option<CountryCode>,
        address: &AddressComponents,
        geometry: &Geometry,
    ) {
        let Some(postcode) = address.postcode.as_deref().and_then(clean_postcode) else {
            return;
        };
        let Some([lon, lat]) = point_lon_lat(geometry) else {
            return;
        };
        let group = self.groups.entry((country, postcode)).or_default();
        group.record_count += 1;
        group.offer((hash_point(lon, lat), [lon, lat]));
    }

    pub(crate) fn into_records(self) -> impl Iterator<Item = PostcodeRecord> {
        self.groups
            .into_iter()
            .map(|((country, postcode), group)| group.to_record(country, postcode))
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.groups.len()
    }
}

/// The postcodes the addresses of one street state, lent to its addresses
/// that state none. Fed in street order, where a street's addresses stating
/// a postcode come before the ones that do not.
#[derive(Debug, Default)]
pub(crate) struct Street {
    key: Vec<u8>,
    postcodes: Vec<String>,
}

impl Street {
    pub(crate) fn lend(&mut self, pending: &mut PendingAddress) {
        let key = &pending.order[..pending.street_len];
        if self.key != key {
            self.key = key.to_vec();
            self.postcodes.clear();
        }
        let Record::Address(record) = &mut pending.record else {
            unreachable!("a pending address holds an address");
        };
        // No street, no street to borrow from: the key is the cell alone.
        if record.address.subject().is_none() {
            return;
        }
        match &record.address.postcode {
            Some(postcode) => {
                if let Some(postcode) = clean_postcode(postcode)
                    && let Err(at) = self.postcodes.binary_search(&postcode)
                {
                    self.postcodes.insert(at, postcode);
                }
            }
            None => record.street_postcodes.clone_from(&self.postcodes),
        }
    }
}

impl Group {
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

    fn to_record(&self, country: Option<CountryCode>, postcode: String) -> PostcodeRecord {
        let [lon, lat] = self.medoid();
        PostcodeRecord {
            postcode,
            country,
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

    use super::*;

    #[test]
    fn derives_postcode_record_from_accepted_addresses() {
        let mut accumulator = Accumulator::default();
        accumulator.accept(None, &address("m5v 2t6"), &point_geometry(-79.4, 43.6));
        accumulator.accept(None, &address("M5V   2T6"), &point_geometry(-79.2, 43.8));

        assert_eq!(accumulator.len(), 1);
        let record = accumulator.into_records().next().expect("record");
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
    fn postcode_point_is_a_member_and_ignores_a_stray_address_whatever_the_order() {
        let mut points: Vec<(f64, f64)> = (0..9)
            .map(|step| (-79.39 + step as f64 * 0.001, 43.65))
            .collect();
        points.push((151.2, -33.9)); // one address mistagged with a Toronto postcode
        let point_of = |reversed: bool| {
            let mut accumulator = Accumulator::default();
            let mut ordered = points.clone();
            if reversed {
                ordered.reverse();
            }
            for (lon, lat) in ordered {
                accumulator.accept(None, &address("M5V 2T6"), &point_geometry(lon, lat));
            }
            let record = accumulator.into_records().next().expect("record");
            assert_eq!(record.source.record_count, 10);
            match record.geometry.value {
                GeometryValue::Point { coordinates } => (coordinates[0], coordinates[1]),
                other => panic!("expected Point, got {}", other.type_name()),
            }
        };

        let point = point_of(false);
        assert!(points.contains(&point));
        assert!((point.0 - -79.386).abs() < 0.002 && point.1 == 43.65);
        assert_eq!(point_of(true), point);
    }

    #[test]
    fn keeps_a_bounded_sample_per_postcode() {
        let mut accumulator = Accumulator::default();
        for step in 0..100 {
            accumulator.accept(
                None,
                &address("M5V 2T6"),
                &point_geometry(-79.0 + step as f64 * 0.001, 43.0),
            );
        }
        let group = &accumulator.groups[&(None, "M5V 2T6".to_string())];
        assert_eq!(group.sample.len(), SAMPLE_POINTS);
        assert_eq!(group.record_count, 100);
    }

    #[test]
    fn skips_placeholder_postcodes_without_letters_or_numbers() {
        assert_eq!(clean_postcode("---"), None);
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
