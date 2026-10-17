//! Semi-Markov Viterbi decoding of a lattice, n-best.
//!
//! A path covers the query with segments, each a lattice span with one
//! label. Its score is the sum of its edges' scores and of the transitions
//! between consecutive labels, from a start state before the first segment
//! to an end state after the last. Two segmentations that give every token
//! the same label are one labelling, so each decoding cell keeps the best
//! path per label sequence and the n best are n different labellings.

use anyhow::{Context, Result};

use super::{CountryCode, Tokens, Units, weights::Weights};
use crate::{
    parse::types::Label,
    util::codec::{get_string, get_u8, get_u32, get_u64, put_str, put_u64},
};

/// Labels, then the start and end states of the transition table.
pub(super) const LABELS: usize = Label::ALL.len();
pub(super) const START: usize = LABELS;
pub(super) const END: usize = LABELS + 1;
pub(super) const STATES: usize = LABELS + 2;

/// A run of tokens with the features of every edge over it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Span {
    pub start: usize,
    pub end: usize,
    pub features: Vec<u32>,
    /// Features that hold only when the query is decoded as that country:
    /// its postcode pattern matched, or a dictionary hit is in it.
    pub country_features: Vec<(CountryCode, u32)>,
}

impl Span {
    /// The feature ids of the span decoded as `country`.
    pub(super) fn features_in(
        &self,
        country: Option<CountryCode>,
    ) -> impl Iterator<Item = u32> + '_ {
        self.features.iter().copied().chain(
            self.country_features
                .iter()
                .filter(move |(code, _)| Some(*code) == country)
                .map(|(_, feature)| *feature),
        )
    }
}

/// The spans over the units of one query, and the countries worth decoding
/// it for.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Lattice {
    /// How many units the query has.
    pub len: usize,
    pub units: Units,
    pub spans: Vec<Span>,
    pub candidates: Vec<CountryCode>,
    /// The spans that are names of areas the Pack knows, as `(start, end,
    /// the record ids of the areas)`.
    pub places: Vec<(usize, usize, Vec<u32>)>,
    /// Span indices by the token they end before.
    by_end: Vec<Vec<usize>>,
}

impl Lattice {
    pub(super) fn new(units: Units, spans: Vec<Span>, candidates: Vec<CountryCode>) -> Self {
        let len = units.len();
        let mut by_end = vec![Vec::new(); len + 1];
        for (index, span) in spans.iter().enumerate() {
            by_end[span.end].push(index);
        }
        Self {
            len,
            units,
            spans,
            candidates,
            places: Vec::new(),
            by_end,
        }
    }

    /// The areas the span `start..end` names, when the Pack knows them.
    pub(super) fn place(&self, start: usize, end: usize) -> Option<&[u32]> {
        self.places
            .iter()
            .find(|place| place.0 == start && place.1 == end)
            .map(|place| place.2.as_slice())
    }

    /// Bytes the lattice holds, its heap data included.
    pub(super) fn resident_bytes(&self) -> usize {
        use std::mem::size_of;
        let tokens = &self.units.tokens;
        size_of::<Self>()
            + tokens
                .words
                .iter()
                .map(|word| size_of::<String>() + word.capacity())
                .sum::<usize>()
            + tokens.breaks.capacity()
            + tokens.joiners.capacity() * size_of::<Option<char>>()
            + self.units.token_of.capacity() * size_of::<usize>()
            + self.units.glued.capacity()
            + self.spans.capacity() * size_of::<Span>()
            + self
                .spans
                .iter()
                .map(|span| {
                    span.features.capacity() * size_of::<u32>()
                        + span.country_features.capacity() * size_of::<(CountryCode, u32)>()
                })
                .sum::<usize>()
            + self.candidates.capacity() * size_of::<CountryCode>()
            + self.places.capacity() * size_of::<(usize, usize, Vec<u32>)>()
            + self
                .places
                .iter()
                .map(|place| place.2.capacity() * size_of::<u32>())
                .sum::<usize>()
            + self.by_end.capacity() * size_of::<Vec<usize>>()
            + self
                .by_end
                .iter()
                .map(|spans| spans.capacity() * size_of::<usize>())
                .sum::<usize>()
    }

