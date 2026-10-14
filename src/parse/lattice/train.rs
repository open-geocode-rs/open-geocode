//! Training the parser's weights from the Pack's own records.
//!
//! Records drawn from the Pack (addresses, POIs, streets, places, postcodes)
//! are written out the way people write addresses: through an address
//! template of OpenCageData's address-formatting
//! (<https://github.com/OpenCageData/address-formatting>, MIT, see
//! `data/LICENSE-address-formatting.txt`), vendored as `data/templates.tsv`
//! by `scripts/parse_lattice_data.py`, with the noise real queries carry:
//! commas dropped, fields left out or swapped, street types abbreviated, a
//! unit added, one-letter typos. Some rows hide the dictionary hits of their
//! street, locality or region, so the model also learns to read names the
//! Pack does not know from their shape and place alone. Labelled rows in
//! libpostal's training format can be added.
//!
//! The weights are learned by the averaged structured perceptron (Collins,
//! 2002): decode each row, and when the best labelling is wrong, move the
//! weights toward the features of the gold labelling and away from those of
//! the prediction. Averaging the weights over every step keeps the model
//! from following the last rows it saw.

use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Instant,
};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::Serialize;

use super::{
    CountryCode, FIELD_BREAKS, LatticeTagger, Suppression, Tokens,
    decode::{self, END, LABELS, Lattice, Path as LabelPath, START, STATES},
    dictionary::{Dictionaries, Field, unit_words},
    is_unspaced,
    weights::Weights,
};
use crate::{
    pack::{PackReader, RecordId},
    parse::types::Label,
    record::{AddressComponents, Layer, Record},
    text_index::normalize_index_text,
};

/// `CC<TAB>template` rows, `ZZ` the default.
const TEMPLATES: &str = include_str!("data/templates.tsv");
/// Rows decoded per parallel batch: lattices are built in parallel, the
/// weights updated in order.
const BATCH_ROWS: usize = 2_048;
/// The share of rendered rows held out to measure the model.
const HELD_OUT_SHARE: f64 = 0.05;
/// How many records are drawn per wanted row at most, so a Pack without some
/// layer still finishes.
const MAX_DRAWS_PER_ROW: usize = 40;

/// What [`train_weights`] trains on and where it writes the weights.
#[derive(Debug, Clone)]
pub struct TrainOptions {
    pub pack: PathBuf,
    /// Labelled rows in libpostal's format, used as they are.
    pub extra: Vec<PathBuf>,
    pub out: PathBuf,
    /// Rows rendered from the Pack's records.
    pub rows: usize,
    pub epochs: usize,
    pub seed: u64,
}

/// What a training run did and how well the weights label held-out rows.
#[derive(Debug, Clone, Serialize)]
pub struct TrainReport {
    pub out: String,
    pub rendered_rows: usize,
    pub extra_rows: usize,
    pub held_out_rows: usize,
    pub epochs: usize,
    /// Training rows the model got wrong in each epoch.
    pub epoch_errors: Vec<usize>,
    /// Countries with their own transition table.
    pub countries: Vec<String>,
    pub built_in: Accuracy,
    pub trained: Accuracy,
    pub seconds: f64,
}

/// Token and whole-row accuracy on the held-out rows.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Accuracy {
    pub tokens: f64,
    pub rows: f64,
    /// Per gold label: the share of its tokens labelled right.
    pub by_label: BTreeMap<String, f64>,
}

