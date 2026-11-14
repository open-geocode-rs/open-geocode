//! og's results in Google's response shapes. A field og has no value for is
//! left out, as Google leaves out a field it has no data for.

use serde::Serialize;

use crate::{
    api::{Accuracy, Common, ContextArea},
    compat::{self, encode_id},
    record::Layer,
};

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LatLng {
    pub(super) latitude: f64,
    pub(super) longitude: f64,
}

impl LatLng {
    fn of(coordinates: [f64; 2]) -> Self {
        Self {
            latitude: coordinates[1],
            longitude: coordinates[0],
        }
    }
}

// `languageCode` is left out, since og does not know the language a name is in.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct LocalizedText {
    pub(super) text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AddressComponent {
    pub(super) long_text: String,
    pub(super) short_text: String,
    pub(super) types: Vec<&'static str>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PostalAddress {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) region_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) postal_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) administrative_area: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) locality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) sublocality: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) address_lines: Vec<String>,
}

/// A Places API `Place`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Place {
    pub(super) name: String,
    pub(super) id: String,
    pub(super) display_name: LocalizedText,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) types: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) primary_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) formatted_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) postal_address: Option<PostalAddress>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) address_components: Vec<AddressComponent>,
    pub(super) location: LatLng,
}

impl Place {
    pub(super) fn of(common: &Common, coordinates: [f64; 2]) -> Self {
        let id = encode_id(&common.id);
        let kind = Kind::of(common);
        Self {
            name: format!("places/{id}"),
            id,
            display_name: LocalizedText {
                text: common.name.clone(),
            },
            types: kind.types(),
            primary_type: kind.primary(),
            formatted_address: formatted_address(common),
            postal_address: postal_address(common),
            address_components: address_components(common),
            location: LatLng::of(coordinates),
        }
    }
}

/// A Geocoding API v4 `GeocodeResult`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GeocodeResult {
    pub(super) place: String,
    pub(super) place_id: String,
    pub(super) location: LatLng,
    pub(super) granularity: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) formatted_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) postal_address: Option<PostalAddress>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) address_components: Vec<AddressComponent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) types: Vec<&'static str>,
}

impl GeocodeResult {
    pub(super) fn of(common: &Common, coordinates: [f64; 2]) -> Self {
        let place_id = encode_id(&common.id);
        Self {
            // Every v4 response Google documents writes the full resource
            // name, host included, though its reference says `places/{id}`.
            place: format!("//places.googleapis.com/places/{place_id}"),
            place_id,
            location: LatLng::of(coordinates),
            granularity: granularity(common.accuracy),
            formatted_address: formatted_address(common),
            postal_address: postal_address(common),
            address_components: address_components(common),
            types: Kind::of(common).types(),
        }
    }
}

pub(super) const fn granularity(accuracy: Accuracy) -> &'static str {
    match accuracy {
        Accuracy::Rooftop => "ROOFTOP",
        Accuracy::Interpolated => "RANGE_INTERPOLATED",
        // og's street point is a point on the street's own geometry, its
        // nearest equivalent of Google's centre of a feature.
        Accuracy::Centroid | Accuracy::Street => "GEOMETRIC_CENTER",
        Accuracy::Approximate => "APPROXIMATE",
    }
}

/// The text after a result's name in its label, which is a POI's address
/// or where an address or area lies. `None` when the label is the name alone.
pub(super) fn after_name(common: &Common) -> Option<String> {
    let rest = common.label.strip_prefix(common.name.as_str())?;
    let rest = rest.strip_prefix(", ")?;
    (!rest.is_empty()).then(|| rest.to_string())
}

/// A POI's address is its label after its name; for anything else the label
/// is the address.
fn formatted_address(common: &Common) -> Option<String> {
    if common.layer == Layer::Poi {
        return after_name(common);
    }
    (!common.label.is_empty()).then(|| common.label.clone())
}

