//! Pack writer and reader.
//!
//! A Pack is one container file (see [`crate::container`]) holding the record
//! store, admin context table, text index, spatial index and the address
//! parser's dictionaries and weights (see [`crate::parse::lattice`]). Builds write a new
//! generation next to the published one and switch the `CURRENT` pointer only
//! after the new Pack passes validation, so running servers never see a
//! partial Pack.
//!
//! ```text
//! <pack dir>/CURRENT                         id of the published generation
//! <pack dir>/generations/<id>/pack.ogp       the Pack
//! <pack dir>/generations/<id>/audit/         build report and rejections
//! ```
//!
//! Sealing writes the parts the writer holds in memory into the Pack file
//! first, then copies in the parts it streamed to scratch one at a time
//! ([`Sealing`]), so a build can checkpoint between parts and go on from
//! there.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::error::{Error, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    container::{self, Container},
    context::{self, TupleWriter},
    extsort::Scratch,
    labels,
    memory::Budget,
    parse::lattice::{
        CountryCode,
        dictionary::{Builder, RecordNames},
    },
    record::{Extent, InterpolationRecord, Layer, Record, RejectedRecord, StreetRecord},
    records, spatial_index,
    text_index::{self, PostcodeAreas},
};

pub use crate::{
    context::{AdminContextTuple, RecordContext},
    records::{ContextRecord, Point, PointPrecision, Source, Summary},
};

pub type RecordId = u64;

mod publication;
pub use publication::{resolve_pack_path, unpublished_generations};

pub const PACK_FILE: &str = "pack.ogp";
pub const AUDIT_DIR: &str = "audit";
pub const REJECTIONS_FILE: &str = "rejections.jsonl";
pub const BUILD_REPORT_FILE: &str = "build-report.json";
pub const PACK_SCHEMA_VERSION: u32 = 10;

const MANIFEST_SECTION: &str = "manifest";
const MANIFEST_VERSION: u32 = 1;
const POSTCODE_ADDRESSES_SECTION: &str = "postcode_addresses";
const POSTCODE_ADDRESSES_VERSION: u32 = 1;
const COUNTRY_CODES_SECTION: &str = "country_codes";
const COUNTRY_CODES_VERSION: u32 = 1;
const SUBDIVISION_CODES_SECTION: &str = "subdivision_codes";
const SUBDIVISION_CODES_VERSION: u32 = 1;
const LANGUAGE_NAMES_SECTION: &str = "language_names";
const LANGUAGE_NAMES_VERSION: u32 = 1;
const EXTENTS_SECTION: &str = "extents";
const EXTENTS_VERSION: u32 = 1;
const TIMEZONES_SECTION: &str = "timezones";
const TIMEZONES_VERSION: u32 = 1;
const SCRATCH_DIR: &str = ".scratch";
const DEFAULT_TEXT_INDEX_MEMORY_BYTES: usize = 1 << 30;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub schema_version: u32,
    pub crate_version: String,
    pub built_at_unix: u64,
    pub record_count: u64,
    pub layer_counts: BTreeMap<String, u64>,
    pub string_count: u64,
    pub context_tuple_count: u64,
    pub text_document_count: u64,
    pub spatial: SpatialManifest,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpatialManifest {
    pub point_count: u64,
    pub line_count: u64,
    pub block_count: u64,
    pub context_point_count: u64,
}

/// How many addresses state a postcode, per country: what one postcode's
/// addresses are a share of, so search weighs agreeing on a postcode by how
/// few records it covers in its country. Summed from the postcode records as
/// they are written, each counting the addresses it was derived from. `None`
/// holds the postcodes no country holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostcodeAddresses(BTreeMap<Option<CountryCode>, u64>);

/// Bytes of one [`PostcodeAddresses`] entry: the country as a `u16` (0 for
/// none) and the count, little-endian.
const POSTCODE_ADDRESSES_ENTRY_BYTES: usize = 10;

impl PostcodeAddresses {
    fn add(&mut self, country: Option<CountryCode>, addresses: u64) {
        *self.0.entry(country).or_default() += addresses;
    }

    /// The addresses of `country` that state a postcode.
    pub fn of(&self, country: Option<CountryCode>) -> u64 {
        self.0.get(&country).copied().unwrap_or_default()
    }

    /// Every country some postcode is in.
    pub fn countries(&self) -> impl Iterator<Item = CountryCode> + '_ {
        self.0.keys().flatten().copied()
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.0.len() * POSTCODE_ADDRESSES_ENTRY_BYTES);
        for (country, addresses) in &self.0 {
            bytes.extend_from_slice(&country.map_or(0, |country| country.to_u16()).to_le_bytes());
            bytes.extend_from_slice(&addresses.to_le_bytes());
        }
        bytes
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if !bytes.len().is_multiple_of(POSTCODE_ADDRESSES_ENTRY_BYTES) {
            return Err(Error::Decode("postcode address counts are truncated"));
        }
        let mut counts = BTreeMap::new();
        for entry in bytes.as_chunks::<POSTCODE_ADDRESSES_ENTRY_BYTES>().0 {
            let (country, addresses) = entry.split_at(2);
            let country = match u16::from_le_bytes([country[0], country[1]]) {
                0 => None,
                code => Some(CountryCode::from_u16(code).ok_or(Error::Decode(
                    "postcode address country is not a country code",
                ))?),
            };
            let addresses = u64::from_le_bytes(addresses.try_into().expect("eight bytes"));
            counts.insert(country, addresses);
        }
        Ok(Self(counts))
    }
}

/// Bytes of one country code entry: the country record's id and its code as a
/// `u16`, little-endian.
const COUNTRY_CODES_ENTRY_BYTES: usize = 10;

fn country_codes_to_bytes(codes: &HashMap<RecordId, CountryCode>) -> Vec<u8> {
    let mut sorted = codes.iter().collect::<Vec<_>>();
    sorted.sort_unstable();
    let mut bytes = Vec::with_capacity(sorted.len() * COUNTRY_CODES_ENTRY_BYTES);
    for (record_id, code) in sorted {
        bytes.extend_from_slice(&record_id.to_le_bytes());
        bytes.extend_from_slice(&code.to_u16().to_le_bytes());
    }
    bytes
}

fn country_codes_from_bytes(bytes: &[u8]) -> Result<HashMap<RecordId, CountryCode>> {
    if !bytes.len().is_multiple_of(COUNTRY_CODES_ENTRY_BYTES) {
        return Err(Error::Decode("country codes are truncated"));
    }
    bytes
        .as_chunks::<COUNTRY_CODES_ENTRY_BYTES>()
        .0
        .iter()
        .map(|entry| {
            let (record_id, code) = entry.split_at(8);
            let record_id = RecordId::from_le_bytes(record_id.try_into().expect("eight bytes"));
            let code = CountryCode::from_u16(u16::from_le_bytes([code[0], code[1]]))
                .ok_or(Error::Decode("country record's code is not a country code"))?;
            Ok((record_id, code))
        })
        .collect()
}

/// Each entry is the area record's id, little-endian, then its code's length
/// in one byte and the code itself: `CA-ON`.
fn subdivision_codes_to_bytes(codes: &HashMap<RecordId, String>) -> Vec<u8> {
    let mut sorted = codes
        .iter()
        .filter(|(_, code)| u8::try_from(code.len()).is_ok())
        .collect::<Vec<_>>();
    sorted.sort_unstable();
    let mut bytes = Vec::new();
    for (record_id, code) in sorted {
        bytes.extend_from_slice(&record_id.to_le_bytes());
        bytes.push(u8::try_from(code.len()).expect("filtered to fit"));
        bytes.extend_from_slice(code.as_bytes());
    }
    bytes
}

fn subdivision_codes_from_bytes(mut bytes: &[u8]) -> Result<HashMap<RecordId, String>> {
    let truncated = || Error::Decode("subdivision codes are truncated");
    let mut codes = HashMap::new();
    while !bytes.is_empty() {
        let (record_id, rest) = bytes.split_at_checked(8).ok_or_else(truncated)?;
        let (&length, rest) = rest.split_first().ok_or_else(truncated)?;
        let (code, rest) = rest
            .split_at_checked(usize::from(length))
            .ok_or_else(truncated)?;
        let record_id = RecordId::from_le_bytes(record_id.try_into().expect("eight bytes"));
        let code = std::str::from_utf8(code)
            .map_err(|_| Error::Decode("a subdivision code is not text"))?;
        codes.insert(record_id, code.to_string());
        bytes = rest;
    }
    Ok(codes)
}

/// The name of an area in each language it has one in, as pairs of a
/// language code as its tag writes it (`de`, `zh-Hant`) and the name, in code
/// order.
pub type LanguageNames = Vec<(String, String)>;

