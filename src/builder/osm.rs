mod address;
mod collector;
mod geometry;
mod interpolation;
mod pbf;
mod place;
mod postcode;
mod street;

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    time::Instant,
};

use anyhow::Result;

use crate::{
    builder::{progress::item_progress_bar, report::BuilderReport},
    pack::{PackWriter, RecordWriter},
    record::{LocationPrecision, OsmObjectType},
};
use address::{AddressCandidate, write_candidate, write_rejected_record};
use collector::{
    AddressWayStub, CollectedRejection, PlaceNodeCandidate, discover_address_features,
};
use geometry::{centroid, resolve_required_node_locations, resolve_way_points};
use interpolation::{InterpolationWayStub, write_interpolation_records};
use place::write_place_node;
use postcode::PostcodeAccumulator;
use street::{StreetWayStub, write_street_record};

use super::report::CandidateIssue;

#[derive(Debug, Clone)]
pub struct BuildOsmOptions {
    pub input: PathBuf,
    pub pack: PathBuf,
}

pub fn build_osm_pack(options: BuildOsmOptions) -> Result<()> {
    let total_started = Instant::now();
    let pack_create_started = Instant::now();
    let mut pack_writer = PackWriter::create(&options.pack)?;
    let pack_create_ms = pack_create_started.elapsed().as_millis();

    let discovery_started = Instant::now();
    let mut discovery = discover_address_features(&options.input)?;
    discovery.report.phases.pack_create_ms = pack_create_ms;
    discovery.report.phases.discovery_ms = discovery_started.elapsed().as_millis();
    discovery.report.pack = options.pack.display().to_string();
    discovery.report.geometry_resolution.address_way_stubs = discovery.way_stubs.len();
    discovery.report.geometry_resolution.interpolation_way_stubs =
        discovery.interpolation_way_stubs.len();
    discovery.report.geometry_resolution.street_way_stubs = discovery.street_way_stubs.len();
    discovery.report.geometry_resolution.required_node_refs = discovery.required_node_ids.len();

    let resolution_started = Instant::now();
    let node_locations =
        resolve_required_node_locations(&options.input, &discovery.required_node_ids)?;
    discovery.report.phases.coordinate_resolution_ms = resolution_started.elapsed().as_millis();
    discovery.report.geometry_resolution.resolved_node_refs = node_locations.len();
    discovery.report.node_cache_entries = node_locations.len();

    let emission_started = Instant::now();
    let mut postcode_accumulator = PostcodeAccumulator::default();
    emit_normalized_records(
        &mut pack_writer,
        EmissionInputs {
            place_node_candidates: &discovery.place_node_candidates,
            address_node_candidates: &discovery.address_node_candidates,
            way_stubs: &discovery.way_stubs,
            interpolation_way_stubs: &discovery.interpolation_way_stubs,
            street_way_stubs: &discovery.street_way_stubs,
            rejections: &discovery.rejections,
            address_node_tags: &discovery.address_node_tags,
            node_locations: &node_locations,
        },
        &mut postcode_accumulator,
        &mut discovery.report,
    )?;
    discovery.report.phases.record_emission_ms = emission_started.elapsed().as_millis();
    discovery.report.phases.total_ms = total_started.elapsed().as_millis();
    pack_writer.finish(&mut discovery.report)?;

    Ok(())
}

struct EmissionInputs<'a> {
    place_node_candidates: &'a [PlaceNodeCandidate],
    address_node_candidates: &'a [AddressCandidate],
    way_stubs: &'a [AddressWayStub],
    interpolation_way_stubs: &'a [InterpolationWayStub],
    street_way_stubs: &'a [StreetWayStub],
    rejections: &'a [CollectedRejection],
    address_node_tags: &'a HashMap<i64, BTreeMap<String, String>>,
    node_locations: &'a HashMap<i64, (f64, f64)>,
}

