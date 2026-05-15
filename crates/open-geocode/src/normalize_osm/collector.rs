use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

use anyhow::{Context, Result};
use osmpbf::Element;

use super::{
    address::{
        AddressCandidate, LocationPrecision, OsmObjectType, collect_addr_tags,
        validate_address_tags, write_candidate,
    },
    osm_reader::{element_reader_with_progress, input_bytes},
    report::{CandidateIssue, ImportReport},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddressWayStub {
    pub object_id: i64,
    pub node_refs: Vec<i64>,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug)]
pub(crate) struct DiscoveryResult {
    pub report: ImportReport,
    pub way_stubs: Vec<AddressWayStub>,
    pub required_node_ids: HashSet<i64>,
}

pub(crate) fn discover_address_features(
    input: &Path,
    node_records_path: &Path,
) -> Result<DiscoveryResult> {
    let mut report = ImportReport {
        schema_version: 1,
        input: input.display().to_string(),
        output: node_records_path.display().to_string(),
        input_bytes: input_bytes(input)?,
        ..ImportReport::default()
    };
    let output_file = File::create(node_records_path)
        .with_context(|| format!("failed to create {}", node_records_path.display()))?;
    let mut node_records = BufWriter::new(output_file);
    let mut way_stubs = Vec::new();
    let mut required_node_ids = HashSet::new();
    let mut write_error: Option<anyhow::Error> = None;
    let (reader, progress) = element_reader_with_progress(input, "1/3 discover address features")?;

    reader
        .for_each(|element| match element {
            Element::DenseNode(node) => {
                report.scanned.dense_nodes += 1;
                if write_error.is_some() {
                    return;
                }
                let tags = collect_addr_tags(node.tags());
                if tags.is_empty() {
                    return;
                }
                let candidate = AddressCandidate {
                    object_type: OsmObjectType::Node,
                    object_id: node.id(),
                    lat: node.lat(),
                    lon: node.lon(),
                    location_precision: LocationPrecision::Point,
                    tags,
                };
                if let Err(error) = write_candidate(candidate, &mut node_records, &mut report) {
                    write_error = Some(error);
                }
            }
            Element::Node(node) => {
                report.scanned.nodes += 1;
                if write_error.is_some() {
                    return;
                }
                let tags = collect_addr_tags(node.tags());
                if tags.is_empty() {
                    return;
                }
                let candidate = AddressCandidate {
                    object_type: OsmObjectType::Node,
                    object_id: node.id(),
                    lat: node.lat(),
                    lon: node.lon(),
                    location_precision: LocationPrecision::Point,
                    tags,
                };
                if let Err(error) = write_candidate(candidate, &mut node_records, &mut report) {
                    write_error = Some(error);
                }
            }
            Element::Way(way) => {
                report.scanned.ways += 1;
                if write_error.is_some() {
                    return;
                }
                let tags = collect_addr_tags(way.tags());
                if tags.is_empty() {
                    return;
                }

                if let Err(issue) = validate_address_tags(&tags) {
                    report.reject(issue);
                    return;
                }

                let node_refs = way.refs().collect::<Vec<_>>();
                if node_refs.is_empty() {
                    report.reject(CandidateIssue::WayWithoutResolvedNodes);
                    return;
                }

                for node_id in &node_refs {
                    required_node_ids.insert(*node_id);
                }
                way_stubs.push(AddressWayStub {
                    object_id: way.id(),
                    tags,
                    node_refs,
                });
            }
            Element::Relation(relation) => {
                report.scanned.relations += 1;
                if write_error.is_some() {
                    return;
                }
                let tags = collect_addr_tags(relation.tags());
                if !tags.is_empty() {
                    report.reject(CandidateIssue::UnsupportedRelation);
                }
            }
        })
        .with_context(|| format!("failed to parse {}", input.display()))?;
    progress.finish_with_message("1/3 discover address features complete");

    if let Some(error) = write_error {
        return Err(error);
    }

    node_records
        .flush()
        .with_context(|| format!("failed to flush {}", node_records_path.display()))?;

    Ok(DiscoveryResult {
        report,
        way_stubs,
        required_node_ids,
    })
}
