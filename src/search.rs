use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    path::Path,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tantivy::{
    Index, IndexReader, Score, Term,
    collector::TopDocs,
    columnar::Column,
    query::{BooleanQuery, Occur, PhrasePrefixQuery, Query, QueryParser, TermQuery},
    schema::{Field, IndexRecordOption},
};

use crate::{
    pack::{PackReader, RecordId, RecordPointPrecision, RecordSummary},
    record::{Layer, OsmObjectType},
    spatial_index::SpatialIndexReader,
    text_index::{TextIndexFields, normalize_index_text, open_text_index},
};

pub struct PackTextSearcher {
    pack: Arc<PackReader>,
    spatial: SpatialIndexReader,
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
/// For a hit that states no postcode: postcode records within this radius are
/// compared with the requested postcode on their leading characters (a
/// Canadian FSA, a US sectional centre, the start of a UK outward code).
const POSTCODE_AREA_RADIUS_M: f64 = 10_000.0;
const POSTCODE_AREA_CHARS: usize = 3;
/// Hits examined per requested hit, so ties at the cutoff can be ordered.
const RANK_WINDOW_FACTOR: usize = 3;
const MAX_RANK_WINDOW: usize = 256;

impl PackTextSearcher {
    pub fn open(pack_path: impl AsRef<Path>) -> Result<Self> {
        Self::from_pack(Arc::new(PackReader::open(pack_path)?))
    }

