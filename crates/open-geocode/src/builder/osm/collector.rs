use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

use anyhow::{Context, Result};
use osmpbf::Element;

use crate::{
    builder::report::{BuilderReport, CandidateIssue},
    record::{LocationPrecision, OsmObjectType},
};

use super::{
    address::{
        AddressCandidate, collect_addr_tags_from_map, collect_clean_tags, validate_address_tags,
        write_candidate, write_rejected_record,
    },
    interpolation::{InterpolationWayStub, has_interpolation_tag},
    pbf::{element_reader_with_progress, input_bytes},
    place::{has_place_tag, write_place_node},
    postcode::PostcodeAccumulator,
    street::{StreetWayStub, has_highway_tag, missing_street_name_issue, street_name},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddressWayStub {
    pub object_id: i64,
    pub node_refs: Vec<i64>,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug)]
pub(crate) struct DiscoveryResult {
    pub report: BuilderReport,
    pub way_stubs: Vec<AddressWayStub>,
    pub interpolation_way_stubs: Vec<InterpolationWayStub>,
    pub street_way_stubs: Vec<StreetWayStub>,
    pub postcode_accumulator: PostcodeAccumulator,
    pub address_node_tags: HashMap<i64, BTreeMap<String, String>>,
    pub required_node_ids: HashSet<i64>,
}

