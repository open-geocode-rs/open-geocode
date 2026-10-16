use std::collections::BTreeMap;

use crate::{
    builder::report::CandidateIssue,
    record::{
        InterpolationAddressComponents, InterpolationRange, InterpolationRecord, OsmObjectType,
        Record, SourceProvenance,
    },
    util::text::normalize_for_compare,
};

use super::{emitted::Emitted, geometry::line_string_geometry, tags::OsmTags};

/// A vertex of an interpolation way with the address tags of its node, which
/// make it a numbered anchor.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InterpolationNode {
    pub node_id: i64,
    pub lat: f64,
    pub lon: f64,
    pub addr_tags: Option<BTreeMap<String, String>>,
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
    tags.has("addr:interpolation")
}

/// Emit one interpolation record per pair of consecutive numeric anchors.
/// `nodes` is `None` when any vertex could not be resolved.
pub(crate) fn emit_interpolation(
    way_id: i64,
    tags: &BTreeMap<String, String>,
    nodes: Option<&[InterpolationNode]>,
    out: &mut Emitted,
) {
    let Ok(rule) = interpolation_rule(tags) else {
        reject_interpolation(
            CandidateIssue::InterpolationUnsupportedValue,
            way_id,
            tags,
            out,
        );
        return;
    };

    let Some(nodes) = nodes else {
        reject_interpolation(
            CandidateIssue::InterpolationUnresolvedGeometry,
            way_id,
            tags,
            out,
        );
        return;
    };
    let points = nodes
        .iter()
        .map(|node| (node.lat, node.lon))
        .collect::<Vec<_>>();

    let anchors = match numeric_anchors(nodes) {
        Ok(anchors) => anchors,
        Err(issue) => {
            reject_interpolation(issue, way_id, tags, out);
            return;
        }
    };
    let issue = match anchors.len() {
        0 => Some(CandidateIssue::InterpolationMissingAnchors),
        1 => Some(CandidateIssue::InterpolationInsufficientNumericAnchors),
        _ => None,
    };
    if let Some(issue) = issue {
        reject_interpolation(issue, way_id, tags, out);
        return;
    }

    for pair in anchors.windows(2) {
        match interpolation_record_from_segment(way_id, tags, &rule, &pair[0], &pair[1], &points) {
            Ok(record) => {
                out.report.accept_interpolation();
                out.records.push(Record::Interpolation(record));
            }
            Err(issue) => reject_interpolation(issue, way_id, tags, out),
        }
    }
}

fn interpolation_record_from_segment(
    way_id: i64,
    way_tags: &BTreeMap<String, String>,
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

    let address = segment_address(way_tags, &low_anchor.tags, &high_anchor.tags)?;
    if address.street.is_none() && address.place.is_none() {
        return Err(CandidateIssue::InterpolationMissingStreetOrPlace);
    }

    let segment_points = segment_points_between(points, first_anchor.index, second_anchor.index)
        .ok_or(CandidateIssue::InterpolationUnresolvedGeometry)?;
    let segment_points = if reverse_geometry {
        segment_points.into_iter().rev().collect::<Vec<_>>()
    } else {
        segment_points
    };
    let built = line_string_geometry(&segment_points)
        .ok_or(CandidateIssue::InterpolationUnresolvedGeometry)?;

    Ok(InterpolationRecord {
        address,
        interpolation: InterpolationRange {
            kind: rule.kind.clone(),
            start: low_anchor.number,
            end: high_anchor.number,
            step: rule.step,
        },
        anchor_node_ids: [low_anchor.node_id, high_anchor.node_id],
        geometry: built.geometry,
        representative_point: built.representative_point,
        source: SourceProvenance::osm(OsmObjectType::Way, way_id),
    })
}

