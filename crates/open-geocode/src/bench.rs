use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    pack::{PackManifest, PackReader},
    reverse::{PackReverseGeocoder, ReverseGeocodeOptions},
    search::{PackTextSearcher, TextAutocompleteOptions, TextSearchOptions},
};

#[derive(Debug, Clone)]
pub struct PackBenchmarkOptions {
    pub pack: PathBuf,
    pub queries: Option<PathBuf>,
    pub iterations: usize,
    pub warmup: usize,
}

#[derive(Debug, Serialize)]
pub struct PackBenchmarkReport {
    pub settings: PackBenchmarkSettings,
    pub pack: PackMetricReport,
    pub open: OpenBenchmarkReport,
    pub queries: QueryBenchmarkReport,
}

#[derive(Debug, Serialize)]
pub struct PackBenchmarkSettings {
    pub pack: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queries: Option<String>,
    pub iterations: usize,
    pub warmup: usize,
}

#[derive(Debug, Serialize)]
pub struct PackMetricReport {
    pub manifest: PackManifestSummary,
    pub bytes: PackByteMetrics,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildMetricReport>,
}

#[derive(Debug, Serialize)]
pub struct BuildMetricReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_records: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_records: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_mib_per_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accepted_records_per_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejected_records_per_sec: Option<f64>,
    pub phases: BuildPhaseMetrics,
    pub record_store: RecordStoreBuildMetrics,
    pub text_index: TextIndexBuildMetrics,
    pub spatial_index: SpatialIndexBuildMetrics,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_prefix: Option<TextPrefixMetricSummary>,
    pub osm_scan: OsmScanMetrics,
    pub geometry_resolution: GeometryResolutionMetrics,
}

#[derive(Debug, Default, Serialize)]
pub struct BuildPhaseMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pack_create_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub osm_feature_scan_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_coordinate_resolution_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_emission_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pack_finalize_ms: Option<u128>,
}

#[derive(Debug, Default, Serialize)]
pub struct RecordStoreBuildMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_encode_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_table_write_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection_encode_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rejection_table_write_ms: Option<u128>,
}

#[derive(Debug, Default, Serialize)]
pub struct TextIndexBuildMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projection_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix_generation_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tantivy_document_build_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tantivy_add_document_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_ms: Option<u128>,
}

#[derive(Debug, Default, Serialize)]
pub struct SpatialIndexBuildMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub segment_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub add_record_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finalize_ms: Option<u128>,
}

#[derive(Debug, Default, Serialize)]
pub struct TextPrefixMetricSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terms_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terms_avg_per_record: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terms_p95_per_record: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terms_max_per_record: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terms_cap_hit_count: Option<u64>,
    pub terms_by_field: BTreeMap<String, u64>,
}

#[derive(Debug, Default, Serialize)]
pub struct OsmScanMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dense_nodes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nodes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ways: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relations: Option<u64>,
}

#[derive(Debug, Default, Serialize)]
pub struct GeometryResolutionMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address_way_stubs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interpolation_way_stubs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub street_way_stubs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_node_refs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_node_refs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct PackManifestSummary {
    pub schema_version: u32,
    pub crate_version: String,
    pub built_at_unix: u64,
    pub record_count: u64,
    pub rejection_count: u64,
    pub layer_counts: BTreeMap<String, u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_index_schema_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_index_document_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spatial_index_schema_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spatial_index_point_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spatial_index_segment_count: Option<u64>,
}