pub(crate) fn discover_address_features(
    input: &Path,
    node_records_path: &Path,
    rejected_records_path: &Path,
) -> Result<DiscoveryResult> {
    let mut report = BuilderReport {
        schema_version: 5,
        input: input.display().to_string(),
        output: node_records_path.display().to_string(),
        input_bytes: input_bytes(input)?,
        ..BuilderReport::default()
    };
    let output_file = File::create(node_records_path)
        .with_context(|| format!("failed to create {}", node_records_path.display()))?;
    let mut node_records = BufWriter::new(output_file);
    let rejected_file = File::create(rejected_records_path)
        .with_context(|| format!("failed to create {}", rejected_records_path.display()))?;
    let mut rejected_records = BufWriter::new(rejected_file);
    let mut way_stubs = Vec::new();
    let mut interpolation_way_stubs = Vec::new();
    let mut street_way_stubs = Vec::new();
    let mut postcode_accumulator = PostcodeAccumulator::default();
    let mut address_node_tags = HashMap::new();
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
                let all_tags = collect_clean_tags(node.tags());
                if has_place_tag(&all_tags)
                    && let Err(error) = write_place_node(
                        node.id(),
                        node.lat(),
                        node.lon(),
                        &all_tags,
                        &mut node_records,
                        &mut report,
                    )
                {
                    write_error = Some(error);
                    return;
                }

                let tags = collect_addr_tags_from_map(&all_tags);
                if tags.is_empty() {
                    return;
                }
                address_node_tags.insert(node.id(), tags.clone());
                if has_interpolation_tag(&tags) {
                    report.reject_with_tags(
                        CandidateIssue::InterpolationUnsupportedObject,
                        OsmObjectType::Node,
                        &all_tags,
                        &tags,
                    );
                    if let Err(error) = write_rejected_record(
                        CandidateIssue::InterpolationUnsupportedObject,
                        OsmObjectType::Node,
                        node.id(),
                        &all_tags,
                        Some("interpolation"),
                        &mut rejected_records,
                    ) {
                        write_error = Some(error);
                    }
                    return;
                }
                if let Err(issue) = validate_address_tags(&tags) {
                    report.reject_with_tags(issue, OsmObjectType::Node, &all_tags, &tags);
                    if let Err(error) = write_rejected_record(
                        issue,
                        OsmObjectType::Node,
                        node.id(),
                        &all_tags,
                        Some("address"),
                        &mut rejected_records,
                    ) {
                        write_error = Some(error);
                    }
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
                match write_candidate(candidate, &mut node_records, &mut report) {
                    Ok(Some(record)) => postcode_accumulator.accept_address(&record),
                    Ok(None) => {}
                    Err(error) => write_error = Some(error),
                }
            }
            Element::Node(node) => {
                report.scanned.nodes += 1;
                if write_error.is_some() {
                    return;
                }
                let all_tags = collect_clean_tags(node.tags());
                if has_place_tag(&all_tags)
                    && let Err(error) = write_place_node(
                        node.id(),
                        node.lat(),
                        node.lon(),
                        &all_tags,
                        &mut node_records,
                        &mut report,
                    )
                {
                    write_error = Some(error);
                    return;
                }

                let tags = collect_addr_tags_from_map(&all_tags);
                if tags.is_empty() {
                    return;
                }
                address_node_tags.insert(node.id(), tags.clone());
                if has_interpolation_tag(&tags) {
                    report.reject_with_tags(
                        CandidateIssue::InterpolationUnsupportedObject,
                        OsmObjectType::Node,
                        &all_tags,
                        &tags,
                    );
                    if let Err(error) = write_rejected_record(
                        CandidateIssue::InterpolationUnsupportedObject,
                        OsmObjectType::Node,
                        node.id(),
                        &all_tags,
                        Some("interpolation"),
                        &mut rejected_records,
                    ) {
                        write_error = Some(error);
                    }
                    return;
                }
                if let Err(issue) = validate_address_tags(&tags) {
                    report.reject_with_tags(issue, OsmObjectType::Node, &all_tags, &tags);
                    if let Err(error) = write_rejected_record(
                        issue,
                        OsmObjectType::Node,
                        node.id(),
                        &all_tags,
                        Some("address"),
                        &mut rejected_records,
                    ) {
                        write_error = Some(error);
                    }
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
                match write_candidate(candidate, &mut node_records, &mut report) {
                    Ok(Some(record)) => postcode_accumulator.accept_address(&record),
                    Ok(None) => {}
                    Err(error) => write_error = Some(error),
                }
            }
            Element::Way(way) => {
                report.scanned.ways += 1;
                if write_error.is_some() {
                    return;
                }
                let all_tags = collect_clean_tags(way.tags());
                if has_highway_tag(&all_tags) {
                    if street_name(&all_tags).is_some() {
                        let node_refs = way.refs().collect::<Vec<_>>();
                        if node_refs.is_empty() {
                            report.reject_with_tags(
                                CandidateIssue::StreetUnresolvedGeometry,
                                OsmObjectType::Way,
                                &all_tags,
                                &BTreeMap::new(),
                            );
                            if let Err(error) = write_rejected_record(
                                CandidateIssue::StreetUnresolvedGeometry,
                                OsmObjectType::Way,
                                way.id(),
                                &all_tags,
                                Some("street"),
                                &mut rejected_records,
                            ) {
                                write_error = Some(error);
                            }
                        } else {
                            for node_id in &node_refs {
                                required_node_ids.insert(*node_id);
                            }
                            street_way_stubs.push(StreetWayStub {
                                object_id: way.id(),
                                node_refs,
                                tags: all_tags.clone(),
                            });
                        }
                    } else {
                        report.reject(missing_street_name_issue(&all_tags));
                    }
                }

                let tags = collect_addr_tags_from_map(&all_tags);
                if tags.is_empty() {
                    return;
                }

                if has_interpolation_tag(&tags) {
                    let node_refs = way.refs().collect::<Vec<_>>();
                    if node_refs.is_empty() {
                        report.reject_with_tags(
                            CandidateIssue::InterpolationWayWithoutNodes,
                            OsmObjectType::Way,
                            &all_tags,
                            &tags,
                        );
                        if let Err(error) = write_rejected_record(
                            CandidateIssue::InterpolationWayWithoutNodes,
                            OsmObjectType::Way,
                            way.id(),
                            &all_tags,
                            Some("interpolation"),
                            &mut rejected_records,
                        ) {
                            write_error = Some(error);
                        }
                        return;
                    }

                    for node_id in &node_refs {
                        required_node_ids.insert(*node_id);
                    }
                    interpolation_way_stubs.push(InterpolationWayStub {
                        object_id: way.id(),
                        node_refs,
                        tags: all_tags,
                    });
                    return;
                }

                if let Err(issue) = validate_address_tags(&tags) {
                    report.reject_with_tags(issue, OsmObjectType::Way, &all_tags, &tags);
                    if let Err(error) = write_rejected_record(
                        issue,
                        OsmObjectType::Way,
                        way.id(),
                        &all_tags,
                        Some("address"),
                        &mut rejected_records,
                    ) {
                        write_error = Some(error);
                    }
                    return;
                }

                let node_refs = way.refs().collect::<Vec<_>>();
                if node_refs.is_empty() {
                    report.reject(CandidateIssue::WayWithoutResolvedNodes);
                    if let Err(error) = write_rejected_record(
                        CandidateIssue::WayWithoutResolvedNodes,
                        OsmObjectType::Way,
                        way.id(),
                        &all_tags,
                        Some("address"),
                        &mut rejected_records,
                    ) {
                        write_error = Some(error);
                    }
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
                let all_tags = collect_clean_tags(relation.tags());
                let tags = collect_addr_tags_from_map(&all_tags);
                if !tags.is_empty() {
                    let (issue, layer_hint) = if has_interpolation_tag(&tags) {
                        (
                            CandidateIssue::InterpolationUnsupportedObject,
                            "interpolation",
                        )
                    } else {
                        (CandidateIssue::UnsupportedRelation, "address")
                    };
                    report.reject_with_tags(issue, OsmObjectType::Relation, &all_tags, &tags);
                    if let Err(error) = write_rejected_record(
                        issue,
                        OsmObjectType::Relation,
                        relation.id(),
                        &all_tags,
                        Some(layer_hint),
                        &mut rejected_records,
                    ) {
                        write_error = Some(error);
                    }
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
    rejected_records
        .flush()
        .with_context(|| format!("failed to flush {}", rejected_records_path.display()))?;

    Ok(DiscoveryResult {
        report,
        way_stubs,
        interpolation_way_stubs,
        street_way_stubs,
        postcode_accumulator,
        address_node_tags,
        required_node_ids,
    })
}
