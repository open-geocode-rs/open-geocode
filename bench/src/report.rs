//! `summary.json` and `report.md`, rebuilt from a run's `results.jsonl` at
//! any time with `bench report --out <run>`.
//!
//! Each figure is the median over repeats with the min and max beside it; the
//! raw records go into the summary unchanged so nothing is lost on the way to
//! a table.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

use crate::engine::NAMES;
use crate::host::{BoxSize, mib_label};
use crate::run::{Direction, ErrorRecord, FloorRecord, Record, SCALE_CPUS};

#[derive(Debug, Clone, Serialize)]
pub struct Stat {
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub n: usize,
    pub values: Vec<f64>,
}

pub fn stat(values: Vec<f64>) -> Option<Stat> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.clone();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    let median = if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    };
    Some(Stat {
        median,
        min: v[0],
        max: v[n - 1],
        n,
        values,
    })
}

#[derive(Debug, Clone, Serialize, Default)]
struct Footprint {
    cold_start_s: Option<Stat>,
    memory_peak_bytes: Option<Stat>,
    anon_bytes: Option<Stat>,
    file_bytes: Option<Stat>,
    data_bytes: Option<Stat>,
    image_bytes: Option<Stat>,
    disk_total_bytes: Option<Stat>,
}

#[derive(Debug, Clone, Serialize, Default)]
struct DirectionSummary {
    p50_ms: Option<Stat>,
    p90_ms: Option<Stat>,
    p99_ms: Option<Stat>,
    p999_ms: Option<Stat>,
    max_ms: Option<Stat>,
    service_errors: Option<Stat>,
    share_within_100m: Option<Stat>,
    share_any_result: Option<Stat>,
    ceiling: Ceiling,
}

#[derive(Debug, Clone, Serialize, Default)]
struct Ceiling {
    rps: Option<Stat>,
    /// Repeats where a rate above the load-generator limit passed.
    load_generator_bound: usize,
    /// Repeats where no rate passed at all.
    none_passed: usize,
    limit_rps: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Default)]
struct EngineSummary {
    footprint: Footprint,
    equal_box: BTreeMap<Direction, DirectionSummary>,
    floor: BTreeMap<Direction, FloorRecord>,
    /// Direction, then CPU count.
    scale: BTreeMap<Direction, BTreeMap<u32, Ceiling>>,
}

#[derive(Debug, Serialize)]
struct Summary {
    run: String,
    /// Repeats behind the equal-box figures.
    repeats: u32,
    load_generator_max_rps: Option<f64>,
    load_generator_limit_rps: Option<f64>,
    engines: BTreeMap<String, EngineSummary>,
    retried_steps: Vec<String>,
    invalid_steps: Vec<String>,
    errors: Vec<ErrorRecord>,
    records: Vec<Record>,
}

fn ceiling_of<'a>(recs: impl Iterator<Item = &'a crate::run::CeilingRecord>) -> Ceiling {
    let mut c = Ceiling::default();
    let mut values = Vec::new();
    for r in recs {
        c.limit_rps = r.load_limit_rps;
        if r.load_generator_bound {
            c.load_generator_bound += 1;
        } else if let Some(v) = r.ceiling_rps {
            values.push(v);
        } else {
            c.none_passed += 1;
        }
    }
    c.rps = stat(values);
    c
}

