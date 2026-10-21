//! The parser's dictionaries, written into the Pack at build time.
//!
//! Each field is one FST section, `parse/<field>`, mapping a name's
//! [`dictionary_key`] (the `normalize_index_text` the search path uses,
//! glued as a query's units are) to how often the Pack uses it and the
//! country it is used in most. Every name a record has counts: its `name`
//! and its alternate names (`name:ja-Hira`, `name:en`, `alt_name`, ...). Street, locality, region
//! and country names come from the Pack's own records, so the Pack stays the
//! only copy of place data. Street-type words are the words that start or end
//! many of those street names, plus a vendored list of common types and
//! abbreviations; unit words are a vendored list plus the words that
//! designate many of the Pack's `addr:unit` values. libpostal's open word
//! lists (unit, PO box, level, building and street types, directionals) in
//! every language they hold join the street-type and unit dictionaries and
//! fill dictionaries of their own. POI names are a field of
//! their own, so a venue's name is known as one. A `words` section holds
//! every word of every name but a POI's, so a lookup knows when a word is no
//! typo.
//!
//! An admin area is also named without the word of its type ("愛知" for
//! "愛知県", "Bayern" for "Freistaat Bayern"), from libpostal's qualifiers
//! and a vendored list of admin type words (see [`admin_aliases`]). Which
//! areas each area name names and which areas lie in which are sections of
//! their own (`parse/areas`, `parse/area_ids`, `parse/nesting`), so a parse
//! naming places that cannot be in one address is known as one.
//!
//! A Pack built before these sections existed still opens: its dictionaries
//! are empty and the parser runs on token shapes alone.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    sync::OnceLock,
};

use crate::error::{Error, Result};
use fst::{Automaton, IntoStreamer, Map, MapBuilder, Streamer};
use levenshtein_automata::{DFA, Distance, SINK_STATE};

use unicode_script::{Script, UnicodeScript};

use super::{CountryCode, dictionary_key, is_numeral, is_unspaced, mask_numerals};
use crate::{
    container::{Bytes, Container, Writer},
    context::AdminContextTuple,
    pack::{RecordContext, RecordId},
    record::{AddressComponents, PlaceLayer, Record},
};

/// Format version of every `parse/` dictionary section.
pub const DICTIONARY_VERSION: u32 = 4;
/// The Pack section mapping each area name's key to where its areas are
/// listed in [`AREA_IDS_SECTION`].
pub const AREAS_SECTION: &str = "parse/areas";
/// The record ids of the areas each area name names, little-endian `u32`.
pub const AREA_IDS_SECTION: &str = "parse/area_ids";
/// Every pair of areas one record lies in, sorted little-endian `u64`s of
/// the smaller id above the larger.
pub const NESTING_SECTION: &str = "parse/nesting";
/// Each region with the groups of regions it lies in ("千葉県" in
/// "関東地方"), sorted little-endian `u64`s of the region's id above the
/// group's.
pub const REGION_GROUPS_SECTION: &str = "parse/region_groups";
/// A name of more areas than this ("2丁目", "中央区") says too little of
/// where it is: its areas are not listed.
const MAX_AREAS_PER_NAME: usize = 64;
/// Bits of an FST value that hold the count; the country code sits above.
const COUNT_BITS: u32 = 40;
const COUNT_MASK: u64 = (1 << COUNT_BITS) - 1;
/// The lowest bit of an FST value that holds the admin levels, above the
/// country code.
const LEVELS_SHIFT: u32 = 56;
/// A word that starts or ends at least this share of the Pack's street names
/// is a street type ("street", "rue", "chemin")...
const STREET_TYPE_SHARE: f64 = 0.002;
/// ...and at least this many of them, so a small Pack does not promote a
/// name that happens to repeat.
const MIN_STREET_TYPE_NAMES: u64 = 20;
/// A word that designates at least this many of the Pack's `addr:unit`
/// values ("suite" of "suite 200") is a unit word.
const MIN_UNIT_WORD_VALUES: u64 = 20;

/// Street types and their common abbreviations in many languages, one per
/// line; vocabulary, not place data.
const STREET_TYPE_WORDS: &str = include_str!("data/street_words.txt");
/// Words that introduce a unit, floor or building part, one per line.
const UNIT_WORDS: &str = include_str!("data/unit_words.txt");
/// libpostal's open word lists (MIT, see `NOTICE`),
/// every language of a list in one file, one phrase per line with its
/// `|`-separated variants. Vendored.
const LIBPOSTAL_UNIT_TYPES: &str = include_str!("data/unit_types.txt");
const LIBPOSTAL_PO_BOXES: &str = include_str!("data/po_boxes.txt");
const LIBPOSTAL_LEVEL_TYPES: &str = include_str!("data/level_types.txt");
const LIBPOSTAL_BUILDING_TYPES: &str = include_str!("data/building_types.txt");
const LIBPOSTAL_STREET_TYPES: &str = include_str!("data/street_types.txt");
const LIBPOSTAL_DIRECTIONALS: &str = include_str!("data/directionals.txt");
/// Words of a staircase or an entrance ("stiege", "eingang"), which libpostal
/// labels as parts of a unit.
const LIBPOSTAL_STAIRCASES: &str = include_str!("data/staircases.txt");
const LIBPOSTAL_ENTRANCES: &str = include_str!("data/entrances.txt");
/// Words that ask for what is near a place ("near", "in der nähe von").
const LIBPOSTAL_NEAR: &str = include_str!("data/near.txt");
/// Words of kinds of place a query asks for ("bäckerei", "hotels").
const LIBPOSTAL_CATEGORIES: &str = include_str!("data/categories.txt");
/// Vendored words dropped because they clash with what queries mean by them
/// (`data/vocabulary_overrides.txt`).
const VOCABULARY_OVERRIDES: &str = include_str!("data/vocabulary_overrides.txt");
/// libpostal's qualifiers: words that say what kind of place a name is, its
/// admin type words among them ("県", "bezirk", "township").
const LIBPOSTAL_QUALIFIERS: &str = include_str!("data/qualifiers.txt");
/// Admin type words libpostal's qualifiers lack ("freistaat",
/// "comunidad de", "-도"), with how they attach to a name.
const ADMIN_TYPES: &str = include_str!("data/admin_types.txt");
/// The admin type words libpostal strips from place names, per language, in
/// the format of [`ADMIN_TYPES`] (`data/admin_affixes.txt`).
const LIBPOSTAL_ADMIN_AFFIXES: &str = include_str!("data/admin_affixes.txt");
/// How many area names a numbered form must be shared by to be a name of its
/// own: a kind of area (`#丁目`), not one name with a numeral in it
/// (`四日市市`).
const MIN_NUMBERED_NAMES: usize = 5;

/// The admin levels a name is used at, one bit each.
pub(super) mod level {
    pub(crate) const COUNTRY: u8 = 1;
    pub(crate) const REGION: u8 = 1 << 1;
    pub(crate) const DISTRICT: u8 = 1 << 2;
    pub(crate) const LOCALITY: u8 = 1 << 3;
    pub(crate) const NEIGHBOURHOOD: u8 = 1 << 4;
    pub(crate) const PLACE: u8 = 1 << 5;
    /// The levels whose areas are also named without their type word.
    pub(crate) const ALIASED: u8 = REGION | DISTRICT | LOCALITY;

    /// The levels whose areas are named in `field`.
    pub(crate) fn of_field(field: super::Field) -> u8 {
        match field {
            super::Field::Country => COUNTRY,
            super::Field::Region => REGION,
            super::Field::Locality => DISTRICT | LOCALITY | NEIGHBOURHOOD | PLACE,
            _ => 0,
        }
    }
}

