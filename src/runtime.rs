use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result};
use axum::{
    Extension, Json, Router,
    extract::{Query, State},
    handler::{Handler, HandlerWithoutStateExt},
    middleware,
    routing::{MethodFilter, MethodRouter, on},
};
use serde::Deserialize;
use tokio::{net::TcpListener, task};
use tower_http::services::{ServeDir, ServeFile};

use crate::{
    api::{self, ForwardCollection, ReverseCollection},
    http::{
        bounds, health, method,
        problem::{Problem, classify_search_error},
        request_id::{self, RequestId},
    },
    pack::PackReader,
    resources::Resources,
    reverse::{PackReverseGeocoder, ReverseGeocodeOptions},
    search::{PackTextSearcher, TextAutocompleteOptions, TextSearchOptions},
};

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub pack: PathBuf,
    pub demo: PathBuf,
    pub bind: SocketAddr,
    pub basemap: PathBuf,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pack: Arc<PackReader>,
    searcher: Arc<PackTextSearcher>,
    reverse_geocoder: Arc<PackReverseGeocoder>,
    ready: Arc<AtomicBool>,
}

impl AppState {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let pack = Arc::new(PackReader::open(path)?);
        // Every query looks things up all over the Pack: read it in now
        // when it fits, so the first queries do not each wait on faults.
        pack.container()
            .advise_random_reads(Resources::probe().available_memory_bytes);
        let searcher = PackTextSearcher::from_pack(Arc::clone(&pack))?;
        let reverse_geocoder = PackReverseGeocoder::from_pack(Arc::clone(&pack))?;
        Ok(Self {
            pack,
            searcher: Arc::new(searcher),
            reverse_geocoder: Arc::new(reverse_geocoder),
            ready: Arc::new(AtomicBool::new(true)),
        })
    }

    /// True once the Pack is loaded and the Runtime can answer API traffic
    /// (ADR 0017 Decision 28).
    pub(crate) fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    q: Option<String>,
    #[serde(default)]
    limit: usize,
    layer: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReverseParams {
    lon: f64,
    lat: f64,
}

pub async fn serve(options: ServeOptions) -> Result<()> {
    let state = AppState::open(&options.pack)
        .with_context(|| format!("failed to open Pack {}", options.pack.display()))?;
    let app = build_router(state, &options.demo, &options.basemap);

    let listener = TcpListener::bind(options.bind)
        .await
        .with_context(|| format!("failed to bind {}", options.bind))?;
    println!(
        "Serving {} at http://{}",
        options.pack.display(),
        options.bind
    );
    axum::serve(listener, app)
        .await
        .context("runtime server failed")
}

/// Assemble the public-demo router with its boundary policy: method-restricted
/// routes (ADR 0017 Decision 27), Problem Details 404/405 fallbacks
/// (Decisions 27, 37), and request-id propagation (Decision 30). The basemap is
/// served raw with no guard — in the deployed topology PMTiles is served from
/// Cloudflare R2, not the Runtime (Decisions 6/35a, amended 2026-06-03).
///
/// Split out from [`serve`] so it can be exercised without binding a socket.
pub(crate) fn build_router(state: AppState, demo: &Path, basemap: &Path) -> Router {
    // Read-only API and health routes: GET only (Decision 27). axum would
    // otherwise route HEAD to the GET handler automatically, so `get_only`
    // registers an explicit HEAD responder that returns 405. Other methods fall
    // through to `method_not_allowed_fallback`.
    let mut app: Router<AppState> = Router::new()
        .route("/search", get_only(search))
        .route("/autocomplete", get_only(autocomplete))
        .route("/reverse", get_only(reverse))
        .route("/healthz", get_only(health::healthz))
        .route("/readyz", get_only(health::readyz));

    // The basemap PMTiles lives with the other data artifacts (not in the demo
    // dir), so serve it explicitly. ServeFile honors HTTP range requests, which
    // is what the PMTiles client uses. Skip it if the file is absent so a fresh
    // clone without a basemap still serves the API and demo.
    //
    // This is served raw and is a local-dev convenience only. In the public
    // demo the basemap is served from Cloudflare R2, not the Runtime (ADR 0017
    // Decisions 6/7/8, amended 2026-06-03); R2's free egress makes PMTiles
    // abuse protection a non-issue. The bare binary hands back the file with
    // native range support and no policy.
    if basemap.exists() {
        app = app.route_service("/basemap.pmtiles", ServeFile::new(basemap));
        println!("Serving basemap {}", basemap.display());
    } else {
        eprintln!(
            "basemap {} not found; serving demo without a basemap",
            basemap.display()
        );
    }

    // ServeDir keeps local-dev static serving (Decision 10) but its built-in 404
    // is replaced with a Problem Details 404 so unknown paths don't fall through
    // to a static-file response (Decision 37). `method_not_allowed_fallback`
    // turns method mismatches into a Problem 405 instead of an empty body. The
    // request-id layer is outermost so it also wraps both fallbacks.
    app.method_not_allowed_fallback(method::method_not_allowed)
        .fallback_service(ServeDir::new(demo).not_found_service(method::not_found.into_service()))
        .layer(middleware::from_fn(request_id::propagate))
        .with_state(state)
}

