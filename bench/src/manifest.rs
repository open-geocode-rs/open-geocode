//! `manifest.json`: everything needed to say what was measured, on what.
//!
//! The disk numbers come from fio and take two minutes, so they are run once
//! per machine and cached under `results/`.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::engine::Engine;
use crate::log;
use crate::run::Ctx;

pub fn sha256(path: &Path) -> Result<String> {
    let out = Command::new("sha256sum").arg(path).output()?;
    ensure!(out.status.success(), "sha256sum {} failed", path.display());
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string())
}

/// The first line of a command's stdout, or the error in its place.
fn first_line(cmd: &str, args: &[&str]) -> String {
    match Command::new(cmd).args(args).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
        Err(e) => format!("unavailable: {e}"),
    }
}

pub fn write(ctx: &Ctx, engines: &[Engine]) -> Result<()> {
    let qdir = ctx.bench_dir.join("data/queries");
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(qdir.join("meta.json"))?)?;
    let pbf = meta["pbf"].as_str().context("meta.json has no pbf")?;
    log("manifest: hashing the pbf and query files");
    let repo = ctx.bench_dir.parent().context("bench dir has no parent")?;
    let git = |args: &[&str]| {
        let mut a = vec!["-C", repo.to_str().unwrap_or(".")];
        a.extend_from_slice(args);
        first_line("git", &a)
    };
    let cpu_model = Command::new("lscpu")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .find_map(|l| l.strip_prefix("Model name:").map(|m| m.trim().to_string()))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let mut engine_list = Vec::new();
    for e in engines {
        engine_list.push(json!({
            "name": e.spec.name,
            "engine_toml": e.toml,
            "images": e.images().unwrap_or_default(),
        }));
    }
    let manifest = json!({
        "date_utc": crate::utc_stamp(),
        "command": std::env::args().collect::<Vec<_>>(),
        "profile": ctx.profile,
        "pbf": { "path": pbf, "sha256": sha256(Path::new(pbf))? },
        "queries": {
            "forward_sha256": sha256(&qdir.join("forward.jsonl"))?,
            "reverse_sha256": sha256(&qdir.join("reverse.jsonl"))?,
            "meta": meta,
        },
        "engines": engine_list,
        "og_git_sha": git(&["rev-parse", "HEAD"]),
        "og_git_dirty": !git(&["status", "--porcelain"]).is_empty(),
        "kernel": first_line("uname", &["-r"]),
        "cpu_model": cpu_model,
        "virt": first_line("systemd-detect-virt", &[]),
        "docker_version": first_line("docker", &["version", "--format", "{{.Server.Version}}"]),
        "compose_version": first_line("docker", &["compose", "version", "--short"]),
        "oha_version": ctx.oha.version()?,
        "fio": fio(ctx)?,
    });
    std::fs::write(
        ctx.out.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)? + "\n",
    )?;
    Ok(())
}

/// One 4k random read and one 1M sequential read on the disk that holds the
/// engine data, 60 s each with direct I/O so the page cache is not measured.
fn fio(ctx: &Ctx) -> Result<Value> {
    let cache = ctx.bench_dir.join("results/fio-cache.json");
    if let Ok(text) = std::fs::read_to_string(&cache) {
        return Ok(serde_json::from_str(&text)?);
    }
    std::fs::create_dir_all(cache.parent().unwrap())?;
    let file = ctx.bench_dir.join("results/fio-testfile");
    let device = first_line(
        "findmnt",
        &[
            "-n",
            "-o",
            "SOURCE",
            "--target",
            ctx.bench_dir.to_str().unwrap_or("/"),
        ],
    );
    let mut results = serde_json::Map::new();
    for (name, rw, bs) in [
        ("randread_4k", "randread", "4k"),
        ("seqread_1m", "read", "1M"),
    ] {
        log(&format!("manifest: fio {name}, 60 s"));
        let out = Command::new("fio")
            .args([
                "--name",
                name,
                "--rw",
                rw,
                "--bs",
                bs,
                "--size",
                "4G",
                "--direct=1",
            ])
            .args([
                "--ioengine=libaio",
                "--iodepth=32",
                "--numjobs=1",
                "--runtime=60",
                "--time_based",
            ])
            .args(["--group_reporting", "--output-format=json"])
            .arg(format!("--filename={}", file.display()))
            .output()
            .context("running fio")?;
        ensure!(
            out.status.success(),
            "fio failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        let v: Value = serde_json::from_slice(&out.stdout)?;
        let read = &v["jobs"][0]["read"];
        results.insert(
            name.to_string(),
            json!({
                "iops": read["iops"],
                "bw_mib_s": read["bw_bytes"].as_f64().map(|b| b / 1048576.0),
                "clat_p50_us": read["clat_ns"]["percentile"]["50.000000"].as_f64().map(|n| n / 1000.0),
                "clat_p99_us": read["clat_ns"]["percentile"]["99.000000"].as_f64().map(|n| n / 1000.0),
                "params": format!("rw={rw} bs={bs} size=4G direct=1 ioengine=libaio iodepth=32 numjobs=1 runtime=60"),
                "fio_version": v["fio version"],
            }),
        );
    }
    let _ = std::fs::remove_file(&file);
    let v = json!({ "device": device, "measured_utc": crate::utc_stamp(), "results": results });
    std::fs::write(&cache, serde_json::to_string_pretty(&v)? + "\n")?;
    Ok(v)
}
