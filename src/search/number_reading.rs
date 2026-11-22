use std::sync::LazyLock;

use regex::Regex;

use crate::{
    api::PartMatch,
    pack::{PointPrecision, RecordId},
    record::Layer,
    util::fold::search_key,
};

use super::{TextSearchHit, numbered_area};

const MARKERS: &str = include_str!("../parse/lattice/data/house_number_markers.txt");
const DIGITS: &str = "[0-9０-９]{1,8}";
const DASH: &str = "[-‐‑–－]";

fn markers() -> impl Iterator<Item = (&'static str, &'static str)> {
    MARKERS.lines().map(|row| row.split_once('\t').unwrap())
}

static QUERY_NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = |role| {
        markers()
            .filter(|(position, _)| *position == role || *position == "both")
            .map(|(_, marker)| regex::escape(marker))
            .collect::<Vec<_>>()
            .join("|")
    };
    let separators = pattern("separator");
    let endings = pattern("terminal");
    Regex::new(&format!(
        r"{DIGITS}(?:\s*(?:{separators}|{DASH})\s*{DIGITS}){{0,3}}(?:\s*(?:{endings}))?\z"
    ))
    .expect("valid number marker lexicon")
});
static STORED_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"\A{DIGITS}(?:{DASH}{DIGITS}){{0,3}}\z")).unwrap());

pub(super) struct AddressVariant {
    pub(super) query: String,
    number: String,
    prefix: String,
    witnessed: Option<RecordId>,
    ambiguous: bool,
}

impl AddressVariant {
    fn new(prefix: &str, number: String) -> Self {
        Self {
            query: format!("{prefix}{}", number.replace(' ', "-")),
            number,
            prefix: prefix.to_owned(),
            witnessed: None,
            ambiguous: false,
        }
    }

    pub(super) fn unique_confirmed(&self, hits: Vec<TextSearchHit>) -> Option<TextSearchHit> {
        if self.ambiguous {
            return None;
        }
        let mut confirmed = hits.into_iter().filter(|hit| self.confirms(hit));
        let mut first = confirmed.next()?;
        if self.witnessed.is_some_and(|id| id != first.record_id)
            || confirmed.any(|hit| hit.record_id != first.record_id)
        {
            return None;
        }
        // No confidence model was fitted to alternate readings.
        first.confidence = None;
        Some(first)
    }

    pub(super) fn could_add_witness(&self, hit: &TextSearchHit) -> bool {
        !self.ambiguous && self.witnessed != Some(hit.record_id) && self.matches_written(hit)
    }

    pub(super) fn add_witness(&mut self, hit: &TextSearchHit) {
        if self.confirms(hit) {
            self.ambiguous = self.witnessed.replace(hit.record_id).is_some();
        }
    }

    fn confirms(&self, hit: &TextSearchHit) -> bool {
        let matched = hit.matched;
        let scope = [matched.locality, matched.region, matched.postcode];
        matched.housenumber == Some(PartMatch::Matched)
            && matched.street == Some(PartMatch::Matched)
            && scope.contains(&Some(PartMatch::Matched))
            && [
                matched.locality,
                matched.region,
                matched.postcode,
                matched.unit,
                matched.country,
            ]
            .into_iter()
            .flatten()
            .all(|part| part == PartMatch::Matched)
            && self.matches_written(hit)
    }

    fn matches_written(&self, hit: &TextSearchHit) -> bool {
        hit.record.layer == Layer::Address.as_str()
            && !hit.fallback
            && hit.record.point.is_some_and(|point| {
                matches!(
                    point.precision,
                    PointPrecision::Point | PointPrecision::Centroid
                )
            })
            && hit
                .record
                .address
                .housenumber
                .as_deref()
                .is_some_and(|stored| same_number(stored, &self.number))
            && hit
                .record
                .address
                .street
                .as_deref()
                .is_some_and(|street| written_street(&self.prefix, street))
    }
}

