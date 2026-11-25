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

pub(super) fn error(status: StatusCode, code: &str, message: impl Into<String>) -> VendorError {
    VendorError::new(
        status,
        json!({"errors": [{"status": code, "errorMessage": message.into()}]}),
    )
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Failure {
    pub(super) http: StatusCode,
    pub(super) status: &'static str,
    pub(super) message: String,
    pub(super) of_request: bool,
}

impl Failure {
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

    pub(super) fn into_error(self, problem: Problem) -> VendorError {
        let retry = retry_after(&problem);
        let mut error = error(self.http, self.status, self.message).caused_by(problem);
        if let Some((name, value)) = retry {
            error = error.with_header(name, value);
        }
        error
    }
}

fn country_code(country: &str) -> String {
    let country = country.trim();
    if country.len() == 3 {
        let mapped = if country.bytes().all(|byte| byte.is_ascii_digit()) {
            crate::iso::alpha2_of_numeric(country)
        } else {
            crate::iso::alpha2_of(country)
        };
        if let Some(alpha2) = mapped {
            return alpha2.to_string();
        }
    }
    country.to_string()
}

fn given(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Preferences {
    max_results: Option<i64>,
    distance: Option<Distance>,
    #[serde(rename = "originXY", default)]
    origin_xy: Vec<f64>,
    filter_bias: Option<FilterBias>,
    client_locale: Option<Locale>,
}

#[derive(Debug, Deserialize)]
pub(super) struct FilterBias {
    #[serde(rename = "type")]
    kind: BiasKind,
    fields: BiasFields,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BiasKind {
    Bias,
    Filter,
}

#[derive(Debug, Default, Deserialize)]
struct BiasFields {
    #[serde(default)]
    include: std::collections::BTreeMap<String, BiasValue>,
    #[serde(default)]
    exclude: std::collections::BTreeMap<String, BiasValue>,
}

#[derive(Debug, Deserialize)]
struct BiasValue {
    value: String,
}

impl FilterBias {
    pub(super) fn check(&self) -> Result<(), VendorError> {
        let invalid =
            |message: String| Err(error(StatusCode::BAD_REQUEST, "INVALID_REQUEST", &message));
        if self.kind == BiasKind::Bias && !self.fields.exclude.is_empty() {
            return invalid("filterBias of type bias takes no exclude".to_string());
        }
        for field in self.fields.include.keys().chain(self.fields.exclude.keys()) {
            match field.as_str() {
                "city" | "postalCode" => {}
                "poiCategory" => {
                    return invalid("poiCategory filters a POI search only".to_string());
                }
                other => return invalid(format!("filterBias cannot use the field {other}")),
            }
        }
        Ok(())
    }

    fn matches(
        fields: &std::collections::BTreeMap<String, BiasValue>,
        city: Option<&str>,
        postcode: Option<&str>,
    ) -> bool {
        fields.iter().any(|(field, wanted)| {
            let wanted = wanted.value.trim();
            match field.as_str() {
                "city" => city.is_some_and(|city| city.eq_ignore_ascii_case(wanted)),
                "postalCode" => {
                    !wanted.is_empty() && postcode.is_some_and(|code| code.starts_with(wanted))
                }
                _ => false,
            }
        })
    }

    pub(super) fn keeps(&self, city: Option<&str>, postcode: Option<&str>) -> bool {
        match self.kind {
            BiasKind::Bias => true,
            BiasKind::Filter => {
                (self.fields.include.is_empty()
                    || Self::matches(&self.fields.include, city, postcode))
                    && !Self::matches(&self.fields.exclude, city, postcode)
            }
        }
    }

    pub(super) fn favours(&self, city: Option<&str>, postcode: Option<&str>) -> bool {
        self.kind == BiasKind::Bias && Self::matches(&self.fields.include, city, postcode)
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Locale {
    Text(String),
    Parts {
        language: Option<String>,
        script: Option<String>,
        country: Option<String>,
    },
    Other(serde::de::IgnoredAny),
}

impl Locale {
    fn lang(&self) -> Option<String> {
        let lang = match self {
            Self::Text(text) => text.trim().replace('_', "-"),
            Self::Parts {
                language,
                script,
                country,
            } => [language, script, country]
                .into_iter()
                .flatten()
                .map(|part| part.trim())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("-"),
            Self::Other(_) => return None,
        };
        crate::api::Languages::parse(&lang).is_ok().then_some(lang)
    }
}

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

const DEFAULT_MAX_RESULTS: i64 = 1;

const DEFAULT_DISTANCE_M: f64 = 5.0;

impl Preferences {
    fn lang(&self) -> Option<String> {
        self.client_locale.as_ref().and_then(Locale::lang)
    }

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

fn base_url(state: &AppState, headers: &HeaderMap) -> String {
    format!("{}/compat/precisely", state.origin(headers))
}

fn channel_body(chunks: mpsc::Receiver<Result<Bytes, std::io::Error>>) -> Body {
    let stream = futures_util::stream::unfold(chunks, |mut chunks| async move {
        chunks.recv().await.map(|chunk| (chunk, chunks))
    });
    // Fused, since tower-http compression polls a body once more after its
    // end, and unfold panics when polled past its end (see `jobs::tail`).
    Body::from_stream(stream.fuse())
}
