//! The lattice address parser.
//!
//! A query is split into the tokens the text index uses, and each token into
//! the units the lattice is built on (see [`Units`]): a token is one unit,
//! except that every character of a script written without spaces (Han,
//! kana, Thai) is a unit of its own, so "東京都千代田区" reads the same with
//! spaces between its names, without them, or with a space after every
//! character. Every span of one to [`MAX_SPAN_TOKENS`] units, or of up to
//! [`MAX_UNSPACED_SPAN`] units written without spaces, is looked up in the
//! Pack's own dictionaries of
//! street, locality, region and country names (see [`dictionary`]), exactly
//! and then within a small edit distance, and matched against per-country
//! postcode patterns (see [`postcode`]). Each span with evidence, and every
//! single token, becomes an edge of a lattice that may carry any label. A
//! linear model scores each edge from its features and each pair of
//! consecutive labels from a per-country transition table, and a semi-Markov
//! Viterbi pass returns the best labellings (see [`decode`]).
//!
//! Nothing in the parser is written per country: the vocabulary lives in the
//! Pack, the postcode patterns and address templates are vendored data, and
//! the weights are learned from the Pack's own records (see [`train`]).

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

/// The longest span looked up as one name.
pub const MAX_SPAN_TOKENS: usize = 5;
/// The longest span of units written without spaces between them looked up
/// as one name: a name of a script without spaces is one unit per character
/// ("千代田区" is four, "南アルプス市" six).
pub const MAX_UNSPACED_SPAN: usize = 10;
/// How many labellings [`LatticeTagger::parse`] returns at most.
pub const N_BEST: usize = 3;
/// How many labellings are decoded per country before those that cannot be
/// one address are dropped (see [`LatticeTagger::is_one_address`]).
const CONSTRAINED_PATHS: usize = 8;
/// How many candidate countries are decoded besides the global fallback.
const MAX_CANDIDATE_COUNTRIES: usize = 3;
/// Spans shorter than this many characters are only looked up exactly: one
/// edit turns most two- and three-letter words into many others.
const MIN_FUZZY_CHARS: usize = 4;
/// Spans at least this long may be two edits from a name...
const TWO_EDIT_CHARS: usize = 8;
/// ...up to this long. A longer span may be one edit from a name: the
/// automaton of two edits grows with the key, and costs hundreds of
/// microseconds to build for a key of twenty characters.
const MAX_TWO_EDIT_CHARS: usize = 16;
/// Spans longer than this are only looked up exactly.
const MAX_FUZZY_CHARS: usize = 32;
/// A one-token span shorter than this is only looked up exactly in the POI
/// names: they are many, and a short word is one edit from many venues.
const MIN_FUZZY_POI_CHARS: usize = 6;
/// Characters that end one field of a query and start the next.
const FIELD_BREAKS: [char; 4] = [',', ';', '\n', '|'];
/// Characters that join two words into one written unit: the parts of a
/// number ("993.5", "1/2") or a unit and its house number ("306-1333").
pub const JOINERS: [char; 3] = ['.', '/', '-'];

/// A two-letter ISO 3166-1 country code, upper case.
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

/// A query split into the words [`search_key`] produces, with where
/// punctuation that separates fields (a comma, a semicolon, a line break)
/// stood between them, and which words punctuation joins to the word before
/// ("993.5", "1/2", "306-1333"). Commas are a feature of the parse, never its
/// rule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tokens {
    pub words: Vec<String>,
    /// `breaks[index]` is whether a field break precedes word `index`.
    pub breaks: Vec<bool>,
    /// `joiners[index]` is the one character of [`JOINERS`] that joins word
    /// `index` to the word before it, with no space around it.
    pub joiners: Vec<Option<char>>,
}