pub(super) fn address_variant(query: &str) -> Option<AddressVariant> {
    let area = numbered_area::alternate(query);
    let written = area.as_deref().unwrap_or(query);
    alternate(written).or_else(|| {
        let area = area?;
        let suffix = COMPOUND_END.find(&area)?;
        let prefix = &area[..suffix.start()];
        if prefix
            .trim_end()
            .ends_with(|c: char| c.is_numeric() || "-‐‑–－/／".contains(c))
        {
            return None;
        }
        Some(AddressVariant::new(prefix, search_key(suffix.as_str())?))
    })
}

fn alternate(query: &str) -> Option<AddressVariant> {
    if !markers().any(|(_, marker)| query.contains(marker)) {
        return None;
    }
    let query = query.trim_end();
    let suffix = QUERY_NUMBER.find(query)?;
    let prefix = &query[..suffix.start()];
    let preceding = prefix.trim_end();
    // A partial match would discard a component of a longer number or fraction.
    if preceding
        .ends_with(|character: char| character.is_numeric() || "-‐‑–－/／".contains(character))
        || markers().any(|(_, marker)| preceding.ends_with(marker))
        || !markers().any(|(_, marker)| suffix.as_str().contains(marker))
    {
        return None;
    }
    let number = search_key(
        suffix
            .as_str()
            .split(|c: char| !c.is_numeric())
            .filter(|group| !group.is_empty())
            .collect::<Vec<_>>()
            .join("-")
            .as_str(),
    )?;
    if !number.contains(' ') {
        return None;
    }
    Some(AddressVariant::new(prefix, number))
}

static COMPOUND_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"{DIGITS}(?:{DASH}{DIGITS}){{1,3}}\z")).unwrap());

fn written_street(prefix: &str, street: &str) -> bool {
    // A shortened parse can mark its street matched after dropping part of the name.
    let compact = |text: &str| search_key(text).map(|key| key.replace(' ', ""));
    compact(street)
        .is_some_and(|street| compact(prefix).is_some_and(|prefix| prefix.ends_with(&street)))
}

fn same_number(stored: &str, reading: &str) -> bool {
    // Search keys fold slashes and other punctuation to the same gap as dashes.
    STORED_NUMBER.is_match(stored) && search_key(stored).as_deref() == Some(reading)
}

pub(super) fn allows_retry<'a>(
    layer: Option<&str>,
    hits: impl Iterator<Item = &'a TextSearchHit>,
) -> bool {
    layer.is_none_or(|layer| layer == Layer::Address.as_str())
        && !hits
            .into_iter()
            .any(|hit| !hit.fallback && hit.matched.locality != Some(PartMatch::Unmatched))
}

#[cfg(test)]
mod tests {
    use super::{alternate, same_number, written_street};

    #[test]
    fn only_reads_the_final_number_expression() {
        for (query, rewritten, number) in [
            ("安中町９丁目１番１２号", "安中町９丁目1-12", "1 12"),
            ("三民路二段26號之5", "三民路二段26-5", "26 5"),
            (
                "달구벌대로530길 145-2번지",
                "달구벌대로530길 145-2",
                "145 2",
            ),
        ] {
            let variant = alternate(query).expect(query);
            assert_eq!(variant.query, rewritten);
            assert_eq!(variant.number, number);
        }
        for query in [
            "달구벌대로530길 145-2",
            "安中町9丁目1番12号 建物",
            "三民路二段26號之",
            "三民路二段26號",
            "ул. Вавилова, 81 корпус 2",
            "Town 11/4號",
            "Town 123456789號",
            "Town 1番2番3番4番5号",
        ] {
            assert!(alternate(query).is_none(), "{query}");
        }
    }

    #[test]
    fn corroboration_distinguishes_other_number_formats() {
        assert!(same_number("１－１２", "1 12"));
        for stored in ["1/12", "1 12", "12-1", "1-12;2", "1-12A"] {
            assert!(!same_number(stored, "1 12"), "{stored}");
        }
    }

    #[test]
    fn corroboration_keeps_the_written_street() {
        assert!(written_street("東京都品川区東大井5丁目", "東大井５丁目"));
        assert!(!written_street("北海道帯広市東９条南１２丁目", "９条"));
    }
}
