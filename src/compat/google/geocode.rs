//! Address, location and place geocoding of Geocoding API v4.

use axum::{
    extract::{Path, RawPathParams, RawQuery, State},
    http::HeaderMap,
    response::Response,
};
use serde::Serialize;

use super::{
    LatLng, QueryParams, Viewport, field_mask, from_problem, invalid,
    model::{GeocodeResult, granularity},
    places::by_place_id,
    respond, run,
};
use crate::{
    compat::{Args, VendorError, native},
    runtime::AppState,
};

#[derive(Debug, Serialize)]
struct GeocodeResponse {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    results: Vec<GeocodeResult>,
}

/// `GET /v4/geocode/address/{addressQuery}`.
pub(super) async fn address_in_path(
    State(state): State<AppState>,
    params: RawPathParams,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        let query = QueryParams::parse(query.as_deref())?;
        let raw = params
            .iter()
            .find(|(name, _)| *name == "address_query")
            .map(|(_, value)| value)
            .unwrap_or_default();
        geocode_address(&state, &headers, &query, Some(path_text(raw)?)).await
    })
    .await
}

/// A path segment as Google reads it. `+` is a space and a literal plus is
/// sent as `%2B`, so `+` must become a space before the percent-decoding.
fn path_text(raw: &str) -> Result<String, VendorError> {
    serde_urlencoded::from_str::<Vec<(String, String)>>(&format!("q={raw}"))
        .ok()
        .and_then(|pairs| pairs.into_iter().next())
        .map(|(_, value)| value)
        .ok_or_else(|| invalid(format!("Invalid addressQuery: {raw:?}")))
}

/// `GET /v4/geocode/address?addressQuery=` or `?address.*=`.
pub(super) async fn address(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        let query = QueryParams::parse(query.as_deref())?;
        let text = query.get("addressQuery").map(str::to_string);
        geocode_address(&state, &headers, &query, text).await
    })
    .await
}

async fn geocode_address(
    state: &AppState,
    headers: &HeaderMap,
    query: &QueryParams,
    text: Option<String>,
) -> Result<Response, VendorError> {
    let mask = field_mask(headers, query);
    let lines = query.all("address.addressLines");
    let locality = query.get("address.locality");
    let region = query.get("address.administrativeArea");
    let postcode = query.get("address.postalCode");
    let country = query.get("address.regionCode");
    let structured =
        !lines.is_empty() || locality.is_some() || region.is_some() || postcode.is_some();
    let args = match (text.filter(|text| !text.trim().is_empty()), structured) {
        (Some(_), true) => {
            return Err(invalid(
                "Give the address as addressQuery or as address fields, not both",
            ));
        }
        (Some(text), false) => Args::new()
            .set("q", text)
            .maybe("focus", bias(query)?)
            .maybe("country", country),
        // og matches the street line on its own and checks the town, region
        // and postcode against it.
        (None, true) if !lines.is_empty() => Args::new()
            .set("address", lines.join(", "))
            .maybe("locality", locality)
            .maybe("region", region)
            .maybe("postcode", postcode)
            .maybe("country", country),
        // No street line. Google reads the parts as one address, and og
        // searches them as one text.
        (None, true) => Args::new()
            .set(
                "q",
                [locality, region, postcode]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(", "),
            )
            .maybe("focus", bias(query)?)
            .maybe("country", country),
        (None, false) => {
            return Err(invalid("addressQuery or address is required"));
        }
    };
    let found = native::geocode(state, args).await.map_err(from_problem)?;
    let results = found
        .features
        .iter()
        .map(|feature| GeocodeResult::of(&feature.properties.common, feature.geometry.coordinates))
        .collect();
    Ok(respond(&GeocodeResponse { results }, mask.as_ref()))
}

