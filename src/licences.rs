//! OpenAddresses licences come from `catalog/licences.json`, generated from
//! the pinned snapshot. A data set it does not list, or a file handed by
//! path, is credited by name alone.
//!
//! A build judges each data set by the licence OpenAddresses states on the
//! job whose output it reads ([`judge`]), not by the pinned snapshot, so terms
//! a publisher changed since the pin decide. Rules read a standard licence's
//! URL and a few phrases of free text; the catalog's `decisions` overrule
//! them for texts read by hand, and only for the exact text read.

use std::{collections::HashMap, sync::OnceLock};

use regex::Regex;
use serde::Deserialize;

use crate::{openaddresses::DATASET_PREFIX, record::OSM_DATASET};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Licence {
    /// The URL of its terms when the source states no name.
    pub name: String,
    pub url: Option<String>,
    pub attribution: Option<String>,
}

// Every answer's areas and labels come from OSM, so it is always credited.
pub fn osm() -> Licence {
    Licence {
        name: "ODbL 1.0".to_string(),
        url: Some("https://www.openstreetmap.org/copyright".to_string()),
        attribution: Some("© OpenStreetMap contributors".to_string()),
    }
}

/// What an OpenAddresses data set's licence lets a Pack do with its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Terms {
    /// Nothing in the licence forbids serving the rows, commercial use
    /// included. A data set that states no licence is one.
    Permitted,
    /// Use for gain is forbidden: a Pack with these rows is for
    /// non-commercial use only.
    NonCommercial,
    /// Serving the rows in a Pack is forbidden for any use: all rights
    /// reserved, no copies, no derivatives, or a condition a Pack cannot
    /// carry with every copy.
    Restricted,
}

impl Terms {
    pub fn taken(self, non_commercial: bool) -> bool {
        match self {
            Terms::Permitted => true,
            Terms::NonCommercial => non_commercial,
            Terms::Restricted => false,
        }
    }
}

/// A licence as OpenAddresses states it: its text and the URL of its terms,
/// either or both missing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stated {
    pub text: Option<String>,
    pub url: Option<String>,
}

impl Stated {
    fn words(&self) -> String {
        [self.text.as_deref(), self.url.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ")
    }
}

// Creative Commons writes a licence's terms into its URL and short name:
// `nc` non-commercial, `nd` no derivatives. A Pack's records are derived
// from the rows, so ND forbids them as much as reserved rights do.
fn restricted() -> &'static Regex {
    static RULE: OnceLock<Regex> = OnceLock::new();
    RULE.get_or_init(|| {
        Regex::new(
            r"(?i)creativecommons\.org/licen[cs]es/by(-nc)?-nd\b|\bcc[- ]by(-nc)?-nd\b|all rights reserved|shall (not )?(sell|give|reproduce)|without permission|prohibited",
        )
        .expect("restricted rule is a regex")
    })
}

fn non_commercial() -> &'static Regex {
    static RULE: OnceLock<Regex> = OnceLock::new();
    RULE.get_or_init(|| {
        Regex::new(
            r"(?i)creativecommons\.org/licen[cs]es/by-nc\b|\bcc[- ]by[- ]nc\b|non-?commercial|not for commercial",
        )
        .expect("non-commercial rule is a regex")
    })
}

/// What the licence `stated` for a data set of OpenAddresses source `source`
/// lets a Pack do. A decision read by hand for the source settles it while
/// the text is the one read; otherwise the rules do, restriction first, so
/// `CC BY-NC-ND` is restricted.
pub fn judge(source: &str, stated: &Stated) -> Terms {
    if let (Some(decision), Some(text)) = (catalog().decisions.get(source), &stated.text)
        && decision.text.trim() == text.trim()
    {
        return decision.terms;
    }
    let words = stated.words();
    if restricted().is_match(&words) {
        Terms::Restricted
    } else if non_commercial().is_match(&words) {
        Terms::NonCommercial
    } else {
        Terms::Permitted
    }
}

/// The licence the pinned snapshot records for data set `name` of `source`,
/// for a job that states none: its source's, when the name is not listed.
pub fn pinned(source: &str, name: &str) -> Stated {
    let rows = &catalog().datasets;
    let row = rows
        .iter()
        .find(|row| row.source == source && row.name == name)
        .or_else(|| rows.iter().find(|row| row.source == source));
    row.map(|row| Stated {
        text: row.licence.clone(),
        url: row.url.clone(),
    })
    .unwrap_or_default()
}

#[derive(Debug, Deserialize)]
struct Decision {
    terms: Terms,
    text: String,
}

#[derive(Debug, Deserialize)]
struct Catalog {
    #[serde(default)]
    decisions: HashMap<String, Decision>,
    datasets: Vec<Row>,
}

#[derive(Debug, Deserialize)]
struct Row {
    source: String,
    name: String,
    licence: Option<String>,
    url: Option<String>,
    #[serde(default)]
    attribution: bool,
    attribution_name: Option<String>,
}

pub fn of(dataset: &str) -> Option<Licence> {
    if dataset == OSM_DATASET {
        return Some(osm());
    }
    let path = dataset.strip_prefix(DATASET_PREFIX)?;
    openaddresses().get(path).cloned()
}

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("../catalog/licences.json"))
            .expect("catalog/licences.json is a licence catalog")
    })
}

