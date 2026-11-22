//! Search Box v1: `suggest`, `retrieve`, `forward` and `reverse`. Category
//! search is in [`super::category`].

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
};
use serde::Serialize;

use super::{
    Api, Context, Coordinates, Found, Geometry, LayerFields, Params, Translated, Types, admitted,
    attribution, category, error,
};
use crate::{
    api::{Common, ForwardCollection, ReverseCollection},
    categories,
    compat::{Args, VendorError, decode_id, native},
    record::Contact,
    runtime::AppState,
};

/// A Search Box context layer, keyed `id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Layer {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(flatten)]
    fields: LayerFields,
}

/// What a suggestion and a feature's properties share.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct Described {
    name: String,
    mapbox_id: String,
    feature_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    full_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    place_formatted: Option<String>,
    context: Context<Layer>,
}

/// The POI fields og has, after `context` and `coordinates`.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct Poi {
    #[serde(skip_serializing_if = "Option::is_none")]
    maki: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    brand: Option<Vec<String>>,
    /// The names of the listed kinds the place is, closest first.
    #[serde(skip_serializing_if = "Option::is_none")]
    poi_category: Option<Vec<String>>,
    /// The canonical ids of those kinds, as `list/category` gives them.
    #[serde(skip_serializing_if = "Option::is_none")]
    poi_category_ids: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Metadata::is_empty")]
    metadata: Metadata,
}

/// What og knows of how to reach a place and when it is open.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
struct Metadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    phone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    website: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    open_hours: Option<super::hours::OpenHours>,
}

