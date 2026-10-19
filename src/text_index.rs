//! Tantivy text index.
//!
//! The index is built in the scratch directory, merged to one segment, and its
//! files are copied into the Pack as `text/<file>` sections. At runtime a
//! read-only [`Directory`] serves those sections straight from the Pack
//! mapping, so the text index needs no files of its own.

use std::{
    collections::{BTreeSet, HashMap},
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::error::{Error, Result};
use rstar::{RTree, primitives::GeomWithData};
use tantivy::{
    Index, IndexWriter, TantivyDocument,
    directory::{
        Directory, FileHandle, RamDirectory, WatchCallback, WatchHandle, WritePtr,
        error::{DeleteError, OpenReadError, OpenWriteError},
    },
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
        Layer, PlaceRecord, PoiRecord, PostcodeRecord, Record, StreetRecord,
    },
    spatial_index::haversine_m,
    util::text::collapse_whitespace,
};

pub const TEXT_SECTION_PREFIX: &str = "text/";
pub const TEXT_INDEX_SCHEMA_VERSION: u32 = 10;
/// Postcode data within this radius describes where a record is: a record
/// that states no postcode is ranked by the nearest postcode centroid within
/// it, and the batch check compares the postcode areas within it.
pub(crate) const POSTCODE_AREA_RADIUS_M: f64 = 10_000.0;

/// Tantivy needs at least 15 MB per indexing thread (it uses fewer threads
/// when given less) and gains little beyond 1.6 GB.
const MIN_INDEX_MEMORY_BYTES: usize = 16 << 20;
const MAX_INDEX_MEMORY_BYTES: usize = 1_600_000_000;
/// Memory each indexing thread gets at least: less makes many small segments
/// for the final merge to join.
const INDEX_THREAD_MEMORY_BYTES: usize = 32 << 20;
/// Indexing threads at most. Tantivy's default is 8, which leaves the cores of
/// a larger machine idle while every record waits on the index.
const MAX_INDEX_THREADS: usize = 16;
/// Segments the finished text index keeps, one per merge run in parallel
/// (Tantivy merges on four threads).
const MERGED_SEGMENTS: usize = 4;
const AUTOCOMPLETE_SUBJECT_FIELD: &str = "autocomplete_subject_text";
pub(crate) const INTERPOLATION_START_FIELD: &str = "interpolation_start";
const INTERPOLATION_END_FIELD: &str = "interpolation_end";
pub(crate) const HOUSE_NUMBER_FIELD: &str = "house_number";
pub(crate) const RANK_POSTCODE_FIELD: &str = "rank_postcode";
pub(crate) const COUNTRY_CODE_FIELD: &str = "country_code";
pub(crate) const RECORD_COUNT_FIELD: &str = "record_count";
/// Addresses one document holds at most, so finding a number in a group reads
/// a bounded run of records.
const MAX_GROUP_RECORDS: u64 = 1_024;

pub struct Writer {
    /// Created with the first document, so its buffers are reserved only for
    /// the phase that writes records.
    writer: Option<(IndexWriter, Reservation)>,
    budget: Arc<Budget>,
    memory_bytes: usize,
    index: Index,
    path: PathBuf,
    fields: Fields,
    document_count: u64,
    /// The address group the next address may join.
    group: Option<Document>,
}

/// What a record adds to the text index: a document of its own, or, for an
/// address, its share of the document of its street group.
#[derive(Debug)]
pub enum Prepared {
    Document(TantivyDocument),
    Address(Box<Document>),
}

