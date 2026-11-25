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
    categories, compat,
    compat::house,
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
        AddressGeocodeOptions, BIAS_RADIUS_M, BoundingBox, DEFAULT_SEARCH_LIMIT,
        TextAutocompleteOptions, TextSearchHit, TextSearchOptions, TextSearcher,
        autocomplete_tokens, category_typo, effective_autocomplete_limit, name_starts_as, suggest,
        typed_highlights,
    },
    stop::{self, CancelOnDrop, Stop},
    util::geo::haversine_m,
    util::{fold::search_key, hilbert::hilbert_key},
};

const LOOKUP_RETRY_AFTER_SECONDS: u32 = 1;

pub const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub pack: PathBuf,
    pub demo: Option<PathBuf>,
    pub bind: SocketAddr,
    pub basemap: Option<PathBuf>,
    /// Where job results are kept: one directory per port under the
    /// system's temporary directory when not given.
    pub job_dir: Option<PathBuf>,
    pub limits: Limits,
    /// Hosts a job may call back when it ends, as `name` or `name:port`.
    pub hook_hosts: Vec<String>,
    /// The `scheme://host[:port][/prefix]` clients reach this server by,
    /// which every absolute URL it hands back starts with. Without it a URL
    /// takes the `Host` the client connected with, over plain HTTP, and
    /// never a forwarded header a client could set.
    pub public_url: Option<String>,
    /// Longest the server takes to stop once Ctrl-C or SIGTERM arrives;
    /// what is still open then is dropped. Zero drops it at once.
    pub shutdown_timeout: Duration,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pack: Arc<Reader>,
    searcher: Arc<TextSearcher>,
    places: Arc<TextSearcher>,
    catalogue: Option<Arc<Catalogue>>,
    reverse_geocoder: Arc<PackReverseGeocoder>,
    ready: Arc<AtomicBool>,
    stopping: CancellationToken,
    error_log: ErrorLog,
    batch_pool: Arc<ThreadPool>,
    batch_slots: Arc<Semaphore>,
    here_upload_slot: Arc<Semaphore>,
    jobs: Arc<JobStore>,
    limits: Limits,
    lookup_slots: Arc<Semaphore>,
    spec: Arc<str>,
    hook_hosts: Arc<[String]>,
    public_url: Option<Arc<str>>,
}

