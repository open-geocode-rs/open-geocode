use std::{
    fs, io,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Bytes,
    extract::{Path as UrlPath, State},
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task;

use crate::runtime::AppState;

use super::{
    Rid,
    batch::{BatchError, date_time, store_dir},
};

pub(super) const MAX_PER_JOB: usize = 5;
const PREFIX: &str = "NOTIFICATION-";
/// The one type HERE has. Its docs also spell it `WEB_HOOK`, which is read
/// as the same.
const WEB_HOOK: &str = "webHook";
const TIMEOUT: Duration = Duration::from_secs(10);

static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    id: String,
    url: String,
    created: u64,
    #[serde(default)]
    last_used: Option<u64>,
}

impl Stored {
    fn body(&self) -> Value {
        let mut body = json!({
            "id": self.id,
            "type": WEB_HOOK,
        });
        if let Some(last_used) = self.last_used {
            body["lastUsedTime"] = date_time(last_used).into();
        }
        body["config"] = json!({ "url": self.url });
        body
    }
}

/// HERE's `Notification` as a client sends it. `id` and `lastUsedTime` are
/// read-only, so a body that carries them back is read without them.
#[derive(Debug, Deserialize)]
struct Given {
    #[serde(rename = "type")]
    kind: Option<String>,
    config: Option<GivenConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GivenConfig {
    url: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct Facts {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) status: &'static str,
    pub(super) output_type: &'static str,
    pub(super) succeeded: usize,
    pub(super) failed: usize,
    pub(super) total: usize,
    pub(super) valid: usize,
    pub(super) invalid: usize,
}

impl Facts {
    fn dummy() -> Self {
        Self {
            id: "JOB-00000000-0000-0000-0000-000000000000".to_string(),
            name: "test".to_string(),
            status: "completed",
            output_type: "csv",
            succeeded: 0,
            failed: 0,
            total: 0,
            valid: 0,
            invalid: 0,
        }
    }

