pub(crate) fn collapse_whitespace(value: &str) -> Option<String> {
    let mut cleaned = String::with_capacity(value.len());
    for word in value.split_whitespace() {
        if !cleaned.is_empty() {
            cleaned.push(' ');
        }
        cleaned.push_str(word);
    }
    (!cleaned.is_empty()).then_some(cleaned)
}

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
}
