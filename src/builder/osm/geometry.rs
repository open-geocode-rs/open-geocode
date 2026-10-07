use geo::{Area, BoundingRect, Centroid, LineString, Point, Polygon, Rect};
use geojson::{Bbox, Geometry, GeometryValue};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NonPointGeometry {
    pub geometry: Geometry,
    pub representative_point: [f64; 2],
}

pub(crate) fn centroid(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    if points.is_empty() {
        return None;
    }

    closed_way_polygon_geometry(points)
        .or_else(|| line_string_geometry(points))
        .map(|built| lat_lon_from_position(built.representative_point))
        .or_else(|| average_point(points))
}

pub(crate) fn line_string_geometry(points: &[(f64, f64)]) -> Option<NonPointGeometry> {
    let line_string = line_string(points)?;
    let mut geometry = Geometry::new(GeometryValue::from(&line_string));
    geometry.bbox = line_string.bounding_rect().map(bbox_from_rect);
    Some(NonPointGeometry {
        geometry,
        representative_point: line_string.centroid().map(position_from_point)?,
    })
}

pub(crate) fn closed_way_polygon_geometry(points: &[(f64, f64)]) -> Option<NonPointGeometry> {
    let polygon = closed_way_polygon(points)?;
    let mut geometry = Geometry::new(GeometryValue::from(&polygon));
    geometry.bbox = polygon.bounding_rect().map(bbox_from_rect);
    Some(NonPointGeometry {
        geometry,
        representative_point: polygon.centroid().map(position_from_point)?,
    })
}

fn average_point(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let count = points.len() as f64;
    let lat = points.iter().map(|(lat, _)| lat).sum::<f64>() / count;
    let lon = points.iter().map(|(_, lon)| lon).sum::<f64>() / count;
    Some((lat, lon))
}

fn line_string(points: &[(f64, f64)]) -> Option<LineString<f64>> {
    if points.len() < 2 {
        return None;
    }
    Some(LineString::from(
        points
            .iter()
            .map(|(lat, lon)| (*lon, *lat))
            .collect::<Vec<_>>(),
    ))
}

fn closed_way_polygon(points: &[(f64, f64)]) -> Option<Polygon<f64>> {
    if points.len() < 4 || points.first() != points.last() {
        return None;
    }

    let polygon = Polygon::new(line_string(points)?, vec![]);
    if polygon.unsigned_area() <= f64::EPSILON {
        return None;
    }
    Some(polygon)
}

fn bbox_from_rect(rect: Rect<f64>) -> Bbox {
    vec![rect.min().x, rect.min().y, rect.max().x, rect.max().y]
}

fn position_from_point(point: Point<f64>) -> [f64; 2] {
    [point.x(), point.y()]
}

fn lat_lon_from_position(position: [f64; 2]) -> (f64, f64) {
    (position[1], position[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_closed_way_centroid() {
        let points = [(0.0, 0.0), (0.0, 2.0), (2.0, 2.0), (2.0, 0.0), (0.0, 0.0)];

        let (lat, lon) = centroid(&points).expect("centroid");

        assert!((lat - 1.0).abs() < 0.000001);
        assert!((lon - 1.0).abs() < 0.000001);
    }

    #[test]
    fn builds_line_string_geometry_with_bbox_and_representative_point() {
        let points = [(43.64, -79.41), (43.66, -79.36)];

        let built = line_string_geometry(&points).expect("line geometry");

        assert_eq!(
            built.geometry.bbox,
            Some(vec![-79.41, 43.64, -79.36, 43.66])
        );
        assert!((built.representative_point[0] - -79.385).abs() < 0.000001);
        assert!((built.representative_point[1] - 43.65).abs() < 0.000001);
        match built.geometry.value {
            GeometryValue::LineString { coordinates } => {
                assert_eq!(coordinates.len(), 2);
                assert_eq!(coordinates[0].as_slice(), &[-79.41, 43.64]);
            }
            other => panic!("expected LineString, got {}", other.type_name()),
        }
    }

    #[test]
    fn builds_closed_way_polygon_geometry() {
        let points = [(0.0, 0.0), (0.0, 2.0), (2.0, 2.0), (2.0, 0.0), (0.0, 0.0)];

        let built = closed_way_polygon_geometry(&points).expect("polygon geometry");

        assert_eq!(built.geometry.bbox, Some(vec![0.0, 0.0, 2.0, 2.0]));
        assert!((built.representative_point[0] - 1.0).abs() < 0.000001);
        assert!((built.representative_point[1] - 1.0).abs() < 0.000001);
        match built.geometry.value {
            GeometryValue::Polygon { coordinates } => {
                assert_eq!(coordinates.len(), 1);
                assert_eq!(coordinates[0].len(), 5);
            }
            other => panic!("expected Polygon, got {}", other.type_name()),
        }
    }

    #[test]
    fn rejects_open_or_degenerate_polygons() {
        assert!(closed_way_polygon_geometry(&[(0.0, 0.0), (0.0, 2.0), (2.0, 2.0)]).is_none());
        assert!(
            closed_way_polygon_geometry(&[(0.0, 0.0), (1.0, 1.0), (2.0, 2.0), (0.0, 0.0)])
                .is_none()
        );
    }
}
