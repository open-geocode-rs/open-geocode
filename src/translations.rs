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
//!               tag varint-length text | ids (offset, length) |
//!               street text (offset, length) | street verts (offset, length),
//!               every value a varint; offsets count from the first block
//! ids         entries u64 | 2 * entries + 1 offsets u32 | heap,
//!             texts alternating key "node|way|relation:id" and name
//! street text entries u64 | 2 * entries + 1 offsets u32 | heap, texts
//!             alternating local name and translation
//! street verts entries u64 | entries + 1 offsets u32 | heap, each entry
//!             the way as lon,lat i32 LE pairs, simplified to within 20 m
//!             and at most 64 pairs; a pair of i32::MIN marks a missing node
//!             and ends a run, no segment crosses it; entries in the same
//!             order as street text
//! ```

use std::{
    collections::HashSet,
    fs::File,
    io::{self, BufWriter, Write},
    iter::Peekable,
    path::{Path, PathBuf},
    sync::Arc,
};

use osmpbf::Element;

use crate::{
    builder::{
        osm::{
            locations::{self, NodeSet, Slots},
            pbf::for_each_block,
            street,
            tags::{OsmTags, is_language_tag, without_pasted_tags},
        },
        progress::{BuildStage, Progress},
    },
    container::{Bytes, Container},
    error::{Error, Result},
    extsort::{ExternalSorter, Scratch, ScratchFile, Sorted, Spill, Spool, SpoolReader},
    memory::Budget,
    pack::SealedPack,
    record::ObjectType,
    spatial_index::haversine_m,
    util::{
        codec::{
            get_i64, get_str, get_string, get_u32, get_u64, put_i64, put_str, put_u64, read_u32_le,
            read_u64_le,
        },
        text::collapse_whitespace,
    },
};

pub const SECTION: &str = "names/lang";
const VERSION: u32 = 3;
const IO_BUFFER_BYTES: usize = 1 << 20;
/// The most vertices kept for one way, run breaks included.
const MAX_VERTICES: usize = 64;
/// How far the kept line may stray from the way: far below the distance a
/// house sits at, so simplifying never changes which way is nearest.
const SIMPLIFY_TOLERANCE_M: f64 = 20.0;
/// Marks a missing node: no coordinate is this far west and south.
const BREAK: [i32; 2] = [i32::MIN; 2];

// A house usually sits within 1.5 km of its way.
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

/// A translated name, keyed by the OSM object it belongs to.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct IdRow {
    language: u32,
    key: String,
    value: String,
}

impl Spill for IdRow {
    fn encode(&self, out: &mut Vec<u8>) {
        put_u64(out, u64::from(self.language));
        put_str(out, &self.key);
        put_str(out, &self.value);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(Self {
            language: get_u32(input)?,
            key: get_string(input)?,
            value: get_string(input)?,
        })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.key.len() + self.value.len()
    }
}

/// A street way as the scan saw it: its nodes are ids, not yet coordinates.
/// `translated` holds one text per language, empty where the way has none.
#[derive(Debug)]
struct PendingStreet {
    name: String,
    translated: Vec<String>,
    refs: Vec<i64>,
}

impl Spill for PendingStreet {
    fn encode(&self, out: &mut Vec<u8>) {
        put_str(out, &self.name);
        put_u64(out, self.translated.len() as u64);
        for text in &self.translated {
            put_str(out, text);
        }
        put_u64(out, self.refs.len() as u64);
        for id in &self.refs {
            put_i64(out, *id);
        }
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let name = get_string(input)?;
        let languages = usize::try_from(get_u64(input)?)?;
        let mut translated = Vec::with_capacity(languages.min(MAX_VERTICES));
        for _ in 0..languages {
            translated.push(get_string(input)?);
        }
        let count = usize::try_from(get_u64(input)?)?;
        let mut refs = Vec::with_capacity(count.min(1 << 10));
        for _ in 0..count {
            refs.push(get_i64(input)?);
        }
        Ok(Self {
            name,
            translated,
            refs,
        })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.name.len()
            + self
                .translated
                .iter()
                .map(|text| text.len() + std::mem::size_of::<String>())
                .sum::<usize>()
            + self.refs.len() * 8
    }
}

