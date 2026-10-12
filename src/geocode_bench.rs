//! Coordinate-scored geocoding benchmark.
//!
//! Separate from [`crate::bench`] on purpose: that harness checks whether a
//! named object came back by `osm_type`/`osm_id`, an equality test. This one
//! scores how far the returned point is from an authoritative coordinate, an
//! ordering test. Sharing one fixture would leave the scoring rule implicit in
//! which fields a case happened to populate.
//!
//! The distance buckets and the geohash precisions are read from the fixture's
//! own metadata and are never derived from the run, so a later build cannot
//! retune the scale it is measured against.

use std::collections::BTreeMap;
use std::f64::consts::PI;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::search::{PackTextSearcher, TextSearchOptions};

/// Metres per degree of latitude, good enough for the sub-kilometre
/// comparisons these buckets make.
const METRES_PER_DEGREE: f64 = 111_320.0;

#[derive(Debug, Clone)]
pub struct GeocodeBenchmarkOptions {
    pub pack: PathBuf,
    pub fixture: PathBuf,
    pub limit: usize,
    pub output: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct GeocodeFixture {
    #[serde(default)]
    meta: FixtureMeta,
    cases: Vec<GeocodeCase>,
}

/// Frozen scoring contract. Defaults match the committed fixtures so an older
/// fixture without a `meta` block still scores on the same scale.
#[derive(Debug, Clone, Deserialize)]
struct FixtureMeta {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    gnaf_release: Option<String>,
    #[serde(default)]
    datum: Option<String>,
    #[serde(default = "default_buckets")]
    buckets_m: BTreeMap<String, f64>,
}

impl Default for FixtureMeta {
    fn default() -> Self {
        Self {
            state: None,
            gnaf_release: None,
            datum: None,
            buckets_m: default_buckets(),
        }
    }
}

fn default_buckets() -> BTreeMap<String, f64> {
    BTreeMap::from([
        ("exact".to_string(), 25.0),
        ("correct".to_string(), 100.0),
        ("near".to_string(), 500.0),
        ("off".to_string(), 5_000.0),
    ])
}

#[derive(Debug, Clone, Deserialize)]
struct GeocodeCase {
    /// Case identifier, carried into the worst-case list for triage.
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "q", alias = "query")]
    query: String,
    #[serde(default)]
    form: Option<String>,
    #[serde(default)]
    cohort: Option<String>,
    /// Whether the expected street exists in the Pack at all. Cases that are
    /// not covered are counted apart so an OSM coverage gap is never read as a
    /// ranking failure.
    #[serde(default = "covered_by_default")]
    covered: bool,
    expect_lat: f64,
    expect_lon: f64,
    #[serde(default)]
    gnaf_pid: Option<String>,
}

fn covered_by_default() -> bool {
    true
}

