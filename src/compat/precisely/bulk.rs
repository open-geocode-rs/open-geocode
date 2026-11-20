//! Bulk Geo Addressing v2 on og's `/jobs`.
//!
//! Precisely's flow, on og's own URLs. An upload URL that points back at og,
//! a `PUT` of the CSV to it, a submit that starts an og job, polling, and a
//! download. The uploaded file and the job's bookkeeping live in the job
//! store's directory and expire with its jobs.
//!
//! The CSV streams into the job as one JSONL item a row, so each row carries
//! its own country and id; the job's JSONL results stream back out as
//! Precisely's output CSV. Neither file is ever held in memory whole.

use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    body::Bytes,
    extract::{Path as UrlPath, RawQuery, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use csv::{ByteRecord, ReaderBuilder, WriterBuilder};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{sync::mpsc, task};

use super::{
    Failure, INTERNAL_MESSAGE, Preferences, base_url, channel_body, error, given, read_json,
    realtime::InputAddress,
    result::{Feature, Product},
};
use crate::{
    compat::VendorError,
    http::{
        bounds::UPLOAD_STALL_SECONDS,
        jobs::{self, Status},
    },
    runtime::AppState,
};

/// Bytes handed on at a time, to a job's upload or a download.
const CHUNK_BYTES: usize = 64 << 10;

const INVALID_FILE: &str = "Invalid File Id (DIS-LI-1052)";
const INVALID_JOB: &str = "Invalid Job Id (DIS-LI-1051)";
const INVALID_CLIENT_INPUT: &str = "Request is not valid (DIS-LI-1018)";

/// An upload, as its record on disk keeps it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredFile {
    file_id: String,
    file_name: String,
    /// Unix seconds.
    created_at: u64,
}

/// A job started here, og's job and the upload it read.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredJob {
    job_id: String,
    file_id: String,
    file_name: String,
    /// Unix milliseconds.
    submitted_at: u64,
    /// Absent on a job stored before bulk verify, which was a geocode.
    #[serde(default)]
    product: Product,
}

pub(super) fn now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// Inside og's job directory, so uploads and jobs expire together and one
/// setting moves both.
fn root(state: &AppState) -> PathBuf {
    state.jobs().dir().join("precisely")
}

fn files_dir(state: &AppState) -> PathBuf {
    root(state).join("files")
}

fn jobs_dir(state: &AppState) -> PathBuf {
    root(state).join("jobs")
}

/// A file id as og issued it. Being a UUID also makes it safe as a file
/// name.
fn file_id(raw: &str) -> Option<String> {
    uuid::Uuid::try_parse(raw.trim())
        .ok()
        .map(|id| id.hyphenated().to_string())
}

/// og's job ids are UUIDs written without hyphens.
fn job_id(raw: &str) -> Option<String> {
    uuid::Uuid::try_parse(raw.trim())
        .ok()
        .map(|id| id.simple().to_string())
}

fn read_record<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Written aside and renamed into place, so a reader never sees half a
/// record.
fn write_record<T: Serialize>(path: &Path, record: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let aside = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    fs::write(&aside, serde_json::to_vec(record)?)?;
    fs::rename(&aside, path).inspect_err(|_| {
        let _ = fs::remove_file(&aside);
    })
}

fn internal(cause: io::Error) -> VendorError {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "INTERNAL_ERROR",
        INTERNAL_MESSAGE,
    )
    .caused_by(crate::http::problem::Problem::internal(cause.into()))
}

/// The upload `id` names, unless it expired.
fn stored_file(state: &AppState, id: &str) -> Option<StoredFile> {
    let id = file_id(id)?;
    let file = read_record::<StoredFile>(&files_dir(state).join(format!("{id}.json")))?;
    let retention = state.limits().job_retention().as_secs();
    (file.created_at.saturating_add(retention) > now().as_secs()).then_some(file)
}

fn content_path(state: &AppState, id: &str) -> PathBuf {
    files_dir(state).join(format!("{id}.csv"))
}

/// The job `id` names, if this module started it and og still keeps it.
fn stored_job(state: &AppState, id: &str) -> Option<(StoredJob, jobs::JobStatus)> {
    let id = job_id(id)?;
    let path = jobs_dir(state).join(format!("{id}.json"));
    let job = read_record::<StoredJob>(&path)?;
    match state.jobs().status(&id) {
        Some(status) => Some((job, status)),
        None => {
            let _ = fs::remove_file(path);
            None
        }
    }
}

