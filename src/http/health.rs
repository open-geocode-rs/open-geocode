//! Health endpoints (ADR 0017 Decision 28).
//!
//! `/healthz` answers "the process is alive"; `/readyz` answers "the Pack is
//! loaded and the Runtime can serve geocoding traffic". Both return empty bodies
//! so they cannot leak filesystem paths, Pack internals, or diagnostics.

use axum::{extract::State, http::StatusCode};

use crate::runtime::AppState;

pub(crate) async fn healthz() -> StatusCode {
    StatusCode::OK
}

pub(crate) async fn readyz(State(state): State<AppState>) -> StatusCode {
    if state.is_ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}
