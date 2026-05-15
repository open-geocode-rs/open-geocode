use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AddressRecord {
    pub schema_version: u32,
    pub record_id: String,
    pub kind: RecordKind,
    pub label: String,
    pub house_number: String,
    pub street: Option<String>,
    pub place: Option<String>,
    pub unit: Option<String>,
    pub city: Option<String>,
    pub postcode: Option<String>,
    pub state: Option<String>,
    pub country: Option<String>,
    pub lat: f64,
    pub lon: f64,
    pub location_precision: LocationPrecision,
    pub source: SourceProvenance,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    Address,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocationPrecision {
    Point,
    Centroid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceProvenance {
    pub dataset: String,
    pub object_type: OsmObjectType,
    pub object_id: i64,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OsmObjectType {
    Node,
    Way,
    Relation,
}
