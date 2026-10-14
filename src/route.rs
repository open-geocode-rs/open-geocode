//! `open-geocode route`: one HTTP entry point in front of one Runtime per Pack.
//!
//! A Pack is built from one extract, and a Runtime serves one Pack, so a deployment covering
//! several countries runs several Runtimes. The router gives clients a single URL:
//!
//! - `/search`, `/autocomplete` with `country=XX` go, unchanged, to that country's Runtime.
//!   Without `country` they go to every Runtime at once; the results are merged by score and cut
//!   to `limit`, and each merged result carries the `country` it came from.
//! - `/reverse` needs `country`: a coordinate's country is only known to the Runtimes themselves.
//! - `/readyz` is ready only when every Runtime is.
//!
//! The router holds no Pack and does no geocoding. It keeps the Runtime's boundary policy: GET
//! only, Problem Details for errors, request-id propagation.

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use axum::{
    Extension, Router,
    body::Body,
    extract::{RawQuery, State},
    handler::{Handler, HandlerWithoutStateExt},
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{MethodFilter, MethodRouter, on},
};
use futures_util::future::join_all;
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::{
    http::{
        method,
        problem::Problem,
        request_id::{self, RequestId},
    },
    search::DEFAULT_SEARCH_LIMIT,
};

/// How long the router waits for one Runtime before answering 502 (or leaving it out of a merge).
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct RouteOptions {
    /// (country code, Runtime base URL) pairs, e.g. ("NZ", "http://127.0.0.1:8081").
    pub workers: Vec<(String, String)>,
    pub bind: SocketAddr,
}

#[derive(Clone)]
struct RouteState {
    /// Country code (upper case) -> Runtime base URL without a trailing slash.
    workers: Arc<BTreeMap<String, String>>,
    client: Client<HttpConnector, Empty<Bytes>>,
}

impl RouteState {
    fn new(workers: Vec<(String, String)>) -> Result<Self> {
        let mut map = BTreeMap::new();
        for (country, url) in workers {
            let country = country.trim().to_ascii_uppercase();
            if country.is_empty()
                || map
                    .insert(country.clone(), url.trim_end_matches('/').to_string())
                    .is_some()
            {
                bail!(
                    "each --worker needs a distinct non-empty country, got {country:?} twice or empty"
                );
            }
        }
        if map.is_empty() {
            bail!("route needs at least one --worker COUNTRY=URL");
        }
        Ok(Self {
            workers: Arc::new(map),
            client: Client::builder(TokioExecutor::new()).build_http(),
        })
    }

    fn known(&self) -> String {
        self.workers.keys().cloned().collect::<Vec<_>>().join(", ")
    }

    /// GET `url`: (status, content type, body), or None when the Runtime did not answer in time.
    async fn get(&self, url: &str) -> Option<(StatusCode, Option<HeaderValue>, Bytes)> {
        let uri = url.parse().ok()?;
        let response = tokio::time::timeout(UPSTREAM_TIMEOUT, self.client.get(uri))
            .await
            .ok()?
            .ok()?;
        let status = response.status();
        let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
        let body = tokio::time::timeout(UPSTREAM_TIMEOUT, response.into_body().collect())
            .await
            .ok()?
            .ok()?
            .to_bytes();
        Some((status, content_type, body))
    }
}

/// Parse `country=` and `limit=` out of a raw query string; the query itself is forwarded as is.
fn query_params(raw: Option<&str>) -> (Option<String>, usize) {
    let mut country = None;
    let mut limit = DEFAULT_SEARCH_LIMIT;
    for pair in raw.unwrap_or_default().split('&') {
        match pair.split_once('=') {
            Some(("country", value)) if !value.is_empty() => {
                country = Some(value.to_ascii_uppercase())
            }
            Some(("limit", value)) => limit = value.parse().unwrap_or(DEFAULT_SEARCH_LIMIT),
            _ => {}
        }
    }
    (country, limit)
}

