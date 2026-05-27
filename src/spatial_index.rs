use std::{
    cmp::Ordering,
    collections::BTreeSet,
    fmt,
    fs::{self, File},
    io::{BufWriter, Write},
    mem,
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, bail};
use bytemuck::{Pod, Zeroable};
use geojson::{Geometry, GeometryValue};
use h3o::{CellIndex, LatLng, Resolution};
use memmap2::{Mmap, MmapOptions};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    pack::RecordId,
    record::{NormalizedRecord, PlaceRecord},
};

#[cfg(not(target_endian = "little"))]
compile_error!("open-geocode spatial pack files currently require little-endian targets");

pub const SPATIAL_INDEX_V2_RELATIVE_DIR: &str = "spatial/v2";
pub const SPATIAL_INDEX_SCHEMA_VERSION: u32 = 2;

const SPATIAL_INDEX_V2_MANIFEST: &str = "manifest.json";

const V2_CELLS_FILE: &str = "cells.bin";
const V2_POINTS_FILE: &str = "points.bin";
const V2_SEGMENTS_FILE: &str = "segments.bin";
const V2_CELL_POINTS_FILE: &str = "cell_points.bin";
const V2_CELL_SEGMENTS_FILE: &str = "cell_segments.bin";
const V2_CONTEXT_CELLS_FILE: &str = "context_cells.bin";
const V2_CONTEXT_CELL_POINTS_FILE: &str = "context_cell_points.bin";

const V2_CELLS_MAGIC: &[u8; 8] = b"OGC2CELL";
const V2_POINTS_MAGIC: &[u8; 8] = b"OGC2PNTS";
const V2_SEGMENTS_MAGIC: &[u8; 8] = b"OGC2SEGS";
const V2_POINT_REFS_MAGIC: &[u8; 8] = b"OGC2PREF";
const V2_SEGMENT_REFS_MAGIC: &[u8; 8] = b"OGC2SREF";

const COUNTED_HEADER_BYTES: usize = 16;
const CELL_ENTRY_BYTES: usize = 40;
const POINT_ENTRY_BYTES: usize = 17;
const SEGMENT_ENTRY_BYTES: usize = 33;
const REF_ENTRY_BYTES: usize = 4;
const SPATIAL_FILE_BUFFER_BYTES: usize = 8 * 1024 * 1024;
const SPATIAL_ENCODE_BUFFER_BYTES: usize = 8 * 1024 * 1024;

const COORDINATE_SCALE: f64 = 10_000_000.0;
const FRACTION_SCALE: f64 = u32::MAX as f64;
const H3_FINE_RESOLUTION: Resolution = Resolution::Eleven;
const H3_CONTEXT_RESOLUTION: Resolution = Resolution::Six;
const H3_SEGMENT_SAMPLE_DIVISOR: f64 = 2.0;
const H3_RADIUS_EXTRA_RING: u32 = 1;
const H3_MAX_QUERY_K: u32 = 128;
const EARTH_RADIUS_M: f64 = 6_371_008.8;
const SPATIAL_PAIR_CHUNK_SIZE: usize = 8_192;

#[derive(Debug, Default)]
pub struct PackSpatialIndexWriter {
    points: Vec<SpatialPointEntry>,
    segments: Vec<SpatialSegmentEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpatialIndexCommit {
    pub schema_version: u32,
    pub relative_path: String,
    pub point_count: u64,
    pub segment_count: u64,
    pub build_timings: SpatialIndexBuildTimings,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpatialIndexBuildTimings {
    pub point_pair_generation_ms: u128,
    pub segment_pair_generation_ms: u128,
    pub pair_sort_dedupe_ms: u128,
    pub cell_directory_build_ms: u128,
    pub file_write_ms: u128,
}

pub struct PackSpatialIndexReader {
    index: SpatialIndexV2Reader,
}

impl fmt::Debug for PackSpatialIndexReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackSpatialIndexReader")
            .field("index", &self.index)
            .finish()
    }
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SpatialIndexV2Manifest {
    schema_version: u32,
    h3_fine_resolution: u8,
    h3_context_resolution: u8,
    coordinate_scale: f64,
    point_count: u64,
    segment_count: u64,
    cell_count: u64,
    context_cell_count: u64,
    cell_point_ref_count: u64,
    cell_segment_ref_count: u64,
    context_cell_point_ref_count: u64,
}

struct SpatialIndexV2Reader {
    manifest: SpatialIndexV2Manifest,
    cells: CountedMmap,
    points: CountedMmap,
    segments: CountedMmap,
    cell_points: CountedMmap,
    cell_segments: CountedMmap,
    context_cells: CountedMmap,
    context_cell_points: CountedMmap,
}

impl fmt::Debug for SpatialIndexV2Reader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpatialIndexV2Reader")
            .field("manifest", &self.manifest)
            .field("cells", &self.cells)
            .field("points", &self.points)
            .field("segments", &self.segments)
            .field("cell_points", &self.cell_points)
            .field("cell_segments", &self.cell_segments)
            .field("context_cells", &self.context_cells)
            .field("context_cell_points", &self.context_cell_points)
            .finish()
    }
}

