//! Geocoding and Search v7: `/v1/geocode`, `/v1/revgeocode`,
//! `/v1/multi-revgeocode`, `/v1/autosuggest`, `/v1/autocomplete`,
//! `/v1/discover` and `/v1/lookup`.

use axum::{
    body::Bytes,
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde::Serialize;

use crate::{
    api::{ForwardCollection, ReverseCollection},
    compat::{Args, decode_id, native},
    http::{bounds::MAX_SEARCH_LIMIT, problem::Problem},
    runtime::AppState,
};

use super::{
    Area, Gs7Error, LatLng, Params, Rid,
    item::{Feature, Item, item},
    media_type, respond,
};

/// The most suggestions og's `/autocomplete` gives in one answer.
const MAX_AUTOCOMPLETE_LIMIT: usize = 20;

const GEOCODE_TYPES: [&str; 7] = [
    "address",
    "area",
    "city",
    "houseNumber",
    "place",
    "postalCode",
    "street",
];
const REVGEOCODE_TYPES: [&str; 6] = ["address", "area", "city", "houseNumber", "place", "street"];
const AUTOCOMPLETE_TYPES: [&str; 6] = [
    "address",
    "area",
    "city",
    "houseNumber",
    "postalCode",
    "street",
];

#[derive(Debug, Serialize)]
pub(super) struct Items {
    pub(super) items: Vec<Item>,
}

/// An autosuggest or discover answer, paged when the request set `offset`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    items: Vec<Item>,
    /// Required on autosuggest, absent from discover. og suggests no
    /// refinements of the query's words, so it is always empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    query_terms: Option<[(); 0]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<usize>,
}

fn features(collection: &ForwardCollection) -> Result<Vec<Feature>, Gs7Error> {
    collection.features.iter().map(feature).collect()
}

fn reverse_features(collection: &ReverseCollection) -> Result<Vec<Feature>, Gs7Error> {
    collection.features.iter().map(feature).collect()
}

fn feature(feature: &impl Serialize) -> Result<Feature, Gs7Error> {
    Feature::of(feature).map_err(|error| Gs7Error::of(Problem::internal(error.into())))
}

fn args(fields: Vec<(&'static str, String)>) -> Args {
    fields
        .into_iter()
        .fold(Args::new(), |args, (name, value)| args.set(name, value))
}

/// HERE's qualified query `qq`, written `{sub-parameter}={string}[;...]`.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Qualified {
    pub(super) country: Option<String>,
    pub(super) state: Option<String>,
    pub(super) county: Option<String>,
    pub(super) city: Option<String>,
    pub(super) district: Option<String>,
    pub(super) street: Option<String>,
    pub(super) house_number: Option<String>,
    pub(super) postal_code: Option<String>,
}

impl Qualified {
    const PARTS: [&str; 8] = [
        "country",
        "state",
        "county",
        "city",
        "district",
        "street",
        "houseNumber",
        "postalCode",
    ];

    pub(super) fn parse(value: &str) -> Result<Self, Gs7Error> {
        let mut qualified = Self::default();
        for part in value.split(';').filter(|part| !part.trim().is_empty()) {
            let Some((name, text)) = part.split_once('=') else {
                return Err(Gs7Error::illegal(
                    "qq",
                    value,
                    "Provide {sub-parameter}={string}[;{sub-parameter}={string}]*",
                ));
            };
            let text = Some(text.trim().to_string()).filter(|text| !text.is_empty());
            let slot = match name.trim() {
                "country" => &mut qualified.country,
                "state" => &mut qualified.state,
                "county" => &mut qualified.county,
                "city" => &mut qualified.city,
                "district" => &mut qualified.district,
                "street" => &mut qualified.street,
                "houseNumber" => &mut qualified.house_number,
                "postalCode" => &mut qualified.postal_code,
                unknown => {
                    return Err(Gs7Error::illegal(
                        "qq",
                        value,
                        super::supported(unknown, &Self::PARTS),
                    ));
                }
            };
            *slot = text;
        }
        Ok(qualified)
    }

