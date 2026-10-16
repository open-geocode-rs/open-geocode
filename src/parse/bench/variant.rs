//! The three forms each gold row is put to a parser in.
//!
//! A variant is a pure function of the gold row, so the fixture stays one
//! file per country and every system sees the same perturbed rows. Tokens
//! move with their labels, so the gold labels of a variant are the labels of
//! the row it returns.
//!
//! - [`Variant::AsIs`]: the row as the corpus wrote it, parts joined with
//!   commas.
//! - [`Variant::Stripped`]: the same tokens without the commas.
//! - [`Variant::Messy`]: no commas, the parts in a shuffled order, street-type
//!   words abbreviated or fused onto the word before them, and one typo. See
//!   [`messy`] for the exact rules.

use xxhash_rust::xxh3::xxh3_64_with_seed;

use super::fixture::TaggedRow;
use crate::{
    parse::types::{Label, Tagged},
    text_index::{STREET_TYPE_ABBREVIATIONS, is_street_type, normalize_index_text},
};

/// The seed every messy row's random draws derive from.
pub const SEED: u64 = 2026;

/// How a gold row's query is put to a system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Variant {
    AsIs,
    Stripped,
    Messy,
}

impl Variant {
    pub const ALL: [Variant; 3] = [Variant::AsIs, Variant::Stripped, Variant::Messy];

    /// The name in reports and file names: `jp.as-is.txt`, `jp.messy.tsv`.
    pub fn name(self) -> &'static str {
        match self {
            Variant::AsIs => "as-is",
            Variant::Stripped => "stripped",
            Variant::Messy => "messy",
        }
    }

    /// The row in this form: its query is [`TaggedRow::query`] and its gold
    /// labels are [`TaggedRow::labels`].
    pub fn apply(self, row: &TaggedRow) -> TaggedRow {
        match self {
            Variant::AsIs => row.clone(),
            Variant::Stripped => TaggedRow {
                breaks: Vec::new(),
                ..row.clone()
            },
            Variant::Messy => messy(row),
        }
    }
}

/// The messy form of a row. Its random draws come from a generator seeded by
/// [`SEED`] and the row's text, so the same row is always perturbed the same
/// way, wherever it sits in the file. In order:
///
/// 1. The parts (the runs of tokens between the row's commas) are shuffled
///    into a uniformly random order; the tokens of a part keep their order.
///    The commas are dropped.
/// 2. Each street-labelled token that is a street-type word is either
///    abbreviated or fused onto the token before it, on a fair coin. A word is
///    abbreviated to its usual abbreviation in the text index's table
///    ([`STREET_TYPE_ABBREVIATIONS`], read backwards: `street` becomes `st`),
///    and fused when the token before it in its part is also a street token
///    (`manosquer straße` becomes the single token `manosquerstraße`, labelled
///    `street`). When the coin's pick is impossible for the word the other is
///    used, and when neither is the word is left alone.
/// 3. Exactly one typo goes into one token of five or more characters, all of
///    them alphabetic, when the row has one: a substitution, a deletion or a
///    transposition of two adjacent characters, picked uniformly, at a uniform
///    position. A substitution draws a different letter from `a-z` for a Latin
///    letter, from `а-я` for a Cyrillic one, and otherwise from the token's
///    other characters. A transposition swaps two different characters; a
///    substitution or transposition with no such choice is a deletion.
pub fn messy(row: &TaggedRow) -> TaggedRow {
    let mut rng = Rng::new(xxh3_64_with_seed(row.to_line().as_bytes(), SEED));

    let mut parts = parts(row);
    rng.shuffle(&mut parts);

    let mut tokens = Vec::with_capacity(row.tokens.len());
    for part in parts {
        let start = tokens.len();
        for tagged in part {
            let fusable = tokens.len() > start
                && tokens
                    .last()
                    .is_some_and(|previous: &Tagged| previous.label == Label::Street);
            if tagged.label != Label::Street || !is_street_type_word(&tagged.token) {
                tokens.push(tagged);
                continue;
            }
            let abbreviation = abbreviation(&tagged.token);
            let abbreviate = rng.coin();
            match (abbreviate, abbreviation, fusable) {
                (true, Some(short), _) | (false, Some(short), false) => tokens.push(Tagged {
                    token: short.to_string(),
                    label: Label::Street,
                }),
                (_, _, true) => {
                    let previous = tokens.last_mut().expect("a fusable token follows one");
                    previous.token.push_str(&tagged.token);
                }
                (_, None, false) => tokens.push(tagged),
            }
        }
    }

    add_typo(&mut tokens, &mut rng);
    debug_assert!(
        tokens
            .iter()
            .all(|tagged| normalize_index_text(&tagged.token).as_deref() == Some(&tagged.token)),
        "a messy token is not normalized"
    );
    TaggedRow {
        lang: row.lang.clone(),
        country: row.country.clone(),
        tokens,
        breaks: Vec::new(),
    }
}

