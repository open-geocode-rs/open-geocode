use std::{
    collections::{BTreeMap, BTreeSet},
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
    builder::report::TextIndexPrefixStats,
    pack::RecordId,
    record::{AddressComponents, InterpolationAddressComponents, NormalizedRecord, PlaceRecord},
};

pub const TEXT_INDEX_RELATIVE_PATH: &str = "text/tantivy";
pub const TEXT_INDEX_SCHEMA_VERSION: u32 = 3;

const INDEX_MEMORY_BUDGET_BYTES: usize = 1_600_000_000;
const TEXT_INDEX_BATCH_SIZE: usize = 10_000;
const MIN_AUTOCOMPLETE_PREFIX_CHARS: usize = 2;
const MAX_AUTOCOMPLETE_PREFIX_TERMS_PER_RECORD: usize = 128;
const MAX_AUTOCOMPLETE_PREFIX_TERM_BYTES: usize = 96;

const AUTOCOMPLETE_PREFIX_FIELD: &str = "autocomplete_prefix";
const AUTOCOMPLETE_POSTCODE_PREFIX_FIELD: &str = "autocomplete_postcode_prefix";
const AUTOCOMPLETE_HOUSE_NUMBER_PREFIX_FIELD: &str = "autocomplete_house_number_prefix";
const PREFIX_FIELD_NAMES: [&str; 3] = [
    AUTOCOMPLETE_PREFIX_FIELD,
    AUTOCOMPLETE_POSTCODE_PREFIX_FIELD,
    AUTOCOMPLETE_HOUSE_NUMBER_PREFIX_FIELD,
];

pub struct TantivyTextIndexWriter {
    writer: IndexWriter,
    fields: TextIndexFields,
    pending_documents: Vec<TantivyDocument>,
    prefix_stats: PrefixStatsAccumulator,
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
    pub autocomplete_prefix: Field,
    pub autocomplete_postcode_prefix: Field,
    pub autocomplete_house_number_prefix: Field,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextIndexWriteMetrics {
    pub text_projection_ns: u128,
    pub text_prefix_generation_ns: u128,
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
    pub autocomplete_prefixes: Vec<String>,
    pub autocomplete_postcode_prefixes: Vec<String>,
    pub autocomplete_house_number_prefixes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TextIndexProjection {
    document: TextIndexDocument,
    prefix_counts: PrefixTermCounts,
    prefix_generation_ns: u128,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PrefixTermCounts {
    autocomplete_prefix_terms: usize,
    autocomplete_postcode_prefix_terms: usize,
    autocomplete_house_number_prefix_terms: usize,
    autocomplete_prefix_cap_hit: bool,
    autocomplete_postcode_prefix_cap_hit: bool,
    autocomplete_house_number_prefix_cap_hit: bool,
}

#[derive(Debug, Default, Clone)]
struct PrefixStatsAccumulator {
    total_terms: u64,
    terms_per_record: Vec<u16>,
    cap_hit_count: u64,
    by_layer: BTreeMap<String, u64>,
    by_field: BTreeMap<String, u64>,
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
            prefix_stats: PrefixStatsAccumulator::default(),
            document_count: 0,
        })
    }

    pub fn add_record(
        &mut self,
        record_id: RecordId,
        record: &NormalizedRecord,
    ) -> Result<TextIndexWriteMetrics> {
        let mut metrics = TextIndexWriteMetrics::default();

        let started = Instant::now();
        let projected = TextIndexDocument::project(record_id, record);
        let projection_ns = elapsed_ns(started);
        metrics.text_projection_ns += projection_ns.saturating_sub(projected.prefix_generation_ns);
        metrics.text_prefix_generation_ns += projected.prefix_generation_ns;

        self.prefix_stats
            .record(&projected.document.layer, &projected.prefix_counts);

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

    pub fn prefix_stats(&self) -> TextIndexPrefixStats {
        self.prefix_stats.to_report()
    }
}

impl TextIndexWriteMetrics {
    pub const fn total_ns(self) -> u128 {
        self.text_projection_ns
            + self.text_prefix_generation_ns
            + self.tantivy_document_build_ns
            + self.tantivy_add_document_ns
    }

