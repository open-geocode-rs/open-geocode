//! `/compat/mapbox`: Mapbox Geocoding v6 and Search Box v1.
//!
//! Geocoding v6 answers `/search/geocode/v6/{forward,reverse,batch}` from
//! og's `/geocode`, `/places?ids` and `/reverse`. Search Box v1 answers
//! `/search/searchbox/v1/{suggest,retrieve,forward,reverse}` from
//! `/autocomplete`, `/places`, `/geocode` and `/reverse`. Its category
//! endpoints answer 501: og has no category search to serve them from.

mod geocoding;
mod searchbox;
#[cfg(test)]
mod tests;

use axum::{
    Router,
    extract::{Query, rejection::QueryRejection},
    http::StatusCode,
    routing::{any, get, post},
};
use serde::Serialize;
use serde_json::json;

use crate::{
    api::{Accuracy, Attribution, Common},
    compat::{VendorError, encode_id, iso, retry_after},
    http::problem::Problem,
    record::Layer,
    runtime::AppState,
};

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/search/geocode/v6/forward", get(geocoding::forward))
        .route("/search/geocode/v6/reverse", get(geocoding::reverse))
        .route("/search/geocode/v6/batch", post(geocoding::batch))
        .route("/search/searchbox/v1/suggest", get(searchbox::suggest))
        .route(
            "/search/searchbox/v1/retrieve/{id}",
            get(searchbox::retrieve),
        )
        .route("/search/searchbox/v1/forward", get(searchbox::forward))
        .route("/search/searchbox/v1/reverse", get(searchbox::reverse))
        .route("/search/searchbox/v1/category/{id}", any(not_implemented))
        .route("/search/searchbox/v1/list/category", any(not_implemented))
        .route("/", any(not_found))
        // Mapbox documents no 405: a wrong method is an unknown endpoint.
        .method_not_allowed_fallback(not_found)
        .fallback(not_found)
}

async fn not_found() -> VendorError {
    error(StatusCode::NOT_FOUND, "Not Found")
}

// og's categories are OSM tags with no canonical id list to serve
// `list/category` from, and no search that returns only one category.
async fn not_implemented() -> VendorError {
    error(StatusCode::NOT_IMPLEMENTED, "Not Implemented")
}

/// Mapbox's error body, `{"message": ...}`.
fn error(status: StatusCode, message: impl Into<String>) -> VendorError {
    VendorError::new(status, json!({ "message": message.into() }))
}

/// Which product answers. Geocoding rejects bad input with 422, Search Box
/// with 400.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Api {
    Geocoding,
    SearchBox,
}

impl Api {
    fn invalid(self, message: impl Into<String>) -> VendorError {
        let status = match self {
            Api::Geocoding => StatusCode::UNPROCESSABLE_ENTITY,
            Api::SearchBox => StatusCode::BAD_REQUEST,
        };
        error(status, message)
    }

    /// og's Problem as Mapbox's error. Mapbox documents no message for most
    /// of og's checks, so og's own detail is the message.
    fn failed(self, problem: Problem) -> VendorError {
        let detail = problem.detail().unwrap_or_default().to_string();
        let vendor = match problem.status() {
            400 | 413 => self.invalid(detail),
            404 => error(StatusCode::NOT_FOUND, "Not Found"),
            501 => error(StatusCode::NOT_IMPLEMENTED, detail),
            503 => {
                let vendor = error(StatusCode::SERVICE_UNAVAILABLE, detail);
                match retry_after(&problem) {
                    Some((name, value)) => vendor.with_header(name, value),
                    None => vendor,
                }
            }
            _ => error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
        };
        vendor.caused_by(problem)
    }
}

/// A request's query parameters, or a batch query's keys, as text.
#[derive(Debug, Default)]
struct Params(Vec<(String, String)>);

impl Params {
    fn from_query(
        api: Api,
        query: Result<Query<Vec<(String, String)>>, QueryRejection>,
    ) -> Result<Self, VendorError> {
        query
            .map(|Query(pairs)| Self(pairs))
            .map_err(|_| api.invalid("could not parse query parameters"))
    }

    /// The value sent, trimmed; a blank one counts as not sent, as Mapbox's
    /// optional parameters do.
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
    }

    /// `proximity` as og's `focus`. `ip` asks for the caller's own location,
    /// which og has no way to know, so it biases nothing.
    fn focus(&self) -> Option<&str> {
        self.get("proximity")
            .filter(|value| !value.eq_ignore_ascii_case("ip"))
    }

    fn types(&self, api: Api) -> Result<Option<Types>, VendorError> {
        self.get("types")
            .map(|types| Types::parse(api, types))
            .transpose()
    }
}