struct CountedMmap {
    bytes: Mmap,
    count: u64,
    entry_bytes: usize,
}

impl fmt::Debug for CountedMmap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CountedMmap")
            .field("count", &self.count)
            .field("entry_bytes", &self.entry_bytes)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
struct CellDirectoryEntry {
    h3_cell: u64,
    point_start: u64,
    point_count: u64,
    segment_start: u64,
    segment_count: u64,
}

const _: () = assert!(mem::size_of::<CellDirectoryEntry>() == CELL_ENTRY_BYTES);
const _: () = assert!(mem::size_of::<u32>() == REF_ENTRY_BYTES);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CellRefPair {
    h3_cell: u64,
    id: u32,
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
        self.finish_v2(pack_path)
    }

    fn finish_v2(self, pack_path: &Path) -> Result<SpatialIndexCommit> {
        let point_count = self.points.len() as u64;
        let segment_count = self.segments.len() as u64;
        let root = pack_path.join(SPATIAL_INDEX_V2_RELATIVE_DIR);
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create {}", root.display()))?;
        let mut build_timings = SpatialIndexBuildTimings::default();

        let started = Instant::now();
        let (mut point_pairs, mut context_point_pairs) = build_point_pairs(&self.points)?;
        build_timings.point_pair_generation_ms = elapsed_ms(started);

        let started = Instant::now();
        let mut segment_pairs = build_segment_pairs(&self.segments)?;
        build_timings.segment_pair_generation_ms = elapsed_ms(started);

        let started = Instant::now();
        sort_dedupe_cell_pairs(&mut point_pairs);
        sort_dedupe_cell_pairs(&mut segment_pairs);
        sort_dedupe_cell_pairs(&mut context_point_pairs);
        build_timings.pair_sort_dedupe_ms = elapsed_ms(started);

        let started = Instant::now();
        let (cells, point_refs, segment_refs) = build_cell_directory(&point_pairs, &segment_pairs);
        let (context_cells, context_point_refs, context_segment_refs) =
            build_cell_directory(&context_point_pairs, &[]);
        debug_assert!(context_segment_refs.is_empty());
        build_timings.cell_directory_build_ms = elapsed_ms(started);

        let started = Instant::now();
        write_points_file(&root.join(V2_POINTS_FILE), &self.points)?;
        write_segments_file(&root.join(V2_SEGMENTS_FILE), &self.segments)?;
        write_cells_file(&root.join(V2_CELLS_FILE), &cells)?;
        write_refs_file(
            &root.join(V2_CELL_POINTS_FILE),
            V2_POINT_REFS_MAGIC,
            &point_refs,
        )?;
        write_refs_file(
            &root.join(V2_CELL_SEGMENTS_FILE),
            V2_SEGMENT_REFS_MAGIC,
            &segment_refs,
        )?;
        write_cells_file(&root.join(V2_CONTEXT_CELLS_FILE), &context_cells)?;
        write_refs_file(
            &root.join(V2_CONTEXT_CELL_POINTS_FILE),
            V2_POINT_REFS_MAGIC,
            &context_point_refs,
        )?;

        let manifest = SpatialIndexV2Manifest {
            schema_version: SPATIAL_INDEX_SCHEMA_VERSION,
            h3_fine_resolution: u8::from(H3_FINE_RESOLUTION),
            h3_context_resolution: u8::from(H3_CONTEXT_RESOLUTION),
            coordinate_scale: COORDINATE_SCALE,
            point_count,
            segment_count,
            cell_count: cells.len() as u64,
            context_cell_count: context_cells.len() as u64,
            cell_point_ref_count: point_refs.len() as u64,
            cell_segment_ref_count: segment_refs.len() as u64,
            context_cell_point_ref_count: context_point_refs.len() as u64,
        };
        let manifest_path = root.join(SPATIAL_INDEX_V2_MANIFEST);
        let manifest_file = File::create(&manifest_path)
            .with_context(|| format!("failed to create {}", manifest_path.display()))?;
        serde_json::to_writer_pretty(manifest_file, &manifest)
            .with_context(|| format!("failed to write {}", manifest_path.display()))?;
        build_timings.file_write_ms = elapsed_ms(started);

        Ok(SpatialIndexCommit {
            schema_version: SPATIAL_INDEX_SCHEMA_VERSION,
            relative_path: SPATIAL_INDEX_V2_RELATIVE_DIR.to_string(),
            point_count,
            segment_count,
            build_timings,
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
        let pack_path = pack_path.as_ref();
        Ok(Self {
            index: SpatialIndexV2Reader::open(pack_path)?,
        })
    }

    pub fn point_candidates(
        &self,
        lon: f64,
        lat: f64,
        layer: SpatialLayer,
        radius_m: f64,
        limit: usize,
    ) -> Vec<PointCandidate> {
        self.index
            .point_candidates(lon, lat, layer, radius_m, limit)
    }

    pub fn context_candidates(
        &self,
        lon: f64,
        lat: f64,
        radius_m: f64,
        limit: usize,
    ) -> Vec<PointCandidate> {
        self.index.context_candidates(lon, lat, radius_m, limit)
    }

    pub fn segment_candidates(
        &self,
        lon: f64,
        lat: f64,
        layer: SpatialLayer,
        radius_m: f64,
        limit: usize,
    ) -> Vec<SegmentCandidate> {
        self.index
            .segment_candidates(lon, lat, layer, radius_m, limit)
    }
}

impl SpatialIndexV2Reader {
    fn open(pack_path: &Path) -> Result<Self> {
        let root = pack_path.join(SPATIAL_INDEX_V2_RELATIVE_DIR);
        let manifest_path = root.join(SPATIAL_INDEX_V2_MANIFEST);
        let manifest_file = File::open(&manifest_path)
            .with_context(|| format!("failed to open {}", manifest_path.display()))?;
        let manifest: SpatialIndexV2Manifest = serde_json::from_reader(manifest_file)
            .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
        if manifest.schema_version != SPATIAL_INDEX_SCHEMA_VERSION {
            bail!(
                "unsupported spatial index schema version {}; expected {}",
                manifest.schema_version,
                SPATIAL_INDEX_SCHEMA_VERSION
            );
        }

        Ok(Self {
            manifest,
            cells: open_counted_mmap(root.join(V2_CELLS_FILE), V2_CELLS_MAGIC, CELL_ENTRY_BYTES)?,
            points: open_counted_mmap(
                root.join(V2_POINTS_FILE),
                V2_POINTS_MAGIC,
                POINT_ENTRY_BYTES,
            )?,
            segments: open_counted_mmap(
                root.join(V2_SEGMENTS_FILE),
                V2_SEGMENTS_MAGIC,
                SEGMENT_ENTRY_BYTES,
            )?,
            cell_points: open_counted_mmap(
                root.join(V2_CELL_POINTS_FILE),
                V2_POINT_REFS_MAGIC,
                REF_ENTRY_BYTES,
            )?,
            cell_segments: open_counted_mmap(
                root.join(V2_CELL_SEGMENTS_FILE),
                V2_SEGMENT_REFS_MAGIC,
                REF_ENTRY_BYTES,
            )?,
            context_cells: open_counted_mmap(
                root.join(V2_CONTEXT_CELLS_FILE),
                V2_CELLS_MAGIC,
                CELL_ENTRY_BYTES,
            )?,
            context_cell_points: open_counted_mmap(
                root.join(V2_CONTEXT_CELL_POINTS_FILE),
                V2_POINT_REFS_MAGIC,
                REF_ENTRY_BYTES,
            )?,
        })
    }

    fn point_candidates(
        &self,
        lon: f64,
        lat: f64,
        layer: SpatialLayer,
        radius_m: f64,
        limit: usize,
    ) -> Vec<PointCandidate> {
        let mut candidates = Vec::new();
        for h3_cell in h3_query_cell_ids(lon, lat, H3_FINE_RESOLUTION, radius_m) {
            let Some(cell) = self.find_cell(&self.cells, h3_cell) else {
                continue;
            };
            for point_id in self.point_ref_ids(cell.point_start, cell.point_count) {
                let Some(entry) = self.read_point(point_id) else {
                    continue;
                };
                if entry.layer != layer {
                    continue;
                }
                let distance_m = haversine_m(lon, lat, entry.lon, entry.lat);
                if distance_m <= radius_m {
                    candidates.push(PointCandidate {
                        record_id: entry.record_id,
                        layer: entry.layer,
                        lon: entry.lon,
                        lat: entry.lat,
                        distance_m,
                    });
                }
            }
        }
        candidates.sort_by(compare_distance);
        truncate(candidates, limit)
    }

    fn context_candidates(
        &self,
        lon: f64,
        lat: f64,
        radius_m: f64,
        limit: usize,
    ) -> Vec<PointCandidate> {
        let mut candidates = Vec::new();
        for h3_cell in h3_query_cell_ids(lon, lat, H3_CONTEXT_RESOLUTION, radius_m) {
            let Some(cell) = self.find_cell(&self.context_cells, h3_cell) else {
                continue;
            };
            for point_id in self.context_point_ref_ids(cell.point_start, cell.point_count) {
                let Some(entry) = self.read_point(point_id) else {
                    continue;
                };
                if !is_context_layer(entry.layer) {
                    continue;
                }
                let distance_m = haversine_m(lon, lat, entry.lon, entry.lat);
                if distance_m <= radius_m {
                    candidates.push(PointCandidate {
                        record_id: entry.record_id,
                        layer: entry.layer,
                        lon: entry.lon,
                        lat: entry.lat,
                        distance_m,
                    });
                }
            }
        }
        candidates.sort_by(compare_distance);
        truncate(candidates, limit)
    }

    fn segment_candidates(
        &self,
        lon: f64,
        lat: f64,
        layer: SpatialLayer,
        radius_m: f64,
        limit: usize,
    ) -> Vec<SegmentCandidate> {
        let mut seen_segment_ids = BTreeSet::new();
        let mut candidates = Vec::new();
        for h3_cell in h3_query_cell_ids(lon, lat, H3_FINE_RESOLUTION, radius_m) {
            let Some(cell) = self.find_cell(&self.cells, h3_cell) else {
                continue;
            };
            for segment_id in self.segment_ref_ids(cell.segment_start, cell.segment_count) {
                if !seen_segment_ids.insert(segment_id) {
                    continue;
                }
                let Some(entry) = self.read_segment(segment_id) else {
                    continue;
                };
                if entry.layer != layer {
                    continue;
                }
                let projection = project_to_segment_m(
                    lon,
                    lat,
                    entry.start_lon,
                    entry.start_lat,
                    entry.end_lon,
                    entry.end_lat,
                );
                if projection.distance_m <= radius_m {
                    candidates.push(SegmentCandidate {
                        record_id: entry.record_id,
                        layer: entry.layer,
                        closest_lon: projection.lon,
                        closest_lat: projection.lat,
                        distance_m: projection.distance_m,
                        fraction: entry.start_fraction
                            + projection.t * (entry.end_fraction - entry.start_fraction),
                    });
                }
            }
        }
        candidates.sort_by(compare_distance);
        truncate(candidates, limit)
    }

    fn find_cell(&self, cells: &CountedMmap, h3_cell: u64) -> Option<CellDirectoryEntry> {
        let mut low = 0;
        let mut high = cells.count;
        while low < high {
            let mid = low + (high - low) / 2;
            let entry = read_cell_entry(cells, mid)?;
            match entry.h3_cell.cmp(&h3_cell) {
                Ordering::Less => low = mid + 1,
                Ordering::Greater => high = mid,
                Ordering::Equal => return Some(entry),
            }
        }
        None
    }

    fn read_point(&self, point_id: u32) -> Option<SpatialPointEntry> {
        read_point_entry(&self.points, u64::from(point_id))
    }

    fn read_segment(&self, segment_id: u32) -> Option<SpatialSegmentEntry> {
        read_segment_entry(&self.segments, u64::from(segment_id))
    }

    fn point_ref_ids(&self, start: u64, count: u64) -> impl Iterator<Item = u32> + '_ {
        read_ref_range(&self.cell_points, start, count)
    }

    fn segment_ref_ids(&self, start: u64, count: u64) -> impl Iterator<Item = u32> + '_ {
        read_ref_range(&self.cell_segments, start, count)
    }

    fn context_point_ref_ids(&self, start: u64, count: u64) -> impl Iterator<Item = u32> + '_ {
        read_ref_range(&self.context_cell_points, start, count)
    }
}

