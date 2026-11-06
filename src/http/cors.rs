//! Opt-in CORS, so a browser page on another origin can call the API.

use std::time::Duration;

use axum::http::{HeaderName, HeaderValue, Method, header};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::error::{Error, Result};

/// The layer for `origins`, `*` for any; `None` when there are none, so no
/// CORS headers are sent. Applied around the whole router, so a preflight is
/// answered before the routes' 405 fallbacks see it.
pub(crate) fn layer(origins: &[String]) -> Result<Option<CorsLayer>> {
    if origins.is_empty() {
        return Ok(None);
    }
    let origin = if origins.iter().any(|origin| origin == "*") {
        AllowOrigin::any()
    } else {
        let list = origins
            .iter()
            .map(|origin| {
                HeaderValue::from_str(origin.trim_end_matches('/')).map_err(|_| {
                    Error::BadOptions(
                        format!("--cors-origin {origin:?} is not a valid origin").into(),
                    )
                })
            })
            .collect::<Result<Vec<_>>>()?;
        AllowOrigin::list(list)
    };
    Ok(Some(
        CorsLayer::new()
            .allow_origin(origin)
            // QUERY and DELETE are not safelisted, so a browser preflights
            // them.
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::DELETE,
                Method::from_bytes(b"QUERY").expect("QUERY is a valid method"),
            ])
            .allow_headers([
                header::ACCEPT,
                header::CONTENT_TYPE,
                header::IF_NONE_MATCH,
                HeaderName::from_static("x-request-id"),
            ])
            // Not safelisted, so a page reads them only when listed.
            .expose_headers([
                header::ALLOW,
                header::CONTENT_LOCATION,
                header::ETAG,
                header::LOCATION,
                header::RETRY_AFTER,
                HeaderName::from_static("accept-query"),
                HeaderName::from_static("x-request-id"),
            ])
            .max_age(Duration::from_secs(3600)),
    ))
}
