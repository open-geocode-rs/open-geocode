//! Real-time Geo Addressing v1. Geocode, reverse geocode, lookup and
//! autocomplete, each item answered by og's own lookup.

use std::time::Instant;

use axum::{
    Json,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use super::{
    Failure, Preferences, country_code, error, given, read_json,
    result::{Candidate, Feature, Product},
};
use crate::{
    compat::{Args, VendorError, decode_id, native},
    http::{
        problem::Problem,
        request_id::{ErrorLog, RequestId},
    },
    runtime::AppState,
};

/// Items one request may carry.
const MAX_ITEMS: usize = 25;

/// An input address, `RequestAddress`. Fields og has no parameter for
/// (`admin2`, `building`, `floor` and so on) are not read.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct InputAddress {
    pub(super) address_id: Option<String>,
    #[serde(default)]
    pub(super) address_lines: Vec<String>,
    pub(super) country: Option<String>,
    pub(super) address_number: Option<String>,
    pub(super) street: Option<String>,
    pub(super) city: Option<String>,
    pub(super) admin1: Option<String>,
    pub(super) postal_code: Option<String>,
    pub(super) place_name: Option<String>,
}

impl InputAddress {
    fn country(&self) -> Option<&str> {
        given(self.country.as_deref())
    }

    /// The street line. The address lines, or failing them the house
    /// number and street.
    fn street_line(&self) -> Option<String> {
        let lines = self
            .address_lines
            .iter()
            .filter_map(|line| given(Some(line)))
            .collect::<Vec<_>>();
        let line = if lines.is_empty() {
            [self.address_number.as_deref(), self.street.as_deref()]
                .into_iter()
                .filter_map(given)
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            lines.join(", ")
        };
        (!line.is_empty()).then_some(line)
    }

    /// The address as free text.
    fn text(&self) -> String {
        let street_line = self.street_line();
        [
            self.place_name.as_deref(),
            street_line.as_deref(),
            self.city.as_deref(),
            self.admin1.as_deref(),
            self.postal_code.as_deref(),
        ]
        .into_iter()
        .filter_map(given)
        .collect::<Vec<_>>()
        .join(", ")
    }

    /// og's `/geocode` parameters. A street line with its town, region or
    /// postcode apart goes in og's address fields, which match each part on
    /// its own; anything else is free text.
    pub(super) fn geocode_params(&self, limit: usize) -> Vec<(&'static str, String)> {
        let mut params = vec![("limit", limit.to_string())];
        if let Some(country) = self.country() {
            params.push(("country", country_code(country)));
        }
        let parts = [
            ("locality", &self.city),
            ("region", &self.admin1),
            ("postcode", &self.postal_code),
        ]
        .into_iter()
        .filter_map(|(name, value)| given(value.as_deref()).map(|value| (name, value.to_string())))
        .collect::<Vec<_>>();
        match self.street_line() {
            Some(address) if !parts.is_empty() => {
                params.push(("address", address));
                params.extend(parts);
            }
            _ => params.push(("q", self.text())),
        }
        params
    }
}

#[derive(Debug, Deserialize)]
struct GeocodeRequest {
    #[serde(default)]
    preferences: Preferences,
    addresses: Vec<InputAddress>,
}

#[derive(Debug, Deserialize)]
struct ReverseRequest {
    #[serde(default)]
    preferences: Preferences,
    locations: Vec<InputLocation>,
}

/// `LocationDetail`. og finds the country from the point, so a stated
/// `country` keeps only results in it, as a point near a border may answer
/// with the other side.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputLocation {
    address_id: Option<String>,
    longitude: Option<f64>,
    latitude: Option<f64>,
    country: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LookupRequest {
    keys: Vec<LookupKey>,
}

/// `LookupSingleRequest`. Its `country` is not read, since an og id names
/// one record wherever it is.
#[derive(Debug, Deserialize)]
struct LookupKey {
    #[serde(default)]
    key: Option<String>,
    #[serde(rename = "type", default)]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AutocompleteRequest {
    #[serde(default)]
    preferences: Preferences,
    address: InputAddress,
}

/// One item's answer, `Response`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    address_id: Option<String>,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    results: Option<Vec<Candidate>>,
}

#[derive(Debug, Serialize)]
struct Responses {
    responses: Vec<ItemResponse>,
}

/// One item's answer, and the Problem behind a failed one.
#[derive(Debug)]
struct Outcome {
    response: ItemResponse,
    failure: Option<(Failure, Option<Problem>)>,
}