    /// Write the lattice for [`Self::decode`], which reads back an equal one.
    pub(super) fn encode(&self, out: &mut Vec<u8>) {
        let units = &self.units;
        put_u64(out, units.len() as u64);
        for index in 0..units.len() {
            put_str(out, &units.tokens.words[index]);
            out.push(u8::from(units.tokens.breaks[index]) | u8::from(units.glued[index]) << 1);
            put_u64(out, units.tokens.joiners[index].map_or(0, u64::from));
            put_u64(out, units.token_of[index] as u64);
        }
        put_u64(out, self.spans.len() as u64);
        for span in &self.spans {
            put_u64(out, span.start as u64);
            put_u64(out, span.end as u64);
            put_u64(out, span.features.len() as u64);
            for feature in &span.features {
                put_u64(out, u64::from(*feature));
            }
            put_u64(out, span.country_features.len() as u64);
            for (country, feature) in &span.country_features {
                put_u64(out, u64::from(country.to_u16()));
                put_u64(out, u64::from(*feature));
            }
        }
        put_u64(out, self.candidates.len() as u64);
        for country in &self.candidates {
            put_u64(out, u64::from(country.to_u16()));
        }
        put_u64(out, self.places.len() as u64);
        for (start, end, ids) in &self.places {
            put_u64(out, *start as u64);
            put_u64(out, *end as u64);
            put_u64(out, ids.len() as u64);
            for id in ids {
                put_u64(out, u64::from(*id));
            }
        }
    }

    pub(super) fn decode(input: &mut &[u8]) -> Result<Self> {
        let country = |input: &mut &[u8]| -> Result<CountryCode> {
            u16::try_from(get_u64(input)?)
                .ok()
                .and_then(CountryCode::from_u16)
                .context("invalid country in a lattice")
        };
        let count = |input: &mut &[u8]| -> Result<usize> {
            usize::try_from(get_u64(input)?).context("lattice length out of range")
        };
        let mut units = Units {
            tokens: Tokens::default(),
            token_of: Vec::new(),
            glued: Vec::new(),
        };
        for _ in 0..count(input)? {
            units.tokens.words.push(get_string(input)?);
            let flags = get_u8(input)?;
            units.tokens.breaks.push(flags & 1 != 0);
            units.glued.push(flags & 2 != 0);
            let joiner = match get_u64(input)? {
                0 => None,
                code => Some(
                    u32::try_from(code)
                        .ok()
                        .and_then(char::from_u32)
                        .context("invalid joiner in a lattice")?,
                ),
            };
            units.tokens.joiners.push(joiner);
            units.token_of.push(count(input)?);
        }
        let span_count = count(input)?;
        // A corrupt count must not reserve more than the input could hold.
        let mut spans = Vec::with_capacity(span_count.min(input.len()));
        for _ in 0..span_count {
            let start = count(input)?;
            let end = count(input)?;
            let features = (0..count(input)?)
                .map(|_| get_u32(input))
                .collect::<Result<Vec<_>>>()?;
            let country_features = (0..count(input)?)
                .map(|_| Ok((country(input)?, get_u32(input)?)))
                .collect::<Result<Vec<_>>>()?;
            if start >= end || end > units.len() {
                anyhow::bail!("invalid span in a lattice");
            }
            spans.push(Span {
                start,
                end,
                features,
                country_features,
            });
        }
        let candidates = (0..count(input)?)
            .map(|_| country(input))
            .collect::<Result<Vec<_>>>()?;
        let mut places = Vec::new();
        for _ in 0..count(input)? {
            let start = count(input)?;
            let end = count(input)?;
            let ids = (0..count(input)?)
                .map(|_| get_u32(input))
                .collect::<Result<Vec<_>>>()?;
            places.push((start, end, ids));
        }
        let mut lattice = Self::new(units, spans, candidates);
        lattice.places = places;
        Ok(lattice)
    }
}

/// One labelled span of a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Segment {
    pub span: usize,
    pub label: Label,
}

/// A labelling of the whole query.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Path {
    pub segments: Vec<Segment>,
    pub country: Option<CountryCode>,
    pub score: f32,
}

impl Path {
    /// The units and label of every segment, in order.
    pub(super) fn segments(&self, lattice: &Lattice) -> Vec<(usize, usize, Label)> {
        self.segments
            .iter()
            .map(|segment| {
                let span = &lattice.spans[segment.span];
                (span.start, span.end, segment.label)
            })
            .collect()
    }

    /// The label of every unit.
    pub(super) fn labels(&self, lattice: &Lattice) -> Vec<Label> {
        let mut labels = Vec::with_capacity(lattice.len);
        for segment in &self.segments {
            let span = &lattice.spans[segment.span];
            labels.extend(std::iter::repeat_n(segment.label, span.end - span.start));
        }
        labels
    }
}

/// The countries a lattice is decoded as: every candidate country, or the
/// global fallback alone when the query points to none. A query whose
/// dictionary hits and postcode matches point to a country is read with
/// that country's evidence and transitions, never against a global reading
/// that leaves its evidence out.
pub(super) fn countries(lattice: &Lattice) -> Vec<Option<CountryCode>> {
    if lattice.candidates.is_empty() {
        vec![None]
    } else {
        lattice.candidates.iter().copied().map(Some).collect()
    }
}

