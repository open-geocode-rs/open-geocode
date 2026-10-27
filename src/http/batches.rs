//! `/batches`: a CSV or JSONL file of lookups, answered while it uploads.
//!
//! Rows are looked up as they arrive and their results streamed back in the
//! same response, so nothing waits for the whole file. The results are also
//! written to disk as they are made: `GET /batches/{id}/results` streams
//! them again, live while the batch still runs, to a client that lost its
//! connection or asked to be answered at once (`Prefer: respond-async`).
//!
//! Each row is the item a `QUERY /geocode` or `QUERY /reverse` would carry,
//! read by the same code, so its answer is the one its `GET` returns.

use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{Body, Bytes},
    extract::{Path as UrlPath, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use csv::{ByteRecord, ReaderBuilder, WriterBuilder};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::{
    sync::{OwnedSemaphorePermit, mpsc as channel, oneshot, watch},
    task,
};

use crate::{
    api::{self, ForwardCollection, ReverseCollection},
    batch::{audit_columns, delimiter_of, text},
    http::{
        batch::{self, Item, Lookup, Prepared, RawItem},
        bounds::{BATCH_RETENTION, MAX_BATCH_ROWS, MAX_BATCH_UPLOAD_BYTES, UPLOAD_STALL_SECONDS},
        openapi::{self, PROBLEM_JSON},
        problem::{InvalidParam, Problem},
        request_id::RequestId,
    },
    runtime::{AppState, GEOCODE, REVERSE},
};

/// The media types a batch file is read and written as.
const CSV: &str = "text/csv";
const JSONL: &str = "application/jsonl";
/// What JSONL went by before it had a name of its own; still common.
const NDJSON: &str = "application/x-ndjson";

/// Rows read before they are looked up. They are looked up in Pack order
/// within it, so a window is the reach of that ordering.
const WINDOW_ROWS: usize = 256;

/// Lookups one batch has queued or running, per pool thread. Enough to keep
/// every core busy without one batch filling the pool's queue ahead of the
/// others.
const IN_FLIGHT_PER_THREAD: usize = 2;

/// Answers kept for rows that repeat a query. Cleared when full: a repeat
/// then costs one more lookup, never wrong output.
const REPEAT_CACHE_ENTRIES: usize = 100_000;

/// Bytes read from the results file per streamed chunk.
const STREAM_CHUNK_BYTES: usize = 64 << 10;

/// How much of the upload is read to tell its CSV delimiter.
const SNIFF_BYTES: usize = 64 << 10;

const BYTE_ORDER_MARK: &[u8] = "\u{feff}".as_bytes();

/// The lookup every row of a batch runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = BatchEndpoint)]
pub(crate) enum Endpoint {
    Geocode,
    Reverse,
}

impl Endpoint {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().trim_start_matches('/') {
            "geocode" => Some(Self::Geocode),
            "reverse" => Some(Self::Reverse),
            _ => None,
        }
    }
}

/// A batch file's format: CSV, or one JSON object a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = BatchFormat)]
pub(crate) enum Format {
    Csv,
    Jsonl,
}

impl Format {
    fn media_type(self) -> &'static str {
        match self {
            Self::Csv => CSV,
            Self::Jsonl => JSONL,
        }
    }

    fn of(media_type: &str) -> Option<Self> {
        let media_type = media_type.trim();
        if media_type.eq_ignore_ascii_case(CSV) {
            Some(Self::Csv)
        } else if media_type.eq_ignore_ascii_case(JSONL) || media_type.eq_ignore_ascii_case(NDJSON)
        {
            Some(Self::Jsonl)
        } else {
            None
        }
    }

    /// The first format `Accept` names that a batch can be written as.
    fn accepted(headers: &HeaderMap) -> Option<Self> {
        headers
            .get(header::ACCEPT)?
            .to_str()
            .ok()?
            .split(',')
            .find_map(|range| Self::of(range.split(';').next().unwrap_or_default()))
    }
}

/// Where a batch is: running, or stopped and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = BatchState)]
pub(crate) enum Status {
    Running,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) struct Summary {
    pub(crate) total: usize,
    pub(crate) succeeded: usize,
    pub(crate) failed: usize,
}

/// A batch as `GET /batches/{id}` shows it, and as it is kept on disk.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub(crate) struct BatchStatus {
    pub(crate) id: String,
    pub(crate) endpoint: Endpoint,
    pub(crate) status: Status,
    pub(crate) input_format: Format,
    pub(crate) output_format: Format,
    /// Rows answered so far.
    #[schema(value_type = openapi::BatchSummary)]
    pub(crate) summary: Summary,
    /// Unix seconds.
    pub(crate) created_at: u64,
    /// Unix seconds, once the batch stops.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) completed_at: Option<u64>,
    /// When the results are deleted. Set once the batch stops.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<u64>,
    pub(crate) results_url: String,
    /// Why a failed batch stopped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

/// Bytes of results on disk, and whether more will come.
#[derive(Debug, Clone, Copy, Default)]
struct Progress {
    bytes: u64,
    done: bool,
}

/// One batch, running or kept.
pub(crate) struct Batch {
    status: Mutex<BatchStatus>,
    progress: watch::Sender<Progress>,
    cancel: AtomicBool,
    /// Deleted while running: its files go when it stops.
    deleted: AtomicBool,
    results: PathBuf,
    record: PathBuf,
}

impl Batch {
    fn snapshot(&self) -> BatchStatus {
        self.status.lock().expect("batch status").clone()
    }

    fn save(&self) {
        let status = self.snapshot();
        // A status that fails to save leaves the batch served from memory;
        // only a restart loses it.
        if let Ok(json) = serde_json::to_vec(&status) {
            let _ = fs::write(&self.record, json);
        }
    }

    fn remove_files(&self) {
        let _ = fs::remove_file(&self.record);
        let _ = fs::remove_file(&self.results);
    }
}

/// Every batch this server keeps, and the directory that holds their files.
pub(crate) struct BatchStore {
    dir: PathBuf,
    batches: Mutex<HashMap<String, Arc<Batch>>>,
}