impl Outcome {
    fn found(
        address_id: Option<String>,
        features: &[Feature],
        max: usize,
        product: Product,
    ) -> Self {
        let results = features
            .iter()
            .take(max)
            .map(|feature| feature.candidate(product))
            .collect::<Vec<_>>();
        let status = if results.is_empty() {
            "ZERO_RESULTS"
        } else {
            "OK"
        };
        Self {
            response: ItemResponse {
                address_id,
                status,
                error_message: None,
                results: Some(results),
            },
            failure: None,
        }
    }

    fn failed(address_id: Option<String>, failure: Failure, problem: Option<Problem>) -> Self {
        Self {
            response: ItemResponse {
                address_id,
                status: failure.status,
                error_message: Some(failure.message.clone()),
                results: None,
            },
            failure: Some((failure, problem)),
        }
    }

    /// An item og could not answer, its Problem as Precisely names it.
    fn refused(address_id: Option<String>, problem: Problem, country: Option<&str>) -> Self {
        Self::failed(address_id, Failure::of(&problem, country), Some(problem))
    }

    fn of_features<'a, T: 'a>(
        address_id: Option<String>,
        features: &'a [T],
        max: usize,
        product: Product,
    ) -> Self
    where
        &'a T: Into<Feature>,
    {
        let features = features.iter().map(Into::into).collect::<Vec<_>>();
        Self::found(address_id, &features, max, product)
    }
}

fn args(params: Vec<(&'static str, String)>) -> Args {
    params
        .into_iter()
        .fold(Args::new(), |args, (name, value)| args.set(name, value))
}

/// The batch's items, refused whole when there are none or too many.
fn checked<T>(items: Vec<T>) -> Result<Vec<T>, VendorError> {
    if items.is_empty() {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "INVALID_REQUEST",
            "Request Body is not valid (DIS-LI-1018)",
        ));
    }
    if items.len() > MAX_ITEMS {
        return Err(error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "BATCH_SIZE_EXCEEDED",
            format!("Batch size must not exceed {MAX_ITEMS} (DIS-LI-1014)"),
        ));
    }
    Ok(items)
}

/// A batch answers 200 when every item is answered, 206 when one failed. A
/// failure no item escaped, or one of the request's own preferences, fails
/// the request instead, as the whole request's error.
fn respond(
    log: &ErrorLog,
    route: &str,
    request_id: Option<&str>,
    started: Instant,
    outcomes: Vec<Outcome>,
) -> Response {
    let failures = outcomes
        .iter()
        .filter_map(|outcome| outcome.failure.as_ref())
        .collect::<Vec<_>>();
    let whole = failures.iter().find(|(failure, _)| {
        failure.of_request
            || (failures.len() == outcomes.len()
                && failure.http.is_server_error()
                && failures.iter().all(|(other, _)| other == failure))
    });
    if let Some((failure, problem)) = whole {
        let failure = failure.clone();
        return match problem.clone() {
            Some(problem) => failure.into_error(problem).into_response(),
            None => error(failure.http, failure.status, failure.message).into_response(),
        };
    }
    // A 500 inside a 206 is no 500 to the request-id layer, so it is
    // logged here, by item.
    for (index, outcome) in outcomes.iter().enumerate() {
        if let Some((failure, Some(problem))) = &outcome.failure
            && failure.http == StatusCode::INTERNAL_SERVER_ERROR
        {
            log.internal_error(
                request_id.unwrap_or("-"),
                route,
                Some(index),
                started,
                problem.cause().map(|cause| format!("{cause:#}")).as_deref(),
            );
        }
    }
    let status = if failures.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::PARTIAL_CONTENT
    };
    let responses = outcomes
        .into_iter()
        .map(|outcome| outcome.response)
        .collect();
    (status, Json(Responses { responses })).into_response()
}

fn request_id(request: &Request) -> Option<String> {
    request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.0.clone())
}