/// The `types` a request asks for, as Mapbox `feature_type`s.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Types(Vec<&'static str>);

impl Types {
    fn parse(api: Api, value: &str) -> Result<Self, VendorError> {
        let mut types = Vec::new();
        for part in value
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
        {
            let canonical = match (api, part.to_ascii_lowercase().as_str()) {
                (_, "country") => "country",
                (_, "region") | (Api::SearchBox, "prefecture") => "region",
                (_, "postcode") => "postcode",
                (_, "district") => "district",
                (_, "place") | (Api::SearchBox, "city") => "place",
                (_, "locality") | (Api::SearchBox, "oaza") => "locality",
                (_, "neighborhood") | (Api::SearchBox, "chome") => "neighborhood",
                (_, "street") => "street",
                (_, "block") => "block",
                (_, "address") => "address",
                (Api::Geocoding, "secondary_address") => "secondary_address",
                (Api::SearchBox, "poi") => "poi",
                (Api::SearchBox, "category") => "category",
                _ => {
                    return Err(api.invalid(format!(
                        "Type \"{part}\" is not a known type. Must be one of: country, \
                         region, place, district, postcode, locality, neighborhood, and \
                         address"
                    )));
                }
            };
            if !types.contains(&canonical) {
                types.push(canonical);
            }
        }
        Ok(Self(types))
    }

    /// og's `layer` for these types, only when they are one type og has a
    /// layer for. Several types are filtered from og's answer instead, by
    /// [`Types::admits`], as og takes one layer at a time.
    fn og_layer(&self) -> Option<&'static str> {
        match self.0.as_slice() {
            [single] => match *single {
                "country" => Some("country"),
                "region" => Some("region"),
                "postcode" => Some("postcode"),
                "district" => Some("district"),
                "place" => Some("locality"),
                "neighborhood" => Some("neighbourhood"),
                "street" => Some("street"),
                "address" => Some("address"),
                "poi" => Some("poi"),
                _ => None,
            },
            _ => None,
        }
    }

    fn admits(&self, feature_type: &str) -> bool {
        self.0.contains(&feature_type)
    }
}

/// Keeps the features `types` asks for, so a type og has no layer for, or
/// one of several, is never answered with a feature of another type.
fn admitted(types: Option<&Types>, common: &Common) -> bool {
    types.is_none_or(|types| types.admits(feature_type(common)))
}

/// og's layer as Mapbox's `feature_type`.
fn feature_type(common: &Common) -> &'static str {
    match common.layer {
        Layer::Address => "address",
        // og's interpolation layer holds an estimated address, with its
        // number, and the range it was estimated on, a stretch of street.
        Layer::Interpolation if is_address(common) => "address",
        Layer::Interpolation | Layer::Street => "street",
        Layer::Postcode => "postcode",
        // og's locality is the city, town or village, Mapbox's `place`. og's
        // `place` layer is a named area outside the admin tiers, such as an
        // island or a hamlet, which Mapbox also files as a `place`.
        Layer::Locality | Layer::Place => "place",
        Layer::Neighbourhood => "neighborhood",
        Layer::District => "district",
        Layer::Region => "region",
        Layer::Country => "country",
        Layer::Poi => "poi",
    }
}

fn is_address(common: &Common) -> bool {
    common.layer == Layer::Address
        || common.layer == Layer::Interpolation && common.address.housenumber.is_some()
}

/// `coordinates.accuracy`, which Mapbox sends on addresses and streets only.
fn accuracy(common: &Common) -> Option<&'static str> {
    if !matches!(
        common.layer,
        Layer::Address | Layer::Interpolation | Layer::Street
    ) {
        return None;
    }
    Some(match common.accuracy {
        Accuracy::Rooftop => "rooftop",
        Accuracy::Interpolated => "interpolated",
        Accuracy::Street => "street",
        // The centre of a mapped area, as Mapbox's `approximate` is the
        // centre of a postal area.
        Accuracy::Centroid | Accuracy::Approximate => "approximate",
    })
}