    pub fn add_assign(&mut self, other: Self) {
        self.text_projection_ns += other.text_projection_ns;
        self.text_prefix_generation_ns += other.text_prefix_generation_ns;
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
            autocomplete_prefix: schema.get_field(AUTOCOMPLETE_PREFIX_FIELD)?,
            autocomplete_postcode_prefix: schema.get_field(AUTOCOMPLETE_POSTCODE_PREFIX_FIELD)?,
            autocomplete_house_number_prefix: schema
                .get_field(AUTOCOMPLETE_HOUSE_NUMBER_PREFIX_FIELD)?,
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
        add_generated_terms(
            &mut document,
            self.autocomplete_prefix,
            &projected.autocomplete_prefixes,
        );
        add_generated_terms(
            &mut document,
            self.autocomplete_postcode_prefix,
            &projected.autocomplete_postcode_prefixes,
        );
        add_generated_terms(
            &mut document,
            self.autocomplete_house_number_prefix,
            &projected.autocomplete_house_number_prefixes,
        );
        document
    }
}

impl TextIndexDocument {
    pub fn from_record(record_id: RecordId, record: &NormalizedRecord) -> Self {
        Self::project(record_id, record).document
    }

    fn project(record_id: RecordId, record: &NormalizedRecord) -> TextIndexProjection {
        match record {
            NormalizedRecord::Address(address) => {
                let mut builder = ProjectionBuilder::new(record_id, record.layer());
                builder.label(&address.label);
                builder.name(&address.name);
                builder.address(&address.address);
                builder.build()
            }
            NormalizedRecord::Interpolation(interpolation) => {
                let mut builder = ProjectionBuilder::new(record_id, record.layer());
                builder.label(&interpolation.label);
                builder.name(&interpolation.name);
                builder.interpolation_address(&interpolation.address);
                builder.build()
            }
            NormalizedRecord::Street(street) => {
                let mut builder = ProjectionBuilder::new(record_id, record.layer());
                builder.label(&street.label);
                builder.name(&street.name);
                builder.build()
            }
            NormalizedRecord::Postcode(postcode) => {
                let mut builder = ProjectionBuilder::new(record_id, record.layer());
                builder.label(&postcode.label);
                builder.name(&postcode.name);
                builder.postcode(&postcode.postcode);
                builder.build()
            }
            NormalizedRecord::Country(place)
            | NormalizedRecord::District(place)
            | NormalizedRecord::Locality(place)
            | NormalizedRecord::Neighbourhood(place)
            | NormalizedRecord::Place(place)
            | NormalizedRecord::Region(place) => project_place(record_id, record, place),
        }
    }
}

impl PrefixTermCounts {
    const fn total_terms(self) -> usize {
        self.autocomplete_prefix_terms
            + self.autocomplete_postcode_prefix_terms
            + self.autocomplete_house_number_prefix_terms
    }

    const fn cap_hit_count(self) -> u64 {
        self.autocomplete_prefix_cap_hit as u64
            + self.autocomplete_postcode_prefix_cap_hit as u64
            + self.autocomplete_house_number_prefix_cap_hit as u64
    }
}

impl PrefixStatsAccumulator {
    fn record(&mut self, layer: &str, counts: &PrefixTermCounts) {
        let total_terms = counts.total_terms() as u64;
        self.total_terms += total_terms;
        self.terms_per_record
            .push(total_terms.min(u16::MAX as u64) as u16);
        self.cap_hit_count += counts.cap_hit_count();
        *self.by_layer.entry(layer.to_string()).or_default() += total_terms;
        *self
            .by_field
            .entry(AUTOCOMPLETE_PREFIX_FIELD.to_string())
            .or_default() += counts.autocomplete_prefix_terms as u64;
        *self
            .by_field
            .entry(AUTOCOMPLETE_POSTCODE_PREFIX_FIELD.to_string())
            .or_default() += counts.autocomplete_postcode_prefix_terms as u64;
        *self
            .by_field
            .entry(AUTOCOMPLETE_HOUSE_NUMBER_PREFIX_FIELD.to_string())
            .or_default() += counts.autocomplete_house_number_prefix_terms as u64;
    }

