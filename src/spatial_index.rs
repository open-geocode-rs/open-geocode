//! H3 spatial index over the record store.
//!
//! The index maps H3 cells to record references. It stores no coordinates of
//! its own: point references are record ids and segment references are
//! `(record id, segment index)`, both resolved against the record store, which
//! already holds every point and line. Because records are in Hilbert order,
//! the ids inside one cell are close together and their deltas are tiny.
//!
//! Two indexes share one layout: a fine one (H3 resolution 11) over addresses,
//! POIs, places, postcodes and road segments, and a coarse one (resolution 6) over
//! context points for wide-radius admin lookups.
//!
//! ```text
//! spatial/<name>/index  cell_count u64, block_count u64,
//!                       then per block of 16 cells: first_cell u64, data_offset u64,
//!                       then the data length u64
//! spatial/<name>/data   per cell: varint cell delta (0 for a block's first cell),
//!                       varint refs length, refs:
//!                       varint point count, delta-coded record ids,
//!                       varint segment count, delta-coded segment refs
//! ```

use std::{cmp::Ordering, collections::HashMap, io::Write, sync::Arc};

use anyhow::{Context, Result, bail};
use geojson::GeometryValue;
use h3o::{CellIndex, LatLng, Resolution};
use serde::Serialize;

use crate::{
    container::{Bytes, Container, ContainerWriter},
    extsort::{ExternalSorter, Scratch, SortStats, Spill},
    memory::MemoryBudget,
    pack::RecordId,
    record::{Layer, Record},
    records::{RecordsReader, dequantize},
    util::codec::{get_u64, put_u64, read_u64_le},
};

pub const SPATIAL_VERSION: u32 = 2;
const FINE: &str = "spatial/fine";
const CONTEXT: &str = "spatial/context";

const H3_FINE_RESOLUTION: Resolution = Resolution::Eleven;
const H3_CONTEXT_RESOLUTION: Resolution = Resolution::Six;
const H3_SEGMENT_SAMPLE_DIVISOR: f64 = 2.0;
const H3_RADIUS_EXTRA_RING: u32 = 1;
const H3_MAX_QUERY_K: u32 = 128;
const EARTH_RADIUS_M: f64 = 6_371_008.8;
const CELLS_PER_BLOCK: u64 = 16;
const SEGMENT_INDEX_BITS: u32 = 16;
const KIND_POINT: u8 = 0;
const KIND_SEGMENT: u8 = 1;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PointCandidate {
    pub record_id: RecordId,
    pub layer: Layer,
    pub lon: f64,
    pub lat: f64,
    pub distance_m: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SegmentCandidate {
    pub record_id: RecordId,
    pub layer: Layer,
    pub closest_lon: f64,
    pub closest_lat: f64,
    pub distance_m: f64,
    pub fraction: f64,
}

/// One (cell, reference) pair produced while records are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellRef {
    cell: u64,
    kind: u8,
    reference: u64,
}

impl Spill for CellRef {
    fn encode(&self, out: &mut Vec<u8>) {
        (self.cell, self.kind, self.reference).encode(out);
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let (cell, kind, reference) = <(u64, u8, u64)>::decode(input)?;
        Ok(Self {
            cell,
            kind,
            reference,
        })
    }
}

/// Index pairs for one record, computed off the write path in parallel.
#[derive(Debug, Default)]
pub struct RecordCells {
    fine: Vec<CellRef>,
    context: Vec<CellRef>,
    points: u64,
    segments: u64,
}