/// og's credits as one line. They name the data the answer comes from,
/// never Mapbox's.
fn attribution(credits: &[Attribution]) -> String {
    credits
        .iter()
        .map(|credit| {
            let mut line = credit.text.clone();
            if let Some(licence) = credit
                .licence
                .as_deref()
                .filter(|licence| Some(*licence) != credit.url.as_deref())
            {
                line.push_str(", ");
                line.push_str(licence);
            }
            if let Some(url) = &credit.url {
                line.push_str(&format!(" ({url})"));
            }
            line
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// A Mapbox `coordinates` object.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct Coordinates {
    longitude: f64,
    latitude: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    accuracy: Option<&'static str>,
}

/// GeoJSON point geometry.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct Geometry {
    #[serde(rename = "type")]
    kind: &'static str,
    coordinates: [f64; 2],
}

impl Geometry {
    fn point(coordinates: [f64; 2]) -> Self {
        Self {
            kind: "Point",
            coordinates,
        }
    }
}

/// One context layer, before it is written with Geocoding's `mapbox_id` or
/// Search Box's `id`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Area {
    /// The encoded og id. Absent on a layer filled from an address part
    /// the source states, which names no record.
    id: Option<String>,
    name: String,
    address_number: Option<String>,
    street_name: Option<String>,
    country_code: Option<String>,
    country_code_alpha_3: Option<&'static str>,
    region_code: Option<String>,
    region_code_full: Option<String>,
}

impl Area {
    fn named(id: Option<String>, name: &str) -> Self {
        Self {
            id,
            name: name.to_string(),
            ..Self::default()
        }
    }

    /// An area og found the result in. A country's code is its own two
    /// letters; any other area's is its subdivision code, written on the
    /// region only.
    fn of(area: &crate::api::ContextArea) -> Self {
        let mut layer = Self::named(Some(encode_id(&area.id)), &area.name);
        if let Some(code) = area.code.as_deref().filter(|code| !code.contains('-')) {
            layer.country_code_alpha_3 = iso::alpha3(code);
            layer.country_code = Some(code.to_string());
        }
        layer
    }

    /// The region og found the result in, with its code (`ON` of `CA-ON`)
    /// and its full code.
    fn region(area: &crate::api::ContextArea) -> Self {
        let mut layer = Self::of(area);
        if let Some((_, own)) = area.code.as_deref().and_then(|code| code.split_once('-')) {
            layer.region_code = Some(own.to_string());
            layer.region_code_full.clone_from(&area.code);
        }
        layer
    }
}

/// The fields every layer may carry besides its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct LayerFields {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    address_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    street_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    country_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    country_code_alpha_3: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region_code_full: Option<String>,
}

impl Area {
    /// The id, and the fields each product writes beside it under its own
    /// key.
    fn split(self) -> (Option<String>, LayerFields) {
        (
            self.id,
            LayerFields {
                name: self.name,
                address_number: self.address_number,
                street_name: self.street_name,
                country_code: self.country_code,
                country_code_alpha_3: self.country_code_alpha_3,
                region_code: self.region_code,
                region_code_full: self.region_code_full,
            },
        )
    }
}

/// A Mapbox `context`, most specific layer first. `L` is the layer as each
/// product writes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Context<L> {
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<L>,
    #[serde(skip_serializing_if = "Option::is_none")]
    secondary_address: Option<L>,
    #[serde(skip_serializing_if = "Option::is_none")]
    street: Option<L>,
    #[serde(skip_serializing_if = "Option::is_none")]
    neighborhood: Option<L>,
    #[serde(skip_serializing_if = "Option::is_none")]
    postcode: Option<L>,
    #[serde(skip_serializing_if = "Option::is_none")]
    locality: Option<L>,
    #[serde(skip_serializing_if = "Option::is_none")]
    place: Option<L>,
    #[serde(skip_serializing_if = "Option::is_none")]
    district: Option<L>,
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<L>,
    #[serde(skip_serializing_if = "Option::is_none")]
    country: Option<L>,
}

impl<L> Default for Context<L> {
    fn default() -> Self {
        Self {
            address: None,
            secondary_address: None,
            street: None,
            neighborhood: None,
            postcode: None,
            locality: None,
            place: None,
            district: None,
            region: None,
            country: None,
        }
    }
}

