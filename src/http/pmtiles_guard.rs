//! PMTiles full-file fetch guard (ADR 0017 Decision 35a, Level 1).
//!
//! A legitimate PMTiles client always sends a bounded `Range` header and reads
//! small chunks. Two request shapes each egress the entire multi-gigabyte archive
//! in one request and must be rejected: a Range-less `GET` (which `ServeFile`
//! would answer `200` with the whole body) and a whole-file range such as
//! `Range: bytes=0-` (which returns `206` but still streams everything). This
//! guard requires a single bounded `Range` on `GET`, allows `HEAD` without one,
//! and caps the range span so a single request cannot pull the archive.

use axum::{
    Extension,
    body::Body,
    extract::Request,
    http::{HeaderMap, Method, header},
    response::{IntoResponse, Response},
};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::http::{problem::Problem, request_id::RequestId};

/// Maximum `Range` span served in one request. Must exceed the demo client's
/// 512 KB PMTiles header read (`getBytes(0, 512e3)` => `bytes=0-511999`) while
/// staying far below the multi-gigabyte archive size.
const MAX_RANGE_SPAN: u64 = 1_048_576; // 1 MiB

/// Why a PMTiles `GET` range was refused. Both map to `413`; the distinction only
/// drives the response `error_code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeReject {
    /// Not a single bounded closed range: a missing header, an open-ended
    /// `bytes=N-`, a suffix `bytes=-N`, a multi-range, or anything malformed. A
    /// legitimate client always sends `bytes=START-END`.
    NotBounded,
    /// A valid closed range whose span exceeds [`MAX_RANGE_SPAN`].
    TooLarge,
}

/// Accept only a single closed `bytes=START-END` range with `START <= END` and a
/// span within `cap`. Every other shape (open-ended, suffix, multi-range,
/// malformed) fails parsing and lands in [`RangeReject::NotBounded`].
fn check_bounded_range(raw: &str, cap: u64) -> Result<(), RangeReject> {
    let Some((start, end)) = raw
        .trim()
        .strip_prefix("bytes=")
        .and_then(|spec| spec.trim().split_once('-'))
    else {
        return Err(RangeReject::NotBounded);
    };
    let (Ok(start), Ok(end)) = (start.trim().parse::<u64>(), end.trim().parse::<u64>()) else {
        return Err(RangeReject::NotBounded);
    };
    // `checked_*` rejects end < start and guards the +1 against overflow.
    let Some(span) = end.checked_sub(start).and_then(|len| len.checked_add(1)) else {
        return Err(RangeReject::NotBounded);
    };
    if span > cap {
        return Err(RangeReject::TooLarge);
    }
    Ok(())
}

/// Guard handler mounted on `/basemap.pmtiles`. The router restricts methods to
/// `GET` and `HEAD`; this enforces the bounded-range requirement on `GET`.
pub(crate) async fn serve(
    serve_file: ServeFile,
    method: Method,
    headers: HeaderMap,
    request_id: Option<Extension<RequestId>>,
    request: Request,
) -> Response {
    // HEAD is allowed without a Range (it returns headers only, no body egress).
    if method == Method::HEAD {
        return run(serve_file, request).await;
    }

    let result = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .map(|range| check_bounded_range(range, MAX_RANGE_SPAN))
        .unwrap_or(Err(RangeReject::NotBounded)); // no Range header at all

    match result {
        Ok(()) => run(serve_file, request).await,
        Err(reject) => refuse(reject, request_id),
    }
}

async fn run(serve_file: ServeFile, request: Request) -> Response {
    match serve_file.oneshot(request).await {
        Ok(response) => response.map(Body::new),
        // ServeFile's error type is Infallible, so this arm is unreachable in
        // practice, but stay safe rather than unwrap.
        Err(_) => Problem::internal().into_response(),
    }
}

fn refuse(reject: RangeReject, request_id: Option<Extension<RequestId>>) -> Response {
    let problem = match reject {
        RangeReject::TooLarge => Problem::payload_too_large(
            "range_too_large",
            format!("Range span must be at most {MAX_RANGE_SPAN} bytes"),
        ),
        RangeReject::NotBounded => Problem::payload_too_large(
            "range_required",
            "a bounded Range request is required for this resource",
        ),
    };
    match request_id {
        Some(Extension(RequestId(id))) => problem.with_request_id(id),
        None => problem,
    }
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u64 = MAX_RANGE_SPAN;

    #[test]
    fn accepts_bounded_ranges() {
        assert!(check_bounded_range("bytes=0-511999", CAP).is_ok()); // demo header read
        assert!(check_bounded_range("bytes=0-1048575", CAP).is_ok()); // exactly the cap
        assert!(check_bounded_range("bytes=4096-8191", CAP).is_ok()); // a tile range
    }

    #[test]
    fn refuses_whole_file_ranges_as_too_large() {
        assert_eq!(
            check_bounded_range("bytes=0-1048576", CAP), // one byte past the cap
            Err(RangeReject::TooLarge)
        );
        assert_eq!(
            check_bounded_range("bytes=0-9999999999", CAP), // pmtiles.js 416-fallback shape
            Err(RangeReject::TooLarge)
        );
    }

    #[test]
    fn refuses_unbounded_or_malformed_ranges() {
        for raw in [
            "bytes=0-",          // open-ended
            "bytes=1024-",       // open-ended
            "bytes=-1000",       // suffix
            "bytes=0-1,100-101", // multi-range
            "0-100",             // missing unit
            "bytes=abc-def",     // non-numeric
            "bytes=100-0",       // end before start
        ] {
            assert_eq!(
                check_bounded_range(raw, CAP),
                Err(RangeReject::NotBounded),
                "{raw}"
            );
        }
    }
}
