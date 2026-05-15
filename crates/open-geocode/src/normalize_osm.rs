mod address;
mod collector;
mod geometry;
mod osm_reader;
mod progress;
mod report;

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result};

use address::{AddressCandidate, write_candidate};
pub use address::{AddressRecord, LocationPrecision, OsmObjectType, RecordKind, SourceProvenance};
use collector::{AddressWayStub, discover_address_features};
use geometry::{centroid, resolve_required_node_locations, resolve_way_points};
use progress::item_progress_bar;
use report::CandidateIssue;
pub use report::{
    AcceptedCounts, CandidateDispositionCounts, CompletenessCounts, GeometryResolutionCounts,
    ImportReport, PhaseTimings, RejectedCounts, ScannedCounts,
};

#[derive(Debug, Clone)]
pub struct NormalizeOsmOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    pub report: PathBuf,
}

pub fn normalize_osm(options: NormalizeOsmOptions) -> Result<()> {
    ensure_parent_dir(&options.output)?;
    ensure_parent_dir(&options.report)?;

    let total_started = Instant::now();
    let node_records_path = temp_node_records_path(&options.output);
    let _ = fs::remove_file(&options.output);
    let _ = fs::remove_file(&node_records_path);

    let discovery_started = Instant::now();
    let mut discovery = discover_address_features(&options.input, &node_records_path)?;
    discovery.report.phases.discovery_ms = discovery_started.elapsed().as_millis();
    discovery.report.output = options.output.display().to_string();
    discovery.report.geometry_resolution.address_way_stubs = discovery.way_stubs.len();
    discovery.report.geometry_resolution.required_node_refs = discovery.required_node_ids.len();

    let resolution_started = Instant::now();
    let node_locations =
        resolve_required_node_locations(&options.input, &discovery.required_node_ids)?;
    discovery.report.phases.coordinate_resolution_ms = resolution_started.elapsed().as_millis();
    discovery.report.geometry_resolution.resolved_node_refs = node_locations.len();
    discovery.report.node_cache_entries = node_locations.len();

    let emission_started = Instant::now();
    emit_normalized_records(
        &options.output,
        &node_records_path,
        &discovery.way_stubs,
        &node_locations,
        &mut discovery.report,
    )?;
    discovery.report.phases.record_emission_ms = emission_started.elapsed().as_millis();

    discovery.report.output_ndjson_bytes = fs::metadata(&options.output)
        .with_context(|| format!("failed to stat {}", options.output.display()))?
        .len();
    discovery.report.phases.total_ms = total_started.elapsed().as_millis();

    let report_file = File::create(&options.report)
        .with_context(|| format!("failed to create {}", options.report.display()))?;
    serde_json::to_writer_pretty(BufWriter::new(report_file), &discovery.report)
        .with_context(|| format!("failed to write {}", options.report.display()))?;

    let _ = fs::remove_file(node_records_path);

    Ok(())
}

fn emit_normalized_records(
    output_path: &Path,
    node_records_path: &Path,
    way_stubs: &[AddressWayStub],
    node_locations: &HashMap<i64, (f64, f64)>,
    report: &mut ImportReport,
) -> Result<()> {
    let output_file = File::create(output_path)
        .with_context(|| format!("failed to create {}", output_path.display()))?;
    let mut output = BufWriter::new(output_file);

    let mut node_records = File::open(node_records_path)
        .with_context(|| format!("failed to open {}", node_records_path.display()))?;
    io::copy(&mut node_records, &mut output)
        .with_context(|| format!("failed to copy {}", node_records_path.display()))?;

    let progress = item_progress_bar(way_stubs.len() as u64, "3/3 emit way centroids");
    for stub in way_stubs {
        let Some(points) = resolve_way_points(stub, node_locations) else {
            report.reject(CandidateIssue::WayWithoutResolvedNodes);
            progress.inc(1);
            continue;
        };

        let Some((lat, lon)) = centroid(&points) else {
            report.reject(CandidateIssue::WayWithoutResolvedNodes);
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
        write_candidate(candidate, &mut output, report)?;
        progress.inc(1);
    }
    progress.finish_with_message("3/3 emit way centroids complete");

    output
        .flush()
        .with_context(|| format!("failed to flush {}", output_path.display()))?;
    Ok(())
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    Ok(())
}

fn temp_node_records_path(output: &Path) -> PathBuf {
    let file_name = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("address-records.ndjson");
    output.with_file_name(format!("{file_name}.nodes.tmp"))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, HashMap},
        fs,
    };

    use super::*;
    use crate::normalize_osm::address::{AddressCandidate, write_record};

    #[test]
    fn emits_node_records_and_resolved_way_records_deterministically() {
        let temp_dir =
            std::env::temp_dir().join(format!("open-geocode-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).expect("temp dir");

        let node_records_path = temp_dir.join("nodes.ndjson");
        let output_path = temp_dir.join("records.ndjson");
        let mut node_records = BufWriter::new(File::create(&node_records_path).expect("nodes"));
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
        let node_record =
            address::address_record_from_candidate(node_candidate).expect("node record");
        write_record(&node_record, &mut node_records).expect("write node");
        node_records.flush().expect("flush node");

        let way_stubs = vec![AddressWayStub {
            object_id: 200,
            node_refs: vec![1, 2, 3, 1],
            tags: BTreeMap::from([
                ("addr:housenumber".to_string(), "20".to_string()),
                ("addr:street".to_string(), "Queen Street".to_string()),
            ]),
        }];
        let node_locations = HashMap::from([(1, (0.0, 0.0)), (2, (0.0, 2.0)), (3, (2.0, 0.0))]);
        let mut report = ImportReport::default();

        emit_normalized_records(
            &output_path,
            &node_records_path,
            &way_stubs,
            &node_locations,
            &mut report,
        )
        .expect("emit");

        let output = fs::read_to_string(&output_path).expect("output");
        let lines = output.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"record_id\":\"osm:node:100\""));
        assert!(lines[1].contains("\"record_id\":\"osm:way:200\""));
        assert_eq!(report.accepted.way_centroid_addresses, 1);

        let _ = fs::remove_dir_all(temp_dir);
    }
}
