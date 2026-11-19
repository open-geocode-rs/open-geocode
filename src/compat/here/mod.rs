//! `/compat/here`, HERE Geocoding and Search v7 and Batch v7.
//!
//! HERE serves each endpoint from a host of its own
//! (`geocode.search.hereapi.com/v1/geocode`,
//! `batch.search.hereapi.com/v7/batch/jobs`). og serves them all under this
//! one prefix by path, so a client points every service's base URL here.
//!
//! A batch job runs as og `/jobs` jobs, one per start, each listed at
//! `/jobs/{id}` as any other.

mod batch;
mod category;
mod item;
mod search;
#[cfg(test)]
mod tests;

use axum::{
    Router,
    extract::{FromRequestParts, OriginalUri},
    http::{StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
    routing::{MethodRouter, any, get, post, put},
};
use serde_json::{Value, json};

use crate::{
    compat::{VendorError, iso, retry_after},
    http::{problem::Problem, request_id::RequestId},
    runtime::AppState,
};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/geocode", search_route(get(search::geocode)))
        .route("/v1/revgeocode", search_route(get(search::revgeocode)))
        .route(
            "/v1/multi-revgeocode",
            search_route(post(search::multi_revgeocode)),
        )
        .route(
            "/v1/autosuggest",
            search_route(get(search::autosuggest).post(search::autosuggest)),
        )
        .route("/v1/autocomplete", search_route(get(search::autocomplete)))
        .route(
            "/v1/discover",
            search_route(get(search::discover).post(search::discover)),
        )
        .route("/v1/lookup", search_route(get(search::lookup)))
        .route(
            "/v1/browse",
            search_route(get(search::browse).post(search::browse)),
        )
        .route("/v1/signals", search_route(post(not_implemented)))
        .route(
            "/v7/batch/jobs",
            batch_route(
                post(batch::create)
                    .get(batch::list)
                    .delete(batch::delete_many),
            ),
        )
        .route(
            "/v7/batch/jobs/{id}",
            batch_route(get(batch::details).delete(batch::delete)),
        )
        .route("/v7/batch/jobs/{id}/start", batch_route(put(batch::start)))
        .route("/v7/batch/jobs/{id}/stop", batch_route(put(batch::stop)))
        .route(
            "/v7/batch/jobs/{id}/results",
            batch_route(get(batch::results)),
        )
        .route(
            "/v7/batch/jobs/{id}/errors",
            batch_route(get(batch::errors)),
        )
        .route("/v7/batch/notifications", any(batch::not_implemented))
        .route(
            "/v7/batch/notifications/{*rest}",
            any(batch::not_implemented),
        )
        .route("/v7/openapi", any(batch::not_implemented))
        .route("/v7/version", batch_route(get(batch::version)))
        .route("/v7/health", batch_route(get(batch::health)))
        .fallback(not_found)
}

/// A Geocoding and Search route. Another method is HERE's 405.
fn search_route(route: MethodRouter<AppState>) -> MethodRouter<AppState> {
    route.fallback(method_not_allowed)
}

/// A Batch route. Its API documents no 405, so another method is its 404.
fn batch_route(route: MethodRouter<AppState>) -> MethodRouter<AppState> {
    route.fallback(batch::not_found)
}

/// The id og logs the request under, which HERE's error bodies carry as
/// both `requestId` and `correlationId`. A client that sent `X-Request-ID`
/// gets its own id back, and every id is one the log can be searched for.
pub(super) struct Rid(pub(super) String);

impl<S: Send + Sync> FromRequestParts<S> for Rid {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.extensions.get::<RequestId>().map_or_else(
            || uuid::Uuid::new_v4().to_string(),
            |id| id.0.clone(),
        )))
    }
}

/// A Geocoding and Search error, as HERE's `ErrorResponse` and its status.
#[derive(Debug)]
pub(super) struct Gs7Error {
    status: StatusCode,
    title: String,
    cause: Option<String>,
    action: Option<String>,
    problem: Option<Problem>,
}

impl Gs7Error {
    fn new(status: StatusCode, title: impl Into<String>) -> Self {
        Self {
            status,
            title: title.into(),
            cause: None,
            action: None,
            problem: None,
        }
    }