    /// The country as an og filter, when it is given as a code. A country
    /// given by name is searched as text instead.
    fn country_code(&self) -> Option<&'static str> {
        let country = self.country.as_deref()?.trim();
        match country.len() {
            2 => crate::compat::iso::alpha3(&country.to_ascii_uppercase())
                .and_then(crate::compat::iso::alpha2_of),
            3 => crate::compat::iso::alpha2_of(country),
            _ => None,
        }
    }
}

/// A forward geocode as HERE asks it, free text, qualified or both.
#[derive(Debug, Default)]
pub(super) struct Geocode {
    pub(super) q: Option<String>,
    pub(super) qualified: Qualified,
    pub(super) countries: Vec<&'static str>,
    pub(super) at: Option<LatLng>,
    pub(super) limit: usize,
}

impl Geocode {
    /// og's `/geocode` parameters, or `None` when nothing is asked, with blank
    /// text and no qualified part.
    ///
    /// A street with no free text is og's structured geocode, the address
    /// fields apart. Anything else is one text. og's structured form has no
    /// field for a district or a county, and text loses none of them.
    pub(super) fn og_fields(&self) -> Option<Vec<(&'static str, String)>> {
        let qq = &self.qualified;
        let q = self.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
        let mut countries = self.countries.clone();
        let code = qq.country_code();
        if let Some(code) = code
            && !countries.contains(&code)
        {
            countries.push(code);
        }
        let mut fields = Vec::new();
        let number_and_street = [qq.house_number.as_deref(), qq.street.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        if q.is_none() && qq.street.is_some() {
            fields.push(("address", number_and_street));
            let parts = [
                ("locality", &qq.city),
                ("region", &qq.state),
                ("postcode", &qq.postal_code),
            ];
            for (name, value) in parts {
                if let Some(value) = value {
                    fields.push((name, value.clone()));
                }
            }
        } else {
            let country_name = qq.country.as_deref().filter(|_| code.is_none());
            let text = [
                q,
                Some(number_and_street.as_str()),
                qq.district.as_deref(),
                qq.city.as_deref(),
                qq.county.as_deref(),
                qq.state.as_deref(),
                qq.postal_code.as_deref(),
                country_name,
            ]
            .into_iter()
            .flatten()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join(", ");
            if text.is_empty() {
                return None;
            }
            fields.push(("q", text));
            if let Some(at) = self.at {
                fields.push(("focus", at.og_point()));
            }
        }
        if !countries.is_empty() {
            fields.push(("country", countries.join(",")));
        }
        fields.push(("limit", self.limit.min(MAX_SEARCH_LIMIT).to_string()));
        Some(fields)
    }
}

pub(super) async fn geocode(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
) -> Response {
    respond(&request_id, geocode_answer(&state, query.as_deref()).await)
}

async fn geocode_answer(state: &AppState, query: Option<&str>) -> Result<Items, Gs7Error> {
    let params = Params::parse(query)?;
    let qualified = params
        .get("qq")
        .map(Qualified::parse)
        .transpose()?
        .unwrap_or_default();
    if params.get("q").is_none() && params.get("qq").is_none() {
        return Err(
            Gs7Error::new(StatusCode::BAD_REQUEST, "Missing parameter 'q' or 'qq'")
                .action("Provide 'q', 'qq' or both"),
        );
    }
    let countries = match params.get("in").map(Area::parse).transpose()? {
        None => Vec::new(),
        Some(Area::Countries(countries)) => countries,
        Some(_) => {
            return Err(Gs7Error::illegal(
                "in",
                params.get("in").unwrap_or_default(),
                "Provide countryCode:{countryCode}[,{countryCode}]*",
            ));
        }
    };
    let at = params
        .get("at")
        .map(|at| LatLng::parse("at", at))
        .transpose()?;
    let limit = params.limit(20, 100)?;
    let types = params.types(&GEOCODE_TYPES)?;
    let request = Geocode {
        q: params.get("q").map(str::to_string),
        qualified,
        countries,
        at,
        // A `types` filter drops some of what og finds, so og is asked for
        // all it may give.
        limit: if types.is_some() {
            MAX_SEARCH_LIMIT
        } else {
            limit
        },
    };
    let Some(fields) = request.og_fields() else {
        return Ok(Items { items: Vec::new() });
    };
    let collection = native::geocode(state, args(fields))
        .await
        .map_err(Gs7Error::of)?;
    let items = features(&collection)?
        .iter()
        .map(|feature| item(feature).distance_from(at))
        .filter(|item| item.is_any_of(types.as_deref()))
        .take(limit)
        .collect();
    Ok(Items { items })
}

/// A reverse geocode as HERE asks it.
#[derive(Debug)]
pub(super) struct Reverse {
    pub(super) center: LatLng,
    /// `in=circle`'s radius, a hard filter on the distance.
    pub(super) radius_m: Option<f64>,
    /// og's `layer` when HERE's `types` or `bearing` asks for one kind only.
    pub(super) layer: Option<&'static str>,
    pub(super) types: Option<Vec<String>>,
    pub(super) limit: usize,
}

impl Reverse {
    fn read(params: &Params, max_limit: usize) -> Result<Self, Gs7Error> {
        let (center, radius_m) = location(params.get("at"), params.get("in"))?;
        let types = params.types(&REVGEOCODE_TYPES)?;
        let bearing = params.int("bearing", 0, 359)?;
        if bearing.is_some()
            && let Some(types) = &types
            && types.iter().any(|kind| kind != "street")
        {
            return Err(Gs7Error::illegal(
                "types",
                params.get("types").unwrap_or_default(),
                "With 'bearing', the only accepted value is 'street'",
            ));
        }
        // og has no heading to match, but HERE's bearing answers streets
        // only, and that og can do.
        let layer = match (bearing, types.as_deref()) {
            (Some(_), _) => Some("street"),
            (None, Some([kind])) => match kind.as_str() {
                "street" => Some("street"),
                "houseNumber" => Some("address"),
                "place" => Some("poi"),
                "city" => Some("locality"),
                _ => None,
            },
            _ => None,
        };
        Ok(Self {
            center,
            radius_m,
            layer,
            types: if bearing.is_some() {
                Some(vec!["street".to_string()])
            } else {
                types
            },
            limit: params.limit(1, max_limit)?,
        })
    }