/// The house number and street (or `addr:place`) as one line, joined as og
/// joins them in an address's own name.
fn street_line(common: &Common) -> Option<String> {
    let address = &common.address;
    let parts = [
        address.housenumber.as_deref(),
        address.street.as_deref().or(address.place.as_deref()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// The postal address as the source states it, with the areas og found the
/// result in standing in for a part the source leaves out.
fn postal_address(common: &Common) -> Option<PostalAddress> {
    let address = &common.address;
    let context = &common.context;
    let area = |area: &Option<ContextArea>| area.as_ref().map(|area| area.name.clone());
    let postal = PostalAddress {
        region_code: compat::country_code(context).map(str::to_string),
        postal_code: address.postcode.clone(),
        administrative_area: address.region.clone().or_else(|| area(&context.region)),
        locality: address.locality.clone().or_else(|| area(&context.locality)),
        // `areas` is outermost first, as an envelope lists them.
        sublocality: if address.areas.is_empty() {
            area(&context.sublocality)
        } else {
            Some(address.areas.join(", "))
        },
        address_lines: street_line(common)
            .into_iter()
            .chain(address.unit.clone())
            .collect(),
    };
    (postal != PostalAddress::default()).then_some(postal)
}

fn component(text: &str, types: &[&'static str]) -> AddressComponent {
    AddressComponent {
        long_text: text.to_string(),
        short_text: text.to_string(),
        types: types.to_vec(),
    }
}

/// Google's address components, most specific first. Areas come from the
/// areas og found the result in, falling back to the part the source states.
fn address_components(common: &Common) -> Vec<AddressComponent> {
    const LOWER_ADMIN: [&str; 3] = [
        "administrative_area_level_2",
        "administrative_area_level_3",
        "administrative_area_level_4",
    ];
    let address = &common.address;
    let context = &common.context;
    fn name(area: &Option<ContextArea>) -> Option<&str> {
        area.as_ref().map(|area| area.name.as_str())
    }
    let mut components = Vec::new();
    let mut push = |text: Option<&str>, types: &[&'static str]| {
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            components.push(component(text, types));
        }
    };
    push(address.unit.as_deref(), &["subpremise"]);
    push(address.housenumber.as_deref(), &["street_number"]);
    push(address.street.as_deref(), &["route"]);
    push(name(&context.neighbourhood), &["neighborhood", "political"]);
    push(name(&context.sublocality), &["sublocality", "political"]);
    push(
        name(&context.locality).or(address.locality.as_deref()),
        &["locality", "political"],
    );
    // Google numbers the levels below the region by order, not by name, so
    // the first one present is level 2. og's tiers are sparse, so a country that
    // maps only `district` gets it as level 2, as Google would number it.
    let lower = [&context.subregion, &context.district, &context.subdistrict]
        .into_iter()
        .filter_map(|area| name(area))
        .collect::<Vec<_>>();
    for (level, text) in lower.iter().enumerate().rev() {
        push(Some(text), &[LOWER_ADMIN[level], "political"]);
    }
    // The region's short text is its code within its country (`ON` of
    // `CA-ON`) when og knows it.
    match &context.region {
        Some(region) => components.push(AddressComponent {
            long_text: region.name.clone(),
            short_text: region
                .code
                .as_deref()
                .and_then(|code| code.split_once('-'))
                .map_or_else(|| region.name.clone(), |(_, own)| own.to_string()),
            types: vec!["administrative_area_level_1", "political"],
        }),
        None => push(
            address.region.as_deref(),
            &["administrative_area_level_1", "political"],
        ),
    }
    match &context.country {
        Some(country) => components.push(AddressComponent {
            long_text: country.name.clone(),
            short_text: country.code.clone().unwrap_or_else(|| country.name.clone()),
            types: vec!["country", "political"],
        }),
        None => {
            if let Some(country) = address.country.as_deref().filter(|text| !text.is_empty()) {
                components.push(component(country, &["country", "political"]));
            }
        }
    }
    if let Some(postcode) = address.postcode.as_deref() {
        components.push(component(postcode, &["postal_code"]));
    }
    components
}

/// What a result is, in Google's place types. Partial on purpose, so a
/// layer or POI category gets types only where Google's type means the same
/// thing, and nothing otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Types(&'static [&'static str]),
    Poi(Option<&'static str>),
    Unknown,
}

impl Kind {
    pub(super) fn of(common: &Common) -> Self {
        match common.layer {
            Layer::Address | Layer::Interpolation => Self::Types(&["street_address"]),
            Layer::Street => Self::Types(&["route"]),
            Layer::Neighbourhood => Self::Types(&["neighborhood", "political"]),
            Layer::Locality => Self::Types(&["locality", "political"]),
            Layer::Region => Self::Types(&["administrative_area_level_1", "political"]),
            Layer::Country => Self::Types(&["country", "political"]),
            Layer::Postcode => Self::Types(&["postal_code"]),
            Layer::Poi => Self::Poi(common.category.as_deref().and_then(category_type)),
            // og's district holds every tier between region and locality,
            // so which administrative level it is varies by country.
            Layer::District | Layer::Place => Self::Unknown,
        }
    }

    pub(super) fn types(self) -> Vec<&'static str> {
        match self {
            Self::Types(types) => types.to_vec(),
            Self::Poi(category) => category
                .into_iter()
                .chain(["point_of_interest", "establishment"])
                .collect(),
            Self::Unknown => Vec::new(),
        }
    }

    /// Google's word for what the place is, as `primaryType`. Never a generic
    /// type such as `establishment`.
    pub(super) fn primary(self) -> Option<&'static str> {
        match self {
            Self::Poi(category) => category,
            Self::Types(types) => types.first().copied(),
            Self::Unknown => None,
        }
    }
}

/// og's POI category (`key:value`) to Google's Table A type of the same
/// meaning.
fn category_type(category: &str) -> Option<&'static str> {
    Some(match category {
        "amenity:atm" => "atm",
        "amenity:bank" => "bank",
        "amenity:bar" => "bar",
        "amenity:cafe" => "cafe",
        "amenity:cinema" => "movie_theater",
        "amenity:dentist" => "dentist",
        "amenity:doctors" => "doctor",
        "amenity:fast_food" => "fast_food_restaurant",
        "amenity:fire_station" => "fire_station",
        "amenity:fuel" => "gas_station",
        "amenity:hospital" => "hospital",
        "amenity:library" => "library",
        "amenity:parking" => "parking",
        "amenity:pharmacy" => "pharmacy",
        "amenity:police" => "police",
        "amenity:post_office" => "post_office",
        "amenity:pub" => "pub",
        "amenity:restaurant" => "restaurant",
        "amenity:school" => "school",
        "amenity:university" => "university",
        "leisure:park" => "park",
        "shop:bakery" => "bakery",
        "shop:convenience" => "convenience_store",
        "shop:supermarket" => "supermarket",
        "tourism:hotel" => "hotel",
        "tourism:museum" => "museum",
        _ => return None,
    })
}