/// A street way of one language with the kept vertices of its line.
/// `translated` is empty when the way has no other name in the language: the
/// row still competes on distance, so a nearer way with no translation is
/// not skipped for a farther one that has it.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StreetRow {
    language: u32,
    name: String,
    translated: String,
    vertices: Vec<[i32; 2]>,
}

impl Spill for StreetRow {
    fn encode(&self, out: &mut Vec<u8>) {
        put_u64(out, u64::from(self.language));
        put_str(out, &self.name);
        put_str(out, &self.translated);
        put_u64(out, self.vertices.len() as u64);
        for [lon, lat] in &self.vertices {
            put_i64(out, i64::from(*lon));
            put_i64(out, i64::from(*lat));
        }
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let language = get_u32(input)?;
        let name = get_string(input)?;
        let translated = get_string(input)?;
        let count = usize::try_from(get_u64(input)?)?;
        let mut vertices = Vec::with_capacity(count.min(MAX_VERTICES));
        for _ in 0..count {
            vertices.push([
                i32::try_from(get_i64(input)?)?,
                i32::try_from(get_i64(input)?)?,
            ]);
        }
        Ok(Self {
            language,
            name,
            translated,
            vertices,
        })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.name.len()
            + self.translated.len()
            + self.vertices.len() * 8
    }
}

/// What one PBF block holds that the section keeps.
#[derive(Default)]
struct BlockNames {
    offset: u64,
    has_nodes: bool,
    ids: Vec<IdRow>,
    streets: Vec<PendingStreet>,
}

