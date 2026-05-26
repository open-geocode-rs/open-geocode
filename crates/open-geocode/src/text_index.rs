use std::{
    collections::BTreeSet,
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tantivy::{
    Index, IndexWriter, TantivyDocument,
    schema::{
        FAST, Field, INDEXED, IndexRecordOption, STORED, STRING, Schema, TEXT, TextFieldIndexing,
        TextOptions,
    },
};

use crate::{
    pack::RecordId,
    record::{
        AddressComponents, InterpolationAddressComponents, LocationPrecision, NormalizedRecord,
        PlaceRecord,
    },
};

pub const TEXT_INDEX_RELATIVE_PATH: &str = "text/tantivy";
pub const TEXT_INDEX_SCHEMA_VERSION: u32 = 2;

const INDEX_MEMORY_BUDGET_BYTES: usize = 50_000_000;
const MIN_AUTOCOMPLETE_PREFIX_CHARS: usize = 2;
const MAX_AUTOCOMPLETE_PREFIX_TERMS_PER_RECORD: usize = 128;
const MAX_AUTOCOMPLETE_PREFIX_TERM_BYTES: usize = 96;

pub struct TantivyTextIndexWriter {
    writer: IndexWriter,
    fields: TextIndexFields,
    document_count: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct TextIndexFields {
    pub record_id: Field,
    pub layer: Field,
    pub source_id: Field,
    pub place_type: Field,
    pub location_precision: Field,
    pub label_text: Field,
    pub name_text: Field,
    pub all_text: Field,
    pub address_number: Field,
    pub street_text: Field,
    pub place_text: Field,
    pub unit_text: Field,
    pub locality_text: Field,
    pub region_text: Field,
    pub postcode_text: Field,
    pub postcode_exact: Field,
    pub country_text: Field,
    pub autocomplete_prefix: Field,
    pub autocomplete_postcode_prefix: Field,
    pub autocomplete_house_number_prefix: Field,
    pub interpolation_start: Field,
    pub interpolation_end: Field,
    pub interpolation_step: Field,
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
    pub source_id: String,
    pub place_type: Option<String>,
    pub location_precision: Option<String>,
    pub label: Option<String>,
    pub name: Option<String>,
    pub address_number: Option<String>,
    pub street: Option<String>,
    pub place: Option<String>,
    pub unit: Option<String>,
    pub locality: Option<String>,
    pub region: Option<String>,
    pub postcode: Option<String>,
    pub country: Option<String>,
    pub autocomplete_prefixes: Vec<String>,
    pub autocomplete_postcode_prefixes: Vec<String>,
    pub autocomplete_house_number_prefixes: Vec<String>,
    pub interpolation_start: Option<u64>,
    pub interpolation_end: Option<u64>,
    pub interpolation_step: Option<u64>,
    pub all_text: String,
}

impl fmt::Debug for TantivyTextIndexWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TantivyTextIndexWriter")
            .field("fields", &self.fields)
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
            document_count: 0,
        })
    }

    pub fn add_record(&mut self, record_id: RecordId, record: &NormalizedRecord) -> Result<()> {
        let projected = TextIndexDocument::from_record(record_id, record);
        let document = self.fields.to_tantivy_document(&projected);
        self.writer
            .add_document(document)
            .with_context(|| format!("failed to index record {record_id}"))?;
        self.document_count += 1;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<TextIndexCommit> {
        self.writer
            .commit()
            .context("failed to commit Tantivy text index")?;
        Ok(TextIndexCommit {
            schema_version: TEXT_INDEX_SCHEMA_VERSION,
            document_count: self.document_count,
        })
    }
}

impl TextIndexFields {
    pub fn from_schema(schema: &Schema) -> Result<Self> {
        Ok(Self {
            record_id: schema.get_field("record_id")?,
            layer: schema.get_field("layer")?,
            source_id: schema.get_field("source_id")?,
            place_type: schema.get_field("place_type")?,
            location_precision: schema.get_field("location_precision")?,
            label_text: schema.get_field("label_text")?,
            name_text: schema.get_field("name_text")?,
            all_text: schema.get_field("all_text")?,
            address_number: schema.get_field("address_number")?,
            street_text: schema.get_field("street_text")?,
            place_text: schema.get_field("place_text")?,
            unit_text: schema.get_field("unit_text")?,
            locality_text: schema.get_field("locality_text")?,
            region_text: schema.get_field("region_text")?,
            postcode_text: schema.get_field("postcode_text")?,
            postcode_exact: schema.get_field("postcode_exact")?,
            country_text: schema.get_field("country_text")?,
            autocomplete_prefix: schema.get_field("autocomplete_prefix")?,
            autocomplete_postcode_prefix: schema.get_field("autocomplete_postcode_prefix")?,
            autocomplete_house_number_prefix: schema
                .get_field("autocomplete_house_number_prefix")?,
            interpolation_start: schema.get_field("interpolation_start")?,
            interpolation_end: schema.get_field("interpolation_end")?,
            interpolation_step: schema.get_field("interpolation_step")?,
        })
    }

