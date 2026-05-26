use std::collections::{BTreeMap, HashMap};

use anyhow::Result;

use crate::{
    builder::report::{BuilderReport, CandidateIssue},
    pack::RecordWriter,
    record::{
        InterpolationAddressComponents, InterpolationRange, InterpolationRecord, NormalizedRecord,
        OsmObjectType, SourceProvenance,
    },
};

use super::{
    address::{collect_addr_tags_from_map, write_rejected_record},
    geometry::{line_string_geometry, resolve_node_ref_points},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterpolationWayStub {
    pub object_id: i64,
    pub node_refs: Vec<i64>,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InterpolationRule {
    kind: String,
    step: u32,
    parity: Option<NumberParity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumberParity {
    Odd,
    Even,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Anchor {
    index: usize,
    node_id: i64,
    number: u32,
    tags: BTreeMap<String, String>,
}

pub(crate) fn has_interpolation_tag(tags: &BTreeMap<String, String>) -> bool {
    tag_value(tags, "addr:interpolation").is_some()
}

pub(crate) fn write_interpolation_records(
    stub: &InterpolationWayStub,
    node_locations: &HashMap<i64, (f64, f64)>,
    address_node_tags: &HashMap<i64, BTreeMap<String, String>>,
    writer: &mut dyn RecordWriter,
    report: &mut BuilderReport,
) -> Result<()> {
    let Ok(rule) = interpolation_rule(&stub.tags) else {
        reject_interpolation(
            CandidateIssue::InterpolationUnsupportedValue,
            stub,
            writer,
            report,
        )?;
        return Ok(());
    };

    if stub.node_refs.is_empty() {
        reject_interpolation(
            CandidateIssue::InterpolationWayWithoutNodes,
            stub,
            writer,
            report,
        )?;
        return Ok(());
    }

    let Some(points) = resolve_node_ref_points(&stub.node_refs, node_locations) else {
        reject_interpolation(
            CandidateIssue::InterpolationUnresolvedGeometry,
            stub,
            writer,
            report,
        )?;
        return Ok(());
    };

    let anchors = match numeric_anchors(stub, address_node_tags) {
        Ok(anchors) => anchors,
        Err(issue) => {
            reject_interpolation(issue, stub, writer, report)?;
            return Ok(());
        }
    };
    let issue = if anchors.is_empty() {
        Some(CandidateIssue::InterpolationMissingAnchors)
    } else if anchors.len() == 1 {
        Some(CandidateIssue::InterpolationInsufficientNumericAnchors)
    } else {
        None
    };
    if let Some(issue) = issue {
        reject_interpolation(issue, stub, writer, report)?;
        return Ok(());
    }

    for pair in anchors.windows(2) {
        let start_anchor = &pair[0];
        let end_anchor = &pair[1];
        match interpolation_record_from_segment(stub, &rule, start_anchor, end_anchor, &points) {
            Ok(record) => {
                let record = NormalizedRecord::interpolation(record);
                writer.write_record(record.clone())?;
                report.accept(&record);
            }
            Err(issue) => {
                reject_interpolation(issue, stub, writer, report)?;
            }
        }
    }

    Ok(())
}

fn interpolation_record_from_segment(
    stub: &InterpolationWayStub,
    rule: &InterpolationRule,
    first_anchor: &Anchor,
    second_anchor: &Anchor,
    points: &[(f64, f64)],
) -> std::result::Result<InterpolationRecord, CandidateIssue> {
    let (low_anchor, high_anchor, reverse_geometry) = if first_anchor.number < second_anchor.number
    {
        (first_anchor, second_anchor, false)
    } else if first_anchor.number > second_anchor.number {
        (second_anchor, first_anchor, true)
    } else {
        return Err(CandidateIssue::InterpolationInvalidNumberRange);
    };

    validate_range(low_anchor.number, high_anchor.number, rule)?;

    let address = segment_address(&stub.tags, &low_anchor.tags, &high_anchor.tags)?;
    let name = address
        .street
        .as_deref()
        .or(address.place.as_deref())
        .ok_or(CandidateIssue::InterpolationMissingStreetOrPlace)?
        .to_string();

    let segment_points = segment_points_between(points, first_anchor.index, second_anchor.index)
        .ok_or(CandidateIssue::InterpolationUnresolvedGeometry)?;
    let segment_points = if reverse_geometry {
        segment_points.into_iter().rev().collect::<Vec<_>>()
    } else {
        segment_points
    };
    let built = line_string_geometry(&segment_points)
        .ok_or(CandidateIssue::InterpolationUnresolvedGeometry)?;

    let label = build_label(&name, low_anchor.number, high_anchor.number, rule, &address);
    let anchor_ids = vec![
        format!("osm:node:{}", low_anchor.node_id),
        format!("osm:node:{}", high_anchor.node_id),
    ];

    Ok(InterpolationRecord {
        id: format!(
            "osm:way:{}:interp:{}-{}",
            stub.object_id, low_anchor.node_id, high_anchor.node_id
        ),
        label,
        name,
        address,
        interpolation: InterpolationRange {
            kind: rule.kind.clone(),
            start: low_anchor.number,
            end: high_anchor.number,
            step: rule.step,
        },
        anchor_ids,
        geometry: built.geometry,
        representative_point: built.representative_point,
        source: SourceProvenance::osm(OsmObjectType::Way, stub.object_id),
    })
}

fn numeric_anchors(
    stub: &InterpolationWayStub,
    address_node_tags: &HashMap<i64, BTreeMap<String, String>>,
) -> std::result::Result<Vec<Anchor>, CandidateIssue> {
    let mut anchors = Vec::new();
    let mut found_housenumber = false;
    for (index, node_id) in stub.node_refs.iter().enumerate() {
        let Some(tags) = address_node_tags.get(node_id) else {
            continue;
        };
        let Some(house_number) = tag_value(tags, "addr:housenumber") else {
            continue;
        };
        found_housenumber = true;
        let Some(number) = parse_house_number(&house_number) else {
            continue;
        };
        anchors.push(Anchor {
            index,
            node_id: *node_id,
            number,
            tags: tags.clone(),
        });
    }

    if anchors.is_empty() && found_housenumber {
        return Err(CandidateIssue::InterpolationNonNumericAnchor);
    }

    Ok(anchors)
}

fn interpolation_rule(
    tags: &BTreeMap<String, String>,
) -> std::result::Result<InterpolationRule, CandidateIssue> {
    let value = tag_value(tags, "addr:interpolation")
        .ok_or(CandidateIssue::InterpolationUnsupportedValue)?;
    let normalized = value.to_ascii_lowercase();
    match normalized.as_str() {
        "odd" => Ok(InterpolationRule {
            kind: "odd".to_string(),
            step: 2,
            parity: Some(NumberParity::Odd),
        }),
        "even" => Ok(InterpolationRule {
            kind: "even".to_string(),
            step: 2,
            parity: Some(NumberParity::Even),
        }),
        "all" => Ok(InterpolationRule {
            kind: "all".to_string(),
            step: 1,
            parity: None,
        }),
        _ => {
            let Some(step) = parse_house_number(&normalized) else {
                return Err(CandidateIssue::InterpolationUnsupportedValue);
            };
            Ok(InterpolationRule {
                kind: step.to_string(),
                step,
                parity: None,
            })
        }
    }
}

fn validate_range(
    start: u32,
    end: u32,
    rule: &InterpolationRule,
) -> std::result::Result<(), CandidateIssue> {
    if start >= end {
        return Err(CandidateIssue::InterpolationInvalidNumberRange);
    }

    match rule.parity {
        Some(NumberParity::Odd) if start % 2 != 1 || end % 2 != 1 => {
            return Err(CandidateIssue::InterpolationInvalidParity);
        }
        Some(NumberParity::Even) if start % 2 != 0 || end % 2 != 0 => {
            return Err(CandidateIssue::InterpolationInvalidParity);
        }
        _ => {}
    }

    if (end - start) % rule.step != 0 {
        return Err(CandidateIssue::InterpolationInvalidNumberRange);
    }

    Ok(())
}

fn segment_address(
    way_tags: &BTreeMap<String, String>,
    start_tags: &BTreeMap<String, String>,
    end_tags: &BTreeMap<String, String>,
) -> std::result::Result<InterpolationAddressComponents, CandidateIssue> {
    let street = required_context("addr:street", way_tags, start_tags, end_tags)?;
    let place = required_context("addr:place", way_tags, start_tags, end_tags)?;
    if street.is_none() && place.is_none() {
        return Err(CandidateIssue::InterpolationMissingStreetOrPlace);
    }

    Ok(InterpolationAddressComponents {
        street,
        place,
        locality: optional_context("addr:city", way_tags, start_tags, end_tags),
        region: optional_context("addr:state", way_tags, start_tags, end_tags),
        postcode: optional_context("addr:postcode", way_tags, start_tags, end_tags),
        country: optional_context("addr:country", way_tags, start_tags, end_tags),
    })
}

fn required_context(
    key: &str,
    way_tags: &BTreeMap<String, String>,
    start_tags: &BTreeMap<String, String>,
    end_tags: &BTreeMap<String, String>,
) -> std::result::Result<Option<String>, CandidateIssue> {
    let way = tag_value(way_tags, key);
    let start = tag_value(start_tags, key);
    let end = tag_value(end_tags, key);

    if let Some(way_value) = way {
        for anchor_value in [start.as_deref(), end.as_deref()].into_iter().flatten() {
            if normalize_for_compare(anchor_value) != normalize_for_compare(&way_value) {
                return Err(CandidateIssue::InterpolationAnchorStreetMismatch);
            }
        }
        return Ok(Some(way_value));
    }

    match (start, end) {
        (Some(start), Some(end)) => {
            if normalize_for_compare(&start) == normalize_for_compare(&end) {
                Ok(Some(start))
            } else {
                Err(CandidateIssue::InterpolationAnchorStreetMismatch)
            }
        }
        _ => Ok(None),
    }
}

fn optional_context(
    key: &str,
    way_tags: &BTreeMap<String, String>,
    start_tags: &BTreeMap<String, String>,
    end_tags: &BTreeMap<String, String>,
) -> Option<String> {
    let values = [way_tags, start_tags, end_tags]
        .into_iter()
        .filter_map(|tags| tag_value(tags, key))
        .collect::<Vec<_>>();
    if values.is_empty() {
        return None;
    }
    let first = normalize_for_compare(&values[0]);
    if values
        .iter()
        .all(|value| normalize_for_compare(value) == first)
    {
        Some(values[0].clone())
    } else {
        None
    }
}

fn segment_points_between(
    points: &[(f64, f64)],
    start_index: usize,
    end_index: usize,
) -> Option<Vec<(f64, f64)>> {
    let start = start_index.min(end_index);
    let end = start_index.max(end_index);
    if end >= points.len() || end <= start {
        return None;
    }
    Some(points[start..=end].to_vec())
}

fn reject_interpolation(
    issue: CandidateIssue,
    stub: &InterpolationWayStub,
    writer: &mut dyn RecordWriter,
    report: &mut BuilderReport,
) -> Result<()> {
    let addr_tags = collect_addr_tags_from_map(&stub.tags);
    report.reject_with_tags(issue, OsmObjectType::Way, &stub.tags, &addr_tags);
    write_rejected_record(
        issue,
        OsmObjectType::Way,
        stub.object_id,
        &stub.tags,
        Some("interpolation"),
        writer,
    )
}

fn build_label(
    name: &str,
    start: u32,
    end: u32,
    rule: &InterpolationRule,
    address: &InterpolationAddressComponents,
) -> String {
    [
        Some(format!("{name} {start}-{end} {}", rule.kind)),
        address.locality.clone(),
        address.region.clone(),
        address.postcode.clone(),
        address.country.clone(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ")
}

fn parse_house_number(value: &str) -> Option<u32> {
    let value = value.trim();
    if value.is_empty() || !value.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    let number = value.parse::<u32>().ok()?;
    if number == 0 { None } else { Some(number) }
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
    use crate::pack::test_support::MemoryRecordWriter;

    use super::*;

    #[test]
    fn emits_one_segment_per_numeric_anchor_pair() {
        let stub = InterpolationWayStub {
            object_id: 42,
            node_refs: vec![1, 2, 3],
            tags: BTreeMap::from([
                ("addr:interpolation".to_string(), "odd".to_string()),
                ("addr:street".to_string(), "King Street".to_string()),
            ]),
        };
        let node_locations =
            HashMap::from([(1, (43.0, -79.0)), (2, (43.1, -79.1)), (3, (43.2, -79.2))]);
        let address_node_tags = HashMap::from([
            (
                1,
                BTreeMap::from([
                    ("addr:housenumber".to_string(), "101".to_string()),
                    ("addr:street".to_string(), "King Street".to_string()),
                ]),
            ),
            (
                2,
                BTreeMap::from([
                    ("addr:housenumber".to_string(), "103".to_string()),
                    ("addr:street".to_string(), "King Street".to_string()),
                ]),
            ),
            (
                3,
                BTreeMap::from([
                    ("addr:housenumber".to_string(), "105".to_string()),
                    ("addr:street".to_string(), "King Street".to_string()),
                ]),
            ),
        ]);
        let mut writer = MemoryRecordWriter::default();
        let mut report = BuilderReport::default();

        write_interpolation_records(
            &stub,
            &node_locations,
            &address_node_tags,
            &mut writer,
            &mut report,
        )
        .expect("write interpolation");

        assert_eq!(writer.records.len(), 2);
        assert_eq!(writer.records[0].layer(), "interpolation");
        let NormalizedRecord::Interpolation(first) = &writer.records[0] else {
            panic!("expected interpolation");
        };
        let NormalizedRecord::Interpolation(second) = &writer.records[1] else {
            panic!("expected interpolation");
        };
        assert_eq!(first.interpolation.start, 101);
        assert_eq!(second.interpolation.start, 103);
        assert!(writer.rejections.is_empty());
        assert_eq!(report.accepted.interpolation_ranges, 2);
    }

    #[test]
    fn reverses_descending_segment_geometry() {
        let stub = InterpolationWayStub {
            object_id: 42,
            node_refs: vec![1, 2],
            tags: BTreeMap::from([
                ("addr:interpolation".to_string(), "even".to_string()),
                ("addr:street".to_string(), "King Street".to_string()),
            ]),
        };
        let rule = interpolation_rule(&stub.tags).expect("rule");
        let first_anchor = Anchor {
            index: 0,
            node_id: 1,
            number: 200,
            tags: BTreeMap::from([("addr:street".to_string(), "King Street".to_string())]),
        };
        let second_anchor = Anchor {
            index: 1,
            node_id: 2,
            number: 100,
            tags: BTreeMap::from([("addr:street".to_string(), "King Street".to_string())]),
        };

        let record = interpolation_record_from_segment(
            &stub,
            &rule,
            &first_anchor,
            &second_anchor,
            &[(43.0, -79.0), (44.0, -80.0)],
        )
        .expect("record");

        assert_eq!(record.interpolation.start, 100);
        assert_eq!(record.interpolation.end, 200);
        assert_eq!(
            record.anchor_ids,
            vec!["osm:node:2".to_string(), "osm:node:1".to_string()]
        );
        assert!(
            record
                .geometry
                .to_string()
                .contains("\"coordinates\":[[-80.0,44.0],[-79.0,43.0]]")
        );
    }

    #[test]
    fn rejects_anchor_street_mismatch() {
        let way_tags = BTreeMap::from([("addr:street".to_string(), "Main Street".to_string())]);
        let start_tags = BTreeMap::from([("addr:street".to_string(), "Main Street".to_string())]);
        let end_tags = BTreeMap::from([("addr:street".to_string(), "Queen Street".to_string())]);

        assert_eq!(
            segment_address(&way_tags, &start_tags, &end_tags),
            Err(CandidateIssue::InterpolationAnchorStreetMismatch)
        );
    }

    #[test]
    fn inherits_street_from_matching_anchors() {
        let way_tags = BTreeMap::new();
        let start_tags = BTreeMap::from([("addr:street".to_string(), "Main Street".to_string())]);
        let end_tags = BTreeMap::from([("addr:street".to_string(), " main   street ".to_string())]);

        let address = segment_address(&way_tags, &start_tags, &end_tags).expect("address");

        assert_eq!(address.street.as_deref(), Some("Main Street"));
    }
}
