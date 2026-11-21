//! og's feature as a Precisely `Result`.
//!
//! A real-time lookup hands og's typed feature over as it is. A job's results
//! come back from disk as og wrote them, and are parsed there into the same
//! [`Feature`], so both fill the same Precisely slots.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::match_code::{Holds, MatchCode};
use crate::{
    api::{self, Accuracy, Highlight, PartMatch, QueryMatch},
    compat::{
        encode_parts, town,
        usps::{self, Style, UsAddress},
    },
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
    #[serde(default)]
    highlights: Vec<Highlight>,
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
    street_parts: Option<StreetParts>,
}

/// og's `street_parts`, the street as its prefix, name, type and suffix.
#[derive(Debug, Deserialize)]
struct StreetParts {
    prefix: Option<String>,
    name: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    suffix: Option<String>,
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
    /// The parts both a forward and a reverse feature have.
    fn of(common: &api::Common, coordinates: [f64; 2]) -> Self {
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
                    street_parts: address.street_parts.as_ref().map(|parts| StreetParts {
                        prefix: parts.prefix.clone(),
                        name: parts.name.clone(),
                        kind: parts.kind.clone(),
                        suffix: parts.suffix.clone(),
                    }),
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
                confidence: None,
                address_lines: common.address_lines.clone(),
                matched: None,
                distance_m: None,
                highlights: Vec::new(),
            },
        }
    }
}

impl From<&api::ForwardFeature> for Feature {
    fn from(feature: &api::ForwardFeature) -> Self {
        let properties = &feature.properties;
        let mut own = Self::of(&properties.common, feature.geometry.coordinates);
        own.properties.confidence = properties.confidence;
        own.properties.matched = properties.matched;
        own.properties.distance_m = properties.distance_m;
        own.properties.highlights.clone_from(&properties.highlights);
        own
    }
}

impl From<&api::ReverseFeature> for Feature {
    fn from(feature: &api::ReverseFeature) -> Self {
        let properties = &feature.properties;
        let mut own = Self::of(&properties.common, feature.geometry.coordinates);
        own.properties.distance_m = properties.distance_m;
        own
    }
}