/// One element's names, and the node ids needed to place a translated street.
/// A place or boundary has its own source id; a street also answers to its
/// vertices so an address whose way is not in the extract still gets its name.
fn count_element(element: Element<'_>, languages: &[String], out: &mut BlockNames) {
    let (object_type, object_id, tags, refs) = match element {
        Element::Node(node) => (
            ObjectType::Node,
            node.id(),
            node.tags().collect::<Vec<_>>(),
            None,
        ),
        Element::DenseNode(node) => (
            ObjectType::Node,
            node.id(),
            node.tags().collect::<Vec<_>>(),
            None,
        ),
        Element::Way(way) => (
            ObjectType::Way,
            way.id(),
            way.tags().collect::<Vec<_>>(),
            Some(way.refs().collect::<Vec<_>>()),
        ),
        Element::Relation(relation) => (
            ObjectType::Relation,
            relation.id(),
            relation.tags().collect::<Vec<_>>(),
            None,
        ),
    };
    // Nearly every element lacks a name or a kind this section keeps; skip
    // those before an owned tag map is built for each.
    let has = |wanted: &[&str]| tags.iter().any(|(key, _)| wanted.contains(key));
    if !has(&["name"]) || !has(&["highway", "place", "boundary", "admin_level"]) {
        return;
    }
    let map = tags
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let Some(name) = map.cleaned("name") else {
        return;
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
    let id = key(object_type, object_id);
    for (index, translated) in &translations {
        out.ids.push(IdRow {
            language: *index as u32,
            key: id.clone(),
            value: translated.clone(),
        });
    }
    if !street {
        return;
    }
    // Every language keeps a row so a nearer untranslated way still wins; rows
    // no translated street shares a name with are dropped after the scan.
    let mut translated = vec![String::new(); languages.len()];
    for (index, value) in translations {
        translated[index] = value;
    }
    out.streets.push(PendingStreet {
        name,
        translated,
        refs: refs.unwrap_or_default(),
    });
}

/// The line of a way as stored: the coordinates of its nodes in order, cut
/// into runs where a node is missing from the extract, each run simplified.
/// Joining across a gap would put a segment where there may be no road.
fn line_vertices(coords: &[Option<[i32; 2]>]) -> Vec<[i32; 2]> {
    let runs = coords
        .split(Option::is_none)
        .map(|run| run.iter().flatten().copied().collect::<Vec<_>>())
        .filter(|run| !run.is_empty())
        .collect::<Vec<_>>();
    let mut tolerance = SIMPLIFY_TOLERANCE_M;
    loop {
        let mut line = Vec::new();
        for run in &runs {
            if !line.is_empty() {
                line.push(BREAK);
            }
            line.extend(simplify(run, tolerance));
        }
        // A way still too long at any tolerance is one with many gaps.
        if line.len() <= MAX_VERTICES || tolerance > 1e7 {
            return line;
        }
        tolerance *= 2.0;
    }
}

/// Douglas-Peucker: the points of `run` that keep the line within
/// `tolerance_m` of the original, first and last included.
fn simplify(run: &[[i32; 2]], tolerance_m: f64) -> Vec<[i32; 2]> {
    if run.len() <= 2 {
        return run.to_vec();
    }
    let points = run
        .iter()
        .map(|[lon, lat]| [f64::from(*lon) / 1e7, f64::from(*lat) / 1e7])
        .collect::<Vec<_>>();
    let mut keep = vec![false; run.len()];
    keep[0] = true;
    keep[run.len() - 1] = true;
    let mut stack = vec![(0, run.len() - 1)];
    while let Some((first, last)) = stack.pop() {
        let mut farthest = (0.0, first);
        for at in first + 1..last {
            let distance = crate::spatial_index::point_segment_distance_m(
                points[at][0],
                points[at][1],
                points[first],
                points[last],
            );
            if distance > farthest.0 {
                farthest = (distance, at);
            }
        }
        if farthest.0 > tolerance_m {
            keep[farthest.1] = true;
            stack.push((first, farthest.1));
            stack.push((farthest.1, last));
        }
    }
    run.iter()
        .zip(keep)
        .filter_map(|(point, keep)| keep.then_some(*point))
        .collect()
}

/// How far `lon, lat` is from the stored line, in metres: the nearest
/// segment, or the nearest vertex of a run that is a single node.
fn distance_to_line(vertices: &[u8], lon: f64, lat: f64) -> f64 {
    let mut best = f64::INFINITY;
    let mut previous: Option<[f64; 2]> = None;
    let mut run = 0usize;
    let lone = |previous: Option<[f64; 2]>, run: usize, best: f64| match previous {
        Some(point) if run == 1 => best.min(haversine_m(lon, lat, point[0], point[1])),
        _ => best,
    };
    for chunk in vertices.as_chunks::<8>().0 {
        let [a, b, c, d, e, f, g, h] = *chunk;
        let (x, y) = (
            i32::from_le_bytes([a, b, c, d]),
            i32::from_le_bytes([e, f, g, h]),
        );
        if [x, y] == BREAK {
            best = lone(previous, run, best);
            previous = None;
            run = 0;
            continue;
        }
        let point = [f64::from(x) / 1e7, f64::from(y) / 1e7];
        if let Some(start) = previous {
            best = best.min(crate::spatial_index::point_segment_distance_m(
                lon, lat, start, point,
            ));
        }
        previous = Some(point);
        run += 1;
    }
    lone(previous, run, best)
}

fn too_big(language: &str) -> Error {
    Error::OutOfResources(format!(
        "the {language} names are more than 4 GiB of text; keep fewer languages"
    ))
}

/// What a build lends the section: the pool its sorters spill under, the
/// scratch they spill to, and where its progress goes.
pub(crate) struct Env<'a> {
    pub scratch: &'a Arc<Scratch>,
    pub memory: &'a Arc<Budget>,
    pub budget: usize,
    pub progress: &'a Progress,
}

fn scratch_file(scratch: &Arc<Scratch>, label: &str) -> Result<ScratchFile> {
    let path = scratch.new_file(label);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(Error::Decode("scratch file name is not UTF-8"))?;
    ScratchFile::create(scratch, label, name)
}

fn buffered(file: ScratchFile) -> BufWriter<ScratchFile> {
    BufWriter::with_capacity(IO_BUFFER_BYTES, file)
}

fn unbuffered(writer: BufWriter<ScratchFile>) -> Result<ScratchFile> {
    writer
        .into_inner()
        .map_err(|error| Error::Io(error.into_error()))
}

fn append(into: &mut impl Write, file: &ScratchFile) -> Result<()> {
    let mut source = File::open(file.path()).map_err(|source| Error::IoPath {
        action: "failed to open",
        path: file.path().to_path_buf(),
        source,
    })?;
    io::copy(&mut source, into)?;
    Ok(())
}