    fn to_report(&self) -> TextIndexPrefixStats {
        let record_count = self.terms_per_record.len() as u64;
        let mut sorted_counts = self.terms_per_record.clone();
        sorted_counts.sort_unstable();
        let p95 = percentile_u16(&sorted_counts, 0.95);
        let max = sorted_counts.last().copied().unwrap_or_default() as u64;
        let mut by_field = self.by_field.clone();
        for field in PREFIX_FIELD_NAMES {
            by_field.entry(field.to_string()).or_default();
        }
        TextIndexPrefixStats {
            autocomplete_prefix_terms_total: self.total_terms,
            autocomplete_prefix_terms_avg_per_record: if record_count == 0 {
                0.0
            } else {
                self.total_terms as f64 / record_count as f64
            },
            autocomplete_prefix_terms_p95_per_record: p95,
            autocomplete_prefix_terms_max_per_record: max,
            autocomplete_prefix_terms_cap_hit_count: self.cap_hit_count,
            autocomplete_prefix_terms_by_layer: self.by_layer.clone(),
            autocomplete_prefix_terms_by_field: by_field,
        }
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
    let postcode_exact = builder.add_text_field("postcode_exact", exact_unstored.clone());
    let autocomplete_prefix =
        builder.add_text_field(AUTOCOMPLETE_PREFIX_FIELD, exact_unstored.clone());
    let autocomplete_postcode_prefix =
        builder.add_text_field(AUTOCOMPLETE_POSTCODE_PREFIX_FIELD, exact_unstored.clone());
    let autocomplete_house_number_prefix =
        builder.add_text_field(AUTOCOMPLETE_HOUSE_NUMBER_PREFIX_FIELD, exact_unstored);
    let schema = builder.build();
    let fields = TextIndexFields {
        record_id,
        layer,
        content_text,
        label_text,
        name_text,
        address_number,
        postcode_exact,
        autocomplete_prefix,
        autocomplete_postcode_prefix,
        autocomplete_house_number_prefix,
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

fn exact_string_options() -> TextOptions {
    STRING.set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("raw")
            .set_index_option(IndexRecordOption::Basic)
            .set_fieldnorms(false),
    )
}

fn project_place(
    record_id: RecordId,
    record: &NormalizedRecord,
    place: &PlaceRecord,
) -> TextIndexProjection {
    let mut builder = ProjectionBuilder::new(record_id, record.layer());
    builder.label(&place.label);
    builder.name(&place.name);
    builder.build()
}

#[derive(Debug)]
struct ProjectionBuilder {
    projected: TextIndexDocument,
    content_parts: Vec<String>,
    autocomplete_parts: Vec<String>,
    postcode_parts: Vec<String>,
    house_number_parts: Vec<String>,
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
                autocomplete_prefixes: Vec::new(),
                autocomplete_postcode_prefixes: Vec::new(),
                autocomplete_house_number_prefixes: Vec::new(),
            },
            content_parts: Vec::new(),
            autocomplete_parts: Vec::new(),
            postcode_parts: Vec::new(),
            house_number_parts: Vec::new(),
        }
    }

    fn label(&mut self, value: &str) {
        self.projected.label = clean_text(value);
        self.add_content_text(value);
        self.add_autocomplete_text(value);
    }

    fn name(&mut self, value: &str) {
        self.projected.name = clean_text(value);
        self.add_content_text(value);
        self.add_autocomplete_text(value);
    }

    fn address(&mut self, address: &AddressComponents) {
        self.projected.address_number = clean_text(&address.number);
        self.add_content_text(&address.number);
        self.add_house_number_text(&address.number);
        self.add_optional_autocomplete_text(address.street.as_deref());
        self.add_optional_autocomplete_text(address.place.as_deref());
        self.add_optional_autocomplete_text(address.unit.as_deref());
        self.add_optional_autocomplete_text(address.locality.as_deref());
        self.add_optional_autocomplete_text(address.region.as_deref());
        self.projected.postcode = self.add_optional_postcode_text(address.postcode.as_deref());
        self.add_optional_autocomplete_text(address.country.as_deref());
    }

    fn interpolation_address(&mut self, address: &InterpolationAddressComponents) {
        self.add_optional_autocomplete_text(address.street.as_deref());
        self.add_optional_autocomplete_text(address.place.as_deref());
        self.add_optional_autocomplete_text(address.locality.as_deref());
        self.add_optional_autocomplete_text(address.region.as_deref());
        self.projected.postcode = self.add_optional_postcode_text(address.postcode.as_deref());
        self.add_optional_autocomplete_text(address.country.as_deref());
    }

    fn postcode(&mut self, value: &str) {
        self.projected.postcode = clean_text(value);
        self.add_content_text(value);
        self.add_postcode_text(value);
    }

    fn add_optional_autocomplete_text(&mut self, value: Option<&str>) {
        let Some(cleaned) = value.and_then(clean_text) else {
            return;
        };
        self.add_content_text(&cleaned);
        self.add_autocomplete_text(&cleaned);
    }

    fn add_optional_postcode_text(&mut self, value: Option<&str>) -> Option<String> {
        let cleaned = value.and_then(clean_text)?;
        self.add_content_text(&cleaned);
        self.add_postcode_text(&cleaned);
        Some(cleaned)
    }

    fn add_content_text(&mut self, value: &str) {
        if let Some(normalized) = normalize_index_text(value) {
            self.content_parts.push(normalized);
        }
    }

