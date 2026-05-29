use std::path::Path;

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
    pack: PackReader,
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

const DEFAULT_SEARCH_LIMIT: usize = 10;
const MAX_AUTOCOMPLETE_LIMIT: usize = 20;
const MIN_AUTOCOMPLETE_QUERY_CHARS: usize = 3;
const AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS: u32 = 1_024;

impl PackTextSearcher {
    pub fn open(pack_path: impl AsRef<Path>) -> Result<Self> {
        let pack = PackReader::open(&pack_path)?;
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
        let index = open_text_index(&pack_path)?;
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

        let query = self.build_query(query_text, options.layer.as_deref())?;
        let searcher = self.reader.searcher();
        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(limit))
            .with_context(|| format!("failed to search text index for {query_text:?}"))?;

        self.hydrate_top_docs(top_docs)
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
