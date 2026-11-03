//! The structured index: sections of a Pack that answer the common query, a house
//! number on a named street, without a text search. Built from the sealed
//! Pack in one pass over its text index, and added to it before it is
//! published (see [`add_to`]).
//!
//! Three maps. `streets` from a street and country to the street's
//! documents, sorted by map cell (see [`cell`]), so the documents inside a
//! place are a few ranges of the list. `places` from a place name and
//! country to its footprint: the cells where the Pack's records use that
//! name, from their own locality, region and postcode values, every name of
//! every area of their chain, and the areas themselves. `cell streets` from
//! a cell to the streets in it, for a misspelt street found near the places
//! a query names. Every key is written as the parser writes a query (see
//! [`crate::search::TextSearcher::structured_keys`]). The country goes last,
//! so a query that names none reads every country with one range scan.

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashSet},
    fs,
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use fst::{Automaton, IntoStreamer, Map, MapBuilder, Streamer};
use levenshtein_automata::{DFA, Distance, SINK_STATE};
use memmap2::Mmap;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use tantivy::DocId;

use crate::{
    container::{Bytes, Container},
    error::{Error, Result},
    pack,
    parse::lattice::automaton_builder,
    search::TextSearcher,
};

/// Between the parts of a key. No parser word holds a control character.
pub(crate) const SEPARATOR: u8 = 0x1f;
/// A run entry of `cell streets`, as cell, street and country. Its document
/// is always zero.
const CELL_TAG: u8 = b'C';
/// A run entry of `places`, as name and country. Its document is a cell.
const PLACE_TAG: u8 = b'P';
/// A run entry of `streets`, as street and country.
const STREET_TAG: u8 = b'Z';
/// Documents keyed per run file: a run is sorted in memory, and 2M
/// documents' keys, places with them, fit in a few gigabytes.
const RUN_DOCUMENTS: u32 = 1 << 21;
/// Documents keyed together. Neighbouring documents share most places and
/// cells, so a chunk's entries are deduplicated before they join a run:
/// every name of every area of every address would not fit in memory.
const CHUNK_DOCUMENTS: u32 = 4096;
const META_FILE: &str = "meta.json";
const STREET_FILE: &str = "streets.fst";
const LISTS_FILE: &str = "docs.u32";
/// The cell of each document, by document id: the build's own, to sort a
/// street's documents by; the Pack keeps none, a document's cell follows
/// from its extent (see [`crate::search::TextSearcher::doc_cell`]).
const DOC_CELLS_FILE: &str = "doc-cells.u32";
const PLACE_FILE: &str = "places.fst";
const PLACE_RECORDS_FILE: &str = "places.rec";
const PLACE_CELLS_FILE: &str = "places.u32";
const CELL_STREET_FILE: &str = "cell-streets.fst";
/// The Pack section each file of a build goes into, the meta first: a Pack
/// holds the structured index when it holds its meta.
const SECTIONS: [(&str, &str); 7] = [
    ("streets/meta", META_FILE),
    ("streets/keys", STREET_FILE),
    ("streets/lists", LISTS_FILE),
    ("gazetteer/names", PLACE_FILE),
    ("gazetteer/places", PLACE_RECORDS_FILE),
    ("gazetteer/cells", PLACE_CELLS_FILE),
    ("streets/by_cell", CELL_STREET_FILE),
];
const SECTION_VERSION: u32 = 1;
/// A list's length in the low bits of its map value, its offset above.
const LENGTH_BITS: u32 = 24;
/// A place used in more cells than this (a region, a large city) keeps only
/// its extent: a list that long filters little a box does not, and costs a
/// query more to read.
pub(crate) const PLACE_CELLS: usize = 4096;
/// An area record adds every cell of its extent up to this many, about
/// 32 by 32 km; a larger one adds its corners, which keep its extent.
pub(crate) const AREA_CELLS: usize = 1024;
/// Extent and cell list of one place, see [`Place`].
const PLACE_RECORD_BYTES: usize = 24;

/// Grid rows: 0.009 degrees of latitude each, about one kilometre.
const ROW_E7: i64 = 90_000;
const ROWS: u32 = 20_000;
/// Columns at the equator, 0.009 degrees of longitude each.
const EQUATOR_COLUMNS: f64 = 40_000.0;

/// How many columns grid row `row` has: as many as keep a cell about one
/// kilometre wide at its latitude, so a ring of cells is about the same
/// size anywhere.
fn columns(row: u32) -> u32 {
    static COLUMNS: OnceLock<Vec<u32>> = OnceLock::new();
    COLUMNS.get_or_init(|| {
        (0..ROWS)
            .map(|row| {
                let middle = (f64::from(row) + 0.5) * ROW_E7 as f64 / 1e7 - 90.0;
                ((EQUATOR_COLUMNS * middle.to_radians().cos()).floor() as u32).max(1)
            })
            .collect()
    })[row.min(ROWS - 1) as usize]
}

