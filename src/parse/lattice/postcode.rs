//! Per-country postcode patterns.
//!
//! The patterns are the `zip` field of Google's libaddressinput country data
//! (<https://github.com/google/libaddressinput>, `testdata/countryinfo.txt`,
//! Apache License 2.0, see `NOTICE`), vendored as
//! `data/postcodes.tsv`. They are data,
//! compiled into the binary; nothing here is written per country.

use std::sync::OnceLock;

use regex::RegexSet;

use super::CountryCode;

/// `CC<TAB>pattern` rows.
const POSTCODES: &str = include_str!("data/postcodes.tsv");
/// The most tokens a postcode spans once normalized ("sw1a 1aa", "1234 ab").
pub const MAX_TOKENS: usize = 3;

/// Every country's postcode pattern, matched in one pass.
#[derive(Debug)]
pub struct PostcodeTable {
    countries: Vec<CountryCode>,
    patterns: RegexSet,
}

impl PostcodeTable {
    /// The table compiled once per process.
    pub fn global() -> &'static Self {
        static TABLE: OnceLock<PostcodeTable> = OnceLock::new();
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
            patterns: RegexSet::new(patterns).expect("vendored postcode patterns compile"),
        }
    }

    /// Countries with a pattern.
    pub fn len(&self) -> usize {
        self.countries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.countries.is_empty()
    }

    /// The countries whose pattern matches all of `text`, in code order.
    /// Case does not matter.
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

    /// Whether `country`'s pattern matches all of `text`.
    pub fn matches_country(&self, country: CountryCode, text: &str) -> bool {
        self.matches(text).contains(&country)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(table: &PostcodeTable, text: &str) -> Vec<String> {
        table
            .matches(text)
            .iter()
            .map(|code| code.as_str().to_string())
            .collect()
    }

    #[test]
    fn the_vendored_table_compiles_and_covers_the_world() {
        let table = PostcodeTable::global();
        assert!(table.len() > 150, "{}", table.len());
    }

    #[test]
    fn patterns_match_whole_postcodes_in_any_case() {
        let table = PostcodeTable::global();
        let ca = CountryCode::parse("CA").unwrap();
        assert!(table.matches_country(ca, "m5v 1m9"));
        assert!(table.matches_country(ca, "M5V1M9"));
        assert!(!table.matches_country(ca, "m5v"));
        assert!(!table.matches_country(ca, "670"));
        assert!(table.matches_country(CountryCode::parse("GB").unwrap(), "sw1a 1aa"));
        assert!(table.matches_country(CountryCode::parse("NL").unwrap(), "1234 ab"));
        // Five digits are a postcode in many countries, so a pattern match
        // alone does not decide the country.
        let five = codes(table, "72100");
        for code in ["DE", "FR", "US", "IT", "ES"] {
            assert!(five.contains(&code.to_string()), "{five:?}");
        }
        assert!(!five.contains(&"CA".to_string()));
        // Some patterns match plain words (Bermuda's "AB 12" also matches
        // "king"), which is why the parser tries only spans with a digit.
        assert!(table.matches("toronto").is_empty());
    }

    #[test]
    fn rows_without_a_pattern_or_code_are_skipped() {
        let table = PostcodeTable::parse("CA\t\\d{3}\nbad row\nXYZ\t\\d\n");
        assert_eq!(table.len(), 1);
        assert_eq!(codes(&table, "123"), vec!["CA"]);
    }
}
