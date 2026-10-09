//! Scratch-file types for the streaming build.
//!
//! The build never holds all features in memory: way features go to a spool,
//! node references and resolved coordinates go through external sorts, and
//! finished records wait in Hilbert-ordered sorters until they are written.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use geojson::{Geometry, GeometryValue};

use crate::{
    extsort::Spill,
    record::{
        AddressComponents, AddressRecord, DerivedSourceProvenance, InterpolationAddressComponents,
        InterpolationRange, InterpolationRecord, Layer, LocationPrecision, OsmObjectType,
        PlaceRecord, PoiRecord, PostcodeRecord, Record, SourceProvenance, StreetRecord,
        point_geometry,
    },
    util::codec::{
        get_i32, get_i64, get_opt_string, get_string, get_tags, get_u8, get_u32, get_u64, put_i64,
        put_opt_str, put_str, put_tags, put_u64,
    },
};

use super::boundary::ContextOrigin;

const COORDINATE_SCALE: f64 = 10_000_000.0;

/// "Way `owner` needs node `node_id` at position `pos`." Sorted by node id so
/// it can be joined against the node section of the PBF, which is sorted too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct NodeRequest {
    pub node_id: i64,
    pub owner: u64,
    pub pos: u32,
    pub want_tags: bool,
}

impl Spill for NodeRequest {
    fn encode(&self, out: &mut Vec<u8>) {
        put_i64(out, self.node_id);
        put_u64(out, self.owner);
        put_u64(out, u64::from(self.pos));
        out.push(u8::from(self.want_tags));
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(Self {
            node_id: get_i64(input)?,
            owner: get_u64(input)?,
            pos: get_u32(input)?,
            want_tags: get_u8(input)? != 0,
        })
    }
}

/// A node coordinate attached to the way position that asked for it. Sorted
/// by owner and position, each way's vertices come back contiguous and in
/// order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ResolvedRef {
    pub owner: u64,
    pub pos: u32,
    pub node_id: i64,
    pub lat_e7: i32,
    pub lon_e7: i32,
    /// Address tags of the node, for interpolation anchors.
    pub tags: Option<BTreeMap<String, String>>,
}

impl ResolvedRef {
    pub(crate) fn lat(&self) -> f64 {
        osm_degrees(self.lat_e7)
    }

    pub(crate) fn lon(&self) -> f64 {
        osm_degrees(self.lon_e7)
    }
}

/// Degrees from a 1e-7 fixed-point PBF coordinate, computed the way `osmpbf`
/// computes `lat()`/`lon()` (via nanodegrees), so a coordinate read through the
/// node join is bit-identical to one read from the node itself.
fn osm_degrees(value: i32) -> f64 {
    1e-9 * (i64::from(value) * 100) as f64
}

impl Spill for ResolvedRef {
    fn encode(&self, out: &mut Vec<u8>) {
        put_u64(out, self.owner);
        put_u64(out, u64::from(self.pos));
        put_i64(out, self.node_id);
        put_i64(out, i64::from(self.lat_e7));
        put_i64(out, i64::from(self.lon_e7));
        match &self.tags {
            Some(tags) => {
                out.push(1);
                put_tags(out, tags);
            }
            None => out.push(0),
        }
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(Self {
            owner: get_u64(input)?,
            pos: get_u32(input)?,
            node_id: get_i64(input)?,
            lat_e7: get_i32(input)?,
            lon_e7: get_i32(input)?,
            tags: match get_u8(input)? {
                0 => None,
                _ => Some(get_tags(input)?),
            },
        })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.tags.as_ref().map_or(0, tags_bytes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WayKind {
    Address,
    Street,
    Interpolation,
    Boundary,
    Poi,
}

/// A way kept by the scan, without its node list: the node references travel
/// separately through the node join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WayFeature {
    pub kind: WayKind,
    pub way_id: i64,
    pub node_count: u32,
    pub tags: BTreeMap<String, String>,
}

impl Spill for WayFeature {
    fn encode(&self, out: &mut Vec<u8>) {
        out.push(match self.kind {
            WayKind::Address => 0,
            WayKind::Street => 1,
            WayKind::Interpolation => 2,
            WayKind::Boundary => 3,
            WayKind::Poi => 4,
        });
        put_i64(out, self.way_id);
        put_u64(out, u64::from(self.node_count));
        put_tags(out, &self.tags);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(Self {
            kind: match get_u8(input)? {
                0 => WayKind::Address,
                1 => WayKind::Street,
                2 => WayKind::Interpolation,
                3 => WayKind::Boundary,
                4 => WayKind::Poi,
                other => bail!("unknown way feature kind {other}"),
            },
            way_id: get_i64(input)?,
            node_count: get_u32(input)?,
            tags: get_tags(input)?,
        })
    }
}

/// A finished record waiting for its final position. `key` is the Hilbert
/// index of its display point; `seq` is emission order, which breaks ties and
/// keeps the build deterministic.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingRecord {
    pub key: u64,
    pub seq: u64,
    pub origin: Option<ContextOrigin>,
    pub record: Record,
}

impl Eq for PendingRecord {}

impl PartialOrd for PendingRecord {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PendingRecord {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.key, self.seq).cmp(&(other.key, other.seq))
    }
}

