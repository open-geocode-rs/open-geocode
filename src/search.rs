use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    fmt, io,
    ops::{Bound, Range},
    path::Path,
    sync::Arc,
};

use crate::error::{Error, Result};
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
    api::Address,
    labels,
    pack::{self, Point, PointPrecision, RecordId, Source, Summary},
    parse::{
        lattice::{LatticeTagger, Tokens},
        types::{Label, Tagged},
    },
    record::{
        AddressComponents, AddressRecord, DERIVED_FROM_ADDRESS_RECORDS, Layer, ObjectType,
        PlaceLayer, PoiRecord, Record,
    },
    spatial_index::{self, haversine_m},
    text_index::{
        Fields, HOUSE_NUMBER_FIELD, INTERPOLATION_START_FIELD, POSTCODE_AREA_RADIUS_M,
        RANK_POSTCODE_FIELD, RECORD_COUNT_FIELD, address_number_terms, base_house_number,
        compact_postcode, expand_address_abbreviations, expand_final_direction, flat_number_term,
        is_numeric_token, open_text_index, street_search_text,
    },
    util::fold::{exact_key, search_key},
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSearchOptions {
    pub query: String,
    pub limit: usize,
    pub layer: Option<String>,
    /// Return only records in these countries, by ISO 3166-1 alpha-2 code.
    /// Empty means every country the Pack covers. A Pack of one country
    /// needs none; one covering several is narrowed with it.
    pub countries: Vec<String>,
}

/// What narrows a search to part of the Pack, apart from the query itself.
/// Both are exact terms on indexed fields, so they cost a term lookup and
/// leave scoring alone: the index and its collection statistics stay whole,
/// which is what keeps scores comparable across the countries in a Pack.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Filters<'a> {
    pub layer: Option<&'a str>,
    pub countries: &'a [String],
}

impl<'a> Filters<'a> {
    fn new(layer: Option<&'a str>, countries: &'a [String]) -> Self {
        Self { layer, countries }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextAutocompleteOptions {
    pub query: String,
    pub limit: usize,
    pub layer: Option<String>,
    /// Return only records in these countries; see [`TextSearchOptions`].
    pub countries: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TextSearchHit {
    pub record_id: RecordId,
    pub score: Score,
    pub record: Summary,
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
/// For a hit that states no postcode, the batch check compares the postcode
/// records within [`POSTCODE_AREA_RADIUS_M`] with the requested postcode on
/// their leading characters (a Canadian FSA, a US sectional centre, the start
/// of a UK outward code).
const POSTCODE_AREA_CHARS: usize = 3;
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
/// where it is.
const NAME_BOOST: Score = 5.5;
const NUMBER_BOOST: Score = 2.0;
const POSTCODE_BOOST: Score = 2.0;
/// A word that may name an area, where the query names none, counts for less
/// there than in what a record is: "7 Lynn Place" is the street, not a place
/// in the New Lynn suburb. Close scores between the two otherwise rank on
/// how common each word happens to be in each field.
const AREA_STANDS_IN_BOOST: Score = 0.5;
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
                },
            );
        }
        Ok(Self {
            spatial,
            pack,
            reader,
            fields,
            tagger,
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
            return Err(Error::EmptyQuery);
        }

        // A hit that contradicts a postcode the query states is not an answer
        // to it, so it does not end the pass loop. Without this a stated house
        // number in the wrong postcode wins the strict pass and returns alone,
        // and the later passes that would have found the right street never
        // run: "8 High Street, Deloraine TAS 7304" answered with the 7018 High
        // Street, 163 km away. Only a stated postcode can reject, so queries
        // that name none are unaffected.
        let (_, hits) = self.search_variants(
            query_text,
            &[],
            Filters::new(options.layer.as_deref(), &options.countries),
            limit,
            Parses::All,
            |_, postcode, hit| self.hit_in_postcode(hit, postcode),
        )?;
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
        let (query, hits) = self.search_variants(
            address,
            &context,
            Filters::new(options.layer.as_deref(), &options.countries),
            limit,
            // A weaker parse that reads the number as a unit or a word finds
            // what the query never asked for: another house, or a POI on the
            // street. Search lists such hits; a geocoded row would give one.
            Parses::Numbered,
            |plan, postcode, hit| {
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
                Ok(hit.record.point.is_some()
                    && !unnumbered_address
                    && in_postcode
                    && self.hit_matches_address_context(hit, &options)?)
            },
        )?;
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

        let Some(query) = self.build_autocomplete_query(
            &query_text,
            Filters::new(options.layer.as_deref(), &options.countries),
        )?
        else {
            return Ok(Vec::new());
        };
        let tokens = autocomplete_query_tokens(&query_text);
        let ranking = Ranking {
            query_words: QueryWords::new(&options.query),
            number_text: autocomplete_number(&tokens).map(str::to_string),
            ..Ranking::default()
        };
        self.ranked_hits(&query, &ranking, limit)
    }

