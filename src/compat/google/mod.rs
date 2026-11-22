//! `/compat/google`, serving Places API (New) v1 and Geocoding API v4.
//!
//! Routes keep Google's own paths after the prefix, so a client built for
//! `places.googleapis.com` or `geocode.googleapis.com` needs only its base
//! URL changed. The two hosts share one prefix, as their paths never collide.

mod destinations;
mod geocode;
mod hours;
mod language;
mod model;
mod places;
#[cfg(test)]
mod tests;

use axum::{
    Router,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{MethodFilter, MethodRouter, on},
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::{
    compat::{self, VendorError, fieldmask::FieldMask},
    http::problem::{ErrorCode, Problem},
    runtime::AppState,
};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/places:searchText",
            only(MethodFilter::POST, places::search_text),
        )
        .route(
            "/v1/places:autocomplete",
            only(MethodFilter::POST, places::autocomplete),
        )
        .route(
            "/v1/places:searchNearby",
            only(MethodFilter::POST, places::search_nearby),
        )
        .route(
            "/v1/places/{place_id}",
            only(MethodFilter::GET, places::details),
        )
        .route(
            "/v1/places/{place_id}/photos/{photo}/media",
            only(MethodFilter::GET, places::photo_media),
        )
        .route(
            "/v4/geocode/address",
            only(MethodFilter::GET, geocode::address),
        )
        .route(
            "/v4/geocode/address/{address_query}",
            only(MethodFilter::GET, geocode::address_in_path),
        )
        .route(
            "/v4/geocode/location",
            only(MethodFilter::GET, geocode::location),
        )
        .route(
            "/v4/geocode/location/{location_query}",
            only(MethodFilter::GET, geocode::location_in_path),
        )
        .route(
            "/v4/geocode/places/{place_id}",
            only(MethodFilter::GET, geocode::place),
        )
        .route(
            "/v4/geocode/destinations",
            only(MethodFilter::POST, destinations::search),
        )
        .fallback(|| async { method_not_found() })
}

/// Google has no 405. A path served under another method is a method its
/// front end does not know, the same 404 as an unknown path.
fn only<H, T>(filter: MethodFilter, handler: H) -> MethodRouter<AppState>
where
    H: axum::handler::Handler<T, AppState>,
    T: 'static,
{
    on(filter, handler).fallback(|| async { method_not_found() })
}

/// `google.rpc.Code` names with the HTTP status Google answers each with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Code {
    InvalidArgument,
    NotFound,
    Internal,
    Unimplemented,
    Unavailable,
    DeadlineExceeded,
}

impl Code {
    const fn status(self) -> StatusCode {
        match self {
            Self::InvalidArgument => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Unimplemented => StatusCode::NOT_IMPLEMENTED,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::DeadlineExceeded => StatusCode::GATEWAY_TIMEOUT,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::NotFound => "NOT_FOUND",
            Self::Internal => "INTERNAL",
            Self::Unimplemented => "UNIMPLEMENTED",
            Self::Unavailable => "UNAVAILABLE",
            Self::DeadlineExceeded => "DEADLINE_EXCEEDED",
        }
    }
}

/// Google's error envelope.
pub(super) fn error(code: Code, message: impl Into<String>) -> VendorError {
    let status = code.status();
    VendorError::new(
        status,
        json!({"error": {
            "code": status.as_u16(),
            "message": message.into(),
            "status": code.name(),
        }}),
    )
}

pub(super) fn invalid(message: impl Into<String>) -> VendorError {
    error(Code::InvalidArgument, message)
}

fn method_not_found() -> VendorError {
    error(Code::NotFound, "Method not found.")
}

