//! og's results in Google's response shapes. A field og has no value for is
//! left out, as Google leaves out a field it has no data for.

use serde::Serialize;

use super::{hours, language::Language};
use crate::{
    api::{Accuracy, Common, ContextArea},
    compat::{self, encode_id},
    record::{AccessPoint, Layer},
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

/// Google's `Viewport`, the box `[west, south, east, north]` og holds. A
/// box across the antimeridian keeps `west` greater than `east`, as Google
/// writes one.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(super) struct Viewport {
    pub(super) low: LatLng,
    pub(super) high: LatLng,
}

impl Viewport {
    fn of(bbox: Option<[f64; 4]>) -> Option<Self> {
        bbox.map(|[west, south, east, north]| Self {
            low: LatLng::of([west, south]),
            high: LatLng::of([east, north]),
        })
    }

    fn geocode(common: &Common, [lon, lat]: [f64; 2]) -> Option<Self> {
        Self::of(common.bbox).or_else(|| {
            if !matches!(common.layer, Layer::Address | Layer::Interpolation) {
                return None;
            }
            // A display window for an address represented only by a point.
            // It has no bearing on the record geometry or its accuracy.
            const HALF_SPAN: f64 = 0.00135;
            let wrap = |longitude: f64| {
                if longitude < -180.0 {
                    longitude + 360.0
                } else if longitude > 180.0 {
                    longitude - 360.0
                } else {
                    longitude
                }
            };
            Self::of(Some([
                wrap(lon - HALF_SPAN),
                (lat - HALF_SPAN).max(-90.0),
                wrap(lon + HALF_SPAN),
                (lat + HALF_SPAN).min(90.0),
            ]))
        })
    }
}

/// Google's `timeZone`. og knows the IANA name, not which release of the
/// database it is from, so `version` is left out.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct TimeZone {
    pub(super) id: String,
}

/// A Place's `entrances` entry, an entrance og maps for it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct PlaceEntrance {
    pub(super) location: LatLng,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) tags: Vec<&'static str>,
}

/// `PREFERRED` marks an entrance into the main part of the place, which is
/// what an `entrance=main` tag states. Any other entrance is untagged.
fn entrance_tags(access: &AccessPoint) -> Vec<&'static str> {
    if access.kind == "main" {
        vec!["PREFERRED"]
    } else {
        Vec::new()
    }
}

/// A phone number as its source writes it. One written with its country
/// code (`+1 401 ...`) is the international form; any other is the form
/// dialled inside its country.
fn phones(common: &Common) -> (Option<String>, Option<String>) {
    let Some(phone) = common
        .contact
        .as_ref()
        .and_then(|contact| contact.phone.as_deref())
        .map(str::trim)
        .filter(|phone| !phone.is_empty())
    else {
        return (None, None);
    };
    match phone.starts_with('+') {
        true => (None, Some(phone.to_string())),
        false => (Some(phone.to_string()), None),
    }
}

/// `languageCode` is set only when og holds the name in the language asked.
/// A local name is in a language og does not know, so it goes without one.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LocalizedText {
    pub(super) text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) language_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AddressComponent {
    pub(super) long_text: String,
    pub(super) short_text: String,
    pub(super) types: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) language_code: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PostalAddress {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) region_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) language_code: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) viewport: Option<Viewport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) national_phone_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) international_phone_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) website_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) regular_opening_hours: Option<hours::OpeningHours>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) time_zone: Option<TimeZone>,
    /// The place's offset from UTC now, in minutes, daylight saving time
    /// included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) utc_offset_minutes: Option<i32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) entrances: Vec<PlaceEntrance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) address_descriptor: Option<AddressDescriptor>,
}

/// Where a place is, told by the places near it and the areas holding it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct AddressDescriptor {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) landmarks: Vec<Landmark>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) areas: Vec<DescriptorArea>,
}

/// A place near another, and how it stands to it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Landmark {
    pub(super) name: String,
    pub(super) place_id: String,
    pub(super) display_name: LocalizedText,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) types: Vec<&'static str>,
    /// `WITHIN` where the place lies inside the landmark's outline. Near,
    /// Google's default, is left out, as Google leaves it out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) spatial_relationship: Option<&'static str>,
    pub(super) straight_line_distance_meters: f64,
}

/// An area holding a place.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DescriptorArea {
    pub(super) name: String,
    pub(super) place_id: String,
    pub(super) display_name: LocalizedText,
    pub(super) containment: &'static str,
}

