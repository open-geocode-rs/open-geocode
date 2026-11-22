//! Point-in-area tests for areas of any size, read from a scratch file.
//!
//! The boundaries of a planet hold several times the memory a build is
//! given, so no shape stays on the heap. Each built area is written to one
//! scratch file as a shape block (see [`crate::areas`]), the same block the
//! Pack keeps for reverse geocoding. The file is mapped, so the shapes take
//! page cache the system reclaims as it needs, not heap.

use std::{
    fs::File,
    io::{BufWriter, Write},
    sync::Arc,
};

use memmap2::Mmap;

use super::locations::degrees;
pub(crate) use crate::areas::{ShapeRef, encode_shape};
use crate::{
    areas::{Shape, ShapeSource},
    error::{Error, Result},
    extsort::{Scratch, ScratchFile},
};

/// Scratch label of the shape file.
const SHAPES_LABEL: &str = "boundary-shapes";

/// Whether the area of `block` covers the point, its boundary included.
/// The block is the build's own, so one that does not read is a bug.
pub(crate) fn covers(block: &[u8], lon: f64, lat: f64) -> bool {
    Shape::parse(block)
        .and_then(|shape| shape.covers(lon, lat))
        .expect("a shape block this build wrote")
}

/// The box around the area of `block`: west, south, east, north.
pub(crate) fn extent(block: &[u8]) -> [f64; 4] {
    let [west, south, east, north] = Shape::parse(block)
        .and_then(|shape| shape.bounds())
        .expect("a shape block this build wrote");
    [west, south, east, north].map(degrees)
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
        // A block past the file's end is a checkpoint gone wrong, which the
        // restore refuses before any test.
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

impl ShapeSource for Shapes {
    fn block(&self, shape: ShapeRef) -> Option<&[u8]> {
        let start = usize::try_from(shape.offset).ok()?;
        self.map
            .as_deref()?
            .get(start..start.checked_add(usize::try_from(shape.bytes).ok()?)?)
    }
}

#[cfg(test)]
mod tests {
    use geo::{BoundingRect, Covers, LineString, MultiPolygon, Point, Polygon};

    use super::*;
    use crate::areas::fixed;

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
