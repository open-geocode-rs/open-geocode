//! Training the parser's weights from the Pack's own records, inside `build`.
//!
//! While the builder writes records, a [`Sampler`] draws a uniform sample of
//! them for each country and kind of record (Vitter's Algorithm R), so a
//! planet build trains each country on as many rows as a build of that
//! country alone, in memory that does not grow with the input. Once the Pack
//! is sealed, [`train`] reads the drawn records back in Pack order and writes
//! them out the way people write addresses: through an address template of
//! OpenCageData's address-formatting
//! (<https://github.com/OpenCageData/address-formatting>, MIT, see `NOTICE`),
//! vendored as `data/templates.tsv`, with the noise real queries carry:
//! commas dropped, fields left out or swapped, street types abbreviated, a
//! unit added, one-letter typos. Some rows hide the dictionary hits of their
//! street or venue, so the model also learns to read names the Pack does not
//! know from their shape and place alone; the areas a row names are always
//! names the Pack knows, as a locality or region must be.
//!
//! The weights are those of a semi-Markov conditional random field (Sarawagi
//! and Cohen, 2004), learned by stochastic gradient ascent on each row's
//! log-likelihood with AdaGrad step sizes: move every weight by the expected
//! count of its feature over the gold labellings less that over all
//! labellings, both from forward-backward over the row's lattice. Averaging
//! the weights over every step keeps the model from following the last rows
//! it saw. Rows stream from scratch files, so training memory does not grow
//! with the number of countries either. A lattice does not depend on the
//! weights, so each row's is built once and read by every epoch: from memory
//! while the build's pool grants it, else from a scratch file.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, OnceLock},
    time::Instant,
};

use crate::error::{Error, Result};
use rayon::prelude::*;
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
    extsort::{ExternalSorter, IndexedSpool, IndexedSpoolFile, Scratch, Spill, Spool, SpoolFile},
    memory::{Budget, Reservation, Spillable},
    pack::{Reader, RecordId},
    parse::types::Label,
    record::{AddressComponents, Layer, PlaceLayer, Record},
    resources::Resources,
    text_index::normalize_index_text,
    util::codec::{get_string, get_u8, get_u64, put_str, put_u64},
};

/// `CC<TAB>template` rows, `ZZ` the default.
const TEMPLATES: &str = include_str!("data/templates.tsv");
/// Training rows rendered for each country of the Pack. Deepparse trains on
/// 100,000 addresses per country; on the Japan Pack 200,000 rows label the
/// held-out rows and the libpostal bench as well as 400,000, and 100,000
/// worse.
pub const ROWS_PER_COUNTRY: usize = 200_000;
/// Passes over the training rows.
pub const EPOCHS: usize = 5;
/// Seed of the record draw, the noise and the row order, so a build of the
/// same input trains the same weights.
const SEED: u64 = 1;
/// A Pack that renders fewer rows than this parses with the built-in
/// weights: too few rows teach a model less than the hand-set weights know.
const MIN_TRAIN_ROWS: usize = 1_000;
/// Shards trained side by side: more shards see fewer rows between two mixes
/// and converge slower. Fixed, not taken from the cores, so a build trains
/// the same weights on every machine.
const TRAIN_SHARDS: usize = 4;
/// The share of rendered rows held out to measure the model.
const HELD_OUT_SHARE: f64 = 0.05;
/// Records drawn per wanted row: a few draws render no row.
const OVERSAMPLE: f64 = 1.25;
/// Rows a shard holds to read its rows in an order of its own each epoch.
const SHUFFLE_WINDOW: usize = 1 << 16;
/// Rows whose lattices are built side by side.
const LATTICE_BATCH: usize = 1 << 10;
/// Held-out rows labelled per parallel batch.
const EVALUATE_BATCH: usize = 1 << 12;

/// What one country's rows are made of: mostly addresses, then POIs,
/// streets, places and postcodes. Block scheme addresses have a share of
/// their own on top of the addresses', so their house numbers and the
/// streets beside them are seen often.
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

/// The rows one country renders of a kind of record, or `None` for a kind
/// that is not trained on.
fn quota(draw: Draw) -> Option<usize> {
    SHARES
        .iter()
        .find(|(kind, _)| *kind == draw)
        .map(|(_, share)| ((ROWS_PER_COUNTRY as f64 * share).ceil() as usize).max(1))
}

