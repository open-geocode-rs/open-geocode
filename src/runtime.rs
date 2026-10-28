use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::error::{Error, Result};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, Request, State, rejection::QueryRejection},
    handler::{Handler, HandlerWithoutStateExt},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{MethodFilter, MethodRouter, on},
};
use rayon::{ThreadPool, ThreadPoolBuilder};
use serde::Deserialize;
use tokio::{net::TcpListener, sync::Semaphore, task};
use tower_http::services::{ServeDir, ServeFile};

use crate::{
    api::{self, ForwardCollection, ReverseCollection},
    http::{
        batch::{self, Lookup},
        batches::{self, BatchStore},
        bounds, health, method, openapi,
        problem::{InvalidParam, Problem},
        request_id::{self, ErrorLog},
        shutdown::shutdown_signal,
    },
    pack::Reader,
    record::Layer,
    resources::Resources,
    reverse::{GeocodeOptions, PackReverseGeocoder},
    search::{AddressGeocodeOptions, TextAutocompleteOptions, TextSearchOptions, TextSearcher},
    util::hilbert::hilbert_key,
};

pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 30;

/// Seconds a request that timed out is told to wait.
const TIMEOUT_RETRY_AFTER_SECONDS: u32 = 1;

/// How long a stopping server waits for running batches to record that they
/// stopped. Each looks at its cancel flag every window of rows.
const BATCH_SETTLE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub pack: PathBuf,
    /// Static demo files to serve; without them only the API is served.
    pub demo: Option<PathBuf>,
    pub bind: SocketAddr,
    /// The basemap PMTiles archive the demo shows, when there is one.
    pub basemap: Option<PathBuf>,
    /// Longest a lookup or a `QUERY`/`POST` batch may take before it is
    /// answered `503`. Zero for no limit.
    pub request_timeout: Duration,
    /// Longest to let in-flight requests finish once Ctrl-C or SIGTERM
    /// arrives. Zero drops them at once.
    pub shutdown_timeout: Duration,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pack: Arc<Reader>,
    searcher: Arc<TextSearcher>,
    reverse_geocoder: Arc<PackReverseGeocoder>,
    ready: Arc<AtomicBool>,
    error_log: ErrorLog,
    batch_pool: Arc<ThreadPool>,
    batch_slots: Arc<Semaphore>,
    batches: Arc<BatchStore>,
    request_timeout: Duration,
}

impl AppState {
    /// Open the Pack at `path`, keeping `/batches` files in `batches`.
    pub(crate) fn open(path: &Path, batches: &Path) -> Result<Self> {
        let pack = Arc::new(Reader::open(path)?);
        let resources = Resources::probe();
        // Every query looks things up all over the Pack: read it in now
        // when it fits, so the first queries do not each wait on faults.
        pack.container()
            .advise_random_reads(resources.available_memory_bytes);
        let searcher = TextSearcher::from_pack(Arc::clone(&pack))?;
        let reverse_geocoder = PackReverseGeocoder::from_pack(Arc::clone(&pack))?;
        // Every CPU, at low priority: batches take all the CPU single
        // lookups leave idle, and a single lookup never waits for a core.
        let batch_pool = ThreadPoolBuilder::new()
            .num_threads(resources.usable_cpus)
            .thread_name(|index| format!("batch-{index}"))
            .start_handler(|_| crate::resources::lower_this_thread())
            .build()
            .map_err(std::io::Error::other)?;
        Ok(Self {
            pack,
            searcher: Arc::new(searcher),
            reverse_geocoder: Arc::new(reverse_geocoder),
            ready: Arc::new(AtomicBool::new(true)),
            error_log: ErrorLog::default(),
            batch_pool: Arc::new(batch_pool),
            // Batches share the pool's cores, so running more of them at once
            // than there are cores only slows each one down.
            batch_slots: Arc::new(Semaphore::new(resources.usable_cpus)),
            batches: Arc::new(BatchStore::open(batches.to_path_buf())?),
            request_timeout: Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
        })
    }

    pub(crate) fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub(crate) fn batches(&self) -> &BatchStore {
        &self.batches
    }

    /// Where batch items run.
    pub(crate) fn batch_pool(&self) -> &ThreadPool {
        &self.batch_pool
    }

