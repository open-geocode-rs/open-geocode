//! Batch v7, with HERE's job flow outside and og's `/jobs` inside.
//!
//! The upload is read as it arrives and written as the og items its records
//! ask, one JSON line each, so a job never holds its input in memory. Each
//! start runs those items as an og job, kept and listed at `/jobs/{id}` as
//! any other. The first runs under the HERE job's own UUID, a resumed one
//! under a new one. Results and errors are og's results, read back and written as
//! HERE's files on download.
//!
//! An og item's `id` carries what HERE's output needs back and og cannot
//! honour, as `{ordinal}|{limit}|{radius}|{recId}`. og echoes it untouched.

use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{Body, Bytes},
    extract::{OriginalUri, Path as UrlPath, RawQuery, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{sync::mpsc, task};

use crate::{
    compat::{VendorError, retry_after},
    http::{
        jobs::{self, Endpoint, Status},
        problem::{ErrorCode, Problem},
    },
    runtime::AppState,
};

use super::{
    Area, Gs7Error, LatLng, Params, Rid,
    item::{Feature, Item, item},
    notify,
    search::{Geocode, Qualified, first_language, reverse_items},
};

pub(super) const GEOCODE_HRN: &str = "hrn:here:service::olp-here:search-geocode-7";
pub(super) const REVGEOCODE_HRN: &str = "hrn:here:service::olp-here:search-revgeocode-7";

/// The longest `recId` og can carry back. og's item id holds 128
/// characters, and the ordinal, limit and radius go first.
const MAX_REC_ID_CHARS: usize = 100;
/// HERE's longest record line.
const MAX_LINE_CHARS: usize = 4096;
/// Bytes a record line may take before it is cut off as too long, at most
/// four per character in UTF-8.
const MAX_LINE_BYTES: u64 = MAX_LINE_CHARS as u64 * 4;
/// Bytes per chunk when streaming a file out.
const CHUNK_BYTES: usize = 64 << 10;

/// Read-modify-write of a job's record, one at a time. A start and a stop
/// arriving together must not both win.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The service a job wraps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Service {
    Geocode,
    Revgeocode,
}

impl Service {
    fn hrn(self) -> &'static str {
        match self {
            Self::Geocode => GEOCODE_HRN,
            Self::Revgeocode => REVGEOCODE_HRN,
        }
    }

    fn endpoint(self) -> Endpoint {
        match self {
            Self::Geocode => Endpoint::Geocode,
            Self::Revgeocode => Endpoint::Reverse,
        }
    }

    /// The CSV columns after `recId|seqNumber|seqLength` when the job names
    /// none.
    fn default_columns(self) -> &'static [&'static str] {
        match self {
            Self::Geocode => &["position", "addressLabel", "title"],
            Self::Revgeocode => &["position", "distance", "addressLabel"],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OutputType {
    Csv,
    Jsonl,
}

impl OutputType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Jsonl => "jsonl",
        }
    }
}

/// A HERE job as this server keeps it, beside og's own job records.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    id: String,
    service: Service,
    #[serde(default)]
    tag: Option<String>,
    billing_tags: Vec<String>,
    output_type: OutputType,
    output_delimiter: String,
    /// The CSV columns asked, in HERE's spelling; the default when `None`.
    #[serde(default)]
    output_columns: Option<Vec<String>>,
    /// JSONL paths to keep; every field when `None`.
    #[serde(default)]
    output_filter: Option<Vec<String>>,
    created: u64,
    #[serde(default)]
    started: Option<u64>,
    /// Records written for og to look up.
    items: usize,
    /// Records refused by batch validation, HERE's `invalid`.
    invalid: usize,
    /// Valid records whose parameters the wrapped service would refuse, failed
    /// without a lookup.
    rejected: usize,
    /// og job ids, one per start, oldest first.
    runs: Vec<String>,
    stopped: bool,
    deleted: bool,
    /// Unix seconds. For a job never started or deleted; a started one
    /// lives as long as its og jobs.
    expires_at: u64,
    request_id: String,
    /// The counts and end of a deleted job, whose og jobs are gone with it.
    #[serde(default)]
    frozen: Option<Frozen>,
    /// Notifications called each time a run of the job ends.
    #[serde(default)]
    notification_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Frozen {
    records: Records,
    ended: Option<u64>,
}

/// HERE's `JobRecords`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Records {
    total: usize,
    valid: usize,
    invalid: usize,
    processed: usize,
    pending: usize,
    succeeded: usize,
    failed: usize,
}

/// A job as it stands, with its record, the HERE status its og jobs come to
/// and their counts.
#[derive(Debug)]
struct View {
    meta: Meta,
    status: &'static str,
    records: Records,
    ended: Option<u64>,
}

impl View {
    fn results_ready(&self) -> bool {
        matches!(self.status, "completed" | "stopped")
    }

    fn errors_ready(&self) -> bool {
        matches!(self.status, "completed" | "stopped" | "failure")
    }

    fn errors(&self) -> usize {
        self.records.invalid + self.records.failed
    }
}

/// Batch's `ErrorResponse`, and its status.
#[derive(Debug)]
pub(super) struct BatchError {
    status: StatusCode,
    pub(super) title: String,
    code: &'static str,
    cause: String,
    action: String,
    problem: Option<Problem>,
}

impl BatchError {
    pub(super) fn new(
        status: StatusCode,
        code: &'static str,
        title: impl Into<String>,
        cause: impl Into<String>,
        action: impl Into<String>,
    ) -> Self {
        Self {
            status,
            title: title.into(),
            code,
            cause: cause.into(),
            action: action.into(),
            problem: None,
        }
    }

    // HERE publishes few error codes. Unspecified cases use local E600 codes.
    pub(super) fn bad_request(title: impl Into<String>, cause: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "E6004001",
            title,
            cause,
            "Correct the request and try again",
        )
    }

    fn job_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "E6004041",
            "Job not found",
            "Job with the specified ID could not be found",
            "Verify the job ID and try again",
        )
    }

    fn of(problem: Problem) -> Self {
        let mut error = if problem.error_code() == ErrorCode::Busy {
            Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "E6004291",
                "Too many concurrent jobs running",
                "You have exceeded the number of jobs you may run concurrently",
                "Stop or wait for your existing jobs to complete and then try starting the job again",
            )
        } else {
            let status =
                StatusCode::from_u16(problem.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            Self::new(
                status,
                if status.is_client_error() {
                    "E6004001"
                } else {
                    "E6005000"
                },
                status.canonical_reason().unwrap_or("Error"),
                problem
                    .detail()
                    .unwrap_or("The server failed to run the job"),
                "Try again later",
            )
        };
        error.problem = Some(problem);
        error
    }

    pub(super) fn into_response(self, request_id: &str) -> Response {
        let body = json!({
            "title": self.title,
            "status": self.status.as_u16(),
            "code": self.code,
            "cause": self.cause,
            "action": self.action,
            "correlationId": request_id,
        });
        let mut error = VendorError::new(self.status, body);
        if let Some(problem) = self.problem {
            if let Some((name, value)) = retry_after(&problem) {
                error = error.with_header(name, value);
            }
            error = error.caused_by(problem);
        }
        error.into_response()
    }
}

fn param_error(error: &Gs7Error) -> BatchError {
    BatchError::bad_request(error.title.clone(), error.cause.clone().unwrap_or_default())
}

pub(super) async fn not_found(Rid(request_id): Rid, OriginalUri(uri): OriginalUri) -> Response {
    BatchError::new(
        StatusCode::NOT_FOUND,
        "E6004040",
        "Not found",
        format!("No resource at {} for this method", uri.path()),
        "Check the URL and method",
    )
    .into_response(&request_id)
}

