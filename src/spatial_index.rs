//! Spatial index over the record store.
//!
//! Three packed R-trees, each built bottom up from items already in Hilbert
//! order, as Flatbush packs them: a node's children are the next 16 nodes of
//! the level below, so the tree stores boxes and no pointers. The trees hold
//! no coordinates of their own beyond the boxes; points and lines are read
//! from the record store.
//!
//! - `spatial/blocks`: one box per record block, around the block's point
//!   records. A query decodes the blocks near it whole, which takes
//!   microseconds for 64 records, and a box per block costs a fraction of a
//!   byte per record.
//! - `spatial/lines`: one box per street or interpolation line, with its
//!   record id. Lines get boxes of their own because one long road would
//!   stretch a block's box over every query along it.
//! - `spatial/context`: one box per place and postcode point, with its record
//!   id, so wide-radius admin lookups pass over the address blocks.
//!
//! ```text
//! spatial/<name>  item_count u64 | id width u8 (0 = the item's index, 4 or 8)
//!                 | boxes: the items in order, then each level above, root last;
//!                   min_lon i32, min_lat i32, max_lon i32, max_lat i32
//!                 | ids, when stored
//! ```

use std::{
    cmp::{Ordering, Reverse},
    collections::BinaryHeap,
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::PathBuf,
    sync::Arc,
};

use crate::error::{Error, Result};
use geojson::{Geometry, GeometryValue};
use serde::Serialize;

use crate::{
    container::{self, Bytes, Container},
    extsort::Scratch,
    pack::RecordId,
    record::{Layer, Record},
    records::{self, BLOCK_RECORDS, dequantize, quantize},
    util::codec::read_u64_le,
};

pub const SPATIAL_VERSION: u32 = 3;
const BLOCKS: &str = "spatial/blocks";
const LINES: &str = "spatial/lines";
const CONTEXT: &str = "spatial/context";

const NODE_SIZE: usize = 16;
const BOX_BYTES: usize = 16;
const HEADER_BYTES: usize = 9;
const EARTH_RADIUS_M: f64 = 6_371_008.8;
/// Box distances are flat-earth estimates. Scaled down by this they stay
/// below the haversine distance of every point inside the box for the radii
/// queries use, so no candidate is skipped.
const LOWER_BOUND_SLACK: f64 = 0.99;

type BoundingBox = [i32; 4];
const EMPTY: BoundingBox = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];

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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Commit {
    pub point_count: u64,
    pub line_count: u64,
    pub block_count: u64,
    pub context_point_count: u64,
}

fn union(left: BoundingBox, right: BoundingBox) -> BoundingBox {
    [
        left[0].min(right[0]),
        left[1].min(right[1]),
        left[2].max(right[2]),
        left[3].max(right[3]),
    ]
}

/// Leaves stream to scratch as they come; only the levels above them, a
/// sixteenth of the items and less, are held in memory.
#[derive(Debug)]
struct TreeWriter {
    leaves: BufWriter<File>,
    leaves_path: PathBuf,
    ids: Option<(BufWriter<File>, PathBuf)>,
    count: u64,
    widest_id: u64,
    parents: Vec<BoundingBox>,
    open: BoundingBox,
}

impl TreeWriter {
    fn create(scratch: &Arc<Scratch>, name: &str, with_ids: bool) -> Result<Self> {
        let create = |suffix: &str| -> Result<(BufWriter<File>, PathBuf)> {
            let path = scratch.path().join(format!("spatial-{name}-{suffix}.bin"));
            let file = File::create(&path).map_err(|source| Error::IoPath {
                action: "failed to create",
                path: path.clone(),
                source,
            })?;
            Ok((BufWriter::with_capacity(1 << 20, file), path))
        };
        let (leaves, leaves_path) = create("leaves")?;
        Ok(Self {
            leaves,
            leaves_path,
            ids: with_ids.then(|| create("ids")).transpose()?,
            count: 0,
            widest_id: 0,
            parents: Vec::new(),
            open: EMPTY,
        })
    }

