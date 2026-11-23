//! A route a vendor searches along: its points, decoded from the polyline
//! the vendor takes, and how far either side of it a result may lie.

use crate::util::geo::haversine_m;

/// A polyline that does not decode.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Malformed;

/// The points of a polyline in the encoding Google's routes write, five
/// decimals, latitude before longitude. Points come back as `[lon, lat]`.
pub(crate) fn google_polyline(encoded: &str) -> Result<Vec<[f64; 2]>, Malformed> {
    let mut values = encoded.bytes().map(|byte| {
        byte.checked_sub(63)
            .filter(|value| *value < 64)
            .map(u64::from)
            .ok_or(Malformed)
    });
    let mut points = Vec::new();
    let (mut lat, mut lon) = (0i64, 0i64);
    while let Some(delta_lat) = signed(&mut values)? {
        let delta_lon = signed(&mut values)?.ok_or(Malformed)?;
        lat = lat.checked_add(delta_lat).ok_or(Malformed)?;
        lon = lon.checked_add(delta_lon).ok_or(Malformed)?;
        points.push(coordinate(lon, lat, 1e5)?);
    }
    Ok(points)
}

/// The points of a HERE flexible polyline, its precision and any third
/// dimension read from its header. Points come back as `[lon, lat]`.
pub(crate) fn flexible_polyline(encoded: &str) -> Result<Vec<[f64; 2]>, Malformed> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut values = encoded.bytes().map(|byte| {
        ALPHABET
            .iter()
            .position(|letter| *letter == byte)
            .map(|value| value as u64)
            .ok_or(Malformed)
    });
    if unsigned(&mut values)? != Some(1) {
        return Err(Malformed);
    }
    let header = unsigned(&mut values)?.ok_or(Malformed)?;
    let scale = 10f64.powi(i32::try_from(header & 15).map_err(|_| Malformed)?);
    let third = (header >> 4) & 7 != 0;
    let mut points = Vec::new();
    let (mut lat, mut lon) = (0i64, 0i64);
    while let Some(delta_lat) = signed(&mut values)? {
        lat = lat.checked_add(delta_lat).ok_or(Malformed)?;
        let delta_lon = signed(&mut values)?.ok_or(Malformed)?;
        lon = lon.checked_add(delta_lon).ok_or(Malformed)?;
        if third {
            signed(&mut values)?.ok_or(Malformed)?;
        }
        points.push(coordinate(lon, lat, scale)?);
    }
    Ok(points)
}

fn coordinate(lon: i64, lat: i64, scale: f64) -> Result<[f64; 2], Malformed> {
    let point = [lon as f64 / scale, lat as f64 / scale];
    if (-180.0..=180.0).contains(&point[0]) && (-90.0..=90.0).contains(&point[1]) {
        Ok(point)
    } else {
        Err(Malformed)
    }
}

/// One varint of 5-bit chunks, a set sixth bit meaning another follows.
/// `None` where the text has ended.
fn unsigned(
    values: &mut impl Iterator<Item = Result<u64, Malformed>>,
) -> Result<Option<u64>, Malformed> {
    let mut result = 0u64;
    let mut shift = 0;
    let mut started = false;
    for value in values.by_ref() {
        let value = value?;
        started = true;
        if shift > 60 || (shift == 60 && value & 0x10 != 0) {
            return Err(Malformed);
        }
        result |= (value & 0x1f) << shift;
        if value & 0x20 == 0 {
            return Ok(Some(result));
        }
        shift += 5;
    }
    if started { Err(Malformed) } else { Ok(None) }
}

/// A varint in zig-zag form, the sign in its lowest bit.
fn signed(
    values: &mut impl Iterator<Item = Result<u64, Malformed>>,
) -> Result<Option<i64>, Malformed> {
    Ok(unsigned(values)?.map(|value| {
        let magnitude = i64::try_from(value >> 1).unwrap_or(i64::MAX);
        if value & 1 == 1 {
            !magnitude
        } else {
            magnitude
        }
    }))
}

/// A route and how far either side of it a result may lie.
#[derive(Debug, Clone)]
pub(crate) struct Corridor {
    points: Vec<[f64; 2]>,
    width_m: f64,
}

/// Where a point lies by a route: how far along it the nearest point of the
/// route is, and how far off the route the point lies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Reach {
    pub(crate) along_m: f64,
    pub(crate) off_m: f64,
}

impl Corridor {
    /// A corridor of at least two points.
    pub(crate) fn new(points: Vec<[f64; 2]>, width_m: f64) -> Option<Self> {
        (points.len() >= 2).then_some(Self { points, width_m })
    }

