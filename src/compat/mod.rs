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
/// a query string unescaped. An id og's catalogue cannot look up (an
/// imported row, an estimate, an interpolation, a derived postcode) carries
/// `label` after it, which finds the record again (see [`native::by_id`]).
pub(crate) fn encode_id(og_id: &str, label: &str) -> String {
    match crate::ids::parse(og_id) {
        Ok(Some(_)) => URL_SAFE_NO_PAD.encode(og_id),
        _ => URL_SAFE_NO_PAD.encode(format!("{og_id}{LABEL_SEPARATOR}{label}")),
    }
}

/// Between an og id and the label a vendor id carries with it. No id or
/// label holds a line break.
const LABEL_SEPARATOR: char = '\n';

/// What a vendor id names: an og id, and the label of a record the
/// catalogue cannot look up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OgRef {
    pub(crate) id: String,
    pub(crate) label: Option<String>,
}

/// The og id a vendor id stands for, or `None` for one og never issued.
pub(crate) fn decode_id(vendor_id: &str) -> Option<OgRef> {
    let bytes = URL_SAFE_NO_PAD.decode(vendor_id.trim()).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (id, label) = match text.split_once(LABEL_SEPARATOR) {
        Some((id, label)) => (id.to_string(), Some(label.to_string())),
        None => (text, None),
    };
    // Every og id is `dataset:kind:number`; anything else is not one of ours.
    (id.split(':').count() >= 3).then_some(OgRef { id, label })
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

    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    fn copied(&self) -> Self {
        Self(self.0.clone())
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

    /// Free text answered with addresses and places together, as a vendor
    /// search that knows no split between the two does. og keeps them
    /// apart (`/geocode`, `/places`), so both run. An address joins the
    /// answer only when it is what the query names, its street matched and
    /// no part it states unmatched, since `/geocode` answers any text with
    /// its nearest reading. Those addresses come first, the places after.
    pub(crate) async fn search(state: &AppState, args: Args) -> Result<ForwardCollection, Problem> {
        let limit = args
            .get("limit")
            .and_then(|limit| limit.parse::<usize>().ok())
            .filter(|limit| *limit > 0);
        let (addresses, places) =
            tokio::join!(geocode(state, args.copied()), self::places(state, args));
        let (mut addresses, places) = match (addresses, places) {
            (Ok(addresses), Ok(places)) => (addresses, places.features),
            (Ok(addresses), Err(_)) => (addresses, Vec::new()),
            (Err(_), Ok(places)) => return Ok(places),
            (Err(_), Err(problem)) => return Err(problem),
        };
        let limit = limit.unwrap_or_else(|| addresses.features.len().max(places.len()));
        addresses.features.retain(names_address);
        let mut seen = addresses
            .features
            .iter()
            .map(|feature| feature.properties.common.id.clone())
            .collect::<std::collections::HashSet<_>>();
        addresses.features.extend(
            places
                .into_iter()
                .filter(|feature| seen.insert(feature.properties.common.id.clone())),
        );
        addresses.features.truncate(limit);
        Ok(addresses)
    }

    /// Whether an address answer is the address or street the query names.
    fn names_address(feature: &ForwardFeature) -> bool {
        use crate::api::PartMatch;
        feature.properties.matched.as_ref().is_some_and(|found| {
            found.street == Some(PartMatch::Matched)
                && [
                    found.housenumber,
                    found.unit,
                    found.locality,
                    found.region,
                    found.postcode,
                    found.country,
                ]
                .iter()
                .all(|part| *part != Some(PartMatch::Unmatched))
        })
    }

    pub(crate) async fn reverse(
        state: &AppState,
        args: Args,
    ) -> Result<ReverseCollection, Problem> {
        runtime::reverse(State(state.clone()), args.query())
            .await
            .map(|json| json.0)
    }

    /// The places of `categories`, as the catalog writes each one.
    pub(crate) async fn of_categories(
        state: &AppState,
        categories: Vec<&'static str>,
        args: Args,
    ) -> Result<ForwardCollection, Problem> {
        runtime::of_categories(State(state.clone()), categories, args.query())
            .await
            .map(|json| json.0)
    }

    /// The record a vendor id names, or `None` when no record has it,
    /// named in `lang` when it is given.
    pub(crate) async fn by_id(
        state: &AppState,
        named: &OgRef,
        lang: Option<&str>,
    ) -> Result<Option<ForwardFeature>, Problem> {
        Ok(lookup(state, named, lang)
            .await?
            .and_then(|collection| collection.features.into_iter().next()))
    }

    /// The answer holding the record a vendor id names, named in `lang`
    /// when it is given: none when og could never have issued the id, no
    /// feature when no record has it. The catalogue looks up an OSM id.
    /// Any other is found by geocoding the label it carries: the record
    /// with that id, or else the one with that very label, since an
    /// imported row's id renumbers with each release of its data.
    pub(crate) async fn lookup(
        state: &AppState,
        named: &OgRef,
        lang: Option<&str>,
    ) -> Result<Option<ForwardCollection>, Problem> {
        let Some(label) = &named.label else {
            return match places(state, Args::new().set("ids", &named.id).maybe("lang", lang)).await
            {
                Ok(collection) => Ok(Some(collection)),
                Err(problem) if problem.status() == 400 => Ok(None),
                Err(problem) => Err(problem),
            };
        };
        let args = Args::new()
            .set("q", label)
            .set("limit", LABEL_CANDIDATES.to_string())
            .maybe("lang", lang);
        let mut collection = match geocode(state, args).await {
            Ok(collection) => collection,
            Err(problem) if problem.status() == 400 => return Ok(None),
            Err(problem) => return Err(problem),
        };
        keep_named(&mut collection, named);
        Ok(Some(collection))
    }

    /// Keeps of the answers to a vendor id's label only the record it
    /// names: the one with its id and label, else its id, else its label.
    /// An id alone can be read otherwise: a range the label names whole is
    /// also the house its last number names.
    pub(crate) fn keep_named(collection: &mut ForwardCollection, named: &OgRef) {
        let same_id = |feature: &ForwardFeature| feature.properties.common.id == named.id;
        let same_label = |feature: &ForwardFeature| {
            Some(&feature.properties.common.label) == named.label.as_ref()
        };
        let features = std::mem::take(&mut collection.features);
        let at = features
            .iter()
            .position(|feature| same_id(feature) && same_label(feature))
            .or_else(|| features.iter().position(same_id))
            .or_else(|| features.iter().position(same_label));
        collection.features = at
            .and_then(|at| features.into_iter().nth(at))
            .into_iter()
            .collect();
    }

    /// Answers read for the record a label names: enough that the record
    /// is among them though the label also names its neighbours.
    pub(crate) const LABEL_CANDIDATES: usize = 10;
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
