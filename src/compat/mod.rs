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
pub(crate) mod position;
pub(crate) mod precisely;
pub(crate) mod route;
pub(crate) mod time_zone;
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

    /// These parameters with `name` set to `value` in place of what it was.
    fn with(&self, name: &'static str, value: String) -> Self {
        let mut args = Self(
            self.0
                .iter()
                .filter(|(key, _)| *key != name)
                .cloned()
                .collect(),
        );
        args.0.push((name, value));
        args
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

/// Names each area named in several languages at once, as OSM writes a
/// country of several (`Schweiz/Suisse/Svizzera/Svizra`), in the language of
/// the place, as a vendor names it when no language is asked. That language
/// is the one the nearest area named once is named in: Conthey is Conthey in
/// French and Gundis in German, so a house in it is French-speaking and its
/// country is Suisse. An area whose parts name it in no such language keeps
/// its name.
pub(crate) fn name_joined_areas(pack: &crate::pack::Reader, context: &mut crate::api::Context) {
    let Some(languages) = place_languages(pack, context) else {
        return;
    };
    let mut areas = [
        &mut context.neighbourhood,
        &mut context.sublocality,
        &mut context.locality,
        &mut context.subdistrict,
        &mut context.district,
        &mut context.subregion,
        &mut context.region,
        &mut context.country,
    ];
    let joined = |name: &str| name.contains('/');
    for area in areas.iter_mut().flat_map(|area| area.as_mut()) {
        if !joined(&area.name) {
            continue;
        }
        let parts = area.name.split('/').map(str::trim).collect::<Vec<_>>();
        // The first part in the place's language, where it speaks several.
        let names = pack.names_by_language(area.record_id);
        let local = parts
            .iter()
            .find(|part| {
                names
                    .iter()
                    .any(|(code, name)| languages.contains(code) && name == *part)
            })
            .map(|part| (*part).to_string());
        if let Some(local) = local {
            // The joined name stays its local name, so it tells no language
            // the part was picked by ([`place_language`]).
            let joined = std::mem::replace(&mut area.name, local);
            area.local_name.get_or_insert(joined);
        }
    }
}

/// The languages an area's name is written in, those that name it so:
/// Conthey is Conthey in French and Italian and Gundis in German, so it
/// speaks French or Italian. None for a name joined from several
/// languages, or one no language names it by.
fn speaking(pack: &crate::pack::Reader, area: &crate::api::ContextArea) -> Option<Vec<String>> {
    // An area renamed in the language asked is read by its local name.
    let local = area.local_name.as_deref().unwrap_or(&area.name);
    if local.contains('/') {
        return None;
    }
    let names = pack.names_by_language(area.record_id);
    let speaking = names
        .iter()
        .filter(|(_, name)| name == local)
        .map(|(code, _)| code.clone())
        .collect::<Vec<_>>();
    (!speaking.is_empty()).then_some(speaking)
}

/// The languages a place speaks, which its local names are written in.
/// They are the languages its nearest area is named in ([`speaking`]),
/// narrowed by each area farther out while that leaves more than one. None
/// when no area tells a language.
fn place_languages(
    pack: &crate::pack::Reader,
    context: &crate::api::Context,
) -> Option<Vec<String>> {
    let mut languages: Option<Vec<String>> = None;
    for area in context.areas() {
        let Some(spoken) = speaking(pack, area) else {
            continue;
        };
        languages = Some(match languages {
            None => spoken,
            Some(held) => {
                let narrowed = held
                    .iter()
                    .filter(|code| spoken.contains(code))
                    .cloned()
                    .collect::<Vec<_>>();
                if narrowed.is_empty() { held } else { narrowed }
            }
        });
        if languages.as_ref().is_some_and(|held| held.len() == 1) {
            break;
        }
    }
    languages
}

/// The one language a place speaks. It is the default language the nearest
/// boundary stating one names (`default_language`), when some area of the
/// place is named in it by its local name: the United States state English,
/// and a town named alike only in Occitan still speaks it. A default no area
/// is named in tells nothing (Switzerland's is Latin), and then the areas'
/// names alone tell it ([`place_languages`]), when they leave one.
pub(crate) fn place_language(
    pack: &crate::pack::Reader,
    context: &crate::api::Context,
) -> Option<String> {
    let named_in = |code: &str| {
        context.areas().any(|area| {
            speaking(pack, area).is_some_and(|spoken| spoken.iter().any(|held| held == code))
        })
    };
    let defaults = context
        .areas()
        .map(|area| pack.default_languages(area.record_id).collect::<Vec<_>>())
        .find(|defaults| !defaults.is_empty())
        .unwrap_or_default();
    if let Some(code) = defaults.into_iter().find(|code| named_in(code)) {
        return Some(code.to_string());
    }
    match place_languages(pack, context)?.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// An area's name without the word for its kind, as an address writes it:
/// `Suffolk` of `Suffolk County`, `Oder-Spree` of `Landkreis Oder-Spree`.
/// The kind words are og's parser's, from libpostal's lists in every
/// language. A name with none, or whose rest is not whole words of it,
/// stays as it is.
pub(crate) fn without_kind(name: &str) -> String {
    let Some(key) = crate::util::fold::search_key(name) else {
        return name.to_string();
    };
    let words = name.split_whitespace().collect::<Vec<_>>();
    let mut best: Option<String> = None;
    for alias in crate::parse::lattice::dictionary::admin_aliases(&key) {
        for start in 0..words.len() {
            for end in start + 1..=words.len() {
                let span = words[start..end].join(" ");
                if (end - start) < words.len()
                    && crate::util::fold::search_key(&span).as_deref() == Some(alias.as_str())
                    && best.as_ref().is_none_or(|kept| span.len() > kept.len())
                {
                    best = Some(span);
                }
            }
        }
    }
    best.unwrap_or_else(|| name.to_string())
}

/// Keeps the results that match the query no worse than the first, when
/// the first matches every part the query states, fully or in part: a
/// vendor answers the address it found, not the near misses og also holds
/// ("Back Bay, Boston" is the district, not Back Bay Road in Newark). A
/// first result that leaves a stated part unmatched is no sure match, and
/// every result stays. Of the areas kept, one that holds another kept area
/// goes too, so the narrowest of nested matches answers. Paris the city
/// answers, not the department of the same name that holds it, while Köln
/// and the Cologne in Italy both stay.
pub(crate) fn best_matches(features: &mut Vec<crate::api::ForwardFeature>) {
    use crate::api::{PartMatch, QueryMatch};
    fn parts(found: QueryMatch) -> [Option<PartMatch>; 7] {
        [
            found.housenumber,
            found.street,
            found.unit,
            found.locality,
            found.region,
            found.postcode,
            found.country,
        ]
    }
    fn rank(part: Option<PartMatch>) -> u8 {
        match part {
            Some(PartMatch::Matched) => 0,
            Some(PartMatch::Partial) => 1,
            Some(PartMatch::Unmatched) | None => 2,
        }
    }
    let Some(first) = features
        .first()
        .and_then(|feature| feature.properties.matched)
        .filter(|found| !found.is_empty())
        .map(parts)
    else {
        return;
    };
    if first.contains(&Some(PartMatch::Unmatched)) {
        return;
    }
    features.retain(|feature| {
        feature.properties.matched.is_some_and(|found| {
            parts(found)
                .iter()
                .zip(&first)
                .all(|(part, best)| best.is_none() || rank(*part) <= rank(*best))
        })
    });
    let held = features
        .iter()
        .filter(|feature| feature.properties.common.layer.is_context())
        .flat_map(|feature| feature.properties.common.context.areas())
        .map(|area| area.record_id)
        .collect::<std::collections::HashSet<_>>();
    features.retain(|feature| {
        let common = &feature.properties.common;
        !(common.layer.is_context() && common.record_id.is_some_and(|id| held.contains(&id)))
    });
}

/// og's native lookups, called as their routes would be.
/// Gives an address whose source states no postcode the one the other
/// addresses of its street agree on (`street_postcode`).
fn with_street_postcode(address: &mut crate::api::Address, street_postcode: Option<&String>) {
    if address.postcode.is_none() {
        address.postcode = street_postcode.cloned();
    }
}

pub(crate) mod native {
    use super::*;

    // A vendor answers a house once, og each record of it it holds, so the
    // answers that can hold a house keep one copy of it
    // (see [`house::one_per_house`]).

    /// The answer as a vendor writes it ([`as_vendors_write`]).
    fn local_forward(
        state: &AppState,
        asked: bool,
        mut found: ForwardCollection,
    ) -> ForwardCollection {
        for feature in &mut found.features {
            as_vendors_write(state, asked, &mut feature.properties.common);
        }
        found
    }

    /// A result as a vendor writes it. Each area named in several languages
    /// at once is named in the place's own when no language was asked
    /// ([`name_joined_areas`]). An address whose source states no postcode
    /// has the one the other addresses of its street agree on, since a
    /// vendor writes every address with its postcode.
    fn as_vendors_write(state: &AppState, asked: bool, common: &mut crate::api::Common) {
        if !asked {
            name_joined_areas(state.pack(), &mut common.context);
        }
        with_street_postcode(&mut common.address, common.street_postcode.as_ref());
    }

    /// Runs a lookup and keeps one copy of each house in its answer. Where
    /// the caller asked for a number of answers and copies leave fewer, og
    /// is asked for more, up to `most`, the route's own limit, so the answer
    /// holds as many houses as og has.
    async fn one_per_house<C, A, Fut>(
        args: Args,
        most: usize,
        features: fn(&mut C) -> &mut Vec<A>,
        run: impl Fn(Args) -> Fut,
    ) -> Result<C, Problem>
    where
        A: house::Answer,
        Fut: std::future::Future<Output = Result<C, Problem>>,
    {
        let asked = args
            .get("limit")
            .and_then(|limit| limit.parse::<usize>().ok());
        let mut ask = asked.unwrap_or(0);
        loop {
            let mut found = run(args.with("limit", ask.to_string())).await?;
            let answers = features(&mut found);
            let held = answers.len();
            house::one_per_house(answers);
            let Some(asked) = asked else {
                return Ok(found);
            };
            match ask_again(asked, ask, held, answers.len(), most) {
                Some(more) => ask = more,
                None => {
                    answers.truncate(asked);
                    return Ok(found);
                }
            }
        }
    }

    /// How many answers to ask og for next, having asked for `ask`, been
    /// answered `held` and kept `kept` houses of the `asked`. `None` once
    /// enough are kept, og has no more, or `most` was asked.
    pub(super) fn ask_again(
        asked: usize,
        ask: usize,
        held: usize,
        kept: usize,
        most: usize,
    ) -> Option<usize> {
        let more = (ask * 2).min(most);
        (kept < asked && held >= ask && more > ask).then_some(more)
    }

    pub(crate) async fn geocode(
        state: &AppState,
        args: Args,
    ) -> Result<ForwardCollection, Problem> {
        let args_lang = args.get("lang").is_some();
        one_per_house(
            args,
            crate::http::bounds::MAX_SEARCH_LIMIT,
            |found: &mut ForwardCollection| &mut found.features,
            |args| async move {
                Ok(runtime::geocode(State(state.clone()), args.query())
                    .await?
                    .0)
            },
        )
        .await
        .map(|found| local_forward(state, args_lang, found))
    }

    pub(crate) async fn places(state: &AppState, args: Args) -> Result<ForwardCollection, Problem> {
        let args_lang = args.get("lang").is_some();
        runtime::places(State(state.clone()), args.query())
            .await
            .map(|json| local_forward(state, args_lang, json.0))
    }

    pub(crate) async fn autocomplete(
        state: &AppState,
        args: Args,
    ) -> Result<ForwardCollection, Problem> {
        let args_lang = args.get("lang").is_some();
        one_per_house(
            args,
            crate::search::MAX_AUTOCOMPLETE_LIMIT,
            |found: &mut ForwardCollection| &mut found.features,
            |args| async move {
                Ok(runtime::autocomplete(State(state.clone()), args.query())
                    .await?
                    .0)
            },
        )
        .await
        .map(|found| local_forward(state, args_lang, found))
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
        let args_lang = args.get("lang").is_some();
        one_per_house(
            args,
            crate::http::bounds::MAX_SEARCH_LIMIT,
            |found: &mut ReverseCollection| &mut found.features,
            |args| async move {
                Ok(runtime::reverse(State(state.clone()), args.query())
                    .await?
                    .0)
            },
        )
        .await
        .map(|mut found| {
            for feature in &mut found.features {
                as_vendors_write(state, args_lang, &mut feature.properties.common);
            }
            found
        })
    }

    /// The places og holds within `radius_m` of `point`, the most prominent
    /// first (QRank, a Wikidata link, names in many languages), then the
    /// nearest, as many as `limit`, the place `except` left out.
    pub(crate) async fn landmarks(
        state: &AppState,
        point: [f64; 2],
        radius_m: f64,
        limit: usize,
        lang: Option<String>,
        except: &str,
    ) -> Result<Vec<ForwardFeature>, Problem> {
        let args = Args::new()
            .set("limit", crate::http::bounds::MAX_SEARCH_LIMIT.to_string())
            .set("focus", format!("{},{}", point[0], point[1]))
            .set("radius", radius_m.to_string())
            .maybe("lang", lang);
        let found = of_categories(state, crate::categories::keys(), args).await?;
        let records = state.pack().records();
        let mut ranked = found
            .features
            .into_iter()
            .filter(|feature| feature.properties.common.id != except)
            .map(|feature| {
                let prominence = feature
                    .properties
                    .common
                    .record_id
                    .and_then(|id| records.prominence(id).ok().flatten())
                    .unwrap_or_default();
                (prominence, feature)
            })
            .collect::<Vec<_>>();
        let distance = |feature: &ForwardFeature| feature.properties.distance_m.unwrap_or(f64::MAX);
        ranked.sort_by(|(left, a), (right, b)| {
            right.cmp(left).then(distance(a).total_cmp(&distance(b)))
        });
        Ok(ranked
            .into_iter()
            .take(limit)
            .map(|(_, feature)| feature)
            .collect())
    }

    /// The places of `categories`, as the catalog writes each one.
    pub(crate) async fn of_categories(
        state: &AppState,
        categories: Vec<&'static str>,
        args: Args,
    ) -> Result<ForwardCollection, Problem> {
        let args_lang = args.get("lang").is_some();
        runtime::of_categories(State(state.clone()), categories, args.query())
            .await
            .map(|json| local_forward(state, args_lang, json.0))
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
        // The locality's own spelling, where the source shouts it (`BOSTON`).
        Some(name) if holding.is_some_and(|holding| holding.eq_ignore_ascii_case(name)) => holding,
        stated => stated.or(holding),
    }
}

/// The town of a result as a vendor writes it, by [`town`] over the areas
/// holding it.
pub(crate) fn town_of<'a>(
    address: &'a crate::api::Address,
    context: &'a crate::api::Context,
) -> Option<&'a str> {
    let name =
        |area: &'a Option<crate::api::ContextArea>| area.as_ref().map(|area| area.name.as_str());
    town(
        address.locality.as_deref(),
        name(&context.locality),
        &[
            name(&context.neighbourhood),
            name(&context.sublocality),
            name(&context.district),
            name(&context.subregion),
            name(&context.region),
        ],
    )
}

