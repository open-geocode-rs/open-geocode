use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    io,
    ops::{Bound, Range},
    path::Path,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tantivy::{
    DocId, IndexReader, Order, Score, Searcher, SegmentReader, Term,
    collector::{ScoreSegmentTweaker, ScoreTweaker, TopDocs},
    columnar::{Column, StrColumn},
    index::SegmentId,
    query::{
        BooleanQuery, BoostQuery, ConstScoreQuery, DisjunctionMaxQuery, Occur, PhrasePrefixQuery,
        Query, RangeQuery, TermQuery,
    },
    schema::{Field, IndexRecordOption},
};

use crate::{
    labels,
    pack::{PackReader, RecordId, RecordPoint, RecordPointPrecision, RecordSource, RecordSummary},
    record::{
        AddressComponents, DERIVED_FROM_ADDRESS_RECORDS, Layer, OsmObjectType, PoiRecord, Record,
    },
    spatial_index::SpatialIndexReader,
    text_index::{
        HOUSE_NUMBER_FIELD, INTERPOLATION_START_FIELD, POSTCODE_AREA_RADIUS_M, RANK_POSTCODE_FIELD,
        TextIndexFields, base_house_number, compact_postcode, expand_address_abbreviations,
        expand_final_direction, is_direction, is_numeric_token, is_street_type,
        normalize_index_text, open_text_index, street_search_text,
    },
};

pub struct PackTextSearcher {
    pack: Arc<PackReader>,
    spatial: SpatialIndexReader,
    reader: IndexReader,
    fields: TextIndexFields,
    /// The fast fields ranking reads, per segment. The Pack's index never
    /// changes, so they are opened once rather than per query.
    columns: Arc<HashMap<SegmentId, SegmentColumns>>,
}

#[derive(Clone)]
struct SegmentColumns {
    record_id: Column<u64>,
    house_number: Column<u64>,
    interpolation_start: Column<u64>,
    rank_postcode: Option<StrColumn>,
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
/// For a hit that states no postcode, the batch check compares the postcode
/// records within [`POSTCODE_AREA_RADIUS_M`] with the requested postcode on
/// their leading characters (a Canadian FSA, a US sectional centre, the start
/// of a UK outward code).
const POSTCODE_AREA_CHARS: usize = 3;
/// Hits ranked per requested hit: the text index keeps this window of the
/// best ranked records (see [`RankKey`]), and ties within it are broken by
/// [`TieBreak`].
const RANK_WINDOW_FACTOR: usize = 3;
const MAX_RANK_WINDOW: usize = 256;
/// How much a word found in each field weighs; the content field holds every
/// word of what a record is at weight 1, the context field every word of
/// where it is.
const NAME_BOOST: Score = 5.5;
const NUMBER_BOOST: Score = 2.0;
const POSTCODE_BOOST: Score = 2.0;
/// Stated house numbers read on each side of one that no record states: a
/// few, so the nearest of the same parity can be found and the numbers of a
/// same-named street elsewhere passed over.
const NEIGHBOUR_CANDIDATES: usize = 4;
/// How far an address point may lie from the street it is on. Address points
/// sit on their lots, and the lots front the street: 100 m covers the depth
/// of nearly every lot, including large buildings set back behind parking,
/// while a point placed between addresses on two different streets of one
/// name lies far from both.
const NEIGHBOUR_STREET_RADIUS_M: f64 = 100.0;

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
        let mut columns = HashMap::new();
        for segment in reader.searcher().segment_readers() {
            let fast_fields = segment.fast_fields();
            columns.insert(
                segment.segment_id(),
                SegmentColumns {
                    record_id: fast_fields.u64("record_id")?,
                    house_number: fast_fields.u64(HOUSE_NUMBER_FIELD)?,
                    interpolation_start: fast_fields.u64(INTERPOLATION_START_FIELD)?,
                    rank_postcode: fast_fields.str(RANK_POSTCODE_FIELD)?,
                },
            );
        }
        Ok(Self {
            spatial,
            pack,
            reader,
            fields,
            columns: Arc::new(columns),
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
            self.search_variants(query_text, options.layer.as_deref(), limit, |_, _| Ok(true))?;
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

        // The context fields join the query as context, so they rank the
        // records in the stated area first; the checks below then reject the
        // records they contradict.
        let query = [
            Some(address),
            options.locality.as_deref(),
            options.region.as_deref(),
            options.postcode.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
        let (query, hits) =
            self.search_variants(&query, options.layer.as_deref(), limit, |plan, hit| {
                // A street stands in for a house number it does not have in
                // search, but a geocoded row needs the address itself.
                Ok(hit.record.point.is_some()
                    && !(plan.house_number.is_some() && hit.record.layer == Layer::Street.as_str())
                    && self.hit_matches_address_context(hit, &options)?)
            })?;
        Ok(hits
            .into_iter()
            .next()
            .map(|hit| AddressGeocodeHit { query, hit }))
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
        let ranking = Ranking {
            query_words: query_text.split_whitespace().map(str::to_string).collect(),
            ..Ranking::default()
        };
        self.ranked_hits(&query, &ranking, limit)
            .with_context(|| format!("failed to autocomplete text index for {query_text:?}"))
    }

    /// The house number, then what a record is and where. See
    /// [`Self::house_number_query`] for what `pass` lets stand in for the
    /// number.
    fn build_query(&self, plan: &QueryPlan, pass: Pass, layer: Option<&str>) -> Box<dyn Query> {
        let mut clauses = Vec::new();
        if let Some(number) = &plan.house_number {
            clauses.push((Occur::Must, self.house_number_query(number, pass)));
        }
        clauses.extend(self.subject_clauses(plan, pass));
        for word in &plan.optional {
            clauses.push((Occur::Should, self.word_query(word)));
        }
        if let Some(layer) = layer.map(str::trim).filter(|layer| !layer.is_empty()) {
            clauses.push((Occur::Must, self.layer_query(layer)));
        }
        Box::new(BooleanQuery::new(clauses))
    }

    /// Every identifying word is required: of a record's street when the
    /// query asks for a house number on it, else of what the record is. A
    /// locality part is required of where it is unless `pass` is lenient; a
    /// postcode part only adds to the score of the records it matches, since
    /// a record's tags often state a different postcode than the one written.
    fn subject_clauses(&self, plan: &QueryPlan, pass: Pass) -> Vec<(Occur, Box<dyn Query>)> {
        let mut clauses = Vec::new();
        // Without commas a query does not say which of its words place it
        // ("Riverdale Farm Toronto"), so any of them may name an area. Nor
        // does one whose localities matched nothing ("Tim Hortons Toronto,
        // ON").
        let anywhere = plan.context.is_empty() || pass == Pass::Lenient;
        for word in &plan.required {
            let what = if plan.house_number.is_some() {
                self.street_word_query(word)
            } else {
                self.word_query(word)
            };
            let query = if anywhere {
                Box::new(DisjunctionMaxQuery::new(vec![what, self.area_query(word)]))
            } else {
                what
            };
            clauses.push((Occur::Must, query));
        }
        for part in &plan.context {
            let clause = if pass.requires_locality() && !is_postcode_part(part) {
                (Occur::Must, self.locality_part_query(part))
            } else {
                (Occur::Should, self.context_part_query(part))
            };
            clauses.push(clause);
        }
        clauses
    }

    /// Every word of a locality part, in where a record is.
    fn locality_part_query(&self, part: &[String]) -> Box<dyn Query> {
        Box::new(BooleanQuery::new(
            part.iter()
                .map(|word| (Occur::Must, self.area_term_query(word)))
                .collect(),
        ))
    }

    /// Any word of a context part in where a record is, or all of it as one
    /// postcode ("m6g 1b8").
    fn context_part_query(&self, part: &[String]) -> Box<dyn Query> {
        let mut alternatives = part
            .iter()
            .map(|word| (Occur::Should, self.area_query(word)))
            .collect::<Vec<_>>();
        if part.len() > 1 {
            let postcode = TermQuery::new(
                Term::from_field_text(self.fields.postcode_exact, &part.join(" ")),
                IndexRecordOption::Basic,
            );
            alternatives.push((
                Occur::Should,
                Box::new(BoostQuery::new(Box::new(postcode), POSTCODE_BOOST)),
            ));
        }
        Box::new(BooleanQuery::new(alternatives))
    }

    /// One word, in the fields that hold what a record is.
    fn word_query(&self, word: &str) -> Box<dyn Query> {
        self.fields_query(
            word,
            &[
                (
                    self.fields.name_text,
                    NAME_BOOST,
                    IndexRecordOption::WithFreqs,
                ),
                (self.fields.content_text, 1.0, IndexRecordOption::WithFreqs),
                (
                    self.fields.address_number,
                    NUMBER_BOOST,
                    IndexRecordOption::Basic,
                ),
            ],
        )
    }

    /// One word, in the street a record is on (or is).
    fn street_word_query(&self, word: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(self.fields.street_text, word),
            IndexRecordOption::WithFreqs,
        ))
    }