/// Train weights on `options.pack` and write them to `options.out`.
pub fn train_weights(options: &TrainOptions) -> Result<TrainReport> {
    let started = Instant::now();
    let pack = PackReader::open(&options.pack)?;
    // Only the dictionaries: the weights the Pack carries are what this
    // replaces, and may be of another feature layout.
    let tagger = LatticeTagger::new(Dictionaries::open(pack.container())?, Weights::zero());
    let mut random = Random::new(options.seed);
    let mut rows = Renderer::new(&pack, &tagger).render(options.rows, &mut random)?;
    let rendered_rows = rows.len();
    let mut extra_rows = 0;
    for path in &options.extra {
        let extra = read_libpostal(path)?;
        extra_rows += extra.len();
        rows.extend(extra);
    }
    if rows.len() < 2 {
        bail!("the Pack has too few records to train on");
    }
    random.shuffle(&mut rows);
    let held_out = ((rows.len() as f64 * HELD_OUT_SHARE) as usize).clamp(1, rows.len() - 1);
    let (test, train) = rows.split_at(held_out);

    let epochs = options.epochs.max(1);
    let mut model = Model::default();
    let mut order = (0..train.len()).collect::<Vec<_>>();
    let mut epoch_errors = Vec::new();
    for _ in 0..epochs {
        random.shuffle(&mut order);
        let mut errors = 0;
        for batch in order.chunks(BATCH_ROWS) {
            let lattices = batch
                .par_iter()
                .map(|index| {
                    let row = &train[*index];
                    tagger.lattice(&row.tokens, &row.suppression)
                })
                .collect::<Vec<_>>();
            for (index, lattice) in batch.iter().zip(&lattices) {
                errors += usize::from(model.learn(lattice, &train[*index]));
            }
        }
        epoch_errors.push(errors);
    }
    let weights = model.averaged();
    weights.write(&options.out)?;
    let built_in = evaluate(&tagger, &Weights::built_in(), test);
    let trained = evaluate(&tagger, &weights, test);
    Ok(TrainReport {
        out: options.out.display().to_string(),
        rendered_rows,
        extra_rows,
        held_out_rows: test.len(),
        epochs,
        epoch_errors,
        countries: weights.countries().map(|code| code.to_string()).collect(),
        built_in,
        trained,
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// One labelled training row.
#[derive(Debug, Clone)]
pub(super) struct Row {
    pub tokens: Tokens,
    pub gold: Vec<Label>,
    /// The country the row is written as, when its template is that
    /// country's own.
    pub country: Option<CountryCode>,
    pub suppression: Suppression,
}

/// The averaged perceptron's state.
#[derive(Debug)]
struct Model {
    weights: Weights,
    /// Every update times the step it was made at, to average lazily.
    totals: Weights,
    step: f32,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            weights: Weights::zero(),
            totals: Weights::zero(),
            step: 1.0,
        }
    }
}

impl Model {
    /// Decode `row` and update toward its gold labels when it is wrong.
    /// Returns whether it was wrong.
    fn learn(&mut self, lattice: &Lattice, row: &Row) -> bool {
        let predicted = decode::best(lattice, &self.weights, 1)
            .into_iter()
            .next()
            .expect("a lattice with tokens has a path");
        // A token's label is the label of every unit of it.
        let gold = lattice.units.expand(&row.gold);
        let wrong = predicted.labels(lattice) != gold;
        if wrong {
            // The gold labelling as the row's country when that country is
            // decoded at all, else as the global model.
            let country = row
                .country
                .filter(|country| lattice.candidates.contains(country));
            let gold = decode::decode(lattice, &self.weights, country, 1, Some(&gold))
                .into_iter()
                .next()
                .expect("every token is a span, so every labelling has a path");
            self.update(lattice, &gold, 1.0);
            self.update(lattice, &predicted, -1.0);
        }
        self.step += 1.0;
        wrong
    }

    /// Add `sign` times the features of `path` to the weights.
    fn update(&mut self, lattice: &Lattice, path: &LabelPath, sign: f32) {
        let mut previous = START;
        for segment in &path.segments {
            let label = segment.label as usize;
            let span = &lattice.spans[segment.span];
            for feature in span.features_in(path.country) {
                let index = feature as usize * LABELS + label;
                self.weights.edge[index] += sign;
                self.totals.edge[index] += sign * self.step;
            }
            self.transition(path.country, previous, label, sign);
            previous = label;
        }
        self.transition(path.country, previous, END, sign);
    }

    fn transition(
        &mut self,
        country: Option<CountryCode>,
        previous: usize,
        next: usize,
        sign: f32,
    ) {
        let index = previous * STATES + next;
        self.weights.global[index] += sign;
        self.totals.global[index] += sign * self.step;
        if let Some(country) = country {
            for (weights, delta) in [
                (&mut self.weights, sign),
                (&mut self.totals, sign * self.step),
            ] {
                weights
                    .countries
                    .entry(country)
                    .or_insert_with(|| vec![0.0; STATES * STATES])[index] += delta;
            }
        }
    }

    /// The weights averaged over every step.
    fn averaged(&self) -> Weights {
        let average = |weights: &[f32], totals: &[f32]| {
            weights
                .iter()
                .zip(totals)
                .map(|(weight, total)| weight - total / self.step)
                .collect::<Vec<_>>()
        };
        Weights {
            edge: average(&self.weights.edge, &self.totals.edge),
            global: average(&self.weights.global, &self.totals.global),
            countries: self
                .weights
                .countries
                .iter()
                .map(|(country, table)| (*country, average(table, &self.totals.countries[country])))
                .collect(),
        }
    }
}

/// Token and row accuracy of `weights` on `rows`.
fn evaluate(tagger: &LatticeTagger, weights: &Weights, rows: &[Row]) -> Accuracy {
    let results = rows
        .par_iter()
        .map(|row| {
            let lattice = tagger.lattice(&row.tokens, &Suppression::default());
            decode::best(&lattice, weights, 1)
                .into_iter()
                .next()
                .map(|path| lattice.units.collapse(&path.labels(&lattice)))
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let mut right = 0usize;
    let mut tokens = 0usize;
    let mut whole = 0usize;
    let mut by_label = BTreeMap::<Label, (usize, usize)>::new();
    for (row, labels) in rows.iter().zip(&results) {
        whole += usize::from(*labels == row.gold);
        for (gold, label) in row.gold.iter().zip(labels) {
            let counts = by_label.entry(*gold).or_default();
            counts.1 += 1;
            tokens += 1;
            if gold == label {
                counts.0 += 1;
                right += 1;
            }
        }
    }
    let share = |part: usize, all: usize| {
        if all == 0 {
            0.0
        } else {
            part as f64 / all as f64
        }
    };
    Accuracy {
        tokens: share(right, tokens),
        rows: share(whole, rows.len()),
        by_label: by_label
            .into_iter()
            .map(|(label, (right, all))| (label.name().to_string(), share(right, all)))
            .collect(),
    }
}

/// The fields of one address, each as the words it is written in.
#[derive(Debug, Clone, Default)]
struct Fields {
    house: Option<Tokens>,
    house_number: Option<Tokens>,
    unit: Option<Tokens>,
    street: Option<Tokens>,
    suburb: Option<Tokens>,
    locality: Option<Tokens>,
    region: Option<Tokens>,
    postcode: Option<Tokens>,
    country: Option<Tokens>,
}

/// A field of a template the parser has a label for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    House,
    HouseNumber,
    Street,
    Suburb,
    Locality,
    Region,
    Postcode,
    Country,
}

impl Slot {
    fn of(name: &str) -> Option<Self> {
        Some(match name {
            "house" => Self::House,
            "house_number" => Self::HouseNumber,
            "road" => Self::Street,
            "suburb" | "city_district" | "neighbourhood" | "quarter" => Self::Suburb,
            "city" | "town" | "village" | "hamlet" | "municipality" | "postal_city" => {
                Self::Locality
            }
            "state" | "state_code" | "province" | "region" => Self::Region,
            "postcode" => Self::Postcode,
            "country" => Self::Country,
            _ => return None,
        })
    }

    fn label(self) -> Label {
        match self {
            Self::House => Label::Other,
            Self::HouseNumber => Label::HouseNumber,
            Self::Street => Label::Street,
            Self::Suburb | Self::Locality => Label::Locality,
            Self::Region => Label::Region,
            Self::Postcode => Label::Postcode,
            Self::Country => Label::Country,
        }
    }

    fn value(self, fields: &Fields) -> Option<&Tokens> {
        match self {
            Self::House => fields.house.as_ref(),
            Self::HouseNumber => fields.house_number.as_ref(),
            Self::Street => fields.street.as_ref(),
            Self::Suburb => fields.suburb.as_ref(),
            Self::Locality => fields.locality.as_ref(),
            Self::Region => fields.region.as_ref(),
            Self::Postcode => fields.postcode.as_ref(),
            Self::Country => fields.country.as_ref(),
        }
    }
}

/// A parsed address template.
#[derive(Debug, Clone)]
enum Node {
    /// Literal text; only the field breaks in it matter.
    Text(String),
    Slot(Slot),
    /// A field the parser has no label for (`attention`, `county`).
    Unused,
    /// `{{#first}} a || b {{/first}}`: the first alternative with a value.
    First(Vec<Vec<Node>>),
}

fn parse_template(template: &str) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut rest = template;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("{{#first}}") {
            let (inner, tail) = after.split_once("{{/first}}").unwrap_or((after, ""));
            nodes.push(Node::First(inner.split("||").map(parse_template).collect()));
            rest = tail;
        } else if let Some(after) = rest.strip_prefix("{{{") {
            let (name, tail) = after.split_once("}}}").unwrap_or((after, ""));
            nodes.push(Slot::of(name.trim()).map_or(Node::Unused, Node::Slot));
            rest = tail;
        } else {
            let first = rest.chars().next().map_or(1, char::len_utf8);
            let end = rest[first..]
                .find("{{")
                .map_or(rest.len(), |end| end + first);
            nodes.push(Node::Text(rest[..end].to_string()));
            rest = &rest[end..];
        }
    }
    nodes
}

