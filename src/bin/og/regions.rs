//! The region tree the build picker browses: the planet, continents,
//! countries and sub-regions. It is the catalog `catalog/regions.json`, which
//! `catalog/catalog.py` generates from pinned upstream snapshots, so
//! every size and date here is upstream's own.

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
    /// An extract that overlaps others, such as the Alps. Not browsable.
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
}

/// The region's OpenStreetMap extract, as the catalog snapshot saw it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Osm {
    pub url: String,
    pub bytes: u64,
}

/// The catalog schema this build reads.
const SCHEMA: u32 = 1;

pub(crate) struct Region {
    pub index: usize,
    pub id: String,
    pub name: String,
    /// The name with the countries above it, `Canada / Ontario`.
    pub label: String,
    pub kind: Kind,
    pub parent: Option<usize>,
    /// Browsable regions directly below this one, by name.
    pub children: Vec<usize>,
    /// Other names the region is known by, for search.
    pub aliases: Vec<String>,
    pub osm: Osm,
}

impl Region {
    /// The Pack directory name: the id with its parts joined by `-`, so two
    /// regions of one name (`georgia`, `us-georgia`) never share one.
    pub(crate) fn slug(&self) -> String {
        self.id.replace('/', "-")
    }

    /// The dataset a Pack of the region is built from: its extract is
    /// OpenStreetMap's.
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

    /// What the regions below are called.
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

    /// Everything a search can find: the browsable regions, not the planet.
    pub(crate) fn searchable(&self) -> impl Iterator<Item = &Region> {
        self.regions
            .iter()
            .filter(|region| !matches!(region.kind, Kind::World | Kind::Group))
    }

    /// The regions above this one, nearest first, without the planet:
    /// `Canada · North America` for Ontario.
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

    /// The indices from the top continent down to this region's parent.
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

/// How a dataset id reads: `osm` is OpenStreetMap.
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
    // A label is the name under the continent, with the country before a
    // sub-region: `Canada / Ontario`.
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
}