/// One dictionary of the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Field {
    Locality,
    Region,
    Street,
    Country,
    /// Names of the Pack's POIs: venues, shops, buildings.
    Poi,
    StreetType,
    Unit,
    Word,
    /// Words of a PO box ("po box", "私書箱"), from libpostal.
    PoBox,
    /// Words of a floor or level ("floor", "階"), from libpostal.
    Level,
    /// Words of a building ("tower", "ビル"), from libpostal.
    Building,
    /// Compass directions ("north", "nw"), from libpostal.
    Directional,
    /// Words that ask for what is near a place ("near", "in der nähe"),
    /// from libpostal.
    Near,
    /// Kinds of place a query asks for ("bäckerei", "hotels"), from
    /// libpostal.
    Category,
}

impl Field {
    pub const ALL: [Field; 14] = [
        Field::Locality,
        Field::Region,
        Field::Street,
        Field::Country,
        Field::Poi,
        Field::StreetType,
        Field::Unit,
        Field::Word,
        Field::PoBox,
        Field::Level,
        Field::Building,
        Field::Directional,
        Field::Near,
        Field::Category,
    ];
    /// The fields of names that a whole span of a query can be.
    pub const NAMES: [Field; 5] = [
        Field::Locality,
        Field::Region,
        Field::Street,
        Field::Country,
        Field::Poi,
    ];
    /// The fields of names of the areas a record lies in.
    pub const AREAS: [Field; 3] = [Field::Locality, Field::Region, Field::Country];
    /// The fields of vocabulary phrases a span of a query can be: words
    /// that say what the words beside them are.
    pub const PHRASES: [Field; 8] = [
        Field::Unit,
        Field::PoBox,
        Field::Level,
        Field::Building,
        Field::StreetType,
        Field::Directional,
        Field::Near,
        Field::Category,
    ];

    /// The Pack section that holds the field.
    pub fn section(self) -> &'static str {
        match self {
            Field::Locality => "parse/locality",
            Field::Region => "parse/region",
            Field::Street => "parse/street",
            Field::Country => "parse/country",
            Field::Poi => "parse/poi",
            Field::StreetType => "parse/street_type",
            Field::Unit => "parse/unit",
            Field::Word => "parse/words",
            Field::PoBox => "parse/po_box",
            Field::Level => "parse/level",
            Field::Building => "parse/building",
            Field::Directional => "parse/directional",
            Field::Near => "parse/near",
            Field::Category => "parse/category",
        }
    }

    pub(super) fn index(self) -> usize {
        self as usize
    }

    /// How much a hit says about the country of the query: a whole country
    /// or region name says more than a street, which many countries share.
    pub(super) fn country_vote(self, distance: u8, tokens: usize) -> f32 {
        let field = match self {
            Field::Country => 4.0,
            Field::Region => 2.0,
            Field::Locality => 1.5,
            _ => 0.5,
        };
        field * tokens as f32 / (1.0 + f32::from(distance))
    }
}

/// What the Pack says about one name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// How many records use it, or lie in the area it names.
    pub count: u64,
    /// The country most of those records are in, when known.
    pub country: Option<CountryCode>,
    /// The admin levels of the areas it names, bits of [`level`]: whether a
    /// locality name is a city, a district or a neighbourhood.
    pub levels: u8,
}

impl Entry {
    fn encode(self) -> u64 {
        self.count.min(COUNT_MASK)
            | self
                .country
                .map_or(0, |country| u64::from(country.to_u16()) << COUNT_BITS)
            | u64::from(self.levels) << LEVELS_SHIFT
    }

    fn decode(value: u64) -> Self {
        Self {
            count: value & COUNT_MASK,
            country: CountryCode::from_u16((value >> COUNT_BITS) as u16),
            levels: (value >> LEVELS_SHIFT) as u8,
        }
    }
}

/// A dictionary hit and how many edits away from the query it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub entry: Entry,
    pub distance: u8,
}

/// The parser's dictionaries, memory-mapped from the Pack.
pub struct Dictionaries {
    maps: Vec<Map<Bytes>>,
    /// Area name keys to `offset << 8 | count` of their ids in `area_ids`.
    areas: Map<Bytes>,
    area_ids: Bytes,
    nesting: Bytes,
    region_groups: Bytes,
    /// Records per country, from the country dictionary, to prefer the
    /// countries the Pack covers when only a postcode pattern names one.
    countries: HashMap<CountryCode, u64>,
}

impl fmt::Debug for Dictionaries {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("Dictionaries");
        for field in Field::ALL {
            debug.field(field.section(), &self.len(field));
        }
        debug.field(AREAS_SECTION, &self.areas.len());
        debug.field(NESTING_SECTION, &(self.nesting.len() / 8));
        debug.field(REGION_GROUPS_SECTION, &(self.region_groups.len() / 8));
        debug.finish()
    }
}

impl Dictionaries {
    /// The dictionaries a Pack carries; a field it does not carry is empty.
    pub fn open(container: &Container) -> Result<Self> {
        let maps = Field::ALL
            .into_iter()
            .map(|field| {
                if !container.sections().contains_key(field.section()) {
                    return Ok(empty_map());
                }
                let bytes = container.section(field.section(), DICTIONARY_VERSION)?;
                Ok(Map::new(bytes)?)
            })
            .collect::<Result<Vec<_>>>()?;
        let section = |name: &str| -> Result<Option<Bytes>> {
            if container.sections().contains_key(name) {
                Ok(Some(container.section(name, DICTIONARY_VERSION)?))
            } else {
                Ok(None)
            }
        };
        let mut dictionaries = Self::from_maps(maps);
        if let (Some(areas), Some(area_ids), Some(nesting)) = (
            section(AREAS_SECTION)?,
            section(AREA_IDS_SECTION)?,
            section(NESTING_SECTION)?,
        ) {
            dictionaries.areas =
                Map::new(areas).map_err(|_| Error::Decode("invalid dictionary map in the Pack"))?;
            dictionaries.area_ids = area_ids;
            dictionaries.nesting = nesting;
        }
        if let Some(region_groups) = section(REGION_GROUPS_SECTION)? {
            dictionaries.region_groups = region_groups;
        }
        Ok(dictionaries)
    }

    /// Empty dictionaries, for a Pack without them.
    pub fn empty() -> Self {
        Self::from_maps(Field::ALL.iter().map(|_| empty_map()).collect())
    }