pub(super) async fn version() -> Response {
    axum::Json(json!({
        "apiVersion": "v7",
        "serviceVersion": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

pub(super) async fn health(State(state): State<AppState>) -> Response {
    let status = if state.is_ready() { "ok" } else { "fail" };
    axum::Json(json!({ "status": status })).into_response()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Unix seconds as RFC 3339 in UTC, HERE's `date-time`.
pub(super) fn date_time(seconds: u64) -> String {
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    let (year, month, day) = super::super::civil_date_from_unix_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// Where HERE jobs are kept, beside og's in a folder og's own scan skips.
pub(super) fn store_dir(state: &AppState) -> PathBuf {
    state.jobs().dir().join("here")
}

/// A job's file. The id is checked to be `JOB-` and a UUID first, so it can
/// never name a path outside the store.
fn file(state: &AppState, id: &str, extension: &str) -> PathBuf {
    store_dir(state).join(format!("{id}.{extension}"))
}

fn job_uuid(id: &str) -> Option<uuid::Uuid> {
    let uuid = uuid::Uuid::parse_str(id.strip_prefix("JOB-")?).ok()?;
    // Only the hyphenated form this server hands out names a job, so one
    // job has one id.
    (format!("JOB-{uuid}") == id).then_some(uuid)
}

fn load(state: &AppState, id: &str) -> Option<Meta> {
    job_uuid(id)?;
    let bytes = fs::read(file(state, id, "json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save(state: &AppState, meta: &Meta) -> io::Result<()> {
    let json = serde_json::to_vec(meta).map_err(io::Error::other)?;
    let path = file(state, &meta.id, "json");
    let partial = path.with_extension("json.partial");
    fs::write(&partial, json)?;
    fs::rename(partial, path)
}

fn remove_files(state: &AppState, id: &str, record_too: bool) {
    for extension in ["items", "rejects"] {
        let _ = fs::remove_file(file(state, id, extension));
    }
    if record_too {
        let _ = fs::remove_file(file(state, id, "json"));
        let _ = fs::remove_file(file(state, id, "json.partial"));
    }
}

/// The job's status as HERE names it, or `None` once it has expired, past
/// its time or with an og job of it gone. An expired job's files are removed.
fn view(state: &AppState, meta: Meta) -> Option<View> {
    let mut runs = Vec::with_capacity(meta.runs.len());
    for run in &meta.runs {
        match state.jobs().get(run) {
            Some(job) => runs.push(job.snapshot()),
            None if !meta.deleted => {
                remove_files(state, &meta.id, true);
                return None;
            }
            None => {}
        }
    }
    if (meta.deleted || meta.runs.is_empty()) && meta.expires_at <= unix_now() {
        remove_files(state, &meta.id, true);
        return None;
    }
    let last = runs.last();
    let status = here_status(&meta, last.map(|run| run.status));
    let mut records = Records {
        valid: meta.items + meta.rejected,
        invalid: meta.invalid,
        processed: meta.rejected,
        failed: meta.rejected,
        ..Records::default()
    };
    records.total = records.valid + records.invalid;
    for run in &runs {
        records.processed += run.summary.total;
        records.succeeded += run.summary.succeeded;
        records.failed += run.summary.failed;
    }
    records.pending = records.valid.saturating_sub(records.processed);
    let mut ended = last
        .filter(|run| run.status != Status::Running)
        .and_then(|run| run.completed_at);
    if let Some(frozen) = meta.frozen {
        records = frozen.records;
        ended = frozen.ended;
    }
    Some(View {
        meta,
        status,
        records,
        ended,
    })
}

/// HERE's status of a job whose latest og job is in `last`.
fn here_status(meta: &Meta, last: Option<Status>) -> &'static str {
    match last {
        _ if meta.deleted => "deleted",
        None => "submitted",
        Some(Status::Running) => "inProgress",
        Some(Status::Completed) => "completed",
        Some(Status::Cancelled) if meta.stopped => "stopped",
        // Cancelled by a server shutting down, not by a stop. That is HERE's
        // failure, which a start resumes.
        Some(Status::Cancelled | Status::Failed) => "failure",
    }
}

fn find(state: &AppState, id: &str) -> Result<View, BatchError> {
    load(state, id)
        .and_then(|meta| view(state, meta))
        .ok_or_else(BatchError::job_not_found)
}

fn stored(state: &AppState) -> Vec<Meta> {
    let Ok(entries) = fs::read_dir(store_dir(state)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            load(state, name.to_str()?.strip_suffix(".json")?)
        })
        .collect()
}

pub(crate) async fn sweep(state: &AppState) {
    let _guard = LOCK.lock().await;
    for meta in stored(state) {
        view(state, meta);
    }
}

/// Every kept job, newest first.
fn all(state: &AppState) -> Vec<View> {
    let mut views = stored(state)
        .into_iter()
        .filter_map(|meta| view(state, meta))
        .collect::<Vec<_>>();
    views.sort_by(|a, b| {
        b.meta
            .created
            .cmp(&a.meta.created)
            .then_with(|| a.meta.id.cmp(&b.meta.id))
    });
    views
}

/// Where this job's links point. They start where clients reach the server
/// (see [`AppState::origin`]) and keep the request's prefix, so a link works
/// through whatever path the server is mounted at.
#[derive(Debug)]
pub(super) struct Links {
    jobs: String,
}

impl Links {
    pub(super) fn of(state: &AppState, headers: &HeaderMap, uri: &axum::http::Uri) -> Self {
        let path = uri.path();
        let prefix = path.find("/v7/").map_or("", |at| &path[..at]);
        Self {
            jobs: format!("{}{prefix}/v7/batch/jobs", state.origin(headers)),
        }
    }

    fn job(&self, id: &str) -> String {
        format!("{}/{id}", self.jobs)
    }
}

/// HERE's short `JobResponse`, which every action answers with.
fn job_response(view: &View, links: &Links) -> Value {
    let mut body = json!({
        "id": view.meta.id,
        "serviceHrn": view.meta.service.hrn(),
    });
    if let Some(tag) = &view.meta.tag {
        body["tag"] = tag.as_str().into();
    }
    if view.status == "deleted" {
        body["status"] = "deleted".into();
        return body;
    }
    body["billingTags"] = json!(view.meta.billing_tags);
    body["status"] = view.status.into();
    body["href"] = links.job(&view.meta.id).into();
    body
}

/// HERE's `JobDetailResponse`.
fn job_details(view: &View, links: &Links) -> Value {
    let mut body = job_response(view, links);
    let href = links.job(&view.meta.id);
    body["billingTags"] = json!(view.meta.billing_tags);
    body["href"] = href.as_str().into();
    body["created"] = date_time(view.meta.created).into();
    if let Some(started) = view.meta.started {
        body["started"] = date_time(started).into();
    }
    if let Some(ended) = view.ended {
        body["ended"] = date_time(ended).into();
    }
    if !(view.status == "deleted" && view.meta.runs.is_empty()) {
        body["records"] = json!(view.records);
    }
    let results = view.results_ready() && view.records.succeeded > 0;
    let errors = view.errors_ready() && view.errors() > 0;
    body["resultsHref"] = if results {
        format!("{href}/results")
    } else {
        String::new()
    }
    .into();
    body["errorsHref"] = if errors {
        format!("{href}/errors")
    } else {
        String::new()
    }
    .into();
    body["outputType"] = view.meta.output_type.as_str().into();
    if !view.meta.notification_ids.is_empty() {
        body["notificationIds"] = json!(view.meta.notification_ids);
    }
    body
}

/// How the upload is read and the output written, from `POST`'s query.
#[derive(Debug)]
struct Plan {
    service: Service,
    tag: Option<String>,
    billing_tags: Vec<String>,
    input_delimiter: char,
    output_delimiter: char,
    input_columns: Option<String>,
    output_columns: Option<Vec<String>>,
    output_filter: Option<Vec<String>>,
    start: bool,
    output_type: OutputType,
    notification_ids: Vec<String>,
}

/// HERE's longest `inputColumns`, `outputColumns` or `outputFilter`.
const MAX_DESCRIPTOR_CHARS: usize = 1024;

impl Plan {
    fn read(params: &Params) -> Result<Self, BatchError> {
        let service = match params.get("serviceHrn").unwrap_or(GEOCODE_HRN) {
            GEOCODE_HRN => Service::Geocode,
            REVGEOCODE_HRN => Service::Revgeocode,
            _ => {
                return Err(BatchError::new(
                    StatusCode::NOT_FOUND,
                    "E6004043",
                    "Service not found or is unsupported",
                    "The Service with the specified HRN could not be found or is not supported at this time",
                    "Check that the Service HRN was entered correctly",
                ));
            }
        };
        for name in ["inputColumns", "outputColumns", "outputFilter"] {
            if params
                .get(name)
                .is_some_and(|value| value.chars().count() > MAX_DESCRIPTOR_CHARS)
            {
                return Err(BatchError::new(
                    StatusCode::BAD_REQUEST,
                    "E6004002",
                    "Argument exceeds maximum length limit",
                    format!(
                        "The argument {name} exceeds the maximum supported character length ({MAX_DESCRIPTOR_CHARS} characters)"
                    ),
                    "Shorten the argument to match length requirements",
                ));
            }
        }
        let start = match params.get("startJob") {
            None | Some("true") => true,
            Some("false") => false,
            Some(other) => {
                return Err(BatchError::bad_request(
                    "Illegal input for parameter 'startJob'",
                    format!("Actual parameter value: '{other}'; expected true or false"),
                ));
            }
        };
        let output_type = match params.get("outputType") {
            None | Some("csv") => OutputType::Csv,
            Some("jsonl") => OutputType::Jsonl,
            Some(other) => {
                return Err(BatchError::bad_request(
                    "Illegal input for parameter 'outputType'",
                    format!("Actual parameter value: '{other}'; expected csv or jsonl"),
                ));
            }
        };
        let list = |name: &str, separator: char| {
            params.get(name).map(|value| {
                value
                    .split(separator)
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
        };
        let output_delimiter = delimiter(params.get("outputDelimiter"), true)?;
        Ok(Self {
            service,
            tag: params.get("tag").map(str::to_string),
            billing_tags: billing_tags(params.get("billingTags"))?,
            input_delimiter: delimiter(params.get("inputDelimiter"), false)?,
            output_delimiter,
            input_columns: params.get("inputColumns").map(str::to_string),
            // The descriptor is written as a result row is, split by the
            // output delimiter; the filter is a list of comma-separated paths.
            output_columns: list("outputColumns", output_delimiter),
            output_filter: list("outputFilter", ','),
            start,
            output_type,
            notification_ids: notification_ids(params.get("notificationIds"))?,
        })
    }
}

/// `notificationIds`, at most 5, comma-separated. Whether each is kept is
/// checked against the store once the job is made.
fn notification_ids(value: Option<&str>) -> Result<Vec<String>, BatchError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let mut ids = Vec::new();
    for id in value.split(',').map(str::trim).filter(|id| !id.is_empty()) {
        if !ids.iter().any(|seen: &String| seen == id) {
            ids.push(id.to_string());
        }
    }
    if ids.len() > notify::MAX_PER_JOB {
        return Err(BatchError::bad_request(
            "Too many notifications",
            format!(
                "The job names {} notifications; at most {} are allowed",
                ids.len(),
                notify::MAX_PER_JOB
            ),
        ));
    }
    Ok(ids)
}

/// A delimiter, one character or `\t` written out for a tab. An output
/// delimiter must not be one HERE's own output formats use.
fn delimiter(value: Option<&str>, output: bool) -> Result<char, BatchError> {
    let Some(value) = value else {
        return Ok('|');
    };
    let unsupported = || {
        BatchError::new(
            StatusCode::BAD_REQUEST,
            "E6004005",
            "Delimiter character is unsupported",
            "The supplied character string is not supported as a delimiter for output/input",
            "Provide a single character delimiter that can be used to delimit output/input",
        )
    };
    let delimiter = if value == "\\t" {
        '\t'
    } else {
        let mut chars = value.chars();
        match (chars.next(), chars.next()) {
            (Some(delimiter), None) => delimiter,
            _ => return Err(unsupported()),
        }
    };
    let reserved = if output {
        &[' ', '.', '"', '(', ')', '\n', '\r'][..]
    } else {
        &['"', '\n', '\r'][..]
    };
    if reserved.contains(&delimiter) {
        return Err(unsupported());
    }
    Ok(delimiter)
}

/// `billingTags`, at most 6 tags of 4 to 16 of `[A-Za-z0-9._-]` with the
/// punctuation never first or last, joined by `+` or `,` but not both. A
/// `+` sent unescaped arrives as a space, and is read as the `+` it was. A
/// tag given twice is taken once.
fn billing_tags(value: Option<&str>) -> Result<Vec<String>, BatchError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let invalid = || {
        BatchError::bad_request(
            "Invalid billing tags.",
            format!("Actual parameter value: '{value}'"),
        )
    };
    if value.contains(['+', ' ']) && value.contains(',') {
        return Err(invalid());
    }
    let mut tags = Vec::<String>::new();
    for tag in value.split(['+', ' ', ',']) {
        if !tags.iter().any(|held| held == tag) {
            tags.push(tag.to_string());
        }
    }
    let valid_tag = |tag: &String| {
        let punct = |byte: u8| matches!(byte, b'-' | b'_' | b'.');
        (4..=16).contains(&tag.len())
            && tag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || punct(byte))
            && !tag.bytes().next().is_some_and(punct)
            && !tag.bytes().last().is_some_and(punct)
    };
    if tags.len() > 6 || !tags.iter().all(valid_tag) {
        return Err(invalid());
    }
    Ok(tags)
}

/// The upload as a [`Read`], fed chunk by chunk from the request body.
#[derive(Debug)]
struct Upload {
    chunks: mpsc::Receiver<Result<Bytes, String>>,
    chunk: Bytes,
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

/// What the intake found.
#[derive(Debug, Default)]
struct Intake {
    items: usize,
    invalid: usize,
    rejected: usize,
}

/// Why an upload as a whole was refused.
#[derive(Debug)]
enum Refusal {
    TooLarge,
    TooManyRecords(usize),
    Unreadable(String),
}

/// One column of the input. Its name is lower-cased, since HERE's are not
/// case-sensitive, and a `name=value` header cell gives it a default.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Column {
    name: String,
    default: Option<String>,
}

fn columns(cells: Vec<String>) -> Vec<Column> {
    cells
        .into_iter()
        .map(|cell| match cell.split_once('=') {
            Some((name, default)) => Column {
                name: name.trim().to_ascii_lowercase(),
                default: Some(default.trim().to_string()).filter(|value| !value.is_empty()),
            },
            None => Column {
                name: cell.trim().to_ascii_lowercase(),
                default: None,
            },
        })
        .collect()
}

/// A record line's cells. HERE's input is CSV-like. A cell holding the
/// delimiter is quoted with `"`, a quote inside quotes is `""`, and quoting
/// may start or end anywhere in a cell.
fn cells(line: &str, delimiter: char) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            c if c == delimiter && !quoted => cells.push(std::mem::take(&mut cell)),
            c => cell.push(c),
        }
    }
    cells.push(cell);
    cells
}

/// How record lines are read.
#[derive(Debug)]
enum Layout {
    Delimited(Vec<Column>),
    /// Each line a URL query string; the delimiter does not apply.
    Query,
}

/// One input record, its values by lower-cased column name.
#[derive(Debug, Default)]
struct Record(Vec<(String, String)>);

impl Record {
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(given, _)| given == name)
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty())
    }
}