impl Context<Area> {
    fn map<L>(self, layer: impl Fn(Area) -> L) -> Context<L> {
        Context {
            address: self.address.map(&layer),
            secondary_address: self.secondary_address.map(&layer),
            street: self.street.map(&layer),
            neighborhood: self.neighborhood.map(&layer),
            postcode: self.postcode.map(&layer),
            locality: self.locality.map(&layer),
            place: self.place.map(&layer),
            district: self.district.map(&layer),
            region: self.region.map(&layer),
            country: self.country.map(&layer),
        }
    }
}

/// One og result, read into the parts Mapbox's two products share.
#[derive(Debug, Clone, PartialEq)]
struct Translated {
    mapbox_id: String,
    feature_type: &'static str,
    name: String,
    /// House number and street, as Search Box's `address`.
    address: Option<String>,
    full_address: Option<String>,
    place_formatted: Option<String>,
    coordinates: Coordinates,
    context: Context<Area>,
    maki: Option<&'static str>,
    brand: Option<String>,
}

impl Translated {
    fn of(common: &Common, [longitude, latitude]: [f64; 2]) -> Self {
        let mapbox_id = encode_id(&common.id);
        let feature_type = feature_type(common);
        let parts = &common.address;
        let is_address = is_address(common);
        let address = if is_address {
            Some(common.name.clone())
        } else {
            address_line(
                parts.housenumber.as_deref(),
                parts.street.as_deref().or(parts.place.as_deref()),
            )
        };
        let (full_address, place_formatted) = split_label(
            common,
            address.as_deref().filter(|_| common.layer == Layer::Poi),
        );
        Self {
            context: context(common, &mapbox_id, is_address),
            mapbox_id,
            feature_type,
            name: common.name.clone(),
            address,
            full_address,
            place_formatted,
            coordinates: Coordinates {
                longitude,
                latitude,
                accuracy: accuracy(common),
            },
            maki: common.category.as_deref().and_then(maki),
            brand: common.brand.as_ref().map(|brand| brand.name.clone()),
        }
    }
}

/// House number then street, as og's own address names are written.
fn address_line(number: Option<&str>, street: Option<&str>) -> Option<String> {
    let line = [number, street]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    (!line.is_empty()).then_some(line)
}

/// `full_address` and `place_formatted` cut from og's label.
///
/// og's label is `name`, then for a POI its address line, then the unit,
/// then the stated locality, region, postcode and country. Mapbox's
/// `place_formatted` is only that last run, and a POI's `full_address` is
/// its address without the POI's name. A label that does not start with
/// `name` cannot be cut honestly, so neither is sent but the whole label.
fn split_label(common: &Common, poi_address: Option<&str>) -> (Option<String>, Option<String>) {
    let label = common.label.as_str();
    let non_empty = |text: &str| (!text.is_empty()).then(|| text.to_string());
    let rest = if label == common.name {
        Some("")
    } else {
        label
            .strip_prefix(common.name.as_str())
            .and_then(|rest| rest.strip_prefix(", "))
    };
    let Some(rest) = rest else {
        let full = (common.layer != Layer::Poi).then(|| label.to_string());
        return (full, None);
    };
    let full_address = if common.layer == Layer::Poi {
        non_empty(rest)
    } else {
        Some(label.to_string())
    };
    let mut place = rest;
    if let Some(line) = poi_address {
        place = strip_segment(place, line);
    }
    if common.address.unit.is_some() {
        // The unit as the label writes it ("Unit 2"), one segment.
        place = place.split_once(", ").map_or("", |(_, after)| after);
    }
    (full_address, non_empty(place))
}

fn strip_segment<'a>(text: &'a str, segment: &str) -> &'a str {
    if text == segment {
        return "";
    }
    text.strip_prefix(segment)
        .and_then(|rest| rest.strip_prefix(", "))
        .unwrap_or(text)
}

