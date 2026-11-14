//! Words that name a kind of place, in every language the catalog has, and
//! the place categories each one names.
//!
//! `catalog/categories.json` is generated (see `catalog/catalog.py`). A
//! category is returned as the words a POI's category is indexed under, such
//! as `amenity pharmacy`, or the key alone (`shop`) for a preset of any value.
//! A word is matched by its [`search_key`], so "Apotheke" and "apotheke" are
//! one word, and a phrase ("drug store") is one word of the table.
//!
//! Words come in three strengths, and a weaker one answers only when no
//! stronger one matches. Strongest are a preset's whole name, unless the name
//! is a regional variant's (en-AU), and every group word. Next are single
//! words of a name ("coffee" of "Coffee Shop"), which can sit in a narrower
//! preset's name ("park" of "Park and Ride Lot"). Weakest are terms and
//! aliases, which reach wider ("coffee" is a restaurant term).

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
    /// Language, then preset name, then preset indexes.
    names: BTreeMap<String, BTreeMap<String, Vec<usize>>>,
    /// Language, then term or alias, then preset indexes.
    terms: BTreeMap<String, BTreeMap<String, Vec<usize>>>,
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
    strong: HashMap<String, Box<[usize]>>,
    name_words: HashMap<String, Box<[usize]>>,
    weak: HashMap<String, Box<[usize]>>,
    /// Strong words with a minimum prefix length, sorted by word for a range
    /// scan from a typed prefix.
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

    let mut strong = HashMap::<String, BTreeSet<usize>>::new();
    let mut name_words = HashMap::<String, BTreeSet<usize>>::new();
    let mut weak = HashMap::<String, BTreeSet<usize>>::new();
    let add = |table: &mut HashMap<String, BTreeSet<usize>>, key: &str, presets: &[usize]| {
        table
            .entry(key.to_string())
            .or_default()
            .extend(presets.iter().map(|&preset| of_preset[preset]));
    };
    for (language, names) in &catalog.names {
        // en-AU names the chemist "Pharmacy"; a regional name must not outrank the language's own.
        let regional = language
            .split_once('-')
            .is_some_and(|(base, _)| catalog.names.contains_key(base));
        for (name, presets) in names {
            let Some(key) = search_key(name) else {
                continue;
            };
            let whole = if regional {
                &mut name_words
            } else {
                &mut strong
            };
            add(whole, &key, presets);
            for word in key.split(' ') {
                add(&mut name_words, word, presets);
            }
        }
    }
    for (term, presets) in catalog.terms.values().flatten() {
        if let Some(key) = search_key(term) {
            add(&mut weak, &key, presets);
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
            strong.entry(key.clone()).or_default().extend(members);
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
    let frozen = |table: HashMap<String, BTreeSet<usize>>| {
        table
            .into_iter()
            .map(|(word, set)| (word, set.into_iter().collect()))
            .collect()
    };
    Table {
        categories,
        strong: frozen(strong),
        name_words: frozen(name_words),
        weak: frozen(weak),
        prefixed,
    }
}

/// The categories a whole word or phrase names, sorted, from its strongest
/// matching words.
pub fn presets_for_word(word: &str) -> Vec<&'static str> {
    search_key(word).map_or_else(Vec::new, |key| for_key(&key, false))
}

/// The categories a typed prefix names, sorted. It names those of every word
/// it spells out in full, and of every group word whose minimum prefix length
/// it reaches. A word with no minimum length matches only in full. Weaker
/// words answer only when none of these match.
pub fn presets_for_prefix(prefix: &str) -> Vec<&'static str> {
    search_key(prefix).map_or_else(Vec::new, |key| for_key(&key, true))
}

/// The categories a whole query names, by the rules of [`presets_for_word`],
/// and for a single word also of [`presets_for_prefix`]. `None` when it names
/// none.
pub fn of_query(query: &str) -> Option<Vec<&'static str>> {
    let key = search_key(query)?;
    // "park" typed whole is a park, not yet the start of "parking".
    let mut found = for_key(&key, false);
    if found.is_empty() && !key.contains(' ') {
        found = for_key(&key, true);
    }
    (!found.is_empty()).then_some(found)
}