#[derive(Debug, Default, Serialize)]
pub struct PackByteMetrics {
    pub total: u64,
    pub manifest: u64,
    pub records: u64,
    pub offsets: u64,
    pub record_store: u64,
    pub audit: u64,
    pub text_index: u64,
    pub spatial_index: u64,
    pub other: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_per_record: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_store_bytes_per_record: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_index_bytes_per_record: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spatial_index_bytes_per_record: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackFileMetric {
    pub path: String,
    pub bytes: u64,
    pub category: PackFileCategory,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PackFileCategory {
    Manifest,
    Records,
    Offsets,
    Audit,
    TextIndex,
    SpatialIndex,
    Other,
}

#[derive(Debug, Serialize)]
pub struct OpenBenchmarkReport {
    pub pack_reader_ms: f64,
    pub text_searcher_ms: f64,
    pub reverse_geocoder_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct QueryBenchmarkReport {
    pub search: OperationBenchmarkReport<TextQueryCaseReport>,
    pub autocomplete: OperationBenchmarkReport<TextQueryCaseReport>,
    pub reverse: OperationBenchmarkReport<ReverseQueryCaseReport>,
}

#[derive(Debug, Serialize)]
pub struct OperationBenchmarkReport<T> {
    pub case_count: usize,
    pub measured_runs: usize,
    pub warmup_runs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency: Option<LatencyStats>,
    pub cases: Vec<T>,
}

#[derive(Debug, Serialize)]
pub struct TextQueryCaseReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub query: String,
    pub limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    pub hit_count: usize,
    pub latency: LatencyStats,
}

#[derive(Debug, Serialize)]
pub struct ReverseQueryCaseReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub lon: f64,
    pub lat: f64,
    pub result_present: bool,
    pub latency: LatencyStats,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LatencyStats {
    pub min_ms: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub mean_ms: f64,
    pub total_ms: f64,
}

#[derive(Debug, Default, Deserialize)]
struct BenchmarkFixture {
    #[serde(default)]
    search: Vec<TextQueryFixture>,
    #[serde(default)]
    autocomplete: Vec<TextQueryFixture>,
    #[serde(default)]
    reverse: Vec<ReverseQueryFixture>,
}

#[derive(Debug, Clone, Deserialize)]
struct TextQueryFixture {
    #[serde(default)]
    name: Option<String>,
    #[serde(alias = "q")]
    query: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    layer: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReverseQueryFixture {
    #[serde(default)]
    name: Option<String>,
    lon: f64,
    lat: f64,
}

pub fn benchmark_pack(options: PackBenchmarkOptions) -> Result<PackBenchmarkReport> {
    let iterations = options.iterations.max(1);
    let warmup = options.warmup;
    let fixture = read_fixture(options.queries.as_deref())?;

    let (reader, pack_reader_ms) = measure_value(|| PackReader::open(&options.pack))?;
    let pack = pack_metrics(&options.pack, reader.manifest())?;
    let (searcher, text_searcher_ms) = measure_value(|| PackTextSearcher::open(&options.pack))?;
    let (reverse_geocoder, reverse_geocoder_ms) =
        measure_value(|| PackReverseGeocoder::open(&options.pack))?;

    let queries = QueryBenchmarkReport {
        search: benchmark_search_cases(&searcher, &fixture.search, iterations, warmup)?,
        autocomplete: benchmark_autocomplete_cases(
            &searcher,
            &fixture.autocomplete,
            iterations,
            warmup,
        )?,
        reverse: benchmark_reverse_cases(&reverse_geocoder, &fixture.reverse, iterations, warmup)?,
    };

    Ok(PackBenchmarkReport {
        settings: PackBenchmarkSettings {
            pack: options.pack.display().to_string(),
            queries: options
                .queries
                .as_ref()
                .map(|path| path.display().to_string()),
            iterations,
            warmup,
        },
        pack,
        open: OpenBenchmarkReport {
            pack_reader_ms,
            text_searcher_ms,
            reverse_geocoder_ms,
        },
        queries,
    })
}

fn read_fixture(path: Option<&Path>) -> Result<BenchmarkFixture> {
    let Some(path) = path else {
        return Ok(BenchmarkFixture::default());
    };
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::from_reader(file).with_context(|| format!("failed to parse {}", path.display()))
}

fn pack_metrics(pack_path: &Path, manifest: &PackManifest) -> Result<PackMetricReport> {
    let files = collect_file_metrics(pack_path)?;
    let bytes = byte_metrics(&files, manifest.record_count);
    let build_report = read_build_report(pack_path)?;
    let build = build_report.as_ref().map(build_metrics);
    Ok(PackMetricReport {
        manifest: PackManifestSummary::from_manifest(manifest),
        bytes,
        build,
    })
}

fn collect_file_metrics(pack_path: &Path) -> Result<Vec<PackFileMetric>> {
    let mut files = Vec::new();
    collect_file_metrics_under(pack_path, pack_path, &mut files)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn collect_file_metrics_under(
    pack_path: &Path,
    current: &Path,
    files: &mut Vec<PackFileMetric>,
) -> Result<()> {
    for entry in
        fs::read_dir(current).with_context(|| format!("failed to read {}", current.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_file_metrics_under(pack_path, &path, files)?;
        } else if metadata.is_file() {
            let relative = pack_relative_path(pack_path, &path)?;
            files.push(PackFileMetric {
                category: categorize_pack_file(&relative),
                path: relative,
                bytes: metadata.len(),
            });
        }
    }
    Ok(())
}

fn pack_relative_path(pack_path: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(pack_path)
        .with_context(|| format!("{} is not inside {}", path.display(), pack_path.display()))?;
    Ok(relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

fn categorize_pack_file(path: &str) -> PackFileCategory {
    if path == "manifest.json" {
        PackFileCategory::Manifest
    } else if path == "records/records.bin" {
        PackFileCategory::Records
    } else if path == "records/offsets.bin" {
        PackFileCategory::Offsets
    } else if path.starts_with("audit/") {
        PackFileCategory::Audit
    } else if path.starts_with("text/") {
        PackFileCategory::TextIndex
    } else if path.starts_with("spatial/") {
        PackFileCategory::SpatialIndex
    } else {
        PackFileCategory::Other
    }
}

fn byte_metrics(files: &[PackFileMetric], record_count: u64) -> PackByteMetrics {
    let mut metrics = PackByteMetrics::default();
    for file in files {
        metrics.total += file.bytes;
        match file.category {
            PackFileCategory::Manifest => metrics.manifest += file.bytes,
            PackFileCategory::Records => metrics.records += file.bytes,
            PackFileCategory::Offsets => metrics.offsets += file.bytes,
            PackFileCategory::Audit => metrics.audit += file.bytes,
            PackFileCategory::TextIndex => metrics.text_index += file.bytes,
            PackFileCategory::SpatialIndex => metrics.spatial_index += file.bytes,
            PackFileCategory::Other => metrics.other += file.bytes,
        }
    }
    metrics.record_store = metrics.records + metrics.offsets;
    if record_count > 0 {
        let record_count = record_count as f64;
        metrics.bytes_per_record = Some(metrics.total as f64 / record_count);
        metrics.record_store_bytes_per_record = Some(metrics.record_store as f64 / record_count);
        metrics.text_index_bytes_per_record = Some(metrics.text_index as f64 / record_count);
        metrics.spatial_index_bytes_per_record = Some(metrics.spatial_index as f64 / record_count);
    }
    metrics
}

fn read_build_report(pack_path: &Path) -> Result<Option<Value>> {
    let path = pack_path.join("audit").join("build-report.json");
    if !path.exists() {
        return Ok(None);
    }
    let file =
        fs::File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::from_reader(file)
        .map(Some)
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn build_metrics(report: &Value) -> BuildMetricReport {
    let schema_version = value_at_u64(report, &["schema_version"]);
    let input_bytes = value_at_u64(report, &["input_bytes"]);
    let accepted_records = value_at_u64(report, &["accepted", "total"]);
    let rejected_records = value_at_u64(report, &["rejected", "total"]);
    let total_ms = value_at_u128(report, &["phases", "total_ms"]);
    let total_seconds = total_ms.map(|ms| ms as f64 / 1_000.0);

    BuildMetricReport {
        schema_version,
        input_bytes,
        accepted_records,
        rejected_records,
        total_ms,
        total_seconds,
        input_mib_per_sec: rate_mib_per_sec(input_bytes, total_ms),
        accepted_records_per_sec: rate_per_sec(accepted_records, total_ms),
        rejected_records_per_sec: rate_per_sec(rejected_records, total_ms),
        phases: BuildPhaseMetrics {
            pack_create_ms: value_at_u128(report, &["phases", "pack_create_ms"]),
            osm_feature_scan_ms: value_at_u128(report, &["phases", "discovery_ms"]),
            node_coordinate_resolution_ms: value_at_u128(
                report,
                &["phases", "coordinate_resolution_ms"],
            ),
            record_emission_ms: value_at_u128(report, &["phases", "record_emission_ms"]),
            pack_finalize_ms: value_at_u128(report, &["phases", "pack_finish_ms"]),
        },
        record_store: RecordStoreBuildMetrics {
            record_encode_ms: value_at_u128(report, &["pack_write", "record_encode_ms"]),
            record_table_write_ms: value_at_u128(report, &["pack_write", "record_table_write_ms"]),
            rejection_encode_ms: value_at_u128(report, &["pack_write", "rejection_encode_ms"]),
            rejection_table_write_ms: value_at_u128(
                report,
                &["pack_write", "rejection_table_write_ms"],
            ),
        },
        text_index: TextIndexBuildMetrics {
            schema_version: value_at_u64(report, &["text_index_schema_version"]),
            document_count: value_at_u64(report, &["text_index_document_count"]),
            bytes: value_at_u64(report, &["text_index_bytes"]),
            write_ms: value_at_u128(report, &["pack_write", "text_index_write_ms"]),
            projection_ms: value_at_u128(report, &["pack_write", "text_projection_ms"]),
            prefix_generation_ms: value_at_u128(
                report,
                &["pack_write", "text_prefix_generation_ms"],
            ),
            tantivy_document_build_ms: value_at_u128(
                report,
                &["pack_write", "tantivy_document_build_ms"],
            ),
            tantivy_add_document_ms: value_at_u128(
                report,
                &["pack_write", "tantivy_add_document_ms"],
            ),
            commit_ms: value_at_u128(report, &["pack_write", "text_index_commit_ms"]),
        },
        spatial_index: SpatialIndexBuildMetrics {
            schema_version: value_at_u64(report, &["spatial_index_schema_version"]),
            point_count: value_at_u64(report, &["spatial_index_point_count"]),
            segment_count: value_at_u64(report, &["spatial_index_segment_count"]),
            bytes: value_at_u64(report, &["spatial_index_bytes"]),
            add_record_ms: value_at_u128(report, &["pack_write", "spatial_index_write_ms"]),
            finalize_ms: value_at_u128(report, &["pack_write", "spatial_index_finish_ms"]),
        },
        text_prefix: text_prefix_metrics(report),
        osm_scan: OsmScanMetrics {
            dense_nodes: value_at_u64(report, &["scanned", "dense_nodes"]),
            nodes: value_at_u64(report, &["scanned", "nodes"]),
            ways: value_at_u64(report, &["scanned", "ways"]),
            relations: value_at_u64(report, &["scanned", "relations"]),
        },
        geometry_resolution: GeometryResolutionMetrics {
            address_way_stubs: value_at_u64(report, &["geometry_resolution", "address_way_stubs"]),
            interpolation_way_stubs: value_at_u64(
                report,
                &["geometry_resolution", "interpolation_way_stubs"],
            ),
            street_way_stubs: value_at_u64(report, &["geometry_resolution", "street_way_stubs"]),
            required_node_refs: value_at_u64(
                report,
                &["geometry_resolution", "required_node_refs"],
            ),
            resolved_node_refs: value_at_u64(
                report,
                &["geometry_resolution", "resolved_node_refs"],
            ),
        },
    }
}

fn text_prefix_metrics(report: &Value) -> Option<TextPrefixMetricSummary> {
    let prefix = report.get("text_index_prefix")?;
    Some(TextPrefixMetricSummary {
        terms_total: value_at_u64(prefix, &["autocomplete_prefix_terms_total"]),
        terms_avg_per_record: value_at_f64(prefix, &["autocomplete_prefix_terms_avg_per_record"]),
        terms_p95_per_record: value_at_u64(prefix, &["autocomplete_prefix_terms_p95_per_record"]),
        terms_max_per_record: value_at_u64(prefix, &["autocomplete_prefix_terms_max_per_record"]),
        terms_cap_hit_count: value_at_u64(prefix, &["autocomplete_prefix_terms_cap_hit_count"]),
        terms_by_field: object_u64_map(prefix.get("autocomplete_prefix_terms_by_field")),
    })
}

fn value_at_u64(value: &Value, path: &[&str]) -> Option<u64> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_u64()
}

fn value_at_u128(value: &Value, path: &[&str]) -> Option<u128> {
    value_at_u64(value, path).map(u128::from)
}

fn value_at_f64(value: &Value, path: &[&str]) -> Option<f64> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_f64()
}

fn object_u64_map(value: Option<&Value>) -> BTreeMap<String, u64> {
    let Some(object) = value.and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    object
        .iter()
        .filter_map(|(key, value)| value.as_u64().map(|number| (key.clone(), number)))
        .collect()
}

fn rate_per_sec(count: Option<u64>, total_ms: Option<u128>) -> Option<f64> {
    let total_ms = total_ms?;
    if total_ms == 0 {
        return None;
    }
    Some(count? as f64 / (total_ms as f64 / 1_000.0))
}

fn rate_mib_per_sec(bytes: Option<u64>, total_ms: Option<u128>) -> Option<f64> {
    let total_ms = total_ms?;
    if total_ms == 0 {
        return None;
    }
    Some(bytes? as f64 / 1_048_576.0 / (total_ms as f64 / 1_000.0))
}

fn benchmark_search_cases(
    searcher: &PackTextSearcher,
    cases: &[TextQueryFixture],
    iterations: usize,
    warmup: usize,
) -> Result<OperationBenchmarkReport<TextQueryCaseReport>> {
    let mut reports = Vec::new();
    let mut all_durations = Vec::new();
    for case in cases {
        let limit = case.limit.unwrap_or(10);
        let mut hit_count = 0;
        let durations = measure_iterations(iterations, warmup, || {
            let hits = searcher.search(TextSearchOptions {
                query: case.query.clone(),
                limit,
                layer: case.layer.clone(),
            })?;
            hit_count = hits.len();
            Ok(())
        })?;
        all_durations.extend(durations.iter().copied());
        reports.push(TextQueryCaseReport {
            name: case.name.clone(),
            query: case.query.clone(),
            limit,
            layer: case.layer.clone(),
            hit_count,
            latency: LatencyStats::from_nanos(&durations),
        });
    }

    Ok(operation_report(
        cases.len(),
        iterations,
        warmup,
        reports,
        &all_durations,
    ))
}

fn benchmark_autocomplete_cases(
    searcher: &PackTextSearcher,
    cases: &[TextQueryFixture],
    iterations: usize,
    warmup: usize,
) -> Result<OperationBenchmarkReport<TextQueryCaseReport>> {
    let mut reports = Vec::new();
    let mut all_durations = Vec::new();
    for case in cases {
        let limit = case.limit.unwrap_or(10);
        let mut hit_count = 0;
        let durations = measure_iterations(iterations, warmup, || {
            let hits = searcher.autocomplete(TextAutocompleteOptions {
                query: case.query.clone(),
                limit,
                layer: case.layer.clone(),
            })?;
            hit_count = hits.len();
            Ok(())
        })?;
        all_durations.extend(durations.iter().copied());
        reports.push(TextQueryCaseReport {
            name: case.name.clone(),
            query: case.query.clone(),
            limit,
            layer: case.layer.clone(),
            hit_count,
            latency: LatencyStats::from_nanos(&durations),
        });
    }

    Ok(operation_report(
        cases.len(),
        iterations,
        warmup,
        reports,
        &all_durations,
    ))
}

fn benchmark_reverse_cases(
    geocoder: &PackReverseGeocoder,
    cases: &[ReverseQueryFixture],
    iterations: usize,
    warmup: usize,
) -> Result<OperationBenchmarkReport<ReverseQueryCaseReport>> {
    let mut reports = Vec::new();
    let mut all_durations = Vec::new();
    for case in cases {
        let mut result_present = false;
        let durations = measure_iterations(iterations, warmup, || {
            let response = geocoder.reverse(ReverseGeocodeOptions {
                lon: case.lon,
                lat: case.lat,
            })?;
            result_present = response.result.is_some();
            Ok(())
        })?;
        all_durations.extend(durations.iter().copied());
        reports.push(ReverseQueryCaseReport {
            name: case.name.clone(),
            lon: case.lon,
            lat: case.lat,
            result_present,
            latency: LatencyStats::from_nanos(&durations),
        });
    }

    Ok(operation_report(
        cases.len(),
        iterations,
        warmup,
        reports,
        &all_durations,
    ))
}

fn operation_report<T>(
    case_count: usize,
    iterations: usize,
    warmup: usize,
    cases: Vec<T>,
    durations: &[u128],
) -> OperationBenchmarkReport<T> {
    OperationBenchmarkReport {
        case_count,
        measured_runs: case_count * iterations,
        warmup_runs: case_count * warmup,
        latency: (!durations.is_empty()).then(|| LatencyStats::from_nanos(durations)),
        cases,
    }
}

fn measure_value<T>(measure: impl FnOnce() -> Result<T>) -> Result<(T, f64)> {
    let started = Instant::now();
    let value = measure()?;
    Ok((value, nanos_to_ms(started.elapsed().as_nanos())))
}

fn measure_iterations(
    iterations: usize,
    warmup: usize,
    mut measure: impl FnMut() -> Result<()>,
) -> Result<Vec<u128>> {
    for _ in 0..warmup {
        measure()?;
    }

    let mut durations = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        measure()?;
        durations.push(started.elapsed().as_nanos());
    }
    Ok(durations)
}

impl PackManifestSummary {
    fn from_manifest(manifest: &PackManifest) -> Self {
        Self {
            schema_version: manifest.schema_version,
            crate_version: manifest.crate_version.clone(),
            built_at_unix: manifest.built_at_unix,
            record_count: manifest.record_count,
            rejection_count: manifest.rejection_count,
            layer_counts: manifest.layer_counts.clone(),
            text_index_schema_version: manifest
                .text_index
                .as_ref()
                .map(|index| index.schema_version),
            text_index_document_count: manifest
                .text_index
                .as_ref()
                .map(|index| index.document_count),
            spatial_index_schema_version: manifest
                .spatial_index
                .as_ref()
                .map(|index| index.schema_version),
            spatial_index_point_count: manifest
                .spatial_index
                .as_ref()
                .map(|index| index.point_count),
            spatial_index_segment_count: manifest
                .spatial_index
                .as_ref()
                .map(|index| index.segment_count),
        }
    }
}

impl LatencyStats {
    fn from_nanos(durations: &[u128]) -> Self {
        debug_assert!(!durations.is_empty());
        let mut sorted = durations.to_vec();
        sorted.sort_unstable();
        let total = sorted.iter().sum::<u128>();
        let mean = total as f64 / sorted.len() as f64;
        Self {
            min_ms: nanos_to_ms(*sorted.first().expect("duration")),
            p50_ms: nanos_to_ms(percentile(&sorted, 50.0)),
            p90_ms: nanos_to_ms(percentile(&sorted, 90.0)),
            p95_ms: nanos_to_ms(percentile(&sorted, 95.0)),
            p99_ms: nanos_to_ms(percentile(&sorted, 99.0)),
            max_ms: nanos_to_ms(*sorted.last().expect("duration")),
            mean_ms: mean / 1_000_000.0,
            total_ms: nanos_to_ms(total),
        }
    }
}

fn percentile(sorted: &[u128], percentile: f64) -> u128 {
    let rank = ((percentile / 100.0) * sorted.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

fn nanos_to_ms(nanos: u128) -> f64 {
    nanos as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use crate::{
        builder::report::BuilderReport,
        pack::{PackWriter, RecordWriter},
        record::{
            AddressComponents, AddressRecord, LocationPrecision, NormalizedRecord, OsmObjectType,
            SourceProvenance, point_geometry,
        },
    };

    use super::*;

    #[test]
    fn reports_pack_metrics_without_query_fixture() {
        let temp_dir = temp_pack_path("bench-metrics");
        let _ = fs::remove_dir_all(&temp_dir);
        write_test_pack(&temp_dir);

        let report = benchmark_pack(PackBenchmarkOptions {
            pack: temp_dir.clone(),
            queries: None,
            iterations: 2,
            warmup: 1,
        })
        .expect("benchmark");

        assert_eq!(report.pack.manifest.record_count, 1);
        assert!(report.pack.bytes.total > 0);
        assert!(report.pack.bytes.record_store > 0);
        let build = report.pack.build.as_ref().expect("build metrics");
        assert!(build.phases.pack_finalize_ms.is_some());
        assert!(build.text_index.commit_ms.is_some());
        assert!(report.open.pack_reader_ms >= 0.0);
        assert_eq!(report.queries.search.case_count, 0);

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn benchmarks_query_fixture_cases() {
        let temp_dir = temp_pack_path("bench-queries");
        let _ = fs::remove_dir_all(&temp_dir);
        write_test_pack(&temp_dir);

        let fixture_path = temp_dir.join("queries.json");
        fs::write(
            &fixture_path,
            r#"{
              "search": [{"name": "king", "q": "King Street Toronto", "limit": 5}],
              "autocomplete": [{"name": "prefix", "q": "kin", "limit": 5}],
              "reverse": [{"name": "point", "lon": -79.4, "lat": 43.6}]
            }"#,
        )
        .expect("write fixture");

        let report = benchmark_pack(PackBenchmarkOptions {
            pack: temp_dir.clone(),
            queries: Some(fixture_path),
            iterations: 2,
            warmup: 1,
        })
        .expect("benchmark");

        assert_eq!(report.queries.search.case_count, 1);
        assert_eq!(report.queries.search.measured_runs, 2);
        assert_eq!(report.queries.search.warmup_runs, 1);
        assert_eq!(report.queries.search.cases[0].hit_count, 1);
        assert_eq!(report.queries.autocomplete.cases[0].hit_count, 1);
        assert!(report.queries.reverse.cases[0].result_present);
        assert!(report.queries.search.latency.is_some());

        let _ = fs::remove_dir_all(temp_dir);
    }

    fn write_test_pack(path: &Path) {
        let mut writer = PackWriter::create(path).expect("writer");
        writer
            .write_record(NormalizedRecord::address(AddressRecord {
                id: "osm:node:1".to_string(),
                label: "10 King Street, Toronto".to_string(),
                name: "10 King Street".to_string(),
                address: AddressComponents {
                    number: "10".to_string(),
                    street: Some("King Street".to_string()),
                    place: None,
                    unit: None,
                    locality: Some("Toronto".to_string()),
                    region: Some("Ontario".to_string()),
                    postcode: Some("M5V 1A1".to_string()),
                    country: Some("CA".to_string()),
                },
                geometry: point_geometry(-79.4, 43.6),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(OsmObjectType::Node, 1),
            }))
            .expect("write address");
        writer
            .finish(&mut BuilderReport::default())
            .expect("finish");
    }

    fn temp_pack_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("open-geocode-{name}-{}", std::process::id()))
    }
}