fn build_point_pairs(points: &[SpatialPointEntry]) -> Result<(Vec<CellRefPair>, Vec<CellRefPair>)> {
    let mut point_pairs = Vec::with_capacity(points.len());
    let mut context_point_pairs = Vec::new();

    for (index, point) in points.iter().enumerate() {
        let point_id = u32::try_from(index).context("too many spatial points for v2 index")?;
        point_pairs.push(CellRefPair {
            h3_cell: h3_cell_id(point.lon, point.lat, H3_FINE_RESOLUTION)?,
            id: point_id,
        });
        if is_context_layer(point.layer) {
            context_point_pairs.push(CellRefPair {
                h3_cell: h3_cell_id(point.lon, point.lat, H3_CONTEXT_RESOLUTION)?,
                id: point_id,
            });
        }
    }

    Ok((point_pairs, context_point_pairs))
}

fn build_segment_pairs(segments: &[SpatialSegmentEntry]) -> Result<Vec<CellRefPair>> {
    let chunks: Result<Vec<Vec<CellRefPair>>> = segments
        .par_chunks(SPATIAL_PAIR_CHUNK_SIZE)
        .enumerate()
        .map(|(chunk_index, chunk)| {
            let mut pairs = Vec::with_capacity(chunk.len() * 3);
            let base_index = chunk_index * SPATIAL_PAIR_CHUNK_SIZE;
            for (offset, segment) in chunk.iter().enumerate() {
                let segment_id = u32::try_from(base_index + offset)
                    .context("too many spatial segments for v2 index")?;
                for h3_cell in h3_segment_cell_ids(segment, H3_FINE_RESOLUTION)? {
                    pairs.push(CellRefPair {
                        h3_cell,
                        id: segment_id,
                    });
                }
            }
            Ok(pairs)
        })
        .collect();

    let chunks = chunks?;
    let total_pairs = chunks.iter().map(Vec::len).sum();
    let mut pairs = Vec::with_capacity(total_pairs);
    for mut chunk in chunks {
        pairs.append(&mut chunk);
    }
    Ok(pairs)
}

