//! Names in another language.
//!
//! A request with `lang=es` answers the `name:es` of the object an answer
//! names, and keeps the local name elsewhere. The records hold only local
//! names, so a Pack built with `--lang` carries a side section keyed by source
//! id: an address's street has no id, so it also carries the translated way's
//! local name and a few of its vertices. A Pack built without languages
//! carries nothing and the record format does not change.
//!
//! Only features an answer names a place by feed it: named street ways
//! (`highway`), places (`place`) and boundaries (`boundary`, `admin_level`).
//! A translation equal to its name is left out.
//!
//! A language tag is `es`, `zh-Hans`: two or three lower case letters, then
//! subtags of letters and digits. `_` is written `-` (OSM has `name:ja_rm`).
//!
//! ```text
//! names/lang  languages varint | per language, ascending by tag:
//!               tag varint-length text | 3 offsets u64 | 3 lengths u64;
//!               offsets count from the first block
//! ids         entries u64 | 2 * entries + 1 offsets u32 | heap,
//!             texts alternating key "node|way|relation:id" and name
//! street text entries u64 | 2 * entries + 1 offsets u32 | heap, texts
//!             alternating local name and translation
//! street verts entries u64 | entries + 1 offsets u32 | heap, each entry
//!             up to 8 lon,lat i32 LE pairs; entries in the same order
//!             as street text
//! ```

use std::{collections::HashMap, fs::File, io::BufReader, path::PathBuf, sync::Arc};

use osmpbf::{Element, ElementReader};
use rustc_hash::FxHashSet;

use crate::{
    builder::osm::{
        street,
        tags::{OsmTags, is_language_tag, without_pasted_tags},
    },
    container::{Bytes, Container},
    error::{Error, Result},
    pack::SealedPack,
    record::ObjectType,
    spatial_index::haversine_m,
    util::{
        codec::{get_str, get_u64, put_str, put_u64, read_u32_le, read_u64_le},
        text::collapse_whitespace,
    },
};

pub const SECTION: &str = "names/lang";
const VERSION: u32 = 2;
const READ_BUFFER_BYTES: usize = 4 << 20;
const MAX_VERTICES: usize = 8;

// A house numbered on a street usually sits within a few dozen metres of its
// way. 1.5 km spans long residential ways and the gap between two samples of
// an avenue without borrowing a same-named street in the next town.
const MAX_STREET_DISTANCE_M: f64 = 1500.0;

/// `value` as the tag a Pack and a request name a language by, `zh_Hans` as
/// `zh-Hans`; `None` when it is not a language tag.
pub fn parse_language(value: &str) -> Option<String> {
    is_language_tag(value).then(|| value.replace('_', "-"))
}

