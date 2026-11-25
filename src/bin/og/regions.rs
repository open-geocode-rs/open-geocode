use std::sync::OnceLock;

use open_geocode::record::OSM_DATASET;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    World,
    Continent,
    Country,
    Subregion,
    Group,
}

#[derive(Deserialize)]
struct Catalog {
    schema: u32,
    regions: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    id: String,
    name: String,
    kind: Kind,
    parent: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
    osm: Osm,
    #[serde(default)]
    openaddresses: Option<OpenAddresses>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Osm {
    pub id: Option<String>,
    pub url: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub(crate) struct OpenAddresses {
    pub rows: u64,
    pub bytes: u64,
}

const SCHEMA: u32 = 1;

pub(crate) struct Region {
    pub index: usize,
    pub id: String,
    pub name: String,
    pub label: String,
    pub kind: Kind,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub aliases: Vec<String>,
    pub osm: Osm,
    pub openaddresses: Option<OpenAddresses>,
}

impl Region {
    pub(crate) fn slug(&self) -> String {
        self.id.replace('/', "-")
    }

    pub(crate) fn dataset(&self) -> &'static str {
        OSM_DATASET
    }

    pub(crate) fn is_world(&self) -> bool {
        self.kind == Kind::World
    }

    /// The Geofabrik replication state of the extract, which names the time
    /// its data is from. The planet is published elsewhere and has none.
    pub(crate) fn state_url(&self) -> Option<String> {
        self.osm
            .url
            .strip_suffix("-latest.osm.pbf")
            .filter(|_| self.osm.url.starts_with("https://download.geofabrik.de/"))
            .map(|base| format!("{base}-updates/state.txt"))
    }

    pub(crate) fn unit(&self) -> &'static str {
        match self.kind {
            Kind::World => "continents",
            Kind::Continent => "countries",
            _ => "regions",
        }
    }
}

pub(crate) struct Tree {
    regions: Vec<Region>,
    root: usize,
}

impl Tree {
    pub(crate) fn root(&self) -> &Region {
        &self.regions[self.root]
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Region> {
        self.regions.iter().find(|region| region.id == id)
    }

    pub(crate) fn at(&self, index: usize) -> &Region {
        &self.regions[index]
    }

    pub(crate) fn searchable(&self) -> impl Iterator<Item = &Region> {
        self.regions
            .iter()
            .filter(|region| !matches!(region.kind, Kind::World | Kind::Group))
    }

    pub(crate) fn trail(&self, region: &Region) -> String {
        let mut names = Vec::new();
        let mut next = region.parent;
        while let Some(index) = next {
            let parent = &self.regions[index];
            if !parent.is_world() {
                names.push(parent.name.as_str());
            }
            next = parent.parent;
        }
        names.join(" · ")
    }

    pub(crate) fn ancestors(&self, region: &Region) -> Vec<usize> {
        let mut chain = Vec::new();
        let mut next = region.parent;
        while let Some(index) = next {
            if !self.regions[index].is_world() {
                chain.push(index);
            }
            next = self.regions[index].parent;
        }
        chain.reverse();
        chain
    }
}

pub(crate) fn overlaps(tree: &Tree, one: &Region, other: &Region) -> bool {
    one.index == other.index
        || tree.ancestors(one).contains(&other.index)
        || tree.ancestors(other).contains(&one.index)
}

pub(crate) fn dataset_name(id: &str) -> &str {
    match id {
        OSM_DATASET => "OpenStreetMap",
        other => other,
    }
}

pub(crate) fn tree() -> &'static Tree {
    static TREE: OnceLock<Tree> = OnceLock::new();
    TREE.get_or_init(|| load(include_str!("../../../catalog/regions.json")))
}

fn load(text: &str) -> Tree {
    let catalog: Catalog =
        serde_json::from_str(text).expect("catalog/regions.json is a region catalog");
    assert_eq!(
        catalog.schema, SCHEMA,
        "catalog/regions.json has schema {}, this build reads {SCHEMA}",
        catalog.schema
    );
    let entries = catalog.regions;
    let index_of = |id: &str| entries.iter().position(|entry| entry.id == id);
    let mut regions: Vec<Region> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| Region {
            index,
            id: entry.id.clone(),
            name: entry.name.clone(),
            label: entry.name.clone(),
            kind: entry.kind,
            parent: entry.parent.as_deref().and_then(index_of),
            children: Vec::new(),
            aliases: entry.aliases.clone(),
            osm: entry.osm.clone(),
            openaddresses: entry.openaddresses,
        })
        .collect();
    let root = regions
        .iter()
        .position(Region::is_world)
        .expect("the catalog has the planet");
    for index in 0..regions.len() {
        if regions[index].kind == Kind::Group {
            continue;
        }
        if let Some(parent) = regions[index].parent {
            regions[parent].children.push(index);
        }
    }
    let names: Vec<String> = regions
        .iter()
        .map(|region| region.name.to_lowercase())
        .collect();
    for region in &mut regions {
        region.children.sort_by(|a, b| names[*a].cmp(&names[*b]));
    }
    for index in 0..regions.len() {
        let mut parts = vec![regions[index].name.clone()];
        let mut next = regions[index].parent;
        while let Some(parent) = next {
            if matches!(regions[parent].kind, Kind::World | Kind::Continent) {
                break;
            }
            parts.push(regions[parent].name.clone());
            next = regions[parent].parent;
        }
        parts.reverse();
        regions[index].label = parts.join(" / ");
    }
    Tree { regions, root }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn the_catalog_is_one_tree_with_distinct_pack_names() {
        let tree = tree();
        assert!(tree.root().is_world());
        let mut slugs = HashSet::new();
        for region in &tree.regions {
            assert!(slugs.insert(region.slug()), "{} twice", region.slug());
            if !region.is_world() {
                assert!(region.parent.is_some(), "{} has no parent", region.id);
            }
            assert!(region.osm.bytes > 0, "{} has no size", region.id);
        }
    }

    #[test]
    fn overlapping_regions_are_those_one_contains() {
        let tree = tree();
        let region = |id: &str| tree.get(id).unwrap_or_else(|| panic!("no region {id}"));
        let (canada, ontario) = (region("canada"), region("canada/ontario"));
        let australia = region("australia");

        assert!(overlaps(tree, canada, canada), "a region overlaps itself");
        assert!(overlaps(tree, canada, ontario), "a parent contains a child");
        assert!(overlaps(tree, ontario, canada), "and the other way round");
        assert!(
            !overlaps(tree, ontario, australia),
            "regions in different countries are disjoint"
        );
        assert!(!overlaps(tree, canada, australia));
    }
}