/// Each entry is the record's id, little-endian, then its name count as a
/// little-endian `u16`, then each name as its code's length in one byte, the
/// code, the name's length as a little-endian `u16` and the name.
fn language_names_to_bytes(names: &HashMap<RecordId, LanguageNames>) -> Vec<u8> {
    let fits = |(code, name): &&(String, String)| {
        u8::try_from(code.len()).is_ok() && u16::try_from(name.len()).is_ok()
    };
    let mut sorted = names.iter().collect::<Vec<_>>();
    sorted.sort_unstable_by_key(|(record_id, _)| **record_id);
    let mut bytes = Vec::new();
    for (record_id, names) in sorted {
        let kept = names
            .iter()
            .filter(fits)
            .take(usize::from(u16::MAX))
            .collect::<Vec<_>>();
        if kept.is_empty() {
            continue;
        }
        bytes.extend_from_slice(&record_id.to_le_bytes());
        let count = u16::try_from(kept.len()).expect("taken to fit");
        bytes.extend_from_slice(&count.to_le_bytes());
        for (code, name) in kept {
            bytes.push(u8::try_from(code.len()).expect("filtered to fit"));
            bytes.extend_from_slice(code.as_bytes());
            let length = u16::try_from(name.len()).expect("filtered to fit");
            bytes.extend_from_slice(&length.to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());
        }
    }
    bytes
}

fn language_names_from_bytes(mut bytes: &[u8]) -> Result<HashMap<RecordId, LanguageNames>> {
    let truncated = || Error::Decode("language names are truncated");
    let text = |bytes: &[u8]| {
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|_| Error::Decode("a language name is not text"))
    };
    let mut all = HashMap::new();
    while !bytes.is_empty() {
        let (record_id, rest) = bytes.split_at_checked(8).ok_or_else(truncated)?;
        let (count, mut rest) = rest.split_at_checked(2).ok_or_else(truncated)?;
        let count = u16::from_le_bytes(count.try_into().expect("two bytes"));
        let mut names = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let (&length, after) = rest.split_first().ok_or_else(truncated)?;
            let (code, after) = after
                .split_at_checked(usize::from(length))
                .ok_or_else(truncated)?;
            let (length, after) = after.split_at_checked(2).ok_or_else(truncated)?;
            let length = u16::from_le_bytes(length.try_into().expect("two bytes"));
            let (name, after) = after
                .split_at_checked(usize::from(length))
                .ok_or_else(truncated)?;
            names.push((text(code)?, text(name)?));
            rest = after;
        }
        let record_id = RecordId::from_le_bytes(record_id.try_into().expect("eight bytes"));
        all.insert(record_id, names);
        bytes = rest;
    }
    Ok(all)
}

/// Bytes of one extent entry, which is the area record's id, then its west,
/// south, east and north sides in 1e-7 degrees, all little-endian.
const EXTENT_ENTRY_BYTES: usize = 24;

fn extents_to_bytes(mut extents: Vec<(RecordId, Extent)>) -> Vec<u8> {
    extents.sort_unstable();
    extents.dedup_by_key(|(record_id, _)| *record_id);
    let mut bytes = Vec::with_capacity(extents.len() * EXTENT_ENTRY_BYTES);
    for (record_id, extent) in extents {
        bytes.extend_from_slice(&record_id.to_le_bytes());
        for side in extent {
            bytes.extend_from_slice(&side.to_le_bytes());
        }
    }
    bytes
}

/// The box around each area record, in id order and read where the Pack
/// holds it, as a planet has an area for every town and suburb.
#[derive(Debug, Default)]
struct Extents(Option<container::Bytes>);

impl Extents {
    fn open(bytes: container::Bytes) -> Result<Self> {
        let (entries, rest) = bytes.as_chunks::<EXTENT_ENTRY_BYTES>();
        if !rest.is_empty() {
            return Err(Error::Decode("area extents are truncated"));
        }
        let id = |entry: &[u8; EXTENT_ENTRY_BYTES]| {
            RecordId::from_le_bytes(entry[..8].try_into().expect("eight bytes"))
        };
        if !entries.is_sorted_by(|left, right| id(left) < id(right)) {
            return Err(Error::Decode("area extents are out of order"));
        }
        Ok(Self(Some(bytes)))
    }

    fn get(&self, record_id: RecordId) -> Option<Extent> {
        let entries = self.0.as_ref()?.as_chunks::<EXTENT_ENTRY_BYTES>().0;
        let at = entries
            .binary_search_by_key(&record_id, |entry| {
                RecordId::from_le_bytes(entry[..8].try_into().expect("eight bytes"))
            })
            .ok()?;
        let entry = &entries[at];
        Some(std::array::from_fn(|index| {
            let from = 8 + 4 * index;
            i32::from_le_bytes(entry[from..from + 4].try_into().expect("four bytes"))
        }))
    }
}

/// Bytes of one area's timezone, which is the area record's id, then the
/// zone's index in the names, both little-endian.
const AREA_ZONE_BYTES: usize = 10;

/// The zone names as a little-endian `u16` count, then each name's length
/// in one byte and the name, then an entry per area that lies wholly in one
/// zone, in id order.
fn timezones_to_bytes(names: &[String], mut areas: Vec<(RecordId, u16)>) -> Result<Vec<u8>> {
    let count = u16::try_from(names.len())
        .map_err(|_| Error::Inconsistent("more timezones than the Pack names".into()))?;
    let mut bytes = count.to_le_bytes().to_vec();
    for name in names {
        let length = u8::try_from(name.len()).map_err(|_| {
            Error::Inconsistent(format!("timezone name {name:?} is longer than 255 bytes"))
        })?;
        bytes.push(length);
        bytes.extend_from_slice(name.as_bytes());
    }
    areas.sort_unstable();
    areas.dedup_by_key(|(record_id, _)| *record_id);
    for (record_id, zone) in areas {
        if zone >= count {
            return Err(Error::Inconsistent(format!(
                "area record {record_id} lies in timezone {zone} of {count}"
            )));
        }
        bytes.extend_from_slice(&record_id.to_le_bytes());
        bytes.extend_from_slice(&zone.to_le_bytes());
    }
    Ok(bytes)
}

/// The timezone names of a Pack built with timezone boundaries, and the
/// zone of each area that lies wholly in one, read where the Pack holds it.
#[derive(Debug, Default)]
struct Timezones {
    names: Vec<String>,
    /// The section, and where its area entries start.
    areas: Option<(container::Bytes, usize)>,
}

impl Timezones {
    fn open(bytes: container::Bytes) -> Result<Self> {
        let truncated = || Error::Decode("timezones are truncated");
        let (count, mut rest) = bytes.split_at_checked(2).ok_or_else(truncated)?;
        let count = u16::from_le_bytes([count[0], count[1]]);
        let mut names = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let (&length, after) = rest.split_first().ok_or_else(truncated)?;
            let (name, after) = after
                .split_at_checked(usize::from(length))
                .ok_or_else(truncated)?;
            let name = std::str::from_utf8(name)
                .map_err(|_| Error::Decode("a timezone name is not text"))?;
            names.push(name.to_string());
            rest = after;
        }
        let (entries, cut) = rest.as_chunks::<AREA_ZONE_BYTES>();
        if !cut.is_empty() {
            return Err(truncated());
        }
        if !entries.is_sorted_by(|left, right| area_of(left) < area_of(right)) {
            return Err(Error::Decode("area timezones are out of order"));
        }
        if entries.iter().any(|entry| zone_of(entry) >= count) {
            return Err(Error::Decode(
                "an area lies in a timezone the Pack does not name",
            ));
        }
        let areas_at = bytes.len() - rest.len();
        Ok(Self {
            names,
            areas: Some((bytes, areas_at)),
        })
    }

    /// The zone the area record lies wholly in, by its index in the names.
    fn of_area(&self, record_id: RecordId) -> Option<u16> {
        let (bytes, at) = self.areas.as_ref()?;
        let entries = bytes[*at..].as_chunks::<AREA_ZONE_BYTES>().0;
        let found = entries.binary_search_by_key(&record_id, area_of).ok()?;
        Some(zone_of(&entries[found]))
    }
}

fn area_of(entry: &[u8; AREA_ZONE_BYTES]) -> RecordId {
    RecordId::from_le_bytes(entry[..8].try_into().expect("eight bytes"))
}

fn zone_of(entry: &[u8; AREA_ZONE_BYTES]) -> u16 {
    u16::from_le_bytes([entry[8], entry[9]])
}