impl BatchStore {
    /// Open the store in `dir`, keeping the batches a past run left there.
    /// One that was running then is failed: its upload went with the server.
    pub(crate) fn open(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let store = Self {
            dir,
            batches: Mutex::new(HashMap::new()),
        };
        let now = unix_now();
        let mut kept = Vec::new();
        for entry in fs::read_dir(&store.dir)?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(mut status) = fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<BatchStatus>(&bytes).ok())
            else {
                let _ = fs::remove_file(&path);
                continue;
            };
            if status.status == Status::Running {
                status.status = Status::Failed;
                status.error = Some("the server restarted while the batch ran".to_string());
                status.completed_at = Some(now);
                status.expires_at = Some(now + BATCH_RETENTION.as_secs());
            }
            let batch = store.batch(status);
            if batch.snapshot().expires_at.is_some_and(|at| at <= now) {
                batch.remove_files();
                continue;
            }
            batch.save();
            let bytes = fs::metadata(&batch.results).map_or(0, |meta| meta.len());
            batch.progress.send_replace(Progress { bytes, done: true });
            kept.push(batch);
        }
        let mut batches = store.batches.lock().expect("batches");
        for batch in kept {
            let id = batch.snapshot().id;
            batches.insert(id, batch);
        }
        drop(batches);
        store.remove_orphans();
        Ok(store)
    }

    fn batch(&self, status: BatchStatus) -> Arc<Batch> {
        let results = self.dir.join(format!("{}.results", status.id));
        let record = self.dir.join(format!("{}.json", status.id));
        Arc::new(Batch {
            status: Mutex::new(status),
            progress: watch::Sender::new(Progress::default()),
            cancel: AtomicBool::new(false),
            deleted: AtomicBool::new(false),
            results,
            record,
        })
    }

    /// Results files no status names, left by a crash between the two.
    fn remove_orphans(&self) {
        let batches = self.batches.lock().expect("batches");
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let orphan = path.extension().and_then(|ext| ext.to_str()) == Some("results")
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_none_or(|id| !batches.contains_key(id));
            if orphan {
                let _ = fs::remove_file(path);
            }
        }
    }

    fn get(&self, id: &str) -> Option<Arc<Batch>> {
        let batch = self.batches.lock().expect("batches").get(id).cloned()?;
        if batch
            .snapshot()
            .expires_at
            .is_some_and(|at| at <= unix_now())
        {
            self.remove(id);
            return None;
        }
        Some(batch)
    }

    fn insert(&self, batch: Arc<Batch>) {
        let id = batch.snapshot().id;
        self.batches.lock().expect("batches").insert(id, batch);
    }

    fn remove(&self, id: &str) -> Option<Arc<Batch>> {
        let batch = self.batches.lock().expect("batches").remove(id)?;
        batch.deleted.store(true, Ordering::Relaxed);
        batch.cancel.store(true, Ordering::Relaxed);
        if batch.progress.borrow().done {
            batch.remove_files();
        }
        Some(batch)
    }

    /// Delete every batch past its retention.
    pub(crate) fn sweep(&self) {
        let now = unix_now();
        let expired = self
            .batches
            .lock()
            .expect("batches")
            .iter()
            .filter(|(_, batch)| batch.snapshot().expires_at.is_some_and(|at| at <= now))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in expired {
            self.remove(&id);
        }
    }

    /// Cancel every running batch and wait, up to `wait`, for each to record
    /// that it stopped. Rows already answered stay in its results. One left
    /// `running` on disk would only be failed by the next start, with no
    /// sign of how far it got.
    pub(crate) async fn stop_running(&self, wait: Duration) {
        let running = self
            .batches
            .lock()
            .expect("batches")
            .values()
            .filter(|batch| !batch.progress.borrow().done)
            .cloned()
            .collect::<Vec<_>>();
        for batch in &running {
            batch.cancel.store(true, Ordering::Relaxed);
        }
        let deadline = tokio::time::Instant::now() + wait;
        for batch in running {
            let _ = tokio::time::timeout_at(
                deadline,
                batch
                    .progress
                    .subscribe()
                    .wait_for(|progress| progress.done),
            )
            .await;
        }
    }
}

/// What a row's answer adds to a CSV row: `geocode batch`'s audit columns.
pub(crate) trait Answer: Serialize + Clone + Send + Sync + 'static {
    /// The row's status (`resolved`, `fallback` or `unresolved`), the query
    /// it was looked up as, and the result columns.
    fn columns(&self) -> (&'static str, String, Vec<String>);
}

impl Answer for ForwardCollection {
    fn columns(&self) -> (&'static str, String, Vec<String>) {
        let feature = self.features.first();
        let status = match feature {
            Some(feature) if feature.properties.fallback => "fallback",
            Some(_) => "resolved",
            None => "unresolved",
        };
        let values = api::flat_columns(feature)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        (status, self.geocoding.query.clone(), values)
    }
}

impl Answer for ReverseCollection {
    fn columns(&self) -> (&'static str, String, Vec<String>) {
        let feature = self.features.first();
        let status = if feature.is_some() {
            "resolved"
        } else {
            "unresolved"
        };
        let point = self.geocoding.point;
        let values = api::reverse_flat_columns(feature)
            .into_iter()
            .map(|(_, value)| value)
            .collect();
        (status, format!("{},{}", point.lon, point.lat), values)
    }
}

/// The columns of a CSV upload that a row's lookup reads, by place.
#[derive(Debug, Default)]
struct Columns {
    /// Joined with `, ` into one `address`.
    address: Vec<usize>,
    locality: Option<usize>,
    region: Option<usize>,
    postcode: Option<usize>,
    lat: Option<usize>,
    lon: Option<usize>,
}

/// The query-string parameters that name CSV columns rather than apply to
/// every row.
const COLUMN_PARAMETERS: [&str; 6] = ["address", "locality", "region", "postcode", "lat", "lon"];

/// Header names read as each column when the request names none, compared
/// without regard to case, spaces, hyphens or underscores.
const ADDRESS_HEADERS: [&str; 8] = [
    "address",
    "streetaddress",
    "address1",
    "addressline1",
    "street",
    "fulladdress",
    "addr",
    "location",
];
const LOCALITY_HEADERS: [&str; 5] = ["locality", "city", "town", "municipality", "village"];
const REGION_HEADERS: [&str; 4] = ["region", "state", "province", "stateprovince"];
const POSTCODE_HEADERS: [&str; 6] = [
    "postcode",
    "postalcode",
    "zip",
    "zipcode",
    "postal",
    "postcodezip",
];
const LAT_HEADERS: [&str; 2] = ["lat", "latitude"];
const LON_HEADERS: [&str; 4] = ["lon", "lng", "long", "longitude"];