    /// The house number, then what a record is and where. See
    /// [`Self::house_number_query`] for what `pass` lets stand in for the
    /// number.
    fn build_query(&self, plan: &QueryPlan, pass: Pass, filters: Filters<'_>) -> Box<dyn Query> {
        let mut clauses = Vec::new();
        if let Some(number) = &plan.house_number {
            clauses.push((
                Occur::Must,
                self.house_number_query(number, plan.unit.as_deref(), pass),
            ));
        }
        clauses.extend(self.subject_clauses(plan, pass));
        for word in &plan.optional {
            clauses.push((Occur::Should, self.word_query(word)));
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
        Box::new(BooleanQuery::new(clauses))
    }

    /// Every identifying word is required: of a record's street when the
    /// query asks for a house number on it, else of what the record is. A
    /// locality part is required of where it is unless `pass` is lenient. A
    /// region or country part only ranks, since few records state the "ON"
    /// a query may write, and so does a postcode part, since a record's tags
    /// often state a different postcode than the one written.
    fn subject_clauses(&self, plan: &QueryPlan, pass: Pass) -> Vec<(Occur, Box<dyn Query>)> {
        let mut clauses = Vec::new();
        // A query the parser found no places in may still name one
        // ("Riverdale Farm Toronto" when the Pack does not know Toronto), so
        // any of its words may name an area. So may one whose localities
        // matched nothing.
        let anywhere = plan.context.is_empty() || pass == Pass::Lenient;
        for word in &plan.required {
            let what = if plan.house_number.is_some() {
                self.street_word_query(word)
            } else {
                self.word_query(word)
            };
            let query = if anywhere {
                Box::new(DisjunctionMaxQuery::new(vec![
                    what,
                    Box::new(BoostQuery::new(self.area_query(word), AREA_STANDS_IN_BOOST)),
                ]))
            } else {
                what
            };
            clauses.push((Occur::Must, query));
        }
        for part in &plan.context {
            let clause = if pass.requires_locality() && part.label == Label::Locality {
                (Occur::Must, self.locality_part_query(&part.words))
            } else {
                (Occur::Should, self.context_part_query(&part.words))
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
    fn house_number_query(
        &self,
        number: &HouseNumber,
        unit: Option<&str>,
        pass: Pass,
    ) -> Box<dyn Query> {
        // A named unit also finds its flat as written with it ("29/181"),
        // which the build keeps where the street could not say which side
        // is the unit.
        let mut alternatives: Vec<(Occur, Box<dyn Query>)> = std::iter::once(number.text.clone())
            .chain(unit.map(|unit| number.flat_text(unit)))
            .map(|text| {
                let stated = TermQuery::new(
                    Term::from_field_text(self.fields.address_number, &text),
                    IndexRecordOption::Basic,
                );
                (
                    Occur::Should,
                    Box::new(BoostQuery::new(Box::new(stated), NUMBER_BOOST)) as Box<dyn Query>,
                )
            })
            .collect();
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
    /// then anything, with the localities only ranking. The next labelling is
    /// searched only when every pass of the one before found nothing, so a
    /// reading that drops what the best one requires never outranks it.
    /// Fails when the query has no words to search.
    fn search_variants(
        &self,
        query_text: &str,
        context: &[ContextPart],
        filters: Filters<'_>,
        limit: usize,
        parses: Parses,
        mut accept: impl FnMut(&QueryPlan, &QueryPostcode, &TextSearchHit) -> Result<bool>,
    ) -> Result<(String, Vec<TextSearchHit>)> {
        let mut plans = Vec::<QueryPlan>::new();
        for parsed in self.tagger.parse(query_text) {
            if let Some(plan) = QueryPlan::from_tagged(&parsed.pieces, context)
                && !plans.contains(&plan)
            {
                plans.push(plan);
            }
        }
        if parses == Parses::Numbered
            && plans
                .first()
                .is_some_and(|plan| plan.house_number.is_some())
        {
            plans.retain(|plan| plan.house_number.is_some());
        }
        let Some(first_plan) = plans.first() else {
            return Err(Error::UnparsableQuery {
                query: query_text.to_string(),
            });
        };
        let first_query = first_plan.text();
        let mut query_words = QueryWords::new(query_text);
        query_words
            .search
            .extend(context.iter().flat_map(|part| part.words.iter().cloned()));
        for plan in plans {
            let text = plan.text();
            let base = plan.with_base_house_number();
            let variants = std::iter::once(plan).chain(base).collect::<Vec<_>>();
            for pass in [Pass::Strict, Pass::Street, Pass::Lenient] {
                for plan in &variants {
                    if !pass.searches_anew(plan) {
                        continue;
                    }
                    let hits =
                        self.search_plan(plan, pass, &query_words, filters, limit, &mut accept)?;
                    if !hits.is_empty() {
                        return Ok((text, hits));
                    }
                }
            }
        }
        Ok((first_query, Vec::new()))
    }

    /// The hits of one pass over one plan that `accept` keeps, at most
    /// `limit`.
    fn search_plan(
        &self,
        plan: &QueryPlan,
        pass: Pass,
        query_words: &QueryWords,
        filters: Filters<'_>,
        limit: usize,
        accept: &mut impl FnMut(&QueryPlan, &QueryPostcode, &TextSearchHit) -> Result<bool>,
    ) -> Result<Vec<TextSearchHit>> {
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
        };
        let mut hits = Vec::new();
        if pass == Pass::Street
            && filters
                .layer
                .map(str::trim)
                .filter(|layer| !layer.is_empty())
                .is_none_or(|layer| layer == Layer::Address.as_str())
            && let Some(estimate) = self.place_between_neighbours(plan, pass, &ranking)?
            && accept(plan, &ranking.postcode, &estimate)?
        {
            hits.push(estimate);
        }
        let query = self.build_query(plan, pass, filters);
        for hit in self.ranked_hits(&query, &ranking, limit)? {
            if accept(plan, &ranking.postcode, &hit)? {
                hits.push(hit);
            }
        }
        hits.truncate(limit);
        Ok(hits)
    }

    fn hit_matches_address_context(
        &self,
        hit: &TextSearchHit,
        options: &AddressGeocodeOptions,
    ) -> Result<bool> {
        let desired_region = normalized_for_match(options.region.as_deref());
        let desired_locality = normalized_for_match(options.locality.as_deref());
        if desired_region.is_none() && desired_locality.is_none() {
            return Ok(true);
        }
        let areas = self.areas_of(hit.record_id)?;
        Ok(
            desired_region.is_none_or(|region| areas.regions.contains(&region))
                && desired_locality.is_none_or(|locality| areas.localities.contains(&locality)),
        )
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

    /// Whether a hit can be in the postcode the query names. A hit that
    /// states a postcode must agree with it; either may be a prefix of the
    /// other, so "M5V" matches "M5V 1A1". A hit without one is judged by the
    /// postcode records around it.
    fn hit_in_postcode(&self, hit: &TextSearchHit, postcode: &QueryPostcode) -> Result<bool> {
        Ok(self.postcode_agreement(hit, postcode)?.unwrap_or(true))
    }

    /// Whether a hit is in the postcode the query names, `None` when nothing
    /// says either way: the query names none, or the hit states none and no
    /// postcode data lies around it.
    fn postcode_agreement(
        &self,
        hit: &TextSearchHit,
        postcode: &QueryPostcode,
    ) -> Result<Option<bool>> {
        if postcode.forms.is_empty() {
            return Ok(None);
        }
        if let Some(stated) = self.pack.records().postcode(hit.record_id)? {
            return Ok(compact_postcode(&stated).map(|stated| {
                postcode
                    .forms
                    .iter()
                    .any(|form| stated.starts_with(form.as_str()) || form.starts_with(&stated))
            }));
        }
        let Some(point) = hit.record.point else {
            return Ok(None);
        };
        let mut agreement = Some(false);
        for form in &postcode.forms {
            match self.near_postcode_area(point.lon, point.lat, form)? {
                Some(true) => return Ok(Some(true)),
                Some(false) => {}
                None => agreement = None,
            }
        }
        Ok(agreement)
    }

    /// Whether the postcode records around a point share the leading
    /// characters of `postcode`, `None` with no postcode data nearby.
    fn near_postcode_area(&self, lon: f64, lat: f64, postcode: &str) -> Result<Option<bool>> {
        let area: String = postcode.chars().take(POSTCODE_AREA_CHARS).collect();
        let in_area = self.spatial.any_context_point(
            lon,
            lat,
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
        Ok(in_area)
    }

    /// The postcode `plan` names, with the points of the Pack's records of
    /// it, so hits can be ranked and judged by how far they lie from it.
    fn query_postcode(&self, plan: &QueryPlan) -> Result<QueryPostcode> {
        let mut postcode = plan.postcode();
        if postcode.forms.is_empty() {
            return Ok(postcode);
        }
        let terms = unique_strings(
            std::iter::once(postcode.exact.clone())
                .chain(postcode.forms.iter().cloned())
                .collect(),
        );
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
        for (_, doc) in searcher.search(&query, &TopDocs::with_limit(POSTCODE_RECORD_LIMIT))? {
            let (record_id, _) = self.document_records(&searcher, doc)?;
            let (_, lon, lat) = self.pack.records().point(record_id)?;
            postcode.points.push([lon, lat]);
        }
        Ok(postcode)
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
            .search
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
                    pack: &self.pack,
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
            let (first, count) = self.document_records(&searcher, doc)?;
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
                hits.push((
                    key.clone(),
                    TieBreak::new(&record, &query_words, ranking),
                    TextSearchHit {
                        record_id,
                        score: key.text_score,
                        record,
                    },
                ));
            }
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
    ) -> Result<Option<TextSearchHit>> {
        let Some(number) = plan.house_number.as_ref().map(|number| number.base) else {
            return Ok(None);
        };
        let (lower, higher) = self.neighbours(plan, pass, ranking, number)?;
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
        number: u32,
    ) -> Result<(Vec<Neighbour>, Vec<Neighbour>)> {
        let searcher = self.reader.searcher();
        let mut clauses = self.subject_clauses(plan, pass);
        clauses.push((Occur::Must, self.layer_query(Layer::Address.as_str())));
        let docs = searcher.search(
            &BooleanQuery::new(clauses),
            &TopDocs::with_limit(NEIGHBOUR_DOCUMENTS),
        )?;
        let mut lower = Vec::new();
        let mut higher = Vec::new();
        for (_, doc) in docs {
            let columns = self.segment_columns(&searcher, doc.segment_ord)?;
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
    pack: &'a Arc<pack::Reader>,
}

struct SegmentRankKeys {
    numbered: bool,
    house_number: Column<u64>,
    interpolation_start: Column<u64>,
    postcode: Option<(StrColumn, Vec<Vec<Range<u64>>>)>,
    nearness: Option<PostcodeNearness>,
}

/// Ranks a matched record by its distance from the records of the postcode a
/// query names, when the record does not have that postcode itself. The first
/// address of a street group stands for all of it: they share a street, their
/// areas and the postcode nearest them.
struct PostcodeNearness {
    record_id: Column<u64>,
    pack: Arc<pack::Reader>,
    postcode: QueryPostcode,
    /// The agreement of a record whose postcode is the named one in full.
    full_agreement: u8,
}

impl PostcodeNearness {
    fn fit(&self, doc: DocId, agreement: u8) -> u8 {
        if agreement >= self.full_agreement {
            return u8::MAX;
        }
        // Beyond the last band (a code shared with another country) only
        // the agreement ranks, as without the postcode's records.
        let Some(distance) = self
            .record_id
            .first(doc)
            .and_then(|record_id| self.pack.records().point(record_id).ok())
            .and_then(|(_, lon, lat)| self.postcode.distance_m(lon, lat))
            .filter(|distance| {
                *distance <= POSTCODE_NEARNESS_BANDS_M[POSTCODE_NEARNESS_BANDS_M.len() - 1]
            })
        else {
            return agreement;
        };
        let band = POSTCODE_NEARNESS_BANDS_M
            .iter()
            .filter(|band| distance > **band)
            .count();
        u8::MAX - 1 - band as u8
    }
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
        let nearness = (!self.ranking.postcode.points.is_empty()).then(|| PostcodeNearness {
            record_id: columns.record_id.clone(),
            pack: Arc::clone(self.pack),
            postcode: self.ranking.postcode.clone(),
            full_agreement: self
                .ranking
                .postcode
                .forms
                .iter()
                .map(|form| form.chars().count())
                .min()
                .unwrap_or(0)
                .min(u8::MAX.into()) as u8,
        });
        Ok(SegmentRankKeys {
            numbered: self.ranking.numbered,
            house_number: columns.house_number.clone(),
            interpolation_start: columns.interpolation_start.clone(),
            postcode,
            nearness,
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
        if let Some(nearness) = &self.nearness {
            key.postcode_agreement = nearness.fit(doc, key.postcode_agreement);
        }
        key
    }
}

/// Secondary order for hits with equal ranks.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TieBreak {
    /// The flat the query names, then the building, then its other flats:
    /// "19 Eros Place" is the house, not its flat 1, though a richer house
    /// label would lose on the words below. The unit is the one the plan
    /// read, never a word of the house number ("7 Vialou Street" is no
    /// unit 7).
    unit: u8,
    /// Words of the record's own label the query did not ask for, as both
    /// were written: "342 Albert Street" beats "342 Prince Albert Street" for
    /// the query "342 albert street".
    extra_label_tokens: usize,
    /// Query words the label lacks. The addresses of a street group share
    /// one document and so one score, though only some state the unit the
    /// query names.
    missing_query_words: usize,
    /// Query words the label does not spell as the query does: a query that
    /// writes "Taupō" asks for Taupō before a "Taupo" its search key also
    /// finds, and one that writes "Taupo" for Taupo first.
    inexact_query_words: usize,
    /// Exact points before centroids before estimates before line midpoints.
    precision: u8,
    /// Nodes before ways before relations, then the lower id.
    source: (u8, i64),
    id: String,
}

impl TieBreak {
    fn new(record: &Summary, query_words: &HashSet<&str>, ranking: &Ranking) -> Self {
        let label = search_key(&record.label).unwrap_or_default();
        let label_words = label.split_whitespace().collect::<HashSet<_>>();
        let extra_label_tokens = label
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
        let unit = match (ranking.unit.as_deref(), stated.as_deref()) {
            _ if as_written => 0,
            (Some(sought), Some(stated))
                if stated.split_whitespace().any(|word| word == sought) =>
            {
                0
            }
            (Some(_), None) | (None, None) => 1,
            (_, Some(_)) => 2,
        };
        Self {
            unit,
            extra_label_tokens,
            missing_query_words,
            inexact_query_words,
            precision,
            source: (source, record.source.object_id.unwrap_or_default()),
            id: record.id.clone(),
        }
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

/// The retrieval passes of [`TextSearcher::search_variants`], in order.
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
            Self::Lenient => plan.context.iter().any(|part| !part.is_postcode()),
        }
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

/// The postcode a query names, to rank records by how much of it their own
/// postcode shares from its start: "M5C 2A1" agrees with "M5C 1B2" on three
/// characters, with "M4C 1B2" on one. Longer agreement ranks first, whatever
/// the country's postcode format.
#[derive(Debug, Clone, Default)]
struct QueryPostcode {
    /// Compacted forms of the postcode part: all of it, and from its first
    /// word with a digit when other words lead ("on m5c 2a1" is "m5c2a1").
    forms: Vec<String>,
    /// The part as the postcode field indexes it ("m5v 1a1").
    exact: String,
    /// Points of the Pack's records of the postcode, found by
    /// [`TextSearcher::query_postcode`].
    points: Vec<[f64; 2]>,
}

impl QueryPostcode {
    /// How far a point lies from the nearest record of the postcode, when the
    /// Pack has one.
    fn distance_m(&self, lon: f64, lat: f64) -> Option<f64> {
        self.points
            .iter()
            .map(|[point_lon, point_lat]| haversine_m(lon, lat, *point_lon, *point_lat))
            .min_by(f64::total_cmp)
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
/// parser (see [`crate::parse::lattice`]). Street and other words name what
/// is sought and are required; locality, region, postcode and country words
/// place it; unit words only rank (see [`TextSearcher::build_query`]).
#[derive(Debug, Clone, Default, PartialEq)]
struct QueryPlan {
    /// The house number the parser read ("670", "221b", "993_5"), when other
    /// required words name the street it is on.
    house_number: Option<HouseNumber>,
    /// The unit the query names by a number ("29" of "unit 29" or of
    /// "29/181" read as a unit), to find and rank the flat.
    unit: Option<String>,
    required: Vec<String>,
    optional: Vec<String>,
    /// Each run of locality, region, postcode or country words, in order.
    context: Vec<ContextPart>,
}

/// A run of query words that says where the sought record is.
#[derive(Debug, Clone, PartialEq)]
struct ContextPart {
    /// Locality, region, postcode or country.
    label: Label,
    words: Vec<String>,
}

impl ContextPart {
    /// The part of a structured field ("Toronto", "M5V 1M9"), `None` when
    /// it has no words.
    fn of(label: Label, value: Option<&str>) -> Option<Self> {
        let words = Tokens::new(value?).words;
        (!words.is_empty()).then_some(Self { label, words })
    }

    fn is_postcode(&self) -> bool {
        self.label == Label::Postcode
    }
}

impl QueryPlan {
    /// Plan one labelling of a query's tokens, with `context` (the
    /// structured fields of a geocoded row) after the parts the labelling
    /// names. `None` when the query has no words.
    fn from_tagged(tokens: &[Tagged], context: &[ContextPart]) -> Option<Self> {
        let mut plan = Self::default();
        let mut runs = Vec::<(Label, Vec<String>)>::new();
        for tagged in tokens {
            match runs.last_mut() {
                Some((label, words)) if *label == tagged.label => words.push(tagged.token.clone()),
                _ => runs.push((tagged.label, vec![tagged.token.clone()])),
            }
        }
        for (label, words) in runs {
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
                        plan.required.extend(words);
                    }
                }
                Label::Unit => {
                    if let Some(unit) = words
                        .iter()
                        .rev()
                        .find(|word| word.chars().any(|character| character.is_ascii_digit()))
                    {
                        plan.unit = Some(unit.clone());
                    }
                    plan.optional.extend(words);
                }
                Label::HouseNumber => plan.optional.extend(words),
                Label::Street | Label::Other => plan.required.extend(
                    expand_address_abbreviations(&words.join(" "))
                        .split_whitespace()
                        .map(str::to_string),
                ),
                Label::Locality | Label::Region | Label::Postcode | Label::Country => {
                    plan.context.push(ContextPart { label, words });
                }
            }
        }
        plan.context.extend(context.iter().cloned());
        // A query of places only ("Toronto", "Seaforth, Huron East") seeks
        // the first; a house number alone is sought as a word.
        if plan.required.is_empty() {
            if !plan.context.is_empty() {
                plan.required = plan.context.remove(0).words;
            } else if let Some(number) = plan.house_number.take() {
                plan.required = number.text.split_whitespace().map(str::to_string).collect();
            } else {
                plan.required = std::mem::take(&mut plan.optional);
            }
        }
        if plan.required.is_empty() {
            return None;
        }
        if plan.house_number.is_some() {
            expand_final_direction(&mut plan.required);
        }
        Some(plan)
    }

    /// The plan as text: the house number, the words it seeks, then each
    /// context part after a comma.
    fn text(&self) -> String {
        let subject = self
            .house_number
            .iter()
            .map(|number| number.text.as_str())
            .chain(self.required.iter().map(String::as_str))
            .chain(self.optional.iter().map(String::as_str))
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

    /// The postcode the last postcode part names. A postcode given apart
    /// from the query follows the parts parsed from it, so it wins over a
    /// house number read as a postcode ("6011 highland lakes road").
    fn postcode(&self) -> QueryPostcode {
        let Some(part) = self.context.iter().rev().find(|part| part.is_postcode()) else {
            return QueryPostcode::default();
        };
        let words = &part.words;
        let first_digit = words
            .iter()
            .position(|word| word.chars().any(|character| character.is_ascii_digit()))
            .unwrap_or_default();
        QueryPostcode {
            forms: unique_strings(vec![words.concat(), words[first_digit..].concat()]),
            exact: words.join(" "),
            points: Vec::new(),
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
            })
            .expect("postcode autocomplete");
        let number_hits = searcher
            .autocomplete(TextAutocompleteOptions {
                query: "221".to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
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
                })
                .expect("two character autocomplete")
                .is_empty()
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    fn top_ids(searcher: &TextSearcher, query: &str) -> Vec<String> {
        searcher
            .search(TextSearchOptions {
                query: query.to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
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
        // contradicts the query outright and is left out: it is not an answer
        // to it, however little else matches.
        assert_eq!(
            top_ids(&searcher, "5 Main Street, A1A 9Z9"),
            vec!["osm:node:2", "osm:node:3"]
        );
        assert_eq!(
            top_ids(&searcher, "5 Main Street, Toronto ON B2B 2B2")[0],
            "osm:node:1",
            "the postcode starts at its first word with a digit"
        );
        // A postcode nobody shares prefers no one: the closest labels first,
        // and again without the record whose own postcode disagrees.
        assert_eq!(
            top_ids(&searcher, "5 Main Street, Z9Z 9Z9"),
            vec!["osm:node:2", "osm:node:3"]
        );
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

        let ids = top_ids(&searcher, "8 High Street, 7304");
        assert!(
            !ids.contains(&"osm:node:1".to_string()),
            "the 7018 address answers no query for 7304: {ids:?}"
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
    fn plans_numbers_units_streets_and_places_from_labelled_tokens() {
        let plan = |text: &str| QueryPlan::from_tagged(&tagged(text), &[]).expect("plan");
        let words = |words: &[&str]| {
            words
                .iter()
                .map(|word| word.to_string())
                .collect::<Vec<_>>()
        };
        let part = |label: Label, words: &[&str]| ContextPart {
            label,
            words: words.iter().map(|word| word.to_string()).collect(),
        };

        let floor =
            plan("79/house_number wellington/street st/street w/street fl36/unit toronto/locality");
        assert_eq!(floor.house_number.expect("number").text, "79");
        assert_eq!(floor.required, words(&["wellington", "street", "west"]));
        assert_eq!(floor.optional, words(&["fl36"]));
        assert_eq!(floor.context, [part(Label::Locality, &["toronto"])]);

        // Unit text only ranks, before or after the address.
        let unit = plan(
            "unit/unit 602/unit 670/house_number king/street st/street w/street \
             toronto/locality on/region m5v/postcode 1m9/postcode",
        );
        assert_eq!(unit.optional, words(&["unit", "602"]));
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
            plan("1000/house_number highway/street 7/street").required,
            words(&["highway", "7"])
        );
        assert_eq!(
            plan("st/street clair/street ave/street w/street").required,
            words(&["st", "clair", "avenue", "west"])
        );
        assert_eq!(
            plan("15/house_number the/street donway/street e/street").required,
            words(&["the", "donway", "east"])
        );
        // A street named first, its number after it.
        let german = plan("maximilianstr/street 5/house_number münchen/locality");
        assert_eq!(german.house_number.expect("number").text, "5");
        assert_eq!(german.required, words(&["maximilianstr"]));

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
            &[part(Label::Region, &["ontario"])],
        )
        .expect("plan");
        assert_eq!(places.required, words(&["seaforth", "huron", "east"]));
        assert_eq!(places.context, [part(Label::Region, &["ontario"])]);
        let number = plan("670/house_number");
        assert_eq!(
            (number.house_number, number.required),
            (None, words(&["670"]))
        );
        assert!(QueryPlan::from_tagged(&[], &[]).is_none());
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