    pub(super) fn og_fields(&self) -> Vec<(&'static str, String)> {
        let mut fields = vec![
            ("lon", self.center.lng.to_string()),
            ("lat", self.center.lat.to_string()),
        ];
        if let Some(layer) = self.layer {
            fields.push(("layer", layer.to_string()));
        }
        fields
    }
}

/// og's reverse answer as HERE's items. An area og finds the point in has
/// no distance; it lies at the point, inside any circle.
pub(super) fn reverse_items(
    features: &[Feature],
    radius_m: Option<f64>,
    types: Option<&[String]>,
    limit: usize,
) -> Vec<Item> {
    features
        .iter()
        .map(|feature| item(feature).without_scoring())
        .filter(|item| {
            radius_m.is_none_or(|radius| {
                item.distance
                    .is_none_or(|distance| distance as f64 <= radius)
            })
        })
        .filter(|item| item.is_any_of(types))
        .take(limit)
        .collect()
}

/// The point to reverse geocode, from exactly one of `at` or `in=circle`.
fn location(at: Option<&str>, area: Option<&str>) -> Result<(LatLng, Option<f64>), Gs7Error> {
    match (at, area) {
        (Some(at), None) => Ok((LatLng::parse("at", at)?, None)),
        (None, Some(value)) => match Area::parse(value)? {
            Area::Circle { center, radius_m } => Ok((center, Some(radius_m))),
            _ => Err(Gs7Error::illegal(
                "in",
                value,
                "Provide circle:{latitude},{longitude};r={radius}",
            )),
        },
        (Some(_), Some(_)) => Err(Gs7Error::new(
            StatusCode::BAD_REQUEST,
            "Parameters 'at' and 'in' are mutually exclusive",
        )
        .action("Provide either 'at' or 'in=circle', not both")),
        (None, None) => Err(Gs7Error::new(
            StatusCode::BAD_REQUEST,
            "Missing parameter 'at' or 'in'",
        )
        .action("Provide 'at' or 'in=circle'")),
    }
}

pub(super) async fn revgeocode(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
) -> Response {
    let answer = async {
        let params = Params::parse(query.as_deref())?;
        let request = Reverse::read(&params, 100)?;
        reverse_lookup(&state, &request).await
    };
    respond(&request_id, answer.await.map(|items| Items { items }))
}

async fn reverse_lookup(state: &AppState, request: &Reverse) -> Result<Vec<Item>, Gs7Error> {
    let collection = native::reverse(state, args(request.og_fields()))
        .await
        .map_err(Gs7Error::of)?;
    Ok(reverse_items(
        &reverse_features(&collection)?,
        request.radius_m,
        request.types.as_deref(),
        request.limit,
    ))
}

/// One answer of a multi reverse geocode, its items or why it has none.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum MultiResult {
    Items { id: String, items: Vec<Item> },
    Error { id: String, error: MultiError },
}