impl AddressDescriptor {
    /// The descriptor of the place at `point`, from the landmarks og found
    /// near it and the areas below its town that hold it, smallest first.
    pub(super) fn of(
        point: [f64; 2],
        common: &Common,
        landmarks: &[crate::api::ForwardFeature],
        language: &Language,
    ) -> Self {
        let landmarks = landmarks
            .iter()
            .map(|feature| {
                let found = &feature.properties.common;
                let [lon, lat] = feature.geometry.coordinates;
                let place_id = encode_id(found);
                let within = found.bbox.is_some_and(|[west, south, east, north]| {
                    (west..=east).contains(&point[0]) && (south..=north).contains(&point[1])
                });
                Landmark {
                    name: format!("places/{place_id}"),
                    place_id,
                    display_name: LocalizedText {
                        text: found.name.clone(),
                        language_code: language.of(found.record_id, &found.context),
                    },
                    types: Kind::of(found).types(),
                    spatial_relationship: within.then_some("WITHIN"),
                    straight_line_distance_meters: crate::spatial_index::haversine_m(
                        point[0], point[1], lon, lat,
                    ),
                }
            })
            .collect();
        let context = &common.context;
        let areas = [&context.neighbourhood, &context.sublocality]
            .into_iter()
            .flatten()
            .map(|area| {
                let place_id = compat::encode_area_id(area);
                DescriptorArea {
                    name: format!("places/{place_id}"),
                    place_id,
                    display_name: LocalizedText {
                        text: area.name.clone(),
                        language_code: language.of(Some(area.record_id), context),
                    },
                    containment: "WITHIN",
                }
            })
            .collect();
        Self { landmarks, areas }
    }
}

impl Place {
    pub(super) fn of(common: &Common, coordinates: [f64; 2], language: &Language) -> Self {
        let id = encode_id(common);
        let kind = Kind::of(common);
        let (postal_address, formatted_address) = address_fields(common, language);
        Self {
            name: format!("places/{id}"),
            id,
            display_name: LocalizedText {
                text: common.name.clone(),
                language_code: language.of(common.record_id, &common.context),
            },
            types: kind.types(),
            primary_type: kind.primary(),
            formatted_address,
            postal_address,
            address_components: address_components(common, language),
            location: LatLng::of(coordinates),
            viewport: Viewport::of(common.bbox),
            national_phone_number: phones(common).0,
            international_phone_number: phones(common).1,
            website_uri: common
                .contact
                .as_ref()
                .and_then(|contact| contact.website.clone()),
            regular_opening_hours: common.opening_hours.as_deref().and_then(hours::parse),
            time_zone: common.timezone.clone().map(|id| TimeZone { id }),
            utc_offset_minutes: common
                .timezone
                .as_deref()
                .and_then(crate::compat::time_zone::Offset::now)
                .map(crate::compat::time_zone::Offset::minutes),
            address_descriptor: None,
            entrances: common
                .access
                .iter()
                .map(|access| PlaceEntrance {
                    location: LatLng::of([access.lon, access.lat]),
                    tags: entrance_tags(access),
                })
                .collect(),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) viewport: Option<Viewport>,
    /// The whole area's box, which Google sends for an area and not for an
    /// address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) bounds: Option<Viewport>,
}

impl GeocodeResult {
    pub(super) fn of(common: &Common, coordinates: [f64; 2], language: &Language) -> Self {
        let place_id = encode_id(common);
        let (postal_address, formatted_address) = address_fields(common, language);
        Self {
            place: format!("places/{place_id}"),
            place_id,
            location: LatLng::of(coordinates),
            granularity: granularity(common),
            formatted_address,
            postal_address,
            address_components: address_components(common, language),
            types: Kind::of(common).types(),
            viewport: Viewport::geocode(common, coordinates),
            bounds: Viewport::of(common.bbox).filter(|_| common.layer.is_context()),
        }
    }
}

/// A Geocoding API v4 `PlaceView`, the place a destination is or holds.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PlaceView {
    pub(super) place: String,
    pub(super) place_id: String,
    pub(super) display_name: LocalizedText,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) primary_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) types: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) formatted_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) postal_address: Option<PostalAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) structure_type: Option<&'static str>,
    pub(super) location: LatLng,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) address_components: Vec<AddressComponent>,
}

