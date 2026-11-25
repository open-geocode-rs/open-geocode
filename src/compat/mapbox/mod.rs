mod category;
mod geocoding;
mod hours;
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
    api::{
        Accuracy, Attribution, Common, ForwardCollection, ForwardFeature, PartMatch, QueryMatch,
        ReverseCollection, ReverseFeature,
    },
    compat::{Args, VendorError, encode_area_id, encode_id, native, retry_after},
    http::problem::Problem,
    iso,
    record::Layer,
    runtime::AppState,
};

/// Mapbox's address type also includes numbered points on mapped ranges.
async fn complete(
    state: &AppState,
    args: Args,
    types: Option<&Types>,
) -> Result<ForwardCollection, Problem> {
    let layer = types.and_then(Types::og_layer);
    if layer != Some("address") {
        return native::autocomplete(state, args.maybe("layer", layer)).await;
    }
    let found = native::autocomplete(state, args.clone().set("layer", "address")).await?;
    if found.features.is_empty() {
        return native::autocomplete(state, args.set("layer", "interpolation")).await;
    }
    Ok(found)
}

/// An address filter keeps both mapped points and numbered interpolation.
async fn forward_geocode(
    state: &AppState,
    args: Args,
    types: Option<&Types>,
) -> Result<ForwardCollection, Problem> {
    let layer = types.and_then(Types::og_layer);
    if layer != Some("address") {
        return native::geocode(state, args.maybe("layer", layer)).await;
    }
    let found = native::geocode(state, args.clone().set("layer", "address")).await?;
    let matches_address = |feature: &ForwardFeature| {
        feature.properties.matched.as_ref().is_some_and(|matched| {
            matched.street == Some(PartMatch::Matched)
                && matches!(
                    matched.housenumber,
                    Some(PartMatch::Matched | PartMatch::Partial)
                )
                && [
                    matched.unit,
                    matched.locality,
                    matched.region,
                    matched.postcode,
                    matched.country,
                ]
                .into_iter()
                .all(|part| part != Some(PartMatch::Unmatched))
        })
    };
    if found.features.first().is_some_and(matches_address) {
        return Ok(found);
    }
    let mut ranges = native::geocode(state, args.set("layer", "interpolation")).await?;
    ranges.features.retain(matches_address);
    if ranges.features.is_empty() {
        return Ok(found);
    }
    Ok(ranges)
}

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
        .route("/search/searchbox/v1/category/{id}", get(category::search))
        .route("/search/searchbox/v1/list/category", get(category::list))
        .route("/", any(not_found))
        // Mapbox documents no 405: a wrong method is an unknown endpoint.
        .method_not_allowed_fallback(not_found)
        .fallback(not_found)
}