#[derive(Debug, Serialize)]
struct Multi {
    results: Vec<MultiResult>,
}

#[derive(Debug, Serialize)]
struct MultiError {
    title: String,
}

/// Sub-requests one call may carry.
const MAX_MULTI: usize = 100;

pub(super) async fn multi_revgeocode(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let answer = async {
        let accepted = media_type(&headers).is_some_and(|media_type| {
            media_type == "text/plain" || media_type == "application/x-www-form-urlencoded"
        });
        if !accepted {
            return Err(Gs7Error::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Unsupported Media Type",
            )
            .action("Send the body as text/plain or application/x-www-form-urlencoded"));
        }
        let shared = Params::parse(query.as_deref())?;
        let limit = shared.limit(1, 10)?;
        let body = std::str::from_utf8(&body).map_err(|_| {
            Gs7Error::new(StatusCode::BAD_REQUEST, "Malformed request body")
                .cause("The body is not UTF-8")
        })?;
        let lines = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        if lines.is_empty() {
            return Err(Gs7Error::new(StatusCode::BAD_REQUEST, "Empty request body")
                .action("Send one sub-request per line, such as id=1&at=50.11,8.68"));
        }
        if lines.len() > MAX_MULTI {
            return Err(Gs7Error::new(
                StatusCode::BAD_REQUEST,
                format!("Too many sub-requests: {}", lines.len()),
            )
            .action(format!("Send at most {MAX_MULTI} sub-requests per call")));
        }
        let mut results: Vec<MultiResult> = Vec::with_capacity(lines.len());
        for line in lines {
            let params = Params::parse(Some(line))?;
            let id = sub_request_id(&params, line)?;
            // The first of two sub-requests with one id is the one answered.
            if results.iter().any(|result| match result {
                MultiResult::Items { id: seen, .. } | MultiResult::Error { id: seen, .. } => {
                    *seen == id
                }
            }) {
                continue;
            }
            let answer = match Reverse::read(&params, 10) {
                Ok(mut request) => {
                    request.limit = limit;
                    reverse_lookup(&state, &request).await
                }
                Err(error) => Err(error),
            };
            results.push(match answer {
                Ok(items) => MultiResult::Items { id, items },
                Err(error) => MultiResult::Error {
                    id,
                    error: MultiError { title: error.title },
                },
            });
        }
        Ok(Multi { results })
    };
    respond(&request_id, answer.await)
}

/// A sub-request's `id`, at most 64 of `[A-Za-z0-9_-]`.
fn sub_request_id(params: &Params, line: &str) -> Result<String, Gs7Error> {
    let id = params.get("id").unwrap_or_default();
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if valid {
        Ok(id.to_string())
    } else {
        Err(Gs7Error::illegal(
            "id",
            line,
            "Give every sub-request an id of at most 64 letters, digits, '_' or '-'",
        ))
    }
}

