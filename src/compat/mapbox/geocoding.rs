//! Geocoding v6: `forward`, `reverse` and `batch`.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, atomic::AtomicBool},
};

use axum::{
    Json,
    body::Bytes,
    extract::{Query, State, rejection::BytesRejection, rejection::QueryRejection},
    http::StatusCode,
};
use serde::Serialize;
use serde_json::Value;
use tokio::task;

use super::{
    Api, Context, Coordinates, Found, Geometry, LayerFields, Params, Translated, Types,
    address_line, admitted, attribution, error,
};
use crate::{
    api::{
        Accuracy, Common, ContextArea, ForwardCollection, Languages, PartMatch, QueryMatch,
        ReverseCollection,
    },
    batch::map_in_order,
    compat::{Args, decode_id, encode_area_id, encode_id, iso, native},
    http::{
        batch::{self as og_batch, Prepared, RawItem},
        problem::Problem,
    },
    reverse::GeocodeOptions,
    runtime::{self, AppState, GeocodeRequest, Named, PlacesRequest},
    stop::CancelOnDrop,
};

/// Seconds a refused batch is told to wait, as og's own batches are.
const BATCH_RETRY_AFTER_SECONDS: u32 = 1;

/// Mapbox's forward default; og's own would let the server choose.
const DEFAULT_LIMIT: usize = 5;

/// `autocomplete`, on unless sent `false`.
fn typed(params: &Params) -> Result<bool, crate::compat::VendorError> {
    match params.get("autocomplete") {
        None => Ok(true),
        Some(value) if value.eq_ignore_ascii_case("true") => Ok(true),
        Some(value) if value.eq_ignore_ascii_case("false") => Ok(false),
        Some(value) => Err(Api::Geocoding.invalid(format!(
            "autocomplete must be true or false, not \"{value}\""
        ))),
    }
}

/// Mapbox's cap on the languages one request names.
const MAX_LANGUAGES: usize = 20;

/// A Geocoding v6 response.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct Collection {
    #[serde(rename = "type")]
    kind: &'static str,
    features: Vec<Feature>,
    attribution: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Feature {
    #[serde(rename = "type")]
    kind: &'static str,
    id: String,
    geometry: Geometry,
    properties: Properties,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Properties {
    mapbox_id: String,
    feature_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    full_address: Option<String>,
    name: String,
    coordinates: Coordinates,
    #[serde(skip_serializing_if = "Option::is_none")]
    place_formatted: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_code: Option<MatchCode>,
    context: Context<Layer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bbox: Option<[f64; 4]>,
}

/// How an address feature answers each part of a forward query, in
/// Mapbox's words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MatchCode {
    address_number: &'static str,
    street: &'static str,
    postcode: &'static str,
    place: &'static str,
    region: &'static str,
    country: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    secondary_address: Option<&'static str>,
    confidence: &'static str,
}

impl MatchCode {
    /// og's `match` of an address as Mapbox's `match_code`. A part the query
    /// does not state is `unmatched`, Mapbox's word for one not in the
    /// input, except a country og fills from where the address lies,
    /// `inferred`. og judges no sub-city part, Mapbox's `locality`, so it
    /// is not sent. A partial number is `plausible` only where it is
    /// interpolated; any other partial part is not the one asked for.
    fn of(found: QueryMatch, common: &Common) -> Self {
        let stated = |part: Option<PartMatch>| match part {
            Some(PartMatch::Matched) => "matched",
            Some(PartMatch::Partial | PartMatch::Unmatched) | None => "unmatched",
        };
        let address_number = match found.housenumber {
            Some(PartMatch::Partial) if common.accuracy == Accuracy::Interpolated => "plausible",
            part => stated(part),
        };
        let country = match found.country {
            None if common.context.country.is_some() => "inferred",
            part => stated(part),
        };
        Self {
            address_number,
            street: stated(found.street),
            postcode: stated(found.postcode),
            place: stated(found.locality),
            region: stated(found.region),
            country,
            secondary_address: found.unit.map(|part| stated(Some(part))),
            confidence: confidence(found, address_number),
        }
    }
}