    /// The box holding the corridor, `[west, south, east, north]`.
    pub(crate) fn bbox(&self) -> [f64; 4] {
        let mut bbox = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
        for [lon, lat] in &self.points {
            bbox = [
                bbox[0].min(*lon),
                bbox[1].min(*lat),
                bbox[2].max(*lon),
                bbox[3].max(*lat),
            ];
        }
        // Use the outer latitude for longitude padding, where a degree spans less distance.
        let pad_lat = self.width_m / 111_000.0;
        let widest = bbox[1].abs().max(bbox[3].abs()).min(89.0);
        let pad_lon = pad_lat / widest.to_radians().cos();
        let wraps = bbox[0] - pad_lon < -180.0
            || bbox[2] + pad_lon > 180.0
            || self
                .points
                .windows(2)
                .any(|pair| (pair[1][0] - pair[0][0]).abs() > 180.0);
        [
            if wraps { -180.0 } else { bbox[0] - pad_lon },
            (bbox[1] - pad_lat).max(-90.0),
            if wraps { 180.0 } else { bbox[2] + pad_lon },
            (bbox[3] + pad_lat).min(90.0),
        ]
    }

    /// Where `point` lies by the route, when it lies within the width.
    pub(crate) fn reach(&self, point: [f64; 2]) -> Option<Reach> {
        let mut best: Option<Reach> = None;
        let mut walked = 0.0;
        for pair in self.points.windows(2) {
            let (start, end) = (pair[0], pair[1]);
            let length = haversine_m(start[0], start[1], end[0], end[1]);
            let share = nearest_share(start, end, point);
            let foot = [
                start[0] + longitude_delta(start[0], end[0]) * share,
                start[1] + (end[1] - start[1]) * share,
            ];
            let off_m = haversine_m(foot[0], foot[1], point[0], point[1]);
            if best.is_none_or(|best| off_m < best.off_m) {
                best = Some(Reach {
                    along_m: walked + length * share,
                    off_m,
                });
            }
            walked += length;
        }
        best.filter(|reach| reach.off_m <= self.width_m)
    }
}

fn longitude_delta(from: f64, to: f64) -> f64 {
    (to - from + 180.0).rem_euclid(360.0) - 180.0
}

/// How far along a segment its point nearest `point` lies, 0 to 1, on a
/// plane scaled to the segment's latitude.
fn nearest_share(start: [f64; 2], end: [f64; 2], point: [f64; 2]) -> f64 {
    let scale = ((start[1] + end[1]) / 2.0).to_radians().cos();
    let (dx, dy) = (longitude_delta(start[0], end[0]) * scale, end[1] - start[1]);
    let (px, py) = (
        longitude_delta(start[0], point[0]) * scale,
        point[1] - start[1],
    );
    let length = dx * dx + dy * dy;
    if length == 0.0 {
        return 0.0;
    }
    ((px * dx + py * dy) / length).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_corridor_keeps_nearby_points_across_the_dateline() {
        let corridor = Corridor::new(vec![[179.9, 0.0], [-179.9, 0.0]], 500.0).expect("corridor");
        assert_eq!([corridor.bbox()[0], corridor.bbox()[2]], [-180.0, 180.0]);
        assert!(corridor.reach([-179.99, 0.001]).is_some());
        assert!(corridor.reach([0.0, 0.0]).is_none());
    }

    fn close(points: &[[f64; 2]], expected: &[[f64; 2]]) {
        assert_eq!(points.len(), expected.len(), "{points:?}");
        for (got, want) in points.iter().zip(expected) {
            assert!(
                (got[0] - want[0]).abs() < 1e-6 && (got[1] - want[1]).abs() < 1e-6,
                "{got:?} {want:?}"
            );
        }
    }

    /// The examples each encoding's own description decodes.
    #[test]
    fn polylines_decode_as_their_encodings_describe() {
        close(
            &google_polyline("_p~iF~ps|U_ulLnnqC_mqNvxq`@").expect("google"),
            &[[-120.2, 38.5], [-120.95, 40.7], [-126.453, 43.252]],
        );
        close(
            &flexible_polyline("BFoz5xJ67i1B1B7PzIhaxL7Y").expect("flexible"),
            &[
                [8.69821, 50.10228],
                [8.69567, 50.10201],
                [8.69150, 50.10063],
                [8.68752, 50.09878],
            ],
        );
        assert_eq!(flexible_polyline("B!"), Err(Malformed));
        assert_eq!(google_polyline("_p~iF"), Err(Malformed));
    }

    #[test]
    fn a_corridor_holds_what_lies_within_its_width_and_says_how_far_along() {
        // About 1.1 km east along the equator.
        let corridor = Corridor::new(vec![[0.0, 0.0], [0.01, 0.0]], 100.0).expect("two points");
        let reach = corridor.reach([0.005, 0.0005]).expect("55 m off");
        assert!((reach.along_m - 556.0).abs() < 2.0, "{reach:?}");
        assert!((reach.off_m - 55.6).abs() < 1.0, "{reach:?}");
        assert_eq!(corridor.reach([0.005, 0.002]), None, "222 m off");
        assert_eq!(corridor.reach([0.02, 0.0]), None, "past the end");
        let bbox = corridor.bbox();
        assert!(bbox[0] < 0.0 && bbox[2] > 0.01 && bbox[1] < 0.0 && bbox[3] > 0.0);
        assert!(Corridor::new(vec![[0.0, 0.0]], 100.0).is_none());
    }
}
