use std::collections::BTreeMap;
use std::f64::consts::PI;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    search::{TextSearchOptions, TextSearcher},
    util::geo::{METRES_PER_DEGREE, geohash},
};

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
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "q", alias = "query")]
    query: String,
    #[serde(default)]
    form: Option<String>,
    #[serde(default)]
    cohort: Option<String>,
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
    pub groups: BTreeMap<String, GroupReport>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GroupReport {
    pub cases: usize,
    pub hit1: BTreeMap<String, usize>,
    pub hit5: BTreeMap<String, usize>,
    pub gh6_hit1: usize,
    pub gh4_hit1: usize,
    pub median_distance_m: Option<f64>,
    pub p90_distance_m: Option<f64>,
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

pub fn benchmark_geocode(options: &GeocodeBenchmarkOptions) -> Result<GeocodeBenchmarkReport> {
    let fixture: GeocodeFixture = {
        let text = std::fs::read_to_string(&options.fixture)
            .with_context(|| format!("failed to read fixture {}", options.fixture.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("failed to parse fixture {}", options.fixture.display()))?
    };

    let searcher = TextSearcher::open(&options.pack)?;
    let limit = options.limit.max(5);

    let mut groups: BTreeMap<String, GroupAccumulator> = BTreeMap::new();
    for case in &fixture.cases {
        let hits = searcher
            .search(&TextSearchOptions {
                query: case.query.clone(),
                limit,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
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
        assert_eq!(bucket_of(None, &buckets), "none");
    }
}