impl PlaceView {
    pub(super) fn of(common: &Common, coordinates: [f64; 2], language: &Language) -> Self {
        let place_id = encode_id(common);
        let kind = Kind::of(common);
        let (postal_address, formatted_address) = address_fields(common, language);
        // A destination's address leads with a POI's name.
        let formatted_address = match common.layer {
            Layer::Poi => formatted_address.map(|address| format!("{}, {address}", common.name)),
            _ => formatted_address,
        };
        Self {
            // Destinations write the resource name without the host, unlike
            // the other v4 methods.
            place: format!("places/{place_id}"),
            place_id,
            display_name: LocalizedText {
                text: common.name.clone(),
                language_code: language.of(common.record_id, &common.context),
            },
            primary_type: kind.primary(),
            types: kind.types(),
            formatted_address,
            postal_address,
            // og holds only a point for a result without a box. One with a
            // box may be a building or grounds, which og cannot tell apart.
            structure_type: common.bbox.is_none().then_some("POINT"),
            location: LatLng::of(coordinates),
            address_components: address_components(common, language),
        }
    }
}

/// A destination's `Entrance`, one of the entrances og maps for a place.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Entrance {
    pub(super) location: LatLng,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) tags: Vec<&'static str>,
    pub(super) place: String,
}

impl Entrance {
    pub(super) fn of(access: &AccessPoint, place: &str) -> Self {
        Self {
            location: LatLng::of([access.lon, access.lat]),
            tags: entrance_tags(access),
            place: place.to_string(),
        }
    }
}

/// Google's `granularity` for what og's point stands for. An address mapped
/// as an outline stands at the outline's centre, the house itself, which
/// is Google's street-address precision.
pub(super) fn granularity(common: &Common) -> &'static str {
    match common.accuracy {
        Accuracy::Rooftop => "ROOFTOP",
        Accuracy::Centroid if common.layer == Layer::Address => "ROOFTOP",
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

/// The postal address, and the formatted address: a house or a place's
/// postal lines in its country's form, one line after another, else og's
/// label. Google writes the United States as `USA` there.
fn address_fields(common: &Common, language: &Language) -> (Option<PostalAddress>, Option<String>) {
    let mut postal = postal_address(common);
    if let Some(postal) = postal.as_mut() {
        // The address names its town in the language its town is named in.
        let town = common.context.locality.as_ref().map(|town| town.record_id);
        postal.language_code = language.of(town, &common.context);
    }
    if is_us(common)
        && let Some(postal) = postal.as_mut()
        && let Some(state) = us_state_code(common, postal)
    {
        postal.administrative_area = Some(state);
    }
    let mut lines = match common.layer {
        Layer::Address | Layer::Poi => {
            compat::postal_lines(common.layer, &common.address, &common.context)
        }
        _ => Vec::new(),
    };
    if is_us(common)
        && let Some(country) = lines.last_mut()
    {
        *country = "USA".to_string();
    }
    let formatted = (lines.len() > 1).then(|| lines.join(", "));
    (postal, formatted.or_else(|| formatted_address(common)))
}

fn is_us(common: &Common) -> bool {
    compat::country_code(&common.context) == Some("US")
}

/// A US state's two-letter code, which Google writes as its administrative
/// area: the code of the state holding the house, else a code the source
/// states.
fn us_state_code(common: &Common, postal: &PostalAddress) -> Option<String> {
    common
        .context
        .region
        .as_ref()
        .and_then(|area| area.code.as_deref())
        .and_then(|code| code.strip_prefix("US-"))
        .or_else(|| {
            postal.administrative_area.as_deref().filter(|code| {
                code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_uppercase())
            })
        })
        .map(str::to_owned)
}

/// The house number and street (or `addr:place`) as one line, joined as og
/// joins them in an address's own name. A US street is abbreviated.
fn street_line(common: &Common) -> Option<String> {
    let address = &common.address;
    let street = is_us(common)
        .then(|| compat::us_street(address))
        .flatten()
        .or_else(|| address.street.clone());
    let parts = [
        address.housenumber.as_deref(),
        street.as_deref().or(address.place.as_deref()),
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
        language_code: None,
        postal_code: address.postcode.clone(),
        administrative_area: address.region.clone().or_else(|| area(&context.region)),
        locality: compat::town_of(address, context).map(str::to_string),
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
        language_code: None,
    }
}

