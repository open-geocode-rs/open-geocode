use std::collections::BTreeMap;

use crate::{
    builder::report::CandidateIssue,
    record::{
        AddressComponents, AddressRecord, LocationPrecision, OsmObjectType, Record,
        SourceProvenance, point_geometry,
    },
    util::text::collapse_whitespace,
};

use super::{emitted::Emitted, tags::OsmTags};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AddressCandidate {
    pub object_type: OsmObjectType,
    pub object_id: i64,
    pub lat: f64,
    pub lon: f64,
    pub location_precision: LocationPrecision,
    pub tags: BTreeMap<String, String>,
}

/// Turn a candidate into an address record, or count why it was not one.
pub(crate) fn emit_address(candidate: AddressCandidate, out: &mut Emitted) {
    match address_record_from_candidate(&candidate) {
        Ok(record) => {
            out.report
                .accept_address_with_tags(&record, Some(&candidate.tags));
            out.postcodes.accept_address(&record);
            out.records.push(Record::Address(record));
        }
        Err(issue) => out.reject_in_report(
            issue,
            candidate.object_type,
            candidate.object_id,
            &candidate.tags,
            Some(&candidate.tags),
            Some("address"),
        ),
    }
}

pub(crate) fn address_record_from_candidate(
    candidate: &AddressCandidate,
) -> std::result::Result<AddressRecord, CandidateIssue> {
    let tags = &candidate.tags;
    let house_number = tags
        .cleaned("addr:housenumber")
        .ok_or(CandidateIssue::MissingHouseNumber)?;
    let street = tags.cleaned("addr:street");
    let place = tags.cleaned("addr:place");

    if street.is_none() && place.is_none() {
        return Err(CandidateIssue::MissingStreetOrPlace);
    }

    Ok(AddressRecord {
        address: AddressComponents {
            number: house_number,
            street,
            place,
            unit: tags.cleaned("addr:unit"),
            locality: address_locality(tags),
            region: tags.cleaned("addr:state"),
            postcode: tags.cleaned("addr:postcode"),
            country: tags.cleaned("addr:country"),
        },
        geometry: point_geometry(candidate.lon, candidate.lat),
        location_precision: candidate.location_precision,
        source: SourceProvenance::osm(candidate.object_type, candidate.object_id),
    })
}

/// The town an address names. Mappers in Australia and New Zealand, among others,
/// put it in `addr:suburb` and leave `addr:city` empty.
fn address_locality(tags: &BTreeMap<String, String>) -> Option<String> {
    [
        "addr:city",
        "addr:suburb",
        "addr:town",
        "addr:village",
        "addr:hamlet",
    ]
    .into_iter()
    .find_map(|key| tags.cleaned(key))
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
        .filter(|(key, _)| key.starts_with("addr:"))
        .map(|(key, value)| (key.clone(), value.clone()))
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

    use super::*;

    fn candidate(tags: &[(&str, &str)]) -> AddressCandidate {
        AddressCandidate {
            object_type: OsmObjectType::Node,
            object_id: 42,
            lat: 43.6532,
            lon: -79.3832,
            location_precision: LocationPrecision::Point,
            tags: tags
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        }
    }

    #[test]
    fn builds_indexing_ready_address_record() {
        let record = address_record_from_candidate(&candidate(&[
            ("addr:housenumber", "10"),
            ("addr:street", "King Street"),
            ("addr:city", "Toronto"),
            ("addr:state", "ON"),
            ("addr:country", "CA"),
        ]))
        .expect("record should be accepted");

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
        assert_eq!(record.source.tags, None);
    }

    #[test]
    fn rejects_candidates_without_house_number_or_street() {
        assert_eq!(
            address_record_from_candidate(&candidate(&[("addr:street", "King Street")])),
            Err(CandidateIssue::MissingHouseNumber)
        );
        assert_eq!(
            address_record_from_candidate(&candidate(&[("addr:housenumber", "10")])),
            Err(CandidateIssue::MissingStreetOrPlace)
        );
    }

    #[test]
    fn emits_records_postcodes_and_report_counts() {
        let mut out = Emitted::default();
        emit_address(
            candidate(&[
                ("addr:housenumber", "10"),
                ("addr:street", "King Street"),
                ("addr:postcode", "m5v 1a1"),
            ]),
            &mut out,
        );
        emit_address(candidate(&[("addr:housenumber", "10")]), &mut out);
        assert_eq!(out.records.len(), 1);
        assert_eq!(out.report.accepted.node_addresses, 1);
        assert_eq!(out.report.rejected.total, 1);
        assert!(out.rejections.is_empty(), "invalid nodes are report-only");
        assert_eq!(out.postcodes.len(), 1);
    }

    #[test]
    fn collects_only_non_empty_addr_tags() {
        let tags = collect_clean_tags(
            [
                ("addr:housenumber", " 10 "),
                ("name", "Not an address tag"),
                ("addr:street", " King   Street "),
                ("addr:unit", "   "),
            ]
            .into_iter(),
        );
        assert_eq!(
            collect_addr_tags_from_map(&tags),
            BTreeMap::from([
                ("addr:housenumber".to_string(), "10".to_string()),
                ("addr:street".to_string(), "King Street".to_string())
            ])
        );
    }

    #[test]
    fn audited_rejections_keep_source_tags() {
        let tags = BTreeMap::from([
            ("addr:street".to_string(), "King Street".to_string()),
            ("building".to_string(), "yes".to_string()),
        ]);
        let mut out = Emitted::default();
        out.reject(
            CandidateIssue::MissingHouseNumber,
            OsmObjectType::Way,
            42,
            &tags,
            Some(&tags),
            Some("address"),
        );
        let record = out.rejections.first().expect("rejection");
        assert_eq!(record.reason, "missing_housenumber");
        assert_eq!(record.layer_hint.as_deref(), Some("address"));
        assert_eq!(record.source.object_id, 42);
        assert_eq!(
            record
                .source
                .tags
                .as_ref()
                .and_then(|tags| tags.get("addr:street")),
            Some(&"King Street".to_string())
        );
        assert_eq!(out.report.rejected.total, 1);
    }
}