/// A GET-only route that also rejects HEAD with 405, defeating axum's default
/// HEAD-to-GET routing so the public method surface stays exactly GET.
fn get_only<H, T>(handler: H) -> MethodRouter<AppState>
where
    H: Handler<T, AppState>,
    T: 'static,
{
    on(MethodFilter::GET, handler).on(MethodFilter::HEAD, method::method_not_allowed)
}

/// Attach the request id (if present) to a Problem so it appears in the body.
fn with_id(problem: Problem, request_id: &Option<String>) -> Problem {
    match request_id {
        Some(id) => problem.with_request_id(id.clone()),
        None => problem,
    }
}

/// A bare 500 for the client, with the cause logged here under the request id: the body must not
/// carry it (ADR 0017 Decision 31), so the log is the only place it survives.
fn internal(cause: impl std::fmt::Display, request_id: &Option<String>) -> Problem {
    eprintln!(
        "internal error (request {}): {cause:#}",
        request_id.as_deref().unwrap_or("-")
    );
    with_id(Problem::internal(), request_id)
}

/// A search error as a Problem, logging the cause when it is ours rather than the query's.
fn search_error(error: anyhow::Error, request_id: &Option<String>) -> Problem {
    let cause = format!("{error:#}");
    let problem = classify_search_error(error);
    if problem.is_internal() {
        return internal(cause, request_id);
    }
    with_id(problem, request_id)
}

