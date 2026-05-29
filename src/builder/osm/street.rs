use std::collections::{BTreeMap, HashMap};

use anyhow::Result;

use crate::{
    builder::report::{BuilderReport, CandidateIssue},
    pack::RecordWriter,
    record::{OsmObjectType, SourceProvenance, StreetRecord},
};

use super::{
    address::write_rejected_record,
    geometry::{line_string_geometry, resolve_node_ref_points},
    tags::OsmTags,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StreetWayStub {
    pub object_id: i64,
    pub node_refs: Vec<i64>,
    pub tags: BTreeMap<String, String>,
}

pub(crate) fn has_highway_tag(tags: &BTreeMap<String, String>) -> bool {
    tags.has("highway")
}

pub(crate) fn street_name(tags: &BTreeMap<String, String>) -> Option<String> {
    tags.cleaned("name")
}

pub(crate) fn missing_street_name_issue(tags: &BTreeMap<String, String>) -> CandidateIssue {
    if tags.has("ref") {
        CandidateIssue::StreetRefOnlyName
    } else {
        CandidateIssue::StreetMissingName
    }
}

pub(crate) fn write_street_record(
    stub: &StreetWayStub,
    node_locations: &HashMap<i64, (f64, f64)>,
    writer: &mut dyn RecordWriter,
    report: &mut BuilderReport,
) -> Result<()> {
    let Some(record) = street_record_from_stub(stub, node_locations) else {
        reject_street_geometry(stub, writer, report)?;
        return Ok(());
    };

    writer.write_street(&record)?;
    report.accept_street();
    Ok(())
}

fn street_record_from_stub(
    stub: &StreetWayStub,
    node_locations: &HashMap<i64, (f64, f64)>,
) -> Option<StreetRecord> {
    let name = street_name(&stub.tags)?;
    let points = resolve_node_ref_points(&stub.node_refs, node_locations)?;
    let built = line_string_geometry(&points)?;

    Some(StreetRecord {
        name,
        geometry: built.geometry,
        representative_point: built.representative_point,
        source: SourceProvenance::osm(OsmObjectType::Way, stub.object_id),
    })
}

fn reject_street_geometry(
    stub: &StreetWayStub,
    writer: &mut dyn RecordWriter,
    report: &mut BuilderReport,
) -> Result<()> {
    let empty_addr_tags = BTreeMap::new();
    report.reject_with_context(
        CandidateIssue::StreetUnresolvedGeometry,
        OsmObjectType::Way,
        stub.object_id,
        &stub.tags,
        Some(&empty_addr_tags),
        Some("street"),
        true,
    );
    write_rejected_record(
        CandidateIssue::StreetUnresolvedGeometry,
        OsmObjectType::Way,
        stub.object_id,
        &stub.tags,
        Some("street"),
        writer,
    )
}

#[cfg(test)]
mod tests {
    use geojson::GeometryValue;

    use crate::pack::test_support::MemoryRecordWriter;

    use super::*;

    #[test]
    fn builds_street_record_from_named_highway_way() {
        let stub = StreetWayStub {
            object_id: 99,
            node_refs: vec![1, 2],
            tags: BTreeMap::from([
                ("highway".to_string(), "residential".to_string()),
                ("name".to_string(), " King   Street ".to_string()),
            ]),
        };
        let node_locations = HashMap::from([(1, (43.64, -79.41)), (2, (43.66, -79.36))]);

        let record = street_record_from_stub(&stub, &node_locations).expect("street record");

        assert_eq!(record.id(), "osm:way:99");
        assert_eq!(record.label(), "King Street");
        assert_eq!(record.name, "King Street");
        assert!((record.representative_point[0] - -79.385).abs() < 0.000001);
        assert!((record.representative_point[1] - 43.65).abs() < 0.000001);
        assert_eq!(record.source.object_type, OsmObjectType::Way);
        assert_eq!(record.source.object_id, 99);
        assert_eq!(record.source.tags, None);
        match record.geometry.value {
            GeometryValue::LineString { coordinates } => {
                assert_eq!(coordinates.len(), 2);
                assert_eq!(coordinates[0].as_slice(), &[-79.41, 43.64]);
            }
            other => panic!("expected LineString, got {}", other.type_name()),
        }
    }

    #[test]
    fn rejects_named_highway_without_complete_geometry() {
        let stub = StreetWayStub {
            object_id: 99,
            node_refs: vec![1, 2],
            tags: BTreeMap::from([
                ("highway".to_string(), "residential".to_string()),
                ("name".to_string(), "King Street".to_string()),
            ]),
        };
        let node_locations = HashMap::from([(1, (43.64, -79.41))]);
        let mut writer = MemoryRecordWriter::default();
        let mut report = BuilderReport::default();

        write_street_record(&stub, &node_locations, &mut writer, &mut report)
            .expect("write street");

        assert!(writer.records.is_empty());
        assert_eq!(writer.rejections.len(), 1);
        assert_eq!(writer.rejections[0].reason, "street_unresolved_geometry");
        assert_eq!(writer.rejections[0].layer_hint.as_deref(), Some("street"));
        assert_eq!(report.rejected.total, 1);
        assert_eq!(report.disposition.unresolved_geometry, 1);
    }

    #[test]
    fn distinguishes_ref_only_highways_from_unnamed_highways() {
        let ref_only = BTreeMap::from([
            ("highway".to_string(), "primary".to_string()),
            ("ref".to_string(), "401".to_string()),
        ]);
        let unnamed = BTreeMap::from([("highway".to_string(), "path".to_string())]);

        assert_eq!(
            missing_street_name_issue(&ref_only),
            CandidateIssue::StreetRefOnlyName
        );
        assert_eq!(
            missing_street_name_issue(&unnamed),
            CandidateIssue::StreetMissingName
        );
    }
}
