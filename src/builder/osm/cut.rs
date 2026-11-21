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
    Area, BoundingRect, Contains, Coord, InteriorPoint, Intersects, LineString, MultiPolygon,
    Point, Polygon, Rect,
};

use crate::error::{Error, Result};

use super::boundary::Vertex;

/// The polygon an extract was cut to, as its separate parts: one box for
/// each input of a build, or each outer ring of a `.poly` file with its
/// holes. An area is closed inside the part its `label` lies in: the space
/// between two parts holds nothing of either.
#[derive(Debug, Clone)]
pub(crate) struct Cut {
    parts: Vec<Polygon<f64>>,
}

impl Cut {
    /// The bounds each input's header states, `[min_lon, min_lat, max_lon,
    /// max_lat]`: the box around its polygon, which holds every way the cut
    /// left out of it only where the polygon fills its box.
    pub(crate) fn of_bounds(bounds: impl IntoIterator<Item = [f64; 4]>) -> Self {
        Self {
            parts: bounds
                .into_iter()
                .map(|[min_lon, min_lat, max_lon, max_lat]| {
                    Rect::new(
                        Coord {
                            x: min_lon,
                            y: min_lat,
                        },
                        Coord {
                            x: max_lon,
                            y: max_lat,
                        },
                    )
                    .to_polygon()
                })
                .collect(),
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
        Ok(Self { parts: outers })
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
    /// clipped to the cut's box and read even-odd, as a boundary's shape is.
    /// The other side is the cut's outline with them. A cut that is not a
    /// box leaves the rings as they run between it and its box, where the
    /// extract holds nothing to place. Where that path runs differs
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
        let point = Point::new(anchor[0], anchor[1]);
        self.parts
            .iter()
            .filter(|part| part.contains(&point))
            .find_map(|part| close_in(part, rings, open, point.0))
    }
}

/// [`Cut::close`] inside one part of the cut.
fn close_in(
    part: &Polygon<f64>,
    rings: &[Vec<Vertex>],
    open: &[Vec<Vertex>],
    anchor: Coord<f64>,
) -> Option<MultiPolygon<f64>> {
    let outlines = outlines(part);
    let mut contours = rings
        .iter()
        .map(|ring| ring.iter().map(coord).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let mut ends = Vec::with_capacity(open.len());
    for chain in open {
        let (first, last) = (coord(chain.first()?), coord(chain.last()?));
        if part.intersects(&Point::from(first)) || part.intersects(&Point::from(last)) {
            return None;
        }
        let (from, to) = (nearest_on(&outlines, first)?, nearest_on(&outlines, last)?);
        if from.ring != to.ring {
            return None;
        }
        ends.push((from, to));
    }
    contours.extend(closures(&outlines, open, &ends));
    let bounds = part.bounding_rect()?;
    let mut clipped = contours
        .iter()
        .map(|contour| clip_to(contour, &bounds))
        .filter(|ring| ring.len() >= 3)
        .collect::<Vec<_>>();
    if !even_odd(&clipped, anchor) {
        clipped.extend(outlines);
    }
    if !even_odd(&clipped, anchor) {
        return None;
    }
    let area = nested(&clipped);
    (!area.0.is_empty()).then_some(area)
}

/// The open chains closed into rings along the outline they meet. On each
/// ring of the outline their ends lie in some order; the stretches between
/// them alternate, and every other one, from the first end to the second,
/// the third to the fourth and so on, joins the chains into rings that
/// never cross, whichever way each chain runs. The other stretches would
/// make the rest of the cut, which [`close_in`] reaches by adding the
/// outline when the anchor says so.
fn closures(
    outlines: &[Vec<Coord<f64>>],
    open: &[Vec<Vertex>],
    ends: &[(OnOutline, OnOutline)],
) -> Vec<Vec<Coord<f64>>> {
    // Every end on the outline, in order along its ring: the chain, and
    // whether it is the chain's first end.
    let mut order = ends
        .iter()
        .enumerate()
        .flat_map(|(chain, (from, to))| [(*from, chain, true), (*to, chain, false)])
        .collect::<Vec<_>>();
    order.sort_by(|(a, ..), (b, ..)| {
        (a.ring, a.segment)
            .cmp(&(b.ring, b.segment))
            .then(a.along.total_cmp(&b.along))
    });
    // Each end's place among the ends of its own ring.
    let mut place = vec![[0usize; 2]; open.len()];
    for (at, (_, chain, first)) in order.iter().enumerate() {
        place[*chain][usize::from(!first)] = at;
    }
    let ring_start = |at: usize| {
        let ring = order[at].0.ring;
        order
            .iter()
            .position(|(on, ..)| on.ring == ring)
            .unwrap_or(0)
    };
    // The end the stretch from end `at` leads to.
    let partner = |at: usize| match (at - ring_start(at)) % 2 {
        0 => at + 1,
        _ => at - 1,
    };
    let mut used = vec![false; open.len()];
    let mut contours = Vec::new();
    for start in 0..open.len() {
        if used[start] {
            continue;
        }
        let mut contour = Vec::new();
        let (mut chain, mut forward) = (start, true);
        loop {
            used[chain] = true;
            let points = open[chain].iter().map(coord);
            match forward {
                true => contour.extend(points),
                false => contour.extend(points.rev()),
            }
            let leave = place[chain][usize::from(forward)];
            let arrive = partner(leave);
            let (from, to) = (order[leave].0, order[arrive].0);
            match arrive > leave {
                true => contour.extend(walk(&outlines[from.ring], from, to)),
                false => {
                    let mut back = walk(&outlines[from.ring], to, from);
                    back.reverse();
                    contour.extend(back);
                }
            }
            let (_, then, first) = order[arrive];
            if then == start || used[then] {
                break;
            }
            (chain, forward) = (then, first);
        }
        contours.push(contour);
    }
    contours
}

/// Every ring of `part`'s outline, each as its points in order, closed.
fn outlines(part: &Polygon<f64>) -> Vec<Vec<Coord<f64>>> {
    std::iter::once(part.exterior())
        .chain(part.interiors())
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

/// A side of a box: the coordinate it bounds, where, and whether the inside
/// lies above it.
type Side = (fn(Coord<f64>) -> f64, f64, bool);

/// The part of `ring` inside `bounds`, as one ring: each edge cut where it
/// crosses a side of the box. A box is convex, so clipping every ring of an
/// even-odd shape alone leaves its parity inside the box as it was.
fn clip_to(ring: &[Coord<f64>], bounds: &Rect<f64>) -> Vec<Coord<f64>> {
    let (min, max) = (bounds.min(), bounds.max());
    let sides: [Side; 4] = [
        (|point| point.x, min.x, true),
        (|point| point.x, max.x, false),
        (|point| point.y, min.y, true),
        (|point| point.y, max.y, false),
    ];
    let mut points = ring.to_vec();
    for (along, edge, above) in sides {
        let inside = |point: Coord<f64>| match above {
            true => along(point) >= edge,
            false => along(point) <= edge,
        };
        let crossing = |from: Coord<f64>, to: Coord<f64>| {
            let (a, b) = (along(from), along(to));
            from + (to - from) * ((edge - a) / (b - a))
        };
        let mut kept = Vec::with_capacity(points.len());
        for (index, &point) in points.iter().enumerate() {
            let previous = points[(index + points.len() - 1) % points.len()];
            match (inside(previous), inside(point)) {
                (true, true) => kept.push(point),
                (true, false) => kept.push(crossing(previous, point)),
                (false, true) => {
                    kept.push(crossing(previous, point));
                    kept.push(point);
                }
                (false, false) => {}
            }
        }
        points = kept;
        if points.is_empty() {
            break;
        }
    }
    points
}

/// Whether `point` lies inside `rings` by the even-odd rule, the rule a
/// boundary's shape is read by (see [`super::containment::encode_shape`]).
fn even_odd(rings: &[Vec<Coord<f64>>], point: Coord<f64>) -> bool {
    let mut inside = false;
    for ring in rings {
        for (index, &to) in ring.iter().enumerate() {
            let from = ring[(index + ring.len() - 1) % ring.len()];
            if (from.y > point.y) != (to.y > point.y)
                && point.x < from.x + (point.y - from.y) / (to.y - from.y) * (to.x - from.x)
            {
                inside = !inside;
            }
        }
    }
    inside
}

/// `rings` read by the even-odd rule as polygons: a ring inside an odd
/// number of the others is a hole of the smallest of them.
fn nested(rings: &[Vec<Coord<f64>>]) -> MultiPolygon<f64> {
    let polygons = rings
        .iter()
        .map(|ring| Polygon::new(LineString::new(ring.clone()), Vec::new()))
        .collect::<Vec<_>>();
    let areas = polygons
        .iter()
        .map(|polygon| polygon.unsigned_area())
        .collect::<Vec<_>>();
    // A point inside each ring, off its outline, to count the larger rings it
    // lies in: a ring sharing part of the cut's outline holds none of it.
    let probe = |index: usize| polygons[index].interior_point().map(|point| point.0);
    let holders = |index: usize| {
        probe(index).map_or_else(Vec::new, |point| {
            (0..rings.len())
                .filter(|&other| {
                    other != index
                        && areas[other] > areas[index]
                        && even_odd(&rings[other..=other], point)
                })
                .collect::<Vec<_>>()
        })
    };
    let depths = (0..rings.len()).map(holders).collect::<Vec<_>>();
    let mut outers = Vec::new();
    let mut holes = Vec::<(usize, LineString<f64>)>::new();
    for (index, held_by) in depths.iter().enumerate() {
        let ring = LineString::new(rings[index].clone());
        match held_by.len() % 2 {
            0 => outers.push((index, Polygon::new(ring, Vec::new()))),
            _ => {
                let Some(&smallest) = held_by.iter().min_by(|a, b| {
                    polygons[**a]
                        .unsigned_area()
                        .total_cmp(&polygons[**b].unsigned_area())
                }) else {
                    continue;
                };
                holes.push((smallest, ring));
            }
        }
    }
    for (holder, ring) in holes {
        if let Some((_, polygon)) = outers.iter_mut().find(|(index, _)| *index == holder) {
            polygon.interiors_push(ring);
        }
    }
    MultiPolygon::new(outers.into_iter().map(|(_, polygon)| polygon).collect())
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
        let cut = Cut::of_bounds([[0.0, 0.0, 10.0, 10.0]]);
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
        let cut = Cut::of_bounds([[0.0, 0.0, 10.0, 10.0]]);
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
        assert!(cut.parts[0].contains(&Point::new(1.0, 1.0)));
        assert!(!cut.parts[0].contains(&Point::new(5.0, 5.0)));
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

    /// A `.poly` cut of two separate parts: an outline the cut broke in one
    /// says nothing of the other, which no part of the area reaches.
    #[test]
    fn a_part_of_the_cut_the_outline_never_reaches_stays_out() {
        let cut = Cut::parse_poly(
            "two\n1\n  0 0\n  10 0\n  10 10\n  0 10\nEND\n2\n  20 0\n  30 0\n  30 10\n  20 10\nEND\nEND\n",
        )
        .expect("poly");
        let chain = line(&[(5.0, 12.0), (5.0, 5.0), (5.0, -2.0)], 1);
        let west = cut.close(&[], &[chain], [2.0, 5.0]).expect("west");
        assert!(west.contains(&Point::new(2.0, 5.0)));
        assert!(!west.contains(&Point::new(8.0, 5.0)));
        assert!(!west.contains(&Point::new(25.0, 5.0)));
    }

    /// An area all of whose outer ways the cut left out, the cut lying
    /// inside it, keeps its inner ring: it is the cut less that ring.
    #[test]
    fn an_area_round_the_whole_cut_keeps_its_inner_ring() {
        let cut = Cut::of_bounds([[0.0, 0.0, 10.0, 10.0]]);
        let inner = line(
            &[(4.0, 4.0), (6.0, 4.0), (6.0, 6.0), (4.0, 6.0), (4.0, 4.0)],
            1,
        );
        let area = cut.close(&[inner], &[], [2.0, 2.0]).expect("area");
        assert!(area.contains(&Point::new(2.0, 2.0)));
        assert!(area.contains(&Point::new(8.0, 8.0)));
        assert!(!area.contains(&Point::new(5.0, 5.0)));
    }

    /// Three outlines the cut broke, whose closures along the cut overlap:
    /// the strips between them alternate in and out.
    #[test]
    fn outlines_whose_closures_overlap_keep_their_parity() {
        let cut = Cut::of_bounds([[0.0, 0.0, 10.0, 10.0]]);
        let down = |x: f64, id| line(&[(x, 12.0), (x, 5.0), (x, -2.0)], id);
        let up = |x: f64, id| line(&[(x, -2.0), (x, 5.0), (x, 12.0)], id);
        let area = cut
            .close(&[], &[down(3.0, 1), up(8.0, 4), down(9.0, 7)], [1.0, 5.0])
            .expect("area");
        assert!(area.contains(&Point::new(1.0, 5.0)));
        assert!(!area.contains(&Point::new(5.0, 5.0)));
        assert!(area.contains(&Point::new(8.5, 5.0)));
        assert!(!area.contains(&Point::new(9.5, 5.0)));
    }

    /// A `.poly` cut with a hole: the area stops at the hole the extract
    /// was cut around only as far as its box (see [`Cut::close`]).
    #[test]
    fn a_hole_of_the_cut_is_held_only_by_the_box() {
        let cut = Cut::parse_poly(
            "holed\n1\n  0 0\n  10 0\n  10 10\n  0 10\nEND\n!2\n  4 4\n  6 4\n  6 6\n  4 6\nEND\nEND\n",
        )
        .expect("poly");
        let chain = line(&[(3.0, 12.0), (3.0, 5.0), (3.0, -2.0)], 1);
        let east = cut.close(&[], &[chain], [8.0, 5.0]).expect("east");
        assert!(east.contains(&Point::new(8.0, 5.0)));
        assert!(!east.contains(&Point::new(1.0, 5.0)));
        assert!(
            east.contains(&Point::new(5.0, 5.0)),
            "the hole, where the extract holds nothing"
        );
    }

    /// A build of two extracts far apart: an outline the first one's cut
    /// broke closes inside that one's box, never across the gap between
    /// them, which holds nothing of either. One box round both would hold
    /// the outline's ends and leave the area out.
    #[test]
    fn an_outline_closes_inside_the_box_of_its_own_extract() {
        let (_, chain) = west_of_a_line();
        let two = Cut::of_bounds([[0.0, 0.0, 10.0, 10.0], [20.0, 20.0, 30.0, 30.0]]);
        let west = two
            .close(&[], std::slice::from_ref(&chain), [1.0, 5.0])
            .expect("west");
        assert!(west.contains(&Point::new(2.0, 8.0)));
        assert!(!west.contains(&Point::new(25.0, 25.0)));
        let one = Cut::of_bounds([[0.0, 0.0, 30.0, 30.0]]);
        assert!(one.close(&[], &[chain], [1.0, 5.0]).is_none());
    }

    /// Two outlines the cut broke that run the same way, one inside the
    /// other: the area is the label's corner and the far corner, not the
    /// strip between them, whichever way and in whichever order the chains
    /// come.
    #[test]
    fn outlines_that_run_alike_close_into_alternate_strips() {
        let cut = Cut::of_bounds([[0.0, 0.0, 10.0, 10.0]]);
        let a = line(&[(12.0, 2.0), (2.0, 2.0), (2.0, 12.0)], 1);
        let b = line(&[(12.0, 4.0), (4.0, 4.0), (4.0, 12.0)], 4);
        let reversed = |chain: &Vec<Vertex>| chain.iter().rev().copied().collect::<Vec<_>>();
        for chains in [
            vec![a.clone(), b.clone()],
            vec![b.clone(), a.clone()],
            vec![reversed(&a), b.clone()],
            vec![a.clone(), reversed(&b)],
            vec![reversed(&b), reversed(&a)],
        ] {
            let area = cut.close(&[], &chains, [1.0, 1.0]).expect("area");
            assert!(area.contains(&Point::new(1.0, 1.0)));
            assert!(area.contains(&Point::new(8.0, 8.0)), "{chains:?}");
            assert!(!area.contains(&Point::new(3.0, 3.0)), "{chains:?}");
        }
    }
}