/// Merge each Runtime's `key` array into one, best score first, cut to `limit`, each tagged with
/// its country. `answers` are (country, parsed response body) pairs.
fn merge(answers: Vec<(String, Value)>, key: &str, limit: usize) -> Value {
    let query = answers
        .iter()
        .find_map(|(_, body)| body.get("query").cloned())
        .unwrap_or(Value::Null);
    let mut merged: Vec<Value> = answers
        .into_iter()
        .flat_map(|(country, body)| {
            let items = body
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            items.into_iter().map(move |mut item| {
                if let Some(object) = item.as_object_mut() {
                    object.insert("country".to_string(), Value::String(country.clone()));
                }
                item
            })
        })
        .collect();
    let score = |item: &Value| {
        item.get("score")
            .and_then(Value::as_f64)
            .unwrap_or(f64::MIN)
    };
    merged.sort_by(|a, b| score(b).total_cmp(&score(a)));
    merged.truncate(limit);
    json!({ "query": query, key: merged })
}

fn with_id(problem: Problem, request_id: &Option<Extension<RequestId>>) -> Response {
    match request_id {
        Some(Extension(RequestId(id))) => problem.with_request_id(id.clone()).into_response(),
        None => problem.into_response(),
    }
}

/// Forward `path?raw` to one country's Runtime, or fan out and merge `key` when no country is given.
async fn forward_or_merge(
    state: &RouteState,
    path: &str,
    key: Option<&str>,
    raw: Option<String>,
    request_id: Option<Extension<RequestId>>,
) -> Response {
    let (country, limit) = query_params(raw.as_deref());
    let suffix = raw.map(|q| format!("?{q}")).unwrap_or_default();

    if let Some(country) = country {
        let Some(base) = state.workers.get(&country) else {
            return with_id(
                Problem::unknown_country(&country, &state.known()),
                &request_id,
            );
        };
        return match state.get(&format!("{base}{path}{suffix}")).await {
            Some((status, content_type, body)) => {
                let mut response = (status, Body::from(body)).into_response();
                if let Some(content_type) = content_type {
                    response
                        .headers_mut()
                        .insert(header::CONTENT_TYPE, content_type);
                }
                response
            }
            None => with_id(Problem::upstream_unavailable(&country), &request_id),
        };
    }

    let Some(key) = key else {
        return with_id(Problem::country_required(path), &request_id);
    };
    let calls = state.workers.iter().map(|(country, base)| {
        let url = format!("{base}{path}{suffix}");
        async move { (country.clone(), state.get(&url).await) }
    });
    let mut answers = Vec::new();
    for (country, answer) in join_all(calls).await {
        match answer {
            // A Runtime's 4xx (a bad query) is every Runtime's: hand the first one back as is.
            Some((status, content_type, body)) if status.is_client_error() => {
                let mut response = (status, Body::from(body)).into_response();
                if let Some(content_type) = content_type {
                    response
                        .headers_mut()
                        .insert(header::CONTENT_TYPE, content_type);
                }
                return response;
            }
            Some((status, _, body)) if status.is_success() => {
                if let Ok(value) = serde_json::from_slice::<Value>(&body) {
                    answers.push((country, value));
                }
            }
            // A Runtime that failed or did not answer is left out of the merge.
            _ => {}
        }
    }
    if answers.is_empty() {
        return with_id(Problem::upstream_unavailable("any"), &request_id);
    }
    axum::Json(merge(answers, key, limit)).into_response()
}

async fn search(
    State(state): State<RouteState>,
    request_id: Option<Extension<RequestId>>,
    RawQuery(raw): RawQuery,
) -> Response {
    forward_or_merge(&state, "/search", Some("results"), raw, request_id).await
}

async fn autocomplete(
    State(state): State<RouteState>,
    request_id: Option<Extension<RequestId>>,
    RawQuery(raw): RawQuery,
) -> Response {
    forward_or_merge(
        &state,
        "/autocomplete",
        Some("suggestions"),
        raw,
        request_id,
    )
    .await
}

async fn reverse(
    State(state): State<RouteState>,
    request_id: Option<Extension<RequestId>>,
    RawQuery(raw): RawQuery,
) -> Response {
    forward_or_merge(&state, "/reverse", None, raw, request_id).await
}

