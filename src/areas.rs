//! The areas records lie in, kept as shapes, so reverse geocoding names the
//! areas holding any point, not only a point a record stands on.
//!
//! The build tests every record against these same blocks and ranks the
//! areas holding it with [`chain_of`], so a point and a record at it name
//! one chain.
//!
//! A shape block keeps an area's rings as 1e-7 fixed-point coordinates and
//! files its edges into horizontal bands of equal height. A test reads the
//! band its latitude falls in and casts a ray east across that band's edges
//! only, so an island drawn from a coastline of a million vertices costs a
//! few dozen edge tests.
//!
//! ```text
//! areas/index   area_count u64, then per area, in Hilbert order of its box:
//!                 record_id u32 | country_record_id u32 (u32::MAX: none)
//!                 | rank u32 | tiers u16 | layer u8 | 0 u8
//!                 | shape_offset u64 | shape_bytes u32
//! areas/shapes  the areas' shape blocks, in the same order
//! areas/boxes   a box tree (see spatial_index), item i the box of area i
//!
//! shape block   vertex_count u32 | ring_count u32 | band_count u32
//!               | entry_count u32 | lat_lo f64 | lat_hi f64
//!               | ring_end u32 x ring_count
//!               | (lon i32, lat i32) x vertex_count
//!               | band_start u32 x (band_count + 1) | edge u32 x entry_count
//! ```

use std::{fmt::Debug, io::Write, sync::Arc};

use geo::MultiPolygon;

use crate::{
    container::{self, Bytes, Container},
    context::{AdminContextTuple, CONTEXT_FLAG_AMBIGUOUS_ADMIN, RecordContext, TierSet},
    error::{Error, Result},
    pack::RecordId,
    record::PlaceLayer,
    spatial_index::{self, BoxTree},
    util::{
        codec::{read_u32_le, read_u64_le},
        geo::degrees,
        hilbert::hilbert_key,
    },
};

pub const SECTION_INDEX: &str = "areas/index";
pub const SECTION_SHAPES: &str = "areas/shapes";
pub const SECTION_BOXES: &str = "areas/boxes";
pub const AREAS_VERSION: u32 = 1;

const ENTRY_BYTES: usize = 28;
const NO_RECORD: u32 = u32::MAX;
/// Edges a band holds on average when its edges are spread evenly.
const EDGES_PER_BAND: usize = 16;
/// Band entries a shape may hold per edge: an edge spanning several bands
/// is filed in each, and a shape whose long edges would fill more halves its
/// bands until they fit.
const ENTRIES_PER_EDGE: usize = 4;
/// Bytes of a block's header: four counts and the band span.
const HEADER_BYTES: usize = 32;

/// Where an area's block lies in the file that holds it.
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

/// A shape block whose length agrees with the counts it states. Every
/// offset and index it holds is still checked as it is read, since a Pack
/// is external data.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shape<'a> {
    block: &'a [u8],
    vertices: usize,
    bands: usize,
    entries: usize,
    coords_at: usize,
    starts_at: usize,
    entries_at: usize,
    lat_lo: f64,
    lat_hi: f64,
}

fn bad_shape() -> Error {
    Error::Inconsistent("an area shape block does not match its counts".into())
}

impl<'a> Shape<'a> {
    pub(crate) fn parse(block: &'a [u8]) -> Result<Self> {
        let count = |at: usize| -> Result<usize> {
            Ok(usize::try_from(
                read_u32_le(block, at).ok_or_else(bad_shape)?,
            )?)
        };
        let float = |at: usize| -> Result<f64> {
            Ok(f64::from_bits(
                read_u64_le(block, at).ok_or_else(bad_shape)?,
            ))
        };
        let (vertices, rings, bands, entries) = (count(0)?, count(4)?, count(8)?, count(12)?);
        let words = |count: usize, width: usize| count.checked_mul(width).ok_or_else(bad_shape);
        let coords_at = HEADER_BYTES
            .checked_add(words(rings, 4)?)
            .ok_or_else(bad_shape)?;
        let starts_at = coords_at
            .checked_add(words(vertices, 8)?)
            .ok_or_else(bad_shape)?;
        let entries_at = starts_at
            .checked_add(words(bands.checked_add(1).ok_or_else(bad_shape)?, 4)?)
            .ok_or_else(bad_shape)?;
        let end = entries_at
            .checked_add(words(entries, 4)?)
            .ok_or_else(bad_shape)?;
        if bands == 0 || end != block.len() {
            return Err(bad_shape());
        }
        Ok(Self {
            block,
            vertices,
            bands,
            entries,
            coords_at,
            starts_at,
            entries_at,
            lat_lo: float(16)?,
            lat_hi: float(24)?,
        })
    }

