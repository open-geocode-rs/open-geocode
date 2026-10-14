//! Tantivy text index.
//!
//! The index is built in the scratch directory, merged to one segment, and its
//! files are copied into the Pack as `text/<file>` sections. At runtime a
//! read-only [`Directory`] serves those sections straight from the Pack
//! mapping, so the text index needs no files of its own.

use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap},
    fmt, fs, io, mem,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use tantivy::{
    Index, IndexWriter, TantivyDocument,
    directory::{
        Directory, FileHandle, RamDirectory, WatchCallback, WatchHandle, WritePtr,
        error::{DeleteError, OpenReadError, OpenWriteError},
    },
    indexer::UserOperation,
    merge_policy::NoMergePolicy,
    schema::{FAST, Field, IndexRecordOption, STRING, Schema, TextFieldIndexing, TextOptions},
};

use crate::{
    container::{Bytes, Container, ContainerWriter},
    extsort::Scratch,
    labels,
    memory::{MemoryBudget, Reservation},
    pack::RecordId,
    record::{
        AddressComponents, AddressRecord, InterpolationAddressComponents, InterpolationRecord,
        PlaceRecord, PostcodeRecord, Record, StreetRecord,
    },
    util::text::collapse_whitespace,
};

pub const TEXT_SECTION_PREFIX: &str = "text/";
pub const TEXT_INDEX_SCHEMA_VERSION: u32 = 5;

/// Tantivy needs at least 15 MB per indexing thread (it uses fewer threads
/// when given less) and gains little beyond 1.6 GB.
const MIN_INDEX_MEMORY_BYTES: usize = 16 << 20;
const MAX_INDEX_MEMORY_BYTES: usize = 1_600_000_000;
const TEXT_INDEX_BATCH_SIZE: usize = 10_000;
const AUTOCOMPLETE_SUBJECT_FIELD: &str = "autocomplete_subject_text";