fn sort_dedupe_cell_pairs(pairs: &mut Vec<CellRefPair>) {
    pairs.par_sort_unstable();
    pairs.dedup();
}

fn build_cell_directory(
    point_pairs: &[CellRefPair],
    segment_pairs: &[CellRefPair],
) -> (Vec<CellDirectoryEntry>, Vec<u32>, Vec<u32>) {
    let mut entries = Vec::new();
    let mut point_refs = Vec::new();
    let mut segment_refs = Vec::new();
    let mut point_index = 0;
    let mut segment_index = 0;

    while point_index < point_pairs.len() || segment_index < segment_pairs.len() {
        let h3_cell = match (
            point_pairs.get(point_index).map(|pair| pair.h3_cell),
            segment_pairs.get(segment_index).map(|pair| pair.h3_cell),
        ) {
            (Some(point_cell), Some(segment_cell)) => point_cell.min(segment_cell),
            (Some(point_cell), None) => point_cell,
            (None, Some(segment_cell)) => segment_cell,
            (None, None) => break,
        };

        let point_start = point_refs.len() as u64;
        while point_pairs
            .get(point_index)
            .is_some_and(|pair| pair.h3_cell == h3_cell)
        {
            point_refs.push(point_pairs[point_index].id);
            point_index += 1;
        }
        let point_count = point_refs.len() as u64 - point_start;

        let segment_start = segment_refs.len() as u64;
        while segment_pairs
            .get(segment_index)
            .is_some_and(|pair| pair.h3_cell == h3_cell)
        {
            segment_refs.push(segment_pairs[segment_index].id);
            segment_index += 1;
        }
        let segment_count = segment_refs.len() as u64 - segment_start;

        entries.push(CellDirectoryEntry {
            h3_cell,
            point_start,
            point_count,
            segment_start,
            segment_count,
        });
    }
    (entries, point_refs, segment_refs)
}

