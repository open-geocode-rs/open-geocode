//! The features of a lattice span.
//!
//! Features say what a span looks like and what the dictionaries know of it,
//! never which words it holds, so a model learned on one Pack scores queries
//! of any other: a word's identity is the Pack's business, its role the
//! model's. Each feature is conjoined with the label of the edge it is on,
//! so one feature id carries one weight per label.
//!
//! The one exception reads a little of a word's letters: the affix blocks
//! hash the character n-grams (n = 2, 3, 4) a word starts and ends with,
//! shorter than the word, into [`AFFIX_BUCKETS`] buckets per side and n, so
//! endings such as "-strasse", "-ville" or "-shi" tell their role on names
//! no dictionary holds. The word of a span is its one token, or the units of
//! a script written without spaces glued into one key ("みなとみらい").
//!
//! The words themselves are hashed too, each word of a span into
//! [`WORD_BUCKETS`] buckets, so the model learns the words the Pack it is
//! trained on writes its streets, venues and places with ("strasse",
//! "bäckerei", "sebun"). The model stays one per Pack: the words are the
//! Pack's own, learned when it is built.
//!
//! ```text
//! ids 0 .. NAMED                      the named blocks of BLOCKS, in order
//! NAMED + (side * 3 + n - 2) * 1024   bucket 0 of the affix block of side
//!   + bucket                          (0 prefix, 1 suffix) and n (2..=4)
//! WORDS + bucket                      a word of the span
//! ```

use super::{
    CountryCode, MAX_SPAN_TOKENS, Tokens,
    decode::Span,
    dictionary::{Dictionaries, Field, Match, level},
};

/// The feature blocks, in id order, with the names of their features.
const BLOCKS: &[(&str, &[&str])] = &[
    ("bias", &["bias"]),
    ("span_tokens", &["1", "2", "3", "4", "5"]),
    (
        "position",
        &[
            "first",
            "last",
            "break_before",
            "break_after",
            "whole_query",
            "break_inside",
        ],
    ),
    (
        "neighbour",
        &[
            "prev_unit_word",
            "prev_number",
            "prev_street_type",
            "prev_alpha",
            "next_street_type",
            "next_number",
            "next_unit_word",
            "next_alpha",
        ],
    ),
    (
        "shape",
        &[
            "digits",
            "digits_letter",
            "letters_digits",
            "alpha",
            "mixed",
        ],
    ),
    (
        "script",
        &["latin", "greek", "cyrillic", "semitic", "cjk", "other"],
    ),
    ("length", &["1", "2", "3", "4-5", "6-8", "9+"]),
    ("digits", &["1-2", "3", "4", "5", "6+"]),
    (
        "number_position",
        &["first", "last", "after_unit_word", "before_number"],
    ),
    (
        "vocabulary",
        &[
            "street_type",
            "unit_word",
            "street_type_suffix",
            "unknown_word",
            "unspaced",
        ],
    ),
    ("dict_distance", &FIELD_BY_DISTANCE),
    ("dict_tokens", &FIELD_BY_TOKENS),
    ("dict_frequency", &FIELD_BY_FREQUENCY),
    ("dict_short", &FIELD_BY_SHORT),
    ("dict_in_country", &DICT_FIELDS),
    ("joined", &["number_parts", "hyphen_before", "hyphen_after"]),
    ("postcode_any", &["1", "2", "3"]),
    ("postcode_country", &["1", "2", "3"]),
    ("dict_cover", &DICT_COVER),
    ("phrase", &PHRASE_KINDS),
    ("phrase_before", &PHRASE_KINDS),
    ("phrase_after", &PHRASE_KINDS),
    ("chain", &["2", "3", "4+", "with_region"]),
    (
        "dict_level",
        &["district", "locality", "neighbourhood", "place"],
    ),
    ("glued_phrase", &["unit", "po_box", "level", "admin_type"]),
    (
        "grounding",
        &["no_locality", "no_region", "no_country", "no_postcode"],
    ),
];