    /// HERE's 400 for a parameter, its name in the title and the value sent
    /// in the cause, as HERE's own validation errors read.
    fn illegal(param: &str, value: &str, action: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            format!("Illegal input for parameter '{param}'"),
        )
        .cause(format!("Actual parameter value: '{value}'"))
        .action(action)
    }

    fn cause(mut self, cause: impl Into<String>) -> Self {
        self.cause = Some(cause.into());
        self
    }

    fn action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }

    /// og's refusal in HERE's shape. og names its own parameters; the title
    /// names the HERE parameter that became it.
    fn of(problem: Problem) -> Self {
        let param = problem
            .invalid_params()
            .first()
            .map(|param| param.name.clone());
        let mut error = Self::of_parts(
            problem.status(),
            param.as_deref(),
            problem.error_code().as_str(),
            problem.detail(),
        );
        error.problem = Some(problem);
        error
    }

    /// The parts of og's Problem that HERE's body has room for, so a
    /// Problem read back from a job's results maps as a live one does.
    fn of_parts(
        status: u16,
        og_param: Option<&str>,
        error_code: &str,
        detail: Option<&str>,
    ) -> Self {
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        // A refusal that names no parameter names one by its code.
        let og_param = og_param.or(match error_code {
            "invalid_query" => Some("q"),
            "invalid_coordinate" => Some("focus"),
            "limit_too_large" => Some("limit"),
            _ => None,
        });
        let title = match og_param.map(here_param) {
            Some(param) if status == StatusCode::BAD_REQUEST => {
                format!("Illegal input for parameter '{param}'")
            }
            _ => status.canonical_reason().unwrap_or("Error").to_string(),
        };
        Self {
            status,
            title,
            cause: detail.map(str::to_string),
            action: None,
            problem: None,
        }
    }

    fn body(&self, request_id: &str) -> Value {
        let mut body = json!({
            "status": self.status.as_u16(),
            "title": self.title,
            "correlationId": request_id,
            "requestId": request_id,
        });
        if let Some(cause) = &self.cause {
            body["cause"] = cause.as_str().into();
        }
        if let Some(action) = &self.action {
            body["action"] = action.as_str().into();
        }
        body
    }

    fn into_vendor(self, request_id: &str) -> VendorError {
        let mut error = VendorError::new(self.status, self.body(request_id));
        if let Some(problem) = self.problem {
            if let Some((name, value)) = retry_after(&problem) {
                error = error.with_header(name, value);
            }
            error = error.caused_by(problem);
        }
        error
    }
}

/// The HERE parameter an og one was translated from.
fn here_param(og: &str) -> &str {
    match og {
        "q" => "q",
        "address" | "locality" | "region" | "postcode" => "qq",
        "country" | "bbox" => "in",
        "focus" | "lat" | "lon" => "at",
        "layer" => "types",
        "ids" => "id",
        other => other,
    }
}

/// A route's answer, or its HERE error.
fn respond<T: serde::Serialize>(request_id: &str, answer: Result<T, Gs7Error>) -> Response {
    match answer {
        Ok(answer) => axum::Json(answer).into_response(),
        Err(error) => error.into_vendor(request_id).into_response(),
    }
}

async fn not_implemented(Rid(request_id): Rid, OriginalUri(uri): OriginalUri) -> Response {
    Gs7Error::new(StatusCode::NOT_IMPLEMENTED, "Not Implemented")
        .cause(format!("This server does not serve {}", uri.path()))
        .into_vendor(&request_id)
        .into_response()
}

async fn method_not_allowed(Rid(request_id): Rid) -> Response {
    Gs7Error::new(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed")
        .into_vendor(&request_id)
        .into_response()
}

async fn not_found(rid: Rid, OriginalUri(uri): OriginalUri) -> Response {
    let path = uri.path();
    if path.contains("/compat/here/v7/") || path.ends_with("/compat/here/v7") {
        return batch::not_found(rid, OriginalUri(uri)).await;
    }
    Gs7Error::new(StatusCode::NOT_FOUND, "Not Found")
        .cause(format!("No endpoint at {path}"))
        .into_vendor(&rid.0)
        .into_response()
}

/// A request's query parameters. HERE refuses a parameter sent twice.
pub(super) struct Params(Vec<(String, String)>);

impl Params {
    fn parse(query: Option<&str>) -> Result<Self, Gs7Error> {
        let pairs: Vec<(String, String)> = serde_urlencoded::from_str(query.unwrap_or_default())
            .map_err(|_| {
                Gs7Error::new(StatusCode::BAD_REQUEST, "Malformed query string")
                    .cause("The query string is not URL-encoded key=value pairs")
            })?;
        for (index, (name, _)) in pairs.iter().enumerate() {
            if pairs[..index].iter().any(|(seen, _)| seen == name) {
                return Err(Gs7Error::new(
                    StatusCode::BAD_REQUEST,
                    format!("Duplicate parameter '{name}'"),
                )
                .action("Send each parameter once"));
            }
        }
        Ok(Self(pairs))
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(given, _)| given == name)
            .map(|(_, value)| value.as_str())
    }

    /// `limit`, checked against HERE's own bounds for the endpoint.
    fn limit(&self, default: usize, max: usize) -> Result<usize, Gs7Error> {
        self.int("limit", 1, max)
            .map(|limit| limit.unwrap_or(default))
    }

    fn int(&self, name: &str, min: usize, max: usize) -> Result<Option<usize>, Gs7Error> {
        let Some(value) = self.get(name) else {
            return Ok(None);
        };
        value
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|number| (min..=max).contains(number))
            .map(Some)
            .ok_or_else(|| {
                Gs7Error::illegal(
                    name,
                    value,
                    format!("Provide an integer from {min} to {max}"),
                )
            })
    }

    /// `types`, each value one the endpoint lists.
    fn types(&self, allowed: &[&str]) -> Result<Option<Vec<String>>, Gs7Error> {
        let Some(value) = self.get("types") else {
            return Ok(None);
        };
        let types = value
            .split(',')
            .map(str::trim)
            .filter(|kind| !kind.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if let Some(unknown) = types.iter().find(|kind| !allowed.contains(&kind.as_str())) {
            return Err(Gs7Error::illegal(
                "types",
                value,
                supported(unknown, allowed),
            ));
        }
        Ok(Some(types))
    }
}