#[derive(Debug, Clone, Copy)]
pub struct Fields {
    pub record_id: Field,
    /// How many records from `record_id` on the document stands for: the
    /// addresses of one street group, else 1.
    pub record_count: Field,
    pub layer: Field,
    /// What a record is: its name, street or place, house number, unit and
    /// category.
    pub content_text: Field,
    /// Where a record is: the locality, region, country and postcode it
    /// states, and the names of the admin areas it lies in.
    pub context_text: Field,
    pub name_text: Field,
    /// The street (or `addr:place`) an address, a POI's address or a range
    /// is on, and a street's own name: what the words beside a house number
    /// name.
    pub street_text: Field,
    pub address_number: Field,
    pub postcode_exact: Field,
    pub autocomplete_subject_text: Field,
    /// First and last number of an interpolation range, so a search finds the
    /// ranges that contain a requested house number.
    pub interpolation_start: Field,
    pub interpolation_end: Field,
    /// The smallest number a stated house number starts with ("407" of
    /// "407A"). Ranking asks only whether a record states one, so a street
    /// group keeps one value, not one per address.
    pub house_number: Field,
    /// The postcode a record is ranked by: the one it states, else the
    /// nearest postcode centroid (see [`PostcodeAreas`]).
    pub rank_postcode: Field,
    /// The ISO 3166-1 alpha-2 code of the country the record lies in, upper
    /// case, so a Pack covering several countries can be searched one at a
    /// time. Exact, like `layer`: it is filtered on, never matched as text.
    pub country_code: Field,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commit {
    pub document_count: u64,
    pub bytes: u64,
}

/// The text document of one record, or of the run of addresses a group
/// document holds: consecutive addresses whose documents differ only in their
/// numbers and units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub record_id: RecordId,
    pub record_count: u64,
    /// Layers a `layer` filter finds the record under: its own, and `address`
    /// for a POI that carries one.
    pub layers: Vec<String>,
    pub content_text: String,
    pub context_text: String,
    pub name: Option<String>,
    /// The names of the street (see [`Fields::street_text`]): a
    /// street's every name, an address's one.
    pub streets: Vec<String>,
    /// The house numbers of the record, or of each address of a group.
    pub address_numbers: Vec<String>,
    /// Flats as their units and numbers are written together, "10 1367":
    /// found as written, but no house number.
    pub flat_numbers: Vec<String>,
    /// The units of a group's addresses, searched with what it is.
    pub units: Vec<String>,
    pub postcode: Option<String>,
    pub autocomplete_subject_text: String,
    /// First and last number of an interpolation range.
    pub interpolation_range: Option<(u32, u32)>,
    /// Normalized without spaces, as [`compact_postcode`] gives.
    pub rank_postcode: Option<String>,
    /// The country the record lies in, from its admin context.
    pub country_code: Option<String>,
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
        let path = scratch.path().join("text");
        fs::create_dir_all(&path).map_err(|source| Error::IoPath {
            action: "failed to create",
            path: path.clone(),
            source,
        })?;
        let index = Index::create_in_dir(&path, schema)?;
        Ok(Self {
            writer: None,
            budget: Arc::clone(budget),
            memory_bytes: memory_bytes.clamp(MIN_INDEX_MEMORY_BYTES, MAX_INDEX_MEMORY_BYTES),
            index,
            path,
            fields,
            document_count: 0,
            group: None,
        })
    }

    pub fn fields(&self) -> Fields {
        self.fields
    }

    fn writer(&mut self) -> Result<&mut IndexWriter> {
        if self.writer.is_none() {
            // Reserve before Tantivy allocates; the share was sized within the
            // budget, so this only overshoots if a caller asked for more.
            let mut memory = Reservation::empty(&self.budget);
            if !memory.try_grow(self.memory_bytes) {
                memory.force_grow(self.memory_bytes);
            }
            // As many as the build's worker pool, which is sized to the
            // CPUs and memory the process can use.
            let threads = rayon::current_num_threads()
                .min(MAX_INDEX_THREADS)
                .min(self.memory_bytes / INDEX_THREAD_MEMORY_BYTES)
                .max(1);
            let writer = self
                .index
                .writer_with_num_threads(threads, self.memory_bytes)?;
            // Disable background auto-merges during the build so the only
            // merge is the single deterministic one forced in `finish()`.
            writer.set_merge_policy(Box::new(NoMergePolicy));
            self.writer = Some((writer, memory));
        }
        Ok(&mut self.writer.as_mut().expect("writer created").0)
    }

    /// Index one document. Tantivy's indexing queue holds a bounded number of
    /// messages, so handing it documents one at a time bounds the documents
    /// waiting in memory: when the indexer falls behind, this blocks instead
    /// of queuing every record of the build (a batch of documents would be
    /// one message, and the queue would hold thousands of batches).
    pub fn add(&mut self, document: TantivyDocument) -> Result<()> {
        self.close_group()?;
        self.add_document(document)
    }

    fn add_document(&mut self, document: TantivyDocument) -> Result<()> {
        self.writer()?.add_document(document)?;
        self.document_count += 1;
        Ok(())
    }

    /// Index the address `document` projects: into the group of the address
    /// before it when it is the next record and states the same, else into a
    /// group of its own.
    pub fn add_address(&mut self, document: Box<Document>) -> Result<()> {
        if let Some(group) = &mut self.group
            && group.record_id + group.record_count == document.record_id
            && group.record_count < MAX_GROUP_RECORDS
            && group.same_group(&document)
        {
            group.record_count += 1;
            group.address_numbers.extend(document.address_numbers);
            group.flat_numbers.extend(document.flat_numbers);
            group.units.extend(document.units);
            return Ok(());
        }
        self.close_group()?;
        self.group = Some(*document);
        Ok(())
    }

    fn close_group(&mut self) -> Result<()> {
        if let Some(group) = self.group.take() {
            let document = self.fields.to_tantivy_document(&group);
            self.add_document(document)?;
        }
        Ok(())
    }

    /// Commit, merge to a single segment and copy the index into the Pack.
    pub fn finish(mut self, pack: &mut container::Writer) -> Result<Commit> {
        self.close_group()?;
        self.writer()?;
        let (mut writer, memory) = self.writer.take().expect("writer created");
        writer.commit()?;
        // Join the segments the indexing threads wrote into a few, merged side
        // by side: one merge of every segment runs on one thread, and search
        // reads a few segments as well as one.
        let segment_ids = self.index.searchable_segment_ids()?;
        let groups = segment_ids.len().min(MERGED_SEGMENTS);
        if segment_ids.len() > groups {
            let mut merges = vec![Vec::new(); groups];
            for (index, segment) in segment_ids.into_iter().enumerate() {
                merges[index % groups].push(segment);
            }
            let pending = merges
                .iter()
                .filter(|segments| segments.len() > 1)
                .map(|segments| writer.merge(segments))
                .collect::<Vec<_>>();
            for merge in pending {
                merge.wait()?;
            }
            writer.commit()?;
        }
        writer.garbage_collect_files().wait()?;
        writer.wait_merging_threads()?;
        drop(memory);

        let mut files = fs::read_dir(&self.path)?
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
            bytes += fs::metadata(&file)?.len();
            pack.add_file(
                &format!("{TEXT_SECTION_PREFIX}{name}"),
                TEXT_INDEX_SCHEMA_VERSION,
                &file,
            )?;
        }
        Ok(Commit {
            document_count: self.document_count,
            bytes,
        })
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
            interpolation_start: schema.get_field(INTERPOLATION_START_FIELD)?,
            interpolation_end: schema.get_field(INTERPOLATION_END_FIELD)?,
            street_text: schema.get_field("street_text")?,
            house_number: schema.get_field(HOUSE_NUMBER_FIELD)?,
            rank_postcode: schema.get_field(RANK_POSTCODE_FIELD)?,
            country_code: schema.get_field(COUNTRY_CODE_FIELD)?,
        })
    }

    /// What a record adds to the text index. Pure, so callers can prepare
    /// records in parallel before handing them to the writer.
    /// `context_names` are the admin areas the record lies in.
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
        match record {
            Record::Address(_) => Prepared::Address(Box::new(document)),
            _ => Prepared::Document(self.to_tantivy_document(&document)),
        }
    }

    fn to_tantivy_document(self, projected: &Document) -> TantivyDocument {
        let mut document = TantivyDocument::default();
        document.add_u64(self.record_id, projected.record_id);
        document.add_u64(self.record_count, projected.record_count);
        for layer in &projected.layers {
            document.add_text(self.layer, layer);
        }
        let units = unique_parts(
            projected
                .units
                .iter()
                .filter_map(|unit| search_text(unit))
                .collect(),
        );
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
        for street in &projected.streets {
            add_text_if_present(&mut document, self.street_text, Some(street));
        }
        let mut terms = BTreeSet::new();
        let mut lowest: Option<u32> = None;
        for number in &projected.address_numbers {
            terms.extend(address_number_terms(number));
            if let Some(base) = base_house_number(number) {
                lowest = Some(lowest.map_or(base, |lowest| lowest.min(base)));
            }
        }
        terms.extend(projected.flat_numbers.iter().cloned());
        for term in &terms {
            document.add_text(self.address_number, term);
        }
        if let Some(base) = lowest {
            document.add_u64(self.house_number, base.into());
        }
        add_normalized_text_if_present(
            &mut document,
            self.postcode_exact,
            projected.postcode.as_deref(),
        );
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
        if let Some(postcode) = &projected.rank_postcode {
            document.add_text(self.rank_postcode, postcode);
        }
        if let Some(country) = &projected.country_code {
            document.add_text(self.country_code, country);
        }
        document
    }
}