fn header_key(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

impl Columns {
    /// The columns the request names, or failing that the ones the header
    /// row's names suggest, and the endpoint they mean.
    fn resolve(
        headers: &[String],
        named: &[(String, String)],
        endpoint: Option<Endpoint>,
    ) -> Result<(Endpoint, Self), Problem> {
        let keys = headers
            .iter()
            .map(|name| header_key(name))
            .collect::<Vec<_>>();
        let find = |name: &str| -> Result<usize, Problem> {
            let wanted = header_key(name);
            keys.iter().position(|key| *key == wanted).ok_or_else(|| {
                Problem::malformed_parameter(format!(
                    "the CSV has no column {name:?}; its columns are {}",
                    headers.join(", ")
                ))
                .with_invalid_params(vec![InvalidParam::new(name, "no such column")])
            })
        };
        let guess = |names: &[&str]| keys.iter().position(|key| names.contains(&key.as_str()));

        let mut columns = Self::default();
        for (parameter, value) in named {
            let slot = match parameter.as_str() {
                "address" => {
                    for name in value.split(',').filter(|name| !name.trim().is_empty()) {
                        columns.address.push(find(name.trim())?);
                    }
                    continue;
                }
                "locality" => &mut columns.locality,
                "region" => &mut columns.region,
                "postcode" => &mut columns.postcode,
                "lat" => &mut columns.lat,
                _ => &mut columns.lon,
            };
            *slot = Some(find(value)?);
        }
        if named.is_empty() {
            columns.address.extend(guess(&ADDRESS_HEADERS));
            columns.locality = guess(&LOCALITY_HEADERS);
            columns.region = guess(&REGION_HEADERS);
            columns.postcode = guess(&POSTCODE_HEADERS);
            columns.lat = guess(&LAT_HEADERS);
            columns.lon = guess(&LON_HEADERS);
        }

        let has_point = columns.lat.is_some() && columns.lon.is_some();
        let endpoint = match endpoint {
            Some(endpoint) => endpoint,
            None if !columns.address.is_empty() => Endpoint::Geocode,
            None if has_point => Endpoint::Reverse,
            None => {
                return Err(Problem::malformed_parameter(format!(
                    "no address or lat and lon columns found; name them with \
                     ?address=, ?lat= and ?lon=. The CSV's columns are {}",
                    headers.join(", ")
                )));
            }
        };
        match endpoint {
            Endpoint::Geocode if columns.address.is_empty() => Err(Problem::malformed_parameter(
                "a geocode batch needs an address column; name it with ?address=",
            )),
            Endpoint::Reverse if !has_point => Err(Problem::malformed_parameter(
                "a reverse batch needs lat and lon columns; name them with ?lat= and ?lon=",
            )),
            _ => Ok((endpoint, columns)),
        }
    }

    /// The row as a batch item: the fields its `GET` would carry.
    fn item(&self, endpoint: Endpoint, row: &ByteRecord) -> RawItem {
        let cell = |index: Option<usize>| -> Option<String> {
            let value = text(row.get(index?)?);
            let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
            (!value.is_empty()).then_some(value)
        };
        let mut fields = Vec::new();
        let mut push = |name: &str, value: Option<String>| {
            if let Some(value) = value {
                fields.push((name.to_string(), Value::String(value)));
            }
        };
        match endpoint {
            Endpoint::Geocode => {
                let address = self
                    .address
                    .iter()
                    .filter_map(|index| cell(Some(*index)))
                    .collect::<Vec<_>>();
                push("address", (!address.is_empty()).then(|| address.join(", ")));
                push("locality", cell(self.locality));
                push("region", cell(self.region));
                push("postcode", cell(self.postcode));
            }
            Endpoint::Reverse => {
                push("lat", cell(self.lat));
                push("lon", cell(self.lon));
            }
        }
        RawItem::Fields(fields)
    }
}

/// The upload as a [`Read`], fed chunk by chunk from the request body.
struct Upload {
    chunks: channel::Receiver<Result<Bytes, String>>,
    chunk: Bytes,
}

impl Upload {
    /// At least `bytes` of what comes next, or all of it if less, left in
    /// place to be read again.
    fn peek(&mut self, bytes: usize) -> io::Result<Vec<u8>> {
        let mut head = self.chunk.to_vec();
        while head.len() < bytes {
            match self.chunks.blocking_recv() {
                Some(Ok(chunk)) => head.extend_from_slice(&chunk),
                Some(Err(error)) => return Err(io::Error::other(error)),
                None => break,
            }
        }
        self.chunk = Bytes::from(head.clone());
        Ok(head)
    }

    fn skip(&mut self, bytes: usize) {
        self.chunk = self.chunk.slice(bytes.min(self.chunk.len())..);
    }
}

impl Read for Upload {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.chunk.is_empty() {
            match self.chunks.blocking_recv() {
                Some(Ok(chunk)) => self.chunk = chunk,
                Some(Err(error)) => return Err(io::Error::other(error)),
                None => return Ok(0),
            }
        }
        let length = buf.len().min(self.chunk.len());
        buf[..length].copy_from_slice(&self.chunk[..length]);
        self.chunk = self.chunk.slice(length..);
        Ok(length)
    }
}

/// One input row, as the output writes it back.
struct RowIn {
    index: usize,
    id: Option<String>,
    /// The CSV row's own cells; none for JSONL.
    cells: Option<ByteRecord>,
}

/// Rows answered alike: one row, or every row of a window that asked the
/// same query.
struct Answered<A> {
    rows: Vec<RowIn>,
    item: Item<A>,
}

/// How the reader ends, for the batch's status.
enum Ending {
    Read,
    Cancelled,
    Failed(String),
}

/// The CSV header and the columns its rows are read from, or the JSONL
/// format, known before the response starts.
enum Layout {
    Csv {
        headers: ByteRecord,
        columns: Columns,
        delimiter: u8,
        endpoint: Endpoint,
    },
    Jsonl,
}

