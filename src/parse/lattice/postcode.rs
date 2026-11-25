//! Per-country postcode patterns.
//!
//! The patterns are the `zip` field of Google's libaddressinput country data
//! (<https://github.com/google/libaddressinput>, `testdata/countryinfo.txt`,
//! Apache License 2.0, see `NOTICE`), vendored as
//! `data/postcodes.tsv`. They are data,
//! compiled into the binary; nothing here is written per country.

use std::sync::OnceLock;

use regex::{RegexSet, RegexSetBuilder};

use super::CountryCode;

const POSTCODES: &str = include_str!("data/postcodes.tsv");
pub const MAX_TOKENS: usize = 3;
/// Room for the set's lazy DFA cache, per thread. The crate builds a lazy
/// DFA only when its cache can hold the minimum the reverse NFA asks, and the
/// Unicode classes of every pattern make that 2.9 MB. Under the default 2 MiB
/// it builds none, and every span runs on the PikeVM, about 70 times slower,
/// on every query with a digit. The cache grows only as far as it is used.
const DFA_CACHE_BYTES: usize = 8 << 20;

#[derive(Debug)]
pub struct Table {
    countries: Vec<CountryCode>,
    patterns: RegexSet,
}

impl Table {
    pub fn global() -> &'static Self {
        static TABLE: OnceLock<Table> = OnceLock::new();
        TABLE.get_or_init(|| Self::parse(POSTCODES))
    }

    fn parse(rows: &str) -> Self {
        let (countries, patterns): (Vec<_>, Vec<_>) = rows
            .lines()
            .filter_map(|line| {
                let (code, pattern) = line.split_once('\t')?;
                Some((
                    CountryCode::parse(code)?,
                    format!("^(?i:{})$", pattern.trim()),
                ))
            })
            .unzip();
        Self {
            countries,
            patterns: RegexSetBuilder::new(patterns)
                .dfa_size_limit(DFA_CACHE_BYTES)
                .build()
                .expect("vendored postcode patterns compile"),
        }
    }

    pub fn len(&self) -> usize {
        self.countries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.countries.is_empty()
    }

    pub fn matches(&self, text: &str) -> Vec<CountryCode> {
        let mut countries = self
            .patterns
            .matches(text)
            .into_iter()
            .map(|index| self.countries[index])
            .collect::<Vec<_>>();
        countries.sort_unstable();
        countries
    }

    pub fn matches_country(&self, country: CountryCode, text: &str) -> bool {
        self.matches(text).contains(&country)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(table: &Table, text: &str) -> Vec<String> {
        table
            .matches(text)
            .iter()
            .map(|code| code.as_str().to_string())
            .collect()
    }

    #[test]
    fn patterns_match_whole_postcodes_in_any_case() {
        let table = Table::global();
        let ca = CountryCode::parse("CA").unwrap();
        assert!(table.matches_country(ca, "m5v 1m9"));
        assert!(table.matches_country(ca, "M5V1M9"));
        assert!(!table.matches_country(ca, "m5v"));
        assert!(!table.matches_country(ca, "670"));
        assert!(table.matches_country(CountryCode::parse("GB").unwrap(), "sw1a 1aa"));
        assert!(table.matches_country(CountryCode::parse("NL").unwrap(), "1234 ab"));
        let five = codes(table, "72100");
        for code in ["DE", "FR", "US", "IT", "ES"] {
            assert!(five.contains(&code.to_string()), "{five:?}");
        }
        assert!(!five.contains(&"CA".to_string()));
        assert!(table.matches("toronto").is_empty());
    }

    #[test]
    fn rows_from_the_start_middle_and_end_of_the_vendored_table_load() {
        let table = Table::global();
        for (country, postcode) in [
            ("AC", "ASCN 1ZZ"),
            ("AD", "AD500"),
            ("AF", "1001"),
            ("KR", "110-110"),
            ("KY", "KY1-1100"),
            ("KZ", "050000"),
            ("XK", "10000"),
            ("ZA", "2001"),
            ("ZM", "10101"),
        ] {
            let code = CountryCode::parse(country).unwrap();
            assert!(
                table.matches_country(code, postcode),
                "{country} {postcode}"
            );
        }
    }

    #[test]
    fn rows_without_a_pattern_or_code_are_skipped() {
        let table = Table::parse("CA\t\\d{3}\nbad row\nXYZ\t\\d\n");
        assert_eq!(table.len(), 1);
        assert_eq!(codes(&table, "123"), vec!["CA"]);
    }
}