fn emit_normalized_records(
    writer: &mut dyn RecordWriter,
    inputs: EmissionInputs<'_>,
    postcode_accumulator: &mut PostcodeAccumulator,
    report: &mut BuilderReport,
) -> Result<()> {
    for rejection in inputs.rejections {
        write_rejected_record(
            rejection.issue,
            rejection.object_type,
            rejection.object_id,
            &rejection.tags,
            rejection.layer_hint,
            writer,
        )?;
    }

    for candidate in inputs.place_node_candidates {
        write_place_node(
            candidate.object_id,
            candidate.lat,
            candidate.lon,
            &candidate.tags,
            writer,
            report,
        )?;
    }

    let progress = item_progress_bar(
        (inputs.address_node_candidates.len() + inputs.way_stubs.len()) as u64,
        "3/7 emit address records",
    );
    for candidate in inputs.address_node_candidates {
        if let Some(record) = write_candidate(candidate.clone(), writer, report)? {
            postcode_accumulator.accept_address(&record);
        }
        progress.inc(1);
    }

    for stub in inputs.way_stubs {
        let Some(points) = resolve_way_points(stub, inputs.node_locations) else {
            report.reject(CandidateIssue::WayWithoutResolvedNodes);
            write_rejected_record(
                CandidateIssue::WayWithoutResolvedNodes,
                OsmObjectType::Way,
                stub.object_id,
                &stub.tags,
                Some("address"),
                writer,
            )?;
            progress.inc(1);
            continue;
        };

        let Some((lat, lon)) = centroid(&points) else {
            report.reject(CandidateIssue::WayWithoutResolvedNodes);
            write_rejected_record(
                CandidateIssue::WayWithoutResolvedNodes,
                OsmObjectType::Way,
                stub.object_id,
                &stub.tags,
                Some("address"),
                writer,
            )?;
            progress.inc(1);
            continue;
        };

        let candidate = AddressCandidate {
            object_type: OsmObjectType::Way,
            object_id: stub.object_id,
            lat,
            lon,
            location_precision: LocationPrecision::Centroid,
            tags: stub.tags.clone(),
        };
        if let Some(record) = write_candidate(candidate, writer, report)? {
            postcode_accumulator.accept_address(&record);
        }
        progress.inc(1);
    }
    progress.finish_with_message("3/7 emit address records complete");

    postcode_accumulator.write_records(writer, report)?;

    let progress = item_progress_bar(
        inputs.interpolation_way_stubs.len() as u64,
        "4/7 emit interpolation ranges",
    );
    for stub in inputs.interpolation_way_stubs {
        write_interpolation_records(
            stub,
            inputs.node_locations,
            inputs.address_node_tags,
            writer,
            report,
        )?;
        progress.inc(1);
    }
    progress.finish_with_message("4/7 emit interpolation ranges complete");

    let progress = item_progress_bar(
        inputs.street_way_stubs.len() as u64,
        "5/7 emit street segments",
    );
    for stub in inputs.street_way_stubs {
        write_street_record(stub, inputs.node_locations, writer, report)?;
        progress.inc(1);
    }
    progress.finish_with_message("5/7 emit street segments complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use crate::pack::test_support::MemoryRecordWriter;

    use super::address::AddressCandidate;
    use super::*;

    #[test]
    fn emits_node_records_and_resolved_way_records_deterministically() {
        let mut writer = MemoryRecordWriter::default();
        let node_candidate = AddressCandidate {
            object_type: OsmObjectType::Node,
            object_id: 100,
            lat: 43.0,
            lon: -79.0,
            location_precision: LocationPrecision::Point,
            tags: BTreeMap::from([
                ("addr:housenumber".to_string(), "10".to_string()),
                ("addr:street".to_string(), "King Street".to_string()),
            ]),
        };

        let way_stubs = vec![AddressWayStub {
            object_id: 200,
            node_refs: vec![1, 2, 3, 1],
            tags: BTreeMap::from([
                ("addr:housenumber".to_string(), "20".to_string()),
                ("addr:street".to_string(), "Queen Street".to_string()),
            ]),
        }];
        let street_way_stubs = vec![StreetWayStub {
            object_id: 300,
            node_refs: vec![1, 2],
            tags: BTreeMap::from([
                ("highway".to_string(), "residential".to_string()),
                ("name".to_string(), "King Street".to_string()),
            ]),
        }];
        let node_locations = HashMap::from([(1, (0.0, 0.0)), (2, (0.0, 2.0)), (3, (2.0, 0.0))]);
        let mut report = BuilderReport::default();
        let mut postcode_accumulator = PostcodeAccumulator::default();

        emit_normalized_records(
            &mut writer,
            EmissionInputs {
                place_node_candidates: &[],
                address_node_candidates: &[node_candidate],
                way_stubs: &way_stubs,
                interpolation_way_stubs: &[],
                street_way_stubs: &street_way_stubs,
                rejections: &[],
                address_node_tags: &HashMap::new(),
                node_locations: &node_locations,
            },
            &mut postcode_accumulator,
            &mut report,
        )
        .expect("emit");

        assert_eq!(writer.records.len(), 3);
        assert_eq!(writer.records[0].layer(), "address");
        assert_eq!(writer.records[0].id(), "osm:node:100");
        assert_eq!(writer.records[1].id(), "osm:way:200");
        assert_eq!(writer.records[2].layer(), "street");
        assert_eq!(writer.records[2].id(), "osm:way:300");
        assert_eq!(writer.records[2].label(), "King Street");
        assert_eq!(report.accepted.node_addresses, 1);
        assert_eq!(report.accepted.way_centroid_addresses, 1);
        assert_eq!(report.accepted.street_segments, 1);
        assert_eq!(report.accepted.by_layer.get("street"), Some(&1),);
    }
}
