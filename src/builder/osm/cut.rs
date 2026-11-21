//! Where an extract was cut from the map, and the areas whose outline the
//! cut runs through.
//!
//! An extract keeps the ways that reach into its polygon, whole, and leaves
//! out the rest. A boundary that runs past the cut loses the ways that lie
//! wholly outside it, so its relation no longer closes. Inside the cut,
//! though, every way of its outline is there: the part of the area the
//! extract holds is the area clipped to the cut, and that is what is built.

use std::{fs, path::Path};

use geo::{
    BooleanOps, Contains, Coord, Intersects, LineString, MultiPolygon, Point, Polygon, Rect,
};

use crate::error::{Error, Result};

use super::boundary::Vertex;

/// The polygon an extract was cut to.
#[derive(Debug, Clone)]
pub(crate) struct Cut {
    shape: MultiPolygon<f64>,
}

impl Cut {
    /// The bounds an extract's header states, `[min_lon, min_lat, max_lon,
    /// max_lat]`: the box around its polygon, which holds every way the cut
    /// left out of it only where the polygon fills its box.
    pub(crate) fn of_bounds([min_lon, min_lat, max_lon, max_lat]: [f64; 4]) -> Self {
        let rect = Rect::new(
            Coord {
                x: min_lon,
                y: min_lat,
            },
            Coord {
                x: max_lon,
                y: max_lat,
            },
        );
        Self {
            shape: MultiPolygon::new(vec![rect.to_polygon()]),
        }
    }

