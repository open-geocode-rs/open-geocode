//! Point-in-area tests for areas of any size, read from a scratch file.
//!
//! The boundaries of a planet hold several times the memory a build is
//! given, so no shape stays on the heap. Each built area is written to one
//! scratch file as a block: its rings as OSM's 1e-7 fixed-point coordinates,
//! and its edges filed into horizontal bands of equal height. A test reads
//! the band its latitude falls in and casts a ray east from the point across
//! that band's edges only, so an island drawn from a coastline of a million
//! vertices costs a few dozen edge tests. The file is mapped, so the shapes
//! take page cache the system reclaims as it needs, not heap.

use std::{
    fs::File,
    io::{BufWriter, Write},
    sync::Arc,
};

use geo::MultiPolygon;
use memmap2::Mmap;

use crate::{
    error::{Error, Result},
    extsort::{Scratch, ScratchFile},
    record::Extent,
    util::geo::{box_around, narrowest_box},
};

use super::locations::degrees;

/// Edges a band holds on average when its edges are spread evenly.
const EDGES_PER_BAND: usize = 16;
/// Band entries a shape may hold per edge: an edge spanning several bands
/// is filed in each, and a shape whose long edges would fill more halves its
/// bands until they fit.
const ENTRIES_PER_EDGE: usize = 4;
/// Bytes of a block's header: four counts and the band span.
const HEADER_BYTES: usize = 32;
/// Scratch label of the boundaries' shape file.
pub(crate) const SHAPES_LABEL: &str = "boundary-shapes";

/// Where an area's block lies in the shape file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ShapeRef {
    pub offset: u64,
    pub bytes: u64,
}

/// Write the block of `geometry`: its rings, exterior first, and their edges
/// by band. Rings are read by the even-odd rule, so holes and separate parts
/// need no bookkeeping.
pub(crate) fn encode_shape(geometry: &MultiPolygon<f64>, out: &mut Vec<u8>) {
    let rings = geometry
        .iter()
        .flat_map(|polygon| std::iter::once(polygon.exterior()).chain(polygon.interiors()))
        .collect::<Vec<_>>();
    let mut coords = Vec::new();
    let mut ring_ends = Vec::with_capacity(rings.len());
    for ring in &rings {
        coords.extend(ring.0.iter().map(|coord| [fixed(coord.x), fixed(coord.y)]));
        ring_ends.push(coords.len() as u32);
    }
    encode_rings(&coords, &ring_ends, out);
}

/// [`encode_shape`] for rings already in fixed point. `coords` holds every
/// ring's vertices, its first repeated as its last, and `ring_ends` where
/// each ring stops.
pub(crate) fn encode_rings(coords: &[[i32; 2]], ring_ends: &[u32], out: &mut Vec<u8>) {
    let (lat_lo, lat_hi) = coords
        .iter()
        .map(|[_, lat]| degrees(*lat))
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), lat| {
            (lo.min(lat), hi.max(lat))
        });
    // An edge joins a vertex to the next one of its ring.
    let mut edges = Vec::new();
    let mut start = 0;
    for &end in ring_ends {
        edges.extend(start..end.saturating_sub(1));
        start = end;
    }
    let span = |edge: u32| {
        let (from, to) = (coords[edge as usize][1], coords[edge as usize + 1][1]);
        (degrees(from.min(to)), degrees(from.max(to)))
    };
    let mut bands = (edges.len() / EDGES_PER_BAND).max(1);
    let filed = loop {
        let filed = edges
            .iter()
            .map(|&edge| {
                let (low, high) = span(edge);
                (
                    band_of(low, lat_lo, lat_hi, bands),
                    band_of(high, lat_lo, lat_hi, bands),
                )
            })
            .collect::<Vec<_>>();
        let entries = filed
            .iter()
            .map(|(low, high)| high - low + 1)
            .sum::<usize>();
        if bands == 1 || entries <= edges.len() * ENTRIES_PER_EDGE {
            break filed;
        }
        bands /= 2;
    };
    let mut band_starts = vec![0u32; bands + 1];
    for (low, high) in &filed {
        for band in *low..=*high {
            band_starts[band + 1] += 1;
        }
    }
    for band in 0..bands {
        band_starts[band + 1] += band_starts[band];
    }
    let mut next = band_starts.clone();
    let mut entries = vec![0u32; band_starts[bands] as usize];
    for (&edge, (low, high)) in edges.iter().zip(&filed) {
        for band in *low..=*high {
            entries[next[band] as usize] = edge;
            next[band] += 1;
        }
    }

    out.reserve(
        HEADER_BYTES + 4 * ring_ends.len() + 8 * coords.len() + 4 * (bands + 1 + entries.len()),
    );
    for count in [coords.len(), ring_ends.len(), bands, entries.len()] {
        out.extend_from_slice(&(count as u32).to_le_bytes());
    }
    out.extend_from_slice(&lat_lo.to_le_bytes());
    out.extend_from_slice(&lat_hi.to_le_bytes());
    for end in ring_ends {
        out.extend_from_slice(&end.to_le_bytes());
    }
    for [lon, lat] in coords {
        out.extend_from_slice(&lon.to_le_bytes());
        out.extend_from_slice(&lat.to_le_bytes());
    }
    for value in band_starts.into_iter().chain(entries) {
        out.extend_from_slice(&value.to_le_bytes());
    }
}