/// The best `n` labellings over the countries of [`countries`], each
/// labelling once, under the country that scores it best.
pub(super) fn best(lattice: &Lattice, weights: &Weights, n: usize) -> Vec<Path> {
    let mut paths = countries(lattice)
        .into_iter()
        .flat_map(|country| decode(lattice, weights, country, n, None))
        .collect::<Vec<_>>();
    sort_paths(&mut paths);
    let mut seen = Vec::new();
    paths.retain(|path| {
        let labels = path.labels(lattice);
        let new = !seen.contains(&labels);
        if new {
            seen.push(labels);
        }
        new
    });
    paths.truncate(n);
    paths
}

/// The best `k` labellings decoded as `country`, or, with `gold`, the best
/// that give every token its gold label.
pub(super) fn decode(
    lattice: &Lattice,
    weights: &Weights,
    country: Option<CountryCode>,
    k: usize,
    gold: Option<&[Label]>,
) -> Vec<Path> {
    let k = k.max(1);
    let transitions = weights.transitions(country);
    // cells[position][state]: the best partial paths ending there.
    let mut cells = vec![vec![Vec::<Entry>::new(); STATES]; lattice.len + 1];
    cells[0][START].push(Entry {
        score: 0.0,
        hash: HASH_SEED,
        back: None,
    });
    for end in 1..=lattice.len {
        for &span_index in &lattice.by_end[end] {
            let span = &lattice.spans[span_index];
            let starts = if span.start == 0 {
                START..START + 1
            } else {
                0..LABELS
            };
            for label in Label::ALL {
                if let Some(gold) = gold
                    && gold[span.start..span.end].iter().any(|gold| *gold != label)
                {
                    continue;
                }
                let next = label as usize;
                let edge = weights.edge(span, label, country);
                let mut hash_extension = HashExtension::new(next, span.end - span.start);
                for previous in starts.clone() {
                    let transition = transitions.get(previous, next);
                    for rank in 0..cells[span.start][previous].len() {
                        let entry = &cells[span.start][previous][rank];
                        let candidate = Entry {
                            score: entry.score + transition + edge,
                            hash: hash_extension.apply(entry.hash),
                            back: Some(Back {
                                span: span_index,
                                previous,
                                rank,
                            }),
                        };
                        insert(&mut cells[end][next], candidate, k);
                    }
                }
            }
        }
    }
    let mut finals = Vec::new();
    for (label, cell) in cells[lattice.len].iter().enumerate().take(LABELS) {
        for (rank, entry) in cell.iter().enumerate() {
            finals.push((entry.score + transitions.get(label, END), label, rank));
        }
    }
    finals.sort_by(|left, right| right.0.total_cmp(&left.0));
    finals
        .into_iter()
        .take(k)
        .map(|(score, label, rank)| {
            let mut segments = Vec::new();
            let (mut position, mut state, mut rank) = (lattice.len, label, rank);
            while let Some(back) = cells[position][state][rank].back {
                segments.push(Segment {
                    span: back.span,
                    label: Label::ALL[state],
                });
                position = lattice.spans[back.span].start;
                state = back.previous;
                rank = back.rank;
            }
            segments.reverse();
            Path {
                segments,
                country,
                score,
            }
        })
        .collect()
}

/// Best first; equal scores keep their order.
pub(super) fn sort_paths(paths: &mut [Path]) {
    paths.sort_by(|left, right| right.score.total_cmp(&left.score));
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    score: f32,
    /// Of the label of every token so far, to keep one path per labelling.
    hash: u64,
    back: Option<Back>,
}

#[derive(Debug, Clone, Copy)]
struct Back {
    span: usize,
    previous: usize,
    rank: usize,
}

const HASH_SEED: u64 = 0xcbf2_9ce4_8422_2325;
const HASH_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Extends a labelling hash by one label for each token of a segment.
struct HashExtension {
    label: u64,
    tokens: usize,
}

impl HashExtension {
    fn new(label: usize, tokens: usize) -> Self {
        Self {
            label: label as u64 + 1,
            tokens,
        }
    }

    fn apply(&mut self, mut hash: u64) -> u64 {
        for _ in 0..self.tokens {
            hash = (hash ^ self.label).wrapping_mul(HASH_PRIME);
        }
        hash
    }
}

/// Keep the `k` best entries of a cell, one per labelling.
fn insert(cell: &mut Vec<Entry>, candidate: Entry, k: usize) {
    if let Some(same) = cell.iter_mut().find(|entry| entry.hash == candidate.hash) {
        if candidate.score > same.score {
            *same = candidate;
            cell.sort_by(|left, right| right.score.total_cmp(&left.score));
        }
        return;
    }
    if cell.len() == k
        && cell
            .last()
            .is_some_and(|last| last.score >= candidate.score)
    {
        return;
    }
    cell.push(candidate);
    cell.sort_by(|left, right| right.score.total_cmp(&left.score));
    cell.truncate(k);
}
