use std::collections::BTreeMap;

use crate::{
    builder::report::CandidateIssue,
    record::{OsmObjectType, PlaceLayer, PlaceRecord, Record, SourceProvenance, point_geometry},
    util::text::normalize_for_compare,
};

use super::{emitted::Emitted, tags::OsmTags};

pub(crate) fn has_place_tag(tags: &BTreeMap<String, String>) -> bool {
    tags.has("place")
}

pub(crate) fn emit_place_node(
    object_id: i64,
    lat: f64,
    lon: f64,
    tags: &BTreeMap<String, String>,
    out: &mut Emitted,
) {
    match place_record_from_node(object_id, lat, lon, tags) {
        Ok((record, layer)) => {
            out.report.accept_place(layer);
            out.records.push(Record::Place(layer, record));
        }
        Err(issue) => out.reject_in_report(
            issue,
            OsmObjectType::Node,
            object_id,
            tags,
            None,
            Some("place"),
        ),
    }
}

fn place_record_from_node(
    object_id: i64,
    lat: f64,
    lon: f64,
    tags: &BTreeMap<String, String>,
) -> std::result::Result<(PlaceRecord, PlaceLayer), CandidateIssue> {
    let place_type = tags
        .cleaned("place")
        .ok_or(CandidateIssue::PlaceUnsupportedValue)?;
    let layer = place_layer(&place_type).ok_or(CandidateIssue::PlaceUnsupportedValue)?;
    let name = tags
        .cleaned("name")
        .ok_or(CandidateIssue::PlaceMissingName)?;

    Ok((
        PlaceRecord {
            name,
            place_type,
            geometry: point_geometry(lon, lat),
            source: SourceProvenance::osm(OsmObjectType::Node, object_id),
        },
        layer,
    ))
}

fn place_layer(place_type: &str) -> Option<PlaceLayer> {
    match normalize_for_compare(place_type).as_str() {
        "country" => Some(PlaceLayer::Country),
        "state" | "province" | "region" => Some(PlaceLayer::Region),
        "county" | "district" | "municipality" => Some(PlaceLayer::District),
        "city" | "town" | "village" | "hamlet" | "locality" => Some(PlaceLayer::Locality),
        "suburb" | "neighbourhood" | "quarter" | "borough" => Some(PlaceLayer::Neighbourhood),
        "island" | "islet" | "farm" | "isolated_dwelling" => Some(PlaceLayer::Place),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use geojson::GeometryValue;

    use super::*;

    #[test]
    fn builds_locality_record_from_city_node() {
        let tags = BTreeMap::from([
            ("place".to_string(), "city".to_string()),
            ("name".to_string(), " Toronto ".to_string()),
        ]);

        let (record, layer) = place_record_from_node(42, 43.6532, -79.3832, &tags).expect("place");

        assert_eq!(layer, PlaceLayer::Locality);
        assert_eq!(record.id(), "osm:node:42");
        assert_eq!(record.label(), "Toronto");
        assert_eq!(record.place_type, "city");
        match record.geometry.value {
            GeometryValue::Point { coordinates } => {
                assert_eq!(coordinates.as_slice(), &[-79.3832, 43.6532]);
            }
            other => panic!("expected Point, got {}", other.type_name()),
        }
    }

    #[test]
    fn rejects_unnamed_or_unsupported_places() {
        let unnamed = BTreeMap::from([("place".to_string(), "city".to_string())]);
        assert_eq!(
            place_record_from_node(42, 0.0, 0.0, &unnamed),
            Err(CandidateIssue::PlaceMissingName)
        );
        let sea = BTreeMap::from([
            ("place".to_string(), "sea".to_string()),
            ("name".to_string(), "Example Sea".to_string()),
        ]);
        let mut out = Emitted::default();
        emit_place_node(42, 0.0, 0.0, &sea, &mut out);
        assert!(out.records.is_empty());
        assert_eq!(
            out.report.rejected.by_reason.get("place_unsupported_value"),
            Some(&1)
        );
    }
}
