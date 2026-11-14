//! `/compat/google`.

use axum::Router;

use crate::runtime::AppState;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
}