/// The request's parameters, split: which lookup, which columns, and what
/// applies to every row.
struct Plan {
    endpoint: Option<Endpoint>,
    named: Vec<(String, String)>,
    shared: Vec<(String, String)>,
}

impl Plan {
    fn read(query: Option<&str>, input: Format) -> Result<Self, Problem> {
        let mut plan = Self {
            endpoint: None,
            named: Vec::new(),
            shared: Vec::new(),
        };
        for (name, value) in batch::shared_parameters(query)? {
            if name == "endpoint" {
                plan.endpoint = Some(Endpoint::parse(&value).ok_or_else(|| {
                    Problem::malformed_parameter("endpoint must be geocode or reverse")
                        .with_invalid_params(vec![InvalidParam::new(
                            "endpoint",
                            "must be geocode or reverse",
                        )])
                })?);
            } else if input == Format::Csv && COLUMN_PARAMETERS.contains(&name.as_str()) {
                plan.named.push((name, value));
            } else {
                plan.shared.push((name, value));
            }
        }
        Ok(plan)
    }
}

/// Geocode a file
///
/// Geocodes every row of a CSV or JSONL upload. Rows are looked up while the
/// file uploads, and their results stream back in this response: CSV in the
/// order rows were sent, JSONL in the order they finish, each line with its
/// `row`. The results are kept for 72 hours at `Content-Location`.
///
/// - CSV: name the columns with `address` (several, comma-separated, are
///   joined), `locality`, `region` and `postcode`, or `lat` and `lon` for
///   reverse. Without them, the header row's names are matched to common
///   ones (`street`, `city`, `zip`, `latitude` and so on). The output is the
///   input with `geocode batch`'s `geocode_*` columns added.
/// - JSONL: each line is a `QUERY /geocode` or `QUERY /reverse` item, and
///   `endpoint` is required.
///
/// Every other query-string parameter, such as `country`, applies to every
/// row. Each row's answer is the one its `GET` returns. Send
/// `Prefer: respond-async` to be answered with the batch's status once the
/// upload is in, instead of with the results.
#[utoipa::path(
    post,
    path = "/batches",
    tag = "batches",
    params(
        ("endpoint" = Option<Endpoint>, Query,
         description = "The lookup every row runs. Required for JSONL; for CSV, \
                        inferred from the columns."),
        ("address" = Option<String>, Query,
         description = "CSV only: the column, or comma-separated columns, holding \
                        the address.",
         example = "street"),
        ("locality" = Option<String>, Query, description = "CSV only: the town column."),
        ("region" = Option<String>, Query, description = "CSV only: the region column."),
        ("postcode" = Option<String>, Query, description = "CSV only: the postcode column."),
        ("lat" = Option<String>, Query, description = "CSV only: the latitude column."),
        ("lon" = Option<String>, Query, description = "CSV only: the longitude column."),
        ("country" = Option<String>, Query,
         description = "Comma-separated ISO 3166-1 alpha-2 codes every row is \
                        limited to.",
         example = "CA"),
        ("Prefer" = Option<String>, Header,
         description = "`respond-async` to be answered with the batch's status once \
                        the upload is in.",
         example = "respond-async"),
        ("Accept" = Option<String>, Header,
         description = "`text/csv` or `application/jsonl`. Defaults to the upload's \
                        own format."),
    ),
    request_body(
        description = "The rows: a CSV with a header row, or one JSON object a line.",
        content(
            (String = "text/csv"),
            (String = "application/jsonl"),
        ),
    ),
    responses(
        (status = 201, description = "The results, streamed as rows finish.",
         headers(
             ("Location" = String, description = "The batch."),
             ("Content-Location" = String, description = "The results, kept for 72 hours."),
         ),
         content(
             (String = "text/csv"),
             (String = "application/jsonl"),
         )),
        (status = 202, description = "With `Prefer: respond-async`: the upload is in \
                                      and the batch runs.",
         body = BatchStatus,
         headers(("Location" = String, description = "The batch."))),
        (status = 400, description = "The CSV has no header row or no column to look \
                                      up, or a parameter is malformed.",
         body = openapi::Problem, content_type = PROBLEM_JSON),
        (status = 413, description = "The declared body is over 1 GiB.",
         body = openapi::Problem, content_type = PROBLEM_JSON),
        (status = 415, description = "The body is not `text/csv` or `application/jsonl`.",
         body = openapi::Problem, content_type = PROBLEM_JSON),
        (status = 503, description = "Every batch slot is taken. Retry after the \
                                      `Retry-After` header's seconds.",
         body = openapi::Problem, content_type = PROBLEM_JSON),
    ),
)]
pub(crate) async fn create(State(state): State<AppState>, request: Request) -> Response {
    match start(state, request).await {
        Ok(response) => response,
        Err(problem) => problem.into_response(),
    }
}

