use std::collections::BTreeMap;

use anyhow::Result;

use crate::{
    builder::report::{BuilderReport, CandidateIssue},
    pack::RecordWriter,
    record::{
        AddressComponents, AddressRecord, LocationPrecision, OsmObjectType, RejectedRecord,
        SourceProvenance, point_geometry,
    },
};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AddressCandidate {
    pub object_type: OsmObjectType,
    pub object_id: i64,
    pub lat: f64,
    pub lon: f64,
    pub location_precision: LocationPrecision,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy)]
struct LabelParts<'a> {
    house_number: &'a str,
    street: Option<&'a str>,
    place: Option<&'a str>,
    unit: Option<&'a str>,
    city: Option<&'a str>,
    state: Option<&'a str>,
    postcode: Option<&'a str>,
    country: Option<&'a str>,
}

pub(crate) fn write_candidate(
    candidate: AddressCandidate,
    writer: &mut dyn RecordWriter,
    report: &mut BuilderReport,
) -> Result<Option<AddressRecord>> {
    match address_record_from_candidate(candidate) {
        Ok(record) => {
            writer.write_address(&record)?;
            report.accept_address(&record);
            Ok(Some(record))
        }
        Err(issue) => {
            report.reject(issue);
            Ok(None)
        }
    }
}

pub(crate) fn write_rejected_record(
    issue: CandidateIssue,
    object_type: OsmObjectType,
    object_id: i64,
    tags: &BTreeMap<String, String>,
    layer_hint: Option<&str>,
    writer: &mut dyn RecordWriter,
) -> Result<()> {
    let record = RejectedRecord {
        reason: issue.as_str().to_string(),
        layer_hint: layer_hint.map(str::to_string),
        source: SourceProvenance::osm_with_tags(object_type, object_id, tags.clone()),
    };
    writer.write_rejection(record)
}

