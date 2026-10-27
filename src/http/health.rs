//! Health endpoints (ADR 0017 Decision 28).
//!
//! `/healthz` answers "the process is alive"; `/readyz` answers "the Pack is
//! loaded and the Runtime can serve geocoding traffic". Both return empty bodies
//! so they cannot leak filesystem paths, Pack internals, or diagnostics.

use axum::{extract::State, http::StatusCode};

use crate::runtime::AppState;

/// Check liveness
///
/// Returns `200` while the server is running, even before the Pack is loaded.
/// Use `/readyz` to check that it can geocode. The body is empty.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "operations",
    responses((status = 200, description = "The server is running.")),
)]
pub(crate) async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Check readiness
///
/// Returns `200` once the Pack is loaded, and `503` before that. The body is
/// empty.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "operations",
    responses(
        (status = 200, description = "The Pack is loaded."),
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
