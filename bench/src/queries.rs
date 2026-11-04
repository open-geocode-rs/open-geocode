//! The query sets.
//!
//! Every node or way in the extract with a house number, a street, and a city
//! or postcode is an address someone could ask for. From all of them a seeded
//! sample gives 10,000 forward queries and 10,000 reverse points, with the
//! address location kept as the truth each answer is scored against.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

use crate::geo::{jitter, round7};

pub const SEED: u64 = 20261101;
pub const PER_SET: usize = 10_000;
pub const JITTER_M: f64 = 50.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Forward {
    pub q: String,
    pub lon: f64,
    pub lat: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reverse {
    pub lat: f64,
    pub lon: f64,
    pub truth_lon: f64,
    pub truth_lat: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Meta {
    pub pbf: PathBuf,
    pub pbf_sha256: String,
    pub eligible_addresses: usize,
    pub seed: u64,
    pub per_set: usize,
    pub jitter_m: f64,
    pub osmium: String,
    pub commands: Vec<String>,
}

pub fn load(dir: &Path) -> Result<(Vec<Forward>, Vec<Reverse>)> {
    Ok((
        read_jsonl(&dir.join("forward.jsonl"))?,
        read_jsonl(&dir.join("reverse.jsonl"))?,
    ))
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    BufReader::new(f)
        .lines()
        .map(|l| Ok(serde_json::from_str(&l?)?))
        .collect::<Result<Vec<T>>>()
        .with_context(|| format!("reading {}", path.display()))
}

/// Runs the extraction and writes `forward.jsonl`, `reverse.jsonl` and
/// `meta.json` into `out`. `work` holds the intermediate files.
pub fn make(pbf: &Path, work: &Path, out: &Path) -> Result<Meta> {
    std::fs::create_dir_all(work)?;
    std::fs::create_dir_all(out)?;
    let filtered = work.join("addr.osm.pbf");
    let seq = work.join("addr.geojsonseq");
    let filter = vec![
        "osmium".to_string(),
        "tags-filter".into(),
        pbf.display().to_string(),
        "nw/addr:housenumber".into(),
        "-o".into(),
        filtered.display().to_string(),
        "--overwrite".into(),
    ];
    // Ways come out as their geometry; referenced nodes are untagged and so
    // are not exported.
    let export = vec![
        "osmium".to_string(),
        "export".into(),
        filtered.display().to_string(),
        "-f".into(),
        "geojsonseq".into(),
        "-x".into(),
        "print_record_separator=false".into(),
        "-o".into(),
        seq.display().to_string(),
        "--overwrite".into(),
    ];
    for cmd in [&filter, &export] {
        crate::log(&format!("$ {}", cmd.join(" ")));
        let st = Command::new(&cmd[0]).args(&cmd[1..]).status()?;
        ensure!(st.success(), "{} failed", cmd.join(" "));
    }

    let addresses = read_addresses(&seq)?;
    let n = addresses.len();
    ensure!(n >= 2 * PER_SET, "only {n} eligible addresses");
    crate::log(&format!("{n} eligible addresses"));

    let mut rng = ChaCha8Rng::seed_from_u64(SEED);
    let picks = rand::seq::index::sample(&mut rng, n, 2 * PER_SET).into_vec();
    let mut fw = BufWriter::new(File::create(out.join("forward.jsonl"))?);
    for &i in &picks[..PER_SET] {
        let a = &addresses[i];
        let q = Forward {
            q: a.text.clone(),
            lon: a.lon,
            lat: a.lat,
        };
        writeln!(fw, "{}", serde_json::to_string(&q)?)?;
    }
    fw.flush()?;
    let mut rw = BufWriter::new(File::create(out.join("reverse.jsonl"))?);
    for &i in &picks[PER_SET..] {
        let a = &addresses[i];
        let (lon, lat) = jitter(a.lon, a.lat, JITTER_M, &mut rng);
        let q = Reverse {
            lat: round7(lat),
            lon: round7(lon),
            truth_lon: a.lon,
            truth_lat: a.lat,
        };
        writeln!(rw, "{}", serde_json::to_string(&q)?)?;
    }
    rw.flush()?;

    let osmium = Command::new("osmium").arg("--version").output()?;
    let meta = Meta {
        pbf: pbf.to_path_buf(),
        pbf_sha256: crate::manifest::sha256(pbf)?,
        eligible_addresses: n,
        seed: SEED,
        per_set: PER_SET,
        jitter_m: JITTER_M,
        osmium: String::from_utf8_lossy(&osmium.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string(),
        commands: vec![filter.join(" "), export.join(" ")],
    };
    std::fs::write(
        out.join("meta.json"),
        serde_json::to_string_pretty(&meta)? + "\n",
    )?;
    Ok(meta)
}

struct Address {
    text: String,
    lon: f64,
    lat: f64,
}

#[derive(Deserialize)]
struct Feature {
    geometry: Geometry,
    properties: HashMap<String, String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "coordinates")]
enum Geometry {
    Point([f64; 2]),
    LineString(Vec<[f64; 2]>),
    MultiLineString(Vec<Vec<[f64; 2]>>),
    Polygon(Vec<Vec<[f64; 2]>>),
    MultiPolygon(Vec<Vec<Vec<[f64; 2]>>>),
}

/// Every eligible address in file order. osmium writes objects sorted by type
/// and id, so the order, and with it the seeded sample, is fixed by the pbf.
fn read_addresses(path: &Path) -> Result<Vec<Address>> {
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in BufReader::with_capacity(1 << 20, f).lines().enumerate() {
        let line = line?;
        let feat: Feature =
            serde_json::from_str(&line).with_context(|| format!("{}:{}", path.display(), i + 1))?;
        let Some(text) = address_text(&feat.properties) else {
            continue;
        };
        let Some([lon, lat]) = centroid(&feat.geometry) else {
            continue;
        };
        out.push(Address {
            text,
            lon: round7(lon),
            lat: round7(lat),
        });
    }
    Ok(out)
}

/// `"{street} {housenumber}, {city}"`, the order Dutch addresses are written
/// in, with the postcode standing in when there is no city.
fn address_text(tags: &HashMap<String, String>) -> Option<String> {
    let get = |k: &str| tags.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
    let street = get("addr:street")?;
    let number = get("addr:housenumber")?;
    let place = get("addr:city").or_else(|| get("addr:postcode"))?;
    Some(format!("{street} {number}, {place}"))
}

/// The point a way's address stands for: the area centroid for polygons (the
/// largest one if there are several), the length-weighted midpoint for lines.
fn centroid(g: &Geometry) -> Option<[f64; 2]> {
    match g {
        Geometry::Point(p) => Some(*p),
        Geometry::LineString(l) => line_centroid(std::slice::from_ref(l)),
        Geometry::MultiLineString(ls) => line_centroid(ls),
        Geometry::Polygon(rings) => ring_centroid(rings.first()?).map(|(c, _)| c),
        Geometry::MultiPolygon(polys) => polys
            .iter()
            .filter_map(|rings| ring_centroid(rings.first()?))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(c, _)| c),
    }
}

/// Shoelace centroid and area of a ring. A centroid is unchanged by an affine
/// map, so working in raw degrees gives the same point as projecting first.
/// Degenerate rings fall back to the mean of their vertices.
fn ring_centroid(ring: &[[f64; 2]]) -> Option<([f64; 2], f64)> {
    let first = *ring.first()?;
    let mut pts: Vec<[f64; 2]> = ring
        .iter()
        .map(|p| [p[0] - first[0], p[1] - first[1]])
        .collect();
    if pts.len() > 1 && pts.first() == pts.last() {
        pts.pop();
    }
    let (mut a, mut cx, mut cy) = (0.0, 0.0, 0.0);
    for i in 0..pts.len() {
        let (p, q) = (pts[i], pts[(i + 1) % pts.len()]);
        let c = p[0] * q[1] - q[0] * p[1];
        a += c;
        cx += (p[0] + q[0]) * c;
        cy += (p[1] + q[1]) * c;
    }
    if a.abs() < 1e-18 {
        let n = pts.len() as f64;
        let mx = pts.iter().map(|p| p[0]).sum::<f64>() / n;
        let my = pts.iter().map(|p| p[1]).sum::<f64>() / n;
        return Some(([first[0] + mx, first[1] + my], 0.0));
    }
    Some((
        [first[0] + cx / (3.0 * a), first[1] + cy / (3.0 * a)],
        a.abs() / 2.0,
    ))
}

fn line_centroid(lines: &[Vec<[f64; 2]>]) -> Option<[f64; 2]> {
    let (mut len, mut sx, mut sy) = (0.0, 0.0, 0.0);
    for l in lines {
        for w in l.windows(2) {
            let d = ((w[1][0] - w[0][0]).powi(2) + (w[1][1] - w[0][1]).powi(2)).sqrt();
            len += d;
            sx += d * (w[0][0] + w[1][0]) / 2.0;
            sy += d * (w[0][1] + w[1][1]) / 2.0;
        }
    }
    if len > 0.0 {
        Some([sx / len, sy / len])
    } else {
        lines.iter().find_map(|l| l.first().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f64; 2], b: [f64; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-12 && (a[1] - b[1]).abs() < 1e-12
    }

    #[test]
    fn centroids() {
        let square = vec![
            [4.0, 52.0],
            [4.002, 52.0],
            [4.002, 52.001],
            [4.0, 52.001],
            [4.0, 52.0],
        ];
        let (c, area) = ring_centroid(&square).unwrap();
        assert!(close(c, [4.001, 52.0005]), "{c:?}");
        assert!((area - 0.002 * 0.001).abs() < 1e-15);
        // An L shape: centroid is pulled toward the heavier arm.
        let l = vec![
            [0.0, 0.0],
            [2.0, 0.0],
            [2.0, 1.0],
            [1.0, 1.0],
            [1.0, 2.0],
            [0.0, 2.0],
            [0.0, 0.0],
        ];
        let (c, _) = ring_centroid(&l).unwrap();
        assert!(close(c, [5.0 / 6.0, 5.0 / 6.0]), "{c:?}");
        // Largest polygon wins.
        let big = vec![square.clone()];
        let small = vec![vec![
            [5.0, 52.0],
            [5.0001, 52.0],
            [5.0001, 52.0001],
            [5.0, 52.0],
        ]];
        let c = centroid(&Geometry::MultiPolygon(vec![small, big])).unwrap();
        assert!(close(c, [4.001, 52.0005]));
        // A line's weighted midpoint.
        let c = centroid(&Geometry::LineString(vec![
            [0.0, 0.0],
            [2.0, 0.0],
            [2.0, 2.0],
        ]))
        .unwrap();
        assert!(close(c, [1.5, 0.5]), "{c:?}");
        // Degenerate ring: vertex mean.
        let (c, a) = ring_centroid(&[[1.0, 1.0], [1.0, 1.0], [1.0, 1.0]]).unwrap();
        assert_eq!((c, a), ([1.0, 1.0], 0.0));
    }

    #[test]
    fn address_text_in_dutch_order() {
        let tags = |kv: &[(&str, &str)]| {
            kv.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let t = tags(&[
            ("addr:street", "Damrak"),
            ("addr:housenumber", "1"),
            ("addr:city", "Amsterdam"),
            ("addr:postcode", "1012LG"),
        ]);
        assert_eq!(address_text(&t).as_deref(), Some("Damrak 1, Amsterdam"));
        let t = tags(&[
            ("addr:street", "Damrak"),
            ("addr:housenumber", "1"),
            ("addr:postcode", "1012LG"),
        ]);
        assert_eq!(address_text(&t).as_deref(), Some("Damrak 1, 1012LG"));
        let t = tags(&[("addr:street", "Damrak"), ("addr:housenumber", "1")]);
        assert_eq!(address_text(&t), None);
        let t = tags(&[("addr:housenumber", "1"), ("addr:city", "Amsterdam")]);
        assert_eq!(address_text(&t), None);
        let t = tags(&[
            ("addr:street", " "),
            ("addr:housenumber", "1"),
            ("addr:city", "A"),
        ]);
        assert_eq!(address_text(&t), None);
    }

    #[test]
    fn parses_osmium_features() {
        let line = r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[5.1179062,52.0979188]},"properties":{"addr:city":"Utrecht","addr:housenumber":"49","addr:street":"Lijsterstraat"}}"#;
        let f: Feature = serde_json::from_str(line).unwrap();
        assert_eq!(
            address_text(&f.properties).as_deref(),
            Some("Lijsterstraat 49, Utrecht")
        );
        assert_eq!(centroid(&f.geometry), Some([5.1179062, 52.0979188]));
    }
}