    fn push(&mut self, bbox: BoundingBox, id: u64) -> Result<()> {
        for value in bbox {
            self.leaves.write_all(&value.to_le_bytes())?;
        }
        if let Some((ids, _)) = &mut self.ids {
            ids.write_all(&id.to_le_bytes())?;
            self.widest_id = self.widest_id.max(id);
        }
        self.open = union(self.open, bbox);
        self.count += 1;
        if self.count.is_multiple_of(NODE_SIZE as u64) {
            self.parents.push(std::mem::replace(&mut self.open, EMPTY));
        }
        Ok(())
    }

    fn finish(mut self, pack: &mut container::Writer, name: &str) -> Result<u64> {
        if !self.count.is_multiple_of(NODE_SIZE as u64) {
            self.parents.push(self.open);
        }
        let mut levels = Vec::new();
        if !self.parents.is_empty() {
            levels.push(std::mem::take(&mut self.parents));
            while levels.last().is_some_and(|level| level.len() > 1) {
                let below = levels.last().expect("a level");
                let above = below
                    .chunks(NODE_SIZE)
                    .map(|children| children.iter().copied().fold(EMPTY, union))
                    .collect();
                levels.push(above);
            }
        }
        let id_width = match &self.ids {
            None => 0u8,
            Some(_) if self.widest_id <= u64::from(u32::MAX) => 4,
            Some(_) => 8,
        };

        self.leaves.flush()?;
        pack.begin(name, SPATIAL_VERSION)?;
        pack.write_all(&self.count.to_le_bytes())?;
        pack.write_all(&[id_width])?;
        std::io::copy(&mut File::open(&self.leaves_path)?, pack)?;
        for level in &levels {
            for bbox in level {
                for value in bbox {
                    pack.write_all(&value.to_le_bytes())?;
                }
            }
        }
        if let Some((mut ids, path)) = self.ids {
            ids.flush()?;
            drop(ids);
            let mut input = BufReader::new(File::open(&path)?);
            let mut id = [0; 8];
            for _ in 0..self.count {
                input.read_exact(&mut id)?;
                pack.write_all(&id[..usize::from(id_width)])?;
            }
        }
        pack.end()?;
        Ok(self.count)
    }
}

/// Boxes for the spatial index, taken from records as they are written, in
/// record id order.
#[derive(Debug)]
pub struct Writer {
    blocks: TreeWriter,
    lines: TreeWriter,
    context: TreeWriter,
    /// The block whose box is open, and the box.
    block: u64,
    block_box: BoundingBox,
    record_count: u64,
    points: u64,
}

impl Writer {
    pub fn new(scratch: &Arc<Scratch>) -> Result<Self> {
        Ok(Self {
            blocks: TreeWriter::create(scratch, "blocks", false)?,
            lines: TreeWriter::create(scratch, "lines", true)?,
            context: TreeWriter::create(scratch, "context", true)?,
            block: 0,
            block_box: EMPTY,
            record_count: 0,
            points: 0,
        })
    }

    /// Index the record written at `record_id`. Records come in id order.
    pub fn add(&mut self, record_id: RecordId, record: &Record) -> Result<()> {
        if record_id < self.record_count {
            return Err(Error::Inconsistent(format!(
                "record {record_id} was indexed after record {}",
                self.record_count - 1
            )));
        }
        self.close_blocks_before(record_id / BLOCK_RECORDS)?;
        self.record_count = record_id + 1;
        match record {
            Record::Street(_) | Record::Interpolation(_) => {
                if let Some(bbox) = line_box(record.geometry())? {
                    self.lines.push(bbox, record_id)?;
                }
            }
            _ => {
                let Some([lon, lat]) = record.display_point() else {
                    return Ok(());
                };
                let (lon, lat) = (quantize(lon)?, quantize(lat)?);
                let bbox = [lon, lat, lon, lat];
                self.block_box = union(self.block_box, bbox);
                if record.layer().is_context() {
                    self.context.push(bbox, record_id)?;
                }
                self.points += 1;
            }
        }
        Ok(())
    }

