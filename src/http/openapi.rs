//! The published API contract: an OpenAPI 3.1 description of every route,
//! generated from the types that serve them and served at `/openapi.json`.
//!
//! Generating it from the Rust types rather than writing it by hand is what
//! keeps it true: a field cannot change shape without the spec changing with
//! it, and `contract_tests` fails when the committed
//! `contract/openapi.json` and the code disagree.
//!
//! Three things are adjusted after generation, in [`transform`]:
//!
//! 1. `Option<T>` renders as a nullable union, `["string", "null"]`, but this
//!    API never emits a null: an absent value is an absent key
//!    (`skip_serializing_if`). The nulls are dropped so the spec says what
//!    the API does.
//! 2. Every route answers a non-GET with a 405, from the router's fallbacks
//!    rather than from a handler, so there is no attribute to carry it.
//! 3. Every response gains the `X-Request-Id` header it actually returns.
//!
//! Bounds in the parameter attributes below are written as literals, because
//! the attribute macro cannot read a constant. `bounds_match_the_constants`
//! keeps them honest.

use std::sync::LazyLock;

use axum::{
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use serde_json::{Map, Value};
use utoipa::OpenApi;

use crate::{
    api::{
        Accuracy, Address, Attribution, Common, Context, ContextArea, FeatureCollectionType,
        FeatureType, ForwardCollection, ForwardFeature, ForwardGeocoding, ForwardProperties,
        LonLat, Point, PointType, ReverseCollection, ReverseFeature, ReverseGeocoding,
        ReverseProperties, Version,
    },
    record::{Layer, ObjectType},
    records::Source,
};

/// The media type every error is served as (RFC 9457).
pub(crate) const PROBLEM_JSON: &str = "application/problem+json";

/// The response header every answer carries, echoing or minting the id the
/// logs are indexed by.
const REQUEST_ID_HEADER: &str = "X-Request-Id";

#[derive(OpenApi)]
#[openapi(
    info(
        title = "open-geocode",
        description = "Forward, autocomplete and reverse geocoding of addresses, \
                       served from a Pack built from OpenStreetMap and OpenAddresses.\n\n\
                       Every response is a GeoJSON FeatureCollection. Forward results \
                       carry a `score`, and a `confidence` when the Pack is calibrated. \
                       Reverse results carry a `distance_m`.\n\n\
                       Errors are RFC 9457 problem documents (`application/problem+json`). \
                       Branch on `error_code`. `title` and `detail` are prose and may change.",
        license(name = "MIT", identifier = "MIT"),
    ),
    servers(
        (url = "http://127.0.0.1:8080",
         description = "Local server on the default address."),
    ),
    paths(
        crate::runtime::search,
        crate::runtime::autocomplete,
        crate::runtime::geocode,
        crate::runtime::reverse,
        crate::http::health::healthz,
        crate::http::health::readyz,
        openapi_json,
    ),
    components(schemas(
        ForwardCollection,
        ReverseCollection,
        ForwardFeature,
        ReverseFeature,
        ForwardProperties,
        ReverseProperties,
        Common,
        ForwardGeocoding,
        ReverseGeocoding,
        FeatureCollectionType,
        FeatureType,
        Point,
        PointType,
        LonLat,
        Version,
        Attribution,
        Accuracy,
        Address,
        Context,
        ContextArea,
        Source,
        ObjectType,
        // Only ever reached through a parameter, so it needs naming here or
        // it is left out of the components and the `$ref` dangles.
        Layer,
        Problem,
    )),
    tags(
        (name = "geocoding", description = "Geocode addresses and points."),
        (name = "operations", description = "Health checks and the OpenAPI document."),
    ),
)]
pub(crate) struct ApiDoc;

