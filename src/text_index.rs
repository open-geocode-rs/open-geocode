use std::{
    collections::BTreeSet,
    fmt, mem,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result};
use tantivy::{
    Index, IndexWriter, TantivyDocument,
    indexer::UserOperation,
    schema::{FAST, Field, IndexRecordOption, STRING, Schema, TextFieldIndexing, TextOptions},
};

use crate::{
    pack::RecordId,
    record::{
        AddressComponents, AddressRecord, InterpolationAddressComponents, InterpolationRecord,
        PlaceLayer, PlaceRecord, PostcodeRecord, StreetRecord,
    },
};

pub const TEXT_INDEX_RELATIVE_PATH: &str = "text/tantivy";
pub const TEXT_INDEX_SCHEMA_VERSION: u32 = 4;

const INDEX_MEMORY_BUDGET_BYTES: usize = 1_600_000_000;
const TEXT_INDEX_BATCH_SIZE: usize = 10_000;
const AUTOCOMPLETE_SUBJECT_FIELD: &str = "autocomplete_subject_text";

pub struct TantivyTextIndexWriter {
    writer: IndexWriter,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextIndexWriteMetrics {
    pub text_projection_ns: u128,
    pub tantivy_document_build_ns: u128,
    pub tantivy_add_document_ns: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextIndexCommit {
    pub schema_version: u32,
    pub document_count: u64,
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

impl fmt::Debug for TantivyTextIndexWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TantivyTextIndexWriter")
            .field("fields", &self.fields)
            .field("pending_document_count", &self.pending_documents.len())
            .field("document_count", &self.document_count)
            .finish_non_exhaustive()
    }
}

impl TantivyTextIndexWriter {
    pub fn create(pack_path: impl AsRef<Path>) -> Result<Self> {
        let (schema, fields) = build_schema();
        let index_path = text_index_path(pack_path);
        std::fs::create_dir_all(&index_path)
            .with_context(|| format!("failed to create {}", index_path.display()))?;
        let index = Index::create_in_dir(&index_path, schema)
            .with_context(|| format!("failed to create Tantivy index {}", index_path.display()))?;
        let writer = index
            .writer(INDEX_MEMORY_BUDGET_BYTES)
            .context("failed to create Tantivy index writer")?;
        Ok(Self {
            writer,
            fields,
            pending_documents: Vec::with_capacity(TEXT_INDEX_BATCH_SIZE),
            document_count: 0,
        })
    }

    pub fn add_address(
        &mut self,
        record_id: RecordId,
        record: &AddressRecord,
    ) -> Result<TextIndexWriteMetrics> {
        let started = Instant::now();
        let projected = TextIndexDocument::project_address(record_id, record);
        self.add_projection(projected, elapsed_ns(started))
    }

    pub fn add_place(
        &mut self,
        record_id: RecordId,
        record: &PlaceRecord,
        layer: PlaceLayer,
    ) -> Result<TextIndexWriteMetrics> {
        let started = Instant::now();
        let projected =
            TextIndexDocument::project_place(record_id, place_layer_name(layer), record);
        self.add_projection(projected, elapsed_ns(started))
    }

    pub fn add_interpolation(
        &mut self,
        record_id: RecordId,
        record: &InterpolationRecord,
    ) -> Result<TextIndexWriteMetrics> {
        let started = Instant::now();
        let projected = TextIndexDocument::project_interpolation(record_id, record);
        self.add_projection(projected, elapsed_ns(started))
    }

    pub fn add_street(
        &mut self,
        record_id: RecordId,
        record: &StreetRecord,
    ) -> Result<TextIndexWriteMetrics> {
        let started = Instant::now();
        let projected = TextIndexDocument::project_street(record_id, record);
        self.add_projection(projected, elapsed_ns(started))
    }

    pub fn add_postcode(
        &mut self,
        record_id: RecordId,
        record: &PostcodeRecord,
    ) -> Result<TextIndexWriteMetrics> {
        let started = Instant::now();
        let projected = TextIndexDocument::project_postcode(record_id, record);
        self.add_projection(projected, elapsed_ns(started))
    }

    fn add_projection(
        &mut self,
        projected: TextIndexProjection,
        projection_ns: u128,
    ) -> Result<TextIndexWriteMetrics> {
        let mut metrics = TextIndexWriteMetrics::default();

        metrics.text_projection_ns += projection_ns;

        let started = Instant::now();
        let document = self.fields.to_tantivy_document(&projected.document);
        metrics.tantivy_document_build_ns += elapsed_ns(started);

        self.pending_documents.push(document);
        self.document_count += 1;

        if self.pending_documents.len() >= TEXT_INDEX_BATCH_SIZE {
            metrics.add_assign(self.flush()?);
        }

        Ok(metrics)
    }

    pub fn flush(&mut self) -> Result<TextIndexWriteMetrics> {
        let mut metrics = TextIndexWriteMetrics::default();
        if self.pending_documents.is_empty() {
            return Ok(metrics);
        }

        let documents = mem::replace(
            &mut self.pending_documents,
            Vec::with_capacity(TEXT_INDEX_BATCH_SIZE),
        );
        let operations = documents
            .into_iter()
            .map(UserOperation::Add)
            .collect::<Vec<_>>();

        let started = Instant::now();
        self.writer
            .run(operations)
            .context("failed to batch index records")?;
        metrics.tantivy_add_document_ns += elapsed_ns(started);
        Ok(metrics)
    }