/// Delete uploads past the job store's retention, and the records of jobs
/// og no longer keeps.
pub(crate) fn sweep(state: &AppState) {
    let retention = state.limits().job_retention().as_secs();
    let now = now().as_secs();
    // Every file is named for its id first. The record, its content, and a
    // write in progress or cut off by a crash all go with the record.
    let id_of = |path: &Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.split('.').next())
            .map(str::to_string)
            .unwrap_or_default()
    };
    let files = files_dir(state);
    if let Ok(entries) = fs::read_dir(&files) {
        for entry in entries.flatten() {
            let path = entry.path();
            let record = files.join(format!("{}.json", id_of(&path)));
            let expired = read_record::<StoredFile>(&record)
                .is_none_or(|file| file.created_at.saturating_add(retention) <= now);
            if expired {
                let _ = fs::remove_file(&path);
            }
        }
    }
    if let Ok(entries) = fs::read_dir(jobs_dir(state)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if state.jobs().status(&id_of(&path)).is_none() {
                let _ = fs::remove_file(path);
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadUrlRequest {
    file_name: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UploadUrl {
    file_id: String,
    url: String,
    message: &'static str,
}

/// Longest file name kept. It is a label, never a path.
const MAX_FILE_NAME_CHARS: usize = 255;

pub(super) async fn upload_url(
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<UploadUrl>, VendorError> {
    let base = base_url(&state, request.headers());
    let body = read_json::<UploadUrlRequest>(&state, request, "INVALID_CLIENT_INPUT").await?;
    let file_name = given(body.file_name.as_deref())
        .filter(|name| name.chars().count() <= MAX_FILE_NAME_CHARS)
        .ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                "INVALID_CLIENT_INPUT",
                INVALID_CLIENT_INPUT,
            )
        })?;
    let id = uuid::Uuid::new_v4().hyphenated().to_string();
    let file = StoredFile {
        file_id: id.clone(),
        file_name: file_name.to_string(),
        created_at: now().as_secs(),
    };
    write_record(&files_dir(&state).join(format!("{id}.json")), &file).map_err(internal)?;
    Ok(Json(UploadUrl {
        url: format!("{base}/v2/files/{id}/content"),
        file_id: id,
        message: "This upload URL will be valid until the file expires.",
    }))
}

/// A write in progress, removed unless it is renamed into place.
#[derive(Debug)]
struct Partial(PathBuf);

impl Drop for Partial {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// The upload URL's `PUT`, the file's bytes written as they arrive.
pub(super) async fn put_content(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    request: Request,
) -> Result<StatusCode, VendorError> {
    let invalid = || error(StatusCode::BAD_REQUEST, "INVALID_REQUEST", INVALID_FILE);
    let file = stored_file(&state, &id).ok_or_else(invalid)?;
    let max_bytes = state.limits().max_job_upload_bytes();
    let too_large = || {
        error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "INVALID_REQUEST",
            format!("The file is over {max_bytes} bytes"),
        )
    };
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|length| length > max_bytes) {
        return Err(too_large());
    }
    let dir = files_dir(&state);
    if stored_bytes(&dir) >= state.limits().max_job_store_bytes() {
        let full = crate::http::problem::Problem::busy(60);
        return Err(Failure::of(&full, None).into_error(full));
    }
    fs::create_dir_all(&dir).map_err(internal)?;
    let partial = Partial(dir.join(format!(
        "{}.{}.part",
        file.file_id,
        uuid::Uuid::new_v4().simple()
    )));
    let (chunks_tx, mut chunks_rx) = mpsc::channel::<Bytes>(16);
    let path = partial.0.clone();
    let writer = task::spawn_blocking(move || -> io::Result<()> {
        let mut out = io::BufWriter::new(File::create(path)?);
        while let Some(chunk) = chunks_rx.blocking_recv() {
            out.write_all(&chunk)?;
        }
        out.into_inner()
            .map_err(io::IntoInnerError::into_error)?
            .sync_all()
    });
    let mut stream = request.into_body().into_data_stream();
    let mut received = 0u64;
    let stall = Duration::from_secs(UPLOAD_STALL_SECONDS);
    let read = loop {
        let chunk = match tokio::time::timeout(stall, stream.next()).await {
            Err(_) => {
                break Err(error(
                    StatusCode::REQUEST_TIMEOUT,
                    "REQUEST_TIMEOUT",
                    format!("No data for {UPLOAD_STALL_SECONDS} s"),
                ));
            }
            Ok(None) => break Ok(()),
            Ok(Some(Err(_))) => {
                break Err(error(
                    StatusCode::BAD_REQUEST,
                    "INVALID_REQUEST",
                    "The upload broke off",
                ));
            }
            Ok(Some(Ok(chunk))) => chunk,
        };
        received += chunk.len() as u64;
        if received > max_bytes {
            break Err(too_large());
        }
        if chunks_tx.send(chunk).await.is_err() {
            break Ok(());
        }
    };
    drop(chunks_tx);
    let written = writer
        .await
        .unwrap_or_else(|join| Err(io::Error::other(join)));
    read?;
    written.map_err(internal)?;
    fs::rename(&partial.0, content_path(&state, &file.file_id)).map_err(internal)?;
    Ok(StatusCode::OK)
}

