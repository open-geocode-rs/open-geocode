//! Geocoding v6: `forward`, `reverse` and `batch`.

use std::sync::{Arc, atomic::AtomicBool};

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
    Api, Context, Coordinates, Geometry, LayerFields, Params, Translated, Types, address_line,
    admitted, attribution, error,
};
use crate::{
    api::{ForwardCollection, ReverseCollection},
    batch::map_in_order,
    compat::{Args, decode_id, iso, native},
    http::{
        batch::{self as og_batch, Prepared, RawItem},
        problem::Problem,
    },
    reverse::GeocodeOptions,
    runtime::{self, AppState, GeocodeRequest, PlacesRequest},
    stop::CancelOnDrop,
};

/// Seconds a refused batch is told to wait, as og's own batches are.
const BATCH_RETRY_AFTER_SECONDS: u32 = 1;

/// Mapbox's forward default; og's own would let the server choose.
const DEFAULT_FORWARD_LIMIT: &str = "5";

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
    context: Context<Layer>,
}

/// A Geocoding context layer, keyed `mapbox_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Layer {
    #[serde(skip_serializing_if = "Option::is_none")]
    mapbox_id: Option<String>,
    #[serde(flatten)]
    fields: LayerFields,
}

impl Feature {
    fn of(common: &crate::api::Common, coordinates: [f64; 2]) -> Self {
        let translated = Translated::of(common, coordinates);
        Self {
            kind: "Feature",
            id: translated.mapbox_id.clone(),
            geometry: Geometry::point(coordinates),
            properties: Properties {
                mapbox_id: translated.mapbox_id,
                feature_type: translated.feature_type,
                full_address: translated.full_address,
                name: translated.name,
                coordinates: translated.coordinates,
                place_formatted: translated.place_formatted,
                context: translated.context.map(|area| {
                    let (mapbox_id, fields) = area.split();
                    Layer { mapbox_id, fields }
                }),
            },
        }
    }
}

impl Collection {
    fn forward(collection: &ForwardCollection, types: Option<&Types>) -> Self {
        Self {
            kind: "FeatureCollection",
            features: collection
                .features
                .iter()
                .filter(|feature| admitted(types, &feature.properties.common))
                .map(|feature| {
                    Feature::of(&feature.properties.common, feature.geometry.coordinates)
                })
                .collect(),
            attribution: attribution(&collection.geocoding.attribution),
        }
    }

    fn reverse(collection: &ReverseCollection, types: Option<&Types>) -> Self {
        Self {
            kind: "FeatureCollection",
            features: collection
                .features
                .iter()
                .filter(|feature| admitted(types, &feature.properties.common))
                .map(|feature| {
                    Feature::of(&feature.properties.common, feature.geometry.coordinates)
                })
                .collect(),
            attribution: attribution(&collection.geocoding.attribution),
        }
    }
}

/// Which og lookup answers a Mapbox query.
#[derive(Debug)]
enum Lookup {
    Geocode(Args),
    /// A `mapbox_id` sent as `q`, looked up by its og id. `text` is `q` as
    /// a plain search, for an id `/places` cannot look up.
    Id {
        ids: Args,
        text: Args,
    },
    Reverse(Args),
}

