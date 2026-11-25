use std::sync::LazyLock;

use regex::Regex;

use crate::parse::lattice::is_unspaced;

const MARKERS: &str = include_str!("../parse/lattice/data/numbered_area_markers.txt");

static EXPLICIT_ORDINAL: LazyLock<Regex> = LazyLock::new(|| {
    let markers = MARKERS
        .lines()
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&format!(r"([〇一二三四五六七八九十]{{1,3}})(?:{markers})")).unwrap()
});
static COMPACT_NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([0-9０-９]{1,2})[-－‐‑–]([0-9０-９]{1,8})[-－‐‑–]([0-9０-９]{1,8})\z").unwrap()
});

fn ordinal(value: &str) -> Option<u32> {
    let digit = |c| {
        "〇一二三四五六七八九"
            .chars()
            .position(|n| n == c)
            .map(|n| n as u32)
    };
    let one = |s: &str| {
        let mut chars = s.chars();
        let n = digit(chars.next()?)?;
        chars.next().is_none().then_some(n)
    };
    let result = if let Some((tens, units)) = value.split_once('十') {
        let tens = if tens.is_empty() { 1 } else { one(tens)? };
        let units = if units.is_empty() { 0 } else { one(units)? };
        tens * 10 + units
    } else {
        one(value)?
    };
    (result > 0).then_some(result)
}

pub(super) fn alternate(query: &str) -> Option<String> {
    if !query.chars().any(is_unspaced) {
        return None;
    }
    let query = query.trim_end();
    if let Some(parts) = EXPLICIT_ORDINAL.captures_iter(query).last() {
        let written = parts.get(1)?;
        let prefix = &query[..written.start()];
        if prefix.ends_with(|c: char| {
            c.is_numeric() || "〇零一二三四五六七八九十百千万億兆壱弐参拾".contains(c)
        }) {
            return None;
        }
        let value = ordinal(written.as_str())?;
        return Some(format!("{prefix}{value}{}", &query[written.end()..]));
    }

    let parts = COMPACT_NUMBER.captures(query)?;
    let number = parts.get(0)?;
    let prefix = &query[..number.start()];
    if !prefix.ends_with(is_unspaced) || parts[1].starts_with(['0', '０']) {
        return None;
    }
    let marker = MARKERS.lines().next()?;
    Some(format!(
        "{prefix}{}{marker}{}-{}",
        &parts[1], &parts[2], &parts[3]
    ))
}

#[cfg(test)]
mod tests {
    use super::alternate;

    #[test]
    fn area_reading_keeps_number_order() {
        for (written, reading) in [
            ("安中町九丁目1-12", "安中町9丁目1-12"),
            ("安中町二十一丁目1-12", "安中町21丁目1-12"),
            ("安中町9-1-12", "安中町9丁目1-12"),
            ("安中町９－１－１２", "安中町９丁目１-１２"),
        ] {
            assert_eq!(alternate(written).as_deref(), Some(reading));
        }
        for written in [
            "King Street 9-1-12",
            "安中町99-9-1-12",
            "安中町9-1-12/7",
            "安中町9丁目1-12",
            "安中町０９－１－１２",
            "安中町二百二十一丁目1-12",
        ] {
            assert_eq!(alternate(written), None);
        }
    }
}