    /// One word, in the fields that hold where a record is.
    fn area_query(&self, word: &str) -> Box<dyn Query> {
        self.fields_query(
            word,
            &[
                (self.fields.context_text, 1.0, IndexRecordOption::WithFreqs),
                (
                    self.fields.postcode_exact,
                    POSTCODE_BOOST,
                    IndexRecordOption::Basic,
                ),
            ],
        )
    }

    fn area_term_query(&self, word: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(self.fields.context_text, word),
            IndexRecordOption::WithFreqs,
        ))
    }

    fn fields_query(
        &self,
        word: &str,
        fields: &[(Field, Score, IndexRecordOption)],
    ) -> Box<dyn Query> {
        Box::new(BooleanQuery::new(
            fields
                .iter()
                .map(|&(field, boost, option)| {
                    let term = Box::new(TermQuery::new(Term::from_field_text(field, word), option));
                    (
                        Occur::Should,
                        Box::new(BoostQuery::new(term, boost)) as Box<dyn Query>,
                    )
                })
                .collect(),
        ))
    }

    /// The house number a record states, or an interpolation range that
    /// contains it. In the later passes a street, which has no numbers, also
    /// stands in for it. Range and street score nothing for the number.
    fn house_number_query(&self, number: &HouseNumber, pass: Pass) -> Box<dyn Query> {
        let stated = TermQuery::new(
            Term::from_field_text(self.fields.address_number, &number.text),
            IndexRecordOption::Basic,
        );
        let mut alternatives: Vec<(Occur, Box<dyn Query>)> = vec![(
            Occur::Should,
            Box::new(BoostQuery::new(Box::new(stated), NUMBER_BOOST)),
        )];
        if let Some(number) = number.plain() {
            let bound = |field| Bound::Included(Term::from_field_u64(field, number.into()));
            let in_range = BooleanQuery::intersection(vec![
                Box::new(RangeQuery::new(
                    Bound::Unbounded,
                    bound(self.fields.interpolation_start),
                )),
                Box::new(RangeQuery::new(
                    bound(self.fields.interpolation_end),
                    Bound::Unbounded,
                )),
            ]);
            alternatives.push((
                Occur::Should,
                Box::new(ConstScoreQuery::new(Box::new(in_range), 0.0)),
            ));
        }
        if pass.street_stands_in() {
            alternatives.push((
                Occur::Should,
                Box::new(ConstScoreQuery::new(
                    self.layer_query(Layer::Street.as_str()),
                    0.0,
                )),
            ));
        }
        Box::new(BooleanQuery::new(alternatives))
    }

    fn layer_query(&self, layer: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(self.fields.layer, layer),
            IndexRecordOption::Basic,
        ))
    }

    /// Search the query as the index spells it, then without unit text, then
    /// both with the number a house number starts with ("407" of "407A"), and
    /// return the first variant with hits that `accept` keeps. Each [`Pass`]
    /// runs only when the ones before it found nothing: first the house number
    /// in the localities the query names, then the number placed between its
    /// stated neighbours there or a street standing in for it, then anything,
    /// with the localities only ranking. Fails when the query has no words to
    /// search.
    fn search_variants(
        &self,
        query_text: &str,
        layer: Option<&str>,
        limit: usize,
        mut accept: impl FnMut(&QueryPlan, &TextSearchHit) -> Result<bool>,
    ) -> Result<(String, Vec<TextSearchHit>)> {
        let mut variants = search_query_variants(query_text)
            .into_iter()
            .filter_map(|variant| Some((QueryPlan::parse(&variant)?, variant)))
            .collect::<Vec<_>>();
        let Some((_, last_query)) = variants.last() else {
            bail!("failed to parse search query {query_text:?}: it has no words to search");
        };
        let last_query = last_query.clone();
        let base_variants = variants
            .iter()
            .filter_map(|(plan, variant)| Some((plan.with_base_house_number()?, variant.clone())))
            .collect::<Vec<_>>();
        variants.extend(base_variants);
        let query_words: HashSet<String> = normalize_index_text(query_text)
            .map(|text| text.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();
        for pass in [Pass::Strict, Pass::Street, Pass::Lenient] {
            for (plan, variant) in &variants {
                if !pass.searches_anew(plan) {
                    continue;
                }
                let ranking = Ranking {
                    query_words: query_words.clone(),
                    house_number: plan.house_number.as_ref().and_then(HouseNumber::plain),
                    numbered: plan.house_number.is_some(),
                    postcode: plan.postcode(),
                };
                let mut hits = Vec::new();
                if pass == Pass::Street
                    && layer
                        .map(str::trim)
                        .filter(|layer| !layer.is_empty())
                        .is_none_or(|layer| layer == Layer::Address.as_str())
                    && let Some(estimate) = self.place_between_neighbours(plan, pass, &ranking)?
                    && accept(plan, &estimate)?
                {
                    hits.push(estimate);
                }
                let query = self.build_query(plan, pass, layer);
                for hit in self
                    .ranked_hits(&query, &ranking, limit)
                    .with_context(|| format!("failed to search text index for {variant:?}"))?
                {
                    if accept(plan, &hit)? {
                        hits.push(hit);
                    }
                }
                if !hits.is_empty() {
                    hits.truncate(limit);
                    return Ok((variant.clone(), hits));
                }
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
        let desired_postcode = options.postcode.as_deref().and_then(compact_postcode);
        if desired_region.is_none() && desired_locality.is_none() && desired_postcode.is_none() {
            return Ok(true);
        }

        if let Some(desired) = &desired_postcode
            && !self.hit_in_postcode(hit, desired)?
        {
            return Ok(false);
        }

        // A community inside a larger municipality (Seaforth in Huron East) has
        // no boundary of its own, but the record can state it.
        let locality_stated = match &desired_locality {
            Some(locality) => self.record_states_locality(hit.record_id, locality)?,
            None => false,
        };
        let desired_locality = desired_locality.filter(|_| !locality_stated);
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

    /// Whether the record's own address names `locality` (normalized) as its
    /// `addr:city` or `addr:place`.
    fn record_states_locality(&self, record_id: RecordId, locality: &str) -> Result<bool> {
        let (city, place) = match self.pack.records().record(record_id)? {
            Record::Address(record) => (record.address.locality, record.address.place),
            Record::Poi(PoiRecord {
                address: Some(address),
                ..
            }) => (address.locality, address.place),
            Record::Interpolation(record) => (record.address.locality, record.address.place),
            _ => return Ok(false),
        };
        Ok([city, place]
            .iter()
            .any(|name| normalized_for_match(name.as_deref()).as_deref() == Some(locality)))
    }

    /// Whether a hit can be in the requested (compacted) postcode.
    ///
    /// A hit that states a postcode must agree with it; either may be a prefix
    /// of the other, so "M5V" matches "M5V 1A1". A hit without one is judged by
    /// the postcode areas around it: if there are some nearby and none shares
    /// the requested postcode's leading characters, the hit is somewhere else.
    /// With no postcode data nearby there is nothing to contradict the row.
    fn hit_in_postcode(&self, hit: &TextSearchHit, desired: &str) -> Result<bool> {
        if let Some(postcode) = self.pack.records().postcode(hit.record_id)? {
            return Ok(compact_postcode(&postcode).is_none_or(|postcode| {
                postcode.starts_with(desired) || desired.starts_with(postcode.as_str())
            }));
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
                    .and_then(|postcode| compact_postcode(&postcode))
                    .is_some_and(|postcode| {
                        postcode.starts_with(&area) || area.starts_with(postcode.as_str())
                    }))
            },
        )?;
        // No postcode data nearby means nothing contradicts the row.
        Ok(in_area.unwrap_or(true))
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

    /// The best `limit` hits, ranked by [`RankKey`] over every record the
    /// query matches, and equal keys by [`TieBreak`].
    ///
    /// An interpolation hit is placed at the house number along its range, and
    /// left out when the number falls between its steps.
    fn ranked_hits(
        &self,
        query: &dyn Query,
        ranking: &Ranking,
        limit: usize,
    ) -> Result<Vec<TextSearchHit>> {
        let searcher = self.reader.searcher();
        let window = limit
            .saturating_mul(RANK_WINDOW_FACTOR)
            .min(MAX_RANK_WINDOW)
            .max(limit);
        let query_words = ranking
            .query_words
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        // When only the text score ranks, the key is the score, and the index
        // can skip the records that cannot make the window.
        let mut top_docs = if ranking.numbered || !ranking.postcode.forms.is_empty() {
            searcher.search(
                query,
                &TopDocs::with_limit(window).tweak_score(RankKeys {
                    ranking,
                    columns: &self.columns,
                }),
            )?
        } else {
            searcher
                .search(query, &TopDocs::with_limit(window))?
                .into_iter()
                .map(|(text_score, doc)| (RankKey::text(text_score), doc))
                .collect()
        };
        // Without a house number no hit is left out, so only the hits that
        // can still make the cut need hydrating: everything above the cutoff
        // key plus whatever ties it.
        if !ranking.numbered
            && let Some((cutoff, _)) = top_docs.get(limit.saturating_sub(1)).cloned()
        {
            top_docs.retain(|(key, _)| *key >= cutoff);
        }
        let mut hits = Vec::new();
        for (key, doc) in top_docs {
            let record_id = self
                .segment_columns(&searcher, doc.segment_ord)?
                .record_id
                .values_for_doc(doc.doc_id)
                .next()
                .context("text index hit is missing fast record_id")?;
            let mut record = self.pack.record_summary(record_id)?;
            if let Some(number) = ranking.house_number
                && record.layer == Layer::Interpolation.as_str()
                && !self.estimate_house(record_id, number, &mut record)?
            {
                continue;
            }
            hits.push((
                key.clone(),
                TieBreak::new(&record, &query_words),
                TextSearchHit {
                    record_id,
                    score: key.text_score,
                    record,
                },
            ));
        }
        hits.sort_by(|(left_key, left_tie, _), (right_key, right_tie, _)| {
            right_key
                .partial_cmp(left_key)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left_tie.cmp(right_tie))
        });
        hits.truncate(limit);
        Ok(hits.into_iter().map(|(_, _, hit)| hit).collect())
    }

    /// Place `number` on the interpolation range `record` summarizes: the
    /// point that far along its line, labelled as that address. `false` when
    /// the range does not contain the number.
    fn estimate_house(
        &self,
        record_id: RecordId,
        number: u32,
        record: &mut RecordSummary,
    ) -> Result<bool> {
        let Some(interpolation) = self.pack.interpolation(record_id)? else {
            return Ok(false);
        };
        let Some(fraction) = interpolation.interpolation.fraction_of(number) else {
            return Ok(false);
        };
        let Some([lon, lat]) = self.spatial.point_along_line(record_id, fraction)? else {
            return Ok(false);
        };
        record.point = Some(RecordPoint {
            lon,
            lat,
            precision: RecordPointPrecision::Estimated,
        });
        if let Some(label) = labels::estimated_address_label(number, &interpolation.address) {
            record.label = label;
        }
        Ok(true)
    }

    /// A house number no record states or range holds, placed between the
    /// nearest stated numbers below and above it on its street, in the
    /// localities `pass` requires (as Pelias interpolation estimates a
    /// missing number from the address points along a street). Never past the
    /// last number known on either side, so a street with numbers on one side
    /// of it only stays a street.
    ///
    /// Numbers of the house number's own parity are preferred: on most
    /// streets odd and even numbers face each other, so the same parity is
    /// the same side of the street. Both neighbours must state the same
    /// street, lie on a street of that name, and so must the point between
    /// them: two streets that share a name in one locality ("Victoria
    /// Street") never pair. Of the pairs that pass, the one in the postcode
    /// the query names wins, then the closest numbers.
    fn place_between_neighbours(
        &self,
        plan: &QueryPlan,
        pass: Pass,
        ranking: &Ranking,
    ) -> Result<Option<TextSearchHit>> {
        let Some(number) = plan.house_number.as_ref().map(|number| number.base) else {
            return Ok(None);
        };
        let searcher = self.reader.searcher();
        let lower = self.neighbours(
            &searcher,
            plan,
            pass,
            ranking,
            (Bound::Unbounded, Bound::Included(number)),
            Order::Desc,
        )?;
        if lower.is_empty() {
            return Ok(None);
        }
        let higher = self.neighbours(
            &searcher,
            plan,
            pass,
            ranking,
            (Bound::Excluded(number), Bound::Unbounded),
            Order::Asc,
        )?;
        let parity = |neighbour: &Neighbour| neighbour.number % 2 == number % 2;
        let mut pairs = lower
            .iter()
            .flat_map(|low| higher.iter().map(move |high| (low, high)))
            .filter(|(low, high)| low.street == high.street)
            .collect::<Vec<_>>();
        pairs.sort_by_key(|(low, high)| {
            (
                !(parity(low) && parity(high)),
                std::cmp::Reverse(low.postcode_agreement.min(high.postcode_agreement)),
                high.number - low.number,
            )
        });
        let mut on_street = HashMap::new();
        for (low, high) in pairs {
            let mut neighbours_on_street = true;
            for neighbour in [low, high] {
                let on = match on_street.entry(neighbour.record_id) {
                    Entry::Occupied(entry) => *entry.get(),
                    Entry::Vacant(entry) => {
                        *entry.insert(self.on_named_street(neighbour.point, &neighbour.street)?)
                    }
                };
                neighbours_on_street &= on;
            }
            if !neighbours_on_street {
                continue;
            }
            let fraction = f64::from(number - low.number) / f64::from(high.number - low.number);
            let point = [
                low.point[0] + fraction * (high.point[0] - low.point[0]),
                low.point[1] + fraction * (high.point[1] - low.point[1]),
            ];
            if self.on_named_street(point, &low.street)? {
                return Ok(Some(self.neighbour_estimate(number, low, high, point)?));
            }
        }
        Ok(None)
    }

    /// The stated house numbers nearest `number` in `range`, on the street
    /// and in the localities `plan` and `pass` require, nearest first.
    fn neighbours(
        &self,
        searcher: &Searcher,
        plan: &QueryPlan,
        pass: Pass,
        ranking: &Ranking,
        (lower, upper): (Bound<u32>, Bound<u32>),
        order: Order,
    ) -> Result<Vec<Neighbour>> {
        let term = |number: u32| Term::from_field_u64(self.fields.house_number, number.into());
        let mut clauses = self.subject_clauses(plan, pass);
        clauses.push((Occur::Must, self.layer_query(Layer::Address.as_str())));
        clauses.push((
            Occur::Must,
            Box::new(RangeQuery::new(lower.map(term), upper.map(term))),
        ));
        let docs = searcher.search(
            &BooleanQuery::new(clauses),
            &TopDocs::with_limit(NEIGHBOUR_CANDIDATES)
                .order_by_fast_field::<u64>(HOUSE_NUMBER_FIELD, order),
        )?;
        let mut neighbours = Vec::new();
        for (number, doc) in docs {
            let columns = self.segment_columns(searcher, doc.segment_ord)?;
            let record_id = columns
                .record_id
                .values_for_doc(doc.doc_id)
                .next()
                .context("text index hit is missing fast record_id")?;
            let address = match self.pack.records().record(record_id)? {
                Record::Address(record) => record.address,
                Record::Poi(PoiRecord {
                    address: Some(address),
                    ..
                }) => address,
                _ => continue,
            };
            let Some(street) = address
                .street
                .as_deref()
                .or(address.place.as_deref())
                .and_then(street_search_text)
            else {
                continue;
            };
            let (_, lon, lat) = self.pack.records().point(record_id)?;
            let mut postcode = String::new();
            let postcode_agreement = match &columns.rank_postcode {
                Some(column) => match column.term_ords(doc.doc_id).next() {
                    Some(ord) if column.ord_to_str(ord, &mut postcode)? => {
                        ranking.postcode.agreement(&postcode)
                    }
                    _ => 0,
                },
                None => 0,
            };
            neighbours.push(Neighbour {
                record_id,
                number: u32::try_from(number).context("house number is out of range")?,
                point: [lon, lat],
                street,
                address,
                postcode_agreement,
            });
        }
        Ok(neighbours)
    }

    fn segment_columns(&self, searcher: &Searcher, segment_ord: u32) -> Result<&SegmentColumns> {
        self.columns
            .get(&searcher.segment_reader(segment_ord).segment_id())
            .context("text index segment has no fast fields")
    }

    /// Whether a street record named `street` (as [`street_search_text`]
    /// spells it) passes within [`NEIGHBOUR_STREET_RADIUS_M`] of `point`.
    fn on_named_street(&self, [lon, lat]: [f64; 2], street: &str) -> Result<bool> {
        let mut seen = HashSet::new();
        for candidate in self.spatial.segment_candidates(
            lon,
            lat,
            Layer::Street,
            NEIGHBOUR_STREET_RADIUS_M,
            0,
        )? {
            if seen.insert(candidate.record_id)
                && street_search_text(&self.pack.records().summary(candidate.record_id)?.label)
                    .as_deref()
                    == Some(street)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The hit for `number` placed at `point` between `low` and `high`,
    /// labelled like the nearer of the two with the number in its place. It
    /// stands on the nearer record for the context checks of a geocoded row.
    fn neighbour_estimate(
        &self,
        number: u32,
        low: &Neighbour,
        high: &Neighbour,
        [lon, lat]: [f64; 2],
    ) -> Result<TextSearchHit> {
        let nearer = if number - low.number <= high.number - number {
            low
        } else {
            high
        };
        let address = AddressComponents {
            number: number.to_string(),
            unit: None,
            postcode: low
                .address
                .postcode
                .clone()
                .filter(|_| low.address.postcode == high.address.postcode),
            ..nearer.address.clone()
        };
        let id = |neighbour: &Neighbour| -> Result<String> {
            Ok(self.pack.records().summary(neighbour.record_id)?.id)
        };
        Ok(TextSearchHit {
            record_id: nearer.record_id,
            score: 0.0,
            record: RecordSummary {
                id: labels::estimated_address_id(number, &id(low)?, &id(high)?),
                layer: Layer::Address.as_str().to_string(),
                label: labels::address_label(&address),
                category: None,
                point: Some(RecordPoint {
                    lon,
                    lat,
                    precision: RecordPointPrecision::Estimated,
                }),
                source: RecordSource {
                    dataset: "osm".to_string(),
                    object_type: None,
                    object_id: None,
                    derived_from: Some(DERIVED_FROM_ADDRESS_RECORDS.to_string()),
                    record_count: Some(2),
                },
            },
        })
    }
}

/// A stated house number beside one no record states.
#[derive(Debug)]
struct Neighbour {
    record_id: RecordId,
    /// The number its house number starts with.
    number: u32,
    point: [f64; 2],
    /// Its street, as [`street_search_text`] spells it.
    street: String,
    address: AddressComponents,
    postcode_agreement: u8,
}

/// What ranks a hit, as a sequence of questions, each deciding only between
/// hits the ones before it tie: whether it answers the house number itself
/// (a stated number or an estimate) or only its street; how much of the
/// postcode the query names it shares; whether its point is stated or
/// estimated on a range; then how well its text matches. Every hit already
/// lies in the localities its pass requires. Compared field by field, larger
/// is better.
#[derive(Debug, Clone, PartialEq, PartialOrd)]
struct RankKey {
    house_level: bool,
    postcode_agreement: u8,
    stated: bool,
    text_score: Score,
}

impl RankKey {
    /// The key of a hit when the query asks for no house number or postcode.
    fn text(text_score: Score) -> Self {
        Self {
            house_level: true,
            postcode_agreement: 0,
            stated: true,
            text_score,
        }
    }
}

/// Computes each matched record's [`RankKey`] as the text index collects
/// them, so the window holds the best ranked records of all that match, not
/// the best scoring.
struct RankKeys<'a> {
    ranking: &'a Ranking,
    columns: &'a HashMap<SegmentId, SegmentColumns>,
}

struct SegmentRankKeys {
    numbered: bool,
    house_number: Column<u64>,
    interpolation_start: Column<u64>,
    postcode: Option<(StrColumn, Vec<Vec<Range<u64>>>)>,
}

impl ScoreTweaker<RankKey> for RankKeys<'_> {
    type Child = SegmentRankKeys;

    fn segment_tweaker(&self, segment: &SegmentReader) -> tantivy::Result<SegmentRankKeys> {
        let columns = self.columns.get(&segment.segment_id()).ok_or_else(|| {
            tantivy::TantivyError::InternalError("text index segment has no fast fields".into())
        })?;
        let postcode = match (
            &columns.rank_postcode,
            self.ranking.postcode.forms.is_empty(),
        ) {
            (Some(column), false) => {
                Some((column.clone(), self.ranking.postcode.ord_ranges(column)?))
            }
            _ => None,
        };
        Ok(SegmentRankKeys {
            numbered: self.ranking.numbered,
            house_number: columns.house_number.clone(),
            interpolation_start: columns.interpolation_start.clone(),
            postcode,
        })
    }
}

impl ScoreSegmentTweaker<RankKey> for SegmentRankKeys {
    fn score(&mut self, doc: DocId, text_score: Score) -> RankKey {
        // A numbered query matches a record by its stated number, by a range
        // that holds the number, or as a street that stands in for it.
        let mut key = RankKey::text(text_score);
        if self.numbered && self.house_number.first(doc).is_none() {
            key.house_level = self.interpolation_start.first(doc).is_some();
            key.stated = false;
        }
        if let Some((column, ranges)) = &self.postcode {
            key.postcode_agreement = column
                .term_ords(doc)
                .map(|ord| QueryPostcode::agreement_of_ord(ranges, ord))
                .max()
                .unwrap_or(0);
        }
        key
    }
}

/// Secondary order for hits with equal ranks.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TieBreak {
    /// Words of the record's own label the query did not ask for, as both
    /// were written: "342 Albert Street" beats "342 Prince Albert Street" for
    /// the query "342 albert street".
    extra_label_tokens: usize,
    /// Exact points before centroids before estimates before line midpoints.
    precision: u8,
    /// Nodes before ways before relations, then the lower id.
    source: (u8, i64),
    id: String,
}