fn summarize(run: &str, records: Vec<Record>) -> Summary {
    let repeats = records
        .iter()
        .filter_map(|r| match r {
            Record::Measure(m) => Some(m.repeat + 1),
            Record::Ceiling(c) if c.context == "equal-box" => Some(c.repeat + 1),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let mut s = Summary {
        run: run.to_string(),
        repeats,
        load_generator_max_rps: None,
        load_generator_limit_rps: None,
        engines: BTreeMap::new(),
        retried_steps: Vec::new(),
        invalid_steps: Vec::new(),
        errors: Vec::new(),
        records: Vec::new(),
    };
    let mut note = |label: String, valid: bool, retries: &[String]| {
        s.retried_steps.extend(retries.iter().cloned());
        if !valid {
            s.invalid_steps.push(label);
        }
    };
    let mut measures = Vec::new();
    let mut ceilings = Vec::new();
    for r in &records {
        match r {
            Record::Headroom(h) => {
                s.load_generator_max_rps = h.max_rps;
                s.load_generator_limit_rps = h.limit_rps;
                for st in &h.steps {
                    note(st.run.value.raw.clone(), st.run.valid, &st.run.retries);
                }
            }
            Record::Measure(m) => {
                note(
                    format!("{} cold start r{}", m.engine, m.repeat),
                    m.cold_start_s.valid,
                    &m.cold_start_s.retries,
                );
                for d in Direction::BOTH {
                    let t = m.service.get(d);
                    note(t.value.raw.clone(), t.valid, &t.retries);
                }
                measures.push(m.as_ref());
            }
            Record::Ceiling(c) => {
                for st in &c.steps {
                    note(st.run.value.raw.clone(), st.run.valid, &st.run.retries);
                }
                ceilings.push(c);
            }
            Record::Floor(f) => {
                let steps = f
                    .cpu_steps
                    .iter()
                    .chain(&f.memory_steps)
                    .chain(&f.confirm_found)
                    .chain(&f.confirm_next_smaller);
                for st in steps.filter_map(|j| j.step.as_ref()) {
                    note(st.run.value.raw.clone(), st.run.valid, &st.run.retries);
                }
                s.engines
                    .entry(f.engine.clone())
                    .or_default()
                    .floor
                    .insert(f.direction, f.clone());
            }
            Record::Error(e) => s.errors.push(e.clone()),
        }
    }

    let names: Vec<String> = measures
        .iter()
        .map(|m| m.engine.clone())
        .chain(ceilings.iter().map(|c| c.engine.clone()))
        .collect();
    for name in names {
        let ms: Vec<_> = measures.iter().filter(|m| m.engine == name).collect();
        let e = s.engines.entry(name.clone()).or_default();
        let col = |f: &dyn Fn(&crate::run::MeasureRecord) -> Option<f64>| {
            stat(ms.iter().filter_map(|m| f(m)).collect())
        };
        e.footprint = Footprint {
            cold_start_s: col(&|m| Some(m.cold_start_s.value)),
            memory_peak_bytes: col(&|m| Some(m.memory.peak_bytes as f64)),
            anon_bytes: col(&|m| Some(m.memory.anon_bytes as f64)),
            file_bytes: col(&|m| Some(m.memory.file_bytes as f64)),
            data_bytes: col(&|m| Some(m.disk.data_bytes as f64)),
            image_bytes: col(&|m| Some(m.disk.image_bytes as f64)),
            disk_total_bytes: col(&|m| Some(m.disk.total_bytes as f64)),
        };
        for d in Direction::BOTH {
            let sv = |f: &dyn Fn(&crate::oha::OhaRun) -> Option<f64>| {
                col(&|m| f(&m.service.get(d).value))
            };
            let ds = DirectionSummary {
                p50_ms: sv(&|o| o.p50_ms),
                p90_ms: sv(&|o| o.p90_ms),
                p99_ms: sv(&|o| o.p99_ms),
                p999_ms: sv(&|o| o.p999_ms),
                max_ms: sv(&|o| o.max_ms),
                service_errors: sv(&|o| Some((o.errors + o.non_2xx) as f64)),
                share_within_100m: col(&|m| Some(m.correctness.get(d).share_within_100m)),
                share_any_result: col(&|m| Some(m.correctness.get(d).share_any_result)),
                ceiling: ceiling_of(ceilings.iter().copied().filter(|c| {
                    c.engine == name
                        && c.direction == d
                        && c.context == "equal-box"
                        && c.boxed == BoxSize::BIG
                })),
            };
            e.equal_box.insert(d, ds);
            for cpus in SCALE_CPUS {
                let c = ceiling_of(ceilings.iter().copied().filter(|c| {
                    c.engine == name
                        && c.direction == d
                        && c.context == "scale"
                        && c.boxed.cpus == cpus
                }));
                if c.rps.is_some() || c.load_generator_bound + c.none_passed > 0 {
                    e.scale.entry(d).or_default().insert(cpus, c);
                }
            }
        }
    }
    s.records = records;
    s
}

pub fn write(out: &Path) -> Result<()> {
    let text =
        std::fs::read_to_string(out.join("results.jsonl")).context("reading results.jsonl")?;
    let records = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).context("parsing a record"))
        .collect::<Result<Vec<Record>>>()?;
    let manifest: Value = std::fs::read_to_string(out.join("manifest.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let run = out
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let summary = summarize(&run, records);
    std::fs::write(
        out.join("summary.json"),
        serde_json::to_string_pretty(&summary)? + "\n",
    )?;
    std::fs::write(out.join("report.md"), render(&summary, &manifest))?;
    Ok(())
}

fn bytes(b: f64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else {
        format!("{:.0} MiB", b / 1024.0 / 1024.0)
    }
}

fn show(s: &Option<Stat>, f: impl Fn(f64) -> String) -> String {
    match s {
        None => "–".into(),
        Some(s) if s.n == 1 || s.min == s.max => f(s.median),
        Some(s) => format!("{} ({}–{})", f(s.median), f(s.min), f(s.max)),
    }
}

fn show_ceiling(c: &Ceiling) -> String {
    let mut parts = Vec::new();
    if c.rps.is_some() {
        parts.push(show(&c.rps, |v| format!("{v:.0}")));
    }
    if c.load_generator_bound > 0 {
        let limit = c
            .limit_rps
            .map_or("the limit".into(), |l| format!("{l:.0}"));
        parts.push(format!(
            "> {limit}, load-generator bound ({}×)",
            c.load_generator_bound
        ));
    }
    if c.none_passed > 0 {
        parts.push(format!("no rate passed ({}×)", c.none_passed));
    }
    if parts.is_empty() {
        "–".into()
    } else {
        parts.join("; ")
    }
}

fn repeat_label(n: u32) -> String {
    if n == 1 {
        "1 repeat".into()
    } else {
        format!("{n} repeats")
    }
}

fn ms(v: f64) -> String {
    format!("{v:.2}")
}

fn pct(v: f64) -> String {
    format!("{:.1}%", v * 100.0)
}

fn box_text(b: &BoxSize) -> String {
    format!("{} CPU / {}", b.cpus, mib_label(b.mem_mib))
}

fn engine_order(s: &Summary) -> Vec<&String> {
    let mut names: Vec<&String> = s.engines.keys().collect();
    names.sort_by_key(|n| NAMES.iter().position(|k| k == n).unwrap_or(usize::MAX));
    names
}

fn render(s: &Summary, m: &Value) -> String {
    let mut o = String::new();
    let str_of = |v: &Value| v.as_str().unwrap_or("–").to_string();
    let _ = writeln!(o, "# Speed benchmark {}\n", s.run);
    let _ = writeln!(o, "- Date (UTC): {}", str_of(&m["date_utc"]));
    let _ = writeln!(o, "- Profile: {}", str_of(&m["profile"]["name"]));
    let _ = writeln!(
        o,
        "- og commit: {}{}",
        str_of(&m["og_git_sha"]),
        if m["og_git_dirty"] == true {
            " (dirty)"
        } else {
            ""
        }
    );
    let _ = writeln!(
        o,
        "- Host: {} ({}), kernel {}",
        str_of(&m["cpu_model"]),
        str_of(&m["virt"]),
        str_of(&m["kernel"])
    );
    let _ = writeln!(o, "- Data: pbf sha256 {}", str_of(&m["pbf"]["sha256"]));
    let _ = writeln!(
        o,
        "- Queries: {} eligible addresses; forward sha256 {}, reverse sha256 {}",
        m["queries"]["meta"]["eligible_addresses"],
        str_of(&m["queries"]["forward_sha256"]),
        str_of(&m["queries"]["reverse_sha256"])
    );
    let _ = writeln!(
        o,
        "- Load generator: sustains {} req/s on cores 8-15; ceilings above {} req/s are reported as load-generator bound",
        s.load_generator_max_rps
            .map_or("–".into(), |v| format!("{v:.0}")),
        s.load_generator_limit_rps
            .map_or("–".into(), |v| format!("{v:.0}"))
    );
    let _ = writeln!(
        o,
        "\nFigures are medians over repeats, with min–max in brackets when they differ.\n"
    );

    let names = engine_order(s);
    let reps = repeat_label(s.repeats);
    let _ = writeln!(o, "## Footprint and cold start, {reps}\n");
    let _ = writeln!(
        o,
        "Memory is the peak of the engine's cgroup, page cache included, in the 16 CPU / 64 GiB box. Images are the content size docker image inspect reports for the image store.\n"
    );
    let _ = writeln!(
        o,
        "| Engine | Cold start (s) | Memory peak | Anon at end | File at end | Data on disk | Images (content) | Disk total |"
    );
    let _ = writeln!(o, "|---|---|---|---|---|---|---|---|");
    for n in &names {
        let f = &s.engines[*n].footprint;
        if f.cold_start_s.is_none() {
            continue;
        }
        let _ = writeln!(
            o,
            "| {n} | {} | {} | {} | {} | {} | {} | {} |",
            show(&f.cold_start_s, |v| format!("{v:.1}")),
            show(&f.memory_peak_bytes, bytes),
            show(&f.anon_bytes, bytes),
            show(&f.file_bytes, bytes),
            show(&f.data_bytes, bytes),
            show(&f.image_bytes, bytes),
            show(&f.disk_total_bytes, bytes),
        );
    }

    let _ = writeln!(o, "\n## Equal big box (16 vCPU / 64 GiB), {reps}\n");
    let _ = writeln!(
        o,
        "Service time is one connection, requests back to back. The ceiling is the highest rate with p99 < 100 ms, no errors and ≥ 99% of the rate served.\n"
    );
    let _ = writeln!(
        o,
        "| Engine | Direction | p50 ms | p90 ms | p99 ms | p99.9 ms | max ms | Errors | Ceiling (req/s) | Within 100 m | Any result |"
    );
    let _ = writeln!(o, "|---|---|---|---|---|---|---|---|---|---|---|");
    for n in &names {
        for (d, ds) in &s.engines[*n].equal_box {
            if ds.p50_ms.is_none()
                && ds.ceiling.rps.is_none()
                && ds.ceiling.load_generator_bound == 0
            {
                continue;
            }
            let _ = writeln!(
                o,
                "| {n} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                d.name(),
                show(&ds.p50_ms, ms),
                show(&ds.p90_ms, ms),
                show(&ds.p99_ms, ms),
                show(&ds.p999_ms, ms),
                show(&ds.max_ms, ms),
                show(&ds.service_errors, |v| format!("{v:.0}")),
                show_ceiling(&ds.ceiling),
                show(&ds.share_within_100m, pct),
                show(&ds.share_any_result, pct),
            );
        }
    }

    let _ = writeln!(o, "\n## Smallest box for the job (100 req/s), 1 run\n");
    let _ = writeln!(
        o,
        "The job is 100 req/s with p99 < 100 ms and no errors, from a cold start and one warm-up pass. The found box and the next memory step down are each run again from cold.\n"
    );
    let _ = writeln!(
        o,
        "| Engine | Direction | Smallest box | Passes | Next smaller | Passes | Verdict |"
    );
    let _ = writeln!(o, "|---|---|---|---|---|---|---|");
    for n in &names {
        for (d, f) in &s.engines[*n].floor {
            let passes = |v: &[crate::run::JobStep]| {
                if !v.is_empty() && v.iter().all(|j| j.not_configurable) {
                    "not configurable".to_string()
                } else {
                    format!("{}/{}", v.iter().filter(|j| j.pass).count(), v.len())
                }
            };
            let _ = writeln!(
                o,
                "| {n} | {} | {} | {} | {} | {} | {} |",
                d.name(),
                f.found.as_ref().map_or("–".into(), box_text),
                passes(&f.confirm_found),
                f.next_smaller.as_ref().map_or("–".into(), box_text),
                passes(&f.confirm_next_smaller),
                f.verdict
            );
        }
    }

    for n in &names {
        let sc = &s.engines[*n].scale;
        if sc.is_empty() {
            continue;
        }
        let _ = writeln!(o, "\n## {n} scaling (64 GiB), 1 run\n");
        let _ = writeln!(
            o,
            "| CPUs | Forward ceiling (req/s) | Reverse ceiling (req/s) | Forward per CPU | Reverse per CPU |"
        );
        let _ = writeln!(o, "|---|---|---|---|---|");
        let empty = BTreeMap::new();
        let fw = sc.get(&Direction::Forward).unwrap_or(&empty);
        let rv = sc.get(&Direction::Reverse).unwrap_or(&empty);
        for cpus in SCALE_CPUS {
            let (f, r) = (fw.get(&cpus), rv.get(&cpus));
            if f.is_none() && r.is_none() {
                continue;
            }
            let per = |c: Option<&Ceiling>| {
                c.and_then(|c| c.rps.as_ref()).map_or("–".into(), |s| {
                    format!("{:.0}", s.median / f64::from(cpus))
                })
            };
            let _ = writeln!(
                o,
                "| {cpus} | {} | {} | {} | {} |",
                f.map_or("–".into(), show_ceiling),
                r.map_or("–".into(), show_ceiling),
                per(f),
                per(r)
            );
        }
    }

    let _ = writeln!(o, "\n## Validity\n");
    let _ = writeln!(
        o,
        "- Timed steps re-run for steal over 1% of the engine cores' time: {}",
        s.retried_steps.len()
    );
    for r in &s.retried_steps {
        let _ = writeln!(o, "  - {r}");
    }
    let _ = writeln!(
        o,
        "- Steps still over the limit after two retries (kept, marked invalid): {}",
        s.invalid_steps.len()
    );
    for r in &s.invalid_steps {
        let _ = writeln!(o, "  - {r}");
    }
    let _ = writeln!(
        o,
        "- Errors that stopped a unit of work: {}",
        s.errors.len()
    );
    for e in &s.errors {
        let _ = writeln!(o, "  - {} {}: {}", e.engine, e.stage, e.message);
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats() {
        let s = stat(vec![3.0, 1.0, 2.0]).unwrap();
        assert_eq!((s.median, s.min, s.max, s.n), (2.0, 1.0, 3.0, 3));
        assert_eq!(stat(vec![4.0, 1.0]).unwrap().median, 2.5);
        assert!(stat(vec![]).is_none());
        assert_eq!(
            show(&stat(vec![1.0, 2.0, 3.0]), |v| format!("{v:.0}")),
            "2 (1–3)"
        );
        assert_eq!(show(&stat(vec![5.0]), |v| format!("{v:.0}")), "5");
    }
}
