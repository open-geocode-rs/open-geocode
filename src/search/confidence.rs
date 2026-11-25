use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    api::Accuracy,
    error::{Error, Result},
    pack,
    parse::lattice::CountryCode,
    search::Pass,
};

pub const MODEL_VERSION: u32 = 1;

const SIDECAR_EXTENSION: &str = "confidence.json";

const POOLED_RIDGE: f64 = 1.0;

const COUNTRY_PRIOR: f64 = 10.0;

const MAX_NEWTON_STEPS: usize = 100;
const CONVERGED_STEP: f64 = 1e-10;

const MAX_DROPPED_WORDS: u8 = 4;

pub const FEATURES: usize = 23;

pub const FEATURE_NAMES: [&str; FEATURES] = [
    "bias",
    "pass_street",
    "pass_lenient",
    "one_edit",
    "two_edits",
    "prefix",
    "accuracy_interpolated",
    "accuracy_street",
    "accuracy_coarse",
    "postcode_agrees",
    "postcode_contradicts",
    "postcode_unknown",
    "area_agrees",
    "area_contradicts",
    "area_unknown",
    "number_matched",
    "number_missed",
    "unit_matched",
    "unit_missed",
    "disputed",
    "dropped_words",
    "margin",
    "lower_rank",
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Agreement {
    #[default]
    Unstated,
    Agrees,
    Contradicts,
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Features {
    /// The country the answer lies in, whose model rates it; `None` rates it
    /// with the pooled model.
    pub country: Option<CountryCode>,
    pub pass: Pass,
    pub edits: u8,
    pub prefix: bool,
    pub accuracy: Option<Accuracy>,
    pub postcode: Agreement,
    pub area: Agreement,
    pub number: Option<bool>,
    pub unit: Option<bool>,
    pub disputed: bool,
    pub dropped_words: u8,
    /// How far its score stands above the next answer's, as a share of its
    /// own: 1 when no answer follows.
    pub margin: f32,
    pub lower_rank: bool,
}

impl Features {
    pub fn inputs(&self) -> [f64; FEATURES] {
        let flag = |value: bool| f64::from(u8::from(value));
        let agreement = |agreement: Agreement| {
            [
                flag(agreement == Agreement::Agrees),
                flag(agreement == Agreement::Contradicts),
                flag(agreement == Agreement::Unknown),
            ]
        };
        let [postcode_agrees, postcode_contradicts, postcode_unknown] = agreement(self.postcode);
        let [area_agrees, area_contradicts, area_unknown] = agreement(self.area);
        [
            1.0,
            flag(self.pass == Pass::Street),
            flag(self.pass == Pass::Lenient),
            flag(self.edits == 1),
            flag(self.edits >= 2),
            flag(self.prefix),
            flag(self.accuracy == Some(Accuracy::Interpolated)),
            flag(self.accuracy == Some(Accuracy::Street)),
            flag(matches!(
                self.accuracy,
                Some(Accuracy::Centroid | Accuracy::Approximate)
            )),
            postcode_agrees,
            postcode_contradicts,
            postcode_unknown,
            area_agrees,
            area_contradicts,
            area_unknown,
            flag(self.number == Some(true)),
            flag(self.number == Some(false)),
            flag(self.unit == Some(true)),
            flag(self.unit == Some(false)),
            flag(self.disputed),
            f64::from(self.dropped_words.min(MAX_DROPPED_WORDS)),
            f64::from(self.margin.clamp(0.0, 1.0)),
            flag(self.lower_rank),
        ]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Example {
    pub features: Features,
    pub correct: bool,
}

type Weights = [f64; FEATURES];

#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pooled: Weights,
    countries: BTreeMap<CountryCode, Weights>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    version: u32,
    pack: String,
    features: Vec<String>,
    pooled: Vec<f64>,
    countries: BTreeMap<String, Vec<f64>>,
}

impl Model {
    pub fn new(pooled: Weights, countries: BTreeMap<CountryCode, Weights>) -> Self {
        Self { pooled, countries }
    }

    /// The chance an answer with `features` is right, in `[0, 1]`.
    pub fn rate(&self, features: &Features) -> f32 {
        let weights = features
            .country
            .and_then(|country| self.countries.get(&country))
            .unwrap_or(&self.pooled);
        sigmoid(dot(weights, &features.inputs())) as f32
    }

    pub fn fit(examples: &[Example]) -> Self {
        let mut free = [POOLED_RIDGE; FEATURES];
        free[0] = 0.0;
        let pooled = fit_logistic(&examples.iter(), &[0.0; FEATURES], &free);
        let mut by_country = BTreeMap::<CountryCode, Vec<&Example>>::new();
        for example in examples {
            if let Some(country) = example.features.country {
                by_country.entry(country).or_default().push(example);
            }
        }
        let countries = by_country
            .into_iter()
            .map(|(country, examples)| {
                let weights =
                    fit_logistic(&examples.into_iter(), &pooled, &[COUNTRY_PRIOR; FEATURES]);
                (country, weights)
            })
            .collect();
        Self { pooled, countries }
    }

    pub fn countries(&self) -> impl Iterator<Item = CountryCode> + '_ {
        self.countries.keys().copied()
    }

    pub fn path(pack_file: &Path) -> PathBuf {
        pack_file.with_extension(SIDECAR_EXTENSION)
    }

    /// The model fit to `pack`, `None` when it has none. Fails on a model
    /// fit to another Pack or written for other features.
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
        if stored.version != MODEL_VERSION {
            return Err(refit("is of another format version"));
        }
        if stored.features != FEATURE_NAMES {
            return Err(refit("rates other features"));
        }
        if stored.pack != fingerprint(pack) {
            return Err(refit("was fit to another Pack"));
        }
        let weights = |values: Vec<f64>| -> Result<Weights> {
            Weights::try_from(values).map_err(|_| refit("holds the wrong number of weights"))
        };
        let pooled = weights(stored.pooled)?;
        let countries = stored
            .countries
            .into_iter()
            .map(|(country, values)| {
                let country =
                    CountryCode::parse(&country).ok_or_else(|| refit("names no country"))?;
                Ok((country, weights(values)?))
            })
            .collect::<Result<_>>()?;
        Ok(Some(Self { pooled, countries }))
    }

    /// Write the model beside `pack`'s file, replacing any before it whole:
    /// a server opening the Pack meanwhile reads the old model or the new
    /// one, never part of either.
    pub fn save(&self, pack: &pack::Reader) -> Result<PathBuf> {
        let path = Self::path(pack.path());
        let stored = Stored {
            version: MODEL_VERSION,
            pack: fingerprint(pack),
            features: FEATURE_NAMES.iter().map(|name| name.to_string()).collect(),
            pooled: self.pooled.to_vec(),
            countries: self
                .countries
                .iter()
                .map(|(country, weights)| (country.to_string(), weights.to_vec()))
                .collect(),
        };
        let mut text = serde_json::to_vec_pretty(&stored)?;
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

fn dot(weights: &Weights, inputs: &[f64; FEATURES]) -> f64 {
    weights
        .iter()
        .zip(inputs)
        .map(|(weight, input)| weight * input)
        .sum()
}

fn sigmoid(value: f64) -> f64 {
    1.0 / (1.0 + (-value).exp())
}

fn fit_logistic<'a>(
    examples: &(impl Iterator<Item = &'a Example> + Clone),
    prior: &Weights,
    strength: &Weights,
) -> Weights {
    let mut weights = *prior;
    for _ in 0..MAX_NEWTON_STEPS {
        let mut gradient = [0.0; FEATURES];
        let mut hessian = [[0.0; FEATURES]; FEATURES];
        for (index, strength) in strength.iter().enumerate() {
            gradient[index] = strength * (weights[index] - prior[index]);
            hessian[index][index] = strength + 1e-9;
        }
        for example in examples.clone() {
            let inputs = example.features.inputs();
            let chance = sigmoid(dot(&weights, &inputs));
            let error = chance - f64::from(u8::from(example.correct));
            let curvature = chance * (1.0 - chance);
            for row in 0..FEATURES {
                gradient[row] += error * inputs[row];
                for column in 0..FEATURES {
                    hessian[row][column] += curvature * inputs[row] * inputs[column];
                }
            }
        }
        let step = solve(hessian, gradient);
        let mut largest = 0.0f64;
        for (weight, step) in weights.iter_mut().zip(step) {
            *weight -= step;
            largest = largest.max(step.abs());
        }
        if largest < CONVERGED_STEP {
            break;
        }
    }
    weights
}

fn solve(mut matrix: [[f64; FEATURES]; FEATURES], mut vector: Weights) -> Weights {
    for column in 0..FEATURES {
        let pivot = (column..FEATURES)
            .max_by(|&left, &right| {
                matrix[left][column]
                    .abs()
                    .total_cmp(&matrix[right][column].abs())
            })
            .expect("a column has rows");
        matrix.swap(column, pivot);
        vector.swap(column, pivot);
        let (upper, lower) = matrix.split_at_mut(column + 1);
        let pivot_row = &upper[column];
        for (offset, row) in lower.iter_mut().enumerate() {
            let factor = row[column] / pivot_row[column];
            if factor == 0.0 {
                continue;
            }
            for (value, pivot) in row[column..].iter_mut().zip(&pivot_row[column..]) {
                *value -= factor * pivot;
            }
            vector[column + 1 + offset] -= factor * vector[column];
        }
    }
    let mut solution = [0.0; FEATURES];
    for row in (0..FEATURES).rev() {
        let known = (row + 1..FEATURES)
            .map(|index| matrix[row][index] * solution[index])
            .sum::<f64>();
        solution[row] = (vector[row] - known) / matrix[row][row];
    }
    solution
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Draws(u64);

    impl Draws {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut value = self.0;
            value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (value ^ (value >> 31)) as f64 / u64::MAX as f64
        }
    }

    fn synthetic(count: usize, country: Option<CountryCode>, seed: u64) -> Vec<Example> {
        let mut draws = Draws(seed);
        (0..count)
            .map(|_| {
                let features = Features {
                    country,
                    margin: draws.next() as f32,
                    edits: u8::from(draws.next() < 0.3),
                    ..Features::default()
                };
                let chance = sigmoid(
                    -1.0 + 4.0 * f64::from(features.margin) - 2.0 * f64::from(features.edits),
                );
                Example {
                    correct: draws.next() < chance,
                    features,
                }
            })
            .collect()
    }

    #[test]
    fn a_fit_recovers_a_monotone_law() {
        let model = Model::fit(&synthetic(4_000, None, 7));
        let margin = FEATURE_NAMES
            .iter()
            .position(|name| *name == "margin")
            .expect("margin");
        let typo = FEATURE_NAMES
            .iter()
            .position(|name| *name == "one_edit")
            .expect("one edit");
        assert!(
            (model.pooled[margin] - 4.0).abs() < 0.8,
            "{:?}",
            model.pooled
        );
        assert!((model.pooled[typo] + 2.0).abs() < 0.6, "{:?}", model.pooled);
        assert!((model.pooled[0] + 1.0).abs() < 0.5, "{:?}", model.pooled);

        let mut previous = 0.0;
        for step in 0..=10 {
            let features = Features {
                margin: step as f32 / 10.0,
                ..Features::default()
            };
            let rated = model.rate(&features);
            assert!((0.0..=1.0).contains(&rated));
            assert!(rated > previous, "confidence rises with the margin");
            let typo = model.rate(&Features {
                edits: 1,
                ..features
            });
            assert!(typo < rated, "a typo lowers it");
            previous = rated;
        }
    }

    #[test]
    fn a_country_of_few_examples_keeps_the_pooled_model() {
        let ca = CountryCode::parse("CA");
        let nz = CountryCode::parse("NZ");
        let mut examples = synthetic(3_000, ca, 1);
        examples.extend((0..5).map(|index| Example {
            features: Features {
                country: nz,
                margin: index as f32 / 10.0,
                ..Features::default()
            },
            correct: true,
        }));
        let model = Model::fit(&examples);
        let features = Features {
            margin: 0.2,
            ..Features::default()
        };
        let pooled = model.rate(&features);
        let new_zealand = model.rate(&Features {
            country: nz,
            ..features
        });
        let canada = model.rate(&Features {
            country: ca,
            ..features
        });
        assert!(
            (new_zealand - pooled).abs() < 0.15,
            "{new_zealand} strays from {pooled}"
        );
        assert!((canada - pooled).abs() < 0.05, "{canada} {pooled}");
        assert_eq!(
            model.countries().collect::<Vec<_>>(),
            [ca.unwrap(), nz.unwrap()]
        );
    }
}