    fn properties(&self) -> [(&'static str, String); 9] {
        [
            ("JOB_ID", self.id.clone()),
            ("JOB_NAME", self.name.clone()),
            ("JOB_STATUS", self.status.to_string()),
            ("JOB_OUTPUT_TYPE", self.output_type.to_string()),
            ("JOB_RECORDS_SUCCEEDED", self.succeeded.to_string()),
            ("JOB_RECORDS_FAILED", self.failed.to_string()),
            ("JOB_RECORDS_TOTAL", self.total.to_string()),
            ("JOB_RECORDS_VALID", self.valid.to_string()),
            ("JOB_RECORDS_INVALID", self.invalid.to_string()),
        ]
    }
}

fn fill(url: &str, facts: &Facts) -> String {
    let mut url = url.to_string();
    for (name, value) in facts.properties() {
        url = url.replace(&format!("${{{name}}}"), &encode(&value));
    }
    url
}

fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn dir(state: &AppState) -> PathBuf {
    store_dir(state).join("notifications")
}

fn file(state: &AppState, id: &str) -> Option<PathBuf> {
    let uuid = uuid::Uuid::parse_str(id.strip_prefix(PREFIX)?).ok()?;
    (format!("{PREFIX}{uuid}") == id).then(|| dir(state).join(format!("{id}.json")))
}

fn load(state: &AppState, id: &str) -> Option<Stored> {
    let bytes = fs::read(file(state, id)?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save(state: &AppState, stored: &Stored) -> io::Result<()> {
    let path = file(state, &stored.id).ok_or_else(|| io::Error::other("bad id"))?;
    fs::create_dir_all(dir(state))?;
    let json = serde_json::to_vec(stored).map_err(io::Error::other)?;
    let partial = path.with_extension("json.partial");
    fs::write(&partial, json)?;
    fs::rename(partial, path)
}

pub(super) fn exists(state: &AppState, id: &str) -> bool {
    load(state, id).is_some()
}

fn not_found() -> BatchError {
    BatchError::new(
        StatusCode::NOT_FOUND,
        "E6004044",
        "Notification not found",
        "Notification with the specified ID could not be found",
        "Verify the notification ID and try again",
    )
}

fn internal(error: &io::Error) -> BatchError {
    BatchError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "E6005000",
        "Internal Server Error",
        format!("The notification could not be kept: {error}"),
        "Try again later",
    )
}

fn read(body: &[u8]) -> Result<String, BatchError> {
    let given: Given = serde_json::from_slice(body).map_err(|error| {
        BatchError::bad_request(
            "Invalid notification",
            format!("The body is not a notification: {error}"),
        )
    })?;
    match given.kind.as_deref() {
        None | Some(WEB_HOOK | "WEB_HOOK") => {}
        Some(other) => {
            return Err(BatchError::bad_request(
                "Illegal input for field 'type'",
                format!("Actual value: '{other}'; supported values: '{WEB_HOOK}'"),
            ));
        }
    }
    let url = given
        .config
        .and_then(|config| config.url)
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
        .ok_or_else(|| {
            BatchError::bad_request(
                "Missing field 'config.url'",
                "A webHook notification needs the URL to call",
            )
        })?;
    let filled = fill(&url, &Facts::dummy());
    let valid = !filled.contains("${")
        && filled.parse::<Uri>().is_ok_and(|uri| {
            matches!(uri.scheme_str(), Some("http" | "https")) && uri.authority().is_some()
        });
    if !valid {
        return Err(BatchError::bad_request(
            "Illegal input for field 'config.url'",
            "Provide an absolute http or https URL whose placeholders are job properties",
        ));
    }
    Ok(url)
}

fn on_allowed_host(state: &AppState, url: String) -> Result<String, BatchError> {
    match allowed(state, &fill(&url, &Facts::dummy())) {
        true => Ok(url),
        false => Err(BatchError::bad_request(
            "Illegal input for field 'config.url'",
            "This server calls back only the hosts its operator allows",
        )),
    }
}

fn allowed(state: &AppState, url: &str) -> bool {
    url.parse::<Uri>()
        .ok()
        .and_then(|uri| {
            let host = uri
                .host()?
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string();
            let port = match uri.scheme_str()? {
                "http" => 80,
                "https" => 443,
                _ => return None,
            };
            Some(state.may_call(&host, Some(uri.port_u16().unwrap_or(port))))
        })
        .unwrap_or(false)
}

pub(super) async fn create(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    body: Bytes,
) -> Response {
    let answer = || {
        let url = on_allowed_host(&state, read(&body)?)?;
        let stored = Stored {
            id: format!("{PREFIX}{}", uuid::Uuid::new_v4()),
            url,
            created: unix_now(),
            last_used: None,
        };
        save(&state, &stored).map_err(|error| internal(&error))?;
        Ok::<_, BatchError>(stored)
    };
    match answer() {
        Ok(stored) => (StatusCode::CREATED, axum::Json(stored.body())).into_response(),
        Err(error) => error.into_response(&request_id),
    }
}

/// Every notification, oldest first. HERE's docs declare one object here;
/// a list is the only shape that holds more than one.
pub(super) async fn list(State(state): State<AppState>) -> Response {
    let mut kept = fs::read_dir(dir(&state))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            load(&state, name.to_str()?.strip_suffix(".json")?)
        })
        .collect::<Vec<_>>();
    kept.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
    axum::Json(kept.iter().map(Stored::body).collect::<Vec<_>>()).into_response()
}

pub(super) async fn get(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    UrlPath(id): UrlPath<String>,
) -> Response {
    match load(&state, &id) {
        Some(stored) => axum::Json(stored.body()).into_response(),
        None => not_found().into_response(&request_id),
    }
}

pub(super) async fn replace(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    UrlPath(id): UrlPath<String>,
    body: Bytes,
) -> Response {
    let answer = || {
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut stored = load(&state, &id).ok_or_else(not_found)?;
        stored.url = on_allowed_host(&state, read(&body)?)?;
        save(&state, &stored).map_err(|error| internal(&error))?;
        Ok::<_, BatchError>(stored)
    };
    match answer() {
        Ok(stored) => axum::Json(stored.body()).into_response(),
        Err(error) => error.into_response(&request_id),
    }
}

pub(super) async fn delete(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let _guard = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let removed = file(&state, &id).is_some_and(|path| fs::remove_file(path).is_ok());
    if removed {
        StatusCode::NO_CONTENT.into_response()
    } else {
        not_found().into_response(&request_id)
    }
}

pub(super) async fn test(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    UrlPath(id): UrlPath<String>,
) -> Response {
    if !exists(&state, &id) {
        return not_found().into_response(&request_id);
    }
    let called = {
        let state = state.clone();
        task::spawn_blocking(move || call(&state, &id, &Facts::dummy()))
            .await
            .unwrap_or_else(|error| Err(error.to_string()))
    };
    match called {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(cause) => BatchError::new(
            StatusCode::BAD_GATEWAY,
            "E6005020",
            "Notification endpoint failed",
            cause,
            "Check that the notification URL is reachable from this server",
        )
        .into_response(&request_id),
    }
}

pub(super) fn deliver(state: &AppState, ids: &[String], facts: &Facts) {
    for id in ids {
        let state = state.clone();
        let id = id.clone();
        let facts = facts.clone();
        task::spawn_blocking(move || {
            let _ = call(&state, &id, &facts);
        });
    }
}

fn call(state: &AppState, id: &str, facts: &Facts) -> Result<(), String> {
    let url = {
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(mut stored) = load(state, id) else {
            let cause = "the notification was deleted".to_string();
            log(state, id, facts, None, Some(&cause));
            return Err(cause);
        };
        stored.last_used = Some(unix_now());
        let _ = save(state, &stored);
        fill(&stored.url, facts)
    };
    if !allowed(state, &url) {
        let cause = "the host is not one this server calls back".to_string();
        log(state, id, facts, None, Some(&cause));
        return Err(cause);
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .max_redirects(0)
        .user_agent(concat!("open-geocode/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let outcome = match agent.get(&url).call() {
        Ok(response) if response.status().is_success() => Ok(response.status().as_u16()),
        Ok(response) => Err(format!(
            "the endpoint answered {}",
            response.status().as_u16()
        )),
        Err(_) => Err("the call failed before receiving an HTTP response".to_string()),
    };
    match outcome {
        Ok(status) => {
            log(state, id, facts, Some(status), None);
            Ok(())
        }
        Err(cause) => {
            log(state, id, facts, None, Some(&cause));
            Err(cause)
        }
    }
}

fn log(state: &AppState, id: &str, facts: &Facts, status: Option<u16>, cause: Option<&str>) {
    state.error_log().event(&json!({
        "level": if cause.is_none() { "info" } else { "warn" },
        "message": "here batch notification",
        "notification_id": id,
        "job_id": facts.id,
        "job_status": facts.status,
        "status": status,
        "cause": cause,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_takes_each_job_property_encoded() {
        let facts = Facts {
            name: "night & day".to_string(),
            succeeded: 7,
            ..Facts::dummy()
        };
        assert_eq!(
            fill(
                "http://hook.test/${JOB_ID}?n=${JOB_NAME}&s=${JOB_STATUS}&ok=${JOB_RECORDS_SUCCEEDED}",
                &facts
            ),
            "http://hook.test/JOB-00000000-0000-0000-0000-000000000000?n=night%20%26%20day&s=completed&ok=7"
        );
    }

    #[test]
    fn a_notification_body_is_checked() {
        let url = |body: &str| read(body.as_bytes()).map_err(|error| error.title);
        assert_eq!(
            url(r#"{"type":"WEB_HOOK","config":{"url":"https://a.test/?id=${JOB_ID}"}}"#),
            Ok("https://a.test/?id=${JOB_ID}".to_string())
        );
        assert_eq!(
            url(r#"{"id":"NOTIFICATION-x","config":{"url":"http://a.test"}}"#),
            Ok("http://a.test".to_string())
        );
        assert_eq!(
            url(r#"{"type":"email","config":{"url":"http://a.test"}}"#),
            Err("Illegal input for field 'type'".to_string())
        );
        assert_eq!(
            url(r#"{"config":{}}"#),
            Err("Missing field 'config.url'".to_string())
        );
        for bad in ["ftp://a.test", "/relative", "http://a.test/${NOPE}"] {
            assert_eq!(
                url(&format!(r#"{{"config":{{"url":"{bad}"}}}}"#)),
                Err("Illegal input for field 'config.url'".to_string()),
                "{bad}"
            );
        }
        assert_eq!(
            url(r#"{"config":{"url":"http://a.test","method":"POST"}}"#),
            Err("Invalid notification".to_string())
        );
    }
}
