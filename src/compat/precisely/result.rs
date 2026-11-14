//! og's feature as a Precisely `Result`.
//!
//! og's answer is read as its JSON, the same for a real-time lookup and for a
//! job's results read back from disk, so both carry a value into the same
//! Precisely slot.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::compat::{encode_id, iso};

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
    layer: String,
    accuracy: String,
    name: String,
    label: String,
    #[serde(default)]
    address: Address,
    #[serde(default)]
    context: Context,
    source: Source,
    confidence: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct Address {
    housenumber: Option<String>,
    unit: Option<String>,
    unit_type: Option<String>,
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

#[derive(Debug, Deserialize)]
struct Source {
    dataset: String,
}

/// og's features as this module reads them.
pub(super) fn features<T: Serialize>(features: &[T]) -> serde_json::Result<Vec<Feature>> {
    serde_json::from_value(serde_json::to_value(features)?)
}

/// Precisely's `Result`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Candidate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) score: Option<u8>,
    pub(super) address: ResponseAddress,
    pub(super) location: Location,
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
}

/// og's coordinates are WGS 84 longitude and latitude.
const CRS_NAME: &str = "epsg:4326";

/// The key Precisely's own id is under. og's id goes there, so a client
/// that looks a result up again by `PB_KEY` gets it back from og.
pub(super) const ID_FIELD: &str = "PB_KEY";

impl Feature {
    /// The label, as an autocomplete `prediction` shows it.
    pub(super) fn label(&self) -> &str {
        &self.properties.label
    }

    pub(super) fn candidate(&self) -> Candidate {
        let properties = &self.properties;
        let address = &properties.address;
        let context = &properties.context;
        let layer = properties.layer.as_str();
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
        let region_code = context
            .region
            .as_ref()
            .and_then(|region| region.code.as_deref()?.split_once('-'))
            .map(|(_, own)| own.to_string());
        let admin1 = match (area(&context.region), address.region.clone()) {
            (Some(long_name), stated) => Some(AdminField {
                short_name: region_code.or(stated).filter(|short| *short != long_name),
                long_name,
            }),
            (None, stated) => admin(stated),
        };
        let code = context
            .country
            .as_ref()
            .and_then(|country| country.code.clone());
        let country_name = area(&context.country).or_else(|| address.country.clone());
        let country = (country_name.is_some() || code.is_some()).then(|| CountryField {
            name: country_name,
            iso_alpha3_code: code.as_deref().and_then(iso::alpha3),
            iso_numeric_code: code.as_deref().and_then(iso::numeric),
            iso_alpha2_code: code,
        });
        // `areas` runs outermost first, a suburb before a neighbourhood.
        let neighborhood = area(&context.neighbourhood).or_else(|| address.areas.last().cloned());
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
        Candidate {
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
                unit: match (&address.unit, &address.unit_type) {
                    (Some(unit), Some(kind)) => unit
                        .strip_prefix(kind.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                        .or_else(|| Some(unit.clone())),
                    (unit, _) => unit.clone(),
                },
                unit_type: address.unit_type.clone(),
                place_name: (layer == "poi").then(|| properties.name.clone()),
                neighborhood: admin(neighborhood),
                suburb: admin(suburb),
                // The record's own locality is the one its address is
                // written with; the containing area may name the wider
                // municipality.
                city: admin(address.locality.clone().or_else(|| area(&context.locality))),
                admin2: admin(area(&context.district).or_else(|| area(&context.subregion))),
                admin1,
                postal_code,
                postal_code_ext,
                country,
            },
            location: Location {
                explanation: LocationExplanation {
                    kind: location_type(layer, &properties.accuracy),
                    designation: (properties.accuracy == "centroid").then_some("CENTROID"),
                    additional_metadata: BTreeMap::from([(
                        "SOURCE",
                        properties.source.dataset.clone(),
                    )]),
                },
                feature: SimpleFeature {
                    kind: "Feature",
                    properties: BTreeMap::from([("crsName", CRS_NAME)]),
                    geometry: PointGeometry {
                        kind: "Point",
                        coordinates: self.geometry.coordinates,
                    },
                },
            },
            explanation: Explanation {
                address_match: AddressMatch {
                    kind: match_type(layer, &properties.accuracy),
                },
            },
            custom_fields: BTreeMap::from([(ID_FIELD, encode_id(&properties.id))]),
        }
    }
}

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

/// The columns of a bulk output row, in order. Precisely names the fields
/// but documents no order, so this is og's.
pub(super) const COLUMNS: [&str; 30] = [
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

impl Candidate {
    /// The candidate's cells from `score` on, as [`COLUMNS`] lists them.
    pub(super) fn cells(&self) -> Vec<String> {
        let address = &self.address;
        let text = |value: &Option<String>| value.clone().unwrap_or_default();
        let name = |field: &Option<AdminField>| {
            field
                .as_ref()
                .map(|field| field.long_name.clone())
                .unwrap_or_default()
        };
        let country = address.country.as_ref();
        let [lon, lat] = self.location.feature.geometry.coordinates;
        // Bulk output names custom fields in camelCase, unlike real time.
        let custom_fields = self
            .custom_fields
            .get(ID_FIELD)
            .map(|id| serde_json::json!({ "pbKey": id }).to_string())
            .unwrap_or_default();
        vec![
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
            lat.to_string(),
            lon.to_string(),
            CRS_NAME.to_string(),
            "Point".to_string(),
            self.location
                .explanation
                .kind
                .unwrap_or_default()
                .to_string(),
            self.explanation.address_match.kind.to_string(),
            custom_fields,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
