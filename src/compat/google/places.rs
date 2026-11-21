//! Text Search, Nearby Search, Autocomplete and Place Details of Places API
//! (New) v1.

use axum::{
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::HeaderMap,
    response::Response,
};
use serde::{Deserialize, Serialize};

use super::{
    Area, Code, LatLng, QueryParams, body, both_shapes, error, field_mask, from_problem, invalid,
    language::{Language, accept_language},
    model::{Kind, Place, after_name, categories_of},
    proto_int, respond, run,
};
use crate::{
    api::ForwardFeature,
    compat::{
        Args, VendorError, decode_id, native,
        route::{self, Corridor},
    },
    runtime::AppState,
    spatial_index::haversine_m,
};

/// Google's own cap on one page of Text Search results.
const MAX_PAGE_SIZE: i64 = 20;

/// Google's cap on suggestions in one Autocomplete answer.
const MAX_SUGGESTIONS: usize = 5;

const METERS_PER_DEGREE: f64 = 111_320.0;

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SearchTextRequest {
    #[serde(alias = "text_query")]
    text_query: Option<String>,
    #[serde(alias = "page_size", deserialize_with = "proto_int")]
    page_size: Option<i64>,
    #[serde(alias = "max_result_count", deserialize_with = "proto_int")]
    max_result_count: Option<i64>,
    #[serde(alias = "location_bias")]
    location_bias: Option<Area>,
    #[serde(alias = "location_restriction")]
    location_restriction: Option<Area>,
    #[serde(alias = "language_code")]
    language_code: Option<String>,
    #[serde(alias = "included_type")]
    included_type: Option<String>,
    #[serde(alias = "strict_type_filtering")]
    strict_type_filtering: bool,
    #[serde(alias = "rank_preference")]
    rank_preference: Option<String>,
    #[serde(alias = "open_now")]
    open_now: bool,
    #[serde(alias = "min_rating")]
    min_rating: Option<f64>,
    #[serde(alias = "price_levels")]
    price_levels: Vec<String>,
    #[serde(alias = "page_token")]
    page_token: Option<String>,
    #[serde(alias = "ev_options")]
    ev_options: Option<serde_json::Value>,
    #[serde(alias = "search_along_route_parameters")]
    search_along_route_parameters: Option<serde_json::Value>,
}

impl SearchTextRequest {
    /// Refuses what og cannot answer either way. Whether a place is open
    /// now needs its local time, which the Pack does not hold.
    fn refuse_unheld(&self) -> Result<(), VendorError> {
        let unheld = |what: &str| invalid(format!("{what} is not supported by this server."));
        if self.open_now {
            return Err(unheld("openNow"));
        }
        // og answers in one page and so never issues a token.
        if let Some(token) = self.page_token.as_deref().filter(|token| !token.is_empty()) {
            return Err(invalid(format!("Invalid pageToken \"{token}\".")));
        }
        Ok(())
    }

    /// Whether the request keeps only places of a rating, a price or EV
    /// charging options. og holds none of these for any place, so no place
    /// it holds is one of them.
    /// The route `searchAlongRouteParameters` names, as a corridor of
    /// [`ALONG_ROUTE_M`] either side of it.
    fn corridor(&self) -> Result<Option<Corridor>, VendorError> {
        let Some(parameters) = &self.search_along_route_parameters else {
            return Ok(None);
        };
        let encoded = parameters["polyline"]["encodedPolyline"]
            .as_str()
            .ok_or_else(|| {
                invalid("searchAlongRouteParameters.polyline.encodedPolyline is required")
            })?;
        let points = route::google_polyline(encoded).map_err(|_| {
            invalid("searchAlongRouteParameters.polyline.encodedPolyline does not decode")
        })?;
        Corridor::new(points, ALONG_ROUTE_M)
            .map(Some)
            .ok_or_else(|| {
                invalid("searchAlongRouteParameters.polyline holds fewer than two points")
            })
    }

    fn keeps_only_unheld(&self) -> bool {
        self.min_rating.is_some_and(|rating| rating > 0.0)
            || !self.price_levels.is_empty()
            || self.ev_options.is_some()
    }
}

/// How far from a route a place along it may lie: a place a kilometre off
/// the road is a short detour, and one further is no longer on the way.
const ALONG_ROUTE_M: f64 = 1_000.0;