/// The row's parts: the runs of tokens between its part boundaries.
fn parts(row: &TaggedRow) -> Vec<Vec<Tagged>> {
    let mut parts = Vec::with_capacity(row.breaks.len() + 1);
    let mut start = 0;
    for &end in row.breaks.iter().chain(std::iter::once(&row.tokens.len())) {
        parts.push(row.tokens[start..end].to_vec());
        start = end;
    }
    parts
}

/// Street-type words beyond the text index's English table, for the
/// countries the benchmark covers, so their streets can be fused too. They
/// have no abbreviation in the table.
const OTHER_STREET_TYPES: &[&str] = &[
    // German.
    "straße",
    "strasse",
    "weg",
    "platz",
    "allee",
    "gasse",
    "ring",
    "damm",
    "ufer",
    "chaussee",
    // French.
    "rue",
    "chemin",
    "allée",
    "impasse",
    "route",
    "quai",
    "cours",
    // Russian.
    "улица",
    "проспект",
    "переулок",
    "шоссе",
    "бульвар",
    "площадь",
    "проезд",
    "набережная",
];

fn is_street_type_word(token: &str) -> bool {
    is_street_type(token) || abbreviation(token).is_some() || OTHER_STREET_TYPES.contains(&token)
}

/// A street-type word's usual abbreviation: the first the text index's table
/// expands to it.
pub fn abbreviation(word: &str) -> Option<&'static str> {
    STREET_TYPE_ABBREVIATIONS
        .iter()
        .find(|(_, full)| *full == word)
        .map(|(short, _)| *short)
}

