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
//! designate many of the Pack's `addr:unit` values. POI names are a field of
//! their own, so a venue's name is known as one. A `words` section holds
//! every word of every name but a POI's, so a lookup knows when a word is no
//! typo.
//!
//! A Pack built before these sections existed still opens: its dictionaries
//! are empty and the parser runs on token shapes alone.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
};

use anyhow::{Context, Result};
use fst::{Automaton, IntoStreamer, Map, MapBuilder, Streamer};
use levenshtein_automata::{DFA, Distance, SINK_STATE};

use super::{CountryCode, dictionary_key};
use crate::{
    container::{Bytes, Container, ContainerWriter},
    pack::{RecordContext, RecordId},
    record::{AddressComponents, PlaceLayer, Record},
};

/// Format version of every `parse/` dictionary section.
pub const DICTIONARY_VERSION: u32 = 2;
/// Bits of an FST value that hold the count; the country code sits above.
const COUNT_BITS: u32 = 40;
const COUNT_MASK: u64 = (1 << COUNT_BITS) - 1;
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
const STREET_TYPE_WORDS: &str = include_str!("data/street_types.txt");
/// Words that introduce a unit, floor or building part, one per line.
const UNIT_WORDS: &str = include_str!("data/unit_words.txt");

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
}

impl Field {
    pub const ALL: [Field; 8] = [
        Field::Locality,
        Field::Region,
        Field::Street,
        Field::Country,
        Field::Poi,
        Field::StreetType,
        Field::Unit,
        Field::Word,
    ];
    /// The fields of names that a whole span of a query can be.
    pub const NAMES: [Field; 5] = [
        Field::Locality,
        Field::Region,
        Field::Street,
        Field::Country,
        Field::Poi,
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
}

impl Entry {
    fn encode(self) -> u64 {
        self.count.min(COUNT_MASK)
            | self
                .country
                .map_or(0, |country| u64::from(country.to_u16()) << COUNT_BITS)
    }

    fn decode(value: u64) -> Self {
        Self {
            count: value & COUNT_MASK,
            country: CountryCode::from_u16((value >> COUNT_BITS) as u16),
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
                Map::new(bytes).with_context(|| format!("invalid Pack section {}", field.section()))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self::from_maps(maps))
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
        Self { maps, countries }
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

/// The names a record adds to the dictionaries, normalized. Computed beside
/// its text document, in parallel; [`DictionaryBuilder::add`] then counts it.
#[derive(Debug, Default)]
pub struct RecordNames {
    names: Vec<(Field, String)>,
    /// The country its own address states (`addr:country`).
    country: Option<CountryCode>,
}

impl RecordNames {
    pub fn of(record: &Record) -> Self {
        let mut names = Self::default();
        match record {
            Record::Address(record) => names.address(&record.address),
            Record::Poi(poi) => {
                for name in record.names() {
                    names.add(Field::Poi, Some(name));
                }
                if let Some(address) = &poi.address {
                    names.address(address);
                }
            }
            Record::Interpolation(record) => {
                let address = &record.address;
                names.add(Field::Street, address.street.as_deref());
                names.add(Field::Locality, address.place.as_deref());
                names.add(Field::Locality, address.locality.as_deref());
                names.add(Field::Region, address.region.as_deref());
                names.country_name(address.country.as_deref());
            }
            Record::Street(_) => {
                for name in record.names() {
                    names.add(Field::Street, Some(name));
                }
            }
            Record::Place(layer, place) => {
                let field = match layer {
                    PlaceLayer::Country => Field::Country,
                    PlaceLayer::Region => Field::Region,
                    _ => Field::Locality,
                };
                for name in record.names() {
                    names.add(field, Some(name));
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
                self.names.push((Field::Unit, word));
            }
        }
        self.add(Field::Street, address.street.as_deref());
        self.add(Field::Locality, address.place.as_deref());
        self.add(Field::Locality, address.locality.as_deref());
        for area in &address.areas {
            self.add(Field::Locality, Some(area));
        }
        self.add(Field::Region, address.region.as_deref());
        self.country_name(address.country.as_deref());
    }

    /// `addr:country` is usually a code ("CA"), which is both the country of
    /// the record and a country word a query may use.
    fn country_name(&mut self, value: Option<&str>) {
        self.add(Field::Country, value);
        if let Some(code) = value.and_then(CountryCode::parse) {
            self.country = Some(code);
        }
    }

    fn add(&mut self, field: Field, value: Option<&str>) {
        if let Some(name) = value.and_then(dictionary_key) {
            self.names.push((field, name));
        }
    }
}

/// How often one name is used, and in which countries.
#[derive(Debug, Default)]
struct NameStats {
    count: u64,
    countries: Vec<(CountryCode, u64)>,
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
        }
    }
}

/// Collects the dictionaries while the Pack writer writes records.
#[derive(Debug, Default)]
pub struct DictionaryBuilder {
    names: HashMap<(Field, String), NameStats>,
    /// Records lying in each admin area, by the area's record id and the
    /// field its name belongs to.
    areas: HashMap<RecordId, (Field, NameStats)>,
    /// The country code of each country record admin contexts point at.
    country_codes: HashMap<RecordId, CountryCode>,
}

impl DictionaryBuilder {
    /// Country codes of the country records, so every record can be counted
    /// in the country its admin context places it in.
    pub fn set_country_codes(&mut self, codes: HashMap<RecordId, CountryCode>) {
        self.country_codes = codes;
    }