/// The kinds of vocabulary phrase, in the order of
/// [`Field::PHRASES`](super::dictionary::Field::PHRASES).
const PHRASE_KINDS: [&str; 7] = [
    "unit",
    "po_box",
    "level",
    "building",
    "street_type",
    "directional",
    "near",
];

const DICT_COVER: [&str; 10] = [
    "locality:begin",
    "locality:inside",
    "region:begin",
    "region:inside",
    "street:begin",
    "street:inside",
    "country:begin",
    "country:inside",
    "poi:begin",
    "poi:inside",
];

const DICT_FIELDS: [&str; 5] = ["locality", "region", "street", "country", "poi"];
const FIELD_BY_DISTANCE: [&str; 15] = [
    "locality:0",
    "locality:1",
    "locality:2",
    "region:0",
    "region:1",
    "region:2",
    "street:0",
    "street:1",
    "street:2",
    "country:0",
    "country:1",
    "country:2",
    "poi:0",
    "poi:1",
    "poi:2",
];
const FIELD_BY_TOKENS: [&str; 25] = [
    "locality:1",
    "locality:2",
    "locality:3",
    "locality:4",
    "locality:5",
    "region:1",
    "region:2",
    "region:3",
    "region:4",
    "region:5",
    "street:1",
    "street:2",
    "street:3",
    "street:4",
    "street:5",
    "country:1",
    "country:2",
    "country:3",
    "country:4",
    "country:5",
    "poi:1",
    "poi:2",
    "poi:3",
    "poi:4",
    "poi:5",
];
const FIELD_BY_FREQUENCY: [&str; 30] = [
    "locality:1",
    "locality:2-9",
    "locality:10-99",
    "locality:100-999",
    "locality:1000-9999",
    "locality:10000+",
    "region:1",
    "region:2-9",
    "region:10-99",
    "region:100-999",
    "region:1000-9999",
    "region:10000+",
    "street:1",
    "street:2-9",
    "street:10-99",
    "street:100-999",
    "street:1000-9999",
    "street:10000+",
    "country:1",
    "country:2-9",
    "country:10-99",
    "country:100-999",
    "country:1000-9999",
    "country:10000+",
    "poi:1",
    "poi:2-9",
    "poi:10-99",
    "poi:100-999",
    "poi:1000-9999",
    "poi:10000+",
];
const FIELD_BY_SHORT: [&str; 10] = [
    "locality:1-2",
    "locality:3",
    "region:1-2",
    "region:3",
    "street:1-2",
    "street:3",
    "country:1-2",
    "country:3",
    "poi:1-2",
    "poi:3",
];

#[derive(Debug, Clone, Copy)]
#[repr(usize)]
enum Block {
    Bias,
    SpanTokens,
    Position,
    Neighbour,
    Shape,
    Script,
    Length,
    Digits,
    NumberPosition,
    Vocabulary,
    DictDistance,
    DictTokens,
    DictFrequency,
    DictShort,
    DictInCountry,
    Joined,
    PostcodeAny,
    PostcodeCountry,
    DictCover,
    Phrase,
    PhraseBefore,
    PhraseAfter,
    Chain,
    DictLevel,
    GluedPhrase,
    Grounding,
}

const fn offset(block: Block) -> u32 {
    let mut offset = 0;
    let mut index = 0;
    while index < block as usize {
        offset += BLOCKS[index].1.len();
        index += 1;
    }
    offset as u32
}

/// Feature ids of the named blocks.
const NAMED: usize = offset(Block::Grounding) as usize + 4;
/// The lengths of the hashed affixes.
const AFFIX_SIZES: [usize; 3] = [2, 3, 4];
/// Buckets of each affix block: one per side and length.
pub(super) const AFFIX_BUCKETS: usize = 1_024;
const AFFIX_BLOCKS: usize = 2 * AFFIX_SIZES.len();
/// The first id of the word block.
const WORDS: usize = NAMED + AFFIX_BLOCKS * AFFIX_BUCKETS;
/// Buckets of the word block.
pub(super) const WORD_BUCKETS: usize = 1 << 15;
/// Feature ids, without labels.
pub(super) const COUNT: usize = WORDS + WORD_BUCKETS;