fn add_typo(tokens: &mut [Tagged], rng: &mut Rng) {
    let eligible = tokens
        .iter()
        .enumerate()
        .filter(|(_, tagged)| {
            tagged.token.chars().count() >= 5 && tagged.token.chars().all(char::is_alphabetic)
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if eligible.is_empty() {
        return;
    }
    let token = &mut tokens[eligible[rng.below(eligible.len())]].token;
    let mut chars = token.chars().collect::<Vec<_>>();
    match rng.below(3) {
        0 => {
            let position = rng.below(chars.len());
            match substitute(&chars, chars[position], rng) {
                Some(replacement) => chars[position] = replacement,
                None => {
                    chars.remove(position);
                }
            }
        }
        1 => {
            chars.remove(rng.below(chars.len()));
        }
        _ => {
            let swappable = (0..chars.len() - 1)
                .filter(|&position| chars[position] != chars[position + 1])
                .collect::<Vec<_>>();
            if swappable.is_empty() {
                chars.remove(rng.below(chars.len()));
            } else {
                let position = swappable[rng.below(swappable.len())];
                chars.swap(position, position + 1);
            }
        }
    }
    *token = chars.into_iter().collect();
}

/// A different character to put in place of `original` in `chars`.
fn substitute(chars: &[char], original: char, rng: &mut Rng) -> Option<char> {
    let alphabet = if original.is_ascii_lowercase() {
        ('a'..='z').collect::<Vec<_>>()
    } else if ('а'..='я').contains(&original) {
        ('а'..='я').collect()
    } else {
        let mut own = chars.to_vec();
        own.sort_unstable();
        own.dedup();
        own
    };
    let choices = alphabet
        .into_iter()
        .filter(|&character| character != original)
        .collect::<Vec<_>>();
    (!choices.is_empty()).then(|| choices[rng.below(choices.len())])
}

/// SplitMix64: small, fast and fixed, so the variants never change with a
/// dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A uniform index in `0..bound`; `bound` must be positive.
    fn below(&mut self, bound: usize) -> usize {
        ((u128::from(self.next()) * bound as u128) >> 64) as usize
    }

    fn coin(&mut self) -> bool {
        self.next() >> 63 == 1
    }

    /// Fisher-Yates.
    fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            items.swap(index, self.below(index + 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(line: &str) -> TaggedRow {
        TaggedRow::parse_line(line).expect("row")
    }

    fn tokens(row: &TaggedRow) -> Vec<&str> {
        row.tokens
            .iter()
            .map(|tagged| tagged.token.as_str())
            .collect()
    }

    /// Edit distance of one substitution, deletion or adjacent swap.
    fn one_typo_apart(left: &str, right: &str) -> bool {
        let left = left.chars().collect::<Vec<_>>();
        let right = right.chars().collect::<Vec<_>>();
        if left.len() == right.len() {
            let diffs = (0..left.len())
                .filter(|&i| left[i] != right[i])
                .collect::<Vec<_>>();
            diffs.len() == 1
                || (diffs.len() == 2
                    && diffs[1] == diffs[0] + 1
                    && left[diffs[0]] == right[diffs[1]]
                    && left[diffs[1]] == right[diffs[0]])
        } else if left.len() == right.len() + 1 {
            (0..left.len()).any(|skip| {
                left.iter()
                    .enumerate()
                    .filter(|&(i, _)| i != skip)
                    .map(|(_, c)| *c)
                    .eq(right.iter().copied())
            })
        } else {
            false
        }
    }

    const LINE: &str = "de\tde\tmanosquer/street straße/street 12/house_number , 80331/postcode münchen/locality , deutschland/country";

    #[test]
    fn as_is_keeps_the_commas_and_stripped_drops_them() {
        let gold = row(LINE);
        assert_eq!(Variant::AsIs.apply(&gold), gold);
        let stripped = Variant::Stripped.apply(&gold);
        assert_eq!(stripped.tokens, gold.tokens);
        assert_eq!(
            stripped.query(),
            "manosquer straße 12 80331 münchen deutschland"
        );
    }

    #[test]
    fn messy_is_deterministic_and_comma_free() {
        let gold = row(LINE);
        let first = messy(&gold);
        assert_eq!(first, messy(&gold));
        assert!(first.breaks.is_empty());
        assert!(!first.query().contains(','));
        assert!(TaggedRow::parse_line(&first.to_line()).is_ok());
    }

    #[test]
    fn messy_shuffles_whole_parts_and_carries_labels() {
        // Without street types or long words, only the shuffle applies.
        let gold =
            row("en\tus\t1/house_number a/street , b/locality c/locality , d/region , e/country");
        let mut orders = std::collections::BTreeSet::new();
        for index in 0..64 {
            let mut variant = gold.clone();
            variant.lang = format!("en{index}");
            let shuffled = messy(&variant);
            let parts = shuffled
                .tokens
                .iter()
                .map(|tagged| tagged.token.as_str())
                .collect::<String>();
            for whole in ["1a", "bc", "d", "e"] {
                assert!(parts.contains(whole), "{parts}");
            }
            for tagged in &shuffled.tokens {
                let original = gold
                    .tokens
                    .iter()
                    .find(|gold| gold.token == tagged.token)
                    .expect("same tokens");
                assert_eq!(original.label, tagged.label);
            }
            orders.insert(parts);
        }
        assert!(orders.len() > 10, "{orders:?}");
    }

    #[test]
    fn street_types_are_abbreviated_or_fused_about_evenly() {
        let (mut abbreviated, mut fused) = (0, 0);
        for index in 0..400 {
            let gold = row(&format!("en{index}\tca\tking/street street/street"));
            let messy = messy(&gold);
            match tokens(&messy).as_slice() {
                ["king", "st"] => abbreviated += 1,
                ["kingstreet"] => fused += 1,
                // A typo hit the fused token.
                [one] if one_typo_apart("kingstreet", one) => fused += 1,
                other => panic!("{other:?}"),
            }
            assert!(
                messy
                    .tokens
                    .iter()
                    .all(|tagged| tagged.label == Label::Street)
            );
        }
        assert!((150..=250).contains(&abbreviated), "{abbreviated} {fused}");
    }

    #[test]
    fn a_street_type_with_one_form_possible_takes_it() {
        for index in 0..50 {
            // First in its part: "avenue" can only be abbreviated.
            let out = messy(&row(&format!("fr{index}\tfr\tavenue/street foch/street")));
            assert_eq!(tokens(&out), ["ave", "foch"]);
            // No abbreviation: "straße" can only be fused, and the fused
            // token is the row's one typo candidate.
            let out = messy(&row(&format!(
                "de{index}\tde\tmanosquer/street straße/street"
            )));
            assert_eq!(out.tokens.len(), 1);
            assert!(
                one_typo_apart("manosquerstraße", &out.tokens[0].token),
                "{out:?}"
            );
            // Neither: "rue" first and unabbreviated stays.
            let out = messy(&row(&format!("fr{index}\tfr\true/street x/street")));
            assert_eq!(tokens(&out), ["rue", "x"]);
        }
    }

    #[test]
    fn a_street_type_after_another_label_is_not_fused() {
        for index in 0..50 {
            let messy = messy(&row(&format!("de{index}\tde\t12/house_number weg/street")));
            assert_eq!(tokens(&messy), ["12", "weg"]);
        }
    }

    #[test]
    fn exactly_one_typo_in_one_long_alphabetic_token() {
        for index in 0..300 {
            let gold = row(&format!(
                "en{index}\tgb\tflat2/unit 10/house_number abcde/locality москва/region"
            ));
            let messy = messy(&gold);
            let mut changed = 0;
            for (before, after) in gold.tokens.iter().zip(&messy.tokens) {
                if before.token != after.token {
                    changed += 1;
                    assert!(
                        one_typo_apart(&before.token, &after.token),
                        "{} {}",
                        before.token,
                        after.token
                    );
                    assert!(["abcde", "москва"].contains(&before.token.as_str()));
                }
            }
            assert_eq!(changed, 1, "{:?}", tokens(&messy));
        }
    }

    #[test]
    fn rows_without_a_long_alphabetic_token_get_no_typo() {
        let gold =
            row("ja\tjp\t静/region 岡/region 県/region , 浜/locality 松/locality 市/locality");
        let messy = messy(&gold);
        let mut before = tokens(&gold);
        let mut after = tokens(&messy);
        before.sort_unstable();
        after.sort_unstable();
        assert_eq!(before, after);
    }

    #[test]
    fn substitutions_stay_in_the_script() {
        let mut rng = Rng::new(1);
        for _ in 0..100 {
            let latin = substitute(&['a', 'b'], 'a', &mut rng).expect("letter");
            assert!(latin.is_ascii_lowercase() && latin != 'a');
            let cyrillic = substitute(&['д'], 'д', &mut rng).expect("letter");
            assert!(('а'..='я').contains(&cyrillic) && cyrillic != 'д');
            let other = substitute(&['ü', 'ß', 'ü'], 'ü', &mut rng).expect("letter");
            assert_eq!(other, 'ß');
        }
        assert_eq!(substitute(&['ü', 'ü'], 'ü', &mut rng), None);
    }

    #[test]
    fn abbreviations_read_the_index_table_backwards() {
        assert_eq!(abbreviation("street"), Some("st"));
        assert_eq!(abbreviation("avenue"), Some("ave"));
        assert_eq!(abbreviation("trail"), Some("trl"));
        assert_eq!(abbreviation("straße"), None);
    }
}
