use std::{path::Path, sync::Arc};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tantivy::{
    Index, IndexReader, Score, Searcher, Term,
    collector::TopDocs,
    query::{BooleanQuery, Occur, PhrasePrefixQuery, Query, QueryParser, TermQuery},
    schema::{Field, IndexRecordOption},
};

use crate::{
    pack::{PackReader, RecordId, RecordSummary},
    text_index::{
        TEXT_INDEX_SCHEMA_VERSION, TextIndexFields, normalize_index_text, open_text_index,
    },
};

pub struct PackTextSearcher {
    pack: Arc<PackReader>,
    index: Index,
    reader: IndexReader,
    fields: TextIndexFields,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSearchOptions {
    pub query: String,
    pub limit: usize,
    pub layer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextAutocompleteOptions {
    pub query: String,
    pub limit: usize,
    pub layer: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TextSearchHit {
    pub record_id: RecordId,
    pub score: Score,
    pub record: RecordSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressGeocodeOptions {
    pub address: String,
    pub locality: Option<String>,
    pub region: Option<String>,
    pub postcode: Option<String>,
    pub limit: usize,
    pub layer: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AddressGeocodeHit {
    pub query: String,
    pub hit: TextSearchHit,
}

pub const DEFAULT_SEARCH_LIMIT: usize = 10;
pub const MAX_AUTOCOMPLETE_LIMIT: usize = 20;
const MIN_AUTOCOMPLETE_QUERY_CHARS: usize = 3;
const AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS: u32 = 1_024;

impl PackTextSearcher {
    pub fn open(pack_path: impl AsRef<Path>) -> Result<Self> {
        Self::from_pack(Arc::new(PackReader::open(pack_path)?))
    }

    /// Open the text index using an existing shared pack reader.
    pub fn from_pack(pack: Arc<PackReader>) -> Result<Self> {
        let text_index_manifest = pack
            .manifest()
            .text_index
            .as_ref()
            .context("pack manifest is missing text index metadata")?;
        if text_index_manifest.schema_version != TEXT_INDEX_SCHEMA_VERSION {
            bail!(
                "text index schema version {} is unsupported; rebuild pack for schema {}",
                text_index_manifest.schema_version,
                TEXT_INDEX_SCHEMA_VERSION
            );
        }
        let index = open_text_index(pack.path())?;
        let schema = index.schema();
        let fields = TextIndexFields::from_schema(&schema)?;
        let reader = index.reader().context("failed to open Tantivy reader")?;
        Ok(Self {
            pack,
            index,
            reader,
            fields,
        })
    }

    pub fn search(&self, options: TextSearchOptions) -> Result<Vec<TextSearchHit>> {
        let limit = effective_limit(options.limit);
        if limit == 0 {
            return Ok(Vec::new());
        }

        let query_text = options.query.trim();
        if query_text.is_empty() {
            bail!("search query cannot be empty");
        }

        let (_, hits) = self.search_variants(query_text, options.layer.as_deref(), limit)?;
        Ok(hits)
    }

    pub fn geocode_address(
        &self,
        options: AddressGeocodeOptions,
    ) -> Result<Option<AddressGeocodeHit>> {
        let limit = effective_limit(options.limit);
        if limit == 0 {
            return Ok(None);
        }

        let address = options.address.trim();
        if address.is_empty() {
            return Ok(None);
        }

        for candidate in address_geocode_candidates(address, options.postcode.as_deref()) {
            let (query, hits) =
                self.search_variants(&candidate, options.layer.as_deref(), limit)?;
            for hit in hits {
                if hit.record.point.is_none() {
                    continue;
                }
                if self.hit_matches_address_context(&hit, &options)? {
                    return Ok(Some(AddressGeocodeHit { query, hit }));
                }
            }
        }

        Ok(None)
    }

    pub fn autocomplete(&self, options: TextAutocompleteOptions) -> Result<Vec<TextSearchHit>> {
        let limit = effective_autocomplete_limit(options.limit);
        if limit == 0 {
            return Ok(Vec::new());
        }

        let Some(query_text) = normalize_index_text(options.query.trim()) else {
            return Ok(Vec::new());
        };
        if query_text
            .chars()
            .filter(|character| !character.is_whitespace())
            .count()
            < MIN_AUTOCOMPLETE_QUERY_CHARS
        {
            return Ok(Vec::new());
        }

        let Some(query) = self.build_autocomplete_query(&query_text, options.layer.as_deref())?
        else {
            return Ok(Vec::new());
        };
        let searcher = self.reader.searcher();
        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(limit))
            .with_context(|| format!("failed to autocomplete text index for {query_text:?}"))?;

        self.hydrate_top_docs(top_docs)
    }

    fn build_query(&self, query_text: &str, layer: Option<&str>) -> Result<Box<dyn Query>> {
        let mut query_parser = QueryParser::for_index(&self.index, self.search_fields());
        query_parser.set_conjunction_by_default();
        query_parser.set_field_boost(self.fields.label_text, 3.0);
        query_parser.set_field_boost(self.fields.name_text, 2.5);
        query_parser.set_field_boost(self.fields.address_number, 2.0);
        query_parser.set_field_boost(self.fields.postcode_exact, 2.0);

        let text_query = query_parser
            .parse_query(query_text)
            .with_context(|| format!("failed to parse search query {query_text:?}"))?;

        let Some(layer) = layer.map(str::trim).filter(|layer| !layer.is_empty()) else {
            return Ok(text_query);
        };

        let layer_query = TermQuery::new(
            Term::from_field_text(self.fields.layer, layer),
            IndexRecordOption::Basic,
        );
        Ok(Box::new(BooleanQuery::new(vec![
            (Occur::Must, text_query),
            (Occur::Must, Box::new(layer_query)),
        ])))
    }

    fn search_variants(
        &self,
        query_text: &str,
        layer: Option<&str>,
        limit: usize,
    ) -> Result<(String, Vec<TextSearchHit>)> {
        let mut last_query = query_text.trim().to_string();
        let mut parse_error = None;
        for variant in search_query_variants(query_text) {
            last_query = variant.clone();
            let query = match self.build_query(&variant, layer) {
                Ok(query) => query,
                Err(error) => {
                    parse_error = Some(error);
                    continue;
                }
            };
            let searcher = self.reader.searcher();
            let top_docs = searcher
                .search(&query, &TopDocs::with_limit(limit))
                .with_context(|| format!("failed to search text index for {variant:?}"))?;
            let hits = self.hydrate_top_docs(top_docs)?;
            if !hits.is_empty() {
                return Ok((variant, hits));
            }
        }

        if let Some(error) = parse_error {
            if search_query_variants(query_text).is_empty() {
                return Err(error);
            }
        }

        Ok((last_query, Vec::new()))
    }

    fn hit_matches_address_context(
        &self,
        hit: &TextSearchHit,
        options: &AddressGeocodeOptions,
    ) -> Result<bool> {
        let desired_region = normalized_for_match(options.region.as_deref());
        let desired_locality = normalized_for_match(options.locality.as_deref());
        let desired_postcode = normalized_postcode_for_match(options.postcode.as_deref());
        if desired_region.is_none() && desired_locality.is_none() && desired_postcode.is_none() {
            return Ok(true);
        }

        let Some(context) = self.pack.boundary_context(hit.record_id)? else {
            return Ok(desired_region.is_none() && desired_locality.is_none());
        };

        let mut admin_labels = Vec::new();
        if let Some(tuple) = context.admin_context {
            for (layer, record_id) in [
                ("country", tuple.country_record_id),
                ("region", tuple.region_record_id),
                ("district", tuple.district_record_id),
                ("locality", tuple.locality_record_id),
                ("neighbourhood", tuple.neighbourhood_record_id),
                ("place", tuple.place_record_id),
            ] {
                if let Some(record_id) = record_id
                    && let Some(record) = self.pack.context_record(record_id)?
                {
                    admin_labels.push((
                        layer,
                        normalized_for_match(Some(&record.label)),
                        normalized_for_match(Some(&record.name)),
                    ));
                }
            }
        }

        if let Some(region) = desired_region
            && !admin_labels.iter().any(|(layer, label, name)| {
                *layer == "region"
                    && (label.as_deref() == Some(region.as_str())
                        || name.as_deref() == Some(region.as_str()))
            })
        {
            return Ok(false);
        }

        if let Some(locality) = desired_locality {
            let locality_layers = ["district", "locality", "neighbourhood", "place"];
            if !admin_labels.iter().any(|(layer, label, name)| {
                locality_layers.contains(layer)
                    && (label.as_deref() == Some(locality.as_str())
                        || name.as_deref() == Some(locality.as_str()))
            }) {
                return Ok(false);
            }
        }

        if let Some(postcode) = desired_postcode
            && let Some(postcode_record_id) = context.postcode_record_id
            && let Some(record) = self.pack.context_record(postcode_record_id)?
            && normalized_postcode_for_match(record.postcode.as_deref()).as_deref()
                != Some(postcode.as_str())
        {
            return Ok(false);
        }

        Ok(true)
    }

    fn search_fields(&self) -> Vec<tantivy::schema::Field> {
        vec![
            self.fields.label_text,
            self.fields.name_text,
            self.fields.content_text,
            self.fields.address_number,
            self.fields.postcode_exact,
        ]
    }

    fn build_autocomplete_query(
        &self,
        query_text: &str,
        layer: Option<&str>,
    ) -> Result<Option<Box<dyn Query>>> {
        let tokens = autocomplete_query_tokens(query_text);
        if tokens.is_empty() {
            return Ok(None);
        }

        let mut subqueries = Vec::new();

        let subject_tokens = if tokens.len() > 1 && is_address_number_token(&tokens[0]) {
            subqueries.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.address_number, &tokens[0]),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ));
            &tokens[1..]
        } else {
            tokens.as_slice()
        };

        if subject_tokens.is_empty() {
            return Ok(None);
        }

        subqueries.push((
            Occur::Must,
            autocomplete_subject_query(self.fields.autocomplete_subject_text, subject_tokens),
        ));

        if let Some(layer) = layer.map(str::trim).filter(|layer| !layer.is_empty()) {
            subqueries.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.layer, layer),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ));
        }

        Ok(Some(Box::new(BooleanQuery::new(subqueries))))
    }

    fn record_id_from_doc_address(
        &self,
        searcher: &Searcher,
        doc_address: tantivy::DocAddress,
    ) -> Result<RecordId> {
        let record_id_reader = searcher
            .segment_reader(doc_address.segment_ord)
            .fast_fields()
            .u64("record_id")?;
        record_id_reader
            .values_for_doc(doc_address.doc_id)
            .next()
            .context("text index hit is missing fast record_id")
    }

    fn hydrate_top_docs(
        &self,
        top_docs: Vec<(Score, tantivy::DocAddress)>,
    ) -> Result<Vec<TextSearchHit>> {
        let searcher = self.reader.searcher();
        top_docs
            .into_iter()
            .map(|(score, doc_address)| {
                let record_id = self.record_id_from_doc_address(&searcher, doc_address)?;
                let record = self.pack.record_summary(record_id)?;
                Ok(TextSearchHit {
                    record_id,
                    score,
                    record,
                })
            })
            .collect()
    }
}

