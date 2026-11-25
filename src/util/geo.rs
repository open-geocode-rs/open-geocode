use geojson::{Geometry, GeometryValue, Position};

use crate::record::Extent;

pub(crate) const METRES_PER_DEGREE: f64 = 111_320.0;

pub(crate) const EARTH_RADIUS_M: f64 = 6_371_008.8;

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

pub(crate) fn point_lon_lat(geometry: &Geometry) -> Option<[f64; 2]> {
    let GeometryValue::Point { coordinates } = &geometry.value else {
        return None;
    };
    let [lon, lat, ..] = coordinates.as_slice() else {
        return None;
    };
    (lon.is_finite() && lat.is_finite()).then_some([*lon, *lat])
}

pub(crate) fn line_parts(geometry: &Geometry) -> Option<Vec<&[Position]>> {
    match &geometry.value {
        GeometryValue::LineString { coordinates } => Some(vec![coordinates.as_slice()]),
        GeometryValue::MultiLineString { coordinates } => {
            Some(coordinates.iter().map(Vec::as_slice).collect())
        }
        _ => None,
    }
}

pub(crate) fn line_geometry(mut parts: Vec<Vec<Position>>) -> Geometry {
    Geometry::new(if parts.len() == 1 {
        GeometryValue::LineString {
            coordinates: parts.remove(0),
        }
    } else {
        GeometryValue::MultiLineString { coordinates: parts }
    })
}

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

pub(crate) fn narrowest_box(parts: impl IntoIterator<Item = Extent>) -> Option<Extent> {
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
    struct Gap {
        width: i64,
        west_after: i32,
        east_before: i32,
    }
    let mut widest = None::<Gap>;
    for &(west, east) in &spans[1..] {
        let width = i64::from(west) - i64::from(reach);
        if width > widest.as_ref().map_or(0, |held| held.width) {
            widest = Some(Gap {
                width,
                west_after: west,
                east_before: reach,
            });
        }
        reach = reach.max(east);
    }
    let around = i64::from(first) + TURN - i64::from(reach);
    Some(match widest {
        Some(gap) if gap.width > around => [gap.west_after, south, gap.east_before, north],
        _ => [first, south, reach, north],
    })
}

const GEOHASH_ALPHABET: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";

pub(crate) fn geohash(lat: f64, lon: f64, chars: usize) -> String {
    let mut lat_range = (-90.0f64, 90.0f64);
    let mut lon_range = (-180.0f64, 180.0f64);
    let mut hash = String::with_capacity(chars);
    let mut bit = 0;
    let mut value = 0usize;
    let mut even = true;
    while hash.len() < chars {
        if even {
            let mid = (lon_range.0 + lon_range.1) / 2.0;
            if lon > mid {
                value = (value << 1) | 1;
                lon_range.0 = mid;
            } else {
                value <<= 1;
                lon_range.1 = mid;
            }
        } else {
            let mid = (lat_range.0 + lat_range.1) / 2.0;
            if lat > mid {
                value = (value << 1) | 1;
                lat_range.0 = mid;
            } else {
                value <<= 1;
                lat_range.1 = mid;
            }
        }
        even = !even;
        bit += 1;
        if bit == 5 {
            hash.push(GEOHASH_ALPHABET[value] as char);
            bit = 0;
            value = 0;
        }
    }
    hash
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
        assert_eq!(
            narrowest_box([part(-60, -5, -50, 5), part(100, 10, 110, 20)]),
            Some(part(-60, -5, 110, 20))
        );
        assert_eq!(
            narrowest_box([part(-180, -20, -178, -16), part(177, -19, 180, -12)]),
            Some(part(177, -20, -178, -12))
        );
        assert_eq!(
            narrowest_box([
                part(-179, -18, -178, -17),
                part(176, -19, 178, -16),
                part(177, -17, 179, -15),
            ]),
            Some(part(176, -19, -178, -15))
        );
        assert_eq!(
            narrowest_box([part(-90, 0, -80, 1), part(90, 0, 100, 1)]),
            Some(part(-90, 0, 100, 1))
        );
        assert_eq!(
            narrowest_box([part(0, 0, 10, 10), part(2, 2, 3, 3), part(20, 1, 30, 2)]),
            Some(part(0, 0, 30, 10))
        );
    }

    #[test]
    fn reads_a_finite_point_and_rejects_non_point_and_non_finite() {
        assert_eq!(
            point_lon_lat(&point_geometry(-79.0, 43.0)),
            Some([-79.0, 43.0])
        );
        assert_eq!(point_lon_lat(&point_geometry(f64::NAN, 43.0)), None);
        let line = Geometry::new(GeometryValue::LineString {
            coordinates: vec![vec![0.0, 0.0].into(), vec![1.0, 1.0].into()],
        });
        assert_eq!(point_lon_lat(&line), None);
    }

    #[test]
    fn a_geohash_of_twelve_characters_names_a_point_to_centimetres() {
        assert_eq!(geohash(42.483_939, -71.207_799, 12), "drt3etunfh0x");
        assert_eq!(geohash(42.682_242, -73.704_443, 12), "dredff441fxf");
    }
}
