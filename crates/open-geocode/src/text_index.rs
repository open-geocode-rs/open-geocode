use std::{
    collections::BTreeSet,
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tantivy::{
    Index, IndexWriter, TantivyDocument,
    schema::{FAST, Field, INDEXED, STORED, STRING, Schema, TEXT},
};

use crate::{
    pack::RecordId,
    record::{
        AddressComponents, InterpolationAddressComponents, LocationPrecision, NormalizedRecord,
        PlaceRecord,
    },
};

pub const TEXT_INDEX_RELATIVE_PATH: &str = "text/tantivy";
pub const TEXT_INDEX_SCHEMA_VERSION: u32 = 1;

const INDEX_MEMORY_BUDGET_BYTES: usize = 50_000_000;
const MAX_AUTOCOMPLETE_TERMS: usize = 128;
const MAX_PREFIX_CHARS: usize = 24;
const MAX_LEADING_SEQUENCE_TOKENS: usize = 4;

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
    pub autocomplete_text: Field,
    pub address_number: Field,
    pub street_text: Field,
    pub place_text: Field,
    pub unit_text: Field,
    pub locality_text: Field,
    pub region_text: Field,
    pub postcode_text: Field,
    pub postcode_exact: Field,
    pub country_text: Field,
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
    pub interpolation_start: Option<u64>,
    pub interpolation_end: Option<u64>,
    pub interpolation_step: Option<u64>,
    pub all_text: String,
    pub autocomplete_terms: Vec<String>,
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
            autocomplete_text: schema.get_field("autocomplete_text")?,
            address_number: schema.get_field("address_number")?,
            street_text: schema.get_field("street_text")?,
            place_text: schema.get_field("place_text")?,
            unit_text: schema.get_field("unit_text")?,
            locality_text: schema.get_field("locality_text")?,
            region_text: schema.get_field("region_text")?,
            postcode_text: schema.get_field("postcode_text")?,
            postcode_exact: schema.get_field("postcode_exact")?,
            country_text: schema.get_field("country_text")?,
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
        for term in &projected.autocomplete_terms {
            document.add_text(self.autocomplete_text, term);
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
    let layer = builder.add_text_field("layer", STRING | STORED);
    let source_id = builder.add_text_field("source_id", STRING | STORED);
    let place_type = builder.add_text_field("place_type", STRING | STORED);
    let location_precision = builder.add_text_field("location_precision", STRING | STORED);
    let label_text = builder.add_text_field("label_text", TEXT);
    let name_text = builder.add_text_field("name_text", TEXT);
    let all_text = builder.add_text_field("all_text", TEXT);
    let autocomplete_text = builder.add_text_field("autocomplete_text", STRING);
    let address_number = builder.add_text_field("address_number", STRING | STORED);
    let street_text = builder.add_text_field("street_text", TEXT);
    let place_text = builder.add_text_field("place_text", TEXT);
    let unit_text = builder.add_text_field("unit_text", TEXT);
    let locality_text = builder.add_text_field("locality_text", TEXT);
    let region_text = builder.add_text_field("region_text", TEXT);
    let postcode_text = builder.add_text_field("postcode_text", TEXT);
    let postcode_exact = builder.add_text_field("postcode_exact", STRING | STORED);
    let country_text = builder.add_text_field("country_text", TEXT);
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
        autocomplete_text,
        address_number,
        street_text,
        place_text,
        unit_text,
        locality_text,
        region_text,
        postcode_text,
        postcode_exact,
        country_text,
        interpolation_start,
        interpolation_end,
        interpolation_step,
    };
    (schema, fields)
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
    autocomplete_sources: Vec<String>,
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
                interpolation_start: None,
                interpolation_end: None,
                interpolation_step: None,
                all_text: String::new(),
                autocomplete_terms: Vec::new(),
            },
            text_parts: Vec::new(),
            autocomplete_sources: Vec::new(),
        }
    }

    fn label(&mut self, value: &str) {
        self.projected.label = clean_text(value);
        self.add_text(value);
    }

    fn name(&mut self, value: &str) {
        self.projected.name = clean_text(value);
        self.add_text(value);
    }

    fn address(&mut self, address: &AddressComponents) {
        self.projected.address_number = clean_text(&address.number);
        self.add_text(&address.number);
        self.projected.street = self.add_optional_text(address.street.as_deref());
        self.projected.place = self.add_optional_text(address.place.as_deref());
        self.projected.unit = self.add_optional_text(address.unit.as_deref());
        self.projected.locality = self.add_optional_text(address.locality.as_deref());
        self.projected.region = self.add_optional_text(address.region.as_deref());
        self.projected.postcode = self.add_optional_text(address.postcode.as_deref());
        self.projected.country = self.add_optional_text(address.country.as_deref());
    }

    fn interpolation_address(&mut self, address: &InterpolationAddressComponents) {
        self.projected.street = self.add_optional_text(address.street.as_deref());
        self.projected.place = self.add_optional_text(address.place.as_deref());
        self.projected.locality = self.add_optional_text(address.locality.as_deref());
        self.projected.region = self.add_optional_text(address.region.as_deref());
        self.projected.postcode = self.add_optional_text(address.postcode.as_deref());
        self.projected.country = self.add_optional_text(address.country.as_deref());
    }

    fn interpolation_range(&mut self, start: u64, end: u64, step: u64) {
        self.projected.interpolation_start = Some(start);
        self.projected.interpolation_end = Some(end);
        self.projected.interpolation_step = Some(step);
    }

    fn postcode(&mut self, value: &str) {
        self.projected.postcode = clean_text(value);
        self.add_text(value);
    }

    fn place(&mut self, value: &str) {
        self.projected.place = clean_text(value);
        self.add_text(value);
    }

    fn place_type(&mut self, value: &str) {
        self.projected.place_type = clean_text(value);
    }

    fn location_precision(&mut self, value: &str) {
        self.projected.location_precision = clean_text(value);
    }

    fn add_optional_text(&mut self, value: Option<&str>) -> Option<String> {
        let cleaned = value.and_then(clean_text)?;
        self.add_text(&cleaned);
        Some(cleaned)
    }

    fn add_text(&mut self, value: &str) {
        if let Some(normalized) = normalize_index_text(value) {
            self.text_parts.push(normalized);
        }
        if let Some(cleaned) = clean_text(value) {
            self.autocomplete_sources.push(cleaned);
        }
    }

    fn build(mut self) -> TextIndexDocument {
        let mut seen = BTreeSet::new();
        self.projected.all_text = self
            .text_parts
            .into_iter()
            .filter(|part| seen.insert(part.clone()))
            .collect::<Vec<_>>()
            .join(" ");
        self.projected.autocomplete_terms = autocomplete_terms(&self.autocomplete_sources);
        self.projected
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

fn normalize_index_text(value: &str) -> Option<String> {
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

fn autocomplete_terms(values: &[String]) -> Vec<String> {
    let mut terms = Vec::new();
    let mut seen = BTreeSet::new();
    for value in values {
        let Some(normalized) = normalize_index_text(value) else {
            continue;
        };
        add_prefix_terms(&normalized, &mut terms, &mut seen);
        if terms.len() >= MAX_AUTOCOMPLETE_TERMS {
            break;
        }
    }
    terms
}

fn add_prefix_terms(value: &str, terms: &mut Vec<String>, seen: &mut BTreeSet<String>) {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    for token in &tokens {
        for prefix in token_prefixes(token) {
            push_autocomplete_term(prefix, terms, seen);
            if terms.len() >= MAX_AUTOCOMPLETE_TERMS {
                return;
            }
        }
    }

    for end_index in 0..tokens.len().min(MAX_LEADING_SEQUENCE_TOKENS) {
        for prefix in token_prefixes(tokens[end_index]) {
            let mut sequence = tokens[..end_index].to_vec();
            sequence.push(prefix.as_str());
            push_autocomplete_term(sequence.join(" "), terms, seen);
            if terms.len() >= MAX_AUTOCOMPLETE_TERMS {
                return;
            }
        }
    }
}

fn push_autocomplete_term(term: String, terms: &mut Vec<String>, seen: &mut BTreeSet<String>) {
    if seen.insert(term.clone()) {
        terms.push(term);
    }
}

fn token_prefixes(token: &str) -> Vec<String> {
    let characters = token.chars().collect::<Vec<_>>();
    if characters.len() < 2 {
        return Vec::new();
    }
    let max = characters.len().min(MAX_PREFIX_CHARS);
    (2..=max)
        .map(|length| characters[..length].iter().collect::<String>())
        .collect()
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
    fn projects_address_fields_and_autocomplete_prefixes() {
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
                .autocomplete_terms
                .contains(&"221b ba".to_string())
        );
        assert!(
            projected
                .autocomplete_terms
                .contains(&"baker st".to_string())
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
        assert!(postcode.autocomplete_terms.contains(&"m5".to_string()));
        assert_eq!(place.layer, "locality");
        assert_eq!(place.place_type.as_deref(), Some("city"));
        assert_eq!(place.place.as_deref(), Some("Toronto"));
    }
}
