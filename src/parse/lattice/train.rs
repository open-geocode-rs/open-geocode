use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    path::Path,
    rc::Rc,
    sync::{Arc, OnceLock},
    time::Instant,
};

use crate::error::{Error, Result};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use super::{
    CountryCode, FIELD_BREAKS, LatticeTagger, NUMERALS, Suppression, Tokens,
    abbreviations::abbreviations,
    decode::{self, END, LABELS, Lattice, START, STATES},
    dictionary::{Dictionaries, Field, admin_aliases, is_admin_type, level},
    dictionary_key, is_numeral, is_unspaced,
    weights::Weights,
};
use crate::{
    address_template::{self, Piece},
    extsort::{ExternalSorter, IndexedSpool, IndexedSpoolFile, Scratch, Spill, Spool, SpoolFile},
    memory::{Budget, Reservation, Spillable},
    pack::{Reader, RecordId},
    parse::types::Label,
    record::{AddressComponents, Layer, PlaceLayer, Record},
    resources::Resources,
    util::{
        codec::{Delta, get_delta, get_string, get_u8, get_u64, put_delta, put_str, put_u64},
        fold::search_key,
    },
};

pub const ROWS_PER_COUNTRY: usize = 200_000;
pub const EPOCHS: usize = 5;
const SEED: u64 = 1;
const MIN_TRAIN_ROWS: usize = 1_000;
const TRAIN_SHARDS: usize = 4;
const HELD_OUT_SHARE: f64 = 0.05;
const OVERSAMPLE: f64 = 1.25;
const SHUFFLE_WINDOW: usize = 1 << 16;
const LATTICE_BATCH: usize = 1 << 10;
const EVALUATE_BATCH: usize = 1 << 12;

const SHARES: [(Draw, f64); 11] = [
    (Draw::Layer(Layer::Address), 0.40),
    (Draw::BlockAddress, BLOCK_ADDRESS_SHARE),
    (Draw::Layer(Layer::Poi), 0.15),
    (Draw::Layer(Layer::Street), 0.10),
    (Draw::Layer(Layer::Country), 0.002),
    (Draw::Layer(Layer::Region), 0.01),
    (Draw::Layer(Layer::District), 0.01),
    (Draw::Layer(Layer::Locality), 0.03),
    (Draw::Layer(Layer::Place), 0.02),
    (Draw::Layer(Layer::Neighbourhood), 0.01),
    (Draw::Layer(Layer::Postcode), 0.04),
];

