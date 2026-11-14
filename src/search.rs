use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, hash_map::Entry},
    fmt, io,
    ops::{Bound, Range},
    path::Path,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::error::{Error, Result};
use crate::structured::{self, Place, StructuredIndex};
use levenshtein_automata::{DFA, Distance, SINK_STATE};
use serde::Serialize;
use tantivy::{
    DocAddress, DocId, DocSet, IndexReader, Score, Searcher, SegmentReader, Term,
    collector::{ScoreSegmentTweaker, ScoreTweaker, TopDocs},
    columnar::{Column, StrColumn},
    index::SegmentId,
    query::{
        BooleanQuery, BoostQuery, ConstScoreQuery, DisjunctionMaxQuery, Occur, PhrasePrefixQuery,
        PhraseQuery, Query, RangeQuery, TermQuery,
    },
    schema::{Field, IndexRecordOption},
};

use crate::{
    api::{Accuracy, Address},
    confidence::{self, Agreement, Features},
    labels,
    pack::{self, Point, PointPrecision, RecordId, Source, Summary},
    parse::{
        lattice::{
            self, CountryCode, Explanation, LabelSet, LatticeTagger, Tokens, automaton_builder,
            dictionary::{Field as NameField, designates_unit},
            dictionary_key, glued_stem,
        },
        types::{Label, Tagged},
    },
    rank::{Ranker, Signals},
    record::{
        AddressComponents, AddressRecord, DERIVED_FROM_ADDRESS_RECORDS, Layer, ObjectType,
        PlaceLayer, PoiRecord, Prominence, Record, place_rank,
    },
    records::{dequantize, quantize},
    road,
    spatial_index::{self, haversine_m},
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
};

#[derive(Clone)]
pub struct TextSearcher {
    pack: Arc<pack::Reader>,
    spatial: spatial_index::Reader,
    reader: IndexReader,
    fields: Fields,
    /// Labels the words of a query from the Pack's own dictionaries.
    tagger: Arc<LatticeTagger>,
    /// The fast fields ranking reads, per segment. The Pack's index never
    /// changes, so they are opened once rather than per query.
    columns: Arc<HashMap<SegmentId, SegmentColumns>>,
    /// What rates each answer's confidence, when the Pack has been
    /// calibrated.
    confidence: Option<confidence::Model>,
    /// What orders the hits of a query's pool, when the Pack has been
    /// calibrated; else they are chosen by their [`Evidence`].
    ranker: Option<Ranker>,
    places: bool,
    /// How long one query searches before it answers with what it found.
    budget: Duration,
    /// The structured index that answers a house number on a known street without a
    /// text search.
    structured: Option<Arc<StructuredIndex>>,
    /// Each segment with the first and one past the last of its documents'
    /// ids across the index, as the structured index numbers them.
    segments: Arc<Vec<(SegmentId, DocId, DocId)>>,
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
    /// Return only records inside this box.
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

/// What narrows a search to part of the Pack, apart from the query itself.
/// Exact terms and ranges on indexed fields, so they cost a lookup and leave
/// scoring alone: the index and its collection statistics stay whole, which
/// is what keeps scores comparable across the countries in a Pack.
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

    /// Whether a hit lies where the filters ask: inside the box, when there
    /// is one. A document's extent only says some of its records may.
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
    /// Return only records in these countries; see [`TextSearchOptions`].
    pub countries: Vec<String>,
    /// See [`TextSearchOptions::focus`].
    pub focus: Option<[f64; 2]>,
    /// See [`TextSearchOptions::bbox`].
    pub bbox: Option<BoundingBox>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TextSearchHit {
    pub record_id: RecordId,
    pub score: Score,
    pub record: Summary,
    /// What search knew of the answer when it gave it.
    #[serde(skip)]
    pub features: Features,
    /// The chance the answer is the one asked for (see [`confidence`]),
    /// `None` when the Pack has no fitted model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// Whether the answer is no match of what the query asks for but the
    /// finest area it names that the Pack holds, given because nothing more
    /// precise was found (see [`TextSearcher::fallback`]).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
    /// What the ranker knows of the answer besides its features.
    #[serde(skip)]
    pub signals: Signals,
    /// See [`RankKey::named_place`].
    #[serde(skip)]
    pub named_place: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressGeocodeOptions {
    /// Return only records in these countries; see [`TextSearchOptions`].
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
/// How long a query searches before it answers with the best it found: the
/// readings run best first, so what the budget cuts off is the least
/// likely to answer, and the tail of slow queries ends here.
#[cfg(not(test))]
pub const SEARCH_BUDGET: Duration = Duration::from_millis(300);
/// Tests run unoptimised on a loaded machine: a clock would make their
/// answers depend on it, so only [`MAX_SEARCHES`] bounds them.
#[cfg(test)]
pub const SEARCH_BUDGET: Duration = Duration::from_secs(3_600);
pub const MAX_AUTOCOMPLETE_LIMIT: usize = 20;
const MIN_AUTOCOMPLETE_QUERY_CHARS: usize = 3;
const AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS: u32 = 1_024;
/// Planet Pack holds 33,000 words of "mar" (7 ms to read) and 606,000 of "s"
/// (210 ms, read per search per keystroke).
const MAX_COMPLETION_SCAN: usize = 1 << 16;
/// Terms and postcode records read to find the area of a postcode the query
/// names (see [`TextSearcher::postcode_areas`]): a sample, enough to place
/// it and measure how far it reaches.
const POSTCODE_AREA_TERMS: usize = 256;
const POSTCODE_AREA_RECORDS: usize = 64;
/// The least a postcode area reaches, so a few postcodes standing close
/// together still leave a house between them its room.
const POSTCODE_AREA_REACH_FLOOR_M: f64 = 500.0;
/// How far around a postcode area its nearest other postcode is looked for,
/// and so the most it reaches: past the farthest
/// [`POSTCODE_NEARNESS_BANDS_M`] a postcode ranks nothing.
const POSTCODE_AREA_REACH_LIMIT_M: f64 = 200_000.0;
/// Postcode records looked at around a point to find the nearest other one.
const POSTCODE_NEIGHBOURS: usize = 16;
/// The fewest records of a country that place a postcode the Pack has no
/// record of: one postcode sharing a start with it may lie anywhere in the
/// area that start names ("m1k 4g7" is no sign of where "m1k 4c9" is), a
/// few spread over it say where that area is.
const POSTCODE_AREA_MIN_NEIGHBOURS: usize = 3;
/// How many of its reaches from a postcode's area a hit lies before it
/// contradicts the postcode. Between one and this it neither agrees nor
/// contradicts.
const POSTCODE_CONTRADICTING_REACHES: f64 = 3.0;
/// How a hit's distance from the named postcode ranks it: hits in the same
/// band tie on it and are ranked by what comes after.
const POSTCODE_NEARNESS_BANDS_M: [f64; 8] = [
    1_000.0, 2_000.0, 5_000.0, 10_000.0, 20_000.0, 50_000.0, 100_000.0, 200_000.0,
];
/// Records of one postcode looked up for a query; a code is shared by at most
/// a few countries.
const POSTCODE_RECORD_LIMIT: usize = 16;
/// Hits ranked per requested hit: the text index keeps this window of the
/// best ranked records (see [`RankKey`]), and ties within it are broken by
/// [`TieBreak`].
const RANK_WINDOW_FACTOR: usize = 3;
const MAX_RANK_WINDOW: usize = 256;
/// Documents collected at most when the rank window ends inside a run of
/// equal text scores, for the rest of [`RankKey`] to choose among.
const TIE_WINDOW: usize = 1_024;
/// What a range holding the house number scores for it: above a street
/// standing in for the number, which scores nothing, so of streets and
/// ranges that match a query's words alike the ranges reach the window
/// first, and far below a stated number's own term.
const RANGE_SCORE: Score = 1e-3;
/// How much a word found in each field weighs; the content field holds every
/// word of what a record is at weight 1, the context field every word of
/// where it is. Not rarity weights: BM25's idf already weighs a word by how
/// rare it is in each field, the `u` of [`MATCH_AGREEMENT`]. These set how
/// much more often a true match agrees on a word of its name or number than
/// on the rest of what it is, an `m` per field only an answer key could
/// measure.
const NAME_BOOST: Score = 5.5;
const NUMBER_BOOST: Score = 2.0;
/// How often a true match agrees on a part it states: the `m` of a
/// Fellegi-Sunter weight `ln(m / u)`, where `u` is how often a record at
/// random agrees, which the Pack's own counts give per country. Measuring `m`
/// per part needs an answer key, and none exists: every derived weight
/// shares this one, so their order rests on `u` alone and `ln m` is one
/// offset for them all. High, as a query and its record mostly write the
/// same postcode; the misses are typos and stale codes. The log is natural,
/// not base 2, so a weight adds to BM25's idf, itself a natural log of a
/// rarity, on one scale.
const MATCH_AGREEMENT: f64 = 0.95;
/// A word that may name an area, where the query names none, counts for less
/// there than in what a record is: "7 Lynn Place" is the street, not a place
/// in the New Lynn suburb. The share of the word's idf in what a record is
/// (see [`TextSearcher::stand_in_weights`]).
const AREA_STANDS_IN_BOOST: Score = 0.5;
/// The word of the name the parser read a typo as counts for less than a
/// word as written, and for more than the other terms near the typo, which
/// count nothing: Tantivy's fuzzy query scores every term it matches the
/// same (tantivy#563), and a term's own BM25 would rank a rare neighbour of
/// a word over the common word itself.
const CORRECTED_BOOST: Score = 0.5;
/// Words shorter than this are only matched as written: one edit turns most
/// words of two or three letters into many others.
const MIN_TYPO_CHARS: usize = 4;
/// Words at least this long may be two edits from a record's; shorter ones
/// one edit.
const LONG_TYPO_CHARS: usize = 7;
/// The first characters of a word a typo keeps as written, as the parser's
/// fuzzy lookup keeps them. The cost lever: they keep the walk of the term
/// dictionary to one branch. One, not two: a swap of the second and third
/// letters ("Mcihael") is the commonest slip, one edit with transpositions
/// counted as one, and two would rule it out.
const TYPO_PREFIX_CHARS: usize = 1;
/// The terms near one word kept per field, fewest edits then most documents
/// first: a common short word has dozens of neighbours on a planet.
const MAX_TYPO_TERMS: usize = 32;
// Wide enough that the best-known of many same-named places makes it.
const NAMED_PLACES_WINDOW: usize = 200;
/// Steps of [`Prominence::importance`] read as one level of fame: 15 is two
/// doublings of page views. Places closer than that are about as well read,
/// and what the query names them by decides (see
/// [`TextSearcher::places_by_name`]).
const FAME_TIER: u8 = 15;
/// Stated house numbers read on each side of one that no record states: a
/// few, so the nearest of the same parity can be found and the numbers of a
/// same-named street elsewhere passed over.
const NEIGHBOUR_CANDIDATES: usize = 4;
/// Documents read for neighbours: street groups and addressed POIs that match the
/// street and localities, best first.
const NEIGHBOUR_DOCUMENTS: usize = 256;
/// How far an address point may lie from the street it is on. Address points
/// sit on their lots, and the lots front the street: 100 m covers the depth
/// of nearly every lot, including large buildings set back behind parking,
/// while a point placed between addresses on two different streets of one
/// name lies far from both.
const NEIGHBOUR_STREET_RADIUS_M: f64 = 100.0;
/// How far a neighbour, and the point placed between two, may lie from the
/// line of its road. A divided road's line runs down its median, so the
/// lots along either carriageway sit further from it than a plain street's
/// (100 to 200 m on the widest); the pair is still held to
/// one road by name and nearness (see [`road::same_road_near`]).
const ROAD_SETBACK_M: f64 = 200.0;
/// A POI this near an address stands at it: a building's entrances, its
/// shops and its parcel lie within a lot's depth of its address point, the
/// depth [`NEIGHBOUR_STREET_RADIUS_M`] allows an address from its street.
const AT_ADDRESS_RADIUS_M: f64 = NEIGHBOUR_STREET_RADIUS_M;
/// Settlements held only as points read around a hit whose stated locality
/// none of its areas names (see [`TextSearcher::near_settlements`]): the
/// one it lies nearest, and the neighbours that crowd round a town where
/// suburbs are mapped as points too.
const NEAR_SETTLEMENTS: usize = 5;
/// The farthest a settlement point names the land around it: few towns'
/// built-up areas reach farther from their centre.
const NEAR_SETTLEMENT_RADIUS_M: f64 = 15_000.0;

impl TextSearchHit {
    /// A record asked for by its id rather than found by words: no score.
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
        }
    }
}

impl TextSearcher {
    pub fn open(pack_path: impl AsRef<Path>) -> Result<Self> {
        Self::from_pack(Arc::new(pack::Reader::open(pack_path)?))
    }

    /// Open the text index using an existing shared pack reader.
    pub fn from_pack(pack: Arc<pack::Reader>) -> Result<Self> {
        let index = open_text_index(pack.container())?;
        let schema = index.schema();
        let fields = Fields::from_schema(&schema)?;
        let reader = index.reader()?;
        let spatial = spatial_index::Reader::open(pack.container(), pack.records().clone())?;
        let tagger = Arc::new(LatticeTagger::from_container(pack.container())?);
        // Every query looks names up all over the parser's dictionaries.
        pack.container().prefault_sections(
            "parse/",
            crate::resources::Resources::probe().available_memory_bytes,
        );
        let mut columns = HashMap::new();
        let mut segments = Vec::new();
        for segment in reader.searcher().segment_readers() {
            let base = segments.last().map_or(0, |(_, _, end)| *end);
            segments.push((segment.segment_id(), base, base + segment.max_doc()));
            let fast_fields = segment.fast_fields();
            columns.insert(
                segment.segment_id(),
                SegmentColumns {
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
                },
            );
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
        })
    }

    /// No structured index, confidence model or ranker: those know
    /// addresses alone.
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

    /// The same searcher without its structured index: every query runs the text
    /// search, to compare the two.
    #[cfg(test)]
    fn without_structured(mut self) -> Self {
        self.structured = None;
        self
    }