fn effective_limit(limit: usize) -> usize {
    if limit == 0 {
        DEFAULT_SEARCH_LIMIT
    } else {
        limit
    }
}

fn effective_autocomplete_limit(limit: usize) -> usize {
    let limit = effective_limit(limit);
    limit.min(MAX_AUTOCOMPLETE_LIMIT)
}

fn address_geocode_candidates(address: &str, postcode: Option<&str>) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(postcode) = postcode.and_then(normalize_index_text)
        && let Some(address) = meaningful_address_query(address)
    {
        candidates.push(format!("{address} {postcode}"));
    }
    candidates.push(address.to_string());
    unique_strings(candidates)
}

fn meaningful_address_query(address: &str) -> Option<String> {
    let normalized = normalize_index_text(address)?;
    let expanded = expand_address_abbreviations(&normalized);
    strip_unit_terms(&expanded)
}

fn search_query_variants(query_text: &str) -> Vec<String> {
    let mut variants = Vec::new();
    if let Some(cleaned) = collapse_query(query_text) {
        variants.push(cleaned);
    }
    if let Some(normalized) = normalize_index_text(query_text) {
        variants.push(normalized.clone());
        let expanded = expand_address_abbreviations(&normalized);
        variants.push(expanded.clone());
        if let Some(without_unit) = strip_unit_terms(&expanded) {
            variants.push(without_unit);
        }
    }
    unique_strings(variants)
}