    fn close_blocks_before(&mut self, block: u64) -> Result<()> {
        while self.block < block {
            self.blocks.push(self.block_box, self.block)?;
            self.block_box = EMPTY;
            self.block += 1;
        }
        Ok(())
    }

    pub fn finish(mut self, pack: &mut container::Writer) -> Result<Commit> {
        self.close_blocks_before(self.record_count.div_ceil(BLOCK_RECORDS))?;
        Ok(Commit {
            point_count: self.points,
            block_count: self.blocks.finish(pack, BLOCKS)?,
            line_count: self.lines.finish(pack, LINES)?,
            context_point_count: self.context.finish(pack, CONTEXT)?,
        })
    }
}

fn line_box(geometry: &Geometry) -> Result<Option<BoundingBox>> {
    let GeometryValue::LineString { coordinates } = &geometry.value else {
        return Ok(None);
    };
    let mut bbox = EMPTY;
    for position in coordinates {
        if let [lon, lat, ..] = position.as_slice()
            && lon.is_finite()
            && lat.is_finite()
        {
            let (lon, lat) = (quantize(*lon)?, quantize(*lat)?);
            bbox = union(bbox, [lon, lat, lon, lat]);
        }
    }
    Ok((bbox[0] <= bbox[2]).then_some(bbox))
}

#[derive(Clone)]
struct Tree {
    bytes: Bytes,
    count: usize,
    id_width: usize,
    /// First node and node count of each level, leaves first.
    levels: Vec<(usize, usize)>,
    ids_start: usize,
}

impl Tree {
    fn open(container: &Container, name: &str) -> Result<Self> {
        let bytes = container.section(name, SPATIAL_VERSION)?;
        let inconsistent = || Error::Inconsistent(format!("spatial index {name} is inconsistent"));
        let count = usize::try_from(read_u64_le(&bytes, 0).ok_or_else(inconsistent)?)?;
        let id_width = usize::from(*bytes.get(8).ok_or_else(inconsistent)?);
        if ![0, 4, 8].contains(&id_width) {
            return Err(inconsistent());
        }
        let mut levels = Vec::new();
        let mut nodes = 0;
        if count > 0 {
            let mut len = count;
            loop {
                levels.push((nodes, len));
                nodes += len;
                if levels.len() > 1 && len == 1 {
                    break;
                }
                len = len.div_ceil(NODE_SIZE);
            }
        }
        let ids_start = HEADER_BYTES + nodes * BOX_BYTES;
        if bytes.len() != ids_start + count * id_width {
            return Err(inconsistent());
        }
        Ok(Self {
            bytes,
            count,
            id_width,
            levels,
            ids_start,
        })
    }

    fn bbox(&self, node: usize) -> BoundingBox {
        let at = HEADER_BYTES + node * BOX_BYTES;
        std::array::from_fn(|index| {
            let start = at + index * 4;
            i32::from_le_bytes(
                self.bytes[start..start + 4]
                    .try_into()
                    .expect("validated tree"),
            )
        })
    }

    fn id(&self, item: usize) -> u64 {
        if self.id_width == 0 {
            return item as u64;
        }
        let at = self.ids_start + item * self.id_width;
        let mut id = [0; 8];
        id[..self.id_width].copy_from_slice(&self.bytes[at..at + self.id_width]);
        u64::from_le_bytes(id)
    }

    /// Items whose boxes lie within `radius_m` of the point, nearest box
    /// first, each with a lower bound on its distance.
    fn nearest(&self, lon: f64, lat: f64, radius_m: f64) -> Nearest<'_> {
        let mut pending = BinaryHeap::new();
        if let Some(&(root, _)) = self.levels.last() {
            pending.push(Reverse(Pending {
                distance_m: lower_bound_m(&self.bbox(root), lon, lat),
                level: self.levels.len() - 1,
                index: 0,
            }));
        }
        Nearest {
            tree: self,
            lon,
            lat,
            radius_m,
            pending,
        }
    }
}

struct Nearest<'a> {
    tree: &'a Tree,
    lon: f64,
    lat: f64,
    radius_m: f64,
    pending: BinaryHeap<Reverse<Pending>>,
}

