//! Vendor-compatible routes under `/compat/<vendor>/`.
//!
//! Each vendor module translates its vendor's request into og's own lookups
//! and og's answer into the vendor's response shape, so a client written for
//! the vendor reads og unchanged. Translation only: every lookup goes through
//! og's native handlers, with their validation, limits and lookup slots, so
//! `/compat` can never answer what og itself would not.
//!
//! Shared rules (decision record 10 to 15f):
//! - A field og has no value for is sent as the vendor sends a missing value.
//! - Request parameters og cannot honour are ignored.
//! - Vendor credentials are never checked: auth belongs to the gateway og
//!   sits behind.
//! - Errors use the vendor's own body and status.
//! - Vendor id fields carry og's id as URL-safe base64.
//! - An endpoint og lacks answers the vendor's own "not implemented".

pub(crate) mod fieldmask;
pub(crate) mod google;
pub(crate) mod here;
pub(crate) mod iso;
pub(crate) mod mapbox;
pub(crate) mod precisely;

use axum::{
    Router,
    extract::{Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;

use crate::{
    api::{ForwardCollection, ForwardFeature, ReverseCollection},
    http::{problem::Problem, request_id::Logged},
    runtime::{self, AppState},
};

/// Every vendor's routes, each under its own prefix.
pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .nest("/compat/google", google::router())
        .nest("/compat/mapbox", mapbox::router())
        .nest("/compat/here", here::router())
        .nest("/compat/precisely", precisely::router())
}

/// og's id as a vendor id: URL-safe base64, so it survives a path segment and
/// a query string unescaped.
pub(crate) fn encode_id(og_id: &str) -> String {
    URL_SAFE_NO_PAD.encode(og_id)
}

/// The og id a vendor id stands for, or `None` for one og never issued.
pub(crate) fn decode_id(vendor_id: &str) -> Option<String> {
    let bytes = URL_SAFE_NO_PAD.decode(vendor_id.trim()).ok()?;
    let id = String::from_utf8(bytes).ok()?;
    // Every og id is `dataset:kind:number`; anything else is not one of ours.
    (id.split(':').count() >= 3).then_some(id)
}

/// The parameters of one native lookup, as its query string would carry them.
#[derive(Debug, Default)]
pub(crate) struct Args(Vec<(&'static str, String)>);

impl Args {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.0.push((name, value.into()));
        self
    }

    /// Sets `name` only when there is a value: an absent vendor parameter is
    /// an absent og one, never an empty one, which og reads differently.
    pub(crate) fn maybe(self, name: &'static str, value: Option<impl Into<String>>) -> Self {
        match value {
            Some(value) => self.set(name, value),
            None => self,
        }
    }

    fn query<T: serde::de::DeserializeOwned>(
        &self,
    ) -> Result<Query<T>, axum::extract::rejection::QueryRejection> {
        let encoded = serde_urlencoded::to_string(&self.0).unwrap_or_default();
        let uri = format!("/?{encoded}")
            .parse()
            .unwrap_or_else(|_| axum::http::Uri::from_static("/"));
        Query::try_from_uri(&uri)
    }
}

/// og's native lookups, called as their routes would be.
pub(crate) mod native {
    use super::*;

    pub(crate) async fn geocode(
        state: &AppState,
        args: Args,
    ) -> Result<ForwardCollection, Problem> {
        runtime::geocode(State(state.clone()), args.query())
            .await
            .map(|json| json.0)
    }

    pub(crate) async fn places(state: &AppState, args: Args) -> Result<ForwardCollection, Problem> {
        runtime::places(State(state.clone()), args.query())
            .await
            .map(|json| json.0)
    }

    pub(crate) async fn autocomplete(
        state: &AppState,
        args: Args,
    ) -> Result<ForwardCollection, Problem> {
        runtime::autocomplete(State(state.clone()), args.query())
            .await
            .map(|json| json.0)
    }

    pub(crate) async fn reverse(
        state: &AppState,
        args: Args,
    ) -> Result<ReverseCollection, Problem> {
        runtime::reverse(State(state.clone()), args.query())
            .await
            .map(|json| json.0)
    }

    /// The record an og id names, or `None` when no record has it. An id
    /// `/places` cannot look up, such as an imported address row's, is
    /// `None` too: those renumber every release.
    pub(crate) async fn by_id(
        state: &AppState,
        og_id: &str,
    ) -> Result<Option<ForwardFeature>, Problem> {
        match places(state, Args::new().set("ids", og_id)).await {
            Ok(collection) => Ok(collection.features.into_iter().next()),
            Err(problem) if problem.status() == 400 => Ok(None),
            Err(problem) => Err(problem),
        }
    }
}

/// What a vendor route answers with on failure: the vendor's body and
/// status, and the Problem behind it, so a 500's cause is still logged.
#[derive(Debug)]
pub(crate) struct VendorError {
    pub(crate) status: StatusCode,
    pub(crate) body: Value,
    pub(crate) headers: Vec<(HeaderName, HeaderValue)>,
    pub(crate) cause: Option<Problem>,
}

impl VendorError {
    pub(crate) fn new(status: StatusCode, body: Value) -> Self {
        Self {
            status,
            body,
            headers: Vec::new(),
            cause: None,
        }
    }

    pub(crate) fn caused_by(mut self, problem: Problem) -> Self {
        self.cause = Some(problem);
        self
    }

    pub(crate) fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.push((name, value));
        self
    }
}

impl IntoResponse for VendorError {
    fn into_response(self) -> Response {
        let mut response = (self.status, axum::Json(self.body)).into_response();
        for (name, value) in self.headers {
            response.headers_mut().insert(name, value);
        }
        if let Some(problem) = self.cause {
            // Not a `Problem` extension: the request-id middleware would
            // rewrite the body into og's own problem document.
            response.extensions_mut().insert(Logged(problem));
        }
        response
    }
}

/// The `Retry-After` og set on a 503, for a vendor error to carry.
pub(crate) fn retry_after(problem: &Problem) -> Option<(HeaderName, HeaderValue)> {
    problem
        .retry_after()
        .map(|seconds| (axum::http::header::RETRY_AFTER, HeaderValue::from(seconds)))
}

/// A header's value as text, when it is sent and readable.
pub(crate) fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// The ISO 3166-1 alpha-2 code of the country a feature lies in.
pub(crate) fn country_code(context: &crate::api::Context) -> Option<&str> {
    context.country.as_ref()?.code.as_deref()
}
