//! The region tree the build picker browses: the planet, continents,
//! countries and sub-regions, each with the size of its download. It is read
//! from `regions.json`, built from Geofabrik's index.

use std::sync::OnceLock;

use serde::Deserialize;

/// Seconds of build time per MB of download, from an Ontario build on the
/// development machine. A placeholder until builds record their own rate.
const SECS_PER_MB: f64 = 0.085;

/// Extracts the preview pretends are already on disk.
const ON_DISK: [&str; 5] = ["canada", "france", "germany", "japan", "russia"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    World,
    Continent,
    Country,
    Subregion,
    /// An extract that overlaps others, such as the Alps. Not browsable.
    Group,
}

#[derive(Deserialize)]
struct Entry {
    id: String,
    name: String,
    kind: Kind,
    parent: Option<String>,
    bytes: u64,
}

pub struct Region {
    pub index: usize,
    pub id: String,
    pub name: String,
    /// The name with the countries above it, `Canada / Ontario`.
    pub label: String,
    pub kind: Kind,
    pub parent: Option<usize>,
    /// Browsable regions directly below this one, by name.
    pub children: Vec<usize>,
    pub bytes: u64,
}

impl Region {
    pub fn size_mb(&self) -> f64 {
        self.bytes as f64 / 1e6
    }

    /// The Pack directory name: the last part of the id.
    pub fn slug(&self) -> &str {
        self.id.rsplit('/').next().unwrap_or(&self.id)
    }

    pub fn is_world(&self) -> bool {
        self.kind == Kind::World
    }

    /// Build time estimated from the download size.
    pub fn build_secs(&self) -> f64 {
        self.size_mb() * SECS_PER_MB
    }

    /// The extract's path if it is already on disk, so no download is needed.
    pub fn local(&self) -> Option<String> {
        ON_DISK
            .contains(&self.id.as_str())
            .then(|| format!("data/pbf/{}.osm.pbf", self.id))
    }

    /// What the regions below are called.
    pub fn unit(&self) -> &'static str {
        match self.kind {
            Kind::World => "continents",
            Kind::Continent => "countries",
            _ => "regions",
        }
    }
}

pub struct Tree {
    regions: Vec<Region>,
}

impl Tree {
    pub fn root(&self) -> &Region {
        &self.regions[0]
    }

    pub fn get(&self, id: &str) -> Option<&Region> {
        self.regions.iter().find(|region| region.id == id)
    }

    pub fn at(&self, index: usize) -> &Region {
        &self.regions[index]
    }

    /// Everything a search can find: the browsable regions, not the planet.
    pub fn searchable(&self) -> impl Iterator<Item = &Region> {
        self.regions
            .iter()
            .filter(|region| !matches!(region.kind, Kind::World | Kind::Group))
    }

    /// The regions above this one, nearest first, without the planet:
    /// `Canada · North America` for Ontario.
    pub fn trail(&self, region: &Region) -> String {
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
    pub fn ancestors(&self, region: &Region) -> Vec<usize> {
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

pub fn tree() -> &'static Tree {
    static TREE: OnceLock<Tree> = OnceLock::new();
    TREE.get_or_init(load)
}

fn load() -> Tree {
    let entries: Vec<Entry> = serde_json::from_str(include_str!("regions.json"))
        .expect("regions.json is a list of regions");
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
            bytes: entry.bytes,
        })
        .collect();
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
    Tree { regions }
}