    /// Dictionaries from `(field, normalized name, entry)` rows, as the Pack
    /// writer would store them.
    pub fn from_entries<'a>(entries: impl IntoIterator<Item = (Field, &'a str, Entry)>) -> Self {
        let mut fields = vec![BTreeMap::new(); Field::ALL.len()];
        for (field, key, entry) in entries {
            fields[field.index()].insert(key.to_string(), entry);
        }
        Self::from_maps(
            fields
                .iter()
                .map(|entries| {
                    let bytes = build_fst(entries).expect("in-memory FST");
                    Map::new(Bytes::new(bytes)).expect("valid FST")
                })
                .collect(),
        )
    }

    fn from_maps(maps: Vec<Map<Bytes>>) -> Self {
        let mut countries = HashMap::new();
        let mut stream = maps[Field::Country.index()].stream();
        while let Some((_, value)) = stream.next() {
            let entry = Entry::decode(value);
            if let Some(country) = entry.country {
                *countries.entry(country).or_default() += entry.count;
            }
        }
        Self {
            maps,
            areas: empty_values(),
            area_ids: Bytes::empty(),
            nesting: Bytes::empty(),
            region_groups: Bytes::empty(),
            countries,
        }
    }

    /// The same dictionaries with the areas each name names and the pairs
    /// of areas one record lies in, as the Pack writer stores them.
    #[cfg(test)]
    pub(crate) fn with_areas(mut self, areas: &[(&str, &[u32])], nesting: &[(u32, u32)]) -> Self {
        let lists = areas
            .iter()
            .map(|(key, ids)| (key.to_string(), ids.to_vec()))
            .collect();
        let (map, ids) = area_sections(lists);
        self.areas = Map::new(Bytes::new(map)).expect("valid FST");
        self.area_ids = Bytes::new(ids);
        self.nesting = Bytes::new(nesting_section(
            nesting
                .iter()
                .map(|&(left, right)| nesting_pair(left, right))
                .collect(),
        ));
        self
    }

    fn map(&self, field: Field) -> &Map<Bytes> {
        &self.maps[field.index()]
    }

    /// Names in `field`.
    pub fn len(&self, field: Field) -> usize {
        self.map(field).len()
    }

    pub fn get(&self, field: Field, key: &str) -> Option<Entry> {
        self.map(field).get(key).map(Entry::decode)
    }

    pub fn contains(&self, field: Field, key: &str) -> bool {
        self.map(field).contains_key(key)
    }

    /// Whether `key` says what kind of street or place a name is or which
    /// way it runs ("road", "north", "oblast") rather than naming it: a
    /// street type, a direction or a qualifier.
    pub fn describes_street(&self, key: &str) -> bool {
        self.contains(Field::StreetType, key)
            || self.contains(Field::Directional, key)
            || is_admin_type(key)
    }

    /// Every name in `field`, in key order.
    pub fn keys(&self, field: Field) -> Vec<String> {
        let mut keys = Vec::with_capacity(self.len(field));
        let mut stream = self.map(field).keys();
        while let Some(key) = stream.next() {
            keys.push(String::from_utf8_lossy(key).into_owned());
        }
        keys
    }

    /// The best name `dfa` accepts that starts with `prefix`: fewest edits,
    /// then most used. The prefix keeps the walk to one branch of the FST:
    /// without it, two edits reach every two-character start of every name.
    pub fn fuzzy(&self, field: Field, dfa: &DFA, prefix: &str) -> Option<Match> {
        // No key holds the byte 0xFF, which UTF-8 never uses, so every key
        // with the prefix sorts below it.
        let mut upper = prefix.as_bytes().to_vec();
        upper.push(0xFF);
        let mut stream = self
            .map(field)
            .search_with_state(Levenshtein(dfa))
            .ge(prefix)
            .lt(upper)
            .into_stream();
        let mut best: Option<Match> = None;
        while let Some((_, value, state)) = stream.next() {
            let Distance::Exact(distance) = dfa.distance(state) else {
                continue;
            };
            let found = Match {
                entry: Entry::decode(value),
                distance,
            };
            if best.is_none_or(|best| {
                (found.distance, std::cmp::Reverse(found.entry.count))
                    < (best.distance, std::cmp::Reverse(best.entry.count))
            }) {
                best = Some(found);
            }
        }
        best
    }

    /// The record ids of the areas named `key`; empty when it names none, or
    /// more than [`MAX_AREAS_PER_NAME`].
    pub fn areas(&self, key: &str) -> Vec<u32> {
        let Some(value) = self.areas.get(key) else {
            return Vec::new();
        };
        let (offset, count) = ((value >> 8) as usize, (value & 0xFF) as usize);
        self.area_ids
            .get(offset * 4..(offset + count) * 4)
            .map(|bytes| {
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|chunk| u32::from_le_bytes(*chunk))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The groups of regions the region `region` lies in ("関東地方" of
    /// "千葉県").
    pub fn region_groups(&self, region: u32) -> Vec<u32> {
        let pair_at = |index: usize| {
            u64::from_le_bytes(
                self.region_groups[index * 8..index * 8 + 8]
                    .try_into()
                    .expect("eight bytes"),
            )
        };
        let pairs = self.region_groups.len() / 8;
        let (mut low, mut high) = (0, pairs);
        while low < high {
            let middle = (low + high) / 2;
            if ((pair_at(middle) >> 32) as u32) < region {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        (low..pairs)
            .map(pair_at)
            .take_while(|pair| (pair >> 32) as u32 == region)
            .map(|pair| pair as u32)
            .collect()
    }

    /// Whether places of the areas `left` and of the areas `right` can be
    /// named in one address: two different places, one of each, lie one in
    /// the other. One place named twice is not.
    pub fn nest(&self, left: &[u32], right: &[u32]) -> bool {
        let pairs = self.nesting.len() / 8;
        let pair_at = |index: usize| {
            u64::from_le_bytes(
                self.nesting[index * 8..index * 8 + 8]
                    .try_into()
                    .expect("eight bytes"),
            )
        };
        left.iter().any(|left| {
            right.iter().any(|right| {
                if left == right {
                    return false;
                }
                let wanted = nesting_pair(*left, *right);
                let (mut low, mut high) = (0, pairs);
                while low < high {
                    let middle = (low + high) / 2;
                    match pair_at(middle).cmp(&wanted) {
                        std::cmp::Ordering::Less => low = middle + 1,
                        std::cmp::Ordering::Greater => high = middle,
                        std::cmp::Ordering::Equal => return true,
                    }
                }
                false
            })
        })
    }

    /// How strongly the Pack covers `country`, between 0 and 1.
    pub fn country_weight(&self, country: CountryCode) -> f32 {
        let total = self.countries.values().sum::<u64>();
        match self.countries.get(&country) {
            Some(count) if total > 0 => *count as f32 / total as f32,
            _ => 0.0,
        }
    }
}

/// A Levenshtein DFA as an FST automaton, so the search walks only the
/// names within its distance.
struct Levenshtein<'a>(&'a DFA);

impl Automaton for Levenshtein<'_> {
    type State = u32;

    fn start(&self) -> u32 {
        self.0.initial_state()
    }

    fn is_match(&self, state: &u32) -> bool {
        matches!(self.0.distance(*state), Distance::Exact(_))
    }

    fn can_match(&self, state: &u32) -> bool {
        *state != SINK_STATE
    }

    fn accept(&self, state: &u32, byte: u8) -> u32 {
        self.0.transition(*state, byte)
    }
}

fn empty_map() -> Map<Bytes> {
    let bytes = build_fst(&BTreeMap::new()).expect("empty FST");
    Map::new(Bytes::new(bytes)).expect("valid empty FST")
}

fn build_fst(entries: &BTreeMap<String, Entry>) -> Result<Vec<u8>> {
    let mut builder = MapBuilder::memory();
    for (key, entry) in entries {
        builder.insert(key, entry.encode())?;
    }
    Ok(builder.into_inner()?)
}

fn empty_values() -> Map<Bytes> {
    let bytes = MapBuilder::memory().into_inner().expect("empty FST");
    Map::new(Bytes::new(bytes)).expect("valid empty FST")
}

/// The FST of area name keys and the id lists it points into, from the ids
/// of the areas each key names. A key of more than [`MAX_AREAS_PER_NAME`]
/// areas is left out.
fn area_sections(lists: BTreeMap<String, Vec<u32>>) -> (Vec<u8>, Vec<u8>) {
    let mut builder = MapBuilder::memory();
    let mut ids = Vec::new();
    for (key, mut areas) in lists {
        areas.sort_unstable();
        areas.dedup();
        if areas.is_empty() || areas.len() > MAX_AREAS_PER_NAME {
            continue;
        }
        let offset = (ids.len() / 4) as u64;
        for area in &areas {
            ids.extend_from_slice(&area.to_le_bytes());
        }
        builder
            .insert(&key, (offset << 8) | areas.len() as u64)
            .expect("keys in order");
    }
    (builder.into_inner().expect("in-memory FST"), ids)
}

/// Two areas as one sortable value, the smaller id above the larger.
fn nesting_pair(left: u32, right: u32) -> u64 {
    (u64::from(left.min(right)) << 32) | u64::from(left.max(right))
}

fn nesting_section(mut pairs: Vec<u64>) -> Vec<u8> {
    pairs.sort_unstable();
    pairs.dedup();
    pairs.iter().flat_map(|pair| pair.to_le_bytes()).collect()
}

/// The names a record adds to the dictionaries, normalized. Computed beside
/// its text document, in parallel; [`Builder::add`] then counts it.
#[derive(Debug, Default)]
pub struct RecordNames {
    /// Each name with its field and the admin level it names (see
    /// [`level`]; none for a street or a POI).
    names: Vec<(Field, String, u8)>,
    /// The country its own address states (`addr:country`).
    country: Option<CountryCode>,
    /// Whether the record is a place: an admin area, or a place the Pack
    /// holds only as a point.
    place: bool,
}

impl RecordNames {
    pub fn is_place(&self) -> bool {
        self.place
    }

    pub fn of(record: &Record) -> Self {
        let mut names = Self::default();
        match record {
            Record::Address(record) => names.address(&record.address),
            Record::Poi(poi) => {
                for name in record.names() {
                    names.add(Field::Poi, Some(name), 0);
                }
                if let Some(address) = &poi.address {
                    names.address(address);
                }
            }
            Record::Interpolation(record) => {
                let address = &record.address;
                names.add(Field::Street, address.street.as_deref(), 0);
                names.add(Field::Locality, address.place.as_deref(), level::LOCALITY);
                names.add(
                    Field::Locality,
                    address.locality.as_deref(),
                    level::LOCALITY,
                );
                names.add(Field::Region, address.region.as_deref(), level::REGION);
                names.country_name(address.country.as_deref());
            }
            Record::Street(_) => {
                for name in record.names() {
                    names.add(Field::Street, Some(name), 0);
                }
            }
            Record::Place(layer, place) => {
                names.place = true;
                let (field, level) = field_and_level(*layer);
                for name in record.names() {
                    names.add(field, Some(name), level);
                }
                if let Some(code) = place.place_type.strip_prefix("derived_country:") {
                    names.country = CountryCode::parse(code);
                }
            }
            Record::Postcode(_) => {}
        }
        names
    }

    fn address(&mut self, address: &AddressComponents) {
        if let Some(unit) = &address.unit {
            for word in unit_designators(unit) {
                self.names.push((Field::Unit, word, 0));
            }
        }
        self.add(Field::Street, address.street.as_deref(), 0);
        self.add(Field::Locality, address.place.as_deref(), level::LOCALITY);
        self.add(
            Field::Locality,
            address.locality.as_deref(),
            level::LOCALITY,
        );
        for area in &address.areas {
            self.add(Field::Locality, Some(area), level::NEIGHBOURHOOD);
        }
        self.add(Field::Region, address.region.as_deref(), level::REGION);
        self.country_name(address.country.as_deref());
    }

    /// `addr:country` is usually a code ("CA"), which is both the country of
    /// the record and a country word a query may use.
    fn country_name(&mut self, value: Option<&str>) {
        self.add(Field::Country, value, level::COUNTRY);
        if let Some(code) = value.and_then(CountryCode::parse) {
            self.country = Some(code);
        }
    }

    fn add(&mut self, field: Field, value: Option<&str>, level: u8) {
        if let Some(name) = value.and_then(dictionary_key) {
            self.names.push((field, name, level));
        }
    }
}

/// How often one name is used, in which countries, and at which admin
/// levels.
#[derive(Debug, Default, Clone)]
struct NameStats {
    count: u64,
    countries: Vec<(CountryCode, u64)>,
    levels: u8,
}

impl NameStats {
    fn add(&mut self, count: u64, country: Option<CountryCode>) {
        self.count += count;
        if let Some(country) = country {
            self.add_country(country, count);
        }
    }

    fn add_country(&mut self, country: CountryCode, count: u64) {
        match self.countries.iter_mut().find(|(code, _)| *code == country) {
            Some((_, seen)) => *seen += count,
            None => self.countries.push((country, count)),
        }
    }

    fn merge(&mut self, other: &NameStats) {
        self.count += other.count;
        self.levels |= other.levels;
        for &(country, count) in &other.countries {
            self.add_country(country, count);
        }
    }

    fn entry(&self) -> Entry {
        Entry {
            count: self.count,
            country: self
                .countries
                .iter()
                .max_by_key(|(code, count)| (*count, std::cmp::Reverse(*code)))
                .map(|(code, _)| *code),
            levels: self.levels,
        }
    }
}

/// An admin area records were counted in.
#[derive(Debug)]
struct Area {
    field: Field,
    /// Its admin level, one bit of [`level`].
    level: u8,
    stats: NameStats,
}

/// A name of admin areas without their type word, before it is known not
/// to name an area of another level.
#[derive(Debug, Default)]
struct Alias {
    fields: Vec<Field>,
    stats: NameStats,
    areas: Vec<u32>,
}

/// Collects the dictionaries while the Pack writer writes records.
#[derive(Debug, Default)]
pub struct Builder {
    names: HashMap<(Field, String), NameStats>,
    /// Records lying in each admin area, by the area's record id.
    areas: HashMap<RecordId, Area>,
    /// Every admin context a record lies in, for which areas lie in which.
    contexts: HashSet<AdminContextTuple>,
    /// Each place with the areas its own context places it in, as
    /// [`nesting_pair`]s (see [`Builder::add_place`]).
    area_pairs: Vec<u64>,
    /// Each name of a place with the place's record id, so a name of a
    /// place held only as a point names it too.
    place_names: Vec<(String, u32, u8)>,
    /// The country code of each country record admin contexts point at.
    country_codes: HashMap<RecordId, CountryCode>,
    /// The groups of regions each region lies in, by the region's id.
    region_groups: HashMap<RecordId, Vec<RecordId>>,
}

impl Builder {
    /// Country codes of the country records, so every record can be counted
    /// in the country its admin context places it in.
    pub fn set_country_codes(&mut self, codes: HashMap<RecordId, CountryCode>) {
        self.country_codes = codes;
    }

    /// The groups of regions each region lies in ("関東地方" of "千葉県"):
    /// a record keeps only its region, yet lies in the region's groups too.
    pub fn set_region_groups(&mut self, groups: HashMap<RecordId, Vec<RecordId>>) {
        self.region_groups = groups;
    }

    /// Note that the place `place` lies in the areas of its own context, and
    /// which of its names name it. An area is in no context of its own
    /// layer, and a place the Pack holds only as a point (the ward node
    /// "東区" of Sapporo) in none at all, so without this its names would
    /// name only the areas of other places they name.
    pub fn add_place(&mut self, place: RecordId, context: &RecordContext, names: &RecordNames) {
        let Ok(place) = u32::try_from(place) else {
            return;
        };
        for parent in context.admin_context.parent_record_ids() {
            if let Ok(parent) = u32::try_from(parent)
                && parent != place
            {
                self.area_pairs.push(nesting_pair(place, parent));
            }
        }
        for (field, name, level) in &names.names {
            if Field::AREAS.contains(field) {
                self.place_names.push((name.clone(), place, *level));
            }
        }
    }

    /// Count one written record: its own names, and its admin areas.
    pub fn add(&mut self, names: RecordNames, context: Option<&RecordContext>) {
        let tuple = context.map(|context| &context.admin_context);
        let country = tuple
            .and_then(|tuple| tuple.country_record_id())
            .and_then(|id| self.country_codes.get(&id).copied())
            .or(names.country);
        for (field, name, level) in names.names {
            let stats = self.names.entry((field, name)).or_default();
            stats.add(1, country);
            stats.levels |= level;
        }
        let Some(tuple) = tuple else {
            return;
        };
        self.contexts.insert(tuple.clone());
        let groups = tuple
            .region_record_id()
            .and_then(|region| self.region_groups.get(&region))
            .into_iter()
            .flatten()
            .map(|group| (Field::Region, level::REGION, Some(*group)))
            .collect::<Vec<_>>();
        // Walked rather than listed per tier, so a tier added to the chain is
        // indexed without a second place to remember.
        // An area that is two tiers is indexed under both: Berlin has to be
        // found as a city and as a state.
        let tiers = tuple.entries().iter().flat_map(|entry| {
            entry.tiers.iter().map(move |tier| {
                let (field, level) = field_and_level(tier);
                (field, level, Some(entry.record_id))
            })
        });
        for (field, level, area) in groups.into_iter().chain(tiers) {
            if let Some(area) = area {
                self.areas
                    .entry(area)
                    .or_insert_with(|| Area {
                        field,
                        level,
                        stats: NameStats {
                            levels: level,
                            ..NameStats::default()
                        },
                    })
                    .stats
                    .add(1, country);
            }
        }
    }

    /// Write every dictionary section. `area_names` names the admin areas
    /// records were counted in, each area's `name` first; every name of an
    /// area is counted for the records lying in it, and so is every name of
    /// a region, district or locality without its type word
    /// ([`admin_aliases`]) that names no area of another level.
    pub fn finish(
        self,
        pack: &mut Writer,
        area_names: &HashMap<RecordId, Vec<String>>,
    ) -> Result<()> {
        let Self {
            mut names,
            areas,
            contexts,
            area_pairs,
            place_names,
            country_codes,
            region_groups,
        } = self;
        // The ids of the areas each area name names, and the levels of the
        // areas each name names.
        let mut places = BTreeMap::<String, Vec<u32>>::new();
        let mut levels = HashMap::<String, u8>::new();
        let mut aliases = HashMap::<String, Alias>::new();
        // The numbered forms of area names, with the names that share each.
        let mut numbered = HashMap::<(Field, String), (NameStats, HashSet<String>)>::new();
        for (
            area,
            Area {
                field,
                level,
                stats,
            },
        ) in &areas
        {
            let id = u32::try_from(*area).ok();
            let keys = area_names
                .get(area)
                .into_iter()
                .flatten()
                .filter_map(|name| dictionary_key(name))
                .collect::<HashSet<_>>();
            for key in keys {
                if level & level::ALIASED != 0 {
                    for alias in admin_aliases(&key) {
                        let alias = aliases.entry(alias).or_default();
                        if !alias.fields.contains(field) {
                            alias.fields.push(*field);
                        }
                        alias.stats.merge(stats);
                        alias.areas.extend(id);
                    }
                }
                if let Some(masked) = mask_numerals(&key) {
                    let (masked_stats, keys) = numbered.entry((*field, masked)).or_default();
                    masked_stats.merge(stats);
                    keys.insert(key.clone());
                }
                names.entry((*field, key.clone())).or_default().merge(stats);
                *levels.entry(key.clone()).or_default() |= level;
                places.entry(key).or_default().extend(id);
            }
        }
        // The names of each place, to tell one place held as both a point and
        // an area ("那覇市" the node and the boundary) from two places.
        let mut names_of = HashMap::<u32, HashSet<String>>::new();
        for (area, names) in area_names {
            if let Ok(area) = u32::try_from(*area) {
                names_of
                    .entry(area)
                    .or_default()
                    .extend(names.iter().filter_map(|name| dictionary_key(name)));
            }
        }
        // The levels of the places each name names, points among them.
        let mut place_levels = levels.clone();
        for (key, place, level) in place_names {
            names_of.entry(place).or_default().insert(key.clone());
            *place_levels.entry(key.clone()).or_default() |= level;
            places.entry(key).or_default().push(place);
        }
        // An alias of areas of one field joins their names, unless it is
        // already the name of an area of another field, or names areas of
        // two ("京都" of "京都府" and "京都市" is neither). Levels of one
        // field are one label, so "ōta" of the ward "ōta ku" and of the city
        // "ōta shi" is a locality either way, and the places it names are
        // told apart by the rest of the address. Names that only records
        // state ("addr:city=東京") are no areas, and often the very area the
        // alias names.
        for (key, alias) in aliases {
            let [field] = alias.fields[..] else {
                continue;
            };
            let other_field = levels
                .get(&key)
                .is_some_and(|levels| levels & !level::of_field(field) != 0);
            if other_field {
                continue;
            }
            names
                .entry((field, key.clone()))
                .or_default()
                .merge(&alias.stats);
            places.entry(key).or_default().extend(alias.areas);
        }
        // A numbered form that many areas share is a name of its own, so a
        // block, sector or ward the Pack does not hold by its number is still
        // a place of a kind it does ("5丁目" of "#丁目"). It names no area,
        // so the address check never reads one place in it.
        for ((field, masked), (stats, keys)) in numbered {
            if keys.len() < MIN_NUMBERED_NAMES
                || levels
                    .get(&masked)
                    .is_some_and(|levels| levels & !level::of_field(field) != 0)
            {
                continue;
            }
            names.entry((field, masked)).or_default().merge(&stats);
        }
        // A country is also written as its code ("ca"), as people write it.
        for (area, code) in &country_codes {
            let count = area_names
                .get(area)
                .and_then(|names| names.first())
                .and_then(|name| dictionary_key(name))
                .and_then(|name| names.get(&(Field::Country, name)))
                .map_or(1, |stats| stats.count.max(1));
            names
                .entry((Field::Country, code.as_str().to_ascii_lowercase()))
                .or_default()
                .add(count, Some(*code));
        }

        let mut fields = vec![BTreeMap::new(); Field::ALL.len()];
        for ((field, name), stats) in &names {
            fields[field.index()].insert(name.clone(), stats.entry());
        }
        fields[Field::StreetType.index()] = street_types(&fields[Field::Street.index()]);
        let mut units = vocabulary(UNIT_WORDS);
        units.extend(vocabulary(LIBPOSTAL_UNIT_TYPES));
        units.extend(vocabulary(LIBPOSTAL_STAIRCASES));
        units.extend(vocabulary(LIBPOSTAL_ENTRANCES));
        for (word, entry) in &fields[Field::Unit.index()] {
            if entry.count >= MIN_UNIT_WORD_VALUES {
                units.entry(word.clone()).or_insert(*entry).count = entry.count;
            }
        }
        fields[Field::Unit.index()] = with_glued_forms(units);
        for (field, list) in [
            (Field::PoBox, LIBPOSTAL_PO_BOXES),
            (Field::Level, LIBPOSTAL_LEVEL_TYPES),
            (Field::Building, LIBPOSTAL_BUILDING_TYPES),
            (Field::Directional, LIBPOSTAL_DIRECTIONALS),
            (Field::Near, LIBPOSTAL_NEAR),
            (Field::Category, LIBPOSTAL_CATEGORIES),
        ] {
            fields[field.index()] = match field {
                Field::PoBox | Field::Level => with_glued_forms(vocabulary(list)),
                _ => vocabulary(list),
            };
        }
        // Every word of every name but a POI's: a venue's name is any word
        // at all, and would make typos of street names known words.
        let mut words = BTreeMap::<String, Entry>::new();
        for (field, entries) in Field::ALL.into_iter().zip(&fields) {
            if field == Field::Poi {
                continue;
            }
            for name in entries.keys() {
                for word in name.split_whitespace() {
                    words
                        .entry(word.to_string())
                        .or_insert(Entry {
                            count: 0,
                            country: None,
                            levels: 0,
                        })
                        .count += 1;
                }
            }
        }
        fields[Field::Word.index()] = words;
        for (field, phrase) in vocabulary_overrides() {
            fields[field.index()].remove(&phrase);
        }
        for field in Field::ALL {
            pack.add(
                field.section(),
                DICTIONARY_VERSION,
                &build_fst(&fields[field.index()])?,
            )?;
        }
        // Which places a name names is only known where the Pack holds every
        // place of a level: countries, regions, districts and cities are all
        // drawn, neighbourhoods and blocks only here and there ("中島町" of
        // one city, "5丁目" of hundreds). A name that also names one of those
        // is no place the address check can hold to the rest of an address.
        places.retain(|key, _| {
            place_levels
                .get(key)
                .is_none_or(|levels| levels & (level::NEIGHBOURHOOD | level::PLACE) == 0)
        });
        let (map, ids) = area_sections(places);
        pack.add(AREAS_SECTION, DICTIONARY_VERSION, &map)?;
        pack.add(AREA_IDS_SECTION, DICTIONARY_VERSION, &ids)?;
        // Every two areas one record lies in lie one in the other, and so does
        // every place with the areas its own context places it in, unless the
        // two share a name and so are one place.
        let shares_name = |left: u32, right: u32| {
            names_of
                .get(&left)
                .zip(names_of.get(&right))
                .is_some_and(|(left, right)| !left.is_disjoint(right))
        };
        let mut pairs = area_pairs
            .into_iter()
            .filter(|pair| !shares_name((pair >> 32) as u32, *pair as u32))
            .collect::<Vec<_>>();
        // A record lies in the groups of its region as well.
        for context in &contexts {
            let groups = context
                .region_record_id()
                .and_then(|region| region_groups.get(&region))
                .into_iter()
                .flatten()
                .copied();
            let ids = context
                .parent_record_ids()
                .chain(groups)
                .filter_map(|id| u32::try_from(id).ok())
                .collect::<Vec<_>>();
            for (index, left) in ids.iter().enumerate() {
                for right in &ids[index + 1..] {
                    pairs.push(nesting_pair(*left, *right));
                }
            }
        }
        pack.add(NESTING_SECTION, DICTIONARY_VERSION, &nesting_section(pairs))?;
        let mut groups = region_groups
            .iter()
            .flat_map(|(region, groups)| groups.iter().map(move |group| (*region, *group)))
            .filter_map(|(region, group)| {
                let region = u64::from(u32::try_from(region).ok()?);
                Some((region << 32) | u64::from(u32::try_from(group).ok()?))
            })
            .collect::<Vec<_>>();
        groups.sort_unstable();
        groups.dedup();
        let groups = groups
            .iter()
            .flat_map(|pair| pair.to_le_bytes())
            .collect::<Vec<_>>();
        pack.add(REGION_GROUPS_SECTION, DICTIONARY_VERSION, &groups)?;
        Ok(())
    }
}

/// How an admin type word is written beside the name of its area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Attach {
    /// A word of its own before or after the name ("freistaat bayern",
    /// "aichi prefecture").
    Word,
    /// A word of its own before the name ("landkreis barnim", "aza ...").
    WordBefore,
    /// A word of its own after the name ("nagoya shi", "2 chome").
    WordAfter,
    /// Attached after the name ("愛知県", "경기도").
    After,
    /// Attached before the name ("จังหวัดเชียงใหม่").
    Before,
}

/// The admin type words of [`LIBPOSTAL_QUALIFIERS`] and [`ADMIN_TYPES`] as
/// dictionary keys with how they attach to a name. In [`ADMIN_TYPES`] and
/// [`LIBPOSTAL_ADMIN_AFFIXES`] a leading `-` attaches a word after the name
/// and a trailing `-` before it, `word ~` is a word of its own before the
/// name and `~ word` one after it, and a bare word is either. A libpostal
/// qualifier of a script written without spaces, or of Hangul, whose words
/// carry their type attached, attaches after the name; any other is a word
/// of its own after it, the side libpostal's qualifiers do not state and a
/// word before a name cannot be told from the end of the field before it
/// ("2 chome taitoku" is no "taitoku" chome).
fn admin_types() -> &'static [(String, Attach)] {
    static TYPES: OnceLock<Vec<(String, Attach)>> = OnceLock::new();
    TYPES.get_or_init(|| {
        let phrases = |list: &'static str| {
            list.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .flat_map(|line| line.split('|'))
        };
        let mut types = Vec::new();
        for phrase in phrases(LIBPOSTAL_QUALIFIERS) {
            let attached = phrase
                .chars()
                .next()
                .is_some_and(|first| is_unspaced(first) || first.script() == Script::Hangul);
            let attach = if attached {
                Attach::After
            } else {
                Attach::WordAfter
            };
            types.extend(dictionary_key(phrase).map(|key| (key, attach)));
        }
        for phrase in phrases(ADMIN_TYPES).chain(phrases(LIBPOSTAL_ADMIN_AFFIXES)) {
            let (phrase, attach) = if let Some(phrase) = phrase.strip_prefix('-') {
                (phrase, Attach::After)
            } else if let Some(phrase) = phrase.strip_suffix('-') {
                (phrase, Attach::Before)
            } else if let Some(phrase) = phrase.strip_suffix(" ~") {
                (phrase, Attach::WordBefore)
            } else if let Some(phrase) = phrase.strip_prefix("~ ") {
                (phrase, Attach::WordAfter)
            } else {
                (phrase, Attach::Word)
            };
            types.extend(dictionary_key(phrase).map(|key| (key, attach)));
        }
        types.sort();
        types.dedup();
        types
    })
}