/// Google's address components, most specific first. Areas come from the
/// areas og found the result in, falling back to the part the source states.
fn address_components(common: &Common, language: &Language) -> Vec<AddressComponent> {
    const LOWER_ADMIN: [&str; 3] = [
        "administrative_area_level_2",
        "administrative_area_level_3",
        "administrative_area_level_4",
    ];
    let address = &common.address;
    let context = &common.context;
    // A name says which language it is in. A number or a code is in none.
    let area = |area: &ContextArea, types: &[&'static str]| AddressComponent {
        language_code: language.of(Some(area.record_id), context),
        ..component(&area.name, types)
    };
    let local = language.local(context);
    let named = |text: &str, types: &[&'static str]| AddressComponent {
        language_code: local.clone(),
        ..component(text, types)
    };
    let mut components = Vec::new();
    let mut push = |text: Option<&str>, types: &[&'static str]| {
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            components.push(component(text, types));
        }
    };
    push(address.unit.as_deref(), &["subpremise"]);
    push(address.housenumber.as_deref(), &["street_number"]);
    if let Some(street) = address.street.as_deref().filter(|text| !text.is_empty()) {
        components.push(AddressComponent {
            short_text: is_us(common)
                .then(|| compat::us_street(address))
                .flatten()
                .unwrap_or_else(|| street.to_string()),
            ..named(street, &["route"])
        });
    }
    let holding = [
        (&context.neighbourhood, &["neighborhood", "political"][..]),
        (&context.sublocality, &["sublocality", "political"][..]),
    ];
    for (found, types) in holding {
        if let Some(found) = found.as_ref().filter(|found| !found.name.is_empty()) {
            components.push(area(found, types));
        }
    }
    match context
        .locality
        .as_ref()
        .filter(|found| !found.name.is_empty())
    {
        Some(locality) => components.push(area(locality, &["locality", "political"])),
        None => {
            if let Some(text) = address.locality.as_deref().filter(|text| !text.is_empty()) {
                components.push(named(text, &["locality", "political"]));
            }
        }
    }
    // Google numbers the levels below the region by order, not by name, so
    // the first one present is level 2. og's tiers are sparse, so a country that
    // maps only `district` gets it as level 2, as Google would number it.
    let lower = [&context.subregion, &context.district, &context.subdistrict]
        .into_iter()
        .filter_map(Option::as_ref)
        .filter(|found| !found.name.is_empty())
        .collect::<Vec<_>>();
    for (level, found) in lower.iter().enumerate().rev() {
        components.push(area(found, &[LOWER_ADMIN[level], "political"]));
    }
    // The region's short text is its code within its country (`ON` of
    // `CA-ON`) when og knows it.
    match &context.region {
        Some(region) => components.push(AddressComponent {
            short_text: region_short(region.code.as_deref(), &region.name),
            ..area(region, &["administrative_area_level_1", "political"])
        }),
        // A region result is not in its own context, so it is its own
        // component, with its own code.
        None if common.layer == Layer::Region => components.push(AddressComponent {
            short_text: region_short(common.code.as_deref(), &common.name),
            language_code: language.of(common.record_id, context),
            ..component(&common.name, &["administrative_area_level_1", "political"])
        }),
        None => {
            if let Some(text) = address.region.as_deref().filter(|text| !text.is_empty()) {
                components.push(named(text, &["administrative_area_level_1", "political"]));
            }
        }
    }
    match &context.country {
        Some(country) => components.push(AddressComponent {
            short_text: country.code.clone().unwrap_or_else(|| country.name.clone()),
            ..area(country, &["country", "political"])
        }),
        None if common.layer == Layer::Country => components.push(AddressComponent {
            short_text: common.code.clone().unwrap_or_else(|| common.name.clone()),
            language_code: language.of(common.record_id, context),
            ..component(&common.name, &["country", "political"])
        }),
        None => {
            if let Some(country) = address.country.as_deref().filter(|text| !text.is_empty()) {
                components.push(named(country, &["country", "political"]));
            }
        }
    }
    if let Some(postcode) = address.postcode.as_deref() {
        components.push(component(postcode, &["postal_code"]));
    }
    components
}

/// A region's short text, its code within its country (`ON` of `CA-ON`)
/// when og knows it, else its name.
fn region_short(code: Option<&str>, name: &str) -> String {
    code.and_then(|code| code.split_once('-'))
        .map_or_else(|| name.to_string(), |(_, own)| own.to_string())
}

