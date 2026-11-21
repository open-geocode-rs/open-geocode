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
pub(crate) mod hours;
pub(crate) mod house;
pub(crate) mod mapbox;
pub(crate) mod precisely;
pub(crate) mod usps;

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
    record::Layer,
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

/// An og reference for a record in a vendor-compatible response.
pub(crate) fn encode_id(common: &crate::api::Common) -> String {
    encode_parts(&common.id, Some(common.layer), &common.label)
}

/// A vendor id of its parts: an og id, the layer of a record (`None` for
/// a context area), and its label or name.
pub(crate) fn encode_parts(id: &str, layer: Option<Layer>, label: &str) -> String {
    OgRef::of(id, layer, label).encode()
}

/// An area's id as a vendor id. A context area's tier is no one layer, and
/// its id names one area, so it carries no layer.
pub(crate) fn encode_area_id(area: &crate::api::ContextArea) -> String {
    encode_parts(&area.id, None, &area.name)
}

/// What a vendor id names. og's id alone does not name one record: an OSM
/// node is a POI and the address it states, and og's catalogue holds no
/// address, estimate or derived postcode. So the layer travels with the id,
/// and a record the catalogue cannot look up carries its label, which finds
/// it again (see [`native::lookup`]). Only a [`CatalogueId`] reaches
/// `/places?ids`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OgRef {
    Catalogued {
        id: CatalogueId,
        layer: Option<Layer>,
    },
    Labelled {
        id: String,
        layer: Option<Layer>,
        label: String,
    },
}

/// An og id the catalogue looks up: an OSM place, area or street. Built
/// only by [`CatalogueId::of`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogueId(String);