impl Spill for PendingRecord {
    fn encode(&self, out: &mut Vec<u8>) {
        put_u64(out, self.key);
        put_u64(out, self.seq);
        match &self.origin {
            None => out.push(0),
            Some(ContextOrigin::Boundary(index)) => {
                out.push(1);
                put_u64(out, u64::from(*index));
            }
            Some(ContextOrigin::DerivedCountry(code)) => {
                out.push(2);
                put_str(out, code);
            }
        }
        encode_record(out, &self.record);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        Ok(Self {
            key: get_u64(input)?,
            seq: get_u64(input)?,
            origin: match get_u8(input)? {
                0 => None,
                1 => Some(ContextOrigin::Boundary(get_u32(input)?)),
                2 => Some(ContextOrigin::DerivedCountry(get_string(input)?)),
                other => bail!("unknown record origin {other}"),
            },
            record: decode_record(input)?,
        })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + record_heap_bytes(&self.record)
    }
}

fn tags_bytes(tags: &BTreeMap<String, String>) -> usize {
    tags.iter()
        .map(|(key, value)| key.len() + value.len() + 64)
        .sum()
}

fn record_heap_bytes(record: &Record) -> usize {
    let strings = match record {
        Record::Address(record) => address_heap_bytes(&record.address),
        Record::Poi(record) => {
            record.name.len()
                + record.category.len()
                + record.address.as_ref().map_or(0, address_heap_bytes)
        }
        Record::Interpolation(_) => 96,
        Record::Street(record) => record.name.len(),
        Record::Postcode(record) => record.postcode.len(),
        Record::Place(_, record) => record.name.len() + record.place_type.len(),
    };
    let positions = match &record.geometry().value {
        GeometryValue::LineString { coordinates } => coordinates.len() * 40,
        _ => 40,
    };
    strings + positions + 128
}

fn address_heap_bytes(address: &AddressComponents) -> usize {
    address.number.len()
        + [
            &address.street,
            &address.place,
            &address.unit,
            &address.locality,
            &address.region,
            &address.postcode,
            &address.country,
        ]
        .into_iter()
        .flatten()
        .map(String::len)
        .sum::<usize>()
}

fn encode_record(out: &mut Vec<u8>, record: &Record) {
    out.push(
        Layer::ALL
            .iter()
            .position(|layer| *layer == record.layer())
            .expect("layer has a code") as u8,
    );
    match record {
        Record::Address(record) => {
            encode_address(out, &record.address);
            encode_precision(out, record.location_precision);
            encode_source(out, &record.source);
            encode_geometry(out, &record.geometry);
        }
        Record::Poi(record) => {
            put_str(out, &record.name);
            put_str(out, &record.category);
            match &record.address {
                Some(address) => {
                    out.push(1);
                    encode_address(out, address);
                }
                None => out.push(0),
            }
            encode_precision(out, record.location_precision);
            encode_source(out, &record.source);
            encode_geometry(out, &record.geometry);
        }
        Record::Interpolation(record) => {
            let address = &record.address;
            for value in [
                &address.street,
                &address.place,
                &address.locality,
                &address.region,
                &address.postcode,
                &address.country,
            ] {
                put_opt_str(out, value.as_deref());
            }
            put_str(out, &record.interpolation.kind);
            put_u64(out, u64::from(record.interpolation.start));
            put_u64(out, u64::from(record.interpolation.end));
            put_u64(out, u64::from(record.interpolation.step));
            put_i64(out, record.anchor_node_ids[0]);
            put_i64(out, record.anchor_node_ids[1]);
            encode_point(out, record.representative_point);
            encode_source(out, &record.source);
            encode_geometry(out, &record.geometry);
        }
        Record::Street(record) => {
            put_str(out, &record.name);
            encode_point(out, record.representative_point);
            encode_source(out, &record.source);
            encode_geometry(out, &record.geometry);
        }
        Record::Postcode(record) => {
            put_str(out, &record.postcode);
            put_u64(out, record.source.record_count);
            encode_geometry(out, &record.geometry);
        }
        Record::Place(_, record) => {
            put_str(out, &record.name);
            put_str(out, &record.place_type);
            encode_source(out, &record.source);
            encode_geometry(out, &record.geometry);
        }
    }
}

