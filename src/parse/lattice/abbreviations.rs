//! Street-type and direction abbreviations of every language libpostal lists,
//! read both ways: a query's "str" is also looked up as "straße", and
//! training writes a name's "straße" as "str" some of the time.
//!
//! The phrases are libpostal's street types and directionals (MIT, see
//! `NOTICE`), each line a phrase and its `|`-separated variants. The languages that
//! glue a street type onto the name ("hauptstraße", "kerkstraat") are those
//! libpostal lists concatenated suffixes for, and in them a name ending in a
//! variant also ends in each other form of it ("hauptstr" is "hauptstraße").
//! Nothing here names a language: every list is read the same way.

use std::{
    collections::{BTreeSet, HashMap},
    sync::OnceLock,
};

use crate::text_index::normalize_index_text;

const STREET_TYPES: &str = include_str!("data/street_types.txt");
const DIRECTIONALS: &str = include_str!("data/directionals.txt");
const CONCATENATED_SUFFIXES: &str = include_str!("data/concatenated_suffixes.txt");

/// Spellings of one key looked up besides the key, at most.
const MAX_SPELLINGS: usize = 8;
/// A glued street type is only read off a word whose stem keeps at least
/// this many characters ("hauptstr" but not "estr").
const MIN_GLUED_STEM_CHARS: usize = 3;

/// The abbreviations of every language.
#[derive(Debug, Default)]
pub(super) struct Abbreviations {
    /// A variant to its full forms ("str" to "straße").
    full: HashMap<String, Vec<String>>,
    /// A full form to its variants ("straße" to "str", "strasse").
    short: HashMap<String, Vec<String>>,
    /// The forms of the street types of languages that glue them onto the
    /// name, longest first, each with the other forms of its phrase.
    glued: Vec<(String, Vec<String>)>,
}

/// The abbreviations, read once.
pub(super) fn abbreviations() -> &'static Abbreviations {
    static TABLE: OnceLock<Abbreviations> = OnceLock::new();
    TABLE.get_or_init(|| {
        let gluing = languages(CONCATENATED_SUFFIXES);
        let mut table = Abbreviations::default();
        let mut glued = HashMap::<String, BTreeSet<String>>::new();
        for (list, glues) in [(STREET_TYPES, true), (DIRECTIONALS, false)] {
            for (language, forms) in phrases(list) {
                let [full, variants @ ..] = &forms[..] else {
                    continue;
                };
                for variant in variants.iter().filter(|variant| *variant != full) {
                    push(&mut table.full, variant, full);
                    push(&mut table.short, full, variant);
                }
                if glues && gluing.contains(language) {
                    for form in &forms {
                        glued
                            .entry(form.clone())
                            .or_default()
                            .extend(forms.iter().filter(|other| *other != form).cloned());
                    }
                }
            }
        }
        table.glued = glued
            .into_iter()
            .map(|(form, others)| (form, others.into_iter().collect()))
            .collect();
        table
            .glued
            .sort_by(|left, right| right.0.len().cmp(&left.0.len()).then(left.0.cmp(&right.0)));
        table
    })
}

impl Abbreviations {
    /// Other spellings of a dictionary key with its street types and
    /// directions written in full, or a glued street type in its other
    /// forms, at most [`MAX_SPELLINGS`]; the key itself is not one of them.
    pub(super) fn spellings(&self, key: &str) -> Vec<String> {
        let words = key.split(' ').collect::<Vec<_>>();
        let mut spellings = vec![String::new()];
        for (index, word) in words.iter().enumerate() {
            let mut forms = vec![(*word).to_string()];
            forms.extend(self.full.get(*word).into_iter().flatten().cloned());
            forms.extend(self.unglued(word));
            let mut next = Vec::new();
            for spelling in &spellings {
                for form in &forms {
                    if next.len() > MAX_SPELLINGS {
                        break;
                    }
                    let mut spelling = spelling.clone();
                    if index > 0 {
                        spelling.push(' ');
                    }
                    spelling.push_str(form);
                    next.push(spelling);
                }
            }
            spellings = next;
        }
        // A street type written apart from a name it glues onto in its
        // language ("lechwehr strasse" of "lechwehrstraße").
        if let [head @ .., stem, form] = &words[..]
            && self.glued.iter().any(|(glued, _)| glued == form)
        {
            for other in
                std::iter::once(*form).chain(self.glued_forms(form).iter().map(String::as_str))
            {
                let mut glued = head.join(" ");
                if !glued.is_empty() {
                    glued.push(' ');
                }
                glued.push_str(stem);
                glued.push_str(other);
                spellings.push(glued);
            }
        }
        spellings.retain(|spelling| spelling != key);
        spellings.dedup();
        spellings.truncate(MAX_SPELLINGS);
        spellings
    }

