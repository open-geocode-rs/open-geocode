//! og features as HERE result items.

use serde::{Deserialize, Serialize};

use crate::{
    api::{self, Accuracy, ForwardFeature, Highlight, PartMatch, QueryMatch, ReverseFeature},
    compat::{self, encode_parts},
    iso,
    record::{Contact, Layer},
    spatial_index::haversine_m,
};

use super::{
    LatLng,
    category::{self, ItemCategory},
    food_types,
    hours::{self, OpeningHours},
};

/// What HERE reads of an og feature. A live answer converts from og's own
/// types; a batch job's results come back from disk as og's JSON, and that
/// boundary reads the same fields (pinned equal by a test).
#[derive(Debug, PartialEq, Deserialize)]
pub(super) struct Feature {
    geometry: Geometry,
    properties: Properties,
}

#[derive(Debug, PartialEq, Deserialize)]
struct Geometry {
    coordinates: [f64; 2],
}

#[derive(Debug, PartialEq, Deserialize)]
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
    bbox: Option<[f64; 4]>,
    #[serde(rename = "match")]
    matched: Option<QueryMatch>,
    #[serde(default)]
    highlights: Vec<Highlight>,
    #[serde(default)]
    access: Vec<Access>,
    /// An area's own ISO code (`US`, `US-RI`).
    code: Option<String>,
    contact: Option<Contact>,
    /// OpenStreetMap's `opening_hours`, as written.
    opening_hours: Option<String>,
    /// The address as the lines of a postal address, in its country's form.
    #[serde(default)]
    address_lines: Vec<String>,
    /// The IANA zone the result lies in.
    timezone: Option<String>,
    /// The kinds of food a place serves, as its `cuisine` tag lists them.
    #[serde(default)]
    cuisine: Vec<String>,
}

/// A way in og maps for the result.
#[derive(Debug, PartialEq, Deserialize)]
struct Access {
    lon: f64,
    lat: f64,
}

#[derive(Debug, Default, PartialEq, Deserialize)]
struct Address {
    housenumber: Option<String>,
    block: Option<String>,
    subblock: Option<String>,
    unit: Option<String>,
    street: Option<String>,
    #[serde(default)]
    areas: Vec<String>,
    locality: Option<String>,
    region: Option<String>,
    postcode: Option<String>,
    country: Option<String>,
}

#[derive(Debug, Default, PartialEq, Deserialize)]
struct Context {
    neighbourhood: Option<Area>,
    sublocality: Option<Area>,
    locality: Option<Area>,
    district: Option<Area>,
    region: Option<Area>,
    country: Option<Area>,
}

#[derive(Debug, PartialEq, Deserialize)]
struct Area {
    name: String,
    code: Option<String>,
    plate_code: Option<String>,
}

impl Area {
    fn of(area: &api::ContextArea) -> Self {
        Self {
            name: area.name.clone(),
            code: area.code.clone(),
            plate_code: area.plate_code.clone(),
        }
    }
}

/// The parts of a forward or a reverse answer HERE reads that `Common`
/// does not hold.
struct Extra {
    confidence: Option<f32>,
    distance_m: Option<f64>,
    matched: Option<QueryMatch>,
    highlights: Vec<Highlight>,
}

impl Feature {
    /// How far the result lies from a reverse geocode's point, none for an
    /// area holding the point.
    pub(super) fn distance_m(&self) -> f64 {
        self.properties.distance_m.unwrap_or(0.0)
    }

