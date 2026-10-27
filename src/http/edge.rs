//! What sits between the Runtime and the browser or CDN in front of it: CORS,
//! response compression, and cache headers on the lookup routes.
//!
//! Applied around the finished router, so the routes and their Problem
//! handling stay as they are.

use std::time::Duration;

use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderName, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::Response,
};
use tower_http::{
    compression::CompressionLayer,
    cors::{AllowOrigin, CorsLayer},
};
use xxhash_rust::xxh3::Xxh3;

use crate::error::{Error, Result};

/// The routes whose answer is a pure function of the Pack and the query.
/// Batches are left out: they are uploads, and a batch's status moves.
const LOOKUPS: [&str; 4] = ["/search", "/autocomplete", "/geocode", "/reverse"];

#[derive(Debug, Clone)]
pub struct EdgeOptions {
    /// Origins a browser page may call the API from, `*` for any. Empty sends
    /// no CORS headers at all.
    pub cors_origins: Vec<String>,
    /// Seconds a lookup answer may be reused. `0` sends no cache headers.
    pub cache_max_age: u32,
}

impl Default for EdgeOptions {
    fn default() -> Self {
        Self {
            cors_origins: Vec::new(),
            cache_max_age: 3600,
        }
    }
}

/// Wrap `app`. `pack_id` names the Pack being served, so a cached answer ends
/// with it.
pub(crate) fn apply(app: Router, options: &EdgeOptions, pack_id: u64) -> Result<Router> {
    let mut app = app;
    if options.cache_max_age > 0 {
        app = app.layer(middleware::from_fn_with_state(
            Cache {
                seed: pack_id,
                max_age: options.cache_max_age,
            },
            cache,
        ));
    }
    if let Some(cors) = cors(&options.cors_origins)? {
        app = app.layer(cors);
    }
    // Outermost: it must see the final headers and body of everything below.
    Ok(app.layer(CompressionLayer::new()))
}

fn cors(origins: &[String]) -> Result<Option<CorsLayer>> {
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
            // QUERY is not safelisted, so a browser preflights it; the layer
            // answers that before the routes' 405 fallbacks can.
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
                HeaderName::from_static("prefer"),
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

#[derive(Clone)]
struct Cache {
    seed: u64,
    max_age: u32,
}

impl Cache {
    /// Weak: the same answer is sent gzip, zstd or plain, which are not
    /// byte-identical. Derived from the request, not the body, so a match
    /// skips the lookup.
    fn etag(&self, request: &Request) -> String {
        let mut hasher = Xxh3::new();
        hasher.update(&self.seed.to_le_bytes());
        // A new build can answer differently from the same Pack.
        hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
        hasher.update(&[0]);
        hasher.update(request.uri().path().as_bytes());
        hasher.update(&[0]);
        hasher.update(request.uri().query().unwrap_or_default().as_bytes());
        format!("W/\"{:016x}\"", hasher.digest())
    }

    fn stamp(&self, response: &mut Response, etag: &str) {
        let headers = response.headers_mut();
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_str(&format!("public, max-age={}", self.max_age))
                .expect("digits are a valid header value"),
        );
        headers.insert(
            header::ETAG,
            HeaderValue::from_str(etag).expect("hex is a valid header value"),
        );
        // Set here as well as by the compression layer, which skips a 304:
        // the 304 must carry the Vary of the 200 it stands for.
        headers.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
    }
}

async fn cache(State(cache): State<Cache>, request: Request, next: Next) -> Response {
    if request.method() != Method::GET || !LOOKUPS.contains(&request.uri().path()) {
        return next.run(request).await;
    }
    let etag = cache.etag(&request);
    if matches_etag(request.headers().get_all(header::IF_NONE_MATCH), &etag) {
        let mut response = Response::new(axum::body::Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        cache.stamp(&mut response, &etag);
        return response;
    }
    let mut response = next.run(request).await;
    // A Problem is the request's or the server's fault, not an answer to keep.
    if response.status() == StatusCode::OK {
        cache.stamp(&mut response, &etag);
    }
    response
}

/// `If-None-Match` compares weakly (RFC 9110 section 13.1.2).
fn matches_etag(values: header::GetAll<'_, HeaderValue>, etag: &str) -> bool {
    let opaque = |tag: &str| tag.trim().trim_start_matches("W/").to_owned();
    let ours = opaque(etag);
    values
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|tag| tag.trim() == "*" || opaque(tag) == ours)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(seed: u64, uri: &str) -> String {
        let request = Request::get(uri).body(axum::body::Body::empty()).unwrap();
        Cache { seed, max_age: 1 }.etag(&request)
    }

    #[test]
    fn the_etag_follows_the_pack_and_the_request() {
        let tag_a = tag(1, "/search?q=a");
        assert_eq!(tag_a, tag(1, "/search?q=a"));
        assert_ne!(tag_a, tag(2, "/search?q=a"));
        assert_ne!(tag_a, tag(1, "/search?q=b"));
        assert_ne!(tag_a, tag(1, "/geocode?q=a"));
    }
}