    fn to_tantivy_document(self, projected: &TextIndexDocument) -> TantivyDocument {
        let mut document = TantivyDocument::default();
        document.add_u64(self.record_id, projected.record_id);
        document.add_text(self.layer, &projected.layer);
        document.add_text(self.source_id, &projected.source_id);
        add_text_if_present(
            &mut document,
            self.place_type,
            projected.place_type.as_deref(),
        );
        add_text_if_present(
            &mut document,
            self.location_precision,
            projected.location_precision.as_deref(),
        );
        add_text_if_present(&mut document, self.label_text, projected.label.as_deref());
        add_text_if_present(&mut document, self.name_text, projected.name.as_deref());
        add_normalized_text_if_present(
            &mut document,
            self.address_number,
            projected.address_number.as_deref(),
        );
        add_text_if_present(&mut document, self.street_text, projected.street.as_deref());
        add_text_if_present(&mut document, self.place_text, projected.place.as_deref());
        add_text_if_present(&mut document, self.unit_text, projected.unit.as_deref());
        add_text_if_present(
            &mut document,
            self.locality_text,
            projected.locality.as_deref(),
        );
        add_text_if_present(&mut document, self.region_text, projected.region.as_deref());
        add_text_if_present(
            &mut document,
            self.postcode_text,
            projected.postcode.as_deref(),
        );
        add_normalized_text_if_present(
            &mut document,
            self.postcode_exact,
            projected.postcode.as_deref(),
        );
        add_text_if_present(
            &mut document,
            self.country_text,
            projected.country.as_deref(),
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
        if let Some(value) = projected.interpolation_start {
            document.add_u64(self.interpolation_start, value);
        }
        if let Some(value) = projected.interpolation_end {
            document.add_u64(self.interpolation_end, value);
        }
        if let Some(value) = projected.interpolation_step {
            document.add_u64(self.interpolation_step, value);
        }
        if !projected.all_text.is_empty() {
            document.add_text(self.all_text, &projected.all_text);
        }
        document
    }
}

impl TextIndexDocument {
    pub fn from_record(record_id: RecordId, record: &NormalizedRecord) -> Self {
        match record {
            NormalizedRecord::Address(address) => {
                let mut builder = ProjectionBuilder::new(record_id, record.layer(), record.id());
                builder.location_precision(location_precision_name(address.location_precision()));
                builder.label(&address.label);
                builder.name(&address.name);
                builder.address(&address.address);
                builder.build()
            }
            NormalizedRecord::Interpolation(interpolation) => {
                let mut builder = ProjectionBuilder::new(record_id, record.layer(), record.id());
                builder.label(&interpolation.label);
                builder.name(&interpolation.name);
                builder.interpolation_address(&interpolation.address);
                builder.interpolation_range(
                    interpolation.interpolation.start as u64,
                    interpolation.interpolation.end as u64,
                    interpolation.interpolation.step as u64,
                );
                builder.build()
            }
            NormalizedRecord::Street(street) => {
                let mut builder = ProjectionBuilder::new(record_id, record.layer(), record.id());
                builder.label(&street.label);
                builder.name(&street.name);
                builder.build()
            }
            NormalizedRecord::Postcode(postcode) => {
                let mut builder = ProjectionBuilder::new(record_id, record.layer(), record.id());
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
    let record_id = builder.add_u64_field("record_id", INDEXED | STORED | FAST);
    let exact_stored = exact_string_options(true);
    let layer = builder.add_text_field("layer", exact_stored.clone());
    let source_id = builder.add_text_field("source_id", exact_stored.clone());
    let place_type = builder.add_text_field("place_type", exact_stored.clone());
    let location_precision = builder.add_text_field("location_precision", exact_stored.clone());
    let label_text = builder.add_text_field("label_text", TEXT);
    let name_text = builder.add_text_field("name_text", TEXT);
    let all_text = builder.add_text_field("all_text", TEXT);
    let address_number = builder.add_text_field("address_number", exact_stored.clone());
    let street_text = builder.add_text_field("street_text", TEXT);
    let place_text = builder.add_text_field("place_text", TEXT);
    let unit_text = builder.add_text_field("unit_text", TEXT);
    let locality_text = builder.add_text_field("locality_text", TEXT);
    let region_text = builder.add_text_field("region_text", TEXT);
    let postcode_text = builder.add_text_field("postcode_text", TEXT);
    let postcode_exact = builder.add_text_field("postcode_exact", exact_stored);
    let country_text = builder.add_text_field("country_text", TEXT);
    let exact_unstored = exact_string_options(false);
    let autocomplete_prefix = builder.add_text_field("autocomplete_prefix", exact_unstored.clone());
    let autocomplete_postcode_prefix =
        builder.add_text_field("autocomplete_postcode_prefix", exact_unstored.clone());
    let autocomplete_house_number_prefix =
        builder.add_text_field("autocomplete_house_number_prefix", exact_unstored);
    let interpolation_start = builder.add_u64_field("interpolation_start", INDEXED | STORED);
    let interpolation_end = builder.add_u64_field("interpolation_end", INDEXED | STORED);
    let interpolation_step = builder.add_u64_field("interpolation_step", INDEXED | STORED);
    let schema = builder.build();
    let fields = TextIndexFields {
        record_id,
        layer,
        source_id,
        place_type,
        location_precision,
        label_text,
        name_text,
        all_text,
        address_number,
        street_text,
        place_text,
        unit_text,
        locality_text,
        region_text,
        postcode_text,
        postcode_exact,
        country_text,
        autocomplete_prefix,
        autocomplete_postcode_prefix,
        autocomplete_house_number_prefix,
        interpolation_start,
        interpolation_end,
        interpolation_step,
    };
    (schema, fields)
}

fn exact_string_options(stored: bool) -> TextOptions {
    let options = STRING.set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("raw")
            .set_index_option(IndexRecordOption::Basic)
            .set_fieldnorms(false),
    );
    if stored {
        options.set_stored()
    } else {
        options
    }
}

fn project_place(
    record_id: RecordId,
    record: &NormalizedRecord,
    place: &PlaceRecord,
) -> TextIndexDocument {
    let mut builder = ProjectionBuilder::new(record_id, record.layer(), record.id());
    builder.label(&place.label);
    builder.name(&place.name);
    builder.place_type(&place.place_type);
    builder.place(&place.name);
    builder.build()
}

#[derive(Debug)]
struct ProjectionBuilder {
    projected: TextIndexDocument,
    text_parts: Vec<String>,
    autocomplete_parts: Vec<String>,
    postcode_parts: Vec<String>,
    house_number_parts: Vec<String>,
}

impl ProjectionBuilder {
    fn new(record_id: RecordId, layer: &str, source_id: &str) -> Self {
        Self {
            projected: TextIndexDocument {
                record_id,
                layer: layer.to_string(),
                source_id: source_id.to_string(),
                place_type: None,
                location_precision: None,
                label: None,
                name: None,
                address_number: None,
                street: None,
                place: None,
                unit: None,
                locality: None,
                region: None,
                postcode: None,
                country: None,
                autocomplete_prefixes: Vec::new(),
                autocomplete_postcode_prefixes: Vec::new(),
                autocomplete_house_number_prefixes: Vec::new(),
                interpolation_start: None,
                interpolation_end: None,
                interpolation_step: None,
                all_text: String::new(),
            },
            text_parts: Vec::new(),
            autocomplete_parts: Vec::new(),
            postcode_parts: Vec::new(),
            house_number_parts: Vec::new(),
        }
    }

    fn label(&mut self, value: &str) {
        self.projected.label = clean_text(value);
        self.add_text(value);
        self.add_autocomplete_text(value);
    }

    fn name(&mut self, value: &str) {
        self.projected.name = clean_text(value);
        self.add_text(value);
        self.add_autocomplete_text(value);
    }

    fn address(&mut self, address: &AddressComponents) {
        self.projected.address_number = clean_text(&address.number);
        self.add_text(&address.number);
        self.add_house_number_text(&address.number);
        self.projected.street = self.add_optional_autocomplete_text(address.street.as_deref());
        self.projected.place = self.add_optional_autocomplete_text(address.place.as_deref());
        self.projected.unit = self.add_optional_autocomplete_text(address.unit.as_deref());
        self.projected.locality = self.add_optional_autocomplete_text(address.locality.as_deref());
        self.projected.region = self.add_optional_autocomplete_text(address.region.as_deref());
        self.projected.postcode = self.add_optional_postcode_text(address.postcode.as_deref());
        self.projected.country = self.add_optional_autocomplete_text(address.country.as_deref());
    }

    fn interpolation_address(&mut self, address: &InterpolationAddressComponents) {
        self.projected.street = self.add_optional_autocomplete_text(address.street.as_deref());
        self.projected.place = self.add_optional_autocomplete_text(address.place.as_deref());
        self.projected.locality = self.add_optional_autocomplete_text(address.locality.as_deref());
        self.projected.region = self.add_optional_autocomplete_text(address.region.as_deref());
        self.projected.postcode = self.add_optional_postcode_text(address.postcode.as_deref());
        self.projected.country = self.add_optional_autocomplete_text(address.country.as_deref());
    }

    fn interpolation_range(&mut self, start: u64, end: u64, step: u64) {
        self.projected.interpolation_start = Some(start);
        self.projected.interpolation_end = Some(end);
        self.projected.interpolation_step = Some(step);
    }

    fn postcode(&mut self, value: &str) {
        self.projected.postcode = clean_text(value);
        self.add_text(value);
        self.add_postcode_text(value);
    }

    fn place(&mut self, value: &str) {
        self.projected.place = clean_text(value);
        self.add_text(value);
        self.add_autocomplete_text(value);
    }

    fn place_type(&mut self, value: &str) {
        self.projected.place_type = clean_text(value);
    }

    fn location_precision(&mut self, value: &str) {
        self.projected.location_precision = clean_text(value);
    }

    fn add_optional_autocomplete_text(&mut self, value: Option<&str>) -> Option<String> {
        let cleaned = value.and_then(clean_text)?;
        self.add_text(&cleaned);
        self.add_autocomplete_text(&cleaned);
        Some(cleaned)
    }

    fn add_optional_postcode_text(&mut self, value: Option<&str>) -> Option<String> {
        let cleaned = value.and_then(clean_text)?;
        self.add_text(&cleaned);
        self.add_postcode_text(&cleaned);
        Some(cleaned)
    }

    fn add_text(&mut self, value: &str) {
        if let Some(normalized) = normalize_index_text(value) {
            self.text_parts.push(normalized);
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

    fn build(mut self) -> TextIndexDocument {
        let mut seen = BTreeSet::new();
        let text_parts = self
            .text_parts
            .into_iter()
            .filter(|part| seen.insert(part.clone()))
            .collect::<Vec<_>>();
        self.projected.all_text = text_parts.join(" ");
        self.projected.autocomplete_prefixes =
            autocomplete_text_prefix_terms(&self.autocomplete_parts);
        self.projected.autocomplete_postcode_prefixes =
            autocomplete_postcode_prefix_terms(&self.postcode_parts);
        self.projected.autocomplete_house_number_prefixes =
            autocomplete_compact_prefix_terms(&self.house_number_parts);
        self.projected
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

fn autocomplete_text_prefix_terms(parts: &[String]) -> Vec<String> {
    let mut terms = PrefixTerms::default();
    for part in parts {
        add_token_prefixes(part, &mut terms);
        add_leading_sequence_prefixes(part, &mut terms);
        if terms.is_full() {
            break;
        }
    }
    terms.into_vec()
}

fn autocomplete_postcode_prefix_terms(parts: &[String]) -> Vec<String> {
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
    terms.into_vec()
}

fn autocomplete_compact_prefix_terms(parts: &[String]) -> Vec<String> {
    let mut terms = PrefixTerms::default();
    for part in parts {
        let compact = part.split_whitespace().collect::<String>();
        add_full_value_prefixes(&compact, &mut terms);
        if terms.is_full() {
            break;
        }
    }
    terms.into_vec()
}

#[derive(Debug, Default)]
struct PrefixTerms {
    seen: BTreeSet<String>,
    terms: Vec<String>,
}

impl PrefixTerms {
    fn insert(&mut self, term: String) {
        if self.is_full() || term.len() > MAX_AUTOCOMPLETE_PREFIX_TERM_BYTES {
            return;
        }
        if self.seen.insert(term.clone()) {
            self.terms.push(term);
        }
    }

    fn is_full(&self) -> bool {
        self.terms.len() >= MAX_AUTOCOMPLETE_PREFIX_TERMS_PER_RECORD
    }

    fn into_vec(self) -> Vec<String> {
        self.terms
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

fn location_precision_name(precision: LocationPrecision) -> &'static str {
    match precision {
        LocationPrecision::Point => "point",
        LocationPrecision::Centroid => "centroid",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::record::{
        AddressRecord, DerivedSourceProvenance, InterpolationRange, OsmObjectType, PlaceLayer,
        PostcodeRecord, SourceProvenance, point_geometry,
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
        assert_eq!(projected.source_id, "osm:node:123");
        assert_eq!(projected.address_number.as_deref(), Some("221B"));
        assert_eq!(projected.street.as_deref(), Some("Baker Street"));
        assert!(projected.all_text.contains("221b baker street"));
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
    fn projects_interpolation_as_range_not_materialized_addresses() {
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

        assert_eq!(projected.interpolation_start, Some(1));
        assert_eq!(projected.interpolation_end, Some(99));
        assert_eq!(projected.interpolation_step, Some(2));
        assert_eq!(projected.address_number, None);
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
        assert_eq!(place.place_type.as_deref(), Some("city"));
        assert_eq!(place.place.as_deref(), Some("Toronto"));
    }
}