impl Candidate {
    /// Restates the language the request asked for, by its language code
    /// alone (`en` for `en-US`), as every postal answer's custom fields do.
    pub(super) fn in_language(mut self, lang: Option<&str>) -> Self {
        let code = lang
            .and_then(|lang| lang.split(['-', '_']).next())
            .map(str::to_ascii_lowercase)
            .filter(|code| !code.is_empty());
        if let Some(code) = code {
            self.custom_fields.insert("LANGUAGE", code);
        }
        self
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

/// Precisely's `AdminField`. Autocomplete writes a county and a
/// neighbourhood as an empty one.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AdminField {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) long_name: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) matched_substring: Option<String>,
}

/// og's coordinates are WGS 84 longitude and latitude.
const CRS_NAME: &str = "epsg:4326";

/// The compatibility field carrying the og reference for lookup.
pub(super) const ID_FIELD: &str = "PB_KEY";
/// The custom field naming the result's point as a geohash.
const GEOHASH: &str = "GEOHASH";
/// Characters of the geohash, which names the point to centimetres.
const GEOHASH_CHARS: usize = 12;
/// The custom field naming what kind of thing matched the address asked.
const MATCH_TYPE: &str = "MATCH_TYPE";

/// Which autocomplete a prediction answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Autocomplete {
    /// `/v1/autocomplete`.
    V1,
    /// `/v2/express-autocomplete`.
    Express,
}

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

    /// Judges the street's type and direction apart from its name, as
    /// Precisely describes them: each is `EXACT` where the asked street and
    /// the record agree on it, neither stating one included, and `NONE`
    /// where they differ or the street did not match. The record's part
    /// agrees when any asked word is a form of it, wherever it stands, so a
    /// unit read into the asked street (`District Ave Ste`) leaves its type
    /// standing.
    pub(super) fn judge_street_parts(&self, candidate: &mut Candidate, asked: Option<&str>) {
        use crate::{
            parse::lattice::other_forms,
            road::{StreetParts, direction_of},
        };
        let description = &mut candidate.explanation.address_match.description;
        let street_matched = description
            .iter()
            .any(|entry| entry.label == "street" && entry.match_type != "NONE");
        let Some(asked) = asked.filter(|_| street_matched) else {
            return;
        };
        let held = self
            .properties
            .address
            .street
            .as_deref()
            .unwrap_or_default();
        let key = |word: &str| {
            crate::util::fold::search_key(word.trim_end_matches('.')).unwrap_or_default()
        };
        let words = asked.split_whitespace().map(key).collect::<Vec<_>>();
        let (asked, held) = (
            StreetParts::of(asked).unwrap_or_default(),
            StreetParts::of(held).unwrap_or_default(),
        );
        let direction = |parts: &StreetParts<'_>| {
            parts
                .prefix
                .or(parts.suffix)
                .and_then(|word| direction_of(&key(word)))
        };
        let kind_agrees = match held.kind.map(key) {
            Some(kind) => words.iter().any(|word| {
                *word == kind
                    || other_forms(word).contains(&kind)
                    || other_forms(&kind).contains(word)
            }),
            None => asked.kind.is_none(),
        };
        let direction_agrees = match direction(&held) {
            Some(stated) => words.iter().any(|word| direction_of(word) == Some(stated)),
            None => direction(&asked).is_none(),
        };
        let judged = [
            ("streetType", kind_agrees),
            ("streetDirectional", direction_agrees),
        ];
        for entry in description.iter_mut() {
            if let Some((_, agrees)) = judged.iter().find(|(label, _)| *label == entry.label) {
                entry.match_type = if *agrees { "EXACT" } else { "NONE" };
            }
        }
    }

    /// A reverse geocode candidate. It carries how far the result is from
    /// the asked point in the custom fields Precisely writes it to. An area
    /// holds the point, so it has none.
    pub(super) fn reverse_candidate(&self) -> Candidate {
        let mut candidate = self.unasked_candidate(FoundBy::Point);
        // The point is the one asked, so the answer restates no geohash.
        candidate.custom_fields.remove(GEOHASH);
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

    /// A candidate found with no address asked, by a point or a key.
    /// Precisely scores how well a result matches the address asked, and
    /// writes 0 when none was. Each part is judged by how the record was
    /// found ([`judged_unasked`]).
    pub(super) fn unasked_candidate(&self, found_by: FoundBy) -> Candidate {
        let mut candidate = self.candidate(Product::Geocode);
        candidate.score = Some(0);
        // No address was asked, so no kind of match is named.
        candidate.custom_fields.remove(MATCH_TYPE);
        candidate.explanation.address_match.description = judged_unasked(
            &candidate.address,
            self.properties.address.street.is_some(),
            found_by,
        );
        candidate
    }

    /// Metres from the point a reverse geocode, or an autocomplete with
    /// `originXY`, measured from.
    pub(super) const fn distance_m(&self) -> Option<f64> {
        self.properties.distance_m
    }

    /// A geocode, verify, lookup or reverse candidate, in the postal form.
    pub(super) fn candidate(&self, product: Product) -> Candidate {
        self.build(product, Style::Postal).0
    }

    /// An autocomplete prediction's candidate. Autocomplete writes the
    /// address in its own case and without the country, names the state by
    /// its code and leaves the county and neighbourhood empty, and says how
    /// the typed text matched each part.
    pub(super) fn prediction(&self, version: Autocomplete) -> Candidate {
        let (mut candidate, us) = self.build(Product::Geocode, Style::Display);
        self.restate_typed(&mut candidate, us.as_ref(), version);
        let address = &mut candidate.address;
        if us.is_some() {
            address.admin2 = Some(AdminField::default());
            address.neighborhood = Some(AdminField::default());
            if let Some(admin1) = address.admin1.as_mut()
                && let Some(code) = admin1.short_name.clone()
            {
                admin1.long_name = Some(code);
            }
        }
        candidate.explanation.address_match.description = self.typed(&candidate.address);
        candidate
    }

    /// The parts of a prediction its `customFields` restate, under the keys
    /// Precisely's examples of each autocomplete write. The first
    /// autocomplete restates the address on one line and the number and
    /// street it matched; an address point is its own first number. Express
    /// restates the address as it formats it and the unit.
    fn restate_typed(
        &self,
        candidate: &mut Candidate,
        us: Option<&UsAddress>,
        version: Autocomplete,
    ) {
        let address = &candidate.address;
        let restated = match version {
            Autocomplete::V1 => {
                let one_line = us
                    .map(|us| {
                        format!(
                            "{}, {}",
                            us.street_line(Style::Display),
                            us.location_line(Style::Display)
                        )
                    })
                    .or_else(|| address.formatted_address.clone());
                let point = self.properties.layer == Layer::Address
                    && self.properties.accuracy == Accuracy::Rooftop;
                vec![
                    ("FORMATTED_STRING", one_line),
                    ("MATCHED_FROM_ADDRESSNUMBER", address.address_number.clone()),
                    (
                        "MATCHED_FROM_STREETNAME",
                        self.street_name_and_type(us).map(|(name, _)| name),
                    ),
                    (
                        "ADDRESS_NUMBER_FROM",
                        address.address_number.clone().filter(|_| point),
                    ),
                ]
            }
            Autocomplete::Express => vec![
                ("FORMATTED_STRING", address.formatted_address.clone()),
                (
                    "UNIT_INFO",
                    us.and_then(|us| us.unit(Style::Display))
                        .or_else(|| self.properties.address.unit.clone()),
                ),
            ],
        };
        for (key, value) in restated {
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                candidate.custom_fields.insert(key, value);
            }
        }
    }

    fn build(&self, product: Product, style: Style) -> (Candidate, Option<UsAddress>) {
        let properties = &self.properties;
        let address = &properties.address;
        let context = &properties.context;
        let layer = properties.layer.as_str();
        let accuracy = properties.accuracy.as_str();
        let is_street_line = matches!(layer, "address" | "interpolation" | "street");
        let area = |area: &Option<Area>| area.as_ref().map(|area| area.name.clone());
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
                long_name: Some(long_name),
            }),
            (None, stated) => AdminField::named(stated),
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
        let admin2 = area(&context.district)
            .or_else(|| area(&context.subregion))
            .or_else(|| own("district"));
        let city = town(
            address.locality.as_deref(),
            context.locality.as_ref().map(|area| area.name.as_str()),
            &[
                admin2.as_deref(),
                region.map(|region| region.name),
                address.region.as_deref(),
            ],
        )
        .map(str::to_string)
        .or_else(|| own("locality"));
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
                neighborhood: AdminField::named(neighborhood),
                suburb: AdminField::named(suburb),
                city: AdminField::named(city),
                admin2: AdminField::named(admin2),
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
        let us = self.us_address(&candidate.address);
        if let Some(us) = &us {
            us.write(&mut candidate, style);
        }
        if style == Style::Postal {
            self.restate_postal(&mut candidate, us.as_ref());
        }
        let holds = Holds::of(&candidate.address);
        candidate.explanation.address_match.description = describe(judged);
        if product == Product::Verify {
            // Verify standardizes an address and does not place it.
            candidate.location = None;
            candidate.custom_fields.remove(GEOHASH);
            if let Some(code) = MatchCode::derive(judged, holds, candidate.score) {
                candidate
                    .custom_fields
                    .insert(SIMPLE_MATCH_CODE, code.simple());
                candidate
                    .custom_fields
                    .insert(EXTENDED_MATCH_CODE, code.extended());
            }
        }
        (candidate, us)
    }

    /// The street's own name and its type: the postal form's for a US
    /// address, og's parts for any other.
    fn street_name_and_type(&self, us: Option<&UsAddress>) -> Option<(String, Option<String>)> {
        if let Some(us) = us {
            return Some((us.street(Style::Postal), us.street_type(Style::Postal)));
        }
        let street = self.properties.address.street.as_deref()?;
        Some(match &self.properties.address.street_parts {
            Some(parts) => (parts.name.clone(), parts.kind.clone()),
            None => (street.to_string(), None),
        })
    }

    /// The parts of a postal answer its `customFields` restate, under the
    /// keys every Precisely example of geocode, verify, lookup and reverse
    /// writes. og keeps one name for a town, so its short, postal and
    /// preferred names are that one. og splits a street's type off only
    /// after its name, so the type is always the one after it. og answers
    /// no intersection.
    fn restate_postal(&self, candidate: &mut Candidate, us: Option<&UsAddress>) {
        let address = &candidate.address;
        let city = address
            .city
            .as_ref()
            .and_then(|city| city.short_name.clone().or_else(|| city.long_name.clone()));
        let (name, kind) = self.street_name_and_type(us).unzip();
        let point = candidate
            .location
            .as_ref()
            .map(|location| location.feature.geometry.coordinates);
        let restated = [
            ("ADDRLINE_SHORT", address.formatted_street_address.clone()),
            ("LASTLINE_SHORT", address.formatted_location_address.clone()),
            ("CITY_SHORT", city.clone()),
            ("NAME_CITY", city.clone()),
            ("PREF_CITY", city),
            ("NAME", name.clone()),
            ("NAME_SHORT", name),
            ("TYPE_SHORT", kind.clone().flatten()),
            ("THOROUGHFARE_TYPE", kind.clone().flatten()),
            ("POST_THOROUGHFARE_TYPE", kind.flatten()),
            ("UNIT_TYPE", address.unit_type.clone()),
            ("INTERSECTION", Some("F".to_string())),
            (
                MATCH_TYPE,
                Some(candidate.explanation.address_match.kind.to_string()),
            ),
            (
                GEOHASH,
                point.map(|[lon, lat]| crate::util::geo::geohash(lat, lon, GEOHASH_CHARS)),
            ),
        ];
        for (key, value) in restated {
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                candidate.custom_fields.insert(key, value);
            }
        }
    }

    /// The candidate's address in the US postal form, for a US house on a
    /// street whose town and state og knows. Every Precisely example of a
    /// US address is written so; og writes no other country's address in a
    /// form Precisely has not shown.
    fn us_address(&self, address: &ResponseAddress) -> Option<UsAddress> {
        let country = address.country.as_ref()?.iso_alpha2_code.as_deref();
        if country != Some("US") {
            return None;
        }
        let parts = self.properties.address.street_parts.as_ref();
        let street = self.properties.address.street.as_deref()?;
        let admin1 = address.admin1.as_ref()?;
        Some(UsAddress::of(&usps::Parts {
            number: address.address_number.as_deref()?,
            prefix: parts.and_then(|parts| parts.prefix.as_deref()),
            name: parts.map_or(street, |parts| parts.name.as_str()),
            kind: parts.and_then(|parts| parts.kind.as_deref()),
            suffix: parts.and_then(|parts| parts.suffix.as_deref()),
            unit_type: address.unit_type.as_deref(),
            unit: address.unit.as_deref(),
            city: address.city.as_ref()?.long_name.as_deref()?,
            state: admin1
                .short_name
                .as_deref()
                .or(admin1.long_name.as_deref())
                .filter(|code| code.len() == 2)?,
            zip: address.postal_code.as_deref(),
            zip4: address.postal_code_ext.as_deref(),
        }))
    }

    /// How the typed text matched each part, from the parts of the label
    /// it marked. Precisely names what matched in capitals. Empty when og
    /// marked nothing.
    fn typed(&self, address: &ResponseAddress) -> Vec<MatchDescription> {
        let label = self.properties.label.chars().collect::<Vec<_>>();
        let marked = self
            .properties
            .highlights
            .iter()
            .filter(|mark| mark.start < mark.end && mark.end <= label.len())
            .map(|mark| label[mark.start..mark.end].iter().collect::<String>())
            .collect::<Vec<_>>();
        if marked.is_empty() {
            return Vec::new();
        }
        let key = |text: &str| text.to_lowercase();
        let judge = |part: Option<&str>| -> (&'static str, Option<String>) {
            let Some(part) = part else {
                return ("NONE", None);
            };
            let words = part.split_whitespace().map(key).collect::<Vec<_>>();
            let hits = marked
                .iter()
                .filter(|text| words.contains(&key(text)))
                .map(|text| text.to_uppercase())
                .collect::<Vec<_>>();
            match hits.len() {
                0 => ("NONE", None),
                n if n == words.len() => ("EXACT", Some(hits.join(" "))),
                _ => ("PARTIAL", Some(hits.join(" "))),
            }
        };
        let street = self.properties.address.street.as_deref();
        let city = address
            .city
            .as_ref()
            .and_then(|city| city.long_name.as_deref());
        LABELS
            .into_iter()
            .filter(|label| *label != "unitMatch")
            .map(|label| {
                let (match_type, matched_substring) = match label {
                    "placeName" => judge(address.place_name.as_deref()),
                    "addressNumber" => judge(address.address_number.as_deref()),
                    "street" => judge(street),
                    "city" => judge(city),
                    "postalCode" => judge(address.postal_code.as_deref()),
                    _ => ("NONE", None),
                };
                MatchDescription {
                    label,
                    match_type,
                    matched_substring,
                }
            })
            .collect()
    }
}

