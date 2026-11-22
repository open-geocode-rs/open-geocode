//! Search Destinations of Geocoding API v4.
//!
//! og knows a destination's place, the entrances OpenStreetMap maps for it,
//! the places whose outline holds it and the best known places near it.
//! Navigation points, outlines and the places a destination holds are left
//! out, as og has none of them.

use axum::{
    body::Bytes,
    extract::{RawQuery, State},
    http::HeaderMap,
    response::Response,
};
use serde::{Deserialize, Serialize};

use super::{
    LatLng, QueryParams, body, field_mask, from_problem,
    geocode::AddressQuery,
    invalid,
    language::Language,
    model::{Entrance, LocalizedText, PlaceView},
    places::by_place_id,
    respond, run,
};
use crate::{
    api::Common,
    compat::{Args, VendorError, fieldmask::FieldMask, native},
    record::Layer,
    runtime::AppState,
};

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SearchDestinationsRequest {
    place: Option<String>,
    #[serde(alias = "address_query")]
    address_query: Option<AddressQueryBody>,
    #[serde(alias = "location_query")]
    location_query: Option<LocationQueryBody>,
    #[serde(alias = "language_code")]
    language_code: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct AddressQueryBody {
    #[serde(alias = "address_query")]
    address_query: Option<String>,
    address: Option<PostalAddressBody>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PostalAddressBody {
    #[serde(alias = "address_lines")]
    address_lines: Vec<String>,
    locality: Option<String>,
    #[serde(alias = "administrative_area")]
    administrative_area: Option<String>,
    #[serde(alias = "postal_code")]
    postal_code: Option<String>,
    #[serde(alias = "region_code")]
    region_code: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LocationQueryBody {
    location: Option<LatLng>,
}

#[derive(Debug, Serialize)]
struct SearchDestinationsResponse {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    destinations: Vec<Destination>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Destination {
    primary: PlaceView,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    landmarks: Vec<Landmark>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    entrances: Vec<Entrance>,
    /// og's id of the primary place and where it is, to find its
    /// surroundings.
    #[serde(skip)]
    og_id: String,
    #[serde(skip)]
    point: [f64; 2],
}

/// A place near a destination that tells where it is.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Landmark {
    place: PlaceView,
    /// `Near ...`, written in English only, the one language og words it in.
    #[serde(skip_serializing_if = "Option::is_none")]
    relational_description: Option<LocalizedText>,
    /// A landmark og finds tells where the address is. og cannot tell
    /// where one arrives, so it never tags `ARRIVAL`.
    tags: [&'static str; 1],
    straight_line_distance_meters: f64,
}

/// How far around a destination its surroundings are looked for, a short
/// walk, and how many landmarks tell where it is.
const SURROUNDINGS_RADIUS_M: f64 = 300.0;
const MAX_LANDMARKS: usize = 5;

impl Destination {
    fn of(common: &Common, coordinates: [f64; 2], language: &Language) -> Self {
        let primary = PlaceView::of(common, coordinates, language);
        let entrances = common
            .access
            .iter()
            .map(|access| Entrance::of(access, &primary.place))
            .collect();
        Self {
            primary,
            landmarks: Vec::new(),
            entrances,
            og_id: common.id.clone(),
            point: coordinates,
        }
    }

    /// A bounding box alone cannot establish that a place contains the destination.
    async fn with_surroundings(
        mut self,
        state: &AppState,
        language: &Language<'_>,
    ) -> Result<Self, VendorError> {
        let around = native::landmarks(
            state,
            self.point,
            SURROUNDINGS_RADIUS_M,
            crate::http::bounds::MAX_SEARCH_LIMIT,
            language.code().map(str::to_string),
            &self.og_id,
        )
        .await
        .map_err(from_problem)?;
        let [lon, lat] = self.point;
        let english = language
            .code()
            .is_none_or(|code| code == "en" || code.starts_with("en-"));
        for feature in around.iter().take(MAX_LANDMARKS) {
            let common = &feature.properties.common;
            let view = PlaceView::of(common, feature.geometry.coordinates, language);
            let [x, y] = feature.geometry.coordinates;
            self.landmarks.push(Landmark {
                relational_description: english.then(|| LocalizedText {
                    text: format!("Near {}", common.name),
                    language_code: Some("en".to_string()),
                }),
                place: view,
                tags: ["ADDRESS"],
                straight_line_distance_meters: crate::spatial_index::haversine_m(lon, lat, x, y),
            });
        }
        Ok(self)
    }
}

/// Only a place one travels to is a destination. Areas are not, nor is a
/// street or a number guessed along a range, as Google refuses those too.
fn is_destination(common: &Common) -> bool {
    matches!(common.layer, Layer::Address | Layer::Poi)
}

/// `POST /v4/geocode/destinations`.
pub(super) async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    bytes: Bytes,
) -> Response {
    run(async {
        let query = QueryParams::parse(query.as_deref())?;
        let mask = field_mask(&headers, &query);
        let request: SearchDestinationsRequest = body(&bytes)?;
        // Geocoding's default is the local language, og's own default too.
        let language = Language::parse(state.pack(), request.language_code.as_deref())?;
        let destinations = match (request.place, request.address_query, request.location_query) {
            (Some(place), None, None) => by_place(&state, &place, &language).await?,
            (None, Some(address), None) => by_address(&state, address, &language).await?,
            (None, None, Some(location)) => by_location(&state, location, &language).await?,
            (None, None, None) => {
                return Err(invalid(
                    "One of place, addressQuery or locationQuery is required",
                ));
            }
            _ => {
                return Err(invalid(
                    "Give only one of place, addressQuery or locationQuery",
                ));
            }
        };
        // The surroundings are a lookup of their own, so a mask that leaves
        // them out costs none.
        let surroundings = FieldMask::asks(mask.as_ref(), &["destinations", "landmarks"]);
        let mut described = Vec::new();
        for destination in destinations {
            described.push(match surroundings {
                true => destination.with_surroundings(&state, &language).await?,
                false => destination,
            });
        }
        Ok(respond(
            &SearchDestinationsResponse {
                destinations: described,
            },
            mask.as_ref(),
        ))
    })
    .await
}

async fn by_place(
    state: &AppState,
    place: &str,
    language: &Language<'_>,
) -> Result<Vec<Destination>, VendorError> {
    let place_id = place
        .strip_prefix("places/")
        .filter(|id| !id.is_empty() && !id.contains('/'))
        .ok_or_else(|| invalid(format!("Invalid place {place:?}: expected places/PLACE_ID")))?;
    let feature = by_place_id(state, place_id, language).await?;
    let common = &feature.properties.common;
    Ok(is_destination(common)
        .then(|| Destination::of(common, feature.geometry.coordinates, language))
        .into_iter()
        .collect())
}

/// Each address og finds is a destination of its own, which is how Google
/// offers a choice when an address is ambiguous.
async fn by_address(
    state: &AppState,
    address: AddressQueryBody,
    language: &Language<'_>,
) -> Result<Vec<Destination>, VendorError> {
    let parts = address.address.unwrap_or_default();
    let query = AddressQuery {
        text: address.address_query,
        lines: parts
            .address_lines
            .iter()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .collect(),
        locality: blank_as_none(parts.locality.as_deref()),
        region: blank_as_none(parts.administrative_area.as_deref()),
        postcode: blank_as_none(parts.postal_code.as_deref()),
        country: blank_as_none(parts.region_code.as_deref()),
    };
    let args = query.args(|| Ok(None))?.maybe("lang", language.code());
    let found = native::geocode(state, args).await.map_err(from_problem)?;
    Ok(found
        .features
        .iter()
        .map(|feature| (&feature.properties.common, feature.geometry.coordinates))
        .filter(|(common, _)| is_destination(common))
        .map(|(common, coordinates)| Destination::of(common, coordinates, language))
        .collect())
}

/// The address or place nearest the point is the destination there.
async fn by_location(
    state: &AppState,
    location: LocationQueryBody,
    language: &Language<'_>,
) -> Result<Vec<Destination>, VendorError> {
    let [lon, lat] = location
        .location
        .ok_or_else(|| invalid("locationQuery.location is required"))?
        .checked("locationQuery.location")?;
    let around = |layer: &'static str| {
        native::reverse(
            state,
            Args::new()
                .set("lon", lon.to_string())
                .set("lat", lat.to_string())
                .set("layer", layer)
                .set(
                    "radius",
                    crate::compat::radius(crate::compat::NEAREST_HOUSE_RADIUS_M),
                )
                .maybe("lang", language.code()),
        )
    };
    let (addresses, places) = tokio::join!(around("address"), around("poi"));
    let (addresses, places) = (
        addresses.map_err(from_problem)?,
        places.map_err(from_problem)?,
    );
    let nearest = addresses
        .features
        .iter()
        .chain(&places.features)
        .filter(|feature| is_destination(&feature.properties.common))
        .min_by(|a, b| {
            let distance = |feature: &&crate::api::ReverseFeature| {
                feature.properties.distance_m.unwrap_or(f64::INFINITY)
            };
            distance(a).total_cmp(&distance(b))
        });
    Ok(nearest
        .map(|feature| {
            Destination::of(
                &feature.properties.common,
                feature.geometry.coordinates,
                language,
            )
        })
        .into_iter()
        .collect())
}

fn blank_as_none(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|text| !text.is_empty())
}
