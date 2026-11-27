use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap},
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::error::{Error, Result};
use rstar::{RTree, primitives::GeomWithData};
use tantivy::{
    Index, IndexWriter, TantivyDocument,
    directory::MmapDirectory,
    directory::{
        Directory, FileHandle, RamDirectory, WatchCallback, WatchHandle, WritePtr,
        error::{DeleteError, OpenReadError, OpenWriteError},
    },
    indexer::merge_indices,
    merge_policy::NoMergePolicy,
    schema::{FAST, Field, IndexRecordOption, STRING, Schema, TextFieldIndexing, TextOptions},
};

use crate::{
    container::{self, Bytes, Container},
    extsort::Scratch,
    memory::{Budget, Reservation},
    pack::RecordId,
    record::{
        AddressComponents, AddressRecord, InterpolationAddressComponents, InterpolationRecord,
        Layer, PlaceRecord, PoiRecord, PostcodeRecord, Record, StreetRecord, place_rank,
    },
    records::quantize,
    util::geo::haversine_m,
    util::{fold::search_key, geo::line_parts, text::collapse_whitespace},
};

pub const TEXT_SECTION_PREFIX: &str = "text/";
pub const TEXT_INDEX_SCHEMA_VERSION: u32 = 15;
pub(crate) const POSTCODE_AREA_RADIUS_M: f64 = 10_000.0;

/// Tantivy needs at least 15 MB per indexing thread (it uses fewer threads
/// when given less) and gains little beyond 1.6 GB.
const MIN_INDEX_MEMORY_BYTES: usize = 16 << 20;
const MAX_INDEX_MEMORY_BYTES: usize = 1_600_000_000;
const INDEX_THREAD_MEMORY_BYTES: usize = 32 << 20;
const MAX_INDEX_PARTS: usize = 16;
const CHUNK_DOCUMENTS: u64 = 8_192;
const DIRECTORY: &str = "text";
const MERGED_DIR: &str = "merged";

fn part_dir(part: usize) -> String {
    format!("part-{part}")
}

fn part_count(memory_bytes: usize) -> usize {
    rayon::current_num_threads()
        .min(MAX_INDEX_PARTS)
        .min(memory_bytes / INDEX_THREAD_MEMORY_BYTES)
        .max(1)
}