    fn word(&self, at: usize) -> Result<usize> {
        Ok(usize::try_from(
            read_u32_le(self.block, at).ok_or_else(bad_shape)?,
        )?)
    }

    fn vertex(&self, index: usize) -> Result<[i32; 2]> {
        if index >= self.vertices {
            return Err(bad_shape());
        }
        let at = self.coords_at + 8 * index;
        let value = |at: usize| -> Result<i32> {
            Ok(read_u32_le(self.block, at)
                .ok_or_else(bad_shape)?
                .cast_signed())
        };
        Ok([value(at)?, value(at + 4)?])
    }

    /// Whether the area covers the point, its boundary included.
    pub(crate) fn covers(&self, lon: f64, lat: f64) -> Result<bool> {
        if !(self.lat_lo..=self.lat_hi).contains(&lat) {
            return Ok(false);
        }
        let band = band_of(lat, self.lat_lo, self.lat_hi, self.bands);
        let (first, last) = (
            self.word(self.starts_at + 4 * band)?,
            self.word(self.starts_at + 4 * (band + 1))?,
        );
        if first > last || last > self.entries {
            return Err(bad_shape());
        }
        let point = |index: usize| -> Result<(f64, f64)> {
            let [x, y] = self.vertex(index)?;
            Ok((degrees(x), degrees(y)))
        };
        let mut inside = false;
        for entry in first..last {
            let edge = self.word(self.entries_at + 4 * entry)?;
            let ((x0, y0), (x1, y1)) = (point(edge)?, point(edge + 1)?);
            if y0 == lat && y1 == lat {
                if lon >= x0.min(x1) && lon <= x0.max(x1) {
                    return Ok(true);
                }
                continue;
            }
            if (y0 > lat) == (y1 > lat) {
                if (y0 == lat && x0 == lon) || (y1 == lat && x1 == lon) {
                    return Ok(true);
                }
                continue;
            }
            let x = x0 + (lat - y0) * (x1 - x0) / (y1 - y0);
            if x == lon {
                return Ok(true);
            }
            if x > lon {
                inside = !inside;
            }
        }
        Ok(inside)
    }

    /// The box around the area in fixed point, as west, south, east, north.
    /// Inverted when the block holds no vertex.
    pub(crate) fn bounds(&self) -> Result<[i32; 4]> {
        let mut bounds = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
        for index in 0..self.vertices {
            let [lon, lat] = self.vertex(index)?;
            bounds = [
                bounds[0].min(lon),
                bounds[1].min(lat),
                bounds[2].max(lon),
                bounds[3].max(lat),
            ];
        }
        Ok(bounds)
    }
}

/// What ranks an area against the others of its layer holding one point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AreaKey {
    pub record_id: RecordId,
    /// The layer it is ranked in. One area per layer holds a point.
    pub layer: PlaceLayer,
    /// Everything it is. Usually just `layer`, but Berlin is a state and a
    /// city.
    pub tiers: TierSet,
    /// Its place among every area of the Pack, lowest first. Admin
    /// boundaries come before places drawn as areas, then the smallest.
    pub rank: u32,
    /// The country its `ISO3166-2` code names, for a region whose country
    /// has no boundary in the input.
    pub country_record_id: Option<RecordId>,
}

/// The chain of the areas holding one point, each with whether the record
/// at the point states its name. Of the areas of one layer, a stated one
/// wins, then the lowest rank; more than one flags the chain ambiguous.
pub(crate) fn chain_of(holding: impl IntoIterator<Item = (AreaKey, bool)>) -> RecordContext {
    let mut flags = 0;
    let mut best: [Option<(AreaKey, bool)>; PlaceLayer::ALL.len()] = Default::default();
    for (area, stated) in holding {
        let slot = &mut best[area.layer as usize];
        match slot {
            Some((held, held_stated)) => {
                flags |= CONTEXT_FLAG_AMBIGUOUS_ADMIN;
                if (!stated, area.rank) < (!*held_stated, held.rank) {
                    *slot = Some((area, stated));
                }
            }
            None => *slot = Some((area, stated)),
        }
    }

    // Each tier takes the best area ranked into it, and an area that is
    // more than one thing is set under each: it stays one entry of the
    // chain carrying both tiers.
    let mut tuple = AdminContextTuple::from_tiers(PlaceLayer::ALL.into_iter().flat_map(|tier| {
        best[tier as usize]
            .into_iter()
            .flat_map(|(area, _)| area.tiers.iter().map(move |at| (at, area.record_id)))
    }));
    if tuple.country_record_id().is_none()
        && let Some(country) =
            best[PlaceLayer::Region as usize].and_then(|(region, _)| region.country_record_id)
    {
        tuple.set(PlaceLayer::Country, country);
    }

    RecordContext {
        admin_context: tuple,
        flags,
    }
}

