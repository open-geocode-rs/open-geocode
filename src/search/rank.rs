use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    confidence::Agreement,
    error::{Error, Result},
    pack,
    record::Layer,
    search::{Pass, TextSearchHit},
};

pub const RANKER_VERSION: u32 = 1;
const SIDECAR_EXTENSION: &str = "ranker.json";
const TREES: usize = 300;
const DEPTH: usize = 5;
const LEARNING_RATE: f64 = 0.1;
const MIN_LEAF: usize = 32;
const BINS: usize = 64;
const UNEXPLAINED: f32 = -64.0;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Signals {
    pub house_level: bool,
    pub stated: bool,
    pub postcode_agreement: u8,
    pub name_words: u8,
    pub place_rank: u8,
    pub extra_street_words: u8,
    pub unit: u8,
    pub extra_name_words: u8,
    pub missing_query_words: u8,
    pub inexact_query_words: u8,
    pub focus_band: u8,
    pub precision: u8,
    pub poi: bool,
    pub demoted: bool,
    pub window_rank: u16,
    pub reading: u8,
    pub parse_gap: f32,
    pub explained: Option<f32>,
    pub confirmed: bool,
    pub contradicted: bool,
    pub text_share: f32,
    pub ranked: f32,
}

pub const INPUTS: usize = 41;

pub const INPUT_NAMES: [&str; INPUTS] = [
    "edits",
    "pass_street",
    "pass_lenient",
    "postcode_agrees",
    "postcode_contradicts",
    "postcode_unknown",
    "area_agrees",
    "area_contradicts",
    "area_unknown",
    "number_stated",
    "number_missing",
    "unit_stated",
    "unit_missing",
    "disputed",
    "dropped_words",
    "fallback",
    "layer_address",
    "layer_street",
    "layer_interpolation",
    "layer_poi",
    "layer_postcode",
    "house_level",
    "stated",
    "postcode_agreement",
    "name_words",
    "place_rank",
    "extra_street_words",
    "unit_rank",
    "extra_name_words",
    "missing_query_words",
    "inexact_query_words",
    "focus_band",
    "precision",
    "demoted",
    "window_rank",
    "reading",
    "parse_gap",
    "explained",
    "confirmed",
    "contradicted",
    "text_share",
];