fn create_dir(path: &Path) -> Result<PathBuf> {
    fs::create_dir_all(path).map_err(|source| Error::IoPath {
        action: "failed to create",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(path.to_path_buf())
}
const LABEL: &str = "pack-text";
const AUTOCOMPLETE_SUBJECT_FIELD: &str = "autocomplete_subject_text";
const PLACE_NAME_FIELD: &str = "place_name_text";
const PLACE_CONTENT_FIELD: &str = "place_content_text";
const PLACE_AUTOCOMPLETE_SUBJECT_FIELD: &str = "place_autocomplete_subject_text";
const PLACE_ADDRESS_NUMBER_FIELD: &str = "place_address_number";
pub(crate) const INTERPOLATION_START_FIELD: &str = "interpolation_start";
pub(crate) const INTERPOLATION_END_FIELD: &str = "interpolation_end";
pub(crate) const HOUSE_NUMBER_FIELD: &str = "house_number";
pub(crate) const RANK_POSTCODE_FIELD: &str = "rank_postcode";
pub(crate) const AREA_POSTCODE_FIELD: &str = "area_postcode";
pub(crate) const COUNTRY_CODE_FIELD: &str = "country_code";
pub(crate) const MIN_LON_FIELD: &str = "min_lon";
pub(crate) const MIN_LAT_FIELD: &str = "min_lat";
pub(crate) const MAX_LON_FIELD: &str = "max_lon";
pub(crate) const MAX_LAT_FIELD: &str = "max_lat";
pub(crate) const RECORD_COUNT_FIELD: &str = "record_count";
pub(crate) const NAME_WORDS_FIELD: &str = "name_words";
pub(crate) const PLACE_RANK_FIELD: &str = "place_rank";
pub(crate) const PROMINENCE_FIELD: &str = "prominence";
const MAX_GROUP_RECORDS: u64 = 1_024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WriterState {
    document_count: u64,
    parts: usize,
    merged: bool,
}

pub struct Writer {
    writers: Option<(Vec<IndexWriter>, Reservation)>,
    budget: Arc<Budget>,
    memory_bytes: usize,
    parts: Vec<Index>,
    path: PathBuf,
    scratch: Arc<Scratch>,
    fields: Fields,
    chunk_documents: u64,
    document_count: u64,
    committed: u64,
    group: Option<(Document, Numbers)>,
    merged: bool,
}

#[derive(Debug)]
pub enum Prepared {
    Document(TantivyDocument),
    Address(Box<Document>, Numbers),
}

#[derive(Debug, Default)]
pub struct Numbers {
    units: Vec<String>,
    terms: Vec<String>,
    lowest: Option<u32>,
}

impl Numbers {
    fn of(projected: &Document) -> Self {
        let mut terms = Vec::new();
        let mut lowest: Option<u32> = None;
        for number in &projected.address_numbers {
            terms.extend(address_number_terms(number));
            if let Some(base) = base_house_number(number) {
                lowest = Some(lowest.map_or(base, |lowest| lowest.min(base)));
            }
        }
        terms.extend(projected.flat_numbers.iter().cloned());
        Self {
            units: projected
                .units
                .iter()
                .filter_map(|unit| search_text(unit))
                .collect(),
            terms,
            lowest,
        }
    }

    fn join(&mut self, other: Self) {
        self.units.extend(other.units);
        self.terms.extend(other.terms);
        self.lowest = match (self.lowest, other.lowest) {
            (Some(lowest), Some(other)) => Some(lowest.min(other)),
            (lowest, other) => lowest.or(other),
        };
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Fields {
    pub record_id: Field,
    pub record_count: Field,
    pub layer: Field,
    pub content_text: Field,
    pub context_text: Field,
    pub name_text: Field,
    pub street_text: Field,
    pub address_number: Field,
    pub postcode_exact: Field,
    pub autocomplete_subject_text: Field,
    pub place_name_text: Field,
    pub place_content_text: Field,
    pub place_autocomplete_subject_text: Field,
    pub place_address_number: Field,
    pub interpolation_start: Field,
    pub interpolation_end: Field,
    pub house_number: Field,
    pub rank_postcode: Field,
    pub area_postcode: Field,
    /// The ISO 3166-1 alpha-2 code of the country the record lies in, upper
    /// case, so a Pack covering several countries can be searched one at a
    /// time. Exact, like `layer`: it is filtered on, never matched as text.
    pub country_code: Field,
    pub min_lon: Field,
    pub min_lat: Field,
    pub max_lon: Field,
    pub max_lat: Field,
    pub name_words: Field,
    pub place_rank: Field,
    pub prominence: Field,
}

/// The smallest box holding a document's records, in 1e-7 degrees, the
/// precision the record store keeps. A street group's covers each of its
/// addresses, a street's its whole line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub min_lon: i32,
    pub min_lat: i32,
    pub max_lon: i32,
    pub max_lat: i32,
}

impl Extent {
    /// The extent of a record's geometry, `None` when it has no finite
    /// coordinates in range.
    pub fn of(record: &Record) -> Option<Self> {
        let point = |lon: f64, lat: f64| {
            let (lon, lat) = (quantize(lon).ok()?, quantize(lat).ok()?);
            Some(Self {
                min_lon: lon,
                min_lat: lat,
                max_lon: lon,
                max_lat: lat,
            })
        };
        match line_parts(record.geometry()) {
            Some(parts) => parts
                .into_iter()
                .flatten()
                .map(|position| point(position[0], position[1]))
                .reduce(|left, right| Some(left?.union(right?)))
                .flatten(),
            None => record
                .display_point()
                .and_then(|[lon, lat]| point(lon, lat)),
        }
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            min_lon: self.min_lon.min(other.min_lon),
            min_lat: self.min_lat.min(other.min_lat),
            max_lon: self.max_lon.max(other.max_lon),
            max_lat: self.max_lat.max(other.max_lat),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Commit {
    pub document_count: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub record_id: RecordId,
    pub record_count: u64,
    pub layers: Vec<String>,
    pub content_text: String,
    pub context_text: String,
    pub name: Option<String>,
    pub streets: Vec<String>,
    pub address_numbers: Vec<String>,
    pub flat_numbers: Vec<String>,
    pub units: Vec<String>,
    pub postcode: Option<String>,
    pub autocomplete_subject_text: String,
    pub interpolation_range: Option<(u32, u32)>,
    pub rank_postcodes: Vec<String>,
    pub area_postcode: Option<String>,
    pub country_code: Option<String>,
    pub extent: Option<Extent>,
    pub name_words: Option<u64>,
    pub place_rank: Option<u8>,
    pub prominence: Option<u64>,
}

impl fmt::Debug for Writer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Writer")
            .field("fields", &self.fields)
            .field("document_count", &self.document_count)
            .finish_non_exhaustive()
    }
}

impl Writer {
    pub fn create(
        scratch: &Arc<Scratch>,
        budget: &Arc<Budget>,
        memory_bytes: usize,
    ) -> Result<Self> {
        let (schema, fields) = build_schema();
        let path = scratch.path().join(DIRECTORY);
        fs::create_dir_all(&path).map_err(|source| Error::IoPath {
            action: "failed to create",
            path: path.clone(),
            source,
        })?;
        scratch.watch(LABEL, &path);
        let memory_bytes = memory_bytes.clamp(MIN_INDEX_MEMORY_BYTES, MAX_INDEX_MEMORY_BYTES);
        let parts = (0..part_count(memory_bytes))
            .map(|part| {
                let dir = create_dir(&path.join(part_dir(part)))?;
                Ok(Index::create_in_dir(&dir, schema.clone())?)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            scratch: Arc::clone(scratch),
            writers: None,
            budget: Arc::clone(budget),
            memory_bytes,
            parts,
            path,
            fields,
            chunk_documents: CHUNK_DOCUMENTS,
            document_count: 0,
            committed: 0,
            group: None,
            merged: false,
        })
    }

    pub fn fields(&self) -> Fields {
        self.fields
    }

    pub fn state(&mut self) -> Result<WriterState> {
        self.close_group()?;
        if let Some((writers, _)) = &mut self.writers
            && self.committed < self.document_count
        {
            for writer in writers {
                writer.commit()?;
            }
            self.committed = self.document_count;
        }
        Ok(WriterState {
            document_count: self.document_count,
            parts: self.parts.len(),
            merged: self.merged,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn restore(
        scratch: &Arc<Scratch>,
        budget: &Arc<Budget>,
        memory_bytes: usize,
        state: &WriterState,
    ) -> Result<Self> {
        let path = scratch.path().join(DIRECTORY);
        scratch.watch(LABEL, &path);
        let parts = (0..state.parts)
            .map(|part| Ok(Index::open_in_dir(path.join(part_dir(part)))?))
            .collect::<Result<Vec<_>>>()?;
        let Some(first) = parts.first() else {
            return Err(Error::Inconsistent(
                "a text index checkpoint names no parts".into(),
            ));
        };
        let fields = Fields::from_schema(&first.schema())?;
        Ok(Self {
            scratch: Arc::clone(scratch),
            writers: None,
            budget: Arc::clone(budget),
            memory_bytes: memory_bytes.clamp(MIN_INDEX_MEMORY_BYTES, MAX_INDEX_MEMORY_BYTES),
            parts,
            path,
            fields,
            chunk_documents: CHUNK_DOCUMENTS,
            document_count: state.document_count,
            committed: state.document_count,
            group: None,
            merged: state.merged,
        })
    }

    fn writers(&mut self) -> Result<&mut Vec<IndexWriter>> {
        if self.writers.is_none() {
            let mut memory = Reservation::empty(&self.budget);
            if !memory.try_grow(self.memory_bytes) {
                memory.force_grow(self.memory_bytes);
            }
            let share = self.memory_bytes / self.parts.len();
            let writers = self
                .parts
                .iter()
                .map(|part| {
                    let writer = part.writer_with_num_threads(1, share)?;
                    writer.set_merge_policy(Box::new(NoMergePolicy));
                    Ok(writer)
                })
                .collect::<Result<Vec<_>>>()?;
            self.writers = Some((writers, memory));
        }
        Ok(&mut self.writers.as_mut().expect("writers created").0)
    }

    pub fn add(&mut self, document: TantivyDocument) -> Result<()> {
        self.close_group()?;
        self.add_document(document)
    }

    fn add_document(&mut self, document: TantivyDocument) -> Result<()> {
        let parts = self.parts.len() as u64;
        let part = (self.document_count / self.chunk_documents % parts) as usize;
        self.writers()?[part].add_document(document)?;
        self.document_count += 1;
        Ok(())
    }

    pub fn add_address(&mut self, document: Box<Document>, numbers: Numbers) -> Result<()> {
        if let Some((group, group_numbers)) = &mut self.group
            && group.record_id + group.record_count == document.record_id
            && group.record_count < MAX_GROUP_RECORDS
            && group.same_group(&document)
        {
            group.record_count += 1;
            group.extent = match (group.extent, document.extent) {
                (Some(group), Some(address)) => Some(group.union(address)),
                (group, address) => group.or(address),
            };
            group_numbers.join(numbers);
            return Ok(());
        }
        self.close_group()?;
        self.group = Some((*document, numbers));
        Ok(())
    }

    fn close_group(&mut self) -> Result<()> {
        if let Some((group, numbers)) = self.group.take() {
            let document = self.fields.to_tantivy_document(&group, numbers);
            self.add_document(document)?;
        }
        Ok(())
    }

    pub fn finish(mut self, pack: &mut container::Writer) -> Result<Commit> {
        if !self.merged {
            self.merge()?;
        }
        let merged = self.path.join(MERGED_DIR);
        let mut files = fs::read_dir(&merged)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<_>>>()?;
        files.sort();
        let mut bytes = 0;
        for file in files {
            let name = file
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or(Error::Decode("text index file name is not UTF-8"))?;
            if name.ends_with(".lock") {
                continue;
            }
            let section = format!("{TEXT_SECTION_PREFIX}{name}");
            if name.ends_with(STORE_EXTENSION) {
                let store = empty_store()?;
                bytes += store.len() as u64;
                pack.add(&section, TEXT_INDEX_SCHEMA_VERSION, &store)?;
                continue;
            }
            bytes += fs::metadata(&file)?.len();
            pack.add_file(&section, TEXT_INDEX_SCHEMA_VERSION, &file)?;
        }
        let Self {
            parts,
            path,
            scratch,
            document_count,
            ..
        } = self;
        drop(parts);
        scratch.measure();
        scratch.remove_dir(&path)?;
        Ok(Commit {
            document_count,
            bytes,
        })
    }

    pub fn merge(&mut self) -> Result<()> {
        if self.merged {
            return Ok(());
        }
        self.close_group()?;
        let mut writers = std::mem::take(self.writers()?);
        std::thread::scope(|scope| {
            let commits = writers
                .iter_mut()
                .map(|writer| scope.spawn(move || writer.commit()))
                .collect::<Vec<_>>();
            commits.into_iter().try_for_each(|commit| {
                commit
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
                    .map(|_| ())
            })
        })?;
        for writer in writers {
            writer.wait_merging_threads()?;
        }
        self.writers = None;
        let merged = self.path.join(MERGED_DIR);
        if merged.exists() {
            self.scratch.remove_dir(&merged)?;
        }
        let merged = create_dir(&merged)?;
        let directory = MmapDirectory::open(&merged)
            .map_err(|error| Error::Inconsistent(format!("text index directory: {error}")))?;
        let index = merge_indices(&self.parts, directory)?;
        drop(index);
        self.merged = true;
        Ok(())
    }
}

impl Fields {
    pub fn from_schema(schema: &Schema) -> Result<Self> {
        Ok(Self {
            record_id: schema.get_field("record_id")?,
            record_count: schema.get_field(RECORD_COUNT_FIELD)?,
            layer: schema.get_field("layer")?,
            content_text: schema.get_field("content_text")?,
            context_text: schema.get_field("context_text")?,
            name_text: schema.get_field("name_text")?,
            address_number: schema.get_field("address_number")?,
            postcode_exact: schema.get_field("postcode_exact")?,
            autocomplete_subject_text: schema.get_field(AUTOCOMPLETE_SUBJECT_FIELD)?,
            place_name_text: schema.get_field(PLACE_NAME_FIELD)?,
            place_content_text: schema.get_field(PLACE_CONTENT_FIELD)?,
            place_autocomplete_subject_text: schema.get_field(PLACE_AUTOCOMPLETE_SUBJECT_FIELD)?,
            place_address_number: schema.get_field(PLACE_ADDRESS_NUMBER_FIELD)?,
            interpolation_start: schema.get_field(INTERPOLATION_START_FIELD)?,
            interpolation_end: schema.get_field(INTERPOLATION_END_FIELD)?,
            street_text: schema.get_field("street_text")?,
            house_number: schema.get_field(HOUSE_NUMBER_FIELD)?,
            rank_postcode: schema.get_field(RANK_POSTCODE_FIELD)?,
            area_postcode: schema.get_field(AREA_POSTCODE_FIELD)?,
            country_code: schema.get_field(COUNTRY_CODE_FIELD)?,
            min_lon: schema.get_field(MIN_LON_FIELD)?,
            min_lat: schema.get_field(MIN_LAT_FIELD)?,
            max_lon: schema.get_field(MAX_LON_FIELD)?,
            max_lat: schema.get_field(MAX_LAT_FIELD)?,
            name_words: schema.get_field(NAME_WORDS_FIELD)?,
            place_rank: schema.get_field(PLACE_RANK_FIELD)?,
            prominence: schema.get_field(PROMINENCE_FIELD)?,
        })
    }

    pub fn prepare(
        self,
        record_id: RecordId,
        record: &Record,
        context_names: &[&str],
        postcode_areas: &PostcodeAreas,
        country_code: Option<&str>,
    ) -> Prepared {
        let document = Document::from_record(
            record_id,
            record,
            context_names,
            postcode_areas,
            country_code,
        );
        let numbers = Numbers::of(&document);
        match record {
            Record::Address(_) => Prepared::Address(Box::new(document), numbers),
            _ => Prepared::Document(self.to_tantivy_document(&document, numbers)),
        }
    }

    pub fn for_places(self) -> Self {
        Self {
            name_text: self.place_name_text,
            content_text: self.place_content_text,
            street_text: self.place_content_text,
            autocomplete_subject_text: self.place_autocomplete_subject_text,
            address_number: self.place_address_number,
            ..self
        }
    }

    fn to_tantivy_document(self, projected: &Document, numbers: Numbers) -> TantivyDocument {
        let place = projected
            .layers
            .iter()
            .any(|layer| layer == Layer::Poi.as_str());
        let fields = if place { self.for_places() } else { self };
        fields.write_document(projected, numbers, place)
    }

    fn write_document(
        self,
        projected: &Document,
        numbers: Numbers,
        place: bool,
    ) -> TantivyDocument {
        let mut document = TantivyDocument::default();
        document.add_u64(self.record_id, projected.record_id);
        document.add_u64(self.record_count, projected.record_count);
        for layer in &projected.layers {
            document.add_text(self.layer, layer);
        }
        let units = unique_parts(numbers.units);
        let content_text = std::iter::once(projected.content_text.as_str())
            .chain(units.iter().map(String::as_str))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !content_text.is_empty() {
            document.add_text(self.content_text, &content_text);
        }
        if !projected.context_text.is_empty() {
            document.add_text(self.context_text, &projected.context_text);
        }
        add_text_if_present(&mut document, self.name_text, projected.name.as_deref());
        add_text_if_present(
            &mut document,
            self.street_text,
            unique_words(&projected.streets.join(" ")).as_deref(),
        );
        let terms = numbers.terms.into_iter().collect::<BTreeSet<_>>();
        for term in &terms {
            document.add_text(self.address_number, term);
        }
        if let Some(base) = numbers.lowest {
            document.add_u64(self.house_number, base.into());
        }
        if !place {
            add_normalized_text_if_present(
                &mut document,
                self.postcode_exact,
                projected.postcode.as_deref(),
            );
        }
        if !projected.autocomplete_subject_text.is_empty() {
            document.add_text(
                self.autocomplete_subject_text,
                &projected.autocomplete_subject_text,
            );
        }
        if let Some((start, end)) = projected.interpolation_range {
            document.add_u64(self.interpolation_start, start.into());
            document.add_u64(self.interpolation_end, end.into());
        }
        for postcode in &projected.rank_postcodes {
            document.add_text(self.rank_postcode, postcode);
        }
        if let Some(postcode) = &projected.area_postcode {
            document.add_text(self.area_postcode, postcode);
        }
        if let Some(country) = &projected.country_code {
            document.add_text(self.country_code, country);
        }
        if let Some(extent) = projected.extent {
            document.add_i64(self.min_lon, extent.min_lon.into());
            document.add_i64(self.min_lat, extent.min_lat.into());
            document.add_i64(self.max_lon, extent.max_lon.into());
            document.add_i64(self.max_lat, extent.max_lat.into());
        }
        if let Some(words) = projected.name_words {
            document.add_u64(self.name_words, words);
        }
        if let Some(rank) = projected.place_rank {
            document.add_u64(self.place_rank, rank.into());
        }
        if let Some(rank) = projected.prominence {
            document.add_u64(self.prominence, rank);
        }
        document
    }
}

impl Document {
    fn same_group(&self, other: &Self) -> bool {
        self.layers == other.layers
            && self.content_text == other.content_text
            && self.context_text == other.context_text
            && self.name == other.name
            && self.streets == other.streets
            && self.postcode == other.postcode
            && self.autocomplete_subject_text == other.autocomplete_subject_text
            && self.interpolation_range == other.interpolation_range
            && self.rank_postcodes == other.rank_postcodes
            && self.area_postcode == other.area_postcode
            && self.country_code == other.country_code
    }

    pub fn from_record(
        record_id: RecordId,
        record: &Record,
        context_names: &[&str],
        postcode_areas: &PostcodeAreas,
        country_code: Option<&str>,
    ) -> Self {
        let builder = match record {
            Record::Address(record) => Self::project_address(record_id, record),
            Record::Poi(record) => Self::project_poi(record_id, record),
            Record::Interpolation(record) => Self::project_interpolation(record_id, record),
            Record::Street(record) => Self::project_street(record_id, record),
            Record::Postcode(record) => Self::project_postcode(record_id, record),
            Record::Place(layer, record) => Self::project_place(record_id, layer.as_str(), record),
        };
        let mut document = builder.build(context_names);
        document.country_code = country_code.map(str::to_string);
        document.extent = Extent::of(record);
        if !matches!(record, Record::Place(..)) {
            document.rank_postcodes = match (document.postcode.as_deref(), record) {
                (Some(postcode), _) => compact_postcode(postcode).into_iter().collect(),
                (None, Record::Address(address)) => address
                    .street_postcodes
                    .iter()
                    .filter_map(|postcode| compact_postcode(postcode))
                    .collect(),
                (None, _) => Vec::new(),
            };
            if document.rank_postcodes.is_empty() {
                document.area_postcode = record
                    .display_point()
                    .and_then(|point| postcode_areas.nearest(point))
                    .map(str::to_string);
            }
        }
        match record {
            Record::Place(_, place) => {
                document.place_rank = place_rank(&place.place_type);
                document.prominence = Some(place.prominence.rank());
            }
            Record::Poi(poi) => document.prominence = Some(poi.prominence.rank()),
            _ => {}
        }
        document.name = document.name.as_deref().and_then(unique_words);
        document.content_text = unique_words(&document.content_text).unwrap_or_default();
        document
    }

    fn project_address(record_id: RecordId, address: &AddressRecord) -> ProjectionBuilder {
        let mut builder = ProjectionBuilder::new(record_id, "address");
        if let Some(subject) = address.address.subject() {
            builder.name_for_search(subject);
            if let Some(words) = &mut builder.projected.name_words {
                *words += 1;
            }
        }
        builder.address_parts(&address.address);
        builder.projected.units.extend(address.address.unit.clone());
        builder.projected.flat_numbers.extend(flat_number_term(
            &address.address.number,
            address.address.unit.as_deref(),
        ));
        builder
    }

    fn project_poi(record_id: RecordId, poi: &PoiRecord) -> ProjectionBuilder {
        let mut builder = ProjectionBuilder::new(record_id, Layer::Poi.as_str());
        builder.name(&poi.name);
        for name in &poi.alt_names {
            builder.name(name);
        }
        builder.add_content_text(&poi.category);
        if let Some(brand) = &poi.brand {
            builder.add_content_text(&brand.name);
            builder.add_optional_autocomplete_text(Some(&brand.name));
        }
        if let Some(address) = &poi.address {
            if poi.source.gers().is_some() {
                builder.add_optional_context_text(address.locality.as_deref());
                builder.add_optional_context_text(address.region.as_deref());
                builder.add_optional_context_text(address.country.as_deref());
            } else {
                builder.address(address);
            }
        }
        builder
    }

    fn project_interpolation(
        record_id: RecordId,
        interpolation: &InterpolationRecord,
    ) -> ProjectionBuilder {
        let mut builder = ProjectionBuilder::new(record_id, "interpolation");
        builder.name_for_search(&interpolation.name());
        let address = &interpolation.address;
        builder.street(address.street.as_deref().or(address.place.as_deref()));
        builder.interpolation_address(address);
        let range = &interpolation.interpolation;
        builder.projected.interpolation_range = Some((range.start, range.end));
        builder
    }

    fn project_street(record_id: RecordId, street: &StreetRecord) -> ProjectionBuilder {
        let mut builder = ProjectionBuilder::new(record_id, "street");
        for name in std::iter::once(&street.name).chain(&street.alt_names) {
            builder.name(name);
            builder.street(Some(name));
        }
        builder
    }

    fn project_postcode(record_id: RecordId, postcode: &PostcodeRecord) -> ProjectionBuilder {
        let mut builder = ProjectionBuilder::new(record_id, "postcode");
        builder.name_for_search(&postcode.name());
        builder.postcode(&postcode.postcode);
        builder
    }

    fn project_place(record_id: RecordId, layer: &str, place: &PlaceRecord) -> ProjectionBuilder {
        let mut builder = ProjectionBuilder::new(record_id, layer);
        builder.name(&place.name);
        for name in &place.alt_names {
            builder.name(name);
        }
        builder
    }
}

pub fn open_text_index(container: &Container) -> Result<Index> {
    Ok(Index::open(PackDirectory::new(container)?)?)
}

#[derive(Clone)]
struct PackDirectory {
    files: Arc<HashMap<PathBuf, Bytes>>,
    scratch: RamDirectory,
}

impl fmt::Debug for PackDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackDirectory")
            .field("files", &self.files.len())
            .finish()
    }
}

impl PackDirectory {
    fn new(container: &Container) -> Result<Self> {
        let mut files = HashMap::new();
        for (name, info) in container.sections() {
            let Some(file) = name.strip_prefix(TEXT_SECTION_PREFIX) else {
                continue;
            };
            if info.version != TEXT_INDEX_SCHEMA_VERSION {
                return Err(Error::UnsupportedFormat {
                    part: format!("section {name}"),
                    found: info.version,
                    expected: TEXT_INDEX_SCHEMA_VERSION,
                });
            }
            files.insert(PathBuf::from(file), container.raw_section(name)?);
        }
        if files.is_empty() {
            return Err(Error::MissingSection {
                section: "text index",
            });
        }
        Ok(Self {
            files: Arc::new(files),
            scratch: RamDirectory::create(),
        })
    }
}

impl Directory for PackDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        match self.files.get(path) {
            Some(bytes) => Ok(Arc::new(bytes.clone())),
            None => self.scratch.get_file_handle(path),
        }
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        self.scratch.delete(path)
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        Ok(self.files.contains_key(path) || self.scratch.exists(path)?)
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        self.scratch.open_write(path)
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        match self.files.get(path) {
            Some(bytes) => Ok(bytes.as_slice().to_vec()),
            None => self.scratch.atomic_read(path),
        }
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        self.scratch.atomic_write(path, data)
    }

    fn sync_directory(&self) -> io::Result<()> {
        Ok(())
    }

    fn watch(&self, _watch_callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        Ok(WatchHandle::empty())
    }
}

const STORE_EXTENSION: &str = ".store";

/// Nothing reads stored documents, but tantivy writes a 4-byte offset per
/// document anyway (471 MB on planet) and opens every segment's store, so
/// the file must stay a valid store. Tantivy never checks its doc count.
fn empty_store() -> Result<Vec<u8>> {
    let (schema, _) = build_schema();
    let index = Index::create_in_ram(schema);
    let mut writer: IndexWriter = index.writer_with_num_threads(1, MIN_INDEX_MEMORY_BYTES)?;
    writer.add_document(TantivyDocument::default())?;
    writer.commit()?;
    drop(writer);
    let segment = index
        .searchable_segments()?
        .into_iter()
        .next()
        .ok_or_else(|| Error::Inconsistent("an empty store's index has no segment".into()))?;
    let path = segment.relative_path(tantivy::index::SegmentComponent::Store);
    index
        .directory()
        .atomic_read(&path)
        .map_err(|error| Error::Inconsistent(format!("empty document store: {error}")))
}

fn build_schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let record_id = builder.add_u64_field("record_id", FAST);
    let record_count = builder.add_u64_field(RECORD_COUNT_FIELD, FAST);
    let exact_unstored = exact_string_options();
    let layer = builder.add_text_field("layer", exact_unstored.clone());
    let content_text = builder.add_text_field("content_text", searchable_text_options());
    let context_text = builder.add_text_field("context_text", searchable_text_options());
    let name_text = builder.add_text_field("name_text", searchable_text_options());
    let address_number = builder.add_text_field("address_number", exact_unstored.clone());
    let postcode_exact = builder.add_text_field("postcode_exact", exact_unstored);
    let autocomplete_subject_text = builder.add_text_field(
        AUTOCOMPLETE_SUBJECT_FIELD,
        autocomplete_subject_text_options(),
    );
    let place_name_text = builder.add_text_field(PLACE_NAME_FIELD, searchable_text_options());
    let place_content_text = builder.add_text_field(PLACE_CONTENT_FIELD, searchable_text_options());
    let place_autocomplete_subject_text = builder.add_text_field(
        PLACE_AUTOCOMPLETE_SUBJECT_FIELD,
        autocomplete_subject_text_options(),
    );
    let place_address_number =
        builder.add_text_field(PLACE_ADDRESS_NUMBER_FIELD, exact_string_options());
    let interpolation_start = builder.add_u64_field(INTERPOLATION_START_FIELD, FAST);
    let interpolation_end = builder.add_u64_field(INTERPOLATION_END_FIELD, FAST);
    let street_text = builder.add_text_field("street_text", searchable_text_options());
    let house_number = builder.add_u64_field(HOUSE_NUMBER_FIELD, FAST);
    let rank_postcode = builder.add_text_field(
        RANK_POSTCODE_FIELD,
        TextOptions::default().set_fast(Some("raw")),
    );
    let area_postcode = builder.add_text_field(
        AREA_POSTCODE_FIELD,
        TextOptions::default().set_fast(Some("raw")),
    );
    let country_code = builder.add_text_field(
        COUNTRY_CODE_FIELD,
        exact_string_options().set_fast(Some("raw")),
    );
    let min_lon = builder.add_i64_field(MIN_LON_FIELD, FAST);
    let min_lat = builder.add_i64_field(MIN_LAT_FIELD, FAST);
    let max_lon = builder.add_i64_field(MAX_LON_FIELD, FAST);
    let max_lat = builder.add_i64_field(MAX_LAT_FIELD, FAST);
    let name_words = builder.add_u64_field(NAME_WORDS_FIELD, FAST);
    let place_rank = builder.add_u64_field(PLACE_RANK_FIELD, FAST);
    let prominence = builder.add_u64_field(PROMINENCE_FIELD, FAST);
    let schema = builder.build();
    let fields = Fields {
        record_id,
        record_count,
        layer,
        content_text,
        context_text,
        name_text,
        address_number,
        postcode_exact,
        autocomplete_subject_text,
        place_name_text,
        place_content_text,
        place_autocomplete_subject_text,
        place_address_number,
        interpolation_start,
        interpolation_end,
        street_text,
        house_number,
        rank_postcode,
        area_postcode,
        country_code,
        min_lon,
        min_lat,
        max_lon,
        max_lat,
        name_words,
        place_rank,
        prominence,
    };
    (schema, fields)
}

fn searchable_text_options() -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("default")
            .set_index_option(IndexRecordOption::WithFreqs)
            .set_fieldnorms(false),
    )
}

