//! OpenAddresses licences come from `catalog/licences.json`, generated from
//! the pinned snapshot. A data set it does not list, or a file handed by
//! path, is credited by name alone.

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
}

pub fn of(dataset: &str) -> Option<Licence> {
    if dataset == OSM_DATASET {
        return Some(osm());
    }
    let path = dataset.strip_prefix(DATASET_PREFIX)?;
    openaddresses().get(path).cloned()
}

fn openaddresses() -> &'static HashMap<String, Licence> {
    static TABLE: OnceLock<HashMap<String, Licence>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let catalog: Catalog = serde_json::from_str(include_str!("../catalog/licences.json"))
            .expect("catalog/licences.json is a licence catalog");
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
}