async fn not_found() -> VendorError {
    error(StatusCode::NOT_FOUND, "Not Found")
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

    /// `proximity` as og's `focus`, checked in Mapbox's words. `ip` asks
    /// for the caller's own location, which og has no way to know, so it
    /// biases nothing.
    fn focus(&self, api: Api) -> Result<Option<String>, VendorError> {
        let Some(value) = self
            .get("proximity")
            .filter(|value| !value.eq_ignore_ascii_case("ip"))
        else {
            return Ok(None);
        };
        let numbers = numbers(value)
            .filter(|numbers| numbers.len() == 2)
            .ok_or_else(|| api.invalid("Proximity must be an array in the form [lon, lat]"))?;
        let (lon, lat) = (numbers[0], numbers[1]);
        if !(-180.0..=180.0).contains(&lon) {
            return Err(api.invalid("Proximity lon value must be a number between -180 and 180"));
        }
        if !(-90.0..=90.0).contains(&lat) {
            return Err(api.invalid("Proximity lat value must be a number between -90 and 90"));
        }
        Ok(Some(format!("{lon},{lat}")))
    }

    fn point(&self, api: Api) -> Result<(String, String), VendorError> {
        let coordinate = |name: &str, label: &str, bound: f64| {
            self.get(name)
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && (-bound..=bound).contains(value))
                .map(|value| value.to_string())
                .ok_or_else(|| {
                    api.invalid(format!(
                        "{label} must be a number between -{bound} and {bound}"
                    ))
                })
        };
        Ok((
            coordinate("longitude", "Longitude", 180.0)?,
            coordinate("latitude", "Latitude", 90.0)?,
        ))
    }

    fn bbox(&self, api: Api) -> Result<Option<String>, VendorError> {
        let Some(value) = self.get("bbox") else {
            return Ok(None);
        };
        let numbers = numbers(value)
            .filter(|numbers| numbers.len() == 4)
            .ok_or_else(|| {
                api.invalid(
                    "BBox is not valid. Must be an array of format [minX, minY, maxX, maxY]",
                )
            })?;
        let [min_x, min_y, max_x, max_y] = [numbers[0], numbers[1], numbers[2], numbers[3]];
        for (name, value) in [("minX", min_x), ("maxX", max_x)] {
            if !(-180.0..=180.0).contains(&value) {
                return Err(api.invalid(format!(
                    "BBox {name} value must be a number between -180 and 180"
                )));
            }
        }
        for (name, value) in [("minY", min_y), ("maxY", max_y)] {
            if !(-90.0..=90.0).contains(&value) {
                return Err(api.invalid(format!(
                    "BBox {name} value must be a number between -90 and 90"
                )));
            }
        }
        for (low, high, low_value, high_value) in [
            ("minX", "maxX", min_x, max_x),
            ("minY", "maxY", min_y, max_y),
        ] {
            if low_value > high_value {
                return Err(api.invalid(format!(
                    "BBox {low} value cannot be greater than {high} value"
                )));
            }
        }
        Ok(Some(format!("{min_x},{min_y},{max_x},{max_y}")))
    }

    fn types(&self, api: Api) -> Result<Option<Types>, VendorError> {
        self.get("types")
            .map(|types| Types::parse(api, types))
            .transpose()
    }
}

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

    fn reverse_layer(&self) -> Option<&'static str> {
        self.og_layer()
            .filter(|layer| matches!(*layer, "address" | "street" | "poi"))
    }

    fn admits(&self, feature_type: &str) -> bool {
        self.0.contains(&feature_type)
    }
}

fn admitted(types: Option<&Types>, common: &Common) -> bool {
    types.is_none_or(|types| types.admits(feature_type(common)))
}

fn feature_type(common: &Common) -> &'static str {
    match common.layer {
        Layer::Address => "address",
        Layer::Interpolation if is_address(common) => "address",
        Layer::Interpolation | Layer::Street => "street",
        Layer::Postcode => "postcode",
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

/// `coordinates.accuracy`, which Mapbox sends on addresses only, in its own
/// words for how the point was found. og's own point of an address is a
/// known address point, Mapbox's `point`. The centre of the outline an
/// address is drawn with lies in its building, Mapbox's `rooftop`. A house
/// placed between its neighbours is `interpolated`. Search Box also has
/// `street` for a point on the street. og's `approximate`, a node standing
/// for something larger, is no ZIP centroid, Mapbox's `approximate`, so it
/// is sent as nothing.
fn accuracy(api: Api, common: &Common) -> Option<&'static str> {
    if !is_address(common) {
        return None;
    }
    match common.accuracy {
        Accuracy::Rooftop => Some("point"),
        Accuracy::Centroid => Some("rooftop"),
        Accuracy::Interpolated => Some("interpolated"),
        Accuracy::Street => match api {
            Api::SearchBox => Some("street"),
            Api::Geocoding => None,
        },
        Accuracy::Approximate => None,
    }
}

fn point_country(collection: &ReverseCollection) -> Option<&str> {
    collection.features.iter().find_map(|feature| {
        let common = &feature.properties.common;
        common
            .context
            .country
            .as_ref()
            .and_then(|country| country.code.as_deref())
            .or_else(|| common.id.strip_prefix("derived:country:"))
    })
}

fn keep_countries(collection: &mut ReverseCollection, countries: &[String]) {
    let kept = countries.is_empty()
        || point_country(collection)
            .is_some_and(|code| countries.iter().any(|asked| asked == code));
    if !kept {
        collection.features.clear();
    }
}

fn countries(api: Api, params: &Params) -> Result<Vec<String>, crate::compat::VendorError> {
    params.get("country").map_or(Ok(Vec::new()), |value| {
        value
            .split(',')
            .map(str::trim)
            .map(|code| {
                let letters =
                    code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_alphabetic());
                match letters {
                    true => Ok(code.to_ascii_uppercase()),
                    false => Err(api.invalid(format!("Stack \"{code}\" is not a known stack."))),
                }
            })
            .collect()
    })
}

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

