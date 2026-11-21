//! og's feature as a Precisely `Result`.
//!
//! A real-time lookup hands og's typed feature over as it is. A job's results
//! come back from disk as og wrote them, and are parsed there into the same
//! [`Feature`], so both fill the same Precisely slots.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::match_code::{Holds, MatchCode};
use crate::{
    api::{self, Accuracy, PartMatch, QueryMatch},
    compat::encode_parts,
    iso,
    record::Layer,
};

/// The real-time or bulk product a result answers. Verify standardizes an
/// address, so its result has no location; geocode places it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Product {
    #[default]
    Geocode,
    Verify,
}

/// og's feature, the fields a Precisely result carries.
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
    /// The ISO code of an area result itself, which its `context` does not
    /// name.
    code: Option<String>,
    #[serde(default)]
    address: Address,
    #[serde(default)]
    context: Context,
    source: Source,
    confidence: Option<f32>,
    #[serde(default)]
    address_lines: Vec<String>,
    #[serde(rename = "match")]
    matched: Option<QueryMatch>,
    distance_m: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct Address {
    housenumber: Option<String>,
    unit: Option<String>,
    street: Option<String>,
    place: Option<String>,
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
    subregion: Option<Area>,
    region: Option<Area>,
    country: Option<Area>,
}

#[derive(Debug, Deserialize)]
struct Area {
    name: String,
    code: Option<String>,
}

/// An area the result lies in or is: its name and ISO code.
#[derive(Debug, Clone, Copy)]
struct AreaRef<'a> {
    name: &'a str,
    code: Option<&'a str>,
}

impl<'a> From<&'a Area> for AreaRef<'a> {
    fn from(area: &'a Area) -> Self {
        Self {
            name: &area.name,
            code: area.code.as_deref(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Source {
    dataset: String,
}

impl Feature {
    fn of(
        common: &api::Common,
        coordinates: [f64; 2],
        confidence: Option<f32>,
        matched: Option<QueryMatch>,
        distance_m: Option<f64>,
    ) -> Self {
        let address = &common.address;
        let context = &common.context;
        let area = |area: &Option<api::ContextArea>| {
            area.as_ref().map(|area| Area {
                name: area.name.clone(),
                code: area.code.clone(),
            })
        };
        Self {
            geometry: Geometry { coordinates },
            properties: Properties {
                id: common.id.clone(),
                layer: common.layer,
                accuracy: common.accuracy,
                name: common.name.clone(),
                label: common.label.clone(),
                code: common.code.clone(),
                address: Address {
                    housenumber: address.housenumber.clone(),
                    unit: address.unit.clone(),
                    street: address.street.clone(),
                    place: address.place.clone(),
                    areas: address.areas.clone(),
                    locality: address.locality.clone(),
                    region: address.region.clone(),
                    postcode: address.postcode.clone(),
                    country: address.country.clone(),
                },
                context: Context {
                    neighbourhood: area(&context.neighbourhood),
                    sublocality: area(&context.sublocality),
                    locality: area(&context.locality),
                    district: area(&context.district),
                    subregion: area(&context.subregion),
                    region: area(&context.region),
                    country: area(&context.country),
                },
                source: Source {
                    dataset: common.source.dataset.clone(),
                },
                confidence,
                address_lines: common.address_lines.clone(),
                matched,
                distance_m,
            },
        }
    }
}

impl From<&api::ForwardFeature> for Feature {
    fn from(feature: &api::ForwardFeature) -> Self {
        let properties = &feature.properties;
        Self::of(
            &properties.common,
            feature.geometry.coordinates,
            properties.confidence,
            properties.matched,
            properties.distance_m,
        )
    }
}

impl From<&api::ReverseFeature> for Feature {
    fn from(feature: &api::ReverseFeature) -> Self {
        let properties = &feature.properties;
        Self::of(
            &properties.common,
            feature.geometry.coordinates,
            None,
            None,
            properties.distance_m,
        )
    }
}

/// Precisely's `Result`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Candidate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) score: Option<u8>,
    pub(super) address: ResponseAddress,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) address_lines: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) location: Option<Location>,
    pub(super) explanation: Explanation,
    pub(super) custom_fields: BTreeMap<&'static str, String>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ResponseAddress {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) formatted_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) formatted_street_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) formatted_location_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) address_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) street: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) unit_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) place_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) neighborhood: Option<AdminField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) suburb: Option<AdminField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) city: Option<AdminField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) admin2: Option<AdminField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) admin1: Option<AdminField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) postal_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) postal_code_ext: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) country: Option<CountryField>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AdminField {
    pub(super) long_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) short_name: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CountryField {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) iso_alpha2_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) iso_alpha3_code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) iso_numeric_code: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub(super) struct Location {
    pub(super) explanation: LocationExplanation,
    pub(super) feature: SimpleFeature,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LocationExplanation {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub(super) kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) designation: Option<&'static str>,
    pub(super) additional_metadata: BTreeMap<&'static str, String>,
}