/// The feature of a one-unit span that lies inside a longer name of `field`
/// the dictionaries know, at its first unit or after it. A run of
/// characters labelled one at a time ("千", "代", "田", "区") still sees the
/// name it is part of.
pub(super) fn cover(field: Field, begins: bool) -> Option<u32> {
    let index = field_index(field)?;
    Some(id(Block::DictCover, index * 2 + usize::from(!begins)))
}

/// The id of the first dict_cover feature.
pub(super) const fn cover_base() -> u32 {
    offset(Block::DictCover)
}

/// The feature of a chain of `chain.pieces` area names.
pub(super) fn chain_feature(chain: super::Chain) -> u32 {
    id(Block::Chain, chain.pieces.clamp(2, 4) - 2)
}

/// The feature of a chain one of whose names is a region.
pub(super) fn chain_region() -> u32 {
    id(Block::Chain, 3)
}

/// Say what the Pack does not explain `span` as, of the closed-class
/// labels. A locality, region or country is a place the Pack knows and a
/// postcode is a pattern some country uses, so a span is one only when the
/// Pack explains it: a name of that field, exactly or within a few edits (a
/// locality also as a chain of area names), or a postcode pattern match.
/// `grounding` features mark what the Pack does not explain, and their
/// weights, one per label, say how unlikely each label is there.
pub(super) fn ground(span: &mut Span) {
    let distance = |field: Field| {
        let index = field_index(field).expect("a name field") as u32;
        let first = offset(Block::DictDistance) + 3 * index;
        first..first + 3
    };
    let chains = offset(Block::Chain)..offset(Block::Chain) + 3;
    let postcodes = offset(Block::PostcodeAny)..offset(Block::PostcodeAny) + 3;
    let has =
        |range: std::ops::Range<u32>| span.features.iter().any(|feature| range.contains(feature));
    let explained = [
        has(distance(Field::Locality)) || has(chains),
        has(distance(Field::Region)),
        has(distance(Field::Country)),
        has(postcodes),
    ];
    for (index, explained) in explained.into_iter().enumerate() {
        if !explained {
            span.features.push(id(Block::Grounding, index));
        }
    }
}

fn field_index(field: Field) -> Option<usize> {
    match field {
        Field::Locality => Some(0),
        Field::Region => Some(1),
        Field::Street => Some(2),
        Field::Country => Some(3),
        Field::Poi => Some(4),
        _ => None,
    }
}

fn id(block: Block, index: usize) -> u32 {
    debug_assert!(index < BLOCKS[block as usize].1.len());
    offset(block) + index as u32
}

/// `block:feature`, for weight dumps and the schema fingerprint.
pub(super) fn name(id: u32) -> String {
    if let Some(bucket) = (id as usize).checked_sub(WORDS) {
        return if bucket < WORD_BUCKETS {
            format!("word:{bucket}")
        } else {
            format!("unknown:{id}")
        };
    }
    if let Some(affix) = (id as usize).checked_sub(NAMED) {
        let (block, bucket) = (affix / AFFIX_BUCKETS, affix % AFFIX_BUCKETS);
        if block < AFFIX_BLOCKS {
            let side = if block < AFFIX_SIZES.len() {
                "prefix"
            } else {
                "suffix"
            };
            let size = AFFIX_SIZES[block % AFFIX_SIZES.len()];
            return format!("affix:{side}{size}:{bucket}");
        }
        return format!("unknown:{id}");
    }
    let mut first = 0;
    for (block, names) in BLOCKS {
        let index = id as usize - first;
        if index < names.len() {
            return format!("{block}:{}", names[index]);
        }
        first += names.len();
    }
    format!("unknown:{id}")
}

