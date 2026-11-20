//! `/compat/precisely`, Precisely's Data Integrity Suite APIs. Real-time Geo
//! Addressing v1 and Bulk Geo Addressing v2. No token call: og checks no
//! credentials, so the SDK sends none to og.
//!
//! Real-time calls answer each item with og's own lookup. Bulk jobs are og
//! `/jobs`. The uploaded CSV streams into a job as JSONL, and the job's JSONL
//! results stream back out as Precisely's output CSV.

mod bulk;
mod explain;
mod match_code;
mod postal_city;
mod proximity;
mod realtime;
mod result;
#[cfg(test)]
mod tests;

use axum::{
    Router,
    body::{Body, Bytes},
    extract::Request,
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post, put},
};
use futures_util::StreamExt;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use tokio::sync::mpsc;

use super::{VendorError, retry_after};
use crate::{http::problem::Problem, runtime::AppState};

pub(crate) use bulk::sweep;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/geocode", post(realtime::geocode).fallback(not_found))
        .route(
            "/v1/reverse-geocode",
            post(realtime::reverse).fallback(not_found),
        )
        .route("/v1/lookup", post(realtime::lookup).fallback(not_found))
        .route("/v1/verify", post(realtime::verify).fallback(not_found))
        .route(
            "/v1/autocomplete",
            post(realtime::autocomplete).fallback(not_found),
        )
        // Same request and response as `/v1/autocomplete`, the one schema
        // Precisely documents for both.
        .route(
            "/v1/express-autocomplete",
            post(realtime::autocomplete).fallback(not_found),
        )
        .route(
            "/v2/express-autocomplete",
            post(realtime::express_autocomplete_v2).fallback(not_found),
        )
        .route(
            "/v1/autocomplete/postal-city",
            post(postal_city::postal_city).fallback(not_found),
        )
        .route(
            "/v1/explain-addresscode",
            post(explain::explain_addresscode).fallback(not_found),
        )
        .route(
            "/v1/proximity-search",
            post(proximity::proximity_search).fallback(not_found),
        )
        .route(
            "/v2/files/upload-url",
            post(bulk::upload_url).fallback(not_found),
        )
        .route("/v2/files", get(bulk::list_files).fallback(not_found))
        .route(
            "/v2/files/{file_id}",
            delete(bulk::delete_file).fallback(not_found),
        )
        .route(
            "/v2/files/{file_id}/content",
            put(bulk::put_content).fallback(not_found),
        )
        .route("/v2/bulk-geocode", post(bulk::geocode).fallback(not_found))
        .route("/v2/bulk-verify", post(bulk::verify).fallback(not_found))
        .route("/v2/bulk/jobs", get(bulk::list_jobs).fallback(not_found))
        .route(
            "/v2/bulk/jobs/{job_id}",
            get(bulk::job_status)
                .delete(bulk::cancel_job)
                .fallback(not_found),
        )
        .route(
            "/v2/bulk/jobs/{job_id}/output",
            get(bulk::output).fallback(not_found),
        )
        .fallback(not_found)
}

/// Precisely's error envelope. `status` names the failure; the message ends
/// with Precisely's error code in parentheses, as Precisely writes it.
pub(super) fn error(status: StatusCode, code: &str, message: impl Into<String>) -> VendorError {
    VendorError::new(
        status,
        json!({"errors": [{"status": code, "errorMessage": message.into()}]}),
    )
}

/// Precisely documents no 404 or 405; a wrong method answers this too.
async fn not_found() -> VendorError {
    error(
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
        "No such resource on this server",
    )
}

pub(super) const INTERNAL_MESSAGE: &str =
    "Issue with internal services. Please try again after sometime (DIS-1003)";
const MAX_RESULTS_MESSAGE: &str = "Invalid value for maxResults (DIS-LI-1036)";
const UNAVAILABLE_MESSAGE: &str =
    "Issue with internal services. Please try again after sometime (DIS-LI-1011)";

