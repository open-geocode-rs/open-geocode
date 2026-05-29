/// Collapse runs of whitespace to single spaces and trim; `None` if the value
/// is empty once normalized.
pub(crate) fn collapse_whitespace(value: &str) -> Option<String> {
    let cleaned = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Whitespace-collapsed and ASCII-lowercased, for case-insensitive comparison.
/// Always returns a string (empty for blank input) to keep the previous
/// `normalize_for_compare` semantics.
pub(crate) fn normalize_for_compare(value: &str) -> String {
    collapse_whitespace(value)
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_internal_and_edge_whitespace() {
        assert_eq!(
            collapse_whitespace("  King   Street "),
            Some("King Street".to_string())
        );
        assert_eq!(collapse_whitespace("   "), None);
        assert_eq!(collapse_whitespace(""), None);
    }

    #[test]
    fn normalizes_for_case_insensitive_compare() {
        assert_eq!(normalize_for_compare(" Main   STREET "), "main street");
        assert_eq!(normalize_for_compare("   "), "");
    }
}