pub struct TextIndexWriter {
    /// Created with the first document, so its buffers are reserved only for
    /// the phase that writes records.
    writer: Option<(IndexWriter, Reservation)>,
    budget: Arc<MemoryBudget>,
    memory_bytes: usize,
    index: Index,
    path: PathBuf,
    fields: TextIndexFields,
    pending_documents: Vec<TantivyDocument>,
    document_count: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct TextIndexFields {
    pub record_id: Field,
    pub layer: Field,
    pub content_text: Field,
    pub label_text: Field,
    pub name_text: Field,
    pub address_number: Field,
    pub postcode_exact: Field,
    pub autocomplete_subject_text: Field,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextIndexCommit {
    pub document_count: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextIndexDocument {
    pub record_id: RecordId,
    pub layer: String,
    pub content_text: String,
    pub label: Option<String>,
    pub name: Option<String>,
    pub address_number: Option<String>,
    pub postcode: Option<String>,
    pub autocomplete_subject_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TextIndexProjection {
    document: TextIndexDocument,
}

impl fmt::Debug for TextIndexWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TextIndexWriter")
            .field("fields", &self.fields)
            .field("pending_document_count", &self.pending_documents.len())
            .field("document_count", &self.document_count)
            .finish_non_exhaustive()
    }
}

impl TextIndexWriter {
    pub fn create(
        scratch: &Arc<Scratch>,
        budget: &Arc<MemoryBudget>,
        memory_bytes: usize,
    ) -> Result<Self> {
        let (schema, fields) = build_schema();
        let path = scratch.path().join("text");
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        let index = Index::create_in_dir(&path, schema)
            .with_context(|| format!("failed to create Tantivy index {}", path.display()))?;
        Ok(Self {
            writer: None,
            budget: Arc::clone(budget),
            memory_bytes: memory_bytes.clamp(MIN_INDEX_MEMORY_BYTES, MAX_INDEX_MEMORY_BYTES),
            index,
            path,
            fields,
            pending_documents: Vec::with_capacity(TEXT_INDEX_BATCH_SIZE),
            document_count: 0,
        })
    }

    pub fn fields(&self) -> TextIndexFields {
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
            let writer = self
                .index
                .writer(self.memory_bytes)
                .context("failed to create Tantivy index writer")?;
            // Disable background auto-merges during the build so the only
            // merge is the single deterministic one forced in `finish()`.
            writer.set_merge_policy(Box::new(NoMergePolicy));
            self.writer = Some((writer, memory));
        }
        Ok(&mut self.writer.as_mut().expect("writer created").0)
    }

    pub fn add(&mut self, document: TantivyDocument) -> Result<()> {
        self.pending_documents.push(document);
        self.document_count += 1;
        if self.pending_documents.len() >= TEXT_INDEX_BATCH_SIZE {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.pending_documents.is_empty() {
            return Ok(());
        }
        let documents = mem::replace(
            &mut self.pending_documents,
            Vec::with_capacity(TEXT_INDEX_BATCH_SIZE),
        );
        self.writer()?
            .run(documents.into_iter().map(UserOperation::Add))
            .context("failed to batch index records")?;
        Ok(())
    }

    /// Commit, merge to a single segment and copy the index into the Pack.
    pub fn finish(mut self, pack: &mut ContainerWriter) -> Result<TextIndexCommit> {
        self.flush()?;
        self.writer()?;
        let (mut writer, memory) = self.writer.take().expect("writer created");
        writer
            .commit()
            .context("failed to commit Tantivy text index")?;
        let segment_ids = self
            .index
            .searchable_segment_ids()
            .context("failed to list text index segments")?;
        if segment_ids.len() > 1 {
            writer
                .merge(&segment_ids)
                .wait()
                .context("failed to merge text index segments")?;
            writer
                .commit()
                .context("failed to commit merged text index")?;
        }
        writer
            .garbage_collect_files()
            .wait()
            .context("failed to garbage collect text index files")?;
        writer
            .wait_merging_threads()
            .context("failed to stop Tantivy indexing threads")?;
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
                .context("text index file name is not UTF-8")?;
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
        Ok(TextIndexCommit {
            document_count: self.document_count,
            bytes,
        })
    }
}

impl TextIndexFields {
    pub fn from_schema(schema: &Schema) -> Result<Self> {
        Ok(Self {
            record_id: schema.get_field("record_id")?,
            layer: schema.get_field("layer")?,
            content_text: schema.get_field("content_text")?,
            label_text: schema.get_field("label_text")?,
            name_text: schema.get_field("name_text")?,
            address_number: schema.get_field("address_number")?,
            postcode_exact: schema.get_field("postcode_exact")?,
            autocomplete_subject_text: schema.get_field(AUTOCOMPLETE_SUBJECT_FIELD)?,
        })
    }

    /// Tantivy document for a record. Pure, so callers can build documents in
    /// parallel before handing them to the writer.
    pub fn document(self, record_id: RecordId, record: &Record) -> TantivyDocument {
        self.to_tantivy_document(&TextIndexDocument::from_record(record_id, record))
    }

    fn to_tantivy_document(self, projected: &TextIndexDocument) -> TantivyDocument {
        let mut document = TantivyDocument::default();
        document.add_u64(self.record_id, projected.record_id);
        document.add_text(self.layer, &projected.layer);
        if !projected.content_text.is_empty() {
            document.add_text(self.content_text, &projected.content_text);
        }
        add_text_if_present(&mut document, self.label_text, projected.label.as_deref());
        add_text_if_present(&mut document, self.name_text, projected.name.as_deref());
        add_normalized_text_if_present(
            &mut document,
            self.address_number,
            projected.address_number.as_deref(),
        );
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
        document
    }
}

impl TextIndexDocument {
    pub fn from_record(record_id: RecordId, record: &Record) -> Self {
        match record {
            Record::Address(record) => Self::project_address(record_id, record),
            Record::Interpolation(record) => Self::project_interpolation(record_id, record),
            Record::Street(record) => Self::project_street(record_id, record),
            Record::Postcode(record) => Self::project_postcode(record_id, record),
            Record::Place(layer, record) => Self::project_place(record_id, layer.as_str(), record),
        }
        .document
    }

    fn project_address(record_id: RecordId, address: &AddressRecord) -> TextIndexProjection {
        let mut builder = ProjectionBuilder::new(record_id, "address");
        let (searchable, unit) = split_unit(&address.address);
        builder.label_for_search(&labels::address_label(&searchable));
        builder.name_for_search(&labels::address_name(&searchable));
        builder.address(&searchable);
        if let Some(unit) = unit {
            builder.add_content_text(&unit_token(&unit));
        }
        builder.build()
    }

    fn project_interpolation(
        record_id: RecordId,
        interpolation: &InterpolationRecord,
    ) -> TextIndexProjection {
        let mut builder = ProjectionBuilder::new(record_id, "interpolation");
        builder.label_for_search(&interpolation.label());
        builder.name_for_search(&interpolation.name());
        builder.interpolation_address(&interpolation.address);
        builder.build()
    }

    fn project_street(record_id: RecordId, street: &StreetRecord) -> TextIndexProjection {
        let mut builder = ProjectionBuilder::new(record_id, "street");
        builder.label(&street.label());
        builder.name(&street.name);
        builder.build()
    }

    fn project_postcode(record_id: RecordId, postcode: &PostcodeRecord) -> TextIndexProjection {
        let mut builder = ProjectionBuilder::new(record_id, "postcode");
        builder.label_for_search(&postcode.label());
        builder.name_for_search(&postcode.name());
        builder.postcode(&postcode.postcode);
        builder.build()
    }

    fn project_place(record_id: RecordId, layer: &str, place: &PlaceRecord) -> TextIndexProjection {
        let mut builder = ProjectionBuilder::new(record_id, layer);
        builder.label(&place.label());
        builder.name(&place.name);
        builder.build()
    }
}

pub fn open_text_index(container: &Container) -> Result<Index> {
    Index::open(PackDirectory::new(container)?).context("failed to open the Pack text index")
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
                bail!(
                    "text index version {} is unsupported; rebuild the Pack for version {TEXT_INDEX_SCHEMA_VERSION}",
                    info.version
                );
            }
            files.insert(PathBuf::from(file), container.raw_section(name)?);
        }
        if files.is_empty() {
            bail!("Pack has no text index");
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

fn build_schema() -> (Schema, TextIndexFields) {
    let mut builder = Schema::builder();
    let record_id = builder.add_u64_field("record_id", FAST);
    let exact_unstored = exact_string_options();
    let layer = builder.add_text_field("layer", exact_unstored.clone());
    let content_text = builder.add_text_field("content_text", searchable_text_options());
    let label_text = builder.add_text_field("label_text", searchable_text_options());
    let name_text = builder.add_text_field("name_text", searchable_text_options());
    let address_number = builder.add_text_field("address_number", exact_unstored.clone());
    let postcode_exact = builder.add_text_field("postcode_exact", exact_unstored);
    let autocomplete_subject_text = builder.add_text_field(
        AUTOCOMPLETE_SUBJECT_FIELD,
        autocomplete_subject_text_options(),
    );
    let schema = builder.build();
    let fields = TextIndexFields {
        record_id,
        layer,
        content_text,
        label_text,
        name_text,
        address_number,
        postcode_exact,
        autocomplete_subject_text,
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
    projected: TextIndexDocument,
    content_parts: Vec<String>,
    autocomplete_subject_parts: Vec<String>,
}

impl ProjectionBuilder {
    fn new(record_id: RecordId, layer: &str) -> Self {
        Self {
            projected: TextIndexDocument {
                record_id,
                layer: layer.to_string(),
                content_text: String::new(),
                label: None,
                name: None,
                address_number: None,
                postcode: None,
                autocomplete_subject_text: String::new(),
            },
            content_parts: Vec::new(),
            autocomplete_subject_parts: Vec::new(),
        }
    }

    fn label(&mut self, value: &str) {
        self.projected.label = collapse_whitespace(value);
        self.add_content_text(value);
        self.add_autocomplete_subject_text(value);
    }

    fn name(&mut self, value: &str) {
        self.projected.name = collapse_whitespace(value);
        self.add_content_text(value);
        self.add_autocomplete_subject_text(value);
    }

    fn label_for_search(&mut self, value: &str) {
        self.projected.label = collapse_whitespace(value);
        self.add_content_text(value);
    }

    fn name_for_search(&mut self, value: &str) {
        self.projected.name = collapse_whitespace(value);
        self.add_content_text(value);
    }

    fn address(&mut self, address: &AddressComponents) {
        self.projected.address_number = collapse_whitespace(&address.number);
        self.add_content_text(&address.number);
        self.add_optional_content_text(address.street.as_deref());
        self.add_optional_content_text(address.place.as_deref());
        self.add_optional_content_text(address.unit.as_deref());
        self.add_optional_content_text(address.locality.as_deref());
        self.add_optional_content_text(address.region.as_deref());
        let address_subject = address.street.as_deref().or(address.place.as_deref());
        self.add_optional_autocomplete_text(address_subject);
        self.projected.postcode = self.add_optional_postcode_text(address.postcode.as_deref());
        self.add_optional_content_text(address.country.as_deref());
    }

    fn interpolation_address(&mut self, address: &InterpolationAddressComponents) {
        self.add_optional_content_text(address.street.as_deref());
        self.add_optional_content_text(address.place.as_deref());
        self.add_optional_content_text(address.locality.as_deref());
        self.add_optional_content_text(address.region.as_deref());
        self.projected.postcode =
            self.add_optional_postcode_for_search(address.postcode.as_deref());
        self.add_optional_content_text(address.country.as_deref());
    }

    fn postcode(&mut self, value: &str) {
        self.projected.postcode = collapse_whitespace(value);
        self.add_content_text(value);
        self.add_postcode_subject_text(value);
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

    fn add_optional_postcode_text(&mut self, value: Option<&str>) -> Option<String> {
        let cleaned = value.and_then(collapse_whitespace)?;
        self.add_content_text(&cleaned);
        self.add_postcode_subject_text(&cleaned);
        Some(cleaned)
    }

    fn add_optional_postcode_for_search(&mut self, value: Option<&str>) -> Option<String> {
        let cleaned = value.and_then(collapse_whitespace)?;
        self.add_content_text(&cleaned);
        Some(cleaned)
    }

    fn add_content_text(&mut self, value: &str) {
        if let Some(normalized) = normalize_index_text(value) {
            self.content_parts.push(normalized);
        }
    }

    fn add_autocomplete_subject_text(&mut self, value: &str) {
        if let Some(normalized) = normalize_index_text(value) {
            self.autocomplete_subject_parts.push(normalized);
        }
    }

    fn add_postcode_subject_text(&mut self, value: &str) {
        if let Some(normalized) = normalize_index_text(value) {
            self.content_parts.push(normalized.clone());
            self.autocomplete_subject_parts.push(normalized.clone());
            let compact = normalized.split_whitespace().collect::<String>();
            if compact != normalized {
                self.content_parts.push(compact.clone());
                self.autocomplete_subject_parts.push(compact);
            }
        }
    }

    fn build(mut self) -> TextIndexProjection {
        let content_parts = unique_parts(self.content_parts);
        self.projected.content_text = content_parts.join(" ");
        let autocomplete_subject_parts = unique_parts(self.autocomplete_subject_parts);
        self.projected.autocomplete_subject_text = autocomplete_subject_parts.join(" ");

        TextIndexProjection {
            document: self.projected,
        }
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

/// The index token of a unit. It cannot collide with a house number, and
/// queries that name the unit are rewritten to it (`unit_query` in search.rs).
pub(crate) fn unit_token(unit: &str) -> String {
    format!("u{unit}")
}

/// An address as it is indexed, and the unit taken out of it.
///
/// AU/NZ write a flat as "UNIT/NUMBER" ("10/1367"), and the tokenizer would
/// otherwise index the 10 as if it were a house, so "10 Eruera Street" matched
/// unit 10 of number 1367. The number keeps the house alone and the unit
/// becomes its own token. A unit given as its own tag ("Flat 4") is handled
/// the same way.
///
/// This assumes unit first, as `strip_unit_terms` does for queries. Europe's
/// "BUILDING/FLAT" ("10/12" in Poland) is read the wrong way round, and the
/// country is too rarely tagged to tell them apart. Only all-digit units
/// before a number that starts with a digit are split, so "10/A", "12 1/2",
/// "12-14/3" and "1/2/34" are indexed as written.
fn split_unit(address: &AddressComponents) -> (Cow<'_, AddressComponents>, Option<String>) {
    let all_digits =
        |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    let mut number = address.number.as_str();
    let mut unit = None;
    if let Some((flat, house)) = number.rsplit_once('/')
        && all_digits(flat)
        && house.starts_with(|character: char| character.is_ascii_digit())
    {
        number = house;
        unit = Some(flat.to_string());
    }
    let tag = address
        .unit
        .as_deref()
        .and_then(normalize_index_text)
        .and_then(|tag| {
            tag.split_whitespace()
                .last()
                .filter(|last| all_digits(last))
                .map(str::to_string)
        });
    let tag_taken = tag.is_some();
    if number.len() == address.number.len() && !tag_taken {
        return (Cow::Borrowed(address), None);
    }
    let split = AddressComponents {
        number: number.to_string(),
        unit: if tag_taken {
            None
        } else {
            address.unit.clone()
        },
        ..address.clone()
    };
    (Cow::Owned(split), unit.or(tag))
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
        AddressRecord, DerivedSourceProvenance, InterpolationRange, LocationPrecision,
        OsmObjectType, PlaceLayer, PostcodeRecord, SourceProvenance, point_geometry,
    };

    use super::*;

    #[test]
    fn projects_address_fields_for_search() {
        let record = AddressRecord {
            address: AddressComponents {
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
            source: SourceProvenance::osm(OsmObjectType::Node, 123),
        };

        let projected = TextIndexDocument::from_record(42, &record.into());

        assert_eq!(projected.record_id, 42);
        assert_eq!(projected.layer, "address");
        assert_eq!(projected.address_number.as_deref(), Some("221B"));
        assert!(projected.content_text.contains("221b baker street"));
        assert_eq!(projected.autocomplete_subject_text, "baker street nw1");
        assert!(!projected.autocomplete_subject_text.contains("london"));
    }

    #[test]
    fn projects_a_flat_by_its_building_number() {
        let mut components = AddressComponents {
            number: "10/1367".to_string(),
            street: Some("Eruera Street".to_string()),
            place: None,
            unit: Some("Flat 4".to_string()),
            locality: Some("Rotorua".to_string()),
            region: None,
            postcode: None,
            country: None,
        };
        let record = |components: &AddressComponents| AddressRecord {
            address: components.clone(),
            geometry: point_geometry(176.2, -38.1),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(OsmObjectType::Node, 1),
        };

        let projected = TextIndexDocument::from_record(1, &record(&components).into());
        assert_eq!(projected.address_number.as_deref(), Some("1367"));
        let tokens: Vec<_> = projected.content_text.split_whitespace().collect();
        assert!(tokens.contains(&"1367") && tokens.contains(&"u10"));
        assert!(!tokens.contains(&"10") && !tokens.contains(&"flat"));

        // A unit given as a tag only is indexed the same way.
        components.number = "1367".to_string();
        let projected = TextIndexDocument::from_record(1, &record(&components).into());
        assert!(
            projected
                .content_text
                .split_whitespace()
                .any(|token| token == "u4")
        );

        // These are not "UNIT/NUMBER" and keep their number.
        for number in ["12 1/2", "10/A", "12-14/3", "1/2/34"] {
            components.number = number.to_string();
            let projected = TextIndexDocument::from_record(1, &record(&components).into());
            assert_eq!(projected.address_number.as_deref(), Some(number));
        }
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
            source: SourceProvenance::osm(OsmObjectType::Way, 9),
        };

        let projected = TextIndexDocument::from_record(7, &record.into());

        assert_eq!(projected.address_number, None);
        assert_eq!(projected.postcode.as_deref(), Some("NW1"));
        assert!(projected.content_text.contains("baker street"));
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
            name: "Toronto".to_string(),
            place_type: "city".to_string(),
            geometry: point_geometry(-79.4, 43.6),
            source: SourceProvenance {
                dataset: "osm".to_string(),
                object_type: OsmObjectType::Node,
                object_id: 1,
                tags: Some(BTreeMap::new()),
            },
        };

        let postcode = TextIndexDocument::from_record(1, &postcode.into());
        let place = TextIndexDocument::from_record(2, &Record::Place(PlaceLayer::Locality, place));

        assert_eq!(postcode.postcode.as_deref(), Some("M5V"));
        assert_eq!(place.layer, "locality");
        assert!(place.content_text.contains("toronto"));
        assert_eq!(postcode.autocomplete_subject_text, "m5v");
        assert_eq!(place.autocomplete_subject_text, "toronto");
    }
}