async fn start(state: AppState, request: Request) -> Result<Response, Problem> {
    let started = Instant::now();
    let input = batch::media_type(request.headers())
        .and_then(Format::of)
        .ok_or_else(|| Problem::unsupported_media_type("text/csv or application/jsonl"))?;
    let output = Format::accepted(request.headers()).unwrap_or(input);
    let respond_async = prefers_async(request.headers());
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > MAX_BATCH_UPLOAD_BYTES) {
        return Err(Problem::body_too_large(MAX_BATCH_UPLOAD_BYTES as usize));
    }
    let plan = Plan::read(request.uri().query(), input)?;
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone());

    let Ok(permit) = Arc::clone(state.batch_slots()).try_acquire_owned() else {
        return Err(Problem::busy(1));
    };

    let (chunks_tx, chunks_rx) = channel::channel(32);
    let (uploaded_tx, uploaded_rx) = oneshot::channel();
    tokio::spawn(forward_upload(request.into_body(), chunks_tx, uploaded_tx));
    let mut upload = Upload {
        chunks: chunks_rx,
        chunk: Bytes::new(),
    };

    // The header row is read before answering, so a CSV the batch cannot
    // read is a 400 rather than a stream that stops.
    let (upload, layout, endpoint, plan) = task::spawn_blocking(move || {
        let (layout, endpoint) = read_layout(&mut upload, input, &plan)?;
        Ok::<_, Problem>((upload, layout, endpoint, plan))
    })
    .await??;

    let id = uuid::Uuid::new_v4().simple().to_string();
    let store = state.batches();
    let batch = store.batch(BatchStatus {
        id: id.clone(),
        endpoint,
        status: Status::Running,
        input_format: input,
        output_format: output,
        summary: Summary::default(),
        created_at: unix_now(),
        completed_at: None,
        expires_at: None,
        results_url: format!("/batches/{id}/results"),
        error: None,
    });
    let file = File::create(&batch.results).map_err(|error| Problem::internal(error.into()))?;
    batch.save();
    store.insert(Arc::clone(&batch));

    let run = Run {
        state: state.clone(),
        batch: Arc::clone(&batch),
        shared: plan.shared,
        output,
        request_id,
        started,
    };
    task::spawn_blocking(move || match endpoint {
        Endpoint::Geocode => run.go(GEOCODE, upload, layout, file, permit),
        Endpoint::Reverse => run.go(REVERSE, upload, layout, file, permit),
    });

    let location = HeaderValue::from_str(&format!("/batches/{id}"))
        .map_err(|error| Problem::internal(error.into()))?;
    if respond_async {
        let _ = uploaded_rx.await;
        let mut response = (StatusCode::ACCEPTED, axum::Json(batch.snapshot())).into_response();
        response.headers_mut().insert(header::LOCATION, location);
        return Ok(response);
    }
    let mut response = (
        StatusCode::CREATED,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(output.media_type()),
        )],
        tail(batch),
    )
        .into_response();
    let results = HeaderValue::from_str(&format!("/batches/{id}/results"))
        .map_err(|error| Problem::internal(error.into()))?;
    response.headers_mut().insert(header::LOCATION, location);
    response
        .headers_mut()
        .insert(header::CONTENT_LOCATION, results);
    Ok(response)
}

/// `Prefer: respond-async` (RFC 7240): answer once the upload is in, with
/// the batch's status, instead of streaming its results.
fn prefers_async(headers: &HeaderMap) -> bool {
    headers
        .get_all("prefer")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|preference| preference.trim().eq_ignore_ascii_case("respond-async"))
}

/// Read the CSV header, or nothing for JSONL, and settle the endpoint.
fn read_layout(
    upload: &mut Upload,
    input: Format,
    plan: &Plan,
) -> Result<(Layout, Endpoint), Problem> {
    let unreadable =
        |error: io::Error| Problem::malformed_body(format!("the upload failed: {error}"));
    if input == Format::Jsonl {
        let endpoint = plan.endpoint.ok_or_else(|| {
            Problem::malformed_parameter(
                "a JSONL batch names its lookup: ?endpoint=geocode or ?endpoint=reverse",
            )
        })?;
        return Ok((Layout::Jsonl, endpoint));
    }
    let head = upload.peek(SNIFF_BYTES).map_err(unreadable)?;
    let bom = head.starts_with(BYTE_ORDER_MARK);
    if bom {
        upload.skip(BYTE_ORDER_MARK.len());
    }
    let sample = if bom {
        &head[BYTE_ORDER_MARK.len()..]
    } else {
        &head[..]
    };
    let delimiter = delimiter_of(sample);
    let header_line = sample
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    let mut reader = ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .from_reader(header_line);
    let mut headers = ByteRecord::new();
    if !reader.read_byte_record(&mut headers).unwrap_or(false) || headers.is_empty() {
        return Err(Problem::malformed_body(
            "the CSV must start with a header row",
        ));
    }
    let names = headers
        .iter()
        .map(|name| text(name).trim().to_string())
        .collect::<Vec<_>>();
    let (endpoint, columns) = Columns::resolve(&names, &plan.named, plan.endpoint)?;
    let layout = Layout::Csv {
        headers,
        columns,
        delimiter,
        endpoint,
    };
    Ok((layout, endpoint))
}

/// Pass the request body to the reader, a chunk at a time, refusing one that
/// grows past its cap or stalls. Tells `uploaded` once it is all in.
async fn forward_upload(
    body: Body,
    chunks: channel::Sender<Result<Bytes, String>>,
    uploaded: oneshot::Sender<()>,
) {
    let mut stream = body.into_data_stream();
    let mut received = 0u64;
    let stall = Duration::from_secs(UPLOAD_STALL_SECONDS);
    loop {
        let next = match tokio::time::timeout(stall, stream.next()).await {
            Ok(next) => next,
            Err(_) => {
                let _ = chunks
                    .send(Err(format!("no data for {UPLOAD_STALL_SECONDS} s")))
                    .await;
                break;
            }
        };
        let chunk = match next {
            Some(Ok(chunk)) => chunk,
            Some(Err(error)) => {
                let _ = chunks
                    .send(Err(format!("the body broke off: {error}")))
                    .await;
                break;
            }
            None => break,
        };
        received += chunk.len() as u64;
        if received > MAX_BATCH_UPLOAD_BYTES {
            let _ = chunks
                .send(Err(format!(
                    "the upload is over {MAX_BATCH_UPLOAD_BYTES} bytes"
                )))
                .await;
            break;
        }
        // A reader that stopped (the batch was cancelled) ends the upload.
        if chunks.send(Ok(chunk)).await.is_err() {
            break;
        }
    }
    let _ = uploaded.send(());
}

/// One running batch's pipeline.
struct Run {
    state: AppState,
    batch: Arc<Batch>,
    shared: Vec<(String, String)>,
    output: Format,
    request_id: Option<String>,
    started: Instant,
}