/// Bytes the uploads take, so they cannot fill the disk og's job store is
/// capped on.
fn stored_bytes(dir: &Path) -> u64 {
    fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .filter_map(|entry| entry.metadata().ok())
            .map(|meta| meta.len())
            .sum()
    })
}

#[derive(Debug, Serialize)]
pub(super) struct InputFiles {
    results: Vec<InputFile>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InputFile {
    file_id: String,
    file_name: String,
}

/// Uploaded files, oldest first. One whose upload URL was never used is
/// not one yet.
pub(super) async fn list_files(State(state): State<AppState>) -> Json<InputFiles> {
    let dir = files_dir(&state);
    let mut files = fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name();
                    let id = name.to_str()?.strip_suffix(".json")?;
                    let file = stored_file(&state, id)?;
                    content_path(&state, &file.file_id).exists().then_some(file)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    files.sort_by(|left, right| {
        (left.created_at, &left.file_id).cmp(&(right.created_at, &right.file_id))
    });
    Json(InputFiles {
        results: files
            .into_iter()
            .map(|file| InputFile {
                file_id: file.file_id,
                file_name: file.file_name,
            })
            .collect(),
    })
}

#[derive(Debug, Serialize)]
pub(super) struct Deleted {
    status: &'static str,
    message: &'static str,
}

pub(super) async fn delete_file(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Deleted>, VendorError> {
    let file = stored_file(&state, &id)
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "INVALID_REQUEST", INVALID_FILE))?;
    // A job reading the file keeps its open handle.
    let _ = fs::remove_file(content_path(&state, &file.file_id));
    fs::remove_file(files_dir(&state).join(format!("{}.json", file.file_id))).map_err(internal)?;
    Ok(Json(Deleted {
        status: "SUCCESS",
        message: "File Deleted Successfully",
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmitRequest {
    file_id: Option<String>,
    delimiter: Option<String>,
    #[serde(default)]
    preferences: Preferences,
    notify_emails: Option<Vec<Option<String>>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Submitted {
    status_url: String,
}

/// Precisely's default when a job names no delimiter.
const DEFAULT_DELIMITER: u8 = b'|';
const MAX_NOTIFY_EMAILS: usize = 5;

fn delimiter(value: Option<&str>) -> Result<u8, VendorError> {
    match value.unwrap_or_default() {
        "" => Ok(DEFAULT_DELIMITER),
        "," => Ok(b','),
        ";" => Ok(b';'),
        "|" => Ok(b'|'),
        "\t" => Ok(b'\t'),
        _ => Err(error(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            r"Delimiter must be one of the following: ',', ';', '\t', or '|'",
        )),
    }
}

/// `notifyEmails` is checked as Precisely checks it, though og sends no
/// mail, so a job Precisely would refuse is refused here too.
fn check_emails(emails: Option<&[Option<String>]>) -> Result<(), VendorError> {
    let Some(emails) = emails else {
        return Ok(());
    };
    let refuse = |message: &str| error(StatusCode::BAD_REQUEST, "INVALID_REQUEST", message);
    if emails.len() > MAX_NOTIFY_EMAILS {
        return Err(refuse(
            "notifyEmails cannot contain more than 5 email addresses",
        ));
    }
    for email in emails {
        let Some(email) = given(email.as_deref()) else {
            return Err(refuse(
                "notifyEmails cannot contain a null or empty email address",
            ));
        };
        let valid = email.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty() && domain.contains('.') && !domain.contains('@')
        });
        if !valid {
            return Err(refuse("notifyEmails contains an invalid email address"));
        }
    }
    Ok(())
}

/// The columns a row is read from. The three Precisely requires, and the
/// address parts its utility maps a file's own columns to.
#[derive(Debug, Clone, Copy)]
struct Columns {
    address_id: usize,
    address: usize,
    country: usize,
    city: Option<usize>,
    region: Option<usize>,
    postcode: Option<usize>,
}

const BYTE_ORDER_MARK: &[u8] = "\u{feff}".as_bytes();

/// Read the header row, leaving `reader` at the first row.
fn read_header(reader: &mut csv::Reader<File>) -> Result<Columns, VendorError> {
    let missing = || {
        error(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "Mandatory input headers are missing in input file or the file is not UTF-8 \
             encoded (DIS-LI-1053)",
        )
    };
    let mut header = ByteRecord::new();
    if !reader.read_byte_record(&mut header).unwrap_or(false) {
        return Err(missing());
    }
    let mut names = Vec::with_capacity(header.len());
    for (index, name) in header.iter().enumerate() {
        let name = if index == 0 {
            name.strip_prefix(BYTE_ORDER_MARK).unwrap_or(name)
        } else {
            name
        };
        let name = std::str::from_utf8(name).map_err(|_| missing())?;
        names.push(name.trim().to_ascii_lowercase());
    }
    let mut seen = HashSet::new();
    if names
        .iter()
        .any(|name| !name.is_empty() && !seen.insert(name))
    {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "Duplicate input headers found (DIS-LI-1054)",
        ));
    }
    let find = |wanted: &str| names.iter().position(|name| name == wanted);
    Ok(Columns {
        address_id: find("addressid").ok_or_else(missing)?,
        address: find("address").ok_or_else(missing)?,
        country: find("country").ok_or_else(missing)?,
        city: find("city"),
        region: find("admin1").or_else(|| find("state")),
        postcode: find("postalcode"),
    })
}

/// Feed the upload's rows to the job as JSONL, a chunk at a time. Stops
/// when the job stops reading, as a cancelled one does.
fn translate(
    mut reader: csv::Reader<File>,
    columns: &Columns,
    limit: usize,
    chunks: &mpsc::Sender<Result<Bytes, io::Error>>,
) {
    let mut buffer = Vec::with_capacity(CHUNK_BYTES);
    let mut record = ByteRecord::new();
    loop {
        match reader.read_byte_record(&mut record) {
            Ok(true) => {
                let cell = |index: Option<usize>| {
                    index
                        .and_then(|index| record.get(index))
                        .map(|cell| String::from_utf8_lossy(cell).into_owned())
                };
                let address = InputAddress {
                    address_id: cell(Some(columns.address_id)),
                    address_lines: Vec::from_iter(cell(Some(columns.address))),
                    country: cell(Some(columns.country)),
                    city: cell(columns.city),
                    admin1: cell(columns.region),
                    postal_code: cell(columns.postcode),
                    ..InputAddress::default()
                };
                // A `QUERY /geocode` item, the parameters and the id the
                // result echoes.
                let mut item = address
                    .geocode_params(limit)
                    .into_iter()
                    .map(|(name, value)| (name.to_string(), Value::String(value)))
                    .collect::<serde_json::Map<_, _>>();
                if let Some(id) = given(address.address_id.as_deref()) {
                    item.insert("id".to_string(), Value::String(id.to_string()));
                }
                if serde_json::to_writer(&mut buffer, &item).is_err() {
                    let _ = chunks.blocking_send(Err(io::Error::other("a row did not encode")));
                    return;
                }
                buffer.push(b'\n');
                if buffer.len() >= CHUNK_BYTES {
                    let chunk = Bytes::from(std::mem::take(&mut buffer));
                    if chunks.blocking_send(Ok(chunk)).is_err() {
                        return;
                    }
                }
            }
            Ok(false) => break,
            Err(cause) => {
                let _ = chunks.blocking_send(Err(io::Error::other(format!(
                    "the uploaded file could not be read: {cause}"
                ))));
                return;
            }
        }
    }
    if !buffer.is_empty() {
        let _ = chunks.blocking_send(Ok(Bytes::from(buffer)));
    }
}

/// `/v2/bulk-geocode`.
pub(super) async fn geocode(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, VendorError> {
    submit(state, request, Product::Geocode).await
}

/// `/v2/bulk-verify`. The same job as a geocode, written out as verify
/// output.
pub(super) async fn verify(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, VendorError> {
    submit(state, request, Product::Verify).await
}

async fn submit(
    state: AppState,
    request: Request,
    product: Product,
) -> Result<Response, VendorError> {
    let base = base_url(&state, request.headers());
    let body = read_json::<SubmitRequest>(&state, request, "INVALID_REQUEST").await?;
    let invalid_file = || error(StatusCode::BAD_REQUEST, "INVALID_REQUEST", INVALID_FILE);
    let file = given(body.file_id.as_deref())
        .and_then(|id| stored_file(&state, id))
        .ok_or_else(invalid_file)?;
    let delimiter = delimiter(body.delimiter.as_deref())?;
    let limit = body.preferences.max_results()?;
    check_emails(body.notify_emails.as_deref())?;

    let path = content_path(&state, &file.file_id);
    let (reader, columns) = task::spawn_blocking(move || {
        let upload = File::open(path).map_err(|_| invalid_file())?;
        let mut reader = ReaderBuilder::new()
            .delimiter(delimiter)
            .has_headers(false)
            .flexible(true)
            .from_reader(upload);
        let columns = read_header(&mut reader)?;
        Ok::<_, VendorError>((reader, columns))
    })
    .await
    .map_err(|join| internal(io::Error::other(join)))??;

    let (chunks_tx, chunks_rx) = mpsc::channel(16);
    task::spawn_blocking(move || translate(reader, &columns, limit, &chunks_tx));
    let job = Request::builder()
        .method("POST")
        .uri("/jobs?endpoint=geocode")
        .header(header::CONTENT_TYPE, "application/jsonl")
        .header(header::ACCEPT, "application/jsonl")
        .body(channel_body(chunks_rx))
        .map_err(|cause| internal(io::Error::other(cause)))?;
    let started = jobs::start(state.clone(), job).await.map_err(|problem| {
        // Every slot taken, or the store full. Precisely answers a client
        // sending too much this way.
        let failure = if problem.status() == 503 {
            Failure {
                http: StatusCode::TOO_MANY_REQUESTS,
                status: "TOO_MANY_REQUESTS",
                message: "Rate limit exceeded (DIS-1005)".to_string(),
                of_request: false,
            }
        } else {
            Failure::of(&problem, None)
        };
        failure.into_error(problem)
    })?;
    let job_id = started
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|location| location.strip_prefix("/jobs/"))
        .map(str::to_string)
        .ok_or_else(|| internal(io::Error::other("the job has no Location")))?;
    // Not streamed, since the client polls and the job runs on without a
    // reader.
    drop(started);
    let stored = StoredJob {
        job_id: job_id.clone(),
        file_id: file.file_id,
        file_name: file.file_name,
        submitted_at: u64::try_from(now().as_millis()).unwrap_or(u64::MAX),
        product,
    };
    write_record(&jobs_dir(&state).join(format!("{job_id}.json")), &stored).map_err(internal)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(Submitted {
            status_url: format!("{base}/v2/bulk/jobs/{job_id}"),
        }),
    )
        .into_response())
}

