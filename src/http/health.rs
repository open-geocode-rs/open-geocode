//! Health endpoints (ADR 0017 Decision 28).
//!
//! `/healthz` answers "the process is alive"; `/readyz` answers "the Pack is
//! loaded and the Runtime can serve geocoding traffic". Both return empty bodies
//! so they cannot leak filesystem paths, Pack internals, or diagnostics.

use axum::{extract::State, http::StatusCode};

use crate::runtime::AppState;

/// Is the process alive?
///
/// Answers 200 as soon as the server is listening, whether or not the Pack is
/// loaded. Use `/readyz` to know whether it can answer geocoding requests.
/// The body is always empty.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "operations",
    responses((status = 200, description = "The process is alive.")),
)]
pub(crate) async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Can the server answer geocoding requests?
///
/// 200 once the Pack is open, 503 until then. The body is always empty.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "operations",
    responses(
        (status = 200, description = "The Pack is loaded and queries can be served."),
        (status = 503, description = "The Pack is not loaded yet."),
    ),
)]
pub(crate) async fn readyz(State(state): State<AppState>) -> StatusCode {
    if state.is_ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}