fn quota(draw: Draw) -> Option<usize> {
    SHARES
        .iter()
        .find(|(kind, _)| *kind == draw)
        .map(|(_, share)| ((ROWS_PER_COUNTRY as f64 * share).ceil() as usize).max(1))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Report {
    pub rows_per_country: usize,
    pub rows_by_country: BTreeMap<String, usize>,
    pub rendered_rows: usize,
    pub house_number_rows: usize,
    pub block_address_rows: usize,
    pub held_out_rows: usize,
    pub sample_bytes: u64,
    pub shards: usize,
    pub epochs: usize,
    pub countries: Vec<String>,
    pub trained: bool,
    pub accuracy: Accuracy,
    pub seconds: f64,
    pub render_seconds: f64,
    pub lattice_seconds: f64,
    pub learn_seconds: f64,
    pub lattice_memory_bytes: u64,
    pub lattice_file_bytes: u64,
    pub lattice_file_shards: usize,
    pub pack_prefaulted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Accuracy {
    pub tokens: f64,
    pub rows: f64,
    pub by_label: BTreeMap<String, f64>,
}

#[derive(Debug)]
pub struct Sampler {
    reservoirs: BTreeMap<(Option<RecordId>, Draw), Reservoir>,
    random: Random,
}

#[derive(Debug, Default)]
struct Reservoir {
    seen: u64,
    ids: Vec<RecordId>,
}

impl Default for Sampler {
    fn default() -> Self {
        Self {
            reservoirs: BTreeMap::new(),
            random: Random::new(SEED),
        }
    }
}

impl Sampler {
    /// Offer record `id`, which lies in the country record `country`. Offers
    /// come in record order, so the sample depends on the input alone.
    pub fn offer(&mut self, id: RecordId, country: Option<RecordId>, record: &Record) {
        if matches!(record, Record::Poi(poi) if poi.source.gers().is_some()) {
            return;
        }
        self.add(country, Draw::Layer(record.layer()), id);
        if is_block_address(record) {
            self.add(country, Draw::BlockAddress, id);
        }
    }

    fn add(&mut self, country: Option<RecordId>, draw: Draw, id: RecordId) {
        let Some(quota) = quota(draw) else {
            return;
        };
        let capacity = (quota as f64 * OVERSAMPLE).ceil() as usize;
        let reservoir = self.reservoirs.entry((country, draw)).or_default();
        reservoir.seen += 1;
        if reservoir.ids.len() < capacity {
            reservoir.ids.push(id);
        } else {
            let at = self.random.below(reservoir.seen) as usize;
            if at < capacity {
                reservoir.ids[at] = id;
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_u64(&mut out, self.random.0);
        put_u64(&mut out, self.reservoirs.len() as u64);
        for ((country, draw), reservoir) in &self.reservoirs {
            put_u64(&mut out, country.map_or(0, |country| country + 1));
            match draw {
                Draw::Layer(layer) => put_str(&mut out, layer.as_str()),
                Draw::BlockAddress => put_str(&mut out, ""),
            }
            put_u64(&mut out, reservoir.seen);
            put_u64(&mut out, reservoir.ids.len() as u64);
            for id in &reservoir.ids {
                put_u64(&mut out, *id);
            }
        }
        out
    }

    pub fn from_bytes(mut input: &[u8]) -> Result<Self> {
        let input = &mut input;
        let random = Random(get_u64(input)?);
        let mut reservoirs = BTreeMap::new();
        for _ in 0..get_u64(input)? {
            let country = get_u64(input)?.checked_sub(1);
            let draw = match get_string(input)?.as_str() {
                "" => Draw::BlockAddress,
                layer => Draw::Layer(
                    Layer::parse(layer).ok_or(Error::Decode("unknown layer in a saved sample"))?,
                ),
            };
            let seen = get_u64(input)?;
            let ids = (0..get_u64(input)?)
                .map(|_| get_u64(input))
                .collect::<Result<Vec<_>>>()?;
            reservoirs.insert((country, draw), Reservoir { seen, ids });
        }
        if !input.is_empty() {
            return Err(Error::Decode("a saved sample has trailing bytes"));
        }
        Ok(Self { reservoirs, random })
    }

    pub fn resident_bytes(&self) -> usize {
        self.reservoirs
            .values()
            .map(|reservoir| {
                std::mem::size_of::<((Option<RecordId>, Draw), Reservoir)>()
                    + reservoir.ids.capacity() * std::mem::size_of::<RecordId>()
            })
            .sum()
    }

    fn candidates(self, random: &mut Random) -> BTreeMap<Option<RecordId>, Vec<Candidate>> {
        let mut countries = BTreeMap::<Option<RecordId>, Vec<Candidate>>::new();
        for ((country, draw), reservoir) in self.reservoirs {
            let mut ids = reservoir.ids;
            random.shuffle(&mut ids);
            countries.entry(country).or_default().extend(
                ids.into_iter()
                    .enumerate()
                    .map(|(rank, id)| Candidate { id, draw, rank }),
            );
        }
        for candidates in countries.values_mut() {
            candidates.sort_unstable_by_key(|candidate| {
                (candidate.id, candidate.draw != Draw::BlockAddress)
            });
            candidates.dedup_by_key(|candidate| candidate.id);
        }
        countries
    }
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    id: RecordId,
    draw: Draw,
    rank: usize,
}

/// Train weights on the records `sample` drew from the sealed, unpublished
/// Pack file at `pack`. The weights are `None` when the Pack renders too few
/// rows to learn from.
pub fn train(
    pack: &Path,
    sample: Sampler,
    scratch: &Arc<Scratch>,
    memory: &Arc<Budget>,
    sort_budget_bytes: usize,
) -> Result<(Option<Weights>, Report)> {
    let started = Instant::now();
    let mut report = Report {
        rows_per_country: ROWS_PER_COUNTRY,
        sample_bytes: sample.resident_bytes() as u64,
        epochs: EPOCHS,
        ..Report::default()
    };
    let pack = Reader::open(pack)?;
    let dictionaries = pack.container();
    report.pack_prefaulted =
        dictionaries.prefault_sections("parse/", Resources::probe().available_memory_bytes) > 0;
    let tagger = LatticeTagger::new(Dictionaries::open(dictionaries)?, Weights::zero());
    let mut random = Random::new(SEED);

    let mut shuffled =
        ExternalSorter::<Shuffled>::new(scratch, "parse-rows", memory, sort_budget_bytes);
    let mut renderer = Renderer::new(&pack, &tagger);
    let candidates = sample.candidates(&mut random);
    let in_countries = candidates.keys().any(Option::is_some);
    let mut drawn_rows = 0;
    for (country, candidates) in candidates {
        scratch.check_cancelled()?;
        if country.is_none() && in_countries {
            continue;
        }
        let mut drawn = BTreeMap::<Draw, Vec<(usize, Row)>>::new();
        for candidate in candidates {
            let record = pack.records().record(candidate.id)?;
            if let Some(mut row) = renderer.row(candidate.id, &record, &mut random)? {
                row.block_address = is_block_address(&record);
                drawn
                    .entry(candidate.draw)
                    .or_default()
                    .push((candidate.rank, row));
            }
        }
        renderer.names.clear();
        let mut rows = 0;
        for (draw, mut drawn) in drawn {
            drawn.sort_unstable_by_key(|(rank, _)| *rank);
            let quota = quota(draw).expect("only kinds with a quota are drawn");
            for (_, row) in drawn.into_iter().take(quota) {
                shuffled.push(Shuffled {
                    key: random.next(),
                    seq: drawn_rows,
                    row,
                })?;
                drawn_rows += 1;
                rows += 1;
            }
        }
        let name = match country {
            Some(id) => pack
                .records()
                .names(id)?
                .into_iter()
                .next()
                .unwrap_or_else(|| format!("record {id}")),
            None => "none".to_string(),
        };
        *report.rows_by_country.entry(name).or_default() += rows;
    }
    drop(renderer);

    let shards = TRAIN_SHARDS;
    report.shards = shards;
    let mut held_out = Spool::<Row>::create(scratch, "parse-held-out")?;
    let mut parts = (0..shards)
        .map(|_| Spool::<Row>::create(scratch, "parse-train"))
        .collect::<Result<Vec<_>>>()?;
    let mut train_rows = 0;
    for item in shuffled.finish()? {
        let Shuffled { row, .. } = item?;
        report.rendered_rows += 1;
        report.house_number_rows += usize::from(row.gold.contains(&Label::HouseNumber));
        report.block_address_rows += usize::from(row.block_address);
        if random.chance(HELD_OUT_SHARE) {
            held_out.write(&row)?;
        } else {
            parts[train_rows % shards].write(&row)?;
            train_rows += 1;
        }
    }
    let held_out = held_out.finish()?;
    let parts = parts
        .into_iter()
        .map(Spool::finish)
        .collect::<Result<Vec<_>>>()?;
    report.held_out_rows = held_out.len() as usize;
    report.render_seconds = started.elapsed().as_secs_f64();
    if report.rendered_rows < MIN_TRAIN_ROWS {
        report.seconds = started.elapsed().as_secs_f64();
        return Ok((None, report));
    }

    let at = Instant::now();
    let mut caches = parts
        .iter()
        .map(|_| LatticeCache::new(scratch, memory))
        .collect::<Vec<_>>();
    for (part, cache) in parts.iter().zip(&mut caches) {
        let mut rows = part.reader()?;
        loop {
            scratch.check_cancelled()?;
            let batch = rows
                .by_ref()
                .take(LATTICE_BATCH)
                .collect::<Result<Vec<_>>>()?;
            if batch.is_empty() {
                break;
            }
            let built = batch
                .into_par_iter()
                .map(|row| Learned {
                    lattice: tagger.lattice(&row.tokens, &row.suppression),
                    gold: row.gold,
                })
                .collect::<Vec<_>>();
            for row in built {
                cache.push(row)?;
            }
        }
    }
    drop(parts);
    let caches = caches
        .into_iter()
        .map(LatticeCache::finish)
        .collect::<Result<Vec<_>>>()?;
    report.lattice_seconds = at.elapsed().as_secs_f64();
    for cache in &caches {
        match cache {
            Lattices::Memory(_, reservation) => {
                report.lattice_memory_bytes += reservation.bytes() as u64;
            }
            Lattices::File(file) => {
                report.lattice_file_bytes += file.bytes();
                report.lattice_file_shards += 1;
            }
        }
    }

    let mut mixed = Weights::zero();
    let mut averages = Vec::with_capacity(EPOCHS);
    for epoch in 0..EPOCHS {
        let results = caches
            .par_iter()
            .enumerate()
            .map(|(shard, cache)| -> Result<_> {
                let mut order = Random::new(SEED ^ ((epoch as u64) << 32 | shard as u64));
                let mut model = Model::from_weights(mixed.clone());
                let at = Instant::now();
                for (seen, index) in shuffled_order(cache.len(), &mut order)
                    .into_iter()
                    .enumerate()
                {
                    if seen % LATTICE_BATCH == 0 {
                        scratch.check_cancelled()?;
                    }
                    cache.visit(index as usize, |row| {
                        model.learn(&row.lattice, &row.gold);
                    })?;
                }
                Ok((model.averaged(), model.weights, at.elapsed().as_secs_f64()))
            })
            .collect::<Result<Vec<_>>>()?;
        report.learn_seconds += results.iter().map(|result| result.2).sum::<f64>() / shards as f64;
        mixed = Weights::mean(results.iter().map(|result| &result.1));
        averages.push(Weights::mean(results.iter().map(|result| &result.0)));
    }
    drop(caches);
    let weights = Weights::mean(averages.iter());
    report.accuracy = evaluate_file(&tagger, &weights, &held_out)?;
    report.countries = weights.countries().map(|code| code.to_string()).collect();
    report.trained = true;
    report.seconds = started.elapsed().as_secs_f64();
    Ok((Some(weights), report))
}

fn shuffled_order(len: usize, random: &mut Random) -> Vec<u32> {
    let mut order = Vec::with_capacity(len);
    let mut window = Vec::with_capacity(SHUFFLE_WINDOW.min(len));
    for index in 0..len as u32 {
        if window.len() < SHUFFLE_WINDOW {
            window.push(index);
            continue;
        }
        let at = random.below(window.len() as u64) as usize;
        order.push(std::mem::replace(&mut window[at], index));
    }
    random.shuffle(&mut window);
    order.extend(window);
    order
}

#[derive(Debug, Clone, PartialEq)]
struct Learned {
    gold: Vec<Label>,
    lattice: Lattice,
}

impl Spill for Learned {
    fn encode(&self, out: &mut Vec<u8>) {
        put_u64(out, self.gold.len() as u64);
        out.extend(self.gold.iter().map(|label| *label as u8));
        self.lattice.encode(out);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let mut gold = Vec::new();
        for _ in 0..get_u64(input)? {
            let Some(label) = Label::ALL.get(usize::from(get_u8(input)?)) else {
                return Err(Error::BadFixtureRow(
                    "invalid label in a training lattice".into(),
                ));
            };
            gold.push(*label);
        }
        Ok(Self {
            gold,
            lattice: Lattice::decode(input)?,
        })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.gold.capacity() + self.lattice.resident_bytes()
            - std::mem::size_of::<Lattice>()
    }
}

struct LatticeCache {
    scratch: Arc<Scratch>,
    reservation: Spillable,
    rows: Vec<Learned>,
    file: Option<IndexedSpool<Learned>>,
}

enum Lattices {
    Memory(Vec<Learned>, Reservation),
    File(Box<IndexedSpoolFile<Learned>>),
}

impl LatticeCache {
    fn new(scratch: &Arc<Scratch>, memory: &Arc<Budget>) -> Self {
        Self {
            scratch: Arc::clone(scratch),
            reservation: Spillable::new(memory),
            rows: Vec::new(),
            file: None,
        }
    }

    fn push(&mut self, row: Learned) -> Result<()> {
        if let Some(file) = &mut self.file {
            return file.write(&row);
        }
        let bytes = row.resident_bytes();
        let fits = self.reservation.bytes() + bytes <= self.reservation.share(usize::MAX)
            && self.reservation.reservation().try_grow(bytes)
            && self.rows.try_reserve(1).is_ok();
        if fits {
            self.rows.push(row);
            return Ok(());
        }
        let mut file = IndexedSpool::create(&self.scratch, "parse-lattices")?;
        for held in self.rows.drain(..) {
            file.write(&held)?;
        }
        file.write(&row)?;
        self.rows = Vec::new();
        self.reservation.reservation().release_all();
        self.file = Some(file);
        Ok(())
    }

    fn finish(self) -> Result<Lattices> {
        Ok(match self.file {
            Some(file) => Lattices::File(Box::new(file.finish()?)),
            None => Lattices::Memory(self.rows, self.reservation.into_reservation()),
        })
    }
}

impl Lattices {
    fn len(&self) -> usize {
        match self {
            Self::Memory(rows, _) => rows.len(),
            Self::File(file) => file.len(),
        }
    }

    fn visit(&self, index: usize, visit: impl FnOnce(&Learned)) -> Result<()> {
        match self {
            Self::Memory(rows, _) => visit(&rows[index]),
            Self::File(file) => visit(&file.get(index)?),
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Shuffled {
    key: u64,
    seq: u64,
    row: Row,
}

impl PartialEq for Shuffled {
    fn eq(&self, other: &Self) -> bool {
        (self.key, self.seq) == (other.key, other.seq)
    }
}

impl Eq for Shuffled {}

impl PartialOrd for Shuffled {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Shuffled {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.key, self.seq).cmp(&(other.key, other.seq))
    }
}

impl Spill for Shuffled {
    fn encode(&self, out: &mut Vec<u8>) {
        put_u64(out, self.key);
        put_u64(out, self.seq);
        self.row.encode(out);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(Self {
            key: get_u64(input)?,
            seq: get_u64(input)?,
            row: Row::decode(input)?,
        })
    }

    fn encode_after(&self, delta: &mut Delta, out: &mut Vec<u8>) {
        put_delta(out, delta, self.key as i64);
        put_u64(out, self.seq);
        self.row.encode(out);
    }

    fn decode_after(delta: &mut Delta, input: &mut &[u8]) -> Result<Self> {
        Ok(Self {
            key: get_delta(input, delta)? as u64,
            seq: get_u64(input)?,
            row: Row::decode(input)?,
        })
    }

    fn resident_bytes(&self) -> usize {
        2 * std::mem::size_of::<u64>() + self.row.resident_bytes()
    }
}

#[derive(Debug, Clone)]
pub(super) struct Row {
    pub tokens: Tokens,
    pub gold: Vec<Label>,
    pub suppression: Suppression,
    pub block_address: bool,
}

const HIDDEN_FIELDS: [Field; 2] = [Field::Street, Field::Poi];

impl Spill for Row {
    fn encode(&self, out: &mut Vec<u8>) {
        put_u64(out, self.tokens.words.len() as u64);
        for ((word, break_before), joiner) in self
            .tokens
            .words
            .iter()
            .zip(&self.tokens.breaks)
            .zip(&self.tokens.joiners)
        {
            put_str(out, word);
            out.push(u8::from(*break_before));
            put_u64(out, joiner.map_or(0, u64::from));
        }
        put_u64(out, self.gold.len() as u64);
        out.extend(self.gold.iter().map(|label| *label as u8));
        put_u64(out, self.suppression.0.len() as u64);
        for (field, start, end) in &self.suppression.0 {
            let field = HIDDEN_FIELDS
                .iter()
                .position(|hidden| hidden == field)
                .expect("training rows hide street and venue hits only");
            out.push(field as u8);
            put_u64(out, *start as u64);
            put_u64(out, *end as u64);
        }
        out.push(u8::from(self.block_address));
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let mut tokens = Tokens::default();
        for _ in 0..get_u64(input)? {
            let word = get_string(input)?;
            let break_before = get_u8(input)? != 0;
            let joiner = match get_u64(input)? {
                0 => None,
                code => Some(
                    u32::try_from(code)
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or_else(|| {
                            Error::BadFixtureRow("invalid joiner in a training row".into())
                        })?,
                ),
            };
            tokens.words.push(word);
            tokens.breaks.push(break_before);
            tokens.joiners.push(joiner);
        }
        let mut gold = Vec::new();
        for _ in 0..get_u64(input)? {
            let Some(label) = Label::ALL.get(usize::from(get_u8(input)?)) else {
                return Err(Error::BadFixtureRow(
                    "invalid label in a training row".into(),
                ));
            };
            gold.push(*label);
        }
        let mut suppression = Suppression::default();
        for _ in 0..get_u64(input)? {
            let Some(field) = HIDDEN_FIELDS.get(usize::from(get_u8(input)?)) else {
                return Err(Error::BadFixtureRow(
                    "invalid hidden field in a training row".into(),
                ));
            };
            let start = get_u64(input)? as usize;
            let end = get_u64(input)? as usize;
            suppression.hide(*field, start, end);
        }
        Ok(Self {
            tokens,
            gold,
            suppression,
            block_address: get_u8(input)? != 0,
        })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .tokens
                .words
                .iter()
                .map(|word| word.capacity() + std::mem::size_of::<String>())
                .sum::<usize>()
            + self.tokens.breaks.capacity()
            + self.tokens.joiners.capacity() * std::mem::size_of::<Option<char>>()
            + self.gold.capacity()
            + self.suppression.0.capacity() * std::mem::size_of::<(Field, usize, usize)>()
    }
}

#[derive(Debug)]
struct Model {
    weights: Weights,
    totals: Weights,
    squares: Weights,
    step: f32,
}

const LEARNING_RATE: f32 = 0.1;
const MIN_GRADIENT: f64 = 1e-4;

impl Model {
    fn from_weights(weights: Weights) -> Self {
        Self {
            weights,
            totals: Weights::zero(),
            squares: Weights::zero(),
            step: 1.0,
        }
    }

    fn learn(&mut self, lattice: &Lattice, gold: &[Label]) {
        let gold = lattice.units.expand(gold);
        let gradient = Gradient::of(lattice, &self.weights, &gold);
        for (feature, (values, set)) in gradient.edge {
            for (label, value) in values.into_iter().enumerate() {
                if set & (1 << label) == 0 {
                    continue;
                }
                let index = feature * LABELS + label;
                let delta = adagrad(&mut self.squares.edge[index], value);
                self.weights.edge[index] += delta;
                self.totals.edge[index] += delta * self.step;
            }
        }
        for (country, values) in gradient.transitions {
            let kept = |value: f32| f64::from(value).abs() >= MIN_GRADIENT;
            if !values.iter().any(|value| kept(*value)) {
                continue;
            }
            let squares = table(&mut self.squares, country);
            let weights = table(&mut self.weights, country);
            let totals = table(&mut self.totals, country);
            for (index, value) in values.into_iter().enumerate() {
                if !kept(value) {
                    continue;
                }
                let delta = adagrad(&mut squares[index], value);
                weights[index] += delta;
                totals[index] += delta * self.step;
            }
        }
        self.step += 1.0;
    }

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
                .map(|(country, table)| {
                    let totals = self.totals.countries.get(country);
                    let average = match totals {
                        Some(totals) => average(table, totals),
                        None => table.clone(),
                    };
                    (*country, average)
                })
                .collect(),
        }
    }
}

fn adagrad(squares: &mut f32, gradient: f32) -> f32 {
    *squares += gradient * gradient;
    LEARNING_RATE * gradient / (squares.sqrt() + 1e-6)
}

fn table(weights: &mut Weights, country: Option<CountryCode>) -> &mut Vec<f32> {
    match country {
        None => &mut weights.global,
        Some(country) => weights
            .countries
            .entry(country)
            .or_insert_with(|| vec![0.0; STATES * STATES]),
    }
}

#[derive(Debug, Default)]
struct Gradient {
    edge: FxHashMap<usize, ([f32; LABELS], u8)>,
    transitions: HashMap<Option<CountryCode>, Vec<f32>>,
}

impl Gradient {
    fn of(lattice: &Lattice, weights: &Weights, gold: &[Label]) -> Self {
        let countries = decode::countries(lattice);
        let order = span_order(lattice);
        let golds = gold_labels(lattice, gold);
        let readings = countries
            .iter()
            .map(|country| {
                let edges = span_edges(lattice, weights, *country);
                (
                    Reading::new(lattice, weights, *country, None, &order, edges.clone()),
                    Reading::new(lattice, weights, *country, Some(&golds), &order, edges),
                )
            })
            .collect::<Vec<_>>();
        let total = |gold: bool| {
            log_sum(readings.iter().map(
                |(all, constrained)| {
                    if gold { constrained.log_z } else { all.log_z }
                },
            ))
        };
        let (all_z, gold_z) = (total(false), total(true));
        let mut gradient = Self::default();
        if !gold_z.is_finite() {
            return gradient;
        }
        for (country, (all, constrained)) in countries.iter().zip(&readings) {
            for (reading, sign, z) in [(constrained, 1.0, gold_z), (all, -1.0, all_z)] {
                let share = (reading.log_z - z).exp();
                if share < MIN_GRADIENT {
                    continue;
                }
                reading.add_expectations(lattice, *country, sign * share, &mut gradient);
            }
        }
        gradient
    }
}

fn span_edges(
    lattice: &Lattice,
    weights: &Weights,
    country: Option<CountryCode>,
) -> Vec<[f64; LABELS]> {
    lattice
        .spans
        .iter()
        .map(|span| weights.edges(span, country).map(f64::from))
        .collect()
}

fn span_order(lattice: &Lattice) -> Vec<usize> {
    let mut order = (0..lattice.spans.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| lattice.spans[*index].end);
    order
}

fn gold_labels(lattice: &Lattice, gold: &[Label]) -> Vec<u8> {
    lattice
        .spans
        .iter()
        .map(|span| {
            let units = &gold[span.start..span.end];
            match units.first() {
                None => u8::MAX,
                Some(first) if units.iter().all(|unit| unit == first) => 1 << *first as u8,
                Some(_) => 0,
            }
        })
        .collect()
}

struct Reading<'a> {
    golds: Option<&'a [u8]>,
    edges: Vec<[f64; LABELS]>,
    transitions: [[f64; STATES]; STATES],
    alpha: Vec<[f64; STATES]>,
    beta: Vec<[f64; STATES]>,
    log_z: f64,
}

impl<'a> Reading<'a> {
    fn new(
        lattice: &Lattice,
        weights: &Weights,
        country: Option<CountryCode>,
        golds: Option<&'a [u8]>,
        order: &[usize],
        edges: Vec<[f64; LABELS]>,
    ) -> Self {
        let table = weights.transitions(country);
        let mut transitions = [[0.0; STATES]; STATES];
        for (previous, row) in transitions.iter_mut().enumerate() {
            for (next, value) in row.iter_mut().enumerate() {
                *value = f64::from(table.get(previous, next));
            }
        }
        let len = lattice.len;
        let mut alpha = vec![[f64::NEG_INFINITY; STATES]; len + 1];
        alpha[0][START] = 0.0;
        let mut sums = vec![[None; LABELS]; len + 1];
        for &index in order {
            let span = &lattice.spans[index];
            for label in Label::ALL {
                if !allowed(golds, index, label) {
                    continue;
                }
                let next = label as usize;
                let incoming =
                    *sums[span.start][next].get_or_insert_with(|| {
                        log_sum(previous_states(span.start).map(|previous| {
                            alpha[span.start][previous] + transitions[previous][next]
                        }))
                    });
                let value = incoming + edges[index][next];
                alpha[span.end][next] = log_add(alpha[span.end][next], value);
            }
        }
        let mut beta = vec![[f64::NEG_INFINITY; STATES]; len + 1];
        for label in 0..LABELS {
            beta[len][label] = transitions[label][END];
        }
        for &index in order.iter().rev() {
            let span = &lattice.spans[index];
            for label in Label::ALL {
                if !allowed(golds, index, label) {
                    continue;
                }
                let next = label as usize;
                let onward = edges[index][next] + beta[span.end][next];
                for previous in previous_states(span.start) {
                    beta[span.start][previous] = log_add(
                        beta[span.start][previous],
                        transitions[previous][next] + onward,
                    );
                }
            }
        }
        let log_z = log_sum((0..LABELS).map(|label| alpha[len][label] + beta[len][label]));
        Self {
            golds,
            edges,
            transitions,
            alpha,
            beta,
            log_z,
        }
    }

    fn add_expectations(
        &self,
        lattice: &Lattice,
        country: Option<CountryCode>,
        scale: f64,
        gradient: &mut Gradient,
    ) {
        if !self.log_z.is_finite() {
            return;
        }
        let mut transitions = vec![0.0f64; STATES * STATES];
        for (index, span) in lattice.spans.iter().enumerate() {
            let mut values = [0.0f32; LABELS];
            let mut set = 0u8;
            for label in Label::ALL {
                let next = label as usize;
                if !allowed(self.golds, index, label) {
                    continue;
                }
                let onward = self.edges[index][next] + self.beta[span.end][next] - self.log_z;
                let mut probability = 0.0;
                for previous in previous_states(span.start) {
                    let marginal = (self.alpha[span.start][previous]
                        + self.transitions[previous][next]
                        + onward)
                        .exp();
                    transitions[previous * STATES + next] += marginal;
                    probability += marginal;
                }
                let value = scale * probability;
                if value.abs() < MIN_GRADIENT {
                    continue;
                }
                values[next] = value as f32;
                set |= 1 << next;
            }
            if set == 0 {
                continue;
            }
            for feature in span.features_in(country) {
                let (sums, seen) = gradient.edge.entry(feature as usize).or_default();
                for (label, value) in values.iter().enumerate() {
                    if set & (1 << label) != 0 {
                        sums[label] += value;
                    }
                }
                *seen |= set;
            }
        }
        let len = lattice.len;
        for label in 0..LABELS {
            transitions[label * STATES + END] +=
                (self.alpha[len][label] + self.beta[len][label] - self.log_z).exp();
        }
        let mut tables = vec![None];
        if country.is_some() {
            tables.push(country);
        }
        for table in tables {
            let target = gradient
                .transitions
                .entry(table)
                .or_insert_with(|| vec![0.0; STATES * STATES]);
            for (target, value) in target.iter_mut().zip(&transitions) {
                *target += (scale * value) as f32;
            }
        }
    }
}

fn allowed(golds: Option<&[u8]>, index: usize, label: Label) -> bool {
    golds.is_none_or(|golds| golds[index] & (1 << label as u8) != 0)
}

pub(super) fn label_marginals(lattice: &Lattice, weights: &Weights) -> Vec<[f64; LABELS]> {
    let order = span_order(lattice);
    let readings = decode::countries(lattice)
        .into_iter()
        .map(|country| {
            let edges = span_edges(lattice, weights, country);
            Reading::new(lattice, weights, country, None, &order, edges)
        })
        .collect::<Vec<_>>();
    let total = log_sum(readings.iter().map(|reading| reading.log_z));
    let mut marginals = vec![[0.0; LABELS]; lattice.len];
    if !total.is_finite() {
        return marginals;
    }
    for reading in &readings {
        for (index, span) in lattice.spans.iter().enumerate() {
            for label in 0..LABELS {
                let onward = reading.edges[index][label] + reading.beta[span.end][label] - total;
                let probability = previous_states(span.start)
                    .map(|previous| {
                        (reading.alpha[span.start][previous]
                            + reading.transitions[previous][label]
                            + onward)
                            .exp()
                    })
                    .sum::<f64>();
                for unit in &mut marginals[span.start..span.end] {
                    unit[label] += probability;
                }
            }
        }
    }
    marginals
}

fn previous_states(start: usize) -> std::ops::Range<usize> {
    if start == 0 {
        START..START + 1
    } else {
        0..LABELS
    }
}

fn log_add(left: f64, right: f64) -> f64 {
    let high = left.max(right);
    let gap = -(left - right).abs();
    if high == f64::NEG_INFINITY || gap < -37.5 {
        return high;
    }
    high + (1.0 + gap.exp()).ln()
}

fn log_sum(values: impl Iterator<Item = f64>) -> f64 {
    values.fold(f64::NEG_INFINITY, log_add)
}

fn evaluate_file(
    tagger: &LatticeTagger,
    weights: &Weights,
    rows: &SpoolFile<Row>,
) -> Result<Accuracy> {
    let mut tally = Tally::default();
    let mut reader = rows.reader()?;
    loop {
        let batch = reader
            .by_ref()
            .take(EVALUATE_BATCH)
            .collect::<Result<Vec<_>>>()?;
        if batch.is_empty() {
            break;
        }
        tally.add(tagger, weights, &batch);
    }
    Ok(tally.accuracy())
}

#[derive(Debug, Default)]
struct Tally {
    rows: usize,
    whole: usize,
    tokens: usize,
    right: usize,
    by_label: BTreeMap<Label, (usize, usize)>,
}

impl Tally {
    fn add(&mut self, tagger: &LatticeTagger, weights: &Weights, rows: &[Row]) {
        let results = rows
            .par_iter()
            .map(|row| {
                let lattice = tagger.lattice(&row.tokens, &Suppression::default());
                decode::best(&lattice, weights, 1, None)
                    .into_iter()
                    .next()
                    .map(|path| lattice.units.collapse(&path.labels(&lattice)))
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        for (row, labels) in rows.iter().zip(&results) {
            self.rows += 1;
            self.whole += usize::from(*labels == row.gold);
            for (gold, label) in row.gold.iter().zip(labels) {
                let counts = self.by_label.entry(*gold).or_default();
                counts.1 += 1;
                self.tokens += 1;
                if gold == label {
                    counts.0 += 1;
                    self.right += 1;
                }
            }
        }
    }

    fn accuracy(&self) -> Accuracy {
        let share = |part: usize, all: usize| {
            if all == 0 {
                0.0
            } else {
                part as f64 / all as f64
            }
        };
        Accuracy {
            tokens: share(self.right, self.tokens),
            rows: share(self.whole, self.rows),
            by_label: self
                .by_label
                .iter()
                .map(|(label, (right, all))| (label.name().to_string(), share(*right, *all)))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Fields {
    house: Option<Tokens>,
    house_number: Option<Tokens>,
    unit: Option<Tokens>,
    street: Option<Tokens>,
    suburb: Option<Tokens>,
    locality: Option<Tokens>,
    island: Option<Tokens>,
    district: Option<Tokens>,
    region: Option<Tokens>,
    postcode: Option<Tokens>,
    country: Option<Tokens>,
    subject: Option<Slot>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    House,
    HouseNumber,
    Street,
    Suburb,
    Locality,
    Island,
    District,
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
            "island" | "archipelago" => Self::Island,
            "county" | "state_district" => Self::District,
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
            Self::Suburb | Self::Locality | Self::Island => Label::Locality,
            Self::District => Label::Region,
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
            Self::Island => fields.island.as_ref(),
            Self::District => fields.district.as_ref(),
            Self::Region => fields.region.as_ref(),
            Self::Postcode => fields.postcode.as_ref(),
            Self::Country => fields.country.as_ref(),
        }
    }
}

type Node = address_template::Node<Option<Slot>>;

fn templates() -> &'static BTreeMap<String, Vec<Node>> {
    static TABLE: OnceLock<BTreeMap<String, Vec<Node>>> = OnceLock::new();
    TABLE.get_or_init(|| address_template::table(Slot::of))
}

#[derive(Debug, Clone)]
struct Group {
    label: Label,
    words: Tokens,
    break_before: bool,
    joiner: Option<char>,
}

fn render(nodes: &[Node], fields: &Fields, groups: &mut Vec<Group>, pending_break: &mut bool) {
    address_template::write(
        nodes,
        &|slot| slot.is_some_and(|slot| slot.value(fields).is_some()),
        &mut |piece| match piece {
            Piece::Text(text) => {
                if text.contains(FIELD_BREAKS) && !groups.is_empty() {
                    *pending_break = true;
                }
            }
            Piece::Field(Some(slot)) => {
                if let Some(words) = slot.value(fields) {
                    groups.push(Group {
                        label: slot.label(),
                        words: words.clone(),
                        break_before: std::mem::take(pending_break),
                        joiner: None,
                    });
                }
            }
            Piece::Field(None) | Piece::Unfilled => {}
        },
    );
}

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

fn writes_slot(nodes: &[Node], slot: Slot) -> bool {
    nodes.iter().any(|node| match node {
        Node::Field(written) => *written == Some(slot),
        Node::First(alternatives) => alternatives.iter().any(|nodes| writes_slot(nodes, slot)),
        _ => false,
    })
}

fn insert_area(groups: &mut Vec<Group>, label: Label, words: &Tokens) {
    let at = groups
        .iter()
        .rposition(|group| group.label == Label::Locality)
        .map(|index| index + 1)
        .or_else(|| {
            groups.iter().position(|group| {
                matches!(
                    group.label,
                    Label::Region | Label::Postcode | Label::Country
                )
            })
        })
        .unwrap_or(groups.len());
    if let Some(next) = groups.get_mut(at) {
        next.break_before = true;
        next.joiner = None;
    }
    groups.insert(
        at,
        Group {
            label,
            words: words.clone(),
            break_before: at > 0,
            joiner: None,
        },
    );
}

const ALT_NAME_SHARE: f64 = 0.3;
const POI_ADDRESS_SHARE: f64 = 0.7;
const UNIT_WORD_AFTER_SHARE: f64 = 0.5;
const KEEP_BREAKS_SHARE: f64 = 0.4;
const UNIT_BREAK_SHARE: f64 = 0.5;
const HIDDEN_STREET_SHARE: f64 = 0.3;
const HIDDEN_POI_SHARE: f64 = 0.15;
const SPLIT_STREET_TYPE_SHARE: f64 = 0.15;
/// libpostal's level and unit words written without a number
/// ("erdgeschoss", "penthouse"), MIT (see `NOTICE`).
const STANDALONE_UNIT_WORDS: [&str; 2] = [
    include_str!("data/level_standalone.txt"),
    include_str!("data/unit_standalone.txt"),
];
const GLUED_UNIT_SHARE: f64 = 0.3;
const NEAR_SHARE: f64 = 0.04;
const CATEGORY_SHARE: f64 = 0.03;
const NUMBERED_AREA_SHARE: f64 = 0.3;
const POSTCODE_PREFIX_SHARE: f64 = 0.05;
const NUMERAL_SHARE: f64 = 0.1;
const ISLAND_SHARE: f64 = 0.35;
const DISTRICT_SHARE: f64 = 0.4;
const CHAIN_SHARE: f64 = 0.5;
const ADMIN_ALIAS_SHARE: f64 = 0.3;
const REGION_GROUP_SHARE: f64 = 0.3;
const REGION_AND_GROUP_SHARE: f64 = 0.2;

#[derive(Debug, Default)]
struct Seen {
    venues: Vec<Tokens>,
    regions: Vec<Tokens>,
}

impl Seen {
    fn add_noise(
        &mut self,
        unit_words: &[Vec<String>; 3],
        fields: &Fields,
        groups: &mut Vec<Group>,
        random: &mut Random,
    ) {
        let keep = |seen: &mut Vec<Tokens>, value: &Option<Tokens>, random: &mut Random| {
            if let Some(value) = value {
                if seen.len() < SEEN_NAMES {
                    seen.push(value.clone());
                } else {
                    let at = random.below(SEEN_NAMES as u64) as usize;
                    seen[at] = value.clone();
                }
            }
        };
        keep(&mut self.venues, &fields.house, random);
        keep(&mut self.regions, &fields.region, random);
        if groups.is_empty() {
            return;
        }
        if random.chance(FIELD_TWICE_SHARE)
            && let Some(at) = groups
                .iter()
                .position(|group| matches!(group.label, Label::Locality | Label::Region))
        {
            let mut twice = groups[at].clone();
            twice.joiner = None;
            groups.insert(at + 1, twice);
        }
        if random.chance(ADDRESS_TWICE_SHARE) {
            let mut again = groups.clone();
            if let Some(first) = again.first_mut() {
                first.break_before = true;
                first.joiner = None;
            }
            groups.extend(again);
        }
        if groups.len() > 1 && random.chance(MIDDLE_UNIT_SHARE) {
            let vocabulary = &unit_words[match random.chance(0.5) {
                true => 0,
                false => 2,
            }];
            if !vocabulary.is_empty()
                && let Some(mut phrase) =
                    words(&vocabulary[random.below(vocabulary.len() as u64) as usize])
            {
                let number = 1 + random.below(200);
                let letter = (b'a' + random.below(6) as u8) as char;
                phrase.push(
                    match random.chance(0.5) {
                        true => number.to_string(),
                        false => letter.to_string(),
                    },
                    false,
                    None,
                );
                let at = 1 + random.below(groups.len() as u64 - 1) as usize;
                groups.insert(
                    at,
                    Group {
                        label: Label::Unit,
                        words: phrase,
                        break_before: true,
                        joiner: None,
                    },
                );
            }
        }
        if !self.venues.is_empty() && fields.house.is_none() && random.chance(VENUE_BEFORE_SHARE) {
            let venue = self.venues[random.below(self.venues.len() as u64) as usize].clone();
            groups.insert(
                0,
                Group {
                    label: Label::Other,
                    words: venue,
                    break_before: false,
                    joiner: None,
                },
            );
            if let Some(next) = groups.get_mut(1) {
                next.break_before = true;
            }
        }
        if !self.regions.is_empty() && random.chance(EXTRA_REGION_SHARE) {
            let region = self.regions[random.below(self.regions.len() as u64) as usize].clone();
            let at = groups
                .iter()
                .position(|group| matches!(group.label, Label::Postcode | Label::Country))
                .unwrap_or(groups.len());
            groups.insert(
                at,
                Group {
                    label: Label::Region,
                    words: region,
                    break_before: true,
                    joiner: None,
                },
            );
        }
        if random.chance(NUMBER_SUFFIX_SHARE)
            && let Some(group) = groups
                .iter_mut()
                .find(|group| group.label == Label::HouseNumber)
            && let Some(word) = group.words.words.first_mut()
            && word.chars().all(|character| character.is_ascii_digit())
        {
            word.push((b'a' + random.below(6) as u8) as char);
        }
    }
}

struct Renderer<'a> {
    pack: &'a Reader,
    tagger: &'a LatticeTagger,
    names: FxHashMap<RecordId, Rc<[String]>>,
    country_codes: FxHashMap<RecordId, Option<CountryCode>>,
    localities: RefCell<FxHashMap<String, bool>>,
    unit_words: [Vec<String>; 3],
    near: Vec<String>,
    categories: Vec<String>,
    template_countries: Vec<String>,
    seen: Seen,
}

impl<'a> Renderer<'a> {
    fn new(pack: &'a Reader, tagger: &'a LatticeTagger) -> Self {
        Self {
            pack,
            tagger,
            names: FxHashMap::default(),
            country_codes: FxHashMap::default(),
            localities: RefCell::default(),
            unit_words: [Field::Unit, Field::PoBox, Field::Level]
                .map(|field| tagger.dictionaries().keys(field)),
            near: tagger.dictionaries().keys(Field::Near),
            categories: tagger.dictionaries().keys(Field::Category),
            template_countries: templates().keys().cloned().collect(),
            seen: Seen::default(),
        }
    }

    fn row(&mut self, id: RecordId, record: &Record, random: &mut Random) -> Result<Option<Row>> {
        let context = self.context(id, random)?;
        let mut fields = Fields::default();
        let mut stated = None;
        let name = pick_name(&record.names(), random).and_then(words);
        match record {
            Record::Address(record) => {
                stated = Some(&record.address);
                address_fields(&record.address, &mut fields);
                number_areas(&mut fields, random);
            }
            Record::Poi(record) => {
                fields.house = name;
                if let Some(address) = &record.address
                    && random.chance(POI_ADDRESS_SHARE)
                {
                    stated = Some(address);
                    address_fields(address, &mut fields);
                    number_areas(&mut fields, random);
                }
            }
            Record::Street(_) => fields.street = name,
            Record::Place(layer, _) => {
                fields.country = context.country.as_deref().and_then(words);
                match layer {
                    PlaceLayer::Country => {
                        fields.country = name;
                        fields.subject = Some(Slot::Country);
                    }
                    PlaceLayer::Region if !self.names_a_locality(name.as_ref()) => {
                        fields.region = name;
                        fields.subject = Some(Slot::Region);
                    }
                    PlaceLayer::District if !self.names_a_locality(name.as_ref()) => {
                        fields.district = name;
                        fields.subject = Some(Slot::District);
                        fields.region = context.region.as_deref().and_then(words);
                    }
                    _ => {
                        fields.locality = name;
                        fields.island = self.island(&context, random);
                        fields.district = self.district(&context, random);
                        fields.region = context.region.as_deref().and_then(words);
                    }
                }
                return Ok(self.finish(fields, context.country_code, random));
            }
            Record::Postcode(record) => fields.postcode = words(&record.postcode),
            Record::Interpolation(_) => return Ok(None),
        }
        let numbered_in_area = fields.house_number.is_some() && fields.suburb.is_some();
        if fields.house.is_none()
            && fields.street.is_none()
            && fields.postcode.is_none()
            && !numbered_in_area
        {
            return Ok(None);
        }
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
        if fields.suburb.is_none() && random.chance(0.3) {
            fields.suburb = context.suburb.as_deref().and_then(words);
        }
        fields.island = self.island(&context, random);
        fields.district = self.district(&context, random);
        if let Some(chain) = stated
            .filter(|_| random.chance(CHAIN_SHARE))
            .and_then(|address| chained_areas(address, random))
        {
            fields.locality = Some(chain);
            if random.chance(0.5) {
                fields.suburb = None;
            }
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

    fn finish(
        &mut self,
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
        if let (Some(postcode), Some(country)) = (fields.postcode.as_mut(), country)
            && random.chance(POSTCODE_PREFIX_SHARE)
        {
            *postcode = with_country_prefix(postcode, country, random);
        }
        if random.chance(0.5) && fields.subject != Some(Slot::Region) {
            fields.region = None;
        }
        if random.chance(0.8) && fields.subject != Some(Slot::Country) {
            fields.country = None;
        }
        if let Some(street) = fields.street.as_mut()
            && random.chance(0.5)
        {
            let table = abbreviations();
            let mut split = Tokens::default();
            for (word, joiner) in street.words.iter().zip(&street.joiners) {
                match table.split_glued(word) {
                    Some((stem, form)) if random.chance(SPLIT_STREET_TYPE_SHARE) => {
                        split.push(stem.to_string(), false, *joiner);
                        split.push(form.to_string(), false, None);
                    }
                    _ => split.push(word.clone(), false, *joiner),
                }
            }
            *street = split;
            for word in street.words.iter_mut() {
                if !random.chance(0.7) {
                    continue;
                }
                let short = table.short_forms(word);
                let other = if short.is_empty() {
                    table.unglued(word)
                } else {
                    short.to_vec()
                };
                if !other.is_empty() {
                    word.clone_from(&other[random.below(other.len() as u64) as usize]);
                }
            }
        }
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
        if let Some(island) = &fields.island
            && !writes_slot(template, Slot::Island)
        {
            insert_area(&mut groups, Label::Locality, island);
        }
        if let Some(district) = &fields.district
            && !writes_slot(template, Slot::District)
        {
            insert_area(&mut groups, Label::Region, district);
        }
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
        let unspaced = groups.iter().any(|group| {
            group
                .words
                .words
                .iter()
                .any(|word| word.chars().any(is_unspaced))
        });
        if unspaced && random.chance(NUMERAL_SHARE) {
            for group in groups
                .iter_mut()
                .filter(|group| group.label != Label::Postcode)
            {
                for word in &mut group.words.words {
                    if random.chance(0.5) {
                        *word = in_numerals(word);
                    }
                }
            }
        }
        if (fields.house.is_some() || fields.subject.is_some())
            && !self.near.is_empty()
            && random.chance(NEAR_SHARE)
            && let Some(phrase) = words(&self.near[random.below(self.near.len() as u64) as usize])
        {
            self.add_near(&mut groups, phrase);
        }
        let areas = groups.iter().any(|group| {
            matches!(
                group.label,
                Label::Locality | Label::Region | Label::Postcode | Label::Country
            )
        });
        if areas
            && !self.categories.is_empty()
            && !self.near.is_empty()
            && random.chance(CATEGORY_SHARE)
            && let (Some(category), Some(near)) = (
                words(&self.categories[random.below(self.categories.len() as u64) as usize]),
                words(&self.near[random.below(self.near.len() as u64) as usize]),
            )
        {
            groups.retain(|group| {
                matches!(
                    group.label,
                    Label::Locality | Label::Region | Label::Postcode | Label::Country
                )
            });
            self.add_near(&mut groups, near);
            groups.insert(
                0,
                Group {
                    label: Label::Other,
                    words: category,
                    break_before: false,
                    joiner: None,
                },
            );
        }
        self.seen
            .add_noise(&self.unit_words, &fields, &mut groups, random);
        let keep_breaks = random.chance(KEEP_BREAKS_SHARE);
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
                    let unit_break = group.label == Label::Unit && random.chance(UNIT_BREAK_SHARE);
                    (
                        group.break_before && (keep_breaks || unit_break),
                        group.joiner,
                    )
                } else {
                    (false, group.words.joiners[index])
                };
                tokens.push(word, break_before, joiner);
                gold.push(group.label);
            }
            let (field, share) = match group.label {
                Label::Street => (Field::Street, HIDDEN_STREET_SHARE),
                Label::Other => (Field::Poi, HIDDEN_POI_SHARE),
                _ => continue,
            };
            if random.chance(share) {
                suppression.hide(field, start, tokens.len());
            }
        }
        (!tokens.is_empty()).then_some(Row {
            tokens,
            gold,
            suppression,
            block_address: false,
        })
    }

    fn district(&self, context: &Areas, random: &mut Random) -> Option<Tokens> {
        context
            .district
            .as_deref()
            .filter(|_| random.chance(DISTRICT_SHARE))
            .and_then(words)
    }

    fn names_a_locality(&self, name: Option<&Tokens>) -> bool {
        let Some(name) = name else {
            return false;
        };
        let name = name.words.join(" ");
        if let Some(&known) = self.localities.borrow().get(&name) {
            return known;
        }
        let known = dictionary_key(&name)
            .and_then(|key| self.tagger.dictionaries().get(Field::Locality, &key))
            .is_some_and(|entry| entry.levels & level::LOCALITY != 0);
        self.localities.borrow_mut().insert(name, known);
        known
    }

    fn island(&self, context: &Areas, random: &mut Random) -> Option<Tokens> {
        context
            .island
            .as_deref()
            .filter(|_| random.chance(ISLAND_SHARE))
            .and_then(words)
    }

    fn add_near(&self, groups: &mut Vec<Group>, phrase: Tokens) {
        let at = groups
            .iter()
            .position(|group| {
                matches!(
                    group.label,
                    Label::Locality | Label::Region | Label::Postcode | Label::Country
                )
            })
            .unwrap_or(groups.len());
        if let Some(next) = groups.get_mut(at) {
            next.break_before = false;
            next.joiner = None;
        }
        groups.insert(
            at,
            Group {
                label: Label::Other,
                words: phrase,
                break_before: false,
                joiner: None,
            },
        );
    }

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
            groups[at].joiner = match random.below(5) {
                0 | 1 => None,
                2 | 3 => Some('-'),
                _ => Some('/'),
            };
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
        let kind = match random.below(10) {
            0..=5 => 0,
            6 | 7 => 1,
            _ => 2,
        };
        let vocabulary = &self.unit_words[kind];
        let phrase = (!vocabulary.is_empty())
            .then(|| &vocabulary[random.below(vocabulary.len() as u64) as usize])
            .and_then(|phrase| words(phrase));
        let after = random.chance(UNIT_WORD_AFTER_SHARE);
        if let Some(phrase) = &phrase
            && standalone_unit_words().contains(&phrase.words.join(" "))
        {
            self.insert_unit(groups, phrase.clone(), random);
            return;
        }
        let glue = phrase.as_ref().is_some_and(|phrase| {
            phrase.words.len() == 1 && !phrase.words[0].chars().any(is_unspaced)
        }) && value.words.len() == 1
            && value.words[0].chars().all(char::is_numeric)
            && random.chance(GLUED_UNIT_SHARE);
        let mut words = Tokens::default();
        if glue {
            let (phrase, number) = (&phrase.as_ref().expect("phrase").words[0], &value.words[0]);
            words.push(
                if after {
                    format!("{number}{phrase}")
                } else {
                    format!("{phrase}{number}")
                },
                false,
                None,
            );
        }
        let push_phrase = |words: &mut Tokens| {
            if let Some(phrase) = &phrase {
                for (word, joiner) in phrase.words.iter().zip(&phrase.joiners) {
                    words.push(word.clone(), false, *joiner);
                }
            }
        };
        if !glue {
            if !after {
                push_phrase(&mut words);
            }
            for (word, joiner) in value.words.into_iter().zip(value.joiners) {
                words.push(word, false, joiner);
            }
            if after {
                push_phrase(&mut words);
            }
        }
        self.insert_unit(groups, words, random);
    }

    fn insert_unit(&self, groups: &mut Vec<Group>, words: Tokens, random: &mut Random) {
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

    fn context(&mut self, id: RecordId, random: &mut Random) -> Result<Areas> {
        let Some(context) = self.pack.boundary_context(id)? else {
            return Ok(Areas::default());
        };
        let tuple = context.admin_context;
        let (locality, district) = match tuple.locality_record_id() {
            Some(locality) => (
                self.admin_name(Some(locality), Field::Locality, random)?,
                self.admin_name(tuple.district_record_id(), Field::Locality, random)?,
            ),
            None => (
                self.admin_name(tuple.district_record_id(), Field::Locality, random)?,
                None,
            ),
        };
        let district = district.filter(|district| !self.names_a_locality(words(district).as_ref()));
        let mut region = self.admin_name(tuple.region_record_id(), Field::Region, random)?;
        let locality = match locality {
            None if district.is_none()
                && self.names_a_locality(region.as_deref().and_then(words).as_ref()) =>
            {
                region.take()
            }
            locality => locality,
        };
        if let Some(group) = self.region_group(tuple.region_record_id(), random)? {
            region = Some(match region {
                Some(region) if random.chance(REGION_AND_GROUP_SHARE) => {
                    if group.chars().next().is_some_and(is_unspaced) {
                        format!("{region}{group}")
                    } else {
                        format!("{region} {group}")
                    }
                }
                _ => group,
            });
        }
        let countries = self.names(tuple.country_record_id())?;
        let country_code = self.country_code(tuple.country_record_id(), &countries);
        let country = pick_name(&countries, random).map(str::to_string);
        Ok(Areas {
            locality,
            district,
            island: self.name(tuple.place_record_id(), random)?,
            suburb: self.name(tuple.neighbourhood_record_id(), random)?,
            region,
            country,
            country_code,
        })
    }

    fn region_group(
        &mut self,
        id: Option<RecordId>,
        random: &mut Random,
    ) -> Result<Option<String>> {
        let Some(id) = id.and_then(|id| u32::try_from(id).ok()) else {
            return Ok(None);
        };
        let groups = self.tagger.dictionaries().region_groups(id);
        if groups.is_empty() || !random.chance(REGION_GROUP_SHARE) {
            return Ok(None);
        }
        let group = groups[random.below(groups.len() as u64) as usize];
        self.admin_name(Some(RecordId::from(group)), Field::Region, random)
    }

    fn country_code(&mut self, id: Option<RecordId>, names: &[String]) -> Option<CountryCode> {
        let read = || {
            names
                .first()
                .and_then(|name| search_key(name))
                .and_then(|name| self.tagger.dictionaries().get(Field::Country, &name))
                .and_then(|entry| entry.country)
        };
        let Some(id) = id else {
            return read();
        };
        if let Some(&code) = self.country_codes.get(&id) {
            return code;
        }
        let code = read();
        self.country_codes.insert(id, code);
        code
    }

    fn name(&mut self, id: Option<RecordId>, random: &mut Random) -> Result<Option<String>> {
        let names = self.names(id)?;
        Ok(pick_name(&names, random).map(str::to_string))
    }

    fn admin_name(
        &mut self,
        id: Option<RecordId>,
        field: Field,
        random: &mut Random,
    ) -> Result<Option<String>> {
        let Some(name) = self.name(id, random)? else {
            return Ok(None);
        };
        if random.chance(ADMIN_ALIAS_SHARE)
            && let Some(key) = dictionary_key(&name)
        {
            let aliases = admin_aliases(&key)
                .into_iter()
                .filter(|alias| self.tagger.dictionaries().contains(field, alias))
                .collect::<Vec<_>>();
            if !aliases.is_empty() {
                let pick = random.below(aliases.len() as u64) as usize;
                return Ok(aliases.into_iter().nth(pick));
            }
        }
        Ok(Some(name))
    }

    fn names(&mut self, id: Option<RecordId>) -> Result<Rc<[String]>> {
        let Some(id) = id else {
            return Ok(Rc::default());
        };
        if let Some(names) = self.names.get(&id) {
            return Ok(Rc::clone(names));
        }
        let names = Rc::<[String]>::from(self.pack.records().names(id)?);
        self.names.insert(id, Rc::clone(&names));
        Ok(names)
    }
}

fn standalone_unit_words() -> &'static std::collections::HashSet<String> {
    static WORDS: OnceLock<std::collections::HashSet<String>> = OnceLock::new();
    WORDS.get_or_init(|| {
        STANDALONE_UNIT_WORDS
            .iter()
            .flat_map(|list| list.lines())
            .filter(|line| !line.starts_with('#'))
            .flat_map(|line| line.split('|'))
            .filter_map(words)
            .map(|tokens| tokens.words.join(" "))
            .collect()
    })
}

fn pick_name<'n, S: AsRef<str>>(names: &'n [S], random: &mut Random) -> Option<&'n str> {
    let name = match names {
        [] => return None,
        [only] => only,
        [first, others @ ..] => {
            if random.chance(ALT_NAME_SHARE) {
                &others[random.below(others.len() as u64) as usize]
            } else {
                first
            }
        }
    };
    Some(name.as_ref())
}

