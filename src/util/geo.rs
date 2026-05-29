use geojson::{Geometry, GeometryValue};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::point_geometry;

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