async fn search(
    State(state): State<AppState>,
    request_id: Option<Extension<RequestId>>,
    params: Result<Query<SearchParams>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<ForwardCollection>, Problem> {
    let request_id = request_id.map(|Extension(RequestId(id))| id);
    let Query(params) = params.map_err(|_| {
        with_id(
            Problem::malformed_parameter("could not parse query parameters"),
            &request_id,
        )
    })?;

    let query = bounds::validate_query(params.q.as_deref().unwrap_or_default())
        .map_err(|problem| with_id(problem, &request_id))?;
    bounds::validate_search_limit(params.limit).map_err(|problem| with_id(problem, &request_id))?;

    let options = TextSearchOptions {
        query: query.clone(),
        limit: params.limit,
        layer: params.layer,
    };
    let collection = task::spawn_blocking(move || {
        let hits = state.searcher.search(options)?;
        api::forward_collection(&state.pack, query, hits)
    })
    .await
    .map_err(|error| internal(error, &request_id))?
    .map_err(|error| search_error(error, &request_id))?;

    Ok(Json(collection))
}

async fn autocomplete(
    State(state): State<AppState>,
    request_id: Option<Extension<RequestId>>,
    params: Result<Query<SearchParams>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<ForwardCollection>, Problem> {
    let request_id = request_id.map(|Extension(RequestId(id))| id);
    let Query(params) = params.map_err(|_| {
        with_id(
            Problem::malformed_parameter("could not parse query parameters"),
            &request_id,
        )
    })?;

    // Autocomplete tolerates an empty/short query (it returns no suggestions), so
    // only bound the length here; the searcher caps the result count internally.
    let query = bounds::validate_query_length(params.q.as_deref().unwrap_or_default())
        .map_err(|problem| with_id(problem, &request_id))?;
    bounds::validate_autocomplete_limit(params.limit)
        .map_err(|problem| with_id(problem, &request_id))?;

    let options = TextAutocompleteOptions {
        query: query.clone(),
        limit: params.limit,
        layer: params.layer,
    };
    let collection = task::spawn_blocking(move || {
        let hits = state.searcher.autocomplete(options)?;
        api::forward_collection(&state.pack, query, hits)
    })
    .await
    .map_err(|error| internal(error, &request_id))?
    .map_err(|error| search_error(error, &request_id))?;

    Ok(Json(collection))
}

async fn reverse(
    State(state): State<AppState>,
    request_id: Option<Extension<RequestId>>,
    params: Result<Query<ReverseParams>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<ReverseCollection>, Problem> {
    let request_id = request_id.map(|Extension(RequestId(id))| id);
    let Query(params) = params.map_err(|_| {
        with_id(
            Problem::malformed_parameter("could not parse query parameters"),
            &request_id,
        )
    })?;

    bounds::validate_coords(params.lon, params.lat)
        .map_err(|problem| with_id(problem, &request_id))?;

    let collection = task::spawn_blocking(move || {
        let response = state.reverse_geocoder.reverse(ReverseGeocodeOptions {
            lon: params.lon,
            lat: params.lat,
        })?;
        api::reverse_collection(&state.pack, &response)
    })
    .await
    .map_err(|error| internal(error, &request_id))?
    .map_err(|error| internal(error, &request_id))?;

    Ok(Json(collection))
}

#[cfg(test)]
mod router_tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;
    use crate::pack::PackWriter;
    use crate::record::{
        AddressComponents, AddressRecord, LocationPrecision, OsmObjectType, SourceProvenance,
        point_geometry,
    };

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("open-geocode-http-{name}-{}", std::process::id()))
    }

    fn write_pack(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
        let mut writer = PackWriter::create(dir).expect("writer");
        writer
            .write(
                &AddressRecord {
                    address: AddressComponents {
                        areas: Vec::new(),
                        number: "10".to_string(),
                        street: Some("King Street".to_string()),
                        place: None,
                        unit: None,
                        locality: Some("Toronto".to_string()),
                        region: None,
                        postcode: Some("M5V 1A1".to_string()),
                        country: None,
                    },
                    geometry: point_geometry(-79.0, 43.0),
                    location_precision: LocationPrecision::Point,
                    source: SourceProvenance {
                        dataset: "osm".to_string(),
                        object_type: OsmObjectType::Node,
                        object_id: 1,
                        tags: Some(BTreeMap::new()),
                    },
                }
                .into(),
                None,
            )
            .expect("write address");
        writer.finish().expect("finish");
    }

    fn state_for(pack_dir: &Path, ready: bool) -> AppState {
        let state = AppState::open(pack_dir).expect("open shared pack");
        state.ready.store(ready, Ordering::Release);
        state
    }

    fn request(method: &str, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    struct Reply {
        status: StatusCode,
        headers: axum::http::HeaderMap,
        body: Vec<u8>,
    }

    impl Reply {
        fn content_type(&self) -> &str {
            self.headers
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
        }

        fn json(&self) -> serde_json::Value {
            serde_json::from_slice(&self.body).expect("response body is JSON")
        }

        fn request_id(&self) -> Option<String> {
            self.headers
                .get("x-request-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        }
    }

    async fn send(router: Router, request: Request<Body>) -> Reply {
        let response = router.oneshot(request).await.expect("router response");
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body")
            .to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }

    #[tokio::test]
    async fn health_endpoints_reflect_readiness() {
        let pack = temp_path("health");
        write_pack(&pack);
        let demo = temp_path("health-demo");
        let no_basemap = temp_path("health-no-basemap");

        let router = build_router(state_for(&pack, true), &demo, &no_basemap);
        assert_eq!(
            send(router.clone(), request("GET", "/healthz"))
                .await
                .status,
            StatusCode::OK
        );
        assert_eq!(
            send(router, request("GET", "/readyz")).await.status,
            StatusCode::OK
        );

        let not_ready = build_router(state_for(&pack, false), &demo, &no_basemap);
        assert_eq!(
            send(not_ready, request("GET", "/readyz")).await.status,
            StatusCode::SERVICE_UNAVAILABLE
        );

        let _ = std::fs::remove_dir_all(&pack);
    }

    #[tokio::test]
    async fn search_enforces_query_and_limit_bounds() {
        let pack = temp_path("search-bounds");
        write_pack(&pack);
        let demo = temp_path("search-demo");
        let no_basemap = temp_path("search-no-basemap");
        let router = build_router(state_for(&pack, true), &demo, &no_basemap);

        // Empty query -> 400 invalid_query, Problem Details.
        let reply = send(router.clone(), request("GET", "/search?q=")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.content_type(), "application/problem+json");
        assert_eq!(reply.json()["error_code"], "invalid_query");

        // Over-length query -> 400.
        let long = "a".repeat(300);
        let reply = send(router.clone(), request("GET", &format!("/search?q={long}"))).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);

        // limit above the cap -> 400 with invalid_params naming "limit".
        let reply = send(
            router.clone(),
            request("GET", "/search?q=king&limit=999999"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["invalid_params"][0]["name"], "limit");

        // Malformed limit type -> 400 malformed_parameter.
        let reply = send(router.clone(), request("GET", "/search?q=king&limit=abc")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["error_code"], "malformed_parameter");

        // A valid query succeeds.
        let reply = send(router, request("GET", "/search?q=King%20Street&limit=5")).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.content_type(), "application/json");

        let _ = std::fs::remove_dir_all(&pack);
    }

    #[tokio::test]
    async fn reverse_validates_coordinates() {
        let pack = temp_path("reverse-bounds");
        write_pack(&pack);
        let demo = temp_path("reverse-demo");
        let no_basemap = temp_path("reverse-no-basemap");
        let router = build_router(state_for(&pack, true), &demo, &no_basemap);

        let reply = send(router.clone(), request("GET", "/reverse?lon=999&lat=0")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["invalid_params"][0]["name"], "lon");

        let reply = send(router.clone(), request("GET", "/reverse?lon=0&lat=999")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["invalid_params"][0]["name"], "lat");

        let reply = send(router, request("GET", "/reverse?lon=-79.0&lat=43.0")).await;
        assert_eq!(reply.status, StatusCode::OK);

        let _ = std::fs::remove_dir_all(&pack);
    }

    #[tokio::test]
    async fn methods_unknown_paths_and_request_ids() {
        let pack = temp_path("methods");
        write_pack(&pack);
        let demo = temp_path("methods-demo");
        let no_basemap = temp_path("methods-no-basemap");
        let router = build_router(state_for(&pack, true), &demo, &no_basemap);

        // POST to a GET-only route -> 405 with Allow: GET.
        let reply = send(router.clone(), request("POST", "/search")).await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(reply.content_type(), "application/problem+json");
        assert_eq!(
            reply
                .headers
                .get(header::ALLOW)
                .and_then(|v| v.to_str().ok()),
            Some("GET")
        );

        // HEAD is not part of the API method surface (Decision 27) -> 405.
        let reply = send(router.clone(), request("HEAD", "/search")).await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);

        // Unknown path -> 404 Problem Details, not a static-file response.
        let reply = send(router.clone(), request("GET", "/does-not-exist")).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.content_type(), "application/problem+json");
        assert_eq!(reply.json()["error_code"], "not_found");

        // Every response carries a request id; an inbound cf-ray is honored.
        let reply = send(router.clone(), request("GET", "/healthz")).await;
        assert!(reply.request_id().is_some());

        let mut tagged = request("GET", "/search?q=");
        tagged
            .headers_mut()
            .insert("cf-ray", header::HeaderValue::from_static("ray-abc-123"));
        let reply = send(router, tagged).await;
        assert_eq!(reply.request_id().as_deref(), Some("ray-abc-123"));
        assert_eq!(reply.json()["request_id"], "ray-abc-123");

        let _ = std::fs::remove_dir_all(&pack);
    }

    #[tokio::test]
    async fn basemap_is_served_raw_with_range_support() {
        // The runtime serves the basemap raw: range requests work natively and
        // there is deliberately no full-file fetch guard here (that is a
        // deployment-layer concern, ADR 0017 Decision 35a). The bare binary hands
        // back exactly what was asked for.
        let pack = temp_path("basemap");
        write_pack(&pack);
        let demo = temp_path("basemap-demo");
        let basemap = temp_path("basemap.pmtiles");
        std::fs::write(&basemap, vec![0u8; 600_000]).expect("write basemap");

        let router = build_router(state_for(&pack, true), &demo, &basemap);

        // A bounded range is honored as a 206 with exactly the requested bytes.
        let mut req = request("GET", "/basemap.pmtiles");
        req.headers_mut().insert(
            header::RANGE,
            header::HeaderValue::from_static("bytes=0-511999"),
        );
        let reply = send(router.clone(), req).await;
        assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(reply.body.len(), 512_000);

        // A Range-less GET returns the whole file (200) — raw, unguarded.
        let reply = send(router.clone(), request("GET", "/basemap.pmtiles")).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.body.len(), 600_000);

        // HEAD works too.
        let reply = send(router, request("HEAD", "/basemap.pmtiles")).await;
        assert_eq!(reply.status, StatusCode::OK);

        let _ = std::fs::remove_dir_all(&pack);
        let _ = std::fs::remove_file(&basemap);
    }
}