impl CatalogueId {
    /// `id` when the catalogue can hold a record of it on `layer`. An
    /// address or a range of one shares its OSM id with a POI or a street
    /// the catalogue holds, but is none of them.
    fn of(id: &str, layer: Option<Layer>) -> Option<Self> {
        let held = matches!(crate::ids::parse(id), Ok(Some(_)))
            && !matches!(layer, Some(Layer::Address | Layer::Interpolation));
        held.then(|| Self(id.to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Between the parts of a vendor id. No id, layer or label holds a line
/// break.
const SEPARATOR: char = '\n';

const OG_ID_PREFIX: &str = "og_";

impl OgRef {
    fn of(id: &str, layer: Option<Layer>, label: &str) -> Self {
        // An area's id is the same whether it is the answer or holds it.
        let layer = layer.filter(|layer| !layer.is_context());
        match CatalogueId::of(id, layer) {
            Some(id) => Self::Catalogued { id, layer },
            None => Self::Labelled {
                id: id.to_string(),
                layer,
                label: label.to_string(),
            },
        }
    }

    pub(crate) fn id(&self) -> &str {
        match self {
            Self::Catalogued { id, .. } => id.as_str(),
            Self::Labelled { id, .. } => id,
        }
    }

    pub(crate) fn layer(&self) -> Option<Layer> {
        match self {
            Self::Catalogued { layer, .. } | Self::Labelled { layer, .. } => *layer,
        }
    }

    fn label(&self) -> Option<&str> {
        match self {
            Self::Catalogued { .. } => None,
            Self::Labelled { label, .. } => Some(label),
        }
    }

    /// URL-safe base64, so it survives a path segment and a query string
    /// unescaped.
    fn encode(&self) -> String {
        let mut text = self.id().to_string();
        if self.layer().is_some() || self.label().is_some() {
            text.push(SEPARATOR);
            text.push_str(self.layer().map_or("", Layer::as_str));
        }
        if let Some(label) = self.label() {
            text.push(SEPARATOR);
            text.push_str(label);
        }
        let mut reference = String::from(OG_ID_PREFIX);
        URL_SAFE_NO_PAD.encode_string(text, &mut reference);
        reference
    }
}

/// What a vendor id names, or `None` for one og never issued: one whose id
/// is not og's, or that asks the catalogue for a record it cannot hold.
pub(crate) fn decode_id(vendor_id: &str) -> Option<OgRef> {
    let id = vendor_id.trim();
    // Keep references issued before the provider prefix was added usable.
    let payload = id.strip_prefix(OG_ID_PREFIX).unwrap_or(id);
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let mut parts = text.splitn(3, SEPARATOR);
    let id = parts.next()?;
    // Every og id is `dataset:kind:number`; anything else is not one of ours.
    if id.split(':').count() < 3 {
        return None;
    }
    let layer = match parts.next() {
        None | Some("") => None,
        Some(layer) => Some(Layer::parse(layer)?),
    };
    Some(match parts.next() {
        Some(label) => OgRef::Labelled {
            id: id.to_string(),
            layer,
            label: label.to_string(),
        },
        None => OgRef::Catalogued {
            id: CatalogueId::of(id, layer)?,
            layer,
        },
    })
}

/// The parameters of one native lookup, as its query string would carry them.
#[derive(Debug, Clone, Default)]
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

    // A vendor answers a house once, og each record of it it holds, so the
    // answers that can hold a house keep one copy of it
    // (see [`house::one_per_house`]).

    pub(crate) async fn geocode(
        state: &AppState,
        args: Args,
    ) -> Result<ForwardCollection, Problem> {
        let mut found = runtime::geocode(State(state.clone()), args.query())
            .await?
            .0;
        house::one_per_house(&mut found.features);
        Ok(found)
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
        let mut found = runtime::autocomplete(State(state.clone()), args.query())
            .await?
            .0;
        house::one_per_house(&mut found.features);
        Ok(found)
    }

    /// Free text answered with addresses, areas and places together, as a
    /// vendor search that knows no split between them does. og keeps them
    /// apart (`/geocode`, `/places`), so both run. An address or an area
    /// joins the answer only when it is what the query names, its street or
    /// its own part matched and no part it states unmatched, since
    /// `/geocode` answers any text with its nearest reading. Those come
    /// first, the places after.
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
        addresses.features.retain(names_answer);
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

    /// Whether a `/geocode` answer is the address, street or area the query
    /// names, which is so when its street, or the part the area is, matched.
    fn names_answer(feature: &ForwardFeature) -> bool {
        use crate::api::PartMatch;
        use crate::record::Layer;
        feature.properties.matched.as_ref().is_some_and(|found| {
            let own = match feature.properties.common.layer {
                Layer::Locality | Layer::Neighbourhood | Layer::District | Layer::Place => {
                    found.locality
                }
                Layer::Region => found.region,
                Layer::Postcode => found.postcode,
                Layer::Country => found.country,
                _ => found.street,
            };
            own == Some(PartMatch::Matched)
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
        let mut found = runtime::reverse(State(state.clone()), args.query())
            .await?
            .0;
        house::one_per_house(&mut found.features);
        Ok(found)
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
        let label = match named {
            OgRef::Labelled { label, .. } => label,
            OgRef::Catalogued { id, .. } => {
                // `/places?ids` answers every record of the id, and of the
                // relation of a region also the country it derives.
                return match places(
                    state,
                    Args::new().set("ids", id.as_str()).maybe("lang", lang),
                )
                .await
                {
                    Ok(mut collection) => {
                        keep_named(&mut collection, named);
                        Ok(Some(collection))
                    }
                    Err(problem) if problem.status() == 400 => Ok(None),
                    Err(problem) => Err(problem),
                };
            }
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

    /// Keeps of the answers only the record a vendor id names, on its
    /// layer: the one with its id and label, else its id, else its label.
    /// An id alone can be read otherwise: a range the label names whole is
    /// also the house its last number names. A label alone is a row whose
    /// id a newer release of its data renumbered.
    pub(crate) fn keep_named(collection: &mut ForwardCollection, named: &OgRef) {
        let same_id = |feature: &ForwardFeature| feature.properties.common.id == named.id();
        let same_label = |feature: &ForwardFeature| {
            Some(feature.properties.common.label.as_str()) == named.label()
        };
        let mut features = std::mem::take(&mut collection.features);
        features.retain(|feature| {
            named
                .layer()
                .is_none_or(|layer| feature.properties.common.layer == layer)
        });
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

/// How far a vendor that states no distance looks for the house nearest a
/// point. The vendors answer the nearest house wherever the point falls on
/// its lot or the street before it, and a lot runs to about 100 m deep.
pub(crate) const NEAREST_HOUSE_RADIUS_M: f64 = 150.0;

/// A vendor's search distance as og's reverse `radius`. It only widens
/// og's search, up to the farthest og looks: a nearer limit is the
/// vendor's filter on what og finds, so og still answers the house and
/// areas it would answer without one.
pub(crate) fn radius(metres: f64) -> String {
    metres
        .clamp(
            crate::reverse::DEFAULT_RADIUS_M,
            crate::reverse::MAX_RADIUS_M,
        )
        .to_string()
}

/// The town a vendor's city slot names: the one the record's address is
/// written with, else the locality holding it. The stated one comes first
/// since it is often the postal town where the holding area names a wider
/// municipality. A stated name that is another tier's area holding the
/// record is no town, and the locality holding the record is: a county
/// (OpenAddresses `district` stands in for an empty `city`) or a district
/// of the town (`Paris 2e Arrondissement`).
pub(crate) fn town<'a>(
    stated: Option<&'a str>,
    holding: Option<&'a str>,
    others: &[Option<&str>],
) -> Option<&'a str> {
    let names_other = |name: &str| {
        others
            .iter()
            .flatten()
            .any(|area| area.eq_ignore_ascii_case(name))
    };
    match stated {
        Some(name) if holding.is_some() && names_other(name) => holding,
        stated => stated.or(holding),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_county_written_as_the_town_gives_way_to_the_town_holding_the_record() {
        let county = Some("Lycoming County");
        assert_eq!(
            town(
                county,
                Some("Montoursville"),
                &[county, Some("Pennsylvania")]
            ),
            Some("Montoursville")
        );
        // A postal town the holding township does not name stays.
        assert_eq!(
            town(Some("Montoursville"), Some("Loyalsock Township"), &[county]),
            Some("Montoursville")
        );
        // A district of the town is no town either.
        assert_eq!(
            town(
                Some("Paris 2e Arrondissement"),
                Some("Paris"),
                &[Some("Paris 2e Arrondissement")]
            ),
            Some("Paris")
        );
        // With no town holding it, the stated name is all og has.
        assert_eq!(town(county, None, &[county]), county);
        assert_eq!(town(None, Some("Troy"), &[]), Some("Troy"));
    }

    #[test]
    fn only_a_place_area_or_street_id_reaches_the_catalogue() {
        let poi = encode_parts("osm:node:7", Some(Layer::Poi), "Diner, 25 Dorrance Street");
        let address = encode_parts("osm:node:7", Some(Layer::Address), "25 Dorrance Street");
        assert_ne!(poi, address, "one node, two records");
        for id in [&poi, &address] {
            let legacy = id.strip_prefix("og_").expect("og-issued prefix");
            assert_eq!(decode_id(id), decode_id(legacy));
        }
        assert!(matches!(decode_id(&poi), Some(OgRef::Catalogued { .. })));
        assert_eq!(
            decode_id(&address),
            Some(OgRef::Labelled {
                id: "osm:node:7".to_string(),
                layer: Some(Layer::Address),
                label: "25 Dorrance Street".to_string(),
            })
        );
        // An address id without its label would ask the catalogue for a
        // record it never holds, so og never issued it.
        let forged = URL_SAFE_NO_PAD.encode("osm:node:7\naddress");
        assert_eq!(decode_id(&forged), None);
        let row = encode_parts(
            "openaddresses:us/ri:row:9",
            Some(Layer::Address),
            "1 Main St",
        );
        assert!(matches!(decode_id(&row), Some(OgRef::Labelled { .. })));
        assert_eq!(
            encode_parts("osm:relation:5", Some(Layer::Region), "Rhode Island"),
            encode_parts("osm:relation:5", None, "Rhode Island"),
            "an area's id is the same as answer and as context"
        );
    }
}