/// A fingerprint of the feature layout, so weights learned for another
/// layout are refused rather than misread.
pub(super) fn schema() -> u64 {
    let names = (0..COUNT as u32).map(name).collect::<Vec<_>>().join("\n");
    xxhash_rust::xxh3::xxh3_64(names.as_bytes())
}

/// The shape of a token's characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Shape {
    /// "670"
    Digits,
    /// "221b"
    DigitsLetter,
    /// "g120", "fl36"
    LettersDigits,
    /// "king"
    Alpha,
    /// "m5v", "1st"
    Mixed,
}

/// What one token is, computed once per query.
#[derive(Debug, Clone)]
pub(super) struct TokenInfo {
    pub shape: Shape,
    pub script: Option<usize>,
    pub chars: usize,
    pub has_digit: bool,
    pub street_type: bool,
    pub unit_word: bool,
    pub street_type_suffix: bool,
    /// Whether the word is in any dictionary name, so it is not a typo.
    pub known_word: bool,
    /// Whether the word is of a script written without spaces (see
    /// [`super::is_unspaced`]).
    pub unspaced: bool,
}

impl TokenInfo {
    pub fn new(word: &str, dictionaries: &Dictionaries) -> Self {
        let chars = word.chars().count();
        // A numeral of any script counts as a digit (see [`super::is_numeral`]).
        let digits = word
            .chars()
            .filter(|character| super::is_numeral(*character))
            .count();
        let leading_digits = word
            .chars()
            .take_while(|character| super::is_numeral(*character))
            .count();
        let leading_letters = word
            .chars()
            .take_while(|character| character.is_alphabetic() && !super::is_numeral(*character))
            .count();
        let shape = if digits == chars {
            Shape::Digits
        } else if digits == 0 {
            Shape::Alpha
        } else if leading_digits == digits && leading_digits + 1 == chars {
            Shape::DigitsLetter
        } else if leading_letters > 0 && leading_letters + digits == chars {
            Shape::LettersDigits
        } else {
            Shape::Mixed
        };
        Self {
            shape,
            script: word
                .chars()
                .find(|character| character.is_alphabetic())
                .map(script),
            chars,
            has_digit: digits > 0,
            street_type: dictionaries.contains(Field::StreetType, word),
            unit_word: dictionaries.contains(Field::Unit, word),
            street_type_suffix: shape == Shape::Alpha && has_street_type_suffix(word, dictionaries),
            known_word: dictionaries.contains(Field::Word, word)
                || dictionaries.contains(Field::StreetType, word)
                || dictionaries.contains(Field::Unit, word),
            unspaced: word.chars().any(super::is_unspaced),
        }
    }

    /// Digits, maybe with one letter after them: how house numbers look.
    pub fn is_number(&self) -> bool {
        matches!(self.shape, Shape::Digits | Shape::DigitsLetter)
    }

    fn is_alpha(&self) -> bool {
        self.shape == Shape::Alpha
    }
}

/// Whether the word ends with a street type of at least three letters after
/// a stem of at least three: "maximilianstr", "bahnhofstrasse".
fn has_street_type_suffix(word: &str, dictionaries: &Dictionaries) -> bool {
    let boundaries = word
        .char_indices()
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let chars = boundaries.len();
    (3..chars.saturating_sub(2))
        .any(|start| dictionaries.contains(Field::StreetType, &word[boundaries[start]..]))
}

/// The script block of a letter, by its Unicode Script property.
fn script(character: char) -> usize {
    use unicode_script::{Script, UnicodeScript};
    match character.script() {
        Script::Latin => 0,
        Script::Greek => 1,
        Script::Cyrillic => 2,
        Script::Hebrew | Script::Arabic | Script::Syriac => 3,
        Script::Han | Script::Hiragana | Script::Katakana | Script::Hangul => 4,
        _ => 5,
    }
}

fn frequency_bucket(count: u64) -> usize {
    match count {
        0..=1 => 0,
        2..=9 => 1,
        10..=99 => 2,
        100..=999 => 3,
        1_000..=9_999 => 4,
        _ => 5,
    }
}

