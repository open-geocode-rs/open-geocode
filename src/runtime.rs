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
    middleware,
    response::{IntoResponse, Response},
    routing::{MethodFilter, MethodRouter, on},
};
use rayon::{ThreadPool, ThreadPoolBuilder};
use serde::Deserialize;
use tokio::{net::TcpListener, sync::Semaphore, task};
use tokio_util::sync::CancellationToken;
use tower_http::{
    compression::CompressionLayer,
    services::{ServeDir, ServeFile},
};

use crate::{
    api::{self, ForwardCollection, ReverseCollection},
    http::{
        batch::{self, Lookup},
        bounds, health,
        jobs::{self, JobStore},
        method, openapi,
        param::Param,
        problem::{InvalidParam, Problem},
        request_id::{self, ErrorLog},
        shutdown::shutdown_signal,
        version,
    },
    ids::{self, Catalogue},
    labels,
    limits::Limits,
    pack::Reader,
    record::{Layer, ObjectType},
    resources::Resources,
    reverse::{GeocodeOptions, PackReverseGeocoder},
    search::{
        AddressGeocodeOptions, TextAutocompleteOptions, TextSearchHit, TextSearchOptions,
        TextSearcher, suggest,
    },
    stop::{self, CancelOnDrop, Stop},
    util::hilbert::hilbert_key,
};

/// Seconds a refused single lookup is told to wait: lookups take
/// milliseconds, so a slot frees within it.
const LOOKUP_RETRY_AFTER_SECONDS: u32 = 1;

/// Kubernetes' default grace period, and actix-web's default shutdown
/// timeout.
pub const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub pack: PathBuf,
    /// Static demo files to serve; without them only the API is served.
    pub demo: Option<PathBuf>,
    pub bind: SocketAddr,
    /// The basemap PMTiles archive the demo shows, when there is one.
    pub basemap: Option<PathBuf>,
    /// Where job results are kept: one directory per port under the
    /// system's temporary directory when not given.
    pub job_dir: Option<PathBuf>,
    pub limits: Limits,
    /// Longest the server takes to stop once Ctrl-C or SIGTERM arrives;
    /// what is still open then is dropped. Zero drops it at once.
    pub shutdown_timeout: Duration,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pack: Arc<Reader>,
    searcher: Arc<TextSearcher>,
    places: Arc<TextSearcher>,
    /// `None` for a Pack built before the catalogue.
    catalogue: Option<Arc<Catalogue>>,
    reverse_geocoder: Arc<PackReverseGeocoder>,
    ready: Arc<AtomicBool>,
    /// Cancelled once the server starts to stop.
    stopping: CancellationToken,
    error_log: ErrorLog,
    batch_pool: Arc<ThreadPool>,
    batch_slots: Arc<Semaphore>,
    jobs: Arc<JobStore>,
    limits: Limits,
    /// One permit per single lookup running; one without a permit is refused.
    lookup_slots: Arc<Semaphore>,
    /// `/openapi.json` for this server's limits, built once.
    spec: Arc<str>,
}

impl AppState {
    /// Open the Pack at `path`, keeping `/jobs` files in `jobs`, under
    /// `limits`.
    pub(crate) fn open(path: &Path, jobs: &Path, limits: Limits) -> Result<Self> {
        let pack = Arc::new(Reader::open(path)?);
        let resources = Resources::probe();
        // Every query looks things up all over the Pack: read it in now
        // when it fits, so the first queries do not each wait on faults.
        pack.container()
            .advise_random_reads(resources.available_memory_bytes);
        let searcher = TextSearcher::from_pack(Arc::clone(&pack))?;
        let reverse_geocoder = PackReverseGeocoder::from_pack(Arc::clone(&pack))?;
        let catalogue = Catalogue::open(pack.container())?;
        // Every CPU, at low priority: batches and jobs take all the CPU
        // single lookups leave idle, and a single lookup never waits for a
        // core.
        let batch_pool = ThreadPoolBuilder::new()
            .num_threads(resources.usable_cpus)
            .thread_name(|index| format!("batch-{index}"))
            .start_handler(|_| crate::resources::lower_this_thread())
            .build()
            .map_err(std::io::Error::other)?;
        Ok(Self {
            pack,
            places: Arc::new(searcher.places()),
            catalogue,
            searcher: Arc::new(searcher),
            reverse_geocoder: Arc::new(reverse_geocoder),
            ready: Arc::new(AtomicBool::new(true)),
            stopping: CancellationToken::new(),
            error_log: ErrorLog::default(),
            batch_pool: Arc::new(batch_pool),
            // Batches and jobs share the pool's cores, so running more of them
            // at once than there are cores only slows each one down.
            batch_slots: Arc::new(Semaphore::new(resources.usable_cpus)),
            jobs: Arc::new(JobStore::open(jobs.to_path_buf(), limits.job_retention())?),
            lookup_slots: Arc::new(Semaphore::new(
                limits
                    .max_concurrent_lookups(resources.usable_cpus)
                    .min(Semaphore::MAX_PERMITS),
            )),
            spec: openapi::document(&limits)?.into(),
            limits,
        })
    }

    pub(crate) fn stopping(&self) -> &CancellationToken {
        &self.stopping
    }

    pub(crate) fn jobs(&self) -> &JobStore {
        &self.jobs
    }

    pub(crate) fn limits(&self) -> &Limits {
        &self.limits
    }

    pub(crate) fn spec(&self) -> &str {
        &self.spec
    }

    /// Where batch items and job rows run.
    pub(crate) fn batch_pool(&self) -> &ThreadPool {
        &self.batch_pool
    }

    /// One permit per batch or job running; one without a permit is refused.
    pub(crate) fn batch_slots(&self) -> &Arc<Semaphore> {
        &self.batch_slots
    }

    pub(crate) fn error_log(&self) -> &ErrorLog {
        &self.error_log
    }

    pub(crate) fn pack(&self) -> &Reader {
        &self.pack
    }