fn h3_cell_id(lon: f64, lat: f64, resolution: Resolution) -> Result<u64> {
    let lat_lng = LatLng::new(lat, lon).context("invalid coordinate for h3 cell")?;
    Ok(u64::from(lat_lng.to_cell(resolution)))
}

fn h3_query_cell_ids(lon: f64, lat: f64, resolution: Resolution, radius_m: f64) -> Vec<u64> {
    let Ok(lat_lng) = LatLng::new(lat, lon) else {
        return Vec::new();
    };
    let cell = lat_lng.to_cell(resolution);
    let k = h3_radius_k(resolution, radius_m);
    cell.grid_disk::<Vec<CellIndex>>(k)
        .into_iter()
        .map(u64::from)
        .collect()
}

fn h3_segment_cell_ids(segment: &SpatialSegmentEntry, resolution: Resolution) -> Result<Vec<u64>> {
    let length = haversine_m(
        segment.start_lon,
        segment.start_lat,
        segment.end_lon,
        segment.end_lat,
    );
    let step = (resolution.edge_length_m() / H3_SEGMENT_SAMPLE_DIVISOR).max(1.0);
    let sample_count = ((length / step).ceil() as usize).max(1);
    let mut cells = Vec::with_capacity(sample_count + 1);
    for sample in 0..=sample_count {
        let t = sample as f64 / sample_count as f64;
        let lon = segment.start_lon + t * (segment.end_lon - segment.start_lon);
        let lat = segment.start_lat + t * (segment.end_lat - segment.start_lat);
        cells.push(h3_cell_id(lon, lat, resolution)?);
    }
    cells.sort_unstable();
    cells.dedup();
    Ok(cells)
}

