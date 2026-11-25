use std::{any::Any, sync::Arc};

use axum::{
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use tokio::task::JoinError;

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
    MalformedBody,
    NotFound,
    MethodNotAllowed,
    TooManyItems,
    BodyTooLarge,
    UnsupportedMediaType,
    InternalError,
    Busy,
    Timeout,
    PackOutdated,
}

impl ErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidQuery => "invalid_query",
            Self::InvalidCoordinate => "invalid_coordinate",
            Self::LimitTooLarge => "limit_too_large",
            Self::MalformedParameter => "malformed_parameter",
            Self::MalformedBody => "malformed_body",
            Self::NotFound => "not_found",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::TooManyItems => "too_many_items",
            Self::BodyTooLarge => "body_too_large",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::InternalError => "internal_error",
            Self::Busy => "busy",
            Self::Timeout => "timeout",
            Self::PackOutdated => "pack_outdated",
        }
    }

    fn slug(self) -> String {
        self.as_str().replace('_', "-")
    }

    fn type_uri(self) -> String {
        match DOCS_BASE {
            Some(base) => format!("{base}/{}", self.slug()),
            None => "about:blank".to_string(),
        }
    }
}

const DOCS_BASE: Option<&str> = None;

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
    #[serde(skip)]
    allow: Option<String>,
    #[serde(skip)]
    retry_after: Option<u32>,
    #[serde(skip)]
    cause: Option<Arc<anyhow::Error>>,
}

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
            retry_after: None,
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

    pub(crate) fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.0.request_id = Some(request_id.into());
        self
    }

    pub(crate) fn cause(&self) -> Option<&anyhow::Error> {
        self.0.cause.as_deref()
    }

    pub(crate) fn status(&self) -> u16 {
        self.0.status
    }

    pub(crate) fn error_code(&self) -> ErrorCode {
        self.0.error_code
    }

    pub(crate) fn detail(&self) -> Option<&str> {
        self.0.detail.as_deref()
    }

    pub(crate) fn retry_after(&self) -> Option<u32> {
        self.0.retry_after
    }

    pub(crate) fn invalid_params(&self) -> &[InvalidParam] {
        &self.0.invalid_params
    }

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

    pub(crate) fn unparsable_parameters() -> Self {
        Self::malformed_parameter("could not parse query parameters")
    }

    pub(crate) fn malformed_body(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::MalformedBody).with_detail(detail)
    }

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

    pub(crate) fn too_many_items(max: usize) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, ErrorCode::TooManyItems)
            .with_detail(format!("a batch holds at most {max} items"))
    }

    pub(crate) fn body_too_large(max_bytes: usize) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, ErrorCode::BodyTooLarge)
            .with_detail(format!("the body must be at most {max_bytes} bytes"))
    }

    pub(crate) fn unsupported_media_type(expected: &str) -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorCode::UnsupportedMediaType,
        )
        .with_detail(format!("the body must be sent as Content-Type: {expected}"))
    }

    pub(crate) fn busy(retry_after: u32) -> Self {
        let mut problem = Self::new(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Busy)
            .with_detail("the server is answering as many requests as it can; retry shortly");
        problem.0.retry_after = Some(retry_after);
        problem
    }

    pub(crate) fn timed_out(after: std::time::Duration) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Timeout).with_detail(format!(
            "the lookup took longer than the server allows, {} ms",
            after.as_millis()
        ))
    }

    pub(crate) fn pack_outdated(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_IMPLEMENTED, ErrorCode::PackOutdated).with_detail(detail)
    }

    pub(crate) fn internal(cause: anyhow::Error) -> Self {
        let mut problem = Self::new(StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::InternalError);
        problem.0.cause = Some(Arc::new(cause));
        problem
    }

    pub(crate) fn panicked(payload: &(dyn Any + Send)) -> Self {
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("(payload is not a string)");
        Problem::internal(anyhow::anyhow!("blocking task panicked: {message}"))
    }

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
        if let Some(seconds) = self.0.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let mut response = self.render();
        response.extensions_mut().insert(self);
        response
    }
}

impl From<crate::Error> for Problem {
    fn from(error: crate::Error) -> Self {
        if let crate::Error::TimedOut { after } = error {
            Problem::timed_out(after)
        } else if error.is_caller_error() {
            Problem::invalid_query(error.to_string())
        } else {
            Problem::internal(anyhow::Error::new(error))
        }
    }
}

impl From<JoinError> for Problem {
    fn from(error: JoinError) -> Self {
        if !error.is_panic() {
            return Problem::internal(anyhow::anyhow!("blocking task was cancelled"));
        }
        Problem::panicked(error.into_panic().as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_error_converts_to_400_without_a_cause() {
        let problem = Problem::from(crate::Error::UnparsableQuery {
            query: "(".to_string(),
        });
        assert_eq!(problem.status(), 400);
        assert_eq!(problem.error_code(), ErrorCode::InvalidQuery);
        assert!(problem.cause().is_none());
    }
}