/// Every country's template, parsed once.
fn templates() -> &'static BTreeMap<String, Vec<Node>> {
    static TABLE: OnceLock<BTreeMap<String, Vec<Node>>> = OnceLock::new();
    TABLE.get_or_init(|| {
        TEMPLATES
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(code, template)| {
                (
                    code.to_string(),
                    parse_template(&template.replace("\\n", "\n")),
                )
            })
            .collect()
    })
}

/// One field of a rendered address.
#[derive(Debug, Clone)]
struct Group {
    label: Label,
    words: Tokens,
    /// Whether a comma or line break precedes it.
    break_before: bool,
    /// What joins its first word to the word before it ("602-670").
    joiner: Option<char>,
}

/// The fields of `fields` in the order `nodes` writes them.
fn render(nodes: &[Node], fields: &Fields, groups: &mut Vec<Group>, pending_break: &mut bool) {
    for node in nodes {
        match node {
            Node::Text(text) => {
                if text.contains(FIELD_BREAKS) && !groups.is_empty() {
                    *pending_break = true;
                }
            }
            Node::Slot(slot) => {
                if let Some(words) = slot.value(fields) {
                    groups.push(Group {
                        label: slot.label(),
                        words: words.clone(),
                        break_before: std::mem::take(pending_break),
                        joiner: None,
                    });
                }
            }
            Node::Unused => {}
            Node::First(alternatives) => {
                if let Some(alternative) = alternatives
                    .iter()
                    .find(|alternative| has_value(alternative, fields))
                {
                    render(alternative, fields, groups, pending_break);
                }
            }
        }
    }
}