fn collapse_query(value: &str) -> Option<String> {
    let cleaned = value.split_whitespace().collect::<Vec<_>>().join(" ");
    (!cleaned.is_empty()).then_some(cleaned)
}

fn expand_address_abbreviations(value: &str) -> String {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    let mut expanded = Vec::with_capacity(tokens.len());
    for (index, token) in tokens.iter().enumerate() {
        let replacement = match *token {
            "ave" | "av" => Some("avenue"),
            "blvd" => Some("boulevard"),
            "cir" => Some("circle"),
            "ct" | "crt" => Some("court"),
            "cres" => Some("crescent"),
            "dr" => Some("drive"),
            "hwy" => Some("highway"),
            "ln" => Some("lane"),
            "pkwy" => Some("parkway"),
            "pl" => Some("place"),
            "rd" => Some("road"),
            "sq" => Some("square"),
            "st" if index > 0 && !is_numeric_token(tokens[index - 1]) => Some("street"),
            "ter" | "terr" => Some("terrace"),
            "trl" | "tr" => Some("trail"),
            "wy" => Some("way"),
            "e" if previous_token_is_street_type(&expanded) => Some("east"),
            "n" if previous_token_is_street_type(&expanded) => Some("north"),
            "s" if previous_token_is_street_type(&expanded) => Some("south"),
            "w" if previous_token_is_street_type(&expanded) => Some("west"),
            _ => None,
        };
        expanded.push(replacement.unwrap_or(*token));
    }
    expanded.join(" ")
}