    /// Search each query for at most `budget` before answering with what
    /// it found (see [`Session`]).
    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }

    /// Order hits by `ranker` from now on, or by their evidence alone.
    pub fn set_ranker(&mut self, ranker: Option<Ranker>) {
        self.ranker = ranker;
    }

    /// Whether `word`, a search key, only describes a street (its type, a
    /// direction or a qualifier), by the Pack's parser dictionaries.
    pub fn describes_street(&self, word: &str) -> bool {
        self.tagger.dictionaries().describes_street(word)
    }

    /// The Pack's confidence model, `None` when it has not been calibrated.
    pub fn confidence_model(&self) -> Option<&confidence::Model> {
        self.confidence.as_ref()
    }

    /// Rate each of `hits`, ranked best first, against the hit after it:
    /// what [`Features`] search could not know of one hit alone, then the
    /// confidence when the Pack has a model.
    fn rate(&self, hits: &mut [TextSearchHit]) {
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
            // A fallback is not what the query asks for, by what it is.
            hit.confidence = self.confidence.as_ref().map(|model| match hit.fallback {
                true => 0.0,
                false => model.rate(&hit.features),
            });
        }
    }

    pub fn search(&self, options: &TextSearchOptions) -> Result<Vec<TextSearchHit>> {
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

        // One hit past the limit, so the last one asked for has a margin;
        // [`CHOOSER_HITS`] at least, so the answer is chosen among them.
        let filters = Filters::new(options.layer.as_deref(), &options.countries, options.bbox);
        // Name pass first: address reading scans planet-wide context postings
        // and adds nothing once names fill the limit.
        let mut named = self.places_by_name(query_text, &filters, options.focus, limit)?;
        if named.len() >= limit {
            self.rate(&mut named);
            return Ok(named);
        }
        let (_, mut hits) = self.search_variants(
            query_text,
            &[],
            &filters,
            options.focus,
            (limit + 1).max(CHOOSER_HITS),
            Parses::All,
            None,
            Some(limit),
            |_, _, _, _| Ok(true),
        )?;
        if hits.is_empty() && filters.layer.is_none() {
            hits = self.fallback(query_text, &[], &filters, options.focus, limit + 1)?;
        }
        if let Some(named) = NAMED.with(RefCell::take) {
            named.choose(&mut hits);
        }
        // No ranker knows places yet: a place whose name the query covers
        // fully beats one that only repeats a street's words.
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
        self.one_per_item(&mut hits, limit)?;
        self.rate(&mut hits);
        Ok(hits)
    }

    /// `hits` with each Wikidata item once, at its best place, cut to
    /// `limit`: one place mapped twice, as a node and a boundary, is one
    /// answer.
    fn one_per_item(&self, hits: &mut Vec<TextSearchHit>, limit: usize) -> Result<()> {
        let records = self.pack.records();
        let mut items = HashSet::new();
        let mut kept = 0;
        let mut keep = Vec::with_capacity(hits.len());
        for hit in hits.iter() {
            let first = kept < limit
                && match records.wikidata(hit.record_id)? {
                    Some(item) => items.insert(item),
                    None => true,
                };
            kept += usize::from(first);
            keep.push(first);
        }
        let mut keep = keep.into_iter();
        hits.retain(|_| keep.next().unwrap_or(false));
        Ok(())
    }

    /// The answers [`Self::search`] gives without a ranker, and every hit
    /// of the pool they were chosen from, each record once: what a ranker
    /// learns from (see [`crate::calibration`]).
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
        let (_, mut hits) = self.search_variants(
            query_text,
            &[],
            &filters,
            options.focus,
            limit + 1,
            Parses::All,
            Some(&mut pooled),
            None,
            |_, _, _, _| Ok(true),
        )?;
        hits.truncate(limit);
        Ok((hits, pooled))
    }

    pub fn geocode_address(
        &self,
        options: &AddressGeocodeOptions,
    ) -> Result<Option<AddressGeocodeHit>> {
        let limit = effective_limit(options.limit);
        if limit == 0 {
            return Ok(None);
        }

        let address = options.address.trim();
        if address.is_empty() {
            return Ok(None);
        }

        // Only the address is parsed: the context fields are already labelled,
        // and join each plan as context, so they rank the records in the
        // stated area first; the checks below then reject the records they
        // contradict.
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
        // A region given by its code ("ON") is no name of the areas a record
        // lies in, so it is told apart by the regions it names (see
        // [`Self::region_agreement`]).
        let places = self.stated_places(&context);
        // Two at least, so the one answer given has a margin over the next.
        let (query, mut hits) = self.search_variants(
            address,
            &context,
            &Filters::new(options.layer.as_deref(), &options.countries, None),
            None,
            limit.max(2),
            // A weaker parse that reads the number as a unit or a word finds
            // what the query never asked for: another house, or a POI on the
            // street. Search lists such hits; a geocoded row would give one.
            Parses::Numbered,
            None,
            None,
            |session, plan, postcode, hit| {
                // A geocoded row names its postcode as a field, so a hit
                // outside it is no answer to the row, where in search it only
                // ranks down.
                //
                // A street stands in for a house number it lacks, as in search;
                // its `accuracy` says so. Refused, the query fell through to
                // parses that matched another street ("10 Eruera Street" to
                // "10 Kevin Street"). A street states no postcode, so a given
                // one must be confirmed by postcode data, not merely left
                // uncontradicted.
                let stands_in =
                    plan.house_number.is_some() && hit.record.layer == Layer::Street.as_str();
                let in_postcode = match self.postcode_agreement(hit, postcode)? {
                    Some(agrees) => agrees,
                    None => !stands_in || postcode.forms.is_empty(),
                };
                // A parse with no house number ("997" read as a unit) matches
                // a street group by its words alone, and search answers such
                // a query with the group's first address: as a geocoded row
                // that is a house the query never named, given as exact.
                let unnumbered_address =
                    plan.house_number.is_none() && hit.record.layer == Layer::Address.as_str();
                // A row's stated area must be confirmed: a hit in no named
                // area is not taken to be in it. A name the Pack has never
                // heard of confirms nothing and rejects nothing.
                // A region the Pack confirms by what it names needs no name
                // of the hit's areas to agree too: "ON" is no word of
                // "Ontario".
                let in_areas = match self.region_agreement(hit, &places, session)? {
                    Some(false) => false,
                    Some(true) => self
                        .area_agreement(hit, &stated_localities, postcode, session)?
                        .unwrap_or(stated_localities.is_empty()),
                    None => self
                        .area_agreement(hit, &stated, postcode, session)?
                        .unwrap_or(stated.is_empty()),
                };
                Ok(hit.record.point.is_some() && !unnumbered_address && in_postcode && in_areas)
            },
        )?;
        if hits.is_empty() && options.layer.is_none() {
            let filters = Filters::new(None, &options.countries, None);
            hits = self.fallback(address, &context, &filters, None, 2)?;
        }
        self.rate(&mut hits);
        Ok(hits
            .into_iter()
            .next()
            .map(|hit| AddressGeocodeHit { query, hit }))
    }

    /// Fallback only when nothing starts as typed. It reads planet-wide
    /// postings.
    pub fn autocomplete(&self, options: &TextAutocompleteOptions) -> Result<Vec<TextSearchHit>> {
        for completion in Completion::ALL {
            let hits = self.autocomplete_as(options, completion)?;
            if !hits.is_empty() {
                return Ok(hits);
            }
        }
        Ok(Vec::new())
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
        if query_text
            .chars()
            .filter(|character| !character.is_whitespace())
            .count()
            < MIN_AUTOCOMPLETE_QUERY_CHARS
        {
            return Ok(Vec::new());
        }

        let filters = Filters::new(options.layer.as_deref(), &options.countries, options.bbox);
        let tokens = autocomplete_query_tokens(&query_text);
        if let Some(number) = autocomplete_number(&tokens) {
            return self.autocomplete_reading(
                options,
                completion,
                Some(number),
                &tokens[1..],
                true,
                &filters,
            );
        }
        let as_typed =
            self.autocomplete_reading(options, completion, None, &tokens, true, &filters)?;
        let [street @ .., last] = tokens.as_slice() else {
            return Ok(as_typed);
        };
        if street.is_empty() || !is_address_number_token(last) {
            return Ok(as_typed);
        }
        // Trailing number names a place ("Pier 39") or is the house number
        // ("Kerkstraat 12"). Postcode prefix ("1211CK") is neither.
        let names_it = |hit: &TextSearchHit| {
            search_key(&hit.record.name).is_some_and(|name| {
                name.split_whitespace()
                    .any(|word| types_word(last, word, true))
            })
        };
        if as_typed.iter().any(names_it) {
            return Ok(as_typed);
        }
        let numbered =
            self.autocomplete_reading(options, completion, Some(last), street, false, &filters)?;
        Ok(if numbered.is_empty() {
            as_typed
        } else {
            numbered
        })
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
            number_text: number.map(str::to_string),
            focus: options.focus,
            near_first: options.focus.is_some(),
            typed_words: u8::try_from(words.len() + usize::from(number.is_some()))
                .unwrap_or(u8::MAX),
            ..Ranking::default()
        };
        let mut hits = self.ranked_hits(&query, &ranking, limit + 1, &mut |hit| {
            Ok(filters.admit(hit))
        })?;
        for hit in &mut hits {
            let features = &mut hit.features;
            features.prefix = true;
            features.number = number.map(|number| states_number(&hit.record, number, None));
            let label = label_words(&hit.record);
            features.dropped_words = count(words.iter().enumerate().filter(|(index, word)| {
                // The last word may be cut short by the typing.
                let last = index + 1 == words.len();
                !label.iter().any(|label| types_word(word, label, last))
            }));
        }
        // Index cannot match "mkt" to "market". Label can.
        if completion == Completion::AllWords {
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
        self.rate(&mut hits);
        hits.truncate(limit);
        Ok(hits)
    }

    /// The house number, then what a record is and where. See
    /// [`Self::house_number_query`] for what `pass` lets stand in for the
    /// number, and [`Self::subject_clauses`] for `known`.
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

    /// The documents whose extent meets `bbox`, scored at zero like every
    /// filter.
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

    /// What a record is and where. A required subject word must be a word
    /// of a record's street when the query asks for a house number on it,
    /// else of what the record is; the other subject words and the unplaced
    /// words only rank, unless the plan has no subject: then
    /// [`unplaced_floor`] of its unplaced words must match. The words of a
    /// locality part that `known` holds are required of where a record is
    /// unless `pass` is lenient; one the Pack has never heard of only ranks,
    /// since it cannot say a record is elsewhere. A region or country part
    /// only ranks, since few records state the "ON" a query may write, and
    /// so does a postcode part, since a record's tags often state a
    /// different postcode than the one written.
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
        // A query the parser found no places in may still name one
        // ("Riverdale Farm Toronto" when the Pack does not know Toronto), so
        // any of its words may name an area. So may one whose localities
        // matched nothing, and so may any word the parser could not place.
        let anywhere = plan.context.is_empty() || pass == Pass::Lenient;
        // A word of a stated area is searched there already; the subject's
        // own copy is a separate word ("Eden" of "Eden Road, Mount Eden")
        // and standing in would let that area absorb it.
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
        // Areas may answer for some required words, never all: else any
        // street in Mount Eden answers "10 Eden Road Mount Eden" read as one
        // street name. Scored at zero, the words already rank above.
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
            // A word that only describes a street ("road") names nothing: of
            // "Ponsonby Road, Christchurch", any road in Christchurch would
            // meet a floor it counted toward. It ranks, unless every word is
            // one.
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

    /// `what` a record is, or else the word in where it is, for less (see
    /// [`AreaWords::stand_ins`]).
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
            // A typo's neighbours still find the record, for nothing, as
            // they do in what it is: the weight of a word no record holds
            // is the largest there is.
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

    /// What each subject and unplaced word of `plan` scores standing in for
    /// an area: [`AREA_STANDS_IN_BOOST`] of its idf in the field a house
    /// number's street is searched in, else in what a record is, or in where
    /// records are when that is lower ("Toronto" names few streets). A word's
    /// own BM25 in where records are would not do: "road" is rare among
    /// area names and common among streets, so "47 Mill Road" would find
    /// a Millton Park Road in a locality named Mill Road before Mill Road
    /// itself. A word scores near its idf where it is asked for, so half of
    /// it ranks the stand-in below.
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

    /// The names of a locality part in where a record is: every word of each,
    /// and of every name unless the names cannot all hold one address, when
    /// one will do (see [`StatedNames::any`]).
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

    /// The places a locality part names, by the words of it the Pack knows
    /// (`known`); `None` when it knows none, so the part says nothing of
    /// where a record is.
    ///
    /// A part that is itself the name of a place the Pack knows names that
    /// place. Else it is read as the names in it, longest first from its
    /// start, and the words no name covers only rank: "Serfaus
    /// Serfaus-Fiss-Ladis", a village and the ski area named after it and
    /// two neighbours, is Serfaus, Fiss and Ladis. Names that lie one in
    /// another are one address's ("Mount Eden Auckland") and must all agree;
    /// names that cannot all hold one address (Serfaus, Fiss and Ladis are
    /// neighbours) are the query naming its area more than once, and one
    /// agreeing will do. A part with no name in it the Pack's dictionaries
    /// hold keeps its known words as one name.
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

    /// A locality or region a query names, as [`Self::area_agreement`]
    /// checks it: its names (see [`Self::stated_names`]) and whether it is a
    /// town addresses of the query's postcode state, `postcode` as the
    /// postcode field indexes it. `None` when the Pack knows no word of it.
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

    /// Whether some address of the postcode written `postcode` states one
    /// of `names` as where it is: the town of a postal address, which need
    /// not be any area the address lies in ("Alexandria, VA 22310" is a
    /// postal town over part of Fairfax County). The Pack's own addresses
    /// say so, so no table of postal towns is needed.
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

    /// The countries and regions the parts of a query name, as the Pack
    /// knows them (see [`StatedPlaces`]).
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
                    // "CA" read as a region may still be Canada, not
                    // California: a key that is also another country's name
                    // says nothing of the country.
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

    /// Whether a hit lies in the countries and regions a query names, `None`
    /// when nothing says either way. A hit in another country than one the
    /// query names contradicts it, and so does one in another country than
    /// the one a stated region lies in ("NC" lies in the United States,
    /// whatever else "Highlands" names). A region the Pack lists by its key
    /// agrees with a hit whose chain holds one of its regions or whose
    /// address states it, and a hit in other regions contradicts it. A
    /// country or region the Pack does not know only ranks.
    fn region_agreement(
        &self,
        hit: &TextSearchHit,
        places: &StatedPlaces,
        session: &Session,
    ) -> Result<Option<bool>> {
        if places.countries.is_empty() && places.regions.is_empty() {
            return Ok(None);
        }
        let country = hit.features.country;
        if let Some(country) = country
            && !places.countries.is_empty()
            && !places.countries.contains(&country)
        {
            return Ok(Some(false));
        }
        let mut agreement = (!places.countries.is_empty() && country.is_some()).then_some(true);
        for region in &places.regions {
            if let (Some(stated), Some(country)) = (region.country, country)
                && stated != country
            {
                return Ok(Some(false));
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
                agreement = Some(true);
            } else if !chain.is_empty() {
                return Ok(Some(false));
            }
        }
        Ok(agreement)
    }

    /// The words found in where some record is: those the Pack knows as a
    /// word of an area. One no record's areas hold says nothing of where a
    /// record is.
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

    /// Any word of a context part in where a record is, or all of it as one
    /// postcode ("m6g 1b8").
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

    /// One word, in the fields that hold what a record is.
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

    /// One word, in the street a record is on (or is).
    fn street_word_query(&self, word: &str, spellings: &Spellings) -> Box<dyn Query> {
        self.spelt_query(
            self.fields.street_text,
            word,
            IndexRecordOption::WithFreqs,
            spellings,
        )
    }

    /// One word, in the fields that hold where a record is: a word of its
    /// areas, or its whole postcode.
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

    /// The records that state `term` as their postcode, each scored what
    /// agreeing on it weighs in its country (see [`PostcodeWeights`]).
    /// `None` when the Pack holds no postcode of the term: nothing says how
    /// rare it is.
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
                    // A record in a country is weighed by that country's
                    // counts, never by those of the postcodes no country
                    // holds.
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

    /// `word` in `field`, or a term `spellings` let it match instead, which
    /// scores less (see [`CORRECTED_BOOST`]).
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

    /// What each word of `plans` may match besides itself, worked out once
    /// for the whole query: the parser's reading of a typo, and the index
    /// terms its typos may be of (see [`Self::near_terms`]), in each field a
    /// plan searches it in. Every search of the query matches them, scored
    /// below the word as written, so no search runs again to allow a typo.
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

    /// The terms of `field` that start with `prefix` and that `dfa` accepts,
    /// other than `word` and `corrected`: at most
    /// [`MAX_TYPO_TERMS`], fewest edits then most documents first. A typo
    /// turns a common word into a rare one far more often than the reverse
    /// (the noisy channel of spelling correction), so of a word the index
    /// holds only the terms more common than it are kept: "maine" may be a
    /// typo of "main", "main" is no typo of "maine", and a common word keeps
    /// no neighbours to search at all.
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
        // No term holds the byte 0xFF, which UTF-8 never uses, so every term
        // with the prefix sorts below it.
        let mut upper = prefix.as_bytes().to_vec();
        upper.push(0xFF);
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

    /// The house number a record states, or an interpolation range that
    /// contains it. In the later passes a street, which has no numbers, also
    /// stands in for it. Range and street score nothing for the number.
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
        let mut stated = vec![
            Box::new(BoostQuery::new(Box::new(term(&number.text)), NUMBER_BOOST)) as Box<dyn Query>,
        ];
        // A named unit also finds its flat as written with it ("29/181"),
        // which the build keeps where the street could not say which side
        // is the unit. It scores what the house number does, never more: a
        // flat's number is rarer than the house's, so its own score would
        // rank a street with that flat above the street the query names
        // ("Unit 2, 1 Balfour Street" answered on a Balfour Road with a
        // 2/1). The unit chooses among the addresses of one street in the
        // tie-break.
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

    /// Both products share `context_text`: a query matching on it alone
    /// would reach the other product's records.
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

    /// Records in any of `countries`. `None` when none are asked for, which
    /// leaves the query untouched rather than adding a clause matching all.
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
        // Scored at zero: the filter narrows what is answered, it does not
        // rank it. A plain term query would add its own BM25 score to every
        // hit, and because that score depends on how many records the country
        // holds, the same query would rank differently filtered than
        // unfiltered -- the cross-index comparison problem reintroduced
        // inside one index.
        (!clauses.is_empty()).then(|| {
            Box::new(ConstScoreQuery::new(
                Box::new(BooleanQuery::new(clauses)),
                0.0,
            )) as Box<dyn Query>
        })
    }

    /// Search the parser's best readings of the query and return the hits
    /// `accept` keeps, best first, with the text of the reading that found
    /// the first. `context` joins every reading.
    ///
    /// Each reading is searched once with every word's typos (see
    /// [`Self::spellings`]), scored below the words as written, and once
    /// more with its localities only ranking when that could find better
    /// evidence. A reading is also searched with the number its house
    /// number starts with ("407" of "407A") when it finds nothing as
    /// written. Every hit joins one pool, with the [`Evidence`] it rests on:
    /// fewer typos, then a stricter [`Pass`], then a confirmed postcode,
    /// then the parser's score of its reading. Readings run best first, and
    /// a search runs only when what it could find outweighs the best
    /// evidence so far, so a query the best reading answers exactly in its
    /// localities runs one search. A hit whose own postcode or street
    /// contradicts the query (see [`Self::search_plan`]) joins the pool but
    /// rests on no evidence: "8 High Street, Deloraine TAS 7304" must not be
    /// answered by the 7018 High Street 163 km away just because it states
    /// the number.
    ///
    /// When no reading finds anything, each is searched again with its
    /// street cut to the words of it that name a street the Pack has (see
    /// [`Self::trimmed`]). The searches stop once the query's [`Session`]
    /// budget is spent, and the pool answers with what it holds. Fails when
    /// the query has no words to search.
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
    ) -> Result<(String, Vec<TextSearchHit>)> {
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
        NAMED.with(|named| named.replace(Some(choose::Named::of(first_plan))));
        // Where the best reading says the record is holds in every reading:
        // a later one that reads "Christchurch" of "Ponsonby Road
        // Christchurch" as a region, which only ranks, must not answer with
        // the Ponsonby Road in Auckland.
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
        // Readings past the parser's usual best only widen the structured stage:
        // the text search reads as many as it always has.
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
            match answer {
                Some(answer) if answer.1.len() >= wanted => return Ok(answer),
                // Fewer answers than asked for: they come first, and the text
                // search gives the rest.
                Some(answer) => first = Some(answer),
                None => {}
            }
            // The text search keeps its whole budget: the structured stage's time
            // is its own.
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
        let (query, hits) = match &self.ranker {
            Some(ranker) => pool.ranked(ranker, &readings, limit),
            None => pool.chosen(&readings, limit),
        }
        .unwrap_or((first_query, Vec::new()));
        // The text search found nothing: the answer the structured stage left
        // to it beats the bare area the query names, all that is left.
        if hits.is_empty()
            && first.is_none()
            && let Some(declined) = declined
        {
            let (hits, _) =
                self.structured_judged(&declined.plan, declined.hits, &search, &session)?;
            return Ok((declined.text, hits));
        }
        Ok(match first {
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
        })
    }

    /// Answer an address or a street from the structured index `structured`, before any
    /// text search (see [`crate::structured`]).
    ///
    /// Every reading that names a street and a number, best first, finds
    /// the documents of its street holding the number inside the most places
    /// it names, a spot each, the best of each spot hydrated; a number the
    /// street lacks there is placed between its neighbours, else the street
    /// answers. A query no reading numbers is answered by its street. The
    /// parser scores each candidate against the whole query by joint
    /// decoding, as [`Self::explain`] scores the text search's hits, plus
    /// [`STRUCTURED_W`] for each place word holding it. The best answers when two
    /// fitted models say it is likelier within 100 m than the text search's
    /// answer by [`STRUCTURED_DELTA`]; else `None`, and the text search runs as it
    /// always has. An answer left to the text search goes to `declined`,
    /// unless it is a street held by no place word, two spots of a street
    /// explained alike, coarser than a street or outside the towns the query
    /// states.
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
        LAST_STRUCTURED.with(|last| {
            last.set(StructuredTrace {
                answered: false,
                support: best.found.support,
                gap: best.gap,
            });
        });
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
        // Two spots of one street explained alike: either may be the town.
        if best.found.form == FORM_STREET && margin <= 0.0 {
            return Ok(None);
        }
        // A street answer is judged row by row by the hand-off models; the
        // coarser types stay with the text search.
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
        // A lone name may be a street, a place or a business: the text
        // search ranks one against the other.
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
            // Outside the towns the query states, an answer is never given,
            // not even when the text search finds nothing.
            if Self::in_stated_towns(&hits[0].features, towns_known) {
                *declined = decline(hits);
            }
            return Ok(None);
        }
        #[cfg(test)]
        LAST_STRUCTURED.with(|last| {
            last.set(StructuredTrace {
                answered: true,
                ..last.get()
            });
        });
        Ok(Some((reading.text.clone(), hits)))
    }

    /// The structured stage's answers to `plan`, in its order, each with the
    /// [`Features`] the text search would give it (see [`Self::judged`]),
    /// and whether the Pack knows every town `plan` states.
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
            hits,
            session,
            usize::MAX,
        )?;
        let mut hits = kept.into_iter().chain(contradicted).collect::<Vec<_>>();
        hits.sort_by_key(|hit| order.iter().position(|id| *id == hit.record_id));
        Ok((hits, towns_known))
    }

    /// The finest area each of `readings` names, when the structured stage finds
    /// no street: its postcode, else its town, district or neighbourhood,
    /// else its region, each an area the Pack holds that nests with every
    /// other place the reading names (see [`Dictionaries::nest`]), as the
    /// text search's [`Self::fallback`] requires. Its point is the Pack's
    /// own point of the area; its layer says what it is.
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
        // A street the query names found inside the postcode it names says
        // more than any area alone, which would only tie with it.
        if found.iter().any(|found| found.entry == "postcode-street") {
            found.retain(|found| found.entry == "postcode-street");
        }
        Ok(found)
    }

    /// The street of `reading` inside the footprint of the postcode it
    /// names, found by its words as [`Self::street_matches`] finds one in a
    /// town: its record, or the address stating the reading's number, at
    /// the record's point. A postcode names a few streets; the one the query
    /// writes says where in it the answer is.
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

    /// The middle of the road of `docs` (see [`road::road_middle`]) inside
    /// the postcode `plan` names: of the addresses that state it, else of
    /// all those in its cells. `None` when none has a point.
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

    /// Whether the postcode record `postcode` lies in one of the areas
    /// `areas`, or its chain says nothing either way.
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

    /// The spot bests of `readings`, best first, up to the first that finds
    /// the exact address inside every place it names, and how many documents
    /// of their streets they were chosen from. The readings after it are the
    /// parser's less likely ones, and each costs a street lookup.
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

    /// One reading's candidates: its street's documents (found by its key,
    /// else by its words among the streets of the places it names), those
    /// holding its number (when `numbered`) inside the most of those places,
    /// a spot each, the best of each spot hydrated, at most [`STRUCTURED_SPOTS`].
    /// A number none holds there is placed between its neighbours on its
    /// side, else between any, else the street answers; without a number
    /// the street answers. Else why there are none. With them, how many
    /// documents its street lists hold, and whether one is the exact
    /// address inside every place the reading names.
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
        // A street alone answers only where every place the reading names
        // holds it: a street of the name in one of them but not the others
        // is most often another town's.
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
        // The street the key names lies in none of the places the query
        // names: one of their own streets whose words the query writes
        // otherwise ("Łokietka" of "Władysława Łokietka") is likelier.
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
        // A document under two keys of the street counts once, at its best.
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
                    // A street answer is the street's own record before any
                    // one address on it.
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
                        if item.address && spot.points.len() < SPOT_POINTS {
                            spot.points.push(item.point);
                        }
                    }
                    None if room => spots.push(Spot {
                        docs: vec![item.doc],
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
                    let Some(address) = self.doc_address(spot.head.doc) else {
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

    /// The street of `spot` as an answer: its best record hydrated, at the
    /// centre of the densest [`STREET_DISK_M`] disk of the spot's address
    /// points, the point likeliest within that distance of any one address
    /// on it; at the record's own point when the spot has none.
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
        // An answer's id names the record it describes: an address on the
        // street is no answer for the street.
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

    /// The street answers `pieces` (each with its spot's address points)
    /// with the pieces of one road joined (see [`road::road_groups`]): one
    /// answer, the first piece's record at the middle of the points of
    /// them all, which lie in the places the query names. Two pieces of one
    /// road are one answer, not a tie between two.
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

    /// The number of `plan` placed between its stated neighbours on its own
    /// side of the street of `docs` (odd or even), inside the range of the
    /// numbers they state: on most streets the numbers of one side rise
    /// along it. Its layer says it is interpolated.
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

    /// The streets in the cells of the places `places` names whose words
    /// the reading's street words name: each word of four or more letters
    /// that is not a type or direction as written or a typo away, a short
    /// word its letters in order from the first ("pg" of "passeig"), a word
    /// of up to three letters without a digit perhaps not at all; in any
    /// order, the street perhaps with words more. At most
    /// [`MATCHED_STREETS`] of them, those naming the most query letters.
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

    /// Of `docs`, those holding the number of `plan`, each with its
    /// [`form_name`] and whether the base of the number (`base_plan`)
    /// holds it rather than the number as written.
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

    /// `found`, one per record at its best support and number form, each
    /// with its gap: the parser's score of the query read as the record
    /// reads it (see [`Self::explain`]) less its best reading's, and its
    /// score, the gap and [`STRUCTURED_W`] for each place word holding it; best
    /// first (ties by number form, then support); with the best score of a
    /// record in another spot. `None` when the parser explains none.
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

    /// The keys a reading's street may have in the structured index, each with how
    /// it was written: as the query writes it, its ambiguous abbreviations
    /// read as a type (see [`Self::street_keys`]), and as the parser
    /// corrected it.
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

    /// The places the words of `reading` may name: each word the reading
    /// leaves to no street, number, unit or country whose probability of
    /// naming a place, over every labelling the parser weighs
    /// (`probabilities`, one per word), is at least [`PLACE_FLOOR`]. Each
    /// run of such words is read longest first, as written and, for a
    /// postcode, without its spaces; then what is left one edit off, when
    /// one place alone is that near. A place counts by the mean probability
    /// of its words, so a word only the best reading calls a town, or a
    /// unit word it calls one, weighs what the parser thinks of it.
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

    /// Add to `supported` each document of `list` inside a place of any of
    /// `words`, with how many of `words` hold it. Only the documents near a
    /// place kept as cells are read, as ranges of the list (it is sorted by
    /// cell); a place kept only as its extent (a region, a large city) is
    /// checked on those documents and never read through, unless no word
    /// names a smaller place: then its extent is read when it holds at most
    /// [`EXTENT_READ`] of the list.
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

    /// Whether the structured stage may give `hit`, its best answer, judged
    /// (see [`Self::structured_judged`]): only when the rules the text search
    /// ranks by hold, as the text search's own best would. It is not a
    /// business, which the text search weighs by its name; its postcode
    /// does not contradict the stated one; it lies in the stated towns (see
    /// [`Self::in_stated_towns`]); it states the unit asked for; and its
    /// street's type or direction does not contradict the query's. Else the
    /// text search answers.
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

    /// Whether an answer with `features` lies in the towns its query states,
    /// as far as the Pack knows them all (`towns_known`). A stated postcode
    /// it agrees with outweighs a town it does not: its street and number
    /// already agree, and a query may name a postal or a neighbouring town;
    /// the town it does not lie in still costs it in the structured stage's
    /// score.
    fn in_stated_towns(features: &Features, towns_known: bool) -> bool {
        match features.area {
            Agreement::Agrees | Agreement::Unstated => true,
            Agreement::Unknown => !towns_known,
            Agreement::Contradicts => features.postcode == Agreement::Agrees,
        }
    }

    /// The countries a structured-stage lookup is held to: those the caller asks
    /// for, else those the plan names; any when neither names one.
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

    /// The number of `plan` placed between its stated neighbours on the
    /// street of `docs`, as [`Self::place_between_neighbours`] places it
    /// from the documents a text search finds. Every document of the road
    /// is read, up to [`STRUCTURED_CANDIDATES`]: the numbers nearest the query's
    /// may be under any spelling of it and anywhere along it, not in the
    /// first documents by id.
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

    /// The document of id `doc` across the index, as the structured index numbers it.
    fn doc_address(&self, doc: DocId) -> Option<DocAddress> {
        let ord = self.segments.iter().position(|(_, _, end)| doc < *end)?;
        Some(DocAddress::new(ord as u32, doc - self.segments[ord].1))
    }

    /// Of `docs`, those that state `number` (or its flat with `unit`) or
    /// hold it on a range, what [`Self::house_number_query`] matches in its
    /// strict pass, each with whether it states it.
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
        let mut terms = vec![number.text.clone()];
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

    /// The columns of the segment holding document `doc`, and its id there.
    fn doc_columns(&self, doc: DocId) -> Option<(&SegmentColumns, DocId)> {
        let &(segment, base, _) = self.segments.iter().find(|(_, _, end)| doc < *end)?;
        Some((self.columns.get(&segment)?, doc - base))
    }

    /// The layer of document `doc` as the structured index tells layers apart (see
    /// [`structured_layer`]): its first record's.
    pub(crate) fn doc_layer(&self, doc: DocId) -> Option<u8> {
        let (columns, doc) = self.doc_columns(doc)?;
        let record_id = columns.record_id.first(doc)?;
        let layer = self.pack.records().header(record_id).ok()?.layer;
        Some(structured_layer(layer))
    }

    /// The middle of document `doc`'s extent in 1e-7 degrees, `None` when
    /// it has none.
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

    /// The cell of document `doc`, as the structured index sorted its street lists
    /// by: of the middle of its extent.
    pub(crate) fn doc_cell(&self, doc: DocId) -> u32 {
        let [lon, lat] = self.doc_point(doc).unwrap_or([0, 0]);
        structured::cell(lon, lat)
    }

    /// Document `doc`'s own postcode and its nearest centroid's, compacted
    /// as the rank columns hold them; empty when it has none.
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

    /// The text index's segments and their document counts, in the order
    /// the structured index numbers documents by.
    pub(crate) fn structured_segments(&self) -> Vec<(String, u32)> {
        self.segments
            .iter()
            .map(|(segment, base, end)| (segment.uuid_string(), end - base))
            .collect()
    }

    /// A street's keys in the structured index, from its words as the parser writes
    /// a query's street (see [`expand_street_words`]): the words that name
    /// it, without those that only describe it, its type, a direction or a
    /// qualifier, as a query's street requires only the former (see
    /// [`SubjectWord::required`]). A type glued onto a name is left out
    /// alike, so "Hauptstr", "Hauptstraße" and "Haupt Straße" key as one.
    /// An abbreviation the parser's tables expand is keyed as its full form.
    ///
    /// An abbreviation of both a type and a name ("st", "dr") cannot be
    /// read one way for every street: "Main St" is "Main Street", "St Johns"
    /// is "Saint Johns". So a street has a second key where such a word is
    /// left out as a type, and record and query alike are keyed both ways.
    /// The second, when there is one, is last; empty for no words.
    pub(crate) fn street_keys(&self, words: &[String]) -> Vec<String> {
        street_keys_of(words, &|word| self.describes_street(word))
    }

    /// What the structured index keys of document `doc` (see [`crate::structured`]).
    /// An address, interpolation, street or POI: the keys of each street it
    /// is on (or is), its record, and the places it states or lies in, in
    /// its cell. An area or a postcode: its names, in the cells of its
    /// extent.
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

    /// Every area name key and region key the parser's dictionaries list,
    /// with the areas it names.
    pub(crate) fn area_names(&self) -> Vec<(String, Vec<u32>)> {
        self.tagger.dictionaries().area_lists()
    }

    /// Search `readings[range]` into `pool`, best first, each search only
    /// when it could outweigh the best [`Evidence`] found so far.
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
                let variants = std::iter::once(reading.plan.clone())
                    .chain(reading.plan.with_base_house_number());
                for plan in variants {
                    if best.is_some_and(|held| !held.beaten_by(scope.best_pass())) {
                        break;
                    }
                    crate::stop::check()?;
                    if !session.allows_search() {
                        return Ok(());
                    }
                    let (hits, contradicted) = self.search_plan(
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
                    let found = !hits.is_empty();
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

    /// `plan` with each place the parser read as a typo of a name it knows
    /// read as that name, unless the Pack knows the place as written, and
    /// with `spellings` for its words.
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
            }
        }
        spelt.spellings = Arc::clone(spellings);
        Ok(spelt)
    }

    /// The plans of the parser's best labellings of `query_text`, best first,
    /// each with `context` after its own parts, the parser's score of each,
    /// and the query's lattice to explain answers with (see
    /// [`Self::explain`]). `parses` says which are kept. The country the
    /// query writes is its country in each (see [`country_not_town`]).
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
        for (index, parsed) in parsed.into_iter().enumerate() {
            let written = self.written_country(&parsed.pieces);
            let read = country_not_town(parsed.pieces, written, parsed.country);
            if let Some(plan) =
                QueryPlan::from_tagged(&read, &parsed.corrections, context, |word| {
                    self.tagger.dictionaries().describes_street(word)
                })
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

    /// Score each distinct record of `pool` by how well the parser reads
    /// the query as that record reads it (see [`LatticeTagger::explain`]):
    /// each query word may take only the labels of the record's parts it
    /// names, and no label of a part the record has but the word does not
    /// name, so a word the answer cannot account for costs what labelling
    /// it something else costs. Joint decoding of the parse and the answer,
    /// on the parser's own scale, against its best labelling.
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

    /// The finest area a query names that the Pack holds, when nothing it
    /// asks for is found (#31): its postcode, else a place it names, else
    /// its region, else its country, each in the coarser areas the query
    /// names and agreeing with every country and region it states, so a
    /// fallback never answers in an area the query did not name. Every hit
    /// is marked [`TextSearchHit::fallback`]: it says where the query is,
    /// not what it asks for, and its `layer` and `accuracy` say how coarse
    /// it is. Empty when the query names no area the Pack holds.
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
                        }
                        return Ok(hits);
                    }
                }
            }
        }
        Ok(Vec::new())
    }

    /// `plan` with its street cut to the longest run of its words that is
    /// the name of a street the Pack has, the rest of them only ranking:
    /// "W Harrison Street M/C 147" is West Harrison Street and a mail code,
    /// "Rue de la Riaille Sur les bords du" the street and where it runs.
    /// A run of only words that describe a street ("road") names none.
    /// `None` when the whole street is such a name already, or no part of
    /// it is.
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

    /// The hits of one search of one plan that `accept` keeps, at most
    /// `limit`, and apart from them the kept hits whose own postcode or
    /// street contradicts the plan (see [`Self::search_variants`]). A
    /// search in the plan's localities finds the house number stated or on
    /// a range, and else a street standing in for it, or the number placed
    /// between its stated neighbours; each hit's [`Pass`] says which. A
    /// search that does not require the plan's localities still leaves out
    /// a hit that lies in other ones: "Ponsonby Road Christchurch" is not
    /// answered in Auckland. Every search leaves out a hit in another country or region
    /// than one the plan names and the Pack knows (see
    /// [`Self::region_agreement`]), and every pass does with the words of
    /// `placed` (the localities of the best reading the Pack knows) the plan
    /// reads as a region or a country, which only rank, and with the plan's
    /// unplaced words the Pack knows as an area's: each is where the query
    /// says the record is, unless it is a word of the record itself (see
    /// [`Self::in_loose_areas`]). A plan with no subject is exempt: the
    /// parser placed none of its words, and a few of them may go unmatched
    /// (see [`unplaced_floor`]). Each hit carries the [`Features`] of what
    /// found it.
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
        // Every stated locality, known or not, for what the hits rest on.
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
        let mut keep = |hit: &TextSearchHit| -> Result<bool> {
            Ok(
                self.kept(&checks, &filters, hit, &ranking.postcode, session)?
                    && accept(session, plan, &ranking.postcode, hit)?,
            )
        };
        let query = self.build_query(plan, pass, &filters, &areas)?;
        let mut candidates = self.ranked_hits(&query, &ranking, limit, &mut keep)?;
        // A number no record states nor any range holds is placed between
        // its stated neighbours, before the streets that stand in for it.
        let estimated = scope == Scope::Scoped
            && plan.house_number.is_some()
            && !candidates.iter().any(|hit| hit.signals.house_level)
            && filters
                .layer
                .map(str::trim)
                .filter(|layer| !layer.is_empty())
                .is_none_or(|layer| layer == Layer::Address.as_str());
        if estimated
            && let Some(estimate) = self.place_between_neighbours(plan, pass, &ranking, &areas)?
            && keep(&estimate)?
        {
            candidates.insert(0, estimate);
        }
        self.judged(
            plan,
            scope,
            &ranking,
            &localities_stated,
            candidates,
            session,
            limit,
        )
    }

    /// What a search's ranking reads of `plan`: its words, number, unit
    /// and `postcode`, and the caller's `focus`.
    fn plan_ranking(
        &self,
        plan: &QueryPlan,
        query_words: &QueryWords,
        focus: Option<[f64; 2]>,
        postcode: QueryPostcode,
    ) -> Ranking {
        Ranking {
            query_words: query_words.clone(),
            house_number: plan.house_number.as_ref().and_then(HouseNumber::plain),
            number_text: plan.house_number.as_ref().map(|number| number.text.clone()),
            flat_text: plan
                .house_number
                .as_ref()
                .zip(plan.unit.as_deref())
                .map(|(number, unit)| number.flat_text(unit)),
            unit: plan.unit.clone().or_else(|| {
                let number = plan.house_number.as_ref()?;
                Some(number.text.split_once(' ')?.0.to_string())
            }),
            numbered: plan.house_number.is_some(),
            postcode,
            focus,
            near_first: false,
            typed_words: 0,
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

    /// The words of `plan`'s localities the Pack knows, and each locality
    /// as stated (see [`Self::stated_area`]), `None` for one it knows no
    /// word of. `postcode`, as the postcode field indexes it, lets a stated
    /// town be its postal town.
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

    /// What every hit of a search of `plan` is checked against (see
    /// [`Self::kept`]): the countries and regions it names, the localities
    /// in `stated`, and the words it states without requiring them.
    fn plan_checks(
        &self,
        plan: &QueryPlan,
        placed: &HashSet<String>,
        stated: Vec<StatedArea>,
    ) -> Result<Checks> {
        let places = self.stated_places(&plan.context);
        // A country the query names narrows retrieval as a filter would, so
        // the rank window holds its records and not a namesake abroad.
        let countries = places
            .countries
            .iter()
            .map(|country| country.as_str().to_string())
            .collect::<Vec<_>>();
        // A word of a country the Pack knows is held to that country, not to
        // the names of the hit's areas: "ES" read first as a locality and
        // then as Spain is Spain.
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

    /// Whether `hit` passes `filters` and lies where `checks` say: not in
    /// another country or region, nor in another of the stated localities,
    /// and in the areas of the loose words.
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

    /// `candidates`, ranked hits of a search of `plan` in `scope`, each with
    /// the [`Features`] of what found it, at most `limit`; apart from them
    /// those whose own postcode or street contradicts the plan.
    #[allow(clippy::too_many_arguments)]
    fn judged(
        &self,
        plan: &QueryPlan,
        scope: Scope,
        ranking: &Ranking,
        localities_stated: &[Option<StatedArea>],
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
        let (mut hits, mut contradicted) = (Vec::new(), Vec::new());
        for mut hit in candidates {
            let postcode = self.postcode_agreement(&hit, &ranking.postcode)?;
            let area = if localities_stated.is_empty() {
                Agreement::Unstated
            } else if localities_stated.iter().any(Option::is_none) {
                Agreement::Unknown
            } else {
                let known = localities_stated
                    .iter()
                    .flatten()
                    .cloned()
                    .collect::<Vec<_>>();
                agreement(self.area_agreement(&hit, &known, &ranking.postcode, session)?)
            };
            let label = label_words(&hit.record);
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
            features.number = plan.house_number.as_ref().map(|number| {
                states_number(&hit.record, &number.text, ranking.flat_text.as_deref())
            });
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

    /// The words of `words`, a query's street, that say its type or which
    /// way it runs, each in all its forms ("n" is "north"). A type glued
    /// onto a name is a type too ("plein" of "wilderinksplein"), as the
    /// street keys read it: "Wilderinksplein" and "Wilderinksstraat" key
    /// alike, and are two streets. A name of fewer than three letters is
    /// no name a type is glued onto: "16th" is an ordinal.
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

    /// Whether the street of `hit` is another street of the name the query
    /// writes (`stated`): it has every word of the name, and a type or a
    /// direction but none of the ones of that kind the query writes. "South
    /// US Highway 281" for "North US Highway 281", "16th Avenue North" for
    /// "16th St N". It ranks after the streets that agree, never out: a
    /// street type or direction never rejects a record (a query may write a
    /// street's type wrong). A street that says nothing of a kind the query
    /// states contradicts nothing, and nor does one of another name: the
    /// words of a POI's name may be a direction somewhere ("Tim" is east in
    /// Indonesian).
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

    /// Whether a hit lies in the areas `stated` names, `None` when nothing
    /// says either way: it names none, or the hit lies in no area of the
    /// level of any it names. A hit agrees with a stated name when one name
    /// of its areas of that level has every word of it; with a stated area
    /// when it agrees with each of its names, or with one where they cannot
    /// all hold one address (see [`StatedNames::any`]). A part of a query
    /// that is no one name may name areas of any level, so its names are
    /// looked for in the hit's regions too.
    ///
    /// A locality that no area of the hit names still agrees with it in two
    /// cases: it is the postal town of the query's postcode and the hit lies
    /// in that postcode (see [`Self::postal_town`]), or it is a settlement
    /// mapped only as a point among the few nearest the hit (see
    /// [`Self::near_settlements`]): Bootle is a town with no boundary of its
    /// own, and its streets lie only in Sefton's.
    fn area_agreement(
        &self,
        hit: &TextSearchHit,
        stated: &[StatedArea],
        postcode: &QueryPostcode,
        session: &Session,
    ) -> Result<Option<bool>> {
        if stated.is_empty() {
            return Ok(None);
        }
        let areas = self.areas_of(hit.record_id, session)?;
        let mut settlements = None;
        let mut agreement = None;
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
            let mut agrees = agrees_with(&names);
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
                return Ok(Some(false));
            }
            agreement = Some(true);
        }
        Ok(agreement)
    }

    /// The names, as search keys, of the [`NEAR_SETTLEMENTS`] settlements
    /// nearest a hit, within [`NEAR_SETTLEMENT_RADIUS_M`], that the Pack
    /// holds only as points: a town or suburb OSM maps as a node names the
    /// land nearer it than most others, with no boundary to say so. A node
    /// inside a boundary of its own name is the point of a town its
    /// boundary bounds, and names nothing past it (Dayton's node is no
    /// sign that Clayton is Dayton). None for a hit with no point.
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

    /// Whether a hit lies where `words`, area words a query states without
    /// requiring them, say: each word that is not a word of the hit itself
    /// (its name or label: "Wellington" of "Wellington Road" is the street)
    /// must be a word of an area it lies in, of any level, since the parser
    /// may have read a locality as a region. A hit in no named area says
    /// nothing either way.
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

    /// Every name a record lies in: what its own address states and every
    /// area of its chain, whatever tier the chain ranks it. A community
    /// inside a larger municipality (Seaforth in Huron East) or a suburb
    /// mapped only as a point (Grey Lynn in Auckland) has no boundary of its
    /// own, but the address can state it.
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

    /// The parts of `hit` a query word may name (see [`Parts`]).
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

    /// The regions of a record's admin chain.
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
        for entry in context.admin_context.entries() {
            let region = entry
                .tiers
                .iter()
                .any(|tier| matches!(tier, PlaceLayer::Region | PlaceLayer::SubRegion));
            let below_region = entry.tiers.iter().any(|tier| tier > PlaceLayer::SubRegion);
            if !(region || below_region) {
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
                if below_region {
                    areas.add_locality(name);
                }
            }
        }
        Ok(areas)
    }

    /// Whether a hit lies in the postcode the query names, `None` when
    /// nothing says either way. Its own postcode or one its street's
    /// addresses state holding it agrees. Else where it lies decides (see
    /// [`QueryPostcode::placement`]): a street's postcodes name the blocks
    /// its addresses state, and a long street crosses many, so the one asked
    /// for missing from them says nothing ("1161 Queen Street East" lies
    /// between the M4M 1L2 and M4M 1L5 its street states, not in M4M 1L3).
    /// A postcode the hit states itself that does not hold the one asked for
    /// contradicts it when where it lies cannot say.
    fn postcode_agreement(
        &self,
        hit: &TextSearchHit,
        postcode: &QueryPostcode,
    ) -> Result<Option<bool>> {
        if postcode.forms.is_empty() {
            return Ok(None);
        }
        let records = self.pack.records();
        let held = records
            .postcodes(hit.record_id)?
            .iter()
            .filter_map(|own| compact_postcode(own))
            .any(|own| postcode.holds(&own));
        if held {
            return Ok(Some(true));
        }
        if let Some(point) = hit.record.point {
            if !postcode.areas.is_empty() {
                if let Some(placement) = postcode.placement(point.lon, point.lat) {
                    return Ok(Some(placement));
                }
            } else if self.unlike_postcode_near(point.lon, point.lat, postcode)? {
                return Ok(Some(false));
            }
        }
        Ok(records.postcode(hit.record_id)?.map(|_| false))
    }

    /// Whether the postcode nearest a point, within
    /// [`POSTCODE_AREA_RADIUS_M`], shares not even its first character with
    /// the one asked for: for a postcode the Pack cannot place, the one sign
    /// a hit lies elsewhere.
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

    /// The postcode `plan` names, with the points of the Pack's records of
    /// it, so hits can be ranked and judged by how far they lie from it.
    ///
    /// A postcode written with an extension the Pack holds no record of is
    /// the code it starts with, when the Pack holds that: "45415-3457", a
    /// ZIP+4, is the 45415 its addresses state.
    fn query_postcode(&self, plan: &QueryPlan) -> Result<QueryPostcode> {
        let mut postcode = self.query_postcode_unplaced(plan)?;
        if !postcode.forms.is_empty() {
            postcode.areas = self.postcode_areas(&postcode.exact)?;
        }
        Ok(postcode)
    }

    /// [`Self::query_postcode`] before it is placed.
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
            postcode = QueryPostcode::of(&[first.to_string()]);
        }
        Ok(postcode)
    }

    /// Where the postcode written `exact` lies, in each country the Pack
    /// holds it in: its own records, or for a postcode the Pack has no
    /// record of, the records that share the longest start with it, at
    /// least [`POSTCODE_AREA_MIN_NEIGHBOURS`] of a country ("m4m 1l3" by
    /// "m4m 1l2", "m4m 1l5" and "m4m 1l7"), else the longest start any
    /// record shares. Each area reaches as far as its
    /// nearest other postcode, so as far as postcodes of that grain do:
    /// blocks in a Canadian city, tens of kilometres for a rural code.
    /// Countries stay apart, so "2110" is an area in Sydney and one in
    /// Papakura, never one between them.
    fn postcode_areas(&self, exact: &str) -> Result<Vec<PostcodeArea>> {
        let searcher = self.reader.searcher();
        let mut ends = exact
            .char_indices()
            .map(|(at, character)| at + character.len_utf8())
            .collect::<Vec<_>>();
        ends.reverse();
        // The longest start shared by too few records to place the postcode,
        // used when no start is shared by enough.
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

    /// An area of the records that place a postcode, for each country.
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

    /// How far the nearest postcode record that is none of `members` lies
    /// from them, at most [`POSTCODE_AREA_REACH_LIMIT_M`].
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

    /// Up to [`POSTCODE_AREA_TERMS`] terms of `field` that start with
    /// `prefix`.
    fn terms_with_prefix(
        &self,
        searcher: &Searcher,
        field: Field,
        prefix: &str,
    ) -> Result<Vec<String>> {
        // No term holds the byte 0xFF, which UTF-8 never uses, so every term
        // with the prefix sorts below it.
        let mut upper = prefix.as_bytes().to_vec();
        upper.push(0xFF);
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

    /// What agreeing on each term of `terms` as a postcode weighs, in each
    /// country the Pack holds a postcode of it in.
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

    /// The Pack's postcode records of any of `terms`, as the postcode field
    /// indexes them.
    fn postcode_records(&self, terms: &[String], limit: usize) -> Result<Vec<RecordId>> {
        let query = BooleanQuery::new(vec![
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
        ]);
        let searcher = self.reader.searcher();
        searcher
            .search(&query, &TopDocs::with_limit(limit))?
            .into_iter()
            .map(|(_, doc)| Ok(self.document_records(&searcher, doc)?.0))
            .collect()
    }

    /// One word must match the record itself, else "ballard" suggests every
    /// place in Ballard.
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
        // Street before a trailing house number is typed whole. In-order
        // lookup skips the lists, so a query it answers costs what it did.
        let lists = match completion {
            Completion::InOrder => None,
            _ if !typing => None,
            _ => self
                .common_completions(subject, &words[last])?
                .zip(self.common_completions(self.fields.context_text, &words[last])?),
        };
        let completes = lists.is_some();
        let (subject_completions, context_completions) = lists.unwrap_or_default();
        let word = |field, at: usize| {
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
            self.autocomplete_word_query(field, &words[at], typed)
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
                clauses.push((Occur::Must, autocomplete_subject_query(subject, words)));
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
            Completion::AllWords => {
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
        if let Some(bbox) = filters.bbox {
            clauses.push((Occur::Must, self.bbox_query(bbox)));
        }
        clauses.push(self.product_clause());
        Ok(Some(Box::new(BooleanQuery::new(clauses))))
    }

    /// Ties keep index order, so the same input picks the same words. `None`
    /// past [`MAX_COMPLETION_SCAN`]. "s" then stands only for "south".
    fn common_completions(
        &self,
        field: Field,
        prefix: &str,
    ) -> Result<Option<(Vec<String>, bool)>> {
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
                autocomplete_subject_query(field, std::slice::from_ref(&word.to_string())),
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
    fn places_by_name(
        &self,
        query_text: &str,
        filters: &Filters<'_>,
        focus: Option<[f64; 2]>,
        limit: usize,
    ) -> Result<Vec<TextSearchHit>> {
        let Some(key) = search_key(query_text) else {
            return Ok(Vec::new());
        };
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
        if let Some(bbox) = filters.bbox {
            clauses.push((Occur::Must, self.bbox_query(bbox)));
        }
        clauses.push(self.product_clause());
        let ranking = Ranking {
            query_words: QueryWords::new(query_text),
            focus,
            ..Ranking::default()
        };
        // Collected by fame, not text score: BM25 scores a name field down
        // for its length, and the best known places carry the most names
        // (Roma's own and its "Rome", "Rom", "Rzym" ...), so by text score
        // every small Rome would fill the window first.
        let window = limit.max(NAMED_PLACES_WINDOW);
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
        let mut hits = self.rank_documents(&searcher, top_docs, &ranking, window, &mut |hit| {
            Ok(filters.admit(hit))
        })?;
        let records = self.pack.records();
        let mut order = HashMap::new();
        for hit in &hits {
            // Exact name, own or alternate ("AMS", "Rome" of Roma), before a
            // name holding the words; then nearer the caller, so fame never
            // outranks the caller's own surroundings; then better known. Of
            // places about as well read, the one the query names by its own
            // name: "Lagos" is the city, not Laos, which one language spells
            // so and which is read a little more.
            let names = records.names(hit.record_id)?;
            let named = |name: &String| search_key(name).as_deref() == Some(key.as_str());
            let exact = names.iter().any(named);
            let own = names.first().is_some_and(named);
            let band = focus
                .zip(hit.record.point)
                .map_or(0, |([lon, lat], point)| {
                    focus_band(haversine_m(lon, lat, point.lon, point.lat))
                });
            let known = records.prominence(hit.record_id)?;
            let fame = known.unwrap_or_default();
            order.insert(
                hit.record_id,
                (
                    std::cmp::Reverse(exact),
                    band,
                    std::cmp::Reverse(fame.importance / FAME_TIER),
                    std::cmp::Reverse(own),
                    std::cmp::Reverse(fame),
                    known.is_some(),
                ),
            );
        }
        hits.sort_by_key(|hit| {
            let (exact, band, tier, own, fame, _) = order[&hit.record_id];
            (exact, band, tier, own, fame)
        });
        // Address product: only a place named the query takes the lead, as
        // only places carry fame; a street or an address of the name keeps
        // the place the address reading gives it.
        if !self.places {
            hits.retain(|hit| {
                let (exact, _, _, _, _, known) = order[&hit.record_id];
                exact.0 && known
            });
        }
        self.one_per_item(&mut hits, limit)?;
        Ok(hits)
    }

    /// The best `limit` hits that `accept` keeps, ranked by [`RankKey`] over
    /// every record the query matches, and equal keys by [`TieBreak`].
    /// Rejected before the cut, so the whole rank window can backfill a hit
    /// that fails.
    ///
    /// An interpolation hit is placed at the house number along its range, and
    /// left out when the number falls between its steps.
    fn ranked_hits(
        &self,
        query: &dyn Query,
        ranking: &Ranking,
        limit: usize,
        accept: &mut impl FnMut(&TextSearchHit) -> Result<bool>,
    ) -> Result<Vec<TextSearchHit>> {
        let searcher = self.reader.searcher();
        let window = limit
            .saturating_mul(RANK_WINDOW_FACTOR)
            .min(MAX_RANK_WINDOW)
            .max(limit);
        // Collected by text score alone, so the index skips the documents
        // that cannot make the window (block-max WAND), and only the window
        // is keyed: a house number's term, a range's and a postcode's are
        // scored in the query, so what the key ranks first scores first.
        // When the window ends inside a run of equal scores, which of them
        // made it is arbitrary, and the rest of the key must choose
        // ("Richmond" the town among a hundred Richmond Roads, the one near
        // the caller among many Main Streets): the run is collected whole,
        // up to [`TIE_WINDOW`].
        let mut scored = match ranking.near_first {
            true => Vec::new(),
            false => searcher.search(query, &TopDocs::with_limit(window))?,
        };
        let tie = |scored: &[(Score, DocAddress)], window: usize| {
            scored.len() == window && window > 1 && scored[window - 1].0 == scored[window - 2].0
        };
        if tie(&scored, window) && window < TIE_WINDOW {
            scored = searcher.search(query, &TopDocs::with_limit(TIE_WINDOW))?;
        }
        // A run past the tie window keeps an arbitrary part of it ("Paris"),
        // so every match is keyed: no skipping, paid by such runs alone.
        // Near first ranks a lower score above, so every match is keyed.
        let top_docs = if ranking.near_first || tie(&scored, TIE_WINDOW) {
            let keys = RankKeys {
                ranking,
                columns: &self.columns,
                pack: &self.pack,
            };
            searcher.search(query, &TopDocs::with_limit(window).tweak_score(keys))?
        } else {
            let mut top_docs = self.keyed_documents(&searcher, scored, ranking)?;
            top_docs.truncate(window);
            top_docs
        };
        self.rank_documents(&searcher, top_docs, ranking, limit, accept)
    }

    /// Each scored document with its [`RankKey`], best first.
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

    /// The best `limit` hits of `top_docs`, keyed documents best first, that
    /// `accept` keeps, ranked by [`RankKey`] and equal keys by [`TieBreak`].
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
        // Without a house number no document is left out for its number, so
        // once `limit` hits are kept only the documents that tie the last of
        // them need hydrating. The cutoff is the last kept hit's, not the
        // `limit`th document's: a rejected hit leaves its place to the next.
        // A POI may yet rank below an address or street after it (see
        // [`Self::demoted`]), so it holds no place while one may.
        let mut cutoff = None;
        let mut hits = Vec::new();
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
            for record_id in self.hit_records(first, count, ranking)? {
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
            }
            if !ranking.numbered && cutoff.is_none() && placed >= limit {
                cutoff = Some(key);
            }
        }
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

    /// Whether each of `hits` is a POI ranked below every address, range and
    /// street hit: addresses beat POIs when the query carries address
    /// evidence. A hit carries it when the query asks for a house number,
    /// or when the street it is on (or is) accounts for every word the
    /// query seeks: the query names a street the Pack has. A query some of
    /// whose words a street lacks may name a POI ("Queen's Quay Family
    /// Medicine"), and its POIs rank as before.
    ///
    /// A POI that confirms such a hit is not demoted: the words of the query
    /// the street lacks are all of its name, and it stands at the hit ("227
    /// Walmer Road Parkette" is the park at 227 Walmer Road). One named only
    /// by the street's own words confirms nothing ("Walmer Road Parkette"
    /// for "Walmer Road").
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

    /// The words the query seeks that the street of `hit` lacks: its own
    /// name for a street, else the street it states.
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

    /// Whether the POI `poi` stands at the address, range or street
    /// `anchor`: it states the same street and number, or lies on that
    /// street, or within [`AT_ADDRESS_RADIUS_M`] of the address.
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

    /// The country the records of a document lie in.
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

    /// The first record a document stands for, and how many.
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

    /// The records a matched document answers: of a street group, the
    /// addresses with the house number the query asks for, or the first of
    /// them when it asks for none; else the document's own record.
    fn hit_records(&self, first: RecordId, count: u64, ranking: &Ranking) -> Result<Vec<RecordId>> {
        let Some(number) = &ranking.number_text else {
            return Ok(vec![first]);
        };
        if self.pack.records().header(first)?.layer != Layer::Address {
            return Ok(vec![first]);
        }
        let mut matched = Vec::new();
        self.pack
            .records()
            .address_numbers(first, count, |record_id, stated, unit| {
                let sought = |term: &String| {
                    term == number || ranking.flat_text.as_ref().is_some_and(|flat| term == flat)
                };
                if address_number_terms(stated).any(|term| sought(&term))
                    || flat_number_term(stated, unit).is_some_and(|term| sought(&term))
                {
                    matched.push(record_id);
                }
                Ok(())
            })?;
        Ok(matched)
    }

    /// Move a street standing in for a house number onto its own centre line.
    ///
    /// A street's representative point is the centroid of its geometry, which
    /// is the right label anchor but need not sit on the street at all: the
    /// centroid of an L-shaped or crescent road falls in the space the bend
    /// encloses, and on a long road it can be a kilometre from the pavement.
    /// A point halfway along the line is on the street by construction, so it
    /// is never further from the house asked for than the centroid is, and
    /// usually much closer.
    ///
    /// This asks nothing of the address data and so holds for every country:
    /// it uses only the street's own geometry. Where a number can be placed
    /// properly, by a range that holds it or between stated neighbours, that
    /// runs first and this never applies.
    fn place_on_street_line(&self, record_id: RecordId, record: &mut Summary) -> Result<()> {
        let Some(point) = record.point else {
            return Ok(());
        };
        // Half way along the line, by distance rather than by vertex count, so
        // a densely drawn bend does not pull the point into it.
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

    /// Place `number` on the interpolation range `record` summarizes: the
    /// point that far along its line, labelled as that address. `false` when
    /// the range does not contain the number.
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

    /// A house number no record states or range holds, placed between the
    /// nearest stated numbers below and above it on its street, in the
    /// localities `pass` requires. Never past the last number known on either
    /// side, so a street with numbers on one side of it only stays a street.
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
        areas: &AreaWords,
    ) -> Result<Option<TextSearchHit>> {
        let Some(number) = plan.house_number.as_ref().map(|number| number.base) else {
            return Ok(None);
        };
        let (lower, higher) = self.neighbours(plan, pass, ranking, areas, number)?;
        self.place_between(number, &lower, &higher, NEIGHBOUR_STREET_RADIUS_M)
    }

    /// `number` placed between the nearest of its neighbours `lower` and
    /// `higher` (see [`Self::place_between_neighbours`]), each of them and
    /// the point within `setback_m` of their road: [`ROAD_SETBACK_M`] for
    /// the documents of a road inside the named places, the nearer
    /// [`NEIGHBOUR_STREET_RADIUS_M`] for those a text search finds anywhere.
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

    /// The stated house numbers nearest `number` on the street and in the
    /// localities `plan` and `pass` require: those at or below it, then those
    /// above it, each nearest first.
    fn neighbours(
        &self,
        plan: &QueryPlan,
        pass: Pass,
        ranking: &Ranking,
        areas: &AreaWords,
        number: u32,
    ) -> Result<(Vec<Neighbour>, Vec<Neighbour>)> {
        let searcher = self.reader.searcher();
        let mut clauses = self.subject_clauses(plan, pass, areas);
        clauses.push((Occur::Must, self.layer_query(Layer::Address.as_str())));
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

    /// The stated house numbers of `docs` nearest `number` (see
    /// [`Self::neighbours`]).
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

    /// The address or POI at `record_id` as a neighbour, `None` when it names
    /// no street.
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
        self.columns
            .get(&searcher.segment_reader(segment_ord).segment_id())
            .ok_or_else(|| Error::Inconsistent("text index segment has no fast fields".into()))
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
                && street_search_text(&self.pack.records().summary(candidate.record_id)?.name)
                    .as_deref()
                    == Some(street)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether a street record of the road `street` names (see
    /// [`road::same_road`]) passes within `within_m` of `point`: the
    /// sources of a Pack spell one road differently.
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
        })
    }
}

/// A stated house number beside one no record states.
/// The names a record lies in, normalized for matching: its regions, and the
/// areas below them that a locality may name.
#[derive(Debug, Default)]
struct RecordAreas {
    regions: Vec<String>,
    localities: Vec<String>,
    /// The areas of its chain these names come from.
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

/// The words of each part of a record a query word may name, as search
/// keys, for joint decoding (see [`TextSearcher::explain`]). A part the
/// record lacks is empty, and says nothing of any word.
#[derive(Debug, Default)]
struct Parts {
    house_number: HashSet<String>,
    unit: HashSet<String>,
    street: HashSet<String>,
    locality: HashSet<String>,
    region: HashSet<String>,
    /// The regions of the record's admin chain, which a region word the
    /// Pack lists by its key names ("on" names Ontario).
    regions: Rc<Vec<RecordId>>,
    postcode: HashSet<String>,
}

impl Parts {
    /// The labels the record lets `word` take: those of the parts it
    /// names, those of the parts the record lacks, and always
    /// [`Label::Other`]. A country is never checked: a query names one in
    /// more ways than a record states it.
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
/// estimated on a range; how well its text matches; then, of records the
/// text cannot tell apart, the one whose name has the fewest words, the
/// larger place, and the one nearer the caller. Every hit already lies in
/// the localities its pass requires. Larger is better.
#[derive(Debug, Clone, PartialEq)]
struct RankKey {
    house_level: bool,
    /// See [`QueryPostcode::rank`].
    postcode_agreement: u8,
    stated: bool,
    /// A place whose whole name the query types, under
    /// [`Ranking::near_first`]: "tacoma" from Seattle is the city, not
    /// Tacoma Screw down the road.
    named_place: bool,
    /// [`Self::nearness`] within [`BIAS_BANDS`], 0 beyond or unless
    /// [`Ranking::near_first`].
    near: u8,
    text_score: Score,
    /// Words of the record's own name: of names a query matches alike, the
    /// one it covers with fewest left over ("Richmond" before "Richmond
    /// Road", "Queen Street" before "Upper Queen Street"). [`TieBreak`]
    /// asks the same of the hits.
    name_words: u8,
    /// See [`Prominence::rank`], larger better known; 0 for an address, a
    /// street or a POI. Before the size of a place: Salzburg the city is better
    /// known than the state, Paris in France than Paris in Benin, though
    /// both are cities.
    prominence: u64,
    /// See [`place_rank`], smaller larger; [`u8::MAX`] for a record that is
    /// no place.
    place_rank: u8,
    /// How near the caller's focus the document's extent lies, larger
    /// nearer. It orders only the window the text index collects, so that
    /// of many documents the query cannot tell apart the near ones make it;
    /// the hits are ranked by [`Self::cmp_evidence`] and then by
    /// [`TieBreak`], whose own focus band follows what the query states.
    nearness: u8,
    /// A POI's [`Prominence::rank`], after nearness as in [`TieBreak::poi`]:
    /// else the window holds the best known of a chain's thousand equal
    /// names, and the ones near the caller never make it.
    poi_prominence: u64,
}

impl RankKey {
    /// The key of a hit when the query asks for no house number or postcode.
    fn text(text_score: Score) -> Self {
        Self {
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

    /// The order of what the query states, leaving out what only breaks
    /// ties.
    fn cmp_evidence(&self, other: &Self) -> std::cmp::Ordering {
        (
            self.house_level,
            self.postcode_agreement,
            self.stated,
            self.named_place,
            self.near,
            self.text_score,
        )
            .partial_cmp(&(
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

/// How a caller's focus ranks a hit: hits in one band tie on it. The bands
/// widen as they go, as nearness does: a town 30 km away is about as near as
/// one 40 km away, while 1 km and 10 km are not. Within the first a caller
/// is among the hits, and what follows the band (the size of a place, the
/// precision of a point) decides. Beyond the last, focus says nothing.
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

/// Bands of [`FOCUS_BANDS_M`] up to 50 km, as far as a map view's bias
/// reaches. Beyond, a better match wins: "paris" from Seattle is France.
const BIAS_BANDS: usize = 6;

/// [`Prominence::rank`] of a place known beyond its own language: read on
/// Wikipedia, or linked to Wikidata and named in another language. New York
/// is; Pike County, Arkansas, and Albert, Somme, are not.
fn known_abroad(prominence: u64) -> bool {
    prominence >> 8 > 0 || (prominence & 0x80 != 0 && prominence & 0x7f > 0)
}

/// The first of [`FOCUS_BANDS_M`] a point `distance_m` from the focus lies
/// within, smaller nearer; one past the last beyond it.
fn focus_band(distance_m: f64) -> u8 {
    FOCUS_BANDS_M
        .iter()
        .position(|band| distance_m <= *band)
        .unwrap_or(FOCUS_BANDS_M.len()) as u8
}

/// The caller's focus, against the extents of a segment's documents.
struct SegmentFocus {
    focus: [f64; 2],
    min_lon: Column<i64>,
    min_lat: Column<i64>,
    max_lon: Column<i64>,
    max_lat: Column<i64>,
}

impl SegmentFocus {
    /// [`RankKey::nearness`] of a document: the band of the point of its
    /// extent nearest the focus, counted down from the nearest.
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

/// Computes each matched record's [`RankKey`] as the text index collects
/// them, so the window holds the best ranked records of all that match, not
/// the best scoring.
struct RankKeys<'a> {
    ranking: &'a Ranking,
    columns: &'a HashMap<SegmentId, SegmentColumns>,
    pack: &'a Arc<pack::Reader>,
}

struct SegmentRankKeys {
    numbered: bool,
    near_first: bool,
    typed_words: u8,
    house_number: Column<u64>,
    interpolation_start: Column<u64>,
    name_words: Column<u64>,
    prominence: Column<u64>,
    place_rank: Column<u64>,
    postcode: Option<SegmentPostcodes>,
    focus: Option<SegmentFocus>,
}

/// What a segment knows of each record's postcodes, read against the
/// postcode a query names.
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

/// A postcode column with, for the postcode a query names, the ordinals of
/// the postcodes that share each of its prefixes and of the ones it holds.
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
        // A postcode shorter than the form is held when the form starts with
        // it: "m5v" holds "m5v1a1".
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

    /// Whether any of `ords` is held, and the longest prefix of the named
    /// postcode any of them shares; `None` for no ords.
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

/// Ranks a matched record with no postcode of its own by its distance from
/// the records of the postcode a query names. The first address of a street
/// group stands for all of it: they share a street, their areas and the
/// postcode nearest them.
struct PostcodeNearness {
    record_id: Column<u64>,
    pack: Arc<pack::Reader>,
    postcode: QueryPostcode,
}

impl PostcodeNearness {
    /// The first of [`POSTCODE_NEARNESS_BANDS_M`] the record lies within.
    /// Beyond the last (a code shared with another country) only the
    /// nearest postcode centroid ranks, as without the postcode's records.
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

/// Collects the documents a name query matches nearest the caller's focus
/// first, then best known, whatever their text score (see
/// [`TextSearcher::places_by_name`]).
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
            typed_words: self.ranking.typed_words,
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
        // A numbered query matches a record by its stated number, by a range
        // that holds the number, or as a street that stands in for it.
        let mut key = RankKey::text(text_score);
        let small =
            |value: Option<u64>| value.map_or(u8::MAX, |value| value.min(u64::from(u8::MAX)) as u8);
        key.name_words = small(self.name_words.first(doc));
        let prominence = self.prominence.first(doc).unwrap_or_default();
        match self.place_rank.first(doc) {
            Some(rank) => {
                key.prominence = prominence;
                key.place_rank = small(Some(rank));
            }
            None => key.poi_prominence = prominence,
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
            key.named_place = key.place_rank != u8::MAX
                && key.name_words <= self.typed_words
                && (key.near > 0 || known_abroad(key.prominence));
        }
        key
    }
}

/// Secondary order for hits with equal ranks.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TieBreak {
    /// Of a query for a house number, words of the street a record is on
    /// that no query word names: "45 Lomond Crescent" is a flat of Lomond
    /// Crescent before the house of a Ben Lomond Crescent. Before the unit,
    /// which only chooses among the addresses of one street. A word an
    /// abbreviation of the query stands for is named ("crescent" of
    /// "cres"), and so is one its typo tier lets a query word match. Without a house number the street is no part of what is
    /// sought: a POI is no worse for the street it is on.
    extra_street_words: usize,
    /// The flat the query names, then the building, then its other flats:
    /// "19 Eros Place" is the house, not its flat 1, though a richer house
    /// label would lose on the words below. The unit is the one the plan
    /// read, never a word of the house number ("7 Vialou Street" is no
    /// unit 7).
    unit: u8,
    /// Words of the record's own name the query did not ask for: "342
    /// Albert Street" beats "342 Prince Albert Street" for the query "342
    /// albert street". The name, not the label, so where a record lies
    /// never counts against it.
    extra_name_words: usize,
    /// Query words the label lacks. The addresses of a street group share
    /// one document and so one score, though only some state the unit the
    /// query names.
    missing_query_words: usize,
    /// Query words the label does not spell as the query does: a query that
    /// writes "Taupō" asks for Taupō before a "Taupo" its search key also
    /// finds, and one that writes "Taupo" for Taupo first.
    inexact_query_words: usize,
    /// Better known places first (see [`Prominence`]), before their size:
    /// "Paris" is the one in France before the one in Benin, both cities.
    /// Every record but a place ties on it.
    fame: std::cmp::Reverse<Prominence>,
    /// Larger places first (see [`place_rank`]), then every other record:
    /// "Richmond" is the town before a locality of that name. Only a tie on
    /// the words and the areas a query states reaches it, so a size prior
    /// never outweighs what the query says. The text index keeps the
    /// window by it too (see [`RankKey::place_rank`]).
    place_rank: u8,
    /// Nearer the caller's focus first, by [`FOCUS_BANDS_M`]; all tie
    /// without one. After every word of the query, so a caller's location
    /// never outranks a better match, and every hit already lies in the
    /// areas the query states.
    focus_band: u8,
    /// An address no other source disagrees on first: where two sources
    /// placed one house far apart, its point may be wrong (see
    /// [`AddressRecord::disputed`]).
    disputed: bool,
    /// Every other record before a POI, then the POI its OSM tags show
    /// better known first: read more, linked to Wikidata or Wikipedia, then
    /// named in more languages (see [`Prominence`]). Only POIs that tie on
    /// all the query states and on the caller's focus reach it. It is no
    /// measure of an address, so `None` keeps it from ever ordering one
    /// against a POI by prominence; at so full a tie an address before a
    /// POI is what [`TextSearcher::demoted`] says anyway.
    poi: Option<std::cmp::Reverse<Prominence>>,
    /// Exact points before centroids before estimates before line midpoints.
    precision: u8,
    /// Nodes before ways before relations, then the lower id.
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

/// What the Pack knows of a record apart from the query, for [`TieBreak`].
#[derive(Debug, Clone, Copy)]
struct Prior {
    /// See [`place_rank`].
    place_rank: Option<u8>,
    /// A POI's or a place's, `None` for the other layers.
    prominence: Option<Prominence>,
    disputed: bool,
}

/// How a record answers the unit the query names: 0 the flat it names, 1
/// the building, 2 another flat (see [`TieBreak::unit`]).
fn unit_rank(record: &Summary, ranking: &Ranking) -> u8 {
    // A flat kept as written ("2/10") is the one "2/10" names, though it
    // states no unit.
    let as_written = ranking.unit.is_some()
        && record
            .address
            .housenumber
            .as_deref()
            .and_then(search_key)
            .is_some_and(|written| {
                [&ranking.number_text, &ranking.flat_text]
                    .into_iter()
                    .flatten()
                    .any(|text| *text == written)
            });
    let stated = record.address.unit.as_deref().and_then(search_key);
    match (ranking.unit.as_deref(), stated.as_deref()) {
        _ if as_written => 0,
        (Some(sought), Some(stated)) if stated.split_whitespace().any(|word| word == sought) => 0,
        (Some(_), None) | (None, None) => 1,
        (_, Some(_)) => 2,
    }
}

/// Whether a record states `number` as its house number, or `flat`, the
/// flat the query names written with it.
fn states_number(record: &Summary, number: &str, flat: Option<&str>) -> bool {
    record
        .address
        .housenumber
        .as_deref()
        .and_then(search_key)
        .is_some_and(|stated| stated == number || Some(stated.as_str()) == flat)
}

/// The words of a record's label, as search keys.
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

/// An [`Agreement`] of a check that says yes, no, or nothing.
fn agreement(agrees: Option<bool>) -> Agreement {
    match agrees {
        Some(true) => Agreement::Agrees,
        Some(false) => Agreement::Contradicts,
        None => Agreement::Unknown,
    }
}

/// Which parses of a query [`TextSearcher::search_variants`] tries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parses {
    /// Every parse, best first.
    All,
    /// Every parse, unless the best reads a house number: then only those
    /// that read one.
    Numbered,
}

#[derive(Clone, Copy)]
enum Typed<'a> {
    Whole,
    /// Too many index words start with it to list them ("s"). First ones in
    /// index order, as before the lists.
    Prefix,
    Completions(&'a [String]),
}

/// Tried in order. Later ones only when earlier find nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Completion {
    InOrder,
    /// Index order stops at [`AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS`] words
    /// before common ones on a planet Pack. "mar" never reaches "market".
    CommonCompletions,
    AllWords,
}

impl Completion {
    const ALL: [Self; 3] = [Self::InOrder, Self::CommonCompletions, Self::AllWords];
}

/// The retrieval passes of [`TextSearcher::search_variants`], in order,
/// strictest first. The order is also how strongly a hit found by each
/// answers the query.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pass {
    /// The house number, stated or on a range, in every named locality.
    #[default]
    Strict,
    /// The number between its stated neighbours, or a street standing in for
    /// it, still in every locality.
    Street,
    /// The localities only rank, and a street may stand in for the number.
    /// A hit in other localities than the ones named is still left out.
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

/// How one search of a plan treats its localities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Every locality is required, and a street may stand in for the
    /// house number: [`Pass::Strict`] and [`Pass::Street`] hits.
    Scoped,
    /// The localities only rank: [`Pass::Lenient`] hits.
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

    /// The strictest pass its hits may rest on.
    fn best_pass(self) -> Pass {
        match self {
            Self::Scoped => Pass::Strict,
            Self::Open => Pass::Lenient,
        }
    }

    /// Whether it searches `plan` differently from the scopes before it: a
    /// query that names no locality has none to relax.
    fn searches(self, plan: &QueryPlan) -> bool {
        match self {
            Self::Scoped => true,
            Self::Open => plan.context.iter().any(|part| !part.is_postcode()),
        }
    }
}

/// How strongly a hit answers the query: the typos its words were matched
/// through, then the pass that found it, then whether anything confirms the
/// postcode its reading states, then the parser's score of the reading.
/// Compared in that order, never summed: the score is an unnormalised
/// Viterbi path score on no scale shared with retrieval, so any weight
/// joining the two would be made up. Fewer typos outweigh any pass, as a
/// query spelt as the Pack spells it means what it spells; a stricter pass
/// outweighs any score; the score decides only between hits found alike.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Evidence {
    edits: u8,
    pass: Pass,
    /// A hit agrees with the postcode the reading states, or it states none.
    /// A number read as a postcode that nothing says the hits are in may be
    /// their house number: "3010 Coast Road, Barrytown" is the house 3010,
    /// not any house in a postcode 3010 no Barrytown record states.
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

    /// Whether a later reading, which scores no higher, may still outweigh
    /// it: matched with fewer typos, or in a stricter pass, or in the same
    /// pass with its postcode confirmed. A confirmed hit of a pass that
    /// requires the localities, matched as written, ends the search.
    fn open(self) -> bool {
        self.edits > 0 || self.pass == Pass::Lenient || !self.confirmed
    }

    /// Whether a search whose hits rest on `pass` at best may outweigh it.
    fn beaten_by(self, pass: Pass) -> bool {
        self.edits > 0 || pass < self.pass || (pass == self.pass && !self.confirmed)
    }
}

/// A reading of the query searched as one plan.
#[derive(Debug, Clone)]
struct Reading {
    text: String,
    plan: QueryPlan,
    /// The parser's score of the labelling it plans.
    score: f32,
    /// The query's words with the labels of that labelling.
    pieces: Vec<Tagged>,
}

/// The plans of a query's readings, the parser's score and labelled words
/// of each, and the explanation of the best.
type Readings = (
    Vec<QueryPlan>,
    Vec<f32>,
    Vec<Vec<Tagged>>,
    Option<Explanation>,
    usize,
);

/// The items of `items` whose index `keep` keeps.
fn filter_by<T>(items: Vec<T>, keep: impl Fn(usize) -> bool) -> Vec<T> {
    items
        .into_iter()
        .enumerate()
        .filter(|(index, _)| keep(*index))
        .map(|(_, item)| item)
        .collect()
}

/// Searches one query runs at most, whatever its budget: the readings, each
/// in and out of its localities and with the base of its house number, and
/// then the same with their streets trimmed.
const MAX_SEARCHES: usize = 16;

thread_local! {
    /// What the best reading of the query being searched names, for
    /// [`choose::Named::choose`].
    static NAMED: RefCell<Option<choose::Named>> = const { RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// What the structured stage made of the last query it weighed, for tests.
    static LAST_STRUCTURED: Cell<StructuredTrace> = const {
        Cell::new(StructuredTrace {
            answered: false,
            support: 0,
            gap: f32::NAN,
        })
    };
}

/// Whether the structured stage answered the last query it weighed, and its best
/// candidate's place support and gap to the parser's best reading.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
struct StructuredTrace {
    answered: bool,
    support: usize,
    gap: f32,
}

/// Documents of the structured index's matching keys a structured-stage search ranks at
/// most. A street that common needs the text search's ranking to choose.
const STRUCTURED_CANDIDATES: usize = 4_000;

/// Of `docs`, ids across the index and sorted, those from `base` to `end`,
/// as ids in their segment.
fn segment_docs(docs: &[DocId], base: DocId, end: DocId) -> Vec<DocId> {
    let from = docs.partition_point(|doc| *doc < base);
    let to = docs.partition_point(|doc| *doc < end);
    docs[from..to].iter().map(|doc| doc - base).collect()
}

/// `query` as the parser should read it. A run of two or more words written
/// twice in a row is read once ("191 Beaufort Street 191 Beaufort Street").
/// Words glued across letters and digits ("Choranche2865") or a lower and
/// an upper case letter ("EmbarcadèreRue") are split where each side is a
/// word of its own: three letters or more before a digit, four or more
/// after one, four or more lower case before an upper case letter, so a
/// postcode ("T5H"), an ordinal ("18th"), a suffix ("116bis") or a name
/// such as "DeKalb" stays whole. A replacement character is dropped, and
/// typo tolerance reads the word it broke as one letter short.
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
                // Hangul and the scripts written without spaces glue numbers
                // into names ("달구벌대로530길"): only a word of a spaced
                // alphabet is split from a digit.
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

/// A word the parser thinks names a place at least this likely is read as
/// one: below it, a footprint lookup costs more than the word can add.
const PLACE_FLOOR: f32 = 0.1;
/// Areas of one reading answered when no street is found, at most.
const AREA_ANSWERS: usize = 8;
/// Readings of a query the structured stage tries, best first: more than the text
/// search's, as each costs it only structured index lookups.
const STRUCTURED_READINGS: usize = 5;
/// Spots a reading's candidates are explained in at most, the best by
/// support and number form: each costs a Pack record and a decode.
const STRUCTURED_SPOTS: usize = 8;
/// Hits a search gathers to choose its answers among, whatever it returns.
const CHOOSER_HITS: usize = 10;
/// Address points of a spot kept for its street answer.
const SPOT_POINTS: usize = 256;
/// A street answer's point is the centre of the disk of this radius that
/// holds the most of the street's address points: an answer within 100 m
/// is the goal, and that disk's centre is likeliest to be.
const STREET_DISK_M: f64 = 100.0;
/// Weight of each place word holding a candidate in its score, on the
/// parser's scale. Chosen with [`STRUCTURED_DELTA`] and
/// [`STRUCTURED_MIN_FORM`] so the structured stage answers as often as it
/// can while its answers are as often within 100 m and 600 m as the text
/// search's, and seldom in a town the query does not name.
const STRUCTURED_W: f32 = 8.00;
/// The coarsest answer type the structured stage answers with, chosen with
/// [`STRUCTURED_W`]: coarser ones (a street, a postcode, a town or a region
/// alone) are left to the text search.
const STRUCTURED_MIN_FORM: u8 = 4;
/// The least lead of [`P_STRUCTURED`] over [`P_TEXT`] the structured stage answers
/// with. Chosen with [`STRUCTURED_W`].
const STRUCTURED_DELTA: f32 = -0.02;
/// Inputs of the hand-off models, [`structured_features`].
const FEATURES: usize = 19;
/// A margin larger than this says no more.
const MARGIN_CAP: f32 = 30.0;
/// Logistic model, intercept first, of whether the structured answer is within
/// 100 m of the truth; fit with [`STRUCTURED_W`].
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
/// The same of the text search's answer.
const P_TEXT: [f32; FEATURES + 1] = [
    -0.171209, 0.889543, 0.330719, -0.515181, 0.00348658, -0.398113, -0.715066, -1.04156, -1.86512,
    -1.13006, -0.412767, -1.01375, -0.260571, 0.0460241, 0.0421718, -0.324983, -0.0255055,
    -0.0191589, -0.0539111, 0.0151013,
];
/// A place kept only as its extent is read through for support only when
/// it holds at most this many documents of the street.
const EXTENT_READ: usize = 4_000;
/// Cells a street is looked for in by its words at most.
const MATCH_CELLS: usize = 1_024;
/// Streets found by their words that are read at most.
const MATCHED_STREETS: usize = 16;
/// Survivors farther than this from the best are another spot: the structured
/// tier cannot tell which the query means.
const SPOT_M: f64 = 200.0;
/// A place name shorter than this is not read one edit off: most short
/// names are that near several others.
const FUZZY_PLACE_CHARS: usize = 4;
/// A street record adds the cells of its extent to the streets of each,
/// up to this many; a longer one only its middle.
const STREET_CELLS: usize = 64;

/// What one query's structured stage reads.
struct StructuredContext<'a, 'b> {
    structured: &'a StructuredIndex,
    readings: &'a [Reading],
    /// The readings the parser gave, before the trimmed ones.
    parsed: usize,
    search: &'a Search<'b>,
    session: &'a Session,
    /// The query's lattice, to score each candidate against the whole query.
    explanation: Option<&'a Explanation>,
    /// Per query word, the parser's probability that it names a place.
    probabilities: Vec<f32>,
}

/// The answers the structured stage left to the text search, given when the
/// text search finds nothing (see [`TextSearcher::structured_stage`]).
struct Declined {
    /// The text of the reading that found them.
    text: String,
    plan: QueryPlan,
    hits: Vec<TextSearchHit>,
}

/// One spot's best record of a reading, hydrated (see
/// [`TextSearcher::structured_reading`]).
struct StructuredFound {
    hit: TextSearchHit,
    point: [f64; 2],
    /// How many of the reading's place words hold it.
    support: usize,
    form: u8,
    reading: usize,
    /// street or place: what found the street.
    entry: &'static str,
    /// The changes that found it, `LEVER_*` bits.
    lever: u8,
}

/// A structured index document ranked for its spot before any Pack record is read.
struct Keyed {
    /// Support, number form, whether it is the street's own record, and
    /// postcode agreement: larger is better.
    order: (usize, u8, bool, u8),
    doc: DocId,
    base: bool,
    address: bool,
    point: [f64; 2],
}

/// The candidates of one reading within [`SPOT_M`] of the best of them.
struct Spot {
    head: Keyed,
    docs: Vec<DocId>,
    /// Address points, at most [`SPOT_POINTS`].
    points: Vec<[f64; 2]>,
}

/// A candidate with its gap and score (see
/// [`TextSearcher::structured_explained`]).
struct Scored {
    gap: f32,
    score: f32,
    found: StructuredFound,
}

/// The candidates of a structured stage, explained (see
/// [`TextSearcher::structured_explained`]).
struct StructuredExplained {
    /// Best score first.
    scored: Vec<Scored>,
    /// The best score in a spot other than the best's, `-inf` when none.
    second: f32,
    spots: usize,
}

/// How the structured index tells a record's layer apart: an address, an
/// interpolation, a street or a POI, else 0.
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

/// The middle of `[west, south, east, north]`.
fn extent_middle([west, south, east, north]: [i32; 4]) -> [i32; 2] {
    [
        ((i64::from(west) + i64::from(east)) / 2) as i32,
        ((i64::from(south) + i64::from(north)) / 2) as i32,
    ]
}

/// Answer types, better first by value.
const FORM_EXACT: u8 = 8;
const FORM_RANGE_START: u8 = 7;
const FORM_RANGE: u8 = 6;
const FORM_INTERPOLATION: u8 = 5;
const FORM_NEIGHBOUR: u8 = 4;
const FORM_STREET: u8 = 3;
const FORM_POSTCODE: u8 = 2;
/// A locality, district, neighbourhood or place.
const FORM_TOWN: u8 = 1;
const FORM_REGION: u8 = 0;

/// What found a structured answer, which the hand-off models weigh: a street by
/// its words (A), a number the plan once dropped (C), a street the parser's
/// dictionary lacks (D), a street answer (F), an answer placed on its side
/// or the street's for a number it lacks (G).
const LEVER_A: u8 = 1;
const LEVER_C: u8 = 4;
const LEVER_D: u8 = 8;
const LEVER_F: u8 = 16;
const LEVER_G: u8 = 32;
/// The finest area the query names, when no street answers.
const LEVER_T: u8 = 64;

/// Inputs of the two hand-off models (see [`P_STRUCTURED`]), in order: one per
/// answer type exact, range-start, range, interpolation, neighbour,
/// street, postcode, town and region; levers A, C and D; the gap; the margin, at most
/// [`MARGIN_CAP`]; whether one spot alone was found; support, spots and
/// reading index; the log of one more than the documents read.
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

/// The probability a logistic model of `weights` (intercept first) gives
/// `features`.
fn logistic(weights: &[f32; FEATURES + 1], features: &[f32; FEATURES]) -> f32 {
    let z = weights[0]
        + weights[1..]
            .iter()
            .zip(features)
            .map(|(weight, feature)| weight * feature)
            .sum::<f32>();
    1.0 / (1.0 + (-z).exp())
}

/// `record`, a street's own, as the street's answer at `point`.
fn street_at(record: &mut Summary, [lon, lat]: [f64; 2]) {
    record.point = Some(Point {
        lon,
        lat,
        precision: PointPrecision::Estimated,
    });
}

/// Of the neighbours `lower` and `higher`, those inside the places `plan`
/// names by their own fields (see [`choose::Named::holds`]): a road of the
/// name in another town numbers its houses alike.
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

/// A postcode's letters and digits alone, upper case, to compare two
/// spellings of it.
fn postcode_letters(postcode: &str) -> String {
    postcode
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_uppercase())
        .collect()
}

/// The centre of the densest disk of [`STREET_DISK_M`] among `points`: the
/// mean of the points within it of the point with the most such; `None`
/// for none.
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

/// How many query letters a street's key `name` accounts for when its words
/// name every word of `query` (see [`TextSearcher::street_matches`]), `None`
/// when they do not. Words that only describe a street are left out.
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

/// How many letters of a street's key `name` the reading's street words
/// `query` name when every word of the name of four or more letters is
/// one of them, as written, a typo away or abbreviated: a reading may take
/// the town or a repeat of the address into its street ("voltaire berlijn
/// voltaire berlijn"), where every query word cannot be the street's. A
/// word of up to three letters may be missing ("di san cristoforo" of "di
/// s. cristoforo"). The name must take the first of the query's words, and
/// a name only of words the reading names as places (`places`), a street
/// called after its town, is not named this way.
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
    // The street starts the reading's street words; a name matching only
    // words after it is likelier a place the reading took in.
    let first = (0..query.len()).find(|&at| !describes_street(query[at]));
    (own && letters >= 4 && first.is_some_and(|at| used[at])).then_some(letters)
}

/// The words of the places a reading names apart from its street.
fn context_words(plan: &QueryPlan) -> Vec<String> {
    plan.context
        .iter()
        .flat_map(|part| part.words.iter().cloned())
        .collect()
}

fn types_word(typed: &str, word: &str, last: bool) -> bool {
    typed == word
        || (last && word.starts_with(typed))
        || (contraction(typed) && abbreviates(typed, word))
        || lattice::forms_both_ways(typed)
            .iter()
            .any(|form| form == word)
}

/// Contractions drop vowels. "cit" keeps one, so it is "city" cut short,
/// not "chipotle".
fn contraction(typed: &str) -> bool {
    (2..=4).contains(&typed.chars().count())
        && !typed
            .chars()
            .skip(1)
            .any(|character| character.is_ascii_digit() || "aeiou".contains(character))
}

/// Not required of a record. Almost any long word holds "mkt" in order, so
/// the index cannot tell which one it shortens.
fn unlisted_contraction(typed: &str) -> bool {
    contraction(typed) && lattice::forms_both_ways(typed).is_empty()
}

/// Whether `short` is written with letters of `long` in order from its
/// first, as an abbreviation is ("pg" of "passeig", "brg" of "brigadeiro").
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

/// The places the words of a reading name (see
/// [`TextSearcher::structured_places`]).
#[derive(Debug, Default)]
struct PlaceWords {
    /// Per run of words naming a place, every place it names.
    words: Vec<Vec<Place>>,
    /// Per run, whether it holds a digit, as a postcode does.
    digits: Vec<bool>,
    /// Runs of words naming no place.
    skipped: usize,
    /// Runs naming a place one edit off.
    fuzzy: usize,
    /// When the query names no country, those its postcodes are used in.
    countries: Vec<String>,
    /// Each word read for a place, `word/label/matched/probability`, for
    /// the log.
    log: String,
    /// Per run, its weight: the mean probability of its words naming a
    /// place, in thousandths.
    weights: Vec<usize>,
}

/// What a search checks each hit of a plan against (see
/// [`TextSearcher::kept`]).
struct Checks {
    places: StatedPlaces,
    stated: Vec<StatedArea>,
    loose: Vec<String>,
    /// The countries the plan names, the filter when the caller names none.
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

/// Records of a pool explained by joint decoding at most.
const MAX_EXPLAINED: usize = 64;

/// What one query's searches share: what they may spend, and what they
/// learn of the records they find, so a record that several searches find,
/// or several checks ask about, is read from the Pack once.
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

    /// Move the deadline `by` later.
    fn extend(&self, by: Duration) {
        self.deadline.set(self.deadline.get() + by);
    }

    /// Whether one more search may run, counting it: the first always does,
    /// so every query is searched; the rest only within the budget. An
    /// anytime search: past the budget the pool answers with what it holds.
    fn allows_search(&self) -> bool {
        let searches = self.searches.get();
        if searches > 0 && (searches >= MAX_SEARCHES || Instant::now() >= self.deadline.get()) {
            return false;
        }
        self.searches.set(searches + 1);
        true
    }
}

/// Every hit the searches of one query found, with what it rests on.
#[derive(Debug, Default)]
struct Pool {
    candidates: Vec<Candidate>,
}

#[derive(Debug)]
struct Candidate {
    hit: TextSearchHit,
    /// The reading whose search found it.
    reading: usize,
    confirmed: bool,
    /// Its own postcode or street contradicts the query: it answers only
    /// after every hit that does not.
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
    /// Fill in what each hit's signals say of the pool around it.
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

    /// Whether any hit answers the query without contradicting it.
    fn answers(&self) -> bool {
        self.candidates
            .iter()
            .any(|candidate| !candidate.contradicted)
    }

    /// The hits of the reading whose hit rests on the strongest
    /// [`Evidence`], those found as that hit was, in the order its search
    /// ranked them; after them the hits that contradict the query, of that
    /// reading and the ones before it. With no hit that does not contradict
    /// the query, those that do alone, from the first reading that found
    /// any. At most `limit`, each record once, with the text of the
    /// reading. `None` when the pool is empty.
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
            // Stable, so hits found alike keep the order their search
            // ranked them in.
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

    /// Every hit, ordered by `ranker`'s score of what it rests on, at most
    /// `limit`, each record once, with the text of the reading that found
    /// the first. `None` when the pool is empty.
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

/// `pieces` with the country the query writes (`written`, see
/// [`TextSearcher::written_country`]) read as its country where the parser
/// read every word of it as a town, when the parser scored the query by
/// that country's rules (`parsed`). A code names towns and cantons as well
/// ("FR" of Fribourg), and a town every answer must lie in would keep only
/// the places of that name. A country written after a town of another
/// ("Fribourg FR", scored as Swiss) stays as read.
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

/// The edits a query word may be from a record's: one for words of
/// [`MIN_TYPO_CHARS`] to six characters, two for longer ones, and every
/// character a wrong encoding replaced ("Is\u{fffd}re") counted on top, as
/// each is an edit whatever the word's length. `None` when it is matched only
/// as written: it is short, or holds a digit. In a dense run of numbers every
/// edit names another real address ("12 Main St" is one edit from "13 Main
/// St"), so a house number, a postcode, a unit or a route number is never a
/// typo.
fn allowed_edits(word: &str) -> Option<u8> {
    let chars = word.chars().count();
    if chars < MIN_TYPO_CHARS || word.chars().any(char::is_numeric) {
        return None;
    }
    let base = if chars < LONG_TYPO_CHARS { 1 } else { 2 };
    Some((base + replaced_characters(word)).min(2) as u8)
}

/// The edits `other` is from `word` when it may be a typo of it: within
/// the edits `word` allows (see [`allowed_edits`]), and with the first
/// [`TYPO_PREFIX_CHARS`] characters as written.
fn typo_distance(word: &str, other: &str) -> Option<u8> {
    if !other.starts_with(first_chars(word)) {
        return None;
    }
    // Ranking asks of every query word against every word of every hit,
    // and building a two-edit automaton costs far more than running one:
    // each thread keeps the automata of the words it saw last, and how many
    // states they hold between them.
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

/// Automata of query words each thread keeps for [`typo_distance`], `None`
/// for a word that allows no typo, and how many states they hold between them.
#[derive(Default)]
struct TypoAutomata {
    automata: HashMap<String, Option<Rc<DFA>>>,
    states: usize,
}

/// Automata of query words each thread keeps for [`typo_distance`].
const MAX_CACHED_AUTOMATA: usize = 4_096;

/// States those automata may hold between them. A state is a table of 256
/// `u32`, 1 KiB, and the two-edit automaton of a ten-letter word has about
/// 250, so 4,096 automata came to a gigabyte a thread. Lookups run on the
/// server's blocking threads, which come and go, so under load each new
/// thread filled one more: on 16 CPUs the server ran out of memory and
/// refused nearly every request until it was restarted. 16 MiB a thread
/// still holds every word of a query and those of the queries just before.
const MAX_CACHED_AUTOMATON_STATES: usize = 16 * 1024;

/// The first [`TYPO_PREFIX_CHARS`] characters of `word`, which a typo
/// keeps as written.
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

/// What each query word may match besides itself, by the field of the text
/// index it is searched in and the word (see [`TextSearcher::spellings`]).
#[derive(Debug, Clone, Default, PartialEq)]
struct Spellings {
    words: HashMap<(Field, String), Spelt>,
}

/// What the Pack says of the words of a plan that may name where a record
/// is.
#[derive(Debug, Default)]
struct AreaWords {
    /// The words of its localities the Pack knows as a word of an area (see
    /// [`TextSearcher::subject_clauses`]).
    known: HashSet<String>,
    postcodes: PostcodeWeights,
    /// What each word of what is sought scores where it stands in for an
    /// area (see [`TextSearcher::stand_in_weights`]).
    stand_ins: HashMap<String, Score>,
}

/// What agreeing on a postcode weighs, by each term of a plan the postcode
/// field may index and each country the Pack holds a postcode of it in.
/// Fellegi-Sunter: `ln(m / u)`, `u` the share of the country's addresses
/// stating a postcode that state this one, `m` [`MATCH_AGREEMENT`]. One
/// global weight cannot fit both a UK postcode, a street's side, and a US
/// ZIP, thousands of addresses; the counts give each its own. Counted per
/// country, not over the Pack, as a code names a postcode only within one
/// (AU and NZ both have a "2110"), and a Pack covering more countries would
/// otherwise make every postcode rarer.
#[derive(Debug, Default)]
struct PostcodeWeights {
    terms: HashMap<String, Vec<(Option<CountryCode>, Score)>>,
}

impl PostcodeWeights {
    /// What agreeing on a postcode `covered` of `addresses` addresses state
    /// weighs. A postcode most addresses state says next to nothing, and
    /// never counts against a record.
    fn agreement(covered: u64, addresses: u64) -> Score {
        let covered = covered.max(1) as f64;
        let addresses = (addresses as f64).max(covered);
        (MATCH_AGREEMENT * addresses / covered).ln().max(0.0) as Score
    }
}

/// The terms of one field a query word may be a typo of.
#[derive(Debug, Clone, PartialEq)]
struct Spelt {
    /// The word of the name the parser read it as.
    corrected: Option<String>,
    /// The other terms near it (see [`TextSearcher::near_terms`]).
    near: Vec<String>,
}

/// A Levenshtein automaton, as Tantivy's term dictionaries walk one.
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

/// What [`TextSearcher::ranked_hits`] ranks by besides the text score.
#[derive(Debug, Clone, Default)]
struct Ranking {
    query_words: QueryWords,
    /// Whether the query asks for a house number.
    numbered: bool,
    /// The house number, when interpolation ranges can estimate it.
    house_number: Option<u32>,
    /// The house number as the address number field indexes it, to find
    /// the addresses of a street group that state it.
    number_text: Option<String>,
    /// The named unit and the number written together ("29 181").
    flat_text: Option<String>,
    /// The unit the query names, also the first part of a number written
    /// with its unit ("29" of "29 181").
    unit: Option<String>,
    postcode: QueryPostcode,
    /// Where the caller is, `[lon, lat]`.
    focus: Option<[f64; 2]>,
    /// Nearness within [`BIAS_BANDS`] ranks before the text score. A map
    /// search box means the one near: "starbucks ballard" in Seattle is not
    /// the one on Ballard Road in Oklahoma, though it matches better.
    near_first: bool,
    /// Words the query types, so a place whose name has no more is named
    /// whole (see [`RankKey::named_place`]).
    typed_words: u8,
    /// The words of what the query seeks, its subject and unplaced words:
    /// what an address or street
    /// must account for to rank above a POI (see
    /// [`TextSearcher::demoted`]). Empty leaves POIs where they rank.
    sought: Vec<Word>,
}

/// The query's words as written, before abbreviations were expanded, to
/// compare with records' own labels.
#[derive(Debug, Clone, Default)]
struct QueryWords {
    /// As [`search_key`] writes them, and the words of the structured
    /// context.
    search: HashSet<String>,
    /// As [`exact_key`] writes them.
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
        Self {
            search: words(search_key(query)),
            exact: words(exact_key(query)),
        }
    }
}

/// The postcode a query names, to rank records by whether their own
/// postcodes hold it, and else by how much of it they share from its start:
/// "M5C 2A1" agrees with "M5C 1B2" on three characters, with "M4C 1B2" on
/// one. Longer agreement ranks first, whatever the country's postcode format.
#[derive(Debug, Clone, Default)]
struct QueryPostcode {
    /// Compacted forms of the postcode part: all of it, and from its first
    /// word with a digit when other words lead ("on m5c 2a1" is "m5c2a1").
    forms: Vec<String>,
    /// The part as the postcode field indexes it ("m5v 1a1").
    exact: String,
    /// Where it lies, found by [`TextSearcher::postcode_areas`].
    areas: Vec<PostcodeArea>,
}

/// Where a postcode lies in one country: the points of the postcode records
/// that place it, and how far it reaches from them: as far as they spread,
/// or to the nearest other postcode when that lies farther.
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

/// How [`QueryPostcode::rank`] ranks a record its own postcodes place in the
/// named postcode.
const POSTCODE_HELD: u8 = u8::MAX;
/// The least [`QueryPostcode::rank`] gives a record with no postcode of its
/// own: one whose own postcodes contradict the named one ranks below.
const POSTCODE_UNKNOWN: u8 = 128;

impl QueryPostcode {
    /// The postcode a query writes in `words`, not yet placed.
    fn of(words: &[String]) -> Self {
        let first_digit = words
            .iter()
            .position(|word| word.chars().any(|character| character.is_ascii_digit()))
            .unwrap_or_default();
        Self {
            forms: unique_strings(vec![words.concat(), words[first_digit..].concat()]),
            exact: words.join(" "),
            areas: Vec::new(),
        }
    }

    /// Whether `postcode`, compacted, is in the named one: either starts the
    /// other.
    fn holds(&self, postcode: &str) -> bool {
        self.forms
            .iter()
            .any(|form| postcode.starts_with(form.as_str()) || form.starts_with(postcode))
    }

    /// A record's rank by the named postcode, larger is better. `own` is
    /// whether the postcodes it is in by its own data hold the named one
    /// and the longest prefix of it they share, `None` when it has none;
    /// `area` the same of the postcode centroid nearest it. Its own
    /// postcodes holding it come first and contradicting it last, by the
    /// prefix they share: a mismatch ranks down, never out. Between them,
    /// with none of its own, the nearest centroid holding it, then the
    /// record's [`band`](PostcodeNearness::band) from the postcode's own
    /// records, then the prefix the nearest centroid shares.
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

    /// [`Self::rank`] of compacted postcodes, without the bands.
    fn rank_of(&self, own: &[String], area: Option<&str>) -> u8 {
        let read = |postcode: &str| (self.holds(postcode), self.agreement(postcode));
        let own = own.iter().map(|postcode| read(postcode)).reduce(
            |(left_held, left), (right_held, right)| (left_held || right_held, left.max(right)),
        );
        Self::rank(own, area.map(read), || None)
    }

    /// How far a point lies from the nearest record placing the postcode,
    /// when the Pack has any.
    fn distance_m(&self, lon: f64, lat: f64) -> Option<f64> {
        self.areas
            .iter()
            .map(|area| area.distance_m(lon, lat))
            .min_by(f64::total_cmp)
    }

    /// Whether a point lies in the postcode by where its area lies: within
    /// its reach it does, beyond [`POSTCODE_CONTRADICTING_REACHES`] of them
    /// it does not, and between, or with no area, nothing says. Of the
    /// areas of several countries the one it lies nearest decides.
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

/// Fallback only when neither searcher has an in-order match, so a query
/// that answered before keeps its answer and speed.
pub fn suggest(
    addresses: &TextSearcher,
    places: &TextSearcher,
    options: &TextAutocompleteOptions,
) -> Result<Vec<TextSearchHit>> {
    for completion in Completion::ALL {
        let mixed = mix(
            &options.query,
            addresses.autocomplete_as(options, completion)?,
            places.autocomplete_as(options, completion)?,
            options.limit,
            options.focus.is_some(),
        );
        if !mixed.is_empty() {
            return Ok(mixed);
        }
    }
    Ok(Vec::new())
}

/// Scores over different fields do not compare, so the query decides the
/// order: a leading house number puts addresses first; else a place whose
/// name starts as the query, then addresses, then other places.
pub fn mix_suggestions(
    query: &str,
    addresses: Vec<TextSearchHit>,
    places: Vec<TextSearchHit>,
    limit: usize,
) -> Vec<TextSearchHit> {
    mix(query, addresses, places, limit, false)
}

/// With `near_first`, a town or region typed whole leads: near POIs that
/// start with its name ("Tacoma Screw") would else bury it.
fn mix(
    query: &str,
    mut addresses: Vec<TextSearchHit>,
    places: Vec<TextSearchHit>,
    limit: usize,
    near_first: bool,
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
                // Far, only once typed out: "amazon" is not yet Amazonas.
                hit.named_place
                    && search_key(&hit.record.name).is_some_and(|name| {
                        if usize::from(hit.signals.focus_band) < BIAS_BANDS {
                            name.split_whitespace().count() == tokens.len()
                                && name_starts_as(&hit.record.name, &tokens)
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
            .partition(|hit| name_starts_as(&hit.record.name, &tokens));
        mixed.extend(named);
        mixed.extend(addresses);
        mixed.extend(others);
    }
    mixed.truncate(limit);
    mixed
}

fn name_starts_as(name: &str, words: &[String]) -> bool {
    let Some(name) = search_key(name) else {
        return false;
    };
    let name = name.split_whitespace().collect::<Vec<_>>();
    !words.is_empty()
        && words.len() <= name.len()
        && words
            .iter()
            .zip(&name)
            .enumerate()
            .all(|(at, (typed, word))| types_word(typed, word, at + 1 == words.len()))
}

fn effective_autocomplete_limit(limit: usize) -> usize {
    let limit = effective_limit(limit);
    limit.min(MAX_AUTOCOMPLETE_LIMIT)
}

/// A query's words by role, from one labelling of its tokens by the address
/// parser (see [`crate::parse::lattice`]). Street words name what is sought;
/// words the parser placed nowhere may; locality, region, postcode and
/// country words place it; unit words only rank (see
/// [`TextSearcher::build_query`]).
#[derive(Debug, Clone, Default, PartialEq)]
struct QueryPlan {
    /// The house number the parser read ("670", "221b", "993_5"), when other
    /// words name the street it is on.
    house_number: Option<HouseNumber>,
    /// The unit the query names by a number ("29" of "unit 29" or of
    /// "29/181" read as a unit), to find and rank the flat.
    unit: Option<String>,
    /// The words that name [`Self::unit`] as written ("unit 602"), kept for
    /// [`Self::text`]. They score nothing.
    unit_words: Vec<String>,
    /// The words of what is sought, in order: its street's, or of a query
    /// with neither street nor unplaced words, its first place or its
    /// number.
    subject: Vec<SubjectWord>,
    /// Words the parser could not place: a suite designator, a country
    /// exonym, a word of a language the Pack does not speak. Each only
    /// ranks, unless the plan has no subject: then most must match (see
    /// [`unplaced_floor`]).
    unplaced: Vec<Word>,
    optional: Vec<String>,
    /// Each run of locality, region, postcode or country words, in order.
    context: Vec<ContextPart>,
    /// What its words may match besides themselves (see
    /// [`TextSearcher::spellings`]).
    spellings: Arc<Spellings>,
    /// Whether the house number is one the parser labelled that a plain
    /// reading of its run would drop (see [`labelled_number`]).
    number_kept: bool,
}

/// A query word, with the word of the name the parser read it as when it
/// took it for a typo of one.
#[derive(Debug, Clone, PartialEq)]
struct Word {
    text: String,
    corrected: Option<String>,
}

impl Word {
    /// `written` words, each with the word of `corrected` in its place
    /// where that differs: corrections only when both have as many words.
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

/// A word of what a query seeks.
#[derive(Debug, Clone, PartialEq)]
struct SubjectWord {
    text: String,
    /// The parser's reading of it, when it took it for a typo.
    corrected: Option<String>,
    /// Whether a record must have it. A street's type, direction or
    /// qualifier ("boulevard", "north") only ranks: "Roosevelt Blvd N" is
    /// "Roosevelt Boulevard", and a "Main Street South" ranks below a "Main
    /// Street North" for "Main St N", never out. All are required when the
    /// street has no other words ("North Road").
    required: bool,
}

/// A run of query words that says where the sought record is.
#[derive(Debug, Clone, PartialEq)]
struct ContextPart {
    /// Locality, region, postcode or country.
    label: Label,
    words: Vec<String>,
    /// The words of the name the parser read the part as, when it took a
    /// word of it for a typo.
    corrected: Option<Vec<String>>,
}

impl ContextPart {
    /// The part of a structured field ("Toronto", "M5V 1M9"), `None` when
    /// it has no words.
    fn of(label: Label, value: Option<&str>) -> Option<Self> {
        let words = Tokens::new(value?).words;
        (!words.is_empty()).then_some(Self {
            label,
            words,
            corrected: None,
        })
    }

    fn is_postcode(&self) -> bool {
        self.label == Label::Postcode
    }
}

/// A locality or region a query names, as [`TextSearcher::area_agreement`]
/// checks it.
#[derive(Debug, Clone, PartialEq)]
struct StatedArea {
    label: Label,
    names: StatedNames,
    /// Whether addresses of the query's postcode state it as their town (see
    /// [`TextSearcher::postal_town`]).
    postal: bool,
}

/// The places a part of a query names, by the words of them the Pack knows
/// (see [`TextSearcher::stated_names`]).
#[derive(Debug, Clone, PartialEq)]
struct StatedNames {
    names: Vec<Vec<String>>,
    /// Whether one name agreeing will do: the names cannot all hold one
    /// address.
    any: bool,
    /// Whether the part is one name, so names an area of its own level
    /// only: a part made of several names may have read a region as a
    /// locality.
    whole: bool,
}

impl StatedNames {
    /// The words of `part` that `known` holds as one name, `None` when it
    /// holds none.
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

/// The countries and regions a query names, as the Pack knows them (see
/// [`TextSearcher::stated_places`]).
#[derive(Debug, Default)]
struct StatedPlaces {
    /// The countries its country parts name that the Pack holds records in,
    /// by the Pack's names and codes of countries.
    countries: Vec<CountryCode>,
    /// The words of the parts that name those countries.
    country_words: Vec<String>,
    regions: Vec<StatedRegion>,
}

/// A region a query names.
#[derive(Debug)]
struct StatedRegion {
    words: Vec<String>,
    /// The country most records stating it lie in, when that is no other
    /// country's name and the Pack holds records there.
    country: Option<CountryCode>,
    /// The regions the Pack lists for it (see
    /// [`crate::parse::lattice::dictionary::REGIONS_SECTION`]); empty when it
    /// lists none, as a Pack built before it listed them does.
    regions: Vec<RecordId>,
}

/// The words of a street that say its type or which way it runs, each in
/// all its forms, and the words that name it (see
/// [`TextSearcher::street_contradicts`]).
#[derive(Debug, Default)]
struct Descriptors {
    directions: HashSet<String>,
    types: HashSet<String>,
    names: Vec<String>,
}

/// What every pass over every reading of one query searches with.
struct Search<'a> {
    placed: &'a HashSet<String>,
    query_words: &'a QueryWords,
    filters: Filters<'a>,
    focus: Option<[f64; 2]>,
    limit: usize,
}

/// How many of `count` unplaced words a record must match when a query has
/// no subject: one may be missing of up to five, a quarter of more. Without
/// a subject the parser placed nothing, which is every word of a query in a
/// language the Pack's parser does not know.
fn unplaced_floor(count: usize) -> usize {
    match count {
        ..=1 => 1,
        2..=5 => count - 1,
        _ => count - count / 4,
    }
}

/// Where a run of words the parser read as a street or a place, from word
/// `from` on, holds a unit: a unit designator and the flat after it, a
/// number or a letter ("Suite A", "Ste G", "Bat-C"). A street's first word
/// is its own ("Unit Road"), so a street's run is read from its second.
fn unit_in(words: &[String], from: usize) -> Option<usize> {
    let flat = |word: &String| {
        word.chars().any(|character| character.is_ascii_digit()) || word.chars().count() == 1
    };
    (from..words.len().saturating_sub(1))
        .find(|&at| designates_unit(&words[at]) && flat(&words[at + 1]))
}

/// Words of a run, with the parser's corrections of them (see
/// [`ContextPart::corrected`]).
type RunPart = (Vec<String>, Option<Vec<String>>);

/// A run of words split around the unit at `at`: the words before it, the
/// unit's two words, and the words after it.
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

/// The flat a run of unit words names: its word with a digit, else the word
/// after its designator ("B" of "Flat B"), else a word that is none ("G" of
/// "G/9"). Libpostal's designators hold letters too ("b" of some language),
/// so a word after one is the flat whatever it is. `None` for a designator
/// alone.
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
    /// Plan one labelling of a query's tokens, with `context` (the
    /// structured fields of a geocoded row) after the parts the labelling
    /// names. `corrections` are the parser's, one per token where it has
    /// them (see [`crate::parse::lattice::ParsedQuery::corrections`]). `describes_street` tells a
    /// word that only describes a street from one of its name (see
    /// [`SubjectWord::required`]). `None` when the query has no words.
    fn from_tagged(
        tokens: &[Tagged],
        corrections: &[Option<String>],
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
        // Each run of one label, as written and, when the parser corrected
        // a word of it, as corrected.
        let mut runs = Vec::<(Label, Vec<String>, Option<Vec<String>>)>::new();
        for (index, tagged) in tokens.iter().enumerate() {
            let correction = corrections.get(index).cloned().flatten();
            match runs.last_mut() {
                Some((label, words, corrected)) if *label == tagged.label => {
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
        // A word the parser could not place, written just before the house
        // number with a digit of its own, is the flat's ("G11" of
        // "G11/40"): a street word holds no digit, and a number the parser
        // would have read.
        let unplaced_unit = runs
            .iter()
            .position(|(label, ..)| *label == Label::HouseNumber)
            .and_then(|at| at.checked_sub(1))
            .and_then(|before| match &runs[before] {
                (Label::Other, words, _) => words.last().cloned(),
                _ => None,
            })
            .filter(|word| word.chars().any(|character| character.is_ascii_digit()));
        for (label, words, corrected) in runs {
            match label {
                Label::HouseNumber if plan.house_number.is_none() => {
                    // A number written in parts ("993.5", "1384 1/2") is one
                    // house number; else its first word is.
                    if let Some(number) = HouseNumber::parse(&words.join("_")) {
                        plan.house_number = Some(number);
                    } else if let Some(number) = HouseNumber::parse(&words[0]) {
                        plan.house_number = Some(number);
                        plan.optional.extend(words.into_iter().skip(1));
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
                // A named flat chooses among the addresses of one street in
                // the tie-break and scores nothing: as words, "unit 2" would
                // rank a street with many flats above the street the query
                // names ("Unit 2, 1 Balfour Street" answered on Balfour Road).
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
                // A run the parser could not place that opens with a unit
                // designator and the flat after it ("unit 2", "suite 410")
                // is that flat, whatever the parser read it as.
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
                // A suite the parser read as a place ("Suite A" of "Suite A
                // Dayton") is the flat, and the words around it the place.
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
                            });
                        }
                    }
                }
                Label::Locality | Label::Region | Label::Postcode | Label::Country => {
                    plan.context.push(ContextPart {
                        label,
                        words,
                        corrected,
                    });
                }
            }
        }
        plan.context.extend(context.iter().cloned());
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
        plan.subject = street
            .into_iter()
            .map(|word| SubjectWord {
                required: !named || !describes_street(&word.text),
                text: word.text,
                corrected: word.corrected,
            })
            .collect();
        if plan.subject.is_empty() && plan.unplaced.is_empty() {
            // A query of places only ("Toronto", "Seaforth, Huron East")
            // seeks the first; a house number alone is sought as a word.
            let words = if !plan.context.is_empty() {
                let part = plan.context.remove(0);
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

    /// The words a street-type or direction abbreviation of the plan stands
    /// for, searched besides it: "tce" is "terrace" ("46 Highfield Tce" is a
    /// terrace before a "Highfield Way"). Libpostal's lists, every language
    /// alike: too ambiguous to rewrite the indexed text by ("pl" is "place"
    /// and "platz"), so the query asks for each. None for a word the text
    /// index expands, which it leaves as written on purpose ("St" before a
    /// name is "Saint"), or for a form the plan already seeks: "st" of "12
    /// St George Street" must not count the "street" again.
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

    /// The words and context parts of the plan that are searched in where a
    /// record is, so any may write a postcode: an unplaced word or one that
    /// stands in for an area as much as a postcode part.
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

    /// The plan as text: the house number, the words it seeks, then each
    /// context part after a comma.
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
            ..self.clone()
        })
    }

    /// Whether a plan with no house number reads as its postcode a word that
    /// could be one: "3010" of "3010 Coast Road" is a postcode in one
    /// reading and the house in another. "A1A 9Z9" never is.
    fn postcode_may_be_house_number(&self) -> bool {
        self.house_number.is_none()
            && self.context.iter().any(|part| {
                part.is_postcode()
                    && matches!(part.words.as_slice(), [word] if HouseNumber::parse(word).is_some())
            })
    }

    /// The postcode the last postcode part names. A postcode given apart
    /// from the query follows the parts parsed from it, so it wins over a
    /// house number read as a postcode ("6011 highland lakes road").
    fn postcode(&self) -> QueryPostcode {
        let Some(part) = self.context.iter().rev().find(|part| part.is_postcode()) else {
            return QueryPostcode::default();
        };
        QueryPostcode::of(&part.words)
    }

    /// Plans of the areas the plan names, finest first, each with the layers
    /// an area of its kind is: its postcode (and the code a postcode written
    /// with an extension starts with), each name of each locality (see
    /// [`TextSearcher::stated_names`], `names`), its region, its country.
    /// Each seeks its area in the coarser areas the plan names, and is ranked
    /// by its postcode (see [`TextSearcher::fallback`]).
    fn area_plans(
        &self,
        names: impl Fn(&ContextPart) -> Option<StatedNames>,
    ) -> Vec<(Self, &'static [Layer])> {
        const POSTCODE: &[Layer] = &[Layer::Postcode];
        const LOCALITY: &[Layer] = &[
            Layer::Locality,
            Layer::Neighbourhood,
            Layer::District,
            Layer::Place,
        ];
        const REGION: &[Layer] = &[Layer::Region, Layer::District];
        const COUNTRY: &[Layer] = &[Layer::Country];
        let rank = |label: Label| match label {
            Label::Postcode => 0,
            Label::Locality => 1,
            Label::Region => 2,
            _ => 3,
        };
        let mut plans = Vec::new();
        let mut add = |words: Vec<String>, label: Label, layers: &'static [Layer]| {
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
                    ..Self::default()
                },
                layers,
            ));
        };
        for part in self.context.iter().rev().filter(|part| part.is_postcode()) {
            add(part.words.clone(), Label::Postcode, POSTCODE);
            if part.words.len() > 1 {
                add(vec![part.words[0].clone()], Label::Postcode, POSTCODE);
            }
        }
        for part in self
            .context
            .iter()
            .filter(|part| part.label == Label::Locality)
        {
            let named = names(part).map_or_else(|| vec![part.words.clone()], |names| names.names);
            for name in named {
                add(name, Label::Locality, LOCALITY);
            }
        }
        for (label, layers) in [(Label::Region, REGION), (Label::Country, COUNTRY)] {
            for part in self.context.iter().filter(|part| part.label == label) {
                add(part.words.clone(), label, layers);
            }
        }
        plans
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
    /// The number written after `unit`, as the address number field indexes
    /// a flat ("29 181").
    fn flat_text(&self, unit: &str) -> String {
        format!("{unit} {}", self.text)
    }

    /// Digits with at most one letter after them: "670", "221b", but not
    /// "1st". The parts of a number written in parts, joined by "_", read as
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

/// [`TextSearcher::street_keys`], with `describes_street` telling a word
/// that only describes a street.
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

/// The house number of a run the parser labelled one that no plain number
/// starts: its first word that is one ("2" of "no 2", "51" of "nº 51"),
/// else the digits a word starts with ("116" of "116bis"), with where it
/// is in the run. A designator or a suffix the parser kept with the number
/// is no reason to drop the number.
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

/// Street words as a query's are planned, with their abbreviations
/// expanded as the text index expands them.
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
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>()
}

fn autocomplete_subject_query(field: Field, tokens: &[String]) -> Box<dyn Query> {
    let mut head = tokens
        .iter()
        .map(|token| Term::from_field_text(field, token))
        .collect::<Vec<_>>();
    let prefix = head.pop().expect("an autocomplete query has a word");
    if head.is_empty() {
        let mut query = PhrasePrefixQuery::new(vec![prefix]);
        query.set_max_expansions(AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS);
        return Box::new(query);
    }
    Box::new(PhrasePrefix::new(
        head,
        prefix,
        AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS,
    ))
}

/// The house number an autocomplete query starts with, when words follow it.
fn autocomplete_number(tokens: &[String]) -> Option<&str> {
    (tokens.len() > 1 && is_address_number_token(&tokens[0])).then(|| tokens[0].as_str())
}

fn is_address_number_token(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_digit())
}

mod named;

mod choose;

mod phrase_prefix;

use phrase_prefix::PhrasePrefix;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::{
        pack::Writer,
        record::{
            AddressComponents, AddressRecord, DerivedSourceProvenance, LocationPrecision,
            ObjectType, PostcodeRecord, SourceProvenance, StreetRecord, point_geometry,
        },
        text_index::PostcodeAreas,
    };

    use super::*;

    #[test]
    fn equal_scores_prefer_the_closest_label_then_points_then_lower_ids() {
        let temp_dir = temp_pack_path("search-ties");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = Writer::create(&temp_dir).expect("writer");
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
        writer.finish().expect("finish");

        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
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

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn equal_hits_prefer_an_address_no_source_disputes() {
        let temp_dir = temp_pack_path("search-disputed");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let mut disputed = address_record("osm:node:5", "", "342", "Albert Street", None, None);
        disputed.disputed = true;
        for record in [
            disputed,
            address_record("osm:node:7", "", "342", "Albert Street", None, None),
        ] {
            writer.write(&record.into(), None).expect("write");
        }
        writer.finish().expect("finish");

        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        assert_eq!(
            top_ids(&searcher, "342 Albert Street"),
            vec!["osm:node:7", "osm:node:5"]
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn answers_carry_a_confidence_only_from_a_calibrated_pack() {
        use crate::confidence::{FEATURES, Model};

        let temp_dir = temp_pack_path("search-confidence");
        let _ = std::fs::remove_dir_all(&temp_dir);
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
        let top = &hits[0].features;
        assert_eq!(top.pass, Pass::Strict);
        assert_eq!(top.number, Some(true));
        assert_eq!(top.dropped_words, 0);
        assert!(!top.lower_rank);

        let pack = pack::Reader::open(&temp_dir).expect("pack");
        let mut weights = [0.0; FEATURES];
        weights[0] = 1.0;
        Model::new(weights, std::collections::BTreeMap::new())
            .save(&pack)
            .expect("save");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        for query in ["10 King Street", "99 King Street", "King"] {
            let hits = search(&searcher, query);
            assert!(!hits.is_empty(), "{query}");
            for hit in hits {
                let confidence = hit
                    .confidence
                    .expect("a calibrated Pack rates every answer");
                assert!((0.0..=1.0).contains(&confidence), "{query}: {confidence}");
            }
        }
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

        // A model fit to another Pack is refused, not misread.
        let other = temp_pack_path("search-confidence-other");
        let _ = std::fs::remove_dir_all(&other);
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
        let _ = std::fs::remove_dir_all(other);
        let _ = std::fs::remove_dir_all(temp_dir);
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
        let temp_dir = temp_pack_path("search-focus");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        // Forty streets of one name the query cannot tell apart, more than
        // the rank window holds for one hit, the nearest written last.
        for index in 1..=40 {
            let mut street = street_record(&format!("osm:way:{index}"), "Mill Road");
            let lon = -100.0 + f64::from(index);
            street.geometry = point_geometry(lon, 45.0);
            street.representative_point = [lon, 45.0];
            writer.write(&street.into(), None).expect("street");
        }
        // The better match lies far away, the worse beside the caller.
        let mut prince =
            address_record("osm:node:1", "", "342", "Prince Albert Street", None, None);
        prince.geometry = point_geometry(-60.0, 45.0);
        writer.write(&prince.into(), None).expect("prince");
        let mut albert = address_record("osm:node:2", "", "342", "Albert Street", None, None);
        albert.geometry = point_geometry(-120.0, 45.0);
        writer.write(&albert.into(), None).expect("albert");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(
            search_near(&searcher, "Mill Road", 1, Some([-60.001, 45.0]), None),
            ["osm:way:40"],
            "the nearest of the tied makes the window and ranks first"
        );
        assert_eq!(
            search_near(&searcher, "Mill Road", 2, Some([-98.999, 45.0]), None),
            ["osm:way:1", "osm:way:2"]
        );
        assert_eq!(
            search_near(&searcher, "342 Albert Street", 2, Some([-60.0, 45.0]), None)[0],
            "osm:node:2",
            "a focus never outranks a better match"
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn of_equal_scores_the_window_keeps_the_closest_name_then_the_larger_place() {
        use crate::record::{PlaceLayer, PlaceRecord, Record};

        let temp_dir = temp_pack_path("search-window-ties");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        // More roads of the name than the rank window holds, written first,
        // so a window that kept any of the tie would keep only roads.
        for index in 1..=60 {
            writer
                .write(
                    &street_record(&format!("osm:way:{index}"), "Richmond Road").into(),
                    None,
                )
                .expect("road");
        }
        for (object_id, place_type) in [(101, "hamlet"), (102, "town")] {
            writer
                .write(
                    &Record::Place(
                        PlaceLayer::Locality,
                        PlaceRecord {
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
        // A street known by a second name repeats a word of it, which must
        // not score it over the street the query names.
        let mut upper = street_record("osm:way:201", "Upper Queen Street");
        upper.alt_names = vec!["Belgium Street".to_string()];
        writer.write(&upper.into(), None).expect("upper");
        writer
            .write(&street_record("osm:way:202", "Queen Street").into(), None)
            .expect("queen");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "Richmond")[..2],
            ["osm:node:102", "osm:node:101"],
            "the town, then the hamlet, before any road"
        );
        assert_eq!(top_ids(&searcher, "Queen Street")[0], "osm:way:202");
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn a_known_area_word_places_the_record_in_every_reading() {
        use crate::{
            context::AdminContextTuple,
            pack::RecordContext,
            record::{PlaceLayer, PlaceRecord, Record},
        };

        let temp_dir = temp_pack_path("search-loose-areas");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let mut locality = |name: &str, object_id: i64| {
            writer
                .write(
                    &Record::Place(
                        PlaceLayer::Locality,
                        PlaceRecord {
                            alt_names: Vec::new(),
                            name: name.to_string(),
                            place_type: "city".to_string(),
                            geometry: point_geometry(-79.0, 43.0),
                            source: SourceProvenance::osm(ObjectType::Relation, object_id),
                            prominence: Default::default(),
                            wikidata: None,
                        },
                    ),
                    None,
                )
                .expect("locality")
        };
        let auckland = locality("Auckland", 100);
        let christchurch = locality("Christchurch", 101);
        writer.set_context_names(
            [
                (auckland, vec!["Auckland".to_string()]),
                (christchurch, vec!["Christchurch".to_string()]),
            ]
            .into_iter()
            .collect(),
        );
        let within = |locality| {
            Some(RecordContext {
                admin_context: AdminContextTuple::from_tiers([(PlaceLayer::Locality, locality)]),
                flags: 0,
            })
        };
        let in_auckland = within(auckland);
        // So the Pack knows Christchurch as where a record is.
        writer
            .write(
                &street_record("osm:way:3", "Riccarton Road").into(),
                within(christchurch),
            )
            .expect("riccarton road");
        writer
            .write(
                &street_record("osm:way:1", "Ponsonby Road").into(),
                in_auckland.clone(),
            )
            .expect("ponsonby road");
        writer
            .write(
                &street_record("osm:way:2", "Christchurch Road").into(),
                in_auckland,
            )
            .expect("christchurch road");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        let search = |reading: &str, placed: &[&str]| {
            let plan = QueryPlan::from_tagged(&tagged(reading), &[], &[], |word| {
                searcher.describes_street(word)
            })
            .expect("plan");
            let placed = placed.iter().map(|word| word.to_string()).collect();
            let query = reading
                .split_whitespace()
                .filter_map(|pair| pair.split_once('/'))
                .map(|(word, _)| word)
                .collect::<Vec<_>>()
                .join(" ");
            let (hits, _) = searcher
                .search_plan(
                    &plan,
                    &placed,
                    Scope::Scoped,
                    &QueryWords::new(&query),
                    &Filters::default(),
                    None,
                    5,
                    &Session::new(SEARCH_BUDGET),
                    &mut |_, _, _, _| Ok(true),
                )
                .expect("search");
            hits.into_iter()
                .map(|hit| hit.record.id)
                .collect::<Vec<_>>()
        };

        // A region only ranks, so alone it places nothing; the best reading
        // read the word as a locality, and that holds.
        let as_region = "ponsonby/street road/street christchurch/region";
        assert_eq!(search(as_region, &[]), ["osm:way:1"]);
        assert!(search(as_region, &["christchurch"]).is_empty());
        // A word no label placed is where the record is too, once the Pack
        // knows it as an area's.
        assert!(search("ponsonby/street road/street christchurch/other", &[]).is_empty());
        // Unless it is a word of the record itself.
        assert_eq!(
            search("christchurch/street road/street", &["christchurch"])[0],
            "osm:way:2"
        );
        assert_eq!(search("christchurch/other road/other", &[])[0], "osm:way:2");
        // A word that only describes a street names no road of its own.
        assert!(search("ponsonby/other road/other christchurch/locality", &[]).is_empty());
        // A query the parser placed nothing in may leave a word unmatched,
        // even one the best reading took for a locality.
        assert_eq!(
            search(
                "ponsonby/other road/other christchurch/other",
                &["christchurch"]
            )[0],
            "osm:way:1"
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn a_bounding_box_keeps_only_the_records_inside_it() {
        let temp_dir = temp_pack_path("search-bbox");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        // One street group whose addresses run across the box's east edge,
        // and the same street name far away.
        for number in 1..=6 {
            let mut address = address_record(
                &format!("osm:node:{number}"),
                "",
                &number.to_string(),
                "Mill Road",
                None,
                None,
            );
            address.geometry = point_geometry(-79.0 + f64::from(number) * 0.001, 43.0);
            writer.write(&address.into(), None).expect("address");
        }
        let mut far = address_record("osm:node:9", "", "2", "Mill Road", None, None);
        far.geometry = point_geometry(10.0, 50.0);
        writer.write(&far.into(), None).expect("far");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        let bbox = BoundingBox {
            min_lon: -79.0,
            min_lat: 42.9,
            max_lon: -78.9965,
            max_lat: 43.1,
        };

        assert_eq!(
            search_near(&searcher, "2 Mill Road", 5, None, None).len(),
            2,
            "unboxed, both houses 2 answer"
        );
        assert_eq!(
            search_near(&searcher, "2 Mill Road", 5, None, Some(bbox)),
            ["osm:node:2"]
        );
        assert!(
            !search_near(&searcher, "5 Mill Road", 5, None, Some(bbox))
                .contains(&"osm:node:5".to_string()),
            "house 5 lies past the east edge, though its group's extent meets the box"
        );
        let suggestions = searcher
            .autocomplete(&TextAutocompleteOptions {
                query: "2 Mill".to_string(),
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
            ["osm:node:2"]
        );
        let _ = std::fs::remove_dir_all(temp_dir);
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

    #[test]
    fn geocode_rejects_hits_in_a_different_postcode() {
        let temp_dir = temp_pack_path("geocode-postcode");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        writer
            .write(
                &address_record("osm:node:1", "", "10", "King Street", None, Some("M5V 1A1"))
                    .into(),
                None,
            )
            .expect("write");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

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
        let mut writer = Writer::create(&temp_dir).expect("writer");
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
                        country: None,
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
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        for limit in [1, 10] {
            let hit = searcher
                .geocode_address(&AddressGeocodeOptions {
                    address: "10 King Street".to_string(),
                    locality: None,
                    region: None,
                    postcode: Some("K7L 1A1".to_string()),
                    limit,
                    layer: None,
                    countries: Vec::new(),
                })
                .expect("geocode")
                .map(|hit| hit.hit.record.id);
            assert_eq!(hit.as_deref(), Some("osm:node:99"), "limit {limit}");
        }
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn geocode_ranks_hits_by_distance_from_the_named_postcode() {
        let temp_dir = temp_pack_path("geocode-postcode-distance");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let mut write_postcode = |postcode: &str, lon: f64| {
            writer
                .write(
                    &PostcodeRecord {
                        postcode: postcode.to_string(),
                        country: None,
                        geometry: point_geometry(lon, 43.0),
                        source: DerivedSourceProvenance::osm_address_records(5),
                    }
                    .into(),
                    None,
                )
                .expect("postcode");
        };
        // A rural address lies about 12 km from the record of its postcode,
        // too far for the postcode nearest it to stand in for its own.
        write_postcode("3902", -76.15);
        for id in 1..=12 {
            write_postcode("3000", -90.0 - id as f64 + 0.001);
        }
        for id in 1..=12 {
            let town = format!("Town {id}");
            let mut record = address_record(
                &format!("osm:node:{id}"),
                "",
                "10",
                "King Street",
                Some(&town),
                None,
            );
            record.geometry = point_geometry(-90.0 - id as f64, 43.0);
            writer.write(&record.into(), None).expect("address");
        }
        let mut rural = address_record(
            "osm:node:99",
            "",
            "10",
            "King Street",
            Some("Bairnsdale"),
            None,
        );
        rural.geometry = point_geometry(-76.0, 43.0);
        writer.write(&rural.into(), None).expect("address");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        let hit = searcher
            .geocode_address(&AddressGeocodeOptions {
                address: "10 King Street".to_string(),
                locality: None,
                region: None,
                postcode: Some("3902".to_string()),
                limit: 1,
                layer: None,
                countries: Vec::new(),
            })
            .expect("geocode")
            .map(|hit| hit.hit.record.id);
        assert_eq!(hit.as_deref(), Some("osm:node:99"));
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn geocode_infers_the_postcode_area_of_hits_without_one() {
        let temp_dir = temp_pack_path("geocode-inferred-postcode");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        // An address without addr:postcode, inside postcode area K0J.
        let mut cobden = address_record("osm:node:1", "", "44", "Gould Street", None, None);
        cobden.geometry = point_geometry(-76.88, 45.63);
        writer.write(&cobden.into(), None).expect("cobden");
        writer
            .write(
                &PostcodeRecord {
                    postcode: "K0J 1K0".to_string(),
                    country: None,
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
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

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
    fn geocode_reads_a_postcode_two_countries_share_in_each_one() {
        use crate::parse::lattice::CountryCode;

        // AU and NZ both use "2110": Hunters Hill in Sydney, Papakura in
        // Auckland, each among postcodes of its own country.
        let codes = [
            ("AU", "2110", 151.150, -33.830),
            ("AU", "2111", 151.135, -33.815),
            ("AU", "2112", 151.120, -33.800),
            ("NZ", "2110", 174.944, -37.065),
            ("NZ", "2112", 174.900, -37.150),
            ("NZ", "2113", 174.980, -37.120),
        ];
        for (country, lon, lat) in [("NZ", 174.945, -37.066), ("AU", 151.151, -33.831)] {
            let temp_dir = temp_pack_path(&format!("geocode-postcode-shared-{country}"));
            let _ = std::fs::remove_dir_all(&temp_dir);
            let mut writer = Writer::create(&temp_dir).expect("writer");
            for (code_country, code, code_lon, code_lat) in codes {
                writer
                    .write(
                        &PostcodeRecord {
                            postcode: code.to_string(),
                            country: CountryCode::parse(code_country),
                            geometry: point_geometry(code_lon, code_lat),
                            source: DerivedSourceProvenance::osm_address_records(5),
                        }
                        .into(),
                        None,
                    )
                    .expect("postcode");
            }
            let mut address = address_record("osm:node:1", "", "10", "Elliot Street", None, None);
            address.geometry = point_geometry(lon, lat);
            writer.write(&address.into(), None).expect("address");
            writer.finish().expect("finish");
            let searcher = TextSearcher::open(&temp_dir).expect("searcher");

            assert_eq!(
                geocode(&searcher, "10 Elliot Street", None, Some("2110")).as_deref(),
                Some("osm:node:1"),
                "the {country} 2110 is not measured against the other country's"
            );
            // A code only the other country uses lies a sea away: the house
            // is no answer, and the row falls back to the postcode it names.
            let (foreign, other) = if country == "NZ" {
                ("2111", "au")
            } else {
                ("2113", "nz")
            };
            assert_eq!(
                geocode(&searcher, "10 Elliot Street", None, Some(foreign)),
                Some(format!("derived:osm:postcode:{other}:{foreign}")),
                "{foreign} contradicts the {country} address"
            );
            let _ = std::fs::remove_dir_all(temp_dir);
        }
    }

    /// A street's postcodes name the blocks its addresses state. The one a
    /// query asks for missing from them says nothing: where the hit lies
    /// decides, and the house in that postcode's neighbourhood wins.
    #[test]
    fn a_house_between_its_streets_postcodes_lies_in_the_one_asked_for() {
        let temp_dir = temp_pack_path("search-postcode-between");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        for (code, lon) in [
            ("M4M 1L2", -79.3330),
            ("M4M 1L5", -79.3338),
            ("M4M 1L7", -79.3345),
            ("M6J 1H9", -79.4250),
            ("M6J 1J2", -79.4258),
            ("M6J 1J4", -79.4265),
        ] {
            writer
                .write(
                    &PostcodeRecord {
                        postcode: code.to_string(),
                        country: None,
                        geometry: point_geometry(lon, 43.65),
                        source: DerivedSourceProvenance::osm_address_records(3),
                    }
                    .into(),
                    None,
                )
                .expect("postcode");
        }
        let mut east = address_record("osm:node:1", "", "1161", "Queen Street East", None, None);
        east.street_postcodes = vec!["M4M 1L2".to_string(), "M4M 1L5".to_string()];
        east.geometry = point_geometry(-79.3334, 43.65);
        let mut west = address_record("osm:node:2", "", "1161", "Queen Street West", None, None);
        west.street_postcodes = vec!["M6J 1H9".to_string(), "M6J 1J2".to_string()];
        west.geometry = point_geometry(-79.4255, 43.65);
        for record in [east, west] {
            writer.write(&record.into(), None).expect("address");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        for limit in [1, 10] {
            assert_eq!(
                top_ids_limited(&searcher, "1161 Queen Street, M4M 1L3", limit)
                    .first()
                    .map(String::as_str),
                Some("osm:node:1"),
                "limit {limit}"
            );
        }
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn geocode_accepts_hits_near_a_busier_neighbouring_postcode_area() {
        let temp_dir = temp_pack_path("geocode-postcode-border");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let mut border = address_record("osm:node:1", "", "5", "Line Road", None, None);
        border.geometry = point_geometry(-79.0, 43.0);
        writer.write(&border.into(), None).expect("address");
        let mut postcode = |code: String, lon: f64| {
            writer
                .write(
                    &PostcodeRecord {
                        postcode: code,
                        country: None,
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
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

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
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let mut locality = |name: &str, object_id: i64| {
            writer
                .write(
                    &Record::Place(
                        PlaceLayer::Locality,
                        PlaceRecord {
                            alt_names: Vec::new(),
                            name: name.to_string(),
                            place_type: "city".to_string(),
                            geometry: point_geometry(-79.0, 43.0),
                            source: SourceProvenance::osm(ObjectType::Node, object_id),
                            prominence: Default::default(),
                            wikidata: None,
                        },
                    ),
                    None,
                )
                .expect("locality")
        };
        let hamilton = locality("Hamilton", 100);
        let toronto = locality("Toronto", 101);
        // As a build names them, so the Pack knows the localities' words.
        writer.set_context_names(
            [
                (hamilton, vec!["Hamilton".to_string()]),
                (toronto, vec!["Toronto".to_string()]),
            ]
            .into_iter()
            .collect(),
        );
        let in_locality = |record_id| {
            Some(RecordContext {
                admin_context: AdminContextTuple::from_tiers([(PlaceLayer::Locality, record_id)]),
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
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(
            geocode(&searcher, "10 King St W", Some("Toronto"), None).as_deref(),
            Some("osm:node:2")
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn pois_named_after_a_place_or_street_are_found_apart_from_it() {
        use crate::{
            context::AdminContextTuple,
            pack::RecordContext,
            record::{PlaceLayer, PlaceRecord, PoiRecord, Record},
        };

        let temp_dir = temp_pack_path("search-poi-repeats");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let township = writer
            .write(
                &Record::Place(
                    PlaceLayer::Locality,
                    PlaceRecord {
                        alt_names: Vec::new(),
                        name: "Muskoka Lakes Township".to_string(),
                        place_type: "admin_level:8".to_string(),
                        geometry: point_geometry(-79.5, 45.1),
                        source: SourceProvenance::osm(ObjectType::Relation, 1),
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
            aliases: Vec::new(),
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
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
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

        assert_eq!(top("Muskoka Lakes Township"), "osm:relation:1");
        assert_eq!(top("Walmer Road"), "osm:node:3");
        let places = searcher.places();
        let place = |query: &str| top_ids(&places, query)[0].clone();
        assert_eq!(place("Walmer Road Parkette"), "osm:node:4");
        assert_eq!(place("Fire Station, Muskoka Lakes Township"), "osm:node:2");
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// A Pack of one street: a POI named after the street, a shop at one of
    /// its addresses and a bakery off it.
    #[test]
    fn address_search_answers_no_poi_and_place_search_finds_them() {
        let temp_dir = temp_pack_path("search-addresses-beat-pois");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let mut street = street_record("osm:way:1", "Walmer Road");
        street.geometry = queen_street_line();
        writer.write(&street.into(), None).expect("street");
        writer
            .write(
                &address_record("osm:node:4", "", "4", "Walmer Road", Some("Toronto"), None).into(),
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
            aliases: Vec::new(),
        };
        for record in [
            poi(10, "Walmer Road Walmer Road Variety", Some("6"), -79.0),
            poi(11, "Corner Store", Some("4"), -79.0),
            poi(12, "Walmer Road Bakery", None, -79.3),
        ] {
            writer.write(&record.into(), None).expect("poi");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
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
        assert_eq!(numbered[0].0, "osm:node:4", "{numbered:?}");
        assert!(no_poi(&numbered), "{numbered:?}");
        assert!(no_poi(&layers_of(&searcher, "Walmer Road Bakery")));
        let confirmed = layers_of(&places, "Corner Store 4 Walmer Road");
        assert_eq!(confirmed[0].0, "osm:node:11", "{confirmed:?}");
        assert_eq!(layers_of(&places, "Walmer Road Bakery")[0].0, "osm:node:12");
        assert_eq!(
            layers_of(&places, "Walmer Road Variety")[0].0,
            "osm:node:10"
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// More records of one name than the tie window holds: the best known
    /// place or POI is collected and ranked first, wherever it lies in the
    /// index.
    #[test]
    fn the_best_known_of_many_same_named_places_wins() {
        let temp_dir = temp_pack_path("search-place-prominence");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let known = Prominence {
            importance: 160,
            linked: true,
            languages: 100,
        };
        let city = |object_id, prominence| {
            Record::Place(
                PlaceLayer::Locality,
                crate::record::PlaceRecord {
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
                geometry: point_geometry(-79.0, 43.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, object_id),
                prominence,
                wikidata: None,
                brand: None,
                aliases: Vec::new(),
            })
        };
        writer
            .write(&city(1, Prominence::default()), None)
            .expect("benin");
        // More streets and cafes of the name than the tie window holds, all
        // scoring as the cities and the known cafe do.
        for id in 2..=(TIE_WINDOW as i64 + 100) {
            writer
                .write(
                    &street_record(&format!("osm:way:{id}"), "Paris").into(),
                    None,
                )
                .expect("street");
            writer
                .write(&cafe(id, Prominence::default()), None)
                .expect("cafe");
        }
        writer.write(&city(9_999, known), None).expect("france");
        writer.write(&cafe(9_998, known), None).expect("known cafe");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        assert_eq!(
            top_ids(&searcher, "Paris")[..2],
            ["osm:node:9999", "osm:node:1"]
        );
        assert_eq!(top_ids(&searcher.places(), "Paris")[0], "osm:node:9998");
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// A place the query names by any of its names leads, nearest the
    /// caller first, then best known, then, of places about as well read,
    /// the one it names by its own name; a place mapped twice answers once,
    /// and a street of the name never takes the lead for fame it has none of.
    #[test]
    fn a_place_named_by_any_of_its_names_leads_by_fame() {
        let pack = TempPack::new("search-place-any-name");
        let mut writer = Writer::create(&pack.0).expect("writer");
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
                crate::record::PlaceRecord {
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
        for record in [
            place(
                (ObjectType::Node, 1),
                "Roma",
                &["Rome", "Rom"],
                roma,
                160,
                Some(220),
            ),
            place(
                (ObjectType::Relation, 2),
                "Roma",
                &["Rome"],
                roma,
                160,
                Some(220),
            ),
            place((ObjectType::Node, 3), "Rome", &[], georgia, 60, None),
            place(
                (ObjectType::Node, 5),
                "Lagos",
                &[],
                [3.39, 6.46],
                154,
                Some(8673),
            ),
            place(
                (ObjectType::Node, 6),
                "Laos",
                &["Lagos"],
                [103.4, 20.0],
                163,
                Some(819),
            ),
            Record::from(street_record("osm:way:4", "Rome")),
        ] {
            writer.write(&record, None).expect("record");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&pack.0).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "Rome")[..2],
            ["osm:node:1", "osm:node:3"]
        );
        assert_eq!(top_ids_limited(&searcher, "Rome", 1), ["osm:node:1"]);
        assert_eq!(top_ids(&searcher, "Lagos"), ["osm:node:5", "osm:node:6"]);
        let near = searcher
            .search(&TextSearchOptions {
                query: "Rome".to_string(),
                limit: 1,
                layer: None,
                countries: Vec::new(),
                focus: Some(georgia),
                bbox: None,
            })
            .expect("search");
        assert_eq!(
            near.iter()
                .map(|hit| hit.record.id.as_str())
                .collect::<Vec<_>>(),
            ["osm:node:3"]
        );
    }

    /// On /places too a POI mapped twice with one Wikidata item, as a node
    /// and a way, answers once, and an alternate name finds it.
    #[test]
    fn a_poi_mapped_twice_answers_once() {
        let pack = TempPack::new("search-poi-one-item");
        let mut writer = Writer::create(&pack.0).expect("writer");
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
                aliases: Vec::new(),
            })
        };
        for record in [
            colosseo(ObjectType::Node, 1, Some(10285)),
            colosseo(ObjectType::Way, 2, Some(10285)),
            colosseo(ObjectType::Node, 3, None),
        ] {
            writer.write(&record, None).expect("poi");
        }
        writer.finish().expect("finish");
        let places = TextSearcher::open(&pack.0).expect("searcher").places();
        assert_eq!(top_ids(&places, "Colosseum"), ["osm:node:1", "osm:node:3"]);
    }

    /// Two cafes of one name the query cannot tell apart: the one its tags
    /// show better known first, never ahead of an address it ties with.
    #[test]
    fn equal_pois_prefer_the_better_known() {
        let temp_dir = temp_pack_path("search-poi-prominence");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let cafe = |object_id, prominence| PoiRecord {
            alt_names: Vec::new(),
            name: "Blue Door".to_string(),
            category: "amenity:cafe".to_string(),
            address: None,
            geometry: point_geometry(-79.0, 43.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, object_id),
            prominence,
            wikidata: None,
            brand: None,
            aliases: Vec::new(),
        };
        for record in [
            cafe(1, Prominence::default()),
            cafe(
                2,
                Prominence {
                    linked: false,
                    languages: 3,
                    importance: 0,
                },
            ),
            cafe(
                3,
                Prominence {
                    linked: true,
                    languages: 1,
                    importance: 0,
                },
            ),
        ] {
            writer.write(&record.into(), None).expect("cafe");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher").places();
        assert_eq!(
            top_ids(&searcher, "Blue Door"),
            ["osm:node:3", "osm:node:2", "osm:node:1"]
        );
        let _ = std::fs::remove_dir_all(temp_dir);

        // At a full tie a record of another layer stays ahead of a POI,
        // however well known.
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

    #[test]
    fn search_reports_queries_that_cannot_be_parsed() {
        let temp_dir = temp_pack_path("search-parse-error");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        writer
            .write(
                &address_record("osm:node:1", "", "10", "King Street", None, None).into(),
                None,
            )
            .expect("write");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

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
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn searches_and_hydrates_records_from_pack() {
        let temp_dir = temp_pack_path("search-hydrates");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = Writer::create(&temp_dir).expect("writer");
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

        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        let hits = searcher
            .search(&TextSearchOptions {
                query: "King Street Toronto".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
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

        let mut writer = Writer::create(&temp_dir).expect("writer");
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

        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        let hits = searcher
            .search(&TextSearchOptions {
                query: "King Street".to_string(),
                limit: 10,
                layer: Some("street".to_string()),
                countries: Vec::new(),
                focus: None,
                bbox: None,
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

        let mut writer = Writer::create(&temp_dir).expect("writer");
        writer
            .write(
                &PostcodeRecord {
                    postcode: "M5V".to_string(),
                    country: None,
                    geometry: point_geometry(-79.4, 43.6),
                    source: DerivedSourceProvenance::osm_address_records(2),
                }
                .into(),
                None,
            )
            .expect("write postcode");
        writer.finish().expect("finish");

        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        let hits = searcher
            .search(&TextSearchOptions {
                query: "M5V".to_string(),
                limit: 5,
                layer: Some("postcode".to_string()),
                countries: Vec::new(),
                focus: None,
                bbox: None,
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

        let mut writer = Writer::create(&temp_dir).expect("writer");
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

        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
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
        assert_eq!(hits[0].record_id, 0);
        assert_eq!(hits[0].record.label, "10 King Street, Toronto, M5V 1A1");

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn autocompletes_multi_token_prefixes() {
        let temp_dir = temp_pack_path("autocomplete-multi-token");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = Writer::create(&temp_dir).expect("writer");
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

        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        let hits = searcher
            .autocomplete(&TextAutocompleteOptions {
                query: "king st".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
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

        let mut writer = Writer::create(&temp_dir).expect("writer");
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

        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        let hits = searcher
            .autocomplete(&TextAutocompleteOptions {
                query: "kin".to_string(),
                limit: 10,
                layer: Some("street".to_string()),
                countries: Vec::new(),
                focus: None,
                bbox: None,
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

        let mut writer = Writer::create(&temp_dir).expect("writer");
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

        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
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
        assert_eq!(postcode_hits[0].record_id, 0);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn autocompletes_house_number_with_street_prefix() {
        let temp_dir = temp_pack_path("autocomplete-number-street-prefix");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = Writer::create(&temp_dir).expect("writer");
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

        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        let hits = searcher
            .autocomplete(&TextAutocompleteOptions {
                query: "221 bak".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("number plus street autocomplete");

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, 0);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// #172
    #[test]
    fn autocompletes_area_words_and_abbreviated_words() {
        let temp_dir = temp_pack_path("autocomplete-area-abbreviated");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let poi = |object_id, name: &str, number: &str, street: &str, area: &str| PoiRecord {
            alt_names: Vec::new(),
            name: name.to_string(),
            category: "amenity:cafe".to_string(),
            address: Some(
                address_record("osm:node:0", "", number, street, Some(area), None).address,
            ),
            geometry: point_geometry(-122.34, 47.61),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, object_id),
            prominence: Default::default(),
            wikidata: None,
            brand: None,
            aliases: Vec::new(),
        };
        for record in [
            poi(1, "Starbucks", "915", "Northwest 45th Street", "Ballard"),
            poi(2, "Starbucks", "3400", "Fremont Avenue North", "Fremont"),
            poi(3, "Fred Meyer", "915", "Northwest 45th Street", "Ballard"),
            poi(4, "Pike Place Market", "85", "Pike Street", "Seattle"),
            poi(5, "Pike Place Chowder", "1530", "Post Alley", "Seattle"),
        ] {
            writer.write(&record.into(), None).expect("poi");
        }
        for (id, number, street) in [
            ("osm:node:6", "2101", "North 34th Street"),
            ("osm:node:7", "2101", "1st Avenue"),
        ] {
            writer
                .write(
                    &address_record(id, "", number, street, Some("Seattle"), None).into(),
                    None,
                )
                .expect("address");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
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

        assert_eq!(suggested("starbucks ballard"), ["osm:node:1"]);
        assert_eq!(suggested("ballard starbucks"), ["osm:node:1"]);
        assert_eq!(suggested("fred meyer ballard"), ["osm:node:3"]);
        assert_eq!(suggested("starbucks ba"), ["osm:node:1"]);
        assert_eq!(suggested("pike pl mar")[0], "osm:node:4");
        assert_eq!(suggested("pike pl mkt")[0], "osm:node:4");
        assert_eq!(suggested("2101 N 34th St")[0], "osm:node:6");
        assert_eq!(suggested("2101 n 34")[0], "osm:node:6");
        // A town's name is no suggestion of every place in it.
        assert!(!suggested("ballard").contains(&"osm:node:3".to_string()));
        assert_eq!(suggested("pike place mar"), ["osm:node:4"]);
        assert_eq!(suggested("pike mkt"), ["osm:node:4", "osm:node:5"]);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn autocompletes_a_house_number_after_its_street() {
        let temp_dir = temp_pack_path("autocomplete-number-after-street");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        for (id, number, postcode) in [
            ("osm:node:1", "12", "1017AA"),
            ("osm:node:2", "29", "1211CK"),
        ] {
            writer
                .write(
                    &address_record(
                        id,
                        "",
                        number,
                        "Kerkstraat",
                        Some("Amsterdam"),
                        Some(postcode),
                    )
                    .into(),
                    None,
                )
                .expect("address");
        }
        writer
            .write(
                &PoiRecord {
                    alt_names: Vec::new(),
                    name: "Pier 39".to_string(),
                    category: "tourism:attraction".to_string(),
                    address: None,
                    geometry: point_geometry(-122.41, 37.81),
                    location_precision: LocationPrecision::Point,
                    source: SourceProvenance::osm(ObjectType::Node, 3),
                    prominence: Default::default(),
                    wikidata: None,
                    brand: None,
                    aliases: Vec::new(),
                }
                .into(),
                None,
            )
            .expect("poi");
        writer
            .write(
                &address_record("osm:node:4", "", "39", "Pier Street", None, None).into(),
                None,
            )
            .expect("pier street");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
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

        assert_eq!(first("kerkstraat 12").as_deref(), Some("osm:node:1"));
        assert_eq!(first("12 kerkstraat").as_deref(), Some("osm:node:1"));
        assert_eq!(first("pier 39").as_deref(), Some("osm:node:3"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// #179
    #[test]
    fn autocompletes_a_common_word_past_the_expansion_cap() {
        let temp_dir = temp_pack_path("autocomplete-common-completion");
        let _ = std::fs::remove_dir_all(&temp_dir);
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

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// #180: a chain's best known branches far away must not fill the
    /// window before the one near the caller.
    #[test]
    fn autocompletes_the_branch_near_the_focus_before_known_far_ones() {
        let temp_dir = temp_pack_path("autocomplete-near-branch");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let cafe = |object_id, [lon, lat]: [f64; 2], prominence| PoiRecord {
            alt_names: Vec::new(),
            name: "Starbucks".to_string(),
            category: "amenity:cafe".to_string(),
            address: None,
            geometry: point_geometry(lon, lat),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, object_id),
            prominence,
            wikidata: None,
            brand: None,
            aliases: Vec::new(),
        };
        let known = Prominence {
            importance: 30,
            linked: true,
            languages: 5,
        };
        for at in 0..MAX_RANK_WINDOW as i64 {
            writer
                .write(&cafe(at + 10, [-1.0, 53.0], known).into(), None)
                .expect("far");
        }
        writer
            .write(&cafe(1, [4.89, 52.37], Prominence::default()).into(), None)
            .expect("near");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        let options = TextAutocompleteOptions {
            query: "starbucks".to_string(),
            limit: 5,
            layer: None,
            countries: Vec::new(),
            focus: Some([4.89, 52.37]),
            bbox: None,
        };

        let found = suggest(&searcher, &searcher.places(), &options).expect("suggest");
        assert_eq!(found[0].record.id, "osm:node:1");

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// #180: within 50 km of the focus the near match leads, though a far
    /// one matches better; beyond, the better match does.
    #[test]
    fn autocompletes_the_near_match_before_a_better_far_one() {
        let temp_dir = temp_pack_path("autocomplete-near-match");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let poi = |object_id, name: &str, [lon, lat]: [f64; 2]| PoiRecord {
            alt_names: Vec::new(),
            name: name.to_string(),
            category: "amenity:cafe".to_string(),
            address: None,
            geometry: point_geometry(lon, lat),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, object_id),
            prominence: Default::default(),
            wikidata: None,
            brand: None,
            aliases: Vec::new(),
        };
        writer
            .write(&poi(1, "Pike Place", [-97.26, 34.74]).into(), None)
            .expect("far");
        writer
            .write(
                &poi(2, "Pike Place Fish Market", [-122.34, 47.61]).into(),
                None,
            )
            .expect("near");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        let first = |focus| {
            let options = TextAutocompleteOptions {
                query: "pike place".to_string(),
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

        assert_eq!(first([-122.33, 47.61]), "osm:node:2");
        // 80 km from the near one: past the bias, the better match leads.
        assert_eq!(first([-123.4, 47.61]), "osm:node:1");

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// #180: a town typed whole leads near shops named after it when known
    /// beyond its own language; a hamlet that is not leaves them first.
    #[test]
    fn autocompletes_a_known_town_typed_whole_before_near_shops() {
        let temp_dir = temp_pack_path("autocomplete-known-town");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let town = |object_id, name: &str, [lon, lat]: [f64; 2], prominence| {
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
        };
        let shop = |object_id, name: &str| {
            Record::from(PoiRecord {
                alt_names: Vec::new(),
                name: name.to_string(),
                category: "shop:bakery".to_string(),
                address: None,
                geometry: point_geometry(-122.34, 47.61),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, object_id),
                prominence: Default::default(),
                wikidata: None,
                brand: None,
                aliases: Vec::new(),
            })
        };
        let known = Prominence {
            importance: 0,
            linked: true,
            languages: 40,
        };
        writer
            .write(&town(1, "Paris", [2.35, 48.86], known), None)
            .expect("paris");
        writer
            .write(&town(2, "Pike", [-96.3, 33.3], Prominence::default()), None)
            .expect("pike");
        writer
            .write(&shop(3, "Paris Bakery"), None)
            .expect("bakery");
        writer
            .write(&shop(4, "Pike Grocery"), None)
            .expect("grocery");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        let first = |query: &str| {
            let options = TextAutocompleteOptions {
                query: query.to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: Some([-122.33, 47.61]),
                bbox: None,
            };
            suggest(&searcher, &searcher.places(), &options).expect("suggest")[0]
                .record
                .id
                .clone()
        };

        assert_eq!(first("paris"), "osm:node:1");
        assert_eq!(first("pike"), "osm:node:4");

        let _ = std::fs::remove_dir_all(temp_dir);
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
        assert!(name_starts_as("Pike Place Market", &words("pike pl mkt")));
        assert!(name_starts_as(
            "Pike Place Market",
            &words("pike place mar")
        ));
        assert!(!name_starts_as("Pike Place Market", &words("place pike")));
        assert!(!name_starts_as("Pike", &words("pike place")));
    }

    #[test]
    fn autocomplete_ignores_blank_and_short_queries() {
        let temp_dir = temp_pack_path("autocomplete-short");
        let _ = std::fs::remove_dir_all(&temp_dir);

        let mut writer = Writer::create(&temp_dir).expect("writer");
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

        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

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

        let _ = std::fs::remove_dir_all(temp_dir);
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
        let temp_dir = temp_pack_path("search-lenient");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
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
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(top_ids(&searcher, "10 King Street"), vec!["osm:node:1"]);
        // No record has 99: the street answers, never the address with 10.
        assert_eq!(top_ids(&searcher, "99 King St"), vec!["osm:way:9"]);
        // Geocoding falls back to the street too, never to another number.
        assert_eq!(
            geocode(&searcher, "99 King Street", None, None).as_deref(),
            Some("osm:way:9")
        );
        // A street states no postcode: no postcode data confirms it here.
        assert_eq!(
            geocode(&searcher, "99 King Street", None, Some("M5V 1A1")),
            None
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn a_long_word_with_its_second_and_third_letters_swapped_is_one_typo() {
        let temp_dir = temp_pack_path("search-swapped-letters");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        writer
            .write(
                &address_record("osm:node:1", "", "53", "Michael Jones Drive", None, None).into(),
                None,
            )
            .expect("address");
        writer
            .write(
                &address_record("osm:node:2", "", "53", "Morgan Jones Drive", None, None).into(),
                None,
            )
            .expect("address");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(typo_distance("mcihael", "michael"), Some(1));
        assert_eq!(
            top_ids(&searcher, "53 Mcihael Jones Drive")
                .first()
                .map(String::as_str),
            Some("osm:node:1")
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn an_abbreviated_street_type_ranks_the_street_of_that_type_first() {
        let temp_dir = temp_pack_path("search-street-type-abbreviation");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        for (id, street) in [
            ("osm:node:1", "Highfield Way"),
            ("osm:node:2", "Highfield Terrace"),
            ("osm:node:3", "Highfield Road"),
        ] {
            writer
                .write(
                    &address_record(id, "", "46", street, None, None).into(),
                    None,
                )
                .expect("address");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        // Libpostal's lists hold "tce", which the text index keeps as
        // written.
        assert_eq!(
            top_ids(&searcher, "46 Highfield Tce")
                .first()
                .map(String::as_str),
            Some("osm:node:2")
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn the_street_the_query_names_beats_one_with_more_words() {
        let temp_dir = temp_pack_path("search-exact-street-name");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let mut flat = address_record("osm:node:1", "", "45", "Lomond Crescent", None, None);
        flat.address.unit = Some("2".to_string());
        let records = [
            flat,
            address_record("osm:node:2", "", "45", "Ben Lomond Crescent", None, None),
            address_record("osm:node:3", "", "47", "Mill Road", None, None),
            address_record(
                "osm:node:4",
                "",
                "47",
                "Millton Park Road",
                Some("Mill Road"),
                None,
            ),
        ];
        for record in records {
            writer.write(&record.into(), None).expect("address");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        // A flat of the street named before the house of another street.
        assert_eq!(
            top_ids(&searcher, "45 Lomond Crescent")
                .first()
                .map(String::as_str),
            Some("osm:node:1")
        );
        // "mill" in the locality Mill Road stands in for less than in the
        // street, though "road" names few areas and many streets.
        assert_eq!(
            top_ids(&searcher, "47 Mill Road")
                .first()
                .map(String::as_str),
            Some("osm:node:3")
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn stated_numbers_rank_before_range_estimates_before_streets() {
        use crate::record::{
            InterpolationAddressComponents, InterpolationRange, InterpolationRecord,
        };

        let temp_dir = temp_pack_path("search-tiers");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
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
                    source: SourceProvenance::osm(ObjectType::Way, 20),
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
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(
            top_ids(&searcher, "10 Queen Street"),
            vec!["osm:node:10", "osm:way:20:interp:2-98"],
            "the stated number first, then the range that contains it"
        );

        let hits = searcher
            .search(&TextSearchOptions {
                query: "50 Queen Street".to_string(),
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
        assert_eq!(estimate.label, "50 Queen Street");
        let point = estimate.point.expect("point");
        assert_eq!(point.precision, PointPrecision::Estimated);
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
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let mut locality = |name: &str, object_id: i64| {
            writer
                .write(
                    &Record::Place(
                        PlaceLayer::Locality,
                        PlaceRecord {
                            alt_names: Vec::new(),
                            name: name.to_string(),
                            place_type: "city".to_string(),
                            geometry: point_geometry(-79.0, 43.0),
                            source: SourceProvenance::osm(ObjectType::Relation, object_id),
                            prominence: Default::default(),
                            wikidata: None,
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
                (toronto, vec!["Toronto".to_string()]),
                (st_catharines, vec!["St. Catharines".to_string()]),
            ]
            .into_iter()
            .collect(),
        );
        let in_locality = |record_id| {
            Some(RecordContext {
                admin_context: AdminContextTuple::from_tiers([(PlaceLayer::Locality, record_id)]),
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
                    source: SourceProvenance::osm(ObjectType::Way, 20),
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
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

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
        // A geocoded row falls back to the street in the named town, not to
        // the number elsewhere.
        assert_eq!(
            geocode(&searcher, "99 Queen Street", Some("Toronto"), None).as_deref(),
            Some("osm:way:50")
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
        let mut writer = Writer::create(&temp_dir).expect("writer");
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
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        // The index knows no "A1A 9Z9", but the record near "A1A 1A1" shares
        // its first three characters. The record that states "B2B 2B2"
        // contradicts the query outright and ranks last: a mismatch ranks
        // down, never out.
        assert_eq!(
            top_ids(&searcher, "5 Main Street, A1A 9Z9"),
            vec!["osm:node:2", "osm:node:3", "osm:node:1"]
        );
        assert_eq!(
            top_ids(&searcher, "5 Main Street, Toronto ON B2B 2B2")[0],
            "osm:node:1",
            "the postcode starts at its first word with a digit"
        );
        // A postcode nobody shares, which this Pack's parser does not read
        // as one, prefers no one: what a label states beyond the name is
        // where the record lies, and counts neither way.
        let mut ids = top_ids(&searcher, "5 Main Street, Z9Z 9Z9");
        ids.sort();
        assert_eq!(ids, vec!["osm:node:1", "osm:node:2", "osm:node:3"]);
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// A stated house number in the wrong postcode used to win the strict
    /// pass and return alone, so the later passes that would have found the
    /// right street never ran.
    #[test]
    fn a_hit_contradicting_the_stated_postcode_does_not_end_the_search() {
        let temp_dir = temp_pack_path("search-postcode-rejects-strict");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        // The number exists, but only in the wrong postcode.
        let mut elsewhere =
            address_record("osm:node:1", "", "8", "High Street", None, Some("7018"));
        elsewhere.geometry = point_geometry(147.4, -42.9);
        // The right locality has the street but not the number.
        let mut street = street_record("osm:way:2", "High Street");
        street.geometry = point_geometry(146.65, -41.52);
        street.representative_point = [146.65, -41.52];
        writer.write(&elsewhere.into(), None).expect("address");
        writer.write(&street.into(), None).expect("street");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        // The 7018 address still answers, after the street.
        assert_eq!(
            top_ids(&searcher, "8 High Street, 7304"),
            vec!["osm:way:2", "osm:node:1"]
        );
        // A query that states no postcode still reaches it.
        assert!(top_ids(&searcher, "8 High Street").contains(&"osm:node:1".to_string()));
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn ranking_asks_level_then_postcode_then_source_then_text() {
        use crate::record::{
            InterpolationAddressComponents, InterpolationRange, InterpolationRecord,
        };

        let temp_dir = temp_pack_path("search-rank-order");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
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
                    source: SourceProvenance::osm(ObjectType::Way, 20),
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
        // So the Pack knows Atlantis, and does not drop it from the query.
        let mut atlantis =
            address_record("osm:node:5", "", "1", "Ocean Drive", Some("Atlantis"), None);
        atlantis.geometry = point_geometry(-60.0, 30.0);
        writer.write(&atlantis.into(), None).expect("atlantis");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

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
        let mut writer = Writer::create(&temp_dir).expect("writer");
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
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
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
        assert_eq!(estimate.id, "derived:estimate:14:osm:node:10:osm:node:11");
        let point = estimate.point.expect("point");
        assert_eq!(point.precision, PointPrecision::Estimated);
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
        assert_eq!(
            geocode(&searcher, "40 Bayview Avenue", None, None).as_deref(),
            Some("osm:way:1")
        );
        // 81 and 91 are on different Victoria Streets.
        assert_eq!(top("85 Victoria St").layer, "street");
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn geocoding_no_number_names_no_house() {
        let temp_dir = temp_pack_path("geocode-no-number");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        for (id, number) in [("osm:node:1", "1"), ("osm:node:2", "3")] {
            writer
                .write(
                    &address_record(id, "", number, "Elizabeth Street", None, None).into(),
                    None,
                )
                .expect("address");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        // Search lists the street's houses; a geocoded row would claim one
        // the query never named.
        assert_eq!(top_ids(&searcher, "Elizabeth Street")[0], "osm:node:1");
        assert_eq!(geocode(&searcher, "Elizabeth Street", None, None), None);
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn a_house_number_needs_its_street_not_a_name() {
        let temp_dir = temp_pack_path("search-number-street");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
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
                &address_record(
                    "osm:node:1",
                    "",
                    "300",
                    "St. Clair Avenue West",
                    Some("Toronto"),
                    None,
                )
                .into(),
                None,
            )
            .expect("dental address");
        writer
            .write(
                &PoiRecord {
                    alt_names: Vec::new(),
                    name: "St. Clair Spadina Dental".to_string(),
                    category: "amenity:dentist".to_string(),
                    address: Some(dental),
                    geometry: point_geometry(-79.41, 43.68),
                    location_precision: LocationPrecision::Point,
                    source: SourceProvenance::osm(ObjectType::Node, 1),
                    prominence: Default::default(),
                    wikidata: None,
                    brand: None,
                    aliases: Vec::new(),
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
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(top_ids(&searcher, "300 Spadina Ave"), vec!["osm:node:2"]);
        assert_eq!(
            top_ids(&searcher.places(), "Spadina Dental")[0],
            "osm:node:1"
        );
        assert_eq!(
            top_ids(&searcher, "300 St Clair Ave W"),
            vec!["osm:node:1"],
            "the dentist's address is its street's"
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// Tokens labelled as the parser labels them, written `word/label`.
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
        // Scored by Swiss rules, "FR" after a town is its canton, as read.
        assert_eq!(
            last(country_not_town(
                read.clone(),
                written.clone(),
                CountryCode::parse("CH")
            )),
            Label::Locality
        );
        // A word read as a region stays one ("MA" after Boston).
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
            let plan = QueryPlan::from_tagged(&tagged(text), &[], &[], |_| false).expect("plan");
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
            QueryPlan::from_tagged(&tagged(text), &[], &[], |_| false)
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
        // "3010" read as a postcode no hit is known to be in, then as the
        // house number: the next reading still runs, and its house wins.
        assert!(found(false).open());
        assert!(found(false).beaten_by(Pass::Strict));
        let house = Evidence {
            score: 1.0,
            ..found(true)
        };
        assert!(house.outweighs(found(false)));
        // A confirmed postcode ends the search at the first reading.
        assert!(!found(true).open());
        // Fewer typos outweigh a stricter pass.
        let typo = Evidence {
            edits: 1,
            ..found(true)
        };
        assert!(typo.open());
        assert!(typo.beaten_by(Pass::Lenient));
    }

    #[test]
    fn plans_numbers_units_streets_and_places_from_labelled_tokens() {
        let plan =
            |text: &str| QueryPlan::from_tagged(&tagged(text), &[], &[], |_| false).expect("plan");
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
        };

        let floor =
            plan("79/house_number wellington/street st/street w/street fl36/unit toronto/locality");
        assert_eq!(floor.house_number.as_ref().expect("number").text, "79");
        assert_eq!(subject(&floor), words(&["wellington", "street", "west"]));
        assert_eq!(floor.unit.as_deref(), Some("fl36"));
        assert!(floor.optional.is_empty());
        assert_eq!(floor.context, [part(Label::Locality, &["toronto"])]);

        // A named unit is the plan's flat, before or after the address, and
        // scores nothing.
        let unit = plan(
            "unit/unit 602/unit 670/house_number king/street st/street w/street \
             toronto/locality on/region m5v/postcode 1m9/postcode",
        );
        assert_eq!(unit.unit.as_deref(), Some("602"));
        assert!(unit.optional.is_empty());
        // So is a designator and flat the parser could not place.
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

        // A street type that leads the name keeps the number after it, and
        // an initial "st" is Saint.
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
        // A street named first, its number after it.
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
        // A number in parts that is no number keeps its first word.
        let lettered = plan("221/house_number b/house_number baker/street street/street");
        assert_eq!(lettered.house_number.expect("number").text, "221");
        assert_eq!(lettered.optional, words(&["b"]));

        // A query of places seeks the first of them, and a structured
        // field joins after the parsed parts.
        let places = QueryPlan::from_tagged(
            &tagged("seaforth/locality huron/locality east/locality"),
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
        assert!(QueryPlan::from_tagged(&[], &[], &[], |_| false).is_none());
    }

    #[test]
    fn searches_written_house_numbers_then_the_number_they_start_with() {
        let temp_dir = temp_pack_path("search-house-number-forms");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
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
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

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
    fn finds_each_address_of_a_street_group_by_its_number_and_unit() {
        let temp_dir = temp_pack_path("search-street-group");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let mut records = Vec::new();
        for (index, number) in ["1", "3", "5", "5", "5A", "7", "9;11", "13"]
            .iter()
            .enumerate()
        {
            let mut record = address_record(
                &format!("osm:node:{}", index + 1),
                "",
                number,
                "Queen Street",
                Some("Toronto"),
                None,
            );
            record.geometry = point_geometry(-79.0 + index as f64 * 0.0002, 43.0);
            if index == 3 {
                record.address.unit = Some("Unit 2".to_string());
            }
            records.push(record);
        }
        // A street of the same name in another town is a group of its own.
        records.push(address_record(
            "osm:node:20",
            "",
            "5",
            "Queen Street",
            Some("Oakville"),
            None,
        ));
        for record in records {
            writer.write(&record.into(), None).expect("address");
        }
        let manifest = writer.finish().expect("finish");
        assert_eq!(
            manifest.text_document_count, 2,
            "one document per street group"
        );
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(top_ids(&searcher, "7 Queen Street"), vec!["osm:node:6"]);
        assert_eq!(top_ids(&searcher, "11 Queen Street"), vec!["osm:node:7"]);
        assert_eq!(top_ids(&searcher, "5a Queen Street"), vec!["osm:node:5"]);
        assert_eq!(
            top_ids(&searcher, "5 Queen Street, Oakville"),
            vec!["osm:node:20"]
        );
        let toronto = top_ids(&searcher, "Unit 2 5 Queen Street, Toronto");
        assert_eq!(toronto[0], "osm:node:4", "the stated unit ranks first");
        assert_eq!(toronto.len(), 2);
        let autocomplete = searcher
            .autocomplete(&TextAutocompleteOptions {
                query: "13 queen".to_string(),
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
            vec!["osm:node:8"]
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
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let huron_east = writer
            .write(
                &Record::Place(
                    PlaceLayer::Locality,
                    PlaceRecord {
                        alt_names: Vec::new(),
                        name: "Huron East".to_string(),
                        place_type: "admin_level:8".to_string(),
                        geometry: point_geometry(-81.4, 43.5),
                        source: SourceProvenance::osm(ObjectType::Relation, 1),
                        prominence: Default::default(),
                        wikidata: None,
                    },
                ),
                None,
            )
            .expect("municipality");
        writer.set_context_names(
            [(huron_east, vec!["Huron East".to_string()])]
                .into_iter()
                .collect(),
        );
        // So the Pack knows Goderich, as a town of its own.
        writer
            .write(
                &address_record("osm:node:3", "", "1", "The Square", Some("Goderich"), None).into(),
                None,
            )
            .expect("goderich");
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
                    admin_context: AdminContextTuple::from_tiers([(
                        PlaceLayer::Locality,
                        huron_east,
                    )]),
                    flags: 0,
                }),
            )
            .expect("address");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

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

    #[test]
    fn geocode_accepts_any_area_the_record_lies_in() {
        use crate::{
            context::AdminContextTuple,
            pack::RecordContext,
            record::{PlaceLayer, PlaceRecord, Record},
        };

        let temp_dir = temp_pack_path("geocode-areas");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let mut area = |layer: PlaceLayer, name: &str, object_id: i64| {
            writer
                .write(
                    &Record::Place(
                        layer,
                        PlaceRecord {
                            alt_names: Vec::new(),
                            name: name.to_string(),
                            place_type: "boundary".to_string(),
                            geometry: point_geometry(174.7, -36.8),
                            source: SourceProvenance::osm(ObjectType::Relation, object_id),
                            prominence: Default::default(),
                            wikidata: None,
                        },
                    ),
                    None,
                )
                .expect("area")
        };
        let auckland = area(PlaceLayer::Locality, "Auckland", 1);
        let dickson = area(PlaceLayer::SubLocality, "Dickson", 2);
        writer.set_context_names(
            [
                (auckland, vec!["Auckland".to_string()]),
                (dickson, vec!["Dickson".to_string()]),
            ]
            .into_iter()
            .collect(),
        );
        // So the Pack knows Ponsonby, the suburb next to Grey Lynn.
        writer
            .write(
                &address_record(
                    "osm:node:5",
                    "",
                    "1",
                    "Jervois Road",
                    Some("Ponsonby"),
                    None,
                )
                .into(),
                None,
            )
            .expect("ponsonby");
        // Grey Lynn is a point in OSM: no boundary holds the address, but
        // the address states it.
        let mut grey_lynn = address_record("osm:node:3", "", "123", "Crummer Road", None, None);
        grey_lynn.address.areas = vec!["Grey Lynn".to_string()];
        writer
            .write(
                &grey_lynn.into(),
                Some(RecordContext {
                    admin_context: AdminContextTuple::from_tiers([(
                        PlaceLayer::Locality,
                        auckland,
                    )]),
                    flags: 0,
                }),
            )
            .expect("stated suburb");
        // An ACT suburb is ranked below the locality.
        writer
            .write(
                &address_record("osm:node:4", "", "38", "Archer Street", None, None).into(),
                Some(RecordContext {
                    admin_context: AdminContextTuple::from_tiers([(
                        PlaceLayer::SubLocality,
                        dickson,
                    )]),
                    flags: 0,
                }),
            )
            .expect("sub-locality");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        for locality in ["Grey Lynn", "Auckland"] {
            assert_eq!(
                geocode(&searcher, "123 Crummer Road", Some(locality), None).as_deref(),
                Some("osm:node:3"),
                "{locality}"
            );
        }
        assert_eq!(
            geocode(&searcher, "38 Archer Street", Some("Dickson"), None).as_deref(),
            Some("osm:node:4")
        );
        assert_eq!(
            geocode(&searcher, "123 Crummer Road", Some("Ponsonby"), None),
            None
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// One labelled reading of a query, searched as one reading of
    /// [`TextSearcher::search_variants`] searches it in the exact tier: the
    /// hits of the first pass that finds any, then the hits that contradict
    /// the query.
    fn answer(searcher: &TextSearcher, reading: &str) -> Vec<String> {
        let plan = reading_plan(searcher, reading);
        answer_plan(searcher, &plan)
            .into_iter()
            .map(|hit| hit.record.id)
            .collect()
    }

    fn reading_plan(searcher: &TextSearcher, reading: &str) -> QueryPlan {
        QueryPlan::from_tagged(&tagged(reading), &[], &[], |word| {
            searcher.describes_street(word)
        })
        .expect("plan")
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

    /// A place record, an area when `object_type` is a relation, else a
    /// point.
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

    #[test]
    fn a_stated_country_or_region_the_pack_knows_removes_a_record_elsewhere() {
        use crate::record::PlaceLayer::{Country, Locality, Region};

        let temp_dir = temp_pack_path("search-countries");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let at = [-83.2, 35.05];
        let us = place(
            &mut writer,
            Country,
            "United States",
            ObjectType::Relation,
            1,
            at,
            None,
        );
        let canada = place(
            &mut writer,
            Country,
            "Canada",
            ObjectType::Relation,
            2,
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
            &mut writer,
            Region,
            "North Carolina",
            ObjectType::Relation,
            3,
            at,
            chain(&[(Country, us)]),
        );
        let texas = place(
            &mut writer,
            Region,
            "Texas",
            ObjectType::Relation,
            4,
            at,
            chain(&[(Country, us)]),
        );
        let ontario = place(
            &mut writer,
            Region,
            "Ontario",
            ObjectType::Relation,
            5,
            at,
            chain(&[(Country, canada)]),
        );
        let in_region = |country, region| chain(&[(Country, country), (Region, region)]);
        let highlands_nc = place(
            &mut writer,
            Locality,
            "Highlands",
            ObjectType::Relation,
            6,
            at,
            in_region(us, carolina),
        );
        let highlands_tx = place(
            &mut writer,
            Locality,
            "Highlands",
            ObjectType::Relation,
            7,
            at,
            in_region(us, texas),
        );
        let addington = place(
            &mut writer,
            Locality,
            "Addington Highlands",
            ObjectType::Relation,
            8,
            at,
            in_region(canada, ontario),
        );
        writer.set_context_names(
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
            .map(|(id, name)| (id, vec![name.to_string()]))
            .collect(),
        );
        let in_town = |country, region, locality| {
            chain(&[(Country, country), (Region, region), (Locality, locality)])
        };
        for (id, region, context) in [
            ("osm:node:10", "NC", in_town(us, carolina, highlands_nc)),
            ("osm:node:11", "TX", in_town(us, texas, highlands_tx)),
            ("osm:node:12", "ON", in_town(canada, ontario, addington)),
        ] {
            let mut address = address_record(id, "", "134", "North 5th Street", None, None);
            address.address.region = Some(region.to_string());
            writer.write(&address.into(), context).expect("address");
        }
        writer
            .write(
                &street_record("osm:way:20", "Main Street").into(),
                in_town(us, texas, highlands_tx),
            )
            .expect("street");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        let street = "134/house_number north/street 5th/street street/street highlands/locality";

        // A geocoded row's region, by its name or the code its addresses
        // state, though the street states none.
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
            Some("osm:node:10")
        );
        assert_eq!(
            geocode_in("134 North 5th Street", "North Carolina").as_deref(),
            Some("osm:node:10")
        );
        assert_eq!(
            geocode_in("Main Street", "TX").as_deref(),
            Some("osm:way:20")
        );
        assert_eq!(
            geocode_in("Main Street", "Texas").as_deref(),
            Some("osm:way:20")
        );

        // "NC" lies in the United States, and in North Carolina.
        assert_eq!(
            answer(&searcher, &format!("{street} nc/region")),
            ["osm:node:10"]
        );
        let mut american = answer(
            &searcher,
            &format!("{street} united/country states/country"),
        );
        american.sort();
        assert_eq!(american, ["osm:node:10", "osm:node:11"]);
        assert_eq!(
            answer(&searcher, &format!("{street} ca/country")),
            ["osm:node:12"]
        );
        // A country or region the Pack does not know only ranks.
        assert_eq!(
            answer(&searcher, &format!("{street} atlantis/country")).len(),
            3
        );
        assert_eq!(answer(&searcher, &format!("{street} zz/region")).len(), 3);

        // An address the Pack lacks falls back to the town in the region the
        // query names, never a namesake elsewhere.
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
            ["osm:relation:6"]
        );
        assert!(fallback.iter().all(|hit| hit.fallback));
        let nowhere = [ContextPart::of(Label::Locality, Some("Atlantis")).expect("part")];
        assert!(
            searcher
                .fallback("999 Nowhere Lane", &nowhere, &Filters::default(), None, 5)
                .expect("fallback")
                .is_empty(),
            "a town the Pack does not know is no answer"
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn a_postal_town_or_a_settlement_point_nearby_agrees_with_a_record_outside_it() {
        use crate::record::PlaceLayer::{District, Locality, Neighbourhood};

        let temp_dir = temp_pack_path("search-postal-towns");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let franconia_at = [-77.15, 38.78];
        let franconia = place(
            &mut writer,
            Locality,
            "Franconia",
            ObjectType::Relation,
            1,
            franconia_at,
            None,
        );
        let springfield = place(
            &mut writer,
            Locality,
            "Springfield",
            ObjectType::Relation,
            2,
            [-89.6, 39.8],
            None,
        );
        let sefton = place(
            &mut writer,
            District,
            "Sefton",
            ObjectType::Relation,
            3,
            [-3.0, 53.46],
            None,
        );
        // Bootle is a town mapped only as a point, a kilometre from the road.
        place(
            &mut writer,
            Neighbourhood,
            "Bootle",
            ObjectType::Node,
            4,
            [-2.99, 53.455],
            None,
        );
        writer.set_context_names(
            [
                (franconia, "Franconia"),
                (springfield, "Springfield"),
                (sefton, "Sefton"),
            ]
            .into_iter()
            .map(|(id, name)| (id, vec![name.to_string()]))
            .collect(),
        );
        // Fairfax County's addresses state the postal town, not the CDP.
        writer
            .write(
                &located(
                    address_record(
                        "osm:node:10",
                        "",
                        "1",
                        "Maple Street",
                        Some("Alexandria"),
                        Some("22310"),
                    ),
                    franconia_at,
                )
                .into(),
                chain(&[(Locality, franconia)]),
            )
            .expect("alexandria address");
        writer
            .write(
                &PostcodeRecord {
                    postcode: "22310".to_string(),
                    country: None,
                    geometry: point_geometry(franconia_at[0], franconia_at[1]),
                    source: DerivedSourceProvenance::osm_address_records(1),
                }
                .into(),
                None,
            )
            .expect("postcode");
        writer
            .write(
                &street_at("osm:way:20", "Applemint Lane", franconia_at).into(),
                chain(&[(Locality, franconia)]),
            )
            .expect("applemint lane");
        writer
            .write(
                &located(
                    address_record("osm:node:11", "", "5", "Elm Street", None, None),
                    [-89.6, 39.8],
                )
                .into(),
                chain(&[(Locality, springfield)]),
            )
            .expect("springfield address");
        writer
            .write(
                &located(
                    address_record("osm:node:12", "", "7", "Stanley Road", Some("Bootle"), None),
                    [-2.98, 53.45],
                )
                .into(),
                chain(&[(District, sefton)]),
            )
            .expect("bootle address");
        writer
            .write(
                &street_at("osm:way:21", "Knowsley Road", [-3.0, 53.46]).into(),
                chain(&[(District, sefton)]),
            )
            .expect("knowsley road");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        assert_eq!(
            answer(
                &searcher,
                "applemint/street lane/street alexandria/locality 22310/postcode"
            ),
            ["osm:way:20"]
        );
        // A known town no address of the postcode states still rejects.
        assert!(
            answer(
                &searcher,
                "applemint/street lane/street springfield/locality 22310/postcode"
            )
            .is_empty()
        );
        assert!(
            answer(
                &searcher,
                "applemint/street lane/street alexandria/locality"
            )
            .is_empty()
        );
        assert_eq!(
            answer(&searcher, "knowsley/street road/street bootle/locality"),
            ["osm:way:21"]
        );
        assert!(
            answer(
                &searcher,
                "knowsley/street road/street springfield/locality"
            )
            .is_empty()
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn a_locality_run_is_read_as_the_places_named_in_it() {
        use crate::record::PlaceLayer::{Locality, Neighbourhood};

        let temp_dir = temp_pack_path("search-locality-runs");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let at = [10.6, 47.03];
        let serfaus = place(
            &mut writer,
            Locality,
            "Serfaus",
            ObjectType::Relation,
            1,
            at,
            None,
        );
        let fiss = place(
            &mut writer,
            Locality,
            "Fiss",
            ObjectType::Relation,
            2,
            at,
            None,
        );
        let ladis = place(
            &mut writer,
            Locality,
            "Ladis",
            ObjectType::Relation,
            3,
            at,
            None,
        );
        let auckland = place(
            &mut writer,
            Locality,
            "Auckland",
            ObjectType::Relation,
            4,
            at,
            None,
        );
        let mount_eden = place(
            &mut writer,
            Neighbourhood,
            "Mount Eden",
            ObjectType::Relation,
            5,
            at,
            chain(&[(Locality, auckland)]),
        );
        writer.set_context_names(
            [
                (serfaus, "Serfaus"),
                (fiss, "Fiss"),
                (ladis, "Ladis"),
                (auckland, "Auckland"),
                (mount_eden, "Mount Eden"),
            ]
            .into_iter()
            .map(|(id, name)| (id, vec![name.to_string()]))
            .collect(),
        );
        for (id, number, street, context) in [
            (
                "osm:node:10",
                "15",
                "Dorfbahnstraße",
                chain(&[(Locality, serfaus)]),
            ),
            ("osm:node:11", "3", "Dorfstraße", chain(&[(Locality, fiss)])),
            (
                "osm:node:12",
                "5",
                "Dorfstraße",
                chain(&[(Locality, ladis)]),
            ),
            (
                "osm:node:13",
                "10",
                "Eden Road",
                chain(&[(Locality, auckland), (Neighbourhood, mount_eden)]),
            ),
            (
                "osm:node:14",
                "10",
                "Eden Road",
                chain(&[(Locality, auckland)]),
            ),
        ] {
            writer
                .write(
                    &address_record(id, "", number, street, None, None).into(),
                    context,
                )
                .expect("address");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        // The village, and the ski area named after it and its neighbours.
        assert_eq!(
            answer(
                &searcher,
                "dorfbahnstrasse/street 15/house_number serfaus/locality serfaus/locality fiss/locality ladis/locality"
            ),
            ["osm:node:10"]
        );
        // A suburb and its city are one address's, and both must agree.
        assert_eq!(
            answer(
                &searcher,
                "10/house_number eden/street road/street mount/locality eden/locality auckland/locality"
            ),
            ["osm:node:13"]
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn a_street_whose_direction_or_type_contradicts_the_query_ranks_after() {
        let temp_dir = temp_pack_path("search-descriptors");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        writer
            .write(
                &address_record(
                    "osm:node:10",
                    "",
                    "5525",
                    "South US Highway 281",
                    None,
                    None,
                )
                .into(),
                None,
            )
            .expect("south address");
        writer
            .write(
                &street_record("osm:way:20", "North US Highway 281").into(),
                None,
            )
            .expect("north");
        writer
            .write(
                &street_record("osm:way:21", "South US Highway 281").into(),
                None,
            )
            .expect("south");
        writer
            .write(
                &address_record("osm:node:11", "", "3935", "16th Avenue North", None, None).into(),
                None,
            )
            .expect("avenue address");
        writer
            .write(
                &street_record("osm:way:22", "16th Street North").into(),
                None,
            )
            .expect("16th street");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        let north = answer(
            &searcher,
            "5525/house_number north/street us/street highway/street 281/street",
        );
        assert_eq!(north[0], "osm:way:20", "{north:?}");
        assert!(
            north.contains(&"osm:node:10".to_string()),
            "ranked after, never out"
        );
        assert_eq!(
            answer(
                &searcher,
                "5525/house_number south/street us/street highway/street 281/street"
            )[0],
            "osm:node:10"
        );
        assert_eq!(
            answer(
                &searcher,
                "3935/house_number 16th/street st/street n/street"
            )[0],
            "osm:way:22"
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// A place search answers first with the places whose names hold every
    /// word, the one named exactly so first, then the better known, however
    /// the address reading of the query takes the words.
    #[test]
    fn a_place_search_answers_with_the_places_so_named_the_best_known_first() {
        let temp_dir = temp_pack_path("search-places-by-name");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
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
            aliases: Vec::new(),
        };
        for record in [
            place(1, "Stedelijk Museum Breda", 0, 4.78),
            place(2, "Stedelijk Museum Amsterdam", 120, 4.88),
            place(3, "Van Gogh Museum", 140, 4.881),
            place(4, "AMS", 0, 4.882),
            place(5, "Luchthaven Schiphol", 160, 4.76),
        ] {
            writer.write(&record.into(), None).expect("place");
        }
        writer
            .write(
                &address_record(
                    "osm:node:9",
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
        writer.finish().expect("finish");
        let places = TextSearcher::open(&temp_dir).expect("searcher").places();
        assert_eq!(top_ids(&places, "Stedelijk Museum")[0], "osm:node:2");
        assert_eq!(top_ids(&places, "Van Gogh Museum")[0], "osm:node:3");
        assert_eq!(
            top_ids(&places, "AMS")[0],
            "osm:node:5",
            "an exact other name of the better known"
        );
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// Two streets whose names differ only by the type glued onto them key
    /// alike, and are two streets: the one of the type written answers.
    #[test]
    fn a_street_whose_glued_type_contradicts_the_query_ranks_after() {
        let temp_dir = temp_pack_path("search-glued-types");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        for (id, street, lon) in [
            ("osm:node:1", "Wilderinksstraat", 6.7930),
            ("osm:node:2", "Wilderinksplein", 6.7935),
        ] {
            let mut record = address_record(id, "", "2", street, Some("Hengelo"), None);
            record.geometry = point_geometry(lon, 52.265);
            writer.write(&record.into(), None).expect("address");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
        for (street, id) in [
            ("Wilderinksplein", "osm:node:2"),
            ("Wilderinksstraat", "osm:node:1"),
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
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn noise_around_an_address_leaves_the_address() {
        let temp_dir = temp_pack_path("search-noise");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        for record in [
            address_record(
                "osm:node:10",
                "",
                "1040",
                "West Harrison Street",
                None,
                Some("60607"),
            ),
            address_record(
                "osm:node:11",
                "",
                "8600",
                "North Main Street",
                None,
                Some("45415"),
            ),
            address_record("osm:node:12", "", "73", "Eibacher Hauptstraße", None, None),
            address_record("osm:node:13", "", "131", "Impasse de la Noria", None, None),
        ] {
            writer.write(&record.into(), None).expect("address");
        }
        writer
            .write(
                &PostcodeRecord {
                    postcode: "45415".to_string(),
                    country: None,
                    geometry: point_geometry(-79.0, 43.0),
                    source: DerivedSourceProvenance::osm_address_records(1),
                }
                .into(),
                None,
            )
            .expect("postcode");
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        // A suite read as a place, or written in the street, is the flat.
        let dayton = reading_plan(
            &searcher,
            "8600/house_number n/street main/street street/street suite/locality a/locality dayton/locality",
        );
        assert_eq!(dayton.unit.as_deref(), Some("a"));
        assert_eq!(dayton.context.len(), 1);
        assert_eq!(dayton.context[0].words, ["dayton"]);
        let warner = reading_plan(
            &searcher,
            "7451/house_number warner/street ave/street ste/street g/street",
        );
        assert_eq!(warner.unit.as_deref(), Some("g"));
        assert_eq!(
            warner
                .subject
                .iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>(),
            ["warner", "avenue"]
        );
        assert!(
            reading_plan(&searcher, "5/house_number unit/street road/street")
                .unit
                .is_none()
        );

        // A ZIP+4 is the ZIP its addresses state.
        let zip = answer_plan(
            &searcher,
            &reading_plan(
                &searcher,
                "8600/house_number n/street main/street street/street 45415/postcode 3457/postcode",
            ),
        );
        assert_eq!(zip[0].record.id, "osm:node:11");
        assert_eq!(zip[0].features.postcode, Agreement::Agrees);

        // A mail code after the street names no street.
        let mail = reading_plan(
            &searcher,
            "1040/house_number w/street harrison/street street/street mail/street code/street 147/street",
        );
        assert!(answer_plan(&searcher, &mail).is_empty());
        let trimmed = searcher.trimmed(&mail).expect("trimmed");
        assert_eq!(
            trimmed
                .subject
                .iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>(),
            ["w", "harrison", "street"]
        );
        assert_eq!(trimmed.optional, ["mail", "code", "147"]);
        assert_eq!(answer_plan(&searcher, &trimmed)[0].record.id, "osm:node:10");
        assert!(
            searcher.trimmed(&trimmed).is_none(),
            "the whole street is one"
        );

        // A street type glued onto the name, abbreviated.
        assert_eq!(
            answer(&searcher, "eibacher/street hauptstr/street 73/house_number"),
            ["osm:node:12"]
        );

        // A letter a wrong encoding replaced is one edit from it.
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
        assert_eq!(hits[0].record.id, "osm:node:13");
        assert_eq!(typo_distance("\u{fffd}tretat", "etretat"), Some(1));
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// The rule #36 rests on: a known locality that disagrees still rejects,
    /// whatever the reading, and a fallback answers only in it.
    #[test]
    fn a_known_town_the_record_is_not_in_rejects_it() {
        use crate::record::PlaceLayer::{Locality, Neighbourhood, Region};

        let temp_dir = temp_pack_path("search-ponsonby");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
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
            let object_id = object_id as i64 * 2;
            let region = place(
                &mut writer,
                Region,
                region,
                ObjectType::Relation,
                object_id + 1,
                at,
                None,
            );
            let town = place(
                &mut writer,
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
        // Suburbs mapped as points beside the streets.
        let in_town =
            |(region, town): (RecordId, RecordId)| chain(&[(Region, region), (Locality, town)]);
        place(
            &mut writer,
            Neighbourhood,
            "Ponsonby",
            ObjectType::Node,
            10,
            [174.745, -36.855],
            in_town(auckland),
        );
        place(
            &mut writer,
            Neighbourhood,
            "Thorndon",
            ObjectType::Node,
            11,
            [174.775, -41.28],
            in_town(wellington),
        );
        writer.set_context_names(
            [
                (auckland.0, "Auckland"),
                (auckland.1, "Auckland"),
                (wellington.0, "Wellington"),
                (wellington.1, "Wellington"),
                (christchurch.0, "Canterbury"),
                (christchurch.1, "Christchurch"),
            ]
            .into_iter()
            .map(|(id, name)| (id, vec![name.to_string()]))
            .collect(),
        );
        for (id, name, at, town) in [
            ("osm:way:20", "Ponsonby Road", [174.746, -36.855], auckland),
            ("osm:way:21", "Lambton Quay", [174.776, -41.282], wellington),
            (
                "osm:way:22",
                "Riccarton Road",
                [172.6, -43.53],
                christchurch,
            ),
        ] {
            writer
                .write(&street_at(id, name, at).into(), in_town(town))
                .expect("street");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");

        for reading in [
            "ponsonby/street road/street christchurch/locality",
            "lambton/street quay/street auckland/locality",
            // A region the Pack lists by its name places the record too.
            "lambton/street quay/street auckland/region",
            "ponsonby/street road/street canterbury/region",
        ] {
            assert!(answer(&searcher, reading).is_empty(), "{reading}");
        }
        assert_eq!(
            answer(&searcher, "ponsonby/street road/street auckland/locality"),
            ["osm:way:20"]
        );
        assert_eq!(
            answer(&searcher, "lambton/street quay/street wellington/region"),
            ["osm:way:21"]
        );
        // With nothing in the town it names, the answer is the town, marked
        // a fallback, and never the street elsewhere.
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
            ["osm:relation:6"]
        );
        assert!(fallback[0].fallback);
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn the_structured_stage_answers_as_the_text_search_does() {
        let temp_dir = temp_pack_path("search-structured-stage");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        for (id, number, street, town, postcode, at) in [
            (
                "osm:node:1",
                "10",
                "Queen Street",
                "Toronto",
                "M5H 2N2",
                [-79.38, 43.65],
            ),
            (
                "osm:node:2",
                "12",
                "Queen Street",
                "Toronto",
                "M5H 2N2",
                [-79.381, 43.65],
            ),
            (
                "osm:node:3",
                "10",
                "Queen Street",
                "Ottawa",
                "K1P 5G4",
                [-75.7, 45.42],
            ),
            (
                "osm:node:4",
                "10",
                "King Street",
                "Toronto",
                "M5V 1A1",
                [-79.39, 43.64],
            ),
            (
                "osm:node:5",
                "7",
                "Hauptstraße",
                "Berlin",
                "10115",
                [13.4, 52.52],
            ),
            (
                "osm:node:6",
                "64",
                "Hauptstraße",
                "Berlin",
                "10115",
                [13.41, 52.52],
            ),
            (
                "osm:node:7",
                "3",
                "Breite Straße",
                "Potsdam",
                "14467",
                [13.06, 52.4],
            ),
        ] {
            let record = located(
                address_record(id, "", number, street, Some(town), Some(postcode)),
                at,
            );
            writer.write(&record.into(), None).expect("address");
        }
        writer.finish().expect("finish");
        let structured = TextSearcher::open(&temp_dir).expect("searcher");
        assert!(
            structured.structured.is_some(),
            "the Pack holds its structured index"
        );
        let plain = TextSearcher::open(&temp_dir)
            .expect("searcher")
            .without_structured();
        let ids = |searcher: &TextSearcher, query: &str| {
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
                .collect::<Vec<_>>()
        };
        let mut answered = 0;
        for query in [
            "10 Queen Street Toronto",
            "10 Queen St, Ottawa",
            "10 Queen Street Toronto M5H 2N2",
            "12 Queen Street",
            "7 Hauptstr Berlin",
            "10 Quen Street Toronto",
        ] {
            let found = ids(&structured, query);
            if LAST_STRUCTURED.with(Cell::get).answered {
                answered += 1;
            }
            assert_eq!(found, ids(&plain, query), "{query}");
        }
        assert_eq!(answered, 6, "answered by the structured stage");
        // A street the stated town lacks: the one another town has is held
        // by no place word, and costs the parser's score of it, not a veto.
        let query = "64 Hauptstraße Potsdam";
        ids(&structured, query);
        let last = LAST_STRUCTURED.with(Cell::get);
        assert_eq!(last.support, 0);
        assert!(last.gap < 0.0);
        // A postcode the hit does not agree with costs it in the parser's
        // score; the hand-off models decide, and the answer is the text
        // search's.
        let query = "10 Queen Street Toronto K1P 5G4";
        let found = ids(&structured, query);
        assert_eq!(found, ids(&plain, query));
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn a_stated_postcode_outweighs_a_town_the_answer_is_not_in() {
        let temp_dir = temp_pack_path("search-postcode-over-town");
        let _ = std::fs::remove_dir_all(&temp_dir);
        let mut writer = Writer::create(&temp_dir).expect("writer");
        for (id, number, street, town, postcode, at) in [
            (
                "osm:node:1",
                "10",
                "Queen Street",
                "Toronto",
                "M5H 2N2",
                [-79.38, 43.65],
            ),
            (
                "osm:node:2",
                "12",
                "Queen Street",
                "Toronto",
                "M5H 2N2",
                [-79.381, 43.65],
            ),
            (
                "osm:node:3",
                "5",
                "Dundas Street",
                "Mississauga",
                "L5B 1M2",
                [-79.64, 43.59],
            ),
        ] {
            let record = located(
                address_record(id, "", number, street, Some(town), Some(postcode)),
                at,
            );
            writer.write(&record.into(), None).expect("address");
        }
        writer.finish().expect("finish");
        let structured = TextSearcher::open(&temp_dir).expect("searcher");
        let plain = TextSearcher::open(&temp_dir)
            .expect("searcher")
            .without_structured();
        let ids = |searcher: &TextSearcher, query: &str| {
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
                .collect::<Vec<_>>()
        };
        // The town is another and the postcode the address's own: the
        // address the postcode, street and number agree on.
        assert_eq!(
            ids(&structured, "10 Queen Street Mississauga M5H 2N2"),
            ["osm:node:1"]
        );
        assert!(LAST_STRUCTURED.with(Cell::get).answered);
        // The other town alone still holds it: the text search answers, and
        // never with the address in another town.
        let query = "10 Queen Street Mississauga";
        let found = ids(&structured, query);
        assert!(!LAST_STRUCTURED.with(Cell::get).answered);
        assert_eq!(found, ids(&plain, query));
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// A temporary Pack directory, removed when it goes out of scope, a
    /// failed assertion included.
    struct TempPack(std::path::PathBuf);

    impl TempPack {
        fn new(name: &str) -> Self {
            let path = temp_pack_path(name);
            let _ = std::fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for TempPack {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
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
            disputed: false,
            street_postcodes: Vec::new(),
            address: AddressComponents {
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