/// A coordinate in the PBF's fixed point. The node store hands out exactly
/// these, so a vertex reads back with the bits it was given.
pub(crate) fn fixed(value: f64) -> i32 {
    (value * 1e7)
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

/// The band of `bands` over `lo..=hi` that `lat` falls in. Monotonic in
/// `lat`, so an edge filed from its lowest to its highest band is found from
/// every latitude it spans.
fn band_of(lat: f64, lo: f64, hi: f64, bands: usize) -> usize {
    if hi.partial_cmp(&lo) != Some(std::cmp::Ordering::Greater) {
        return 0;
    }
    let at = ((lat - lo) / (hi - lo) * bands as f64).floor();
    if at.is_nan() || at < 0.0 {
        0
    } else {
        (at as usize).min(bands - 1)
    }
}

/// Whether the area of `block` covers the point, its boundary included.
pub(crate) fn covers(block: &[u8], lon: f64, lat: f64) -> bool {
    let u32_at = |at: usize| u32::from_le_bytes(block[at..at + 4].try_into().expect("4 bytes"));
    let i32_at = |at: usize| i32::from_le_bytes(block[at..at + 4].try_into().expect("4 bytes"));
    let f64_at = |at: usize| f64::from_le_bytes(block[at..at + 8].try_into().expect("8 bytes"));
    let vertices = u32_at(0) as usize;
    let rings = u32_at(4) as usize;
    let bands = u32_at(8) as usize;
    let (lat_lo, lat_hi) = (f64_at(16), f64_at(24));
    if !(lat_lo..=lat_hi).contains(&lat) {
        return false;
    }
    let coords_at = HEADER_BYTES + 4 * rings;
    let starts_at = coords_at + 8 * vertices;
    let entries_at = starts_at + 4 * (bands + 1);
    let vertex = |index: usize| {
        let at = coords_at + 8 * index;
        (degrees(i32_at(at)), degrees(i32_at(at + 4)))
    };
    let band = band_of(lat, lat_lo, lat_hi, bands);
    let (first, last) = (
        u32_at(starts_at + 4 * band) as usize,
        u32_at(starts_at + 4 * (band + 1)) as usize,
    );
    let mut inside = false;
    for entry in first..last {
        let edge = u32_at(entries_at + 4 * entry) as usize;
        let ((x0, y0), (x1, y1)) = (vertex(edge), vertex(edge + 1));
        if y0 == lat && y1 == lat {
            if lon >= x0.min(x1) && lon <= x0.max(x1) {
                return true;
            }
            continue;
        }
        if (y0 > lat) == (y1 > lat) {
            if (y0 == lat && x0 == lon) || (y1 == lat && x1 == lon) {
                return true;
            }
            continue;
        }
        let x = x0 + (lat - y0) * (x1 - x0) / (y1 - y0);
        if x == lon {
            return true;
        }
        if x > lon {
            inside = !inside;
        }
    }
    inside
}

/// The box around the area of `block`: west, south, east, north.
pub(crate) fn extent(block: &[u8]) -> [f64; 4] {
    let u32_at = |at: usize| u32::from_le_bytes(block[at..at + 4].try_into().expect("4 bytes"));
    let f64_at = |at: usize| f64::from_le_bytes(block[at..at + 8].try_into().expect("8 bytes"));
    let vertices = u32_at(0) as usize;
    let coords_at = HEADER_BYTES + 4 * u32_at(4) as usize;
    let (west, east) = (0..vertices)
        .map(|index| {
            let at = coords_at + 8 * index;
            degrees(i32::from_le_bytes(
                block[at..at + 4].try_into().expect("4 bytes"),
            ))
        })
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), lon| {
            (lo.min(lon), hi.max(lon))
        });
    [west, f64_at(16), east, f64_at(24)]
}