fn decode_record(input: &mut &[u8]) -> Result<Record> {
    let layer = *Layer::ALL
        .get(usize::from(get_u8(input)?))
        .context("unknown spilled record layer")?;
    Ok(match layer {
        Layer::Address => Record::Address(AddressRecord {
            address: decode_address(input)?,
            location_precision: decode_precision(input)?,
            source: decode_source(input)?,
            geometry: decode_geometry(input)?,
        }),
        Layer::Poi => Record::Poi(PoiRecord {
            name: get_string(input)?,
            category: get_string(input)?,
            address: match get_u8(input)? {
                0 => None,
                _ => Some(decode_address(input)?),
            },
            location_precision: decode_precision(input)?,
            source: decode_source(input)?,
            geometry: decode_geometry(input)?,
        }),
        Layer::Interpolation => {
            let mut fields = [None, None, None, None, None, None];
            for field in &mut fields {
                *field = get_opt_string(input)?;
            }
            let [street, place, locality, region, postcode, country] = fields;
            Record::Interpolation(InterpolationRecord {
                address: InterpolationAddressComponents {
                    street,
                    place,
                    locality,
                    region,
                    postcode,
                    country,
                },
                interpolation: InterpolationRange {
                    kind: get_string(input)?,
                    start: get_u32(input)?,
                    end: get_u32(input)?,
                    step: get_u32(input)?,
                },
                anchor_node_ids: [get_i64(input)?, get_i64(input)?],
                representative_point: decode_point(input)?,
                source: decode_source(input)?,
                geometry: decode_geometry(input)?,
            })
        }
        Layer::Street => Record::Street(StreetRecord {
            name: get_string(input)?,
            representative_point: decode_point(input)?,
            source: decode_source(input)?,
            geometry: decode_geometry(input)?,
        }),
        Layer::Postcode => Record::Postcode(PostcodeRecord {
            postcode: get_string(input)?,
            source: DerivedSourceProvenance::osm_address_records(get_u64(input)?),
            geometry: decode_geometry(input)?,
        }),
        layer => Record::Place(
            layer.place_layer().expect("remaining layers are places"),
            PlaceRecord {
                name: get_string(input)?,
                place_type: get_string(input)?,
                source: decode_source(input)?,
                geometry: decode_geometry(input)?,
            },
        ),
    })
}

fn encode_address(out: &mut Vec<u8>, address: &AddressComponents) {
    put_str(out, &address.number);
    for value in [
        &address.street,
        &address.place,
        &address.unit,
        &address.locality,
        &address.region,
        &address.postcode,
        &address.country,
    ] {
        put_opt_str(out, value.as_deref());
    }
}

fn decode_address(input: &mut &[u8]) -> Result<AddressComponents> {
    let number = get_string(input)?;
    let mut fields = [None, None, None, None, None, None, None];
    for field in &mut fields {
        *field = get_opt_string(input)?;
    }
    let [street, place, unit, locality, region, postcode, country] = fields;
    Ok(AddressComponents {
        number,
        street,
        place,
        unit,
        locality,
        region,
        postcode,
        country,
    })
}

fn encode_precision(out: &mut Vec<u8>, precision: LocationPrecision) {
    out.push(match precision {
        LocationPrecision::Point => 0,
        LocationPrecision::Centroid => 1,
    });
}

fn decode_precision(input: &mut &[u8]) -> Result<LocationPrecision> {
    Ok(match get_u8(input)? {
        0 => LocationPrecision::Point,
        _ => LocationPrecision::Centroid,
    })
}

fn encode_source(out: &mut Vec<u8>, source: &SourceProvenance) {
    out.push(match source.object_type {
        OsmObjectType::Node => 0,
        OsmObjectType::Way => 1,
        OsmObjectType::Relation => 2,
    });
    put_i64(out, source.object_id);
}

fn decode_source(input: &mut &[u8]) -> Result<SourceProvenance> {
    let object_type = match get_u8(input)? {
        0 => OsmObjectType::Node,
        1 => OsmObjectType::Way,
        2 => OsmObjectType::Relation,
        other => bail!("unknown spilled object type {other}"),
    };
    Ok(SourceProvenance::osm(object_type, get_i64(input)?))
}

