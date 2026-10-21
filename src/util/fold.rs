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
fn split(value: &str, key: Key, mut piece: impl FnMut(Piece<'_>)) {
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
        if !character.is_alphanumeric() {
            if !word.is_empty() {
                piece(Piece::Word(&word));
                word.clear();
            }
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
    if !word.is_empty() {
        piece(Piece::Word(&word));
    }
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

#[cfg(test)]
mod smoke {
    use super::*;
    use crate::{
        pack::Writer,
        record::{ObjectType, PlaceLayer, PlaceRecord, Record, SourceProvenance, point_geometry},
        search::{TextSearchOptions, TextSearcher},
    };

    #[test]
    fn letters_without_a_mark_fold_to_ascii() {
        assert_eq!(search_key("Łódź").as_deref(), Some("lodz"));
        assert_eq!(search_key("Straße").as_deref(), Some("strasse"));
        assert_eq!(search_key("Ørsted").as_deref(), Some("orsted"));
        assert_eq!(
            search_key("Æbeltoft Đakovo Işıklar Þingvellir").as_deref(),
            Some("aebeltoft dakovo isiklar thingvellir")
        );
        assert_eq!(search_key("Hawaiʻi").as_deref(), Some("hawaii"));
        assert_eq!(exact_key("Łódź").as_deref(), Some("łódź"));
    }

    #[test]
    fn a_decomposed_accent_is_one_word() {
        let decomposed = "Wa\u{304}naka";
        assert_eq!(search_key(decomposed).as_deref(), Some("wanaka"));
        assert_eq!(exact_key(decomposed).as_deref(), Some("wānaka"));
        assert_eq!(exact_key(decomposed), exact_key("Wānaka"));
        // A mark with no precomposed letter, and a virama, stay in their word.
        assert_eq!(exact_key("m\u{303}a").as_deref(), Some("m\u{303}a"));
        assert_eq!(search_key("हिन्दी"), Some("हिन्दी".to_string()));
    }

    #[test]
    fn fullwidth_input_is_ascii() {
        assert_eq!(
            search_key("Ｍａｉｎ　Ｓｔ　１２").as_deref(),
            Some("main st 12")
        );
        assert_eq!(search_key("４丁目").as_deref(), Some("4丁目"));
        assert_eq!(search_key("ｶﾞｰﾃﾞﾝ").as_deref(), Some("ガーデン"));
    }

    fn place(name: &str, object_id: i64) -> Record {
        Record::Place(
            PlaceLayer::Locality,
            PlaceRecord {
                name: name.to_string(),
                alt_names: Vec::new(),
                place_type: "town".to_string(),
                geometry: point_geometry(175.0, -38.7),
                source: SourceProvenance::osm(ObjectType::Node, object_id),
            },
        )
    }

    fn searcher(name: &str, places: &[(&str, i64)]) -> (std::path::PathBuf, TextSearcher) {
        let dir =
            std::env::temp_dir().join(format!("open-geocode-fold-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut writer = Writer::create(&dir).expect("writer");
        for (place_name, id) in places {
            writer.write(&place(place_name, *id), None).expect("write");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&dir).expect("searcher");
        (dir, searcher)
    }

    fn labels(searcher: &TextSearcher, query: &str) -> Vec<String> {
        searcher
            .search(TextSearchOptions {
                query: query.to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
            })
            .expect("search")
            .into_iter()
            .map(|hit| hit.record.label)
            .collect()
    }

    #[test]
    fn a_plain_query_finds_the_name_written_with_its_letters() {
        let (dir, searcher) = searcher(
            "plain",
            &[
                ("Łódź", 1),
                ("Lange Straße", 2),
                ("Ørsted", 3),
                ("Wānaka", 4),
            ],
        );
        assert_eq!(
            labels(&searcher, "Lodz").first().map(String::as_str),
            Some("Łódź")
        );
        assert_eq!(
            labels(&searcher, "Łódź").first().map(String::as_str),
            Some("Łódź")
        );
        assert_eq!(
            labels(&searcher, "Lange Strasse")
                .first()
                .map(String::as_str),
            Some("Lange Straße")
        );
        assert_eq!(
            labels(&searcher, "Orsted").first().map(String::as_str),
            Some("Ørsted")
        );
        assert_eq!(
            labels(&searcher, "Wa\u{304}naka")
                .first()
                .map(String::as_str),
            Some("Wānaka")
        );
        assert_eq!(
            labels(&searcher, "Ｌｏｄｚ").first().map(String::as_str),
            Some("Łódź")
        );
        drop(searcher);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_exact_spelling_wins_the_tie() {
        // The plain record has the lower id, which wins every other tie.
        let (dir, searcher) = searcher("exact", &[("Taupo", 1), ("Taupō", 2)]);
        assert_eq!(labels(&searcher, "Taupō"), ["Taupō", "Taupo"]);
        assert_eq!(labels(&searcher, "Taupo"), ["Taupo", "Taupō"]);
        assert_eq!(labels(&searcher, "TAUPŌ"), ["Taupō", "Taupo"]);
        drop(searcher);
        let _ = std::fs::remove_dir_all(dir);
    }
}