/// A shape block's parts, found once for a test that reads many of them.
#[derive(Clone, Copy)]
struct Block<'a> {
    bytes: &'a [u8],
    rings: usize,
    bands: usize,
    lat_lo: f64,
    lat_hi: f64,
    coords_at: usize,
    starts_at: usize,
    entries_at: usize,
}

impl<'a> Block<'a> {
    fn of(bytes: &'a [u8]) -> Self {
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        let f64_at = |at: usize| f64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        let (vertices, rings, bands) = (u32_at(0) as usize, u32_at(4) as usize, u32_at(8) as usize);
        let coords_at = HEADER_BYTES + 4 * rings;
        let starts_at = coords_at + 8 * vertices;
        Self {
            bytes,
            rings,
            bands,
            lat_lo: f64_at(16),
            lat_hi: f64_at(24),
            coords_at,
            starts_at,
            entries_at: starts_at + 4 * (bands + 1),
        }
    }

    fn u32_at(&self, at: usize) -> usize {
        u32::from_le_bytes(self.bytes[at..at + 4].try_into().expect("4 bytes")) as usize
    }

    fn fixed_vertex(&self, index: usize) -> [i32; 2] {
        let at = self.coords_at + 8 * index;
        let i32_at =
            |at: usize| i32::from_le_bytes(self.bytes[at..at + 4].try_into().expect("4 bytes"));
        [i32_at(at), i32_at(at + 4)]
    }

    fn vertex(&self, index: usize) -> [f64; 2] {
        self.fixed_vertex(index).map(degrees)
    }

    /// The vertices of each ring, as the range of their indexes.
    fn rings(&self) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
        let mut start = 0;
        (0..self.rings).map(move |ring| {
            let end = self.u32_at(HEADER_BYTES + 4 * ring);
            let vertices = start..end;
            start = end;
            vertices
        })
    }

    /// The edges that reach a latitude of `lo..=hi`, each as its first
    /// vertex. One that spans several of the bands comes more than once.
    fn edges_between(&self, lo: f64, hi: f64) -> impl Iterator<Item = usize> + '_ {
        let bands = if hi < self.lat_lo || lo > self.lat_hi {
            0..0
        } else {
            band_of(lo, self.lat_lo, self.lat_hi, self.bands)
                ..band_of(hi, self.lat_lo, self.lat_hi, self.bands) + 1
        };
        let entries = self.u32_at(self.starts_at + 4 * bands.start)
            ..self.u32_at(self.starts_at + 4 * bands.end);
        entries.map(|entry| self.u32_at(self.entries_at + 4 * entry))
    }
}

/// The narrowest box around the area of `block`, which crosses the
/// antimeridian when the area does. None for a block of no ring.
pub(crate) fn narrowest_extent(block: &[u8]) -> Option<Extent> {
    let block = Block::of(block);
    narrowest_box(
        block
            .rings()
            .filter_map(|ring| box_around(ring.map(|index| block.fixed_vertex(index)))),
    )
}

/// The box around each ring of `block`, in ring order, as its west, south,
/// east and north. A ring of no vertex has none.
pub(crate) fn ring_extents(block: &[u8]) -> Vec<Option<[f64; 4]>> {
    let block = Block::of(block);
    block
        .rings()
        .map(|ring| {
            box_around(ring.map(|index| block.fixed_vertex(index)))
                .map(|extent| extent.map(degrees))
        })
        .collect()
}

/// A point inside the area of `block` and off its outline. It is the
/// middle of the widest stretch the area covers along one latitude. None
/// for an area of no height.
pub(crate) fn interior_point(block: &[u8]) -> Option<[f64; 2]> {
    let block = Block::of(block);
    let lat = block
        .rings()
        .flat_map(|ring| ring.skip(1).map(|index| index - 1))
        .find_map(|edge| {
            let ([_, from], [_, to]) = (block.vertex(edge), block.vertex(edge + 1));
            (from != to).then_some((from + to) / 2.0)
        })?;
    let mut crossings = block
        .edges_between(lat, lat)
        .filter_map(|edge| {
            let ([x0, y0], [x1, y1]) = (block.vertex(edge), block.vertex(edge + 1));
            ((y0 > lat) != (y1 > lat)).then(|| x0 + (lat - y0) * (x1 - x0) / (y1 - y0))
        })
        .collect::<Vec<_>>();
    crossings.sort_unstable_by(f64::total_cmp);
    crossings
        .as_chunks::<2>()
        .0
        .iter()
        .max_by(|left, right| (left[1] - left[0]).total_cmp(&(right[1] - right[0])))
        .map(|[west, east]| [(west + east) / 2.0, lat])
}