fn autocomplete_subject_text_options() -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("default")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions)
            .set_fieldnorms(false),
    )
}

fn exact_string_options() -> TextOptions {
    STRING.set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("raw")
            .set_index_option(IndexRecordOption::Basic)
            .set_fieldnorms(false),
    )
}

#[derive(Debug)]
struct ProjectionBuilder {
    projected: Document,
    name_parts: Vec<String>,
    content_parts: Vec<String>,
    context_parts: Vec<String>,
    autocomplete_subject_parts: Vec<String>,
}

impl ProjectionBuilder {
    fn new(record_id: RecordId, layer: &str) -> Self {
        Self {
            projected: Document {
                record_id,
                record_count: 1,
                layers: vec![layer.to_string()],
                content_text: String::new(),
                context_text: String::new(),
                name: None,
                streets: Vec::new(),
                address_numbers: Vec::new(),
                flat_numbers: Vec::new(),
                units: Vec::new(),
                postcode: None,
                autocomplete_subject_text: String::new(),
                interpolation_range: None,
                rank_postcodes: Vec::new(),
                area_postcode: None,
                country_code: None,
                extent: None,
                name_words: None,
                place_rank: None,
                prominence: None,
            },
            name_parts: Vec::new(),
            content_parts: Vec::new(),
            context_parts: Vec::new(),
            autocomplete_subject_parts: Vec::new(),
        }
    }