/// Where autosuggest and discover search, from `at`, `in=circle` or
/// `in=bbox`, and `in=countryCode` beside `at`.
#[derive(Debug, Default)]
struct Near {
    center: Option<LatLng>,
    radius_m: Option<f64>,
    fields: Vec<(&'static str, String)>,
}

impl Near {
    fn read(params: &Params, required: bool) -> Result<Self, Gs7Error> {
        let at = params
            .get("at")
            .map(|at| LatLng::parse("at", at))
            .transpose()?;
        let area = params.get("in").map(Area::parse).transpose()?;
        let mut near = Self {
            center: at,
            ..Self::default()
        };
        match area {
            Some(Area::Countries(countries)) => {
                if required && at.is_none() {
                    return Err(missing_location());
                }
                near.fields.push(("country", countries.join(",")));
            }
            Some(_) if at.is_some() => {
                return Err(Gs7Error::new(
                    StatusCode::BAD_REQUEST,
                    "Parameters 'at', 'in=circle' and 'in=bbox' are mutually exclusive",
                )
                .action("Provide only one of them"));
            }
            Some(Area::Circle { center, radius_m }) => {
                near.center = Some(center);
                near.radius_m = Some(radius_m);
                // The circle's bounding box narrows og's search; the
                // distance check on the answer makes it the circle.
                near.fields.push(("bbox", circle_bbox(center, radius_m)));
            }
            Some(Area::Bbox([west, south, east, north])) => {
                near.fields
                    .push(("bbox", format!("{west},{south},{east},{north}")));
            }
            None if required && at.is_none() => return Err(missing_location()),
            None => {}
        }
        if let Some(center) = near.center {
            near.fields.push(("focus", center.og_point()));
        }
        Ok(near)
    }

