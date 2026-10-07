use std::{
    collections::HashMap,
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
    record::{Layer, OsmObjectType, Record},
    reverse::{PackReverseGeocoder, ReverseGeocodeOptions},
    search::{PackTextSearcher, TextAutocompleteOptions, TextSearchHit, TextSearchOptions},
    text_index::normalize_index_text,
};

/// Hits a sampled POI case asks for: enough to score hit@5.
const POI_CASE_LIMIT: usize = 5;

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
    /// Over the cases that name the object they expect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accuracy: Option<AccuracyReport>,
    pub cases: Vec<T>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AccuracyReport {
    pub cases: usize,
    pub hit_at_1: usize,
    pub hit_at_5: usize,
    pub hit_at_1_rate: f64,
    pub hit_at_5_rate: f64,
}

/// The OSM object a query case should find.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExpectedObject {
    pub osm_type: OsmObjectType,
    pub osm_id: i64,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expect: Option<ExpectedObject>,
    /// 1-based rank of the expected object, absent when it was not returned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_rank: Option<usize>,
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

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct BenchmarkFixture {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search: Vec<TextQueryFixture>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub autocomplete: Vec<TextQueryFixture>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reverse: Vec<ReverseQueryFixture>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextQueryFixture {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "q", alias = "query")]
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<ExpectedObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReverseQueryFixture {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub lon: f64,
    pub lat: f64,
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
    benchmark_text_cases(cases, iterations, warmup, |case, limit| {
        searcher.search(TextSearchOptions {
            query: case.query.clone(),
            limit,
            layer: case.layer.clone(),
        })
    })
}

fn benchmark_autocomplete_cases(
    searcher: &PackTextSearcher,
    cases: &[TextQueryFixture],
    iterations: usize,
    warmup: usize,
) -> Result<OperationBenchmarkReport<TextQueryCaseReport>> {
    benchmark_text_cases(cases, iterations, warmup, |case, limit| {
        searcher.autocomplete(TextAutocompleteOptions {
            query: case.query.clone(),
            limit,
            layer: case.layer.clone(),
        })
    })
}

fn benchmark_text_cases(
    cases: &[TextQueryFixture],
    iterations: usize,
    warmup: usize,
    run: impl Fn(&TextQueryFixture, usize) -> Result<Vec<TextSearchHit>>,
) -> Result<OperationBenchmarkReport<TextQueryCaseReport>> {
    let mut reports = Vec::new();
    let mut all_durations = Vec::new();
    for case in cases {
        let limit = case.limit.unwrap_or(10);
        let mut hits = Vec::new();
        let durations = measure_iterations(iterations, warmup, || {
            hits = run(case, limit)?;
            Ok(())
        })?;
        all_durations.extend(durations.iter().copied());
        reports.push(TextQueryCaseReport {
            name: case.name.clone(),
            query: case.query.clone(),
            limit,
            layer: case.layer.clone(),
            hit_count: hits.len(),
            expect: case.expect,
            expected_rank: case.expect.and_then(|expect| {
                hits.iter()
                    .position(|hit| {
                        hit.record.source.object_type == Some(expect.osm_type)
                            && hit.record.source.object_id == Some(expect.osm_id)
                    })
                    .map(|index| index + 1)
            }),
            latency: LatencyStats::from_nanos(&durations),
        });
    }

    let mut report = operation_report(cases.len(), iterations, warmup, reports, &all_durations);
    report.accuracy = AccuracyReport::from_ranks(
        report
            .cases
            .iter()
            .filter(|case| case.expect.is_some())
            .map(|case| case.expected_rank),
    );
    Ok(report)
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
        accuracy: None,
        cases,
    }
}

impl AccuracyReport {
    /// `None` when no case expects an object.
    fn from_ranks(ranks: impl Iterator<Item = Option<usize>>) -> Option<Self> {
        let (mut cases, mut hit_at_1, mut hit_at_5) = (0, 0, 0);
        for rank in ranks {
            cases += 1;
            hit_at_1 += usize::from(rank == Some(1));
            hit_at_5 += usize::from(rank.is_some_and(|rank| rank <= 5));
        }
        (cases > 0).then(|| Self {
            cases,
            hit_at_1,
            hit_at_5,
            hit_at_1_rate: hit_at_1 as f64 / cases as f64,
            hit_at_5_rate: hit_at_5 as f64 / cases as f64,
        })
    }
}