#[derive(Debug, Clone)]
pub struct WriterOptions {
    /// Pool every buffer of the build reserves from.
    pub memory: Arc<Budget>,
    /// Memory for Tantivy's indexing buffers, taken from the pool.
    pub text_index_memory_bytes: usize,
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self {
            memory: Budget::unlimited(),
            text_index_memory_bytes: DEFAULT_TEXT_INDEX_MEMORY_BYTES,
        }
    }
}

/// The names of the areas a record lies in that its text document carries:
/// every name of each area but its country and the place area it lies in
/// (an island), which are indexed by their `name` alone. A country or an
/// island has hundreds of names, one per language, and every record in it
/// would carry them all; the parser's dictionaries still hold every one.
fn indexed_area_names<'a>(
    context: &RecordContext,
    names: &'a HashMap<RecordId, Vec<String>>,
) -> impl Iterator<Item = &'a str> {
    let tuple = &context.admin_context;
    let large = [tuple.country_record_id(), tuple.place_record_id()];
    context
        .admin_context
        .parent_record_ids()
        .filter_map(move |id| {
            let names = names.get(&id)?;
            Some(if large.contains(&Some(id)) {
                &names[..names.len().min(1)]
            } else {
                &names[..]
            })
        })
        .flatten()
        .map(String::as_str)
}

/// The timezone names, and the zone of each area record that lies wholly
/// in one, by its index in them.
#[derive(Debug)]
struct AreaZones {
    names: Vec<String>,
    areas: Vec<(RecordId, u16)>,
}

/// Writes records in their final order. Record ids are assigned sequentially.
#[derive(Debug)]
pub struct Writer {
    scratch: Arc<Scratch>,
    records: records::Writer,
    contexts: TupleWriter,
    text: text_index::Writer,
    spatial: spatial_index::Writer,
    layer_counts: BTreeMap<Layer, u64>,
    context_names: Arc<HashMap<RecordId, Vec<String>>>,
    country_codes: Arc<HashMap<RecordId, CountryCode>>,
    subdivision_codes: HashMap<RecordId, String>,
    language_names: HashMap<RecordId, LanguageNames>,
    extents: Vec<(RecordId, Extent)>,
    /// When the build was given timezone boundaries.
    timezones: Option<AreaZones>,
    postcode_areas: Arc<PostcodeAreas>,
    postcode_addresses: PostcodeAddresses,
    dictionaries: Builder,
    /// Last, so it drops after every writer of a file in it (see
    /// [`Generation`]'s drop).
    generation: Generation,
}

/// A finished, unpublished Pack.
#[derive(Debug)]
pub struct SealedPack {
    generation: Generation,
    manifest: Manifest,
    bytes: u64,
    /// Where the sections end, before the table of contents.
    sections: container::State,
}

/// A Pack generation being built, beside the published one. Removed when
/// dropped unless it was published, or its build's scratch holds a
/// checkpoint the build may go on from: no reader can be using it.
#[derive(Debug)]
pub struct Generation {
    destination: PathBuf,
    path: PathBuf,
    keep: bool,
    scratch: Option<Arc<Scratch>>,
}

/// A file in an unpublished generation naming its build's scratch
/// directory, so a later build of the Pack finds a build to go on from.
pub const SCRATCH_POINTER_FILE: &str = "scratch";

impl Generation {
    /// A new, empty generation of the Pack at `destination`.
    pub fn create(destination: impl AsRef<Path>) -> Result<Self> {
        let destination = destination.as_ref().to_path_buf();
        Ok(Self {
            path: publication::create_generation(&destination)?,
            destination,
            keep: false,
            scratch: None,
        })
    }

    /// The unpublished generation at `path`, of the Pack at `destination`,
    /// to go on building.
    pub fn reopen(destination: impl AsRef<Path>, path: impl AsRef<Path>) -> Self {
        Self {
            destination: destination.as_ref().to_path_buf(),
            path: path.as_ref().to_path_buf(),
            keep: false,
            scratch: None,
        }
    }

    /// Create the build's scratch directory: in `scratch_dir` when given,
    /// else inside the generation. Its bytes are capped by `budget`.
    pub fn create_scratch(
        &mut self,
        scratch_dir: Option<&Path>,
        budget: Option<u64>,
    ) -> Result<Arc<Scratch>> {
        let scratch = Scratch::create(self.scratch_path(scratch_dir))?;
        scratch.set_budget(budget);
        self.scratch = Some(Arc::clone(&scratch));
        Ok(scratch)
    }

    /// Where the build's scratch directory goes.
    pub fn scratch_path(&self, scratch_dir: Option<&Path>) -> PathBuf {
        scratch_dir
            .map(|dir| dir.join(self.path.file_name().expect("generation name")))
            .unwrap_or_else(|| self.path.join(SCRATCH_DIR))
    }

    /// Tie the generation to `scratch`: it outlives a failed build while
    /// `scratch` holds a checkpoint.
    pub fn attach(&mut self, scratch: &Arc<Scratch>) {
        self.scratch = Some(Arc::clone(scratch));
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }
}

