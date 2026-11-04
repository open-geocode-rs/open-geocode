//! Correctness: does each answer land where the address is?
//!
//! Queries go one at a time from the harness, untimed, so the score is about
//! the answers and not about load. Every outcome is written down so a
//! surprising share can be traced to the queries behind it.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::engine::probe;
use crate::geo::haversine_m;

/// The distance within which a first result counts as correct.
pub const CORRECT_M: f64 = 100.0;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Score {
    pub queries: usize,
    /// First result within 100 m of the truth.
    pub within_100m: usize,
    /// Any result at all.
    pub any_result: usize,
    /// Transport failures and non-2xx answers.
    pub failed: usize,
    pub share_within_100m: f64,
    pub share_any_result: f64,
    /// Median distance of the first result, over queries that had one.
    pub median_error_m: Option<f64>,
}

#[derive(Serialize)]
struct Outcome {
    i: usize,
    status: Option<u16>,
    dist_m: Option<f64>,
}

/// Sends every URL in turn and scores its first result against `truths`
/// (`(lon, lat)`), writing one outcome per line to `out`.
pub fn score(
    http: &ureq::Agent,
    pointer: &str,
    urls: &[String],
    truths: &[(f64, f64)],
    out: &Path,
) -> Result<Score> {
    let mut w = BufWriter::new(File::create(out)?);
    let mut s = Score {
        queries: urls.len(),
        ..Score::default()
    };
    let mut dists = Vec::with_capacity(urls.len());
    for (i, (url, &(tlon, tlat))) in urls.iter().zip(truths).enumerate() {
        let (status, coords) = match probe(http, url, pointer) {
            Ok((st, c)) => (Some(st), c),
            Err(_) => (None, None),
        };
        if !status.is_some_and(|s| (200..300).contains(&s)) {
            s.failed += 1;
        }
        let dist = coords.map(|(lon, lat)| haversine_m(lon, lat, tlon, tlat));
        if let Some(d) = dist {
            s.any_result += 1;
            if d <= CORRECT_M {
                s.within_100m += 1;
            }
            dists.push(d);
        }
        writeln!(
            w,
            "{}",
            serde_json::to_string(&Outcome {
                i,
                status,
                dist_m: dist
            })?
        )?;
    }
    w.flush()?;
    let n = s.queries.max(1) as f64;
    s.share_within_100m = s.within_100m as f64 / n;
    s.share_any_result = s.any_result as f64 / n;
    dists.sort_by(f64::total_cmp);
    s.median_error_m = (!dists.is_empty()).then(|| dists[dists.len() / 2]);
    Ok(s)
}