impl Run {
    /// Read, look up and write the whole batch, then record how it ended.
    fn go<P, Q, A, K>(
        self,
        lookup: Lookup<P, Q, A, K>,
        upload: Upload,
        layout: Layout,
        file: File,
        permit: OwnedSemaphorePermit,
    ) where
        P: DeserializeOwned + 'static,
        Q: Send + 'static,
        A: Answer,
        K: Ord + 'static,
    {
        let (answers_tx, answers_rx) = mpsc::channel::<Answered<A>>();
        let width = match &layout {
            Layout::Csv { headers, .. } => Some(headers.clone()),
            Layout::Jsonl => None,
        };
        let writer = {
            let batch = Arc::clone(&self.batch);
            let state = self.state.clone();
            let output = self.output;
            let request_id = self.request_id.clone();
            let started = self.started;
            std::thread::Builder::new()
                .name("batch-writer".to_string())
                .spawn(move || {
                    write_results(
                        &batch,
                        &state,
                        output,
                        width,
                        file,
                        answers_rx,
                        request_id.as_deref(),
                        started,
                    )
                })
        };
        let writer = match writer {
            Ok(writer) => writer,
            Err(error) => {
                drop(permit);
                let ending = Ending::Failed(format!("could not start the writer: {error}"));
                return self.finish(ending, Ok(()));
            }
        };
        let ending = self.read(lookup, upload, layout, answers_tx);
        let written = writer
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("the writer panicked")));
        drop(permit);
        self.finish(ending, written);
    }

    /// Read rows a window at a time and hand each window's lookups to the
    /// pool, in Pack order, repeated queries once.
    fn read<P, Q, A, K>(
        &self,
        lookup: Lookup<P, Q, A, K>,
        upload: Upload,
        layout: Layout,
        answers: mpsc::Sender<Answered<A>>,
    ) -> Ending
    where
        P: DeserializeOwned + 'static,
        Q: Send + 'static,
        A: Answer,
        K: Ord + 'static,
    {
        let in_flight = self.state.batch_pool().current_num_threads() * IN_FLIGHT_PER_THREAD;
        let (tokens_tx, tokens_rx) = mpsc::sync_channel::<()>(in_flight);
        for _ in 0..in_flight {
            let _ = tokens_tx.send(());
        }
        let repeats = Arc::new(Mutex::new(HashMap::<String, Item<A>>::new()));
        let mut rows = Rows::new(upload, layout);
        let mut index = 0;
        loop {
            if self.batch.cancel.load(Ordering::Relaxed) {
                return Ending::Cancelled;
            }
            let mut window = Vec::with_capacity(WINDOW_ROWS);
            let ending = loop {
                if window.len() == WINDOW_ROWS {
                    break None;
                }
                if index == MAX_BATCH_ROWS {
                    break Some(Ending::Failed(format!(
                        "a batch holds at most {MAX_BATCH_ROWS} rows; the rest were not read"
                    )));
                }
                match rows.next() {
                    Ok(Some((cells, item))) => {
                        window.push((index, cells, item));
                        index += 1;
                    }
                    Ok(None) => break Some(Ending::Read),
                    Err(error) => break Some(Ending::Failed(error)),
                }
            };

            let mut groups = HashMap::<String, (K, Q, Vec<RowIn>)>::new();
            for (index, cells, item) in window {
                let prepared = match item {
                    Ok(item) => batch::prepare(lookup, &self.shared, item),
                    Err(problem) => Prepared::Failed { id: None, problem },
                };
                match prepared {
                    Prepared::Failed { id, problem } => {
                        let row = RowIn {
                            index,
                            id: id.clone(),
                            cells,
                        };
                        let _ = answers.send(Answered {
                            rows: vec![row],
                            item: Item::failed(id, problem),
                        });
                    }
                    Prepared::Ready { id, key, query } => {
                        let row = RowIn { index, id, cells };
                        if let Some(item) = repeats.lock().expect("repeats").get(&key).cloned() {
                            let _ = answers.send(Answered {
                                rows: vec![row],
                                item,
                            });
                        } else if let Some((_, _, rows)) = groups.get_mut(&key) {
                            rows.push(row);
                        } else {
                            groups.insert(key, ((lookup.place)(&query), query, vec![row]));
                        }
                    }
                }
            }
            let mut groups = groups.into_iter().collect::<Vec<_>>();
            groups.sort_by(|(_, (left, ..)), (_, (right, ..))| left.cmp(right));
            for (key, (_, query, rows)) in groups {
                // Waits while this batch already has its share queued, which
                // leaves the pool to other batches and holds memory flat.
                if tokens_rx.recv().is_err() {
                    break;
                }
                let state = self.state.clone();
                let answers = answers.clone();
                let tokens = tokens_tx.clone();
                let repeats = Arc::clone(&repeats);
                let batch = Arc::clone(&self.batch);
                self.state.batch_pool().spawn(move || {
                    if !batch.cancel.load(Ordering::Relaxed) {
                        let item = batch::look_up(&state, lookup, None, query);
                        let mut repeats = repeats.lock().expect("repeats");
                        if repeats.len() >= REPEAT_CACHE_ENTRIES {
                            repeats.clear();
                        }
                        repeats.insert(key, item.clone());
                        drop(repeats);
                        let _ = answers.send(Answered { rows, item });
                    }
                    let _ = tokens.send(());
                });
            }
            if let Some(ending) = ending {
                return ending;
            }
        }
    }

    fn finish(self, ending: Ending, written: io::Result<()>) {
        let now = unix_now();
        {
            let mut status = self.batch.status.lock().expect("batch status");
            // A cancelled batch whose upload then ends early (the server is
            // stopping and drops the connection) reads as a finished one.
            let ending = match ending {
                Ending::Read if self.batch.cancel.load(Ordering::Relaxed) => Ending::Cancelled,
                other => other,
            };
            let (state, error) = match (ending, written) {
                (_, Err(error)) => (
                    Status::Failed,
                    Some(format!("writing results failed: {error}")),
                ),
                (Ending::Read, Ok(())) => (Status::Completed, None),
                (Ending::Cancelled, Ok(())) => (Status::Cancelled, None),
                (Ending::Failed(error), Ok(())) => (Status::Failed, Some(error)),
            };
            status.status = state;
            status.error = error;
            status.completed_at = Some(now);
            status.expires_at = Some(now + BATCH_RETENTION.as_secs());
        }
        if self.batch.deleted.load(Ordering::Relaxed) {
            self.batch.remove_files();
        } else {
            self.batch.save();
        }
        self.batch
            .progress
            .send_modify(|progress| progress.done = true);
    }
}

/// The upload's rows, each as its cells (CSV only) and the item it asks.
struct Rows {
    csv: Option<(csv::Reader<Upload>, Columns, Endpoint)>,
    jsonl: Option<BufReader<Upload>>,
    line: String,
}

type Row = (Option<ByteRecord>, Result<RawItem, Problem>);

