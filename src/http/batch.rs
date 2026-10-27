//! Batch lookups: many items in one request, on the routes that answer one.
//!
//! `QUERY` (RFC 10008) is the batch method, and `POST` an alias for clients
//! that cannot send `QUERY` yet. Each item is read exactly as a `GET` of the
//! same route reads its query string, so an item's answer, or its error, is
//! the one that `GET` would return. A bad item never fails the batch.

use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use axum::{
    body::Bytes,
    extract::{
        FromRequest, Request,
        rejection::{BytesRejection, FailedToBufferBody},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{DeserializeOwned, IgnoredAny, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
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

/// Seconds a refused batch is told to wait. Batches of 1000 items finish in
/// well under that on a warm Pack.
const RETRY_AFTER_SECONDS: u32 = 1;

/// One route's lookup, split where a batch needs it: `prepare` checks
/// parameters by the rules `GET` checks them with, `place` says where the
/// lookup will read in the Pack, and `answer` looks it up. The `GET` handler
/// runs `prepare` and `answer` too.
pub(crate) struct Lookup<P, Q, A, K> {
    pub(crate) route: &'static str,
    pub(crate) prepare: fn(P) -> Result<Q, Problem>,
    pub(crate) place: fn(&Q) -> K,
    pub(crate) answer: fn(&AppState, Q) -> crate::Result<A>,
}

// By hand: a derive would demand the type parameters be `Copy` too, though
// only function pointers are held.
impl<P, Q, A, K> Clone for Lookup<P, Q, A, K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<P, Q, A, K> Copy for Lookup<P, Q, A, K> {}

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

/// An item as sent: its fields in order, or what made it unreadable.
enum RawItem {
    Fields(Vec<(String, Value)>),
    /// A field named twice. `GET` with a repeated parameter fails to parse,
    /// so the item fails the same way rather than keeping the last value.
    Repeated,
    NotAnObject,
}

impl<'de> Deserialize<'de> for RawItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(RawItemVisitor)
    }
}

struct RawItemVisitor;

impl<'de> Visitor<'de> for RawItemVisitor {
    type Value = RawItem;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<RawItem, M::Error> {
        let mut fields = Vec::<(String, Value)>::new();
        let mut repeated = false;
        while let Some((name, value)) = map.next_entry::<String, Value>()? {
            repeated |= fields.iter().any(|(seen, _)| *seen == name);
            fields.push((name, value));
        }
        Ok(if repeated {
            RawItem::Repeated
        } else {
            RawItem::Fields(fields)
        })
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<RawItem, S::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(RawItem::NotAnObject)
    }

    fn visit_bool<E>(self, _: bool) -> Result<RawItem, E> {
        Ok(RawItem::NotAnObject)
    }

    fn visit_i64<E>(self, _: i64) -> Result<RawItem, E> {
        Ok(RawItem::NotAnObject)
    }

    fn visit_u64<E>(self, _: u64) -> Result<RawItem, E> {
        Ok(RawItem::NotAnObject)
    }

    fn visit_f64<E>(self, _: f64) -> Result<RawItem, E> {
        Ok(RawItem::NotAnObject)
    }

    fn visit_str<E>(self, _: &str) -> Result<RawItem, E> {
        Ok(RawItem::NotAnObject)
    }

    fn visit_unit<E>(self) -> Result<RawItem, E> {
        Ok(RawItem::NotAnObject)
    }
}

/// An item checked and ready to look up, or already failed.
enum Prepared<Q> {
    Ready(Option<String>, Q),
    Failed(Option<String>, Problem),
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
pub(crate) async fn run<P, Q, A, K>(
    state: AppState,
    lookup: Lookup<P, Q, A, K>,
    request: Request,
) -> Response
where
    P: DeserializeOwned + 'static,
    Q: Send + Sync + Clone + 'static,
    A: Serialize + Send + 'static,
    K: Ord + 'static,
{
    match answer(state, lookup, request).await {
        Ok(response) => response,
        Err(problem) => problem.into_response(),
    }
}

async fn answer<P, Q, A, K>(
    state: AppState,
    lookup: Lookup<P, Q, A, K>,
    request: Request,
) -> Result<Response, Problem>
where
    P: DeserializeOwned + 'static,
    Q: Send + Sync + Clone + 'static,
    A: Serialize + Send + 'static,
    K: Ord + 'static,
{
    let started = Instant::now();
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone());
    let shared = shared_parameters(request.uri().query())?;
    let items = read_items(request).await?;
    // Taken before any thread is, so a refused batch holds nothing while it
    // waits to be told no.
    let Ok(permit) = Arc::clone(state.batch_slots()).try_acquire_owned() else {
        return Err(Problem::busy(RETRY_AFTER_SECONDS));
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let _stop = StopOnDrop(Arc::clone(&cancel));
    let worker = state.clone();
    let body = task::spawn_blocking(move || {
        let _permit = permit;
        worker.batch_pool().install(|| {
            let batch = answer_items(&worker, lookup, &shared, items, &cancel)?;
            let batch = finish(&worker, lookup.route, request_id.as_deref(), started, batch);
            serde_json::to_vec(&batch).map_err(crate::Error::from)
        })
    })
    .await?
    .map_err(Problem::from)?;

    Ok((
        [(header::CONTENT_TYPE, HeaderValue::from_static(MEDIA_TYPE))],
        body,
    )
        .into_response())
}

/// Every item's result, in input order. Items are looked up in the order of
/// where they read in the Pack, so neighbours share the pages they fault in.
fn answer_items<P, Q, A, K>(
    state: &AppState,
    lookup: Lookup<P, Q, A, K>,
    shared: &[(String, String)],
    items: Vec<RawItem>,
    cancel: &AtomicBool,
) -> crate::Result<Vec<Item<A>>>
where
    P: DeserializeOwned,
    Q: Send + Sync + Clone,
    A: Send,
    K: Ord,
{
    let prepared = items
        .into_iter()
        .map(|item| prepare(lookup, shared, item))
        .collect::<Vec<_>>();
    map_in_order(
        &prepared,
        |item| match item {
            Prepared::Ready(_, query) => Some((lookup.place)(query)),
            Prepared::Failed(..) => None,
        },
        cancel,
        |_, item| {
            Ok(match item {
                Prepared::Ready(id, query) => look_up(state, lookup, id.clone(), query.clone()),
                Prepared::Failed(id, problem) => Item::failed(id.clone(), problem.clone()),
            })
        },
    )
}

/// Count the outcomes, and log and stamp the failures. Inside a 200 the
/// request-id middleware sees no Problem, so the batch does both itself.
fn finish<A>(
    state: &AppState,
    route: &str,
    request_id: Option<&str>,
    started: Instant,
    results: Vec<Item<A>>,
) -> Batch<A> {
    let mut summary = Summary {
        total: results.len(),
        ..Summary::default()
    };
    let results = results
        .into_iter()
        .enumerate()
        .map(|(index, mut item)| {
            let Some(mut error) = item.error.take() else {
                summary.succeeded += 1;
                return item;
            };
            summary.failed += 1;
            if error.status() == StatusCode::INTERNAL_SERVER_ERROR.as_u16() {
                state.error_log().internal_error(
                    request_id.unwrap_or("-"),
                    route,
                    Some(index),
                    started,
                    error.cause().map(|cause| format!("{cause:#}")),
                );
            }
            if let Some(id) = request_id {
                error = error.with_request_id(id);
            }
            item.error = Some(error);
            item
        })
        .collect();
    Batch { results, summary }
}

/// Check one item by the rules its `GET` is checked with. A panic fails the
/// item, not the batch.
fn prepare<P, Q, A, K>(
    lookup: Lookup<P, Q, A, K>,
    shared: &[(String, String)],
    item: RawItem,
) -> Prepared<Q>
where
    P: DeserializeOwned,
{
    let fields = match item {
        RawItem::Fields(fields) => fields,
        RawItem::Repeated => return Prepared::Failed(None, Problem::unparsable_parameters()),
        RawItem::NotAnObject => unreachable!("read_items rejects a body with one"),
    };
    let id = match item_id(&fields) {
        Ok(id) => id,
        Err(problem) => return Prepared::Failed(None, problem),
    };
    let checked = catch_unwind(AssertUnwindSafe(|| -> Result<Q, Problem> {
        let query = item_query(shared, &fields)?;
        let params = serde_urlencoded::from_str::<P>(&query)
            .map_err(|_| Problem::unparsable_parameters())?;
        (lookup.prepare)(params)
    }))
    .unwrap_or_else(|payload| Err(Problem::panicked(payload.as_ref())));
    match checked {
        Ok(query) => Prepared::Ready(id, query),
        Err(problem) => Prepared::Failed(id, problem),
    }
}

/// One item looked up. A panic fails the item, not the batch.
fn look_up<P, Q, A, K>(
    state: &AppState,
    lookup: Lookup<P, Q, A, K>,
    id: Option<String>,
    query: Q,
) -> Item<A> {
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        (lookup.answer)(state, query).map_err(Problem::from)
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
fn item_query(shared: &[(String, String)], item: &[(String, Value)]) -> Result<String, Problem> {
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
                    "{name} must be a string, number or boolean"
                ))
                .with_invalid_params(vec![InvalidParam::new(
                    name.as_str(),
                    "not a string, number or boolean",
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
fn item_id(item: &[(String, Value)]) -> Result<Option<String>, Problem> {
    let invalid = |reason: &str| {
        Problem::malformed_parameter(format!("id {reason}"))
            .with_invalid_params(vec![InvalidParam::new("id", reason)])
    };
    match item.iter().find(|(name, _)| name == "id").map(|(_, id)| id) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) if id.chars().count() <= MAX_BATCH_ID_CHARS => Ok(Some(id.clone())),
        Some(Value::String(_)) => Err(invalid(&format!(
            "must be at most {MAX_BATCH_ID_CHARS} characters"
        ))),
        Some(_) => Err(invalid("must be a string")),
    }
}

/// The body's items, checked before any is looked up.
async fn read_items(request: Request) -> Result<Vec<RawItem>, Problem> {
    if !is_json(request.headers()) {
        return Err(Problem::unsupported_media_type());
    }
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    // A declared length is refused before a byte is read. A chunked body
    // declares none, and the route's body limit stops it while it is read.
    if declared.is_some_and(|length| length > MAX_BATCH_BODY_BYTES as u64) {
        return Err(Problem::body_too_large(MAX_BATCH_BODY_BYTES));
    }
    let bytes = Bytes::from_request(request, &())
        .await
        .map_err(|rejection| match rejection {
            BytesRejection::FailedToBufferBody(FailedToBufferBody::LengthLimitError(_)) => {
                Problem::body_too_large(MAX_BATCH_BODY_BYTES)
            }
            _ => Problem::malformed_body("the body could not be read"),
        })?;

    let items: Vec<RawItem> = serde_json::from_slice(&bytes)
        .map_err(|_| Problem::malformed_body("the body must be a JSON array of objects"))?;
    if items.len() > MAX_BATCH_ITEMS {
        return Err(Problem::too_many_items(MAX_BATCH_ITEMS));
    }
    if let Some(index) = items
        .iter()
        .position(|item| matches!(item, RawItem::NotAnObject))
    {
        return Err(Problem::malformed_body(format!(
            "the body must be a JSON array of objects, and item {index} is not an object"
        )));
    }
    Ok(items)
}

/// `application/json`, with or without parameters such as `charset`.
fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case(MEDIA_TYPE))
}

/// The headers every response of a batchable route carries, so a client can
/// find the batch methods from any answer.
pub(crate) async fn advertise(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::ALLOW, HeaderValue::from_static(METHODS));
    headers.insert("accept-query", HeaderValue::from_static(MEDIA_TYPE));
    response
}
