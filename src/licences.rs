//! OpenAddresses licences come from `catalog/licences.json`, generated from
//! the pinned snapshot. A data set it does not list, or a file handed by
//! path, is credited by name alone.
//!
//! The catalog also says what each data set's terms let a Pack do with its
//! rows ([`Use`]), and a build takes a data set only when [`Licences`] lets
//! it.

use std::{collections::HashMap, sync::OnceLock};

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

/// What an OpenAddresses data set's terms let a Pack do with its rows, as
/// `catalog/licences.json` records it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Use {
    /// Use, change and redistribute, commercial use included, at most with
    /// credit and share-alike.
    Open,
    /// No licence stated, or terms that grant nothing clear.
    #[default]
    Unknown,
    /// Terms forbid serving the rows to anyone for any use: non-commercial,
    /// all rights reserved, or conditions a Pack cannot carry.
    Restricted,
}

/// Which OpenAddresses data sets a build takes, by their [`Use`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Licences {
    /// Every data set but the restricted ones. Most address data stating
    /// no licence is a county's or a city's public record.
    #[default]
    Permitted,
    /// Only data sets under an open licence, for a Pack whose every row
    /// may be used commercially.
    Open,
}

impl Licences {
    pub fn takes(self, used: Use) -> bool {
        match self {
            Licences::Permitted => used != Use::Restricted,
            Licences::Open => used == Use::Open,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Catalog {
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
    #[serde(default, rename = "use")]
    used: Use,
}

pub fn of(dataset: &str) -> Option<Licence> {
    if dataset == OSM_DATASET {
        return Some(osm());
    }
    let path = dataset.strip_prefix(DATASET_PREFIX)?;
    openaddresses().get(path).cloned()
}

/// What the terms of OpenAddresses data set `name` of `source` let a Pack
/// do; one the catalog does not list states no licence.
pub fn openaddresses_use(source: &str, name: &str) -> Use {
    static TABLE: OnceLock<HashMap<(String, String), Use>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            catalog()
                .datasets
                .iter()
                .map(|row| ((row.source.clone(), row.name.clone()), row.used))
                .collect()
        })
        .get(&(source.to_string(), name.to_string()))
        .copied()
        .unwrap_or_default()
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

    #[test]
    fn a_build_takes_a_data_set_by_what_its_terms_allow() {
        assert_eq!(
            openaddresses_use("ar/c/city_of_buenos_aires", "city"),
            Use::Open
        );
        assert_eq!(
            openaddresses_use("us/wa/spokane", "county"),
            Use::Restricted
        );
        assert_eq!(openaddresses_use("zz/nowhere", "city"), Use::Unknown);
        assert!(Licences::Permitted.takes(Use::Unknown));
        assert!(!Licences::Permitted.takes(Use::Restricted));
        assert!(!Licences::Open.takes(Use::Unknown));
        assert!(Licences::Open.takes(Use::Open));
    }
}
