//! Words that name a kind of place, in every language the catalog has, and
//! the place categories each one names.
//!
//! `catalog/categories.json` is generated (see `catalog/catalog.py`). A
//! category is returned as the words a POI's category is indexed under, such
//! as `amenity pharmacy`, or the key alone (`shop`) for a preset of any value.
//! A word is matched by its [`search_key`], so "Apotheke" and "apotheke" are
//! one word, and a phrase ("drug store") is one word of the table.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::OnceLock,
};

use serde::{Deserialize, de::IgnoredAny};

use crate::util::fold::search_key;

#[derive(Deserialize)]
struct Catalog {
    /// `[preset id, category, group indexes]`.
    presets: Vec<(IgnoredAny, String, Vec<usize>)>,
    /// `[group id, words by language]`.
    groups: Vec<(IgnoredAny, BTreeMap<String, Vec<GroupWord>>)>,
    /// Language, then word, then preset indexes.
    words: BTreeMap<String, BTreeMap<String, Vec<usize>>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GroupWord {
    Whole(String),
    /// Typed this far, the word already names the group.
    Prefix(String, usize),
}

struct Table {
    /// Sorted, so an index order is the categories' order.
    categories: Vec<String>,
    whole: HashMap<String, Box<[usize]>>,
    /// Sorted by word, for a range scan from a typed prefix.
    prefixed: Vec<Prefixed>,
}

struct Prefixed {
    word: String,
    min_chars: usize,
    categories: Box<[usize]>,
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        let catalog: Catalog = serde_json::from_str(include_str!("../catalog/categories.json"))
            .expect("catalog/categories.json is a category catalog");
        build(&catalog)
    })
}

fn build(catalog: &Catalog) -> Table {
    let categories: Vec<String> = catalog
        .presets
        .iter()
        .map(|(_, category, _)| category.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let position = |category: &str| {
        categories
            .binary_search_by(|known| known.as_str().cmp(category))
            .expect("every preset category is in the list")
    };
    let of_preset: Vec<usize> = catalog
        .presets
        .iter()
        .map(|(_, category, _)| position(category))
        .collect();
    let mut members = vec![BTreeSet::new(); catalog.groups.len()];
    for (preset, (_, _, groups)) in catalog.presets.iter().enumerate() {
        for &group in groups {
            members[group].insert(of_preset[preset]);
        }
    }

    let mut whole = HashMap::<String, BTreeSet<usize>>::new();
    for words in catalog.words.values() {
        for (word, presets) in words {
            if let Some(key) = search_key(word) {
                whole
                    .entry(key)
                    .or_default()
                    .extend(presets.iter().map(|&preset| of_preset[preset]));
            }
        }
    }
    let mut prefixed = Vec::new();
    for ((_, words), members) in catalog.groups.iter().zip(&members) {
        for word in words.values().flatten() {
            let (word, min_chars) = match word {
                GroupWord::Whole(word) => (word, None),
                GroupWord::Prefix(word, min_chars) => (word, Some(*min_chars)),
            };
            let Some(key) = search_key(word) else {
                continue;
            };
            whole.entry(key.clone()).or_default().extend(members);
            if let Some(min_chars) = min_chars {
                prefixed.push(Prefixed {
                    word: key,
                    min_chars,
                    categories: members.iter().copied().collect(),
                });
            }
        }
    }
    prefixed.sort_by(|a, b| a.word.cmp(&b.word));
    Table {
        categories,
        whole: whole
            .into_iter()
            .map(|(word, set)| (word, set.into_iter().collect()))
            .collect(),
        prefixed,
    }
}

/// The categories a whole word or phrase names, sorted.
pub fn presets_for_word(word: &str) -> Vec<&'static str> {
    let table = table();
    let Some(key) = search_key(word) else {
        return Vec::new();
    };
    names(table, table.whole.get(&key).into_iter().flatten().copied())
}

/// The categories a typed prefix names, sorted. It names those of every word
/// it spells out in full, and of every word whose minimum prefix length it
/// reaches. A word with no minimum length matches only in full.
pub fn presets_for_prefix(prefix: &str) -> Vec<&'static str> {
    let table = table();
    let Some(key) = search_key(prefix) else {
        return Vec::new();
    };
    let typed = key.chars().count();
    let start = table
        .prefixed
        .partition_point(|entry| entry.word.as_str() < key.as_str());
    let reached = table.prefixed[start..]
        .iter()
        .take_while(|entry| entry.word.starts_with(&key))
        .filter(|entry| typed >= entry.min_chars)
        .flat_map(|entry| entry.categories.iter().copied());
    let full = table.whole.get(&key).into_iter().flatten().copied();
    names(table, full.chain(reached))
}

fn names(table: &'static Table, indexes: impl Iterator<Item = usize>) -> Vec<&'static str> {
    indexes
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|index| table.categories[index].as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pharmacy_words_name_pharmacies_in_each_language() {
        for word in ["pharmacy", "apotheek", "Apotheke"] {
            assert!(
                presets_for_word(word).contains(&"amenity pharmacy"),
                "{word}: {:?}",
                presets_for_word(word)
            );
        }
    }

    #[test]
    fn group_word_names_every_member() {
        let food = presets_for_word("food");
        assert!(food.contains(&"amenity restaurant"), "{food:?}");
        assert!(food.contains(&"amenity fast_food"), "{food:?}");
    }

    #[test]
    fn prefix_reaching_a_minimum_length_names_the_group() {
        // "3Pharmacy" lets three typed letters suggest pharmacies.
        assert_eq!(presets_for_prefix("phar"), vec!["amenity pharmacy"]);
        assert_eq!(presets_for_prefix("pha"), vec!["amenity pharmacy"]);
        assert_eq!(presets_for_word("phar"), Vec::<&str>::new());
        // "4Groceries" needs four letters, not three.
        assert!(presets_for_prefix("groc").contains(&"shop supermarket"));
        assert_eq!(presets_for_prefix("gro"), Vec::<&str>::new());
    }

    #[test]
    fn word_without_a_minimum_length_matches_only_in_full() {
        assert_eq!(presets_for_prefix("restauran"), Vec::<&str>::new());
        assert!(presets_for_prefix("restaurant").contains(&"amenity restaurant"));
        assert!(presets_for_prefix("Restaurant").contains(&"amenity restaurant"));
    }

    #[test]
    fn unknown_and_empty_words_name_nothing() {
        assert_eq!(presets_for_word("zzzqqq"), Vec::<&str>::new());
        assert_eq!(presets_for_word(""), Vec::<&str>::new());
        assert_eq!(presets_for_prefix(""), Vec::<&str>::new());
    }
}