/// Points keep full precision; they are quantized once, by the record store.
fn encode_point(out: &mut Vec<u8>, point: [f64; 2]) {
    out.extend_from_slice(&point[0].to_bits().to_le_bytes());
    out.extend_from_slice(&point[1].to_bits().to_le_bytes());
}

fn decode_point(input: &mut &[u8]) -> Result<[f64; 2]> {
    let mut point = [0.0; 2];
    for value in &mut point {
        let (bytes, rest) = input
            .split_at_checked(8)
            .context("spilled point is truncated")?;
        *value = f64::from_bits(u64::from_le_bytes(bytes.try_into().expect("8 bytes")));
        *input = rest;
    }
    Ok(point)
}

/// Line vertices are stored as 1e-7 degree deltas, the precision the record
/// store keeps, without its range limit: a spilled line decodes to the values
/// the store would quantize anyway, so an out-of-range vertex fails the build
/// the same way whether or not it was spilled. Non-finite vertices keep their
/// raw bits for the same reason.
fn encode_geometry(out: &mut Vec<u8>, geometry: &Geometry) {
    match &geometry.value {
        GeometryValue::LineString { coordinates } => {
            let finite = coordinates
                .iter()
                .all(|position| position[0].is_finite() && position[1].is_finite());
            if !finite {
                out.push(2);
                put_u64(out, coordinates.len() as u64);
                for position in coordinates {
                    encode_point(out, [position[0], position[1]]);
                }
                return;
            }
            out.push(1);
            put_u64(out, coordinates.len() as u64);
            let mut previous = (0i64, 0i64);
            for position in coordinates {
                let lon = scale(position[0]);
                let lat = scale(position[1]);
                put_i64(out, lon.wrapping_sub(previous.0));
                put_i64(out, lat.wrapping_sub(previous.1));
                previous = (lon, lat);
            }
        }
        GeometryValue::Point { coordinates } => {
            out.push(0);
            encode_point(out, [coordinates[0], coordinates[1]]);
        }
        _ => unreachable!("builder records are points or lines"),
    }
}

fn scale(value: f64) -> i64 {
    (value * COORDINATE_SCALE).round() as i64
}