/// One table of one language, written as it is read: its offsets and its
/// heap go to files of their own, and [`TableWriter::finish`] lays them
/// after the entry count the section's layout asks for first.
struct TableWriter {
    language: String,
    offsets: BufWriter<ScratchFile>,
    heap: BufWriter<ScratchFile>,
    heap_bytes: u64,
    cuts: u64,
    entries: u64,
}

impl TableWriter {
    fn new(scratch: &Arc<Scratch>, language: &str) -> Result<Self> {
        let mut offsets = buffered(scratch_file(scratch, "translation-offsets")?);
        offsets.write_all(&0u32.to_le_bytes())?;
        Ok(Self {
            language: language.to_string(),
            offsets,
            heap: buffered(scratch_file(scratch, "translation-heap")?),
            heap_bytes: 0,
            cuts: 0,
            entries: 0,
        })
    }

    /// Add `bytes` to the heap and end a text there.
    fn cut(&mut self, bytes: &[u8]) -> Result<()> {
        self.heap.write_all(bytes)?;
        self.heap_bytes += bytes.len() as u64;
        let at = u32::try_from(self.heap_bytes).map_err(|_| too_big(&self.language))?;
        self.offsets.write_all(&at.to_le_bytes())?;
        self.cuts += 1;
        Ok(())
    }

    fn entry(&mut self) {
        self.entries += 1;
    }

    /// Append the table to `blocks`; its length in bytes.
    fn finish(self, blocks: &mut impl Write) -> Result<u64> {
        let offsets = unbuffered(self.offsets)?;
        let heap = unbuffered(self.heap)?;
        blocks.write_all(&self.entries.to_le_bytes())?;
        append(blocks, &offsets)?;
        append(blocks, &heap)?;
        Ok(8 + (self.cuts + 1) * 4 + self.heap_bytes)
    }
}

/// Hand `each` the rows of `language` that come next in `rows`, sorted by
/// language first.
fn lane<T: Spill + Ord>(
    rows: &mut Peekable<Sorted<T>>,
    language: u32,
    of: impl Fn(&T) -> u32,
    mut each: impl FnMut(T) -> Result<()>,
) -> Result<()> {
    loop {
        match rows.peek() {
            Some(Ok(row)) if of(row) == language => {}
            Some(Err(_)) => {
                return match rows.next() {
                    Some(Err(error)) => Err(error),
                    _ => unreachable!("peeked an error"),
                };
            }
            _ => return Ok(()),
        }
        each(rows.next().expect("peeked")?)?;
    }
}

/// Write the section from the sorted rows, one language after another.
fn write_section(
    languages: &[String],
    ids: Sorted<IdRow>,
    streets: Sorted<StreetRow>,
    scratch: &Arc<Scratch>,
) -> Result<ScratchFile> {
    let mut ids = ids.peekable();
    let mut streets = streets.peekable();
    let mut blocks = buffered(scratch_file(scratch, "translation-blocks")?);
    let mut position = 0u64;
    let mut directory = Vec::new();
    put_u64(&mut directory, languages.len() as u64);
    for (index, language) in languages.iter().enumerate() {
        let index = index as u32;
        let mut placed = [(0u64, 0u64); 3];

        let mut names = TableWriter::new(scratch, language)?;
        let mut last: Option<String> = None;
        lane(
            &mut ids,
            index,
            |row| row.language,
            |row| {
                // One object can arrive twice, from inputs that overlap.
                if last.as_deref() == Some(row.key.as_str()) {
                    return Ok(());
                }
                names.cut(row.key.as_bytes())?;
                names.cut(row.value.as_bytes())?;
                names.entry();
                last = Some(row.key);
                Ok(())
            },
        )?;
        let len = names.finish(&mut blocks)?;
        placed[0] = (position, len);
        position += len;

        let mut text = TableWriter::new(scratch, language)?;
        let mut vertices = TableWriter::new(scratch, language)?;
        let mut bytes = Vec::new();
        lane(
            &mut streets,
            index,
            |row| row.language,
            |row| {
                text.cut(row.name.as_bytes())?;
                text.cut(row.translated.as_bytes())?;
                text.entry();
                bytes.clear();
                for [lon, lat] in &row.vertices {
                    bytes.extend_from_slice(&lon.to_le_bytes());
                    bytes.extend_from_slice(&lat.to_le_bytes());
                }
                vertices.cut(&bytes)?;
                vertices.entry();
                Ok(())
            },
        )?;
        let len = text.finish(&mut blocks)?;
        placed[1] = (position, len);
        position += len;
        let len = vertices.finish(&mut blocks)?;
        placed[2] = (position, len);
        position += len;

        put_str(&mut directory, language);
        for (at, len) in placed {
            put_u64(&mut directory, at);
            put_u64(&mut directory, len);
        }
    }
    let blocks = unbuffered(blocks)?;
    let mut section = buffered(scratch_file(scratch, "translations")?);
    section.write_all(&directory)?;
    append(&mut section, &blocks)?;
    drop(blocks);
    unbuffered(section)
}