/// og's failure as Google reports it. og's own detail is kept as the
/// message, since it names the value rejected and why.
pub(super) fn from_problem(problem: Problem) -> VendorError {
    let code = match (problem.status(), problem.error_code()) {
        (_, ErrorCode::Timeout) => Code::DeadlineExceeded,
        (400, _) => Code::InvalidArgument,
        (404, _) => Code::NotFound,
        (501, _) => Code::Unimplemented,
        (503, _) => Code::Unavailable,
        _ => Code::Internal,
    };
    let message = match (code, problem.detail()) {
        (Code::Internal, _) | (_, None) => "Internal error encountered.".to_string(),
        (_, Some(detail)) => detail.to_string(),
    };
    let retry = compat::retry_after(&problem);
    let mut error = error(code, message);
    if let Some((name, value)) = retry {
        error = error.with_header(name, value);
    }
    error.caused_by(problem)
}

/// A handler body whose `?` turns into Google's error answer.
pub(super) async fn run(handler: impl Future<Output = Result<Response, VendorError>>) -> Response {
    handler.await.unwrap_or_else(IntoResponse::into_response)
}

/// A query string as Google's REST transcoding reads it, with dotted names
/// for message fields and a repeated field as its key repeated.
#[derive(Debug, Default)]
pub(super) struct QueryParams(Vec<(String, String)>);

impl QueryParams {
    pub(super) fn parse(raw: Option<&str>) -> Result<Self, VendorError> {
        serde_urlencoded::from_str(raw.unwrap_or_default())
            .map(Self)
            .map_err(|error| invalid(format!("Invalid query string: {error}")))
    }

    /// The first value sent for `name`, unless blank. proto3 reads an empty
    /// string as an unset field.
    pub(super) fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
    }

    pub(super) fn all(&self, name: &str) -> Vec<&str> {
        self.0
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
            .collect()
    }

    /// A double field, as Google names a bad one in its message.
    pub(super) fn double(&self, name: &str) -> Result<Option<f64>, VendorError> {
        self.get(name)
            .map(|value| {
                value.parse::<f64>().map_err(|_| {
                    invalid(format!(
                        "Invalid value at '{name}' (TYPE_DOUBLE), \"{value}\""
                    ))
                })
            })
            .transpose()
    }
}

/// The response field mask, from the `X-Goog-FieldMask` header, else
/// `$fields`, else `fields`. Google refuses a Places request without one; og answers
/// every field instead (decision 15a).
pub(super) fn field_mask(headers: &HeaderMap, query: &QueryParams) -> Option<FieldMask> {
    compat::header(headers, "x-goog-fieldmask")
        .or_else(|| query.get("$fields"))
        .or_else(|| query.get("fields"))
        .and_then(FieldMask::parse)
}

/// The JSON answer, pruned to the mask.
pub(super) fn respond<T: Serialize>(answer: &T, mask: Option<&FieldMask>) -> Response {
    match serde_json::to_value(answer) {
        Ok(mut value) => {
            if let Some(mask) = mask {
                mask.apply(&mut value);
            }
            axum::Json(value).into_response()
        }
        Err(cause) => from_problem(Problem::internal(cause.into())).into_response(),
    }
}

/// A JSON request body. Unknown fields are ignored, as they are the
/// parameters og cannot honour.
pub(super) fn body<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, VendorError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return serde_json::from_slice(b"{}")
            .map_err(|error| invalid(format!("Invalid JSON payload received. {error}")));
    }
    serde_json::from_slice(bytes)
        .map_err(|error| invalid(format!("Invalid JSON payload received. {error}")))
}

/// A `google.type.LatLng`. proto3 JSON may send a double as a string.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub(super) struct LatLng {
    #[serde(deserialize_with = "proto_double")]
    pub(super) latitude: f64,
    #[serde(deserialize_with = "proto_double")]
    pub(super) longitude: f64,
}

impl LatLng {
    /// og's `lon,lat`, once the point is on the globe.
    pub(super) fn checked(self, field: &str) -> Result<[f64; 2], VendorError> {
        if !(-90.0..=90.0).contains(&self.latitude) {
            return Err(invalid(format!(
                "{field}.latitude must be within [-90, 90], got {}",
                self.latitude
            )));
        }
        if !(-180.0..=180.0).contains(&self.longitude) {
            return Err(invalid(format!(
                "{field}.longitude must be within [-180, 180], got {}",
                self.longitude
            )));
        }
        Ok([self.longitude, self.latitude])
    }
}