/// What a result is, in Google's place types. Partial on purpose, so a
/// layer or POI category gets types only where Google's type means the same
/// thing, and nothing otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Types(&'static [&'static str]),
    /// A place: Google's type of its category, and whether OSM maps it as a
    /// shop, which Google writes `store` whatever its type.
    Poi {
        primary: Option<&'static str>,
        shop: bool,
        /// Google's types of the kinds of food it serves
        /// (`pizza_restaurant`), as its `cuisine` lists them, each by where
        /// it stands in [`RESTAURANT_TYPES`].
        cuisine: [Option<u8>; CUISINE_TYPES],
    },
    Unknown,
}

/// The most types of food one place's `cuisine` adds to its types.
const CUISINE_TYPES: usize = 4;

impl Kind {
    pub(super) fn of(common: &Common) -> Self {
        match common.layer {
            Layer::Poi => {
                let category = common.category.as_deref();
                let mut cuisine = [None; CUISINE_TYPES];
                let mut held = common
                    .cuisine
                    .iter()
                    .filter_map(|value| cuisine_type(value));
                for slot in &mut cuisine {
                    *slot = held.next();
                }
                Self::Poi {
                    primary: category.and_then(category_type),
                    shop: category.is_some_and(|category| category.starts_with("shop:")),
                    cuisine,
                }
            }
            layer => Self::of_layer(layer),
        }
    }

    /// What a result of `layer` is, for any layer but a POI's, which its
    /// category says.
    fn of_layer(layer: Layer) -> Self {
        match layer {
            Layer::Address | Layer::Interpolation => Self::Types(&["street_address"]),
            Layer::Street => Self::Types(&["route"]),
            Layer::Neighbourhood => Self::Types(&["neighborhood", "political"]),
            Layer::Locality => Self::Types(&["locality", "political"]),
            Layer::Region => Self::Types(&["administrative_area_level_1", "political"]),
            Layer::Country => Self::Types(&["country", "political"]),
            Layer::Postcode => Self::Types(&["postal_code"]),
            // og's district holds every tier between region and locality,
            // so which administrative level it is varies by country. It is
            // an administrative area all the same. A POI is what its
            // category says, which only `of` reads.
            Layer::District => Self::Types(&["political"]),
            Layer::Place | Layer::Poi => Self::Unknown,
        }
    }

    pub(super) fn types(self) -> Vec<&'static str> {
        match self {
            Self::Types(types) => types.to_vec(),
            Self::Poi {
                primary,
                shop,
                cuisine,
            } => {
                let mut types = primary.into_iter().collect::<Vec<_>>();
                for group in cuisine
                    .into_iter()
                    .flatten()
                    .map(|at| RESTAURANT_TYPES[usize::from(at)])
                    .chain(primary.map_or(&[][..], groups_of).iter().copied())
                    .chain(shop.then_some("store"))
                {
                    if !types.contains(&group) {
                        types.push(group);
                    }
                }
                types.extend(ANY_PLACE);
                types
            }
            Self::Unknown => Vec::new(),
        }
    }

    /// Google's word for what the place is, as `primaryType`. Never a generic
    /// type such as `establishment`.
    pub(super) fn primary(self) -> Option<&'static str> {
        match self {
            Self::Poi { primary, .. } => primary,
            Self::Types(types) => types.first().copied().filter(|kind| *kind != "political"),
            Self::Unknown => None,
        }
    }
}

/// Google's type of a kind of food, as OpenStreetMap's `cuisine` names it,
/// by where it stands in [`RESTAURANT_TYPES`]. It is the restaurant type of
/// the same name (`pizza` is `pizza_restaurant`), when Google has one.
fn cuisine_type(value: &str) -> Option<u8> {
    let wanted = format!("{}_restaurant", value.trim().replace(' ', "_"));
    RESTAURANT_TYPES
        .iter()
        .position(|named| *named == wanted)
        .and_then(|at| u8::try_from(at).ok())
}

