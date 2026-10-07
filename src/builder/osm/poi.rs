//! Named points of interest.
//!
//! A node or way is a POI when it has a name and one of the tags below with a
//! value that is not street furniture. A POI that also states a valid address
//! becomes one record carrying that address, not a POI plus an address record.

use std::collections::BTreeMap;

use crate::record::{
    LocationPrecision, OsmObjectType, PoiRecord, Record, SourceProvenance, point_geometry,
};

use super::{address::address_components, emitted::Emitted, tags::OsmTags};

/// Keys that make a named object a POI, in category order: an object tagged
/// `amenity=school` and `building=school` is an `amenity:school`.
const POI_KEYS: [&str; 12] = [
    "amenity",
    "shop",
    "tourism",
    "leisure",
    "office",
    "craft",
    "healthcare",
    "historic",
    "aeroway",
    "railway",
    "public_transport",
    "building",
];

/// Street furniture and fixtures that carry names but are not places anyone
/// looks up by name.
const NOISE_AMENITIES: [&str; 22] = [
    "atm",
    "bbq",
    "bench",
    "bicycle_parking",
    "clock",
    "drinking_water",
    "give_box",
    "grit_bin",
    "hunting_stand",
    "letter_box",
    "motorcycle_parking",
    "parking_entrance",
    "parking_space",
    "post_box",
    "shelter",
    "telephone",
    "toilets",
    "vending_machine",
    "waste_basket",
    "waste_disposal",
    "water_point",
    "watering_place",
];
const NOISE_LEISURE: [&str; 4] = ["bleachers", "firepit", "outdoor_seating", "picnic_table"];
const AEROWAYS: [&str; 5] = ["aerodrome", "airstrip", "helipad", "heliport", "terminal"];

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PoiCandidate {
    pub object_type: OsmObjectType,
    pub object_id: i64,
    pub lat: f64,
    pub lon: f64,
    pub location_precision: LocationPrecision,
    /// The tags [`poi_tags`] keeps.
    pub tags: BTreeMap<String, String>,
}

pub(crate) fn is_poi(tags: &BTreeMap<String, String>) -> bool {
    poi_name(tags).is_some() && poi_category(tags).is_some()
}

/// The tags a POI keeps: its name, its category tags and, when they state a
/// valid address, its `addr:*` tags.
pub(crate) fn poi_tags(
    tags: &BTreeMap<String, String>,
    address_tags: Option<&BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    ["name", "information"]
        .into_iter()
        .chain(POI_KEYS)
        .filter_map(|key| Some((key.to_string(), tags.get(key)?.clone())))
        .chain(
            address_tags
                .into_iter()
                .flatten()
                .map(|(key, value)| (key.clone(), value.clone())),
        )
        .collect()
}

/// Emit a POI from its kept tags; their `addr:*` tags are already validated.
pub(crate) fn emit_poi(candidate: PoiCandidate, out: &mut Emitted) {
    let tags = &candidate.tags;
    let (Some(name), Some(category)) = (poi_name(tags), poi_category(tags)) else {
        return;
    };
    let record = PoiRecord {
        name,
        category,
        address: address_components(tags).ok(),
        geometry: point_geometry(candidate.lon, candidate.lat),
        location_precision: candidate.location_precision,
        source: SourceProvenance::osm(candidate.object_type, candidate.object_id),
    };
    out.report.accept_poi(&record);
    if let Some(address) = &record.address {
        out.postcodes.accept(address, &record.geometry);
    }
    out.records.push(Record::Poi(record));
}

/// A name with at least one letter: house numbers and ref codes in `name` are
/// not names.
fn poi_name(tags: &BTreeMap<String, String>) -> Option<String> {
    tags.cleaned("name")
        .filter(|name| name.chars().any(char::is_alphabetic))
}

/// `key:value` of the first POI tag that is not noise.
fn poi_category(tags: &BTreeMap<String, String>) -> Option<String> {
    POI_KEYS.into_iter().find_map(|key| {
        let value = tags.cleaned(key)?;
        is_poi_value(key, &value, tags).then(|| format!("{key}:{value}"))
    })
}