/// Whether `key` is a word of an admin area's type ("丁目", "chome", "kreis").
pub(super) fn is_admin_type(key: &str) -> bool {
    admin_types().iter().any(|(phrase, _)| phrase == key)
}

/// The names an admin area's name `key` (a dictionary key) is also written
/// as without the word of its type: "愛知" of "愛知県", "bayern" of
/// "freistaat bayern", "madrid" of "comunidad de madrid", "경기" of "경기도".
/// What is left must be two or more characters with no numeral, so "北区"
/// and "3丁目" have none.
pub(super) fn admin_aliases(key: &str) -> Vec<String> {
    let mut aliases = Vec::new();
    for (phrase, attach) in admin_types() {
        let rests = match attach {
            Attach::Word => [
                key.strip_suffix(phrase.as_str())
                    .and_then(|rest| rest.strip_suffix(' ')),
                key.strip_prefix(phrase.as_str())
                    .and_then(|rest| rest.strip_prefix(' ')),
            ],
            Attach::WordBefore => [
                key.strip_prefix(phrase.as_str())
                    .and_then(|rest| rest.strip_prefix(' ')),
                None,
            ],
            Attach::WordAfter => [
                key.strip_suffix(phrase.as_str())
                    .and_then(|rest| rest.strip_suffix(' ')),
                None,
            ],
            Attach::After => [
                key.strip_suffix(phrase.as_str())
                    .filter(|rest| !rest.ends_with(' ')),
                None,
            ],
            Attach::Before => [
                key.strip_prefix(phrase.as_str())
                    .filter(|rest| !rest.starts_with(' ')),
                None,
            ],
        };
        for rest in rests.into_iter().flatten() {
            let usable = rest.chars().count() >= 2
                && !rest.chars().any(is_numeral)
                && rest.chars().any(char::is_alphabetic);
            if usable && !aliases.iter().any(|alias| alias == rest) {
                aliases.push(rest.to_string());
            }
        }
    }
    aliases
}