    fn add_autocomplete_text(&mut self, value: &str) {
        if let Some(normalized) = normalize_index_text(value) {
            self.autocomplete_parts.push(normalized);
        }
    }

    fn add_postcode_text(&mut self, value: &str) {
        if let Some(normalized) = normalize_index_text(value) {
            self.postcode_parts.push(normalized);
        }
    }

    fn add_house_number_text(&mut self, value: &str) {
        if let Some(normalized) = normalize_index_text(value) {
            self.house_number_parts.push(normalized);
        }
    }

    fn build(mut self) -> TextIndexProjection {
        let mut seen = BTreeSet::new();
        let content_parts = self
            .content_parts
            .into_iter()
            .filter(|part| seen.insert(part.clone()))
            .collect::<Vec<_>>();
        self.projected.content_text = content_parts.join(" ");

        let started = Instant::now();
        let autocomplete_prefixes = autocomplete_text_prefix_terms(&self.autocomplete_parts);
        let autocomplete_postcode_prefixes =
            autocomplete_postcode_prefix_terms(&self.postcode_parts);
        let autocomplete_house_number_prefixes =
            autocomplete_compact_prefix_terms(&self.house_number_parts);
        let prefix_generation_ns = elapsed_ns(started);

        let prefix_counts = PrefixTermCounts {
            autocomplete_prefix_terms: autocomplete_prefixes.terms.len(),
            autocomplete_postcode_prefix_terms: autocomplete_postcode_prefixes.terms.len(),
            autocomplete_house_number_prefix_terms: autocomplete_house_number_prefixes.terms.len(),
            autocomplete_prefix_cap_hit: autocomplete_prefixes.cap_hit,
            autocomplete_postcode_prefix_cap_hit: autocomplete_postcode_prefixes.cap_hit,
            autocomplete_house_number_prefix_cap_hit: autocomplete_house_number_prefixes.cap_hit,
        };

        self.projected.autocomplete_prefixes = autocomplete_prefixes.terms;
        self.projected.autocomplete_postcode_prefixes = autocomplete_postcode_prefixes.terms;
        self.projected.autocomplete_house_number_prefixes =
            autocomplete_house_number_prefixes.terms;

        TextIndexProjection {
            document: self.projected,
            prefix_counts,
            prefix_generation_ns,
        }
    }
}