    fn name(&mut self, value: &str) {
        self.name_for_search(value);
        self.add_autocomplete_subject_text(value);
    }

    fn name_for_search(&mut self, value: &str) {
        if let Some(text) = search_text(value) {
            self.projected
                .name_words
                .get_or_insert_with(|| text.split_whitespace().count() as u64);
            self.name_parts.push(text);
        }
        self.add_content_text(value);
    }

    fn street(&mut self, value: Option<&str>) {
        if let Some(street) = value.and_then(street_search_text)
            && !self.projected.streets.contains(&street)
        {
            self.projected.streets.push(street);
        }
    }

    fn address(&mut self, address: &AddressComponents) {
        self.add_content_text(&address.number);
        self.address_parts(address);
        self.add_optional_content_text(address.unit.as_deref());
    }

    fn address_parts(&mut self, address: &AddressComponents) {
        self.street(address.subject());
        self.projected
            .address_numbers
            .extend(collapse_whitespace(&address.number));
        self.add_optional_content_text(address.street.as_deref());
        self.add_optional_content_text(address.place.as_deref());
        self.add_optional_context_text(address.locality.as_deref());
        for area in &address.areas {
            self.add_optional_context_text(Some(area));
        }
        self.add_optional_context_text(address.region.as_deref());
        self.add_optional_autocomplete_text(address.subject());
        self.projected.postcode = self.add_optional_postcode_text(address.postcode.as_deref());
        self.add_optional_context_text(address.country.as_deref());
    }

