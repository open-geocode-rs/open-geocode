//! Point-in-area tests that stay fast for areas of any size.
//!
//! An island or a coastal region drawn from its coastline has hundreds of
//! thousands of vertices, and a plain point-in-polygon test walks every edge
//! for every record inside it. An area with many vertices keeps its edges in
//! an R-tree instead, and a test casts a ray east from the point and counts
//! only the edges whose boxes the ray meets.

use geo::{BoundingRect, Covers, MultiPolygon, Point};
use rstar::{AABB, RTree, RTreeObject};

/// Areas with at least this many vertices index their edges.
const INDEXED_VERTICES: usize = 1_024;

/// An area that answers whether it covers a point.
#[derive(Debug)]
pub(crate) enum AreaShape {
    Small(MultiPolygon<f64>),
    Indexed { edges: RTree<Edge>, max_x: f64 },
}

/// One edge of a ring of an indexed area.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Edge {
    from: [f64; 2],
    to: [f64; 2],
}

impl RTreeObject for Edge {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners(self.from, self.to)
    }
}

impl AreaShape {
    pub(crate) fn new(geometry: MultiPolygon<f64>) -> Self {
        let vertices = geometry
            .iter()
            .flat_map(|polygon| std::iter::once(polygon.exterior()).chain(polygon.interiors()))
            .map(|ring| ring.0.len())
            .sum::<usize>();
        let Some(bounds) = geometry.bounding_rect() else {
            return Self::Small(geometry);
        };
        if vertices < INDEXED_VERTICES {
            return Self::Small(geometry);
        }
        let edges = geometry
            .iter()
            .flat_map(|polygon| std::iter::once(polygon.exterior()).chain(polygon.interiors()))
            .flat_map(|ring| {
                ring.lines().map(|line| Edge {
                    from: [line.start.x, line.start.y],
                    to: [line.end.x, line.end.y],
                })
            })
            .collect::<Vec<_>>();
        Self::Indexed {
            edges: RTree::bulk_load(edges),
            max_x: bounds.max().x,
        }
    }

    /// Whether the area covers the point, its boundary included. Rings are
    /// read by the even-odd rule, so holes and separate parts need no
    /// bookkeeping.
    pub(crate) fn covers(&self, lon: f64, lat: f64) -> bool {
        match self {
            Self::Small(geometry) => geometry.covers(&Point::new(lon, lat)),
            Self::Indexed { edges, max_x } => {
                let ray = AABB::from_corners([lon, lat], [max_x.max(lon), lat]);
                let mut inside = false;
                for edge in edges.locate_in_envelope_intersecting(&ray) {
                    let ([x0, y0], [x1, y1]) = (edge.from, edge.to);
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
        }
    }
}

#[cfg(test)]
mod tests {
    use geo::{LineString, Polygon};

    use super::*;

    /// A ring of `points` vertices around a jagged circle, so it is indexed.
    fn jagged(center: (f64, f64), radius: f64, points: usize) -> LineString<f64> {
        let mut ring = (0..points)
            .map(|index| {
                let angle = index as f64 / points as f64 * std::f64::consts::TAU;
                let r = radius * if index % 2 == 0 { 1.0 } else { 0.8 };
                (center.0 + r * angle.cos(), center.1 + r * angle.sin())
            })
            .collect::<Vec<_>>();
        ring.push(ring[0]);
        LineString::from(ring)
    }

    #[test]
    fn indexed_areas_agree_with_the_plain_test() {
        let geometry = MultiPolygon::new(vec![
            Polygon::new(
                jagged((0.0, 0.0), 10.0, 2_000),
                vec![jagged((2.0, 1.0), 3.0, 600)],
            ),
            Polygon::new(jagged((30.0, 5.0), 4.0, 900), Vec::new()),
        ]);
        let indexed = AreaShape::new(geometry.clone());
        assert!(matches!(indexed, AreaShape::Indexed { .. }));
        let mut inside = 0;
        for step in 0..4_000 {
            let lon = -12.0 + (step % 80) as f64 * 0.57;
            let lat = -12.0 + (step / 80) as f64 * 0.49;
            let expected = geometry.covers(&Point::new(lon, lat));
            assert_eq!(indexed.covers(lon, lat), expected, "{lon} {lat}");
            inside += usize::from(expected);
        }
        assert!(inside > 500, "the grid reaches inside: {inside}");
        assert!(indexed.covers(10.0, 0.0), "a vertex is covered");
        assert!(!indexed.covers(2.0, 1.0), "the hole is not");
        assert!(indexed.covers(30.0, 5.0), "the second part is");
    }
}