/// Google's restaurant types, as its place type tables list them.
const RESTAURANT_TYPES: [&str; 116] = [
    "afghani_restaurant",
    "african_restaurant",
    "american_restaurant",
    "argentinian_restaurant",
    "asian_fusion_restaurant",
    "asian_restaurant",
    "australian_restaurant",
    "austrian_restaurant",
    "bangladeshi_restaurant",
    "barbecue_restaurant",
    "basque_restaurant",
    "bavarian_restaurant",
    "belgian_restaurant",
    "brazilian_restaurant",
    "breakfast_restaurant",
    "british_restaurant",
    "brunch_restaurant",
    "buffet_restaurant",
    "burmese_restaurant",
    "burrito_restaurant",
    "cajun_restaurant",
    "californian_restaurant",
    "cambodian_restaurant",
    "cantonese_restaurant",
    "caribbean_restaurant",
    "chicken_restaurant",
    "chicken_wings_restaurant",
    "chilean_restaurant",
    "chinese_noodle_restaurant",
    "chinese_restaurant",
    "colombian_restaurant",
    "croatian_restaurant",
    "cuban_restaurant",
    "czech_restaurant",
    "danish_restaurant",
    "dessert_restaurant",
    "dim_sum_restaurant",
    "dumpling_restaurant",
    "dutch_restaurant",
    "eastern_european_restaurant",
    "ethiopian_restaurant",
    "european_restaurant",
    "falafel_restaurant",
    "family_restaurant",
    "fast_food_restaurant",
    "filipino_restaurant",
    "fine_dining_restaurant",
    "fish_and_chips_restaurant",
    "fondue_restaurant",
    "french_restaurant",
    "fusion_restaurant",
    "german_restaurant",
    "greek_restaurant",
    "gyro_restaurant",
    "halal_restaurant",
    "hamburger_restaurant",
    "hawaiian_restaurant",
    "hot_dog_restaurant",
    "hot_pot_restaurant",
    "hungarian_restaurant",
    "indian_restaurant",
    "indonesian_restaurant",
    "irish_restaurant",
    "israeli_restaurant",
    "italian_restaurant",
    "japanese_curry_restaurant",
    "japanese_izakaya_restaurant",
    "japanese_restaurant",
    "korean_barbecue_restaurant",
    "korean_restaurant",
    "latin_american_restaurant",
    "lebanese_restaurant",
    "malaysian_restaurant",
    "mediterranean_restaurant",
    "mexican_restaurant",
    "middle_eastern_restaurant",
    "mongolian_barbecue_restaurant",
    "moroccan_restaurant",
    "north_indian_restaurant",
    "oyster_bar_restaurant",
    "pakistani_restaurant",
    "persian_restaurant",
    "peruvian_restaurant",
    "pizza_restaurant",
    "polish_restaurant",
    "portuguese_restaurant",
    "ramen_restaurant",
    "romanian_restaurant",
    "russian_restaurant",
    "scandinavian_restaurant",
    "seafood_restaurant",
    "shawarma_restaurant",
    "soul_food_restaurant",
    "soup_restaurant",
    "south_american_restaurant",
    "south_indian_restaurant",
    "southwestern_us_restaurant",
    "spanish_restaurant",
    "sri_lankan_restaurant",
    "sushi_restaurant",
    "swiss_restaurant",
    "taco_restaurant",
    "taiwanese_restaurant",
    "tapas_restaurant",
    "tex_mex_restaurant",
    "thai_restaurant",
    "tibetan_restaurant",
    "tonkatsu_restaurant",
    "turkish_restaurant",
    "ukrainian_restaurant",
    "vegan_restaurant",
    "vegetarian_restaurant",
    "vietnamese_restaurant",
    "western_restaurant",
    "yakiniku_restaurant",
    "yakitori_restaurant",
];