#[derive(Debug, Clone)]
pub struct PoiFixtureOptions {
    pub pack: PathBuf,
    pub count: usize,
    pub seed: u64,
}

/// An answer key of named POIs: `count` cases drawn with `seed` from the POIs
/// whose name is unique within their locality, each queried as
/// "<name>, <locality>" and expected to find that POI. The locality is the
/// one [`PackReader::locality`] names; POIs without one are not drawn.
pub fn sample_poi_fixture(options: PoiFixtureOptions) -> Result<BenchmarkFixture> {
    let reader = PackReader::open(&options.pack)?;
    let mut candidates = Vec::new();
    // POIs per (area, name), over both the locality and the district a POI
    // lies in: "<name>, Toronto" must be unique among every POI in Toronto,
    // including those in a locality inside it.
    let mut name_counts: HashMap<(String, String), usize> = HashMap::new();
    for record_id in 0..reader.manifest().record_count {
        if reader.records().header(record_id)?.layer != Layer::Poi {
            continue;
        }
        let Record::Poi(poi) = reader.records().record(record_id)? else {
            continue;
        };
        let Some(context) = reader.boundary_context(record_id)? else {
            continue;
        };
        let name = normalize_index_text(&poi.name).unwrap_or_default();
        let tuple = context.admin_context;
        let mut areas = Vec::new();
        for area_id in [tuple.locality_record_id, tuple.district_record_id]
            .into_iter()
            .flatten()
        {
            if let Some(area) = reader.context_record(area_id)? {
                areas.push(normalize_index_text(&area.name).unwrap_or_default());
            }
        }
        areas.dedup();
        for area in areas {
            *name_counts.entry((area, name.clone())).or_default() += 1;
        }
        if let Some(locality) = reader.locality(record_id)? {
            let key = (normalize_index_text(&locality).unwrap_or_default(), name);
            candidates.push((key, poi, locality));
        }
    }
    candidates.retain(|(key, _, _)| name_counts[key] == 1);

    // A seeded Fisher-Yates prefix: reproducible for one Pack and seed.
    let mut random = SplitMix64(options.seed);
    let count = options.count.min(candidates.len());
    for index in 0..count {
        let pick = index + (random.next() % (candidates.len() - index) as u64) as usize;
        candidates.swap(index, pick);
    }
    let search = candidates
        .into_iter()
        .take(count)
        .map(|(_, poi, locality)| TextQueryFixture {
            name: Some(poi.category.clone()),
            query: format!("{}, {locality}", poi.name),
            limit: Some(POI_CASE_LIMIT),
            layer: None,
            expect: Some(ExpectedObject {
                osm_type: poi.source.object_type,
                osm_id: poi.source.object_id,
            }),
        })
        .collect();
    Ok(BenchmarkFixture {
        search,
        ..BenchmarkFixture::default()
    })
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
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
        context::AdminContextTuple,
        pack::{PackWriter, RecordContext},
        record::{
            AddressComponents, AddressRecord, LocationPrecision, OsmObjectType, PlaceLayer,
            PlaceRecord, PoiRecord, SourceProvenance, point_geometry,
        },
    };

    use super::*;

    #[test]
    fn samples_unique_pois_and_scores_hits_at_one_and_five() {
        let temp_dir = temp_pack_path("bench-poi-fixture");
        let _ = fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        let mut area = |layer, name: &str, object_id| {
            writer
                .write(
                    &Record::Place(
                        layer,
                        PlaceRecord {
                            name: name.to_string(),
                            place_type: "admin_level:8".to_string(),
                            geometry: point_geometry(-79.4, 43.6),
                            source: SourceProvenance::osm(OsmObjectType::Relation, object_id),
                        },
                    ),
                    None,
                )
                .expect("area")
        };
        // Toronto is a district with no locality over its core.
        let toronto = area(PlaceLayer::District, "Toronto", 1);
        let north_york = area(PlaceLayer::Locality, "North York", 2);
        let hamilton = area(PlaceLayer::Locality, "Hamilton", 3);
        writer.set_context_names(
            [
                (toronto, "Toronto".to_string()),
                (north_york, "North York".to_string()),
                (hamilton, "Hamilton".to_string()),
            ]
            .into_iter()
            .collect(),
        );
        let in_area = |locality, district| {
            Some(RecordContext {
                admin_context: AdminContextTuple {
                    locality_record_id: locality,
                    district_record_id: district,
                    ..AdminContextTuple::default()
                },
                flags: 0,
            })
        };
        let downtown = in_area(None, Some(toronto));
        for (object_id, name, context) in [
            // Two in Toronto share a name: neither can be expected.
            (10, "Tim Hortons", downtown),
            (11, "TIM  HORTONS", downtown),
            (12, "Tim Hortons", in_area(Some(hamilton), None)),
            (13, "Riverdale Farm", downtown),
            // No locality to query it with.
            (14, "Lonely Cabin", None),
            // Unique in North York, but "Ali Baba, Toronto" would find both.
            (15, "Ali Baba", in_area(Some(north_york), Some(toronto))),
            (16, "Ali Baba", downtown),
        ] {
            let poi = PoiRecord {
                name: name.to_string(),
                category: "amenity:cafe".to_string(),
                address: None,
                geometry: point_geometry(-79.4, 43.6),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(OsmObjectType::Node, object_id),
            };
            writer.write(&poi.into(), context).expect("POI");
        }
        writer.finish().expect("finish");
        let reader = PackReader::open(&temp_dir).expect("reader");
        let riverdale = reader
            .find_by_source_id("osm:node:13")
            .expect("lookup")
            .expect("Riverdale Farm");
        assert_eq!(
            reader.record_summary(riverdale).expect("summary").label,
            "Riverdale Farm, Toronto",
            "labelled with its district where no locality covers it"
        );

        let sample = |seed| {
            sample_poi_fixture(PoiFixtureOptions {
                pack: temp_dir.clone(),
                count: 10,
                seed,
            })
            .expect("sample")
        };
        let fixture = sample(7);
        let mut queries = fixture
            .search
            .iter()
            .map(|case| (case.query.as_str(), case.expect.expect("expect").osm_id))
            .collect::<Vec<_>>();
        queries.sort();
        assert_eq!(
            queries,
            vec![
                ("Ali Baba, North York", 15),
                ("Riverdale Farm, Toronto", 13),
                ("Tim Hortons, Hamilton", 12)
            ]
        );
        assert_eq!(
            serde_json::to_value(&sample(7).search).expect("json"),
            serde_json::to_value(&fixture.search).expect("json"),
            "the same seed draws the same cases"
        );

        // One case expects an object the query cannot find.
        let mut fixture = fixture;
        fixture.search.push(TextQueryFixture {
            name: None,
            query: "Riverdale Farm, Toronto".to_string(),
            limit: Some(5),
            layer: None,
            expect: Some(ExpectedObject {
                osm_type: OsmObjectType::Way,
                osm_id: 13,
            }),
        });
        let fixture_path = temp_dir.join("poi.json");
        fs::write(
            &fixture_path,
            serde_json::to_vec(&fixture).expect("fixture json"),
        )
        .expect("write fixture");
        let report = benchmark_pack(PackBenchmarkOptions {
            pack: temp_dir.clone(),
            queries: Some(fixture_path),
            iterations: 1,
            warmup: 0,
        })
        .expect("benchmark");
        let accuracy = report.queries.search.accuracy.expect("accuracy");
        assert_eq!(
            (accuracy.cases, accuracy.hit_at_1, accuracy.hit_at_5),
            (4, 3, 3)
        );
        assert_eq!(report.queries.search.cases[3].expected_rank, None);
        assert!(report.queries.autocomplete.accuracy.is_none());

        let _ = fs::remove_dir_all(temp_dir);
    }

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