#[derive(Debug, Serialize)]
pub(super) struct SimpleFeature {
    #[serde(rename = "type")]
    kind: &'static str,
    properties: BTreeMap<&'static str, &'static str>,
    pub(super) geometry: PointGeometry,
}

impl SimpleFeature {
    /// A point in og's coordinates.
    pub(super) fn point(coordinates: [f64; 2]) -> Self {
        Self {
            kind: "Feature",
            properties: BTreeMap::from([("crsName", CRS_NAME)]),
            geometry: PointGeometry {
                kind: "Point",
                coordinates,
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct PointGeometry {
    #[serde(rename = "type")]
    kind: &'static str,
    pub(super) coordinates: [f64; 2],
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Explanation {
    pub(super) address_match: AddressMatch,
}

#[derive(Debug, Serialize)]
pub(super) struct AddressMatch {
    #[serde(rename = "type")]
    pub(super) kind: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) description: Vec<MatchDescription>,
}

/// How one address part matched, `MatchDescription`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MatchDescription {
    pub(super) label: &'static str,
    pub(super) match_type: &'static str,
}

/// og's coordinates are WGS 84 longitude and latitude.
const CRS_NAME: &str = "epsg:4326";

/// The compatibility field carrying the og reference for lookup.
pub(super) const ID_FIELD: &str = "PB_KEY";

impl Feature {
    /// The label, as an autocomplete `prediction` shows it.
    pub(super) fn label(&self) -> &str {
        &self.properties.label
    }

    pub(super) fn name(&self) -> &str {
        &self.properties.name
    }

    /// `[longitude, latitude]`.
    pub(super) const fn coordinates(&self) -> [f64; 2] {
        self.geometry.coordinates
    }

    /// Whether og took it from `dataset`, `osm` or `openaddresses`, whose
    /// sources og names `openaddresses:<source>`.
    pub(super) fn is_from(&self, dataset: &str) -> bool {
        let source = &self.properties.source.dataset;
        source == dataset
            || source
                .strip_prefix(dataset)
                .is_some_and(|rest| rest.starts_with(':'))
    }

    /// The result itself as an area of `layer`, when it is one. An area's
    /// `context` holds only the areas around it.
    fn own(&self, layer: Layer) -> Option<AreaRef<'_>> {
        let properties = &self.properties;
        (properties.layer == layer).then(|| AreaRef {
            name: &properties.name,
            code: properties.code.as_deref(),
        })
    }

    /// The region the result lies in, or is.
    fn region(&self) -> Option<AreaRef<'_>> {
        self.properties
            .context
            .region
            .as_ref()
            .map(AreaRef::from)
            .or_else(|| self.own(Layer::Region))
    }

    /// The country the result lies in, or is.
    fn country(&self) -> Option<AreaRef<'_>> {
        self.properties
            .context
            .country
            .as_ref()
            .map(AreaRef::from)
            .or_else(|| self.own(Layer::Country))
    }