// Schema-only mirror of [`crate::http::problem::Problem`], whose fields are
// private so only its constructors build one.
// `problem_schema_matches_the_rendered_body` holds the two together.
/// Error response, an RFC 9457 problem document.
#[derive(utoipa::ToSchema)]
#[schema(as = Problem, title = "Problem")]
#[allow(dead_code)]
pub(crate) struct Problem {
    /// URI identifying the problem type. Currently always `about:blank`.
    #[schema(example = "about:blank")]
    pub r#type: String,
    /// Name of the HTTP status. Do not branch on it.
    #[schema(example = "Bad Request")]
    pub title: String,
    #[schema(example = 400, minimum = 400, maximum = 599)]
    pub status: u16,
    /// Human-readable explanation. Absent on `500` errors.
    #[schema(example = "q must not be empty")]
    pub detail: Option<String>,
    /// Stable error code. Branch on this.
    pub error_code: ErrorCode,
    /// ID the request is logged under. Include it in bug reports.
    #[schema(example = "01JA7Q2K3M4N5P6Q7R8S9T0V1W")]
    pub request_id: Option<String>,
    /// Rejected parameters. Present only when the error is about
    /// parameters.
    pub invalid_params: Option<Vec<InvalidParam>>,
}

// Mirrors [`crate::http::problem::ErrorCode`] for the schema.
/// Stable error code.
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum ErrorCode {
    /// `q` was empty, too long, or could not be parsed as a query.
    InvalidQuery,
    /// `lon` or `lat` was missing or outside its range, or `focus` or
    /// `bbox` named a point outside it.
    InvalidCoordinate,
    /// `limit` was above the maximum for the route.
    LimitTooLarge,
    /// A parameter was the wrong type, or named a value that does not exist
    /// — an unknown `layer`, for instance.
    MalformedParameter,
    /// No such route.
    NotFound,
    /// The route exists but does not take this method. The `Allow` header
    /// lists the ones it does.
    MethodNotAllowed,
    /// Something failed inside the server. The cause is in the logs under
    /// `request_id`, never in the body.
    InternalError,
}

// Mirrors [`crate::http::problem::InvalidParam`] for the schema.
/// A rejected parameter.
#[derive(utoipa::ToSchema)]
#[allow(dead_code)]
pub(crate) struct InvalidParam {
    /// Parameter name.
    #[schema(example = "limit")]
    pub name: String,
    /// Why it was rejected.
    #[schema(example = "must be at most 50")]
    pub reason: String,
}

// The same bytes as the committed `contract/openapi.json`, which the
// contract tests hold to the code.
/// Get the OpenAPI document
///
/// Returns the OpenAPI 3.1 document describing this server's API.
#[utoipa::path(
    get,
    path = "/openapi.json",
    tag = "operations",
    responses(
        (status = 200, description = "The OpenAPI document.",
         content_type = "application/json"),
    ),
)]
pub(crate) async fn openapi_json() -> Response {
    let mut response = spec().clone().into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// The spec as served, built once.
pub(crate) fn spec() -> &'static String {
    static SPEC: LazyLock<String> = LazyLock::new(|| document().expect("the spec serializes"));
    &SPEC
}

/// The spec as a canonical JSON string: the one text both `/openapi.json` and
/// `contract/openapi.json` are.
pub(crate) fn document() -> serde_json::Result<String> {
    let mut value = serde_json::to_value(ApiDoc::openapi())?;
    transform(&mut value);
    let mut text = serde_json::to_string_pretty(&value)?;
    text.push('\n');
    Ok(text)
}

/// Adjust the generated spec to describe what the API actually does.
fn transform(value: &mut Value) {
    drop_nulls(value);
    add_boundary_responses(value);
    add_request_id_header(value);
}

/// Document the 405 every route answers a non-GET with.
///
/// It comes from the router's own fallbacks rather than from a handler, so
/// there is no handler attribute to carry it and it has to be added here.
/// Every route is GET-only, including HEAD, which the router answers 405
/// rather than letting axum route it to the GET handler.
///
/// The 404 is deliberately not added per operation: it belongs to paths that
/// are not in this document at all, so hanging it off the paths that are
/// would say the wrong thing.
fn add_boundary_responses(value: &mut Value) {
    let Some(paths) = value.get_mut("paths").and_then(Value::as_object_mut) else {
        return;
    };
    for operations in paths.values_mut() {
        let Some(operations) = operations.as_object_mut() else {
            continue;
        };
        for operation in operations.values_mut() {
            let Some(responses) = operation
                .get_mut("responses")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            responses
                .entry("405")
                .or_insert_with(method_not_allowed_response);
        }
    }
}

