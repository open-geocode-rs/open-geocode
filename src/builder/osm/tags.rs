use std::collections::BTreeMap;

use crate::util::text::collapse_whitespace;

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
}