/// What a training run did and how well the weights label held-out rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Report {
    pub rows_per_country: usize,
    /// Rendered rows by the `name` of the country record they lie in.
    pub rows_by_country: BTreeMap<String, usize>,
    pub rendered_rows: usize,
    /// Rendered rows with a house number, and rows of block scheme
    /// addresses (see [`BLOCK_ADDRESS_SHARE`]).
    pub house_number_rows: usize,
    pub block_address_rows: usize,
    pub held_out_rows: usize,
    /// Memory the record sample held while records were written.
    pub sample_bytes: u64,
    pub shards: usize,
    pub epochs: usize,
    /// Training rows the model got wrong in the last epoch. Only the last
    /// epoch decodes each row before learning it: the count is all the
    /// decode is for, and it costs a pass of Viterbi per row.
    pub last_epoch_errors: usize,
    /// Countries with their own transition table.
    pub countries: Vec<String>,
    /// Whether the Pack carries the trained weights; without them it parses
    /// with the built-in ones.
    pub trained: bool,
    pub accuracy: Accuracy,
    pub seconds: f64,
    /// Seconds spent rendering rows from the Pack, building their lattices,
    /// and learning from them.
    pub render_seconds: f64,
    pub lattice_seconds: f64,
    pub learn_seconds: f64,
    /// Bytes the training rows' lattices took, in memory and in scratch
    /// files, and how many shards read theirs from a file.
    pub lattice_memory_bytes: u64,
    pub lattice_file_bytes: u64,
    pub lattice_file_shards: usize,
    /// Whether the Pack was read into memory up front, as it is when it
    /// fits in half the memory available.
    pub pack_prefaulted: bool,
}

/// Token and whole-row accuracy on the held-out rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Accuracy {
    pub tokens: f64,
    pub rows: f64,
    /// Per gold label: the share of its tokens labelled right.
    pub by_label: BTreeMap<String, f64>,
}

