//! Distances, coordinates and URL encoding.
//!
//! Everything is on a sphere of the mean Earth radius. Against a 100 m
//! threshold the ellipsoid would move a distance by centimetres, which decides
//! no query here.

use rand::Rng;
use serde_json::Value;

pub const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// Great-circle distance in metres between two lon/lat points.
pub fn haversine_m(lon1: f64, lat1: f64, lon2: f64, lat2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * a.sqrt().min(1.0).asin()
}

/// A point drawn uniformly from the disc of `radius_m` around `(lon, lat)`.
///
/// The square root on the radius is what makes it uniform over the area
/// rather than bunched at the centre.
pub fn jitter(lon: f64, lat: f64, radius_m: f64, rng: &mut impl Rng) -> (f64, f64) {
    let r = radius_m * rng.random::<f64>().sqrt();
    let theta = std::f64::consts::TAU * rng.random::<f64>();
    let (dx, dy) = (r * theta.cos(), r * theta.sin());
    let dlat = (dy / EARTH_RADIUS_M).to_degrees();
    let dlon = (dx / (EARTH_RADIUS_M * lat.to_radians().cos())).to_degrees();
    (lon + dlon, lat + dlat)
}

/// The `[lon, lat]` pair at `pointer` in a response body, if it is there.
pub fn coords_at(body: &Value, pointer: &str) -> Option<(f64, f64)> {
    let pair = body.pointer(pointer)?.as_array()?;
    let lon = pair.first()?.as_f64()?;
    let lat = pair.get(1)?.as_f64()?;
    (lon.is_finite() && lat.is_finite()).then_some((lon, lat))
}

/// Rounds to 7 decimal places, about a centimetre: the precision OSM stores.
pub fn round7(x: f64) -> f64 {
    (x * 1e7).round() / 1e7
}

/// Percent-encodes a query-string value. Unreserved characters stay as they
/// are and every other byte, space included, becomes `%XX`, so the same text
/// reaches every engine byte for byte.
pub fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    #[test]
    fn haversine_known_distances() {
        assert_eq!(haversine_m(4.9, 52.37, 4.9, 52.37), 0.0);
        // One degree of latitude on this sphere.
        let d = haversine_m(5.0, 52.0, 5.0, 53.0);
        assert!((d - 111_195.08).abs() < 0.1, "{d}");
        // A quarter of the equator, and a degree of longitude at 60 N is half
        // a degree at the equator.
        let d = haversine_m(0.0, 0.0, 90.0, 0.0);
        assert!(
            (d - std::f64::consts::FRAC_PI_2 * EARTH_RADIUS_M).abs() < 1e-6,
            "{d}"
        );
        let eq = haversine_m(0.0, 0.0, 0.001, 0.0);
        let sixty = haversine_m(0.0, 60.0, 0.001, 60.0);
        assert!((sixty - eq / 2.0).abs() < 1e-6, "{sixty} {eq}");
        // Symmetric.
        assert_eq!(
            haversine_m(4.1, 51.2, 6.3, 53.1),
            haversine_m(6.3, 53.1, 4.1, 51.2)
        );
    }

    #[test]
    fn jitter_stays_inside_the_disc_and_is_seeded() {
        let mut a = ChaCha8Rng::seed_from_u64(7);
        let mut b = ChaCha8Rng::seed_from_u64(7);
        let mut far = 0.0f64;
        for _ in 0..10_000 {
            let (lon, lat) = jitter(5.0, 52.0, 50.0, &mut a);
            assert_eq!((lon, lat), jitter(5.0, 52.0, 50.0, &mut b));
            far = far.max(haversine_m(5.0, 52.0, lon, lat));
        }
        assert!(far <= 50.01 && far > 45.0, "{far}");
    }

    #[test]
    fn coords_from_pointer() {
        let body: Value = serde_json::json!({
            "features": [{"geometry": {"coordinates": [4.9, 52.37]}}]
        });
        assert_eq!(
            coords_at(&body, "/features/0/geometry/coordinates"),
            Some((4.9, 52.37))
        );
        assert_eq!(coords_at(&body, "/features/1/geometry/coordinates"), None);
        let empty: Value = serde_json::json!({"features": []});
        assert_eq!(coords_at(&empty, "/features/0/geometry/coordinates"), None);
        let text: Value =
            serde_json::json!({"features": [{"geometry": {"coordinates": ["4.9", "52"]}}]});
        assert_eq!(coords_at(&text, "/features/0/geometry/coordinates"), None);
    }

    #[test]
    fn encodes_query_text() {
        assert_eq!(
            encode_component("Damrak 1, Amsterdam"),
            "Damrak%201%2C%20Amsterdam"
        );
        assert_eq!(encode_component("'s-Gravenhage"), "%27s-Gravenhage");
        assert_eq!(encode_component("Zuiderdiep 1ª"), "Zuiderdiep%201%C2%AA");
        assert_eq!(
            encode_component("a&b=c/d?e#f+g"),
            "a%26b%3Dc%2Fd%3Fe%23f%2Bg"
        );
    }
}