/// A record HERE's batch validation refuses, `999` in the error report.
#[derive(Debug)]
struct Invalid {
    cause: String,
    action: &'static str,
}

impl Invalid {
    fn line() -> Self {
        Self {
            cause: "The input line does not conform to given input format for job.".to_string(),
            action: "Please check format for job record and resubmit in new job.",
        }
    }
}

/// Reads an upload into og items and refused records, a line at a time.
#[derive(Debug)]
struct Reader<'a> {
    plan: &'a Plan,
    request_id: &'a str,
    items: BufWriter<File>,
    rejects: BufWriter<File>,
    intake: Intake,
    max_items: usize,
}

impl Reader<'_> {
    fn run(&mut self, upload: impl Read, max_bytes: u64) -> Result<(), Refusal> {
        let mut upload = BufReader::new(upload.take(max_bytes.saturating_add(1)));
        let mut total = 0u64;
        let mut layout = self
            .plan
            .input_columns
            .as_deref()
            .map(|header| Layout::Delimited(columns(cells(header, self.plan.input_delimiter))));
        let mut record_number = 0usize;
        let mut first_line = true;
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = (&mut upload)
                .take(MAX_LINE_BYTES + 1)
                .read_until(b'\n', &mut line)
                .map_err(|error| Refusal::Unreadable(error.to_string()))?;
            if read == 0 {
                break;
            }
            total += read as u64;
            let mut too_long = false;
            if read as u64 > MAX_LINE_BYTES && line.last() != Some(&b'\n') {
                too_long = true;
                total += skip_line(&mut upload)
                    .map_err(|error| Refusal::Unreadable(error.to_string()))?;
            }
            if total > max_bytes {
                return Err(Refusal::TooLarge);
            }
            if first_line {
                first_line = false;
                if line.starts_with("\u{feff}".as_bytes()) {
                    line.drain(..3);
                }
            }
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            let text = String::from_utf8_lossy(&line).into_owned();
            if text.trim().is_empty() {
                continue;
            }
            if layout.is_none() {
                let query = !text.contains(self.plan.input_delimiter) && text.contains('=');
                if query {
                    layout = Some(Layout::Query);
                } else {
                    layout = Some(Layout::Delimited(columns(cells(
                        &text,
                        self.plan.input_delimiter,
                    ))));
                    continue;
                }
            }
            let layout = layout.as_ref().unwrap_or(&Layout::Query);
            let broken = too_long
                || text.chars().count() > MAX_LINE_CHARS
                || std::str::from_utf8(&line).is_err();
            // Only an inline layout consumes a header from the upload.
            record_number += 1;
            self.record(layout, record_number, &text, broken)
                .map_err(|error| Refusal::Unreadable(error.to_string()))?;
            if self.intake.items > self.max_items {
                return Err(Refusal::TooManyRecords(self.max_items));
            }
        }
        self.items
            .flush()
            .and_then(|()| self.rejects.flush())
            .map_err(|error| Refusal::Unreadable(error.to_string()))
    }

    fn record(
        &mut self,
        layout: &Layout,
        number: usize,
        text: &str,
        broken: bool,
    ) -> io::Result<()> {
        let record = if broken {
            None
        } else {
            read_record(layout, text, self.plan.input_delimiter)
        };
        let Some(record) = record else {
            return self.invalid(&format!("InputLine_{number}"), text, &Invalid::line());
        };
        let rec_id = match record.get("recid") {
            None => number.to_string(),
            Some(rec_id) => match check_rec_id(rec_id) {
                Ok(()) => rec_id.to_string(),
                Err(invalid) => {
                    return self.invalid(&format!("InputLine_{number}"), text, &invalid);
                }
            },
        };
        let ordinal = self.intake.items;
        match translate(self.plan.service, &record) {
            Ok(Translated {
                mut fields,
                limit,
                radius_m,
            }) => {
                let radius = radius_m
                    .map(|radius| radius.to_string())
                    .unwrap_or_default();
                let id = format!("{ordinal}|{limit}|{radius}|{rec_id}");
                fields.insert(0, ("id", id));
                let object = fields
                    .into_iter()
                    .map(|(name, value)| (name.to_string(), Value::String(value)))
                    .collect::<serde_json::Map<_, _>>();
                serde_json::to_writer(&mut self.items, &object)?;
                self.items.write_all(b"\n")?;
                self.intake.items += 1;
            }
            Err(error) => {
                let message = error.body_text(&format!("{}-{number}", self.request_id));
                self.reject(&rec_id, error.status.as_u16(), &message)?;
                self.intake.rejected += 1;
            }
        }
        Ok(())
    }

    fn invalid(&mut self, rec_id: &str, text: &str, invalid: &Invalid) -> io::Result<()> {
        /// HERE's message for a line it cannot read, its fields in HERE's
        /// order, as the errors file quotes it.
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Message<'a> {
            status: u16,
            record_id: &'a str,
            title: &'a str,
            cause: &'a str,
            action: &'a str,
            job_record: &'a str,
        }
        let message = serde_json::to_string(&Message {
            status: 999,
            record_id: rec_id,
            title: "Illegal input line",
            cause: &invalid.cause,
            action: invalid.action,
            job_record: text,
        })?;
        self.intake.invalid += 1;
        self.reject(rec_id, 999, &message)
    }

    /// A record refused before it runs, its message already as text.
    fn reject(&mut self, rec_id: &str, code: u16, message: &str) -> io::Result<()> {
        serde_json::to_writer(
            &mut self.rejects,
            &json!({ "recId": rec_id, "code": code, "message": message }),
        )?;
        self.rejects.write_all(b"\n")
    }
}

/// Read past the rest of an overlong line. Returns the bytes skipped.
fn skip_line(upload: &mut impl BufRead) -> io::Result<u64> {
    let mut skipped = 0u64;
    loop {
        let buffer = upload.fill_buf()?;
        if buffer.is_empty() {
            return Ok(skipped);
        }
        match buffer.iter().position(|byte| *byte == b'\n') {
            Some(end) => {
                upload.consume(end + 1);
                return Ok(skipped + end as u64 + 1);
            }
            None => {
                let length = buffer.len();
                upload.consume(length);
                skipped += length as u64;
            }
        }
    }
}

