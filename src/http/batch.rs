//! Batch lookups: many items in one request, on the routes that answer one.
//!
//! `QUERY` (RFC 10008) is the batch method, and `POST` an alias for clients
//! that cannot send `QUERY` yet. Each item is read exactly as a `GET` of the
//! same route reads its query string, so an item's answer, or its error, is
//! the one that `GET` would return. A bad item never fails the batch.

use std::{
    error::Error as _,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use axum::{
    Json,
    body::to_bytes,
    extract::Request,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use http_body_util::LengthLimitError;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
use tokio::task;

use crate::{
    batch::map_in_order,
    http::{
        bounds::{MAX_BATCH_BODY_BYTES, MAX_BATCH_ID_CHARS, MAX_BATCH_ITEMS},
        problem::{InvalidParam, Problem},
        request_id::RequestId,
    },
    runtime::AppState,
};

/// The methods a batchable route takes, as its `Allow` header lists them.
pub(crate) const METHODS: &str = "GET, QUERY, POST";

/// The one media type a batch body is read as, as `Accept-Query` lists it.
pub(crate) const MEDIA_TYPE: &str = "application/json";

/// One route's lookup, split where a batch needs it: `prepare` checks
/// parameters by the rules `GET` checks them with, and `answer` looks them
/// up. The `GET` handler runs the same two.
pub(crate) struct Lookup<P, Q, A> {
    pub(crate) route: &'static str,
    pub(crate) prepare: fn(P) -> Result<Q, Problem>,
    pub(crate) answer: fn(&AppState, Q) -> crate::Result<A>,
}

// By hand: a derive would demand `P`, `Q` and `A` be `Copy` too, though only
// function pointers are held.
impl<P, Q, A> Clone for Lookup<P, Q, A> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<P, Q, A> Copy for Lookup<P, Q, A> {}

#[derive(Debug, Serialize)]
struct Batch<A> {
    results: Vec<Item<A>>,
    summary: Summary,
}

#[derive(Debug, Serialize)]
struct Item<A> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<A>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Problem>,
}

impl<A> Item<A> {
    fn failed(id: Option<String>, error: Problem) -> Self {
        Self {
            id,
            status: error.status(),
            response: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Default, Serialize)]
struct Summary {
    total: usize,
    succeeded: usize,
    failed: usize,
}

/// Sets its flag when dropped. axum drops a handler's future when its client
/// disconnects, and this with it, which stops the items not yet started.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Answer a batch request on `lookup`'s route.
pub(crate) async fn run<P, Q, A>(
    state: AppState,
    lookup: Lookup<P, Q, A>,
    request: Request,
) -> Response
where
    P: DeserializeOwned + 'static,
    Q: 'static,
    A: Serialize + Send + 'static,
{
    match answer(state, lookup, request).await {
        Ok(response) => response,
        Err(problem) => problem.into_response(),
    }
}

async fn answer<P, Q, A>(
    state: AppState,
    lookup: Lookup<P, Q, A>,
    request: Request,
) -> Result<Response, Problem>
where
    P: DeserializeOwned + 'static,
    Q: 'static,
    A: Serialize + Send + 'static,
{
    let started = Instant::now();
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone());
    let shared = shared_parameters(request.uri().query())?;
    let items = read_items(request).await?;

    let cancel = Arc::new(AtomicBool::new(false));
    let _stop = StopOnDrop(Arc::clone(&cancel));
    let worker = state.clone();
    let results = task::spawn_blocking(move || {
        // A pool of its own, so a single `GET` on the blocking pool never
        // waits behind a batch's items.
        worker.batch_pool().install(|| {
            map_in_order(
                &items,
                |_| (),
                &cancel,
                |_, item| Ok(look_up(&worker, lookup, &shared, item)),
            )
        })
    })
    .await?
    .map_err(Problem::from)?;

    let mut summary = Summary {
        total: results.len(),
        ..Summary::default()
    };
    let results = results
        .into_iter()
        .enumerate()
        .map(|(index, mut item)| {
            let Some(error) = item.error.take() else {
                summary.succeeded += 1;
                return item;
            };
            summary.failed += 1;
            // Inside a 200 the middleware sees no Problem, so the batch logs
            // its own 500s and stamps its own ids.
            let Some(id) = &request_id else {
                item.error = Some(error);
                return item;
            };
            if error.status() == StatusCode::INTERNAL_SERVER_ERROR.as_u16() {
                state.error_log().internal_error(
                    id,
                    lookup.route,
                    Some(index),
                    started,
                    error.cause().map(|cause| format!("{cause:#}")),
                );
            }
            item.error = Some(error.with_request_id(id.clone()));
            item
        })
        .collect();

    Ok(Json(Batch { results, summary }).into_response())
}

/// One item, answered or failed. A panic fails the item, not the batch.
fn look_up<P, Q, A>(
    state: &AppState,
    lookup: Lookup<P, Q, A>,
    shared: &[(String, String)],
    item: &Map<String, Value>,
) -> Item<A>
where
    P: DeserializeOwned,
{
    let id = match item_id(item) {
        Ok(id) => id,
        Err(problem) => return Item::failed(None, problem),
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<A, Problem> {
        let query = item_query(shared, item)?;
        let params = serde_urlencoded::from_str::<P>(&query)
            .map_err(|_| Problem::unparsable_parameters())?;
        let prepared = (lookup.prepare)(params)?;
        Ok((lookup.answer)(state, prepared)?)
    }))
    .unwrap_or_else(|payload| Err(Problem::panicked(payload.as_ref())));
    match outcome {
        Ok(response) => Item {
            id,
            status: StatusCode::OK.as_u16(),
            response: Some(response),
            error: None,
        },
        Err(problem) => Item::failed(id, problem),
    }
}