#[derive(Debug, Clone, Serialize)]
pub struct GeocodeBenchmarkReport {
    pub pack: String,
    pub fixture: String,
    pub state: Option<String>,
    pub gnaf_release: Option<String>,
    pub datum: Option<String>,
    pub buckets_m: BTreeMap<String, f64>,
    pub cases: usize,
    /// Keyed `<cohort>/<form>/<coverage>`, plus `all` roll-ups.
    pub groups: BTreeMap<String, GroupReport>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GroupReport {
    pub cases: usize,
    /// `hit@1`: bucket of the first result's distance.
    pub hit1: BTreeMap<String, usize>,
    /// `hit@5`: bucket of the *best* distance among the first five results, so
    /// a block of neighbouring houses cannot satisfy it by accident.
    pub hit5: BTreeMap<String, usize>,
    /// Geohash-prefix agreement, an absolute grid that cannot be retuned.
    pub gh6_hit1: usize,
    pub gh4_hit1: usize,
    pub median_distance_m: Option<f64>,
    pub p90_distance_m: Option<f64>,
    /// Up to five of the worst `hit@1` outcomes, for triage.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub worst: Vec<WorstCase>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorstCase {
    pub name: Option<String>,
    pub query: String,
    pub gnaf_pid: Option<String>,
    pub distance_m: Option<f64>,
}

pub fn benchmark_geocode(options: GeocodeBenchmarkOptions) -> Result<GeocodeBenchmarkReport> {
    let fixture: GeocodeFixture = {
        let text = std::fs::read_to_string(&options.fixture)
            .with_context(|| format!("failed to read fixture {}", options.fixture.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("failed to parse fixture {}", options.fixture.display()))?
    };

    let searcher = PackTextSearcher::open(&options.pack)?;
    let limit = options.limit.max(5);

    let mut groups: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    for case in &fixture.cases {
        let hits = searcher
            .search(TextSearchOptions {
                query: case.query.clone(),
                limit,
                layer: None,
            })
            .with_context(|| format!("failed to search for {:?}", case.query))?;

        let expected = (case.expect_lat, case.expect_lon);
        let distances: Vec<f64> = hits
            .iter()
            .filter_map(|hit| hit.record.point)
            .map(|point| distance_metres(expected, (point.lat, point.lon)))
            .collect();

        let first = distances.first().copied();
        let best5 = distances
            .iter()
            .take(5)
            .copied()
            .fold(None::<f64>, |acc, d| Some(acc.map_or(d, |a| a.min(d))));

        let first_point = hits.iter().find_map(|hit| hit.record.point);
        let coverage = if case.covered { "covered" } else { "uncovered" };
        let cohort = case.cohort.as_deref().unwrap_or("primary");
        let form = case.form.as_deref().unwrap_or("full");

        for key in [
            format!("{cohort}/{form}/{coverage}"),
            format!("{cohort}/{form}/all"),
            format!("all/{form}/{coverage}"),
            "all/all/all".to_string(),
        ] {
            let slot = groups.entry(key).or_default();
            slot.cases += 1;
            slot.hit1
                .entry(bucket_of(first, &fixture.meta.buckets_m))
                .and_modify(|n| *n += 1)
                .or_insert(1);
            slot.hit5
                .entry(bucket_of(best5, &fixture.meta.buckets_m))
                .and_modify(|n| *n += 1)
                .or_insert(1);
            if let (Some(point), true) = (first_point, first.is_some()) {
                if geohash(point.lat, point.lon, 6) == geohash(expected.0, expected.1, 6) {
                    slot.gh6 += 1;
                }
                if geohash(point.lat, point.lon, 4) == geohash(expected.0, expected.1, 4) {
                    slot.gh4 += 1;
                }
            }
            if let Some(d) = first {
                slot.distances.push(d);
            }
            slot.worst.push(WorstCase {
                name: case.name.clone(),
                query: case.query.clone(),
                gnaf_pid: case.gnaf_pid.clone(),
                distance_m: first.map(|d| (d * 100.0).round() / 100.0),
            });
        }
    }

    let report = GeocodeBenchmarkReport {
        pack: options.pack.display().to_string(),
        fixture: options.fixture.display().to_string(),
        state: fixture.meta.state.clone(),
        gnaf_release: fixture.meta.gnaf_release.clone(),
        datum: fixture.meta.datum.clone(),
        buckets_m: fixture.meta.buckets_m.clone(),
        cases: fixture.cases.len(),
        groups: groups
            .into_iter()
            .map(|(key, acc)| (key, acc.finish()))
            .collect(),
    };

    if let Some(path) = options.output.as_deref() {
        write_report(path, &report)?;
    }
    Ok(report)
}

fn write_report(path: &Path, report: &GeocodeBenchmarkReport) -> Result<()> {
    let text = serde_json::to_string_pretty(report)?;
    std::fs::write(path, text)
        .with_context(|| format!("failed to write report {}", path.display()))?;
    Ok(())
}

#[derive(Default)]
struct GroupAccumulator {
    cases: usize,
    hit1: BTreeMap<String, usize>,
    hit5: BTreeMap<String, usize>,
    gh6: usize,
    gh4: usize,
    distances: Vec<f64>,
    worst: Vec<WorstCase>,
}

impl GroupAccumulator {
    fn finish(mut self) -> GroupReport {
        self.distances
            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = percentile(&self.distances, 0.5);
        let p90 = percentile(&self.distances, 0.9);
        // Unanswered first, then the furthest misses.
        self.worst
            .sort_by(|a, b| match (a.distance_m, b.distance_m) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(x), Some(y)) => y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal),
            });
        self.worst.truncate(5);
        GroupReport {
            cases: self.cases,
            hit1: self.hit1,
            hit5: self.hit5,
            gh6_hit1: self.gh6,
            gh4_hit1: self.gh4,
            median_distance_m: median,
            p90_distance_m: p90,
            worst: self.worst,
        }
    }
}

