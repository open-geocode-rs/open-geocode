use std::collections::BTreeMap;

use geojson::{Geometry, GeometryValue};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};

use crate::labels;

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
    pub anchor_ids: Vec<String>,
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

#[derive(Debug, Clone, PartialEq)]
pub struct PlaceRecord {
    pub name: String,
    pub place_type: String,
    pub geometry: Geometry,
    pub source: SourceProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceLayer {
    Country,
    Region,
    District,
    Place,
    Locality,
    Neighbourhood,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OsmObjectType {
    Node,
    Way,
    Relation,
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
        let (low, high) = self.anchor_node_ids().unwrap_or((0, 0));
        labels::interpolation_record_id(self.source.object_id, low, high)
    }

    pub fn name(&self) -> String {
        labels::interpolation_name(&self.address)
    }

    pub fn label(&self) -> String {
        labels::interpolation_label(&self.name(), &self.interpolation, &self.address)
    }

    fn anchor_node_ids(&self) -> Option<(i64, i64)> {
        let low = self
            .anchor_ids
            .first()?
            .strip_prefix("osm:node:")?
            .parse::<i64>()
            .ok()?;
        let high = self
            .anchor_ids
            .get(1)?
            .strip_prefix("osm:node:")?
            .parse::<i64>()
            .ok()?;
        Some((low, high))
    }
}

impl Serialize for InterpolationRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("InterpolationRecord", 9)?;
        state.serialize_field("id", &self.id())?;
        state.serialize_field("label", &self.label())?;
        state.serialize_field("name", &self.name())?;
        state.serialize_field("address", &self.address)?;
        state.serialize_field("interpolation", &self.interpolation)?;
        state.serialize_field("anchor_ids", &self.anchor_ids)?;
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
        if let Some(code) = self.place_type.strip_prefix("derived_country:") {
            return labels::derived_country_id(code);
        }
        labels::osm_record_id(self.source.object_type, self.source.object_id)
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

impl DerivedSourceProvenance {
    pub fn osm_address_records(record_count: u64) -> Self {
        Self {
            dataset: "osm".to_string(),
            derived_from: "accepted_address_records".to_string(),
            record_count,
        }
    }
}
