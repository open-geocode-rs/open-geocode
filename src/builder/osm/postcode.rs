use std::collections::BTreeMap;

use geojson::Geometry;

use crate::{
    record::{AddressComponents, DerivedSourceProvenance, PostcodeRecord, point_geometry},
    util::geo::point_lon_lat,
};

/// Postcode centroids derived from accepted addresses.
#[derive(Debug, Clone, Default)]
pub(crate) struct PostcodeAccumulator {
    groups: BTreeMap<String, PostcodeGroup>,
}

#[derive(Debug, Clone, Default)]
struct PostcodeGroup {
    lon_sum: f64,
    lat_sum: f64,
    record_count: u64,
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
        group.lon_sum += lon;
        group.lat_sum += lat;
        group.record_count += 1;
    }

    /// Fold in a worker's accumulator. Workers are merged in input order.
    pub(crate) fn merge(&mut self, other: PostcodeAccumulator) {
        for (postcode, other) in other.groups {
            let group = self.groups.entry(postcode).or_default();
            group.lon_sum += other.lon_sum;
            group.lat_sum += other.lat_sum;
            group.record_count += other.record_count;
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
    fn to_record(&self, postcode: String) -> PostcodeRecord {
        let lon = self.lon_sum / self.record_count as f64;
        let lat = self.lat_sum / self.record_count as f64;
        PostcodeRecord {
            postcode,
            geometry: point_geometry(lon, lat),
            source: DerivedSourceProvenance::osm_address_records(self.record_count),
        }
    }
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
    fn derives_postcode_record_from_accepted_addresses_across_workers() {
        let mut first = PostcodeAccumulator::default();
        let mut second = PostcodeAccumulator::default();
        first.accept(&address("m5v 2t6"), &point_geometry(-79.4, 43.6));
        second.accept(&address("M5V   2T6"), &point_geometry(-79.2, 43.8));
        first.merge(second);

        assert_eq!(first.len(), 1);
        let record = first.into_records().next().expect("record");
        assert_eq!(record.id(), "derived:osm:postcode:M5V%202T6");
        assert_eq!(record.label(), "M5V 2T6");
        assert_eq!(record.source.derived_from, "accepted_address_records");
        assert_eq!(record.source.record_count, 2);
        match record.geometry.value {
            GeometryValue::Point { coordinates } => {
                assert!((coordinates[0] - -79.3).abs() < 0.000001);
                assert!((coordinates[1] - 43.7).abs() < 0.000001);
            }
            other => panic!("expected Point, got {}", other.type_name()),
        }
    }

    #[test]
    fn skips_placeholder_postcodes_without_letters_or_numbers() {
        assert_eq!(clean_postcode("---"), None);
    }

    fn address(postcode: &str) -> AddressComponents {
        AddressComponents {
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