/// Where the build keeps the blocks [`Writer`] copies into the Pack.
pub(crate) trait ShapeSource: Debug + Send + Sync {
    fn block(&self, shape: ShapeRef) -> Option<&[u8]>;
}

/// The areas of a build, written into the Pack once every record has its id.
#[derive(Debug)]
pub struct Writer {
    areas: Vec<(AreaKey, ShapeRef)>,
    shapes: Arc<dyn ShapeSource>,
}

impl Writer {
    pub(crate) fn new(areas: Vec<(AreaKey, ShapeRef)>, shapes: Arc<dyn ShapeSource>) -> Self {
        Self { areas, shapes }
    }

    /// Write the three sections.
    pub(crate) fn finish(self, pack: &mut container::Writer) -> Result<()> {
        let missing = || Error::Inconsistent("an area's shape is missing from its file".into());
        let mut placed = Vec::with_capacity(self.areas.len());
        for (key, shape) in self.areas {
            let block = self.shapes.block(shape).ok_or_else(missing)?;
            let bounds = Shape::parse(block)?.bounds()?;
            // An area without a vertex holds no point.
            if bounds[0] > bounds[2] {
                continue;
            }
            let middle = |low: i32, high: i32| ((i64::from(low) + i64::from(high)) / 2) as i32;
            let order = hilbert_key(middle(bounds[0], bounds[2]), middle(bounds[1], bounds[3]));
            placed.push((order, key, shape, bounds));
        }
        placed.sort_by_key(|(order, key, ..)| (*order, key.record_id));

        let narrow = |id: RecordId| {
            u32::try_from(id)
                .ok()
                .filter(|id| *id != NO_RECORD)
                .ok_or(Error::IdOutOfRange {
                    what: "area record",
                    id,
                })
        };
        pack.begin(SECTION_INDEX, AREAS_VERSION)?;
        pack.write_all(&(placed.len() as u64).to_le_bytes())?;
        let mut offset = 0u64;
        for (_, key, shape, _) in &placed {
            let bytes = u32::try_from(shape.bytes)
                .map_err(|_| Error::Inconsistent("an area's shape is too large".into()))?;
            pack.write_all(&narrow(key.record_id)?.to_le_bytes())?;
            let country = key.country_record_id.map(narrow).transpose()?;
            pack.write_all(&country.unwrap_or(NO_RECORD).to_le_bytes())?;
            pack.write_all(&key.rank.to_le_bytes())?;
            pack.write_all(&key.tiers.bits().to_le_bytes())?;
            pack.write_all(&[key.layer as u8, 0])?;
            pack.write_all(&offset.to_le_bytes())?;
            pack.write_all(&bytes.to_le_bytes())?;
            offset += shape.bytes;
        }
        pack.end()?;

        pack.begin(SECTION_SHAPES, AREAS_VERSION)?;
        for (_, _, shape, _) in &placed {
            pack.write_all(self.shapes.block(*shape).ok_or_else(missing)?)?;
        }
        pack.end()?;

        let boxes = placed
            .iter()
            .map(|(.., bounds)| *bounds)
            .collect::<Vec<_>>();
        spatial_index::write_box_tree(pack, SECTION_BOXES, AREAS_VERSION, &boxes)
    }
}

/// The areas of a Pack, to find those holding a point.
#[derive(Clone)]
pub struct Reader {
    index: Bytes,
    shapes: Bytes,
    boxes: BoxTree,
}

impl Debug for Reader {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Reader")
            .field("areas", &self.boxes.len())
            .finish()
    }
}

fn bad_index() -> Error {
    Error::Inconsistent("the area index does not match its sections".into())
}

impl Reader {
    /// The areas of the Pack; `None` for a Pack built before it kept them.
    pub fn open(container: &Container) -> Result<Option<Self>> {
        if !container.sections().contains_key(SECTION_INDEX) {
            return Ok(None);
        }
        let index = container.section(SECTION_INDEX, AREAS_VERSION)?;
        let shapes = container.section(SECTION_SHAPES, AREAS_VERSION)?;
        let boxes = BoxTree::open(container, SECTION_BOXES, AREAS_VERSION)?;
        let count = usize::try_from(read_u64_le(&index, 0).ok_or_else(bad_index)?)?;
        let expected = count
            .checked_mul(ENTRY_BYTES)
            .and_then(|bytes| bytes.checked_add(8))
            .ok_or_else(bad_index)?;
        if index.len() != expected || boxes.len() != count {
            return Err(bad_index());
        }
        Ok(Some(Self {
            index,
            shapes,
            boxes,
        }))
    }