fn h3_radius_k(resolution: Resolution, radius_m: f64) -> u32 {
    if radius_m <= 0.0 {
        return H3_RADIUS_EXTRA_RING;
    }
    ((radius_m / resolution.edge_length_m()).ceil() as u32)
        .saturating_add(H3_RADIUS_EXTRA_RING)
        .min(H3_MAX_QUERY_K)
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

fn write_cells_file(path: &Path, entries: &[CellDirectoryEntry]) -> Result<()> {
    write_counted_file(path, V2_CELLS_MAGIC, entries.len() as u64, |file| {
        file.write_all(bytemuck::cast_slice(entries))?;
        Ok(())
    })
}

fn write_points_file(path: &Path, points: &[SpatialPointEntry]) -> Result<()> {
    write_counted_file(path, V2_POINTS_MAGIC, points.len() as u64, |file| {
        let mut buffer = Vec::with_capacity(SPATIAL_ENCODE_BUFFER_BYTES);
        for point in points {
            flush_if_full(&mut buffer, POINT_ENTRY_BYTES, file)?;
            buffer.extend_from_slice(&point.record_id.to_le_bytes());
            buffer.push(spatial_layer_code(point.layer));
            buffer.extend_from_slice(&quantize_coordinate(point.lon).to_le_bytes());
            buffer.extend_from_slice(&quantize_coordinate(point.lat).to_le_bytes());
        }
        if !buffer.is_empty() {
            file.write_all(&buffer)?;
        }
        Ok(())
    })
}

fn write_segments_file(path: &Path, segments: &[SpatialSegmentEntry]) -> Result<()> {
    write_counted_file(path, V2_SEGMENTS_MAGIC, segments.len() as u64, |file| {
        let mut buffer = Vec::with_capacity(SPATIAL_ENCODE_BUFFER_BYTES);
        for segment in segments {
            flush_if_full(&mut buffer, SEGMENT_ENTRY_BYTES, file)?;
            buffer.extend_from_slice(&segment.record_id.to_le_bytes());
            buffer.push(spatial_layer_code(segment.layer));
            buffer.extend_from_slice(&quantize_coordinate(segment.start_lon).to_le_bytes());
            buffer.extend_from_slice(&quantize_coordinate(segment.start_lat).to_le_bytes());
            buffer.extend_from_slice(&quantize_coordinate(segment.end_lon).to_le_bytes());
            buffer.extend_from_slice(&quantize_coordinate(segment.end_lat).to_le_bytes());
            buffer.extend_from_slice(&quantize_fraction(segment.start_fraction).to_le_bytes());
            buffer.extend_from_slice(&quantize_fraction(segment.end_fraction).to_le_bytes());
        }
        if !buffer.is_empty() {
            file.write_all(&buffer)?;
        }
        Ok(())
    })
}

fn write_refs_file(path: &Path, magic: &[u8; 8], refs: &[u32]) -> Result<()> {
    write_counted_file(path, magic, refs.len() as u64, |file| {
        file.write_all(bytemuck::cast_slice(refs))?;
        Ok(())
    })
}

fn flush_if_full(
    buffer: &mut Vec<u8>,
    next_entry_bytes: usize,
    file: &mut BufWriter<File>,
) -> Result<()> {
    if buffer.len() + next_entry_bytes > SPATIAL_ENCODE_BUFFER_BYTES {
        file.write_all(buffer)?;
        buffer.clear();
    }
    Ok(())
}

fn write_counted_file(
    path: &Path,
    magic: &[u8; 8],
    count: u64,
    write_entries: impl FnOnce(&mut BufWriter<File>) -> Result<()>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    let mut file = BufWriter::with_capacity(SPATIAL_FILE_BUFFER_BYTES, file);
    file.write_all(magic)?;
    file.write_all(&count.to_le_bytes())?;
    write_entries(&mut file)?;
    file.flush()?;
    Ok(())
}

fn open_counted_mmap(
    path: PathBuf,
    expected_magic: &[u8; 8],
    entry_bytes: usize,
) -> Result<CountedMmap> {
    let file = File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
    // SAFETY: the map is read-only, the file handle is not used for writes here,
    // and Pack files are immutable once built.
    let bytes = unsafe { MmapOptions::new().map(&file) }
        .with_context(|| format!("failed to mmap {}", path.display()))?;
    if bytes.len() < COUNTED_HEADER_BYTES {
        bail!("{} is too short for a counted spatial file", path.display());
    }
    let Some(magic) = bytes.get(0..8) else {
        bail!("{} is missing magic", path.display());
    };
    if magic != expected_magic {
        bail!("{} has an invalid magic header", path.display());
    }
    let count = read_u64(&bytes, 8).expect("validated counted header");
    let entries_bytes = usize::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(entry_bytes))
        .context("spatial file entry count overflows usize")?;
    let expected_len = COUNTED_HEADER_BYTES
        .checked_add(entries_bytes)
        .context("spatial file length overflows usize")?;
    if bytes.len() != expected_len {
        bail!(
            "{} has {} bytes but expected {}",
            path.display(),
            bytes.len(),
            expected_len
        );
    }
    Ok(CountedMmap {
        bytes,
        count,
        entry_bytes,
    })
}

