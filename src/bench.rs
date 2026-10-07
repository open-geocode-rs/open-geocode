use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    builder::report::{BuilderReport, PhaseTimings, ScratchReport, Throughput},
    pack::{AUDIT_DIR, BUILD_REPORT_FILE, PackManifest, PackReader},
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
    pub manifest: PackManifest,
    pub bytes: PackByteMetrics,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildMetricReport>,
}

/// Build timings and throughput from the build report next to the Pack.
#[derive(Debug, Serialize)]
pub struct BuildMetricReport {
    pub input_bytes: u64,
    pub accepted_records: u64,
    pub rejected_records: u64,
    pub total_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_mib_per_sec: Option<f64>,
    pub phases: PhaseTimings,
    pub throughput: Throughput,
    pub scratch: ScratchReport,
}

/// Pack bytes by section group.
#[derive(Debug, Default, Serialize)]
pub struct PackByteMetrics {
    pub total: u64,
    pub records: u64,
    pub context: u64,
    pub text_index: u64,
    pub spatial_index: u64,
    pub other: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_per_record: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub records_bytes_per_record: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_index_bytes_per_record: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spatial_index_bytes_per_record: Option<f64>,
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

    let (reader, pack_reader_ms) = measure_value(|| PackReader::open(&options.pack).map(Arc::new))?;
    let pack = pack_metrics(&reader)?;
    let (searcher, text_searcher_ms) =
        measure_value(|| PackTextSearcher::from_pack(Arc::clone(&reader)))?;
    let (reverse_geocoder, reverse_geocoder_ms) =
        measure_value(|| PackReverseGeocoder::from_pack(Arc::clone(&reader)))?;

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

fn pack_metrics(reader: &PackReader) -> Result<PackMetricReport> {
    let manifest = reader.manifest();
    let mut bytes = PackByteMetrics {
        total: reader.container().file_size(),
        ..PackByteMetrics::default()
    };
    for (name, len) in reader.section_sizes() {
        let group = match name.split('/').next().unwrap_or_default() {
            "records" => &mut bytes.records,
            "context" => &mut bytes.context,
            "text" => &mut bytes.text_index,
            "spatial" => &mut bytes.spatial_index,
            _ => &mut bytes.other,
        };
        *group += len;
    }
    if manifest.record_count > 0 {
        let count = manifest.record_count as f64;
        bytes.bytes_per_record = Some(bytes.total as f64 / count);
        bytes.records_bytes_per_record = Some(bytes.records as f64 / count);
        bytes.text_index_bytes_per_record = Some(bytes.text_index as f64 / count);
        bytes.spatial_index_bytes_per_record = Some(bytes.spatial_index as f64 / count);
    }
    Ok(PackMetricReport {
        manifest: manifest.clone(),
        bytes,
        build: read_build_report(reader.path())?.map(build_metrics),
    })
}

fn read_build_report(pack_file: &Path) -> Result<Option<BuilderReport>> {
    let Some(path) = pack_file
        .parent()
        .map(|dir| dir.join(AUDIT_DIR).join(BUILD_REPORT_FILE))
        .filter(|path| path.is_file())
    else {
        return Ok(None);
    };
    let file =
        fs::File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::from_reader(file)
        .map(Some)
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn build_metrics(report: BuilderReport) -> BuildMetricReport {
    let total_seconds = report.phases.total_ms as f64 / 1_000.0;
    BuildMetricReport {
        input_bytes: report.input_bytes,
        accepted_records: report.accepted.total,
        rejected_records: report.rejected.total,
        total_seconds,
        input_mib_per_sec: (total_seconds > 0.0)
            .then(|| report.input_bytes as f64 / 1_048_576.0 / total_seconds),
        phases: report.phases,
        throughput: report.throughput,
        scratch: report.scratch,
    }
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
        pack::PackWriter,
        record::{
            AddressComponents, AddressRecord, LocationPrecision, OsmObjectType, SourceProvenance,
            point_geometry,
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
        assert!(report.pack.bytes.records > 0);
        assert!(report.pack.bytes.text_index > 0);
        assert!(report.pack.bytes.spatial_index > 0);
        assert!(
            report.pack.build.is_none(),
            "a Pack written directly has no build report"
        );
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
            .write(
                &AddressRecord {
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
                }
                .into(),
                None,
            )
            .expect("write address");
        writer.finish().expect("finish");
    }

    fn temp_pack_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("open-geocode-{name}-{}", std::process::id()))
    }
}