/// A line's values by column, or `None` for a line its layout cannot read,
/// with more cells than columns or a query string that does not parse.
fn read_record(layout: &Layout, text: &str, delimiter: char) -> Option<Record> {
    match layout {
        Layout::Query => {
            let pairs: Vec<(String, String)> = serde_urlencoded::from_str(text).ok()?;
            let mut record = Record::default();
            for (name, value) in pairs {
                let name = name.trim().to_ascii_lowercase();
                if record.0.iter().any(|(seen, _)| *seen == name) {
                    return None;
                }
                record.0.push((name, value.trim().to_string()));
            }
            Some(record)
        }
        Layout::Delimited(columns) => {
            let cells = cells(text, delimiter);
            // The header can supply defaults for omitted trailing columns.
            let short_of_default = columns
                .iter()
                .skip(cells.len())
                .any(|column| column.default.is_none());
            if cells.len() > columns.len() || short_of_default {
                return None;
            }
            Some(Record(
                columns
                    .iter()
                    .enumerate()
                    .map(|(index, column)| {
                        let cell = cells.get(index).map(|cell| cell.trim()).unwrap_or_default();
                        let value = match (cell, &column.default) {
                            ("", Some(default)) => default.clone(),
                            (cell, _) => cell.to_string(),
                        };
                        (column.name.clone(), value)
                    })
                    .collect(),
            ))
        }
    }
}

/// HERE's `recId`, at most 512 of `[a-zA-Z0-9.,_-]`, and here at most what
/// og's item id has room for.
fn check_rec_id(rec_id: &str) -> Result<(), Invalid> {
    let allowed =
        |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b',' | b'_' | b'-');
    if !rec_id.bytes().all(allowed) {
        return Err(Invalid {
            cause: format!("The recId '{rec_id}' holds a character outside [a-zA-Z0-9.,_-]."),
            action: "Please correct the recId and resubmit in new job.",
        });
    }
    if rec_id.len() > MAX_REC_ID_CHARS {
        return Err(Invalid {
            cause: format!("The recId is longer than {MAX_REC_ID_CHARS} characters."),
            action: "Please shorten the recId and resubmit in new job.",
        });
    }
    Ok(())
}

/// A record as og item fields, and what HERE's output needs that og does not
/// carry.
#[derive(Debug)]
struct Translated {
    fields: Vec<(&'static str, String)>,
    limit: usize,
    radius_m: Option<u64>,
}

/// A batch record defaults to one result, not the endpoint's 20.
const BATCH_DEFAULT_LIMIT: usize = 1;

/// The og item a record asks, by the rules its endpoint reads a request
/// with; a parameter that endpoint would refuse fails the record as its 400.
fn translate(service: Service, record: &Record) -> Result<Translated, Gs7Error> {
    let limit = match record.get("limit") {
        None => BATCH_DEFAULT_LIMIT,
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=100).contains(limit))
            .ok_or_else(|| Gs7Error::illegal("limit", value, "Provide an integer from 1 to 100"))?,
    };
    let lang = record.get("lang").and_then(first_language);
    super::check_show(
        record.get("show"),
        match service {
            Service::Geocode => &super::GEOCODE_SHOW,
            Service::Revgeocode => &super::REVGEOCODE_SHOW,
        },
    )?;
    let translated = match service {
        Service::Geocode => {
            let qualified = match record.get("qq") {
                Some(qq) => Qualified::parse(qq)?,
                None => Qualified {
                    country: record.get("country").map(str::to_string),
                    state: record.get("state").map(str::to_string),
                    county: record.get("county").map(str::to_string),
                    city: record.get("city").map(str::to_string),
                    district: record.get("district").map(str::to_string),
                    street: record.get("street").map(str::to_string),
                    house_number: record.get("housenumber").map(str::to_string),
                    postal_code: record.get("postalcode").map(str::to_string),
                },
            };
            let countries = match record.get("in") {
                None => Vec::new(),
                Some(value) => {
                    // A bare code, as HERE's own example input has it, is a
                    // country.
                    let value =
                        if value.len() == 3 && value.bytes().all(|b| b.is_ascii_alphabetic()) {
                            format!("countryCode:{value}")
                        } else {
                            value.to_string()
                        };
                    match Area::parse(&value)? {
                        Area::Countries(countries) => countries,
                        _ => {
                            return Err(Gs7Error::illegal(
                                "in",
                                &value,
                                "Provide countryCode:{countryCode}[,{countryCode}]*",
                            ));
                        }
                    }
                }
            };
            let at = record
                .get("at")
                .map(|at| LatLng::parse("at", at))
                .transpose()?;
            let request = Geocode {
                q: record.get("q").map(str::to_string),
                qualified,
                countries,
                at,
                limit,
            };
            let fields = request.og_fields().ok_or_else(|| {
                Gs7Error::new(StatusCode::BAD_REQUEST, "Missing parameter 'q' or 'qq'")
                    .action("Provide 'q', 'qq' or qualified columns")
            })?;
            Ok(Translated {
                fields,
                limit,
                radius_m: None,
            })
        }
        Service::Revgeocode => {
            let area = record.get("in").map(|value| {
                // Three bare numbers, joined by any of `/;, :|=`, are a
                // circle too.
                let numbers = value
                    .split(['/', ';', ',', ' ', ':', '|', '='])
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>();
                match numbers[..] {
                    [lat, lng, radius]
                        if !value.starts_with("circle")
                            && [lat, lng, radius].iter().all(|n| n.parse::<f64>().is_ok()) =>
                    {
                        format!("circle:{lat},{lng};r={radius}")
                    }
                    _ => value.to_string(),
                }
            });
            let (center, radius_m) = match (record.get("at"), area.as_deref()) {
                (Some(at), None) => (LatLng::parse("at", at)?, None),
                (None, Some(value)) => match Area::parse(value)? {
                    Area::Circle { center, radius_m } => (center, Some(radius_m)),
                    _ => {
                        return Err(Gs7Error::illegal(
                            "in",
                            value,
                            "Provide circle:{latitude},{longitude};r={radius}",
                        ));
                    }
                },
                (Some(_), Some(_)) => {
                    return Err(Gs7Error::new(
                        StatusCode::BAD_REQUEST,
                        "Parameters 'at' and 'in' are mutually exclusive",
                    ));
                }
                (None, None) => {
                    return Err(Gs7Error::new(
                        StatusCode::BAD_REQUEST,
                        "Missing parameter 'at' or 'in'",
                    ));
                }
            };
            Ok(Translated {
                fields: vec![
                    ("lon", center.lng.to_string()),
                    ("lat", center.lat.to_string()),
                ],
                limit,
                radius_m: radius_m.map(|radius| radius.ceil() as u64),
            })
        }
    };
    translated.map(|mut translated| {
        translated.fields.extend(lang.map(|lang| ("lang", lang)));
        translated
    })
}

/// Pass the request body on a chunk at a time, refusing one that grows past
/// `max_bytes`.
async fn forward(body: Body, max_bytes: u64, chunks: mpsc::Sender<Result<Bytes, String>>) {
    let mut stream = body.into_data_stream();
    let mut received = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                let _ = chunks
                    .send(Err(format!("the body broke off: {error}")))
                    .await;
                return;
            }
        };
        received += chunk.len() as u64;
        if received > max_bytes {
            let _ = chunks.send(Err("too large".to_string())).await;
            return;
        }
        if chunks.send(Ok(chunk)).await.is_err() {
            return;
        }
    }
}

fn too_large(max_bytes: u64) -> BatchError {
    BatchError::new(
        StatusCode::BAD_REQUEST,
        "E6004003",
        "Input exceeds maximum size limit",
        format!("The input exceeds the maximum allowed size of {max_bytes} bytes"),
        "Trim the input or split the job into smaller jobs",
    )
}

pub(super) async fn create(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    OriginalUri(uri): OriginalUri,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Body,
) -> Response {
    match create_job(&state, &request_id, &uri, query.as_deref(), &headers, body).await {
        Ok(response) => response,
        Err(error) => error.into_response(&request_id),
    }
}