/// Whether the outline of `area` stays in `shape`. A stretch of it within
/// `tolerance` degrees of the outline of `shape` is not out of it. Two
/// sources draw one border a little apart, and that is no crossing of it.
pub(crate) fn outline_within(area: &[u8], shape: &[u8], tolerance: f64) -> bool {
    let (area, shape) = (Block::of(area), Block::of(shape));
    let mut edges = Vec::new();
    let mut cuts = Vec::new();
    let mut spans = Vec::new();
    for ring in area.rings() {
        for index in ring.skip(1) {
            let (from, to) = (area.vertex(index - 1), area.vertex(index));
            edges.clear();
            edges.extend(shape.edges_between(
                from[1].min(to[1]) - tolerance,
                from[1].max(to[1]) + tolerance,
            ));
            edges.sort_unstable();
            edges.dedup();
            cuts.clear();
            cuts.extend([0.0, 1.0]);
            cuts.extend(edges.iter().filter_map(|&edge| {
                crossing(from, to, shape.vertex(edge), shape.vertex(edge + 1))
            }));
            cuts.sort_unstable_by(f64::total_cmp);
            // Between two crossings the edge is on one side, and its middle
            // says which.
            for piece in cuts.windows(2) {
                let [lon, lat] = along(from, to, (piece[0] + piece[1]) / 2.0);
                if piece[0] < piece[1]
                    && !covers(shape.bytes, lon, lat)
                    && !hugs(
                        along(from, to, piece[0]),
                        along(from, to, piece[1]),
                        &shape,
                        &edges,
                        tolerance,
                        &mut spans,
                    )
                {
                    return false;
                }
            }
        }
    }
    true
}

/// Whether the point is within `tolerance` degrees of the outline of `block`.
pub(crate) fn near_outline(block: &[u8], [lon, lat]: [f64; 2], tolerance: f64) -> bool {
    let block = Block::of(block);
    block
        .edges_between(lat - tolerance, lat + tolerance)
        .any(|edge| {
            capsule_span(
                [lon, lat],
                [lon, lat],
                block.vertex(edge),
                block.vertex(edge + 1),
                tolerance,
            )
            .is_some()
        })
}

/// The vertices of ring `ring` of `block`.
pub(crate) fn ring_vertices(block: &[u8], ring: usize) -> impl Iterator<Item = [f64; 2]> + '_ {
    let block = Block::of(block);
    block
        .rings()
        .nth(ring)
        .into_iter()
        .flatten()
        .map(move |index| block.vertex(index))
}

fn along(from: [f64; 2], to: [f64; 2], at: f64) -> [f64; 2] {
    [
        from[0] + (to[0] - from[0]) * at,
        from[1] + (to[1] - from[1]) * at,
    ]
}

fn cross(left: [f64; 2], right: [f64; 2]) -> f64 {
    left[0] * right[1] - left[1] * right[0]
}

fn dot(left: [f64; 2], right: [f64; 2]) -> f64 {
    left[0] * right[0] + left[1] * right[1]
}

fn minus(left: [f64; 2], right: [f64; 2]) -> [f64; 2] {
    [left[0] - right[0], left[1] - right[1]]
}

/// How far along `from..to`, strictly between its ends, it crosses the edge
/// `start..end`. None for edges that run alongside. Those meet over a
/// stretch, which [`hugs`] judges.
fn crossing(from: [f64; 2], to: [f64; 2], start: [f64; 2], end: [f64; 2]) -> Option<f64> {
    let (line, edge) = (minus(to, from), minus(end, start));
    let turn = cross(line, edge);
    if turn == 0.0 {
        return None;
    }
    let offset = minus(start, from);
    let (at, on_edge) = (cross(offset, edge) / turn, cross(offset, line) / turn);
    (at > 0.0 && at < 1.0 && (0.0..=1.0).contains(&on_edge)).then_some(at)
}