fn supported(unknown: &str, allowed: &[&str]) -> String {
    let allowed = allowed
        .iter()
        .map(|value| format!("'{value}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("Unsupported value: '{unknown}'. Supported values: {allowed}")
}

/// A point, which HERE writes as `{latitude},{longitude}`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct LatLng {
    lat: f64,
    lng: f64,
}

impl LatLng {
    fn parse(param: &str, value: &str) -> Result<Self, Gs7Error> {
        let illegal = || Gs7Error::illegal(param, value, "Provide {latitude},{longitude}");
        let (lat, lng) = value.split_once(',').ok_or_else(illegal)?;
        let lat = lat.trim().parse::<f64>().map_err(|_| illegal())?;
        let lng = lng.trim().parse::<f64>().map_err(|_| illegal())?;
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
            return Err(illegal());
        }
        Ok(Self { lat, lng })
    }

    /// og's `focus` form, `lon,lat`.
    fn og_point(self) -> String {
        format!("{},{}", self.lng, self.lat)
    }
}

/// The area `in` names.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Area {
    /// ISO 3166-1 alpha-2 codes, as og's `country` takes them.
    Countries(Vec<&'static str>),
    Circle {
        center: LatLng,
        radius_m: f64,
    },
    /// `west,south,east,north`, which is og's `bbox` order too.
    Bbox([f64; 4]),
}

/// The members of HERE's `XEA` group, the European Union.
const EU: [&str; 27] = [
    "AT", "BE", "BG", "HR", "CY", "CZ", "DK", "EE", "FI", "FR", "DE", "GR", "HU", "IE", "IT", "LV",
    "LT", "LU", "MT", "NL", "PL", "PT", "RO", "SK", "SI", "ES", "SE",
];

impl Area {
    fn parse(value: &str) -> Result<Self, Gs7Error> {
        if let Some(codes) = value.strip_prefix("countryCode:") {
            let mut countries = Vec::new();
            for code in codes.split(',').map(str::trim) {
                let found: Vec<&'static str> = match code {
                    "XEA" => EU.to_vec(),
                    // HERE's own code for Kosovo, which ISO leaves to users;
                    // `XK` is the one in common use.
                    "XKO" => vec!["XK"],
                    code => iso::alpha2_of(code).into_iter().collect(),
                };
                if found.is_empty() {
                    return Err(Gs7Error::illegal(
                        "in",
                        value,
                        format!(
                            "Unsupported country code: '{code}'. Provide ISO 3166-1 alpha-3 codes"
                        ),
                    ));
                }
                for code in found {
                    if !countries.contains(&code) {
                        countries.push(code);
                    }
                }
            }
            return Ok(Self::Countries(countries));
        }
        if let Some(circle) = value.strip_prefix("circle:") {
            let illegal = || {
                Gs7Error::illegal(
                    "in",
                    value,
                    "Provide circle:{latitude},{longitude};r={radius}",
                )
            };
            let (center, radius) = circle.split_once(";r=").ok_or_else(illegal)?;
            let center = LatLng::parse("in", center).map_err(|_| illegal())?;
            let radius_m = radius
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|radius| radius.is_finite() && *radius > 0.0)
                .ok_or_else(illegal)?;
            return Ok(Self::Circle { center, radius_m });
        }
        if let Some(bbox) = value.strip_prefix("bbox:") {
            let illegal = || {
                Gs7Error::illegal(
                    "in",
                    value,
                    "Provide bbox:{west longitude},{south latitude},{east longitude},{north latitude}",
                )
            };
            let parts = bbox
                .split(',')
                .map(|part| part.trim().parse::<f64>().map_err(|_| illegal()))
                .collect::<Result<Vec<_>, _>>()?;
            let [west, south, east, north] = parts[..] else {
                return Err(illegal());
            };
            return Ok(Self::Bbox([west, south, east, north]));
        }
        Err(Gs7Error::illegal(
            "in",
            value,
            "Provide countryCode:{codes}, circle:{latitude},{longitude};r={radius} or bbox:{west},{south},{east},{north}",
        ))
    }
}

/// The `Content-Type` a body was sent as, without its parameters.
fn media_type(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::CONTENT_TYPE)?
        .to_str()
        .ok()?
        .split(';')
        .next()
        .map(|media_type| media_type.trim().to_ascii_lowercase())
}
