//! The rate and box searches.
//!
//! These hold no I/O: each takes a `passes` closure that runs one real step,
//! so the stepping rules can be tested against a fake engine with a known
//! answer.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// How close the ceiling bisection gets: the last pass and the first fail
/// end within 5% of each other.
pub const CEILING_TOLERANCE: f64 = 0.05;
/// The ramp multiplier while a rate keeps passing.
pub const RAMP: f64 = 1.5;
/// How close the memory bisection gets.
pub const MEMORY_TOLERANCE: f64 = 0.10;

/// Where the ceiling ramp starts: half of what the box could serve if every
/// core answered back to back at the single-connection median.
pub fn start_rate(cpus: u32, p50_s: f64) -> f64 {
    (0.5 * f64::from(cpus) / p50_s).round().max(1.0)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CeilingSearch {
    pub start: f64,
    /// Every rate tried, in order, and whether it passed.
    pub probes: Vec<(f64, bool)>,
    /// The highest passing rate, `None` if no rate passed.
    pub ceiling: Option<f64>,
    /// The lowest failing rate above the ceiling.
    pub first_fail: Option<f64>,
    /// A rate above `limit` passed, so the search stopped: past that point the
    /// load generator, not the engine, may be what limits the rate.
    pub load_generator_bound: bool,
}

/// Ramps by `RAMP` from `start` while rates pass, then bisects between the
/// last pass and the first fail until they are within `tolerance` (a share
/// of the last pass; `CEILING_TOLERANCE` for the full profile).
///
/// If `start` itself fails the ramp runs downward by the same factor until a
/// rate passes, so the bisection always has a bracket. Any passing rate above
/// `limit` ends the search there.
pub fn ceiling(
    start: f64,
    tolerance: f64,
    limit: Option<f64>,
    mut passes: impl FnMut(f64) -> Result<bool>,
) -> Result<CeilingSearch> {
    let mut s = CeilingSearch {
        start,
        probes: Vec::new(),
        ceiling: None,
        first_fail: None,
        load_generator_bound: false,
    };
    let above_limit = |rate: f64| limit.is_some_and(|l| rate > l);
    let mut probe = |s: &mut CeilingSearch, rate: f64| -> Result<bool> {
        let ok = passes(rate)?;
        s.probes.push((rate, ok));
        if ok {
            s.ceiling = Some(rate);
            if above_limit(rate) {
                s.load_generator_bound = true;
            }
        } else {
            s.first_fail = Some(rate);
        }
        Ok(ok)
    };

    let (mut lo, mut hi);
    if probe(&mut s, start)? {
        lo = start;
        loop {
            if s.load_generator_bound {
                return Ok(s);
            }
            let next = (lo * RAMP).round();
            if probe(&mut s, next)? {
                lo = next;
            } else {
                hi = next;
                break;
            }
        }
    } else {
        hi = start;
        loop {
            let next = (hi / RAMP).round();
            if next < 1.0 || next >= hi {
                return Ok(s);
            }
            if probe(&mut s, next)? {
                lo = next;
                break;
            }
            hi = next;
        }
    }
    while (hi - lo) > tolerance * lo && !s.load_generator_bound {
        let mid = ((lo + hi) / 2.0).round();
        if mid <= lo || mid >= hi {
            break;
        }
        if probe(&mut s, mid)? {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    s.ceiling = Some(lo);
    s.first_fail = Some(hi);
    Ok(s)
}

/// Runs exactly `rates`, in order, with no ramp or bisection. This is the
/// smoke profile's plumbing check: it goes through every step of a ceiling
/// without searching for one.
pub fn fixed(
    rates: &[f64],
    limit: Option<f64>,
    mut passes: impl FnMut(f64) -> Result<bool>,
) -> Result<CeilingSearch> {
    let mut s = CeilingSearch {
        start: rates.first().copied().unwrap_or(0.0),
        probes: Vec::new(),
        ceiling: None,
        first_fail: None,
        load_generator_bound: false,
    };
    for &rate in rates {
        let ok = passes(rate)?;
        s.probes.push((rate, ok));
        if ok {
            s.ceiling = Some(s.ceiling.map_or(rate, |c: f64| c.max(rate)));
            s.load_generator_bound |= limit.is_some_and(|l| rate > l);
        } else if s.first_fail.is_none_or(|f| rate < f) {
            s.first_fail = Some(rate);
        }
    }
    Ok(s)
}

/// The CPU counts tried for the job, smallest-first order of preference.
pub const JOB_CPUS: [u32; 3] = [4, 2, 1];
/// The box that decides "does not meet job" when even 4 CPUs fail.
pub const BIG_CPUS: u32 = 16;

/// The smallest CPU count in `JOB_CPUS` that passes, walking down from 4 and
/// stopping at the first failure. If 4 fails, 16 is tried: a pass there makes
/// 16 the answer, a fail means the engine does not meet the job at all.
pub fn cpu_floor(mut passes: impl FnMut(u32) -> Result<bool>) -> Result<CpuSearch> {
    let mut probes = Vec::new();
    let mut found = None;
    for cpus in JOB_CPUS {
        let ok = passes(cpus)?;
        probes.push((cpus, ok));
        if !ok {
            break;
        }
        found = Some(cpus);
    }
    if found.is_none() {
        let ok = passes(BIG_CPUS)?;
        probes.push((BIG_CPUS, ok));
        found = ok.then_some(BIG_CPUS);
    }
    Ok(CpuSearch { found, probes })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CpuSearch {
    pub found: Option<u32>,
    pub probes: Vec<(u32, bool)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySearch {
    pub probes: Vec<(u64, bool)>,
    /// The smallest memory, in MiB, known to pass.
    pub found_mib: u64,
    /// The largest memory known to fail, or the untested lower bound when
    /// every probe passed.
    pub below_mib: u64,
    /// Whether `below_mib` was actually run and failed.
    pub below_tested: bool,
}

/// Bisects memory between `lo_mib` (assumed to fail, not run) and `hi_mib`
/// (known to pass) until they are within `MEMORY_TOLERANCE` of each other.
///
/// The midpoint is geometric because the tolerance is a ratio: each step then
/// halves the remaining ratio, and 64 GiB down to 256 MiB takes six steps
/// instead of nine.
pub fn memory_floor(
    lo_mib: u64,
    hi_mib: u64,
    mut passes: impl FnMut(u64) -> Result<bool>,
) -> Result<MemorySearch> {
    let (mut lo, mut hi) = (lo_mib, hi_mib);
    let mut probes = Vec::new();
    let mut below_tested = false;
    while hi as f64 > lo as f64 * (1.0 + MEMORY_TOLERANCE) {
        let mid = ((lo as f64) * (hi as f64)).sqrt().round() as u64;
        if mid <= lo || mid >= hi {
            break;
        }
        let ok = passes(mid)?;
        probes.push((mid, ok));
        if ok {
            hi = mid;
        } else {
            lo = mid;
            below_tested = true;
        }
    }
    Ok(MemorySearch {
        probes,
        found_mib: hi,
        below_mib: lo,
        below_tested,
    })
}

/// Walks memory down a fixed `ladder` (largest first) until the first step
/// fails, with no bisection. `known_pass_mib` is the box already known to
/// pass, reported as found if the first rung fails.
pub fn memory_ladder(
    ladder: &[u64],
    known_pass_mib: u64,
    mut passes: impl FnMut(u64) -> Result<bool>,
) -> Result<MemorySearch> {
    let mut s = MemorySearch {
        probes: Vec::new(),
        found_mib: known_pass_mib,
        below_mib: known_pass_mib,
        below_tested: false,
    };
    for &mib in ladder {
        let ok = passes(mib)?;
        s.probes.push((mib, ok));
        if ok {
            s.found_mib = mib;
            s.below_mib = mib;
        } else {
            s.below_mib = mib;
            s.below_tested = true;
            break;
        }
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake engine that passes every rate up to `max`.
    fn fake(max: f64) -> impl FnMut(f64) -> Result<bool> {
        move |r| Ok(r <= max)
    }

    #[test]
    fn start_rate_is_half_the_back_to_back_rate() {
        assert_eq!(start_rate(16, 0.002), 4000.0);
        assert_eq!(start_rate(1, 0.0031), 161.0);
        assert_eq!(start_rate(1, 10.0), 1.0);
    }

    #[test]
    fn ceiling_ramps_then_bisects_to_tolerance() {
        let s = ceiling(1000.0, CEILING_TOLERANCE, None, fake(7300.0)).unwrap();
        let c = s.ceiling.unwrap();
        let f = s.first_fail.unwrap();
        assert!(c <= 7300.0 && f > 7300.0);
        assert!(f - c <= CEILING_TOLERANCE * c, "{c} {f}");
        // The ramp is 1000, 1500, 2250, 3375, 5063, 7595 (fail).
        assert_eq!(
            &s.probes[..6],
            &[
                (1000.0, true),
                (1500.0, true),
                (2250.0, true),
                (3375.0, true),
                (5063.0, true),
                (7595.0, false)
            ]
        );
        assert!(!s.load_generator_bound);
    }

    #[test]
    fn ceiling_walks_down_when_the_start_fails() {
        let s = ceiling(1000.0, CEILING_TOLERANCE, None, fake(130.0)).unwrap();
        let c = s.ceiling.unwrap();
        assert!(c <= 130.0 && s.first_fail.unwrap() - c <= CEILING_TOLERANCE * c);
        assert_eq!(&s.probes[..2], &[(1000.0, false), (667.0, false)]);
    }

    #[test]
    fn ceiling_reports_nothing_when_no_rate_passes() {
        let s = ceiling(10.0, CEILING_TOLERANCE, None, fake(0.5)).unwrap();
        assert_eq!(s.ceiling, None);
        assert!(s.probes.iter().all(|p| !p.1));
    }

    #[test]
    fn ceiling_stops_above_the_load_generator_limit() {
        let s = ceiling(1000.0, CEILING_TOLERANCE, Some(3000.0), fake(1e9)).unwrap();
        assert!(s.load_generator_bound);
        assert_eq!(s.ceiling, Some(3375.0));
        assert_eq!(s.probes.len(), 4);
        // Bound during the bisection too.
        let s = ceiling(1000.0, CEILING_TOLERANCE, Some(2400.0), fake(2600.0)).unwrap();
        assert!(s.load_generator_bound);
        assert!(s.ceiling.unwrap() > 2400.0);
    }

    #[test]
    fn ceiling_exact_at_tight_bracket() {
        // A fake that passes exactly at the start and nowhere above.
        let s = ceiling(100.0, CEILING_TOLERANCE, None, fake(100.0)).unwrap();
        assert_eq!(s.ceiling, Some(100.0));
        assert!(s.first_fail.unwrap() <= 105.0);
    }

    #[test]
    fn fixed_runs_exactly_the_rates_given() {
        let s = fixed(&[100.0, 4000.0], None, fake(1000.0)).unwrap();
        assert_eq!(s.probes, vec![(100.0, true), (4000.0, false)]);
        assert_eq!((s.ceiling, s.first_fail), (Some(100.0), Some(4000.0)));
        let s = fixed(&[100.0, 4000.0], Some(50.0), fake(1e9)).unwrap();
        assert_eq!(s.ceiling, Some(4000.0));
        assert!(s.load_generator_bound);
    }

    #[test]
    fn cpu_floor_walks_down_and_falls_back_to_sixteen() {
        let s = cpu_floor(|c| Ok(c >= 2)).unwrap();
        assert_eq!(s.found, Some(2));
        assert_eq!(s.probes, vec![(4, true), (2, true), (1, false)]);
        assert_eq!(cpu_floor(|_| Ok(true)).unwrap().found, Some(1));
        let s = cpu_floor(|c| Ok(c >= 8)).unwrap();
        assert_eq!(s.found, Some(16));
        assert_eq!(s.probes, vec![(4, false), (16, true)]);
        assert_eq!(cpu_floor(|_| Ok(false)).unwrap().found, None);
    }

    #[test]
    fn memory_floor_bisects_geometrically_to_tolerance() {
        let need = 3000;
        let s = memory_floor(256, 65536, |m| Ok(m >= need)).unwrap();
        assert!(s.found_mib >= need);
        assert!(s.below_tested && s.below_mib < need);
        assert!(s.found_mib as f64 <= s.below_mib as f64 * (1.0 + MEMORY_TOLERANCE));
        assert!(s.probes.len() <= 7, "{:?}", s.probes);
        // First probe is the geometric midpoint, 4 GiB.
        assert_eq!(s.probes[0], (4096, true));
    }

    #[test]
    fn ceiling_honours_a_looser_tolerance() {
        let s = ceiling(1000.0, 0.10, None, fake(7300.0)).unwrap();
        let (c, f) = (s.ceiling.unwrap(), s.first_fail.unwrap());
        assert!(c <= 7300.0 && f > 7300.0 && f - c <= 0.10 * c, "{c} {f}");
        let tight = ceiling(1000.0, CEILING_TOLERANCE, None, fake(7300.0)).unwrap();
        assert!(s.probes.len() < tight.probes.len());
    }

    #[test]
    fn memory_ladder_stops_at_the_first_fail() {
        let ladder = [16384, 8192, 4096, 2048, 1024, 512, 256];
        let s = memory_ladder(&ladder, 65536, |m| Ok(m >= 2048)).unwrap();
        assert_eq!(
            (s.found_mib, s.below_mib, s.below_tested),
            (2048, 1024, true)
        );
        assert_eq!(s.probes.len(), 5);
        let s = memory_ladder(&ladder, 65536, |_| Ok(true)).unwrap();
        assert_eq!((s.found_mib, s.below_tested), (256, false));
        let s = memory_ladder(&ladder, 65536, |_| Ok(false)).unwrap();
        assert_eq!(
            (s.found_mib, s.below_mib, s.below_tested),
            (65536, 16384, true)
        );
    }

    #[test]
    fn memory_floor_runs_down_to_the_lower_bound() {
        let s = memory_floor(256, 65536, |_| Ok(true)).unwrap();
        assert!(!s.below_tested);
        assert_eq!(s.below_mib, 256);
        assert!(s.found_mib <= 282);
    }
}