/// The features of one span, collected while the lattice is built.
pub(super) struct FeatureSet {
    start: usize,
    end: usize,
    features: Vec<u32>,
    country_features: Vec<(CountryCode, u32)>,
}

impl FeatureSet {
    /// What every span has: its length, where it sits, its neighbours, and
    /// for one token its shape.
    pub fn new(tokens: &Tokens, infos: &[TokenInfo], start: usize, end: usize) -> Self {
        let mut set = Self {
            start,
            end,
            features: Vec::with_capacity(24),
            country_features: Vec::new(),
        };
        let len = tokens.len();
        set.push(Block::Bias, 0);
        for word in &tokens.words[start..end] {
            let bucket = xxhash_rust::xxh3::xxh3_64(word.as_bytes()) % WORD_BUCKETS as u64;
            set.features.push((WORDS + bucket as usize) as u32);
        }
        set.push(Block::SpanTokens, set.length_bucket());
        let position = [
            start == 0,
            end == len,
            tokens.breaks[start],
            end < len && tokens.breaks[end],
            start == 0 && end == len,
            (start + 1..end).any(|index| tokens.breaks[index]),
        ];
        set.flags(Block::Position, &position);
        let prev = start.checked_sub(1).map(|index| &infos[index]);
        let next = infos.get(end);
        let neighbour = [
            prev.is_some_and(|info| info.unit_word),
            prev.is_some_and(TokenInfo::is_number),
            prev.is_some_and(|info| info.street_type),
            prev.is_some_and(TokenInfo::is_alpha),
            next.is_some_and(|info| info.street_type),
            next.is_some_and(TokenInfo::is_number),
            next.is_some_and(|info| info.unit_word),
            next.is_some_and(TokenInfo::is_alpha),
        ];
        set.flags(Block::Neighbour, &neighbour);
        if end - start == 1 {
            set.token(&infos[start], position, neighbour);
            let hyphen = |index: usize| tokens.joiners.get(index) == Some(&Some('-'));
            set.flags(Block::Joined, &[false, hyphen(start), hyphen(end)]);
        }
        set
    }

    fn token(&mut self, info: &TokenInfo, position: [bool; 6], neighbour: [bool; 8]) {
        self.push(Block::Shape, info.shape as usize);
        if let Some(script) = info.script {
            self.push(Block::Script, script);
        }
        let length = match info.chars {
            0..=1 => 0,
            2 => 1,
            3 => 2,
            4..=5 => 3,
            6..=8 => 4,
            _ => 5,
        };
        self.push(Block::Length, length);
        if info.shape == Shape::Digits {
            let digits = match info.chars {
                0..=2 => 0,
                3 => 1,
                4 => 2,
                5 => 3,
                _ => 4,
            };
            self.push(Block::Digits, digits);
        }
        if info.is_number() {
            self.flags(
                Block::NumberPosition,
                &[position[0], position[1], neighbour[0], neighbour[5]],
            );
        }
        self.flags(
            Block::Vocabulary,
            &[
                info.street_type,
                info.unit_word,
                info.street_type_suffix,
                !info.known_word && info.shape == Shape::Alpha,
                info.unspaced,
            ],
        );
    }