/// og's job state as Precisely names it. og starts a job at once, so none
/// is ever `SCHEDULED`.
fn status_name(status: Status) -> &'static str {
    match status {
        Status::Running => "RUNNING",
        Status::Completed => "COMPLETED",
        Status::Cancelled => "CANCELED",
        Status::Failed => "FAILED",
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct JobStatus {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    download_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub(super) async fn job_status(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Result<Json<JobStatus>, VendorError> {
    let (job, status) = stored_job(&state, &id)
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "INVALID_REQUEST", INVALID_JOB))?;
    let base = base_url(&state, &headers);
    Ok(Json(JobStatus {
        status: status_name(status.status),
        download_url: (status.status == Status::Completed)
            .then(|| format!("{base}/v2/bulk/jobs/{}/output", job.job_id)),
        error: status.error,
    }))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Cancelled {
    job_id: String,
    message: &'static str,
}

/// Precisely cancels a job only while it waits to start. og's never wait,
/// so a running one is cancelled; the rows it answered are kept.
pub(super) async fn cancel_job(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Cancelled>, VendorError> {
    let (job, status) = stored_job(&state, &id)
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "BAD_REQUEST", INVALID_JOB))?;
    if status.status != Status::Running || state.jobs().cancel(&job.job_id).is_none() {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "BAD_REQUEST",
            format!(
                "Job is {} and can no longer be canceled",
                status_name(status.status)
            ),
        ));
    }
    Ok(Json(Cancelled {
        job_id: job.job_id,
        message: "Request received for job cancellation",
    }))
}