async fn create_job(
    state: &AppState,
    request_id: &str,
    uri: &axum::http::Uri,
    query: Option<&str>,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, BatchError> {
    let params = Params::parse(query).map_err(|error| param_error(&error))?;
    let plan = Plan::read(&params)?;
    if let Some(unknown) = plan
        .notification_ids
        .iter()
        .find(|id| !notify::exists(state, id))
    {
        return Err(BatchError::bad_request(
            "Notification not found",
            format!("No notification has the ID '{unknown}'; create it first"),
        ));
    }
    let gzip = match crate::compat::header(headers, "content-encoding") {
        None | Some("identity") => false,
        Some(encoding) if encoding.eq_ignore_ascii_case("gzip") => true,
        Some(encoding) => {
            return Err(BatchError::bad_request(
                "Unsupported Content-Encoding",
                format!("The body is encoded as {encoding}; send it plain or as gzip"),
            ));
        }
    };
    let max_bytes = state.limits().max_job_upload_bytes();
    let declared = crate::compat::header(headers, "content-length")
        .and_then(|value| value.parse::<u64>().ok());
    if !gzip && declared.is_some_and(|length| length > max_bytes) {
        return Err(too_large(max_bytes));
    }

    let uuid = uuid::Uuid::new_v4();
    let id = format!("JOB-{uuid}");
    fs::create_dir_all(store_dir(state)).map_err(internal)?;
    let items = File::create(file(state, &id, "items")).map_err(internal)?;
    let rejects = File::create(file(state, &id, "rejects")).map_err(internal)?;

    let (chunks_tx, chunks_rx) = mpsc::channel(32);
    // The compressed body is held to the cap too. No upload is read past
    // what a plain one may hold.
    tokio::spawn(forward(body, max_bytes, chunks_tx));
    let upload = Upload {
        chunks: chunks_rx,
        chunk: Bytes::new(),
    };
    let max_items = state.limits().max_job_rows();
    let reading = {
        let request_id = request_id.to_string();
        task::spawn_blocking(move || {
            let mut reader = Reader {
                plan: &plan,
                request_id: &request_id,
                items: BufWriter::new(items),
                rejects: BufWriter::new(rejects),
                intake: Intake::default(),
                max_items,
            };
            let read = if gzip {
                reader.run(flate2::read::MultiGzDecoder::new(upload), max_bytes)
            } else {
                reader.run(upload, max_bytes)
            };
            let intake = std::mem::take(&mut reader.intake);
            drop(reader);
            read.map(|()| (plan, intake))
        })
    };
    let read = reading
        .await
        .map_err(|error| internal(io::Error::other(error)))?;
    let (plan, intake) = match read {
        Ok(read) => read,
        Err(refusal) => {
            remove_files(state, &id, true);
            return Err(match refusal {
                Refusal::TooLarge => too_large(max_bytes),
                Refusal::TooManyRecords(max) => BatchError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "E6004131",
                    "Input exceeds maximum record count limit.",
                    format!("The input holds more than {max} records"),
                    "Split the job into smaller jobs",
                ),
                Refusal::Unreadable(cause) if cause == "too large" => too_large(max_bytes),
                Refusal::Unreadable(cause) => BatchError::bad_request(
                    "Input could not be read",
                    format!("The upload failed: {cause}"),
                ),
            });
        }
    };
    if intake.items + intake.invalid + intake.rejected == 0 {
        remove_files(state, &id, true);
        return Err(BatchError::bad_request(
            "Input for the Job is not defined.",
            "The upload holds no records",
        ));
    }

    let now = unix_now();
    let retention = state.limits().job_retention().as_secs();
    let mut meta = Meta {
        id: id.clone(),
        service: plan.service,
        tag: plan.tag,
        billing_tags: plan.billing_tags,
        output_type: plan.output_type,
        output_delimiter: plan.output_delimiter.to_string(),
        output_columns: plan.output_columns,
        output_filter: plan.output_filter,
        created: now,
        started: None,
        items: intake.items,
        invalid: intake.invalid,
        rejected: intake.rejected,
        runs: Vec::new(),
        stopped: false,
        deleted: false,
        expires_at: now + retention,
        request_id: request_id.to_string(),
        frozen: None,
        notification_ids: plan.notification_ids,
    };
    let _guard = LOCK.lock().await;
    if plan.start
        && let Err(error) = launch(state, &mut meta, uuid).await
    {
        remove_files(state, &id, true);
        return Err(error);
    }
    save(state, &meta).map_err(internal)?;
    let mut view = view(state, meta).ok_or_else(BatchError::job_not_found)?;
    // A created job is submitted, whether its start follows or not; a read
    // after tells how far it has run.
    view.status = "submitted";
    let links = Links::of(state, headers, uri);
    Ok((StatusCode::CREATED, axum::Json(job_response(&view, &links))).into_response())
}

fn internal(error: io::Error) -> BatchError {
    BatchError::of(Problem::internal(error.into()))
}

/// Start an og job over the items no earlier run of this job answered.
async fn launch(state: &AppState, meta: &mut Meta, uuid: uuid::Uuid) -> Result<(), BatchError> {
    let answered = {
        let state = state.clone();
        let meta = meta.clone();
        task::spawn_blocking(move || answered(&state, &meta))
            .await
            .map_err(|error| internal(io::Error::other(error)))?
            .map_err(internal)?
    };
    let items = File::open(file(state, &meta.id, "items")).map_err(internal)?;
    let (lines_tx, mut lines_rx) = mpsc::channel::<io::Result<Bytes>>(16);
    task::spawn_blocking(move || feed(items, &answered, &lines_tx));
    let body = Body::from_stream(futures_util::stream::poll_fn(move |context| {
        lines_rx.poll_recv(context)
    }));
    let job = jobs::submit(
        state,
        uuid,
        meta.service.endpoint(),
        body,
        Some(meta.request_id.clone()),
    )
    .await
    .map_err(BatchError::of)?;
    let run = job.snapshot().id;
    {
        let state = state.clone();
        let id = meta.id.clone();
        let run = run.clone();
        tokio::spawn(async move {
            job.ended().await;
            announce_end(&state, &id, &run).await;
        });
    }
    meta.runs.push(run);
    meta.started.get_or_insert_with(unix_now);
    meta.stopped = false;
    Ok(())
}

/// Calls the job's notifications once run `run` has ended, unless a later
/// run took over or the job was deleted meanwhile.
async fn announce_end(state: &AppState, id: &str, run: &str) {
    // Taken so a stop's record is saved before the end is read.
    let view = {
        let _guard = LOCK.lock().await;
        load(state, id).and_then(|meta| view(state, meta))
    };
    let Some(view) = view else {
        return;
    };
    let ended = matches!(view.status, "completed" | "stopped" | "failure");
    if !ended
        || view.meta.notification_ids.is_empty()
        || view.meta.runs.last().map(String::as_str) != Some(run)
    {
        return;
    }
    let facts = notify::Facts {
        id: view.meta.id.clone(),
        name: view.meta.tag.clone().unwrap_or_default(),
        status: view.status,
        output_type: view.meta.output_type.as_str(),
        succeeded: view.records.succeeded,
        failed: view.records.failed,
        total: view.records.total,
        valid: view.records.valid,
        invalid: view.records.invalid,
    };
    notify::deliver(state, &view.meta.notification_ids, &facts);
}

/// The items file, less the lines an earlier run answered, in chunks.
fn feed(items: File, answered: &[u64], lines: &mpsc::Sender<io::Result<Bytes>>) {
    let mut items = BufReader::new(items);
    let mut chunk = Vec::with_capacity(CHUNK_BYTES);
    let mut line = Vec::new();
    let mut ordinal = 0usize;
    loop {
        line.clear();
        match items.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                let _ = lines.blocking_send(Err(error));
                return;
            }
        }
        let done = answered
            .get(ordinal / 64)
            .is_some_and(|word| word & (1 << (ordinal % 64)) != 0);
        ordinal += 1;
        if done {
            continue;
        }
        chunk.extend_from_slice(&line);
        if chunk.len() >= CHUNK_BYTES
            && lines
                .blocking_send(Ok(Bytes::from(std::mem::take(&mut chunk))))
                .is_err()
        {
            return;
        }
    }
    if !chunk.is_empty() {
        let _ = lines.blocking_send(Ok(Bytes::from(chunk)));
    }
}

/// One line of an og job's JSONL results.
#[derive(Debug, Deserialize)]
struct OgLine {
    #[serde(default)]
    id: Option<String>,
    status: u16,
    #[serde(default)]
    response: Option<OgResponse>,
    #[serde(default)]
    error: Option<OgProblem>,
}

#[derive(Debug, Deserialize)]
struct OgResponse {
    features: Vec<Feature>,
}

#[derive(Debug, Deserialize)]
struct OgProblem {
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    invalid_params: Vec<OgParam>,
}

#[derive(Debug, Deserialize)]
struct OgParam {
    name: String,
}

/// What an og item id carries back.
#[derive(Debug, PartialEq)]
struct Carried<'a> {
    ordinal: usize,
    limit: usize,
    radius_m: Option<f64>,
    rec_id: &'a str,
}

fn carried(id: &str) -> Option<Carried<'_>> {
    let mut parts = id.splitn(4, '|');
    let ordinal = parts.next()?.parse().ok()?;
    let limit = parts.next()?.parse().ok()?;
    let radius = parts.next()?;
    let radius_m = if radius.is_empty() {
        None
    } else {
        Some(radius.parse().ok()?)
    };
    Some(Carried {
        ordinal,
        limit,
        radius_m,
        rec_id: parts.next()?,
    })
}

