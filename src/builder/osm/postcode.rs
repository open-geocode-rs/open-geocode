use std::collections::BTreeMap;

use anyhow::Result;
use geojson::GeometryValue;

use crate::{
    builder::report::BuilderReport,
    pack::RecordWriter,
    record::{AddressRecord, DerivedSourceProvenance, PostcodeRecord, point_geometry},
};

#[derive(Debug, Clone, Default)]
pub(crate) struct PostcodeAccumulator {
    groups: BTreeMap<String, PostcodeGroup>,
}

#[derive(Debug, Clone)]
struct PostcodeGroup {
    postcode: String,
    lon_sum: f64,
    lat_sum: f64,
    record_count: u64,
}

impl PostcodeAccumulator {
    pub(crate) fn accept_address(&mut self, address: &AddressRecord) {
        let Some(postcode) = address.address.postcode.as_deref().and_then(clean_postcode) else {
            return;
        };
        let Some((lon, lat)) = point_coordinates(address) else {
            return;
        };

        let group = self
            .groups
            .entry(postcode.clone())
            .or_insert(PostcodeGroup {
                postcode,
                lon_sum: 0.0,
                lat_sum: 0.0,
                record_count: 0,
            });
        group.lon_sum += lon;
        group.lat_sum += lat;
        group.record_count += 1;
    }

    pub(crate) fn write_records(
        &self,
        writer: &mut dyn RecordWriter,
        report: &mut BuilderReport,
    ) -> Result<()> {
        for group in self.groups.values() {
            let record = group.to_record();
            writer.write_postcode(&record)?;
            report.accept_postcode();
        }
        Ok(())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.groups.len()
    }
}

impl PostcodeGroup {
    fn to_record(&self) -> PostcodeRecord {
        let lon = self.lon_sum / self.record_count as f64;
        let lat = self.lat_sum / self.record_count as f64;
        PostcodeRecord {
            postcode: self.postcode.clone(),
            geometry: point_geometry(lon, lat),
            source: DerivedSourceProvenance::osm_address_records(self.record_count),
        }
    }
}

fn point_coordinates(address: &AddressRecord) -> Option<(f64, f64)> {
    match &address.geometry.value {
        GeometryValue::Point { coordinates } if coordinates.len() == 2 => {
            Some((coordinates[0], coordinates[1]))
        }
        _ => None,
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
    use crate::record::{
        AddressComponents, LocationPrecision, OsmObjectType, SourceProvenance, point_geometry,
    };

    use super::*;

    #[test]
    fn derives_postcode_record_from_accepted_addresses() {
        let first = address_record("a", "m5v 2t6", -79.4, 43.6);
        let second = address_record("b", "M5V   2T6", -79.2, 43.8);
        let mut accumulator = PostcodeAccumulator::default();

        accumulator.accept_address(&first);
        accumulator.accept_address(&second);

        assert_eq!(accumulator.len(), 1);
        let group = accumulator.groups.get("M5V 2T6").expect("group");
        let record = group.to_record();

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

    fn address_record(_id: &str, postcode: &str, lon: f64, lat: f64) -> AddressRecord {
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