fn decode_geometry(input: &mut &[u8]) -> Result<Geometry> {
    let line = |coordinates| Ok(Geometry::new(GeometryValue::LineString { coordinates }));
    match get_u8(input)? {
        0 => {
            let [lon, lat] = decode_point(input)?;
            Ok(point_geometry(lon, lat))
        }
        1 => {
            let count = usize::try_from(get_u64(input)?)?;
            let mut coordinates = Vec::with_capacity(count.min(input.len()));
            let mut previous = (0i64, 0i64);
            for _ in 0..count {
                previous.0 = previous.0.wrapping_add(get_i64(input)?);
                previous.1 = previous.1.wrapping_add(get_i64(input)?);
                coordinates.push(
                    vec![
                        previous.0 as f64 / COORDINATE_SCALE,
                        previous.1 as f64 / COORDINATE_SCALE,
                    ]
                    .into(),
                );
            }
            line(coordinates)
        }
        2 => {
            let count = usize::try_from(get_u64(input)?)?;
            let mut coordinates = Vec::with_capacity(count.min(input.len()));
            for _ in 0..count {
                coordinates.push(decode_point(input)?.to_vec().into());
            }
            line(coordinates)
        }
        other => bail!("unknown spilled geometry {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::PlaceLayer;

    fn round_trip<T: Spill + std::fmt::Debug + PartialEq>(value: &T) {
        let mut out = Vec::new();
        value.encode(&mut out);
        let mut input = out.as_slice();
        assert_eq!(&T::decode(&mut input).expect("decode"), value);
        assert!(input.is_empty());
    }

    #[test]
    fn round_trips_scratch_types() {
        round_trip(&NodeRequest {
            node_id: -5,
            owner: 7,
            pos: 3,
            want_tags: true,
        });
        round_trip(&ResolvedRef {
            owner: 1,
            pos: 2,
            node_id: 3,
            lat_e7: 436_532_000,
            lon_e7: -793_832_000,
            tags: Some(BTreeMap::from([("addr:housenumber".into(), "10".into())])),
        });
        round_trip(&WayFeature {
            kind: WayKind::Interpolation,
            way_id: 9,
            node_count: 4,
            tags: BTreeMap::from([("addr:interpolation".into(), "odd".into())]),
        });
    }

    #[test]
    fn out_of_range_line_coordinates_are_not_spilled_as_zero() {
        let record = Record::Street(StreetRecord {
            name: "Edge Road".into(),
            geometry: Geometry::new(GeometryValue::LineString {
                coordinates: vec![vec![-79.0, 43.0].into(), vec![300.0, 43.1].into()],
            }),
            representative_point: [-79.0, 43.05],
            source: SourceProvenance::osm(OsmObjectType::Way, 13),
        });
        let mut out = Vec::new();
        PendingRecord {
            key: 0,
            seq: 0,
            origin: None,
            record: record.clone(),
        }
        .encode(&mut out);
        let decoded = PendingRecord::decode(&mut out.as_slice()).expect("decode");
        assert_eq!(
            decoded.record, record,
            "spilling must not change coordinates"
        );

        let mut infinite = record;
        if let Record::Street(street) = &mut infinite {
            street.geometry = Geometry::new(GeometryValue::LineString {
                coordinates: vec![vec![-79.0, 43.0].into(), vec![f64::INFINITY, 43.1].into()],
            });
        }
        let mut out = Vec::new();
        PendingRecord {
            key: 0,
            seq: 0,
            origin: None,
            record: infinite.clone(),
        }
        .encode(&mut out);
        let decoded = PendingRecord::decode(&mut out.as_slice()).expect("decode");
        assert_eq!(decoded.record, infinite);
    }

    #[test]
    fn round_trips_pending_records() {
        let line = Geometry::new(GeometryValue::LineString {
            coordinates: vec![vec![-79.0, 43.0].into(), vec![-79.0000001, 43.1].into()],
        });
        let records = [
            Record::Address(AddressRecord {
                address: AddressComponents {
                    number: "10".into(),
                    street: Some("King Street".into()),
                    place: None,
                    unit: Some("4".into()),
                    locality: Some("Toronto".into()),
                    region: None,
                    postcode: Some("M5V".into()),
                    country: None,
                },
                geometry: point_geometry(-79.38321234567, 43.653),
                location_precision: LocationPrecision::Centroid,
                source: SourceProvenance::osm(OsmObjectType::Way, 12),
            }),
            Record::Street(StreetRecord {
                name: "King Street".into(),
                geometry: line.clone(),
                representative_point: [-79.0, 43.05],
                source: SourceProvenance::osm(OsmObjectType::Way, 13),
            }),
            Record::Interpolation(InterpolationRecord {
                address: InterpolationAddressComponents {
                    street: Some("King Street".into()),
                    place: None,
                    locality: None,
                    region: None,
                    postcode: None,
                    country: Some("CA".into()),
                },
                interpolation: InterpolationRange {
                    kind: "even".into(),
                    start: 2,
                    end: 10,
                    step: 2,
                },
                anchor_node_ids: [1, 2],
                geometry: line,
                representative_point: [-79.0, 43.05],
                source: SourceProvenance::osm(OsmObjectType::Way, 14),
            }),
            Record::Postcode(PostcodeRecord {
                postcode: "M5V".into(),
                geometry: point_geometry(-79.0, 43.0),
                source: DerivedSourceProvenance::osm_address_records(3),
            }),
            Record::Place(
                PlaceLayer::Neighbourhood,
                PlaceRecord {
                    name: "Annex".into(),
                    place_type: "suburb".into(),
                    geometry: point_geometry(-79.4, 43.67),
                    source: SourceProvenance::osm(OsmObjectType::Node, 15),
                },
            ),
            Record::Poi(PoiRecord {
                name: "Tim Hortons".into(),
                category: "amenity:cafe".into(),
                address: Some(AddressComponents {
                    number: "123".into(),
                    street: Some("King Street West".into()),
                    place: None,
                    unit: None,
                    locality: None,
                    region: None,
                    postcode: Some("M5V 1A1".into()),
                    country: None,
                }),
                geometry: point_geometry(-79.38, 43.65),
                location_precision: LocationPrecision::Centroid,
                source: SourceProvenance::osm(OsmObjectType::Way, 16),
            }),
            Record::Poi(PoiRecord {
                name: "Riverdale Farm".into(),
                category: "tourism:attraction".into(),
                address: None,
                geometry: point_geometry(-79.36, 43.67),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(OsmObjectType::Node, 17),
            }),
        ];
        for (seq, record) in records.into_iter().enumerate() {
            round_trip(&PendingRecord {
                key: 99,
                seq: seq as u64,
                origin: match seq {
                    0 => None,
                    1 => Some(ContextOrigin::Boundary(4)),
                    _ => Some(ContextOrigin::DerivedCountry("CA".into())),
                },
                record,
            });
        }
    }
}
