use std::collections::BTreeMap;

use anyhow::Result;

use crate::{
    builder::report::{BuilderReport, CandidateIssue},
    pack::RecordWriter,
    record::{
        AddressComponents, AddressRecord, LocationPrecision, OsmObjectType, RejectedRecord,
        SourceProvenance, point_geometry,
    },
    util::text::collapse_whitespace,
};

use super::tags::OsmTags;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AddressCandidate {
    pub object_type: OsmObjectType,
    pub object_id: i64,
    pub lat: f64,
    pub lon: f64,
    pub location_precision: LocationPrecision,
    pub tags: BTreeMap<String, String>,
}

pub(crate) fn write_candidate(
    candidate: AddressCandidate,
    writer: &mut dyn RecordWriter,
    report: &mut BuilderReport,
) -> Result<Option<AddressRecord>> {
    let report_candidate = candidate.clone();
    match address_record_from_candidate(candidate) {
        Ok(record) => {
            writer.write_address(&record)?;
            report.accept_address_with_tags(&record, Some(&report_candidate.tags));
            Ok(Some(record))
        }
        Err(issue) => {
            report.reject_with_context(
                issue,
                report_candidate.object_type,
                report_candidate.object_id,
                &report_candidate.tags,
                Some(&report_candidate.tags),
                Some("address"),
                false,
            );
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
        candidate.tags.cleaned("addr:housenumber").ok_or(CandidateIssue::MissingHouseNumber)?;
    let street = candidate.tags.cleaned("addr:street");
    let place = candidate.tags.cleaned("addr:place");

    if street.is_none() && place.is_none() {
        return Err(CandidateIssue::MissingStreetOrPlace);
    }

    let unit = candidate.tags.cleaned("addr:unit");
    let city = candidate.tags.cleaned("addr:city");
    let postcode = candidate.tags.cleaned("addr:postcode");
    let state = candidate.tags.cleaned("addr:state");
    let country = candidate.tags.cleaned("addr:country");

    Ok(AddressRecord {
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
        let value = collapse_whitespace(value)?;
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
    if !tags.has("addr:housenumber") {
        return Err(CandidateIssue::MissingHouseNumber);
    }
    if !tags.has("addr:street") && !tags.has("addr:place") {
        return Err(CandidateIssue::MissingStreetOrPlace);
    }
    Ok(())
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

        assert_eq!(record.id(), "osm:node:42");
        assert_eq!(record.label(), "10 King Street, Toronto, ON, CA");
        assert_eq!(record.name(), "10 King Street");
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