    /// The polygon of a `.poly` file, the format extracts are cut by: a name
    /// line, then rings of `lon lat` lines each ending in `END`, a ring whose
    /// name starts with `!` a hole, then a closing `END`.
    pub(crate) fn read_poly(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(|source| Error::IoPath {
            action: "failed to read",
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse_poly(&text)
    }

    fn parse_poly(text: &str) -> Result<Self> {
        let malformed = || Error::Decode("a cut polygon file is not in the .poly format");
        let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
        lines.next().ok_or_else(malformed)?;
        let mut outers = Vec::<Polygon<f64>>::new();
        let mut holes = Vec::<LineString<f64>>::new();
        loop {
            let name = lines.next().ok_or_else(malformed)?;
            if name == "END" {
                break;
            }
            let mut ring = Vec::new();
            loop {
                let line = lines.next().ok_or_else(malformed)?;
                if line == "END" {
                    break;
                }
                let mut numbers = line.split_whitespace().map(str::parse::<f64>);
                let (Some(Ok(x)), Some(Ok(y)), None) =
                    (numbers.next(), numbers.next(), numbers.next())
                else {
                    return Err(malformed());
                };
                if !x.is_finite() || !y.is_finite() {
                    return Err(malformed());
                }
                ring.push(Coord { x, y });
            }
            if ring.len() < 3 {
                return Err(malformed());
            }
            let ring = LineString::new(ring);
            match name.starts_with('!') {
                true => holes.push(ring),
                false => outers.push(Polygon::new(ring, Vec::new())),
            }
        }
        if outers.is_empty() {
            return Err(malformed());
        }
        for hole in holes {
            let Some(first) = hole.0.first().copied() else {
                continue;
            };
            if let Some(outer) = outers
                .iter_mut()
                .find(|outer| outer.contains(&Point::from(first)))
            {
                outer.interiors_push(hole);
            }
        }
        Ok(Self {
            shape: MultiPolygon::new(outers),
        })
    }

    /// The part of an area inside the cut, from what the extract holds of
    /// its outline: `rings` that close and `open` chains the cut broke,
    /// with `anchor`, its `label` point, inside it.
    ///
    /// Inside the cut a point lies in the area when a path to it from the
    /// anchor crosses the outline an even number of times; every crossing
    /// is a way the extract holds, as the ways left out lie outside it. So
    /// each open chain is closed by any path outside the cut, here to the
    /// nearest point of the cut's outline and along it, and the rings are
    /// filled even-odd and clipped to the cut. Where that path runs differs
    /// from the ways left out only by how often it goes round the cut, which
    /// flips every point inside alike; the anchor says which side is the
    /// area.
    ///
    /// None when a chain ends inside the cut (a way left out reaches into
    /// it, so the outline there is not known), when the anchor is not
    /// inside it, or when both ends of a chain do not meet one ring of the
    /// cut's outline.
    pub(crate) fn close(
        &self,
        rings: &[Vec<Vertex>],
        open: &[Vec<Vertex>],
        anchor: [f64; 2],
    ) -> Option<MultiPolygon<f64>> {
        let anchor = Point::new(anchor[0], anchor[1]);
        if !self.shape.contains(&anchor) {
            return None;
        }
        let outlines = self.outlines();
        let mut contours = Vec::with_capacity(rings.len() + open.len());
        contours.extend(
            rings
                .iter()
                .map(|ring| ring.iter().map(coord).collect::<Vec<_>>()),
        );
        for chain in open {
            let (first, last) = (coord(chain.first()?), coord(chain.last()?));
            if self.shape.intersects(&Point::from(first))
                || self.shape.intersects(&Point::from(last))
            {
                return None;
            }
            let from = nearest_on(&outlines, last)?;
            let to = nearest_on(&outlines, first)?;
            if from.ring != to.ring {
                return None;
            }
            let mut contour = chain.iter().map(coord).collect::<Vec<_>>();
            contour.extend(walk(&outlines[from.ring], from, to));
            contours.push(contour);
        }
        let subject = MultiPolygon::new(
            contours
                .into_iter()
                .filter(|contour| contour.len() >= 3)
                .map(|contour| Polygon::new(LineString::new(contour), Vec::new()))
                .collect(),
        );
        let mut inside = subject.intersection(&self.shape);
        if !inside.contains(&anchor) && !open.is_empty() {
            inside = self.shape.difference(&inside);
        }
        (inside.contains(&anchor) && !inside.0.is_empty()).then_some(inside)
    }

    /// Every ring of the cut's outline, each as its points in order, closed.
    fn outlines(&self) -> Vec<Vec<Coord<f64>>> {
        self.shape
            .iter()
            .flat_map(|polygon| std::iter::once(polygon.exterior()).chain(polygon.interiors()))
            .map(|ring| {
                let mut points = ring.0.clone();
                if points.first() != points.last()
                    && let Some(first) = points.first().copied()
                {
                    points.push(first);
                }
                points
            })
            .filter(|points| points.len() >= 4)
            .collect()
    }
}

fn coord(vertex: &Vertex) -> Coord<f64> {
    Coord {
        x: vertex.lon,
        y: vertex.lat,
    }
}

/// A point on one ring of the cut's outline: the ring, the segment it lies
/// on, how far along that segment, and the point.
#[derive(Debug, Clone, Copy)]
struct OnOutline {
    ring: usize,
    segment: usize,
    along: f64,
    point: Coord<f64>,
}

/// The point of `outlines` nearest `point`, in degrees: the segment from
/// `point` to it meets the cut nowhere else, so a path along it stays
/// outside the cut.
fn nearest_on(outlines: &[Vec<Coord<f64>>], point: Coord<f64>) -> Option<OnOutline> {
    let mut best = None::<(f64, OnOutline)>;
    for (ring, points) in outlines.iter().enumerate() {
        for (segment, pair) in points.windows(2).enumerate() {
            let (start, end) = (pair[0], pair[1]);
            let step = end - start;
            let length = step.x * step.x + step.y * step.y;
            let along = match length > 0.0 {
                true => (((point - start).x * step.x + (point - start).y * step.y) / length)
                    .clamp(0.0, 1.0),
                false => 0.0,
            };
            let on = start + step * along;
            let distance = (point - on).x.powi(2) + (point - on).y.powi(2);
            if best.is_none_or(|(held, _)| distance < held) {
                best = Some((
                    distance,
                    OnOutline {
                        ring,
                        segment,
                        along,
                        point: on,
                    },
                ));
            }
        }
    }
    best.map(|(_, on)| on)
}

/// The points of `outline` from `from` forward to `to`, both included.
fn walk(outline: &[Coord<f64>], from: OnOutline, to: OnOutline) -> Vec<Coord<f64>> {
    let segments = outline.len() - 1;
    let mut points = vec![from.point];
    if from.segment != to.segment || from.along > to.along {
        let mut segment = from.segment;
        loop {
            segment = (segment + 1) % segments;
            points.push(outline[segment]);
            if segment == to.segment {
                break;
            }
        }
    }
    points.push(to.point);
    points
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(points: &[(f64, f64)], first_id: i64) -> Vec<Vertex> {
        points
            .iter()
            .zip(first_id..)
            .map(|(&(lon, lat), node_id)| Vertex { node_id, lat, lon })
            .collect()
    }

    /// A unit cut; an area's outline runs in through its top edge, down,
    /// and out through its bottom edge, the area lying to the west.
    fn west_of_a_line() -> (Cut, Vec<Vertex>) {
        let cut = Cut::of_bounds([0.0, 0.0, 10.0, 10.0]);
        let chain = line(&[(6.0, 12.0), (6.0, 5.0), (4.0, 3.0), (6.0, -2.0)], 1);
        (cut, chain)
    }

    #[test]
    fn an_outline_the_cut_broke_closes_on_the_side_of_its_label() {
        let (cut, chain) = west_of_a_line();
        let west = cut
            .close(&[], std::slice::from_ref(&chain), [1.0, 5.0])
            .expect("west");
        assert!(west.contains(&Point::new(2.0, 8.0)));
        assert!(west.contains(&Point::new(3.5, 3.0)));
        assert!(!west.contains(&Point::new(8.0, 5.0)));
        assert!(!west.contains(&Point::new(5.0, 3.0)));
        let east = cut.close(&[], &[chain], [9.0, 5.0]).expect("east");
        assert!(east.contains(&Point::new(8.0, 5.0)));
        assert!(east.contains(&Point::new(5.0, 3.0)));
        assert!(!east.contains(&Point::new(3.5, 3.0)));
    }

    #[test]
    fn an_outline_that_ends_inside_the_cut_is_not_closed() {
        let cut = Cut::of_bounds([0.0, 0.0, 10.0, 10.0]);
        let chain = line(&[(6.0, 12.0), (6.0, 5.0)], 1);
        assert!(cut.close(&[], &[chain], [1.0, 5.0]).is_none());
    }

    #[test]
    fn an_area_whose_label_lies_outside_the_cut_is_not_closed() {
        let (cut, chain) = west_of_a_line();
        assert!(cut.close(&[], &[chain], [-3.0, 5.0]).is_none());
    }

    #[test]
    fn a_cut_polygon_reads_from_a_poly_file() {
        let cut = Cut::parse_poly(
            "illinois\n1\n  0 0\n  10 0\n  10 10\n  0 10\n  0 0\nEND\n!2\n  4 4\n  6 4\n  6 6\n  4 6\nEND\nEND\n",
        )
        .expect("poly");
        assert!(cut.shape.contains(&Point::new(1.0, 1.0)));
        assert!(!cut.shape.contains(&Point::new(5.0, 5.0)));
        assert!(Cut::parse_poly("illinois\n1\n  0 x\nEND\nEND\n").is_err());
        assert!(Cut::parse_poly("illinois\nEND\n").is_err());
    }

    /// An L-shaped cut: a chain broken in the notch of the L closes along
    /// the cut's outline, never across the notch.
    #[test]
    fn an_outline_closes_along_a_cut_that_is_not_convex() {
        let cut = Cut::parse_poly(
            "l\n1\n  0 0\n  10 0\n  10 4\n  4 4\n  4 10\n  0 10\n  0 0\nEND\nEND\n",
        )
        .expect("poly");
        let chain = line(&[(2.0, 11.0), (2.0, 2.0), (11.0, 2.0)], 1);
        let inside = cut.close(&[], &[chain], [1.0, 1.0]).expect("closed");
        assert!(inside.contains(&Point::new(1.0, 8.0)));
        assert!(inside.contains(&Point::new(8.0, 1.0)));
        assert!(!inside.contains(&Point::new(3.0, 3.0)));
        assert!(!inside.contains(&Point::new(3.0, 8.0)));
    }
}
