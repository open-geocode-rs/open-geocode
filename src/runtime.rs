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
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, task};
use tower_http::services::{ServeDir, ServeFile};

use crate::{
    http::{
        bounds, health, method,
        problem::{Problem, classify_search_error},
        request_id::{self, RequestId},
    },
    pack::{RecordPoint, RecordPointPrecision, RecordSource},
    record::OsmObjectType,
    reverse::{PackReverseGeocoder, ReverseGeocodeOptions, ReverseGeocodeResponse},
    search::{PackTextSearcher, TextAutocompleteOptions, TextSearchHit, TextSearchOptions},
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
    searcher: Arc<PackTextSearcher>,
    reverse_geocoder: Arc<PackReverseGeocoder>,
    ready: Arc<AtomicBool>,
}

impl AppState {
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

#[derive(Debug, Serialize, PartialEq)]
pub struct SearchResponse {
    pub query: String,
    pub results: Vec<SearchApiResult>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct AutocompleteResponse {
    pub query: String,
    pub suggestions: Vec<SearchApiResult>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SearchApiResult {
    pub record_id: u64,
    pub id: String,
    pub layer: String,
    pub label: String,
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<SearchApiPoint>,
    pub source: SearchApiSource,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SearchApiPoint {
    pub lon: f64,
    pub lat: f64,
    pub precision: SearchApiPointPrecision,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchApiPointPrecision {
    Point,
    Centroid,
    RepresentativePoint,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SearchApiSource {
    pub dataset: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_type: Option<OsmObjectType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derived_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_count: Option<u64>,
}

pub async fn serve(options: ServeOptions) -> Result<()> {
    let searcher = PackTextSearcher::open(&options.pack)
        .with_context(|| format!("failed to open Pack {}", options.pack.display()))?;
    let reverse_geocoder = PackReverseGeocoder::open(&options.pack).with_context(|| {
        format!(
            "failed to open Pack Spatial Index {}",
            options.pack.display()
        )
    })?;
    // Both opens succeeded, so the Runtime is ready to answer API traffic.
    let state = AppState {
        searcher: Arc::new(searcher),
        reverse_geocoder: Arc::new(reverse_geocoder),
        ready: Arc::new(AtomicBool::new(true)),
    };
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

async fn search(
    State(state): State<AppState>,
    request_id: Option<Extension<RequestId>>,
    params: Result<Query<SearchParams>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<SearchResponse>, Problem> {
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
    let searcher = Arc::clone(&state.searcher);
    let hits = task::spawn_blocking(move || searcher.search(options))
        .await
        .map_err(|_| with_id(Problem::internal(), &request_id))?
        .map_err(|error| with_id(classify_search_error(error), &request_id))?;

    Ok(Json(SearchResponse {
        query,
        results: hits.into_iter().map(SearchApiResult::from_hit).collect(),
    }))
}

async fn autocomplete(
    State(state): State<AppState>,
    request_id: Option<Extension<RequestId>>,
    params: Result<Query<SearchParams>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<AutocompleteResponse>, Problem> {
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
    let searcher = Arc::clone(&state.searcher);
    let hits = task::spawn_blocking(move || searcher.autocomplete(options))
        .await
        .map_err(|_| with_id(Problem::internal(), &request_id))?
        .map_err(|error| with_id(classify_search_error(error), &request_id))?;

    Ok(Json(AutocompleteResponse {
        query,
        suggestions: hits.into_iter().map(SearchApiResult::from_hit).collect(),
    }))
}

async fn reverse(
    State(state): State<AppState>,
    request_id: Option<Extension<RequestId>>,
    params: Result<Query<ReverseParams>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<ReverseGeocodeResponse>, Problem> {
    let request_id = request_id.map(|Extension(RequestId(id))| id);
    let Query(params) = params.map_err(|_| {
        with_id(
            Problem::malformed_parameter("could not parse query parameters"),
            &request_id,
        )
    })?;

    bounds::validate_coords(params.lon, params.lat)
        .map_err(|problem| with_id(problem, &request_id))?;

    let geocoder = Arc::clone(&state.reverse_geocoder);
    let response = task::spawn_blocking(move || {
        geocoder.reverse(ReverseGeocodeOptions {
            lon: params.lon,
            lat: params.lat,
        })
    })
    .await
    .map_err(|_| with_id(Problem::internal(), &request_id))?
    .map_err(|_| with_id(Problem::internal(), &request_id))?;

    Ok(Json(response))
}

impl SearchApiResult {
    fn from_hit(hit: TextSearchHit) -> Self {
        Self {
            record_id: hit.record_id,
            id: hit.record.id,
            layer: hit.record.layer,
            label: hit.record.label,
            score: hit.score,
            point: hit.record.point.map(SearchApiPoint::from),
            source: SearchApiSource::from(hit.record.source),
        }
    }
}

impl From<RecordPoint> for SearchApiPoint {
    fn from(point: RecordPoint) -> Self {
        Self {
            lon: point.lon,
            lat: point.lat,
            precision: match point.precision {
                RecordPointPrecision::Point => SearchApiPointPrecision::Point,
                RecordPointPrecision::Centroid => SearchApiPointPrecision::Centroid,
                RecordPointPrecision::RepresentativePoint => {
                    SearchApiPointPrecision::RepresentativePoint
                }
            },
        }
    }
}

impl From<RecordSource> for SearchApiSource {
    fn from(source: RecordSource) -> Self {
        Self {
            dataset: source.dataset,
            object_type: source.object_type,
            object_id: source.object_id,
            derived_from: source.derived_from,
            record_count: source.record_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::pack::RecordSummary;
    use crate::record::OsmObjectType;

    use super::*;

    #[test]
    fn api_result_shapes_address_point() {
        let hit = TextSearchHit {
            record_id: 7,
            score: 4.25,
            record: RecordSummary {
                id: "osm:node:1".to_string(),
                layer: "address".to_string(),
                label: "10 King Street, Toronto".to_string(),
                point: Some(RecordPoint {
                    lon: -79.3832,
                    lat: 43.6532,
                    precision: RecordPointPrecision::Point,
                }),
                source: RecordSource {
                    dataset: "osm".to_string(),
                    object_type: Some(OsmObjectType::Node),
                    object_id: Some(1),
                    derived_from: None,
                    record_count: None,
                },
            },
        };

        let result = SearchApiResult::from_hit(hit);

        assert_eq!(result.record_id, 7);
        assert_eq!(result.id, "osm:node:1");
        assert_eq!(result.layer, "address");
        assert_eq!(result.label, "10 King Street, Toronto");
        assert_eq!(
            result.point,
            Some(SearchApiPoint {
                lon: -79.3832,
                lat: 43.6532,
                precision: SearchApiPointPrecision::Point,
            })
        );
        assert_eq!(result.source.dataset, "osm");
        assert_eq!(result.source.object_type, Some(OsmObjectType::Node));
    }

    #[test]
    fn api_result_uses_representative_point_for_street() {
        let hit = TextSearchHit {
            record_id: 12,
            score: 3.0,
            record: RecordSummary {
                id: "osm:way:9".to_string(),
                layer: "street".to_string(),
                label: "King Street".to_string(),
                point: Some(RecordPoint {
                    lon: -79.41,
                    lat: 43.61,
                    precision: RecordPointPrecision::RepresentativePoint,
                }),
                source: RecordSource {
                    dataset: "osm".to_string(),
                    object_type: Some(OsmObjectType::Way),
                    object_id: Some(9),
                    derived_from: None,
                    record_count: None,
                },
            },
        };

        let result = SearchApiResult::from_hit(hit);

        assert_eq!(
            result.point,
            Some(SearchApiPoint {
                lon: -79.41,
                lat: 43.61,
                precision: SearchApiPointPrecision::RepresentativePoint,
            })
        );
    }
}

#[cfg(test)]
mod router_tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;
    use crate::builder::report::BuilderReport;
    use crate::pack::{PackWriter, RecordWriter};
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
            .write_address(&AddressRecord {
                address: AddressComponents {
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
            })
            .expect("write address");
        writer
            .finish(&mut BuilderReport::default())
            .expect("finish");
    }

    fn state_for(pack_dir: &Path, ready: bool) -> AppState {
        let searcher = PackTextSearcher::open(pack_dir).expect("searcher");
        let reverse_geocoder = PackReverseGeocoder::open(pack_dir).expect("reverse geocoder");
        AppState {
            searcher: Arc::new(searcher),
            reverse_geocoder: Arc::new(reverse_geocoder),
            ready: Arc::new(AtomicBool::new(ready)),
        }
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
