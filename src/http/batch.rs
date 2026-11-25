use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, atomic::AtomicBool},
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
        bounds::MAX_BATCH_ID_CHARS,
        problem::{InvalidParam, Problem},
        request_id::RequestId,
    },
    limits::Limits,
    runtime::AppState,
    stop::{self, CancelOnDrop, Stop},
};

pub(crate) const METHODS: &str = "GET, QUERY, POST";

pub(crate) const MEDIA_TYPE: &str = "application/json";

const RETRY_AFTER_SECONDS: u32 = 1;

pub(crate) struct Lookup<P, Q, A, K> {
    pub(crate) route: &'static str,
    pub(crate) prepare: fn(&AppState, P) -> Result<Q, Problem>,
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

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Item<A> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<String>,
    pub(crate) status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) response: Option<A>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<Problem>,
}

impl<A> Item<A> {
    pub(crate) fn failed(id: Option<String>, error: Problem) -> Self {
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

pub(crate) enum RawItem {
    Fields(Vec<(String, Value)>),
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

pub(crate) enum Prepared<Q> {
    Ready {
        id: Option<String>,
        key: String,
        query: Q,
    },
    Failed {
        id: Option<String>,
        problem: Problem,
    },
}

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
    let items = read_items(request, state.limits()).await?;
    let Ok(permit) = Arc::clone(state.batch_slots()).try_acquire_owned() else {
        return Err(Problem::busy(RETRY_AFTER_SECONDS));
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let _cancel = CancelOnDrop(Arc::clone(&cancel));
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

fn answer_items<P, Q, A, K>(
    state: &AppState,
    lookup: Lookup<P, Q, A, K>,
    shared: &[(String, String)],
    items: Vec<RawItem>,
    cancel: &Arc<AtomicBool>,
) -> crate::Result<Vec<Item<A>>>
where
    P: DeserializeOwned,
    Q: Send + Sync + Clone,
    A: Send,
    K: Ord,
{
    let prepared = items
        .into_iter()
        .map(|item| prepare(state, lookup, shared, item))
        .collect::<Vec<_>>();
    map_in_order(
        &prepared,
        |item| match item {
            Prepared::Ready { query, .. } => Some((lookup.place)(query)),
            Prepared::Failed { .. } => None,
        },
        cancel,
        |_, item| {
            Ok(match item {
                Prepared::Ready { id, query, .. } => {
                    look_up(state, lookup, id.clone(), query.clone(), cancel)
                }
                Prepared::Failed { id, problem } => Item::failed(id.clone(), problem.clone()),
            })
        },
    )
}

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
                    error.cause().map(|cause| format!("{cause:#}")).as_deref(),
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

pub(crate) fn prepare<P, Q, A, K>(
    state: &AppState,
    lookup: Lookup<P, Q, A, K>,
    shared: &[(String, String)],
    item: RawItem,
) -> Prepared<Q>
where
    P: DeserializeOwned,
{
    let fields = match item {
        RawItem::Fields(fields) => fields,
        RawItem::Repeated => {
            return Prepared::Failed {
                id: None,
                problem: Problem::unparsable_parameters(),
            };
        }
        RawItem::NotAnObject => {
            return Prepared::Failed {
                id: None,
                problem: Problem::malformed_body("an item must be a JSON object"),
            };
        }
    };
    let id = match item_id(&fields) {
        Ok(id) => id,
        Err(problem) => return Prepared::Failed { id: None, problem },
    };
    let checked = catch_unwind(AssertUnwindSafe(|| -> Result<(String, Q), Problem> {
        let key = item_query(shared, &fields)?;
        let params =
            serde_urlencoded::from_str::<P>(&key).map_err(|_| Problem::unparsable_parameters())?;
        Ok((key, (lookup.prepare)(state, params)?))
    }))
    .unwrap_or_else(|payload| Err(Problem::panicked(payload.as_ref())));
    match checked {
        Ok((key, query)) => Prepared::Ready { id, key, query },
        Err(problem) => Prepared::Failed { id, problem },
    }
}

pub(crate) fn look_up<P, Q, A, K>(
    state: &AppState,
    lookup: Lookup<P, Q, A, K>,
    id: Option<String>,
    query: Q,
    cancel: &Arc<AtomicBool>,
) -> Item<A> {
    let stop = Stop::new(state.limits().lookup_timeout(), Arc::clone(cancel));
    let outcome = stop::within(stop, || {
        catch_unwind(AssertUnwindSafe(|| {
            (lookup.answer)(state, query).map_err(Problem::from)
        }))
    })
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

pub(crate) fn shared_parameters(query: Option<&str>) -> Result<Vec<(String, String)>, Problem> {
    serde_urlencoded::from_str(query.unwrap_or_default())
        .map_err(|_| Problem::unparsable_parameters())
}

fn item_query(shared: &[(String, String)], item: &[(String, Value)]) -> Result<String, Problem> {
    let mut own = Vec::with_capacity(item.len());
    for (name, value) in item {
        let value = match value {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
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

async fn read_items(request: Request, limits: &Limits) -> Result<Vec<RawItem>, Problem> {
    let max_bytes = limits.max_batch_body_bytes();
    if !is_json(request.headers()) {
        return Err(Problem::unsupported_media_type(MEDIA_TYPE));
    }
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > max_bytes as u64) {
        return Err(Problem::body_too_large(max_bytes));
    }
    let bytes = Bytes::from_request(request, &())
        .await
        .map_err(|rejection| match rejection {
            BytesRejection::FailedToBufferBody(FailedToBufferBody::LengthLimitError(_)) => {
                Problem::body_too_large(max_bytes)
            }
            _ => Problem::malformed_body("the body could not be read"),
        })?;

    let items: Vec<RawItem> = serde_json::from_slice(&bytes)
        .map_err(|_| Problem::malformed_body("the body must be a JSON array of objects"))?;
    if items.len() > limits.max_batch_items() {
        return Err(Problem::too_many_items(limits.max_batch_items()));
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

fn is_json(headers: &HeaderMap) -> bool {
    media_type(headers).is_some_and(|media_type| media_type.eq_ignore_ascii_case(MEDIA_TYPE))
}

pub(crate) fn media_type(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
}

pub(crate) async fn advertise(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::ALLOW, HeaderValue::from_static(METHODS));
    headers.insert("accept-query", HeaderValue::from_static(MEDIA_TYPE));
    response
}