/// Whether a record's category (`amenity:pharmacy`) is one of `categories`,
/// where a bare key (`shop`) holds every value of that key.
pub fn holds(categories: &[&str], category: &str) -> bool {
    let (key, value) = category.split_once(':').unwrap_or((category, ""));
    categories.iter().any(|named| match named.split_once(' ') {
        Some((named_key, named_value)) => named_key == key && named_value == value,
        None => *named == key,
    })
}

fn for_key(key: &str, prefix: bool) -> Vec<&'static str> {
    let table = table();
    let full = table.strong.get(key).into_iter().flatten().copied();
    let reached = prefix.then(|| {
        let typed = key.chars().count();
        let start = table
            .prefixed
            .partition_point(|entry| entry.word.as_str() < key);
        table.prefixed[start..]
            .iter()
            .take_while(|entry| entry.word.starts_with(key))
            .filter(move |entry| typed >= entry.min_chars)
            .flat_map(|entry| entry.categories.iter().copied())
    });
    let strong = names(table, full.chain(reached.into_iter().flatten()));
    if !strong.is_empty() {
        return strong;
    }
    let in_names = names(
        table,
        table.name_words.get(key).into_iter().flatten().copied(),
    );
    if !in_names.is_empty() {
        return in_names;
    }
    names(table, table.weak.get(key).into_iter().flatten().copied())
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
    fn name_words_outrank_wider_terms() {
        // Restaurant terms hold "coffee", parking terms hold "park", chemist terms hold "pharmacy".
        let coffee = presets_for_word("coffee");
        assert!(coffee.contains(&"amenity cafe"), "{coffee:?}");
        assert!(!coffee.contains(&"amenity restaurant"), "{coffee:?}");
        let park = presets_for_word("park");
        assert!(park.contains(&"leisure park"), "{park:?}");
        assert!(!park.contains(&"amenity parking"), "{park:?}");
        let pharmacy = presets_for_word("pharmacy");
        assert!(pharmacy.contains(&"amenity pharmacy"), "{pharmacy:?}");
        assert!(!pharmacy.contains(&"shop chemist"), "{pharmacy:?}");
    }

    #[test]
    fn terms_answer_when_no_name_matches() {
        // "Drugstore" names the chemist; "apothecary" is only a term of both.
        assert_eq!(presets_for_word("drugstore"), vec!["shop chemist"]);
        assert_eq!(presets_for_word("chemist"), vec!["shop chemist"]);
        assert_eq!(
            presets_for_word("apothecary"),
            vec!["amenity pharmacy", "shop chemist"]
        );
        assert_eq!(
            presets_for_prefix("apothecary"),
            vec!["amenity pharmacy", "shop chemist"]
        );
    }

    #[test]
    fn query_uses_prefixes_only_for_one_word() {
        assert_eq!(of_query("phar"), Some(vec!["amenity pharmacy"]));
        assert_eq!(of_query("Coffee"), Some(presets_for_word("coffee")));
        assert_eq!(of_query("drug store"), Some(presets_for_word("drug store")));
        assert_eq!(of_query("phar store"), None);
        assert_eq!(of_query(""), None);
        assert_eq!(of_query("zzzqqq"), None);
    }

    #[test]
    fn holds_matches_value_or_any_value_of_a_bare_key() {
        assert!(holds(&["amenity pharmacy"], "amenity:pharmacy"));
        assert!(!holds(&["amenity pharmacy"], "shop:chemist"));
        assert!(!holds(&["amenity pharmacy"], "amenity:pharmacy_counter"));
        assert!(holds(&["shop"], "shop:chemist"));
        assert!(!holds(&["shop"], "amenity:shop"));
        assert!(!holds(&[], "shop:chemist"));
    }

    #[test]
    fn unknown_and_empty_words_name_nothing() {
        assert_eq!(presets_for_word("zzzqqq"), Vec::<&str>::new());
        assert_eq!(presets_for_word(""), Vec::<&str>::new());
        assert_eq!(presets_for_prefix(""), Vec::<&str>::new());
    }

    #[test]
    fn a_whole_word_beats_the_start_of_another() {
        let park = of_query("park").expect("park");
        assert!(park.contains(&"leisure park"));
        assert!(!park.contains(&"amenity parking"));
        assert_eq!(of_query("parking"), Some(vec!["amenity parking"]));
    }
}