/// The words that start or end many street names, with the vendored types.
fn street_types(streets: &BTreeMap<String, Entry>) -> BTreeMap<String, Entry> {
    let mut ends = HashMap::<&str, u64>::new();
    for name in streets.keys() {
        let words = name.split_whitespace().collect::<Vec<_>>();
        if words.len() < 2 {
            continue;
        }
        let distinct = [words[0], words[words.len() - 1]]
            .into_iter()
            .collect::<HashSet<_>>();
        for word in distinct {
            *ends.entry(word).or_default() += 1;
        }
    }
    let threshold = ((streets.len() as f64 * STREET_TYPE_SHARE) as u64).max(MIN_STREET_TYPE_NAMES);
    let mut types = vocabulary(STREET_TYPE_WORDS);
    types.extend(vocabulary(LIBPOSTAL_STREET_TYPES));
    for (word, count) in ends {
        if count >= threshold && !word.chars().any(|character| character.is_numeric()) {
            types
                .entry(word.to_string())
                .or_insert(Entry {
                    count: 0,
                    country: None,
                    levels: 0,
                })
                .count += count;
        }
    }
    types
}

/// The words of an `addr:unit` value that say what kind of unit its number
/// is: the words of letters of a value that also holds a number ("suite" and
/// "floor" of "Suite 200, Floor 3"). A word of a script written without
/// spaces is one character per unit of the parser, never a unit word.
fn unit_designators(value: &str) -> Vec<String> {
    let tokens = super::Tokens::new(value);
    if !tokens
        .words
        .iter()
        .any(|word| word.chars().any(char::is_numeric))
    {
        return Vec::new();
    }
    let mut words = tokens
        .words
        .into_iter()
        .filter(|word| {
            word.chars().count() >= 2
                && word.chars().all(char::is_alphabetic)
                && !word.chars().any(super::is_unspaced)
        })
        .collect::<Vec<_>>();
    words.sort_unstable();
    words.dedup();
    words
}

