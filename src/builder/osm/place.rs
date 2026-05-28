use std::collections::BTreeMap;

use anyhow::Result;

use crate::{
    builder::report::{BuilderReport, CandidateIssue},
    pack::RecordWriter,
    record::{OsmObjectType, PlaceLayer, PlaceRecord, SourceProvenance, point_geometry},
};

pub(crate) fn has_place_tag(tags: &BTreeMap<String, String>) -> bool {
    tag_value(tags, "place").is_some()
}

pub(crate) fn write_place_node(
    object_id: i64,
    lat: f64,
    lon: f64,
    tags: &BTreeMap<String, String>,
    writer: &mut dyn RecordWriter,
    report: &mut BuilderReport,
) -> Result<()> {
    match place_record_from_node(object_id, lat, lon, tags) {
        Ok((record, layer)) => {
            writer.write_place(&record, layer)?;
            report.accept_place(layer);
        }
        Err(issue) => report.reject_with_context(
            issue,
            OsmObjectType::Node,
            object_id,
            tags,
            None,
            Some("place"),
            false,
        ),
    }
    Ok(())
}

fn place_record_from_node(
    object_id: i64,
    lat: f64,
    lon: f64,
    tags: &BTreeMap<String, String>,
) -> std::result::Result<(PlaceRecord, PlaceLayer), CandidateIssue> {
    let place_type = tag_value(tags, "place").ok_or(CandidateIssue::PlaceUnsupportedValue)?;
    let layer = place_layer(&place_type).ok_or(CandidateIssue::PlaceUnsupportedValue)?;
    let name = tag_value(tags, "name").ok_or(CandidateIssue::PlaceMissingName)?;

    Ok((
        PlaceRecord {
            id: format!("osm:node:{object_id}"),
            label: name.clone(),
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

fn tag_value(tags: &BTreeMap<String, String>, key: &str) -> Option<String> {
    tags.get(key).and_then(|value| clean_text(value))
}

fn clean_text(value: &str) -> Option<String> {
    let cleaned = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

fn normalize_for_compare(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
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
        assert_eq!(record.id, "osm:node:42");
        assert_eq!(record.label, "Toronto");
        assert_eq!(record.name, "Toronto");
        assert_eq!(record.place_type, "city");
        assert_eq!(record.source.object_type, OsmObjectType::Node);
        assert_eq!(record.source.object_id, 42);
        match record.geometry.value {
            GeometryValue::Point { coordinates } => {
                assert_eq!(coordinates.as_slice(), &[-79.3832, 43.6532]);
            }
            other => panic!("expected Point, got {}", other.type_name()),
        }
    }

    #[test]
    fn rejects_place_node_without_clean_name() {
        let tags = BTreeMap::from([("place".to_string(), "city".to_string())]);

        assert_eq!(
            place_record_from_node(42, 0.0, 0.0, &tags),
            Err(CandidateIssue::PlaceMissingName)
        );
    }

    #[test]
    fn rejects_unsupported_place_values() {
        let tags = BTreeMap::from([
            ("place".to_string(), "sea".to_string()),
            ("name".to_string(), "Example Sea".to_string()),
        ]);

        assert_eq!(
            place_record_from_node(42, 0.0, 0.0, &tags),
            Err(CandidateIssue::PlaceUnsupportedValue)
        );
    }
}