/// Write `groups` in the opposite order, each field break staying between
/// the same two fields.
fn reverse_groups(groups: &mut [Group]) {
    let breaks = groups
        .iter()
        .map(|group| group.break_before)
        .collect::<Vec<_>>();
    groups.reverse();
    let count = groups.len();
    for (index, group) in groups.iter_mut().enumerate() {
        group.break_before = index > 0 && breaks[count - index];
        group.joiner = None;
    }
}

fn has_value(nodes: &[Node], fields: &Fields) -> bool {
    nodes.iter().any(|node| match node {
        Node::Slot(slot) => slot.value(fields).is_some(),
        Node::First(alternatives) => alternatives.iter().any(|nodes| has_value(nodes, fields)),
        _ => false,
    })
}

/// The abbreviations people write for street types and directions, the
/// inverse of the expansions the text index applies.
const ABBREVIATIONS: &[(&str, &str)] = &[
    ("street", "st"),
    ("avenue", "ave"),
    ("road", "rd"),
    ("drive", "dr"),
    ("boulevard", "blvd"),
    ("court", "crt"),
    ("crescent", "cres"),
    ("place", "pl"),
    ("lane", "ln"),
    ("highway", "hwy"),
    ("parkway", "pkwy"),
    ("terrace", "terr"),
    ("trail", "trl"),
    ("square", "sq"),
    ("circle", "cir"),
    ("gardens", "gdns"),
    ("heights", "hts"),
    ("grove", "grv"),
    ("east", "e"),
    ("west", "w"),
    ("north", "n"),
    ("south", "s"),
    ("northeast", "ne"),
    ("northwest", "nw"),
    ("southeast", "se"),
    ("southwest", "sw"),
];

/// Renders training rows from the Pack's records.
struct Renderer<'a> {
    pack: &'a PackReader,
    tagger: &'a LatticeTagger,
    names: HashMap<RecordId, Option<String>>,
    unit_words: Vec<String>,
    template_countries: Vec<String>,
}

impl<'a> Renderer<'a> {
    fn new(pack: &'a PackReader, tagger: &'a LatticeTagger) -> Self {
        Self {
            pack,
            tagger,
            names: HashMap::new(),
            unit_words: unit_words(),
            template_countries: templates().keys().cloned().collect(),
        }
    }