/// A generation no build keeps is removed as it drops. Windows keeps a
/// directory while a file in it is open, so whatever holds one drops first:
/// the structs holding a generation declare it last.
impl Drop for Generation {
    fn drop(&mut self) {
        let resumable = self
            .scratch
            .as_ref()
            .is_some_and(|scratch| scratch.resumable());
        if !self.keep && !resumable {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Statistics a build reports after sealing the Pack.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildStats {
    pub bytes: u64,
    pub sections: BTreeMap<String, u64>,
    pub text_index_bytes: u64,
}

impl Writer {
    pub fn create(destination: impl AsRef<Path>) -> Result<Self> {
        Self::create_with(destination, &WriterOptions::default())
    }

    pub fn create_with(destination: impl AsRef<Path>, options: &WriterOptions) -> Result<Self> {
        let mut generation = Generation::create(destination)?;
        let scratch = generation.create_scratch(None, None)?;
        Self::create_in(generation, scratch, options)
    }

    /// Write into `generation`, streaming to `scratch`.
    pub fn create_in(
        generation: Generation,
        scratch: Arc<Scratch>,
        options: &WriterOptions,
    ) -> Result<Self> {
        Ok(Self {
            records: records::Writer::create(&scratch, &options.memory)?,
            contexts: TupleWriter::default(),
            text: text_index::Writer::create(
                &scratch,
                &options.memory,
                options.text_index_memory_bytes,
            )?,
            spatial: spatial_index::Writer::new(&scratch)?,
            scratch,
            generation,
            layer_counts: BTreeMap::new(),
            context_names: Arc::default(),
            country_codes: Arc::default(),
            subdivision_codes: HashMap::new(),
            language_names: HashMap::new(),
            extents: Vec::new(),
            timezones: None,
            postcode_areas: Arc::default(),
            postcode_addresses: PostcodeAddresses::default(),
            dictionaries: Builder::default(),
        })
    }

    /// Scratch directory shared with the builder's own sorters.
    pub fn scratch(&self) -> &Arc<Scratch> {
        &self.scratch
    }

    /// Directory of the generation being built, for build outputs that live
    /// beside the Pack file.
    pub fn generation(&self) -> &Path {
        &self.generation.path
    }

    pub fn record_count(&self) -> u64 {
        self.records.record_count()
    }

    /// Names of the records admin contexts point at, each area's `name`
    /// first, so the records written after this can be indexed under the
    /// areas they lie in.
    pub fn set_context_names(&mut self, names: HashMap<RecordId, Vec<String>>) {
        self.context_names = Arc::new(names);
    }

    /// Country codes of the country records admin contexts point at, so the
    /// parser's dictionaries know which country each name is used in.
    pub fn set_country_codes(&mut self, codes: HashMap<RecordId, CountryCode>) {
        // The same map indexes each record's country, so a Pack covering
        // several countries can be searched one at a time. Cloned rather
        // than shared: the dictionary builder consumes its copy, and the map
        // holds one entry per country record, not per record.
        self.country_codes = Arc::new(codes.clone());
        self.dictionaries.set_country_codes(codes);
    }

    /// The ISO 3166-2 code of each area record below a country that has one
    /// (`CA-ON`), by the area's id.
    pub fn set_subdivision_codes(&mut self, codes: HashMap<RecordId, String>) {
        self.subdivision_codes = codes;
    }

    /// The names of each area record in the languages it has one in, by the
    /// area's id.
    pub fn set_language_names(&mut self, names: HashMap<RecordId, LanguageNames>) {
        self.language_names = names;
    }

    /// The box around each area record, by the area's id.
    pub fn set_extents(&mut self, extents: Vec<(RecordId, Extent)>) {
        self.extents = extents;
    }

    /// The timezone names a record's context may point at by index (see
    /// [`RecordContext::zone`]), and the zone of each area record that lies
    /// wholly in one, by the area's id.
    pub fn set_timezones(&mut self, names: Vec<String>, areas: Vec<(RecordId, u16)>) {
        self.timezones = Some(AreaZones { names, areas });
    }

    /// The groups of regions each region lies in, by the region's id.
    pub fn set_region_groups(&mut self, groups: HashMap<RecordId, Vec<RecordId>>) {
        self.dictionaries.set_region_groups(groups);
    }

    /// Postcode centroids, so the records written after this that have no
    /// postcode of their own are ranked by the nearest one.
    pub fn set_postcode_areas(&mut self, areas: PostcodeAreas) {
        self.postcode_areas = Arc::new(areas);
    }

    pub fn write(&mut self, record: &Record, context: Option<RecordContext>) -> Result<RecordId> {
        let first = self.records.record_count();
        self.write_batch(std::slice::from_ref(&(record.clone(), context)))?;
        Ok(first)
    }

    /// Write records in order: [`BatchPreparer::prepare`], then [`Self::commit`].
    pub fn write_batch(&mut self, batch: &[(Record, Option<RecordContext>)]) -> Result<()> {
        let prepared = self.preparer().prepare(batch.to_vec())?;
        self.commit(prepared)
    }

    /// What computes the text documents, spatial cells and dictionary names
    /// of the records written next, apart from the writer, so a builder can
    /// prepare one batch while the writer appends the one before.
    pub fn preparer(&self) -> BatchPreparer {
        BatchPreparer {
            next: self.records.record_count(),
            fields: self.text.fields(),
            context_names: Arc::clone(&self.context_names),
            country_codes: Arc::clone(&self.country_codes),
            postcode_areas: Arc::clone(&self.postcode_areas),
        }
    }

    /// Append a prepared batch. The dictionaries, the text index and the
    /// store with its spatial pairs each take every record in order.
    pub fn commit(&mut self, batch: PreparedBatch) -> Result<()> {
        let first = self.records.record_count();
        if batch.first != first {
            return Err(Error::Inconsistent(format!(
                "batch prepared for record {} was committed at record {first}",
                batch.first
            )));
        }
        let mut placed = Vec::with_capacity(batch.records.len());
        let mut names = Vec::with_capacity(batch.records.len());
        let mut documents = Vec::with_capacity(batch.records.len());
        for prepared in batch.records {
            placed.push((prepared.record, prepared.context));
            names.push(prepared.names);
            documents.push(prepared.document);
        }
        let Self {
            records,
            contexts,
            text,
            spatial,
            layer_counts,
            postcode_addresses,
            dictionaries,
            ..
        } = self;
        let placed = &placed;
        // None of the three reads another's state, and one thread feeding
        // them in turn bound the whole step. Each runs on its own, joined
        // per batch, so each still takes records in order.
        std::thread::scope(|scope| {
            let named = scope.spawn(move || {
                for (offset, (names, (_, context))) in names.into_iter().zip(placed).enumerate() {
                    if let Some(context) = context
                        && names.is_place()
                    {
                        dictionaries.add_place(first + offset as u64, context, &names);
                    }
                    dictionaries.add(names, context.as_ref());
                }
            });
            let indexed = scope.spawn(move || {
                documents
                    .into_iter()
                    .try_for_each(|document| match document {
                        text_index::Prepared::Document(document) => text.add(document),
                        text_index::Prepared::Address(document, numbers) => {
                            text.add_address(document, numbers)
                        }
                    })
            });
            let stored = placed
                .iter()
                .enumerate()
                .try_for_each(|(offset, (record, context))| {
                    let context = match context {
                        Some(context) => contexts.intern(context)?,
                        None => None,
                    };
                    records.write(record, context)?;
                    spatial.add(first + offset as u64, record)?;
                    if let Record::Postcode(postcode) = record {
                        postcode_addresses.add(postcode.country, postcode.source.record_count);
                    }
                    *layer_counts.entry(record.layer()).or_default() += 1;
                    Ok(())
                });
            named
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            indexed
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))?;
            stored
        })
    }

    /// Write the Pack file without publishing it.
    pub fn seal(self) -> Result<(SealedPack, BuildStats)> {
        let mut sealing = self.into_sealing()?;
        while sealing.seal_part()?.is_some() {}
        sealing.finish()
    }

    /// Start the Pack file with the parts held in memory, the context
    /// table, the parser's dictionaries and the postcode counts, once every
    /// record is written. The parts streamed to scratch follow, one
    /// [`Sealing::seal_part`] at a time.
    pub fn into_sealing(self) -> Result<Sealing> {
        let Self {
            generation,
            scratch,
            records,
            contexts,
            text,
            spatial,
            layer_counts,
            context_names,
            country_codes,
            subdivision_codes,
            language_names,
            extents,
            timezones,
            postcode_addresses,
            dictionaries,
            ..
        } = self;
        // Merging the text index's segments is most of the sealing, and on
        // one thread: it starts now, beside the parts sealed before it.
        let text = Merging::start(text);
        let mut pack = container::Writer::create(generation.path.join(PACK_FILE))?;
        let counts = SealCounts {
            record_count: records.record_count(),
            string_count: records.string_count(),
            context_tuple_count: contexts.tuple_count(),
            layer_counts: layer_counts
                .iter()
                .map(|(layer, count)| (layer.as_str().to_string(), *count))
                .collect(),
            text: None,
            spatial: None,
        };
        let tuples = contexts.finish(&mut pack)?;
        dictionaries.finish(&mut pack, &context_names, &tuples)?;
        drop(tuples);
        drop(context_names);
        pack.add(
            POSTCODE_ADDRESSES_SECTION,
            POSTCODE_ADDRESSES_VERSION,
            &postcode_addresses.to_bytes(),
        )?;
        pack.add(
            COUNTRY_CODES_SECTION,
            COUNTRY_CODES_VERSION,
            &country_codes_to_bytes(&country_codes),
        )?;
        drop(country_codes);
        pack.add(
            SUBDIVISION_CODES_SECTION,
            SUBDIVISION_CODES_VERSION,
            &subdivision_codes_to_bytes(&subdivision_codes),
        )?;
        drop(subdivision_codes);
        pack.add(
            LANGUAGE_NAMES_SECTION,
            LANGUAGE_NAMES_VERSION,
            &language_names_to_bytes(&language_names),
        )?;
        drop(language_names);
        pack.add(EXTENTS_SECTION, EXTENTS_VERSION, &extents_to_bytes(extents))?;
        if let Some(AreaZones { names, areas }) = timezones {
            pack.add(
                TIMEZONES_SECTION,
                TIMEZONES_VERSION,
                &timezones_to_bytes(&names, areas)?,
            )?;
        }
        Ok(Sealing {
            generation,
            scratch,
            pack,
            records: Some(records),
            text: Some(text),
            spatial: Some(spatial),
            counts,
        })
    }

    /// Seal, add the structured index, and publish.
    pub fn finish(self) -> Result<Manifest> {
        let (mut sealed, _) = self.seal()?;
        crate::structured::add_to(&mut sealed)?;
        crate::ids::add_to(&mut sealed)?;
        sealed.publish()
    }
}

/// A Pack file being sealed: the parts held in memory are in it, and the
/// parts streamed to scratch are copied in one at a time.
#[derive(Debug)]
pub struct Sealing {
    scratch: Arc<Scratch>,
    pack: container::Writer,
    records: Option<records::Writer>,
    text: Option<Merging>,
    spatial: Option<spatial_index::Writer>,
    counts: SealCounts,
    /// Last, so it drops after every writer of a file in it (see
    /// [`Generation`]'s drop).
    generation: Generation,
}

/// The text index, its segments merging on a thread of their own until the
/// index is asked for.
#[derive(Debug)]
struct Merging {
    path: PathBuf,
    state: MergeState,
}

#[derive(Debug)]
enum MergeState {
    Running(std::thread::JoinHandle<(text_index::Writer, Result<()>)>),
    Done(Box<text_index::Writer>),
    /// The merge failed or panicked, and its error was handed on.
    Failed,
}

impl Merging {
    fn start(mut text: text_index::Writer) -> Self {
        Self {
            path: text.path().to_path_buf(),
            state: MergeState::Running(std::thread::spawn(move || {
                let merged = text.merge();
                (text, merged)
            })),
        }
    }

    /// The index once its merge is over.
    fn writer(&mut self) -> Result<&mut text_index::Writer> {
        if matches!(self.state, MergeState::Running(_))
            && let MergeState::Running(merging) =
                std::mem::replace(&mut self.state, MergeState::Failed)
        {
            let (text, merged) = merging
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            merged?;
            self.state = MergeState::Done(Box::new(text));
        }
        match &mut self.state {
            MergeState::Done(text) => Ok(text),
            _ => Err(Error::Inconsistent(
                "the text index was asked for after its merge failed".into(),
            )),
        }
    }

