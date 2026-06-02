//! RFC 9457 Problem Details (`application/problem+json`) error responses.
//!
//! ADR 0017 Decisions 29 and 31: public-demo API errors use correct status codes
//! and a standard problem shape instead of an ad hoc error body, and must not
//! leak stack traces, filesystem paths, or Pack internals.

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// One `invalid_params` entry, a safe extension member naming a rejected field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct InvalidParam {
    pub name: String,
    pub reason: String,
}

impl InvalidParam {
    pub(crate) fn new(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            reason: reason.into(),
        }
    }
}

/// The RFC 9457 problem document fields.
#[derive(Debug, Clone, Serialize)]
struct ProblemInner {
    #[serde(rename = "type")]
    type_uri: String,
    title: String,
    status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    invalid_params: Vec<InvalidParam>,
    /// Value for the `Allow` response header (405 only); never serialized.
    #[serde(skip)]
    allow: Option<String>,
}

/// An RFC 9457 problem response. Boxed so it stays small in the `Err` position of
/// the many `Result<_, Problem>` returned across the boundary (clippy
/// `result_large_err`).
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub(crate) struct Problem(Box<ProblemInner>);

impl Problem {
    fn new(status: StatusCode, error_code: &str) -> Self {
        Self(Box::new(ProblemInner {
            type_uri: format!("/problems/{}", error_code.replace('_', "-")),
            title: status.canonical_reason().unwrap_or("Error").to_string(),
            status: status.as_u16(),
            detail: None,
            instance: None,
            request_id: None,
            error_code: Some(error_code.to_string()),
            invalid_params: Vec::new(),
            allow: None,
        }))
    }

    pub(crate) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.0.detail = Some(detail.into());
        self
    }

    pub(crate) fn with_invalid_params(mut self, params: Vec<InvalidParam>) -> Self {
        self.0.invalid_params = params;
        self
    }

    /// Attach a request id (a safe Problem extension member, Decision 30).
    pub(crate) fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.0.request_id = Some(request_id.into());
        self
    }

    // Read accessors, used only by tests to assert on a constructed Problem
    // before it is rendered to a response.

    #[cfg(test)]
    pub(crate) fn status(&self) -> u16 {
        self.0.status
    }

    #[cfg(test)]
    pub(crate) fn error_code(&self) -> Option<&str> {
        self.0.error_code.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn detail(&self) -> Option<&str> {
        self.0.detail.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn invalid_params(&self) -> &[InvalidParam] {
        &self.0.invalid_params
    }

    // --- 400 ---------------------------------------------------------------

    pub(crate) fn invalid_query(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_query").with_detail(detail)
    }

    pub(crate) fn invalid_coordinate(params: Vec<InvalidParam>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_coordinate")
            .with_detail("coordinate out of range")
            .with_invalid_params(params)
    }

    pub(crate) fn limit_too_large(params: Vec<InvalidParam>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "limit_too_large")
            .with_detail("requested result count exceeds the maximum")
            .with_invalid_params(params)
    }

    pub(crate) fn malformed_parameter(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "malformed_parameter").with_detail(detail)
    }

    // --- 404 / 405 ---------------------------------------------------------

    pub(crate) fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found").with_detail("unknown route")
    }

    pub(crate) fn method_not_allowed(allow: impl Into<String>) -> Self {
        let allow = allow.into();
        let mut problem = Self::new(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed")
            .with_detail("method not allowed for this route");
        problem.0.allow = Some(allow);
        problem
    }

    // --- 413 / 414 / 429 ---------------------------------------------------

    pub(crate) fn payload_too_large(error_code: &str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, error_code).with_detail(detail)
    }

    #[allow(dead_code)]
    pub(crate) fn uri_too_long() -> Self {
        Self::new(StatusCode::URI_TOO_LONG, "uri_too_long").with_detail("request URI is too long")
    }

    #[allow(dead_code)]
    pub(crate) fn too_many_requests() -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, "too_many_requests")
            .with_detail("rate limit exceeded")
    }

    // --- 500 / 503 ---------------------------------------------------------

    /// A generic 500. Carries no detail so internal errors never leak.
    pub(crate) fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
    }

    #[allow(dead_code)]
    pub(crate) fn unavailable() -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable")
            .with_detail("runtime is not ready")
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let allow = self.0.allow.clone();
        let body = serde_json::to_vec(&self).unwrap_or_else(|_| b"{}".to_vec());

        let mut response = (status, body).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        if let Some(allow) = allow
            && let Ok(value) = HeaderValue::from_str(&allow)
        {
            response.headers_mut().insert(header::ALLOW, value);
        }
        response
    }
}

/// Map a search/autocomplete error into a Problem without leaking internals.
///
/// `PackTextSearcher` reports a Tantivy query-parse failure with a message that
/// contains `"failed to parse search query"`; that is a client error (400).
/// Everything else is an unexpected runtime failure (500, no detail).
pub(crate) fn classify_search_error(error: anyhow::Error) -> Problem {
    let details = format!("{error:#}");
    if details.contains("failed to parse search query") {
        Problem::invalid_query("invalid search query")
    } else {
        Problem::internal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_json(problem: Problem) -> (StatusCode, serde_json::Value) {
        let response = problem.into_response();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert_eq!(content_type, "application/problem+json");
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read problem body");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("valid problem json");
        (status, value)
    }

    #[tokio::test]
    async fn renders_problem_json_with_required_members() {
        let (status, value) =
            body_json(Problem::invalid_query("invalid search query").with_request_id("req-123"))
                .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(value["type"], "/problems/invalid-query");
        assert_eq!(value["title"], "Bad Request");
        assert_eq!(value["status"], 400);
        assert_eq!(value["detail"], "invalid search query");
        assert_eq!(value["error_code"], "invalid_query");
        assert_eq!(value["request_id"], "req-123");
    }

    #[tokio::test]
    async fn renders_invalid_params() {
        let (status, value) = body_json(Problem::invalid_coordinate(vec![InvalidParam::new(
            "lat",
            "must be within [-90, 90]",
        )]))
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(value["invalid_params"][0]["name"], "lat");
        assert_eq!(
            value["invalid_params"][0]["reason"],
            "must be within [-90, 90]"
        );
    }

    #[tokio::test]
    async fn internal_error_leaks_no_detail() {
        let (status, value) = body_json(Problem::internal()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(value.get("detail").is_none());
        assert_eq!(value["error_code"], "internal_error");
    }

    #[test]
    fn classify_maps_parse_error_to_400() {
        let problem = classify_search_error(anyhow::anyhow!("failed to parse search query \"(\""));
        assert_eq!(problem.status(), 400);
        assert_eq!(problem.error_code(), Some("invalid_query"));
    }

    #[test]
    fn classify_maps_other_errors_to_500_without_detail() {
        let problem = classify_search_error(anyhow::anyhow!("tantivy index read failed"));
        assert_eq!(problem.status(), 500);
        assert!(problem.detail().is_none());
    }
}
