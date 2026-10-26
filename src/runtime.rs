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
    extract::{Query, Request, State, rejection::QueryRejection},
    handler::{Handler, HandlerWithoutStateExt},
    middleware,
    response::{IntoResponse, Response},
    routing::{MethodFilter, MethodRouter, on},
};
use rayon::{ThreadPool, ThreadPoolBuilder};
use serde::Deserialize;
use tokio::{net::TcpListener, task};
use tower_http::services::{ServeDir, ServeFile};

use crate::{
    api::{self, ForwardCollection, ReverseCollection},
    http::{
        batch::{self, Lookup},
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
    batch_pool: Arc<ThreadPool>,
}

impl AppState {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let pack = Arc::new(Reader::open(path)?);
        let resources = Resources::probe();
        // Every query looks things up all over the Pack: read it in now
        // when it fits, so the first queries do not each wait on faults.
        pack.container()
            .advise_random_reads(resources.available_memory_bytes);
        let searcher = TextSearcher::from_pack(Arc::clone(&pack))?;
        let reverse_geocoder = PackReverseGeocoder::from_pack(Arc::clone(&pack))?;
        // One CPU short of all of them, so single lookups still find one
        // free while batches run.
        let batch_pool = ThreadPoolBuilder::new()
            .num_threads(resources.usable_cpus.saturating_sub(1).max(1))
            .thread_name(|index| format!("batch-{index}"))
            .build()
            .map_err(std::io::Error::other)?;
        Ok(Self {
            pack,
            searcher: Arc::new(searcher),
            reverse_geocoder: Arc::new(reverse_geocoder),
            ready: Arc::new(AtomicBool::new(true)),
            error_log: ErrorLog::default(),
            batch_pool: Arc::new(batch_pool),
        })
    }

    /// Where batch items run.
    pub(crate) fn batch_pool(&self) -> &ThreadPool {
        &self.batch_pool
    }

    pub(crate) fn error_log(&self) -> &ErrorLog {
        &self.error_log
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
    /// `lon,lat`; see [`bounds::parse_focus`].
    focus: Option<String>,
    /// `min_lon,min_lat,max_lon,max_lat`; see [`bounds::parse_bbox`].
    bbox: Option<String>,
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
    // Read-only routes: GET only (Decision 27), and QUERY or POST for a batch
    // on the lookup routes. axum would otherwise route HEAD to the GET
    // handler automatically, so both register an explicit HEAD responder that
    // returns 405. Other methods fall through to a 405.
    let mut app: Router<AppState> = Router::new()
        .route("/search", batchable(search, search_batch))
        .route("/autocomplete", get_only(autocomplete))
        .route("/geocode", batchable(geocode, geocode_batch))
        .route("/reverse", batchable(reverse, reverse_batch))
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

/// A route that answers one lookup on GET and a batch of them on QUERY or
/// POST. axum filters only the standard methods, so QUERY reaches the batch
/// through the route's fallback, which turns every other method away.
fn batchable<H, T, B, U>(single: H, batch: B) -> MethodRouter<AppState>
where
    H: Handler<T, AppState>,
    B: Handler<U, AppState>,
    T: 'static,
    U: 'static,
{
    let query = batch.clone();
    on(MethodFilter::GET, single)
        .on(MethodFilter::POST, batch)
        .on(MethodFilter::HEAD, batch_method_not_allowed)
        .fallback(
            move |State(state): State<AppState>, request: Request| async move {
                if request.method().as_str() == "QUERY" {
                    query.call(request, state).await
                } else {
                    batch_method_not_allowed().await.into_response()
                }
            },
        )
        .layer(middleware::map_response(batch::advertise))
}

async fn batch_method_not_allowed() -> Problem {
    Problem::method_not_allowed(batch::METHODS)
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

fn malformed_parameters(_: QueryRejection) -> Problem {
    Problem::unparsable_parameters()
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
        ("focus" = Option<String>, Query,
         description = "Where the caller is, `lon,lat`. Of answers the query \
                        cannot tell apart, the nearer ranks first; it never \
                        outranks what the query states or a better match. A \
                        point out of range is rejected.",
         example = "-79.38,43.65"),
        ("bbox" = Option<String>, Query,
         description = "Return only answers inside this box, \
                        `min_lon,min_lat,max_lon,max_lat`. Its west edge must \
                        not lie east of its east edge, nor its south edge \
                        north of its north edge: a box across the \
                        antimeridian is two requests.",
         example = "-79.64,43.58,-79.12,43.86"),
    ),
    responses(
        (status = 200, description = "The places that matched, best first.",
         body = ForwardCollection),
        (status = 400, description = "`q` was empty or too long, `limit` was above \
                                      the maximum, `focus` or `bbox` was out of \
                                      range, or a parameter did not parse.",
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
    let options = search_request(params)?;
    let collection = blocking(move || search_answer(&state, options)).await?;
    Ok(Json(collection))
}

/// Search for many places by text in one request.
///
/// The body is a JSON array of up to 1000 items, each taking the parameters
/// `GET /search` takes, plus an optional `id` echoed in its result. The
/// request's own query-string parameters apply to every item, and an item's
/// own fields override them. `QUERY` takes the same body and answers the
/// same: `POST` is for clients that cannot send `QUERY`.
///
/// Each result's `response` is exactly what `GET` returns for its item, and
/// its `error` the Problem `GET` would return. Results are in input order,
/// one per item, and a bad item never fails the batch.
#[utoipa::path(
    post,
    path = "/search",
    tag = "geocoding",
    request_body(content = Vec<openapi::SearchItem>, content_type = "application/json"),
    responses(
        (status = 200, description = "One result per item, in input order.",
         body = openapi::ForwardBatch),
        (status = 400, description = "The body is not a JSON array of objects, or the \
                                      query string did not parse.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 413, description = "More than 1000 items, or a body over its cap.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 415, description = "The body was not sent as `application/json`.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "The batch failed inside the server.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn search_batch(State(state): State<AppState>, request: Request) -> Response {
    let lookup = Lookup {
        route: "/search",
        prepare: search_request,
        answer: search_answer,
    };
    batch::run(state, lookup, request).await
}

/// `/search`'s parameters, checked.
fn search_request(params: SearchParams) -> Result<TextSearchOptions, Problem> {
    let query = bounds::validate_query(params.q.as_deref().unwrap_or_default())?;
    bounds::validate_search_limit(params.limit)?;
    let layer = parse_layer(params.layer.as_deref())?;
    Ok(TextSearchOptions {
        query,
        limit: params.limit,
        layer: layer.map(|layer| layer.as_str().to_string()),
        countries: parse_countries(params.country.as_deref())?,
        focus: bounds::parse_focus(params.focus.as_deref())?,
        bbox: bounds::parse_bbox(params.bbox.as_deref())?,
    })
}

fn search_answer(state: &AppState, options: TextSearchOptions) -> Result<ForwardCollection> {
    let query = options.query.clone();
    let hits = state.searcher.search(options)?;
    api::forward_collection(&state.pack, query, hits)
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
        ("focus" = Option<String>, Query,
         description = "Where the caller is, `lon,lat`. Of answers the query \
                        cannot tell apart, the nearer ranks first; it never \
                        outranks what the query states or a better match. A \
                        point out of range is rejected.",
         example = "-79.38,43.65"),
        ("bbox" = Option<String>, Query,
         description = "Return only answers inside this box, \
                        `min_lon,min_lat,max_lon,max_lat`. Its west edge must \
                        not lie east of its east edge, nor its south edge \
                        north of its north edge: a box across the \
                        antimeridian is two requests.",
         example = "-79.64,43.58,-79.12,43.86"),
    ),
    responses(
        (status = 200, description = "The suggestions, best first.",
         body = ForwardCollection),
        (status = 400, description = "`q` was too long, `limit` was above the \
                                      maximum, `focus` or `bbox` was out of range, \
                                      or a parameter did not parse.",
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
    let focus = bounds::parse_focus(params.focus.as_deref())?;
    let bbox = bounds::parse_bbox(params.bbox.as_deref())?;

    let options = TextAutocompleteOptions {
        query: query.clone(),
        limit: params.limit,
        layer: layer.map(|layer| layer.as_str().to_string()),
        countries,
        focus,
        bbox,
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
    let options = geocode_request(params)?;
    let collection = blocking(move || geocode_answer(&state, options)).await?;
    Ok(Json(collection))
}

/// Geocode many addresses whose parts are already apart in one request.
///
/// The body is a JSON array of up to 1000 items, each taking the parameters
/// `GET /geocode` takes, plus an optional `id` echoed in its result. The
/// request's own query-string parameters apply to every item, and an item's
/// own fields override them: `?country=CA` scopes every item to Canada.
/// `QUERY` takes the same body and answers the same: `POST` is for clients
/// that cannot send `QUERY`.
///
/// Each result's `response` is exactly what `GET` returns for its item, and
/// its `error` the Problem `GET` would return. Results are in input order,
/// one per item, and a bad item never fails the batch.
#[utoipa::path(
    post,
    path = "/geocode",
    tag = "geocoding",
    request_body(content = Vec<openapi::GeocodeItem>, content_type = "application/json"),
    responses(
        (status = 200, description = "One result per item, in input order.",
         body = openapi::ForwardBatch),
        (status = 400, description = "The body is not a JSON array of objects, or the \
                                      query string did not parse.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 413, description = "More than 1000 items, or a body over its cap.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 415, description = "The body was not sent as `application/json`.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "The batch failed inside the server.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn geocode_batch(State(state): State<AppState>, request: Request) -> Response {
    let lookup = Lookup {
        route: "/geocode",
        prepare: geocode_request,
        answer: geocode_answer,
    };
    batch::run(state, lookup, request).await
}

/// `/geocode`'s parameters, checked.
fn geocode_request(params: GeocodeParams) -> Result<AddressGeocodeOptions, Problem> {
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
    Ok(AddressGeocodeOptions {
        address,
        locality,
        region,
        postcode,
        limit: params.limit,
        layer: layer.map(|layer| layer.as_str().to_string()),
        countries: parse_countries(params.country.as_deref())?,
    })
}

fn geocode_answer(state: &AppState, options: AddressGeocodeOptions) -> Result<ForwardCollection> {
    let address = options.address.clone();
    let (query, hits) = match state.searcher.geocode_address(options)? {
        Some(found) => (found.query, vec![found.hit]),
        None => (address, Vec::new()),
    };
    api::forward_collection(&state.pack, query, hits)
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
    let options = reverse_request(params)?;
    let collection = blocking(move || reverse_answer(&state, options)).await?;
    Ok(Json(collection))
}

/// Find the places at many points in one request.
///
/// The body is a JSON array of up to 1000 items, each taking the parameters
/// `GET /reverse` takes, plus an optional `id` echoed in its result.
/// `QUERY` takes the same body and answers the same: `POST` is for clients
/// that cannot send `QUERY`.
///
/// Each result's `response` is exactly what `GET` returns for its item, and
/// its `error` the Problem `GET` would return. Results are in input order,
/// one per item, and a bad item never fails the batch.
#[utoipa::path(
    post,
    path = "/reverse",
    tag = "geocoding",
    request_body(content = Vec<openapi::ReverseItem>, content_type = "application/json"),
    responses(
        (status = 200, description = "One result per item, in input order.",
         body = openapi::ReverseBatch),
        (status = 400, description = "The body is not a JSON array of objects, or the \
                                      query string did not parse.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 413, description = "More than 1000 items, or a body over its cap.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 415, description = "The body was not sent as `application/json`.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "The batch failed inside the server.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn reverse_batch(State(state): State<AppState>, request: Request) -> Response {
    let lookup = Lookup {
        route: "/reverse",
        prepare: reverse_request,
        answer: reverse_answer,
    };
    batch::run(state, lookup, request).await
}

/// `/reverse`'s parameters, checked.
fn reverse_request(params: ReverseParams) -> Result<GeocodeOptions, Problem> {
    bounds::validate_coords(params.lon, params.lat)?;
    Ok(GeocodeOptions {
        lon: params.lon,
        lat: params.lat,
    })
}

fn reverse_answer(state: &AppState, options: GeocodeOptions) -> Result<ReverseCollection> {
    let response = state.reverse_geocoder.reverse(options)?;
    api::reverse_collection(&state.pack, &response)
}

#[cfg(test)]
mod router_tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use super::*;
    use crate::http::bounds::{MAX_BATCH_BODY_BYTES, MAX_BATCH_ITEMS};
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
                    disputed: false,
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
        assert!(
            feature["properties"].get("confidence").is_none(),
            "a Pack never calibrated rates no answer"
        );

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
        let reply = send(router.clone(), request("POST", "/autocomplete")).await;
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
            ("POST", "/autocomplete", StatusCode::METHOD_NOT_ALLOWED),
            ("POST", "/search", StatusCode::UNSUPPORTED_MEDIA_TYPE),
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

    fn batch_request(method: &str, uri: &str, body: impl Into<String>) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.into()))
            .unwrap()
    }

    fn header_value<'a>(reply: &'a Reply, name: &str) -> Option<&'a str> {
        reply
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
    }

    /// Each item is answered as its own `GET` would be, a bad item fails
    /// alone, ids are echoed and the query string scopes every item.
    #[tokio::test]
    async fn batch_answers_each_item_alone() {
        let pack = temp_path("batch-items");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);

        let reply = send(
            router.clone(),
            batch_request(
                "POST",
                "/geocode?postcode=M5V%201A1",
                r#"[
                    {"id": "row-1", "address": "10 King Street"},
                    {"id": "row-2", "address": ""},
                    {"address": "10 King Street", "postcode": "V6B 1A1"},
                    {"id": 7, "address": "10 King Street"},
                    {"id": "row-5", "address": "10 King Street", "limit": 999},
                    {"id": "row-6", "address": ["10 King Street"]},
                    {"id": "row-7", "address": "10 King Street", "postcode": null}
                ]"#,
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.content_type(), "application/json");
        let body = reply.json();
        let results = body["results"].as_array().expect("results");
        assert_eq!(results.len(), 7);

        // The shared postcode applies: the address is in it.
        assert_eq!(results[0]["id"], "row-1");
        assert_eq!(results[0]["status"], 200);
        assert_eq!(
            results[0]["response"]["features"]
                .as_array()
                .expect("features")
                .len(),
            1
        );
        assert!(results[0].get("error").is_none());

        // Fails as `GET` fails, with the batch's request id, and the batch
        // goes on.
        assert_eq!(results[1]["id"], "row-2");
        assert_eq!(results[1]["status"], 400);
        assert_eq!(results[1]["error"]["error_code"], "invalid_query");
        assert_eq!(
            results[1]["error"]["request_id"].as_str(),
            reply.request_id().as_deref()
        );
        assert!(results[1].get("response").is_none());

        // The item's own postcode overrides the shared one; no id, no `id`.
        assert!(results[2].get("id").is_none());
        assert_eq!(results[2]["status"], 200);
        assert_eq!(results[2]["response"]["features"], serde_json::json!([]));

        // An id that is not a string is the item's error, and is not echoed.
        assert!(results[3].get("id").is_none());
        assert_eq!(results[3]["status"], 400);
        assert_eq!(results[3]["error"]["invalid_params"][0]["name"], "id");

        assert_eq!(results[4]["error"]["error_code"], "limit_too_large");
        assert_eq!(results[5]["error"]["invalid_params"][0]["name"], "address");

        // A null is an absent field, so the shared postcode still applies.
        assert_eq!(results[6]["status"], 200);
        assert_eq!(
            results[6]["response"]["features"]
                .as_array()
                .expect("features")
                .len(),
            1
        );

        assert_eq!(
            body["summary"],
            serde_json::json!({"total": 7, "succeeded": 3, "failed": 4})
        );

        let _ = std::fs::remove_dir_all(&pack);
    }

    #[tokio::test]
    async fn batch_rejects_a_bad_request_whole() {
        let pack = temp_path("batch-request-errors");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);
        let error = |reply: &Reply| reply.json()["error_code"].as_str().map(str::to_string);

        for body in [r#"{"q": "King"}"#, "[1, 2]", "[{}, \"q\"]", "not json", ""] {
            let reply = send(router.clone(), batch_request("POST", "/search", body)).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(reply.content_type(), "application/problem+json");
            assert_eq!(error(&reply).as_deref(), Some("malformed_body"), "{body}");
        }

        let too_many = format!(
            "[{}]",
            vec![r#"{"q": "King"}"#; MAX_BATCH_ITEMS + 1].join(",")
        );
        let reply = send(router.clone(), batch_request("POST", "/search", too_many)).await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error(&reply).as_deref(), Some("too_many_items"));

        let full = format!("[{}]", vec![r#"{"q": "King"}"#; MAX_BATCH_ITEMS].join(","));
        let reply = send(router.clone(), batch_request("POST", "/search", full)).await;
        assert_eq!(reply.status, StatusCode::OK);

        // Turned away on its declared length, before a byte is read.
        let mut declared = batch_request("POST", "/search", "[]");
        declared.headers_mut().insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from(MAX_BATCH_BODY_BYTES + 1),
        );
        let reply = send(router.clone(), declared).await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error(&reply).as_deref(), Some("body_too_large"));

        // And on what arrives, when no length is declared.
        let oversized = " ".repeat(MAX_BATCH_BODY_BYTES + 1);
        let reply = send(router.clone(), batch_request("POST", "/search", oversized)).await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error(&reply).as_deref(), Some("body_too_large"));

        for content_type in [None, Some("text/plain"), Some("application/jsonx")] {
            let mut request = batch_request("POST", "/search", "[]");
            match content_type {
                Some(value) => request.headers_mut().insert(
                    header::CONTENT_TYPE,
                    header::HeaderValue::from_static(value),
                ),
                None => request.headers_mut().remove(header::CONTENT_TYPE),
            };
            let reply = send(router.clone(), request).await;
            assert_eq!(
                reply.status,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{content_type:?}"
            );
            assert_eq!(error(&reply).as_deref(), Some("unsupported_media_type"));
        }

        let mut charset = batch_request("POST", "/search", "[]");
        charset.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("Application/JSON; charset=utf-8"),
        );
        let reply = send(router.clone(), charset).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.json()["summary"],
            serde_json::json!({"total": 0, "succeeded": 0, "failed": 0})
        );

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// QUERY reaches the batch, every answer of a batchable route says how
    /// to batch, and the methods it does not take are a 405 that lists the
    /// ones it does.
    #[tokio::test]
    async fn batchable_routes_take_query_and_post() {
        let pack = temp_path("batch-methods");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);

        let body = r#"[{"lon": -79.0, "lat": 43.0}, {"lon": 999, "lat": 0}]"#;
        let tagged = |method| {
            let mut request = batch_request(method, "/reverse", body);
            request
                .headers_mut()
                .insert("x-request-id", header::HeaderValue::from_static("same"));
            request
        };
        let query = send(router.clone(), tagged("QUERY")).await;
        let post = send(router.clone(), tagged("POST")).await;
        assert_eq!(query.status, StatusCode::OK);
        assert_eq!(query.json()["results"][0]["status"], 200);
        assert_eq!(query.json()["results"][1]["status"], 400);
        assert_eq!(query.body, post.body);

        for route in [
            "/search?q=King",
            "/geocode?address=10%20King%20Street",
            "/reverse?lon=0&lat=0",
        ] {
            let reply = send(router.clone(), request("GET", route)).await;
            assert_eq!(reply.status, StatusCode::OK, "{route}");
            assert_eq!(header_value(&reply, "allow"), Some(batch::METHODS));
            assert_eq!(
                header_value(&reply, "accept-query"),
                Some("application/json")
            );
        }
        // A Problem keeps the route's headers when the id is written in.
        let reply = send(router.clone(), request("GET", "/search?q=")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            header_value(&reply, "accept-query"),
            Some("application/json")
        );
        assert!(reply.json()["request_id"].is_string());

        for method in ["HEAD", "PUT", "DELETE", "PATCH", "OPTIONS"] {
            let reply = send(router.clone(), request(method, "/geocode")).await;
            assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
            assert_eq!(
                header_value(&reply, "allow"),
                Some(batch::METHODS),
                "{method}"
            );
        }
        // A route without a batch still takes GET only.
        let reply = send(router.clone(), request("QUERY", "/autocomplete")).await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(header_value(&reply, "allow"), Some("GET"));
        assert!(header_value(&reply, "accept-query").is_none());

        let _ = std::fs::remove_dir_all(&pack);
    }

    #[derive(Debug, Deserialize)]
    struct PanicParams {
        q: String,
    }

    fn panic_on_boom(state: &AppState, query: String) -> Result<ForwardCollection> {
        assert_ne!(query, "boom", "label {} out of range", 7);
        api::forward_collection(&state.pack, query, Vec::new())
    }

    /// A panic fails its item with a bare 500, logged with the item's place,
    /// and the items around it are answered.
    #[tokio::test]
    async fn batch_item_panic_fails_only_that_item() {
        let pack = temp_path("batch-panic");
        write_pack(&pack);
        let log = ErrorLog::capture();
        let mut state = state_for(&pack, true);
        state.error_log = log.clone();
        let router = Router::new()
            .route(
                "/boom",
                axum::routing::post(
                    |State(state): State<AppState>, request: Request<Body>| async move {
                        let lookup = Lookup {
                            route: "/boom",
                            prepare: |params: PanicParams| Ok(params.q),
                            answer: panic_on_boom,
                        };
                        batch::run(state, lookup, request).await
                    },
                ),
            )
            .layer(middleware::from_fn_with_state(
                log.clone(),
                request_id::propagate,
            ))
            .with_state(state);

        let mut tagged = batch_request(
            "POST",
            "/boom",
            r#"[{"q": "fine"}, {"id": "x", "q": "boom"}, {"q": "fine"}]"#,
        );
        tagged
            .headers_mut()
            .insert("cf-ray", header::HeaderValue::from_static("ray-batch"));
        let reply = send(router, tagged).await;
        assert_eq!(reply.status, StatusCode::OK);
        let body = reply.json();
        let statuses = body["results"]
            .as_array()
            .expect("results")
            .iter()
            .map(|result| result["status"].as_u64().expect("status"))
            .collect::<Vec<_>>();
        assert_eq!(statuses, [200, 500, 200]);
        let error = &body["results"][1]["error"];
        assert_eq!(body["results"][1]["id"], "x");
        assert_eq!(error["error_code"], "internal_error");
        assert_eq!(error["request_id"], "ray-batch");
        assert!(error.get("detail").is_none());
        assert!(!body.to_string().contains("out of range"));

        let lines = log_lines(&log);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["request_id"], "ray-batch");
        assert_eq!(lines[0]["route"], "/boom");
        assert_eq!(lines[0]["item"], 1);
        assert!(
            lines[0]["cause"]
                .as_str()
                .expect("cause")
                .contains("label 7 out of range")
        );

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