fn openaddresses() -> &'static HashMap<String, Licence> {
    static TABLE: OnceLock<HashMap<String, Licence>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let catalog = catalog();
        let mut names = HashMap::<&str, usize>::new();
        for row in &catalog.datasets {
            *names.entry(row.source.as_str()).or_default() += 1;
        }
        let mut table = HashMap::new();
        for row in &catalog.datasets {
            let Some(name) = row.licence.clone().or_else(|| row.url.clone()) else {
                continue;
            };
            let licence = Licence {
                name,
                url: row.url.clone(),
                attribution: row.attribution.then(|| {
                    row.attribution_name
                        .clone()
                        .unwrap_or_else(|| format!("OpenAddresses {}", row.source))
                }),
            };
            if names[row.source.as_str()] == 1 {
                table.insert(row.source.clone(), licence.clone());
            }
            table.insert(format!("{}/{}", row.source, row.name), licence);
        }
        table
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_openaddresses_data_set_is_credited_by_its_catalog_licence() {
        let buenos_aires = of("openaddresses:ar/c/city_of_buenos_aires").expect("licence");
        assert_eq!(buenos_aires.name, "CC BY 2.5 AR");
        assert_eq!(
            buenos_aires.attribution.as_deref(),
            Some("OpenAddresses ar/c/city_of_buenos_aires")
        );
        assert_eq!(of("osm"), Some(osm()));
        assert_eq!(of("openaddresses:zz/nowhere"), None);
        assert_eq!(of("my-file"), None);
    }

    fn stated(text: Option<&str>, url: Option<&str>) -> Stated {
        Stated {
            text: text.map(str::to_string),
            url: url.map(str::to_string),
        }
    }

    #[test]
    fn judges_a_licence_by_its_standard_url_and_its_words() {
        let spokane = stated(
            Some("CC BY-NC-SA 4.0"),
            Some("https://creativecommons.org/licenses/by-nc-sa/4.0/"),
        );
        assert_eq!(judge("us/wa/spokane", &spokane), Terms::NonCommercial);
        let no_derivatives = stated(
            None,
            Some("https://creativecommons.org/licenses/by-nc-nd/4.0/"),
        );
        assert_eq!(judge("zz/a", &no_derivatives), Terms::Restricted);
        let bucks = stated(Some("Not for commercial use or resale."), None);
        assert_eq!(judge("us/pa/bucks", &bucks), Terms::NonCommercial);
        let reserved = stated(Some("Crown Copyright, all rights reserved"), None);
        assert_eq!(judge("ky/countrywide", &reserved), Terms::Restricted);
        let open = stated(
            Some("CC BY 4.0"),
            Some("https://creativecommons.org/licenses/by/4.0/"),
        );
        assert_eq!(judge("zz/b", &open), Terms::Permitted);
        assert_eq!(
            judge("zz/c", &Stated::default()),
            Terms::Permitted,
            "no licence stated"
        );
    }

    #[test]
    fn a_decision_read_by_hand_holds_only_for_the_text_read() {
        let estonia = pinned("ee/countrywide", "country");
        assert!(
            non_commercial().is_match(&estonia.words()),
            "\"or non-commercial purposes\""
        );
        assert_eq!(judge("ee/countrywide", &estonia), Terms::Permitted);
        let changed = Stated {
            text: Some("Not for commercial use.".to_string()),
            ..estonia
        };
        assert_eq!(judge("ee/countrywide", &changed), Terms::NonCommercial);
        let omaha = pinned("us/ne/omaha", "state");
        assert_eq!(judge("us/ne/omaha", &omaha), Terms::Restricted);
        assert_eq!(
            judge("us/ne/omaha", &stated(Some("CC0 1.0"), None)),
            Terms::Permitted,
            "a new licence is judged by the rules, not the old reading"
        );
    }

    #[test]
    fn of_the_pinned_licences_seven_forbid_what_a_default_pack_does() {
        let mut judged: Vec<(&str, Terms)> = catalog()
            .datasets
            .iter()
            .map(|row| {
                let terms = judge(
                    &row.source,
                    &Stated {
                        text: row.licence.clone(),
                        url: row.url.clone(),
                    },
                );
                (row.source.as_str(), terms)
            })
            .filter(|(_, terms)| *terms != Terms::Permitted)
            .collect();
        judged.dedup();
        assert_eq!(
            judged,
            vec![
                ("ca/on/city_of_peterborough", Terms::Restricted),
                ("ky/countrywide", Terms::Restricted),
                ("us/ia/linn", Terms::Restricted),
                ("us/ne/omaha", Terms::Restricted),
                ("us/nm/city_of_las_cruces", Terms::Restricted),
                ("us/pa/bucks", Terms::NonCommercial),
                ("us/wa/spokane", Terms::NonCommercial),
            ]
        );
        assert!(Terms::NonCommercial.taken(true) && !Terms::NonCommercial.taken(false));
        assert!(!Terms::Restricted.taken(true) && Terms::Permitted.taken(false));
    }

    #[test]
    fn a_job_without_a_licence_falls_back_to_its_sources_pinned_one() {
        assert_eq!(
            pinned("us/wa/spokane", "renamed").text.as_deref(),
            Some("CC BY-NC-SA 4.0")
        );
        assert_eq!(pinned("zz/nowhere", "city"), Stated::default());
    }
}
