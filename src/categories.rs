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
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
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

/// The categories a whole query names when it is a type's whole name or a
/// group word, typed out. A word inside a name ("for", "van") or a search
/// term names a kind of place in some language but is more often typed as a
/// name, so it does not take over the answer. `None` when it names none.
pub fn of_query(query: &str) -> Option<Vec<&'static str>> {
    let table = table();
    let key = search_key(query)?;
    let found = names(table, table.strong.get(&key).into_iter().flatten().copied());
    (!found.is_empty()).then_some(found)
}

/// A query that asks for a kind of place, and where when it names a place
/// too ("pharmacy amsterdam", "hotel near pike place").
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub categories: Vec<&'static str>,
    /// The words of the place, connectors ("near", "in de buurt van") left
    /// out; `None` for the caller's own surroundings.
    pub place: Option<String>,
}

/// The kind of place a query asks for: all of it, or its longest first or
/// last words with a place in the rest.
pub fn of_request(query: &str) -> Option<Request> {
    let key = search_key(query)?;
    if let Some(categories) = of_query(&key) {
        return Some(Request {
            categories,
            place: None,
        });
    }
    let tokens = key.split_whitespace().collect::<Vec<_>>();
    for size in (1..tokens.len()).rev() {
        let splits = [
            (&tokens[..size], &tokens[size..]),
            (
                &tokens[tokens.len() - size..],
                &tokens[..tokens.len() - size],
            ),
        ];
        for (kind, rest) in splits {
            let Some(categories) = of_query(&kind.join(" ")) else {
                continue;
            };
            let place = without_connectors(rest);
            return Some(Request {
                categories,
                place: (!place.is_empty()).then(|| place.join(" ")),
            });
        }
    }
    None
}

/// `words` without the "near" words around them, longest first.
fn without_connectors<'a>(mut words: &'a [&'a str]) -> &'a [&'a str] {
    let connectors = connectors();
    let longest = connectors
        .iter()
        .map(|connector| connector.split(' ').count())
        .max();
    let longest = longest.unwrap_or(0);
    let mut cut = true;
    while cut {
        cut = false;
        for size in (1..=longest.min(words.len())).rev() {
            if connectors.contains(&words[..size].join(" ")) {
                words = &words[size..];
                cut = true;
                break;
            }
            if connectors.contains(&words[words.len() - size..].join(" ")) {
                words = &words[..words.len() - size];
                cut = true;
                break;
            }
        }
    }
    words
}

/// Words that place what a query asks for ("near", "bij", "in"), every
/// language the parser knows.
fn connectors() -> &'static HashSet<String> {
    static CONNECTORS: OnceLock<HashSet<String>> = OnceLock::new();
    CONNECTORS.get_or_init(|| {
        include_str!("parse/lattice/data/near.txt")
            .lines()
            .filter(|line| !line.starts_with('#'))
            .flat_map(|line| line.split('|'))
            .filter_map(search_key)
            .collect()
    })
}

/// Every whole name and group word, for a typo of one.
pub fn strong_words() -> impl Iterator<Item = &'static str> {
    table().strong.keys().map(String::as_str)
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
    fn query_takes_whole_names_and_group_words_only() {
        assert_eq!(of_query("Pharmacy"), Some(vec!["amenity pharmacy"]));
        assert_eq!(of_query("phar"), None);
        assert_eq!(of_query("for"), None);
        assert_eq!(
            of_query("gas station"),
            Some(presets_for_word("gas station"))
        );
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

    #[test]
    fn a_request_splits_the_kind_from_the_place() {
        let request = |query: &str| of_request(query).map(|request| request.place);
        assert_eq!(request("pharmacy"), Some(None));
        assert_eq!(
            request("pharmacy amsterdam"),
            Some(Some("amsterdam".to_string()))
        );
        assert_eq!(
            request("amsterdam pharmacy"),
            Some(Some("amsterdam".to_string()))
        );
        assert_eq!(
            request("hotel near pike place"),
            Some(Some("pike place".to_string()))
        );
        assert_eq!(
            request("apotheek in de buurt van dam"),
            Some(Some("dam".to_string()))
        );
        assert_eq!(request("pharmacy near me"), Some(None));
        assert_eq!(request("starbucks seattle"), None);
    }
}
