use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use osmpbf::{Element, ElementReader};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct NormalizeOsmOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    pub report: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AddressRecord {
    pub schema_version: u32,
    pub record_id: String,
    pub kind: RecordKind,
    pub label: String,
    pub house_number: String,
    pub street: Option<String>,
    pub place: Option<String>,
    pub unit: Option<String>,
    pub city: Option<String>,
    pub postcode: Option<String>,
    pub state: Option<String>,
    pub country: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub location_precision: LocationPrecision,
    pub source: SourceProvenance,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    Address,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocationPrecision {
    Point,
    Centroid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceProvenance {
    pub dataset: String,
    pub object_type: OsmObjectType,
    pub object_id: i64,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OsmObjectType {
    Node,
    Way,
    Relation,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub schema_version: u32,
    pub input: String,
    pub output: String,
    pub scanned: ScannedCounts,
    pub accepted: AcceptedCounts,
    pub rejected: RejectedCounts,
    pub geometry_resolution: GeometryResolutionCounts,
    pub completeness: CompletenessCounts,
    pub node_cache_entries: usize,
    pub output_ndjson_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ScannedCounts {
    pub nodes: u64,
    pub dense_nodes: u64,
    pub ways: u64,
    pub relations: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AcceptedCounts {
    pub total: u64,
    pub node_addresses: u64,
    pub way_centroid_addresses: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RejectedCounts {
    pub total: u64,
    pub by_reason: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GeometryResolutionCounts {
    pub address_way_stubs: usize,
    pub required_node_refs: usize,
    pub resolved_node_refs: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CompletenessCounts {
    pub city: u64,
    pub postcode: u64,
    pub state: u64,
    pub country: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectReason {
    MissingHouseNumber,
    MissingStreetOrPlace,
    UnsupportedRelation,
    WayWithoutResolvedNodes,
}

impl RejectReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MissingHouseNumber => "missing_housenumber",
            Self::MissingStreetOrPlace => "missing_street_or_place",
            Self::UnsupportedRelation => "unsupported_relation",
            Self::WayWithoutResolvedNodes => "way_without_resolved_nodes",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct AddressCandidate {
    object_type: OsmObjectType,
    object_id: i64,
    lat: f64,
    lon: f64,
    location_precision: LocationPrecision,
    tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AddressWayStub {
    object_id: i64,
    node_refs: Vec<i64>,
    tags: BTreeMap<String, String>,
}

#[derive(Debug)]
struct DiscoveryResult {
    report: ImportReport,
    way_stubs: Vec<AddressWayStub>,
    required_node_ids: HashSet<i64>,
}

#[derive(Debug, Clone, Copy)]
struct LabelParts<'a> {
    house_number: &'a str,
    street: Option<&'a str>,
    place: Option<&'a str>,
    unit: Option<&'a str>,
    city: Option<&'a str>,
    state: Option<&'a str>,
    postcode: Option<&'a str>,
    country: Option<&'a str>,
}

pub fn normalize_osm(options: NormalizeOsmOptions) -> Result<()> {
    ensure_parent_dir(&options.output)?;
    ensure_parent_dir(&options.report)?;

    let node_records_path = temp_node_records_path(&options.output);
    let _ = fs::remove_file(&options.output);
    let _ = fs::remove_file(&node_records_path);

    let mut discovery = discover_address_features(&options.input, &node_records_path)?;
    discovery.report.output = options.output.display().to_string();
    discovery.report.geometry_resolution.address_way_stubs = discovery.way_stubs.len();
    discovery.report.geometry_resolution.required_node_refs = discovery.required_node_ids.len();

    let node_locations =
        resolve_required_node_locations(&options.input, &discovery.required_node_ids)?;
    discovery.report.geometry_resolution.resolved_node_refs = node_locations.len();
    discovery.report.node_cache_entries = node_locations.len();

    emit_normalized_records(
        &options.output,
        &node_records_path,
        &discovery.way_stubs,
        &node_locations,
        &mut discovery.report,
    )?;

    discovery.report.output_ndjson_bytes = fs::metadata(&options.output)
        .with_context(|| format!("failed to stat {}", options.output.display()))?
        .len();

    let report_file = File::create(&options.report)
        .with_context(|| format!("failed to create {}", options.report.display()))?;
    serde_json::to_writer_pretty(BufWriter::new(report_file), &discovery.report)
        .with_context(|| format!("failed to write {}", options.report.display()))?;

    let _ = fs::remove_file(node_records_path);

    Ok(())
}

fn discover_address_features(input: &Path, node_records_path: &Path) -> Result<DiscoveryResult> {
    let mut report = ImportReport {
        schema_version: 1,
        input: input.display().to_string(),
        output: node_records_path.display().to_string(),
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

                if let Err(reason) = validate_address_tags(&tags) {
                    report.reject(reason);
                    return;
                }

                let node_refs = way.refs().collect::<Vec<_>>();
                if node_refs.is_empty() {
                    report.reject(RejectReason::WayWithoutResolvedNodes);
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
                    report.reject(RejectReason::UnsupportedRelation);
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

fn resolve_required_node_locations(
    input: &Path,
    required_node_ids: &HashSet<i64>,
) -> Result<HashMap<i64, (f64, f64)>> {
    let mut node_locations = HashMap::with_capacity(required_node_ids.len());
    let (reader, progress) = element_reader_with_progress(input, "2/3 resolve required nodes")?;

    reader
        .for_each(|element| match element {
            Element::DenseNode(node) => {
                if required_node_ids.contains(&node.id()) {
                    node_locations.insert(node.id(), (node.lat(), node.lon()));
                }
            }
            Element::Node(node) => {
                if required_node_ids.contains(&node.id()) {
                    node_locations.insert(node.id(), (node.lat(), node.lon()));
                }
            }
            Element::Way(_) | Element::Relation(_) => {}
        })
        .with_context(|| format!("failed to resolve node locations from {}", input.display()))?;
    progress.finish_with_message("2/3 resolve required nodes complete");

    Ok(node_locations)
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
            report.reject(RejectReason::WayWithoutResolvedNodes);
            progress.inc(1);
            continue;
        };

        let Some((lat, lon)) = centroid(&points) else {
            report.reject(RejectReason::WayWithoutResolvedNodes);
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

fn resolve_way_points(
    stub: &AddressWayStub,
    node_locations: &HashMap<i64, (f64, f64)>,
) -> Option<Vec<(f64, f64)>> {
    let mut points = Vec::with_capacity(stub.node_refs.len());
    for node_id in &stub.node_refs {
        points.push(*node_locations.get(node_id)?);
    }
    Some(points)
}

fn validate_address_tags(tags: &BTreeMap<String, String>) -> std::result::Result<(), RejectReason> {
    if tag_value(tags, "addr:housenumber").is_none() {
        return Err(RejectReason::MissingHouseNumber);
    }
    if tag_value(tags, "addr:street").is_none() && tag_value(tags, "addr:place").is_none() {
        return Err(RejectReason::MissingStreetOrPlace);
    }
    Ok(())
}

fn element_reader_with_progress(
    input: &Path,
    message: &'static str,
) -> Result<(ElementReader<BufReader<ProgressReader<File>>>, ProgressBar)> {
    let input_bytes = fs::metadata(input)
        .with_context(|| format!("failed to stat {}", input.display()))?
        .len();
    let progress = byte_progress_bar(input_bytes, message);
    let file = File::open(input).with_context(|| format!("failed to open {}", input.display()))?;
    let reader = ElementReader::new(BufReader::new(ProgressReader {
        inner: file,
        progress: progress.clone(),
    }));
    Ok((reader, progress))
}

fn byte_progress_bar(len: u64, message: &'static str) -> ProgressBar {
    let progress = ProgressBar::new(len);
    progress.set_style(
        ProgressStyle::with_template(
            "{msg:32} [{bar:40.cyan/blue}] {percent:>3}% {bytes}/{total_bytes} {bytes_per_sec} elapsed {elapsed_precise}",
        )
        .expect("valid byte progress template")
        .progress_chars("=> "),
    );
    progress.set_message(message);
    progress
}

fn item_progress_bar(len: u64, message: &'static str) -> ProgressBar {
    let progress = ProgressBar::new(len);
    progress.set_style(
        ProgressStyle::with_template(
            "{msg:32} [{bar:40.cyan/blue}] {percent:>3}% {pos}/{len} elapsed {elapsed_precise}",
        )
        .expect("valid item progress template")
        .progress_chars("=> "),
    );
    progress.set_message(message);
    progress
}

struct ProgressReader<R> {
    inner: R,
    progress: ProgressBar,
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let bytes_read = self.inner.read(buffer)?;
        self.progress.inc(bytes_read as u64);
        Ok(bytes_read)
    }
}

fn write_candidate<W: Write>(
    candidate: AddressCandidate,
    output: &mut W,
    report: &mut ImportReport,
) -> Result<()> {
    match address_record_from_candidate(candidate) {
        Ok(record) => {
            serde_json::to_writer(&mut *output, &record)?;
            writeln!(output)?;
            report.accept(&record);
        }
        Err(reason) => report.reject(reason),
    }
    Ok(())
}

fn address_record_from_candidate(
    candidate: AddressCandidate,
) -> std::result::Result<AddressRecord, RejectReason> {
    let house_number =
        tag_value(&candidate.tags, "addr:housenumber").ok_or(RejectReason::MissingHouseNumber)?;
    let street = tag_value(&candidate.tags, "addr:street");
    let place = tag_value(&candidate.tags, "addr:place");

    if street.is_none() && place.is_none() {
        return Err(RejectReason::MissingStreetOrPlace);
    }

    let unit = tag_value(&candidate.tags, "addr:unit");
    let city = tag_value(&candidate.tags, "addr:city");
    let postcode = tag_value(&candidate.tags, "addr:postcode");
    let state = tag_value(&candidate.tags, "addr:state");
    let country = tag_value(&candidate.tags, "addr:country");
    let label = build_label(LabelParts {
        house_number: &house_number,
        street: street.as_deref(),
        place: place.as_deref(),
        unit: unit.as_deref(),
        city: city.as_deref(),
        state: state.as_deref(),
        postcode: postcode.as_deref(),
        country: country.as_deref(),
    });

    Ok(AddressRecord {
        schema_version: 1,
        record_id: format!(
            "osm:{}:{}",
            osm_object_type_name(candidate.object_type),
            candidate.object_id
        ),
        kind: RecordKind::Address,
        label,
        house_number,
        street,
        place,
        unit,
        city,
        postcode,
        state,
        country,
        lat: candidate.lat,
        lon: candidate.lon,
        location_precision: candidate.location_precision,
        source: SourceProvenance {
            dataset: "osm".to_string(),
            object_type: candidate.object_type,
            object_id: candidate.object_id,
            tags: candidate.tags,
        },
    })
}

fn collect_addr_tags<'a>(
    tags: impl Iterator<Item = (&'a str, &'a str)>,
) -> BTreeMap<String, String> {
    tags.filter_map(|(key, value)| {
        if !key.starts_with("addr:") {
            return None;
        }
        let value = clean_text(value)?;
        Some((key.to_string(), value))
    })
    .collect()
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

fn build_label(parts: LabelParts<'_>) -> String {
    let primary = [Some(parts.house_number), parts.street.or(parts.place)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");

    [
        Some(primary.as_str()),
        parts.unit,
        parts.city,
        parts.state,
        parts.postcode,
        parts.country,
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(", ")
}

fn centroid(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    if points.is_empty() {
        return None;
    }

    if points.len() >= 4 && points.first() == points.last() {
        polygon_centroid(points).or_else(|| average_point(points))
    } else {
        average_point(points)
    }
}

fn average_point(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let count = points.len() as f64;
    let lat = points.iter().map(|(lat, _)| lat).sum::<f64>() / count;
    let lon = points.iter().map(|(_, lon)| lon).sum::<f64>() / count;
    Some((lat, lon))
}

fn polygon_centroid(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let mut signed_area = 0.0;
    let mut centroid_lon = 0.0;
    let mut centroid_lat = 0.0;

    for window in points.windows(2) {
        let (lat_a, lon_a) = window[0];
        let (lat_b, lon_b) = window[1];
        let cross = lon_a.mul_add(lat_b, -(lon_b * lat_a));
        signed_area += cross;
        centroid_lon += (lon_a + lon_b) * cross;
        centroid_lat += (lat_a + lat_b) * cross;
    }

    if signed_area.abs() < f64::EPSILON {
        return None;
    }

    let signed_area = signed_area * 0.5;
    Some((
        centroid_lat / (6.0 * signed_area),
        centroid_lon / (6.0 * signed_area),
    ))
}

fn osm_object_type_name(object_type: OsmObjectType) -> &'static str {
    match object_type {
        OsmObjectType::Node => "node",
        OsmObjectType::Way => "way",
        OsmObjectType::Relation => "relation",
    }
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

impl ImportReport {
    fn accept(&mut self, record: &AddressRecord) {
        self.accepted.total += 1;
        match record.location_precision {
            LocationPrecision::Point => self.accepted.node_addresses += 1,
            LocationPrecision::Centroid => self.accepted.way_centroid_addresses += 1,
        }

        if record.city.is_some() {
            self.completeness.city += 1;
        }
        if record.postcode.is_some() {
            self.completeness.postcode += 1;
        }
        if record.state.is_some() {
            self.completeness.state += 1;
        }
        if record.country.is_some() {
            self.completeness.country += 1;
        }
    }

    fn reject(&mut self, reason: RejectReason) {
        self.rejected.total += 1;
        *self
            .rejected
            .by_reason
            .entry(reason.as_str().to_string())
            .or_default() += 1;
    }
}

#[cfg(test)]
mod tests {
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

        assert_eq!(record.record_id, "osm:node:42");
        assert_eq!(record.kind, RecordKind::Address);
        assert_eq!(record.label, "10 King Street, Toronto, ON, CA");
        assert_eq!(record.street.as_deref(), Some("King Street"));
        assert_eq!(record.source.object_type, OsmObjectType::Node);
        assert_eq!(record.source.object_id, 42);
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
            Err(RejectReason::MissingHouseNumber)
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
            Err(RejectReason::MissingStreetOrPlace)
        );
    }

    #[test]
    fn computes_closed_way_centroid() {
        let points = [(0.0, 0.0), (0.0, 2.0), (2.0, 2.0), (2.0, 0.0), (0.0, 0.0)];

        let (lat, lon) = centroid(&points).expect("centroid");

        assert!((lat - 1.0).abs() < 0.000001);
        assert!((lon - 1.0).abs() < 0.000001);
    }

    #[test]
    fn collects_only_non_empty_addr_tags() {
        let tags = [
            ("addr:housenumber", " 10 "),
            ("name", "Not an address tag"),
            ("addr:street", " King   Street "),
            ("addr:unit", "   "),
        ];

        let collected = collect_addr_tags(tags.into_iter());

        assert_eq!(
            collected,
            BTreeMap::from([
                ("addr:housenumber".to_string(), "10".to_string()),
                ("addr:street".to_string(), "King Street".to_string())
            ])
        );
    }
}