impl TieBreak {
    fn new(record: &RecordSummary, query_words: &HashSet<&str>) -> Self {
        let extra_label_tokens = normalize_index_text(&record.label)
            .map(|label| {
                label
                    .split_whitespace()
                    .filter(|word| !query_words.contains(word))
                    .count()
            })
            .unwrap_or_default();
        let precision = match record.point.map(|point| point.precision) {
            Some(RecordPointPrecision::Point) => 0,
            Some(RecordPointPrecision::Centroid) => 1,
            Some(RecordPointPrecision::Estimated) => 2,
            Some(RecordPointPrecision::RepresentativePoint) => 3,
            None => 4,
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

/// The retrieval passes of [`PackTextSearcher::search_variants`], in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// The house number, stated or on a range, in every named locality.
    Strict,
    /// The number between its stated neighbours, or a street standing in for
    /// it, still in every locality.
    Street,
    /// The localities only rank, and a street may stand in for the number.
    Lenient,
}

impl Pass {
    fn requires_locality(self) -> bool {
        self != Self::Lenient
    }

    fn street_stands_in(self) -> bool {
        self != Self::Strict
    }

    /// Whether the pass searches `plan` differently from the passes before
    /// it: a street stands in only for a house number, and a query that names
    /// no locality has none to relax.
    fn searches_anew(self, plan: &QueryPlan) -> bool {
        match self {
            Self::Strict => true,
            Self::Street => plan.house_number.is_some(),
            Self::Lenient => plan.context.iter().any(|part| !is_postcode_part(part)),
        }
    }
}

/// What [`PackTextSearcher::ranked_hits`] ranks by besides the text score.
#[derive(Debug, Clone, Default)]
struct Ranking {
    /// The query's words as written, before abbreviations were expanded, to
    /// compare with records' own labels.
    query_words: HashSet<String>,
    /// Whether the query asks for a house number.
    numbered: bool,
    /// The house number, when interpolation ranges can estimate it.
    house_number: Option<u32>,
    postcode: QueryPostcode,
}

/// The postcode a query names, to rank records by how much of it their own
/// postcode shares from its start: "M5C 2A1" agrees with "M5C 1B2" on three
/// characters, with "M4C 1B2" on one. Longer agreement ranks first, whatever
/// the country's postcode format.
#[derive(Debug, Clone, Default)]
struct QueryPostcode {
    /// Compacted forms of the postcode part: all of it, and from its first
    /// word with a digit when other words lead ("on m5c 2a1" is "m5c2a1").
    forms: Vec<String>,
}

impl QueryPostcode {
    fn agreement(&self, postcode: &str) -> u8 {
        self.forms
            .iter()
            .map(|form| {
                form.chars()
                    .zip(postcode.chars())
                    .take_while(|(left, right)| left == right)
                    .count()
            })
            .max()
            .unwrap_or(0)
            .min(u8::MAX.into()) as u8
    }

    /// For each form, the dictionary ordinals of the postcodes that share its
    /// first 1, 2, ... characters. The postcodes sharing a prefix are a run of
    /// the sorted dictionary, so a record's agreement is the longest prefix
    /// whose run holds its ordinal.
    fn ord_ranges(&self, column: &StrColumn) -> io::Result<Vec<Vec<Range<u64>>>> {
        let dictionary = column.dictionary();
        self.forms
            .iter()
            .map(|form| {
                form.char_indices()
                    .map(|(index, character)| {
                        let prefix = &form.as_bytes()[..index + character.len_utf8()];
                        // No UTF-8 byte is 0xFF, so every postcode with the
                        // prefix sorts before the prefix followed by it.
                        let mut end = prefix.to_vec();
                        end.push(u8::MAX);
                        let (start, end) = dictionary.term_bounds_to_ord(
                            Bound::Included(prefix.to_vec()),
                            Bound::Excluded(end),
                        )?;
                        Ok(ord_range(start, end))
                    })
                    .collect()
            })
            .collect()
    }

    fn agreement_of_ord(ranges: &[Vec<Range<u64>>], ord: u64) -> u8 {
        ranges
            .iter()
            .map(|prefixes| {
                prefixes
                    .iter()
                    .take_while(|range| range.contains(&ord))
                    .count()
            })
            .max()
            .unwrap_or(0)
            .min(u8::MAX.into()) as u8
    }
}

fn ord_range(start: Bound<u64>, end: Bound<u64>) -> Range<u64> {
    let start = match start {
        Bound::Included(ord) => ord,
        Bound::Excluded(ord) => ord.saturating_add(1),
        Bound::Unbounded => 0,
    };
    let end = match end {
        Bound::Included(ord) => ord.saturating_add(1),
        Bound::Excluded(ord) => ord,
        Bound::Unbounded => u64::MAX,
    };
    start..end
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

/// A query's words by role. The first comma-separated part with words other
/// than unit text names what is sought, and its words are required. The
/// parts after it ("Toronto", "M6G 1B8") place it, and unit text ("unit 602")
/// and words after the street with digits ("fl36") only rank (see
/// [`PackTextSearcher::build_query`]).
#[derive(Debug, Default, PartialEq)]
struct QueryPlan {
    /// The leading required word when it reads as a house number ("670",
    /// "221b", "993_5") and other required words follow.
    house_number: Option<HouseNumber>,
    required: Vec<String>,
    optional: Vec<String>,
    /// The words of each part after the subject, in order.
    context: Vec<Vec<String>>,
}

impl QueryPlan {
    /// Plan a normalized query whose parts are separated by commas.
    fn parse(query: &str) -> Option<Self> {
        let parts = query
            .split(',')
            .map(|part| part.split_whitespace().collect::<Vec<_>>())
            .filter(|words| !words.is_empty())
            .collect::<Vec<_>>();
        let subject = parts
            .iter()
            .position(|words| !split_unit_words(words).0.is_empty())?;
        let mut plan = Self::default();
        for (index, words) in parts.iter().enumerate() {
            if index > subject && is_unit_part(words) {
                plan.optional
                    .extend(words.iter().map(|word| word.to_string()));
                continue;
            }
            let (identifying, unit) = split_unit_words(words);
            plan.optional.extend(unit);
            if index == subject {
                plan.required = identifying;
            } else if index > subject && !identifying.is_empty() {
                plan.context.push(identifying);
            }
        }
        if plan.required.len() > 1
            && let Some(number) = HouseNumber::parse(&plan.required[0])
        {
            plan.house_number = Some(number);
            plan.required.remove(0);
        }
        // The street name ends at its type or direction ("Wellington Street
        // West"), unless the type leads it ("Highway 7"); words with digits
        // after that are a unit or floor ("FL36", "G120", a trailing "2").
        if let Some(end) = plan
            .required
            .iter()
            .enumerate()
            .position(|(index, word)| index > 0 && (is_street_type(word) || is_direction(word)))
        {
            let after = plan.required.split_off(end + 1);
            for word in after {
                if word.chars().any(|character| character.is_ascii_digit()) {
                    plan.optional.push(word);
                } else {
                    plan.required.push(word);
                }
            }
        }
        if plan.house_number.is_some() {
            expand_final_direction(&mut plan.required);
        }
        Some(plan)
    }

    /// The plan with the number its house number starts with in place of a
    /// house number with a letter or fraction ("407" for "407a"). `None` when
    /// there is no such number to fall back to.
    fn with_base_house_number(&self) -> Option<Self> {
        let number = self.house_number.as_ref()?;
        if number.plain().is_some() {
            return None;
        }
        Some(Self {
            house_number: Some(HouseNumber {
                text: number.base.to_string(),
                base: number.base,
            }),
            required: self.required.clone(),
            optional: self.optional.clone(),
            context: self.context.clone(),
        })
    }

    /// The postcode the first postcode part names.
    fn postcode(&self) -> QueryPostcode {
        let Some(part) = self.context.iter().find(|part| is_postcode_part(part)) else {
            return QueryPostcode::default();
        };
        let first_digit = part
            .iter()
            .position(|word| word.chars().any(|character| character.is_ascii_digit()))
            .unwrap_or_default();
        QueryPostcode {
            forms: unique_strings(vec![part.concat(), part[first_digit..].concat()]),
        }
    }
}

/// A house number as the address number field indexes it ("407a", "993 5").
#[derive(Debug, Clone, PartialEq)]
struct HouseNumber {
    text: String,
    /// The number it starts with.
    base: u32,
}

impl HouseNumber {
    /// Digits with at most one letter after them: "670", "221b", but not
    /// "1st". A fraction joined by [`join_fractional_house_number`] reads as
    /// one ("993_5").
    fn parse(word: &str) -> Option<Self> {
        let digits = word.trim_end_matches(|character: char| character.is_alphabetic());
        if word[digits.len()..].chars().count() > 1
            || !digits.split('_').all(|group| {
                !group.is_empty() && group.chars().all(|character| character.is_ascii_digit())
            })
        {
            return None;
        }
        Some(Self {
            text: word.replace('_', " "),
            base: base_house_number(word)?,
        })
    }

    /// The number, when it is digits alone.
    fn plain(&self) -> Option<u32> {
        is_numeric_token(&self.text).then_some(self.base)
    }
}

/// A context part with a digit names a postcode ("M6G 1B8", "SW1A", "10115"),
/// not a locality.
fn is_postcode_part(part: &[String]) -> bool {
    part.iter()
        .any(|word| word.chars().any(|character| character.is_ascii_digit()))
}

/// A comma-separated part that is a unit designator and its value, though the
/// value has no digit ("unit b", "suite b"). The short designators "ste",
/// "fl" and "rm" also abbreviate place names (Sainte, Florida), so only they
/// need a digit to be read as a unit.
fn is_unit_part<T: AsRef<str>>(words: &[T]) -> bool {
    let [designator, value] = words else {
        return false;
    };
    let designator = designator.as_ref();
    is_unit_designator(designator)
        && (is_unit_value_token(value.as_ref()) || !matches!(designator, "ste" | "fl" | "rm"))
}

/// Words that identify a place, and unit text: a unit designator followed by
/// a value with a digit ("unit 602", "suite 4b").
fn split_unit_words(words: &[&str]) -> (Vec<String>, Vec<String>) {
    let mut identifying = Vec::new();
    let mut unit = Vec::new();
    let mut index = 0;
    while index < words.len() {
        if is_unit_designator(words[index])
            && words
                .get(index + 1)
                .is_some_and(|value| is_unit_value_token(value))
        {
            unit.extend([words[index].to_string(), words[index + 1].to_string()]);
            index += 2;
        } else {
            identifying.push(words[index].to_string());
            index += 1;
        }
    }
    (identifying, unit)
}

/// The query as the index spells it (normalized, abbreviations expanded),
/// then without unit text, each keeping its comma-separated parts.
fn search_query_variants(query_text: &str) -> Vec<String> {
    let Some(expanded) = map_query_parts(query_text, |part| {
        let (number, rest) = join_fractional_house_number(part);
        let rest = normalize_index_text(rest).map(|rest| expand_address_abbreviations(&rest));
        match (number, rest) {
            (Some(number), Some(rest)) => Some(format!("{number} {rest}")),
            (number, rest) => number.or(rest),
        }
    }) else {
        return Vec::new();
    };
    let without_unit = expanded
        .split(',')
        .enumerate()
        .filter(|(index, part)| {
            *index == 0 || !is_unit_part(&part.split_whitespace().collect::<Vec<_>>())
        })
        .filter_map(|(_, part)| strip_unit_terms(part))
        .collect::<Vec<_>>();
    unique_strings(
        [
            Some(expanded),
            (!without_unit.is_empty()).then(|| without_unit.join(", ")),
        ]
        .into_iter()
        .flatten()
        .collect(),
    )
}

/// A part that starts with a house number with a fraction ("993.5 Bloor",
/// "1384 1/2 Queen"), split into the number as one word, its parts joined by
/// "_" ("993_5", which [`HouseNumber::parse`] reads), and the rest of the
/// part. Normalizing would split the number into two.
fn join_fractional_house_number(part: &str) -> (Option<String>, &str) {
    let trimmed = part.trim_start();
    let digits = |text: &str| {
        text.find(|character: char| !character.is_ascii_digit())
            .unwrap_or(text.len())
    };
    let whole = digits(trimmed);
    let after = &trimmed[whole..];
    let fraction = if let Some(decimals) = after.strip_prefix('.') {
        let count = digits(decimals);
        (count > 0).then(|| (decimals[..count].to_string(), &decimals[count..]))
    } else {
        let spaced = after.trim_start();
        let numerator = digits(spaced);
        spaced[numerator..]
            .strip_prefix('/')
            .and_then(|denominator| {
                let count = digits(denominator);
                (numerator > 0 && count > 0 && spaced.len() < after.len()).then(|| {
                    (
                        format!("{}_{}", &spaced[..numerator], &denominator[..count]),
                        &denominator[count..],
                    )
                })
            })
    };
    match fraction {
        Some((fraction, rest))
            if whole > 0 && rest.chars().next().is_none_or(char::is_whitespace) =>
        {
            (Some(format!("{}_{fraction}", &trimmed[..whole])), rest)
        }
        _ => (None, part),
    }
}

/// Apply `map` to each comma-separated part of a query, dropping the parts it
/// empties. `None` when no part is left.
fn map_query_parts(query: &str, map: impl Fn(&str) -> Option<String>) -> Option<String> {
    let parts = query.split(',').filter_map(&map).collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(", "))
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

fn normalized_for_match(value: Option<&str>) -> Option<String> {
    normalize_index_text(value?.trim())
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
        text_index::PostcodeAreas,
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
    fn pois_named_after_a_place_or_street_do_not_outrank_it() {
        use crate::{
            context::AdminContextTuple,
            pack::RecordContext,
            record::{PlaceLayer, PlaceRecord, PoiRecord, Record},
        };

        let temp_dir = temp_pack_path("search-poi-repeats");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        let township = writer
            .write(
                &Record::Place(
                    PlaceLayer::Locality,
                    PlaceRecord {
                        name: "Muskoka Lakes Township".to_string(),
                        place_type: "admin_level:8".to_string(),
                        geometry: point_geometry(-79.5, 45.1),
                        source: SourceProvenance::osm(OsmObjectType::Relation, 1),
                    },
                ),
                None,
            )
            .expect("township");
        writer.set_context_names(
            [(township, "Muskoka Lakes Township".to_string())]
                .into_iter()
                .collect(),
        );
        let in_township = Some(RecordContext {
            admin_context: AdminContextTuple {
                locality_record_id: Some(township),
                ..AdminContextTuple::default()
            },
            flags: 0,
        });
        let poi = |object_id, name: &str, address| PoiRecord {
            name: name.to_string(),
            category: "amenity:fire_station".to_string(),
            address,
            geometry: point_geometry(-79.5, 45.1),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(OsmObjectType::Node, object_id),
        };
        writer
            .write(
                &poi(2, "Muskoka Lakes Township Fire Station", None).into(),
                in_township,
            )
            .expect("fire station");
        writer
            .write(
                &address_record("osm:node:3", "", "4", "Walmer Road", Some("Toronto"), None).into(),
                None,
            )
            .expect("address");
        let parkette_address = address_record(
            "osm:node:4",
            "",
            "227",
            "Walmer Road",
            Some("Toronto"),
            None,
        )
        .address;
        writer
            .write(
                &poi(4, "Walmer Road Parkette", Some(parkette_address)).into(),
                None,
            )
            .expect("parkette");
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");
        let top = |query: &str| {
            searcher
                .search(TextSearchOptions {
                    query: query.to_string(),
                    limit: 5,
                    layer: None,
                })
                .expect("search")[0]
                .record
                .id
                .clone()
        };

        assert_eq!(top("Muskoka Lakes Township"), "osm:relation:1");
        assert_eq!(top("Walmer Road"), "osm:node:3");
        assert_eq!(top("Walmer Road Parkette"), "osm:node:4");
        assert_eq!(top("Fire Station, Muskoka Lakes Township"), "osm:node:2");
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

    fn top_ids(searcher: &PackTextSearcher, query: &str) -> Vec<String> {
        searcher
            .search(TextSearchOptions {
                query: query.to_string(),
                limit: 5,
                layer: None,
            })
            .expect("search")
            .into_iter()
            .map(|hit| hit.record.id)
            .collect()
    }

    fn queen_street_line() -> geojson::Geometry {
        geojson::Geometry::new(geojson::GeometryValue::LineString {
            coordinates: vec![vec![-79.0, 43.0].into(), vec![-79.0, 43.001].into()],
        })
    }

    #[test]
    fn a_street_stands_in_only_for_a_house_number_no_record_has() {
        let temp_dir = temp_pack_path("search-lenient");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write(
                &address_record("osm:node:1", "", "10", "King Street", None, None).into(),
                None,
            )
            .expect("address");
        writer
            .write(&street_record("osm:way:9", "King Street").into(), None)
            .expect("street");
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(top_ids(&searcher, "10 King Street"), vec!["osm:node:1"]);
        // No record has 99: the street answers, never the address with 10.
        assert_eq!(top_ids(&searcher, "99 King St"), vec!["osm:way:9"]);
        // A geocoded row needs the address itself, not its street.
        assert_eq!(geocode(&searcher, "99 King Street", None, None), None);
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn stated_numbers_rank_before_range_estimates_before_streets() {
        use crate::record::{
            InterpolationAddressComponents, InterpolationRange, InterpolationRecord,
        };

        let temp_dir = temp_pack_path("search-tiers");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        let mut street = street_record("osm:way:30", "Queen Street");
        street.geometry = queen_street_line();
        writer.write(&street.into(), None).expect("street");
        writer
            .write(
                &InterpolationRecord {
                    address: InterpolationAddressComponents {
                        street: Some("Queen Street".to_string()),
                        place: None,
                        locality: None,
                        region: None,
                        postcode: None,
                        country: None,
                    },
                    interpolation: InterpolationRange {
                        kind: "even".to_string(),
                        start: 2,
                        end: 98,
                        step: 2,
                    },
                    anchor_node_ids: [2, 98],
                    geometry: queen_street_line(),
                    representative_point: [-79.0, 43.0005],
                    source: SourceProvenance::osm(OsmObjectType::Way, 20),
                }
                .into(),
                None,
            )
            .expect("interpolation");
        writer
            .write(
                &address_record("osm:node:10", "", "10", "Queen Street", None, None).into(),
                None,
            )
            .expect("address");
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "10 Queen Street"),
            vec!["osm:node:10", "osm:way:20:interp:2-98"],
            "the stated number first, then the range that contains it"
        );

        let hits = searcher
            .search(TextSearchOptions {
                query: "50 Queen Street".to_string(),
                limit: 5,
                layer: None,
            })
            .expect("search");
        assert_eq!(
            hits.len(),
            1,
            "the street does not answer a number a range has"
        );
        let estimate = &hits[0].record;
        assert_eq!(estimate.label, "50 Queen Street");
        let point = estimate.point.expect("point");
        assert_eq!(point.precision, RecordPointPrecision::Estimated);
        assert!((point.lat - 43.0005).abs() < 1e-6, "{point:?}");
        assert!((point.lon + 79.0).abs() < 1e-6, "{point:?}");

        // An odd number is not on an even range: only the street is left.
        assert_eq!(top_ids(&searcher, "51 Queen Street"), vec!["osm:way:30"]);
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn named_localities_are_required_before_they_only_rank() {
        use crate::{
            context::AdminContextTuple,
            pack::RecordContext,
            record::{
                InterpolationAddressComponents, InterpolationRange, InterpolationRecord,
                PlaceLayer, PlaceRecord, Record,
            },
        };

        let temp_dir = temp_pack_path("search-localities");
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
                            source: SourceProvenance::osm(OsmObjectType::Relation, object_id),
                        },
                    ),
                    None,
                )
                .expect("locality")
        };
        let toronto = locality("Toronto", 100);
        let st_catharines = locality("St. Catharines", 101);
        writer.set_context_names(
            [
                (toronto, "Toronto".to_string()),
                (st_catharines, "St. Catharines".to_string()),
            ]
            .into_iter()
            .collect(),
        );
        let in_locality = |record_id| {
            Some(RecordContext {
                admin_context: AdminContextTuple {
                    locality_record_id: Some(record_id),
                    ..AdminContextTuple::default()
                },
                flags: 0,
            })
        };
        // The same number elsewhere, and a range that has it in Toronto.
        writer
            .write(
                &address_record("osm:node:1", "", "182", "Baldwin Street", None, None).into(),
                None,
            )
            .expect("baldwin elsewhere");
        writer
            .write(
                &InterpolationRecord {
                    address: InterpolationAddressComponents {
                        street: Some("Baldwin Street".to_string()),
                        place: None,
                        locality: None,
                        region: None,
                        postcode: None,
                        country: None,
                    },
                    interpolation: InterpolationRange {
                        kind: "even".to_string(),
                        start: 100,
                        end: 200,
                        step: 2,
                    },
                    anchor_node_ids: [100, 200],
                    geometry: queen_street_line(),
                    representative_point: [-79.0, 43.0005],
                    source: SourceProvenance::osm(OsmObjectType::Way, 20),
                }
                .into(),
                in_locality(toronto),
            )
            .expect("baldwin range in toronto");
        // A road in a town named like the street a query asks for.
        writer
            .write(
                &address_record(
                    "osm:node:3",
                    "",
                    "2228",
                    "Gibraltar Road",
                    Some("Kingston"),
                    None,
                )
                .into(),
                None,
            )
            .expect("kingston address");
        writer
            .write(
                &street_record("osm:way:40", "Kingston Road").into(),
                in_locality(toronto),
            )
            .expect("kingston road");
        writer
            .write(
                &address_record("osm:node:5", "", "99", "Queen Street", None, None).into(),
                None,
            )
            .expect("queen elsewhere");
        writer
            .write(
                &street_record("osm:way:50", "Queen Street").into(),
                in_locality(toronto),
            )
            .expect("queen street");
        writer
            .write(
                &address_record("osm:node:6", "", "5", "Ontario Street", None, None).into(),
                in_locality(st_catharines),
            )
            .expect("st catharines address");
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        // The house number in the named town, though only a range has it.
        assert_eq!(
            top_ids(&searcher, "182 Baldwin St, Toronto, M5T 1L8"),
            vec!["osm:way:20:interp:100-200"]
        );
        // A postcode part is not required: the stated number ranks first.
        assert_eq!(
            top_ids(&searcher, "182 Baldwin St, M5T 1L8")[0],
            "osm:node:1"
        );
        // A locality that matches nothing only ranks.
        assert_eq!(
            top_ids(&searcher, "182 Baldwin St, Atlantis")[0],
            "osm:node:1"
        );
        // "Kingston" names the road, not the town of the address.
        assert_eq!(
            top_ids(&searcher, "2228 Kingston Rd, Toronto"),
            vec!["osm:way:40"]
        );
        assert_eq!(
            top_ids(&searcher, "2228 Gibraltar Rd, Kingston"),
            vec!["osm:node:3"]
        );
        // A street in the named town before the number elsewhere.
        assert_eq!(
            top_ids(&searcher, "99 Queen St, Toronto"),
            vec!["osm:way:50"]
        );
        // A geocoded row needs the address itself, in the named town.
        assert_eq!(
            geocode(&searcher, "99 Queen Street", Some("Toronto"), None),
            None
        );
        // Area names are expanded like query parts: "St" stays a saint.
        assert_eq!(
            top_ids(&searcher, "5 Ontario St, St. Catharines"),
            vec!["osm:node:6"]
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn a_postcode_ranks_records_by_how_much_of_it_they_share() {
        let temp_dir = temp_pack_path("search-postcode-rank");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer.set_postcode_areas(PostcodeAreas::new([(
            "A1A 1A1".to_string(),
            [-79.001, 43.001],
        )]));
        let mut stated =
            address_record("osm:node:1", "", "5", "Main Street", None, Some("B2B 2B2"));
        stated.geometry = point_geometry(-81.0, 45.0);
        let mut near = address_record("osm:node:2", "", "5", "Main Street", None, None);
        near.geometry = point_geometry(-79.0, 43.0);
        let mut far = address_record("osm:node:3", "", "5", "Main Street", None, None);
        far.geometry = point_geometry(-80.0, 44.0);
        for record in [stated, near, far] {
            writer.write(&record.into(), None).expect("address");
        }
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        // The index knows no "A1A 9Z9", but the record near "A1A 1A1" shares
        // its first three characters.
        assert_eq!(
            top_ids(&searcher, "5 Main Street, A1A 9Z9"),
            vec!["osm:node:2", "osm:node:3", "osm:node:1"]
        );
        assert_eq!(
            top_ids(&searcher, "5 Main Street, Toronto ON B2B 2B2")[0],
            "osm:node:1",
            "the postcode starts at its first word with a digit"
        );
        // A postcode nobody shares prefers no one: the closest labels first.
        assert_eq!(
            top_ids(&searcher, "5 Main Street, Z9Z 9Z9"),
            vec!["osm:node:2", "osm:node:3", "osm:node:1"]
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn ranking_asks_level_then_postcode_then_source_then_text() {
        use crate::record::{
            InterpolationAddressComponents, InterpolationRange, InterpolationRecord,
        };

        let temp_dir = temp_pack_path("search-rank-order");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer.set_postcode_areas(PostcodeAreas::new([
            ("M5C 1A1".to_string(), [-79.0, 43.0005]),
            ("M8V 1A1".to_string(), [-79.5, 43.6]),
        ]));
        // The number stated on a street of that name elsewhere, and a range
        // that holds it in the postcode the query names.
        let mut elsewhere = address_record("osm:node:1", "", "23", "Victoria Street", None, None);
        elsewhere.geometry = point_geometry(-79.5, 43.6);
        writer.write(&elsewhere.into(), None).expect("elsewhere");
        writer
            .write(
                &InterpolationRecord {
                    address: InterpolationAddressComponents {
                        street: Some("Victoria Street".to_string()),
                        place: None,
                        locality: None,
                        region: None,
                        postcode: None,
                        country: None,
                    },
                    interpolation: InterpolationRange {
                        kind: "odd".to_string(),
                        start: 1,
                        end: 99,
                        step: 2,
                    },
                    anchor_node_ids: [1, 99],
                    geometry: queen_street_line(),
                    representative_point: [-79.0, 43.0005],
                    source: SourceProvenance::osm(OsmObjectType::Way, 20),
                }
                .into(),
                None,
            )
            .expect("range");
        // A street in the named postcode, and the number on it elsewhere.
        let mut elm = street_record("osm:way:30", "Elm Street");
        elm.geometry = queen_street_line();
        elm.representative_point = [-79.0, 43.0005];
        writer.write(&elm.into(), None).expect("street");
        let mut number = address_record("osm:node:4", "", "7", "Elm Street", None, None);
        number.geometry = point_geometry(-79.5, 43.6);
        writer.write(&number.into(), None).expect("number");
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "23 Victoria St, M5C 2A1"),
            vec!["osm:way:20:interp:1-99", "osm:node:1"],
            "an estimate in the postcode before a stated number elsewhere"
        );
        assert_eq!(
            top_ids(&searcher, "23 Victoria St, M8V 2A1"),
            vec!["osm:node:1", "osm:way:20:interp:1-99"]
        );
        assert_eq!(
            top_ids(&searcher, "23 Victoria St"),
            vec!["osm:node:1", "osm:way:20:interp:1-99"],
            "without a postcode the stated number first"
        );
        // The localities only rank here: the house itself, though elsewhere,
        // before the street in the postcode.
        assert_eq!(
            top_ids(&searcher, "7 Elm St, Atlantis, M5C 1A1"),
            vec!["osm:node:4", "osm:way:30"]
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    fn line(lon: f64, lats: [f64; 2]) -> geojson::Geometry {
        geojson::Geometry::new(geojson::GeometryValue::LineString {
            coordinates: vec![vec![lon, lats[0]].into(), vec![lon, lats[1]].into()],
        })
    }

    #[test]
    fn a_missing_number_is_placed_between_its_stated_neighbours() {
        let temp_dir = temp_pack_path("search-neighbours");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        let mut street = |id: &str, name: &str, lon: f64| {
            let mut street = street_record(id, name);
            street.geometry = line(lon, [43.0, 43.01]);
            street.representative_point = [lon, 43.005];
            writer.write(&street.into(), None).expect("street");
        };
        street("osm:way:1", "Bayview Avenue", -79.0);
        // Two streets of one name, 1.6 km apart.
        street("osm:way:2", "Victoria Street", -79.1);
        street("osm:way:3", "Victoria Street", -79.12);
        for (id, number, street, lon, lat) in [
            ("osm:node:10", "10", "Bayview Avenue", -79.0002, 43.001),
            ("osm:node:11", "20", "Bayview Avenue", -79.0002, 43.002),
            ("osm:node:12", "30", "Bayview Avenue", -79.0002, 43.003),
            ("osm:node:13", "81", "Victoria Street", -79.1002, 43.001),
            ("osm:node:14", "91", "Victoria Street", -79.1202, 43.002),
        ] {
            let mut address = address_record(id, "", number, street, None, None);
            address.geometry = point_geometry(lon, lat);
            writer.write(&address.into(), None).expect("address");
        }
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");
        let top = |query: &str| {
            searcher
                .search(TextSearchOptions {
                    query: query.to_string(),
                    limit: 5,
                    layer: None,
                })
                .expect("search")
                .into_iter()
                .next()
                .expect("a hit")
                .record
        };

        let estimate = top("14 Bayview Ave");
        assert_eq!(estimate.layer, "address");
        assert_eq!(estimate.label, "14 Bayview Avenue");
        assert_eq!(estimate.id, "derived:estimate:14:osm:node:10:osm:node:11");
        let point = estimate.point.expect("point");
        assert_eq!(point.precision, RecordPointPrecision::Estimated);
        assert!((point.lat - 43.0014).abs() < 1e-9, "{point:?}");
        assert!((point.lon + 79.0002).abs() < 1e-9, "{point:?}");
        assert_eq!(
            geocode(&searcher, "14 Bayview Avenue", None, None).as_deref(),
            Some("derived:estimate:14:osm:node:10:osm:node:11")
        );
        // No odd number is stated: the nearest of either parity.
        let odd = top("25 Bayview Ave").point.expect("point");
        assert!((odd.lat - 43.0025).abs() < 1e-9, "{odd:?}");
        // Never past the last number known.
        assert_eq!(top("40 Bayview Ave").id, "osm:way:1");
        assert_eq!(geocode(&searcher, "40 Bayview Avenue", None, None), None);
        // 81 and 91 are on different Victoria Streets.
        assert_eq!(top("85 Victoria St").layer, "street");
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn a_house_number_needs_its_street_not_a_name() {
        let temp_dir = temp_pack_path("search-number-street");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        let dental = address_record(
            "osm:node:1",
            "",
            "300",
            "St. Clair Avenue West",
            Some("Toronto"),
            None,
        )
        .address;
        writer
            .write(
                &PoiRecord {
                    name: "St. Clair Spadina Dental".to_string(),
                    category: "amenity:dentist".to_string(),
                    address: Some(dental),
                    geometry: point_geometry(-79.41, 43.68),
                    location_precision: LocationPrecision::Point,
                    source: SourceProvenance::osm(OsmObjectType::Node, 1),
                }
                .into(),
                None,
            )
            .expect("poi");
        writer
            .write(
                &address_record("osm:node:2", "", "300", "Spadina Avenue", None, None).into(),
                None,
            )
            .expect("address");
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(top_ids(&searcher, "300 Spadina Ave"), vec!["osm:node:2"]);
        assert_eq!(top_ids(&searcher, "Spadina Dental")[0], "osm:node:1");
        assert_eq!(
            top_ids(&searcher, "300 St Clair Ave W"),
            vec!["osm:node:1"],
            "a POI's address is its street's"
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn plans_units_floors_directions_and_fractions() {
        let plan = |query: &str| {
            let variants = search_query_variants(query);
            QueryPlan::parse(&variants[0]).expect("plan")
        };
        let words = |words: &[&str]| {
            words
                .iter()
                .map(|word| word.to_string())
                .collect::<Vec<_>>()
        };

        let floor = plan("79 Wellington St W FL36, Toronto");
        assert_eq!(floor.required, words(&["wellington", "street", "west"]));
        assert_eq!(floor.optional, words(&["fl36"]));
        assert_eq!(plan("6628 Finch Ave W 1").optional, words(&["1"]));
        // A street type that leads the name is followed by the name.
        assert_eq!(plan("1000 Highway 7").required, words(&["highway", "7"]));
        assert_eq!(
            plan("Highway 7, Markham").required,
            words(&["highway", "7"])
        );

        let query = "283 Morningside Ave, Unit-B, Toronto";
        let unit = plan(query);
        assert_eq!(unit.context, vec![words(&["toronto"])]);
        assert_eq!(unit.optional, words(&["unit", "b"]));
        assert_eq!(
            search_query_variants(query),
            vec![
                "283 morningside avenue, unit b, toronto",
                "283 morningside avenue, toronto"
            ]
        );
        // "Ste" also abbreviates Sainte.
        assert_eq!(
            plan("10 Rue Principale, Ste Foy").context,
            vec![words(&["ste", "foy"])]
        );

        assert_eq!(
            plan("15 THE DONWAY E").required,
            words(&["the", "donway", "east"])
        );

        for (query, text, base) in [
            ("993.5 Bloor St W", "993 5", 993),
            ("1384 1/2 Queen St E", "1384 1 2", 1384),
            ("407A Yonge St", "407a", 407),
        ] {
            let plan = plan(query);
            let number = plan.house_number.as_ref().expect("number");
            assert_eq!((number.text.as_str(), number.base), (text, base), "{query}");
            let fallback = plan.with_base_house_number().expect("fallback");
            assert_eq!(
                fallback.house_number.expect("number").text,
                base.to_string()
            );
        }
        assert_eq!(
            plan("306-1333 Sheppard Ave E")
                .house_number
                .expect("n")
                .text,
            "306"
        );
        assert_eq!(plan("10 King St").with_base_house_number(), None);
    }

    #[test]
    fn searches_written_house_numbers_then_the_number_they_start_with() {
        let temp_dir = temp_pack_path("search-house-number-forms");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        for (id, number, street) in [
            ("osm:node:1", "1384 1/2", "Queen Street East"),
            ("osm:node:2", "1384", "Queen Street East"),
            ("osm:node:3", "993", "Bloor Street West"),
            ("osm:node:4", "100", "Highway 7"),
            ("osm:node:5", "100", "Highway 8"),
            ("osm:node:6", "8", "The Donway East"),
        ] {
            writer
                .write(
                    &address_record(id, "", number, street, None, None).into(),
                    None,
                )
                .expect("address");
        }
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(top_ids(&searcher, "1384 1/2 Queen St E")[0], "osm:node:1");
        assert_eq!(top_ids(&searcher, "993.5 Bloor St W"), vec!["osm:node:3"]);
        assert_eq!(top_ids(&searcher, "100 Highway 7")[0], "osm:node:4");
        assert_eq!(
            top_ids(&searcher, "8 The Donway E, Unit-B"),
            vec!["osm:node:6"]
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn geocode_accepts_a_community_the_record_states_inside_a_larger_municipality() {
        use crate::{
            context::AdminContextTuple,
            pack::RecordContext,
            record::{PlaceLayer, PlaceRecord, Record},
        };

        let temp_dir = temp_pack_path("geocode-community");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        let huron_east = writer
            .write(
                &Record::Place(
                    PlaceLayer::Locality,
                    PlaceRecord {
                        name: "Huron East".to_string(),
                        place_type: "admin_level:8".to_string(),
                        geometry: point_geometry(-81.4, 43.5),
                        source: SourceProvenance::osm(OsmObjectType::Relation, 1),
                    },
                ),
                None,
            )
            .expect("municipality");
        writer
            .write(
                &address_record(
                    "osm:node:2",
                    "",
                    "18",
                    "Main Street South",
                    Some("Seaforth"),
                    None,
                )
                .into(),
                Some(RecordContext {
                    admin_context: AdminContextTuple {
                        locality_record_id: Some(huron_east),
                        ..AdminContextTuple::default()
                    },
                    flags: 0,
                }),
            )
            .expect("address");
        writer.finish().expect("finish");
        let searcher = PackTextSearcher::open(&temp_dir).expect("searcher");

        for locality in ["Seaforth", "Huron East"] {
            assert_eq!(
                geocode(&searcher, "18 Main Street South", Some(locality), None).as_deref(),
                Some("osm:node:2"),
                "{locality}"
            );
        }
        assert_eq!(
            geocode(&searcher, "18 Main Street South", Some("Goderich"), None),
            None
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