    /// True once the Pack is loaded and the Runtime can answer API traffic
    /// (ADR 0017 Decision 28).
    pub(crate) fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

/// `/places`: places by name in `q`, or by id in `ids`.
#[derive(Debug, Deserialize)]
pub(crate) struct PlacesParams {
    q: Param,
    /// Comma-separated stable ids, such as `osm:node:123`.
    ids: Param,
    #[serde(default)]
    limit: usize,
    country: Param,
    focus: Param,
    bbox: Param,
}

/// `/autocomplete`: unlike `/places`, an empty `q` answers no suggestions
/// rather than an error, as a user who cleared the box expects.
#[derive(Debug, Deserialize)]
pub(crate) struct AutocompleteParams {
    q: Param,
    #[serde(default)]
    limit: usize,
    /// Comma-separated ISO 3166-1 alpha-2 codes; see [`parse_countries`].
    country: Param,
    /// Left as a string here and parsed by [`parse_layer`], so an unknown
    /// layer is a 400 naming the valid ones rather than a serde rejection
    /// reading `unknown variant`.
    layer: Param,
    /// `lon,lat`; see [`bounds::parse_focus`].
    focus: Param,
    /// `min_lon,min_lat,max_lon,max_lat`; see [`bounds::parse_bbox`].
    bbox: Param,
}

/// Resolve the `layer` filter, rejecting a layer that does not exist.
///
/// An unknown layer is a 400 listing the valid ones, not a filter that
/// matches nothing: a typo would look exactly like "no results here", and a
/// client could not tell the two apart.
///
/// An empty value means "no filter", which is what a form that always sends
/// the key produces.
fn parse_layer(layer: &Param) -> Result<Option<Layer>, Problem> {
    let value = match layer {
        Param::Missing | Param::Empty => return Ok(None),
        Param::Value(value) => value.trim(),
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
/// from several covers them all, and this narrows a search to part of it: a
/// filter on one index, not a route to one of many.
///
/// A code that is not two letters is rejected, like an unknown `layer`: a
/// typo that silently matched nothing would look exactly like "no results
/// here". A country the Pack does not cover is *not* rejected, because it is
/// a legitimate empty answer.
///
/// An empty value means "no filter", which is what a form that always sends
/// the key produces.
fn parse_countries(country: &Param) -> Result<Vec<String>, Problem> {
    let value = match country {
        Param::Missing | Param::Empty => return Ok(Vec::new()),
        Param::Value(value) => value.trim(),
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

/// `/geocode`: an address as free text in `q`, or one whose parts the
/// caller already has apart.
#[derive(Debug, Deserialize)]
pub(crate) struct GeocodeParams {
    q: Param,
    /// `lon,lat`, with `q` only; see [`bounds::parse_focus`].
    focus: Param,
    /// `min_lon,min_lat,max_lon,max_lat`, with `q` only; see
    /// [`bounds::parse_bbox`].
    bbox: Param,
    address: Param,
    locality: Param,
    region: Param,
    postcode: Param,
    #[serde(default)]
    limit: usize,
    layer: Param,
    /// Comma-separated ISO 3166-1 alpha-2 codes; see [`parse_countries`].
    country: Param,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReverseParams {
    lon: f64,
    lat: f64,
    /// See [`parse_layer`].
    layer: Param,
}

pub async fn serve(options: ServeOptions) -> Result<()> {
    let jobs_dir = options
        .job_dir
        .clone()
        .unwrap_or_else(|| jobs::default_dir(options.bind.port()));
    let state = AppState::open(&options.pack, &jobs_dir, options.limits)?;
    tokio::spawn(jobs::sweep_forever(state.clone()));
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

/// Split from [`serve`] so a test can stop it without a signal.
async fn run(
    listener: TcpListener,
    app: Router,
    state: AppState,
    stop: impl Future<Output = ()>,
    budget: Duration,
) -> Result<()> {
    let stopping = state.stopping().clone();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(stopping.clone().cancelled_owned())
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        served = &mut server => return served.map_err(Error::from),
        () = stop => {}
    }
    eprintln!("Shutting down, for at most {} s", budget.as_secs());
    // Jobs first: a job's upload is a request as long as the job, so requests
    // drain only once jobs stop.
    state.jobs().cancel_running();
    stopping.cancel();
    let stopped = tokio::time::timeout(budget, async {
        let served = (&mut server).await;
        // A job can outlive its request (`Prefer: respond-async`).
        state.jobs().stopped().await;
        served
    })
    .await;
    match stopped {
        Ok(served) => served.map_err(Error::from)?,
        Err(_) => eprintln!("Shutdown timeout reached; dropping what is still open"),
    }
    eprintln!("Stopped");
    Ok(())
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
    let max_body = state.limits().max_batch_body_bytes();
    let mut app: Router<AppState> = Router::new()
        .route("/places", batchable(places, places_batch, max_body))
        .route("/autocomplete", get_only(autocomplete))
        .route("/geocode", batchable(geocode, geocode_batch, max_body))
        .route("/reverse", batchable(reverse, reverse_batch, max_body))
        .route("/jobs", only(MethodFilter::POST, "POST", jobs::create))
        .route(
            "/jobs/{id}",
            on(MethodFilter::GET, jobs::status)
                .on(MethodFilter::DELETE, jobs::delete)
                .on(MethodFilter::HEAD, || async {
                    Problem::method_not_allowed("DELETE, GET")
                })
                .fallback(|| async { Problem::method_not_allowed("DELETE, GET") }),
        )
        .route(
            "/jobs/{id}/results",
            only(MethodFilter::GET, "GET", jobs::results),
        )
        .route(
            "/jobs/{id}/cancel",
            only(MethodFilter::POST, "POST", jobs::cancel),
        )
        .route("/openapi.json", get_only(openapi::openapi_json))
        .route("/healthz", get_only(health::healthz))
        .route("/readyz", get_only(health::readyz))
        .route("/version", get_only(version::version));

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
    // gzip or zstd by Accept-Encoding. The default predicate leaves bodies
    // under 32 bytes, ranges and already-encoded bodies alone.
    .layer(CompressionLayer::new())
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
fn batchable<H, T, B, U>(single: H, batch: B, max_body: usize) -> MethodRouter<AppState>
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
        .layer(DefaultBodyLimit::max(max_body))
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

/// Run one lookup under the server's limits: refused with 503 `busy` while
/// as many run as the server allows, and stopped at its next check past the
/// lookup timeout, or once this future is dropped, as axum drops it when the
/// client disconnects.
async fn lookup<T, F>(state: &AppState, work: F) -> Result<T, Problem>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let Ok(permit) = Arc::clone(&state.lookup_slots).try_acquire_owned() else {
        return Err(Problem::busy(LOOKUP_RETRY_AFTER_SECONDS));
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let _cancel = CancelOnDrop(Arc::clone(&cancel));
    let stop = Stop::new(state.limits.lookup_timeout(), cancel);
    // The work holds the slot, not this future: a lookup whose client left
    // holds it until it stops.
    blocking(move || {
        let _permit = permit;
        stop::within(stop, work)
    })
    .await
}

fn malformed_parameters(_: QueryRejection) -> Problem {
    Problem::unparsable_parameters()
}

/// Search places
///
/// Finds places (POIs) by name, such as a museum, a station or a shop, and
/// the words of where they are, in `q`. Results are sorted by `score`,
/// highest first. Addresses, streets and areas are `/geocode`'s.
///
/// Or finds places and areas by their stable ids in `ids`, such as
/// `osm:node:1383955044` from a shared link, a saved place or a map tile.
/// Results come in the order asked; an id no record has is left out.
#[utoipa::path(
    get,
    path = "/places",
    tag = "Geocoding",
    params(
        ("q" = Option<String>, Query,
         description = "Name of the place, with where it is when known. Give `q` \
                        or `ids`, not both; one sent empty counts as given.",
         example = "Rijksmuseum Amsterdam",
         max_length = 256),
        ("ids" = Option<String>, Query,
         description = "Comma-separated stable ids of places, areas or streets, at \
                        most the server's batch item limit (1000 by default), such \
                        as `osm:node:1383955044,osm:way:4`. A street answers to \
                        the id of any of its ways; when a place and a street \
                        share an id, the place answers.",
         example = "osm:node:1383955044"),
        ("limit" = Option<usize>, Query,
         description = "Maximum number of results. `0`, the default, lets the \
                        server choose.",
         minimum = 0, maximum = 50),
        ("country" = Option<String>, Query,
         description = "Return only results in these countries, as comma-separated \
                        ISO 3166-1 alpha-2 codes. A code that is not two letters \
                        returns `400`. A code the Pack does not cover returns no \
                        results.",
         example = "NL"),
        ("focus" = Option<String>, Query,
         description = "Point to prefer results near, as `lon,lat`. Breaks ties \
                        between equally good matches only.",
         example = "4.88,52.36"),
        ("bbox" = Option<String>, Query,
         description = "Return only results inside this box, as \
                        `min_lon,min_lat,max_lon,max_lat`. The box cannot cross \
                        the antimeridian.",
         example = "4.73,52.28,5.07,52.43"),
    ),
    responses(
        (status = 200, description = "Matching places, highest `score` first.",
         body = ForwardCollection),
        (status = 400, description = "Neither `q` nor `ids` is given, or both \
                                      are, `q` is empty or too long, `limit` is \
                                      too large, `focus` or `bbox` is out of \
                                      range, `ids` names no id, an id is \
                                      malformed or there are more than the \
                                      server's batch item limit, or a parameter \
                                      is malformed.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "Internal server error.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 501, description = "`ids` was given to a Pack built before it had \
                                      an id catalogue. Rebuild the Pack.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn places(
    State(state): State<AppState>,
    params: Result<Query<PlacesParams>, QueryRejection>,
) -> Result<Json<ForwardCollection>, Problem> {
    let Query(params) = params.map_err(malformed_parameters)?;
    let request = places_request(&state, params)?;
    let worker = state.clone();
    let collection = lookup(&state, move || places_answer(&worker, request)).await?;
    Ok(Json(collection))
}

/// Find a batch of places
///
/// The body is a JSON array of items, at most the server's item limit (1000
/// by default). Each takes the parameters `GET /places` takes, `q` or `ids`,
/// plus an optional `id` echoed in its result. The request's own
/// query-string parameters apply to every item, and an item's own fields
/// override them: `?country=NL` scopes every item to the Netherlands.
/// `QUERY` takes the same body and answers the same: `POST` is for clients
/// that cannot send `QUERY`.
///
/// Each result's `response` is exactly what `GET` returns for its item, and
/// its `error` the Problem `GET` would return. Results are in input order,
/// one per item, and a bad item never fails the batch. An item's `ids` is
/// held to the same limit as `GET`'s, the server's item limit, on its own.
#[utoipa::path(
    post,
    path = "/places",
    tag = "Geocoding",
    params(
        ("q" = Option<String>, Query,
         description = "Name of the place. Applies to every item; an item's own \
                        field overrides it.",
         max_length = 256),
        ("ids" = Option<String>, Query,
         description = "Comma-separated stable ids. Applies to every item; an \
                        item's own field overrides it."),
        ("limit" = Option<usize>, Query,
         description = "Maximum number of results. Applies to every item; an \
                        item's own field overrides it.",
         minimum = 0, maximum = 50),
        ("country" = Option<String>, Query,
         description = "Comma-separated ISO 3166-1 alpha-2 codes. Applies to every \
                        item; an item's own field overrides it.",
         example = "NL"),
        ("focus" = Option<String>, Query,
         description = "Point to prefer results near, as `lon,lat`. Applies to \
                        every item; an item's own field overrides it."),
        ("bbox" = Option<String>, Query,
         description = "Return only results inside this box, as \
                        `min_lon,min_lat,max_lon,max_lat`. Applies to every item; \
                        an item's own field overrides it."),
    ),
    request_body(content = Vec<openapi::PlacesItem>, content_type = "application/json"),
    responses(
        (status = 200, description = "One result per item, in input order.",
         body = openapi::ForwardBatch),
        (status = 400, description = "The body is not a JSON array of objects, or the \
                                      query string did not parse.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 413, description = "More items than the server's limit, or a body over its cap.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 415, description = "The body was not sent as `application/json`.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "The batch failed inside the server.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 503, description = "Every slot batches and jobs share is taken. Retry after the \
                                      `Retry-After` header's seconds.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn places_batch(State(state): State<AppState>, request: Request) -> Response {
    batch::run(state, PLACES, request).await
}

/// `/places` as a batch item, in `QUERY /places`. Not a `/jobs` endpoint: a
/// job is a file of addresses or points too long for one request, and one
/// batch already holds a saved-place list, ids and names alike.
pub(crate) const PLACES: Lookup<PlacesParams, PlacesRequest, ForwardCollection, String> = Lookup {
    route: "/places",
    prepare: places_request,
    place: places_place,
    answer: places_answer,
};

#[derive(Clone)]
pub(crate) enum PlacesRequest {
    Text(TextSearchOptions),
    Ids {
        catalogue: Arc<Catalogue>,
        /// In the order asked, without the ids no record can have.
        ids: Vec<(ObjectType, i64)>,
    },
}

/// `/places`' parameters, checked.
fn places_request(state: &AppState, params: PlacesParams) -> Result<PlacesRequest, Problem> {
    let q = match (params.q, params.ids) {
        (Param::Missing, Param::Missing) => {
            return Err(Problem::invalid_query("give q, or ids"));
        }
        (Param::Empty, Param::Missing) => return Err(bounds::empty_query()),
        (Param::Value(q), Param::Missing) => q,
        (Param::Missing, Param::Empty) => return Err(no_ids()),
        (Param::Missing, Param::Value(ids)) => {
            let ids = place_ids(&ids, state.limits.max_batch_items())?;
            let Some(catalogue) = state.catalogue.clone() else {
                return Err(Problem::pack_outdated(
                    "this Pack has no id catalogue: rebuild it to look places up by id",
                ));
            };
            return Ok(PlacesRequest::Ids { catalogue, ids });
        }
        (Param::Empty | Param::Value(_), Param::Empty | Param::Value(_)) => {
            return Err(Problem::malformed_parameter("give q or ids, not both")
                .with_invalid_params(vec![InvalidParam::new("ids", "given with q")]));
        }
    };
    text_request(TextParams {
        q,
        limit: params.limit,
        country: params.country,
        focus: params.focus,
        bbox: params.bbox,
    })
    .map(PlacesRequest::Text)
}

/// Names that read alike sit together, as `/geocode`'s text queries do. An
/// id's block is known only once the catalogue is read, so id lookups take no
/// place and run first.
fn places_place(request: &PlacesRequest) -> String {
    match request {
        PlacesRequest::Text(options) => options.query.to_lowercase(),
        PlacesRequest::Ids { .. } => String::new(),
    }
}

fn places_answer(state: &AppState, request: PlacesRequest) -> Result<ForwardCollection> {
    let (catalogue, ids) = match request {
        PlacesRequest::Text(options) => {
            let hits = state.places.search(&options)?;
            return api::forward_collection(&state.pack, options.query, hits);
        }
        PlacesRequest::Ids { catalogue, ids } => (catalogue, ids),
    };
    let records = state.pack.records();
    let mut hits = Vec::new();
    for (object_type, object_id) in &ids {
        for record_id in catalogue.find(records, *object_type, *object_id)? {
            hits.push(TextSearchHit::of_record(
                record_id,
                state.pack.record_summary(record_id)?,
            ));
        }
    }
    let asked = ids
        .iter()
        .map(|(kind, id)| labels::osm_record_id(*kind, *id))
        .collect::<Vec<_>>();
    api::forward_collection(&state.pack, asked.join(","), hits)
}

fn no_ids() -> Problem {
    Problem::malformed_parameter("ids names no id")
        .with_invalid_params(vec![InvalidParam::new("ids", "empty")])
}

/// `ids` as the catalogue reads them, at most `max` of them: the server's
/// batch item limit, so one setting caps the lookups one request makes
/// whether they come as items or as ids.
fn place_ids(ids: &str, max: usize) -> Result<Vec<(ObjectType, i64)>, Problem> {
    let ids = ids
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        // One past the limit is enough to refuse the list, so a list of a
        // million ids costs no more than one just over it.
        .take(max.saturating_add(1))
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Err(no_ids());
    }
    if ids.len() > max {
        return Err(Problem::limit_too_large(vec![InvalidParam::new(
            "ids",
            format!("at most {max} ids"),
        )]));
    }
    let parsed = ids
        .into_iter()
        .map(|id| {
            ids::parse(id).map_err(|_| {
                Problem::malformed_parameter(format!(
                    "{id:?} is no record id, such as osm:node:123"
                ))
                .with_invalid_params(vec![InvalidParam::new("ids", "not a record id")])
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(parsed.into_iter().flatten().collect())
}

struct TextParams {
    /// As sent, holding at least one word.
    q: String,
    limit: usize,
    country: Param,
    focus: Param,
    bbox: Param,
}

fn text_request(params: TextParams) -> Result<TextSearchOptions, Problem> {
    let TextParams {
        q,
        limit,
        country,
        focus,
        bbox,
    } = params;
    let query = bounds::validate_query(&q)?;
    bounds::validate_search_limit(limit)?;
    Ok(TextSearchOptions {
        query,
        limit,
        layer: None,
        countries: parse_countries(&country)?,
        focus: bounds::parse_focus(&focus)?,
        bbox: bounds::parse_bbox(&bbox)?,
    })
}

/// Autocomplete addresses and places
///
/// Suggests addresses and places for partial text, as a user types, in one
/// list. A query that starts with a house number suggests addresses first; a
/// place whose name starts as the query does comes before the addresses. An
/// empty or very short `q` returns no results.
///
/// When nothing starts as typed, the words are matched in any order, as the
/// area a result is in (`starbucks ballard`) and as the words an
/// abbreviation stands for (`pike pl mkt`, `2101 n 34`).
#[utoipa::path(
    get,
    path = "/autocomplete",
    tag = "Geocoding",
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
         description = "Return only results of this layer: `poi` suggests places \
                        alone, any other layer addresses alone. An unknown layer \
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
        (status = 200, description = "Suggested results, best first.",
         body = ForwardCollection),
        (status = 400, description = "`q` is too long, `limit` is too large, \
                                      `focus` or `bbox` is out of range, or a \
                                      parameter is malformed.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "Internal server error.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn autocomplete(
    State(state): State<AppState>,
    params: Result<Query<AutocompleteParams>, QueryRejection>,
) -> Result<Json<ForwardCollection>, Problem> {
    let Query(params) = params.map_err(malformed_parameters)?;

    // Nothing typed yet is no suggestions, not an error, so only the length is
    // bounded here; the searcher answers an empty or very short query with
    // none, and caps the result count itself.
    let query = match &params.q {
        Param::Missing | Param::Empty => String::new(),
        Param::Value(q) => bounds::validate_query_length(q)?,
    };
    bounds::validate_autocomplete_limit(params.limit)?;
    let layer = parse_layer(&params.layer)?;
    let countries = parse_countries(&params.country)?;
    let focus = bounds::parse_focus(&params.focus)?;
    let bbox = bounds::parse_bbox(&params.bbox)?;

    let options = TextAutocompleteOptions {
        query: query.clone(),
        limit: params.limit,
        layer: layer.map(|layer| layer.as_str().to_string()),
        countries,
        focus,
        bbox,
    };
    let worker = state.clone();
    let collection = lookup(&state, move || {
        let hits = match layer {
            Some(Layer::Poi) => worker.places.autocomplete(&TextAutocompleteOptions {
                layer: None,
                ..options
            })?,
            Some(_) => worker.searcher.autocomplete(&options)?,
            None => suggest(&worker.searcher, &worker.places, &options)?,
        };
        api::forward_collection(&worker.pack, query, hits)
    })
    .await?;

    Ok(Json(collection))
}

// For callers holding street, town and postcode in separate fields: joined
// into one string, the parser would have to split them apart again, and a
// town in the string could outrank the street. `geocode batch` runs this
// same lookup per row.
/// Geocode an address
///
/// Geocodes an address to coordinates, given as free text in `q` or as
/// separate fields. Answers are addresses, streets, postcodes and areas,
/// never places: those are `/places`'.
///
/// With `q`, returns the matches best first, highest `score` first, as many
/// as `limit`.
///
/// With the fields, returns the best match or no result. `address` is
/// matched on its own, then checked against `locality`, `region` and
/// `postcode`.
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
    tag = "Geocoding",
    params(
        ("q" = Option<String>, Query,
         description = "The address as free text, such as a full address. Give \
                        `q` or the address fields, not both; a field sent empty \
                        counts as given.",
         example = "10 King Street, Toronto", max_length = 256),
        ("focus" = Option<String>, Query,
         description = "With `q`: point to prefer results near, as `lon,lat`. \
                        Breaks ties between equally good matches only.",
         example = "-79.38,43.65"),
        ("bbox" = Option<String>, Query,
         description = "With `q`: return only results inside this box, as \
                        `min_lon,min_lat,max_lon,max_lat`. The box cannot cross \
                        the antimeridian.",
         example = "-79.64,43.58,-79.12,43.86"),
        ("address" = Option<String>, Query,
         description = "House number and street, without town or postcode.",
         example = "10 King Street", max_length = 256),
        ("locality" = Option<String>, Query,
         description = "City or town.",
         example = "Toronto", max_length = 256),
        ("region" = Option<String>, Query,
         description = "State, province or region.",
         example = "Ontario", max_length = 256),
        ("postcode" = Option<String>, Query,
         description = "Postal code.", example = "M5V 1A1", max_length = 256),
        ("limit" = Option<usize>, Query,
         description = "With `q`, the most results to return; with the fields, the \
                        candidates to consider, of which at most one is returned. \
                        `0`, the default, lets the server choose.",
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
         description = "The matches best first, or no features when nothing matches.",
         body = ForwardCollection),
        (status = 400, description = "Neither `q` nor `address` is given, `q` and \
                                      an address field are both given, `q` or \
                                      `address` is empty, a field is too long, \
                                      `limit` is too large, or a parameter is \
                                      malformed.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "Internal server error.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn geocode(
    State(state): State<AppState>,
    params: Result<Query<GeocodeParams>, QueryRejection>,
) -> Result<Json<ForwardCollection>, Problem> {
    let Query(params) = params.map_err(malformed_parameters)?;
    let options = geocode_request(&state, params)?;
    let worker = state.clone();
    let collection = lookup(&state, move || geocode_answer(&worker, options)).await?;
    Ok(Json(collection))
}

/// Geocode a batch of addresses
///
/// The body is a JSON array of items, at most the server's item limit (1000
/// by default). Each takes the parameters `GET /geocode` takes, plus an
/// optional `id` echoed in its result. The request's own query-string
/// parameters apply to every item, and an item's own fields override them:
/// `?country=CA` scopes every item to Canada.
/// `QUERY` takes the same body and answers the same: `POST` is for clients
/// that cannot send `QUERY`.
///
/// Each result's `response` is exactly what `GET` returns for its item, and
/// its `error` the Problem `GET` would return. Results are in input order,
/// one per item, and a bad item never fails the batch.
#[utoipa::path(
    post,
    path = "/geocode",
    tag = "Geocoding",
    params(
        ("q" = Option<String>, Query,
         description = "The address as free text. Applies to every item; an item's \
                        own field overrides it.",
         max_length = 256),
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
        (status = 413, description = "More items than the server's limit, or a body over its cap.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 415, description = "The body was not sent as `application/json`.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "The batch failed inside the server.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 503, description = "Every slot batches and jobs share is taken. Retry after the \
                                      `Retry-After` header's seconds.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn geocode_batch(State(state): State<AppState>, request: Request) -> Response {
    batch::run(state, GEOCODE, request).await
}

/// `/geocode` as a batch item, in `QUERY /geocode` and in `/jobs`.
pub(crate) const GEOCODE: Lookup<GeocodeParams, GeocodeRequest, ForwardCollection, [String; 3]> =
    Lookup {
        route: "/geocode",
        prepare: geocode_request,
        place: geocode_place,
        answer: geocode_answer,
    };

#[derive(Debug, Clone)]
pub(crate) enum GeocodeRequest {
    Text(TextSearchOptions),
    Fields(AddressGeocodeOptions),
}

/// `/geocode`'s parameters, checked.
fn geocode_request(_: &AppState, params: GeocodeParams) -> Result<GeocodeRequest, Problem> {
    // A field sent empty is still sent: `q=&address=10 King Street` names two
    // ways of giving the address, and which one the caller meant is a guess.
    let fields = [
        &params.address,
        &params.locality,
        &params.region,
        &params.postcode,
    ]
    .into_iter()
    .any(Param::is_sent);
    let q = match (&params.q, fields) {
        (Param::Empty | Param::Value(_), true) => {
            return Err(Problem::malformed_parameter(
                "give the address as q or as address, locality, region and postcode, not both",
            )
            .with_invalid_params(vec![InvalidParam::new("q", "given with address fields")]));
        }
        (Param::Empty, false) => return Err(bounds::empty_query()),
        (Param::Value(q), false) => q.clone(),
        (Param::Missing, _) => {
            return geocode_fields(&params).map(GeocodeRequest::Fields);
        }
    };
    let mut options = text_request(TextParams {
        q,
        limit: params.limit,
        country: params.country,
        focus: params.focus,
        bbox: params.bbox,
    })?;
    options.layer = parse_layer(&params.layer)?.map(|layer| layer.as_str().to_string());
    Ok(GeocodeRequest::Text(options))
}

fn geocode_fields(params: &GeocodeParams) -> Result<AddressGeocodeOptions, Problem> {
    let address = match &params.address {
        Param::Missing | Param::Empty => None,
        Param::Value(address) => bounds::validate_query(address).ok(),
    }
    .ok_or_else(|| Problem::invalid_query("give q, or address with its other fields"))?;
    let locality = context_field(&params.locality)?;
    let region = context_field(&params.region)?;
    let postcode = context_field(&params.postcode)?;
    bounds::validate_search_limit(params.limit)?;
    let layer = parse_layer(&params.layer)?;
    Ok(AddressGeocodeOptions {
        address,
        locality,
        region,
        postcode,
        limit: params.limit,
        layer: layer.map(|layer| layer.as_str().to_string()),
        countries: parse_countries(&params.country)?,
    })
}

/// `locality`, `region` or `postcode`, which narrow an address search rather
/// than drive it.
fn context_field(field: &Param) -> Result<Option<String>, Problem> {
    match field {
        Param::Missing => Ok(None),
        // A form that always sends every field sends the ones left blank
        // empty; a blank locality narrows nothing, unlike a blank `address`.
        Param::Empty => Ok(None),
        Param::Value(value) => bounds::validate_query_length(value).map(Some),
    }
}

/// Addresses in one postcode and town read the same part of the Pack, so a
/// batch looks them up together, as `geocode batch` does its rows.
fn geocode_place(request: &GeocodeRequest) -> [String; 3] {
    let field = |value: &Option<String>| value.as_deref().unwrap_or_default().to_lowercase();
    match request {
        GeocodeRequest::Fields(options) => [
            field(&options.postcode).replace(' ', ""),
            field(&options.region),
            field(&options.locality),
        ],
        GeocodeRequest::Text(options) => {
            [String::new(), String::new(), options.query.to_lowercase()]
        }
    }
}

fn geocode_answer(state: &AppState, request: GeocodeRequest) -> Result<ForwardCollection> {
    let (query, hits) = match request {
        GeocodeRequest::Text(options) => {
            let hits = state.searcher.search(&options)?;
            (options.query, hits)
        }
        GeocodeRequest::Fields(options) => match state.searcher.geocode_address(&options)? {
            Some(found) => (found.query, vec![found.hit]),
            None => (options.address, Vec::new()),
        },
    };
    api::forward_collection(&state.pack, query, hits)
}

/// Reverse geocode a point
///
/// Returns the nearest address or street to a point, followed by the areas
/// that contain the point, most precise first. Only the first result carries
/// `distance_m`. With `layer`, returns only the nearest result of that layer.
#[utoipa::path(
    get,
    path = "/reverse",
    tag = "Geocoding",
    params(
        ("lon" = f64, Query, description = "Longitude in degrees.",
         example = -79.3832, minimum = -180.0, maximum = 180.0),
        ("lat" = f64, Query, description = "Latitude in degrees.",
         example = 43.6532, minimum = -90.0, maximum = 90.0),
        ("layer" = Option<Layer>, Query,
         description = "Return only the nearest result of this layer, or for an area \
                        layer the area of that layer the point lies in, and no other \
                        area. An unknown layer returns `400`."),
    ),
    responses(
        (status = 200, description = "Results at the point, most precise first. \
                                      `features` is empty when nothing is near.",
         body = ReverseCollection),
        (status = 400, description = "`lon` or `lat` is missing or out of range, or \
                                      `layer` is unknown.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "Internal server error.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn reverse(
    State(state): State<AppState>,
    params: Result<Query<ReverseParams>, QueryRejection>,
) -> Result<Json<ReverseCollection>, Problem> {
    let Query(params) = params.map_err(malformed_parameters)?;
    let options = reverse_request(&state, params)?;
    let worker = state.clone();
    let collection = lookup(&state, move || reverse_answer(&worker, options)).await?;
    Ok(Json(collection))
}

/// Reverse geocode a batch of points
///
/// The body is a JSON array of items, at most the server's item limit (1000
/// by default). Each takes the parameters `GET /reverse` takes, plus an
/// optional `id` echoed in its result.
/// `QUERY` takes the same body and answers the same: `POST` is for clients
/// that cannot send `QUERY`.
///
/// Each result's `response` is exactly what `GET` returns for its item, and
/// its `error` the Problem `GET` would return. Results are in input order,
/// one per item, and a bad item never fails the batch.
#[utoipa::path(
    post,
    path = "/reverse",
    tag = "Geocoding",
    params(
        ("lon" = Option<f64>, Query,
         description = "Longitude in degrees. Applies to every item; an item's own \
                        field overrides it.",
         minimum = -180.0, maximum = 180.0),
        ("lat" = Option<f64>, Query,
         description = "Latitude in degrees. Applies to every item; an item's own \
                        field overrides it.",
         minimum = -90.0, maximum = 90.0),
        ("layer" = Option<Layer>, Query,
         description = "Return only results of this layer. Applies to every item; an \
                        item's own field overrides it."),
    ),
    request_body(content = Vec<openapi::ReverseItem>, content_type = "application/json"),
    responses(
        (status = 200, description = "One result per item, in input order.",
         body = openapi::ReverseBatch),
        (status = 400, description = "The body is not a JSON array of objects, or the \
                                      query string did not parse.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 413, description = "More items than the server's limit, or a body over its cap.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 415, description = "The body was not sent as `application/json`.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 500, description = "The batch failed inside the server.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
        (status = 503, description = "Every slot batches and jobs share is taken. Retry after the \
                                      `Retry-After` header's seconds.",
         body = openapi::Problem, content_type = openapi::PROBLEM_JSON),
    ),
)]
pub(crate) async fn reverse_batch(State(state): State<AppState>, request: Request) -> Response {
    batch::run(state, REVERSE, request).await
}

/// `/reverse` as a batch item, in `QUERY /reverse` and in `/jobs`.
pub(crate) const REVERSE: Lookup<ReverseParams, GeocodeOptions, ReverseCollection, u64> = Lookup {
    route: "/reverse",
    prepare: reverse_request,
    place: reverse_place,
    answer: reverse_answer,
};

/// `/reverse`'s parameters, checked.
fn reverse_request(_: &AppState, params: ReverseParams) -> Result<GeocodeOptions, Problem> {
    let ReverseParams { lon, lat, layer } = params;
    bounds::validate_coords(lon, lat)?;
    Ok(GeocodeOptions {
        lon,
        lat,
        layer: parse_layer(&layer)?,
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

    use std::num::{NonZeroU64, NonZeroUsize};
    use std::time::Duration;

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
        state_with(pack_dir, ready, Limits::DEFAULT)
    }

    fn state_with(pack_dir: &Path, ready: bool, limits: Limits) -> AppState {
        let state = AppState::open(pack_dir, &pack_dir.with_extension("jobs"), limits)
            .expect("open shared pack");
        state.ready.store(ready, Ordering::Release);
        state
    }

    fn limits_with(change: impl FnOnce(&mut Limits)) -> Limits {
        let mut limits = Limits::DEFAULT;
        change(&mut limits);
        limits
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
    async fn version_names_the_build_and_the_pack() {
        let pack = temp_path("version");
        write_pack(&pack);
        let reader = Reader::open(&pack).expect("pack");
        let manifest = reader.manifest();
        let router = build_router(state_for(&pack, true), None, None);

        let reply = send(router.clone(), request("GET", "/version")).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.content_type(), "application/json");
        assert_eq!(
            reply.json(),
            serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "pack": {
                    "id": format!("{:016x}", reader.container().fingerprint()),
                    "schema_version": manifest.schema_version,
                    "built_by": manifest.crate_version,
                    "built_at_unix": manifest.built_at_unix,
                },
            })
        );
        let body = String::from_utf8_lossy(&reply.body);
        for path in [pack.as_path(), reader.path()] {
            let path = path.to_string_lossy();
            assert!(!body.contains(path.as_ref()), "{path} in {body}");
        }

        assert_eq!(
            send(router, request("HEAD", "/version")).await.status,
            StatusCode::METHOD_NOT_ALLOWED
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
        let reply = send(router.clone(), request("GET", "/geocode?q=")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.content_type(), "application/problem+json");
        assert_eq!(reply.json()["error_code"], "invalid_query");

        // Over-length query -> 400.
        let long = "a".repeat(300);
        let reply = send(
            router.clone(),
            request("GET", &format!("/geocode?q={long}")),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);

        // limit above the cap -> 400 with invalid_params naming "limit".
        let reply = send(
            router.clone(),
            request("GET", "/geocode?q=king&limit=999999"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["invalid_params"][0]["name"], "limit");

        // Malformed limit type -> 400 malformed_parameter.
        let reply = send(router.clone(), request("GET", "/geocode?q=king&limit=abc")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["error_code"], "malformed_parameter");

        // An unknown layer -> 400 naming the layers that would have been
        // accepted, not an empty answer that looks like "nothing here".
        let reply = send(
            router.clone(),
            request("GET", "/geocode?q=king&layer=bogus"),
        )
        .await;
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
            request("GET", "/geocode?q=King%20Street&layer=address"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        let reply = send(router.clone(), request("GET", "/geocode?q=king&layer=")).await;
        assert_eq!(reply.status, StatusCode::OK);

        // A valid query succeeds.
        let reply = send(router, request("GET", "/geocode?q=King%20Street&limit=5")).await;
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
        let reply = send(router.clone(), request("HEAD", "/places")).await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);

        // Unknown path -> 404 Problem Details, not a static-file response.
        let reply = send(router.clone(), request("GET", "/does-not-exist")).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.content_type(), "application/problem+json");
        assert_eq!(reply.json()["error_code"], "not_found");

        // Every response carries a request id; an inbound cf-ray is honored.
        let reply = send(router.clone(), request("GET", "/healthz")).await;
        assert!(reply.request_id().is_some());

        let mut tagged = request("GET", "/geocode?q=");
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
            ("GET", "/geocode?q=", StatusCode::BAD_REQUEST),
            ("GET", "/reverse?lon=999&lat=0", StatusCode::BAD_REQUEST),
            ("GET", "/does-not-exist", StatusCode::NOT_FOUND),
            ("POST", "/autocomplete", StatusCode::METHOD_NOT_ALLOWED),
            ("PUT", "/places", StatusCode::METHOD_NOT_ALLOWED),
            ("POST", "/places", StatusCode::UNSUPPORTED_MEDIA_TYPE),
            ("POST", "/geocode", StatusCode::UNSUPPORTED_MEDIA_TYPE),
            ("GET", "/geocode?q=King%20Street", StatusCode::OK),
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
    /// holding its `GET` answer, and the job can be read again by its id.
    #[tokio::test]
    async fn jobs_stream_a_csv_back_with_each_rows_answer() {
        let pack = temp_path("jobs-csv");
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
                "/jobs?address=street&locality=town&postcode=zip",
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
    async fn jobs_answer_jsonl_rows_as_their_get() {
        let pack = temp_path("jobs-jsonl");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);

        let body = "{\"id\": \"p\", \"lon\": -79.0, \"lat\": 43.0}\n\
                    \n\
                    {\"lon\": 999, \"lat\": 0}\n\
                    not json\n";
        let reply = send(
            router.clone(),
            upload("/jobs?endpoint=reverse", "application/jsonl", body),
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
    /// results are read from the job.
    #[tokio::test]
    async fn jobs_answer_at_once_when_asked() {
        let pack = temp_path("jobs-async");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);

        let mut request_ = upload("/jobs", "text/csv", "Address,City\n10 King Street,\n");
        request_
            .headers_mut()
            .insert("prefer", header::HeaderValue::from_static("respond-async"));
        let reply = send(router.clone(), request_).await;
        assert_eq!(reply.status, StatusCode::ACCEPTED);
        let id = reply.json()["id"].as_str().expect("id").to_string();
        assert_eq!(
            header_value(&reply, "location"),
            Some(format!("/jobs/{id}").as_str())
        );

        let results = send(
            router.clone(),
            request("GET", &format!("/jobs/{id}/results")),
        )
        .await;
        let rows = csv_rows(&results.body);
        assert_eq!(rows[0]["geocode_status"], "resolved");
        let status = send(router.clone(), request("GET", &format!("/jobs/{id}"))).await;
        assert_eq!(status.json()["status"], "completed");

        let deleted = send(router.clone(), request("DELETE", &format!("/jobs/{id}"))).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        let gone = send(router.clone(), request("GET", &format!("/jobs/{id}"))).await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// What `/jobs` cannot read is refused before it starts.
    #[tokio::test]
    async fn jobs_refuse_what_they_cannot_read() {
        let pack = temp_path("jobs-refused");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);
        let code = |reply: &Reply| reply.json()["error_code"].as_str().map(str::to_string);

        let reply = send(router.clone(), upload("/jobs", "application/json", "[]")).await;
        assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);

        let reply = send(
            router.clone(),
            upload("/jobs", "text/csv", "foo,bar\n1,2\n"),
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
            upload("/jobs?address=street", "text/csv", "foo,bar\n1,2\n"),
        )
        .await;
        assert_eq!(code(&reply).as_deref(), Some("malformed_parameter"));
        assert_eq!(reply.json()["invalid_params"][0]["name"], "street");

        let reply = send(router.clone(), upload("/jobs", "application/jsonl", "{}\n")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);

        let reply = send(
            router.clone(),
            upload("/jobs?endpoint=search", "application/jsonl", "{}\n"),
        )
        .await;
        assert_eq!(reply.json()["invalid_params"][0]["name"], "endpoint");

        for (method, uri) in [
            ("GET", "/jobs/nope"),
            ("GET", "/jobs/nope/results"),
            ("POST", "/jobs/nope/cancel"),
            ("DELETE", "/jobs/nope"),
        ] {
            let reply = send(router.clone(), request(method, uri)).await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {uri}");
        }
        for (method, uri, allow) in [
            ("GET", "/jobs", "POST"),
            ("PUT", "/jobs/nope", "DELETE, GET"),
            ("POST", "/jobs/nope/results", "GET"),
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

    /// A finished job is still served after a restart, and one that was
    /// running when the server stopped is failed, not left running forever.
    #[tokio::test]
    async fn jobs_outlive_a_restart() {
        let pack = temp_path("jobs-restart");
        write_pack(&pack);
        let spool = pack.with_extension("jobs");
        let _ = std::fs::remove_dir_all(&spool);
        let router = build_router(state_for(&pack, true), None, None);
        let reply = send(
            router,
            upload("/jobs", "text/csv", "street\n10 King Street\n"),
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
                "results_url": "/jobs/interrupted/results",
            })
            .to_string(),
        )
        .expect("write a running job");

        let router = build_router(state_for(&pack, true), None, None);
        let status = send(router.clone(), request("GET", &location)).await.json();
        assert_eq!(status["status"], "completed");
        let results = send(
            router.clone(),
            request("GET", &format!("{location}/results")),
        )
        .await;
        assert_eq!(results.body, reply.body);
        let interrupted = send(router, request("GET", "/jobs/interrupted"))
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
        let taken = Arc::clone(&slots)
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
        let limits =
            limits_with(|limits| limits.max_batch_items = NonZeroUsize::new(3).expect("3"));
        let router = build_router(state_with(&pack, true, limits), None, None);
        let error = |reply: &Reply| reply.json()["error_code"].as_str().map(str::to_string);

        for body in [r#"{"q": "King"}"#, "[1, 2]", "[{}, \"q\"]", "not json", ""] {
            let reply = send(router.clone(), batch_request("POST", "/geocode", body)).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(reply.content_type(), "application/problem+json");
            assert_eq!(error(&reply).as_deref(), Some("malformed_body"), "{body}");
        }

        let too_many = format!(
            "[{}]",
            vec![r#"{"address": "King"}"#; limits.max_batch_items() + 1].join(",")
        );
        let reply = send(router.clone(), batch_request("POST", "/geocode", too_many)).await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error(&reply).as_deref(), Some("too_many_items"));

        let full = format!(
            "[{}]",
            vec![r#"{"address": "King"}"#; limits.max_batch_items()].join(",")
        );
        let reply = send(router.clone(), batch_request("POST", "/geocode", full)).await;
        assert_eq!(reply.status, StatusCode::OK);

        // Turned away on its declared length, before a byte is read.
        let mut declared = batch_request("POST", "/geocode", "[]");
        declared.headers_mut().insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from(limits.max_batch_body_bytes() + 1),
        );
        let reply = send(router.clone(), declared).await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error(&reply).as_deref(), Some("body_too_large"));

        // And on what arrives, when no length is declared.
        let oversized = " ".repeat(limits.max_batch_body_bytes() + 1);
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
            "/places?ids=osm:node:1",
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

        for route in ["/geocode", "/places", "/reverse"] {
            for method in ["HEAD", "PUT", "DELETE", "PATCH", "OPTIONS"] {
                let reply = send(router.clone(), request(method, route)).await;
                assert_eq!(
                    reply.status,
                    StatusCode::METHOD_NOT_ALLOWED,
                    "{method} {route}"
                );
                assert_eq!(
                    header_value(&reply, "allow"),
                    Some(batch::METHODS),
                    "{method} {route}"
                );
            }
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
                            prepare: |_: &AppState, params: PanicParams| Ok(params.q),
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
        let parse = |value: Option<&str>| {
            let param = serde_json::from_value::<Param>(serde_json::json!(value)).expect("param");
            parse_countries(&param).map_err(|_| ())
        };

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

    /// A lookup past the lookup timeout stops at its next check and answers
    /// 503 `timeout`, with no `Retry-After`: the same lookup takes as long
    /// the next time.
    #[tokio::test]
    async fn a_lookup_past_its_timeout_stops_with_a_503() {
        let pack = temp_path("lookup-timeout");
        write_pack(&pack);
        let limits = limits_with(|limits| limits.lookup_timeout_ms = NonZeroU64::MIN);
        let router = Router::new()
            .route(
                "/slow",
                axum::routing::get(|State(state): State<AppState>| async move {
                    lookup(&state, || -> Result<&'static str> {
                        std::thread::sleep(Duration::from_millis(20));
                        stop::check()?;
                        Ok("answered")
                    })
                    .await
                }),
            )
            .with_state(state_with(&pack, true, limits));

        let reply = send(router, request("GET", "/slow")).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.content_type(), "application/problem+json");
        assert_eq!(reply.json()["error_code"], "timeout");
        assert_eq!(
            reply.json()["detail"],
            "the lookup took longer than the server allows, 1 ms"
        );
        assert!(header_value(&reply, "retry-after").is_none());

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// With every lookup slot taken a single lookup is refused at once with
    /// 503 `busy` and a `Retry-After`, and answers again once a slot frees.
    #[tokio::test]
    async fn a_lookup_is_refused_while_every_lookup_slot_is_taken() {
        let pack = temp_path("lookup-busy");
        write_pack(&pack);
        let limits = limits_with(|limits| limits.max_concurrent_lookups = Some(NonZeroUsize::MIN));
        let state = state_with(&pack, true, limits);
        let taken = Arc::clone(&state.lookup_slots)
            .try_acquire_owned()
            .expect("take the slot");
        let router = build_router(state, None, None);

        for route in [
            "/geocode?q=10%20King%20Street",
            "/autocomplete?q=10%20King",
            "/geocode?address=10%20King%20Street",
            "/reverse?lon=-79&lat=43",
        ] {
            let reply = send(router.clone(), request("GET", route)).await;
            assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE, "{route}");
            assert_eq!(reply.json()["error_code"], "busy", "{route}");
            assert_eq!(header_value(&reply, "retry-after"), Some("1"), "{route}");
        }

        drop(taken);
        let reply = send(
            router,
            request("GET", "/geocode?address=10%20King%20Street"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);

        let _ = std::fs::remove_dir_all(&pack);
    }

    fn slow_when_asked(state: &AppState, query: String) -> Result<ForwardCollection> {
        if query == "slow" {
            std::thread::sleep(Duration::from_millis(20));
            stop::check()?;
        }
        api::forward_collection(&state.pack, query, Vec::new())
    }

    /// An item past the lookup timeout fails alone with 503 `timeout`, and
    /// the items around it are answered.
    #[tokio::test]
    async fn a_batch_item_past_its_timeout_fails_alone() {
        let pack = temp_path("batch-timeout");
        write_pack(&pack);
        let limits =
            limits_with(|limits| limits.lookup_timeout_ms = NonZeroU64::new(5).expect("5"));
        let router = Router::new()
            .route(
                "/slow",
                axum::routing::post(
                    |State(state): State<AppState>, request: Request<Body>| async move {
                        let lookup = Lookup {
                            route: "/slow",
                            prepare: |_: &AppState, params: PanicParams| Ok(params.q),
                            place: |_: &String| (),
                            answer: slow_when_asked,
                        };
                        batch::run(state, lookup, request).await
                    },
                ),
            )
            .with_state(state_with(&pack, true, limits));

        let body = r#"[{"q": "fast"}, {"q": "slow"}, {"q": "fast"}]"#;
        let reply = send(router, batch_request("POST", "/slow", body)).await;
        assert_eq!(reply.status, StatusCode::OK);
        let body = reply.json();
        let statuses = body["results"]
            .as_array()
            .expect("results")
            .iter()
            .map(|result| result["status"].as_u64().expect("status"))
            .collect::<Vec<_>>();
        assert_eq!(statuses, [200, 503, 200]);
        assert_eq!(body["results"][1]["error"]["error_code"], "timeout");
        assert_eq!(body["summary"]["failed"], 1);

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// A job reads at most the server's row cap, then fails naming it, with
    /// the rows it read answered.
    #[tokio::test]
    async fn a_job_stops_reading_at_the_row_cap() {
        let pack = temp_path("jobs-row-cap");
        write_pack(&pack);
        let spool = pack.with_extension("jobs");
        let _ = std::fs::remove_dir_all(&spool);
        let limits = limits_with(|limits| limits.max_job_rows = NonZeroUsize::new(2).expect("2"));
        let router = build_router(state_with(&pack, true, limits), None, None);

        let rows = "street\n10 King Street\n12 King Street\n14 King Street\n";
        let reply = send(router.clone(), upload("/jobs", "text/csv", rows)).await;
        assert_eq!(reply.status, StatusCode::CREATED);
        assert_eq!(csv_rows(&reply.body).len(), 2);
        let location = header_value(&reply, "location")
            .expect("location")
            .to_string();
        let status = send(router, request("GET", &location)).await.json();
        assert_eq!(status["status"], "failed");
        assert_eq!(status["summary"]["total"], 2);
        assert_eq!(
            status["error"],
            "a job holds at most 2 rows; the rest were not read"
        );

        let _ = std::fs::remove_dir_all(&pack);
        let _ = std::fs::remove_dir_all(&spool);
    }

    /// An upload declared over the server's upload limit is refused before
    /// a byte of it is read.
    #[tokio::test]
    async fn a_job_upload_over_the_limit_is_refused() {
        let pack = temp_path("jobs-upload-cap");
        write_pack(&pack);
        let spool = pack.with_extension("jobs");
        let limits = limits_with(|limits| limits.max_job_upload_mib = Some(NonZeroU64::MIN));
        let router = build_router(state_with(&pack, true, limits), None, None);

        let mut declared = upload("/jobs", "text/csv", "street\n10 King Street\n");
        declared.headers_mut().insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from((1u64 << 20) + 1),
        );
        let reply = send(router, declared).await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(reply.json()["error_code"], "body_too_large");

        let _ = std::fs::remove_dir_all(&pack);
        let _ = std::fs::remove_dir_all(&spool);
    }

    /// While the results kept fill the job store, a new job is refused with
    /// 503 `busy`, and the kept ones are still served.
    #[tokio::test]
    async fn a_full_job_store_refuses_new_jobs() {
        let pack = temp_path("jobs-store-full");
        write_pack(&pack);
        let spool = pack.with_extension("jobs");
        let _ = std::fs::remove_dir_all(&spool);
        std::fs::create_dir_all(&spool).expect("spool");
        // Sparse: the store counts the length, and the disk holds none of it.
        std::fs::File::create(spool.join("kept.results"))
            .expect("results")
            .set_len(1 << 30)
            .expect("length");
        std::fs::write(
            spool.join("kept.json"),
            serde_json::json!({
                "id": "kept",
                "endpoint": "geocode",
                "status": "completed",
                "input_format": "csv",
                "output_format": "csv",
                "summary": {"total": 0, "succeeded": 0, "failed": 0},
                "created_at": 0,
                "results_url": "/jobs/kept/results",
            })
            .to_string(),
        )
        .expect("write a kept job");
        let limits = limits_with(|limits| limits.max_job_store_gib = NonZeroU64::MIN);
        let router = build_router(state_with(&pack, true, limits), None, None);

        let reply = send(
            router.clone(),
            upload("/jobs", "text/csv", "street\n10 King Street\n"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.json()["error_code"], "busy");
        assert_eq!(header_value(&reply, "retry-after"), Some("60"));
        let kept = send(router, request("GET", "/jobs/kept")).await;
        assert_eq!(kept.status, StatusCode::OK);

        let _ = std::fs::remove_dir_all(&pack);
        let _ = std::fs::remove_dir_all(&spool);
    }

    /// `/openapi.json` states the batch item cap this server was started
    /// with.
    #[tokio::test]
    async fn the_served_spec_states_the_servers_limits() {
        let pack = temp_path("spec-limits");
        write_pack(&pack);
        let limits =
            limits_with(|limits| limits.max_batch_items = NonZeroUsize::new(7).expect("7"));
        let router = build_router(state_with(&pack, true, limits), None, None);

        let spec = send(router, request("GET", "/openapi.json")).await.json();
        for path in ["/geocode", "/reverse"] {
            assert_eq!(
                spec["paths"][path]["post"]["requestBody"]["content"]["application/json"]["schema"]
                    ["maxItems"],
                7,
                "{path}"
            );
        }

        let _ = std::fs::remove_dir_all(&pack);
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
            .with_state(state)
    }

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
            app,
            state,
            async move {
                let _ = stopped.await;
            },
            Duration::from_secs(10),
        ));

        let client = task::spawn_blocking(move || raw_get(addr, "/slow"));
        started.notified().await;
        stop.send(()).expect("stop");
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

    #[tokio::test]
    async fn shutdown_gives_up_on_a_stuck_request_after_its_timeout() {
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
            state,
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

        let _ = std::fs::remove_dir_all(&pack);
    }

    #[tokio::test]
    async fn shutdown_cancels_a_running_job_and_stops_at_once() {
        let pack = temp_path("shutdown-job");
        write_pack(&pack);
        let spool = pack.with_extension("jobs");
        let _ = std::fs::remove_dir_all(&spool);
        let state = state_for(&pack, true);
        let router = build_router(state.clone(), None, None);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("address");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(run(
            listener,
            router.clone(),
            state.clone(),
            async move {
                let _ = stopped.await;
            },
            Duration::from_secs(30),
        ));

        // Past the 64 KiB the reader sniffs before it answers, then held open.
        let (upload, location) = task::spawn_blocking(move || {
            use std::io::{Read, Write};
            let rows = format!("street\n{}", "10 King Street\n".repeat(6000));
            let mut upload = std::net::TcpStream::connect(addr).expect("connect");
            write!(
                upload,
                "POST /jobs HTTP/1.1\r\nHost: test\r\nContent-Type: text/csv\r\n\
                 Transfer-Encoding: chunked\r\n\r\n{:x}\r\n{rows}\r\n",
                rows.len()
            )
            .expect("send");
            let mut head = [0; 512];
            let read = upload.read(&mut head).expect("response head");
            let head = String::from_utf8_lossy(&head[..read]).into_owned();
            assert!(head.starts_with("HTTP/1.1 201"), "{head}");
            let location = head
                .lines()
                .find_map(|line| line.strip_prefix("location: "))
                .expect("location")
                .to_string();
            // The response streams the results. A client that stops reading
            // holds the stop until its timeout: small socket buffers fill on
            // macOS and Windows.
            let mut results = upload.try_clone().expect("results");
            std::thread::spawn(move || std::io::copy(&mut results, &mut std::io::sink()));
            (upload, location)
        })
        .await
        .expect("upload");
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

        let began = std::time::Instant::now();
        stop.send(()).expect("stop");
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("shutdown is bounded")
            .expect("server")
            .expect("clean stop");
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "waited {:?} for a cancelled job",
            began.elapsed()
        );

        let stopped = status().await;
        assert_eq!(stopped["status"], "cancelled");
        assert!(stopped["completed_at"].is_u64());
        assert!(stopped["summary"]["total"].as_u64() >= Some(answered));
        let record = std::fs::read_to_string(
            spool.join(format!("{}.json", stopped["id"].as_str().expect("id"))),
        )
        .expect("record on disk");
        assert!(record.contains("\"cancelled\""), "{record}");

        drop(upload);
        let _ = std::fs::remove_dir_all(&pack);
        let _ = std::fs::remove_dir_all(&spool);
    }

    /// How a text parameter is sent in one cell of the matrix below.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum Sent {
        Missing,
        Empty,
        Filled,
    }

    type Sents = BTreeMap<&'static str, Sent>;

    /// What one combination of sent parameters must give.
    #[derive(Debug)]
    enum Expect {
        Answer,
        Problem {
            error_code: &'static str,
            detail: &'static str,
        },
    }

    const NOT_BOTH_FIELDS: Expect = Expect::Problem {
        error_code: "malformed_parameter",
        detail: "give the address as q or as address, locality, region and postcode, not both",
    };
    const EMPTY_Q: Expect = Expect::Problem {
        error_code: "invalid_query",
        detail: "q must not be empty",
    };

    /// One endpoint's text parameters and the rules they answer by.
    struct Rules {
        route: &'static str,
        /// Every field of the endpoint's params struct, as serde reads it.
        fields: &'static [&'static str],
        /// The parameters that are not text, and the values they keep.
        fixed: &'static [(&'static str, &'static str)],
        /// A valid value for every text parameter.
        filled: &'static [(&'static str, &'static str)],
        /// The text parameters that choose what the endpoint does, tried in
        /// every combination. Each other one is tried empty and filled alone
        /// beside every such combination: it filters, and filters do not
        /// interact on whether a request is answered.
        modes: &'static [&'static str],
        /// Where it answers, these sent empty answer as if not sent.
        empty_is_missing: &'static [&'static str],
        /// Takes a `QUERY` batch, whose items each answer as their `GET`.
        batchable: bool,
        expect: fn(&Sents) -> Expect,
    }

    /// The names of a params struct's fields, as serde's derive lists them to
    /// a deserializer, so a parameter added to the struct joins the matrix
    /// without being named in it.
    fn field_names<P: serde::de::DeserializeOwned>() -> &'static [&'static str] {
        use serde::de::{Error as _, Visitor, value::Error};

        struct Fields<'a>(&'a std::cell::Cell<&'static [&'static str]>);

        impl<'de> serde::Deserializer<'de> for Fields<'_> {
            type Error = Error;

            fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, Error> {
                Err(Error::custom("params are a struct"))
            }

            fn deserialize_struct<V: Visitor<'de>>(
                self,
                _: &'static str,
                fields: &'static [&'static str],
                _: V,
            ) -> Result<V::Value, Error> {
                self.0.set(fields);
                Err(Error::custom("fields read"))
            }

            serde::forward_to_deserialize_any! {
                bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
                bytes byte_buf option unit unit_struct newtype_struct seq tuple
                tuple_struct map enum identifier ignored_any
            }
        }

        let fields = std::cell::Cell::new(&[][..]);
        let _ = P::deserialize(Fields(&fields));
        assert!(!fields.get().is_empty(), "read no fields");
        fields.get()
    }

    fn rules() -> [Rules; 4] {
        [
            Rules {
                route: "/geocode",
                batchable: true,
                fields: field_names::<GeocodeParams>(),
                fixed: &[("limit", "0")],
                filled: &[
                    ("q", "10 King Street"),
                    ("focus", "-79,43"),
                    ("bbox", "-80,42,-78,44"),
                    ("address", "10 King Street"),
                    ("locality", "Toronto"),
                    ("region", "Ontario"),
                    ("postcode", "M5V 1A1"),
                    ("layer", "address"),
                    ("country", "CA"),
                ],
                modes: &["q", "address", "locality", "region", "postcode"],
                empty_is_missing: &[
                    "focus", "bbox", "locality", "region", "postcode", "layer", "country",
                ],
                expect: |sent| {
                    let fields = ["address", "locality", "region", "postcode"]
                        .iter()
                        .any(|field| sent[field] != Sent::Missing);
                    match (sent["q"], fields) {
                        (Sent::Empty | Sent::Filled, true) => NOT_BOTH_FIELDS,
                        (Sent::Empty, false) => EMPTY_Q,
                        (Sent::Filled, false) => Expect::Answer,
                        (Sent::Missing, _) if sent["address"] == Sent::Filled => Expect::Answer,
                        (Sent::Missing, _) => Expect::Problem {
                            error_code: "invalid_query",
                            detail: "give q, or address with its other fields",
                        },
                    }
                },
            },
            Rules {
                route: "/places",
                batchable: true,
                fields: field_names::<PlacesParams>(),
                fixed: &[("limit", "0")],
                filled: &[
                    ("q", "King Street"),
                    ("ids", "osm:node:1"),
                    ("country", "CA"),
                    ("focus", "-79,43"),
                    ("bbox", "-80,42,-78,44"),
                ],
                modes: &["q", "ids"],
                empty_is_missing: &["country", "focus", "bbox"],
                expect: |sent| match (sent["q"], sent["ids"]) {
                    (Sent::Missing, Sent::Missing) => Expect::Problem {
                        error_code: "invalid_query",
                        detail: "give q, or ids",
                    },
                    (Sent::Empty, Sent::Missing) => EMPTY_Q,
                    (Sent::Filled, Sent::Missing) | (Sent::Missing, Sent::Filled) => Expect::Answer,
                    (Sent::Missing, Sent::Empty) => Expect::Problem {
                        error_code: "malformed_parameter",
                        detail: "ids names no id",
                    },
                    (Sent::Empty | Sent::Filled, Sent::Empty | Sent::Filled) => Expect::Problem {
                        error_code: "malformed_parameter",
                        detail: "give q or ids, not both",
                    },
                },
            },
            Rules {
                route: "/autocomplete",
                batchable: false,
                fields: field_names::<AutocompleteParams>(),
                fixed: &[("limit", "0")],
                filled: &[
                    ("q", "10 King"),
                    ("country", "CA"),
                    ("layer", "address"),
                    ("focus", "-79,43"),
                    ("bbox", "-80,42,-78,44"),
                ],
                modes: &["q"],
                empty_is_missing: &["q", "country", "layer", "focus", "bbox"],
                expect: |_| Expect::Answer,
            },
            Rules {
                route: "/reverse",
                batchable: true,
                fields: field_names::<ReverseParams>(),
                fixed: &[("lon", "-79"), ("lat", "43")],
                filled: &[("layer", "address")],
                modes: &["layer"],
                empty_is_missing: &["layer"],
                expect: |_| Expect::Answer,
            },
        ]
    }

    /// Every text parameter of every endpoint missing, empty and filled,
    /// against the rules: empty, missing and a value are three states, and
    /// each endpoint says what each means.
    #[tokio::test]
    async fn every_text_parameter_missing_empty_or_filled_answers_by_the_rules() {
        let pack = temp_path("param-matrix");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);
        for rules in rules() {
            let mut stated = rules
                .fixed
                .iter()
                .chain(rules.filled)
                .map(|(name, _)| *name)
                .collect::<Vec<_>>();
            stated.sort_unstable();
            let mut fields = rules.fields.to_vec();
            fields.sort_unstable();
            assert_eq!(
                stated, fields,
                "{}: every parameter needs a rule and a value",
                rules.route
            );

            assert!(
                rules
                    .modes
                    .iter()
                    .all(|mode| rules.filled.iter().any(|(name, _)| name == mode)),
                "{}: a mode is a text parameter",
                rules.route
            );
            let filters = rules
                .filled
                .iter()
                .map(|(name, _)| *name)
                .filter(|name| !rules.modes.contains(name))
                .collect::<Vec<_>>();
            let mut cells = Vec::new();
            for index in 0..3usize.pow(u32::try_from(rules.modes.len()).expect("few")) {
                let mut digits = index;
                let modes = rules
                    .modes
                    .iter()
                    .map(|name| {
                        let state = [Sent::Missing, Sent::Empty, Sent::Filled][digits % 3];
                        digits /= 3;
                        (*name, state)
                    })
                    .collect::<Sents>();
                let alone = filters
                    .iter()
                    .flat_map(|filter| [(*filter, Sent::Empty), (*filter, Sent::Filled)]);
                for (filter, state) in std::iter::once(("", Sent::Missing)).chain(alone) {
                    let mut sent = modes.clone();
                    for name in &filters {
                        sent.insert(
                            name,
                            if *name == filter {
                                state
                            } else {
                                Sent::Missing
                            },
                        );
                    }
                    cells.push(sent);
                }
            }

            let mut answers = BTreeMap::<Sents, serde_json::Value>::new();
            for sent in cells {
                let mut pairs = rules.fixed.to_vec();
                for (name, value) in rules.filled {
                    match sent[name] {
                        Sent::Missing => {}
                        Sent::Empty => pairs.push((name, "")),
                        Sent::Filled => pairs.push((name, value)),
                    }
                }
                let uri = format!(
                    "{}?{}",
                    rules.route,
                    serde_urlencoded::to_string(&pairs).expect("query")
                );
                let reply = send(router.clone(), request("GET", &uri)).await;
                let body = reply.json();
                if rules.batchable {
                    let item = pairs
                        .iter()
                        .map(|(name, value)| ((*name).to_string(), serde_json::json!(value)))
                        .collect::<serde_json::Map<_, _>>();
                    let batch = serde_json::json!([item]).to_string();
                    let batched = send(router.clone(), batch_request("QUERY", rules.route, batch))
                        .await
                        .json();
                    let result = &batched["results"][0];
                    assert_eq!(result["status"], reply.status.as_u16(), "QUERY {uri}");
                    let mut answer = if reply.status == StatusCode::OK {
                        result["response"].clone()
                    } else {
                        result["error"].clone()
                    };
                    let mut get = body.clone();
                    // Each request has its own id.
                    for value in [&mut answer, &mut get] {
                        if let Some(object) = value.as_object_mut() {
                            object.remove("request_id");
                        }
                    }
                    assert_eq!(answer, get, "QUERY {uri} answers as its GET");
                }
                match (rules.expect)(&sent) {
                    Expect::Answer => {
                        assert_eq!(reply.status, StatusCode::OK, "{uri}: {body}");
                        // Cells run with fewer parameters sent first, so this
                        // one's twin without its empty ones has answered.
                        let mut without_empty = sent.clone();
                        for (name, state) in &mut without_empty {
                            if *state == Sent::Empty && rules.empty_is_missing.contains(name) {
                                *state = Sent::Missing;
                            }
                        }
                        if without_empty != sent {
                            assert_eq!(
                                answers.get(&without_empty),
                                Some(&body),
                                "{uri}: an empty parameter that means none answers as if not sent"
                            );
                        }
                        answers.insert(sent, body);
                    }
                    Expect::Problem { error_code, detail } => {
                        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri}: {body}");
                        assert_eq!(body["error_code"], error_code, "{uri}: {body}");
                        assert_eq!(body["detail"], detail, "{uri}: {body}");
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// `q` sent empty beside an address field is two ways of giving the
    /// address, in a batch item as in `GET`.
    #[tokio::test]
    async fn an_empty_q_beside_address_fields_is_both() {
        let pack = temp_path("param-both");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);
        for uri in [
            "/geocode?q=&address=10%20King%20Street",
            "/geocode?q=%20&address=10%20King%20Street",
            "/geocode?q=cafe&locality=",
        ] {
            let reply = send(router.clone(), request("GET", uri)).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(reply.json()["error_code"], "malformed_parameter", "{uri}");
            assert_eq!(reply.json()["invalid_params"][0]["name"], "q", "{uri}");
        }
        let answered = send(
            router.clone(),
            request("GET", "/geocode?address=10%20King%20Street&locality="),
        )
        .await;
        assert_eq!(answered.status, StatusCode::OK);
        assert_eq!(
            answered.json()["features"][0]["properties"]["id"],
            "osm:node:1"
        );

        let batch = Request::builder()
            .method("POST")
            .uri("/geocode")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"[{"q":"","address":"10 King Street"},{"q":null,"address":"10 King Street"}]"#,
            ))
            .unwrap();
        let reply = send(router, batch).await;
        assert_eq!(reply.status, StatusCode::OK);
        let results = reply.json()["results"].clone();
        assert_eq!(results[0]["status"], 400);
        assert_eq!(
            results[0]["error"]["detail"],
            "give the address as q or as address, locality, region and postcode, not both"
        );
        assert_eq!(results[1]["status"], 200);
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// `ids` holds at most the server's batch item limit, in `GET` and in
    /// each batch item alike.
    #[tokio::test]
    async fn ids_past_the_limit_are_refused_however_many() {
        let pack = temp_path("param-ids");
        write_pack(&pack);
        let limits =
            limits_with(|limits| limits.max_batch_items = NonZeroUsize::new(7).expect("7"));
        let router = build_router(state_with(&pack, true, limits), None, None);
        let at_limit = ["osm:node:1"; 7].join(",");
        let reply = send(
            router.clone(),
            request("GET", &format!("/places?ids={at_limit}")),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        for count in [8, 5_000] {
            let many = ["osm:node:1"; 5_000][..count].join(",");
            let reply = send(
                router.clone(),
                request("GET", &format!("/places?ids={many}")),
            )
            .await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{count} ids");
            assert_eq!(reply.json()["error_code"], "limit_too_large", "{count} ids");
            assert_eq!(
                reply.json()["invalid_params"][0]["reason"],
                "at most 7 ids",
                "{count} ids"
            );
        }
        let one_past = ["osm:node:1"; 8].join(",");
        let items = serde_json::json!([{"ids": at_limit}, {"ids": one_past}]).to_string();
        let reply = send(router.clone(), batch_request("QUERY", "/places", items)).await;
        assert_eq!(reply.status, StatusCode::OK);
        let results = reply.json()["results"].clone();
        assert_eq!(results[0]["status"], 200);
        assert_eq!(results[1]["status"], 400);
        assert_eq!(
            results[1]["error"]["invalid_params"][0]["reason"],
            "at most 7 ids"
        );
        // Past what a URI holds, so through the parser itself: a malformed id
        // past the limit is never read.
        let huge = format!("{}not-an-id", "osm:node:1,".repeat(1_000_000));
        let refused = place_ids(&huge, 7).unwrap_err();
        assert_eq!(refused.error_code().as_str(), "limit_too_large");
        assert_eq!(refused.invalid_params()[0].reason, "at most 7 ids");
        let _ = std::fs::remove_dir_all(&pack);
    }

    fn with_headers(method: &str, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
        let mut request = request(method, uri);
        for (name, value) in headers {
            request.headers_mut().append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                header::HeaderValue::from_str(value).unwrap(),
            );
        }
        request
    }

    #[tokio::test]
    async fn responses_are_compressed_as_the_client_accepts() {
        use std::io::Read;

        let pack = temp_path("compress");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);
        let uri = "/geocode?q=10%20King%20Street";

        let plain = send(router.clone(), request("GET", uri)).await;
        assert!(plain.headers.get(header::CONTENT_ENCODING).is_none());

        let gzip = send(
            router.clone(),
            with_headers("GET", uri, &[("accept-encoding", "gzip")]),
        )
        .await;
        assert_eq!(header_value(&gzip, "content-encoding"), Some("gzip"));
        assert!(
            header_value(&gzip, "vary")
                .unwrap()
                .to_ascii_lowercase()
                .contains("accept-encoding")
        );
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(&gzip.body[..])
            .read_to_end(&mut decoded)
            .expect("gunzip");
        assert_eq!(decoded, plain.body);

        let zstd = send(
            router.clone(),
            with_headers("GET", uri, &[("accept-encoding", "zstd")]),
        )
        .await;
        assert_eq!(header_value(&zstd, "content-encoding"), Some("zstd"));
        assert_eq!(
            zstd::stream::decode_all(&zstd.body[..]).expect("unzstd"),
            plain.body
        );

        // Too small to be worth the frame.
        let tiny = send(
            router,
            with_headers("GET", "/healthz", &[("accept-encoding", "gzip")]),
        )
        .await;
        assert!(tiny.headers.get(header::CONTENT_ENCODING).is_none());

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// Compression must not hold a job's rows back until the upload ends, nor
    /// panic on the end of the results stream.
    #[tokio::test]
    async fn compressed_jobs_still_stream_while_uploading() {
        use futures_util::{StreamExt, stream};
        use std::convert::Infallible;
        use std::io::Read;

        let pack = temp_path("compress-stream");
        write_pack(&pack);
        let router = build_router(state_for(&pack, true), None, None);

        for encoding in ["gzip", "zstd"] {
            let (upload, rows) = tokio::sync::mpsc::channel::<String>(4);
            let body = Body::from_stream(stream::unfold(rows, |mut rows| async {
                rows.recv()
                    .await
                    .map(|chunk| (Ok::<_, Infallible>(chunk), rows))
            }));
            let request = Request::builder()
                .method("POST")
                .uri("/jobs?endpoint=reverse")
                .header(header::CONTENT_TYPE, "application/jsonl")
                .header(header::ACCEPT_ENCODING, encoding)
                .body(body)
                .unwrap();
            // Two windows of rows, so the first is answered while the upload
            // is still open.
            let rows = 2 * 256;
            upload
                .send("{\"lon\": -79.0, \"lat\": 43.0}\n".repeat(rows))
                .await
                .unwrap();
            let response = router
                .clone()
                .oneshot(request)
                .await
                .expect("router response");
            assert_eq!(response.status(), StatusCode::CREATED);
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_ENCODING)
                    .and_then(|value| value.to_str().ok()),
                Some(encoding)
            );

            let mut chunks = response.into_body().into_data_stream();
            let mut compressed = Vec::new();
            let first = tokio::time::timeout(Duration::from_secs(5), chunks.next()).await;
            let Ok(Some(Ok(first))) = first else {
                panic!("{encoding}: no bytes arrived before the upload ended");
            };
            compressed.extend_from_slice(&first);

            drop(upload);
            while let Some(chunk) = chunks.next().await {
                compressed.extend_from_slice(&chunk.expect("chunk"));
            }
            let mut decoded = Vec::new();
            match encoding {
                "gzip" => flate2::read::MultiGzDecoder::new(&compressed[..])
                    .read_to_end(&mut decoded)
                    .map(drop),
                _ => zstd::stream::copy_decode(&compressed[..], &mut decoded),
            }
            .expect("decode");
            assert_eq!(decoded.split(|byte| *byte == b'\n').count() - 1, rows);
        }

        let _ = std::fs::remove_dir_all(&pack);
    }

    /// Canary for tower-rs/tower-http#732. Passes while tower-http compression
    /// polls a body past its end, which is why `tail()` in `jobs.rs` fuses its
    /// stream. Fails once a tower-http bump carries the fix: then drop that
    /// `.fuse()` and this test.
    #[tokio::test]
    async fn tower_http_compression_still_polls_bodies_past_their_end() {
        let router = Router::new()
            .route(
                "/",
                axum::routing::get(|| async {
                    Body::from_stream(futures_util::stream::unfold(0, |n| async move {
                        (n < 2).then(|| (Ok::<_, std::io::Error>(vec![b'x'; 100]), n + 1))
                    }))
                }),
            )
            .layer(CompressionLayer::new());
        let response = router
            .oneshot(with_headers("GET", "/", &[("accept-encoding", "gzip")]))
            .await
            .expect("router response");
        let read = tokio::spawn(to_bytes(response.into_body(), usize::MAX)).await;
        assert!(
            read.is_err_and(|error| error.is_panic()),
            "tower-http#732 is fixed: drop .fuse() in tail() in jobs.rs, and this test"
        );
    }
}