fn row_of(lat: i32) -> u32 {
    ((i64::from(lat) + 900_000_000) / ROW_E7).clamp(0, i64::from(ROWS) - 1) as u32
}

fn column_of(row: u32, lon: i32) -> u32 {
    let count = i64::from(columns(row));
    ((i64::from(lon) + 1_800_000_000) * count / 3_600_000_000).clamp(0, count - 1) as u32
}

/// The map cell of a point in 1e-7 degrees: a fixed grid of rows of
/// latitude, each cut into as many columns as keep its cells about a
/// kilometre square. Chosen over S2 or H3 for needing no library and for
/// ids that sort row by row, so the cells of a box are one range per row.
pub(crate) fn cell(lon: i32, lat: i32) -> u32 {
    let row = row_of(lat);
    row << 16 | column_of(row, lon)
}

/// The box `[min_lon, min_lat, max_lon, max_lat]` of `cell` in 1e-7
/// degrees.
pub(crate) fn cell_box(cell: u32) -> [i32; 4] {
    let (row, column) = (cell >> 16, i64::from(cell & 0xFFFF));
    let count = i64::from(columns(row));
    let south = i64::from(row) * ROW_E7 - 900_000_000;
    let west = column * 3_600_000_000 / count - 1_800_000_000;
    let east = (column + 1) * 3_600_000_000 / count - 1_800_000_000;
    [
        west as i32,
        south as i32,
        east.min(i64::from(i32::MAX)) as i32,
        (south + ROW_E7).min(i64::from(i32::MAX)) as i32,
    ]
}

/// `cell` and the cells around it, sorted. A row above or below may have a
/// column more or less, so the ring holds every cell there that touches
/// the columns beside `cell`'s.
pub(crate) fn ring(cell: u32) -> Vec<u32> {
    let (row, column) = (cell >> 16, cell & 0xFFFF);
    let count = u64::from(columns(row));
    let mut ring = Vec::with_capacity(12);
    for near in row.saturating_sub(1)..=(row + 1).min(ROWS - 1) {
        let near_count = u64::from(columns(near));
        let first = u64::from(column) * near_count / count;
        let last = ((u64::from(column) + 1) * near_count).div_ceil(count);
        for at in first as i64 - 1..=last as i64 {
            let wrapped = at.rem_euclid(near_count as i64) as u32;
            ring.push(near << 16 | wrapped);
        }
    }
    ring.sort_unstable();
    ring.dedup();
    ring
}

/// The cells of the box `[min_lon, min_lat, max_lon, max_lat]`, `None` when
/// more than `limit`.
pub(crate) fn box_cells(extent: [i32; 4], limit: usize) -> Option<Vec<u32>> {
    let mut cells = Vec::new();
    for (row, west, east) in box_rows(extent) {
        for column in west..=east {
            if cells.len() == limit {
                return None;
            }
            cells.push(row << 16 | column);
        }
    }
    Some(cells)
}

/// The rows of the box `extent`, each with its first and last column in it.
pub(crate) fn box_rows(extent: [i32; 4]) -> impl Iterator<Item = (u32, u32, u32)> {
    let [west, south, east, north] = extent;
    (row_of(south)..=row_of(north))
        .map(move |row| (row, column_of(row, west), column_of(row, east)))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Meta {
    /// The text index's segments and their document counts, in the order
    /// a searcher reads them: a document's id is its id in its segment plus
    /// the counts of the segments before it.
    segments: Vec<(String, u32)>,
    max_doc: u32,
    keyed_documents: u64,
    street_keys: u64,
    /// Street names keyed a second way, their ambiguous abbreviations left
    /// out as a type rather than expanded (see
    /// [`crate::search::TextSearcher::street_keys`]).
    second_street_keys: u64,
    place_keys: u64,
    /// Places past [`PLACE_CELLS`], kept as an extent.
    capped_places: u64,
    /// Cells per place: 50th, 90th, 99th percentile and largest.
    place_cells: [u64; 4],
    cell_street_keys: u64,
    entries: u64,
}

/// The structured index of a Pack, read from its sections.
pub struct StructuredIndex {
    streets: Map<Bytes>,
    lists: Bytes,
    places: Map<Bytes>,
    place_records: Bytes,
    place_cells: Bytes,
    cell_streets: Map<Bytes>,
}

pub(crate) const LAYER_ADDRESS: u8 = 1;
pub(crate) const LAYER_INTERPOLATION: u8 = 2;
pub(crate) const LAYER_STREET: u8 = 3;
pub(crate) const LAYER_POI: u8 = 4;

fn int_at(bytes: &[u8], at: usize) -> i32 {
    i32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

impl std::fmt::Debug for StructuredIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StructuredIndex")
            .field("street_keys", &self.streets.len())
            .field("place_keys", &self.places.len())
            .field("cell_street_keys", &self.cell_streets.len())
            .finish_non_exhaustive()
    }
}

/// The documents of one street key in one country, sorted by cell.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StreetList {
    pub(crate) country: String,
    offset: usize,
    len: usize,
}