fn previous_token_is_street_type(tokens: &[&str]) -> bool {
    tokens.last().is_some_and(|token| {
        matches!(
            *token,
            "avenue"
                | "boulevard"
                | "circle"
                | "court"
                | "crescent"
                | "drive"
                | "highway"
                | "lane"
                | "parkway"
                | "place"
                | "road"
                | "square"
                | "street"
                | "terrace"
                | "trail"
                | "way"
        )
    })
}

fn strip_unit_terms(value: &str) -> Option<String> {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    let mut stripped = Vec::with_capacity(tokens.len());
    let mut index = 0;
    while index < tokens.len() {
        if is_unit_designator(tokens[index]) {
            index += 1;
            if index < tokens.len() && is_unit_value_token(tokens[index]) {
                index += 1;
            }
            continue;
        }
        stripped.push(tokens[index]);
        index += 1;
    }

    if stripped.len() > 2 && is_numeric_token(stripped[0]) && is_numeric_token(stripped[1]) {
        stripped.remove(0);
    }

    let stripped = stripped.join(" ");
    (!stripped.is_empty()).then_some(stripped)
}

fn is_unit_designator(token: &str) -> bool {
    matches!(
        token,
        "apt"
            | "apartment"
            | "bldg"
            | "building"
            | "dept"
            | "department"
            | "fl"
            | "floor"
            | "rm"
            | "room"
            | "ste"
            | "suite"
            | "unit"
    )
}

fn is_unit_value_token(token: &str) -> bool {
    token.chars().any(|character| character.is_ascii_digit())
}

fn is_numeric_token(token: &str) -> bool {
    token.chars().all(|character| character.is_ascii_digit())
}

fn normalized_for_match(value: Option<&str>) -> Option<String> {
    normalize_index_text(value?.trim())
}

fn normalized_postcode_for_match(value: Option<&str>) -> Option<String> {
    normalized_for_match(value).map(|value| value.split_whitespace().collect())
}

fn unique_strings(values: Vec<String>) -> Vec<String> {
    let mut unique = Vec::new();
    for value in values {
        if !unique.contains(&value) {
            unique.push(value);
        }
    }
    unique
}