    fn interpolation_address(&mut self, address: &InterpolationAddressComponents) {
        self.add_optional_content_text(address.street.as_deref());
        self.add_optional_content_text(address.place.as_deref());
        self.add_optional_context_text(address.locality.as_deref());
        self.add_optional_context_text(address.region.as_deref());
        self.projected.postcode =
            self.add_optional_postcode_for_search(address.postcode.as_deref());
        self.add_optional_context_text(address.country.as_deref());
    }

    fn postcode(&mut self, value: &str) {
        self.projected.postcode = collapse_whitespace(value);
        self.add_content_text(value);
        if let Some(normalized) = search_key(value) {
            for form in postcode_forms(&normalized) {
                self.content_parts.push(form.clone());
                self.autocomplete_subject_parts.push(form);
            }
        }
    }

    fn add_optional_autocomplete_text(&mut self, value: Option<&str>) {
        let Some(cleaned) = value.and_then(collapse_whitespace) else {
            return;
        };
        self.add_autocomplete_subject_text(&cleaned);
    }

    fn add_optional_content_text(&mut self, value: Option<&str>) {
        if let Some(cleaned) = value.and_then(collapse_whitespace) {
            self.add_content_text(&cleaned);
        }
    }

    fn add_optional_context_text(&mut self, value: Option<&str>) {
        if let Some(cleaned) = value.and_then(collapse_whitespace) {
            self.context_parts.push(cleaned);
        }
    }

    fn add_optional_postcode_text(&mut self, value: Option<&str>) -> Option<String> {
        let cleaned = self.add_optional_postcode_for_search(value)?;
        if let Some(normalized) = search_key(&cleaned) {
            self.autocomplete_subject_parts
                .extend(postcode_forms(&normalized));
        }
        Some(cleaned)
    }

    fn add_optional_postcode_for_search(&mut self, value: Option<&str>) -> Option<String> {
        let cleaned = value.and_then(collapse_whitespace)?;
        if let Some(normalized) = search_key(&cleaned) {
            self.context_parts.extend(postcode_forms(&normalized));
        }
        Some(cleaned)
    }

    fn add_content_text(&mut self, value: &str) {
        if let Some(text) = search_text(value) {
            self.content_parts.push(text);
        }
    }

    fn add_autocomplete_subject_text(&mut self, value: &str) {
        if let Some(normalized) = search_key(value) {
            self.autocomplete_subject_parts.push(normalized);
        }
    }

    fn build(mut self, context_names: &[&str]) -> Document {
        let name_parts = unique_parts(self.name_parts);
        self.projected.name = (!name_parts.is_empty()).then(|| name_parts.join(" "));
        let content_parts = unique_parts(self.content_parts);
        self.projected.content_text = content_parts.join(" ");
        self.projected.context_text = context_text(
            &self
                .context_parts
                .iter()
                .map(String::as_str)
                .chain(context_names.iter().copied())
                .collect::<Vec<_>>(),
        );
        let autocomplete_subject_parts = unique_parts(self.autocomplete_subject_parts);
        self.projected.autocomplete_subject_text = autocomplete_subject_parts.join(" ");
        self.projected
    }
}

fn postcode_forms(normalized: &str) -> Vec<String> {
    let compact = normalized.split_whitespace().collect::<String>();
    if compact == normalized {
        vec![compact]
    } else {
        vec![normalized.to_string(), compact]
    }
}

fn add_text_if_present(document: &mut TantivyDocument, field: Field, value: Option<&str>) {
    if let Some(value) = value.and_then(collapse_whitespace) {
        document.add_text(field, value);
    }
}

fn add_normalized_text_if_present(
    document: &mut TantivyDocument,
    field: Field,
    value: Option<&str>,
) {
    if let Some(value) = value.and_then(search_key) {
        document.add_text(field, value);
    }
}