/// A vendored word list as dictionary entries: one phrase per line, its
/// variants separated by `|`, `#` lines comments. Every language of a list
/// is read alike.
/// The vendored words [`VOCABULARY_OVERRIDES`] drops, as dictionary keys.
fn vocabulary_overrides() -> Vec<(Field, String)> {
    VOCABULARY_OVERRIDES
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let (field, phrase) = line.split_once('\t')?;
            let field = match field {
                "unit" => Field::Unit,
                "po_box" => Field::PoBox,
                "level" => Field::Level,
                "building" => Field::Building,
                "street_type" => Field::StreetType,
                "directional" => Field::Directional,
                "near" => Field::Near,
                "category" => Field::Category,
                _ => return None,
            };
            Some((field, dictionary_key(phrase)?))
        })
        .collect()
}

/// A phrase of several words is also written glued into one, beside its
/// number ("pobox29", "shishobako4256"), so its glued form is a key too.
fn with_glued_forms(mut entries: BTreeMap<String, Entry>) -> BTreeMap<String, Entry> {
    let glued = entries
        .iter()
        .filter(|(key, _)| key.contains(' '))
        .map(|(key, entry)| (key.replace(' ', ""), *entry))
        .collect::<Vec<_>>();
    for (key, entry) in glued {
        entries.entry(key).or_insert(entry);
    }
    entries
}

