mod abbreviations;
mod decode;
pub mod dictionary;
mod features;
pub mod postcode;
pub mod train;
pub mod weights;

use std::{collections::HashMap, fmt, path::Path, rc::Rc, sync::OnceLock};

use crate::error::Result;
use levenshtein_automata::{DFA, LevenshteinAutomatonBuilder};
use unicode_script::{Script, UnicodeScript};

use self::{
    decode::Lattice,
    dictionary::{Dictionaries, Field, Match},
    features::{FeatureSet, TokenInfo},
    postcode::Table,
    weights::Weights,
};
use super::types::{Label, Tagged, Tagger};
use crate::{
    container::Container,
    pack::resolve_pack_path,
    text_index::{expand_address_abbreviations, expand_final_direction},
    util::fold::{Piece, search_key, search_pieces},
};

pub const MAX_SPAN_TOKENS: usize = 5;
pub const MAX_UNSPACED_SPAN: usize = 10;
pub const N_BEST: usize = 3;
const MAX_COUNTRY_TOKENS: usize = 4;
const CONSTRAINED_PATHS: usize = 8;
const MAX_CANDIDATE_COUNTRIES: usize = 3;
const MIN_FUZZY_CHARS: usize = 4;
const TWO_EDIT_CHARS: usize = 8;
const MAX_TWO_EDIT_CHARS: usize = 16;
const MAX_FUZZY_CHARS: usize = 32;
const MIN_FUZZY_POI_CHARS: usize = 6;
const FIELD_BREAKS: [char; 4] = [',', ';', '\n', '|'];
pub const JOINERS: [char; 3] = ['.', '/', '-'];

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CountryCode([u8; 2]);

impl CountryCode {
    /// Two ASCII letters in either case; `None` for anything else.
    pub fn parse(code: &str) -> Option<Self> {
        let bytes = code.trim().as_bytes();
        let [first, second] = bytes else {
            return None;
        };
        (first.is_ascii_alphabetic() && second.is_ascii_alphabetic())
            .then(|| Self([first.to_ascii_uppercase(), second.to_ascii_uppercase()]))
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("country codes are ASCII")
    }

    pub(crate) fn to_u16(self) -> u16 {
        u16::from_be_bytes(self.0)
    }

    pub(crate) fn from_u16(value: u16) -> Option<Self> {
        Self::parse(std::str::from_utf8(&value.to_be_bytes()).ok()?)
    }
}

impl fmt::Debug for CountryCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Display for CountryCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tokens {
    pub words: Vec<String>,
    pub breaks: Vec<bool>,
    pub joiners: Vec<Option<char>>,
}

impl Tokens {
    pub fn new(text: &str) -> Self {
        let mut tokens = Self::default();
        let mut pending_break = false;
        let mut gap = Gap::None;
        search_pieces(text, |piece| match piece {
            Piece::Word(word) => {
                tokens.push(word.to_string(), pending_break, gap.joiner());
                pending_break = false;
                gap = Gap::None;
            }
            Piece::Gap(character) => {
                gap = match gap {
                    Gap::None if JOINERS.contains(&character) => Gap::Joiner(character),
                    _ => Gap::Other,
                };
                if FIELD_BREAKS.contains(&character) && !tokens.words.is_empty() {
                    pending_break = true;
                }
            }
        });
        tokens
    }

    pub fn from_words<S: AsRef<str>>(words: &[S]) -> Self {
        Self {
            words: words.iter().map(|word| word.as_ref().to_string()).collect(),
            breaks: vec![false; words.len()],
            joiners: vec![None; words.len()],
        }
    }

    pub(crate) fn prefix(&self, end: usize) -> Self {
        Self {
            words: self.words[..end].to_vec(),
            breaks: self.breaks[..end].to_vec(),
            joiners: self.joiners[..end].to_vec(),
        }
    }

    pub(crate) fn push(&mut self, word: String, break_before: bool, joiner: Option<char>) {
        let first = self.words.is_empty();
        self.breaks.push(break_before && !first);
        self.joiners.push(joiner.filter(|_| !first));
        self.words.push(word);
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

const UNSPACED_SCRIPTS: [Script; 7] = [
    Script::Han,
    Script::Hiragana,
    Script::Katakana,
    Script::Thai,
    Script::Lao,
    Script::Khmer,
    Script::Myanmar,
];

pub fn is_unspaced(character: char) -> bool {
    if character.is_ascii() {
        return false;
    }
    let scripts = character.script_extension();
    !scripts.is_common()
        && !scripts.is_inherited()
        && UNSPACED_SCRIPTS
            .iter()
            .any(|script| scripts.contains_script(*script))
}

/// Whether every character of `word` is of no one script (Unicode Common or
/// Inherited): digits, fullwidth or not. Such a word takes the script of the
/// characters beside it, as UAX #24 resolves Common characters, so it joins
/// a run of a script without spaces on either side ("４ 丁 目" is "４丁目").
fn is_neutral(word: &str) -> bool {
    !word.is_empty()
        && word.chars().all(|character| {
            let scripts = character.script_extension();
            scripts.is_common() || scripts.is_inherited()
        })
}

/// `character<TAB>value` rows of the ideographs that are numerals, from the
/// Unihan kPrimaryNumeric property (Unicode License V3, see
/// `NOTICE`), vendored.
pub(super) const NUMERALS: &str = include_str!("lattice/data/numerals.tsv");

pub fn numeral_value(character: char) -> Option<u32> {
    static TABLE: OnceLock<HashMap<char, u32>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            NUMERALS
                .lines()
                .filter_map(|line| {
                    let (character, value) = line.split_once('\t')?;
                    let mut chars = character.chars();
                    let character = chars.next().filter(|_| chars.next().is_none())?;
                    Some((character, value.trim().parse().ok()?))
                })
                .collect()
        })
        .get(&character)
        .copied()
}

