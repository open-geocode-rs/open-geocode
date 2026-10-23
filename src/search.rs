use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    fmt, io,
    ops::{Bound, Range},
    path::Path,
    sync::Arc,
};

use crate::error::{Error, Result};
use levenshtein_automata::{DFA, Distance, SINK_STATE};
use serde::Serialize;
use tantivy::{
    DocAddress, DocId, IndexReader, Score, Searcher, SegmentReader, Term,
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
    api::{Accuracy, Address},
    confidence::{self, Agreement, Features},
    labels,
    pack::{self, Point, PointPrecision, RecordId, Source, Summary},
    parse::{
        lattice::{
            self, CountryCode, LatticeTagger, Tokens, automaton_builder,
            dictionary::{Field as NameField, designates_unit},
            dictionary_key,
        },
        types::{Label, Tagged},
    },
    record::{
        AddressComponents, AddressRecord, DERIVED_FROM_ADDRESS_RECORDS, Layer, ObjectType,
        PlaceLayer, PoiRecord, Prominence, Record, place_rank,
    },
    records::{dequantize, quantize},
    spatial_index::{self, haversine_m},
    text_index::{
        AREA_POSTCODE_FIELD, COUNTRY_CODE_FIELD, Fields, HOUSE_NUMBER_FIELD,
        INTERPOLATION_START_FIELD, MAX_LAT_FIELD, MAX_LON_FIELD, MIN_LAT_FIELD, MIN_LON_FIELD,
        NAME_WORDS_FIELD, PLACE_RANK_FIELD, POSTCODE_AREA_RADIUS_M, RANK_POSTCODE_FIELD,
        RECORD_COUNT_FIELD, STREET_TYPE_ABBREVIATIONS, address_number_terms, base_house_number,
        compact_postcode, expand_address_abbreviations, expand_final_direction, flat_number_term,
        is_numeric_token, open_text_index, street_search_text,
    },
    util::fold::{exact_key, replaced_characters, search_key},
};

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
    rank_postcode: Option<StrColumn>,
    area_postcode: Option<StrColumn>,
    country_code: Option<StrColumn>,
    min_lon: Column<i64>,
    min_lat: Column<i64>,
    max_lon: Column<i64>,
    max_lat: Column<i64>,
    name_words: Column<u64>,
    place_rank: Column<u64>,
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
pub const MAX_AUTOCOMPLETE_LIMIT: usize = 20;
const MIN_AUTOCOMPLETE_QUERY_CHARS: usize = 3;
const AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS: u32 = 1_024;
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
/// one edit. Photon's thresholds, the geocoder precedent.
const LONG_TYPO_CHARS: usize = 7;
/// The first characters of a word a typo keeps as written, as the parser's
/// fuzzy lookup keeps them. The cost lever: they keep the walk of the term
/// dictionary to one branch. One, not Photon's two for long words: a swap of
/// the second and third letters ("Mcihael") is the commonest slip, one edit
/// with transpositions counted as one, and two would rule it out.
const TYPO_PREFIX_CHARS: usize = 1;
/// The terms near one word kept per field, fewest edits then most documents
/// first: a common short word has dozens of neighbours on a planet.
const MAX_TYPO_TERMS: usize = 32;
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
        let mut columns = HashMap::new();
        for segment in reader.searcher().segment_readers() {
            let fast_fields = segment.fast_fields();
            columns.insert(
                segment.segment_id(),
                SegmentColumns {
                    record_id: fast_fields.u64("record_id")?,
                    record_count: fast_fields.u64(RECORD_COUNT_FIELD)?,
                    house_number: fast_fields.u64(HOUSE_NUMBER_FIELD)?,
                    interpolation_start: fast_fields.u64(INTERPOLATION_START_FIELD)?,
                    rank_postcode: fast_fields.str(RANK_POSTCODE_FIELD)?,
                    area_postcode: fast_fields.str(AREA_POSTCODE_FIELD)?,
                    country_code: fast_fields.str(COUNTRY_CODE_FIELD)?,
                    min_lon: fast_fields.i64(MIN_LON_FIELD)?,
                    min_lat: fast_fields.i64(MIN_LAT_FIELD)?,
                    max_lon: fast_fields.i64(MAX_LON_FIELD)?,
                    max_lat: fast_fields.i64(MAX_LAT_FIELD)?,
                    name_words: fast_fields.u64(NAME_WORDS_FIELD)?,
                    place_rank: fast_fields.u64(PLACE_RANK_FIELD)?,
                },
            );
        }
        let confidence = confidence::Model::load(&pack)?;
        Ok(Self {
            spatial,
            pack,
            reader,
            fields,
            tagger,
            columns: Arc::new(columns),
            confidence,
        })
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

    pub fn search(&self, options: TextSearchOptions) -> Result<Vec<TextSearchHit>> {
        let limit = effective_limit(options.limit);
        if limit == 0 {
            return Ok(Vec::new());
        }

        let query_text = options.query.trim();
        if query_text.is_empty() {
            return Err(Error::EmptyQuery);
        }

        // One hit past the limit, so the last one asked for has a margin.
        let filters = Filters::new(options.layer.as_deref(), &options.countries, options.bbox);
        let (_, mut hits) = self.search_variants(
            query_text,
            &[],
            filters,
            options.focus,
            limit + 1,
            Parses::All,
            |_, _, _| Ok(true),
        )?;
        if hits.is_empty() && filters.layer.is_none() {
            hits = self.fallback(query_text, &[], filters, options.focus, limit + 1)?;
        }
        self.rate(&mut hits);
        hits.truncate(limit);
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
            Filters::new(options.layer.as_deref(), &options.countries, None),
            None,
            limit.max(2),
            // A weaker parse that reads the number as a unit or a word finds
            // what the query never asked for: another house, or a POI on the
            // street. Search lists such hits; a geocoded row would give one.
            Parses::Numbered,
            |plan, postcode, hit| {
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
                let in_areas = match self.region_agreement(hit, &places)? {
                    Some(false) => false,
                    Some(true) => self
                        .area_agreement(hit, &stated_localities, postcode)?
                        .unwrap_or(stated_localities.is_empty()),
                    None => self
                        .area_agreement(hit, &stated, postcode)?
                        .unwrap_or(stated.is_empty()),
                };
                Ok(hit.record.point.is_some() && !unnumbered_address && in_postcode && in_areas)
            },
        )?;
        if hits.is_empty() && options.layer.is_none() {
            let filters = Filters::new(None, &options.countries, None);
            hits = self.fallback(address, &context, filters, None, 2)?;
        }
        self.rate(&mut hits);
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
        let Some(query) = self.build_autocomplete_query(&query_text, filters)? else {
            return Ok(Vec::new());
        };
        let tokens = autocomplete_query_tokens(&query_text);
        let number = autocomplete_number(&tokens);
        let ranking = Ranking {
            query_words: QueryWords::new(&options.query),
            number_text: number.map(str::to_string),
            focus: options.focus,
            ..Ranking::default()
        };
        let mut hits = self.ranked_hits(&query, &ranking, limit + 1, &mut |hit| {
            Ok(filters.admit(hit))
        })?;
        let words = &tokens[usize::from(number.is_some())..];
        for hit in &mut hits {
            let features = &mut hit.features;
            features.prefix = true;
            features.number = number.map(|number| states_number(&hit.record, number, None));
            // The last word may be cut short by the typing.
            let label = label_words(&hit.record);
            features.dropped_words = count(words.iter().enumerate().filter(|(index, word)| {
                !label.iter().any(|label| {
                    label == *word || (index + 1 == words.len() && label.starts_with(word.as_str()))
                })
            }));
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
        filters: Filters<'_>,
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
                    if let Some(country) = country_of(NameField::Country)
                        && !places.countries.contains(&country)
                    {
                        places.countries.push(country);
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
    fn region_agreement(&self, hit: &TextSearchHit, places: &StatedPlaces) -> Result<Option<bool>> {
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
            let chain = self
                .pack
                .boundary_context(hit.record_id)?
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
                .unwrap_or_default();
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

    /// `plan` as the typo tier `edits` searches it, `None` when the tier
    /// would search nothing the tiers before it did not. Each word the tier
    /// lets have a typo (see [`Edits::allowed`]) also matches the parser's
    /// reading of it and the index terms that near it, in each field the
    /// plan searches it in. A place the parser read as a typo of a name is
    /// that name, so the Pack knows it, and requires and checks it as any
    /// place it knows.
    fn spelt(&self, plan: &QueryPlan, edits: Edits) -> Result<Option<QueryPlan>> {
        if edits == Edits::None {
            return Ok(Some(plan.clone()));
        }
        let mut spelt = plan.clone();
        // Whether a place is read as a name the tiers before did not read it
        // as.
        let mut corrected = false;
        for part in &mut spelt.context {
            if let Some(words) = part.corrected.take()
                && words.len() == part.words.len()
                && part
                    .words
                    .iter()
                    .zip(&words)
                    .all(|(written, word)| written == word || edits.reaches(written, word))
            {
                corrected |= part.words.iter().zip(&words).any(|(written, word)| {
                    written != word && (edits == Edits::One || !Edits::One.reaches(written, word))
                });
                part.words = words;
            }
        }
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
        for word in &spelt.subject {
            words.push((&word.text, word.corrected.as_deref(), &subject_fields));
        }
        for word in &spelt.unplaced {
            words.push((&word.text, word.corrected.as_deref(), &subject_fields));
        }
        for part in spelt.context.iter().filter(|part| !part.is_postcode()) {
            for word in &part.words {
                words.push((word, None, &context_fields));
            }
        }
        // The two-edit tier only differs from the one-edit tier in the words
        // it lets have two.
        if !words.iter().any(|(word, ..)| {
            edits
                .allowed(word)
                .is_some_and(|(most, _)| most == edits.most())
        }) {
            return Ok(corrected.then_some(spelt));
        }
        let searcher = self.reader.searcher();
        let mut spellings = Spellings::default();
        for (word, correction, fields) in words {
            let Some((most, prefix)) = edits.allowed(word) else {
                continue;
            };
            let dfa = automaton_builder(most).build_dfa(word);
            let prefix = &word[..word
                .char_indices()
                .nth(prefix)
                .map_or(word.len(), |(at, _)| at)];
            let correction = correction
                .filter(|correction| {
                    correction.starts_with(prefix)
                        && matches!(dfa.eval(correction), Distance::Exact(_))
                })
                .map(str::to_string);
            for &field in fields {
                if spellings.words.contains_key(&(field, word.to_string())) {
                    continue;
                }
                let near =
                    self.near_terms(&searcher, field, word, &dfa, prefix, correction.as_deref())?;
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
        if spellings.words.is_empty() && !corrected {
            return Ok(None);
        }
        spelt.spellings = spellings;
        Ok(Some(spelt))
    }

    /// The terms of `field` that `dfa` accepts and that start with
    /// `prefix`, other than `word` and `corrected`: at most
    /// [`MAX_TYPO_TERMS`], fewest edits then most documents first. Tantivy's
    /// fuzzy query takes no prefix length, and its `new_prefix` matches the
    /// word as a prefix, which at two edits misses matches (tantivy#3200).
    /// A shared prefix leaves the edit distance as it is, so walking only
    /// the prefix's range of the dictionary with the word's automaton is a
    /// prefix length.
    fn near_terms(
        &self,
        searcher: &Searcher,
        field: Field,
        word: &str,
        dfa: &DFA,
        prefix: &str,
        corrected: Option<&str>,
    ) -> Result<Vec<String>> {
        // No term holds the byte 0xFF, which UTF-8 never uses, so every term
        // with the prefix sorts below it.
        let mut upper = prefix.as_bytes().to_vec();
        upper.push(0xFF);
        let mut found = HashMap::<Vec<u8>, (u8, u32)>::new();
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
                found.entry(term.to_vec()).or_insert((distance, 0)).1 += terms.value().doc_freq;
            }
        }
        let mut found = found.into_iter().collect::<Vec<_>>();
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
        Ok(Box::new(BooleanQuery::new(alternatives)))
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

    /// Search a plan of each of the parser's best labellings of the query in
    /// turn, best first, and return the first hits that `accept` keeps.
    /// `context` joins every plan. A plan is searched as it is and with the
    /// number its house number starts with ("407" of "407A"), and each
    /// [`Pass`] runs only when the ones before it found nothing: first the
    /// house number in the localities the plan names, then the number placed
    /// between its stated neighbours there or a street standing in for it,
    /// then anything in no other locality, with the named ones only ranking.
    /// The passes run in each typo tier in turn, exact first (see
    /// [`Edits`]): a typo tier runs only when no labelling kept anything in
    /// the tiers before it, so a query spelt as the Pack spells it searches no
    /// typo, the street standing in for a number no record states wins over
    /// the number on a street one edit away, and a labelling matched as
    /// written outranks one matched only through a typo. Within a tier the
    /// next labelling is searched only when every pass of the one before
    /// found nothing, so a reading that drops what the best one requires
    /// never outranks it. Hits found only with the localities ranking are the
    /// exception: the later labellings' stricter passes run too, and the hits
    /// on the stronger [`Evidence`] answer ("10 Ellerslie Panmure" read with
    /// Panmure the locality finds an Ellerslie Road outside it; read as one
    /// street name, Ellerslie Panmure Highway). So are hits that nothing
    /// says lie in the postcode their reading states (see
    /// [`Evidence::confirmed`]).
    ///
    /// A hit whose own postcode contradicts the one the query names is kept,
    /// after the hits that do not, but it does not end the search: a stated
    /// house number in the wrong postcode would win the strict pass alone,
    /// and the later passes that find the right street would never run ("8
    /// High Street, Deloraine TAS 7304" answered with the 7018 High Street,
    /// 163 km away). So is a hit on a street whose type or direction
    /// contradicts the one the query writes (see [`Descriptors`]): "5525
    /// North US Highway 281" is the street the query names before the 5525
    /// of South US Highway 281.
    ///
    /// When no labelling finds anything, each is searched again with its
    /// street cut to the words of it that name a street the Pack has (see
    /// [`Self::trimmed`]). Fails when the query has no words to search.
    #[allow(clippy::too_many_arguments)]
    fn search_variants(
        &self,
        query_text: &str,
        context: &[ContextPart],
        filters: Filters<'_>,
        focus: Option<[f64; 2]>,
        limit: usize,
        parses: Parses,
        mut accept: impl FnMut(&QueryPlan, &QueryPostcode, &TextSearchHit) -> Result<bool>,
    ) -> Result<(String, Vec<TextSearchHit>)> {
        let (plans, scores) = self.readings(query_text, context, parses)?;
        let Some(first_plan) = plans.first() else {
            return Err(Error::UnparsableQuery {
                query: query_text.to_string(),
            });
        };
        let first_query = first_plan.text();
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
            filters,
            focus,
            limit,
        };
        let readings = plans
            .into_iter()
            .map(|plan| (plan.text(), plan))
            .collect::<Vec<_>>();
        let mut contradicting = Contradicting::default();
        if let Some(found) =
            self.choose_in_tiers(&readings, &scores, &search, &mut accept, &mut contradicting)?
        {
            return Ok(found);
        }
        let mut trimmed = Vec::<(String, QueryPlan)>::new();
        let mut trimmed_scores = Vec::new();
        for ((_, plan), score) in readings.iter().zip(&scores) {
            if let Some(plan) = self.trimmed(plan)
                && !trimmed.iter().any(|(_, held)| *held == plan)
            {
                trimmed.push((plan.text(), plan));
                trimmed_scores.push(*score);
            }
        }
        if let Some(found) = self.choose_in_tiers(
            &trimmed,
            &trimmed_scores,
            &search,
            &mut accept,
            &mut contradicting,
        )? {
            return Ok(found);
        }
        Ok(contradicting
            .alone(limit)
            .unwrap_or((first_query, Vec::new())))
    }

    /// The plans of the parser's best labellings of `query_text`, best first,
    /// each with `context` after its own parts, and the parser's score of
    /// each. `parses` says which are kept.
    fn readings(
        &self,
        query_text: &str,
        context: &[ContextPart],
        parses: Parses,
    ) -> Result<(Vec<QueryPlan>, Vec<f32>)> {
        let mut plans = Vec::<QueryPlan>::new();
        let mut scores = Vec::<f32>::new();
        for parsed in self.tagger.parse(query_text) {
            if let Some(plan) =
                QueryPlan::from_tagged(&parsed.pieces, &parsed.corrections, context, |word| {
                    self.tagger.dictionaries().describes_street(word)
                })
                && !plans.contains(&plan)
            {
                plans.push(plan);
                scores.push(parsed.score);
            }
        }
        if parses == Parses::Numbered
            && plans
                .first()
                .is_some_and(|plan| plan.house_number.is_some())
        {
            (plans, scores) = plans
                .into_iter()
                .zip(scores)
                .filter(|(plan, _)| plan.house_number.is_some())
                .unzip();
        }
        Ok((plans, scores))
    }

    /// The hits of the reading with the strongest [`Evidence`] in the first
    /// typo tier any reading finds hits in, after them the hits whose
    /// postcode or street contradicts the query (see
    /// [`Self::search_variants`]), with the text of the reading that found
    /// them. `None` when no reading finds any, and then the contradicting
    /// hits wait in `contradicting`, after the ones there already.
    fn choose_in_tiers(
        &self,
        readings: &[(String, QueryPlan)],
        scores: &[f32],
        search: &Search<'_>,
        accept: &mut impl FnMut(&QueryPlan, &QueryPostcode, &TextSearchHit) -> Result<bool>,
        contradicting: &mut Contradicting,
    ) -> Result<Option<(String, Vec<TextSearchHit>)>> {
        // Which readings' contradicting hits count is known only once one is
        // chosen, so they wait here, in search order, with their tier and
        // reading.
        let mut contradicted = Vec::<(usize, usize, Vec<TextSearchHit>)>::new();
        for (tier, edits) in Edits::TIERS.into_iter().enumerate() {
            // Spelt only when searched: a typo tier walks the term
            // dictionaries, and most readings never run in it.
            let mut variants = vec![None::<Vec<QueryPlan>>; readings.len()];
            let chosen = choose_reading(scores, |reading, pass| {
                if variants[reading].is_none() {
                    variants[reading] = Some(match self.spelt(&readings[reading].1, edits)? {
                        Some(plan) => {
                            let base = plan.with_base_house_number();
                            std::iter::once(plan).chain(base).collect()
                        }
                        None => Vec::new(),
                    });
                }
                for plan in variants[reading].iter().flatten() {
                    if !pass.searches_anew(plan) {
                        continue;
                    }
                    let (hits, kept) = self.search_plan(
                        plan,
                        search.placed,
                        pass,
                        edits,
                        search.query_words,
                        search.filters,
                        search.focus,
                        search.limit,
                        &mut *accept,
                    )?;
                    contradicted.push((tier, reading, kept));
                    if !hits.is_empty() {
                        let confirmed = !plan.postcode_may_be_house_number()
                            || hits
                                .iter()
                                .any(|hit| hit.features.postcode != Agreement::Unknown);
                        return Ok(Some((hits, confirmed)));
                    }
                }
                Ok(None)
            })?;
            if let Some((reading, hits)) = chosen {
                // As if the readings had run in turn up to the chosen one:
                // those after it ran only to be outweighed.
                let mut kept = Contradicting::default();
                for (searched_tier, searched, hits) in contradicted {
                    if searched_tier < tier || searched <= reading {
                        kept.keep(&readings[searched].0, hits);
                    }
                }
                kept.append(std::mem::take(contradicting));
                return Ok(Some((
                    readings[reading].0.clone(),
                    kept.after(hits, search.limit),
                )));
            }
        }
        for (_, reading, hits) in contradicted {
            contradicting.keep(&readings[reading].0, hits);
        }
        Ok(None)
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
        filters: Filters<'_>,
        focus: Option<[f64; 2]>,
        limit: usize,
    ) -> Result<Vec<TextSearchHit>> {
        let (plans, _) = self.readings(query_text, context, Parses::All)?;
        let query_words = QueryWords::new(query_text);
        let no_places = HashSet::new();
        for plan in &plans {
            let known = self.known_area_words(
                plan.context
                    .iter()
                    .filter(|part| part.label == Label::Locality)
                    .flat_map(|part| &part.words),
            )?;
            for (area, layers) in plan.area_plans(|part| self.stated_names(part, &known)) {
                for layer in layers {
                    let filters = Filters {
                        layer: Some(layer.as_str()),
                        ..filters
                    };
                    for edits in Edits::TIERS {
                        let Some(spelt) = self.spelt(&area, edits)? else {
                            continue;
                        };
                        let (mut hits, contradicted) = self.search_plan(
                            &spelt,
                            &no_places,
                            Pass::Strict,
                            edits,
                            &query_words,
                            filters,
                            focus,
                            limit,
                            &mut |_, _, _| Ok(true),
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
        }
        Ok(Vec::new())
    }

    /// The hits of one pass over one plan that `accept` keeps, at most
    /// `limit`, and apart from them the kept hits whose own postcode or
    /// street contradicts the plan (see [`Self::search_variants`]). A pass
    /// that does not require the plan's localities still leaves out a hit
    /// that lies in other ones: "Ponsonby Road Christchurch" is not answered
    /// in Auckland. Every pass leaves out a hit in another country or region
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
        pass: Pass,
        edits: Edits,
        query_words: &QueryWords,
        filters: Filters<'_>,
        focus: Option<[f64; 2]>,
        limit: usize,
        accept: &mut impl FnMut(&QueryPlan, &QueryPostcode, &TextSearchHit) -> Result<bool>,
    ) -> Result<(Vec<TextSearchHit>, Vec<TextSearchHit>)> {
        let ranking = Ranking {
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
            postcode: self.query_postcode(plan)?,
            focus,
            sought: plan
                .subject
                .iter()
                .map(|word| Word {
                    text: word.text.clone(),
                    corrected: word.corrected.clone(),
                })
                .chain(plan.unplaced.iter().cloned())
                .collect(),
            edits,
        };
        let localities = plan
            .context
            .iter()
            .filter(|part| part.label == Label::Locality)
            .collect::<Vec<_>>();
        let areas = AreaWords {
            known: self.known_area_words(localities.iter().flat_map(|part| &part.words))?,
            postcodes: self.postcode_weights(&plan.area_terms())?,
            stand_ins: self.stand_in_weights(plan)?,
        };
        // Every stated locality, known or not, for what the hits rest on.
        let postcode =
            (!ranking.postcode.forms.is_empty()).then_some(ranking.postcode.exact.as_str());
        let mut localities_stated = Vec::with_capacity(localities.len());
        for part in &localities {
            localities_stated.push(self.stated_area(part, &areas.known, postcode)?);
        }
        let stated = if pass.requires_locality() {
            Vec::new()
        } else {
            localities_stated.iter().flatten().cloned().collect()
        };
        let places = self.stated_places(&plan.context);
        // A country the query names narrows retrieval as a filter would, so
        // the rank window holds its records and not a namesake abroad.
        let countries = places
            .countries
            .iter()
            .map(|country| country.as_str().to_string())
            .collect::<Vec<_>>();
        let filters = match filters.countries.is_empty() {
            true => Filters {
                countries: &countries,
                ..filters
            },
            false => filters,
        };
        let relabelled = plan
            .context
            .iter()
            .filter(|part| !matches!(part.label, Label::Locality | Label::Postcode))
            .flat_map(|part| &part.words)
            .filter(|word| placed.contains(*word))
            .cloned();
        let unplaced = match plan.subject.is_empty() {
            true => HashSet::new(),
            false => self.known_area_words(plan.unplaced.iter().map(|word| &word.text))?,
        };
        let mut loose = unplaced.into_iter().chain(relabelled).collect::<Vec<_>>();
        loose.sort();
        loose.dedup();
        let mut keep = |hit: &TextSearchHit| -> Result<bool> {
            Ok(filters.admit(hit)
                && self.region_agreement(hit, &places)? != Some(false)
                && self.area_agreement(hit, &stated, &ranking.postcode)? != Some(false)
                && self.in_loose_areas(hit, &loose)?
                && accept(plan, &ranking.postcode, hit)?)
        };
        let mut candidates = Vec::new();
        if pass == Pass::Street
            && filters
                .layer
                .map(str::trim)
                .filter(|layer| !layer.is_empty())
                .is_none_or(|layer| layer == Layer::Address.as_str())
            && let Some(estimate) = self.place_between_neighbours(plan, pass, &ranking, &areas)?
            && keep(&estimate)?
        {
            candidates.push(estimate);
        }
        let query = self.build_query(plan, pass, filters, &areas)?;
        candidates.extend(self.ranked_hits(&query, &ranking, limit, &mut keep)?);
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
                agreement(self.area_agreement(&hit, &known, &ranking.postcode)?)
            };
            let label = label_words(&hit.record);
            let features = &mut hit.features;
            features.pass = pass;
            features.edits = edits.most();
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
                .map(|_| unit_rank(&hit.record, &ranking) == 0);
            features.dropped_words = count(sought.iter().filter(|(text, corrected)| {
                !label.iter().any(|label| {
                    label == *text
                        || corrected.as_ref() == Some(label)
                        || edits.reaches(text, label)
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
    /// way it runs, each in all its forms ("n" is "north").
    fn descriptors<'w>(&self, words: impl Iterator<Item = &'w str>) -> Descriptors {
        let dictionaries = self.tagger.dictionaries();
        let mut descriptors = Descriptors::default();
        for word in words {
            let kind = if dictionaries.is_direction(word) {
                &mut descriptors.directions
            } else if dictionaries.is_street_type(word) {
                &mut descriptors.types
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
    ) -> Result<Option<bool>> {
        if stated.is_empty() {
            return Ok(None);
        }
        let areas = self.areas_of(hit.record_id)?;
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
                        settlements = Some(self.near_settlements(hit)?);
                    }
                    agrees = settlements.as_deref().is_some_and(&agrees_with);
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
    /// land nearer it than most others, with no boundary to say so. None
    /// for a hit with no point.
    fn near_settlements(&self, hit: &TextSearchHit) -> Result<Vec<String>> {
        let Some(point) = hit.record.point else {
            return Ok(Vec::new());
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
            for name in records.names(candidate.record_id)?.iter() {
                if let Some(name) = normalized_for_match(Some(name)) {
                    names.push(name);
                }
            }
        }
        Ok(names)
    }

    /// Whether a hit lies where `words`, area words a query states without
    /// requiring them, say: each word that is not a word of the hit itself
    /// (its name or label: "Wellington" of "Wellington Road" is the street)
    /// must be a word of an area it lies in, of any level, since the parser
    /// may have read a locality as a region. A hit in no named area says
    /// nothing either way.
    fn in_loose_areas(&self, hit: &TextSearchHit, words: &[String]) -> Result<bool> {
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
        let areas = self.areas_of(hit.record_id)?;
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
    fn areas_of(&self, record_id: RecordId) -> Result<RecordAreas> {
        let mut areas = RecordAreas::default();
        match self.pack.records().record(record_id)? {
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
        postcode.areas = self.postcode_areas(&postcode.exact)?;
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

    fn build_autocomplete_query(
        &self,
        query_text: &str,
        filters: Filters<'_>,
    ) -> Result<Option<Box<dyn Query>>> {
        let tokens = autocomplete_query_tokens(query_text);
        if tokens.is_empty() {
            return Ok(None);
        }

        let mut subqueries = Vec::new();

        let subject_tokens = if let Some(number) = autocomplete_number(&tokens) {
            subqueries.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.address_number, number),
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

        if let Some(layer) = filters
            .layer
            .map(str::trim)
            .filter(|layer| !layer.is_empty())
        {
            subqueries.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.layer, layer),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ));
        }
        if let Some(bbox) = filters.bbox {
            subqueries.push((Occur::Must, self.bbox_query(bbox)));
        }

        Ok(Some(Box::new(BooleanQuery::new(subqueries))))
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
        let query_words = ranking
            .query_words
            .search
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let ranked = || {
            searcher.search(
                query,
                &TopDocs::with_limit(window).tweak_score(RankKeys {
                    ranking,
                    columns: &self.columns,
                    pack: &self.pack,
                }),
            )
        };
        // When only the text score ranks, the key is the score, and the index
        // can skip the records that cannot make the window. Unless the window
        // ends inside a run of equal scores: then which of them made it is
        // arbitrary, and the rest of the key must choose ("Richmond" the town
        // among a hundred Richmond Roads).
        let top_docs =
            if ranking.numbered || !ranking.postcode.forms.is_empty() || ranking.focus.is_some() {
                ranked()?
            } else {
                let scored = searcher.search(query, &TopDocs::with_limit(window))?;
                let cut_tie = scored.len() == window
                    && window > 1
                    && scored[window - 1].0 == scored[window - 2].0;
                if cut_tie {
                    ranked()?
                } else {
                    scored
                        .into_iter()
                        .map(|(text_score, doc)| (RankKey::text(text_score), doc))
                        .collect()
                }
            };
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
            if cutoff
                .as_ref()
                .is_some_and(|cutoff| key.cmp_evidence(cutoff).is_lt())
            {
                break;
            }
            let (first, count) = self.document_records(&searcher, doc)?;
            let country = self.document_country(&searcher, doc)?;
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
        Ok(hits.into_iter().map(|((_, _, hit), _)| hit).collect())
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
                        || ranking.edits.reaches(&word.text, street)
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
        let record_id =
            columns
                .record_id
                .values_for_doc(doc.doc_id)
                .next()
                .ok_or(Error::Inconsistent(
                    "text index hit is missing fast record_id".into(),
                ))?;
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
        areas: &AreaWords,
    ) -> Result<Option<TextSearchHit>> {
        let Some(number) = plan.house_number.as_ref().map(|number| number.base) else {
            return Ok(None);
        };
        let (lower, higher) = self.neighbours(plan, pass, ranking, areas, number)?;
        if lower.is_empty() {
            return Ok(None);
        }
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
        let mut lower = Vec::new();
        let mut higher = Vec::new();
        for (_, doc) in docs {
            let columns = self.segment_columns(&searcher, doc.segment_ord)?;
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
            let (first, count) = self.document_records(&searcher, doc)?;
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
            .ok_or(Error::Inconsistent(
                "text index segment has no fast fields".into(),
            ))
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
            record: Summary {
                id: labels::estimated_address_id(number, &id(low)?, &id(high)?),
                layer: Layer::Address.as_str().to_string(),
                name: labels::address_name(&address),
                label: labels::address_label(&address),
                address: Address::from(&address),
                category: None,
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
    text_score: Score,
    /// Words of the record's own name: of names a query matches alike, the
    /// one it covers with fewest left over ("Richmond" before "Richmond
    /// Road", "Queen Street" before "Upper Queen Street"). [`TieBreak`]
    /// asks the same of the hits.
    name_words: u8,
    /// See [`place_rank`], smaller larger; [`u8::MAX`] for a record that is
    /// no place.
    place_rank: u8,
    /// How near the caller's focus the document's extent lies, larger
    /// nearer. It orders only the window the text index collects, so that
    /// of many documents the query cannot tell apart the near ones make it;
    /// the hits are ranked by [`Self::cmp_evidence`] and then by
    /// [`TieBreak`], whose own focus band follows what the query states.
    nearness: u8,
}

impl RankKey {
    /// The key of a hit when the query asks for no house number or postcode.
    fn text(text_score: Score) -> Self {
        Self {
            house_level: true,
            postcode_agreement: 0,
            stated: true,
            text_score,
            name_words: u8::MAX,
            place_rank: u8::MAX,
            nearness: 0,
        }
    }

    /// The order of what the query states, leaving out what only breaks
    /// ties.
    fn cmp_evidence(&self, other: &Self) -> std::cmp::Ordering {
        (
            self.house_level,
            self.postcode_agreement,
            self.stated,
            self.text_score,
        )
            .partial_cmp(&(
                other.house_level,
                other.postcode_agreement,
                other.stated,
                other.text_score,
            ))
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

impl PartialOrd for RankKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp_evidence(other).then_with(|| {
            (other.name_words, other.place_rank, self.nearness).cmp(&(
                self.name_words,
                self.place_rank,
                other.nearness,
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
    house_number: Column<u64>,
    interpolation_start: Column<u64>,
    name_words: Column<u64>,
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
            house_number: columns.house_number.clone(),
            interpolation_start: columns.interpolation_start.clone(),
            name_words: columns.name_words.clone(),
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
        key.place_rank = small(self.place_rank.first(doc));
        if self.numbered && self.house_number.first(doc).is_none() {
            key.house_level = self.interpolation_start.first(doc).is_some();
            key.stated = false;
        }
        if let Some(postcode) = &self.postcode {
            key.postcode_agreement = postcode.rank(doc);
        }
        if let Some(focus) = &self.focus {
            key.nearness = focus.nearness(doc);
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
    /// better known first: linked to Wikidata or Wikipedia, then named in
    /// more languages (see [`Prominence`]). Only POIs that tie on all the
    /// query states and on the caller's focus reach it. It is no measure of
    /// an address or a place, so `None` keeps it from ever ordering one
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
                        ranking.edits.reaches(query_word, word)
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
            place_rank: prior.place_rank.unwrap_or(u8::MAX),
            focus_band,
            disputed: prior.disputed,
            poi: prior.prominence.map(std::cmp::Reverse),
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
    /// A POI's, `None` for the other layers.
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
    const ALL: [Self; 3] = [Self::Strict, Self::Street, Self::Lenient];

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
            Self::Lenient => plan.context.iter().any(|part| !part.is_postcode()),
        }
    }
}

/// How strongly a reading's hits answer the query: the pass that found them,
/// then whether anything confirms the postcode the reading states, then the
/// parser's score of the reading. Compared in that order, never summed: the
/// score is an unnormalised Viterbi path score on no scale shared with
/// retrieval, so any weight joining the two would be made up. A stricter
/// pass outweighs any score; the score decides only between readings found
/// by the same pass and as confirmed.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Evidence {
    pass: Pass,
    /// A hit agrees with the postcode the reading states, or it states none.
    /// A number read as a postcode that nothing says the hits are in may be
    /// their house number: "3010 Coast Road, Barrytown" is the house 3010,
    /// not any house in a postcode 3010 no Barrytown record states.
    confirmed: bool,
    score: f32,
}

impl Evidence {
    fn outweighs(self, other: Self) -> bool {
        self.pass < other.pass
            || (self.pass == other.pass
                && (self.confirmed, self.score) > (other.confirmed, other.score))
    }

    /// Whether a later reading, which scores no higher, may still outweigh
    /// it: in a stricter pass, or in the same pass with its postcode
    /// confirmed.
    fn open(self) -> bool {
        self.pass == Pass::Lenient || !self.confirmed
    }
}

/// The reading whose hits rest on the strongest [`Evidence`], with its hits;
/// `None` when no reading finds any. `scores` are the readings' parser
/// scores, best first, and `search` runs one pass over one reading.
///
/// Readings run in turn, each through the passes until one finds hits;
/// `search` also says whether they confirm the reading's postcode (see
/// [`Evidence::confirmed`]). Confirmed hits from [`Pass::Strict`] or
/// [`Pass::Street`] end the search. After others the later readings run
/// only the passes that could outweigh them: a later reading scores no
/// higher, so only a stricter pass, or a confirmed one at the same pass,
/// could. That bounds the extra work to a few passes over each remaining
/// reading.
fn choose_reading<T>(
    scores: &[f32],
    mut search: impl FnMut(usize, Pass) -> Result<Option<(T, bool)>>,
) -> Result<Option<(usize, T)>> {
    let mut best = None::<(Evidence, usize, T)>;
    for (reading, &score) in scores.iter().enumerate() {
        if best.as_ref().is_some_and(|(held, ..)| !held.open()) {
            break;
        }
        for pass in Pass::ALL {
            if best
                .as_ref()
                .is_some_and(|(held, ..)| pass > held.pass || (pass == held.pass && held.confirmed))
            {
                break;
            }
            let Some((hits, confirmed)) = search(reading, pass)? else {
                continue;
            };
            let evidence = Evidence {
                pass,
                confirmed,
                score,
            };
            if best
                .as_ref()
                .is_none_or(|(held, ..)| evidence.outweighs(*held))
            {
                best = Some((evidence, reading, hits));
            }
            break;
        }
    }
    Ok(best.map(|(_, reading, hits)| (reading, hits)))
}

/// How many edits a query word may be from a record's in one typo tier of
/// [`TextSearcher::search_variants`]. Tantivy scores every term a fuzzy
/// query matches the same (tantivy#563), so no one query lets the spelling
/// as written outrank a typo: a tier runs only when the ones before it kept
/// nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Edits {
    #[default]
    None,
    One,
    Two,
}

impl Edits {
    const TIERS: [Self; 3] = [Self::None, Self::One, Self::Two];

    fn most(self) -> u8 {
        match self {
            Self::None => 0,
            Self::One => 1,
            Self::Two => 2,
        }
    }

    /// The edits `word` may be from a record's word in this tier, and how
    /// many of its first characters must be as written: one edit for words
    /// of [`MIN_TYPO_CHARS`] to six, two for longer ones, and
    /// [`TYPO_PREFIX_CHARS`] characters for both. `None` when it is matched
    /// only as written: it is short, or holds a digit. In a dense run of
    /// numbers every edit names another real address ("12 Main St" is one
    /// edit from "13 Main St"), so a house number, a postcode, a unit or a
    /// route number is never a typo.
    ///
    /// A character a wrong encoding replaced ("Is\u{fffd}re") is an edit
    /// whatever the word's length, so a word holding one has a typo in every
    /// tier but the exact one, and keeps no first character that is one.
    fn allowed(self, word: &str) -> Option<(u8, usize)> {
        if self == Self::None || word.chars().any(char::is_numeric) {
            return None;
        }
        if replaced_characters(word) > 0 {
            let prefix = match word.starts_with(char::REPLACEMENT_CHARACTER) {
                true => 0,
                false => TYPO_PREFIX_CHARS,
            };
            return Some((self.most(), prefix));
        }
        let chars = word.chars().count();
        if chars < MIN_TYPO_CHARS {
            None
        } else if chars < LONG_TYPO_CHARS {
            Some((1, TYPO_PREFIX_CHARS))
        } else {
            Some((self.most(), TYPO_PREFIX_CHARS))
        }
    }

    /// Whether `other` is a typo of `word` this tier allows.
    fn reaches(self, word: &str, other: &str) -> bool {
        let Some((most, prefix)) = self.allowed(word) else {
            return false;
        };
        word.chars().take(prefix).eq(other.chars().take(prefix))
            && matches!(
                automaton_builder(most).build_dfa(word).eval(other),
                Distance::Exact(_)
            )
    }
}

/// What a typo tier lets each query word match besides itself, by the field
/// of the text index it is searched in and the word.
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
    /// The words of what the query seeks, its subject and unplaced words,
    /// and the typo tier they were matched in: what an address or street
    /// must account for to rank above a POI (see
    /// [`TextSearcher::demoted`]). Empty leaves POIs where they rank.
    sought: Vec<Word>,
    edits: Edits,
}

/// Hits whose own postcode contradicts the one their query names, kept
/// across passes and readings: they answer a query only after every hit
/// that does not.
#[derive(Debug, Default)]
struct Contradicting {
    /// The reading that found the first of them.
    query: Option<String>,
    hits: Vec<TextSearchHit>,
}

impl Contradicting {
    fn keep(&mut self, query: &str, hits: Vec<TextSearchHit>) {
        for hit in hits {
            if !self.hits.iter().any(|kept| kept.record.id == hit.record.id) {
                self.query.get_or_insert_with(|| query.to_string());
                self.hits.push(hit);
            }
        }
    }

    /// The contradicting hits of `other` after these, those not held yet.
    fn append(&mut self, other: Self) {
        if let Some(query) = other.query {
            self.keep(&query, other.hits);
        }
    }

    /// `hits`, then the contradicting hits they do not hold, at most `limit`.
    fn after(self, mut hits: Vec<TextSearchHit>, limit: usize) -> Vec<TextSearchHit> {
        for hit in self.hits {
            if hits.len() >= limit {
                break;
            }
            if !hits.iter().any(|held| held.record.id == hit.record.id) {
                hits.push(hit);
            }
        }
        hits
    }

    /// The contradicting hits alone, at most `limit`, with the reading that
    /// found the first; `None` when there are none.
    fn alone(mut self, limit: usize) -> Option<(String, Vec<TextSearchHit>)> {
        self.hits.truncate(limit);
        Some((self.query?, self.hits))
    }
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
    /// What the typo tier the plan is searched in lets its words match
    /// besides themselves; empty in the exact tier (see
    /// [`TextSearcher::spelt`]).
    spellings: Spellings,
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
/// no subject, as Pelias's `minimum_should_match` of `1<-1 3<-25%` asks: one
/// may be missing of up to five, a quarter of more. Without a subject the
/// parser placed nothing, which is every word of a query in a language the
/// Pack's parser does not know.
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
        let expanded = |words: &[String]| {
            expand_address_abbreviations(&words.join(" "))
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
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
    let terms = tokens
        .iter()
        .map(|token| Term::from_field_text(field, token))
        .collect::<Vec<_>>();
    let mut query = PhrasePrefixQuery::new(terms);
    query.set_max_expansions(AUTOCOMPLETE_PREFIX_MAX_EXPANSIONS);
    Box::new(query)
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
            .search(TextSearchOptions {
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
                .search(TextSearchOptions {
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
            .autocomplete(TextAutocompleteOptions {
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
            .search(TextSearchOptions {
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
                    Pass::Strict,
                    Edits::None,
                    &QueryWords::new(&query),
                    Filters::default(),
                    None,
                    5,
                    &mut |_, _, _| Ok(true),
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
            .autocomplete(TextAutocompleteOptions {
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
            .geocode_address(AddressGeocodeOptions {
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
                .geocode_address(AddressGeocodeOptions {
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
            .geocode_address(AddressGeocodeOptions {
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
    fn pois_named_after_a_place_or_street_do_not_outrank_it() {
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
                .search(TextSearchOptions {
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
        assert_eq!(top("Walmer Road Parkette"), "osm:node:4");
        assert_eq!(top("Fire Station, Muskoka Lakes Township"), "osm:node:2");
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    /// The `ontario-poi` cases on a Pack of one street: a POI named after
    /// the street, a shop at one of its addresses and a park off it.
    #[test]
    fn addresses_rank_above_pois_unless_a_poi_confirms_the_address() {
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
        let layers = |query: &str| {
            searcher
                .search(TextSearchOptions {
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
        let first_poi = |hits: &[(String, String)]| {
            hits.iter()
                .position(|(_, layer)| layer == "poi")
                .unwrap_or(hits.len())
        };
        let last_address = |hits: &[(String, String)]| {
            hits.iter()
                .rposition(|(_, layer)| layer != "poi")
                .expect("an address or street")
        };

        // The query names a street the Pack has: every POI follows it.
        let street = layers("Walmer Road");
        assert!(last_address(&street) < first_poi(&street), "{street:?}");
        // A house number: the shop at number 4 follows the address.
        let numbered = layers("4 Walmer Road");
        assert_eq!(numbered[0].0, "osm:node:4", "{numbered:?}");
        assert!(
            last_address(&numbered) < first_poi(&numbered),
            "{numbered:?}"
        );
        // The shop's own name confirms the address it stands at.
        let confirmed = layers("Corner Store 4 Walmer Road");
        assert!(
            confirmed.iter().take(2).any(|(id, _)| id == "osm:node:11"),
            "{confirmed:?}"
        );
        // A number only a POI states is the POI.
        assert_eq!(layers("6 Walmer Road")[0].0, "osm:node:10");
        // Name-only POI queries rank as they did: the street lacks a word.
        assert_eq!(layers("Walmer Road Bakery")[0].0, "osm:node:12");
        let _ = std::fs::remove_dir_all(temp_dir);
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
        };
        for record in [
            cafe(1, Prominence::default()),
            cafe(
                2,
                Prominence {
                    linked: false,
                    languages: 3,
                },
            ),
            cafe(
                3,
                Prominence {
                    linked: true,
                    languages: 1,
                },
            ),
        ] {
            writer.write(&record.into(), None).expect("cafe");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&temp_dir).expect("searcher");
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
            &street_summary(),
            Prior {
                place_rank: None,
                prominence: Some(Prominence {
                    linked: true,
                    languages: 40,
                }),
                disputed: false,
            },
            &HashSet::new(),
            &Ranking::default(),
        );
        assert!(first < known);
    }

    fn street_summary() -> Summary {
        Summary {
            id: "osm:way:1".to_string(),
            layer: "street".to_string(),
            name: "Blue Door".to_string(),
            label: "Blue Door".to_string(),
            address: Address::default(),
            category: None,
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
            .search(TextSearchOptions {
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
            .search(TextSearchOptions {
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
            .search(TextSearchOptions {
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
            .search(TextSearchOptions {
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
            .autocomplete(TextAutocompleteOptions {
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
            .autocomplete(TextAutocompleteOptions {
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
            .autocomplete(TextAutocompleteOptions {
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
            .autocomplete(TextAutocompleteOptions {
                query: "nw16".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("postcode autocomplete");
        let number_hits = searcher
            .autocomplete(TextAutocompleteOptions {
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
            .autocomplete(TextAutocompleteOptions {
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
                .autocomplete(TextAutocompleteOptions {
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
                .autocomplete(TextAutocompleteOptions {
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
                .autocomplete(TextAutocompleteOptions {
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
            .search(TextSearchOptions {
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

        assert!(Edits::One.reaches("mcihael", "michael"));
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
            .search(TextSearchOptions {
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
                .search(TextSearchOptions {
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
                &PoiRecord {
                    alt_names: Vec::new(),
                    name: "St. Clair Spadina Dental".to_string(),
                    category: "amenity:dentist".to_string(),
                    address: Some(dental),
                    geometry: point_geometry(-79.41, 43.68),
                    location_precision: LocationPrecision::Point,
                    source: SourceProvenance::osm(ObjectType::Node, 1),
                    prominence: Default::default(),
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
        assert_eq!(top_ids(&searcher, "Spadina Dental")[0], "osm:node:1");
        assert_eq!(
            top_ids(&searcher, "300 St Clair Ave W"),
            vec!["osm:node:1"],
            "a POI's address is its street's"
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
        let mut searched = Vec::new();
        let mut search = |confirms: bool| {
            searched.clear();
            choose_reading(&[2.0, 1.0], |reading, pass| {
                searched.push((reading, pass));
                Ok(Some((reading, reading == 1 || confirms)))
            })
            .expect("chosen")
            .map(|(reading, _)| reading)
        };
        // "3010" read as a postcode no hit is known to be in, then as the
        // house number: the house wins.
        assert_eq!(search(false), Some(1));
        // A confirmed postcode ends the search at the first reading.
        assert_eq!(search(true), Some(0));
        assert_eq!(searched, [(0, Pass::Strict)]);
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
            .autocomplete(TextAutocompleteOptions {
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
        for pass in Pass::ALL {
            if !pass.searches_anew(plan) {
                continue;
            }
            let (hits, kept) = searcher
                .search_plan(
                    plan,
                    &HashSet::new(),
                    pass,
                    Edits::None,
                    &QueryWords::new(&plan.text()),
                    Filters::default(),
                    None,
                    5,
                    &mut |_, _, _| Ok(true),
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
                .geocode_address(AddressGeocodeOptions {
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
            .fallback("999 Nowhere Lane", &context, Filters::default(), None, 5)
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
                .fallback("999 Nowhere Lane", &nowhere, Filters::default(), None, 5)
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
        let spelt = searcher
            .spelt(&mangled, Edits::One)
            .expect("spelt")
            .expect("a typo tier");
        let (hits, _) = searcher
            .search_plan(
                &spelt,
                &HashSet::new(),
                Pass::Strict,
                Edits::One,
                &QueryWords::new("131 impasse de la nor\u{fffd}a"),
                Filters::default(),
                None,
                5,
                &mut |_, _, _| Ok(true),
            )
            .expect("search");
        assert_eq!(hits[0].record.id, "osm:node:13");
        assert!(Edits::One.reaches("\u{fffd}tretat", "etretat"));
        assert!(!Edits::None.reaches("ari\u{fffd}ge", "ariege"));
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
                Filters::default(),
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