pub(super) async fn geocode(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, VendorError> {
    addresses(state, request, Product::Geocode).await
}

/// Verify reads the address as geocode does and answers the record og
/// matched, standardized, with how each stated part matched it.
pub(super) async fn verify(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, VendorError> {
    addresses(state, request, Product::Verify).await
}

async fn addresses(
    state: AppState,
    request: Request,
    product: Product,
) -> Result<Response, VendorError> {
    let started = Instant::now();
    let id = request_id(&request);
    let body = read_json::<GeocodeRequest>(&state, request, "INVALID_REQUEST").await?;
    let max = body.preferences.max_results()?;
    let mut outcomes = Vec::new();
    for address in checked(body.addresses)? {
        let address_id = address.address_id.clone();
        outcomes.push(
            match native::geocode(&state, args(address.geocode_params(max))).await {
                Ok(collection) => {
                    Outcome::of_features(address_id, &collection.features, max, product)
                }
                Err(problem) => Outcome::refused(address_id, problem, address.country()),
            },
        );
    }
    let route = match product {
        Product::Geocode => "/compat/precisely/v1/geocode",
        Product::Verify => "/compat/precisely/v1/verify",
    };
    Ok(respond(
        state.error_log(),
        route,
        id.as_deref(),
        started,
        outcomes,
    ))
}

pub(super) async fn reverse(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, VendorError> {
    let started = Instant::now();
    let id = request_id(&request);
    let body = read_json::<ReverseRequest>(&state, request, "INVALID_REQUEST").await?;
    let max = body.preferences.max_results()?;
    let radius_m = body.preferences.distance_m()?;
    let mut outcomes = Vec::new();
    for location in checked(body.locations)? {
        let args = Args::new()
            .maybe("lon", location.longitude.map(|lon| lon.to_string()))
            .maybe("lat", location.latitude.map(|lat| lat.to_string()));
        let wanted = given(location.country.as_deref()).map(country_code);
        outcomes.push(match native::reverse(&state, args).await {
            Ok(collection) => {
                // A result past the search radius is not one Precisely would
                // find; an area holds the point, so it has no distance.
                let features = collection
                    .features
                    .iter()
                    .map(Feature::from)
                    .filter(|feature| {
                        radius_m.is_none_or(|radius| {
                            feature
                                .distance_m()
                                .is_none_or(|distance| distance <= radius)
                        })
                    })
                    .filter(|feature| {
                        wanted.as_deref().is_none_or(|wanted| {
                            feature
                                .country_code()
                                .is_some_and(|code| code.eq_ignore_ascii_case(wanted))
                        })
                    })
                    .collect::<Vec<_>>();
                Outcome::found(location.address_id, &features, max, Product::Geocode)
            }
            Err(problem) => Outcome::refused(location.address_id, problem, None),
        });
    }
    Ok(respond(
        state.error_log(),
        "/compat/precisely/v1/reverse-geocode",
        id.as_deref(),
        started,
        outcomes,
    ))
}

/// Key types Precisely documents. og answers only `PB_KEY`, which carries
/// og's own id; the others name records in registers og does not keep.
const KEY_TYPES: [&str; 5] = ["PB_KEY", "GNAF_PID", "UDPRN", "UPRN", "EIR_CODE"];

pub(super) async fn lookup(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, VendorError> {
    let started = Instant::now();
    let id = request_id(&request);
    let body = read_json::<LookupRequest>(&state, request, "INVALID_REQUEST").await?;
    let invalid = |message: String| Outcome::failed(None, Failure::invalid(message), None);
    let mut outcomes = Vec::new();
    for key in checked(body.keys)? {
        let kind = key.kind.as_deref().unwrap_or_default();
        let value = given(key.key.as_deref());
        outcomes.push(match (kind, value) {
            (kind, _) if !KEY_TYPES.contains(&kind) => invalid(
                "Key type should be either PB_KEY, GNAF_PID, EIR_CODE, UPRN or UDPRN only \
                 (DIS-LI-1017)"
                    .to_string(),
            ),
            (_, None) => invalid("Lookup key cannot be null or empty (DIS-LI-1027)".to_string()),
            ("PB_KEY", Some(value)) => match decode_id(value) {
                None => Outcome::found(None, &[], 1, Product::Geocode),
                Some(og_id) => match native::by_id(&state, &og_id, None).await {
                    Ok(feature) => {
                        Outcome::of_features(None, feature.as_slice(), 1, Product::Geocode)
                    }
                    Err(problem) => Outcome::refused(None, problem, None),
                },
            },
            (kind, Some(_)) => invalid(format!("Key type {kind} is not supported by this server")),
        });
    }
    Ok(respond(
        state.error_log(),
        "/compat/precisely/v1/lookup",
        id.as_deref(),
        started,
        outcomes,
    ))
}

#[derive(Debug, Serialize)]
pub(super) struct AutocompleteResponse {
    response: Predictions,
}

#[derive(Debug, Serialize)]
struct Predictions {
    status: &'static str,
    predictions: Vec<Prediction>,
}

#[derive(Debug, Serialize)]
struct Prediction {
    prediction: String,
    #[serde(flatten)]
    candidate: Candidate,
}

pub(super) async fn autocomplete(
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<AutocompleteResponse>, VendorError> {
    let body = read_json::<AutocompleteRequest>(&state, request, "INVALID_REQUEST").await?;
    let max = body.preferences.max_results()?;
    let address = body.address;
    let args = Args::new()
        .set("q", address.text())
        .set("limit", max.to_string())
        .maybe("country", address.country().map(country_code));
    let collection = native::autocomplete(&state, args)
        .await
        .map_err(|problem| Failure::of(&problem, address.country()).into_error(problem))?;
    let features = collection
        .features
        .iter()
        .map(Feature::from)
        .collect::<Vec<_>>();
    let predictions = features
        .iter()
        .map(|feature| Prediction {
            prediction: feature.label().to_string(),
            candidate: feature.candidate(Product::Geocode),
        })
        .collect::<Vec<_>>();
    Ok(Json(AutocompleteResponse {
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

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use serde_json::{Value, json};

    use super::*;

    async fn answer(log: &ErrorLog, outcomes: Vec<Outcome>) -> (StatusCode, Option<String>, Value) {
        let response = respond(
            log,
            "/compat/precisely/v1/geocode",
            Some("id-1"),
            Instant::now(),
            outcomes,
        );
        let status = response.status();
        let retry = response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .map(|value| value.to_str().expect("ASCII").to_string());
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, retry, serde_json::from_slice(&body).expect("JSON"))
    }

    fn busy(id: &str) -> Outcome {
        Outcome::refused(Some(id.to_string()), Problem::busy(7), None)
    }

    #[tokio::test]
    async fn a_busy_server_fails_the_request_only_when_no_item_got_through() {
        let log = ErrorLog::capture();
        assert_eq!(
            answer(&log, vec![busy("1"), busy("2")]).await,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Some("7".to_string()),
                json!({"errors": [{"status": "SERVICE_UNAVAILABLE", "errorMessage":
                    "Issue with internal services. Please try again after sometime (DIS-LI-1011)"}]})
            )
        );
        assert_eq!(
            answer(
                &log,
                vec![
                    busy("1"),
                    Outcome::found(Some("2".to_string()), &[], 1, Product::Geocode)
                ]
            )
            .await,
            (
                StatusCode::PARTIAL_CONTENT,
                None,
                json!({"responses": [
                    {"addressId": "1", "status": "SERVICE_UNAVAILABLE", "errorMessage":
                        "Issue with internal services. Please try again after sometime (DIS-LI-1011)"},
                    {"addressId": "2", "status": "ZERO_RESULTS", "results": []},
                ]})
            )
        );
        let timed_out = Outcome::refused(
            Some("2".to_string()),
            Problem::timed_out(std::time::Duration::from_millis(5)),
            None,
        );
        let (status, retry, body) = answer(&log, vec![busy("1"), timed_out]).await;
        assert_eq!((status, retry), (StatusCode::PARTIAL_CONTENT, None));
        assert_eq!(body["responses"][1]["status"], "REQUEST_TIMEOUT");
        assert_eq!(log.lines(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn an_item_that_failed_inside_og_is_logged_with_its_place() {
        let log = ErrorLog::capture();
        let broken = Outcome::refused(
            Some("2".to_string()),
            Problem::internal(anyhow::anyhow!("section offset past end of file")),
            None,
        );
        let (status, _, body) = answer(
            &log,
            vec![
                Outcome::found(Some("1".to_string()), &[], 1, Product::Geocode),
                broken,
            ],
        )
        .await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            body["responses"][1],
            json!({"addressId": "2", "status": "INTERNAL_ERROR", "errorMessage":
                "Issue with internal services. Please try again after sometime (DIS-1003)"})
        );
        let lines = log
            .lines()
            .iter()
            .map(|line| serde_json::from_str::<Value>(line).expect("JSON line"))
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            [
                &lines[0]["request_id"],
                &lines[0]["route"],
                &lines[0]["item"],
                &lines[0]["cause"]
            ],
            [
                &json!("id-1"),
                &json!("/compat/precisely/v1/geocode"),
                &json!(1),
                &json!("section offset past end of file")
            ]
        );
    }
}