#[derive(Debug)]
struct Plan {
    lookup: Lookup,
    types: Option<Types>,
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
    let failed = |problem| Api::Geocoding.failed(problem);
    match plan.lookup {
        Lookup::Geocode(args) => native::geocode(state, args)
            .await
            .map(|collection| Collection::forward(&collection, types))
            .map_err(failed),
        Lookup::Id { ids, text } => match native::places(state, ids).await {
            Ok(collection) => Ok(Collection::forward(&collection, types)),
            Err(problem) if problem.status() == 400 => native::geocode(state, text)
                .await
                .map(|collection| Collection::forward(&collection, types))
                .map_err(failed),
            Err(problem) => Err(failed(problem)),
        },
        Lookup::Reverse(args) => native::reverse(state, args)
            .await
            .map(|collection| Collection::reverse(&collection, types))
            .map_err(failed),
    }
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
    let layer = types.as_ref().and_then(Types::og_layer);
    let text = |q: &str| {
        Args::new()
            .set("q", q)
            .set(
                "limit",
                params.get("limit").unwrap_or(DEFAULT_FORWARD_LIMIT),
            )
            .maybe("country", params.get("country"))
            .maybe("focus", params.focus())
            .maybe("bbox", params.get("bbox"))
            .maybe("layer", layer)
    };
    if let Some(q) = params.get("q") {
        let lookup = match decode_id(q) {
            Some(og_id) => Lookup::Id {
                ids: Args::new().set("ids", og_id),
                text: text(q),
            },
            None => Lookup::Geocode(text(q)),
        };
        return Ok(Plan { lookup, types });
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
                .maybe("layer", layer),
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
                .set(
                    "limit",
                    params.get("limit").unwrap_or(DEFAULT_FORWARD_LIMIT),
                )
                .maybe("country", code)
                .maybe("focus", params.focus())
                .maybe("bbox", params.get("bbox"))
                .maybe("layer", layer);
            Lookup::Geocode(args)
        }
    };
    Ok(Plan { lookup, types })
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
    let limit = params
        .get("limit")
        .and_then(|limit| limit.parse::<u64>().ok());
    let single = types.as_ref().is_some_and(|types| types.0.len() == 1);
    if limit.is_some_and(|limit| limit > 1) && !single {
        return Err(Api::Geocoding.invalid(
            "limit must be combined with a single type parameter when reverse geocoding",
        ));
    }
    let args = Args::new()
        .maybe("lon", params.get("longitude"))
        .maybe("lat", params.get("latitude"))
        .maybe("layer", types.as_ref().and_then(Types::og_layer));
    Ok(Plan {
        lookup: Lookup::Reverse(args),
        types,
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
    Places(PlacesRequest),
    Geocode(GeocodeRequest),
    Reverse(GeocodeOptions),
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
    for query in &queries {
        let params = batch_params(query)?;
        let plan = if params.get("longitude").is_some() || params.get("latitude").is_some() {
            reverse_plan(&params)?
        } else {
            forward_plan(&params)?
        };
        checked.push(check(&state, plan.lookup)?);
        types.push(plan.types);
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
    for (answer, types) in answers.into_iter().zip(&types) {
        batch.push(
            match answer.map_err(|problem| Api::Geocoding.failed(problem))? {
                Answer::Forward(collection) => Collection::forward(&collection, types.as_ref()),
                Answer::Reverse(collection) => Collection::reverse(&collection, types.as_ref()),
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
        Lookup::Geocode(args) => {
            prepared(og_batch::prepare(state, runtime::GEOCODE, &[], item(args)))
                .map(Checked::Geocode)
        }
        Lookup::Id { ids, text } => {
            match prepared(og_batch::prepare(state, runtime::PLACES, &[], item(ids))) {
                Ok(request) => Ok(Checked::Places(request)),
                Err(problem) if problem.status() == 400 => {
                    prepared(og_batch::prepare(state, runtime::GEOCODE, &[], item(text)))
                        .map(Checked::Geocode)
                }
                Err(problem) => Err(problem),
            }
        }
        Lookup::Reverse(args) => {
            prepared(og_batch::prepare(state, runtime::REVERSE, &[], item(args)))
                .map(Checked::Reverse)
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
        Checked::Geocode(request) => Place::Geocode((runtime::GEOCODE.place)(request)),
        Checked::Reverse(options) => Place::Reverse((runtime::REVERSE.place)(options)),
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
        Checked::Reverse(options) => outcome(og_batch::look_up(
            state,
            runtime::REVERSE,
            None,
            *options,
            cancel,
        ))
        .map(Answer::Reverse),
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