/// A Text Search `rankPreference`. og ranks by relevance, and by distance
/// only from a point the request gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextRank {
    Relevance,
    Distance,
}

impl TextRank {
    fn of(named: Option<&str>, focus: Option<&str>) -> Result<Self, VendorError> {
        match named {
            None | Some("" | "RANK_PREFERENCE_UNSPECIFIED" | "RELEVANCE") => Ok(Self::Relevance),
            Some("DISTANCE") if focus.is_some() => Ok(Self::Distance),
            Some("DISTANCE") => Err(invalid(
                "rankPreference DISTANCE needs a locationBias on this server.",
            )),
            Some(other) => Err(invalid(format!(
                "Invalid value at 'rank_preference' (type.googleapis.com/google.maps.places.v1.SearchTextRequest.RankPreference), \"{other}\""
            ))),
        }
    }
}

#[derive(Debug, Serialize)]
struct SearchTextResponse {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    places: Vec<Place>,
}

pub(super) async fn search_text(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    bytes: Bytes,
) -> Response {
    run(async {
        let query = QueryParams::parse(query.as_deref())?;
        let mask = field_mask(&headers, &query);
        let request: SearchTextRequest = body(&bytes)?;
        let text = request
            .text_query
            .clone()
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| invalid("textQuery is required"))?;
        request.refuse_unheld()?;
        let limit = page_size(request.page_size.or(request.max_result_count))?;
        let (focus, bbox) = bias_and_restriction(
            request.location_bias.as_ref(),
            request.location_restriction.as_ref(),
        )?;
        let rank = TextRank::of(request.rank_preference.as_deref(), focus.as_deref())?;
        let language = places_language(&state, request.language_code.clone())?;
        if request.keeps_only_unheld() {
            return Ok(respond(
                &SearchTextResponse { places: Vec::new() },
                mask.as_ref(),
            ));
        }
        let included = request
            .included_type
            .as_deref()
            .filter(|named| !named.is_empty());
        // A type filter or a reordering reads more than the page holds.
        let corridor = request.corridor()?;
        let window = match (included, rank, &corridor) {
            (None, TextRank::Relevance, None) => limit,
            _ => i64::try_from(NEARBY_WINDOW).unwrap_or(limit),
        };
        // Along a route og searches the box around it, unless the request
        // restricts the search to a box of its own.
        let bbox = bbox.or_else(|| {
            corridor.as_ref().map(|corridor| {
                let [west, south, east, north] = corridor.bbox();
                format!("{west},{south},{east},{north}")
            })
        });
        let args = Args::new()
            .set("q", text)
            .set("limit", window.to_string())
            .maybe("focus", focus)
            .maybe("bbox", bbox)
            .maybe("lang", language.code());
        let found = native::search(&state, args).await.map_err(from_problem)?;
        let mut features = found.features.iter().collect::<Vec<_>>();
        if let Some(named) = included {
            let is = |feature: &&ForwardFeature| {
                Kind::of(&feature.properties.common)
                    .types()
                    .contains(&named)
            };
            match request.strict_type_filtering {
                true => features.retain(is),
                // Without strict filtering the type biases: places of it
                // first, the rest after in their order.
                false => features.sort_by_key(|feature| !is(feature)),
            }
        }
        if let Some(corridor) = &corridor {
            // Along a route, the places on the way, the shortest detour first.
            let detour = |feature: &ForwardFeature| {
                let [lon, lat] = feature.geometry.coordinates;
                corridor.reach([lon, lat]).map(|reach| reach.off_m)
            };
            features.retain(|feature| detour(feature).is_some());
            features.sort_by(|a, b| {
                detour(a)
                    .partial_cmp(&detour(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        if rank == TextRank::Distance {
            features.sort_by(|a, b| {
                let distance = |feature: &&ForwardFeature| feature.properties.distance_m;
                distance(a)
                    .partial_cmp(&distance(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        let answer = SearchTextResponse {
            places: features
                .into_iter()
                .take(usize::try_from(limit).unwrap_or_default())
                .map(|feature| place(feature, &language))
                .collect(),
        };
        Ok(respond(&answer, mask.as_ref()))
    })
    .await
}

/// Google's cap on one Nearby Search answer, and its default.
const MAX_NEARBY: i64 = 20;
/// The widest circle Nearby Search takes, in metres.
const MAX_NEARBY_RADIUS: f64 = 50_000.0;
/// The most types one list of a Nearby Search request holds.
const MAX_TYPES: usize = 50;
/// How many of the nearest places are read before the request's own type
/// rules pick among them. It is the most one og search answers.
const NEARBY_WINDOW: usize = crate::http::bounds::MAX_SEARCH_LIMIT;

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SearchNearbyRequest {
    #[serde(alias = "location_restriction")]
    location_restriction: Option<Area>,
    #[serde(alias = "included_types")]
    included_types: Vec<String>,
    #[serde(alias = "excluded_types")]
    excluded_types: Vec<String>,
    #[serde(alias = "included_primary_types")]
    included_primary_types: Vec<String>,
    #[serde(alias = "excluded_primary_types")]
    excluded_primary_types: Vec<String>,
    #[serde(alias = "max_result_count", deserialize_with = "proto_int")]
    max_result_count: Option<i64>,
    #[serde(alias = "rank_preference")]
    rank_preference: Option<String>,
    #[serde(alias = "language_code")]
    language_code: Option<String>,
}

/// Search places around a point (Places API v1)
///
/// The answer is the places inside the circle, nearest first, under either
/// `rankPreference`: og holds no measure of how popular a place is. A type og
/// has no category for names no place.
pub(super) async fn search_nearby(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    bytes: Bytes,
) -> Response {
    run(async {
        let query = QueryParams::parse(query.as_deref())?;
        let mask = field_mask(&headers, &query);
        let request: SearchNearbyRequest = body(&bytes)?;
        let (center, radius) = nearby_circle(request.location_restriction.as_ref())?;
        let limit = match request.max_result_count {
            None | Some(0) => MAX_NEARBY,
            Some(count) if (1..=MAX_NEARBY).contains(&count) => count,
            Some(count) => {
                return Err(invalid(format!(
                    "maxResultCount must be within [1, {MAX_NEARBY}], got {count}"
                )));
            }
        };
        if let Some(rank) = request.rank_preference.as_deref()
            && !["RANK_PREFERENCE_UNSPECIFIED", "DISTANCE", "POPULARITY"].contains(&rank)
        {
            return Err(invalid(format!(
                "Invalid value at 'rank_preference' (type.googleapis.com/google.maps.places.v1.SearchNearbyRequest.RankPreference), \"{rank}\""
            )));
        }
        let rules = TypeRules::of(&request)?;
        let language = places_language(&state, request.language_code.clone())?;
        let categories = rules.categories();
        if categories.is_empty() {
            return Ok(respond(
                &SearchTextResponse { places: Vec::new() },
                mask.as_ref(),
            ));
        }
        let args = Args::new()
            .set("limit", NEARBY_WINDOW.to_string())
            .set("focus", format!("{},{}", center[0], center[1]))
            .set("radius", radius.to_string())
            .maybe("lang", language.code());
        let found = native::of_categories(&state, categories, args)
            .await
            .map_err(from_problem)?;
        let answer = SearchTextResponse {
            places: found
                .features
                .iter()
                .filter(|feature| rules.admit(Kind::of(&feature.properties.common)))
                .take(usize::try_from(limit).unwrap_or_default())
                .map(|feature| place(feature, &language))
                .collect(),
        };
        Ok(respond(&answer, mask.as_ref()))
    })
    .await
}

/// The circle Nearby Search is held to, as og's `focus` point and a radius
/// in metres.
fn nearby_circle(restriction: Option<&Area>) -> Result<([f64; 2], f64), VendorError> {
    let circle = match restriction {
        Some(Area {
            circle: Some(circle),
            rectangle: None,
        }) => circle,
        Some(Area {
            rectangle: Some(_), ..
        }) => {
            return Err(invalid(
                "locationRestriction takes only a circle in Nearby Search",
            ));
        }
        _ => return Err(invalid("locationRestriction.circle is required")),
    };
    let center = circle.center.checked("locationRestriction.circle.center")?;
    if !(circle.radius > 0.0 && circle.radius <= MAX_NEARBY_RADIUS) {
        return Err(invalid(format!(
            "locationRestriction.circle.radius must be within (0, {MAX_NEARBY_RADIUS}], got {}",
            circle.radius
        )));
    }
    Ok((center, circle.radius))
}

/// The four type lists of a Nearby Search request. A place must pass each
/// list that is sent.
struct TypeRules<'a> {
    included: &'a [String],
    excluded: &'a [String],
    included_primary: &'a [String],
    excluded_primary: &'a [String],
}

impl<'a> TypeRules<'a> {
    fn of(request: &'a SearchNearbyRequest) -> Result<Self, VendorError> {
        let lists = [
            ("includedTypes", &request.included_types),
            ("excludedTypes", &request.excluded_types),
            ("includedPrimaryTypes", &request.included_primary_types),
            ("excludedPrimaryTypes", &request.excluded_primary_types),
        ];
        for (name, list) in lists {
            if list.len() > MAX_TYPES {
                return Err(invalid(format!(
                    "{name} takes at most {MAX_TYPES} types, got {}",
                    list.len()
                )));
            }
        }
        for (included, excluded) in [(lists[0], lists[1]), (lists[2], lists[3])] {
            if let Some(both) = included.1.iter().find(|named| excluded.1.contains(named)) {
                return Err(invalid(format!(
                    "{both} is in both {} and {}",
                    included.0, excluded.0
                )));
            }
        }
        Ok(Self {
            included: &request.included_types,
            excluded: &request.excluded_types,
            included_primary: &request.included_primary_types,
            excluded_primary: &request.excluded_primary_types,
        })
    }

    /// og's categories that can hold a place the lists admit. With no type
    /// asked for they are every kind.
    fn categories(&self) -> Vec<&'static str> {
        let named = |list: &'a [String]| categories_of(list.iter().map(String::as_str));
        match (self.included.is_empty(), self.included_primary.is_empty()) {
            (true, true) => crate::categories::keys(),
            (false, true) => named(self.included),
            (true, false) => named(self.included_primary),
            // A place must pass both, so the narrower list finds them all.
            (false, false) => {
                let (types, primary) = (named(self.included), named(self.included_primary));
                if primary.len() <= types.len() {
                    primary
                } else {
                    types
                }
            }
        }
    }

    fn admit(&self, kind: Kind) -> bool {
        let types = kind.types();
        let primary = kind.primary();
        let holds = |list: &[String], named: &str| list.iter().any(|listed| listed == named);
        let any_type = |list: &[String]| types.iter().any(|named| holds(list, named));
        let is_primary = |list: &[String]| primary.is_some_and(|primary| holds(list, primary));
        (self.included.is_empty() || any_type(self.included))
            && !any_type(self.excluded)
            && (self.included_primary.is_empty() || is_primary(self.included_primary))
            && !is_primary(self.excluded_primary)
    }
}

/// `pageSize` wins over the older `maxResultCount`; 0 or unset is Google's
/// default of 20, and more than 20 is cut to 20 rather than refused.
fn page_size(size: Option<i64>) -> Result<i64, VendorError> {
    match size {
        Some(size) if size < 0 => Err(invalid(format!(
            "pageSize must not be negative, got {size}"
        ))),
        Some(size) if size > 0 => Ok(size.min(MAX_PAGE_SIZE)),
        _ => Ok(MAX_PAGE_SIZE),
    }
}

/// og's `focus` from `locationBias` and its `bbox` from a rectangle
/// `locationRestriction`. Google takes one or the other.
fn bias_and_restriction(
    bias: Option<&Area>,
    restriction: Option<&Area>,
) -> Result<(Option<String>, Option<String>), VendorError> {
    if bias.is_some() && restriction.is_some() {
        return Err(invalid(
            "locationBias and locationRestriction cannot both be set",
        ));
    }
    let focus = bias
        .map(|bias| bias.focus("locationBias"))
        .transpose()?
        .flatten();
    let bbox = match restriction {
        Some(Area {
            rectangle: Some(rectangle),
            circle: None,
        }) => Some(rectangle.bbox("locationRestriction.rectangle")?),
        Some(Area {
            rectangle: Some(_),
            circle: Some(_),
        }) => return Err(both_shapes("locationRestriction")),
        Some(Area {
            circle: Some(_), ..
        }) => {
            return Err(invalid(
                "locationRestriction takes only a rectangle in Text Search",
            ));
        }
        Some(Area {
            circle: None,
            rectangle: None,
        })
        | None => None,
    };
    Ok((focus, bbox))
}

fn place(feature: &ForwardFeature, language: &Language) -> Place {
    Place::of(
        &feature.properties.common,
        feature.geometry.coordinates,
        language,
    )
}

/// Places names its results in `languageCode`, English when none is sent.
fn places_language(state: &AppState, code: Option<String>) -> Result<Language<'_>, VendorError> {
    let code = code.filter(|code| !code.trim().is_empty());
    Language::parse(state.pack(), Some(code.as_deref().unwrap_or("en")))
}

pub(super) async fn details(
    State(state): State<AppState>,
    Path(place_id): Path<String>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        let query = QueryParams::parse(query.as_deref())?;
        let mask = field_mask(&headers, &query);
        let language = places_language(&state, query.get("languageCode").map(str::to_string))?;
        let feature = by_place_id(&state, &place_id, &language).await?;
        Ok(respond(&place(&feature, &language), mask.as_ref()))
    })
    .await
}

/// The bounds Google puts on each side of a photo asked for.
const PHOTO_SIZE: std::ops::RangeInclusive<i64> = 1..=4800;

/// `GET /v1/places/{place}/photos/{photo}/media`. og holds no photos, so a
/// place og knows has none by any name, and the name is refused as Google
/// refuses one it never issued.
pub(super) async fn photo_media(
    State(state): State<AppState>,
    Path((place_id, photo)): Path<(String, String)>,
    RawQuery(query): RawQuery,
) -> Response {
    run(async {
        let query = QueryParams::parse(query.as_deref())?;
        let size = |name: &str| -> Result<Option<i64>, VendorError> {
            let Some(value) = query.get(name) else {
                return Ok(None);
            };
            let pixels = value.parse::<i64>().map_err(|_| {
                invalid(format!(
                    "Invalid value at '{name}' (TYPE_INT32), \"{value}\""
                ))
            })?;
            if !PHOTO_SIZE.contains(&pixels) {
                return Err(invalid(format!(
                    "{name} must be within [1, 4800], got {pixels}"
                )));
            }
            Ok(Some(pixels))
        };
        let width = size("maxWidthPx")?;
        let height = size("maxHeightPx")?;
        if width.is_none() && height.is_none() {
            return Err(invalid(
                "Neither maxWidthPx nor maxHeightPx is set, and at least one is required",
            ));
        }
        let language = places_language(&state, None)?;
        by_place_id(&state, &place_id, &language).await?;
        Err(invalid(format!(
            "The photo resource 'places/{place_id}/photos/{photo}' is invalid."
        )))
    })
    .await
}

/// The record a Google place id names, shared with Place Geocoding.
pub(super) async fn by_place_id(
    state: &AppState,
    place_id: &str,
    language: &Language<'_>,
) -> Result<ForwardFeature, VendorError> {
    let not_found = || {
        error(
            Code::NotFound,
            format!("Place 'places/{place_id}' was not found."),
        )
    };
    let og_id = decode_id(place_id).ok_or_else(not_found)?;
    native::by_id(state, &og_id, language.code())
        .await
        .map_err(from_problem)?
        .ok_or_else(not_found)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct AutocompleteRequest {
    input: Option<String>,
    #[serde(alias = "input_offset", deserialize_with = "proto_int")]
    input_offset: Option<i64>,
    #[serde(alias = "location_bias")]
    location_bias: Option<Area>,
    #[serde(alias = "location_restriction")]
    location_restriction: Option<Area>,
    #[serde(alias = "included_region_codes")]
    included_region_codes: Vec<String>,
    origin: Option<LatLng>,
    #[serde(alias = "language_code")]
    language_code: Option<String>,
    #[serde(alias = "included_primary_types")]
    included_primary_types: Vec<String>,
}

/// The most types `includedPrimaryTypes` takes.
const MAX_PRIMARY_TYPES: usize = 5;

/// Google's collection types for Autocomplete, by the types each admits.
fn collection(named: &str) -> Option<&'static [&'static str]> {
    match named {
        "(cities)" => Some(&["locality"]),
        "(regions)" => Some(&[
            "locality",
            "sublocality",
            "postal_code",
            "country",
            "administrative_area_level_1",
            "administrative_area_level_2",
        ]),
        _ => None,
    }
}

#[derive(Debug, Serialize)]
struct AutocompleteResponse {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    suggestions: Vec<Suggestion>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Suggestion {
    place_prediction: PlacePrediction,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlacePrediction {
    place: String,
    place_id: String,
    text: FormattableText,
    structured_format: StructuredFormat,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    types: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    distance_meters: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StructuredFormat {
    main_text: FormattableText,
    #[serde(skip_serializing_if = "Option::is_none")]
    secondary_text: Option<FormattableText>,
}

#[derive(Debug, PartialEq, Serialize)]
struct FormattableText {
    text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    matches: Vec<StringRange>,
}

/// Offsets in Unicode scalar values, end exclusive. A start of 0 is none,
/// as proto3 JSON leaves out a default.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct StringRange {
    #[serde(skip_serializing_if = "Option::is_none")]
    start_offset: Option<std::num::NonZeroUsize>,
    end_offset: usize,
}

/// Where a circle restriction is, once og has searched its bounding box.
struct Circle {
    center: [f64; 2],
    radius: f64,
}

pub(super) async fn autocomplete(
    State(state): State<AppState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    bytes: Bytes,
) -> Response {
    run(async {
        let query = QueryParams::parse(query.as_deref())?;
        let mask = field_mask(&headers, &query);
        let request: AutocompleteRequest = body(&bytes)?;
        let input = request
            .input
            .filter(|input| !input.trim().is_empty())
            .ok_or_else(|| invalid("input is required"))?;
        let input = typed_before_cursor(&input, request.input_offset)?;
        let primary = request
            .included_primary_types
            .iter()
            .map(|named| named.trim())
            .filter(|named| !named.is_empty())
            .collect::<Vec<_>>();
        if primary.len() > MAX_PRIMARY_TYPES {
            return Err(invalid(format!(
                "includedPrimaryTypes takes at most {MAX_PRIMARY_TYPES} types, got {}",
                primary.len()
            )));
        }
        let admits = |kind: Kind| {
            primary.is_empty()
                || primary.iter().any(|named| match collection(named) {
                    Some(types) => kind.types().iter().any(|held| types.contains(held)),
                    None => kind.primary() == Some(*named) || kind.types().contains(named),
                })
        };
        if request.location_bias.is_some() && request.location_restriction.is_some() {
            return Err(invalid(
                "locationBias and locationRestriction cannot both be set",
            ));
        }
        let focus = request
            .location_bias
            .map(|bias| bias.focus("locationBias"))
            .transpose()?
            .flatten();
        let (bbox, circle) = match request.location_restriction {
            None => (None, None),
            Some(area) => restriction(&area)?,
        };
        let origin = request
            .origin
            .map(|origin| origin.checked("origin"))
            .transpose()?;
        let countries = request
            .included_region_codes
            .iter()
            .map(|code| code.trim())
            .filter(|code| !code.is_empty())
            .collect::<Vec<_>>();
        // Autocomplete reads the browser's language when the body names none.
        let language = places_language(
            &state,
            request
                .language_code
                .filter(|code| !code.trim().is_empty())
                .or_else(|| accept_language(&headers)),
        )?;
        // A type filter reads more than the answer holds.
        let window = match primary.is_empty() {
            true => MAX_SUGGESTIONS,
            false => crate::search::MAX_AUTOCOMPLETE_LIMIT,
        };
        let args = Args::new()
            .set("q", input.clone())
            .set("limit", window.to_string())
            .maybe("lang", language.code())
            .maybe("focus", focus)
            .maybe("bbox", bbox)
            .maybe(
                "country",
                (!countries.is_empty()).then(|| countries.join(",")),
            );
        let found = native::autocomplete(&state, args)
            .await
            .map_err(from_problem)?;
        let suggestions = found
            .features
            .iter()
            .filter(|feature| {
                circle.as_ref().is_none_or(|circle| {
                    let [lon, lat] = feature.geometry.coordinates;
                    haversine_m(circle.center[0], circle.center[1], lon, lat) <= circle.radius
                })
            })
            .filter(|feature| admits(Kind::of(&feature.properties.common)))
            .take(MAX_SUGGESTIONS)
            .map(|feature| Suggestion {
                place_prediction: prediction(feature, origin, &language),
            })
            .collect();
        Ok(respond(
            &AutocompleteResponse { suggestions },
            mask.as_ref(),
        ))
    })
    .await
}

/// The text before the cursor, which is what the user has typed so far.
fn typed_before_cursor(input: &str, offset: Option<i64>) -> Result<String, VendorError> {
    let Some(offset) = offset else {
        return Ok(input.to_string());
    };
    let offset = usize::try_from(offset)
        .map_err(|_| invalid(format!("inputOffset must not be negative, got {offset}")))?;
    let typed = input.chars().take(offset).collect::<String>();
    if typed.trim().is_empty() {
        return Err(invalid("input before inputOffset is empty"));
    }
    Ok(typed)
}

/// og's `bbox` for a restriction. A circle is searched as the box around it
/// and the results outside the circle dropped after, so no result lies
/// outside it. Clamped at the poles and the antimeridian, which loses only
/// the part of the circle beyond them.
fn restriction(area: &Area) -> Result<(Option<String>, Option<Circle>), VendorError> {
    match (area.circle, area.rectangle) {
        (Some(_), Some(_)) => Err(both_shapes("locationRestriction")),
        (None, Some(rectangle)) => {
            Ok((Some(rectangle.bbox("locationRestriction.rectangle")?), None))
        }
        (Some(circle), None) => {
            let [lon, lat] = circle.center.checked("locationRestriction.circle.center")?;
            if !(0.0..=50_000.0).contains(&circle.radius) {
                return Err(invalid(format!(
                    "locationRestriction.circle.radius must be within [0, 50000], got {}",
                    circle.radius
                )));
            }
            let lat_delta = circle.radius / METERS_PER_DEGREE;
            let lon_delta = circle.radius / (METERS_PER_DEGREE * lat.to_radians().cos().max(1e-6));
            let bbox = format!(
                "{},{},{},{}",
                (lon - lon_delta).max(-180.0),
                (lat - lat_delta).max(-90.0),
                (lon + lon_delta).min(180.0),
                (lat + lat_delta).min(90.0),
            );
            Ok((
                Some(bbox),
                Some(Circle {
                    center: [lon, lat],
                    radius: circle.radius,
                }),
            ))
        }
        (None, None) => Ok((None, None)),
    }
}

fn prediction(
    feature: &ForwardFeature,
    origin: Option<[f64; 2]>,
    language: &Language,
) -> PlacePrediction {
    let properties = &feature.properties;
    let common = &properties.common;
    let place = Place::of(common, feature.geometry.coordinates, language);
    let [lon, lat] = feature.geometry.coordinates;
    let highlights = properties
        .highlights
        .iter()
        .map(|highlight| (highlight.start, highlight.end))
        .collect::<Vec<_>>();
    // og marks what was typed in the label. The name leads the label, and
    // the text after it follows its ", ", so each is a window of it.
    let name_chars = common.name.chars().count();
    let main_text = match common.label.starts_with(common.name.as_str()) {
        true => within(&common.name, &highlights, 0),
        false => plain(&common.name),
    };
    PlacePrediction {
        text: within(&common.label, &highlights, 0),
        structured_format: StructuredFormat {
            main_text,
            secondary_text: after_name(common)
                .map(|text| within(&text, &highlights, name_chars + ", ".chars().count())),
        },
        types: Kind::of(common).types(),
        // Google leaves the distance out when it is 0.
        distance_meters: origin
            .map(|[from_lon, from_lat]| haversine_m(from_lon, from_lat, lon, lat).round() as u64)
            .filter(|meters| *meters > 0),
        place: place.name,
        place_id: place.id,
    }
}

fn plain(text: &str) -> FormattableText {
    FormattableText {
        text: text.to_string(),
        matches: Vec::new(),
    }
}

/// `text`, which starts at character `from` of the label, with og's
/// highlights of the label that fall in it, cut to it. Both count Unicode
/// scalar values, as Google's offsets do.
fn within(text: &str, highlights: &[(usize, usize)], from: usize) -> FormattableText {
    let characters = text.chars().collect::<Vec<_>>();
    let mut matches: Vec<StringRange> = Vec::new();
    for &(start, end) in highlights {
        let start = start.max(from) - from;
        let end = end.saturating_sub(from).min(characters.len());
        if start >= end {
            continue;
        }
        // Words typed together read as one match, the gap between them too.
        match matches.last_mut() {
            Some(last)
                if last.end_offset <= start
                    && characters[last.end_offset..start]
                        .iter()
                        .all(|character| !character.is_alphanumeric()) =>
            {
                last.end_offset = end;
            }
            _ => matches.push(StringRange {
                start_offset: std::num::NonZeroUsize::new(start),
                end_offset: end,
            }),
        }
    }
    FormattableText {
        text: text.to_string(),
        matches,
    }
}
