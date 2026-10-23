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
/// Scratch label of the shape file.
const SHAPES_LABEL: &str = "boundary-shapes";

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
    let (lat_lo, lat_hi) = coords
        .iter()
        .map(|[_, lat]| degrees(*lat))
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), lat| {
            (lo.min(lat), hi.max(lat))
        });
    // An edge joins a vertex to the next one of its ring.
    let mut edges = Vec::new();
    let mut start = 0;
    for &end in &ring_ends {
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
fn fixed(value: f64) -> i32 {
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

/// The shape file as it is written, one block after another.
pub(crate) struct ShapeWriter {
    file: BufWriter<ScratchFile>,
    bytes: u64,
}

impl ShapeWriter {
    pub(crate) fn create(scratch: &Arc<Scratch>) -> Result<Self> {
        let path = scratch.new_file(SHAPES_LABEL);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(Error::Decode("scratch file name is not UTF-8"))?;
        Ok(Self {
            file: BufWriter::new(ScratchFile::create(scratch, SHAPES_LABEL, name)?),
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

    /// Whether the shape covers the point, its boundary included.
    pub(crate) fn covers(&self, shape: ShapeRef, lon: f64, lat: f64) -> bool {
        let block = self.map.as_deref().and_then(|bytes| {
            let start = usize::try_from(shape.offset).ok()?;
            bytes.get(start..start.checked_add(usize::try_from(shape.bytes).ok()?)?)
        });
        // A block past the file's end is a checkpoint gone wrong, which the
        // restore refuses before any test.
        block.is_some_and(|block| covers(block, lon, lat))
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
    use geo::{Covers, LineString, Point, Polygon};

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
        let mut writer = ShapeWriter::create(&scratch).expect("writer");
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