    /// About `rows` rows from records drawn at random, by layer: mostly
    /// addresses, then POIs, streets, places and postcodes.
    fn render(&mut self, rows: usize, random: &mut Random) -> Result<Vec<Row>> {
        let count = self.pack.records().record_count();
        if count == 0 {
            return Ok(Vec::new());
        }
        let quota = |share: f64| ((rows as f64 * share).ceil() as usize).max(1);
        let mut quotas = HashMap::from([
            (Layer::Address, quota(0.55)),
            (Layer::Poi, quota(0.2)),
            (Layer::Street, quota(0.15)),
            (Layer::Locality, quota(0.03)),
            (Layer::Place, quota(0.02)),
            (Layer::Neighbourhood, quota(0.01)),
            (Layer::Postcode, quota(0.04)),
        ]);
        let mut out = Vec::with_capacity(rows);
        for _ in 0..rows.saturating_mul(MAX_DRAWS_PER_ROW) {
            if out.len() >= rows {
                break;
            }
            let id = random.below(count);
            let layer = self.pack.records().header(id)?.layer;
            if quotas.get(&layer).is_none_or(|left| *left == 0) {
                continue;
            }
            let record = self.pack.records().record(id)?;
            if let Some(row) = self.row(id, &record, random)? {
                *quotas.get_mut(&layer).expect("quota") -= 1;
                out.push(row);
            }
        }
        Ok(out)
    }

    fn row(&mut self, id: RecordId, record: &Record, random: &mut Random) -> Result<Option<Row>> {
        let context = self.context(id)?;
        let mut fields = Fields::default();
        let mut stated = None;
        match record {
            Record::Address(record) => {
                stated = Some(&record.address);
                address_fields(&record.address, &mut fields);
            }
            Record::Poi(record) => {
                fields.house = words(&record.name);
                if let Some(address) = &record.address
                    && random.chance(0.5)
                {
                    stated = Some(address);
                    address_fields(address, &mut fields);
                }
            }
            Record::Street(record) => fields.street = words(&record.name),
            Record::Place(_, record) => {
                // A place is named with the areas above it, not within.
                fields.locality = words(&record.name);
                fields.region = context.region.as_deref().and_then(words);
                fields.country = context.country.as_deref().and_then(words);
                return Ok(self.finish(fields, context.country_code, random));
            }
            Record::Postcode(record) => fields.postcode = words(&record.postcode),
            Record::Interpolation(_) => return Ok(None),
        }
        if fields.house.is_none() && fields.street.is_none() && fields.postcode.is_none() {
            return Ok(None);
        }
        // Where the record is: as it states it, or the areas it lies in.
        let pick = |stated: Option<&str>, area: Option<&str>, random: &mut Random| match (
            stated.and_then(words),
            area.and_then(words),
        ) {
            (Some(stated), Some(area)) => Some(if random.chance(0.5) { stated } else { area }),
            (stated, area) => stated.or(area),
        };
        let stated_locality =
            stated.and_then(|address| address.locality.as_deref().or(address.place.as_deref()));
        fields.locality = pick(stated_locality, context.locality.as_deref(), random);
        if random.chance(0.3) {
            fields.suburb = context.suburb.as_deref().and_then(words);
        }
        fields.region = pick(
            stated.and_then(|address| address.region.as_deref()),
            context.region.as_deref(),
            random,
        );
        fields.country = pick(
            stated.and_then(|address| address.country.as_deref()),
            context.country.as_deref(),
            random,
        );
        let country = stated
            .and_then(|address| address.country.as_deref())
            .and_then(CountryCode::parse)
            .or(context.country_code);
        Ok(self.finish(fields, country, random))
    }