#[derive(Debug, Clone, PartialEq, Serialize)]
struct Coordinates {
    longitude: f64,
    latitude: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    accuracy: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    routable_points: Vec<RoutablePoint>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct RoutablePoint {
    name: &'static str,
    longitude: f64,
    latitude: f64,
}

#[derive(Debug, Clone, Copy)]
struct Found<'a> {
    common: &'a Common,
    coordinates: [f64; 2],
    matched: Option<&'a QueryMatch>,
    distance_m: Option<f64>,
}

impl<'a> Found<'a> {
    fn forward(feature: &'a ForwardFeature) -> Self {
        Self {
            common: &feature.properties.common,
            coordinates: feature.geometry.coordinates,
            matched: feature.properties.matched.as_ref(),
            distance_m: feature.properties.distance_m,
        }
    }

    fn reverse(feature: &'a ReverseFeature) -> Self {
        Self {
            common: &feature.properties.common,
            coordinates: feature.geometry.coordinates,
            matched: None,
            // Mapbox measures `distance` from `proximity` or `origin`, which
            // a reverse lookup has neither of, so og's distance from the
            // point has no slot.
            distance_m: None,
        }
    }
}

fn numbers(value: &str) -> Option<Vec<f64>> {
    value
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<f64>()
                .ok()
                .filter(|number| number.is_finite())
        })
        .collect()
}

/// og's box as Mapbox's `[minLon, minLat, maxLon, maxLat]`. Mapbox's box
/// cannot cross the antimeridian, so og's box that does is sent as every
/// longitude, which still holds the result.
fn bbox(common: &Common) -> Option<[f64; 4]> {
    let [west, south, east, north] = common.bbox?;
    Some(match west > east {
        true => [-180.0, south, 180.0, north],
        false => [west, south, east, north],
    })
}

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Area {
    id: Option<String>,
    name: String,
    address_number: Option<String>,
    street_name: Option<String>,
    country_code: Option<String>,
    country_code_alpha_3: Option<&'static str>,
    region_code: Option<String>,
    region_code_full: Option<String>,
    wikidata_id: Option<String>,
}

impl Area {
    fn named(id: Option<String>, name: &str) -> Self {
        Self {
            id,
            name: name.to_string(),
            ..Self::default()
        }
    }

    fn of(area: &crate::api::ContextArea) -> Self {
        Self {
            wikidata_id: area.wikidata.map(|item| format!("Q{item}")),
            ..Self::named(Some(encode_area_id(area)), &area.name)
        }
        .with_country_code(area.code.as_deref())
    }

    fn region(area: &crate::api::ContextArea) -> Self {
        Self::of(area).with_region_code(area.code.as_deref())
    }

    fn with_country_code(mut self, code: Option<&str>) -> Self {
        if let Some(code) = code.filter(|code| !code.contains('-')) {
            self.country_code_alpha_3 = iso::alpha3(code);
            self.country_code = Some(code.to_string());
        }
        self
    }