impl UsAddress {
    /// Writes the address into a candidate in `style`.
    fn write(&self, candidate: &mut Candidate, style: Style) {
        let address = &mut candidate.address;
        let street_line = self.street_line(style);
        let last_line = self.last_line(style);
        let country = match style {
            Style::Postal => address
                .country
                .as_ref()
                .and_then(|country| country.name.clone()),
            Style::Display => None,
        };
        let lines = [Some(street_line.clone()), Some(last_line), country]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        address.formatted_address = Some(lines.join(", "));
        address.formatted_street_address = Some(street_line);
        address.formatted_location_address = Some(self.location_line(style));
        address.street = Some(self.street(style));
        address.unit_type = address.unit_type.as_deref().map(usps::designator);
        let city = self.city(style);
        address.city = Some(AdminField::both(city));
        if style == Style::Postal
            && let Some(admin2) = address.admin2.as_mut()
            && let Some(name) = admin2.long_name.as_deref()
        {
            *admin2 = AdminField::both(name.to_uppercase());
        }
        candidate.address_lines = lines;
    }
}

impl AdminField {
    fn named(name: Option<String>) -> Option<Self> {
        name.map(|long_name| Self {
            long_name: Some(long_name),
            short_name: None,
        })
    }

    /// A name Precisely writes as both the long and the short name.
    fn both(name: String) -> Self {
        Self {
            short_name: Some(name.clone()),
            long_name: Some(name),
        }
    }
}

