//! Request-id propagation (ADR 0017 Decision 30) and the internal-error log.
//!
//! Every public-demo request gets an id that flows through Cloudflare, the
//! Runtime, and Problem Details responses. Prefer an upstream id (Cloudflare
//! `cf-ray`, then a generic `x-request-id`) if present; otherwise generate one.
//! Incoming values are sanitized because clients can spoof ordinary headers
//! (Decision 24). (cloudflared connects directly to the Runtime; there is no
//! Nginx hop — ADR 0017 amended 2026-06-03b.)
//!
//! The same middleware puts the id in every Problem body and logs each 500
//! with its cause, so no handler can forget either (issue #2). The body never
//! carries the cause (Decision 31), which makes the log the only place it
//! survives.

use std::time::Instant;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::Response,
};
use tower_http::{
    LatencyUnit,
    classify::{ServerErrorsAsFailures, SharedClassifier},
    trace::{DefaultOnBodyChunk, DefaultOnEos, DefaultOnRequest, DefaultOnResponse, TraceLayer},
};
use tracing::{Level, Span};

use crate::http::problem::Problem;

const MAX_REQUEST_ID_LEN: usize = 128;

/// Where internal-error lines go: one JSON object per line on stderr, or, in
/// tests, a buffer to assert on.
#[derive(Debug, Clone, Default)]
pub(crate) struct ErrorLog {
    #[cfg(test)]
    captured: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
}

impl ErrorLog {
    /// A log that keeps its lines for the test instead of printing them.
    #[cfg(test)]
    pub(crate) fn capture() -> Self {
        Self {
            captured: Some(Default::default()),
        }
    }

    /// The lines written so far to a [`capture`](Self::capture) log.
    #[cfg(test)]
    pub(crate) fn lines(&self) -> Vec<String> {
        self.captured
            .as_ref()
            .map(|lines| lines.lock().expect("log lock").clone())
            .unwrap_or_default()
    }

    /// One line for a 500: the request, the route, how long it took and the
    /// cause. `item` is the place of the batch item that failed, when one did.
    pub(crate) fn internal_error(
        &self,
        request_id: &str,
        route: &str,
        item: Option<usize>,
        started: Instant,
        cause: Option<String>,
    ) {
        let mut line = serde_json::json!({
            "level": "error",
            "message": "internal error",
            "request_id": request_id,
            "route": route,
            "status": StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
            "latency_ms": started.elapsed().as_millis() as u64,
            "cause": cause,
        });
        if let Some(item) = item {
            line["item"] = item.into();
        }
        self.write(line.to_string());
    }

    fn write(&self, line: String) {
        #[cfg(test)]
        if let Some(lines) = &self.captured {
            lines.lock().expect("log lock").push(line);
            return;
        }
        eprintln!("{line}");
    }
}

/// The id a request is logged under, for a handler that reports errors
/// inside a 200, as a batch does.
#[derive(Debug, Clone)]
pub(crate) struct RequestId(pub String);

/// Middleware: resolve (or generate) the request id, set it on the response
/// `x-request-id` header and in any Problem body, and log a 500 with its cause.
pub(crate) async fn propagate(
    State(log): State<ErrorLog>,
    mut request: Request,
    next: Next,
) -> Response {
    let started = Instant::now();
    let incoming = request
        .headers()
        .get("cf-ray")
        .or_else(|| request.headers().get("x-request-id"))
        .and_then(|value| value.to_str().ok())
        .and_then(sanitize);
    let id = incoming.unwrap_or_else(generate);
    // The path only: the query string holds what the user searched for,
    // which is personal data and stays out of the log.
    let route = request.uri().path().to_string();
    request.extensions_mut().insert(RequestId(id.clone()));

    let mut response = next.run(request).await;
    // Only the body changes: headers set around the Problem, such as a
    // route's `Allow`, stay.
    let problem = response.extensions_mut().remove::<Problem>();
    if let Some(problem) = &problem {
        *response.body_mut() = problem
            .clone()
            .with_request_id(id.clone())
            .render()
            .into_body();
        response.headers_mut().remove(header::CONTENT_LENGTH);
    }
    // Only a 500 is ours to fix; a 4xx is the request's fault and a 503 from
    // `/readyz` is the expected answer while the Pack loads.
    if response.status() == StatusCode::INTERNAL_SERVER_ERROR {
        let cause = problem
            .as_ref()
            .and_then(Problem::cause)
            .map(|cause| format!("{cause:#}"));
        log.internal_error(&id, &route, None, started, cause);
    }
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

/// The per-request log layer: a span carrying the request id, method and
/// path, and one line per response with its status and latency. No separate
/// failure line: the response line has the status, and [`propagate`] logs an
/// internal error with its cause.
pub(crate) type RequestTraceLayer = TraceLayer<
    SharedClassifier<ServerErrorsAsFailures>,
    fn(&axum::http::Request<Body>) -> Span,
    DefaultOnRequest,
    DefaultOnResponse,
    DefaultOnBodyChunk,
    DefaultOnEos,
    (),
>;

/// Must sit inside [`propagate`] so the [`RequestId`] extension is already
/// set. The query string is left out of the span: it holds the searched
/// addresses.
pub(crate) fn trace_layer() -> RequestTraceLayer {
    TraceLayer::new_for_http()
        .make_span_with(request_span as fn(&axum::http::Request<Body>) -> Span)
        .on_response(
            DefaultOnResponse::new()
                .level(Level::INFO)
                .latency_unit(LatencyUnit::Millis),
        )
        .on_failure(())
}

fn request_span(request: &axum::http::Request<Body>) -> Span {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map_or("-", |RequestId(id)| id.as_str());
    tracing::info_span!(
        "request",
        request_id,
        method = %request.method(),
        path = request.uri().path(),
    )
}

/// Keep only ASCII graphic characters and cap the length, so a hostile or
/// malformed upstream header can never inject control characters or unbounded
/// data into logs and response headers.
fn sanitize(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_graphic())
        .take(MAX_REQUEST_ID_LEN)
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

fn generate() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_control_chars_and_caps_length() {
        assert_eq!(sanitize("abc-123").as_deref(), Some("abc-123"));
        // Whitespace and control characters are dropped.
        assert_eq!(sanitize("a b\nc").as_deref(), Some("abc"));
        // Empty after cleaning -> None so we fall back to a generated id.
        assert_eq!(sanitize("   "), None);
        let long = "x".repeat(MAX_REQUEST_ID_LEN + 50);
        assert_eq!(sanitize(&long).unwrap().len(), MAX_REQUEST_ID_LEN);
    }

    #[test]
    fn generate_produces_distinct_ids() {
        assert_ne!(generate(), generate());
    }
}