/// Whether every point of `start..end` is within `tolerance` of one of
/// `edges` of `shape`. `spans` is scratch.
fn hugs(
    start: [f64; 2],
    end: [f64; 2],
    shape: &Block<'_>,
    edges: &[usize],
    tolerance: f64,
    spans: &mut Vec<(f64, f64)>,
) -> bool {
    spans.clear();
    spans.extend(edges.iter().filter_map(|&edge| {
        capsule_span(
            start,
            end,
            shape.vertex(edge),
            shape.vertex(edge + 1),
            tolerance,
        )
    }));
    spans.sort_unstable_by(|left, right| left.0.total_cmp(&right.0));
    let mut reach = 0.0;
    for &(from, to) in spans.iter() {
        if from > reach {
            return false;
        }
        reach = to.max(reach);
    }
    reach >= 1.0
}

/// The stretch of `start..end`, as fractions of it, within `tolerance` of
/// the edge `from..to`. That neighbourhood is the edge's band and a disc
/// around each of its ends. It is convex, so it is one stretch.
fn capsule_span(
    start: [f64; 2],
    end: [f64; 2],
    from: [f64; 2],
    to: [f64; 2],
    tolerance: f64,
) -> Option<(f64, f64)> {
    let line = minus(end, start);
    let disc = |centre: [f64; 2]| {
        let offset = minus(start, centre);
        let (a, b, c) = (
            dot(line, line),
            2.0 * dot(line, offset),
            dot(offset, offset) - tolerance * tolerance,
        );
        if a == 0.0 {
            return (c <= 0.0).then_some((0.0, 1.0));
        }
        let root = (b * b - 4.0 * a * c).sqrt();
        // NaN when the line misses the disc, which no comparison passes.
        let (low, high) = ((-b - root) / (2.0 * a), (-b + root) / (2.0 * a));
        (low <= 1.0 && high >= 0.0).then_some((low.max(0.0), high.min(1.0)))
    };
    let band = || {
        let edge = minus(to, from);
        let length = dot(edge, edge).sqrt();
        if length == 0.0 {
            return None;
        }
        let unit = [edge[0] / length, edge[1] / length];
        let offset = minus(start, from);
        let (mut low, mut high) = (0.0_f64, 1.0_f64);
        // Along the edge within its ends, and across it within the tolerance.
        for (at, step, least, most) in [
            (dot(offset, unit), dot(line, unit), 0.0, length),
            (
                cross(unit, offset),
                cross(unit, line),
                -tolerance,
                tolerance,
            ),
        ] {
            if step == 0.0 {
                if at < least || at > most {
                    return None;
                }
                continue;
            }
            let (enter, leave) = ((least - at) / step, (most - at) / step);
            low = low.max(enter.min(leave));
            high = high.min(enter.max(leave));
        }
        (low <= high).then_some((low, high))
    };
    [disc(from), disc(to), band()]
        .into_iter()
        .flatten()
        .reduce(|held, span| (held.0.min(span.0), held.1.max(span.1)))
}

/// The shape file as it is written, one block after another.
pub(crate) struct ShapeWriter {
    file: BufWriter<ScratchFile>,
    bytes: u64,
}

impl ShapeWriter {
    /// A shape file of scratch label `label`.
    pub(crate) fn create(scratch: &Arc<Scratch>, label: &str) -> Result<Self> {
        let path = scratch.new_file(label);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(Error::Decode("scratch file name is not UTF-8"))?;
        Ok(Self {
            file: BufWriter::new(ScratchFile::create(scratch, label, name)?),
            bytes: 0,
        })
    }

    /// Append a block [`encode_shape`] wrote.
    pub(crate) fn write(&mut self, block: &[u8]) -> Result<ShapeRef> {
        self.file.write_all(block)?;
        let shape = ShapeRef {
            offset: self.bytes,
            bytes: block.len() as u64,
        };
        self.bytes += shape.bytes;
        Ok(shape)
    }

    /// Close the file and map it to test points against its shapes.
    pub(crate) fn finish(self) -> Result<Shapes> {
        let file = self
            .file
            .into_inner()
            .map_err(|error| Error::Io(error.into_error()))?;
        Shapes::map(file)
    }
}

/// The shape file, mapped. Removed once dropped.
#[derive(Debug)]
pub(crate) struct Shapes {
    /// Taken before the file is removed: Windows cannot remove a mapped file.
    map: Option<Mmap>,
    file: ScratchFile,
}