impl Rows {
    fn new(upload: Upload, layout: Layout) -> Self {
        match layout {
            Layout::Csv {
                columns,
                delimiter,
                endpoint,
                ..
            } => {
                // Reads the header row again, past it to the first row.
                let reader = ReaderBuilder::new()
                    .delimiter(delimiter)
                    .has_headers(true)
                    .flexible(true)
                    .from_reader(upload);
                Self {
                    csv: Some((reader, columns, endpoint)),
                    jsonl: None,
                    line: String::new(),
                }
            }
            Layout::Jsonl => Self {
                csv: None,
                jsonl: Some(BufReader::new(upload)),
                line: String::new(),
            },
        }
    }

    fn next(&mut self) -> Result<Option<Row>, String> {
        if let Some((reader, columns, endpoint)) = &mut self.csv {
            let mut record = ByteRecord::new();
            return match reader.read_byte_record(&mut record) {
                Ok(true) => {
                    let item = columns.item(*endpoint, &record);
                    Ok(Some((Some(record), Ok(item))))
                }
                Ok(false) => Ok(None),
                Err(error) => Err(format!("the CSV could not be read: {error}")),
            };
        }
        let Some(reader) = &mut self.jsonl else {
            return Ok(None);
        };
        loop {
            self.line.clear();
            match reader.read_line(&mut self.line) {
                Ok(0) => return Ok(None),
                Ok(_) if self.line.trim().is_empty() => continue,
                Ok(_) => {
                    let item = serde_json::from_str::<RawItem>(&self.line)
                        .map_err(|_| Problem::malformed_body("the line is not JSON"));
                    return Ok(Some((None, item)));
                }
                Err(error) => return Err(format!("the upload failed: {error}")),
            }
        }
    }
}

/// Write each answer as it comes, CSV in input order and JSONL in the order
/// rows finish, flushing whenever no answer waits so a reader sees each
/// result about as soon as it exists.
#[allow(clippy::too_many_arguments)]
fn write_results<A: Answer>(
    batch: &Batch,
    state: &AppState,
    output: Format,
    headers: Option<ByteRecord>,
    file: File,
    answers: mpsc::Receiver<Answered<A>>,
    request_id: Option<&str>,
    started: Instant,
) -> io::Result<()> {
    let mut sink = Sink::new(file);
    let width = headers.as_ref().map_or(0, ByteRecord::len);
    let mut csv = match output {
        Format::Csv => {
            let mut writer = WriterBuilder::new().flexible(true).from_writer(Vec::new());
            let mut header = headers.unwrap_or_else(|| ByteRecord::from(vec!["row", "id"]));
            for column in audit_columns() {
                header.push_field(column.as_bytes());
            }
            writer.write_byte_record(&header)?;
            sink.write(&writer.into_inner().map_err(|error| error.into_error())?)?;
            Some(BTreeMap::<usize, Vec<u8>>::new())
        }
        Format::Jsonl => None,
    };
    let mut next_row = 0;
    let publish = |sink: &mut Sink| -> io::Result<()> {
        sink.flush()?;
        let bytes = sink.written;
        batch
            .progress
            .send_modify(|progress| progress.bytes = bytes);
        Ok(())
    };

    let mut handle = |answered: Answered<A>, sink: &mut Sink| -> io::Result<()> {
        for row in answered.rows {
            let mut item = answered.item.clone();
            item.id = row.id.clone();
            count(batch, state, &mut item, row.index, request_id, started);
            match &mut csv {
                Some(pending) => {
                    pending.insert(row.index, csv_row(&row, width, &item)?);
                    while let Some(bytes) = pending.remove(&next_row) {
                        sink.write(&bytes)?;
                        next_row += 1;
                    }
                }
                None => {
                    let mut line = serde_json::to_vec(&Line {
                        row: row.index,
                        item: &item,
                    })?;
                    line.push(b'\n');
                    sink.write(&line)?;
                }
            }
        }
        Ok(())
    };

    while let Ok(answered) = answers.recv() {
        handle(answered, &mut sink)?;
        while let Ok(answered) = answers.try_recv() {
            handle(answered, &mut sink)?;
        }
        publish(&mut sink)?;
    }
    // Rows a cancelled batch never answered leave gaps; what was answered
    // is still written, in order.
    if let Some(pending) = csv {
        for bytes in pending.into_values() {
            sink.write(&bytes)?;
        }
    }
    publish(&mut sink)
}

/// Count a row in the summary, and log and stamp it if it failed inside the
/// server.
fn count<A>(
    batch: &Batch,
    state: &AppState,
    item: &mut Item<A>,
    index: usize,
    request_id: Option<&str>,
    started: Instant,
) {
    let mut status = batch.status.lock().expect("batch status");
    status.summary.total += 1;
    let Some(error) = item.error.take() else {
        status.summary.succeeded += 1;
        return;
    };
    status.summary.failed += 1;
    drop(status);
    if error.status() == StatusCode::INTERNAL_SERVER_ERROR.as_u16() {
        state.error_log().internal_error(
            request_id.unwrap_or("-"),
            "/batches",
            Some(index),
            started,
            error.cause().map(|cause| format!("{cause:#}")),
        );
    }
    item.error = Some(match request_id {
        Some(id) => error.with_request_id(id),
        None => error,
    });
}

/// A JSONL result: the row's place in the upload, and its item.
#[derive(Serialize)]
struct Line<'a, A> {
    row: usize,
    #[serde(flatten)]
    item: &'a Item<A>,
}

