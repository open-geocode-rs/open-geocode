//! The keys names and queries are matched by.
//!
//! A search key is what retrieval and the parser's dictionaries match on:
//! NFKC (fullwidth "１" is "1", "ﬁ" is "fi"), lower case, and Latin letters
//! written in ASCII by CLDR's Latin-ASCII transform ("łódź" is "lodz",
//! "straße" is "strasse", "ørsted" is "orsted"), so a query typed without a
//! letter's accent, stroke or ligature finds the name. Decomposing and
//! dropping marks alone would leave `ł`, `ø`, `ß`, `æ`, `đ` and `ı` as they
//! are: they carry no mark. An exact key keeps the accents (NFC, lower case),
//! so a query that spells a name exactly can rank it above a namesake
//! spelled plainer ("Taupō" over "Taupo").
//!
//! Both keys cut words in [`split`], the one place text becomes words: index
//! text, queries and dictionary keys alike.

use std::{collections::HashMap, sync::OnceLock};

use icu_normalizer::ComposingNormalizerBorrowed;
use icu_properties::{
    CodePointMapData,
    props::{GeneralCategory, GeneralCategoryGroup},
};
use unicode_script::{Script, UnicodeScript};

/// `character<TAB>replacement` rows of CLDR's Latin-ASCII transform for
/// lower-case letters (Unicode License V3, see `NOTICE`), vendored. A
/// replacement may be empty: a modifier letter such as the ʻokina of
/// "hawaiʻi" is dropped, not cut into a gap.
const LATIN_ASCII: &str = include_str!("../parse/lattice/data/latin_ascii.tsv");