impl StreetList {
    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

/// Where one place name of one country is used: its extent, and its cells
/// unless there are more than [`PLACE_CELLS`].
#[derive(Debug, Clone)]
pub(crate) struct Place {
    pub(crate) name: String,
    pub(crate) country: String,
    pub(crate) extent: [i32; 4],
    cells: Option<(usize, usize)>,
}

impl Place {
    /// Whether only the extent is kept.
    pub(crate) fn capped(&self) -> bool {
        self.cells.is_none()
    }
}

type Visit<'a> = dyn FnMut(&[u8], &[u8], u64) -> bool + 'a;

fn map_file(path: &Path) -> Result<Mmap> {
    let file = fs::File::open(path).map_err(|source| Error::IoPath {
        action: "failed to open",
        path: path.to_path_buf(),
        source,
    })?;
    // SAFETY: a build's scratch file, written before it is mapped and
    // removed after the Pack holds it.
    Ok(unsafe { Mmap::map(&file)? })
}

fn country_wanted(countries: &[String], country: &[u8]) -> bool {
    countries.is_empty()
        || countries
            .iter()
            .any(|wanted| wanted.as_bytes().eq_ignore_ascii_case(country))
}

impl StructuredIndex {
    /// The structured index of the Pack `container`, `None` when it was built
    /// without one: every query then runs the text search.
    pub fn open(container: &Container) -> Result<Option<Self>> {
        if !container.sections().contains_key(SECTIONS[0].0) {
            return Ok(None);
        }
        let section = |at: usize| container.section(SECTIONS[at].0, SECTION_VERSION);
        Ok(Some(Self {
            streets: Map::new(section(1)?)?,
            lists: section(2)?,
            places: Map::new(section(3)?)?,
            place_records: section(4)?,
            place_cells: section(5)?,
            cell_streets: Map::new(section(6)?)?,
        }))
    }

    /// The lists of the street keyed `street` in `countries` (any when
    /// empty), and of the same name with a direction word more or less at
    /// either end: one source's "roosevelt blvd" is another's "e roosevelt
    /// blvd". Whether such a list is the same road is for its records'
    /// places and points to tell (see [`crate::road`]).
    pub(crate) fn street_lists(&self, street: &str, countries: &[String]) -> Vec<StreetList> {
        let mut found = self.street_lists_keyed(street, countries);
        for other in direction_spellings(street) {
            for list in self.street_lists_keyed(&other, countries) {
                if !found.iter().any(|held| held.offset == list.offset) {
                    found.push(list);
                }
            }
        }
        found
    }

    /// The lists of the street keyed exactly `street` in `countries`.
    fn street_lists_keyed(&self, street: &str, countries: &[String]) -> Vec<StreetList> {
        let mut found = Vec::new();
        scan(
            &self.streets,
            &[],
            street,
            true,
            false,
            &mut |_, country, value| {
                if country_wanted(countries, country) {
                    found.push(StreetList {
                        country: String::from_utf8_lossy(country).into_owned(),
                        offset: (value >> LENGTH_BITS) as usize,
                        len: (value & ((1 << LENGTH_BITS) - 1)) as usize,
                    });
                }
                true
            },
        );
        found
    }

    /// Document `index` of `list`.
    pub(crate) fn list_doc(&self, list: &StreetList, index: usize) -> DocId {
        u32_at(&self.lists, (list.offset + index) * 4)
    }

    /// The positions in `list` of its documents in cells `first` to `last`,
    /// each document's cell read by `cell_of`.
    pub(crate) fn list_range(
        &self,
        list: &StreetList,
        first: u32,
        last: u32,
        cell_of: &impl Fn(DocId) -> u32,
    ) -> (usize, usize) {
        let bound = |wanted: u32| {
            let (mut low, mut high) = (0, list.len);
            while low < high {
                let middle = (low + high) / 2;
                if cell_of(self.list_doc(list, middle)) < wanted {
                    low = middle + 1;
                } else {
                    high = middle;
                }
            }
            low
        };
        (bound(first), bound(last.saturating_add(1)))
    }

    /// The places keyed `name` in `countries` (any when empty), or when
    /// `fuzzy` those one edit from it with its first character as written,
    /// as a typo keeps it.
    pub(crate) fn places(&self, name: &str, countries: &[String], fuzzy: bool) -> Vec<Place> {
        let mut found = Vec::new();
        scan(
            &self.places,
            &[],
            name,
            true,
            fuzzy,
            &mut |named, country, value| {
                if !country_wanted(countries, country) {
                    return true;
                }
                let at = value as usize * PLACE_RECORD_BYTES;
                let Some(bytes) = self.place_records.get(at..at + PLACE_RECORD_BYTES) else {
                    return true;
                };
                let (offset, len) = (u32_at(bytes, 16) as usize, u32_at(bytes, 20) as usize);
                found.push(Place {
                    name: String::from_utf8_lossy(named).into_owned(),
                    country: String::from_utf8_lossy(country).into_owned(),
                    extent: [
                        int_at(bytes, 0),
                        int_at(bytes, 4),
                        int_at(bytes, 8),
                        int_at(bytes, 12),
                    ],
                    cells: (len > 0).then_some((offset, len)),
                });
                true
            },
        );
        found
    }