    /// One permit per batch running; a batch without one is refused.
    pub(crate) fn batch_slots(&self) -> &Arc<Semaphore> {
        &self.batch_slots
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
    let state = AppState::open(&options.pack, &batches::default_dir(options.bind.port()))?
        .with_request_timeout(options.request_timeout);
    tokio::spawn(batches::sweep_forever(state.clone()));
    let app = build_router(
        state.clone(),
        options.demo.as_deref(),
        options.basemap.as_deref(),
    );

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
    run(
        listener,
        app,
        state,
        shutdown_signal(),
        options.shutdown_timeout,
    )
    .await
}

/// Serve until `stop` resolves, then drain for at most `drain`.
///
/// Split out from [`serve`] so a test can stop it without a signal.
async fn run(
    listener: TcpListener,
    app: Router,
    state: AppState,
    stop: impl Future<Output = ()> + Send + 'static,
    drain: Duration,
) -> Result<()> {
    let ready = Arc::clone(&state.ready);
    let (stopping_tx, stopping_rx) = tokio::sync::oneshot::channel();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            stop.await;
            ready.store(false, Ordering::Release);
            eprintln!(
                "Shutting down: finishing in-flight requests, for at most {} s",
                drain.as_secs()
            );
            let _ = stopping_tx.send(());
        })
        .into_future();
    tokio::pin!(server);
    // axum waits for open connections with no bound of its own, and a batch
    // upload can hold one for minutes. The clock starts at the signal, not at
    // startup.
    let deadline = async {
        if stopping_rx.await.is_ok() {
            tokio::time::sleep(drain).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! {
        result = &mut server => result.map_err(Error::from)?,
        () = deadline => eprintln!("Shutdown timeout reached; dropping open connections"),
    }
    // A batch can outlive its request (`Prefer: respond-async`), and its
    // blocking thread would otherwise hold the process open until it ends.
    state.batches().stop_running(BATCH_SETTLE).await;
    eprintln!("Stopped");
    Ok(())
}

/// Answer `503` to a request that outlives the request timeout.
///
/// Dropping the handler's future cancels what runs on the async side only. A
/// batch stops for real: `batch::answer` holds a `StopOnDrop`, which the drop
/// sets, so its items not yet started are skipped and its batch slot frees.
/// A single lookup runs on `spawn_blocking`, which cannot be interrupted: it
/// finishes in the background and its answer is discarded. The client is
/// freed on time; the CPU is not.
async fn deadline(State(limit): State<Duration>, request: Request, next: Next) -> Response {
    if limit.is_zero() {
        return next.run(request).await;
    }
    match tokio::time::timeout(limit, next.run(request)).await {
        Ok(response) => response,
        Err(_) => Problem::timed_out(TIMEOUT_RETRY_AFTER_SECONDS).into_response(),
    }
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
        .route("/search", get_only(search))
        .route("/autocomplete", get_only(autocomplete))
        .route("/geocode", batchable(geocode, geocode_batch))
        .route("/reverse", batchable(reverse, reverse_batch))
        // `route_layer` covers the four routes above and no later one:
        // `/batches` streams for minutes under its own stall cutoff, and the
        // probes and the spec must answer however slow the lookups are.
        .route_layer(middleware::from_fn_with_state(
            state.request_timeout,
            deadline,
        ))
        .route(
            "/batches",
            only(MethodFilter::POST, "POST", batches::create),
        )
        .route(
            "/batches/{id}",
            on(MethodFilter::GET, batches::status)
                .on(MethodFilter::DELETE, batches::delete)
                .on(MethodFilter::HEAD, || async {
                    Problem::method_not_allowed("DELETE, GET")
                })
                .fallback(|| async { Problem::method_not_allowed("DELETE, GET") }),
        )
        .route(
            "/batches/{id}/results",
            only(MethodFilter::GET, "GET", batches::results),
        )
        .route(
            "/batches/{id}/cancel",
            only(MethodFilter::POST, "POST", batches::cancel),
        )
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
        // After the fallback, so the limit covers QUERY as well as POST.
        .layer(DefaultBodyLimit::max(bounds::MAX_BATCH_BODY_BYTES))
        .layer(middleware::map_response(batch::advertise))
}

/// A route that takes one method. HEAD is refused too, rather than routed to
/// a GET handler, and every other method is a 405 naming the one it takes.
fn only<H, T>(filter: MethodFilter, allow: &'static str, handler: H) -> MethodRouter<AppState>
where
    H: Handler<T, AppState>,
    T: 'static,
{
    let refuse = move || async move { Problem::method_not_allowed(allow) };
    on(filter, handler)
        .on(MethodFilter::HEAD, refuse)
        .fallback(refuse)
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

/// Search addresses
///
/// Geocodes free-form text, such as a full address, to coordinates. Results
/// are sorted by `score`, highest first.
#[utoipa::path(
    get,
    path = "/search",
    tag = "geocoding",
    params(
        ("q" = String, Query,
         description = "Text to geocode, such as a full address.",
         example = "10 King Street, Toronto",
         min_length = 1, max_length = 256),
        ("limit" = Option<usize>, Query,
         description = "Maximum number of results. `0`, the default, lets the \
                        server choose.",
         minimum = 0, maximum = 50),
        ("layer" = Option<Layer>, Query,
         description = "Return only results of this layer. An unknown layer \
                        returns `400`."),
        ("country" = Option<String>, Query,
         description = "Return only results in these countries, as comma-separated \
                        ISO 3166-1 alpha-2 codes. A code that is not two letters \
                        returns `400`. A code the Pack does not cover returns no \
                        results.",
         example = "NZ,AU"),
        ("focus" = Option<String>, Query,
         description = "Point to prefer results near, as `lon,lat`. Breaks ties \
                        between equally good matches only.",
         example = "-79.38,43.65"),
        ("bbox" = Option<String>, Query,
         description = "Return only results inside this box, as \
                        `min_lon,min_lat,max_lon,max_lat`. The box cannot cross \
                        the antimeridian.",
         example = "-79.64,43.58,-79.12,43.86"),
    ),
    responses(
        (status = 200, description = "Matching results, highest `score` first.",
         body = ForwardCollection),
        (status = 400, description = "`q` is empty or too long, `limit` is too \
                                      large, `focus` or `bbox` is out of range, \
                                      or a parameter is malformed.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "Internal server error.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 503, description = "The lookup ran past the server's request \
                                      timeout. Retry after the `Retry-After` \
                                      header's seconds.",
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

/// Autocomplete addresses
///
/// Suggests addresses for partial text, as a user types. An empty or very
/// short `q` returns no results.
#[utoipa::path(
    get,
    path = "/autocomplete",
    tag = "geocoding",
    params(
        ("q" = Option<String>, Query,
         description = "Text typed so far.",
         example = "10 King St",
         max_length = 256),
        ("limit" = Option<usize>, Query,
         description = "Maximum number of results. `0`, the default, lets the \
                        server choose.",
         minimum = 0, maximum = 20),
        ("layer" = Option<Layer>, Query,
         description = "Return only results of this layer. An unknown layer \
                        returns `400`."),
        ("country" = Option<String>, Query,
         description = "Return only results in these countries, as comma-separated \
                        ISO 3166-1 alpha-2 codes. A code that is not two letters \
                        returns `400`. A code the Pack does not cover returns no \
                        results.",
         example = "NZ,AU"),
        ("focus" = Option<String>, Query,
         description = "Point to prefer results near, as `lon,lat`. Breaks ties \
                        between equally good matches only.",
         example = "-79.38,43.65"),
        ("bbox" = Option<String>, Query,
         description = "Return only results inside this box, as \
                        `min_lon,min_lat,max_lon,max_lat`. The box cannot cross \
                        the antimeridian.",
         example = "-79.64,43.58,-79.12,43.86"),
    ),
    responses(
        (status = 200, description = "Suggested results, highest `score` first.",
         body = ForwardCollection),
        (status = 400, description = "`q` is too long, `limit` is too large, \
                                      `focus` or `bbox` is out of range, or a \
                                      parameter is malformed.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "Internal server error.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 503, description = "The lookup ran past the server's request \
                                      timeout. Retry after the `Retry-After` \
                                      header's seconds.",
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

// For callers holding street, town and postcode in separate fields: joined
// into one string, the parser would have to split them apart again, and a
// town in the string could outrank the street. `geocode batch` runs this
// same lookup per row.
/// Geocode a structured address
///
/// Geocodes an address given as separate fields, and returns the best match
/// or no result. `address` is matched on its own, then checked against
/// `locality`, `region` and `postcode`.
///
/// - A house number missing from the data is interpolated between its
///   neighbours (`accuracy: interpolated`), or answered with its street
///   (`accuracy: street`).
/// - A street matches `postcode` only when postcode data confirms it.
/// - A different house is never returned: `10/1367` matches as written or
///   as 1367, never as house 10.
/// - `geocoding.query` is the form of the address that matched, which can
///   differ from `address`.
#[utoipa::path(
    get,
    path = "/geocode",
    tag = "geocoding",
    params(
        ("address" = String, Query,
         description = "House number and street, without town or postcode.",
         example = "10 King Street", min_length = 1, max_length = 256),
        ("locality" = Option<String>, Query,
         description = "City or town.",
         example = "Toronto", max_length = 256),
        ("region" = Option<String>, Query,
         description = "State, province or region.",
         example = "Ontario", max_length = 256),
        ("postcode" = Option<String>, Query,
         description = "Postal code.", example = "M5V 1A1", max_length = 256),
        ("limit" = Option<usize>, Query,
         description = "Number of candidates to consider. `0`, the default, lets \
                        the server choose. At most one result is returned.",
         minimum = 0, maximum = 50),
        ("layer" = Option<Layer>, Query,
         description = "Consider only candidates of this layer. An unknown layer \
                        returns `400`."),
        ("country" = Option<String>, Query,
         description = "Return only results in these countries, as comma-separated \
                        ISO 3166-1 alpha-2 codes. A code that is not two letters \
                        returns `400`. A code the Pack does not cover returns no \
                        results.",
         example = "NZ,AU"),
    ),
    responses(
        (status = 200,
         description = "The best match, or no features when nothing matches.",
         body = ForwardCollection),
        (status = 400, description = "`address` is empty or too long, another \
                                      field is too long, `limit` is too large, or \
                                      a parameter is malformed.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "Internal server error.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 503, description = "The lookup ran past the server's request \
                                      timeout. Retry after the `Retry-After` \
                                      header's seconds.",
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
    params(
        ("address" = Option<String>, Query,
         description = "House number and street, without town or postcode. Applies \
                        to every item; an item's own field overrides it.",
         max_length = 256),
        ("locality" = Option<String>, Query,
         description = "City or town. Applies to every item; an item's own field \
                        overrides it.",
         max_length = 256),
        ("region" = Option<String>, Query,
         description = "State, province or region. Applies to every item; an \
                        item's own field overrides it.",
         max_length = 256),
        ("postcode" = Option<String>, Query,
         description = "Postal code. Applies to every item; an item's own field \
                        overrides it.",
         max_length = 256),
        ("limit" = Option<usize>, Query,
         description = "Number of candidates to consider. Applies to every item; \
                        an item's own field overrides it.",
         minimum = 0, maximum = 50),
        ("layer" = Option<Layer>, Query,
         description = "Consider only candidates of this layer. Applies to every \
                        item; an item's own field overrides it."),
        ("country" = Option<String>, Query,
         description = "Comma-separated ISO 3166-1 alpha-2 codes. Applies to every \
                        item; an item's own field overrides it.",
         example = "CA"),
    ),
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
        (status = 503, description = "Every batch slot is taken, or the batch ran \
                                      past the server's request timeout. Retry \
                                      after the `Retry-After` header's seconds.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn geocode_batch(State(state): State<AppState>, request: Request) -> Response {
    batch::run(state, GEOCODE, request).await
}

/// `/geocode` as a batch item, in `QUERY /geocode` and in `/batches`.
pub(crate) const GEOCODE: Lookup<
    GeocodeParams,
    AddressGeocodeOptions,
    ForwardCollection,
    [String; 3],
> = Lookup {
    route: "/geocode",
    prepare: geocode_request,
    place: geocode_place,
    answer: geocode_answer,
};

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

/// Addresses in one postcode and town read the same part of the Pack, so a
/// batch looks them up together, as `geocode batch` does its rows.
fn geocode_place(options: &AddressGeocodeOptions) -> [String; 3] {
    let field = |value: &Option<String>| value.as_deref().unwrap_or_default().to_lowercase();
    [
        field(&options.postcode).replace(' ', ""),
        field(&options.region),
        field(&options.locality),
    ]
}

fn geocode_answer(state: &AppState, options: AddressGeocodeOptions) -> Result<ForwardCollection> {
    let address = options.address.clone();
    let (query, hits) = match state.searcher.geocode_address(options)? {
        Some(found) => (found.query, vec![found.hit]),
        None => (address, Vec::new()),
    };
    api::forward_collection(&state.pack, query, hits)
}

/// Reverse geocode a point
///
/// Returns the nearest address or street to a point, followed by the areas
/// that contain the point, most precise first. Only the first result carries
/// `distance_m`.
#[utoipa::path(
    get,
    path = "/reverse",
    tag = "geocoding",
    params(
        ("lon" = f64, Query, description = "Longitude in degrees.",
         example = -79.3832, minimum = -180.0, maximum = 180.0),
        ("lat" = f64, Query, description = "Latitude in degrees.",
         example = 43.6532, minimum = -90.0, maximum = 90.0),
    ),
    responses(
        (status = 200, description = "Results at the point, most precise first. \
                                      `features` is empty when nothing is near.",
         body = ReverseCollection),
        (status = 400, description = "`lon` or `lat` is missing or out of range.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "Internal server error.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 503, description = "The lookup ran past the server's request \
                                      timeout. Retry after the `Retry-After` \
                                      header's seconds.",
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
    params(
        ("lon" = Option<f64>, Query,
         description = "Longitude in degrees. Applies to every item; an item's own \
                        field overrides it.",
         minimum = -180.0, maximum = 180.0),
        ("lat" = Option<f64>, Query,
         description = "Latitude in degrees. Applies to every item; an item's own \
                        field overrides it.",
         minimum = -90.0, maximum = 90.0),
    ),
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
        (status = 503, description = "Every batch slot is taken, or the batch ran \
                                      past the server's request timeout. Retry \
                                      after the `Retry-After` header's seconds.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn reverse_batch(State(state): State<AppState>, request: Request) -> Response {
    batch::run(state, REVERSE, request).await
}

/// `/reverse` as a batch item, in `QUERY /reverse` and in `/batches`.
pub(crate) const REVERSE: Lookup<ReverseParams, GeocodeOptions, ReverseCollection, u64> = Lookup {
    route: "/reverse",
    prepare: reverse_request,
    place: reverse_place,
    answer: reverse_answer,
};

/// `/reverse`'s parameters, checked.
fn reverse_request(params: ReverseParams) -> Result<GeocodeOptions, Problem> {
    bounds::validate_coords(params.lon, params.lat)?;
    Ok(GeocodeOptions {
        lon: params.lon,
        lat: params.lat,
    })
}

/// The Pack stores records in Hilbert order, so points close on this curve
/// read neighbouring pages.
fn reverse_place(options: &GeocodeOptions) -> u64 {
    hilbert_key(
        (options.lon * 1e7).round() as i32,
        (options.lat * 1e7).round() as i32,
    )
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
        let state = AppState::open(pack_dir, &pack_dir.with_extension("batches"))
            .expect("open shared pack");
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
            ("POST", "/search", StatusCode::METHOD_NOT_ALLOWED),
            ("POST", "/geocode", StatusCode::UNSUPPORTED_MEDIA_TYPE),
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

    fn upload(uri: &str, content_type: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn csv_rows(body: &[u8]) -> Vec<BTreeMap<String, String>> {
        let mut reader = csv::Reader::from_reader(body);
        let headers = reader.headers().expect("headers").clone();
        reader
            .records()
            .map(|record| {
                let record = record.expect("record");
                headers
                    .iter()
                    .zip(record.iter())
                    .map(|(name, value)| (name.to_string(), value.to_string()))
                    .collect()
            })
            .collect()
    }

    /// A CSV comes back as it went, each row with `geocode batch`'s columns
    /// holding its `GET` answer, and the batch can be read again by its id.
    #[tokio::test]
    async fn batches_stream_a_csv_back_with_each_rows_answer() {
        let pack = temp_path("batches-csv");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);

        let body = "ref,street,town,zip\n\
                    a,10 King Street,,M5V 1A1\n\
                    b,,,\n\
                    c,10 King Street,,V6B 1A1\n\
                    d,10 King Street,,M5V 1A1\n";
        let reply = send(
            router.clone(),
            upload(
                "/batches?address=street&locality=town&postcode=zip",
                "text/csv",
                body,
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert_eq!(reply.content_type(), "text/csv");
        let location = header_value(&reply, "location")
            .expect("location")
            .to_string();

        let rows = csv_rows(&reply.body);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows.iter()
                .map(|row| row["ref"].as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c", "d"]
        );
        let get = send(
            router.clone(),
            request(
                "GET",
                "/geocode?address=10%20King%20Street&postcode=M5V%201A1",
            ),
        )
        .await
        .json();
        let coordinates = &get["features"][0]["geometry"]["coordinates"];
        assert_eq!(rows[0]["geocode_status"], "resolved");
        assert_eq!(
            rows[0]["geocode_lon"].parse::<f64>().unwrap(),
            coordinates[0].as_f64().unwrap()
        );
        assert_eq!(rows[1]["geocode_status"], "error");
        assert!(rows[1]["geocode_reason"].starts_with("invalid_query"));
        assert_eq!(rows[2]["geocode_status"], "unresolved");
        // The repeat of row a is answered from it, alike.
        assert_eq!(rows[3]["geocode_lon"], rows[0]["geocode_lon"]);

        let status = send(router.clone(), request("GET", &location)).await.json();
        assert_eq!(status["status"], "completed");
        assert_eq!(status["endpoint"], "geocode");
        assert_eq!(
            status["summary"],
            serde_json::json!({"total": 4, "succeeded": 3, "failed": 1})
        );
        assert!(status["expires_at"].is_u64());
        let again = send(
            router.clone(),
            request("GET", &format!("{location}/results")),
        )
        .await;
        assert_eq!(again.body, reply.body);

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// JSONL rows are batch items, answered as their `GET` is, each tagged
    /// with its place in the upload.
    #[tokio::test]
    async fn batches_answer_jsonl_rows_as_their_get() {
        let pack = temp_path("batches-jsonl");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);

        let body = "{\"id\": \"p\", \"lon\": -79.0, \"lat\": 43.0}\n\
                    \n\
                    {\"lon\": 999, \"lat\": 0}\n\
                    not json\n";
        let reply = send(
            router.clone(),
            upload("/batches?endpoint=reverse", "application/jsonl", body),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert_eq!(reply.content_type(), "application/jsonl");
        let mut lines = std::str::from_utf8(&reply.body)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        lines.sort_by_key(|line| line["row"].as_u64());
        assert_eq!(lines.len(), 3);

        let get = send(router.clone(), request("GET", "/reverse?lon=-79&lat=43"))
            .await
            .json();
        assert_eq!(lines[0]["id"], "p");
        assert_eq!(lines[0]["status"], 200);
        assert_eq!(lines[0]["response"], get);
        assert_eq!(lines[1]["status"], 400);
        assert_eq!(lines[1]["error"]["error_code"], "invalid_coordinate");
        assert_eq!(lines[2]["error"]["error_code"], "malformed_body");

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// `Prefer: respond-async` is answered once the upload is in, and the
    /// results are read from the batch.
    #[tokio::test]
    async fn batches_answer_at_once_when_asked() {
        let pack = temp_path("batches-async");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);

        let mut request_ = upload("/batches", "text/csv", "Address,City\n10 King Street,\n");
        request_
            .headers_mut()
            .insert("prefer", header::HeaderValue::from_static("respond-async"));
        let reply = send(router.clone(), request_).await;
        assert_eq!(reply.status, StatusCode::ACCEPTED);
        let id = reply.json()["id"].as_str().expect("id").to_string();
        assert_eq!(
            header_value(&reply, "location"),
            Some(format!("/batches/{id}").as_str())
        );

        let results = send(
            router.clone(),
            request("GET", &format!("/batches/{id}/results")),
        )
        .await;
        let rows = csv_rows(&results.body);
        assert_eq!(rows[0]["geocode_status"], "resolved");
        let status = send(router.clone(), request("GET", &format!("/batches/{id}"))).await;
        assert_eq!(status.json()["status"], "completed");

        let deleted = send(router.clone(), request("DELETE", &format!("/batches/{id}"))).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        let gone = send(router.clone(), request("GET", &format!("/batches/{id}"))).await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// What `/batches` cannot read is refused before it starts.
    #[tokio::test]
    async fn batches_refuse_what_they_cannot_read() {
        let pack = temp_path("batches-refused");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);
        let code = |reply: &Reply| reply.json()["error_code"].as_str().map(str::to_string);

        let reply = send(router.clone(), upload("/batches", "application/json", "[]")).await;
        assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

        let reply = send(
            router.clone(),
            upload("/batches", "text/csv", "foo,bar\n1,2\n"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(
            reply.json()["detail"]
                .as_str()
                .unwrap()
                .contains("foo, bar")
        );

        let reply = send(
            router.clone(),
            upload("/batches?address=street", "text/csv", "foo,bar\n1,2\n"),
        )
        .await;
        assert_eq!(code(&reply).as_deref(), Some("malformed_parameter"));
        assert_eq!(reply.json()["invalid_params"][0]["name"], "street");

        let reply = send(
            router.clone(),
            upload("/batches", "application/jsonl", "{}\n"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);

        let reply = send(
            router.clone(),
            upload("/batches?endpoint=search", "application/jsonl", "{}\n"),
        )
        .await;
        assert_eq!(reply.json()["invalid_params"][0]["name"], "endpoint");

        for (method, uri) in [
            ("GET", "/batches/nope"),
            ("GET", "/batches/nope/results"),
            ("POST", "/batches/nope/cancel"),
            ("DELETE", "/batches/nope"),
        ] {
            let reply = send(router.clone(), request(method, uri)).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {uri}");
        }
        for (method, uri, allow) in [
            ("GET", "/batches", "POST"),
            ("PUT", "/batches/nope", "DELETE, GET"),
            ("POST", "/batches/nope/results", "GET"),
        ] {
            let reply = send(router.clone(), request(method, uri)).await;
            assert_eq!(
                reply.status,
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {uri}"
            );
            assert_eq!(header_value(&reply, "allow"), Some(allow));
        }

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// A finished batch is still served after a restart, and one that was
    /// running when the server stopped is failed, not left running forever.
    #[tokio::test]
    async fn batches_outlive_a_restart() {
        let pack = temp_path("batches-restart");
        write_pack(&pack);
        let spool = pack.with_extension("batches");
        let _ = std::fs::remove_dir_all(&spool);
        let router = build_router(state_for(&pack, true), None, None);
        let reply = send(
            router,
            upload("/batches", "text/csv", "street\n10 King Street\n"),
        )
        .await;
        let location = header_value(&reply, "location")
            .expect("location")
            .to_string();
        std::fs::write(
            spool.join("interrupted.json"),
            serde_json::json!({
                "id": "interrupted",
                "endpoint": "geocode",
                "status": "running",
                "input_format": "csv",
                "output_format": "csv",
                "summary": {"total": 0, "succeeded": 0, "failed": 0},
                "created_at": 0,
                "results_url": "/batches/interrupted/results",
            })
            .to_string(),
        )
        .expect("write a running batch");

        let router = build_router(state_for(&pack, true), None, None);
        let status = send(router.clone(), request("GET", &location)).await.json();
        assert_eq!(status["status"], "completed");
        let results = send(
            router.clone(),
            request("GET", &format!("{location}/results")),
        )
        .await;
        assert_eq!(results.body, reply.body);
        let interrupted = send(router, request("GET", "/batches/interrupted"))
            .await
            .json();
        assert_eq!(interrupted["status"], "failed");
        assert!(interrupted["error"].as_str().unwrap().contains("restarted"));

        let _ = std::fs::remove_dir_all(&pack);
        let _ = std::fs::remove_dir_all(&spool);
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

    /// JSON keeps the last of two equal keys, but `GET` refuses a repeated
    /// parameter, so the item is refused the same way.
    #[tokio::test]
    async fn batch_item_with_a_repeated_field_fails_as_get_does() {
        let pack = temp_path("batch-repeated");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);

        let get = send(
            router.clone(),
            request("GET", "/geocode?address=10%20King%20Street&address=x"),
        )
        .await;
        let batch = send(
            router,
            batch_request(
                "POST",
                "/geocode",
                r#"[{"address": "10 King Street", "address": "x"}]"#,
            ),
        )
        .await;
        let result = &batch.json()["results"][0];
        assert_eq!(result["status"], get.status.as_u16());
        assert_eq!(result["error"]["error_code"], get.json()["error_code"]);

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// With every slot taken a batch is refused at once, told when to retry,
    /// and single lookups still answer.
    #[tokio::test]
    async fn batch_is_refused_when_every_slot_is_taken() {
        let pack = temp_path("batch-busy");
        write_pack(&pack);
        let state = state_for(&pack, true);
        let slots = Arc::clone(state.batch_slots());
        let taken = slots
            .clone()
            .try_acquire_many_owned(slots.available_permits() as u32)
            .expect("take every slot");
        let router = build_router(state, None, None);

        let body = r#"[{"address": "10 King Street"}]"#;
        let reply = send(router.clone(), batch_request("POST", "/geocode", body)).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.json()["error_code"], "busy");
        assert_eq!(header_value(&reply, "retry-after"), Some("1"));

        let single = send(
            router.clone(),
            request("GET", "/geocode?address=10%20King%20Street"),
        )
        .await;
        assert_eq!(single.status, StatusCode::OK);

        drop(taken);
        let reply = send(router, batch_request("POST", "/geocode", body)).await;
        assert_eq!(reply.status, StatusCode::OK);

        let _ = std::fs::remove_dir_all(&pack);
    }

    #[tokio::test]
    async fn batch_rejects_a_bad_request_whole() {
        let pack = temp_path("batch-request-errors");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);
        let error = |reply: &Reply| reply.json()["error_code"].as_str().map(str::to_string);

        for body in [r#"{"q": "King"}"#, "[1, 2]", "[{}, \"q\"]", "not json", ""] {
            let reply = send(router.clone(), batch_request("POST", "/geocode", body)).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(reply.content_type(), "application/problem+json");
            assert_eq!(error(&reply).as_deref(), Some("malformed_body"), "{body}");
        }

        let too_many = format!(
            "[{}]",
            vec![r#"{"address": "King"}"#; MAX_BATCH_ITEMS + 1].join(",")
        );
        let reply = send(router.clone(), batch_request("POST", "/geocode", too_many)).await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error(&reply).as_deref(), Some("too_many_items"));

        let full = format!(
            "[{}]",
            vec![r#"{"address": "King"}"#; MAX_BATCH_ITEMS].join(",")
        );
        let reply = send(router.clone(), batch_request("POST", "/geocode", full)).await;
        assert_eq!(reply.status, StatusCode::OK);

        // Turned away on its declared length, before a byte is read.
        let mut declared = batch_request("POST", "/geocode", "[]");
        declared.headers_mut().insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from(MAX_BATCH_BODY_BYTES + 1),
        );
        let reply = send(router.clone(), declared).await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error(&reply).as_deref(), Some("body_too_large"));

        // And on what arrives, when no length is declared.
        let oversized = " ".repeat(MAX_BATCH_BODY_BYTES + 1);
        for method in ["POST", "QUERY"] {
            let request = batch_request(method, "/geocode", oversized.clone());
            let reply = send(router.clone(), request).await;
            assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE, "{method}");
            assert_eq!(error(&reply).as_deref(), Some("body_too_large"));
        }

        for content_type in [None, Some("text/plain"), Some("application/jsonx")] {
            let mut request = batch_request("POST", "/geocode", "[]");
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

        let mut charset = batch_request("POST", "/geocode", "[]");
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
        let reply = send(router.clone(), request("GET", "/geocode?address=")).await;
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
                            place: |_: &String| (),
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

    fn raw_get(addr: SocketAddr, path: &str) -> std::io::Result<(u16, String)> {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(addr)?;
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n"
        )?;
        let mut reply = String::new();
        stream.read_to_string(&mut reply)?;
        let status = reply
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse().ok())
            .ok_or_else(|| std::io::Error::other(format!("no status in {reply:?}")))?;
        let body = reply.split("\r\n\r\n").nth(1).unwrap_or_default();
        Ok((status, body.to_string()))
    }

    /// `/slow` answers after a while and `/hang` never does; `started` is
    /// told when either is entered.
    fn drain_app(state: AppState, started: Arc<tokio::sync::Notify>) -> Router {
        use axum::routing::get;
        let on_slow = Arc::clone(&started);
        Router::new()
            .route(
                "/slow",
                get(move || async move {
                    on_slow.notify_one();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    "done"
                }),
            )
            .route(
                "/hang",
                get(move || async move {
                    started.notify_one();
                    std::future::pending::<&str>().await
                }),
            )
            .route("/readyz", get(health::readyz))
            .with_state(state)
    }

    /// After the stop signal the server is not ready and takes no new
    /// connection, and the request it was answering still completes.
    #[tokio::test]
    async fn shutdown_lets_an_in_flight_request_finish() {
        let pack = temp_path("shutdown-drain");
        write_pack(&pack);
        let state = state_for(&pack, true);
        let started = Arc::new(tokio::sync::Notify::new());
        let app = drain_app(state.clone(), Arc::clone(&started));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(run(
            listener,
            app.clone(),
            state.clone(),
            async move {
                let _ = stopped.await;
            },
            Duration::from_secs(10),
        ));

        let client = task::spawn_blocking(move || raw_get(addr, "/slow"));
        started.notified().await;
        assert!(state.is_ready());
        stop.send(()).expect("stop");
        while state.is_ready() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let readyz = send(app, request("GET", "/readyz")).await;
        assert_eq!(readyz.status, StatusCode::SERVICE_UNAVAILABLE);
        let mut refused = false;
        for _ in 0..200 {
            if std::net::TcpStream::connect(addr).is_err() {
                refused = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(refused, "the listener is still accepting");

        let (status, body) = client.await.expect("client").expect("reply");
        assert_eq!((status, body.as_str()), (200, "done"));
        server.await.expect("server").expect("clean stop");

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// A request that never ends cannot hold the server past the drain
    /// timeout.
    #[tokio::test]
    async fn shutdown_gives_up_on_a_stuck_request_after_the_drain_timeout() {
        use std::io::Write;
        let pack = temp_path("shutdown-timeout");
        write_pack(&pack);
        let state = state_for(&pack, true);
        let started = Arc::new(tokio::sync::Notify::new());
        let app = drain_app(state.clone(), Arc::clone(&started));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let server = tokio::spawn(run(
            listener,
            app,
            state.clone(),
            async move {
                started.notified().await;
            },
            Duration::from_millis(300),
        ));
        let mut stuck = std::net::TcpStream::connect(addr).expect("connect");
        write!(stuck, "GET /hang HTTP/1.1\r\nHost: test\r\n\r\n").expect("send");

        let began = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("shutdown is bounded")
            .expect("server")
            .expect("clean stop");
        assert!(began.elapsed() >= Duration::from_millis(300));
        assert!(!state.is_ready());

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// A batch still running at shutdown is cancelled, keeping the rows it
    /// answered, even when its upload then ends as if it were complete. It
    /// is not left `running` for the next start to fail, nor recorded as
    /// `completed` with rows missing.
    #[tokio::test]
    async fn shutdown_cancels_a_running_batch() {
        let pack = temp_path("shutdown-batch");
        write_pack(&pack);
        let spool = pack.with_extension("batches");
        let _ = std::fs::remove_dir_all(&spool);
        let state = state_for(&pack, true);
        let router = build_router(state.clone(), None, None);

        // Past the 64 KiB the reader sniffs before it answers, then held open.
        let rows = format!("street\n{}", "10 King Street\n".repeat(6000));
        let (feed, chunks) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        feed.send(rows.into_bytes()).await.expect("first chunk");
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/batches")
                    .header(header::CONTENT_TYPE, "text/csv")
                    .body(streamed(chunks))
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::CREATED);
        let location = response.headers()[header::LOCATION]
            .to_str()
            .expect("location")
            .to_string();
        let status = || async { send(router.clone(), request("GET", &location)).await.json() };
        // The reader answers whole windows of 256 rows and then waits for the
        // rest of the next one, so 23 windows in it is parked on the upload.
        let answered = 23 * 256;
        let mut running = status().await;
        for _ in 0..1000 {
            if running["summary"]["total"].as_u64() >= Some(answered) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            running = status().await;
        }
        assert_eq!(running["status"], "running");
        assert_eq!(running["summary"]["total"], answered);

        state
            .batches()
            .stop_running(Duration::from_millis(100))
            .await;
        drop(feed);
        let mut stopped = status().await;
        for _ in 0..500 {
            if stopped["status"] != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            stopped = status().await;
        }
        assert_eq!(stopped["status"], "cancelled");
        assert!(stopped["completed_at"].is_u64());
        assert!(stopped["summary"]["total"].as_u64() >= Some(answered));
        let record = std::fs::read_to_string(
            spool.join(format!("{}.json", stopped["id"].as_str().expect("id"))),
        )
        .expect("record on disk");
        assert!(record.contains("\"cancelled\""), "{record}");

        drop(response);
        let _ = std::fs::remove_dir_all(&pack);
        let _ = std::fs::remove_dir_all(&spool);
    }

    /// A request body that goes on for as long as `chunks` stays open.
    fn streamed(chunks: tokio::sync::mpsc::Receiver<Vec<u8>>) -> Body {
        Body::from_stream(futures_util::stream::unfold(chunks, |mut chunks| async {
            let chunk = chunks.recv().await?;
            Some((Ok::<_, std::io::Error>(chunk), chunks))
        }))
    }

    /// A lookup that outruns the timeout is answered with the 503 Problem
    /// and a `Retry-After`; the probes are not held to it.
    ///
    /// The blocking pool is cut to one thread and that thread parked, so a
    /// lookup waits for it for as long as the test says, with no clock in
    /// the test beyond the timeout itself.
    #[test]
    fn a_lookup_slower_than_the_timeout_gets_a_503_problem() {
        let pack = temp_path("timeout-single");
        write_pack(&pack);
        let state = state_for(&pack, true).with_request_timeout(Duration::from_millis(100));
        let router = build_router(state, None, None);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");

        runtime.block_on(async {
            let (release, parked) = std::sync::mpsc::channel::<()>();
            let blocker = task::spawn_blocking(move || {
                let _ = parked.recv();
            });
            for route in [
                "/search?q=10%20King%20Street",
                "/autocomplete?q=10%20King",
                "/geocode?address=10%20King%20Street",
                "/reverse?lon=-79&lat=43",
            ] {
                let reply = send(router.clone(), request("GET", route)).await;
                assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{route}");
                assert_eq!(reply.content_type(), "application/problem+json");
                assert_eq!(reply.json()["error_code"], "timeout", "{route}");
                assert!(reply.json()["request_id"].is_string());
                assert_eq!(header_value(&reply, "retry-after"), Some("1"));
            }
            for route in ["/healthz", "/readyz", "/openapi.json"] {
                let reply = send(router.clone(), request("GET", route)).await;
                assert_eq!(reply.status, StatusCode::OK, "{route}");
            }

            release.send(()).expect("release");
            blocker.await.expect("blocker");
            let reply = send(
                router.clone(),
                request("GET", "/geocode?address=10%20King%20Street"),
            )
            .await;
            assert_eq!(reply.status, StatusCode::OK);
        });

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// A timeout of zero is no timeout.
    #[test]
    fn a_zero_timeout_never_expires() {
        let pack = temp_path("timeout-zero");
        write_pack(&pack);
        let state = state_for(&pack, true).with_request_timeout(Duration::ZERO);
        let router = build_router(state, None, None);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");

        runtime.block_on(async {
            let (release, parked) = std::sync::mpsc::channel::<()>();
            let blocker = task::spawn_blocking(move || {
                let _ = parked.recv();
            });
            let lookup = tokio::spawn(send(
                router.clone(),
                request("GET", "/geocode?address=10%20King%20Street"),
            ));
            tokio::time::sleep(Duration::from_millis(300)).await;
            release.send(()).expect("release");
            blocker.await.expect("blocker");
            assert_eq!(lookup.await.expect("lookup").status, StatusCode::OK);
        });

        let _ = std::fs::remove_dir_all(&pack);
    }

    static SLOW_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn slow_answer(state: &AppState, query: String) -> Result<ForwardCollection> {
        SLOW_CALLS.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        api::forward_collection(&state.pack, query, Vec::new())
    }

    /// A batch past the timeout gets the 503, and its work stops: items not
    /// yet started are skipped and the batch slot comes back.
    #[tokio::test]
    async fn a_batch_past_the_timeout_is_stopped() {
        let pack = temp_path("timeout-batch");
        write_pack(&pack);
        let state = state_for(&pack, true).with_request_timeout(Duration::from_millis(100));
        let slots = Arc::clone(state.batch_slots());
        let all_slots = slots.available_permits();
        let router = Router::new()
            .route(
                "/slow",
                axum::routing::post(
                    |State(state): State<AppState>, request: Request<Body>| async move {
                        let lookup = Lookup {
                            route: "/slow",
                            prepare: |params: PanicParams| Ok(params.q),
                            place: |_: &String| (),
                            answer: slow_answer,
                        };
                        batch::run(state, lookup, request).await
                    },
                ),
            )
            .route_layer(middleware::from_fn_with_state(
                state.request_timeout,
                deadline,
            ))
            .with_state(state);

        let items = (0..MAX_BATCH_ITEMS)
            .map(|index| format!(r#"{{"q": "street {index}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let reply = send(router, batch_request("POST", "/slow", format!("[{items}]"))).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.json()["error_code"], "timeout");
        assert_eq!(header_value(&reply, "retry-after"), Some("1"));

        for _ in 0..500 {
            if slots.available_permits() == all_slots {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(slots.available_permits(), all_slots, "the slot is held");
        let calls = SLOW_CALLS.load(Ordering::SeqCst);
        assert!(calls < MAX_BATCH_ITEMS, "{calls} items ran");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(SLOW_CALLS.load(Ordering::SeqCst), calls, "work went on");

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// An upload that takes longer than the timeout is not cut short by it.
    #[tokio::test]
    async fn batches_are_not_held_to_the_request_timeout() {
        let pack = temp_path("timeout-batches");
        write_pack(&pack);
        let spool = pack.with_extension("batches");
        let _ = std::fs::remove_dir_all(&spool);
        let state = state_for(&pack, true).with_request_timeout(Duration::from_millis(50));
        let router = build_router(state, None, None);

        let (feed, chunks) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let rows = format!("street\n{}", "10 King Street\n".repeat(6000));
        feed.send(rows.into_bytes()).await.expect("first chunk");
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = feed.send(b"10 King Street\n".to_vec()).await;
        });
        let mut slow = Request::builder()
            .method("POST")
            .uri("/batches")
            .header(header::CONTENT_TYPE, "text/csv")
            .body(streamed(chunks))
            .unwrap();
        slow.headers_mut()
            .insert("prefer", header::HeaderValue::from_static("respond-async"));
        let began = std::time::Instant::now();
        let reply = send(router, slow).await;
        assert!(began.elapsed() >= Duration::from_millis(300));
        assert_eq!(reply.status, StatusCode::ACCEPTED);

        let _ = std::fs::remove_dir_all(&pack);
        let _ = std::fs::remove_dir_all(&spool);
    }
}