/// Mapbox's confidence from how many stated parts changed. A part the query
/// does not state did not change. A changed house number or region is
/// `low`, as is a change to three or more of the others. Two are `medium`,
/// one `high`, none `exact`. An interpolated number is not a change.
fn confidence(found: QueryMatch, address_number: &str) -> &'static str {
    let changed = |part: Option<PartMatch>| part.is_some_and(|part| part != PartMatch::Matched);
    let number_changed = changed(found.housenumber) && address_number != "plausible";
    if number_changed || changed(found.region) {
        return "low";
    }
    let others = [
        found.street,
        found.unit,
        found.locality,
        found.postcode,
        found.country,
    ]
    .into_iter()
    .filter(|part| changed(*part))
    .count();
    match others {
        0 => "exact",
        1 => "high",
        2 => "medium",
        _ => "low",
    }
}

/// Feature types whose `bbox` Geocoding sends.
const BOXED: [&str; 7] = [
    "country",
    "region",
    "postcode",
    "district",
    "place",
    "locality",
    "neighborhood",
];

/// A Geocoding context layer, keyed `mapbox_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Layer {
    #[serde(skip_serializing_if = "Option::is_none")]
    mapbox_id: Option<String>,
    #[serde(flatten)]
    fields: LayerFields,
    #[serde(skip_serializing_if = "Option::is_none")]
    translations: Option<BTreeMap<String, Translation>>,
}

/// A layer's name in one of the languages asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Translation {
    language: String,
    name: String,
}

/// The languages a request names in, as `language` lists them.
#[derive(Clone, Copy)]
struct Naming<'a> {
    codes: &'a [String],
}

/// What og named a layer's record besides its name in the first language
/// asked. That is its local name when the two differ, and its names in the
/// other languages.
#[derive(Clone, Copy, Default)]
struct Names<'a> {
    local_name: Option<&'a str>,
    translations: Option<&'a BTreeMap<String, String>>,
}

impl<'a> Names<'a> {
    fn of_area(area: &'a ContextArea) -> Self {
        Self {
            local_name: area.local_name.as_deref(),
            translations: area.translations.as_ref(),
        }
    }

    fn of_result(common: &'a Common) -> Self {
        Self {
            local_name: common.local_name.as_deref(),
            translations: common.translations.as_ref(),
        }
    }
}

impl Naming<'_> {
    /// A layer's `translations`, sent only when more than one language is
    /// asked. Every language asked is in it, the first too. A language og
    /// holds no name in gets the name og answers for it, the local one. A
    /// layer filled from a part the source states names no record, so its
    /// text is the name in every language.
    fn translations(self, name: &str, names: Names) -> Option<BTreeMap<String, Translation>> {
        let (first, others) = self.codes.split_first().filter(|_| self.codes.len() > 1)?;
        let local = names.local_name.unwrap_or(name);
        let named_in = |code: &String| {
            names
                .translations
                .and_then(|found| found.get(code))
                .map_or(local, String::as_str)
        };
        let entry = |code: &String, name: &str| {
            (
                code.clone(),
                Translation {
                    language: code.clone(),
                    name: name.to_string(),
                },
            )
        };
        Some(
            std::iter::once(entry(first, name))
                .chain(others.iter().map(|code| entry(code, named_in(code))))
                .collect(),
        )
    }
}

/// The layers `translations` is sent on.
const TRANSLATED_LAYERS: [&str; 5] = ["neighborhood", "place", "district", "region", "country"];

impl Feature {
    fn of(found: Found, naming: Naming) -> Self {
        let common = found.common;
        // What og named each area, by the id its layer carries. A result
        // that is an area is its own layer.
        let context = &common.context;
        let mut areas = [
            &context.neighbourhood,
            &context.locality,
            &context.district,
            &context.region,
            &context.country,
        ]
        .into_iter()
        .flatten()
        .map(|area| (encode_area_id(area), Names::of_area(area)))
        .collect::<HashMap<_, _>>();
        areas.insert(encode_id(common), Names::of_result(common));
        let translated = Translated::of(Api::Geocoding, found);
        let layer = |key: &str, area: super::Area| {
            let (mapbox_id, fields) = area.split();
            let translations = TRANSLATED_LAYERS
                .contains(&key)
                .then(|| {
                    let names = mapbox_id
                        .as_ref()
                        .and_then(|id| areas.get(id).copied())
                        .unwrap_or_default();
                    naming.translations(&fields.name, names)
                })
                .flatten();
            Layer {
                mapbox_id,
                fields,
                translations,
            }
        };
        let match_code = found
            .matched
            .filter(|_| translated.feature_type == "address")
            .map(|matched| MatchCode::of(*matched, common));
        Self {
            kind: "Feature",
            id: translated.mapbox_id.clone(),
            geometry: Geometry::point(found.coordinates),
            properties: Properties {
                mapbox_id: translated.mapbox_id,
                feature_type: translated.feature_type,
                full_address: translated.full_address,
                name: translated.name,
                coordinates: translated.coordinates,
                place_formatted: translated.place_formatted,
                match_code,
                context: translated.context.map_named(layer),
                bbox: translated
                    .bbox
                    .filter(|_| BOXED.contains(&translated.feature_type)),
            },
        }
    }
}

