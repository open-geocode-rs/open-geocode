//! Global 404 / 405 fallbacks rendered as Problem Details (ADR 0017
//! Decisions 27 and 37).
//!
//! Unknown paths return 404 and disallowed methods return 405, both as
//! `application/problem+json` rather than an empty body or a static-file 404.
//! The request-id middleware adds the request id to both.

use crate::http::problem::Problem;

/// Router fallback for unmatched paths (Decision 37).
pub(crate) async fn not_found() -> Problem {
    Problem::not_found()
}

/// Router `method_not_allowed_fallback` for matched paths with a disallowed
/// method (Decision 27). This is a global fallback and cannot see the matched
/// route, so it advertises the common case `Allow: GET`; the basemap route also
/// accepts HEAD, but the demo's clients never probe it with other methods.
pub(crate) async fn method_not_allowed() -> Problem {
    Problem::method_not_allowed("GET")
}
