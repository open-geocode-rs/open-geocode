use std::collections::BTreeMap;

use geojson::{Geometry, GeometryValue};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "layer", rename_all = "snake_case")]
pub enum NormalizedRecord {
    Address(AddressRecord),
    Interpolation(InterpolationRecord),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AddressRecord {
    pub id: String,
    pub label: String,
    pub name: String,
    pub address: AddressComponents,
    pub geometry: Geometry,
    pub location_precision: LocationPrecision,
    pub source: SourceProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InterpolationRecord {
    pub id: String,
    pub label: String,
    pub name: String,
    pub address: InterpolationAddressComponents,
    pub interpolation: InterpolationRange,
    pub anchor_ids: Vec<String>,
    pub geometry: Geometry,
    pub representative_point: [f64; 2],
    pub source: SourceProvenance,
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

impl NormalizedRecord {
    pub fn address(record: AddressRecord) -> Self {
        Self::Address(record)
    }

    pub fn interpolation(record: InterpolationRecord) -> Self {
        Self::Interpolation(record)
    }

    pub const fn layer(&self) -> &'static str {
        match self {
            Self::Address(_) => "address",
            Self::Interpolation(_) => "interpolation",
        }
    }
}

impl AddressRecord {
    pub const fn location_precision(&self) -> LocationPrecision {
        self.location_precision
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
