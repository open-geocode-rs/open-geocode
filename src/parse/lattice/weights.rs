//! The parser's linear model.
//!
//! An edge scores the sum of the weights of its features under its label; a
//! path adds the transition between each pair of consecutive labels, from the
//! global table plus the table of the country it is decoded as. Weights are
//! learned by [`super::train`] when the Pack is built and stored in it
//! (`parse/weights`), in the format [`Weights::to_bytes`] writes. A Pack too
//! small to train on carries none, and [`Weights::built_in`] holds hand-set
//! weights that read the common address shapes for it.
//!
//! ```text
//! magic "OGPARSEW" | version u32 | schema u64 | features u32 | labels u32
//! | states u32 | edge f32 x features*labels | global f32 x states^2
//! | countries u32 | per country: code u16, f32 x states^2
//! ```
//!
//! Every number is little-endian.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};

use super::{
    CountryCode,
    decode::{END, LABELS, START, STATES, Span},
    features,
};
use crate::{container::Container, parse::types::Label};

/// The Pack section that carries trained weights.
pub const WEIGHTS_SECTION: &str = "parse/weights";
/// Format version of the weights, in the file and in the Pack section.
pub const WEIGHTS_VERSION: u32 = 1;
const MAGIC: &[u8; 8] = b"OGPARSEW";

/// Edge and transition weights of the parser.
#[derive(Debug, Clone, PartialEq)]
pub struct Weights {
    /// `edge[feature * LABELS + label]`.
    pub(super) edge: Vec<f32>,
    /// `global[previous * STATES + next]`, with the start and end states.
    pub(super) global: Vec<f32>,
    /// Per-country transitions added to the global ones.
    pub(super) countries: BTreeMap<CountryCode, Vec<f32>>,
}

/// The transitions of one decoding: global plus one country's.
#[derive(Debug, Clone, Copy)]
pub struct Transitions<'a> {
    global: &'a [f32],
    country: Option<&'a [f32]>,
}

impl Transitions<'_> {
    /// The weight of moving from state `previous` to state `next`.
    pub fn get(&self, previous: usize, next: usize) -> f32 {
        let index = previous * STATES + next;
        self.global[index] + self.country.map_or(0.0, |country| country[index])
    }
}