/// One input after its names pass: the blocks that hold nodes, and every
/// named street way, its nodes still ids. Nodes precede ways in a sorted
/// extract but a tile cut can put a way before its nodes, so the node ids are
/// collected first and the node blocks read in a second pass, through the
/// store the build's own node pass uses: it holds the coordinates in the
/// pool or maps them from scratch.
struct Scanned<'a> {
    input: &'a Path,
    node_blobs: Vec<u64>,
    pending: SpoolReader<PendingStreet>,
}

/// Read one input's names into the sorters. Streets wait in a spool: which of
/// them matter is known only once every input has been read.
fn scan_input<'a>(
    input: &'a Path,
    languages: &[String],
    env: &Env<'_>,
    ids: &mut ExternalSorter<IdRow>,
    translated_names: &mut [HashSet<String>],
) -> Result<Scanned<'a>> {
    let mut node_blobs = Vec::new();
    let mut pending = Spool::<PendingStreet>::create(env.scratch, "translation-pending")?;
    for_each_block(
        input,
        env.progress,
        BuildStage::Translations,
        None,
        |offset, block| {
            let mut names = BlockNames {
                offset,
                ..BlockNames::default()
            };
            for element in block.elements() {
                names.has_nodes |= matches!(element, Element::Node(_) | Element::DenseNode(_));
                count_element(element, languages, &mut names);
            }
            Ok(names)
        },
        |names| {
            if names.has_nodes {
                node_blobs.push(names.offset);
            }
            for row in names.ids {
                ids.push(row)?;
            }
            for street in names.streets {
                for (index, value) in street.translated.iter().enumerate() {
                    if !value.is_empty() {
                        translated_names[index].insert(street.name.clone());
                    }
                }
                pending.write(&street)?;
            }
            Ok(())
        },
    )?;
    Ok(Scanned {
        input,
        node_blobs,
        pending: pending.into_reader()?,
    })
}

/// The languages whose row of `street` can change an answer: those with a
/// translation, and those sharing a name with a translated street of the same
/// language, which must keep their local name when they are nearer. Any other
/// row can never change the answer, so it stores no vertices.
fn kept_languages<'a>(
    street: &'a PendingStreet,
    translated_names: &'a [HashSet<String>],
) -> impl Iterator<Item = usize> + 'a {
    (0..street.translated.len()).filter(move |language| {
        !street.translated[*language].is_empty()
            || translated_names[*language].contains(&street.name)
    })
}

/// Place the streets of one input that some language keeps.
fn place_input(
    scanned: Scanned<'_>,
    env: &Env<'_>,
    translated_names: &[HashSet<String>],
    streets: &mut ExternalSorter<StreetRow>,
) -> Result<()> {
    let nodes = NodeSet::new(env.memory);
    let mut kept = Spool::<PendingStreet>::create(env.scratch, "translation-kept")?;
    for street in scanned.pending {
        let street = street?;
        if kept_languages(&street, translated_names).next().is_none() {
            continue;
        }
        nodes.mark(&street.refs)?;
        kept.write(&street)?;
    }
    if kept.written() == 0 {
        env.progress.start(BuildStage::TranslationNodes, Some(0))?;
        env.progress.finish(BuildStage::TranslationNodes);
        return Ok(());
    }
    let slots = Slots::new(nodes.len(), true, env.memory, env.scratch)?;
    let (locations, _) = locations::load(
        scanned.input,
        &scanned.node_blobs,
        nodes,
        &[],
        slots,
        env.progress,
        BuildStage::TranslationNodes,
    )?;
    for street in kept.into_reader()? {
        let street = street?;
        let coords = street
            .refs
            .iter()
            .map(|id| locations.get(*id).map(|at| [at.lon_e7, at.lat_e7]))
            .collect::<Vec<_>>();
        let vertices = line_vertices(&coords);
        for language in kept_languages(&street, translated_names) {
            streets.push(StreetRow {
                language: language as u32,
                name: street.name.clone(),
                translated: street.translated[language].clone(),
                vertices: vertices.clone(),
            })?;
        }
    }
    Ok(())
}

