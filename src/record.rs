use std::collections::BTreeMap;

use geojson::{Geometry, GeometryValue};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};

use crate::{labels, parse::lattice::CountryCode, util::geo::point_lon_lat};

/// One normalized geocoding record, as the builder emits it and the Pack
/// stores it.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Address(AddressRecord),
    Interpolation(InterpolationRecord),
    Street(StreetRecord),
    Postcode(PostcodeRecord),
    Place(PlaceLayer, PlaceRecord),
    Poi(PoiRecord),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AddressRecord {
    pub address: AddressComponents,
    /// For an address that states no postcode, the postcodes the other
    /// addresses of its street state: the evidence of where it is. Empty when
    /// it states one, or its street's addresses state none.
    pub street_postcodes: Vec<String>,
    pub geometry: Geometry,
    pub location_precision: LocationPrecision,
    pub source: SourceProvenance,
    /// Whether another source places the same house farther from this point
    /// than sources placing one house ever disagree: the point may be wrong.
    pub disputed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InterpolationRecord {
    pub address: InterpolationAddressComponents,
    pub interpolation: InterpolationRange,
    /// OSM node ids of the low and high numbered anchors.
    pub anchor_node_ids: [i64; 2],
    pub geometry: Geometry,
    pub representative_point: [f64; 2],
    pub source: SourceProvenance,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreetRecord {
    pub name: String,
    /// The other names the feature is known by (see [`PlaceRecord::alt_names`]).
    pub alt_names: Vec<String>,
    pub geometry: Geometry,
    pub representative_point: [f64; 2],
    pub source: SourceProvenance,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PostcodeRecord {
    pub postcode: String,
    /// The country its addresses lie in. A code names a postcode only within
    /// one country: AU and NZ both have a "2110".
    pub country: Option<CountryCode>,
    pub geometry: Geometry,
    pub source: DerivedSourceProvenance,
}

/// A named point of interest. One that also states a valid address carries it,
/// so the same OSM object is found by its name and by its address.
#[derive(Debug, Clone, PartialEq)]
pub struct PoiRecord {
    pub name: String,
    /// The other names the feature is known by (see [`PlaceRecord::alt_names`]).
    pub alt_names: Vec<String>,
    /// The tag that made it a POI, as `key:value`, for example `amenity:cafe`.
    pub category: String,
    pub address: Option<AddressComponents>,
    pub geometry: Geometry,
    pub location_precision: LocationPrecision,
    pub source: SourceProvenance,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaceRecord {
    /// The OSM `name`, the label the record is shown by.
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
}

/// Every record layer. Place layers are the context layers used to describe
/// where an address is.
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

    /// Layers that describe the surroundings of a point (admin areas, named
    /// places and postcodes) rather than a specific address or road.
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

/// A tier of the admin hierarchy, outermost first.
///
/// The tiers between the conventional ones (`SubRegion`, `SubDistrict`,
/// `SubLocality`) exist because a country's hierarchy is deeper than the
/// conventional five. Germany maps nine admin levels, three of them
/// `state_district` and two `city_district`; with one tier each those
/// collapse and all but one boundary of each is dropped from every German
/// address. The names follow the commercial geocoders, which hit the same
/// wall and answered it the same way: HERE and AWS prefix `sub`, TomTom
/// counts `secondary`/`tertiary`, Google numbers `administrative_area_level_N`
/// up to 7 and `sublocality_level_N` up to 5.
///
/// Ordering is the hierarchy, outermost to innermost, and is relied on: a
/// chain is sorted by it. `Place` sorts last because it is no admin tier at
/// all -- an island can be smaller than a locality or larger than a region.
/// The discriminants are the Pack's on-disk encoding of a context tier, so a
/// variant may not be reordered or renumbered without a `CONTEXT_VERSION`.
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
    /// Every admin tier, outermost first. `Place` is left out: it belongs to
    /// place areas, not to the admin hierarchy.
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

    /// The record layer a tier is stored as. The tiers [`Layer`] has no
    /// variant for are stored as the conventional tier above them, which is
    /// what they are a subdivision of.
    pub const fn record_layer(self) -> Layer {
        Layer::from_place(self)
    }
}

impl Layer {
    /// The record layer a place tier is stored and searched as. A `Sub` tier
    /// has no [`Layer`] of its own: it is a subdivision of the tier above and
    /// is searched as one, so a query for a district still matches a
    /// subdistrict. The tier itself is kept on the context entry, which is
    /// what the response reports.
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub street: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locality: Option<String>,
    /// The areas below the locality the address states, outermost first:
    /// `addr:suburb`, `addr:quarter` and `addr:neighbourhood`. An address
    /// of the Japanese block scheme names no street, only these and its
    /// block and house numbers.
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
    /// What the house number is on: the street, else the `addr:place`, else
    /// the innermost area the address states.
    pub fn subject(&self) -> Option<&str> {
        self.street
            .as_deref()
            .or(self.place.as_deref())
            .or(self.areas.last().map(String::as_str))
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

/// A range runs from `start` at the first vertex of its line to `end` at the
/// last, so a house number and its fraction of the line's length estimate
/// each other.
impl InterpolationRange {
    /// The number in the range nearest to `fraction` of the way along it.
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
/// What a record's source object is: an OSM node, way or relation, or a row
/// of an imported address dataset such as OpenAddresses.
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

    /// The names of a named record (street, place, POI): its `name`, then
    /// its alternate names. Empty for the other layers.
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

    /// The point used to place the record on a map: the point itself for point
    /// records, the representative point for lines.
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
        let mut state = serializer.serialize_struct("AddressRecord", 8)?;
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
        self.name.clone()
    }
}

impl Serialize for StreetRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("StreetRecord", 7)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("label", &self.label())?;
        state.serialize_field("name", &self.name)?;
        serialize_alt_names(&mut state, &self.alt_names)?;
        state.serialize_field("geometry", &self.geometry)?;
        state.serialize_field("representative_point", &self.representative_point)?;
        state.serialize_field("source", &self.source)?;
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
/// usually stands beside, as Nominatim's search ranks place them. One scale
/// for every country, read from what mappers tagged. None for an area of
/// unknown kind.
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
        let mut state = serializer.serialize_struct("PlaceRecord", 7)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("label", &self.label())?;
        state.serialize_field("name", &self.name)?;
        serialize_alt_names(&mut state, &self.alt_names)?;
        state.serialize_field("place_type", &self.place_type)?;
        state.serialize_field("geometry", &self.geometry)?;
        state.serialize_field("source", &self.source)?;
        state.end()
    }
}

impl PoiRecord {
    pub fn id(&self) -> String {
        labels::record_id(
            &self.source.dataset,
            self.source.object_type,
            self.source.object_id,
        )
    }

    pub fn label(&self) -> String {
        labels::poi_label(&self.name, self.address.as_ref(), None)
    }
}

impl Serialize for PoiRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("PoiRecord", 9)?;
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
        state.end()
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

/// The dataset id of records built from OpenStreetMap.
pub const OSM_DATASET: &str = "osm";

impl SourceProvenance {
    /// Row `row` of an imported address dataset.
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
