//! og features as HERE result items.

use serde::{Deserialize, Serialize};

use crate::{
    compat::{encode_id, iso},
    record::Layer,
    spatial_index::haversine_m,
};

use super::{
    LatLng,
    category::{self, ItemCategory},
};

/// An og feature as og's JSON carries it. A live answer is read through its
/// JSON too, so a batch job's results, read back from disk, map by the same
/// path.
#[derive(Debug, Deserialize)]
pub(super) struct Feature {
    geometry: Geometry,
    properties: Properties,
}

#[derive(Debug, Deserialize)]
struct Geometry {
    coordinates: [f64; 2],
}

#[derive(Debug, Deserialize)]
struct Properties {
    id: String,
    layer: Layer,
    accuracy: Accuracy,
    name: String,
    label: String,
    #[serde(default)]
    address: Address,
    #[serde(default)]
    context: Context,
    confidence: Option<f32>,
    distance_m: Option<f64>,
    /// A place's kind, as og's map tag (`amenity:cafe`).
    category: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Accuracy {
    Rooftop,
    Interpolated,
    Street,
    Centroid,
    Approximate,
}

#[derive(Debug, Default, Deserialize)]
struct Address {
    housenumber: Option<String>,
    unit: Option<String>,
    street: Option<String>,
    #[serde(default)]
    areas: Vec<String>,
    locality: Option<String>,
    region: Option<String>,
    postcode: Option<String>,
    country: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Context {
    neighbourhood: Option<Area>,
    sublocality: Option<Area>,
    locality: Option<Area>,
    district: Option<Area>,
    region: Option<Area>,
    country: Option<Area>,
}

#[derive(Debug, Deserialize)]
struct Area {
    name: String,
    code: Option<String>,
}

impl Feature {
    /// A live og feature, read as its JSON reads.
    pub(super) fn of(feature: &impl Serialize) -> serde_json::Result<Self> {
        serde_json::to_value(feature).and_then(serde_json::from_value)
    }
}

/// A HERE result item. Every field og has no value for is left out, as HERE
/// leaves out what it lacks.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Item {
    pub(super) title: String,
    pub(super) id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) result_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) house_number_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) locality_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) administrative_area_type: Option<&'static str>,
    pub(super) address: ItemAddress,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) position: Option<Position>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) distance: Option<i64>,
    /// The HERE categories of a place whose kind has one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) categories: Vec<ItemCategory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scoring: Option<Scoring>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ItemAddress {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) country_code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) country_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) state_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) county: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) district: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) subdistrict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) street: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) postal_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) house_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) unit: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(super) struct Position {
    pub(super) lat: f64,
    pub(super) lng: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Scoring {
    /// og's calibrated confidence, the chance the result is the one asked
    /// for, which is what HERE's score says of a match. og has no score per
    /// field, so `fieldScore` is never sent.
    pub(super) query_score: f32,
}

/// HERE's kind of result for an og layer, as `resultType`, then
/// `localityType` or `administrativeAreaType`.
fn kind(
    layer: Layer,
) -> (
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
) {
    match layer {
        Layer::Address | Layer::Interpolation => (Some("houseNumber"), None, None),
        Layer::Street => (Some("street"), None, None),
        Layer::Poi => (Some("place"), None, None),
        Layer::Postcode => (Some("postalCodePoint"), None, None),
        Layer::Locality => (Some("locality"), Some("city"), None),
        // og's neighbourhood is a part of a town, which is HERE's district.
        Layer::Neighbourhood => (Some("locality"), Some("district"), None),
        // og's district is the tier between region and town, a county.
        Layer::District => (Some("administrativeArea"), None, Some("county")),
        Layer::Region => (Some("administrativeArea"), None, Some("state")),
        Layer::Country => (Some("administrativeArea"), None, Some("country")),
        // A named area outside the admin tiers, usually an island. HERE has
        // no kind for one.
        Layer::Place => (None, None, None),
    }
}

