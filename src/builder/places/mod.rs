pub(super) mod overture;

use crate::record::PoiRecord;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Identity {
    pub outline: Option<Vec<u8>>,
    pub nodes: Vec<i64>,
    pub websites: Vec<String>,
    pub phones: Vec<String>,
    pub refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Place {
    pub record: PoiRecord,
    pub identity: Identity,
}

pub(super) fn normalized_website(value: &str) -> Option<String> {
    let value = value.to_lowercase();
    let value = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .unwrap_or(&value);
    let value = value.strip_prefix("www.").unwrap_or(value);
    Some(value.trim_end_matches('/').to_string())
}

pub(super) fn normalized_phone(value: &str) -> Option<String> {
    let digits = value
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<Vec<_>>();
    let tail = &digits[digits.len().saturating_sub(9)..];
    (!tail.is_empty()).then(|| tail.iter().collect())
}