/// Every og line of every run of the job, in the order the runs wrote them,
/// each passed to `each` with what its id carries and where it starts in its
/// run's results.
fn each_line_as_written(
    state: &AppState,
    meta: &Meta,
    mut each: impl FnMut(&Carried<'_>, OgLine, Written) -> io::Result<()>,
) -> io::Result<()> {
    for (run_index, run) in meta.runs.iter().enumerate() {
        let Some(job) = state.jobs().get(run) else {
            continue;
        };
        let mut lines = BufReader::new(job.open_results()?);
        let mut line = String::new();
        let mut offset = 0u64;
        loop {
            line.clear();
            let read = lines.read_line(&mut line)?;
            if read == 0 {
                break;
            }
            let at = Written {
                run: run_index,
                offset,
            };
            offset += read as u64;
            let Ok(parsed) = serde_json::from_str::<OgLine>(&line) else {
                continue;
            };
            let Some(id) = parsed.id.clone() else {
                continue;
            };
            let Some(carried) = carried(&id) else {
                continue;
            };
            each(&carried, parsed, at)?;
        }
    }
    Ok(())
}

/// Where a run wrote an og line.
#[derive(Clone, Copy)]
struct Written {
    run: usize,
    offset: u64,
}

/// Every og line of the job, in the order of the records it answers, each
/// passed to `each` with what its id carries. Runs answer records as they
/// finish, so a first pass notes where each record's line is, at most one
/// place per record, and a second reads them in record order.
fn each_line(
    state: &AppState,
    meta: &Meta,
    mut each: impl FnMut(&Carried<'_>, OgLine) -> io::Result<()>,
) -> io::Result<()> {
    let mut at: Vec<Option<Written>> = vec![None; meta.items];
    each_line_as_written(state, meta, |carried, _, written| {
        if let Some(slot) = at.get_mut(carried.ordinal) {
            slot.get_or_insert(written);
        }
        Ok(())
    })?;
    let mut runs = meta
        .runs
        .iter()
        .map(|run| {
            state
                .jobs()
                .get(run)
                .map(|job| job.open_results().map(BufReader::new))
                .transpose()
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut line = String::new();
    for written in at.into_iter().flatten() {
        let Some(Some(reader)) = runs.get_mut(written.run) else {
            continue;
        };
        reader.seek(SeekFrom::Start(written.offset))?;
        line.clear();
        reader.read_line(&mut line)?;
        let Ok(parsed) = serde_json::from_str::<OgLine>(&line) else {
            continue;
        };
        let Some(id) = parsed.id.clone() else {
            continue;
        };
        let Some(carried) = carried(&id) else {
            continue;
        };
        each(&carried, parsed)?;
    }
    Ok(())
}

/// A bit per item, set for the ones a run has answered.
fn answered(state: &AppState, meta: &Meta) -> io::Result<Vec<u64>> {
    let mut answered = vec![0u64; meta.items.div_ceil(64)];
    each_line_as_written(state, meta, |carried, _, _| {
        if let Some(word) = answered.get_mut(carried.ordinal / 64) {
            *word |= 1 << (carried.ordinal % 64);
        }
        Ok(())
    })?;
    Ok(answered)
}

fn links_for(state: &AppState, headers: &HeaderMap, uri: &axum::http::Uri) -> Links {
    Links::of(state, headers, uri)
}

pub(super) async fn details(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    match find(&state, &id) {
        Ok(view) => {
            axum::Json(job_details(&view, &links_for(&state, &headers, &uri))).into_response()
        }
        Err(error) => error.into_response(&request_id),
    }
}

pub(super) async fn start(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let answer = async {
        let _guard = LOCK.lock().await;
        let current = find(&state, &id)?;
        if !matches!(current.status, "submitted" | "stopped" | "failure") {
            return Ok(current);
        }
        let mut meta = current.meta;
        // The first run takes the job's own UUID, so the og job is found by
        // it; a resumed one needs a fresh one.
        let uuid = match (meta.runs.is_empty(), job_uuid(&meta.id)) {
            (true, Some(uuid)) => uuid,
            _ => uuid::Uuid::new_v4(),
        };
        launch(&state, &mut meta, uuid).await?;
        save(&state, &meta).map_err(internal)?;
        let mut view = view(&state, meta).ok_or_else(BatchError::job_not_found)?;
        view.status = "queued";
        Ok::<_, BatchError>(view)
    };
    match answer.await {
        Ok(view) => {
            axum::Json(job_response(&view, &links_for(&state, &headers, &uri))).into_response()
        }
        Err(error) => error.into_response(&request_id),
    }
}

/// The longest a stop waits for its run to end before it answers.
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) async fn stop(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let answer = async {
        let _guard = LOCK.lock().await;
        let current = find(&state, &id)?;
        if current.status != "inProgress" {
            return Ok(current);
        }
        let mut meta = current.meta;
        meta.stopped = true;
        save(&state, &meta).map_err(internal)?;
        if let Some(job) = meta.runs.last().and_then(|run| state.jobs().get(run)) {
            job.cancel();
            // Await the stop: the final record may already have completed the job.
            let _ = tokio::time::timeout(STOP_WAIT, job.ended()).await;
        }
        view(&state, meta).ok_or_else(BatchError::job_not_found)
    };
    match answer.await {
        Ok(view) => {
            axum::Json(job_response(&view, &links_for(&state, &headers, &uri))).into_response()
        }
        Err(error) => error.into_response(&request_id),
    }
}

/// Delete a job's og jobs and files, keeping its record, which HERE still
/// shows by id.
fn delete_job(state: &AppState, view: View) -> io::Result<View> {
    let mut meta = view.meta;
    for run in &meta.runs {
        state.jobs().remove(run);
    }
    remove_files(state, &meta.id, false);
    meta.frozen = Some(Frozen {
        records: view.records,
        ended: view.ended,
    });
    meta.deleted = true;
    meta.expires_at = unix_now() + state.limits().job_retention().as_secs();
    save(state, &meta)?;
    Ok(View {
        meta,
        status: "deleted",
        ..view
    })
}

pub(super) async fn delete(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let answer = async {
        let _guard = LOCK.lock().await;
        let current = find(&state, &id)?;
        if current.status == "deleted" {
            return Ok(current);
        }
        delete_job(&state, current).map_err(internal)
    };
    match answer.await {
        Ok(view) => {
            axum::Json(job_response(&view, &links_for(&state, &headers, &uri))).into_response()
        }
        Err(error) => error.into_response(&request_id),
    }
}

/// The statuses a list or a bulk delete filters by.
pub(super) const FILTERS: [&str; 8] = [
    "any",
    "submitted",
    "queued",
    "pending",
    "inProgress",
    "completed",
    "stopped",
    "failure",
];

fn status_filter(value: Option<&str>) -> Result<&str, BatchError> {
    let value = value.unwrap_or("any");
    if FILTERS.contains(&value) {
        Ok(value)
    } else {
        Err(BatchError::bad_request(
            "Unsupported filtering status.",
            format!("Actual parameter value: '{value}'"),
        ))
    }
}

fn selected(view: &View, status: &str, tag: Option<&str>) -> bool {
    view.status != "deleted"
        && (status == "any" || view.status == status)
        && tag.is_none_or(|tag| view.meta.tag.as_deref() == Some(tag))
}

pub(super) async fn list(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let answer = async {
        let params = Params::parse(query.as_deref()).map_err(|error| param_error(&error))?;
        let limit = params
            .int("limit", 1, 100)
            .map_err(|error| param_error(&error))?
            .unwrap_or(100);
        let offset = params
            .int("offset", 0, usize::MAX)
            .map_err(|error| param_error(&error))?
            .unwrap_or(0);
        let status = status_filter(params.get("status"))?;
        let tag = params.get("tag");
        let _guard = LOCK.lock().await;
        let links = links_for(&state, &headers, &uri);
        let items = all(&state)
            .iter()
            .filter(|view| selected(view, status, tag))
            .skip(offset)
            .take(limit)
            .map(|view| job_details(view, &links))
            .collect::<Vec<_>>();
        Ok::<_, BatchError>(json!({ "limit": limit, "count": items.len(), "items": items }))
    };
    match answer.await {
        Ok(body) => axum::Json(body).into_response(),
        Err(error) => error.into_response(&request_id),
    }
}

pub(super) async fn delete_many(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
) -> Response {
    let answer = async {
        let params = Params::parse(query.as_deref()).map_err(|error| param_error(&error))?;
        let Some(status) = params.get("status") else {
            return Err(BatchError::bad_request(
                "Missing parameter 'status'",
                "Deleting jobs in bulk takes a status, 'any' for every one",
            ));
        };
        let status = status_filter(Some(status))?;
        let tag = params.get("tag");
        let _guard = LOCK.lock().await;
        let mut ids = Vec::new();
        for view in all(&state) {
            if selected(&view, status, tag) {
                ids.push(view.meta.id.clone());
                delete_job(&state, view).map_err(internal)?;
            }
        }
        Ok(json!({ "total": ids.len(), "jobIds": ids }))
    };
    match answer.await {
        Ok(body) => axum::Json(body).into_response(),
        Err(error) => error.into_response(&request_id),
    }
}

/// HERE's CSV output column names, so `outputColumns` matches them without
/// regard to case and a name HERE does not know is left out.
const COLUMNS: [&str; 46] = [
    "title",
    "id",
    "resultType",
    "houseNumberType",
    "addressBlockType",
    "localityType",
    "administrativeAreaType",
    "addressLabel",
    "addressCountryCode",
    "addressCountryName",
    "addressStateCode",
    "addressState",
    "addressCountyCode",
    "addressCounty",
    "addressCity",
    "addressPostalCity",
    "addressDistrict",
    "addressSubdistrict",
    "addressStreet",
    "addressStreets",
    "addressBlock",
    "addressSubblock",
    "addressPostalCode",
    "addressHouseNumber",
    "addressBuilding",
    "addressUnit",
    "position",
    "positionLat",
    "positionLng",
    "access",
    "accessLat",
    "accessLng",
    "distance",
    "mapView",
    "mapViewWest",
    "mapViewSouth",
    "mapViewEast",
    "mapViewNorth",
    "houseNumberFallback",
    "estimatedPointAddress",
    "scoringQueryScore",
    "politicalView",
    "timeZoneName",
    "timeZoneUtcOffset",
    "categories",
    "foodTypes",
];

/// The always-first columns.
const STANDARD: [&str; 3] = ["recId", "seqNumber", "seqLength"];

fn output_columns(meta: &Meta) -> Vec<&'static str> {
    let asked = match &meta.output_columns {
        Some(asked) => asked
            .iter()
            .filter_map(|name| {
                COLUMNS
                    .iter()
                    .find(|column| column.eq_ignore_ascii_case(name))
                    .copied()
            })
            .collect::<Vec<_>>(),
        None => meta.service.default_columns().to_vec(),
    };
    let mut columns = Vec::with_capacity(asked.len());
    for column in asked {
        if !columns.contains(&column) {
            columns.push(column);
        }
    }
    columns
}

/// An item's value in a CSV column, in HERE's renderings.
fn cell_of(item: &Item, column: &str) -> String {
    let address = &item.address;
    let text = |value: &Option<String>| value.clone().unwrap_or_default();
    let number = |value: Option<f64>| value.map(|value| value.to_string()).unwrap_or_default();
    match column {
        "title" => item.title.clone(),
        "id" => item.id.clone(),
        "resultType" => item.result_type.to_string(),
        "houseNumberType" => item.house_number_type.unwrap_or_default().to_string(),
        "localityType" => item.locality_type.unwrap_or_default().to_string(),
        "administrativeAreaType" => item
            .administrative_area_type
            .unwrap_or_default()
            .to_string(),
        "addressLabel" => text(&address.label),
        "addressCountryCode" => address.country_code.unwrap_or_default().to_string(),
        "addressCountryName" => text(&address.country_name),
        "addressStateCode" => text(&address.state_code),
        "addressState" => text(&address.state),
        "addressCounty" => text(&address.county),
        "addressCity" => text(&address.city),
        "addressDistrict" => text(&address.district),
        "addressSubdistrict" => text(&address.subdistrict),
        "addressStreet" => text(&address.street),
        "addressPostalCode" => text(&address.postal_code),
        "addressHouseNumber" => text(&address.house_number),
        "addressUnit" => text(&address.unit),
        "position" => item
            .position
            .map(|position| format!("({},{})", position.lat, position.lng))
            .unwrap_or_default(),
        "positionLat" => number(item.position.map(|position| position.lat)),
        "positionLng" => number(item.position.map(|position| position.lng)),
        "distance" => item
            .distance
            .map(|distance| distance.to_string())
            .unwrap_or_default(),
        "access" => item
            .access
            .iter()
            .map(|point| format!("({},{})", point.lat, point.lng))
            .collect::<Vec<_>>()
            .join(","),
        "accessLat" => joined(item.access.iter().map(|point| point.lat)),
        "accessLng" => joined(item.access.iter().map(|point| point.lng)),
        "mapView" => item
            .map_view
            .map(|view| {
                format!(
                    "({},{});({},{})",
                    view.south, view.west, view.north, view.east
                )
            })
            .unwrap_or_default(),
        "mapViewWest" => number(item.map_view.map(|view| view.west)),
        "mapViewSouth" => number(item.map_view.map(|view| view.south)),
        "mapViewEast" => number(item.map_view.map(|view| view.east)),
        "mapViewNorth" => number(item.map_view.map(|view| view.north)),
        "categories" => item
            .categories
            .iter()
            .map(|category| category.id)
            .collect::<Vec<_>>()
            .join(","),
        "foodTypes" => item
            .food_types
            .iter()
            .map(|food| food.id)
            .collect::<Vec<_>>()
            .join(","),
        "timeZoneName" => item
            .time_zone
            .as_ref()
            .map(|zone| zone.name.clone())
            .unwrap_or_default(),
        "timeZoneUtcOffset" => item
            .time_zone
            .as_ref()
            .map(|zone| zone.utc_offset.clone())
            .unwrap_or_default(),
        "scoringQueryScore" => item
            .scoring
            .as_ref()
            .map(|scoring| scoring.query_score.to_string())
            .unwrap_or_default(),
        // A column HERE has and og never fills.
        _ => String::new(),
    }
}

/// An array cell, its values joined by commas as HERE renders one.
fn joined(values: impl Iterator<Item = f64>) -> String {
    values
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// A cell as written, quoted when it holds the delimiter, a quote or a line
/// break, its quotes doubled.
fn quoted(value: &str, delimiter: &str) -> String {
    if value.contains(delimiter) || value.contains(['"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// Keep only the paths asked, each `a.b.c`, through arrays to every element.
fn project(value: &Value, paths: &[Vec<&str>]) -> Value {
    match value {
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| project(item, paths)).collect())
        }
        Value::Object(object) => {
            let mut kept = serde_json::Map::new();
            for (key, inner) in object {
                let deeper = paths
                    .iter()
                    .filter(|path| path.first() == Some(&key.as_str()))
                    .map(|path| path[1..].to_vec())
                    .collect::<Vec<_>>();
                if deeper.is_empty() {
                    continue;
                }
                if deeper.iter().any(Vec::is_empty) {
                    kept.insert(key.clone(), inner.clone());
                } else {
                    kept.insert(key.clone(), project(inner, &deeper));
                }
            }
            Value::Object(kept)
        }
        other => other.clone(),
    }
}

/// The HERE items one og line answered.
fn line_items(meta: &Meta, carried: &Carried<'_>, response: &OgResponse) -> Vec<Item> {
    match meta.service {
        // A column asks for the time zone, so every item carries it.
        Service::Geocode => response
            .features
            .iter()
            .map(|feature| item(feature).with_time_zone(feature, true))
            .take(carried.limit)
            .collect(),
        Service::Revgeocode => reverse_items(
            &response.features,
            carried.radius_m,
            None,
            carried.limit,
            true,
        ),
    }
}

/// Write the job's results in HERE's format, a record at a time.
fn write_results(state: &AppState, meta: &Meta, out: &mut impl Write) -> io::Result<()> {
    let delimiter = meta.output_delimiter.as_str();
    let columns = output_columns(meta);
    let filter = meta.output_filter.as_ref().map(|paths| {
        paths
            .iter()
            .map(|path| path.split('.').collect::<Vec<_>>())
            .collect::<Vec<_>>()
    });
    if meta.output_type == OutputType::Csv {
        let header = STANDARD
            .iter()
            .chain(columns.iter())
            .copied()
            .collect::<Vec<_>>()
            .join(delimiter);
        writeln!(out, "{header}")?;
    }
    each_line(state, meta, |carried, line| {
        let Some(response) = line.response.as_ref().filter(|_| line.status == 200) else {
            return Ok(());
        };
        let items = line_items(meta, carried, response);
        match meta.output_type {
            OutputType::Jsonl => {
                let results = items
                    .iter()
                    .map(|item| {
                        let value = serde_json::to_value(item).unwrap_or(Value::Null);
                        match &filter {
                            Some(paths) => project(&value, paths),
                            None => value,
                        }
                    })
                    .collect::<Vec<_>>();
                serde_json::to_writer(
                    &mut *out,
                    &json!({ "recId": carried.rec_id, "results": results }),
                )?;
                out.write_all(b"\n")
            }
            OutputType::Csv => {
                let rec_id = quoted(carried.rec_id, delimiter);
                if items.is_empty() {
                    let empty = delimiter.repeat(columns.len());
                    return writeln!(out, "{rec_id}{delimiter}1{delimiter}1{empty}");
                }
                for (index, item) in items.iter().enumerate() {
                    write!(
                        out,
                        "{rec_id}{delimiter}{}{delimiter}{}",
                        index + 1,
                        items.len()
                    )?;
                    for column in &columns {
                        write!(
                            out,
                            "{delimiter}{}",
                            quoted(&cell_of(item, column), delimiter)
                        )?;
                    }
                    out.write_all(b"\n")?;
                }
                Ok(())
            }
        }
    })
}

/// Write the job's error report, the records refused on upload, then the
/// lookups og failed.
fn write_errors(state: &AppState, meta: &Meta, out: &mut impl Write) -> io::Result<()> {
    let delimiter = meta.output_delimiter.as_str();
    writeln!(out, "recId{delimiter}errorCode{delimiter}errorMessage")?;
    if let Ok(rejects) = File::open(file(state, &meta.id, "rejects")) {
        for line in BufReader::new(rejects).lines() {
            let line = line?;
            let Ok(reject) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            writeln!(
                out,
                "{}{delimiter}{}{delimiter}{}",
                quoted(reject["recId"].as_str().unwrap_or_default(), delimiter),
                reject["code"],
                // A job written before the message was kept as text holds
                // it as an object.
                reject["message"]
                    .as_str()
                    .map_or_else(|| reject["message"].to_string(), str::to_string),
            )?;
        }
    }
    each_line(state, meta, |carried, line| {
        let Some(problem) = line.error.as_ref().filter(|_| line.status != 200) else {
            return Ok(());
        };
        let error = Gs7Error::of_parts(
            line.status,
            problem
                .invalid_params
                .first()
                .map(|param| param.name.as_str()),
            problem.error_code.as_deref().unwrap_or_default(),
            problem.detail.as_deref(),
        );
        let request_id = problem.request_id.as_deref().unwrap_or(&meta.request_id);
        writeln!(
            out,
            "{}{delimiter}{}{delimiter}{}",
            quoted(carried.rec_id, delimiter),
            line.status,
            error.body_text(&format!("{request_id}-{}", carried.ordinal + 1)),
        )
    })
}

/// A file written by `write` on a blocking thread, streamed out as written.
fn download(
    state: &AppState,
    meta: Meta,
    name: &str,
    write: fn(&AppState, &Meta, &mut ChunkWriter) -> io::Result<()>,
) -> Response {
    let (chunks_tx, mut chunks_rx) = mpsc::channel::<io::Result<Bytes>>(16);
    let state = state.clone();
    task::spawn_blocking(move || {
        let mut out = ChunkWriter {
            chunk: Vec::with_capacity(CHUNK_BYTES),
            chunks: chunks_tx,
        };
        let written = write(&state, &meta, &mut out).and_then(|()| out.flush());
        if let Err(error) = written {
            let _ = out.chunks.blocking_send(Err(error));
        }
    });
    let body = Body::from_stream(futures_util::stream::poll_fn(move |context| {
        chunks_rx.poll_recv(context)
    }));
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"));
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("text/plain")),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        body,
    )
        .into_response()
}

/// A [`Write`] that sends what is written as body chunks.
#[derive(Debug)]
struct ChunkWriter {
    chunk: Vec<u8>,
    chunks: mpsc::Sender<io::Result<Bytes>>,
}

impl Write for ChunkWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.chunk.extend_from_slice(bytes);
        if self.chunk.len() >= CHUNK_BYTES {
            self.flush()?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.chunk.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::take(&mut self.chunk));
        self.chunks
            .blocking_send(Ok(chunk))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the download was dropped"))
    }
}