pub fn is_numeral(character: char) -> bool {
    character.is_numeric() || numeral_value(character).is_some()
}

fn after_number_sign(text: &str) -> Vec<bool> {
    let mut signed = Vec::new();
    let mut sign = false;
    search_pieces(text, |piece| match piece {
        Piece::Word(_) => {
            signed.push(sign && !signed.is_empty());
            sign = false;
        }
        Piece::Gap('#') => sign = true,
        Piece::Gap(character) => sign &= character.is_whitespace(),
    });
    signed
}

pub fn mask_numerals(key: &str) -> Option<String> {
    let mut masked = String::with_capacity(key.len());
    let mut numbered = false;
    for character in key.chars() {
        if is_numeral(character) {
            if !masked.ends_with('#') {
                masked.push('#');
            }
            numbered = true;
        } else {
            masked.push(character);
        }
    }
    (numbered && masked.chars().any(char::is_alphabetic)).then_some(masked)
}

pub fn dictionary_key(name: &str) -> Option<String> {
    let normalized = search_key(name)?;
    if normalized.is_ascii() {
        return Some(normalized);
    }
    let words = normalized.split(' ').collect::<Vec<_>>();
    let units = Units::of(&Tokens::from_words(&words));
    Some(units.key(0, units.len()))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Units {
    pub tokens: Tokens,
    pub token_of: Vec<usize>,
    pub glued: Vec<bool>,
}

impl Units {
    pub fn of(tokens: &Tokens) -> Self {
        let mut units = Self {
            tokens: Tokens::default(),
            token_of: Vec::with_capacity(tokens.len()),
            glued: Vec::with_capacity(tokens.len()),
        };
        for (index, word) in tokens.words.iter().enumerate() {
            if !word.chars().any(is_unspaced) {
                units.push(tokens, index, word.clone(), true);
                continue;
            }
            let mut first = true;
            let mut run = String::new();
            for character in word.chars() {
                if is_unspaced(character) {
                    if !run.is_empty() {
                        units.push(tokens, index, std::mem::take(&mut run), first);
                        first = false;
                    }
                    units.push(tokens, index, character.to_string(), first);
                    first = false;
                } else {
                    run.push(character);
                }
            }
            if !run.is_empty() {
                units.push(tokens, index, run, first);
            }
        }
        units
    }

    fn push(&mut self, tokens: &Tokens, index: usize, text: String, first: bool) {
        let glued = if first {
            let spaced = !tokens.breaks[index] && tokens.joiners[index].is_none();
            spaced
                && self.tokens.words.last().is_some_and(|before| {
                    let unspaced_before = before.chars().last().is_some_and(is_unspaced);
                    let unspaced_after = text.chars().next().is_some_and(is_unspaced);
                    (unspaced_before && (unspaced_after || is_neutral(&text)))
                        || (unspaced_after && is_neutral(before))
                })
        } else {
            true
        };
        if first {
            self.tokens
                .push(text, tokens.breaks[index], tokens.joiners[index]);
        } else {
            self.tokens.push(text, false, None);
        }
        self.token_of.push(index);
        self.glued.push(glued);
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    fn key(&self, start: usize, end: usize) -> String {
        let mut key = String::new();
        for index in start..end {
            if index > start && !self.glued[index] {
                key.push(' ');
            }
            key.push_str(&self.tokens.words[index]);
        }
        key
    }

    fn runs(&self) -> Vec<(usize, usize)> {
        let mut runs = Vec::new();
        let mut start = 0;
        for end in 1..=self.len() {
            if end == self.len() || !self.glued[end] {
                if end - start > 1 {
                    runs.push((start, end));
                }
                start = end;
            }
        }
        runs
    }

    fn unspaced(&self, start: usize, end: usize) -> bool {
        self.glued[start + 1..end].iter().all(|glued| *glued)
    }

    fn of_tokens(&self, start: usize, end: usize) -> (usize, usize) {
        let first = |token: usize| self.token_of.partition_point(|of| *of < token);
        (first(start), first(end))
    }

    pub fn expand(&self, labels: &[Label]) -> Vec<Label> {
        self.token_of.iter().map(|token| labels[*token]).collect()
    }

    pub fn collapse(&self, labels: &[Label]) -> Vec<Label> {
        let mut out: Vec<Label> = Vec::new();
        let mut counts = [0usize; Label::ALL.len()];
        let mut first_seen = [usize::MAX; Label::ALL.len()];
        for (index, label) in labels.iter().enumerate() {
            let label = *label as usize;
            counts[label] += self.tokens.words[index].chars().count();
            first_seen[label] = first_seen[label].min(index);
            let last_of_token = self.token_of.get(index + 1) != Some(&self.token_of[index]);
            if last_of_token {
                let best = (0..Label::ALL.len())
                    .filter(|label| counts[*label] > 0)
                    .max_by(|left, right| {
                        counts[*left]
                            .cmp(&counts[*right])
                            .then(first_seen[*right].cmp(&first_seen[*left]))
                    })
                    .map_or(Label::Other, |label| Label::ALL[label]);
                out.push(best);
                counts = [0; Label::ALL.len()];
                first_seen = [usize::MAX; Label::ALL.len()];
            }
        }
        out
    }

    pub fn pieces(&self, segments: &[(usize, usize, Label)]) -> Vec<Tagged> {
        self.corrected_pieces(segments, &[]).0
    }

    fn corrected_pieces(
        &self,
        segments: &[(usize, usize, Label)],
        corrections: &[(usize, usize, Label, String)],
    ) -> (Vec<Tagged>, Vec<Option<String>>) {
        let by_unit = self.unit_corrections(segments, corrections);
        let mut pieces: Vec<Tagged> = Vec::with_capacity(segments.len());
        let mut corrected: Vec<Option<String>> = Vec::with_capacity(segments.len());
        let mut open_run = false;
        for &(start, end, label) in segments {
            let single = end - start == 1;
            for (unit, correction) in (start..end).zip(&by_unit[start..end]) {
                let word = &self.tokens.words[unit];
                let joins = self.glued[unit] && (unit > start || (single && open_run));
                match pieces.last_mut() {
                    Some(last) if joins && last.label == label => {
                        last.token.push_str(word);
                        *corrected.last_mut().expect("one per piece") = None;
                    }
                    _ => {
                        pieces.push(Tagged {
                            token: word.clone(),
                            label,
                        });
                        corrected.push(correction.clone());
                    }
                }
            }
            open_run = single;
        }
        (pieces, corrected)
    }

    fn unit_corrections(
        &self,
        segments: &[(usize, usize, Label)],
        corrections: &[(usize, usize, Label, String)],
    ) -> Vec<Option<String>> {
        let mut labels = vec![Label::Other; self.len()];
        for &(start, end, label) in segments {
            labels[start..end].fill(label);
        }
        let mut candidates = corrections
            .iter()
            .filter(|(start, end, label, _)| labels[*start..*end].iter().all(|at| at == label))
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(start, end, label, _)| {
            (
                !segments.contains(&(*start, *end, *label)),
                std::cmp::Reverse(end - start),
            )
        });
        let mut by_unit = vec![None; self.len()];
        let mut decided = vec![false; self.len()];
        for (start, end, _, name) in candidates {
            if decided[*start..*end].iter().any(|decided| *decided) {
                continue;
            }
            if let Some(words) = self.correction(*start, *end, name) {
                for (unit, word) in (*start..*end).zip(words) {
                    by_unit[unit] = word;
                    decided[unit] = true;
                }
            }
        }
        by_unit
    }

    fn correction(&self, start: usize, end: usize, name: &str) -> Option<Vec<Option<String>>> {
        if self.glued[start..end].iter().any(|glued| *glued) {
            return None;
        }
        let expanded = expand_key(&self.key(start, end));
        let written = expanded.split(' ').collect::<Vec<_>>();
        let named = name.split(' ').collect::<Vec<_>>();
        (written.len() == end - start && named.len() == written.len()).then(|| {
            written
                .iter()
                .zip(named)
                .map(|(written, named)| (*written != named).then(|| named.to_string()))
                .collect()
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum Gap {
    None,
    Joiner(char),
    Other,
}

impl Gap {
    fn joiner(self) -> Option<char> {
        match self {
            Self::Joiner(character) => Some(character),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedQuery {
    pub tokens: Vec<Tagged>,
    pub pieces: Vec<Tagged>,
    pub corrections: Vec<Option<String>>,
    pub country: Option<CountryCode>,
    pub score: f32,
}

impl ParsedQuery {
    pub fn words(&self, label: Label) -> impl Iterator<Item = &str> {
        self.tokens
            .iter()
            .filter(move |tagged| tagged.label == label)
            .map(|tagged| tagged.token.as_str())
    }
}

#[derive(Debug)]
pub struct Explanation {
    lattice: Lattice,
    signed: Vec<bool>,
    pub best: f32,
}

fn signed_allows(lattice: &Lattice, signed: &[bool], span: &decode::Span, label: Label) -> bool {
    label == Label::Unit
        || !(span.start..span.end)
            .any(|unit| signed.get(lattice.units.token_of[unit]) == Some(&true))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LabelSet(u16);

impl LabelSet {
    pub fn with(self, label: Label) -> Self {
        Self(self.0 | 1 << label as u16)
    }

    pub fn contains(self, label: Label) -> bool {
        self.0 & 1 << label as u16 != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

pub struct LatticeTagger {
    dictionaries: Dictionaries,
    postcodes: &'static Table,
    weights: Weights,
}

pub(crate) fn automaton_builder(edits: u8) -> &'static LevenshteinAutomatonBuilder {
    static BUILDERS: [OnceLock<LevenshteinAutomatonBuilder>; 2] =
        [OnceLock::new(), OnceLock::new()];
    BUILDERS[usize::from(edits - 1)].get_or_init(|| LevenshteinAutomatonBuilder::new(edits, true))
}

pub(crate) fn other_forms(word: &str) -> Vec<String> {
    let table = abbreviations::abbreviations();
    let mut forms = table.full_forms(word).to_vec();
    for form in table.unglued(word) {
        if !forms.contains(&form) {
            forms.push(form);
        }
    }
    forms
}

pub(crate) fn forms_both_ways(word: &str) -> Vec<String> {
    let mut forms = other_forms(word);
    for form in abbreviations::abbreviations().short_forms(word) {
        if !form.contains(' ') && !forms.contains(form) {
            forms.push(form.clone());
        }
    }
    forms
}

pub(crate) fn full_form(word: &str) -> Option<&'static str> {
    abbreviations::abbreviations()
        .full_forms(word)
        .iter()
        .map(String::as_str)
        .min()
}

pub(crate) fn glued_stem(word: &str) -> Option<&str> {
    glued_parts(word).map(|(stem, _)| stem)
}

pub(crate) fn glued_parts(word: &str) -> Option<(&str, &str)> {
    abbreviations::abbreviations().split_glued(word)
}

impl fmt::Debug for LatticeTagger {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LatticeTagger")
            .field("dictionaries", &self.dictionaries)
            .finish_non_exhaustive()
    }
}

impl LatticeTagger {
    pub fn open(pack: &Path) -> Result<Self> {
        let path = resolve_pack_path(pack)?;
        Self::from_container(&Container::open(&path)?)
    }

    pub fn from_container(container: &Container) -> Result<Self> {
        let dictionaries = Dictionaries::open(container)?;
        let weights = Weights::from_container(container)?.unwrap_or_else(Weights::built_in);
        Ok(Self::new(dictionaries, weights))
    }

    pub fn new(dictionaries: Dictionaries, weights: Weights) -> Self {
        Self {
            dictionaries,
            postcodes: Table::global(),
            weights,
        }
    }

    pub fn dictionaries(&self) -> &Dictionaries {
        &self.dictionaries
    }

    pub fn weights(&self) -> &Weights {
        &self.weights
    }

    pub fn set_weights(&mut self, weights: Weights) {
        self.weights = weights;
    }

    pub fn names_street(&self, words: &[String]) -> bool {
        let Some(key) = dictionary_key(&words.join(" ")) else {
            return false;
        };
        let names = |key: &str| self.dictionaries.contains(Field::Street, key);
        names(&key)
            || names(&expand_key(&key))
            || spellings(&key).iter().any(|spelling| names(spelling))
    }

    /// The best labellings of raw query text, best first, at most
    /// [`N_BEST`], each with a different label sequence. Empty when the
    /// query has no words.
    ///
    /// A query that ends in a country the Pack knows by that name or code
    /// ("… K9H 2H7 CA", "… United States") is also read as the words before
    /// it with the country after them, and the best readings of both are
    /// kept. The model scores a country's code where it was trained to see
    /// it, so a code it rarely saw last can wreck the reading of every word
    /// before it ("Peterborough ON" read as a street and a locality); read
    /// apart, the words keep the labels they have without it.
    pub fn parse(&self, query: &str) -> Vec<ParsedQuery> {
        self.parse_explained(query).0
    }

    pub fn parse_explained(&self, query: &str) -> (Vec<ParsedQuery>, Option<Explanation>) {
        self.parse_explained_n(query, N_BEST)
    }

    pub fn parse_explained_n(
        &self,
        query: &str,
        n: usize,
    ) -> (Vec<ParsedQuery>, Option<Explanation>) {
        let tokens = Tokens::new(query);
        if tokens.is_empty() {
            return (Vec::new(), None);
        }
        let signed = after_number_sign(query);
        let lattice = self.lattice(&tokens, &Suppression::default());
        let mut parsed = self.parse_lattice(&tokens, &lattice, n, &signed);
        let mut explanation = Explanation {
            best: parsed.first().map_or(0.0, |parse| parse.score),
            lattice,
            signed,
        };
        let Some(at) = self.trailing_country(&tokens).filter(|&at| {
            !explanation
                .signed
                .get(at..)
                .unwrap_or_default()
                .contains(&true)
        }) else {
            return (parsed, Some(explanation));
        };
        let country = &tokens.words[at..];
        let prefix = tokens.prefix(at);
        let prefix_lattice = self.lattice(&prefix, &Suppression::default());
        let signed_prefix = &explanation.signed[..at.min(explanation.signed.len())];
        for mut reading in self.parse_lattice(&prefix, &prefix_lattice, n, signed_prefix) {
            let words = country.iter().map(|word| Tagged {
                token: word.clone(),
                label: Label::Country,
            });
            reading.tokens.extend(words.clone());
            reading.pieces.extend(words);
            reading
                .corrections
                .extend(std::iter::repeat_n(None, country.len()));
            if !parsed.iter().any(|held| held.tokens == reading.tokens) {
                parsed.push(reading);
            }
        }
        parsed.sort_by(|left, right| right.score.total_cmp(&left.score));
        parsed.truncate(n);
        if let Some(best) = parsed.first() {
            explanation.best = best.score;
        }
        (parsed, Some(explanation))
    }

    /// The score of the best labelling of an explained query that gives
    /// each of its words only a label `labels` allows it, `labels` saying of
    /// each word (a unit of the lattice, as a search key) the labels an
    /// answer agrees with; an empty set leaves the word free. Joint
    /// decoding of the parse and the answer: the parser's own score of the
    /// query read as the answer reads it. `None` when no labelling does.
    pub fn explain(
        &self,
        explanation: &Explanation,
        labels: impl Fn(&str) -> LabelSet,
    ) -> Option<f32> {
        let lattice = &explanation.lattice;
        let allowed = lattice
            .units
            .tokens
            .words
            .iter()
            .map(|word| labels(word))
            .collect::<Vec<_>>();
        let allows = |span: &decode::Span, label: Label| {
            signed_allows(lattice, &explanation.signed, span, label)
                && allowed[span.start..span.end]
                    .iter()
                    .all(|set| set.is_empty() || set.contains(label))
        };
        decode::best_allowed(lattice, &self.weights, &allows).map(|path| path.score)
    }

    pub fn place_probabilities(&self, explanation: &Explanation) -> Vec<f32> {
        let lattice = &explanation.lattice;
        let marginals = train::label_marginals(lattice, &self.weights);
        let words = lattice
            .units
            .token_of
            .iter()
            .max()
            .map_or(0, |last| last + 1);
        let mut sums = vec![(0.0f64, 0usize); words];
        for (unit, marginal) in marginals.iter().enumerate() {
            let place = [Label::Locality, Label::Region, Label::Postcode]
                .iter()
                .map(|label| marginal[*label as usize])
                .sum::<f64>();
            let token = lattice.units.token_of[unit];
            sums[token].0 += place;
            sums[token].1 += 1;
        }
        sums.into_iter()
            .map(|(sum, count)| (sum / count.max(1) as f64).clamp(0.0, 1.0) as f32)
            .collect()
    }

    fn trailing_country(&self, tokens: &Tokens) -> Option<usize> {
        (1..=MAX_COUNTRY_TOKENS.min(tokens.len().saturating_sub(1)))
            .rev()
            .map(|count| tokens.len() - count)
            .find(|&at| {
                tokens.joiners[at].is_none()
                    && dictionary_key(&tokens.words[at..].join(" ")).is_some_and(|key| {
                        self.dictionaries
                            .get(Field::Country, &key)
                            .is_some_and(|entry| entry.country.is_some())
                    })
            })
    }

    pub fn parse_tokens(&self, tokens: &Tokens, n: usize) -> Vec<ParsedQuery> {
        if tokens.is_empty() || n == 0 {
            return Vec::new();
        }
        let lattice = self.lattice(tokens, &Suppression::default());
        self.parse_lattice(tokens, &lattice, n, &[])
    }

    fn parse_lattice(
        &self,
        tokens: &Tokens,
        lattice: &Lattice,
        n: usize,
        signed: &[bool],
    ) -> Vec<ParsedQuery> {
        if n == 0 {
            return Vec::new();
        }
        let flat = |span: &decode::Span, label: Label| signed_allows(lattice, signed, span, label);
        let allows: Option<decode::Allows<'_>> = signed.contains(&true).then_some(&flat);
        let mut paths = decode::best(lattice, &self.weights, n, allows);
        if !paths.iter().all(|path| self.is_one_address(lattice, path)) {
            let addresses = decode::best(lattice, &self.weights, n.max(CONSTRAINED_PATHS), allows)
                .into_iter()
                .filter(|path| self.is_one_address(lattice, path))
                .collect::<Vec<_>>();
            if !addresses.is_empty() {
                paths = addresses;
            }
        }
        paths
            .into_iter()
            .take(n)
            .map(|path| {
                let labels = path.labels(lattice);
                let (pieces, corrections) = lattice
                    .units
                    .corrected_pieces(&path.segments(lattice), &lattice.corrections);
                ParsedQuery {
                    tokens: tokens
                        .words
                        .iter()
                        .zip(lattice.units.collapse(&labels))
                        .map(|(token, label)| Tagged {
                            token: token.clone(),
                            label,
                        })
                        .collect(),
                    pieces,
                    corrections,
                    country: path.country,
                    score: path.score,
                }
            })
            .collect()
    }

    fn is_one_address(&self, lattice: &Lattice, path: &decode::Path) -> bool {
        let labels = path.labels(lattice);
        for label in [Label::Country, Label::Region, Label::Postcode] {
            let fields = labels
                .iter()
                .enumerate()
                .filter(|(index, at)| **at == label && (*index == 0 || labels[index - 1] != label))
                .count();
            if fields > 1 {
                return false;
            }
        }
        let numberless_house = path.segments.iter().any(|segment| {
            let span = &lattice.spans[segment.span];
            segment.label == Label::HouseNumber
                && !lattice
                    .units
                    .key(span.start, span.end)
                    .chars()
                    .any(is_numeral)
        });
        if numberless_house {
            return false;
        }
        let places = path
            .segments
            .iter()
            .filter(|segment| {
                matches!(
                    segment.label,
                    Label::Locality | Label::Region | Label::Country
                )
            })
            .filter_map(|segment| {
                let span = &lattice.spans[segment.span];
                lattice.place(span.start, span.end)
            })
            .collect::<Vec<_>>();
        places.iter().enumerate().all(|(index, left)| {
            places[index + 1..]
                .iter()
                .all(|right| self.dictionaries.nest(left, right))
        })
    }

    fn lattice(&self, tokens: &Tokens, suppression: &Suppression) -> Lattice {
        let units = Units::of(tokens);
        let suppression = suppression.in_units(&units);
        let tokens = &units.tokens;
        let infos = tokens
            .words
            .iter()
            .map(|word| TokenInfo::new(word, &self.dictionaries))
            .collect::<Vec<_>>();
        let mut countries = CountryVotes::default();
        let mut spans = Vec::new();
        let mut single = Vec::with_capacity(tokens.len());
        let mut covers = vec![0u16; tokens.len()];
        let longest = MAX_SPAN_TOKENS.max(MAX_UNSPACED_SPAN);
        let runs = units.runs();
        let mut phrases = Phrases::new(self, &units, longest);
        let mut areas = Vec::new();
        let mut places = Vec::new();
        let mut glued_spans = Vec::new();
        let mut corrections = Vec::new();
        for start in 0..tokens.len() {
            for end in start + 1..=(start + longest).min(tokens.len()) {
                if end - start > MAX_SPAN_TOKENS && !units.unspaced(start, end) {
                    break;
                }
                let mut set = FeatureSet::new(tokens, &infos, start, end);
                let mut evidence = end - start == 1;
                let kinds = phrases.kinds(start, end);
                if kinds != 0 {
                    evidence = true;
                    set.phrase(kinds);
                }
                set.phrase_neighbours(phrases.ending[start], phrases.starting[end]);
                if end - start == 1
                    && !infos[start].unspaced
                    && let Some(field) = self.glued_phrase(&tokens.words[start])
                    && self.postcode_matches(tokens, &infos, start, end).is_none()
                {
                    evidence = true;
                    set.glued_phrase(field);
                }
                let mut key = SpanKey::new(phrases.take_key(start, end), &infos[start..end]);
                let glued = end - start > 1 && units.unspaced(start, end);
                if (end - start == 1 && !infos[start].unspaced) || glued {
                    set.affixes(&key.key);
                }
                if glued && runs.contains(&(start, end)) {
                    evidence = true;
                }
                let mut area = false;
                let mut street = false;
                let mut dominant: Option<(Field, u64)> = None;
                for field in Field::NAMES {
                    if suppression.hides(field, start, end) {
                        continue;
                    }
                    if let Some((hit, name)) = self.lookup(field, &mut key) {
                        evidence = true;
                        street |= field == Field::Street;
                        if let (Some(name), Some(label)) = (name, field_label(field)) {
                            corrections.push((start, end, label, name));
                        }
                        if hit.distance == 0 && Field::AREAS.contains(&field) {
                            areas.push((start, end, field == Field::Region));
                            area = true;
                        }
                        set.dictionary_hit(field, &hit, key.key.chars().count());
                        if dominant.is_none_or(|(_, count)| hit.entry.count > count) {
                            dominant = Some((field, hit.entry.count));
                        }
                        if end - start > 1 {
                            for (unit, cover) in covers[start..end].iter_mut().enumerate() {
                                if let Some(feature) = features::cover(field, unit == 0) {
                                    *cover |= 1 << (feature - features::cover_base());
                                }
                            }
                        }
                        if let Some(country) = hit.entry.country {
                            countries.vote(country, field.country_vote(hit.distance, end - start));
                        }
                    }
                }
                if let Some((field, _)) = dominant {
                    set.dominant_field(field);
                }
                if !street && !suppression.hides(Field::Street, start, end) {
                    let held = |field: Field| {
                        [&key.key, &key.expanded]
                            .into_iter()
                            .any(|key| self.dictionaries.contains(field, key))
                    };
                    if held(Field::StreetBare) {
                        evidence = true;
                        set.street_alias(false);
                    } else if held(Field::StreetTail) {
                        evidence = true;
                        set.street_alias(true);
                    }
                }
                if area {
                    let ids = [&key.key, &key.expanded]
                        .into_iter()
                        .map(|key| self.dictionaries.areas(key))
                        .find(|ids| !ids.is_empty());
                    if let Some(ids) = ids {
                        places.push((start, end, ids));
                    }
                }
                if is_number_in_parts(tokens, &infos, start, end)
                    || is_glued_numeral(&units, &infos, start, end)
                {
                    evidence = true;
                    set.number_in_parts();
                }
                if let Some(matches) = self.postcode_matches(tokens, &infos, start, end) {
                    evidence = true;
                    set.postcode_any();
                    for country in matches {
                        set.postcode_in(country);
                        countries.postcode(country, self.dictionaries.country_weight(country));
                    }
                }
                if end - start == 1 {
                    single.push(spans.len());
                }
                if evidence {
                    spans.push(set.into_span());
                } else if glued {
                    glued_spans.push(set);
                }
            }
        }
        let chains = Chains::new(&areas);
        for span in spans.iter_mut() {
            if let Some(chain) = chains.of(&units, span.start, span.end) {
                span.features.push(features::chain_feature(chain));
                if chain.region {
                    span.features.push(features::chain_region());
                }
            }
        }
        for mut set in glued_spans {
            let (start, end) = set.range();
            if let Some(chain) = chains.of(&units, start, end) {
                set.chain(chain.pieces, chain.region);
                spans.push(set.into_span());
            }
        }
        for (unit, bits) in covers.into_iter().enumerate() {
            for bit in (0..16).filter(|bit| bits & (1 << bit) != 0) {
                spans[single[unit]]
                    .features
                    .push(features::cover_base() + bit);
            }
        }
        for &(start, end) in runs.iter().filter(|(start, end)| end - start > longest) {
            let mut set = FeatureSet::new(tokens, &infos, start, end);
            set.affixes(&units.key(start, end));
            if let Some(chain) = chains.of(&units, start, end) {
                set.chain(chain.pieces, chain.region);
            }
            spans.push(set.into_span());
        }
        for span in &mut spans {
            features::ground(span);
        }
        let candidates = countries.top(MAX_CANDIDATE_COUNTRIES, &self.weights);
        let mut lattice = Lattice::new(units, spans, candidates);
        lattice.places = places;
        lattice.corrections = corrections;
        lattice
    }

    fn glued_phrase(&self, word: &str) -> Option<Glued> {
        let split = word
            .char_indices()
            .skip(1)
            .find(|(index, character)| {
                let previous = word[..*index].chars().next_back().expect("a character");
                previous.is_numeric() != character.is_numeric()
            })
            .map(|(index, _)| index)?;
        let (head, tail) = word.split_at(split);
        let numeric = |part: &str| part.chars().all(char::is_numeric);
        let letters = |part: &str| part.chars().all(char::is_alphabetic);
        let phrase = match (numeric(head), numeric(tail)) {
            (true, false) if letters(tail) => tail,
            (false, true) if letters(head) => head,
            _ => return None,
        };
        [
            (Field::Level, Glued::Level),
            (Field::Unit, Glued::Unit),
            (Field::PoBox, Glued::PoBox),
        ]
        .into_iter()
        .find(|(field, _)| self.dictionaries.contains(*field, phrase))
        .map(|(_, glued)| glued)
        .or_else(|| dictionary::is_admin_type(phrase).then_some(Glued::AdminType))
    }

    fn lookup(&self, field: Field, key: &mut SpanKey) -> Option<(Match, Option<String>)> {
        let best = |keys: &mut dyn Iterator<Item = &String>| {
            keys.filter_map(|key| self.dictionaries.get(field, key))
                .max_by_key(|entry| entry.count)
        };
        let exact = best(
            &mut [Some(&key.key), Some(&key.expanded), key.masked.as_ref()]
                .into_iter()
                .flatten(),
        )
        .or_else(|| best(&mut key.spellings.iter()));
        if let Some(entry) = exact {
            return Some((Match { entry, distance: 0 }, None));
        }
        if field == Field::Poi && key.tokens < 2 && key.chars < MIN_FUZZY_POI_CHARS {
            return None;
        }
        let dfa = if field == Field::Poi {
            key.poi_dfa()?
        } else {
            key.dfa()?
        };
        let first = key.expanded.chars().next().map_or(0, char::len_utf8);
        let (name, hit) = self
            .dictionaries
            .fuzzy(field, &dfa, &key.expanded[..first])?;
        Some((hit, Some(name)))
    }

    fn postcode_matches(
        &self,
        tokens: &Tokens,
        infos: &[TokenInfo],
        start: usize,
        end: usize,
    ) -> Option<Vec<CountryCode>> {
        if end - start > postcode::MAX_TOKENS
            || !infos[start..end].iter().any(|info| info.has_digit)
        {
            return None;
        }
        let words = &tokens.words[start..end];
        let mut matches = self.postcodes.matches(&words.join(" "));
        if words.len() > 1 {
            matches.extend(self.postcodes.matches(&words.concat()));
        }
        if let Some((prefix, rest)) = country_prefix(words) {
            let prefix = prefix.to_ascii_uppercase();
            matches.extend(
                self.postcodes
                    .matches(&rest)
                    .into_iter()
                    .filter(|country| country.as_str().starts_with(&prefix)),
            );
        }
        matches.sort_unstable();
        matches.dedup();
        (!matches.is_empty()).then_some(matches)
    }
}

impl Tagger for LatticeTagger {
    fn tag(&self, query: &str) -> Vec<Tagged> {
        let tokens = Tokens::new(query);
        self.parse_tokens(&tokens, 1)
            .into_iter()
            .next()
            .map(|parsed| parsed.tokens)
            .unwrap_or_else(|| {
                tokens
                    .words
                    .into_iter()
                    .map(|token| Tagged {
                        token,
                        label: Label::Other,
                    })
                    .collect()
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Chain {
    pub pieces: usize,
    pub region: bool,
}

struct Chains {
    from: HashMap<usize, Vec<(usize, bool)>>,
}

impl Chains {
    fn new(areas: &[(usize, usize, bool)]) -> Self {
        let mut from = HashMap::<usize, Vec<(usize, bool)>>::new();
        for &(start, end, region) in areas {
            from.entry(start).or_default().push((end, region));
        }
        Self { from }
    }

    fn of(&self, units: &Units, start: usize, end: usize) -> Option<Chain> {
        if end - start < 2 || self.from.is_empty() || !units.unspaced(start, end) {
            return None;
        }
        let mut best_by_offset = vec![None; end - start + 1];
        best_by_offset[0] = Some(Chain {
            pieces: 0,
            region: false,
        });
        let score = |chain: Chain| (chain.pieces, chain.region);
        for at in start..end {
            let Some(chain) = best_by_offset[at - start] else {
                continue;
            };
            for &(to, is_region) in self.from.get(&at).into_iter().flatten() {
                if to > end {
                    continue;
                }
                let next = Chain {
                    pieces: chain.pieces + 1,
                    region: chain.region || is_region,
                };
                let cell = &mut best_by_offset[to - start];
                if cell.is_none_or(|held| score(next) > score(held)) {
                    *cell = Some(next);
                }
            }
        }
        best_by_offset[end - start].filter(|chain| chain.pieces >= 2)
    }
}

struct Phrases {
    spans: Vec<(usize, usize, u8)>,
    keys: Vec<String>,
    first_key: Vec<usize>,
    ending: Vec<u8>,
    starting: Vec<u8>,
}

impl Phrases {
    fn new(tagger: &LatticeTagger, units: &Units, longest: usize) -> Self {
        let len = units.len();
        let mut phrases = Self {
            spans: Vec::new(),
            keys: Vec::with_capacity(len * MAX_SPAN_TOKENS),
            first_key: Vec::with_capacity(len),
            ending: vec![0; len + 1],
            starting: vec![0; len + 1],
        };
        for start in 0..len {
            phrases.first_key.push(phrases.keys.len());
            for end in start + 1..=(start + longest).min(len) {
                if end - start > MAX_SPAN_TOKENS && !units.unspaced(start, end) {
                    break;
                }
                let key = units.key(start, end);
                let kinds = Field::PHRASES
                    .iter()
                    .enumerate()
                    .filter(|(_, field)| tagger.dictionaries.contains(**field, &key))
                    .fold(0u8, |kinds, (bit, _)| kinds | 1 << bit);
                if kinds != 0 {
                    phrases.spans.push((start, end, kinds));
                    phrases.ending[end] |= kinds;
                    phrases.starting[start] |= kinds;
                }
                phrases.keys.push(key);
            }
        }
        phrases
    }

    fn take_key(&mut self, start: usize, end: usize) -> String {
        std::mem::take(&mut self.keys[self.first_key[start] + (end - start - 1)])
    }

    fn kinds(&self, start: usize, end: usize) -> u8 {
        self.spans
            .iter()
            .find(|span| span.0 == start && span.1 == end)
            .map_or(0, |span| span.2)
    }
}

struct SpanKey {
    key: String,
    expanded: String,
    masked: Option<String>,
    spellings: Vec<String>,
    fuzzy: bool,
    tokens: usize,
    chars: usize,
    dfa: Option<Rc<DFA>>,
    poi_dfa: Option<Rc<DFA>>,
}

impl SpanKey {
    fn new(key: String, span: &[TokenInfo]) -> Self {
        let expanded = expand_key(&key);
        let chars = expanded.chars().count();
        let fuzzy = (MIN_FUZZY_CHARS..=MAX_FUZZY_CHARS).contains(&chars)
            && !span.iter().any(|info| info.has_digit || info.unspaced)
            && !span.iter().all(|info| info.known_word);
        Self {
            tokens: expanded.split(' ').count(),
            masked: mask_numerals(&key),
            spellings: spellings(&key),
            key,
            expanded,
            fuzzy,
            chars,
            dfa: None,
            poi_dfa: None,
        }
    }

    fn edits(&self) -> u8 {
        if (TWO_EDIT_CHARS..=MAX_TWO_EDIT_CHARS).contains(&self.chars) {
            2
        } else {
            1
        }
    }

    fn poi_dfa(&mut self) -> Option<Rc<DFA>> {
        if !self.fuzzy {
            return None;
        }
        if self.poi_dfa.is_none() {
            self.poi_dfa = Some(match &self.dfa {
                Some(dfa) if self.edits() == 1 => Rc::clone(dfa),
                _ => Rc::new(automaton_builder(1).build_dfa(&self.expanded)),
            });
        }
        self.poi_dfa.clone()
    }

    fn dfa(&mut self) -> Option<Rc<DFA>> {
        if !self.fuzzy {
            return None;
        }
        if self.dfa.is_none() {
            self.dfa = Some(match &self.poi_dfa {
                Some(dfa) if self.edits() == 1 => Rc::clone(dfa),
                _ => Rc::new(automaton_builder(self.edits()).build_dfa(&self.expanded)),
            });
        }
        self.dfa.clone()
    }
}

fn is_number_in_parts(tokens: &Tokens, infos: &[TokenInfo], start: usize, end: usize) -> bool {
    let digits = |info: &TokenInfo| info.shape == features::Shape::Digits;
    (2..=3).contains(&(end - start))
        && infos[start..end].iter().all(digits)
        && matches!(tokens.joiners[end - 1], Some('.' | '/'))
        && !start
            .checked_sub(1)
            .is_some_and(|before| digits(&infos[before]))
}

fn is_glued_numeral(units: &Units, infos: &[TokenInfo], start: usize, end: usize) -> bool {
    end - start >= 2
        && units.unspaced(start, end)
        && infos[start..end]
            .iter()
            .all(|info| info.shape == features::Shape::Digits)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Glued {
    Unit,
    PoBox,
    Level,
    AdminType,
}

fn country_prefix(words: &[String]) -> Option<(&str, String)> {
    let is_prefix = |part: &str| {
        (1..=2).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_alphabetic())
    };
    match words {
        [first, rest @ ..] if !rest.is_empty() && is_prefix(first) => {
            Some((first.as_str(), rest.join(" ")))
        }
        [only] => {
            let split = only.find(|character: char| character.is_ascii_digit())?;
            let (prefix, rest) = only.split_at(split);
            is_prefix(prefix).then(|| (prefix, rest.to_string()))
        }
        _ => None,
    }
}

fn spellings(key: &str) -> Vec<String> {
    let mut spellings = abbreviations::abbreviations().spellings(key);
    for alias in dictionary::admin_aliases(key) {
        if !spellings.contains(&alias) {
            spellings.push(alias);
        }
    }
    spellings
}

fn field_label(field: Field) -> Option<Label> {
    match field {
        Field::Locality => Some(Label::Locality),
        Field::Region => Some(Label::Region),
        Field::Street => Some(Label::Street),
        Field::Country => Some(Label::Country),
        Field::Poi => Some(Label::Other),
        _ => None,
    }
}

fn expand_key(key: &str) -> String {
    let mut words = expand_address_abbreviations(key)
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    expand_final_direction(&mut words);
    words.join(" ")
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Suppression(Vec<(Field, usize, usize)>);

impl Suppression {
    pub(crate) fn hide(&mut self, field: Field, start: usize, end: usize) {
        self.0.push((field, start, end));
    }

    fn in_units(&self, units: &Units) -> Self {
        Self(
            self.0
                .iter()
                .map(|&(field, start, end)| {
                    let (start, end) = units.of_tokens(start, end);
                    (field, start, end)
                })
                .collect(),
        )
    }

    fn hides(&self, field: Field, start: usize, end: usize) -> bool {
        self.0
            .iter()
            .any(|&(hidden, from, to)| hidden == field && start < to && from < end)
    }
}

#[derive(Debug, Default)]
struct CountryVotes {
    votes: Vec<(CountryCode, f32, f32)>,
}

impl CountryVotes {
    fn entry(&mut self, country: CountryCode) -> &mut (CountryCode, f32, f32) {
        let index = match self.votes.iter().position(|(code, ..)| *code == country) {
            Some(index) => index,
            None => {
                self.votes.push((country, 0.0, 0.0));
                self.votes.len() - 1
            }
        };
        &mut self.votes[index]
    }

    fn vote(&mut self, country: CountryCode, weight: f32) {
        self.entry(country).1 += weight;
    }

    fn postcode(&mut self, country: CountryCode, known: f32) {
        let entry = self.entry(country);
        entry.2 = entry.2.max(known + 1.0);
    }

    fn top(mut self, limit: usize, weights: &Weights) -> Vec<CountryCode> {
        self.votes.sort_by(|left, right| {
            let known = |code: &CountryCode| f32::from(u8::from(weights.knows(*code)));
            (right.1, right.2 + known(&right.0))
                .partial_cmp(&(left.1, left.2 + known(&left.0)))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.0.cmp(&right.0))
        });
        self.votes
            .into_iter()
            .take(limit)
            .map(|(code, ..)| code)
            .collect()
    }
}

#[cfg(test)]
mod tests;
