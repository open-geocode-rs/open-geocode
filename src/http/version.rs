//! `GET /version`: which build serves which Pack.
//!
//! The body names no file path: a version, a content id and the Pack's
//! build details.

use axum::{Json, extract::State};
use serde::Serialize;

use crate::runtime::AppState;

/// The server build and the Pack it serves.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(crate) struct VersionInfo {
    /// Version of the `open-geocode` server.
    #[schema(example = "0.1.0")]
    version: &'static str,
    pack: PackInfo,
}

/// Which Pack is served, and how it was built.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct PackInfo {
    /// Identifies the Pack's content: equal ids, same Pack.
    #[schema(example = "ebd8ae77aea58c01")]
    id: String,
    /// Version of the Pack file format.
    #[schema(example = 10)]
    schema_version: u32,
    /// Version of the `open-geocode` that built the Pack.
    #[schema(example = "0.1.0")]
    built_by: String,
    /// When the Pack was built, in seconds since the Unix epoch.
    #[schema(example = 1793074078)]
    built_at_unix: u64,
}

/// Get the server and Pack versions
///
/// Returns the server's version and what identifies the Pack it serves: a
/// content id, the schema version, and which build made it and when. It
/// names no path.
#[utoipa::path(
    get,
    path = "/version",
    tag = "Operations",
    responses(
        (status = 200, description = "The server and the Pack.", body = VersionInfo),
    ),
)]
pub(crate) async fn version(State(state): State<AppState>) -> Json<VersionInfo> {
    let pack = state.pack();
    let manifest = pack.manifest();
    Json(VersionInfo {
        version: env!("CARGO_PKG_VERSION"),
        pack: PackInfo {
            id: format!("{:016x}", pack.container().fingerprint()),
            schema_version: manifest.schema_version,
            built_by: manifest.crate_version.clone(),
            built_at_unix: manifest.built_at_unix,
        },
    })
}
