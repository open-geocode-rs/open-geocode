use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::error::{Error, Result};
use axum::{
    Json, Router,
    extract::{Query, State, rejection::QueryRejection},
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
        bounds, health, method, openapi,
        problem::{InvalidParam, Problem},
        request_id::{self, ErrorLog},
    },
    pack::Reader,
    record::Layer,
    resources::Resources,
    reverse::{GeocodeOptions, PackReverseGeocoder},
    search::{AddressGeocodeOptions, TextAutocompleteOptions, TextSearchOptions, TextSearcher},
};

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub pack: PathBuf,
    /// Static demo files to serve; without them only the API is served.
    pub demo: Option<PathBuf>,
    pub bind: SocketAddr,
    /// The basemap PMTiles archive the demo shows, when there is one.
    pub basemap: Option<PathBuf>,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pack: Arc<Reader>,
    searcher: Arc<TextSearcher>,
    reverse_geocoder: Arc<PackReverseGeocoder>,
    ready: Arc<AtomicBool>,
    error_log: ErrorLog,
}

impl AppState {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let pack = Arc::new(Reader::open(path)?);
        // Every query looks things up all over the Pack: read it in now
        // when it fits, so the first queries do not each wait on faults.
        pack.container()
            .advise_random_reads(Resources::probe().available_memory_bytes);
        let searcher = TextSearcher::from_pack(Arc::clone(&pack))?;
        let reverse_geocoder = PackReverseGeocoder::from_pack(Arc::clone(&pack))?;
        Ok(Self {
            pack,
            searcher: Arc::new(searcher),
            reverse_geocoder: Arc::new(reverse_geocoder),
            ready: Arc::new(AtomicBool::new(true)),
            error_log: ErrorLog::default(),
        })
    }

    /// True once the Pack is loaded and the Runtime can answer API traffic
    /// (ADR 0017 Decision 28).
    pub(crate) fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

/// `/search` and `/autocomplete` take the same parameters. They differ only in
/// how `q` is bounded, which the handlers do: `/search` requires it, and
/// `/autocomplete` answers an empty query with no suggestions.
#[derive(Debug, Deserialize)]
pub(crate) struct SearchParams {
    q: Option<String>,
    #[serde(default)]
    limit: usize,
    /// Comma-separated ISO 3166-1 alpha-2 codes; see [`parse_countries`].
    country: Option<String>,
    /// Left as a string here and parsed by [`parse_layer`], so an unknown
    /// layer is a 400 naming the valid ones rather than a serde rejection
    /// reading `unknown variant`.
    layer: Option<String>,
}

/// Resolve the `layer` filter, rejecting a layer that does not exist.
///
/// An unknown layer used to pass through to the index and match nothing, so a
/// typo looked exactly like "no results here" and clients had no way to tell
/// the two apart. Both comparable open geocoders reject it instead: Pelias
/// answers `'bogus' is an invalid layers parameter. Valid options: ...` and
/// Photon `Unknown layer type. Available layers: [...]`, each a 400 listing
/// what it would have accepted. We do the same.
///
/// An empty or whitespace-only value means "no filter", which is what a form
/// that always sends the key produces.
fn parse_layer(layer: Option<&str>) -> Result<Option<Layer>, Problem> {
    let Some(value) = layer.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    Layer::parse(value).map(Some).ok_or_else(|| {
        let valid = Layer::ALL
            .iter()
            .map(|layer| layer.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        Problem::malformed_parameter(format!("unknown layer {value:?}. Valid layers: {valid}"))
            .with_invalid_params(vec![InvalidParam::new("layer", "unknown layer")])
    })
}

/// Resolve the `country` filter: a comma-separated list of ISO 3166-1
/// alpha-2 codes, upper-cased, naming the countries to answer from.
///
/// A Pack built from one extract covers one country and needs none. One built
/// from several covers them all, and this narrows a search to part of it —
/// the same job `country` does on a Pelias or Photon query, where it is also
/// a filter on one index rather than a route to one of many.
///
/// A code that is not two letters is rejected, like an unknown `layer`: a
/// typo that silently matched nothing would look exactly like "no results
/// here". A country the Pack does not cover is *not* rejected, because it is
/// a legitimate empty answer.
///
/// An empty or whitespace-only value means "no filter", which is what a form
/// that always sends the key produces.
fn parse_countries(country: Option<&str>) -> Result<Vec<String>, Problem> {
    let Some(value) = country.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut codes = Vec::new();
    for part in value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let code = part.to_ascii_uppercase();
        if code.len() != 2 || !code.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            return Err(Problem::malformed_parameter(format!(
                "country {part:?} is not an ISO 3166-1 alpha-2 code, such as CA or NZ"
            ))
            .with_invalid_params(vec![InvalidParam::new("country", "not a country code")]));
        }
        if !codes.contains(&code) {
            codes.push(code);
        }
    }
    Ok(codes)
}