/// The HERE item for an og feature, with `position` set, `distance` from
/// og's own `distance_m` (a reverse answer's) and `scoring` from og's
/// confidence.
pub(super) fn item(feature: &Feature) -> Item {
    let properties = &feature.properties;
    let (mut result_type, locality_type, administrative_area_type) = kind(properties.layer);
    // An interpolation range found by its street alone, with no number
    // asked, is a street to HERE.
    if properties.layer == Layer::Interpolation && properties.address.housenumber.is_none() {
        result_type = Some("street");
    }
    let house_number_type = match (result_type, properties.accuracy) {
        (Some("houseNumber"), Accuracy::Interpolated) => Some("interpolated"),
        // The address's own point or building is a point address.
        (Some("houseNumber"), Accuracy::Rooftop | Accuracy::Centroid) => Some("PA"),
        _ => None,
    };
    let title = if properties.layer == Layer::Poi {
        properties.name.clone()
    } else {
        properties.label.clone()
    };
    let [lon, lat] = feature.geometry.coordinates;
    Item {
        title,
        id: encode_id(&properties.id),
        result_type,
        house_number_type,
        locality_type,
        administrative_area_type,
        address: address(properties),
        position: Some(Position { lat, lng: lon }),
        // HERE counts whole meters.
        distance: properties
            .distance_m
            .map(|distance| distance.round() as i64),
        categories: properties
            .category
            .as_deref()
            .map(category::of_place)
            .unwrap_or_default(),
        scoring: properties.confidence.map(|confidence| Scoring {
            query_score: confidence,
        }),
    }
}

/// The areas the feature lies in first. HERE's tiers are admin areas, and
/// og's context is the admin hierarchy. What the record states fills a tier
/// og has no area for. An area feature fills its own tier with its name, so
/// a town is its own city. Street, number, unit and postcode are the
/// record's.
fn address(properties: &Properties) -> ItemAddress {
    let stated = &properties.address;
    let context = &properties.context;
    let area = |area: &Option<Area>| area.as_ref().map(|area| area.name.clone());
    let own = |layer: Layer| (properties.layer == layer).then(|| properties.name.clone());
    let country_code = context
        .country
        .as_ref()
        .and_then(|country| country.code.as_deref())
        // OSM's `addr:country` is the two-letter code by convention.
        .or(stated.country.as_deref())
        .and_then(|code| iso::alpha3(&code.to_ascii_uppercase()));
    ItemAddress {
        label: Some(properties.label.clone()),
        country_code,
        country_name: area(&context.country).or_else(|| own(Layer::Country)),
        // The region's code within its country: `ON` of `CA-ON`.
        state_code: context
            .region
            .as_ref()
            .and_then(|region| region.code.as_deref()?.split_once('-'))
            .map(|(_, own)| own.to_string()),
        state: area(&context.region)
            .or_else(|| stated.region.clone())
            .or_else(|| own(Layer::Region)),
        county: area(&context.district).or_else(|| own(Layer::District)),
        city: area(&context.locality)
            .or_else(|| stated.locality.clone())
            .or_else(|| own(Layer::Locality)),
        district: area(&context.sublocality)
            .or_else(|| stated.areas.first().cloned())
            .or_else(|| own(Layer::Neighbourhood)),
        subdistrict: area(&context.neighbourhood).or_else(|| stated.areas.get(1).cloned()),
        street: stated.street.clone().or_else(|| own(Layer::Street)),
        postal_code: stated.postcode.clone(),
        house_number: stated.housenumber.clone(),
        unit: stated.unit.clone(),
    }
}

impl Item {
    /// The crow-flies distance from the search center to the result, in
    /// whole meters, as HERE counts it.
    pub(super) fn distance_from(mut self, center: Option<LatLng>) -> Self {
        if let (Some(center), Some(position)) = (center, self.position) {
            let meters = haversine_m(center.lng, center.lat, position.lng, position.lat);
            self.distance = Some(meters.round() as i64);
        }
        self
    }

    pub(super) fn without_scoring(mut self) -> Self {
        self.scoring = None;
        self
    }

    pub(super) fn without_position(mut self) -> Self {
        self.position = None;
        self
    }

    /// Whether a `types` value admits the item, by HERE's definitions.
    pub(super) fn is_of(&self, kind: &str) -> bool {
        let result_type = self.result_type.unwrap_or_default();
        match kind {
            "address" => matches!(
                result_type,
                "houseNumber" | "street" | "postalCodePoint" | "intersection" | "addressBlock"
            ),
            "area" => matches!(result_type, "locality" | "administrativeArea"),
            "city" => result_type == "locality" && self.locality_type == Some("city"),
            "postalCode" => {
                result_type == "postalCodePoint"
                    || (result_type == "locality" && self.locality_type == Some("postalCode"))
            }
            other => result_type == other,
        }
    }

    pub(super) fn is_any_of(&self, kinds: Option<&[String]>) -> bool {
        kinds.is_none_or(|kinds| kinds.iter().any(|kind| self.is_of(kind)))
    }
}