/// Records drawn for training while the Pack is written: for each country
/// and kind of record a uniform sample of a fixed size.
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

    /// Bytes the sample holds.
    pub fn resident_bytes(&self) -> usize {
        self.reservoirs
            .values()
            .map(|reservoir| {
                std::mem::size_of::<((Option<RecordId>, Draw), Reservoir)>()
                    + reservoir.ids.capacity() * std::mem::size_of::<RecordId>()
            })
            .sum()
    }

    /// The drawn records of each country, each with the kind it was drawn
    /// as and its rank in a random order of its kind's draws, in record
    /// order. A block scheme address drawn for both of its kinds is drawn
    /// once, as a block address.
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
    // Every lattice looks names up all over the dictionaries.
    report.pack_prefaulted = pack
        .container()
        .advise_random_reads(Resources::probe().available_memory_bytes);
    // Only the dictionaries: the weights are what this trains.
    let tagger = LatticeTagger::new(Dictionaries::open(pack.container())?, Weights::zero());
    let mut random = Random::new(SEED);

    // Each country's rows, rendered from its drawn records read in Pack
    // order, and sorted by a random key into one random order of all rows.
    let mut shuffled =
        ExternalSorter::<Shuffled>::new(scratch, "parse-rows", memory, sort_budget_bytes);
    let mut renderer = Renderer::new(&pack, &tagger);
    let candidates = sample.candidates(&mut random);
    // Records in no country stand for a country only in a Pack that has
    // none, such as a city extract cut inside its country's boundary. Beside
    // countries they are the strip an extract takes past a border, and would
    // take a whole country's rows.
    let in_countries = candidates.keys().any(Option::is_some);
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
        // The areas of one country are not those of the next.
        renderer.names.clear();
        let mut rows = 0;
        for (draw, mut drawn) in drawn {
            drawn.sort_unstable_by_key(|(rank, _)| *rank);
            let quota = quota(draw).expect("only kinds with a quota are drawn");
            for (_, row) in drawn.into_iter().take(quota) {
                shuffled.push(Shuffled {
                    key: random.next(),
                    row,
                })?;
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

    // Every row is held out or goes to one shard, in the random order.
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

    // Every row's lattice, built once on every core. Each shard's cache is
    // open on the pool before any is filled, so they split it fairly.
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

    // Iterative parameter mixing (McDonald, Hall and Mann 2010): every epoch
    // each shard of the rows trains its own model from the mixed weights,
    // in parallel, and the shards' weights are averaged. The result is the
    // average over epochs of the shards' averaged weights. Each shard reads
    // its rows in an order of its own seed, so the weights do not depend on
    // scheduling.
    let mut mixed = Weights::zero();
    let mut averages = Vec::with_capacity(EPOCHS);
    for epoch in 0..EPOCHS {
        let results = caches
            .par_iter()
            .enumerate()
            .map(|(shard, cache)| -> Result<_> {
                let mut order = Random::new(SEED ^ ((epoch as u64) << 32 | shard as u64));
                let mut model = Model::from_weights(mixed.clone());
                let mut errors = 0;
                let count = epoch + 1 == EPOCHS;
                let at = Instant::now();
                for (seen, index) in shuffled_order(cache.len(), &mut order)
                    .into_iter()
                    .enumerate()
                {
                    if seen % LATTICE_BATCH == 0 {
                        scratch.check_cancelled()?;
                    }
                    cache.visit(index as usize, |row| {
                        if count {
                            errors += usize::from(model.is_wrong(&row.lattice, &row.gold));
                        }
                        model.learn(&row.lattice, &row.gold);
                    })?;
                }
                Ok((
                    model.averaged(),
                    model.weights,
                    errors,
                    at.elapsed().as_secs_f64(),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        // Per-shard seconds, spread over the shards that ran side by side.
        report.learn_seconds += results.iter().map(|result| result.3).sum::<f64>() / shards as f64;
        report.last_epoch_errors = results.iter().map(|result| result.2).sum();
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

/// The indices `0..len` in a random order drawn through a window of
/// [`SHUFFLE_WINDOW`] indices: once the window is full, each next index
/// takes the place of a random one in it, which comes next, and the window
/// ends in a random order of its own. A shard reads its rows in this order.
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

/// A training row as learning reads it: its gold labels and its lattice.
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

/// One shard's rows with their lattices, as they are built: in memory while
/// its fair share of the pool grants them, else, from the row the pool
/// refuses on, every row in a scratch file read back by index.
struct LatticeCache {
    scratch: Arc<Scratch>,
    reservation: Spillable,
    rows: Vec<Learned>,
    file: Option<IndexedSpool<Learned>>,
}

/// One shard's rows with their lattices, built.
enum Lattices {
    Memory(Vec<Learned>, Reservation),
    File(IndexedSpoolFile<Learned>),
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
        // The encoded rows take less than their resident bytes.
        self.scratch
            .check_room((self.reservation.bytes() + bytes) as u64)?;
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
            Some(file) => Lattices::File(file.finish()?),
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

    /// Call `visit` with row `index`.
    fn visit(&self, index: usize, visit: impl FnOnce(&Learned)) -> Result<()> {
        match self {
            Self::Memory(rows, _) => visit(&rows[index]),
            Self::File(file) => visit(&file.get(index)?),
        }
        Ok(())
    }
}

/// A row with the random key that orders it among all rows.
#[derive(Debug)]
struct Shuffled {
    key: u64,
    row: Row,
}

impl PartialEq for Shuffled {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
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
        self.key.cmp(&other.key)
    }
}

impl Spill for Shuffled {
    fn encode(&self, out: &mut Vec<u8>) {
        put_u64(out, self.key);
        self.row.encode(out);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(Self {
            key: get_u64(input)?,
            row: Row::decode(input)?,
        })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<u64>() + self.row.resident_bytes()
    }
}

/// One labelled training row.
#[derive(Debug, Clone)]
pub(super) struct Row {
    pub tokens: Tokens,
    pub gold: Vec<Label>,
    pub suppression: Suppression,
    /// Whether the row was rendered from a block scheme address.
    pub block_address: bool,
}

/// The dictionaries a training row hides its hits in.
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
                code => Some(u32::try_from(code).ok().and_then(char::from_u32).ok_or(
                    Error::BadFixtureRow("invalid joiner in a training row".into()),
                )?),
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

/// The model being trained: a semi-Markov conditional random field
/// (Sarawagi and Cohen, 2004) over the lattice, learned by stochastic
/// gradient ascent on each row's log-likelihood with AdaGrad step sizes
/// (Duchi, Hazan and Singer, 2011), and averaged over every step.
///
/// The likelihood of a row sums over every segmentation that gives its
/// tokens their gold labels and over the countries the lattice is decoded
/// as, since neither is given, as neither is for a query. Its gradient is
/// the expected features of the gold labellings less those of all
/// labellings, both from forward-backward over the lattice.
#[derive(Debug)]
struct Model {
    weights: Weights,
    /// Every update times the step it was made at, to average lazily.
    totals: Weights,
    /// The sum of every squared gradient, for AdaGrad's step sizes.
    squares: Weights,
    step: f32,
}

/// AdaGrad's base step size.
const LEARNING_RATE: f32 = 0.1;
/// A feature whose expected count differs by less than this between the gold
/// labellings and all labellings is left as it is: the cost of a row is then
/// the spans and labels the model is unsure of, not every one.
const MIN_GRADIENT: f64 = 1e-4;

impl Model {
    /// A model that starts from `weights`.
    fn from_weights(weights: Weights) -> Self {
        Self {
            weights,
            totals: Weights::zero(),
            squares: Weights::zero(),
            step: 1.0,
        }
    }

    /// Whether the best labelling of a row, labelled `gold` by token, is
    /// wrong under the current weights.
    fn is_wrong(&self, lattice: &Lattice, gold: &[Label]) -> bool {
        let predicted = decode::best(lattice, &self.weights, 1)
            .into_iter()
            .next()
            .expect("a lattice with tokens has a path");
        predicted.labels(lattice) != lattice.units.expand(gold)
    }

    /// Learn from one row, labelled `gold` by token.
    fn learn(&mut self, lattice: &Lattice, gold: &[Label]) {
        // A token's label is the label of every unit of it.
        let gold = lattice.units.expand(gold);
        let gradient = Gradient::of(lattice, &self.weights, &gold);
        for (index, value) in gradient.edge {
            let delta = adagrad(&mut self.squares.edge[index], value);
            self.weights.edge[index] += delta;
            self.totals.edge[index] += delta * self.step;
        }
        for (country, values) in gradient.transitions {
            for (index, value) in values.into_iter().enumerate() {
                if f64::from(value).abs() < MIN_GRADIENT {
                    continue;
                }
                let delta = adagrad(&mut table(&mut self.squares, country)[index], value);
                table(&mut self.weights, country)[index] += delta;
                table(&mut self.totals, country)[index] += delta * self.step;
            }
        }
        self.step += 1.0;
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

/// AdaGrad's step for one weight: the gradient scaled down by the root of
/// every squared gradient the weight has had.
fn adagrad(squares: &mut f32, gradient: f32) -> f32 {
    *squares += gradient * gradient;
    LEARNING_RATE * gradient / (squares.sqrt() + 1e-6)
}

/// The transition table of `country` in `weights`, the global one for
/// `None`, made when it is missing.
fn table(weights: &mut Weights, country: Option<CountryCode>) -> &mut Vec<f32> {
    match country {
        None => &mut weights.global,
        Some(country) => weights
            .countries
            .entry(country)
            .or_insert_with(|| vec![0.0; STATES * STATES]),
    }
}

/// The gradient of one row's log-likelihood: per edge weight index, and per
/// transition table (`None` the global one) by previous and next state.
#[derive(Debug, Default)]
struct Gradient {
    edge: HashMap<usize, f32>,
    transitions: HashMap<Option<CountryCode>, Vec<f32>>,
}

impl Gradient {
    fn of(lattice: &Lattice, weights: &Weights, gold: &[Label]) -> Self {
        let countries = decode::countries(lattice);
        let readings = countries
            .iter()
            .map(|country| {
                (
                    Reading::new(lattice, weights, *country, None),
                    Reading::new(lattice, weights, *country, Some(gold)),
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
                // How much of the row's probability this country carries.
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

/// Forward-backward over a lattice decoded as one country, over every
/// labelling or only those that give each unit its gold label.
struct Reading<'a> {
    gold: Option<&'a [Label]>,
    /// `edges[span][label]`.
    edges: Vec<[f64; LABELS]>,
    transitions: [[f64; STATES]; STATES],
    /// `alpha[position][state]`: the log-sum of the paths ending there.
    alpha: Vec<[f64; STATES]>,
    /// `beta[position][state]`: the log-sum of the rest of the paths from a
    /// segment of `state` ending at `position`.
    beta: Vec<[f64; STATES]>,
    log_z: f64,
}

impl<'a> Reading<'a> {
    fn new(
        lattice: &Lattice,
        weights: &Weights,
        country: Option<CountryCode>,
        gold: Option<&'a [Label]>,
    ) -> Self {
        let table = weights.transitions(country);
        let mut transitions = [[0.0; STATES]; STATES];
        for (previous, row) in transitions.iter_mut().enumerate() {
            for (next, value) in row.iter_mut().enumerate() {
                *value = f64::from(table.get(previous, next));
            }
        }
        let edges = lattice
            .spans
            .iter()
            .map(|span| Label::ALL.map(|label| f64::from(weights.edge(span, label, country))))
            .collect::<Vec<_>>();
        let allowed = |span: &decode::Span, label: Label| {
            gold.is_none_or(|gold| gold[span.start..span.end].iter().all(|gold| *gold == label))
        };
        let len = lattice.len;
        let mut alpha = vec![[f64::NEG_INFINITY; STATES]; len + 1];
        alpha[0][START] = 0.0;
        let mut order = (0..lattice.spans.len()).collect::<Vec<_>>();
        order.sort_by_key(|index| lattice.spans[*index].end);
        for &index in &order {
            let span = &lattice.spans[index];
            for label in Label::ALL {
                if !allowed(span, label) {
                    continue;
                }
                let next = label as usize;
                let incoming = log_sum(
                    previous_states(span.start)
                        .map(|previous| alpha[span.start][previous] + transitions[previous][next]),
                );
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
                if !allowed(span, label) {
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
            gold,
            edges,
            transitions,
            alpha,
            beta,
            log_z,
        }
    }

    /// Add `scale` times the expected count of every feature and transition
    /// under this reading to `gradient`.
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
            for label in Label::ALL {
                let next = label as usize;
                if self.gold.is_some_and(|gold| {
                    gold[span.start..span.end].iter().any(|gold| *gold != label)
                }) {
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
                for feature in span.features_in(country) {
                    *gradient
                        .edge
                        .entry(feature as usize * LABELS + next)
                        .or_default() += value as f32;
                }
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

/// The states a segment starting at `start` can follow.
fn previous_states(start: usize) -> std::ops::Range<usize> {
    if start == 0 {
        START..START + 1
    } else {
        0..LABELS
    }
}

fn log_add(left: f64, right: f64) -> f64 {
    let high = left.max(right);
    if high == f64::NEG_INFINITY {
        return high;
    }
    high + ((left - high).exp() + (right - high).exp()).ln()
}

fn log_sum(values: impl Iterator<Item = f64>) -> f64 {
    values.fold(f64::NEG_INFINITY, log_add)
}

/// Token and row accuracy of `weights` on the rows of `rows`, read in
/// batches.
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

/// Counts of rows and tokens labelled right.
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
                decode::best(&lattice, weights, 1)
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

/// The fields of one address, each as the words it is written in.
#[derive(Debug, Clone, Default)]
struct Fields {
    house: Option<Tokens>,
    house_number: Option<Tokens>,
    unit: Option<Tokens>,
    street: Option<Tokens>,
    suburb: Option<Tokens>,
    locality: Option<Tokens>,
    /// The place area the record lies in outside the admin areas: an island.
    island: Option<Tokens>,
    /// The area between the locality and the region: a county, a Kreis.
    district: Option<Tokens>,
    region: Option<Tokens>,
    postcode: Option<Tokens>,
    country: Option<Tokens>,
    /// The field the row is about, which is never dropped: the region or
    /// country a row of a region or country names.
    subject: Option<Slot>,
}

/// A field of a template the parser has a label for.
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
            // A district is written on the region side, as libpostal labels
            // its state_district.
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

/// Whether `nodes` have a place for `slot`.
fn writes_slot(nodes: &[Node], slot: Slot) -> bool {
    nodes.iter().any(|node| match node {
        Node::Slot(written) => *written == slot,
        Node::First(alternatives) => alternatives.iter().any(|nodes| writes_slot(nodes, slot)),
        _ => false,
    })
}

/// Few templates write an island or a district; a query writes each as a
/// field of its own after the localities and before the region.
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

fn has_value(nodes: &[Node], fields: &Fields) -> bool {
    nodes.iter().any(|node| match node {
        Node::Slot(slot) => slot.value(fields).is_some(),
        Node::First(alternatives) => alternatives.iter().any(|nodes| has_value(nodes, fields)),
        _ => false,
    })
}

/// Share of rows that name a record or an area by one of its alternate names
/// (`name:ja-Hira`, `name:en`, ...) when it has any, so the model sees names
/// in the other scripts and languages queries write them in.
const ALT_NAME_SHARE: f64 = 0.3;
/// Share of the rows of POIs with an address that write the address beside
/// the POI's name, so the model sees a venue name next to the street and
/// areas it is in, and the POI dictionary hit on the venue alone.
const POI_ADDRESS_SHARE: f64 = 0.7;
/// Share of written units whose unit, PO box or level word follows the
/// number instead of leading it.
const UNIT_WORD_AFTER_SHARE: f64 = 0.5;
/// Share of rows that keep the commas and line breaks between their fields.
const KEEP_BREAKS_SHARE: f64 = 0.4;
/// Share of the unit fields of rows without their breaks that keep the
/// comma before them anyway, as an address written on one line keeps it.
const UNIT_BREAK_SHARE: f64 = 0.5;
/// Share of the street fields of rows whose street hits are hidden, so a
/// name before a street type is learned as a street without the dictionary.
const HIDDEN_STREET_SHARE: f64 = 0.3;
/// Share of the venue fields of rows whose POI hits are hidden.
const HIDDEN_POI_SHARE: f64 = 0.15;
/// Share of street names with a glued street type that write it apart.
const SPLIT_STREET_TYPE_SHARE: f64 = 0.15;
/// libpostal's level and unit words written without a number
/// ("erdgeschoss", "penthouse"), MIT (see `NOTICE`).
const STANDALONE_UNIT_WORDS: [&str; 2] = [
    include_str!("data/level_standalone.txt"),
    include_str!("data/unit_standalone.txt"),
];
/// Share of written units of one word and a number that glue the two into
/// one token ("4og", "apt4"), in scripts written with spaces.
const GLUED_UNIT_SHARE: f64 = 0.3;
/// Share of the rows of a venue or a place that ask for what is near it
/// ("cafe near toronto", "haus in der nähe von köln"), the phrase labelled
/// other as libpostal labels it.
const NEAR_SHARE: f64 = 0.04;
/// Share of the rows that name areas that ask for a kind of place in them
/// instead ("bäckerei in bremen", "hotels near toronto"), the kind and the
/// phrase labelled other as libpostal labels them.
const CATEGORY_SHARE: f64 = 0.03;
/// Share of the numbered areas of addresses ("2丁目") written as their
/// number alone after the area they number ("林2"), as libpostal's data
/// writes them.
const NUMBERED_AREA_SHARE: f64 = 0.3;
/// Share of the postcodes of rows written after their country's letters
/// ("de09496", "D-80331", "d 18211"), as libpostal's generator writes them
/// (`resources/postal_codes/config.yaml`).
const POSTCODE_PREFIX_SHARE: f64 = 0.05;
/// Share of rows with words of a script written without spaces whose
/// numbers are written in numeral ideographs.
const NUMERAL_SHARE: f64 = 0.1;
/// Share of rows of records on an island that name it.
const ISLAND_SHARE: f64 = 0.35;
/// Share of rows of records in a district that name it.
const DISTRICT_SHARE: f64 = 0.4;
/// Share of rows of addresses that state several areas that write them as
/// one locality field (see [`chained_areas`]).
const CHAIN_SHARE: f64 = 0.5;
/// Share of the rows naming a region, district or locality that name it
/// without the word of its type, when the dictionaries hold it so ("愛知" of
/// "愛知県", see [`admin_aliases`]).
const ADMIN_ALIAS_SHARE: f64 = 0.3;
/// Share of the rows in a region that lies in a group of regions
/// ("千葉県" in "関東地方") that name the group, as libpostal's data does
/// ("北九州市, 九州地方, 日本").
const REGION_GROUP_SHARE: f64 = 0.3;
/// Share of the rows naming a group of regions that write the region
/// before it ("東京都関東地方"), not the group alone.
const REGION_AND_GROUP_SHARE: f64 = 0.2;

/// Renders training rows from the Pack's records.
struct Renderer<'a> {
    pack: &'a Reader,
    tagger: &'a LatticeTagger,
    /// Every name of each area record, its `name` first.
    names: HashMap<RecordId, Vec<String>>,
    /// The words a unit is written with: the Pack's unit words, PO box
    /// words and level words.
    unit_words: [Vec<String>; 3],
    /// The words that ask for what is near a place.
    near: Vec<String>,
    /// The kinds of place a query asks for.
    categories: Vec<String>,
    template_countries: Vec<String>,
}

impl<'a> Renderer<'a> {
    fn new(pack: &'a Reader, tagger: &'a LatticeTagger) -> Self {
        Self {
            pack,
            tagger,
            names: HashMap::new(),
            // The vendored unit words and those the Pack's units use.
            unit_words: [Field::Unit, Field::PoBox, Field::Level]
                .map(|field| tagger.dictionaries().keys(field)),
            near: tagger.dictionaries().keys(Field::Near),
            categories: tagger.dictionaries().keys(Field::Category),
            template_countries: templates().keys().cloned().collect(),
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
                // A place is named with the areas above it, not within, in
                // the field of its layer.
                fields.country = context.country.as_deref().and_then(words);
                match layer {
                    PlaceLayer::Country => {
                        fields.country = name;
                        fields.subject = Some(Slot::Country);
                    }
                    // A region that is a city as well (Berlin) is written as
                    // the locality it is in queries.
                    PlaceLayer::Region if !self.names_a_locality(name.as_ref()) => {
                        fields.region = name;
                        fields.subject = Some(Slot::Region);
                    }
                    // A district that is also the place addresses name (a city
                    // drawn only as a district) is written as a locality, any
                    // other on the region side.
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
        // A block scheme address is its number in its areas.
        let numbered_in_area = fields.house_number.is_some() && fields.suburb.is_some();
        if fields.house.is_none()
            && fields.street.is_none()
            && fields.postcode.is_none()
            && !numbered_in_area
        {
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
            // Street types and directions in another of their forms, from
            // every language's list: "straße" as "str", "hauptstraße" as
            // "hauptstr", "north" as "n".
            let table = abbreviations();
            // A street type glued onto the name written apart from it some
            // of the time ("lechwehrstraße" as "lechwehr straße"), as people
            // who do not know the name write it.
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
                    *word = other[random.below(other.len() as u64) as usize].clone();
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
        // Rows of a script written without spaces write their numbers in
        // numeral ideographs some of the time ("二丁目7番2号").
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
                    // A unit keeps the comma before it more often than other
                    // fields: "lessingstraße 11, 9 og".
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
            // Streets and venues are open classes: a query names many the
            // Pack does not hold. Areas are closed: a locality or region is
            // one the Pack knows, so their hits are never hidden.
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

    /// The district the record lies in, in [`DISTRICT_SHARE`] of the rows of
    /// records in one.
    fn district(&self, context: &Areas, random: &mut Random) -> Option<Tokens> {
        context
            .district
            .as_deref()
            .filter(|_| random.chance(DISTRICT_SHARE))
            .and_then(words)
    }

    /// Whether the dictionaries know `name` as the place an address names:
    /// a locality name addresses state or a city is drawn as.
    fn names_a_locality(&self, name: Option<&Tokens>) -> bool {
        name.and_then(|name| dictionary_key(&name.words.join(" ")))
            .and_then(|key| self.tagger.dictionaries().get(Field::Locality, &key))
            .is_some_and(|entry| entry.levels & level::LOCALITY != 0)
    }

    /// The island the record lies on, in [`ISLAND_SHARE`] of the rows of
    /// records on one.
    fn island(&self, context: &Areas, random: &mut Random) -> Option<Tokens> {
        context
            .island
            .as_deref()
            .filter(|_| random.chance(ISLAND_SHARE))
            .and_then(words)
    }

    /// A near phrase before the areas of a row ("cafe near toronto"), or
    /// after everything when it names none ("cafe near me").
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
        // A unit word, or a PO box or level phrase, before the number or,
        // some of the time, after it ("301号室", "3階").
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
        // A word that is a unit or level of its own ("erdgeschoss") takes no
        // number.
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

    /// A unit field after the street and house number, or before the
    /// address some of the time.
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

    /// The names of the areas a record lies in, and its country's code.
    fn context(&mut self, id: RecordId, random: &mut Random) -> Result<Areas> {
        let Some(context) = self.pack.boundary_context(id)? else {
            return Ok(Areas::default());
        };
        let tuple = context.admin_context;
        // An area with no locality below it is the locality an address
        // names (a city drawn only as a district, a kreisfreie Stadt); a
        // district between the record's locality and its region is written
        // on the region side.
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
        // A district goes on the region side only when its name names no
        // locality: "Gütersloh" is a city as well as its Kreis, and a query
        // naming it means the city, as libpostal's data labels such names.
        let district = district.filter(|district| !self.names_a_locality(words(district).as_ref()));
        let mut region = self.admin_name(tuple.region_record_id(), Field::Region, random)?;
        // A city that is a region of its own (Berlin, Hamburg) is the
        // locality of the records in it that lie in no other.
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
        let country_code = countries
            .first()
            .and_then(|name| normalize_index_text(name))
            .and_then(|name| self.tagger.dictionaries().get(Field::Country, &name))
            .and_then(|entry| entry.country);
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

    /// In [`REGION_GROUP_SHARE`] of draws, a name of a group of regions the
    /// region `id` lies in.
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

    /// One name of an area, as [`pick_name`] picks it.
    fn name(&mut self, id: Option<RecordId>, random: &mut Random) -> Result<Option<String>> {
        let names = self.names(id)?;
        Ok(pick_name(&names, random).map(str::to_string))
    }

    /// One name of an admin area, as [`Self::name`] picks it, or in
    /// [`ADMIN_ALIAS_SHARE`] of draws that name without its type word when
    /// `field` holds it so.
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

    /// Every name of an area, its `name` first.
    fn names(&mut self, id: Option<RecordId>) -> Result<Vec<String>> {
        let Some(id) = id else {
            return Ok(Vec::new());
        };
        if let Some(names) = self.names.get(&id) {
            return Ok(names.clone());
        }
        let names = self.pack.records().names(id)?;
        self.names.insert(id, names.clone());
        Ok(names)
    }
}

/// The standalone unit and level words, as [`words`] writes them.
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

/// A record's `name`, or one of its alternate names in [`ALT_NAME_SHARE`]
/// of draws. `names` holds the `name` first.
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

/// The areas a record lies in.
#[derive(Debug, Default)]
struct Areas {
    locality: Option<String>,
    district: Option<String>,
    /// The place area the record lies in (see [`Fields::island`]).
    island: Option<String>,
    suburb: Option<String>,
    region: Option<String>,
    country: Option<String>,
    country_code: Option<CountryCode>,
}

/// What a drawn record is counted against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Draw {
    Layer(Layer),
    BlockAddress,
}

/// The share of rows of block scheme addresses besides the addresses'.
const BLOCK_ADDRESS_SHARE: f64 = 0.25;

/// Whether a record is an address of the block scheme: numbered in the
/// areas it states, with no street.
fn is_block_address(record: &Record) -> bool {
    matches!(record, Record::Address(record)
        if record.address.street.is_none() && !record.address.areas.is_empty())
}

fn address_fields(address: &AddressComponents, fields: &mut Fields) {
    fields.house_number = words(&address.number);
    fields.street = address.street.as_deref().and_then(words);
    // The areas an address states, largest first as Japanese addresses
    // write them: the ward, quarter and neighbourhood a block number is in.
    fields.suburb = words(&address.areas.join(" "));
    fields.unit = address.unit.as_deref().and_then(words);
    fields.postcode = address.postcode.as_deref().and_then(words);
}

/// `postcode` after its country's code or the code's first letter, glued
/// ("de09496"), joined by a hyphen ("d-80331") or apart ("d 18211").
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

/// The numbered areas of an address ("2丁目", "3 chome") written as their
/// number alone in [`NUMBERED_AREA_SHARE`] of rows, so "林2" is read as one
/// area with its number.
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

/// The areas an address states, largest first, glued into one field the
/// way an `addr:city` tag or a written address often holds them:
/// `神奈川県横浜市西区みなとみらい3丁目` is a region, a city, a ward and a
/// neighbourhood with no break between them. Names of a script written
/// without spaces join directly, others with a space. The region leads half
/// the time. `None` when the address states fewer than two areas.
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

/// `word` with its ASCII digits written as the numeral ideographs of the
/// same value (see [`super::numeral_value`]), digit by digit.
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
            address: AddressComponents {
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
            source: crate::record::SourceProvenance::osm(crate::record::OsmObjectType::Node, 1),
        })
    }

    #[test]
    fn the_sample_holds_a_fixed_share_of_each_country() {
        let street = address(Some("King Street"), &[]);
        let block = address(None, &["5丁目"]);
        let mut sampler = Sampler::default();
        let addresses = quota(Draw::Layer(Layer::Address)).expect("quota");
        let blocks = quota(Draw::BlockAddress).expect("quota");
        // One large country, one small, and records in no country.
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
        // The large country's draws spread over all its records.
        let large = &candidates[&Some(1)];
        assert!(large.windows(2).all(|pair| pair[0].id < pair[1].id));
        assert!(large.last().expect("drawn").id > 2 * addresses as u64);
        // A block address drawn for both of its kinds is drawn once.
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
        // Two words no dictionary knows: one span each, so every labelling
        // has one path, and the decoder can list them all.
        let tagger = LatticeTagger::new(Dictionaries::empty(), Weights::built_in());
        let lattice = tagger.lattice(&Tokens::new("kingsway 12"), &Suppression::default());
        assert_eq!(lattice.spans.len(), 2);
        let weights = Weights::built_in();
        let paths = decode::decode(&lattice, &weights, None, LABELS * LABELS, None);
        assert_eq!(paths.len(), LABELS * LABELS);
        let brute = log_sum(paths.iter().map(|path| f64::from(path.score)));
        let reading = Reading::new(&lattice, &weights, None, None);
        assert!(
            (reading.log_z - brute).abs() < 1e-3,
            "{} {brute}",
            reading.log_z
        );
        // Every unit has one label in each labelling, so the expected count
        // of each span's bias over its labels is one.
        let mut gradient = Gradient::default();
        reading.add_expectations(&lattice, None, 1.0, &mut gradient);
        let bias = super::super::features::by_name("bias:bias") as usize;
        let total = (0..LABELS)
            .filter_map(|label| gradient.edge.get(&(bias * LABELS + label)))
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
    fn rows_are_read_in_the_order_the_shuffle_window_draws() {
        // The window as it shuffled the rows themselves, before the lattices
        // were cached: each epoch must read the rows in this same order.
        fn through_window(len: usize, random: &mut Random) -> Vec<u32> {
            let mut window = Vec::new();
            let mut order = Vec::new();
            for row in 0..len as u32 {
                if window.len() < SHUFFLE_WINDOW {
                    window.push(row);
                    continue;
                }
                let at = random.below(window.len() as u64) as usize;
                order.push(std::mem::replace(&mut window[at], row));
            }
            random.shuffle(&mut window);
            order.extend(window);
            order
        }
        for len in [0, 1, 7, SHUFFLE_WINDOW, SHUFFLE_WINDOW + 4_321] {
            let order = shuffled_order(len, &mut Random::new(len as u64));
            assert_eq!(order, through_window(len, &mut Random::new(len as u64)));
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert!(sorted.into_iter().eq(0..len as u32), "every row once");
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
        let scratch = Scratch::create(
            std::env::temp_dir().join(format!("open-geocode-lattices-{}", uuid::Uuid::new_v4())),
        )
        .expect("scratch");
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