    /// Drop, render, perturb and label the fields of one row. `None` when
    /// the template writes none of the fields left.
    fn finish(
        &self,
        mut fields: Fields,
        country: Option<CountryCode>,
        random: &mut Random,
    ) -> Option<Row> {
        let has_subject = fields.house.is_some() || fields.street.is_some();
        if has_subject && random.chance(0.2) {
            fields.locality = None;
        }
        if random.chance(0.5) {
            fields.postcode = None;
        }
        if random.chance(0.5) {
            fields.region = None;
        }
        if random.chance(0.8) {
            fields.country = None;
        }
        if let Some(street) = fields.street.as_mut()
            && random.chance(0.5)
        {
            for word in street.words.iter_mut() {
                if let Some((_, short)) = ABBREVIATIONS.iter().find(|(long, _)| *long == word) {
                    if random.chance(0.7) {
                        *word = short.to_string();
                    }
                } else if let Some(stem) = word.strip_suffix("strasse")
                    && !stem.is_empty()
                    && random.chance(0.7)
                {
                    *word = format!("{stem}str");
                }
            }
        }
        // A number in parts ("993.5", "1384 1/2"), which few records state.
        if let Some(number) = fields.house_number.as_mut()
            && number
                .words
                .iter()
                .all(|word| word.chars().all(char::is_numeric))
            && random.chance(0.03)
        {
            if random.chance(0.5) {
                number.push("5".to_string(), false, Some('.'));
            } else {
                number.push("1".to_string(), false, None);
                number.push("2".to_string(), false, Some('/'));
            }
        }
        // The record's own country writes it most often, any other some of
        // the time, so the model sees every order of fields.
        let template_country = match country {
            Some(code) if random.chance(0.7) => code.to_string(),
            _ => self.template_countries
                [random.below(self.template_countries.len() as u64) as usize]
                .clone(),
        };
        let template = templates()
            .get(&template_country)
            .or_else(|| templates().get("ZZ"))
            .expect("the vendored templates have a default");
        let mut groups = Vec::new();
        render(template, &fields, &mut groups, &mut false);
        let row_country = country.filter(|code| code.as_str() == template_country);
        // Many places write the largest area first and the house number
        // last ("100-0005 東京都千代田区丸の内2-7-2"), whatever order the
        // template states, so some rows run from largest to smallest.
        if random.chance(0.25) {
            reverse_groups(&mut groups);
        }

        if (fields.house_number.is_some() || fields.street.is_some()) && random.chance(0.2) {
            self.add_unit(&mut groups, fields.unit.clone(), random);
        }
        if groups.len() > 1 && random.chance(0.1) {
            if random.chance(0.5) {
                let index = random.below(groups.len() as u64 - 1) as usize;
                groups.swap(index, index + 1);
            } else {
                let last = groups.pop().expect("groups");
                groups.insert(0, last);
            }
        }
        let keep_breaks = random.chance(0.2);
        let mut tokens = Tokens::default();
        let mut gold = Vec::new();
        let mut suppression = Suppression::default();
        for group in &groups {
            let start = tokens.len();
            for (index, word) in group.words.words.iter().enumerate() {
                let word = if random.chance(0.1) {
                    typo(word, random)
                } else {
                    word.clone()
                };
                let (break_before, joiner) = if index == 0 {
                    (group.break_before && keep_breaks, group.joiner)
                } else {
                    (false, group.words.joiners[index])
                };
                tokens.push(word, break_before, joiner);
                gold.push(group.label);
            }
            let field = match group.label {
                Label::Street => Field::Street,
                Label::Locality => Field::Locality,
                Label::Region => Field::Region,
                _ => continue,
            };
            if random.chance(0.15) {
                suppression.hide(field, start, tokens.len());
            }
        }
        (!tokens.is_empty()).then_some(Row {
            tokens,
            gold,
            country: row_country,
            suppression,
        })
    }

    /// A unit, as its own field after the street ("unit 602"), before the
    /// address ("apt 4, 670 ..."), or before the house number ("602 670 king
    /// street", from "602-670").
    fn add_unit(&self, groups: &mut Vec<Group>, unit: Option<Tokens>, random: &mut Random) {
        let value = unit.unwrap_or_else(|| {
            let number = 1 + random.below(2_000);
            let letter = (b'a' + random.below(6) as u8) as char;
            Tokens::from_words(&[match random.below(4) {
                0 => format!("{number}{letter}"),
                1 => letter.to_string(),
                _ => number.to_string(),
            }])
        });
        let number_at = groups
            .iter()
            .position(|group| group.label == Label::HouseNumber);
        if let Some(at) = number_at
            && value
                .words
                .iter()
                .all(|word| word.chars().any(char::is_numeric))
            && random.chance(0.35)
        {
            let break_before = groups[at].break_before;
            groups[at].break_before = false;
            groups[at].joiner = random.chance(0.6).then_some('-');
            groups.insert(
                at,
                Group {
                    label: Label::Unit,
                    words: value,
                    break_before,
                    joiner: None,
                },
            );
            return;
        }
        let mut words = Tokens::from_words(&[
            &self.unit_words[random.below(self.unit_words.len() as u64) as usize]
        ]);
        for (word, joiner) in value.words.into_iter().zip(value.joiners) {
            words.push(word, false, joiner);
        }
        let street_end = groups
            .iter()
            .rposition(|group| matches!(group.label, Label::Street | Label::HouseNumber))
            .map_or(0, |index| index + 1);
        let at = if random.chance(0.25) { 0 } else { street_end };
        if at == 0
            && let Some(first) = groups.first_mut()
        {
            first.break_before = true;
        }
        groups.insert(
            at,
            Group {
                label: Label::Unit,
                words,
                break_before: at > 0,
                joiner: None,
            },
        );
    }