impl RecordCells {
    pub fn for_record(record_id: RecordId, record: &Record) -> Result<Self> {
        let mut cells = Self::default();
        match record {
            Record::Street(_) | Record::Interpolation(_) => {
                let GeometryValue::LineString { coordinates } = &record.geometry().value else {
                    return Ok(cells);
                };
                let positions = coordinates
                    .iter()
                    .filter_map(|position| match position.as_slice() {
                        [lon, lat, ..] if lon.is_finite() && lat.is_finite() => Some([*lon, *lat]),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                for (index, pair) in positions.windows(2).enumerate() {
                    if haversine_m(pair[0][0], pair[0][1], pair[1][0], pair[1][1]) <= f64::EPSILON {
                        continue;
                    }
                    let index = u64::try_from(index)?;
                    if index >= 1 << SEGMENT_INDEX_BITS {
                        bail!("line record {} has too many segments to index", record.id());
                    }
                    let reference = record_id << SEGMENT_INDEX_BITS | index;
                    for cell in segment_cells(pair[0], pair[1], H3_FINE_RESOLUTION)? {
                        cells.fine.push(CellRef {
                            cell,
                            kind: KIND_SEGMENT,
                            reference,
                        });
                    }
                    cells.segments += 1;
                }
            }
            _ => {
                let Some([lon, lat]) = record.display_point() else {
                    return Ok(cells);
                };
                cells.fine.push(CellRef {
                    cell: cell_id(lon, lat, H3_FINE_RESOLUTION)?,
                    kind: KIND_POINT,
                    reference: record_id,
                });
                if record.layer().is_context() {
                    cells.context.push(CellRef {
                        cell: cell_id(lon, lat, H3_CONTEXT_RESOLUTION)?,
                        kind: KIND_POINT,
                        reference: record_id,
                    });
                }
                cells.points += 1;
            }
        }
        Ok(cells)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpatialCommit {
    pub point_count: u64,
    pub segment_count: u64,
    pub cell_count: u64,
    pub context_cell_count: u64,
    pub fine_pairs: SortStats,
    pub context_pairs: SortStats,
}

pub struct SpatialIndexWriter {
    fine: ExternalSorter<CellRef>,
    context: ExternalSorter<CellRef>,
    points: u64,
    segments: u64,
}

impl SpatialIndexWriter {
    pub fn new(scratch: &Arc<Scratch>, budget: &Arc<MemoryBudget>, share_bytes: usize) -> Self {
        Self {
            fine: ExternalSorter::new(scratch, "spatial-fine", budget, share_bytes),
            context: ExternalSorter::new(scratch, "spatial-context", budget, share_bytes / 8),
            points: 0,
            segments: 0,
        }
    }

    pub fn add(&mut self, cells: RecordCells) -> Result<()> {
        for pair in cells.fine {
            self.fine.push(pair)?;
        }
        for pair in cells.context {
            self.context.push(pair)?;
        }
        self.points += cells.points;
        self.segments += cells.segments;
        Ok(())
    }

    pub fn finish(self, pack: &mut ContainerWriter) -> Result<SpatialCommit> {
        let fine_pairs = self.fine.stats();
        let context_pairs = self.context.stats();
        let cell_count = write_cell_index(pack, FINE, self.fine)?;
        let context_cell_count = write_cell_index(pack, CONTEXT, self.context)?;
        Ok(SpatialCommit {
            point_count: self.points,
            segment_count: self.segments,
            cell_count,
            context_cell_count,
            fine_pairs,
            context_pairs,
        })
    }
}

fn write_cell_index(
    pack: &mut ContainerWriter,
    name: &str,
    pairs: ExternalSorter<CellRef>,
) -> Result<u64> {
    let mut blocks: Vec<(u64, u64)> = Vec::new();
    let mut cell_count = 0u64;
    let mut data_len = 0u64;
    let mut previous_cell = 0u64;
    let mut current: Option<u64> = None;
    let mut points = Vec::new();
    let mut segments = Vec::new();
    let mut entry = Vec::new();
    let mut refs = Vec::new();

    pack.begin(&format!("{name}/data"), SPATIAL_VERSION)?;
    let mut flush = |pack: &mut ContainerWriter,
                     cell: u64,
                     points: &mut Vec<u64>,
                     segments: &mut Vec<u64>|
     -> Result<()> {
        if cell_count % CELLS_PER_BLOCK == 0 {
            blocks.push((cell, data_len));
            previous_cell = cell;
        }
        refs.clear();
        put_delta_list(&mut refs, points);
        put_delta_list(&mut refs, segments);
        entry.clear();
        put_u64(&mut entry, cell - previous_cell);
        put_u64(&mut entry, refs.len() as u64);
        pack.write_all(&entry)?;
        pack.write_all(&refs)?;
        data_len += (entry.len() + refs.len()) as u64;
        previous_cell = cell;
        cell_count += 1;
        points.clear();
        segments.clear();
        Ok(())
    };
    let mut last: Option<CellRef> = None;
    for pair in pairs.finish()? {
        let pair = pair?;
        if last == Some(pair) {
            continue;
        }
        last = Some(pair);
        if current.is_some_and(|cell| cell != pair.cell) {
            flush(
                pack,
                current.expect("current cell"),
                &mut points,
                &mut segments,
            )?;
        }
        current = Some(pair.cell);
        match pair.kind {
            KIND_POINT => points.push(pair.reference),
            _ => segments.push(pair.reference),
        }
    }
    if let Some(cell) = current {
        flush(pack, cell, &mut points, &mut segments)?;
    }
    pack.end()?;

    let mut index = Vec::with_capacity(24 + blocks.len() * 16);
    index.extend_from_slice(&cell_count.to_le_bytes());
    index.extend_from_slice(&(blocks.len() as u64).to_le_bytes());
    for (first_cell, offset) in &blocks {
        index.extend_from_slice(&first_cell.to_le_bytes());
        index.extend_from_slice(&offset.to_le_bytes());
    }
    index.extend_from_slice(&data_len.to_le_bytes());
    pack.add(&format!("{name}/index"), SPATIAL_VERSION, &index)?;
    Ok(cell_count)
}

fn put_delta_list(out: &mut Vec<u8>, values: &[u64]) {
    put_u64(out, values.len() as u64);
    let mut previous = 0;
    for value in values {
        put_u64(out, value - previous);
        previous = *value;
    }
}

fn get_delta_list(input: &mut &[u8]) -> Result<Vec<u64>> {
    let count = usize::try_from(get_u64(input)?)?;
    if count > input.len() {
        bail!("spatial reference list is truncated");
    }
    let mut values = Vec::with_capacity(count);
    let mut previous = 0u64;
    for _ in 0..count {
        previous = previous
            .checked_add(get_u64(input)?)
            .context("spatial reference overflows")?;
        values.push(previous);
    }
    Ok(values)
}

#[derive(Clone)]
struct CellTable {
    index: Bytes,
    data: Bytes,
    block_count: u64,
}

struct CellRefs {
    points: Vec<u64>,
    segments: Vec<u64>,
}

impl CellTable {
    fn open(container: &Container, name: &str) -> Result<Self> {
        let index = container.section(&format!("{name}/index"), SPATIAL_VERSION)?;
        let data = container.section(&format!("{name}/data"), SPATIAL_VERSION)?;
        let cell_count = read_u64_le(&index, 0).context("spatial index is truncated")?;
        let block_count = read_u64_le(&index, 8).context("spatial index is truncated")?;
        if block_count != cell_count.div_ceil(CELLS_PER_BLOCK)
            || index.len() as u64 != 24 + block_count * 16
            || read_u64_le(&index, index.len() - 8) != Some(data.len() as u64)
        {
            bail!("spatial index {name} is inconsistent with its data");
        }
        Ok(Self {
            index,
            data,
            block_count,
        })
    }

    fn block(&self, block: u64) -> (u64, u64) {
        let offset = 16 + block as usize * 16;
        (
            read_u64_le(&self.index, offset).expect("validated index"),
            read_u64_le(&self.index, offset + 8).expect("validated index"),
        )
    }

    fn refs(&self, cell: u64) -> Result<Option<CellRefs>> {
        // Last block whose first cell is <= cell.
        let (mut low, mut high) = (0, self.block_count);
        while low < high {
            let mid = low + (high - low) / 2;
            if self.block(mid).0 <= cell {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        let Some(block) = low.checked_sub(1) else {
            return Ok(None);
        };
        let (first_cell, start) = self.block(block);
        let end = if block + 1 < self.block_count {
            self.block(block + 1).1
        } else {
            self.data.len() as u64
        };
        let mut input = self
            .data
            .get(start as usize..end as usize)
            .context("spatial block is out of range")?;
        let mut current = first_cell;
        while !input.is_empty() {
            current += get_u64(&mut input)?;
            let len = usize::try_from(get_u64(&mut input)?)?;
            let mut refs = input.get(..len).context("spatial cell is truncated")?;
            input = &input[len..];
            match current.cmp(&cell) {
                Ordering::Less => continue,
                Ordering::Greater => return Ok(None),
                Ordering::Equal => {
                    return Ok(Some(CellRefs {
                        points: get_delta_list(&mut refs)?,
                        segments: get_delta_list(&mut refs)?,
                    }));
                }
            }
        }
        Ok(None)
    }
}

#[derive(Clone)]
pub struct SpatialIndexReader {
    fine: CellTable,
    context: CellTable,
    records: RecordsReader,
}

impl std::fmt::Debug for SpatialIndexReader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SpatialIndexReader")
            .field("fine_blocks", &self.fine.block_count)
            .field("context_blocks", &self.context.block_count)
            .finish()
    }
}

struct LineCache {
    points: Vec<[f64; 2]>,
    /// Cumulative length at each vertex, in metres.
    lengths: Vec<f64>,
}

impl SpatialIndexReader {
    pub fn open(container: &Container, records: RecordsReader) -> Result<Self> {
        Ok(Self {
            fine: CellTable::open(container, FINE)?,
            context: CellTable::open(container, CONTEXT)?,
            records,
        })
    }

    pub fn point_candidates(
        &self,
        lon: f64,
        lat: f64,
        layer: Layer,
        radius_m: f64,
        limit: usize,
    ) -> Result<Vec<PointCandidate>> {
        let candidates = self.collect_points(
            &self.fine,
            H3_FINE_RESOLUTION,
            lon,
            lat,
            radius_m,
            limit,
            |_, candidate| Ok(candidate == layer),
        )?;
        Ok(closest_candidates(candidates, limit))
    }

    /// Explicit address points: address records and POIs that carry an
    /// address.
    pub fn address_candidates(
        &self,
        lon: f64,
        lat: f64,
        radius_m: f64,
        limit: usize,
    ) -> Result<Vec<PointCandidate>> {
        let candidates = self.collect_points(
            &self.fine,
            H3_FINE_RESOLUTION,
            lon,
            lat,
            radius_m,
            limit,
            |record_id, layer| match layer {
                Layer::Address => Ok(true),
                Layer::Poi => self.records.is_explicit_address(record_id),
                _ => Ok(false),
            },
        )?;
        Ok(closest_candidates(candidates, limit))
    }

    pub fn context_candidates(
        &self,
        lon: f64,
        lat: f64,
        radius_m: f64,
        limit: usize,
    ) -> Result<Vec<PointCandidate>> {
        let candidates = self.collect_points(
            &self.context,
            H3_CONTEXT_RESOLUTION,
            lon,
            lat,
            radius_m,
            limit,
            |_, layer| Ok(layer.is_context()),
        )?;
        Ok(closest_candidates(candidates, limit))
    }

    /// Whether any context point of `layer` within `radius_m` satisfies
    /// `matches`, scanning rings outward and stopping at the first match.
    /// `None` when there is no point of that layer within the radius at all.
    pub fn any_context_point(
        &self,
        lon: f64,
        lat: f64,
        layer: Layer,
        radius_m: f64,
        mut matches: impl FnMut(RecordId) -> Result<bool>,
    ) -> Result<Option<bool>> {
        let Ok(lat_lng) = LatLng::new(lat, lon) else {
            return Ok(None);
        };
        let mut cells = lat_lng
            .to_cell(H3_CONTEXT_RESOLUTION)
            .grid_disk_distances::<Vec<(CellIndex, u32)>>(disk_k(H3_CONTEXT_RESOLUTION, radius_m));
        cells.sort_unstable_by_key(|(cell, k)| (*k, u64::from(*cell)));
        let mut seen = false;
        for (cell, _) in cells {
            let Some(refs) = self.context.refs(u64::from(cell))? else {
                continue;
            };
            for record_id in refs.points {
                let (point_layer, point_lon, point_lat) = self.records.point(record_id)?;
                if point_layer != layer || haversine_m(lon, lat, point_lon, point_lat) > radius_m {
                    continue;
                }
                seen = true;
                if matches(record_id)? {
                    return Ok(Some(true));
                }
            }
        }
        Ok(seen.then_some(false))
    }

    pub fn segment_candidates(
        &self,
        lon: f64,
        lat: f64,
        layer: Layer,
        radius_m: f64,
        limit: usize,
    ) -> Result<Vec<SegmentCandidate>> {
        let mut seen = std::collections::BTreeSet::new();
        let mut lines: HashMap<RecordId, Option<LineCache>> = HashMap::new();
        let mut candidates = Vec::new();
        for cell in query_cells(lon, lat, H3_FINE_RESOLUTION, radius_m) {
            let Some(refs) = self.fine.refs(cell)? else {
                continue;
            };
            for reference in refs.segments {
                if !seen.insert(reference) {
                    continue;
                }
                let record_id = reference >> SEGMENT_INDEX_BITS;
                let segment = (reference & ((1 << SEGMENT_INDEX_BITS) - 1)) as usize;
                let line = match lines.entry(record_id) {
                    std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(if self.records.header(record_id)?.layer == layer {
                            self.records.line(record_id)?.map(line_cache)
                        } else {
                            None
                        })
                    }
                };
                let Some(line) = line.as_ref() else {
                    continue;
                };
                let (Some(start), Some(end)) =
                    (line.points.get(segment), line.points.get(segment + 1))
                else {
                    bail!("segment reference {reference} is out of range");
                };
                let projection = project_to_segment_m(lon, lat, *start, *end);
                if projection.distance_m <= radius_m {
                    let total = *line.lengths.last().expect("line has vertices");
                    let start_fraction = line.lengths[segment] / total;
                    let end_fraction = line.lengths[segment + 1] / total;
                    candidates.push(SegmentCandidate {
                        record_id,
                        layer,
                        closest_lon: projection.lon,
                        closest_lat: projection.lat,
                        distance_m: projection.distance_m,
                        fraction: start_fraction + projection.t * (end_fraction - start_fraction),
                    });
                }
            }
        }
        Ok(closest_candidates(candidates, limit))
    }

    /// The point `fraction` of the way along a line record by length, the
    /// inverse of the `fraction` [`Self::segment_candidates`] reports. `None`
    /// for point records.
    pub fn point_along_line(&self, record_id: RecordId, fraction: f64) -> Result<Option<[f64; 2]>> {
        let Some(line) = self.records.line(record_id)?.map(line_cache) else {
            return Ok(None);
        };
        let total = *line.lengths.last().expect("line has vertices");
        let target = fraction.clamp(0.0, 1.0) * total;
        let segment = line
            .lengths
            .windows(2)
            .position(|pair| target <= pair[1])
            .unwrap_or(line.points.len().saturating_sub(2));
        let (Some(start), Some(end)) = (line.points.get(segment), line.points.get(segment + 1))
        else {
            return Ok(line.points.first().copied());
        };
        let length = line.lengths[segment + 1] - line.lengths[segment];
        let t = if length <= f64::EPSILON {
            0.0
        } else {
            (target - line.lengths[segment]) / length
        };
        Ok(Some([
            start[0] + t * (end[0] - start[0]),
            start[1] + t * (end[1] - start[1]),
        ]))
    }

    /// Points within `radius_m`, scanning H3 rings outward from the query
    /// cell. With a `limit`, the scan stops as soon as the `limit` closest hits
    /// are certain: once the disk scanned so far covers a radius beyond the
    /// `limit`-th distance, no unscanned point can be closer. Dense areas (a
    /// city centre full of postcodes) then cost a few rings instead of the
    /// whole radius.
    #[allow(clippy::too_many_arguments)]
    fn collect_points(
        &self,
        index: &CellTable,
        resolution: Resolution,
        lon: f64,
        lat: f64,
        radius_m: f64,
        limit: usize,
        accepts: impl Fn(RecordId, Layer) -> Result<bool>,
    ) -> Result<Vec<PointCandidate>> {
        let Ok(lat_lng) = LatLng::new(lat, lon) else {
            return Ok(Vec::new());
        };
        let max_k = disk_k(resolution, radius_m);
        let mut cells = lat_lng
            .to_cell(resolution)
            .grid_disk_distances::<Vec<(CellIndex, u32)>>(max_k);
        cells.sort_unstable_by_key(|(cell, k)| (*k, u64::from(*cell)));

        let mut candidates = Vec::new();
        let mut next = 0;
        for k in 0..=max_k {
            while let Some(&(cell, ring)) = cells.get(next)
                && ring == k
            {
                next += 1;
                let Some(refs) = index.refs(u64::from(cell))? else {
                    continue;
                };
                for record_id in refs.points {
                    let (layer, point_lon, point_lat) = self.records.point(record_id)?;
                    let distance_m = haversine_m(lon, lat, point_lon, point_lat);
                    if distance_m <= radius_m && accepts(record_id, layer)? {
                        candidates.push(PointCandidate {
                            record_id,
                            layer,
                            lon: point_lon,
                            lat: point_lat,
                            distance_m,
                        });
                    }
                }
            }
            if limit > 0 && candidates.len() >= limit && k < max_k {
                // `disk_k` scans `ceil(r / edge) + H3_RADIUS_EXTRA_RING` rings to
                // cover radius r, so the rings scanned so far cover this radius.
                let covered_m =
                    f64::from(k.saturating_sub(H3_RADIUS_EXTRA_RING)) * resolution.edge_length_m();
                let mut distances = candidates
                    .iter()
                    .map(|candidate| candidate.distance_m)
                    .collect::<Vec<_>>();
                let (_, kth, _) = distances.select_nth_unstable_by(limit - 1, f64::total_cmp);
                if *kth < covered_m {
                    break;
                }
            }
        }
        Ok(candidates)
    }
}

fn line_cache(points: Vec<[i32; 2]>) -> LineCache {
    let points = points
        .into_iter()
        .map(|[lon, lat]| [dequantize(lon), dequantize(lat)])
        .collect::<Vec<_>>();
    let mut lengths = Vec::with_capacity(points.len());
    let mut total = 0.0;
    lengths.push(0.0);
    for pair in points.windows(2) {
        total += haversine_m(pair[0][0], pair[0][1], pair[1][0], pair[1][1]);
        lengths.push(total);
    }
    LineCache { points, lengths }
}

fn cell_id(lon: f64, lat: f64, resolution: Resolution) -> Result<u64> {
    let lat_lng = LatLng::new(lat, lon).context("invalid coordinate for h3 cell")?;
    Ok(u64::from(lat_lng.to_cell(resolution)))
}

/// Rings around the query cell that cover `radius_m`.
fn disk_k(resolution: Resolution, radius_m: f64) -> u32 {
    if radius_m <= 0.0 {
        H3_RADIUS_EXTRA_RING
    } else {
        ((radius_m / resolution.edge_length_m()).ceil() as u32)
            .saturating_add(H3_RADIUS_EXTRA_RING)
            .min(H3_MAX_QUERY_K)
    }
}

fn query_cells(lon: f64, lat: f64, resolution: Resolution, radius_m: f64) -> Vec<u64> {
    let Ok(lat_lng) = LatLng::new(lat, lon) else {
        return Vec::new();
    };
    let mut cells = lat_lng
        .to_cell(resolution)
        .grid_disk::<Vec<CellIndex>>(disk_k(resolution, radius_m))
        .into_iter()
        .map(u64::from)
        .collect::<Vec<_>>();
    // Ascending cell ids read the index front to back.
    cells.sort_unstable();
    cells
}

fn segment_cells(start: [f64; 2], end: [f64; 2], resolution: Resolution) -> Result<Vec<u64>> {
    let length = haversine_m(start[0], start[1], end[0], end[1]);
    let step = (resolution.edge_length_m() / H3_SEGMENT_SAMPLE_DIVISOR).max(1.0);
    let sample_count = ((length / step).ceil() as usize).max(1);
    let mut cells = Vec::with_capacity(sample_count + 1);
    for sample in 0..=sample_count {
        let t = sample as f64 / sample_count as f64;
        let lon = start[0] + t * (end[0] - start[0]);
        let lat = start[1] + t * (end[1] - start[1]);
        cells.push(cell_id(lon, lat, resolution)?);
    }
    cells.sort_unstable();
    cells.dedup();
    Ok(cells)
}

struct SegmentProjection {
    lon: f64,
    lat: f64,
    distance_m: f64,
    t: f64,
}

fn project_to_segment_m(lon: f64, lat: f64, start: [f64; 2], end: [f64; 2]) -> SegmentProjection {
    let (sx, sy) = local_xy_m(start[0], start[1], lon, lat);
    let (ex, ey) = local_xy_m(end[0], end[1], lon, lat);
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
        lon: start[0] + t * (end[0] - start[0]),
        lat: start[1] + t * (end[1] - start[1]),
        distance_m: (x * x + y * y).sqrt(),
        t,
    }
}

fn local_xy_m(lon: f64, lat: f64, origin_lon: f64, origin_lat: f64) -> (f64, f64) {
    let x = (lon - origin_lon).to_radians() * EARTH_RADIUS_M * origin_lat.to_radians().cos();
    let y = (lat - origin_lat).to_radians() * EARTH_RADIUS_M;
    (x, y)
}

pub(crate) fn haversine_m(a_lon: f64, a_lat: f64, b_lon: f64, b_lat: f64) -> f64 {
    let d_lat = (b_lat - a_lat).to_radians();
    let d_lon = (b_lon - a_lon).to_radians();
    let a_lat = a_lat.to_radians();
    let b_lat = b_lat.to_radians();
    let sin_d_lat = (d_lat / 2.0).sin();
    let sin_d_lon = (d_lon / 2.0).sin();
    let h = sin_d_lat * sin_d_lat + a_lat.cos() * b_lat.cos() * sin_d_lon * sin_d_lon;
    2.0 * EARTH_RADIUS_M * h.sqrt().asin()
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

fn compare_distance<T: CandidateDistance>(left: &T, right: &T) -> Ordering {
    left.distance_m()
        .partial_cmp(&right.distance_m())
        .unwrap_or(Ordering::Equal)
}

fn closest_candidates<T: CandidateDistance>(mut candidates: Vec<T>, limit: usize) -> Vec<T> {
    if limit > 0 && candidates.len() > limit {
        // Select indices so equal-distance candidates retain their original order,
        // including ties at the cutoff. Only the retained candidates are sorted.
        let mut indices: Vec<usize> = (0..candidates.len()).collect();
        indices.select_nth_unstable_by(limit - 1, |&left, &right| {
            compare_distance(&candidates[left], &candidates[right]).then_with(|| left.cmp(&right))
        });
        let cutoff_index = indices[limit - 1];
        let cutoff_distance = candidates[cutoff_index].distance_m();
        let mut index = 0;
        candidates.retain(|candidate| {
            let keep = candidate
                .distance_m()
                .partial_cmp(&cutoff_distance)
                .unwrap_or(Ordering::Equal)
                .then_with(|| index.cmp(&cutoff_index))
                != Ordering::Greater;
            index += 1;
            keep
        });
    }
    candidates.sort_by(compare_distance);
    candidates
}

#[cfg(test)]
mod tests {
    use geojson::Geometry;

    use super::*;
    use crate::{
        record::{
            AddressComponents, AddressRecord, DerivedSourceProvenance, LocationPrecision,
            OsmObjectType, PostcodeRecord, SourceProvenance, StreetRecord, point_geometry,
        },
        records::RecordsWriter,
    };

    #[test]
    fn closest_candidates_matches_stable_full_sort() {
        for count in [0, 1, 8, 1_000] {
            let candidates: Vec<_> = (0..count)
                .map(|id| PointCandidate {
                    record_id: id,
                    layer: Layer::Address,
                    lon: 0.0,
                    lat: 0.0,
                    // Repeated, unsorted distances exercise ties at the cutoff.
                    distance_m: ((id * 37) % 23) as f64,
                })
                .collect();
            for limit in [0, 1, 3, 5, 8, 999, 1_000, 1_001] {
                let mut expected = candidates.clone();
                expected.sort_by(compare_distance);
                if limit > 0 {
                    expected.truncate(limit);
                }
                let actual = closest_candidates(candidates.clone(), limit);
                assert_eq!(
                    actual.iter().map(|hit| hit.record_id).collect::<Vec<_>>(),
                    expected.iter().map(|hit| hit.record_id).collect::<Vec<_>>(),
                    "count={count}, limit={limit}",
                );
            }
        }
    }

    fn address(lon: f64, lat: f64, object_id: i64) -> Record {
        Record::Address(AddressRecord {
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
            geometry: point_geometry(lon, lat),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(OsmObjectType::Node, object_id),
        })
    }

    /// Build a Pack holding only records and the spatial index, with a sorter
    /// budget small enough to force spilling.
    fn build(records: &[Record]) -> (SpatialIndexReader, SpatialCommit) {
        let root =
            std::env::temp_dir().join(format!("open-geocode-spatial-{}", uuid::Uuid::new_v4()));
        let scratch = Scratch::create(root.join("scratch")).expect("scratch");
        let mut writer =
            RecordsWriter::create(&scratch, &MemoryBudget::unlimited()).expect("records");
        let mut spatial = SpatialIndexWriter::new(&scratch, &MemoryBudget::unlimited(), 64);
        for record in records {
            let id = writer.write(record, None).expect("write");
            spatial
                .add(RecordCells::for_record(id, record).expect("cells"))
                .expect("add");
        }
        let path = root.join("pack.ogp");
        let mut pack = ContainerWriter::create(&path).expect("pack");
        writer.finish(&mut pack).expect("records finish");
        let commit = spatial.finish(&mut pack).expect("spatial finish");
        pack.finish().expect("pack finish");
        let container = Container::open(&path).expect("open");
        let records = RecordsReader::open(&container).expect("records reader");
        (
            SpatialIndexReader::open(&container, records).expect("spatial reader"),
            commit,
        )
    }

    #[test]
    fn indexes_and_queries_address_points() {
        let mut records = vec![address(-79.0, 43.0, 7)];
        for id in 8..1_008 {
            records.push(address(
                -79.0,
                43.0 + ((id * 37) % 100) as f64 * 0.000001,
                id,
            ));
        }
        let (reader, commit) = build(&records);
        assert!(commit.fine_pairs.runs > 0, "the tiny budget must spill");
        assert_eq!(commit.point_count, 1_001);

        let hits = reader
            .point_candidates(-79.0, 43.0, Layer::Address, 5.0, 1)
            .expect("hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, 0);

        let all = reader
            .point_candidates(-79.0, 43.0, Layer::Address, 100.0, 0)
            .expect("all");
        let limited = reader
            .point_candidates(-79.0, 43.0, Layer::Address, 100.0, 5)
            .expect("limited");
        assert_eq!(all.len(), 1_001);
        assert_eq!(
            limited.iter().map(|hit| hit.record_id).collect::<Vec<_>>(),
            all.iter()
                .take(5)
                .map(|hit| hit.record_id)
                .collect::<Vec<_>>(),
        );
        assert!(
            reader
                .point_candidates(-79.0, 43.0, Layer::Postcode, 100.0, 0)
                .expect("other layer")
                .is_empty()
        );
    }

    #[test]
    fn early_stopping_ring_scan_matches_the_full_scan() {
        // Postcodes scattered over ~60 km, denser near the centre.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % 1_000_000) as f64 / 1_000_000.0 - 0.5
        };
        let records = (0..2_000)
            .map(|index| {
                let spread = if index % 3 == 0 { 0.6 } else { 0.05 };
                Record::Postcode(PostcodeRecord {
                    postcode: format!("P{index}"),
                    geometry: point_geometry(-79.4 + next() * spread, 43.7 + next() * spread),
                    source: DerivedSourceProvenance::osm_address_records(1),
                })
            })
            .collect::<Vec<_>>();
        let (reader, _) = build(&records);
        for (lon, lat) in [(-79.4, 43.7), (-79.6, 43.9), (-79.1, 43.5), (-78.0, 43.0)] {
            for limit in [1, 16, 100] {
                let early = reader
                    .context_candidates(lon, lat, 50_000.0, limit)
                    .expect("early");
                let mut full = reader
                    .context_candidates(lon, lat, 50_000.0, 0)
                    .expect("full");
                full.truncate(limit);
                let distances = |hits: &[PointCandidate]| {
                    hits.iter().map(|hit| hit.distance_m).collect::<Vec<_>>()
                };
                assert_eq!(
                    distances(&early),
                    distances(&full),
                    "{lon},{lat} limit {limit}"
                );
            }
        }
    }

    #[test]
    fn indexes_context_points_with_coarse_cells() {
        let (reader, _) = build(&[Record::Postcode(PostcodeRecord {
            postcode: "M5V".to_string(),
            geometry: point_geometry(-79.4, 43.6),
            source: DerivedSourceProvenance::osm_address_records(1),
        })]);
        let hits = reader
            .context_candidates(-79.39, 43.6, 5_000.0, 5)
            .expect("hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, 0);
        assert_eq!(hits[0].layer, Layer::Postcode);
    }

    #[test]
    fn indexes_line_segments_with_fraction() {
        let street = |object_id: i64, lon: f64| {
            Record::Street(StreetRecord {
                name: "King Street".to_string(),
                geometry: Geometry::new(GeometryValue::LineString {
                    coordinates: vec![
                        vec![lon, 43.0].into(),
                        vec![lon, 43.0005].into(),
                        vec![lon, 43.001].into(),
                    ],
                }),
                representative_point: [lon, 43.0005],
                source: SourceProvenance::osm(OsmObjectType::Way, object_id),
            })
        };
        let (reader, commit) = build(&[street(1, -79.0), street(2, -78.99)]);
        assert_eq!(commit.segment_count, 4);
        let hits = reader
            .segment_candidates(-79.00001, 43.00075, Layer::Street, 5.0, 1)
            .expect("hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, 0);
        assert!(
            (hits[0].fraction - 0.75).abs() < 0.01,
            "{}",
            hits[0].fraction
        );
        assert!(
            reader
                .segment_candidates(-79.00001, 43.00075, Layer::Interpolation, 5.0, 1)
                .expect("other layer")
                .is_empty()
        );
    }
}