/// A `Viewport`, whose `low` is the south-west corner and `high` the
/// north-east.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub(super) struct Viewport {
    pub(super) low: LatLng,
    pub(super) high: LatLng,
}

impl Viewport {
    /// og's `bbox`. og cannot search a box across the antimeridian, and
    /// dropping the restriction would return places outside it, so such a
    /// box is refused.
    pub(super) fn bbox(self, field: &str) -> Result<String, VendorError> {
        let [west, south] = self.low.checked(&format!("{field}.low"))?;
        let [east, north] = self.high.checked(&format!("{field}.high"))?;
        if south > north {
            return Err(invalid(format!(
                "{field} is empty: low.latitude is north of high.latitude"
            )));
        }
        if west > east {
            return Err(invalid(format!(
                "{field} crosses the antimeridian, which this server cannot restrict to"
            )));
        }
        Ok(format!("{west},{south},{east},{north}"))
    }

    /// The middle of the box, across the antimeridian too, as og's `focus`.
    pub(super) fn center(self, field: &str) -> Result<[f64; 2], VendorError> {
        let [west, south] = self.low.checked(&format!("{field}.low"))?;
        let [east, north] = self.high.checked(&format!("{field}.high"))?;
        let east = if west > east { east + 360.0 } else { east };
        let mut lon = f64::midpoint(west, east);
        if lon > 180.0 {
            lon -= 360.0;
        }
        Ok([lon, f64::midpoint(south, north)])
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub(super) struct Circle {
    pub(super) center: LatLng,
    #[serde(deserialize_with = "proto_double")]
    pub(super) radius: f64,
}

/// `locationBias` or `locationRestriction`, a circle or a rectangle.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default)]
pub(super) struct Area {
    pub(super) circle: Option<Circle>,
    pub(super) rectangle: Option<Viewport>,
}

impl Area {
    /// The point a bias prefers results near, as og's `focus`. og breaks
    /// ties by nearness to one point and has no radius or box to bias by.
    pub(super) fn focus(self, field: &str) -> Result<Option<String>, VendorError> {
        let point = match (self.circle, self.rectangle) {
            (Some(_), Some(_)) => return Err(both_shapes(field)),
            (Some(circle), None) => circle.center.checked(&format!("{field}.circle.center"))?,
            (None, Some(rectangle)) => rectangle.center(&format!("{field}.rectangle"))?,
            (None, None) => return Ok(None),
        };
        Ok(Some(format!("{},{}", point[0], point[1])))
    }
}

pub(super) fn both_shapes(field: &str) -> VendorError {
    invalid(format!("{field} takes a circle or a rectangle, not both"))
}

fn proto_double<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    match Value::deserialize(deserializer)? {
        Value::Number(number) => number
            .as_f64()
            .ok_or_else(|| serde::de::Error::custom("not a double")),
        Value::String(text) => text
            .trim()
            .parse()
            .map_err(|_| serde::de::Error::custom(format!("{text:?} is not a double"))),
        Value::Null => Ok(0.0),
        other => Err(serde::de::Error::custom(format!("{other} is not a double"))),
    }
}

/// An int32 field. proto3 JSON sends one as a number or a decimal string.
pub(super) fn proto_int<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<i64>, D::Error> {
    match Value::deserialize(deserializer)? {
        Value::Number(number) => number
            .as_i64()
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("{number} is not an integer"))),
        Value::String(text) => text
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| serde::de::Error::custom(format!("{text:?} is not an integer"))),
        Value::Null => Ok(None),
        other => Err(serde::de::Error::custom(format!(
            "{other} is not an integer"
        ))),
    }
}