    fn into_writer(mut self) -> Result<text_index::Writer> {
        self.writer()?;
        match std::mem::replace(&mut self.state, MergeState::Failed) {
            MergeState::Done(text) => Ok(*text),
            _ => unreachable!("the merge is over"),
        }
    }
}

impl Drop for Merging {
    /// A sealing dropped on an error waits for the merge: the scratch it
    /// writes to goes only after it.
    fn drop(&mut self) {
        if let MergeState::Running(merging) = std::mem::replace(&mut self.state, MergeState::Failed)
        {
            let _ = merging.join();
        }
    }
}

/// What the Pack manifest counts, gathered as the parts are sealed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SealCounts {
    record_count: u64,
    string_count: u64,
    context_tuple_count: u64,
    layer_counts: BTreeMap<String, u64>,
    text: Option<text_index::Commit>,
    spatial: Option<spatial_index::Commit>,
}

/// A Pack file being sealed, as a checkpoint names it: how far the file
/// was written, and the parts left to copy in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealingState {
    pack: container::State,
    records: Option<records::WriterState>,
    text: Option<text_index::WriterState>,
    spatial: Option<spatial_index::WriterState>,
    counts: SealCounts,
}

impl Sealing {
    /// Copy the next part streamed to scratch into the Pack file, removing
    /// its scratch: its name, or `None` once every part is in.
    pub fn seal_part(&mut self) -> Result<Option<&'static str>> {
        self.scratch.check_cancelled()?;
        if let Some(records) = self.records.take() {
            records.finish(&mut self.pack)?;
            return Ok(Some("records"));
        }
        if let Some(text) = self.text.take() {
            self.counts.text = Some(text.into_writer()?.finish(&mut self.pack)?);
            return Ok(Some("text index"));
        }
        if let Some(spatial) = self.spatial.take() {
            self.counts.spatial = Some(spatial.finish(&mut self.pack)?);
            return Ok(Some("spatial index"));
        }
        Ok(None)
    }

    /// Where the sealing stands, for a checkpoint: the Pack file is synced.
    pub fn state(&mut self) -> Result<SealingState> {
        Ok(SealingState {
            pack: self.pack.state()?,
            records: self
                .records
                .as_mut()
                .map(records::Writer::state)
                .transpose()?,
            text: match &mut self.text {
                Some(text) => Some(text.writer()?.state()?),
                None => None,
            },
            spatial: match &mut self.spatial {
                Some(spatial) => Some(spatial.state(&self.scratch)?),
                None => None,
            },
            counts: self.counts.clone(),
        })
    }

    /// The scratch files and directories the parts left to copy are in.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        if let Some(records) = &self.records {
            files.extend(records.files());
        }
        if let Some(text) = &self.text {
            files.push(text.path.clone());
        }
        if let Some(spatial) = &self.spatial {
            files.extend(spatial.files());
        }
        files
    }

    /// The sealing a checkpoint left, in `generation`, its parts in
    /// `scratch`.
    pub fn restore(
        generation: Generation,
        scratch: Arc<Scratch>,
        options: &WriterOptions,
        state: &SealingState,
    ) -> Result<Self> {
        let pack = container::Writer::resume(generation.path.join(PACK_FILE), &state.pack)?;
        Ok(Self {
            records: state
                .records
                .as_ref()
                .map(|records| records::Writer::restore(&scratch, &options.memory, records))
                .transpose()?,
            text: state
                .text
                .as_ref()
                .map(|text| {
                    text_index::Writer::restore(
                        &scratch,
                        &options.memory,
                        options.text_index_memory_bytes,
                        text,
                    )
                    .map(Merging::start)
                })
                .transpose()?,
            spatial: state
                .spatial
                .as_ref()
                .map(|spatial| spatial_index::Writer::restore(&scratch, spatial))
                .transpose()?,
            generation,
            scratch,
            pack,
            counts: state.counts.clone(),
        })
    }

    /// Write the manifest and the table of contents, once every part is in.
    pub fn finish(mut self) -> Result<(SealedPack, BuildStats)> {
        if self.records.is_some() || self.text.is_some() || self.spatial.is_some() {
            return Err(Error::Inconsistent(
                "a Pack was finished before every part was sealed".into(),
            ));
        }
        let counts = &self.counts;
        let text = counts.text.unwrap_or(text_index::Commit {
            document_count: 0,
            bytes: 0,
        });
        let spatial = counts.spatial.clone().unwrap_or_default();
        let manifest = Manifest {
            schema_version: PACK_SCHEMA_VERSION,
            crate_version: env!("CARGO_PKG_VERSION").to_string(),
            built_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or_default(),
            record_count: counts.record_count,
            layer_counts: counts.layer_counts.clone(),
            string_count: counts.string_count,
            context_tuple_count: counts.context_tuple_count,
            text_document_count: text.document_count,
            spatial: SpatialManifest {
                point_count: spatial.point_count,
                line_count: spatial.line_count,
                block_count: spatial.block_count,
                context_point_count: spatial.context_point_count,
            },
        };
        self.pack.add(
            MANIFEST_SECTION,
            MANIFEST_VERSION,
            &serde_json::to_vec_pretty(&manifest)?,
        )?;
        let sections = self.pack.state()?;
        let bytes = self.pack.finish()?;
        let stats = BuildStats {
            bytes,
            sections: sections
                .sections
                .iter()
                .map(|(name, info)| (name.clone(), info.len))
                .collect(),
            text_index_bytes: text.bytes,
        };
        Ok((
            SealedPack {
                generation: self.generation,
                manifest,
                bytes,
                sections,
            },
            stats,
        ))
    }
}

/// A sealed Pack as a checkpoint names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedState {
    manifest: Manifest,
    sections: container::State,
    stats: BuildStats,
}

impl SealedPack {
    /// The sealed Pack and what sealing it reported, for a checkpoint.
    pub fn state(&self, stats: &BuildStats) -> SealedState {
        SealedState {
            manifest: self.manifest.clone(),
            sections: self.sections.clone(),
            stats: stats.clone(),
        }
    }

    /// The Pack a checkpoint sealed, in `generation`. Sections a failed
    /// build added after the checkpoint are cut off again.
    pub fn restore(generation: Generation, state: &SealedState) -> Result<(Self, BuildStats)> {
        let pack = container::Writer::resume(generation.path.join(PACK_FILE), &state.sections)?;
        let bytes = pack.finish()?;
        Ok((
            Self {
                generation,
                manifest: state.manifest.clone(),
                bytes,
                sections: state.sections.clone(),
            },
            state.stats.clone(),
        ))
    }

