use std::collections::BTreeMap;

use crate::{
    builder::report::CandidateIssue,
    record::{
        AddressComponents, AddressRecord, LocationPrecision, ObjectType, Record, SourceProvenance,
        point_geometry,
    },
    util::text::collapse_whitespace,
};

use super::{emitted::Emitted, tags::OsmTags};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AddressCandidate {
    pub object_type: ObjectType,
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
            out.postcodes.accept(&record.address, &record.geometry);
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
    Ok(AddressRecord {
        address: address_components(&candidate.tags)?,
        geometry: point_geometry(candidate.lon, candidate.lat),
        location_precision: candidate.location_precision,
        source: SourceProvenance::osm(candidate.object_type, candidate.object_id),
    })
}

/// The keys that name the settlement an address is in, the first one stated
/// winning. A rural address often names the small place it is in instead of
/// a city: 237k of New Zealand's addresses state only an `addr:hamlet`.
pub(crate) const LOCALITY_KEYS: [&str; 4] =
    ["addr:city", "addr:town", "addr:village", "addr:hamlet"];
/// The areas below the locality an address may state, outermost first.
const AREA_KEYS: [&str; 3] = ["addr:suburb", "addr:quarter", "addr:neighbourhood"];
/// The areas that place a house number when no street does: the Japanese
/// block scheme numbers blocks within a quarter or neighbourhood
/// (`addr:quarter=八乙女中央`, `addr:neighbourhood=5丁目`,
/// `addr:block_number=10`, `addr:housenumber=8`).
const BLOCK_AREA_KEYS: [&str; 2] = ["addr:quarter", "addr:neighbourhood"];

/// The address `addr:*` tags state, if they name a house number and a street
/// or place, or a number of the block scheme.
///
/// A block scheme address has an `addr:block_number`, or a house number in
/// an `addr:quarter` or `addr:neighbourhood`, and no street. Its number is
/// the block and house numbers joined as they are written, `10-8`, or the
/// one of them it has, and its areas say where the block is.
pub(crate) fn address_components(
    tags: &BTreeMap<String, String>,
) -> std::result::Result<AddressComponents, CandidateIssue> {
    let house_number = tags.cleaned("addr:housenumber");
    let block_number = tags.cleaned("addr:block_number");
    let street = tags.cleaned("addr:street");
    let place = tags.cleaned("addr:place");
    let block_scheme =
        block_number.is_some() || BLOCK_AREA_KEYS.into_iter().any(|key| tags.has(key));
    let number = match (block_number, house_number) {
        (Some(block), Some(house)) => format!("{block}-{house}"),
        (Some(block), None) => block,
        (None, Some(house)) => house,
        (None, None) => return Err(CandidateIssue::MissingHouseNumber),
    };
    if street.is_none() && place.is_none() && !block_scheme {
        return Err(CandidateIssue::MissingStreetOrPlace);
    }
    let mut areas = Vec::new();
    for area in AREA_KEYS.into_iter().filter_map(|key| tags.cleaned(key)) {
        if !areas.contains(&area) {
            areas.push(area);
        }
    }

    Ok(AddressComponents {
        number,
        street,
        place,
        unit: tags.cleaned("addr:unit"),
        locality: LOCALITY_KEYS.into_iter().find_map(|key| tags.cleaned(key)),
        areas,
        region: tags
            .cleaned("addr:state")
            .or_else(|| tags.cleaned("addr:province")),
        postcode: tags.cleaned("addr:postcode"),
        country: tags.cleaned("addr:country"),
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
        .filter(|(key, _)| key.starts_with("addr:"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub(crate) fn validate_address_tags(
    tags: &BTreeMap<String, String>,
) -> std::result::Result<(), CandidateIssue> {
    address_components(tags).map(|_| ())
}

#[cfg(test)]
mod tests {
    use geojson::GeometryValue;

    use super::*;

    fn candidate(tags: &[(&str, &str)]) -> AddressCandidate {
        AddressCandidate {
            object_type: ObjectType::Node,
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
        assert_eq!(record.source.object_type, ObjectType::Node);
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
    fn reads_the_locality_from_the_settlement_the_address_names() {
        let locality = |tags: &[(&str, &str)]| {
            let mut all = vec![("addr:housenumber", "12"), ("addr:street", "Ruru Road")];
            all.extend_from_slice(tags);
            address_record_from_candidate(&candidate(&all))
                .expect("address")
                .address
                .locality
        };
        assert_eq!(
            locality(&[("addr:hamlet", "Waimauku")]).as_deref(),
            Some("Waimauku")
        );
        assert_eq!(
            locality(&[("addr:city", "Auckland"), ("addr:hamlet", "Waimauku")]).as_deref(),
            Some("Auckland")
        );
        // A suburb is an area of the locality, not the locality.
        assert_eq!(locality(&[("addr:suburb", "Grey Lynn")]), None);
    }

    #[test]
    fn accepts_block_scheme_addresses_without_a_street() {
        let record = address_record_from_candidate(&candidate(&[
            ("addr:province", "宮城県"),
            ("addr:city", "仙台市"),
            ("addr:suburb", "泉区"),
            ("addr:quarter", "八乙女中央"),
            ("addr:neighbourhood", "5丁目"),
            ("addr:block_number", "10"),
            ("addr:housenumber", "8"),
        ]))
        .expect("block address");
        let address = &record.address;
        assert_eq!(address.number, "10-8");
        assert_eq!(address.street, None);
        assert_eq!(address.areas, vec!["泉区", "八乙女中央", "5丁目"]);
        assert_eq!(address.locality.as_deref(), Some("仙台市"));
        assert_eq!(address.region.as_deref(), Some("宮城県"));
        assert_eq!(record.name(), "10-8 5丁目");

        // A block with no house number is numbered by the block.
        let block = address_record_from_candidate(&candidate(&[
            ("addr:neighbourhood", "遠藤"),
            ("addr:block_number", "4262"),
        ]))
        .expect("block number only");
        assert_eq!(block.address.number, "4262");
        // A house number in a quarter.
        let quarter = address_record_from_candidate(&candidate(&[
            ("addr:quarter", "中荻野"),
            ("addr:housenumber", "1"),
        ]))
        .expect("quarter");
        assert_eq!(quarter.address.number, "1");
        // A suburb alone places no house number.
        assert_eq!(
            address_record_from_candidate(&candidate(&[
                ("addr:suburb", "泉区"),
                ("addr:housenumber", "1"),
            ])),
            Err(CandidateIssue::MissingStreetOrPlace)
        );
        assert_eq!(
            address_record_from_candidate(&candidate(&[("addr:quarter", "中荻野")])),
            Err(CandidateIssue::MissingHouseNumber)
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
            ObjectType::Way,
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