/// Read the names of `inputs` in `languages` into a section file.
fn build_section(inputs: &[PathBuf], languages: &[String], env: &Env<'_>) -> Result<ScratchFile> {
    let languages = parse_languages(languages)?;
    let share = (env.budget / 4).max(1);
    let mut ids = ExternalSorter::<IdRow>::new(env.scratch, "translation-ids", env.memory, share);
    let mut streets =
        ExternalSorter::<StreetRow>::new(env.scratch, "translation-streets", env.memory, share);
    let mut translated_names = vec![HashSet::new(); languages.len()];
    let mut scanned = Vec::with_capacity(inputs.len());
    for input in inputs {
        scanned.push(scan_input(
            input,
            &languages,
            env,
            &mut ids,
            &mut translated_names,
        )?);
    }
    for input in scanned {
        place_input(input, env, &translated_names, &mut streets)?;
    }
    write_section(&languages, ids.finish()?, streets.finish()?, env.scratch)
}

/// Read the `name:<language>` names of `inputs` for `languages` and add them
/// to the sealed Pack, returning the section's size. Without languages the
/// stage runs empty and nothing is added: the passes over the inputs are the
/// cost, and only a build that asked for them pays it.
pub(crate) fn add_to(
    sealed: &mut SealedPack,
    inputs: &[PathBuf],
    languages: &[String],
    env: &Env<'_>,
) -> Result<Option<u64>> {
    if languages.is_empty() {
        for stage in [BuildStage::Translations, BuildStage::TranslationNodes] {
            env.progress.start(stage, Some(0))?;
            env.progress.finish(stage);
        }
        return Ok(None);
    }
    let section = build_section(inputs, languages, env)?;
    let bytes = section.bytes();
    sealed.add_section_files(&[(SECTION, VERSION, section.path().to_path_buf())])?;
    Ok(Some(bytes))
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
                let offsets_count = if table == 2 {
                    entries.checked_add(1).ok_or_else(truncated)?
                } else {
                    entries
                        .checked_mul(2)
                        .and_then(|doubled| doubled.checked_add(1))
                        .ok_or_else(truncated)?
                };
                let table_bytes = offsets_count.checked_mul(4).ok_or_else(truncated)?;
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
                let mut previous = 0;
                for index in 0..offsets_count {
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
            if counts[1] != counts[2] {
                return Err(Error::Decode("the name translations are out of order"));
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
        // Lower bound of `name` among sorted street rows.

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

        let mut found: Option<(Option<&str>, f64)> = None;
        for index in low..count {
            let text = table.text(bytes, 1, index * 2)?;
            if text != name.as_bytes() {
                break;
            }
            let translated_raw = std::str::from_utf8(table.text(bytes, 1, index * 2 + 1)?).ok()?;
            let translated = if translated_raw.is_empty() {
                None
            } else {
                Some(translated_raw)
            };
            let Some(vertices) = table.text(bytes, 2, index) else {
                continue;
            };
            let distance = distance_to_line(vertices, lon, lat);
            if distance > MAX_STREET_DISTANCE_M {
                continue;
            }
            if found.is_none_or(|(held, best)| {
                (distance, translated.unwrap_or("")) < (best, held.unwrap_or(""))
            }) {
                found = Some((translated, distance));
            }
        }
        found.and_then(|(translated, _)| translated)
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

    struct Quiet;

    impl crate::builder::BuildObserver for Quiet {
        fn started(&self, _: BuildStage, _: Option<u64>) {}
        fn advanced(&self, _: u64) {}
        fn finished(&self, _: BuildStage) {}
        fn warned(&self, _: &str) {}
    }

    /// The section a build of `input` writes, read back from scratch.
    fn build_bytes(input: &Path, languages: &[String], dir: &Path) -> Result<Vec<u8>> {
        build_bytes_with(input, languages, dir, 1 << 20, &Budget::unlimited()).map(|built| built.0)
    }

    /// As [`build_bytes`], with the sorters' share and the pool set, so a
    /// test can make every buffer spill and every node store map a file.
    /// Also returns what the sorters wrote to scratch.
    fn build_bytes_with(
        input: &Path,
        languages: &[String],
        dir: &Path,
        budget: usize,
        memory: &Arc<Budget>,
    ) -> Result<(
        Vec<u8>,
        std::collections::BTreeMap<String, crate::extsort::Traffic>,
    )> {
        let scratch = Scratch::create(dir.join("scratch"))?;
        let progress = Progress::new(
            Arc::new(Quiet),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        let env = Env {
            scratch: &scratch,
            memory,
            budget,
            progress: &progress,
        };
        let section = build_section(&[input.to_path_buf()], languages, &env)?;
        Ok((std::fs::read(section.path())?, scratch.traffic()))
    }

    fn built(
        blocks: &[Vec<crate::builder::osm::test_pbf::TestElement>],
        languages: &[&str],
    ) -> Translations {
        let (input, dir) = scratch_input("roundtrip");
        write_pbf(&input, blocks);
        let bytes = build_bytes(&input, &owned(languages), &dir).expect("section");
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
                    &[
                        ("highway", "residential"),
                        ("name", "Missing node"),
                        ("name:es", "Nodo ausente"),
                    ],
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
        assert_eq!(
            translations.street_near("es", "Missing node", -0.44, 39.56),
            Some("Nodo ausente")
        );
        assert_eq!(
            translations.by_id("es", ObjectType::Way, 8),
            Some("Nodo ausente")
        );
    }

    #[test]
    fn the_nearest_way_wins_even_when_it_has_no_translation() {
        // Way A has no name:es. Way B, about 800 m from the query and inside
        // the 1.5 km limit, has one. A query on A must not borrow B's.
        let block = vec![
            node(1, 39.47, -0.37, &[]),
            node(2, 39.4701, -0.37, &[]),
            node(3, 39.4772, -0.37, &[]),
            node(4, 39.4773, -0.37, &[]),
            way(
                1,
                &[1, 2],
                &[("highway", "residential"), ("name", "Carrer Major")],
            ),
            street(2, &[3, 4], "Carrer Major", "Calle Mayor"),
        ];
        let translations = built(&[block], &["es"]);
        assert_eq!(
            translations.street_near("es", "Carrer Major", -0.37, 39.47005),
            None,
            "the nearest way has no name:es; it must not borrow a farther translation"
        );
    }

    #[test]
    fn a_way_translated_in_one_language_still_blocks_the_others() {
        let block = vec![
            node(1, 39.47, -0.37, &[]),
            node(2, 39.4701, -0.37, &[]),
            node(3, 39.4772, -0.37, &[]),
            node(4, 39.4773, -0.37, &[]),
            // Nearest: es only.
            street(1, &[1, 2], "Carrer Major", "Calle Mayor"),
            // Farther: ca only.
            way(
                2,
                &[3, 4],
                &[
                    ("highway", "residential"),
                    ("name", "Carrer Major"),
                    ("name:ca", "Carrer Gran"),
                ],
            ),
        ];
        let translations = built(&[block], &["es", "ca"]);
        assert_eq!(
            translations.street_near("es", "Carrer Major", -0.37, 39.47005),
            Some("Calle Mayor")
        );
        assert_eq!(
            translations.street_near("ca", "Carrer Major", -0.37, 39.47005),
            None,
            "the nearest way has no name:ca; it must not borrow the farther one's"
        );
    }

    #[test]
    fn proximity_is_measured_to_the_way_not_only_its_vertices() {
        // A straight 4 km way: both endpoints are 2 km from the midpoint,
        // beyond the 1.5 km limit.
        let block = vec![
            node(1, 39.47, -0.37, &[]),
            node(2, 39.506, -0.37, &[]),
            street(1, &[1, 2], "Carrer Major", "Calle Mayor"),
        ];
        let translations = built(&[block], &["es"]);
        assert_eq!(
            translations.street_near("es", "Carrer Major", -0.37, 39.488),
            Some("Calle Mayor"),
            "the midpoint lies on the way even though every vertex is far"
        );
    }

    #[test]
    fn a_missing_node_does_not_join_the_nodes_on_either_side() {
        // Node 2 is not in the extract. Joining 1 to 3 would put a street
        // through the midpoint, 2 km from both nodes.
        let block = vec![
            node(1, 39.47, -0.37, &[]),
            node(3, 39.506, -0.37, &[]),
            street(1, &[1, 2, 3], "Carrer Major", "Calle Mayor"),
        ];
        let translations = built(&[block], &["es"]);
        assert_eq!(
            translations.street_near("es", "Carrer Major", -0.37, 39.488),
            None,
            "no segment crosses the gap"
        );
        assert_eq!(
            translations.street_near("es", "Carrer Major", -0.37, 39.4701),
            Some("Calle Mayor"),
            "a node that is there still places the street"
        );
    }

    #[test]
    fn a_bend_is_kept_so_the_chord_across_it_is_not_a_street() {
        // 50 nodes north, then 50 east, 0.5 km apart: an L whose corner lies
        // between the eight evenly spaced nodes an earlier format kept.
        let mut block = Vec::new();
        let mut refs = Vec::new();
        for index in 0..100i64 {
            let (lat, lon) = if index < 50 {
                (39.47 + index as f64 * 0.005, -0.37)
            } else {
                (39.47 + 49.0 * 0.005, -0.37 + (index - 49) as f64 * 0.0065)
            };
            block.push(node(index + 1, lat, lon, &[]));
            refs.push(index + 1);
        }
        block.push(street(1, &refs, "Carrer Major", "Calle Mayor"));
        let translations = built(&[block], &["es"]);
        // The middle of the chord from node 43 to node 57, 1.9 km from the L.
        assert_eq!(
            translations.street_near("es", "Carrer Major", -0.34725, 39.6975),
            None
        );
        assert_eq!(
            translations.street_near("es", "Carrer Major", -0.37, 39.6975),
            Some("Calle Mayor")
        );
    }

    #[test]
    fn a_line_keeps_its_bends_and_drops_the_points_between() {
        let metres = |north: f64| (north / 111_000.0 * 1e7) as i32;
        let corner = [0, metres(5000.0)];
        let mut coords = (0..=50)
            .map(|step| Some([0, metres(f64::from(step) * 100.0)]))
            .collect::<Vec<_>>();
        coords.extend((1..=50).map(|step| Some([metres(f64::from(step) * 100.0), corner[1]])));
        let line = line_vertices(&coords);
        assert_eq!(line.len(), 3, "start, corner, end: {line:?}");
        assert_eq!(line[1], corner);

        let gap = [Some([0, 0]), None, Some([0, metres(1000.0)])];
        assert_eq!(
            line_vertices(&gap),
            vec![[0, 0], BREAK, [0, metres(1000.0)]]
        );
    }

    #[test]
    fn a_build_that_spills_writes_the_same_section_as_one_in_memory() {
        let section = |budget: usize, memory: Arc<Budget>| {
            let (input, dir) = scratch_input("spill");
            write_pbf(&input, &examples());
            let built = build_bytes_with(&input, &owned(&["es", "fr"]), &dir, budget, &memory)
                .expect("section");
            std::fs::remove_dir_all(dir).expect("cleanup");
            built
        };
        let (roomy, _) = section(1 << 20, Budget::unlimited());
        let (starved, traffic) = section(1, Budget::new(0));
        for label in ["translation-ids", "translation-streets"] {
            assert!(
                traffic.get(label).is_some_and(|owner| owner.runs > 1),
                "{label} must have spilled more than one run: {traffic:?}"
            );
        }
        assert_eq!(roomy, starved);
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
        let bytes = build_bytes(&input, &owned(&["es"]), &dir).expect("section");
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