    pub fn generation(&self) -> &Path {
        &self.generation.path
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The Pack file, to read before it is published.
    pub fn path(&self) -> PathBuf {
        self.generation.path.join(PACK_FILE)
    }

    /// Add a section made from the sealed Pack itself, such as the parser
    /// weights trained on its records. Nothing reads the Pack file yet.
    pub fn add_section(&mut self, name: &str, version: u32, bytes: &[u8]) -> Result<()> {
        let mut pack = container::Writer::append(self.path())?;
        pack.add(name, version, bytes)?;
        self.bytes = pack.finish()?;
        Ok(())
    }

    /// Add sections made from the sealed Pack, each copied from a file, such
    /// as its structured index (see [`crate::structured::add_to`]).
    pub fn add_section_files(&mut self, sections: &[(&str, u32, PathBuf)]) -> Result<()> {
        let mut pack = container::Writer::append(self.path())?;
        for (name, version, path) in sections {
            pack.add_file(name, *version, path)?;
        }
        self.bytes = pack.finish()?;
        Ok(())
    }

    /// Validate the Pack with the serving readers, then point `CURRENT` at it.
    pub fn publish(mut self) -> Result<Manifest> {
        publication::publish(
            &self.generation.destination,
            &self.generation.path,
            &self.manifest,
            &mut self.generation.keep,
        )?;
        Ok(self.manifest)
    }
}

pub struct Reader {
    path: PathBuf,
    container: Container,
    manifest: Manifest,
    records: records::Reader,
    context: context::Reader,
    postcode_addresses: PostcodeAddresses,
    /// The code of each country record, by its id.
    country_codes: HashMap<RecordId, CountryCode>,
    /// The ISO 3166-2 code of each area record that has one, by its id.
    subdivision_codes: HashMap<RecordId, String>,
    /// The names of each area record in other languages, by its id.
    language_names: HashMap<RecordId, LanguageNames>,
    extents: Extents,
    timezones: Timezones,
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Reader")
            .field("path", &self.path)
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

impl Reader {
    /// Open a Pack directory (its published generation) or a Pack file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = resolve_pack_path(path)?;
        let container = Container::open(&path)?;
        let manifest: Manifest =
            serde_json::from_slice(&container.section(MANIFEST_SECTION, MANIFEST_VERSION)?)?;
        if manifest.schema_version != PACK_SCHEMA_VERSION {
            return Err(Error::UnsupportedVersion {
                found: manifest.schema_version,
            });
        }
        let records = records::Reader::open(&container)?;
        if records.record_count() != manifest.record_count {
            return Err(Error::ManifestMismatch);
        }
        let context = context::Reader::open(&container)?;
        let postcode_addresses = PostcodeAddresses::from_bytes(
            &container.section(POSTCODE_ADDRESSES_SECTION, POSTCODE_ADDRESSES_VERSION)?,
        )?;
        // Optional: a Pack sealed before the section has none, and answers
        // without country codes rather than failing to open.
        let country_codes = if container.sections().contains_key(COUNTRY_CODES_SECTION) {
            country_codes_from_bytes(
                &container.section(COUNTRY_CODES_SECTION, COUNTRY_CODES_VERSION)?,
            )?
        } else {
            HashMap::new()
        };
        let subdivision_codes = if container.sections().contains_key(SUBDIVISION_CODES_SECTION) {
            subdivision_codes_from_bytes(
                &container.section(SUBDIVISION_CODES_SECTION, SUBDIVISION_CODES_VERSION)?,
            )?
        } else {
            HashMap::new()
        };
        let language_names = if container.sections().contains_key(LANGUAGE_NAMES_SECTION) {
            language_names_from_bytes(
                &container.section(LANGUAGE_NAMES_SECTION, LANGUAGE_NAMES_VERSION)?,
            )?
        } else {
            HashMap::new()
        };
        let extents = if container.sections().contains_key(EXTENTS_SECTION) {
            Extents::open(container.section(EXTENTS_SECTION, EXTENTS_VERSION)?)?
        } else {
            Extents::default()
        };
        let timezones = if container.sections().contains_key(TIMEZONES_SECTION) {
            Timezones::open(container.section(TIMEZONES_SECTION, TIMEZONES_VERSION)?)?
        } else {
            Timezones::default()
        };
        Ok(Self {
            path,
            container,
            manifest,
            records,
            context,
            postcode_addresses,
            country_codes,
            subdivision_codes,
            language_names,
            extents,
            timezones,
        })
    }

    pub const fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The Pack file opened by this reader.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn container(&self) -> &Container {
        &self.container
    }

    /// Check every section of the Pack file against its checksum.
    pub fn verify(&self) -> Result<()> {
        self.container.verify()
    }

    /// The ISO 3166-1 code of a country record. `None` for any other record,
    /// and for every record of a Pack sealed without country codes.
    pub fn country_code(&self, record_id: RecordId) -> Option<CountryCode> {
        self.country_codes.get(&record_id).copied()
    }

    /// The ISO 3166-2 code of an area record below a country (`CA-ON`).
    /// `None` for an area OpenStreetMap gives no code, for any other record,
    /// and for every record of a Pack sealed without subdivision codes.
    pub fn subdivision_code(&self, record_id: RecordId) -> Option<&str> {
        self.subdivision_codes.get(&record_id).map(String::as_str)
    }

    /// An area record's name in `language` (`de`, `zh-Hant`), matched
    /// without regard to case. A language with a region or script that has
    /// no name of its own takes the name of its base language, so `de-CH`
    /// reads `de`. `None` when the record has no name in it, and for every
    /// record of a Pack sealed without language names.
    pub fn language_name(&self, record_id: RecordId, language: &str) -> Option<&str> {
        let names = self.language_names.get(&record_id)?;
        let find = |wanted: &str| {
            names
                .iter()
                .find(|(code, _)| code.eq_ignore_ascii_case(wanted))
                .map(|(_, name)| name.as_str())
        };
        find(language).or_else(|| {
            let base = language.split(['-', '_']).next()?;
            (base.len() < language.len()).then(|| find(base)).flatten()
        })
    }

    /// The IANA name of the timezone a record lies in, such as
    /// `Europe/Monaco`. An area's is the zone it lies wholly in. Any other
    /// record's is that of the most specific area of its context that has
    /// one, else the zone the build found at its point. `None` where the
    /// timezone boundaries name no zone or two, for an area that reaches
    /// into two, and for every record of a Pack built without them.
    pub fn timezone(&self, record_id: RecordId) -> Result<Option<&str>> {
        if self.timezones.names.is_empty() {
            return Ok(None);
        }
        let zone = match self.timezones.of_area(record_id) {
            Some(zone) => Some(zone),
            None => self.boundary_context(record_id)?.and_then(|context| {
                context
                    .admin_context
                    .parent_record_ids()
                    .find_map(|area| self.timezones.of_area(area))
                    .or(context.zone)
            }),
        };
        zone.map(|zone| {
            self.timezones
                .names
                .get(usize::from(zone))
                .map(String::as_str)
                .ok_or(Error::Decode(
                    "a record lies in a timezone the Pack does not name",
                ))
        })
        .transpose()
    }

    /// See [`PostcodeAddresses`].
    pub fn postcode_addresses(&self) -> &PostcodeAddresses {
        &self.postcode_addresses
    }

    pub fn records(&self) -> &records::Reader {
        &self.records
    }

    /// Section sizes in bytes, by section name.
    pub fn section_sizes(&self) -> BTreeMap<String, u64> {
        self.container
            .sections()
            .iter()
            .map(|(name, info)| (name.clone(), info.len))
            .collect()
    }

    /// The summary search and autocomplete return. A POI whose address states
    /// no locality is labelled with the locality it lies in, and an area
    /// carries the box around it.
    pub fn record_summary(&self, record_id: RecordId) -> Result<Summary> {
        let mut summary = self.records.summary(record_id)?;
        if summary.bbox.is_none() {
            summary.bbox = self.extents.get(record_id).map(records::bbox);
        }
        summary.timezone = self.timezone(record_id)?.map(str::to_string);
        if summary.layer == Layer::Poi.as_str()
            && let Record::Poi(poi) = self.records.record(record_id)?
            && poi
                .address
                .as_ref()
                .is_none_or(|address| address.locality.is_none())
            && let Some(locality) = self.locality(record_id)?
        {
            summary.label = labels::poi_label(&poi.name, poi.address.as_ref(), Some(&locality));
        }
        Ok(summary)
    }

    /// Name of the locality a record lies in, from its boundary context, or
    /// of its district where no locality covers it: a single-tier city such as
    /// Toronto is a district with no locality over its core.
    pub fn locality(&self, record_id: RecordId) -> Result<Option<String>> {
        let Some(area_id) = self.boundary_context(record_id)?.and_then(|context| {
            let tuple = &context.admin_context;
            tuple
                .locality_record_id()
                .or_else(|| tuple.district_record_id())
        }) else {
            return Ok(None);
        };
        Ok(self.context_record(area_id)?.map(|record| record.name))
    }

    pub fn record_json(&self, record_id: RecordId) -> Result<Value> {
        self.records.record_json(record_id)
    }

    pub fn records_json_by_layer(&self, layer: &str, limit: usize) -> Result<Vec<Value>> {
        let layer = Layer::parse(layer).ok_or(Error::Decode("unknown record layer"))?;
        let mut records = Vec::new();
        for record_id in 0..self.records.record_count() {
            if self.records.header(record_id)?.layer != layer {
                continue;
            }
            records.push(self.record_json(record_id)?);
            if limit > 0 && records.len() >= limit {
                break;
            }
        }
        Ok(records)
    }

    /// Full scan: a test oracle for the id catalogue, too slow to serve.
    #[cfg(test)]
    pub(crate) fn find_by_source_id(&self, source_id: &str) -> Result<Option<RecordId>> {
        for record_id in 0..self.records.record_count() {
            if self.record_summary(record_id)?.id == source_id {
                return Ok(Some(record_id));
            }
        }
        Ok(None)
    }

    pub fn interpolation(&self, record_id: RecordId) -> Result<Option<InterpolationRecord>> {
        if self.records.header(record_id)?.layer != Layer::Interpolation {
            return Ok(None);
        }
        Ok(match self.records.record(record_id)? {
            Record::Interpolation(record) => Some(record),
            _ => None,
        })
    }

    pub fn street(&self, record_id: RecordId) -> Result<Option<StreetRecord>> {
        if self.records.header(record_id)?.layer != Layer::Street {
            return Ok(None);
        }
        Ok(match self.records.record(record_id)? {
            Record::Street(record) => Some(record),
            _ => None,
        })
    }

    pub fn context_record(&self, record_id: RecordId) -> Result<Option<ContextRecord>> {
        self.records.context_record(record_id)
    }