impl Shapes {
    fn map(file: ScratchFile) -> Result<Self> {
        let path = file.path().to_path_buf();
        // Windows cannot map an empty file, and an empty one has no shape.
        let map = if file.bytes() == 0 {
            None
        } else {
            let read = File::open(&path).map_err(|source| Error::IoPath {
                action: "failed to open boundary shape file",
                path: path.clone(),
                source,
            })?;
            // SAFETY: the file is this build's own scratch and is not
            // written again while mapped.
            Some(unsafe { Mmap::map(&read) }.map_err(|source| Error::IoPath {
                action: "failed to map boundary shape file",
                path,
                source,
            })?)
        };
        Ok(Self { map, file })
    }

    /// The block of a shape. None for one past the file's end, which is a
    /// checkpoint gone wrong that the restore refuses before any test.
    pub(crate) fn block(&self, shape: ShapeRef) -> Option<&[u8]> {
        let bytes = self.map.as_deref()?;
        let start = usize::try_from(shape.offset).ok()?;
        bytes.get(start..start.checked_add(usize::try_from(shape.bytes).ok()?)?)
    }

    /// Whether the shape covers the point, its boundary included.
    pub(crate) fn covers(&self, shape: ShapeRef, lon: f64, lat: f64) -> bool {
        self.block(shape)
            .is_some_and(|block| covers(block, lon, lat))
    }

    /// Bytes the file holds.
    pub(crate) fn bytes(&self) -> u64 {
        self.file.bytes()
    }

    /// The file as a checkpoint names it: its name and length.
    pub(crate) fn state(&self) -> (String, u64) {
        (self.file.name(), self.file.bytes())
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        self.file.path()
    }

    /// The file a checkpoint named.
    pub(crate) fn restore(scratch: &Arc<Scratch>, (name, bytes): &(String, u64)) -> Result<Self> {
        Self::map(ScratchFile::reopen(scratch, SHAPES_LABEL, name, *bytes)?)
    }

    /// Whether every one of `shapes` lies inside the file.
    pub(crate) fn holds(&self, shapes: impl IntoIterator<Item = ShapeRef>) -> bool {
        let len = self.file.bytes();
        shapes.into_iter().all(|shape| {
            shape
                .offset
                .checked_add(shape.bytes)
                .is_some_and(|end| end <= len)
        })
    }
}

impl Drop for Shapes {
    fn drop(&mut self) {
        drop(self.map.take());
    }
}

#[cfg(test)]
mod tests {
    use geo::{BoundingRect, Covers, LineString, Point, Polygon};

    use super::*;

    /// A ring of `points` vertices around a jagged circle.
    fn jagged(center: (f64, f64), radius: f64, points: usize) -> LineString<f64> {
        let mut ring = (0..points)
            .map(|index| {
                let angle = index as f64 / points as f64 * std::f64::consts::TAU;
                let r = radius * if index % 2 == 0 { 1.0 } else { 0.8 };
                let fixed = |value: f64| degrees(fixed(value));
                (
                    fixed(center.0 + r * angle.cos()),
                    fixed(center.1 + r * angle.sin()),
                )
            })
            .collect::<Vec<_>>();
        ring.push(ring[0]);
        LineString::from(ring)
    }

    #[test]
    fn banded_shapes_agree_with_the_plain_test() {
        let geometry = MultiPolygon::new(vec![
            Polygon::new(
                jagged((0.0, 0.0), 10.0, 2_000),
                vec![jagged((2.0, 1.0), 3.0, 600)],
            ),
            Polygon::new(jagged((30.0, 5.0), 4.0, 900), Vec::new()),
        ]);
        let mut block = Vec::new();
        encode_shape(&geometry, &mut block);
        let mut inside = 0;
        for step in 0..4_000 {
            let lon = -12.0 + (step % 80) as f64 * 0.57;
            let lat = -12.0 + (step / 80) as f64 * 0.49;
            let expected = geometry.covers(&Point::new(lon, lat));
            assert_eq!(covers(&block, lon, lat), expected, "{lon} {lat}");
            inside += usize::from(expected);
        }
        assert!(inside > 500, "the grid reaches inside: {inside}");
        assert!(covers(&block, 10.0, 0.0), "a vertex is covered");
        assert!(!covers(&block, 2.0, 1.0), "the hole is not");
        assert!(covers(&block, 30.0, 5.0), "the second part is");
        let [west, south, east, north] = extent(&block);
        let bounds = geometry.bounding_rect().expect("a box");
        assert_eq!(
            [west, south, east, north],
            [
                bounds.min().x,
                bounds.min().y,
                bounds.max().x,
                bounds.max().y
            ]
        );
    }