async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Ready only when every Runtime answers its own `/readyz` with 200.
async fn readyz(State(state): State<RouteState>) -> StatusCode {
    let calls = state.workers.values().map(|base| {
        let url = format!("{base}/readyz");
        let state = &state;
        async move { state.get(&url).await.map(|(status, _, _)| status) }
    });
    let all_ready = join_all(calls)
        .await
        .into_iter()
        .all(|status| status == Some(StatusCode::OK));
    if all_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

fn get_only<H, T>(handler: H) -> MethodRouter<RouteState>
where
    H: Handler<T, RouteState>,
    T: 'static,
{
    on(MethodFilter::GET, handler).on(MethodFilter::HEAD, method::method_not_allowed)
}

fn build_router(state: RouteState) -> Router {
    Router::new()
        .route("/search", get_only(search))
        .route("/autocomplete", get_only(autocomplete))
        .route("/reverse", get_only(reverse))
        .route("/healthz", get_only(healthz))
        .route("/readyz", get_only(readyz))
        .method_not_allowed_fallback(method::method_not_allowed)
        .fallback_service(method::not_found.into_service())
        .layer(middleware::from_fn(request_id::propagate))
        .with_state(state)
}

pub async fn route(options: RouteOptions) -> Result<()> {
    let state = RouteState::new(options.workers)?;
    println!("Routing {} at http://{}", state.known(), options.bind);
    let listener = TcpListener::bind(options.bind)
        .await
        .with_context(|| format!("failed to bind {}", options.bind))?;
    axum::serve(listener, build_router(state))
        .await
        .context("router failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in Runtime: answers /search with fixed results and /readyz with `ready`.
    async fn fake_runtime(results: Value, ready: StatusCode) -> String {
        let app = Router::new()
            .route(
                "/search",
                axum::routing::get(move || async move {
                    axum::Json(json!({ "query": "q", "results": results }))
                }),
            )
            .route("/readyz", axum::routing::get(move || async move { ready }));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        url
    }

    async fn router(workers: Vec<(&str, String)>) -> String {
        let state = RouteState::new(
            workers
                .into_iter()
                .map(|(c, u)| (c.to_string(), u))
                .collect(),
        )
        .expect("state");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, build_router(state))
                .await
                .expect("serve")
        });
        url
    }

    async fn get(url: &str) -> (StatusCode, Value) {
        let client: Client<HttpConnector, Empty<Bytes>> =
            Client::builder(TokioExecutor::new()).build_http();
        let response = client
            .get(url.parse().expect("uri"))
            .await
            .expect("response");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn country_goes_to_its_runtime_and_none_merges_by_score() {
        let au = fake_runtime(
            json!([{ "label": "au-a", "score": 5.0 }, { "label": "au-b", "score": 1.0 }]),
            StatusCode::OK,
        )
        .await;
        let nz = fake_runtime(json!([{ "label": "nz-a", "score": 3.0 }]), StatusCode::OK).await;
        let base = router(vec![("AU", au), ("nz", nz)]).await;

        let (status, body) = get(&format!("{base}/search?q=x&country=NZ")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["label"], "nz-a");
        assert!(
            body["results"][0].get("country").is_none(),
            "a forwarded answer is passed through untouched"
        );

        let (status, body) = get(&format!("{base}/search?q=x&limit=2")).await;
        assert_eq!(status, StatusCode::OK);
        let labels: Vec<_> = body["results"]
            .as_array()
            .expect("results")
            .iter()
            .map(|r| r["label"].clone())
            .collect();
        assert_eq!(labels, vec![json!("au-a"), json!("nz-a")]);
        assert_eq!(body["results"][1]["country"], "NZ");
    }

    #[tokio::test]
    async fn unknown_country_and_reverse_without_country_are_problems() {
        let au = fake_runtime(json!([]), StatusCode::OK).await;
        let base = router(vec![("AU", au)]).await;

        let (status, body) = get(&format!("{base}/search?q=x&country=FR")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error_code"], "unknown_country");

        let (status, body) = get(&format!("{base}/reverse?lon=1&lat=2")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error_code"], "country_required");
    }

    #[tokio::test]
    async fn ready_only_when_every_runtime_is() {
        let up = fake_runtime(json!([]), StatusCode::OK).await;
        let down = fake_runtime(json!([]), StatusCode::SERVICE_UNAVAILABLE).await;
        let (status, _) = get(&format!(
            "{}/readyz",
            router(vec![("AU", up.clone())]).await
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = get(&format!(
            "{}/readyz",
            router(vec![("AU", up), ("NZ", down)]).await
        ))
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn query_params_read_country_and_limit() {
        assert_eq!(
            query_params(Some("q=x&country=nz&limit=3")),
            (Some("NZ".to_string()), 3)
        );
        assert_eq!(query_params(None), (None, DEFAULT_SEARCH_LIMIT));
    }
}