/// The middle of `locationBias.rectangle`, as og's `focus`.
fn bias(query: &QueryParams) -> Result<Option<String>, VendorError> {
    let corner = |prefix: &str| -> Result<Option<LatLng>, VendorError> {
        let latitude = query.double(&format!("{prefix}.latitude"))?;
        let longitude = query.double(&format!("{prefix}.longitude"))?;
        Ok((latitude.is_some() || longitude.is_some()).then(|| LatLng {
            latitude: latitude.unwrap_or_default(),
            longitude: longitude.unwrap_or_default(),
        }))
    };
    let low = corner("locationBias.rectangle.low")?;
    let high = corner("locationBias.rectangle.high")?;
    if low.is_none() && high.is_none() {
        return Ok(None);
    }
    let [lon, lat] = Viewport {
        low: low.unwrap_or_default(),
        high: high.unwrap_or_default(),
    }
    .center("locationBias.rectangle")?;
    Ok(Some(format!("{lon},{lat}")))
}

/// `GET /v4/geocode/location/{lat},{lng}`.
pub(super) async fn location_in_path(
    State(state): State<AppState>,
    Path(location): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        let query = QueryParams::parse(query.as_deref())?;
        let point = location_query(&location)?;
        reverse(&state, &headers, &query, point).await
    })
    .await
}

/// `GET /v4/geocode/location?locationQuery=` or `?location.latitude=`.
pub(super) async fn location(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        let query = QueryParams::parse(query.as_deref())?;
        let latitude = query.double("location.latitude")?;
        let longitude = query.double("location.longitude")?;
        let point = match (query.get("locationQuery"), latitude, longitude) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                return Err(invalid(
                    "Give the location as locationQuery or as location, not both",
                ));
            }
            (Some(text), None, None) => location_query(text)?,
            (None, None, None) => return Err(invalid("locationQuery or location is required")),
            (None, latitude, longitude) => LatLng {
                latitude: latitude.unwrap_or_default(),
                longitude: longitude.unwrap_or_default(),
            },
        };
        reverse(&state, &headers, &query, point).await
    })
    .await
}

/// `"lat,lng"`.
fn location_query(text: &str) -> Result<LatLng, VendorError> {
    let parse = |part: Option<&str>| part.and_then(|part| part.trim().parse::<f64>().ok());
    let mut parts = text.split(',');
    match (parse(parts.next()), parse(parts.next()), parts.next()) {
        (Some(latitude), Some(longitude), None) => Ok(LatLng {
            latitude,
            longitude,
        }),
        _ => Err(invalid(format!(
            "Invalid locationQuery {text:?}: expected \"latitude,longitude\""
        ))),
    }
}

async fn reverse(
    state: &AppState,
    headers: &HeaderMap,
    query: &QueryParams,
    point: LatLng,
) -> Result<Response, VendorError> {
    let mask = field_mask(headers, query);
    let [lon, lat] = point.checked("location")?;
    let found = native::reverse(
        state,
        Args::new()
            .set("lon", lon.to_string())
            .set("lat", lat.to_string()),
    )
    .await
    .map_err(from_problem)?;
    // Both are filters on the results, applied after the search as Google
    // applies them. A result is kept when it has any listed type and any
    // listed granularity.
    let types = query.all("types");
    let granularities = query.all("granularity");
    let results = found
        .features
        .iter()
        .filter(|feature| {
            granularities.is_empty()
                || granularities.contains(&granularity(feature.properties.common.accuracy))
        })
        .map(|feature| GeocodeResult::of(&feature.properties.common, feature.geometry.coordinates))
        .filter(|result| types.is_empty() || result.types.iter().any(|kind| types.contains(kind)))
        .collect();
    Ok(respond(&GeocodeResponse { results }, mask.as_ref()))
}

/// `GET /v4/geocode/places/{place}`, answered with one bare result and no
/// `results` list.
pub(super) async fn place(
    State(state): State<AppState>,
    Path(place_id): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        let query = QueryParams::parse(query.as_deref())?;
        let mask = field_mask(&headers, &query);
        let feature = by_place_id(&state, &place_id).await?;
        let result = GeocodeResult::of(&feature.properties.common, feature.geometry.coordinates);
        Ok(respond(&result, mask.as_ref()))
    })
    .await
}