/// The request's own query string, which every item starts from.
fn shared_parameters(query: Option<&str>) -> Result<Vec<(String, String)>, Problem> {
    serde_urlencoded::from_str(query.unwrap_or_default())
        .map_err(|_| Problem::unparsable_parameters())
}

/// The query string `GET` would have been sent for this item: the shared
/// parameters, each overridden by the item's own of the same name.
///
/// Going through the query string, rather than reading the item as JSON into
/// the parameters, is what makes an item mean what the same `GET` means: one
/// parser, one set of rules, one set of errors.
fn item_query(shared: &[(String, String)], item: &Map<String, Value>) -> Result<String, Problem> {
    let mut own = Vec::with_capacity(item.len());
    for (name, value) in item {
        let value = match value {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
            // An absent value is an absent key, here as in the answers.
            Value::Null => continue,
            Value::Array(_) | Value::Object(_) => {
                return Err(Problem::malformed_parameter(format!(
                    "{name} must be a string or a number"
                ))
                .with_invalid_params(vec![InvalidParam::new(
                    name.as_str(),
                    "not a string or a number",
                )]));
            }
        };
        if name != "id" {
            own.push((name.clone(), value));
        }
    }
    let mut pairs = shared
        .iter()
        .filter(|(name, _)| !own.iter().any(|(given, _)| given == name))
        .cloned()
        .collect::<Vec<_>>();
    pairs.extend(own);
    serde_urlencoded::to_string(&pairs).map_err(|error| Problem::internal(error.into()))
}

/// The item's `id`, echoed in its result.
fn item_id(item: &Map<String, Value>) -> Result<Option<String>, Problem> {
    let invalid = |reason: &str| {
        Problem::malformed_parameter(format!("id {reason}"))
            .with_invalid_params(vec![InvalidParam::new("id", reason)])
    };
    match item.get("id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) if id.chars().count() <= MAX_BATCH_ID_CHARS => Ok(Some(id.clone())),
        Some(Value::String(_)) => Err(invalid(&format!(
            "must be at most {MAX_BATCH_ID_CHARS} characters"
        ))),
        Some(_) => Err(invalid("must be a string")),
    }
}

/// The body's items, checked before any is looked up.
async fn read_items(request: Request) -> Result<Vec<Map<String, Value>>, Problem> {
    let (parts, body) = request.into_parts();
    if !is_json(&parts.headers) {
        return Err(Problem::unsupported_media_type());
    }
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > MAX_BATCH_BODY_BYTES as u64) {
        return Err(Problem::body_too_large(MAX_BATCH_BODY_BYTES));
    }
    // A chunked body declares no length, so the cap is held while reading
    // too.
    let bytes = to_bytes(body, MAX_BATCH_BODY_BYTES)
        .await
        .map_err(|error| {
            if is_length_limit(&error) {
                Problem::body_too_large(MAX_BATCH_BODY_BYTES)
            } else {
                Problem::malformed_body("the body could not be read")
            }
        })?;

    let not_an_array = || Problem::malformed_body("the body must be a JSON array of objects");
    let items: Vec<Value> = serde_json::from_slice(&bytes).map_err(|_| not_an_array())?;
    if items.len() > MAX_BATCH_ITEMS {
        return Err(Problem::too_many_items(MAX_BATCH_ITEMS));
    }
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| match item {
            Value::Object(item) => Ok(item),
            _ => Err(Problem::malformed_body(format!(
                "the body must be a JSON array of objects, and item {index} is not an object"
            ))),
        })
        .collect()
}

/// `application/json`, with or without parameters such as `charset`.
fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case(MEDIA_TYPE))
}

fn is_length_limit(error: &axum::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = error.source();
    while let Some(error) = source {
        if error.is::<LengthLimitError>() {
            return true;
        }
        source = error.source();
    }
    false
}

/// The headers every response of a batchable route carries, so a client can
/// find the batch methods from any answer.
pub(crate) async fn advertise(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::ALLOW, HeaderValue::from_static(METHODS));
    headers.insert("accept-query", HeaderValue::from_static(MEDIA_TYPE));
    response
}