/// Each of `values` as [`parse_language`] reads it, sorted, once each.
pub(crate) fn parse_languages(values: &[String]) -> Result<Vec<String>> {
    let mut languages = values
        .iter()
        .map(|value| {
            parse_language(value).ok_or_else(|| {
                Error::BadOptions(
                    format!(
                        "{value:?} is not a language tag: two or three lower case letters, then subtags such as `-Hans`"
                    )
                    .into(),
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    languages.sort_unstable();
    languages.dedup();
    Ok(languages)
}

/// Whether the `name:<suffix>` key suffix `tag` names `language`: OSM writes
/// `ja_rm` where the Pack says `ja-rm`.
fn same_language(tag: &str, language: &str) -> bool {
    tag.len() == language.len()
        && tag
            .bytes()
            .zip(language.bytes())
            .all(|(found, wanted)| found == wanted || (found == b'_' && wanted == b'-'))
}

fn clean(value: &str) -> Option<String> {
    collapse_whitespace(without_pasted_tags(value))
}

fn translation<'a>(
    tags: impl Iterator<Item = (&'a str, &'a str)>,
    name: &str,
    languages: &[String],
) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (key, value) in tags {
        let Some(tag) = key.strip_prefix("name:") else {
            continue;
        };
        let Some(index) = languages
            .iter()
            .position(|language| same_language(tag, language))
        else {
            continue;
        };
        if let Some(translated) = clean(value)
            && translated != name
        {
            found.push((index, translated));
        }
    }
    found
}

fn key(object_type: ObjectType, object_id: i64) -> String {
    let kind = match object_type {
        ObjectType::Node => "node",
        ObjectType::Way => "way",
        ObjectType::Relation => "relation",
        ObjectType::Row => "row",
    };
    format!("{kind}:{object_id}")
}

#[derive(Default)]
struct Found {
    by_id: Vec<Vec<(String, String)>>,
    streets: Vec<Vec<(String, String, Vec<i64>)>>,
    wanted_nodes: FxHashSet<i64>,
}

impl Found {
    fn new(languages: usize) -> Self {
        Self {
            by_id: vec![Vec::new(); languages],
            streets: vec![Vec::new(); languages],
            wanted_nodes: FxHashSet::default(),
        }
    }

    fn merge(mut self, other: Self) -> Self {
        for (into, from) in self.by_id.iter_mut().zip(other.by_id) {
            into.extend(from);
        }
        for (into, from) in self.streets.iter_mut().zip(other.streets) {
            into.extend(from);
        }
        self.wanted_nodes.extend(other.wanted_nodes);
        self
    }

    fn sort(&mut self) {
        for language in &mut self.by_id {
            language.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            language.dedup_by(|a, b| a.0 == b.0);
        }
        for language in &mut self.streets {
            language.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        }
    }
}

/// One element's names, and the node ids needed to place a translated street.
/// A place or boundary has its own source id; a street also answers to its
/// vertices so an address whose way is not in the extract still gets its name.
fn count_element(element: Element<'_>, languages: &[String], found: &mut Found) {
    let refs = if let Element::Way(ref way) = element {
        Some(way.refs().collect::<Vec<_>>())
    } else {
        None
    };
    let (object_type, object_id, tags) = match element {
        Element::Node(node) => (ObjectType::Node, node.id(), node.tags().collect::<Vec<_>>()),
        Element::DenseNode(node) => (ObjectType::Node, node.id(), node.tags().collect::<Vec<_>>()),
        Element::Way(way) => (ObjectType::Way, way.id(), way.tags().collect::<Vec<_>>()),
        Element::Relation(relation) => (
            ObjectType::Relation,
            relation.id(),
            relation.tags().collect::<Vec<_>>(),
        ),
    };
    let tags = tags
        .iter()
        .map(|(key, value)| (*key, *value))
        .collect::<Vec<_>>();
    let map = tags
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let name = match map.cleaned("name") {
        Some(name) => name,
        None => return,
    };
    let street = object_type == ObjectType::Way && street::is_street(&map);
    let named = street
        || map.contains_key("place")
        || map.contains_key("boundary")
        || map.contains_key("admin_level");
    if !named {
        return;
    }
    let translations = translation(tags.iter().copied(), &name, languages);
    if translations.is_empty() {
        return;
    }
    let id = key(object_type, object_id);
    for (index, translated) in translations {
        found.by_id[index].push((id.clone(), translated.clone()));
        if street {
            let refs = refs.as_deref().unwrap_or(&[]);
            found.wanted_nodes.extend(refs.iter().copied());
            let sampled = samples(refs.len())
                .into_iter()
                .map(|at| refs[at])
                .collect::<Vec<_>>();
            found.streets[index].push((name.clone(), translated, sampled));
        }
    }
}

/// Up to eight evenly spaced indices, first and last included.
fn samples(len: usize) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }
    let count = MAX_VERTICES.min(len);
    let last = len - 1;
    (0..count)
        .map(|step| step * last / (count - 1).max(1))
        .collect()
}

/// Hexagonal-ish quantisation to 1e-7 degrees, the records' coordinate scale.
fn coordinate(decimicro: i32) -> i32 {
    decimicro
}

thread_local! {
    static NODE_COORDS: std::cell::RefCell<HashMap<i64, [i32; 2]>> =
        std::cell::RefCell::new(HashMap::new());
}

#[derive(Default)]
struct Pass1 {
    found: Found,
}

impl Pass1 {
    fn merge(mut self, other: Self) -> Self {
        self.found = self.found.merge(other.found);
        self
    }
}
fn too_big(language: &str) -> Error {
    Error::OutOfResources(format!(
        "the {language} names are more than 4 GiB of text; keep fewer languages"
    ))
}

fn encode(languages: &[String], found: &Found) -> Result<Vec<u8>> {
    let mut directory = Vec::new();
    put_u64(&mut directory, languages.len() as u64);
    let mut blocks = Vec::new();
    for (index, language) in languages.iter().enumerate() {
        let ids_start = blocks.len();
        let mut heap = Vec::new();
        let mut offsets = Vec::with_capacity(found.by_id[index].len() * 2 + 1);
        offsets.push(0u32);
        for (key, value) in &found.by_id[index] {
            heap.extend_from_slice(key.as_bytes());
            offsets.push(u32::try_from(heap.len()).map_err(|_| too_big(language))?);
            heap.extend_from_slice(value.as_bytes());
            offsets.push(u32::try_from(heap.len()).map_err(|_| too_big(language))?);
        }
        blocks.extend_from_slice(&(found.by_id[index].len() as u64).to_le_bytes());
        for offset in offsets {
            blocks.extend_from_slice(&offset.to_le_bytes());
        }
        blocks.extend_from_slice(&heap);
        let ids_len = blocks.len() - ids_start;

        let text_start = blocks.len();
        let mut text_heap = Vec::new();
        let mut text_offsets = Vec::with_capacity(found.streets[index].len() * 2 + 1);
        text_offsets.push(0u32);
        let mut vertex_heap = Vec::new();
        let mut vertex_offsets = Vec::with_capacity(found.streets[index].len() + 1);
        vertex_offsets.push(0u32);
        for (name, translated, refs) in &found.streets[index] {
            let vertices = refs
                .iter()
                .filter_map(|node| NODE_COORDS.with(|coords| coords.borrow().get(node).copied()))
                .collect::<Vec<_>>();
            let vertices = &vertices;
            text_heap.extend_from_slice(name.as_bytes());
            text_offsets.push(u32::try_from(text_heap.len()).map_err(|_| too_big(language))?);
            text_heap.extend_from_slice(translated.as_bytes());
            text_offsets.push(u32::try_from(text_heap.len()).map_err(|_| too_big(language))?);
            for [lon, lat] in vertices {
                vertex_heap.extend_from_slice(&lon.to_le_bytes());
                vertex_heap.extend_from_slice(&lat.to_le_bytes());
            }
            vertex_offsets.push(u32::try_from(vertex_heap.len()).map_err(|_| too_big(language))?);
        }
        blocks.extend_from_slice(&(found.streets[index].len() as u64).to_le_bytes());
        for offset in text_offsets {
            blocks.extend_from_slice(&offset.to_le_bytes());
        }
        blocks.extend_from_slice(&text_heap);
        let text_len = blocks.len() - text_start;

        let verts_start = blocks.len();
        blocks.extend_from_slice(&(found.streets[index].len() as u64).to_le_bytes());
        for offset in vertex_offsets {
            blocks.extend_from_slice(&offset.to_le_bytes());
        }
        blocks.extend_from_slice(&vertex_heap);
        let verts_len = blocks.len() - verts_start;

        put_str(&mut directory, language);
        for (at, len) in [
            (ids_start, ids_len),
            (text_start, text_len),
            (verts_start, verts_len),
        ] {
            put_u64(&mut directory, at as u64);
            put_u64(&mut directory, len as u64);
        }
    }
    directory.extend_from_slice(&blocks);
    Ok(directory)
}

/// Read the names of `inputs` in `languages`. Nodes precede ways in a sorted
/// extract, but a tile cut can put a way before its nodes, so the node ids the
/// translated way needs are collected first and the nodes are read in a second
/// pass: memory stays proportional to translated streets, not to all nodes.
fn build_bytes(inputs: &[PathBuf], languages: &[String]) -> Result<Vec<u8>> {
    let languages = parse_languages(languages)?;
    let mut found = Found::new(languages.len());
    for input in inputs {
        let file = File::open(input).map_err(|source| Error::IoPath {
            action: "failed to open",
            path: input.clone(),
            source,
        })?;
        let reader = ElementReader::new(BufReader::with_capacity(READ_BUFFER_BYTES, file));
        let pass = reader.par_map_reduce(
            |element| {
                let mut found = Found::new(languages.len());
                count_element(element, &languages, &mut found);
                Pass1 { found }
            },
            || Pass1 {
                found: Found::new(languages.len()),
            },
            Pass1::merge,
        )?;
        found = found.merge(pass.found);
    }
    if found.wanted_nodes.is_empty() {
        found.sort();
        return encode(&languages, &found);
    }
    let wanted = std::sync::Arc::new(std::mem::take(&mut found.wanted_nodes));
    for input in inputs {
        let file = File::open(input).map_err(|source| Error::IoPath {
            action: "failed to open",
            path: input.clone(),
            source,
        })?;
        let reader = ElementReader::new(BufReader::with_capacity(READ_BUFFER_BYTES, file));
        reader.for_each(|element| {
            let (node, lon, lat) = match element {
                Element::Node(node) => (node.id(), node.decimicro_lon(), node.decimicro_lat()),
                Element::DenseNode(node) => (node.id(), node.decimicro_lon(), node.decimicro_lat()),
                _ => return,
            };
            if wanted.contains(&node) {
                NODE_COORDS.with(|coords| {
                    coords
                        .borrow_mut()
                        .insert(node, [coordinate(lon), coordinate(lat)]);
                });
            }
        })?;
    }
    found.sort();
    encode(&languages, &found)
}

/// Read the `name:<language>` names of `inputs` for `languages` and add them
/// to the sealed Pack. Does nothing without languages: the pass over the
/// inputs is the cost, and only a build that asked for it pays it.
pub fn add_to(sealed: &mut SealedPack, inputs: &[PathBuf], languages: &[String]) -> Result<()> {
    if languages.is_empty() {
        return Ok(());
    }
    let bytes = build_bytes(inputs, languages)?;
    sealed.add_section(SECTION, VERSION, &bytes)
}

#[derive(Debug)]
struct Table {
    language: String,
    /// Where each block's offsets start.
    offsets: [usize; 3],
    /// Where each block's heap starts.
    heap: [usize; 3],
    /// Number of records in each heap.
    entries: [usize; 3],
}

impl Table {
    /// The text at `index` of block `table` (0: ids, 1: street names, 2: vertices).
    fn text<'a>(&self, bytes: &'a [u8], table: usize, index: usize) -> Option<&'a [u8]> {
        let offset = self.offsets[table].checked_add(index.checked_mul(4)?)?;
        let from =
            self.heap[table].checked_add(usize::try_from(read_u32_le(bytes, offset)?).ok()?)?;
        let to = self.heap[table]
            .checked_add(usize::try_from(read_u32_le(bytes, offset.checked_add(4)?)?).ok()?)?;
        bytes.get(from..to)
    }
}

