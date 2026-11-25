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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Piece<'a> {
    Word(&'a str),
    Gap(char),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Search,
    Exact,
}

pub(crate) fn search_key(value: &str) -> Option<String> {
    join(value, Key::Search)
}

pub(crate) fn exact_key(value: &str) -> Option<String> {
    join(value, Key::Exact)
}

pub(crate) fn search_pieces(value: &str, piece: impl FnMut(Piece<'_>)) {
    split(value, Key::Search, piece);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) key: String,
    pub(crate) start: usize,
    ends: Vec<usize>,
}

impl Span {
    pub(crate) fn end(&self) -> usize {
        self.end_of(self.ends.len())
    }

    pub(crate) fn end_of(&self, chars: usize) -> usize {
        match chars.min(self.ends.len()).checked_sub(1) {
            Some(last) => self.ends[last],
            None => self.start,
        }
    }
}

pub(crate) fn search_spans(value: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut open = None::<Span>;
    let mut close = |open: &mut Option<Span>| {
        spans.extend(open.take().filter(|span| {
            span.key
                .chars()
                .any(|character| character != char::REPLACEMENT_CHARACTER)
        }));
    };
    let mut characters = value.char_indices().peekable();
    let mut at = 0;
    while let Some((from, first)) = characters.next() {
        let start = at;
        at += 1;
        let mut to = from + first.len_utf8();
        while let Some(&(next, mark)) = characters.peek()
            && is_mark(mark)
        {
            characters.next();
            at += 1;
            to = next + mark.len_utf8();
        }
        let add = |open: &mut Option<Span>, folded: &str| {
            let span = open.get_or_insert_with(|| Span {
                key: String::new(),
                start,
                ends: Vec::new(),
            });
            span.key.push_str(folded);
            span.ends.extend(folded.chars().map(|_| at));
        };
        if first.is_ascii() && to == from + 1 {
            match first.is_ascii_alphanumeric() {
                true => add(
                    &mut open,
                    first.to_ascii_lowercase().encode_utf8(&mut [0; 4]),
                ),
                false => close(&mut open),
            }
        } else if first == char::REPLACEMENT_CHARACTER {
            add(&mut open, first.encode_utf8(&mut [0; 4]));
        } else {
            split(&value[from..to], Key::Search, |piece| match piece {
                Piece::Word(word) => add(&mut open, word),
                Piece::Gap(_) => close(&mut open),
            });
        }
    }
    close(&mut open);
    spans
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

fn split(value: &str, key: Key, piece: impl FnMut(Piece<'_>)) {
    if value.is_ascii() {
        split_ascii(value, piece);
    } else {
        split_unicode(value, key, piece);
    }
}

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
    let mut latin = false;
    for character in lower.chars() {
        if !word.is_empty() && is_mark(character) {
            if !(key == Key::Search && latin) {
                word.push(character);
            }
            continue;
        }
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

pub(crate) fn replaced_characters(word: &str) -> usize {
    word.chars()
        .filter(|character| *character == char::REPLACEMENT_CHARACTER)
        .count()
}

fn is_mark(character: char) -> bool {
    GeneralCategoryGroup::Mark.contains(CodePointMapData::<GeneralCategory>::new().get(character))
}

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
    fn spans_are_the_words_of_the_search_key_where_the_text_writes_them() {
        for value in [
            "",
            "  Main--STREET (rear) ",
            "12A King St.",
            "Palais de l'Élysée",
            "Łódź, Straße 5",
            "wa\u{304}naka",
            "Ch\u{fffd}teauneuf \u{fffd}\u{fffd} de Gadagne",
            "\u{fffd}cole",
            "ﬁn １２ ½",
            "東京都 渋谷",
            "ʻokina hawaiʻi",
            " \u{301}x",
        ] {
            let keys = search_spans(value)
                .into_iter()
                .map(|span| span.key)
                .collect::<Vec<_>>();
            assert_eq!(
                (!keys.is_empty()).then(|| keys.join(" ")),
                search_key(value),
                "{value:?}"
            );
        }
        let cut = |value: &str| {
            search_spans(value)
                .into_iter()
                .map(|span| (span.key.clone(), span.start, span.end()))
                .collect::<Vec<_>>()
        };
        let word = |key: &str, start, end| (key.to_string(), start, end);
        assert_eq!(
            cut("Palais de l'Élysée"),
            [
                word("palais", 0, 6),
                word("de", 7, 9),
                word("l", 10, 11),
                word("elysee", 12, 18),
            ]
        );
        assert_eq!(
            cut("Été, Straße 5"),
            [word("ete", 0, 3), word("strasse", 5, 11), word("5", 12, 13)]
        );
        assert_eq!(cut("wa\u{304}naka"), [word("wanaka", 0, 7)]);
        let strasse = &search_spans("Straße 5")[0];
        assert_eq!(
            [0, 1, 4, 5, 6, 7, 99].map(|chars| strasse.end_of(chars)),
            [0, 1, 4, 5, 5, 6, 6]
        );
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