/// A failure og answered with, as Precisely names it. The HTTP status a
/// whole request would get, the `status` value and the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Failure {
    pub(super) http: StatusCode,
    pub(super) status: &'static str,
    pub(super) message: String,
    /// The request's own preferences were refused, which fails every item
    /// alike, so the whole request answers it.
    pub(super) of_request: bool,
}

impl Failure {
    /// `country` is the value the client sent, named in the message when og
    /// refuses it.
    pub(super) fn of(problem: &Problem, country: Option<&str>) -> Self {
        let params = problem
            .invalid_params()
            .iter()
            .map(|param| param.name.as_str())
            .collect::<Vec<_>>();
        Self::from_parts(
            problem.status(),
            problem.error_code().as_str(),
            &params,
            country,
        )
    }

    pub(super) fn from_parts(
        status: u16,
        code: &str,
        params: &[&str],
        country: Option<&str>,
    ) -> Self {
        if code == "limit_too_large" {
            return Self {
                http: StatusCode::BAD_REQUEST,
                status: "INVALID_REQUEST",
                message: MAX_RESULTS_MESSAGE.to_string(),
                of_request: true,
            };
        }
        let (http, status, message) = match (status, code) {
            (400, _) if params.contains(&"country") => (
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                match country {
                    Some(country) => {
                        format!("Not able to identify {country} country (DIS-LI-1016)")
                    }
                    None => "Not able to identify requested country (DIS-LI-1016)".to_string(),
                },
            ),
            (_, "timeout") => (
                StatusCode::SERVICE_UNAVAILABLE,
                "REQUEST_TIMEOUT",
                UNAVAILABLE_MESSAGE.to_string(),
            ),
            (503, _) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "SERVICE_UNAVAILABLE",
                UNAVAILABLE_MESSAGE.to_string(),
            ),
            // Precisely words every other refusal one way; og's own words
            // stay in og's log, never in a Precisely answer.
            (400..500, _) => (
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "Request Body is not valid (DIS-LI-1018)".to_string(),
            ),
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                INTERNAL_MESSAGE.to_string(),
            ),
        };
        Self {
            http,
            status,
            message,
            of_request: false,
        }
    }

    pub(super) fn invalid(message: impl Into<String>) -> Self {
        Self {
            http: StatusCode::BAD_REQUEST,
            status: "INVALID_REQUEST",
            message: message.into(),
            of_request: false,
        }
    }

    /// The whole request failing for this reason, with og's Problem behind
    /// it for the log and its `Retry-After`.
    pub(super) fn into_error(self, problem: Problem) -> VendorError {
        let retry = retry_after(&problem);
        let mut error = error(self.http, self.status, self.message).caused_by(problem);
        if let Some((name, value)) = retry {
            error = error.with_header(name, value);
        }
        error
    }
}

/// The two-letter code og filters by, from the alpha-2, alpha-3 or numeric
/// code Precisely takes. Anything else goes to og as sent, which refuses it
/// unless it has two letters; then og answers it as a country it does not
/// cover, with nothing.
fn country_code(country: &str) -> String {
    let country = country.trim();
    if country.len() == 3 {
        let mapped = if country.bytes().all(|byte| byte.is_ascii_digit()) {
            super::iso::alpha2_of_numeric(country)
        } else {
            super::iso::alpha2_of(country)
        };
        if let Some(alpha2) = mapped {
            return alpha2.to_string();
        }
    }
    country.to_string()
}

/// A field Precisely sends as `""` when it has no value.
fn given(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// `preferences`, the fields og can honour. The rest, and their types, are
/// not read, since `clientLocale` comes as a string or an object.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Preferences {
    max_results: Option<i64>,
    distance: Option<Distance>,
    /// The point to measure from, `[x, y]` in the client's coordinate
    /// system, which og takes as longitude and latitude. Requests send `[]`
    /// for none.
    #[serde(rename = "originXY", default)]
    origin_xy: Vec<f64>,
    /// Read only to tell it was sent (see
    /// [`realtime::express_autocomplete_v2`]).
    filter_bias: Option<serde::de::IgnoredAny>,
}