#[derive(Debug, Serialize)]
pub(super) struct QueuedJobs {
    jobs: Vec<QueuedJob>,
}

/// `QueuedJobView`. Its `emailId` is omitted, since og has no users.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct QueuedJob {
    job_id: String,
    file_id: String,
    file_name: String,
    status: &'static str,
    submitted_at: String,
}

/// Jobs still to finish, newest first. `status` picks `scheduled`,
/// `running` or `all` of them.
pub(super) async fn list_jobs(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Json<QueuedJobs>, VendorError> {
    let refuse = || {
        error(
            StatusCode::BAD_REQUEST,
            "INVALID_CLIENT_INPUT",
            INVALID_CLIENT_INPUT,
        )
    };
    let query =
        serde_urlencoded::from_str::<Vec<(String, String)>>(query.as_deref().unwrap_or_default())
            .map_err(|_| refuse())?;
    let mut scheduled_only = false;
    for (name, value) in &query {
        match (name.as_str(), value.as_str()) {
            ("status", "running" | "all") => scheduled_only = false,
            ("status", "scheduled") => scheduled_only = true,
            _ => return Err(refuse()),
        }
    }
    let mut jobs = Vec::new();
    if !scheduled_only && let Ok(entries) = fs::read_dir(jobs_dir(&state)) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            if let Some((job, status)) = stored_job(&state, id)
                && status.status == Status::Running
            {
                jobs.push(job);
            }
        }
    }
    jobs.sort_by(|left, right| {
        (right.submitted_at, &right.job_id).cmp(&(left.submitted_at, &left.job_id))
    });
    Ok(Json(QueuedJobs {
        jobs: jobs
            .into_iter()
            .map(|job| QueuedJob {
                status: status_name(Status::Running),
                submitted_at: iso8601(job.submitted_at),
                job_id: job.job_id,
                file_id: job.file_id,
                file_name: job.file_name,
            })
            .collect(),
    }))
}