impl Document {
    /// Whether `other` belongs to this document's group: it states the same
    /// in every field but its numbers and units.
    fn same_group(&self, other: &Self) -> bool {
        self.layers == other.layers
            && self.content_text == other.content_text
            && self.context_text == other.context_text
            && self.name == other.name
            && self.streets == other.streets
            && self.postcode == other.postcode
            && self.autocomplete_subject_text == other.autocomplete_subject_text
            && self.interpolation_range == other.interpolation_range
            && self.rank_postcode == other.rank_postcode
            && self.country_code == other.country_code
    }

    /// What a record is and where it is go to separate fields, so a word that
    /// names what is sought is not matched by an area's name ("Kingston Road"
    /// by an address in Kingston, "Danforth Avenue" by one in the Danforth
    /// neighbourhood). Where it is holds the locality, region, country and
    /// postcode the record states and `context_names`, the admin areas it lies
    /// in: a POI name alone rarely identifies one place ("Tim Hortons"), and
    /// most addresses carry no `addr:city`.
    ///
    /// Every record but a place is ranked by the postcode it states, or else
    /// by the nearest of `postcode_areas`.
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
        document.rank_postcode = match record {
            Record::Place(..) => None,
            _ => match document.postcode.as_deref() {
                Some(postcode) => compact_postcode(postcode),
                None => record
                    .display_point()
                    .and_then(|point| postcode_areas.nearest(point))
                    .map(str::to_string),
            },
        };
        if let Record::Poi(_) = record {
            // A POI repeats words across its name and address ("Walmer Road
            // Parkette, 227 Walmer Road"). Without field norms every repeat
            // would add to its score, so each word counts once: a POI must not
            // outrank the address or place a query names on repetition alone.
            document.name = document.name.as_deref().and_then(unique_words);
            document.content_text = unique_words(&document.content_text).unwrap_or_default();
        }
        document
    }

    fn project_address(record_id: RecordId, address: &AddressRecord) -> ProjectionBuilder {
        // Named by its street alone, and its number and unit kept apart, so
        // the addresses of one street project alike and share a group.
        let mut builder = ProjectionBuilder::new(record_id, "address");
        if let Some(subject) = address.address.subject() {
            builder.name_for_search(subject);
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
        if let Some(address) = &poi.address {
            builder.also_layer(Layer::Address.as_str());
            builder.address(address);
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

/// Read-only Tantivy directory over the `text/` sections of a Pack. Writes the
/// reader needs (lock files) go to a private in-memory directory.
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
                return Err(Error::UnsupportedVersion {
                    found: info.version,
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
    let interpolation_start = builder.add_u64_field(INTERPOLATION_START_FIELD, FAST);
    let interpolation_end = builder.add_u64_field(INTERPOLATION_END_FIELD, FAST);
    let street_text = builder.add_text_field("street_text", searchable_text_options());
    let house_number = builder.add_u64_field(HOUSE_NUMBER_FIELD, FAST);
    let rank_postcode = builder.add_text_field(
        RANK_POSTCODE_FIELD,
        TextOptions::default().set_fast(Some("raw")),
    );
    let country_code = builder.add_text_field(COUNTRY_CODE_FIELD, exact_string_options());
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
        interpolation_start,
        interpolation_end,
        street_text,
        house_number,
        rank_postcode,
        country_code,
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
    /// Where the record says it is, one part per stated value.
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
                rank_postcode: None,
                country_code: None,
            },
            name_parts: Vec::new(),
            content_parts: Vec::new(),
            context_parts: Vec::new(),
            autocomplete_subject_parts: Vec::new(),
        }
    }

    fn also_layer(&mut self, layer: &str) {
        self.projected.layers.push(layer.to_string());
    }

    fn name(&mut self, value: &str) {
        self.name_for_search(value);
        self.add_autocomplete_subject_text(value);
    }

    fn name_for_search(&mut self, value: &str) {
        if let Some(text) = search_text(value) {
            self.name_parts.push(text);
        }
        self.add_content_text(value);
    }

    /// A name of the street or place a house number is on, spelled as
    /// [`street_search_text`] gives.
    fn street(&mut self, value: Option<&str>) {
        if let Some(street) = value.and_then(street_search_text)
            && !self.projected.streets.contains(&street)
        {
            self.projected.streets.push(street);
        }
    }

    /// A POI's address: its number and unit are part of what it is.
    fn address(&mut self, address: &AddressComponents) {
        self.add_content_text(&address.number);
        self.address_parts(address);
        self.add_optional_content_text(address.unit.as_deref());
    }

    /// Every part of an address but its unit; the number only as a house
    /// number.
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

    /// A postcode record: the postcode is what it is.
    fn postcode(&mut self, value: &str) {
        self.projected.postcode = collapse_whitespace(value);
        self.add_content_text(value);
        if let Some(normalized) = normalize_index_text(value) {
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

    /// The postcode an address states: where it is, and a prefix to complete.
    fn add_optional_postcode_text(&mut self, value: Option<&str>) -> Option<String> {
        let cleaned = self.add_optional_postcode_for_search(value)?;
        if let Some(normalized) = normalize_index_text(&cleaned) {
            self.autocomplete_subject_parts
                .extend(postcode_forms(&normalized));
        }
        Some(cleaned)
    }

    fn add_optional_postcode_for_search(&mut self, value: Option<&str>) -> Option<String> {
        let cleaned = value.and_then(collapse_whitespace)?;
        if let Some(normalized) = normalize_index_text(&cleaned) {
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
        if let Some(normalized) = normalize_index_text(value) {
            self.autocomplete_subject_parts.push(normalized);
        }
    }

    /// The document, placed in the stated areas and in `context_names`. Each
    /// area word is indexed once, and each name is expanded on its own, as a
    /// query's comma-separated parts are ("St Catharines" keeps its "st").
    fn build(mut self, context_names: &[&str]) -> Document {
        let name_parts = unique_parts(self.name_parts);
        self.projected.name = (!name_parts.is_empty()).then(|| name_parts.join(" "));
        let content_parts = unique_parts(self.content_parts);
        self.projected.content_text = content_parts.join(" ");
        let context = self
            .context_parts
            .iter()
            .map(String::as_str)
            .chain(context_names.iter().copied())
            .filter_map(search_text)
            .collect::<Vec<_>>()
            .join(" ");
        self.projected.context_text = unique_words(&context).unwrap_or_default();
        let autocomplete_subject_parts = unique_parts(self.autocomplete_subject_parts);
        self.projected.autocomplete_subject_text = autocomplete_subject_parts.join(" ");
        self.projected
    }
}

/// A normalized postcode as written, and without its spaces when it has any:
/// "m5v 1a1" and "m5v1a1".
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
    if let Some(value) = value.and_then(normalize_index_text) {
        document.add_text(field, value);
    }
}

pub(crate) fn normalize_index_text(value: &str) -> Option<String> {
    let mut normalized = String::with_capacity(value.len());
    let mut previous_was_space = true;
    for character in value.chars() {
        if character.is_alphanumeric() {
            for folded in character.to_lowercase() {
                normalized.push(folded);
            }
            previous_was_space = false;
        } else if !previous_was_space {
            normalized.push(' ');
            previous_was_space = true;
        }
    }
    let normalized = normalized.trim().to_string();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// Street-type abbreviations and the words they stand for, in normalized
/// text. A word's first abbreviation here is its usual one.
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

/// Expand the common abbreviations of street types and of directions that
/// follow a street type, in normalized text: "king st w" becomes "king street
/// west". Indexed text and queries are expanded alike, so either spelling
/// finds the other. An initial or post-number "st" is left alone: there it is
/// usually "Saint" ("St Clair Avenue", "12 St George Street").
pub(crate) fn expand_address_abbreviations(value: &str) -> String {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    let mut expanded: Vec<&str> = Vec::with_capacity(tokens.len());
    for (index, token) in tokens.iter().enumerate() {
        let replacement = match *token {
            "st" if index == 0 || is_numeric_token(tokens[index - 1]) => None,
            _ => STREET_TYPE_ABBREVIATIONS
                .iter()
                .find(|(abbreviation, _)| abbreviation == token)
                .map(|(_, word)| *word)
                .or_else(|| {
                    expanded
                        .last()
                        .is_some_and(|word| is_street_type(word))
                        .then(|| direction_word(token))
                        .flatten()
                }),
        };
        expanded.push(replacement.unwrap_or(*token));
    }
    expanded.join(" ")
}

/// A street name as the street field indexes it: like [`search_text`], and a
/// direction abbreviation that ends it is expanded whatever word precedes it
/// ("The Donway E" is "the donway east"). A query's street is expanded alike
/// (see [`expand_final_direction`]).
pub(crate) fn street_search_text(value: &str) -> Option<String> {
    let mut words = search_text(value)?
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    expand_final_direction(&mut words);
    Some(words.join(" "))
}

/// Expand a direction abbreviation that is the last of a street's words.
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

/// Whether an expanded word is a street type ("street", "avenue").
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

/// The terms a house number is indexed under: each value of a list ("12;14")
/// is a number of its own, normalized ("407A" is "407a").
pub(crate) fn address_number_terms(number: &str) -> impl Iterator<Item = String> + '_ {
    number.split(';').filter_map(normalize_index_text)
}

/// The term a flat is also found under: its unit and number as a slash
/// number writes them, "10 1367" for unit 10 of 1367 ("10/1367"). Only a unit
/// of one word: "Flat 4" is written before no number.
pub(crate) fn flat_number_term(number: &str, unit: Option<&str>) -> Option<String> {
    let unit = normalize_index_text(unit?)?;
    if unit.contains(' ') {
        return None;
    }
    Some(format!("{unit} {}", normalize_index_text(number)?))
}

/// The digits a house number starts with: 407 of "407A", "407 1/2" and
/// "407;409". `None` when it does not start with one.
pub(crate) fn base_house_number(number: &str) -> Option<u32> {
    let number = number.trim_start();
    let digits = number
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(number.len());
    number[..digits].parse().ok()
}

/// A postcode normalized without spaces: "M5V 1A1" is "m5v1a1".
pub(crate) fn compact_postcode(value: &str) -> Option<String> {
    normalize_index_text(value.trim()).map(|value| value.split_whitespace().collect())
}

/// Postcode centroids, so a record that states no postcode can be ranked by
/// the one nearest it. Most addresses state none, and without this a query's
/// postcode could rank only the few that do.
#[derive(Default)]
pub struct PostcodeAreas {
    /// Centroids as unit vectors, where straight-line nearness is nearness
    /// on the sphere, each with its index in `postcodes`.
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
    /// `centroids` are postcodes and their `[lon, lat]` centroids.
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
    /// `[lon, lat]` point, compacted.
    pub fn nearest(&self, [lon, lat]: [f64; 2]) -> Option<&str> {
        let nearest = self.centroids.nearest_neighbor(&unit_vector(lon, lat))?;
        let (postcode, [centroid_lon, centroid_lat]) = &self.postcodes[nearest.data];
        (haversine_m(lon, lat, *centroid_lon, *centroid_lat) <= POSTCODE_AREA_RADIUS_M)
            .then_some(postcode.as_str())
    }
}

fn unit_vector(lon: f64, lat: f64) -> [f64; 3] {
    let (lon, lat) = (lon.to_radians(), lat.to_radians());
    [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
}

/// Text as the searchable fields index it: normalized, with address
/// abbreviations expanded.
fn search_text(value: &str) -> Option<String> {
    normalize_index_text(value).map(|text| expand_address_abbreviations(&text))
}

/// Normalized words of `value`, each once, in order of first use.
fn unique_words(value: &str) -> Option<String> {
    let normalized = normalize_index_text(value)?;
    let mut seen = BTreeSet::new();
    Some(
        normalized
            .split_whitespace()
            .filter(|word| seen.insert(*word))
            .collect::<Vec<_>>()
            .join(" "),
    )
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

    #[test]
    fn projects_address_fields_for_search() {
        let record = AddressRecord {
            address: AddressComponents {
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
        };

        let projected = Document::from_record(
            3,
            &poi.clone().into(),
            &["Toronto", "Ontario"],
            &PostcodeAreas::default(),
            None,
        );

        assert_eq!(projected.layers, vec!["poi", "address"]);
        assert_eq!(
            projected.name.as_deref(),
            Some("tim hortons"),
            "the address is its street's, not the name's"
        );
        assert_eq!(projected.streets, vec!["king street west"]);
        assert_eq!(projected.address_numbers, vec!["123"]);
        assert_eq!(projected.rank_postcode.as_deref(), Some("m5v1a1"));
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
        // A postcode centroid 1 km away, and one farther than the radius.
        let areas = PostcodeAreas::new([
            ("M5R 2Z3".to_string(), [-79.38, 43.659]),
            ("K0J 1K0".to_string(), [-79.38, 43.8]),
        ]);
        let projected = Document::from_record(4, &plain.clone().into(), &["Toronto"], &areas, None);
        assert_eq!(projected.name.as_deref(), Some("walmer road parkette"));
        assert_eq!(projected.streets, vec!["walmer road"]);
        assert_eq!(
            projected.rank_postcode.as_deref(),
            Some("m5r2z3"),
            "the nearest postcode stands in for the one it does not state"
        );
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
        assert_eq!(projected.rank_postcode, None, "no postcode data nearby");
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
    fn indexes_streets_with_a_final_direction_expanded() {
        assert_eq!(
            street_search_text("The Donway E").as_deref(),
            Some("the donway east")
        );
        assert_eq!(
            street_search_text("King St W").as_deref(),
            Some("king street west")
        );
        // Only the street field: a name keeps its letters.
        assert_eq!(search_text("Plan E").as_deref(), Some("plan e"));
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