/// Precisely's `Distance`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Distance {
    value: f64,
    distance_unit: Option<DistanceUnit>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum DistanceUnit {
    Foot,
    Meter,
    Mile,
    Kilometer,
}

impl DistanceUnit {
    const fn metres(self) -> f64 {
        match self {
            Self::Foot => 0.3048,
            Self::Meter => 1.0,
            Self::Mile => 1609.344,
            Self::Kilometer => 1000.0,
        }
    }
}

/// Precisely answers one candidate unless asked for more.
const DEFAULT_MAX_RESULTS: i64 = 1;

impl Preferences {
    /// The search radius in metres, when the request states one. Precisely
    /// measures in metres unless told otherwise.
    fn distance_m(&self) -> Result<Option<f64>, VendorError> {
        let Some(distance) = &self.distance else {
            return Ok(None);
        };
        if !(distance.value.is_finite() && distance.value > 0.0) {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "Invalid value for distance (DIS-LI-1018)",
            ));
        }
        let unit = distance.distance_unit.unwrap_or(DistanceUnit::Meter);
        Ok(Some(distance.value * unit.metres()))
    }

    /// The `originXY` point as longitude and latitude, when the request
    /// gives one. A point og cannot place is refused, not dropped.
    fn origin(&self) -> Result<Option<[f64; 2]>, VendorError> {
        match self.origin_xy[..] {
            [] => Ok(None),
            [lon, lat] if (-180.0..=180.0).contains(&lon) && (-90.0..=90.0).contains(&lat) => {
                Ok(Some([lon, lat]))
            }
            _ => Err(error(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "Invalid value for originXY (DIS-LI-1018)",
            )),
        }
    }

    fn max_results(&self) -> Result<usize, VendorError> {
        let max = self.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
        usize::try_from(max)
            .ok()
            .filter(|max| *max > 0)
            .ok_or_else(|| {
                error(
                    StatusCode::BAD_REQUEST,
                    "INVALID_REQUEST",
                    MAX_RESULTS_MESSAGE,
                )
            })
    }
}

/// The JSON body, refused in Precisely's shape when it is missing, too
/// large or not the request's schema.
async fn read_json<T: DeserializeOwned>(
    state: &AppState,
    request: Request,
    invalid_status: &'static str,
) -> Result<T, VendorError> {
    let limit = state.limits().max_batch_body_bytes();
    let bytes = axum::body::to_bytes(request.into_body(), limit)
        .await
        .map_err(|_| {
            error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "INVALID_REQUEST",
                format!("Request body is over {limit} bytes (DIS-LI-1018)"),
            )
        })?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(error(
            StatusCode::BAD_REQUEST,
            invalid_status,
            "Request body is not specified in the request (DIS-LI-1019)",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        error(
            StatusCode::BAD_REQUEST,
            invalid_status,
            "Request Body is not valid (DIS-LI-1018)",
        )
    })
}

/// Where a URL handed back for the client to call starts (see
/// [`AppState::origin`]).
fn base_url(state: &AppState, headers: &HeaderMap) -> String {
    format!("{}/compat/precisely", state.origin(headers))
}

/// A body fed from a blocking writer through `chunks`; an `Err` ends it
/// broken, so a reader can tell a cut-off file from a whole one.
fn channel_body(chunks: mpsc::Receiver<Result<Bytes, std::io::Error>>) -> Body {
    let stream = futures_util::stream::unfold(chunks, |mut chunks| async move {
        chunks.recv().await.map(|chunk| (chunk, chunks))
    });
    // Fused, since tower-http compression polls a body once more after its
    // end, and unfold panics when polled past its end (see `jobs::tail`).
    Body::from_stream(stream.fuse())
}