/// Unix milliseconds as `2025-11-03T09:39:17.128Z`.
pub(super) fn iso8601(millis: u64) -> String {
    let seconds = millis / 1000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let of_day = seconds % 86_400;
    // Days since 1970 to a date in the proleptic Gregorian calendar.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60,
        millis % 1000
    )
}

/// A line of og's JSONL results. The row's place, its id, and its answer
/// or its Problem.
#[derive(Debug, Deserialize)]
struct ResultLine {
    row: usize,
    id: Option<String>,
    response: Option<Collection>,
    error: Option<RowProblem>,
}

#[derive(Debug, Deserialize)]
struct Collection {
    features: Vec<Feature>,
}

#[derive(Debug, Deserialize)]
struct RowProblem {
    status: u16,
    error_code: String,
    #[serde(default)]
    invalid_params: Vec<RowParam>,
}

#[derive(Debug, Deserialize)]
struct RowParam {
    name: String,
}

/// The output rows of one input row. One a candidate, or one saying why
/// there is none.
fn output_rows(line: ResultLine, product: Product) -> io::Result<Vec<u8>> {
    let mut writer = WriterBuilder::new().from_writer(Vec::new());
    let id = line.id.unwrap_or_default();
    let blank = vec![String::new(); product.columns().len() - 3];
    let mut row = |status: &str, message: &str, cells: Vec<String>| {
        writer.write_record(
            [id.as_str(), status, message]
                .into_iter()
                .chain(cells.iter().map(String::as_str)),
        )
    };
    match (line.response, line.error) {
        (Some(collection), _) if !collection.features.is_empty() => {
            for feature in &collection.features {
                row("OK", "", feature.candidate(product).cells(product))?;
            }
        }
        (Some(_), _) => row("ZERO_RESULTS", "", blank)?,
        (None, problem) => {
            let failure = match &problem {
                Some(problem) => {
                    let params = problem
                        .invalid_params
                        .iter()
                        .map(|param| param.name.as_str())
                        .collect::<Vec<_>>();
                    Failure::from_parts(problem.status, &problem.error_code, &params, None)
                }
                None => Failure::from_parts(500, "internal_error", &[], None),
            };
            row(failure.status, &failure.message, blank)?;
        }
    }
    writer.into_inner().map_err(|error| error.into_error())
}

