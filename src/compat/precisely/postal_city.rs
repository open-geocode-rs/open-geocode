//! `/v1/autocomplete/postal-city`, the postcodes or towns that start with
//! what is typed, each with the areas around it.

use axum::{
    Json,
    extract::{Request, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use super::{
    Failure, Preferences, country_code, error, given, read_json,
    result::{AdminField, CountryField, Feature, Product, SimpleFeature},
};
use crate::compat::{Args, VendorError, native};
use crate::runtime::AppState;

#[derive(Debug, Deserialize)]
struct PostalCityRequest {
    #[serde(default)]
    preferences: Preferences,
    address: PostalCityAddress,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostalCityAddress {
    #[serde(rename = "type")]
    kind: Option<String>,
    post_address: Option<String>,
    country: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Postal,
    City,
}

impl Kind {
    /// The kind asked for. With none named, text with a digit in it is read
    /// as a postcode, since a town's name rarely has one and nearly every
    /// country's postcodes do.
    fn of(named: Option<&str>, text: &str) -> Result<Self, VendorError> {
        match named.map(str::to_ascii_uppercase).as_deref() {
            Some("POSTAL") => Ok(Self::Postal),
            Some("CITY") => Ok(Self::City),
            Some(_) => Err(error(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "Type should be either POSTAL or CITY only (DIS-LI-1025)",
            )),
            None if text.chars().any(|c| c.is_ascii_digit()) => Ok(Self::Postal),
            None => Ok(Self::City),
        }
    }

    const fn layer(self) -> &'static str {
        match self {
            Self::Postal => "postcode",
            Self::City => "locality",
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct PostalCityResponse {
    response: Predictions,
}

#[derive(Debug, Serialize)]
struct Predictions {
    status: &'static str,
    predictions: Vec<Prediction>,
}

#[derive(Debug, Serialize)]
struct Prediction {
    address: PredictionAddress,
    location: PredictionLocation,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PredictionAddress {
    #[serde(skip_serializing_if = "Option::is_none")]
    country: Option<CountryField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    admin1: Option<AdminField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    admin2: Option<AdminField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    city: Option<AdminField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    postal_code: Option<String>,
}

#[derive(Debug, Serialize)]
struct PredictionLocation {
    feature: SimpleFeature,
}

fn prediction(feature: &Feature, kind: Kind) -> Prediction {
    let candidate = feature.candidate(Product::Geocode);
    let address = candidate.address;
    let own = |name: &str| AdminField {
        long_name: Some(name.to_string()),
        short_name: None,
    };
    Prediction {
        address: PredictionAddress {
            // The postal-city answer names the country by its alpha-3 code
            // alone.
            country: address
                .country
                .and_then(|country| country.iso_alpha3_code)
                .map(|code| CountryField {
                    name: None,
                    iso_alpha2_code: None,
                    iso_alpha3_code: Some(code),
                    iso_numeric_code: None,
                }),
            admin1: address.admin1,
            admin2: address.admin2,
            city: match kind {
                Kind::City => Some(own(feature.name())),
                Kind::Postal => address.city,
            },
            postal_code: match kind {
                Kind::Postal => Some(feature.name().to_string()),
                Kind::City => address.postal_code,
            },
        },
        location: PredictionLocation {
            feature: SimpleFeature::point(feature.coordinates()),
        },
    }
}

pub(super) async fn postal_city(
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<PostalCityResponse>, VendorError> {
    let body = read_json::<PostalCityRequest>(&state, request, "INVALID_REQUEST").await?;
    let max = body.preferences.max_results()?;
    let address = body.address;
    let country = given(address.country.as_deref()).ok_or_else(|| {
        error(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "Country is not provided in the request (DIS-LI-1015)",
        )
    })?;
    let text = given(address.post_address.as_deref()).ok_or_else(|| {
        error(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "Request Body is not valid (DIS-LI-1018)",
        )
    })?;
    let kind = Kind::of(given(address.kind.as_deref()), text)?;
    let args = Args::new()
        .set("q", text)
        .set("limit", max.to_string())
        .set("layer", kind.layer())
        .set("country", country_code(country));
    let collection = native::autocomplete(&state, args)
        .await
        .map_err(|problem| Failure::of(&problem, Some(country)).into_error(problem))?;
    let features = collection
        .features
        .iter()
        .map(Feature::from)
        .collect::<Vec<_>>();
    let predictions = features
        .iter()
        .map(|feature| prediction(feature, kind))
        .collect::<Vec<_>>();
    Ok(Json(PostalCityResponse {
        response: Predictions {
            status: if predictions.is_empty() {
                "ZERO_RESULTS"
            } else {
                "OK"
            },
            predictions,
        },
    }))
}
