use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tantivy::{
    Index, IndexReader, Score, TantivyDocument, Term,
    collector::TopDocs,
    query::{BooleanQuery, Occur, Query, QueryParser, TermQuery},
    schema::{IndexRecordOption, Value},
};

use crate::{
    pack::{PackReader, RecordId},
    record::NormalizedRecord,
    text_index::{TextIndexFields, open_text_index},
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

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TextSearchHit {
    pub record_id: RecordId,
    pub score: Score,
    pub record: NormalizedRecord,
}

const DEFAULT_SEARCH_LIMIT: usize = 10;

impl PackTextSearcher {
    pub fn open(pack_path: impl AsRef<Path>) -> Result<Self> {
        let pack = PackReader::open(&pack_path)?;
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

        top_docs
            .into_iter()
            .map(|(score, doc_address)| {
                let document: TantivyDocument = searcher.doc(doc_address)?;
                let record_id = self.record_id_from_document(&document)?;
                let record = self.pack.read_record(record_id)?;
                Ok(TextSearchHit {
                    record_id,
                    score,
                    record,
                })
            })
            .collect()
    }

    fn build_query(&self, query_text: &str, layer: Option<&str>) -> Result<Box<dyn Query>> {
        let mut query_parser = QueryParser::for_index(&self.index, self.search_fields());
        query_parser.set_conjunction_by_default();
        query_parser.set_field_boost(self.fields.label_text, 3.0);
        query_parser.set_field_boost(self.fields.name_text, 2.5);
        query_parser.set_field_boost(self.fields.address_number, 2.0);
        query_parser.set_field_boost(self.fields.postcode_exact, 2.0);
        query_parser.set_field_boost(self.fields.street_text, 1.5);
        query_parser.set_field_boost(self.fields.postcode_text, 1.5);

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
            self.fields.all_text,
            self.fields.address_number,
            self.fields.street_text,
            self.fields.place_text,
            self.fields.unit_text,
            self.fields.locality_text,
            self.fields.region_text,
            self.fields.postcode_text,
            self.fields.postcode_exact,
            self.fields.country_text,
        ]
    }

    fn record_id_from_document(&self, document: &TantivyDocument) -> Result<RecordId> {
        document
            .get_first(self.fields.record_id)
            .and_then(|value| value.as_u64())
            .context("text index hit is missing stored record_id")
    }
}

fn effective_limit(limit: usize) -> usize {
    if limit == 0 {
        DEFAULT_SEARCH_LIMIT
    } else {
        limit
    }
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
            .write_record(NormalizedRecord::address(address_record(
                "osm:node:1",
                "10 King Street, Toronto",
                "10",
                "King Street",
                Some("Toronto"),
                Some("M5V 1A1"),
            )))
            .expect("write king address");
        writer
            .write_record(NormalizedRecord::address(address_record(
                "osm:node:2",
                "20 Queen Street, Toronto",
                "20",
                "Queen Street",
                Some("Toronto"),
                Some("M5V 1A1"),
            )))
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
        assert_eq!(hits[0].record.id(), "osm:node:1");
        assert_eq!(hits[0].record.label(), "10 King Street, Toronto");

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn filters_hits_by_layer() {
        let temp_dir = temp_pack_path("search-layer");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write_record(NormalizedRecord::address(address_record(
                "osm:node:1",
                "10 King Street, Toronto",
                "10",
                "King Street",
                Some("Toronto"),
                None,
            )))
            .expect("write address");
        writer
            .write_record(NormalizedRecord::street(street_record(
                "osm:way:9",
                "King Street",
            )))
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
        assert_eq!(hits[0].record.layer(), "street");

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn searches_postcode_text() {
        let temp_dir = temp_pack_path("search-postcode");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write_record(NormalizedRecord::postcode(PostcodeRecord {
                id: "derived:osm:postcode:M5V".to_string(),
                label: "M5V".to_string(),
                name: "M5V".to_string(),
                postcode: "M5V".to_string(),
                geometry: point_geometry(-79.4, 43.6),
                source: DerivedSourceProvenance::osm_address_records(2),
            }))
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
        assert_eq!(hits[0].record.layer(), "postcode");

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    fn temp_pack_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("open-geocode-{name}-{}", std::process::id()))
    }

    fn address_record(
        id: &str,
        label: &str,
        number: &str,
        street: &str,
        locality: Option<&str>,
        postcode: Option<&str>,
    ) -> AddressRecord {
        AddressRecord {
            id: id.to_string(),
            label: label.to_string(),
            name: label.to_string(),
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
                object_id: 1,
                tags: Some(BTreeMap::new()),
            },
        }
    }

    fn street_record(id: &str, label: &str) -> StreetRecord {
        StreetRecord {
            id: id.to_string(),
            label: label.to_string(),
            name: label.to_string(),
            geometry: point_geometry(-79.0, 43.0),
            representative_point: [-79.0, 43.0],
            source: SourceProvenance {
                dataset: "osm".to_string(),
                object_type: OsmObjectType::Way,
                object_id: 9,
                tags: Some(BTreeMap::new()),
            },
        }
    }
}
