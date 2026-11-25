use crate::http::{
    param::Param,
    problem::{InvalidParam, Problem},
};
use crate::search::{BoundingBox, MAX_AUTOCOMPLETE_LIMIT};

pub(crate) const MAX_QUERY_CHARS: usize = 256;

pub(crate) const MAX_SEARCH_LIMIT: usize = 50;

pub(crate) const BATCH_ITEM_BYTES: usize = 4096;

pub(crate) const MAX_BATCH_ID_CHARS: usize = 128;

pub(crate) const UPLOAD_STALL_SECONDS: u64 = 30;

pub(crate) fn validate_query_length(raw: &str) -> Result<String, Problem> {
    let query = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(Problem::invalid_query(format!(
            "q must be at most {MAX_QUERY_CHARS} characters"
        )));
    }
    Ok(query)
}

pub(crate) fn validate_query(raw: &str) -> Result<String, Problem> {
    let query = validate_query_length(raw)?;
    if query.is_empty() {
        return Err(empty_query());
    }
    Ok(query)
}

pub(crate) fn empty_query() -> Problem {
    Problem::invalid_query("q must not be empty")
}

pub(crate) fn validate_search_limit(limit: usize) -> Result<(), Problem> {
    if limit > MAX_SEARCH_LIMIT {
        return Err(Problem::limit_too_large(vec![InvalidParam::new(
            "limit",
            format!("must be at most {MAX_SEARCH_LIMIT}"),
        )]));
    }
    Ok(())
}

pub(crate) fn validate_autocomplete_limit(limit: usize) -> Result<(), Problem> {
    if limit > MAX_AUTOCOMPLETE_LIMIT {
        return Err(Problem::limit_too_large(vec![InvalidParam::new(
            "limit",
            format!("must be at most {MAX_AUTOCOMPLETE_LIMIT}"),
        )]));
    }
    Ok(())
}

pub(crate) fn validate_coords(lon: f64, lat: f64) -> Result<(), Problem> {
    let mut invalid = Vec::new();
    if !(-180.0..=180.0).contains(&lon) {
        invalid.push(InvalidParam::new("lon", "must be within [-180, 180]"));
    }
    if !(-90.0..=90.0).contains(&lat) {
        invalid.push(InvalidParam::new("lat", "must be within [-90, 90]"));
    }
    if invalid.is_empty() {
        Ok(())
    } else {
        Err(Problem::invalid_coordinate(invalid))
    }
}

fn degrees(name: &str, value: &Param, count: usize) -> Result<Option<Vec<f64>>, Problem> {
    let value = match value {
        Param::Missing | Param::Empty => return Ok(None),
        Param::Value(value) => value.trim(),
    };
    let numbers = value
        .split(',')
        .map(|part| part.trim().parse::<f64>())
        .collect::<Result<Vec<_>, _>>()
        .ok()
        .filter(|numbers| numbers.len() == count);
    let Some(numbers) = numbers else {
        let shape = if count == 2 {
            "lon,lat"
        } else {
            "min_lon,min_lat,max_lon,max_lat"
        };
        return Err(Problem::malformed_parameter(format!(
            "{name} must be {count} comma-separated numbers, {shape}"
        ))
        .with_invalid_params(vec![InvalidParam::new(name, format!("not {shape}"))]));
    };
    let mut invalid = Vec::new();
    for pair in numbers.chunks(2) {
        if !(-180.0..=180.0).contains(&pair[0]) {
            invalid.push(InvalidParam::new(
                name,
                "longitudes must be within [-180, 180]",
            ));
        }
        if !(-90.0..=90.0).contains(&pair[1]) {
            invalid.push(InvalidParam::new(
                name,
                "latitudes must be within [-90, 90]",
            ));
        }
    }
    if invalid.is_empty() {
        Ok(Some(numbers))
    } else {
        invalid.dedup_by(|left, right| left.reason == right.reason);
        Err(Problem::invalid_coordinate(invalid))
    }
}

pub(crate) fn parse_focus(value: &Param) -> Result<Option<[f64; 2]>, Problem> {
    Ok(degrees("focus", value, 2)?.map(|numbers| [numbers[0], numbers[1]]))
}