fn read_cell_entry(cells: &CountedMmap, index: u64) -> Option<CellDirectoryEntry> {
    let offset = entry_offset(cells, index)?;
    Some(CellDirectoryEntry {
        h3_cell: read_u64(&cells.bytes, offset)?,
        point_start: read_u64(&cells.bytes, offset + 8)?,
        point_count: read_u64(&cells.bytes, offset + 16)?,
        segment_start: read_u64(&cells.bytes, offset + 24)?,
        segment_count: read_u64(&cells.bytes, offset + 32)?,
    })
}

fn read_point_entry(points: &CountedMmap, index: u64) -> Option<SpatialPointEntry> {
    let offset = entry_offset(points, index)?;
    let layer = spatial_layer_from_code(*points.bytes.get(offset + 8)?)?;
    Some(SpatialPointEntry {
        record_id: read_u64(&points.bytes, offset)?,
        layer,
        lon: dequantize_coordinate(read_i32(&points.bytes, offset + 9)?),
        lat: dequantize_coordinate(read_i32(&points.bytes, offset + 13)?),
    })
}

fn read_segment_entry(segments: &CountedMmap, index: u64) -> Option<SpatialSegmentEntry> {
    let offset = entry_offset(segments, index)?;
    let layer = spatial_layer_from_code(*segments.bytes.get(offset + 8)?)?;
    Some(SpatialSegmentEntry {
        record_id: read_u64(&segments.bytes, offset)?,
        layer,
        start_lon: dequantize_coordinate(read_i32(&segments.bytes, offset + 9)?),
        start_lat: dequantize_coordinate(read_i32(&segments.bytes, offset + 13)?),
        end_lon: dequantize_coordinate(read_i32(&segments.bytes, offset + 17)?),
        end_lat: dequantize_coordinate(read_i32(&segments.bytes, offset + 21)?),
        start_fraction: dequantize_fraction(read_u32(&segments.bytes, offset + 25)?),
        end_fraction: dequantize_fraction(read_u32(&segments.bytes, offset + 29)?),
    })
}

fn read_ref_range(refs: &CountedMmap, start: u64, count: u64) -> impl Iterator<Item = u32> + '_ {
    (start..start.saturating_add(count)).filter_map(move |index| {
        let offset = entry_offset(refs, index)?;
        read_u32(&refs.bytes, offset)
    })
}

