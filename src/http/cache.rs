//! Cache headers on the lookup routes: an ETag on every successful GET, and
//! `Cache-Control: no-cache` unless `--cache-max-age` allows reuse.
//!
//! The ETag comes from the request, not the body, so an `If-None-Match` hit is
//! a 304 before the lookup runs.

use axum::{
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::Response,
};
use xxhash_rust::xxh3::Xxh3;

/// The routes whose answer is a pure function of the Pack and the query.
/// Jobs are left out: they are uploads, and a job's status moves.
const LOOKUPS: [&str; 4] = ["/search", "/autocomplete", "/geocode", "/reverse"];

#[derive(Clone)]
pub(crate) struct Cache {
    /// The served Pack's fingerprint, so a new Pack changes every ETag.
    seed: u64,
    cache_control: HeaderValue,
}

impl Cache {
    /// `max_age` 0 sends `no-cache`: a client keeps the answer but asks
    /// again each time, which costs a 304. An update is a Pack swap, and a
    /// max-age would keep serving the old answers after it.
    pub(crate) fn new(seed: u64, max_age: u32) -> Self {
        let cache_control = if max_age == 0 {
            HeaderValue::from_static("no-cache")
        } else {
            HeaderValue::from_str(&format!("public, max-age={max_age}"))
                .expect("digits are a valid header value")
        };
        Self {
            seed,
            cache_control,
        }
    }

    /// Weak: the same answer is sent gzip, zstd or plain, which are not
    /// byte-identical.
    fn etag(&self, request: &Request) -> HeaderValue {
        let mut hasher = Xxh3::new();
        hasher.update(&self.seed.to_le_bytes());
        // A new build can answer differently from the same Pack.
        hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
        hasher.update(&[0]);
        hasher.update(request.uri().path().as_bytes());
        hasher.update(&[0]);
        hasher.update(request.uri().query().unwrap_or_default().as_bytes());
        HeaderValue::from_str(&format!("W/\"{:016x}\"", hasher.digest()))
            .expect("hex is a valid header value")
    }

    fn stamp(&self, response: &mut Response, etag: HeaderValue) {
        let headers = response.headers_mut();
        headers.insert(header::CACHE_CONTROL, self.cache_control.clone());
        headers.insert(header::ETAG, etag);
    }
}

pub(crate) async fn layer(State(cache): State<Cache>, request: Request, next: Next) -> Response {
    if request.method() != Method::GET || !LOOKUPS.contains(&request.uri().path()) {
        return next.run(request).await;
    }
    let etag = cache.etag(&request);
    if matches(request.headers(), &etag) {
        let mut response = Response::new(axum::body::Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        cache.stamp(&mut response, etag);
        // The 304 carries the Vary of the 200 it stands for; compression
        // adds it to the 200 but skips an empty body.
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("accept-encoding"));
        return response;
    }
    let mut response = next.run(request).await;
    // A Problem is the request's or the server's fault, not an answer to keep.
    if response.status() == StatusCode::OK {
        cache.stamp(&mut response, etag);
    }
    response
}

/// `If-None-Match` compares weakly (RFC 9110 section 13.1.2).
fn matches(headers: &header::HeaderMap, etag: &HeaderValue) -> bool {
    let opaque = |tag: &str| tag.trim().trim_start_matches("W/").to_owned();
    let ours = opaque(etag.to_str().expect("hex"));
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|tag| tag.trim() == "*" || opaque(tag) == ours)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(seed: u64, uri: &str) -> HeaderValue {
        let request = Request::get(uri).body(axum::body::Body::empty()).unwrap();
        Cache::new(seed, 0).etag(&request)
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