#[derive(Debug, Default)]
struct Areas {
    locality: Option<String>,
    district: Option<String>,
    island: Option<String>,
    suburb: Option<String>,
    region: Option<String>,
    country: Option<String>,
    country_code: Option<CountryCode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Draw {
    Layer(Layer),
    BlockAddress,
}

const SEEN_NAMES: usize = 4_096;
const FIELD_TWICE_SHARE: f64 = 0.05;
const ADDRESS_TWICE_SHARE: f64 = 0.01;
const MIDDLE_UNIT_SHARE: f64 = 0.05;
const VENUE_BEFORE_SHARE: f64 = 0.03;
const EXTRA_REGION_SHARE: f64 = 0.04;
const NUMBER_SUFFIX_SHARE: f64 = 0.05;

const BLOCK_ADDRESS_SHARE: f64 = 0.25;

fn is_block_address(record: &Record) -> bool {
    matches!(record, Record::Address(record)
        if record.address.street.is_none() && !record.address.areas.is_empty())
}

fn address_fields(address: &AddressComponents, fields: &mut Fields) {
    fields.house_number = words(&address.number);
    fields.street = address.street.as_deref().and_then(words);
    fields.suburb = words(&address.areas.join(" "));
    fields.unit = address.unit.as_deref().and_then(words);
    fields.postcode = address.postcode.as_deref().and_then(words);
}

fn with_country_prefix(postcode: &Tokens, country: CountryCode, random: &mut Random) -> Tokens {
    let code = country.as_str().to_ascii_lowercase();
    let prefix = if random.chance(0.5) {
        code
    } else {
        code[..1].to_string()
    };
    let mut written = Tokens::default();
    let mut words = postcode.words.iter().zip(&postcode.joiners);
    let Some((first, _)) = words.next() else {
        return postcode.clone();
    };
    match random.below(3) {
        0 => written.push(format!("{prefix}{first}"), false, None),
        1 => {
            written.push(prefix, false, None);
            written.push(first.clone(), false, Some('-'));
        }
        _ => {
            written.push(prefix, false, None);
            written.push(first.clone(), false, None);
        }
    }
    for (word, joiner) in words {
        written.push(word.clone(), false, *joiner);
    }
    written
}

fn number_areas(fields: &mut Fields, random: &mut Random) {
    let Some(areas) = fields.suburb.as_mut() else {
        return;
    };
    for word in &mut areas.words {
        let split = word
            .char_indices()
            .find(|(_, character)| !is_numeral(*character))
            .map_or(word.len(), |(index, _)| index);
        let (number, kind) = word.split_at(split);
        if !number.is_empty()
            && dictionary_key(kind).is_some_and(|kind| is_admin_type(&kind))
            && random.chance(NUMBERED_AREA_SHARE)
        {
            *word = number.to_string();
        }
    }
}

fn chained_areas(address: &AddressComponents, random: &mut Random) -> Option<Tokens> {
    let region = address.region.as_deref().filter(|_| random.chance(0.5));
    let locality = address.locality.as_deref().or(address.place.as_deref());
    let parts = region
        .into_iter()
        .chain(locality)
        .chain(address.areas.iter().map(String::as_str))
        .collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }
    let mut chain = String::new();
    for part in parts {
        let glued = chain.chars().last().is_some_and(is_unspaced)
            || part.chars().next().is_some_and(is_unspaced);
        if !chain.is_empty() && !glued {
            chain.push(' ');
        }
        chain.push_str(part);
    }
    words(&chain)
}

