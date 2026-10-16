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
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::Response,
};

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

    fn write(&self, line: String) {
        #[cfg(test)]
        if let Some(lines) = &self.captured {
            lines.lock().expect("log lock").push(line);
            return;
        }
        eprintln!("{line}");
    }
}

/// Middleware: resolve (or generate) the request id, set it on the response
/// `x-request-id` header and in any Problem body, and log a 500 with its cause.
pub(crate) async fn propagate(
    State(log): State<ErrorLog>,
    request: Request,
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

    let mut response = next.run(request).await;
    // A Problem renders its own body and headers, so rendering it again with
    // the id loses nothing.
    let problem = response.extensions_mut().remove::<Problem>();
    if let Some(problem) = &problem {
        response = problem.clone().with_request_id(id.clone()).render();
    }
    // Only a 500 is ours to fix; a 4xx is the request's fault and a 503 from
    // `/readyz` is the expected answer while the Pack loads.
    if response.status() == StatusCode::INTERNAL_SERVER_ERROR {
        let cause = problem
            .as_ref()
            .and_then(Problem::cause)
            .map(|cause| format!("{cause:#}"));
        log.write(
            serde_json::json!({
                "level": "error",
                "message": "internal error",
                "request_id": id,
                "route": route,
                "status": response.status().as_u16(),
                "latency_ms": started.elapsed().as_millis() as u64,
                "cause": cause,
            })
            .to_string(),
        );
    }
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
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