/// The parts a `MatchDescription` names, in the order Precisely lists them.
/// Autocomplete lists them without `unitMatch`.
const LABELS: [&str; 13] = [
    "placeName",
    "addressNumber",
    "admin1",
    "admin2",
    "city",
    "neighborhood",
    "suburb",
    "street",
    "postalCode",
    "streetType",
    "postalCodeExt",
    "streetDirectional",
    "unitMatch",
];

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

/// How each part matched, as Precisely's `MatchDescription` names it, for
/// every part it lists. A part og did not judge, or the query left out, is
/// `NONE`. og judges the street whole, not its type and direction apart.
/// Empty when og judged no part, as of a record looked up by its key.
/// How a record was found when no address was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FoundBy {
    /// Reverse geocoding: the street segment the point lies on.
    Point,
    /// Lookup: the whole record its key names.
    Key,
}

/// Each part judged with no address asked. Found by a point, the record is
/// the street segment the point lies on, so the street (its name, type and
/// direction as one), the city and the state are `EXACT` where the record
/// holds them, and the house number, postcode and unit, which a point does
/// not fix, are `NONE`. Found by its key, the postal parts of the record
/// are read with it and are `EXACT` too where it holds a postcode. A part
/// the record lacks is `NONE`.
fn judged_unasked(
    address: &ResponseAddress,
    street: bool,
    found_by: FoundBy,
) -> Vec<MatchDescription> {
    let held = |part: bool| if part { "EXACT" } else { "NONE" };
    let city = address
        .city
        .as_ref()
        .is_some_and(|city| city.long_name.is_some());
    let state = address
        .admin1
        .as_ref()
        .is_some_and(|admin1| admin1.long_name.is_some() || admin1.short_name.is_some());
    let postal = found_by == FoundBy::Key && address.postal_code.is_some();
    LABELS
        .into_iter()
        .map(|label| MatchDescription {
            label,
            match_type: match label {
                "street" | "streetType" | "streetDirectional" => held(street),
                "city" => held(city),
                "admin1" => held(state),
                "postalCode" | "postalCodeExt" | "unitMatch" => held(postal),
                _ => "NONE",
            },
            matched_substring: None,
        })
        .collect()
}