/// og's POI categories, as the catalog writes them, each beside Google's
/// Table A type of the same meaning.
const CATEGORY_TYPES: &[(&str, &str)] = &[
    ("aeroway aerodrome", "airport"),
    ("amenity arts_centre", "cultural_center"),
    ("amenity atm", "atm"),
    ("amenity bank", "bank"),
    ("amenity bar", "bar"),
    ("amenity bus_station", "bus_station"),
    ("amenity cafe", "cafe"),
    ("amenity car_rental", "car_rental"),
    ("amenity car_wash", "car_wash"),
    ("amenity casino", "casino"),
    (
        "amenity charging_station",
        "electric_vehicle_charging_station",
    ),
    ("amenity childcare", "child_care_agency"),
    ("amenity cinema", "movie_theater"),
    ("amenity community_centre", "community_center"),
    ("amenity conference_centre", "convention_center"),
    ("amenity courthouse", "courthouse"),
    ("amenity dentist", "dentist"),
    ("amenity doctors", "doctor"),
    ("amenity embassy", "embassy"),
    ("amenity events_venue", "event_venue"),
    ("amenity fast_food", "fast_food_restaurant"),
    ("amenity ferry_terminal", "ferry_terminal"),
    ("amenity fire_station", "fire_station"),
    ("amenity food_court", "food_court"),
    ("amenity fuel", "gas_station"),
    ("amenity funeral_hall", "funeral_home"),
    ("amenity hospital", "hospital"),
    ("amenity ice_cream", "ice_cream_shop"),
    ("amenity kindergarten", "preschool"),
    ("amenity library", "library"),
    ("amenity marketplace", "market"),
    ("amenity nightclub", "night_club"),
    ("amenity parking", "parking"),
    ("amenity pharmacy", "pharmacy"),
    ("amenity place_of_worship", "place_of_worship"),
    ("amenity police", "police"),
    ("amenity post_office", "post_office"),
    ("amenity pub", "pub"),
    ("amenity restaurant", "restaurant"),
    ("amenity school", "school"),
    ("amenity taxi", "taxi_stand"),
    ("amenity theatre", "performing_arts_theater"),
    ("amenity townhall", "city_hall"),
    ("amenity university", "university"),
    ("amenity veterinary", "veterinary_care"),
    ("craft electrician", "electrician"),
    ("highway bus_stop", "bus_stop"),
    ("craft locksmith", "locksmith"),
    ("craft painter", "painter"),
    ("craft plumber", "plumber"),
    ("craft roofer", "roofing_contractor"),
    ("healthcare dentist", "dentist"),
    ("healthcare laboratory", "medical_lab"),
    ("healthcare physiotherapist", "physiotherapist"),
    ("leisure bowling_alley", "bowling_alley"),
    ("leisure dog_park", "dog_park"),
    ("leisure fitness_centre", "gym"),
    ("leisure garden", "garden"),
    ("leisure golf_course", "golf_course"),
    ("leisure ice_rink", "ice_skating_rink"),
    ("leisure marina", "marina"),
    ("leisure park", "park"),
    ("leisure playground", "playground"),
    ("leisure sports_centre", "sports_complex"),
    ("leisure stadium", "stadium"),
    ("leisure swimming_pool", "swimming_pool"),
    ("leisure water_park", "water_park"),
    ("office accountant", "accounting"),
    ("office estate_agent", "real_estate_agency"),
    ("office insurance", "insurance_agency"),
    ("office lawyer", "lawyer"),
    ("office travel_agent", "travel_agency"),
    ("public_transport station", "transit_station"),
    ("railway halt", "train_station"),
    ("railway station", "train_station"),
    ("railway tram_stop", "light_rail_station"),
    ("shop alcohol", "liquor_store"),
    ("shop bakery", "bakery"),
    ("shop beauty", "beauty_salon"),
    ("shop bicycle", "bicycle_store"),
    ("shop books", "book_store"),
    ("shop butcher", "butcher_shop"),
    ("shop car", "car_dealer"),
    ("shop car_repair", "car_repair"),
    ("shop chemist", "drugstore"),
    ("shop clothes", "clothing_store"),
    ("shop convenience", "convenience_store"),
    ("shop department_store", "department_store"),
    ("shop doityourself", "home_improvement_store"),
    ("shop electronics", "electronics_store"),
    ("shop florist", "florist"),
    ("shop furniture", "furniture_store"),
    ("shop gift", "gift_shop"),
    ("shop greengrocer", "grocery_store"),
    ("shop hairdresser", "hair_salon"),
    ("shop hardware", "hardware_store"),
    ("shop jewelry", "jewelry_store"),
    ("shop laundry", "laundry"),
    ("shop mall", "shopping_mall"),
    ("shop mobile_phone", "cell_phone_store"),
    ("shop pet", "pet_store"),
    ("shop shoes", "shoe_store"),
    ("shop sports", "sporting_goods_store"),
    ("shop supermarket", "supermarket"),
    ("shop travel_agency", "travel_agency"),
    ("tourism aquarium", "aquarium"),
    ("tourism attraction", "tourist_attraction"),
    ("tourism camp_site", "campground"),
    ("tourism gallery", "art_gallery"),
    ("tourism guest_house", "guest_house"),
    ("tourism hostel", "hostel"),
    ("tourism hotel", "hotel"),
    ("tourism information", "tourist_information_center"),
    ("tourism motel", "motel"),
    ("tourism museum", "museum"),
    ("tourism theme_park", "amusement_park"),
    ("tourism zoo", "zoo"),
];

