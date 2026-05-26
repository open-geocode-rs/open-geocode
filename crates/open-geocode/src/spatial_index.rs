use std::{
    cmp::Ordering,
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};

use anyhow::{Context, Result, bail};
use geojson::{Geometry, GeometryValue};
use rstar::{AABB, PointDistance, RTree, RTreeObject};
use serde::{Deserialize, Serialize};

use crate::{
    pack::RecordId,
    record::{NormalizedRecord, PlaceRecord},
};

pub const SPATIAL_INDEX_RELATIVE_PATH: &str = "spatial/reverse.rmp";
pub const SPATIAL_INDEX_SCHEMA_VERSION: u32 = 1;

const SPATIAL_INDEX_MAGIC: &[u8; 8] = b"OGSPT001";
const EARTH_RADIUS_M: f64 = 6_371_008.8;

#[derive(Debug, Default)]
pub struct PackSpatialIndexWriter {
    points: Vec<SpatialPointEntry>,
    segments: Vec<SpatialSegmentEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpatialIndexCommit {
    pub schema_version: u32,
    pub point_count: u64,
    pub segment_count: u64,
}

#[derive(Debug)]
pub struct PackSpatialIndexReader {
    index: SpatialIndexFile,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpatialLayer {
    Address,
    Country,
    District,
    Interpolation,
    Locality,
    Neighbourhood,
    Place,
    Postcode,
    Region,
    Street,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PointCandidate {
    pub record_id: RecordId,
    pub layer: SpatialLayer,
    pub lon: f64,
    pub lat: f64,
    pub distance_m: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SegmentCandidate {
    pub record_id: RecordId,
    pub layer: SpatialLayer,
    pub closest_lon: f64,
    pub closest_lat: f64,
    pub distance_m: f64,
    pub fraction: f64,
}

#[derive(Debug, Serialize, Deserialize)]
struct SpatialIndexFile {
    schema_version: u32,
    points: RTree<SpatialPointEntry>,
    segments: RTree<SpatialSegmentEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SpatialPointEntry {
    record_id: RecordId,
    layer: SpatialLayer,
    lon: f64,
    lat: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SpatialSegmentEntry {
    record_id: RecordId,
    layer: SpatialLayer,
    start_lon: f64,
    start_lat: f64,
    end_lon: f64,
    end_lat: f64,
    start_fraction: f64,
    end_fraction: f64,
}

impl PackSpatialIndexWriter {
    pub fn add_record(&mut self, record_id: RecordId, record: &NormalizedRecord) -> Result<()> {
        match record {
            NormalizedRecord::Address(record) => {
                if let Some((lon, lat)) = point_coordinates(&record.geometry) {
                    self.points.push(SpatialPointEntry {
                        record_id,
                        layer: SpatialLayer::Address,
                        lon,
                        lat,
                    });
                }
            }
            NormalizedRecord::Interpolation(record) => {
                self.add_line_segments(record_id, SpatialLayer::Interpolation, &record.geometry);
            }
            NormalizedRecord::Street(record) => {
                self.add_line_segments(record_id, SpatialLayer::Street, &record.geometry);
            }
            NormalizedRecord::Postcode(record) => {
                if let Some((lon, lat)) = point_coordinates(&record.geometry) {
                    self.points.push(SpatialPointEntry {
                        record_id,
                        layer: SpatialLayer::Postcode,
                        lon,
                        lat,
                    });
                }
            }
            NormalizedRecord::Country(record) => {
                self.add_place_point(record_id, SpatialLayer::Country, record);
            }
            NormalizedRecord::District(record) => {
                self.add_place_point(record_id, SpatialLayer::District, record);
            }
            NormalizedRecord::Locality(record) => {
                self.add_place_point(record_id, SpatialLayer::Locality, record);
            }
            NormalizedRecord::Neighbourhood(record) => {
                self.add_place_point(record_id, SpatialLayer::Neighbourhood, record);
            }
            NormalizedRecord::Place(record) => {
                self.add_place_point(record_id, SpatialLayer::Place, record);
            }
            NormalizedRecord::Region(record) => {
                self.add_place_point(record_id, SpatialLayer::Region, record);
            }
        }

        Ok(())
    }

    pub fn finish(self, pack_path: &Path) -> Result<SpatialIndexCommit> {
        let point_count = self.points.len() as u64;
        let segment_count = self.segments.len() as u64;
        let index = SpatialIndexFile {
            schema_version: SPATIAL_INDEX_SCHEMA_VERSION,
            points: RTree::bulk_load(self.points),
            segments: RTree::bulk_load(self.segments),
        };
        let bytes = rmp_serde::to_vec_named(&index).context("failed to encode spatial index")?;
        let path = pack_path.join(SPATIAL_INDEX_RELATIVE_PATH);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut file =
            File::create(&path).with_context(|| format!("failed to create {}", path.display()))?;
        file.write_all(SPATIAL_INDEX_MAGIC)?;
        file.write_all(&bytes)?;
        file.flush()?;

        Ok(SpatialIndexCommit {
            schema_version: SPATIAL_INDEX_SCHEMA_VERSION,
            point_count,
            segment_count,
        })
    }

    fn add_place_point(&mut self, record_id: RecordId, layer: SpatialLayer, record: &PlaceRecord) {
        if let Some((lon, lat)) = point_coordinates(&record.geometry) {
            self.points.push(SpatialPointEntry {
                record_id,
                layer,
                lon,
                lat,
            });
        }
    }

    fn add_line_segments(&mut self, record_id: RecordId, layer: SpatialLayer, geometry: &Geometry) {
        self.segments
            .extend(
                line_segments(geometry)
                    .into_iter()
                    .map(|segment| SpatialSegmentEntry {
                        record_id,
                        layer,
                        start_lon: segment.start[0],
                        start_lat: segment.start[1],
                        end_lon: segment.end[0],
                        end_lat: segment.end[1],
                        start_fraction: segment.start_fraction,
                        end_fraction: segment.end_fraction,
                    }),
            );
    }
}

impl PackSpatialIndexReader {
    pub fn open(pack_path: impl AsRef<Path>) -> Result<Self> {
        let path = pack_path.as_ref().join(SPATIAL_INDEX_RELATIVE_PATH);
        let mut file =
            File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
        let mut magic = [0; 8];
        file.read_exact(&mut magic)?;
        if &magic != SPATIAL_INDEX_MAGIC {
            bail!("{} has an invalid magic header", path.display());
        }

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let index: SpatialIndexFile =
            rmp_serde::from_slice(&bytes).context("failed to decode spatial index")?;
        if index.schema_version != SPATIAL_INDEX_SCHEMA_VERSION {
            bail!(
                "unsupported spatial index schema version {}; expected {}",
                index.schema_version,
                SPATIAL_INDEX_SCHEMA_VERSION
            );
        }

        Ok(Self { index })
    }

    pub fn point_candidates(
        &self,
        lon: f64,
        lat: f64,
        layer: SpatialLayer,
        radius_m: f64,
        limit: usize,
    ) -> Vec<PointCandidate> {
        let envelope = search_envelope(lon, lat, radius_m);
        let mut candidates = self
            .index
            .points
            .locate_in_envelope_intersecting(envelope)
            .filter(|entry| entry.layer == layer)
            .map(|entry| PointCandidate {
                record_id: entry.record_id,
                layer: entry.layer,
                lon: entry.lon,
                lat: entry.lat,
                distance_m: haversine_m(lon, lat, entry.lon, entry.lat),
            })
            .filter(|candidate| candidate.distance_m <= radius_m)
            .collect::<Vec<_>>();
        candidates.sort_by(compare_distance);
        truncate(candidates, limit)
    }

    pub fn context_candidates(
        &self,
        lon: f64,
        lat: f64,
        radius_m: f64,
        limit: usize,
    ) -> Vec<PointCandidate> {
        let envelope = search_envelope(lon, lat, radius_m);
        let mut candidates = self
            .index
            .points
            .locate_in_envelope_intersecting(envelope)
            .filter(|entry| is_context_layer(entry.layer))
            .map(|entry| PointCandidate {
                record_id: entry.record_id,
                layer: entry.layer,
                lon: entry.lon,
                lat: entry.lat,
                distance_m: haversine_m(lon, lat, entry.lon, entry.lat),
            })
            .filter(|candidate| candidate.distance_m <= radius_m)
            .collect::<Vec<_>>();
        candidates.sort_by(compare_distance);
        truncate(candidates, limit)
    }

    pub fn segment_candidates(
        &self,
        lon: f64,
        lat: f64,
        layer: SpatialLayer,
        radius_m: f64,
        limit: usize,
    ) -> Vec<SegmentCandidate> {
        let envelope = search_envelope(lon, lat, radius_m);
        let mut candidates = self
            .index
            .segments
            .locate_in_envelope_intersecting(envelope)
            .filter(|entry| entry.layer == layer)
            .map(|entry| {
                let projection = project_to_segment_m(
                    lon,
                    lat,
                    entry.start_lon,
                    entry.start_lat,
                    entry.end_lon,
                    entry.end_lat,
                );
                SegmentCandidate {
                    record_id: entry.record_id,
                    layer: entry.layer,
                    closest_lon: projection.lon,
                    closest_lat: projection.lat,
                    distance_m: projection.distance_m,
                    fraction: entry.start_fraction
                        + projection.t * (entry.end_fraction - entry.start_fraction),
                }
            })
            .filter(|candidate| candidate.distance_m <= radius_m)
            .collect::<Vec<_>>();
        candidates.sort_by(compare_distance);
        truncate(candidates, limit)
    }
}

impl RTreeObject for SpatialPointEntry {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        AABB::from_point([self.lon, self.lat])
    }
}

impl PointDistance for SpatialPointEntry {
    fn distance_2(&self, point: &[f64; 2]) -> f64 {
        squared_degrees_distance(self.lon, self.lat, point[0], point[1])
    }
}

impl RTreeObject for SpatialSegmentEntry {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners(
            [
                self.start_lon.min(self.end_lon),
                self.start_lat.min(self.end_lat),
            ],
            [
                self.start_lon.max(self.end_lon),
                self.start_lat.max(self.end_lat),
            ],
        )
    }
}

impl PointDistance for SpatialSegmentEntry {
    fn distance_2(&self, point: &[f64; 2]) -> f64 {
        squared_point_segment_degrees_distance(
            point[0],
            point[1],
            self.start_lon,
            self.start_lat,
            self.end_lon,
            self.end_lat,
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct BuiltSegment {
    start: [f64; 2],
    end: [f64; 2],
    start_fraction: f64,
    end_fraction: f64,
}

#[derive(Debug, Clone, Copy)]
struct SegmentProjection {
    lon: f64,
    lat: f64,
    distance_m: f64,
    t: f64,
}

fn point_coordinates(geometry: &Geometry) -> Option<(f64, f64)> {
    let GeometryValue::Point { coordinates } = &geometry.value else {
        return None;
    };
    let [lon, lat, ..] = coordinates.as_slice() else {
        return None;
    };
    if lon.is_finite() && lat.is_finite() {
        Some((*lon, *lat))
    } else {
        None
    }
}

fn line_segments(geometry: &Geometry) -> Vec<BuiltSegment> {
    let GeometryValue::LineString { coordinates } = &geometry.value else {
        return Vec::new();
    };
    let positions = coordinates
        .iter()
        .filter_map(|position| {
            let [lon, lat, ..] = position.as_slice() else {
                return None;
            };
            if lon.is_finite() && lat.is_finite() {
                Some([*lon, *lat])
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    if positions.len() < 2 {
        return Vec::new();
    }

    let mut lengths = Vec::with_capacity(positions.len() - 1);
    let mut total = 0.0;
    for pair in positions.windows(2) {
        let length = haversine_m(pair[0][0], pair[0][1], pair[1][0], pair[1][1]);
        lengths.push(length);
        total += length;
    }
    if total <= f64::EPSILON {
        return Vec::new();
    }

    let mut traversed = 0.0;
    let mut segments = Vec::with_capacity(lengths.len());
    for (index, pair) in positions.windows(2).enumerate() {
        let length = lengths[index];
        if length <= f64::EPSILON {
            continue;
        }
        let start_fraction = traversed / total;
        traversed += length;
        segments.push(BuiltSegment {
            start: pair[0],
            end: pair[1],
            start_fraction,
            end_fraction: traversed / total,
        });
    }
    segments
}

fn is_context_layer(layer: SpatialLayer) -> bool {
    matches!(
        layer,
        SpatialLayer::Country
            | SpatialLayer::District
            | SpatialLayer::Locality
            | SpatialLayer::Neighbourhood
            | SpatialLayer::Place
            | SpatialLayer::Postcode
            | SpatialLayer::Region
    )
}

fn search_envelope(lon: f64, lat: f64, radius_m: f64) -> AABB<[f64; 2]> {
    let lat_delta = radius_m / 111_320.0;
    let lon_scale = lat.to_radians().cos().abs().max(0.01);
    let lon_delta = lat_delta / lon_scale;
    AABB::from_corners(
        [lon - lon_delta, lat - lat_delta],
        [lon + lon_delta, lat + lat_delta],
    )
}

fn project_to_segment_m(
    lon: f64,
    lat: f64,
    start_lon: f64,
    start_lat: f64,
    end_lon: f64,
    end_lat: f64,
) -> SegmentProjection {
    let (sx, sy) = local_xy_m(start_lon, start_lat, lon, lat);
    let (ex, ey) = local_xy_m(end_lon, end_lat, lon, lat);
    let vx = ex - sx;
    let vy = ey - sy;
    let length_2 = vx * vx + vy * vy;
    let t = if length_2 <= f64::EPSILON {
        0.0
    } else {
        (-(sx * vx + sy * vy) / length_2).clamp(0.0, 1.0)
    };
    let x = sx + t * vx;
    let y = sy + t * vy;
    SegmentProjection {
        lon: start_lon + t * (end_lon - start_lon),
        lat: start_lat + t * (end_lat - start_lat),
        distance_m: (x * x + y * y).sqrt(),
        t,
    }
}

fn local_xy_m(lon: f64, lat: f64, origin_lon: f64, origin_lat: f64) -> (f64, f64) {
    let x = (lon - origin_lon).to_radians() * EARTH_RADIUS_M * origin_lat.to_radians().cos();
    let y = (lat - origin_lat).to_radians() * EARTH_RADIUS_M;
    (x, y)
}

fn haversine_m(a_lon: f64, a_lat: f64, b_lon: f64, b_lat: f64) -> f64 {
    let d_lat = (b_lat - a_lat).to_radians();
    let d_lon = (b_lon - a_lon).to_radians();
    let a_lat = a_lat.to_radians();
    let b_lat = b_lat.to_radians();
    let sin_d_lat = (d_lat / 2.0).sin();
    let sin_d_lon = (d_lon / 2.0).sin();
    let h = sin_d_lat * sin_d_lat + a_lat.cos() * b_lat.cos() * sin_d_lon * sin_d_lon;
    2.0 * EARTH_RADIUS_M * h.sqrt().asin()
}

fn squared_degrees_distance(a_lon: f64, a_lat: f64, b_lon: f64, b_lat: f64) -> f64 {
    let d_lon = a_lon - b_lon;
    let d_lat = a_lat - b_lat;
    d_lon * d_lon + d_lat * d_lat
}

fn squared_point_segment_degrees_distance(
    lon: f64,
    lat: f64,
    start_lon: f64,
    start_lat: f64,
    end_lon: f64,
    end_lat: f64,
) -> f64 {
    let vx = end_lon - start_lon;
    let vy = end_lat - start_lat;
    let length_2 = vx * vx + vy * vy;
    let t = if length_2 <= f64::EPSILON {
        0.0
    } else {
        (((lon - start_lon) * vx + (lat - start_lat) * vy) / length_2).clamp(0.0, 1.0)
    };
    let closest_lon = start_lon + t * vx;
    let closest_lat = start_lat + t * vy;
    squared_degrees_distance(lon, lat, closest_lon, closest_lat)
}

fn compare_distance<T>(left: &T, right: &T) -> Ordering
where
    T: CandidateDistance,
{
    left.distance_m()
        .partial_cmp(&right.distance_m())
        .unwrap_or(Ordering::Equal)
}

trait CandidateDistance {
    fn distance_m(&self) -> f64;
}

impl CandidateDistance for PointCandidate {
    fn distance_m(&self) -> f64 {
        self.distance_m
    }
}

impl CandidateDistance for SegmentCandidate {
    fn distance_m(&self) -> f64 {
        self.distance_m
    }
}

fn truncate<T>(mut candidates: Vec<T>, limit: usize) -> Vec<T> {
    if limit > 0 && candidates.len() > limit {
        candidates.truncate(limit);
    }
    candidates
}

#[cfg(test)]
mod tests {
    use geojson::GeometryValue;

    use crate::record::{
        AddressComponents, AddressRecord, LocationPrecision, OsmObjectType, SourceProvenance,
        StreetRecord, point_geometry,
    };

    use super::*;

    #[test]
    fn indexes_and_queries_address_points() {
        let mut writer = PackSpatialIndexWriter::default();
        let record = NormalizedRecord::address(AddressRecord {
            id: "osm:node:1".to_string(),
            label: "10 King Street".to_string(),
            name: "10 King Street".to_string(),
            address: AddressComponents {
                number: "10".to_string(),
                street: Some("King Street".to_string()),
                place: None,
                unit: None,
                locality: None,
                region: None,
                postcode: None,
                country: None,
            },
            geometry: point_geometry(-79.0, 43.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(OsmObjectType::Node, 1),
        });

        writer.add_record(7, &record).expect("add record");
        let temp_dir =
            std::env::temp_dir().join(format!("open-geocode-spatial-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        writer.finish(&temp_dir).expect("finish");

        let reader = PackSpatialIndexReader::open(&temp_dir).expect("reader");
        let hits = reader.point_candidates(-79.0, 43.0, SpatialLayer::Address, 5.0, 1);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, 7);

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn indexes_line_segments_with_fraction() {
        let mut writer = PackSpatialIndexWriter::default();
        let record = NormalizedRecord::street(StreetRecord {
            id: "osm:way:1".to_string(),
            label: "King Street".to_string(),
            name: "King Street".to_string(),
            geometry: Geometry::new(GeometryValue::LineString {
                coordinates: vec![vec![-79.0, 43.0].into(), vec![-79.0, 43.001].into()],
            }),
            representative_point: [-79.0, 43.0005],
            source: SourceProvenance::osm(OsmObjectType::Way, 1),
        });

        writer.add_record(9, &record).expect("add record");
        let temp_dir = std::env::temp_dir().join(format!(
            "open-geocode-spatial-line-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp_dir);
        writer.finish(&temp_dir).expect("finish");

        let reader = PackSpatialIndexReader::open(&temp_dir).expect("reader");
        let hits = reader.segment_candidates(-79.00001, 43.0005, SpatialLayer::Street, 5.0, 1);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, 9);
        assert!((hits[0].fraction - 0.5).abs() < 0.01);

        let _ = fs::remove_dir_all(temp_dir);
    }
}