    /// The span is a name in `field`, `hit.distance` edits away. `key_chars`
    /// is the length of the span as one key.
    pub fn dictionary_hit(&mut self, field: Field, hit: &Match, key_chars: usize) {
        let Some(field_index) = field_index(field) else {
            return;
        };
        self.push(
            Block::DictDistance,
            field_index * 3 + usize::from(hit.distance.min(2)),
        );
        self.push(Block::DictTokens, field_index * 5 + self.length_bucket());
        self.push(
            Block::DictFrequency,
            field_index * 6 + frequency_bucket(hit.entry.count),
        );
        match key_chars {
            0..=2 => self.push(Block::DictShort, field_index * 2),
            3 => self.push(Block::DictShort, field_index * 2 + 1),
            _ => {}
        }
        if let Some(country) = hit.entry.country {
            self.country_features
                .push((country, id(Block::DictInCountry, field_index)));
        }
        // Which admin level a locality name names: a district and a city
        // are both in the locality dictionary, and only this tells them apart.
        if field == Field::Locality {
            for (index, bit) in [
                level::DISTRICT,
                level::LOCALITY,
                level::NEIGHBOURHOOD,
                level::PLACE,
            ]
            .into_iter()
            .enumerate()
            {
                if hit.entry.levels & bit != 0 {
                    self.push(Block::DictLevel, index);
                }
            }
        }
    }

    /// The prefixes and suffixes of `word`, the span as one word, each of
    /// [`AFFIX_SIZES`] characters and shorter than the word, hashed into
    /// their blocks.
    pub fn affixes(&mut self, word: &str) {
        let bounds = word
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(word.len()))
            .collect::<Vec<_>>();
        let chars = bounds.len() - 1;
        for (index, size) in AFFIX_SIZES.into_iter().enumerate() {
            if chars <= size {
                break;
            }
            let prefix = &word[..bounds[size]];
            let suffix = &word[bounds[chars - size]..];
            for (side, affix) in [(0, prefix), (1, suffix)] {
                let block = side * AFFIX_SIZES.len() + index;
                let bucket = xxhash_rust::xxh3::xxh3_64_with_seed(affix.as_bytes(), block as u64)
                    % AFFIX_BUCKETS as u64;
                self.features
                    .push((NAMED + block * AFFIX_BUCKETS + bucket as usize) as u32);
            }
        }
    }

    /// The span is a vocabulary phrase of the kinds of `kinds`, one bit per
    /// field of [`Field::PHRASES`]: "po box", "私書箱", "floor".
    pub fn phrase(&mut self, kinds: u8) {
        self.phrase_bits(Block::Phrase, kinds);
    }

    /// A phrase of the kinds of `before` ends right before the span, and one
    /// of the kinds of `after` starts right after it: the number of "po box
    /// 29" follows a PO box phrase.
    pub fn phrase_neighbours(&mut self, before: u8, after: u8) {
        self.phrase_bits(Block::PhraseBefore, before);
        self.phrase_bits(Block::PhraseAfter, after);
    }

    fn phrase_bits(&mut self, block: Block, kinds: u8) {
        for kind in (0..PHRASE_KINDS.len()).filter(|kind| kinds & (1 << kind) != 0) {
            self.push(block, kind);
        }
    }

    /// The span is `pieces` area names glued one after the other, one of
    /// them a region when `region`.
    pub fn chain(&mut self, pieces: usize, region: bool) {
        self.features
            .push(chain_feature(super::Chain { pieces, region }));
        if region {
            self.features.push(chain_region());
        }
    }

    /// The units the span covers.
    pub fn range(&self) -> (usize, usize) {
        (self.start, self.end)
    }

    /// The span is one token that glues a number to a unit, PO box or level
    /// phrase or an admin type word ("4og", "apt4", "4chome").
    pub fn glued_phrase(&mut self, glued: super::Glued) {
        let index = match glued {
            super::Glued::Unit => 0,
            super::Glued::PoBox => 1,
            super::Glued::Level => 2,
            super::Glued::AdminType => 3,
        };
        self.push(Block::GluedPhrase, index);
    }

    /// The span is one number written in parts: "993.5", "1384 1/2".
    pub fn number_in_parts(&mut self) {
        self.push(Block::Joined, 0);
    }

    /// Some country's postcode pattern matches the span.
    pub fn postcode_any(&mut self) {
        self.push(Block::PostcodeAny, self.end - self.start - 1);
    }

    /// `country`'s postcode pattern matches the span.
    pub fn postcode_in(&mut self, country: CountryCode) {
        self.country_features.push((
            country,
            id(Block::PostcodeCountry, self.end - self.start - 1),
        ));
    }

    /// The span's length in units, the last bucket also holding the longer
    /// spans of a script written without spaces.
    fn length_bucket(&self) -> usize {
        (self.end - self.start).min(MAX_SPAN_TOKENS) - 1
    }

    pub fn into_span(self) -> Span {
        Span {
            start: self.start,
            end: self.end,
            features: self.features,
            country_features: self.country_features,
        }
    }

    fn push(&mut self, block: Block, index: usize) {
        self.features.push(id(block, index));
    }

    fn flags(&mut self, block: Block, flags: &[bool]) {
        for (index, flag) in flags.iter().enumerate() {
            if *flag {
                self.push(block, index);
            }
        }
    }
}