/// A node not yet visited, ordered by its lower bound.
struct Pending {
    distance_m: f64,
    level: usize,
    index: usize,
}

impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Pending {}

impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Pending {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance_m
            .total_cmp(&other.distance_m)
            .then_with(|| self.level.cmp(&other.level))
            .then_with(|| self.index.cmp(&other.index))
    }
}

impl Iterator for Nearest<'_> {
    /// An item's index and the lower bound on its distance.
    type Item = (usize, f64);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(Reverse(node)) = self.pending.pop() {
            if node.distance_m > self.radius_m {
                self.pending.clear();
                return None;
            }
            if node.level == 0 {
                return Some((node.index, node.distance_m));
            }
            let (first, len) = self.tree.levels[node.level - 1];
            let children = node.index * NODE_SIZE..((node.index + 1) * NODE_SIZE).min(len);
            for child in children {
                let distance_m = lower_bound_m(&self.tree.bbox(first + child), self.lon, self.lat);
                if distance_m <= self.radius_m {
                    self.pending.push(Reverse(Pending {
                        distance_m,
                        level: node.level - 1,
                        index: child,
                    }));
                }
            }
        }
        None
    }
}

/// A lower bound on the distance from a point to anything in a box, in
/// metres; infinite for an empty box. Longitude wraps, so a box across the
/// antimeridian from the point is near it.
fn lower_bound_m(bbox: &BoundingBox, lon: f64, lat: f64) -> f64 {
    if bbox[0] > bbox[2] {
        return f64::INFINITY;
    }
    let [min_lon, min_lat, max_lon, max_lat] = bbox.map(dequantize);
    let d_lat = (min_lat - lat).max(lat - max_lat).max(0.0);
    let d_lon = if (min_lon..=max_lon).contains(&lon) {
        0.0
    } else {
        (min_lon - lon)
            .rem_euclid(360.0)
            .min((lon - max_lon).rem_euclid(360.0))
    };
    // A degree of longitude is shortest at the latitude nearest a pole.
    let poleward = lat.abs().max(min_lat.abs()).max(max_lat.abs()).min(90.0);
    let x = d_lon.to_radians() * poleward.to_radians().cos();
    let y = d_lat.to_radians();
    EARTH_RADIUS_M * x.hypot(y) * LOWER_BOUND_SLACK
}

/// The `limit` smallest distances seen, to stop a nearest-first scan once no
/// unread box can hold anything closer.
struct Closest {
    limit: usize,
    distances: BinaryHeap<OrderedDistance>,
}

#[derive(PartialEq)]
struct OrderedDistance(f64);

impl Eq for OrderedDistance {}

impl PartialOrd for OrderedDistance {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedDistance {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl Closest {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            distances: BinaryHeap::new(),
        }
    }

    fn add(&mut self, distance_m: f64) {
        if self.limit == 0 {
            return;
        }
        self.distances.push(OrderedDistance(distance_m));
        if self.distances.len() > self.limit {
            self.distances.pop();
        }
    }

    /// Whether nothing at `bound` or beyond can be among the closest.
    fn settled(&self, bound: f64) -> bool {
        self.distances.len() >= self.limit
            && self.limit > 0
            && self
                .distances
                .peek()
                .is_some_and(|farthest| bound > farthest.0)
    }
}

#[derive(Clone)]
pub struct Reader {
    blocks: Tree,
    lines: Tree,
    context: Tree,
    records: records::Reader,
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Reader")
            .field("blocks", &self.blocks.count)
            .field("lines", &self.lines.count)
            .field("context", &self.context.count)
            .finish()
    }
}

struct LineCache {
    points: Vec<[f64; 2]>,
    /// Cumulative length at each vertex, in metres.
    lengths: Vec<f64>,
}