pub(crate) fn parse_bbox(value: &Param) -> Result<Option<BoundingBox>, Problem> {
    let Some(numbers) = degrees("bbox", value, 4)? else {
        return Ok(None);
    };
    let bbox = BoundingBox {
        min_lon: numbers[0],
        min_lat: numbers[1],
        max_lon: numbers[2],
        max_lat: numbers[3],
    };
    let mut invalid = Vec::new();
    if bbox.min_lon > bbox.max_lon {
        invalid.push(InvalidParam::new("bbox", "min_lon must not exceed max_lon"));
    }
    if bbox.min_lat > bbox.max_lat {
        invalid.push(InvalidParam::new("bbox", "min_lat must not exceed max_lat"));
    }
    if invalid.is_empty() {
        Ok(Some(bbox))
    } else {
        Err(Problem::invalid_coordinate(invalid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(text: &str) -> Param {
        Param::Value(text.to_string())
    }

    #[test]
    fn focus_and_bbox_are_points_in_range() {
        assert_eq!(parse_focus(&Param::Missing).unwrap(), None);
        assert_eq!(parse_focus(&Param::Empty).unwrap(), None);
        assert_eq!(
            parse_focus(&value("-79.38, 43.65")).unwrap(),
            Some([-79.38, 43.65])
        );
        let shape = parse_focus(&value("-79.38")).unwrap_err();
        assert_eq!(shape.error_code().as_str(), "malformed_parameter");
        assert_eq!(shape.invalid_params()[0].name, "focus");
        assert!(parse_focus(&value("a,b")).is_err());
        let range = parse_focus(&value("200,43")).unwrap_err();
        assert_eq!(range.error_code().as_str(), "invalid_coordinate");
        assert!(parse_focus(&value("NaN,43")).is_err());
        assert!(parse_focus(&value("-79,inf")).is_err());

        let bbox = parse_bbox(&value("-79.6,43.5,-79.1,43.9"))
            .unwrap()
            .expect("bbox");
        assert!(bbox.contains(-79.38, 43.65));
        assert!(!bbox.contains(-80.0, 43.65));
        assert_eq!(parse_bbox(&Param::Empty).unwrap(), None);
        assert!(parse_bbox(&value("-79.6,43.5,-79.1")).is_err());
        let crossed = parse_bbox(&value("170,-10,-170,10")).unwrap_err();
        assert_eq!(crossed.error_code().as_str(), "invalid_coordinate");
        assert_eq!(
            crossed.invalid_params()[0].reason,
            "min_lon must not exceed max_lon"
        );
        assert!(parse_bbox(&value("-79,95,-78,96")).is_err());
    }

    #[test]
    fn rejects_empty_and_over_length_queries() {
        assert!(validate_query("   ").is_err());
        let long = "a".repeat(MAX_QUERY_CHARS + 1);
        assert!(validate_query(&long).is_err());
        assert_eq!(validate_query("  king street  ").unwrap(), "king street");
    }

    #[test]
    fn a_query_laid_out_with_whitespace_is_counted_by_its_words() {
        let padded = format!(
            "46 Museum Lp {}Browning{},  59417",
            " 
"
            .repeat(200),
            "	".repeat(100)
        );
        assert_eq!(
            validate_query(&padded).unwrap(),
            "46 Museum Lp Browning , 59417"
        );
    }

    #[test]
    fn query_length_counts_chars_not_bytes() {
        let at_limit = "é".repeat(MAX_QUERY_CHARS);
        assert!(validate_query(&at_limit).is_ok());
        let over = "é".repeat(MAX_QUERY_CHARS + 1);
        assert!(validate_query(&over).is_err());
    }

    #[test]
    fn search_limit_boundary() {
        assert!(validate_search_limit(MAX_SEARCH_LIMIT).is_ok());
        let problem = validate_search_limit(MAX_SEARCH_LIMIT + 1).unwrap_err();
        assert_eq!(problem.status(), 400);
        assert_eq!(problem.invalid_params()[0].name, "limit");
    }

    #[test]
    fn coordinate_ranges() {
        assert!(validate_coords(-180.0, -90.0).is_ok());
        assert!(validate_coords(180.0, 90.0).is_ok());

        let lon_bad = validate_coords(999.0, 0.0).unwrap_err();
        assert_eq!(lon_bad.invalid_params()[0].name, "lon");

        let lat_bad = validate_coords(0.0, 999.0).unwrap_err();
        assert_eq!(lat_bad.invalid_params()[0].name, "lat");

        assert!(validate_coords(f64::NAN, 0.0).is_err());
        assert!(validate_coords(0.0, f64::INFINITY).is_err());
    }
}
