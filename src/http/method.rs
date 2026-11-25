use crate::http::problem::Problem;

pub(crate) async fn not_found() -> Problem {
    Problem::not_found()
}

// axum's method fallback is global and cannot see the matched route, so it always answers `Allow: GET`.
pub(crate) async fn method_not_allowed() -> Problem {
    Problem::method_not_allowed("GET")
}