fn method_not_allowed_response() -> Value {
    serde_json::json!({
        "description": "Method not allowed. The route accepts `GET` only.",
        "headers": {
            "Allow": {
                "description": "Methods the route accepts.",
                "schema": { "type": "string", "example": "GET" },
            },
        },
        "content": {
            PROBLEM_JSON: {
                "schema": { "$ref": "#/components/schemas/Problem" },
            },
        },
    })
}

/// Drop `null` from everywhere the generator admitted one for an `Option<T>`.
///
/// Every optional field here is `skip_serializing_if`, so it is either a
/// value or an absent key, never an explicit null. Leaving the nulls in would
/// tell a client to expect a value this API never sends.
///
/// An `Option<T>` arrives in one of two shapes, so both are handled: a type
/// union, `"type": ["string", "null"]`, for a plain field, and a
/// `oneOf: [{$ref}, {"type": "null"}]` for one whose `T` is a named schema.
fn drop_nulls(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(types)) = map.get_mut("type") {
                types.retain(|ty| ty != "null");
                if types.len() == 1 {
                    let only = types[0].clone();
                    map.insert("type".to_string(), only);
                }
            }

            if let Some(Value::Array(variants)) = map.get_mut("oneOf") {
                variants.retain(|variant| !is_null_schema(variant));
                // A `oneOf` of one is just that one: inline it, so the field
                // reads as the thing it is rather than a choice with no
                // alternative.
                if variants.len() == 1
                    && let Some(Value::Object(only)) = variants.pop()
                {
                    map.remove("oneOf");
                    for (key, item) in only {
                        map.entry(key).or_insert(item);
                    }
                }
            }

            for item in map.values_mut() {
                drop_nulls(item);
            }
        }
        Value::Array(items) => {
            for item in items {
                drop_nulls(item);
            }
        }
        _ => {}
    }
}

/// Is this schema just `{"type": "null"}`?
fn is_null_schema(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.len() == 1 && map.get("type") == Some(&Value::String("null".into())))
}

/// Document the `X-Request-Id` every response carries.
fn add_request_id_header(value: &mut Value) {
    let Some(paths) = value.get_mut("paths").and_then(Value::as_object_mut) else {
        return;
    };
    for operations in paths.values_mut() {
        let Some(operations) = operations.as_object_mut() else {
            continue;
        };
        for operation in operations.values_mut() {
            let Some(responses) = operation
                .get_mut("responses")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            for response in responses.values_mut() {
                let Some(response) = response.as_object_mut() else {
                    continue;
                };
                let headers = response
                    .entry("headers")
                    .or_insert_with(|| Value::Object(Map::new()));
                let Some(headers) = headers.as_object_mut() else {
                    continue;
                };
                headers.insert(REQUEST_ID_HEADER.to_string(), request_id_header());
            }
        }
    }
}