    /// The cells of `place`, sorted; empty when only its extent is kept.
    pub(crate) fn place_cells(&self, place: &Place) -> Vec<u32> {
        let Some((offset, len)) = place.cells else {
            return Vec::new();
        };
        self.place_cells[offset * 4..(offset + len) * 4]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| u32::from_le_bytes(*bytes))
            .collect()
    }

    /// Whether a document in `cell` at `lon`, `lat` lies in `place`: in or
    /// beside one of its cells, or inside its extent when only that is kept.
    pub(crate) fn place_holds(&self, place: &Place, cell: u32, lon: i32, lat: i32) -> bool {
        let Some((offset, len)) = place.cells else {
            let [west, south, east, north] = place.extent;
            return (west..=east).contains(&lon) && (south..=north).contains(&lat);
        };
        let cells = &self.place_cells[offset * 4..(offset + len) * 4];
        let at = |index: usize| u32_at(cells, index * 4);
        ring(cell).into_iter().any(|near| {
            let (mut low, mut high) = (0, len);
            while low < high {
                let middle = (low + high) / 2;
                match at(middle).cmp(&near) {
                    std::cmp::Ordering::Less => low = middle + 1,
                    std::cmp::Ordering::Greater => high = middle,
                    std::cmp::Ordering::Equal => return true,
                }
            }
            false
        })
    }

    /// The streets in `cells` in `countries` (any when empty), each once.
    pub(crate) fn streets_in(&self, cells: &[u32], countries: &[String]) -> Vec<String> {
        let mut found = HashSet::new();
        for cell in cells {
            let lower = cell.to_be_bytes();
            let mut stream = match cell.checked_add(1) {
                Some(next) => self
                    .cell_streets
                    .range()
                    .ge(lower)
                    .lt(next.to_be_bytes())
                    .into_stream(),
                None => self.cell_streets.range().ge(lower).into_stream(),
            };
            while let Some((key, _)) = stream.next() {
                let rest = &key[4..];
                let Some(at) = rest.iter().rposition(|byte| *byte == SEPARATOR) else {
                    continue;
                };
                if country_wanted(countries, &rest[at + 1..]) {
                    found.insert(String::from_utf8_lossy(&rest[..at]).into_owned());
                }
            }
        }
        let mut found = found.into_iter().collect::<Vec<_>>();
        found.sort_unstable();
        found
    }
}

/// Visit the keys of `map` that are `prefix` then `word` (or when `fuzzy`
/// a word one edit from it with its first character as written), then a
/// separator and a country when `tail`, else nothing. `visit` gets the
/// word part, the country and the value, and stops the scan with `false`,
/// which is returned.
fn scan(
    map: &Map<Bytes>,
    prefix: &[u8],
    word: &str,
    tail: bool,
    fuzzy: bool,
    visit: &mut Visit<'_>,
) -> bool {
    let mut split = |key: &[u8], value: u64| -> bool {
        let rest = &key[prefix.len()..];
        match tail {
            true => match rest.iter().rposition(|byte| *byte == SEPARATOR) {
                Some(at) => visit(&rest[..at], &rest[at + 1..], value),
                None => true,
            },
            false => visit(rest, &[], value),
        }
    };
    if fuzzy {
        let Some(first) = word.chars().next() else {
            return true;
        };
        let dfa = automaton_builder(1).build_dfa(word);
        let mut lower = prefix.to_vec();
        let mut buffer = [0; 4];
        lower.extend(first.encode_utf8(&mut buffer).as_bytes());
        let mut upper = lower.clone();
        upper.push(0xFF);
        let automaton = KeyAutomaton {
            prefix,
            dfa: &dfa,
            tail,
        };
        let mut stream = map.search(automaton).ge(&lower).lt(&upper).into_stream();
        while let Some((key, value)) = stream.next() {
            if !split(key, value) {
                return false;
            }
        }
        return true;
    }
    let mut lower = prefix.to_vec();
    lower.extend(word.as_bytes());
    if !tail {
        return map.get(&lower).is_none_or(|value| split(&lower, value));
    }
    lower.push(SEPARATOR);
    let mut upper = lower.clone();
    *upper.last_mut().expect("separator pushed") = SEPARATOR + 1;
    let mut stream = map.range().ge(&lower).lt(&upper).into_stream();
    while let Some((key, value)) = stream.next() {
        if !split(key, value) {
            return false;
        }
    }
    true
}

/// Keys whose word part is within a Levenshtein automaton of the one
/// sought, after a literal prefix, then a separator and any country when
/// `tail`, else the end of the key.
struct KeyAutomaton<'a> {
    prefix: &'a [u8],
    dfa: &'a DFA,
    tail: bool,
}

#[derive(Debug, Clone, Copy)]
enum KeyState {
    Prefix(usize),
    Word(u32),
    Country,
    Dead,
}