fn numeric_anchors(
    nodes: &[InterpolationNode],
) -> std::result::Result<Vec<Anchor>, CandidateIssue> {
    let mut anchors = Vec::new();
    let mut found_housenumber = false;
    for (index, node) in nodes.iter().enumerate() {
        let Some(tags) = &node.addr_tags else {
            continue;
        };
        let Some(house_number) = tags.cleaned("addr:housenumber") else {
            continue;
        };
        found_housenumber = true;
        let Some(number) = parse_house_number(&house_number) else {
            continue;
        };
        anchors.push(Anchor {
            index,
            node_id: node.node_id,
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
    let value = tags
        .cleaned("addr:interpolation")
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
        Some(NumberParity::Even) if !start.is_multiple_of(2) || !end.is_multiple_of(2) => {
            return Err(CandidateIssue::InterpolationInvalidParity);
        }
        _ => {}
    }

    if !(end - start).is_multiple_of(rule.step) {
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
    let way = way_tags.cleaned(key);
    let start = start_tags.cleaned(key);
    let end = end_tags.cleaned(key);

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
        .filter_map(|tags| tags.cleaned(key))
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
    way_id: i64,
    tags: &BTreeMap<String, String>,
    out: &mut Emitted,
) {
    out.reject(
        issue,
        OsmObjectType::Way,
        way_id,
        tags,
        Some(tags),
        Some("interpolation"),
    );
}

fn parse_house_number(value: &str) -> Option<u32> {
    let value = value.trim();
    if value.is_empty() || !value.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    let number = value.parse::<u32>().ok()?;
    if number == 0 { None } else { Some(number) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn way_tags(kind: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("addr:interpolation".to_string(), kind.to_string()),
            ("addr:street".to_string(), "King Street".to_string()),
        ])
    }

    fn node(node_id: i64, lat: f64, number: Option<&str>) -> InterpolationNode {
        InterpolationNode {
            node_id,
            lat,
            lon: -79.0 - (lat - 43.0),
            addr_tags: number.map(|number| {
                BTreeMap::from([
                    ("addr:housenumber".to_string(), number.to_string()),
                    ("addr:street".to_string(), "King Street".to_string()),
                ])
            }),
        }
    }

    #[test]
    fn emits_one_segment_per_numeric_anchor_pair() {
        let nodes = [
            node(1, 43.0, Some("101")),
            node(2, 43.1, Some("103")),
            node(3, 43.2, Some("105")),
        ];
        let mut out = Emitted::default();
        emit_interpolation(42, &way_tags("odd"), Some(&nodes), &mut out);

        assert_eq!(out.records.len(), 2);
        let Record::Interpolation(first) = &out.records[0] else {
            panic!("expected interpolation");
        };
        let Record::Interpolation(second) = &out.records[1] else {
            panic!("expected interpolation");
        };
        assert_eq!(first.interpolation.start, 101);
        assert_eq!(second.interpolation.start, 103);
        assert_eq!(first.anchor_node_ids, [1, 2]);
        assert!(out.rejections.is_empty());
        assert_eq!(out.report.accepted.interpolation_ranges, 2);
    }

    #[test]
    fn reverses_descending_segment_geometry() {
        let tags = way_tags("even");
        let rule = interpolation_rule(&tags).expect("rule");
        let anchor = |index, node_id, number| Anchor {
            index,
            node_id,
            number,
            tags: BTreeMap::from([("addr:street".to_string(), "King Street".to_string())]),
        };

        let record = interpolation_record_from_segment(
            42,
            &tags,
            &rule,
            &anchor(0, 1, 200),
            &anchor(1, 2, 100),
            &[(43.0, -79.0), (44.0, -80.0)],
        )
        .expect("record");

        assert_eq!(record.interpolation.start, 100);
        assert_eq!(record.interpolation.end, 200);
        assert_eq!(record.anchor_node_ids, [2, 1]);
        assert!(
            record
                .geometry
                .to_string()
                .contains("\"coordinates\":[[-80.0,44.0],[-79.0,43.0]]")
        );
    }

    #[test]
    fn rejects_unresolved_geometry_and_missing_anchors() {
        let mut out = Emitted::default();
        emit_interpolation(42, &way_tags("odd"), None, &mut out);
        emit_interpolation(
            43,
            &way_tags("odd"),
            Some(&[node(1, 43.0, None), node(2, 43.1, None)]),
            &mut out,
        );
        emit_interpolation(44, &way_tags("sometimes"), None, &mut out);
        let reasons = out
            .rejections
            .iter()
            .map(|rejection| rejection.reason.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            reasons,
            vec![
                "interpolation_unresolved_geometry",
                "interpolation_missing_anchors",
                "interpolation_unsupported_value",
            ]
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