impl AppState {
    pub(crate) fn open(path: &Path, jobs: &Path, limits: Limits) -> Result<Self> {
        let pack = Arc::new(Reader::open(path)?);
        let resources = Resources::probe();
        pack.container()
            .advise_random_reads(resources.available_memory_bytes);
        crate::categories::load();
        crate::parse::lattice::postcode::Table::global();
        let searcher = TextSearcher::from_pack(Arc::clone(&pack))?;
        let reverse_geocoder = PackReverseGeocoder::from_pack(Arc::clone(&pack))?;
        let catalogue = Catalogue::open(pack.container())?;
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
            batch_slots: Arc::new(Semaphore::new(resources.usable_cpus)),
            here_upload_slot: Arc::new(Semaphore::new(1)),
            jobs: Arc::new(JobStore::open(jobs.to_path_buf(), limits.job_retention())?),
            lookup_slots: Arc::new(Semaphore::new(
                limits
                    .max_concurrent_lookups(resources.usable_cpus)
                    .min(Semaphore::MAX_PERMITS),
            )),
            spec: openapi::document(&limits)?.into(),
            limits,
            hook_hosts: Arc::from([]),
            public_url: None,
        })
    }

    pub(crate) fn with_hook_hosts(mut self, hosts: Vec<String>) -> Self {
        self.hook_hosts = hosts
            .into_iter()
            .map(|host| host.trim().to_ascii_lowercase())
            .filter(|host| !host.is_empty())
            .collect();
        self
    }

    pub(crate) fn with_public_url(mut self, url: Option<String>) -> Self {
        self.public_url = url.map(|url| url.trim_end_matches('/').into());
        self
    }

    pub(crate) fn origin(&self, headers: &axum::http::HeaderMap) -> String {
        match &self.public_url {
            Some(url) => url.to_string(),
            None => {
                let host = headers
                    .get(axum::http::header::HOST)
                    .and_then(|host| host.to_str().ok())
                    .unwrap_or("localhost");
                format!("http://{host}")
            }
        }
    }

    pub(crate) fn may_call(&self, host: &str, port: Option<u16>) -> bool {
        let host = host.to_ascii_lowercase();
        let with_port = port.map(|port| format!("{host}:{port}"));
        self.hook_hosts
            .iter()
            .any(|allowed| *allowed == host || Some(allowed) == with_port.as_ref())
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

    pub(crate) fn batch_pool(&self) -> &ThreadPool {
        &self.batch_pool
    }

    pub(crate) fn batch_slots(&self) -> &Arc<Semaphore> {
        &self.batch_slots
    }

    pub(crate) fn here_upload_slot(&self) -> &Arc<Semaphore> {
        &self.here_upload_slot
    }

    pub(crate) fn error_log(&self) -> &ErrorLog {
        &self.error_log
    }

    pub(crate) fn searcher(&self) -> &TextSearcher {
        &self.searcher
    }

    pub(crate) fn pack(&self) -> &Reader {
        &self.pack
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct PlacesParams {
    q: Param,
    ids: Param,
    #[serde(default)]
    limit: usize,
    country: Param,
    focus: Param,
    bbox: Param,
    lang: Param,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AutocompleteParams {
    q: Param,
    #[serde(default)]
    limit: usize,
    country: Param,
    layer: Param,
    focus: Param,
    bbox: Param,
    lang: Param,
}

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

#[derive(Debug, Deserialize)]
pub(crate) struct GeocodeParams {
    q: Param,
    focus: Param,
    bbox: Param,
    address: Param,
    locality: Param,
    region: Param,
    postcode: Param,
    #[serde(default)]
    limit: usize,
    layer: Param,
    country: Param,
    lang: Param,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReverseParams {
    lon: f64,
    lat: f64,
    layer: Param,
    #[serde(default)]
    limit: usize,
    radius: Option<f64>,
    lang: Param,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ReverseRequest {
    options: GeocodeOptions,
    limit: usize,
}

fn parse_lang(lang: &Param) -> Result<api::Languages, Problem> {
    let value = match lang {
        Param::Missing | Param::Empty => return Ok(api::Languages::default()),
        Param::Value(value) => value,
    };
    api::Languages::parse(value).map_err(|error| match error {
        api::LanguagesError::NotACode(code) => Problem::malformed_parameter(format!(
            "lang {code:?} is not a language code, such as de or zh-Hant"
        ))
        .with_invalid_params(vec![InvalidParam::new("lang", "not a language code")]),
        api::LanguagesError::TooMany(count) => Problem::malformed_parameter(format!(
            "lang asks for {count} languages, and the most is {}",
            api::MAX_LANGUAGES
        ))
        .with_invalid_params(vec![InvalidParam::new(
            "lang",
            format!("at most {} languages", api::MAX_LANGUAGES),
        )]),
    })
}

#[derive(Debug, Clone)]
pub(crate) struct Named<R> {
    pub(crate) request: R,
    languages: api::Languages,
}

pub async fn serve(options: ServeOptions) -> Result<()> {
    let listener = TcpListener::bind(options.bind)
        .await
        .map_err(|source| Error::IoPath {
            action: "failed to bind",
            path: options.bind.to_string().into(),
            source,
        })?;
    serve_on(listener, options).await
}

pub async fn serve_on(listener: TcpListener, options: ServeOptions) -> Result<()> {
    let address = listener.local_addr()?;
    let jobs_dir = options
        .job_dir
        .clone()
        .unwrap_or_else(|| jobs::default_dir(address.port()));
    let state = AppState::open(&options.pack, &jobs_dir, options.limits)?
        .with_hook_hosts(options.hook_hosts.clone())
        .with_public_url(options.public_url.clone());
    tokio::spawn(jobs::sweep_forever(state.clone()));
    let app = build_router(
        state.clone(),
        options.demo.as_deref(),
        options.basemap.as_deref(),
    );
    println!("Serving {} at http://{address}", options.pack.display());
    run(
        listener,
        app,
        state,
        shutdown_signal(),
        options.shutdown_timeout,
    )
    .await
}

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
    state.jobs().cancel_running();
    stopping.cancel();
    let stopped = tokio::time::timeout(budget, async {
        let served = (&mut server).await;
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

pub(crate) fn build_router(state: AppState, demo: Option<&Path>, basemap: Option<&Path>) -> Router {
    // axum routes HEAD to a GET handler unless HEAD is registered, so every route registers a HEAD that answers 405.
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
        .route("/version", get_only(version::version))
        .merge(compat::router());

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
        .layer(DefaultBodyLimit::max(max_body))
        .layer(middleware::map_response(batch::advertise))
}

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

async fn blocking<T, F>(work: F) -> Result<T, Problem>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    task::spawn_blocking(work).await?.map_err(Problem::from)
}

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
        ("lang" = Option<String>, Query,
         description = "Languages to name results and their areas in, as \
                        comma-separated language codes. The first names each \
                        result and area, falling back to its local name; each \
                        other one goes into `translations` where a name in it \
                        exists. Repeats count once. A malformed code, or more \
                        than 20 distinct codes, returns `400`.",
         example = "fr,de"),
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
    let request = (PLACES.prepare)(&state, params)?;
    let worker = state.clone();
    let collection = lookup(&state, move || (PLACES.answer)(&worker, request)).await?;
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

pub(crate) const PLACES: Lookup<PlacesParams, Named<PlacesRequest>, ForwardCollection, String> =
    Lookup {
        route: "/places",
        prepare: |state, params| {
            Ok(Named {
                languages: parse_lang(&params.lang)?,
                request: places_request(state, params)?,
            })
        },
        place: |named| places_place(&named.request),
        answer: |state, named| {
            let mut collection = places_answer(state, named.request)?;
            api::localize_forward(&state.pack, &mut collection, &named.languages);
            Ok(collection)
        },
    };

#[derive(Clone)]
pub(crate) enum PlacesRequest {
    Text(TextSearchOptions),
    Ids {
        catalogue: Arc<Catalogue>,
        ids: Vec<(ObjectType, i64)>,
    },
}

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

fn places_place(request: &PlacesRequest) -> String {
    match request {
        PlacesRequest::Text(options) => options.query.to_lowercase(),
        PlacesRequest::Ids { .. } => String::new(),
    }
}

fn places_answer(state: &AppState, request: PlacesRequest) -> Result<ForwardCollection> {
    let (catalogue, ids) = match request {
        PlacesRequest::Text(options) => {
            let mut named = Vec::new();
            let key = search_key(&options.query).unwrap_or_default();
            let subject = key
                .split_once(" in ")
                .map_or(key.as_str(), |(subject, _)| subject);
            let food = categories::presets_for_word("food");
            let words = subject.split_whitespace().collect::<Vec<_>>();
            for hit in state.places.search(&options)? {
                let asks_for_food = hit.record.category.as_deref().is_some_and(|category| {
                    categories::holds(&food, category)
                        && (hit.record.cuisine.iter().any(|cuisine| {
                            search_key(cuisine).is_some_and(|key| words.contains(&key.as_str()))
                        }) || (words.contains(&"food")
                            && search_key(&hit.record.name).is_some_and(|name| {
                                name.split_whitespace().any(|word| {
                                    word.len() >= 4 && word != "food" && words.contains(&word)
                                })
                            })))
                });
                if hit.record.layer != Layer::Poi.as_str()
                    || asks_for_food
                    || state.places.matches_poi_name(&hit, &key)?
                    || hit
                        .record
                        .address
                        .street
                        .as_deref()
                        .is_some_and(|street| crate::search::road::same_road(street, &key))
                {
                    named.push(hit);
                }
            }
            let as_typed = TextAutocompleteOptions {
                query: options.query.clone(),
                limit: options.limit,
                layer: None,
                countries: options.countries.clone(),
                focus: options.focus,
                bbox: options.bbox,
            };
            let limit = crate::search::effective_limit(options.limit);
            let typed = search_key(&options.query);
            let named_as_typed = named
                .first()
                .is_some_and(|hit| search_key(&hit.record.name) == typed);
            let mut hits = match named_as_typed {
                true => named,
                false => nearest_of_category(state, &as_typed, limit, named)?,
            };
            for hit in &mut hits {
                hit.highlights = Default::default();
            }
            let focus = options.focus;
            let mut collection = api::forward_collection(&state.pack, options.query, hits)?;
            api::measure_from(&mut collection, focus);
            return Ok(collection);
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

fn place_ids(ids: &str, max: usize) -> Result<Vec<(ObjectType, i64)>, Problem> {
    let ids = ids
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
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
        ("lang" = Option<String>, Query,
         description = "Languages to name results and their areas in, as \
                        comma-separated language codes. The first names each \
                        result and area, falling back to its local name; each \
                        other one goes into `translations` where a name in it \
                        exists. Repeats count once. A malformed code, or more \
                        than 20 distinct codes, returns `400`.",
         example = "fr,de"),
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
    let request = (AUTOCOMPLETE.prepare)(&state, params)?;
    let worker = state.clone();
    let collection = lookup(&state, move || (AUTOCOMPLETE.answer)(&worker, request)).await?;
    Ok(Json(collection))
}

pub(crate) type AutocompleteRequest = Named<(TextAutocompleteOptions, Option<Layer>)>;

pub(crate) const AUTOCOMPLETE: batch::Lookup<
    AutocompleteParams,
    AutocompleteRequest,
    ForwardCollection,
    String,
> = batch::Lookup {
    route: "/autocomplete",
    prepare: |_, params| autocomplete_request(&params),
    place: |request| request.request.0.query.to_lowercase(),
    answer: autocomplete_answer,
};

fn autocomplete_request(params: &AutocompleteParams) -> Result<AutocompleteRequest, Problem> {
    let query = match &params.q {
        Param::Missing | Param::Empty => String::new(),
        Param::Value(q) => bounds::validate_query_length(q)?,
    };
    bounds::validate_autocomplete_limit(params.limit)?;
    let languages = parse_lang(&params.lang)?;
    let layer = parse_layer(&params.layer)?;
    let countries = parse_countries(&params.country)?;
    let focus = bounds::parse_focus(&params.focus)?;
    let bbox = bounds::parse_bbox(&params.bbox)?;

    Ok(Named {
        request: (
            TextAutocompleteOptions {
                query,
                limit: params.limit,
                layer: layer.map(|layer| layer.as_str().to_string()),
                countries,
                focus,
                bbox,
            },
            layer,
        ),
        languages,
    })
}

fn autocomplete_answer(state: &AppState, named: AutocompleteRequest) -> Result<ForwardCollection> {
    let (mut options, layer) = named.request;
    let focus = options.focus;
    let languages = named.languages;
    let asked = effective_autocomplete_limit(options.limit);
    let mut collection = loop {
        let limit = effective_autocomplete_limit(options.limit);
        let hits = match layer {
            Some(Layer::Poi) => {
                let options = TextAutocompleteOptions {
                    layer: None,
                    ..options.clone()
                };
                let hits = state.places.autocomplete(&options)?;
                prefer_poi_names(state, &options, limit, hits)?.0
            }
            Some(_) => state.searcher.autocomplete(&options)?,
            None => {
                let named = suggest(&state.searcher, &state.places, &options)?;
                nearest_of_category(state, &options, limit, named)?
            }
        };
        let mut found = api::forward_collection(&state.pack, options.query.clone(), hits)?;
        let held = found.features.len();
        house::one_per_house(&mut found.features);
        let Some(more) = house::ask_again(
            asked,
            limit,
            held,
            found.features.len(),
            crate::search::MAX_AUTOCOMPLETE_LIMIT,
        ) else {
            found.features.truncate(asked);
            break found;
        };
        options.limit = more;
    };
    api::measure_from(&mut collection, focus);
    api::localize_forward(&state.pack, &mut collection, &languages);
    Ok(collection)
}

#[derive(Debug, Deserialize)]
pub(crate) struct CategoryParams {
    #[serde(default)]
    limit: usize,
    country: Param,
    focus: Param,
    radius: Option<f64>,
    bbox: Param,
    lang: Param,
}

pub(crate) async fn of_categories(
    State(state): State<AppState>,
    categories: Vec<&'static str>,
    params: Result<Query<CategoryParams>, QueryRejection>,
) -> Result<Json<ForwardCollection>, Problem> {
    let Query(params) = params.map_err(malformed_parameters)?;
    bounds::validate_search_limit(params.limit)?;
    let limit = match params.limit {
        0 => DEFAULT_SEARCH_LIMIT,
        limit => limit,
    };
    let languages = parse_lang(&params.lang)?;
    let countries = parse_countries(&params.country)?;
    let focus = bounds::parse_focus(&params.focus)?;
    let bbox = bounds::parse_bbox(&params.bbox)?;
    if params
        .radius
        .is_some_and(|radius| !radius.is_finite() || radius <= 0.0)
    {
        return Err(
            Problem::malformed_parameter("radius must be a number of metres above 0")
                .with_invalid_params(vec![InvalidParam::new("radius", "not above 0")]),
        );
    }
    let around = focus.map(|point| (point, params.radius.unwrap_or(BIAS_RADIUS_M)));

    let worker = state.clone();
    let collection = lookup(&state, move || {
        let hits = pois_of_categories(&worker, &categories, around, limit, &countries, bbox)?;
        let mut collection = api::forward_collection(&worker.pack, String::new(), hits)?;
        api::measure_from(&mut collection, focus);
        api::localize_forward(&worker.pack, &mut collection, &languages);
        Ok(collection)
    })
    .await?;

    Ok(Json(collection))
}

fn nearest_of_category(
    state: &AppState,
    options: &TextAutocompleteOptions,
    limit: usize,
    named: Vec<TextSearchHit>,
) -> Result<Vec<TextSearchHit>> {
    let (named, exact_poi) = prefer_poi_names(state, options, limit, named)?;
    let typo = |kind: &str| category_typo(kind);
    let (request, through_typo) = match categories::of_request(&options.query) {
        Some(request) => (request, false),
        None => match categories::of_request_with(&options.query, typo) {
            Some(request) => (
                categories::Request {
                    whole: false,
                    ..request
                },
                true,
            ),
            None => return Ok(named),
        },
    };
    if exact_poi && !request.whole {
        return Ok(named);
    }
    let typed = search_key(&options.query);
    let tokens = autocomplete_tokens(&options.query);
    let near = |hit: &TextSearchHit| {
        options.focus.is_some_and(|[lon, lat]| {
            hit.record
                .point
                .is_some_and(|point| haversine_m(lon, lat, point.lon, point.lat) <= BIAS_RADIUS_M)
        })
    };
    let named_near = named
        .first()
        .is_some_and(|hit| near(hit) && search_key(&hit.record.name) == typed)
        || (!request.whole
            && named
                .iter()
                .any(|hit| near(hit) && name_starts_as(&hit.record.name, &tokens, false)));
    if named_near {
        return Ok(named);
    }
    let focus = match &request.place {
        None => options.focus,
        Some(words) => place_named(state, options, words, through_typo)?,
    };
    let (Some(focus), categories) = (focus, request.categories) else {
        return Ok(named);
    };
    let mut hits = pois_of_categories(
        state,
        &categories,
        Some((focus, BIAS_RADIUS_M)),
        limit,
        &options.countries,
        options.bbox,
    )?;
    if hits.is_empty() {
        return Ok(named);
    }
    for hit in &mut hits {
        hit.highlights = typed_highlights(&options.query, &hit.record.label);
    }
    for hit in named {
        if hits.len() >= limit {
            break;
        }
        if !hits.iter().any(|kept| kept.record_id == hit.record_id) {
            hits.push(hit);
        }
    }
    Ok(hits)
}

fn prefer_poi_names(
    state: &AppState,
    options: &TextAutocompleteOptions,
    limit: usize,
    mut hits: Vec<TextSearchHit>,
) -> Result<(Vec<TextSearchHit>, bool)> {
    let Some(key) = search_key(&options.query) else {
        return Ok((hits, false));
    };
    let first_whole = hits
        .first()
        .is_some_and(|hit| search_key(&hit.record.name).as_ref() == Some(&key));
    for (index, hit) in hits.iter().enumerate() {
        if hit.record.layer == Layer::Poi.as_str()
            && state
                .pack
                .records()
                .names(hit.record_id)?
                .iter()
                .any(|name| search_key(name).as_ref() == Some(&key))
        {
            if index != 0 && !first_whole {
                let hit = hits.remove(index);
                hits.insert(0, hit);
            }
            return Ok((hits, true));
        }
    }
    if first_whole {
        return Ok((hits, false));
    }
    let mut qualified = state.places.qualified_pois(options)?;
    if qualified.is_empty() {
        return Ok((hits, false));
    }
    hits.retain(|hit| {
        !qualified
            .iter()
            .any(|match_| match_.record_id == hit.record_id)
    });
    qualified.extend(hits);
    qualified.truncate(limit);
    Ok((qualified, true))
}

fn pois_of_categories(
    state: &AppState,
    categories: &[&str],
    focus: Option<([f64; 2], f64)>,
    limit: usize,
    countries: &[String],
    bbox: Option<BoundingBox>,
) -> Result<Vec<TextSearchHit>> {
    let count = state.places.category_count(categories)?;
    if count == 0 {
        return Ok(Vec::new());
    }
    let walked = match focus {
        Some((point, radius_m)) if countries.is_empty() => state.reverse_geocoder.nearest_pois(
            point,
            radius_m,
            limit,
            count,
            |lon, lat| bbox.is_none_or(|bbox| bbox.contains(lon, lat)),
            |category| categories::holds(categories, category),
        )?,
        _ => None,
    };
    match walked {
        Some(nearest) => nearest
            .into_iter()
            .map(|record_id| {
                Ok(TextSearchHit::of_record(
                    record_id,
                    state.pack.record_summary(record_id)?,
                ))
            })
            .collect(),
        None => state
            .places
            .of_categories(categories, focus, limit, countries, bbox),
    }
}

fn place_named(
    state: &AppState,
    options: &TextAutocompleteOptions,
    words: &str,
    towns_only: bool,
) -> Result<Option<[f64; 2]>> {
    let towns = state.searcher.place_point(words, options.focus, false)?;
    if let Some(point) = towns.near.or(towns.known_abroad) {
        return Ok(Some(point));
    }
    if towns_only {
        return Ok(None);
    }
    let pois = state.places.place_point(words, options.focus, true)?;
    Ok(pois.near.or(towns.known).or(pois.known))
}

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
         description = "Point to prefer results near, as `lon,lat`. \
                        Breaks ties between equally good matches only.",
         example = "-79.38,43.65"),
        ("bbox" = Option<String>, Query,
         description = "Return only results inside this box, as \
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
        ("lang" = Option<String>, Query,
         description = "Languages to name results and their areas in, as \
                        comma-separated language codes. The first names each \
                        result and area, falling back to its local name; each \
                        other one goes into `translations` where a name in it \
                        exists. Repeats count once. A malformed code, or more \
                        than 20 distinct codes, returns `400`.",
         example = "fr,de"),
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
    let request = (GEOCODE.prepare)(&state, params)?;
    let worker = state.clone();
    let collection = lookup(&state, move || (GEOCODE.answer)(&worker, request)).await?;
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

pub(crate) const GEOCODE: Lookup<
    GeocodeParams,
    Named<GeocodeRequest>,
    ForwardCollection,
    [String; 3],
> = Lookup {
    route: "/geocode",
    prepare: |state, params| {
        Ok(Named {
            languages: parse_lang(&params.lang)?,
            request: geocode_request(state, params)?,
        })
    },
    place: |named| geocode_place(&named.request),
    answer: |state, named| {
        let mut collection = geocode_answer(state, named.request)?;
        api::localize_forward(&state.pack, &mut collection, &named.languages);
        Ok(collection)
    },
};

#[derive(Debug, Clone)]
pub(crate) enum GeocodeRequest {
    Text(TextSearchOptions),
    Fields {
        options: AddressGeocodeOptions,
        focus: Option<[f64; 2]>,
        bbox: Option<crate::search::BoundingBox>,
    },
}

fn geocode_request(_: &AppState, params: GeocodeParams) -> Result<GeocodeRequest, Problem> {
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
            return Ok(GeocodeRequest::Fields {
                options: geocode_fields(&params)?,
                focus: bounds::parse_focus(&params.focus)?,
                bbox: bounds::parse_bbox(&params.bbox)?,
            });
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

fn context_field(field: &Param) -> Result<Option<String>, Problem> {
    match field {
        Param::Missing => Ok(None),
        Param::Empty => Ok(None),
        Param::Value(value) => bounds::validate_query_length(value).map(Some),
    }
}

fn geocode_place(request: &GeocodeRequest) -> [String; 3] {
    let field = |value: &Option<String>| value.as_deref().unwrap_or_default().to_lowercase();
    match request {
        GeocodeRequest::Fields { options, .. } => [
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
    let (query, hits, focus) = match request {
        GeocodeRequest::Text(options) => {
            let hits = state.searcher.search(&options)?;
            (options.query, hits, options.focus)
        }
        GeocodeRequest::Fields {
            options,
            focus,
            bbox,
        } => {
            match state
                .searcher
                .geocode_address_scoped(&options, focus, bbox)?
            {
                Some(found) => (found.query, vec![found.hit], focus),
                None => (options.address, Vec::new(), focus),
            }
        }
    };
    let mut collection = api::forward_collection(&state.pack, query, hits)?;
    api::measure_from(&mut collection, focus);
    Ok(collection)
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
        ("limit" = Option<usize>, Query,
         description = "How many results of `layer` to return, nearest first, up to \
                        50. Default and 0 mean 1. Above 1 it needs `layer`. A point \
                        lies in one area of each layer, so an area layer returns at \
                        most one.",
         minimum = 0, maximum = 50),
        ("radius" = Option<f64>, Query,
         description = "How far to look for the nearest address, street or place, in \
                        metres, up to 1000. Default 30. Areas hold the point, so it \
                        does not bound them.",
         example = 150, minimum = 0.0, maximum = 1000.0),
        ("lang" = Option<String>, Query,
         description = "Languages to name results and their areas in, as \
                        comma-separated language codes. The first names each \
                        result and area, falling back to its local name; each \
                        other one goes into `translations` where a name in it \
                        exists. Repeats count once. A malformed code, or more \
                        than 20 distinct codes, returns `400`.",
         example = "fr,de"),
    ),
    responses(
        (status = 200, description = "Results at the point, most precise first. \
                                      `features` is empty when nothing is near.",
         body = ReverseCollection),
        (status = 400, description = "`lon` or `lat` is missing or out of range, \
                                      `layer` is unknown, `limit` is out of range or \
                                      above 1 with no `layer`, or `radius` is out of \
                                      range.",
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
    let request = (REVERSE.prepare)(&state, params)?;
    let worker = state.clone();
    let collection = lookup(&state, move || (REVERSE.answer)(&worker, request)).await?;
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

pub(crate) const REVERSE: Lookup<ReverseParams, Named<ReverseRequest>, ReverseCollection, u64> =
    Lookup {
        route: "/reverse",
        prepare: |state, params| {
            Ok(Named {
                languages: parse_lang(&params.lang)?,
                request: reverse_request(state, params)?,
            })
        },
        place: |named| reverse_place(&named.request.options),
        answer: |state, named| {
            let mut collection = reverse_answer(state, named.request)?;
            api::localize_reverse(&state.pack, &mut collection, &named.languages);
            Ok(collection)
        },
    };

fn reverse_request(_: &AppState, params: ReverseParams) -> Result<ReverseRequest, Problem> {
    let ReverseParams {
        lon,
        lat,
        layer,
        limit,
        radius,
        lang: _,
    } = params;
    bounds::validate_coords(lon, lat)?;
    let radius_m = radius.unwrap_or(crate::reverse::DEFAULT_RADIUS_M);
    if !(radius_m > 0.0 && radius_m <= crate::reverse::MAX_RADIUS_M) {
        let reason = format!(
            "must be a number of metres above 0 and at most {}",
            crate::reverse::MAX_RADIUS_M
        );
        return Err(Problem::malformed_parameter(format!("radius {reason}"))
            .with_invalid_params(vec![InvalidParam::new("radius", &reason)]));
    }
    let layer = parse_layer(&layer)?;
    let limit = limit.max(1);
    bounds::validate_search_limit(limit)?;
    if limit > 1 && layer.is_none() {
        let reason = "above 1 needs a layer, since without one the answer is the point's areas";
        return Err(Problem::malformed_parameter(format!("limit {reason}"))
            .with_invalid_params(vec![InvalidParam::new("limit", reason)]));
    }
    Ok(ReverseRequest {
        options: GeocodeOptions {
            lon,
            lat,
            layer,
            radius_m,
        },
        limit,
    })
}

fn reverse_place(options: &GeocodeOptions) -> u64 {
    hilbert_key(
        (options.lon * 1e7).round() as i32,
        (options.lat * 1e7).round() as i32,
    )
}

fn reverse_answer(state: &AppState, request: ReverseRequest) -> Result<ReverseCollection> {
    let ReverseRequest { options, limit } = request;
    let Some(layer) = options.layer.filter(|_| limit > 1) else {
        let response = state.reverse_geocoder.reverse(options)?;
        return api::reverse_collection(&state.pack, &response);
    };
    let nearest = state
        .reverse_geocoder
        .reverse_nearest(options, layer, limit)?;
    nearest_collection(state, options, &nearest)
}

pub(crate) async fn reverse_addresses_where(
    state: &AppState,
    query: Result<Query<ReverseParams>, QueryRejection>,
    accept: impl Fn(&AppState, crate::pack::RecordId) -> Result<bool> + Send + 'static,
) -> Result<ReverseCollection, Problem> {
    let Query(query) = query.map_err(malformed_parameters)?;
    let request = reverse_request(state, query)?;
    let worker = state.clone();
    lookup(state, move || {
        let nearest = worker.reverse_geocoder.reverse_addresses_where(
            request.options,
            request.limit,
            |id| accept(&worker, id),
        )?;
        nearest_collection(&worker, request.options, &nearest)
    })
    .await
}

fn nearest_collection(
    state: &AppState,
    options: GeocodeOptions,
    nearest: &[crate::reverse::GeocodeResponse],
) -> Result<ReverseCollection> {
    let mut collection = api::reverse_collection(
        &state.pack,
        &crate::reverse::GeocodeResponse {
            lon: options.lon,
            lat: options.lat,
            result: None,
        },
    )?;
    for response in nearest {
        collection
            .features
            .extend(api::reverse_collection(&state.pack, response)?.features);
    }
    collection.geocoding.attribution = api::attribution(
        &state.pack,
        collection
            .features
            .iter()
            .map(|feature| &feature.properties.common),
    );
    Ok(collection)
}

#[cfg(test)]
pub(crate) mod router_tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    use std::num::{NonZeroU64, NonZeroUsize};
    use std::sync::LazyLock;
    use std::time::Duration;

    use super::*;
    use crate::pack::Writer;
    use crate::record::{
        AddressComponents, AddressRecord, LocationPrecision, ObjectType, SourceProvenance,
        point_geometry,
    };
    use crate::test_support::TempDir;

    pub(crate) static KING: LazyLock<TempDir> = LazyLock::new(|| shared("router-king", write_pack));
    static FEATURES: LazyLock<TempDir> =
        LazyLock::new(|| shared("router-features", write_features_pack));

    fn shared(name: &str, write: fn(&Path)) -> TempDir {
        let dir = TempDir::new(name);
        write(&dir);
        dir
    }

    fn open(pack: &Path, jobs: &Path, limits: Limits) -> AppState {
        AppState::open(pack, jobs, limits).expect("open the pack")
    }

    fn king_state(jobs: &Path, limits: Limits) -> AppState {
        open(&KING, jobs, limits)
    }

    fn features_state(jobs: &Path) -> AppState {
        open(&FEATURES, jobs, Limits::DEFAULT)
    }

    #[derive(Default)]
    struct Areas {
        country_codes:
            std::collections::HashMap<crate::pack::RecordId, crate::parse::lattice::CountryCode>,
        subdivision_codes: std::collections::HashMap<crate::pack::RecordId, String>,
        language_names:
            std::collections::HashMap<crate::pack::RecordId, crate::pack::LanguageNames>,
        context_names: std::collections::HashMap<crate::pack::RecordId, Vec<String>>,
    }

    impl Areas {
        fn replace_writer_maps(&self, writer: &mut Writer) {
            writer.set_country_codes(self.country_codes.clone());
            writer.set_subdivision_codes(self.subdivision_codes.clone());
            writer.set_language_names(self.language_names.clone());
            writer.set_context_names(self.context_names.clone());
        }
    }

    fn write_features_pack(dir: &Path) {
        let mut writer = Writer::create(dir).expect("writer");
        let mut areas = Areas::default();
        write_poi_cafes(&mut writer);
        let town = write_shaped(&mut writer);
        let zones = write_zoned(&mut writer);
        write_entered(&mut writer);
        write_stated_names(&mut writer, &mut areas);
        let canada = write_canada(&mut writer, &mut areas);
        write_lined(&mut writer, &canada);
        write_matched(&mut writer, &canada);
        write_named_pois(&mut writer);
        write_reverse_houses(&mut writer);
        write_reverse_streets(&mut writer);
        writer.set_extents(vec![(
            town,
            [-796_000_000, 454_000_000, -794_000_000, 456_000_000],
        )]);
        writer.set_timezones(
            vec![
                "America/Toronto".to_string(),
                "America/Winnipeg".to_string(),
                "America/Detroit".to_string(),
            ],
            zones,
        );
        writer.finish().expect("finish");
    }

    fn write_pack(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
        let mut writer = Writer::create(dir).expect("writer");
        for (object_id, number, street, postcode, lon) in [
            (1, "10", "King Street", Some("M5V 1A1"), -79.0),
            (2, "20", "Queen Street", None, -79.1),
        ] {
            writer
                .write(
                    &AddressRecord {
                        building: None,
                        access: Vec::new(),
                        disputed: false,
                        street_postcodes: Vec::new(),
                        address: AddressComponents {
                            block_number_len: None,
                            areas: Vec::new(),
                            number: number.to_string(),
                            street: Some(street.to_string()),
                            place: None,
                            unit: None,
                            locality: Some("Toronto".to_string()),
                            region: None,
                            postcode: postcode.map(str::to_string),
                            country: None,
                        },
                        geometry: point_geometry(lon, 43.0),
                        location_precision: LocationPrecision::Point,
                        source: SourceProvenance {
                            dataset: "osm".to_string(),
                            object_type: ObjectType::Node,
                            object_id,
                            tags: Some(BTreeMap::new()),
                        },
                    }
                    .into(),
                    None,
                )
                .expect("write address");
        }
        writer.finish().expect("finish");
    }

    pub(crate) fn state_for(pack_dir: &Path, ready: bool) -> AppState {
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

    fn write_named_pois(writer: &mut Writer) {
        use crate::record::{PoiRecord, Prominence};
        for (id, name, alias, category, street, town, lon, importance) in [
            (
                701,
                "Harbor Tower",
                "Harbor Lookout",
                "tourism:attraction",
                "Bridge Street",
                "Fairview",
                -79.0,
                10,
            ),
            (
                702,
                "Garden Tower",
                "",
                "man_made:tower",
                "Park Street",
                "Fairview",
                -79.2,
                250,
            ),
            (
                703,
                "Juniper Coffee",
                "",
                "amenity:cafe",
                "Bridge Street",
                "Fairview",
                -79.0,
                10,
            ),
            (
                704,
                "Juniper Coffee",
                "",
                "amenity:cafe",
                "Park Street",
                "Lakeside",
                -79.2,
                250,
            ),
        ] {
            writer
                .write(
                    &PoiRecord {
                        name: name.to_string(),
                        category: category.to_string(),
                        alt_names: (!alias.is_empty())
                            .then(|| alias.to_string())
                            .into_iter()
                            .collect(),
                        address: Some(AddressComponents {
                            number: "10".to_string(),
                            street: Some(street.to_string()),
                            locality: Some(town.to_string()),
                            block_number_len: None,
                            areas: Vec::new(),
                            place: None,
                            unit: None,
                            region: None,
                            postcode: None,
                            country: None,
                        }),
                        geometry: point_geometry(lon, 55.0),
                        location_precision: LocationPrecision::Point,
                        source: SourceProvenance::osm(ObjectType::Node, id),
                        prominence: Prominence {
                            importance,
                            ..Default::default()
                        },
                        wikidata: None,
                        brand: None,
                        details: None,
                        aliases: Vec::new(),
                    }
                    .into(),
                    None,
                )
                .expect("POI");
        }
    }

    #[tokio::test]
    async fn named_pois_keep_their_rank_in_mixed_search() {
        let dir = TempDir::new("named-poi-order");
        let router = build_router(features_state(&dir), None, None);
        for route in ["autocomplete", "places"] {
            for (query, id) in [
                ("Harbor Tower", "osm:node:701"),
                ("Harbor Lookout", "osm:node:701"),
                ("Juniper Coffee", "osm:node:704"),
                ("Juniper Coffee, Fairview", "osm:node:703"),
                ("Juniper Coffee, Bridge St", "osm:node:703"),
                ("Juniper Coffee Bridge St", "osm:node:703"),
                ("Juniper Coffee, Bridge St, Fairview", "osm:node:703"),
            ] {
                let params = serde_urlencoded::to_string([
                    ("q", query),
                    ("focus", "-79.2,55"),
                    ("limit", "5"),
                ])
                .expect("query");
                let reply = send(
                    router.clone(),
                    request("GET", &format!("/{route}?{params}")),
                )
                .await;
                assert_eq!(reply.status, StatusCode::OK, "{route}: {query}");
                assert_eq!(
                    reply.json()["features"][0]["properties"]["id"],
                    id,
                    "{route}: {query}",
                );
            }
        }
    }

    #[tokio::test]
    async fn health_endpoints_reflect_readiness() {
        let jobs = TempDir::new("health");
        let missing = TempDir::new("health-missing");
        let demo = missing.join("demo");
        let no_basemap = missing.join("basemap.pmtiles");

        let router = build_router(
            king_state(&jobs, Limits::DEFAULT),
            Some(&demo),
            Some(&no_basemap),
        );
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

        let not_ready = king_state(&jobs, Limits::DEFAULT);
        not_ready.ready.store(false, Ordering::Release);
        let not_ready = build_router(not_ready, Some(&demo), Some(&no_basemap));
        assert_eq!(
            send(not_ready, request("GET", "/readyz")).await.status,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn version_names_the_build_and_the_pack() {
        let jobs = TempDir::new("version");
        let reader = Reader::open(&*KING).expect("pack");
        let manifest = reader.manifest();
        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);

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
        for path in [&**KING, reader.path()] {
            let path = path.to_string_lossy();
            assert!(!body.contains(path.as_ref()), "{path} in {body}");
        }

        assert_eq!(
            send(router, request("HEAD", "/version")).await.status,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[tokio::test]
    async fn reverse_looks_as_far_as_its_radius() {
        let jobs = TempDir::new("reverse-radius");
        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);
        let at = "/reverse?lon=-79.0&lat=43.0006";
        let reply = send(router.clone(), request("GET", at)).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.json()["features"],
            serde_json::json!([]),
            "{}",
            reply.json()
        );

        let reply = send(router.clone(), request("GET", &format!("{at}&radius=100"))).await;
        let house = &reply.json()["features"][0]["properties"];
        assert_eq!(house["name"], "10 King Street", "{}", reply.json());
        let distance = house["distance_m"].as_f64().expect("distance");
        assert!((66.0..68.0).contains(&distance), "{distance}");

        for radius in ["0", "-5", "1001", "NaN"] {
            let reply = send(
                router.clone(),
                request("GET", &format!("{at}&radius={radius}")),
            )
            .await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "radius {radius}");
            assert_eq!(reply.json()["invalid_params"][0]["name"], "radius");
        }
    }

    #[tokio::test]
    async fn basemap_is_served_raw_with_range_support() {
        let jobs = TempDir::new("basemap");
        let missing = TempDir::new("basemap-missing");
        let demo = missing.join("demo");
        let files = TempDir::new("basemap-file");
        let basemap = files.join("basemap.pmtiles");
        std::fs::write(&basemap, vec![0u8; 600_000]).expect("write basemap");

        let router = build_router(
            king_state(&jobs, Limits::DEFAULT),
            Some(&demo),
            Some(&basemap),
        );

        let mut req = request("GET", "/basemap.pmtiles");
        req.headers_mut().insert(
            header::RANGE,
            header::HeaderValue::from_static("bytes=0-511999"),
        );
        let reply = send(router.clone(), req).await;
        assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(reply.body.len(), 512_000);

        let reply = send(router.clone(), request("GET", "/basemap.pmtiles")).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.body.len(), 600_000);

        let reply = send(router, request("HEAD", "/basemap.pmtiles")).await;
        assert_eq!(reply.status, StatusCode::OK);
    }

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

        assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(reply.content_type(), "application/problem+json");
        assert_eq!(reply.request_id().as_deref(), Some("ray-500"));
        let body = reply.json();
        assert_eq!(body["error_code"], "internal_error");
        assert_eq!(body["request_id"], "ray-500");
        assert!(body.get("detail").is_none());
        assert!(!body.to_string().contains("section offset"));

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
        let jobs = TempDir::new("user-errors");
        let missing = TempDir::new("user-errors-missing");
        let demo = missing.join("demo");
        let no_basemap = missing.join("basemap.pmtiles");
        let log = ErrorLog::capture();
        let mut state = king_state(&jobs, Limits::DEFAULT);
        state.error_log = log.clone();
        let router = build_router(state, Some(&demo), Some(&no_basemap));

        for (method, uri, status, error_code) in [
            (
                "GET",
                "/geocode?q=",
                StatusCode::BAD_REQUEST,
                "invalid_query",
            ),
            (
                "GET",
                "/geocode?q=king&limit=abc",
                StatusCode::BAD_REQUEST,
                "malformed_parameter",
            ),
            (
                "GET",
                "/geocode?q=king&limit=999999",
                StatusCode::BAD_REQUEST,
                "limit_too_large",
            ),
            (
                "GET",
                "/autocomplete?q=kin&layer=bogus",
                StatusCode::BAD_REQUEST,
                "malformed_parameter",
            ),
            (
                "GET",
                "/reverse?lon=999&lat=0",
                StatusCode::BAD_REQUEST,
                "invalid_coordinate",
            ),
            ("GET", "/does-not-exist", StatusCode::NOT_FOUND, "not_found"),
            (
                "POST",
                "/autocomplete",
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
            ),
            (
                "PUT",
                "/places",
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
            ),
            (
                "POST",
                "/places",
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
            ),
            (
                "POST",
                "/geocode",
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
            ),
            ("GET", "/geocode?q=King%20Street", StatusCode::OK, ""),
        ] {
            let reply = send(router.clone(), request(method, uri)).await;
            assert_eq!(reply.status, status, "{method} {uri}");
            assert!(reply.request_id().is_some(), "{method} {uri}");
            if status != StatusCode::OK {
                assert_eq!(reply.content_type(), "application/problem+json");
                assert_eq!(reply.json()["error_code"], error_code, "{method} {uri}");
                assert_eq!(
                    reply.json()["request_id"].as_str(),
                    reply.request_id().as_deref()
                );
            }
        }
        assert!(log.lines().is_empty());
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

    fn write_poi_cafes(writer: &mut Writer) {
        use crate::record::{Contact, PoiDetails, PoiRecord};
        let cafe = |id: i64, name: &str, lon: f64, details| PoiRecord {
            alt_names: Vec::new(),
            name: name.to_string(),
            category: "amenity:cafe".to_string(),
            address: None,
            geometry: point_geometry(lon, 44.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, id),
            prominence: Default::default(),
            wikidata: None,
            brand: None,
            details,
            aliases: Vec::new(),
        };
        let stated = PoiDetails::stated(
            Contact::stated(
                Some("+1 (416) 555-0111".to_string()),
                Some("HTTP://Harbour.example/Cafe".to_string()),
                None,
            ),
            Some("Mo-Fr 07:00-18:00; Sa off".to_string()),
        );
        for record in [
            cafe(101, "Saffron Cafe", -79.0, stated),
            cafe(102, "Bramble Cafe", -79.1, None),
        ] {
            writer.write(&record.into(), None).expect("write poi");
        }
    }

    #[tokio::test]
    async fn a_poi_carries_its_contact_and_hours_on_every_route() {
        let dir = TempDir::new("poi-details");
        let router = build_router(features_state(&dir), None, None);
        let contact = serde_json::json!({
            "phone": "+1 (416) 555-0111",
            "website": "HTTP://Harbour.example/Cafe",
        });
        let hours = serde_json::json!("Mo-Fr 07:00-18:00; Sa off");
        let details = |answer: &serde_json::Value| {
            let properties = &answer["features"][0]["properties"];
            (
                properties["id"].clone(),
                properties.get("contact").cloned(),
                properties.get("opening_hours").cloned(),
            )
        };
        let stating = (
            serde_json::json!("osm:node:101"),
            Some(contact),
            Some(hours),
        );
        let bare = (serde_json::json!("osm:node:102"), None, None);

        for (uri, expected) in [
            ("/places?q=Saffron%20Cafe", &stating),
            ("/places?ids=osm:node:101", &stating),
            ("/autocomplete?q=Saffron", &stating),
            ("/reverse?lon=-79.0&lat=44.0&layer=poi", &stating),
            ("/places?q=Bramble%20Cafe", &bare),
            ("/places?ids=osm:node:102", &bare),
            ("/autocomplete?q=Bramble", &bare),
            ("/reverse?lon=-79.1&lat=44.0&layer=poi", &bare),
        ] {
            let reply = send(router.clone(), request("GET", uri)).await;
            assert_eq!(reply.status, StatusCode::OK, "{uri}");
            assert_eq!(&details(&reply.json()), expected, "{uri}");
        }

        for (route, body) in [
            (
                "/places",
                r#"[{"q": "Saffron Cafe"}, {"ids": "osm:node:101"}, {"ids": "osm:node:102"}]"#,
            ),
            (
                "/reverse",
                r#"[{"lon": -79.0, "lat": 44.0, "layer": "poi"},
                    {"lon": -79.0, "lat": 44.0, "layer": "poi"},
                    {"lon": -79.1, "lat": 44.0, "layer": "poi"}]"#,
            ),
        ] {
            for method in ["POST", "QUERY"] {
                let reply = send(router.clone(), batch_request(method, route, body)).await;
                assert_eq!(reply.status, StatusCode::OK, "{method} {route}");
                let results = reply.json()["results"].clone();
                for (item, expected) in [&stating, &stating, &bare].into_iter().enumerate() {
                    assert_eq!(
                        &details(&results[item]["response"]),
                        expected,
                        "{method} {route} item {item}"
                    );
                }
            }
        }

        let rows = "{\"lon\": -79.0, \"lat\": 44.0, \"layer\": \"poi\"}\n\
                    {\"lon\": -79.1, \"lat\": 44.0, \"layer\": \"poi\"}\n";
        let reply = send(
            router.clone(),
            upload("/jobs?endpoint=reverse", "application/jsonl", rows),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let mut lines = std::str::from_utf8(&reply.body)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        lines.sort_by_key(|line| line["row"].as_u64());
        assert_eq!(lines.len(), 2);
        assert_eq!(details(&lines[0]["response"]), stating);
        assert_eq!(details(&lines[1]["response"]), bare);
    }

    fn write_shaped(writer: &mut Writer) -> crate::pack::RecordId {
        use crate::record::{PlaceLayer, PlaceRecord, PoiDetails, PoiRecord, StreetRecord};
        let poi = |id: i64, object_type, name: &str, lon: f64, details| PoiRecord {
            alt_names: Vec::new(),
            name: name.to_string(),
            category: "amenity:cafe".to_string(),
            address: None,
            geometry: point_geometry(lon, 45.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(object_type, id),
            prominence: Default::default(),
            wikidata: None,
            brand: None,
            details,
            aliases: Vec::new(),
        };
        let records: [crate::record::Record; 5] = [
            crate::record::Record::Place(
                PlaceLayer::Locality,
                PlaceRecord {
                    alt_names: Vec::new(),
                    name: "Seaview".to_string(),
                    place_type: "town".to_string(),
                    geometry: point_geometry(-79.5, 45.5),
                    source: SourceProvenance::osm(ObjectType::Relation, 207),
                    prominence: Default::default(),
                    wikidata: None,
                },
            ),
            StreetRecord {
                other_ways: Vec::new(),
                road_class: crate::record::RoadClass::Residential,
                alt_names: Vec::new(),
                locality: None,
                name: "Quay Street".to_string(),
                geometry: geojson::Geometry::new(geojson::GeometryValue::LineString {
                    coordinates: vec![
                        vec![-79.2, 45.1].into(),
                        vec![-79.19, 45.12].into(),
                        vec![-79.21, 45.11].into(),
                    ],
                }),
                representative_point: [-79.2, 45.11],
                source: SourceProvenance::osm(ObjectType::Way, 230),
            }
            .into(),
            poi(
                240,
                ObjectType::Way,
                "Foundry Hall",
                -79.0,
                PoiDetails {
                    extent: Some([-790_002_000, 449_999_000, -789_998_500, 450_001_250]),
                    ..PoiDetails::default()
                }
                .kept(),
            )
            .into(),
            poi(202, ObjectType::Node, "Copper Cafe", -79.1, None).into(),
            AddressRecord {
                building: None,
                access: Vec::new(),
                disputed: false,
                street_postcodes: Vec::new(),
                address: AddressComponents {
                    block_number_len: None,
                    areas: Vec::new(),
                    number: "10".to_string(),
                    street: Some("Wharf Street".to_string()),
                    place: None,
                    unit: None,
                    locality: Some("Millbrook".to_string()),
                    region: None,
                    postcode: Some("M5V 1A1".to_string()),
                    country: None,
                },
                geometry: point_geometry(-79.3, 45.3),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, 201),
            }
            .into(),
        ];
        let ids = records
            .iter()
            .map(|record| writer.write(record, None).expect("write"))
            .collect::<Vec<_>>();
        ids[0]
    }

    #[tokio::test]
    async fn a_result_larger_than_a_point_carries_its_bbox_on_every_route() {
        let dir = TempDir::new("bbox");
        let router = build_router(features_state(&dir), None, None);
        let boxed = |answer: &serde_json::Value| {
            let properties = &answer["features"][0]["properties"];
            (properties["id"].clone(), properties.get("bbox").cloned())
        };
        let with = |id: &str, bbox: serde_json::Value| (serde_json::json!(id), Some(bbox));
        let town = with(
            "osm:relation:207",
            serde_json::json!([-79.6, 45.4, -79.4, 45.6]),
        );
        let street = with(
            "osm:way:230",
            serde_json::json!([-79.21, 45.1, -79.19, 45.12]),
        );
        let hall = with(
            "osm:way:240",
            serde_json::json!([-79.0002, 44.9999, -78.99985, 45.000125]),
        );
        let cafe = (serde_json::json!("osm:node:202"), None);
        let house = (serde_json::json!("osm:node:201"), None);

        for (uri, expected) in [
            ("/geocode?q=Quay%20Street", &street),
            ("/geocode?q=10%20Wharf%20Street%2C%20Millbrook", &house),
            ("/geocode?q=Seaview", &town),
            ("/places?ids=osm:relation:207", &town),
            ("/places?ids=osm:way:230", &street),
            ("/places?q=Foundry%20Hall", &hall),
            ("/places?ids=osm:way:240", &hall),
            ("/places?q=Copper%20Cafe", &cafe),
            ("/autocomplete?q=Seavi", &town),
            ("/autocomplete?q=Quay%20St", &street),
            ("/autocomplete?q=Foundry", &hall),
            ("/autocomplete?q=Copper", &cafe),
            ("/reverse?lon=-79.2&lat=45.1", &street),
            ("/reverse?lon=-79.0&lat=45.0&layer=poi", &hall),
            ("/reverse?lon=-79.1&lat=45.0&layer=poi", &cafe),
            ("/reverse?lon=-79.3&lat=45.3", &house),
        ] {
            let reply = send(router.clone(), request("GET", uri)).await;
            assert_eq!(reply.status, StatusCode::OK, "{uri}");
            assert_eq!(&boxed(&reply.json()), expected, "{uri}");
        }

        for (route, body, expected) in [
            (
                "/geocode",
                r#"[{"q": "Quay Street"}, {"q": "10 Wharf Street, Millbrook"}]"#,
                [&street, &house],
            ),
            (
                "/places",
                r#"[{"ids": "osm:relation:207"}, {"q": "Copper Cafe"}]"#,
                [&town, &cafe],
            ),
            (
                "/reverse",
                r#"[{"lon": -79.0, "lat": 45.0, "layer": "poi"},
                    {"lon": -79.3, "lat": 45.3}]"#,
                [&hall, &house],
            ),
        ] {
            for method in ["POST", "QUERY"] {
                let reply = send(router.clone(), batch_request(method, route, body)).await;
                assert_eq!(reply.status, StatusCode::OK, "{method} {route}");
                let results = reply.json()["results"].clone();
                for (item, expected) in expected.into_iter().enumerate() {
                    assert_eq!(
                        &boxed(&results[item]["response"]),
                        expected,
                        "{method} {route} item {item}"
                    );
                }
            }
        }

        let rows = "{\"lon\": -79.2, \"lat\": 45.1}\n{\"lon\": -79.3, \"lat\": 45.3}\n";
        let reply = send(
            router.clone(),
            upload("/jobs?endpoint=reverse", "application/jsonl", rows),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let mut lines = std::str::from_utf8(&reply.body)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        lines.sort_by_key(|line| line["row"].as_u64());
        assert_eq!(lines.len(), 2);
        assert_eq!(boxed(&lines[0]["response"]), street);
        assert_eq!(boxed(&lines[1]["response"]), house);

        let reply = send(
            router.clone(),
            upload(
                "/jobs?endpoint=geocode",
                "text/csv",
                "address\nQuay Street\n\"10 Wharf Street, Millbrook\"\n",
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let rows = csv_rows(&reply.body);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0]["geocode_bbox"],
            "-79.2100000,45.1000000,-79.1900000,45.1200000"
        );
        assert_eq!(rows[1]["geocode_bbox"], "");
    }

    fn write_zoned(writer: &mut Writer) -> Vec<(crate::pack::RecordId, u16)> {
        use crate::context::{AdminContextTuple, RecordContext};
        use crate::record::{PlaceLayer, PlaceRecord, PoiRecord, Record};
        let area = |layer, name: &str, id: i64, lon: f64| {
            Record::Place(
                layer,
                PlaceRecord {
                    alt_names: Vec::new(),
                    name: name.to_string(),
                    place_type: "admin_level:8".to_string(),
                    geometry: point_geometry(lon, 47.5),
                    source: SourceProvenance::osm(ObjectType::Relation, id),
                    prominence: Default::default(),
                    wikidata: None,
                },
            )
        };
        let house = |id: i64, street: &str, lon: f64| -> Record {
            AddressRecord {
                building: None,
                access: Vec::new(),
                disputed: false,
                street_postcodes: Vec::new(),
                address: AddressComponents {
                    block_number_len: None,
                    areas: Vec::new(),
                    number: "10".to_string(),
                    street: Some(street.to_string()),
                    place: None,
                    unit: None,
                    locality: None,
                    region: None,
                    postcode: None,
                    country: None,
                },
                geometry: point_geometry(lon, 47.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, id),
            }
            .into()
        };
        let region = writer
            .write(&area(PlaceLayer::Region, "Lakeland", 308, -79.6), None)
            .expect("region");
        let town = writer
            .write(&area(PlaceLayer::Locality, "Portside", 307, -79.5), None)
            .expect("town");
        writer
            .write(&area(PlaceLayer::Locality, "Borderton", 309, -79.4), None)
            .expect("an area in two zones");
        writer
            .write(
                &house(301, "Maple Street", -79.0),
                Some(RecordContext {
                    admin_context: AdminContextTuple::from_tiers([
                        (PlaceLayer::Region, region),
                        (PlaceLayer::Locality, town),
                    ]),
                    flags: 0,
                    zone: None,
                    enclosing: Vec::new(),
                }),
            )
            .expect("a house in the town");
        writer
            .write(
                &house(302, "Cedar Street", -79.1),
                Some(RecordContext {
                    zone: Some(2),
                    ..RecordContext::default()
                }),
            )
            .expect("a house zoned by its point");
        writer
            .write(
                &PoiRecord {
                    alt_names: Vec::new(),
                    name: "Thistle Cafe".to_string(),
                    category: "amenity:cafe".to_string(),
                    address: None,
                    geometry: point_geometry(-79.2, 47.0),
                    location_precision: LocationPrecision::Point,
                    source: SourceProvenance::osm(ObjectType::Node, 303),
                    prominence: Default::default(),
                    wikidata: None,
                    brand: None,
                    details: None,
                    aliases: Vec::new(),
                }
                .into(),
                None,
            )
            .expect("a cafe in no zone");
        vec![(region, 1), (town, 0)]
    }

    #[tokio::test]
    async fn a_result_carries_its_timezone_on_every_route() {
        let dir = TempDir::new("timezone");
        let router = build_router(features_state(&dir), None, None);
        let osm = serde_json::json!({
            "text": "© OpenStreetMap contributors",
            "licence": "ODbL 1.0",
            "url": "https://www.openstreetmap.org/copyright",
        });
        let boundaries = serde_json::json!({
            "text": "Timezone boundaries © OpenStreetMap contributors",
            "licence": "ODbL 1.0",
            "url": "https://github.com/evansiroky/timezone-boundary-builder",
        });
        let zoned = |answer: &serde_json::Value| {
            let properties = &answer["features"][0]["properties"];
            (
                properties["id"].clone(),
                properties.get("timezone").cloned(),
                answer["geocoding"]["attribution"].clone(),
            )
        };
        let within = |id: &str, zone: &str| {
            (
                serde_json::json!(id),
                Some(serde_json::json!(zone)),
                serde_json::json!([osm, boundaries]),
            )
        };
        let without = |id: &str| (serde_json::json!(id), None, serde_json::json!([osm]));
        let region = within("osm:relation:308", "America/Winnipeg");
        let town = within("osm:relation:307", "America/Toronto");
        let border = without("osm:relation:309");
        let king = within("osm:node:301", "America/Toronto");
        let queen = within("osm:node:302", "America/Detroit");
        let cafe = without("osm:node:303");

        for (uri, expected) in [
            ("/geocode?q=10%20Maple%20Street", &king),
            ("/geocode?q=10%20Cedar%20Street", &queen),
            ("/geocode?q=Portside", &town),
            ("/geocode?q=Lakeland", &region),
            ("/geocode?q=Borderton", &border),
            ("/places?ids=osm:relation:307", &town),
            ("/places?ids=osm:relation:309", &border),
            ("/places?ids=osm:relation:308", &region),
            ("/places?q=Thistle%20Cafe", &cafe),
            ("/autocomplete?q=10%20Maple%20St", &king),
            ("/autocomplete?q=Portsi", &town),
            ("/autocomplete?q=Thistle", &cafe),
            ("/reverse?lon=-79.0&lat=47.0", &king),
            ("/reverse?lon=-79.1&lat=47.0", &queen),
            ("/reverse?lon=-79.2&lat=47.0&layer=poi", &cafe),
        ] {
            let reply = send(router.clone(), request("GET", uri)).await;
            assert_eq!(reply.status, StatusCode::OK, "{uri}");
            assert_eq!(&zoned(&reply.json()), expected, "{uri}");
        }

        for (route, body, expected) in [
            (
                "/geocode",
                r#"[{"q": "10 Maple Street"}, {"q": "Borderton"}]"#,
                [&king, &border],
            ),
            (
                "/places",
                r#"[{"ids": "osm:relation:308"}, {"q": "Thistle Cafe"}]"#,
                [&region, &cafe],
            ),
            (
                "/reverse",
                r#"[{"lon": -79.0, "lat": 47.0},
                    {"lon": -79.2, "lat": 47.0, "layer": "poi"}]"#,
                [&king, &cafe],
            ),
        ] {
            for method in ["POST", "QUERY"] {
                let reply = send(router.clone(), batch_request(method, route, body)).await;
                assert_eq!(reply.status, StatusCode::OK, "{method} {route}");
                let results = reply.json()["results"].clone();
                for (item, expected) in expected.into_iter().enumerate() {
                    assert_eq!(
                        &zoned(&results[item]["response"]),
                        expected,
                        "{method} {route} item {item}"
                    );
                }
            }
        }

        let rows = "{\"lon\": -79.1, \"lat\": 47.0}\n\
                    {\"lon\": -79.2, \"lat\": 47.0, \"layer\": \"poi\"}\n";
        let reply = send(
            router.clone(),
            upload("/jobs?endpoint=reverse", "application/jsonl", rows),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let mut lines = std::str::from_utf8(&reply.body)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        lines.sort_by_key(|line| line["row"].as_u64());
        assert_eq!(lines.len(), 2);
        assert_eq!(zoned(&lines[0]["response"]), queen);
        assert_eq!(zoned(&lines[1]["response"]), cafe);

        let reply = send(
            router.clone(),
            upload(
                "/jobs?endpoint=geocode",
                "text/csv",
                "address\n10 Maple Street\nBorderton\n",
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let rows = csv_rows(&reply.body);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["geocode_timezone"], "America/Toronto");
        assert_eq!(rows[1]["geocode_timezone"], "");

        let jobs = TempDir::new("no-timezone");
        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);
        for uri in [
            "/geocode?q=10%20King%20Street%2C%20Toronto",
            "/reverse?lon=-79.0&lat=43.0",
            "/autocomplete?q=10%20King%20St",
        ] {
            let reply = send(router.clone(), request("GET", uri)).await;
            assert_eq!(zoned(&reply.json()), without("osm:node:1"), "{uri}");
        }
    }

    #[tokio::test]
    async fn a_vendor_writes_the_zone_and_its_offset_now() {
        let dir = TempDir::new("timezone-vendors");
        let router = build_router(features_state(&dir), None, None);
        let now = |zone: &str| crate::compat::time_zone::Offset::now(zone).expect("a zone");
        let here = |uri: &'static str| {
            let router = router.clone();
            async move {
                let reply = send(router, request("GET", uri)).await;
                assert_eq!(reply.status, StatusCode::OK, "{uri}");
                let body = reply.json();
                body["items"][0]["timeZone"].clone()
            }
        };
        let zone = |name: &str| serde_json::json!({"name": name, "utcOffset": now(name).text()});
        assert_eq!(
            here("/compat/here/v1/geocode?q=10%20Maple%20Street&show=tz").await,
            zone("America/Toronto")
        );
        assert_eq!(
            here("/compat/here/v1/revgeocode?at=47.0,-79.1&show=streetInfo,tz").await,
            zone("America/Detroit")
        );
        assert_eq!(
            here("/compat/here/v1/geocode?q=10%20Maple%20Street").await,
            serde_json::Value::Null,
            "not asked"
        );

        let google = Request::builder()
            .method("POST")
            .uri("/compat/google/v1/places:searchText")
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                "x-goog-fieldmask",
                "places.timeZone,places.utcOffsetMinutes",
            )
            .body(Body::from(r#"{"textQuery": "10 Maple Street"}"#))
            .unwrap();
        let reply = send(router.clone(), google).await;
        assert_eq!(reply.status, StatusCode::OK);
        let place = reply.json()["places"][0].clone();
        assert_eq!(
            place,
            serde_json::json!({
                "timeZone": {"id": "America/Toronto"},
                "utcOffsetMinutes": now("America/Toronto").minutes(),
            })
        );
    }

    fn write_entered(writer: &mut Writer) {
        use crate::record::{Entrance, PoiDetails, PoiRecord, Record};
        let entrance = |lon: i32, lat: i32, kind: &str| Entrance {
            at: [lon, lat],
            kind: kind.to_string(),
        };
        let house = |id: i64, street: &str, lon: f64, access| -> Record {
            AddressRecord {
                building: None,
                access,
                disputed: false,
                street_postcodes: Vec::new(),
                address: AddressComponents {
                    block_number_len: None,
                    areas: Vec::new(),
                    number: "10".to_string(),
                    street: Some(street.to_string()),
                    place: None,
                    unit: None,
                    locality: None,
                    region: None,
                    postcode: None,
                    country: None,
                },
                geometry: point_geometry(lon, 49.0),
                location_precision: LocationPrecision::Centroid,
                source: SourceProvenance::osm(ObjectType::Way, id),
            }
            .into()
        };
        let poi = |id: i64, name: &str, lon: f64, details| -> Record {
            PoiRecord {
                alt_names: Vec::new(),
                name: name.to_string(),
                category: "amenity:cafe".to_string(),
                address: None,
                geometry: point_geometry(lon, 49.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Way, id),
                prominence: Default::default(),
                wikidata: None,
                brand: None,
                details,
                aliases: Vec::new(),
            }
            .into()
        };
        for record in [
            house(
                401,
                "Birch Street",
                -79.0,
                vec![
                    entrance(-790_000_500, 489_999_000, "main"),
                    entrance(-789_999_000, 490_001_250, "service"),
                ],
            ),
            house(402, "Elm Street", -79.1, Vec::new()),
            poi(
                403,
                "Beacon Hall",
                -79.2,
                PoiDetails {
                    access: vec![entrance(-792_000_000, 490_002_000, "yes")],
                    ..PoiDetails::default()
                }
                .kept(),
            ),
            poi(404, "Pebble Cafe", -79.3, None),
        ] {
            writer.write(&record, None).expect("write");
        }
    }

    #[tokio::test]
    async fn a_result_carries_its_access_points_on_every_route() {
        let dir = TempDir::new("access");
        let router = build_router(features_state(&dir), None, None);
        let entered = |answer: &serde_json::Value| {
            let properties = &answer["features"][0]["properties"];
            (properties["id"].clone(), properties.get("access").cloned())
        };
        let king = (
            serde_json::json!("osm:way:401"),
            Some(serde_json::json!([
                {"lon": -79.00005, "lat": 48.9999, "kind": "main"},
                {"lon": -78.9999, "lat": 49.000125, "kind": "service"},
            ])),
        );
        let hall = (
            serde_json::json!("osm:way:403"),
            Some(serde_json::json!([{"lon": -79.2, "lat": 49.0002, "kind": "yes"}])),
        );
        let queen = (serde_json::json!("osm:way:402"), None);
        let cafe = (serde_json::json!("osm:way:404"), None);

        for (uri, expected) in [
            ("/geocode?q=10%20Birch%20Street", &king),
            ("/geocode?q=10%20Elm%20Street", &queen),
            ("/places?q=Beacon%20Hall", &hall),
            ("/places?ids=osm:way:403", &hall),
            ("/places?q=Pebble%20Cafe", &cafe),
            ("/places?ids=osm:way:404", &cafe),
            ("/autocomplete?q=10%20Birch%20St", &king),
            ("/autocomplete?q=Beacon", &hall),
            ("/autocomplete?q=Pebble", &cafe),
            ("/reverse?lon=-79.0&lat=49.0", &king),
            ("/reverse?lon=-79.1&lat=49.0", &queen),
            ("/reverse?lon=-79.2&lat=49.0&layer=poi", &hall),
            ("/reverse?lon=-79.3&lat=49.0&layer=poi", &cafe),
        ] {
            let reply = send(router.clone(), request("GET", uri)).await;
            assert_eq!(reply.status, StatusCode::OK, "{uri}");
            assert_eq!(&entered(&reply.json()), expected, "{uri}");
        }

        for (route, body, expected) in [
            (
                "/geocode",
                r#"[{"q": "10 Birch Street"}, {"q": "10 Elm Street"}]"#,
                [&king, &queen],
            ),
            (
                "/places",
                r#"[{"ids": "osm:way:403"}, {"q": "Pebble Cafe"}]"#,
                [&hall, &cafe],
            ),
            (
                "/reverse",
                r#"[{"lon": -79.0, "lat": 49.0},
                    {"lon": -79.3, "lat": 49.0, "layer": "poi"}]"#,
                [&king, &cafe],
            ),
        ] {
            for method in ["POST", "QUERY"] {
                let reply = send(router.clone(), batch_request(method, route, body)).await;
                assert_eq!(reply.status, StatusCode::OK, "{method} {route}");
                let results = reply.json()["results"].clone();
                for (item, expected) in expected.into_iter().enumerate() {
                    assert_eq!(
                        &entered(&results[item]["response"]),
                        expected,
                        "{method} {route} item {item}"
                    );
                }
            }
        }

        let rows = "{\"lon\": -79.2, \"lat\": 49.0, \"layer\": \"poi\"}\n\
                    {\"lon\": -79.1, \"lat\": 49.0}\n";
        let reply = send(
            router.clone(),
            upload("/jobs?endpoint=reverse", "application/jsonl", rows),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let mut lines = std::str::from_utf8(&reply.body)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        lines.sort_by_key(|line| line["row"].as_u64());
        assert_eq!(lines.len(), 2);
        assert_eq!(entered(&lines[0]["response"]), hall);
        assert_eq!(entered(&lines[1]["response"]), queen);

        let reply = send(
            router.clone(),
            upload(
                "/jobs?endpoint=geocode",
                "text/csv",
                "address\n10 Birch Street\n10 Elm Street\n",
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let rows = csv_rows(&reply.body);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0]["geocode_access"],
            "-79.0000500,48.9999000,main; -78.9999000,49.0001250,service"
        );
        assert_eq!(rows[1]["geocode_access"], "");
    }

    pub(crate) fn write_stated_names_pack(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
        let mut writer = Writer::create(dir).expect("writer");
        let mut areas = Areas::default();
        write_stated_names(&mut writer, &mut areas);
        writer.finish().expect("finish");
    }

    fn write_stated_names(writer: &mut Writer, areas: &mut Areas) {
        use crate::context::{AdminContextTuple, RecordContext};
        use crate::parse::lattice::CountryCode;
        use crate::record::{PlaceLayer, PlaceRecord, Record};
        let mut area = |layer, name: &str, id: i64| {
            let record = Record::Place(
                layer,
                PlaceRecord {
                    alt_names: Vec::new(),
                    name: name.to_string(),
                    place_type: "admin_level:4".to_string(),
                    geometry: point_geometry(0.0, 0.0),
                    source: SourceProvenance::osm(ObjectType::Relation, id),
                    prominence: Default::default(),
                    wikidata: None,
                },
            );
            writer.write(&record, None).expect("area")
        };
        let austria = area(PlaceLayer::Country, "Österreich", 1);
        let wien = area(PlaceLayer::Region, "Wien", 2);
        let germany = area(PlaceLayer::Country, "Deutschland", 3);
        let munich = area(PlaceLayer::Locality, "München", 4);
        let russia = area(PlaceLayer::Country, "Россия", 5);
        let moscow = area(PlaceLayer::Locality, "Москва", 6);
        let switzerland = area(PlaceLayer::Country, "Schweiz/Suisse/Svizzera/Svizra", 7);
        let conthey = area(PlaceLayer::District, "Conthey", 8);
        let code = |code: &str| CountryCode::parse(code).expect("code");
        areas.country_codes.extend([
            (austria, code("AT")),
            (germany, code("DE")),
            (russia, code("RU")),
            (switzerland, code("CH")),
        ]);
        let names = |names: [(&str, &str); 4]| {
            names
                .iter()
                .map(|(code, name)| (code.to_string(), name.to_string()))
                .collect::<Vec<_>>()
        };
        areas.language_names.extend([
            (
                austria,
                names([
                    ("en", "Austria"),
                    ("fr", "Autriche"),
                    ("de", "Österreich"),
                    ("ru", "Австрия"),
                ]),
            ),
            (
                wien,
                names([
                    ("en", "Vienna"),
                    ("fr", "Vienne"),
                    ("de", "Wien"),
                    ("ru", "Вена"),
                ]),
            ),
            (
                germany,
                names([
                    ("en", "Germany"),
                    ("fr", "Allemagne"),
                    ("de", "Deutschland"),
                    ("ru", "Германия"),
                ]),
            ),
            (
                munich,
                names([
                    ("en", "Munich"),
                    ("fr", "Munich"),
                    ("de", "München"),
                    ("ru", "Мюнхен"),
                ]),
            ),
            (
                russia,
                names([
                    ("en", "Russia"),
                    ("fr", "Russie"),
                    ("de", "Russland"),
                    ("ru", "Россия"),
                ]),
            ),
            (
                moscow,
                names([
                    ("en", "Moscow"),
                    ("fr", "Moscou"),
                    ("de", "Moskau"),
                    ("ru", "Москва"),
                ]),
            ),
            (
                switzerland,
                names([
                    ("de", "Schweiz"),
                    ("fr", "Suisse"),
                    ("it", "Svizzera"),
                    ("rm", "Svizra"),
                ]),
            ),
            (
                conthey,
                names([
                    ("de", "Gundis"),
                    ("fr", "Conthey"),
                    ("it", "Conthey"),
                    ("rm", "Gundis"),
                ]),
            ),
        ]);
        areas.replace_writer_maps(writer);
        let within = |tiers: &[(PlaceLayer, crate::pack::RecordId)]| {
            Some(RecordContext {
                admin_context: AdminContextTuple::from_tiers(tiers.iter().copied()),
                flags: 0,
                zone: None,
                enclosing: Vec::new(),
            })
        };
        let house = |id: i64,
                     lon: f64,
                     number: &str,
                     street: &str,
                     town: &str,
                     postcode: &str,
                     country: Option<&str>|
         -> Record {
            AddressRecord {
                building: None,
                access: Vec::new(),
                disputed: false,
                street_postcodes: Vec::new(),
                address: AddressComponents {
                    block_number_len: None,
                    areas: Vec::new(),
                    number: number.to_string(),
                    street: Some(street.to_string()),
                    place: None,
                    unit: None,
                    locality: Some(town.to_string()),
                    region: None,
                    postcode: Some(postcode.to_string()),
                    country: country.map(str::to_string),
                },
                geometry: point_geometry(lon, 10.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, id),
            }
            .into()
        };
        for (record, context) in [
            (
                house(11, 10.0, "42", "Heinestraße", "Vienna", "1020", Some("AT")),
                within(&[(PlaceLayer::Country, austria), (PlaceLayer::Region, wien)]),
            ),
            (
                house(12, 11.0, "1", "Marienplatz", "Munich", "80331", None),
                within(&[
                    (PlaceLayer::Country, germany),
                    (PlaceLayer::Locality, munich),
                ]),
            ),
            (
                house(
                    13,
                    12.0,
                    "43",
                    "Измайловский бульвар",
                    "Moscow",
                    "105043",
                    None,
                ),
                within(&[
                    (PlaceLayer::Country, russia),
                    (PlaceLayer::Locality, moscow),
                ]),
            ),
            (
                house(
                    15,
                    14.0,
                    "67",
                    "Route de la Télécabine",
                    "Nendaz",
                    "1997",
                    None,
                ),
                within(&[
                    (PlaceLayer::Country, switzerland),
                    (PlaceLayer::District, conthey),
                ]),
            ),
            (
                house(14, 13.0, "5", "Hauptstraße", "Schwabing", "80801", None),
                within(&[
                    (PlaceLayer::Country, germany),
                    (PlaceLayer::Locality, munich),
                ]),
            ),
        ] {
            writer.write(&record, context).expect("write");
        }
    }

    #[tokio::test]
    async fn a_stated_place_is_named_as_its_area_in_every_language() {
        let dir = TempDir::new("stated-names");
        let router = build_router(features_state(&dir), None, None);
        let named = |answer: &serde_json::Value| {
            let properties = &answer["features"][0]["properties"];
            (
                properties["label"].clone(),
                properties["address"]["locality"].clone(),
                properties["address_lines"].clone(),
            )
        };
        let expect = |label: &str, town: &str, lines: &[&str]| {
            (
                serde_json::json!(label),
                serde_json::json!(town),
                serde_json::json!(lines),
            )
        };
        for (route, lon, lang, expected) in [
            (
                "/geocode?q=42%20Heinestra%C3%9Fe",
                10.0,
                "",
                expect(
                    "42 Heinestraße, Wien, 1020, AT",
                    "Wien",
                    &["Heinestraße 42", "1020 Wien", "Österreich"],
                ),
            ),
            (
                "/geocode?q=42%20Heinestra%C3%9Fe",
                10.0,
                "en",
                expect(
                    "42 Heinestraße, Vienna, 1020, AT",
                    "Vienna",
                    &["Heinestraße 42", "1020 Vienna", "Austria"],
                ),
            ),
            (
                "/geocode?q=42%20Heinestra%C3%9Fe",
                10.0,
                "fr",
                expect(
                    "42 Heinestraße, Vienne, 1020, AT",
                    "Vienne",
                    &["Heinestraße 42", "1020 Vienne", "Autriche"],
                ),
            ),
            (
                "/geocode?q=42%20Heinestra%C3%9Fe",
                10.0,
                "ru",
                expect(
                    "42 Heinestraße, Вена, 1020, AT",
                    "Вена",
                    &["Heinestraße 42", "1020 Вена", "Австрия"],
                ),
            ),
            (
                "/geocode?q=1%20Marienplatz",
                11.0,
                "",
                expect(
                    "1 Marienplatz, München, 80331",
                    "München",
                    &["Marienplatz 1", "80331 München", "Deutschland"],
                ),
            ),
            (
                "/geocode?q=1%20Marienplatz",
                11.0,
                "fr",
                expect(
                    "1 Marienplatz, Munich, 80331",
                    "Munich",
                    &["Marienplatz 1", "80331 Munich", "Allemagne"],
                ),
            ),
            (
                "/geocode?q=1%20Marienplatz",
                11.0,
                "ru",
                expect(
                    "1 Marienplatz, Мюнхен, 80331",
                    "Мюнхен",
                    &["Marienplatz 1", "80331 Мюнхен", "Германия"],
                ),
            ),
            (
                "/geocode?q=43%20%D0%98%D0%B7%D0%BC%D0%B0%D0%B9%D0%BB%D0%BE%D0%B2%D1%81%D0%BA%D0%B8%D0%B9%20%D0%B1%D1%83%D0%BB%D1%8C%D0%B2%D0%B0%D1%80",
                12.0,
                "",
                expect(
                    "43 Измайловский бульвар, Москва, 105043",
                    "Москва",
                    &["Измайловский бульвар, 43", "Москва", "Россия", "105043"],
                ),
            ),
            (
                "/geocode?q=43%20%D0%98%D0%B7%D0%BC%D0%B0%D0%B9%D0%BB%D0%BE%D0%B2%D1%81%D0%BA%D0%B8%D0%B9%20%D0%B1%D1%83%D0%BB%D1%8C%D0%B2%D0%B0%D1%80",
                12.0,
                "de",
                expect(
                    "43 Измайловский бульвар, Moskau, 105043",
                    "Moskau",
                    &["Измайловский бульвар, 43", "Moskau", "Russland", "105043"],
                ),
            ),
            (
                "/geocode?q=5%20Hauptstra%C3%9Fe",
                13.0,
                "en",
                expect(
                    "5 Hauptstraße, Schwabing, 80801",
                    "Schwabing",
                    &["Hauptstraße 5", "80801 Schwabing", "Germany"],
                ),
            ),
        ] {
            let lang = if lang.is_empty() {
                String::new()
            } else {
                format!("&lang={lang}")
            };
            for uri in [
                format!("{route}{lang}"),
                format!("/reverse?lon={lon}&lat=10.0{lang}"),
            ] {
                let reply = send(router.clone(), request("GET", &uri)).await;
                assert_eq!(reply.status, StatusCode::OK, "{uri}");
                assert_eq!(named(&reply.json()), expected, "{uri}");
            }
        }
    }

    struct Canada {
        canada: crate::pack::RecordId,
        ontario: crate::pack::RecordId,
        toronto: crate::pack::RecordId,
        germany: crate::pack::RecordId,
    }

    fn within(
        tiers: &[(crate::record::PlaceLayer, crate::pack::RecordId)],
    ) -> Option<crate::context::RecordContext> {
        Some(crate::context::RecordContext {
            admin_context: crate::context::AdminContextTuple::from_tiers(tiers.iter().copied()),
            flags: 0,
            zone: None,
            enclosing: Vec::new(),
        })
    }

    fn write_canada(writer: &mut Writer, areas: &mut Areas) -> Canada {
        use crate::parse::lattice::CountryCode;
        use crate::record::{PlaceLayer, PlaceRecord, Record};
        let mut place = |layer, name: &str, id, context| {
            let area = PlaceRecord {
                alt_names: Vec::new(),
                name: name.to_string(),
                place_type: "admin_level:8".to_string(),
                geometry: point_geometry(-79.6, 52.5),
                source: SourceProvenance::osm(ObjectType::Relation, id),
                prominence: Default::default(),
                wikidata: None,
            };
            let id = writer.write(&Record::Place(layer, area), context);
            id.expect("area")
        };
        let canada = place(PlaceLayer::Country, "Canada", 511, None);
        let in_canada = [(PlaceLayer::Country, canada)];
        let ontario = place(PlaceLayer::Region, "Ontario", 512, within(&in_canada));
        let in_ontario = [in_canada[0], (PlaceLayer::Region, ontario)];
        let toronto = place(PlaceLayer::Locality, "Toronto", 513, within(&in_ontario));
        let germany = place(PlaceLayer::Country, "Deutschland", 514, None);
        let code = |code: &str| CountryCode::parse(code).expect("code");
        areas
            .country_codes
            .extend([(canada, code("CA")), (germany, code("DE"))]);
        areas.subdivision_codes.insert(ontario, "CA-ON".to_string());
        areas
            .language_names
            .insert(canada, vec![("de".to_string(), "Kanada".to_string())]);
        areas.context_names.extend([
            (canada, vec!["Canada".to_string()]),
            (ontario, vec!["Ontario".to_string()]),
            (toronto, vec!["Toronto".to_string()]),
        ]);
        areas.replace_writer_maps(writer);
        Canada {
            canada,
            ontario,
            toronto,
            germany,
        }
    }

    fn write_lined(writer: &mut Writer, areas: &Canada) {
        use crate::record::{PlaceLayer, PoiRecord, Record};
        let stated = |number: &str, street: &str| AddressComponents {
            block_number_len: None,
            areas: Vec::new(),
            number: number.to_string(),
            street: Some(street.to_string()),
            place: None,
            unit: None,
            locality: None,
            region: None,
            postcode: None,
            country: None,
        };
        let house = |id: i64, address: AddressComponents, lon: f64| -> Record {
            AddressRecord {
                building: None,
                access: Vec::new(),
                disputed: false,
                street_postcodes: Vec::new(),
                address,
                geometry: point_geometry(lon, 51.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, id),
            }
            .into()
        };
        let poi = |id: i64, name: &str, address, lon: f64| -> Record {
            PoiRecord {
                alt_names: Vec::new(),
                name: name.to_string(),
                category: "amenity:cafe".to_string(),
                address,
                geometry: point_geometry(lon, 51.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, id),
                prominence: Default::default(),
                wikidata: None,
                brand: None,
                details: None,
                aliases: Vec::new(),
            }
            .into()
        };
        let in_toronto = || {
            within(&[
                (PlaceLayer::Country, areas.canada),
                (PlaceLayer::Region, areas.ontario),
                (PlaceLayer::Locality, areas.toronto),
            ])
        };
        for (record, context) in [
            (
                house(
                    501,
                    AddressComponents {
                        postcode: Some("M5J 1A1".to_string()),
                        ..stated("10", "Front Street West")
                    },
                    -79.0,
                ),
                in_toronto(),
            ),
            (
                house(
                    502,
                    AddressComponents {
                        unit: Some("Apt 4".to_string()),
                        ..stated("20", "Dundas Street")
                    },
                    -79.1,
                ),
                in_toronto(),
            ),
            (
                house(
                    503,
                    AddressComponents {
                        locality: Some("Berlin".to_string()),
                        postcode: Some("10117".to_string()),
                        ..stated("12", "Unter den Linden")
                    },
                    -79.2,
                ),
                within(&[(PlaceLayer::Country, areas.germany)]),
            ),
            (house(504, stated("30", "Lone Road"), -79.3), None),
            (
                poi(505, "Ember Cafe", Some(stated("5", "Bay Street")), -79.4),
                in_toronto(),
            ),
            (poi(506, "Granary Hall", None, -79.5), in_toronto()),
        ] {
            writer.write(&record, context).expect("write");
        }
    }

    #[tokio::test]
    async fn a_result_carries_its_address_lines_on_every_route() {
        let dir = TempDir::new("address-lines");
        let router = build_router(features_state(&dir), None, None);
        let lined = |answer: &serde_json::Value| {
            let properties = &answer["features"][0]["properties"];
            (
                properties["id"].clone(),
                properties.get("address_lines").cloned(),
            )
        };
        let with = |id: &str, lines: serde_json::Value| (serde_json::json!(id), Some(lines));
        let without = |id: &str| (serde_json::json!(id), None);
        let king = with(
            "osm:node:501",
            serde_json::json!(["10 Front Street West", "Toronto, ON M5J 1A1", "Canada"]),
        );
        let queen = with(
            "osm:node:502",
            serde_json::json!(["Apt 4", "20 Dundas Street", "Toronto, ON", "Canada"]),
        );
        let linden = with(
            "osm:node:503",
            serde_json::json!(["Unter den Linden 12", "10117 Berlin", "Deutschland"]),
        );
        let lone = without("osm:node:504");
        let cafe = with(
            "osm:node:505",
            serde_json::json!(["5 Bay Street", "Toronto, ON", "Canada"]),
        );
        let hall = without("osm:node:506");
        let town = without("osm:relation:513");
        let king_in_german = with(
            "osm:node:501",
            serde_json::json!(["10 Front Street West", "Toronto, ON M5J 1A1", "Kanada"]),
        );

        for (uri, expected) in [
            ("/geocode?q=10%20Front%20Street%20West", &king),
            (
                "/geocode?q=10%20Front%20Street%20West&lang=de",
                &king_in_german,
            ),
            ("/geocode?q=20%20Dundas%20Street", &queen),
            ("/geocode?q=Unter%20den%20Linden%2012", &linden),
            ("/geocode?q=30%20Lone%20Road", &lone),
            ("/geocode?q=Toronto", &town),
            ("/places?q=Ember%20Cafe", &cafe),
            ("/places?ids=osm:node:505", &cafe),
            (
                "/places?ids=osm:node:505&lang=de",
                &with(
                    "osm:node:505",
                    serde_json::json!(["5 Bay Street", "Toronto, ON", "Kanada"]),
                ),
            ),
            ("/places?q=Granary%20Hall", &hall),
            ("/places?ids=osm:node:506", &hall),
            ("/autocomplete?q=10%20Front%20St", &king),
            ("/autocomplete?q=10%20Front%20St&lang=de", &king_in_german),
            ("/autocomplete?q=Ember", &cafe),
            ("/autocomplete?q=Granary", &hall),
            ("/reverse?lon=-79.0&lat=51.0", &king),
            ("/reverse?lon=-79.0&lat=51.0&lang=de", &king_in_german),
            ("/reverse?lon=-79.1&lat=51.0", &queen),
            ("/reverse?lon=-79.2&lat=51.0", &linden),
            ("/reverse?lon=-79.3&lat=51.0", &lone),
            ("/reverse?lon=-79.4&lat=51.0&layer=poi", &cafe),
            ("/reverse?lon=-79.5&lat=51.0&layer=poi", &hall),
        ] {
            let reply = send(router.clone(), request("GET", uri)).await;
            assert_eq!(reply.status, StatusCode::OK, "{uri}");
            assert_eq!(&lined(&reply.json()), expected, "{uri}");
        }
        let reply = send(
            router.clone(),
            request("GET", "/reverse?lon=-79.0&lat=51.0"),
        )
        .await;
        let areas = reply.json()["features"]
            .as_array()
            .expect("features")
            .iter()
            .skip(1)
            .map(|feature| {
                let properties = &feature["properties"];
                (
                    properties["layer"].clone(),
                    properties.get("address_lines").cloned(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            areas,
            ["locality", "region", "country"].map(|layer| (serde_json::json!(layer), None))
        );

        for (route, body, expected) in [
            (
                "/geocode",
                r#"[{"q": "10 Front Street West"}, {"q": "30 Lone Road"},
                    {"q": "10 Front Street West", "lang": "de"}]"#,
                [&king, &lone, &king_in_german],
            ),
            (
                "/places",
                r#"[{"ids": "osm:node:505"}, {"q": "Granary Hall"}, {"q": "Ember Cafe"}]"#,
                [&cafe, &hall, &cafe],
            ),
            (
                "/reverse",
                r#"[{"lon": -79.1, "lat": 51.0},
                    {"lon": -79.5, "lat": 51.0, "layer": "poi"},
                    {"lon": -79.2, "lat": 51.0}]"#,
                [&queen, &hall, &linden],
            ),
        ] {
            for method in ["POST", "QUERY"] {
                let reply = send(router.clone(), batch_request(method, route, body)).await;
                assert_eq!(reply.status, StatusCode::OK, "{method} {route}");
                let results = reply.json()["results"].clone();
                for (item, expected) in expected.into_iter().enumerate() {
                    assert_eq!(
                        &lined(&results[item]["response"]),
                        expected,
                        "{method} {route} item {item}"
                    );
                }
            }
        }

        let rows = "{\"lon\": -79.4, \"lat\": 51.0, \"layer\": \"poi\"}\n\
                    {\"lon\": -79.3, \"lat\": 51.0}\n";
        let reply = send(
            router.clone(),
            upload("/jobs?endpoint=reverse", "application/jsonl", rows),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let mut lines = std::str::from_utf8(&reply.body)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        lines.sort_by_key(|line| line["row"].as_u64());
        assert_eq!(lines.len(), 2);
        assert_eq!(lined(&lines[0]["response"]), cafe);
        assert_eq!(lined(&lines[1]["response"]), lone);

        let reply = send(
            router.clone(),
            upload(
                "/jobs?endpoint=geocode",
                "text/csv",
                "address\n10 Front Street West\n30 Lone Road\n20 Dundas Street\n",
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let rows = csv_rows(&reply.body);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0]["geocode_address_lines"],
            "10 Front Street West; Toronto, ON M5J 1A1; Canada"
        );
        assert_eq!(rows[1]["geocode_address_lines"], "");
        assert_eq!(
            rows[2]["geocode_address_lines"],
            "Apt 4; 20 Dundas Street; Toronto, ON; Canada"
        );
    }

    fn write_matched(writer: &mut Writer, areas: &Canada) {
        use crate::record::{PlaceLayer, PoiRecord};
        let context = within(&[
            (PlaceLayer::Country, areas.canada),
            (PlaceLayer::Region, areas.ontario),
            (PlaceLayer::Locality, areas.toronto),
        ]);
        let address = AddressComponents {
            block_number_len: None,
            areas: Vec::new(),
            number: "10".to_string(),
            street: Some("King Street West".to_string()),
            place: None,
            unit: None,
            locality: None,
            region: None,
            postcode: Some("M5H 1A1".to_string()),
            country: None,
        };
        let house = AddressRecord {
            building: None,
            access: Vec::new(),
            disputed: false,
            street_postcodes: Vec::new(),
            address,
            geometry: point_geometry(-79.0, 53.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, 601),
        };
        let cafe = PoiRecord {
            alt_names: Vec::new(),
            name: "Lantern Cafe".to_string(),
            category: "amenity:cafe".to_string(),
            address: None,
            geometry: point_geometry(-79.4, 53.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, 605),
            prominence: Default::default(),
            wikidata: None,
            brand: None,
            details: None,
            aliases: Vec::new(),
        };
        let abbreviated = AddressRecord {
            building: None,
            address: AddressComponents {
                number: "12".to_string(),
                street: Some("Queen St W".to_string()),
                postcode: None,
                ..house.address.clone()
            },
            geometry: point_geometry(-79.2, 53.0),
            source: SourceProvenance::osm(ObjectType::Node, 602),
            ..house.clone()
        };
        writer.write(&house.into(), context.clone()).expect("house");
        writer
            .write(&abbreviated.into(), context.clone())
            .expect("abbreviated house");
        writer.write(&cafe.into(), context).expect("cafe");
    }

    fn write_reverse_houses(writer: &mut Writer) {
        for (id, number, lon) in [
            (801, "10", -79.0),
            (802, "12", -79.0001),
            (803, "14", -79.0002),
        ] {
            let house = AddressRecord {
                building: None,
                access: Vec::new(),
                disputed: false,
                street_postcodes: Vec::new(),
                address: AddressComponents {
                    block_number_len: None,
                    areas: Vec::new(),
                    number: number.to_string(),
                    street: Some("Mill Lane".to_string()),
                    place: None,
                    unit: None,
                    locality: None,
                    region: None,
                    postcode: None,
                    country: None,
                },
                geometry: point_geometry(lon, 43.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, id),
            };
            writer.write(&house.into(), None).expect("house");
        }
    }

    #[tokio::test]
    async fn a_reverse_with_a_layer_answers_its_nearest_results_up_to_the_limit() {
        let dir = TempDir::new("reverse-limit");
        let router = build_router(features_state(&dir), None, None);
        let ids = |answer: &serde_json::Value| {
            answer["features"]
                .as_array()
                .expect("features")
                .iter()
                .map(|feature| {
                    feature["properties"]["id"]
                        .as_str()
                        .expect("id")
                        .to_string()
                })
                .collect::<Vec<_>>()
        };
        let reply = send(
            router.clone(),
            request(
                "GET",
                "/reverse?lon=-79.00005&lat=43.0&layer=address&limit=5",
            ),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        let nearest = ids(&reply.json());
        assert_eq!(nearest.len(), 3, "{nearest:?}");
        assert_eq!(nearest[2], "osm:node:803", "nearest first");
        let reply = send(
            router.clone(),
            request(
                "GET",
                "/reverse?lon=-79.00005&lat=43.0&layer=address&limit=2",
            ),
        )
        .await;
        assert_eq!(ids(&reply.json()).len(), 2);
        let reply = send(
            router.clone(),
            request("GET", "/reverse?lon=-79.00005&lat=43.0&layer=address"),
        )
        .await;
        assert_eq!(ids(&reply.json()).len(), 1);
        let reply = send(
            router,
            request("GET", "/reverse?lon=-79.0&lat=43.0&limit=2"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    }

    fn write_reverse_streets(writer: &mut Writer) {
        let street = |id, name: &str, coordinates: Vec<[f64; 2]>| crate::record::StreetRecord {
            other_ways: Vec::new(),
            road_class: crate::record::RoadClass::Residential,
            alt_names: Vec::new(),
            locality: None,
            name: name.to_string(),
            representative_point: coordinates[0],
            geometry: geojson::Geometry::new(geojson::GeometryValue::LineString {
                coordinates: coordinates
                    .into_iter()
                    .map(|point| point.to_vec().into())
                    .collect(),
            }),
            source: SourceProvenance::osm(ObjectType::Way, id),
        };
        let near = (0..8)
            .map(|step| [-79.0 + f64::from(step) * 0.00002, 59.00005])
            .collect();
        writer
            .write(&street(901, "Court Street", near).into(), None)
            .expect("near");
        writer
            .write(
                &street(
                    902,
                    "Park Place",
                    vec![[-79.0, 59.0001], [-78.9998, 59.0001]],
                )
                .into(),
                None,
            )
            .expect("far");
    }

    #[tokio::test]
    async fn a_reverse_with_a_limit_answers_each_street_once() {
        let dir = TempDir::new("reverse-streets");
        let router = build_router(features_state(&dir), None, None);
        let ids = |answer: serde_json::Value| {
            answer["features"]
                .as_array()
                .expect("features")
                .iter()
                .map(|feature| {
                    feature["properties"]["id"]
                        .as_str()
                        .expect("id")
                        .to_string()
                })
                .collect::<Vec<_>>()
        };
        let reply = send(
            router.clone(),
            request(
                "GET",
                "/reverse?lon=-79.00005&lat=59.0&layer=street&limit=5",
            ),
        )
        .await;
        assert_eq!(ids(reply.json()), ["osm:way:901", "osm:way:902"]);
        let reply = send(
            router,
            request("GET", "/reverse?lon=-79.00005&lat=59.0&layer=street"),
        )
        .await;
        assert_eq!(ids(reply.json()), ["osm:way:901"]);
    }

    #[tokio::test]
    async fn a_geocode_result_says_how_the_query_matched_it_on_every_form() {
        let dir = TempDir::new("query-match");
        let router = build_router(features_state(&dir), None, None);
        let matched = |answer: &serde_json::Value| {
            let properties = &answer["features"][0]["properties"];
            (properties["id"].clone(), properties.get("match").cloned())
        };
        let with = |id: &str, parts: serde_json::Value| (serde_json::json!(id), Some(parts));
        let without = |id: &str| (serde_json::json!(id), None);
        let house = |more: &[(&str, &str)]| {
            let mut parts = serde_json::json!({"housenumber": "matched", "street": "matched"});
            for (part, how) in more {
                parts[part] = serde_json::json!(how);
            }
            with("osm:node:601", parts)
        };
        let in_town = house(&[("locality", "matched")]);
        let in_postcode = house(&[("postcode", "matched")]);
        let typo = with(
            "osm:node:601",
            serde_json::json!({"housenumber": "matched", "street": "partial"}),
        );
        let town_for_it = with(
            "osm:relation:513",
            serde_json::json!({
                "housenumber": "unmatched", "street": "unmatched", "locality": "matched",
            }),
        );
        let by_fields = house(&[("locality", "matched"), ("postcode", "matched")]);
        let in_region = house(&[("region", "matched")]);
        let town_by_fields = with(
            "osm:relation:513",
            serde_json::json!({
                "housenumber": "unmatched", "street": "unmatched", "locality": "matched",
                "region": "matched",
            }),
        );
        let town = with(
            "osm:relation:513",
            serde_json::json!({"locality": "matched"}),
        );
        let cafe = without("osm:node:605");
        let queen = with(
            "osm:node:602",
            serde_json::json!({"housenumber": "matched", "street": "matched"}),
        );
        let queen_in_part = with(
            "osm:node:602",
            serde_json::json!({"housenumber": "matched", "street": "partial"}),
        );

        for (uri, expected) in [
            ("/geocode?q=10%20King%20Street%20West,%20Toronto", &in_town),
            (
                "/geocode?q=10%20King%20Street%20West%20M5H%201A1",
                &in_postcode,
            ),
            ("/geocode?q=10%20Knig%20Street%20West", &typo),
            ("/geocode?q=12%20Queen%20Street%20West", &queen),
            ("/geocode?q=99%20Nowhere%20Road,%20Toronto", &town_for_it),
            (
                "/geocode?address=10%20King%20Street%20West&locality=Toronto&postcode=M5H%201A1",
                &by_fields,
            ),
            (
                "/geocode?address=10%20King%20Street%20West&region=Ontario",
                &in_region,
            ),
            (
                "/geocode?address=99%20Nowhere%20Road&locality=Toronto&region=Ontario",
                &town_by_fields,
            ),
            ("/geocode?q=Toronto", &town),
            ("/geocode?q=12%20Queen%20Street", &queen_in_part),
            (
                "/geocode?q=10%20King%20Street%20West&country=CA",
                &house(&[("country", "matched")]),
            ),
            ("/places?q=Lantern%20Cafe,%20Toronto", &cafe),
            ("/places?ids=osm:node:605", &cafe),
            ("/autocomplete?q=10%20King%20St", &without("osm:node:601")),
        ] {
            let reply = send(router.clone(), request("GET", uri)).await;
            assert_eq!(reply.status, StatusCode::OK, "{uri}");
            assert_eq!(&matched(&reply.json()), expected, "{uri}");
        }
        let reply = send(
            router.clone(),
            request("GET", "/geocode?q=99%20Nowhere%20Road,%20Toronto"),
        )
        .await;
        assert_eq!(
            reply.json()["features"][0]["properties"]["fallback"],
            serde_json::json!(true)
        );

        let body = r#"[{"q": "10 King Street West, Toronto"},
                       {"address": "10 King Street West", "locality": "Toronto",
                        "postcode": "M5H 1A1"},
                       {"q": "99 Nowhere Road, Toronto"}, {"q": "Toronto"}]"#;
        for method in ["POST", "QUERY"] {
            let reply = send(router.clone(), batch_request(method, "/geocode", body)).await;
            assert_eq!(reply.status, StatusCode::OK, "{method}");
            let results = reply.json()["results"].clone();
            for (item, expected) in [&in_town, &by_fields, &town_for_it, &town]
                .into_iter()
                .enumerate()
            {
                assert_eq!(
                    &matched(&results[item]["response"]),
                    expected,
                    "{method} item {item}"
                );
            }
        }
        let reply = send(
            router.clone(),
            batch_request("POST", "/places", r#"[{"q": "Lantern Cafe, Toronto"}]"#),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(matched(&reply.json()["results"][0]["response"]), cafe);

        let rows = "{\"q\": \"10 Knig Street West\"}\n\
                    {\"address\": \"10 King Street West\", \"region\": \"Ontario\"}\n";
        let reply = send(
            router.clone(),
            upload("/jobs?endpoint=geocode", "application/jsonl", rows),
        )
        .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let mut lines = std::str::from_utf8(&reply.body)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        lines.sort_by_key(|line| line["row"].as_u64());
        assert_eq!(lines.len(), 2);
        assert_eq!(matched(&lines[0]["response"]), typo);
        assert_eq!(matched(&lines[1]["response"]), in_region);
    }

    #[tokio::test]
    async fn a_suggestion_carries_its_highlights_and_no_other_result_does() {
        let dir = TempDir::new("highlights");
        let router = build_router(features_state(&dir), None, None);
        let highlights = |answer: &serde_json::Value| {
            let properties = &answer["features"][0]["properties"];
            (
                properties["label"].clone(),
                properties.get("highlights").cloned(),
            )
        };
        let range = |start: u64, end: u64| serde_json::json!({"start": start, "end": end});
        let house = serde_json::json!("10 King Street West, M5H 1A1");
        for (uri, expected) in [
            (
                "/autocomplete?q=10%20King%20St",
                (
                    house.clone(),
                    Some(serde_json::json!([range(0, 2), range(3, 7), range(8, 10)])),
                ),
            ),
            (
                "/autocomplete?q=lant",
                (
                    serde_json::json!("Lantern Cafe, Toronto"),
                    Some(serde_json::json!([range(0, 4)])),
                ),
            ),
            (
                "/autocomplete?q=lantern&layer=poi",
                (
                    serde_json::json!("Lantern Cafe, Toronto"),
                    Some(serde_json::json!([range(0, 7)])),
                ),
            ),
            (
                "/autocomplete?q=cafe&focus=-79.4,53.0",
                (
                    serde_json::json!("Lantern Cafe, Toronto"),
                    Some(serde_json::json!([range(8, 12)])),
                ),
            ),
            ("/geocode?q=10%20King%20Street%20West", (house, None)),
            (
                "/places?q=Lantern%20Cafe",
                (serde_json::json!("Lantern Cafe, Toronto"), None),
            ),
            (
                "/places?q=coffeehouse&focus=-79.4,53.0",
                (serde_json::json!("Lantern Cafe, Toronto"), None),
            ),
            (
                "/places?ids=osm:node:605",
                (serde_json::json!("Lantern Cafe, Toronto"), None),
            ),
        ] {
            let reply = send(router.clone(), request("GET", uri)).await;
            assert_eq!(reply.status, StatusCode::OK, "{uri}");
            assert_eq!(highlights(&reply.json()), expected, "{uri}");
        }
    }

    #[tokio::test]
    async fn jobs_stream_a_csv_back_with_each_rows_answer() {
        let jobs = TempDir::new("jobs-csv");
        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);

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
    }

    #[tokio::test]
    async fn jobs_answer_jsonl_rows_as_their_get() {
        let jobs = TempDir::new("jobs-jsonl");
        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);

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
    }

    #[tokio::test]
    async fn jobs_answer_at_once_when_asked() {
        let jobs = TempDir::new("jobs-async");
        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);

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
    }

    #[tokio::test]
    async fn jobs_refuse_what_they_cannot_read() {
        let jobs = TempDir::new("jobs-refused");
        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);
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
    }

    #[tokio::test]
    async fn jobs_outlive_a_restart() {
        let jobs = TempDir::new("jobs-restart");
        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);
        let reply = send(
            router,
            upload("/jobs", "text/csv", "street\n10 King Street\n"),
        )
        .await;
        let location = header_value(&reply, "location")
            .expect("location")
            .to_string();
        std::fs::write(
            jobs.join("interrupted.json"),
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

        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);
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
    }

    #[tokio::test]
    async fn batch_answers_each_item_alone() {
        let jobs = TempDir::new("batch-items");
        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);

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
        assert!(
            results[0]["response"]["features"][0]["properties"]
                .get("confidence")
                .is_none(),
            "a Pack never calibrated rates no answer"
        );

        assert_eq!(results[1]["id"], "row-2");
        assert_eq!(results[1]["status"], 400);
        assert_eq!(results[1]["error"]["error_code"], "invalid_query");
        assert_eq!(
            results[1]["error"]["request_id"].as_str(),
            reply.request_id().as_deref()
        );
        assert!(results[1].get("response").is_none());

        assert!(results[2].get("id").is_none());
        assert_eq!(results[2]["status"], 200);
        assert_eq!(results[2]["response"]["features"], serde_json::json!([]));

        assert!(results[3].get("id").is_none());
        assert_eq!(results[3]["status"], 400);
        assert_eq!(results[3]["error"]["invalid_params"][0]["name"], "id");

        assert_eq!(results[4]["error"]["error_code"], "limit_too_large");
        assert_eq!(results[5]["error"]["invalid_params"][0]["name"], "address");

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
    }

    #[tokio::test]
    async fn batch_item_with_a_repeated_field_fails_as_get_does() {
        let jobs = TempDir::new("batch-repeated");
        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);

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
        assert_eq!(get.status, StatusCode::BAD_REQUEST);
        assert_eq!(get.json()["error_code"], "malformed_parameter");
        let result = &batch.json()["results"][0];
        assert_eq!(result["status"], get.status.as_u16());
        assert_eq!(result["error"]["error_code"], get.json()["error_code"]);
    }

    #[tokio::test]
    async fn batch_is_refused_when_every_slot_is_taken() {
        let jobs = TempDir::new("batch-busy");
        let state = king_state(&jobs, Limits::DEFAULT);
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
    }

    #[tokio::test]
    async fn batch_rejects_a_bad_request_whole() {
        let jobs = TempDir::new("batch-request-errors");
        let limits =
            limits_with(|limits| limits.max_batch_items = NonZeroUsize::new(3).expect("3"));
        let router = build_router(king_state(&jobs, limits), None, None);
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

        let mut declared = batch_request("POST", "/geocode", "[]");
        declared.headers_mut().insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from(limits.max_batch_body_bytes() + 1),
        );
        let reply = send(router.clone(), declared).await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error(&reply).as_deref(), Some("body_too_large"));

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
    }

    #[tokio::test]
    async fn batchable_routes_take_query_and_post() {
        let jobs = TempDir::new("batch-methods");
        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);

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
        let reply = send(router.clone(), request("QUERY", "/autocomplete")).await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(header_value(&reply, "allow"), Some("GET"));
        assert!(header_value(&reply, "accept-query").is_none());
    }

    #[derive(Debug, Deserialize)]
    struct PanicParams {
        q: String,
    }

    fn panic_on_boom(state: &AppState, query: String) -> Result<ForwardCollection> {
        assert_ne!(query, "boom", "label {} out of range", 7);
        api::forward_collection(&state.pack, query, Vec::new())
    }

    #[tokio::test]
    async fn batch_item_panic_fails_only_that_item() {
        let jobs = TempDir::new("batch-panic");
        let log = ErrorLog::capture();
        let mut state = king_state(&jobs, Limits::DEFAULT);
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
    }

    #[test]
    fn parses_the_country_filter() {
        let parse = |value: Option<&str>| {
            let param = serde_json::from_value::<Param>(serde_json::json!(value)).expect("param");
            parse_countries(&param).map_err(|_| ())
        };

        assert_eq!(parse(None), Ok(Vec::new()));
        assert_eq!(parse(Some("")), Ok(Vec::new()));
        assert_eq!(parse(Some("  ")), Ok(Vec::new()));

        assert_eq!(parse(Some("nz")), Ok(vec!["NZ".to_string()]));
        assert_eq!(
            parse(Some(" nz , AU ")),
            Ok(vec!["NZ".to_string(), "AU".to_string()])
        );
        assert_eq!(parse(Some("nz,NZ")), Ok(vec!["NZ".to_string()]));
        assert_eq!(parse(Some("NZ,,AU")).map(|v| v.len()), Ok(2));

        for bad in ["Canada", "C", "CAN", "1A", "C-A"] {
            assert!(parse(Some(bad)).is_err(), "{bad:?} should be rejected");
        }
    }

    #[tokio::test]
    async fn a_lookup_past_its_timeout_stops_with_a_503() {
        let jobs = TempDir::new("lookup-timeout");
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
            .with_state(king_state(&jobs, limits));

        let reply = send(router, request("GET", "/slow")).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(reply.content_type(), "application/problem+json");
        assert_eq!(reply.json()["error_code"], "timeout");
        assert_eq!(
            reply.json()["detail"],
            "the lookup took longer than the server allows, 1 ms"
        );
        assert!(header_value(&reply, "retry-after").is_none());
    }

    #[tokio::test]
    async fn a_lookup_is_refused_while_every_lookup_slot_is_taken() {
        let jobs = TempDir::new("lookup-busy");
        let limits = limits_with(|limits| limits.max_concurrent_lookups = Some(NonZeroUsize::MIN));
        let state = king_state(&jobs, limits);
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
    }

    fn slow_when_asked(state: &AppState, query: String) -> Result<ForwardCollection> {
        if query == "slow" {
            std::thread::sleep(Duration::from_millis(20));
            stop::check()?;
        }
        api::forward_collection(&state.pack, query, Vec::new())
    }

    #[tokio::test]
    async fn a_batch_item_past_its_timeout_fails_alone() {
        let jobs = TempDir::new("batch-timeout");
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
            .with_state(king_state(&jobs, limits));

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
    }

    #[tokio::test]
    async fn a_job_stops_reading_at_the_row_cap() {
        let jobs = TempDir::new("jobs-row-cap");
        let limits = limits_with(|limits| limits.max_job_rows = NonZeroUsize::new(2).expect("2"));
        let router = build_router(king_state(&jobs, limits), None, None);

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
    }

    #[tokio::test]
    async fn a_job_upload_over_the_limit_is_refused() {
        let jobs = TempDir::new("jobs-upload-cap");
        let limits = limits_with(|limits| limits.max_job_upload_mib = Some(NonZeroU64::MIN));
        let router = build_router(king_state(&jobs, limits), None, None);

        let mut declared = upload("/jobs", "text/csv", "street\n10 King Street\n");
        declared.headers_mut().insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from((1u64 << 20) + 1),
        );
        let reply = send(router, declared).await;
        assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(reply.json()["error_code"], "body_too_large");
    }

    #[tokio::test]
    async fn a_full_job_store_refuses_new_jobs() {
        let jobs = TempDir::new("jobs-store-full");
        std::fs::File::create(jobs.join("kept.results"))
            .expect("results")
            .set_len(1 << 30)
            .expect("length");
        std::fs::write(
            jobs.join("kept.json"),
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
        let router = build_router(king_state(&jobs, limits), None, None);

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
    }

    #[tokio::test]
    async fn the_served_spec_states_the_servers_limits() {
        let jobs = TempDir::new("spec-limits");
        let limits =
            limits_with(|limits| limits.max_batch_items = NonZeroUsize::new(7).expect("7"));
        let router = build_router(king_state(&jobs, limits), None, None);

        let spec = send(router, request("GET", "/openapi.json")).await.json();
        for path in ["/geocode", "/reverse"] {
            assert_eq!(
                spec["paths"][path]["post"]["requestBody"]["content"]["application/json"]["schema"]
                    ["maxItems"],
                7,
                "{path}"
            );
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
        let jobs = TempDir::new("shutdown-drain");
        let state = king_state(&jobs, Limits::DEFAULT);
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
    }

    #[tokio::test]
    async fn shutdown_gives_up_on_a_stuck_request_after_its_timeout() {
        use std::io::Write;
        let jobs = TempDir::new("shutdown-timeout");
        let state = king_state(&jobs, Limits::DEFAULT);
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
    }

    #[tokio::test]
    async fn shutdown_cancels_a_running_job_and_stops_at_once() {
        let jobs = TempDir::new("shutdown-job");
        let state = king_state(&jobs, Limits::DEFAULT);
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
            jobs.join(format!("{}.json", stopped["id"].as_str().expect("id"))),
        )
        .expect("record on disk");
        assert!(record.contains("\"cancelled\""), "{record}");

        drop(upload);
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum Sent {
        Missing,
        Empty,
        Filled,
    }

    type Sents = BTreeMap<&'static str, Sent>;

    #[derive(Debug, PartialEq)]
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

    struct Rules {
        route: &'static str,
        fields: &'static [&'static str],
        fixed: &'static [(&'static str, &'static str)],
        filled: &'static [(&'static str, &'static str)],
        outcome_classes: &'static [&'static [&'static str]],
        empty_is_missing: &'static [&'static str],
        batchable: bool,
        expect: fn(&Sents) -> Expect,
    }

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
                    ("lang", "fr,de"),
                ],
                outcome_classes: &[&["q"], &["address"], &["locality", "region", "postcode"]],
                empty_is_missing: &[
                    "focus", "bbox", "locality", "region", "postcode", "layer", "country", "lang",
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
                    ("lang", "fr,de"),
                ],
                outcome_classes: &[&["q"], &["ids"]],
                empty_is_missing: &["country", "focus", "bbox", "lang"],
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
                    ("lang", "fr,de"),
                ],
                outcome_classes: &[&["q"]],
                empty_is_missing: &["q", "country", "layer", "focus", "bbox", "lang"],
                expect: |_| Expect::Answer,
            },
            Rules {
                route: "/reverse",
                batchable: true,
                fields: field_names::<ReverseParams>(),
                fixed: &[
                    ("lon", "-79"),
                    ("lat", "43"),
                    ("limit", "0"),
                    ("radius", "150"),
                ],
                filled: &[("layer", "address"), ("lang", "fr,de")],
                outcome_classes: &[&["layer"]],
                empty_is_missing: &["layer", "lang"],
                expect: |_| Expect::Answer,
            },
        ]
    }

    #[tokio::test]
    async fn every_text_parameter_missing_empty_or_filled_answers_by_the_rules() {
        let jobs = TempDir::new("param-matrix");
        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);
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

            let modes = rules.outcome_classes.iter().flat_map(|class| class.iter());
            assert!(
                modes
                    .clone()
                    .all(|mode| rules.filled.iter().any(|(name, _)| name == mode)),
                "{}: a mode is a text parameter",
                rules.route
            );
            let filters = rules
                .filled
                .iter()
                .map(|(name, _)| *name)
                .filter(|name| !modes.clone().any(|mode| mode == name))
                .collect::<Vec<_>>();
            let unsent = filters
                .iter()
                .map(|name| (*name, Sent::Missing))
                .collect::<Sents>();
            let mut combos = vec![unsent];
            for class in rules.outcome_classes {
                let options = std::iter::once(None)
                    .chain(class.iter().flat_map(|member| {
                        [Sent::Empty, Sent::Filled].map(|state| Some((*member, state)))
                    }))
                    .collect::<Vec<_>>();
                combos = combos
                    .iter()
                    .flat_map(|combo| {
                        options.iter().map(move |option| {
                            let mut sent = combo.clone();
                            for member in *class {
                                let state = match option {
                                    Some((name, state)) if name == member => *state,
                                    _ => Sent::Missing,
                                };
                                sent.insert(member, state);
                            }
                            sent
                        })
                    })
                    .collect();
            }
            let mut outcomes = Vec::<(Expect, &Sents)>::new();
            for combo in &combos {
                let expect = (rules.expect)(combo);
                match outcomes.iter_mut().find(|(seen, _)| *seen == expect) {
                    Some(outcome) => outcome.1 = combo,
                    None => outcomes.push((expect, combo)),
                }
            }
            let mut cells = combos.clone();
            for (_, combo) in &outcomes {
                for filter in &filters {
                    for state in [Sent::Empty, Sent::Filled] {
                        let mut sent = (*combo).clone();
                        sent.insert(filter, state);
                        cells.push(sent);
                    }
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
    }

    #[tokio::test]
    async fn ids_past_the_limit_are_refused_however_many() {
        let jobs = TempDir::new("param-ids");
        let limits =
            limits_with(|limits| limits.max_batch_items = NonZeroUsize::new(7).expect("7"));
        let router = build_router(king_state(&jobs, limits), None, None);
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
        let huge = format!("{}not-an-id", "osm:node:1,".repeat(1_000_000));
        let refused = place_ids(&huge, 7).unwrap_err();
        assert_eq!(refused.error_code().as_str(), "limit_too_large");
        assert_eq!(refused.invalid_params()[0].reason, "at most 7 ids");
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

        let jobs = TempDir::new("compress");
        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);
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

        let tiny = send(
            router,
            with_headers("GET", "/healthz", &[("accept-encoding", "gzip")]),
        )
        .await;
        assert!(tiny.headers.get(header::CONTENT_ENCODING).is_none());
    }

    #[tokio::test]
    async fn compressed_jobs_still_stream_while_uploading() {
        use futures_util::{StreamExt, stream};
        use std::convert::Infallible;
        use std::io::Read;

        let jobs = TempDir::new("compress-stream");
        let router = build_router(king_state(&jobs, Limits::DEFAULT), None, None);

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