impl Automaton for KeyAutomaton<'_> {
    type State = KeyState;

    fn start(&self) -> KeyState {
        match self.prefix.is_empty() {
            true => KeyState::Word(self.dfa.initial_state()),
            false => KeyState::Prefix(0),
        }
    }

    fn is_match(&self, state: &KeyState) -> bool {
        match *state {
            KeyState::Country => true,
            KeyState::Word(at) => !self.tail && matches!(self.dfa.distance(at), Distance::Exact(_)),
            KeyState::Prefix(_) | KeyState::Dead => false,
        }
    }

    fn can_match(&self, state: &KeyState) -> bool {
        !matches!(state, KeyState::Dead)
    }

    fn accept(&self, state: &KeyState, byte: u8) -> KeyState {
        match *state {
            KeyState::Prefix(at) if self.prefix[at] == byte => match at + 1 == self.prefix.len() {
                true => KeyState::Word(self.dfa.initial_state()),
                false => KeyState::Prefix(at + 1),
            },
            KeyState::Word(at) if self.tail && byte == SEPARATOR => match self.dfa.distance(at) {
                Distance::Exact(_) => KeyState::Country,
                Distance::AtLeast(_) => KeyState::Dead,
            },
            KeyState::Word(at) => match self.dfa.transition(at, byte) {
                SINK_STATE => KeyState::Dead,
                next => KeyState::Word(next),
            },
            KeyState::Country => KeyState::Country,
            KeyState::Prefix(_) | KeyState::Dead => KeyState::Dead,
        }
    }
}

/// What [`TextSearcher::structured_keys`] reads of one document.
#[derive(Debug, Default)]
pub(crate) struct Keyed {
    /// The cell of the middle of its extent, when it is on a street.
    pub(crate) cell: u32,
    pub(crate) country: String,
    /// Every key of every street it is on (or is).
    pub(crate) streets: Vec<String>,
    /// The one key per street a misspelt street is matched against.
    pub(crate) canonical: Vec<String>,
    /// How many of its streets have a second key.
    pub(crate) second_keys: u64,
    /// Place names it states or is, and its postcodes.
    pub(crate) names: Vec<String>,
    /// The areas of its chain, or the area it is, whose every name holds it.
    pub(crate) areas: Vec<u32>,
    /// The cells it covers.
    pub(crate) cells: Vec<u32>,
}

/// Every name of every area, by area id: the Pack's own lists of the areas
/// each name names (see [`crate::parse::lattice::dictionary`]), turned
/// round.
struct AreaNames {
    names: Vec<String>,
    /// Area id and index into `names`, sorted.
    pairs: Vec<(u32, u32)>,
}

impl AreaNames {
    fn new(lists: Vec<(String, Vec<u32>)>) -> Self {
        let mut names = Vec::with_capacity(lists.len());
        let mut pairs = Vec::new();
        for (name, areas) in lists {
            let index = names.len() as u32;
            names.push(name);
            pairs.extend(areas.into_iter().map(|area| (area, index)));
        }
        pairs.par_sort_unstable();
        pairs.dedup();
        Self { names, pairs }
    }

    fn of(&self, area: u32) -> impl Iterator<Item = &str> {
        let from = self.pairs.partition_point(|(id, _)| *id < area);
        self.pairs[from..]
            .iter()
            .take_while(move |(id, _)| *id == area)
            .map(|(_, index)| self.names[*index as usize].as_str())
    }
}

/// The run entries and cells of the documents `begin..end`.
struct Chunk {
    cells: Vec<u32>,
    entries: Vec<(Vec<u8>, u32)>,
    keyed: u64,
    second_keys: u64,
}

fn key_chunk(
    searcher: &TextSearcher,
    areas: &AreaNames,
    begin: DocId,
    end: DocId,
) -> Result<Chunk> {
    let mut chunk = Chunk {
        cells: Vec::with_capacity((end - begin) as usize),
        entries: Vec::new(),
        keyed: 0,
        second_keys: 0,
    };
    let mut area_cells = HashSet::<(u32, u32, usize)>::new();
    let mut countries = Vec::<String>::new();
    for doc in begin..end {
        let keyed = searcher.structured_keys(doc)?;
        chunk.cells.push(keyed.cell);
        chunk.keyed += u64::from(!keyed.streets.is_empty());
        chunk.second_keys += keyed.second_keys;
        let country = keyed.country.as_str();
        for street in &keyed.streets {
            chunk.entries.push((street_key(street, country), doc));
        }
        for street in &keyed.canonical {
            for cell in &keyed.cells {
                chunk
                    .entries
                    .push((cell_street_key(*cell, street, country), 0));
            }
        }
        for name in &keyed.names {
            for cell in &keyed.cells {
                chunk.entries.push((place_key(name, country), *cell));
            }
        }
        if keyed.areas.is_empty() || keyed.cells.is_empty() {
            continue;
        }
        let index = match countries.iter().position(|held| held == country) {
            Some(index) => index,
            None => {
                countries.push(keyed.country.clone());
                countries.len() - 1
            }
        };
        for area in &keyed.areas {
            for cell in &keyed.cells {
                area_cells.insert((*area, *cell, index));
            }
        }
    }
    for (area, cell, index) in area_cells {
        for name in areas.of(area) {
            chunk
                .entries
                .push((place_key(name, &countries[index]), cell));
        }
    }
    chunk.entries.sort_unstable();
    chunk.entries.dedup();
    Ok(chunk)
}