impl Weights {
    /// All weights zero.
    /// The mean of `all`, each table entry by entry. A country one of them
    /// has no table for counts as zeros there.
    pub(super) fn mean<'a>(all: impl Iterator<Item = &'a Weights>) -> Self {
        let mut sum = Self::zero();
        let mut count = 0usize;
        for weights in all {
            count += 1;
            for (total, value) in sum.edge.iter_mut().zip(&weights.edge) {
                *total += value;
            }
            for (total, value) in sum.global.iter_mut().zip(&weights.global) {
                *total += value;
            }
            for (country, table) in &weights.countries {
                let totals = sum
                    .countries
                    .entry(*country)
                    .or_insert_with(|| vec![0.0; table.len()]);
                for (total, value) in totals.iter_mut().zip(table) {
                    *total += value;
                }
            }
        }
        let scale = 1.0 / count.max(1) as f32;
        for value in sum
            .edge
            .iter_mut()
            .chain(sum.global.iter_mut())
            .chain(sum.countries.values_mut().flatten())
        {
            *value *= scale;
        }
        sum
    }

    pub fn zero() -> Self {
        Self {
            edge: vec![0.0; features::COUNT * LABELS],
            global: vec![0.0; STATES * STATES],
            countries: BTreeMap::new(),
        }
    }

    /// Hand-set weights for a Pack nothing was trained for. They read a house
    /// number before or after its street, unit text, a street ending or
    /// starting with its type, names the Pack's dictionaries know, and a
    /// postcode some country's pattern matches.
    pub fn built_in() -> Self {
        use Label::*;
        let mut weights = Self::zero();
        let mut set = |name: &str, values: &[(Label, f32)]| {
            let feature = features::by_name(name) as usize;
            for (label, value) in values {
                weights.edge[feature * LABELS + *label as usize] = *value;
            }
        };
        set(
            "bias:bias",
            &[
                (HouseNumber, -1.0),
                (Unit, -2.0),
                (Street, 0.0),
                (Locality, -0.5),
                // No hand-set feature tells a district from a locality.
                (District, -4.0),
                (Region, -2.0),
                (Postcode, -2.0),
                (Country, -2.0),
                (Other, -0.5),
            ],
        );
        for tokens in 2..=5 {
            set(
                &format!("span_tokens:{tokens}"),
                &[(HouseNumber, -20.0), (Unit, -20.0), (Other, -2.0)],
            );
        }
        let not_a_name = [
            (Street, -2.0),
            (Locality, -4.0),
            (District, -4.0),
            (Region, -4.0),
            (Country, -4.0),
        ];
        set(
            "shape:digits",
            &[
                &[(HouseNumber, 3.0), (Unit, 1.0), (Other, -0.5)],
                &not_a_name[..],
            ]
            .concat(),
        );
        set(
            "shape:digits_letter",
            &[
                &[(HouseNumber, 2.5), (Unit, 1.5), (Other, -0.5)],
                &not_a_name[..],
            ]
            .concat(),
        );
        set(
            "shape:letters_digits",
            &[
                &[(Unit, 1.5), (Other, 0.5), (HouseNumber, -1.0)],
                &not_a_name[..],
            ]
            .concat(),
        );
        set(
            "shape:mixed",
            &[
                (Unit, 0.5),
                (Other, 0.5),
                (Postcode, 1.0),
                (Street, -0.5),
                (HouseNumber, -1.0),
                (Locality, -2.0),
            ],
        );
        set(
            "shape:alpha",
            &[
                (HouseNumber, -6.0),
                (Unit, -3.0),
                (Postcode, -4.0),
                (Street, 1.0),
                (Locality, 0.3),
                (Other, 0.5),
            ],
        );
        set("number_position:first", &[(HouseNumber, 1.0)]);
        set(
            "number_position:last",
            &[(HouseNumber, -0.5), (Postcode, 0.5)],
        );
        set(
            "number_position:after_unit_word",
            &[(Unit, 4.0), (HouseNumber, -2.0)],
        );
        set(
            "number_position:before_number",
            &[(Unit, 2.5), (HouseNumber, -1.0)],
        );
        set("vocabulary:street_type", &[(Street, 2.0), (Other, -0.5)]);
        set(
            "vocabulary:unit_word",
            &[(Unit, 6.0), (Street, -1.0), (Locality, -1.0)],
        );
        set("vocabulary:street_type_suffix", &[(Street, 2.0)]);
        set("vocabulary:unknown_word", &[(Street, 0.5), (Other, 0.3)]);
        // One character of a script written without spaces is seldom a whole
        // name; the names it is part of say what it is.
        set("vocabulary:unspaced", &[(Street, -2.5), (Other, -0.8)]);
        set("neighbour:prev_number", &[(Street, 1.0)]);
        set("neighbour:next_street_type", &[(Street, 1.5)]);
        set(
            "neighbour:prev_street_type",
            &[(Street, 0.3), (Locality, 0.5)],
        );
        set("neighbour:prev_unit_word", &[(Unit, 3.0)]);
        set("neighbour:next_number", &[(Street, 0.3)]);
        set(
            "position:break_before",
            &[
                (Street, -2.0),
                (Locality, 1.5),
                (Region, 1.0),
                (Postcode, 1.0),
                (Country, 1.0),
                (Other, -0.5),
            ],
        );
        set(
            "position:first",
            &[(Locality, -0.5), (Region, -1.0), (Country, -1.0)],
        );
        set(
            "position:last",
            &[
                (Locality, 0.5),
                (Region, 0.5),
                (Postcode, 1.0),
                (Country, 1.0),
            ],
        );
        set("position:whole_query", &[(Other, 1.0)]);
        set(
            "position:break_inside",
            &[(Street, -3.0), (Locality, -3.0), (Region, -3.0)],
        );
        for (field, label) in [
            ("locality", Locality),
            ("region", Region),
            ("street", Street),
            ("country", Country),
        ] {
            let exact = if label == Street { 3.0 } else { 3.5 };
            for distance in 0..3 {
                set(
                    &format!("dict_distance:{field}:{distance}"),
                    &[(label, exact - distance as f32)],
                );
            }
            for tokens in 2..=5 {
                set(
                    &format!("dict_tokens:{field}:{tokens}"),
                    &[(label, tokens as f32)],
                );
            }
            for (bucket, value) in ["1", "2-9", "10-99", "100-999", "1000-9999", "10000+"]
                .into_iter()
                .zip([0.0, 0.2, 0.4, 0.6, 0.8, 1.0])
            {
                set(
                    &format!("dict_frequency:{field}:{bucket}"),
                    &[(label, value)],
                );
            }
            set(&format!("dict_in_country:{field}"), &[(label, 0.5)]);
            // A character of a known name, read one at a time.
            for part in ["begin", "inside"] {
                set(&format!("dict_cover:{field}:{part}"), &[(label, 1.0)]);
            }
        }
        set("dict_short:locality:1-2", &[(Locality, -3.0)]);
        set("dict_short:locality:3", &[(Locality, -1.0)]);
        set("dict_short:region:1-2", &[(Region, -0.5)]);
        set("dict_short:street:1-2", &[(Street, -2.0)]);
        set("dict_short:street:3", &[(Street, -0.5)]);
        set("dict_short:country:1-2", &[(Country, -1.0)]);
        set("joined:number_parts", &[(HouseNumber, 30.0)]);
        set("joined:hyphen_after", &[(Unit, 1.5), (HouseNumber, -0.5)]);
        set("joined:hyphen_before", &[(HouseNumber, 0.5)]);
        // A place or postcode the Pack does not explain is seldom one.
        set("grounding:no_locality", &[(Locality, -0.5)]);
        set("grounding:no_region", &[(Region, -1.0)]);
        set("grounding:no_country", &[(Country, -1.0)]);
        set("grounding:no_postcode", &[(Postcode, -1.0)]);
        for (tokens, value) in [("1", 3.0), ("2", 4.0), ("3", 4.0)] {
            set(&format!("postcode_any:{tokens}"), &[(Postcode, value)]);
            set(&format!("postcode_country:{tokens}"), &[(Postcode, 1.0)]);
        }

        let mut transition = |previous: usize, next: usize, value: f32| {
            weights.global[previous * STATES + next] = value;
        };
        let state = |label: Label| label as usize;
        for (previous, next, value) in [
            (START, state(HouseNumber), 1.0),
            (START, state(Unit), 0.5),
            (START, state(Street), 0.5),
            (START, state(Postcode), -1.0),
            (START, state(Region), -1.0),
            (START, state(Country), -2.0),
            (state(HouseNumber), state(Street), 2.0),
            (state(HouseNumber), state(HouseNumber), -10.0),
            (state(HouseNumber), state(Unit), 0.5),
            (state(HouseNumber), END, -0.5),
            (state(Unit), state(Unit), 1.0),
            (state(Unit), state(HouseNumber), 1.5),
            (state(Street), state(Street), 1.0),
            (state(Street), state(HouseNumber), 0.5),
            (state(Street), state(Locality), 0.5),
            (state(Street), state(Unit), 0.2),
            (state(Street), END, 0.5),
            (state(Locality), state(Locality), 0.5),
            (state(Locality), state(Region), 1.0),
            (state(Locality), state(Postcode), 1.0),
            (state(Locality), state(Street), -0.5),
            (state(Locality), END, 0.5),
            (state(Region), state(Postcode), 1.0),
            (state(Region), state(Country), 1.0),
            (state(Region), state(Street), -1.0),
            (state(Region), END, 0.5),
            (state(Postcode), state(Locality), 0.5),
            (state(Postcode), state(Country), 1.0),
            (state(Postcode), state(Street), -1.0),
            (state(Postcode), END, 0.5),
            (state(Country), END, 0.5),
            (state(Other), state(Other), 1.0),
            (state(Other), state(Locality), 0.5),
            (state(Other), state(Street), -0.5),
        ] {
            transition(previous, next, value);
        }
        weights
    }

    /// Whether the weights carry a transition table of `country`.
    pub fn knows(&self, country: CountryCode) -> bool {
        self.countries.contains_key(&country)
    }

    /// The countries the weights carry a transition table of.
    pub fn countries(&self) -> impl Iterator<Item = CountryCode> + '_ {
        self.countries.keys().copied()
    }

    /// The transitions of a decoding as `country`, or the global ones.
    pub fn transitions(&self, country: Option<CountryCode>) -> Transitions<'_> {
        Transitions {
            global: &self.global,
            country: country
                .and_then(|country| self.countries.get(&country))
                .map(Vec::as_slice),
        }
    }

    /// The score of `span` labelled `label`, decoded as `country`.
    pub(super) fn edge(&self, span: &Span, label: Label, country: Option<CountryCode>) -> f32 {
        let label = label as usize;
        span.features_in(country)
            .map(|feature| self.edge[feature as usize * LABELS + label])
            .sum()
    }

    /// The weights a Pack carries, `None` when it carries none.
    pub fn from_container(container: &Container) -> Result<Option<Self>> {
        if !container.sections().contains_key(WEIGHTS_SECTION) {
            return Ok(None);
        }
        let bytes = container.section(WEIGHTS_SECTION, WEIGHTS_VERSION)?;
        Self::from_bytes(&bytes)
            .with_context(|| format!("invalid Pack section {WEIGHTS_SECTION}"))
            .map(Some)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(
            40 + 4 * (self.edge.len() + self.global.len() * (1 + self.countries.len())),
        );
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&WEIGHTS_VERSION.to_le_bytes());
        bytes.extend_from_slice(&features::schema().to_le_bytes());
        for count in [features::COUNT, LABELS, STATES] {
            bytes.extend_from_slice(&(count as u32).to_le_bytes());
        }
        let floats = |bytes: &mut Vec<u8>, values: &[f32]| {
            for value in values {
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        };
        floats(&mut bytes, &self.edge);
        floats(&mut bytes, &self.global);
        bytes.extend_from_slice(&(self.countries.len() as u32).to_le_bytes());
        for (country, table) in &self.countries {
            bytes.extend_from_slice(&country.to_u16().to_le_bytes());
            floats(&mut bytes, table);
        }
        bytes
    }

    /// Weights written by [`Self::to_bytes`] for this build's feature layout.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader(bytes);
        ensure!(reader.take(MAGIC.len())? == MAGIC, "not parser weights");
        let version = reader.u32()?;
        ensure!(
            version == WEIGHTS_VERSION,
            "parser weights have version {version}, this build reads version {WEIGHTS_VERSION}"
        );
        let schema = reader.u64()?;
        let (features, labels, states) = (reader.u32()?, reader.u32()?, reader.u32()?);
        if schema != features::schema()
            || features as usize != features::COUNT
            || labels as usize != LABELS
            || states as usize != STATES
        {
            bail!("parser weights were trained for another feature layout; retrain them");
        }
        let edge = reader.floats(features::COUNT * LABELS)?;
        let global = reader.floats(STATES * STATES)?;
        let mut countries = BTreeMap::new();
        for _ in 0..reader.u32()? {
            let code = u16::from_le_bytes(reader.take(2)?.try_into().expect("two bytes"));
            let country = CountryCode::from_u16(code).context("invalid country code")?;
            countries.insert(country, reader.floats(STATES * STATES)?);
        }
        ensure!(reader.0.is_empty(), "trailing bytes after parser weights");
        Ok(Self {
            edge,
            global,
            countries,
        })
    }

    /// The `limit` largest edge weights, as `(feature, label, weight)`, for
    /// inspecting what a model learned.
    pub fn strongest(&self, limit: usize) -> Vec<(String, Label, f32)> {
        let mut all = self
            .edge
            .iter()
            .enumerate()
            .filter(|(_, weight)| **weight != 0.0)
            .map(|(index, weight)| {
                (
                    features::name((index / LABELS) as u32),
                    Label::ALL[index % LABELS],
                    *weight,
                )
            })
            .collect::<Vec<_>>();
        all.sort_by(|left, right| right.2.abs().total_cmp(&left.2.abs()));
        all.truncate(limit);
        all
    }
}

/// Reads the little-endian fields of a weights file in order.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        ensure!(self.0.len() >= len, "parser weights are truncated");
        let (head, tail) = self.0.split_at(len);
        self.0 = tail;
        Ok(head)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    fn floats(&mut self, count: usize) -> Result<Vec<f32>> {
        Ok(self
            .take(count * 4)?
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("four bytes")))
            .collect())
    }
}