    fn of(coordinates: [f64; 2], common: &api::Common, extra: Extra) -> Self {
        let address = &common.address;
        let context = &common.context;
        let area = |area: &Option<api::ContextArea>| area.as_ref().map(Area::of);
        let (block, subblock) =
            if compat::country_code(context).or(address.country.as_deref()) == Some("JP") {
                address
                    .block_parts()
                    .map_or((None, None), |(block, house)| {
                        (Some(block.to_string()), house.map(str::to_string))
                    })
            } else {
                (None, None)
            };
        Self {
            geometry: Geometry { coordinates },
            properties: Properties {
                id: common.id.clone(),
                layer: common.layer,
                accuracy: common.accuracy,
                name: common.name.clone(),
                label: common.label.clone(),
                address: Address {
                    housenumber: address.housenumber.clone(),
                    block,
                    subblock,
                    unit: address.unit.clone(),
                    // A US street in the postal abbreviation, as its label
                    // writes it.
                    street: match compat::country_code(context) {
                        Some("US") => compat::us_street(address).or_else(|| address.street.clone()),
                        _ => address.street.clone(),
                    },
                    areas: address.areas.clone(),
                    // A stated town as a vendor writes it, in the
                    // locality's spelling.
                    locality: address
                        .locality
                        .as_ref()
                        .and_then(|_| compat::town_of(address, context))
                        .map(str::to_string),
                    region: address.region.clone(),
                    postcode: address.postcode.clone(),
                    country: address.country.clone(),
                },
                context: Context {
                    neighbourhood: area(&context.neighbourhood),
                    sublocality: area(&context.sublocality),
                    locality: area(&context.locality),
                    district: area(&context.district),
                    region: area(&context.region),
                    country: area(&context.country),
                },
                confidence: extra.confidence,
                distance_m: extra.distance_m,
                category: common.category.clone(),
                bbox: common.bbox,
                matched: extra.matched,
                highlights: extra.highlights,
                code: common.code.clone(),
                contact: common.contact.clone(),
                opening_hours: common.opening_hours.clone(),
                timezone: common.timezone.clone(),
                cuisine: common.cuisine.clone(),
                address_lines: compat::postal_lines(common.layer, &common.address, &common.context),
                access: common
                    .access
                    .iter()
                    .map(|point| Access {
                        lon: point.lon,
                        lat: point.lat,
                    })
                    .collect(),
            },
        }
    }
}

impl From<&ForwardFeature> for Feature {
    fn from(feature: &ForwardFeature) -> Self {
        let properties = &feature.properties;
        Self::of(
            feature.geometry.coordinates,
            &properties.common,
            Extra {
                confidence: properties.confidence,
                distance_m: properties.distance_m,
                matched: properties.matched,
                highlights: properties.highlights.clone(),
            },
        )
    }
}

impl From<&ReverseFeature> for Feature {
    fn from(feature: &ReverseFeature) -> Self {
        Self::of(
            feature.geometry.coordinates,
            &feature.properties.common,
            Extra {
                confidence: None,
                distance_m: feature.properties.distance_m,
                matched: None,
                highlights: Vec::new(),
            },
        )
    }
}

/// A HERE result item. Every field og has no value for is left out, as HERE
/// leaves out what it lacks.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Item {
    pub(super) title: String,
    pub(super) id: String,
    pub(super) result_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) house_number_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) locality_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) administrative_area_type: Option<&'static str>,
    /// A query item's search, a `/discover` URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) href: Option<String>,
    #[serde(skip_serializing_if = "ItemAddress::is_empty")]
    pub(super) address: ItemAddress,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) position: Option<Position>,
    /// The entrances og maps for the result, HERE's points to route to.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) access: Vec<Position>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) distance: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) map_view: Option<MapView>,
    /// The HERE categories of a place whose kind has one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) categories: Vec<ItemCategory>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) food_types: Vec<ItemCategory>,
    /// How to reach a place, which only place answers carry.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) contacts: Vec<ContactInformation>,
    /// When a place is open, which only place answers carry.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) opening_hours: Vec<OpeningHours>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) highlights: Option<Highlights>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) scoring: Option<Scoring>,
    /// How og read the query, part by part, when `show=parsing` asks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) parsing: Option<Parsing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) time_zone: Option<ItemTimeZone>,
}

/// HERE's `timeZone`, the IANA zone and its offset from UTC when the
/// request is answered.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ItemTimeZone {
    pub(super) name: String,
    pub(super) utc_offset: String,
}

