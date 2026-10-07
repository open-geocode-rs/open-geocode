use std::collections::BTreeMap;

use geojson::{Geometry, GeometryValue};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};

use crate::{labels, util::geo::point_lon_lat};

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
    pub geometry: Geometry,
    pub location_precision: LocationPrecision,
    pub source: SourceProvenance,
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
    pub geometry: Geometry,
    pub representative_point: [f64; 2],
    pub source: SourceProvenance,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PostcodeRecord {
    pub postcode: String,
    pub geometry: Geometry,
    pub source: DerivedSourceProvenance,
}

/// A named point of interest. One that also states a valid address carries it,
/// so the same OSM object is found by its name and by its address.
#[derive(Debug, Clone, PartialEq)]
pub struct PoiRecord {
    pub name: String,
    /// The tag that made it a POI, as `key:value`, for example `amenity:cafe`.
    pub category: String,
    pub address: Option<AddressComponents>,
    pub geometry: Geometry,
    pub location_precision: LocationPrecision,
    pub source: SourceProvenance,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaceRecord {
    pub name: String,
    pub place_type: String,
    pub geometry: Geometry,
    pub source: SourceProvenance,
}

/// Every record layer. Place layers are the context layers used to describe
/// where an address is.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlaceLayer {
    Country,
    Region,
    District,
    Place,
    Locality,
    Neighbourhood,
}

impl PlaceLayer {
    pub const fn as_str(self) -> &'static str {
        Layer::from_place(self).as_str()
    }
}

impl Layer {
    pub const fn from_place(layer: PlaceLayer) -> Self {
        match layer {
            PlaceLayer::Country => Layer::Country,
            PlaceLayer::Region => Layer::Region,
            PlaceLayer::District => Layer::District,
            PlaceLayer::Place => Layer::Place,
            PlaceLayer::Locality => Layer::Locality,
            PlaceLayer::Neighbourhood => Layer::Neighbourhood,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub postcode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
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
    pub object_type: OsmObjectType,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum OsmObjectType {
    Node,
    Way,
    Relation,
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
        labels::osm_record_id(self.source.object_type, self.source.object_id)
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
        let mut state = serializer.serialize_struct("AddressRecord", 7)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("label", &self.label())?;
        state.serialize_field("name", &self.name())?;
        state.serialize_field("address", &self.address)?;
        state.serialize_field("geometry", &self.geometry)?;
        state.serialize_field("location_precision", &self.location_precision)?;
        state.serialize_field("source", &self.source)?;
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
            .map(|node_id| labels::osm_record_id(OsmObjectType::Node, node_id));
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
        labels::osm_record_id(self.source.object_type, self.source.object_id)
    }

    pub fn label(&self) -> String {
        self.name.clone()
    }
}

impl Serialize for StreetRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("StreetRecord", 6)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("label", &self.label())?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("geometry", &self.geometry)?;
        state.serialize_field("representative_point", &self.representative_point)?;
        state.serialize_field("source", &self.source)?;
        state.end()
    }
}

impl PostcodeRecord {
    pub fn id(&self) -> String {
        labels::derived_postcode_id(&self.postcode)
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
        let mut state = serializer.serialize_struct("PostcodeRecord", 6)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("label", &self.label())?;
        state.serialize_field("name", &self.name())?;
        state.serialize_field("postcode", &self.postcode)?;
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

impl Serialize for PlaceRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("PlaceRecord", 6)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("label", &self.label())?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("place_type", &self.place_type)?;
        state.serialize_field("geometry", &self.geometry)?;
        state.serialize_field("source", &self.source)?;
        state.end()
    }
}

impl PoiRecord {
    pub fn id(&self) -> String {
        labels::osm_record_id(self.source.object_type, self.source.object_id)
    }

    pub fn label(&self) -> String {
        labels::poi_label(&self.name, self.address.as_ref(), None)
    }
}

impl Serialize for PoiRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("PoiRecord", 8)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("label", &self.label())?;
        state.serialize_field("name", &self.name)?;
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

impl SourceProvenance {
    pub fn osm(object_type: OsmObjectType, object_id: i64) -> Self {
        Self {
            dataset: "osm".to_string(),
            object_type,
            object_id,
            tags: None,
        }
    }

    pub fn osm_with_tags(
        object_type: OsmObjectType,
        object_id: i64,
        tags: BTreeMap<String, String>,
    ) -> Self {
        Self {
            dataset: "osm".to_string(),
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
            dataset: "osm".to_string(),
            derived_from: DERIVED_FROM_ADDRESS_RECORDS.to_string(),
            record_count,
        }
    }
}