pub(crate) fn address_record_from_candidate(
    candidate: AddressCandidate,
) -> std::result::Result<AddressRecord, CandidateIssue> {
    let house_number =
        tag_value(&candidate.tags, "addr:housenumber").ok_or(CandidateIssue::MissingHouseNumber)?;
    let street = tag_value(&candidate.tags, "addr:street");
    let place = tag_value(&candidate.tags, "addr:place");

    if street.is_none() && place.is_none() {
        return Err(CandidateIssue::MissingStreetOrPlace);
    }

    let unit = tag_value(&candidate.tags, "addr:unit");
    let city = tag_value(&candidate.tags, "addr:city");
    let postcode = tag_value(&candidate.tags, "addr:postcode");
    let state = tag_value(&candidate.tags, "addr:state");
    let country = tag_value(&candidate.tags, "addr:country");
    let name = [
        Some(house_number.as_str()),
        street.as_deref().or(place.as_deref()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    let label = build_label(LabelParts {
        house_number: &house_number,
        street: street.as_deref(),
        place: place.as_deref(),
        unit: unit.as_deref(),
        city: city.as_deref(),
        state: state.as_deref(),
        postcode: postcode.as_deref(),
        country: country.as_deref(),
    });

    Ok(AddressRecord {
        id: format!(
            "osm:{}:{}",
            osm_object_type_name(candidate.object_type),
            candidate.object_id
        ),
        label,
        name,
        address: AddressComponents {
            number: house_number,
            street,
            place,
            unit,
            locality: city,
            region: state,
            postcode,
            country,
        },
        geometry: point_geometry(candidate.lon, candidate.lat),
        location_precision: candidate.location_precision,
        source: SourceProvenance::osm(candidate.object_type, candidate.object_id),
    })
}

pub(crate) fn collect_clean_tags<'a>(
    tags: impl Iterator<Item = (&'a str, &'a str)>,
) -> BTreeMap<String, String> {
    tags.filter_map(|(key, value)| {
        let value = clean_text(value)?;
        Some((key.to_string(), value))
    })
    .collect()
}

pub(crate) fn collect_addr_tags_from_map(
    tags: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    tags.iter()
        .filter_map(|(key, value)| {
            if !key.starts_with("addr:") {
                return None;
            }
            Some((key.clone(), value.clone()))
        })
        .collect()
}

pub(crate) fn validate_address_tags(
    tags: &BTreeMap<String, String>,
) -> std::result::Result<(), CandidateIssue> {
    if tag_value(tags, "addr:housenumber").is_none() {
        return Err(CandidateIssue::MissingHouseNumber);
    }
    if tag_value(tags, "addr:street").is_none() && tag_value(tags, "addr:place").is_none() {
        return Err(CandidateIssue::MissingStreetOrPlace);
    }
    Ok(())
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

fn build_label(parts: LabelParts<'_>) -> String {
    let primary = [Some(parts.house_number), parts.street.or(parts.place)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");

    [
        Some(primary.as_str()),
        parts.unit,
        parts.city,
        parts.state,
        parts.postcode,
        parts.country,
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(", ")
}

fn osm_object_type_name(object_type: OsmObjectType) -> &'static str {
    match object_type {
        OsmObjectType::Node => "node",
        OsmObjectType::Way => "way",
        OsmObjectType::Relation => "relation",
    }
}

#[cfg(test)]
mod tests {
    use geojson::GeometryValue;

    use crate::pack::test_support::MemoryRecordWriter;

    use super::*;

    #[test]
    fn builds_indexing_ready_address_record() {
        let candidate = AddressCandidate {
            object_type: OsmObjectType::Node,
            object_id: 42,
            lat: 43.6532,
            lon: -79.3832,
            location_precision: LocationPrecision::Point,
            tags: BTreeMap::from([
                ("addr:housenumber".to_string(), "10".to_string()),
                ("addr:street".to_string(), "King Street".to_string()),
                ("addr:city".to_string(), "Toronto".to_string()),
                ("addr:state".to_string(), "ON".to_string()),
                ("addr:country".to_string(), "CA".to_string()),
            ]),
        };

        let record = address_record_from_candidate(candidate).expect("record should be accepted");

        assert_eq!(record.id, "osm:node:42");
        assert_eq!(record.label, "10 King Street, Toronto, ON, CA");
        assert_eq!(record.name, "10 King Street");
        assert_eq!(record.address.street.as_deref(), Some("King Street"));
        assert_eq!(record.address.locality.as_deref(), Some("Toronto"));
        assert_eq!(record.address.region.as_deref(), Some("ON"));
        match &record.geometry.value {
            GeometryValue::Point { coordinates } => {
                assert_eq!(coordinates.as_slice(), &[-79.3832, 43.6532]);
            }
            other => panic!("expected Point, got {}", other.type_name()),
        }
        assert_eq!(record.source.object_type, OsmObjectType::Node);
        assert_eq!(record.source.object_id, 42);
        assert_eq!(record.source.tags, None);
    }

    #[test]
    fn rejects_candidates_without_house_number() {
        let candidate = AddressCandidate {
            object_type: OsmObjectType::Node,
            object_id: 42,
            lat: 0.0,
            lon: 0.0,
            location_precision: LocationPrecision::Point,
            tags: BTreeMap::from([("addr:street".to_string(), "King Street".to_string())]),
        };

        assert_eq!(
            address_record_from_candidate(candidate),
            Err(CandidateIssue::MissingHouseNumber)
        );
    }

    #[test]
    fn rejects_candidates_without_street_or_place() {
        let candidate = AddressCandidate {
            object_type: OsmObjectType::Node,
            object_id: 42,
            lat: 0.0,
            lon: 0.0,
            location_precision: LocationPrecision::Point,
            tags: BTreeMap::from([("addr:housenumber".to_string(), "10".to_string())]),
        };

        assert_eq!(
            address_record_from_candidate(candidate),
            Err(CandidateIssue::MissingStreetOrPlace)
        );
    }

    #[test]
    fn collects_only_non_empty_addr_tags() {
        let tags = [
            ("addr:housenumber", " 10 "),
            ("name", "Not an address tag"),
            ("addr:street", " King   Street "),
            ("addr:unit", "   "),
        ];

        let tags = collect_clean_tags(tags.into_iter());
        let collected = collect_addr_tags_from_map(&tags);

        assert_eq!(
            collected,
            BTreeMap::from([
                ("addr:housenumber".to_string(), "10".to_string()),
                ("addr:street".to_string(), "King Street".to_string())
            ])
        );
    }

    #[test]
    fn writes_rejected_record_with_source_tags() {
        let tags = BTreeMap::from([
            ("addr:street".to_string(), "King Street".to_string()),
            ("building".to_string(), "yes".to_string()),
        ]);
        let mut writer = MemoryRecordWriter::default();

        write_rejected_record(
            CandidateIssue::MissingHouseNumber,
            OsmObjectType::Way,
            42,
            &tags,
            Some("address"),
            &mut writer,
        )
        .expect("write rejected record");

        let record = writer.rejections.first().expect("rejection");
        assert_eq!(record.reason, "missing_housenumber");
        assert_eq!(record.layer_hint.as_deref(), Some("address"));
        assert_eq!(record.source.object_type, OsmObjectType::Way);
        assert_eq!(record.source.object_id, 42);
        assert_eq!(
            record
                .source
                .tags
                .as_ref()
                .and_then(|tags| tags.get("addr:street")),
            Some(&"King Street".to_string())
        );
    }
}