    /// The ISO 3166-1 alpha-2 code of the country it lies in or is. A
    /// region's ISO 3166-2 code starts with it, so a region that names no
    /// country still gives it.
    pub(super) fn country_code(&self) -> Option<&str> {
        self.country().and_then(|country| country.code).or_else(|| {
            self.region()
                .and_then(|region| region.code)
                .and_then(|code| code.split_once('-'))
                .map(|(country, _)| country)
        })
    }

    /// A reverse geocode candidate. It carries how far the result is from
    /// the asked point in the custom fields Precisely writes it to. An area
    /// holds the point, so it has none.
    pub(super) fn reverse_candidate(&self) -> Candidate {
        let mut candidate = self.candidate(Product::Geocode);
        if let Some(distance_m) = self.properties.distance_m {
            candidate
                .custom_fields
                .insert(REVERSE_DISTANCE, distance_m.to_string());
            candidate
                .custom_fields
                .insert(REVERSE_DISTANCE_UNIT, "METERS".to_string());
        }
        candidate
    }

    /// Metres from the point a reverse geocode, or an autocomplete with
    /// `originXY`, measured from.
    pub(super) const fn distance_m(&self) -> Option<f64> {
        self.properties.distance_m
    }

    pub(super) fn candidate(&self, product: Product) -> Candidate {
        let properties = &self.properties;
        let address = &properties.address;
        let context = &properties.context;
        let layer = properties.layer.as_str();
        let accuracy = properties.accuracy.as_str();
        let is_street_line = matches!(layer, "address" | "interpolation" | "street");
        let area = |area: &Option<Area>| area.as_ref().map(|area| area.name.clone());
        let admin = |name: Option<String>| {
            name.map(|long_name| AdminField {
                long_name,
                short_name: None,
            })
        };
        let (postal_code, postal_code_ext) = match address.postcode.as_deref() {
            Some(postcode) => match zip_plus_four(postcode) {
                Some((zip, plus_four)) => (Some(zip.to_string()), Some(plus_four.to_string())),
                None => (Some(postcode.to_string()), None),
            },
            None => (None, None),
        };
        // The record states the region as its address writes it, often an
        // abbreviation (`NJ`); the area containing it has the full name.
        // The region's code within its country (`ON` of `CA-ON`) is its short
        // name when og knows it.
        let region = self.region();
        let region_code = region
            .and_then(|region| region.code?.split_once('-'))
            .map(|(_, own)| own.to_string());
        let admin1 = match (
            region.map(|region| region.name.to_string()),
            address.region.clone(),
        ) {
            (Some(long_name), stated) => Some(AdminField {
                short_name: region_code.or(stated).filter(|short| *short != long_name),
                long_name,
            }),
            (None, stated) => admin(stated),
        };
        let code = self.country_code().map(str::to_string);
        let country_name = self
            .country()
            .map(|country| country.name.to_string())
            .or_else(|| address.country.clone());
        let country = (country_name.is_some() || code.is_some()).then(|| CountryField {
            name: country_name,
            iso_alpha3_code: code.as_deref().and_then(iso::alpha3),
            iso_numeric_code: code.as_deref().and_then(iso::numeric),
            iso_alpha2_code: code,
        });
        // An area result is its own area of its layer: a town fills `city`
        // with its own name, as a region fills `admin1`.
        let own = |of: &str| (layer == of).then(|| properties.name.clone());
        // `areas` runs outermost first, a suburb before a neighbourhood.
        let neighborhood = area(&context.neighbourhood)
            .or_else(|| address.areas.last().cloned())
            .or_else(|| own("neighbourhood"));
        let suburb = area(&context.sublocality)
            .or_else(|| (address.areas.len() > 1).then(|| address.areas[0].clone()));
        let street = address
            .street
            .clone()
            // OSM `addr:place` numbers the house on a place, not a street.
            .or_else(|| address.place.clone())
            .or_else(|| (layer == "street").then(|| properties.name.clone()));
        // og's name is the street line only for these layers; a POI's is
        // its own name.
        let (street_line, location_line) = if is_street_line {
            let (street_line, location_line) =
                split_label(&properties.label, &properties.name, address.unit.as_deref());
            (Some(street_line), location_line)
        } else {
            (None, None)
        };
        let judged = properties.matched.unwrap_or_default();
        let kind = address.unit.as_deref().and_then(unit_type);
        let mut candidate = Candidate {
            score: properties
                .confidence
                .map(|confidence| (confidence * 100.0).round().clamp(0.0, 100.0) as u8),
            address: ResponseAddress {
                formatted_address: Some(properties.label.clone()),
                formatted_street_address: street_line,
                formatted_location_address: location_line,
                address_number: address.housenumber.clone(),
                street,
                // The unit's type apart from its value: `STE` and `300` of
                // `STE 300`.
                unit: match (&address.unit, &kind) {
                    (Some(unit), Some(kind)) => unit
                        .strip_prefix(kind.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                        .or_else(|| Some(unit.clone())),
                    (unit, _) => unit.clone(),
                },
                unit_type: kind,
                place_name: (layer == "poi").then(|| properties.name.clone()),
                neighborhood: admin(neighborhood),
                suburb: admin(suburb),
                // The record's own locality is the one its address is
                // written with; the containing area may name the wider
                // municipality.
                city: admin(
                    address
                        .locality
                        .clone()
                        .or_else(|| area(&context.locality))
                        .or_else(|| own("locality")),
                ),
                admin2: admin(
                    area(&context.district)
                        .or_else(|| area(&context.subregion))
                        .or_else(|| own("district")),
                ),
                admin1,
                postal_code,
                postal_code_ext,
                country,
            },
            address_lines: properties.address_lines.clone(),
            location: Some(Location {
                explanation: LocationExplanation {
                    kind: location_type(layer, accuracy),
                    designation: (properties.accuracy == Accuracy::Centroid).then_some("CENTROID"),
                    additional_metadata: BTreeMap::from([(
                        "SOURCE",
                        properties.source.dataset.clone(),
                    )]),
                },
                feature: SimpleFeature::point(self.geometry.coordinates),
            }),
            explanation: Explanation {
                address_match: AddressMatch {
                    kind: match_type(layer, accuracy),
                    description: Vec::new(),
                },
            },
            custom_fields: BTreeMap::from([(
                ID_FIELD,
                encode_parts(&properties.id, Some(properties.layer), &properties.label),
            )]),
        };
        // A point may have a street-only display label even though its
        // recorded components identify the municipality and state.
        if is_street_line
            && candidate.address.formatted_location_address.is_none()
            && candidate
                .address
                .country
                .as_ref()
                .and_then(|country| country.iso_alpha2_code.as_deref())
                == Some("US")
            && let (Some(city), Some(region), Some(street)) = (
                candidate.address.city.as_ref(),
                candidate.address.admin1.as_ref(),
                candidate.address.formatted_street_address.as_deref(),
            )
        {
            let state = region.short_name.as_deref().unwrap_or(&region.long_name);
            let postal = properties.address.postcode.as_deref();
            let state_and_postal = [Some(state), postal]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            let location = format!("{}, {state_and_postal}", city.long_name);
            let country = candidate
                .address
                .country
                .as_ref()
                .and_then(|country| country.name.as_deref());
            let city_state = format!("{} {state_and_postal}", city.long_name);
            candidate.address.formatted_address = Some(
                [Some(street), Some(&city_state), country]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            candidate.address.formatted_location_address = Some(location);
        }
        let holds = Holds::of(&candidate.address);
        candidate.explanation.address_match.description = describe(judged, &holds);
        if product == Product::Verify {
            // Verify standardizes an address and does not place it.
            candidate.location = None;
            if let Some(code) = MatchCode::derive(judged, holds, candidate.score) {
                candidate
                    .custom_fields
                    .insert(SIMPLE_MATCH_CODE, code.simple());
                candidate
                    .custom_fields
                    .insert(EXTENDED_MATCH_CODE, code.extended());
            }
        }
        candidate
    }
}

impl Holds {
    /// What a result's address holds, for the parts og can fill.
    fn of(address: &ResponseAddress) -> Self {
        Self {
            housenumber: address.address_number.is_some(),
            street: address.street.is_some(),
            unit: address.unit.is_some(),
            sub_city: address.neighborhood.is_some() || address.suburb.is_some(),
            city: address.city.is_some(),
            region: address.admin1.is_some(),
            postcode: address.postal_code.is_some(),
            postcode_ext: address.postal_code_ext.is_some(),
            country: address.country.is_some(),
        }
    }
}

/// How each part matched, as Precisely's `MatchDescription` names it. Only
/// og's own `matched` is `EXACT`; a part the request left out that the
/// result fills is `ADDED`. Empty when og judged no part, as of a record
/// looked up by its key.
fn describe(judged: QueryMatch, holds: &Holds) -> Vec<MatchDescription> {
    if judged.is_empty() {
        return Vec::new();
    }
    [
        ("addressNumber", judged.housenumber, holds.housenumber),
        ("street", judged.street, holds.street),
        ("unitMatch", judged.unit, holds.unit),
        ("city", judged.locality, holds.city),
        ("admin1", judged.region, holds.region),
        ("postalCode", judged.postcode, holds.postcode),
        ("country", judged.country, holds.country),
    ]
    .into_iter()
    .map(|(label, judged, holds)| MatchDescription {
        label,
        match_type: match judged {
            Some(PartMatch::Matched) => "EXACT",
            Some(PartMatch::Partial) => "PARTIAL",
            Some(PartMatch::Unmatched) => "NONE",
            None if holds => "ADDED",
            None => "NONE",
        },
    })
    .collect()
}

/// The keys a reverse geocode result's distance from the asked point is
/// under.
pub(super) const REVERSE_DISTANCE: &str = "REVERSE_GEOCODE_DISTANCE";
pub(super) const REVERSE_DISTANCE_UNIT: &str = "REVERSE_GEOCODE_DISTANCE_UNIT";

/// The keys a verify result's Global Match Codes are under.
pub(super) const SIMPLE_MATCH_CODE: &str = "SIMPLE_MATCH_CODE";
pub(super) const EXTENDED_MATCH_CODE: &str = "EXTENDED_MATCH_CODE";

/// og's label as Precisely's two lines. The street line, with the unit
/// when the label writes it next, and the rest. The label starts with the
/// name, then follows the country's template; a label that does not start
/// so has no location line og can tell apart.
fn split_label(label: &str, name: &str, unit: Option<&str>) -> (String, Option<String>) {
    let Some(rest) = label
        .strip_prefix(name)
        .and_then(|rest| rest.strip_prefix(", "))
    else {
        return (name.to_string(), None);
    };
    let (first, after) = match rest.split_once(", ") {
        Some((first, after)) => (first, Some(after)),
        None => (rest, None),
    };
    let names_unit = unit.is_some_and(|unit| first.split_whitespace().any(|word| word == unit));
    if names_unit {
        (format!("{name}, {first}"), after.map(str::to_string))
    } else {
        (name.to_string(), Some(rest.to_string()))
    }
}

/// A US ZIP+4 apart, as Precisely returns `postalCode` and `postalCodeExt`.
fn zip_plus_four(postcode: &str) -> Option<(&str, &str)> {
    let (zip, plus_four) = postcode.split_once('-')?;
    let digits = |part: &str, length: usize| {
        part.len() == length && part.bytes().all(|byte| byte.is_ascii_digit())
    };
    (digits(zip, 5) && digits(plus_four, 4)).then_some((zip, plus_four))
}

/// Precisely's `LocationType` for what og's point stands for. A named
/// `place` (an island, a hamlet) has no counterpart, so it has none.
fn location_type(layer: &str, accuracy: &str) -> Option<&'static str> {
    Some(match (layer, accuracy) {
        ("address" | "interpolation", "interpolated") => "ADDRESS_INTERPOLATED",
        // With no house to place, the point is on its street.
        ("address" | "interpolation" | "street", "street") | ("street", _) => "STREET_CENTER",
        ("address", _) => "ADDRESS_POINT",
        ("interpolation", _) => "ADDRESS_INTERPOLATED",
        ("poi", _) => "POI",
        ("postcode", _) => "POSTAL_CENTROID",
        ("neighbourhood", _) => "SUB_TOWN_CENTROID",
        ("locality", _) => "TOWN_CENTROID",
        ("district", _) => "ADMIN2_CENTROID",
        ("region", _) => "ADMIN1_CENTROID",
        ("country", _) => "COUNTRY",
        _ => return None,
    })
}

/// Precisely's `MatchType`, what kind of thing matched. An area, og's
/// fallback when the address is not found among them, is `ADMIN`.
fn match_type(layer: &str, accuracy: &str) -> &'static str {
    match (layer, accuracy) {
        ("address" | "interpolation", "street") | ("street", _) => "STREET",
        ("address" | "interpolation", _) => "ADDRESS",
        ("poi", _) => "POI",
        _ => "ADMIN",
    }
}

