//! `/v1/proximity-search`, the address nearest a point.
//!
//! og's reverse geocode finds one nearest address, so at most one result
//! comes back whatever `maxResults` asks.

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{Request, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use super::{
    Failure, country_code, error, given, read_json,
    realtime::internal,
    result::{self, Product},
};
use crate::compat::{Args, VendorError, native};
use crate::runtime::AppState;

#[derive(Debug, Deserialize)]
struct ProximityRequest {
    preferences: Option<ProximityPreferences>,
    location: Option<ProximityLocation>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProximityPreferences {
    dataset: Option<String>,
    max_results: Option<i64>,
    distance: Option<Distance>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Distance {
    value: Option<f64>,
    distance_unit: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProximityLocation {
    address_id: Option<String>,
    longitude: Option<f64>,
    latitude: Option<f64>,
    country: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct ProximityResponse {
    response: ResponseBody,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResponseBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    address_id: Option<String>,
    status: &'static str,
    results: Vec<ProximityResult>,
}

#[derive(Debug, Serialize)]
struct ProximityResult {
    distance: f64,
    location: Point,
    metadata: BTreeMap<&'static str, String>,
}

#[derive(Debug, Serialize)]
struct Point {
    latitude: f64,
    longitude: f64,
}

/// Metres in one of Precisely's distance units.
fn metres_per(unit: &str) -> Option<f64> {
    Some(match unit.to_ascii_uppercase().as_str() {
        "METER" => 1.0,
        "KILOMETER" => 1000.0,
        "FOOT" => 0.3048,
        "MILE" => 1609.344,
        _ => return None,
    })
}

/// The unit a distance comes back in when the request names none. The
/// documented answer measures kilometres.
const DEFAULT_UNIT: &str = "KILOMETER";

pub(super) async fn proximity_search(
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<ProximityResponse>, VendorError> {
    let body = read_json::<ProximityRequest>(&state, request, "INVALID_REQUEST").await?;
    let invalid = |message: &str| error(StatusCode::BAD_REQUEST, "INVALID_REQUEST", message);
    let not_valid = || invalid("Request Body is not valid (DIS-LI-1018)");
    let (Some(preferences), Some(location)) = (body.preferences, body.location) else {
        return Err(not_valid());
    };
    if given(preferences.dataset.as_deref()).is_none() {
        return Err(invalid(
            "Dataset is not provided in the request (DIS-LI-1031)",
        ));
    }
    super::Preferences {
        max_results: preferences.max_results,
    }
    .max_results()?;
    let country = given(location.country.as_deref())
        .ok_or_else(|| invalid("Country is not provided in the request (DIS-LI-1015)"))?;
    let (Some(lon), Some(lat)) = (location.longitude, location.latitude) else {
        return Err(not_valid());
    };
    let unit = preferences
        .distance
        .as_ref()
        .and_then(|distance| given(distance.distance_unit.as_deref()))
        .unwrap_or(DEFAULT_UNIT);
    let metres = metres_per(unit).ok_or_else(not_valid)?;
    let radius = preferences
        .distance
        .and_then(|distance| distance.value)
        .map(|value| value * metres);

    let args = Args::new()
        .set("lon", lon.to_string())
        .set("lat", lat.to_string())
        .set("layer", "address");
    let collection = native::reverse(&state, args)
        .await
        .map_err(|problem| Failure::of(&problem, Some(country)).into_error(problem))?;
    let features = result::features(&collection.features).map_err(internal)?;
    let wanted = country_code(country);
    let results = features
        .iter()
        .filter_map(|feature| {
            let candidate = feature.candidate(Product::Geocode);
            let address = &candidate.address;
            let alpha2 = address
                .country
                .as_ref()
                .and_then(|country| country.iso_alpha2_code.as_deref());
            // An address over the border is not in the country asked for.
            if alpha2.is_some_and(|alpha2| !alpha2.eq_ignore_ascii_case(&wanted)) {
                return None;
            }
            let distance_m = feature.distance_m()?;
            if radius.is_some_and(|radius| distance_m > radius) {
                return None;
            }
            let name = |field: &Option<result::AdminField>| {
                field.as_ref().map(|field| field.long_name.clone())
            };
            let metadata = [
                (
                    "PBKEY",
                    candidate.custom_fields.get(result::ID_FIELD).cloned(),
                ),
                ("FORMATTEDADDRESS", address.formatted_address.clone()),
                ("ADDRLINE1", address.formatted_street_address.clone()),
                ("ADDNUMBER", address.address_number.clone()),
                ("STREETNAME", address.street.clone()),
                ("CITY", name(&address.city)),
                ("ADMIN2", name(&address.admin2)),
                (
                    "ADMIN1",
                    address.admin1.as_ref().map(|admin1| {
                        admin1
                            .short_name
                            .clone()
                            .unwrap_or_else(|| admin1.long_name.clone())
                    }),
                ),
                ("POSTALCODE", address.postal_code.clone()),
                ("POSTALCODEEXT", address.postal_code_ext.clone()),
                (
                    "COUNTRY",
                    address
                        .country
                        .as_ref()
                        .and_then(|country| country.iso_alpha3_code)
                        .map(str::to_string),
                ),
            ]
            .into_iter()
            .filter_map(|(key, value)| Some((key, value?)))
            .collect();
            let [longitude, latitude] = feature.coordinates();
            Some(ProximityResult {
                distance: distance_m / metres,
                location: Point {
                    latitude,
                    longitude,
                },
                metadata,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(ProximityResponse {
        response: ResponseBody {
            address_id: location.address_id,
            status: if results.is_empty() {
                "ZERO_RESULTS"
            } else {
                "OK"
            },
            results,
        },
    }))
}