    /// Open the text index using an existing shared pack reader.
    pub fn from_pack(pack: Arc<PackReader>) -> Result<Self> {
        let index = open_text_index(pack.container())?;
        let schema = index.schema();
        let fields = TextIndexFields::from_schema(&schema)?;
        let reader = index.reader().context("failed to open Tantivy reader")?;
        let spatial = SpatialIndexReader::open(pack.container(), pack.records().clone())?;
        Ok(Self {
            spatial,
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

        let (_, hits) =
            self.search_variants(query_text, options.layer.as_deref(), limit, |_| Ok(true))?;
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

        // Only the first accepted hit is returned, so `limit` just sizes the
        // window the context check looks through. Cutting to it first would drop
        // a correct hit ranked behind the same street in another town. The
        // window stops at MAX_RANK_WINDOW, so a correct hit behind that many
        // better-ranked rejects is still missed. Because the widened window can
        // accept a lower-ranked hit, an earlier query variant now wins over a
        // later rewrite more often than when only `limit` hits were checked.
        let (region, locality, postcode) = desired_address_context(&options);
        let window = if region.is_some() || locality.is_some() || postcode.is_some() {
            limit.max(MAX_RANK_WINDOW)
        } else {
            limit
        };

        for candidate in address_geocode_candidates(address, options.postcode.as_deref()) {
            let (query, hits) =
                self.search_variants(&candidate, options.layer.as_deref(), window, |hit| {
                    Ok(hit.record.point.is_some()
                        && self.hit_matches_address_context(hit, &options)?)
                })?;
            if let Some(hit) = hits.into_iter().next() {
                return Ok(Some(AddressGeocodeHit { query, hit }));
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
        self.ranked_hits(&query, &query_text, limit)
            .with_context(|| format!("failed to autocomplete text index for {query_text:?}"))
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

    /// Try the query as written, then normalized and expanded rewrites, and
    /// return the first variant with hits that `accept` keeps. A variant whose
    /// hits are all rejected falls through to the next one. Fails only when no
    /// variant could be parsed at all.
    fn search_variants(
        &self,
        query_text: &str,
        layer: Option<&str>,
        limit: usize,
        mut accept: impl FnMut(&TextSearchHit) -> Result<bool>,
    ) -> Result<(String, Vec<TextSearchHit>)> {
        let mut last_query = query_text.trim().to_string();
        let mut parse_error = None;
        let mut parsed_any = false;
        for variant in search_query_variants(query_text) {
            last_query = variant.clone();
            let query = match self.build_query(&variant, layer) {
                Ok(query) => query,
                Err(error) => {
                    parse_error = Some(error);
                    continue;
                }
            };
            parsed_any = true;
            let mut hits = Vec::new();
            for hit in self
                .ranked_hits(&query, &variant, limit)
                .with_context(|| format!("failed to search text index for {variant:?}"))?
            {
                if accept(&hit)? {
                    hits.push(hit);
                }
            }
            if !hits.is_empty() {
                return Ok((variant, hits));
            }
        }

        match parse_error {
            Some(error) if !parsed_any => Err(error),
            _ => Ok((last_query, Vec::new())),
        }
    }

    fn hit_matches_address_context(
        &self,
        hit: &TextSearchHit,
        options: &AddressGeocodeOptions,
    ) -> Result<bool> {
        let (desired_region, desired_locality, desired_postcode) = desired_address_context(options);
        if desired_region.is_none() && desired_locality.is_none() && desired_postcode.is_none() {
            return Ok(true);
        }

        if let Some(desired) = &desired_postcode
            && !self.hit_in_postcode(hit, desired)?
        {
            return Ok(false);
        }

        let Some(context) = self.pack.boundary_context(hit.record_id)? else {
            return Ok(desired_region.is_none() && desired_locality.is_none());
        };

        let mut admin_labels = Vec::new();
        {
            let tuple = context.admin_context;
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

        Ok(true)
    }

    /// Whether a hit can be in the requested (normalized) postcode.
    ///
    /// A hit that states a postcode must agree with it; either may be a prefix
    /// of the other, so "M5V" matches "M5V 1A1". A hit without one is judged by
    /// the postcode areas around it: if there are some nearby and none shares
    /// the requested postcode's leading characters, the hit is somewhere else.
    /// With no postcode data nearby there is nothing to contradict the row.
    fn hit_in_postcode(&self, hit: &TextSearchHit, desired: &str) -> Result<bool> {
        if let Some(postcode) = self.pack.records().postcode(hit.record_id)? {
            return Ok(
                normalized_postcode_for_match(Some(&postcode)).is_none_or(|postcode| {
                    postcode.starts_with(desired) || desired.starts_with(postcode.as_str())
                }),
            );
        }
        let Some(point) = hit.record.point else {
            return Ok(true);
        };
        let area: String = desired.chars().take(POSTCODE_AREA_CHARS).collect();
        let in_area = self.spatial.any_context_point(
            point.lon,
            point.lat,
            Layer::Postcode,
            POSTCODE_AREA_RADIUS_M,
            |record_id| {
                Ok(self
                    .pack
                    .records()
                    .postcode(record_id)?
                    .and_then(|postcode| normalized_postcode_for_match(Some(&postcode)))
                    .is_some_and(|postcode| {
                        postcode.starts_with(&area) || area.starts_with(postcode.as_str())
                    }))
            },
        )?;
        // No postcode data nearby means nothing contradicts the row.
        Ok(in_area.unwrap_or(true))
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

    /// The best `limit` hits. Hits whose score ties the cutoff are ordered by
    /// how closely their label matches the query, then by precision, then by
    /// source id, instead of by where records happen to sit in the Pack.
    fn ranked_hits(
        &self,
        query: &dyn Query,
        query_text: &str,
        limit: usize,
    ) -> Result<Vec<TextSearchHit>> {
        let searcher = self.reader.searcher();
        let window = limit
            .saturating_mul(RANK_WINDOW_FACTOR)
            .min(MAX_RANK_WINDOW)
            .max(limit);
        let mut top_docs = searcher.search(query, &TopDocs::with_limit(window))?;
        // Only hits that can still make the cut need hydrating: everything above
        // the cutoff score plus whatever ties it.
        if let Some(&(cutoff, _)) = top_docs.get(limit.saturating_sub(1)) {
            top_docs.retain(|(score, _)| *score >= cutoff);
        }

        let query_tokens = normalize_index_text(query_text)
            .map(|text| {
                text.split_whitespace()
                    .map(str::to_string)
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        let mut record_ids: HashMap<u32, Column<u64>> = HashMap::new();
        let mut hits = Vec::with_capacity(top_docs.len());
        for (score, doc_address) in top_docs {
            let column = match record_ids.entry(doc_address.segment_ord) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => entry.insert(
                    searcher
                        .segment_reader(doc_address.segment_ord)
                        .fast_fields()
                        .u64("record_id")?,
                ),
            };
            let record_id = column
                .values_for_doc(doc_address.doc_id)
                .next()
                .context("text index hit is missing fast record_id")?;
            let record = self.pack.record_summary(record_id)?;
            hits.push((
                TieBreak::new(&record, &query_tokens),
                TextSearchHit {
                    record_id,
                    score,
                    record,
                },
            ));
        }
        hits.sort_by(|(left_key, left), (right_key, right)| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left_key.cmp(right_key))
        });
        hits.truncate(limit);
        Ok(hits.into_iter().map(|(_, hit)| hit).collect())
    }
}

/// Secondary order for hits with equal scores.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TieBreak {
    /// Label words the query did not ask for: "342 Albert Street" beats
    /// "342 Prince Albert Street" for the query "342 albert street".
    extra_label_tokens: usize,
    /// Exact points before centroids before line midpoints.
    precision: u8,
    /// Nodes before ways before relations, then the lower id.
    source: (u8, i64),
    id: String,
}

impl TieBreak {
    fn new(record: &RecordSummary, query_tokens: &HashSet<String>) -> Self {
        let extra_label_tokens = normalize_index_text(&record.label)
            .map(|label| {
                label
                    .split_whitespace()
                    .filter(|token| !query_tokens.contains(*token))
                    .count()
            })
            .unwrap_or_default();
        let precision = match record.point.map(|point| point.precision) {
            Some(RecordPointPrecision::Point) => 0,
            Some(RecordPointPrecision::Centroid) => 1,
            Some(RecordPointPrecision::RepresentativePoint) => 2,
            None => 3,
        };
        let source = match record.source.object_type {
            Some(OsmObjectType::Node) => 0,
            Some(OsmObjectType::Way) => 1,
            Some(OsmObjectType::Relation) => 2,
            None => 3,
        };
        Self {
            extra_label_tokens,
            precision,
            source: (source, record.source.object_id.unwrap_or_default()),
            id: record.id.clone(),
        }
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

/// The region, locality and postcode a hit must match. Blank values count as
/// absent, so a hit is only checked against context that can reject it.
fn desired_address_context(
    options: &AddressGeocodeOptions,
) -> (Option<String>, Option<String>, Option<String>) {
    (
        normalized_for_match(options.region.as_deref()),
        normalized_for_match(options.locality.as_deref()),
        normalized_postcode_for_match(options.postcode.as_deref()),
    )
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
        pack::PackWriter,
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
    fn equal_scores_prefer_the_closest_label_then_points_then_lower_ids() {
        let temp_dir = temp_pack_path("search-ties");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        let mut prince =
            address_record("osm:node:1", "", "342", "Prince Albert Street", None, None);
        prince.geometry = point_geometry(-75.6, 45.4);
        let mut way = address_record("osm:node:9", "", "342", "Albert Street", None, None);
        way.source.object_type = OsmObjectType::Way;
        for record in [
            prince,
            way,
            address_record("osm:node:7", "", "342", "Albert Street", None, None),
            address_record("osm:node:5", "", "342", "Albert Street", None, None),
        ] {
            writer.write(&record.into(), None).expect("write");
        }
        writer.finish().expect("finish");

        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");
        let hits = searcher
            .search(TextSearchOptions {
                query: "342 Albert Street".to_string(),
                limit: 3,
                layer: None,
            })
            .expect("search");
        let ids = hits
            .iter()
            .map(|hit| hit.record.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["osm:node:5", "osm:node:7", "osm:way:9"]);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    fn geocode(
        searcher: &PackTextSearcher,
        address: &str,
        locality: Option<&str>,
        postcode: Option<&str>,
    ) -> Option<String> {
        searcher
            .geocode_address(AddressGeocodeOptions {
                address: address.to_string(),
                locality: locality.map(str::to_string),
                region: None,
                postcode: postcode.map(str::to_string),
                limit: 10,
                layer: None,
            })
            .expect("geocode")
            .map(|hit| hit.hit.record.id)
    }

    #[test]
    fn geocode_rejects_hits_in_a_different_postcode() {
        let temp_dir = temp_pack_path("geocode-postcode");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write(
                &address_record("osm:node:1", "", "10", "King Street", None, Some("M5V 1A1"))
                    .into(),
                None,
            )
            .expect("write");
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(
            geocode(&searcher, "10 King Street", None, Some("K7L 1A1")),
            None
        );
        assert_eq!(
            geocode(&searcher, "10 King Street", None, Some("m5v1a1")).as_deref(),
            Some("osm:node:1")
        );
        // A forward sortation area matches the full postcode it starts.
        assert_eq!(
            geocode(&searcher, "10 King Street", None, Some("M5V")).as_deref(),
            Some("osm:node:1")
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn geocode_filters_hits_ranked_beyond_the_requested_limit() {
        let temp_dir = temp_pack_path("geocode-filter-before-limit");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        // Twelve identical streets without a postcode tie with, and sort ahead
        // of, the one inside the requested postcode area. Only the postcode
        // record beside each of them tells them apart.
        let mut write_street = |id: &str, lon: f64, postcode: &str| {
            let mut record = address_record(id, "", "10", "King Street", None, None);
            record.geometry = point_geometry(lon, 43.0);
            writer.write(&record.into(), None).expect("address");
            writer
                .write(
                    &PostcodeRecord {
                        postcode: postcode.to_string(),
                        geometry: point_geometry(lon + 0.001, 43.0),
                        source: DerivedSourceProvenance::osm_address_records(5),
                    }
                    .into(),
                    None,
                )
                .expect("postcode");
        };
        for id in 1..=12 {
            write_street(&format!("osm:node:{id}"), -90.0 - id as f64, "M5V 1A1");
        }
        write_street("osm:node:99", -76.0, "K7L 1A1");
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        for limit in [1, 10] {
            let hit = searcher
                .geocode_address(AddressGeocodeOptions {
                    address: "10 King Street".to_string(),
                    locality: None,
                    region: None,
                    postcode: Some("K7L 1A1".to_string()),
                    limit,
                    layer: None,
                })
                .expect("geocode")
                .map(|hit| hit.hit.record.id);
            assert_eq!(hit.as_deref(), Some("osm:node:99"), "limit {limit}");
        }
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn geocode_treats_blank_context_as_absent() {
        let options = |postcode: &str| AddressGeocodeOptions {
            address: "10 King Street".to_string(),
            locality: Some("  ".to_string()),
            region: Some(String::new()),
            postcode: Some(postcode.to_string()),
            limit: 1,
            layer: None,
        };
        assert_eq!(desired_address_context(&options(" ")), (None, None, None));
        assert_eq!(
            desired_address_context(&options("k7l 1a1")).2.as_deref(),
            Some("k7l1a1")
        );
    }

    #[test]
    fn geocode_infers_the_postcode_area_of_hits_without_one() {
        let temp_dir = temp_pack_path("geocode-inferred-postcode");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        // An address without addr:postcode, inside postcode area K0J.
        let mut cobden = address_record("osm:node:1", "", "44", "Gould Street", None, None);
        cobden.geometry = point_geometry(-76.88, 45.63);
        writer.write(&cobden.into(), None).expect("cobden");
        writer
            .write(
                &PostcodeRecord {
                    postcode: "K0J 1K0".to_string(),
                    geometry: point_geometry(-76.881, 45.631),
                    source: DerivedSourceProvenance::osm_address_records(5),
                }
                .into(),
                None,
            )
            .expect("postcode");
        // An address far from every postcode record: its area is unknown.
        let mut remote = address_record("osm:node:2", "", "7", "Far Road", None, None);
        remote.geometry = point_geometry(-90.0, 50.0);
        writer.write(&remote.into(), None).expect("remote");
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(
            geocode(&searcher, "44 Gould Street", None, Some("M5V 1A1")),
            None
        );
        assert_eq!(
            geocode(&searcher, "44 Gould Street", None, Some("K0J 1K0")).as_deref(),
            Some("osm:node:1")
        );
        assert_eq!(
            geocode(&searcher, "7 Far Road", None, Some("M5V 1A1")).as_deref(),
            Some("osm:node:2")
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn geocode_accepts_hits_near_a_busier_neighbouring_postcode_area() {
        let temp_dir = temp_pack_path("geocode-postcode-border");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        let mut border = address_record("osm:node:1", "", "5", "Line Road", None, None);
        border.geometry = point_geometry(-79.0, 43.0);
        writer.write(&border.into(), None).expect("address");
        let mut postcode = |code: String, lon: f64| {
            writer
                .write(
                    &PostcodeRecord {
                        postcode: code,
                        geometry: point_geometry(lon, 43.0),
                        source: DerivedSourceProvenance::osm_address_records(1),
                    }
                    .into(),
                    None,
                )
                .expect("postcode");
        };
        // Twenty centres of the neighbouring area right next to the address,
        // and the address's own area 8 km away.
        for index in 0..20 {
            postcode(
                format!("B2B {index}A{index}"),
                -79.0 + 0.0001 * index as f64,
            );
        }
        postcode("A1A 1A1".to_string(), -78.9);
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(
            geocode(&searcher, "5 Line Road", None, Some("A1A 2B2")).as_deref(),
            Some("osm:node:1")
        );
        assert_eq!(
            geocode(&searcher, "5 Line Road", None, Some("C3C 3C3")),
            None
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn geocode_tries_expanded_variants_when_earlier_hits_fail_the_context() {
        use crate::{
            context::AdminContextTuple,
            pack::RecordContext,
            record::{PlaceLayer, PlaceRecord, Record},
        };

        let temp_dir = temp_pack_path("geocode-variants");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        let mut locality = |name: &str, object_id: i64| {
            writer
                .write(
                    &Record::Place(
                        PlaceLayer::Locality,
                        PlaceRecord {
                            name: name.to_string(),
                            place_type: "city".to_string(),
                            geometry: point_geometry(-79.0, 43.0),
                            source: SourceProvenance::osm(OsmObjectType::Node, object_id),
                        },
                    ),
                    None,
                )
                .expect("locality")
        };
        let hamilton = locality("Hamilton", 100);
        let toronto = locality("Toronto", 101);
        let in_locality = |record_id| {
            Some(RecordContext {
                admin_context: AdminContextTuple {
                    locality_record_id: Some(record_id),
                    ..AdminContextTuple::default()
                },
                flags: 0,
            })
        };
        writer
            .write(
                &address_record("osm:node:1", "", "10", "King St W", None, None).into(),
                in_locality(hamilton),
            )
            .expect("hamilton address");
        writer
            .write(
                &address_record("osm:node:2", "", "10", "King Street West", None, None).into(),
                in_locality(toronto),
            )
            .expect("toronto address");
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(
            geocode(&searcher, "10 King St W", Some("Toronto"), None).as_deref(),
            Some("osm:node:2")
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn search_reports_queries_that_cannot_be_parsed() {
        let temp_dir = temp_pack_path("search-parse-error");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write(
                &address_record("osm:node:1", "", "10", "King Street", None, None).into(),
                None,
            )
            .expect("write");
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        let error = searcher
            .search(TextSearchOptions {
                query: "(".to_string(),
                limit: 5,
                layer: None,
            })
            .expect_err("unparseable query");
        assert!(format!("{error:#}").contains("failed to parse search query"));
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn searches_and_hydrates_records_from_pack() {
        let temp_dir = temp_pack_path("search-hydrates");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write(
                &address_record(
                    "osm:node:1",
                    "10 King Street, Toronto",
                    "10",
                    "King Street",
                    Some("Toronto"),
                    Some("M5V 1A1"),
                )
                .into(),
                None,
            )
            .expect("write king address");
        writer
            .write(
                &address_record(
                    "osm:node:2",
                    "20 Queen Street, Toronto",
                    "20",
                    "Queen Street",
                    Some("Toronto"),
                    Some("M5V 1A1"),
                )
                .into(),
                None,
            )
            .expect("write queen address");
        writer.finish().expect("finish");

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
            .write(
                &address_record(
                    "osm:node:1",
                    "10 King Street, Toronto",
                    "10",
                    "King Street",
                    Some("Toronto"),
                    None,
                )
                .into(),
                None,
            )
            .expect("write address");
        writer
            .write(&street_record("osm:way:9", "King Street").into(), None)
            .expect("write street");
        writer.finish().expect("finish");

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
            .write(
                &PostcodeRecord {
                    postcode: "M5V".to_string(),
                    geometry: point_geometry(-79.4, 43.6),
                    source: DerivedSourceProvenance::osm_address_records(2),
                }
                .into(),
                None,
            )
            .expect("write postcode");
        writer.finish().expect("finish");

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
            .write(
                &address_record(
                    "osm:node:1",
                    "10 King Street, Toronto",
                    "10",
                    "King Street",
                    Some("Toronto"),
                    Some("M5V 1A1"),
                )
                .into(),
                None,
            )
            .expect("write king address");
        writer
            .write(
                &address_record(
                    "osm:node:2",
                    "20 Queen Street, Toronto",
                    "20",
                    "Queen Street",
                    Some("Toronto"),
                    None,
                )
                .into(),
                None,
            )
            .expect("write queen address");
        writer.finish().expect("finish");

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
            .write(
                &address_record(
                    "osm:node:1",
                    "10 King Street, Toronto",
                    "10",
                    "King Street",
                    Some("Toronto"),
                    None,
                )
                .into(),
                None,
            )
            .expect("write king address");
        writer.finish().expect("finish");

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
            .write(
                &address_record(
                    "osm:node:1",
                    "10 King Street, Toronto",
                    "10",
                    "King Street",
                    Some("Toronto"),
                    None,
                )
                .into(),
                None,
            )
            .expect("write address");
        writer
            .write(&street_record("osm:way:9", "King Street").into(), None)
            .expect("write street");
        writer.finish().expect("finish");

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
            .write(
                &address_record(
                    "osm:node:1",
                    "221 Baker Street, London, NW1",
                    "221",
                    "Baker Street",
                    Some("London"),
                    Some("NW1 6XE"),
                )
                .into(),
                None,
            )
            .expect("write baker address");
        writer.finish().expect("finish");

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
            .write(
                &address_record(
                    "osm:node:1",
                    "221 Baker Street, London, NW1",
                    "221",
                    "Baker Street",
                    Some("London"),
                    Some("NW1 6XE"),
                )
                .into(),
                None,
            )
            .expect("write baker address");
        writer.finish().expect("finish");

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
            .write(
                &address_record(
                    "osm:node:1",
                    "10 King Street, Toronto",
                    "10",
                    "King Street",
                    Some("Toronto"),
                    None,
                )
                .into(),
                None,
            )
            .expect("write address");
        writer.finish().expect("finish");

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