fn request_id_header() -> Value {
    serde_json::json!({
        "description": "ID the request is logged under: the request's own \
                        `X-Request-Id` when it sent one. Include it in bug reports.",
        "schema": { "type": "string", "maxLength": 128 },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        http::{
            bounds::{MAX_QUERY_CHARS, MAX_SEARCH_LIMIT},
            problem::{ErrorCode as RuntimeErrorCode, Problem as RuntimeProblem},
        },
        search::MAX_AUTOCOMPLETE_LIMIT,
    };

    fn parameter(spec: &Value, path: &str, name: &str) -> Value {
        spec["paths"][path]["get"]["parameters"]
            .as_array()
            .expect("parameters")
            .iter()
            .find(|parameter| parameter["name"] == name)
            .unwrap_or_else(|| panic!("{path} has no {name} parameter"))
            .clone()
    }

    /// The bounds in the `#[utoipa::path]` attributes are literals, because
    /// the macro cannot read a constant. This is what stops them drifting
    /// away from the constants the handlers actually enforce.
    #[test]
    fn bounds_match_the_constants() {
        let spec: Value = serde_json::from_str(&document().expect("spec")).expect("JSON");

        for path in ["/search", "/autocomplete"] {
            assert_eq!(
                parameter(&spec, path, "q")["schema"]["maxLength"],
                Value::from(MAX_QUERY_CHARS),
                "{path} documents the wrong maximum query length"
            );
        }
        assert_eq!(
            parameter(&spec, "/search", "limit")["schema"]["maximum"],
            Value::from(MAX_SEARCH_LIMIT),
        );
        assert_eq!(
            parameter(&spec, "/autocomplete", "limit")["schema"]["maximum"],
            Value::from(MAX_AUTOCOMPLETE_LIMIT),
        );

        // Compared as numbers: an integral bound serializes without its
        // fractional part, so `-180.0` is written `-180`.
        let degrees = |parameter: &Value, bound: &str| {
            parameter["schema"][bound]
                .as_f64()
                .unwrap_or_else(|| panic!("{bound} is not a number"))
        };
        let lon = parameter(&spec, "/reverse", "lon");
        assert_eq!(degrees(&lon, "minimum"), -180.0);
        assert_eq!(degrees(&lon, "maximum"), 180.0);
        let lat = parameter(&spec, "/reverse", "lat");
        assert_eq!(degrees(&lat, "minimum"), -90.0);
        assert_eq!(degrees(&lat, "maximum"), 90.0);
    }

    /// [`Problem`] here describes the body, and `problem::Problem` renders
    /// it. They are separate types, so this holds their field names together:
    /// a member renamed on one side without the other would publish a
    /// contract the server does not honour.
    #[test]
    fn problem_schema_matches_the_rendered_body() {
        let rendered = serde_json::to_value(
            RuntimeProblem::limit_too_large(vec![crate::http::problem::InvalidParam::new(
                "limit",
                "must be at most 50",
            )])
            .with_request_id("contract-test"),
        )
        .expect("problem serializes");
        let rendered: std::collections::BTreeSet<&str> = rendered
            .as_object()
            .expect("problem is an object")
            .keys()
            .map(String::as_str)
            .collect();

        let spec: Value = serde_json::from_str(&document().expect("spec")).expect("JSON");
        let documented: std::collections::BTreeSet<&str> = spec["components"]["schemas"]["Problem"]
            ["properties"]
            .as_object()
            .expect("Problem properties")
            .keys()
            .map(String::as_str)
            .collect();

        let undocumented: Vec<_> = rendered.difference(&documented).collect();
        assert!(
            undocumented.is_empty(),
            "the server sends members the contract does not describe: {undocumented:?}"
        );
        // `detail` is absent from this particular problem but documented, so
        // only check that everything documented could be sent.
        assert!(
            documented.contains("detail"),
            "the contract should still document `detail`"
        );
    }

    /// Every error the server can construct is a value the contract lists.
    #[test]
    fn every_error_code_is_in_the_contract() {
        let spec: Value = serde_json::from_str(&document().expect("spec")).expect("JSON");
        let documented: std::collections::BTreeSet<String> =
            spec["components"]["schemas"]["ErrorCode"]["enum"]
                .as_array()
                .expect("ErrorCode enum")
                .iter()
                .map(|value| value.as_str().expect("string").to_string())
                .collect();

        for code in [
            RuntimeErrorCode::InvalidQuery,
            RuntimeErrorCode::InvalidCoordinate,
            RuntimeErrorCode::LimitTooLarge,
            RuntimeErrorCode::MalformedParameter,
            RuntimeErrorCode::NotFound,
            RuntimeErrorCode::MethodNotAllowed,
            RuntimeErrorCode::InternalError,
        ] {
            assert!(
                documented.contains(code.as_str()),
                "{} is not in the published contract",
                code.as_str()
            );
        }
    }
}