/// A US street in the postal abbreviation, by og's split of it where og
/// made one: `Amphitheatre Pkwy`, `W El Camino Real`.
pub(crate) fn us_street(address: &crate::api::Address) -> Option<String> {
    let street = address.street.as_deref()?;
    let parts = address.street_parts.as_ref();
    Some(usps::display_street(
        parts.map_or(street, |parts| parts.name.as_str()),
        parts.and_then(|parts| parts.prefix.as_deref()),
        parts.and_then(|parts| parts.kind.as_deref()),
        parts.and_then(|parts| parts.suffix.as_deref()),
    ))
}

/// A result's postal lines in its country's form, as a vendor writes them.
/// They are rendered again from og's parts with the town a vendor writes
/// ([`town_of`]) and, in the US, the street in the postal abbreviation, so
/// no rendered text is rewritten. Empty where og has no lines for it.
pub(crate) fn postal_lines(
    layer: Layer,
    address: &crate::api::Address,
    context: &crate::api::Context,
) -> Vec<String> {
    let mut written = address.clone();
    written.locality = town_of(address, context).map(str::to_string);
    if country_code(context) == Some("US") {
        written.street = us_street(address).or(written.street);
    }
    crate::address_lines::address_lines(layer, &written, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_area_is_named_without_the_word_for_its_kind() {
        assert_eq!(without_kind("Suffolk County"), "Suffolk");
        assert_eq!(without_kind("Landkreis Oder-Spree"), "Oder-Spree");
        assert_eq!(without_kind("Freistaat Bayern"), "Bayern");
        assert_eq!(without_kind("Berlin"), "Berlin");
        assert_eq!(without_kind("County Cork"), "Cork");
        // A kind word inside a word is none.
        assert_eq!(without_kind("Statesville"), "Statesville");
    }

    fn area(name: &str, code: Option<&str>) -> Option<crate::api::ContextArea> {
        Some(crate::api::ContextArea {
            id: format!("osm:relation:{}", name.len()),
            name: name.to_string(),
            code: code.map(str::to_string),
            translations: None,
            record_id: 0,
            local_name: None,
            wikidata: None,
            plate_code: None,
        })
    }

    fn house(number: &str, street: &str, locality: &str, postcode: &str) -> crate::api::Address {
        crate::api::Address {
            housenumber: Some(number.to_string()),
            street: Some(street.to_string()),
            locality: Some(locality.to_string()),
            postcode: Some(postcode.to_string()),
            ..crate::api::Address::default()
        }
    }

    /// og is asked for more answers only while copies leave too few houses,
    /// og had more to give, and never past the route's own limit.
    #[test]
    fn an_address_stating_no_postcode_takes_the_one_its_street_agrees_on() {
        let filled = |postcode: Option<&str>, street: Option<&str>| {
            let mut address = crate::api::Address {
                postcode: postcode.map(str::to_string),
                ..house("1", "Redondo Street", "San Francisco", "")
            };
            with_street_postcode(&mut address, street.map(str::to_string).as_ref());
            address.postcode
        };
        assert_eq!(filled(None, Some("94124")).as_deref(), Some("94124"));
        assert_eq!(
            filled(Some("94134"), Some("94124")).as_deref(),
            Some("94134"),
            "the source's own postcode stands"
        );
        assert_eq!(filled(None, None), None);
    }

    #[test]
    fn og_is_asked_again_only_while_it_can_answer_more_houses() {
        // Asked 3, og answered 3 of which 2 houses: ask 6.
        assert_eq!(native::ask_again(3, 3, 3, 2, 50), Some(6));
        // Enough houses kept.
        assert_eq!(native::ask_again(3, 3, 3, 3, 50), None);
        // og had fewer than asked, so it has no more.
        assert_eq!(native::ask_again(3, 6, 5, 2, 50), None);
        // Never past the route's limit, and not again at it.
        assert_eq!(native::ask_again(15, 15, 15, 9, 20), Some(20));
        assert_eq!(native::ask_again(20, 20, 20, 9, 20), None);
    }

    /// A house's lines are rendered from its parts in its country's form,
    /// with the town a vendor writes: the locality holding it where the
    /// source wrote a district of it or shouted its name.
    #[test]
    fn postal_lines_are_rendered_in_the_town_a_vendor_writes() {
        let vienna = crate::api::Context {
            sublocality: area("Leopoldstadt", None),
            locality: area("Wien", None),
            country: area("Austria", Some("AT")),
            ..crate::api::Context::default()
        };
        assert_eq!(
            postal_lines(
                Layer::Address,
                &house("42", "Heinestraße", "Leopoldstadt", "1020"),
                &vienna
            ),
            ["Heinestraße 42", "1020 Wien", "Austria"]
        );
        let paris = crate::api::Context {
            sublocality: area("Paris 2e Arrondissement", None),
            locality: area("Paris", None),
            country: area("France", Some("FR")),
            ..crate::api::Context::default()
        };
        assert_eq!(
            postal_lines(
                Layer::Address,
                &house("5", "Rue Daunou", "Paris 2e Arrondissement", "75002"),
                &paris
            ),
            ["5 Rue Daunou", "75002 Paris", "France"]
        );
        let boston = crate::api::Context {
            locality: area("Boston", None),
            region: area("Massachusetts", Some("US-MA")),
            country: area("United States", Some("US")),
            ..crate::api::Context::default()
        };
        assert_eq!(
            postal_lines(
                Layer::Address,
                &house("240", "Washington Street", "BOSTON", "02108"),
                &boston
            ),
            ["240 Washington St", "Boston, MA 02108", "United States"]
        );
    }

    /// With no language asked, a country OSM names in several languages at
    /// once is named in the language of the place, on every vendor.
    #[tokio::test]
    async fn a_country_named_in_several_languages_takes_the_places_own() {
        use crate::runtime::router_tests::{state_for, temp_path, write_stated_names_pack};
        use axum::body::Body;
        use tower::ServiceExt;
        let pack = temp_path("vendor-joined-names");
        write_stated_names_pack(&pack);
        let router = runtime::build_router(state_for(&pack, true), None, None);
        let get = |uri: &str| {
            let router = router.clone();
            let request = axum::http::Request::get(uri)
                .body(Body::empty())
                .expect("request");
            async move {
                let reply = router.oneshot(request).await.expect("reply");
                let bytes = axum::body::to_bytes(reply.into_body(), usize::MAX)
                    .await
                    .expect("body");
                serde_json::from_slice::<Value>(&bytes).expect("json")
            }
        };
        let q = "q=67%20Route%20de%20la%20T%C3%A9l%C3%A9cabine";
        let here = get(&format!("/compat/here/v1/geocode?{q}")).await;
        assert_eq!(
            here["items"][0]["address"]["countryName"], "Suisse",
            "{here}"
        );
        let mapbox = get(&format!("/compat/mapbox/search/geocode/v6/forward?{q}")).await;
        assert_eq!(
            mapbox["features"][0]["properties"]["context"]["country"]["name"], "Suisse",
            "{mapbox}"
        );
        // Asked in a language, the country is named in it, as before.
        let german = get(&format!("/compat/here/v1/geocode?{q}&lang=de")).await;
        assert_eq!(
            german["items"][0]["address"]["countryName"], "Schweiz",
            "{german}"
        );
    }

    /// A name says the language it is written in. A local name is in the
    /// language its place speaks, told by its areas' names, and a name og
    /// holds in the language asked is in that one. A number or a code is in
    /// none.
    #[tokio::test]
    async fn a_local_name_is_in_the_language_its_place_speaks() {
        use crate::runtime::router_tests::{state_for, temp_path, write_stated_names_pack};
        use axum::body::Body;
        use tower::ServiceExt;
        let pack = temp_path("vendor-local-language");
        write_stated_names_pack(&pack);
        let router = runtime::build_router(state_for(&pack, true), None, None);
        let get = |uri: String| {
            let router = router.clone();
            let request = axum::http::Request::get(uri)
                .body(Body::empty())
                .expect("request");
            async move {
                let reply = router.oneshot(request).await.expect("reply");
                let bytes = axum::body::to_bytes(reply.into_body(), usize::MAX)
                    .await
                    .expect("body");
                serde_json::from_slice::<Value>(&bytes).expect("json")
            }
        };
        let q = "addressQuery=1%20Marienplatz";
        let languages = |answer: &Value| {
            let result = &answer["results"][0];
            let mut seen = result["addressComponents"]
                .as_array()
                .expect("components")
                .iter()
                .map(|component| {
                    (
                        component["types"][0].as_str().expect("a type").to_string(),
                        component["languageCode"].as_str().map(str::to_string),
                    )
                })
                .collect::<Vec<_>>();
            seen.push((
                "postalAddress".to_string(),
                result["postalAddress"]["languageCode"]
                    .as_str()
                    .map(str::to_string),
            ));
            seen
        };
        let local = get(format!("/compat/google/v4/geocode/address?{q}")).await;
        let of = |seen: &[(String, Option<String>)], kind: &str| {
            seen.iter()
                .find(|(found, _)| found == kind)
                .and_then(|(_, code)| code.clone())
        };
        let seen = languages(&local);
        assert_eq!(of(&seen, "route").as_deref(), Some("de"), "{local}");
        assert_eq!(of(&seen, "country").as_deref(), Some("de"), "{local}");
        assert_eq!(of(&seen, "postalAddress").as_deref(), Some("de"), "{local}");
        assert_eq!(of(&seen, "street_number"), None, "{local}");
        assert_eq!(of(&seen, "postal_code"), None, "{local}");
        let english = get(format!(
            "/compat/google/v4/geocode/address?{q}&languageCode=en"
        ))
        .await;
        let seen = languages(&english);
        assert_eq!(of(&seen, "country").as_deref(), Some("en"), "{english}");
        assert_eq!(of(&seen, "route").as_deref(), Some("de"), "{english}");
        // A place whose areas are named alike in French and Italian, under a
        // country named in four languages at once, speaks no one language
        // og can tell, asked in a language or not.
        let swiss = "addressQuery=67%20Route%20de%20la%20T%C3%A9l%C3%A9cabine";
        for asked in ["", "&languageCode=de"] {
            let answer = get(format!("/compat/google/v4/geocode/address?{swiss}{asked}")).await;
            assert_eq!(of(&languages(&answer), "route"), None, "{answer}");
        }
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// A country and a state named alike in English and Jamaican Patois,
    /// above a county and town named in no other language, under a country
    /// whose default language is English, and a county with plate code SIM.
    fn write_defaults_pack(dir: &std::path::Path) {
        use crate::{
            pack::{RecordContext, Writer},
            record::{
                AddressComponents, AddressRecord, LocationPrecision, ObjectType, PlaceLayer,
                PlaceRecord, Record, SourceProvenance, point_geometry,
            },
        };
        let _ = std::fs::remove_dir_all(dir);
        let mut writer = Writer::create(dir).expect("writer");
        let mut area = |layer, name: &str, id: i64| {
            let record = Record::Place(
                layer,
                PlaceRecord {
                    alt_names: Vec::new(),
                    name: name.to_string(),
                    place_type: "admin_level:4".to_string(),
                    geometry: point_geometry(-122.08, 37.42),
                    source: SourceProvenance::osm(ObjectType::Relation, id),
                    prominence: Default::default(),
                    wikidata: None,
                },
            );
            writer.write(&record, None).expect("area")
        };
        let country = area(PlaceLayer::Country, "United States", 1);
        let state = area(PlaceLayer::Region, "California", 2);
        let county = area(PlaceLayer::District, "Rhein-Hunsrück-Kreis", 3);
        let town = area(PlaceLayer::Locality, "Mountain View", 4);
        let named = |names: &[(&str, &str)]| {
            names
                .iter()
                .map(|(code, name)| (code.to_string(), name.to_string()))
                .collect::<Vec<_>>()
        };
        writer.set_language_names(
            [
                (
                    country,
                    named(&[
                        ("en", "United States"),
                        ("jam", "United States"),
                        ("fr", "États-Unis"),
                    ]),
                ),
                // A minor language copying the town's local name tells
                // nothing of what the town speaks.
                (town, named(&[("oc", "Mountain View"), ("zh", "山景城")])),
                (
                    state,
                    named(&[
                        ("en", "California"),
                        ("jam", "California"),
                        ("es", "California"),
                        ("fr", "Californie"),
                    ]),
                ),
            ]
            .into_iter()
            .collect(),
        );
        writer.set_default_languages([(country, "en".to_string())].into_iter().collect());
        writer.set_country_codes(
            [(
                country,
                crate::parse::lattice::CountryCode::parse("US").expect("code"),
            )]
            .into_iter()
            .collect(),
        );
        writer.set_plate_codes([(county, "SIM".to_string())].into_iter().collect());
        let context = Some(RecordContext {
            admin_context: crate::context::AdminContextTuple::from_tiers([
                (PlaceLayer::Country, country),
                (PlaceLayer::Region, state),
                (PlaceLayer::District, county),
                (PlaceLayer::Locality, town),
            ]),
            flags: 0,
            zone: None,
        });
        let house = AddressRecord {
            building: Some(crate::record::Building {
                name: Some("Google Building 41".to_string()),
            }),
            access: Vec::new(),
            disputed: false,
            street_postcodes: Vec::new(),
            address: AddressComponents {
                areas: Vec::new(),
                number: "1600".to_string(),
                street: Some("Amphitheatre Parkway".to_string()),
                place: None,
                unit: None,
                locality: None,
                region: None,
                postcode: Some("94043".to_string()),
                country: None,
            },
            geometry: point_geometry(-122.084, 37.422),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, 10),
        };
        writer
            .write(&house.clone().into(), context.clone())
            .expect("house");
        // A house whose address point is its main entrance.
        let door = crate::record::Entrance::of_tag(Some("main"), -122.09, 37.43)
            .into_iter()
            .collect::<Vec<_>>();
        let front = AddressRecord {
            building: None,
            access: door,
            address: AddressComponents {
                number: "7".to_string(),
                street: Some("Door Street".to_string()),
                ..house.address.clone()
            },
            geometry: point_geometry(-122.09, 37.43),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, 11),
            ..house
        };
        writer.write(&front.into(), context).expect("front door");
        writer.finish().expect("finish");
    }

    /// Where a place's areas are named alike in several languages, the
    /// default language its boundary states picks among them, and a
    /// county's plate code is HERE's county code.
    #[tokio::test]
    async fn a_default_language_picks_among_alike_names_and_a_plate_code_is_the_county_code() {
        use crate::runtime::router_tests::{state_for, temp_path};
        use axum::body::Body;
        use tower::ServiceExt;
        let pack = temp_path("vendor-default-language");
        write_defaults_pack(&pack);
        let router = runtime::build_router(state_for(&pack, true), None, None);
        let get = |uri: &str| {
            let router = router.clone();
            let request = axum::http::Request::get(uri)
                .body(Body::empty())
                .expect("request");
            async move {
                let reply = router.oneshot(request).await.expect("reply");
                let bytes = axum::body::to_bytes(reply.into_body(), usize::MAX)
                    .await
                    .expect("body");
                serde_json::from_slice::<Value>(&bytes).expect("json")
            }
        };
        let google =
            get("/compat/google/v4/geocode/address?addressQuery=1600%20Amphitheatre%20Parkway")
                .await;
        let route = google["results"][0]["addressComponents"]
            .as_array()
            .expect("components")
            .iter()
            .find(|component| component["types"][0] == "route")
            .cloned()
            .expect("a route");
        assert_eq!(route["languageCode"], "en", "{google}");
        let here = get("/compat/here/v1/geocode?q=1600%20Amphitheatre%20Parkway").await;
        assert_eq!(here["items"][0]["address"]["countyCode"], "SIM", "{here}");
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// An address drawn as a named building names it as Google's premise,
    /// and Precisely says its point stands on the building, or on a door
    /// when the point is the address's own main entrance.
    #[tokio::test]
    async fn a_building_is_the_premise_and_a_door_is_a_door() {
        use crate::runtime::router_tests::{state_for, temp_path};
        use axum::body::Body;
        use tower::ServiceExt;
        let pack = temp_path("vendor-premise");
        write_defaults_pack(&pack);
        let router = runtime::build_router(state_for(&pack, true), None, None);
        let call = |request: axum::http::Request<Body>| {
            let router = router.clone();
            async move {
                let reply = router.oneshot(request).await.expect("reply");
                let bytes = axum::body::to_bytes(reply.into_body(), usize::MAX)
                    .await
                    .expect("body");
                serde_json::from_slice::<Value>(&bytes).expect("json")
            }
        };
        let get = |uri: &str| {
            axum::http::Request::get(uri)
                .body(Body::empty())
                .expect("request")
        };
        let google = call(get(
            "/compat/google/v4/geocode/address?addressQuery=1600%20Amphitheatre%20Parkway",
        ))
        .await;
        let first = &google["results"][0]["addressComponents"][0];
        assert_eq!(
            (
                &first["longText"],
                &first["types"][0],
                &first["languageCode"]
            ),
            (
                &serde_json::json!("Google Building 41"),
                &serde_json::json!("premise"),
                &serde_json::json!("en")
            ),
            "{google}"
        );
        let precisely = |line: &str| {
            let body =
                serde_json::json!({"addresses": [{"addressLines": [line], "country": "USA"}]});
            axum::http::Request::post("/compat/precisely/v1/geocode")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("request")
        };
        for (line, description) in [
            ("1600 Amphitheatre Parkway", "BUILDING"),
            ("7 Door Street", "FRONT_DOOR"),
        ] {
            let answer = call(precisely(line)).await;
            assert_eq!(
                answer["responses"][0]["results"][0]["location"]["explanation"]["description"],
                description,
                "{answer}"
            );
        }
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// A town inside a county of the same name answers alone, as the
    /// narrowest of nested matches, while two towns far apart that a name
    /// matches whole both answer, one by another of its names.
    #[tokio::test]
    async fn the_narrowest_of_nested_areas_answers_and_unrelated_ones_stay() {
        use crate::{
            pack::{RecordContext, Writer},
            record::{
                ObjectType, PlaceLayer, PlaceRecord, Record, SourceProvenance, point_geometry,
            },
            runtime::router_tests::{state_for, temp_path},
        };
        use axum::body::Body;
        use tower::ServiceExt;
        let pack = temp_path("vendor-nested-areas");
        let _ = std::fs::remove_dir_all(&pack);
        let mut writer = Writer::create(&pack).expect("writer");
        let mut area = |layer,
                        name: &str,
                        alt: &[&str],
                        id: i64,
                        at: [f64; 2],
                        within: &[(PlaceLayer, u64)]| {
            let record = Record::Place(
                layer,
                PlaceRecord {
                    alt_names: alt.iter().map(|name| name.to_string()).collect(),
                    name: name.to_string(),
                    place_type: "admin_level:8".to_string(),
                    geometry: point_geometry(at[0], at[1]),
                    source: SourceProvenance::osm(ObjectType::Relation, id),
                    prominence: Default::default(),
                    wikidata: None,
                },
            );
            let context = (!within.is_empty()).then(|| RecordContext {
                admin_context: crate::context::AdminContextTuple::from_tiers(
                    within.iter().copied(),
                ),
                flags: 0,
                zone: None,
            });
            writer.write(&record, context).expect("area")
        };
        let county = area(
            PlaceLayer::District,
            "Providence County",
            &["Providence"],
            1,
            [-71.5, 41.8],
            &[],
        );
        area(
            PlaceLayer::Locality,
            "Providence",
            &[],
            2,
            [-71.41, 41.82],
            &[(PlaceLayer::District, county)],
        );
        area(
            PlaceLayer::Locality,
            "Köln",
            &["Cologne"],
            3,
            [6.96, 50.94],
            &[],
        );
        area(PlaceLayer::Locality, "Cologne", &[], 4, [10.4, 45.6], &[]);
        writer.finish().expect("finish");
        let router = runtime::build_router(state_for(&pack, true), None, None);
        let titles = |q: &str| {
            let router = router.clone();
            let request = axum::http::Request::get(format!("/compat/here/v1/geocode?q={q}"))
                .body(Body::empty())
                .expect("request");
            async move {
                let reply = router.oneshot(request).await.expect("reply");
                let bytes = axum::body::to_bytes(reply.into_body(), usize::MAX)
                    .await
                    .expect("body");
                let body = serde_json::from_slice::<Value>(&bytes).expect("json");
                let mut titles = body["items"]
                    .as_array()
                    .expect("items")
                    .iter()
                    .map(|item| item["title"].as_str().expect("title").to_string())
                    .collect::<Vec<_>>();
                titles.sort();
                titles
            }
        };
        assert_eq!(titles("Providence").await, ["Providence"]);
        assert_eq!(titles("Cologne").await, ["Cologne", "Köln"]);
        let _ = std::fs::remove_dir_all(&pack);
    }

    /// Every vendor names a house's town and country as og's answer in the
    /// language asked does, never the source's spelling beside translated
    /// areas.
    #[tokio::test]
    async fn every_vendor_names_a_stated_town_in_the_language_asked() {
        use crate::runtime::router_tests::{state_for, temp_path, write_stated_names_pack};
        use axum::body::Body;
        use tower::ServiceExt;
        let pack = temp_path("vendor-stated-names");
        write_stated_names_pack(&pack);
        let router = runtime::build_router(state_for(&pack, true), None, None);
        let call = |request: axum::http::Request<Body>| {
            let router = router.clone();
            async move {
                let reply = router.oneshot(request).await.expect("reply");
                let bytes = axum::body::to_bytes(reply.into_body(), usize::MAX)
                    .await
                    .expect("body");
                serde_json::from_slice::<Value>(&bytes).expect("json")
            }
        };
        let get = |uri: String| {
            call(
                axum::http::Request::get(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
        };
        for (lang, town, country) in [
            ("fr", "Vienne", "Autriche"),
            ("ru", "Вена", "Австрия"),
            ("de", "Wien", "Österreich"),
        ] {
            let q = "q=42%20Heinestra%C3%9Fe";
            let google = get(format!(
                "/compat/google/v4/geocode/address?addressQuery=42%20Heinestra%C3%9Fe&languageCode={lang}"
            ))
            .await;
            let result = &google["results"][0];
            assert_eq!(result["postalAddress"]["locality"], town, "{google}");
            assert_eq!(
                result["formattedAddress"],
                format!("Heinestraße 42, 1020 {town}, {country}"),
                "{google}"
            );
            let here = get(format!("/compat/here/v1/geocode?{q}&lang={lang}")).await;
            let address = &here["items"][0]["address"];
            assert_eq!(address["city"], town, "{here}");
            assert_eq!(address["countryName"], country, "{here}");
            assert_eq!(
                address["label"],
                format!("Heinestraße 42, 1020 {town}, {country}"),
                "{here}"
            );
            let mapbox = get(format!(
                "/compat/mapbox/search/geocode/v6/forward?{q}&language={lang}"
            ))
            .await;
            let properties = &mapbox["features"][0]["properties"];
            assert_eq!(
                properties["context"]["country"]["name"], country,
                "{mapbox}"
            );
            assert!(
                properties["full_address"]
                    .as_str()
                    .is_some_and(|text| text.contains(town) && !text.contains("Vienna")),
                "{mapbox}"
            );
            let precisely = call(
                axum::http::Request::post("/compat/precisely/v1/geocode")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "preferences": {"maxResults": 1, "clientLocale": lang},
                            "addresses": [{"addressLines": ["42 Heinestraße"], "country": "AT"}],
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await;
            let address = &precisely["responses"][0]["results"][0]["address"];
            assert_eq!(address["city"]["longName"], town, "{precisely}");
            assert_eq!(address["country"]["name"], country, "{precisely}");
        }
        let _ = std::fs::remove_dir_all(&pack);
    }

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