/// og's areas and stated address parts as Mapbox's context layers.
///
/// og's tiers map to Mapbox's by what they hold. og's locality is the city,
/// Mapbox's `place`; og's sublocality a division of it, Mapbox's
/// `locality`. og's subregion, subdistrict and `place` (island) tiers have
/// no Mapbox layer. A part the source states fills its layer only where
/// og found no area. It names no record, so it carries no id.
fn context(common: &Common, mapbox_id: &str, is_address: bool) -> Context<Area> {
    let areas = &common.context;
    let parts = &common.address;
    let stated = |part: &Option<String>| part.as_deref().map(|name| Area::named(None, name));
    let mut context = Context {
        neighborhood: areas
            .neighbourhood
            .as_ref()
            .map(Area::of)
            .or_else(|| stated(&parts.areas.last().cloned())),
        locality: areas.sublocality.as_ref().map(Area::of),
        place: areas
            .locality
            .as_ref()
            .map(Area::of)
            .or_else(|| stated(&parts.locality)),
        district: areas.district.as_ref().map(Area::of),
        region: areas
            .region
            .as_ref()
            .map(Area::region)
            .or_else(|| stated(&parts.region)),
        country: areas
            .country
            .as_ref()
            .map(Area::of)
            .or_else(|| stated(&parts.country)),
        postcode: stated(&parts.postcode),
        secondary_address: stated(&parts.unit),
        ..Context::default()
    };
    let own = || Area::named(Some(mapbox_id.to_string()), &common.name);
    if is_address {
        // Mapbox gives an address's `address` and `street` layers the
        // address's own id.
        context.address = Some(Area {
            address_number: parts.housenumber.clone(),
            street_name: parts.street.clone(),
            ..own()
        });
        context.street = parts
            .street
            .as_deref()
            .map(|street| Area::named(Some(mapbox_id.to_string()), street));
        return context;
    }
    if common.layer == Layer::Poi {
        context.address =
            address_line(parts.housenumber.as_deref(), parts.street.as_deref()).map(|line| Area {
                address_number: parts.housenumber.clone(),
                street_name: parts.street.clone(),
                ..Area::named(None, &line)
            });
        context.street = stated(&parts.street);
        return context;
    }
    // An area's context is what holds it. og reads an area's context at its
    // point, which also lies in areas smaller than the area itself.
    let below = match common.layer {
        Layer::Locality | Layer::Place => 2,
        Layer::District => 3,
        Layer::Region => 4,
        Layer::Country => 5,
        _ => 0,
    };
    let smaller = [
        &mut context.neighborhood,
        &mut context.locality,
        &mut context.place,
        &mut context.district,
        &mut context.region,
    ];
    for layer in smaller.into_iter().take(below) {
        *layer = None;
    }
    let slot = match common.layer {
        Layer::Street | Layer::Interpolation => &mut context.street,
        Layer::Postcode => &mut context.postcode,
        Layer::Neighbourhood => &mut context.neighborhood,
        Layer::Locality | Layer::Place => &mut context.place,
        Layer::District => &mut context.district,
        Layer::Region => &mut context.region,
        Layer::Country => &mut context.country,
        Layer::Address | Layer::Poi => return context,
    };
    // An area is its own layer; a country or region keeps the codes og
    // gave its tier.
    let coded = slot
        .take()
        .filter(|area| area.id.as_deref() == Some(mapbox_id))
        .unwrap_or_default();
    *slot = Some(Area {
        country_code: coded.country_code,
        country_code_alpha_3: coded.country_code_alpha_3,
        region_code: coded.region_code,
        region_code_full: coded.region_code_full,
        ..own()
    });
    context
}

/// The Maki icon for og's POI category, for the categories one icon plainly
/// names. Others get none rather than a guess.
fn maki(category: &str) -> Option<&'static str> {
    Some(match category {
        "amenity:cafe" => "cafe",
        "amenity:restaurant" => "restaurant",
        "amenity:fast_food" => "fast-food",
        "amenity:bar" => "bar",
        "amenity:pub" => "beer",
        "amenity:ice_cream" => "ice-cream",
        "amenity:fuel" => "fuel",
        "amenity:pharmacy" => "pharmacy",
        "amenity:hospital" => "hospital",
        "amenity:dentist" => "dentist",
        "amenity:doctors" => "doctor",
        "amenity:veterinary" => "veterinary",
        "amenity:bank" => "bank",
        "amenity:school" => "school",
        "amenity:college" | "amenity:university" => "college",
        "amenity:library" => "library",
        "amenity:parking" => "parking",
        "amenity:post_office" => "post",
        "amenity:police" => "police",
        "amenity:fire_station" => "fire-station",
        "amenity:cinema" => "cinema",
        "amenity:theatre" => "theatre",
        "amenity:place_of_worship" => "place-of-worship",
        "amenity:toilets" => "toilet",
        "shop:bakery" => "bakery",
        "shop:supermarket" => "grocery",
        "tourism:hotel" => "lodging",
        "tourism:museum" => "museum",
        "leisure:park" => "park",
        "aeroway:aerodrome" => "airport",
        "railway:station" => "rail",
        _ => return None,
    })
}
