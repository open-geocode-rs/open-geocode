use std::{
    collections::{BTreeSet, HashMap},
    sync::OnceLock,
};

use crate::util::fold::search_key;

const STREET_TYPES: &str = include_str!("data/street_types.txt");
const DIRECTIONALS: &str = include_str!("data/directionals.txt");
const CONCATENATED_SUFFIXES: &str = include_str!("data/concatenated_suffixes.txt");

const MAX_SPELLINGS: usize = 8;
const MIN_GLUED_STEM_CHARS: usize = 3;

#[derive(Debug, Default)]
pub(super) struct Abbreviations {
    full: HashMap<String, Vec<String>>,
    short: HashMap<String, Vec<String>>,
    glued: Vec<(String, Vec<String>)>,
}

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

    pub(super) fn full_forms(&self, word: &str) -> &[String] {
        self.full.get(word).map_or(&[], Vec::as_slice)
    }

    pub(super) fn short_forms(&self, word: &str) -> &[String] {
        self.short.get(word).map_or(&[], Vec::as_slice)
    }

    pub(super) fn unglued(&self, word: &str) -> Vec<String> {
        let Some((stem, form)) = self.split_glued(word) else {
            return Vec::new();
        };
        self.glued_forms(form)
            .iter()
            .map(|other| format!("{stem}{other}"))
            .collect()
    }

    pub(super) fn split_glued<'w>(&self, word: &'w str) -> Option<(&'w str, &'w str)> {
        let (form, _) = self.glued.iter().find(|(form, _)| {
            word.len() > form.len()
                && word.ends_with(form.as_str())
                && word[..word.len() - form.len()].chars().count() >= MIN_GLUED_STEM_CHARS
        })?;
        Some(word.split_at(word.len() - form.len()))
    }

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

fn languages(list: &str) -> BTreeSet<&str> {
    list.lines()
        .filter_map(|line| line.strip_prefix("# "))
        .filter(|language| !language.contains(' '))
        .collect()
}

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
            .filter_map(search_key)
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
                .contains(&"hermann vom endt strasse".to_string())
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
        assert!(unglued.contains(&"hauptstrasse".to_string()), "{unglued:?}");
        assert!(table.unglued("str").is_empty());
    }

    #[test]
    fn a_street_type_written_apart_is_also_read_glued() {
        let table = abbreviations();
        let spellings = table.spellings("lechwehr strasse");
        assert!(
            spellings.contains(&"lechwehrstrasse".to_string()),
            "{spellings:?}"
        );
        assert_eq!(
            table.split_glued("hauptstrasse"),
            Some(("haupt", "strasse"))
        );
    }
}
