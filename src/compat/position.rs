//! A query that is a position: decimal degrees (`59.89076,30.35442`) or
//! degrees, minutes and seconds with their hemispheres
//! (`59 53 30 N 30 21 11 E`, `59°53'30"N 30°21'11"E`).

/// The position a query writes, as `[lon, lat]`, latitude first as written.
pub(crate) fn parse(query: &str) -> Option<[f64; 2]> {
    let query = query.trim();
    decimal(query).or_else(|| sexagesimal(query))
}

/// `lat,lon` or `lat lon` in decimal degrees.
fn decimal(query: &str) -> Option<[f64; 2]> {
    let parts = query
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let [lat, lon] = parts[..] else {
        return None;
    };
    // A bare whole number is a house number or a postcode more often than
    // a degree.
    if !lat.contains('.') || !lon.contains('.') {
        return None;
    }
    let (lat, lon) = (lat.parse::<f64>().ok()?, lon.parse::<f64>().ok()?);
    valid(lat, lon)
}

/// Degrees, then optional minutes and seconds, then the hemisphere letter,
/// for the latitude and then the longitude.
fn sexagesimal(query: &str) -> Option<[f64; 2]> {
    let spaced = query
        .chars()
        .map(|c| match c {
            '°' | '\'' | '"' | '′' | '″' | ',' => ' ',
            c => c,
        })
        .collect::<String>();
    let mut numbers = Vec::new();
    let mut angles = Vec::new();
    for token in spaced.split_whitespace() {
        let (number, hemisphere) = match token.char_indices().last() {
            Some((at, letter)) if "NSEWnsew".contains(letter) => (&token[..at], Some(letter)),
            _ => (token, None),
        };
        if !number.is_empty() {
            numbers.push(number.parse::<f64>().ok()?);
        }
        if let Some(hemisphere) = hemisphere {
            let angle = match numbers[..] {
                [degrees] => degrees,
                [degrees, minutes] => degrees + minutes / 60.0,
                [degrees, minutes, seconds] => degrees + minutes / 60.0 + seconds / 3600.0,
                _ => return None,
            };
            let negative = "SWsw".contains(hemisphere);
            angles.push((
                hemisphere.to_ascii_uppercase(),
                if negative { -angle } else { angle },
            ));
            numbers.clear();
        }
    }
    if !numbers.is_empty() {
        return None;
    }
    match angles[..] {
        [('N' | 'S', lat), ('E' | 'W', lon)] => valid(lat, lon),
        _ => None,
    }
}

fn valid(lat: f64, lon: f64) -> Option<[f64; 2]> {
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some([lon, lat])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(got: Option<[f64; 2]>, lon: f64, lat: f64) {
        let [x, y] = got.expect("a position");
        assert!((x - lon).abs() < 1e-4 && (y - lat).abs() < 1e-4, "{x},{y}");
    }

    #[test]
    fn a_position_is_read_in_decimal_or_in_degrees_minutes_seconds() {
        near(parse("59.89076,30.35442"), 30.35442, 59.89076);
        near(parse(" -33.8651 151.2099 "), 151.2099, -33.8651);
        near(parse("59 53 30 N 30 21 11 E"), 30.353_06, 59.891_67);
        near(parse("59°53'30\"N 30°21'11\"E"), 30.353_06, 59.891_67);
        near(parse("33 52 S 151 12 E"), 151.2, -33.866_67);
        near(parse("48 52 9 N 2 19 56 W"), -2.332_22, 48.869_17);
    }

    #[test]
    fn an_address_or_a_name_is_no_position() {
        for query in [
            "10 King Street",
            "59 53",
            "75002 Paris",
            "1 2",
            "91.0,10.0",
            "N 59 53",
            "59 N 60 N",
        ] {
            assert_eq!(parse(query), None, "{query}");
        }
    }
}