/// What [`split`] cuts text into, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Piece<'a> {
    Word(&'a str),
    /// A character between words, normalized as the words are.
    Gap(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Search,
    Exact,
}

/// `value` as retrieval and the parser match it: its words, NFKC, lower case
/// and Latin-ASCII folded, joined by single spaces. `None` when it has no
/// word.
pub(crate) fn search_key(value: &str) -> Option<String> {
    join(value, Key::Search)
}

/// `value` as written, to tell exact spellings apart among the records a
/// search key found: its words, NFC and lower case, joined by single spaces.
/// `None` when it has no word.
pub(crate) fn exact_key(value: &str) -> Option<String> {
    join(value, Key::Exact)
}

/// The pieces of `value` whose words joined by single spaces are its
/// [`search_key`].
pub(crate) fn search_pieces(value: &str, piece: impl FnMut(Piece<'_>)) {
    split(value, Key::Search, piece);
}

fn join(value: &str, key: Key) -> Option<String> {
    let mut joined = String::with_capacity(value.len());
    split(value, key, |piece| {
        if let Piece::Word(word) = piece {
            if !joined.is_empty() {
                joined.push(' ');
            }
            joined.push_str(word);
        }
    });
    (!joined.is_empty()).then_some(joined)
}

/// Words are runs of letters and digits, and a mark continues the word
/// before it. Composed first, so a decomposed "wa\u{304}naka" is one word,
/// and a mark with no precomposed letter ("m\u{303}", a virama) still does not
/// cut its word in two.
fn split(value: &str, key: Key, piece: impl FnMut(Piece<'_>)) {
    if value.is_ascii() {
        split_ascii(value, piece);
    } else {
        split_unicode(value, key, piece);
    }
}

/// [`split_unicode`] for ASCII text, which is most of what a build folds:
/// ASCII is its own NFKC and NFC, has no marks, and needs no Latin-ASCII
/// fold, so both keys come to its lower-case alphanumeric runs.
fn split_ascii(value: &str, mut piece: impl FnMut(Piece<'_>)) {
    let lower = value.to_ascii_lowercase();
    let mut start = 0;
    for (index, byte) in lower.bytes().enumerate() {
        if byte.is_ascii_alphanumeric() {
            continue;
        }
        if start < index {
            piece(Piece::Word(&lower[start..index]));
        }
        piece(Piece::Gap(char::from(byte)));
        start = index + 1;
    }
    if start < lower.len() {
        piece(Piece::Word(&lower[start..]));
    }
}

fn split_unicode(value: &str, key: Key, mut piece: impl FnMut(Piece<'_>)) {
    let normalized = match key {
        Key::Search => ComposingNormalizerBorrowed::new_nfkc().normalize(value),
        Key::Exact => ComposingNormalizerBorrowed::new_nfc().normalize(value),
    };
    // Composed again: a lower-case letter can decompose ("Ϊ\u{301}" is
    // "ϊ\u{301}", which composes to "ΐ"), and a key must be its own key.
    let lower = normalized
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let lower = ComposingNormalizerBorrowed::new_nfc().normalize(&lower);
    let mut word = String::new();
    // Whether the letter a mark would follow is Latin: Latin-ASCII drops the
    // accents of a Latin letter, and only those.
    let mut latin = false;
    for character in lower.chars() {
        if !word.is_empty() && is_mark(character) {
            if !(key == Key::Search && latin) {
                word.push(character);
            }
            continue;
        }
        // A replacement character stands for a letter a wrong encoding lost
        // ("Ch\u{fffd}teauneuf" of "Châteauneuf"), so it stays in its word, one
        // edit from the letter it replaced, for a typo tier to reach.
        if character == char::REPLACEMENT_CHARACTER {
            word.push(character);
            latin = true;
            continue;
        }
        if !character.is_alphanumeric() {
            flush(&mut word, &mut piece);
            piece(Piece::Gap(character));
            continue;
        }
        if key == Key::Exact || character.is_ascii() {
            word.push(character);
            latin = character.is_ascii_alphabetic();
        } else if let Some(ascii) = latin_ascii(character) {
            word.push_str(ascii);
            latin = true;
        } else {
            word.push(character);
            latin = character.script() == Script::Latin;
        }
    }
    flush(&mut word, &mut piece);
}

/// End the word being cut, if there is one. A run of replacement characters
/// alone spells nothing, so it is a gap.
fn flush(word: &mut String, piece: &mut impl FnMut(Piece<'_>)) {
    if word
        .chars()
        .all(|character| character == char::REPLACEMENT_CHARACTER)
    {
        for _ in word.chars() {
            piece(Piece::Gap(char::REPLACEMENT_CHARACTER));
        }
    } else {
        piece(Piece::Word(word));
    }
    word.clear();
}

/// How many characters of `word` a wrong encoding replaced: each is one
/// edit from the letter it stood for.
pub(crate) fn replaced_characters(word: &str) -> usize {
    word.chars()
        .filter(|character| *character == char::REPLACEMENT_CHARACTER)
        .count()
}

fn is_mark(character: char) -> bool {
    GeneralCategoryGroup::Mark.contains(CodePointMapData::<GeneralCategory>::new().get(character))
}

/// The ASCII a lower-case Latin letter is written with, when it is not ASCII
/// already.
fn latin_ascii(character: char) -> Option<&'static str> {
    static TABLE: OnceLock<HashMap<char, &'static str>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            LATIN_ASCII
                .lines()
                .filter(|line| !line.starts_with('#'))
                .filter_map(|line| {
                    let (character, ascii) = line.split_once('\t')?;
                    let mut chars = character.chars();
                    let character = chars.next().filter(|_| chars.next().is_none())?;
                    Some((character, ascii))
                })
                .collect()
        })
        .get(&character)
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_replacement_character_stays_in_its_word_and_alone_is_a_gap() {
        assert_eq!(
            search_key("Ch\u{fffd}teauneuf de Gadagne").as_deref(),
            Some("ch\u{fffd}teauneuf de gadagne")
        );
        assert_eq!(
            search_key("Main \u{fffd} Street").as_deref(),
            Some("main street")
        );
        assert_eq!(search_key("\u{fffd}\u{fffd}"), None);
        assert_eq!(replaced_characters("ari\u{fffd}ge"), 1);
    }

    #[test]
    fn the_ascii_path_cuts_as_the_unicode_path_does() {
        let pieces = |value: &str, ascii: bool| {
            let mut cut = Vec::new();
            let keep = |found: Piece<'_>| {
                cut.push(match found {
                    Piece::Word(word) => (Some(word.to_string()), ' '),
                    Piece::Gap(gap) => (None, gap),
                })
            };
            if ascii {
                split_ascii(value, keep);
            } else {
                split_unicode(value, Key::Search, keep);
            }
            cut
        };
        for value in [
            "",
            " ",
            "12A King St.",
            "  Main--STREET (rear) ",
            "A1/B2,C3;d4\te5\nf6",
            "O'Brien & Sons #7",
            "x",
            "!",
        ] {
            assert_eq!(pieces(value, true), pieces(value, false), "{value:?}");
        }
    }

    #[test]
    fn the_table_writes_lower_case_letters_and_digits() {
        let rows = LATIN_ASCII
            .lines()
            .filter(|line| !line.starts_with('#'))
            .collect::<Vec<_>>();
        assert!(rows.len() > 400);
        for row in rows {
            let (character, ascii) = row.split_once('\t').expect(row);
            assert_eq!(character.chars().count(), 1, "{row}");
            assert!(
                ascii
                    .chars()
                    .all(|c| c.is_alphanumeric() && !c.is_uppercase()),
                "{row}"
            );
            assert_eq!(latin_ascii(character.chars().next().unwrap()), Some(ascii));
        }
    }
}