    fn admits(&self, item: &Item) -> bool {
        self.radius_m.is_none_or(|radius| {
            item.distance
                .is_some_and(|distance| distance as f64 <= radius)
        })
    }
}

fn missing_location() -> Gs7Error {
    Gs7Error::new(
        StatusCode::BAD_REQUEST,
        "Missing parameter 'at', 'in=circle' or 'in=bbox'",
    )
    .action("Provide one of 'at', 'in=circle' or 'in=bbox'; 'in=countryCode' needs 'at' beside it")
}

/// The box around a circle, in og's `min_lon,min_lat,max_lon,max_lat`,
/// clamped to the globe. og's box cannot cross the antimeridian.
fn circle_bbox(center: LatLng, radius_m: f64) -> String {
    const METERS_PER_DEGREE: f64 = 111_320.0;
    let lat_span = radius_m / METERS_PER_DEGREE;
    let cos = center.lat.to_radians().cos().max(1e-6);
    let lon_span = (radius_m / (METERS_PER_DEGREE * cos)).min(360.0);
    let min_lat = (center.lat - lat_span).max(-90.0);
    let max_lat = (center.lat + lat_span).min(90.0);
    let min_lon = (center.lng - lon_span).max(-180.0);
    let max_lon = (center.lng + lon_span).min(180.0);
    format!("{min_lon},{min_lat},{max_lon},{max_lat}")
}

/// `offset` paging over what og answered. Answers the page, and the paging
/// fields HERE sends only when `offset` was asked.
fn page(items: Vec<Item>, offset: Option<usize>, limit: usize, query_terms: bool) -> Page {
    let start = offset.unwrap_or(0);
    let total = items.len();
    let items = items
        .into_iter()
        .skip(start)
        .take(limit)
        .collect::<Vec<_>>();
    let count = items.len();
    Page {
        items,
        query_terms: query_terms.then_some([]),
        offset,
        next_offset: offset
            .filter(|_| total > start + limit)
            .map(|_| start + limit),
        count: offset.map(|_| count),
        limit: offset.map(|_| limit),
    }
}

/// How many results og is asked for. That is a page past `offset`, and one
/// more to tell whether another page follows.
fn fetch(offset: Option<usize>, limit: usize, max: usize) -> usize {
    match offset {
        Some(offset) => offset + limit + 1,
        None => limit,
    }
    .min(max)
}

pub(super) async fn autosuggest(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
) -> Response {
    let answer = async {
        let params = Params::parse(query.as_deref())?;
        let q = required_q(&params)?;
        let near = Near::read(&params, true)?;
        let limit = params.limit(20, 100)?;
        let offset = params.int("offset", 0, 99)?;
        if q.trim().is_empty() {
            return Ok(page(Vec::new(), offset, limit, true));
        }
        let mut fields = vec![
            ("q", q.to_string()),
            (
                "limit",
                fetch(offset, limit, MAX_AUTOCOMPLETE_LIMIT).to_string(),
            ),
        ];
        fields.extend(near.fields.iter().cloned());
        let collection = native::autocomplete(&state, args(fields))
            .await
            .map_err(Gs7Error::of)?;
        let items = features(&collection)?
            .iter()
            .map(|feature| item(feature).without_scoring().distance_from(near.center))
            .filter(|item| near.admits(item))
            .collect();
        Ok(page(items, offset, limit, true))
    };
    respond(&request_id, answer.await)
}

pub(super) async fn autocomplete(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
) -> Response {
    let answer = async {
        let params = Params::parse(query.as_deref())?;
        let q = required_q(&params)?;
        let near = Near::read(&params, false)?;
        let limit = params.limit(5, 20)?;
        let types = params.types(&AUTOCOMPLETE_TYPES)?;
        if q.trim().is_empty() {
            return Ok(Items { items: Vec::new() });
        }
        // og suggests places and addresses in one list; HERE's autocomplete
        // is addresses alone, so og is asked for all it gives and the places
        // are dropped.
        let mut fields = vec![
            ("q", q.to_string()),
            ("limit", MAX_AUTOCOMPLETE_LIMIT.to_string()),
        ];
        fields.extend(near.fields.iter().cloned());
        let collection = native::autocomplete(&state, args(fields))
            .await
            .map_err(Gs7Error::of)?;
        let items = features(&collection)?
            .iter()
            .map(|feature| item(feature).without_scoring().distance_from(near.center))
            .filter(|item| near.admits(item))
            .filter(|item| item.result_type.is_some_and(|kind| kind != "place"))
            .filter(|item| item.is_any_of(types.as_deref()))
            .take(limit)
            .map(Item::without_position)
            .collect();
        Ok(Items { items })
    };
    respond(&request_id, answer.await)
}

pub(super) async fn discover(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
) -> Response {
    let answer = async {
        let params = Params::parse(query.as_deref())?;
        let q = required_q(&params)?;
        let near = Near::read(&params, true)?;
        let limit = params.limit(20, 100)?;
        let offset = params.int("offset", 0, 99)?;
        if q.trim().is_empty() {
            return Ok(page(Vec::new(), offset, limit, false));
        }
        let mut fields = vec![
            ("q", q.to_string()),
            ("limit", fetch(offset, limit, MAX_SEARCH_LIMIT).to_string()),
        ];
        fields.extend(near.fields.iter().cloned());
        let collection = native::places(&state, args(fields))
            .await
            .map_err(Gs7Error::of)?;
        let items = features(&collection)?
            .iter()
            .map(|feature| item(feature).without_scoring().distance_from(near.center))
            .filter(|item| near.admits(item))
            .collect();
        Ok(page(items, offset, limit, false))
    };
    respond(&request_id, answer.await)
}

fn required_q(params: &Params) -> Result<&str, Gs7Error> {
    params.get("q").ok_or_else(|| {
        Gs7Error::new(StatusCode::BAD_REQUEST, "Missing parameter 'q'").action("Provide 'q'")
    })
}

pub(super) async fn lookup(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
) -> Response {
    let answer = async {
        let params = Params::parse(query.as_deref())?;
        let id = params.get("id").ok_or_else(|| {
            Gs7Error::new(StatusCode::BAD_REQUEST, "Missing parameter 'id'").action("Provide 'id'")
        })?;
        let not_found = || {
            Gs7Error::new(StatusCode::NOT_FOUND, "Requested id could not be found")
                .cause(format!("Actual parameter value: '{id}'"))
        };
        let og_id = decode_id(id).ok_or_else(not_found)?;
        let feature = native::by_id(&state, &og_id, None)
            .await
            .map_err(Gs7Error::of)?
            .ok_or_else(not_found)?;
        Ok(item(&self::feature(&feature)?).without_scoring())
    };
    respond(&request_id, answer.await)
}