impl Metadata {
    fn of(common: &Common) -> Self {
        let contact = common
            .contact
            .as_ref()
            .map_or_else(Contact::default, Clone::clone);
        Self {
            phone: contact.phone,
            website: contact.website,
            email: contact.email,
            open_hours: common
                .opening_hours
                .as_deref()
                .and_then(super::hours::OpenHours::parse),
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// What a feature has besides what a suggestion has.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct Placed {
    coordinates: Coordinates,
    #[serde(skip_serializing_if = "Option::is_none")]
    bbox: Option<[f64; 4]>,
    /// Metres from `proximity`, on a forward search only.
    #[serde(skip_serializing_if = "Option::is_none")]
    distance: Option<f64>,
}

fn described(found: Found) -> (Described, Placed, Poi) {
    let common = found.common;
    let translated = Translated::of(Api::SearchBox, found);
    let kinds = common.category.as_deref().and_then(category::kinds);
    // The context keeps the unit og states only in Geocoding, whose
    // `secondary_address` layer Search Box has no counterpart of.
    let context = Context {
        secondary_address: None,
        ..translated.context
    };
    (
        Described {
            name: translated.name,
            mapbox_id: translated.mapbox_id,
            feature_type: translated.feature_type,
            address: translated.address,
            full_address: translated.full_address,
            place_formatted: translated.place_formatted,
            context: context.map(|area| {
                let (id, fields) = area.split();
                Layer { id, fields }
            }),
        },
        Placed {
            coordinates: translated.coordinates,
            bbox: translated.bbox,
            distance: found.distance_m,
        },
        Poi {
            maki: translated.maki,
            brand: translated.brand.map(|brand| vec![brand]),
            poi_category: kinds.clone().map(|(names, _)| names),
            poi_category_ids: kinds.map(|(_, ids)| ids),
            metadata: Metadata::of(common),
        },
    )
}

/// `/suggest`'s answer. Suggestions carry no coordinates. `/retrieve` has
/// them.
#[derive(Debug, Serialize)]
pub(super) struct Suggestions {
    suggestions: Vec<Suggestion>,
    attribution: String,
}

#[derive(Debug, Serialize)]
struct Suggestion {
    #[serde(flatten)]
    described: Described,
    /// The language the suggestion is named in, which every suggestion
    /// states.
    language: String,
    /// Metres from `proximity`.
    #[serde(skip_serializing_if = "Option::is_none")]
    distance: Option<f64>,
    #[serde(flatten)]
    poi: Poi,
}

/// The FeatureCollection `/retrieve`, `/forward` and `/reverse` answer.
#[derive(Debug, Serialize)]
pub(super) struct Collection {
    #[serde(rename = "type")]
    kind: &'static str,
    features: Vec<Feature>,
    attribution: String,
}

#[derive(Debug, Serialize)]
struct Feature {
    #[serde(rename = "type")]
    kind: &'static str,
    geometry: Geometry,
    properties: Properties,
}

#[derive(Debug, Serialize)]
struct Properties {
    #[serde(flatten)]
    described: Described,
    #[serde(flatten)]
    placed: Placed,
    #[serde(flatten)]
    poi: Poi,
}

impl Collection {
    fn of<'a>(
        features: impl Iterator<Item = Found<'a>>,
        types: Option<&Types>,
        credits: &[crate::api::Attribution],
    ) -> Self {
        Self {
            kind: "FeatureCollection",
            features: features
                .filter(|found| admitted(types, found.common))
                .map(|found| {
                    let (described, placed, poi) = described(found);
                    Feature {
                        kind: "Feature",
                        geometry: Geometry::point(found.coordinates),
                        properties: Properties {
                            described,
                            placed,
                            poi,
                        },
                    }
                })
                .collect(),
            attribution: attribution(credits),
        }
    }

    pub(super) fn forward(collection: &ForwardCollection, types: Option<&Types>) -> Self {
        Self::of(
            collection.features.iter().map(Found::forward),
            types,
            &collection.geocoding.attribution,
        )
    }

    fn reverse(collection: &ReverseCollection, types: Option<&Types>) -> Self {
        Self::of(
            collection.features.iter().map(Found::reverse),
            types,
            &collection.geocoding.attribution,
        )
    }
}

/// The one language Search Box names results in. It is the first of
/// `language`, or English when none is sent.
pub(super) fn language(params: &Params) -> &str {
    params
        .get("language")
        .and_then(|value| {
            value
                .split(',')
                .map(str::trim)
                .find(|code| !code.is_empty())
        })
        .unwrap_or("en")
}

/// Search Box's largest `limit` on `suggest` and `forward`.
const MAX_LIMIT: usize = 10;

/// `limit`, checked against Search Box's largest.
fn limit(params: &Params) -> Result<Option<usize>, VendorError> {
    params
        .get("limit")
        .map(|value| {
            value
                .parse::<usize>()
                .ok()
                .filter(|limit| (1..=MAX_LIMIT).contains(limit))
                .ok_or_else(|| {
                    Api::SearchBox
                        .invalid(format!("limit must be a number between 1 and {MAX_LIMIT}"))
                })
        })
        .transpose()
}

/// What a text search asks of og beyond its text, checked.
struct Asked {
    limit: Option<usize>,
    /// og's categories a POI must be of, `None` for any.
    categories: Option<Vec<&'static str>>,
    excluded: Vec<&'static str>,
}

/// Parameters that pick which places come back by what og does not hold:
/// a route, a travel time, a place resolved from free text, a radius around
/// it. Sent, they are refused, since answering without them would answer
/// another question.
const UNHONOURED: [&str; 10] = [
    "near",
    "origin",
    "route",
    "route_geometry",
    "sar_type",
    "time_deviation",
    "eta_type",
    "navigation_profile",
    "radius",
    "rank_strategy",
];

impl Asked {
    fn of(params: &Params) -> Result<Self, VendorError> {
        if let Some(name) = UNHONOURED
            .into_iter()
            .find(|name| params.get(name).is_some())
        {
            return Err(Api::SearchBox.invalid(format!("{name} is not supported by this server")));
        }
        let limit = limit(params)?;
        let categories = |name: &str| {
            params
                .get(name)
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(|id| {
                            category::og_categories(id).ok_or_else(|| {
                                Api::SearchBox.invalid(format!("Unknown {name} \"{id}\""))
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()
                        .map(|lists| lists.concat())
                })
                .transpose()
        };
        Ok(Self {
            limit,
            categories: categories("poi_category")?,
            excluded: categories("poi_category_exclusions")?.unwrap_or_default(),
        })
    }

    /// Keeps the results the categories admit: with `poi_category` only POIs
    /// of those kinds, and never a POI of an excluded kind.
    fn keep(&self, collection: &mut ForwardCollection) {
        collection.features.retain(|feature| {
            let category = feature.properties.common.category.as_deref();
            let held = |kinds: &[&str]| {
                category.is_some_and(|category| categories::holds(kinds, category))
            };
            self.categories.as_deref().is_none_or(held) && !held(&self.excluded)
        });
        if let Some(limit) = self.limit {
            collection.features.truncate(limit);
        }
    }

    /// How many og is asked for: more than answered when categories filter
    /// what comes back.
    fn fetch(&self) -> Option<String> {
        match self.categories.is_some() || !self.excluded.is_empty() {
            true => Some(MAX_LIMIT.to_string()),
            false => self.limit.map(|limit| limit.to_string()),
        }
    }
}

fn required_q(params: &Params) -> Result<&str, VendorError> {
    params
        .get("q")
        .ok_or_else(|| Api::SearchBox.invalid("q is required"))
}

fn failed(problem: crate::http::problem::Problem) -> VendorError {
    Api::SearchBox.failed(problem)
}

/// Suggest places as typed (Mapbox Search Box v1)
pub(super) async fn suggest(
    State(state): State<AppState>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Json<Suggestions>, VendorError> {
    let params = Params::from_query(Api::SearchBox, query)?;
    let q = required_q(&params)?;
    let types = params.types(Api::SearchBox)?;
    let asked = Asked::of(&params)?;
    let args = Args::new()
        .set("q", q)
        .maybe("limit", asked.fetch())
        .maybe("country", params.get("country"))
        .maybe("focus", params.focus(Api::SearchBox)?)
        .maybe("bbox", params.bbox(Api::SearchBox)?)
        .set("lang", language(&params));
    let mut collection = super::complete(&state, args, types.as_ref())
        .await
        .map_err(failed)?;
    asked.keep(&mut collection);
    let suggestions = collection
        .features
        .iter()
        .filter(|feature| admitted(types.as_ref(), &feature.properties.common))
        .map(|feature| {
            let (mut described, placed, poi) = described(Found::forward(feature));
            // Every suggestion states where it lies, empty for a place og
            // finds in no area.
            described.place_formatted.get_or_insert_default();
            Suggestion {
                described,
                language: language(&params).to_string(),
                distance: placed.distance,
                poi,
            }
        })
        .collect();
    Ok(Json(Suggestions {
        suggestions,
        attribution: attribution(&collection.geocoding.attribution),
    }))
}

/// Retrieve a suggestion (Mapbox Search Box v1)
pub(super) async fn retrieve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Json<Collection>, VendorError> {
    let params = Params::from_query(Api::SearchBox, query)?;
    let not_found = || error(StatusCode::NOT_FOUND, "Not Found");
    let named = decode_id(&id).ok_or_else(not_found)?;
    let lang = language(&params);
    let collection = native::lookup(&state, &named, Some(lang))
        .await
        .map_err(failed)?
        .ok_or_else(not_found)?;
    if collection.features.is_empty() {
        return Err(not_found());
    }
    Ok(Json(Collection::forward(&collection, None)))
}

/// Search by text (Mapbox Search Box v1)
///
/// Each Mapbox switch picks the og search of the same meaning.
/// `auto_complete=true` is og's `/autocomplete`, `types=poi` alone is
/// `/places`, another single type is `/geocode` of that layer, and no
/// type searches addresses and places together, with the town, region or
/// country the whole text names first.
pub(super) async fn forward(
    State(state): State<AppState>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Json<Collection>, VendorError> {
    let params = Params::from_query(Api::SearchBox, query)?;
    let q = required_q(&params)?;
    let types = params.types(Api::SearchBox)?;
    let asked = Asked::of(&params)?;
    let layer = types.as_ref().and_then(Types::og_layer);
    let args = Args::new()
        .set("q", q)
        .maybe("limit", asked.fetch())
        .maybe("country", params.get("country"))
        .maybe("focus", params.focus(Api::SearchBox)?)
        .maybe("bbox", params.bbox(Api::SearchBox)?)
        .set("lang", language(&params));
    let autocomplete = params
        .get("auto_complete")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    let mut collection = if autocomplete {
        super::complete(&state, args, types.as_ref()).await
    } else {
        match layer {
            Some("poi") => native::places(&state, args).await,
            Some(_) => super::forward_geocode(&state, args, types.as_ref()).await,
            // A town the text names comes first, as og's search answers it.
            None => native::search(&state, args).await,
        }
    }
    .map_err(failed)?;
    asked.keep(&mut collection);
    Ok(Json(Collection::forward(&collection, types.as_ref())))
}

/// Reverse geocode a point (Mapbox Search Box v1)
pub(super) async fn reverse(
    State(state): State<AppState>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Json<Collection>, VendorError> {
    let params = Params::from_query(Api::SearchBox, query)?;
    let types = params.types(Api::SearchBox)?;
    let countries = super::countries(Api::SearchBox, &params)?;
    let (lon, lat) = params.point(Api::SearchBox)?;
    let limit = limit(&params)?;
    let layer = types.as_ref().and_then(Types::reverse_layer);
    let args = Args::new()
        .set("lon", lon)
        .set("lat", lat)
        .maybe("layer", layer)
        // og answers several of one layer only when asked for that layer.
        .maybe(
            "limit",
            limit
                .filter(|_| layer.is_some())
                .map(|limit| limit.to_string()),
        )
        .set(
            "radius",
            crate::compat::radius(crate::compat::NEAREST_HOUSE_RADIUS_M),
        )
        .set("lang", language(&params));
    let mut collection = native::reverse(&state, args).await.map_err(failed)?;
    super::keep_countries(&mut collection, &countries);
    if let Some(limit) = limit {
        collection.features.truncate(limit);
    }
    Ok(Json(Collection::reverse(&collection, types.as_ref())))
}