fn describe(judged: QueryMatch) -> Vec<MatchDescription> {
    if judged.is_empty() {
        return Vec::new();
    }
    LABELS
        .into_iter()
        .map(|label| {
            let part = match label {
                "addressNumber" => judged.housenumber,
                "admin1" => judged.region,
                "city" => judged.locality,
                "street" => judged.street,
                "postalCode" => judged.postcode,
                "unitMatch" => judged.unit,
                _ => None,
            };
            MatchDescription {
                label,
                match_type: match part {
                    Some(PartMatch::Matched) => "EXACT",
                    Some(PartMatch::Partial) => "PARTIAL",
                    Some(PartMatch::Unmatched) | None => "NONE",
                },
                matched_substring: None,
            }
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
                .and_then(|field| field.long_name.clone())
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

    /// og's answer for 350 Jordan Avenue as the Lycoming County source
    /// writes it, `district` standing in for the town.
    fn jordan() -> Feature {
        serde_json::from_value(serde_json::json!({
            "geometry": {"coordinates": [-76.9251736, 41.2480338]},
            "properties": {
                "id": "openaddresses:us/pa/lycoming:row:21956", "layer": "address",
                "accuracy": "rooftop", "name": "350 JORDAN AVENUE",
                "label": "350 JORDAN AVENUE, Lycoming County, PA, 17754",
                "source": {"dataset": "openaddresses:us/pa/lycoming"},
                "address": {
                    "housenumber": "350", "street": "JORDAN AVENUE",
                    "locality": "Lycoming County", "region": "PA", "postcode": "17754",
                    "street_parts": {"name": "JORDAN", "type": "AVENUE"},
                },
                "context": {
                    "locality": {"name": "Montoursville"},
                    "subregion": {"name": "Lycoming County"},
                    "region": {"name": "Pennsylvania", "code": "US-PA"},
                    "country": {"name": "United States", "code": "US"},
                },
                "highlights": [{"start": 0, "end": 3}, {"start": 4, "end": 10}],
            },
        }))
        .expect("an og feature")
    }

    #[test]
    fn a_us_address_is_written_in_the_postal_form_in_its_town() {
        let candidate = serde_json::to_value(jordan().candidate(Product::Geocode)).expect("JSON");
        let address = &candidate["address"];
        assert_eq!(
            address["formattedAddress"],
            "350 JORDAN AVE, MONTOURSVILLE PA 17754, United States"
        );
        assert_eq!(address["formattedStreetAddress"], "350 JORDAN AVE");
        assert_eq!(
            address["formattedLocationAddress"],
            "MONTOURSVILLE, PA  17754"
        );
        assert_eq!(address["street"], "JORDAN");
        assert_eq!(
            address["city"],
            serde_json::json!({"longName": "MONTOURSVILLE", "shortName": "MONTOURSVILLE"})
        );
        assert_eq!(
            address["admin2"],
            serde_json::json!({"longName": "LYCOMING COUNTY", "shortName": "LYCOMING COUNTY"})
        );
        assert_eq!(
            address["admin1"],
            serde_json::json!({"longName": "Pennsylvania", "shortName": "PA"})
        );
        assert_eq!(
            candidate["addressLines"],
            serde_json::json!(["350 JORDAN AVE", "MONTOURSVILLE PA 17754", "United States"])
        );
    }

    #[test]
    fn a_us_prediction_is_written_as_autocomplete_writes_it() {
        let candidate = serde_json::to_value(jordan().prediction(Autocomplete::V1)).expect("JSON");
        let address = &candidate["address"];
        assert_eq!(
            address["formattedAddress"],
            "350 Jordan Ave, Montoursville PA 17754"
        );
        assert_eq!(
            address["formattedLocationAddress"],
            "Montoursville, PA 17754"
        );
        assert_eq!(address["street"], "Jordan Ave");
        assert_eq!(
            address["admin1"],
            serde_json::json!({"longName": "PA", "shortName": "PA"})
        );
        assert_eq!(address["admin2"], serde_json::json!({}));
        assert_eq!(address["neighborhood"], serde_json::json!({}));
        assert_eq!(
            candidate["addressLines"],
            serde_json::json!(["350 Jordan Ave", "Montoursville PA 17754"])
        );
        let described = candidate["explanation"]["addressMatch"]["description"]
            .as_array()
            .expect("description")
            .iter()
            .filter(|part| part["matchType"] != "NONE")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            described,
            [
                serde_json::json!({"label": "addressNumber", "matchType": "EXACT",
                                   "matchedSubstring": "350"}),
                serde_json::json!({"label": "street", "matchType": "PARTIAL",
                                   "matchedSubstring": "JORDAN"}),
            ]
        );
    }

    /// The record's type agrees with any asked word that is a form of it,
    /// so a unit the parser read into the street leaves the type standing.
    #[test]
    fn a_streets_type_agrees_wherever_the_asked_street_writes_it() {
        let judged = |asked: &str| {
            let feature = jordan();
            let mut candidate = feature.candidate(Product::Geocode);
            candidate.explanation.address_match.description = LABELS
                .into_iter()
                .map(|label| MatchDescription {
                    label,
                    match_type: if label == "street" { "EXACT" } else { "NONE" },
                    matched_substring: None,
                })
                .collect();
            feature.judge_street_parts(&mut candidate, Some(asked));
            let of = |label: &str| {
                candidate
                    .explanation
                    .address_match
                    .description
                    .iter()
                    .find(|entry| entry.label == label)
                    .map(|entry| entry.match_type)
            };
            (of("streetType"), of("streetDirectional"))
        };
        assert_eq!(judged("jordan ave ste"), (Some("EXACT"), Some("EXACT")));
        assert_eq!(judged("Jordan Avenue"), (Some("EXACT"), Some("EXACT")));
        assert_eq!(judged("jordan rd"), (Some("NONE"), Some("EXACT")));
        assert_eq!(judged("jordan"), (Some("NONE"), Some("EXACT")));
        assert_eq!(judged("n jordan ave"), (Some("EXACT"), Some("NONE")));
    }

    /// A postal answer restates its parts in the postal form, as the
    /// documented answers for `1700 DISTRICT AVE STE 300` do.
    #[test]
    fn a_us_answer_restates_its_parts_in_the_postal_form() {
        let fields = jordan()
            .candidate(Product::Geocode)
            .in_language(Some("en-US"))
            .custom_fields;
        let restated = fields
            .iter()
            .filter(|(key, _)| **key != ID_FIELD)
            .map(|(key, value)| (*key, value.as_str()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            restated,
            BTreeMap::from([
                ("ADDRLINE_SHORT", "350 JORDAN AVE"),
                ("LASTLINE_SHORT", "MONTOURSVILLE, PA  17754"),
                ("CITY_SHORT", "MONTOURSVILLE"),
                ("NAME_CITY", "MONTOURSVILLE"),
                ("PREF_CITY", "MONTOURSVILLE"),
                ("NAME", "JORDAN"),
                ("NAME_SHORT", "JORDAN"),
                ("TYPE_SHORT", "AVE"),
                ("THOROUGHFARE_TYPE", "AVE"),
                ("POST_THOROUGHFARE_TYPE", "AVE"),
                ("INTERSECTION", "F"),
                ("MATCH_TYPE", "ADDRESS"),
                ("GEOHASH", "dr369sbz1wc1"),
                ("LANGUAGE", "en"),
            ])
        );
        let reverse = jordan().reverse_candidate().custom_fields;
        assert!(!reverse.contains_key(GEOHASH) && !reverse.contains_key(MATCH_TYPE));
        let verified = jordan().candidate(Product::Verify).custom_fields;
        assert!(!verified.contains_key(GEOHASH) && verified.contains_key(MATCH_TYPE));
    }

    /// Each autocomplete restates what its documented predictions do: the
    /// first the address on one line and the number and street matched,
    /// express its formatted address and the unit.
    #[test]
    fn a_prediction_restates_what_its_autocomplete_does() {
        let fields = |feature: &Feature, version| {
            feature
                .prediction(version)
                .custom_fields
                .into_iter()
                .filter(|(key, _)| *key != ID_FIELD)
                .collect::<BTreeMap<_, _>>()
        };
        let owned = |pairs: &[(&'static str, &str)]| {
            pairs
                .iter()
                .map(|(key, value)| (*key, value.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            fields(&jordan(), Autocomplete::V1),
            owned(&[
                (
                    "FORMATTED_STRING",
                    "350 Jordan Ave, Montoursville, PA 17754"
                ),
                ("MATCHED_FROM_ADDRESSNUMBER", "350"),
                ("MATCHED_FROM_STREETNAME", "JORDAN"),
                ("ADDRESS_NUMBER_FROM", "350"),
            ])
        );
        let mut suite = jordan();
        suite.properties.address.unit = Some("STE 2".to_string());
        let express = fields(&suite, Autocomplete::Express);
        assert_eq!(express.get("UNIT_INFO").map(String::as_str), Some("Ste 2"));
        assert!(express.contains_key("FORMATTED_STRING"));
        assert!(!express.contains_key("MATCHED_FROM_ADDRESSNUMBER"));
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