/// Build the structured index of the sealed Pack `sealed` and add it to the Pack:
/// one pass over its text index's documents into files beside it, each then
/// copied into a section of its own.
pub fn add_to(sealed: &mut pack::SealedPack) -> Result<()> {
    let out = sealed.generation().join("structured.tmp");
    if out.exists() {
        fs::remove_dir_all(&out)?;
    }
    build(&sealed.path(), &out)?;
    let sections = SECTIONS
        .iter()
        .map(|(section, file)| (*section, SECTION_VERSION, out.join(file)))
        .collect::<Vec<_>>();
    sealed.add_section_files(&sections)?;
    fs::remove_dir_all(&out)?;
    Ok(())
}

/// Build the structured index of the Pack at `pack_path` into the directory `out`.
fn build(pack_path: &Path, out: &Path) -> Result<()> {
    let pack = Arc::new(pack::Reader::open(pack_path)?);
    let searcher = TextSearcher::from_pack(Arc::clone(&pack))?;
    let segments = searcher.structured_segments();
    let max_doc = segments
        .iter()
        .try_fold(0u32, |total, (_, count)| total.checked_add(*count))
        .ok_or_else(|| Error::Inconsistent("more documents than a u32 holds".into()))?;
    let areas = AreaNames::new(searcher.area_names());
    fs::create_dir_all(out)?;
    let scratch = out.join("runs.tmp");
    fs::create_dir_all(&scratch)?;
    let mut runs = Vec::new();
    let mut meta = Meta {
        segments,
        max_doc,
        ..Meta::default()
    };
    let second_keys = AtomicU64::new(0);
    let mut doc_cells =
        BufWriter::with_capacity(1 << 20, fs::File::create(out.join(DOC_CELLS_FILE))?);
    let mut begin = 0u32;
    while begin < max_doc {
        let end = begin.saturating_add(RUN_DOCUMENTS).min(max_doc);
        let chunks = (begin..end)
            .step_by(CHUNK_DOCUMENTS as usize)
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|first| {
                let chunk =
                    key_chunk(&searcher, &areas, first, (first + CHUNK_DOCUMENTS).min(end))?;
                second_keys.fetch_add(chunk.second_keys, Ordering::Relaxed);
                Ok(chunk)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut entries = Vec::new();
        for chunk in chunks {
            for cell in &chunk.cells {
                doc_cells.write_all(&cell.to_le_bytes())?;
            }
            meta.keyed_documents += chunk.keyed;
            entries.extend(chunk.entries);
        }
        entries.par_sort_unstable();
        entries.dedup();
        let path = scratch.join(format!("run-{:05}.bin", runs.len()));
        let mut writer = BufWriter::with_capacity(1 << 20, fs::File::create(&path)?);
        // Sorted keys share most of each one's start with the one before:
        // written whole, a planet's runs outgrow the disk.
        let mut previous: &[u8] = &[];
        for (key, doc) in &entries {
            let shared = previous
                .iter()
                .zip(key)
                .take_while(|(left, right)| left == right)
                .count();
            write_varint(&mut writer, shared as u32)?;
            write_varint(&mut writer, u32::try_from(key.len() - shared)?)?;
            writer.write_all(&key[shared..])?;
            write_varint(&mut writer, *doc)?;
            previous = key;
        }
        writer.flush()?;
        runs.push(path);
        begin = end;
    }
    doc_cells.flush()?;
    drop(doc_cells);
    merge(&runs, out, &mut meta)?;
    meta.second_street_keys = second_keys.load(Ordering::Relaxed);
    fs::write(out.join(META_FILE), serde_json::to_vec_pretty(&meta)?)?;
    for run in &runs {
        fs::remove_file(run)?;
    }
    fs::remove_dir(&scratch)?;
    Ok(())
}

fn write_varint(writer: &mut impl Write, mut value: u32) -> io::Result<()> {
    while value >= 0x80 {
        writer.write_all(&[(value as u8) | 0x80])?;
        value >>= 7;
    }
    writer.write_all(&[value as u8])
}

/// `None` at the end of the file.
fn read_varint(reader: &mut impl Read) -> io::Result<Option<u32>> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let mut byte = [0; 1];
        match reader.read_exact(&mut byte) {
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && shift == 0 => {
                return Ok(None);
            }
            result => result?,
        }
        value |= u32::from(byte[0] & 0x7F) << shift;
        if byte[0] < 0x80 {
            return Ok(Some(value));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "varint too long",
    ))
}