/// A place's contacts by kind, each value as its source writes it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct ContactInformation {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    phone: Vec<ContactValue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    www: Vec<ContactValue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    email: Vec<ContactValue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fax: Vec<ContactValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ContactValue {
    value: String,
}

impl ContactInformation {
    fn is_empty(&self) -> bool {
        self.phone.is_empty() && self.www.is_empty() && self.email.is_empty() && self.fax.is_empty()
    }

    /// og's contact as HERE's. OpenStreetMap separates several values of one
    /// kind with `;`, and HERE lists each.
    fn of(contact: &Contact) -> Self {
        let values = |value: &Option<String>| {
            value
                .iter()
                .flat_map(|value| value.split(';'))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| ContactValue {
                    value: value.to_string(),
                })
                .collect()
        };
        Self {
            phone: values(&contact.phone),
            www: values(&contact.website),
            email: values(&contact.email),
            fax: values(&contact.fax),
        }
    }
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
    block: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subblock: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) county: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) county_code: Option<String>,
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

impl ItemAddress {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(super) struct Position {
    pub(super) lat: f64,
    pub(super) lng: f64,
}

/// The box a result covers, from og's `bbox`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(super) struct MapView {
    pub(super) west: f64,
    pub(super) south: f64,
    pub(super) east: f64,
    pub(super) north: f64,
}

/// Where the typed text lies in the title and the address label, in
/// characters, end exclusive.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct Highlights {
    title: Vec<Highlight>,
    /// A query item has a title and no address, so it marks none.
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<LabelHighlights>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct LabelHighlights {
    label: Vec<Highlight>,
    /// Each address field's marks, where the typed text marks it.
    #[serde(flatten)]
    fields: std::collections::BTreeMap<&'static str, Vec<Highlight>>,
}

/// The marks of `label` that fall in `value`, where `label` writes it,
/// counted from the start of `value`.
fn field_marks(label: &str, marks: &[Highlight], value: &str) -> Vec<Highlight> {
    let Some(byte) = label.find(value) else {
        return Vec::new();
    };
    let start = label[..byte].chars().count();
    let end = start + value.chars().count();
    marks
        .iter()
        .filter(|mark| mark.start < end && mark.end > start)
        .map(|mark| Highlight {
            start: mark.start.max(start) - start,
            end: mark.end.min(end) - start,
        })
        .collect()
}

/// HERE's `parsing`: each part of the query og read, by HERE's name of it,
/// with where it stands in the query.
pub(super) type Parsing = std::collections::BTreeMap<&'static str, Vec<ParsedTerm>>;

/// A part of the query: its text, where it starts and ends in characters,
/// and the `qq` field it came from when it came from one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct ParsedTerm {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) qq: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Scoring {
    pub(super) query_score: f64,
    #[serde(skip_serializing_if = "FieldScore::is_empty")]
    field_score: FieldScore,
}

/// og's `match`, part by part, as HERE scores a field. HERE sends a field
/// only when it matched the query, so an unmatched part is left out.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct FieldScore {
    #[serde(skip_serializing_if = "Option::is_none")]
    country: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    city: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    streets: Vec<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    district: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    place_name: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    house_number: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    postal_code: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit: Option<f64>,
}

