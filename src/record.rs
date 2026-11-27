use std::collections::BTreeMap;

use geojson::{Geometry, GeometryValue};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};

use crate::{labels, parse::lattice::CountryCode, util::geo::point_lon_lat};

#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Address(AddressRecord),
    Interpolation(InterpolationRecord),
    Street(StreetRecord),
    Postcode(PostcodeRecord),
    Place(PlaceLayer, PlaceRecord),
    Poi(PoiRecord),
}

/// The building an address is drawn as, when its source maps it as one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Building {
    /// Its name, as its `name` tag writes it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    #[schema(example = "Google Building 41")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AddressRecord {
    pub address: AddressComponents,
    pub building: Option<Building>,
    pub street_postcodes: Vec<String>,
    pub geometry: Geometry,
    pub location_precision: LocationPrecision,
    pub source: SourceProvenance,
    pub disputed: bool,
    pub access: Vec<Entrance>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InterpolationRecord {
    pub address: InterpolationAddressComponents,
    pub interpolation: InterpolationRange,
    pub anchor_node_ids: [i64; 2],
    pub geometry: Geometry,
    pub representative_point: [f64; 2],
    pub source: SourceProvenance,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreetRecord {
    pub name: String,
    pub road_class: RoadClass,
    pub alt_names: Vec<String>,
    pub locality: Option<String>,
    pub geometry: Geometry,
    pub representative_point: [f64; 2],
    pub source: SourceProvenance,
    /// Ascending. A tile names the way a tap lands on.
    pub other_ways: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PostcodeRecord {
    pub postcode: String,
    pub country: Option<CountryCode>,
    pub geometry: Geometry,
    pub source: DerivedSourceProvenance,
}

/// What kind of road a street is, from its OSM `highway` tag, most important
/// first. A link road (`motorway_link`) is its road's class.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum RoadClass {
    Motorway = 0,
    Trunk = 1,
    Primary = 2,
    Secondary = 3,
    Tertiary = 4,
    Unclassified = 5,
    Residential = 6,
    LivingStreet = 7,
    Service = 8,
    Pedestrian = 9,
    Track = 10,
    Busway = 11,
    Cycleway = 12,
    Footway = 13,
    Path = 14,
    Steps = 15,
    #[default]
    Other = 16,
}

impl RoadClass {
    const ALL: [RoadClass; 17] = [
        RoadClass::Motorway,
        RoadClass::Trunk,
        RoadClass::Primary,
        RoadClass::Secondary,
        RoadClass::Tertiary,
        RoadClass::Unclassified,
        RoadClass::Residential,
        RoadClass::LivingStreet,
        RoadClass::Service,
        RoadClass::Pedestrian,
        RoadClass::Track,
        RoadClass::Busway,
        RoadClass::Cycleway,
        RoadClass::Footway,
        RoadClass::Path,
        RoadClass::Steps,
        RoadClass::Other,
    ];

    pub fn of_highway(value: &str) -> Self {
        match value.strip_suffix("_link").unwrap_or(value) {
            "motorway" => Self::Motorway,
            "trunk" => Self::Trunk,
            "primary" => Self::Primary,
            "secondary" => Self::Secondary,
            "tertiary" => Self::Tertiary,
            "unclassified" => Self::Unclassified,
            "residential" => Self::Residential,
            "living_street" => Self::LivingStreet,
            "service" => Self::Service,
            "pedestrian" => Self::Pedestrian,
            "track" => Self::Track,
            "busway" => Self::Busway,
            "cycleway" => Self::Cycleway,
            "footway" | "bridleway" => Self::Footway,
            "path" => Self::Path,
            "steps" => Self::Steps,
            _ => Self::Other,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Motorway => "motorway",
            Self::Trunk => "trunk",
            Self::Primary => "primary",
            Self::Secondary => "secondary",
            Self::Tertiary => "tertiary",
            Self::Unclassified => "unclassified",
            Self::Residential => "residential",
            Self::LivingStreet => "living_street",
            Self::Service => "service",
            Self::Pedestrian => "pedestrian",
            Self::Track => "track",
            Self::Busway => "busway",
            Self::Cycleway => "cycleway",
            Self::Footway => "footway",
            Self::Path => "path",
            Self::Steps => "steps",
            Self::Other => "other",
        }
    }

    pub const fn to_byte(self) -> u8 {
        self as u8
    }

    pub fn from_byte(byte: u8) -> Option<Self> {
        Self::ALL.get(usize::from(byte)).copied()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PoiRecord {
    pub name: String,
    pub alt_names: Vec<String>,
    pub category: String,
    pub address: Option<AddressComponents>,
    pub geometry: Geometry,
    pub location_precision: LocationPrecision,
    pub source: SourceProvenance,
    pub prominence: Prominence,
    pub wikidata: Option<u32>,
    pub brand: Option<Brand>,
    pub details: Option<Box<PoiDetails>>,
    pub aliases: Vec<(ObjectType, i64)>,
}

/// A chain: its name and the number of its `brand:wikidata` item. The item
/// is the chain's, never the place's, so it gives the place no importance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Brand {
    #[schema(example = "Esso")]
    pub name: String,
    /// Its Wikidata item, such as `Q867662`, a stable id of the chain.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_item",
        deserialize_with = "deserialize_item",
        default
    )]
    #[schema(value_type = Option<String>, example = "Q867662")]
    pub wikidata: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entrance {
    /// Its longitude and latitude in 1e-7 degrees.
    pub at: [i32; 2],
    pub kind: String,
}

impl Entrance {
    pub fn of_tag(kind: Option<&str>, lon: f64, lat: f64) -> Option<Self> {
        let kind = kind
            .map(str::trim)
            .filter(|kind| !kind.is_empty() && !kind.eq_ignore_ascii_case("no"))?;
        Some(Self {
            at: [
                crate::records::quantize(lon).ok()?,
                crate::records::quantize(lat).ok()?,
            ],
            kind: kind.to_string(),
        })
    }
}

/// A point to route to when going to a result.
#[derive(Debug, Clone, PartialEq, Serialize, utoipa::ToSchema)]
pub struct AccessPoint {
    #[schema(example = 7.4263)]
    pub lon: f64,
    #[schema(example = 43.7396)]
    pub lat: f64,
    /// What kind of way in it is, as the value of the OpenStreetMap
    /// `entrance` tag that maps it, such as `main`, `yes`, `service` or
    /// `staircase`.
    #[schema(example = "main")]
    pub kind: String,
}

impl From<&Entrance> for AccessPoint {
    fn from(entrance: &Entrance) -> Self {
        let [lon, lat] = entrance.at.map(crate::records::dequantize);
        Self {
            lon,
            lat,
            kind: entrance.kind.clone(),
        }
    }
}

/// The box around something larger than a point, as its west, south, east
/// and north in 1e-7 degrees. West is greater than east when the box crosses the
/// antimeridian, and only then.
pub type Extent = [i32; 4];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoiDetails {
    pub contact: Option<Contact>,
    pub opening_hours: Option<String>,
    pub extent: Option<Extent>,
    pub access: Vec<Entrance>,
    pub cuisine: Vec<String>,
}

impl PoiDetails {
    pub fn stated(contact: Option<Contact>, opening_hours: Option<String>) -> Option<Box<Self>> {
        Self {
            contact,
            opening_hours,
            ..Self::default()
        }
        .kept()
    }

    pub fn kept(self) -> Option<Box<Self>> {
        (self != Self::default()).then(|| Box::new(self))
    }
}

/// How to reach a place, each value as its source writes it: a phone number
/// in any spelling, a website with or without its scheme. og rewrites none
/// of them and does not check that one still answers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Contact {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    #[schema(example = "+1 416-555-0111")]
    pub phone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    #[schema(example = "https://queenbakery.example/")]
    pub website: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    #[schema(example = "hello@queenbakery.example")]
    pub email: Option<String>,
    #[serde(skip)]
    pub(crate) fax: Option<String>,
}

