use geojson::{Geometry, GeometryValue, Position};

use crate::record::Extent;

/// Longitude/latitude of a point geometry, or `None` if it is not a point or
/// either coordinate is non-finite. This is the single canonical, strict
/// reader; callers that previously skipped the finite check now reject NaN/inf
/// coordinates, which is the safer behavior.
pub(crate) fn point_lon_lat(geometry: &Geometry) -> Option<[f64; 2]> {
    let GeometryValue::Point { coordinates } = &geometry.value else {
        return None;
    };
    let [lon, lat, ..] = coordinates.as_slice() else {
        return None;
    };
    (lon.is_finite() && lat.is_finite()).then_some([*lon, *lat])
}

/// The lines of a line geometry: one for a line string, each of a multi line
/// string (a street of several ways). `None` for any other geometry.
pub(crate) fn line_parts(geometry: &Geometry) -> Option<Vec<&[Position]>> {
    match &geometry.value {
        GeometryValue::LineString { coordinates } => Some(vec![coordinates.as_slice()]),
        GeometryValue::MultiLineString { coordinates } => {
            Some(coordinates.iter().map(Vec::as_slice).collect())
        }
        _ => None,
    }
}

/// A line geometry of `parts`: a line string for one, else a multi line
/// string.
pub(crate) fn line_geometry(mut parts: Vec<Vec<Position>>) -> Geometry {
    Geometry::new(if parts.len() == 1 {
        GeometryValue::LineString {
            coordinates: parts.remove(0),
        }
    } else {
        GeometryValue::MultiLineString { coordinates: parts }
    })
}

/// The box around `points`, each a longitude and a latitude in 1e-7
/// degrees. `None` without a point.
pub(crate) fn box_around(points: impl IntoIterator<Item = [i32; 2]>) -> Option<Extent> {
    points
        .into_iter()
        .map(|[lon, lat]| [lon, lat, lon, lat])
        .reduce(|held, at| {
            [
                held[0].min(at[0]),
                held[1].min(at[1]),
                held[2].max(at[2]),
                held[3].max(at[3]),
            ]
        })
}

/// The narrowest box holding every one of `parts`, each the box around one
/// ring or line, which never crosses the antimeridian itself. The widest
/// stretch of longitude no part reaches is left out, so parts on both sides
/// of the antimeridian make a box that crosses it. `None` without a part.
pub(crate) fn narrowest_box(parts: impl IntoIterator<Item = Extent>) -> Option<Extent> {
    /// A full turn of longitude in 1e-7 degrees.
    const TURN: i64 = 3_600_000_000;
    let (mut south, mut north) = (i32::MAX, i32::MIN);
    let mut spans = Vec::new();
    for [west, part_south, east, part_north] in parts {
        spans.push((west, east));
        south = south.min(part_south);
        north = north.max(part_north);
    }
    spans.sort_unstable();
    let &(first, mut reach) = spans.first()?;
    // The widest gap between parts: its width, the part after it and the
    // reach of the parts before it.
    let mut widest = None::<(i64, i32, i32)>;
    for &(west, east) in &spans[1..] {
        let gap = i64::from(west) - i64::from(reach);
        if gap > widest.map_or(0, |(held, ..)| held) {
            widest = Some((gap, west, reach));
        }
        reach = reach.max(east);
    }
    let around = i64::from(first) + TURN - i64::from(reach);
    Some(match widest {
        Some((gap, west, east)) if gap > around => [west, south, east, north],
        _ => [first, south, reach, north],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::point_geometry;

    #[test]
    fn the_narrowest_box_crosses_the_antimeridian_only_when_that_is_narrower() {
        let e7 = |degrees: i32| degrees * 10_000_000;
        let part = |west: i32, south: i32, east: i32, north: i32| {
            [e7(west), e7(south), e7(east), e7(north)]
        };
        assert_eq!(narrowest_box([]), None);
        assert_eq!(
            narrowest_box([part(7, 43, 8, 44)]),
            Some(part(7, 43, 8, 44))
        );
        // Far apart, yet nearer each other the short way round.
        assert_eq!(
            narrowest_box([part(-60, -5, -50, 5), part(100, 10, 110, 20)]),
            Some(part(-60, -5, 110, 20))
        );
        // A ring ending at 180 and one starting at -180 are one area.
        assert_eq!(
            narrowest_box([part(-180, -20, -178, -16), part(177, -19, 180, -12)]),
            Some(part(177, -20, -178, -12))
        );
        // Islands either side of the antimeridian that do not touch it.
        assert_eq!(
            narrowest_box([
                part(-179, -18, -178, -17),
                part(176, -19, 178, -16),
                part(177, -17, 179, -15),
            ]),
            Some(part(176, -19, -178, -15))
        );
        // Two gaps of one width: the box stays in one piece.
        assert_eq!(
            narrowest_box([part(-90, 0, -80, 1), part(90, 0, 100, 1)]),
            Some(part(-90, 0, 100, 1))
        );
        // A part inside another widens nothing.
        assert_eq!(
            narrowest_box([part(0, 0, 10, 10), part(2, 2, 3, 3), part(20, 1, 30, 2)]),
            Some(part(0, 0, 30, 10))
        );
    }

    #[test]
    fn reads_finite_point() {
        assert_eq!(
            point_lon_lat(&point_geometry(-79.0, 43.0)),
            Some([-79.0, 43.0])
        );
    }

    #[test]
    fn rejects_non_point_and_non_finite() {
        assert_eq!(point_lon_lat(&point_geometry(f64::NAN, 43.0)), None);
        let line = Geometry::new(GeometryValue::LineString {
            coordinates: vec![vec![0.0, 0.0].into(), vec![1.0, 1.0].into()],
        });
        assert_eq!(point_lon_lat(&line), None);
    }
}