struct Run {
    reader: BufReader<fs::File>,
    /// The key read last, which the next shares its start with.
    key: Vec<u8>,
}

impl Run {
    fn next(&mut self) -> io::Result<Option<(Vec<u8>, DocId)>> {
        let Some(shared) = read_varint(&mut self.reader)? else {
            return Ok(None);
        };
        let truncated = || io::Error::from(io::ErrorKind::UnexpectedEof);
        let rest = read_varint(&mut self.reader)?.ok_or_else(truncated)?;
        self.key.truncate(shared as usize);
        let start = self.key.len();
        self.key.resize(start + rest as usize, 0);
        self.reader.read_exact(&mut self.key[start..])?;
        let doc = read_varint(&mut self.reader)?.ok_or_else(truncated)?;
        Ok(Some((self.key.clone(), doc)))
    }
}

/// The maps being written, in key order.
struct Writers {
    streets: MapBuilder<BufWriter<fs::File>>,
    places: MapBuilder<BufWriter<fs::File>>,
    cell_streets: MapBuilder<BufWriter<fs::File>>,
    lists: BufWriter<fs::File>,
    offset: u64,
    place_records: BufWriter<fs::File>,
    place_cells: BufWriter<fs::File>,
    place_offset: u64,
    /// Cells of each place.
    sizes: Vec<u32>,
    /// The cell of each document (see [`DOC_CELLS_FILE`]).
    doc_cells: Mmap,
}

impl Writers {
    fn cell_of(&self, doc: DocId) -> u32 {
        let at = doc as usize * 4;
        self.doc_cells
            .get(at..at + 4)
            .map_or(0, |bytes| u32_at(bytes, 0))
    }