pub(crate) const STREET_TYPE_ABBREVIATIONS: &[(&str, &str)] = &[
    ("ave", "avenue"),
    ("av", "avenue"),
    ("blvd", "boulevard"),
    ("cir", "circle"),
    ("ct", "court"),
    ("crt", "court"),
    ("cres", "crescent"),
    ("gdns", "gardens"),
    ("grv", "grove"),
    ("hts", "heights"),
    ("dr", "drive"),
    ("hwy", "highway"),
    ("ln", "lane"),
    ("pkwy", "parkway"),
    ("pl", "place"),
    ("rd", "road"),
    ("sq", "square"),
    ("st", "street"),
    ("ter", "terrace"),
    ("terr", "terrace"),
    ("trl", "trail"),
    ("tr", "trail"),
    ("wy", "way"),
];

pub(crate) fn expand_address_abbreviations(value: &str) -> String {
    let mut words = Vec::<Cow<'_, str>>::new();
    let mut previous: Option<&str> = None;
    let mut last_word: Option<&str> = None;
    for token in value.split_whitespace() {
        let replacement = match token {
            "st" if previous.is_none_or(is_numeric_token) => None,
            _ => STREET_TYPE_ABBREVIATIONS
                .iter()
                .find(|(abbreviation, _)| *abbreviation == token)
                .map(|(_, word)| *word)
                .or_else(|| {
                    last_word
                        .is_some_and(is_street_type)
                        .then(|| direction_word(token))
                        .flatten()
                }),
        };
        let word = replacement.unwrap_or(token);
        if is_street_type(word)
            && let Some(ordinal) = words.last().and_then(|last| ordinal_number(last))
        {
            words.pop();
            let tens = (ordinal < 10)
                .then(|| words.last().and_then(|last| tens_number(last)))
                .flatten();
            if tens.is_some() {
                words.pop();
            }
            words.push(numbered_ordinal(tens.unwrap_or(0) + ordinal).into());
        }
        words.push(word.into());
        previous = Some(token);
        last_word = Some(word);
    }
    words.join(" ")
}

fn ordinal_number(word: &str) -> Option<u32> {
    Some(match word {
        "first" => 1,
        "second" => 2,
        "third" => 3,
        "fourth" => 4,
        "fifth" => 5,
        "sixth" => 6,
        "seventh" => 7,
        "eighth" => 8,
        "ninth" => 9,
        "tenth" => 10,
        "eleventh" => 11,
        "twelfth" => 12,
        "thirteenth" => 13,
        "fourteenth" => 14,
        "fifteenth" => 15,
        "sixteenth" => 16,
        "seventeenth" => 17,
        "eighteenth" => 18,
        "nineteenth" => 19,
        "twentieth" => 20,
        "thirtieth" => 30,
        "fortieth" => 40,
        "fiftieth" => 50,
        "sixtieth" => 60,
        "seventieth" => 70,
        "eightieth" => 80,
        "ninetieth" => 90,
        _ => return None,
    })
}

fn tens_number(word: &str) -> Option<u32> {
    Some(match word {
        "twenty" => 20,
        "thirty" => 30,
        "forty" => 40,
        "fifty" => 50,
        "sixty" => 60,
        "seventy" => 70,
        "eighty" => 80,
        "ninety" => 90,
        _ => return None,
    })
}

fn numbered_ordinal(number: u32) -> String {
    let suffix = match (number % 100, number % 10) {
        (11..=13, _) => "th",
        (_, 1) => "st",
        (_, 2) => "nd",
        (_, 3) => "rd",
        _ => "th",
    };
    format!("{number}{suffix}")
}

pub(crate) fn street_search_text(value: &str) -> Option<String> {
    let text = search_text(value)?;
    let mut street = String::with_capacity(text.len() + 4);
    let mut words = text.split_whitespace().peekable();
    while let Some(word) = words.next() {
        if !street.is_empty() {
            street.push(' ');
        }
        let last = words.peek().is_none();
        street.push_str(last.then(|| direction_word(word)).flatten().unwrap_or(word));
    }
    Some(street)
}

pub(crate) fn expand_final_direction(words: &mut [String]) {
    if let Some(last) = words.last_mut()
        && let Some(direction) = direction_word(last)
    {
        *last = direction.to_string();
    }
}

pub(crate) fn direction_word(abbreviation: &str) -> Option<&'static str> {
    Some(match abbreviation {
        "e" => "east",
        "n" => "north",
        "s" => "south",
        "w" => "west",
        "ne" => "northeast",
        "nw" => "northwest",
        "se" => "southeast",
        "sw" => "southwest",
        _ => return None,
    })
}

pub(crate) fn is_street_type(word: &str) -> bool {
    matches!(
        word,
        "avenue"
            | "boulevard"
            | "circle"
            | "close"
            | "court"
            | "crescent"
            | "crossing"
            | "drive"
            | "esplanade"
            | "gardens"
            | "gate"
            | "grove"
            | "heights"
            | "highway"
            | "hill"
            | "lane"
            | "line"
            | "mews"
            | "parkway"
            | "path"
            | "place"
            | "promenade"
            | "quay"
            | "ridge"
            | "road"
            | "row"
            | "square"
            | "street"
            | "terrace"
            | "trail"
            | "walk"
            | "way"
    )
}

pub(crate) fn is_numeric_token(token: &str) -> bool {
    token.chars().all(|character| character.is_ascii_digit())
}

pub(crate) fn address_number_terms(number: &str) -> impl Iterator<Item = String> + '_ {
    number.split(';').filter_map(search_key)
}

pub(crate) fn flat_number_term(number: &str, unit: Option<&str>) -> Option<String> {
    let unit = search_key(unit?)?;
    if unit.contains(' ') {
        return None;
    }
    Some(format!("{unit} {}", search_key(number)?))
}

pub(crate) fn base_house_number(number: &str) -> Option<u32> {
    let number = number.trim_start();
    let digits = number
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(number.len());
    number[..digits].parse().ok()
}

pub(crate) fn compact_postcode(value: &str) -> Option<String> {
    search_key(value.trim()).map(|value| value.split_whitespace().collect())
}

#[derive(Default)]
pub struct PostcodeAreas {
    centroids: RTree<GeomWithData<[f64; 3], usize>>,
    postcodes: Vec<(String, [f64; 2])>,
}

impl fmt::Debug for PostcodeAreas {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PostcodeAreas")
            .field("postcodes", &self.postcodes.len())
            .finish()
    }
}

impl PostcodeAreas {
    pub fn new(centroids: impl IntoIterator<Item = (String, [f64; 2])>) -> Self {
        let postcodes = centroids
            .into_iter()
            .filter_map(|(postcode, point)| Some((compact_postcode(&postcode)?, point)))
            .collect::<Vec<_>>();
        let centroids = RTree::bulk_load(
            postcodes
                .iter()
                .enumerate()
                .map(|(index, (_, [lon, lat]))| GeomWithData::new(unit_vector(*lon, *lat), index))
                .collect(),
        );
        Self {
            centroids,
            postcodes,
        }
    }

    /// The nearest postcode within [`POSTCODE_AREA_RADIUS_M`] of a
    /// `[lon, lat]` point, compacted. Tied centroids must agree on the code.
    pub fn nearest(&self, [lon, lat]: [f64; 2]) -> Option<&str> {
        let mut neighbors = self
            .centroids
            .nearest_neighbor_iter_with_distance_2(unit_vector(lon, lat));
        let (nearest, distance) = neighbors.next()?;
        let (postcode, [centroid_lon, centroid_lat]) = &self.postcodes[nearest.data];
        let inside_radius =
            haversine_m(lon, lat, *centroid_lon, *centroid_lat) <= POSTCODE_AREA_RADIUS_M;
        let tie_limit = (distance.sqrt() + 64.0 * f64::EPSILON).powi(2);
        (inside_radius
            && !neighbors
                .take_while(|(_, other_distance)| *other_distance <= tie_limit)
                .any(|(other, _)| self.postcodes[other.data].0 != *postcode))
        .then_some(postcode.as_str())
    }
}