impl Reader {
    pub fn open(container: &Container, records: records::Reader) -> Result<Self> {
        let blocks = Tree::open(container, BLOCKS)?;
        if blocks.count as u64 != records.block_count() {
            return Err(Error::Inconsistent(
                "spatial index does not match the record blocks".into(),
            ));
        }
        Ok(Self {
            blocks,
            lines: Tree::open(container, LINES)?,
            context: Tree::open(container, CONTEXT)?,
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
        let candidates = self.block_points(lon, lat, radius_m, limit, |_, candidate| {
            Ok(candidate == layer)
        })?;
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
        let candidates =
            self.block_points(lon, lat, radius_m, limit, |record_id, layer| match layer {
                Layer::Address => Ok(true),
                Layer::Poi => self.records.is_explicit_address(record_id),
                _ => Ok(false),
            })?;
        Ok(closest_candidates(candidates, limit))
    }

    pub fn context_candidates(
        &self,
        lon: f64,
        lat: f64,
        radius_m: f64,
        limit: usize,
    ) -> Result<Vec<PointCandidate>> {
        let mut candidates = Vec::new();
        let mut closest = Closest::new(limit);
        for (item, bound) in self.context.nearest(lon, lat, radius_m) {
            if closest.settled(bound) {
                break;
            }
            let record_id = self.context.id(item);
            let (layer, point_lon, point_lat) = self.records.point(record_id)?;
            let distance_m = haversine_m(lon, lat, point_lon, point_lat);
            if distance_m <= radius_m && layer.is_context() {
                closest.add(distance_m);
                candidates.push(PointCandidate {
                    record_id,
                    layer,
                    lon: point_lon,
                    lat: point_lat,
                    distance_m,
                });
            }
        }
        Ok(closest_candidates(candidates, limit))
    }

    /// Whether any context point of `layer` within `radius_m` satisfies
    /// `matches`, nearest first, stopping at the first match. `None` when
    /// there is no point of that layer within the radius at all.
    pub fn any_context_point(
        &self,
        lon: f64,
        lat: f64,
        layer: Layer,
        radius_m: f64,
        mut matches: impl FnMut(RecordId) -> Result<bool>,
    ) -> Result<Option<bool>> {
        let mut seen = false;
        for (item, _) in self.context.nearest(lon, lat, radius_m) {
            let record_id = self.context.id(item);
            let (point_layer, point_lon, point_lat) = self.records.point(record_id)?;
            if point_layer != layer || haversine_m(lon, lat, point_lon, point_lat) > radius_m {
                continue;
            }
            seen = true;
            if matches(record_id)? {
                return Ok(Some(true));
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
        let mut candidates = Vec::new();
        let mut closest = Closest::new(limit);
        for (item, bound) in self.lines.nearest(lon, lat, radius_m) {
            if closest.settled(bound) {
                break;
            }
            let record_id = self.lines.id(item);
            if self.records.header(record_id)?.layer != layer {
                continue;
            }
            let Some(line) = self.records.line(record_id)?.map(line_cache) else {
                continue;
            };
            let total = *line.lengths.last().expect("line has vertices");
            for (segment, pair) in line.points.windows(2).enumerate() {
                if haversine_m(pair[0][0], pair[0][1], pair[1][0], pair[1][1]) <= f64::EPSILON {
                    continue;
                }
                let projection = project_to_segment_m(lon, lat, pair[0], pair[1]);
                if projection.distance_m <= radius_m {
                    let start_fraction = line.lengths[segment] / total;
                    let end_fraction = line.lengths[segment + 1] / total;
                    closest.add(projection.distance_m);
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

    /// Point records within `radius_m` that `accepts` keeps, reading the
    /// blocks nearest first. With a `limit`, the scan stops once no unread
    /// block can hold a point closer than the `limit` closest so far: a dense
    /// city centre costs the few blocks around the query.
    fn block_points(
        &self,
        lon: f64,
        lat: f64,
        radius_m: f64,
        limit: usize,
        accepts: impl Fn(RecordId, Layer) -> Result<bool>,
    ) -> Result<Vec<PointCandidate>> {
        let mut candidates = Vec::new();
        let mut closest = Closest::new(limit);
        let window = DegreeWindow::around(lat, radius_m);
        for (block, bound) in self.blocks.nearest(lon, lat, radius_m) {
            if closest.settled(bound) {
                break;
            }
            self.records.block_points(
                block as u64,
                |record_id, layer, point_lon, point_lat, line| {
                    if line {
                        return Ok(());
                    }
                    let (point_lon, point_lat) = (dequantize(point_lon), dequantize(point_lat));
                    if !window.holds(lon, lat, point_lon, point_lat) {
                        return Ok(());
                    }
                    let distance_m = haversine_m(lon, lat, point_lon, point_lat);
                    if distance_m <= radius_m && accepts(record_id, layer)? {
                        closest.add(distance_m);
                        candidates.push(PointCandidate {
                            record_id,
                            layer,
                            lon: point_lon,
                            lat: point_lat,
                            distance_m,
                        });
                    }
                    Ok(())
                },
            )?;
        }
        Ok(candidates)
    }
}

/// The degrees of latitude and longitude a radius spans around a point, a
/// little wide: points outside are farther than the radius, so a scan passes
/// over them without the trigonometry of a haversine distance.
struct DegreeWindow {
    lat: f64,
    lon: f64,
}

impl DegreeWindow {
    fn around(lat: f64, radius_m: f64) -> Self {
        let lat_degrees = (radius_m / EARTH_RADIUS_M).to_degrees() / LOWER_BOUND_SLACK;
        // Within a degree of a pole every longitude is near: no window.
        let cos = (lat.abs() + lat_degrees).min(90.0).to_radians().cos();
        Self {
            lat: lat_degrees,
            lon: if cos > 0.02 {
                lat_degrees / cos
            } else {
                f64::INFINITY
            },
        }
    }

    fn holds(&self, lon: f64, lat: f64, point_lon: f64, point_lat: f64) -> bool {
        let d_lon = (point_lon - lon).abs();
        (point_lat - lat).abs() <= self.lat && d_lon.min(360.0 - d_lon) <= self.lon
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

    use super::*;
    use crate::{
        record::{
            AddressComponents, AddressRecord, DerivedSourceProvenance, LocationPrecision,
            ObjectType, PostcodeRecord, SourceProvenance, StreetRecord, point_geometry,
        },
        records,
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
            street_postcodes: Vec::new(),
            address: AddressComponents {
                areas: Vec::new(),
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
            source: SourceProvenance::osm(ObjectType::Node, object_id),
        })
    }

    /// Build a Pack holding only records and the spatial index.
    fn build(records: &[Record]) -> (Reader, Commit) {
        let root =
            std::env::temp_dir().join(format!("open-geocode-spatial-{}", uuid::Uuid::new_v4()));
        let scratch = Scratch::create(root.join("scratch")).expect("scratch");
        let mut writer = records::Writer::create(&scratch, &crate::memory::Budget::unlimited())
            .expect("records");
        let mut spatial = Writer::new(&scratch).expect("spatial");
        for record in records {
            let id = writer.write(record, None).expect("write");
            spatial.add(id, record).expect("add");
        }
        let path = root.join("pack.ogp");
        let mut pack = container::Writer::create(&path).expect("pack");
        writer.finish(&mut pack).expect("records finish");
        let commit = spatial.finish(&mut pack).expect("spatial finish");
        pack.finish().expect("pack finish");
        let container = Container::open(&path).expect("open");
        let records = records::Reader::open(&container).expect("records reader");
        (
            Reader::open(&container, records).expect("spatial reader"),
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
        assert_eq!(commit.point_count, 1_001);
        assert_eq!(commit.block_count, 1_001_u64.div_ceil(BLOCK_RECORDS));

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

    fn scattered(count: usize) -> Vec<[f64; 2]> {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % 1_000_000) as f64 / 1_000_000.0 - 0.5
        };
        (0..count)
            .map(|index| {
                let spread = if index % 3 == 0 { 0.6 } else { 0.05 };
                [-79.4 + next() * spread, 43.7 + next() * spread]
            })
            .collect()
    }

    #[test]
    fn early_stopping_scan_matches_the_full_scan() {
        // Points scattered over ~60 km, denser near the centre.
        let points = scattered(5_000);
        let postcodes = points
            .iter()
            .enumerate()
            .map(|(index, [lon, lat])| {
                Record::Postcode(PostcodeRecord {
                    postcode: format!("P{index}"),
                    country: None,
                    geometry: point_geometry(*lon, *lat),
                    source: DerivedSourceProvenance::osm_address_records(1),
                })
            })
            .collect::<Vec<_>>();
        let addresses = points
            .iter()
            .enumerate()
            .map(|(index, [lon, lat])| address(*lon, *lat, index as i64))
            .collect::<Vec<_>>();
        let (postcodes, _) = build(&postcodes);
        let (addresses, _) = build(&addresses);
        let distances =
            |hits: &[PointCandidate]| hits.iter().map(|hit| hit.distance_m).collect::<Vec<_>>();
        for (lon, lat) in [(-79.4, 43.7), (-79.6, 43.9), (-79.1, 43.5), (-78.0, 43.0)] {
            for limit in [1, 16, 100] {
                let early = postcodes
                    .context_candidates(lon, lat, 50_000.0, limit)
                    .expect("early");
                let mut full = postcodes
                    .context_candidates(lon, lat, 50_000.0, 0)
                    .expect("full");
                full.truncate(limit);
                assert_eq!(
                    distances(&early),
                    distances(&full),
                    "{lon},{lat} limit {limit}"
                );

                let early = addresses
                    .point_candidates(lon, lat, Layer::Address, 50_000.0, limit)
                    .expect("early");
                let mut full = addresses
                    .point_candidates(lon, lat, Layer::Address, 50_000.0, 0)
                    .expect("full");
                full.truncate(limit);
                assert_eq!(
                    distances(&early),
                    distances(&full),
                    "{lon},{lat} limit {limit}"
                );
            }
        }
    }

    #[test]
    fn finds_points_across_the_antimeridian() {
        let (reader, _) = build(&[address(179.99995, -44.0, 1), address(-179.99995, -44.0, 2)]);
        let hits = reader
            .point_candidates(-179.99999, -44.0, Layer::Address, 50.0, 0)
            .expect("hits");
        assert_eq!(
            hits.iter().map(|hit| hit.record_id).collect::<Vec<_>>(),
            vec![1, 0]
        );
    }

    #[test]
    fn indexes_context_points_apart_from_addresses() {
        let (reader, commit) = build(&[
            address(-79.4, 43.6, 1),
            Record::Postcode(PostcodeRecord {
                postcode: "M5V".to_string(),
                country: None,
                geometry: point_geometry(-79.4, 43.6),
                source: DerivedSourceProvenance::osm_address_records(1),
            }),
        ]);
        assert_eq!(commit.context_point_count, 1);
        let hits = reader
            .context_candidates(-79.39, 43.6, 5_000.0, 5)
            .expect("hits");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record_id, 1);
        assert_eq!(hits[0].layer, Layer::Postcode);
    }

    #[test]
    fn indexes_line_segments_with_fraction() {
        let street = |object_id: i64, lon: f64| {
            Record::Street(StreetRecord {
                alt_names: Vec::new(),
                name: "King Street".to_string(),
                geometry: Geometry::new(GeometryValue::LineString {
                    coordinates: vec![
                        vec![lon, 43.0].into(),
                        vec![lon, 43.0005].into(),
                        vec![lon, 43.001].into(),
                    ],
                }),
                representative_point: [lon, 43.0005],
                source: SourceProvenance::osm(ObjectType::Way, object_id),
            })
        };
        let (reader, commit) = build(&[street(1, -79.0), street(2, -78.99)]);
        assert_eq!(commit.line_count, 2);
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
                .point_candidates(-79.0, 43.0005, Layer::Street, 5.0, 1)
                .expect("lines are not points")
                .is_empty()
        );
        assert!(
            reader
                .segment_candidates(-79.00001, 43.00075, Layer::Interpolation, 5.0, 1)
                .expect("other layer")
                .is_empty()
        );
    }
}