pub(super) async fn results(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let view = match find(&state, &id) {
        Ok(view) => view,
        Err(error) => return error.into_response(&request_id),
    };
    if !view.results_ready() {
        return BatchError::new(
            StatusCode::NOT_FOUND,
            "E6004042",
            "Results unavailable",
            if view.status == "failure" {
                "Results for a failed Job is not retrievable."
            } else {
                "The results of the job are unavailable at this time"
            },
            "Check that the job has completed and try again",
        )
        .into_response(&request_id);
    }
    if view.records.succeeded == 0 {
        return StatusCode::NO_CONTENT.into_response();
    }
    let name = format!("{}-results.txt", view.meta.id);
    download(&state, view.meta, &name, |state, meta, out| {
        write_results(state, meta, out)
    })
}

pub(super) async fn errors(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let view = match find(&state, &id) {
        Ok(view) => view,
        Err(error) => return error.into_response(&request_id),
    };
    if !view.errors_ready() {
        return BatchError::new(
            StatusCode::NOT_FOUND,
            "E6004044",
            "Errors for job is unavailable.",
            "The errors of the job are unavailable at this time",
            "Check that the job has ended and try again",
        )
        .into_response(&request_id);
    }
    if view.errors() == 0 {
        return StatusCode::NO_CONTENT.into_response();
    }
    let name = format!("{}-errors.txt", view.meta.id);
    download(&state, view.meta, &name, |state, meta, out| {
        write_errors(state, meta, out)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_rfc_3339_in_utc() {
        assert_eq!(date_time(0), "1970-01-01T00:00:00Z");
        assert_eq!(date_time(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(date_time(1_754_326_116), "2025-08-04T16:48:36Z");
    }

    #[test]
    fn cells_follow_here_quoting() {
        assert_eq!(cells("a|b|c", '|'), ["a", "b", "c"]);
        assert_eq!(cells("1|\"x|y\"|z", '|'), ["1", "x|y", "z"]);
        assert_eq!(cells("1|ab\"c|d\"e", '|'), ["1", "abc|de"]);
        assert_eq!(cells("1|\"say \"\"hi\"\"\"", '|'), ["1", "say \"hi\""]);
        assert_eq!(cells("", '|'), [""]);
    }

    #[test]
    fn header_cells_give_defaults() {
        assert_eq!(
            columns(vec!["recId".into(), "Q".into(), "country=USA".into()]),
            [
                Column {
                    name: "recid".into(),
                    default: None
                },
                Column {
                    name: "q".into(),
                    default: None
                },
                Column {
                    name: "country".into(),
                    default: Some("USA".into())
                },
            ]
        );
    }

    #[test]
    fn og_ids_carry_the_record_back() {
        assert_eq!(
            carried("7|3|250|rec-1"),
            Some(Carried {
                ordinal: 7,
                limit: 3,
                radius_m: Some(250.0),
                rec_id: "rec-1",
            })
        );
        assert_eq!(
            carried("0|1||a.b,c"),
            Some(Carried {
                ordinal: 0,
                limit: 1,
                radius_m: None,
                rec_id: "a.b,c",
            })
        );
        assert_eq!(carried("x|1||a"), None);
        assert_eq!(carried("1|1|"), None);
    }

    #[test]
    fn delimiters_and_billing_tags_follow_here_rules() {
        assert_eq!(delimiter(None, true).unwrap(), '|');
        assert_eq!(delimiter(Some("\\t"), false).unwrap(), '\t');
        assert_eq!(delimiter(Some(";"), true).unwrap(), ';');
        assert_eq!(delimiter(Some("."), true).unwrap_err().code, "E6004005");
        assert_eq!(delimiter(Some("."), false).unwrap(), '.');
        assert_eq!(delimiter(Some("||"), false).unwrap_err().code, "E6004005");
        assert_eq!(billing_tags(Some("abcd+efgh")).unwrap(), ["abcd", "efgh"]);
        assert_eq!(billing_tags(Some("abcd efgh")).unwrap(), ["abcd", "efgh"]);
        assert_eq!(billing_tags(Some("abcd,e.fg")).unwrap(), ["abcd", "e.fg"]);
        assert_eq!(
            billing_tags(Some("abcd+efgh,ijkl")).unwrap_err().code,
            "E6004001"
        );
        assert_eq!(billing_tags(Some("abc")).unwrap_err().code, "E6004001");
        assert_eq!(billing_tags(Some("-abcd")).unwrap_err().code, "E6004001");
        assert_eq!(
            billing_tags(Some("abcd,abcd")).ok(),
            Some(vec!["abcd".to_string()]),
            "a tag given twice is taken once"
        );
    }

    #[test]
    fn projection_keeps_the_paths_asked() {
        let value = json!({
            "title": "T",
            "id": "I",
            "address": {"label": "L", "city": "C"},
            "access": [{"lat": 1, "lng": 2}, {"lat": 3, "lng": 4}],
        });
        let paths = [
            vec!["title"],
            vec!["address", "label"],
            vec!["access", "lat"],
        ];
        assert_eq!(
            project(&value, &paths),
            json!({
                "title": "T",
                "address": {"label": "L"},
                "access": [{"lat": 1}, {"lat": 3}],
            })
        );
    }

    fn meta(stopped: bool, deleted: bool) -> Meta {
        Meta {
            id: "JOB-3fa85f64-5717-4562-b3fc-2c963f66afa6".into(),
            service: Service::Geocode,
            tag: None,
            billing_tags: Vec::new(),
            output_type: OutputType::Csv,
            output_delimiter: "|".into(),
            output_columns: None,
            output_filter: None,
            created: 0,
            started: None,
            items: 3,
            invalid: 0,
            rejected: 0,
            runs: Vec::new(),
            stopped,
            deleted,
            expires_at: u64::MAX,
            request_id: "rid".into(),
            frozen: None,
            notification_ids: Vec::new(),
        }
    }

    #[test]
    fn og_job_states_are_here_statuses() {
        let cases = [
            (false, false, None, "submitted"),
            (false, false, Some(Status::Running), "inProgress"),
            (true, false, Some(Status::Running), "inProgress"),
            (false, false, Some(Status::Completed), "completed"),
            (true, false, Some(Status::Completed), "completed"),
            (true, false, Some(Status::Cancelled), "stopped"),
            (false, false, Some(Status::Cancelled), "failure"),
            (false, false, Some(Status::Failed), "failure"),
            (false, true, None, "deleted"),
            (true, true, Some(Status::Cancelled), "deleted"),
        ];
        for (stopped, deleted, last, status) in cases {
            assert_eq!(
                here_status(&meta(stopped, deleted), last),
                status,
                "{stopped} {deleted} {last:?}"
            );
        }
    }

    #[test]
    fn errors_written_as_objects_still_download_as_json() {
        use super::super::tests::pack;
        use crate::limits::Limits;

        let pack = pack("saved-batch-errors");
        let state = pack.state(Limits::DEFAULT);
        let meta = meta(false, false);
        fs::create_dir_all(store_dir(&state)).expect("HERE job store");
        fs::write(
            file(&state, &meta.id, "rejects"),
            r#"{"recId":"1","code":400,"message":{"status":400,"title":"Illegal input"}}"#,
        )
        .expect("saved reject");

        let mut output = Vec::new();
        write_errors(&state, &meta, &mut output).expect("error report");
        assert_eq!(
            std::str::from_utf8(&output).expect("text"),
            "recId|errorCode|errorMessage\n1|400|{\"status\":400,\"title\":\"Illegal input\"}\n"
        );
    }

    #[test]
    fn a_resumed_run_reads_only_what_no_run_answered() {
        let dir = std::env::temp_dir().join(format!("og-here-feed-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("items");
        let lines = (0..70).map(|ordinal| format!("{{\"id\":\"{ordinal}\"}}\n"));
        fs::write(&path, lines.collect::<String>()).expect("items");
        let answered = [0b101, 0b10];
        let (sender, mut receiver) = mpsc::channel(16);
        feed(File::open(&path).expect("items"), &answered, &sender);
        drop(sender);
        let mut fed = Vec::new();
        while let Some(chunk) = receiver.blocking_recv() {
            fed.extend_from_slice(&chunk.expect("chunk"));
        }
        let _ = fs::remove_dir_all(&dir);
        let fed = String::from_utf8(fed).expect("UTF-8");
        let ids = fed
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("line")["id"].clone())
            .collect::<Vec<_>>();
        let expected = (0..70)
            .filter(|ordinal| ![0, 2, 65].contains(ordinal))
            .map(|ordinal| Value::String(ordinal.to_string()))
            .collect::<Vec<_>>();
        assert_eq!(ids, expected);
    }

    #[tokio::test]
    async fn a_finished_run_answered_every_item() {
        use super::super::tests::{finished_job, pack};

        let pack = pack("batch-answered");
        let state = pack.state(crate::limits::Limits::DEFAULT);
        let router = crate::runtime::build_router(state.clone(), None, None);
        let id = finished_job(&router, "recId|q\n1|10 King Street\n2|Queen\n3|zzzz\n").await;
        let meta = load(&state, &id).expect("meta");
        assert_eq!(meta.items, 3);
        assert_eq!(answered(&state, &meta).expect("answered"), [0b111]);
    }

    #[tokio::test]
    async fn results_follow_the_records_whatever_order_they_finished_in() {
        use super::super::tests::{finished_job, get, pack};

        let pack = pack("batch-order");
        let state = pack.state(crate::limits::Limits::DEFAULT);
        let router = crate::runtime::build_router(state.clone(), None, None);
        let id = finished_job(&router, "recId|q\n1|10 King Street\n2|Queen\n3|zzzz\n").await;
        let meta = load(&state, &id).expect("meta");
        for run in &meta.runs {
            let job = state.jobs().get(run).expect("run");
            let path = job.results_path();
            let written = fs::read_to_string(path).expect("results");
            let mut lines = written.lines().collect::<Vec<_>>();
            let row =
                |line: &&str| serde_json::from_str::<Value>(line).expect("line")["row"].as_u64();
            lines.sort_by_key(|line| match row(line) {
                Some(2) => 0,
                Some(0) => 1,
                _ => 2,
            });
            let finished = lines.iter().map(|line| format!("{line}\n"));
            fs::write(path, finished.collect::<String>()).expect("rewrite");
        }
        let results = get(&router, &format!("/compat/here/v7/batch/jobs/{id}/results")).await;
        let rec_ids = results
            .text()
            .lines()
            .skip(1)
            .map(|line| line.split('|').next().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        assert_eq!(rec_ids, ["1", "2", "3"], "{}", results.text());
    }

    #[test]
    fn revgeocode_reads_a_bare_circle() {
        let record = Record(vec![("in".into(), "52.5,13.3,250".into())]);
        let translated = translate(Service::Revgeocode, &record).unwrap();
        assert_eq!(
            translated.fields,
            [("lon", "13.3".to_string()), ("lat", "52.5".to_string())]
        );
        assert_eq!(translated.radius_m, Some(250));
        assert_eq!(translated.limit, 1);
    }
}
