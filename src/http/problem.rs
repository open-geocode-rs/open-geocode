//! RFC 9457 Problem Details (`application/problem+json`) error responses.
//!
//! ADR 0017 Decisions 29 and 31: public-demo API errors use correct status codes
//! and a standard problem shape instead of an ad hoc error body, and must not
//! leak stack traces, filesystem paths, or Pack internals.
//!
//! An internal error still keeps its cause, privately: the Problem carries it
//! into the response extensions, never the body, so the request-id middleware
//! can log it under the request id (issue #2).

use std::sync::Arc;

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use tokio::task::JoinError;

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

/// Every error this API answers with. The stable, machine-readable half of
/// the contract: `detail` is prose that may be reworded, `error_code` is not.
///
/// A closed enum so a new error cannot reach a client undocumented, and so
/// the published contract can list every value it is allowed to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCode {
    InvalidQuery,
    InvalidCoordinate,
    LimitTooLarge,
    MalformedParameter,
    NotFound,
    MethodNotAllowed,
    InternalError,
}

impl ErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidQuery => "invalid_query",
            Self::InvalidCoordinate => "invalid_coordinate",
            Self::LimitTooLarge => "limit_too_large",
            Self::MalformedParameter => "malformed_parameter",
            Self::NotFound => "not_found",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::InternalError => "internal_error",
        }
    }

    /// The slug this code uses in its `type` URI: the code with underscores
    /// as hyphens, so the URI reads as a path.
    fn slug(self) -> String {
        self.as_str().replace('_', "-")
    }

    /// The `type` member: a documentation URI once [`DOCS_BASE`] is set, and
    /// RFC 9457's `about:blank` until then.
    fn type_uri(self) -> String {
        match DOCS_BASE {
            Some(base) => format!("{base}/{}", self.slug()),
            None => "about:blank".to_string(),
        }
    }
}

/// Where the problem types are documented, once they are.
///
/// `None` until then, which renders `type` as `about:blank`: RFC 9457's
/// default, and the only honest value while there is nothing to dereference.
/// A `type` that looks like a link but 404s is worse than one that never
/// promised anything.
///
/// Giving this a base URL turns every `type` into a real link, which is the
/// property the error contracts developers like best all have: somewhere to
/// read next to the code to branch on (Stripe `doc_url`, GitHub
/// `documentation_url`, Twilio `more_info`, Meilisearch `link`). No separate
/// link member is needed, because `type` already is one.
///
/// That change is additive — `about:blank` promises nothing, so replacing it
/// breaks no client — but it does change every error body, so it lands with
/// the pages, not before. `error_code` is the stable value either way.
const DOCS_BASE: Option<&str> = None;

/// The RFC 9457 problem document fields.
///
/// `instance` is not among them: it identifies the specific occurrence, and
/// `request_id` already does that here, with a value the logs are indexed by.
/// RFC 9457 makes every member optional, so leaving it out stays compliant.
///
/// `error_code` is an extension member: the stable value to branch on, where
/// `title` and `detail` are prose that may be reworded. RFC 9457 requires
/// clients to ignore extensions they do not know, so adding it stays
/// compliant. The companion link those APIs provide is [`DOCS_BASE`], carried
/// by `type` rather than a second member.
#[derive(Debug, Clone, Serialize)]
struct ProblemInner {
    #[serde(rename = "type")]
    type_uri: String,
    title: String,
    status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    error_code: ErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    invalid_params: Vec<InvalidParam>,
    /// Value for the `Allow` response header (405 only); never serialized.
    #[serde(skip)]
    allow: Option<String>,
    /// What went wrong behind a 500, for the log only; never serialized
    /// (Decision 31). An `Arc` because response extensions must be `Clone`.
    #[serde(skip)]
    cause: Option<Arc<anyhow::Error>>,
}

/// An RFC 9457 problem response. Boxed so it stays small in the `Err` position of
/// the many `Result<_, Problem>` returned across the boundary (clippy
/// `result_large_err`).
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub(crate) struct Problem(Box<ProblemInner>);

impl Problem {
    fn new(status: StatusCode, error_code: ErrorCode) -> Self {
        Self(Box::new(ProblemInner {
            type_uri: error_code.type_uri(),
            title: status.canonical_reason().unwrap_or("Error").to_string(),
            status: status.as_u16(),
            detail: None,
            error_code,
            request_id: None,
            invalid_params: Vec::new(),
            allow: None,
            cause: None,
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

    /// The private cause of an internal error, for the log only.
    pub(crate) fn cause(&self) -> Option<&anyhow::Error> {
        self.0.cause.as_deref()
    }

    // Read accessors, used only by tests to assert on a constructed Problem
    // before it is rendered to a response.

    #[cfg(test)]
    pub(crate) fn status(&self) -> u16 {
        self.0.status
    }

    #[cfg(test)]
    pub(crate) fn error_code(&self) -> ErrorCode {
        self.0.error_code
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
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::InvalidQuery).with_detail(detail)
    }

    pub(crate) fn invalid_coordinate(params: Vec<InvalidParam>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::InvalidCoordinate)
            .with_detail("coordinate out of range")
            .with_invalid_params(params)
    }

    pub(crate) fn limit_too_large(params: Vec<InvalidParam>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::LimitTooLarge)
            .with_detail("requested result count exceeds the maximum")
            .with_invalid_params(params)
    }

    pub(crate) fn malformed_parameter(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::MalformedParameter).with_detail(detail)
    }

    // --- 404 / 405 ---------------------------------------------------------