/// Write the job's results as Precisely's output, in input order, a chunk
/// at a time.
fn convert(results: &Path, product: Product, chunks: &mpsc::Sender<Result<Bytes, io::Error>>) {
    let run = || -> io::Result<()> {
        let mut header = WriterBuilder::new().from_writer(Vec::new());
        header.write_record(product.columns())?;
        let mut buffer = header.into_inner().map_err(|error| error.into_error())?;
        // og writes rows as they finish; the order is restored here, as og
        // restores it for its own CSV.
        let mut pending = BTreeMap::new();
        let mut next_row = 0;
        for line in BufReader::new(File::open(results)?).lines() {
            let line = serde_json::from_str::<ResultLine>(&line?)?;
            pending.insert(line.row, output_rows(line, product)?);
            while let Some(rows) = pending.remove(&next_row) {
                buffer.extend_from_slice(&rows);
                next_row += 1;
            }
            if buffer.len() >= CHUNK_BYTES
                && chunks
                    .blocking_send(Ok(Bytes::from(std::mem::take(&mut buffer))))
                    .is_err()
            {
                return Ok(());
            }
        }
        for rows in pending.into_values() {
            buffer.extend_from_slice(&rows);
        }
        let _ = chunks.blocking_send(Ok(Bytes::from(buffer)));
        Ok(())
    };
    if let Err(cause) = run() {
        let _ = chunks.blocking_send(Err(cause));
    }
}

/// The `downloadUrl`, a completed job's output CSV.
pub(super) async fn output(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Response, VendorError> {
    let (job, status) = stored_job(&state, &id)
        .ok_or_else(|| error(StatusCode::BAD_REQUEST, "INVALID_REQUEST", INVALID_JOB))?;
    let results = state
        .jobs()
        .results_path(&job.job_id)
        .filter(|_| status.status == Status::Completed)
        .ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                format!(
                    "Job is {}; only a completed job has output",
                    status_name(status.status)
                ),
            )
        })?;
    let (chunks_tx, chunks_rx) = mpsc::channel(16);
    let product = job.product;
    task::spawn_blocking(move || convert(&results, product, &chunks_tx));
    let mut response = channel_body(chunks_rx).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    if let Ok(disposition) =
        HeaderValue::from_str(&format!("attachment; filename=\"{}.csv\"", job.job_id))
    {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, disposition);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submitted_at_reads_as_iso_8601() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1_762_162_757_128), "2025-11-03T09:39:17.128Z");
        assert_eq!(iso8601(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(iso8601(1_798_761_599_999), "2026-12-31T23:59:59.999Z");
    }

    #[test]
    fn a_job_names_its_delimiter_or_takes_the_pipe() {
        for (sent, read) in [
            (None, b'|'),
            (Some(""), b'|'),
            (Some(","), b','),
            (Some(";"), b';'),
            (Some("|"), b'|'),
            (Some("\t"), b'\t'),
        ] {
            assert_eq!(delimiter(sent).expect("delimiter"), read, "{sent:?}");
        }
        for sent in [":", ",,", " ", "\\t"] {
            assert_eq!(
                delimiter(Some(sent)).expect_err("refused").status,
                StatusCode::BAD_REQUEST,
                "{sent:?}"
            );
        }
    }

    #[test]
    fn ids_are_uuids_and_nothing_else() {
        assert_eq!(
            file_id("8D907C8C-5B63-417F-B69F-2D33DBDD2FCD").as_deref(),
            Some("8d907c8c-5b63-417f-b69f-2d33dbdd2fcd")
        );
        assert_eq!(
            job_id("180c0892-affc-4f7a-b1c3-d5e965a6eefb").as_deref(),
            Some("180c0892affc4f7ab1c3d5e965a6eefb")
        );
        for raw in ["", "..", "../x", "/etc/passwd", "a\0b", "not-a-uuid"] {
            assert_eq!(file_id(raw), None, "{raw:?}");
            assert_eq!(job_id(raw), None, "{raw:?}");
        }
    }
}
