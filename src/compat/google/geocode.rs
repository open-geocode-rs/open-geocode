//! Address, location and place geocoding of Geocoding API v4.

use axum::{
    extract::{OriginalUri, Path, RawQuery, State},
    http::HeaderMap,
    response::Response,
};
use serde::Serialize;

use super::{
    LatLng, QueryParams, Viewport, field_mask, from_problem, invalid,
    language::Language,
    model::{GeocodeResult, PlusCode, granularity},
    places::by_place_id,
    respond, run,
};
use crate::{
    compat::{Args, VendorError, native},
    runtime::AppState,
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeocodeResponse {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    results: Vec<GeocodeResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plus_code: Option<PlusCode>,
}

/// `GET /v4/geocode/address/{addressQuery}`.
pub(super) async fn address_in_path(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        let query = QueryParams::parse(query.as_deref())?;
        let raw = uri.path().rsplit('/').next().unwrap_or_default();
        geocode_address(&state, &headers, &query, Some(path_text(raw)?)).await
    })
    .await
}

/// A path segment as Google reads it. `+` is a space and a literal plus is
/// sent as `%2B`, so `+` must become a space before the percent-decoding.
fn path_text(raw: &str) -> Result<String, VendorError> {
    let value = raw.replace('&', "%26");
    serde_urlencoded::from_str::<Vec<(String, String)>>(&format!("q={value}"))
        .ok()
        .and_then(|pairs| pairs.into_iter().next())
        .map(|(_, value)| value)
        .ok_or_else(|| invalid("Invalid addressQuery"))
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
    let address = AddressQuery {
        text,
        lines: query.all("address.addressLines"),
        locality: query.get("address.locality"),
        region: query.get("address.administrativeArea"),
        postcode: query.get("address.postalCode"),
        country: query.get("address.regionCode"),
    };
    let args = address.args(|| bias(query))?;
    let language = language(state, query)?;
    let args = args.maybe("lang", language.code());
    let found = native::geocode(state, args).await.map_err(from_problem)?;
    let results: Vec<_> = found
        .features
        .iter()
        .map(|feature| {
            GeocodeResult::of(
                &feature.properties.common,
                feature.geometry.coordinates,
                &language,
            )
        })
        .collect();
    Ok(respond(
        &GeocodeResponse {
            results,
            plus_code: None,
        },
        mask.as_ref(),
    ))
}

/// An address as Geocoding takes one, as one text or as postal parts.
pub(super) struct AddressQuery<'a> {
    pub(super) text: Option<String>,
    pub(super) lines: Vec<&'a str>,
    pub(super) locality: Option<&'a str>,
    pub(super) region: Option<&'a str>,
    pub(super) postcode: Option<&'a str>,
    pub(super) country: Option<&'a str>,
}

impl AddressQuery<'_> {
    /// og's geocode arguments. `focus` is read only for a search by text,
    /// the only one a bias applies to.
    pub(super) fn args(
        self,
        focus: impl FnOnce() -> Result<Option<String>, VendorError>,
    ) -> Result<Args, VendorError> {
        let Self {
            text,
            lines,
            locality,
            region,
            postcode,
            country,
        } = self;
        let structured =
            !lines.is_empty() || locality.is_some() || region.is_some() || postcode.is_some();
        Ok(
            match (text.filter(|text| !text.trim().is_empty()), structured) {
                (Some(_), true) => {
                    return Err(invalid(
                        "Give the address as addressQuery or as address fields, not both",
                    ));
                }
                (Some(text), false) => Args::new()
                    .set("q", text)
                    .maybe("focus", focus()?)
                    .maybe("country", country),
                // og matches the street line on its own and checks the town,
                // region and postcode against it.
                (None, true) if !lines.is_empty() => Args::new()
                    .set("address", lines.join(", "))
                    .maybe("locality", locality)
                    .maybe("region", region)
                    .maybe("postcode", postcode)
                    .maybe("country", country),
                // No street line. Google reads the parts as one address, and
                // og searches them as one text.
                (None, true) => Args::new()
                    .set(
                        "q",
                        [locality, region, postcode]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                    .maybe("focus", focus()?)
                    .maybe("country", country),
                (None, false) => {
                    return Err(invalid("addressQuery or address is required"));
                }
            },
        )
    }
}

/// `languageCode`. Geocoding's default is the local language, which is og's
/// own default too.
fn language<'a>(state: &'a AppState, query: &QueryParams) -> Result<Language<'a>, VendorError> {
    Language::parse(state.pack(), query.get("languageCode"))
}

/// The middle of `locationBias.rectangle`, as og's `focus`.
fn bias(query: &QueryParams) -> Result<Option<String>, VendorError> {
    let corner = |prefix: &str| -> Result<Option<LatLng>, VendorError> {
        let latitude = query.double(&format!("{prefix}.latitude"))?;
        let longitude = query.double(&format!("{prefix}.longitude"))?;
        match (latitude, longitude) {
            (Some(latitude), Some(longitude)) => Ok(Some(LatLng {
                latitude,
                longitude,
            })),
            (None, None) => Ok(None),
            _ => Err(invalid(format!("{prefix} requires latitude and longitude"))),
        }
    };
    let low = corner("locationBias.rectangle.low")?;
    let high = corner("locationBias.rectangle.high")?;
    let rectangle = match (low, high) {
        (Some(low), Some(high)) => Viewport { low, high },
        (None, None) => return Ok(None),
        _ => return Err(invalid("locationBias.rectangle requires low and high")),
    };
    let [lon, lat] = rectangle.center("locationBias.rectangle")?;
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
    let language = language(state, query)?;
    let found = native::reverse(
        state,
        Args::new()
            .set("lon", lon.to_string())
            .set("lat", lat.to_string())
            .set(
                "radius",
                crate::compat::radius(crate::compat::NEAREST_HOUSE_RADIUS_M),
            )
            .maybe("lang", language.code()),
    )
    .await
    .map_err(from_problem)?;
    // Google filters types and granularity after choosing results.
    let types = query.all("types");
    let granularities = query.all("granularity");
    let results = found
        .features
        .iter()
        .filter(|feature| {
            granularities.is_empty()
                || granularities.contains(&granularity(&feature.properties.common))
        })
        .map(|feature| {
            GeocodeResult::of(
                &feature.properties.common,
                feature.geometry.coordinates,
                &language,
            )
        })
        .filter(|result| types.is_empty() || result.types.iter().any(|kind| types.contains(kind)))
        .collect();
    let context = found
        .features
        .first()
        .map(|feature| &feature.properties.common.context);
    let plus_code = Some(PlusCode::of([lon, lat], context, &language));
    Ok(respond(
        &GeocodeResponse { results, plus_code },
        mask.as_ref(),
    ))
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
        let language = language(&state, &query)?;
        let feature = by_place_id(&state, &place_id, &language).await?;
        let result = GeocodeResult::of(
            &feature.properties.common,
            feature.geometry.coordinates,
            &language,
        );
        Ok(respond(&result, mask.as_ref()))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::path_text;

    #[test]
    fn path_query_is_decoded_once() {
        assert_eq!(
            path_text("A%2BB+%26+C&D%2520").expect("decoded path"),
            "A+B & C&D%20"
        );
    }
}