    pub(crate) fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrorCode::NotFound).with_detail("unknown route")
    }

    pub(crate) fn method_not_allowed(allow: impl Into<String>) -> Self {
        let allow = allow.into();
        let mut problem = Self::new(StatusCode::METHOD_NOT_ALLOWED, ErrorCode::MethodNotAllowed)
            .with_detail("method not allowed for this route");
        problem.0.allow = Some(allow);
        problem
    }

    // --- 500 ---------------------------------------------------------------

    /// A generic 500. The body carries no detail so internal errors never
    /// leak; the cause rides along privately for the log.
    pub(crate) fn internal(cause: anyhow::Error) -> Self {
        let mut problem = Self::new(StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::InternalError);
        problem.0.cause = Some(Arc::new(cause));
        problem
    }

    /// The problem document with its status and headers. Unlike
    /// [`IntoResponse`], this leaves the Problem out of the extensions.
    pub(crate) fn render(&self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec());

        let mut response = (status, body).into_response();
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/problem+json"),
        );
        if let Some(allow) = &self.0.allow
            && let Ok(value) = HeaderValue::from_str(allow)
        {
            response.headers_mut().insert(header::ALLOW, value);
        }
        response
    }
}

/// The response also carries the Problem itself as an extension, so the
/// request-id middleware can put the request id in the body and log the cause
/// of a 500 without any handler having to remember either.
impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let mut response = self.render();
        response.extensions_mut().insert(self);
        response
    }
}

/// Map a geocoder error into a Problem without leaking internals, so handlers
/// can just use `?`.
///
/// A failure the caller could have avoided is a 400 that says what was wrong;
/// everything else is a 500 whose body carries no detail and whose cause goes
/// to the log. `Error::is_caller_error` draws the line, so a new variant is
/// classified there rather than by matching on its message here.
impl From<crate::Error> for Problem {
    fn from(error: crate::Error) -> Self {
        if error.is_caller_error() {
            Problem::invalid_query(error.to_string())
        } else {
            Problem::internal(anyhow::Error::new(error))
        }
    }
}

/// A blocking task that panicked is a 500 whose cause is the panic message, so
/// the log says what broke rather than just that the task failed.
impl From<JoinError> for Problem {
    fn from(error: JoinError) -> Self {
        if !error.is_panic() {
            return Problem::internal(anyhow::anyhow!("blocking task was cancelled"));
        }
        // `panic!` with a literal carries a `&str`, with a format a `String`.
        let payload = error.into_panic();
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("(payload is not a string)");
        Problem::internal(anyhow::anyhow!("blocking task panicked: {message}"))
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
        // `about:blank` while there are no problem pages to point at; the
        // machine-readable value clients branch on is `error_code`.
        assert_eq!(value["type"], "about:blank");
        assert_eq!(value["title"], "Bad Request");
        assert_eq!(value["status"], 400);
        assert_eq!(value["detail"], "invalid search query");
        assert_eq!(value["error_code"], "invalid_query");
        assert_eq!(value["request_id"], "req-123");
        // `instance` was never populated and `request_id` identifies the
        // occurrence instead. RFC 9457 makes every member optional.
        assert!(value.get("instance").is_none());
    }

    const ALL_CODES: [ErrorCode; 7] = [
        ErrorCode::InvalidQuery,
        ErrorCode::InvalidCoordinate,
        ErrorCode::LimitTooLarge,
        ErrorCode::MalformedParameter,
        ErrorCode::NotFound,
        ErrorCode::MethodNotAllowed,
        ErrorCode::InternalError,
    ];

    /// `type` is either RFC 9457's `about:blank` or a URI that can actually be
    /// followed. A relative path or a link to pages that do not exist would be
    /// worse than promising nothing.
    #[test]
    fn every_type_uri_is_blank_or_absolute() {
        for code in ALL_CODES {
            let uri = code.type_uri();
            assert!(
                uri == "about:blank" || uri.starts_with("https://"),
                "{uri} is neither about:blank nor an absolute https URI"
            );
        }
    }

    /// The slugs are the `type` path segments once there are pages to serve,
    /// so they are fixed now even though nothing reads them yet.
    #[test]
    fn slugs_read_as_paths_and_are_distinct() {
        let slugs: Vec<String> = ALL_CODES.iter().map(|code| code.slug()).collect();
        for slug in &slugs {
            assert!(!slug.contains('_'), "{slug} should use hyphens");
        }
        let mut unique = slugs.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), slugs.len(), "slugs collide: {slugs:?}");
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
        let problem = Problem::internal(anyhow::anyhow!("section offset past end of file"));
        let (status, value) = body_json(problem).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(value.get("detail").is_none());
        assert!(!value.to_string().contains("section offset"));
        assert_eq!(value["error_code"], "internal_error");
    }

    #[test]
    fn parse_error_converts_to_400_without_a_cause() {
        let problem = Problem::from(crate::Error::UnparsableQuery {
            query: "(".to_string(),
        });
        assert_eq!(problem.status(), 400);
        assert_eq!(problem.error_code(), ErrorCode::InvalidQuery);
        assert!(problem.cause().is_none());
    }

    #[test]
    fn other_errors_convert_to_500_keeping_the_cause() {
        let problem = Problem::from(crate::Error::Inconsistent(
            "tantivy index read failed".to_string(),
        ));
        assert_eq!(problem.status(), 500);
        assert!(problem.detail().is_none());
        assert_eq!(
            format!("{:#}", problem.cause().expect("cause kept")),
            "Pack is inconsistent: tantivy index read failed"
        );
    }
}
