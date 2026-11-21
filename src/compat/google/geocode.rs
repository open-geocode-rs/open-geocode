//! Address, location and place geocoding of Geocoding API v4.

use axum::{
    extract::{Path, RawPathParams, RawQuery, State},
    http::HeaderMap,
    response::Response,
};
use serde::Serialize;

use super::{
    LatLng, QueryParams, Viewport, field_mask, from_problem, invalid,
    language::Language,
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
    let mut results: Vec<_> = found
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
    prefer_precise_address_points(&mut results);
    Ok(respond(&GeocodeResponse { results }, mask.as_ref()))
}

// Once an address is known at rooftop accuracy, a nearby centre for the
// same complete postal address is redundant. Keep separately mapped
// rooftops, distinct units and distant results even when labels agree.
fn prefer_precise_address_points(results: &mut Vec<GeocodeResult>) {
    let rooftops = results
        .iter()
        .filter(|result| {
            result.granularity == "ROOFTOP" && result.types.contains(&"street_address")
        })
        .filter_map(|result| {
            let postal = result.postal_address.as_ref()?;
            postal.region_code.as_ref()?;
            postal.locality.as_ref()?;
            postal.postal_code.as_ref()?;
            postal.address_lines.first()?;
            Some((postal.clone(), result.viewport?))
        })
        .collect::<Vec<_>>();
    results.retain(|result| {
        result.granularity != "GEOMETRIC_CENTER"
            || !result.types.contains(&"street_address")
            || !rooftops.iter().any(|(postal, viewport)| {
                result.postal_address.as_ref() == Some(postal) && viewport.contains(result.location)
            })
    });
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
    let language = language(state, query)?;
    let found = native::reverse(
        state,
        Args::new()
            .set("lon", lon.to_string())
            .set("lat", lat.to_string())
            .maybe("lang", language.code()),
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
        .map(|feature| {
            GeocodeResult::of(
                &feature.properties.common,
                feature.geometry.coordinates,
                &language,
            )
        })
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
    use super::*;
    use crate::compat::google::model::{
        LatLng as ResultPoint, PostalAddress, Viewport as DisplayViewport,
    };

    #[test]
    fn a_rooftop_only_supersedes_the_nearby_centre_of_the_same_postal_address() {
        let rooftop = GeocodeResult {
            place: "places/og_rooftop".to_string(),
            place_id: "og_rooftop".to_string(),
            location: ResultPoint {
                latitude: 40.0,
                longitude: -80.0,
            },
            granularity: "ROOFTOP",
            formatted_address: None,
            postal_address: Some(PostalAddress {
                region_code: Some("US".to_string()),
                locality: Some("Town".to_string()),
                postal_code: Some("15201".to_string()),
                address_lines: vec!["12 Main Street".to_string()],
                ..PostalAddress::default()
            }),
            address_components: Vec::new(),
            types: vec!["street_address"],
            viewport: Some(DisplayViewport {
                low: ResultPoint {
                    latitude: 39.999,
                    longitude: -80.001,
                },
                high: ResultPoint {
                    latitude: 40.001,
                    longitude: -79.999,
                },
            }),
            bounds: None,
        };
        let mut centre = rooftop.clone();
        centre.place_id = "og_centre".to_string();
        centre.granularity = "GEOMETRIC_CENTER";
        let mut unit = centre.clone();
        unit.place_id = "og_unit".to_string();
        unit.postal_address
            .as_mut()
            .expect("address")
            .address_lines
            .push("2".to_string());
        let mut far = centre.clone();
        far.place_id = "og_far".to_string();
        far.location.latitude = 41.0;
        let mut other_rooftop = rooftop.clone();
        other_rooftop.place_id = "og_other_rooftop".to_string();
        let mut results = vec![centre, unit, rooftop, far, other_rooftop];
        prefer_precise_address_points(&mut results);
        assert_eq!(
            results
                .iter()
                .map(|result| result.place_id.as_str())
                .collect::<Vec<_>>(),
            ["og_unit", "og_rooftop", "og_far", "og_other_rooftop"]
        );
    }
}