    /// The `item`th area and its shape block.
    fn area(&self, item: usize) -> Result<(AreaKey, &[u8])> {
        let at = item
            .checked_mul(ENTRY_BYTES)
            .and_then(|at| at.checked_add(8))
            .ok_or_else(bad_index)?;
        let entry = self
            .index
            .get(at..at.checked_add(ENTRY_BYTES).ok_or_else(bad_index)?)
            .ok_or_else(bad_index)?;
        let word = |at: usize| read_u32_le(entry, at).ok_or_else(bad_index);
        let country = word(4)?;
        let tiers =
            TierSet::from_bits(u16::from_le_bytes([entry[12], entry[13]])).ok_or_else(bad_index)?;
        let layer = *PlaceLayer::ALL
            .get(usize::from(entry[14]))
            .ok_or_else(bad_index)?;
        let offset = usize::try_from(read_u64_le(entry, 16).ok_or_else(bad_index)?)?;
        let bytes = usize::try_from(word(24)?)?;
        let block = self
            .shapes
            .get(offset..offset.checked_add(bytes).ok_or_else(bad_index)?)
            .ok_or_else(bad_index)?;
        let key = AreaKey {
            record_id: RecordId::from(word(0)?),
            layer,
            tiers,
            rank: word(8)?,
            country_record_id: (country != NO_RECORD).then_some(RecordId::from(country)),
        };
        Ok((key, block))
    }

    /// The chain of the areas holding the point, ranked as the build ranks
    /// those holding a record that states none of them.
    pub fn chain_at(&self, lon: f64, lat: f64) -> Result<AdminContextTuple> {
        let mut holding = Vec::new();
        for item in self.boxes.containing(lon, lat) {
            let (area, block) = self.area(item)?;
            if Shape::parse(block)?.covers(lon, lat)? {
                holding.push((area, false));
            }
        }
        Ok(chain_of(holding).admin_context)
    }
}

#[cfg(test)]
mod tests {
    use geo::{LineString, Polygon};

    use super::*;

    /// A square of `side` degrees with `steps` vertices a side, so its
    /// edges fill several bands.
    fn square_block(side: f64, steps: u32) -> Vec<u8> {
        let edge = |step: u32| side * f64::from(step) / f64::from(steps);
        let ring = (0..steps)
            .map(|step| (edge(step), 0.0))
            .chain((0..steps).map(|step| (side, edge(step))))
            .chain((0..steps).map(|step| (side - edge(step), side)))
            .chain((0..=steps).map(|step| (0.0, side - edge(step))))
            .collect::<Vec<_>>();
        let mut block = Vec::new();
        encode_shape(
            &MultiPolygon::new(vec![Polygon::new(LineString::from(ring), Vec::new())]),
            &mut block,
        );
        block
    }

    fn message(result: Result<impl Debug>) -> String {
        result.expect_err("a damaged block").to_string()
    }

    #[test]
    fn a_shape_tests_points_and_bounds_its_area() {
        let block = square_block(1.0, 64);
        let shape = Shape::parse(&block).expect("shape");
        assert!(shape.covers(0.5, 0.5).expect("test"));
        assert!(shape.covers(1.0, 0.25).expect("test"), "its edge");
        assert!(!shape.covers(1.5, 0.5).expect("test"));
        assert!(!shape.covers(0.5, 1.5).expect("test"));
        assert_eq!(
            shape.bounds().expect("bounds"),
            [0, 0, 10_000_000, 10_000_000]
        );
    }

    /// A Pack's blocks are external data. One that does not add up is an
    /// error, never a read past its end.
    #[test]
    fn a_damaged_shape_is_an_error() {
        let block = square_block(1.0, 64);
        let damaged = "Pack is inconsistent: an area shape block does not match its counts";

        assert_eq!(message(Shape::parse(&block[..block.len() - 1])), damaged);
        assert_eq!(message(Shape::parse(&block[..20])), damaged);

        let mut bands = block.clone();
        bands[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(message(Shape::parse(&bands)), damaged);

        let shape = Shape::parse(&block).expect("shape");
        let mut edge = block.clone();
        edge[shape.entries_at..shape.entries_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let edge = Shape::parse(&edge).expect("lengths still agree");
        let first_band = shape.lat_lo;
        assert_eq!(message(edge.covers(0.5, first_band)), damaged);

        let mut start = block.clone();
        start[shape.starts_at + 4..shape.starts_at + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        let start = Shape::parse(&start).expect("lengths still agree");
        assert_eq!(message(start.covers(0.5, first_band)), damaged);
    }
}