/// The names the Pack holds in other languages (see the module docs).
#[derive(Debug)]
pub struct Translations {
    bytes: Bytes,
    tables: Vec<Table>,
}

impl Translations {
    /// `Ok(None)`: a Pack built without languages.
    pub fn open(container: &Container) -> Result<Option<Arc<Self>>> {
        if !container.sections().contains_key(SECTION) {
            return Ok(None);
        }
        Self::parse(container.section(SECTION, VERSION)?).map(|found| Some(Arc::new(found)))
    }

    /// The section is external data: every offset is checked here, so a
    /// damaged section fails at open instead of at a request.
    fn parse(bytes: Bytes) -> Result<Self> {
        let truncated = || Error::Decode("the name translations are truncated");
        let mut cursor: &[u8] = &bytes;
        let count = get_u64(&mut cursor)?;
        let mut directory = Vec::new();
        for _ in 0..count {
            let language = get_str(&mut cursor)?.to_string();
            let mut ranges = [0usize; 3];
            let mut lengths = [0usize; 3];
            for (range, length) in ranges.iter_mut().zip(&mut lengths) {
                *range = usize::try_from(get_u64(&mut cursor)?)?;
                *length = usize::try_from(get_u64(&mut cursor)?)?;
            }
            directory.push((language, ranges, lengths));
        }
        if directory.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(Error::Decode("the name translations are not sorted"));
        }
        let base = bytes.len() - cursor.len();
        let mut tables = Vec::with_capacity(directory.len());
        for (language, ranges, lengths) in directory {
            let mut offsets = [0usize; 3];
            let mut heap = [0usize; 3];
            let mut counts = [0usize; 3];
            for table in 0..3 {
                let start = base.checked_add(ranges[table]).ok_or_else(truncated)?;
                let end = start.checked_add(lengths[table]).ok_or_else(truncated)?;
                let block = bytes.get(start..end).ok_or_else(truncated)?;
                let entries = usize::try_from(read_u64_le(block, 0).ok_or_else(truncated)?)?;
                counts[table] = entries;
                let texts = if table == 2 {
                    entries + 1
                } else {
                    entries * 2 + 1
                };
                let table_bytes = texts.checked_mul(4).ok_or_else(truncated)?;
                let at = table_bytes
                    .checked_add(8)
                    .filter(|at| *at <= block.len())
                    .ok_or_else(truncated)?;
                let text = if table == 2 {
                    None
                } else {
                    Some(
                        std::str::from_utf8(&block[at..])
                            .map_err(|_| Error::Decode("the name translations are not UTF-8"))?,
                    )
                };
                let count = if table == 2 {
                    entries + 1
                } else {
                    entries * 2 + 1
                };
                let mut previous = 0;
                for index in 0..count {
                    let cut = read_u32_le(block, 8 + index * 4).ok_or_else(truncated)?;
                    let cut = usize::try_from(cut)?;
                    if cut < previous || text.is_some_and(|text| !text.is_char_boundary(cut)) {
                        return Err(Error::Decode("the name translations are out of order"));
                    }
                    previous = cut;
                }
                if previous != block.len() - at {
                    return Err(truncated());
                }
                offsets[table] = start + 8;
                heap[table] = start + at;
            }
            tables.push(Table {
                language,
                offsets,
                heap,
                entries: counts,
            });
        }
        Ok(Self { bytes, tables })
    }

    /// The languages the Pack holds, ascending.
    pub fn languages(&self) -> Vec<&str> {
        self.tables
            .iter()
            .map(|table| table.language.as_str())
            .collect()
    }

    /// Whether the Pack was built with `language`, as [`parse_language`]
    /// writes it. One with no `name:<language>` anywhere is still held.
    pub fn holds(&self, language: &str) -> bool {
        self.table(language).is_some()
    }

    /// The `name:<language>` of an OSM object, only when it differs from the
    /// local name.
    pub fn by_id(&self, language: &str, object_type: ObjectType, object_id: i64) -> Option<&str> {
        let table = self.table(language)?;
        let bytes: &[u8] = &self.bytes;
        let key = key(object_type, object_id);
        let (mut low, mut high) = (0, count(table, 0));
        while low < high {
            let middle = low + (high - low) / 2;
            let text = table.text(bytes, 0, middle * 2)?;
            match text.cmp(key.as_bytes()) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => {
                    return std::str::from_utf8(table.text(bytes, 0, middle * 2 + 1)?).ok();
                }
            }
        }
        None
    }

    /// The `name:<language>` of the street nearest `lon, lat` whose local name
    /// is `name`. A miss keeps the local name: a street with no way in the
    /// extract, or a gap in its stored vertices, has no answer here.
    pub fn street_near(&self, language: &str, name: &str, lon: f64, lat: f64) -> Option<&str> {
        let table = self.table(language)?;
        let bytes: &[u8] = &self.bytes;
        // Lower bound: the first entry whose text is not less than `name`.
        // The earlier search stopped on the first equal entry it found, at
        // `middle`, and then scanned from `low`, so entries below `name` in
        // `[low, middle)` made the first `text != name` break the loop and
        // return nothing. A small table happened to land on the match; a
        // large one did not, which is why only the full build failed.
        let count = count(table, 1);
        let mut low = 0;
        let mut high = count;
        while low < high {
            let middle = low + (high - low) / 2;
            let text = table.text(bytes, 1, middle * 2)?;
            if text < name.as_bytes() {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let mut found: Option<(&str, f64)> = None;
        for index in low..count {
            let text = table.text(bytes, 1, index * 2)?;
            if text != name.as_bytes() {
                break;
            }
            let translated = std::str::from_utf8(table.text(bytes, 1, index * 2 + 1)?).ok()?;
            let Some(vertices) = table.text(bytes, 2, index) else {
                continue;
            };
            for vertex in vertices.as_chunks::<8>().0 {
                let x = i32::from_le_bytes(vertex[..4].try_into().expect("vertex"));
                let y = i32::from_le_bytes(vertex[4..].try_into().expect("vertex"));
                let distance = haversine_m(lon, lat, f64::from(x) / 1e7, f64::from(y) / 1e7);
                if distance > MAX_STREET_DISTANCE_M {
                    continue;
                }
                // Nearest vertex wins; equal distance takes the smaller text
                // so the answer cannot depend on the order of a build.
                if found.is_none_or(|(held, best)| (distance, translated) < (best, held)) {
                    found = Some((translated, distance));
                }
            }
        }
        found.map(|(translated, _)| translated)
    }


    fn table(&self, language: &str) -> Option<&Table> {
        let index = self
            .tables
            .binary_search_by(|table| table.language.as_str().cmp(language))
            .ok()?;
        self.tables.get(index)
    }
}

fn count(table: &Table, index: usize) -> usize {
    table.entries[index]
}

#[cfg(test)]
mod tests {
    use crate::builder::osm::test_pbf::{node, relation, way, write_pbf};

    use super::*;

    fn scratch_input(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "open-geocode-translations-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        (dir.join("in.osm.pbf"), dir)
    }

    fn owned(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn built(
        blocks: &[Vec<crate::builder::osm::test_pbf::TestElement>],
        languages: &[&str],
    ) -> Translations {
        let (input, dir) = scratch_input("roundtrip");
        write_pbf(&input, blocks);
        let bytes = build_bytes(&[input], &owned(languages)).expect("section");
        std::fs::remove_dir_all(dir).expect("cleanup");
        Translations::parse(Bytes::new(bytes)).expect("parsed")
    }

    fn street(
        id: i64,
        refs: &[i64],
        name: &'static str,
        translated: &'static str,
    ) -> crate::builder::osm::test_pbf::TestElement {
        way(
            id,
            refs,
            &[
                ("highway", "residential"),
                ("name", name),
                ("name:es", translated),
            ],
        )
    }

    fn examples() -> Vec<Vec<crate::builder::osm::test_pbf::TestElement>> {
        vec![
            vec![
                node(1, 39.47, -0.37, &[]),
                node(2, 39.4701, -0.3701, &[]),
                node(3, 39.4702, -0.3702, &[]),
                node(
                    10,
                    39.48,
                    -0.38,
                    &[
                        ("place", "locality"),
                        ("name", "València"),
                        ("name:es", "Valencia"),
                        ("name:fr", "Valence"),
                    ],
                ),
                node(
                    11,
                    39.48,
                    -0.38,
                    &[
                        ("highway", "crossing"),
                        ("name", "Pas"),
                        ("name:es", "Paso"),
                    ],
                ),
                node(12, 39.49, -0.39, &[]),
                node(13, 39.4901, -0.3901, &[]),
                node(14, 39.4910, -0.3910, &[]),
                node(15, 39.4911, -0.3911, &[]),
                node(99, 39.56, -0.44, &[]),
            ],
            vec![
                street(1, &[1, 2, 3], "Carrer Major", "Calle Mayor"),
                street(2, &[12, 13], "Carrer Major", "Calle Segunda"),
                way(
                    3,
                    &[14, 15],
                    &[
                        ("highway", "residential"),
                        ("name", "Carrer Major"),
                        ("name:es", "  Calle   Primera  "),
                    ],
                ),
                way(
                    4,
                    &[12, 98],
                    &[("highway", "residential"), ("name", " sense ")],
                ),
                way(
                    5,
                    &[],
                    &[
                        ("highway", "primary"),
                        ("name", "東京"),
                        ("name:ja_rm", "Tōkyō"),
                    ],
                ),
                way(
                    6,
                    &[],
                    &[
                        ("building", "yes"),
                        ("name", "Edifici"),
                        ("name:es", "Edificio"),
                    ],
                ),
                relation(
                    7,
                    &[(1, "way")],
                    &[
                        ("boundary", "administrative"),
                        ("admin_level", "4"),
                        ("name", "Comunitat Valenciana"),
                        ("name:es", "Comunidad  Valenciana"),
                    ],
                ),
                way(
                    8,
                    &[99, 98],
                    &[("highway", "residential"), ("name", "Missing node")],
                ),
            ],
        ]
    }

    #[test]
    fn a_language_tag_is_read_or_refused() {
        for (written, read) in [
            ("es", "es"),
            ("ca", "ca"),
            ("zh-Hans", "zh-Hans"),
            ("zh_Hans", "zh-Hans"),
            ("ja_rm", "ja-rm"),
        ] {
            assert_eq!(parse_language(written).as_deref(), Some(read), "{written}");
        }
        for refused in [
            "Es",
            "e",
            "abcd",
            "es-",
            "es--x",
            "",
            "-es",
            "es-toolongsub",
            "e s",
        ] {
            assert_eq!(parse_language(refused), None, "{refused:?}");
        }
    }

    #[test]
    fn languages_are_normalised_sorted_and_deduplicated() {
        assert_eq!(
            parse_languages(&owned(&["es", "ja_rm", "ca", "es"])).expect("languages"),
            ["ca", "es", "ja-rm"]
        );
        assert!(parse_languages(&owned(&["es", "Spanish"])).is_err());
    }

    #[test]
    fn an_object_is_translated_by_its_own_id() {
        let translations = built(&examples(), &["es", "ja_rm"]);
        assert_eq!(translations.languages(), ["es", "ja-rm"]);
        assert_eq!(
            translations.by_id("es", ObjectType::Way, 1),
            Some("Calle Mayor")
        );
        assert_eq!(
            translations.by_id("es", ObjectType::Node, 10),
            Some("Valencia")
        );
        assert_eq!(
            translations.by_id("es", ObjectType::Relation, 7),
            Some("Comunidad Valenciana")
        );
        assert_eq!(
            translations.by_id("ja-rm", ObjectType::Way, 5),
            Some("Tōkyō")
        );
        assert_eq!(translations.by_id("es", ObjectType::Way, 5), None);
        assert_eq!(translations.by_id("es", ObjectType::Way, 1_000), None);
        assert_eq!(translations.by_id("fr", ObjectType::Way, 1), None);
    }

    #[test]
    fn a_translation_equal_to_the_name_or_no_translation_is_absent() {
        let translations = built(&examples(), &["es", "ca"]);
        assert_eq!(translations.by_id("es", ObjectType::Way, 4), None);
        assert_eq!(translations.by_id("es", ObjectType::Way, 6), None);
        assert_eq!(translations.by_id("es", ObjectType::Node, 11), None);
        assert_eq!(translations.by_id("ca", ObjectType::Node, 10), None);
    }

    #[test]
    fn a_street_answer_finds_the_way_nearest_the_point() {
        let translations = built(&examples(), &["es"]);
        assert_eq!(
            translations.street_near("es", "Carrer Major", -0.3701, 39.4701),
            Some("Calle Mayor")
        );
        assert_eq!(
            translations.street_near("es", "Carrer Major", -0.3910, 39.4910),
            Some("Calle Primera")
        );
        assert_eq!(
            translations.street_near("es", "Carrer Major", -0.3901, 39.4901),
            Some("Calle Segunda")
        );
        assert_eq!(
            translations.street_near("es", "Carrer Major", 0.5, 40.0),
            None
        );
        assert_eq!(
            translations.street_near("es", "Carrer Sense", -0.37, 39.47),
            None
        );
    }

    #[test]
    fn a_same_named_street_in_another_town_does_not_lend_its_name() {
        let translations = built(&examples(), &["es"]);
        assert_eq!(
            translations.street_near("es", "Carrer Major", -0.3701, 39.4701),
            Some("Calle Mayor"),
            "town A: its own way has a translation"
        );
        assert_eq!(
            translations.street_near("es", "Carrer Major", -0.3901, 39.4901),
            Some("Calle Segunda"),
            "town B: its way has a translation too"
        );
        let mut blocks = examples();
        blocks[1].retain(|element| {
            !matches!(
                element,
                crate::builder::osm::test_pbf::TestElement::Way { id: 2 | 3, .. }
            )
        });
        let translations = built(&blocks, &["es"]);
        assert_eq!(
            translations.street_near("es", "Carrer Major", -0.3901, 39.4901),
            None,
            "town B: a same-named way in town A must not answer here"
        );
    }

    #[test]
    fn a_large_table_finds_a_match_that_sorts_after_many_others() {
        let mut block = Vec::new();
        for id in 1..=200 {
            block.push(node(id, 39.47 + id as f64 * 0.001, -0.37, &[]));
        }
        for id in 1..=200 {
            block.push(street(id + 1000, &[id], "Aaaa", "Primero"));
        }
        block.push(node(201, 39.48, -0.38, &[]));
        block.push(node(202, 39.4801, -0.3801, &[]));
        block.push(street(2001, &[201, 202], "Carrer Major", "Calle Mayor"));
        let translations = built(&[block], &["es"]);
        assert_eq!(
            translations.street_near("es", "Carrer Major", -0.38, 39.48),
            Some("Calle Mayor")
        );
    }
    #[test]
    fn a_street_whose_node_is_missing_from_the_extract_does_not_panic() {
        let translations = built(&examples(), &["es"]);
        assert!(
            translations
                .street_near("es", "Missing node", -0.44, 39.56)
                .is_none()
        );
        assert_eq!(translations.by_id("es", ObjectType::Way, 8), None);
    }

    #[test]
    fn a_damaged_section_is_an_error_not_a_panic() {
        let translations = built(&examples(), &["es"]);
        assert_eq!(
            translations.by_id("es", ObjectType::Way, 1),
            Some("Calle Mayor")
        );
        let (input, dir) = scratch_input("damaged");
        write_pbf(
            &input,
            &[vec![node(
                1,
                0.0,
                0.0,
                &[("place", "town"), ("name", "Aaa"), ("name:es", "Bbb")],
            )]],
        );
        let bytes = build_bytes(&[input], &owned(&["es"])).expect("section");
        std::fs::remove_dir_all(dir).expect("cleanup");
        assert!(Translations::parse(Bytes::new(bytes.clone())).is_ok());
        for cut in 0..bytes.len() {
            assert!(
                Translations::parse(Bytes::new(bytes[..cut].to_vec())).is_err(),
                "cut at {cut}"
            );
        }
        let mut lying = bytes;
        let heap = lying.len() - 6;
        lying[heap] = 0xff;
        assert!(Translations::parse(Bytes::new(lying)).is_err());
    }
}
