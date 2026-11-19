//! Search Box v1: `suggest`, `retrieve`, `forward` and `reverse`. Category
//! search is in [`super::category`].

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
};
use serde::Serialize;

use super::{
    Api, Context, Coordinates, Geometry, LayerFields, Params, Translated, Types, admitted,
    attribution, category, error,
};
use crate::{
    api::{Common, ForwardCollection, ReverseCollection},
    compat::{Args, VendorError, decode_id, native},
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
    poi_category: Option<Vec<&'static str>>,
    /// The canonical ids of those kinds, as `list/category` gives them.
    #[serde(skip_serializing_if = "Option::is_none")]
    poi_category_ids: Option<Vec<&'static str>>,
}

fn described(common: &Common, coordinates: [f64; 2]) -> (Described, Coordinates, Poi) {
    let translated = Translated::of(common, coordinates);
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
        translated.coordinates,
        Poi {
            maki: translated.maki,
            brand: translated.brand.map(|brand| vec![brand]),
            poi_category: kinds.clone().map(|(names, _)| names),
            poi_category_ids: kinds.map(|(_, ids)| ids),
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
    coordinates: Coordinates,
    #[serde(flatten)]
    poi: Poi,
}

impl Collection {
    fn of<'a>(
        features: impl Iterator<Item = (&'a Common, [f64; 2])>,
        types: Option<&Types>,
        credits: &[crate::api::Attribution],
    ) -> Self {
        Self {
            kind: "FeatureCollection",
            features: features
                .filter(|(common, _)| admitted(types, common))
                .map(|(common, coordinates)| {
                    let (described, coordinates_property, poi) = described(common, coordinates);
                    Feature {
                        kind: "Feature",
                        geometry: Geometry::point(coordinates),
                        properties: Properties {
                            described,
                            coordinates: coordinates_property,
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
            collection
                .features
                .iter()
                .map(|feature| (&feature.properties.common, feature.geometry.coordinates)),
            types,
            &collection.geocoding.attribution,
        )
    }

    fn reverse(collection: &ReverseCollection, types: Option<&Types>) -> Self {
        Self::of(
            collection
                .features
                .iter()
                .map(|feature| (&feature.properties.common, feature.geometry.coordinates)),
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
    let args = Args::new()
        .set("q", q)
        .maybe("limit", params.get("limit"))
        .maybe("country", params.get("country"))
        .maybe("focus", params.focus())
        .maybe("bbox", params.get("bbox"))
        .maybe("layer", types.as_ref().and_then(Types::og_layer))
        .set("lang", language(&params));
    let collection = native::autocomplete(&state, args).await.map_err(failed)?;
    let suggestions = collection
        .features
        .iter()
        .filter(|feature| admitted(types.as_ref(), &feature.properties.common))
        .map(|feature| {
            let (described, _, poi) =
                described(&feature.properties.common, feature.geometry.coordinates);
            Suggestion { described, poi }
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
    let og_id = decode_id(&id).ok_or_else(not_found)?;
    let args = Args::new().set("ids", og_id).set("lang", language(&params));
    let collection = match native::places(&state, args).await {
        Ok(collection) => collection,
        // An id `/places` cannot read is one it never issued for lookup.
        Err(problem) if problem.status() == 400 => return Err(not_found().caused_by(problem)),
        Err(problem) => return Err(failed(problem)),
    };
    if collection.features.is_empty() {
        return Err(not_found());
    }
    Ok(Json(Collection::forward(&collection, None)))
}

/// Search by text (Mapbox Search Box v1)
///
/// og has no one search over places and addresses but its autocomplete, so
/// each Mapbox switch picks the og search of the same meaning.
/// `auto_complete=true` is og's `/autocomplete`, `types=poi` alone is
/// `/places`, and anything else is `/geocode`.
pub(super) async fn forward(
    State(state): State<AppState>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Json<Collection>, VendorError> {
    let params = Params::from_query(Api::SearchBox, query)?;
    let q = required_q(&params)?;
    let types = params.types(Api::SearchBox)?;
    let layer = types.as_ref().and_then(Types::og_layer);
    let args = Args::new()
        .set("q", q)
        .maybe("limit", params.get("limit"))
        .maybe("country", params.get("country"))
        .maybe("focus", params.focus())
        .maybe("bbox", params.get("bbox"))
        .set("lang", language(&params));
    let autocomplete = params
        .get("auto_complete")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    let collection = if autocomplete {
        native::autocomplete(&state, args.maybe("layer", layer)).await
    } else if layer == Some("poi") {
        native::places(&state, args).await
    } else {
        native::geocode(&state, args.maybe("layer", layer)).await
    }
    .map_err(failed)?;
    Ok(Json(Collection::forward(&collection, types.as_ref())))
}

/// Reverse geocode a point (Mapbox Search Box v1)
pub(super) async fn reverse(
    State(state): State<AppState>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Json<Collection>, VendorError> {
    let params = Params::from_query(Api::SearchBox, query)?;
    let types = params.types(Api::SearchBox)?;
    let args = Args::new()
        .maybe("lon", params.get("longitude"))
        .maybe("lat", params.get("latitude"))
        .maybe("layer", types.as_ref().and_then(Types::og_layer))
        .set("lang", language(&params));
    let collection = native::reverse(&state, args).await.map_err(failed)?;
    Ok(Json(Collection::reverse(&collection, types.as_ref())))
}