fn in_numerals(word: &str) -> String {
    static DIGITS: OnceLock<[Option<char>; 10]> = OnceLock::new();
    let digits = DIGITS.get_or_init(|| {
        let mut digits = [None; 10];
        for line in NUMERALS.lines() {
            if let Some((character, value)) = line.split_once('\t')
                && let (Some(character), Ok(value)) =
                    (character.chars().next(), value.trim().parse::<usize>())
                && value < 10
                && digits[value].is_none()
            {
                digits[value] = Some(character);
            }
        }
        digits
    });
    word.chars()
        .map(|character| {
            character
                .to_digit(10)
                .filter(|_| character.is_ascii_digit())
                .and_then(|digit| digits[digit as usize])
                .unwrap_or(character)
        })
        .collect()
}

fn words(value: &str) -> Option<Tokens> {
    let tokens = Tokens::new(value);
    (!tokens.is_empty()).then_some(tokens)
}

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
    use crate::test_support::TempDir;

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
        let mut no_number = fields();
        no_number.house_number = None;
        assert_eq!(rendered("CA", &no_number)[0].0, Street);
        assert!(templates().len() > 200 && templates().contains_key("ZZ"));
    }

    #[test]
    fn a_share_of_rows_carries_each_noise_of_hand_written_queries() {
        use Label::*;
        let mut seen = Seen::default();
        let unit_words = [
            vec!["unit".to_string()],
            Vec::new(),
            vec!["floor".to_string()],
        ];
        let mut random = Random::new(7);
        let rows = 4_000;
        let mut counts = [0; 5];
        for row in 0..rows {
            let mut fields = fields();
            if row == 0 {
                fields.house = words("Corner Store");
            }
            let mut groups = Vec::new();
            render(&templates()["CA"], &fields, &mut groups, &mut false);
            seen.add_noise(&unit_words, &fields, &mut groups, &mut random);
            let labels = groups.iter().map(|group| group.label).collect::<Vec<_>>();
            let twice = labels
                .windows(2)
                .any(|pair| pair[0] == pair[1] && matches!(pair[0], Locality | Region));
            let suffixed = groups.iter().any(|group| {
                group.label == HouseNumber
                    && group.words.words[0].ends_with(|letter: char| letter.is_ascii_lowercase())
            });
            for (count, noisy) in counts.iter_mut().zip([
                twice,
                labels.contains(&Unit),
                row > 0 && labels.first() == Some(&Other),
                labels.iter().filter(|label| **label == Region).count() > 1,
                suffixed,
            ]) {
                *count += usize::from(noisy);
            }
        }
        assert!(
            counts.iter().all(|&count| count > 0),
            "every noise occurs: {counts:?}"
        );
        assert!(
            counts.iter().all(|&count| count < rows / 5),
            "each in a small share of rows: {counts:?}"
        );
    }

    #[test]
    fn an_island_is_a_field_between_the_localities_and_the_region() {
        use Label::*;
        let mut with_island = fields();
        with_island.island = words("Honshu");
        let template = &templates()["CA"];
        assert!(!writes_slot(template, Slot::Island));
        let mut groups = Vec::new();
        render(template, &with_island, &mut groups, &mut false);
        insert_area(
            &mut groups,
            Label::Locality,
            with_island.island.as_ref().expect("island"),
        );
        let labels = groups
            .iter()
            .map(|group| (group.label, group.words.words.join(" "), group.break_before))
            .collect::<Vec<_>>();
        assert_eq!(labels[2], (Locality, "toronto".to_string(), true));
        assert_eq!(labels[3], (Locality, "honshu".to_string(), true));
        assert_eq!(labels[4], (Region, "on".to_string(), true));
    }

    fn address(street: Option<&str>, areas: &[&str]) -> Record {
        Record::Address(crate::record::AddressRecord {
            building: None,
            access: Vec::new(),
            disputed: false,
            street_postcodes: Vec::new(),
            address: AddressComponents {
                block_number_len: None,
                areas: areas.iter().map(|area| area.to_string()).collect(),
                number: "10-8".into(),
                street: street.map(str::to_string),
                place: None,
                unit: None,
                locality: None,
                region: None,
                postcode: None,
                country: None,
            },
            geometry: crate::record::point_geometry(0.0, 0.0),
            location_precision: crate::record::LocationPrecision::Point,
            source: crate::record::SourceProvenance::osm(crate::record::ObjectType::Node, 1),
        })
    }

    #[test]
    fn the_sample_holds_a_fixed_share_of_each_country() {
        let street = address(Some("King Street"), &[]);
        let block = address(None, &["5丁目"]);
        let mut sampler = Sampler::default();
        let addresses = quota(Draw::Layer(Layer::Address)).expect("quota");
        let blocks = quota(Draw::BlockAddress).expect("quota");
        for id in 0..3 * addresses as u64 {
            sampler.offer(id, Some(1), &street);
        }
        for id in 0..10 {
            sampler.offer(1_000_000 + id, Some(2), &block);
            sampler.offer(2_000_000 + id, None, &street);
        }
        let capacity = |quota: usize| (quota as f64 * OVERSAMPLE).ceil() as usize;
        let held = |country, draw| sampler.reservoirs[&(country, draw)].ids.len();
        assert_eq!(
            held(Some(1), Draw::Layer(Layer::Address)),
            capacity(addresses)
        );
        assert!(capacity(blocks) > 10);
        assert_eq!(held(Some(2), Draw::Layer(Layer::Address)), 10);
        assert_eq!(held(Some(2), Draw::BlockAddress), 10);
        assert_eq!(held(None, Draw::Layer(Layer::Address)), 10);

        let candidates = sampler.candidates(&mut Random::new(5));
        let large = &candidates[&Some(1)];
        assert!(large.windows(2).all(|pair| pair[0].id < pair[1].id));
        assert!(large.last().expect("drawn").id > 2 * addresses as u64);
        let small = &candidates[&Some(2)];
        assert_eq!(small.len(), 10);
        assert!(
            small
                .iter()
                .all(|candidate| candidate.draw == Draw::BlockAddress)
        );
    }

    #[test]
    fn rows_round_trip_through_scratch_files() {
        let mut tokens = Tokens::new("670 king st, toronto 1/2");
        tokens.push("東京".to_string(), true, Some('-'));
        let mut suppression = Suppression::default();
        suppression.hide(Field::Street, 1, 3);
        suppression.hide(Field::Poi, 0, 1);
        let row = Row {
            gold: vec![Label::HouseNumber; tokens.len()],
            tokens,
            suppression,
            block_address: true,
        };
        let mut bytes = Vec::new();
        row.encode(&mut bytes);
        let mut input = bytes.as_slice();
        let read = Row::decode(&mut input).expect("decode");
        assert!(input.is_empty());
        assert_eq!(read.tokens, row.tokens);
        assert_eq!(read.gold, row.gold);
        assert_eq!(read.suppression.0, row.suppression.0);
        assert!(read.block_address);
    }

    #[test]
    fn block_scheme_addresses_are_numbered_in_areas_without_a_street() {
        assert!(is_block_address(&address(None, &["5丁目"])));
        assert!(!is_block_address(&address(Some("King Street"), &["Annex"])));
        assert!(!is_block_address(&address(None, &[])));
    }

    #[test]
    fn stated_areas_chain_into_one_field() {
        let address = AddressComponents {
            block_number_len: None,
            areas: vec!["西区".into(), "みなとみらい".into(), "3丁目".into()],
            number: "3-1".into(),
            street: None,
            place: None,
            unit: None,
            locality: Some("横浜市".into()),
            region: Some("神奈川県".into()),
            postcode: None,
            country: None,
        };
        let mut chains = std::collections::BTreeSet::new();
        let mut random = Random::new(3);
        for _ in 0..40 {
            let chain = chained_areas(&address, &mut random).expect("chain");
            chains.insert(chain.words.join(" "));
        }
        assert_eq!(
            chains.into_iter().collect::<Vec<_>>(),
            [
                "横浜市西区みなとみらい3丁目",
                "神奈川県横浜市西区みなとみらい3丁目"
            ]
        );
        let latin = AddressComponents {
            areas: vec!["Liberty Village".into()],
            locality: Some("Toronto".into()),
            region: None,
            ..address.clone()
        };
        assert_eq!(
            chained_areas(&latin, &mut random).expect("chain").words,
            ["toronto", "liberty", "village"]
        );
        let alone = AddressComponents {
            areas: Vec::new(),
            region: None,
            ..address
        };
        assert!(chained_areas(&alone, &mut random).is_none());
    }

    #[test]
    fn numbers_are_written_in_numeral_ideographs_digit_by_digit() {
        assert_eq!(in_numerals("10-8"), "一零-八");
        assert_eq!(in_numerals("3丁目"), "三丁目");
        assert!(in_numerals("205").chars().all(is_numeral));
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
    fn forward_backward_sums_every_labelling() {
        let tagger = LatticeTagger::new(Dictionaries::empty(), Weights::built_in());
        let lattice = tagger.lattice(&Tokens::new("kingsway 12"), &Suppression::default());
        assert_eq!(lattice.spans.len(), 2);
        let weights = Weights::built_in();
        let paths = decode::decode(&lattice, &weights, None, LABELS * LABELS, None);
        assert_eq!(paths.len(), LABELS * LABELS);
        let brute = log_sum(paths.iter().map(|path| f64::from(path.score)));
        let edges = span_edges(&lattice, &weights, None);
        let order = span_order(&lattice);
        let reading = Reading::new(&lattice, &weights, None, None, &order, edges);
        assert!(
            (reading.log_z - brute).abs() < 1e-3,
            "{} {brute}",
            reading.log_z
        );
        let mut gradient = Gradient::default();
        reading.add_expectations(&lattice, None, 1.0, &mut gradient);
        let bias = super::super::features::by_name("bias:bias") as usize;
        let (values, set) = gradient.edge[&bias];
        let total = (0..LABELS)
            .filter(|label| set & (1 << label) != 0)
            .map(|label| values[label])
            .sum::<f32>();
        assert!((total - 2.0).abs() < 1e-3, "{total}");
    }

    #[test]
    fn the_model_learns_a_toy_language() {
        use Label::*;
        let entry = Entry {
            count: 10,
            country: None,
            levels: 0,
        };
        let dictionaries = Dictionaries::from_entries([
            (Field::Locality, "toronto", entry),
            (Field::Street, "king street", entry),
        ]);
        let tagger = LatticeTagger::new(dictionaries, Weights::zero());
        let row = |text: &str, gold: Vec<Label>| Row {
            tokens: Tokens::new(text),
            gold,
            suppression: Suppression::default(),
            block_address: false,
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
        let mut model = Model::from_weights(Weights::zero());
        for _ in 0..10 {
            for row in &rows {
                model.learn(&tagger.lattice(&row.tokens, &row.suppression), &row.gold);
            }
        }
        let mut tally = Tally::default();
        tally.add(&tagger, &model.averaged(), &rows);
        let accuracy = tally.accuracy();
        assert_eq!(accuracy.rows, 1.0, "{accuracy:?}");
    }

    #[test]
    fn the_shuffle_reads_every_row_exactly_once() {
        for len in [0, 1, 7, SHUFFLE_WINDOW, SHUFFLE_WINDOW + 4_321] {
            let mut order = shuffled_order(len, &mut Random::new(len as u64));
            order.sort_unstable();
            assert!(order.into_iter().eq(0..len as u32), "every row once");
        }
    }

    #[test]
    fn lattices_read_back_from_scratch_equal_those_kept_in_memory() {
        let entry = |country| Entry {
            count: 10,
            country: CountryCode::parse(country),
            levels: 0,
        };
        let dictionaries = Dictionaries::from_entries([
            (Field::Locality, "toronto", entry("CA")),
            (Field::Street, "king street", entry("CA")),
            (Field::Region, "ontario", entry("CA")),
        ])
        .with_areas(&[("toronto", &[7]), ("ontario", &[3])], &[(7, 3)]);
        let tagger = LatticeTagger::new(dictionaries, Weights::zero());
        let rows = [
            "670 King Street West, Toronto, ON M5V 1M9",
            "Kign Stret 12 Torono",
            "東京都千代田区丸の内2-7-2",
            "toronto",
        ]
        .into_iter()
        .map(|text| {
            let tokens = Tokens::new(text);
            Learned {
                gold: vec![Label::Other; tokens.len()],
                lattice: tagger.lattice(&tokens, &Suppression::default()),
            }
        })
        .collect::<Vec<_>>();
        let dir = TempDir::new("lattices");
        let scratch = Scratch::create(dir.join("scratch")).expect("scratch");
        for (budget, in_memory) in [(Budget::unlimited(), true), (Budget::new(1), false)] {
            let mut cache = LatticeCache::new(&scratch, &budget);
            for row in &rows {
                cache.push(row.clone()).expect("push");
            }
            let cache = cache.finish().expect("finish");
            assert_eq!(matches!(cache, Lattices::Memory(..)), in_memory);
            assert_eq!(cache.len(), rows.len());
            for index in [2, 0, 3, 1] {
                cache
                    .visit(index, |row| assert_eq!(row, &rows[index]))
                    .expect("visit");
            }
        }
    }
}