/// A CSV result: the row's own cells, padded to the header's width, then
/// `geocode batch`'s audit columns.
fn csv_row<A: Answer>(row: &RowIn, width: usize, item: &Item<A>) -> io::Result<Vec<u8>> {
    let mut record = ByteRecord::new();
    match &row.cells {
        Some(cells) => {
            for index in 0..width.max(cells.len()) {
                record.push_field(cells.get(index).unwrap_or_default());
            }
        }
        None => {
            record.push_field(row.index.to_string().as_bytes());
            record.push_field(row.id.as_deref().unwrap_or_default().as_bytes());
        }
    }
    let (status, query, values, reason) = match (&item.response, &item.error) {
        (Some(answer), _) => {
            let (status, query, values) = answer.columns();
            let reason = if status == "unresolved" {
                "no match".to_string()
            } else {
                String::new()
            };
            (status, query, values, reason)
        }
        (None, error) => {
            let reason = error.as_ref().map_or_else(String::new, |error| {
                let code = error.error_code().as_str();
                match error.detail() {
                    Some(detail) => format!("{code}: {detail}"),
                    None => code.to_string(),
                }
            });
            let values = api::flat_columns(None)
                .into_iter()
                .map(|(_, value)| value)
                .collect();
            ("error", String::new(), values, reason)
        }
    };
    record.push_field(status.as_bytes());
    record.push_field(query.as_bytes());
    for value in &values {
        record.push_field(value.as_bytes());
    }
    record.push_field(reason.as_bytes());
    let mut writer = WriterBuilder::new().flexible(true).from_writer(Vec::new());
    writer.write_byte_record(&record)?;
    writer.into_inner().map_err(|error| error.into_error())
}

/// The results file, counting what it holds.
struct Sink {
    file: BufWriter<File>,
    written: u64,
}

impl Sink {
    fn new(file: File) -> Self {
        Self {
            file: BufWriter::new(file),
            written: 0,
        }
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

/// The results file as a body: what is written so far, then the rest as it
/// is written, until the batch stops.
fn tail(batch: Arc<Batch>) -> Body {
    let progress = batch.progress.subscribe();
    let stream = futures_util::stream::unfold(
        (batch, progress, 0u64, None::<File>),
        |(batch, mut progress, offset, file)| async move {
            loop {
                let now = *progress.borrow_and_update();
                if now.bytes > offset {
                    let path = batch.results.clone();
                    let wanted = (now.bytes - offset).min(STREAM_CHUNK_BYTES as u64) as usize;
                    let read = task::spawn_blocking(move || -> io::Result<(File, Vec<u8>)> {
                        let mut file = match file {
                            Some(file) => file,
                            None => File::open(path)?,
                        };
                        file.seek(SeekFrom::Start(offset))?;
                        let mut chunk = vec![0; wanted];
                        file.read_exact(&mut chunk)?;
                        Ok((file, chunk))
                    })
                    .await
                    .unwrap_or_else(|error| Err(io::Error::other(error)));
                    return Some(match read {
                        Ok((file, chunk)) => {
                            let next = offset + chunk.len() as u64;
                            (Ok(Bytes::from(chunk)), (batch, progress, next, Some(file)))
                        }
                        // The batch was deleted under the reader; end here.
                        Err(error) => (Err(error), (batch, progress, u64::MAX, None)),
                    });
                }
                if now.done || offset == u64::MAX {
                    return None;
                }
                if progress.changed().await.is_err() {
                    return None;
                }
            }
        },
    );
    Body::from_stream(stream)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn no_such_batch() -> Problem {
    Problem::not_found().with_detail("no batch with this id; it may have expired")
}

/// Get a batch
///
/// Returns a batch's status and how many rows it has answered.
#[utoipa::path(
    get,
    path = "/batches/{id}",
    tag = "batches",
    params(("id" = String, Path, description = "The batch's id.")),
    responses(
        (status = 200, description = "The batch.", body = BatchStatus),
        (status = 404, description = "No batch has this id, or it expired.",
         body = openapi::Problem, content_type = PROBLEM_JSON),
    ),
)]
pub(crate) async fn status(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, Problem> {
    let batch = state.batches().get(&id).ok_or_else(no_such_batch)?;
    Ok(axum::Json(batch.snapshot()).into_response())
}

/// Get a batch's results
///
/// Returns everything the batch has written so far, then the rest as it is
/// written, until the batch stops.
#[utoipa::path(
    get,
    path = "/batches/{id}/results",
    tag = "batches",
    params(("id" = String, Path, description = "The batch's id.")),
    responses(
        (status = 200, description = "The results, in the batch's output format.",
         content(
             (String = "text/csv"),
             (String = "application/jsonl"),
         )),
        (status = 404, description = "No batch has this id, or it expired.",
         body = openapi::Problem, content_type = PROBLEM_JSON),
    ),
)]
pub(crate) async fn results(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, Problem> {
    let batch = state.batches().get(&id).ok_or_else(no_such_batch)?;
    let output = batch.snapshot().output_format;
    Ok((
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static(output.media_type()),
        )],
        tail(batch),
    )
        .into_response())
}

/// Cancel a batch
///
/// Stops reading and looking up rows. The rows already answered are kept.
#[utoipa::path(
    post,
    path = "/batches/{id}/cancel",
    tag = "batches",
    params(("id" = String, Path, description = "The batch's id.")),
    responses(
        (status = 200, description = "The batch, stopping.", body = BatchStatus),
        (status = 404, description = "No batch has this id, or it expired.",
         body = openapi::Problem, content_type = PROBLEM_JSON),
    ),
)]
pub(crate) async fn cancel(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, Problem> {
    let batch = state.batches().get(&id).ok_or_else(no_such_batch)?;
    batch.cancel.store(true, Ordering::Relaxed);
    Ok(axum::Json(batch.snapshot()).into_response())
}

/// Delete a batch
///
/// Cancels the batch if it is running, and deletes its results.
#[utoipa::path(
    delete,
    path = "/batches/{id}",
    tag = "batches",
    params(("id" = String, Path, description = "The batch's id.")),
    responses(
        (status = 204, description = "Deleted."),
        (status = 404, description = "No batch has this id, or it expired.",
         body = openapi::Problem, content_type = PROBLEM_JSON),
    ),
)]
pub(crate) async fn delete(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, Problem> {
    state.batches().remove(&id).ok_or_else(no_such_batch)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Where a server on `port` keeps its batches: under the system's temporary
/// directory, which it may clear. One directory per port, because a server
/// starting up fails the batches it finds running, and two servers sharing
/// one would fail each other's.
pub(crate) fn default_dir(port: u16) -> PathBuf {
    std::env::temp_dir().join(format!("open-geocode-batches-{port}"))
}

/// Run [`BatchStore::sweep`] every so often, for as long as the server runs.
pub(crate) async fn sweep_forever(state: AppState) {
    let mut every = tokio::time::interval(Duration::from_secs(600));
    loop {
        every.tick().await;
        state.batches().sweep();
    }
}