    /// Count one written record: its own names, and its admin areas.
    pub fn add(&mut self, names: RecordNames, context: Option<&RecordContext>) {
        let tuple = context.map(|context| context.admin_context);
        let country = tuple
            .and_then(|tuple| tuple.country_record_id)
            .and_then(|id| self.country_codes.get(&id).copied())
            .or(names.country);
        for (field, name) in names.names {
            self.names.entry((field, name)).or_default().add(1, country);
        }
        let Some(tuple) = tuple else {
            return;
        };
        for (field, area) in [
            (Field::Country, tuple.country_record_id),
            (Field::Region, tuple.region_record_id),
            (Field::Locality, tuple.district_record_id),
            (Field::Locality, tuple.locality_record_id),
            (Field::Locality, tuple.neighbourhood_record_id),
            (Field::Locality, tuple.place_record_id),
        ] {
            if let Some(area) = area {
                self.areas
                    .entry(area)
                    .or_insert_with(|| (field, NameStats::default()))
                    .1
                    .add(1, country);
            }
        }
    }

    /// Write every dictionary section. `area_names` names the admin areas
    /// records were counted in, each area's `name` first; every name of an
    /// area is counted for the records lying in it.
    pub fn finish(
        self,
        pack: &mut ContainerWriter,
        area_names: &HashMap<RecordId, Vec<String>>,
    ) -> Result<()> {
        let Self {
            mut names,
            areas,
            country_codes,
        } = self;
        for (area, (field, stats)) in areas {
            let keys = area_names
                .get(&area)
                .into_iter()
                .flatten()
                .filter_map(|name| dictionary_key(name))
                .collect::<HashSet<_>>();
            for key in keys {
                names.entry((field, key)).or_default().merge(&stats);
            }
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
        for (word, entry) in &fields[Field::Unit.index()] {
            if entry.count >= MIN_UNIT_WORD_VALUES {
                units.entry(word.clone()).or_insert(*entry).count = entry.count;
            }
        }
        fields[Field::Unit.index()] = units;
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
                        })
                        .count += 1;
                }
            }
        }
        fields[Field::Word.index()] = words;
        for field in Field::ALL {
            pack.add(
                field.section(),
                DICTIONARY_VERSION,
                &build_fst(&fields[field.index()])?,
            )?;
        }
        Ok(())
    }
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
    for (word, count) in ends {
        if count >= threshold && !word.chars().any(|character| character.is_numeric()) {
            types
                .entry(word.to_string())
                .or_insert(Entry {
                    count: 0,
                    country: None,
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

/// A vendored word list as dictionary entries.
fn vocabulary(list: &str) -> BTreeMap<String, Entry> {
    list.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(dictionary_key)
        .map(|word| {
            (
                word,
                Entry {
                    count: 1,
                    country: None,
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(count: u64, country: Option<&str>) -> Entry {
        Entry {
            count,
            country: country.and_then(CountryCode::parse),
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
                    crate::record::OsmObjectType::Relation,
                    1,
                ),
            },
        );
        assert_eq!(
            RecordNames::of(&place).names,
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
            source: crate::record::SourceProvenance::osm(crate::record::OsmObjectType::Node, 1),
        });
        assert_eq!(
            RecordNames::of(&poi).names,
            vec![
                (Field::Poi, "cafe uno".to_string()),
                (Field::Poi, "カフェウノ".to_string()),
            ]
        );
    }

    #[test]
    fn every_name_of_an_area_counts_the_records_in_it() {
        let mut builder = DictionaryBuilder::default();
        let context = RecordContext {
            admin_context: crate::pack::AdminContextTuple {
                locality_record_id: Some(9),
                ..Default::default()
            },
            flags: 0,
        };
        for _ in 0..3 {
            builder.add(RecordNames::default(), Some(&context));
        }
        let names = HashMap::from([(9, vec!["千代田区".to_string(), "Chiyoda".to_string()])]);
        let dir = std::env::temp_dir().join(format!("og-dictionary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("pack.ogp");
        let mut pack = ContainerWriter::create(&path).expect("pack");
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
    fn words_many_units_are_written_with_are_unit_words() {
        let address = |unit: &str| {
            Record::Address(crate::record::AddressRecord {
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
                source: crate::record::SourceProvenance::osm(crate::record::OsmObjectType::Node, 1),
            })
        };
        let mut builder = DictionaryBuilder::default();
        for index in 0..MIN_UNIT_WORD_VALUES {
            builder.add(RecordNames::of(&address(&format!("Bay {index}"))), None);
        }
        builder.add(RecordNames::of(&address("Booth 1")), None);
        let dir = std::env::temp_dir().join(format!("og-units-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("pack.ogp");
        let mut pack = ContainerWriter::create(&path).expect("pack");
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
            source: crate::record::SourceProvenance::osm(crate::record::OsmObjectType::Node, 1),
        }));
        assert_eq!(
            names.names,
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