fn unit_vector(lon: f64, lat: f64) -> [f64; 3] {
    let (lon, lat) = (lon.to_radians(), lat.to_radians());
    [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
}

fn search_text(value: &str) -> Option<String> {
    search_key(value).map(|text| expand_address_abbreviations(&text))
}

fn unique_words(value: &str) -> Option<String> {
    let normalized = search_key(value)?;
    let mut seen = BTreeSet::new();
    let mut words = String::with_capacity(normalized.len());
    for word in normalized.split_whitespace() {
        if seen.insert(word) {
            if !words.is_empty() {
                words.push(' ');
            }
            words.push_str(word);
        }
    }
    Some(words)
}

fn context_text(names: &[&str]) -> String {
    const SLOTS: usize = 1 << 10;
    type Slot = Option<(Vec<String>, String)>;
    thread_local! {
        static RECENT: std::cell::RefCell<Vec<Slot>> =
            std::cell::RefCell::new(vec![None; SLOTS]);
    }
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    for name in names {
        hasher.update(name.as_bytes());
        hasher.update(&[0]);
    }
    let slot = hasher.digest() as usize % SLOTS;
    let made = RECENT.with(|recent| {
        recent.borrow()[slot]
            .as_ref()
            .filter(|(key, _)| key.iter().map(String::as_str).eq(names.iter().copied()))
            .map(|(_, text)| text.clone())
    });
    if let Some(text) = made {
        return text;
    }
    let text = unique_words(
        &names
            .iter()
            .copied()
            .filter_map(search_text)
            .collect::<Vec<_>>()
            .join(" "),
    )
    .unwrap_or_default();
    RECENT.with(|recent| {
        recent.borrow_mut()[slot] = Some((
            names.iter().map(|name| (*name).to_string()).collect(),
            text.clone(),
        ));
    });
    text
}

fn unique_parts(parts: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    parts
        .into_iter()
        .filter(|part| seen.insert(part.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::record::{
        AddressRecord, DerivedSourceProvenance, InterpolationRange, LocationPrecision, ObjectType,
        PlaceLayer, PostcodeRecord, SourceProvenance, point_geometry,
    };

    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn documents_run_in_chunks_of_neighbours_in_one_segment() {
        let dir = TempDir::new("text-parts");
        let chunk = 64;
        let count = 2 * chunk + 5;
        let mut writer = crate::pack::Writer::create(&dir).expect("writer");
        writer.text.chunk_documents = chunk;
        for index in 0..count {
            let record = AddressRecord {
                building: None,
                access: Vec::new(),
                disputed: false,
                street_postcodes: Vec::new(),
                address: AddressComponents {
                    block_number_len: None,
                    areas: Vec::new(),
                    number: "1".to_string(),
                    street: Some(format!("Street {index}")),
                    place: None,
                    unit: None,
                    locality: None,
                    region: None,
                    postcode: None,
                    country: None,
                },
                geometry: point_geometry(index as f64 * 1e-4, 0.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, index as i64 + 1),
            };
            writer.write(&record.into(), None).expect("address");
        }
        writer.finish().expect("finish");
        let pack = crate::pack::Reader::open(&dir).expect("pack");
        let searcher = open_text_index(pack.container())
            .expect("text index")
            .reader()
            .expect("reader")
            .searcher();
        let store_bytes = pack
            .section_sizes()
            .into_iter()
            .filter(|(name, _)| name.ends_with(STORE_EXTENSION))
            .map(|(_, bytes)| bytes)
            .sum::<u64>();
        let segments = searcher.segment_readers();
        assert!(
            store_bytes < 1_024,
            "{count} documents keep no store of their own: {store_bytes} bytes"
        );
        assert_eq!(segments.len(), 1, "the parts are joined into one segment");
        let ids = segments[0]
            .fast_fields()
            .u64("record_id")
            .expect("record ids");
        let records = (0..segments[0].max_doc())
            .map(|doc| ids.first(doc).expect("record id"))
            .collect::<Vec<_>>();
        let mut sorted = records.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..count).collect::<Vec<_>>());
        let run_starts = records
            .windows(2)
            .filter(|pair| pair[1] != pair[0] + 1)
            .count();
        assert!(
            run_starts < (count / chunk) as usize + 1,
            "documents run in record order but where a part's chunk ends: {run_starts}"
        );
    }

    #[test]
    fn projects_address_fields_for_search() {
        let record = AddressRecord {
            building: None,
            access: Vec::new(),
            disputed: false,
            street_postcodes: Vec::new(),
            address: AddressComponents {
                block_number_len: None,
                areas: Vec::new(),
                number: "221B".to_string(),
                street: Some("Baker Street".to_string()),
                place: None,
                unit: None,
                locality: Some("London".to_string()),
                region: None,
                postcode: Some("NW1".to_string()),
                country: Some("GB".to_string()),
            },
            geometry: point_geometry(-0.1586, 51.5237),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, 123),
        };

        let projected = Document::from_record(
            42,
            &record.into(),
            &["Westminster"],
            &PostcodeAreas::default(),
            None,
        );

        assert_eq!(projected.record_id, 42);
        assert_eq!(projected.layers, vec!["address"]);
        assert_eq!(
            projected.context_text, "london nw1 gb westminster",
            "addresses are indexed under what they state and the areas they lie in"
        );
        for area in ["london", "nw1", "gb", "westminster"] {
            assert!(!projected.content_text.contains(area), "{area}");
        }
        assert!(!projected.autocomplete_subject_text.contains("westminster"));
        assert_eq!(projected.address_numbers, vec!["221B"]);
        assert_eq!(
            projected.name.as_deref(),
            Some("baker street"),
            "the number is kept apart, so the street's addresses share a document"
        );
        assert_eq!(projected.content_text, "baker street");
        assert_eq!(projected.autocomplete_subject_text, "baker street nw1");
        assert!(!projected.autocomplete_subject_text.contains("london"));
    }

    #[test]
    fn projects_interpolation_without_indexing_range_fields() {
        let record = crate::record::InterpolationRecord {
            address: InterpolationAddressComponents {
                street: Some("Baker Street".to_string()),
                place: None,
                locality: Some("London".to_string()),
                region: None,
                postcode: Some("NW1".to_string()),
                country: Some("GB".to_string()),
            },
            interpolation: InterpolationRange {
                kind: "odd".to_string(),
                start: 1,
                end: 99,
                step: 2,
            },
            anchor_node_ids: [1, 2],
            geometry: point_geometry(-0.1586, 51.5237),
            representative_point: [-0.1586, 51.5237],
            source: SourceProvenance::osm(ObjectType::Way, 9),
        };

        let projected =
            Document::from_record(7, &record.into(), &[], &PostcodeAreas::default(), None);

        assert!(projected.address_numbers.is_empty());
        assert_eq!(projected.postcode.as_deref(), Some("NW1"));
        assert_eq!(projected.content_text, "baker street");
        assert_eq!(projected.context_text, "london nw1 gb");
        assert!(projected.autocomplete_subject_text.is_empty());
    }

    #[test]
    fn projects_postcodes_and_place_layers() {
        let postcode = PostcodeRecord {
            postcode: "M5V".to_string(),
            country: None,
            geometry: point_geometry(-79.4, 43.6),
            source: DerivedSourceProvenance::osm_address_records(2),
        };
        let place = PlaceRecord {
            alt_names: Vec::new(),
            name: "Toronto".to_string(),
            place_type: "city".to_string(),
            geometry: point_geometry(-79.4, 43.6),
            source: SourceProvenance {
                dataset: "osm".to_string(),
                object_type: ObjectType::Node,
                object_id: 1,
                tags: Some(BTreeMap::new()),
            },
            prominence: Default::default(),
            wikidata: None,
        };

        let postcode =
            Document::from_record(1, &postcode.into(), &[], &PostcodeAreas::default(), None);
        let place = Document::from_record(
            2,
            &Record::Place(PlaceLayer::Locality, place),
            &[],
            &PostcodeAreas::default(),
            None,
        );

        assert_eq!(postcode.postcode.as_deref(), Some("M5V"));
        assert_eq!(place.layers, vec!["locality"]);
        assert!(place.content_text.contains("toronto"));
        assert_eq!(postcode.autocomplete_subject_text, "m5v");
        assert_eq!(place.autocomplete_subject_text, "toronto");
    }

    #[test]
    fn projects_poi_name_category_address_and_context() {
        let poi = PoiRecord {
            alt_names: Vec::new(),
            name: "Tim Hortons".to_string(),
            category: "amenity:cafe".to_string(),
            address: Some(AddressComponents {
                block_number_len: None,
                areas: Vec::new(),
                number: "123".to_string(),
                street: Some("King Street West".to_string()),
                place: None,
                unit: None,
                locality: None,
                region: None,
                postcode: Some("M5V 1A1".to_string()),
                country: None,
            }),
            geometry: point_geometry(-79.38, 43.65),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, 5),
            prominence: Default::default(),
            wikidata: None,
            brand: None,
            details: None,
            aliases: Vec::new(),
        };

        let projected = Document::from_record(
            3,
            &poi.clone().into(),
            &["Toronto", "Ontario"],
            &PostcodeAreas::default(),
            None,
        );

        assert_eq!(projected.layers, vec!["poi"]);
        assert_eq!(
            projected.name.as_deref(),
            Some("tim hortons"),
            "the address is its street's, not the name's"
        );
        assert_eq!(projected.streets, vec!["king street west"]);
        assert_eq!(projected.address_numbers, vec!["123"]);
        assert_eq!(projected.rank_postcodes, vec!["m5v1a1"]);
        assert_eq!(projected.area_postcode, None);
        for text in ["tim hortons", "amenity cafe", "king street west"] {
            assert!(projected.content_text.contains(text), "{text}");
        }
        assert_eq!(projected.context_text, "m5v 1a1 m5v1a1 toronto ontario");
        assert!(
            projected
                .autocomplete_subject_text
                .starts_with("tim hortons")
        );
        assert!(!projected.autocomplete_subject_text.contains("toronto"));

        let plain = PoiRecord {
            name: "Walmer Road Parkette".to_string(),
            address: Some(AddressComponents {
                block_number_len: None,
                areas: Vec::new(),
                number: "227".to_string(),
                street: Some("Walmer Road".to_string()),
                place: None,
                unit: None,
                locality: Some("Toronto".to_string()),
                region: None,
                postcode: None,
                country: None,
            }),
            ..poi
        };
        let areas = PostcodeAreas::new([
            ("M5R 2Z3".to_string(), [-79.38, 43.659]),
            ("K0J 1K0".to_string(), [-79.38, 43.8]),
        ]);
        let projected = Document::from_record(4, &plain.clone().into(), &["Toronto"], &areas, None);
        assert_eq!(projected.name.as_deref(), Some("walmer road parkette"));
        assert_eq!(projected.streets, vec!["walmer road"]);
        assert!(projected.rank_postcodes.is_empty());
        assert_eq!(
            projected.area_postcode.as_deref(),
            Some("m5r2z3"),
            "the nearest postcode stands in for the one it does not state"
        );
        let tied = PostcodeAreas::new([
            ("M5R 2Z3".to_string(), [-79.38, 43.659]),
            ("M5V 1A1".to_string(), [-79.38, 43.659]),
        ]);
        let ambiguous = Document::from_record(4, &plain.clone().into(), &["Toronto"], &tied, None);
        assert_eq!(ambiguous.area_postcode, None);
        for (text, word) in [
            (&projected.content_text, "walmer"),
            (&projected.content_text, "road"),
            (&projected.context_text, "toronto"),
        ] {
            assert_eq!(text.split(' ').filter(|w| *w == word).count(), 1, "{word}");
        }

        let unaddressed = PoiRecord {
            name: "Riverdale Farm".to_string(),
            address: None,
            ..plain
        };
        let projected =
            Document::from_record(5, &unaddressed.into(), &[], &PostcodeAreas::default(), None);
        assert_eq!(projected.layers, vec!["poi"]);
        assert!(projected.address_numbers.is_empty());
        assert_eq!(projected.area_postcode, None, "no postcode data nearby");
    }

    #[test]
    fn postcode_areas_give_the_nearest_postcode_within_the_radius() {
        let areas = PostcodeAreas::new([
            ("M5V 1A1".to_string(), [-79.40, 43.64]),
            ("M5C 2A1".to_string(), [-79.377, 43.651]),
        ]);
        assert_eq!(areas.nearest([-79.378, 43.650]), Some("m5c2a1"));
        assert_eq!(areas.nearest([-79.399, 43.641]), Some("m5v1a1"));
        assert_eq!(areas.nearest([-79.4, 44.0]), None, "40 km away");
        assert_eq!(PostcodeAreas::default().nearest([-79.4, 43.6]), None);
    }

    #[test]
    fn postcode_areas_skip_ties_between_distinct_postcodes() {
        let query = [0.0, 0.0];
        for (left, right) in [(query, query), ([-0.01, 0.0], [0.01, 0.0])] {
            let centroids = [
                ("M5V 1A1".to_string(), left),
                ("M5C 2A1".to_string(), right),
                ("M5R 2Z3".to_string(), [0.02, 0.0]),
            ];
            assert_eq!(PostcodeAreas::new(centroids.clone()).nearest(query), None);
            assert_eq!(
                PostcodeAreas::new(centroids.into_iter().rev()).nearest(query),
                None
            );
        }
    }

    #[test]
    fn postcode_areas_keep_a_code_shared_by_tied_centroids() {
        let areas = PostcodeAreas::new([
            ("M5V 1A1".to_string(), [-0.01, 0.0]),
            ("m5v1a1".to_string(), [0.01, 0.0]),
            ("M5R 2Z3".to_string(), [0.02, 0.0]),
        ]);
        assert_eq!(areas.nearest([0.0, 0.0]), Some("m5v1a1"));
        assert_eq!(areas.nearest([0.006, 0.0]), Some("m5v1a1"));
    }

    #[test]
    fn postcode_areas_treat_rounded_midpoints_as_ties() {
        let query = [-79.375, 43.625];
        let west = ("M5V 1A1".to_string(), [-79.390625, 43.625]);
        let east = ("M5C 2A1".to_string(), [-79.359375, 43.625]);
        let areas = PostcodeAreas::new([west.clone(), east.clone()]);
        assert_eq!(areas.nearest(query), None);
        assert_eq!(PostcodeAreas::new([east, west]).nearest(query), None);
        assert_eq!(areas.nearest([-79.375_000_01, 43.625]), Some("m5v1a1"));
    }

    #[test]
    fn indexes_streets_with_a_final_direction_expanded() {
        assert_eq!(
            street_search_text("The Donway E").as_deref(),
            Some("the donway east")
        );
        assert_eq!(
            street_search_text("King St W").as_deref(),
            Some("king street west")
        );
        assert_eq!(search_text("Plan E").as_deref(), Some("plan e"));
    }

    #[test]
    fn writes_an_ordinal_before_a_street_type_as_its_number() {
        for (text, indexed) in [
            ("Fifth Avenue", "5th avenue"),
            ("5th Avenue", "5th avenue"),
            ("Second Ave N", "2nd avenue north"),
            ("West Twentieth St", "west 20th street"),
            ("Twenty-First Street", "21st street"),
            ("Thirty Third Ave", "33rd avenue"),
            ("Twenty Street", "twenty street"),
            ("Eleventh Avenue", "11th avenue"),
            ("First Baptist Church", "first baptist church"),
            ("Avenue Fifth", "avenue fifth"),
            ("Fifth", "fifth"),
        ] {
            assert_eq!(search_text(text).as_deref(), Some(indexed), "{text}");
        }
    }

    #[test]
    fn base_house_numbers_are_the_leading_digits() {
        for (number, base) in [
            ("407", Some(407)),
            ("407A", Some(407)),
            ("993.5", Some(993)),
            ("1384 1/2", Some(1384)),
            ("12;14", Some(12)),
            ("A12", None),
        ] {
            assert_eq!(base_house_number(number), base, "{number}");
        }
    }
}