/// The general types Google writes beside a type of one of its groups:
/// `food` for Food and Drink, `store` for Shopping, `lodging`, `finance`,
/// `health` and `place_of_worship`, as its place type tables group them.
fn groups_of(named: &str) -> &'static [&'static str] {
    const FOOD: &[&str] = &[
        "bakery",
        "bar",
        "cafe",
        "fast_food_restaurant",
        "food_court",
        "ice_cream_shop",
        "pub",
        "restaurant",
    ];
    const STORE: &[&str] = &[
        "bicycle_store",
        "book_store",
        "butcher_shop",
        "car_dealer",
        "cell_phone_store",
        "clothing_store",
        "convenience_store",
        "department_store",
        "drugstore",
        "electronics_store",
        "florist",
        "furniture_store",
        "gift_shop",
        "grocery_store",
        "hardware_store",
        "home_improvement_store",
        "jewelry_store",
        "liquor_store",
        "market",
        "pet_store",
        "shoe_store",
        "shopping_mall",
        "sporting_goods_store",
        "supermarket",
    ];
    const LODGING: &[&str] = &["campground", "guest_house", "hostel", "hotel", "motel"];
    const FINANCE: &[&str] = &["accounting", "atm", "bank"];
    const HEALTH: &[&str] = &[
        "dentist",
        "doctor",
        "drugstore",
        "hospital",
        "medical_lab",
        "pharmacy",
        "physiotherapist",
    ];
    match named {
        "bakery" | "butcher_shop" => &["food", "store"],
        "drugstore" => &["health", "store"],
        "place_of_worship" => &[],
        named if FOOD.contains(&named) => &["food"],
        named if STORE.contains(&named) => &["store"],
        named if LODGING.contains(&named) => &["lodging"],
        named if FINANCE.contains(&named) => &["finance"],
        named if HEALTH.contains(&named) => &["health"],
        _ => &[],
    }
}

/// Google's types every place has, whatever its kind.
const ANY_PLACE: [&str; 2] = ["point_of_interest", "establishment"];

/// og's POI category (`key:value`) as Google's type.
fn category_type(category: &str) -> Option<&'static str> {
    let (key, value) = category.split_once(':')?;
    CATEGORY_TYPES
        .iter()
        .find(|(known, _)| {
            known
                .split_once(' ')
                .is_some_and(|(known_key, known_value)| known_key == key && known_value == value)
        })
        .map(|(_, google)| *google)
}

/// og's categories of the places Google's `types` name, as the catalog
/// writes them. A type every place has names every kind, and a type og has
/// no category for names none.
pub(super) fn categories_of<'a>(types: impl IntoIterator<Item = &'a str>) -> Vec<&'static str> {
    let mut categories = Vec::new();
    for named in types {
        if ANY_PLACE.contains(&named) {
            return crate::categories::keys();
        }
        categories.extend(
            CATEGORY_TYPES
                .iter()
                .filter(|(_, google)| *google == named)
                .map(|(category, _)| *category),
        );
    }
    categories
}

#[cfg(test)]
mod type_tests {
    use super::{Kind, categories_of};

    /// A place carries its group's general type beside its own, and a shop
    /// is a store whatever its kind.
    #[test]
    fn a_place_carries_the_general_type_of_its_group() {
        let types = |primary, shop| {
            Kind::Poi {
                primary,
                shop,
                cuisine: [None; super::CUISINE_TYPES],
            }
            .types()
        };
        assert_eq!(
            types(Some("restaurant"), false),
            ["restaurant", "food", "point_of_interest", "establishment"]
        );
        assert_eq!(
            types(Some("bakery"), true),
            [
                "bakery",
                "food",
                "store",
                "point_of_interest",
                "establishment"
            ]
        );
        assert_eq!(
            types(None, true),
            ["store", "point_of_interest", "establishment"]
        );
        assert_eq!(
            types(Some("museum"), false),
            ["museum", "point_of_interest", "establishment"]
        );
    }

    /// Google's transit types find og's stations and stops.
    #[test]
    fn transit_types_find_og_stations_and_stops() {
        for (google, category) in [
            ("transit_station", "public_transport station"),
            ("train_station", "railway halt"),
            ("light_rail_station", "railway tram_stop"),
            ("bus_stop", "highway bus_stop"),
        ] {
            assert!(categories_of([google]).contains(&category), "{google}");
        }
    }
}