pub fn inputs(hit: &TextSearchHit) -> [f32; INPUTS] {
    let flag = |value: bool| f32::from(u8::from(value));
    let features = &hit.features;
    let signals = &hit.signals;
    let layer = hit.record.layer.as_str();
    [
        f32::from(features.edits),
        flag(features.pass == Pass::Street),
        flag(features.pass == Pass::Lenient),
        flag(features.postcode == Agreement::Agrees),
        flag(features.postcode == Agreement::Contradicts),
        flag(features.postcode == Agreement::Unknown),
        flag(features.area == Agreement::Agrees),
        flag(features.area == Agreement::Contradicts),
        flag(features.area == Agreement::Unknown),
        flag(features.number == Some(true)),
        flag(features.number == Some(false)),
        flag(features.unit == Some(true)),
        flag(features.unit == Some(false)),
        flag(features.disputed),
        f32::from(features.dropped_words),
        flag(hit.fallback),
        flag(layer == Layer::Address.as_str()),
        flag(layer == Layer::Street.as_str()),
        flag(layer == Layer::Interpolation.as_str()),
        flag(layer == Layer::Poi.as_str()),
        flag(layer == Layer::Postcode.as_str()),
        flag(signals.house_level),
        flag(signals.stated),
        f32::from(signals.postcode_agreement),
        f32::from(signals.name_words),
        f32::from(signals.place_rank),
        f32::from(signals.extra_street_words),
        f32::from(signals.unit),
        f32::from(signals.extra_name_words),
        f32::from(signals.missing_query_words),
        f32::from(signals.inexact_query_words),
        f32::from(signals.focus_band),
        f32::from(signals.precision),
        flag(signals.demoted),
        f32::from(signals.window_rank),
        f32::from(signals.reading),
        signals.parse_gap,
        signals.explained.unwrap_or(UNEXPLAINED).max(UNEXPLAINED),
        flag(signals.confirmed),
        flag(signals.contradicted),
        signals.text_share,
    ]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Node {
    input: Option<u16>,
    threshold: f32,
    left: u32,
    right: u32,
    value: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Tree {
    nodes: Vec<Node>,
}

impl Tree {
    fn score(&self, inputs: &[f32; INPUTS]) -> f32 {
        let mut at = 0;
        loop {
            let node = &self.nodes[at];
            let Some(input) = node.input else {
                return node.value;
            };
            at = match inputs[usize::from(input)] <= node.threshold {
                true => node.left,
                false => node.right,
            } as usize;
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub hits: Vec<([f32; INPUTS], bool)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ranker {
    trees: Vec<Tree>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    version: u32,
    pack: String,
    inputs: Vec<String>,
    trees: Vec<Tree>,
}

impl Ranker {
    pub fn score(&self, hit: &TextSearchHit) -> f32 {
        self.score_inputs(&inputs(hit))
    }

    pub fn score_inputs(&self, inputs: &[f32; INPUTS]) -> f32 {
        self.trees.iter().map(|tree| tree.score(inputs)).sum()
    }

    pub fn fit(queries: &[Query]) -> Self {
        let queries = queries
            .iter()
            .filter(|query| {
                query.hits.iter().any(|(_, right)| *right)
                    && query.hits.iter().any(|(_, right)| !*right)
            })
            .collect::<Vec<_>>();
        let rows = queries
            .iter()
            .flat_map(|query| query.hits.iter().map(|(inputs, _)| *inputs))
            .collect::<Vec<_>>();
        let thresholds = thresholds(&rows);
        let mut scores = vec![0.0f64; rows.len()];
        let mut trees = Vec::with_capacity(TREES);
        for _ in 0..TREES {
            let (lambdas, weights) = lambdas(&queries, &scores);
            let tree = grow(&rows, &lambdas, &weights, &thresholds);
            for (score, row) in scores.iter_mut().zip(&rows) {
                *score += f64::from(tree.score(row));
            }
            trees.push(tree);
        }
        Self { trees }
    }

    pub fn path(pack_file: &Path) -> PathBuf {
        pack_file.with_extension(SIDECAR_EXTENSION)
    }

    /// The ranker fit to `pack`, `None` when it has none. Fails on one fit
    /// to another Pack or written for other inputs.
    pub fn load(pack: &pack::Reader) -> Result<Option<Self>> {
        let path = Self::path(pack.path());
        let text = match std::fs::read(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(Error::IoPath {
                    action: "failed to read",
                    path,
                    source,
                });
            }
        };
        let stored: Stored = serde_json::from_slice(&text)?;
        let refit = |why: &str| {
            Error::Inconsistent(format!(
                "{} {why}; run `geocode calibrate` on the Pack again",
                path.display()
            ))
        };
        if stored.version != RANKER_VERSION {
            return Err(refit("is of another format version"));
        }
        if stored.inputs != INPUT_NAMES {
            return Err(refit("ranks by other inputs"));
        }
        if stored.pack != fingerprint(pack) {
            return Err(refit("was fit to another Pack"));
        }
        Ok(Some(Self {
            trees: stored.trees,
        }))
    }

    pub fn save(&self, pack: &pack::Reader) -> Result<PathBuf> {
        let path = Self::path(pack.path());
        let stored = Stored {
            version: RANKER_VERSION,
            pack: fingerprint(pack),
            inputs: INPUT_NAMES.iter().map(|name| name.to_string()).collect(),
            trees: self.trees.clone(),
        };
        let mut text = serde_json::to_vec(&stored)?;
        text.push(b'\n');
        let pending = path.with_extension(format!("{SIDECAR_EXTENSION}.{}", uuid::Uuid::new_v4()));
        let failed = |action, source| Error::IoPath {
            action,
            path: path.clone(),
            source,
        };
        std::fs::write(&pending, &text).map_err(|source| failed("failed to write", source))?;
        std::fs::rename(&pending, &path).map_err(|source| {
            let _ = std::fs::remove_file(&pending);
            failed("failed to replace", source)
        })?;
        Ok(path)
    }
}

fn fingerprint(pack: &pack::Reader) -> String {
    format!("{:016x}", pack.container().fingerprint())
}

fn thresholds(rows: &[[f32; INPUTS]]) -> Vec<Vec<f32>> {
    (0..INPUTS)
        .map(|input| {
            let mut values = rows.iter().map(|row| row[input]).collect::<Vec<_>>();
            values.sort_by(f32::total_cmp);
            values.dedup();
            if values.len() <= BINS {
                return values.windows(2).map(|pair| pair[0]).collect();
            }
            let mut cuts = (1..BINS)
                .map(|share| values[share * values.len() / BINS])
                .collect::<Vec<_>>();
            cuts.dedup();
            cuts
        })
        .collect()
}

fn lambdas(queries: &[&Query], scores: &[f64]) -> (Vec<f64>, Vec<f64>) {
    let mut lambdas = vec![0.0; scores.len()];
    let mut weights = vec![0.0; scores.len()];
    let mut start = 0;
    for query in queries {
        let count = query.hits.len();
        let scores = &scores[start..start + count];
        let mut order = (0..count).collect::<Vec<_>>();
        order.sort_by(|left, right| scores[*right].total_cmp(&scores[*left]));
        let mut rank = vec![0; count];
        for (position, index) in order.iter().enumerate() {
            rank[*index] = position;
        }
        let discount = |position: usize| 1.0 / ((position + 2) as f64).log2();
        let answers = query.hits.iter().filter(|(_, right)| *right).count();
        let ideal = (0..answers).map(discount).sum::<f64>();
        for right in (0..count).filter(|index| query.hits[*index].1) {
            for wrong in (0..count).filter(|index| !query.hits[*index].1) {
                let swing = (discount(rank[right]) - discount(rank[wrong])).abs() / ideal;
                let likely = 1.0 / (1.0 + (scores[right] - scores[wrong]).exp());
                lambdas[start + right] += swing * likely;
                lambdas[start + wrong] -= swing * likely;
                let curvature = swing * likely * (1.0 - likely);
                weights[start + right] += curvature;
                weights[start + wrong] += curvature;
            }
        }
        start += count;
    }
    (lambdas, weights)
}

fn grow(rows: &[[f32; INPUTS]], lambdas: &[f64], weights: &[f64], thresholds: &[Vec<f32>]) -> Tree {
    let mut tree = Tree { nodes: Vec::new() };
    let all = (0..rows.len()).collect::<Vec<_>>();
    split(&mut tree, rows, lambdas, weights, thresholds, all, 0);
    tree
}

fn split(
    tree: &mut Tree,
    rows: &[[f32; INPUTS]],
    lambdas: &[f64],
    weights: &[f64],
    thresholds: &[Vec<f32>],
    members: Vec<usize>,
    depth: usize,
) -> u32 {
    let at = tree.nodes.len() as u32;
    let sum = members.iter().map(|&row| lambdas[row]).sum::<f64>();
    let weight = members.iter().map(|&row| weights[row]).sum::<f64>();
    let value = match weight > 1e-12 {
        true => (LEARNING_RATE * sum / weight) as f32,
        false => 0.0,
    };
    tree.nodes.push(Node {
        input: None,
        threshold: 0.0,
        left: 0,
        right: 0,
        value,
    });
    if depth >= DEPTH || members.len() < 2 * MIN_LEAF {
        return at;
    }
    let base = sum * sum / members.len() as f64;
    let mut best = None::<(f64, usize, f32)>;
    for (input, cuts) in thresholds.iter().enumerate() {
        if cuts.is_empty() {
            continue;
        }
        let mut bins = vec![(0.0f64, 0usize); cuts.len() + 1];
        for &row in &members {
            let value = rows[row][input];
            let bin = cuts.partition_point(|cut| *cut < value);
            bins[bin].0 += lambdas[row];
            bins[bin].1 += 1;
        }
        let (mut left_sum, mut left_count) = (0.0, 0);
        for (bin, cut) in cuts.iter().enumerate() {
            left_sum += bins[bin].0;
            left_count += bins[bin].1;
            let right_count = members.len() - left_count;
            if left_count < MIN_LEAF || right_count < MIN_LEAF {
                continue;
            }
            let right_sum = sum - left_sum;
            let gain = left_sum * left_sum / left_count as f64
                + right_sum * right_sum / right_count as f64
                - base;
            if best.is_none_or(|(held, ..)| gain > held) {
                best = Some((gain, input, *cut));
            }
        }
    }
    let Some((gain, input, threshold)) = best else {
        return at;
    };
    if gain <= 1e-12 {
        return at;
    }
    let (left, right): (Vec<_>, Vec<_>) = members
        .into_iter()
        .partition(|&row| rows[row][input] <= threshold);
    let left = split(tree, rows, lambdas, weights, thresholds, left, depth + 1);
    let right = split(tree, rows, lambdas, weights, thresholds, right, depth + 1);
    let node = &mut tree.nodes[at as usize];
    node.input = Some(input as u16);
    node.threshold = threshold;
    node.left = left;
    node.right = right;
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(edits: f32, noise: f32) -> [f32; INPUTS] {
        let mut row = [0.0; INPUTS];
        row[0] = edits;
        row[INPUTS - 1] = noise;
        row
    }

    #[test]
    fn learns_to_rank_the_answer_first() {
        let queries = (0..400)
            .map(|query| Query {
                hits: (0..4)
                    .map(|hit| {
                        let right = hit == query % 4;
                        let noise = ((query * 7 + hit * 13) % 10) as f32;
                        (row(if right { 0.0 } else { 1.0 }, noise), right)
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        let ranker = Ranker::fit(&queries);
        assert!(ranker.score_inputs(&row(0.0, 5.0)) > ranker.score_inputs(&row(1.0, 5.0)));
    }
}