fn autocomplete_query_tokens(query_text: &str) -> Vec<String> {
    query_text
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>()
}

fn autocomplete_subject_query(field: Field, tokens: &[String]) -> Box<dyn Query> {
    let terms = tokens
        .iter()
        .map(|token| Term::from_field_text(field, token))
        .collect::<Vec<_>>();
    let mut query = PhrasePrefixQuery::new(terms);
    query.set_max_expansions(AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS);
    Box::new(query)
}

fn is_address_number_token(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::{
        builder::report::BuilderReport,
        pack::{PackWriter, RecordWriter},
        record::{
            AddressComponents, AddressRecord, DerivedSourceProvenance, LocationPrecision,
            OsmObjectType, PostcodeRecord, SourceProvenance, StreetRecord, point_geometry,
        },
    };

    use super::*;

    #[test]
    fn search_query_variants_expand_common_address_abbreviations() {
        let variants = search_query_variants("33 Princess St Suite 170");
        assert!(variants.contains(&"33 princess street suite 170".to_string()));
        assert!(variants.contains(&"33 princess street".to_string()));
    }

    #[test]
    fn search_query_variants_strip_leading_unit_numbers() {
        let variants = search_query_variants("306-1333 Sheppard Ave E");
        assert!(variants.contains(&"306 1333 sheppard avenue east".to_string()));
        assert!(variants.contains(&"1333 sheppard avenue east".to_string()));
    }

    #[test]
    fn search_query_variants_do_not_treat_initial_saint_as_street() {
        let variants = search_query_variants("St Clair Ave W");
        assert!(variants.contains(&"st clair avenue west".to_string()));
        assert!(!variants.contains(&"street clair avenue west".to_string()));
    }

    #[test]
    fn street_search_and_autocomplete_do_not_decode_road_geometry() {
        use std::io::{Seek, SeekFrom, Write};

        let root =
            std::env::temp_dir().join(format!("open-geocode-summary-{}", uuid::Uuid::new_v4()));
        let mut writer = PackWriter::create(&root).expect("writer");
        let mut street = street_record("osm:way:9", "King Street");
        street.geometry = geojson::Geometry::new(geojson::GeometryValue::LineString {
            coordinates: vec![vec![-79.0, 43.0].into(), vec![-79.001, 43.001].into()],
        });
        writer.write_street(&street).expect("street");
        writer
            .finish(&mut BuilderReport::default())
            .expect("finish");
        let generation = crate::pack::resolve_pack_path(&root).expect("generation");
        // Damage only the road shape before opening any readers. A summary has no
        // reason to read it, while a full-record request must still report the error.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(generation.join("records/geometries"))
            .expect("geometry file");
        file.seek(SeekFrom::Start(
            crate::records_store::ARENA_HEADER_BYTES as u64,
        ))
        .expect("seek");
        file.write_all(&0u32.to_le_bytes())
            .expect("invalid point count");
        drop(file);
        let searcher = PackTextSearcher::open(&root).expect("searcher");
        let hits = searcher
            .search(TextSearchOptions {
                query: "King".into(),
                limit: 5,
                layer: None,
            })
            .expect("summary search");
        assert_eq!(hits[0].record.label, "King Street");
        let suggestions = searcher
            .autocomplete(TextAutocompleteOptions {
                query: "Kin".into(),
                limit: 5,
                layer: None,
            })
            .expect("summary autocomplete");
        assert_eq!(suggestions[0].record, hits[0].record);
        assert!(searcher.pack.record_json(0).is_err());
    }

    #[test]
    fn searches_and_hydrates_records_from_pack() {
        let temp_dir = temp_pack_path("search-hydrates");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write_address(&address_record(
                "osm:node:1",
                "10 King Street, Toronto",
                "10",
                "King Street",
                Some("Toronto"),
                Some("M5V 1A1"),
            ))
            .expect("write king address");
        writer
            .write_address(&address_record(
                "osm:node:2",
                "20 Queen Street, Toronto",
                "20",
                "Queen Street",
                Some("Toronto"),
                Some("M5V 1A1"),
            ))
            .expect("write queen address");
        writer
            .finish(&mut BuilderReport::default())
            .expect("finish");

        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");
        let hits = searcher
            .search(TextSearchOptions {
                query: "King Street Toronto".to_string(),
                limit: 5,
                layer: None,
            })
            .expect("search");

        assert!(!hits.is_empty());
        assert_eq!(hits[0].record_id, 0);
        assert_eq!(hits[0].record.id, "osm:node:1");
        assert_eq!(hits[0].record.label, "10 King Street, Toronto, M5V 1A1");

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn filters_hits_by_layer() {
        let temp_dir = temp_pack_path("search-layer");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write_address(&address_record(
                "osm:node:1",
                "10 King Street, Toronto",
                "10",
                "King Street",
                Some("Toronto"),
                None,
            ))
            .expect("write address");
        writer
            .write_street(&street_record("osm:way:9", "King Street"))
            .expect("write street");
        writer
            .finish(&mut BuilderReport::default())
            .expect("finish");

        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");
        let hits = searcher
            .search(TextSearchOptions {
                query: "King Street".to_string(),
                limit: 10,
                layer: Some("street".to_string()),
            })
            .expect("search");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, 1);
        assert_eq!(hits[0].record.layer, "street");

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn searches_postcode_text() {
        let temp_dir = temp_pack_path("search-postcode");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write_postcode(&PostcodeRecord {
                postcode: "M5V".to_string(),
                geometry: point_geometry(-79.4, 43.6),
                source: DerivedSourceProvenance::osm_address_records(2),
            })
            .expect("write postcode");
        writer
            .finish(&mut BuilderReport::default())
            .expect("finish");

        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");
        let hits = searcher
            .search(TextSearchOptions {
                query: "M5V".to_string(),
                limit: 5,
                layer: Some("postcode".to_string()),
            })
            .expect("search");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, 0);
        assert_eq!(hits[0].record.layer, "postcode");

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn autocompletes_prefixes_and_hydrates_records_from_pack() {
        let temp_dir = temp_pack_path("autocomplete-prefix");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write_address(&address_record(
                "osm:node:1",
                "10 King Street, Toronto",
                "10",
                "King Street",
                Some("Toronto"),
                Some("M5V 1A1"),
            ))
            .expect("write king address");
        writer
            .write_address(&address_record(
                "osm:node:2",
                "20 Queen Street, Toronto",
                "20",
                "Queen Street",
                Some("Toronto"),
                None,
            ))
            .expect("write queen address");
        writer
            .finish(&mut BuilderReport::default())
            .expect("finish");

        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");
        let hits = searcher
            .autocomplete(TextAutocompleteOptions {
                query: "kin".to_string(),
                limit: 5,
                layer: None,
            })
            .expect("autocomplete");

        assert!(!hits.is_empty());
        assert_eq!(hits[0].record_id, 0);
        assert_eq!(hits[0].record.label, "10 King Street, Toronto, M5V 1A1");

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn autocompletes_multi_token_prefixes() {
        let temp_dir = temp_pack_path("autocomplete-multi-token");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write_address(&address_record(
                "osm:node:1",
                "10 King Street, Toronto",
                "10",
                "King Street",
                Some("Toronto"),
                None,
            ))
            .expect("write king address");
        writer
            .finish(&mut BuilderReport::default())
            .expect("finish");

        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");
        let hits = searcher
            .autocomplete(TextAutocompleteOptions {
                query: "king st".to_string(),
                limit: 5,
                layer: None,
            })
            .expect("autocomplete");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, 0);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn autocompletes_with_layer_filter() {
        let temp_dir = temp_pack_path("autocomplete-layer");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write_address(&address_record(
                "osm:node:1",
                "10 King Street, Toronto",
                "10",
                "King Street",
                Some("Toronto"),
                None,
            ))
            .expect("write address");
        writer
            .write_street(&street_record("osm:way:9", "King Street"))
            .expect("write street");
        writer
            .finish(&mut BuilderReport::default())
            .expect("finish");

        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");
        let hits = searcher
            .autocomplete(TextAutocompleteOptions {
                query: "kin".to_string(),
                limit: 10,
                layer: Some("street".to_string()),
            })
            .expect("autocomplete");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, 1);
        assert_eq!(hits[0].record.layer, "street");

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn autocompletes_postcode_but_not_standalone_house_number_prefixes() {
        let temp_dir = temp_pack_path("autocomplete-postcode-house-number");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write_address(&address_record(
                "osm:node:1",
                "221 Baker Street, London, NW1",
                "221",
                "Baker Street",
                Some("London"),
                Some("NW1 6XE"),
            ))
            .expect("write baker address");
        writer
            .finish(&mut BuilderReport::default())
            .expect("finish");

        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");
        let postcode_hits = searcher
            .autocomplete(TextAutocompleteOptions {
                query: "nw16".to_string(),
                limit: 5,
                layer: None,
            })
            .expect("postcode autocomplete");
        let number_hits = searcher
            .autocomplete(TextAutocompleteOptions {
                query: "221".to_string(),
                limit: 5,
                layer: None,
            })
            .expect("number autocomplete");

        assert_eq!(postcode_hits.len(), 1);
        assert!(number_hits.is_empty());
        assert_eq!(postcode_hits[0].record_id, 0);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn autocompletes_house_number_with_street_prefix() {
        let temp_dir = temp_pack_path("autocomplete-number-street-prefix");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write_address(&address_record(
                "osm:node:1",
                "221 Baker Street, London, NW1",
                "221",
                "Baker Street",
                Some("London"),
                Some("NW1 6XE"),
            ))
            .expect("write baker address");
        writer
            .finish(&mut BuilderReport::default())
            .expect("finish");

        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");
        let hits = searcher
            .autocomplete(TextAutocompleteOptions {
                query: "221 bak".to_string(),
                limit: 5,
                layer: None,
            })
            .expect("number plus street autocomplete");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, 0);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn autocomplete_ignores_blank_and_short_queries() {
        let temp_dir = temp_pack_path("autocomplete-short");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write_address(&address_record(
                "osm:node:1",
                "10 King Street, Toronto",
                "10",
                "King Street",
                Some("Toronto"),
                None,
            ))
            .expect("write address");
        writer
            .finish(&mut BuilderReport::default())
            .expect("finish");

        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        assert!(
            searcher
                .autocomplete(TextAutocompleteOptions {
                    query: " ".to_string(),
                    limit: 5,
                    layer: None,
                })
                .expect("blank autocomplete")
                .is_empty()
        );
        assert!(
            searcher
                .autocomplete(TextAutocompleteOptions {
                    query: "k".to_string(),
                    limit: 5,
                    layer: None,
                })
                .expect("single character autocomplete")
                .is_empty()
        );
        assert!(
            searcher
                .autocomplete(TextAutocompleteOptions {
                    query: "ki".to_string(),
                    limit: 5,
                    layer: None,
                })
                .expect("two character autocomplete")
                .is_empty()
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    fn temp_pack_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("open-geocode-{name}-{}", std::process::id()))
    }

    fn address_record(
        id: &str,
        _label: &str,
        number: &str,
        street: &str,
        locality: Option<&str>,
        postcode: Option<&str>,
    ) -> AddressRecord {
        let object_id = id
            .strip_prefix("osm:node:")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(1);
        AddressRecord {
            address: AddressComponents {
                number: number.to_string(),
                street: Some(street.to_string()),
                place: None,
                unit: None,
                locality: locality.map(str::to_string),
                region: None,
                postcode: postcode.map(str::to_string),
                country: None,
            },
            geometry: point_geometry(-79.0, 43.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance {
                dataset: "osm".to_string(),
                object_type: OsmObjectType::Node,
                object_id,
                tags: Some(BTreeMap::new()),
            },
        }
    }

    fn street_record(id: &str, label: &str) -> StreetRecord {
        let object_id = id
            .strip_prefix("osm:way:")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(9);
        StreetRecord {
            name: label.to_string(),
            geometry: point_geometry(-79.0, 43.0),
            representative_point: [-79.0, 43.0],
            source: SourceProvenance {
                dataset: "osm".to_string(),
                object_type: OsmObjectType::Way,
                object_id,
                tags: Some(BTreeMap::new()),
            },
        }
    }
}