impl Contact {
    pub fn stated(
        phone: Option<String>,
        website: Option<String>,
        email: Option<String>,
    ) -> Option<Self> {
        Self::with_fax(phone, website, email, None)
    }

    pub(crate) fn with_fax(
        phone: Option<String>,
        website: Option<String>,
        email: Option<String>,
        fax: Option<String>,
    ) -> Option<Self> {
        (phone.is_some() || website.is_some() || email.is_some() || fax.is_some()).then_some(Self {
            phone,
            website,
            email,
            fax,
        })
    }
}

// serde's `serialize_with` hands the field by reference.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn serialize_item<S: Serializer>(item: &Option<u32>, serializer: S) -> Result<S::Ok, S::Error> {
    match item {
        Some(item) => serializer.serialize_str(&format!("Q{item}")),
        None => serializer.serialize_none(),
    }
}

fn deserialize_item<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u32>, D::Error> {
    let text = Option::<String>::deserialize(deserializer)?;
    Ok(text.and_then(|text| text.strip_prefix('Q').and_then(|item| item.parse().ok())))
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Prominence {
    pub importance: u8,
    pub linked: bool,
    pub languages: u8,
}

impl Prominence {
    pub const MAX_LANGUAGES: u8 = 127;

    pub fn to_byte(self) -> u8 {
        self.languages.min(Self::MAX_LANGUAGES) << 1 | u8::from(self.linked)
    }

    pub const fn from_byte(byte: u8, importance: u8) -> Self {
        Self {
            importance,
            linked: byte & 1 != 0,
            languages: byte >> 1,
        }
    }

    pub fn rank(self) -> u64 {
        u64::from(self.importance) << 8
            | u64::from(self.linked) << 7
            | u64::from(self.languages.min(Self::MAX_LANGUAGES))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaceRecord {
    pub name: String,
    /// Every other name the feature is known by, in tag order: each
    /// `name:<lang>` (`name:ja-Hira`, `name:en`), `alt_name`, `official_name`,
    /// `short_name` and `old_name`, and their `:<lang>` forms, with
    /// `;`-separated values split. None repeats `name` or another one,
    /// compared case-insensitively. Search and the parser know the record
    /// by all of them.
    pub alt_names: Vec<String>,
    pub place_type: String,
    pub geometry: Geometry,
    pub source: SourceProvenance,
    pub prominence: Prominence,
    pub wikidata: Option<u32>,
}

/// Kind of result: an address, a street, a POI, or an area such as a
/// locality or region.
#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Address,
    Country,
    District,
    Interpolation,
    Locality,
    Neighbourhood,
    Place,
    Poi,
    Postcode,
    Region,
    Street,
}

impl Layer {
    pub const ALL: [Layer; 11] = [
        Layer::Address,
        Layer::Country,
        Layer::District,
        Layer::Interpolation,
        Layer::Locality,
        Layer::Neighbourhood,
        Layer::Place,
        Layer::Poi,
        Layer::Postcode,
        Layer::Region,
        Layer::Street,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Layer::Address => "address",
            Layer::Country => "country",
            Layer::District => "district",
            Layer::Interpolation => "interpolation",
            Layer::Locality => "locality",
            Layer::Neighbourhood => "neighbourhood",
            Layer::Place => "place",
            Layer::Poi => "poi",
            Layer::Postcode => "postcode",
            Layer::Region => "region",
            Layer::Street => "street",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|layer| layer.as_str() == value)
    }

    pub const fn is_context(self) -> bool {
        matches!(
            self,
            Layer::Country
                | Layer::District
                | Layer::Locality
                | Layer::Neighbourhood
                | Layer::Place
                | Layer::Postcode
                | Layer::Region
        )
    }

    pub const fn place_layer(self) -> Option<PlaceLayer> {
        match self {
            Layer::Country => Some(PlaceLayer::Country),
            Layer::Region => Some(PlaceLayer::Region),
            Layer::District => Some(PlaceLayer::District),
            Layer::Place => Some(PlaceLayer::Place),
            Layer::Locality => Some(PlaceLayer::Locality),
            Layer::Neighbourhood => Some(PlaceLayer::Neighbourhood),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum PlaceLayer {
    Country = 0,
    Region = 1,
    SubRegion = 2,
    District = 3,
    SubDistrict = 4,
    Locality = 5,
    SubLocality = 6,
    Neighbourhood = 7,
    SubNeighbourhood = 8,
    Place = 9,
}

impl PlaceLayer {
    pub const ALL: [PlaceLayer; 10] = [
        PlaceLayer::Country,
        PlaceLayer::Region,
        PlaceLayer::SubRegion,
        PlaceLayer::District,
        PlaceLayer::SubDistrict,
        PlaceLayer::Locality,
        PlaceLayer::SubLocality,
        PlaceLayer::Neighbourhood,
        PlaceLayer::SubNeighbourhood,
        PlaceLayer::Place,
    ];
}

impl PlaceLayer {
    pub const ADMIN: [PlaceLayer; 9] = [
        PlaceLayer::Country,
        PlaceLayer::Region,
        PlaceLayer::SubRegion,
        PlaceLayer::District,
        PlaceLayer::SubDistrict,
        PlaceLayer::Locality,
        PlaceLayer::SubLocality,
        PlaceLayer::Neighbourhood,
        PlaceLayer::SubNeighbourhood,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            PlaceLayer::SubRegion => "subregion",
            PlaceLayer::SubDistrict => "subdistrict",
            PlaceLayer::SubLocality => "sublocality",
            PlaceLayer::SubNeighbourhood => "subneighbourhood",
            other => Layer::from_place(other).as_str(),
        }
    }

    pub const fn record_layer(self) -> Layer {
        Layer::from_place(self)
    }
}

impl Layer {
    pub const fn from_place(layer: PlaceLayer) -> Self {
        match layer {
            PlaceLayer::Country => Layer::Country,
            PlaceLayer::Region | PlaceLayer::SubRegion => Layer::Region,
            PlaceLayer::District | PlaceLayer::SubDistrict => Layer::District,
            PlaceLayer::Place => Layer::Place,
            PlaceLayer::Locality | PlaceLayer::SubLocality => Layer::Locality,
            PlaceLayer::Neighbourhood | PlaceLayer::SubNeighbourhood => Layer::Neighbourhood,
        }
    }
}

impl From<PlaceLayer> for Layer {
    fn from(layer: PlaceLayer) -> Self {
        Layer::from_place(layer)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AddressComponents {
    pub number: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub block_number_len: Option<std::num::NonZeroU16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub street: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locality: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub areas: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub postcode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
}

impl AddressComponents {
    pub fn subject(&self) -> Option<&str> {
        self.street
            .as_deref()
            .or(self.place.as_deref())
            .or_else(|| self.areas.last().map(String::as_str))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InterpolationAddressComponents {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub street: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub postcode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InterpolationRange {
    #[serde(rename = "type")]
    pub kind: String,
    pub start: u32,
    pub end: u32,
    pub step: u32,
}

impl InterpolationRange {
    pub fn number_at(&self, fraction: f64) -> u32 {
        let fraction = fraction.clamp(0.0, 1.0);
        let span = self.end.saturating_sub(self.start);
        if span == 0 || self.step == 0 {
            return self.start;
        }
        let steps = (fraction * span as f64 / self.step as f64).round() as u32;
        (self.start + steps * self.step).min(self.end)
    }

    /// How far along the range `number` lies, `None` when the range does not
    /// contain it: outside `start..=end`, or between its steps (an even
    /// number on an odd range).
    pub fn fraction_of(&self, number: u32) -> Option<f64> {
        if !(self.start..=self.end).contains(&number)
            || (self.step > 0 && !(number - self.start).is_multiple_of(self.step))
        {
            return None;
        }
        let span = self.end - self.start;
        Some(if span == 0 {
            0.0
        } else {
            f64::from(number - self.start) / f64::from(span)
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocationPrecision {
    Point,
    Centroid,
}

pub fn point_geometry(lon: f64, lat: f64) -> Geometry {
    Geometry::new(GeometryValue::Point {
        coordinates: vec![lon, lat].into(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceProvenance {
    pub dataset: String,
    pub object_type: ObjectType,
    pub object_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DerivedSourceProvenance {
    pub dataset: String,
    pub derived_from: String,
    pub record_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RejectedRecord {
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_hint: Option<String>,
    pub source: SourceProvenance,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ObjectType {
    Node,
    Way,
    Relation,
    Row,
}

impl Record {
    pub const fn layer(&self) -> Layer {
        match self {
            Record::Address(_) => Layer::Address,
            Record::Interpolation(_) => Layer::Interpolation,
            Record::Street(_) => Layer::Street,
            Record::Postcode(_) => Layer::Postcode,
            Record::Place(layer, _) => Layer::from_place(*layer),
            Record::Poi(_) => Layer::Poi,
        }
    }

    pub fn id(&self) -> String {
        match self {
            Record::Address(record) => record.id(),
            Record::Interpolation(record) => record.id(),
            Record::Street(record) => record.id(),
            Record::Postcode(record) => record.id(),
            Record::Place(_, record) => record.id(),
            Record::Poi(record) => record.id(),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Record::Address(record) => record.label(),
            Record::Interpolation(record) => record.label(),
            Record::Street(record) => record.label(),
            Record::Postcode(record) => record.label(),
            Record::Place(_, record) => record.label(),
            Record::Poi(record) => record.label(),
        }
    }

    pub fn names(&self) -> Vec<&str> {
        let (name, alt_names) = match self {
            Record::Street(record) => (&record.name, &record.alt_names),
            Record::Place(_, record) => (&record.name, &record.alt_names),
            Record::Poi(record) => (&record.name, &record.alt_names),
            _ => return Vec::new(),
        };
        std::iter::once(name.as_str())
            .chain(alt_names.iter().map(String::as_str))
            .collect()
    }

    pub fn geometry(&self) -> &Geometry {
        match self {
            Record::Address(record) => &record.geometry,
            Record::Interpolation(record) => &record.geometry,
            Record::Street(record) => &record.geometry,
            Record::Postcode(record) => &record.geometry,
            Record::Place(_, record) => &record.geometry,
            Record::Poi(record) => &record.geometry,
        }
    }

    pub fn display_point(&self) -> Option<[f64; 2]> {
        match self {
            Record::Address(record) => point_lon_lat(&record.geometry),
            Record::Interpolation(record) => Some(record.representative_point),
            Record::Street(record) => Some(record.representative_point),
            Record::Postcode(record) => point_lon_lat(&record.geometry),
            Record::Place(_, record) => point_lon_lat(&record.geometry),
            Record::Poi(record) => point_lon_lat(&record.geometry),
        }
    }
}

impl From<AddressRecord> for Record {
    fn from(record: AddressRecord) -> Self {
        Record::Address(record)
    }
}

impl From<InterpolationRecord> for Record {
    fn from(record: InterpolationRecord) -> Self {
        Record::Interpolation(record)
    }
}

impl From<StreetRecord> for Record {
    fn from(record: StreetRecord) -> Self {
        Record::Street(record)
    }
}

impl From<PostcodeRecord> for Record {
    fn from(record: PostcodeRecord) -> Self {
        Record::Postcode(record)
    }
}

impl From<PoiRecord> for Record {
    fn from(record: PoiRecord) -> Self {
        Record::Poi(record)
    }
}

impl Serialize for Record {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Record::Address(record) => record.serialize(serializer),
            Record::Interpolation(record) => record.serialize(serializer),
            Record::Street(record) => record.serialize(serializer),
            Record::Postcode(record) => record.serialize(serializer),
            Record::Place(_, record) => record.serialize(serializer),
            Record::Poi(record) => record.serialize(serializer),
        }
    }
}

impl AddressRecord {
    pub const fn location_precision(&self) -> LocationPrecision {
        self.location_precision
    }

    pub fn id(&self) -> String {
        labels::record_id(
            &self.source.dataset,
            self.source.object_type,
            self.source.object_id,
        )
    }

    pub fn name(&self) -> String {
        labels::address_name(&self.address)
    }

    pub fn label(&self) -> String {
        labels::address_label(&self.address)
    }
}

impl Serialize for AddressRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("AddressRecord", 9)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("label", &self.label())?;
        state.serialize_field("name", &self.name())?;
        state.serialize_field("address", &self.address)?;
        state.serialize_field("geometry", &self.geometry)?;
        state.serialize_field("location_precision", &self.location_precision)?;
        state.serialize_field("source", &self.source)?;
        if self.disputed {
            state.serialize_field("disputed", &true)?;
        } else {
            state.skip_field("disputed")?;
        }
        serialize_access(&mut state, &self.access)?;
        state.end()
    }
}

impl InterpolationRecord {
    pub fn id(&self) -> String {
        labels::interpolation_record_id(
            self.source.object_id,
            self.anchor_node_ids[0],
            self.anchor_node_ids[1],
        )
    }

    pub fn name(&self) -> String {
        labels::interpolation_name(&self.address)
    }

    pub fn label(&self) -> String {
        labels::interpolation_label(&self.name(), &self.interpolation, &self.address)
    }
}

impl Serialize for InterpolationRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let anchor_ids = self
            .anchor_node_ids
            .map(|node_id| labels::osm_record_id(ObjectType::Node, node_id));
        let mut state = serializer.serialize_struct("InterpolationRecord", 9)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("label", &self.label())?;
        state.serialize_field("name", &self.name())?;
        state.serialize_field("address", &self.address)?;
        state.serialize_field("interpolation", &self.interpolation)?;
        state.serialize_field("anchor_ids", &anchor_ids)?;
        state.serialize_field("geometry", &self.geometry)?;
        state.serialize_field("representative_point", &self.representative_point)?;
        state.serialize_field("source", &self.source)?;
        state.end()
    }
}

impl StreetRecord {
    pub fn id(&self) -> String {
        labels::record_id(
            &self.source.dataset,
            self.source.object_type,
            self.source.object_id,
        )
    }

    pub fn label(&self) -> String {
        labels::street_label(&self.name, self.locality.as_deref())
    }
}

impl Serialize for StreetRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("StreetRecord", 10)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("label", &self.label())?;
        state.serialize_field("name", &self.name)?;
        serialize_alt_names(&mut state, &self.alt_names)?;
        state.serialize_field("road_class", &self.road_class)?;
        if let Some(locality) = &self.locality {
            state.serialize_field("locality", locality)?;
        }
        state.serialize_field("geometry", &self.geometry)?;
        state.serialize_field("representative_point", &self.representative_point)?;
        state.serialize_field("source", &self.source)?;
        if self.other_ways.is_empty() {
            state.skip_field("aliases")?;
        } else {
            let aliases = self
                .other_ways
                .iter()
                .map(|way| labels::osm_record_id(ObjectType::Way, *way))
                .collect::<Vec<_>>();
            state.serialize_field("aliases", &aliases)?;
        }
        state.end()
    }
}

impl PostcodeRecord {
    pub fn id(&self) -> String {
        labels::derived_postcode_id(self.country, &self.postcode)
    }

    pub fn name(&self) -> String {
        self.postcode.clone()
    }

    pub fn label(&self) -> String {
        self.postcode.clone()
    }
}

impl Serialize for PostcodeRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("PostcodeRecord", 7)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("label", &self.label())?;
        state.serialize_field("name", &self.name())?;
        state.serialize_field("postcode", &self.postcode)?;
        state.serialize_field("country", &self.country.as_ref().map(CountryCode::as_str))?;
        state.serialize_field("geometry", &self.geometry)?;
        state.serialize_field("source", &self.source)?;
        state.end()
    }
}

impl PlaceRecord {
    pub fn id(&self) -> String {
        labels::place_record_id(
            self.source.object_type,
            self.source.object_id,
            &self.place_type,
        )
    }

    pub fn label(&self) -> String {
        self.name.clone()
    }
}

/// How large a place of `place_type` is, smaller being larger: twice an
/// `admin_level`, and each `place=` value at the rank of the admin unit it
/// usually stands beside. One scale for every country, read from what
/// mappers tagged. None for an area of unknown kind.
pub fn place_rank(place_type: &str) -> Option<u8> {
    if let Some(level) = place_type.strip_prefix("admin_level:") {
        return level
            .parse::<u8>()
            .ok()
            .map(|level| level.saturating_mul(2));
    }
    if place_type.starts_with("derived_country:") {
        return Some(4);
    }
    Some(match place_type.to_ascii_lowercase().as_str() {
        "country" => 4,
        "state" | "province" | "region" => 8,
        "county" | "district" => 12,
        "municipality" => 14,
        "city" => 16,
        "island" => 17,
        "town" | "borough" => 18,
        "village" | "suburb" => 19,
        "hamlet" | "quarter" | "islet" => 20,
        "isolated_dwelling" => 22,
        "farm" => 23,
        "neighbourhood" => 24,
        "locality" => 25,
        _ => return None,
    })
}

impl Serialize for PlaceRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("PlaceRecord", 9)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("label", &self.label())?;
        state.serialize_field("name", &self.name)?;
        serialize_alt_names(&mut state, &self.alt_names)?;
        state.serialize_field("place_type", &self.place_type)?;
        state.serialize_field("geometry", &self.geometry)?;
        state.serialize_field("source", &self.source)?;
        if self.prominence == Prominence::default() {
            state.skip_field("prominence")?;
        } else {
            state.serialize_field("prominence", &self.prominence)?;
        }
        match self.wikidata {
            Some(item) => state.serialize_field("wikidata", &format!("Q{item}"))?,
            None => state.skip_field("wikidata")?,
        }
        state.end()
    }
}

impl PoiRecord {
    pub fn id(&self) -> String {
        if let Some(gers) = self.source.gers() {
            return format!("overture:places:{gers}");
        }
        labels::record_id(
            &self.source.dataset,
            self.source.object_type,
            self.source.object_id,
        )
    }

    pub fn label(&self) -> String {
        labels::poi_label(&self.name, self.address.as_ref(), None)
    }

    pub fn contact(&self) -> Option<&Contact> {
        self.details.as_ref()?.contact.as_ref()
    }

    pub fn opening_hours(&self) -> Option<&str> {
        self.details.as_ref()?.opening_hours.as_deref()
    }

    pub fn extent(&self) -> Option<Extent> {
        self.details.as_ref()?.extent
    }

    pub fn access(&self) -> &[Entrance] {
        self.details
            .as_ref()
            .map_or(&[], |details| details.access.as_slice())
    }

    pub fn cuisine(&self) -> &[String] {
        self.details
            .as_ref()
            .map_or(&[], |details| details.cuisine.as_slice())
    }
}

impl Serialize for PoiRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("PoiRecord", 16)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("label", &self.label())?;
        state.serialize_field("name", &self.name)?;
        serialize_alt_names(&mut state, &self.alt_names)?;
        state.serialize_field("category", &self.category)?;
        if let Some(address) = &self.address {
            state.serialize_field("address", address)?;
        } else {
            state.skip_field("address")?;
        }
        state.serialize_field("geometry", &self.geometry)?;
        state.serialize_field("location_precision", &self.location_precision)?;
        state.serialize_field("source", &self.source)?;
        if self.prominence == Prominence::default() {
            state.skip_field("prominence")?;
        } else {
            state.serialize_field("prominence", &self.prominence)?;
        }
        match self.wikidata {
            Some(item) => state.serialize_field("wikidata", &format!("Q{item}"))?,
            None => state.skip_field("wikidata")?,
        }
        match &self.brand {
            Some(brand) => state.serialize_field("brand", brand)?,
            None => state.skip_field("brand")?,
        }
        match self.contact() {
            Some(contact) => state.serialize_field("contact", contact)?,
            None => state.skip_field("contact")?,
        }
        match self.opening_hours() {
            Some(hours) => state.serialize_field("opening_hours", hours)?,
            None => state.skip_field("opening_hours")?,
        }
        serialize_access(&mut state, self.access())?;
        if self.aliases.is_empty() {
            state.skip_field("aliases")?;
        } else {
            let aliases = self
                .aliases
                .iter()
                .map(|(object_type, object_id)| labels::osm_record_id(*object_type, *object_id))
                .collect::<Vec<_>>();
            state.serialize_field("aliases", &aliases)?;
        }
        state.end()
    }
}

fn serialize_access<S: SerializeStruct>(
    state: &mut S,
    access: &[Entrance],
) -> Result<(), S::Error> {
    if access.is_empty() {
        state.skip_field("access")
    } else {
        let points = access.iter().map(AccessPoint::from).collect::<Vec<_>>();
        state.serialize_field("access", &points)
    }
}

fn serialize_alt_names<S: SerializeStruct>(
    state: &mut S,
    alt_names: &[String],
) -> Result<(), S::Error> {
    if alt_names.is_empty() {
        state.skip_field("alt_names")
    } else {
        state.serialize_field("alt_names", alt_names)
    }
}

pub const OSM_DATASET: &str = "osm";

pub(crate) fn gers_object_id(id: &uuid::Uuid) -> i64 {
    (xxhash_rust::xxh3::xxh3_64(id.as_bytes()) & ((1 << 62) - 1)) as i64
}

impl SourceProvenance {
    pub(crate) fn gers(&self) -> Option<&str> {
        if self.object_type != ObjectType::Row {
            return None;
        }
        self.tags.as_ref()?.get("gers").map(String::as_str)
    }

    pub(crate) fn set_gers(&mut self, gers: String) {
        self.tags = Some(BTreeMap::from([("gers".into(), gers)]));
    }

    pub fn imported(dataset: impl Into<String>, row: i64) -> Self {
        Self {
            dataset: dataset.into(),
            object_type: ObjectType::Row,
            object_id: row,
            tags: None,
        }
    }

    pub fn osm(object_type: ObjectType, object_id: i64) -> Self {
        Self {
            dataset: OSM_DATASET.to_string(),
            object_type,
            object_id,
            tags: None,
        }
    }

    pub fn osm_with_tags(
        object_type: ObjectType,
        object_id: i64,
        tags: BTreeMap<String, String>,
    ) -> Self {
        Self {
            dataset: OSM_DATASET.to_string(),
            object_type,
            object_id,
            tags: Some(tags),
        }
    }
}

pub const DERIVED_FROM_ADDRESS_RECORDS: &str = "accepted_address_records";

impl DerivedSourceProvenance {
    pub fn osm_address_records(record_count: u64) -> Self {
        Self {
            dataset: OSM_DATASET.to_string(),
            derived_from: DERIVED_FROM_ADDRESS_RECORDS.to_string(),
            record_count,
        }
    }
}