    /// Write the documents (or cells) of one run key.
    fn flush(&mut self, key: &[u8], docs: &mut Vec<DocId>, meta: &mut Meta) -> Result<()> {
        docs.dedup();
        let rest = &key[1..];
        match key[0] {
            STREET_TAG => {
                let mut placed = docs
                    .iter()
                    .map(|doc| (self.cell_of(*doc), *doc))
                    .collect::<Vec<_>>();
                placed.sort_unstable();
                let length = (placed.len() as u64).min((1 << LENGTH_BITS) - 1);
                for (_, doc) in &placed[..length as usize] {
                    self.lists.write_all(&doc.to_le_bytes())?;
                }
                self.streets
                    .insert(rest, self.offset << LENGTH_BITS | length)?;
                self.offset += length;
                meta.street_keys += 1;
            }
            PLACE_TAG => {
                let [mut west, mut south, mut east, mut north] =
                    [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
                for cell in docs.iter() {
                    let [w, s, e, n] = cell_box(*cell);
                    (west, south, east, north) =
                        (west.min(w), south.min(s), east.max(e), north.max(n));
                }
                let (offset, len) = match docs.len() <= PLACE_CELLS {
                    true => {
                        for cell in docs.iter() {
                            self.place_cells.write_all(&cell.to_le_bytes())?;
                        }
                        let offset = self.place_offset;
                        self.place_offset += docs.len() as u64;
                        (u32::try_from(offset)?, docs.len() as u32)
                    }
                    false => {
                        meta.capped_places += 1;
                        (0, 0)
                    }
                };
                for value in [west, south, east, north] {
                    self.place_records.write_all(&value.to_le_bytes())?;
                }
                self.place_records.write_all(&offset.to_le_bytes())?;
                self.place_records.write_all(&len.to_le_bytes())?;
                self.places.insert(rest, meta.place_keys)?;
                meta.place_keys += 1;
                self.sizes.push(docs.len() as u32);
            }
            CELL_TAG => {
                self.cell_streets.insert(rest, 0)?;
                meta.cell_street_keys += 1;
            }
            _ => {}
        }
        docs.clear();
        Ok(())
    }
}

/// Merge the sorted runs into the three maps and their lists.
fn merge(runs: &[PathBuf], out: &Path, meta: &mut Meta) -> Result<()> {
    let mut readers = runs
        .iter()
        .map(|path| {
            Ok(Run {
                reader: BufReader::with_capacity(1 << 20, fs::File::open(path)?),
                key: Vec::new(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut heap = BinaryHeap::new();
    for (index, run) in readers.iter_mut().enumerate() {
        if let Some((key, doc)) = run.next()? {
            heap.push(Reverse((key, doc, index)));
        }
    }
    let create = |name: &str| -> Result<BufWriter<fs::File>> {
        Ok(BufWriter::with_capacity(
            1 << 20,
            fs::File::create(out.join(name))?,
        ))
    };
    let mut writers = Writers {
        streets: MapBuilder::new(create(STREET_FILE)?)?,
        places: MapBuilder::new(create(PLACE_FILE)?)?,
        cell_streets: MapBuilder::new(create(CELL_STREET_FILE)?)?,
        lists: create(LISTS_FILE)?,
        offset: 0,
        place_records: create(PLACE_RECORDS_FILE)?,
        place_cells: create(PLACE_CELLS_FILE)?,
        place_offset: 0,
        sizes: Vec::new(),
        doc_cells: map_file(&out.join(DOC_CELLS_FILE))?,
    };
    let mut current: Option<Vec<u8>> = None;
    let mut docs = Vec::<DocId>::new();
    while let Some(Reverse((key, doc, index))) = heap.pop() {
        if let Some((next, next_doc)) = readers[index].next()? {
            heap.push(Reverse((next, next_doc, index)));
        }
        meta.entries += 1;
        if current.as_deref() != Some(key.as_slice()) {
            if let Some(held) = current.take() {
                writers.flush(&held, &mut docs, meta)?;
            }
            current = Some(key);
        }
        docs.push(doc);
    }
    if let Some(held) = current.take() {
        writers.flush(&held, &mut docs, meta)?;
    }
    writers.lists.flush()?;
    writers.place_records.flush()?;
    writers.place_cells.flush()?;
    writers.streets.finish()?;
    writers.places.finish()?;
    writers.cell_streets.finish()?;
    let mut sizes = std::mem::take(&mut writers.sizes);
    sizes.par_sort_unstable();
    if let Some(&largest) = sizes.last() {
        let at = |share: f64| u64::from(sizes[((sizes.len() - 1) as f64 * share) as usize]);
        meta.place_cells = [at(0.5), at(0.9), at(0.99), u64::from(largest)];
    }
    Ok(())
}

/// The words a street name may lead or end with to state its direction,
/// abbreviated and in full.
const DIRECTION_WORDS: [&str; 16] = [
    "n",
    "s",
    "e",
    "w",
    "ne",
    "nw",
    "se",
    "sw",
    "north",
    "south",
    "east",
    "west",
    "northeast",
    "northwest",
    "southeast",
    "southwest",
];

/// `street` with a direction word added at either end, or, when it leads
/// or ends with one, without it. A name of one word besides the direction
/// keeps it: "north street" is a name, not a direction.
fn direction_spellings(street: &str) -> Vec<String> {
    let words = street.split_whitespace().collect::<Vec<_>>();
    let is_direction = |word: &str| DIRECTION_WORDS.contains(&word);
    let mut out = Vec::new();
    match (words.first(), words.last()) {
        (Some(first), _) if words.len() > 2 && is_direction(first) => {
            out.push(words[1..].join(" "));
        }
        (_, Some(last)) if words.len() > 2 && is_direction(last) => {
            out.push(words[..words.len() - 1].join(" "));
        }
        (Some(_), Some(_)) if !words.iter().any(|word| is_direction(word)) => {
            for direction in DIRECTION_WORDS {
                out.push(format!("{direction} {street}"));
                out.push(format!("{street} {direction}"));
            }
        }
        _ => {}
    }
    out
}

/// The run key of `street` in `country`: a `streets` entry.
pub(crate) fn street_key(street: &str, country: &str) -> Vec<u8> {
    let mut key = vec![STREET_TAG];
    key.extend(street.as_bytes());
    key.push(SEPARATOR);
    key.extend(country.as_bytes());
    key
}

/// The run key of place `name` in `country`: a `places` entry.
fn place_key(name: &str, country: &str) -> Vec<u8> {
    let mut key = vec![PLACE_TAG];
    key.extend(name.as_bytes());
    key.push(SEPARATOR);
    key.extend(country.as_bytes());
    key
}

/// The run key of `street` in `cell` of `country`: a `cell streets` entry.
fn cell_street_key(cell: u32, street: &str, country: &str) -> Vec<u8> {
    let mut key = vec![CELL_TAG];
    key.extend(cell.to_be_bytes());
    key.extend(street.as_bytes());
    key.push(SEPARATOR);
    key.extend(country.as_bytes());
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ring_holds_its_cell_and_the_cells_around_it() {
        for (lon, lat) in [
            (0, 0),
            (-1_234_567_890, 451_234_567),
            (1_799_999_999, 600_000_000),
        ] {
            let at = cell(lon, lat);
            let ring = ring(at);
            assert!(ring.contains(&at));
            assert!((9..=12).contains(&ring.len()), "{}", ring.len());
            let [west, south, east, north] = cell_box(at);
            assert!((west..=east).contains(&lon) && (south..=north).contains(&lat));
            for (step_lon, step_lat) in [
                (east - west, 0),
                (0, north - south),
                (west - east, south - north),
            ] {
                let near = cell(lon.saturating_add(step_lon), lat.saturating_add(step_lat));
                assert!(ring.contains(&near), "{near} of {at}");
            }
        }
    }

    #[test]
    fn a_box_has_the_cells_of_its_corners() {
        let extent = [-794_000_000, 436_000_000, -793_000_000, 437_000_000];
        let cells = box_cells(extent, 1_000).expect("small box");
        assert!(cells.contains(&cell(extent[0], extent[1])));
        assert!(cells.contains(&cell(extent[2], extent[3])));
        assert!(box_cells(extent, 4).is_none());
    }
}
