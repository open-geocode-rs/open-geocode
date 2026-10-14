use std::collections::BTreeMap;

use crate::util::{text::collapse_whitespace, transliterate::to_latin};

/// Convenience accessors over an OSM tag map. Replaces the per-module
/// `tag_value`/`has_*` free functions with one shared, testable surface.
pub(crate) trait OsmTags {
    /// Whitespace-cleaned value for `key`, or `None` if missing or blank.
    fn cleaned(&self, key: &str) -> Option<String>;

    /// Whether `key` has a non-blank value.
    fn has(&self, key: &str) -> bool {
        self.cleaned(key).is_some()
    }
}

impl OsmTags for BTreeMap<String, String> {
    fn cleaned(&self, key: &str) -> Option<String> {
        self.get(key).and_then(|value| collapse_whitespace(value))
    }
}

/// Keys whose values are other names of the feature, with or without a
/// `:<lang>` suffix.
const ALT_NAME_KEYS: [&str; 4] = ["alt_name", "official_name", "short_name", "old_name"];

/// Whether `key` holds a name of the feature: `name`, `name:<lang>`, or one
/// of [`ALT_NAME_KEYS`] with or without `:<lang>`. A language is a BCP 47
/// style tag (`ja`, `ja-Hira`, `zh-Hans`, `ja_rm`), so `name:etymology`,
/// `name:prefix` and `old_name:1897-2010` are not names.
pub(crate) fn is_name_key(key: &str) -> bool {
    if key == "name" {
        return true;
    }
    let (base, language) = match key.split_once(':') {
        Some((base, language)) => (base, Some(language)),
        None => (key, None),
    };
    let base_ok = (base == "name" && language.is_some()) || ALT_NAME_KEYS.contains(&base);
    base_ok && language.is_none_or(is_language_tag)
}

/// `ja`, `ja-Hira`, `ja_rm`, `zh-Hant-TW`: a two or three letter lower case
/// language, then subtags of letters and digits.
fn is_language_tag(value: &str) -> bool {
    let mut parts = value.split(['-', '_']);
    let language = parts.next().unwrap_or_default();
    (2..=3).contains(&language.len())
        && language.bytes().all(|byte| byte.is_ascii_lowercase())
        && parts.all(|part| {
            !part.is_empty()
                && part.len() <= 8
                && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
}

/// The names a feature has besides `primary`, in key order, `;`-separated
/// values split, then the Latin form of each name written in kana, Cyrillic
/// or Greek (see [`to_latin`]), `primary` first. Each is whitespace-cleaned,
/// and none repeats `primary` or an earlier one, ignoring case.
pub(crate) fn alternate_names(tags: &BTreeMap<String, String>, primary: &str) -> Vec<String> {
    let mut seen = vec![primary.to_lowercase()];
    let mut names = Vec::new();
    let mut add = |name: String, names: &mut Vec<String>| {
        let folded = name.to_lowercase();
        if !seen.contains(&folded) {
            seen.push(folded);
            names.push(name);
        }
    };
    for (key, value) in tags {
        if key == "name" || !is_name_key(key) {
            continue;
        }
        for name in value.split(';').filter_map(collapse_whitespace) {
            add(name, &mut names);
        }
    }
    let written = std::iter::once(primary.to_string())
        .chain(names.iter().cloned())
        .collect::<Vec<_>>();
    for name in written {
        if let Some(latin) = to_latin(&name).as_deref().and_then(collapse_whitespace) {
            add(latin, &mut names);
        }
    }
    names
}

/// The name tags of `tags`, for the features that keep only some tags past
/// the scan.
pub(crate) fn name_tags(tags: &BTreeMap<String, String>) -> impl Iterator<Item = (String, String)> {
    tags.iter()
        .filter(|(key, _)| is_name_key(key))
        .map(|(key, value)| (key.clone(), value.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cleaned_values_and_presence() {
        let tags = BTreeMap::from([
            ("addr:street".to_string(), "  King   Street ".to_string()),
            ("addr:unit".to_string(), "   ".to_string()),
        ]);
        assert_eq!(tags.cleaned("addr:street").as_deref(), Some("King Street"));
        assert_eq!(tags.cleaned("addr:unit"), None);
        assert_eq!(tags.cleaned("missing"), None);
        assert!(tags.has("addr:street"));
        assert!(!tags.has("addr:unit"));
        assert!(!tags.has("missing"));
    }

    #[test]
    fn name_keys_are_names_in_a_language() {
        for key in [
            "name",
            "name:en",
            "name:ja-Hira",
            "name:ja_rm",
            "name:zh-Hant-TW",
            "alt_name",
            "alt_name:ja-Latn",
            "official_name",
            "short_name:en",
            "old_name",
        ] {
            assert!(is_name_key(key), "{key}");
        }
        for key in [
            "name:etymology",
            "name:etymology:wikidata",
            "name:prefix",
            "name:",
            "old_name:1897-2010",
            "loc_name",
            "sorting_name",
            "nameplate",
            "name:EN",
        ] {
            assert!(!is_name_key(key), "{key}");
        }
    }

    #[test]
    fn alternate_names_split_lists_and_drop_repeats() {
        let tags = BTreeMap::from([
            ("name".to_string(), "横浜市".to_string()),
            ("name:ja".to_string(), "横浜市".to_string()),
            ("name:ja-Hira".to_string(), "よこはまし".to_string()),
            ("name:en".to_string(), "Yokohama".to_string()),
            ("name:fr".to_string(), "yokohama".to_string()),
            ("alt_name".to_string(), "Hama; 浜 ;".to_string()),
            ("name:etymology".to_string(), "beach".to_string()),
        ]);
        assert_eq!(
            alternate_names(&tags, "横浜市"),
            vec!["Hama", "浜", "Yokohama", "よこはまし", "yokohamashi"]
        );
        // The Latin form of a name in kana or Cyrillic, unless a tag states it.
        let tags = BTreeMap::from([
            ("name".to_string(), "マクドナルド".to_string()),
            ("name:ru".to_string(), "Макдоналдс".to_string()),
            ("name:en".to_string(), "Makudonarudo".to_string()),
        ]);
        assert_eq!(
            alternate_names(&tags, "マクドナルド"),
            vec!["Makudonarudo", "Макдоналдс", "makdonalds"]
        );
    }
}