    pub fn commit(&mut self) -> Result<TextIndexCommit> {
        debug_assert!(
            self.pending_documents.is_empty(),
            "text index must be flushed before commit"
        );
        self.writer
            .commit()
            .context("failed to commit Tantivy text index")?;
        Ok(TextIndexCommit {
            schema_version: TEXT_INDEX_SCHEMA_VERSION,
            document_count: self.document_count,
        })
    }
}

impl TextIndexWriteMetrics {
    pub const fn total_ns(self) -> u128 {
        self.text_projection_ns + self.tantivy_document_build_ns + self.tantivy_add_document_ns
    }

    pub fn add_assign(&mut self, other: Self) {
        self.text_projection_ns += other.text_projection_ns;
        self.tantivy_document_build_ns += other.tantivy_document_build_ns;
        self.tantivy_add_document_ns += other.tantivy_add_document_ns;
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
    pub fn from_address(record_id: RecordId, record: &AddressRecord) -> Self {
        Self::project_address(record_id, record).document
    }

    fn project_address(record_id: RecordId, address: &AddressRecord) -> TextIndexProjection {
        let mut builder = ProjectionBuilder::new(record_id, "address");
        builder.label_for_search(&address.label());
        builder.name_for_search(&address.name());
        builder.address(&address.address);
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

pub fn text_index_path(pack_path: impl AsRef<Path>) -> PathBuf {
    pack_path.as_ref().join(TEXT_INDEX_RELATIVE_PATH)
}

pub fn open_text_index(pack_path: impl AsRef<Path>) -> Result<Index> {
    let path = text_index_path(pack_path);
    Index::open_in_dir(&path)
        .with_context(|| format!("failed to open Tantivy index {}", path.display()))
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

fn place_layer_name(layer: PlaceLayer) -> &'static str {
    match layer {
        PlaceLayer::Country => "country",
        PlaceLayer::Region => "region",
        PlaceLayer::District => "district",
        PlaceLayer::Place => "place",
        PlaceLayer::Locality => "locality",
        PlaceLayer::Neighbourhood => "neighbourhood",
    }
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
        self.projected.label = clean_text(value);
        self.add_content_text(value);
        self.add_autocomplete_subject_text(value);
    }

    fn name(&mut self, value: &str) {
        self.projected.name = clean_text(value);
        self.add_content_text(value);
        self.add_autocomplete_subject_text(value);
    }

    fn label_for_search(&mut self, value: &str) {
        self.projected.label = clean_text(value);
        self.add_content_text(value);
    }

    fn name_for_search(&mut self, value: &str) {
        self.projected.name = clean_text(value);
        self.add_content_text(value);
    }

    fn address(&mut self, address: &AddressComponents) {
        self.projected.address_number = clean_text(&address.number);
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
        self.projected.postcode = clean_text(value);
        self.add_content_text(value);
        self.add_postcode_subject_text(value);
    }

    fn add_optional_autocomplete_text(&mut self, value: Option<&str>) {
        let Some(cleaned) = value.and_then(clean_text) else {
            return;
        };
        self.add_autocomplete_subject_text(&cleaned);
    }

    fn add_optional_content_text(&mut self, value: Option<&str>) {
        if let Some(cleaned) = value.and_then(clean_text) {
            self.add_content_text(&cleaned);
        }
    }

    fn add_optional_postcode_text(&mut self, value: Option<&str>) -> Option<String> {
        let cleaned = value.and_then(clean_text)?;
        self.add_content_text(&cleaned);
        self.add_postcode_subject_text(&cleaned);
        Some(cleaned)
    }

    fn add_optional_postcode_for_search(&mut self, value: Option<&str>) -> Option<String> {
        let cleaned = value.and_then(clean_text)?;
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
    if let Some(value) = value.and_then(clean_text) {
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

fn clean_text(value: &str) -> Option<String> {
    let cleaned = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
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

fn unique_parts(parts: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    parts
        .into_iter()
        .filter(|part| seen.insert(part.clone()))
        .collect()
}

fn elapsed_ns(started: Instant) -> u128 {
    started.elapsed().as_nanos()
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

        let projected = TextIndexDocument::project_address(42, &record).document;

        assert_eq!(projected.record_id, 42);
        assert_eq!(projected.layer, "address");
        assert_eq!(projected.address_number.as_deref(), Some("221B"));
        assert!(projected.content_text.contains("221b baker street"));
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
            anchor_ids: vec!["osm:node:1".to_string(), "osm:node:2".to_string()],
            geometry: point_geometry(-0.1586, 51.5237),
            representative_point: [-0.1586, 51.5237],
            source: SourceProvenance::osm(OsmObjectType::Way, 9),
        };

        let projected = TextIndexDocument::project_interpolation(7, &record).document;

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

        let postcode = TextIndexDocument::project_postcode(1, &postcode).document;
        let place =
            TextIndexDocument::project_place(2, place_layer_name(PlaceLayer::Locality), &place)
                .document;

        assert_eq!(postcode.postcode.as_deref(), Some("M5V"));
        assert_eq!(place.layer, "locality");
        assert!(place.content_text.contains("toronto"));
        assert_eq!(postcode.autocomplete_subject_text, "m5v");
        assert_eq!(place.autocomplete_subject_text, "toronto");
    }
}