    #[test]
    fn the_narrowest_extent_is_the_box_of_the_rings_the_short_way_round() {
        let square = |west: f64, south: f64, east: f64, north: f64| {
            Polygon::new(
                LineString::from(vec![
                    (west, south),
                    (east, south),
                    (east, north),
                    (west, north),
                    (west, south),
                ]),
                Vec::new(),
            )
        };
        let block = |polygons: Vec<Polygon<f64>>| {
            let mut block = Vec::new();
            encode_shape(&MultiPolygon::new(polygons), &mut block);
            block
        };
        assert_eq!(
            narrowest_extent(&block(vec![square(
                7.4090279, 43.7247599, 7.4398704, 43.7519311
            )])),
            Some([74_090_279, 437_247_599, 74_398_704, 437_519_311])
        );
        // Drawn as OSM draws an area over the antimeridian, one ring each
        // side that meet at 180 and -180.
        assert_eq!(
            narrowest_extent(&block(vec![
                square(177.0, -19.0, 180.0, -16.0),
                square(-180.0, -18.0, -178.5, -15.5),
            ])),
            Some([1_770_000_000, -190_000_000, -1_785_000_000, -155_000_000])
        );
        assert_eq!(narrowest_extent(&block(Vec::new())), None);
    }

    fn shape(rings: &[&[(f64, f64)]]) -> Vec<u8> {
        let polygons = rings
            .iter()
            .map(|ring| {
                let mut ring = ring.to_vec();
                ring.push(ring[0]);
                Polygon::new(LineString::from(ring), Vec::new())
            })
            .collect();
        let mut block = Vec::new();
        encode_shape(&MultiPolygon::new(polygons), &mut block);
        block
    }

    fn rectangle(west: f64, south: f64, east: f64, north: f64) -> Vec<u8> {
        shape(&[&[(west, south), (east, south), (east, north), (west, north)]])
    }