    /// The variants a full form is abbreviated to, none when it has none.
    pub(super) fn short_forms(&self, word: &str) -> &[String] {
        self.short.get(word).map_or(&[], Vec::as_slice)
    }

    /// A word ending in a glued street type, with that type in each of its
    /// other forms ("hauptstr" to "hauptstraße" and "hauptstrasse").
    pub(super) fn unglued(&self, word: &str) -> Vec<String> {
        let Some((stem, form)) = self.split_glued(word) else {
            return Vec::new();
        };
        self.glued_forms(form)
            .iter()
            .map(|other| format!("{stem}{other}"))
            .collect()
    }

    /// A word ending in a glued street type, as its stem and that type
    /// ("hauptstraße" as "haupt" and "straße").
    pub(super) fn split_glued<'w>(&self, word: &'w str) -> Option<(&'w str, &'w str)> {
        let (form, _) = self.glued.iter().find(|(form, _)| {
            word.len() > form.len()
                && word.ends_with(form.as_str())
                && word[..word.len() - form.len()].chars().count() >= MIN_GLUED_STEM_CHARS
        })?;
        Some(word.split_at(word.len() - form.len()))
    }

    /// The other forms of a street type that glues onto a name, none when it
    /// does not.
    fn glued_forms(&self, form: &str) -> &[String] {
        self.glued
            .iter()
            .find(|(glued, _)| glued == form)
            .map_or(&[], |(_, others)| others.as_slice())
    }
}

fn push(map: &mut HashMap<String, Vec<String>>, key: &str, value: &str) {
    let values = map.entry(key.to_string()).or_default();
    if !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
    }
}

/// The languages a list has phrases of.
fn languages(list: &str) -> BTreeSet<&str> {
    list.lines()
        .filter_map(|line| line.strip_prefix("# "))
        .filter(|language| !language.contains(' '))
        .collect()
}

/// Each phrase of a list with its language, as its one-word forms
/// normalized like a dictionary key, the full form first. Phrases of
/// several words are left out: they are written as words of their own.
fn phrases(list: &str) -> Vec<(&str, Vec<String>)> {
    let mut language = "";
    let mut out = Vec::new();
    for line in list.lines() {
        if let Some(name) = line.strip_prefix("# ") {
            language = name;
            continue;
        }
        let forms = line
            .split('|')
            .filter_map(normalize_index_text)
            .filter(|form| !form.contains(' '))
            .collect::<Vec<_>>();
        if !forms.is_empty() {
            out.push((language, forms));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviations_are_read_in_full_in_every_language() {
        let table = abbreviations();
        assert!(
            table
                .spellings("hermann vom endt str")
                .contains(&"hermann vom endt straße".to_string())
        );
        assert!(
            table
                .spellings("king st")
                .contains(&"king street".to_string())
        );
        assert!(table.spellings("toronto").is_empty());
        assert!(table.spellings("a b c d e f st str").len() <= MAX_SPELLINGS);
    }

    #[test]
    fn a_glued_street_type_is_read_in_its_other_forms() {
        let table = abbreviations();
        let unglued = table.unglued("hauptstr");
        assert!(unglued.contains(&"hauptstraße".to_string()), "{unglued:?}");
        assert!(table.unglued("str").is_empty());
    }

    #[test]
    fn a_street_type_written_apart_is_also_read_glued() {
        let table = abbreviations();
        let spellings = table.spellings("lechwehr strasse");
        assert!(
            spellings.contains(&"lechwehrstraße".to_string()),
            "{spellings:?}"
        );
        assert_eq!(table.split_glued("hauptstraße"), Some(("haupt", "straße")));
    }

    #[test]
    fn full_forms_have_their_abbreviations() {
        let table = abbreviations();
        assert!(table.short_forms("straße").contains(&"str".to_string()));
        assert!(table.short_forms("toronto").is_empty());
    }
}