/// Feature ids by name, for hand-set weights and tests.
pub(super) fn by_name(name: &str) -> u32 {
    (0..NAMED as u32)
        .find(|id| self::name(*id) == name)
        .unwrap_or_else(|| panic!("no feature {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_feature_has_a_unique_name() {
        let names = (0..COUNT as u32).map(name).collect::<Vec<_>>();
        let unique = names.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), COUNT);
        assert_eq!(name(0), "bias:bias");
        assert_eq!(name(NAMED as u32 - 1), "grounding:no_postcode");
        assert_eq!(name(NAMED as u32 - 4), "grounding:no_locality");
        assert_eq!(name(NAMED as u32), "affix:prefix2:0");
        assert_eq!(name(WORDS as u32 - 1), "affix:suffix4:1023");
        assert_eq!(name(WORDS as u32), "word:0");
        assert_eq!(name(COUNT as u32 - 1), "word:32767");
        assert_eq!(
            by_name("dict_distance:street:1"),
            id(Block::DictDistance, 7)
        );
    }

    #[test]
    fn affixes_hash_the_starts_and_ends_shorter_than_the_word() {
        let features = |word: &str| {
            let tokens = Tokens::from_words(&[word]);
            let infos = [TokenInfo::new(word, &Dictionaries::empty())];
            let mut set = FeatureSet::new(&tokens, &infos, 0, 1);
            let before = set.features.len();
            set.affixes(word);
            set.features[before..]
                .iter()
                .map(|id| name(*id))
                .collect::<Vec<_>>()
        };
        let names = features("bahnhofstrasse");
        assert_eq!(names.len(), 6);
        for (index, block) in [
            "prefix2", "suffix2", "prefix3", "suffix3", "prefix4", "suffix4",
        ]
        .into_iter()
        .enumerate()
        {
            assert!(
                names[index].starts_with(&format!("affix:{block}:")),
                "{names:?}"
            );
        }
        // The same ending hashes alike in another word.
        assert_eq!(features("hauptstrasse")[5], names[5]);
        assert_eq!(features("abc").len(), 2, "only affixes shorter than it");
        assert!(features("ab").is_empty());
        assert_eq!(features("みなとみらい").len(), 6);
    }

    #[test]
    fn shapes_tell_numbers_from_units_and_words() {
        let dictionaries = Dictionaries::empty();
        let shape = |word| TokenInfo::new(word, &dictionaries).shape;
        assert_eq!(shape("670"), Shape::Digits);
        assert_eq!(shape("221b"), Shape::DigitsLetter);
        assert_eq!(shape("fl36"), Shape::LettersDigits);
        assert_eq!(shape("king"), Shape::Alpha);
        assert_eq!(shape("münchen"), Shape::Alpha);
        assert_eq!(shape("m5v"), Shape::Mixed);
        assert_eq!(shape("1st"), Shape::Mixed);
        assert_eq!(TokenInfo::new("улица", &dictionaries).script, Some(2));
        // Numerals of any script are digits.
        assert_eq!(shape("１２"), Shape::Digits);
        assert_eq!(shape("八"), Shape::Digits);
        assert_eq!(shape("٣"), Shape::Digits);
        assert_eq!(shape("丁"), Shape::Alpha);
    }
}