impl Collection {
    fn forward(collection: &ForwardCollection, types: Option<&Types>, naming: Naming) -> Self {
        Self {
            kind: "FeatureCollection",
            features: collection
                .features
                .iter()
                .filter(|feature| admitted(types, &feature.properties.common))
                .map(|feature| Feature::of(Found::forward(feature), naming))
                .collect(),
            attribution: attribution(&collection.geocoding.attribution),
        }
    }

    fn reverse(collection: &ReverseCollection, types: Option<&Types>, naming: Naming) -> Self {
        let collection = &super::at_point(collection);
        Self {
            kind: "FeatureCollection",
            features: collection
                .features
                .iter()
                .filter(|feature| admitted(types, &feature.properties.common))
                .map(|feature| Feature::of(Found::reverse(feature), naming))
                .collect(),
            attribution: attribution(&collection.geocoding.attribution),
        }
    }
}

/// Which og lookup answers a Mapbox query.
#[derive(Debug)]
enum Lookup {
    Geocode(Args),
    /// Free text with `autocomplete` on, Mapbox's default: the geocode when
    /// it reads the whole query, else what the text so far completes to.
    Typed {
        geocode: Args,
        complete: Args,
        limit: usize,
    },
    /// A `mapbox_id` sent as `q`, looked up by its og id. `text` is `q` as
    /// a plain search, for an id og could never have issued.
    Id {
        named: crate::compat::OgRef,
        lang: Option<String>,
        text: Args,
    },
    /// `countries` are the codes `country` keeps, none for any.
    Reverse {
        args: Args,
        countries: Vec<String>,
    },
}

#[derive(Debug)]
struct Plan {
    lookup: Lookup,
    types: Option<Types>,
    /// `language`, checked as Mapbox checks it.
    languages: Vec<String>,
}

/// `language`, a comma-separated list of at most 20 unique language codes.
/// The first names each result, and the others its context's
/// `translations`.
fn languages(params: &Params) -> Result<Vec<String>, crate::compat::VendorError> {
    let Some(value) = params.get("language") else {
        return Ok(Vec::new());
    };
    let mut codes = Vec::<String>::new();
    for code in value.split(',').map(str::trim) {
        if Languages::parse(code).map_or(true, |parsed| parsed.is_empty()) {
            return Err(Api::Geocoding.invalid(format!("\"{code}\" is not a valid language code")));
        }
        if codes.iter().any(|known| known.eq_ignore_ascii_case(code)) {
            return Err(Api::Geocoding
                .invalid("options.language should be a list of unique language codes"));
        }
        codes.push(code.to_string());
    }
    if codes.len() > MAX_LANGUAGES {
        return Err(Api::Geocoding.invalid(format!(
            "options.language should be a list of no more than {MAX_LANGUAGES} languages"
        )));
    }
    Ok(codes)
}

