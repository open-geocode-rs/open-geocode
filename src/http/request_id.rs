use std::time::Instant;

use axum::{
    extract::{MatchedPath, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::Response,
};

use crate::http::problem::Problem;

const MAX_REQUEST_ID_LEN: usize = 128;

#[derive(Debug, Clone, Default)]
pub(crate) struct ErrorLog {
    #[cfg(test)]
    captured: Option<std::sync::Arc<std::sync::Mutex<Vec<String>>>>,
}

impl ErrorLog {
    #[cfg(test)]
    pub(crate) fn capture() -> Self {
        Self {
            captured: Some(Default::default()),
        }
    }

    #[cfg(test)]
    pub(crate) fn lines(&self) -> Vec<String> {
        self.captured
            .as_ref()
            .map(|lines| lines.lock().expect("log lock").clone())
            .unwrap_or_default()
    }

    pub(crate) fn internal_error(
        &self,
        request_id: &str,
        route: &str,
        item: Option<usize>,
        started: Instant,
        cause: Option<&str>,
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
        self.write(&line.to_string());
    }

    pub(crate) fn event(&self, line: &serde_json::Value) {
        self.write(&line.to_string());
    }

    fn write(&self, line: &str) {
        #[cfg(test)]
        if let Some(lines) = &self.captured {
            lines.lock().expect("log lock").push(line.to_owned());
            return;
        }
        eprintln!("{line}");
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RequestId(pub String);

#[derive(Debug, Clone)]
pub(crate) struct Logged(pub Problem);

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
    let route = request.extensions().get::<MatchedPath>().cloned();
    request.extensions_mut().insert(RequestId(id.clone()));

    let mut response = next.run(request).await;
    let problem = response.extensions_mut().remove::<Problem>();
    if let Some(problem) = &problem {
        *response.body_mut() = problem
            .clone()
            .with_request_id(id.clone())
            .render()
            .into_body();
        response.headers_mut().remove(header::CONTENT_LENGTH);
    }
    let logged = response.extensions_mut().remove::<Logged>();
    if response.status() == StatusCode::INTERNAL_SERVER_ERROR {
        let cause = problem
            .as_ref()
            .or_else(|| logged.as_ref().map(|logged| &logged.0))
            .and_then(Problem::cause)
            .map(|cause| format!("{cause:#}"));
        let route = route.as_ref().map_or("<unmatched>", MatchedPath::as_str);
        log.internal_error(&id, route, None, started, cause.as_deref());
    }
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

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
    use axum::{Router, body::Body, middleware, routing::get};
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn sanitize_strips_control_chars_and_caps_length() {
        assert_eq!(sanitize("abc-123").as_deref(), Some("abc-123"));
        assert_eq!(sanitize("a b\nc").as_deref(), Some("abc"));
        assert_eq!(sanitize("   "), None);
        let long = "x".repeat(MAX_REQUEST_ID_LEN + 50);
        assert_eq!(sanitize(&long).unwrap().len(), MAX_REQUEST_ID_LEN);
    }

    #[tokio::test]
    async fn internal_errors_log_route_templates() {
        let log = ErrorLog::capture();
        let router = Router::new()
            .route(
                "/lookup/{id}",
                get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
            )
            .fallback(|| async { StatusCode::INTERNAL_SERVER_ERROR })
            .layer(middleware::from_fn_with_state(log.clone(), propagate));

        for (uri, template) in [
            ("/lookup/private-input?q=private-query", "/lookup/{id}"),
            ("/private-fallback", "<unmatched>"),
        ] {
            let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
            let response = router.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            let lines = log.lines();
            let last = lines.last().unwrap();
            let event: serde_json::Value = serde_json::from_str(last).unwrap();
            assert_eq!(event["route"], template);
            assert!(!last.contains("private-"));
        }
    }
}