fn is_poi_value(key: &str, value: &str, tags: &BTreeMap<String, String>) -> bool {
    if matches!(value, "no" | "vacant") {
        return false;
    }
    match key {
        "amenity" => !NOISE_AMENITIES.contains(&value),
        "leisure" => !NOISE_LEISURE.contains(&value),
        // Trail maps and guideposts; a staffed information office is a place.
        "tourism" => {
            value != "information" || tags.cleaned("information").as_deref() == Some("office")
        }
        "aeroway" => AEROWAYS.contains(&value),
        "railway" | "public_transport" => value == "station",
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn emit(pairs: &[(&str, &str)]) -> Emitted {
        let mut out = Emitted::default();
        emit_poi(
            PoiCandidate {
                object_type: OsmObjectType::Node,
                object_id: 7,
                lat: 43.65,
                lon: -79.38,
                location_precision: LocationPrecision::Point,
                tags: poi_tags(&tags(pairs), None),
            },
            &mut out,
        );
        out
    }

    #[test]
    fn categorizes_by_the_first_poi_key() {
        assert_eq!(
            poi_category(&tags(&[("building", "school"), ("amenity", "school")])).as_deref(),
            Some("amenity:school")
        );
        assert_eq!(
            poi_category(&tags(&[
                ("railway", "station"),
                ("public_transport", "station")
            ]))
            .as_deref(),
            Some("railway:station")
        );
        assert_eq!(
            poi_category(&tags(&[("building", "yes")])).as_deref(),
            Some("building:yes")
        );
    }

    #[test]
    fn excludes_noise_unnamed_and_unlisted_objects() {
        let named = |pairs: &[(&str, &str)]| {
            let mut tags = tags(pairs);
            tags.insert("name".to_string(), "Example".to_string());
            is_poi(&tags)
        };
        assert!(named(&[("amenity", "cafe")]));
        assert!(!named(&[("amenity", "bench")]));
        assert!(!named(&[("amenity", "waste_basket")]));
        assert!(!named(&[("leisure", "picnic_table")]));
        assert!(!named(&[("shop", "vacant")]));
        assert!(!named(&[("aeroway", "runway")]));
        assert!(!named(&[("railway", "level_crossing")]));
        assert!(named(&[("railway", "station")]));
        assert!(!named(&[
            ("tourism", "information"),
            ("information", "board")
        ]));
        assert!(named(&[
            ("tourism", "information"),
            ("information", "office")
        ]));
        assert!(!named(&[("highway", "bus_stop")]));
        // A bench next to a shop is still the shop.
        assert!(named(&[("amenity", "bench"), ("shop", "bakery")]));

        assert!(!is_poi(&tags(&[("amenity", "cafe")])), "unnamed");
        assert!(!is_poi(&tags(&[("amenity", "cafe"), ("name", "  ")])));
        assert!(!is_poi(&tags(&[("building", "yes"), ("name", "12")])));
    }

    #[test]
    fn keeps_only_poi_and_valid_address_tags() {
        let source = tags(&[
            ("name", "Tim Hortons"),
            ("amenity", "cafe"),
            ("cuisine", "coffee_shop"),
            ("addr:housenumber", "123"),
        ]);
        let address = tags(&[
            ("addr:housenumber", "123"),
            ("addr:street", "King Street West"),
        ]);
        assert_eq!(
            poi_tags(&source, None),
            tags(&[("name", "Tim Hortons"), ("amenity", "cafe")])
        );
        assert_eq!(poi_tags(&source, Some(&address)).len(), 4);
    }

    #[test]
    fn emits_one_record_carrying_its_address() {
        let mut out = Emitted::default();
        let source = tags(&[("name", " Tim  Hortons "), ("amenity", "cafe")]);
        let address = tags(&[
            ("addr:housenumber", "123"),
            ("addr:street", "King Street West"),
            ("addr:postcode", "M5V 1A1"),
        ]);
        emit_poi(
            PoiCandidate {
                object_type: OsmObjectType::Way,
                object_id: 9,
                lat: 43.65,
                lon: -79.38,
                location_precision: LocationPrecision::Centroid,
                tags: poi_tags(&source, Some(&address)),
            },
            &mut out,
        );
        let [Record::Poi(record)] = out.records.as_slice() else {
            panic!("one POI record, got {:?}", out.records);
        };
        assert_eq!(record.id(), "osm:way:9");
        assert_eq!(record.name, "Tim Hortons");
        assert_eq!(record.category, "amenity:cafe");
        assert_eq!(record.label(), "Tim Hortons, 123 King Street West, M5V 1A1");
        assert_eq!(out.report.accepted.by_layer.get("poi"), Some(&1));
        assert_eq!(out.report.accepted.by_layer.get("address"), None);
        assert_eq!(out.report.accepted.pois_with_address, 1);
        assert_eq!(out.postcodes.len(), 1, "its postcode still counts");

        let plain = emit(&[("name", "Riverdale Farm"), ("tourism", "attraction")]);
        let [Record::Poi(record)] = plain.records.as_slice() else {
            panic!("one POI record");
        };
        assert_eq!(record.address, None);
        assert_eq!(plain.postcodes.len(), 0);
    }
}