    /// The names of the areas a record lies in, and its country's code.
    fn context(&mut self, id: RecordId) -> Result<Areas> {
        let Some(context) = self.pack.boundary_context(id)? else {
            return Ok(Areas::default());
        };
        let tuple = context.admin_context;
        let mut locality = None;
        for area in [
            tuple.locality_record_id,
            tuple.place_record_id,
            tuple.district_record_id,
        ] {
            if locality.is_none() {
                locality = self.name(area)?;
            }
        }
        let country = self.name(tuple.country_record_id)?;
        let country_code = country
            .as_deref()
            .and_then(normalize_index_text)
            .and_then(|name| self.tagger.dictionaries().get(Field::Country, &name))
            .and_then(|entry| entry.country);
        Ok(Areas {
            locality,
            suburb: self.name(tuple.neighbourhood_record_id)?,
            region: self.name(tuple.region_record_id)?,
            country,
            country_code,
        })
    }

    fn name(&mut self, id: Option<RecordId>) -> Result<Option<String>> {
        let Some(id) = id else {
            return Ok(None);
        };
        if let Some(name) = self.names.get(&id) {
            return Ok(name.clone());
        }
        let name = self.pack.context_record(id)?.map(|record| record.name);
        self.names.insert(id, name.clone());
        Ok(name)
    }
}

/// The areas a record lies in.
#[derive(Debug, Default)]
struct Areas {
    locality: Option<String>,
    suburb: Option<String>,
    region: Option<String>,
    country: Option<String>,
    country_code: Option<CountryCode>,
}

fn address_fields(address: &AddressComponents, fields: &mut Fields) {
    fields.house_number = words(&address.number);
    fields.street = address.street.as_deref().and_then(words);
    fields.unit = address.unit.as_deref().and_then(words);
    fields.postcode = address.postcode.as_deref().and_then(words);
}

fn words(value: &str) -> Option<Tokens> {
    let tokens = Tokens::new(value);
    (!tokens.is_empty()).then_some(tokens)
}

/// One edit inside a word of four letters or more: a letter dropped,
/// doubled, replaced or swapped with the next. The first and last letters
/// stay, as they do in most typing errors. A word of a script written
/// without spaces is typed through an input method that picks whole
/// characters, so it gets no typo.
fn typo(word: &str, random: &mut Random) -> String {
    let mut chars = word.chars().collect::<Vec<_>>();
    if chars.len() < 4
        || !chars.iter().all(|character| character.is_alphabetic())
        || chars.iter().copied().any(is_unspaced)
    {
        return word.to_string();
    }
    let at = 1 + random.below(chars.len() as u64 - 2) as usize;
    let swap = at.min(chars.len() - 3);
    match random.below(4) {
        0 => {
            chars.remove(at);
        }
        1 => chars.insert(at, chars[at]),
        2 => chars[at] = (b'a' + random.below(26) as u8) as char,
        _ => chars.swap(swap, swap + 1),
    }
    chars.into_iter().collect()
}

/// Rows in libpostal's training format: `language<TAB>country<TAB>address`
/// or `country<TAB>address`, the address as space-separated `token/label`
/// pairs with `,/SEP` or `/FSEP` between fields.
fn read_libpostal(path: &Path) -> Result<Vec<Row>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut rows = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let fields = line.split('\t').collect::<Vec<_>>();
        let (country, address) = match fields.as_slice() {
            [_, country, address] | [country, address] => (*country, *address),
            _ => continue,
        };
        if let Some(row) = libpostal_row(CountryCode::parse(country), address) {
            rows.push(row);
        }
    }
    Ok(rows)
}

fn libpostal_row(country: Option<CountryCode>, address: &str) -> Option<Row> {
    let mut tokens = Tokens::default();
    let mut gold = Vec::new();
    let mut pending_break = false;
    for pair in address.split_whitespace() {
        let (text, label) = pair.rsplit_once('/')?;
        let label = match label {
            "SEP" | "FSEP" => {
                pending_break = true;
                continue;
            }
            "house_number" => Label::HouseNumber,
            "road" => Label::Street,
            "unit" | "level" | "entrance" | "staircase" => Label::Unit,
            "city" | "suburb" | "city_district" | "island" => Label::Locality,
            "state" | "state_district" | "country_region" => Label::Region,
            "postcode" => Label::Postcode,
            "country" => Label::Country,
            _ => Label::Other,
        };
        let words = Tokens::new(text);
        for (word, joiner) in words.words.into_iter().zip(words.joiners) {
            tokens.push(word, std::mem::take(&mut pending_break), joiner);
            gold.push(label);
        }
    }
    (!tokens.is_empty()).then(|| Row {
        tokens,
        gold,
        country,
        suppression: Suppression::default(),
    })
}

