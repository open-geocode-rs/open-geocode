use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, hash_map::Entry},
    fmt, io,
    ops::{Bound, Range},
    path::Path,
    rc::Rc,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

use crate::error::{Error, Result};
use levenshtein_automata::{DFA, Distance, SINK_STATE};
use serde::Serialize;
use structured::{Place, StructuredIndex};
use tantivy::{
    DocAddress, DocId, DocSet, IndexReader, ReloadPolicy, Score, Searcher, SegmentReader, Term,
    collector::{Collector, ScoreSegmentTweaker, ScoreTweaker, TopDocs},
    columnar::{Column, StrColumn},
    directory::{OwnedBytes, RamDirectory},
    fastfield::{AliveBitSet, write_alive_bitset},
    index::SegmentId,
    indexer::merge_filtered_segments,
    postings::SegmentPostings,
    query::{
        BooleanQuery, BoostQuery, ConstScoreQuery, DisjunctionMaxQuery, Occur, PhrasePrefixQuery,
        PhraseQuery, Query, RangeQuery, TermQuery,
    },
    schema::{Field, IndexRecordOption},
};

use crate::{
    api::{Accuracy, Address, Highlight, PartMatch, QueryMatch},
    confidence::{Agreement, Features},
    labels,
    pack::{self, Point, PointPrecision, RecordId, Source, Summary},
    parse::{
        lattice::{
            self, CountryCode, Explanation, LabelSet, LatticeTagger, Tokens, automaton_builder,
            dictionary::{Dictionaries, Field as NameField, designates_unit},
            dictionary_key, glued_stem,
        },
        types::{Label, Tagged},
    },
    rank::{Ranker, Signals},
    record::{
        AddressComponents, AddressRecord, DERIVED_FROM_ADDRESS_RECORDS,
        InterpolationAddressComponents, Layer, ObjectType, PlaceLayer, PoiRecord, Prominence,
        Record, place_rank,
    },
    records::{dequantize, quantize},
    spatial_index::{self},
    text_index::{
        AREA_POSTCODE_FIELD, COUNTRY_CODE_FIELD, Fields, HOUSE_NUMBER_FIELD,
        INTERPOLATION_END_FIELD, INTERPOLATION_START_FIELD, MAX_LAT_FIELD, MAX_LON_FIELD,
        MIN_LAT_FIELD, MIN_LON_FIELD, NAME_WORDS_FIELD, PLACE_RANK_FIELD, POSTCODE_AREA_RADIUS_M,
        PROMINENCE_FIELD, RANK_POSTCODE_FIELD, RECORD_COUNT_FIELD, STREET_TYPE_ABBREVIATIONS,
        address_number_terms, base_house_number, compact_postcode, expand_address_abbreviations,
        expand_final_direction, flat_number_term, is_numeric_token, open_text_index,
        street_search_text,
    },
    util::fold::{exact_key, replaced_characters, search_key},
    util::geo::haversine_m,
};

pub mod confidence;
pub mod rank;
pub(crate) mod road;
pub mod structured;

#[derive(Clone)]
pub struct TextSearcher {
    pack: Arc<pack::Reader>,
    spatial: spatial_index::Reader,
    reader: IndexReader,
    fields: Fields,
    tagger: Arc<LatticeTagger>,
    columns: Arc<HashMap<SegmentId, SegmentColumns>>,
    confidence: Option<confidence::Model>,
    ranker: Option<Ranker>,
    places: bool,
    budget: Duration,
    structured: Option<Arc<StructuredIndex>>,
    segments: Arc<Vec<(SegmentId, DocId, DocId)>>,
    famed: Arc<OnceLock<Famed>>,
    completions: Arc<Mutex<HashMap<(Field, String), Completions>>>,
    expansion_ends: Arc<Mutex<HashMap<(Field, String), ExpansionEnd>>>,
}

type Completions = Option<(Vec<String>, bool)>;

type ExpansionEnd = Option<Term>;

const COMPLETIONS_KEPT: usize = 1 << 16;

struct Famed {
    reader: IndexReader,
    columns: HashMap<SegmentId, SegmentColumns>,
    statistics: Searcher,
}

struct Corpus<'a> {
    searcher: Searcher,
    columns: &'a HashMap<SegmentId, SegmentColumns>,
    statistics: Option<&'a Searcher>,
}

impl Corpus<'_> {
    fn search<C: Collector>(&self, query: &dyn Query, collector: &C) -> tantivy::Result<C::Fruit> {
        match self.statistics {
            Some(statistics) => self
                .searcher
                .search_with_statistics_provider(query, collector, statistics),
            None => self.searcher.search(query, collector),
        }
    }
}

impl fmt::Debug for TextSearcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextSearcher")
            .field("segments", &self.columns.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct SegmentColumns {
    record_id: Column<u64>,
    record_count: Column<u64>,
    house_number: Column<u64>,
    interpolation_start: Column<u64>,
    interpolation_end: Column<u64>,
    rank_postcode: Option<StrColumn>,
    area_postcode: Option<StrColumn>,
    country_code: Option<StrColumn>,
    min_lon: Column<i64>,
    min_lat: Column<i64>,
    max_lon: Column<i64>,
    max_lat: Column<i64>,
    name_words: Column<u64>,
    place_rank: Column<u64>,
    prominence: Column<u64>,
}

impl SegmentColumns {
    fn of(segment: &SegmentReader) -> tantivy::Result<Self> {
        let fast_fields = segment.fast_fields();
        Ok(Self {
            record_id: fast_fields.u64("record_id")?,
            record_count: fast_fields.u64(RECORD_COUNT_FIELD)?,
            house_number: fast_fields.u64(HOUSE_NUMBER_FIELD)?,
            interpolation_start: fast_fields.u64(INTERPOLATION_START_FIELD)?,
            interpolation_end: fast_fields.u64(INTERPOLATION_END_FIELD)?,
            rank_postcode: fast_fields.str(RANK_POSTCODE_FIELD)?,
            area_postcode: fast_fields.str(AREA_POSTCODE_FIELD)?,
            country_code: fast_fields.str(COUNTRY_CODE_FIELD)?,
            min_lon: fast_fields.i64(MIN_LON_FIELD)?,
            min_lat: fast_fields.i64(MIN_LAT_FIELD)?,
            max_lon: fast_fields.i64(MAX_LON_FIELD)?,
            max_lat: fast_fields.i64(MAX_LAT_FIELD)?,
            name_words: fast_fields.u64(NAME_WORDS_FIELD)?,
            place_rank: fast_fields.u64(PLACE_RANK_FIELD)?,
            prominence: fast_fields.u64(PROMINENCE_FIELD)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextSearchOptions {
    pub query: String,
    pub limit: usize,
    pub layer: Option<String>,
    /// Return only records in these countries, by ISO 3166-1 alpha-2 code.
    /// Empty means every country the Pack covers. A Pack of one country
    /// needs none; one covering several is narrowed with it.
    pub countries: Vec<String>,
    /// Where the caller is, `[lon, lat]`: of answers the query cannot tell
    /// apart, the nearer first (see [`TieBreak::focus_band`]). It never
    /// outranks what the query states.
    pub focus: Option<[f64; 2]>,
    pub bbox: Option<BoundingBox>,
}

/// A box of WGS84 degrees, west to east and south to north. It does not
/// cross the antimeridian: a box that would is two boxes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoundingBox {
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
}

impl BoundingBox {
    pub fn contains(&self, lon: f64, lat: f64) -> bool {
        (self.min_lon..=self.max_lon).contains(&lon) && (self.min_lat..=self.max_lat).contains(&lat)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Filters<'a> {
    pub layer: Option<&'a str>,
    pub countries: &'a [String],
    pub bbox: Option<BoundingBox>,
}

impl<'a> Filters<'a> {
    fn new(layer: Option<&'a str>, countries: &'a [String], bbox: Option<BoundingBox>) -> Self {
        Self {
            layer,
            countries,
            bbox,
        }
    }

    fn admit(&self, hit: &TextSearchHit) -> bool {
        self.bbox.is_none_or(|bbox| {
            hit.record
                .point
                .is_some_and(|point| bbox.contains(point.lon, point.lat))
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextAutocompleteOptions {
    pub query: String,
    pub limit: usize,
    pub layer: Option<String>,
    pub countries: Vec<String>,
    pub focus: Option<[f64; 2]>,
    pub bbox: Option<BoundingBox>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TextSearchHit {
    pub record_id: RecordId,
    pub score: Score,
    pub record: Summary,
    #[serde(skip)]
    pub features: Features,
    /// The chance the answer is the one asked for (see [`confidence`]),
    /// `None` when the Pack has no fitted model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
    #[serde(skip)]
    pub signals: Signals,
    #[serde(skip)]
    pub named_place: bool,
    #[serde(skip)]
    pub matched: QueryMatch,
    #[serde(skip)]
    pub highlights: Vec<Highlight>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressGeocodeOptions {
    pub countries: Vec<String>,
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
#[cfg(not(test))]
pub const SEARCH_BUDGET: Duration = Duration::from_millis(300);
#[cfg(test)]
pub const SEARCH_BUDGET: Duration = Duration::from_secs(3_600);
pub const MAX_AUTOCOMPLETE_LIMIT: usize = 20;
const POSTCODE_RECORDS_READ: usize = 16;

const MIN_AUTOCOMPLETE_QUERY_CHARS: usize = 3;
const FAME_WINDOW: usize = 64;
const SHORT_NAME_REACH_M: f64 = 1_000_000.0;

const PLACE_SHORT_FORMS: [(&str, &str); 5] = [
    ("ft", "fort"),
    ("mt", "mount"),
    ("st", "saint"),
    ("ste", "sainte"),
    ("pt", "port"),
];
const AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS: u32 = 1_024;
const TOWN_PREFIX_MAX_EXPANSIONS: u32 = 16_384;
const SAME_STREET_M: f64 = 2_000.0;
const TOWN_PREFIX_WINDOW: usize = 256;
const MAX_COMPLETION_SCAN: usize = 1 << 16;
const POSTCODE_AREA_TERMS: usize = 256;
const POSTCODE_AREA_RECORDS: usize = 64;
const POSTCODE_AREA_REACH_FLOOR_M: f64 = 500.0;
const POSTCODE_AREA_REACH_LIMIT_M: f64 = 200_000.0;
const POSTCODE_NEIGHBOURS: usize = 16;
const POSTCODE_AREA_MIN_NEIGHBOURS: usize = 3;
const POSTCODE_CONTRADICTING_REACHES: f64 = 3.0;
const POSTCODE_NEARNESS_BANDS_M: [f64; 8] = [
    1_000.0, 2_000.0, 5_000.0, 10_000.0, 20_000.0, 50_000.0, 100_000.0, 200_000.0,
];
const POSTCODE_RECORD_LIMIT: usize = 16;
const RANK_WINDOW_FACTOR: usize = 3;
const MAX_RANK_WINDOW: usize = 256;
const TIE_WINDOW: usize = 1_024;
const RANGE_SCORE: Score = 1e-3;
const NAME_BOOST: Score = 5.5;
const NUMBER_BOOST: Score = 2.0;
const MATCH_AGREEMENT: f64 = 0.95;
const AREA_STANDS_IN_BOOST: Score = 0.5;
const CORRECTED_BOOST: Score = 0.5;
pub(crate) const MIN_TYPO_CHARS: usize = 4;
const LONG_TYPO_CHARS: usize = 7;
const TYPO_PREFIX_CHARS: usize = 1;
const MAX_TYPO_TERMS: usize = 32;
const NAMED_PLACES_WINDOW: usize = 200;
const FAME_TIER: u8 = 15;
const NEIGHBOUR_CANDIDATES: usize = 4;
const NEIGHBOUR_DOCUMENTS: usize = 256;
const NEIGHBOUR_STREET_RADIUS_M: f64 = 100.0;
const ROAD_SETBACK_M: f64 = 200.0;
const AT_ADDRESS_RADIUS_M: f64 = NEIGHBOUR_STREET_RADIUS_M;
const NEAR_SETTLEMENTS: usize = 5;
const NEAR_SETTLEMENT_RADIUS_M: f64 = 15_000.0;

impl TextSearchHit {
    pub fn of_record(record_id: RecordId, record: Summary) -> Self {
        Self {
            named_place: false,
            record_id,
            score: 0.0,
            record,
            features: Features::default(),
            confidence: None,
            fallback: false,
            signals: Signals::default(),
            matched: QueryMatch::default(),
            highlights: Vec::new(),
        }
    }
}

impl TextSearcher {
    pub fn open(pack_path: impl AsRef<Path>) -> Result<Self> {
        Self::from_pack(Arc::new(pack::Reader::open(pack_path)?))
    }

    pub fn from_pack(pack: Arc<pack::Reader>) -> Result<Self> {
        let index = open_text_index(pack.container())?;
        let schema = index.schema();
        let fields = Fields::from_schema(&schema)?;
        let reader = index.reader()?;
        let spatial = spatial_index::Reader::open(pack.container(), pack.records().clone())?;
        let tagger = Arc::new(LatticeTagger::from_container(pack.container())?);
        pack.container().prefault_sections(
            "parse/",
            crate::resources::Resources::probe().available_memory_bytes,
        );
        let mut columns = HashMap::new();
        let mut segments = Vec::new();
        for segment in reader.searcher().segment_readers() {
            let base = segments.last().map_or(0, |(_, _, end)| *end);
            segments.push((segment.segment_id(), base, base + segment.max_doc()));
            columns.insert(segment.segment_id(), SegmentColumns::of(segment)?);
        }
        let confidence = confidence::Model::load(&pack)?;
        let ranker = Ranker::load(&pack)?;
        let structured = StructuredIndex::open(pack.container())?.map(Arc::new);
        Ok(Self {
            spatial,
            pack,
            reader,
            fields,
            tagger,
            columns: Arc::new(columns),
            confidence,
            ranker,
            places: false,
            budget: SEARCH_BUDGET,
            structured,
            segments: Arc::new(segments),
            famed: Arc::new(OnceLock::new()),
            completions: Arc::default(),
            expansion_ends: Arc::default(),
        })
    }

    /// Returns how many famed documents the copy holds, 0 when no copy was made.
    pub fn prepare_famed(&self) -> Result<u64> {
        let searcher = self.reader.searcher();
        let index = searcher.index();
        let segments = index.searchable_segments()?;
        if segments.len() != 1 {
            return Ok(0);
        }
        let mut alive = Vec::with_capacity(segments.len());
        let mut kept = 0;
        for segment in &segments {
            let reader = searcher
                .segment_readers()
                .iter()
                .find(|reader| reader.segment_id() == segment.id())
                .ok_or_else(|| Error::Inconsistent("text index segment has no reader".into()))?;
            let prominence = &self
                .columns
                .get(&segment.id())
                .ok_or_else(|| Error::Inconsistent("text index segment has no fast fields".into()))?
                .prominence;
            let mut famed = tantivy_common::BitSet::with_max_value(reader.max_doc());
            for doc in reader.doc_ids_alive() {
                if prominence.first(doc).unwrap_or_default() >> 8 > 0 {
                    famed.insert(doc);
                    kept += 1;
                }
            }
            let mut bytes = Vec::new();
            write_alive_bitset(&famed, &mut bytes)?;
            alive.push(Some(AliveBitSet::open(OwnedBytes::new(bytes))));
        }
        if kept == 0 {
            return Ok(0);
        }
        let index = merge_filtered_segments(
            &segments,
            index.settings().clone(),
            alive,
            RamDirectory::create(),
        )?;
        let reader: IndexReader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let columns = reader
            .searcher()
            .segment_readers()
            .iter()
            .map(|segment| Ok((segment.segment_id(), SegmentColumns::of(segment)?)))
            .collect::<tantivy::Result<_>>()?;
        let _ = self.famed.set(Famed {
            reader,
            columns,
            statistics: searcher,
        });
        Ok(kept)
    }

    fn expansion_end(&self, field: Field, prefix: &str) -> Result<Option<Term>> {
        if self.famed.get().is_none() {
            return Ok(None);
        }
        let key = (field, prefix.to_string());
        if let Some(found) = self
            .expansion_ends
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return Ok(found.clone());
        }
        let searcher = self.reader.searcher();
        let index = searcher.segment_reader(0).inverted_index(field)?;
        let mut stream = index.terms().range().ge(prefix.as_bytes()).into_stream()?;
        let mut seen = 0;
        let mut last = None;
        while seen < AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS
            && stream.advance()
            && stream.key().starts_with(prefix.as_bytes())
        {
            seen += 1;
            last = Some(stream.key().to_vec());
        }
        let end = match seen == AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS {
            true => last
                .and_then(|last| String::from_utf8(last).ok())
                .map(|last| Term::from_field_text(field, &last)),
            false => None,
        };
        let mut kept = self
            .expansion_ends
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if kept.len() >= COMPLETIONS_KEPT {
            kept.clear();
        }
        kept.insert(key, end.clone());
        Ok(end)
    }

    fn corpus(&self) -> Corpus<'_> {
        Corpus {
            searcher: self.reader.searcher(),
            columns: &self.columns,
            statistics: None,
        }
    }

    fn famed_corpus(&self) -> Corpus<'_> {
        match self.famed.get() {
            Some(famed) => Corpus {
                searcher: famed.reader.searcher(),
                columns: &famed.columns,
                statistics: Some(&famed.statistics),
            },
            None => self.corpus(),
        }
    }

    pub fn places(&self) -> Self {
        Self {
            fields: self.fields.for_places(),
            confidence: None,
            ranker: None,
            structured: None,
            places: true,
            ..self.clone()
        }
    }

    #[cfg(test)]
    fn without_structured(mut self) -> Self {
        self.structured = None;
        self
    }

    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }

    pub fn set_ranker(&mut self, ranker: Option<Ranker>) {
        self.ranker = ranker;
    }

    pub fn describes_street(&self, word: &str) -> bool {
        self.tagger.dictionaries().describes_street(word)
    }

    pub(crate) fn best_parse(&self, query: &str) -> Option<lattice::ParsedQuery> {
        self.tagger.parse(query).into_iter().next()
    }

    pub(crate) fn country_named(&self, name: &str) -> Option<CountryCode> {
        let key = lattice::dictionary_key(name)?;
        self.tagger
            .dictionaries()
            .get(NameField::Country, &key)?
            .country
    }

    pub fn confidence_model(&self) -> Option<&confidence::Model> {
        self.confidence.as_ref()
    }

    fn rate(&self, hits: &mut [TextSearchHit], numbered: Numbered) {
        for index in 0..hits.len() {
            let next = hits.get(index + 1).map(|next| next.score);
            let hit = &mut hits[index];
            hit.features.lower_rank = index > 0;
            hit.features.margin = match next {
                None => 1.0,
                Some(_) if hit.score <= 0.0 => 0.0,
                Some(next) => ((hit.score - next) / hit.score).clamp(0.0, 1.0),
            };
            hit.features.accuracy = hit
                .record
                .point
                .map(|point| Accuracy::of(point.precision, &hit.record.source));
            hit.confidence =
                self.confidence
                    .as_ref()
                    .and_then(|model| match (hit.fallback, numbered) {
                        (true, _) => Some(0.0),
                        (false, Numbered::Yes)
                            if ![
                                hit.matched.region,
                                hit.matched.country,
                                hit.matched.postcode,
                                hit.matched.locality,
                            ]
                            .contains(&Some(PartMatch::Unmatched)) =>
                        {
                            Some(model.rate(&hit.features))
                        }
                        (false, Numbered::Yes) => None,
                        (false, Numbered::No) => None,
                    });
        }
    }

    pub fn search(&self, options: &TextSearchOptions) -> Result<Vec<TextSearchHit>> {
        let mut hits = self.search_at(options, Depth::Query)?;
        if self.places || !number_reading::allows_retry(options.layer.as_deref(), hits.iter()) {
            return Ok(hits);
        }
        let Some(mut reading) = number_reading::address_variant(&options.query) else {
            return Ok(hits);
        };
        let alternative = TextSearchOptions {
            query: reading.query.clone(),
            layer: Some(Layer::Address.as_str().to_owned()),
            limit: DEFAULT_SEARCH_LIMIT,
            ..options.clone()
        };
        let retry = self.search_at_with(
            &alternative,
            Depth::Retry,
            |session, plan, postcode, hit| {
                self.witness_number_reading(&mut reading, session, plan, postcode, hit)?;
                Ok(true)
            },
        )?;
        if let Some(hit) = reading.unique_confirmed(retry) {
            hits.retain(|existing| existing.record_id != hit.record_id);
            hits.insert(0, hit);
            hits.truncate(effective_limit(options.limit));
        }
        Ok(hits)
    }

    fn search_at(&self, options: &TextSearchOptions, depth: Depth) -> Result<Vec<TextSearchHit>> {
        self.search_at_with(options, depth, |_, _, _, _| Ok(true))
    }

    fn search_at_with(
        &self,
        options: &TextSearchOptions,
        depth: Depth,
        accept: impl FnMut(&Session, &QueryPlan, &QueryPostcode, &TextSearchHit) -> Result<bool>,
    ) -> Result<Vec<TextSearchHit>> {
        let limit = effective_limit(options.limit);
        if limit == 0 {
            return Ok(Vec::new());
        }

        let written = options.query.trim();
        if written.is_empty() {
            return Err(Error::EmptyQuery);
        }
        let cleaned = clean_query(written);
        let query_text = cleaned.as_str();

        let filters = Filters::new(options.layer.as_deref(), &options.countries, options.bbox);
        let mut named = self.places_by_name(query_text, &filters, options.focus, limit)?;
        if named.len() >= limit {
            self.rate(&mut named, Numbered::No);
            if !self.places {
                stated_country(&options.countries, &mut named);
            }
            return Ok(named);
        }
        let variants = self.search_variants(
            query_text,
            &[],
            &filters,
            options.focus,
            (limit + 1).max(CHOOSER_HITS),
            Parses::All,
            None,
            Some(limit),
            accept,
        )?;
        let mut hits = variants.hits;
        if depth == Depth::Query && !self.places && variants.numbered == Numbered::Yes {
            let recovered = if let Some(hit) = self.address_with_numeric_street(options, &hits)? {
                Some(hit)
            } else if let Some(hit) = self.address_with_abbreviated_street(options, &hits)? {
                Some(hit)
            } else {
                self.address_with_unconfirmed_neighbourhood(options, &hits)?
            };
            if let Some(hit) = recovered {
                hits.insert(0, hit);
            } else if options.layer.is_none() {
                self.prefer_exact_street(options, &mut hits)?;
            }
        }
        if hits.is_empty() && filters.layer.is_none() {
            hits = self.fallback(query_text, &[], &filters, options.focus, limit + 1)?;
        }
        variants.named.choose(&mut hits);
        if self.places {
            hits.sort_by_key(|hit| (hit.features.pass, hit.features.dropped_words));
        }
        if !named.is_empty() {
            let mut seen = named
                .iter()
                .map(|hit| hit.record_id)
                .collect::<HashSet<_>>();
            hits.retain(|hit| seen.insert(hit.record_id));
            hits.splice(0..0, named);
        }
        if let Some(order) = &variants.order {
            self.order_by_names(order, options, depth, &mut hits)?;
        }
        if !self.places && variants.numbered == Numbered::Yes {
            hits.retain(|hit| {
                let parts = &hit.matched;
                !contradicts_country(parts.country, &variants.countries, hit.features.country)
                    && !(parts.region == Some(PartMatch::Unmatched)
                        && parts.postcode == Some(PartMatch::Unmatched))
            });
            if hits.is_empty() && filters.layer.is_none() {
                hits = self.fallback(query_text, &[], &filters, options.focus, limit + 1)?;
            }
        }
        if hits
            .first()
            .is_some_and(|hit| hit.record.layer == Layer::Postcode.as_str())
            && let Some(index) = hits.iter().position(|hit| {
                hit.matched.housenumber == Some(PartMatch::Matched)
                    && hit.matched.street == Some(PartMatch::Matched)
                    && hit.matched.postcode == Some(PartMatch::Matched)
            })
        {
            hits[..=index].rotate_right(1);
        }
        self.one_per_item(&mut hits, limit)?;
        self.rate(&mut hits, variants.numbered);
        if !self.places {
            stated_country(&options.countries, &mut hits);
        }
        Ok(hits)
    }

    fn witness_number_reading(
        &self,
        reading: &mut number_reading::AddressVariant,
        session: &Session,
        plan: &QueryPlan,
        postcode: &QueryPostcode,
        hit: &TextSearchHit,
    ) -> Result<()> {
        if !reading.could_add_witness(hit) {
            return Ok(());
        }
        let given = (!postcode.forms.is_empty()).then_some(postcode.exact.as_str());
        let (_, localities) = self.localities_stated(plan, given)?;
        let ranking = self.plan_ranking(plan, &QueryWords::default(), None, postcode.clone());
        let (hits, _) = self.judged(
            plan,
            Scope::Scoped,
            &ranking,
            &localities,
            &self.stated_places(&plan.context),
            vec![hit.clone()],
            session,
            1,
        )?;
        if let Some(hit) = hits.first() {
            reading.add_witness(hit);
        }
        Ok(())
    }

    fn address_with_numeric_street(
        &self,
        options: &TextSearchOptions,
        hits: &[TextSearchHit],
    ) -> Result<Option<TextSearchHit>> {
        if hits.first().is_some_and(|hit| {
            hit.matched.housenumber == Some(PartMatch::Matched)
                && hit.matched.street == Some(PartMatch::Matched)
                && ![
                    hit.matched.locality,
                    hit.matched.region,
                    hit.matched.postcode,
                    hit.matched.country,
                ]
                .contains(&Some(PartMatch::Unmatched))
        }) {
            return Ok(None);
        }
        let Some(key) = search_key(&options.query) else {
            return Ok(None);
        };
        let expanded = expand_address_abbreviations(&key);
        let mut words = expanded.split_whitespace().collect::<Vec<_>>();
        let Some((index, number)) =
            words
                .windows(2)
                .enumerate()
                .skip(1)
                .find_map(|(index, pair)| {
                    if !crate::text_index::is_street_type(pair[1]) {
                        return None;
                    }
                    street_ordinal_number(pair[0]).map(|number| (index, number))
                })
        else {
            return Ok(None);
        };
        words[index] = number;
        let focus = if let Some(focus) = options.focus {
            Some(focus)
        } else if hits
            .iter()
            .any(|hit| hit.matched.postcode == Some(PartMatch::Unmatched))
        {
            self.fallback(
                &options.query,
                &[],
                &Filters::new(None, &options.countries, options.bbox),
                None,
                1,
            )?
            .into_iter()
            .find_map(|hit| {
                (hit.record.layer == Layer::Postcode.as_str()
                    && hit.matched.postcode == Some(PartMatch::Matched))
                .then_some(hit.record.point)
                .flatten()
                .map(|point| [point.lon, point.lat])
            })
        } else {
            None
        };
        let street_type = words[index + 1];
        let abbreviated = crate::text_index::STREET_TYPE_ABBREVIATIONS
            .iter()
            .find_map(|(short, full)| (*full == street_type).then_some(*short));
        for road_type in std::iter::once(street_type).chain(abbreviated) {
            words[index + 1] = road_type;
            let retry = TextSearchOptions {
                query: words.join(" "),
                focus,
                ..options.clone()
            };
            if let Some(hit) = self
                .search_at(&retry, Depth::Retry)?
                .into_iter()
                .find(|hit| {
                    hit.matched.housenumber == Some(PartMatch::Matched)
                        && hit.matched.street == Some(PartMatch::Matched)
                        && [hit.matched.locality, hit.matched.postcode]
                            .contains(&Some(PartMatch::Matched))
                        && ![
                            hit.matched.locality,
                            hit.matched.region,
                            hit.matched.postcode,
                            hit.matched.country,
                        ]
                        .contains(&Some(PartMatch::Unmatched))
                })
            {
                return Ok(Some(hit));
            }
        }
        Ok(None)
    }

    fn address_with_abbreviated_street(
        &self,
        options: &TextSearchOptions,
        hits: &[TextSearchHit],
    ) -> Result<Option<TextSearchHit>> {
        let Some(anchor) = hits.first().filter(|hit| {
            hit.matched.housenumber != Some(PartMatch::Matched)
                && hit.matched.locality == Some(PartMatch::Matched)
                && hit.matched.street == Some(PartMatch::Partial)
        }) else {
            return Ok(None);
        };
        let Some(point) = anchor.record.point else {
            return Ok(None);
        };
        let Some(query) = search_key(&options.query) else {
            return Ok(None);
        };
        let expanded = expand_address_abbreviations(&query);
        let words = expanded.split_whitespace().collect::<Vec<_>>();
        let Some((number, _)) = words
            .split_first()
            .filter(|(number, _)| is_numeric_token(number))
        else {
            return Ok(None);
        };
        let Some(kind) = words
            .iter()
            .enumerate()
            .skip(2)
            .find_map(|(index, word)| crate::text_index::is_street_type(word).then_some(index))
        else {
            return Ok(None);
        };
        let end = kind
            + 1
            + usize::from(words.get(kind + 1).is_some_and(|word| {
                matches!(
                    *word,
                    "north"
                        | "south"
                        | "west"
                        | "east"
                        | "northwest"
                        | "northeast"
                        | "southwest"
                        | "southeast"
                )
            }));
        let Some(town) = words.get(end..).filter(|town| !town.is_empty()) else {
            return Ok(None);
        };
        let focus = [point.lon, point.lat];
        let complete = TextAutocompleteOptions {
            query: words[..end].join(" "),
            limit: 3,
            layer: Some("address".into()),
            countries: options.countries.clone(),
            focus: Some(focus),
            bbox: options.bbox.or_else(|| Some(bias_bounds(focus))),
        };
        for candidate in self.autocomplete_as(&complete, Completion::AddressPrefix)? {
            let Some(street) = candidate.record.address.street.as_deref() else {
                continue;
            };
            let retry = TextSearchOptions {
                query: format!("{number} {street}, {}", town.join(" ")),
                ..options.clone()
            };
            if let Some(hit) = self
                .search_at(&retry, Depth::Retry)?
                .into_iter()
                .find(|hit| {
                    hit.record_id == candidate.record_id
                        && hit.matched.housenumber == Some(PartMatch::Matched)
                        && hit.matched.street == Some(PartMatch::Matched)
                        && hit.matched.locality == Some(PartMatch::Matched)
                        && ![
                            hit.matched.region,
                            hit.matched.country,
                            hit.matched.postcode,
                        ]
                        .contains(&Some(PartMatch::Unmatched))
                })
            {
                return Ok(Some(hit));
            }
        }
        Ok(None)
    }

    fn prefer_exact_street(
        &self,
        options: &TextSearchOptions,
        hits: &mut Vec<TextSearchHit>,
    ) -> Result<()> {
        let Some(first) = hits.first().filter(|hit| {
            hit.matched.housenumber == Some(PartMatch::Matched)
                && hit.matched.street == Some(PartMatch::Partial)
                && hit.matched.locality == Some(PartMatch::Matched)
        }) else {
            return Ok(());
        };
        let Some(street) = first
            .record
            .address
            .street
            .as_deref()
            .and_then(street_search_text)
        else {
            return Ok(());
        };
        let numbered = self.descriptors(street.split_whitespace());
        if numbered.directions.is_empty() {
            return Ok(());
        }
        let answers_named_street = |hit: &TextSearchHit| {
            if hit.matched.street != Some(PartMatch::Matched)
                || hit.matched.locality != Some(PartMatch::Matched)
                || [
                    hit.matched.region,
                    hit.matched.country,
                    hit.matched.postcode,
                ]
                .contains(&Some(PartMatch::Unmatched))
            {
                return false;
            }
            if hit.matched.housenumber == Some(PartMatch::Matched) {
                return true;
            }
            if hit.record.layer != Layer::Street.as_str() {
                return false;
            }
            street_search_text(&hit.record.name).is_some_and(|name| {
                let exact = self.descriptors(name.split_whitespace());
                exact.names == numbered.names
                    && exact.types == numbered.types
                    && exact.directions.is_empty()
            })
        };
        if let Some(index) = hits.iter().position(answers_named_street) {
            hits[..=index].rotate_right(1);
            return Ok(());
        }
        let retry = TextSearchOptions {
            limit: CHOOSER_HITS,
            ..options.clone()
        };
        if let Some(chosen) = self
            .search_at(&retry, Depth::Retry)?
            .into_iter()
            .find(answers_named_street)
        {
            hits.retain(|hit| hit.record_id != chosen.record_id);
            hits.insert(0, chosen);
        }
        Ok(())
    }

    fn address_with_unconfirmed_neighbourhood(
        &self,
        options: &TextSearchOptions,
        hits: &[TextSearchHit],
    ) -> Result<Option<TextSearchHit>> {
        let exact = |hit: &TextSearchHit| {
            hit.record.layer == Layer::Address.as_str()
                && [
                    hit.matched.housenumber,
                    hit.matched.street,
                    hit.matched.locality,
                ]
                .iter()
                .all(|part| *part == Some(PartMatch::Matched))
                && [
                    hit.matched.unit,
                    hit.matched.region,
                    hit.matched.country,
                    hit.matched.postcode,
                ]
                .iter()
                .all(|part| part.is_none_or(|part| part == PartMatch::Matched))
        };
        if hits.iter().any(exact)
            || options
                .layer
                .as_deref()
                .is_some_and(|layer| layer != "address")
        {
            return Ok(None);
        }
        let fields = options.query.split(',').map(str::trim).collect::<Vec<_>>();
        let dictionaries = self.tagger.dictionaries();
        let locality = |field: &str| {
            dictionary_key(field).is_some_and(|key| {
                dictionaries.contains(NameField::Locality, &key)
                    && !dictionaries.contains(NameField::Country, &key)
            })
        };
        let mut area_pairs = fields
            .windows(2)
            .enumerate()
            .skip(1)
            .filter(|(index, pair)| {
                locality(pair[0])
                    && locality(pair[1])
                    && fields[..*index]
                        .iter()
                        .any(|field| field.split_whitespace().any(is_numeric_token))
            });
        let Some((index, pair)) = area_pairs.next() else {
            return Ok(None);
        };
        if area_pairs.next().is_some() {
            return Ok(None);
        }
        let Some(neighbourhood) = search_key(pair[0]) else {
            return Ok(None);
        };
        let Some(municipality) = search_key(pair[1]) else {
            return Ok(None);
        };
        let retry = TextSearchOptions {
            query: fields[..index]
                .iter()
                .chain(&fields[index + 1..])
                .copied()
                .collect::<Vec<_>>()
                .join(", "),
            limit: CHOOSER_HITS,
            layer: Some(Layer::Address.as_str().to_string()),
            ..options.clone()
        };
        let hits = self.search_at(&retry, Depth::Retry)?;
        let mut exact = hits.into_iter().filter(exact);
        let Some(mut hit) = exact.next() else {
            return Ok(None);
        };
        if exact.any(|other| other.record.point != hit.record.point)
            || hit
                .record
                .address
                .locality
                .as_deref()
                .and_then(search_key)
                .as_deref()
                != Some(municipality.as_str())
        {
            return Ok(None);
        }
        let named = |name: &str| search_key(name).as_deref() == Some(neighbourhood.as_str());
        if !hit.record.address.areas.is_empty()
            && !hit.record.address.areas.iter().any(|name| named(name))
        {
            return Ok(None);
        }
        if let Some(area) = self
            .pack
            .boundary_context(hit.record_id)?
            .and_then(|context| context.admin_context.get(PlaceLayer::Neighbourhood))
            && !self
                .pack
                .records()
                .names(area)?
                .iter()
                .any(|name| named(name))
        {
            return Ok(None);
        }
        hit.matched.locality = Some(PartMatch::Partial);
        hit.features.area = Agreement::Unknown;
        Ok(Some(hit))
    }

    fn one_per_item(&self, hits: &mut Vec<TextSearchHit>, limit: usize) -> Result<()> {
        let records = self.pack.records();
        let mut at = HashMap::new();
        let mut kept = Vec::<TextSearchHit>::with_capacity(hits.len());
        for hit in hits.drain(..) {
            let Some(item) = records.wikidata(hit.record_id)? else {
                kept.push(hit);
                continue;
            };
            match at.get(&item) {
                Some(&held) => {
                    let place: &mut TextSearchHit = &mut kept[held];
                    if place.record.layer == hit.record.layer
                        && place.record.layer != Layer::Poi.as_str()
                        && place.record.bbox.is_none()
                        && hit.record.bbox.is_some()
                    {
                        *place = hit;
                    }
                }
                None => {
                    at.insert(item, kept.len());
                    kept.push(hit);
                }
            }
        }
        kept.truncate(limit);
        *hits = kept;
        Ok(())
    }

    pub fn search_pool(
        &self,
        options: &TextSearchOptions,
    ) -> Result<(Vec<TextSearchHit>, Vec<TextSearchHit>)> {
        let limit = effective_limit(options.limit);
        let query_text = options.query.trim();
        if limit == 0 || query_text.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let filters = Filters::new(options.layer.as_deref(), &options.countries, options.bbox);
        let mut pooled = Vec::new();
        let mut hits = self
            .search_variants(
                query_text,
                &[],
                &filters,
                options.focus,
                limit + 1,
                Parses::All,
                Some(&mut pooled),
                None,
                |_, _, _, _| Ok(true),
            )?
            .hits;
        hits.truncate(limit);
        Ok((hits, pooled))
    }

    pub fn geocode_address(
        &self,
        options: &AddressGeocodeOptions,
    ) -> Result<Option<AddressGeocodeHit>> {
        self.geocode_address_scoped(options, None, None)
    }

    pub(crate) fn geocode_address_scoped(
        &self,
        options: &AddressGeocodeOptions,
        focus: Option<[f64; 2]>,
        bbox: Option<BoundingBox>,
    ) -> Result<Option<AddressGeocodeHit>> {
        let (query, hits) = self.geocode_address_candidates(options, focus, bbox, None)?;
        if number_reading::allows_retry(options.layer.as_deref(), hits.iter())
            && let Some(mut reading) = number_reading::address_variant(&options.address)
        {
            let alternative = AddressGeocodeOptions {
                address: reading.query.clone(),
                layer: Some(Layer::Address.as_str().to_owned()),
                limit: DEFAULT_SEARCH_LIMIT,
                ..options.clone()
            };
            let (_, retry) =
                self.geocode_address_candidates(&alternative, focus, bbox, Some(&mut reading))?;
            if let Some(hit) = reading.unique_confirmed(retry) {
                return Ok(Some(AddressGeocodeHit { query, hit }));
            }
        }
        Ok(hits
            .into_iter()
            .next()
            .map(|hit| AddressGeocodeHit { query, hit }))
    }

    fn geocode_address_candidates(
        &self,
        options: &AddressGeocodeOptions,
        focus: Option<[f64; 2]>,
        bbox: Option<BoundingBox>,
        mut reading: Option<&mut number_reading::AddressVariant>,
    ) -> Result<(String, Vec<TextSearchHit>)> {
        let limit = effective_limit(options.limit);
        if limit == 0 {
            return Ok((options.address.clone(), Vec::new()));
        }

        let address = options.address.trim();
        if address.is_empty() {
            return Ok((options.address.clone(), Vec::new()));
        }

        let context = [
            ContextPart::of(Label::Locality, options.locality.as_deref()),
            ContextPart::of(Label::Region, options.region.as_deref()),
            ContextPart::of(Label::Postcode, options.postcode.as_deref()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let areas = context
            .iter()
            .filter(|part| matches!(part.label, Label::Locality | Label::Region))
            .collect::<Vec<_>>();
        let known = self.known_area_words(areas.iter().flat_map(|part| &part.words))?;
        let postcode_text = context
            .iter()
            .find(|part| part.is_postcode())
            .map(|part| part.words.join(" "));
        let mut stated = Vec::new();
        for part in &areas {
            if let Some(area) = self.stated_area(part, &known, postcode_text.as_deref())? {
                stated.push(area);
            }
        }
        let stated_localities = stated
            .iter()
            .filter(|area| area.label == Label::Locality)
            .cloned()
            .collect::<Vec<_>>();
        let places = self.stated_places(&context);
        let Variants {
            query,
            mut hits,
            numbered,
            ..
        } = self.search_variants(
            address,
            &context,
            &Filters::new(options.layer.as_deref(), &options.countries, bbox),
            focus,
            limit.max(2),
            Parses::Numbered,
            None,
            None,
            |session, plan, postcode, hit| {
                let stands_in =
                    plan.house_number.is_some() && hit.record.layer == Layer::Street.as_str();
                let in_postcode = match self.postcode_agreement(hit, postcode)? {
                    Some(agrees) => agrees,
                    None => !stands_in || postcode.forms.is_empty(),
                };
                let unnumbered_address =
                    plan.house_number.is_none() && hit.record.layer == Layer::Address.as_str();
                let in_areas = match self.region_agreement(hit, &places, session)? {
                    Some(false) => false,
                    Some(true) => self
                        .area_agreement(hit, &stated_localities, postcode, session)?
                        .unwrap_or(stated_localities.is_empty()),
                    None => self
                        .area_agreement(hit, &stated, postcode, session)?
                        .unwrap_or(stated.is_empty()),
                };
                let accepted =
                    hit.record.point.is_some() && !unnumbered_address && in_postcode && in_areas;
                if accepted && let Some(reading) = reading.as_deref_mut() {
                    self.witness_number_reading(reading, session, plan, postcode, hit)?;
                }
                Ok(accepted)
            },
        )?;
        if hits.is_empty() && options.layer.is_none() {
            let filters = Filters::new(None, &options.countries, bbox);
            hits = self.fallback(address, &context, &filters, focus, 2)?;
        }
        self.rate(&mut hits, numbered);
        stated_country(&options.countries, &mut hits);
        Ok((query, hits))
    }

    /// How many documents hold the categories' words, an upper bound on the
    /// POIs of them (`amenity pharmacy`, or a bare key).
    pub fn category_count(&self, categories: &[&str]) -> Result<u64> {
        let searcher = self.reader.searcher();
        let mut count = 0;
        for category in categories {
            let mut fewest = None::<u64>;
            for word in search_key(category)
                .iter()
                .flat_map(|key| key.split_whitespace())
            {
                let docs =
                    searcher.doc_freq(&Term::from_field_text(self.fields.content_text, word))?;
                fewest = Some(fewest.map_or(docs, |fewest| fewest.min(docs)));
            }
            count += fewest.unwrap_or_default();
        }
        Ok(count)
    }

    pub fn of_categories(
        &self,
        categories: &[&str],
        focus: Option<([f64; 2], f64)>,
        limit: usize,
        countries: &[String],
        bbox: Option<BoundingBox>,
    ) -> Result<Vec<TextSearchHit>> {
        let field = self.fields.content_text;
        let clauses = categories
            .iter()
            .filter_map(|category| -> Option<(Occur, Box<dyn Query>)> {
                let words = search_key(category)?
                    .split_whitespace()
                    .map(|word| -> Box<dyn Query> {
                        Box::new(TermQuery::new(
                            Term::from_field_text(field, word),
                            IndexRecordOption::Basic,
                        ))
                    })
                    .collect::<Vec<_>>();
                Some((Occur::Should, Box::new(BooleanQuery::intersection(words))))
            })
            .collect::<Vec<_>>();
        if clauses.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = vec![(
            Occur::Must,
            Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>,
        )];
        query.extend(
            self.countries_query(countries)
                .map(|countries| (Occur::Must, countries)),
        );
        query.extend(bbox.map(|bbox| (Occur::Must, self.bbox_query(bbox))));
        let ranking = Ranking {
            focus: focus.map(|(point, _)| point),
            near_first: focus.is_some(),
            ..Ranking::default()
        };
        let filters = Filters::new(None, countries, bbox);
        let records = self.pack.records();
        self.ranked_hits(&BooleanQuery::new(query), &ranking, limit, &mut |hit| {
            Ok(hit.record.layer == Layer::Poi.as_str()
                && filters.admit(hit)
                && hit.record.point.is_some_and(|point| {
                    focus.is_none_or(|([lon, lat], radius_m)| {
                        haversine_m(lon, lat, point.lon, point.lat) <= radius_m
                    })
                })
                && records.poi_category_is(hit.record_id, |category| {
                    crate::categories::holds(categories, category)
                })?)
        })
    }

    fn numbered_completion_focus(&self, options: &TextAutocompleteOptions) -> Option<[f64; 2]> {
        options.focus.filter(|_| {
            !self.places
                && search_key(&options.query).is_some_and(|key| {
                    let words = expand_address_abbreviations(&key);
                    let tokens = words.split_whitespace().collect::<Vec<_>>();
                    tokens.first().is_some_and(|word| is_numeric_token(word))
                        && tokens.iter().skip(1).any(|word| {
                            self.describes_street(word) || crate::text_index::is_street_type(word)
                        })
                        && tokens.last().is_some_and(|word| {
                            self.describes_street(word)
                                || crate::text_index::is_street_type(word)
                                || key
                                    .split_whitespace()
                                    .last()
                                    .and_then(crate::text_index::direction_word)
                                    .is_some()
                        })
                })
        })
    }

    fn qualified_directional_completion(
        &self,
        options: &TextAutocompleteOptions,
    ) -> Result<Vec<TextSearchHit>> {
        if self.places
            || options.focus.is_some()
            || options.countries.is_empty()
            || !matches!(options.layer.as_deref(), None | Some("address"))
        {
            return Ok(Vec::new());
        }
        let Some(key) = search_key(&options.query) else {
            return Ok(Vec::new());
        };
        let words = key.split_whitespace().collect::<Vec<_>>();
        if words.len() < 4
            || !is_numeric_token(words[0])
            || !self.describes_street(words[words.len() - 2])
            || crate::text_index::direction_word(words[words.len() - 1]).is_none()
        {
            return Ok(Vec::new());
        }
        let local = TextAutocompleteOptions {
            limit: CHOOSER_HITS,
            layer: Some("address".to_string()),
            ..options.clone()
        };
        let mut found = self.autocomplete_as(&local, Completion::AddressPrefix)?;
        let Some(first) = found.first() else {
            return Ok(found);
        };
        let Some(point) = first.record.point else {
            return Ok(Vec::new());
        };
        let street = first
            .record
            .address
            .street
            .as_deref()
            .and_then(street_search_text);
        if found.len() >= CHOOSER_HITS
            || found.iter().any(|hit| {
                hit.record
                    .address
                    .street
                    .as_deref()
                    .and_then(street_search_text)
                    != street
                    || hit.record.point.is_none_or(|other| {
                        haversine_m(point.lon, point.lat, other.lon, other.lat) > FOCUS_BANDS_M[0]
                    })
            })
        {
            return Ok(Vec::new());
        }
        found.truncate(effective_autocomplete_limit(options.limit));
        Ok(found)
    }

    pub fn autocomplete(&self, options: &TextAutocompleteOptions) -> Result<Vec<TextSearchHit>> {
        let directional = self.qualified_directional_completion(options)?;
        if !directional.is_empty() {
            return Ok(directional);
        }
        let scoped = self.numbered_completion_focus(options);
        let street = scoped
            .and_then(|_| search_key(&options.query))
            .map(|query| expand_address_abbreviations(&query));
        let ordinal = street.as_deref().and_then(|street| {
            let mut words = street.split_whitespace().skip(1).peekable();
            while let Some(word) = words.next() {
                if words
                    .peek()
                    .is_some_and(|next| crate::text_index::is_street_type(next))
                    && let Some(number) = street_ordinal_number(word)
                {
                    return Some(number);
                }
            }
            None
        });
        let same_ordinal = |hit: &TextSearchHit| {
            ordinal.is_none_or(|number| {
                hit.record
                    .address
                    .street
                    .as_deref()
                    .and_then(search_key)
                    .is_some_and(|street| {
                        street.split_whitespace().any(|word| {
                            word == number || street_ordinal_number(word) == Some(number)
                        })
                    })
            })
        };
        let matching = |hit: &TextSearchHit, point| within_bias(hit, point) && same_ordinal(hit);
        let mut far = Vec::new();
        for completion in Completion::ALL {
            if completion == Completion::AllWords
                && let Some(point) = scoped
                && matches!(options.layer.as_deref(), None | Some("address"))
            {
                let local = TextAutocompleteOptions {
                    bbox: options.bbox.or_else(|| Some(bias_bounds(point))),
                    layer: Some("address".to_string()),
                    ..options.clone()
                };
                let mut hits = self.autocomplete_as(&local, Completion::AddressPrefix)?;
                hits.retain(|hit| matching(hit, point));
                if !hits.is_empty() {
                    return Ok(hits);
                }
            }
            let mut hits = self.autocomplete_as(options, completion)?;
            if let Some(point) = scoped {
                if options.focus.is_some() {
                    let (near, other): (Vec<_>, Vec<_>) =
                        hits.into_iter().partition(|hit| matching(hit, point));
                    if far.is_empty() {
                        far = other.into_iter().filter(same_ordinal).collect();
                    }
                    hits = near;
                } else {
                    hits.retain(|hit| matching(hit, point));
                }
            }
            if !hits.is_empty() {
                return Ok(hits);
            }
        }
        if let Some(normalized) = autocomplete_country_alias(options) {
            return self.autocomplete(&normalized);
        }
        let completed = self.complete_address(options)?;
        Ok(if completed.is_empty() { far } else { completed })
    }

    pub fn place_in_area(&self, options: &TextAutocompleteOptions) -> Result<Vec<TextSearchHit>> {
        let words = search_key(&options.query)
            .map(|key| {
                key.split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let records = self.pack.records();
        let mut found = Vec::<(u8, TextSearchHit)>::new();
        for tail in 1..words.len().min(4) {
            let (head, area) = words.split_at(words.len() - tail);
            let (head, area) = (head.join(" "), area.join(" "));
            let head_tokens = autocomplete_query_tokens(&head);
            if head.bytes().any(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let towns = self.famous_as(
                &TextAutocompleteOptions {
                    query: head.clone(),
                    layer: None,
                    focus: None,
                    ..options.clone()
                },
                &head_tokens,
                &[Completion::InOrder],
            )?;
            for hit in towns {
                if hit.record.layer == Layer::Poi.as_str()
                    || found
                        .iter()
                        .any(|(_, kept)| kept.record_id == hit.record_id)
                    || !self
                        .names_of(hit.record_id, &hit.record.name)?
                        .contains(&head)
                {
                    continue;
                }
                let Some(context) = self.pack.boundary_context(hit.record_id)? else {
                    continue;
                };
                let mut holds = false;
                for entry in context.admin_context.entries() {
                    let id = entry.record_id;
                    let code = self
                        .pack
                        .subdivision_code(id)
                        .and_then(|iso| iso.split_once('-'))
                        .map(|(_, code)| code.to_lowercase());
                    let country = self
                        .pack
                        .country_code(id)
                        .map(|code| code.as_str().to_lowercase());
                    let named = || -> Result<bool> {
                        let name = self.pack.record_summary(id)?.name;
                        Ok(std::iter::once(name.as_str())
                            .chain(self.pack.language_names(id))
                            .filter_map(search_key)
                            .any(|name| name == area))
                    };
                    if code.as_deref() == Some(area.as_str())
                        || country.as_deref() == Some(area.as_str())
                        || named()?
                    {
                        holds = true;
                        break;
                    }
                }
                if holds {
                    let fame = records
                        .prominence(hit.record_id)?
                        .map_or(0, |prominence| prominence.importance);
                    found.push((fame, hit));
                }
            }
        }
        found.sort_by_key(|(fame, hit)| {
            let wide = [Layer::Country, Layer::Region].map(Layer::as_str);
            (
                wide.contains(&hit.record.layer.as_str()),
                std::cmp::Reverse(*fame),
            )
        });
        let hits = found
            .into_iter()
            .map(|(_, mut hit)| {
                hit.highlights = typed_highlights(&options.query, &hit.record.label);
                hit
            })
            .collect();
        reach::merge_twins(&self.pack, hits)
    }

    pub fn postcode_reading(
        &self,
        options: &TextAutocompleteOptions,
    ) -> Result<Vec<TextSearchHit>> {
        if options
            .layer
            .as_deref()
            .is_some_and(|layer| layer != Layer::Postcode.as_str())
        {
            return Ok(Vec::new());
        }
        let words = options
            .query
            .split(|character: char| character.is_whitespace() || character == ',')
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>();
        let count = words.len();
        let splits = [
            (0, 2),
            (0, 1),
            (count.saturating_sub(2), count),
            (count.saturating_sub(1), count),
        ];
        let searcher = self.reader.searcher();
        for (start, end) in splits {
            if end > count || start >= end || end - start > count {
                continue;
            }
            let code_words = &words[start..end];
            if !code_words
                .iter()
                .any(|word| word.bytes().any(|byte| byte.is_ascii_digit()))
                || code_words.len() == 2
                    && !code_words[1]
                        .bytes()
                        .any(|byte| byte.is_ascii_alphanumeric())
            {
                continue;
            }
            let Some(code) = compact_postcode(&code_words.join(" ")) else {
                continue;
            };
            if code.chars().count() < 3 {
                continue;
            }
            let rest = words[..start]
                .iter()
                .chain(&words[end..])
                .copied()
                .collect::<Vec<_>>()
                .join(" ");
            let rest = search_key(&rest);
            let spaced = search_key(&code_words.join(" ")).unwrap_or_else(|| code.clone());
            let splits = match code_words.len() == 1 && spaced == code && code.is_ascii() {
                true => (1..code.len())
                    .map(|at| format!("{} {}", &code[..at], &code[at..]))
                    .collect::<Vec<_>>(),
                false => vec![spaced],
            };
            let prefixed = |prefix: &str| -> Result<Vec<String>> {
                let mut terms =
                    self.terms_with_prefix(&searcher, self.fields.postcode_exact, prefix)?;
                if prefix == code {
                    for split in splits.iter().filter(|split| **split != code) {
                        terms.extend(self.terms_with_prefix(
                            &searcher,
                            self.fields.postcode_exact,
                            split,
                        )?);
                    }
                }
                Ok(terms)
            };
            let mut terms = prefixed(&code)?;
            let digits = |word: &str| word.bytes().any(|byte| byte.is_ascii_digit());
            let written_apart = match code_words {
                [first, second] => digits(first) && digits(second),
                [word] => word
                    .split_once('-')
                    .is_some_and(|(left, right)| digits(left) && digits(right)),
                _ => false,
            };
            let base = compact_postcode(code_words[0].split('-').next().unwrap_or_default())
                .map_or(0, |base| base.len());
            let mut start = code.len();
            while terms.is_empty() && written_apart && start > base.max(3) {
                start = code[..start].char_indices().last().map_or(0, |(at, _)| at);
                terms = prefixed(&code[..start])?;
            }
            if terms.is_empty() {
                continue;
            }
            let dictionaries = self.tagger.dictionaries();
            let mut found = Vec::new();
            let mut seen = Vec::<(String, [f64; 2])>::new();
            let whole = terms
                .iter()
                .filter(|term| compact_postcode(term).as_deref() == Some(code.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            let exact = self.postcode_records_in(Some(options), &whole, POSTCODE_AREA_RECORDS)?;
            let prefixed =
                self.postcode_records_in(Some(options), &terms, POSTCODE_AREA_RECORDS)?;
            for record_id in exact.into_iter().chain(prefixed) {
                let Record::Postcode(record) = self.pack.records().record(record_id)? else {
                    continue;
                };
                let Some(term) = compact_postcode(&record.postcode) else {
                    continue;
                };
                let summary = self.pack.record_summary(record_id)?;
                let (_, lon, lat) = self.pack.records().point(record_id)?;
                let at = [lon, lat];
                if options.bbox.is_some_and(|bbox| !bbox.contains(lon, lat)) {
                    continue;
                }
                if seen.iter().any(|(held, point)| {
                    *held == term && haversine_m(point[0], point[1], at[0], at[1]) <= BIAS_RADIUS_M
                }) {
                    continue;
                }
                if let Some(rest) = &rest {
                    let mut words = search_key(&summary.label)
                        .unwrap_or_default()
                        .split_whitespace()
                        .map(str::to_string)
                        .collect::<HashSet<_>>();
                    if let Some(context) = self.pack.boundary_context(record_id)? {
                        for entry in context.admin_context.entries() {
                            let name = self.pack.record_summary(entry.record_id)?.name;
                            for name in self.names_of(entry.record_id, &name)? {
                                words.extend(name.split_whitespace().map(str::to_string));
                            }
                            if let Some((_, code)) = self
                                .pack
                                .subdivision_code(entry.record_id)
                                .and_then(|iso| iso.split_once('-'))
                            {
                                words.insert(code.to_lowercase());
                            }
                        }
                    }
                    if !rest.split_whitespace().all(|word| words.contains(word)) {
                        continue;
                    }
                }
                seen.push((term.clone(), at));
                let distance = options
                    .focus
                    .map(|[lon, lat]| haversine_m(lon, lat, at[0], at[1]));
                let near = distance.is_some_and(|distance| distance <= BIAS_RADIUS_M);
                let weight = record
                    .country
                    .map_or(0.0, |country| dictionaries.country_weight(country));
                let key = (
                    !near,
                    term != code,
                    std::cmp::Reverse((weight * 1e6) as u64),
                    distance.map_or(0, |distance| distance as u64),
                    term,
                );
                let mut hit = TextSearchHit::of_record(record_id, summary);
                hit.highlights = typed_highlights(&options.query, &hit.record.label);
                found.push((key, hit));
            }
            if found.is_empty() {
                continue;
            }
            found.sort_by(|left, right| left.0.cmp(&right.0));
            return Ok(found.into_iter().map(|(_, hit)| hit).collect());
        }
        Ok(Vec::new())
    }

    fn names_of(&self, record_id: RecordId, name: &str) -> Result<Vec<String>> {
        let alt_names = match self.pack.records().record(record_id)? {
            crate::record::Record::Place(_, record) => record.alt_names,
            crate::record::Record::Poi(record) => record.alt_names,
            _ => Vec::new(),
        };
        Ok(std::iter::once(name)
            .chain(self.pack.language_names(record_id))
            .chain(alt_names.iter().map(String::as_str))
            .filter_map(search_key)
            .collect())
    }

    fn complete_address(&self, options: &TextAutocompleteOptions) -> Result<Vec<TextSearchHit>> {
        let hits = self.autocomplete_address(options, false)?;
        if !hits.is_empty()
            || self.places
            || !matches!(
                options.layer.as_deref(),
                None | Some("address" | "interpolation")
            )
        {
            return Ok(hits);
        }
        let Some((subject, qualifiers)) = options.query.split_once(',') else {
            return Ok(hits);
        };
        let subject = subject.trim();
        let qualifiers = qualifiers.trim();
        let Some(at) = subject.find(char::is_whitespace) else {
            return Ok(hits);
        };
        let last = subject.rfind(char::is_whitespace).unwrap_or(at);
        let digits =
            |word: &str| !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_digit());
        let (number, street) = if digits(&subject[..at]) {
            (&subject[..at], subject[at..].trim())
        } else {
            (subject[last..].trim(), subject[..last].trim())
        };
        if !digits(number) || !street.chars().any(char::is_alphabetic) || qualifiers.is_empty() {
            return Ok(hits);
        }

        let query = format!("{number} {street}, {qualifiers}");
        let (readings, _) = self.tagger.parse_explained_n(&query, 1);
        let Some(reading) = readings
            .first()
            .filter(|read| read.words(Label::Unit).next().is_none())
        else {
            return Ok(hits);
        };
        let postcode =
            compact_postcode(&reading.words(Label::Postcode).collect::<Vec<_>>().join(" "));
        let region = dictionary_key(&reading.words(Label::Region).collect::<Vec<_>>().join(" "));
        let found = self.search(&TextSearchOptions {
            query,
            limit: 5,
            layer: None,
            countries: options.countries.clone(),
            focus: options.focus,
            bbox: options.bbox,
        })?;
        let session = Session::new(self.budget);
        for mut hit in found {
            let matched = &hit.matched;
            let address = &hit.record.address;
            if hit.fallback
                || !matches!(hit.record.layer.as_str(), "address" | "interpolation")
                || options
                    .layer
                    .as_ref()
                    .is_some_and(|layer| layer != &hit.record.layer)
                || address.housenumber.as_deref() != Some(number)
                || matched.street != Some(PartMatch::Matched)
                || matched.locality != Some(PartMatch::Matched)
                || matched.country == Some(PartMatch::Unmatched)
                || postcode.as_ref().is_some_and(|asked| {
                    address
                        .postcode
                        .as_deref()
                        .and_then(compact_postcode)
                        .as_ref()
                        != Some(asked)
                })
            {
                continue;
            }
            if let Some(region) = &region {
                let chain = self.regions_of(hit.record_id, &session)?;
                let written = address.region.as_deref().and_then(dictionary_key);
                let named = self.tagger.dictionaries().regions(region);
                if written.as_ref() != Some(region)
                    && !chain.iter().any(|id| {
                        if region.len() == 2 {
                            self.pack
                                .subdivision_code(*id)
                                .and_then(|iso| iso.split_once('-'))
                                .is_some_and(|(_, code)| code.eq_ignore_ascii_case(region))
                        } else {
                            named.iter().any(|named| RecordId::from(*named) == *id)
                        }
                    })
                {
                    continue;
                }
            }
            hit.highlights = typed_highlights(&options.query, &hit.record.label);
            return Ok(vec![hit]);
        }
        Ok(hits)
    }

    fn autocomplete_address(
        &self,
        options: &TextAutocompleteOptions,
        focus_only: bool,
    ) -> Result<Vec<TextSearchHit>> {
        let layers = match options.layer.as_deref() {
            None => &[Layer::Address, Layer::Interpolation][..],
            Some("address") => &[Layer::Address],
            Some("interpolation") => &[Layer::Interpolation],
            Some(_) => return Ok(Vec::new()),
        };
        let mut words = options.query.split_whitespace();
        if self.places
            || words.clone().take(3).count() < 3
            || !words.any(|word| is_numeric_token(word.trim_matches(',')))
        {
            return Ok(Vec::new());
        }

        crate::stop::check()?;
        let query = clean_query(options.query.trim());
        let (plans, ..) = self.readings(&query, &[], Parses::Numbered)?;
        let Some(plan) = plans.first() else {
            return Ok(Vec::new());
        };
        let Some(number) = plan
            .house_number
            .as_ref()
            .filter(|number| number.plain().is_some())
        else {
            return Ok(Vec::new());
        };
        if plan.unit.is_some() || plan.subject_part.0 != Some(Label::Street) {
            return Ok(Vec::new());
        }
        let placed = self.known_area_words(
            plan.context
                .iter()
                .filter(|part| part.label == Label::Locality)
                .flat_map(|part| &part.words),
        )?;
        let nearby = if placed.is_empty() {
            if !plan.context.is_empty()
                || !plan.unplaced.is_empty()
                || !plan
                    .subject
                    .iter()
                    .any(|word| self.describes_street(&word.text))
            {
                return Ok(Vec::new());
            }
            let Some(focus) = options.focus else {
                return Ok(Vec::new());
            };
            Some(focus)
        } else if focus_only {
            return Ok(Vec::new());
        } else {
            None
        };
        let bbox = options.bbox.or_else(|| nearby.map(bias_bounds));

        let query_words = QueryWords::new(&query);
        let session = Session::new(self.budget);
        for layer in layers {
            if nearby.is_some() && *layer != Layer::Interpolation {
                continue;
            }
            crate::stop::check()?;
            let filters = Filters::new(Some(layer.as_str()), &options.countries, bbox);
            let (mut hits, _) = self.search_plan(
                plan,
                &placed,
                Scope::Scoped,
                &query_words,
                &filters,
                options.focus,
                1,
                &session,
                &mut |_, _, _, hit| {
                    Ok(
                        hit.record.address.housenumber.as_deref() == Some(number.text.as_str())
                            && nearby.is_none_or(|focus| within_bias(hit, focus)),
                    )
                },
            )?;
            hits.retain(|hit| {
                hit.matched.street == Some(PartMatch::Matched)
                    && (nearby.is_some()
                        || hit.matched.locality == Some(PartMatch::Matched)
                        || (*layer == Layer::Interpolation
                            && hit.matched.locality == Some(PartMatch::Partial)))
            });
            if !hits.is_empty() {
                self.rate(&mut hits, Numbered::Yes);
                for hit in &mut hits {
                    hit.highlights = typed_highlights(&query, &hit.record.label);
                }
                return Ok(hits);
            }
        }
        Ok(Vec::new())
    }

    fn famous_places(
        &self,
        options: &TextAutocompleteOptions,
        more: bool,
    ) -> Result<Vec<TextSearchHit>> {
        let Some(text) = search_key(options.query.trim()) else {
            return Ok(Vec::new());
        };
        if typed_width(&text) < MIN_AUTOCOMPLETE_QUERY_CHARS {
            return Ok(Vec::new());
        }
        let tokens = autocomplete_query_tokens(&text);
        let mut hits = self.famous_as(
            options,
            &tokens,
            &[Completion::CommonCompletions, Completion::InOrder],
        )?;
        if !more {
            return Ok(hits);
        }
        if let [head @ .., last] = tokens.as_slice()
            && !head.is_empty()
            && last.chars().count() == 1
        {
            for hit in self.famous_as(options, head, &[Completion::InOrder])? {
                if name_starts_as(&hit.record.name, &tokens, false)
                    && !hits.iter().any(|held| held.record_id == hit.record_id)
                {
                    hits.push(hit);
                }
            }
        }
        if tokens.len() > 1
            && !hits
                .iter()
                .any(|hit| name_starts_as(&hit.record.name, &tokens, false))
        {
            for hit in self.famous_as(options, &tokens, &[Completion::AllWords])? {
                let last = tokens.len() - 1;
                let mut areas = Vec::new();
                if let Some(context) = self.pack.boundary_context(hit.record_id)? {
                    for entry in context.admin_context.entries() {
                        if let Some(name) =
                            search_key(&self.pack.record_summary(entry.record_id)?.name)
                        {
                            areas.extend(autocomplete_query_tokens(&name));
                        }
                    }
                }
                let all = self
                    .names_of(hit.record_id, &hit.record.name)?
                    .iter()
                    .any(|name| {
                        let words = autocomplete_query_tokens(name);
                        let named = |at: usize, typed: &String| {
                            words.iter().any(|word| types_word(typed, word, at == last))
                        };
                        tokens
                            .iter()
                            .enumerate()
                            .any(|(at, typed)| named(at, typed))
                            && tokens.iter().enumerate().all(|(at, typed)| {
                                named(at, typed)
                                    || areas.iter().any(|word| types_word(typed, word, at == last))
                            })
                    });
                if all && !hits.iter().any(|held| held.record_id == hit.record_id) {
                    hits.push(hit);
                }
            }
        }
        let long = tokens
            .iter()
            .map(|token| {
                PLACE_SHORT_FORMS
                    .iter()
                    .find(|(short, _)| short == token)
                    .map_or_else(|| token.clone(), |(_, long)| (*long).to_string())
            })
            .collect::<Vec<_>>();
        if long != tokens {
            for hit in self.famous_as(
                options,
                &long,
                &[Completion::CommonCompletions, Completion::InOrder],
            )? {
                if name_starts_as(&hit.record.name, &long, false)
                    && !hits.iter().any(|held| held.record_id == hit.record_id)
                {
                    hits.push(hit);
                }
            }
        }
        let typed_right = hits.iter().any(|hit| {
            std::iter::once(hit.record.name.as_str())
                .chain(self.pack.language_names(hit.record_id))
                .filter_map(search_key)
                .any(|name| name.starts_with(&text))
        });
        if typed_width(&text) >= reach::TYPO_WIDTH && !typed_right {
            let fame = |hit: &TextSearchHit| -> Result<u8> {
                Ok(self
                    .pack
                    .records()
                    .prominence(hit.record_id)?
                    .map_or(0, |prominence| prominence.importance))
            };
            let mut typed_best = 0;
            for hit in &hits {
                if self
                    .names_of(hit.record_id, &hit.record.name)?
                    .iter()
                    .any(|name| name.starts_with(&text))
                {
                    typed_best = typed_best.max(fame(hit)?);
                }
            }
            for hit in self.famous_as(options, &tokens, &[Completion::Typos])? {
                if fame(&hit)? >= typed_best.saturating_add(reach::FAME_GROUP)
                    && !hits.iter().any(|held| held.record_id == hit.record_id)
                {
                    hits.push(hit);
                }
            }
        }
        Ok(hits)
    }

    pub fn short_named(&self, options: &TextAutocompleteOptions) -> Result<Vec<TextSearchHit>> {
        let Some(text) = search_key(options.query.trim()) else {
            return Ok(Vec::new());
        };
        if text.contains(' ')
            || typed_width(&text) >= MIN_AUTOCOMPLETE_QUERY_CHARS
            || text.chars().count() < 2
        {
            return Ok(Vec::new());
        }
        let filters = Filters::new(options.layer.as_deref(), &options.countries, options.bbox);
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(self.fields.place_name_text, &text),
                IndexRecordOption::Basic,
            )),
        )];
        if let Some(layer) = filters
            .layer
            .map(str::trim)
            .filter(|layer| !layer.is_empty())
        {
            clauses.push((Occur::Must, self.layer_query(layer)));
        }
        clauses.extend(self.scope_clauses(options));
        let query: Box<dyn Query> = Box::new(BooleanQuery::new(clauses));
        let ranking = Ranking {
            query_words: QueryWords::new(&options.query),
            focus: options.focus,
            fame_first: true,
            typed_words: 1,
            ..Ranking::default()
        };
        let mut hits = self.ranked_hits(&query, &ranking, FAME_WINDOW, &mut |hit| {
            Ok(filters.admit(hit) && reach::is_famed(&hit.record.layer))
        })?;
        let dictionaries = self.tagger.dictionaries();
        for id in dictionaries
            .areas(&text)
            .into_iter()
            .chain(dictionaries.regions(&text))
        {
            let record_id = RecordId::from(id);
            if !hits.iter().any(|hit| hit.record_id == record_id) {
                let hit = TextSearchHit::of_record(record_id, self.pack.record_summary(record_id)?);
                if self.in_scope(options, &hit)? {
                    hits.push(hit);
                }
            }
        }
        let mut found = Vec::new();
        for mut hit in hits {
            if !self
                .names_of(hit.record_id, &hit.record.name)?
                .contains(&text)
            {
                continue;
            }
            let fame = self
                .pack
                .records()
                .prominence(hit.record_id)?
                .map_or(0, |prominence| prominence.importance);
            hit.highlights = Vec::new();
            let (_, lon, lat) = self.pack.records().point(hit.record_id)?;
            let far = options.focus.is_none_or(|[from_lon, from_lat]| {
                haversine_m(from_lon, from_lat, lon, lat) > SHORT_NAME_REACH_M
            });
            found.push(((far, std::cmp::Reverse(fame)), hit));
        }
        found.sort_by_key(|(key, _)| *key);
        reach::merge_twins(&self.pack, found.into_iter().map(|(_, hit)| hit).collect())
    }

    fn famous_as(
        &self,
        options: &TextAutocompleteOptions,
        tokens: &[String],
        completions: &[Completion],
    ) -> Result<Vec<TextSearchHit>> {
        let filters = Filters::new(options.layer.as_deref(), &options.countries, options.bbox);
        let mut query = None;
        for completion in completions {
            query = self.autocomplete_query(*completion, None, tokens, true, &filters)?;
            if query.is_some() {
                break;
            }
        }
        let Some(query) = query else {
            return Ok(Vec::new());
        };
        let ranking = Ranking {
            query_words: QueryWords::new(&options.query),
            focus: options.focus,
            fame_first: true,
            typed_words: u8::try_from(tokens.len()).unwrap_or(u8::MAX),
            ..Ranking::default()
        };
        let window = rank_window(FAME_WINDOW);
        let famed = self.famed_corpus();
        let mut top_docs = self.top_documents(&famed, &query, &ranking, window)?;
        let corpus = match top_docs.len() < window && famed.statistics.is_some() {
            true => {
                let full = self.corpus();
                top_docs = self.top_documents(&full, &query, &ranking, window)?;
                full
            }
            false => famed,
        };
        let mut hits = self.rank_documents(
            &corpus.searcher,
            top_docs,
            &ranking,
            FAME_WINDOW,
            &mut |hit| Ok(filters.admit(hit) && reach::is_famed(&hit.record.layer)),
        )?;
        let highlighter =
            highlight::Highlighter::new(Completion::CommonCompletions, None, tokens, true);
        for hit in &mut hits {
            hit.features.prefix = true;
            hit.highlights = highlighter.ranges(&hit.record.label);
        }
        Ok(hits)
    }

    fn autocomplete_as(
        &self,
        options: &TextAutocompleteOptions,
        completion: Completion,
    ) -> Result<Vec<TextSearchHit>> {
        let limit = effective_autocomplete_limit(options.limit);
        if limit == 0 {
            return Ok(Vec::new());
        }

        let Some(query_text) = search_key(options.query.trim()) else {
            return Ok(Vec::new());
        };
        if typed_width(&query_text) < MIN_AUTOCOMPLETE_QUERY_CHARS {
            return Ok(Vec::new());
        }

        let filters = Filters::new(options.layer.as_deref(), &options.countries, options.bbox);
        let expanded;
        let text = if completion == Completion::AddressPrefix {
            expanded = expand_address_abbreviations(&query_text);
            &expanded
        } else {
            &query_text
        };
        let tokens = autocomplete_query_tokens(text);
        if let Some(number) = autocomplete_number(&tokens) {
            let houses = self.autocomplete_reading(
                options,
                completion,
                Some(number),
                &tokens[1..],
                true,
                &filters,
            )?;
            let mut streets = match &tokens[1..] {
                [street @ .., town]
                    if street.len() > 1 && town.chars().all(char::is_alphabetic) =>
                {
                    self.streets_for_number(
                        options,
                        completion,
                        number,
                        street,
                        Some(town),
                        &filters,
                    )?
                }
                _ => Vec::new(),
            };
            streets.extend(self.streets_for_number(
                options,
                completion,
                number,
                &tokens[1..],
                None,
                &filters,
            )?);
            let mut hits = self.with_streets(houses, streets);
            hits.truncate(limit);
            return Ok(hits);
        }
        let as_typed =
            self.autocomplete_reading(options, completion, None, &tokens, true, &filters)?;
        let Some(at) = tokens
            .iter()
            .skip(1)
            .position(|token| is_address_number_token(token))
            .map(|index| index + 1)
        else {
            return Ok(as_typed);
        };
        let number = &tokens[at];
        let names_it = |hit: &TextSearchHit| {
            search_key(&hit.record.name).is_some_and(|name| {
                name.split_whitespace()
                    .any(|word| types_word(number, word, true))
            })
        };
        if as_typed.iter().any(names_it) {
            return Ok(as_typed);
        }
        let words = [&tokens[..at], &tokens[at + 1..]].concat();
        let typing = at + 1 < tokens.len();
        let numbered =
            self.autocomplete_reading(options, completion, Some(number), &words, typing, &filters)?;
        let mut hits = match &tokens[at + 1..] {
            [town] if typing && town.chars().all(char::is_alphabetic) => {
                self.in_town_starting(options, number, &tokens[..at], town, &filters)?
            }
            _ => Vec::new(),
        };
        for hit in numbered {
            if !hits.iter().any(|held| held.record_id == hit.record_id) {
                hits.push(hit);
            }
        }
        let town = match &tokens[at + 1..] {
            [] => None,
            [town] if typing && town.chars().all(char::is_alphabetic) => Some(town.as_str()),
            _ => return Ok(if hits.is_empty() { as_typed } else { hits }),
        };
        let streets =
            self.streets_for_number(options, completion, number, &tokens[..at], town, &filters)?;
        let mut hits = self.with_streets(hits, streets);
        hits.truncate(limit);
        Ok(if hits.is_empty() { as_typed } else { hits })
    }

    fn in_town_starting(
        &self,
        options: &TextAutocompleteOptions,
        number: &str,
        street: &[String],
        town: &str,
        filters: &Filters<'_>,
    ) -> Result<Vec<TextSearchHit>> {
        let limit = effective_autocomplete_limit(options.limit);
        let Some(street_query) =
            self.autocomplete_query(Completion::InOrder, Some(number), street, false, filters)?
        else {
            return Ok(Vec::new());
        };
        let mut area =
            PhrasePrefixQuery::new(vec![Term::from_field_text(self.fields.context_text, town)]);
        area.set_max_expansions(TOWN_PREFIX_MAX_EXPANSIONS);
        let query: Box<dyn Query> = Box::new(BooleanQuery::new(vec![
            (Occur::Must, street_query),
            (Occur::Must, Box::new(area)),
        ]));
        let ranking = Ranking {
            query_words: QueryWords::new(&options.query),
            number_texts: vec![number.to_string()],
            focus: options.focus,
            near_first: options.focus.is_some(),
            typed_words: u8::try_from(street.len() + 2).unwrap_or(u8::MAX),
            ..Ranking::default()
        };
        let mut hits = self.ranked_hits(&query, &ranking, TOWN_PREFIX_WINDOW, &mut |hit| {
            Ok(filters.admit(hit))
        })?;
        let town_starts = |hit: &TextSearchHit| {
            let address = &hit.record.address;
            [address.locality.as_deref(), address.place.as_deref()]
                .into_iter()
                .flatten()
                .chain(address.areas.iter().map(String::as_str))
                .filter_map(search_key)
                .any(|name| name.split_whitespace().any(|word| word.starts_with(town)))
        };
        hits.sort_by_key(|hit| !town_starts(hit));
        let words = street
            .iter()
            .cloned()
            .chain(std::iter::once(town.to_string()))
            .collect::<Vec<_>>();
        let highlighter =
            highlight::Highlighter::new(Completion::InOrder, Some(number), &words, true);
        for hit in &mut hits {
            hit.features.prefix = true;
            hit.features.number = Some(states_number(&hit.record, &ranking));
            hit.highlights = highlighter.ranges(&hit.record.label);
        }
        self.rate(&mut hits, Numbered::Yes);
        hits.truncate(limit);
        Ok(hits)
    }

    fn streets_for_number(
        &self,
        options: &TextAutocompleteOptions,
        completion: Completion,
        number: &str,
        street: &[String],
        town: Option<&str>,
        filters: &Filters<'_>,
    ) -> Result<Vec<TextSearchHit>> {
        let limit = effective_autocomplete_limit(options.limit);
        let wanted = |layer: &str| filters.layer.is_none_or(|only| only.trim() == layer);
        if self.places || street.is_empty() || !(wanted("street") || wanted("interpolation")) {
            return Ok(Vec::new());
        }
        let typing = town.is_none();
        let completion = if typing {
            completion
        } else {
            Completion::InOrder
        };
        let Some(street_query) =
            self.autocomplete_query(completion, None, street, typing, filters)?
        else {
            return Ok(Vec::new());
        };
        let layers = [Layer::Street, Layer::Interpolation]
            .into_iter()
            .map(|layer| -> (Occur, Box<dyn Query>) {
                (
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.layer, layer.as_str()),
                        IndexRecordOption::Basic,
                    )),
                )
            })
            .collect();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![
            (Occur::Must, street_query),
            (Occur::Must, Box::new(BooleanQuery::new(layers))),
        ];
        if let Some(town) = town {
            let mut area =
                PhrasePrefixQuery::new(vec![Term::from_field_text(self.fields.context_text, town)]);
            area.set_max_expansions(TOWN_PREFIX_MAX_EXPANSIONS);
            clauses.push((Occur::Must, Box::new(area)));
        }
        let ranking = Ranking {
            query_words: QueryWords::new(&options.query),
            numbered: true,
            house_number: number.parse().ok(),
            focus: options.focus,
            near_first: options.focus.is_some(),
            typed_words: u8::try_from(street.len() + 1 + usize::from(town.is_some()))
                .unwrap_or(u8::MAX),
            ..Ranking::default()
        };
        let mut hits = self.ranked_hits(
            &BooleanQuery::new(clauses),
            &ranking,
            TOWN_PREFIX_WINDOW,
            &mut |hit| {
                let estimated = hit
                    .record
                    .point
                    .is_some_and(|point| point.precision == PointPrecision::Estimated);
                Ok(filters.admit(hit)
                    && wanted(&hit.record.layer)
                    && (hit.record.layer == Layer::Street.as_str() || estimated))
            },
        )?;
        let named_town = |hit: &TextSearchHit| {
            town.is_some_and(|town| {
                hit.record
                    .label
                    .split(", ")
                    .nth(1)
                    .and_then(search_key)
                    .is_some_and(|place| {
                        place.split_whitespace().any(|word| word.starts_with(town))
                    })
            })
        };
        hits.sort_by_key(|hit| {
            (
                hit.record.layer != Layer::Interpolation.as_str(),
                !named_town(hit),
            )
        });
        let words = street
            .iter()
            .cloned()
            .chain(town.map(str::to_string))
            .collect::<Vec<_>>();
        let highlighter = highlight::Highlighter::new(Completion::InOrder, None, &words, true);
        for hit in &mut hits {
            hit.features.prefix = true;
            hit.fallback = hit.record.layer == Layer::Street.as_str();
            hit.highlights = highlighter.ranges(&hit.record.label);
        }
        hits.truncate(limit);
        Ok(hits)
    }

    fn with_streets(
        &self,
        mut houses: Vec<TextSearchHit>,
        streets: Vec<TextSearchHit>,
    ) -> Vec<TextSearchHit> {
        let street_of = |hit: &TextSearchHit| match hit.record.layer.as_str() {
            "street" => search_key(&hit.record.name),
            _ => hit.record.address.street.as_deref().and_then(search_key),
        };
        let same = |left: &TextSearchHit, right: &TextSearchHit| {
            left.record.label == right.record.label
                || street_of(left).is_some_and(|name| street_of(right).as_ref() == Some(&name))
                    && left
                        .record
                        .point
                        .zip(right.record.point)
                        .is_some_and(|(a, b)| {
                            crate::util::geo::haversine_m(a.lon, a.lat, b.lon, b.lat)
                                <= SAME_STREET_M
                        })
        };
        for hit in streets {
            if !houses.iter().any(|held| same(held, &hit)) {
                houses.push(hit);
            }
        }
        houses
    }

    fn autocomplete_reading(
        &self,
        options: &TextAutocompleteOptions,
        completion: Completion,
        number: Option<&str>,
        words: &[String],
        typing: bool,
        filters: &Filters<'_>,
    ) -> Result<Vec<TextSearchHit>> {
        let limit = effective_autocomplete_limit(options.limit);
        let Some(query) = self.autocomplete_query(completion, number, words, typing, filters)?
        else {
            return Ok(Vec::new());
        };
        let ranking = Ranking {
            query_words: QueryWords::new(&options.query),
            number_texts: number.map(str::to_string).into_iter().collect(),
            focus: options.focus,
            near_first: options.focus.is_some(),
            typed_words: u8::try_from(words.len() + usize::from(number.is_some()))
                .unwrap_or(u8::MAX),
            whole_last: words
                .last()
                .map(|word| Term::from_field_text(self.fields.autocomplete_subject_text, word)),
            ..Ranking::default()
        };
        let mut hits = self.ranked_hits(&query, &ranking, limit + 1, &mut |hit| {
            Ok(filters.admit(hit))
        })?;
        for hit in &mut hits {
            let features = &mut hit.features;
            features.prefix = true;
            features.number = number.map(|_| states_number(&hit.record, &ranking));
            let label = label_words(&hit.record);
            features.dropped_words = count(words.iter().enumerate().filter(|(index, word)| {
                let last = index + 1 == words.len();
                !label.iter().any(|label| types_word(word, label, last))
            }));
        }
        if matches!(completion, Completion::AllWords | Completion::Typos) {
            hits.sort_by_cached_key(|hit| {
                let label = label_words(&hit.record);
                count(words.iter().enumerate().filter(|&(at, word)| {
                    unlisted_contraction(word)
                        && !label
                            .iter()
                            .any(|label| types_word(word, label, at + 1 == words.len()))
                }))
            });
        }
        let numbered = match number {
            Some(_) => Numbered::Yes,
            None => Numbered::No,
        };
        self.rate(&mut hits, numbered);
        hits.truncate(limit);
        let highlighter = highlight::Highlighter::new(completion, number, words, typing);
        for hit in &mut hits {
            hit.highlights = highlighter.ranges(&hit.record.label);
        }
        Ok(hits)
    }

    fn build_query(
        &self,
        plan: &QueryPlan,
        pass: Pass,
        filters: &Filters<'_>,
        areas: &AreaWords,
    ) -> Result<Box<dyn Query>> {
        let mut clauses = Vec::new();
        if let Some(number) = &plan.house_number {
            clauses.push((
                Occur::Must,
                self.house_number_query(number, plan.unit.as_deref(), pass)?,
            ));
        }
        clauses.extend(self.subject_clauses(plan, pass, areas));
        for word in &plan.optional {
            clauses.push((Occur::Should, self.word_query(word, &plan.spellings)));
        }
        clauses.push(self.product_clause());
        if let Some(layer) = filters
            .layer
            .map(str::trim)
            .filter(|layer| !layer.is_empty())
        {
            clauses.push((Occur::Must, self.layer_query(layer)));
        }
        if let Some(countries) = self.countries_query(filters.countries) {
            clauses.push((Occur::Must, countries));
        }
        if let Some(bbox) = filters.bbox {
            clauses.push((Occur::Must, self.bbox_query(bbox)));
        }
        Ok(Box::new(BooleanQuery::new(clauses)))
    }

    fn bbox_query(&self, bbox: BoundingBox) -> Box<dyn Query> {
        let degrees = |value: f64| i64::from(quantize(value).unwrap_or(0));
        let range = |field, lower: Bound<i64>, upper: Bound<i64>| -> Box<dyn Query> {
            Box::new(RangeQuery::new(
                lower.map(|value| Term::from_field_i64(field, value)),
                upper.map(|value| Term::from_field_i64(field, value)),
            ))
        };
        let fields = &self.fields;
        Box::new(ConstScoreQuery::new(
            Box::new(BooleanQuery::intersection(vec![
                range(
                    fields.min_lon,
                    Bound::Unbounded,
                    Bound::Included(degrees(bbox.max_lon)),
                ),
                range(
                    fields.max_lon,
                    Bound::Included(degrees(bbox.min_lon)),
                    Bound::Unbounded,
                ),
                range(
                    fields.min_lat,
                    Bound::Unbounded,
                    Bound::Included(degrees(bbox.max_lat)),
                ),
                range(
                    fields.max_lat,
                    Bound::Included(degrees(bbox.min_lat)),
                    Bound::Unbounded,
                ),
            ])),
            0.0,
        ))
    }

    fn subject_clauses(
        &self,
        plan: &QueryPlan,
        pass: Pass,
        areas: &AreaWords,
    ) -> Vec<(Occur, Box<dyn Query>)> {
        let mut clauses = Vec::new();
        let spellings = &plan.spellings;
        let as_written = |word: &str| {
            if plan.house_number.is_some() {
                self.street_word_query(word, spellings)
            } else {
                self.word_query(word, spellings)
            }
        };
        let what = |word: &str| -> Box<dyn Query> {
            let forms = plan.full_forms(word);
            if forms.is_empty() {
                return as_written(word);
            }
            Box::new(DisjunctionMaxQuery::new(
                std::iter::once(word.to_string())
                    .chain(forms)
                    .map(|form| as_written(&form))
                    .collect(),
            ))
        };
        let anywhere = plan.context.is_empty() || pass == Pass::Lenient;
        let stated = plan
            .context
            .iter()
            .flat_map(|part| &part.words)
            .collect::<HashSet<_>>();
        let mut named = Vec::new();
        let mut all_stand_in = true;
        for word in &plan.subject {
            let stands_in = anywhere && !stated.contains(&word.text);
            let query = if stands_in {
                self.word_or_area_query(what(&word.text), &word.text, spellings, areas)
            } else {
                what(&word.text)
            };
            if word.required {
                all_stand_in &= stands_in;
                named.push((Occur::Should, what(&word.text)));
            }
            let occur = if word.required {
                Occur::Must
            } else {
                Occur::Should
            };
            clauses.push((occur, query));
        }
        if all_stand_in && !named.is_empty() {
            clauses.push((
                Occur::Must,
                Box::new(ConstScoreQuery::new(
                    Box::new(BooleanQuery::new(named)),
                    0.0,
                )),
            ));
        }
        let unplaced = |words: &mut dyn Iterator<Item = &Word>| {
            words
                .map(|word| {
                    let query =
                        self.word_or_area_query(what(&word.text), &word.text, spellings, areas);
                    (Occur::Should, query)
                })
                .collect::<Vec<_>>()
        };
        if plan.subject.is_empty() {
            let (describing, naming): (Vec<_>, Vec<_>) = plan
                .unplaced
                .iter()
                .partition(|word| self.describes_street(&word.text));
            let (floored, ranked) = match naming.is_empty() {
                true => (describing, Vec::new()),
                false => (naming, describing),
            };
            let floored = unplaced(&mut floored.into_iter());
            let floor = unplaced_floor(floored.len());
            clauses.push((
                Occur::Must,
                Box::new(BooleanQuery::with_minimum_required_clauses(floored, floor)),
            ));
            clauses.extend(unplaced(&mut ranked.into_iter()));
        } else {
            clauses.extend(unplaced(&mut plan.unplaced.iter()));
        }
        for part in &plan.context {
            let stated = match pass.requires_locality() && part.label == Label::Locality {
                true => self.stated_names(part, &areas.known),
                false => None,
            };
            let clause = match stated {
                Some(stated) => (Occur::Must, self.locality_part_query(&stated)),
                None => (
                    Occur::Should,
                    self.context_part_query(part, spellings, areas),
                ),
            };
            clauses.push(clause);
        }
        clauses
    }

    fn word_or_area_query(
        &self,
        what: Box<dyn Query>,
        word: &str,
        spellings: &Spellings,
        areas: &AreaWords,
    ) -> Box<dyn Query> {
        let weight = areas.stand_ins.get(word).copied().unwrap_or_default();
        let as_written = Box::new(TermQuery::new(
            Term::from_field_text(self.fields.context_text, word),
            IndexRecordOption::Basic,
        ));
        let mut alternatives: Vec<Box<dyn Query>> = vec![
            what,
            Box::new(ConstScoreQuery::new(as_written, weight)),
            Box::new(ConstScoreQuery::new(
                self.area_query(word, spellings, areas),
                0.0,
            )),
        ];
        if let Some(postcode) = self.postcode_query(word, &areas.postcodes) {
            alternatives.push(Box::new(ConstScoreQuery::new(postcode, weight)));
        }
        Box::new(DisjunctionMaxQuery::new(alternatives))
    }

    fn stand_in_weights(&self, plan: &QueryPlan) -> Result<HashMap<String, Score>> {
        let searcher = self.reader.searcher();
        let field = if plan.house_number.is_some() {
            self.fields.street_text
        } else {
            self.fields.content_text
        };
        let documents = searcher.num_docs() as f64;
        let mut weights = HashMap::new();
        for word in plan
            .subject
            .iter()
            .map(|word| &word.text)
            .chain(plan.unplaced.iter().map(|word| &word.text))
        {
            if weights.contains_key(word) {
                continue;
            }
            let mut idf = f64::INFINITY;
            for field in [field, self.fields.context_text] {
                let found = searcher.doc_freq(&Term::from_field_text(field, word))? as f64;
                // Tantivy's BM25 idf.
                idf = idf.min((1.0 + (documents - found + 0.5) / (found + 0.5)).ln());
            }
            weights.insert(word.clone(), AREA_STANDS_IN_BOOST * idf as Score);
        }
        Ok(weights)
    }

    fn locality_part_query(&self, stated: &StatedNames) -> Box<dyn Query> {
        let name = |words: &Vec<String>| -> Box<dyn Query> {
            Box::new(BooleanQuery::new(
                words
                    .iter()
                    .map(|word| (Occur::Must, self.area_term_query(word)))
                    .collect(),
            ))
        };
        let occur = if stated.any {
            Occur::Should
        } else {
            Occur::Must
        };
        Box::new(BooleanQuery::new(
            stated
                .names
                .iter()
                .map(|words| (occur, name(words)))
                .collect(),
        ))
    }

    fn stated_names(&self, part: &ContextPart, known: &HashSet<String>) -> Option<StatedNames> {
        let words = &part.words;
        let dictionaries = self.tagger.dictionaries();
        let key = |words: &[String]| dictionary_key(&words.join(" "));
        let names_place = |key: &str| {
            dictionaries.contains(NameField::Locality, key)
                || dictionaries.contains(NameField::Region, key)
        };
        let known_words = |words: &[String]| words.iter().all(|word| known.contains(word));
        let is_name = |words: &[String]| {
            known_words(words) && key(words).is_some_and(|key| names_place(&key))
        };
        if is_name(words) {
            return Some(StatedNames {
                names: vec![words.clone()],
                any: false,
                whole: true,
            });
        }
        let mut names = Vec::<Vec<String>>::new();
        let mut start = 0;
        while start < words.len() {
            match (start + 1..=words.len())
                .rev()
                .find(|&end| is_name(&words[start..end]))
            {
                Some(end) => {
                    let name = words[start..end].to_vec();
                    if !names.contains(&name) {
                        names.push(name);
                    }
                    start = end;
                }
                None => start += 1,
            }
        }
        if names.is_empty() {
            let words = words
                .iter()
                .filter(|word| known.contains(*word))
                .cloned()
                .collect::<Vec<_>>();
            return (!words.is_empty()).then_some(StatedNames {
                names: vec![words],
                any: false,
                whole: true,
            });
        }
        let ids = names
            .iter()
            .map(|name| {
                key(name)
                    .map(|key| dictionaries.areas(&key))
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        let any = ids.iter().enumerate().any(|(index, left)| {
            ids[index + 1..].iter().any(|right| {
                !left.is_empty() && !right.is_empty() && !dictionaries.nest(left, right)
            })
        });
        Some(StatedNames {
            names,
            any,
            whole: false,
        })
    }

    fn stated_area(
        &self,
        part: &ContextPart,
        known: &HashSet<String>,
        postcode: Option<&str>,
    ) -> Result<Option<StatedArea>> {
        let names = match part.label {
            Label::Locality => self.stated_names(part, known),
            _ => StatedNames::known_words(part, known),
        };
        let Some(names) = names else {
            return Ok(None);
        };
        let postal = match (part.label, postcode) {
            (Label::Locality, Some(postcode)) => self.postal_town(&names, postcode)?,
            _ => false,
        };
        Ok(Some(StatedArea {
            label: part.label,
            names,
            postal,
        }))
    }

    fn postal_town(&self, names: &StatedNames, postcode: &str) -> Result<bool> {
        let searcher = self.reader.searcher();
        for name in &names.names {
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.postcode_exact, postcode),
                        IndexRecordOption::Basic,
                    )),
                ),
                (Occur::Must, self.layer_query(Layer::Address.as_str())),
            ];
            clauses.extend(
                name.iter()
                    .map(|word| (Occur::Must, self.area_term_query(word))),
            );
            if !searcher
                .search(&BooleanQuery::new(clauses), &TopDocs::with_limit(1))?
                .is_empty()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn stated_places(&self, context: &[ContextPart]) -> StatedPlaces {
        let dictionaries = self.tagger.dictionaries();
        let covered = |country: &CountryCode| dictionaries.country_weight(*country) > 0.0;
        let mut places = StatedPlaces::default();
        for part in context {
            let Some(key) = dictionary_key(&part.words.join(" ")) else {
                continue;
            };
            let country_of = |field| {
                dictionaries
                    .get(field, &key)
                    .and_then(|entry| entry.country)
                    .filter(covered)
            };
            match part.label {
                Label::Country => {
                    if let Some(country) = country_of(NameField::Country) {
                        places.country_words.extend(part.words.iter().cloned());
                        if !places.countries.contains(&country) {
                            places.countries.push(country);
                        }
                    }
                }
                Label::Region => {
                    let country = country_of(NameField::Region).filter(|country| {
                        dictionaries
                            .get(NameField::Country, &key)
                            .and_then(|entry| entry.country)
                            .is_none_or(|named| named == *country)
                    });
                    places.regions.push(StatedRegion {
                        words: part.words.clone(),
                        country,
                        regions: dictionaries
                            .regions(&key)
                            .into_iter()
                            .map(RecordId::from)
                            .collect(),
                    });
                }
                _ => {}
            }
        }
        places
    }

    fn region_agreement(
        &self,
        hit: &TextSearchHit,
        places: &StatedPlaces,
        session: &Session,
    ) -> Result<Option<bool>> {
        let (country, region) = self.place_agreement(hit, places, session)?;
        Ok(match (country, region) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (None, None) => None,
            _ => Some(true),
        })
    }

    fn place_agreement(
        &self,
        hit: &TextSearchHit,
        places: &StatedPlaces,
        session: &Session,
    ) -> Result<(Option<bool>, Option<bool>)> {
        let country = hit.features.country;
        let in_country = country
            .filter(|_| !places.countries.is_empty())
            .map(|country| places.countries.contains(&country));
        if in_country == Some(false) {
            return Ok((in_country, None));
        }
        let mut in_region = None;
        for region in &places.regions {
            if let (Some(stated), Some(country)) = (region.country, country)
                && stated != country
            {
                return Ok((in_country, Some(false)));
            }
            if region.regions.is_empty() {
                continue;
            }
            let chain = self.regions_of(hit.record_id, session)?;
            let stated_by_address = hit
                .record
                .address
                .region
                .as_deref()
                .and_then(search_key)
                .is_some_and(|stated| {
                    let words = stated.split_whitespace().collect::<HashSet<_>>();
                    region
                        .words
                        .iter()
                        .all(|word| words.contains(word.as_str()))
                });
            if stated_by_address || chain.iter().any(|id| region.regions.contains(id)) {
                in_region = Some(true);
            } else if !chain.is_empty() {
                return Ok((in_country, Some(false)));
            }
        }
        Ok((in_country, in_region))
    }

    fn known_area_words<'a>(
        &self,
        words: impl IntoIterator<Item = &'a String>,
    ) -> Result<HashSet<String>> {
        let searcher = self.reader.searcher();
        let mut known = HashSet::new();
        for word in words {
            if !known.contains(word)
                && searcher.doc_freq(&Term::from_field_text(self.fields.context_text, word))? > 0
            {
                known.insert(word.clone());
            }
        }
        Ok(known)
    }

    fn context_part_query(
        &self,
        part: &ContextPart,
        spellings: &Spellings,
        areas: &AreaWords,
    ) -> Box<dyn Query> {
        let mut alternatives = part
            .words
            .iter()
            .map(|word| (Occur::Should, self.area_query(word, spellings, areas)))
            .collect::<Vec<_>>();
        if part.words.len() > 1
            && let Some(postcode) = self.postcode_query(&part.words.join(" "), &areas.postcodes)
        {
            alternatives.push((Occur::Should, postcode));
        }
        Box::new(BooleanQuery::new(alternatives))
    }

    fn word_query(&self, word: &str, spellings: &Spellings) -> Box<dyn Query> {
        self.fields_query(
            word,
            spellings,
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

    fn street_word_query(&self, word: &str, spellings: &Spellings) -> Box<dyn Query> {
        self.spelt_query(
            self.fields.street_text,
            word,
            IndexRecordOption::WithFreqs,
            spellings,
        )
    }

    fn area_query(&self, word: &str, spellings: &Spellings, areas: &AreaWords) -> Box<dyn Query> {
        let context = self.spelt_query(
            self.fields.context_text,
            word,
            IndexRecordOption::WithFreqs,
            spellings,
        );
        let mut alternatives = vec![(Occur::Should, context)];
        if let Some(postcode) = self.postcode_query(word, &areas.postcodes) {
            alternatives.push((Occur::Should, postcode));
        }
        Box::new(BooleanQuery::new(alternatives))
    }

    fn postcode_query(&self, term: &str, postcodes: &PostcodeWeights) -> Option<Box<dyn Query>> {
        let weights = postcodes
            .terms
            .get(term)
            .filter(|weights| !weights.is_empty())?;
        let country = |country: CountryCode| -> Box<dyn Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(self.fields.country_code, country.as_str()),
                IndexRecordOption::Basic,
            ))
        };
        let clauses = weights
            .iter()
            .map(|&(in_country, weight)| {
                let mut parts: Vec<(Occur, Box<dyn Query>)> = vec![(
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.postcode_exact, term),
                        IndexRecordOption::Basic,
                    )),
                )];
                match in_country {
                    Some(in_country) => parts.push((Occur::Must, country(in_country))),
                    None => parts.extend(
                        self.pack
                            .postcode_addresses()
                            .countries()
                            .map(|other| (Occur::MustNot, country(other))),
                    ),
                }
                let query: Box<dyn Query> = Box::new(ConstScoreQuery::new(
                    Box::new(BooleanQuery::new(parts)),
                    weight,
                ));
                (Occur::Should, query)
            })
            .collect();
        Some(Box::new(BooleanQuery::new(clauses)))
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
        spellings: &Spellings,
        fields: &[(Field, Score, IndexRecordOption)],
    ) -> Box<dyn Query> {
        Box::new(BooleanQuery::new(
            fields
                .iter()
                .map(|&(field, boost, option)| {
                    let term = self.spelt_query(field, word, option, spellings);
                    (
                        Occur::Should,
                        Box::new(BoostQuery::new(term, boost)) as Box<dyn Query>,
                    )
                })
                .collect(),
        ))
    }

    fn spelt_query(
        &self,
        field: Field,
        word: &str,
        option: IndexRecordOption,
        spellings: &Spellings,
    ) -> Box<dyn Query> {
        let term = |text: &str, option| -> Box<dyn Query> {
            Box::new(TermQuery::new(Term::from_field_text(field, text), option))
        };
        let Some(spelt) = spellings.words.get(&(field, word.to_string())) else {
            return term(word, option);
        };
        let mut alternatives = vec![term(word, option)];
        if let Some(corrected) = &spelt.corrected {
            alternatives.push(Box::new(BoostQuery::new(
                term(corrected, option),
                CORRECTED_BOOST,
            )));
        }
        if !spelt.near.is_empty() {
            let near = spelt
                .near
                .iter()
                .map(|text| (Occur::Should, term(text, IndexRecordOption::Basic)))
                .collect();
            alternatives.push(Box::new(ConstScoreQuery::new(
                Box::new(BooleanQuery::new(near)),
                0.0,
            )));
        }
        Box::new(DisjunctionMaxQuery::new(alternatives))
    }

    fn spellings<'p>(&self, plans: impl Iterator<Item = &'p QueryPlan>) -> Result<Arc<Spellings>> {
        let searcher = self.reader.searcher();
        let mut spellings = Spellings::default();
        for plan in plans {
            let subject_fields = if plan.house_number.is_some() {
                vec![self.fields.street_text, self.fields.context_text]
            } else {
                vec![
                    self.fields.name_text,
                    self.fields.content_text,
                    self.fields.context_text,
                ]
            };
            let context_fields = [self.fields.context_text];
            let mut words = Vec::<(&str, Option<&str>, &[Field])>::new();
            for word in &plan.subject {
                words.push((&word.text, word.corrected.as_deref(), &subject_fields));
            }
            for word in &plan.unplaced {
                words.push((&word.text, word.corrected.as_deref(), &subject_fields));
            }
            for word in &plan.optional {
                words.push((word, None, &subject_fields));
            }
            for part in plan.context.iter().filter(|part| !part.is_postcode()) {
                for word in &part.words {
                    words.push((word, None, &context_fields));
                }
            }
            for (word, correction, fields) in words {
                let Some(most) = allowed_edits(word) else {
                    continue;
                };
                let dfa = automaton_builder(most).build_dfa(word);
                let first = first_chars(word);
                let correction = correction
                    .filter(|correction| {
                        correction.starts_with(first)
                            && matches!(dfa.eval(correction), Distance::Exact(_))
                    })
                    .map(str::to_string);
                for &field in fields {
                    if spellings.words.contains_key(&(field, word.to_string())) {
                        continue;
                    }
                    let near = self.near_terms(
                        &searcher,
                        field,
                        word,
                        &dfa,
                        first,
                        correction.as_deref(),
                    )?;
                    if near.is_empty() && correction.is_none() {
                        continue;
                    }
                    spellings.words.insert(
                        (field, word.to_string()),
                        Spelt {
                            corrected: correction.clone(),
                            near,
                        },
                    );
                }
            }
        }
        Ok(Arc::new(spellings))
    }

    fn near_terms(
        &self,
        searcher: &Searcher,
        field: Field,
        word: &str,
        dfa: &DFA,
        prefix: &str,
        corrected: Option<&str>,
    ) -> Result<Vec<String>> {
        let written = searcher.doc_freq(&Term::from_field_text(field, word))?;
        let upper = utf8_prefix_end(prefix);
        let mut found = HashMap::<Vec<u8>, (u8, u64)>::new();
        for segment in searcher.segment_readers() {
            let index = segment.inverted_index(field)?;
            let mut terms = index
                .terms()
                .search(Levenshtein(dfa))
                .ge(prefix)
                .lt(&upper)
                .into_stream()?;
            while terms.advance() {
                let term = terms.key();
                if term == word.as_bytes() || Some(term) == corrected.map(str::as_bytes) {
                    continue;
                }
                let Distance::Exact(distance) = dfa.eval(term) else {
                    continue;
                };
                found.entry(term.to_vec()).or_insert((distance, 0)).1 +=
                    u64::from(terms.value().doc_freq);
            }
        }
        let mut found = found
            .into_iter()
            .filter(|(_, (_, docs))| *docs > written)
            .collect::<Vec<_>>();
        found.sort_unstable_by(
            |(left, (left_edits, left_docs)), (right, (right_edits, right_docs))| {
                (left_edits, std::cmp::Reverse(left_docs), left).cmp(&(
                    right_edits,
                    std::cmp::Reverse(right_docs),
                    right,
                ))
            },
        );
        Ok(found
            .into_iter()
            .take(MAX_TYPO_TERMS)
            .filter_map(|(term, _)| String::from_utf8(term).ok())
            .collect())
    }

    fn with_number_forms(&self, mut plan: QueryPlan) -> Result<QueryPlan> {
        let Some(number) = plan.house_number.as_mut() else {
            return Ok(plan);
        };
        if number.plain().is_some() {
            return Ok(plan);
        }
        let sought = number_runs(&number.text);
        let base = number.base.to_string();
        // After the number, a space, or a letter, which sorts past ':'
        // (after '9'); no term holds the byte 0xFF, which UTF-8 never uses.
        let mut letters_end = base.as_bytes().to_vec();
        letters_end.push(0xFF);
        let ranges = [
            (
                format!("{base} ").into_bytes(),
                format!("{base}!").into_bytes(),
            ),
            (format!("{base}:").into_bytes(), letters_end),
        ];
        let mut forms = NumberForms::of(&number.text);
        let searcher = self.reader.searcher();
        for segment in searcher.segment_readers() {
            let index = segment.inverted_index(self.fields.address_number)?;
            for (from, to) in &ranges {
                let mut terms = index.terms().range().ge(from).lt(to).into_stream()?;
                while terms.advance() {
                    let Ok(term) = std::str::from_utf8(terms.key()) else {
                        continue;
                    };
                    let held = match number_fit(&sought, &number_runs(term)) {
                        Some(NumberFit::Stated) => &mut forms.stated,
                        Some(NumberFit::Abbreviated) => &mut forms.abbreviated,
                        None => continue,
                    };
                    if !held.iter().any(|form| form == term) {
                        held.push(term.to_string());
                    }
                }
            }
        }
        number.forms = forms;
        Ok(plan)
    }

    fn house_number_query(
        &self,
        number: &HouseNumber,
        unit: Option<&str>,
        pass: Pass,
    ) -> Result<Box<dyn Query>> {
        let term = |text: &str| {
            TermQuery::new(
                Term::from_field_text(self.fields.address_number, text),
                IndexRecordOption::Basic,
            )
        };
        let mut stated = number
            .forms
            .terms()
            .map(|spelling| {
                Box::new(BoostQuery::new(Box::new(term(spelling)), NUMBER_BOOST)) as Box<dyn Query>
            })
            .collect::<Vec<_>>();
        if let Some(unit) = unit {
            let searcher = self.reader.searcher();
            let documents = searcher.num_docs() as f32;
            let found = searcher.doc_freq(&Term::from_field_text(
                self.fields.address_number,
                &number.text,
            ))? as f32;
            // BM25 of one term once in a document without field norms.
            let idf = (1.0 + (documents - found + 0.5) / (found + 0.5)).ln();
            stated.push(Box::new(ConstScoreQuery::new(
                Box::new(term(&number.flat_text(unit))),
                NUMBER_BOOST * idf,
            )));
        }
        let mut alternatives: Vec<(Occur, Box<dyn Query>)> = vec![(
            Occur::Should,
            Box::new(DisjunctionMaxQuery::new(stated)) as Box<dyn Query>,
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
                Box::new(ConstScoreQuery::new(Box::new(in_range), RANGE_SCORE)),
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
        Ok(Box::new(BooleanQuery::new(alternatives)))
    }

    fn product_clause(&self) -> (Occur, Box<dyn Query>) {
        let occur = if self.places {
            Occur::Must
        } else {
            Occur::MustNot
        };
        (occur, self.layer_query(Layer::Poi.as_str()))
    }

    fn layer_query(&self, layer: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(self.fields.layer, layer),
            IndexRecordOption::Basic,
        ))
    }

    fn countries_query(&self, countries: &[String]) -> Option<Box<dyn Query>> {
        let clauses: Vec<(Occur, Box<dyn Query>)> = countries
            .iter()
            .map(|country| country.trim().to_ascii_uppercase())
            .filter(|country| !country.is_empty())
            .map(|country| {
                let term = Term::from_field_text(self.fields.country_code, &country);
                let query: Box<dyn Query> =
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                (Occur::Should, query)
            })
            .collect();
        (!clauses.is_empty()).then(|| {
            Box::new(ConstScoreQuery::new(
                Box::new(BooleanQuery::new(clauses)),
                0.0,
            )) as Box<dyn Query>
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn search_variants(
        &self,
        query_text: &str,
        context: &[ContextPart],
        filters: &Filters<'_>,
        focus: Option<[f64; 2]>,
        limit: usize,
        parses: Parses,
        pooled: Option<&mut Vec<TextSearchHit>>,
        structured_limit: Option<usize>,
        mut accept: impl FnMut(&Session, &QueryPlan, &QueryPostcode, &TextSearchHit) -> Result<bool>,
    ) -> Result<Variants> {
        let session = Session::new(self.budget);
        let (plans, scores, pieces, explanation, text_count) =
            self.readings(query_text, context, parses)?;
        let Some(first_plan) = plans.first() else {
            return Err(Error::UnparsableQuery {
                query: query_text.to_string(),
            });
        };
        crate::stop::check()?;
        let first_query = first_plan.text();
        let numbered = Numbered::of(first_plan);
        let named = choose::Named::of(first_plan);
        let countries = self.stated_places(&first_plan.context).countries;
        let order = match (numbered, pieces.first()) {
            (Numbered::No, Some(best)) if !self.places => {
                self.name_order(query_text, best, filters, focus)?
            }
            _ => None,
        };
        let stated = first_plan
            .context
            .iter()
            .map(|part| part.label)
            .collect::<Vec<_>>();
        let finish = |(query, mut hits): (String, Vec<TextSearchHit>),
                      order: Option<names::NameOrder>| {
            for hit in &mut hits {
                unjudged_unmatched(&stated, &mut hit.matched);
            }
            Variants {
                query,
                hits,
                numbered,
                order,
                named,
                countries,
            }
        };
        let placed = self.known_area_words(
            first_plan
                .context
                .iter()
                .filter(|part| part.label == Label::Locality)
                .flat_map(|part| &part.words),
        )?;
        let mut query_words = QueryWords::new(query_text);
        query_words
            .search
            .extend(context.iter().flat_map(|part| part.words.iter().cloned()));
        let search = Search {
            placed: &placed,
            query_words: &query_words,
            filters: *filters,
            focus,
            limit,
        };
        let mut readings = plans
            .into_iter()
            .zip(scores)
            .zip(pieces)
            .map(|((plan, score), pieces)| Reading {
                text: plan.text(),
                plan,
                score,
                pieces,
            })
            .collect::<Vec<_>>();
        let extra = readings.split_off(text_count.min(readings.len()));
        let parsed = readings.len();
        for index in 0..parsed {
            if let Some(plan) = self.trimmed(&readings[index].plan)
                && !readings.iter().any(|held| held.plan == plan)
            {
                readings.push(Reading {
                    text: plan.text(),
                    plan,
                    score: readings[index].score,
                    pieces: readings[index].pieces.clone(),
                });
            }
        }
        let mut first = None;
        let mut declined = None;
        if let Some(wanted) = structured_limit
            && let Some(structured) = &self.structured
        {
            let structured_readings = readings[..parsed]
                .iter()
                .cloned()
                .chain(extra)
                .collect::<Vec<_>>();
            let start = Instant::now();
            let cx = StructuredContext {
                structured,
                readings: &structured_readings,
                parsed: structured_readings.len(),
                search: &search,
                session: &session,
                explanation: explanation.as_ref(),
                probabilities: explanation
                    .as_ref()
                    .map(|explanation| self.tagger.place_probabilities(explanation))
                    .unwrap_or_default(),
            };
            let answer = self.structured_stage(&cx, &mut accept, &mut declined)?;
            let spent = start.elapsed();
            let settles = match (&answer, &order) {
                (Some(answer), Some(order)) => self.answers_first(order, &answer.1)?,
                _ => true,
            };
            match answer {
                Some(answer) if answer.1.len() >= wanted && settles => {
                    return Ok(finish(answer, order));
                }
                Some(answer) => first = Some(answer),
                None => {}
            }
            session.extend(spent);
        }
        crate::stop::check()?;
        let spellings = self.spellings(readings.iter().map(|reading| &reading.plan))?;
        for reading in &mut readings {
            reading.plan = self.spelt(&reading.plan, &spellings)?;
        }
        let mut pool = Pool::default();
        self.gather(
            &readings,
            0..parsed,
            &search,
            &session,
            &mut accept,
            &mut pool,
        )?;
        if !pool.answers() {
            self.gather(
                &readings,
                parsed..readings.len(),
                &search,
                &session,
                &mut accept,
                &mut pool,
            )?;
        }
        if let Some(explanation) = &explanation {
            self.explain(explanation, &session, &mut pool)?;
        }
        pool.weigh(&readings);
        if let Some(structured) = &self.structured
            && let Some(reading) = readings.first()
        {
            let named = self.named_places(structured, reading);
            Self::pool_named(structured, &named, &mut pool);
        }
        if let Some(pooled) = pooled {
            pooled.clear();
            for candidate in &pool.candidates {
                if !pooled
                    .iter()
                    .any(|held| held.record_id == candidate.hit.record_id)
                {
                    pooled.push(candidate.hit.clone());
                }
            }
        }
        let (query, hits) = match self.ranker.as_ref().filter(|_| numbered == Numbered::Yes) {
            Some(ranker) => pool.ranked(ranker, &readings, limit),
            None => pool.chosen(&readings, limit),
        }
        .unwrap_or((first_query, Vec::new()));
        if hits.is_empty()
            && first.is_none()
            && let Some(declined) = declined
        {
            let (hits, _) =
                self.structured_judged(&declined.plan, declined.hits, &search, &session)?;
            return Ok(finish((declined.text, hits), order));
        }
        Ok(finish(
            match first {
                Some((query, mut first)) => {
                    for hit in hits {
                        if !first.iter().any(|held| held.record_id == hit.record_id) {
                            first.push(hit);
                        }
                    }
                    first.truncate(limit);
                    (query, first)
                }
                None => (query, hits),
            },
            order,
        ))
    }

    fn structured_stage(
        &self,
        cx: &StructuredContext<'_, '_>,
        accept: &mut impl FnMut(&Session, &QueryPlan, &QueryPostcode, &TextSearchHit) -> Result<bool>,
        declined: &mut Option<Declined>,
    ) -> Result<Option<(String, Vec<TextSearchHit>)>> {
        let streets = cx.readings[..cx.parsed]
            .iter()
            .enumerate()
            .filter(|(_, reading)| !reading.plan.subject.is_empty())
            .collect::<Vec<_>>();
        let numbered = streets
            .iter()
            .filter(|(_, reading)| reading.plan.house_number.is_some())
            .copied()
            .collect::<Vec<_>>();
        let Some(explanation) = cx.explanation else {
            return Ok(None);
        };
        let (readings, numbers) = match numbered.is_empty() {
            true => (streets, false),
            false => (numbered, true),
        };
        let (mut found, candidates) = match readings.is_empty() {
            true => (Vec::new(), 0),
            false => self.structured_found(cx, &readings, numbers, accept)?,
        };
        let named = cx
            .readings
            .first()
            .map(|reading| self.named_places(cx.structured, reading))
            .unwrap_or_default();
        Self::structured_named(cx.structured, &named, &mut found);
        if found.is_empty() {
            let all = cx.readings[..cx.parsed]
                .iter()
                .enumerate()
                .collect::<Vec<_>>();
            found = self.structured_areas(cx, &all)?;
            Self::structured_named(cx.structured, &named, &mut found);
        }
        let Some(explained) = self.structured_explained(cx, explanation, found)? else {
            return Ok(None);
        };
        let best = &explained.scored[0];
        let margin = best.score - explained.second;
        #[cfg(test)]
        LAST_STRUCTURED.with(|last| last.set(false));
        let features = structured_features(
            best.found.form,
            best.found.lever,
            best.gap,
            margin,
            best.found.support,
            best.found.reading,
            explained.spots,
            candidates,
        );
        if best.found.form == FORM_STREET && best.found.support == 0 {
            return Ok(None);
        }
        if best.found.form == FORM_STREET && margin <= 0.0 {
            return Ok(None);
        }
        if best.found.form < STRUCTURED_MIN_FORM && best.found.form != FORM_STREET {
            return Ok(None);
        }
        let reading = &cx.readings[best.found.reading];
        let hits_of = |explained: StructuredExplained| {
            explained
                .scored
                .into_iter()
                .map(|scored| scored.found.hit)
                .take(cx.search.limit)
                .collect::<Vec<_>>()
        };
        let decline = |hits: Vec<TextSearchHit>| {
            Some(Declined {
                text: reading.text.clone(),
                plan: reading.plan.clone(),
                hits,
            })
        };
        if logistic(&P_STRUCTURED, &features) - logistic(&P_TEXT, &features) < STRUCTURED_DELTA {
            *declined = decline(hits_of(explained));
            return Ok(None);
        }
        let lone_name = reading.plan.context.is_empty()
            && !reading
                .plan
                .subject
                .iter()
                .any(|word| self.describes_street(&word.text));
        if best.found.form == FORM_STREET && lone_name {
            *declined = decline(hits_of(explained));
            return Ok(None);
        }
        let (hits, towns_known) =
            self.structured_judged(&reading.plan, hits_of(explained), cx.search, cx.session)?;
        if !self.verified(&hits[0], towns_known, cx) {
            if Self::in_stated_towns(&hits[0].features, towns_known) {
                *declined = decline(hits);
            }
            return Ok(None);
        }
        #[cfg(test)]
        LAST_STRUCTURED.with(|last| last.set(true));
        Ok(Some((reading.text.clone(), hits)))
    }

    fn structured_judged(
        &self,
        plan: &QueryPlan,
        hits: Vec<TextSearchHit>,
        search: &Search<'_>,
        session: &Session,
    ) -> Result<(Vec<TextSearchHit>, bool)> {
        let postcode = self.query_postcode(plan)?;
        let stated = (!postcode.forms.is_empty()).then(|| postcode.exact.clone());
        let (_, localities) = self.localities_stated(plan, stated.as_deref())?;
        let towns_known = localities.iter().all(Option::is_some);
        let ranking = self.plan_ranking(plan, search.query_words, search.focus, postcode);
        let order = hits.iter().map(|hit| hit.record_id).collect::<Vec<_>>();
        let (kept, contradicted) = self.judged(
            plan,
            Scope::Scoped,
            &ranking,
            &localities,
            &self.stated_places(&plan.context),
            hits,
            session,
            usize::MAX,
        )?;
        let mut hits = kept.into_iter().chain(contradicted).collect::<Vec<_>>();
        hits.sort_by_key(|hit| order.iter().position(|id| *id == hit.record_id));
        Ok((hits, towns_known))
    }

    fn structured_areas(
        &self,
        cx: &StructuredContext<'_, '_>,
        readings: &[(usize, &Reading)],
    ) -> Result<Vec<StructuredFound>> {
        let dictionaries = self.tagger.dictionaries();
        let mut found = Vec::new();
        for &(index, reading) in readings {
            let mut parts = Vec::<(Label, Vec<RecordId>)>::new();
            for part in &reading.plan.context {
                let Some(key) = lattice::dictionary_key(&part.words.join(" ")) else {
                    continue;
                };
                let ids = match part.label {
                    Label::Postcode => match compact_postcode(&part.words.join(" ")) {
                        Some(postcode) => self.postcode_records(&[postcode], AREA_ANSWERS)?,
                        None => Vec::new(),
                    },
                    Label::Locality => dictionaries
                        .areas(&key)
                        .into_iter()
                        .map(RecordId::from)
                        .collect(),
                    Label::Region => dictionaries
                        .regions(&key)
                        .into_iter()
                        .chain(dictionaries.areas(&key))
                        .map(RecordId::from)
                        .collect(),
                    _ => continue,
                };
                if !ids.is_empty() {
                    parts.push((part.label, ids));
                }
            }
            for finest in [Label::Postcode, Label::Locality, Label::Region] {
                let Some(at) = parts.iter().position(|(label, _)| *label == finest) else {
                    continue;
                };
                let mut kept = Vec::new();
                for &id in &parts[at].1 {
                    let mut nests = true;
                    for (other, (label, ids)) in parts.iter().enumerate() {
                        if other == at || *label == Label::Postcode {
                            continue;
                        }
                        let holds = match finest {
                            Label::Postcode => self.postcode_in(id, ids)?,
                            _ => dictionaries.nest(
                                &[u32::try_from(id).unwrap_or(u32::MAX)],
                                &ids.iter()
                                    .filter_map(|id| u32::try_from(*id).ok())
                                    .collect::<Vec<_>>(),
                            ),
                        };
                        nests &= holds;
                    }
                    if nests {
                        kept.push(id);
                    }
                }
                if finest == Label::Postcode
                    && !kept.is_empty()
                    && let Some(street) = self.street_in_postcode(cx, reading, index)?
                {
                    found.push(street);
                    break;
                }
                for id in kept.into_iter().take(AREA_ANSWERS) {
                    let record = self.pack.record_summary(id)?;
                    let Some(point) = record.point else {
                        continue;
                    };
                    let form = match Layer::parse(&record.layer) {
                        Some(Layer::Postcode) => FORM_POSTCODE,
                        Some(Layer::Region | Layer::Country) => FORM_REGION,
                        _ => FORM_TOWN,
                    };
                    let hit = TextSearchHit {
                        named_place: false,
                        record_id: id,
                        score: 0.0,
                        record,
                        features: Features::default(),
                        confidence: None,
                        fallback: false,
                        signals: Signals::default(),
                        matched: QueryMatch::default(),
                        highlights: Vec::new(),
                    };
                    if !cx.search.filters.admit(&hit) {
                        continue;
                    }
                    found.push(StructuredFound {
                        hit,
                        point: [point.lon, point.lat],
                        support: 1000 * (parts.len() - 1),
                        form,
                        reading: index,
                        entry: "area",
                        lever: LEVER_T,
                    });
                }
                break;
            }
        }
        if found.iter().any(|found| found.entry == "postcode-street") {
            found.retain(|found| found.entry == "postcode-street");
        }
        Ok(found)
    }

    fn street_in_postcode(
        &self,
        cx: &StructuredContext<'_, '_>,
        reading: &Reading,
        index: usize,
    ) -> Result<Option<StructuredFound>> {
        let structured = cx.structured;
        let plan = &reading.plan;
        let words = plan
            .subject
            .iter()
            .map(|word| word.text.clone())
            .collect::<Vec<_>>();
        let Some(key) = self.street_keys(&words).into_iter().next() else {
            return Ok(None);
        };
        let query = key.split_whitespace().collect::<Vec<_>>();
        let countries = self.structured_countries(plan, &cx.search.filters);
        let mut cells = Vec::new();
        for part in plan
            .context
            .iter()
            .filter(|part| part.label == Label::Postcode)
        {
            let Some(postcode) = compact_postcode(&part.words.join(" ")) else {
                continue;
            };
            for place in structured.places(&postcode, &countries, false) {
                cells.extend(structured.place_cells(&place));
            }
        }
        cells.sort_unstable();
        cells.dedup();
        if cells.is_empty() || cells.len() > MATCH_CELLS {
            return Ok(None);
        }
        let place_words = context_words(plan);
        let mut names = structured
            .streets_in(&cells, &countries)
            .into_iter()
            .filter_map(|name| {
                let describes = |word: &str| self.describes_street(word);
                Some((
                    street_words_match(&query, &name, describes)
                        .or_else(|| street_named_in(&query, &name, describes, &place_words))?,
                    name,
                ))
            })
            .collect::<Vec<_>>();
        names.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
        let searcher = self.reader.searcher();
        for (_, name) in names.into_iter().take(MATCHED_STREETS) {
            let mut docs = Vec::new();
            for list in structured.street_lists(&name, &countries) {
                for &cell in &cells {
                    let (from, to) =
                        structured.list_range(&list, cell, cell, &|doc| self.doc_cell(doc));
                    docs.extend((from..to).map(|at| structured.list_doc(&list, at)));
                }
            }
            if docs.is_empty() {
                continue;
            }
            let numbered = match plan.house_number {
                Some(_) => self.number_forms(&searcher, &docs, plan, None)?,
                None => Vec::new(),
            };
            let (doc, form) = match numbered.iter().max_by_key(|(_, form, _)| *form) {
                Some(&(doc, form, _)) => (doc, form),
                None => (docs[0], FORM_STREET),
            };
            let Some(address) = self.doc_address(doc) else {
                continue;
            };
            let street_plan = QueryPlan {
                house_number: None,
                ..plan.clone()
            };
            let ranking = self.plan_ranking(
                match form {
                    FORM_STREET => &street_plan,
                    _ => plan,
                },
                cx.search.query_words,
                cx.search.focus,
                self.query_postcode_unplaced(plan)?,
            );
            let hit = self
                .rank_documents(
                    &searcher,
                    vec![(RankKey::text(0.0), address)],
                    &ranking,
                    1,
                    &mut |hit| Ok(cx.search.filters.admit(hit)),
                )?
                .into_iter()
                .next();
            if let Some(mut hit) = hit
                && let Some(point) = hit.record.point
            {
                if form == FORM_STREET && hit.record.layer != Layer::Street.as_str() {
                    continue;
                }
                let mut point = [point.lon, point.lat];
                if form == FORM_STREET
                    && let Some(middle) = self.postcode_road_middle(plan, &docs)
                {
                    point = middle;
                    street_at(&mut hit.record, middle);
                }
                return Ok(Some(StructuredFound {
                    hit,
                    point,
                    support: 1000,
                    form,
                    reading: index,
                    entry: "postcode-street",
                    lever: LEVER_T,
                }));
            }
        }
        Ok(None)
    }

    fn postcode_road_middle(&self, plan: &QueryPlan, docs: &[DocId]) -> Option<[f64; 2]> {
        let wanted = plan
            .context
            .iter()
            .filter(|part| part.label == Label::Postcode)
            .filter_map(|part| compact_postcode(&part.words.join(" ")))
            .map(|postcode| postcode_letters(&postcode))
            .collect::<Vec<_>>();
        let mut inside = Vec::new();
        let mut near = Vec::new();
        for &doc in docs {
            if self.doc_layer(doc) != Some(structured::LAYER_ADDRESS) {
                continue;
            }
            let Some([lon, lat]) = self.doc_point(doc) else {
                continue;
            };
            let point = [dequantize(lon), dequantize(lat)];
            near.push(point);
            if wanted.contains(&postcode_letters(&self.doc_postcodes(doc).0)) {
                inside.push(point);
            }
        }
        road::road_middle(match inside.is_empty() {
            true => &near,
            false => &inside,
        })
    }

    fn postcode_in(&self, postcode: RecordId, areas: &[RecordId]) -> Result<bool> {
        let Some(context) = self.pack.boundary_context(postcode)? else {
            return Ok(true);
        };
        let chain = context
            .admin_context
            .entries()
            .iter()
            .map(|entry| entry.record_id)
            .collect::<Vec<_>>();
        Ok(chain.is_empty() || chain.iter().any(|id| areas.contains(id)))
    }

    fn structured_found(
        &self,
        cx: &StructuredContext<'_, '_>,
        readings: &[(usize, &Reading)],
        numbered: bool,
        accept: &mut impl FnMut(&Session, &QueryPlan, &QueryPostcode, &TextSearchHit) -> Result<bool>,
    ) -> Result<(Vec<StructuredFound>, usize)> {
        let mut found = Vec::new();
        let mut candidates = 0;
        for &(index, reading) in readings {
            let (more, documents, settled) =
                self.structured_reading(cx, index, reading, numbered, accept)?;
            found.extend(more);
            candidates += documents;
            if settled {
                break;
            }
        }
        Ok((found, candidates))
    }

    fn structured_reading(
        &self,
        cx: &StructuredContext<'_, '_>,
        index: usize,
        reading: &Reading,
        numbered: bool,
        accept: &mut impl FnMut(&Session, &QueryPlan, &QueryPostcode, &TextSearchHit) -> Result<bool>,
    ) -> Result<(Vec<StructuredFound>, usize, bool)> {
        let structured = cx.structured;
        let plan = &reading.plan;
        let countries = self.structured_countries(plan, &cx.search.filters);
        let mut lists = Vec::new();
        let mut entry = "street";
        for (street, _) in self.query_street_keys(plan) {
            lists.extend(structured.street_lists(&street, &countries));
        }
        let mut lever = 0;
        if numbered && plan.number_kept {
            lever |= LEVER_C;
        }
        let subject = plan
            .subject
            .iter()
            .map(|word| word.text.clone())
            .collect::<Vec<_>>();
        if !lists.is_empty() && !self.tagger.names_street(&subject) {
            lever |= LEVER_D;
        }
        if !numbered {
            lever |= LEVER_F;
        }
        let places = self.structured_places(structured, reading, &countries, &cx.probabilities);
        let placed = places.weights.iter().sum::<usize>().max(1);
        if lists.is_empty() {
            lists = self.street_matches(structured, plan, &places, &countries);
            if lists.is_empty() {
                return Ok((Vec::new(), 0, false));
            }
            entry = "place";
            lever |= LEVER_A;
        }
        lists
            .retain(|list| places.countries.is_empty() || places.countries.contains(&list.country));
        let mut raw = lists.iter().map(structured::StreetList::len).sum::<usize>();
        if raw == 0 {
            return Ok((Vec::new(), 0, false));
        }
        let mut supported = Vec::new();
        for list in &lists {
            self.list_support(
                structured,
                list,
                &places.words,
                &places.weights,
                &mut supported,
            );
        }
        if supported.is_empty() && !places.words.is_empty() && lever & LEVER_A == 0 {
            let mut more = self.street_matches(structured, plan, &places, &countries);
            more.retain(|list| {
                !lists.contains(list)
                    && (places.countries.is_empty() || places.countries.contains(&list.country))
            });
            let mut found = Vec::new();
            for list in &more {
                self.list_support(structured, list, &places.words, &places.weights, &mut found);
            }
            if !found.is_empty() {
                raw += more.iter().map(structured::StreetList::len).sum::<usize>();
                lists.extend(more);
                supported = found;
                lever |= LEVER_A;
                entry = "place";
            }
        }
        supported.sort_unstable_by(|left: &(DocId, usize), right| {
            left.0.cmp(&right.0).then(right.1.cmp(&left.1))
        });
        supported.dedup_by_key(|(doc, _)| *doc);
        let most = supported.iter().map(|(_, support)| *support).max();
        let all_docs = || {
            lists
                .iter()
                .flat_map(|list| (0..list.len()).map(move |at| structured.list_doc(list, at)))
                .collect::<Vec<_>>()
        };
        let searcher = self.reader.searcher();
        let base_plan = plan.with_base_house_number();
        let ranking = self.plan_ranking(
            plan,
            cx.search.query_words,
            cx.search.focus,
            self.query_postcode_unplaced(plan)?,
        );
        let base_ranking = match &base_plan {
            Some(base) => Some(self.plan_ranking(
                base,
                cx.search.query_words,
                cx.search.focus,
                self.query_postcode_unplaced(plan)?,
            )),
            None => None,
        };
        let street_plan = QueryPlan {
            house_number: None,
            ..plan.clone()
        };
        let street_ranking = self.plan_ranking(
            &street_plan,
            cx.search.query_words,
            cx.search.focus,
            self.query_postcode_unplaced(plan)?,
        );
        let descriptors = self.descriptors(plan.subject.iter().map(|word| word.text.as_str()));
        let mut keep = |hit: &TextSearchHit| -> Result<bool> {
            crate::stop::check()?;
            if !cx.search.filters.admit(hit) || self.street_contradicts(&descriptors, hit) {
                return Ok(false);
            }
            accept(cx.session, plan, &ranking.postcode, hit)
        };
        let postcode = |doc: DocId, ranking: &Ranking| -> u8 {
            if ranking.postcode.forms.is_empty() {
                return 0;
            }
            let (own, area) = self.doc_postcodes(doc);
            let own = match own.is_empty() {
                true => Vec::new(),
                false => vec![own],
            };
            let area = (!area.is_empty()).then_some(area.as_str());
            ranking.postcode.rank_of(&own, area)
        };
        let spots_of = |candidates: Vec<(DocId, usize, u8, bool)>| {
            let mut keyed = candidates
                .into_iter()
                .filter_map(|(doc, support, form, base)| {
                    let [lon, lat] = self.doc_point(doc)?;
                    let layer = self.doc_layer(doc)?;
                    let ranking = match base {
                        true => base_ranking.as_ref()?,
                        false => &ranking,
                    };
                    let street = form == FORM_STREET && layer == structured::LAYER_STREET;
                    Some(Keyed {
                        order: (support, form, street, postcode(doc, ranking)),
                        doc,
                        base,
                        address: layer == structured::LAYER_ADDRESS,
                        point: [dequantize(lon), dequantize(lat)],
                    })
                })
                .collect::<Vec<_>>();
            keyed
                .sort_by(|left, right| right.order.cmp(&left.order).then(left.doc.cmp(&right.doc)));
            let mut spots = Vec::<Spot>::new();
            for item in keyed {
                let room = spots.len() < STRUCTURED_SPOTS;
                let near = spots.iter_mut().find(|spot| {
                    haversine_m(
                        spot.head.point[0],
                        spot.head.point[1],
                        item.point[0],
                        item.point[1],
                    ) <= SPOT_M
                });
                match near {
                    Some(spot) => {
                        if spot.docs.len() < NEIGHBOUR_DOCUMENTS {
                            spot.docs.push(item.doc);
                        }
                        if item.order == spot.head.order
                            && item.base == spot.head.base
                            && spot.tied.len() < NEIGHBOUR_DOCUMENTS
                        {
                            spot.tied.push(item.doc);
                        }
                        if item.address && spot.points.len() < SPOT_POINTS {
                            spot.points.push(item.point);
                        }
                    }
                    None if room => spots.push(Spot {
                        docs: vec![item.doc],
                        tied: vec![item.doc],
                        points: match item.address {
                            true => vec![item.point],
                            false => Vec::new(),
                        },
                        head: item,
                    }),
                    None => {}
                }
            }
            spots
        };
        let mut candidates = Vec::<(DocId, usize, u8, bool)>::new();
        if numbered {
            let docs = supported.iter().map(|(doc, _)| *doc).collect::<Vec<_>>();
            let forms = self.number_forms(&searcher, &docs, plan, base_plan.as_ref())?;
            let support_of = |doc: DocId| {
                supported
                    .binary_search_by_key(&doc, |(held, _)| *held)
                    .map_or(0, |at| supported[at].1)
            };
            let numbered_most = forms.iter().map(|(doc, _, _)| support_of(*doc)).max();
            candidates.extend(
                forms
                    .iter()
                    .filter(|(doc, _, _)| Some(support_of(*doc)) == numbered_most)
                    .map(|&(doc, form, base)| (doc, support_of(doc), form, base)),
            );
            if candidates.is_empty()
                && let Some(most) = most
            {
                let best = supported
                    .iter()
                    .filter(|(_, support)| *support == most)
                    .map(|&(doc, support)| (doc, support, FORM_STREET, false))
                    .collect::<Vec<_>>();
                let best_docs = best.iter().map(|(doc, ..)| *doc).collect::<Vec<_>>();
                let spots = spots_of(best);
                let mut found = Vec::new();
                for spot in &spots {
                    if let Some(hit) = self.structured_side(plan, &ranking, &spot.docs)?
                        && keep(&hit)?
                        && let Some(point) = hit.record.point
                    {
                        found.push(StructuredFound {
                            hit,
                            point: [point.lon, point.lat],
                            support: most,
                            form: FORM_INTERPOLATION,
                            reading: index,
                            entry,
                            lever: lever | LEVER_G,
                        });
                    }
                }
                if found.is_empty() && !self.pack.tiger_ranges.is_empty() && plan.unit.is_none() {
                    let mut estimates = Vec::new();
                    for spot in &spots {
                        if let Some(street) =
                            self.street_answer(&searcher, spot, &street_ranking, &mut keep)?
                            && let Some(hit) = self.tiger_estimate(&street, &ranking)?
                            && keep(&hit)?
                        {
                            estimates.push(hit);
                        }
                    }
                    if let [hit] = estimates.as_slice()
                        && let Some(point) = hit.record.point
                    {
                        found.push(StructuredFound {
                            hit: hit.clone(),
                            point: [point.lon, point.lat],
                            support: most,
                            form: FORM_INTERPOLATION,
                            reading: index,
                            entry,
                            lever: lever | LEVER_G,
                        });
                    }
                }
                if found.is_empty()
                    && let Some(hit) = self.structured_neighbour(plan, &ranking, &best_docs)?
                    && keep(&hit)?
                    && let Some(point) = hit.record.point
                {
                    found.push(StructuredFound {
                        hit,
                        point: [point.lon, point.lat],
                        support: most,
                        form: FORM_NEIGHBOUR,
                        reading: index,
                        entry,
                        lever,
                    });
                }
                if found.is_empty() {
                    let mut pieces = Vec::new();
                    for spot in &spots {
                        if let Some(street) =
                            self.street_answer(&searcher, spot, &street_ranking, &mut keep)?
                        {
                            let found = StructuredFound {
                                point: [
                                    street.record.point.map_or(0.0, |point| point.lon),
                                    street.record.point.map_or(0.0, |point| point.lat),
                                ],
                                hit: street,
                                support: most,
                                form: FORM_STREET,
                                reading: index,
                                entry,
                                lever: lever | LEVER_G,
                            };
                            pieces.push((found, spot.points.clone()));
                        }
                    }
                    found = self.join_road_pieces(pieces);
                    self.postcode_stretch(plan, &best_docs, &mut found);
                }
                found.retain(|found| found.form != FORM_STREET || found.support >= placed);
                if !found.is_empty() {
                    return Ok((found, raw, false));
                }
            }
            if candidates.is_empty() {
                if raw > STRUCTURED_CANDIDATES {
                    return Ok((Vec::new(), raw, false));
                }
                let forms = self.number_forms(&searcher, &all_docs(), plan, base_plan.as_ref())?;
                candidates.extend(
                    forms
                        .into_iter()
                        .map(|(doc, form, base)| (doc, 0, form, base)),
                );
            }
            if candidates.is_empty() {
                return Ok((Vec::new(), raw, false));
            }
        } else {
            match most {
                Some(most) => candidates.extend(
                    supported
                        .iter()
                        .filter(|(_, support)| *support == most)
                        .map(|&(doc, support)| (doc, support, FORM_STREET, false)),
                ),
                None if raw <= STRUCTURED_CANDIDATES => candidates.extend(
                    all_docs()
                        .into_iter()
                        .map(|doc| (doc, 0, FORM_STREET, false)),
                ),
                None => return Ok((Vec::new(), raw, false)),
            }
            candidates.sort_unstable_by_key(|(doc, _, _, _)| *doc);
            candidates.dedup_by_key(|(doc, _, _, _)| *doc);
        }
        let candidate_docs = candidates.iter().map(|(doc, ..)| *doc).collect::<Vec<_>>();
        let spots = spots_of(candidates);
        let mut found = Vec::new();
        let mut pieces = Vec::new();
        for spot in &spots {
            let (support, form, _, agreement) = spot.head.order;
            let hit = match form {
                FORM_STREET => self.street_answer(&searcher, spot, &street_ranking, &mut keep)?,
                _ => {
                    let doc = self.best_tied_document(&searcher, plan, &spot.tied)?;
                    let Some(address) = self.doc_address(doc) else {
                        continue;
                    };
                    let ranking = match spot.head.base {
                        true => base_ranking
                            .as_ref()
                            .expect("a base ranking for a base number"),
                        false => &ranking,
                    };
                    let mut key = RankKey::text(0.0);
                    key.stated = form >= FORM_RANGE_START;
                    key.postcode_agreement = agreement;
                    self.rank_documents(&searcher, vec![(key, address)], ranking, 1, &mut keep)?
                        .into_iter()
                        .next()
                }
            };
            if let Some(hit) = hit
                && let Some(point) = hit.record.point
            {
                let one = StructuredFound {
                    hit,
                    point: [point.lon, point.lat],
                    support,
                    form,
                    reading: index,
                    entry,
                    lever,
                };
                match form {
                    FORM_STREET => pieces.push((one, spot.points.clone())),
                    _ => found.push(one),
                }
            }
        }
        found.extend(self.join_road_pieces(pieces));
        self.postcode_stretch(plan, &candidate_docs, &mut found);
        found.retain(|found| found.form != FORM_STREET || found.support >= placed);
        let settled = found
            .iter()
            .any(|found| found.form == FORM_EXACT && found.support >= placed);
        Ok((found, raw, settled))
    }

    fn street_answer(
        &self,
        searcher: &Searcher,
        spot: &Spot,
        ranking: &Ranking,
        keep: &mut impl FnMut(&TextSearchHit) -> Result<bool>,
    ) -> Result<Option<TextSearchHit>> {
        let Some(address) = self.doc_address(spot.head.doc) else {
            return Ok(None);
        };
        let mut key = RankKey::text(0.0);
        key.postcode_agreement = spot.head.order.3;
        let Some(mut hit) = self
            .rank_documents(searcher, vec![(key, address)], ranking, 1, keep)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        if hit.record.layer != Layer::Street.as_str() {
            return Ok(None);
        }
        let record = &mut hit.record;
        let point =
            densest(&spot.points).or_else(|| record.point.map(|point| [point.lon, point.lat]));
        let Some(point) = point else {
            return Ok(None);
        };
        street_at(record, point);
        Ok(Some(hit))
    }

    fn join_road_pieces(
        &self,
        pieces: Vec<(StructuredFound, Vec<[f64; 2]>)>,
    ) -> Vec<StructuredFound> {
        let named = pieces
            .iter()
            .map(|(found, points)| {
                let mut points = points.clone();
                if points.is_empty() {
                    points.push(found.point);
                }
                (road::RoadName::of(&found.hit.record.name), points)
            })
            .collect::<Vec<_>>();
        let groups = road::road_groups(&named);
        let mut pieces = pieces.into_iter().map(Some).collect::<Vec<_>>();
        let mut joined = Vec::with_capacity(groups.len());
        for group in groups {
            let Some((mut found, _)) = pieces[group[0]].take() else {
                continue;
            };
            let points = group
                .iter()
                .flat_map(|&at| named[at].1.iter().copied())
                .collect::<Vec<_>>();
            if group.len() > 1
                && let Some([lon, lat]) = road::road_middle(&points)
            {
                found.point = [lon, lat];
                found.hit.record.point = Some(Point {
                    lon,
                    lat,
                    precision: PointPrecision::Estimated,
                });
            }
            joined.push(found);
        }
        joined
    }

    fn structured_side(
        &self,
        plan: &QueryPlan,
        ranking: &Ranking,
        docs: &[DocId],
    ) -> Result<Option<TextSearchHit>> {
        let Some(number) = plan.house_number.as_ref().map(|number| number.base) else {
            return Ok(None);
        };
        let searcher = self.reader.searcher();
        let docs = docs
            .iter()
            .filter(|&&doc| {
                matches!(
                    self.doc_layer(doc),
                    Some(structured::LAYER_ADDRESS | structured::LAYER_POI)
                )
            })
            .filter_map(|&doc| self.doc_address(doc))
            .take(NEIGHBOUR_DOCUMENTS)
            .collect::<Vec<_>>();
        let (mut lower, mut higher) = self.neighbours_of(&searcher, docs, ranking, number)?;
        held_to_places(plan, &mut lower, &mut higher);
        let side = |neighbour: &Neighbour| neighbour.number % 2 == number % 2;
        let lower = lower.into_iter().filter(side).collect::<Vec<_>>();
        let higher = higher
            .into_iter()
            .filter(|neighbour| side(neighbour) && neighbour.number > number)
            .collect::<Vec<_>>();
        let mut hit = self.place_between(number, &lower, &higher, ROAD_SETBACK_M)?;
        if let Some(hit) = &mut hit {
            hit.record.layer = Layer::Interpolation.as_str().to_string();
        }
        Ok(hit)
    }

    fn street_matches(
        &self,
        structured: &StructuredIndex,
        plan: &QueryPlan,
        places: &PlaceWords,
        countries: &[String],
    ) -> Vec<structured::StreetList> {
        let words = plan
            .subject
            .iter()
            .map(|word| word.text.clone())
            .collect::<Vec<_>>();
        let Some(key) = self.street_keys(&words).into_iter().next() else {
            return Vec::new();
        };
        let query = key.split_whitespace().collect::<Vec<_>>();
        let mut per_word = Vec::new();
        for places in &places.words {
            let cells = match places.iter().all(|place| !place.capped()) {
                true => {
                    let mut cells = places
                        .iter()
                        .flat_map(|place| structured.place_cells(place))
                        .flat_map(structured::ring)
                        .collect::<Vec<_>>();
                    cells.sort_unstable();
                    cells.dedup();
                    Some(cells)
                }
                false => places
                    .iter()
                    .map(|place| structured::box_cells(place.extent, MATCH_CELLS))
                    .collect::<Option<Vec<_>>>()
                    .map(|cells| cells.concat()),
            };
            if let Some(cells) = cells.filter(|cells| !cells.is_empty()) {
                per_word.push(cells);
            }
        }
        let mut cells = per_word.concat();
        cells.sort_unstable();
        cells.dedup();
        if cells.len() > MATCH_CELLS {
            cells = per_word
                .into_iter()
                .min_by_key(Vec::len)
                .filter(|cells| cells.len() <= MATCH_CELLS)
                .unwrap_or_default();
        }
        if cells.is_empty() {
            return Vec::new();
        }
        let countries = match places.countries.is_empty() {
            true => countries,
            false => &places.countries,
        };
        let place_words = context_words(plan);
        let mut matched = structured
            .streets_in(&cells, countries)
            .into_iter()
            .filter_map(|name| {
                let describes = |word: &str| self.describes_street(word);
                let letters = street_words_match(&query, &name, describes)
                    .or_else(|| street_named_in(&query, &name, describes, &place_words))?;
                Some((letters, name))
            })
            .collect::<Vec<_>>();
        matched.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then(left.1.len().cmp(&right.1.len()))
                .then(left.1.cmp(&right.1))
        });
        matched.truncate(MATCHED_STREETS);
        matched
            .into_iter()
            .flat_map(|(_, name)| structured.street_lists(&name, countries))
            .collect()
    }

    fn number_forms(
        &self,
        searcher: &Searcher,
        docs: &[DocId],
        plan: &QueryPlan,
        base_plan: Option<&QueryPlan>,
    ) -> Result<Vec<(DocId, u8, bool)>> {
        if docs.is_empty() {
            return Ok(Vec::new());
        }
        let number = plan.house_number.as_ref().expect("a numbered reading");
        let unit = plan.unit.as_deref();
        let mut forms = HashMap::<DocId, (u8, bool)>::new();
        for (doc, stated) in self.numbered_docs(searcher, docs, number, unit)? {
            let form = match stated {
                true => FORM_EXACT,
                false => FORM_RANGE,
            };
            forms.insert(doc, (form, false));
        }
        if let Some(base_plan) = base_plan {
            let base = base_plan.house_number.as_ref().expect("a base number");
            for (doc, stated) in self.numbered_docs(searcher, docs, base, unit)? {
                let form = match stated {
                    true => FORM_RANGE_START,
                    false => FORM_RANGE,
                };
                match forms.entry(doc) {
                    Entry::Occupied(mut held) if held.get().0 < form => {
                        held.insert((form, true));
                    }
                    Entry::Occupied(_) => {}
                    Entry::Vacant(free) => {
                        free.insert((form, true));
                    }
                }
            }
        }
        let mut forms = forms
            .into_iter()
            .map(|(doc, (form, base))| (doc, form, base))
            .collect::<Vec<_>>();
        forms.sort_unstable_by_key(|(doc, _, _)| *doc);
        Ok(forms)
    }

    fn structured_explained(
        &self,
        cx: &StructuredContext<'_, '_>,
        explanation: &Explanation,
        found: Vec<StructuredFound>,
    ) -> Result<Option<StructuredExplained>> {
        let mut unique = Vec::<StructuredFound>::new();
        for found in found {
            match unique
                .iter_mut()
                .find(|held| held.hit.record_id == found.hit.record_id)
            {
                Some(held) if (found.support, found.form) > (held.support, held.form) => {
                    *held = found;
                }
                Some(_) => {}
                None => unique.push(found),
            }
        }
        let dictionaries = self.tagger.dictionaries();
        let mut scored = Vec::with_capacity(unique.len());
        for found in unique {
            let parts = self.parts(&found.hit, cx.session)?;
            let joint = self.tagger.explain(explanation, |word| {
                parts.labels(word, |word| {
                    dictionaries
                        .regions(word)
                        .into_iter()
                        .map(RecordId::from)
                        .collect()
                })
            });
            if let Some(joint) = joint {
                let gap = joint - explanation.best;
                scored.push(Scored {
                    gap,
                    score: gap + STRUCTURED_W * found.support as f32 / 1000.0,
                    found,
                });
            }
        }
        if scored.is_empty() {
            return Ok(None);
        }
        scored.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then(right.found.form.cmp(&left.found.form))
                .then(right.found.support.cmp(&left.found.support))
                .then(left.found.hit.record_id.cmp(&right.found.hit.record_id))
        });
        let best = scored[0].found.point;
        let second = scored[1..]
            .iter()
            .filter(|scored| {
                haversine_m(
                    best[0],
                    best[1],
                    scored.found.point[0],
                    scored.found.point[1],
                ) > SPOT_M
            })
            .map(|scored| scored.score)
            .fold(f32::NEG_INFINITY, f32::max);
        let mut heads = Vec::<[f64; 2]>::new();
        for scored in &scored {
            let at = scored.found.point;
            if !heads
                .iter()
                .any(|head| haversine_m(head[0], head[1], at[0], at[1]) <= SPOT_M)
            {
                heads.push(at);
            }
        }
        Ok(Some(StructuredExplained {
            scored,
            second,
            spots: heads.len(),
        }))
    }

    fn query_street_keys(&self, plan: &QueryPlan) -> Vec<(String, &'static str)> {
        let words = plan
            .subject
            .iter()
            .map(|word| word.text.clone())
            .collect::<Vec<_>>();
        let mut keys = self
            .street_keys(&words)
            .into_iter()
            .zip(["exact", "variant"])
            .collect::<Vec<_>>();
        if plan.subject.iter().any(|word| word.corrected.is_some()) {
            let corrected = plan
                .subject
                .iter()
                .map(|word| word.corrected.clone().unwrap_or_else(|| word.text.clone()))
                .collect::<Vec<_>>();
            for key in self.street_keys(&corrected) {
                if !keys.iter().any(|(held, _)| *held == key) {
                    keys.push((key, "spelling"));
                }
            }
        }
        keys
    }

    fn structured_places(
        &self,
        structured: &StructuredIndex,
        reading: &Reading,
        countries: &[String],
        probabilities: &[f32],
    ) -> PlaceWords {
        let probability = |at: usize, piece: &Tagged| {
            probabilities.get(at).copied().unwrap_or(match piece.label {
                Label::Locality | Label::Region | Label::Postcode => 1.0,
                _ => 0.0,
            })
        };
        let free = |at: usize, piece: &Tagged| {
            !matches!(
                piece.label,
                Label::Street | Label::HouseNumber | Label::Unit | Label::Country
            ) && probability(at, piece) >= PLACE_FLOOR
        };
        let mut runs = Vec::<Vec<(String, Label, f32)>>::new();
        let mut open = false;
        for (at, piece) in reading.pieces.iter().enumerate() {
            let word = (piece.token.clone(), piece.label, probability(at, piece));
            match (free(at, piece), open) {
                (true, true) => runs.last_mut().expect("an open run").push(word),
                (true, false) => runs.push(vec![word]),
                (false, _) => {}
            }
            open = free(at, piece);
        }
        let has_digit = |words: &[String]| {
            words
                .iter()
                .any(|word| word.chars().any(|character| character.is_ascii_digit()))
        };
        let exact = |words: &[String]| {
            let places = structured.places(&words.join(" "), countries, false);
            match places.is_empty() && has_digit(words) && words.len() > 1 {
                true => structured.places(&words.concat(), countries, false),
                false => places,
            }
        };
        let fuzzy = |words: &[String]| {
            let key = words.join(" ");
            if has_digit(words) || key.chars().count() < FUZZY_PLACE_CHARS {
                return Vec::new();
            }
            let places = structured.places(&key, countries, true);
            match places.iter().all(|place| place.name == places[0].name) {
                true => places,
                false => Vec::new(),
            }
        };
        let mut found = PlaceWords::default();
        let mut log = Vec::new();
        for labelled in &runs {
            let run = labelled
                .iter()
                .map(|(word, ..)| word.clone())
                .collect::<Vec<_>>();
            let mut taken = vec![false; run.len()];
            for (lookup, is_fuzzy) in [
                (&exact as &dyn Fn(&[String]) -> Vec<Place>, false),
                (&fuzzy, true),
            ] {
                let mut at = 0;
                'start: while at < run.len() {
                    if taken[at] {
                        at += 1;
                        continue;
                    }
                    let mut end = at;
                    while end < run.len() && !taken[end] {
                        end += 1;
                    }
                    for stop in (at + 1..=end).rev() {
                        let places = lookup(&run[at..stop]);
                        if places.is_empty() {
                            continue;
                        }
                        taken[at..stop].fill(true);
                        found.digits.push(has_digit(&run[at..stop]));
                        found.words.push(places);
                        let weight = labelled[at..stop]
                            .iter()
                            .map(|(_, _, probability)| f64::from(*probability))
                            .sum::<f64>()
                            / (stop - at) as f64;
                        found.weights.push((weight * 1000.0).round() as usize);
                        found.fuzzy += usize::from(is_fuzzy);
                        at = stop;
                        continue 'start;
                    }
                    at += 1;
                }
            }
            found.skipped += (0..run.len())
                .filter(|&at| !taken[at] && (at == 0 || taken[at - 1]))
                .count();
            for ((word, label, probability), taken) in labelled.iter().zip(&taken) {
                log.push(format!(
                    "{word}/{label:?}/{}/{probability:.2}",
                    u8::from(*taken)
                ));
            }
        }
        found.log = log.join(",");
        if countries.is_empty() {
            for (places, digits) in found.words.iter().zip(&found.digits) {
                if *digits {
                    for place in places {
                        if !found.countries.contains(&place.country) {
                            found.countries.push(place.country.clone());
                        }
                    }
                }
            }
            if !found.countries.is_empty() {
                let wanted = found.countries.clone();
                for places in &mut found.words {
                    places.retain(|place| wanted.contains(&place.country));
                }
                let kept = found
                    .words
                    .iter()
                    .map(|places| !places.is_empty())
                    .collect::<Vec<_>>();
                found.words = filter_by(std::mem::take(&mut found.words), |at| kept[at]);
                found.weights = filter_by(std::mem::take(&mut found.weights), |at| kept[at]);
            }
        }
        found
    }

    fn list_support(
        &self,
        structured: &StructuredIndex,
        list: &structured::StreetList,
        words: &[Vec<Place>],
        weights: &[usize],
        supported: &mut Vec<(DocId, usize)>,
    ) {
        let (here, weights): (Vec<_>, Vec<_>) = words
            .iter()
            .zip(weights)
            .map(|(places, weight)| {
                (
                    places
                        .iter()
                        .filter(|place| place.country == list.country)
                        .collect::<Vec<_>>(),
                    *weight,
                )
            })
            .filter(|(places, _)| !places.is_empty())
            .unzip();
        if here.is_empty() {
            return;
        }
        let mut ranges = Vec::<(usize, usize)>::new();
        let mut cells = here
            .iter()
            .filter(|places| places.iter().all(|place| !place.capped()))
            .flat_map(|places| {
                places
                    .iter()
                    .flat_map(|place| structured.place_cells(place))
            })
            .flat_map(structured::ring)
            .collect::<Vec<_>>();
        if !cells.is_empty() {
            cells.sort_unstable();
            cells.dedup();
            if cells.len() >= list.len() {
                ranges.push((0, list.len()));
            } else {
                for cell in cells {
                    ranges.push(structured.list_range(list, cell, cell, &|doc| self.doc_cell(doc)));
                }
            }
        } else {
            let extent = |places: &Vec<&Place>| {
                places
                    .iter()
                    .flat_map(|place| {
                        structured::box_rows(place.extent).map(|(row, west, east)| {
                            structured.list_range(
                                list,
                                row << 16 | west,
                                row << 16 | east,
                                &|doc| self.doc_cell(doc),
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            };
            let smallest = here
                .iter()
                .map(extent)
                .min_by_key(|ranges| ranges.iter().map(|(from, to)| to - from).sum::<usize>());
            if let Some(smallest) = smallest
                && smallest.iter().map(|(from, to)| to - from).sum::<usize>() <= EXTENT_READ
            {
                ranges = smallest;
            }
        }
        ranges.retain(|(from, to)| from < to);
        ranges.sort_unstable();
        let mut merged = Vec::<(usize, usize)>::with_capacity(ranges.len());
        for (from, to) in ranges {
            match merged.last_mut() {
                Some(last) if from <= last.1 => last.1 = last.1.max(to),
                _ => merged.push((from, to)),
            }
        }
        for (from, to) in merged {
            for at in from..to {
                let doc = structured.list_doc(list, at);
                let cell = self.doc_cell(doc);
                let Some([lon, lat]) = self.doc_point(doc) else {
                    continue;
                };
                let support = here
                    .iter()
                    .zip(&weights)
                    .filter(|(places, _)| {
                        places
                            .iter()
                            .any(|place| structured.place_holds(place, cell, lon, lat))
                    })
                    .map(|(_, weight)| *weight)
                    .sum::<usize>();
                if support > 0 {
                    supported.push((doc, support));
                }
            }
        }
    }

    fn verified(
        &self,
        hit: &TextSearchHit,
        towns_known: bool,
        cx: &StructuredContext<'_, '_>,
    ) -> bool {
        let features = &hit.features;
        let descriptors = self.descriptors(cx.search.query_words.search.iter().map(String::as_str));
        hit.record.layer != Layer::Poi.as_str()
            && features.postcode != Agreement::Contradicts
            && Self::in_stated_towns(features, towns_known)
            && features.unit != Some(false)
            && !self.street_contradicts(&descriptors, hit)
    }

    fn in_stated_towns(features: &Features, towns_known: bool) -> bool {
        match features.area {
            Agreement::Agrees | Agreement::Unstated => true,
            Agreement::Unknown => !towns_known,
            Agreement::Contradicts => features.postcode == Agreement::Agrees,
        }
    }

    fn structured_countries(&self, plan: &QueryPlan, filters: &Filters<'_>) -> Vec<String> {
        match filters.countries.is_empty() {
            true => self
                .stated_places(&plan.context)
                .countries
                .iter()
                .map(|country| country.as_str().to_string())
                .collect(),
            false => filters
                .countries
                .iter()
                .map(|country| country.trim().to_ascii_uppercase())
                .collect(),
        }
    }

    fn structured_neighbour(
        &self,
        plan: &QueryPlan,
        ranking: &Ranking,
        docs: &[DocId],
    ) -> Result<Option<TextSearchHit>> {
        let Some(number) = plan.house_number.as_ref().map(|number| number.base) else {
            return Ok(None);
        };
        let searcher = self.reader.searcher();
        let docs = docs
            .iter()
            .filter(|&&doc| {
                matches!(
                    self.doc_layer(doc),
                    Some(structured::LAYER_ADDRESS | structured::LAYER_POI)
                )
            })
            .filter_map(|&doc| self.doc_address(doc))
            .take(STRUCTURED_CANDIDATES)
            .collect::<Vec<_>>();
        let (mut lower, mut higher) = self.neighbours_of(&searcher, docs, ranking, number)?;
        held_to_places(plan, &mut lower, &mut higher);
        self.place_between(number, &lower, &higher, ROAD_SETBACK_M)
    }

    fn best_tied_document(
        &self,
        searcher: &Searcher,
        plan: &QueryPlan,
        tied: &[DocId],
    ) -> Result<DocId> {
        let Some(&first) = tied.first() else {
            return Err(Error::Inconsistent("a spot with no document".into()));
        };
        if tied.len() < 2 {
            return Ok(first);
        }
        let records = self.pack.records();
        let mut best = (0, first);
        for &doc in tied {
            let Some(address) = self.doc_address(doc) else {
                continue;
            };
            let (record_id, _) = self.document_records(searcher, address)?;
            let street = match records.record(record_id)? {
                Record::Address(record) => record.address.street.or(record.address.place),
                Record::Poi(PoiRecord {
                    address: Some(address),
                    ..
                }) => address.street.or(address.place),
                Record::Street(record) => Some(record.name),
                _ => None,
            };
            let mut words = HashSet::new();
            for word in street
                .as_deref()
                .and_then(search_key)
                .iter()
                .flat_map(|key| key.split_whitespace())
            {
                words.extend(lattice::forms_both_ways(word));
                words.insert(word.to_string());
            }
            let held = plan
                .subject
                .iter()
                .filter(|word| {
                    words.contains(&word.text)
                        || lattice::other_forms(&word.text)
                            .iter()
                            .any(|form| words.contains(form))
                })
                .count();
            if held > best.0 {
                best = (held, doc);
            }
        }
        Ok(best.1)
    }

    fn doc_address(&self, doc: DocId) -> Option<DocAddress> {
        let ord = self.segments.iter().position(|(_, _, end)| doc < *end)?;
        Some(DocAddress::new(ord as u32, doc - self.segments[ord].1))
    }

    fn numbered_docs(
        &self,
        searcher: &Searcher,
        docs: &[DocId],
        number: &HouseNumber,
        unit: Option<&str>,
    ) -> Result<Vec<(DocId, bool)>> {
        let mut docs = docs.to_vec();
        docs.sort_unstable();
        docs.dedup();
        let mut terms = number.forms.terms().cloned().collect::<Vec<_>>();
        terms.extend(unit.map(|unit| number.flat_text(unit)));
        let plain = number.plain().map(u64::from);
        let mut numbered = Vec::new();
        for (ord, &(_, base, end)) in self.segments.iter().enumerate() {
            let local = segment_docs(&docs, base, end);
            if local.is_empty() {
                continue;
            }
            let ord = ord as u32;
            let segment = searcher.segment_reader(ord);
            let columns = self.segment_columns(searcher, ord)?;
            let index = segment.inverted_index(self.fields.address_number)?;
            let mut postings = Vec::new();
            for text in &terms {
                let term = Term::from_field_text(self.fields.address_number, text);
                if let Some(found) = index.read_postings(&term, IndexRecordOption::Basic)? {
                    postings.push(found);
                }
            }
            for doc in local {
                let mut stated = false;
                for found in &mut postings {
                    if found.doc() < doc {
                        found.seek(doc);
                    }
                    stated |= found.doc() == doc;
                }
                let ranged = plain.is_some_and(|number| {
                    columns
                        .interpolation_start
                        .first(doc)
                        .is_some_and(|start| start <= number)
                        && columns
                            .interpolation_end
                            .first(doc)
                            .is_some_and(|end| number <= end)
                });
                if stated || ranged {
                    numbered.push((base + doc, stated));
                }
            }
        }
        Ok(numbered)
    }

    fn doc_columns(&self, doc: DocId) -> Option<(&SegmentColumns, DocId)> {
        let &(segment, base, _) = self.segments.iter().find(|(_, _, end)| doc < *end)?;
        Some((self.columns.get(&segment)?, doc - base))
    }

    pub(crate) fn doc_layer(&self, doc: DocId) -> Option<u8> {
        let (columns, doc) = self.doc_columns(doc)?;
        let record_id = columns.record_id.first(doc)?;
        let layer = self.pack.records().header(record_id).ok()?.layer;
        Some(structured_layer(layer))
    }

    pub(crate) fn doc_point(&self, doc: DocId) -> Option<[i32; 2]> {
        let (columns, doc) = self.doc_columns(doc)?;
        let bound = |column: &Column<i64>| column.first(doc).map(|value| value as i32);
        let extent = [
            bound(&columns.min_lon)?,
            bound(&columns.min_lat)?,
            bound(&columns.max_lon)?,
            bound(&columns.max_lat)?,
        ];
        Some(extent_middle(extent)).filter(|point| *point != [0, 0])
    }

    pub(crate) fn doc_cell(&self, doc: DocId) -> u32 {
        let [lon, lat] = self.doc_point(doc).unwrap_or([0, 0]);
        structured::cell(lon, lat)
    }

    pub(crate) fn doc_postcodes(&self, doc: DocId) -> (String, String) {
        let Some((columns, doc)) = self.doc_columns(doc) else {
            return (String::new(), String::new());
        };
        let first = |column: &Option<StrColumn>| {
            let mut text = String::new();
            if let Some(column) = column
                && let Some(ord) = column.term_ords(doc).next()
                && column.ord_to_str(ord, &mut text).is_err()
            {
                text.clear();
            }
            text
        };
        (first(&columns.rank_postcode), first(&columns.area_postcode))
    }

    pub(crate) fn structured_segments(&self) -> Vec<(String, u32)> {
        self.segments
            .iter()
            .map(|(segment, base, end)| (segment.uuid_string(), end - base))
            .collect()
    }

    pub(crate) fn street_keys(&self, words: &[String]) -> Vec<String> {
        street_keys_of(words, &|word| self.describes_street(word))
    }

    pub(crate) fn structured_keys(&self, doc: DocId) -> Result<structured::Keyed> {
        let mut keyed = structured::Keyed::default();
        let Some(&(segment, base, _)) = self.segments.iter().find(|(_, _, end)| doc < *end) else {
            return Ok(keyed);
        };
        let Some(columns) = self.columns.get(&segment) else {
            return Ok(keyed);
        };
        let doc = doc - base;
        let Some(record_id) = columns.record_id.first(doc) else {
            return Ok(keyed);
        };
        let records = self.pack.records();
        let header_layer = records.header(record_id)?.layer;
        if header_layer == Layer::Country {
            return Ok(keyed);
        }
        let layer = structured_layer(header_layer);
        let first = |column: &Option<StrColumn>| -> Result<String> {
            let mut text = String::new();
            if let Some(column) = column
                && let Some(ord) = column.term_ords(doc).next()
            {
                column.ord_to_str(ord, &mut text)?;
            }
            Ok(text)
        };
        keyed.country = first(&columns.country_code)?;
        let bound = |column: &Column<i64>| column.first(doc).map(|value| value as i32);
        let extent = match (
            bound(&columns.min_lon),
            bound(&columns.min_lat),
            bound(&columns.max_lon),
            bound(&columns.max_lat),
        ) {
            (Some(west), Some(south), Some(east), Some(north)) => Some([west, south, east, north]),
            _ => None,
        };
        let record = records.record(record_id)?;
        let postcode_keys = |postcode: &str, names: &mut Vec<String>| {
            let Some(key) = search_key(postcode) else {
                return;
            };
            names.push(key.split_whitespace().collect());
            if key.contains(' ') {
                names.extend(key.split_whitespace().next().map(str::to_string));
            }
        };
        if layer == 0 {
            let Some([west, south, east, north]) = extent else {
                return Ok(keyed);
            };
            keyed.cells = structured::box_cells([west, south, east, north], structured::AREA_CELLS)
                .unwrap_or_else(|| {
                    let mut corners = vec![
                        structured::cell(west, south),
                        structured::cell(east, south),
                        structured::cell(west, north),
                        structured::cell(east, north),
                    ];
                    corners.sort_unstable();
                    corners.dedup();
                    corners
                });
            match &record {
                Record::Postcode(postcode) => postcode_keys(&postcode.postcode, &mut keyed.names),
                _ => keyed
                    .names
                    .extend(record.names().into_iter().filter_map(search_key)),
            }
            keyed.areas.extend(u32::try_from(record_id).ok());
            keyed.names.sort_unstable();
            keyed.names.dedup();
            return Ok(keyed);
        }
        let (names, postcode): (Vec<&str>, Option<&str>) = match &record {
            Record::Address(address) => (
                address.address.subject().into_iter().collect(),
                address.address.postcode.as_deref(),
            ),
            Record::Poi(poi) => (
                poi.address
                    .as_ref()
                    .and_then(AddressComponents::subject)
                    .into_iter()
                    .collect(),
                poi.address
                    .as_ref()
                    .and_then(|address| address.postcode.as_deref()),
            ),
            Record::Interpolation(interpolation) => (
                interpolation
                    .address
                    .street
                    .as_deref()
                    .or(interpolation.address.place.as_deref())
                    .into_iter()
                    .collect(),
                interpolation.address.postcode.as_deref(),
            ),
            Record::Street(street) => (
                std::iter::once(&street.name)
                    .chain(&street.alt_names)
                    .map(String::as_str)
                    .collect(),
                None,
            ),
            _ => (Vec::new(), None),
        };
        for name in names {
            let mut words = expand_street_words(&Tokens::new(name).words);
            expand_final_direction(&mut words);
            let keys = self.street_keys(&words);
            keyed.second_keys += u64::from(keys.len() > 1);
            if let Some(canonical) = keys.last()
                && !keyed.canonical.contains(canonical)
            {
                keyed.canonical.push(canonical.clone());
            }
            for key in keys {
                if !keyed.streets.contains(&key) {
                    keyed.streets.push(key);
                }
            }
        }
        if keyed.streets.is_empty() {
            return Ok(keyed);
        }
        let [lon, lat] = extent.map_or([0, 0], extent_middle);
        keyed.cell = structured::cell(lon, lat);
        if (lon, lat) == (0, 0) {
            return Ok(keyed);
        }
        keyed.cells = match (header_layer, extent) {
            (Layer::Street, Some(extent)) => structured::box_cells(extent, STREET_CELLS)
                .unwrap_or_else(|| vec![structured::cell(lon, lat)]),
            _ => vec![structured::cell(lon, lat)],
        };
        let areas = self.record_areas(record_id, &record)?;
        keyed.names.extend(areas.localities.iter().cloned());
        keyed.names.extend(areas.regions.iter().cloned());
        if let Some(postcode) = postcode {
            postcode_keys(postcode, &mut keyed.names);
        }
        keyed.names.sort_unstable();
        keyed.names.dedup();
        keyed.names.retain(|name| !name.is_empty());
        keyed.areas = areas
            .chain
            .iter()
            .filter_map(|id| u32::try_from(*id).ok())
            .collect();
        Ok(keyed)
    }

    pub(crate) fn area_names(&self) -> Vec<(String, Vec<u32>)> {
        self.tagger.dictionaries().area_lists()
    }

    fn gather(
        &self,
        readings: &[Reading],
        range: Range<usize>,
        search: &Search<'_>,
        session: &Session,
        accept: &mut impl FnMut(&Session, &QueryPlan, &QueryPostcode, &TextSearchHit) -> Result<bool>,
        pool: &mut Pool,
    ) -> Result<()> {
        let mut best = None::<Evidence>;
        for index in range {
            let reading = &readings[index];
            if best.is_some_and(|held| !held.open()) {
                break;
            }
            for scope in Scope::ALL {
                if !scope.searches(&reading.plan) {
                    continue;
                }
                let variants = std::iter::successors(Some(reading.plan.clone()), |plan| {
                    plan.with_base_house_number()
                });
                for (variant, plan) in variants.enumerate() {
                    if best.is_some_and(|held| !held.beaten_by(scope.best_pass())) {
                        break;
                    }
                    crate::stop::check()?;
                    if !session.allows_search() {
                        return Ok(());
                    }
                    let (mut hits, mut contradicted) = self.search_plan(
                        &plan,
                        search.placed,
                        scope,
                        search.query_words,
                        &search.filters,
                        search.focus,
                        search.limit,
                        session,
                        &mut *accept,
                    )?;
                    if variant > 0 {
                        for hit in hits.iter_mut().chain(&mut contradicted) {
                            let number = &mut hit.matched.housenumber;
                            *number = number.map(|part| part.max(PartMatch::Partial));
                        }
                    }
                    let found = hits.iter().any(|hit| hit.signals.house_level);
                    for hit in hits {
                        let confirmed = !plan.postcode_may_be_house_number()
                            || hit.features.postcode != Agreement::Unknown;
                        let evidence = Evidence {
                            edits: hit.features.edits,
                            pass: hit.features.pass,
                            confirmed,
                            score: reading.score,
                        };
                        if best.is_none_or(|held| evidence.outweighs(held)) {
                            best = Some(evidence);
                        }
                        pool.push(index, hit, confirmed, false);
                    }
                    for hit in contradicted {
                        pool.push(index, hit, false, true);
                    }
                    if found {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn spelt(&self, plan: &QueryPlan, spellings: &Arc<Spellings>) -> Result<QueryPlan> {
        let mut spelt = plan.clone();
        for part in &mut spelt.context {
            let Some(words) = part.corrected.take() else {
                continue;
            };
            let reaches = words.len() == part.words.len()
                && part.words.iter().zip(&words).all(|(written, word)| {
                    written == word || typo_distance(written, word).is_some()
                });
            if reaches && self.known_area_words(&part.words)?.len() < part.words.len() {
                part.words = words;
                part.respelt = true;
            }
        }
        spelt.spellings = Arc::clone(spellings);
        Ok(spelt)
    }

    fn readings(
        &self,
        query_text: &str,
        context: &[ContextPart],
        parses: Parses,
    ) -> Result<Readings> {
        let mut plans = Vec::<QueryPlan>::new();
        let mut scores = Vec::<f32>::new();
        let mut pieces = Vec::<Vec<Tagged>>::new();
        let (parsed, explanation) = self
            .tagger
            .parse_explained_n(query_text, STRUCTURED_READINGS);
        let mut from_text = Vec::<bool>::new();
        let breaks = Tokens::new(query_text).breaks;
        for (index, parsed) in parsed.into_iter().enumerate() {
            let written = self.written_country(&parsed.pieces);
            let read = country_not_town(parsed.pieces, written, parsed.country);
            let plan =
                QueryPlan::from_tagged(&read, &parsed.corrections, &breaks, context, |word| {
                    self.tagger.dictionaries().describes_street(word)
                });
            if let Some(plan) = plan.map(|plan| self.with_number_forms(plan)).transpose()?
                && !plans.contains(&plan)
            {
                plans.push(plan);
                scores.push(parsed.score);
                pieces.push(read);
                from_text.push(index < lattice::N_BEST);
            }
        }
        if parses == Parses::Numbered
            && plans
                .first()
                .is_some_and(|plan| plan.house_number.is_some())
        {
            let kept = plans
                .iter()
                .map(|plan| plan.house_number.is_some())
                .collect::<Vec<_>>();
            let keep = |index: usize| kept[index];
            plans = filter_by(plans, keep);
            scores = filter_by(scores, keep);
            pieces = filter_by(pieces, keep);
            from_text = filter_by(from_text, keep);
        }
        let text_count = from_text.iter().take_while(|from| **from).count();
        Ok((plans, scores, pieces, explanation, text_count))
    }

    fn explain(&self, explanation: &Explanation, session: &Session, pool: &mut Pool) -> Result<()> {
        let mut explained = HashMap::<RecordId, Option<f32>>::new();
        for candidate in &mut pool.candidates {
            crate::stop::check()?;
            let record_id = candidate.hit.record_id;
            let joint = match explained.get(&record_id) {
                Some(joint) => *joint,
                None => {
                    if explained.len() >= MAX_EXPLAINED {
                        continue;
                    }
                    let parts = self.parts(&candidate.hit, session)?;
                    let dictionaries = self.tagger.dictionaries();
                    let joint = self.tagger.explain(explanation, |word| {
                        parts.labels(word, |word| {
                            dictionaries
                                .regions(word)
                                .into_iter()
                                .map(RecordId::from)
                                .collect()
                        })
                    });
                    explained.insert(record_id, joint);
                    joint
                }
            };
            candidate.hit.signals.explained = joint.map(|joint| joint - explanation.best);
        }
        Ok(())
    }

    fn fallback(
        &self,
        query_text: &str,
        context: &[ContextPart],
        filters: &Filters<'_>,
        focus: Option<[f64; 2]>,
        limit: usize,
    ) -> Result<Vec<TextSearchHit>> {
        let session = Session::new(self.budget);
        let (mut plans, _, _, _, text_count) = self.readings(query_text, context, Parses::All)?;
        plans.truncate(text_count);
        let query_words = QueryWords::new(query_text);
        let no_places = HashSet::new();
        for plan in &plans {
            let known = self.known_area_words(
                plan.context
                    .iter()
                    .filter(|part| part.label == Label::Locality)
                    .flat_map(|part| &part.words),
            )?;
            let areas = plan.area_plans(|part| self.stated_names(part, &known));
            let spellings = self.spellings(areas.iter().map(|(area, _)| area))?;
            for (area, layers) in areas {
                let area = self.spelt(&area, &spellings)?;
                for layer in layers {
                    crate::stop::check()?;
                    if !session.allows_search() {
                        return Ok(Vec::new());
                    }
                    let filters = Filters {
                        layer: Some(layer.as_str()),
                        ..*filters
                    };
                    let (mut hits, contradicted) = self.search_plan(
                        &area,
                        &no_places,
                        Scope::Scoped,
                        &query_words,
                        &filters,
                        focus,
                        limit,
                        &session,
                        &mut |_, _, _, _| Ok(true),
                    )?;
                    hits.extend(contradicted);
                    hits.truncate(limit);
                    if !hits.is_empty() {
                        for hit in &mut hits {
                            hit.fallback = true;
                            for part in plan.stated_parts() {
                                if let Some(slot) = part_of(&mut hit.matched, part) {
                                    slot.get_or_insert(PartMatch::Unmatched);
                                }
                            }
                        }
                        return Ok(hits);
                    }
                }
            }
        }
        Ok(Vec::new())
    }

    fn trimmed(&self, plan: &QueryPlan) -> Option<QueryPlan> {
        let words = plan
            .subject
            .iter()
            .map(|word| word.text.clone())
            .collect::<Vec<_>>();
        if words.len() < 2 || self.tagger.names_street(&words) {
            return None;
        }
        let (start, end) = (1..words.len()).rev().find_map(|length| {
            (0..=words.len() - length)
                .map(|start| (start, start + length))
                .find(|&(start, end)| {
                    let run = &words[start..end];
                    run.iter().any(|word| !self.describes_street(word))
                        && self.tagger.names_street(run)
                })
        })?;
        let mut trimmed = plan.clone();
        let subject = std::mem::take(&mut trimmed.subject);
        let named = subject[start..end]
            .iter()
            .any(|word| !self.describes_street(&word.text));
        for (index, mut word) in subject.into_iter().enumerate() {
            if (start..end).contains(&index) {
                word.required = !named || !self.describes_street(&word.text);
                trimmed.subject.push(word);
            } else {
                trimmed.optional.push(word.text);
            }
        }
        Some(trimmed)
    }

    #[allow(clippy::too_many_arguments)]
    fn search_plan(
        &self,
        plan: &QueryPlan,
        placed: &HashSet<String>,
        scope: Scope,
        query_words: &QueryWords,
        filters: &Filters<'_>,
        focus: Option<[f64; 2]>,
        limit: usize,
        session: &Session,
        accept: &mut impl FnMut(&Session, &QueryPlan, &QueryPostcode, &TextSearchHit) -> Result<bool>,
    ) -> Result<(Vec<TextSearchHit>, Vec<TextSearchHit>)> {
        let pass = scope.pass();
        let ranking = self.plan_ranking(plan, query_words, focus, self.query_postcode(plan)?);
        let postcode =
            (!ranking.postcode.forms.is_empty()).then_some(ranking.postcode.exact.as_str());
        let (known, localities_stated) = self.localities_stated(plan, postcode)?;
        let areas = AreaWords {
            known,
            postcodes: self.postcode_weights(&plan.area_terms())?,
            stand_ins: self.stand_in_weights(plan)?,
        };
        let stated = if pass.requires_locality() {
            Vec::new()
        } else {
            localities_stated.iter().flatten().cloned().collect()
        };
        let checks = self.plan_checks(plan, placed, stated)?;
        let filters = checks.filters(filters);
        let checked =
            |hit: &TextSearchHit| self.kept(&checks, &filters, hit, &ranking.postcode, session);
        let query = self.build_query(plan, pass, &filters, &areas)?;
        let mut range_seeds = Vec::new();
        let can_estimate = scope == Scope::Scoped
            && plan.house_number.is_some()
            && plan.unit.is_none()
            && !self.pack.tiger_ranges.is_empty();
        let mut candidates = self.ranked_hits(&query, &ranking, limit, &mut |hit| {
            if !checked(hit)? {
                return Ok(false);
            }
            if accept(session, plan, &ranking.postcode, hit)? {
                return Ok(true);
            }
            if can_estimate
                && hit.record.layer == Layer::Street.as_str()
                && range_seeds.len() < limit
            {
                range_seeds.push(hit.clone());
            }
            Ok(false)
        })?;
        let mut keep = |hit: &TextSearchHit| -> Result<bool> {
            Ok(checked(hit)? && accept(session, plan, &ranking.postcode, hit)?)
        };
        let estimated = scope == Scope::Scoped
            && plan.house_number.is_some()
            && !candidates.iter().any(|hit| hit.signals.house_level)
            && filters
                .layer
                .map(str::trim)
                .filter(|layer| !layer.is_empty())
                .is_none_or(|layer| layer == Layer::Address.as_str());
        if estimated {
            let neighbouring = self
                .place_between_neighbours(plan, pass, &ranking, &areas, &filters)?
                .map_or(Ok(None), |hit| {
                    keep(&hit).map(|allowed| allowed.then_some(hit))
                })?;
            if let Some(hit) = neighbouring {
                candidates.insert(0, hit);
            } else if !self.pack.tiger_ranges.is_empty() && plan.unit.is_none() {
                let mut unique = None;
                for street in candidates.iter().chain(&range_seeds) {
                    if let Some(hit) = self.tiger_estimate(street, &ranking)?
                        && keep(&hit)?
                    {
                        if unique.is_some() {
                            unique = None;
                            break;
                        }
                        unique = Some(hit);
                    }
                }
                if let Some(hit) = unique {
                    candidates.insert(0, hit);
                }
            }
        }
        self.judged(
            plan,
            scope,
            &ranking,
            &localities_stated,
            &checks.places,
            candidates,
            session,
            limit,
        )
    }

    fn plan_ranking(
        &self,
        plan: &QueryPlan,
        query_words: &QueryWords,
        focus: Option<[f64; 2]>,
        postcode: QueryPostcode,
    ) -> Ranking {
        Ranking {
            fame_first: false,
            query_words: query_words.clone(),
            house_number: plan.house_number.as_ref().and_then(HouseNumber::plain),
            number_texts: plan
                .house_number
                .as_ref()
                .map(|number| number.forms.stated.clone())
                .unwrap_or_default(),
            abbreviated_numbers: plan
                .house_number
                .as_ref()
                .map(|number| number.forms.abbreviated.clone())
                .unwrap_or_default(),
            flat_text: plan
                .house_number
                .as_ref()
                .zip(plan.unit.as_deref())
                .map(|(number, unit)| number.flat_text(unit)),
            unit: plan.unit.clone().or_else(|| {
                let number = plan
                    .house_number
                    .as_ref()
                    .filter(|number| number.short.is_none())?;
                Some(number.text.split_once(' ')?.0.to_string())
            }),
            numbered: plan.house_number.is_some(),
            postcode,
            focus,
            near_first: false,
            typed_words: 0,
            whole_last: None,
            sought: plan
                .subject
                .iter()
                .map(|word| Word {
                    text: word.text.clone(),
                    corrected: word.corrected.clone(),
                })
                .chain(plan.unplaced.iter().cloned())
                .collect(),
        }
    }

    fn localities_stated(
        &self,
        plan: &QueryPlan,
        postcode: Option<&str>,
    ) -> Result<(HashSet<String>, Vec<Option<StatedArea>>)> {
        let localities = plan
            .context
            .iter()
            .filter(|part| part.label == Label::Locality)
            .collect::<Vec<_>>();
        let known = self.known_area_words(localities.iter().flat_map(|part| &part.words))?;
        let mut stated = Vec::with_capacity(localities.len());
        for part in &localities {
            stated.push(self.stated_area(part, &known, postcode)?);
        }
        Ok((known, stated))
    }

    fn plan_checks(
        &self,
        plan: &QueryPlan,
        placed: &HashSet<String>,
        stated: Vec<StatedArea>,
    ) -> Result<Checks> {
        let places = self.stated_places(&plan.context);
        let countries = places
            .countries
            .iter()
            .map(|country| country.as_str().to_string())
            .collect::<Vec<_>>();
        let relabelled = plan
            .context
            .iter()
            .filter(|part| !matches!(part.label, Label::Locality | Label::Postcode))
            .flat_map(|part| &part.words)
            .filter(|word| placed.contains(*word) && !places.country_words.contains(word))
            .cloned();
        let unplaced = match plan.subject.is_empty() {
            true => HashSet::new(),
            false => self.known_area_words(plan.unplaced.iter().map(|word| &word.text))?,
        };
        let mut loose = unplaced.into_iter().chain(relabelled).collect::<Vec<_>>();
        loose.sort();
        loose.dedup();
        Ok(Checks {
            places,
            stated,
            loose,
            countries,
        })
    }

    fn kept(
        &self,
        checks: &Checks,
        filters: &Filters<'_>,
        hit: &TextSearchHit,
        postcode: &QueryPostcode,
        session: &Session,
    ) -> Result<bool> {
        Ok(filters.admit(hit)
            && self.region_agreement(hit, &checks.places, session)? != Some(false)
            && self.area_agreement(hit, &checks.stated, postcode, session)? != Some(false)
            && self.in_loose_areas(hit, &checks.loose, session)?)
    }

    #[allow(clippy::too_many_arguments)]
    fn judged(
        &self,
        plan: &QueryPlan,
        scope: Scope,
        ranking: &Ranking,
        localities_stated: &[Option<StatedArea>],
        places: &StatedPlaces,
        candidates: Vec<TextSearchHit>,
        session: &Session,
        limit: usize,
    ) -> Result<(Vec<TextSearchHit>, Vec<TextSearchHit>)> {
        let sought = plan
            .subject
            .iter()
            .map(|word| (&word.text, &word.corrected))
            .chain(
                plan.unplaced
                    .iter()
                    .map(|word| (&word.text, &word.corrected)),
            )
            .collect::<Vec<_>>();
        let descriptors = self.descriptors(plan.subject.iter().map(|word| word.text.as_str()));
        let stated = match self.places {
            true => None,
            false => Some(self.stated_parts(plan)?),
        };
        let (mut hits, mut contradicted) = (Vec::new(), Vec::new());
        for mut hit in candidates {
            let postcode = self.postcode_standing(&hit, &ranking.postcode)?;
            let locality = if localities_stated.is_empty() {
                None
            } else if localities_stated.iter().any(Option::is_none) {
                Some(None)
            } else {
                let known = localities_stated
                    .iter()
                    .flatten()
                    .cloned()
                    .collect::<Vec<_>>();
                Some(self.area_standing(&hit, &known, &ranking.postcode, session)?)
            };
            let area = match locality {
                None => Agreement::Unstated,
                Some(standing) => agreement(standing.map(Standing::agrees)),
            };
            if let Some(stated) = &stated {
                let stands = Stands { locality, postcode };
                hit.matched = self.matched(stated, ranking, places, &hit, stands, session)?;
            }
            let label = label_words(&hit.record);
            let postcode = postcode.map(Standing::agrees);
            let edits = sought
                .iter()
                .filter_map(|(text, corrected)| {
                    let as_written = label
                        .iter()
                        .any(|label| label == *text || corrected.as_ref() == Some(label));
                    match as_written {
                        true => Some(0),
                        false => label
                            .iter()
                            .filter_map(|label| typo_distance(text, label))
                            .min(),
                    }
                })
                .max()
                .unwrap_or(0);
            let features = &mut hit.features;
            features.pass = match scope {
                Scope::Open => Pass::Lenient,
                Scope::Scoped if plan.house_number.is_some() && !hit.signals.house_level => {
                    Pass::Street
                }
                Scope::Scoped => Pass::Strict,
            };
            features.edits = edits;
            features.postcode = match ranking.postcode.forms.is_empty() {
                true => Agreement::Unstated,
                false => agreement(postcode),
            };
            features.area = area;
            features.number = plan
                .house_number
                .as_ref()
                .map(|_| states_number(&hit.record, ranking));
            features.unit = plan
                .unit
                .as_ref()
                .map(|_| unit_rank(&hit.record, ranking) == 0);
            features.dropped_words = count(sought.iter().filter(|(text, corrected)| {
                !label.iter().any(|label| {
                    label == *text
                        || corrected.as_ref() == Some(label)
                        || typo_distance(text, label).is_some()
                })
            }));
            if postcode == Some(false) || self.street_contradicts(&descriptors, &hit) {
                contradicted.push(hit);
            } else {
                hits.push(hit);
            }
        }
        hits.truncate(limit);
        contradicted.truncate(limit);
        Ok((hits, contradicted))
    }

    fn stated_parts<'p>(&self, plan: &'p QueryPlan) -> Result<StatedParts<'p>> {
        let parts = plan
            .context
            .iter()
            .filter(|part| part.label == Label::Region)
            .collect::<Vec<_>>();
        let known = self.known_area_words(parts.iter().flat_map(|part| &part.words))?;
        let mut regions = Vec::new();
        for part in parts {
            regions.extend(self.stated_area(part, &known, None)?);
        }
        Ok(StatedParts { plan, regions })
    }

    fn matched(
        &self,
        stated: &StatedParts<'_>,
        ranking: &Ranking,
        places: &StatedPlaces,
        hit: &TextSearchHit,
        stands: Stands,
        session: &Session,
    ) -> Result<QueryMatch> {
        let Stands { locality, postcode } = stands;
        let plan = stated.plan;
        let states = |label: Label| plan.context.iter().any(|part| part.label == label);
        let (states_region, states_country) = (states(Label::Region), states(Label::Country));
        let part = |label: Label, standing: Option<Standing>| {
            let respelt = plan
                .context
                .iter()
                .any(|part| part.label == label && (part.respelt || part.split));
            match Standing::part(standing) {
                PartMatch::Matched if respelt => PartMatch::Partial,
                part => part,
            }
        };
        let (in_country, in_region) = match states_region || states_country {
            true => self.place_agreement(hit, places, session)?,
            false => (None, None),
        };
        let own = |agrees: bool| match agrees {
            true => Standing::Own,
            false => Standing::Contradicts,
        };
        let in_region = match in_region {
            Some(agrees) => Some(own(agrees)),
            None => self.area_standing(hit, &stated.regions, &ranking.postcode, session)?,
        };
        let mut matched = QueryMatch {
            housenumber: plan
                .house_number
                .as_ref()
                .map(|number| number_match(&hit.record, number, ranking)),
            street: None,
            unit: plan
                .unit
                .as_ref()
                .and_then(|_| unit_match(&hit.record, ranking)),
            locality: locality.map(|standing| part(Label::Locality, standing)),
            region: states_region.then(|| part(Label::Region, in_region)),
            postcode: (!ranking.postcode.forms.is_empty()).then(|| Standing::part(postcode)),
            country: states_country.then(|| part(Label::Country, in_country.map(own))),
        };
        if let Some(subject) = plan.subject_part.0
            && let Some(slot) = part_of(&mut matched, subject)
            && let Some(how) = subject_match(subject, plan, &hit.record, self.tagger.dictionaries())
        {
            *slot = Some(slot.map_or(how, |held| held.max(how)));
        }
        Ok(matched)
    }

    fn descriptors<'w>(&self, words: impl Iterator<Item = &'w str>) -> Descriptors {
        let dictionaries = self.tagger.dictionaries();
        let mut descriptors = Descriptors::default();
        for word in words {
            let kind = if dictionaries.is_direction(word) {
                &mut descriptors.directions
            } else if dictionaries.is_street_type(word) {
                &mut descriptors.types
            } else if let Some((stem, glued)) = lattice::glued_parts(word)
                .filter(|(stem, _)| stem.chars().filter(|c| c.is_alphabetic()).count() >= 3)
            {
                descriptors.names.push(stem.to_string());
                descriptors.types.insert(glued.to_string());
                descriptors.types.extend(lattice::other_forms(glued));
                continue;
            } else {
                descriptors.names.push(word.to_string());
                continue;
            };
            kind.insert(word.to_string());
            kind.extend(lattice::other_forms(word));
        }
        descriptors
    }

    fn street_contradicts(&self, stated: &Descriptors, hit: &TextSearchHit) -> bool {
        if stated.names.is_empty() || (stated.directions.is_empty() && stated.types.is_empty()) {
            return false;
        }
        let street = if hit.record.layer == Layer::Street.as_str() {
            Some(hit.record.name.as_str())
        } else {
            hit.record
                .address
                .street
                .as_deref()
                .or(hit.record.address.place.as_deref())
        };
        let Some(street) = street.and_then(street_search_text) else {
            return false;
        };
        let own = self.descriptors(street.split_whitespace());
        let named = stated.names.iter().all(|word| {
            own.names.contains(word)
                || lattice::other_forms(word)
                    .iter()
                    .any(|form| own.names.contains(form))
        });
        if !named {
            return false;
        }
        let differs = |stated: &HashSet<String>, own: &HashSet<String>| {
            !stated.is_empty() && !own.is_empty() && stated.is_disjoint(own)
        };
        differs(&stated.directions, &own.directions) || differs(&stated.types, &own.types)
    }

    fn area_agreement(
        &self,
        hit: &TextSearchHit,
        stated: &[StatedArea],
        postcode: &QueryPostcode,
        session: &Session,
    ) -> Result<Option<bool>> {
        Ok(self
            .area_standing(hit, stated, postcode, session)?
            .map(Standing::agrees))
    }

    fn area_standing(
        &self,
        hit: &TextSearchHit,
        stated: &[StatedArea],
        postcode: &QueryPostcode,
        session: &Session,
    ) -> Result<Option<Standing>> {
        if stated.is_empty() {
            return Ok(None);
        }
        let areas = self.areas_of(hit.record_id, session)?;
        let mut settlements = None;
        let mut standing = None;
        for area in stated {
            let mut names = if area.label == Label::Region {
                areas.regions.clone()
            } else {
                areas.localities.clone()
            };
            if area.label == Label::Locality && !area.names.whole {
                names.extend(areas.regions.iter().cloned());
            }
            if names.is_empty() {
                continue;
            }
            let names_one = |words: &Vec<String>, names: &[String]| {
                names.iter().any(|name| {
                    let name = name.split_whitespace().collect::<HashSet<_>>();
                    words.iter().all(|word| name.contains(word.as_str()))
                })
            };
            let agrees_with = |names: &[String]| match area.names.any {
                true => area.names.names.iter().any(|words| names_one(words, names)),
                false => area.names.names.iter().all(|words| names_one(words, names)),
            };
            let whole = area.names.names.iter().any(|words| {
                let words = words.iter().map(String::as_str).collect::<HashSet<_>>();
                names
                    .iter()
                    .any(|name| name.split_whitespace().collect::<HashSet<_>>() == words)
            });
            let named = agrees_with(&names);
            let mut agrees = named;
            if !agrees && area.label == Label::Locality {
                agrees = area.postal && self.postcode_agreement(hit, postcode)? == Some(true);
                if !agrees {
                    if settlements.is_none() {
                        settlements = Some(self.near_settlements(hit, session)?);
                    }
                    agrees = settlements
                        .as_deref()
                        .is_some_and(|names| agrees_with(names));
                }
            }
            if !agrees {
                return Ok(Some(Standing::Contradicts));
            }
            standing = Some(match (named && whole, standing) {
                (true, None | Some(Standing::Own)) => Standing::Own,
                _ => Standing::Taken,
            });
        }
        Ok(standing)
    }

    fn near_settlements(&self, hit: &TextSearchHit, session: &Session) -> Result<Rc<Vec<String>>> {
        if let Some(names) = session.settlements.borrow().get(&hit.record_id) {
            return Ok(Rc::clone(names));
        }
        let Some(point) = hit.record.point else {
            return Ok(Rc::default());
        };
        let records = self.pack.records();
        let near = self.spatial.nearest_context_points(
            point.lon,
            point.lat,
            NEAR_SETTLEMENT_RADIUS_M,
            NEAR_SETTLEMENTS,
            |layer, record_id| {
                Ok(matches!(layer, Layer::Locality | Layer::Neighbourhood)
                    && records.header(record_id)?.object()?.0 == ObjectType::Node)
            },
        )?;
        let mut names = Vec::new();
        for candidate in near {
            let own = records
                .names(candidate.record_id)?
                .iter()
                .filter_map(|name| normalized_for_match(Some(name)))
                .collect::<Vec<_>>();
            let bounded = self
                .areas_of(candidate.record_id, session)?
                .localities
                .iter()
                .any(|area| own.contains(area));
            if !bounded {
                names.extend(own);
            }
        }
        let names = Rc::new(names);
        session
            .settlements
            .borrow_mut()
            .insert(hit.record_id, Rc::clone(&names));
        Ok(names)
    }

    fn in_loose_areas(
        &self,
        hit: &TextSearchHit,
        words: &[String],
        session: &Session,
    ) -> Result<bool> {
        let own = label_words(&hit.record)
            .into_iter()
            .chain(
                search_key(&hit.record.name)
                    .iter()
                    .flat_map(|name| name.split_whitespace().map(str::to_string)),
            )
            .collect::<HashSet<_>>();
        let elsewhere = words
            .iter()
            .filter(|word| !own.contains(*word))
            .collect::<Vec<_>>();
        if elsewhere.is_empty() {
            return Ok(true);
        }
        let areas = self.areas_of(hit.record_id, session)?;
        let names = areas
            .localities
            .iter()
            .chain(&areas.regions)
            .flat_map(|name| name.split_whitespace())
            .collect::<HashSet<_>>();
        Ok(names.is_empty() || elsewhere.iter().all(|word| names.contains(word.as_str())))
    }

    fn areas_of(&self, record_id: RecordId, session: &Session) -> Result<Rc<RecordAreas>> {
        if let Some(areas) = session.areas.borrow().get(&record_id) {
            return Ok(Rc::clone(areas));
        }
        let areas = Rc::new(self.read_areas(record_id)?);
        session
            .areas
            .borrow_mut()
            .insert(record_id, Rc::clone(&areas));
        Ok(areas)
    }

    fn parts(&self, hit: &TextSearchHit, session: &Session) -> Result<Parts> {
        let words = |text: Option<&str>| -> HashSet<String> {
            text.and_then(search_key)
                .iter()
                .flat_map(|key| key.split_whitespace())
                .map(str::to_string)
                .collect()
        };
        let record = &hit.record;
        let address = &record.address;
        let areas = self.areas_of(hit.record_id, session)?;
        let mut parts = Parts {
            house_number: words(address.housenumber.as_deref()),
            unit: words(address.unit.as_deref()),
            street: words(address.street.as_deref().or(address.place.as_deref())),
            locality: areas
                .localities
                .iter()
                .flat_map(|name| name.split_whitespace().map(str::to_string))
                .collect(),
            region: areas
                .regions
                .iter()
                .flat_map(|name| name.split_whitespace().map(str::to_string))
                .collect(),
            regions: self.regions_of(hit.record_id, session)?,
            postcode: words(address.postcode.as_deref()),
        };
        let own = words(Some(&record.name));
        match Layer::parse(&record.layer) {
            Some(Layer::Street | Layer::Poi) => parts.street.extend(own),
            Some(Layer::Region) => parts.region.extend(own),
            Some(Layer::Postcode) => parts.postcode.extend(own),
            Some(Layer::Locality | Layer::Neighbourhood | Layer::District | Layer::Place) => {
                parts.locality.extend(own)
            }
            _ => {}
        }
        Ok(parts)
    }

    fn regions_of(&self, record_id: RecordId, session: &Session) -> Result<Rc<Vec<RecordId>>> {
        if let Some(regions) = session.regions.borrow().get(&record_id) {
            return Ok(Rc::clone(regions));
        }
        let regions = Rc::new(
            self.pack
                .boundary_context(record_id)?
                .map(|context| {
                    context
                        .admin_context
                        .entries()
                        .iter()
                        .filter(|entry| {
                            entry.tiers.iter().any(|tier| {
                                matches!(tier, PlaceLayer::Region | PlaceLayer::SubRegion)
                            })
                        })
                        .map(|entry| entry.record_id)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        );
        session
            .regions
            .borrow_mut()
            .insert(record_id, Rc::clone(&regions));
        Ok(regions)
    }

    fn read_areas(&self, record_id: RecordId) -> Result<RecordAreas> {
        let record = self.pack.records().record(record_id)?;
        self.record_areas(record_id, &record)
    }

    fn record_areas(&self, record_id: RecordId, record: &Record) -> Result<RecordAreas> {
        let mut areas = RecordAreas::default();
        match record {
            Record::Address(AddressRecord { address, .. })
            | Record::Poi(PoiRecord {
                address: Some(address),
                ..
            }) => {
                areas.add_region(address.region.as_deref());
                for name in [&address.locality, &address.place].into_iter().flatten() {
                    areas.add_locality(name);
                }
                for name in &address.areas {
                    areas.add_locality(name);
                }
            }
            Record::Interpolation(record) => {
                areas.add_region(record.address.region.as_deref());
                for name in [&record.address.locality, &record.address.place]
                    .into_iter()
                    .flatten()
                {
                    areas.add_locality(name);
                }
            }
            _ => {}
        }
        let Some(context) = self.pack.boundary_context(record_id)? else {
            return Ok(areas);
        };
        let mut districts = Vec::new();
        for entry in context.admin_context.entries() {
            let region = entry
                .tiers
                .iter()
                .any(|tier| matches!(tier, PlaceLayer::Region | PlaceLayer::SubRegion));
            let town = entry
                .tiers
                .iter()
                .any(|tier| tier > PlaceLayer::SubDistrict);
            let district = entry
                .tiers
                .iter()
                .any(|tier| matches!(tier, PlaceLayer::District | PlaceLayer::SubDistrict));
            if !(region || town || district) {
                continue;
            }
            areas.chain.push(entry.record_id);
            let Some(record) = self.pack.context_record(entry.record_id)? else {
                continue;
            };
            for name in [&record.label, &record.name] {
                if region {
                    areas.add_region(Some(name));
                }
                if town {
                    areas.add_locality(name);
                } else if district {
                    districts.push(name.clone());
                }
            }
        }
        if areas.localities.is_empty() {
            for name in &districts {
                areas.add_locality(name);
            }
        }
        Ok(areas)
    }

    fn postcode_agreement(
        &self,
        hit: &TextSearchHit,
        postcode: &QueryPostcode,
    ) -> Result<Option<bool>> {
        Ok(self.postcode_standing(hit, postcode)?.map(Standing::agrees))
    }

    fn postcode_standing(
        &self,
        hit: &TextSearchHit,
        postcode: &QueryPostcode,
    ) -> Result<Option<Standing>> {
        if postcode.forms.is_empty() {
            return Ok(None);
        }
        if postcode.explicit
            && hit.record.layer == Layer::Interpolation.as_str()
            && hit.record.source.dataset == crate::tiger::DATASET
            && let Some(own) = hit
                .record
                .address
                .postcode
                .as_deref()
                .and_then(compact_postcode)
        {
            return Ok(Some(if postcode.forms.contains(&own) {
                Standing::Own
            } else if postcode.holds(&own) {
                Standing::Taken
            } else {
                Standing::Contradicts
            }));
        }
        let records = self.pack.records();
        let own = records
            .postcodes(hit.record_id)?
            .iter()
            .filter_map(|own| compact_postcode(own))
            .filter(|own| postcode.holds(own))
            .map(|own| match postcode.forms.contains(&own) {
                true => Standing::Own,
                false => Standing::Taken,
            })
            .min();
        if own.is_some() {
            return Ok(own);
        }
        let placed = |placement: bool| match placement {
            true => Standing::Taken,
            false => Standing::Contradicts,
        };
        if let Some(point) = hit.record.point {
            if !postcode.areas.is_empty() {
                if let Some(placement) = postcode.placement(point.lon, point.lat) {
                    return Ok(Some(placed(placement)));
                }
            } else if self.unlike_postcode_near(point.lon, point.lat, postcode)? {
                return Ok(Some(Standing::Contradicts));
            }
        }
        Ok(records
            .postcode(hit.record_id)?
            .map(|_| Standing::Contradicts))
    }

    fn unlike_postcode_near(&self, lon: f64, lat: f64, postcode: &QueryPostcode) -> Result<bool> {
        for candidate in self.spatial.context_candidates(
            lon,
            lat,
            POSTCODE_AREA_RADIUS_M,
            POSTCODE_NEIGHBOURS,
        )? {
            if candidate.layer != Layer::Postcode {
                continue;
            }
            return Ok(self
                .pack
                .records()
                .postcode(candidate.record_id)?
                .and_then(|near| compact_postcode(&near))
                .is_some_and(|near| postcode.agreement(&near) == 0));
        }
        Ok(false)
    }

    fn query_postcode(&self, plan: &QueryPlan) -> Result<QueryPostcode> {
        let mut postcode = self.query_postcode_unplaced(plan)?;
        if !postcode.forms.is_empty() {
            postcode.areas = self.postcode_areas(&postcode.exact)?;
        }
        Ok(postcode)
    }

    fn query_postcode_unplaced(&self, plan: &QueryPlan) -> Result<QueryPostcode> {
        let mut postcode = plan.postcode();
        if postcode.forms.is_empty() {
            return Ok(postcode);
        }
        if let Some((first, _)) = postcode.exact.split_once(' ')
            && self
                .postcode_records(std::slice::from_ref(&postcode.exact), 1)?
                .is_empty()
            && !self.postcode_records(&[first.to_string()], 1)?.is_empty()
        {
            postcode = QueryPostcode {
                explicit: postcode.explicit,
                ..QueryPostcode::of(&[first.to_string()])
            };
        }
        Ok(postcode)
    }

    fn postcode_areas(&self, exact: &str) -> Result<Vec<PostcodeArea>> {
        let searcher = self.reader.searcher();
        let mut ends = exact
            .char_indices()
            .map(|(at, character)| at + character.len_utf8())
            .collect::<Vec<_>>();
        ends.reverse();
        let mut sparse = None;
        for end in ends {
            let prefix = &exact[..end];
            if prefix.ends_with(char::is_whitespace) {
                continue;
            }
            let terms = self.terms_with_prefix(&searcher, self.fields.postcode_exact, prefix)?;
            if terms.is_empty() {
                continue;
            }
            let mut by_country = HashMap::<Option<CountryCode>, Vec<(RecordId, [f64; 2])>>::new();
            for record_id in self.postcode_records(&terms, POSTCODE_AREA_RECORDS)? {
                if let Record::Postcode(record) = self.pack.records().record(record_id)? {
                    let (_, lon, lat) = self.pack.records().point(record_id)?;
                    by_country
                        .entry(record.country)
                        .or_default()
                        .push((record_id, [lon, lat]));
                }
            }
            if by_country.is_empty() {
                continue;
            }
            let own = end == exact.len();
            if !own
                && by_country
                    .values()
                    .all(|members| members.len() < POSTCODE_AREA_MIN_NEIGHBOURS)
            {
                sparse.get_or_insert(by_country);
                continue;
            }
            by_country.retain(|_, members| own || members.len() >= POSTCODE_AREA_MIN_NEIGHBOURS);
            return self.postcode_areas_of(by_country);
        }
        sparse.map_or(Ok(Vec::new()), |by_country| {
            self.postcode_areas_of(by_country)
        })
    }

    fn postcode_areas_of(
        &self,
        by_country: HashMap<Option<CountryCode>, Vec<(RecordId, [f64; 2])>>,
    ) -> Result<Vec<PostcodeArea>> {
        by_country
            .into_values()
            .map(|members| {
                let neighbour_m = self.nearest_other_postcode_m(&members)?;
                Ok(PostcodeArea::new(
                    members.into_iter().map(|(_, point)| point).collect(),
                    neighbour_m,
                ))
            })
            .collect()
    }

    fn nearest_other_postcode_m(&self, members: &[(RecordId, [f64; 2])]) -> Result<f64> {
        let mut nearest = POSTCODE_AREA_REACH_LIMIT_M;
        for (_, [lon, lat]) in members {
            for candidate in self.spatial.context_candidates(
                *lon,
                *lat,
                POSTCODE_AREA_REACH_LIMIT_M,
                POSTCODE_NEIGHBOURS,
            )? {
                if candidate.layer == Layer::Postcode
                    && !members
                        .iter()
                        .any(|(member, _)| *member == candidate.record_id)
                {
                    nearest = nearest.min(candidate.distance_m);
                    break;
                }
            }
        }
        Ok(nearest)
    }

    fn terms_with_prefix(
        &self,
        searcher: &Searcher,
        field: Field,
        prefix: &str,
    ) -> Result<Vec<String>> {
        let upper = utf8_prefix_end(prefix);
        let mut found = std::collections::BTreeSet::new();
        for segment in searcher.segment_readers() {
            let index = segment.inverted_index(field)?;
            let mut terms = index.terms().range().ge(prefix).lt(&upper).into_stream()?;
            while found.len() < POSTCODE_AREA_TERMS && terms.advance() {
                if let Ok(term) = std::str::from_utf8(terms.key()) {
                    found.insert(term.to_string());
                }
            }
        }
        Ok(found.into_iter().collect())
    }

    fn postcode_weights(&self, terms: &[String]) -> Result<PostcodeWeights> {
        let mut weights = PostcodeWeights::default();
        for term in terms {
            if weights.terms.contains_key(term) {
                continue;
            }
            let mut covered = HashMap::<Option<CountryCode>, u64>::new();
            for record_id in
                self.postcode_records(std::slice::from_ref(term), POSTCODE_RECORD_LIMIT)?
            {
                if let Record::Postcode(record) = self.pack.records().record(record_id)? {
                    *covered.entry(record.country).or_default() += record.source.record_count;
                }
            }
            let addresses = self.pack.postcode_addresses();
            let by_country = covered
                .into_iter()
                .map(|(country, covered)| {
                    (
                        country,
                        PostcodeWeights::agreement(covered, addresses.of(country)),
                    )
                })
                .collect();
            weights.terms.insert(term.clone(), by_country);
        }
        Ok(weights)
    }

    fn postcode_records(&self, terms: &[String], limit: usize) -> Result<Vec<RecordId>> {
        self.postcode_records_in(None, terms, limit)
    }

    fn postcode_records_in(
        &self,
        options: Option<&TextAutocompleteOptions>,
        terms: &[String],
        limit: usize,
    ) -> Result<Vec<RecordId>> {
        let mut clauses = vec![
            (Occur::Must, self.layer_query(Layer::Postcode.as_str())),
            (
                Occur::Must,
                Box::new(BooleanQuery::new(
                    terms
                        .iter()
                        .map(|term| {
                            (
                                Occur::Should,
                                Box::new(TermQuery::new(
                                    Term::from_field_text(self.fields.postcode_exact, term),
                                    IndexRecordOption::Basic,
                                )) as Box<dyn Query>,
                            )
                        })
                        .collect(),
                )),
            ),
        ];
        clauses.extend(
            options
                .into_iter()
                .flat_map(|options| self.scope_clauses(options)),
        );
        let query = BooleanQuery::new(clauses);
        let searcher = self.reader.searcher();
        searcher
            .search(&query, &TopDocs::with_limit(limit))?
            .into_iter()
            .map(|(_, doc)| Ok(self.document_records(&searcher, doc)?.0))
            .collect()
    }

    fn scope_clauses(&self, options: &TextAutocompleteOptions) -> Vec<(Occur, Box<dyn Query>)> {
        let mut clauses = Vec::new();
        if let Some(countries) = self.countries_query(&options.countries) {
            clauses.push((Occur::Must, countries));
        }
        if let Some(bbox) = options.bbox {
            clauses.push((Occur::Must, self.bbox_query(bbox)));
        }
        clauses
    }

    pub(crate) fn in_scope(
        &self,
        options: &TextAutocompleteOptions,
        hit: &TextSearchHit,
    ) -> Result<bool> {
        let layer = options.layer.as_deref().map(str::trim).unwrap_or_default();
        if !layer.is_empty() && layer != hit.record.layer {
            return Ok(false);
        }
        if !Filters::new(None, &[], options.bbox).admit(hit) {
            return Ok(false);
        }
        let wanted = options
            .countries
            .iter()
            .map(|country| country.trim())
            .filter(|country| !country.is_empty())
            .collect::<Vec<_>>();
        if wanted.is_empty() {
            return Ok(true);
        }
        Ok(
            reach::country_of(&self.pack, hit.record_id)?.is_some_and(|country| {
                wanted
                    .iter()
                    .any(|wanted| wanted.eq_ignore_ascii_case(country.as_str()))
            }),
        )
    }

    /// The id of the record the Pack derived for `postcode` in `country`
    /// (alpha-2), `None` when it derived none: a postcode no address of the
    /// Pack states has no record.
    pub fn postcode_record_id(
        &self,
        postcode: &str,
        country: Option<&str>,
    ) -> Result<Option<String>> {
        let Some(term) = search_key(postcode) else {
            return Ok(None);
        };
        let wanted = labels::derived_postcode_id(country.and_then(CountryCode::parse), postcode);
        for id in self.postcode_records(&[term], POSTCODE_RECORDS_READ)? {
            let summary = self.pack.record_summary(id)?;
            if summary.id == wanted {
                return Ok(Some(summary.id));
            }
        }
        Ok(None)
    }

    fn autocomplete_query(
        &self,
        completion: Completion,
        number: Option<&str>,
        words: &[String],
        typing: bool,
        filters: &Filters<'_>,
    ) -> Result<Option<Box<dyn Query>>> {
        let subject = self.fields.autocomplete_subject_text;
        let last = words.len() - 1;
        let lists = match completion {
            Completion::InOrder => None,
            _ if !typing => None,
            _ => self
                .common_completions(subject, &words[last])?
                .zip(self.common_completions(self.fields.context_text, &words[last])?),
        };
        let completes = lists.is_some();
        let (subject_completions, context_completions) = lists.unwrap_or_default();
        let mut near = HashMap::<(Field, usize), Vec<String>>::new();
        if completion == Completion::Typos {
            let searcher = self.reader.searcher();
            for field in [subject, self.fields.context_text] {
                for (at, typed) in words.iter().enumerate() {
                    let Some(most) = allowed_edits(typed) else {
                        continue;
                    };
                    let builder = automaton_builder(most);
                    let first = first_chars(typed);
                    let dfa = builder.build_dfa(typed);
                    let mut terms = self.near_terms(&searcher, field, typed, &dfa, first, None)?;
                    let edits = |term: &String| match dfa.eval(term) {
                        Distance::Exact(edits) => edits,
                        Distance::AtLeast(edits) => edits,
                    };
                    if let Some(fewest) = terms.iter().map(edits).min() {
                        terms.retain(|term| edits(term) == fewest);
                    }
                    if terms.is_empty() && at == last && typing {
                        let dfa = builder.build_prefix_dfa(typed);
                        terms = self.near_terms(&searcher, field, typed, &dfa, first, None)?;
                        let edits = |term: &String| match dfa.eval(term) {
                            Distance::Exact(edits) | Distance::AtLeast(edits) => edits,
                        };
                        if let Some(fewest) = terms.iter().map(edits).min() {
                            terms.retain(|term| edits(term) == fewest);
                        }
                    }
                    if !terms.is_empty() {
                        near.insert((field, at), terms);
                    }
                }
            }
            if near.is_empty() {
                return Ok(None);
            }
        }
        let word = |field, at: usize| -> Box<dyn Query> {
            let completions = if field == subject {
                &subject_completions.0
            } else {
                &context_completions.0
            };
            let typed = match () {
                _ if at != last || !typing => Typed::Whole,
                _ if completes => Typed::Completions(completions),
                _ => Typed::Prefix,
            };
            let query = self.autocomplete_word_query(field, &words[at], typed);
            let Some(terms) = near.get(&(field, at)) else {
                return query;
            };
            let mut either = vec![(Occur::Should, query)];
            for term in terms {
                either.push((
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(field, term),
                        IndexRecordOption::Basic,
                    )),
                ));
            }
            Box::new(BooleanQuery::new(either))
        };
        let mut clauses = Vec::new();
        match completion {
            Completion::InOrder if !typing => {
                let terms = words
                    .iter()
                    .map(|word| Term::from_field_text(subject, word))
                    .collect::<Vec<_>>();
                let whole: Box<dyn Query> = match terms.len() {
                    1 => Box::new(TermQuery::new(
                        terms[0].clone(),
                        IndexRecordOption::WithFreqs,
                    )),
                    _ => Box::new(PhraseQuery::new(terms)),
                };
                clauses.push((Occur::Must, whole));
            }
            Completion::InOrder => {
                let last = self.expansion_end(subject, &words[words.len() - 1])?;
                clauses.push((
                    Occur::Must,
                    autocomplete_subject_query(subject, words, last),
                ));
            }
            Completion::CommonCompletions => {
                let (completions, cut) = &subject_completions;
                if !cut {
                    return Ok(None);
                }
                let head = &words[..last];
                let phrases = completions
                    .iter()
                    .map(|completion| -> (Occur, Box<dyn Query>) {
                        let terms = head
                            .iter()
                            .chain(std::iter::once(completion))
                            .map(|word| Term::from_field_text(subject, word))
                            .collect::<Vec<_>>();
                        let query: Box<dyn Query> = match terms.len() {
                            1 => Box::new(TermQuery::new(
                                terms[0].clone(),
                                IndexRecordOption::WithFreqs,
                            )),
                            _ => Box::new(PhraseQuery::new(terms)),
                        };
                        (Occur::Should, query)
                    })
                    .collect();
                clauses.push((Occur::Must, Box::new(BooleanQuery::new(phrases))));
            }
            Completion::AddressPrefix => {
                for (at, token) in words.iter().enumerate() {
                    let typed = if at == last || (token.len() >= 4 && !self.describes_street(token))
                    {
                        Typed::Prefix
                    } else {
                        Typed::Whole
                    };
                    let term = self.autocomplete_word_query(subject, token, typed);
                    let alternate = words
                        .get(at + 1)
                        .filter(|next| crate::text_index::is_street_type(next))
                        .and_then(|_| street_ordinal_number(token));
                    let term = match alternate {
                        Some(number) => Box::new(BooleanQuery::new(vec![
                            (Occur::Should, term),
                            (
                                Occur::Should,
                                self.autocomplete_word_query(subject, number, Typed::Whole),
                            ),
                        ])),
                        None => term,
                    };
                    clauses.push((Occur::Must, term));
                }
            }
            Completion::AllWords | Completion::Typos => {
                let optional = |at: usize| {
                    unlisted_contraction(&words[at])
                        && words.iter().any(|other| !unlisted_contraction(other))
                };
                let of_subject = (0..words.len())
                    .map(|at| (Occur::Should, word(subject, at)))
                    .collect();
                clauses.push((Occur::Must, Box::new(BooleanQuery::new(of_subject))));
                for at in 0..words.len() {
                    let either: Box<dyn Query> = Box::new(BooleanQuery::new(vec![
                        (Occur::Should, word(subject, at)),
                        (Occur::Should, word(self.fields.context_text, at)),
                    ]));
                    let occur = if optional(at) {
                        Occur::Should
                    } else {
                        Occur::Must
                    };
                    clauses.push((occur, either));
                }
            }
        }
        if let Some(number) = number {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.address_number, number),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        if let Some(layer) = filters
            .layer
            .map(str::trim)
            .filter(|layer| !layer.is_empty())
        {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.layer, layer),
                    IndexRecordOption::Basic,
                )),
            ));
        }
        if let Some(countries) = self.countries_query(filters.countries) {
            clauses.push((Occur::Must, countries));
        }
        if let Some(bbox) = filters.bbox {
            clauses.push((Occur::Must, self.bbox_query(bbox)));
        }
        clauses.push(self.product_clause());
        Ok(Some(Box::new(BooleanQuery::new(clauses))))
    }

    fn common_completions(&self, field: Field, prefix: &str) -> Result<Completions> {
        let key = (field, prefix.to_string());
        if let Some(found) = self
            .completions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return Ok(found.clone());
        }
        let found = self.scan_completions(field, prefix)?;
        let mut kept = self
            .completions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if kept.len() >= COMPLETIONS_KEPT {
            kept.clear();
        }
        kept.insert(key, found.clone());
        Ok(found)
    }

    fn scan_completions(&self, field: Field, prefix: &str) -> Result<Completions> {
        let searcher = self.reader.searcher();
        let mut used = HashMap::<Vec<u8>, u64>::new();
        for segment in searcher.segment_readers() {
            let index = segment.inverted_index(field)?;
            let mut stream = index.terms().range().ge(prefix.as_bytes()).into_stream()?;
            while stream.advance() {
                if !stream.key().starts_with(prefix.as_bytes()) {
                    break;
                }
                *used.entry(stream.key().to_vec()).or_default() +=
                    u64::from(stream.value().doc_freq);
                if used.len() > MAX_COMPLETION_SCAN {
                    return Ok(None);
                }
            }
        }
        let cap = AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS as usize;
        let mut words = used.into_iter().collect::<Vec<_>>();
        let cut = words.len() > cap;
        words.sort_by(|(a, a_used), (b, b_used)| b_used.cmp(a_used).then_with(|| a.cmp(b)));
        words.truncate(cap);
        Ok(Some((
            words
                .into_iter()
                .filter_map(|(word, _)| String::from_utf8(word).ok())
                .collect(),
            cut,
        )))
    }

    fn autocomplete_word_query(
        &self,
        field: Field,
        word: &str,
        typed: Typed<'_>,
    ) -> Box<dyn Query> {
        let mut alternatives = std::iter::once(word.to_string())
            .chain(lattice::other_forms(word))
            .map(|form| -> (Occur, Box<dyn Query>) {
                (
                    Occur::Should,
                    Box::new(TermQuery::new(
                        Term::from_field_text(field, &form),
                        IndexRecordOption::WithFreqs,
                    )),
                )
            })
            .collect::<Vec<_>>();
        match typed {
            Typed::Whole => {}
            Typed::Prefix => alternatives.push((
                Occur::Should,
                autocomplete_subject_query(
                    field,
                    std::slice::from_ref(&word.to_string()),
                    self.expansion_end(field, word).ok().flatten(),
                ),
            )),
            Typed::Completions(completions) => {
                for completion in completions {
                    alternatives.push((
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(field, completion),
                            IndexRecordOption::Basic,
                        )),
                    ));
                }
            }
        }
        Box::new(BooleanQuery::new(alternatives))
    }

    /// The address reading takes a town's or street's words in a name
    /// ("Rijksmuseum Twenthe") for where it is, and passes the place over.
    /// Where the place `words` name lies, as `/places` finds it by name
    /// (exact names first): the nearest within [`BIAS_RADIUS_M`] of `focus`,
    /// and the best known anywhere. A region or a country is too wide to
    /// search around.
    /// Near, a town's name must be the words; with `prefix`, a POI's need
    /// only start with them ("pike place" for Pike Place Market).
    pub fn place_point(
        &self,
        words: &str,
        focus: Option<[f64; 2]>,
        prefix: bool,
    ) -> Result<PlacePoint> {
        let wide = [Layer::Country, Layer::Region, Layer::District].map(Layer::as_str);
        let typed = search_key(words);
        let records = self.pack.records();
        let exact = |hit: &TextSearchHit| -> Result<bool> {
            Ok(records
                .names(hit.record_id)?
                .iter()
                .any(|name| search_key(name) == typed))
        };
        let fame = |hit: &TextSearchHit| -> Result<u8> {
            Ok(records
                .prominence(hit.record_id)?
                .map_or(0, |prominence| prominence.importance))
        };
        let first = |focus| -> Result<Option<(TextSearchHit, bool)>> {
            let mut best: Option<((bool, u8, bool), TextSearchHit)> = None;
            for hit in self.places_by_name_keeping(
                words,
                &Filters::default(),
                focus,
                NAMED_PLACES_WINDOW,
                true,
            )? {
                if wide.contains(&hit.record.layer.as_str()) || hit.record.point.is_none() {
                    continue;
                }
                let own = search_key(&hit.record.name) == typed;
                let key = (exact(&hit)?, fame(&hit)?, own);
                if best.as_ref().is_none_or(|(held, _)| key > *held) {
                    best = Some((key, hit));
                }
            }
            let Some((_, hit)) = best else {
                return Ok(None);
            };
            let exact = exact(&hit)?;
            let abroad = records
                .prominence(hit.record_id)?
                .is_some_and(|prominence| known_abroad(prominence.rank()));
            Ok(Some((hit, exact && abroad)))
        };
        let point = |hit: &TextSearchHit| hit.record.point.map(|point| [point.lon, point.lat]);
        let tokens = autocomplete_query_tokens(typed.as_deref().unwrap_or_default());
        let named = |hit: &TextSearchHit| -> Result<bool> {
            Ok(match prefix {
                true => name_starts_as(&hit.record.name, &tokens, false) || exact(hit)?,
                false => exact(hit)?,
            })
        };
        let mut near = None;
        if let Some([lon, lat]) = focus {
            let hits = self.places_by_name_keeping(
                words,
                &Filters::default(),
                focus,
                NAMED_PLACES_WINDOW,
                true,
            )?;
            for hit in &hits {
                if wide.contains(&hit.record.layer.as_str()) || !named(hit)? {
                    continue;
                }
                if let Some(point) = point(hit)
                    && haversine_m(lon, lat, point[0], point[1]) <= BIAS_RADIUS_M
                {
                    near = Some(point);
                    break;
                }
            }
        }
        let known = match near {
            Some(_) => None,
            None => first(None)?,
        };
        Ok(PlacePoint {
            near,
            known_abroad: known
                .as_ref()
                .filter(|(_, abroad)| *abroad)
                .and_then(|(hit, _)| point(hit)),
            known: known.as_ref().and_then(|(hit, _)| point(hit)),
        })
    }

    fn places_by_name(
        &self,
        query_text: &str,
        filters: &Filters<'_>,
        focus: Option<[f64; 2]>,
        limit: usize,
    ) -> Result<Vec<TextSearchHit>> {
        self.places_by_name_keeping(query_text, filters, focus, limit, false)
    }

    fn places_by_name_keeping(
        &self,
        query_text: &str,
        filters: &Filters<'_>,
        focus: Option<[f64; 2]>,
        limit: usize,
        streets: bool,
    ) -> Result<Vec<TextSearchHit>> {
        let Some(key) = search_key(query_text) else {
            return Ok(Vec::new());
        };
        let found = self.by_name(
            &key,
            query_text,
            filters,
            focus,
            limit.max(NAMED_PLACES_WINDOW),
            &[],
        )?;
        let mut hits = Vec::with_capacity(found.len());
        for (mut hit, by_name) in found {
            if !self.places {
                let street = streets && hit.record.layer == Layer::Street.as_str();
                if !(by_name.exact && (by_name.known || street)) {
                    continue;
                }
                let split = Tokens::new(query_text).breaks.contains(&true);
                if let Some(slot) =
                    named_part(&hit.record.layer).and_then(|part| part_of(&mut hit.matched, part))
                {
                    *slot = Some(match split {
                        true => PartMatch::Partial,
                        false => PartMatch::Matched,
                    });
                }
            }
            hits.push(hit);
        }
        self.one_per_item(&mut hits, limit)?;
        Ok(hits)
    }

    pub(crate) fn qualified_pois(
        &self,
        options: &TextAutocompleteOptions,
    ) -> Result<Vec<TextSearchHit>> {
        let query = options.query.trim();
        let filters = Filters::new(Some("poi"), &options.countries, options.bbox);
        let limit = effective_limit(options.limit);
        let mut splits = Vec::new();
        let lowercase = query.to_ascii_lowercase();
        if let Some((at, connector)) = [" near ", " in "]
            .into_iter()
            .find_map(|connector| lowercase.find(connector).map(|at| (at, connector)))
        {
            splits.push((&query[..at], &query[at + connector.len()..]));
        } else if let Some((name, suffix)) = query.split_once(',') {
            splits.push((name, suffix));
        } else {
            let dictionaries = self.tagger.dictionaries();
            for (at, _) in query.rmatch_indices(char::is_whitespace).take(3) {
                let suffix = query[at..].trim();
                let Some(key) = dictionary_key(suffix) else {
                    continue;
                };
                if dictionaries.contains(NameField::Locality, &key)
                    || dictionaries.contains(NameField::Region, &key)
                    || suffix
                        .split_whitespace()
                        .any(|word| search_key(word).is_some_and(|key| self.describes_street(&key)))
                    || (options.focus.is_some() && area_initials(&key))
                {
                    splits.push((&query[..at], suffix));
                }
            }
            splits.reverse();
        }
        for (name, suffix) in splits {
            let Some(key) = search_key(name) else {
                continue;
            };
            if key.chars().count() < 3 || crate::categories::of_query(&key).is_some() {
                continue;
            }
            let qualifiers = suffix.split(',').filter_map(search_key).collect::<Vec<_>>();
            if qualifiers.is_empty() {
                continue;
            }
            let words = qualifiers
                .iter()
                .flat_map(|part| {
                    expand_address_abbreviations(part)
                        .split_whitespace()
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            let allow_initials = qualifiers.len() == 1 && area_initials(&qualifiers[0]);
            let nearby = options.focus.filter(|_| allow_initials);
            let qualified_filters = Filters {
                bbox: filters.bbox.or_else(|| nearby.map(bias_bounds)),
                ..filters
            };
            let mut candidates =
                self.by_name(&key, name, &filters, options.focus, limit.max(32), &words)?;
            if candidates.is_empty() && nearby.is_some() {
                candidates = self.by_name(
                    &key,
                    name,
                    &qualified_filters,
                    options.focus,
                    limit.max(32),
                    &[],
                )?;
            }
            let mut hits = candidates
                .into_iter()
                .filter_map(|(hit, evidence)| evidence.exact.then_some(hit))
                .collect::<Vec<_>>();
            if hits.is_empty() {
                let local = TextAutocompleteOptions {
                    query: name.to_string(),
                    limit: limit.max(32),
                    layer: Some("poi".to_string()),
                    ..options.clone()
                };
                hits = self.autocomplete_as(&local, Completion::Typos)?;
            }
            let mut matched = Vec::new();
            for mut hit in hits {
                if !self.matches_poi_name(&hit, &key)? {
                    continue;
                }
                let areas = self.read_areas(hit.record_id)?;
                let mut matches_initials = false;
                if let Some(point) = nearby
                    && within_bias(&hit, point)
                {
                    for id in &areas.chain {
                        if let Some(area) = self.pack.context_record(*id)?
                            && search_key(&area.name).is_some_and(|name| {
                                name.split_whitespace()
                                    .filter_map(|word| word.chars().next())
                                    .eq(qualifiers[0].chars())
                            })
                        {
                            matches_initials = true;
                            break;
                        }
                    }
                }
                if matches_initials
                    || qualifiers.iter().all(|part| {
                        areas.localities.contains(part)
                            || areas.regions.contains(part)
                            || hit
                                .record
                                .address
                                .street
                                .as_deref()
                                .is_some_and(|street| road::same_road(street, part))
                    })
                {
                    hit.highlights = typed_highlights(query, &hit.record.label);
                    matched.push(hit);
                }
            }
            self.one_per_item(&mut matched, limit)?;
            if !matched.is_empty() {
                return Ok(matched);
            }
        }
        Ok(Vec::new())
    }

    pub(crate) fn matches_poi_name(&self, hit: &TextSearchHit, query: &str) -> Result<bool> {
        let sought = query.split_whitespace().collect::<Vec<_>>();
        for name in self.pack.records().names(hit.record_id)? {
            let Some(name) = search_key(&name) else {
                continue;
            };
            let words = name.split_whitespace().collect::<Vec<_>>();
            let mut corrections = 0;
            if sought.iter().all(|word| {
                if words.iter().any(|held| types_word(word, held, false)) {
                    return true;
                }
                corrections += 1;
                corrections == 1 && words.iter().any(|held| typo_distance(word, held).is_some())
            }) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn by_name(
        &self,
        key: &str,
        written: &str,
        filters: &Filters<'_>,
        focus: Option<[f64; 2]>,
        window: usize,
        context: &[String],
    ) -> Result<Vec<(TextSearchHit, ByName)>> {
        let words = key.split_whitespace().collect::<Vec<_>>();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let mut clauses = words
            .iter()
            .map(|word| {
                (
                    Occur::Must,
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.name_text, word),
                        IndexRecordOption::WithFreqs,
                    )) as Box<dyn Query>,
                )
            })
            .collect::<Vec<_>>();
        if let Some(layer) = filters.layer {
            clauses.push((Occur::Must, self.layer_query(layer)));
        }
        for word in context {
            let either = [self.fields.context_text, self.fields.content_text].map(|field| {
                Box::new(TermQuery::new(
                    Term::from_field_text(field, word),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>
            });
            clauses.push((Occur::Must, Box::new(BooleanQuery::union(either.into()))));
        }
        if let Some(bbox) = filters.bbox {
            clauses.push((Occur::Must, self.bbox_query(bbox)));
        }
        if let Some(countries) = self.countries_query(filters.countries) {
            clauses.push((Occur::Must, countries));
        }
        clauses.push(self.product_clause());
        let ranking = Ranking {
            query_words: QueryWords::new(written),
            focus,
            ..Ranking::default()
        };
        let searcher = self.reader.searcher();
        let docs = searcher.search(
            &BooleanQuery::new(clauses),
            &TopDocs::with_limit(window).tweak_score(FameKeys {
                columns: &self.columns,
                focus,
            }),
        )?;
        let top_docs = docs
            .into_iter()
            .map(|((_, _, text_score), doc)| (RankKey::text(text_score), doc))
            .collect();
        let hits = self.rank_documents(&searcher, top_docs, &ranking, window, &mut |hit| {
            Ok(filters.admit(hit))
        })?;
        let records = self.pack.records();
        let mut found = Vec::with_capacity(hits.len());
        for hit in hits {
            let names = records.names(hit.record_id)?;
            let named = |name: &String| search_key(name).as_deref() == Some(key);
            let exact = names.iter().any(named);
            let own = names.first().is_some_and(named);
            let band = focus
                .zip(hit.record.point)
                .map_or(0, |([lon, lat], point)| {
                    focus_band(haversine_m(lon, lat, point.lon, point.lat))
                });
            let known = records.prominence(hit.record_id)?;
            let fame = known.unwrap_or_default();
            let order = (
                std::cmp::Reverse(exact),
                band,
                std::cmp::Reverse(fame.importance / FAME_TIER),
                std::cmp::Reverse(own),
                std::cmp::Reverse(fame),
            );
            found.push((
                order,
                hit,
                ByName {
                    exact,
                    known: known.is_some(),
                },
            ));
        }
        found.sort_by_key(|(order, _, _)| *order);
        Ok(found
            .into_iter()
            .map(|(_, hit, by_name)| (hit, by_name))
            .collect())
    }

    fn ranked_hits(
        &self,
        query: &dyn Query,
        ranking: &Ranking,
        limit: usize,
        accept: &mut impl FnMut(&TextSearchHit) -> Result<bool>,
    ) -> Result<Vec<TextSearchHit>> {
        self.ranked_hits_in(&self.corpus(), query, ranking, limit, accept)
    }

    fn ranked_hits_in(
        &self,
        corpus: &Corpus,
        query: &dyn Query,
        ranking: &Ranking,
        limit: usize,
        accept: &mut impl FnMut(&TextSearchHit) -> Result<bool>,
    ) -> Result<Vec<TextSearchHit>> {
        let top_docs = self.top_documents(corpus, query, ranking, rank_window(limit))?;
        self.rank_documents(&corpus.searcher, top_docs, ranking, limit, accept)
    }

    fn top_documents(
        &self,
        corpus: &Corpus,
        query: &dyn Query,
        ranking: &Ranking,
        window: usize,
    ) -> Result<Vec<(RankKey, DocAddress)>> {
        let searcher = &corpus.searcher;
        let mut scored = match ranking.near_first || ranking.fame_first {
            true => Vec::new(),
            false => corpus.search(query, &TopDocs::with_limit(window))?,
        };
        let tie = |scored: &[(Score, DocAddress)], window: usize| {
            scored.len() == window && window > 1 && scored[window - 1].0 == scored[window - 2].0
        };
        if tie(&scored, window) && window < TIE_WINDOW {
            scored = corpus.search(query, &TopDocs::with_limit(TIE_WINDOW))?;
        }
        let top_docs = if ranking.near_first || ranking.fame_first || tie(&scored, TIE_WINDOW) {
            let keys = RankKeys {
                ranking,
                columns: corpus.columns,
                pack: &self.pack,
            };
            corpus.search(query, &TopDocs::with_limit(window).tweak_score(keys))?
        } else {
            let mut top_docs = self.keyed_documents(searcher, scored, ranking)?;
            top_docs.truncate(window);
            top_docs
        };
        Ok(top_docs)
    }

    fn keyed_documents(
        &self,
        searcher: &Searcher,
        scored: Vec<(Score, DocAddress)>,
        ranking: &Ranking,
    ) -> Result<Vec<(RankKey, DocAddress)>> {
        let keys = RankKeys {
            ranking,
            columns: &self.columns,
            pack: &self.pack,
        };
        let mut tweakers = HashMap::<u32, SegmentRankKeys>::new();
        let mut top_docs = Vec::with_capacity(scored.len());
        for (text_score, doc) in scored {
            let tweaker = match tweakers.entry(doc.segment_ord) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    entry.insert(keys.segment_tweaker(searcher.segment_reader(doc.segment_ord))?)
                }
            };
            top_docs.push((tweaker.score(doc.doc_id, text_score), doc));
        }
        top_docs.sort_by(|(left, _), (right, _)| {
            right.partial_cmp(left).unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(top_docs)
    }

    fn rank_documents(
        &self,
        searcher: &Searcher,
        top_docs: Vec<(RankKey, DocAddress)>,
        ranking: &Ranking,
        limit: usize,
        accept: &mut impl FnMut(&TextSearchHit) -> Result<bool>,
    ) -> Result<Vec<TextSearchHit>> {
        let query_words = ranking
            .query_words
            .search
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let mut cutoff = None;
        let mut hits = Vec::new();
        let mut abbreviations = Vec::new();
        let mut placed = 0;
        for (key, doc) in top_docs {
            crate::stop::check()?;
            if cutoff
                .as_ref()
                .is_some_and(|cutoff| key.cmp_evidence(cutoff).is_lt())
            {
                break;
            }
            let (first, count) = self.document_records(searcher, doc)?;
            let country = self.document_country(searcher, doc)?;
            for (record_id, abbreviated) in self.hit_records(first, count, ranking)? {
                let mut record = self.pack.record_summary(record_id)?;
                if let Some(number) = ranking.house_number
                    && record.layer == Layer::Interpolation.as_str()
                    && !self.estimate_house(record_id, number, &mut record)?
                {
                    continue;
                }
                if ranking.numbered && record.layer == Layer::Street.as_str() {
                    self.place_on_street_line(record_id, &mut record)?;
                }
                let records = self.pack.records();
                let disputed = records.header(record_id)?.disputed;
                let hit = TextSearchHit {
                    named_place: false,
                    record_id,
                    score: key.text_score,
                    record,
                    features: Features {
                        country,
                        disputed,
                        ..Features::default()
                    },
                    confidence: None,
                    fallback: false,
                    signals: Signals::default(),
                    matched: QueryMatch::default(),
                    highlights: Vec::new(),
                };
                if !accept(&hit)? {
                    continue;
                }
                let rank = records
                    .place_type(record_id)?
                    .as_deref()
                    .and_then(place_rank);
                let prominence = records.prominence(record_id)?;
                if hit.record.layer != Layer::Poi.as_str() || ranking.sought.is_empty() {
                    placed += 1;
                }
                hits.push((
                    key.clone(),
                    TieBreak::new(
                        &hit.record,
                        Prior {
                            place_rank: rank,
                            prominence,
                            disputed,
                        },
                        &query_words,
                        ranking,
                    ),
                    hit,
                ));
                abbreviations.push(abbreviated);
            }
            if !ranking.numbered && cutoff.is_none() && placed >= limit {
                cutoff = Some(key);
            }
        }
        let sure = sure_abbreviations(hits.iter().map(|(_, _, hit)| hit), &abbreviations, ranking);
        let mut sure = sure.into_iter();
        hits.retain(|_| sure.next().unwrap_or(true));
        let demoted = self.demoted(hits.iter().map(|(_, _, hit)| hit), ranking)?;
        let mut hits = hits.into_iter().zip(demoted).collect::<Vec<_>>();
        hits.sort_by(
            |((left_key, left_tie, _), left_demoted),
             ((right_key, right_tie, _), right_demoted)| {
                left_demoted
                    .cmp(right_demoted)
                    .then_with(|| right_key.cmp_evidence(left_key))
                    .then_with(|| left_tie.cmp(right_tie))
            },
        );
        hits.truncate(limit);
        let small = |value: usize| value.min(usize::from(u8::MAX)) as u8;
        Ok(hits
            .into_iter()
            .enumerate()
            .map(|(rank, ((key, tie, mut hit), demoted))| {
                hit.signals = Signals {
                    house_level: key.house_level,
                    stated: key.stated,
                    postcode_agreement: key.postcode_agreement,
                    name_words: key.name_words,
                    place_rank: key.place_rank,
                    extra_street_words: small(tie.extra_street_words),
                    unit: tie.unit,
                    extra_name_words: small(tie.extra_name_words),
                    missing_query_words: small(tie.missing_query_words),
                    inexact_query_words: small(tie.inexact_query_words),
                    focus_band: tie.focus_band,
                    precision: tie.precision,
                    poi: hit.record.layer == Layer::Poi.as_str(),
                    demoted,
                    window_rank: rank.min(usize::from(u16::MAX)) as u16,
                    ..Signals::default()
                };
                hit.named_place = key.named_place;
                hit
            })
            .collect())
    }

    fn demoted<'a>(
        &self,
        hits: impl Iterator<Item = &'a TextSearchHit> + Clone,
        ranking: &Ranking,
    ) -> Result<Vec<bool>> {
        let anchors = hits
            .clone()
            .filter(|hit| {
                [Layer::Address, Layer::Interpolation, Layer::Street]
                    .iter()
                    .any(|layer| hit.record.layer == layer.as_str())
            })
            .map(|hit| (hit, self.unaccounted(hit, ranking)))
            .filter(|(_, unaccounted)| ranking.numbered || unaccounted.is_empty())
            .collect::<Vec<_>>();
        let mut demoted = Vec::new();
        for hit in hits {
            if anchors.is_empty() || hit.record.layer != Layer::Poi.as_str() {
                demoted.push(false);
                continue;
            }
            let names = self
                .pack
                .records()
                .names(hit.record_id)?
                .iter()
                .filter_map(|name| search_key(name))
                .flat_map(|name| {
                    name.split_whitespace()
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .collect::<HashSet<_>>();
            let mut confirms = false;
            for (anchor, unaccounted) in &anchors {
                if !unaccounted.is_empty()
                    && unaccounted.iter().all(|word| names.contains(*word))
                    && self.stands_at(hit, anchor)?
                {
                    confirms = true;
                    break;
                }
            }
            demoted.push(!confirms);
        }
        Ok(demoted)
    }

    fn unaccounted<'a>(&self, hit: &TextSearchHit, ranking: &'a Ranking) -> Vec<&'a String> {
        let street = if hit.record.layer == Layer::Street.as_str() {
            Some(hit.record.name.as_str())
        } else {
            hit.record
                .address
                .street
                .as_deref()
                .or(hit.record.address.place.as_deref())
        };
        let words = street
            .and_then(search_key)
            .iter()
            .flat_map(|street| street.split_whitespace())
            .map(str::to_string)
            .collect::<Vec<_>>();
        ranking
            .sought
            .iter()
            .filter(|word| {
                !words.iter().any(|street| {
                    *street == word.text
                        || word.corrected.as_ref() == Some(street)
                        || lattice::other_forms(&word.text).contains(street)
                        || typo_distance(&word.text, street).is_some()
                })
            })
            .map(|word| &word.text)
            .collect()
    }

    fn stands_at(&self, poi: &TextSearchHit, anchor: &TextSearchHit) -> Result<bool> {
        let street = |hit: &TextSearchHit| {
            if hit.record.layer == Layer::Street.as_str() {
                street_search_text(&hit.record.name)
            } else {
                hit.record
                    .address
                    .street
                    .as_deref()
                    .or(hit.record.address.place.as_deref())
                    .and_then(street_search_text)
            }
        };
        let number = |hit: &TextSearchHit| {
            hit.record
                .address
                .housenumber
                .as_deref()
                .and_then(search_key)
        };
        let anchor_street = street(anchor);
        if anchor_street.is_some()
            && street(poi) == anchor_street
            && (number(anchor).is_none() || number(poi) == number(anchor))
        {
            return Ok(true);
        }
        let (Some(at), Some(from)) = (poi.record.point, anchor.record.point) else {
            return Ok(false);
        };
        match anchor_street {
            Some(name) if anchor.record.layer == Layer::Street.as_str() => {
                self.on_named_street([at.lon, at.lat], &name)
            }
            _ => Ok(haversine_m(at.lon, at.lat, from.lon, from.lat) <= AT_ADDRESS_RADIUS_M),
        }
    }

    fn document_country(
        &self,
        searcher: &Searcher,
        doc: DocAddress,
    ) -> Result<Option<CountryCode>> {
        let columns = self.segment_columns(searcher, doc.segment_ord)?;
        let Some(column) = &columns.country_code else {
            return Ok(None);
        };
        let mut code = String::new();
        for ord in column.term_ords(doc.doc_id) {
            if column.ord_to_str(ord, &mut code)? {
                return Ok(CountryCode::parse(&code));
            }
        }
        Ok(None)
    }

    fn document_records(&self, searcher: &Searcher, doc: DocAddress) -> Result<(RecordId, u64)> {
        let columns = self.segment_columns(searcher, doc.segment_ord)?;
        let record_id = columns
            .record_id
            .values_for_doc(doc.doc_id)
            .next()
            .ok_or_else(|| {
                Error::Inconsistent("text index hit is missing structured record_id".into())
            })?;
        let count = columns.record_count.first(doc.doc_id).unwrap_or(1);
        Ok((record_id, count))
    }

    fn hit_records(
        &self,
        first: RecordId,
        count: u64,
        ranking: &Ranking,
    ) -> Result<Vec<(RecordId, Option<String>)>> {
        if ranking.number_texts.is_empty() {
            return Ok(vec![(first, None)]);
        }
        if self.pack.records().header(first)?.layer != Layer::Address {
            return Ok(vec![(first, None)]);
        }
        let mut matched = Vec::new();
        self.pack
            .records()
            .address_numbers(first, count, |record_id, stated, unit| {
                let sought = |term: &String| {
                    ranking.number_texts.contains(term)
                        || ranking.flat_text.as_ref().is_some_and(|flat| term == flat)
                };
                if address_number_terms(stated).any(|term| sought(&term))
                    || flat_number_term(stated, unit).is_some_and(|term| sought(&term))
                {
                    matched.push((record_id, None));
                } else if let Some(term) = address_number_terms(stated)
                    .find(|term| ranking.abbreviated_numbers.contains(term))
                {
                    matched.push((record_id, Some(term)));
                }
                Ok(())
            })?;
        Ok(matched)
    }

    fn place_on_street_line(&self, record_id: RecordId, record: &mut Summary) -> Result<()> {
        let Some(point) = record.point else {
            return Ok(());
        };
        let Some([lon, lat]) = self.spatial.point_along_line(record_id, 0.5)? else {
            return Ok(());
        };
        record.point = Some(Point {
            lon,
            lat,
            precision: point.precision,
        });
        Ok(())
    }

    fn estimate_house(
        &self,
        record_id: RecordId,
        number: u32,
        record: &mut Summary,
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
        record.point = Some(Point {
            lon,
            lat,
            precision: PointPrecision::Estimated,
        });
        if let Some(label) = labels::estimated_address_label(number, &interpolation.address) {
            record.label = label;
        }
        if let Some(name) = labels::estimated_address_name(number, &interpolation.address) {
            record.name = name;
        }
        record.address = Address {
            housenumber: Some(number.to_string()),
            ..Address::from(&interpolation.address)
        };
        Ok(true)
    }

    fn tiger_estimate(
        &self,
        street: &TextSearchHit,
        ranking: &Ranking,
    ) -> Result<Option<TextSearchHit>> {
        let Some(number) = ranking.house_number else {
            return Ok(None);
        };
        if street.record.layer != Layer::Street.as_str() {
            return Ok(None);
        }
        let written = &ranking.query_words.written;
        let same_street = if ranking.postcode.explicit {
            let number = number.to_string();
            written
                .split_once(' ')
                .filter(|(first, _)| *first == number)
                .map(|(_, street)| street)
                .or_else(|| {
                    written
                        .rsplit_once(' ')
                        .filter(|(_, last)| *last == number)
                        .map(|(street, _)| street)
                })
                .and_then(street_search_text)
                .zip(street_search_text(&street.record.name))
                .is_some_and(|(asked, name)| asked == name)
        } else {
            tiger_street_written(written, number, &street.record.name)
        };
        if !same_street {
            return Ok(None);
        }
        let mut ranges = self
            .pack
            .tiger_ranges
            .for_street(street.record_id)
            .filter_map(|range| {
                let fraction = range.fraction(number)?;
                let postcode = range.postcode().or_else(|| {
                    self.pack
                        .tiger_side_zips
                        .postcode(range.side_key(range.side))
                });
                if !ranking.postcode.exact.is_empty()
                    && postcode
                        .as_ref()
                        .is_some_and(|zip| zip != &ranking.postcode.exact)
                {
                    return None;
                }
                Some((range, fraction, postcode))
            });
        let Some((range, fraction, postcode)) = ranges.next() else {
            return Ok(None);
        };
        if ranges.next().is_some() {
            return Ok(None);
        }
        let Some([lon, lat]) = self.spatial.point_along_line(street.record_id, fraction)? else {
            return Ok(None);
        };
        let mut hit = street.clone();
        let address = &hit.record.address;
        let locality = self
            .pack
            .street(street.record_id)?
            .and_then(|record| record.locality);
        let region = self
            .pack
            .boundary_context(street.record_id)?
            .and_then(|context| context.admin_context.get(PlaceLayer::Region))
            .and_then(|id| self.pack.subdivision_code(id))
            .and_then(|iso| iso.strip_prefix("US-"))
            .map(str::to_string);
        let components = InterpolationAddressComponents {
            street: Some(hit.record.name.clone()),
            locality: address.locality.clone().or(locality),
            region: address.region.clone().or(region),
            country: address.country.clone(),
            postcode,
            place: None,
        };
        let record = &mut hit.record;
        record.id = range.id(number);
        record.layer = Layer::Interpolation.as_str().into();
        record.name = labels::estimated_address_name(number, &components)
            .unwrap_or_else(|| format!("{number} {}", street.record.name));
        record.label = labels::estimated_address_label(number, &components)
            .unwrap_or_else(|| record.name.clone());
        record.address = Address {
            housenumber: Some(number.to_string()),
            ..Address::from(&components)
        };
        record.point = Some(Point {
            lon,
            lat,
            precision: PointPrecision::Estimated,
        });
        record.source = Source {
            dataset: crate::tiger::DATASET.into(),
            object_type: None,
            object_id: None,
            derived_from: None,
            record_count: None,
        };
        hit.signals.house_level = true;
        Ok(Some(hit))
    }

    fn place_between_neighbours(
        &self,
        plan: &QueryPlan,
        pass: Pass,
        ranking: &Ranking,
        areas: &AreaWords,
        filters: &Filters<'_>,
    ) -> Result<Option<TextSearchHit>> {
        let Some(number) = plan.house_number.as_ref().map(|number| number.base) else {
            return Ok(None);
        };
        let (lower, higher) = self.neighbours(plan, pass, ranking, areas, filters, number)?;
        self.place_between(number, &lower, &higher, NEIGHBOUR_STREET_RADIUS_M)
    }

    fn place_between(
        &self,
        number: u32,
        lower: &[Neighbour],
        higher: &[Neighbour],
        setback_m: f64,
    ) -> Result<Option<TextSearchHit>> {
        if lower.is_empty() {
            return Ok(None);
        }
        let parity = |neighbour: &Neighbour| neighbour.number % 2 == number % 2;
        let mut pairs = lower
            .iter()
            .flat_map(|low| higher.iter().map(move |high| (low, high)))
            .filter(|(low, high)| {
                low.street == high.street
                    || road::same_road_near(
                        &low.street,
                        low.point,
                        &high.street,
                        high.point,
                        road::ROAD_LINK_M,
                    )
            })
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
                    Entry::Vacant(entry) => *entry.insert(self.on_road(
                        neighbour.point,
                        &neighbour.street,
                        setback_m,
                    )?),
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
            if self.on_road(point, &low.street, setback_m)? {
                return Ok(Some(self.neighbour_estimate(number, low, high, point)?));
            }
        }
        Ok(None)
    }

    fn neighbours(
        &self,
        plan: &QueryPlan,
        pass: Pass,
        ranking: &Ranking,
        areas: &AreaWords,
        filters: &Filters<'_>,
        number: u32,
    ) -> Result<(Vec<Neighbour>, Vec<Neighbour>)> {
        let searcher = self.reader.searcher();
        let mut clauses = self.subject_clauses(plan, pass, areas);
        clauses.push((Occur::Must, self.layer_query(Layer::Address.as_str())));
        if let Some(countries) = self.countries_query(filters.countries) {
            clauses.push((Occur::Must, countries));
        }
        let docs = searcher.search(
            &BooleanQuery::new(clauses),
            &TopDocs::with_limit(NEIGHBOUR_DOCUMENTS),
        )?;
        self.neighbours_of(
            &searcher,
            docs.into_iter().map(|(_, doc)| doc).collect(),
            ranking,
            number,
        )
    }

    fn neighbours_of(
        &self,
        searcher: &Searcher,
        docs: Vec<DocAddress>,
        ranking: &Ranking,
        number: u32,
    ) -> Result<(Vec<Neighbour>, Vec<Neighbour>)> {
        let mut lower = Vec::new();
        let mut higher = Vec::new();
        for doc in docs {
            let columns = self.segment_columns(searcher, doc.segment_ord)?;
            let postcodes = |column: &Option<StrColumn>| -> Result<Vec<String>> {
                let Some(column) = column else {
                    return Ok(Vec::new());
                };
                let mut postcodes = Vec::new();
                for ord in column.term_ords(doc.doc_id) {
                    let mut postcode = String::new();
                    if column.ord_to_str(ord, &mut postcode)? {
                        postcodes.push(postcode);
                    }
                }
                Ok(postcodes)
            };
            let postcode_agreement = ranking.postcode.rank_of(
                &postcodes(&columns.rank_postcode)?,
                postcodes(&columns.area_postcode)?
                    .first()
                    .map(String::as_str),
            );
            let mut consider = |record_id: RecordId, stated: &str| {
                if let Some(base) = base_house_number(stated) {
                    let side = if base <= number {
                        &mut lower
                    } else {
                        &mut higher
                    };
                    side.push((base, record_id, postcode_agreement));
                }
            };
            let (first, count) = self.document_records(searcher, doc)?;
            match self.pack.records().record(first)? {
                Record::Address(_) => {
                    self.pack
                        .records()
                        .address_numbers(first, count, |record_id, stated, _| {
                            consider(record_id, stated);
                            Ok(())
                        })?
                }
                Record::Poi(PoiRecord {
                    address: Some(address),
                    ..
                }) => consider(first, &address.number),
                _ => {}
            }
        }
        lower.sort_by_key(|&(base, record_id, _)| (std::cmp::Reverse(base), record_id));
        higher.sort_by_key(|&(base, record_id, _)| (base, record_id));
        let found = |candidates: Vec<(u32, RecordId, u8)>| -> Result<Vec<Neighbour>> {
            let mut neighbours = Vec::new();
            for (number, record_id, postcode_agreement) in
                candidates.into_iter().take(NEIGHBOUR_CANDIDATES)
            {
                neighbours.extend(self.neighbour(record_id, number, postcode_agreement)?);
            }
            Ok(neighbours)
        };
        Ok((found(lower)?, found(higher)?))
    }

    fn neighbour(
        &self,
        record_id: RecordId,
        number: u32,
        postcode_agreement: u8,
    ) -> Result<Option<Neighbour>> {
        let address = match self.pack.records().record(record_id)? {
            Record::Address(record) => record.address,
            Record::Poi(PoiRecord {
                address: Some(address),
                ..
            }) => address,
            _ => return Ok(None),
        };
        let Some(street) = address
            .street
            .as_deref()
            .or(address.place.as_deref())
            .and_then(street_search_text)
        else {
            return Ok(None);
        };
        let (_, lon, lat) = self.pack.records().point(record_id)?;
        Ok(Some(Neighbour {
            record_id,
            number,
            point: [lon, lat],
            street,
            address,
            postcode_agreement,
        }))
    }

    fn segment_columns(&self, searcher: &Searcher, segment_ord: u32) -> Result<&SegmentColumns> {
        let segment = searcher.segment_reader(segment_ord).segment_id();
        self.columns
            .get(&segment)
            .or_else(|| self.famed.get()?.columns.get(&segment))
            .ok_or_else(|| Error::Inconsistent("text index segment has no fast fields".into()))
    }

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
                && street_search_text(&self.pack.records().summary(candidate.record_id)?.name)
                    .as_deref()
                    == Some(street)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn on_road(&self, [lon, lat]: [f64; 2], street: &str, within_m: f64) -> Result<bool> {
        let Some(road) = road::RoadName::of(street) else {
            return Ok(false);
        };
        let mut seen = HashSet::new();
        for candidate in self
            .spatial
            .segment_candidates(lon, lat, Layer::Street, within_m, 0)?
        {
            if seen.insert(candidate.record_id)
                && road::RoadName::of(&self.pack.records().summary(candidate.record_id)?.name)
                    .is_some_and(|other| other.same_road(&road))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

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
            named_place: false,
            record_id: nearer.record_id,
            score: 0.0,
            record: Summary {
                id: labels::estimated_address_id(number, &id(low)?, &id(high)?),
                layer: Layer::Address.as_str().to_string(),
                name: labels::address_name(&address),
                label: labels::address_label(&address),
                address: Address::from(&address),
                category: None,
                road_class: None,
                brand: None,
                contact: None,
                opening_hours: None,
                cuisine: Vec::new(),
                bbox: None,
                timezone: None,
                street_postcode: None,
                access: Vec::new(),
                building: None,
                point: Some(Point {
                    lon,
                    lat,
                    precision: PointPrecision::Estimated,
                }),
                source: Source {
                    dataset: "osm".to_string(),
                    object_type: None,
                    object_id: None,
                    derived_from: Some(DERIVED_FROM_ADDRESS_RECORDS.to_string()),
                    record_count: Some(2),
                },
            },
            features: Features::default(),
            confidence: None,
            fallback: false,
            signals: Signals::default(),
            matched: QueryMatch::default(),
            highlights: Vec::new(),
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct Stands {
    locality: Option<Option<Standing>>,
    postcode: Option<Standing>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Standing {
    Own,
    Taken,
    Contradicts,
}

impl Standing {
    fn agrees(self) -> bool {
        self != Self::Contradicts
    }

    fn part(standing: Option<Self>) -> PartMatch {
        match standing {
            Some(Self::Own) => PartMatch::Matched,
            Some(Self::Taken) => PartMatch::Partial,
            Some(Self::Contradicts) | None => PartMatch::Unmatched,
        }
    }
}

#[derive(Debug, Default)]
struct RecordAreas {
    regions: Vec<String>,
    localities: Vec<String>,
    chain: Vec<RecordId>,
}

impl RecordAreas {
    fn add_region(&mut self, name: Option<&str>) {
        if let Some(name) = normalized_for_match(name) {
            self.regions.push(name);
        }
    }

    fn add_locality(&mut self, name: &str) {
        if let Some(name) = normalized_for_match(Some(name)) {
            self.localities.push(name);
        }
    }
}

#[derive(Debug, Default)]
struct Parts {
    house_number: HashSet<String>,
    unit: HashSet<String>,
    street: HashSet<String>,
    locality: HashSet<String>,
    region: HashSet<String>,
    regions: Rc<Vec<RecordId>>,
    postcode: HashSet<String>,
}

impl Parts {
    fn labels(&self, word: &str, regions: impl Fn(&str) -> Vec<RecordId>) -> LabelSet {
        let names = |part: &HashSet<String>| {
            part.is_empty()
                || part.contains(word)
                || lattice::other_forms(word)
                    .iter()
                    .any(|form| part.contains(form))
                || part.iter().any(|name| typo_distance(word, name).is_some())
        };
        let mut labels = LabelSet::default().with(Label::Other).with(Label::Country);
        for (label, part) in [
            (Label::HouseNumber, &self.house_number),
            (Label::Unit, &self.unit),
            (Label::Street, &self.street),
            (Label::Locality, &self.locality),
            (Label::Postcode, &self.postcode),
        ] {
            if names(part) {
                labels = labels.with(label);
            }
        }
        if names(&self.region)
            || regions(word)
                .iter()
                .any(|region| self.regions.contains(region))
        {
            labels = labels.with(Label::Region);
        }
        labels
    }
}

#[derive(Debug)]
struct Neighbour {
    record_id: RecordId,
    number: u32,
    point: [f64; 2],
    street: String,
    address: AddressComponents,
    postcode_agreement: u8,
}

#[derive(Debug, Clone, PartialEq)]
struct RankKey {
    fame: u64,
    house_level: bool,
    postcode_agreement: u8,
    stated: bool,
    named_place: bool,
    near: u8,
    text_score: Score,
    name_words: u8,
    prominence: u64,
    place_rank: u8,
    nearness: u8,
    poi_prominence: u64,
}

impl RankKey {
    fn text(text_score: Score) -> Self {
        Self {
            fame: 0,
            house_level: true,
            postcode_agreement: 0,
            stated: true,
            named_place: false,
            near: 0,
            text_score,
            name_words: u8::MAX,
            prominence: 0,
            place_rank: u8::MAX,
            nearness: 0,
            poi_prominence: 0,
        }
    }

    fn cmp_evidence(&self, other: &Self) -> std::cmp::Ordering {
        (
            self.fame,
            self.house_level,
            self.postcode_agreement,
            self.stated,
            self.named_place,
            self.near,
            self.text_score,
        )
            .partial_cmp(&(
                other.fame,
                other.house_level,
                other.postcode_agreement,
                other.stated,
                other.named_place,
                other.near,
                other.text_score,
            ))
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

impl PartialOrd for RankKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp_evidence(other).then_with(|| {
            (
                other.name_words,
                self.prominence,
                other.place_rank,
                self.nearness,
                self.poi_prominence,
            )
                .cmp(&(
                    self.name_words,
                    other.prominence,
                    self.place_rank,
                    other.nearness,
                    other.poi_prominence,
                ))
        }))
    }
}

const FOCUS_BANDS_M: [f64; 11] = [
    1_000.0,
    2_000.0,
    5_000.0,
    10_000.0,
    20_000.0,
    50_000.0,
    100_000.0,
    200_000.0,
    500_000.0,
    1_000_000.0,
    2_000_000.0,
];

const BIAS_BANDS: usize = 6;
pub(crate) const BIAS_RADIUS_M: f64 = FOCUS_BANDS_M[BIAS_BANDS - 1];

fn bias_bounds([lon, lat]: [f64; 2]) -> BoundingBox {
    const METRES_PER_DEGREE: f64 = 110_000.0;
    let lat_span = BIAS_RADIUS_M / METRES_PER_DEGREE;
    let min_lat = (lat - lat_span).max(-90.0);
    let max_lat = (lat + lat_span).min(90.0);
    let cos = min_lat.abs().max(max_lat.abs()).to_radians().cos();
    let lon_span = lat_span / cos;
    let (min_lon, max_lon) = if lon - lon_span >= -180.0 && lon + lon_span <= 180.0 {
        (lon - lon_span, lon + lon_span)
    } else {
        (-180.0, 180.0)
    };
    BoundingBox {
        min_lon,
        min_lat,
        max_lon,
        max_lat,
    }
}

fn within_bias(hit: &TextSearchHit, [lon, lat]: [f64; 2]) -> bool {
    hit.record
        .point
        .is_some_and(|point| haversine_m(lon, lat, point.lon, point.lat) <= BIAS_RADIUS_M)
}

fn known_abroad(prominence: u64) -> bool {
    prominence >> 8 > 0 || (prominence & 0x80 != 0 && prominence & 0x7f > 0)
}

fn focus_band(distance_m: f64) -> u8 {
    FOCUS_BANDS_M
        .iter()
        .position(|band| distance_m <= *band)
        .unwrap_or(FOCUS_BANDS_M.len()) as u8
}

struct SegmentFocus {
    focus: [f64; 2],
    min_lon: Column<i64>,
    min_lat: Column<i64>,
    max_lon: Column<i64>,
    max_lat: Column<i64>,
}

impl SegmentFocus {
    fn nearness(&self, doc: DocId) -> u8 {
        let read = |column: &Column<i64>| column.first(doc).map(|value| value as i32);
        let (Some(min_lon), Some(min_lat), Some(max_lon), Some(max_lat)) = (
            read(&self.min_lon),
            read(&self.min_lat),
            read(&self.max_lon),
            read(&self.max_lat),
        ) else {
            return 0;
        };
        let [lon, lat] = self.focus;
        let lon = lon.clamp(dequantize(min_lon), dequantize(max_lon));
        let lat = lat.clamp(dequantize(min_lat), dequantize(max_lat));
        FOCUS_BANDS_M.len() as u8 - focus_band(haversine_m(self.focus[0], self.focus[1], lon, lat))
    }
}

struct RankKeys<'a> {
    ranking: &'a Ranking,
    columns: &'a HashMap<SegmentId, SegmentColumns>,
    pack: &'a Arc<pack::Reader>,
}

struct SegmentRankKeys {
    numbered: bool,
    near_first: bool,
    fame_first: bool,
    typed_words: u8,
    whole_last: Option<SegmentPostings>,
    house_number: Column<u64>,
    interpolation_start: Column<u64>,
    name_words: Column<u64>,
    prominence: Column<u64>,
    place_rank: Column<u64>,
    postcode: Option<SegmentPostcodes>,
    focus: Option<SegmentFocus>,
}

struct SegmentPostcodes {
    own: Option<PostcodeColumn>,
    area: Option<PostcodeColumn>,
    nearness: Option<PostcodeNearness>,
}

impl SegmentPostcodes {
    fn rank(&self, doc: DocId) -> u8 {
        let own = self
            .own
            .as_ref()
            .and_then(|own| own.read(own.column.term_ords(doc)));
        let area = self
            .area
            .as_ref()
            .and_then(|area| area.read(area.column.term_ords(doc).take(1)));
        QueryPostcode::rank(own, area, || {
            self.nearness
                .as_ref()
                .and_then(|nearness| nearness.band(doc))
        })
    }
}

struct PostcodeColumn {
    column: StrColumn,
    prefixes: Vec<Vec<Range<u64>>>,
    held: Vec<Range<u64>>,
}

impl PostcodeColumn {
    fn new(column: &StrColumn, postcode: &QueryPostcode) -> io::Result<Self> {
        let prefixes = postcode.ord_ranges(column)?;
        let mut held = prefixes
            .iter()
            .filter_map(|form| form.last().cloned())
            .collect::<Vec<_>>();
        let dictionary = column.dictionary();
        for form in &postcode.forms {
            for (index, character) in form.char_indices() {
                let end = index + character.len_utf8();
                if end < form.len()
                    && let Some(ord) = dictionary.term_ord(&form.as_bytes()[..end])?
                {
                    held.push(ord..ord + 1);
                }
            }
        }
        Ok(Self {
            column: column.clone(),
            prefixes,
            held,
        })
    }

    fn read(&self, ords: impl Iterator<Item = u64>) -> Option<(bool, u8)> {
        ords.map(|ord| {
            (
                self.held.iter().any(|held| held.contains(&ord)),
                QueryPostcode::agreement_of_ord(&self.prefixes, ord),
            )
        })
        .reduce(|(left_held, left), (right_held, right)| (left_held || right_held, left.max(right)))
    }
}

struct PostcodeNearness {
    record_id: Column<u64>,
    pack: Arc<pack::Reader>,
    postcode: QueryPostcode,
}

impl PostcodeNearness {
    fn band(&self, doc: DocId) -> Option<u8> {
        let distance = self
            .record_id
            .first(doc)
            .and_then(|record_id| self.pack.records().point(record_id).ok())
            .and_then(|(_, lon, lat)| self.postcode.distance_m(lon, lat))?;
        POSTCODE_NEARNESS_BANDS_M
            .iter()
            .position(|band| distance <= *band)
            .map(|band| band as u8)
    }
}

struct FameKeys<'a> {
    columns: &'a HashMap<SegmentId, SegmentColumns>,
    focus: Option<[f64; 2]>,
}

struct SegmentFameKeys {
    prominence: Column<u64>,
    focus: Option<SegmentFocus>,
}

impl ScoreTweaker<(u8, u64, Score)> for FameKeys<'_> {
    type Child = SegmentFameKeys;

    fn segment_tweaker(&self, segment: &SegmentReader) -> tantivy::Result<SegmentFameKeys> {
        let columns = self.columns.get(&segment.segment_id()).ok_or_else(|| {
            tantivy::TantivyError::InternalError("text index segment has no fast fields".into())
        })?;
        Ok(SegmentFameKeys {
            prominence: columns.prominence.clone(),
            focus: self.focus.map(|focus| SegmentFocus {
                focus,
                min_lon: columns.min_lon.clone(),
                min_lat: columns.min_lat.clone(),
                max_lon: columns.max_lon.clone(),
                max_lat: columns.max_lat.clone(),
            }),
        })
    }
}

impl ScoreSegmentTweaker<(u8, u64, Score)> for SegmentFameKeys {
    fn score(&mut self, doc: DocId, text_score: Score) -> (u8, u64, Score) {
        (
            self.focus.as_ref().map_or(0, |focus| focus.nearness(doc)),
            self.prominence.first(doc).unwrap_or_default(),
            text_score,
        )
    }
}

impl ScoreTweaker<RankKey> for RankKeys<'_> {
    type Child = SegmentRankKeys;

    fn segment_tweaker(&self, segment: &SegmentReader) -> tantivy::Result<SegmentRankKeys> {
        let columns = self.columns.get(&segment.segment_id()).ok_or_else(|| {
            tantivy::TantivyError::InternalError("text index segment has no fast fields".into())
        })?;
        let named = &self.ranking.postcode;
        let column = |column: &Option<StrColumn>| {
            column
                .as_ref()
                .map(|column| PostcodeColumn::new(column, named))
                .transpose()
        };
        let postcode = match named.forms.is_empty() {
            true => None,
            false => Some(SegmentPostcodes {
                own: column(&columns.rank_postcode)?,
                area: column(&columns.area_postcode)?,
                nearness: (!named.areas.is_empty()).then(|| PostcodeNearness {
                    record_id: columns.record_id.clone(),
                    pack: Arc::clone(self.pack),
                    postcode: named.clone(),
                }),
            }),
        };
        Ok(SegmentRankKeys {
            numbered: self.ranking.numbered,
            near_first: self.ranking.near_first,
            fame_first: self.ranking.fame_first,
            typed_words: self.ranking.typed_words,
            whole_last: match &self.ranking.whole_last {
                Some(term) => segment
                    .inverted_index(term.field())?
                    .read_postings(term, IndexRecordOption::Basic)?,
                None => None,
            },
            house_number: columns.house_number.clone(),
            interpolation_start: columns.interpolation_start.clone(),
            name_words: columns.name_words.clone(),
            prominence: columns.prominence.clone(),
            place_rank: columns.place_rank.clone(),
            postcode,
            focus: self.ranking.focus.map(|focus| SegmentFocus {
                focus,
                min_lon: columns.min_lon.clone(),
                min_lat: columns.min_lat.clone(),
                max_lon: columns.max_lon.clone(),
                max_lat: columns.max_lat.clone(),
            }),
        })
    }
}

impl ScoreSegmentTweaker<RankKey> for SegmentRankKeys {
    fn score(&mut self, doc: DocId, text_score: Score) -> RankKey {
        let mut key = RankKey::text(text_score);
        let small =
            |value: Option<u64>| value.map_or(u8::MAX, |value| value.min(u64::from(u8::MAX)) as u8);
        key.name_words = small(self.name_words.first(doc));
        let prominence = self.prominence.first(doc).unwrap_or_default();
        match self.place_rank.first(doc) {
            Some(rank) => {
                key.prominence = prominence;
                key.place_rank = small(Some(rank));
                if self.fame_first {
                    key.fame = prominence >> 8;
                }
            }
            None => {
                key.poi_prominence = prominence;
                if self.fame_first {
                    key.fame = prominence >> 8;
                }
            }
        }
        if self.numbered && self.house_number.first(doc).is_none() {
            key.house_level = self.interpolation_start.first(doc).is_some();
            key.stated = false;
        }
        if let Some(postcode) = &self.postcode {
            key.postcode_agreement = postcode.rank(doc);
        }
        if let Some(focus) = &self.focus {
            key.nearness = focus.nearness(doc);
            if self.near_first && usize::from(key.nearness) > FOCUS_BANDS_M.len() - BIAS_BANDS {
                key.near = key.nearness;
            }
        }
        if self.near_first {
            let mut whole = || {
                self.whole_last
                    .as_mut()
                    .is_some_and(|postings| postings.doc() <= doc && postings.seek(doc) == doc)
            };
            key.named_place = key.place_rank != u8::MAX
                && key.name_words <= self.typed_words
                && (key.near > 0 || (known_abroad(key.prominence) && whole()));
        }
        key
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TieBreak {
    extra_street_words: usize,
    unit: u8,
    extra_name_words: usize,
    missing_query_words: usize,
    inexact_query_words: usize,
    fame: std::cmp::Reverse<Prominence>,
    place_rank: u8,
    focus_band: u8,
    disputed: bool,
    poi: Option<std::cmp::Reverse<Prominence>>,
    precision: u8,
    source: (u8, i64),
    id: String,
}

impl TieBreak {
    fn new(record: &Summary, prior: Prior, query_words: &HashSet<&str>, ranking: &Ranking) -> Self {
        let label = search_key(&record.label).unwrap_or_default();
        let label_words = label.split_whitespace().collect::<HashSet<_>>();
        let extra_name_words = search_key(&record.name)
            .unwrap_or_default()
            .split_whitespace()
            .filter(|word| !query_words.contains(word))
            .count();
        let missing_query_words = query_words
            .iter()
            .filter(|word| !label_words.contains(*word))
            .count();
        let exact_label = exact_key(&record.label).unwrap_or_default();
        let exact_label_words = exact_label.split_whitespace().collect::<HashSet<_>>();
        let inexact_query_words = ranking
            .query_words
            .exact
            .iter()
            .filter(|word| !exact_label_words.contains(word.as_str()))
            .count();
        let precision = match record.point.map(|point| point.precision) {
            Some(PointPrecision::Point) => 0,
            Some(PointPrecision::Centroid) => 1,
            Some(PointPrecision::Estimated) => 2,
            Some(PointPrecision::RepresentativePoint) => 3,
            None => 4,
        };
        let source = match record.source.object_type {
            Some(ObjectType::Node) => 0,
            Some(ObjectType::Way) => 1,
            Some(ObjectType::Relation) => 2,
            Some(ObjectType::Row) => 3,
            None => 4,
        };
        let focus_band = ranking
            .focus
            .zip(record.point)
            .map_or(0, |([lon, lat], point)| {
                focus_band(haversine_m(lon, lat, point.lon, point.lat))
            });
        let street = if !ranking.numbered {
            None
        } else if record.layer == Layer::Street.as_str() {
            Some(record.name.as_str())
        } else {
            record
                .address
                .street
                .as_deref()
                .or(record.address.place.as_deref())
        };
        let extra_street_words = street
            .and_then(search_key)
            .unwrap_or_default()
            .split_whitespace()
            .filter(|word| {
                !query_words.contains(word)
                    && !query_words.iter().any(|query_word| {
                        typo_distance(query_word, word).is_some()
                            || lattice::other_forms(query_word)
                                .iter()
                                .any(|form| form == word)
                    })
                    && !ranking
                        .sought
                        .iter()
                        .any(|sought| sought.corrected.as_deref() == Some(*word))
            })
            .count();
        Self {
            extra_street_words,
            unit: unit_rank(record, ranking),
            extra_name_words,
            missing_query_words,
            inexact_query_words,
            fame: std::cmp::Reverse(
                prior
                    .prominence
                    .filter(|_| record.layer != Layer::Poi.as_str())
                    .unwrap_or_default(),
            ),
            place_rank: prior.place_rank.unwrap_or(u8::MAX),
            focus_band,
            disputed: prior.disputed,
            poi: prior
                .prominence
                .filter(|_| record.layer == Layer::Poi.as_str())
                .map(std::cmp::Reverse),
            precision,
            source: (source, record.source.object_id.unwrap_or_default()),
            id: record.id.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Prior {
    place_rank: Option<u8>,
    prominence: Option<Prominence>,
    disputed: bool,
}

fn unit_rank(record: &Summary, ranking: &Ranking) -> u8 {
    let as_written = ranking.flat_text.is_some()
        && record.address.housenumber.as_deref().and_then(search_key) == ranking.flat_text;
    let stated = record.address.unit.as_deref().and_then(search_key);
    match (ranking.unit.as_deref(), stated.as_deref()) {
        _ if as_written => 0,
        (Some(sought), Some(stated)) if names_unit(stated, sought) => 0,
        (Some(_), None) | (None, None) => 1,
        (_, Some(_)) => 2,
    }
}

fn names_unit(stated: &str, sought: &str) -> bool {
    stated.split_whitespace().any(|word| word == sought)
}

fn unit_match(record: &Summary, ranking: &Ranking) -> Option<PartMatch> {
    let sought = ranking.unit.as_deref()?;
    let key = |value: &Option<String>| value.as_deref().and_then(search_key);
    let states = key(&record.address.unit).is_some_and(|stated| names_unit(&stated, sought))
        || (ranking.flat_text.is_some() && key(&record.address.housenumber) == ranking.flat_text);
    Some(match states {
        true => PartMatch::Matched,
        false => PartMatch::Unmatched,
    })
}

fn states_number(record: &Summary, ranking: &Ranking) -> bool {
    record
        .address
        .housenumber
        .as_deref()
        .and_then(search_key)
        .is_some_and(|stated| ranking.states_number(&stated))
}

fn number_match(record: &Summary, number: &HouseNumber, ranking: &Ranking) -> PartMatch {
    let estimated = record
        .point
        .is_some_and(|point| point.precision == PointPrecision::Estimated);
    if states_number(record, ranking) {
        return match estimated {
            true => PartMatch::Partial,
            false => PartMatch::Matched,
        };
    }
    let base = record
        .address
        .housenumber
        .as_deref()
        .and_then(search_key)
        .and_then(|stated| base_house_number(&stated));
    match base == Some(number.base) {
        true => PartMatch::Partial,
        false => PartMatch::Unmatched,
    }
}

fn subject_match(
    part: Label,
    plan: &QueryPlan,
    record: &Summary,
    dictionaries: &Dictionaries,
) -> Option<PartMatch> {
    let named = if part != Label::Street {
        area_layers(part)
            .iter()
            .any(|layer| layer.as_str() == record.layer)
            .then_some(record.name.as_str())?
    } else if record.layer == Layer::Street.as_str() {
        record.name.as_str()
    } else {
        record.address.street.as_deref().unwrap_or_default()
    };
    let key = search_key(named).unwrap_or_default();
    let label = key.split_whitespace().collect::<Vec<_>>();
    let mut covered = vec![false; label.len()];
    let (mut written, mut near) = (0, 0);
    for word in &plan.subject {
        let forms = lattice::forms_both_ways(&word.text);
        let as_written = |label: &str| label == word.text || forms.iter().any(|form| form == label);
        let close = |label: &str| {
            word.corrected.as_deref() == Some(label) || typo_distance(&word.text, label).is_some()
        };
        if let Some(at) = label.iter().position(|label| as_written(label)) {
            written += 1;
            covered[at] = true;
        } else if let Some(at) = label.iter().position(|label| close(label)) {
            near += 1;
            covered[at] = true;
        }
    }
    let whole = label.iter().zip(&covered).all(|(word, covered)| {
        *covered
            || (part == Label::Street
                && dictionaries.is_street_type(word)
                && !dictionaries.is_direction(word))
    });
    let as_written = !plan.subject_part.1.0;
    Some(match (written, near) {
        (0, 0) => PartMatch::Unmatched,
        (written, _) if written == plan.subject.len() && whole && as_written => PartMatch::Matched,
        _ => PartMatch::Partial,
    })
}

fn contradicts_country(
    matched: Option<PartMatch>,
    stated: &[CountryCode],
    actual: Option<CountryCode>,
) -> bool {
    matched == Some(PartMatch::Unmatched)
        && !stated.is_empty()
        && actual.is_some_and(|country| !stated.contains(&country))
}

fn stated_country(countries: &[String], hits: &mut [TextSearchHit]) {
    if countries.is_empty() {
        return;
    }
    for hit in hits {
        hit.matched.country.get_or_insert(PartMatch::Matched);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Numbered {
    Yes,
    No,
}

impl Numbered {
    fn of(plan: &QueryPlan) -> Self {
        match plan.house_number.is_some() {
            true => Self::Yes,
            false => Self::No,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Depth {
    Query,
    Bare,
    Retry,
}

struct Variants {
    query: String,
    hits: Vec<TextSearchHit>,
    numbered: Numbered,
    order: Option<names::NameOrder>,
    named: choose::Named,
    countries: Vec<CountryCode>,
}

#[derive(Debug, Clone, Copy)]
struct ByName {
    exact: bool,
    known: bool,
}

fn named_part(layer: &str) -> Option<Label> {
    if layer == Layer::Street.as_str() {
        return Some(Label::Street);
    }
    [
        Label::Locality,
        Label::Region,
        Label::Postcode,
        Label::Country,
    ]
    .into_iter()
    .find(|label| {
        area_layers(*label)
            .iter()
            .any(|area| area.as_str() == layer)
    })
}

fn area_layers(label: Label) -> &'static [Layer] {
    match label {
        Label::Postcode => &[Layer::Postcode],
        Label::Locality => &[
            Layer::Locality,
            Layer::Neighbourhood,
            Layer::District,
            Layer::Place,
        ],
        Label::Region => &[Layer::Region, Layer::District],
        Label::Country => &[Layer::Country],
        Label::HouseNumber | Label::Unit | Label::Street | Label::Other => &[],
    }
}

fn unjudged_unmatched(stated: &[Label], matched: &mut QueryMatch) {
    if matched.is_empty() {
        return;
    }
    for label in stated {
        if let Some(slot) = part_of(matched, *label) {
            slot.get_or_insert(PartMatch::Unmatched);
        }
    }
}

fn part_of(matched: &mut QueryMatch, label: Label) -> Option<&mut Option<PartMatch>> {
    match label {
        Label::HouseNumber => Some(&mut matched.housenumber),
        Label::Unit => Some(&mut matched.unit),
        Label::Street => Some(&mut matched.street),
        Label::Locality => Some(&mut matched.locality),
        Label::Region => Some(&mut matched.region),
        Label::Postcode => Some(&mut matched.postcode),
        Label::Country => Some(&mut matched.country),
        Label::Other => None,
    }
}

fn sure_abbreviations<'h>(
    hits: impl Iterator<Item = &'h TextSearchHit>,
    abbreviations: &[Option<String>],
    ranking: &Ranking,
) -> Vec<bool> {
    let street = |hit: &TextSearchHit| {
        let key = |value: &Option<String>| value.as_deref().and_then(search_key);
        (
            key(&hit.record.address.street),
            key(&hit.record.address.locality),
        )
    };
    let hits = hits.collect::<Vec<_>>();
    let mut full = HashSet::new();
    let mut named = HashMap::<_, Vec<Vec<&str>>>::new();
    for (hit, abbreviated) in hits.iter().zip(abbreviations) {
        match abbreviated {
            Some(term) => {
                let runs = number_runs(term);
                let numbers = named.entry(street(hit)).or_default();
                if !numbers.contains(&runs) {
                    numbers.push(runs);
                }
            }
            None if hit
                .record
                .address
                .housenumber
                .as_deref()
                .and_then(search_key)
                .is_some_and(|stated| ranking.number_texts.contains(&stated)) =>
            {
                full.insert(street(hit));
            }
            None => {}
        }
    }
    hits.iter()
        .zip(abbreviations)
        .map(|(hit, abbreviated)| {
            abbreviated.is_none() || {
                let street = street(hit);
                !full.contains(&street)
                    && named.get(&street).is_some_and(|numbers| numbers.len() == 1)
            }
        })
        .collect()
}

fn label_words(record: &Summary) -> Vec<String> {
    search_key(&record.label)
        .iter()
        .flat_map(|label| label.split_whitespace())
        .map(str::to_string)
        .collect()
}

fn count<T>(items: impl Iterator<Item = T>) -> u8 {
    items.count().min(usize::from(u8::MAX)) as u8
}

fn agreement(agrees: Option<bool>) -> Agreement {
    match agrees {
        Some(true) => Agreement::Agrees,
        Some(false) => Agreement::Contradicts,
        None => Agreement::Unknown,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parses {
    All,
    Numbered,
}

#[derive(Clone, Copy)]
enum Typed<'a> {
    Whole,
    Prefix,
    Completions(&'a [String]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Completion {
    InOrder,
    CommonCompletions,
    AllWords,
    AddressPrefix,
    Typos,
}

impl Completion {
    const ALL: [Self; 4] = [
        Self::InOrder,
        Self::CommonCompletions,
        Self::AllWords,
        Self::Typos,
    ];
}

/// The retrieval passes of [`TextSearcher::search_variants`], in order,
/// strictest first. The order is also how strongly a hit found by each
/// answers the query.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pass {
    #[default]
    Strict,
    Street,
    Lenient,
}

impl Pass {
    fn requires_locality(self) -> bool {
        self != Self::Lenient
    }

    fn street_stands_in(self) -> bool {
        self != Self::Strict
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Scoped,
    Open,
}

impl Scope {
    const ALL: [Self; 2] = [Self::Scoped, Self::Open];

    fn pass(self) -> Pass {
        match self {
            Self::Scoped => Pass::Street,
            Self::Open => Pass::Lenient,
        }
    }

    fn best_pass(self) -> Pass {
        match self {
            Self::Scoped => Pass::Strict,
            Self::Open => Pass::Lenient,
        }
    }

    fn searches(self, plan: &QueryPlan) -> bool {
        match self {
            Self::Scoped => true,
            Self::Open => plan.context.iter().any(|part| !part.is_postcode()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Evidence {
    edits: u8,
    pass: Pass,
    confirmed: bool,
    score: f32,
}

impl Evidence {
    fn key(self) -> (u8, Pass, bool) {
        (self.edits, self.pass, !self.confirmed)
    }

    fn outweighs(self, other: Self) -> bool {
        self.key() < other.key() || (self.key() == other.key() && self.score > other.score)
    }

    fn open(self) -> bool {
        self.edits > 0 || self.pass == Pass::Lenient || !self.confirmed
    }

    fn beaten_by(self, pass: Pass) -> bool {
        self.edits > 0 || pass < self.pass || (pass == self.pass && !self.confirmed)
    }
}

#[derive(Debug, Clone)]
struct Reading {
    text: String,
    plan: QueryPlan,
    score: f32,
    pieces: Vec<Tagged>,
}

type Readings = (
    Vec<QueryPlan>,
    Vec<f32>,
    Vec<Vec<Tagged>>,
    Option<Explanation>,
    usize,
);

fn filter_by<T>(items: Vec<T>, keep: impl Fn(usize) -> bool) -> Vec<T> {
    items
        .into_iter()
        .enumerate()
        .filter(|(index, _)| keep(*index))
        .map(|(_, item)| item)
        .collect()
}

const MAX_SEARCHES: usize = 16;

#[cfg(test)]
thread_local! {
    static LAST_STRUCTURED: Cell<bool> = const { Cell::new(false) };
}

const STRUCTURED_CANDIDATES: usize = 4_000;

fn segment_docs(docs: &[DocId], base: DocId, end: DocId) -> Vec<DocId> {
    let from = docs.partition_point(|doc| *doc < base);
    let to = docs.partition_point(|doc| *doc < end);
    docs[from..to].iter().map(|doc| doc - base).collect()
}

fn clean_query(query: &str) -> String {
    let query = query.replace('\u{fffd}', "");
    let mut words = Vec::<String>::new();
    for word in query.split_whitespace() {
        let characters = word.chars().collect::<Vec<_>>();
        let mut piece = String::new();
        for (at, &character) in characters.iter().enumerate() {
            if at > 0 {
                let before = characters[at - 1];
                let run_before = characters[..at]
                    .iter()
                    .rev()
                    .take_while(|character| character.is_alphabetic())
                    .count();
                let run_after = characters[at..]
                    .iter()
                    .take_while(|character| character.is_alphabetic())
                    .count();
                let lower_before = characters[..at]
                    .iter()
                    .rev()
                    .take_while(|character| character.is_lowercase())
                    .count();
                let spaced = |character: char| {
                    use unicode_script::{Script, UnicodeScript};
                    !lattice::is_unspaced(character) && character.script() != Script::Hangul
                };
                let split = (before.is_alphabetic()
                    && spaced(before)
                    && character.is_ascii_digit()
                    && run_before >= 3)
                    || (before.is_ascii_digit()
                        && character.is_alphabetic()
                        && spaced(character)
                        && run_after >= 4)
                    || (before.is_lowercase() && character.is_uppercase() && lower_before >= 4);
                if split && !piece.is_empty() {
                    words.push(std::mem::take(&mut piece));
                }
            }
            piece.push(character);
        }
        if !piece.is_empty() {
            words.push(piece);
        }
    }
    let key = |word: &String| {
        word.chars()
            .filter(|character| character.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let mut at = 0;
    while at < words.len() {
        let longest = (words.len() - at) / 2;
        let repeated = (2..=longest).rev().find(|&len| {
            (0..len).all(|offset| {
                let word = key(&words[at + offset]);
                !word.is_empty() && word == key(&words[at + len + offset])
            })
        });
        match repeated {
            Some(len) => {
                words.drain(at + len..at + 2 * len);
            }
            None => at += 1,
        }
    }
    words.join(" ")
}

const PLACE_FLOOR: f32 = 0.1;
const AREA_ANSWERS: usize = 8;
const STRUCTURED_READINGS: usize = 5;
const STRUCTURED_SPOTS: usize = 8;
const CHOOSER_HITS: usize = 10;
const SPOT_POINTS: usize = 256;
const STREET_DISK_M: f64 = 100.0;
const STRUCTURED_W: f32 = 8.00;
const STRUCTURED_MIN_FORM: u8 = 4;
const STRUCTURED_DELTA: f32 = -0.02;
const FEATURES: usize = 19;
const MARGIN_CAP: f32 = 30.0;
const P_STRUCTURED: [f32; FEATURES + 1] = [
    -1.09688,
    1.46787,
    0.667334,
    -0.0810829,
    0.370718,
    0.256462,
    -0.274775,
    -3.07266,
    -3.1685,
    -6.63367,
    0.00502387,
    -1.11694,
    -0.103215,
    0.0490169,
    0.0940121,
    -1.193,
    0.0575686,
    -0.0909365,
    -0.110694,
    -0.000170834,
];
const P_TEXT: [f32; FEATURES + 1] = [
    -0.171209, 0.889543, 0.330719, -0.515181, 0.00348658, -0.398113, -0.715066, -1.04156, -1.86512,
    -1.13006, -0.412767, -1.01375, -0.260571, 0.0460241, 0.0421718, -0.324983, -0.0255055,
    -0.0191589, -0.0539111, 0.0151013,
];
const EXTENT_READ: usize = 4_000;
const MATCH_CELLS: usize = 1_024;
const MATCHED_STREETS: usize = 16;
const SPOT_M: f64 = 200.0;
const FUZZY_PLACE_CHARS: usize = 4;
const STREET_CELLS: usize = 64;

struct StructuredContext<'a, 'b> {
    structured: &'a StructuredIndex,
    readings: &'a [Reading],
    parsed: usize,
    search: &'a Search<'b>,
    session: &'a Session,
    explanation: Option<&'a Explanation>,
    probabilities: Vec<f32>,
}

struct Declined {
    text: String,
    plan: QueryPlan,
    hits: Vec<TextSearchHit>,
}

struct StructuredFound {
    hit: TextSearchHit,
    point: [f64; 2],
    support: usize,
    form: u8,
    reading: usize,
    entry: &'static str,
    lever: u8,
}

struct Keyed {
    order: (usize, u8, bool, u8),
    doc: DocId,
    base: bool,
    address: bool,
    point: [f64; 2],
}

struct Spot {
    head: Keyed,
    docs: Vec<DocId>,
    tied: Vec<DocId>,
    points: Vec<[f64; 2]>,
}

struct Scored {
    gap: f32,
    score: f32,
    found: StructuredFound,
}

struct StructuredExplained {
    scored: Vec<Scored>,
    second: f32,
    spots: usize,
}

fn structured_layer(layer: Layer) -> u8 {
    match layer {
        Layer::Address => structured::LAYER_ADDRESS,
        Layer::Interpolation => structured::LAYER_INTERPOLATION,
        Layer::Street => structured::LAYER_STREET,
        Layer::Poi => structured::LAYER_POI,
        Layer::Country
        | Layer::Region
        | Layer::District
        | Layer::Locality
        | Layer::Neighbourhood
        | Layer::Place
        | Layer::Postcode => 0,
    }
}

fn extent_middle([west, south, east, north]: [i32; 4]) -> [i32; 2] {
    [
        ((i64::from(west) + i64::from(east)) / 2) as i32,
        ((i64::from(south) + i64::from(north)) / 2) as i32,
    ]
}

const FORM_EXACT: u8 = 8;
const FORM_RANGE_START: u8 = 7;
const FORM_RANGE: u8 = 6;
const FORM_INTERPOLATION: u8 = 5;
const FORM_NEIGHBOUR: u8 = 4;
const FORM_STREET: u8 = 3;
const FORM_POSTCODE: u8 = 2;
const FORM_TOWN: u8 = 1;
const FORM_REGION: u8 = 0;

const LEVER_A: u8 = 1;
const LEVER_C: u8 = 4;
const LEVER_D: u8 = 8;
const LEVER_F: u8 = 16;
const LEVER_G: u8 = 32;
const LEVER_T: u8 = 64;

#[allow(clippy::too_many_arguments)]
fn structured_features(
    form: u8,
    lever: u8,
    gap: f32,
    margin: f32,
    support: usize,
    reading: usize,
    spots: usize,
    candidates: usize,
) -> [f32; FEATURES] {
    let flag = |on: bool| f32::from(u8::from(on));
    [
        flag(form == FORM_EXACT),
        flag(form == FORM_RANGE_START),
        flag(form == FORM_RANGE),
        flag(form == FORM_INTERPOLATION),
        flag(form == FORM_NEIGHBOUR),
        flag(form == FORM_STREET),
        flag(form == FORM_POSTCODE),
        flag(form == FORM_TOWN),
        flag(form == FORM_REGION),
        flag(lever & LEVER_A != 0),
        flag(lever & LEVER_C != 0),
        flag(lever & LEVER_D != 0),
        gap.clamp(-50.0, 10.0),
        margin.min(MARGIN_CAP),
        flag(margin.is_infinite()),
        (support as f32 / 1000.0).min(8.0),
        spots.min(8) as f32,
        reading as f32,
        (1.0 + candidates as f32).ln(),
    ]
}

fn logistic(weights: &[f32; FEATURES + 1], features: &[f32; FEATURES]) -> f32 {
    let z = weights[0]
        + weights[1..]
            .iter()
            .zip(features)
            .map(|(weight, feature)| weight * feature)
            .sum::<f32>();
    1.0 / (1.0 + (-z).exp())
}

fn street_at(record: &mut Summary, [lon, lat]: [f64; 2]) {
    record.point = Some(Point {
        lon,
        lat,
        precision: PointPrecision::Estimated,
    });
}

fn held_to_places(plan: &QueryPlan, lower: &mut Vec<Neighbour>, higher: &mut Vec<Neighbour>) {
    let named = choose::Named::of(plan);
    for side in [lower, higher] {
        side.retain(|neighbour| {
            named.holds(
                neighbour.address.postcode.as_deref(),
                neighbour.address.locality.as_deref(),
            )
        });
    }
}

fn postcode_letters(postcode: &str) -> String {
    postcode
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_uppercase())
        .collect()
}

fn densest(points: &[[f64; 2]]) -> Option<[f64; 2]> {
    let near = |at: &[f64; 2]| {
        points
            .iter()
            .filter(|other| haversine_m(at[0], at[1], other[0], other[1]) <= STREET_DISK_M)
            .collect::<Vec<_>>()
    };
    let centre = points.iter().max_by_key(|at| near(at).len())?;
    let near = near(centre);
    let count = near.len() as f64;
    Some([
        near.iter().map(|point| point[0]).sum::<f64>() / count,
        near.iter().map(|point| point[1]).sum::<f64>() / count,
    ])
}

fn street_words_match(
    query: &[&str],
    name: &str,
    describes_street: impl Fn(&str) -> bool,
) -> Option<usize> {
    let words = name.split_whitespace().collect::<Vec<_>>();
    let mut used = vec![false; words.len()];
    let mut letters = 0;
    for word in query {
        if describes_street(word) {
            continue;
        }
        let chars = word.chars().count();
        let digit = word.chars().any(|character| character.is_ascii_digit());
        let names = |other: &str| {
            other == *word
                || (chars >= 4 && typo_distance(word, other).is_some())
                || ((2..=4).contains(&chars) && !digit && abbreviates(word, other))
        };
        match (0..words.len()).find(|&at| !used[at] && names(words[at])) {
            Some(at) => {
                used[at] = true;
                letters += chars;
            }
            None if chars <= 3 && !digit => {}
            None => return None,
        }
    }
    (letters >= 4).then_some(letters)
}

fn street_named_in(
    query: &[&str],
    name: &str,
    describes_street: impl Fn(&str) -> bool,
    places: &[String],
) -> Option<usize> {
    let mut used = vec![false; query.len()];
    let mut letters = 0;
    let mut own = false;
    for word in name.split_whitespace() {
        if describes_street(word) {
            continue;
        }
        let chars = word.chars().count();
        let names = |written: &str| {
            written == word
                || (chars >= 4 && typo_distance(written, word).is_some())
                || ((2..=4).contains(&written.chars().count()) && abbreviates(written, word))
        };
        match (0..query.len()).find(|&at| !used[at] && names(query[at])) {
            Some(at) => {
                used[at] = true;
                letters += chars;
                own |= !places.iter().any(|place| place.starts_with(word));
            }
            None if chars <= 3 => {}
            None => return None,
        }
    }
    let first = (0..query.len()).find(|&at| !describes_street(query[at]));
    (own && letters >= 4 && first.is_some_and(|at| used[at])).then_some(letters)
}

fn context_words(plan: &QueryPlan) -> Vec<String> {
    plan.context
        .iter()
        .flat_map(|part| part.words.iter().cloned())
        .collect()
}

fn street_ordinal_number(word: &str) -> Option<&str> {
    ["st", "nd", "rd", "th"].into_iter().find_map(|suffix| {
        let number = word.strip_suffix(suffix)?;
        (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())).then_some(number)
    })
}

fn area_initials(key: &str) -> bool {
    (2..=4).contains(&key.len()) && key.bytes().all(|byte| byte.is_ascii_alphabetic())
}

fn types_word(typed: &str, word: &str, last: bool) -> bool {
    typed == word
        || (last && word.starts_with(typed))
        || stands_for(typed, &lattice::forms_both_ways(typed), word)
}

fn stands_for(typed: &str, forms: &[String], word: &str) -> bool {
    (contraction(typed) && abbreviates(typed, word)) || forms.iter().any(|form| form == word)
}

fn contraction(typed: &str) -> bool {
    (2..=4).contains(&typed.chars().count())
        && !typed
            .chars()
            .skip(1)
            .any(|character| character.is_ascii_digit() || "aeiou".contains(character))
}

fn unlisted_contraction(typed: &str) -> bool {
    contraction(typed) && lattice::forms_both_ways(typed).is_empty()
}

fn abbreviates(short: &str, long: &str) -> bool {
    if long.chars().count() <= short.chars().count() || short.chars().next() != long.chars().next()
    {
        return false;
    }
    let mut rest = long.chars();
    short
        .chars()
        .all(|wanted| rest.by_ref().any(|character| character == wanted))
}

#[derive(Debug, Default)]
struct PlaceWords {
    words: Vec<Vec<Place>>,
    digits: Vec<bool>,
    skipped: usize,
    fuzzy: usize,
    countries: Vec<String>,
    log: String,
    weights: Vec<usize>,
}

struct Checks {
    places: StatedPlaces,
    stated: Vec<StatedArea>,
    loose: Vec<String>,
    countries: Vec<String>,
}

impl Checks {
    fn filters<'a>(&'a self, filters: &Filters<'a>) -> Filters<'a> {
        match filters.countries.is_empty() {
            true => Filters {
                countries: &self.countries,
                ..*filters
            },
            false => *filters,
        }
    }
}

const MAX_EXPLAINED: usize = 64;

struct Session {
    deadline: Cell<Instant>,
    searches: Cell<usize>,
    areas: RefCell<HashMap<RecordId, Rc<RecordAreas>>>,
    settlements: RefCell<HashMap<RecordId, Rc<Vec<String>>>>,
    regions: RefCell<HashMap<RecordId, Rc<Vec<RecordId>>>>,
}

impl Session {
    fn new(budget: Duration) -> Self {
        Self {
            deadline: Cell::new(Instant::now() + budget),
            searches: Cell::new(0),
            areas: RefCell::default(),
            settlements: RefCell::default(),
            regions: RefCell::default(),
        }
    }

    fn extend(&self, by: Duration) {
        self.deadline.set(self.deadline.get() + by);
    }

    fn allows_search(&self) -> bool {
        let searches = self.searches.get();
        if searches > 0 && (searches >= MAX_SEARCHES || Instant::now() >= self.deadline.get()) {
            return false;
        }
        self.searches.set(searches + 1);
        true
    }
}

#[derive(Debug, Default)]
struct Pool {
    candidates: Vec<Candidate>,
}

#[derive(Debug)]
struct Candidate {
    hit: TextSearchHit,
    reading: usize,
    confirmed: bool,
    contradicted: bool,
}

impl Candidate {
    fn evidence(&self, readings: &[Reading]) -> Evidence {
        Evidence {
            edits: self.hit.features.edits,
            pass: self.hit.features.pass,
            confirmed: self.confirmed,
            score: readings[self.reading].score,
        }
    }
}

impl Pool {
    fn weigh(&mut self, readings: &[Reading]) {
        let best = readings.first().map_or(0.0, |reading| reading.score);
        let top = self
            .candidates
            .iter()
            .map(|candidate| candidate.hit.score)
            .fold(0.0, Score::max);
        for candidate in &mut self.candidates {
            let signals = &mut candidate.hit.signals;
            signals.parse_gap = readings[candidate.reading].score - best;
            signals.text_share = match top > 0.0 {
                true => candidate.hit.score / top,
                false => 0.0,
            };
        }
    }

    fn push(
        &mut self,
        reading: usize,
        mut hit: TextSearchHit,
        confirmed: bool,
        contradicted: bool,
    ) {
        hit.signals.reading = reading.min(usize::from(u8::MAX)) as u8;
        hit.signals.confirmed = confirmed;
        hit.signals.contradicted = contradicted;
        self.candidates.push(Candidate {
            hit,
            reading,
            confirmed,
            contradicted,
        });
    }

    fn answers(&self) -> bool {
        self.candidates
            .iter()
            .any(|candidate| !candidate.contradicted)
    }

    fn chosen(self, readings: &[Reading], limit: usize) -> Option<(String, Vec<TextSearchHit>)> {
        let best = self
            .candidates
            .iter()
            .filter(|candidate| !candidate.contradicted)
            .reduce(|held, candidate| {
                match candidate
                    .evidence(readings)
                    .outweighs(held.evidence(readings))
                {
                    true => candidate,
                    false => held,
                }
            });
        let (reading, level) = match best {
            Some(best) => (
                best.reading,
                Some((best.hit.features.edits, best.hit.features.pass)),
            ),
            None => (self.candidates.first()?.reading, None),
        };
        let mut hits = Vec::<TextSearchHit>::new();
        let add = |hit: TextSearchHit, hits: &mut Vec<TextSearchHit>| {
            if hits.len() < limit && !hits.iter().any(|held| held.record_id == hit.record_id) {
                hits.push(hit);
            }
        };
        let (answers, contradicting): (Vec<_>, Vec<_>) = self
            .candidates
            .into_iter()
            .partition(|candidate| !candidate.contradicted);
        if let Some((edits, pass)) = level {
            let mut chosen = answers
                .into_iter()
                .filter(|candidate| {
                    candidate.reading == reading && candidate.hit.features.pass == pass
                })
                .collect::<Vec<_>>();
            chosen.sort_by_key(|candidate| candidate.hit.features.edits.max(edits));
            for candidate in chosen {
                add(candidate.hit, &mut hits);
            }
        }
        for candidate in contradicting {
            if level.is_none() || candidate.reading <= reading {
                add(candidate.hit, &mut hits);
            }
        }
        Some((readings[reading].text.clone(), hits))
    }

    fn ranked(
        self,
        ranker: &Ranker,
        readings: &[Reading],
        limit: usize,
    ) -> Option<(String, Vec<TextSearchHit>)> {
        let mut scored = self
            .candidates
            .into_iter()
            .map(|candidate| (ranker.score(&candidate.hit), candidate))
            .collect::<Vec<_>>();
        scored.sort_by(|(left, _), (right, _)| right.total_cmp(left));
        let reading = scored.first()?.1.reading;
        let mut hits = Vec::<TextSearchHit>::new();
        for (score, candidate) in scored {
            if hits.len() >= limit {
                break;
            }
            if !hits
                .iter()
                .any(|held| held.record_id == candidate.hit.record_id)
            {
                let mut hit = candidate.hit;
                hit.signals.ranked = score;
                hits.push(hit);
            }
        }
        Some((readings[reading].text.clone(), hits))
    }
}

fn country_not_town(
    mut pieces: Vec<Tagged>,
    written: Option<(String, Range<usize>)>,
    parsed: Option<CountryCode>,
) -> Vec<Tagged> {
    if let Some((country, at)) = written
        && parsed.is_some_and(|parsed| parsed.as_str().eq_ignore_ascii_case(&country))
        && pieces[at.clone()]
            .iter()
            .all(|piece| piece.label == Label::Locality)
    {
        for piece in &mut pieces[at] {
            piece.label = Label::Country;
        }
    }
    pieces
}

fn allowed_edits(word: &str) -> Option<u8> {
    let chars = word.chars().count();
    if chars < MIN_TYPO_CHARS || word.chars().any(char::is_numeric) {
        return None;
    }
    let base = if chars < LONG_TYPO_CHARS { 1 } else { 2 };
    Some((base + replaced_characters(word)).min(2) as u8)
}

fn typo_distance(word: &str, other: &str) -> Option<u8> {
    if !other.starts_with(first_chars(word)) {
        return None;
    }
    thread_local! {
        static AUTOMATA: RefCell<TypoAutomata> = RefCell::default();
    }
    let dfa = AUTOMATA.with(|cache| {
        let TypoAutomata { automata, states } = &mut *cache.borrow_mut();
        if let Some(dfa) = automata.get(word) {
            return dfa.clone();
        }
        let dfa = allowed_edits(word).map(|most| Rc::new(automaton_builder(most).build_dfa(word)));
        let added = dfa.as_ref().map_or(0, |dfa| dfa.num_states());
        if automata.len() >= MAX_CACHED_AUTOMATA || *states + added > MAX_CACHED_AUTOMATON_STATES {
            automata.clear();
            *states = 0;
        }
        automata.insert(word.to_string(), dfa.clone());
        *states += added;
        dfa
    })?;
    match dfa.eval(other) {
        Distance::Exact(distance) => Some(distance),
        Distance::AtLeast(_) => None,
    }
}

#[derive(Default)]
struct TypoAutomata {
    automata: HashMap<String, Option<Rc<DFA>>>,
    states: usize,
}

const MAX_CACHED_AUTOMATA: usize = 4_096;

const MAX_CACHED_AUTOMATON_STATES: usize = 16 * 1024;

fn first_chars(word: &str) -> &str {
    if word.starts_with(char::REPLACEMENT_CHARACTER) {
        return "";
    }
    let end = word
        .char_indices()
        .nth(TYPO_PREFIX_CHARS)
        .map_or(word.len(), |(at, _)| at);
    &word[..end]
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Spellings {
    words: HashMap<(Field, String), Spelt>,
}

#[derive(Debug, Default)]
struct AreaWords {
    known: HashSet<String>,
    postcodes: PostcodeWeights,
    stand_ins: HashMap<String, Score>,
}

#[derive(Debug, Default)]
struct PostcodeWeights {
    terms: HashMap<String, Vec<(Option<CountryCode>, Score)>>,
}

impl PostcodeWeights {
    fn agreement(covered: u64, addresses: u64) -> Score {
        let covered = covered.max(1) as f64;
        let addresses = (addresses as f64).max(covered);
        (MATCH_AGREEMENT * addresses / covered).ln().max(0.0) as Score
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Spelt {
    corrected: Option<String>,
    near: Vec<String>,
}

struct Levenshtein<'a>(&'a DFA);

impl tantivy_fst::Automaton for Levenshtein<'_> {
    type State = u32;

    fn start(&self) -> u32 {
        self.0.initial_state()
    }

    fn is_match(&self, state: &u32) -> bool {
        matches!(self.0.distance(*state), Distance::Exact(_))
    }

    fn can_match(&self, state: &u32) -> bool {
        *state != SINK_STATE
    }

    fn accept(&self, state: &u32, byte: u8) -> u32 {
        self.0.transition(*state, byte)
    }
}

#[derive(Debug, Clone, Default)]
struct Ranking {
    query_words: QueryWords,
    numbered: bool,
    house_number: Option<u32>,
    number_texts: Vec<String>,
    abbreviated_numbers: Vec<String>,
    flat_text: Option<String>,
    unit: Option<String>,
    postcode: QueryPostcode,
    focus: Option<[f64; 2]>,
    near_first: bool,
    fame_first: bool,
    typed_words: u8,
    whole_last: Option<Term>,
    sought: Vec<Word>,
}

impl Ranking {
    fn states_number(&self, stated: &str) -> bool {
        self.number_texts.iter().any(|text| text == stated)
            || self.abbreviated_numbers.iter().any(|text| text == stated)
            || self.flat_text.as_deref() == Some(stated)
    }
}

#[derive(Debug, Clone, Default)]
struct QueryWords {
    search: HashSet<String>,
    written: String,
    exact: HashSet<String>,
}

impl QueryWords {
    fn new(query: &str) -> Self {
        let words = |key: Option<String>| {
            key.iter()
                .flat_map(|text| text.split_whitespace())
                .map(str::to_string)
                .collect()
        };
        let written = search_key(query).unwrap_or_default();
        Self {
            search: written.split_whitespace().map(str::to_string).collect(),
            written,
            exact: words(exact_key(query)),
        }
    }
}

fn tiger_street_written(query: &str, number: u32, street: &str) -> bool {
    let mut words = query.split_whitespace();
    if !words.any(|word| word.parse::<u32>() == Ok(number)) {
        return false;
    }
    let mut name = String::new();
    let max = street.split_whitespace().count() + 2;
    words.take(max).any(|word| {
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(word);
        road::same_road(&name, street)
    })
}

#[derive(Debug, Clone, Default)]
struct QueryPostcode {
    explicit: bool,
    forms: Vec<String>,
    exact: String,
    areas: Vec<PostcodeArea>,
}

#[derive(Debug, Clone)]
struct PostcodeArea {
    points: Vec<[f64; 2]>,
    reach_m: f64,
}

impl PostcodeArea {
    fn new(points: Vec<[f64; 2]>, neighbour_m: f64) -> Self {
        let count = points.len() as f64;
        let lon = points.iter().map(|[lon, _]| lon).sum::<f64>() / count;
        let lat = points.iter().map(|[_, lat]| lat).sum::<f64>() / count;
        let reach_m = points
            .iter()
            .map(|[point_lon, point_lat]| haversine_m(lon, lat, *point_lon, *point_lat))
            .fold(neighbour_m.max(POSTCODE_AREA_REACH_FLOOR_M), f64::max);
        Self { points, reach_m }
    }

    fn distance_m(&self, lon: f64, lat: f64) -> f64 {
        self.points
            .iter()
            .map(|[point_lon, point_lat]| haversine_m(lon, lat, *point_lon, *point_lat))
            .fold(f64::INFINITY, f64::min)
    }
}

const POSTCODE_HELD: u8 = u8::MAX;
const POSTCODE_UNKNOWN: u8 = 128;

impl QueryPostcode {
    fn of(words: &[String]) -> Self {
        let first_digit = words
            .iter()
            .position(|word| word.chars().any(|character| character.is_ascii_digit()))
            .unwrap_or_default();
        Self {
            forms: unique_strings(vec![words.concat(), words[first_digit..].concat()]),
            exact: words.join(" "),
            areas: Vec::new(),
            explicit: false,
        }
    }

    fn holds(&self, postcode: &str) -> bool {
        self.forms
            .iter()
            .any(|form| postcode.starts_with(form.as_str()) || form.starts_with(postcode))
    }

    fn rank(
        own: Option<(bool, u8)>,
        area: Option<(bool, u8)>,
        band: impl FnOnce() -> Option<u8>,
    ) -> u8 {
        let farthest_band = POSTCODE_HELD - 2 - (POSTCODE_NEARNESS_BANDS_M.len() - 1) as u8;
        match (own, area) {
            (Some((true, _)), _) => POSTCODE_HELD,
            (None, Some((true, _))) => POSTCODE_HELD - 1,
            (own, area) => match (band(), own) {
                (Some(band), _) => POSTCODE_HELD - 2 - band,
                (None, Some((_, shared))) => shared.min(POSTCODE_UNKNOWN - 1),
                (None, None) => POSTCODE_UNKNOWN
                    .saturating_add(area.map_or(0, |(_, shared)| shared))
                    .min(farthest_band - 1),
            },
        }
    }

    fn rank_of(&self, own: &[String], area: Option<&str>) -> u8 {
        let read = |postcode: &str| (self.holds(postcode), self.agreement(postcode));
        let own = own.iter().map(|postcode| read(postcode)).reduce(
            |(left_held, left), (right_held, right)| (left_held || right_held, left.max(right)),
        );
        Self::rank(own, area.map(read), || None)
    }

    fn distance_m(&self, lon: f64, lat: f64) -> Option<f64> {
        self.areas
            .iter()
            .map(|area| area.distance_m(lon, lat))
            .min_by(f64::total_cmp)
    }

    fn placement(&self, lon: f64, lat: f64) -> Option<bool> {
        let reaches = self
            .areas
            .iter()
            .map(|area| area.distance_m(lon, lat) / area.reach_m)
            .min_by(f64::total_cmp)?;
        if reaches <= 1.0 {
            Some(true)
        } else if reaches > POSTCODE_CONTRADICTING_REACHES {
            Some(false)
        } else {
            None
        }
    }

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

    fn ord_ranges(&self, column: &StrColumn) -> io::Result<Vec<Vec<Range<u64>>>> {
        let dictionary = column.dictionary();
        self.forms
            .iter()
            .map(|form| {
                form.char_indices()
                    .map(|(index, character)| {
                        let prefix = &form[..index + character.len_utf8()];
                        let (start, end) = dictionary.term_bounds_to_ord(
                            Bound::Included(prefix.as_bytes().to_vec()),
                            Bound::Excluded(utf8_prefix_end(prefix)),
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

fn utf8_prefix_end(prefix: &str) -> Vec<u8> {
    // UTF-8 never uses 0xFF, so every term with the prefix sorts below it.
    let mut end = prefix.as_bytes().to_vec();
    end.push(u8::MAX);
    end
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

pub(crate) fn effective_limit(limit: usize) -> usize {
    if limit == 0 {
        DEFAULT_SEARCH_LIMIT
    } else {
        limit
    }
}

#[derive(Debug, Default)]
pub struct Caller {
    pub country: Option<CountryCode>,
    pub areas: Vec<RecordId>,
}

pub fn rank_places(
    searcher: &TextSearcher,
    pois: Option<&TextSearcher>,
    options: &TextAutocompleteOptions,
    hits: Vec<TextSearchHit>,
    caller: &Caller,
    read: bool,
) -> Result<Vec<TextSearchHit>> {
    if !reach::ranks(options) {
        return Ok(hits);
    }
    let mut famous = searcher.famous_places(options, !read)?;
    if let Some(pois) = pois {
        famous.extend(pois.famous_places(options, !read)?);
    }
    reach::rank(
        searcher,
        options,
        hits,
        famous,
        caller,
        effective_autocomplete_limit(options.limit),
    )
}

pub fn suggest(
    addresses: &TextSearcher,
    places: &TextSearcher,
    options: &TextAutocompleteOptions,
) -> Result<Vec<TextSearchHit>> {
    if options.layer.as_deref() == Some(Layer::Poi.as_str()) {
        return places.autocomplete(options);
    }
    if addresses.numbered_completion_focus(options).is_some() {
        return addresses.autocomplete(options);
    }
    let directional = addresses.qualified_directional_completion(options)?;
    if !directional.is_empty() {
        return Ok(directional);
    }
    let tier = |completion, only_near: bool| -> Result<Vec<TextSearchHit>> {
        let mut addresses = addresses.autocomplete_as(options, completion)?;
        let mut places = places.autocomplete_as(options, completion)?;
        if let (true, Some(focus)) = (only_near, options.focus) {
            addresses.retain(|hit| within_bias(hit, focus));
            places.retain(|hit| within_bias(hit, focus));
        }
        Ok(mix(
            &options.query,
            addresses,
            places,
            options.limit,
            options.focus.is_some(),
            completion == Completion::Typos,
        ))
    };
    for completion in Completion::ALL {
        let mixed = tier(completion, false)?;
        if mixed.is_empty() {
            continue;
        }
        if mixed.iter().all(|hit| hit.fallback) {
            let houses = addresses.complete_address(options)?;
            if !houses.is_empty() {
                let mut hits = addresses.with_streets(houses, mixed);
                hits.truncate(effective_autocomplete_limit(options.limit));
                return Ok(hits);
            }
        }
        if let Some(focus) = options.focus
            && !mixed.iter().any(|hit| within_bias(hit, focus))
        {
            if let Some(local) = addresses
                .autocomplete_address(options, true)?
                .into_iter()
                .find(|hit| within_bias(hit, focus))
            {
                let mut hits = vec![local];
                hits.extend(
                    mixed
                        .into_iter()
                        .take(effective_autocomplete_limit(options.limit) - 1),
                );
                return Ok(hits);
            }
            if completion == Completion::Typos || !last_word_may_be_a_typo(&options.query) {
                return Ok(mixed);
            }
            let mut typos = tier(Completion::Typos, true)?;
            if !typos.is_empty() {
                for hit in mixed {
                    if !typos.iter().any(|kept| kept.record_id == hit.record_id) {
                        typos.push(hit);
                    }
                }
                typos.truncate(effective_autocomplete_limit(options.limit));
                return Ok(typos);
            }
        }
        return Ok(mixed);
    }
    if let Some(normalized) = autocomplete_country_alias(options) {
        return suggest(addresses, places, &normalized);
    }
    addresses.complete_address(options)
}

fn autocomplete_country_alias(
    options: &TextAutocompleteOptions,
) -> Option<TextAutocompleteOptions> {
    let (address, suffix) = options.query.rsplit_once(',')?;
    let country = crate::iso::alpha2_of(suffix.trim())?;
    let number = address.split_whitespace().next()?;
    if !is_numeric_token(number)
        || address.split_whitespace().take(3).count() < 3
        || (!options.countries.is_empty()
            && !options
                .countries
                .iter()
                .any(|asked| asked.eq_ignore_ascii_case(country)))
    {
        return None;
    }
    Some(TextAutocompleteOptions {
        query: format!("{}, {country}", address.trim_end()),
        countries: vec![country.to_string()],
        ..options.clone()
    })
}

pub(crate) fn category_typo(query: &str) -> Option<Vec<&'static str>> {
    let word = search_key(query.trim())?;
    if word.contains(' ') {
        return None;
    }
    allowed_edits(&word)?;
    let first = first_chars(&word);
    let mut fewest = None;
    let mut found = Vec::new();
    for candidate in crate::categories::strong_words().filter(|key| key.starts_with(first)) {
        let Some(edits) = typo_distance(&word, candidate) else {
            continue;
        };
        if fewest.is_none_or(|fewest| edits < fewest) {
            fewest = Some(edits);
            found.clear();
        }
        if fewest == Some(edits) {
            found.push(candidate);
        }
    }
    let categories = found
        .into_iter()
        .filter_map(crate::categories::of_query)
        .flatten()
        .collect::<std::collections::BTreeSet<_>>();
    (!categories.is_empty()).then(|| categories.into_iter().collect())
}

fn last_word_may_be_a_typo(query: &str) -> bool {
    search_key(query.trim()).is_some_and(|key| {
        key.split_whitespace()
            .last()
            .is_some_and(|last| allowed_edits(last).is_some())
    })
}

pub fn mix_suggestions(
    query: &str,
    addresses: Vec<TextSearchHit>,
    places: Vec<TextSearchHit>,
    limit: usize,
) -> Vec<TextSearchHit> {
    mix(query, addresses, places, limit, false, false)
}

fn mix(
    query: &str,
    mut addresses: Vec<TextSearchHit>,
    places: Vec<TextSearchHit>,
    limit: usize,
    near_first: bool,
    typos: bool,
) -> Vec<TextSearchHit> {
    let limit = effective_autocomplete_limit(limit);
    let key = search_key(query.trim()).unwrap_or_default();
    let tokens = autocomplete_query_tokens(&key);
    let numbered = autocomplete_number(&tokens).is_some();
    let mut mixed = Vec::with_capacity(addresses.len() + places.len());
    if numbered {
        mixed.extend(addresses);
        mixed.extend(places);
    } else {
        if near_first {
            let (whole, rest): (Vec<_>, Vec<_>) = addresses.into_iter().partition(|hit| {
                hit.named_place
                    && search_key(&hit.record.name).is_some_and(|name| {
                        if usize::from(hit.signals.focus_band) < BIAS_BANDS {
                            name.split_whitespace().count() == tokens.len()
                                && name_starts_as(&hit.record.name, &tokens, typos)
                        } else {
                            name == tokens.join(" ")
                        }
                    })
            });
            mixed.extend(whole);
            addresses = rest;
        }
        let (named, others): (Vec<_>, Vec<_>) = places
            .into_iter()
            .partition(|hit| name_starts_as(&hit.record.name, &tokens, typos));
        let mut rest = named;
        rest.extend(addresses);
        rest.extend(others);
        if near_first {
            let (near, far): (Vec<_>, Vec<_>) = rest
                .into_iter()
                .partition(|hit| usize::from(hit.signals.focus_band) < BIAS_BANDS);
            rest = near;
            rest.extend(far);
        }
        mixed.extend(rest);
    }
    mixed.truncate(limit);
    mixed
}

pub(crate) fn name_words(name: &str) -> Option<String> {
    search_key(name).map(|key| autocomplete_query_tokens(&key).join(" "))
}

pub(crate) fn name_starts_as(name: &str, words: &[String], typos: bool) -> bool {
    let Some(name) = name_words(name) else {
        return false;
    };
    let name = name.split_whitespace().collect::<Vec<_>>();
    !words.is_empty()
        && words.len() <= name.len()
        && words
            .iter()
            .zip(&name)
            .enumerate()
            .all(|(at, (typed, word))| {
                let last = at + 1 == words.len();
                let cut = match last {
                    true => word.chars().take(typed.chars().count()).collect::<String>(),
                    false => word.to_string(),
                };
                types_word(typed, word, last)
                    || (typos
                        && (typo_distance(typed, word).is_some()
                            || typo_distance(typed, &cut).is_some()))
            })
}

pub(crate) fn autocomplete_tokens(query: &str) -> Vec<String> {
    search_key(query.trim()).map_or_else(Vec::new, |key| autocomplete_query_tokens(&key))
}

pub(crate) fn typed_highlights(query: &str, label: &str) -> Vec<Highlight> {
    let words = autocomplete_tokens(query);
    highlight::Highlighter::new(Completion::AllWords, None, &words, true).ranges(label)
}

fn typed_width(text: &str) -> usize {
    use unicode_script::{Script, UnicodeScript};
    text.chars()
        .filter(|character| !character.is_whitespace())
        .map(|character| match character.script() {
            Script::Han | Script::Hiragana | Script::Katakana | Script::Hangul => 2,
            _ => 1,
        })
        .sum()
}

pub(crate) fn effective_autocomplete_limit(limit: usize) -> usize {
    let limit = effective_limit(limit);
    limit.min(MAX_AUTOCOMPLETE_LIMIT)
}

#[derive(Debug, Clone, Default, PartialEq)]
struct QueryPlan {
    explicit_postcode: bool,
    house_number: Option<HouseNumber>,
    unit: Option<String>,
    unit_words: Vec<String>,
    subject: Vec<SubjectWord>,
    unplaced: Vec<Word>,
    optional: Vec<String>,
    context: Vec<ContextPart>,
    spellings: Arc<Spellings>,
    number_kept: bool,
    subject_part: SubjectPart,
}

#[derive(Debug, Clone, Copy, Default)]
struct SubjectPart(Option<Label>, Split);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Split(bool);

impl PartialEq for SubjectPart {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Word {
    text: String,
    corrected: Option<String>,
}

impl Word {
    fn paired(written: Vec<String>, corrected: Option<Vec<String>>) -> Vec<Self> {
        let corrected = corrected.filter(|corrected| corrected.len() == written.len());
        written
            .into_iter()
            .enumerate()
            .map(|(index, text)| Self {
                corrected: corrected
                    .as_ref()
                    .map(|corrected| corrected[index].clone())
                    .filter(|corrected| *corrected != text),
                text,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
struct SubjectWord {
    text: String,
    corrected: Option<String>,
    required: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct ContextPart {
    label: Label,
    words: Vec<String>,
    corrected: Option<Vec<String>>,
    respelt: bool,
    split: bool,
}

impl ContextPart {
    fn of(label: Label, value: Option<&str>) -> Option<Self> {
        let words = Tokens::new(value?).words;
        (!words.is_empty()).then_some(Self {
            label,
            words,
            corrected: None,
            respelt: false,
            split: false,
        })
    }

    fn is_postcode(&self) -> bool {
        self.label == Label::Postcode
    }
}

#[derive(Debug, Clone, PartialEq)]
struct StatedArea {
    label: Label,
    names: StatedNames,
    postal: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct StatedNames {
    names: Vec<Vec<String>>,
    any: bool,
    whole: bool,
}

impl StatedNames {
    fn known_words(part: &ContextPart, known: &HashSet<String>) -> Option<Self> {
        let words = part
            .words
            .iter()
            .filter(|word| known.contains(*word))
            .cloned()
            .collect::<Vec<_>>();
        (!words.is_empty()).then_some(Self {
            names: vec![words],
            any: false,
            whole: true,
        })
    }
}

struct StatedParts<'p> {
    plan: &'p QueryPlan,
    regions: Vec<StatedArea>,
}

#[derive(Debug, Default)]
struct StatedPlaces {
    countries: Vec<CountryCode>,
    country_words: Vec<String>,
    regions: Vec<StatedRegion>,
}

#[derive(Debug)]
struct StatedRegion {
    words: Vec<String>,
    country: Option<CountryCode>,
    regions: Vec<RecordId>,
}

#[derive(Debug, Default)]
struct Descriptors {
    directions: HashSet<String>,
    types: HashSet<String>,
    names: Vec<String>,
}

struct Search<'a> {
    placed: &'a HashSet<String>,
    query_words: &'a QueryWords,
    filters: Filters<'a>,
    focus: Option<[f64; 2]>,
    limit: usize,
}

fn unplaced_floor(count: usize) -> usize {
    match count {
        ..=1 => 1,
        2..=5 => count - 1,
        _ => count - count / 4,
    }
}

fn unit_in(words: &[String], from: usize) -> Option<usize> {
    let flat = |word: &String| {
        word.chars().any(|character| character.is_ascii_digit()) || word.chars().count() == 1
    };
    (from..words.len().saturating_sub(1))
        .find(|&at| designates_unit(&words[at]) && flat(&words[at + 1]))
}

type RunPart = (Vec<String>, Option<Vec<String>>);

fn split_run(
    words: &[String],
    corrected: &Option<Vec<String>>,
    at: usize,
) -> (RunPart, Vec<String>, RunPart) {
    let part = |range: std::ops::Range<usize>| -> RunPart {
        (
            words[range.clone()].to_vec(),
            corrected
                .as_ref()
                .and_then(|corrected| corrected.get(range).map(<[String]>::to_vec)),
        )
    };
    (
        part(0..at),
        words[at..at + 2].to_vec(),
        part(at + 2..words.len()),
    )
}

fn named_unit(words: &[String]) -> Option<String> {
    words
        .iter()
        .rev()
        .find(|word| word.chars().any(|character| character.is_ascii_digit()))
        .or_else(|| match words {
            [only] => (!designates_unit(only)).then_some(only),
            [.., last] => Some(last),
            [] => None,
        })
        .cloned()
}

impl QueryPlan {
    fn from_tagged(
        tokens: &[Tagged],
        corrections: &[Option<String>],
        breaks: &[bool],
        context: &[ContextPart],
        describes_street: impl Fn(&str) -> bool,
    ) -> Option<Self> {
        let mut plan = Self::default();
        let mut street = Vec::new();
        let expanded = |words: &[String]| expand_street_words(words);
        let expanded_run = |words: &[String], corrected: &Option<Vec<String>>| {
            Word::paired(
                expanded(words),
                corrected.as_ref().map(|corrected| expanded(corrected)),
            )
        };
        let mut runs = Vec::<(Label, Vec<String>, Option<Vec<String>>)>::new();
        let mut split = HashSet::new();
        for (index, tagged) in tokens.iter().enumerate() {
            let correction = corrections.get(index).cloned().flatten();
            let at = runs.len().saturating_sub(1);
            match runs.last_mut() {
                Some((label, words, corrected)) if *label == tagged.label => {
                    if breaks.get(index).copied().unwrap_or(false) {
                        split.insert(at);
                    }
                    if let Some(correction) = correction {
                        corrected
                            .get_or_insert_with(|| words.clone())
                            .push(correction);
                    } else if let Some(corrected) = corrected {
                        corrected.push(tagged.token.clone());
                    }
                    words.push(tagged.token.clone());
                }
                _ => runs.push((
                    tagged.label,
                    vec![tagged.token.clone()],
                    correction.map(|correction| vec![correction]),
                )),
            }
        }
        let unplaced_unit = runs
            .iter()
            .position(|(label, ..)| *label == Label::HouseNumber)
            .and_then(|at| at.checked_sub(1))
            .and_then(|before| match &runs[before] {
                (Label::Other, words, _) => words.last().cloned(),
                _ => None,
            })
            .filter(|word| word.chars().any(|character| character.is_ascii_digit()));
        for (run, (label, words, corrected)) in runs.into_iter().enumerate() {
            let split = split.contains(&run);
            match label {
                Label::HouseNumber if plan.house_number.is_none() => {
                    if let Some(number) = HouseNumber::parse(&words.join("_")) {
                        plan.house_number = Some(number);
                    } else if let Some(number) = HouseNumber::compound(&words) {
                        plan.house_number = Some(number);
                    } else if let Some((at, number)) = labelled_number(&words) {
                        plan.house_number = Some(number);
                        plan.number_kept = true;
                        plan.optional.extend(
                            words
                                .into_iter()
                                .enumerate()
                                .filter(|(index, word)| *index != at || !is_numeric_token(word))
                                .map(|(_, word)| word),
                        );
                    } else {
                        street.extend(Word::paired(words, None));
                    }
                }
                Label::Unit => match named_unit(&words) {
                    Some(unit) => {
                        plan.unit = Some(unit);
                        plan.unit_words.extend(words);
                    }
                    None => plan.optional.extend(words),
                },
                Label::HouseNumber => plan.optional.extend(words),
                Label::Street => match unit_in(&words, 1).filter(|_| plan.unit.is_none()) {
                    Some(at) => {
                        let (before, unit, after) = split_run(&words, &corrected, at);
                        street.extend(expanded_run(&before.0, &before.1));
                        plan.unit = named_unit(&unit);
                        plan.unit_words.extend(unit);
                        street.extend(expanded_run(&after.0, &after.1));
                    }
                    None => street.extend(expanded_run(&words, &corrected)),
                },
                Label::Other
                    if words.len() >= 2
                        && plan.unit.is_none()
                        && designates_unit(&words[0])
                        && let Some(unit) = named_unit(&words[..2]) =>
                {
                    plan.unit = Some(unit);
                    plan.unit_words.extend(words[..2].iter().cloned());
                    let rest = corrected
                        .as_ref()
                        .map(|corrected| corrected.get(2..).unwrap_or_default().to_vec());
                    plan.unplaced.extend(expanded_run(&words[2..], &rest));
                }
                Label::Other => plan.unplaced.extend(expanded_run(&words, &corrected)),
                Label::Locality | Label::Region | Label::Country
                    if plan.unit.is_none()
                        && let Some(at) = unit_in(&words, 0) =>
                {
                    let (before, unit, after) = split_run(&words, &corrected, at);
                    plan.unit = named_unit(&unit);
                    plan.unit_words.extend(unit);
                    for (words, corrected) in [before, after] {
                        if !words.is_empty() {
                            plan.context.push(ContextPart {
                                label,
                                words,
                                corrected,
                                respelt: false,
                                split,
                            });
                        }
                    }
                }
                Label::Locality | Label::Region | Label::Postcode | Label::Country => {
                    plan.context.push(ContextPart {
                        label,
                        words,
                        corrected,
                        respelt: false,
                        split,
                    });
                }
            }
        }
        plan.context.extend(context.iter().cloned());
        plan.explicit_postcode = context.iter().any(ContextPart::is_postcode);
        if plan.unit.is_none() && plan.house_number.is_some() {
            plan.unit = unplaced_unit;
        }
        if plan.house_number.is_some()
            && let Some(last) = street.last_mut()
        {
            let mut text = [std::mem::take(&mut last.text)];
            expand_final_direction(&mut text);
            [last.text] = text;
        }
        let named = street.iter().any(|word| !describes_street(&word.text));
        if tokens.iter().any(|tagged| tagged.label == Label::Street) && !street.is_empty() {
            plan.subject_part = SubjectPart(Some(Label::Street), Split::default());
        }
        plan.subject = street
            .into_iter()
            .map(|word| SubjectWord {
                required: !named || !describes_street(&word.text),
                text: word.text,
                corrected: word.corrected,
            })
            .collect();
        if plan.subject.is_empty() && plan.unplaced.is_empty() {
            let words = if !plan.context.is_empty() {
                let part = plan.context.remove(0);
                plan.subject_part = SubjectPart(Some(part.label), Split(part.split));
                Word::paired(part.words, part.corrected)
            } else if let Some(number) = plan.house_number.take() {
                let words = number.text.split_whitespace().map(str::to_string);
                Word::paired(words.collect(), None)
            } else {
                Word::paired(std::mem::take(&mut plan.optional), None)
            };
            plan.subject = words
                .into_iter()
                .map(|word| SubjectWord {
                    text: word.text,
                    corrected: word.corrected,
                    required: true,
                })
                .collect();
        }
        if plan.subject.is_empty() && plan.unplaced.is_empty() {
            return None;
        }
        Some(plan)
    }

    fn stated_parts(&self) -> impl Iterator<Item = Label> + '_ {
        self.house_number
            .iter()
            .map(|_| Label::HouseNumber)
            .chain(self.unit.iter().map(|_| Label::Unit))
            .chain(self.subject_part.0)
            .chain(self.context.iter().map(|part| part.label))
    }

    fn full_forms(&self, word: &str) -> Vec<String> {
        if STREET_TYPE_ABBREVIATIONS
            .iter()
            .any(|(abbreviation, _)| *abbreviation == word)
        {
            return Vec::new();
        }
        lattice::other_forms(word)
            .into_iter()
            .filter(|form| {
                !self
                    .subject
                    .iter()
                    .map(|word| &word.text)
                    .chain(self.unplaced.iter().map(|word| &word.text))
                    .any(|sought| sought == form)
            })
            .collect()
    }

    fn area_terms(&self) -> Vec<String> {
        let words = self
            .subject
            .iter()
            .map(|word| word.text.clone())
            .chain(self.unplaced.iter().map(|word| word.text.clone()));
        let parts = self.context.iter().flat_map(|part| {
            part.words
                .iter()
                .cloned()
                .chain((part.words.len() > 1).then(|| part.words.join(" ")))
        });
        unique_strings(words.chain(parts).collect())
    }

    fn text(&self) -> String {
        let subject = self
            .house_number
            .iter()
            .map(|number| number.text.as_str())
            .chain(self.subject.iter().map(|word| word.text.as_str()))
            .chain(self.unplaced.iter().map(|word| word.text.as_str()))
            .chain(self.optional.iter().map(String::as_str))
            .chain(self.unit_words.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        std::iter::once(subject)
            .chain(self.context.iter().map(|part| part.words.join(" ")))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn with_base_house_number(&self) -> Option<Self> {
        let number = self.house_number.as_ref()?;
        if let Some(short) = &number.short {
            let mut plan = Self {
                house_number: Some(HouseNumber {
                    forms: NumberForms::of(&short.text),
                    text: short.text.clone(),
                    base: number.base,
                    short: None,
                }),
                number_kept: short.kept,
                ..self.clone()
            };
            plan.optional.extend(short.rest.iter().cloned());
            return Some(plan);
        }
        if number.plain().is_some() {
            return None;
        }
        Some(Self {
            house_number: Some(HouseNumber {
                text: number.base.to_string(),
                forms: NumberForms::of(&number.base.to_string()),
                base: number.base,
                short: None,
            }),
            ..self.clone()
        })
    }

    fn postcode_may_be_house_number(&self) -> bool {
        self.house_number.is_none()
            && self.context.iter().any(|part| {
                part.is_postcode()
                    && matches!(part.words.as_slice(), [word] if HouseNumber::parse(word).is_some())
            })
    }

    fn postcode(&self) -> QueryPostcode {
        let Some(part) = self.context.iter().rev().find(|part| part.is_postcode()) else {
            return QueryPostcode::default();
        };
        QueryPostcode {
            explicit: self.explicit_postcode,
            ..QueryPostcode::of(&part.words)
        }
    }

    fn area_plans(
        &self,
        names: impl Fn(&ContextPart) -> Option<StatedNames>,
    ) -> Vec<(Self, &'static [Layer])> {
        let rank = |label: Label| match label {
            Label::Postcode => 0,
            Label::Locality => 1,
            Label::Region => 2,
            _ => 3,
        };
        let mut plans = Vec::new();
        let mut add = |words: Vec<String>, label: Label, split: bool| {
            let context = self
                .context
                .iter()
                .filter(|part| {
                    rank(part.label) > rank(label)
                        || (part.is_postcode() && label != Label::Postcode)
                })
                .cloned()
                .collect();
            let subject = words
                .into_iter()
                .map(|text| SubjectWord {
                    text,
                    corrected: None,
                    required: true,
                })
                .collect();
            plans.push((
                Self {
                    subject,
                    context,
                    subject_part: SubjectPart(Some(label), Split(split)),
                    ..Self::default()
                },
                area_layers(label),
            ));
        };
        for part in self.context.iter().rev().filter(|part| part.is_postcode()) {
            add(part.words.clone(), Label::Postcode, part.split);
            if part.words.len() > 1 {
                add(vec![part.words[0].clone()], Label::Postcode, false);
            }
        }
        for part in self
            .context
            .iter()
            .filter(|part| part.label == Label::Locality)
        {
            let named = names(part).map_or_else(|| vec![part.words.clone()], |names| names.names);
            for name in named {
                add(name, Label::Locality, part.split);
            }
        }
        for label in [Label::Region, Label::Country] {
            for part in self.context.iter().filter(|part| part.label == label) {
                add(part.words.clone(), label, part.split);
            }
        }
        plans
    }
}

#[derive(Debug, Clone, PartialEq)]
struct HouseNumber {
    text: String,
    base: u32,
    short: Option<ShortNumber>,
    forms: NumberForms,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct NumberForms {
    stated: Vec<String>,
    abbreviated: Vec<String>,
}

impl NumberForms {
    fn of(text: &str) -> Self {
        Self {
            stated: vec![text.to_string()],
            abbreviated: Vec::new(),
        }
    }

    fn terms(&self) -> impl Iterator<Item = &String> {
        self.stated.iter().chain(&self.abbreviated)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum NumberFit {
    Stated,
    Abbreviated,
}

fn number_runs(key: &str) -> Vec<&str> {
    let mut runs = Vec::new();
    for word in key.split_whitespace() {
        let mut start = 0;
        let mut digits = None;
        for (at, character) in word.char_indices() {
            let digit = character.is_ascii_digit();
            if digits.is_some_and(|before| before != digit) {
                runs.push(&word[start..at]);
                start = at;
            }
            digits = Some(digit);
        }
        runs.push(&word[start..]);
    }
    runs
}

fn number_fit(sought: &[&str], stated: &[&str]) -> Option<NumberFit> {
    if sought.len() != stated.len() {
        return None;
    }
    let mut fit = NumberFit::Stated;
    for (sought, stated) in sought.iter().zip(stated) {
        if sought == stated {
            continue;
        }
        let digits = |run: &str| run.starts_with(|character: char| character.is_ascii_digit());
        if digits(sought)
            || digits(stated)
            || !(abbreviates(sought, stated) || abbreviates(stated, sought))
        {
            return None;
        }
        fit = NumberFit::Abbreviated;
    }
    Some(fit)
}

#[derive(Debug, Clone, PartialEq)]
struct ShortNumber {
    text: String,
    rest: Vec<String>,
    kept: bool,
}

impl HouseNumber {
    fn compound(words: &[String]) -> Option<Self> {
        let first = words.first()?;
        if !first.starts_with(|character: char| character.is_ascii_digit()) {
            return None;
        }
        let short = match Self::parse(first) {
            Some(number) => ShortNumber {
                text: number.text,
                rest: words[1..].to_vec(),
                kept: false,
            },
            None => ShortNumber {
                text: base_house_number(first)?.to_string(),
                rest: words.to_vec(),
                kept: true,
            },
        };
        let text = words.join(" ");
        Some(Self {
            forms: NumberForms::of(&text),
            text,
            base: base_house_number(first)?,
            short: Some(short),
        })
    }

    fn flat_text(&self, unit: &str) -> String {
        format!("{unit} {}", self.text)
    }

    fn parse(word: &str) -> Option<Self> {
        let digits = word.trim_end_matches(|character: char| character.is_alphabetic());
        if word[digits.len()..].chars().count() > 1
            || !digits.split('_').all(|group| {
                !group.is_empty() && group.chars().all(|character| character.is_ascii_digit())
            })
        {
            return None;
        }
        let text = word.replace('_', " ");
        Some(Self {
            forms: NumberForms::of(&text),
            text,
            base: base_house_number(word)?,
            short: None,
        })
    }

    fn plain(&self) -> Option<u32> {
        is_numeric_token(&self.text).then_some(self.base)
    }
}

fn street_keys_of(words: &[String], describes_street: &dyn Fn(&str) -> bool) -> Vec<String> {
    let full = words
        .iter()
        .map(|word| lattice::full_form(word).unwrap_or(word).to_string())
        .collect::<Vec<_>>();
    let expanded = |at: usize| describes_street(&full[at]);
    let typed = |at: usize| {
        describes_street(&full[at])
            || describes_street(&words[at])
            || lattice::other_forms(&words[at])
                .iter()
                .any(|form| describes_street(form))
    };
    let mut keys = Vec::new();
    for describes in [&expanded as &dyn Fn(usize) -> bool, &typed] {
        let named = (0..full.len()).any(|at| !describes(at));
        let key = (0..full.len())
            .filter(|&at| !named || !describes(at))
            .map(|at| glued_stem(&full[at]).unwrap_or(&full[at]))
            .collect::<Vec<_>>()
            .join(" ");
        if !key.is_empty() && !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

fn labelled_number(words: &[String]) -> Option<(usize, HouseNumber)> {
    words
        .iter()
        .enumerate()
        .find_map(|(at, word)| HouseNumber::parse(word).map(|number| (at, number)))
        .or_else(|| {
            words.iter().enumerate().find_map(|(at, word)| {
                let digits = word
                    .find(|character: char| !character.is_ascii_digit())
                    .map_or(word.as_str(), |end| &word[..end]);
                (!digits.is_empty())
                    .then(|| HouseNumber::parse(digits))
                    .flatten()
                    .map(|number| (at, number))
            })
        })
}

fn expand_street_words(words: &[String]) -> Vec<String> {
    expand_address_abbreviations(&words.join(" "))
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn normalized_for_match(value: Option<&str>) -> Option<String> {
    search_key(value?.trim())
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
        .split(|character: char| {
            character.is_whitespace()
                || !character.is_ascii()
                    && !character.is_alphanumeric()
                    && !lattice::is_unspaced(character)
        })
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>()
}

const fn rank_window(limit: usize) -> usize {
    let window = limit.saturating_mul(RANK_WINDOW_FACTOR);
    let window = if window < MAX_RANK_WINDOW {
        window
    } else {
        MAX_RANK_WINDOW
    };
    if window > limit { window } else { limit }
}

fn autocomplete_subject_query(
    field: Field,
    tokens: &[String],
    last: Option<Term>,
) -> Box<dyn Query> {
    let mut head = tokens
        .iter()
        .map(|token| Term::from_field_text(field, token))
        .collect::<Vec<_>>();
    let prefix = head.pop().expect("an autocomplete query has a word");
    if head.is_empty() {
        if let Some(last) = last {
            return Box::new(RangeQuery::new(
                Bound::Included(prefix),
                Bound::Included(last),
            ));
        }
        let mut query = PhrasePrefixQuery::new(vec![prefix]);
        query.set_max_expansions(AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS);
        return Box::new(query);
    }
    Box::new(PhrasePrefix::new(head, prefix, AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS).ending_at(last))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlacePoint {
    pub near: Option<[f64; 2]>,
    pub known_abroad: Option<[f64; 2]>,
    pub known: Option<[f64; 2]>,
}

fn autocomplete_number(tokens: &[String]) -> Option<&str> {
    (tokens.len() > 1 && is_address_number_token(&tokens[0])).then(|| tokens[0].as_str())
}

fn is_address_number_token(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
}

mod highlight;

mod named;

mod choose;

mod names;
mod number_reading;
mod numbered_area;

mod phrase_prefix;

mod reach;
pub(crate) use reach::FAME_GROUP;

use phrase_prefix::PhrasePrefix;

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::LazyLock};

    use crate::{
        pack::Writer,
        record::{
            AddressComponents, AddressRecord, DerivedSourceProvenance, LocationPrecision,
            ObjectType, PostcodeRecord, SourceProvenance, StreetRecord, point_geometry,
        },
        test_support::TempDir,
        text_index::PostcodeAreas,
    };

    use super::*;

    #[test]
    fn an_unjudged_country_only_rejects_a_verified_conflict() {
        let expected = CountryCode::parse("LU").expect("country code");
        let other = CountryCode::parse("BE").expect("country code");
        let unmatched = Some(PartMatch::Unmatched);

        assert!(!contradicts_country(unmatched, &[expected], Some(expected)));
        assert!(!contradicts_country(unmatched, &[expected], None));
        assert!(!contradicts_country(unmatched, &[], Some(other)));
        assert!(contradicts_country(unmatched, &[expected], Some(other)));
    }

    fn corpus(name: &str, write: impl FnOnce(&mut Writer)) -> TempDir {
        let dir = TempDir::new(name);
        let mut writer = Writer::create(&dir).expect("writer");
        write(&mut writer);
        writer.finish().expect("finish");
        dir
    }

    static RANKING: LazyLock<TempDir> = LazyLock::new(|| corpus("search-ranking", write_ranking));

    fn write_ranking(writer: &mut Writer) {
        let mut prince =
            address_record("osm:node:1", "", "342", "Prince Albert Street", None, None);
        prince.geometry = point_geometry(-75.6, 45.4);
        let mut way = address_record("osm:node:9", "", "342", "Albert Street", None, None);
        way.source.object_type = ObjectType::Way;
        for record in [
            prince,
            way,
            address_record("osm:node:7", "", "342", "Albert Street", None, None),
            address_record("osm:node:5", "", "342", "Albert Street", None, None),
        ] {
            writer.write(&record.into(), None).expect("write");
        }

        let mut disputed = address_record("osm:node:15", "", "77", "Dunmore Avenue", None, None);
        disputed.disputed = true;
        for record in [
            disputed,
            address_record("osm:node:17", "", "77", "Dunmore Avenue", None, None),
        ] {
            writer.write(&record.into(), None).expect("write");
        }

        for index in 1..=40 {
            let mut street = street_record(&format!("osm:way:{}", 100 + index), "Tamarack Road");
            let lon = -100.0 + f64::from(index);
            street.geometry = point_geometry(lon, 45.0);
            street.representative_point = [lon, 45.0];
            writer.write(&street.into(), None).expect("street");
        }
        let mut prince =
            address_record("osm:node:21", "", "342", "Prince Regent Street", None, None);
        prince.geometry = point_geometry(-60.0, 45.0);
        writer.write(&prince.into(), None).expect("prince");
        let mut regent = address_record("osm:node:22", "", "342", "Regent Street", None, None);
        regent.geometry = point_geometry(-120.0, 45.0);
        writer.write(&regent.into(), None).expect("regent");

        for index in 1..=60 {
            writer
                .write(
                    &street_record(&format!("osm:way:{}", 300 + index), "Richmond Road").into(),
                    None,
                )
                .expect("road");
        }
        for (object_id, place_type) in [(101, "hamlet"), (102, "town")] {
            writer
                .write(
                    &Record::Place(
                        PlaceLayer::Locality,
                        crate::record::PlaceRecord {
                            alt_names: Vec::new(),
                            name: "Richmond".to_string(),
                            place_type: place_type.to_string(),
                            geometry: point_geometry(-79.0, 43.0),
                            source: SourceProvenance::osm(ObjectType::Node, object_id),
                            prominence: Default::default(),
                            wikidata: None,
                        },
                    ),
                    None,
                )
                .expect("place");
        }
        let mut upper = street_record("osm:way:201", "Upper Kent Street");
        upper.alt_names = vec!["Belgium Street".to_string()];
        writer.write(&upper.into(), None).expect("upper");
        writer
            .write(&street_record("osm:way:202", "Kent Street").into(), None)
            .expect("kent");

        for number in 1..=6 {
            let mut address = address_record(
                &format!("osm:node:{}", 30 + number),
                "",
                &number.to_string(),
                "Birch Road",
                None,
                None,
            );
            address.geometry = point_geometry(-79.0 + f64::from(number) * 0.001, 43.0);
            writer.write(&address.into(), None).expect("address");
        }
        let mut far = address_record("osm:node:39", "", "2", "Birch Road", None, None);
        far.geometry = point_geometry(10.0, 50.0);
        writer.write(&far.into(), None).expect("far");
    }

    #[test]
    fn equal_scores_prefer_the_closest_label_then_points_then_lower_ids() {
        let searcher = TextSearcher::open(&*RANKING).expect("searcher");
        let hits = searcher
            .search(&TextSearchOptions {
                query: "342 Albert Street".to_string(),
                limit: 3,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search");
        let ids = hits
            .iter()
            .map(|hit| hit.record.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["osm:node:5", "osm:node:7", "osm:way:9"]);
    }

    #[test]
    fn equal_hits_prefer_an_address_no_source_disputes() {
        let searcher = TextSearcher::open(&*RANKING).expect("searcher");
        assert_eq!(
            top_ids(&searcher, "77 Dunmore Avenue"),
            vec!["osm:node:17", "osm:node:15"]
        );
    }

    #[test]
    fn answers_carry_a_confidence_only_from_a_calibrated_pack() {
        use crate::confidence::{FEATURES, Model};

        let temp_dir = TempDir::new("search-confidence");
        let mut writer = Writer::create(&temp_dir).expect("writer");
        for record in [
            address_record("osm:node:1", "", "10", "King Street", None, Some("M5V 1A1")),
            address_record("osm:node:2", "", "12", "King Street", None, Some("M5V 1A1")),
        ] {
            writer.write(&record.into(), None).expect("write");
        }
        writer
            .write(&street_record("osm:way:9", "King Street").into(), None)
            .expect("street");
        writer.finish().expect("finish");
        let search = |searcher: &TextSearcher, query: &str| {
            searcher
                .search(&TextSearchOptions {
                    query: query.to_string(),
                    limit: 5,
                    layer: None,
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                })
                .expect("search")
        };

        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        assert!(searcher.confidence_model().is_none());
        let hits = search(&searcher, "10 King Street");
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|hit| hit.confidence.is_none()));

        let pack = pack::Reader::open(&temp_dir).expect("pack");
        let mut weights = [0.0; FEATURES];
        weights[0] = 1.0;
        Model::new(weights, std::collections::BTreeMap::new())
            .save(&pack)
            .expect("save");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        for query in ["10 King Street", "99 King Street"] {
            let hits = search(&searcher, query);
            assert!(!hits.is_empty(), "{query}");
            for hit in hits {
                let confidence = hit
                    .confidence
                    .expect("a calibrated Pack rates every answer to a numbered address");
                assert!((0.0..=1.0).contains(&confidence), "{query}: {confidence}");
            }
        }
        let hits = search(&searcher, "King");
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|hit| hit.confidence.is_none()));
        let suggestions = searcher
            .autocomplete(&TextAutocompleteOptions {
                query: "10 King St".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("autocomplete");
        assert!(suggestions.iter().all(|hit| hit.confidence.is_some()));
        assert!(suggestions.iter().all(|hit| hit.features.prefix));

        let other = TempDir::new("search-confidence-other");
        let mut writer = Writer::create(&other).expect("writer");
        writer
            .write(&street_record("osm:way:9", "Queen Street").into(), None)
            .expect("street");
        writer.finish().expect("finish");
        let other_pack = pack::Reader::open(&other).expect("pack");
        std::fs::copy(
            confidence::Model::path(pack.path()),
            confidence::Model::path(other_pack.path()),
        )
        .expect("copy");
        assert!(TextSearcher::open(&other).is_err());
    }

    fn search_near(
        searcher: &TextSearcher,
        query: &str,
        limit: usize,
        focus: Option<[f64; 2]>,
        bbox: Option<BoundingBox>,
    ) -> Vec<String> {
        searcher
            .search(&TextSearchOptions {
                query: query.to_string(),
                limit,
                layer: None,
                countries: Vec::new(),
                focus,
                bbox,
            })
            .expect("search")
            .into_iter()
            .map(|hit| hit.record.id)
            .collect()
    }

    #[test]
    fn a_focus_breaks_ties_toward_the_caller_and_nothing_else() {
        let searcher = TextSearcher::open(&*RANKING).expect("searcher");

        assert_eq!(
            search_near(&searcher, "Tamarack Road", 1, Some([-60.001, 45.0]), None),
            ["osm:way:140"],
            "the nearest of the tied makes the window and ranks first"
        );
        assert_eq!(
            search_near(&searcher, "Tamarack Road", 2, Some([-98.999, 45.0]), None),
            ["osm:way:101", "osm:way:102"]
        );
        assert_eq!(
            search_near(&searcher, "342 Regent Street", 2, Some([-60.0, 45.0]), None)[0],
            "osm:node:22",
            "a focus never outranks a better match"
        );
    }

    #[test]
    fn of_equal_scores_the_window_keeps_the_closest_name_then_the_larger_place() {
        let searcher = TextSearcher::open(&*RANKING).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "Richmond")[..2],
            ["osm:node:102", "osm:node:101"],
            "the town, then the hamlet, before any road"
        );
        assert_eq!(top_ids(&searcher, "Kent Street")[0], "osm:way:202");
    }

    #[test]
    fn a_bounding_box_keeps_only_the_records_inside_it() {
        let searcher = TextSearcher::open(&*RANKING).expect("searcher");
        let bbox = BoundingBox {
            min_lon: -79.0,
            min_lat: 42.9,
            max_lon: -78.9965,
            max_lat: 43.1,
        };

        assert_eq!(
            search_near(&searcher, "2 Birch Road", 5, None, None).len(),
            2,
            "unboxed, both houses 2 answer"
        );
        assert_eq!(
            search_near(&searcher, "2 Birch Road", 5, None, Some(bbox)),
            ["osm:node:32"]
        );
        assert!(
            !search_near(&searcher, "5 Birch Road", 5, None, Some(bbox))
                .contains(&"osm:node:35".to_string()),
            "house 5 lies past the east edge, though its group's extent meets the box"
        );
        let suggestions = searcher
            .autocomplete(&TextAutocompleteOptions {
                query: "2 Birch".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: Some(bbox),
            })
            .expect("autocomplete");
        assert_eq!(
            suggestions
                .iter()
                .map(|hit| hit.record.id.as_str())
                .collect::<Vec<_>>(),
            ["osm:node:32"]
        );
    }

    fn geocode(
        searcher: &TextSearcher,
        address: &str,
        locality: Option<&str>,
        postcode: Option<&str>,
    ) -> Option<String> {
        searcher
            .geocode_address(&AddressGeocodeOptions {
                address: address.to_string(),
                locality: locality.map(str::to_string),
                region: None,
                postcode: postcode.map(str::to_string),
                limit: 10,
                layer: None,
                countries: Vec::new(),
            })
            .expect("geocode")
            .map(|hit| hit.hit.record.id)
    }

    static NUMBERS: LazyLock<TempDir> = LazyLock::new(|| {
        let dir = TempDir::new("search-numbers");
        let mut writer = Writer::create(&dir).expect("writer");
        let street = write_numbers(&mut writer);
        let scratch = std::sync::Arc::clone(writer.scratch());
        let (mut pack, _) = writer.seal().expect("seal");
        crate::structured::add_to(
            &mut pack,
            &scratch,
            &crate::memory::Budget::unlimited(),
            usize::MAX,
        )
        .expect("structured index");
        crate::ids::add_to(
            &mut pack,
            &scratch,
            &crate::memory::Budget::unlimited(),
            usize::MAX,
        )
        .expect("ids");
        let ranges = crate::tiger::encode(vec![crate::tiger::Range {
            street,
            from: 2,
            to: 98,
            start: 0.0,
            end: 1.0,
            zip: Some(2852),
            line: 820,
            step: 2,
            side: b'L',
        }])
        .expect("ranges");
        pack.add_section(crate::tiger::SECTION, crate::tiger::VERSION, &ranges)
            .expect("TIGER section");
        pack.publish().expect("publish");
        dir
    });

    fn write_numbers(writer: &mut Writer) -> RecordId {
        use crate::record::{
            InterpolationAddressComponents, InterpolationRange, InterpolationRecord,
        };

        for (id, locality, point) in [
            ("osm:node:101", "Portside", [-90.0, 35.0]),
            ("osm:node:102", "Queensport", [-89.0, 35.0]),
            ("osm:node:103", "Queensport", [-86.0, 37.0]),
        ] {
            let record = address_record(id, "", "1-12", "Quay Street", Some(locality), None);
            writer
                .write(&located(record, point).into(), None)
                .expect("address");
        }

        for (id, number, street) in [
            ("osm:node:201", "350", "5th Avenue"),
            ("osm:node:202", "200", "Fifth Street"),
            ("osm:node:203", "350", "Main Street"),
        ] {
            let record = address_record(id, "", number, street, None, None);
            writer
                .write(&located(record, [-95.0, 40.0]).into(), None)
                .expect("address");
        }

        for (id, street) in [
            ("osm:node:301", "Michael Jones Drive"),
            ("osm:node:302", "Morgan Jones Drive"),
        ] {
            let record = address_record(id, "", "53", street, None, None);
            writer
                .write(&located(record, [-96.0, 40.0]).into(), None)
                .expect("address");
        }

        for (id, street) in [
            ("osm:node:401", "Highfield Way"),
            ("osm:node:402", "Highfield Terrace"),
            ("osm:node:403", "Highfield Road"),
        ] {
            let record = address_record(id, "", "46", street, None, None);
            writer
                .write(&located(record, [-97.0, 40.0]).into(), None)
                .expect("address");
        }

        let mut flat = address_record("osm:node:501", "", "45", "Lomond Crescent", None, None);
        flat.address.unit = Some("2".to_string());
        for record in [
            flat,
            address_record("osm:node:502", "", "45", "Ben Lomond Crescent", None, None),
            address_record("osm:node:503", "", "47", "Mill Road", None, None),
            address_record(
                "osm:node:504",
                "",
                "47",
                "Millton Park Road",
                Some("Mill Road"),
                None,
            ),
        ] {
            writer
                .write(&located(record, [-98.0, 40.0]).into(), None)
                .expect("address");
        }

        let mut street = street_record("osm:way:630", "Wellesley Street");
        street.geometry = queen_street_line();
        writer.write(&street.into(), None).expect("street");
        writer
            .write(
                &InterpolationRecord {
                    address: InterpolationAddressComponents {
                        street: Some("Wellesley Street".to_string()),
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
                    source: SourceProvenance::osm(ObjectType::Way, 620),
                }
                .into(),
                None,
            )
            .expect("interpolation");
        writer
            .write(
                &address_record("osm:node:610", "", "10", "Wellesley Street", None, None).into(),
                None,
            )
            .expect("address");

        let mut street = |id: &str, name: &str, lon: f64| {
            let mut street = street_record(id, name);
            street.geometry = line(lon, [46.0, 46.01]);
            street.representative_point = [lon, 46.005];
            writer.write(&street.into(), None).expect("street");
        };
        street("osm:way:701", "Bayview Avenue", -79.0);
        street("osm:way:702", "Victoria Street", -79.1);
        street("osm:way:703", "Victoria Street", -79.12);
        for (id, number, street, lon, lat) in [
            ("osm:node:710", "10", "Bayview Avenue", -79.0002, 46.001),
            ("osm:node:711", "20", "Bayview Avenue", -79.0002, 46.002),
            ("osm:node:712", "30", "Bayview Avenue", -79.0002, 46.003),
            ("osm:node:713", "81", "Victoria Street", -79.1002, 46.001),
            ("osm:node:714", "91", "Victoria Street", -79.1202, 46.002),
        ] {
            let address = address_record(id, "", number, street, None, None);
            writer
                .write(&located(address, [lon, lat]).into(), None)
                .expect("address");
        }

        let tiger = write_tiger_street(writer);

        for (id, number) in [("osm:node:901", "1"), ("osm:node:902", "3")] {
            let record = address_record(id, "", number, "Elizabeth Lane", None, None);
            writer
                .write(&located(record, [-99.0, 40.0]).into(), None)
                .expect("address");
        }

        for (id, number, street) in [
            ("osm:node:1001", "1384 1/2", "Dundas Street East"),
            ("osm:node:1002", "1384", "Dundas Street East"),
            ("osm:node:1003", "993", "Bloor Street West"),
            ("osm:node:1004", "100", "Highway 7"),
            ("osm:node:1005", "100", "Highway 8"),
            ("osm:node:1006", "8", "The Donway East"),
        ] {
            let record = address_record(id, "", number, street, None, None);
            writer
                .write(&located(record, [-100.0, 40.0]).into(), None)
                .expect("address");
        }

        for (index, (id, number)) in [
            ("osm:node:1101", "81"),
            ("osm:node:1102", "81 к1"),
            ("osm:node:1103", "81 к2"),
            ("osm:node:1104", "83"),
            ("osm:node:1105", "85 с1"),
            ("osm:node:1106", "85 соор1"),
        ]
        .into_iter()
        .enumerate()
        {
            let record = address_record(id, "", number, "улица Вавилова", Some("Москва"), None);
            let point = [37.54 + index as f64 * 0.001, 55.68];
            writer
                .write(&located(record, point).into(), None)
                .expect("address");
        }

        for (index, (id, number, postcode)) in [
            ("osm:node:1201", "85 с1", "420015"),
            ("osm:node:1202", "85 соор1", "420016"),
            ("osm:node:1203", "87 к1", "420015"),
            ("osm:node:1204", "87 корпус 1", "420016"),
            ("osm:node:1205", "88к1", "420015"),
            ("osm:node:1206", "88 к1", "420016"),
        ]
        .into_iter()
        .enumerate()
        {
            let record = address_record(
                id,
                "",
                number,
                "улица Пушкина",
                Some("Казань"),
                Some(postcode),
            );
            let point = [49.12 + index as f64 * 0.001, 55.79];
            writer
                .write(&located(record, point).into(), None)
                .expect("address");
        }

        for (index, number) in ["1", "3", "5", "5", "5A", "7", "9;11", "13"]
            .iter()
            .enumerate()
        {
            let mut record = address_record(
                &format!("osm:node:{}", 1301 + index),
                "",
                number,
                "Spruce Street",
                Some("Toronto"),
                None,
            );
            record.geometry = point_geometry(-101.0 + index as f64 * 0.0002, 40.0);
            if index == 3 {
                record.address.unit = Some("Unit 2".to_string());
            }
            writer.write(&record.into(), None).expect("address");
        }
        let oakville = address_record(
            "osm:node:1320",
            "",
            "5",
            "Spruce Street",
            Some("Oakville"),
            None,
        );
        writer
            .write(&located(oakville, [-101.0, 40.0]).into(), None)
            .expect("address");

        let mut flat = address_record("osm:node:1401", "", "5", "Albion Street", None, None);
        flat.address.unit = Some("Unit 2".to_string());
        let building = address_record("osm:node:1402", "", "5", "Albion Street", None, None);
        for (record, point) in [(flat, [-102.0, 40.0]), (building, [-102.0002, 40.0])] {
            writer
                .write(&located(record, point).into(), None)
                .expect("address");
        }

        let descriptors = [-103.0, 40.0];
        for record in [
            located(
                address_record(
                    "osm:node:1510",
                    "",
                    "5525",
                    "South US Highway 281",
                    None,
                    None,
                ),
                descriptors,
            )
            .into(),
            street_at("osm:way:1520", "North US Highway 281", descriptors).into(),
            street_at("osm:way:1521", "South US Highway 281", descriptors).into(),
            located(
                address_record("osm:node:1511", "", "3935", "16th Avenue North", None, None),
                descriptors,
            )
            .into(),
            street_at("osm:way:1522", "16th Street North", descriptors).into(),
        ] {
            writer.write(&record, None).expect("descriptors");
        }

        for (id, street, lon) in [
            ("osm:node:1601", "Wilderinksstraat", 6.7930),
            ("osm:node:1602", "Wilderinksplein", 6.7935),
        ] {
            let record = address_record(id, "", "2", street, Some("Hengelo"), None);
            writer
                .write(&located(record, [lon, 52.265]).into(), None)
                .expect("address");
        }

        let dayton = [-84.2, 39.75];
        for record in [
            address_record(
                "osm:node:1710",
                "",
                "1040",
                "West Harrison Street",
                None,
                Some("60607"),
            ),
            address_record(
                "osm:node:1711",
                "",
                "8600",
                "North Main Street",
                None,
                Some("45415"),
            ),
            address_record(
                "osm:node:1712",
                "",
                "73",
                "Eibacher Hauptstraße",
                None,
                None,
            ),
            address_record(
                "osm:node:1713",
                "",
                "131",
                "Impasse de la Noria",
                None,
                None,
            ),
            address_record("osm:node:1714", "", "7451", "Warner Avenue", None, None),
            address_record("osm:node:1715", "", "5", "Unit Road", None, None),
        ] {
            writer
                .write(&located(record, dayton).into(), None)
                .expect("address");
        }
        writer
            .write(
                &PostcodeRecord {
                    postcode: "45415".to_string(),
                    country: None,
                    geometry: point_geometry(dayton[0], dayton[1]),
                    source: DerivedSourceProvenance::osm_address_records(1),
                }
                .into(),
                None,
            )
            .expect("postcode");

        tiger
    }

    fn write_tiger_street(writer: &mut Writer) -> RecordId {
        let at = [-71.4, 41.8];
        let region = place(
            writer,
            PlaceLayer::Region,
            "Rhode Island",
            ObjectType::Relation,
            802,
            at,
            None,
        );
        writer.set_subdivision_codes([(region, "US-RI".to_string())].into_iter().collect());
        let locality = place(
            writer,
            PlaceLayer::Locality,
            "Fairview",
            ObjectType::Node,
            801,
            at,
            chain(&[(PlaceLayer::Region, region)]),
        );
        writer.set_context_names(
            [
                (region, vec!["Rhode Island".to_string()]),
                (locality, vec!["Fairview".to_string()]),
            ]
            .into_iter()
            .collect(),
        );
        let mut road = street_record("osm:way:820", "Queen Street West");
        road.locality = Some("Fairview".to_string());
        road.geometry = line(at[0], [at[1], at[1] + 0.001]);
        road.representative_point = [at[0], at[1] + 0.0005];
        writer
            .write(
                &road.into(),
                chain(&[
                    (PlaceLayer::Region, region),
                    (PlaceLayer::Locality, locality),
                ]),
            )
            .expect("street")
    }

    #[test]
    fn number_reading_needs_a_single_corroborated_address() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");
        let query = TextSearchOptions {
            query: "Portside, Quay Street 1番12号".into(),
            limit: 1,
            layer: Some("address".into()),
            countries: Vec::new(),
            focus: None,
            bbox: None,
        };
        assert_eq!(
            searcher.search(&query).unwrap()[0].record.id,
            "osm:node:101"
        );
        assert_eq!(
            geocode(&searcher, "Quay Street 1番12号", Some("Portside"), None).as_deref(),
            Some("osm:node:101")
        );
        let ambiguous = TextSearchOptions {
            query: "Queensport, Quay Street 1番12号".into(),
            ..query
        };
        assert_eq!(
            searcher.search(&ambiguous).unwrap(),
            searcher.search_at(&ambiguous, Depth::Query).unwrap()
        );
        assert_eq!(
            geocode(&searcher, "Quay Street 1番12号", Some("Queensport"), None),
            None,
        );
    }

    static POSTCODES: LazyLock<TempDir> =
        LazyLock::new(|| corpus("search-postcodes", write_postcodes));

    fn postcode_record(postcode: &str, country: Option<&str>, [lon, lat]: [f64; 2]) -> Record {
        PostcodeRecord {
            postcode: postcode.to_string(),
            country: country.and_then(CountryCode::parse),
            geometry: point_geometry(lon, lat),
            source: DerivedSourceProvenance::osm_address_records(5),
        }
        .into()
    }

    fn write_postcodes(writer: &mut Writer) {
        use crate::record::{
            InterpolationAddressComponents, InterpolationRange, InterpolationRecord,
        };

        writer
            .write(
                &address_record(
                    "osm:node:1",
                    "",
                    "10",
                    "Gerrard Street",
                    None,
                    Some("M5V 1A1"),
                )
                .into(),
                None,
            )
            .expect("write");

        let write_street = |writer: &mut Writer, id: &str, lon: f64, postcode: &str| {
            let mut record = address_record(id, "", "10", "King Street", None, None);
            record.geometry = point_geometry(lon, 43.0);
            writer.write(&record.into(), None).expect("address");
            writer
                .write(&postcode_record(postcode, None, [lon + 0.001, 43.0]), None)
                .expect("postcode");
        };
        for id in 1..=12 {
            write_street(
                writer,
                &format!("osm:node:{}", 100 + id),
                -90.0 - id as f64,
                "L5L 1A1",
            );
        }
        write_street(writer, "osm:node:199", -76.0, "N7N 1A1");

        writer
            .write(&postcode_record("3902", None, [-76.15, 30.0]), None)
            .expect("postcode");
        for id in 1..=12 {
            writer
                .write(
                    &postcode_record("3000", None, [-90.0 - id as f64 + 0.001, 30.0]),
                    None,
                )
                .expect("postcode");
        }
        for id in 1..=12 {
            let town = format!("Town {id}");
            let mut record = address_record(
                &format!("osm:node:{}", 200 + id),
                "",
                "10",
                "Church Street",
                Some(&town),
                None,
            );
            record.geometry = point_geometry(-90.0 - id as f64, 30.0);
            writer.write(&record.into(), None).expect("address");
        }
        let mut rural = address_record(
            "osm:node:299",
            "",
            "10",
            "Church Street",
            Some("Bairnsdale"),
            None,
        );
        rural.geometry = point_geometry(-76.0, 30.0);
        writer.write(&rural.into(), None).expect("address");

        let mut cobden = address_record("osm:node:301", "", "44", "Gould Street", None, None);
        cobden.geometry = point_geometry(-76.88, 45.63);
        writer.write(&cobden.into(), None).expect("cobden");
        writer
            .write(&postcode_record("J0J 1K0", None, [-76.881, 45.631]), None)
            .expect("postcode");
        let mut remote = address_record("osm:node:302", "", "7", "Far Road", None, None);
        remote.geometry = point_geometry(-90.0, 50.0);
        writer.write(&remote.into(), None).expect("remote");

        for (country, code, lon, lat) in [
            ("AU", "2110", 151.150, -33.830),
            ("AU", "2111", 151.135, -33.815),
            ("AU", "2112", 151.120, -33.800),
            ("NZ", "2110", 174.944, -37.065),
            ("NZ", "2112", 174.900, -37.150),
            ("NZ", "2113", 174.980, -37.120),
        ] {
            writer
                .write(&postcode_record(code, Some(country), [lon, lat]), None)
                .expect("postcode");
        }
        for (id, street, lon, lat) in [
            ("osm:node:401", "Elliot Street", 174.945, -37.066),
            ("osm:node:402", "Ferry Street", 151.151, -33.831),
        ] {
            let mut address = address_record(id, "", "10", street, None, None);
            address.geometry = point_geometry(lon, lat);
            writer.write(&address.into(), None).expect("address");
        }

        for (code, lon) in [
            ("M4M 1L2", -79.3330),
            ("M4M 1L5", -79.3338),
            ("M4M 1L7", -79.3345),
            ("M6J 1H9", -79.4250),
            ("M6J 1J2", -79.4258),
            ("M6J 1J4", -79.4265),
        ] {
            writer
                .write(&postcode_record(code, None, [lon, 38.65]), None)
                .expect("postcode");
        }
        let mut east = address_record("osm:node:501", "", "1161", "Dundas Street East", None, None);
        east.street_postcodes = vec!["M4M 1L2".to_string(), "M4M 1L5".to_string()];
        east.geometry = point_geometry(-79.3334, 38.65);
        let mut west = address_record("osm:node:502", "", "1161", "Dundas Street West", None, None);
        west.street_postcodes = vec!["M6J 1H9".to_string(), "M6J 1J2".to_string()];
        west.geometry = point_geometry(-79.4255, 38.65);
        for record in [east, west] {
            writer.write(&record.into(), None).expect("address");
        }

        let mut border = address_record("osm:node:601", "", "5", "Line Road", None, None);
        border.geometry = point_geometry(-70.0, 43.0);
        writer.write(&border.into(), None).expect("address");
        for index in 0..20 {
            writer
                .write(
                    &postcode_record(
                        &format!("G5G {index}A{index}"),
                        None,
                        [-70.0 + 0.0001 * index as f64, 43.0],
                    ),
                    None,
                )
                .expect("postcode");
        }
        writer
            .write(&postcode_record("E4E 1E1", None, [-69.9, 43.0]), None)
            .expect("postcode");

        writer.set_postcode_areas(PostcodeAreas::new([(
            "A1A 1A1".to_string(),
            [-79.001, 43.001],
        )]));
        let mut stated = address_record(
            "osm:node:701",
            "",
            "5",
            "Main Street",
            None,
            Some("B2B 2B2"),
        );
        stated.geometry = point_geometry(-81.0, 45.0);
        let mut near = address_record("osm:node:702", "", "5", "Main Street", None, None);
        near.geometry = point_geometry(-79.0, 43.0);
        let mut far = address_record("osm:node:703", "", "5", "Main Street", None, None);
        far.geometry = point_geometry(-80.0, 44.0);
        for record in [stated, near, far] {
            writer.write(&record.into(), None).expect("address");
        }

        let mut elsewhere =
            address_record("osm:node:801", "", "8", "High Street", None, Some("7018"));
        elsewhere.geometry = point_geometry(147.4, -42.9);
        let mut street = street_record("osm:way:802", "High Street");
        street.geometry = point_geometry(146.65, -41.52);
        street.representative_point = [146.65, -41.52];
        writer.write(&elsewhere.into(), None).expect("address");
        writer.write(&street.into(), None).expect("street");

        writer.set_postcode_areas(PostcodeAreas::new([
            ("S5C 1A1".to_string(), [-79.0, 43.0005]),
            ("S8V 1A1".to_string(), [-79.5, 43.6]),
        ]));
        let mut elsewhere = address_record("osm:node:901", "", "23", "Victoria Street", None, None);
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
                    source: SourceProvenance::osm(ObjectType::Way, 920),
                }
                .into(),
                None,
            )
            .expect("range");
        let mut elm = street_record("osm:way:930", "Elm Street");
        elm.geometry = queen_street_line();
        elm.representative_point = [-79.0, 43.0005];
        writer.write(&elm.into(), None).expect("street");
        let mut number = address_record("osm:node:904", "", "7", "Elm Street", None, None);
        number.geometry = point_geometry(-79.5, 43.6);
        writer.write(&number.into(), None).expect("number");
        let mut atlantis = address_record(
            "osm:node:905",
            "",
            "1",
            "Ocean Drive",
            Some("Atlantis"),
            None,
        );
        atlantis.geometry = point_geometry(-60.0, 30.0);
        writer.write(&atlantis.into(), None).expect("atlantis");
    }

    #[test]
    fn geocode_rejects_hits_in_a_different_postcode() {
        let searcher = TextSearcher::open(&*POSTCODES).expect("searcher");

        assert_eq!(
            geocode(&searcher, "10 Gerrard Street", None, Some("K7L 1A1")),
            None
        );
        assert_eq!(
            geocode(&searcher, "10 Gerrard Street", None, Some("m5v1a1")).as_deref(),
            Some("osm:node:1")
        );
        let forward_sortation_area = Some("M5V");
        assert_eq!(
            geocode(&searcher, "10 Gerrard Street", None, forward_sortation_area).as_deref(),
            Some("osm:node:1")
        );
    }

    #[test]
    fn geocode_filters_hits_ranked_beyond_the_requested_limit() {
        let searcher = TextSearcher::open(&*POSTCODES).expect("searcher");

        for limit in [1, 10] {
            let hit = searcher
                .geocode_address(&AddressGeocodeOptions {
                    address: "10 King Street".to_string(),
                    locality: None,
                    region: None,
                    postcode: Some("N7N 1A1".to_string()),
                    limit,
                    layer: None,
                    countries: Vec::new(),
                })
                .expect("geocode")
                .map(|hit| hit.hit.record.id);
            assert_eq!(hit.as_deref(), Some("osm:node:199"), "limit {limit}");
        }
    }

    #[test]
    fn geocode_ranks_hits_by_distance_from_the_named_postcode() {
        let searcher = TextSearcher::open(&*POSTCODES).expect("searcher");

        let hit = searcher
            .geocode_address(&AddressGeocodeOptions {
                address: "10 Church Street".to_string(),
                locality: None,
                region: None,
                postcode: Some("3902".to_string()),
                limit: 1,
                layer: None,
                countries: Vec::new(),
            })
            .expect("geocode")
            .map(|hit| hit.hit.record.id);
        assert_eq!(hit.as_deref(), Some("osm:node:299"));
    }

    #[test]
    fn geocode_infers_the_postcode_area_of_hits_without_one() {
        let searcher = TextSearcher::open(&*POSTCODES).expect("searcher");

        assert_eq!(
            geocode(&searcher, "44 Gould Street", None, Some("T5T 1A1")),
            None
        );
        assert_eq!(
            geocode(&searcher, "44 Gould Street", None, Some("J0J 1K0")).as_deref(),
            Some("osm:node:301")
        );
        assert_eq!(
            geocode(&searcher, "7 Far Road", None, Some("T5T 1A1")).as_deref(),
            Some("osm:node:302")
        );
    }

    #[test]
    fn geocode_reads_a_postcode_two_countries_share_in_each_one() {
        let searcher = TextSearcher::open(&*POSTCODES).expect("searcher");

        for (country, street, id, foreign, other) in [
            ("NZ", "10 Elliot Street", "osm:node:401", "2111", "au"),
            ("AU", "10 Ferry Street", "osm:node:402", "2113", "nz"),
        ] {
            assert_eq!(
                geocode(&searcher, street, None, Some("2110")).as_deref(),
                Some(id),
                "the {country} 2110 is not measured against the other country's"
            );
            assert_eq!(
                geocode(&searcher, street, None, Some(foreign)),
                Some(format!("derived:osm:postcode:{other}:{foreign}")),
                "{foreign} contradicts the {country} address"
            );
        }
    }

    #[test]
    fn a_postcode_two_countries_share_is_read_in_the_country_and_box_asked() {
        use crate::record::PlaceLayer::Country;

        let dir = TempDir::new("search-postcode-scope");
        let mut writer = Writer::create(&dir).expect("writer");
        let countries = [
            ("AU", "Australia", 9001, [134.0, -25.0], [151.150, -33.830]),
            (
                "NZ",
                "New Zealand",
                9002,
                [172.0, -41.0],
                [174.944, -37.065],
            ),
        ]
        .map(|(code, name, id, at, point)| {
            let country = place(
                &mut writer,
                Country,
                name,
                ObjectType::Relation,
                id,
                at,
                None,
            );
            (code, country, point)
        });
        writer.set_country_codes(
            countries
                .iter()
                .map(|(code, country, _)| (*country, CountryCode::parse(code).expect("code")))
                .collect(),
        );
        for (code, country, point) in countries {
            writer
                .write(
                    &postcode_record("2110", Some(code), point),
                    chain(&[(Country, country)]),
                )
                .expect("postcode");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&dir).expect("searcher");
        let ids = |countries: &[&str], bbox| {
            searcher
                .postcode_reading(&TextAutocompleteOptions {
                    query: "2110".to_string(),
                    limit: 10,
                    layer: None,
                    countries: countries
                        .iter()
                        .map(|country| country.to_string())
                        .collect(),
                    focus: None,
                    bbox,
                })
                .expect("reading")
                .into_iter()
                .map(|hit| hit.record.id)
                .collect::<Vec<_>>()
        };
        let auckland = BoundingBox {
            min_lon: 174.0,
            min_lat: -38.0,
            max_lon: 175.5,
            max_lat: -36.0,
        };
        assert_eq!(ids(&["NZ"], None), ["derived:osm:postcode:nz:2110"]);
        assert_eq!(ids(&["AU"], None), ["derived:osm:postcode:au:2110"]);
        assert_eq!(ids(&[], Some(auckland)), ["derived:osm:postcode:nz:2110"]);
        assert_eq!(ids(&["AU"], Some(auckland)), Vec::<String>::new());
    }

    #[test]
    fn a_house_between_its_streets_postcodes_lies_in_the_one_asked_for() {
        let searcher = TextSearcher::open(&*POSTCODES).expect("searcher");

        for limit in [1, 10] {
            assert_eq!(
                top_ids_limited(&searcher, "1161 Dundas Street, M4M 1L3", limit)
                    .first()
                    .map(String::as_str),
                Some("osm:node:501"),
                "limit {limit}"
            );
        }
    }

    #[test]
    fn geocode_accepts_hits_near_a_busier_neighbouring_postcode_area() {
        let searcher = TextSearcher::open(&*POSTCODES).expect("searcher");

        assert_eq!(
            geocode(&searcher, "5 Line Road", None, Some("E4E 2B2")).as_deref(),
            Some("osm:node:601")
        );
        assert_eq!(
            geocode(&searcher, "5 Line Road", None, Some("H6H 3C3")),
            None
        );
    }

    static AREAS: LazyLock<TempDir> = LazyLock::new(|| corpus("search-areas", write_areas));

    fn write_areas(writer: &mut Writer) {
        let mut names = HashMap::new();
        write_variants(writer, &mut names);
        write_unconfirmed_neighbourhood(writer);
        write_named_localities(writer, &mut names);
        write_community(writer, &mut names);
        write_partial_town(writer);
        write_county(writer, &mut names);
        write_any_area(writer, &mut names);
        write_countries(writer, &mut names);
        write_postal_towns(writer, &mut names);
        write_locality_runs(writer, &mut names);
        write_known_towns(writer, &mut names);
        write_matched(writer, &mut names);
    }

    fn extend_context_names(
        writer: &mut Writer,
        names: &mut HashMap<RecordId, Vec<String>>,
        more: impl IntoIterator<Item = (RecordId, Vec<String>)>,
    ) {
        names.extend(more);
        writer.set_context_names(names.clone());
    }

    fn write_variants(writer: &mut Writer, names: &mut HashMap<RecordId, Vec<String>>) {
        use crate::{
            context::AdminContextTuple,
            pack::RecordContext,
            record::{PlaceLayer, PlaceRecord, Record},
        };

        let at = [-100.0, 20.0];
        let mut locality = |name: &str, object_id: i64| {
            writer
                .write(
                    &Record::Place(
                        PlaceLayer::Locality,
                        PlaceRecord {
                            alt_names: Vec::new(),
                            name: name.to_string(),
                            place_type: "city".to_string(),
                            geometry: point_geometry(at[0], at[1]),
                            source: SourceProvenance::osm(ObjectType::Node, object_id),
                            prominence: Default::default(),
                            wikidata: None,
                        },
                    ),
                    None,
                )
                .expect("locality")
        };
        let guelph = locality("Guelph", 1100);
        let brampton = locality("Brampton", 1101);
        extend_context_names(
            writer,
            names,
            [
                (guelph, vec!["Guelph".to_string()]),
                (brampton, vec!["Brampton".to_string()]),
            ],
        );
        let in_locality = |record_id| {
            Some(RecordContext {
                admin_context: AdminContextTuple::from_tiers([(PlaceLayer::Locality, record_id)]),
                flags: 0,
                ..Default::default()
            })
        };
        writer
            .write(
                &located(
                    address_record("osm:node:1001", "", "10", "King St W", None, None),
                    at,
                )
                .into(),
                in_locality(guelph),
            )
            .expect("guelph address");
        writer
            .write(
                &located(
                    address_record("osm:node:1002", "", "10", "King Street West", None, None),
                    at,
                )
                .into(),
                in_locality(brampton),
            )
            .expect("brampton address");
    }

    #[test]
    fn geocode_tries_expanded_variants_when_earlier_hits_fail_the_context() {
        let searcher = TextSearcher::open(&*AREAS).expect("searcher");

        assert_eq!(
            geocode(&searcher, "10 King St W", Some("Brampton"), None).as_deref(),
            Some("osm:node:1002")
        );
    }

    static PLACES: LazyLock<TempDir> = LazyLock::new(|| corpus("search-places", write_places));

    const FAMED_TOWNS: usize = rank_window(FAME_WINDOW) + 8;

    static FAMED: LazyLock<TempDir> = LazyLock::new(|| corpus("search-famed", write_famed_towns));

    fn famed_town(at: usize) -> String {
        format!("Velmora {at:03}")
    }

    fn write_famed_towns(writer: &mut Writer) {
        use crate::record::{PlaceLayer, PlaceRecord, Prominence, Record};

        for at in 0..FAMED_TOWNS {
            let importance = u8::try_from(40 + at).expect("importance fits a byte");
            writer
                .write(
                    &Record::Place(
                        PlaceLayer::Locality,
                        PlaceRecord {
                            alt_names: Vec::new(),
                            name: famed_town(at),
                            place_type: "town".to_string(),
                            geometry: point_geometry(10.0 + at as f64 / 100.0, 50.0),
                            source: SourceProvenance::osm(
                                ObjectType::Node,
                                i64::try_from(7_000 + at).expect("id"),
                            ),
                            prominence: Prominence {
                                importance,
                                linked: true,
                                languages: 0,
                            },
                            wikidata: Some(u32::try_from(9_000 + at).expect("item")),
                        },
                    ),
                    None,
                )
                .expect("town");
        }
    }

    fn write_places(writer: &mut Writer) {
        use crate::{
            context::AdminContextTuple,
            pack::RecordContext,
            record::{PlaceLayer, PlaceRecord, PoiRecord, Record},
        };

        let township = writer
            .write(
                &Record::Place(
                    PlaceLayer::Locality,
                    PlaceRecord {
                        alt_names: Vec::new(),
                        name: "Muskoka Lakes Township".to_string(),
                        place_type: "admin_level:8".to_string(),
                        geometry: point_geometry(-79.5, 45.1),
                        source: SourceProvenance::osm(ObjectType::Relation, 101),
                        prominence: Default::default(),
                        wikidata: None,
                    },
                ),
                None,
            )
            .expect("township");
        writer.set_context_names(
            [(township, vec!["Muskoka Lakes Township".to_string()])]
                .into_iter()
                .collect(),
        );
        let in_township = Some(RecordContext {
            admin_context: AdminContextTuple::from_tiers([(PlaceLayer::Locality, township)]),
            flags: 0,
            ..Default::default()
        });
        let poi = |object_id, name: &str, address| PoiRecord {
            alt_names: Vec::new(),
            name: name.to_string(),
            category: "amenity:fire_station".to_string(),
            address,
            geometry: point_geometry(-79.5, 45.1),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, object_id),
            prominence: Default::default(),
            wikidata: None,
            brand: None,
            details: None,
            aliases: Vec::new(),
        };
        writer
            .write(
                &poi(102, "Muskoka Lakes Township Fire Station", None).into(),
                in_township,
            )
            .expect("fire station");
        let mut lowther = address_record(
            "osm:node:103",
            "",
            "4",
            "Lowther Avenue",
            Some("Toronto"),
            None,
        );
        lowther.geometry = point_geometry(-80.5, 44.5);
        writer.write(&lowther.into(), None).expect("address");
        let parkette_address = address_record(
            "osm:node:104",
            "",
            "227",
            "Lowther Avenue",
            Some("Toronto"),
            None,
        )
        .address;
        writer
            .write(
                &poi(104, "Lowther Avenue Parkette", Some(parkette_address)).into(),
                None,
            )
            .expect("parkette");

        let mut street = street_record("osm:way:201", "Walmer Road");
        street.geometry = queen_street_line();
        writer.write(&street.into(), None).expect("street");
        writer
            .write(
                &address_record(
                    "osm:node:204",
                    "",
                    "4",
                    "Walmer Road",
                    Some("Toronto"),
                    None,
                )
                .into(),
                None,
            )
            .expect("address");
        let poi = |object_id, name: &str, number: Option<&str>, lon: f64| PoiRecord {
            alt_names: Vec::new(),
            name: name.to_string(),
            category: "shop:convenience".to_string(),
            address: number.map(|number| {
                address_record(
                    "osm:node:0",
                    "",
                    number,
                    "Walmer Road",
                    Some("Toronto"),
                    None,
                )
                .address
            }),
            geometry: point_geometry(lon, 43.0005),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, object_id),
            prominence: Default::default(),
            wikidata: None,
            brand: None,
            details: None,
            aliases: Vec::new(),
        };
        for record in [
            poi(210, "Walmer Road Walmer Road Variety", Some("6"), -79.0),
            poi(211, "Corner Store", Some("4"), -79.0),
            poi(212, "Walmer Road Bakery", None, -79.3),
        ] {
            writer.write(&record.into(), None).expect("poi");
        }

        let known = Prominence {
            importance: 160,
            linked: true,
            languages: 100,
        };
        let city = |object_id, prominence| {
            Record::Place(
                PlaceLayer::Locality,
                PlaceRecord {
                    alt_names: Vec::new(),
                    name: "Paris".to_string(),
                    place_type: "city".to_string(),
                    geometry: point_geometry(2.35, 48.86),
                    source: SourceProvenance::osm(ObjectType::Node, object_id),
                    prominence,
                    wikidata: None,
                },
            )
        };
        let cafe = |object_id, prominence| {
            Record::from(PoiRecord {
                alt_names: Vec::new(),
                name: "Paris".to_string(),
                category: "amenity:cafe".to_string(),
                address: None,
                geometry: point_geometry(-75.0, 40.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, object_id),
                prominence,
                wikidata: None,
                brand: None,
                details: None,
                aliases: Vec::new(),
            })
        };
        for id in 10_002..=(10_000 + TIE_WINDOW as i64 + 100) {
            let mut street = street_record(&format!("osm:way:{id}"), "Paris");
            street.geometry = point_geometry(-75.0, 40.0);
            street.representative_point = [-75.0, 40.0];
            writer.write(&street.into(), None).expect("street");
        }
        for id in 10_002..=(10_001 + NAMED_PLACES_WINDOW as i64) {
            writer
                .write(&cafe(id, Prominence::default()), None)
                .expect("cafe");
        }
        writer
            .write(&city(10_001, Prominence::default()), None)
            .expect("benin");
        writer.write(&city(19_999, known), None).expect("france");
        writer
            .write(&cafe(19_998, known), None)
            .expect("known cafe");

        let fame = |importance| Prominence {
            importance,
            linked: true,
            languages: 10,
        };
        let place = |object: (ObjectType, i64),
                     name: &str,
                     alt_names: &[&str],
                     [lon, lat]: [f64; 2],
                     importance,
                     wikidata| {
            Record::Place(
                PlaceLayer::Locality,
                PlaceRecord {
                    alt_names: alt_names.iter().map(|name| name.to_string()).collect(),
                    name: name.to_string(),
                    place_type: "city".to_string(),
                    geometry: point_geometry(lon, lat),
                    source: SourceProvenance::osm(object.0, object.1),
                    prominence: fame(importance),
                    wikidata,
                },
            )
        };
        let roma = [12.48, 41.89];
        let georgia = [-85.16, 34.26];
        let mut rome_street = street_record("osm:way:404", "Rome");
        rome_street.geometry = point_geometry(-86.0, 36.0);
        rome_street.representative_point = [-86.0, 36.0];
        for record in [
            place(
                (ObjectType::Node, 401),
                "Roma",
                &["Rome", "Rom"],
                roma,
                160,
                Some(220),
            ),
            place(
                (ObjectType::Relation, 402),
                "Roma",
                &["Rome"],
                roma,
                160,
                Some(220),
            ),
            place((ObjectType::Node, 403), "Rome", &[], georgia, 60, None),
            place(
                (ObjectType::Node, 405),
                "Lagos",
                &[],
                [3.39, 6.46],
                154,
                Some(8673),
            ),
            place(
                (ObjectType::Node, 406),
                "Laos",
                &["Lagos"],
                [103.4, 20.0],
                163,
                Some(819),
            ),
            Record::from(rome_street),
        ] {
            writer.write(&record, None).expect("record");
        }

        let colosseo = |object_type, object_id, wikidata| {
            Record::from(PoiRecord {
                alt_names: vec!["Colosseum".to_string()],
                name: "Colosseo".to_string(),
                category: "tourism:attraction".to_string(),
                address: None,
                geometry: point_geometry(12.49, 41.89),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(object_type, object_id),
                prominence: Prominence {
                    importance: 150,
                    linked: true,
                    languages: 50,
                },
                wikidata,
                brand: None,
                details: None,
                aliases: Vec::new(),
            })
        };
        for record in [
            colosseo(ObjectType::Node, 501, Some(10285)),
            colosseo(ObjectType::Way, 502, Some(10285)),
            colosseo(ObjectType::Node, 503, None),
        ] {
            writer.write(&record, None).expect("poi");
        }

        let cafe = |object_id, prominence| PoiRecord {
            alt_names: Vec::new(),
            name: "Blue Door".to_string(),
            category: "amenity:cafe".to_string(),
            address: None,
            geometry: point_geometry(-74.0, 41.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, object_id),
            prominence,
            wikidata: None,
            brand: None,
            details: None,
            aliases: Vec::new(),
        };
        for record in [
            cafe(601, Prominence::default()),
            cafe(
                602,
                Prominence {
                    linked: false,
                    languages: 3,
                    importance: 0,
                },
            ),
            cafe(
                603,
                Prominence {
                    linked: true,
                    languages: 1,
                    importance: 0,
                },
            ),
        ] {
            writer.write(&record.into(), None).expect("cafe");
        }

        let poi = |object_id, name: &str, category: &str, [lon, lat]: [f64; 2]| PoiRecord {
            alt_names: Vec::new(),
            name: name.to_string(),
            category: category.to_string(),
            address: None,
            geometry: point_geometry(lon, lat),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, object_id),
            prominence: Default::default(),
            wikidata: None,
            brand: None,
            details: None,
            aliases: Vec::new(),
        };
        for record in [
            poi(701, "Apotheek Dam", "amenity:pharmacy", [4.89, 52.37]),
            poi(702, "Pharmacy Neumann", "amenity:pharmacy", [4.71, 52.01]),
            poi(703, "Pharmacy", "amenity:pharmacy", [6.57, 51.33]),
            poi(704, "Etos", "shop:chemist", [4.89, 52.37]),
            poi(705, "Pharmacy Cafe", "amenity:cafe", [4.89, 52.371]),
            poi(706, "Snackbar", "amenity:fast_food", [4.891, 52.37]),
        ] {
            writer.write(&record.into(), None).expect("poi");
        }

        let mut extents = Vec::new();
        let town = |object_type, prominence| {
            Record::Place(
                PlaceLayer::Locality,
                PlaceRecord {
                    alt_names: Vec::new(),
                    name: "Exeter".to_string(),
                    place_type: "town".to_string(),
                    geometry: point_geometry(-71.6, 41.58),
                    source: SourceProvenance::osm(object_type, 807),
                    prominence,
                    wikidata: Some(7),
                },
            )
        };
        let known = Prominence {
            importance: 40,
            linked: true,
            languages: 10,
        };
        writer
            .write(&town(ObjectType::Node, known), None)
            .expect("node");
        let boundary = writer
            .write(&town(ObjectType::Relation, Prominence::default()), None)
            .expect("boundary");
        extents.push((
            boundary,
            [-717_900_000, 415_000_000, -715_000_000, 416_000_000],
        ));

        let station = |object_type, name: &str| {
            Record::Poi(PoiRecord {
                alt_names: Vec::new(),
                name: name.to_string(),
                category: "railway:station".to_string(),
                address: None,
                geometry: point_geometry(-71.41, 41.82),
                location_precision: crate::record::LocationPrecision::Point,
                source: SourceProvenance::osm(object_type, 909),
                prominence: Prominence {
                    importance: 10,
                    linked: true,
                    languages: 2,
                },
                wikidata: Some(9),
                brand: None,
                details: None,
                aliases: Vec::new(),
            })
        };
        writer
            .write(&station(ObjectType::Node, "Providence"), None)
            .expect("node");
        let building = writer
            .write(&station(ObjectType::Way, "Union Station Providence"), None)
            .expect("building");
        extents.push((
            building,
            [-714_200_000, 418_100_000, -714_000_000, 418_300_000],
        ));
        writer.set_extents(extents);

        let town = |object_id, [lon, lat]: [f64; 2], prominence| {
            Record::Place(
                PlaceLayer::Locality,
                PlaceRecord {
                    alt_names: Vec::new(),
                    name: "Amsterdam".to_string(),
                    place_type: "town".to_string(),
                    geometry: point_geometry(lon, lat),
                    source: SourceProvenance::osm(ObjectType::Node, object_id),
                    prominence,
                    wikidata: None,
                },
            )
        };
        let known = Prominence {
            importance: 40,
            linked: true,
            languages: 100,
        };
        writer
            .write(&town(1001, [4.89, 52.37], known), None)
            .expect("netherlands");
        writer
            .write(&town(1002, [-111.3, 45.8], Prominence::default()), None)
            .expect("montana");

        writer
            .write(
                &Record::Place(
                    PlaceLayer::Locality,
                    PlaceRecord {
                        alt_names: vec!["Florence".to_string()],
                        name: "Firenze".to_string(),
                        place_type: "city".to_string(),
                        geometry: point_geometry(-82.0, 42.0),
                        source: SourceProvenance::osm(ObjectType::Node, 1101),
                        prominence: Default::default(),
                        wikidata: None,
                    },
                ),
                None,
            )
            .expect("firenze");
        let mut queen = street_record("osm:way:1102", "Queen Street");
        queen.geometry = point_geometry(-82.0, 42.0);
        queen.representative_point = [-82.0, 42.0];
        writer.write(&queen.into(), None).expect("street");

        let mut dental = address_record(
            "osm:node:1201",
            "",
            "300",
            "St. Clair Avenue West",
            Some("Toronto"),
            None,
        );
        dental.geometry = point_geometry(-81.0, 44.0);
        writer
            .write(&dental.clone().into(), None)
            .expect("dental address");
        writer
            .write(
                &PoiRecord {
                    alt_names: Vec::new(),
                    name: "St. Clair Spadina Dental".to_string(),
                    category: "amenity:dentist".to_string(),
                    address: Some(dental.address),
                    geometry: point_geometry(-79.41, 43.68),
                    location_precision: LocationPrecision::Point,
                    source: SourceProvenance::osm(ObjectType::Node, 1201),
                    prominence: Default::default(),
                    wikidata: None,
                    brand: None,
                    details: None,
                    aliases: Vec::new(),
                }
                .into(),
                None,
            )
            .expect("poi");
        let mut spadina = address_record("osm:node:1202", "", "300", "Spadina Avenue", None, None);
        spadina.geometry = point_geometry(-81.0, 44.0);
        writer.write(&spadina.into(), None).expect("address");

        let place = |id: i64, name: &str, importance: u8, lon: f64| PoiRecord {
            alt_names: if name == "Luchthaven Schiphol" {
                vec!["AMS".to_string()]
            } else {
                Vec::new()
            },
            name: name.to_string(),
            category: "tourism:museum".to_string(),
            address: None,
            geometry: point_geometry(lon, 52.36),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, id),
            prominence: Prominence {
                importance,
                ..Prominence::default()
            },
            wikidata: None,
            brand: None,
            details: None,
            aliases: Vec::new(),
        };
        for record in [
            place(1301, "Stedelijk Museum Breda", 0, 4.78),
            place(1302, "Stedelijk Museum Amsterdam", 120, 4.88),
            place(1303, "Van Gogh Museum", 140, 4.881),
            place(1304, "AMS", 0, 4.882),
            place(1305, "Luchthaven Schiphol", 160, 4.76),
        ] {
            writer.write(&record.into(), None).expect("place");
        }
        writer
            .write(
                &address_record(
                    "osm:node:1309",
                    "",
                    "1",
                    "Van Goghstraat",
                    Some("Amsterdam"),
                    None,
                )
                .into(),
                None,
            )
            .expect("address");
    }

    #[test]
    fn pois_named_after_a_place_or_street_are_found_apart_from_it() {
        let searcher = TextSearcher::open(&*PLACES).expect("searcher");
        let top = |query: &str| {
            searcher
                .search(&TextSearchOptions {
                    query: query.to_string(),
                    limit: 5,
                    layer: None,
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                })
                .expect("search")[0]
                .record
                .id
                .clone()
        };

        assert_eq!(top("Muskoka Lakes Township"), "osm:relation:101");
        assert_eq!(top("Lowther Avenue"), "osm:node:103");
        let places = searcher.places();
        let place = |query: &str| top_ids(&places, query)[0].clone();
        assert_eq!(place("Lowther Avenue Parkette"), "osm:node:104");
        assert_eq!(
            place("Fire Station, Muskoka Lakes Township"),
            "osm:node:102"
        );
    }

    #[test]
    fn address_search_answers_no_poi_and_place_search_finds_them() {
        let searcher = TextSearcher::open(&*PLACES).expect("searcher");
        let places = searcher.places();
        let layers_of = |searcher: &TextSearcher, query: &str| {
            searcher
                .search(&TextSearchOptions {
                    query: query.to_string(),
                    limit: 5,
                    layer: None,
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                })
                .expect("search")
                .into_iter()
                .map(|hit| (hit.record.id, hit.record.layer))
                .collect::<Vec<_>>()
        };
        let no_poi = |hits: &[(String, String)]| hits.iter().all(|(_, layer)| layer != "poi");

        let street = layers_of(&searcher, "Walmer Road");
        assert!(no_poi(&street), "{street:?}");
        let numbered = layers_of(&searcher, "4 Walmer Road");
        assert_eq!(numbered[0].0, "osm:node:204", "{numbered:?}");
        assert!(no_poi(&numbered), "{numbered:?}");
        assert!(no_poi(&layers_of(&searcher, "Walmer Road Bakery")));
        let confirmed = layers_of(&places, "Corner Store 4 Walmer Road");
        assert_eq!(confirmed[0].0, "osm:node:211", "{confirmed:?}");
        assert_eq!(
            layers_of(&places, "Walmer Road Bakery")[0].0,
            "osm:node:212"
        );
        assert_eq!(
            layers_of(&places, "Walmer Road Variety")[0].0,
            "osm:node:210"
        );
    }

    #[test]
    fn a_place_behind_more_same_named_streets_than_the_tie_window_answers() {
        let searcher = TextSearcher::open(&*PLACES).expect("searcher");
        assert_eq!(
            top_ids(&searcher, "Paris")[..2],
            ["osm:node:19999", "osm:node:10001"]
        );
    }

    #[test]
    fn the_best_known_of_more_same_named_pois_than_the_name_window_wins() {
        let searcher = TextSearcher::open(&*PLACES).expect("searcher");
        assert_eq!(top_ids(&searcher.places(), "Paris")[0], "osm:node:19998");
    }

    #[test]
    fn a_place_named_by_any_of_its_names_leads_by_fame() {
        let searcher = TextSearcher::open(&*PLACES).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "Rome")[..2],
            ["osm:node:401", "osm:node:403"],
            "Roma mapped twice answers once, ahead of the lesser Rome and the Rome street"
        );
        assert_eq!(top_ids_limited(&searcher, "Rome", 1), ["osm:node:401"]);
        assert_eq!(
            top_ids(&searcher, "Lagos"),
            ["osm:node:405", "osm:node:406"],
            "the city named Lagos before Laos, which only has it as another name"
        );
        let near = searcher
            .search(&TextSearchOptions {
                query: "Rome".to_string(),
                limit: 1,
                layer: None,
                countries: Vec::new(),
                focus: Some([-85.16, 34.26]),
                bbox: None,
            })
            .expect("search");
        assert_eq!(
            near.iter()
                .map(|hit| hit.record.id.as_str())
                .collect::<Vec<_>>(),
            ["osm:node:403"],
            "the Rome nearest the caller"
        );
    }

    #[test]
    fn famous_places_read_from_the_famed_copy_match_the_whole_index() {
        let searcher = TextSearcher::open(&*PLACES).expect("searcher");
        let places = searcher.places();
        let famous = |searcher: &TextSearcher, query: &str| {
            let options = TextAutocompleteOptions {
                query: query.to_string(),
                limit: DEFAULT_SEARCH_LIMIT,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            };
            searcher
                .famous_places(&options, true)
                .expect("famous places")
                .into_iter()
                .map(|hit| hit.record_id)
                .collect::<Vec<_>>()
        };
        let queries = [
            "Ro",
            "Rom",
            "Rome",
            "Lag",
            "Lagos",
            "Col",
            "Colosseum",
            "Rom ita",
        ];
        let whole = queries
            .iter()
            .map(|query| (famous(&searcher, query), famous(&places, query)))
            .collect::<Vec<_>>();
        assert!(whole.iter().any(|(addresses, _)| !addresses.is_empty()));
        assert!(searcher.prepare_famed().expect("famed copy") > 0);
        let copied = queries
            .iter()
            .map(|query| (famous(&searcher, query), famous(&places, query)))
            .collect::<Vec<_>>();
        assert_eq!(copied, whole);
    }

    #[test]
    fn a_famed_copy_that_fills_the_window_answers_as_the_whole_index() {
        let searcher = TextSearcher::open(&*FAMED).expect("searcher");
        let options = TextAutocompleteOptions {
            query: "Velmora".to_string(),
            limit: DEFAULT_SEARCH_LIMIT,
            layer: None,
            countries: Vec::new(),
            focus: None,
            bbox: None,
        };
        let names = |searcher: &TextSearcher| {
            searcher
                .famous_places(&options, true)
                .expect("famous places")
                .into_iter()
                .map(|hit| hit.record.name)
                .collect::<Vec<_>>()
        };
        let whole = names(&searcher);
        assert_eq!(
            searcher.prepare_famed().expect("famed copy"),
            FAMED_TOWNS as u64
        );
        let famed = searcher.famed_corpus();
        let query = [Completion::CommonCompletions, Completion::InOrder]
            .into_iter()
            .find_map(|completion| {
                searcher
                    .autocomplete_query(
                        completion,
                        None,
                        &["velmora".to_string()],
                        true,
                        &Filters::new(None, &[], None),
                    )
                    .expect("query")
            })
            .expect("a query");
        let ranking = Ranking {
            fame_first: true,
            typed_words: 1,
            ..Ranking::default()
        };
        assert_eq!(
            searcher
                .top_documents(&famed, &*query, &ranking, rank_window(FAME_WINDOW))
                .expect("famed window")
                .len(),
            rank_window(FAME_WINDOW),
            "the copy fills the window, so the whole index is not read"
        );
        let copied = names(&searcher);
        assert_eq!(copied, whole);
        assert_eq!(copied.first(), Some(&famed_town(FAMED_TOWNS - 1)));
    }

    #[test]
    fn a_poi_mapped_twice_answers_once() {
        let places = TextSearcher::open(&*PLACES).expect("searcher").places();
        assert_eq!(
            top_ids(&places, "Colosseum"),
            ["osm:node:501", "osm:node:503"]
        );
    }

    #[test]
    fn equal_pois_prefer_the_better_known() {
        let searcher = TextSearcher::open(&*PLACES).expect("searcher").places();
        assert_eq!(
            top_ids(&searcher, "Blue Door"),
            ["osm:node:603", "osm:node:602", "osm:node:601"]
        );

        let first = TieBreak::new(
            &street_summary(),
            Prior {
                place_rank: None,
                prominence: None,
                disputed: false,
            },
            &HashSet::new(),
            &Ranking::default(),
        );
        let known = TieBreak::new(
            &Summary {
                layer: Layer::Poi.as_str().to_string(),
                ..street_summary()
            },
            Prior {
                place_rank: None,
                prominence: Some(Prominence {
                    linked: true,
                    languages: 40,
                    importance: 0,
                }),
                disputed: false,
            },
            &HashSet::new(),
            &Ranking::default(),
        );
        assert!(first < known);
    }

    #[test]
    fn suggestions_mix_addresses_and_places_by_what_the_query_says() {
        let hit = |id: &str, layer: &str, name: &str| TextSearchHit {
            named_place: false,
            record_id: 0,
            score: 1.0,
            record: Summary {
                id: id.to_string(),
                layer: layer.to_string(),
                name: name.to_string(),
                label: name.to_string(),
                ..street_summary()
            },
            fallback: false,
            features: Features::default(),
            confidence: None,
            signals: Signals::default(),
            matched: QueryMatch::default(),
            highlights: Vec::new(),
        };
        let addresses = || {
            vec![
                hit("osm:node:1", "address", "10 Queen Street"),
                hit("osm:way:2", "street", "Queen Street"),
            ]
        };
        let places = || {
            vec![
                hit("osm:node:3", "poi", "Corner Store"),
                hit("osm:node:4", "poi", "Queen Bakery"),
            ]
        };
        let ids = |hits: Vec<TextSearchHit>| {
            hits.into_iter()
                .map(|hit| hit.record.id)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            ids(mix_suggestions("queen", addresses(), places(), 0)),
            ["osm:node:4", "osm:node:1", "osm:way:2", "osm:node:3"],
            "a place named as typed first, then the addresses"
        );
        assert_eq!(
            ids(mix_suggestions("10 queen", addresses(), places(), 0)),
            ["osm:node:1", "osm:way:2", "osm:node:3", "osm:node:4"],
            "a house number asks for an address"
        );
        assert_eq!(mix_suggestions("queen", addresses(), places(), 2).len(), 2);
    }

    #[test]
    fn a_part_typed_near_the_caller_answers_what_is_near_first() {
        let hit = |id: &str, layer: &str, name: &str, focus_band: u8| TextSearchHit {
            named_place: false,
            record_id: 0,
            score: 1.0,
            record: Summary {
                id: id.to_string(),
                layer: layer.to_string(),
                name: name.to_string(),
                label: name.to_string(),
                ..street_summary()
            },
            fallback: false,
            features: Features::default(),
            confidence: None,
            signals: Signals {
                focus_band,
                ..Signals::default()
            },
            matched: QueryMatch::default(),
            highlights: Vec::new(),
        };
        let addresses = vec![hit("osm:way:1", "street", "Rest Area", 9)];
        let places = vec![
            hit("osm:node:2", "poi", "Restaurante Tio Pepe", 2),
            hit("osm:node:3", "poi", "Samos Restaurant", 2),
            hit("osm:node:4", "poi", "Far Restaurant", 9),
        ];
        let ids = mix("rest", addresses.clone(), places.clone(), 0, true, false)
            .into_iter()
            .map(|hit| hit.record.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, ["osm:node:2", "osm:node:3", "osm:way:1", "osm:node:4"]);
        let unfocused = mix("rest", addresses, places, 0, false, false)
            .into_iter()
            .map(|hit| hit.record.id)
            .collect::<Vec<_>>();
        assert_eq!(
            unfocused,
            ["osm:node:2", "osm:way:1", "osm:node:3", "osm:node:4"]
        );
    }

    fn street_summary() -> Summary {
        Summary {
            id: "osm:way:1".to_string(),
            layer: "street".to_string(),
            name: "Blue Door".to_string(),
            label: "Blue Door".to_string(),
            address: Address::default(),
            category: None,
            road_class: None,
            brand: None,
            contact: None,
            opening_hours: None,
            cuisine: Vec::new(),
            bbox: None,
            timezone: None,
            street_postcode: None,
            access: Vec::new(),
            building: None,
            point: None,
            source: Source {
                dataset: "osm".to_string(),
                object_type: Some(ObjectType::Way),
                object_id: Some(1),
                derived_from: None,
                record_count: None,
            },
        }
    }

    static BASICS: LazyLock<TempDir> = LazyLock::new(|| corpus("search-basics", write_basics));

    fn write_basics(writer: &mut Writer) {
        for record in [
            address_record(
                "osm:node:1",
                "10 King Street, Toronto",
                "10",
                "King Street",
                Some("Toronto"),
                Some("M5V 1A1"),
            ),
            address_record(
                "osm:node:2",
                "20 Queen Street, Toronto",
                "20",
                "Queen Street",
                Some("Toronto"),
                None,
            ),
            address_record(
                "osm:node:11",
                "10 Dufferin Street, Toronto",
                "10",
                "Dufferin Street",
                Some("Toronto"),
                None,
            ),
            address_record(
                "osm:node:21",
                "10 Spadina Avenue, Toronto",
                "10",
                "Spadina Avenue",
                Some("Toronto"),
                None,
            ),
            address_record(
                "osm:node:31",
                "221 Baker Street, London, NW1",
                "221",
                "Baker Street",
                Some("London"),
                Some("NW1 6XE"),
            ),
            address_record("osm:node:41", "", "10", "Bloor Street", None, None),
        ] {
            writer.write(&record.into(), None).expect("address");
        }
        for (id, name) in [
            ("osm:way:19", "Dufferin Street"),
            ("osm:way:29", "Spadina Avenue"),
            ("osm:way:49", "Bloor Street"),
        ] {
            writer
                .write(&street_record(id, name).into(), None)
                .expect("street");
        }
        writer
            .write(
                &PostcodeRecord {
                    postcode: "V6B".to_string(),
                    country: None,
                    geometry: point_geometry(-123.1, 49.28),
                    source: DerivedSourceProvenance::osm_address_records(2),
                }
                .into(),
                None,
            )
            .expect("write postcode");
    }

    #[test]
    fn search_reports_queries_that_cannot_be_parsed() {
        let searcher = TextSearcher::open(&*BASICS).expect("searcher");

        let error = searcher
            .search(&TextSearchOptions {
                query: "(".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect_err("unparseable query");
        assert!(matches!(error, Error::UnparsableQuery { .. }));
    }

    #[test]
    fn filters_hits_by_layer() {
        let searcher = TextSearcher::open(&*BASICS).expect("searcher");
        let hits = searcher
            .search(&TextSearchOptions {
                query: "Dufferin Street".to_string(),
                limit: 10,
                layer: Some("street".to_string()),
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search");

        assert_eq!(
            hits.iter()
                .map(|hit| hit.record.id.as_str())
                .collect::<Vec<_>>(),
            ["osm:way:19"]
        );
        assert_eq!(hits[0].record.layer, "street");
    }

    #[test]
    fn searches_postcode_text() {
        let searcher = TextSearcher::open(&*BASICS).expect("searcher");
        let hits = searcher
            .search(&TextSearchOptions {
                query: "V6B".to_string(),
                limit: 5,
                layer: Some("postcode".to_string()),
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search");

        assert_eq!(
            hits.iter()
                .map(|hit| hit.record.id.as_str())
                .collect::<Vec<_>>(),
            ["derived:osm:postcode:V6B"]
        );
        assert_eq!(hits[0].record.layer, "postcode");
    }

    #[test]
    fn autocompletes_prefixes_and_hydrates_records_from_pack() {
        let searcher = TextSearcher::open(&*BASICS).expect("searcher");
        let hits = searcher
            .autocomplete(&TextAutocompleteOptions {
                query: "kin".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("autocomplete");

        assert!(!hits.is_empty());
        assert_eq!(hits[0].record.label, "10 King Street, Toronto, M5V 1A1");
    }

    #[test]
    fn autocompletes_with_layer_filter() {
        let searcher = TextSearcher::open(&*BASICS).expect("searcher");
        let hits = searcher
            .autocomplete(&TextAutocompleteOptions {
                query: "spa".to_string(),
                limit: 10,
                layer: Some("street".to_string()),
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("autocomplete");

        assert_eq!(
            hits.iter()
                .map(|hit| hit.record.id.as_str())
                .collect::<Vec<_>>(),
            ["osm:way:29"]
        );
        assert_eq!(hits[0].record.layer, "street");
    }

    #[test]
    fn autocompletes_postcode_but_not_standalone_house_number_prefixes() {
        let searcher = TextSearcher::open(&*BASICS).expect("searcher");
        let postcode_hits = searcher
            .autocomplete(&TextAutocompleteOptions {
                query: "nw16".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("postcode autocomplete");
        let number_hits = searcher
            .autocomplete(&TextAutocompleteOptions {
                query: "221".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("number autocomplete");

        assert_eq!(postcode_hits.len(), 1);
        assert!(number_hits.is_empty());
        assert_eq!(postcode_hits[0].record.id, "osm:node:31");
    }

    static AUTOCOMPLETE: LazyLock<TempDir> =
        LazyLock::new(|| corpus("search-autocomplete", write_autocomplete));

    fn write_autocomplete(writer: &mut Writer) {
        use crate::record::{
            InterpolationAddressComponents, InterpolationRange, InterpolationRecord,
        };

        fn poi(object_id: i64, name: &str, category: &str, [lon, lat]: [f64; 2]) -> PoiRecord {
            PoiRecord {
                alt_names: Vec::new(),
                name: name.to_string(),
                category: category.to_string(),
                address: None,
                geometry: point_geometry(lon, lat),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, object_id),
                prominence: Default::default(),
                wikidata: None,
                brand: None,
                details: None,
                aliases: Vec::new(),
            }
        }
        fn town(
            object_id: i64,
            name: &str,
            [lon, lat]: [f64; 2],
            prominence: Prominence,
        ) -> Record {
            Record::Place(
                PlaceLayer::Locality,
                crate::record::PlaceRecord {
                    alt_names: Vec::new(),
                    name: name.to_string(),
                    place_type: "city".to_string(),
                    geometry: point_geometry(lon, lat),
                    source: SourceProvenance::osm(ObjectType::Node, object_id),
                    prominence,
                    wikidata: None,
                },
            )
        }
        let cafe = "amenity:cafe";

        let seattle = [-122.34, 47.61];
        for (object_id, name, number, street, area) in [
            (1001, "Tullys", "915", "Northwest 45th Street", "Ballard"),
            (1002, "Tullys", "3400", "Fremont Avenue North", "Fremont"),
            (
                1003,
                "Fred Meyer",
                "915",
                "Northwest 45th Street",
                "Ballard",
            ),
            (1004, "Pike Place Market", "85", "Pike Street", "Seattle"),
            (1005, "Pike Place Chowder", "1530", "Post Alley", "Seattle"),
        ] {
            let mut record = poi(object_id, name, cafe, seattle);
            record.address =
                Some(address_record("osm:node:0", "", number, street, Some(area), None).address);
            writer.write(&record.into(), None).expect("poi");
        }
        for (id, number, street) in [
            ("osm:node:1006", "2101", "North 34th Street"),
            ("osm:node:1007", "2101", "1st Avenue"),
        ] {
            let mut record = address_record(id, "", number, street, Some("Seattle"), None);
            record.geometry = point_geometry(-122.35, 47.62);
            writer.write(&record.into(), None).expect("address");
        }

        for (id, number, postcode) in [
            ("osm:node:2001", "12", "1017AA"),
            ("osm:node:2002", "29", "1211CK"),
        ] {
            let mut record = address_record(
                id,
                "",
                number,
                "Kerkstraat",
                Some("Amsterdam"),
                Some(postcode),
            );
            record.geometry = point_geometry(4.88, 52.36);
            writer.write(&record.into(), None).expect("address");
        }
        writer
            .write(
                &poi(2003, "Pier 39", "tourism:attraction", [-122.41, 37.81]).into(),
                None,
            )
            .expect("poi");
        let mut pier = address_record("osm:node:2004", "", "39", "Pier Street", None, None);
        pier.geometry = point_geometry(-89.0, 41.0);
        writer.write(&pier.into(), None).expect("pier street");

        let known = Prominence {
            importance: 30,
            linked: true,
            languages: 5,
        };
        for at in 0..MAX_RANK_WINDOW as i64 {
            let mut far = poi(at + 4010, "Greggs", cafe, [-1.0, 53.0]);
            far.prominence = known;
            writer.write(&far.into(), None).expect("far");
        }
        writer
            .write(&poi(4001, "Greggs", cafe, [13.40, 52.52]).into(), None)
            .expect("near");

        writer
            .write(&poi(5001, "Alki Point", cafe, [-87.26, 34.74]).into(), None)
            .expect("far");
        writer
            .write(
                &poi(5002, "Alki Point Fish Market", cafe, [-112.34, 47.61]).into(),
                None,
            )
            .expect("near");

        let known = Prominence {
            importance: 0,
            linked: true,
            languages: 40,
        };
        writer
            .write(&town(6001, "Paris", [2.35, 48.86], known), None)
            .expect("paris");
        writer
            .write(
                &town(6002, "Moxee", [-96.3, 33.3], Prominence::default()),
                None,
            )
            .expect("moxee");
        for (object_id, name) in [(6003, "Paris Bakery"), (6004, "Moxee Grocery")] {
            writer
                .write(
                    &poi(object_id, name, "shop:bakery", [-102.34, 47.61]).into(),
                    None,
                )
                .expect("shop");
        }

        writer
            .write(
                &Record::Place(
                    PlaceLayer::Region,
                    crate::record::PlaceRecord {
                        alt_names: Vec::new(),
                        name: "Amazonas".to_string(),
                        place_type: "state".to_string(),
                        geometry: point_geometry(-64.6, -4.2),
                        source: SourceProvenance::osm(ObjectType::Node, 7001),
                        prominence: Prominence {
                            importance: 0,
                            linked: true,
                            languages: 30,
                        },
                        wikidata: None,
                    },
                ),
                None,
            )
            .expect("region");
        let mut street = street_record("osm:way:7002", "Amazon Street");
        street.geometry = point_geometry(-107.34, 47.61);
        street.representative_point = [-107.34, 47.61];
        writer.write(&street.into(), None).expect("street");

        for (object_id, name) in [(9001, "Blackbirds"), (9002, "Gasworks Harbor Market")] {
            writer
                .write(&poi(object_id, name, cafe, [-117.34, 47.61]).into(), None)
                .expect("poi");
        }

        for (object_id, name, other) in [
            (10001, "Starbucks", &["Sbux"][..]),
            (10002, "Lake Shore Market", &[]),
            (10003, "Palais de l'Élysée", &[]),
            (10004, "Café Łódź", &[]),
        ] {
            let mut record = poi(object_id, name, cafe, [-127.34, 47.61]);
            record.alt_names = other.iter().map(|name| name.to_string()).collect();
            writer.write(&record.into(), None).expect("poi");
        }
        let mut house = address_record(
            "osm:node:10005",
            "",
            "10",
            "King Street",
            Some("Toronto"),
            None,
        );
        house.geometry = point_geometry(-127.35, 47.62);
        writer.write(&house.into(), None).expect("address");

        writer
            .write(&poi(11001, "Moonbuks", cafe, [127.02, 37.62]).into(), None)
            .expect("far");
        for object_id in 11002..11005 {
            writer
                .write(
                    &poi(object_id, "Moonbucks", cafe, [-92.34, 47.61]).into(),
                    None,
                )
                .expect("near");
        }

        for (object_id, name) in [(12001, "Sunbuks"), (12002, "Sunbucks"), (12003, "Sunbucks")] {
            writer
                .write(&poi(object_id, name, cafe, [129.0, 35.0]).into(), None)
                .expect("far");
        }
        let mut road = street_record("osm:way:12004", "Sunbucks Road");
        road.geometry = point_geometry(-84.0, 40.0);
        road.representative_point = [-84.0, 40.0];
        writer.write(&road.into(), None).expect("street");

        let street_name = "Haynes Street Southwest";
        let mut street = street_record("osm:way:13001", street_name);
        street.geometry = line(-84.4, [33.75, 33.76]);
        street.representative_point = [-84.4, 33.755];
        writer.write(&street.into(), None).expect("street");
        for (id, number, lat) in [
            ("osm:node:13011", "181", 33.751),
            ("osm:node:13012", "197", 33.753),
        ] {
            let mut record =
                address_record(id, "", number, street_name, Some("Atlanta"), Some("30313"));
            record.geometry = point_geometry(-84.4001, lat);
            writer.write(&record.into(), None).expect("address");
        }

        writer
            .write(
                &InterpolationRecord {
                    address: InterpolationAddressComponents {
                        street: Some("Queen Street West".to_string()),
                        locality: Some("Toronto Centre".to_string()),
                        place: None,
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
                    source: SourceProvenance::osm(ObjectType::Way, 14020),
                }
                .into(),
                None,
            )
            .expect("interpolation");
        writer
            .write(
                &address_record(
                    "osm:node:14021",
                    "",
                    "1",
                    "Bay Street",
                    Some("Toronto"),
                    None,
                )
                .into(),
                None,
            )
            .expect("locality evidence");
        let mut distant = address_record(
            "osm:node:14050",
            "",
            "50",
            "Queen Street West",
            Some("Ottawa"),
            None,
        );
        distant.geometry = point_geometry(-75.7, 45.4);
        writer
            .write(&distant.into(), None)
            .expect("distant address");
    }

    #[test]
    fn autocompletes_area_words_and_abbreviated_words() {
        let searcher = TextSearcher::open(&*AUTOCOMPLETE).expect("searcher");
        let places = searcher.places();
        let suggested = |query: &str| {
            suggest(
                &searcher,
                &places,
                &TextAutocompleteOptions {
                    query: query.to_string(),
                    limit: 5,
                    layer: None,
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                },
            )
            .expect("suggest")
            .into_iter()
            .map(|hit| hit.record.id)
            .collect::<Vec<_>>()
        };

        assert_eq!(suggested("tullys ballard"), ["osm:node:1001"]);
        assert_eq!(suggested("ballard tullys"), ["osm:node:1001"]);
        assert_eq!(suggested("fred meyer ballard"), ["osm:node:1003"]);
        assert_eq!(suggested("tullys ba"), ["osm:node:1001"]);
        assert_eq!(suggested("pike pl mar")[0], "osm:node:1004");
        assert_eq!(suggested("pike pl mkt")[0], "osm:node:1004");
        assert_eq!(suggested("2101 N 34th St")[0], "osm:node:1006");
        assert_eq!(suggested("2101 n 34")[0], "osm:node:1006");
        assert!(!suggested("ballard").contains(&"osm:node:1003".to_string()));
        assert_eq!(suggested("pike place mar"), ["osm:node:1004"]);
        assert_eq!(suggested("pike mkt"), ["osm:node:1004", "osm:node:1005"]);
    }

    #[test]
    fn autocompletes_a_house_number_after_its_street() {
        let searcher = TextSearcher::open(&*AUTOCOMPLETE).expect("searcher");
        let places = searcher.places();
        let first = |query: &str| {
            suggest(
                &searcher,
                &places,
                &TextAutocompleteOptions {
                    query: query.to_string(),
                    limit: 5,
                    layer: None,
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                },
            )
            .expect("suggest")
            .first()
            .map(|hit| hit.record.id.clone())
        };

        assert_eq!(first("kerkstraat 12").as_deref(), Some("osm:node:2001"));
        assert_eq!(first("12 kerkstraat").as_deref(), Some("osm:node:2001"));
        assert_eq!(
            first("kerkstraat 12 amsterdam").as_deref(),
            Some("osm:node:2001")
        );
        assert_eq!(
            first("kerkstraat 29 amst").as_deref(),
            Some("osm:node:2002")
        );
        assert_eq!(first("pier 39").as_deref(), Some("osm:node:2003"));
    }

    #[test]
    fn autocompletes_a_common_word_past_the_expansion_cap() {
        let temp_dir = TempDir::new("autocomplete-common-completion");
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let poi = |object_id, name: String| PoiRecord {
            alt_names: Vec::new(),
            name,
            category: "shop:convenience".to_string(),
            address: None,
            geometry: point_geometry(-122.34, 47.61),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, object_id),
            prominence: Default::default(),
            wikidata: None,
            brand: None,
            details: None,
            aliases: Vec::new(),
        };
        let cap = AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS as i64;
        for at in 0..cap + 76 {
            writer
                .write(&poi(at + 10, format!("Mara{at:04}")).into(), None)
                .expect("filler");
        }
        for (at, name) in ["Pike Place Market", "Fish Market", "Flower Market"]
            .into_iter()
            .enumerate()
        {
            writer
                .write(&poi(at as i64 + 1, name.to_string()).into(), None)
                .expect("market");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        let places = searcher.places();
        let options = TextAutocompleteOptions {
            query: "pike place mar".to_string(),
            limit: 5,
            layer: None,
            countries: Vec::new(),
            focus: None,
            bbox: None,
        };

        assert!(
            places
                .autocomplete_as(&options, Completion::InOrder)
                .expect("in order")
                .is_empty()
        );
        let found = suggest(&searcher, &places, &options).expect("suggest");
        assert_eq!(found[0].record.id, "osm:node:1");
    }

    #[test]
    fn autocompletes_the_branch_near_the_focus_before_known_far_ones() {
        let searcher = TextSearcher::open(&*AUTOCOMPLETE).expect("searcher");
        let options = TextAutocompleteOptions {
            query: "greggs".to_string(),
            limit: 5,
            layer: None,
            countries: Vec::new(),
            focus: Some([13.40, 52.52]),
            bbox: None,
        };

        let found = suggest(&searcher, &searcher.places(), &options).expect("suggest");
        assert_eq!(found[0].record.id, "osm:node:4001");
    }

    #[test]
    fn autocompletes_the_near_match_before_a_better_far_one() {
        let searcher = TextSearcher::open(&*AUTOCOMPLETE).expect("searcher");
        let first = |focus| {
            let options = TextAutocompleteOptions {
                query: "alki point".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: Some(focus),
                bbox: None,
            };
            suggest(&searcher, &searcher.places(), &options).expect("suggest")[0]
                .record
                .id
                .clone()
        };

        assert_eq!(first([-112.33, 47.61]), "osm:node:5002");
        assert_eq!(first([-113.4, 47.61]), "osm:node:5001");
    }

    #[test]
    fn autocompletes_a_known_town_typed_whole_before_near_shops() {
        let searcher = TextSearcher::open(&*AUTOCOMPLETE).expect("searcher");
        let first = |query: &str| {
            let options = TextAutocompleteOptions {
                query: query.to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: Some([-102.33, 47.61]),
                bbox: None,
            };
            suggest(&searcher, &searcher.places(), &options).expect("suggest")[0]
                .record
                .id
                .clone()
        };

        assert_eq!(first("paris"), "osm:node:6001");
        assert_eq!(first("moxee"), "osm:node:6004");
    }

    #[test]
    fn a_far_town_needs_its_whole_name_typed_to_lead() {
        let searcher = TextSearcher::open(&*AUTOCOMPLETE).expect("searcher");
        let options = TextAutocompleteOptions {
            query: "amazon".to_string(),
            limit: 1,
            layer: None,
            countries: Vec::new(),
            focus: Some([-107.33, 47.61]),
            bbox: None,
        };

        let found = suggest(&searcher, &searcher.places(), &options).expect("suggest");
        assert_eq!(found[0].record.id, "osm:way:7002");
    }

    #[test]
    fn finds_the_nearest_pois_of_a_category() {
        let places = TextSearcher::open(&*PLACES).expect("searcher").places();

        let ids = |bbox| {
            places
                .of_categories(
                    &["amenity pharmacy"],
                    Some(([4.89, 52.37], BIAS_RADIUS_M)),
                    5,
                    &[],
                    bbox,
                )
                .expect("nearest")
                .into_iter()
                .map(|hit| hit.record.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(None), ["osm:node:701", "osm:node:702"]);
        let amsterdam = BoundingBox {
            min_lon: 4.8,
            min_lat: 52.3,
            max_lon: 5.0,
            max_lat: 52.4,
        };
        assert_eq!(ids(Some(amsterdam)), ["osm:node:701"]);
        assert!(
            places
                .category_count(&["amenity fast_food"])
                .expect("count")
                > 0
        );
        assert_eq!(
            places
                .of_categories(
                    &["amenity fast_food"],
                    Some(([4.89, 52.37], BIAS_RADIUS_M)),
                    5,
                    &[],
                    None,
                )
                .expect("nearest")
                .into_iter()
                .map(|hit| hit.record.id)
                .collect::<Vec<_>>(),
            ["osm:node:706"]
        );
    }

    #[test]
    fn autocompletes_through_a_typo() {
        let searcher = TextSearcher::open(&*AUTOCOMPLETE).expect("searcher");
        let first = |query: &str| {
            let options = TextAutocompleteOptions {
                query: query.to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            };
            suggest(&searcher, &searcher.places(), &options)
                .expect("suggest")
                .first()
                .map(|hit| hit.record.id.clone())
        };

        assert_eq!(first("blackbirs").as_deref(), Some("osm:node:9001"));
        assert_eq!(first("blcakb").as_deref(), Some("osm:node:9001"));
        assert_eq!(
            first("gasworks harbro mark").as_deref(),
            Some("osm:node:9002")
        );
        assert_eq!(first("xlackbirds"), None);
    }

    #[test]
    fn a_suggestion_marks_where_its_label_has_what_was_typed() {
        let searcher = TextSearcher::open(&*AUTOCOMPLETE).expect("searcher");
        let first = |query: &str| {
            let options = TextAutocompleteOptions {
                query: query.to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            };
            let hit = suggest(&searcher, &searcher.places(), &options)
                .expect("suggest")
                .into_iter()
                .next()
                .expect("a suggestion");
            let ranges = hit
                .highlights
                .iter()
                .map(|range| (range.start, range.end))
                .collect::<Vec<_>>();
            (hit.record.label, ranges)
        };
        let (palace, cafe) = ("Palais de l'Élysée", "Café Łódź");
        let house = "10 King Street, Toronto";
        for (typed, label, ranges) in [
            ("starb", "Starbucks", vec![(0, 5)]),
            ("lake shore", "Lake Shore Market", vec![(0, 4), (5, 10)]),
            ("lake sh", "Lake Shore Market", vec![(0, 4), (5, 7)]),
            ("shore mar", "Lake Shore Market", vec![(5, 10), (11, 14)]),
            ("elysee", palace, vec![(12, 18)]),
            (
                "palais de l'ely",
                palace,
                vec![(0, 6), (7, 9), (10, 11), (12, 15)],
            ),
            ("starbuks", "Starbucks", vec![(0, 9)]),
            ("strabu", "Starbucks", vec![(0, 6)]),
            (
                "lake sohre mark",
                "Lake Shore Market",
                vec![(0, 4), (5, 10), (11, 15)],
            ),
            ("lodz", cafe, vec![(5, 9)]),
            ("cafe lo", cafe, vec![(0, 4), (5, 7)]),
            ("10 king st", house, vec![(0, 2), (3, 7), (8, 10)]),
            ("king street 10", house, vec![(0, 2), (3, 7), (8, 14)]),
            ("toronto king", house, vec![(3, 7), (16, 23)]),
            ("king st tor", house, vec![(3, 7), (8, 14), (16, 19)]),
            ("sbux", "Starbucks", Vec::new()),
        ] {
            assert_eq!(first(typed), (label.to_string(), ranges), "{typed}");
        }
        assert_eq!(cafe.char_indices().nth(5).map(|(byte, _)| byte), Some(6));
        assert_eq!(cafe.chars().skip(5).take(4).collect::<String>(), "Łódź");
    }

    #[test]
    fn a_near_typo_leads_a_far_exact_spelling() {
        let searcher = TextSearcher::open(&*AUTOCOMPLETE).expect("searcher");
        let options = |focus| TextAutocompleteOptions {
            query: "moonbuks".to_string(),
            limit: 5,
            layer: None,
            countries: Vec::new(),
            focus,
            bbox: None,
        };

        let near = suggest(
            &searcher,
            &searcher.places(),
            &options(Some([-92.33, 47.61])),
        )
        .expect("suggest");
        assert_eq!(near[0].record.name, "Moonbucks");
        assert!(near.iter().any(|hit| hit.record.id == "osm:node:11001"));
        let anywhere = suggest(&searcher, &searcher.places(), &options(None)).expect("suggest");
        assert_eq!(anywhere[0].record.id, "osm:node:11001");
    }

    #[test]
    fn a_near_typo_street_outlasts_far_pois() {
        let searcher = TextSearcher::open(&*AUTOCOMPLETE).expect("searcher");
        let options = TextAutocompleteOptions {
            query: "sunbuks".to_string(),
            limit: 1,
            layer: None,
            countries: Vec::new(),
            focus: Some([-84.0, 40.0]),
            bbox: None,
        };

        let found = suggest(&searcher, &searcher.places(), &options).expect("suggest");
        assert_eq!(found[0].record.id, "osm:way:12004");
    }

    #[test]
    fn a_typo_of_a_kind_of_place_names_it() {
        assert!(category_typo("pharmcy").is_some_and(|found| found.contains(&"amenity pharmacy")));
        assert!(category_typo("apoteek").is_some_and(|found| found.contains(&"amenity pharmacy")));
        assert_eq!(category_typo("ph"), None);
        assert_eq!(category_typo("pharmacy near"), None);
    }

    #[test]
    fn a_place_mapped_as_a_node_and_a_boundary_answers_the_boundary() {
        let searcher = TextSearcher::open(&*PLACES).expect("searcher");
        let hits = searcher
            .search(&TextSearchOptions {
                query: "Exeter".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search");
        let ids = hits
            .iter()
            .map(|hit| (hit.record.id.as_str(), hit.record.bbox.is_some()))
            .collect::<Vec<_>>();
        assert_eq!(ids, [("osm:relation:807", true)]);
    }

    #[test]
    fn a_poi_mapped_as_a_node_and_a_building_answers_the_node() {
        let searcher = TextSearcher::open(&*PLACES).expect("searcher").places();
        let hits = searcher
            .search(&TextSearchOptions {
                query: "Providence".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search");
        let ids = hits
            .iter()
            .map(|hit| hit.record.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["osm:node:909"]);
    }

    #[test]
    fn a_named_place_is_near_else_best_known() {
        let searcher = TextSearcher::open(&*PLACES).expect("searcher");

        let from_seattle = searcher
            .place_point("amsterdam", Some([-122.33, 47.61]), false)
            .expect("place");
        assert_eq!(from_seattle.near, None);
        assert_eq!(from_seattle.known, Some([4.89, 52.37]));
        let from_bozeman = searcher
            .place_point("amsterdam", Some([-111.2, 45.7]), false)
            .expect("place");
        assert_eq!(from_bozeman.near, Some([-111.3, 45.8]));
    }

    #[test]
    fn a_named_place_may_be_an_alternate_name_or_a_street() {
        let searcher = TextSearcher::open(&*PLACES).expect("searcher");

        let near = |words| {
            searcher
                .place_point(words, Some([-82.01, 42.0]), false)
                .expect("place")
                .near
        };
        assert_eq!(near("florence"), Some([-82.0, 42.0]));
        assert!(near("queen street").is_some());
    }

    #[test]
    fn a_typed_word_stands_for_its_forms_prefixes_and_contractions() {
        assert!(types_word("market", "market", false));
        assert!(types_word("mar", "market", true));
        assert!(!types_word("mar", "market", false));
        assert!(types_word("pl", "place", false));
        assert!(types_word("n", "north", false));
        assert!(types_word("north", "n", false));
        assert!(types_word("mkt", "market", false));
        assert!(!types_word("cit", "chipotle", true));
        assert!(unlisted_contraction("mkt"));
        assert!(!unlisted_contraction("pl"));
        assert!(!unlisted_contraction("market"));

        let words = |query: &str| autocomplete_query_tokens(query);
        assert!(name_starts_as(
            "Pike Place Market",
            &words("pike pl mkt"),
            false
        ));
        assert!(name_starts_as(
            "Pike Place Market",
            &words("pike place mar"),
            false
        ));
        assert!(!name_starts_as(
            "Pike Place Market",
            &words("place pike"),
            false
        ));
        assert!(!name_starts_as("Pike", &words("pike place"), false));
        assert!(!name_starts_as(
            "Pike Place Market",
            &words("pike plcae mar"),
            false
        ));
        assert!(name_starts_as(
            "Pike Place Market",
            &words("pike plcae mar"),
            true
        ));
        assert!(name_starts_as("Space Needle", &words("spce"), true));
    }

    #[test]
    fn autocomplete_ignores_blank_and_short_queries() {
        let searcher = TextSearcher::open(&*BASICS).expect("searcher");

        assert!(
            searcher
                .autocomplete(&TextAutocompleteOptions {
                    query: " ".to_string(),
                    limit: 5,
                    layer: None,
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                })
                .expect("blank autocomplete")
                .is_empty()
        );
        assert!(
            searcher
                .autocomplete(&TextAutocompleteOptions {
                    query: "k".to_string(),
                    limit: 5,
                    layer: None,
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                })
                .expect("single character autocomplete")
                .is_empty()
        );
        assert!(
            searcher
                .autocomplete(&TextAutocompleteOptions {
                    query: "ki".to_string(),
                    limit: 5,
                    layer: None,
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                })
                .expect("two character autocomplete")
                .is_empty()
        );
    }

    fn top_ids(searcher: &TextSearcher, query: &str) -> Vec<String> {
        top_ids_limited(searcher, query, 5)
    }

    fn top_ids_limited(searcher: &TextSearcher, query: &str, limit: usize) -> Vec<String> {
        searcher
            .search(&TextSearchOptions {
                query: query.to_string(),
                limit,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
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
        let searcher = TextSearcher::open(&*BASICS).expect("searcher");

        assert_eq!(top_ids(&searcher, "10 Bloor Street"), vec!["osm:node:41"]);
        assert_eq!(top_ids(&searcher, "99 Bloor St"), vec!["osm:way:49"]);
        assert_eq!(
            geocode(&searcher, "99 Bloor Street", None, None).as_deref(),
            Some("osm:way:49")
        );
        assert_eq!(
            geocode(&searcher, "99 Bloor Street", None, Some("M5V 1A1")),
            None
        );
    }

    #[test]
    fn a_street_with_its_ordinal_written_out_is_the_numbered_street() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");

        assert_eq!(top_ids(&searcher, "350 Fifth Avenue"), ["osm:node:201"]);
        assert_eq!(top_ids(&searcher, "200 5th St"), ["osm:node:202"]);
        assert_eq!(
            geocode(&searcher, "350 Fifth Ave", None, None).as_deref(),
            Some("osm:node:201")
        );
    }

    #[test]
    fn a_long_word_with_its_second_and_third_letters_swapped_is_one_typo() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");

        assert_eq!(typo_distance("mcihael", "michael"), Some(1));
        assert_eq!(
            top_ids(&searcher, "53 Mcihael Jones Drive")
                .first()
                .map(String::as_str),
            Some("osm:node:301")
        );
    }

    #[test]
    fn an_abbreviated_street_type_ranks_the_street_of_that_type_first() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "46 Highfield Tce")
                .first()
                .map(String::as_str),
            Some("osm:node:402")
        );
    }

    #[test]
    fn the_street_the_query_names_beats_one_with_more_words() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "45 Lomond Crescent")
                .first()
                .map(String::as_str),
            Some("osm:node:501")
        );
        assert_eq!(
            top_ids(&searcher, "47 Mill Road")
                .first()
                .map(String::as_str),
            Some("osm:node:503")
        );
    }

    fn write_unconfirmed_neighbourhood(writer: &mut Writer) {
        for (id, number, city, neighbourhood) in [
            ("osm:node:2041", "182", "Maridale", None),
            ("osm:node:2042", "184", "Maridale", Some("Riverbend")),
            ("osm:node:2043", "182", "Old Market", None),
        ] {
            let mut address = located(
                address_record(id, "", number, "Pedro Street", Some(city), None),
                [-105.0, 20.0],
            );
            if let Some(neighbourhood) = neighbourhood {
                address.address.areas.push(neighbourhood.to_string());
            }
            writer.write(&address.into(), None).expect("address");
        }
    }

    #[test]
    fn an_unconfirmed_neighbourhood_cannot_replace_the_stated_municipality() {
        let searcher = TextSearcher::open(&*AREAS).expect("searcher");
        let ask = |query: &str| {
            let options = TextSearchOptions {
                query: query.to_string(),
                limit: 1,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            };
            searcher.search(&options).expect("search")
        };
        let found = ask("182 Pedro Street, Old Market, Maridale");
        assert_eq!(found[0].record.id, "osm:node:2041");
        assert_eq!(found[0].matched.locality, Some(PartMatch::Partial));
        let found = ask("184 Pedro Street, Old Market, Maridale");
        assert!(found.iter().all(|hit| hit.record.id != "osm:node:2042"));
    }

    #[test]
    fn stated_numbers_rank_before_range_estimates_before_streets() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "10 Wellesley Street"),
            vec!["osm:node:610", "osm:way:620:interp:2-98"],
            "the stated number first, then the range that contains it"
        );

        let hits = searcher
            .search(&TextSearchOptions {
                query: "50 Wellesley Street".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search");
        assert_eq!(
            hits.len(),
            1,
            "the street does not answer a number a range has"
        );
        let estimate = &hits[0].record;
        assert_eq!(estimate.label, "50 Wellesley Street");
        let point = estimate.point.expect("point");
        assert_eq!(point.precision, PointPrecision::Estimated);
        assert!((point.lat - 43.0005).abs() < 1e-6, "{point:?}");
        assert!((point.lon + 79.0).abs() < 1e-6, "{point:?}");

        assert_eq!(
            top_ids(&searcher, "51 Wellesley Street"),
            vec!["osm:way:630"]
        );
    }

    fn write_named_localities(writer: &mut Writer, names: &mut HashMap<RecordId, Vec<String>>) {
        use crate::{
            context::AdminContextTuple,
            pack::RecordContext,
            record::{
                InterpolationAddressComponents, InterpolationRange, InterpolationRecord,
                PlaceLayer, PlaceRecord, Record,
            },
        };

        let at = [-110.0, 20.0];
        let mut locality = |name: &str, object_id: i64| {
            writer
                .write(
                    &Record::Place(
                        PlaceLayer::Locality,
                        PlaceRecord {
                            alt_names: Vec::new(),
                            name: name.to_string(),
                            place_type: "city".to_string(),
                            geometry: point_geometry(at[0], at[1]),
                            source: SourceProvenance::osm(ObjectType::Relation, object_id),
                            prominence: Default::default(),
                            wikidata: None,
                        },
                    ),
                    None,
                )
                .expect("locality")
        };
        let toronto = locality("Toronto", 3100);
        let st_catharines = locality("St. Catharines", 3101);
        extend_context_names(
            writer,
            names,
            [
                (toronto, vec!["Toronto".to_string()]),
                (st_catharines, vec!["St. Catharines".to_string()]),
            ],
        );
        let in_locality = |record_id| {
            Some(RecordContext {
                admin_context: AdminContextTuple::from_tiers([(PlaceLayer::Locality, record_id)]),
                flags: 0,
                ..Default::default()
            })
        };
        writer
            .write(
                &located(
                    address_record("osm:node:3001", "", "182", "Baldwin Street", None, None),
                    at,
                )
                .into(),
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
                    geometry: line(at[0], [at[1], at[1] + 0.001]),
                    representative_point: [at[0], at[1] + 0.0005],
                    source: SourceProvenance::osm(ObjectType::Way, 3020),
                }
                .into(),
                in_locality(toronto),
            )
            .expect("baldwin range in toronto");
        writer
            .write(
                &located(
                    address_record(
                        "osm:node:3003",
                        "",
                        "2228",
                        "Gibraltar Road",
                        Some("Kingston"),
                        None,
                    ),
                    at,
                )
                .into(),
                None,
            )
            .expect("kingston address");
        writer
            .write(
                &street_at("osm:way:3040", "Kingston Road", at).into(),
                in_locality(toronto),
            )
            .expect("kingston road");
        writer
            .write(
                &located(
                    address_record("osm:node:3005", "", "99", "Queen Street", None, None),
                    at,
                )
                .into(),
                None,
            )
            .expect("queen elsewhere");
        writer
            .write(
                &street_at("osm:way:3050", "Queen Street", at).into(),
                in_locality(toronto),
            )
            .expect("queen street");
        writer
            .write(
                &located(
                    address_record("osm:node:3006", "", "5", "Geneva Street", None, None),
                    at,
                )
                .into(),
                in_locality(st_catharines),
            )
            .expect("st catharines address");
    }

    #[test]
    fn named_localities_are_required_before_they_only_rank() {
        let searcher = TextSearcher::open(&*AREAS).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "182 Baldwin St, Toronto, M5T 1L8"),
            vec!["osm:way:3020:interp:100-200"]
        );
        assert_eq!(
            top_ids(&searcher, "182 Baldwin St, M5T 1L8")[0],
            "osm:node:3001"
        );
        assert_eq!(
            top_ids(&searcher, "182 Baldwin St, Atlantis")[0],
            "osm:node:3001"
        );
        assert_eq!(
            top_ids(&searcher, "2228 Kingston Rd, Toronto"),
            vec!["osm:way:3040"]
        );
        assert_eq!(
            top_ids(&searcher, "2228 Gibraltar Rd, Kingston"),
            vec!["osm:node:3003"]
        );
        assert_eq!(
            top_ids(&searcher, "99 Queen St, Toronto"),
            vec!["osm:way:3050"]
        );
        assert_eq!(
            geocode(&searcher, "99 Queen Street", Some("Toronto"), None).as_deref(),
            Some("osm:way:3050")
        );
        assert_eq!(
            top_ids(&searcher, "5 Geneva St, St. Catharines"),
            vec!["osm:node:3006"]
        );
    }

    #[test]
    fn a_postcode_ranks_records_by_how_much_of_it_they_share() {
        let searcher = TextSearcher::open(&*POSTCODES).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "5 Main Street, A1A 9Z9"),
            vec!["osm:node:702", "osm:node:703", "osm:node:701"]
        );
        assert_eq!(
            top_ids(&searcher, "5 Main Street, Toronto ON B2B 2B2")[0],
            "osm:node:701",
            "the postcode starts at its first word with a digit"
        );
        let mut ids = top_ids(&searcher, "5 Main Street, Z9Z 9Z9");
        ids.sort();
        assert_eq!(ids, vec!["osm:node:701", "osm:node:702", "osm:node:703"]);
    }

    #[test]
    fn a_hit_contradicting_the_stated_postcode_does_not_end_the_search() {
        let searcher = TextSearcher::open(&*POSTCODES).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "8 High Street, 7304"),
            vec!["osm:way:802", "osm:node:801"]
        );
        assert!(top_ids(&searcher, "8 High Street").contains(&"osm:node:801".to_string()));
    }

    #[test]
    fn ranking_asks_level_then_postcode_then_source_then_text() {
        let searcher = TextSearcher::open(&*POSTCODES).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "23 Victoria St, S5C 2A1"),
            vec!["osm:way:920:interp:1-99", "osm:node:901"],
            "an estimate in the postcode before a stated number elsewhere"
        );
        assert_eq!(
            top_ids(&searcher, "23 Victoria St, S8V 2A1"),
            vec!["osm:node:901", "osm:way:920:interp:1-99"]
        );
        assert_eq!(
            top_ids(&searcher, "23 Victoria St"),
            vec!["osm:node:901", "osm:way:920:interp:1-99"],
            "without a postcode the stated number first"
        );
        assert_eq!(
            top_ids(&searcher, "7 Elm St, Atlantis, S5C 1A1"),
            vec!["osm:node:904", "osm:way:930"]
        );
    }

    fn line(lon: f64, lats: [f64; 2]) -> geojson::Geometry {
        geojson::Geometry::new(geojson::GeometryValue::LineString {
            coordinates: vec![vec![lon, lats[0]].into(), vec![lon, lats[1]].into()],
        })
    }

    #[test]
    fn a_missing_number_is_placed_between_its_stated_neighbours() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");
        let top = |query: &str| {
            searcher
                .search(&TextSearchOptions {
                    query: query.to_string(),
                    limit: 5,
                    layer: None,
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
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
        assert_eq!(estimate.id, "derived:estimate:14:osm:node:710:osm:node:711");
        let point = estimate.point.expect("point");
        assert_eq!(point.precision, PointPrecision::Estimated);
        assert!((point.lat - 46.0014).abs() < 1e-9, "{point:?}");
        assert!((point.lon + 79.0002).abs() < 1e-9, "{point:?}");
        assert_eq!(
            geocode(&searcher, "14 Bayview Avenue", None, None).as_deref(),
            Some("derived:estimate:14:osm:node:710:osm:node:711")
        );
        let in_france = searcher
            .search(&TextSearchOptions {
                query: "14 Bayview Ave".to_string(),
                limit: 5,
                layer: None,
                countries: vec!["FR".to_string()],
                focus: None,
                bbox: None,
            })
            .expect("search");
        assert!(in_france.is_empty(), "{in_france:?}");
        let odd = top("25 Bayview Ave").point.expect("point");
        assert!((odd.lat - 46.0025).abs() < 1e-9, "{odd:?}");
        assert_eq!(top("40 Bayview Ave").id, "osm:way:701");
        assert_eq!(
            geocode(&searcher, "40 Bayview Avenue", None, None).as_deref(),
            Some("osm:way:701")
        );
        assert_eq!(top("85 Victoria St").layer, "street");
    }

    #[test]
    fn autocomplete_retries_country_codes_only_for_numbered_addresses() {
        let options = TextAutocompleteOptions {
            query: "82 Smith St, Providence, RI 02903, USA".to_string(),
            limit: 5,
            layer: None,
            countries: Vec::new(),
            focus: None,
            bbox: None,
        };
        let normalized = autocomplete_country_alias(&options).expect("country alias");
        assert_eq!(normalized.query, "82 Smith St, Providence, RI 02903, US");
        assert_eq!(normalized.countries, ["US"]);
        assert!(autocomplete_country_alias(&normalized).is_none());
        assert!(
            autocomplete_country_alias(&TextAutocompleteOptions {
                countries: vec!["CA".to_string()],
                ..options.clone()
            })
            .is_none()
        );
        assert!(
            autocomplete_country_alias(&TextAutocompleteOptions {
                query: "Providence, USA".to_string(),
                ..options
            })
            .is_none()
        );
    }

    #[test]
    fn autocomplete_recovers_a_house_number_on_its_stated_street_and_locality() {
        let searcher = TextSearcher::open(&*AUTOCOMPLETE).expect("searcher");
        let places = searcher.places();
        let options = TextAutocompleteOptions {
            query: "189 Haynes St SW Atlanta".to_string(),
            limit: 5,
            layer: None,
            countries: Vec::new(),
            focus: None,
            bbox: None,
        };
        let estimate_id = "derived:estimate:189:osm:node:13011:osm:node:13012";
        for hit in [
            searcher
                .autocomplete(&options)
                .expect("address suggestions"),
            suggest(&searcher, &places, &options).expect("mixed suggestions"),
        ] {
            assert_eq!(hit.len(), 1);
            assert_eq!(hit[0].record.id, estimate_id);
            assert_eq!(
                hit[0].record.point.expect("point").precision,
                PointPrecision::Estimated
            );
        }

        let suggest_for = |query: &str| {
            suggest(
                &searcher,
                &places,
                &TextAutocompleteOptions {
                    query: query.to_string(),
                    ..options.clone()
                },
            )
            .expect("mixed suggestions")
        };
        assert_eq!(
            suggest_for("181 Haynes St SW Atlanta")[0].record.id,
            "osm:node:13011"
        );
        for query in [
            "189 Haynes St SW",
            "189 Haynes St SW Atlan",
            "189 Haynes St NE Atlanta",
            "189 Haynes St SW Boston",
            "199 Haynes St SW Atlanta",
        ] {
            assert!(
                !suggest_for(query)
                    .iter()
                    .any(|hit| hit.record.id.starts_with("derived:estimate:")),
                "{query}"
            );
        }

        for excluded in [
            TextAutocompleteOptions {
                countries: vec!["CA".to_string()],
                ..options.clone()
            },
            TextAutocompleteOptions {
                layer: Some("street".to_string()),
                ..options
            },
        ] {
            assert!(
                searcher
                    .autocomplete(&excluded)
                    .expect("suggestions")
                    .is_empty()
            );
        }
    }

    #[test]
    fn qualified_geocode_and_autocomplete_recover_a_tiger_address() {
        let addresses = TextSearcher::open(&*NUMBERS).expect("searcher");
        let places = addresses.places();
        let ask = |query: &str, layer: Option<Layer>| {
            suggest(
                &addresses,
                &places,
                &TextAutocompleteOptions {
                    query: query.to_string(),
                    limit: 5,
                    layer: layer.map(|layer| layer.as_str().to_string()),
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                },
            )
            .expect("suggestions")
        };
        for query in [
            "50 Queen Street West, Fairview",
            "Queen Street West 50, Fairview",
        ] {
            let hits = ask(query, None);
            assert_eq!(hits.len(), 1, "{query}");
            assert_eq!(hits[0].record.id, "derived:tiger:820:L:50", "{query}");
            assert!(!hits[0].highlights.is_empty(), "{query}");
        }
        let street = ask("51 Queen Street West, Fairview", None);
        assert_eq!(street.len(), 1);
        assert_eq!(street[0].record.layer, Layer::Street.as_str());
        assert!(street[0].fallback);
        for query in [
            "50 Queen Street West, Wrongtown",
            "50 Queen Street West, Fairview, Massachusetts",
        ] {
            assert!(ask(query, None).is_empty(), "{query}");
        }
        assert!(ask("50 Queen Street West, Fairview", Some(Layer::Address)).is_empty());
        let filters = AddressGeocodeOptions {
            address: "50 Queen Street West".to_string(),
            locality: Some("Fairview".to_string()),
            region: Some("Rhode Island".to_string()),
            postcode: Some("02852".to_string()),
            limit: 1,
            layer: None,
            countries: Vec::new(),
        };
        let found = addresses
            .geocode_address(&filters)
            .expect("structured geocode")
            .expect("interpolated address");
        assert_eq!(found.hit.record.id, "derived:tiger:820:L:50");
        assert_eq!(found.hit.matched.postcode, Some(PartMatch::Matched));
        let with_other_zip = addresses
            .geocode_address(&AddressGeocodeOptions {
                postcode: Some("02853".to_string()),
                ..filters
            })
            .expect("structured geocode");
        assert!(
            with_other_zip
                .is_none_or(|found| found.hit.record.layer != Layer::Interpolation.as_str())
        );
    }

    #[test]
    fn autocomplete_can_use_a_mapped_house_number_range() {
        let searcher = TextSearcher::open(&*AUTOCOMPLETE).expect("searcher");
        let places = searcher.places();
        let ask = |query: &str, layer: Option<Layer>, focus| {
            suggest(
                &searcher,
                &places,
                &TextAutocompleteOptions {
                    query: query.to_string(),
                    limit: 5,
                    layer: layer.map(|layer| layer.as_str().to_string()),
                    countries: Vec::new(),
                    focus,
                    bbox: None,
                },
            )
            .expect("suggestions")
        };
        let hits = ask("50 Queen St W Toronto", None, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record.id, "osm:way:14020:interp:2-98");
        assert_eq!(
            hits[0].record.point.expect("point").precision,
            PointPrecision::Estimated
        );
        for query in [
            "51 Queen St W Toronto",
            "50 Queen St E Toronto",
            "50 Queen St W Vancouver",
        ] {
            assert!(ask(query, None, None).is_empty(), "{query}");
        }
        assert!(ask("50 Queen St W Toronto", Some(Layer::Address), None).is_empty());
        let bare = "50 Queen St W";
        assert_eq!(ask(bare, None, None)[0].record.id, "osm:node:14050");
        assert_eq!(
            ask(bare, None, Some([-79.0, 43.0]))[0].record.id,
            "osm:way:14020:interp:2-98",
        );
        assert_eq!(
            ask(bare, None, Some([-75.7, 45.4]))[0].record.id,
            "osm:node:14050",
        );
        assert!(ask("50 Queen St W Vancouver", None, Some([-79.0, 43.0])).is_empty());
    }

    #[test]
    fn geocoding_no_number_names_no_house() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");

        assert_eq!(top_ids(&searcher, "Elizabeth Lane")[0], "osm:node:901");
        assert_eq!(geocode(&searcher, "Elizabeth Lane", None, None), None);
    }

    #[test]
    fn a_house_number_needs_its_street_not_a_name() {
        let searcher = TextSearcher::open(&*PLACES).expect("searcher");

        assert_eq!(top_ids(&searcher, "300 Spadina Ave"), vec!["osm:node:1202"]);
        assert_eq!(
            top_ids(&searcher.places(), "Spadina Dental")[0],
            "osm:node:1201"
        );
        assert_eq!(
            top_ids(&searcher, "300 St Clair Ave W"),
            vec!["osm:node:1201"],
            "the dentist's address is its street's"
        );
    }

    fn tagged(text: &str) -> Vec<Tagged> {
        text.split_whitespace()
            .map(|pair| {
                let (token, label) = pair.split_once('/').expect("word/label");
                Tagged {
                    token: token.to_string(),
                    label: Label::from_name(label).expect("label"),
                }
            })
            .collect()
    }

    #[test]
    fn a_country_code_read_as_a_town_is_the_country() {
        let france = CountryCode::parse("FR");
        let read = tagged(
            "12/house_number rue/street paix/street lyon/locality 69002/postcode fr/locality",
        );
        let written = Some(("FR".to_string(), 5..6));
        let last = |pieces: Vec<Tagged>| pieces[5].label;
        assert_eq!(
            last(country_not_town(read.clone(), written.clone(), france)),
            Label::Country
        );
        assert_eq!(
            last(country_not_town(
                read.clone(),
                written.clone(),
                CountryCode::parse("CH")
            )),
            Label::Locality
        );
        let region =
            tagged("12/house_number rue/street paix/street lyon/locality 69002/postcode fr/region");
        assert_eq!(
            last(country_not_town(region, written, france)),
            Label::Region
        );
        assert_eq!(last(country_not_town(read, None, france)), Label::Locality);
    }

    #[test]
    fn a_query_is_read_once_unglued_and_without_replacement_characters() {
        assert_eq!(
            clean_query("191 Beaufort Street 191 Beaufort Street ACT 6000 AU"),
            "191 Beaufort Street ACT 6000 AU"
        );
        assert_eq!(
            clean_query("Site de Choranche2865 route des Grottes"),
            "Site de Choranche 2865 route des Grottes"
        );
        assert_eq!(clean_query("EmbarcadèreRue Haute"), "Embarcadère Rue Haute");
        assert_eq!(clean_query("rue de Beaus\u{fffd}jour"), "rue de Beausjour");
        assert_eq!(
            clean_query("116bis T5H 0W2 18th DeKalb"),
            "116bis T5H 0W2 18th DeKalb"
        );
        assert_eq!(
            clean_query("Zaragoza Zaragoza 50017"),
            "Zaragoza Zaragoza 50017"
        );
    }

    #[test]
    fn a_name_of_hangul_or_an_unspaced_script_keeps_its_numbers() {
        assert_eq!(
            clean_query("수성구 달구벌대로530길 145-2"),
            "수성구 달구벌대로530길 145-2"
        );
        assert_eq!(clean_query("東橄榄路4081号"), "東橄榄路4081号");
    }

    #[test]
    fn a_number_the_parser_labels_is_kept_with_its_designator_or_suffix() {
        let number = |text: &str| {
            let plan =
                QueryPlan::from_tagged(&tagged(text), &[], &[], &[], |_| false).expect("plan");
            let plan = plan.with_base_house_number().unwrap_or(plan);
            (
                plan.house_number.map(|number| number.text),
                plan.number_kept,
            )
        };
        assert_eq!(
            number("116bis/house_number gracia/street barcelona/locality"),
            (Some("116".to_string()), true)
        );
        assert_eq!(
            number("no/house_number 2/house_number jalan/street stesen/street"),
            (Some("2".to_string()), true)
        );
        assert_eq!(
            number("lau/house_number 4/house_number 34a/house_number pham/street"),
            (Some("4".to_string()), true)
        );
        assert_eq!(
            number("12/house_number main/street street/street"),
            (Some("12".to_string()), false)
        );
        assert_eq!(number("near/house_number megha/street"), (None, false));
    }

    #[test]
    fn a_unit_is_read_from_a_letter_or_a_word_glued_to_the_number() {
        let unit = |text: &str| {
            QueryPlan::from_tagged(&tagged(text), &[], &[], &[], |_| false)
                .expect("plan")
                .unit
        };
        assert_eq!(
            unit("g/unit 9/house_number gore/street street/street").as_deref(),
            Some("g")
        );
        assert_eq!(
            unit("flat/unit b/unit 9/house_number gore/street street/street").as_deref(),
            Some("b")
        );
        assert_eq!(
            unit("g11/other 40/house_number rosedale/street road/street").as_deref(),
            Some("g11")
        );
        assert_eq!(
            unit("unit/unit 9/house_number gore/street street/street"),
            None
        );
        assert_eq!(
            unit("acme/other 40/house_number rosedale/street road/street"),
            None
        );
    }

    #[test]
    fn a_reading_whose_postcode_nothing_confirms_yields_to_one_confirmed() {
        let found = |confirmed: bool| Evidence {
            edits: 0,
            pass: Pass::Strict,
            confirmed,
            score: 2.0,
        };
        assert!(found(false).open());
        assert!(found(false).beaten_by(Pass::Strict));
        let house = Evidence {
            score: 1.0,
            ..found(true)
        };
        assert!(house.outweighs(found(false)));
        assert!(!found(true).open());
        let typo = Evidence {
            edits: 1,
            ..found(true)
        };
        assert!(typo.open());
        assert!(typo.beaten_by(Pass::Lenient));
    }

    #[test]
    fn plans_numbers_units_streets_and_places_from_labelled_tokens() {
        let plan = |text: &str| {
            QueryPlan::from_tagged(&tagged(text), &[], &[], &[], |_| false).expect("plan")
        };
        let subject = |plan: &QueryPlan| {
            plan.subject
                .iter()
                .map(|word| word.text.clone())
                .collect::<Vec<_>>()
        };
        let words = |words: &[&str]| {
            words
                .iter()
                .map(|word| word.to_string())
                .collect::<Vec<_>>()
        };
        let part = |label: Label, words: &[&str]| ContextPart {
            label,
            words: words.iter().map(|word| word.to_string()).collect(),
            corrected: None,
            respelt: false,
            split: false,
        };

        let floor =
            plan("79/house_number wellington/street st/street w/street fl36/unit toronto/locality");
        assert_eq!(floor.house_number.as_ref().expect("number").text, "79");
        assert_eq!(subject(&floor), words(&["wellington", "street", "west"]));
        assert_eq!(floor.unit.as_deref(), Some("fl36"));
        assert!(floor.optional.is_empty());
        assert_eq!(floor.context, [part(Label::Locality, &["toronto"])]);

        let unit = plan(
            "unit/unit 602/unit 670/house_number king/street st/street w/street \
             toronto/locality on/region m5v/postcode 1m9/postcode",
        );
        assert_eq!(unit.unit.as_deref(), Some("602"));
        assert!(unit.optional.is_empty());
        let unplaced = plan("unit/other 2/other 1/house_number balfour/street street/street");
        assert_eq!(unplaced.unit.as_deref(), Some("2"));
        assert!(unplaced.unplaced.is_empty());
        assert_eq!(subject(&unplaced), words(&["balfour", "street"]));
        assert_eq!(
            unit.context,
            [
                part(Label::Locality, &["toronto"]),
                part(Label::Region, &["on"]),
                part(Label::Postcode, &["m5v", "1m9"]),
            ]
        );
        assert_eq!(unit.postcode().forms, ["m5v1m9"]);
        assert_eq!(
            unit.text(),
            "670 king street west unit 602, toronto, on, m5v 1m9"
        );
        assert_eq!(
            plan("306/unit 1333/house_number sheppard/street ave/street e/street")
                .house_number
                .expect("number")
                .text,
            "1333"
        );

        assert_eq!(
            subject(&plan("1000/house_number highway/street 7/street")),
            words(&["highway", "7"])
        );
        assert_eq!(
            subject(&plan("st/street clair/street ave/street w/street")),
            words(&["st", "clair", "avenue", "west"])
        );
        assert_eq!(
            subject(&plan("15/house_number the/street donway/street e/street")),
            words(&["the", "donway", "east"])
        );
        let german = plan("maximilianstr/street 5/house_number münchen/locality");
        assert_eq!(german.house_number.as_ref().expect("number").text, "5");
        assert_eq!(subject(&german), words(&["maximilianstr"]));

        for (text, number, base) in [
            (
                "993/house_number 5/house_number bloor/street st/street",
                "993 5",
                993,
            ),
            (
                "1384/house_number 1/house_number 2/house_number queen/street",
                "1384 1 2",
                1384,
            ),
            ("407a/house_number yonge/street st/street", "407a", 407),
        ] {
            let plan = plan(text);
            let house = plan.house_number.as_ref().expect("number");
            assert_eq!((house.text.as_str(), house.base), (number, base), "{text}");
            let fallback = plan.with_base_house_number().expect("fallback");
            assert_eq!(
                fallback.house_number.expect("number").text,
                base.to_string()
            );
        }
        assert_eq!(
            plan("10/house_number king/street st/street").with_base_house_number(),
            None
        );
        for (text, number, rest) in [
            (
                "221/house_number b/house_number baker/street street/street",
                "221 b",
                &["b"][..],
            ),
            (
                "81/house_number к1/house_number вавилова/street",
                "81 к1",
                &["к1"][..],
            ),
            ("81к1/house_number вавилова/street", "81к1", &["81к1"][..]),
        ] {
            let compound = plan(text);
            let house = compound.house_number.as_ref().expect("number");
            assert_eq!(house.text, number, "{text}");
            assert!(compound.optional.is_empty(), "{text}");
            let short = compound.with_base_house_number().expect("short");
            assert_eq!(short.house_number.expect("number").base, house.base);
            assert_eq!(short.optional, words(rest), "{text}");
        }
        let slashed = plan("12/house_number 10/house_number king/street st/street");
        assert_eq!(slashed.house_number.expect("number").text, "12 10");

        let places = QueryPlan::from_tagged(
            &tagged("seaforth/locality huron/locality east/locality"),
            &[],
            &[],
            &[part(Label::Region, &["ontario"])],
            |_| false,
        )
        .expect("plan");
        assert_eq!(subject(&places), words(&["seaforth", "huron", "east"]));
        assert_eq!(places.context, [part(Label::Region, &["ontario"])]);
        let number = plan("670/house_number");
        assert_eq!(
            (number.house_number.clone(), subject(&number)),
            (None, words(&["670"]))
        );
        assert!(QueryPlan::from_tagged(&[], &[], &[], &[], |_| false).is_none());
    }

    #[test]
    fn searches_written_house_numbers_then_the_number_they_start_with() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "1384 1/2 Dundas St E")[0],
            "osm:node:1001"
        );
        assert_eq!(
            top_ids(&searcher, "993.5 Bloor St W"),
            vec!["osm:node:1003"]
        );
        assert_eq!(top_ids(&searcher, "100 Highway 7")[0], "osm:node:1004");
        assert_eq!(
            top_ids(&searcher, "8 The Donway E, Unit-B"),
            vec!["osm:node:1006"]
        );
    }

    #[test]
    fn a_house_number_states_the_same_digits_with_its_words_abbreviated() {
        let fit =
            |sought: &str, stated: &str| number_fit(&number_runs(sought), &number_runs(stated));
        for stated in ["81 к1", "81к1", "81 к 1", "81к 1"] {
            assert_eq!(fit("81 к1", stated), Some(NumberFit::Stated), "{stated}");
        }
        for (sought, stated) in [
            ("81 корпус 1", "81 к1"),
            ("81 корп 1", "81 к1"),
            ("12 к2 строение 1", "12 к2 с1"),
            ("5 bldg 2", "5 building 2"),
        ] {
            assert_eq!(
                fit(sought, stated),
                Some(NumberFit::Abbreviated),
                "{sought}"
            );
        }
        for (sought, stated) in [
            ("81 к1", "81 к2"),
            ("81 с1", "13 с1"),
            ("81 xyz 1", "81 к1"),
            ("81 к1", "81 к1 с1"),
            ("12 10", "1210"),
            ("81к1", "81"),
        ] {
            assert_eq!(fit(sought, stated), None, "{sought} {stated}");
        }
    }

    #[test]
    fn finds_a_building_written_after_its_plot_number() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");

        for reading in [
            "81/house_number к1/house_number улица/street вавилова/street москва/locality",
            "81к1/house_number улица/street вавилова/street москва/locality",
            "81/house_number к/house_number 1/house_number улица/street вавилова/street",
            "81/house_number корпус/house_number 1/house_number улица/street вавилова/street",
            "81/house_number корп/house_number 1/house_number улица/street вавилова/street",
        ] {
            assert_eq!(answer(&searcher, reading)[0], "osm:node:1102", "{reading}");
        }
        assert_eq!(
            answer(
                &searcher,
                "81/house_number корпус/house_number 2/house_number улица/street вавилова/street"
            )[0],
            "osm:node:1103"
        );
        let either = reading_plan(
            &searcher,
            "85/house_number сооружение/house_number 1/house_number улица/street вавилова/street",
        );
        assert!(
            answer_plan(&searcher, &either)
                .iter()
                .all(|hit| !hit.signals.house_level)
        );
        assert_eq!(
            answer(
                &searcher,
                "81/house_number к2/house_number улица/street вавилова/street"
            )[0],
            "osm:node:1103"
        );
        assert_eq!(
            answer(&searcher, "81/house_number улица/street вавилова/street")[0],
            "osm:node:1101"
        );
        let missing = reading_plan(
            &searcher,
            "81/house_number к7/house_number улица/street вавилова/street",
        );
        assert!(
            answer_plan(&searcher, &missing)
                .iter()
                .all(|hit| !hit.signals.house_level)
        );
        let plot = missing.with_base_house_number().expect("plot");
        assert_eq!(answer_plan(&searcher, &plot)[0].record.id, "osm:node:1101");
    }

    #[test]
    fn an_abbreviated_number_is_sure_only_across_its_whole_street() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");
        let houses = |reading: &str| {
            let mut ids = answer_plan(&searcher, &reading_plan(&searcher, reading))
                .into_iter()
                .filter(|hit| hit.signals.house_level)
                .map(|hit| hit.record.id)
                .collect::<Vec<_>>();
            ids.sort_unstable();
            ids
        };

        assert_eq!(
            houses(
                "85/house_number сооружение/house_number 1/house_number улица/street пушкина/street казань/locality"
            ),
            Vec::<String>::new()
        );
        assert_eq!(
            houses(
                "87/house_number корпус/house_number 1/house_number улица/street пушкина/street казань/locality"
            ),
            ["osm:node:1204"]
        );
        assert_eq!(
            houses(
                "88/house_number корпус/house_number 1/house_number улица/street пушкина/street казань/locality"
            ),
            ["osm:node:1205", "osm:node:1206"]
        );
    }

    #[test]
    fn finds_each_address_of_a_street_group_by_its_number_and_unit() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");

        assert_eq!(top_ids(&searcher, "7 Spruce Street"), vec!["osm:node:1306"]);
        assert_eq!(
            top_ids(&searcher, "11 Spruce Street"),
            vec!["osm:node:1307"]
        );
        assert_eq!(
            top_ids(&searcher, "5a Spruce Street"),
            vec!["osm:node:1305"]
        );
        assert_eq!(
            top_ids(&searcher, "5 Spruce Street, Oakville"),
            vec!["osm:node:1320"]
        );
        let toronto = top_ids(&searcher, "Unit 2 5 Spruce Street, Toronto");
        assert_eq!(toronto[0], "osm:node:1304", "the stated unit ranks first");
        assert_eq!(toronto.len(), 2);
        let autocomplete = searcher
            .autocomplete(&TextAutocompleteOptions {
                query: "13 spruce".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("autocomplete");
        assert_eq!(
            autocomplete
                .iter()
                .map(|hit| hit.record.id.as_str())
                .collect::<Vec<_>>(),
            vec!["osm:node:1308"]
        );
    }

    #[test]
    fn a_flat_the_query_does_not_name_ranks_after_its_building() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "Unit 3 5 Albion Street")
                .first()
                .map(String::as_str),
            Some("osm:node:1402")
        );
        assert_eq!(
            top_ids(&searcher, "Unit 2 5 Albion Street")
                .first()
                .map(String::as_str),
            Some("osm:node:1401")
        );
    }

    fn write_community(writer: &mut Writer, names: &mut HashMap<RecordId, Vec<String>>) {
        use crate::{
            context::AdminContextTuple,
            pack::RecordContext,
            record::{PlaceLayer, PlaceRecord, Record},
        };

        let huron_east = writer
            .write(
                &Record::Place(
                    PlaceLayer::Locality,
                    PlaceRecord {
                        alt_names: Vec::new(),
                        name: "Huron East".to_string(),
                        place_type: "admin_level:8".to_string(),
                        geometry: point_geometry(-81.4, 43.5),
                        source: SourceProvenance::osm(ObjectType::Relation, 4001),
                        prominence: Default::default(),
                        wikidata: None,
                    },
                ),
                None,
            )
            .expect("municipality");
        extend_context_names(
            writer,
            names,
            [(huron_east, vec!["Huron East".to_string()])],
        );
        let at = [-120.0, 20.0];
        writer
            .write(
                &located(
                    address_record(
                        "osm:node:4003",
                        "",
                        "1",
                        "The Square",
                        Some("Goderich"),
                        None,
                    ),
                    at,
                )
                .into(),
                None,
            )
            .expect("goderich");
        writer
            .write(
                &located(
                    address_record(
                        "osm:node:4002",
                        "",
                        "18",
                        "Main Street South",
                        Some("Seaforth"),
                        None,
                    ),
                    at,
                )
                .into(),
                Some(RecordContext {
                    admin_context: AdminContextTuple::from_tiers([(
                        PlaceLayer::Locality,
                        huron_east,
                    )]),
                    flags: 0,
                    ..Default::default()
                }),
            )
            .expect("address");
    }

    #[test]
    fn geocode_accepts_a_community_the_record_states_inside_a_larger_municipality() {
        let searcher = TextSearcher::open(&*AREAS).expect("searcher");

        for locality in ["Seaforth", "Huron East"] {
            assert_eq!(
                geocode(&searcher, "18 Main Street South", Some(locality), None).as_deref(),
                Some("osm:node:4002"),
                "{locality}"
            );
        }
        assert_eq!(
            geocode(&searcher, "18 Main Street South", Some("Goderich"), None),
            None
        );
    }

    fn write_partial_town(writer: &mut Writer) {
        writer
            .write(
                &located(
                    address_record(
                        "osm:node:5001",
                        "",
                        "60",
                        "Crompton Avenue",
                        Some("West Warwick"),
                        None,
                    ),
                    [-115.0, 20.0],
                )
                .into(),
                None,
            )
            .expect("address");
    }

    #[test]
    fn a_word_of_a_town_name_is_a_partial_match_and_the_whole_name_its_own() {
        let searcher = TextSearcher::open(&*AREAS).expect("searcher");
        let locality = |query: &str| {
            searcher
                .search(&TextSearchOptions {
                    query: query.to_string(),
                    limit: 1,
                    layer: None,
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                })
                .expect("search")
                .first()
                .and_then(|hit| hit.matched.locality)
        };
        assert_eq!(
            locality("60 Crompton Avenue, West Warwick"),
            Some(PartMatch::Matched)
        );
        assert_eq!(
            locality("60 Crompton Avenue, Warwick"),
            Some(PartMatch::Partial)
        );
    }

    fn write_county(writer: &mut Writer, names: &mut HashMap<RecordId, Vec<String>>) {
        use crate::{
            context::AdminContextTuple,
            pack::RecordContext,
            record::{PlaceLayer, PlaceRecord, Record},
        };

        let mut area = |layer: PlaceLayer, name: &str, object_id: i64| {
            let id = writer
                .write(
                    &Record::Place(
                        layer,
                        PlaceRecord {
                            alt_names: Vec::new(),
                            name: name.to_string(),
                            place_type: "admin_level:6".to_string(),
                            geometry: point_geometry(-71.5, 41.9),
                            source: SourceProvenance::osm(ObjectType::Relation, object_id),
                            prominence: Default::default(),
                            wikidata: None,
                        },
                    ),
                    None,
                )
                .expect("area");
            (id, name.to_string())
        };
        let (county, county_name) = area(PlaceLayer::District, "Providence County", 6001);
        let (town, town_name) = area(PlaceLayer::Locality, "Woonsocket", 6002);
        extend_context_names(
            writer,
            names,
            [(county, vec![county_name]), (town, vec![town_name])],
        );
        let context = |tiers: &[(PlaceLayer, RecordId)]| {
            Some(RecordContext {
                admin_context: AdminContextTuple::from_tiers(tiers.iter().copied()),
                flags: 0,
                zone: None,
                enclosing: Vec::new(),
            })
        };
        let at = [-125.0, 20.0];
        writer
            .write(
                &located(
                    address_record("osm:node:6003", "", "1", "Social Street", None, None),
                    at,
                )
                .into(),
                context(&[(PlaceLayer::District, county), (PlaceLayer::Locality, town)]),
            )
            .expect("in a town");
        writer
            .write(
                &located(
                    address_record("osm:node:6004", "", "7", "Farm Lane", None, None),
                    at,
                )
                .into(),
                context(&[(PlaceLayer::District, county)]),
            )
            .expect("in no town");
    }

    #[test]
    fn a_county_names_the_town_only_of_a_record_no_town_holds() {
        let searcher = TextSearcher::open(&*AREAS).expect("searcher");

        assert_eq!(
            geocode(&searcher, "1 Social Street", Some("Woonsocket"), None).as_deref(),
            Some("osm:node:6003")
        );
        assert_eq!(
            geocode(&searcher, "1 Social Street", Some("Providence"), None).as_deref(),
            Some("osm:relation:6001"),
            "Woonsocket is in Providence County, not in Providence"
        );
        assert_eq!(
            geocode(&searcher, "7 Farm Lane", Some("Providence"), None).as_deref(),
            Some("osm:node:6004")
        );
    }

    fn write_any_area(writer: &mut Writer, names: &mut HashMap<RecordId, Vec<String>>) {
        use crate::{
            context::AdminContextTuple,
            pack::RecordContext,
            record::{PlaceLayer, PlaceRecord, Record},
        };

        let mut area = |layer: PlaceLayer, name: &str, object_id: i64| {
            writer
                .write(
                    &Record::Place(
                        layer,
                        PlaceRecord {
                            alt_names: Vec::new(),
                            name: name.to_string(),
                            place_type: "boundary".to_string(),
                            geometry: point_geometry(175.28, -37.78),
                            source: SourceProvenance::osm(ObjectType::Relation, object_id),
                            prominence: Default::default(),
                            wikidata: None,
                        },
                    ),
                    None,
                )
                .expect("area")
        };
        let hamilton = area(PlaceLayer::Locality, "Hamilton", 7001);
        let dickson = area(PlaceLayer::SubLocality, "Dickson", 7002);
        extend_context_names(
            writer,
            names,
            [
                (hamilton, vec!["Hamilton".to_string()]),
                (dickson, vec!["Dickson".to_string()]),
            ],
        );
        let at = [-130.0, 20.0];
        writer
            .write(
                &located(
                    address_record(
                        "osm:node:7005",
                        "",
                        "1",
                        "Jervois Road",
                        Some("Frankton"),
                        None,
                    ),
                    at,
                )
                .into(),
                None,
            )
            .expect("frankton");
        let mut te_rapa = located(
            address_record("osm:node:7003", "", "123", "Crummer Road", None, None),
            at,
        );
        te_rapa.address.areas = vec!["Te Rapa".to_string()];
        writer
            .write(
                &te_rapa.into(),
                Some(RecordContext {
                    admin_context: AdminContextTuple::from_tiers([(
                        PlaceLayer::Locality,
                        hamilton,
                    )]),
                    flags: 0,
                    ..Default::default()
                }),
            )
            .expect("stated suburb");
        writer
            .write(
                &located(
                    address_record("osm:node:7004", "", "38", "Archer Street", None, None),
                    at,
                )
                .into(),
                Some(RecordContext {
                    admin_context: AdminContextTuple::from_tiers([(
                        PlaceLayer::SubLocality,
                        dickson,
                    )]),
                    flags: 0,
                    ..Default::default()
                }),
            )
            .expect("sub-locality");
    }

    #[test]
    fn geocode_accepts_any_area_the_record_lies_in() {
        let searcher = TextSearcher::open(&*AREAS).expect("searcher");

        for locality in ["Te Rapa", "Hamilton"] {
            assert_eq!(
                geocode(&searcher, "123 Crummer Road", Some(locality), None).as_deref(),
                Some("osm:node:7003"),
                "{locality}"
            );
        }
        assert_eq!(
            geocode(&searcher, "38 Archer Street", Some("Dickson"), None).as_deref(),
            Some("osm:node:7004")
        );
        assert_eq!(
            geocode(&searcher, "123 Crummer Road", Some("Frankton"), None),
            None
        );
    }

    fn answer(searcher: &TextSearcher, reading: &str) -> Vec<String> {
        let plan = reading_plan(searcher, reading);
        answer_plan(searcher, &plan)
            .into_iter()
            .map(|hit| hit.record.id)
            .collect()
    }

    fn reading_plan(searcher: &TextSearcher, reading: &str) -> QueryPlan {
        let plan = QueryPlan::from_tagged(&tagged(reading), &[], &[], &[], |word| {
            searcher.describes_street(word)
        })
        .expect("plan");
        searcher.with_number_forms(plan).expect("number forms")
    }

    fn answer_plan(searcher: &TextSearcher, plan: &QueryPlan) -> Vec<TextSearchHit> {
        let mut contradicted = Vec::<TextSearchHit>::new();
        for scope in Scope::ALL {
            if !scope.searches(plan) {
                continue;
            }
            let (hits, kept) = searcher
                .search_plan(
                    plan,
                    &HashSet::new(),
                    scope,
                    &QueryWords::new(&plan.text()),
                    &Filters::default(),
                    None,
                    5,
                    &Session::new(SEARCH_BUDGET),
                    &mut |_, _, _, _| Ok(true),
                )
                .expect("search");
            for hit in kept {
                if !contradicted
                    .iter()
                    .any(|held| held.record.id == hit.record.id)
                {
                    contradicted.push(hit);
                }
            }
            if !hits.is_empty() {
                let mut answers = hits;
                for hit in contradicted {
                    if !answers.iter().any(|held| held.record.id == hit.record.id) {
                        answers.push(hit);
                    }
                }
                return answers;
            }
        }
        contradicted
    }

    fn place(
        writer: &mut Writer,
        layer: crate::record::PlaceLayer,
        name: &str,
        object_type: ObjectType,
        object_id: i64,
        [lon, lat]: [f64; 2],
        context: Option<crate::pack::RecordContext>,
    ) -> RecordId {
        writer
            .write(
                &crate::record::Record::Place(
                    layer,
                    crate::record::PlaceRecord {
                        alt_names: Vec::new(),
                        name: name.to_string(),
                        place_type: "boundary".to_string(),
                        geometry: point_geometry(lon, lat),
                        source: SourceProvenance::osm(object_type, object_id),
                        prominence: Default::default(),
                        wikidata: None,
                    },
                ),
                context,
            )
            .expect("place")
    }

    fn chain(
        tiers: &[(crate::record::PlaceLayer, RecordId)],
    ) -> Option<crate::pack::RecordContext> {
        Some(crate::pack::RecordContext {
            admin_context: crate::context::AdminContextTuple::from_tiers(tiers.iter().copied()),
            flags: 0,
            ..Default::default()
        })
    }

    fn located(mut record: AddressRecord, [lon, lat]: [f64; 2]) -> AddressRecord {
        record.geometry = point_geometry(lon, lat);
        record
    }

    fn street_at(id: &str, name: &str, [lon, lat]: [f64; 2]) -> StreetRecord {
        let mut street = street_record(id, name);
        street.geometry = point_geometry(lon, lat);
        street.representative_point = [lon, lat];
        street
    }

    fn write_countries(writer: &mut Writer, names: &mut HashMap<RecordId, Vec<String>>) {
        use crate::record::PlaceLayer::{Country, Locality, Region};

        let at = [-83.2, 35.05];
        let us = place(
            writer,
            Country,
            "United States",
            ObjectType::Relation,
            8001,
            at,
            None,
        );
        let canada = place(
            writer,
            Country,
            "Canada",
            ObjectType::Relation,
            8002,
            at,
            None,
        );
        writer.set_country_codes(
            [
                (us, CountryCode::parse("US").expect("code")),
                (canada, CountryCode::parse("CA").expect("code")),
            ]
            .into_iter()
            .collect(),
        );
        let carolina = place(
            writer,
            Region,
            "North Carolina",
            ObjectType::Relation,
            8003,
            at,
            chain(&[(Country, us)]),
        );
        let texas = place(
            writer,
            Region,
            "Texas",
            ObjectType::Relation,
            8004,
            at,
            chain(&[(Country, us)]),
        );
        let ontario = place(
            writer,
            Region,
            "Ontario",
            ObjectType::Relation,
            8005,
            at,
            chain(&[(Country, canada)]),
        );
        let in_region = |country, region| chain(&[(Country, country), (Region, region)]);
        let highlands_nc = place(
            writer,
            Locality,
            "Highlands",
            ObjectType::Relation,
            8006,
            at,
            in_region(us, carolina),
        );
        let highlands_tx = place(
            writer,
            Locality,
            "Highlands",
            ObjectType::Relation,
            8007,
            at,
            in_region(us, texas),
        );
        let addington = place(
            writer,
            Locality,
            "Addington Highlands",
            ObjectType::Relation,
            8008,
            at,
            in_region(canada, ontario),
        );
        extend_context_names(
            writer,
            names,
            [
                (us, "United States"),
                (canada, "Canada"),
                (carolina, "North Carolina"),
                (texas, "Texas"),
                (ontario, "Ontario"),
                (highlands_nc, "Highlands"),
                (highlands_tx, "Highlands"),
                (addington, "Addington Highlands"),
            ]
            .into_iter()
            .map(|(id, name)| (id, vec![name.to_string()])),
        );
        let in_town = |country, region, locality| {
            chain(&[(Country, country), (Region, region), (Locality, locality)])
        };
        for (id, region, context) in [
            ("osm:node:8010", "NC", in_town(us, carolina, highlands_nc)),
            ("osm:node:8011", "TX", in_town(us, texas, highlands_tx)),
            ("osm:node:8012", "ON", in_town(canada, ontario, addington)),
        ] {
            let mut address = located(
                address_record(id, "", "134", "North 5th Street", None, None),
                [-135.0, 20.0],
            );
            address.address.region = Some(region.to_string());
            writer.write(&address.into(), context).expect("address");
        }
        writer
            .write(
                &street_at("osm:way:8020", "Main Street", [-135.0, 20.0]).into(),
                in_town(us, texas, highlands_tx),
            )
            .expect("street");
    }

    #[test]
    fn a_stated_country_or_region_the_pack_knows_removes_a_record_elsewhere() {
        let searcher = TextSearcher::open(&*AREAS).expect("searcher");
        let street = "134/house_number north/street 5th/street street/street highlands/locality";

        let geocode_in = |address: &str, region: &str| {
            searcher
                .geocode_address(&AddressGeocodeOptions {
                    address: address.to_string(),
                    locality: Some("Highlands".to_string()),
                    region: Some(region.to_string()),
                    postcode: None,
                    limit: 5,
                    layer: None,
                    countries: Vec::new(),
                })
                .expect("geocode")
                .map(|found| found.hit.record.id)
        };
        assert_eq!(
            geocode_in("134 North 5th Street", "NC").as_deref(),
            Some("osm:node:8010")
        );
        assert_eq!(
            geocode_in("134 North 5th Street", "North Carolina").as_deref(),
            Some("osm:node:8010")
        );
        assert_eq!(
            geocode_in("Main Street", "TX").as_deref(),
            Some("osm:way:8020")
        );
        assert_eq!(
            geocode_in("Main Street", "Texas").as_deref(),
            Some("osm:way:8020")
        );

        assert_eq!(
            answer(&searcher, &format!("{street} nc/region")),
            ["osm:node:8010"]
        );
        let mut american = answer(
            &searcher,
            &format!("{street} united/country states/country"),
        );
        american.sort();
        assert_eq!(american, ["osm:node:8010", "osm:node:8011"]);
        assert_eq!(
            answer(&searcher, &format!("{street} ca/country")),
            ["osm:node:8012"]
        );
        assert_eq!(
            answer(&searcher, &format!("{street} atlantis/country")).len(),
            3
        );
        assert_eq!(answer(&searcher, &format!("{street} zz/region")).len(), 3);

        let matched = |context: &str| {
            let plan = reading_plan(&searcher, &format!("{street} {context}"));
            let hit = answer_plan(&searcher, &plan).into_iter().next();
            let matched = hit.expect("a hit").matched;
            (matched.region, matched.country)
        };
        let (agrees, unknown) = (PartMatch::Matched, PartMatch::Unmatched);
        assert_eq!(matched("nc/region"), (Some(agrees), None));
        assert_eq!(
            matched("united/country states/country"),
            (None, Some(agrees))
        );
        assert_eq!(matched("ca/country"), (None, Some(agrees)));
        assert_eq!(
            matched("nc/region us/country"),
            (Some(agrees), Some(agrees))
        );
        assert_eq!(matched("atlantis/country"), (None, Some(unknown)));
        assert_eq!(matched("zz/region"), (Some(unknown), None));

        let context = [
            ContextPart::of(Label::Locality, Some("Highlands")).expect("part"),
            ContextPart::of(Label::Region, Some("NC")).expect("part"),
        ];
        let fallback = searcher
            .fallback("999 Nowhere Lane", &context, &Filters::default(), None, 5)
            .expect("fallback");
        assert_eq!(
            fallback
                .iter()
                .map(|hit| hit.record.id.as_str())
                .collect::<Vec<_>>(),
            ["osm:relation:8006"]
        );
        assert!(fallback.iter().all(|hit| hit.fallback));
        assert_eq!(
            fallback[0].matched,
            QueryMatch {
                housenumber: Some(unknown),
                street: Some(unknown),
                locality: Some(agrees),
                region: Some(agrees),
                ..QueryMatch::default()
            }
        );
        let nowhere = [ContextPart::of(Label::Locality, Some("Atlantis")).expect("part")];
        assert!(
            searcher
                .fallback("999 Nowhere Lane", &nowhere, &Filters::default(), None, 5)
                .expect("fallback")
                .is_empty(),
            "a town the Pack does not know is no answer"
        );
    }

    fn write_postal_towns(writer: &mut Writer, names: &mut HashMap<RecordId, Vec<String>>) {
        use crate::record::PlaceLayer::{District, Locality, Neighbourhood};

        let kingstowne_at = [-80.5, 37.2];
        let kingstowne = place(
            writer,
            Locality,
            "Kingstowne",
            ObjectType::Relation,
            9001,
            kingstowne_at,
            None,
        );
        let peoria = place(
            writer,
            Locality,
            "Peoria",
            ObjectType::Relation,
            9002,
            [-89.6, 39.8],
            None,
        );
        let sefton = place(
            writer,
            District,
            "Sefton",
            ObjectType::Relation,
            9003,
            [-3.0, 53.46],
            None,
        );
        place(
            writer,
            Neighbourhood,
            "Bootle",
            ObjectType::Node,
            9004,
            [-2.99, 53.455],
            None,
        );
        extend_context_names(
            writer,
            names,
            [
                (kingstowne, "Kingstowne"),
                (peoria, "Peoria"),
                (sefton, "Sefton"),
            ]
            .into_iter()
            .map(|(id, name)| (id, vec![name.to_string()])),
        );
        writer
            .write(
                &located(
                    address_record(
                        "osm:node:9010",
                        "",
                        "1",
                        "Cedar Street",
                        Some("Lorton"),
                        Some("22079"),
                    ),
                    kingstowne_at,
                )
                .into(),
                chain(&[(Locality, kingstowne)]),
            )
            .expect("lorton address");
        writer
            .write(
                &PostcodeRecord {
                    postcode: "22079".to_string(),
                    country: None,
                    geometry: point_geometry(kingstowne_at[0], kingstowne_at[1]),
                    source: DerivedSourceProvenance::osm_address_records(1),
                }
                .into(),
                None,
            )
            .expect("postcode");
        writer
            .write(
                &street_at("osm:way:9020", "Hollyhock Lane", kingstowne_at).into(),
                chain(&[(Locality, kingstowne)]),
            )
            .expect("hollyhock lane");
        writer
            .write(
                &located(
                    address_record("osm:node:9011", "", "5", "Elm Street", None, None),
                    [-89.6, 39.8],
                )
                .into(),
                chain(&[(Locality, peoria)]),
            )
            .expect("peoria address");
        writer
            .write(
                &located(
                    address_record(
                        "osm:node:9012",
                        "",
                        "7",
                        "Stanley Road",
                        Some("Bootle"),
                        None,
                    ),
                    [-2.98, 53.45],
                )
                .into(),
                chain(&[(District, sefton)]),
            )
            .expect("bootle address");
        writer
            .write(
                &street_at("osm:way:9021", "Knowsley Road", [-3.0, 53.46]).into(),
                chain(&[(District, sefton)]),
            )
            .expect("knowsley road");
    }

    #[test]
    fn a_postal_town_or_a_settlement_point_nearby_agrees_with_a_record_outside_it() {
        let searcher = TextSearcher::open(&*AREAS).expect("searcher");

        assert_eq!(
            answer(
                &searcher,
                "hollyhock/street lane/street lorton/locality 22079/postcode"
            ),
            ["osm:way:9020"]
        );
        assert!(
            answer(
                &searcher,
                "hollyhock/street lane/street peoria/locality 22079/postcode"
            )
            .is_empty()
        );
        assert!(answer(&searcher, "hollyhock/street lane/street lorton/locality").is_empty());
        assert_eq!(
            answer(&searcher, "knowsley/street road/street bootle/locality"),
            ["osm:way:9021"]
        );
        assert!(answer(&searcher, "knowsley/street road/street peoria/locality").is_empty());
    }

    fn write_locality_runs(writer: &mut Writer, names: &mut HashMap<RecordId, Vec<String>>) {
        use crate::record::PlaceLayer::{Locality, Neighbourhood};

        let at = [10.6, 47.03];
        let serfaus = place(
            writer,
            Locality,
            "Serfaus",
            ObjectType::Relation,
            10001,
            at,
            None,
        );
        let fiss = place(
            writer,
            Locality,
            "Fiss",
            ObjectType::Relation,
            10002,
            at,
            None,
        );
        let ladis = place(
            writer,
            Locality,
            "Ladis",
            ObjectType::Relation,
            10003,
            at,
            None,
        );
        let dunedin = place(
            writer,
            Locality,
            "Dunedin",
            ObjectType::Relation,
            10004,
            at,
            None,
        );
        let mount_eden = place(
            writer,
            Neighbourhood,
            "Mount Eden",
            ObjectType::Relation,
            10005,
            at,
            chain(&[(Locality, dunedin)]),
        );
        extend_context_names(
            writer,
            names,
            [
                (serfaus, "Serfaus"),
                (fiss, "Fiss"),
                (ladis, "Ladis"),
                (dunedin, "Dunedin"),
                (mount_eden, "Mount Eden"),
            ]
            .into_iter()
            .map(|(id, name)| (id, vec![name.to_string()])),
        );
        for (id, number, street, context) in [
            (
                "osm:node:10010",
                "15",
                "Dorfbahnstraße",
                chain(&[(Locality, serfaus)]),
            ),
            (
                "osm:node:10011",
                "3",
                "Dorfstraße",
                chain(&[(Locality, fiss)]),
            ),
            (
                "osm:node:10012",
                "5",
                "Dorfstraße",
                chain(&[(Locality, ladis)]),
            ),
            (
                "osm:node:10013",
                "10",
                "Eden Road",
                chain(&[(Locality, dunedin), (Neighbourhood, mount_eden)]),
            ),
            (
                "osm:node:10014",
                "10",
                "Eden Road",
                chain(&[(Locality, dunedin)]),
            ),
        ] {
            writer
                .write(
                    &located(
                        address_record(id, "", number, street, None, None),
                        [-140.0, 20.0],
                    )
                    .into(),
                    context,
                )
                .expect("address");
        }
    }

    #[test]
    fn a_locality_run_is_read_as_the_places_named_in_it() {
        let searcher = TextSearcher::open(&*AREAS).expect("searcher");

        assert_eq!(
            answer(
                &searcher,
                "dorfbahnstrasse/street 15/house_number serfaus/locality serfaus/locality fiss/locality ladis/locality"
            ),
            ["osm:node:10010"]
        );
        assert_eq!(
            answer(
                &searcher,
                "10/house_number eden/street road/street mount/locality eden/locality dunedin/locality"
            ),
            ["osm:node:10013"]
        );
    }

    #[test]
    fn a_street_whose_direction_or_type_contradicts_the_query_ranks_after() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");

        let north = answer(
            &searcher,
            "5525/house_number north/street us/street highway/street 281/street",
        );
        assert_eq!(north[0], "osm:way:1520", "{north:?}");
        assert!(
            north.contains(&"osm:node:1510".to_string()),
            "ranked after, never out"
        );
        assert_eq!(
            answer(
                &searcher,
                "5525/house_number south/street us/street highway/street 281/street"
            )[0],
            "osm:node:1510"
        );
        assert_eq!(
            answer(
                &searcher,
                "3935/house_number 16th/street st/street n/street"
            )[0],
            "osm:way:1522"
        );
    }

    #[test]
    fn a_place_search_answers_with_the_places_so_named_the_best_known_first() {
        let places = TextSearcher::open(&*PLACES).expect("searcher").places();
        assert_eq!(top_ids(&places, "Stedelijk Museum")[0], "osm:node:1302");
        assert_eq!(top_ids(&places, "Van Gogh Museum")[0], "osm:node:1303");
        assert_eq!(
            top_ids(&places, "AMS")[0],
            "osm:node:1305",
            "an exact other name of the better known"
        );
    }

    #[test]
    fn a_street_whose_glued_type_contradicts_the_query_ranks_after() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");
        for (street, id) in [
            ("Wilderinksplein", "osm:node:1602"),
            ("Wilderinksstraat", "osm:node:1601"),
        ] {
            let found = top_ids_limited(&searcher, &format!("{street} 2, Hengelo"), 1);
            assert_eq!(found, [id], "{street}");
            let tagged = answer(
                &searcher,
                &format!(
                    "2/house_number {}/street hengelo/locality",
                    street.to_lowercase()
                ),
            );
            assert_eq!(tagged[0], id, "{street}: {tagged:?}");
        }
    }

    #[test]
    fn noise_around_an_address_leaves_the_address() {
        let searcher = TextSearcher::open(&*NUMBERS).expect("searcher");

        assert_eq!(
            answer(
                &searcher,
                "8600/house_number n/street main/street street/street suite/locality a/locality dayton/locality",
            )[0],
            "osm:node:1711"
        );
        assert_eq!(
            answer(
                &searcher,
                "7451/house_number warner/street ave/street ste/street g/street"
            )[0],
            "osm:node:1714"
        );
        assert_eq!(
            answer(&searcher, "5/house_number unit/street road/street")[0],
            "osm:node:1715"
        );

        let zip = answer_plan(
            &searcher,
            &reading_plan(
                &searcher,
                "8600/house_number n/street main/street street/street 45415/postcode 3457/postcode",
            ),
        );
        assert_eq!(zip[0].record.id, "osm:node:1711");
        assert_eq!(zip[0].features.postcode, Agreement::Agrees);

        let mail = reading_plan(
            &searcher,
            "1040/house_number w/street harrison/street street/street mail/street code/street 147/street",
        );
        assert!(answer_plan(&searcher, &mail).is_empty());
        let trimmed = searcher.trimmed(&mail).expect("trimmed");
        assert_eq!(
            answer_plan(&searcher, &trimmed)[0].record.id,
            "osm:node:1710"
        );
        assert!(
            searcher.trimmed(&trimmed).is_none(),
            "the whole street is one"
        );

        assert_eq!(
            answer(&searcher, "eibacher/street hauptstr/street 73/house_number"),
            ["osm:node:1712"]
        );

        let mangled = reading_plan(
            &searcher,
            "131/house_number impasse/street de/street la/street nor\u{fffd}a/street",
        );
        assert!(answer_plan(&searcher, &mangled).is_empty());
        let spellings = searcher
            .spellings(std::iter::once(&mangled))
            .expect("spellings");
        let spelt = searcher.spelt(&mangled, &spellings).expect("spelt");
        let (hits, _) = searcher
            .search_plan(
                &spelt,
                &HashSet::new(),
                Scope::Scoped,
                &QueryWords::new("131 impasse de la nor\u{fffd}a"),
                &Filters::default(),
                None,
                5,
                &Session::new(SEARCH_BUDGET),
                &mut |_, _, _, _| Ok(true),
            )
            .expect("search");
        assert_eq!(hits[0].record.id, "osm:node:1713");
        assert_eq!(typo_distance("\u{fffd}tretat", "etretat"), Some(1));
    }

    fn write_known_towns(writer: &mut Writer, names: &mut HashMap<RecordId, Vec<String>>) {
        use crate::record::PlaceLayer::{Locality, Neighbourhood, Region};

        let (auckland_at, wellington_at, christchurch_at) =
            ([174.76, -36.85], [174.776, -41.286], [172.63, -43.53]);
        let mut towns = Vec::new();
        for (object_id, (town, region, at)) in [
            ("Auckland", "Auckland", auckland_at),
            ("Wellington", "Wellington", wellington_at),
            ("Christchurch", "Canterbury", christchurch_at),
        ]
        .into_iter()
        .enumerate()
        {
            let object_id = 11000 + object_id as i64 * 2;
            let region = place(
                writer,
                Region,
                region,
                ObjectType::Relation,
                object_id + 1,
                at,
                None,
            );
            let town = place(
                writer,
                Locality,
                town,
                ObjectType::Relation,
                object_id + 2,
                at,
                chain(&[(Region, region)]),
            );
            towns.push((region, town));
        }
        let [auckland, wellington, christchurch] = towns[..] else {
            panic!("three towns");
        };
        let in_town =
            |(region, town): (RecordId, RecordId)| chain(&[(Region, region), (Locality, town)]);
        place(
            writer,
            Neighbourhood,
            "Ponsonby",
            ObjectType::Node,
            11010,
            [174.745, -36.855],
            in_town(auckland),
        );
        place(
            writer,
            Neighbourhood,
            "Thorndon",
            ObjectType::Node,
            11011,
            [174.775, -41.28],
            in_town(wellington),
        );
        extend_context_names(
            writer,
            names,
            [
                (auckland.0, "Auckland"),
                (auckland.1, "Auckland"),
                (wellington.0, "Wellington"),
                (wellington.1, "Wellington"),
                (christchurch.0, "Canterbury"),
                (christchurch.1, "Christchurch"),
            ]
            .into_iter()
            .map(|(id, name)| (id, vec![name.to_string()])),
        );
        for (id, name, at, town) in [
            (
                "osm:way:11020",
                "Ponsonby Road",
                [174.746, -36.855],
                auckland,
            ),
            (
                "osm:way:11021",
                "Lambton Quay",
                [174.776, -41.282],
                wellington,
            ),
            (
                "osm:way:11022",
                "Riccarton Road",
                [172.6, -43.53],
                christchurch,
            ),
        ] {
            writer
                .write(&street_at(id, name, at).into(), in_town(town))
                .expect("street");
        }
    }

    #[test]
    fn a_known_town_the_record_is_not_in_rejects_it_under_any_reading() {
        let searcher = TextSearcher::open(&*AREAS).expect("searcher");

        for reading in [
            "ponsonby/street road/street christchurch/locality",
            "lambton/street quay/street auckland/locality",
            "lambton/street quay/street auckland/region",
            "ponsonby/street road/street canterbury/region",
        ] {
            assert!(answer(&searcher, reading).is_empty(), "{reading}");
        }
        assert_eq!(
            answer(&searcher, "ponsonby/street road/street auckland/locality"),
            ["osm:way:11020"]
        );
        assert_eq!(
            answer(&searcher, "lambton/street quay/street wellington/region"),
            ["osm:way:11021"]
        );
        let fallback = searcher
            .fallback(
                "Ponsonby Road",
                &[ContextPart::of(Label::Locality, Some("Christchurch")).expect("part")],
                &Filters::default(),
                None,
                5,
            )
            .expect("fallback");
        assert_eq!(
            fallback
                .iter()
                .map(|hit| hit.record.id.as_str())
                .collect::<Vec<_>>(),
            ["osm:relation:11006"]
        );
        assert!(fallback[0].fallback);
    }

    static STRUCTURED: LazyLock<TempDir> =
        LazyLock::new(|| corpus("search-structured", write_structured));

    fn write_structured(writer: &mut Writer) {
        for (id, number, street, town, postcode, at) in [
            (
                "osm:node:101",
                "10",
                "Queen Street",
                "Toronto",
                "M5H 2N2",
                [-79.38, 43.65],
            ),
            (
                "osm:node:102",
                "12",
                "Queen Street",
                "Toronto",
                "M5H 2N2",
                [-79.381, 43.65],
            ),
            (
                "osm:node:103",
                "10",
                "Queen Street",
                "Ottawa",
                "K1P 5G4",
                [-75.7, 45.42],
            ),
            (
                "osm:node:104",
                "10",
                "King Street",
                "Toronto",
                "M5V 1A1",
                [-79.39, 43.64],
            ),
            (
                "osm:node:105",
                "7",
                "Hauptstraße",
                "Berlin",
                "10115",
                [13.4, 52.52],
            ),
            (
                "osm:node:106",
                "64",
                "Hauptstraße",
                "Berlin",
                "10115",
                [13.41, 52.52],
            ),
            (
                "osm:node:107",
                "3",
                "Breite Straße",
                "Potsdam",
                "14467",
                [13.06, 52.4],
            ),
            (
                "osm:node:301",
                "10",
                "James Street",
                "Hamilton",
                "L8P 4R5",
                [-79.87, 43.26],
            ),
            (
                "osm:node:302",
                "12",
                "James Street",
                "Hamilton",
                "L8P 4R5",
                [-79.871, 43.26],
            ),
            (
                "osm:node:303",
                "5",
                "Brant Street",
                "Burlington",
                "L7R 2G6",
                [-79.80, 43.33],
            ),
        ] {
            let record = located(
                address_record(id, "", number, street, Some(town), Some(postcode)),
                at,
            );
            writer.write(&record.into(), None).expect("address");
        }
        for (id, street, lon) in [
            ("osm:node:201", "Mobilheimplatz B V", 16.91620),
            ("osm:node:202", "Mobilheimplatz B XI", 16.91640),
            ("osm:node:203", "Mobilheimplatz C XI", 16.91660),
        ] {
            let record = located(
                address_record(
                    id,
                    "",
                    "19",
                    street,
                    Some("Sankt Andrä am Zicksee"),
                    Some("7161"),
                ),
                [lon, 47.789],
            );
            writer.write(&record.into(), None).expect("address");
        }
    }

    fn first_ids(searcher: &TextSearcher, query: &str) -> Vec<String> {
        searcher
            .search(&TextSearchOptions {
                query: query.to_string(),
                limit: 1,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search")
            .into_iter()
            .map(|hit| hit.record.id)
            .collect()
    }

    fn structured_answer(searcher: &TextSearcher, query: &str) -> (Vec<String>, bool) {
        LAST_STRUCTURED.with(|last| last.set(false));
        let found = first_ids(searcher, query);
        (found, LAST_STRUCTURED.with(Cell::get))
    }

    #[test]
    fn the_structured_stage_answers_as_the_text_search_does() {
        let structured = TextSearcher::open(&*STRUCTURED).expect("searcher");
        assert!(
            structured.structured.is_some(),
            "the Pack holds its structured index"
        );
        let plain = TextSearcher::open(&*STRUCTURED)
            .expect("searcher")
            .without_structured();
        for query in [
            "10 Queen Street Toronto",
            "10 Queen St, Ottawa",
            "10 Queen Street Toronto M5H 2N2",
            "12 Queen Street",
            "7 Hauptstr Berlin",
            "10 Quen Street Toronto",
        ] {
            assert_eq!(
                structured_answer(&structured, query),
                (first_ids(&plain, query), true),
                "{query}"
            );
        }
        let query = "64 Hauptstraße Potsdam";
        assert_eq!(first_ids(&structured, query), first_ids(&plain, query));
        let query = "10 Queen Street Toronto K1P 5G4";
        let found = first_ids(&structured, query);
        assert_eq!(found, first_ids(&plain, query));
    }

    #[test]
    fn of_streets_a_key_cannot_tell_apart_the_one_the_query_names_answers() {
        let searcher = TextSearcher::open(&*STRUCTURED).expect("searcher");
        assert_eq!(
            structured_answer(&searcher, "Mobilheimplatz B XI 19, Sankt Andrä am Zicksee"),
            (vec!["osm:node:202".to_string()], true)
        );
    }

    #[test]
    fn a_stated_postcode_outweighs_a_town_the_answer_is_not_in() {
        let structured = TextSearcher::open(&*STRUCTURED).expect("searcher");
        let plain = TextSearcher::open(&*STRUCTURED)
            .expect("searcher")
            .without_structured();
        assert_eq!(
            structured_answer(&structured, "10 James Street Burlington L8P 4R5"),
            (vec!["osm:node:301".to_string()], true)
        );
        let query = "10 James Street Burlington";
        let (found, answered) = structured_answer(&structured, query);
        assert!(!answered);
        assert_eq!(found, first_ids(&plain, query));
    }

    fn write_matched(writer: &mut Writer, names: &mut HashMap<RecordId, Vec<String>>) {
        use crate::record::PlaceLayer::Locality;

        let town_at = [-77.15, 38.78];
        let town = place(
            writer,
            Locality,
            "Franconia",
            ObjectType::Relation,
            12001,
            town_at,
            None,
        );
        let west = place(
            writer,
            Locality,
            "West Franconia",
            ObjectType::Relation,
            12002,
            [-78.0, 39.5],
            None,
        );
        extend_context_names(
            writer,
            names,
            [
                (town, vec!["Franconia".to_string()]),
                (west, vec!["West Franconia".to_string()]),
            ],
        );
        writer
            .write(
                &PostcodeRecord {
                    postcode: "22310".to_string(),
                    country: None,
                    geometry: point_geometry(town_at[0], town_at[1]),
                    source: DerivedSourceProvenance::osm_address_records(1),
                }
                .into(),
                None,
            )
            .expect("postcode");
        let mut street = street_record("osm:way:12001", "Bayview Avenue");
        street.geometry = line(-77.15, [38.78, 38.79]);
        street.representative_point = [-77.15, 38.785];
        writer
            .write(&street.into(), chain(&[(Locality, town)]))
            .expect("street");
        for (id, number, unit, lat) in [
            ("osm:node:12010", "10", None, 38.781),
            ("osm:node:12011", "20", None, 38.782),
            ("osm:node:12012", "30", Some("4"), 38.783),
        ] {
            let mut address = address_record(id, "", number, "Bayview Avenue", None, Some("22310"));
            address.address.unit = unit.map(str::to_string);
            writer
                .write(
                    &located(address, [-77.1502, lat]).into(),
                    chain(&[(Locality, town)]),
                )
                .expect("address");
        }
        writer
            .write(
                &street_at("osm:way:12002", "Applemint Lane", town_at).into(),
                chain(&[(Locality, town)]),
            )
            .expect("lane");
        writer
            .write(
                &located(
                    address_record(
                        "osm:node:12013",
                        "",
                        "1",
                        "Maple Street",
                        Some("Alexandria"),
                        Some("22310"),
                    ),
                    [-77.16, 38.78],
                )
                .into(),
                chain(&[(Locality, town)]),
            )
            .expect("postal town address");
    }

    #[test]
    fn each_part_a_query_states_says_how_it_matched_the_answer() {
        use PartMatch::{Matched, Partial, Unmatched};

        let searcher = TextSearcher::open(&*AREAS).expect("searcher");
        let read = |reading: &str| {
            let hit = answer_plan(&searcher, &reading_plan(&searcher, reading))
                .into_iter()
                .next()
                .expect("a hit");
            (hit.record.id, hit.matched)
        };
        let searched = |query: &str| {
            let hit = searcher
                .search(&TextSearchOptions {
                    query: query.to_string(),
                    limit: 5,
                    layer: None,
                    countries: Vec::new(),
                    focus: None,
                    bbox: None,
                })
                .expect("search")
                .into_iter()
                .next()
                .expect("a hit");
            (hit.record.id, hit.fallback, hit.matched)
        };
        let stated = |parts: &[(Label, PartMatch)]| {
            let mut matched = QueryMatch::default();
            for (label, part) in parts {
                *part_of(&mut matched, *label).expect("an address part") = Some(*part);
            }
            matched
        };
        let (number, street, unit) = (Label::HouseNumber, Label::Street, Label::Unit);
        let (locality, postcode) = (Label::Locality, Label::Postcode);
        let house = [(number, Matched), (street, Matched)];
        let with = |more: &[(Label, PartMatch)]| stated(&[&house[..], more].concat());

        for (reading, id, expected) in [
            (
                "10/house_number bayview/street avenue/street franconia/locality 22310/postcode",
                "osm:node:12010",
                with(&[(locality, Matched), (postcode, Matched)]),
            ),
            (
                "10/house_number bayview/street ave/street",
                "osm:node:12010",
                with(&[]),
            ),
            (
                "10/house_number bayview/street avenue/street 99999/postcode",
                "osm:node:12010",
                with(&[(postcode, Unmatched)]),
            ),
            (
                "10/house_number bayview/street avenue/street 223/postcode",
                "osm:node:12010",
                with(&[(postcode, Partial)]),
            ),
            (
                "1/house_number maple/street street/street alexandria/locality 22310/postcode",
                "osm:node:12013",
                with(&[(locality, Matched), (postcode, Matched)]),
            ),
            (
                "applemint/street lane/street alexandria/locality 22310/postcode",
                "osm:way:12002",
                stated(&[(street, Matched), (locality, Partial), (postcode, Partial)]),
            ),
            (
                "4/unit 30/house_number bayview/street avenue/street",
                "osm:node:12012",
                with(&[(unit, Matched)]),
            ),
            (
                "9/unit 30/house_number bayview/street avenue/street",
                "osm:node:12012",
                with(&[(unit, Unmatched)]),
            ),
            (
                "franconia/locality",
                "osm:relation:12001",
                stated(&[(locality, Matched)]),
            ),
            (
                "22310/postcode",
                "derived:osm:postcode:22310",
                stated(&[(postcode, Matched)]),
            ),
        ] {
            assert_eq!(read(reading), (id.to_string(), expected), "{reading}");
        }

        let estimate = "derived:estimate:14:osm:node:12010:osm:node:12011";
        let missed = [(number, Unmatched), (street, Unmatched)];
        for (query, id, fallback, expected) in [
            (
                "10 Bayview Avenue, Franconia 22310",
                "osm:node:12010",
                false,
                with(&[(locality, Matched), (postcode, Matched)]),
            ),
            (
                "10 Bayvew Avenue",
                "osm:node:12010",
                false,
                stated(&[(number, Matched), (street, Partial)]),
            ),
            (
                "14 Bayview Avenue",
                estimate,
                false,
                stated(&[(number, Partial), (street, Matched)]),
            ),
            (
                "40 Bayview Avenue",
                "osm:way:12001",
                false,
                stated(&[(number, Unmatched), (street, Matched)]),
            ),
            (
                "10a Bayview Avenue",
                "osm:node:12010",
                false,
                stated(&[(number, Partial), (street, Matched)]),
            ),
            (
                "10 Bayview Avenue, Springfield",
                "osm:node:12010",
                false,
                with(&[(locality, Unmatched)]),
            ),
            (
                "5 Nowhere Road, Franconia",
                "osm:relation:12001",
                true,
                stated(&[&missed[..], &[(locality, Matched)]].concat()),
            ),
            (
                "5 Nowhere Road, 22310",
                "derived:osm:postcode:22310",
                true,
                stated(&[&missed[..], &[(postcode, Matched)]].concat()),
            ),
            (
                "Franconia",
                "osm:relation:12001",
                false,
                stated(&[(locality, Matched)]),
            ),
        ] {
            assert_eq!(
                searched(query),
                (id.to_string(), fallback, expected),
                "{query}"
            );
        }

        let towns = answer_plan(&searcher, &reading_plan(&searcher, "franconia/locality"))
            .into_iter()
            .map(|hit| (hit.record.id, hit.matched.locality))
            .collect::<Vec<_>>();
        assert!(
            towns.contains(&("osm:relation:12001".to_string(), Some(Matched))),
            "{towns:?}"
        );
        assert!(
            towns.contains(&("osm:relation:12002".to_string(), Some(Partial))),
            "{towns:?}"
        );
        let split = searcher
            .search(&TextSearchOptions {
                query: "West, Franconia".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search")
            .into_iter()
            .map(|hit| (hit.record.id, hit.matched.locality))
            .collect::<Vec<_>>();
        assert!(
            !split.contains(&("osm:relation:12002".to_string(), Some(Matched))),
            "{split:?}"
        );

        let fields = searcher
            .geocode_address(&AddressGeocodeOptions {
                address: "10 Bayview Avenue".to_string(),
                locality: Some("Franconia".to_string()),
                region: None,
                postcode: Some("22310".to_string()),
                limit: 1,
                layer: None,
                countries: Vec::new(),
            })
            .expect("geocode")
            .expect("a hit")
            .hit;
        assert_eq!(
            (fields.record.id.as_str(), fields.matched),
            (
                "osm:node:12010",
                with(&[(locality, Matched), (postcode, Matched)])
            )
        );
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
            building: None,
            access: Vec::new(),
            disputed: false,
            street_postcodes: Vec::new(),
            address: AddressComponents {
                block_number_len: None,
                areas: Vec::new(),
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
                object_type: ObjectType::Node,
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
            other_ways: Vec::new(),
            road_class: crate::record::RoadClass::Residential,
            alt_names: Vec::new(),
            locality: None,
            name: label.to_string(),
            geometry: point_geometry(-79.0, 43.0),
            representative_point: [-79.0, 43.0],
            source: SourceProvenance {
                dataset: "osm".to_string(),
                object_type: ObjectType::Way,
                object_id,
                tags: Some(BTreeMap::new()),
            },
        }
    }
}