    #[test]
    fn an_outline_is_within_a_shape_it_never_leaves_by_more_than_the_tolerance() {
        let tolerance = 1e-6;
        let square = rectangle(0.0, 0.0, 10.0, 10.0);
        let within = |area: &[u8], zone: &[u8]| outline_within(area, zone, tolerance);
        assert!(within(&rectangle(2.0, 2.0, 4.0, 4.0), &square));
        assert!(
            !within(&rectangle(8.0, 2.0, 12.0, 4.0), &square),
            "crosses out"
        );
        assert!(
            !within(&rectangle(20.0, 20.0, 22.0, 22.0), &square),
            "apart"
        );
        assert!(!within(&rectangle(20.0, 2.0, 22.0, 4.0), &square), "beside");
        // One border drawn by two sources, a little apart.
        assert!(within(&rectangle(-0.000_000_5, 2.0, 3.0, 4.0), &square));
        assert!(!within(&rectangle(-0.000_002, 2.0, 3.0, 4.0), &square));
        assert!(within(
            &rectangle(-0.000_000_5, -0.000_000_4, 10.000_000_3, 10.000_000_6),
            &square
        ));
        // The other source has a vertex where this one has none.
        let split = shape(&[&[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
            (0.0, 5.0),
        ]]);
        assert!(within(&rectangle(-0.000_000_5, 2.0, 3.0, 8.0), &split));
        // Every vertex inside, yet an edge runs over a notch of the shape.
        let notched = shape(&[&[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (6.0, 10.0),
            (6.0, 5.0),
            (4.0, 5.0),
            (4.0, 10.0),
            (0.0, 10.0),
        ]]);
        let over = rectangle(3.0, 6.0, 7.0, 8.0);
        for (lon, lat) in [(3.0, 6.0), (7.0, 6.0), (7.0, 8.0), (3.0, 8.0)] {
            assert!(covers(&notched, lon, lat));
        }
        assert!(!within(&over, &notched));
        assert!(within(&rectangle(1.0, 6.0, 3.0, 8.0), &notched));
        // An outline around a hole of the shape stays in the shape, and
        // one through the hole does not.
        let holed = shape(&[
            &[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)],
            &[(4.0, 4.0), (6.0, 4.0), (6.0, 6.0), (4.0, 6.0)],
        ]);
        assert!(within(&rectangle(3.0, 3.0, 7.0, 7.0), &holed));
        assert!(!within(&rectangle(3.0, 3.0, 5.0, 5.0), &holed));
    }

    #[test]
    fn a_point_is_near_an_outline_within_the_tolerance_of_an_edge_or_a_corner() {
        let square = rectangle(0.0, 0.0, 10.0, 10.0);
        let near = |lon: f64, lat: f64| near_outline(&square, [lon, lat], 1e-6);
        assert!(near(0.0, 5.0));
        assert!(near(0.000_000_9, 5.0));
        assert!(near(-0.000_000_9, 5.0));
        assert!(!near(0.000_001_1, 5.0));
        assert!(near(10.000_000_6, 10.000_000_6), "inside the corner's disc");
        assert!(!near(10.000_000_8, 10.000_000_8), "past the corner's disc");
        assert!(!near(5.0, 5.0));
        assert!(!near(5.0, 20.0));
    }

    #[test]
    fn an_interior_point_is_inside_an_area_whose_centre_is_not() {
        // A C: its centre lies in the gap.
        let c = shape(&[&[
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 2.0),
            (2.0, 2.0),
            (2.0, 8.0),
            (10.0, 8.0),
            (10.0, 10.0),
            (0.0, 10.0),
        ]]);
        assert!(!covers(&c, 6.0, 5.0));
        assert_eq!(interior_point(&c), Some([5.0, 1.0]));
        // Of two parts, the wider one at the latitude tried holds the point.
        let parts = shape(&[
            &[(0.0, 0.0), (1.0, 0.0), (1.0, 4.0), (0.0, 4.0)],
            &[(5.0, 0.0), (9.0, 0.0), (9.0, 4.0), (5.0, 4.0)],
        ]);
        assert_eq!(interior_point(&parts), Some([7.0, 2.0]));
        assert_eq!(interior_point(&shape(&[])), None);
        assert_eq!(
            ring_extents(&parts),
            [Some([0.0, 0.0, 1.0, 4.0]), Some([5.0, 0.0, 9.0, 4.0])]
        );
        assert_eq!(
            ring_vertices(&parts, 1).collect::<Vec<_>>(),
            [[5.0, 0.0], [9.0, 0.0], [9.0, 4.0], [5.0, 4.0], [5.0, 0.0]]
        );
        assert_eq!(ring_vertices(&parts, 2).count(), 0);
    }

    #[test]
    fn a_long_edge_is_found_from_every_band_it_spans() {
        // A tall thin sliver: one long edge on each side, many short ones
        // along the bottom, so the long edges span every band.
        let mut ring = vec![(0.0, 0.0)];
        ring.extend((1..=400).map(|step| (f64::from(step) * 0.0025, 0.0)));
        ring.extend([(1.0, 50.0), (0.0, 50.0), (0.0, 0.0)]);
        let geometry = MultiPolygon::new(vec![Polygon::new(LineString::from(ring), Vec::new())]);
        let mut block = Vec::new();
        encode_shape(&geometry, &mut block);
        for lat in [0.5, 12.0, 25.0, 49.5, 50.0] {
            assert!(covers(&block, 0.5, lat), "{lat}");
            assert!(!covers(&block, 1.5, lat), "{lat}");
        }
        assert!(covers(&block, 0.0, 30.0), "the west edge is covered");
    }

    #[test]
    fn shapes_read_back_from_the_file() {
        let scratch = Scratch::create(
            std::env::temp_dir().join(format!("og-shapes-{}", uuid::Uuid::new_v4())),
        )
        .expect("scratch");
        let square = |x: f64| {
            MultiPolygon::new(vec![Polygon::new(
                LineString::from(vec![(x, 0.0), (x + 1.0, 0.0), (x + 1.0, 1.0), (x, 0.0)]),
                Vec::new(),
            )])
        };
        let mut writer = ShapeWriter::create(&scratch, SHAPES_LABEL).expect("writer");
        let refs = [0.0, 5.0].map(|x| {
            let mut block = Vec::new();
            encode_shape(&square(x), &mut block);
            writer.write(&block).expect("write")
        });
        let shapes = writer.finish().expect("finish");
        assert!(shapes.holds(refs));
        assert!(shapes.covers(refs[0], 0.9, 0.5));
        assert!(!shapes.covers(refs[0], 5.9, 0.5));
        assert!(shapes.covers(refs[1], 5.9, 0.5));
    }
}