    pub fn boundary_context(&self, record_id: RecordId) -> Result<Option<RecordContext>> {
        self.records
            .header(record_id)?
            .context
            .map(|reference| self.context.resolve(reference))
            .transpose()
    }

    /// Rejected source objects recorded by the build that produced this Pack.
    pub fn rejections(&self, limit: usize) -> Result<Vec<RejectedRecord>> {
        let path = self
            .path
            .parent()
            .map(|dir| dir.join(AUDIT_DIR).join(REJECTIONS_FILE))
            .ok_or_else(|| Error::Inconsistent("Pack file has no parent directory".into()))?;
        let file = fs::File::open(&path).map_err(|source| Error::IoPath {
            action: "failed to open build audit",
            path: path.clone(),
            source,
        })?;
        let mut rejections = Vec::new();
        for line in BufReader::new(file).lines() {
            if limit > 0 && rejections.len() >= limit {
                break;
            }
            rejections.push(serde_json::from_str(&line?)?);
        }
        Ok(rejections)
    }
}

/// Computes what a batch of records adds to the Pack's indexes, in parallel,
/// for the record ids they will be written at. See [`Writer::preparer`].
#[derive(Debug)]
pub struct BatchPreparer {
    next: RecordId,
    fields: text_index::Fields,
    context_names: Arc<HashMap<RecordId, Vec<String>>>,
    country_codes: Arc<HashMap<RecordId, CountryCode>>,
    postcode_areas: Arc<PostcodeAreas>,
}

impl BatchPreparer {
    /// Prepare the records that follow the last batch this prepared.
    pub fn prepare(
        &mut self,
        batch: Vec<(Record, Option<RecordContext>)>,
    ) -> Result<PreparedBatch> {
        let first = self.next;
        let records = batch
            .into_par_iter()
            .enumerate()
            .map(|(offset, (record, context))| {
                let record_id = first + offset as u64;
                let names = context
                    .iter()
                    .flat_map(|context| indexed_area_names(context, &self.context_names))
                    .collect::<Vec<_>>();
                // The country the record lies in, from the country record its
                // admin context points at. A record outside every country
                // boundary has none and no country filter finds it.
                let country = context
                    .as_ref()
                    .and_then(|context| context.admin_context.country_record_id())
                    .and_then(|id| self.country_codes.get(&id))
                    .map(CountryCode::as_str);
                Ok(PreparedRecord {
                    document: self.fields.prepare(
                        record_id,
                        &record,
                        &names,
                        &self.postcode_areas,
                        country,
                    ),
                    names: RecordNames::of(&record),
                    record,
                    context,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.next += records.len() as u64;
        Ok(PreparedBatch { first, records })
    }
}

/// Records with their text documents, spatial cells and dictionary names,
/// ready for [`Writer::commit`].
#[derive(Debug)]
pub struct PreparedBatch {
    first: RecordId,
    records: Vec<PreparedRecord>,
}

impl PreparedBatch {
    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The records with the ids they will be written at.
    pub fn records(&self) -> impl Iterator<Item = (RecordId, &Record)> {
        self.records
            .iter()
            .enumerate()
            .map(|(offset, prepared)| (self.first + offset as u64, &prepared.record))
    }
}

#[derive(Debug)]
struct PreparedRecord {
    record: Record,
    context: Option<RecordContext>,
    document: text_index::Prepared,
    names: RecordNames,
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use crate::{
        record::{
            AddressComponents, AddressRecord, LocationPrecision, ObjectType, SourceProvenance,
            point_geometry,
        },
        search::{TextSearchOptions, TextSearcher},
    };

    use super::*;

    #[test]
    fn language_names_survive_the_section_and_a_cut_one_fails() {
        let names = HashMap::from([
            (
                3,
                vec![
                    ("de".to_string(), "Genf".to_string()),
                    ("it".to_string(), "Ginevra".to_string()),
                ],
            ),
            (9, vec![("zh-Hant".to_string(), "日內瓦".to_string())]),
        ]);
        let bytes = language_names_to_bytes(&names);
        assert_eq!(language_names_from_bytes(&bytes).expect("decodes"), names);
        assert!(language_names_from_bytes(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn extents_are_found_by_record_id_and_a_bad_section_fails() {
        let crossing = [1_770_000_000, -190_000_000, -1_785_000_000, -155_000_000];
        let monaco = [74_090_279, 437_247_599, 74_398_704, 437_519_311];
        let bytes = extents_to_bytes(vec![(9, crossing), (3, monaco), (3, crossing)]);
        assert_eq!(bytes.len(), 2 * EXTENT_ENTRY_BYTES);
        let extents = Extents::open(container::Bytes::new(bytes.clone())).expect("opens");
        assert_eq!(extents.get(3), Some(monaco));
        assert_eq!(extents.get(9), Some(crossing));
        assert_eq!(extents.get(4), None);
        assert_eq!(Extents::default().get(3), None);
        assert!(matches!(
            Extents::open(container::Bytes::new(bytes[..bytes.len() - 1].to_vec())),
            Err(Error::Decode("area extents are truncated"))
        ));
        let mut swapped = bytes[EXTENT_ENTRY_BYTES..].to_vec();
        swapped.extend_from_slice(&bytes[..EXTENT_ENTRY_BYTES]);
        assert!(matches!(
            Extents::open(container::Bytes::new(swapped)),
            Err(Error::Decode("area extents are out of order"))
        ));
        let mut twice = bytes[..EXTENT_ENTRY_BYTES].to_vec();
        twice.extend_from_slice(&bytes[..EXTENT_ENTRY_BYTES]);
        assert!(matches!(
            Extents::open(container::Bytes::new(twice)),
            Err(Error::Decode("area extents are out of order"))
        ));
    }

    #[test]
    fn timezones_are_found_by_area_and_a_bad_section_fails() {
        let names = vec!["Europe/Monaco".to_string(), "Europe/Paris".to_string()];
        let bytes = timezones_to_bytes(&names, vec![(9, 1), (3, 0), (3, 1)]).expect("the section");
        let zones = Timezones::open(container::Bytes::new(bytes.clone())).expect("opens");
        assert_eq!(zones.names, names);
        assert_eq!(zones.of_area(3), Some(0));
        assert_eq!(zones.of_area(9), Some(1));
        assert_eq!(zones.of_area(4), None);
        assert_eq!(Timezones::default().of_area(3), None);
        let none = Timezones::open(container::Bytes::new(
            timezones_to_bytes(&names, Vec::new()).expect("the section"),
        ))
        .expect("opens");
        assert_eq!((none.of_area(3), none.names), (None, names.clone()));

        let refused = |bytes: Vec<u8>| {
            Timezones::open(container::Bytes::new(bytes))
                .expect_err("refused")
                .to_string()
        };
        let decode = |message: &'static str| Error::Decode(message).to_string();
        for cut in [0, 1, 5, bytes.len() - 1] {
            assert_eq!(
                refused(bytes[..cut].to_vec()),
                decode("timezones are truncated"),
                "{cut}"
            );
        }
        let areas_at = bytes.len() - 2 * AREA_ZONE_BYTES;
        let mut swapped = bytes[..areas_at].to_vec();
        swapped.extend_from_slice(&bytes[areas_at + AREA_ZONE_BYTES..]);
        swapped.extend_from_slice(&bytes[areas_at..areas_at + AREA_ZONE_BYTES]);
        assert_eq!(refused(swapped), decode("area timezones are out of order"));
        let mut unnamed = bytes.clone();
        *unnamed.last_mut().expect("a zone") = 2;
        unnamed[bytes.len() - 2] = 2;
        assert_eq!(
            refused(unnamed),
            decode("an area lies in a timezone the Pack does not name")
        );
        let mut garbled = bytes.clone();
        garbled[3] = 0xff;
        assert_eq!(refused(garbled), decode("a timezone name is not text"));

        assert_eq!(
            timezones_to_bytes(&names, vec![(3, 2)])
                .expect_err("no such zone")
                .to_string(),
            Error::Inconsistent("area record 3 lies in timezone 2 of 2".into()).to_string()
        );
        assert_eq!(
            timezones_to_bytes(&["x".repeat(256)], Vec::new())
                .expect_err("too long")
                .to_string(),
            Error::Inconsistent(format!(
                "timezone name {:?} is longer than 255 bytes",
                "x".repeat(256)
            ))
            .to_string()
        );
    }

    #[test]
    fn subdivision_codes_survive_the_section_and_a_cut_one_fails() {
        let codes = HashMap::from([(3, "CA-ON".to_string()), (9, "NZ-AUK".to_string())]);
        let bytes = subdivision_codes_to_bytes(&codes);
        assert_eq!(
            subdivision_codes_from_bytes(&bytes).expect("decodes"),
            codes
        );
        assert!(subdivision_codes_from_bytes(&bytes[..bytes.len() - 1]).is_err());
    }

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("open-geocode-{name}-{}", uuid::Uuid::new_v4()))
    }

    fn address_record(object_id: i64, number: &str, street: &str) -> Record {
        Record::Address(AddressRecord {
            access: Vec::new(),
            disputed: false,
            street_postcodes: Vec::new(),
            address: AddressComponents {
                areas: Vec::new(),
                number: number.to_string(),
                street: Some(street.to_string()),
                place: None,
                unit: None,
                locality: None,
                region: None,
                postcode: None,
                country: None,
            },
            geometry: point_geometry(-79.0, 43.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, object_id),
        })
    }

    fn write_pack(root: &Path, records: &[Record]) -> Manifest {
        let mut writer = Writer::create(root).expect("writer");
        for record in records {
            writer.write(record, None).expect("write");
        }
        writer.finish().expect("finish")
    }

    fn first_hit(root: &Path, query: &str) -> Option<String> {
        TextSearcher::open(root)
            .expect("searcher")
            .search(&TextSearchOptions {
                query: query.into(),
                limit: 1,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search")
            .first()
            .map(|hit| hit.record.id.clone())
    }

    #[test]
    fn writes_one_file_and_reads_records_by_row_layer_and_source_id() {
        let root = temp_root("pack");
        let manifest = write_pack(
            &root,
            &[
                address_record(1, "10", "King Street"),
                address_record(2, "20", "Queen Street"),
            ],
        );
        assert_eq!(manifest.record_count, 2);
        assert_eq!(manifest.text_document_count, 2);
        assert_eq!(manifest.spatial.point_count, 2);

        let reader = Reader::open(&root).expect("reader");
        assert_eq!(
            reader.path().file_name().and_then(|name| name.to_str()),
            Some(PACK_FILE)
        );
        assert_eq!(reader.record_summary(1).expect("row 1").id, "osm:node:2");
        assert_eq!(
            reader.find_by_source_id("osm:node:1").expect("lookup"),
            Some(0)
        );
        assert_eq!(
            reader
                .records_json_by_layer("address", 10)
                .expect("layer")
                .len(),
            2
        );
        assert!(
            reader
                .records_json_by_layer("street", 10)
                .expect("layer")
                .is_empty()
        );
        assert!(reader.records_json_by_layer("nope", 10).is_err());
        let sections = reader.section_sizes();
        assert!(sections.contains_key("records/blocks"));
        assert!(sections.keys().any(|name| name.starts_with("text/")));
        assert!(sections.contains_key("spatial/blocks"));
        assert_eq!(first_hit(&root, "queen").as_deref(), Some("osm:node:2"));

        // The Pack file alone is a complete Pack.
        let copy = temp_root("copy").with_extension("ogp");
        fs::copy(reader.path(), &copy).expect("copy");
        assert_eq!(first_hit(&copy, "king").as_deref(), Some("osm:node:1"));
        assert!(!reader.path().parent().unwrap().join(SCRATCH_DIR).exists());
    }

    #[test]
    fn failed_osm_rebuild_preserves_published_pack() {
        let root = temp_root("rebuild");
        write_pack(&root, &[address_record(1, "10", "King Street")]);
        let generations = fs::read_dir(root.join("generations")).expect("dir").count();

        let result = crate::builder::build_osm_pack(&crate::builder::BuildOsmOptions {
            inputs: vec![root.join("missing.osm.pbf")],
            pack: root.clone(),
            ..crate::builder::BuildOsmOptions::default()
        });
        assert!(result.is_err());
        assert_eq!(first_hit(&root, "king").as_deref(), Some("osm:node:1"));
        assert_eq!(
            fs::read_dir(root.join("generations")).expect("dir").count(),
            generations,
            "the failed generation is removed"
        );
    }

    #[test]
    fn publishes_replacement_without_changing_existing_readers() {
        let root = temp_root("publish");
        write_pack(&root, &[address_record(1, "10", "King Street")]);
        let original = Reader::open(&root).expect("original reader");
        let searcher = TextSearcher::open(&root).expect("original searcher");

        let mut replacement = Writer::create(&root).expect("replacement");
        replacement
            .write(&address_record(2, "20", "Queen Street"), None)
            .expect("address");
        assert_eq!(first_hit(&root, "king").as_deref(), Some("osm:node:1"));
        replacement.finish().expect("publish");

        assert_eq!(first_hit(&root, "queen").as_deref(), Some("osm:node:2"));
        assert_eq!(
            original.record_summary(0).expect("original").id,
            "osm:node:1"
        );
        let hits = searcher
            .search(&TextSearchOptions {
                query: "king".into(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("existing searcher after publish");
        assert_eq!(hits[0].record.id, "osm:node:1");
    }

    #[test]
    fn validation_failure_does_not_publish_replacement() {
        let root = temp_root("validation");
        write_pack(&root, &[address_record(1, "10", "King Street")]);
        let published = fs::read(root.join("CURRENT")).expect("pointer");

        let mut replacement = Writer::create(&root).expect("replacement");
        replacement
            .write(&address_record(2, "20", "Queen Street"), None)
            .expect("address");
        let (sealed, _) = replacement.seal().expect("seal");
        let generation = sealed.generation().to_path_buf();
        // Damage the table of contents of the unpublished Pack.
        let pack = generation.join(PACK_FILE);
        let mut bytes = fs::read(&pack).expect("read");
        let len = bytes.len();
        bytes[len - 1] ^= 0xff;
        fs::File::create(&pack)
            .and_then(|mut file| file.write_all(&bytes))
            .expect("corrupt");
        assert!(sealed.publish().is_err());
        assert_eq!(fs::read(root.join("CURRENT")).expect("pointer"), published);
        assert!(!generation.exists());
        assert_eq!(first_hit(&root, "king").as_deref(), Some("osm:node:1"));
    }

    #[test]
    fn publishing_refuses_a_pack_corrupted_inside_a_section() {
        let root = temp_root("checksum");
        write_pack(&root, &[address_record(1, "10", "King Street")]);
        let published = fs::read(root.join("CURRENT")).expect("pointer");

        let mut replacement = Writer::create(&root).expect("replacement");
        replacement
            .write(&address_record(2, "20", "Queen Street"), None)
            .expect("address");
        let (sealed, _) = replacement.seal().expect("seal");
        let pack = sealed.generation().join(PACK_FILE);
        let offset = Container::open(&pack).expect("open").sections()["records/strings"].offset;
        let mut bytes = fs::read(&pack).expect("read");
        let last = bytes.len() - 1;
        bytes[(offset as usize + 20).min(last)] ^= 0x55;
        fs::write(&pack, &bytes).expect("corrupt");

        let error = sealed.publish().unwrap_err();
        assert!(format!("{error:#}").contains("checksum"), "{error:#}");
        assert_eq!(fs::read(root.join("CURRENT")).expect("pointer"), published);
    }

    #[test]
    fn a_failure_after_the_pointer_switch_keeps_the_published_generation() {
        let root = temp_root("switch-failure");
        write_pack(&root, &[address_record(1, "10", "King Street")]);
        let mut replacement = Writer::create(&root).expect("replacement");
        replacement
            .write(&address_record(2, "20", "Queen Street"), None)
            .expect("address");
        let (sealed, _) = replacement.seal().expect("seal");

        publication::FAIL_AFTER_SWITCH.set(true);
        let result = sealed.publish();
        publication::FAIL_AFTER_SWITCH.set(false);
        assert!(result.is_err());
        // CURRENT already names the new generation, so it must still exist.
        assert_eq!(first_hit(&root, "queen").as_deref(), Some("osm:node:2"));
    }

    #[test]
    fn text_index_memory_fits_the_budget_and_waits_for_records() {
        let root = temp_root("text-memory");
        let memory = Budget::new(64 << 20);
        let mut writer = Writer::create_with(
            &root,
            &WriterOptions {
                memory: Arc::clone(&memory),
                text_index_memory_bytes: 16 << 20,
            },
        )
        .expect("writer");
        assert_eq!(
            memory.used(),
            0,
            "nothing is reserved before records arrive"
        );
        writer
            .write(&address_record(1, "10", "King Street"), None)
            .expect("address");
        assert!(memory.used() > 0);
        assert!(
            memory.peak() <= memory.limit(),
            "peak {} of {}",
            memory.peak(),
            memory.limit()
        );
        writer.finish().expect("finish");
        assert_eq!(first_hit(&root, "king").as_deref(), Some("osm:node:1"));
    }

    #[test]
    fn rejects_paths_without_a_pack() {
        let root = temp_root("empty");
        fs::create_dir_all(&root).expect("dir");
        assert!(Reader::open(&root).is_err());
    }
}