fn add_generated_terms(document: &mut TantivyDocument, field: Field, terms: &[String]) {
    for term in terms {
        document.add_text(field, term);
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

fn autocomplete_text_prefix_terms(parts: &[String]) -> GeneratedPrefixTerms {
    let mut terms = PrefixTerms::default();
    for part in parts {
        add_token_prefixes(part, &mut terms);
        add_leading_sequence_prefixes(part, &mut terms);
        if terms.is_full() {
            break;
        }
    }
    terms.into_generated()
}

fn autocomplete_postcode_prefix_terms(parts: &[String]) -> GeneratedPrefixTerms {
    let mut terms = PrefixTerms::default();
    for part in parts {
        add_full_value_prefixes(part, &mut terms);
        let compact = part.split_whitespace().collect::<String>();
        if compact != *part {
            add_full_value_prefixes(&compact, &mut terms);
        }
        if terms.is_full() {
            break;
        }
    }
    terms.into_generated()
}

fn autocomplete_compact_prefix_terms(parts: &[String]) -> GeneratedPrefixTerms {
    let mut terms = PrefixTerms::default();
    for part in parts {
        let compact = part.split_whitespace().collect::<String>();
        add_full_value_prefixes(&compact, &mut terms);
        if terms.is_full() {
            break;
        }
    }
    terms.into_generated()
}

#[derive(Debug, Default)]
struct GeneratedPrefixTerms {
    terms: Vec<String>,
    cap_hit: bool,
}

#[derive(Debug, Default)]
struct PrefixTerms {
    seen: BTreeSet<String>,
    terms: Vec<String>,
    cap_hit: bool,
}

impl PrefixTerms {
    fn insert(&mut self, term: String) {
        if self.is_full() {
            self.cap_hit = true;
            return;
        }
        if term.len() > MAX_AUTOCOMPLETE_PREFIX_TERM_BYTES {
            return;
        }
        if self.seen.insert(term.clone()) {
            self.terms.push(term);
        }
    }

    fn is_full(&self) -> bool {
        self.terms.len() >= MAX_AUTOCOMPLETE_PREFIX_TERMS_PER_RECORD
    }

    fn into_generated(self) -> GeneratedPrefixTerms {
        GeneratedPrefixTerms {
            cap_hit: self.cap_hit || self.is_full(),
            terms: self.terms,
        }
    }
}

fn add_token_prefixes(value: &str, terms: &mut PrefixTerms) {
    for token in value.split_whitespace() {
        add_full_value_prefixes(token, terms);
        if terms.is_full() {
            break;
        }
    }
}

fn add_leading_sequence_prefixes(value: &str, terms: &mut PrefixTerms) {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    for token_count in 2..=tokens.len().min(6) {
        let head = tokens[..token_count - 1].join(" ");
        let tail = tokens[token_count - 1];
        for prefix in prefixes(tail) {
            terms.insert(format!("{head} {prefix}"));
            if terms.is_full() {
                return;
            }
        }
    }
}

fn add_full_value_prefixes(value: &str, terms: &mut PrefixTerms) {
    for prefix in prefixes(value) {
        terms.insert(prefix);
        if terms.is_full() {
            return;
        }
    }
}

fn prefixes(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .char_indices()
        .map(|(index, character)| index + character.len_utf8())
        .filter(|end| {
            value[..*end]
                .chars()
                .filter(|character| !character.is_whitespace())
                .count()
                >= MIN_AUTOCOMPLETE_PREFIX_CHARS
        })
        .filter_map(|end| {
            let prefix = value[..end].trim_end();
            if prefix.is_empty() {
                None
            } else {
                Some(prefix.to_string())
            }
        })
}

fn percentile_u16(sorted_values: &[u16], percentile: f64) -> u64 {
    if sorted_values.is_empty() {
        return 0;
    }
    let index = ((sorted_values.len() as f64 * percentile).ceil() as usize)
        .saturating_sub(1)
        .min(sorted_values.len() - 1);
    sorted_values[index] as u64
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
        let record = NormalizedRecord::address(AddressRecord {
            id: "osm:node:123".to_string(),
            label: "221B Baker Street, London, NW1".to_string(),
            name: "221B Baker Street".to_string(),
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
        });

        let projected = TextIndexDocument::from_record(42, &record);

        assert_eq!(projected.record_id, 42);
        assert_eq!(projected.layer, "address");
        assert_eq!(projected.address_number.as_deref(), Some("221B"));
        assert!(projected.content_text.contains("221b baker street"));
        assert!(
            projected
                .autocomplete_prefixes
                .contains(&"bake".to_string())
        );
        assert!(
            projected
                .autocomplete_prefixes
                .contains(&"221b baker".to_string())
        );
        assert!(
            projected
                .autocomplete_house_number_prefixes
                .contains(&"221b".to_string())
        );
        assert!(
            projected
                .autocomplete_postcode_prefixes
                .contains(&"nw1".to_string())
        );
    }

    #[test]
    fn projects_interpolation_without_indexing_range_fields() {
        let record = NormalizedRecord::interpolation(crate::record::InterpolationRecord {
            id: "osm:way:9:interp:1-2".to_string(),
            label: "Baker Street 1-99 odd, London".to_string(),
            name: "Baker Street".to_string(),
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
        });

        let projected = TextIndexDocument::from_record(7, &record);

        assert_eq!(projected.address_number, None);
        assert_eq!(projected.postcode.as_deref(), Some("NW1"));
        assert!(projected.content_text.contains("baker street"));
    }

    #[test]
    fn projects_postcodes_and_place_layers() {
        let postcode = NormalizedRecord::postcode(PostcodeRecord {
            id: "derived:osm:postcode:M5V".to_string(),
            label: "M5V".to_string(),
            name: "M5V".to_string(),
            postcode: "M5V".to_string(),
            geometry: point_geometry(-79.4, 43.6),
            source: DerivedSourceProvenance::osm_address_records(2),
        });
        let place = NormalizedRecord::place(
            PlaceRecord {
                id: "osm:node:1".to_string(),
                label: "Toronto".to_string(),
                name: "Toronto".to_string(),
                place_type: "city".to_string(),
                geometry: point_geometry(-79.4, 43.6),
                source: SourceProvenance {
                    dataset: "osm".to_string(),
                    object_type: OsmObjectType::Node,
                    object_id: 1,
                    tags: Some(BTreeMap::new()),
                },
            },
            PlaceLayer::Locality,
        );

        let postcode = TextIndexDocument::from_record(1, &postcode);
        let place = TextIndexDocument::from_record(2, &place);

        assert_eq!(postcode.postcode.as_deref(), Some("M5V"));
        assert_eq!(place.layer, "locality");
        assert!(place.content_text.contains("toronto"));
    }
}