    fn with_region_code(mut self, code: Option<&str>) -> Self {
        if let Some((_, own)) = code.and_then(|code| code.split_once('-')) {
            self.region_code = Some(own.to_string());
            self.region_code_full = code.map(str::to_string);
        }
        self
    }
}

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
    block: Option<L>,
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
            block: None,
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
    fn map_named<L>(self, layer: impl Fn(&str, Area) -> L) -> Context<L> {
        let layer = &layer;
        let named = |key: &'static str| move |area| layer(key, area);
        Context {
            address: self.address.map(named("address")),
            secondary_address: self.secondary_address.map(named("secondary_address")),
            street: self.street.map(named("street")),
            neighborhood: self.neighborhood.map(named("neighborhood")),
            block: self.block.map(named("block")),
            postcode: self.postcode.map(named("postcode")),
            locality: self.locality.map(named("locality")),
            place: self.place.map(named("place")),
            district: self.district.map(named("district")),
            region: self.region.map(named("region")),
            country: self.country.map(named("country")),
        }
    }

    fn recorded(self) -> Self {
        let kept = |layer: Option<Area>| layer.filter(|area| area.id.is_some());
        Self {
            address: kept(self.address),
            secondary_address: kept(self.secondary_address),
            street: kept(self.street),
            neighborhood: kept(self.neighborhood),
            block: kept(self.block),
            postcode: kept(self.postcode),
            locality: kept(self.locality),
            place: kept(self.place),
            district: kept(self.district),
            region: kept(self.region),
            country: kept(self.country),
        }
    }

    fn map<L>(self, layer: impl Fn(Area) -> L) -> Context<L> {
        Context {
            address: self.address.map(&layer),
            secondary_address: self.secondary_address.map(&layer),
            street: self.street.map(&layer),
            neighborhood: self.neighborhood.map(&layer),
            block: self.block.map(&layer),
            postcode: self.postcode.map(&layer),
            locality: self.locality.map(&layer),
            place: self.place.map(&layer),
            district: self.district.map(&layer),
            region: self.region.map(&layer),
            country: self.country.map(&layer),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Translated {
    mapbox_id: String,
    feature_type: &'static str,
    name: String,
    address: Option<String>,
    full_address: Option<String>,
    place_formatted: Option<String>,
    coordinates: Coordinates,
    context: Context<Area>,
    maki: Option<&'static str>,
    brand: Option<String>,
    bbox: Option<[f64; 4]>,
}

impl Translated {
    fn of(api: Api, found: Found) -> Self {
        let common = found.common;
        let [longitude, latitude] = found.coordinates;
        let mapbox_id = encode_id(common);
        let feature_type = feature_type(common);
        let parts = &common.address;
        let is_address = is_address(common);
        let address_subject = parts.street.as_deref().or(parts.place.as_deref());
        let address = if is_address {
            Some(common.name.clone())
        } else {
            address_line(parts.housenumber.as_deref(), address_subject)
        };
        let context = context(common, &mapbox_id, is_address);
        let place_formatted = place_line(common.layer, &context);
        let address_name = if common.layer == Layer::Poi {
            address.as_deref().filter(|_| address_subject.is_some())
        } else {
            Some(common.name.as_str())
        };
        let full_address = address_name.map(|name| {
            let unit = parts.unit.as_deref().map(crate::labels::unit_label);
            [Some(name), unit.as_deref(), place_formatted.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ")
        });
        Self {
            context,
            mapbox_id,
            feature_type,
            name: common.name.clone(),
            address,
            full_address,
            place_formatted,
            coordinates: Coordinates {
                longitude,
                latitude,
                accuracy: accuracy(api, common),
                routable_points: common
                    .access
                    .iter()
                    .map(|entrance| RoutablePoint {
                        name: "entrance",
                        longitude: entrance.lon,
                        latitude: entrance.lat,
                    })
                    .collect(),
            },
            maki: common.category.as_deref().and_then(maki),
            brand: common.brand.as_ref().map(|brand| brand.name.clone()),
            bbox: bbox(common),
        }
    }
}

fn address_line(number: Option<&str>, street: Option<&str>) -> Option<String> {
    let line = [number, street]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    (!line.is_empty()).then_some(line)
}

/// Mapbox joins region and postcode with a space inside the place line.
fn place_line(layer: Layer, context: &Context<Area>) -> Option<String> {
    let place =
        area_name(&context.place).filter(|_| !matches!(layer, Layer::Locality | Layer::Place));
    let region = area_name(&context.region).filter(|_| layer != Layer::Region);
    let postcode = area_name(&context.postcode)
        .filter(|_| !matches!(layer, Layer::Postcode | Layer::Region | Layer::Country));
    let country = area_name(&context.country).filter(|_| layer != Layer::Country);
    if place.is_none() && region.is_none() && country.is_none() {
        return None;
    }
    let region_postcode = [region, postcode]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    let line = [
        place,
        (!region_postcode.is_empty()).then_some(region_postcode.as_str()),
        country,
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");
    Some(line)
}

fn area_name(area: &Option<Area>) -> Option<&str> {
    area.as_ref()
        .map(|area| area.name.trim())
        .filter(|name| !name.is_empty())
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
        context.secondary_address = parts
            .unit
            .as_deref()
            .map(|unit| Area::named(Some(mapbox_id.to_string()), unit));
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
    let code = common
        .code
        .as_deref()
        .or_else(|| common.id.strip_prefix("derived:country:"));
    *slot = Some(match common.layer {
        Layer::Country => own().with_country_code(code),
        Layer::Region => own().with_region_code(code),
        _ => own(),
    });
    context
}

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
