use std::{collections::BTreeMap, io::Write};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::report::{CandidateIssue, ImportReport};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AddressRecord {
    pub schema_version: u32,
    pub record_id: String,
    pub kind: RecordKind,
    pub label: String,
    pub house_number: String,
    pub street: Option<String>,
    pub place: Option<String>,
    pub unit: Option<String>,
    pub city: Option<String>,
    pub postcode: Option<String>,
    pub state: Option<String>,
    pub country: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub location_precision: LocationPrecision,
    pub source: SourceProvenance,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    Address,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocationPrecision {
    Point,
    Centroid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceProvenance {
    pub dataset: String,
    pub object_type: OsmObjectType,
    pub object_id: i64,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OsmObjectType {
    Node,
    Way,
    Relation,
}

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

pub(crate) fn write_candidate<W: Write>(
    candidate: AddressCandidate,
    output: &mut W,
    report: &mut ImportReport,
) -> Result<()> {
    match address_record_from_candidate(candidate) {
        Ok(record) => {
            serde_json::to_writer(&mut *output, &record)?;
            writeln!(output)?;
            report.accept(&record);
        }
        Err(issue) => report.reject(issue),
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_record<W: Write>(
    record: &AddressRecord,
    output: &mut W,
) -> std::io::Result<()> {
    serde_json::to_writer(&mut *output, record).map_err(std::io::Error::other)?;
    writeln!(output)
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
        schema_version: 1,
        record_id: format!(
            "osm:{}:{}",
            osm_object_type_name(candidate.object_type),
            candidate.object_id
        ),
        kind: RecordKind::Address,
        label,
        house_number,
        street,
        place,
        unit,
        city,
        postcode,
        state,
        country,
        lat: candidate.lat,
        lon: candidate.lon,
        location_precision: candidate.location_precision,
        source: SourceProvenance {
            dataset: "osm".to_string(),
            object_type: candidate.object_type,
            object_id: candidate.object_id,
            tags: candidate.tags,
        },
    })
}

pub(crate) fn collect_addr_tags<'a>(
    tags: impl Iterator<Item = (&'a str, &'a str)>,
) -> BTreeMap<String, String> {
    tags.filter_map(|(key, value)| {
        if !key.starts_with("addr:") {
            return None;
        }
        let value = clean_text(value)?;
        Some((key.to_string(), value))
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

        assert_eq!(record.record_id, "osm:node:42");
        assert_eq!(record.kind, RecordKind::Address);
        assert_eq!(record.label, "10 King Street, Toronto, ON, CA");
        assert_eq!(record.street.as_deref(), Some("King Street"));
        assert_eq!(record.source.object_type, OsmObjectType::Node);
        assert_eq!(record.source.object_id, 42);
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

        let collected = collect_addr_tags(tags.into_iter());

        assert_eq!(
            collected,
            BTreeMap::from([
                ("addr:housenumber".to_string(), "10".to_string()),
                ("addr:street".to_string(), "King Street".to_string())
            ])
        );
    }
}