impl FieldScore {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A part's share of a full match. `partial` is og's word for a value
/// accepted as the same though it differs (a typo, a range estimate, the
/// postal town), so it counts half.
const fn part_score(part: PartMatch) -> f64 {
    match part {
        PartMatch::Matched => 1.0,
        PartMatch::Partial => 0.5,
        PartMatch::Unmatched => 0.0,
    }
}

/// HERE's scoring of an og answer, from what og judged. `queryScore` is
/// the mean share of the parts the query states, each by [`part_score`],
/// which is 1 when every stated part matched. A result og judged on no part,
/// found by its name, takes og's calibrated confidence when the Pack has
/// one. With neither, og judged nothing, and no score is sent.
fn scoring(properties: &Properties) -> Option<Scoring> {
    let matched = properties.matched.unwrap_or_default();
    let parts = [
        matched.country,
        matched.region,
        matched.locality,
        matched.street,
        matched.housenumber,
        matched.postcode,
        matched.unit,
    ];
    let stated = parts
        .iter()
        .flatten()
        .map(|part| part_score(*part))
        .collect::<Vec<_>>();
    let query_score = match stated.is_empty() {
        false => stated.iter().sum::<f64>() / stated.len() as f64,
        true => f64::from(properties.confidence?),
    };
    let field = |part: Option<PartMatch>| {
        part.and_then(|part| match part {
            PartMatch::Matched | PartMatch::Partial => Some(part_score(part)),
            PartMatch::Unmatched => None,
        })
    };
    Some(Scoring {
        query_score,
        field_score: FieldScore {
            country: field(matched.country),
            state: field(matched.region),
            city: field(matched.locality),
            streets: field(matched.street).into_iter().collect(),
            house_number: field(matched.housenumber),
            postal_code: field(matched.postcode),
            unit: field(matched.unit),
            ..FieldScore::default()
        },
    })
}

/// HERE's kind of result for an og layer, as `resultType`, then
/// `localityType` or `administrativeAreaType`.
fn kind(layer: Layer) -> (&'static str, Option<&'static str>, Option<&'static str>) {
    match layer {
        Layer::Address | Layer::Interpolation => ("houseNumber", None, None),
        Layer::Street => ("street", None, None),
        Layer::Poi => ("place", None, None),
        Layer::Postcode => ("postalCodePoint", None, None),
        Layer::Locality => ("locality", Some("city"), None),
        // og's neighbourhood is a part of a town, which is HERE's district.
        Layer::Neighbourhood => ("locality", Some("district"), None),
        // og's district is the tier between region and town, a county.
        Layer::District => ("administrativeArea", None, Some("county")),
        Layer::Region => ("administrativeArea", None, Some("state")),
        Layer::Country => ("administrativeArea", None, Some("country")),
        // A named area outside the admin tiers, usually an island. HERE has
        // no type of locality for one.
        Layer::Place => ("locality", None, None),
    }
}

/// The HERE item for an og feature, with `position` set, `distance` from
/// og's own `distance_m` (a reverse answer's), `mapView` from og's `bbox`
/// and `scoring` from og's `match`.
pub(super) fn item(feature: &Feature) -> Item {
    let properties = &feature.properties;
    let (mut result_type, locality_type, administrative_area_type) = kind(properties.layer);
    // An interpolation range found by its street alone, with no number
    // asked, is a street to HERE.
    if properties.layer == Layer::Interpolation && properties.address.housenumber.is_none() {
        result_type = "street";
    }
    let house_number_type = match (result_type, properties.accuracy) {
        ("houseNumber", Accuracy::Interpolated) => Some("interpolated"),
        // The address's own point or building is a point address.
        ("houseNumber", Accuracy::Rooftop | Accuracy::Centroid) => Some("PA"),
        _ => None,
    };
    let [lon, lat] = feature.geometry.coordinates;
    Item {
        title: title(properties),
        id: encode_parts(&properties.id, Some(properties.layer), &properties.label),
        result_type,
        house_number_type,
        locality_type,
        administrative_area_type,
        href: None,
        address: address(properties),
        position: Some(Position { lat, lng: lon }),
        access: properties
            .access
            .iter()
            .map(|point| Position {
                lat: point.lat,
                lng: point.lon,
            })
            .collect(),
        // HERE counts whole meters.
        distance: properties
            .distance_m
            .map(|distance| distance.round() as i64),
        // HERE gives a place no box, whatever outline it has.
        map_view: properties.bbox.filter(|_| result_type != "place").map(
            |[west, south, east, north]| MapView {
                west,
                south,
                east,
                north,
            },
        ),
        categories: properties
            .category
            .as_deref()
            .map(category::of_place)
            .unwrap_or_default(),
        food_types: ItemCategory::listed(food_types::of_cuisine(&properties.cuisine)),
        contacts: Vec::new(),
        opening_hours: Vec::new(),
        highlights: None,
        scoring: scoring(properties),
        parsing: None,
        time_zone: None,
    }
}

/// A place is titled by its name, a house or street by its postal label,
/// anything else by og's label.
fn title(properties: &Properties) -> String {
    match properties.layer {
        Layer::Poi => properties.name.clone(),
        _ => label(properties),
    }
}

/// HERE's label: for a house or street its postal address lines in its
/// country's form, joined (`Heinestraße 42, 1020 Wien, Austria`), else
/// og's label. The town is the one [`compat::town`] names, and a US street
/// is abbreviated as the postal service writes it, as HERE writes both.
fn label(properties: &Properties) -> String {
    postal_label(properties).unwrap_or_else(|| properties.label.clone())
}

fn postal_label(properties: &Properties) -> Option<String> {
    let lines = &properties.address_lines;
    if !matches!(
        properties.layer,
        Layer::Address | Layer::Interpolation | Layer::Street
    ) || lines.len() < 2
    {
        return None;
    }
    let address = &properties.address;
    let mut lines = lines.clone();
    // HERE writes the house's own line first; a country whose post puts
    // the unit on a line above it still gets the house line first.
    if let Some(street) = address.street.as_deref()
        && let Some(at) = lines[1..].iter().position(|line| line.contains(street))
    {
        let line = lines.remove(at + 1);
        lines.insert(0, line);
    }
    Some(lines.join(", "))
}

/// og's highlights mark og's label. Each marked text is found again in
/// HERE's label, which may write the parts in another order, and the title
/// keeps the marks that lie in it when the label starts with it.
fn highlights(properties: &Properties) -> Highlights {
    let label = label(properties);
    let marks = moved(&properties.label, &properties.highlights, &label);
    let title = title(properties);
    let in_title = match label.starts_with(&title) {
        true => {
            let length = title.chars().count();
            marks
                .iter()
                .filter(|mark| mark.start < length)
                .map(|mark| Highlight {
                    start: mark.start,
                    end: mark.end.min(length),
                })
                .collect()
        }
        false => Vec::new(),
    };
    Highlights {
        title: in_title,
        address: Some(LabelHighlights {
            label: marks,
            fields: Default::default(),
        }),
    }
}

/// `marks` on `from`, placed on `to` where the text each marks is found,
/// in order and without overlap, ignoring case. A mark whose text `to` does
/// not hold is dropped.
fn moved(from: &str, marks: &[Highlight], to: &str) -> Vec<Highlight> {
    if from == to {
        return marks.to_vec();
    }
    let from = from.chars().collect::<Vec<_>>();
    let to_chars = to.chars().flat_map(char::to_lowercase).collect::<Vec<_>>();
    // Lowercasing can change a character's length, and then offsets would
    // not line up; such a label keeps no marks.
    if to_chars.len() != to.chars().count() {
        return Vec::new();
    }
    let mut taken = vec![false; to_chars.len()];
    let mut placed = Vec::new();
    for mark in marks {
        let Some(text) = from.get(mark.start..mark.end.min(from.len())) else {
            continue;
        };
        let needle = text
            .iter()
            .flat_map(|c| c.to_lowercase())
            .collect::<Vec<_>>();
        if needle.is_empty() || needle.len() != text.len() {
            continue;
        }
        let found = (0..=to_chars.len().saturating_sub(needle.len())).find(|&at| {
            to_chars.len() >= at + needle.len()
                && to_chars[at..at + needle.len()] == needle[..]
                && !taken[at..at + needle.len()].contains(&true)
        });
        if let Some(at) = found {
            taken[at..at + needle.len()]
                .iter_mut()
                .for_each(|t| *t = true);
            placed.push(Highlight {
                start: at,
                end: at + needle.len(),
            });
        }
    }
    placed.sort_by_key(|mark| mark.start);
    placed
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
    // An area's own code names it where no context area does.
    let own_code = |layer: Layer| {
        properties
            .code
            .as_deref()
            .filter(|_| properties.layer == layer)
    };
    let country_code = context
        .country
        .as_ref()
        .and_then(|country| country.code.as_deref())
        .or_else(|| own_code(Layer::Country))
        // OSM's `addr:country` is the two-letter code by convention.
        .or(stated.country.as_deref())
        .and_then(|code| iso::alpha3(&code.to_ascii_uppercase()));
    // HERE's district is the first tier below the town and its subdistrict
    // the one below that. og's sublocality and neighbourhood are those two
    // tiers when both are there, and a lone neighbourhood is the district,
    // as a neighbourhood found as itself is.
    let (district, subdistrict) = match (area(&context.sublocality), area(&context.neighbourhood)) {
        (Some(sublocality), neighbourhood) => (Some(sublocality), neighbourhood),
        (None, Some(neighbourhood)) => (Some(neighbourhood), None),
        (None, None) => (stated.areas.first().cloned(), stated.areas.get(1).cloned()),
    };
    ItemAddress {
        label: Some(label(properties)),
        country_code,
        country_name: area(&context.country).or_else(|| own(Layer::Country)),
        state_code: context
            .region
            .as_ref()
            .and_then(|region| region.code.as_deref())
            .or_else(|| own_code(Layer::Region))
            .and_then(|code| code.split_once('-'))
            .map(|(_, own)| own.to_string()),
        // HERE names a state and a county without the word for its kind.
        state: area(&context.region)
            .map(|name| compat::without_kind(&name))
            .or_else(|| stated.region.clone())
            .or_else(|| own(Layer::Region)),
        county: area(&context.district)
            .or_else(|| own(Layer::District))
            .map(|name| compat::without_kind(&name)),
        // A county's own code is the one on its vehicle plates (`SIM`). An
        // ISO code would invent one where HERE writes none (`75C` of Paris).
        county_code: context
            .district
            .as_ref()
            .and_then(|county| county.plate_code.clone()),
        city: area(&context.locality)
            .or_else(|| stated.locality.clone())
            .or_else(|| own(Layer::Locality)),
        district: district.or_else(|| own(Layer::Neighbourhood)),
        subdistrict,
        street: stated.street.clone().or_else(|| own(Layer::Street)),
        postal_code: stated.postcode.clone(),
        house_number: stated.housenumber.clone(),
        block: stated.block.clone(),
        subblock: stated.subblock.clone(),
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

    pub(super) fn with_name_scores(mut self, district: Option<&str>, query: Option<&str>) -> Self {
        let equal = |got: &str, asked: &str| {
            crate::util::text::normalize_for_compare(got)
                == crate::util::text::normalize_for_compare(asked)
        };
        let district = self
            .address
            .district
            .as_deref()
            .zip(district)
            .is_some_and(|(got, asked)| equal(got, asked));
        let place =
            self.result_type == "place" && query.is_some_and(|asked| equal(&self.title, asked));
        if place && self.scoring.is_none() {
            self.scoring = Some(Scoring {
                query_score: 1.0,
                field_score: FieldScore::default(),
            });
        }
        if let Some(scoring) = &mut self.scoring {
            scoring.field_score.district = district.then_some(1.0);
            scoring.field_score.place_name = place.then_some(1.0);
        }
        self
    }

    /// Only `/geocode` scores its items.
    pub(super) fn without_scoring(mut self) -> Self {
        self.scoring = None;
        self
    }

    /// Where the typed text lies, which only suggestions carry.
    /// A query item, a search HERE offers to run: `categoryQuery` or
    /// `chainQuery`, with its title, the `/discover` URL that runs it and
    /// where the typed text lies in its title.
    pub(super) fn query(
        result_type: &'static str,
        title: String,
        id: String,
        href: String,
        marks: Vec<Highlight>,
    ) -> Self {
        Self {
            title,
            id,
            result_type,
            house_number_type: None,
            locality_type: None,
            administrative_area_type: None,
            href: Some(href),
            address: ItemAddress::default(),
            position: None,
            access: Vec::new(),
            distance: None,
            map_view: None,
            categories: Vec::new(),
            food_types: Vec::new(),
            contacts: Vec::new(),
            opening_hours: Vec::new(),
            highlights: Some(Highlights {
                title: marks,
                address: None,
            }),
            scoring: None,
            parsing: None,
            time_zone: None,
        }
    }

    pub(super) fn with_highlights(mut self, feature: &Feature) -> Self {
        let mut found = highlights(&feature.properties);
        let address = &self.address;
        if let Some(label) = address.label.as_deref() {
            let fields = [
                ("countryName", address.country_name.as_deref()),
                ("state", address.state.as_deref()),
                ("county", address.county.as_deref()),
                ("city", address.city.as_deref()),
                ("district", address.district.as_deref()),
                ("subdistrict", address.subdistrict.as_deref()),
                ("street", address.street.as_deref()),
                ("postalCode", address.postal_code.as_deref()),
                ("houseNumber", address.house_number.as_deref()),
            ];
            if let Some(marked) = found.address.as_mut() {
                for (name, value) in fields {
                    let marks = value
                        .map_or_else(Vec::new, |value| field_marks(label, &marked.label, value));
                    if !marks.is_empty() {
                        marked.fields.insert(name, marks);
                    }
                }
            }
        }
        self.highlights = Some(found);
        self
    }

    /// An autocomplete suggestion's title, its parts from the country down
    /// as HERE's autocomplete writes it: `Deutschland, 60486, Frankfurt am
    /// Main, Pariser Straße 2`. A street spans postcodes, so it names none.
    /// The typed text's marks follow their text into it.
    pub(super) fn titled_country_first(mut self, feature: &Feature) -> Self {
        let properties = &feature.properties;
        let address = &self.address;
        let own_line = match self.result_type {
            "houseNumber" | "street" => postal_label(properties)
                .and_then(|label| label.split(", ").next().map(str::to_string))
                .or_else(|| Some(properties.name.clone())),
            _ => None,
        };
        let parts = [
            address.country_name.clone(),
            address
                .postal_code
                .clone()
                .filter(|_| self.result_type != "street"),
            address.city.clone(),
            own_line,
        ]
        .into_iter()
        .flatten()
        .fold(Vec::<String>::new(), |mut parts, part| {
            if !parts.contains(&part) {
                parts.push(part);
            }
            parts
        });
        if parts.len() < 2 {
            return self;
        }
        self.title = parts.join(", ");
        if let Some(highlights) = self.highlights.as_mut() {
            highlights.title = moved(&properties.label, &properties.highlights, &self.title);
        }
        self
    }

    /// The zone the result lies in and its offset now, when `show=tz` asks
    /// for it.
    pub(super) fn with_time_zone(mut self, feature: &Feature, shown: bool) -> Self {
        self.time_zone = feature
            .properties
            .timezone
            .as_deref()
            .filter(|_| shown)
            .and_then(|name| {
                let offset = crate::compat::time_zone::Offset::now(name)?;
                Some(ItemTimeZone {
                    name: name.to_string(),
                    utc_offset: offset.text(),
                })
            });
        self
    }

    /// A place's contacts and opening hours, which only the answers that
    /// carry place details have: discover, browse, lookup and autosuggest.
    pub(super) fn with_details(mut self, feature: &Feature) -> Self {
        let properties = &feature.properties;
        self.contacts = properties
            .contact
            .as_ref()
            .map(ContactInformation::of)
            .filter(|contacts| !contacts.is_empty())
            .into_iter()
            .collect();
        let kind = self.categories.first().map(|category| category.id);
        self.opening_hours = properties
            .opening_hours
            .as_deref()
            .and_then(|value| hours::parse(value, properties.timezone.as_deref()))
            .map(|hours| hours.of_kind(kind))
            .into_iter()
            .collect();
        self
    }

    /// An `/autocomplete` suggestion has no point, no way in and no box.
    pub(super) fn without_position(mut self) -> Self {
        self.position = None;
        self.access = Vec::new();
        self.map_view = None;
        self
    }

    /// Whether a `types` value admits the item, by HERE's definitions.
    pub(super) fn is_of(&self, kind: &str) -> bool {
        let result_type = self.result_type;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn feature(properties: &serde_json::Value) -> Feature {
        serde_json::from_value(serde_json::json!({
            "geometry": {"coordinates": [16.39, 48.218]},
            "properties": properties,
        }))
        .expect("an og feature")
    }

    #[test]
    fn a_house_is_labelled_by_its_postal_lines_in_its_town() {
        let vienna = feature(&serde_json::json!({
            "id": "osm:node:1", "layer": "address", "accuracy": "rooftop",
            "name": "42 Heinestraße", "label": "42 Heinestraße, Leopoldstadt, 1020, AT",
            "address": {"housenumber": "42", "street": "Heinestraße",
                        "locality": "Leopoldstadt", "postcode": "1020"},
            "context": {"sublocality": {"name": "Leopoldstadt"}, "locality": {"name": "Wien"},
                        "country": {"name": "Austria", "code": "AT"}},
            "address_lines": ["Heinestraße 42", "1020 Wien", "Austria"],
            "highlights": [{"start": 3, "end": 14}],
        }));
        let found = item(&vienna);
        assert_eq!(found.title, "Heinestraße 42, 1020 Wien, Austria");
        assert_eq!(found.address.label.as_deref(), Some(found.title.as_str()));
        let marks = highlights(&vienna.properties);
        assert_eq!(
            marks.address.as_ref().map(|address| address.label.clone()),
            Some(vec![Highlight { start: 0, end: 11 }])
        );
        assert_eq!(marks.title, [Highlight { start: 0, end: 11 }]);

        let boston = feature(&serde_json::json!({
            "id": "osm:node:2", "layer": "address", "accuracy": "rooftop",
            "name": "240 Washington Street", "label": "240 Washington Street, Boston, MA, 02108",
            "address": {"housenumber": "240", "street": "Washington Street",
                        "locality": "Boston", "region": "MA", "postcode": "02108"},
            "context": {"locality": {"name": "Boston"},
                        "country": {"name": "United States", "code": "US"}},
            "address_lines": ["240 Washington St", "Boston, MA 02108", "United States"],
        }));
        assert_eq!(
            item(&boston).title,
            "240 Washington St, Boston, MA 02108, United States"
        );
    }

    #[test]
    fn a_field_takes_the_marks_inside_it_from_its_own_start() {
        let label = "Pariser Platz 1, 10117 Berlin, Deutschland";
        let marks = [
            Highlight { start: 0, end: 5 },
            Highlight { start: 23, end: 26 },
        ];
        assert_eq!(
            field_marks(label, &marks, "Pariser Platz"),
            [Highlight { start: 0, end: 5 }]
        );
        assert_eq!(
            field_marks(label, &marks, "Berlin"),
            [Highlight { start: 0, end: 3 }]
        );
        assert_eq!(field_marks(label, &marks, "10117"), []);
        assert_eq!(
            field_marks(label, &marks, "Paris"),
            [Highlight { start: 0, end: 5 }]
        );
        assert_eq!(field_marks(label, &marks, "Wien"), []);
    }

    #[test]
    fn marks_follow_their_text_to_the_new_label() {
        let from = "15 Schillerstraße, Frankfurt, 60313";
        let marks = [
            Highlight { start: 0, end: 2 },
            Highlight { start: 3, end: 9 },
        ];
        let to = "Schillerstraße 15, 60313 Frankfurt";
        assert_eq!(
            moved(from, &marks, to),
            [
                Highlight { start: 0, end: 6 },
                Highlight { start: 15, end: 17 }
            ]
        );
        assert_eq!(moved(from, &marks, "Elsewhere"), []);
    }
}
