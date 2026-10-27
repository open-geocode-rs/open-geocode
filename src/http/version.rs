//! `GET /version`: which build serves which Pack.
//!
//! Nothing here names a path or a source file, so the body is as safe to
//! show as `/healthz`'s silence: a version, some counts and a content id.

use std::collections::BTreeMap;

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

/// What the Pack's manifest says, and which content it is.
#[derive(Debug, Serialize, utoipa::ToSchema)]
struct PackInfo {
    /// Identifies the Pack's content: equal ids, same Pack.
    #[schema(example = "9f2c4d1e7a3b5c60")]
    id: String,
    schema_version: u32,
    /// Version of the `open-geocode` that built the Pack.
    #[schema(example = "0.1.0")]
    built_by: String,
    /// When the Pack was built, in seconds since the Unix epoch.
    built_at_unix: u64,
    record_count: u64,
    /// Records per layer.
    layer_counts: BTreeMap<String, u64>,
}

/// Get the server and Pack versions
///
/// Returns the server's version and what identifies the Pack it serves: a
/// content id, the schema version, who built it and when, and its record
/// counts. It names no path.
#[utoipa::path(
    get,
    path = "/version",
    tag = "operations",
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
            record_count: manifest.record_count,
            layer_counts: manifest.layer_counts.clone(),
        },
    })
}