fn percentile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted
        .get(idx)
        .copied()
        .map(|v| (v * 100.0).round() / 100.0)
}

/// `None` distance means the query returned nothing, which is a coverage
/// outcome and is kept apart from a wrong answer.
fn bucket_of(distance: Option<f64>, buckets: &BTreeMap<String, f64>) -> String {
    let Some(d) = distance else {
        return "none".to_string();
    };
    let mut named: Vec<(&String, &f64)> = buckets.iter().collect();
    named.sort_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal));
    for (name, limit) in named {
        if d <= *limit {
            return name.clone();
        }
    }
    "far".to_string()
}

fn distance_metres((lat1, lon1): (f64, f64), (lat2, lon2): (f64, f64)) -> f64 {
    let mid = ((lat1 + lat2) / 2.0) * PI / 180.0;
    let dy = (lat2 - lat1) * METRES_PER_DEGREE;
    let dx = (lon2 - lon1) * METRES_PER_DEGREE * mid.cos();
    dx.hypot(dy)
}

const GEOHASH_ALPHABET: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";

fn geohash(lat: f64, lon: f64, chars: usize) -> String {
    let mut lat_range = (-90.0f64, 90.0f64);
    let mut lon_range = (-180.0f64, 180.0f64);
    let mut hash = String::with_capacity(chars);
    let mut bit = 0;
    let mut value = 0usize;
    let mut even = true;
    while hash.len() < chars {
        if even {
            let mid = (lon_range.0 + lon_range.1) / 2.0;
            if lon > mid {
                value = (value << 1) | 1;
                lon_range.0 = mid;
            } else {
                value <<= 1;
                lon_range.1 = mid;
            }
        } else {
            let mid = (lat_range.0 + lat_range.1) / 2.0;
            if lat > mid {
                value = (value << 1) | 1;
                lat_range.0 = mid;
            } else {
                value <<= 1;
                lat_range.1 = mid;
            }
        }
        even = !even;
        bit += 1;
        if bit == 5 {
            hash.push(GEOHASH_ALPHABET[value] as char);
            bit = 0;
            value = 0;
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_order_by_limit_not_by_name() {
        let buckets = default_buckets();
        assert_eq!(bucket_of(Some(10.0), &buckets), "exact");
        assert_eq!(bucket_of(Some(80.0), &buckets), "correct");
        assert_eq!(bucket_of(Some(300.0), &buckets), "near");
        assert_eq!(bucket_of(Some(4_000.0), &buckets), "off");
        assert_eq!(bucket_of(Some(9_000.0), &buckets), "far");
    }

    #[test]
    fn a_missing_result_is_none_not_far() {
        assert_eq!(bucket_of(None, &default_buckets()), "none");
    }

    #[test]
    fn distance_is_symmetric_and_scales_with_latitude() {
        let a = (-42.9, 147.3);
        let b = (-42.9, 147.4);
        let forward = distance_metres(a, b);
        assert!((forward - distance_metres(b, a)).abs() < 1e-6);
        // A degree of longitude is shorter away from the equator.
        let equator = distance_metres((0.0, 147.3), (0.0, 147.4));
        assert!(forward < equator);
    }

    #[test]
    fn geohash_prefixes_agree_for_nearby_points() {
        let a = geohash(-42.91654, 147.26215, 6);
        let b = geohash(-42.91669, 147.26209, 6);
        assert_eq!(a, b, "points ~20m apart share a 6-char geohash");
        let far = geohash(-33.86, 151.21, 6);
        assert_ne!(a, far);
    }

    #[test]
    fn geohash_length_is_requested_length() {
        assert_eq!(geohash(-42.9, 147.3, 4).len(), 4);
        assert_eq!(geohash(-42.9, 147.3, 6).len(), 6);
    }
}