pub(super) fn vocabulary(list: &str) -> BTreeMap<String, Entry> {
    list.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .flat_map(|line| line.split('|'))
        .filter_map(dictionary_key)
        .map(|word| {
            (
                word,
                Entry {
                    count: 1,
                    country: None,
                    levels: 0,
                },
            )
        })
        .collect()
}

/// The dictionary field and level bit a context tier is indexed under.
///
/// A sub-tier is indexed at the level of the tier it divides: a query naming
/// a district still has to match a country whose districts are mapped one
/// level deeper, which is most of them.
fn field_and_level(tier: PlaceLayer) -> (Field, u8) {
    match tier {
        PlaceLayer::Country => (Field::Country, level::COUNTRY),
        PlaceLayer::Region | PlaceLayer::SubRegion => (Field::Region, level::REGION),
        PlaceLayer::District | PlaceLayer::SubDistrict => (Field::Locality, level::DISTRICT),
        PlaceLayer::Locality | PlaceLayer::SubLocality => (Field::Locality, level::LOCALITY),
        PlaceLayer::Neighbourhood | PlaceLayer::SubNeighbourhood => {
            (Field::Locality, level::NEIGHBOURHOOD)
        }
        PlaceLayer::Place => (Field::Locality, level::PLACE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_words_are_dropped_only_on_the_side_they_are_written() {
        let aliases = |key: &str| admin_aliases(key);
        assert!(
            aliases("bundesland nordrhein westfalen").contains(&"nordrhein westfalen".to_string())
        );
        assert!(aliases("landkreis barnim").contains(&"barnim".to_string()));
        assert!(aliases("nagoya shi").contains(&"nagoya".to_string()));
        assert!(aliases("nagoyashi").contains(&"nagoya".to_string()));
        // "chome" follows its number: before a name it is the end of the
        // field before, not this name's type.
        assert!(!aliases("chome taitoku").contains(&"taitoku".to_string()));
        assert!(!aliases("shi nagoya").contains(&"nagoya".to_string()));
    }

    /// The names of `names` with their fields.
    fn named(names: &RecordNames) -> Vec<(Field, String)> {
        names
            .names
            .iter()
            .map(|(field, name, _)| (*field, name.clone()))
            .collect()
    }

    fn entry(count: u64, country: Option<&str>) -> Entry {
        Entry {
            count,
            country: country.and_then(CountryCode::parse),
            levels: 0,
        }
    }

    #[test]
    fn entries_round_trip_count_and_country() {
        for value in [
            entry(0, None),
            entry(7, Some("CA")),
            entry(COUNT_MASK, Some("de")),
        ] {
            assert_eq!(Entry::decode(value.encode()), value);
        }
        assert_eq!(
            Entry::decode(entry(u64::MAX, None).encode()).count,
            COUNT_MASK
        );
    }

    #[test]
    fn fuzzy_lookup_finds_the_nearest_most_used_name() {
        let dictionaries = Dictionaries::from_entries([
            (Field::Locality, "toronto", entry(900, Some("CA"))),
            (Field::Locality, "torino", entry(50, Some("IT"))),
            (Field::Locality, "trenton", entry(20, Some("CA"))),
        ]);
        let one = levenshtein_automata::LevenshteinAutomatonBuilder::new(1, true);
        let two = levenshtein_automata::LevenshteinAutomatonBuilder::new(2, true);
        let hit = dictionaries
            .fuzzy(Field::Locality, &one.build_dfa("torontoo"), "t")
            .expect("one edit");
        assert_eq!(
            hit,
            Match {
                entry: entry(900, Some("CA")),
                distance: 1
            }
        );
        // A transposition is one edit.
        let hit = dictionaries
            .fuzzy(Field::Locality, &one.build_dfa("tornoto"), "t")
            .expect("transposed");
        assert_eq!(hit.distance, 1);
        assert!(
            dictionaries
                .fuzzy(Field::Locality, &one.build_dfa("toront0x"), "t")
                .is_none()
        );
        // Two edits reach both "torino" and "toronto" from "toroino"; the
        // nearer wins, then the more used.
        let hit = dictionaries
            .fuzzy(Field::Locality, &two.build_dfa("torino"), "t")
            .expect("exact");
        assert_eq!(hit.distance, 0);
        assert!(
            dictionaries
                .fuzzy(Field::Street, &two.build_dfa("toronto"), "t")
                .is_none()
        );
        assert_eq!(
            dictionaries.country_weight(CountryCode::parse("CA").unwrap()),
            0.0
        );
    }

    #[test]
    fn street_types_are_the_words_many_street_names_start_or_end_with() {
        let mut streets = BTreeMap::new();
        for index in 0..30 {
            streets.insert(format!("name{index} street"), entry(1, None));
            streets.insert(format!("rue name{index}"), entry(1, None));
        }
        streets.insert("king street west".to_string(), entry(1, None));
        let types = street_types(&streets);
        assert!(types.contains_key("street"));
        assert!(types.contains_key("rue"));
        // From the vendored list, whatever the Pack holds.
        assert!(types.contains_key("strasse"));
        assert!(!types.contains_key("king"));
        assert!(!types.contains_key("name1"));
    }

    #[test]
    fn places_are_named_by_every_name_with_unspaced_scripts_glued() {
        let place = Record::Place(
            PlaceLayer::Locality,
            crate::record::PlaceRecord {
                name: "横浜市".into(),
                alt_names: vec!["よこはま し".into(), "Yokohama-shi".into()],
                place_type: "admin_level:7".into(),
                geometry: crate::record::point_geometry(139.6, 35.4),
                source: crate::record::SourceProvenance::osm(
                    crate::record::ObjectType::Relation,
                    1,
                ),
            },
        );
        assert_eq!(
            named(&RecordNames::of(&place)),
            vec![
                (Field::Locality, "横浜市".to_string()),
                (Field::Locality, "よこはまし".to_string()),
                (Field::Locality, "yokohama shi".to_string()),
            ]
        );
    }

    #[test]
    fn pois_are_named_by_every_name() {
        let poi = Record::Poi(crate::record::PoiRecord {
            name: "Cafe Uno".into(),
            alt_names: vec!["カフェ ウノ".into()],
            category: "amenity:cafe".into(),
            address: None,
            geometry: crate::record::point_geometry(0.0, 0.0),
            location_precision: crate::record::LocationPrecision::Point,
            source: crate::record::SourceProvenance::osm(crate::record::ObjectType::Node, 1),
        });
        assert_eq!(
            named(&RecordNames::of(&poi)),
            vec![
                (Field::Poi, "cafe uno".to_string()),
                (Field::Poi, "カフェウノ".to_string()),
            ]
        );
    }

    #[test]
    fn every_name_of_an_area_counts_the_records_in_it() {
        let mut builder = Builder::default();
        let context = RecordContext {
            admin_context: crate::pack::AdminContextTuple::from_tiers([(PlaceLayer::Locality, 9)]),
            flags: 0,
        };
        for _ in 0..3 {
            builder.add(RecordNames::default(), Some(&context));
        }
        let names = HashMap::from([(9, vec!["千代田区".to_string(), "Chiyoda".to_string()])]);
        let dir = std::env::temp_dir().join(format!("og-dictionary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("pack.ogp");
        let mut pack = Writer::create(&path).expect("pack");
        builder.finish(&mut pack, &names).expect("finish");
        pack.finish().expect("seal");
        let dictionaries =
            Dictionaries::open(&Container::open(&path).expect("open")).expect("dicts");
        for key in ["千代田区", "chiyoda"] {
            assert_eq!(
                dictionaries
                    .get(Field::Locality, key)
                    .map(|entry| entry.count),
                Some(3),
                "{key}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn admin_areas_are_also_named_without_their_type_word() {
        let aliases = |key: &str| admin_aliases(key);
        assert_eq!(aliases("愛知県"), ["愛知"]);
        assert_eq!(aliases("東京都"), ["東京"]);
        assert_eq!(aliases("とうきょうと"), ["とうきょう"]);
        assert_eq!(aliases("freistaat bayern"), ["bayern"]);
        assert!(aliases("comunidad de madrid").contains(&"madrid".to_string()));
        assert_eq!(aliases("aichi prefecture"), ["aichi"]);
        assert_eq!(aliases("경기도"), ["경기"]);
        assert_eq!(aliases("จังหวัดเชียงใหม่"), ["เชียงใหม่"]);
        // Too little or a number is left: no alias.
        assert!(aliases("北区").is_empty());
        assert!(aliases("3丁目").is_empty());
        // A type word inside a word is no type word.
        assert!(aliases("statesville").is_empty());
    }

    #[test]
    fn aliases_and_nesting_come_from_the_areas_records_lie_in() {
        let mut builder = Builder::default();
        let context = |region: RecordId, locality: RecordId| RecordContext {
            admin_context: AdminContextTuple::from_tiers([
                (PlaceLayer::Region, region),
                (PlaceLayer::Locality, locality),
            ]),
            flags: 0,
        };
        for (region, locality) in [(1, 2), (3, 4), (3, 5)] {
            builder.add(RecordNames::default(), Some(&context(region, locality)));
        }
        let names = HashMap::from([
            (1, vec!["愛知県".to_string()]),
            (2, vec!["名古屋市".to_string()]),
            (3, vec!["京都府".to_string()]),
            (4, vec!["京都市".to_string()]),
            (5, vec!["宇治市".to_string()]),
        ]);
        let dir = std::env::temp_dir().join(format!("og-aliases-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("pack.ogp");
        let mut pack = Writer::create(&path).expect("pack");
        builder.finish(&mut pack, &names).expect("finish");
        pack.finish().expect("seal");
        let dictionaries =
            Dictionaries::open(&Container::open(&path).expect("open")).expect("dicts");
        assert!(dictionaries.contains(Field::Region, "愛知"));
        assert!(dictionaries.contains(Field::Locality, "名古屋"));
        // "京都" names a region and a locality: neither.
        assert!(!dictionaries.contains(Field::Region, "京都"));
        assert!(!dictionaries.contains(Field::Locality, "京都"));
        assert_eq!(dictionaries.areas("愛知"), [1]);
        assert_eq!(dictionaries.areas("名古屋市"), [2]);
        assert!(dictionaries.nest(&[1], &[2]));
        assert!(dictionaries.nest(&[3], &[4]) && dictionaries.nest(&[5], &[3]));
        assert!(!dictionaries.nest(&[1], &[4]), "Kyoto is not in Aichi");
        assert!(!dictionaries.nest(&[4], &[5]), "two cities of one region");
        assert!(dictionaries.areas("札幌").is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_name_of_too_many_areas_lists_none() {
        let many = (0..=MAX_AREAS_PER_NAME as u32).collect::<Vec<_>>();
        let dictionaries = Dictionaries::empty().with_areas(
            &[("中央区", &many[..]), ("東京都", &[7]), ("銀座", &[9, 8])],
            &[(8, 7)],
        );
        assert!(dictionaries.areas("中央区").is_empty());
        assert_eq!(dictionaries.areas("銀座"), [8, 9]);
        assert!(dictionaries.nest(&[7], &[9, 8]));
        assert!(!dictionaries.nest(&[7], &[9]));
    }

    #[test]
    fn words_many_units_are_written_with_are_unit_words() {
        let address = |unit: &str| {
            Record::Address(crate::record::AddressRecord {
                street_postcodes: Vec::new(),
                address: AddressComponents {
                    areas: Vec::new(),
                    number: "1".into(),
                    street: Some("Main Street".into()),
                    place: None,
                    unit: Some(unit.into()),
                    locality: None,
                    region: None,
                    postcode: None,
                    country: None,
                },
                geometry: crate::record::point_geometry(0.0, 0.0),
                location_precision: crate::record::LocationPrecision::Point,
                source: crate::record::SourceProvenance::osm(crate::record::ObjectType::Node, 1),
            })
        };
        let mut builder = Builder::default();
        for index in 0..MIN_UNIT_WORD_VALUES {
            builder.add(RecordNames::of(&address(&format!("Bay {index}"))), None);
        }
        builder.add(RecordNames::of(&address("Booth 1")), None);
        let dir = std::env::temp_dir().join(format!("og-units-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("pack.ogp");
        let mut pack = Writer::create(&path).expect("pack");
        builder.finish(&mut pack, &HashMap::new()).expect("finish");
        pack.finish().expect("seal");
        let dictionaries =
            Dictionaries::open(&Container::open(&path).expect("open")).expect("dicts");
        assert!(dictionaries.contains(Field::Unit, "bay"));
        assert!(!dictionaries.contains(Field::Unit, "booth"), "too few");
        assert!(dictionaries.contains(Field::Unit, "suite"), "vendored");
        assert!(dictionaries.keys(Field::Unit).contains(&"bay".to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unit_designators_are_the_words_of_a_numbered_unit() {
        assert_eq!(unit_designators("Suite 200, Floor 3"), ["floor", "suite"]);
        assert_eq!(unit_designators("Apt. 4B"), ["apt"]);
        assert!(unit_designators("Penthouse").is_empty(), "no number");
        assert!(unit_designators("B 12").is_empty(), "one letter");
        assert!(unit_designators("301号室").is_empty(), "unspaced script");
    }

    #[test]
    fn records_name_their_streets_areas_and_country() {
        let names = RecordNames::of(&Record::Address(crate::record::AddressRecord {
            street_postcodes: Vec::new(),
            address: AddressComponents {
                areas: vec!["Liberty Village".into()],
                number: "670".into(),
                street: Some("King Street West".into()),
                place: None,
                unit: None,
                locality: Some("Toronto".into()),
                region: Some("ON".into()),
                postcode: Some("M5V 1M9".into()),
                country: Some("CA".into()),
            },
            geometry: crate::record::point_geometry(-79.4, 43.6),
            location_precision: crate::record::LocationPrecision::Point,
            source: crate::record::SourceProvenance::osm(crate::record::ObjectType::Node, 1),
        }));
        assert_eq!(
            names
                .names
                .iter()
                .map(|(_, _, level)| *level)
                .collect::<Vec<_>>(),
            [
                0,
                level::LOCALITY,
                level::NEIGHBOURHOOD,
                level::REGION,
                level::COUNTRY
            ]
        );
        assert_eq!(
            named(&names),
            vec![
                (Field::Street, "king street west".to_string()),
                (Field::Locality, "toronto".to_string()),
                (Field::Locality, "liberty village".to_string()),
                (Field::Region, "on".to_string()),
                (Field::Country, "ca".to_string()),
            ]
        );
        assert_eq!(names.country, CountryCode::parse("CA"));
    }
}