/// Search addresses (Mapbox Geocoding v6)
pub(super) async fn forward(
    State(state): State<AppState>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Json<Collection>, crate::compat::VendorError> {
    let params = Params::from_query(Api::Geocoding, query)?;
    let plan = forward_plan(&params)?;
    answer(&state, plan).await.map(Json)
}

/// Reverse geocode a point (Mapbox Geocoding v6)
pub(super) async fn reverse(
    State(state): State<AppState>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Json<Collection>, crate::compat::VendorError> {
    let params = Params::from_query(Api::Geocoding, query)?;
    let plan = reverse_plan(&params)?;
    answer(&state, plan).await.map(Json)
}

async fn answer(state: &AppState, plan: Plan) -> Result<Collection, crate::compat::VendorError> {
    let types = plan.types.as_ref();
    let naming = Naming {
        codes: &plan.languages,
    };
    let failed = |problem| Api::Geocoding.failed(problem);
    match plan.lookup {
        Lookup::Geocode(args) => native::geocode(state, args)
            .await
            .map(|collection| Collection::forward(&collection, types, naming))
            .map_err(failed),
        Lookup::Id { named, lang, text } => {
            match native::lookup(state, &named, lang.as_deref()).await {
                Ok(Some(collection)) => Ok(Collection::forward(&collection, types, naming)),
                Ok(None) => native::geocode(state, text)
                    .await
                    .map(|collection| Collection::forward(&collection, types, naming))
                    .map_err(failed),
                Err(problem) => Err(failed(problem)),
            }
        }
        Lookup::Typed {
            geocode,
            complete,
            limit,
        } => {
            let found = native::geocode(state, geocode).await.map_err(failed)?;
            if reads_whole(&found) {
                return Ok(Collection::forward(&found, types, naming));
            }
            let mut completed = native::autocomplete(state, complete)
                .await
                .map_err(failed)?;
            // Geocoding answers no POIs.
            completed
                .features
                .retain(|feature| feature.properties.common.layer != crate::record::Layer::Poi);
            completed.features.truncate(limit);
            Ok(Collection::forward(&completed, types, naming))
        }
        Lookup::Reverse { args, countries } => native::reverse(state, args)
            .await
            .map(|mut collection| {
                super::keep_countries(&mut collection, &countries);
                Collection::reverse(&collection, types, naming)
            })
            .map_err(failed),
    }
}

/// Whether the geocode answers the query as typed, so nothing is left to
/// complete. Its best answer is no stand-in area, and either matched every
/// part the query states, or read a house number on the street it names.
/// An address of the stated number and street in another town is still
/// the address asked for, and its `match_code` says which part changed.
/// One found by its whole name states no part.
fn reads_whole(found: &ForwardCollection) -> bool {
    found.features.first().is_some_and(|best| {
        !best.properties.fallback
            && best.properties.matched.as_ref().is_none_or(|parts| {
                let addressed = parts.street == Some(PartMatch::Matched)
                    && parts
                        .housenumber
                        .is_some_and(|number| number != PartMatch::Unmatched);
                addressed
                    || [
                        parts.housenumber,
                        parts.street,
                        parts.unit,
                        parts.locality,
                        parts.region,
                        parts.postcode,
                        parts.country,
                    ]
                    .into_iter()
                    .flatten()
                    .all(|part| part == PartMatch::Matched)
            })
    })
}

/// Geocoding forward's largest `limit`.
const MAX_FORWARD_LIMIT: usize = 10;
/// Geocoding reverse's largest `limit`.
const MAX_REVERSE_LIMIT: usize = 5;

/// `limit` checked against Mapbox's range.
fn limit(params: &Params, max: usize) -> Result<Option<usize>, crate::compat::VendorError> {
    params
        .get("limit")
        .map(|value| {
            value
                .parse::<usize>()
                .ok()
                .filter(|limit| (1..=max).contains(limit))
                .ok_or_else(|| {
                    Api::Geocoding.invalid(format!("limit must be a number between 1 and {max}"))
                })
        })
        .transpose()
}

/// The structured-input parameters. Any of them, without `q`, makes a
/// structured query.
const STRUCTURED: [&str; 9] = [
    "address_line1",
    "address_number",
    "street",
    "block",
    "place",
    "region",
    "postcode",
    "locality",
    "neighborhood",
];

fn forward_plan(params: &Params) -> Result<Plan, crate::compat::VendorError> {
    let types = params.types(Api::Geocoding)?;
    let languages = languages(params)?;
    let lang = (!languages.is_empty()).then(|| languages.join(","));
    let layer = types.as_ref().and_then(Types::og_layer);
    let wanted = limit(params, MAX_FORWARD_LIMIT)?;
    let limited = |q: &str, limit: usize| {
        Args::new()
            .set("q", q)
            .set("limit", limit.to_string())
            .maybe("country", params.get("country"))
            .maybe("focus", params.focus())
            .maybe("bbox", params.get("bbox"))
            .maybe("layer", layer)
            .maybe("lang", lang.clone())
    };
    let text = |q: &str| limited(q, wanted.unwrap_or(DEFAULT_LIMIT));
    if let Some(q) = params.get("q") {
        let lookup = match decode_id(q) {
            Some(named) => Lookup::Id {
                named,
                lang: lang.clone(),
                text: text(q),
            },
            None if typed(params)? => Lookup::Typed {
                geocode: text(q),
                // POIs are dropped from what the text completes to, so as
                // many are asked for as Geocoding may answer.
                complete: limited(q, MAX_FORWARD_LIMIT),
                limit: wanted.unwrap_or(DEFAULT_LIMIT),
            },
            None => Lookup::Geocode(text(q)),
        };
        return Ok(Plan {
            lookup,
            types,
            languages,
        });
    }
    if !STRUCTURED.iter().any(|name| params.get(name).is_some()) {
        return Err(error(StatusCode::NOT_FOUND, "Not Found"));
    }
    // In structured input `country` is a code, a hard filter, or a country's
    // name, a soft hint og has no parameter for.
    let country = params.get("country");
    let code = country.and_then(country_code);
    let line = params
        .get("address_line1")
        .map(str::to_string)
        .or_else(|| address_line(params.get("address_number"), params.get("street")));
    let lookup = match line {
        // Mapbox's `place` is the city, og's `locality`. Mapbox's `locality`
        // and `neighborhood` are parts of a city, which og's address fields
        // have no slot for.
        Some(line) => Lookup::Geocode(
            Args::new()
                .set("address", line)
                .maybe("locality", params.get("place"))
                .maybe("region", params.get("region"))
                .maybe("postcode", params.get("postcode"))
                .maybe("country", code)
                .maybe("layer", layer)
                .maybe("lang", lang.clone()),
        ),
        // og's address fields need a street. Areas alone are a search for
        // the area, so they are searched as the text they spell.
        None => {
            let named = [
                "block",
                "neighborhood",
                "locality",
                "place",
                "region",
                "postcode",
            ]
            .into_iter()
            .filter_map(|name| params.get(name))
            .chain(country.filter(|_| code.is_none()))
            .collect::<Vec<_>>()
            .join(", ");
            let args = Args::new()
                .set("q", named)
                .set("limit", wanted.unwrap_or(DEFAULT_LIMIT).to_string())
                .maybe("country", code)
                .maybe("focus", params.focus())
                .maybe("bbox", params.get("bbox"))
                .maybe("layer", layer)
                .maybe("lang", lang.clone());
            Lookup::Geocode(args)
        }
    };
    Ok(Plan {
        lookup,
        types,
        languages,
    })
}

/// A country code as og's `country` filter takes it, alpha-2. An alpha-3
/// code is converted.
fn country_code(value: &str) -> Option<String> {
    let letters = value.bytes().all(|byte| byte.is_ascii_alphabetic());
    match value.len() {
        2 if letters => Some(value.to_ascii_uppercase()),
        3 if letters => iso::alpha2_of(value).map(str::to_string),
        _ => None,
    }
}

fn reverse_plan(params: &Params) -> Result<Plan, crate::compat::VendorError> {
    let types = params.types(Api::Geocoding)?;
    // og answers one result per hierarchy level, Mapbox's `limit=1`, and at
    // most one of a single type. A larger limit is checked as Mapbox checks
    // it and then answered with what og has.
    let limit = limit(params, MAX_REVERSE_LIMIT)?;
    let single = types.as_ref().is_some_and(|types| types.0.len() == 1);
    if limit.is_some_and(|limit| limit > 1) && !single {
        return Err(Api::Geocoding.invalid(
            "limit must be combined with a single type parameter when reverse geocoding",
        ));
    }
    let languages = languages(params)?;
    let args = Args::new()
        .maybe("lon", params.get("longitude"))
        .maybe("lat", params.get("latitude"))
        .maybe("layer", types.as_ref().and_then(Types::reverse_layer))
        .maybe("lang", (!languages.is_empty()).then(|| languages.join(",")));
    Ok(Plan {
        lookup: Lookup::Reverse {
            args,
            countries: super::countries(Api::Geocoding, params)?,
        },
        types,
        languages,
    })
}

/// `POST /search/geocode/v6/batch`'s answer.
#[derive(Debug, Serialize)]
pub(super) struct Batch {
    batch: Vec<Collection>,
}

/// One batch query, checked by og's batch engine.
#[derive(Clone)]
enum Checked {
    Places(Named<PlacesRequest>),
    Geocode(Named<GeocodeRequest>),
    /// The geocode of the label a vendor id carries, kept to the record the
    /// id names (see [`native::keep_named`]).
    Labelled(Named<GeocodeRequest>, crate::compat::OgRef),
    /// The reverse lookup and the countries `country` keeps.
    Reverse(Named<GeocodeOptions>, Vec<String>),
}

/// Where a query reads in the Pack, so neighbours run together.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Place {
    Places(String),
    Geocode([String; 3]),
    Reverse(u64),
}

#[derive(Debug)]
enum Answer {
    Forward(ForwardCollection),
    Reverse(ReverseCollection),
}

/// Geocode many queries (Mapbox Geocoding v6)
///
/// Runs og's batch engine once over every query, forward and reverse
/// mixed, under one batch slot. A query og refuses fails the whole request.
/// Mapbox's response has no slot for one query's error.
pub(super) async fn batch(
    State(state): State<AppState>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<Batch>, crate::compat::VendorError> {
    let body = body.map_err(|rejection| error(rejection.status(), rejection.body_text()))?;
    let queries: Vec<Value> = serde_json::from_slice(&body)
        .map_err(|_| Api::Geocoding.invalid("Batch body must be a JSON array of query objects"))?;
    let max = state.limits().max_batch_items();
    if queries.len() > max {
        return Err(
            Api::Geocoding.invalid(format!("Batch queries must include {max} queries or less"))
        );
    }
    let mut checked = Vec::with_capacity(queries.len());
    let mut types = Vec::with_capacity(queries.len());
    let mut languages = Vec::with_capacity(queries.len());
    for query in &queries {
        let params = batch_params(query)?;
        let plan = if params.get("longitude").is_some() || params.get("latitude").is_some() {
            reverse_plan(&params)?
        } else {
            forward_plan(&params)?
        };
        checked.push(check(&state, plan.lookup)?);
        types.push(plan.types);
        languages.push(plan.languages);
    }

    let Ok(permit) = Arc::clone(state.batch_slots()).try_acquire_owned() else {
        return Err(Api::Geocoding.failed(Problem::busy(BATCH_RETRY_AFTER_SECONDS)));
    };
    // Dropped with this future when the client leaves, which stops the
    // queries not yet run.
    let cancel = Arc::new(AtomicBool::new(false));
    let _cancel = CancelOnDrop(Arc::clone(&cancel));
    let worker = state.clone();
    let answers = task::spawn_blocking(move || {
        let _permit = permit;
        worker.batch_pool().install(|| {
            map_in_order(&checked, place, &cancel, |_, query| {
                Ok(look_up(&worker, query, &cancel))
            })
        })
    })
    .await
    .map_err(|join| Api::Geocoding.failed(Problem::from(join)))?
    .map_err(|failed| Api::Geocoding.failed(Problem::from(failed)))?;

    let mut batch = Vec::with_capacity(answers.len());
    for ((answer, types), codes) in answers.into_iter().zip(&types).zip(&languages) {
        let naming = Naming { codes };
        batch.push(
            match answer.map_err(|problem| Api::Geocoding.failed(problem))? {
                Answer::Forward(collection) => {
                    Collection::forward(&collection, types.as_ref(), naming)
                }
                Answer::Reverse(collection) => {
                    Collection::reverse(&collection, types.as_ref(), naming)
                }
            },
        );
    }
    Ok(Json(Batch { batch }))
}

/// A batch query's keys as query parameters. Mapbox takes a list as a JSON
/// array or as the comma-separated text a GET sends.
fn batch_params(query: &Value) -> Result<Params, crate::compat::VendorError> {
    let invalid = || Api::Geocoding.invalid("Each batch query must be a JSON object of parameters");
    let object = query.as_object().ok_or_else(invalid)?;
    let text = |value: &Value| match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    };
    let mut pairs = Vec::with_capacity(object.len());
    for (name, value) in object {
        let value = match value {
            Value::Null => continue,
            Value::Array(items) => items
                .iter()
                .map(|item| text(item).ok_or_else(invalid))
                .collect::<Result<Vec<_>, _>>()?
                .join(","),
            other => text(other).ok_or_else(invalid)?,
        };
        pairs.push((name.clone(), value));
    }
    Ok(Params(pairs))
}

/// A query checked by the rules its og GET is checked with.
fn check(state: &AppState, lookup: Lookup) -> Result<Checked, crate::compat::VendorError> {
    fn prepared<Q>(prepared: Prepared<Q>) -> Result<Q, Problem> {
        match prepared {
            Prepared::Ready { query, .. } => Ok(query),
            Prepared::Failed { problem, .. } => Err(problem),
        }
    }
    let checked = match lookup {
        // og's batch engine has no autocomplete, so a batch query is read
        // whole, as `autocomplete=false` reads it.
        Lookup::Geocode(args) | Lookup::Typed { geocode: args, .. } => {
            prepared(og_batch::prepare(state, runtime::GEOCODE, &[], item(args)))
                .map(Checked::Geocode)
        }
        Lookup::Id { named, lang, text } => {
            let id = match &named {
                crate::compat::OgRef::Labelled { label, .. } => {
                    let args = Args::new()
                        .set("q", label.clone())
                        .set("limit", native::LABEL_CANDIDATES.to_string())
                        .maybe("lang", lang);
                    return prepared(og_batch::prepare(state, runtime::GEOCODE, &[], item(args)))
                        .map(|request| Checked::Labelled(request, named))
                        .map_err(|problem| Api::Geocoding.failed(problem));
                }
                crate::compat::OgRef::Catalogued { id, .. } => id.as_str().to_string(),
            };
            let ids = Args::new().set("ids", id).maybe("lang", lang);
            match prepared(og_batch::prepare(state, runtime::PLACES, &[], item(ids))) {
                Ok(request) => Ok(Checked::Places(request)),
                Err(problem) if problem.status() == 400 => {
                    prepared(og_batch::prepare(state, runtime::GEOCODE, &[], item(text)))
                        .map(Checked::Geocode)
                }
                Err(problem) => Err(problem),
            }
        }
        Lookup::Reverse { args, countries } => {
            prepared(og_batch::prepare(state, runtime::REVERSE, &[], item(args)))
                .map(|options| Checked::Reverse(options, countries))
        }
    };
    checked.map_err(|problem| Api::Geocoding.failed(problem))
}

fn item(args: Args) -> RawItem {
    RawItem::Fields(
        args.0
            .into_iter()
            .map(|(name, value)| (name.to_string(), Value::String(value)))
            .collect(),
    )
}

fn place(query: &Checked) -> Place {
    match query {
        Checked::Places(request) => Place::Places((runtime::PLACES.place)(request)),
        Checked::Geocode(request) | Checked::Labelled(request, _) => {
            Place::Geocode((runtime::GEOCODE.place)(request))
        }
        Checked::Reverse(options, _) => Place::Reverse((runtime::REVERSE.place)(options)),
    }
}

fn look_up(state: &AppState, query: &Checked, cancel: &Arc<AtomicBool>) -> Result<Answer, Problem> {
    match query {
        Checked::Places(request) => outcome(og_batch::look_up(
            state,
            runtime::PLACES,
            None,
            request.clone(),
            cancel,
        ))
        .map(Answer::Forward),
        Checked::Geocode(request) => outcome(og_batch::look_up(
            state,
            runtime::GEOCODE,
            None,
            request.clone(),
            cancel,
        ))
        .map(Answer::Forward),
        Checked::Labelled(request, named) => outcome(og_batch::look_up(
            state,
            runtime::GEOCODE,
            None,
            request.clone(),
            cancel,
        ))
        .map(|mut collection| {
            native::keep_named(&mut collection, named);
            Answer::Forward(collection)
        }),
        Checked::Reverse(options, countries) => outcome(og_batch::look_up(
            state,
            runtime::REVERSE,
            None,
            options.clone(),
            cancel,
        ))
        .map(|mut collection| {
            super::keep_countries(&mut collection, countries);
            Answer::Reverse(collection)
        }),
    }
}

fn outcome<A>(item: og_batch::Item<A>) -> Result<A, Problem> {
    match (item.response, item.error) {
        (Some(response), _) => Ok(response),
        (None, Some(problem)) => Err(problem),
        (None, None) => Err(Problem::internal(anyhow::anyhow!(
            "a batch item has neither an answer nor an error"
        ))),
    }
}