impl Tokens {
    /// Split raw query text. Joined by single spaces the words are exactly
    /// [`search_key`] of the text.
    pub fn new(text: &str) -> Self {
        let mut tokens = Self::default();
        let mut pending_break = false;
        // What stood between the last word and this one, while it can still
        // be a joiner.
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

    /// Words already normalized, with no breaks or joiners between them.
    pub fn from_words<S: AsRef<str>>(words: &[S]) -> Self {
        Self {
            words: words.iter().map(|word| word.as_ref().to_string()).collect(),
            breaks: vec![false; words.len()],
            joiners: vec![None; words.len()],
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

/// The scripts written without spaces between words: a space between two of
/// their characters marks no word boundary.
const UNSPACED_SCRIPTS: [Script; 7] = [
    Script::Han,
    Script::Hiragana,
    Script::Katakana,
    Script::Thai,
    Script::Lao,
    Script::Khmer,
    Script::Myanmar,
];

/// Whether `character` belongs to a script written without spaces between
/// words ([`UNSPACED_SCRIPTS`]), by its Unicode Script_Extensions property:
/// the prolonged sound mark "ー" and the iteration mark "々" are, as kana and
/// Han are, and a character of every script (a digit) is not.
pub fn is_unspaced(character: char) -> bool {
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

/// The value of a numeral character that Unicode does not class as a
/// number: an ideograph of [`NUMERALS`].
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

/// Whether `character` is a numeral of any script: a Unicode number (`Nd`,
/// `Nl`, `No`, so fullwidth and other scripts' digits) or a numeral
/// ideograph of [`NUMERALS`].
pub fn is_numeral(character: char) -> bool {
    character.is_numeric() || numeral_value(character).is_some()
}

/// `key` with each run of numerals written as `#`, the form numbered areas
/// of one kind share ("#丁目" of "5丁目" and "四丁目", "sector #"), or
/// `None` when it has no numeral or nothing but numerals.
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

/// The key a name is stored under in the parser's dictionaries: its
/// normalized words as [`Units`], glued as a query's are, so a name written
/// with spaces between characters of a script without spaces
/// ("とうきょう たからづかげきじょう") is found by the units of a query that
/// writes it.
pub fn dictionary_key(name: &str) -> Option<String> {
    let normalized = search_key(name)?;
    let words = normalized.split(' ').collect::<Vec<_>>();
    let units = Units::of(&Tokens::from_words(&words));
    Some(units.key(0, units.len()))
}

/// The units a lattice is built on: the tokens of a [`Tokens`], except that
/// a token holding characters of a script written without spaces is split
/// into one unit per such character and one per run of other characters
/// ("丸の内2" is "丸", "の", "内", "2"). Units written with no space between
/// them, or with only a space between a character of such a script and
/// another or a digit (see [`is_neutral`]), are *glued*: they join without a
/// space in a dictionary key, so "東京都千代田区", "東京都 千代田区" and
/// "東 京 都 千 代 田 区" are the same units with the same keys, and so are
/// "5番2号" and "5 番 2 号".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Units {
    /// The units as words, with the field breaks and joiners of the tokens
    /// they start.
    pub tokens: Tokens,
    /// The token each unit is part of.
    pub token_of: Vec<usize>,
    /// `glued[index]` is whether unit `index` joins the unit before it
    /// without a space.
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

    /// Add a unit of token `index` of `tokens`, `first` when it starts it.
    /// The first unit of a token is glued to the unit before it when only a
    /// space stands between them and one side is a character of a script
    /// without spaces and the other is too or is [`is_neutral`].
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

    /// The units of `start..end` as one dictionary key: glued units joined
    /// directly, others by a space.
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

    /// The maximal runs of two or more glued units, as `start..end`.
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

    /// Whether every unit of `start..end` after the first is glued to the
    /// one before it.
    fn unspaced(&self, start: usize, end: usize) -> bool {
        self.glued[start + 1..end].iter().all(|glued| *glued)
    }

    /// The units of the tokens `start..end`.
    fn of_tokens(&self, start: usize, end: usize) -> (usize, usize) {
        let first = |token: usize| self.token_of.partition_point(|of| *of < token);
        (first(start), first(end))
    }

    /// One label per unit from one label per token.
    pub fn expand(&self, labels: &[Label]) -> Vec<Label> {
        self.token_of.iter().map(|token| labels[*token]).collect()
    }

    /// One label per token from one label per unit: the label of most of
    /// the token's characters, the first of them on a tie.
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

    /// The units as written words with their labels, from the labelled
    /// segments of a path. Units not glued together are words of their own,
    /// as tokens are. The glued units of a segment are one word, a name the
    /// dictionaries know ("千代田区"), and so are glued one-unit segments
    /// with one label, a name read a character at a time ("丸", "の", "内"
    /// is "丸の内"), so a search looks up the names the text index holds.
    pub fn pieces(&self, segments: &[(usize, usize, Label)]) -> Vec<Tagged> {
        self.corrected_pieces(segments, &[]).0
    }

    /// [`Self::pieces`], each with the word of the name it is a typo of
    /// when that word differs from it (see [`Self::unit_corrections`]).
    fn corrected_pieces(
        &self,
        segments: &[(usize, usize, Label)],
        corrections: &[(usize, usize, Label, String)],
    ) -> (Vec<Tagged>, Vec<Option<String>>) {
        let by_unit = self.unit_corrections(segments, corrections);
        let mut pieces: Vec<Tagged> = Vec::with_capacity(segments.len());
        let mut corrected: Vec<Option<String>> = Vec::with_capacity(segments.len());
        // Whether the last piece is a run of one-unit segments.
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

    /// The word of the name each unit is a typo of, from a lattice's
    /// `corrections`: of a span whose every unit `segments` label as its
    /// name reads, so a name read a word at a time ("lamton", "quay" as two
    /// street segments) is still corrected. A span that is a segment comes
    /// first, then the longest.
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

    /// The word of `name` each unit of `start..end` is a typo of, `None`
    /// where it is the unit as the text index expands it. `None` when the
    /// span's words do not pair one to one with the name's, or it has a
    /// glued unit: fuzzy lookup skips scripts without spaces.
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

/// The characters between two words, while they may be one joiner.
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

/// One labelling of a query, best first in [`LatticeTagger::parse`].
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedQuery {
    /// One label per token of the query, as [`Tokens::new`] splits it.
    pub tokens: Vec<Tagged>,
    /// The query as written words with their labels (see
    /// [`Units::pieces`]): its tokens, except that a token of a script
    /// written without spaces is split where its label changes, and glued
    /// characters with one label are one word.
    pub pieces: Vec<Tagged>,
    /// One per piece: the word of the name the parser read a misspelt
    /// piece as ("lambton" of "lamton"), `None` for a piece read as written.
    /// [`Tagged`] stays the written word, as the parse contract has it.
    pub corrections: Vec<Option<String>>,
    /// The country whose transition table and postcode pattern scored it,
    /// `None` for the global fallback.
    pub country: Option<CountryCode>,
    pub score: f32,
}

impl ParsedQuery {
    /// The query's words with `label`, in order.
    pub fn words(&self, label: Label) -> impl Iterator<Item = &str> {
        self.tokens
            .iter()
            .filter(move |tagged| tagged.label == label)
            .map(|tagged| tagged.token.as_str())
    }
}

/// Labels the tokens of free-text address queries from the Pack's own
/// dictionaries and a learned linear model. Cheap to share across threads.
pub struct LatticeTagger {
    dictionaries: Dictionaries,
    postcodes: &'static Table,
    weights: Weights,
}

/// The builder of the automata of names within `edits` edits (one or two),
/// transpositions counted as one: built once per process and shared, as
/// Tantivy shares its builders, since the two-edit one takes milliseconds
/// and megabytes to build.
pub(crate) fn automaton_builder(edits: u8) -> &'static LevenshteinAutomatonBuilder {
    static BUILDERS: [OnceLock<LevenshteinAutomatonBuilder>; 2] =
        [OnceLock::new(), OnceLock::new()];
    BUILDERS[usize::from(edits - 1)].get_or_init(|| LevenshteinAutomatonBuilder::new(edits, true))
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
    /// Open the dictionaries of a Pack file or Pack directory, and the
    /// weights trained on it when it was built, or else the built-in ones.
    pub fn open(pack: &Path) -> Result<Self> {
        let path = resolve_pack_path(pack)?;
        Self::from_container(&Container::open(&path)?)
    }

    /// The tagger of an open Pack file.
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

    /// Use `weights` from now on.
    pub fn set_weights(&mut self, weights: Weights) {
        self.weights = weights;
    }

    /// The best labellings of raw query text, best first, at most
    /// [`N_BEST`], each with a different label sequence. Empty when the
    /// query has no words.
    pub fn parse(&self, query: &str) -> Vec<ParsedQuery> {
        self.parse_tokens(&Tokens::new(query), N_BEST)
    }

    /// The best `n` labellings of `tokens`.
    pub fn parse_tokens(&self, tokens: &Tokens, n: usize) -> Vec<ParsedQuery> {
        if tokens.is_empty() || n == 0 {
            return Vec::new();
        }
        let lattice = self.lattice(tokens, &Suppression::default());
        let mut paths = decode::best(&lattice, &self.weights, n);
        // The best labellings are almost always addresses; only when one is
        // not are more decoded to find those that are. When none is, the
        // best ones still read it.
        if !paths.iter().all(|path| self.is_one_address(&lattice, path)) {
            let addresses = decode::best(&lattice, &self.weights, n.max(CONSTRAINED_PATHS))
                .into_iter()
                .filter(|path| self.is_one_address(&lattice, path))
                .collect::<Vec<_>>();
            if !addresses.is_empty() {
                paths = addresses;
            }
        }
        paths
            .into_iter()
            .take(n)
            .map(|path| {
                let labels = path.labels(&lattice);
                let (pieces, corrections) = lattice
                    .units
                    .corrected_pieces(&path.segments(&lattice), &lattice.corrections);
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

    /// Whether `path` reads its query as one address can be written: each
    /// of the country, the region and the postcode is one field at most, a
    /// house number has a number, and the places it names that the Pack
    /// knows are each named once and lie one in another (a ward in its city,
    /// the city in its region), so no admin level is named twice.
    /// The model scores parts of a labelling; this holds of the whole.
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
        // A house number has a number in it: "堺" alone is none.
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
        // Each place is named once: a second field naming only the place of
        // another ("那覇" after "那覇市") is part of something else, a venue
        // named after its city.
        places.iter().enumerate().all(|(index, left)| {
            places[index + 1..]
                .iter()
                .all(|right| self.dictionaries.nest(left, right))
        })
    }

    /// Every span of the units of `tokens` with its evidence, and the
    /// countries worth decoding it for. Training passes the token ranges
    /// whose dictionary hits it hides, to teach the model names the Pack
    /// does not know.
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
        // The span of each unit alone, and the dict_cover features each
        // unit gets from the longer names it lies in, one bit per feature.
        let mut single = Vec::with_capacity(tokens.len());
        let mut covers = vec![0u16; tokens.len()];
        let longest = MAX_SPAN_TOKENS.max(MAX_UNSPACED_SPAN);
        let runs = units.runs();
        let mut phrases = Phrases::new(self, &units, longest);
        // Exact area names, `(start, end, is a region)`, and the glued
        // spans without other evidence, which may be chains of them.
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
                // A postcode written after its country's letters ("de09496")
                // is no unit glued to its number.
                if end - start == 1
                    && !infos[start].unspaced
                    && let Some(field) = self.glued_phrase(&tokens.words[start])
                    && self.postcode_matches(tokens, &infos, start, end).is_none()
                {
                    evidence = true;
                    set.glued_phrase(field);
                }
                let mut key = SpanKey::new(phrases.take_key(start, end), &infos[start..end]);
                // A word's affixes: of a token, or of glued units as one.
                let glued = end - start > 1 && units.unspaced(start, end);
                if (end - start == 1 && !infos[start].unspaced) || glued {
                    set.affixes(&key.key);
                }
                // A whole run of glued units is an edge of its own, a name
                // the dictionaries may not hold.
                if glued && runs.contains(&(start, end)) {
                    evidence = true;
                }
                let mut area = false;
                // The field of names the most records use the span in.
                let mut dominant: Option<(Field, u64)> = None;
                for field in Field::NAMES {
                    if suppression.hides(field, start, end) {
                        continue;
                    }
                    if let Some((hit, name)) = self.lookup(field, &mut key) {
                        evidence = true;
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
        // A glued span that is two or more area names written one after
        // the other ("横浜市西区") is an edge with the chain it is.
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
        // Runs longer than the longest span looked up.
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

    /// The vocabulary a token glues to its number: "4og" is a floor, "apt4"
    /// a unit, "4chome" a numbered area. The token is one run of digits and
    /// one of letters, either way round, and the letters a unit, PO box or
    /// level phrase or an admin type word.
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

    /// The best hit of `key` in `field`: exact, as written or with its
    /// abbreviations expanded the way the text index expands them, else the
    /// nearest name the key's fuzzy automaton accepts, given with the hit.
    fn lookup(&self, field: Field, key: &mut SpanKey) -> Option<(Match, Option<String>)> {
        // The span as written first: another spelling of it (the name
        // without its type word, an abbreviation in full) is only read when
        // the span itself names nothing, so "verwaltungsgemeinschaft
        // stockach" is the district it names, not the city "stockach".
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

    /// The countries whose postcode pattern `start..end` matches, written
    /// with or without the spaces between its tokens. `None` when none does
    /// or the span cannot be a postcode: one to three tokens with a digit.
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
        // A postcode written after its country's letters ("D-80331",
        // "de09496", "CA-M5V 1M9"), as Nominatim reads it and libpostal
        // writes it: the rest is a postcode of a country whose code starts
        // with them.
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

/// A span that is area names written one after the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Chain {
    /// How many names, two or more.
    pub pieces: usize,
    /// Whether one of them is a region name.
    pub region: bool,
}

/// The exact area names of a query, to find the glued spans that are two or
/// more of them with no break between them.
struct Chains {
    /// `(end, is a region)` of the area names starting at each unit.
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

    /// The chain `start..end` is: the most names that cover it end to end,
    /// when that is two or more and the span is glued.
    fn of(&self, units: &Units, start: usize, end: usize) -> Option<Chain> {
        if end - start < 2 || self.from.is_empty() || !units.unspaced(start, end) {
            return None;
        }
        // best[offset]: the most names covering start..start + offset, and
        // whether one is a region.
        let mut best: Vec<Option<(usize, bool)>> = vec![None; end - start + 1];
        best[0] = Some((0, false));
        for at in start..end {
            let Some((pieces, region)) = best[at - start] else {
                continue;
            };
            for &(to, is_region) in self.from.get(&at).into_iter().flatten() {
                if to > end {
                    continue;
                }
                let next = (pieces + 1, region || is_region);
                let cell = &mut best[to - start];
                if cell.is_none_or(|cell| next > cell) {
                    *cell = Some(next);
                }
            }
        }
        best[end - start]
            .filter(|(pieces, _)| *pieces >= 2)
            .map(|(pieces, region)| Chain { pieces, region })
    }
}

/// The vocabulary phrases of a query's units (see [`Field::PHRASES`]): the
/// kinds of phrase each span is, one bit per field, and the kinds of the
/// phrases that end before and start at each unit. The dictionary key of
/// every span is made here once, and taken by the lattice after.
struct Phrases {
    /// `(start, end, kinds)` of every span that is a phrase.
    spans: Vec<(usize, usize, u8)>,
    /// The key of every span looked up, in the order they are looked up.
    keys: Vec<String>,
    /// The index in `keys` of the first span starting at each unit.
    first_key: Vec<usize>,
    /// `ending[unit]`: the kinds of the phrases that end right before `unit`.
    ending: Vec<u8>,
    /// `starting[unit]`: the kinds of the phrases that start at `unit`; one
    /// past the last unit is empty.
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

    /// The key of `start..end`, a span this looked up; once.
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

/// A span as dictionary keys, with the fuzzy automaton of its expanded key
/// built once for every field it is looked up in, and only when an exact
/// lookup misses.
struct SpanKey {
    key: String,
    expanded: String,
    /// The key with its numerals masked ([`mask_numerals`]), which names
    /// the numbered areas of a kind the Pack holds many of.
    masked: Option<String>,
    /// The key with its street types and directions in their other forms,
    /// in every language ([`abbreviations::Abbreviations::spellings`]).
    spellings: Vec<String>,
    /// Whether the span may be a typo of a name: it is at least
    /// [`MIN_FUZZY_CHARS`] long, has no digit, not every word of it is a
    /// known word, and it is not of a script written without spaces. A house
    /// number is never corrected, a span of real words is no typo, and in a
    /// script without spaces every character is a word, so one edit names
    /// another place rather than a typo of this one.
    fuzzy: bool,
    /// Tokens and characters of the expanded key.
    tokens: usize,
    chars: usize,
    /// The automaton of the names within one or two edits, and that of the
    /// names within one for the POI names: one automaton when both allow
    /// one edit.
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

    /// Edits the automaton of the names allows: two for keys of
    /// [`TWO_EDIT_CHARS`] to [`MAX_TWO_EDIT_CHARS`] characters, else one.
    fn edits(&self) -> u8 {
        if (TWO_EDIT_CHARS..=MAX_TWO_EDIT_CHARS).contains(&self.chars) {
            2
        } else {
            1
        }
    }

    /// The automaton of the names within one edit of the expanded key, for
    /// the POI names: they are many, and two edits of a span reach many
    /// venues. `None` when the span is no typo.
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

    /// The automaton of the names within [`Self::edits`] edits of the
    /// expanded key; `None` when the span is no typo. Names are only
    /// matched when they start with the key's first character, which typing
    /// errors seldom change (see [`Dictionaries::fuzzy`]).
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

/// Whether `start..end` is two or three runs of digits, the last joined to
/// the one before by a dot or a slash, and no digits before them: one house
/// number written in parts ("993.5", "1384 1/2", not the "1/2" of it).
fn is_number_in_parts(tokens: &Tokens, infos: &[TokenInfo], start: usize, end: usize) -> bool {
    let digits = |info: &TokenInfo| info.shape == features::Shape::Digits;
    (2..=3).contains(&(end - start))
        && infos[start..end].iter().all(digits)
        && matches!(tokens.joiners[end - 1], Some('.' | '/'))
        && !start
            .checked_sub(1)
            .is_some_and(|before| digits(&infos[before]))
}

/// Whether `start..end` is two or more glued units that are all numerals: a
/// number written in numeral ideographs, one unit per character.
fn is_glued_numeral(units: &Units, infos: &[TokenInfo], start: usize, end: usize) -> bool {
    end - start >= 2
        && units.unspaced(start, end)
        && infos[start..end]
            .iter()
            .all(|info| info.shape == features::Shape::Digits)
}

/// What a number is glued to in one token (see
/// [`LatticeTagger::glued_phrase`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Glued {
    Unit,
    PoBox,
    Level,
    /// A type of numbered area ("4chome").
    AdminType,
}

/// The letters of a country a postcode may be written after, one or two
/// Latin letters as a word of their own ("d 80331") or glued to the digits
/// ("de09496"), and the rest of the words.
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

/// Other spellings of a dictionary key looked up besides it: its street
/// types and directions in their other forms, and the key without the admin
/// type word it is written with ("nordrhein westfalen" of "bundesland
/// nordrhein westfalen", "nagoya" of "nagoyashi").
fn spellings(key: &str) -> Vec<String> {
    let mut spellings = abbreviations::abbreviations().spellings(key);
    for alias in dictionary::admin_aliases(key) {
        if !spellings.contains(&alias) {
            spellings.push(alias);
        }
    }
    spellings
}

/// The label a name of `field` is read as, for the fields whose names a
/// span is corrected to.
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

/// A dictionary key with street-type and direction abbreviations expanded,
/// as the text index stores names ("king st w" is "king street west").
fn expand_key(key: &str) -> String {
    let mut words = expand_address_abbreviations(key)
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    expand_final_direction(&mut words);
    words.join(" ")
}

/// Dictionary hits training hides, as `(field, start, end)` token ranges:
/// every hit of the field on a span that overlaps the range is dropped.
#[derive(Debug, Clone, Default)]
pub(crate) struct Suppression(Vec<(Field, usize, usize)>);

impl Suppression {
    pub(crate) fn hide(&mut self, field: Field, start: usize, end: usize) {
        self.0.push((field, start, end));
    }

    /// The same hidden ranges, given in tokens, in units of `units`.
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

/// Evidence for which country a query is in, to pick the countries decoded.
#[derive(Debug, Default)]
struct CountryVotes {
    /// Votes from dictionary hits, and the Pack's weight of the countries
    /// whose postcode pattern matched.
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

    /// The `limit` countries with the most dictionary votes, then those whose
    /// postcode matched and that the Pack or the weights know best, then by
    /// code, so the choice is deterministic.
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
mod smoke {
    use super::{dictionary::Entry, *};

    #[test]
    fn a_plain_spelling_is_labelled_as_the_name_it_folds_to() {
        let entry = |count| Entry {
            count,
            country: CountryCode::parse("PL"),
            levels: 0,
        };
        let locality = dictionary_key("Łódź").expect("key");
        let street = dictionary_key("Piotrkowska").expect("key");
        let tagger = LatticeTagger::new(
            Dictionaries::from_entries([
                (Field::Locality, locality.as_str(), entry(700_000)),
                (Field::Street, street.as_str(), entry(300)),
            ]),
            Weights::built_in(),
        );
        let labels = |query: &str| {
            tagger
                .tag(query)
                .into_iter()
                .map(|tagged| (tagged.token, tagged.label))
                .collect::<Vec<_>>()
        };
        let accented = labels("Piotrkowska 12, Łódź");
        assert_eq!(
            accented.last(),
            Some(&("lodz".to_string(), Label::Locality))
        );
        assert_eq!(labels("Piotrkowska 12, Lodz"), accented);
        assert_eq!(labels("ＰＩＯＴＲＫＯＷＳＫＡ １２, ŁÓDŹ"), accented);
    }
}
#[cfg(test)]
mod tests;