/// SplitMix64: small, fast and the same on every platform, so a seed gives
/// the same rows and weights everywhere.
#[derive(Debug, Clone)]
struct Random(u64);

impl Random {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    /// A number below `bound`, which must not be zero.
    fn below(&mut self, bound: u64) -> u64 {
        ((u128::from(self.next()) * u128::from(bound)) >> 64) as u64
    }

    fn chance(&mut self, probability: f64) -> bool {
        ((self.next() >> 11) as f64 / (1u64 << 53) as f64) < probability
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            items.swap(index, self.below(index as u64 + 1) as usize);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{super::dictionary::Dictionaries, super::dictionary::Entry, *};

    fn fields() -> Fields {
        Fields {
            house_number: words("670"),
            street: words("King Street West"),
            locality: words("Toronto"),
            region: words("ON"),
            postcode: words("M5V 1M9"),
            ..Fields::default()
        }
    }

    fn rendered(country: &str, fields: &Fields) -> Vec<(Label, String, bool)> {
        let mut groups = Vec::new();
        render(&templates()[country], fields, &mut groups, &mut false);
        groups
            .into_iter()
            .map(|group| (group.label, group.words.words.join(" "), group.break_before))
            .collect()
    }

    #[test]
    fn templates_write_fields_in_each_countrys_order() {
        use Label::*;
        assert_eq!(
            rendered("CA", &fields()),
            [
                (HouseNumber, "670".to_string(), false),
                (Street, "king street west".to_string(), false),
                (Locality, "toronto".to_string(), true),
                (Region, "on".to_string(), true),
                (Postcode, "m5v 1m9".to_string(), false),
            ]
        );
        let german = rendered("DE", &fields());
        assert_eq!(german[0].0, Street);
        assert_eq!(german[1].0, HouseNumber);
        assert_eq!(german[2], (Postcode, "m5v 1m9".to_string(), true));
        // A missing field leaves its alternative to the next one.
        let mut no_number = fields();
        no_number.house_number = None;
        assert_eq!(rendered("CA", &no_number)[0].0, Street);
        assert!(templates().len() > 200 && templates().contains_key("ZZ"));
    }

    #[test]
    fn libpostal_rows_keep_labels_and_field_breaks() {
        use Label::*;
        let row = libpostal_row(
            CountryCode::parse("us"),
            "123/house_number main/road st/road ,/SEP new/city york/city ny/state",
        )
        .expect("row");
        assert_eq!(row.tokens.words, ["123", "main", "st", "new", "york", "ny"]);
        assert_eq!(row.tokens.breaks, [false, false, false, true, false, false]);
        assert_eq!(
            row.gold,
            [HouseNumber, Street, Street, Locality, Locality, Region]
        );
        assert!(libpostal_row(None, "no-label").is_none());
    }

    #[test]
    fn typos_are_one_edit_inside_long_words() {
        let mut random = Random::new(7);
        for _ in 0..50 {
            let typo = typo("toronto", &mut random);
            assert!(typo.starts_with('t') && typo.ends_with('o'), "{typo}");
            assert!(typo.chars().count().abs_diff(7) <= 1);
        }
        assert_eq!(typo("670", &mut random), "670");
        assert_eq!(typo("on", &mut random), "on");
    }

    #[test]
    fn the_perceptron_learns_a_toy_language() {
        use Label::*;
        let entry = Entry {
            count: 10,
            country: None,
        };
        let dictionaries = Dictionaries::from_entries([
            (Field::Locality, "toronto", entry),
            (Field::Street, "king street", entry),
        ]);
        let tagger = LatticeTagger::new(dictionaries, Weights::zero());
        let row = |text: &str, gold: Vec<Label>| Row {
            tokens: Tokens::new(text),
            gold,
            country: None,
            suppression: Suppression::default(),
        };
        let rows = [
            row(
                "670 king street toronto",
                vec![HouseNumber, Street, Street, Locality],
            ),
            row(
                "toronto 12 king street",
                vec![Locality, HouseNumber, Street, Street],
            ),
            row("9 king street", vec![HouseNumber, Street, Street]),
        ];
        let mut model = Model::default();
        for _ in 0..10 {
            for row in &rows {
                model.learn(&tagger.lattice(&row.tokens, &row.suppression), row);
            }
        }
        let accuracy = evaluate(&tagger, &model.averaged(), &rows);
        assert_eq!(accuracy.rows, 1.0, "{accuracy:?}");
    }
}