/// `/geocode`: an address whose parts the caller already has apart.
#[derive(Debug, Deserialize)]
pub(crate) struct GeocodeParams {
    address: Option<String>,
    locality: Option<String>,
    region: Option<String>,
    postcode: Option<String>,
    #[serde(default)]
    limit: usize,
    layer: Option<String>,
    /// Comma-separated ISO 3166-1 alpha-2 codes; see [`parse_countries`].
    country: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReverseParams {
    lon: f64,
    lat: f64,
}

pub async fn serve(options: ServeOptions) -> Result<()> {
    let state = AppState::open(&options.pack)?;
    let app = build_router(state, options.demo.as_deref(), options.basemap.as_deref());

    let listener = TcpListener::bind(options.bind)
        .await
        .map_err(|source| Error::IoPath {
            action: "failed to bind",
            path: options.pack.clone(),
            source,
        })?;
    println!(
        "Serving {} at http://{}",
        options.pack.display(),
        options.bind
    );
    axum::serve(listener, app).await.map_err(Error::from)
}

/// Assemble the public-demo router with its boundary policy: method-restricted
/// routes (ADR 0017 Decision 27), Problem Details 404/405 fallbacks
/// (Decisions 27, 37), and request-id propagation (Decision 30). The basemap is
/// served raw with no guard — in the deployed topology PMTiles is served from
/// Cloudflare R2, not the Runtime (Decisions 6/35a, amended 2026-06-03).
///
/// Split out from [`serve`] so it can be exercised without binding a socket.
pub(crate) fn build_router(state: AppState, demo: Option<&Path>, basemap: Option<&Path>) -> Router {
    // Read-only API and health routes: GET only (Decision 27). axum would
    // otherwise route HEAD to the GET handler automatically, so `get_only`
    // registers an explicit HEAD responder that returns 405. Other methods fall
    // through to `method_not_allowed_fallback`.
    let mut app: Router<AppState> = Router::new()
        .route("/search", get_only(search))
        .route("/autocomplete", get_only(autocomplete))
        .route("/geocode", get_only(geocode))
        .route("/reverse", get_only(reverse))
        .route("/openapi.json", get_only(openapi::openapi_json))
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
    match basemap {
        Some(basemap) if basemap.exists() => {
            app = app.route_service("/basemap.pmtiles", ServeFile::new(basemap));
            println!("Serving basemap {}", basemap.display());
        }
        Some(basemap) => eprintln!(
            "basemap {} not found; serving demo without a basemap",
            basemap.display()
        ),
        None => {}
    }

    // ServeDir keeps local-dev static serving (Decision 10) but its built-in 404
    // is replaced with a Problem Details 404 so unknown paths don't fall through
    // to a static-file response (Decision 37). `method_not_allowed_fallback`
    // turns method mismatches into a Problem 405 instead of an empty body. The
    // request-id layer is outermost so it also wraps both fallbacks, which puts
    // the id in every Problem and logs every 500 in one place.
    let error_log = state.error_log.clone();
    let app = app.method_not_allowed_fallback(method::method_not_allowed);
    let app = match demo {
        Some(demo) => app.fallback_service(
            ServeDir::new(demo).not_found_service(method::not_found.into_service()),
        ),
        None => app.fallback(method::not_found),
    };
    app.layer(middleware::from_fn_with_state(
        error_log,
        request_id::propagate,
    ))
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

/// Run geocoder work, GeoJSON included, on the blocking pool. Its errors and
/// a panic both become Problems here, with their cause kept for the log, so a
/// handler only needs `?`.
async fn blocking<T, F>(work: F) -> Result<T, Problem>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    task::spawn_blocking(work).await?.map_err(Problem::from)
}

/// A query string that does not parse; the rejection's own text is not shown.
fn malformed_parameters(_: QueryRejection) -> Problem {
    Problem::malformed_parameter("could not parse query parameters")
}

/// Search for a place by text.
///
/// Answers are ordered by `score`, best first.
#[utoipa::path(
    get,
    path = "/search",
    tag = "geocoding",
    params(
        ("q" = String, Query,
         description = "The text to search for.",
         example = "10 King Street, Toronto",
         min_length = 1, max_length = 256),
        ("limit" = Option<usize>, Query,
         description = "How many answers to return. 0, the default, lets the \
                        server choose.",
         minimum = 0, maximum = 50),
        ("layer" = Option<Layer>, Query,
         description = "Return only answers of this layer. An unknown layer is \
                        rejected rather than silently matching nothing."),
        ("country" = Option<String>, Query,
         description = "Return only answers in these countries: ISO 3166-1 \
                        alpha-2 codes, comma-separated. A Pack covering one \
                        country needs none. A code that is not two letters is \
                        rejected; one the Pack does not cover simply matches \
                        nothing.",
         example = "NZ,AU"),
    ),
    responses(
        (status = 200, description = "The places that matched, best first.",
         body = ForwardCollection),
        (status = 400, description = "`q` was empty or too long, `limit` was above \
                                      the maximum, or a parameter did not parse.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "The search failed inside the server.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn search(
    State(state): State<AppState>,
    params: Result<Query<SearchParams>, QueryRejection>,
) -> Result<Json<ForwardCollection>, Problem> {
    let Query(params) = params.map_err(malformed_parameters)?;

    let query = bounds::validate_query(params.q.as_deref().unwrap_or_default())?;
    bounds::validate_search_limit(params.limit)?;
    let layer = parse_layer(params.layer.as_deref())?;
    let countries = parse_countries(params.country.as_deref())?;

    let options = TextSearchOptions {
        query: query.clone(),
        limit: params.limit,
        layer: layer.map(|layer| layer.as_str().to_string()),
        countries: countries.clone(),
    };
    let collection = blocking(move || {
        let hits = state.searcher.search(options)?;
        api::forward_collection(&state.pack, query, hits)
    })
    .await?;

    Ok(Json(collection))
}

/// Suggest places as a query is typed.
///
/// Like `/search`, but tuned for partial input: an empty or very short `q` is
/// not an error, it simply has no suggestions.
#[utoipa::path(
    get,
    path = "/autocomplete",
    tag = "geocoding",
    params(
        ("q" = Option<String>, Query,
         description = "The text typed so far. Empty returns no suggestions.",
         example = "10 King St",
         max_length = 256),
        ("limit" = Option<usize>, Query,
         description = "How many suggestions to return. 0, the default, lets the \
                        server choose.",
         minimum = 0, maximum = 20),
        ("layer" = Option<Layer>, Query,
         description = "Return only suggestions of this layer. An unknown layer is \
                        rejected rather than silently matching nothing."),
        ("country" = Option<String>, Query,
         description = "Return only answers in these countries: ISO 3166-1 \
                        alpha-2 codes, comma-separated. A Pack covering one \
                        country needs none. A code that is not two letters is \
                        rejected; one the Pack does not cover simply matches \
                        nothing.",
         example = "NZ,AU"),
    ),
    responses(
        (status = 200, description = "The suggestions, best first.",
         body = ForwardCollection),
        (status = 400, description = "`q` was too long, `limit` was above the \
                                      maximum, or a parameter did not parse.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "The search failed inside the server.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn autocomplete(
    State(state): State<AppState>,
    params: Result<Query<SearchParams>, QueryRejection>,
) -> Result<Json<ForwardCollection>, Problem> {
    let Query(params) = params.map_err(malformed_parameters)?;

    // Autocomplete tolerates an empty/short query (it returns no suggestions), so
    // only bound the length here; the searcher caps the result count internally.
    let query = bounds::validate_query_length(params.q.as_deref().unwrap_or_default())?;
    bounds::validate_autocomplete_limit(params.limit)?;
    let layer = parse_layer(params.layer.as_deref())?;
    let countries = parse_countries(params.country.as_deref())?;

    let options = TextAutocompleteOptions {
        query: query.clone(),
        limit: params.limit,
        layer: layer.map(|layer| layer.as_str().to_string()),
        countries,
    };
    let collection = blocking(move || {
        let hits = state.searcher.autocomplete(options)?;
        api::forward_collection(&state.pack, query, hits)
    })
    .await?;

    Ok(Json(collection))
}

/// Geocode an address whose parts are already apart.
///
/// For callers who hold a street address, a town and a postcode in separate
/// fields and would otherwise have to paste them into one string and hope the
/// parser pulls them apart again. The street address is searched on its own
/// and hits are kept only where they fit the locality, region and postcode
/// given, so a town in the address cannot outrank the street.
///
/// This is the lookup `geocode batch` runs per row, over HTTP.
///
/// A house number the data lacks is placed between its neighbours
/// (`accuracy` `interpolated`) or, failing that, answered with its street
/// (`accuracy` `street`). A street answers a given postcode only where
/// postcode data confirms it. The answer is never another house: a flat
/// written "10/1367" is found as itself or by 1367, never as house 10.
///
/// The answer is the same FeatureCollection every other route returns, with
/// at most one feature: `features` holds the one match, or is empty when
/// nothing fits. `geocoding.query` is the rewrite that matched, which is not
/// always the address as it was sent.
#[utoipa::path(
    get,
    path = "/geocode",
    tag = "geocoding",
    params(
        ("address" = String, Query,
         description = "The street address on its own: house number and street, \
                        without the town or postcode.",
         example = "10 King Street", min_length = 1, max_length = 256),
        ("locality" = Option<String>, Query,
         description = "The town or city the address is in.",
         example = "Toronto", max_length = 256),
        ("region" = Option<String>, Query,
         description = "The state, province or region.",
         example = "Ontario", max_length = 256),
        ("postcode" = Option<String>, Query,
         description = "The postcode.", example = "M5V 1A1", max_length = 256),
        ("limit" = Option<usize>, Query,
         description = "How many candidates to consider before picking the best. \
                        0, the default, lets the server choose. At most one is \
                        ever returned.",
         minimum = 0, maximum = 50),
        ("layer" = Option<Layer>, Query,
         description = "Consider only candidates of this layer. An unknown layer \
                        is rejected rather than silently matching nothing."),
        ("country" = Option<String>, Query,
         description = "Return only answers in these countries: ISO 3166-1 \
                        alpha-2 codes, comma-separated. A Pack covering one \
                        country needs none. A code that is not two letters is \
                        rejected; one the Pack does not cover simply matches \
                        nothing.",
         example = "NZ,AU"),
    ),
    responses(
        (status = 200,
         description = "The one address that fits, or no features when none does.",
         body = ForwardCollection),
        (status = 400, description = "`address` was empty or too long, a context \
                                      field was too long, `limit` was above the \
                                      maximum, or a parameter did not parse.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "The lookup failed inside the server.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn geocode(
    State(state): State<AppState>,
    params: Result<Query<GeocodeParams>, QueryRejection>,
) -> Result<Json<ForwardCollection>, Problem> {
    let Query(params) = params.map_err(malformed_parameters)?;

    let address = bounds::validate_query(params.address.as_deref().unwrap_or_default())
        .map_err(|_| Problem::invalid_query("address must not be empty"))?;
    // The context fields narrow the search rather than drive it, so an empty
    // one means "not given" instead of being an error the way `address` is.
    let context = |value: Option<String>| -> Result<Option<String>, Problem> {
        let value = bounds::validate_query_length(value.as_deref().unwrap_or_default())?;
        Ok(Some(value).filter(|value| !value.is_empty()))
    };
    let locality = context(params.locality)?;
    let region = context(params.region)?;
    let postcode = context(params.postcode)?;
    bounds::validate_search_limit(params.limit)?;
    let layer = parse_layer(params.layer.as_deref())?;
    let countries = parse_countries(params.country.as_deref())?;

    let options = AddressGeocodeOptions {
        address: address.clone(),
        locality,
        region,
        postcode,
        limit: params.limit,
        layer: layer.map(|layer| layer.as_str().to_string()),
        countries,
    };
    let collection = blocking(move || {
        let found = state.searcher.geocode_address(options)?;
        let (query, hits) = match found {
            Some(found) => (found.query, vec![found.hit]),
            None => (address, Vec::new()),
        };
        api::forward_collection(&state.pack, query, hits)
    })
    .await?;

    Ok(Json(collection))
}

/// Find the places at a point.
///
/// The first feature is the place itself, when there is one, carrying its
/// `distance_m`. The features after it are the areas the point lies in, from
/// the most precise outwards, and carry no distance.
#[utoipa::path(
    get,
    path = "/reverse",
    tag = "geocoding",
    params(
        ("lon" = f64, Query, description = "Longitude, in degrees.",
         example = -79.3832, minimum = -180.0, maximum = 180.0),
        ("lat" = f64, Query, description = "Latitude, in degrees.",
         example = 43.6532, minimum = -90.0, maximum = 90.0),
    ),
    responses(
        (status = 200, description = "What lies at the point, most precise first. \
                                      `features` is empty when nothing is near.",
         body = ReverseCollection),
        (status = 400, description = "`lon` or `lat` was missing or out of range.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "Reverse geocoding failed inside the server.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn reverse(
    State(state): State<AppState>,
    params: Result<Query<ReverseParams>, QueryRejection>,
) -> Result<Json<ReverseCollection>, Problem> {
    let Query(params) = params.map_err(malformed_parameters)?;

    bounds::validate_coords(params.lon, params.lat)?;

    let collection = blocking(move || {
        let response = state.reverse_geocoder.reverse(GeocodeOptions {
            lon: params.lon,
            lat: params.lat,
        })?;
        api::reverse_collection(&state.pack, &response)
    })
    .await?;

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
    use crate::pack::Writer;
    use crate::record::{
        AddressComponents, AddressRecord, LocationPrecision, ObjectType, SourceProvenance,
        point_geometry,
    };

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("open-geocode-http-{name}-{}", std::process::id()))
    }

    fn write_pack(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
        let mut writer = Writer::create(dir).expect("writer");
        writer
            .write(
                &AddressRecord {
                    street_postcodes: Vec::new(),
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
                        object_type: ObjectType::Node,
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

        let router = build_router(state_for(&pack, true), Some(&demo), Some(&no_basemap));
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

        let not_ready = build_router(state_for(&pack, false), Some(&demo), Some(&no_basemap));
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
        let router = build_router(state_for(&pack, true), Some(&demo), Some(&no_basemap));

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

        // An unknown layer -> 400 naming the layers that would have been
        // accepted, not an empty answer that looks like "nothing here".
        let reply = send(router.clone(), request("GET", "/search?q=king&layer=bogus")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        let body = reply.json();
        assert_eq!(body["error_code"], "malformed_parameter");
        assert_eq!(body["invalid_params"][0]["name"], "layer");
        let detail = body["detail"].as_str().expect("detail");
        assert!(detail.contains("bogus"), "{detail}");
        assert!(detail.contains("address"), "{detail}");

        // An unknown layer is rejected on /autocomplete too.
        let reply = send(
            router.clone(),
            request("GET", "/autocomplete?q=kin&layer=bogus"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);

        // A known layer filters, and an empty one means "no filter" rather
        // than a rejection, which is what a form that always sends the key
        // produces.
        let reply = send(
            router.clone(),
            request("GET", "/search?q=King%20Street&layer=address"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        let reply = send(router.clone(), request("GET", "/search?q=king&layer=")).await;
        assert_eq!(reply.status, StatusCode::OK);

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
        let router = build_router(state_for(&pack, true), Some(&demo), Some(&no_basemap));

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

    /// The cases the endpoint was asked for: a hit inside the postcode, a
    /// miss in another one, and a missing address.
    #[tokio::test]
    async fn geocode_keeps_hits_in_the_given_postcode() {
        let pack = temp_path("geocode");
        write_pack(&pack);
        let demo = temp_path("geocode-demo");
        let no_basemap = temp_path("geocode-no-basemap");
        let router = build_router(state_for(&pack, true), Some(&demo), Some(&no_basemap));

        // The address is in this postcode, so it is found.
        let reply = send(
            router.clone(),
            request(
                "GET",
                "/geocode?address=10%20King%20Street&postcode=M5V%201A1",
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        let body = reply.json();
        assert_eq!(body["type"], "FeatureCollection");
        assert_eq!(body["features"].as_array().expect("features").len(), 1);
        let feature = &body["features"][0];
        assert_eq!(feature["properties"]["layer"], "address");
        assert_eq!(feature["geometry"]["coordinates"][0], -79.0);

        // The same address in a postcode it is not in finds nothing. An empty
        // collection, not an error: the request was fine, the answer is that
        // there is none.
        let reply = send(
            router.clone(),
            request(
                "GET",
                "/geocode?address=10%20King%20Street&postcode=V6B%201A1",
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        let body = reply.json();
        assert_eq!(body["type"], "FeatureCollection");
        assert!(
            body["features"].as_array().expect("features").is_empty(),
            "an address in the wrong postcode should match nothing"
        );

        // Context without an address is not a query.
        let reply = send(router.clone(), request("GET", "/geocode?locality=Toronto")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["error_code"], "invalid_query");

        // The layer filter is validated here as everywhere else.
        let reply = send(
            router,
            request("GET", "/geocode?address=10%20King%20Street&layer=bogus"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["error_code"], "malformed_parameter");

        let _ = std::fs::remove_dir_all(&pack);
    }

    #[tokio::test]
    async fn methods_unknown_paths_and_request_ids() {
        let pack = temp_path("methods");
        write_pack(&pack);
        let demo = temp_path("methods-demo");
        let no_basemap = temp_path("methods-no-basemap");
        let router = build_router(state_for(&pack, true), Some(&demo), Some(&no_basemap));

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

        let router = build_router(state_for(&pack, true), Some(&demo), Some(&basemap));

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

    /// A router whose routes fail on purpose, behind the same request-id layer
    /// as the real one, logging into `log`.
    fn failing_router(log: &ErrorLog) -> Router {
        Router::new()
            .route(
                "/error",
                axum::routing::get(|| async {
                    blocking(|| -> Result<()> {
                        Err(Error::Inconsistent(
                            "section offset past end of file".into(),
                        ))
                    })
                    .await
                }),
            )
            .route(
                "/panic",
                axum::routing::get(|| async {
                    blocking(|| -> Result<()> { panic!("label {} out of range", 7) }).await
                }),
            )
            .layer(middleware::from_fn_with_state(
                log.clone(),
                request_id::propagate,
            ))
    }

    fn log_lines(log: &ErrorLog) -> Vec<serde_json::Value> {
        log.lines()
            .iter()
            .map(|line| serde_json::from_str(line).expect("log line is JSON"))
            .collect()
    }

    #[tokio::test]
    async fn internal_error_is_a_bare_500_with_its_cause_logged() {
        let log = ErrorLog::capture();
        let mut tagged = request("GET", "/error?q=12%20Private%20Lane");
        tagged
            .headers_mut()
            .insert("cf-ray", header::HeaderValue::from_static("ray-500"));
        let reply = send(failing_router(&log), tagged).await;

        // The client sees a bare 500 with the request id and nothing else.
        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.content_type(), "application/problem+json");
        assert_eq!(reply.request_id().as_deref(), Some("ray-500"));
        let body = reply.json();
        assert_eq!(body["error_code"], "internal_error");
        assert_eq!(body["request_id"], "ray-500");
        assert!(body.get("detail").is_none());
        assert!(!body.to_string().contains("section offset"));

        // The log has one line naming the request, route and cause, and not
        // the query, which is personal data.
        let lines = log_lines(&log);
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert_eq!(line["level"], "error");
        assert_eq!(line["request_id"], "ray-500");
        assert_eq!(line["route"], "/error");
        assert_eq!(line["status"], 500);
        assert!(line["latency_ms"].is_u64());
        assert_eq!(
            line["cause"],
            "Pack is inconsistent: section offset past end of file"
        );
        assert!(!log.lines()[0].contains("Private"));
    }

    #[tokio::test]
    async fn panic_in_blocking_work_is_logged_with_its_message() {
        let log = ErrorLog::capture();
        let reply = send(failing_router(&log), request("GET", "/panic")).await;

        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!reply.json().to_string().contains("out of range"));
        let lines = log_lines(&log);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0]["cause"],
            "blocking task panicked: label 7 out of range"
        );
        assert_eq!(
            lines[0]["request_id"].as_str(),
            reply.request_id().as_deref()
        );
    }

    #[tokio::test]
    async fn user_errors_are_not_logged() {
        let pack = temp_path("user-errors");
        write_pack(&pack);
        let demo = temp_path("user-errors-demo");
        let no_basemap = temp_path("user-errors-no-basemap");
        let log = ErrorLog::capture();
        let mut state = state_for(&pack, true);
        state.error_log = log.clone();
        let router = build_router(state, Some(&demo), Some(&no_basemap));

        for (method, uri, status) in [
            ("GET", "/search?q=", StatusCode::BAD_REQUEST),
            ("GET", "/reverse?lon=999&lat=0", StatusCode::BAD_REQUEST),
            ("GET", "/does-not-exist", StatusCode::NOT_FOUND),
            ("POST", "/search", StatusCode::METHOD_NOT_ALLOWED),
            ("GET", "/search?q=King%20Street", StatusCode::OK),
        ] {
            let reply = send(router.clone(), request(method, uri)).await;
            assert_eq!(reply.status, status, "{method} {uri}");
            // Every Problem still carries the id the middleware gave it.
            if status != StatusCode::OK {
                assert_eq!(
                    reply.json()["request_id"].as_str(),
                    reply.request_id().as_deref()
                );
            }
        }
        assert!(log.lines().is_empty());

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// `country` is parsed like `layer`: a value that cannot be a code is a
    /// 400 rather than a filter that silently matches nothing.
    #[test]
    fn parses_the_country_filter() {
        let parse = |value: Option<&str>| parse_countries(value).map_err(|_| ());

        // No filter: absent, empty, or whitespace, as a form that always
        // sends the key produces.
        assert_eq!(parse(None), Ok(Vec::new()));
        assert_eq!(parse(Some("")), Ok(Vec::new()));
        assert_eq!(parse(Some("  ")), Ok(Vec::new()));

        // Upper-cased, trimmed, comma-separated, and de-duplicated so a
        // repeated code does not repeat a clause.
        assert_eq!(parse(Some("nz")), Ok(vec!["NZ".to_string()]));
        assert_eq!(
            parse(Some(" nz , AU ")),
            Ok(vec!["NZ".to_string(), "AU".to_string()])
        );
        assert_eq!(parse(Some("nz,NZ")), Ok(vec!["NZ".to_string()]));
        // An empty item between commas is skipped, not an error.
        assert_eq!(parse(Some("NZ,,AU")).map(|v| v.len()), Ok(2));

        // Anything that is not two ASCII letters is a typo.
        for bad in ["Canada", "C", "CAN", "1A", "C-A"] {
            assert!(parse(Some(bad)).is_err(), "{bad:?} should be rejected");
        }
    }
}