fn entry_offset(file: &CountedMmap, index: u64) -> Option<usize> {
    if index >= file.count {
        return None;
    }
    let index = usize::try_from(index).ok()?;
    let offset = COUNTED_HEADER_BYTES.checked_add(index.checked_mul(file.entry_bytes)?)?;
    (offset + file.entry_bytes <= file.bytes.len()).then_some(offset)
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let array: [u8; 8] = bytes.get(offset..offset + 8)?.try_into().ok()?;
    Some(u64::from_le_bytes(array))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let array: [u8; 4] = bytes.get(offset..offset + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(array))
}

fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    let array: [u8; 4] = bytes.get(offset..offset + 4)?.try_into().ok()?;
    Some(i32::from_le_bytes(array))
}

fn quantize_coordinate(value: f64) -> i32 {
    (value * COORDINATE_SCALE)
        .round()
        .clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

fn dequantize_coordinate(value: i32) -> f64 {
    value as f64 / COORDINATE_SCALE
}

fn quantize_fraction(value: f64) -> u32 {
    (value.clamp(0.0, 1.0) * FRACTION_SCALE).round() as u32
}

fn dequantize_fraction(value: u32) -> f64 {
    value as f64 / FRACTION_SCALE
}

fn spatial_layer_code(layer: SpatialLayer) -> u8 {
    match layer {
        SpatialLayer::Address => 1,
        SpatialLayer::Country => 2,
        SpatialLayer::District => 3,
        SpatialLayer::Interpolation => 4,
        SpatialLayer::Locality => 5,
        SpatialLayer::Neighbourhood => 6,
        SpatialLayer::Place => 7,
        SpatialLayer::Postcode => 8,
        SpatialLayer::Region => 9,
        SpatialLayer::Street => 10,
    }
}

fn spatial_layer_from_code(value: u8) -> Option<SpatialLayer> {
    match value {
        1 => Some(SpatialLayer::Address),
        2 => Some(SpatialLayer::Country),
        3 => Some(SpatialLayer::District),
        4 => Some(SpatialLayer::Interpolation),
        5 => Some(SpatialLayer::Locality),
        6 => Some(SpatialLayer::Neighbourhood),
        7 => Some(SpatialLayer::Place),
        8 => Some(SpatialLayer::Postcode),
        9 => Some(SpatialLayer::Region),
        10 => Some(SpatialLayer::Street),
        _ => None,
    }
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
        let commit = writer.finish(&temp_dir).expect("finish");

        assert_eq!(commit.schema_version, SPATIAL_INDEX_SCHEMA_VERSION);
        assert_eq!(commit.relative_path, SPATIAL_INDEX_V2_RELATIVE_DIR);
        assert!(temp_dir.join(SPATIAL_INDEX_V2_RELATIVE_DIR).exists());
        let manifest = read_test_manifest(&temp_dir);
        assert_eq!(manifest.h3_fine_resolution, 11);
        assert_eq!(manifest.h3_context_resolution, 6);

        let reader = PackSpatialIndexReader::open(&temp_dir).expect("reader");
        let hits = reader.point_candidates(-79.0, 43.0, SpatialLayer::Address, 5.0, 1);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, 7);

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn indexes_context_points_with_coarse_h3_cells() {
        let mut writer = PackSpatialIndexWriter::default();
        let record = NormalizedRecord::postcode(crate::record::PostcodeRecord {
            id: "derived:postcode:m5v".to_string(),
            label: "M5V".to_string(),
            name: "M5V".to_string(),
            postcode: "M5V".to_string(),
            geometry: point_geometry(-79.4, 43.6),
            source: crate::record::DerivedSourceProvenance::osm_address_records(1),
        });

        writer.add_record(11, &record).expect("add record");
        let temp_dir = std::env::temp_dir().join(format!(
            "open-geocode-spatial-context-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp_dir);
        writer.finish(&temp_dir).expect("finish");

        let reader = PackSpatialIndexReader::open(&temp_dir).expect("reader");
        let hits = reader.context_candidates(-79.39, 43.6, 5_000.0, 5);

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, 11);

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

    fn read_test_manifest(path: &Path) -> SpatialIndexV2Manifest {
        let file = File::open(
            path.join(SPATIAL_INDEX_V2_RELATIVE_DIR)
                .join(SPATIAL_INDEX_V2_MANIFEST),
        )
        .expect("manifest");
        serde_json::from_reader(file).expect("parse manifest")
    }
}