/// The columns of a bulk geocode output row, in order. Precisely names the
/// fields but documents no order, so this is og's.
const COLUMNS: [&str; 30] = [
    "addressId",
    "status",
    "errorMessage",
    "score",
    "formattedAddress",
    "formattedStreetAddress",
    "formattedLocationAddress",
    "addressNumber",
    "street",
    "unit",
    "placeName",
    "neighborhood",
    "suburb",
    "city",
    "admin2",
    "admin1",
    "admin1Short",
    "postalCode",
    "postalCodeExt",
    "country",
    "countryName",
    "countryIso2",
    "countryNumeric",
    "latitude",
    "longitude",
    "crsName",
    "geometryType",
    "locationType",
    "addressMatchType",
    "customFields",
];

/// Columns bulk verify output leaves out, since a verified address has no
/// point.
const UNPLACED: [&str; 2] = ["latitude", "longitude"];

impl Product {
    /// The columns of the product's bulk output row, in order.
    pub(super) fn columns(self) -> Vec<&'static str> {
        COLUMNS
            .into_iter()
            .filter(|column| self == Self::Geocode || !UNPLACED.contains(column))
            .collect()
    }
}

impl Candidate {
    /// The candidate's cells from `score` on, as [`Product::columns`] lists
    /// them.
    pub(super) fn cells(&self, product: Product) -> Vec<String> {
        let address = &self.address;
        let text = |value: &Option<String>| value.clone().unwrap_or_default();
        let name = |field: &Option<AdminField>| {
            field
                .as_ref()
                .map(|field| field.long_name.clone())
                .unwrap_or_default()
        };
        let country = address.country.as_ref();
        let location = self.location.as_ref();
        let coordinate = |index: usize| {
            location
                .map(|location| location.feature.geometry.coordinates[index].to_string())
                .unwrap_or_default()
        };
        // Bulk output names custom fields in camelCase, unlike real time.
        let custom_fields = [
            (ID_FIELD, "pbKey"),
            (SIMPLE_MATCH_CODE, "simpleMatchCode"),
            (EXTENDED_MATCH_CODE, "extendedMatchCode"),
        ]
        .into_iter()
        .filter_map(|(key, name)| Some((name, self.custom_fields.get(key)?)))
        .collect::<BTreeMap<_, _>>();
        let custom_fields = if custom_fields.is_empty() {
            String::new()
        } else {
            serde_json::json!(custom_fields).to_string()
        };
        let cells = vec![
            self.score
                .map(|score| score.to_string())
                .unwrap_or_default(),
            text(&address.formatted_address),
            text(&address.formatted_street_address),
            text(&address.formatted_location_address),
            text(&address.address_number),
            text(&address.street),
            text(&address.unit),
            text(&address.place_name),
            name(&address.neighborhood),
            name(&address.suburb),
            name(&address.city),
            name(&address.admin2),
            name(&address.admin1),
            address
                .admin1
                .as_ref()
                .and_then(|admin1| admin1.short_name.clone())
                .unwrap_or_default(),
            text(&address.postal_code),
            text(&address.postal_code_ext),
            // `country` is the alpha-3 code, since v2 output has the name,
            // alpha-2 and numeric code as columns of their own.
            country
                .and_then(|country| country.iso_alpha3_code)
                .unwrap_or_default()
                .to_string(),
            country
                .and_then(|country| country.name.clone())
                .unwrap_or_default(),
            country
                .and_then(|country| country.iso_alpha2_code.clone())
                .unwrap_or_default(),
            country
                .and_then(|country| country.iso_numeric_code)
                .unwrap_or_default()
                .to_string(),
            coordinate(1),
            coordinate(0),
            location.map(|_| CRS_NAME.to_string()).unwrap_or_default(),
            location.map(|_| "Point".to_string()).unwrap_or_default(),
            location
                .and_then(|location| location.explanation.kind)
                .unwrap_or_default()
                .to_string(),
            self.explanation.address_match.kind.to_string(),
            custom_fields,
        ];
        // `COLUMNS` opens with the id, status and message, which the caller
        // writes.
        COLUMNS[3..]
            .iter()
            .zip(cells)
            .filter(|(column, _)| product == Product::Geocode || !UNPLACED.contains(column))
            .map(|(_, cell)| cell)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_area_result_fills_its_own_slot() {
        let candidate = |layer: &str, name: &str| {
            let feature: Feature = serde_json::from_value(serde_json::json!({
                "geometry": {"coordinates": [-71.6, 41.58]},
                "properties": {
                    "id": "osm:relation:2", "layer": layer, "accuracy": "centroid",
                    "name": name, "label": name, "source": {"dataset": "osm"},
                    "context": {"region": {"name": "Rhode Island", "code": "US-RI"}},
                },
            }))
            .expect("an og feature");
            serde_json::to_value(feature.candidate(Product::Geocode).address).expect("address")
        };
        let town = candidate("locality", "Exeter");
        assert_eq!(town["city"], serde_json::json!({"longName": "Exeter"}));
        assert_eq!(town["admin1"]["shortName"], "RI");
        let county = candidate("district", "Washington County");
        assert_eq!(
            county["admin2"],
            serde_json::json!({"longName": "Washington County"})
        );
        assert_eq!(county.get("city"), None);
    }

    #[test]
    fn a_zip_plus_four_comes_apart() {
        assert_eq!(zip_plus_four("12180-8371"), Some(("12180", "8371")));
        assert_eq!(zip_plus_four("12180"), None);
        assert_eq!(zip_plus_four("1218-08371"), None);
        assert_eq!(zip_plus_four("12180-837"), None);
        assert_eq!(zip_plus_four("M5X 1A9"), None);
        assert_eq!(zip_plus_four("ABCDE-FGHI"), None);
    }

    #[test]
    fn a_label_splits_into_street_and_location_lines() {
        let split = |label, name, unit| split_label(label, name, unit);
        assert_eq!(
            split("10 King Street, Toronto, M5V 1A1", "10 King Street", None),
            (
                "10 King Street".to_string(),
                Some("Toronto, M5V 1A1".to_string())
            )
        );
        assert_eq!(
            split(
                "100 King Street West, Unit 5, ON, M5X 1A9",
                "100 King Street West",
                Some("5")
            ),
            (
                "100 King Street West, Unit 5".to_string(),
                Some("ON, M5X 1A9".to_string())
            )
        );
        assert_eq!(
            split(
                "100 King Street West, Unit 5",
                "100 King Street West",
                Some("5")
            ),
            ("100 King Street West, Unit 5".to_string(), None)
        );
        // A unit the label does not write next stays out of the street line.
        assert_eq!(
            split(
                "100 King Street West, Toronto 5",
                "100 King Street West",
                Some("7")
            ),
            (
                "100 King Street West".to_string(),
                Some("Toronto 5".to_string())
            )
        );
        assert_eq!(
            split(
                "King Street West 200-298 even, Toronto",
                "King Street West",
                None
            ),
            ("King Street West".to_string(), None)
        );
        assert_eq!(
            split("Main Street", "Main Street", None),
            ("Main Street".to_string(), None)
        );
    }

    #[test]
    fn each_kind_of_point_has_its_location_type() {
        let cases = [
            ("address", "rooftop", Some("ADDRESS_POINT"), "ADDRESS"),
            ("address", "centroid", Some("ADDRESS_POINT"), "ADDRESS"),
            (
                "address",
                "interpolated",
                Some("ADDRESS_INTERPOLATED"),
                "ADDRESS",
            ),
            ("address", "street", Some("STREET_CENTER"), "STREET"),
            (
                "interpolation",
                "interpolated",
                Some("ADDRESS_INTERPOLATED"),
                "ADDRESS",
            ),
            ("interpolation", "street", Some("STREET_CENTER"), "STREET"),
            ("street", "street", Some("STREET_CENTER"), "STREET"),
            ("poi", "rooftop", Some("POI"), "POI"),
            ("postcode", "approximate", Some("POSTAL_CENTROID"), "ADMIN"),
            (
                "neighbourhood",
                "centroid",
                Some("SUB_TOWN_CENTROID"),
                "ADMIN",
            ),
            ("locality", "approximate", Some("TOWN_CENTROID"), "ADMIN"),
            ("district", "centroid", Some("ADMIN2_CENTROID"), "ADMIN"),
            ("region", "centroid", Some("ADMIN1_CENTROID"), "ADMIN"),
            ("country", "centroid", Some("COUNTRY"), "ADMIN"),
            ("place", "approximate", None, "ADMIN"),
        ];
        for (layer, accuracy, kind, matched) in cases {
            assert_eq!(location_type(layer, accuracy), kind, "{layer} {accuracy}");
            assert_eq!(match_type(layer, accuracy), matched, "{layer} {accuracy}");
        }
    }
}

/// Words a unit starts with that say what kind of unit it is.
const UNIT_TYPES: [&str; 18] = [
    "apartment",
    "apt",
    "building",
    "bldg",
    "flat",
    "floor",
    "fl",
    "level",
    "lot",
    "office",
    "room",
    "rm",
    "shop",
    "space",
    "ste",
    "suite",
    "unit",
    "#",
];

/// `unit`'s designator as written, such as `Apt` of `Apt 4`, for
/// Precisely's `unitType`. `None` when the unit starts with none, or is
/// only one.
fn unit_type(unit: &str) -> Option<String> {
    let mut words = unit.split_whitespace();
    let first = words.next()?;
    // A designator with nothing after it is the whole unit, not its type.
    words.next()?;
    let key = first.trim_end_matches('.').to_lowercase();
    UNIT_TYPES
        .contains(&key.as_str())
        .then(|| first.to_string())
}

#[cfg(test)]
mod unit_tests {
    use super::unit_type;

    #[test]
    fn a_unit_names_its_type_only_when_it_starts_with_one() {
        assert_eq!(unit_type("Apt 4").as_deref(), Some("Apt"));
        assert_eq!(unit_type("Ste. 200").as_deref(), Some("Ste."));
        assert_eq!(unit_type("4"), None);
        assert_eq!(unit_type("Penthouse B"), None);
        assert_eq!(unit_type("Suite"), None);
    }
}
