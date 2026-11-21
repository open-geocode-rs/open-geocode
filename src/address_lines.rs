//! A result's address as the lines of a postal address, top to bottom, laid
//! out by the address template of the country it lies in.

use std::{cell::Cell, collections::BTreeMap, sync::OnceLock};

use crate::{
    address_template::{self, Node, Piece},
    api::{Address, Context, ContextArea},
    labels,
    record::Layer,
};

/// A part of an address og can state, which a template field asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Unit,
    HouseNumber,
    Street,
    Place,
    Sublocality,
    Neighbourhood,
    Locality,
    District,
    DistrictCode,
    Subregion,
    Region,
    RegionCode,
    Postcode,
    Country,
}

const PARTS: usize = Part::Country as usize + 1;

impl Part {
    /// The part a template field of this name takes. `None` for a field og
    /// has nothing for, which stays empty.
    fn of(name: &str) -> Option<Self> {
        Some(match name {
            // The template's line for what stands inside the plot the house
            // number names, which is where a unit is written.
            "house" => Self::Unit,
            "house_number" => Self::HouseNumber,
            "road" => Self::Street,
            "place" => Self::Place,
            "suburb" | "city_district" => Self::Sublocality,
            "neighbourhood" | "quarter" => Self::Neighbourhood,
            // og keeps one locality and not whether it is a city, a town or
            // a village. `city` alone takes it, as a template may write a
            // village and a city on two lines.
            "city" => Self::Locality,
            "county" => Self::District,
            "county_code" => Self::DistrictCode,
            "state_district" => Self::Subregion,
            "state" => Self::Region,
            "state_code" => Self::RegionCode,
            "postcode" => Self::Postcode,
            "country" => Self::Country,
            _ => return None,
        })
    }
}

type Template = Vec<Node<Option<Part>>>;

fn templates() -> &'static BTreeMap<String, Template> {
    static TABLE: OnceLock<BTreeMap<String, Template>> = OnceLock::new();
    TABLE.get_or_init(|| address_template::table(Part::of))
}

/// An area's ISO 3166-2 code without its country, as `ON` of `CA-ON`.
fn short_code<'a>(area: Option<&'a ContextArea>, country: &str) -> Option<&'a str> {
    area?
        .code
        .as_deref()?
        .strip_prefix(country)?
        .strip_prefix('-')
        .filter(|code| !code.is_empty())
}

/// The value of each part, which is what the record states, or else the
/// area the record lies in.
fn parts<'a>(
    address: &'a Address,
    context: &'a Context,
    country: &'a ContextArea,
    country_code: &str,
) -> [Option<String>; PARTS] {
    let name = |area: &'a Option<ContextArea>| area.as_ref().map(|area| area.name.as_str());
    let region_code = short_code(context.region.as_ref(), country_code);
    // A stated region that is not the one the record lies in has no code og
    // knows.
    let region_agrees = address.region.as_deref().is_none_or(|stated| {
        [name(&context.region), region_code]
            .into_iter()
            .flatten()
            .any(|known| known.eq_ignore_ascii_case(stated))
    });
    // Stated areas do not say which tier each is. The outermost is written
    // where the template writes the area below the locality.
    let stated_area = address.areas.first().map(String::as_str);
    let mut values: [Option<String>; PARTS] = Default::default();
    for (part, value) in [
        (Part::HouseNumber, address.housenumber.as_deref()),
        (Part::Street, address.street.as_deref()),
        (Part::Place, address.place.as_deref()),
        (
            Part::Sublocality,
            stated_area.or_else(|| name(&context.sublocality)),
        ),
        (
            Part::Neighbourhood,
            if stated_area.is_some() {
                None
            } else {
                name(&context.neighbourhood)
            },
        ),
        (
            Part::Locality,
            address
                .locality
                .as_deref()
                .or_else(|| name(&context.locality)),
        ),
        (Part::District, name(&context.district)),
        (
            Part::DistrictCode,
            short_code(context.district.as_ref(), country_code),
        ),
        (Part::Subregion, name(&context.subregion)),
        (
            Part::Region,
            address.region.as_deref().or_else(|| name(&context.region)),
        ),
        (Part::RegionCode, region_code.filter(|_| region_agrees)),
        (Part::Postcode, address.postcode.as_deref()),
        (Part::Country, Some(country.name.as_str())),
    ] {
        values[part as usize] = value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
    }
    values[Part::Unit as usize] = address.unit.as_deref().map(labels::unit_label);
    values
}

/// What joins two values of a line, which is the template's text between
/// them. Where
/// a field between them is empty, only the first separator stays, so
/// `Toronto, {state} M5H 1A1` reads `Toronto, M5H 1A1`.
fn separator(gap: &[&str], skipped: bool) -> String {
    let squeeze = |text: &str| {
        let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let lead = if text.starts_with(char::is_whitespace) {
            " "
        } else {
            ""
        };
        let trail = if text.ends_with(char::is_whitespace) && !words.is_empty() {
            " "
        } else {
            ""
        };
        format!("{lead}{words}{trail}")
    };
    if !skipped {
        return squeeze(&gap.concat());
    }
    match gap.iter().find(|text| !text.trim().is_empty()) {
        Some(text) => squeeze(text),
        None if gap.is_empty() => String::new(),
        None => " ".to_string(),
    }
}

/// One line being written.
#[derive(Default)]
struct Line<'a> {
    text: String,
    /// The template's text since the last value.
    gap: Vec<&'a str>,
    /// Whether a field since the last value was empty.
    skipped: bool,
}

impl<'a> Line<'a> {
    fn value(&mut self, value: &str) {
        if !self.text.is_empty() {
            self.text.push_str(&separator(&self.gap, self.skipped));
        }
        self.gap.clear();
        self.skipped = false;
        self.text.push_str(value);
    }

    /// Ends the line. Text after its last value stays only when no empty
    /// field follows it (`{road} {house_number}.`).
    fn end(&mut self, lines: &mut Vec<String>) {
        let mut line = std::mem::take(self);
        if !line.text.is_empty() {
            if !line.skipped {
                line.text.push_str(&separator(&line.gap, false));
            }
            lines.push(line.text.trim().to_string());
        }
    }
}

fn render(template: &Template, values: &[Option<String>; PARTS]) -> Vec<String> {
    // A part is written once, where the template first asks for it. Some
    // templates name a field twice, or fall back to one written above.
    let written = Cell::new(0u32);
    let value = |part: &Option<Part>| {
        (*part)
            .filter(|part| written.get() & (1 << *part as u32) == 0)
            .and_then(|part| values[part as usize].as_deref().map(|value| (part, value)))
    };
    let mut lines = Vec::new();
    let mut line = Line::default();
    address_template::write(
        template,
        &|part| value(part).is_some(),
        &mut |piece| match piece {
            Piece::Text(text) => {
                for (index, text) in text.split('\n').enumerate() {
                    if index > 0 {
                        line.end(&mut lines);
                    }
                    line.gap.push(text);
                }
            }
            Piece::Field(part) => match value(part) {
                Some((part, value)) => {
                    written.set(written.get() | 1 << part as u32);
                    line.value(value);
                }
                None => line.skipped = true,
            },
            Piece::Unfilled => line.skipped = true,
        },
    );
    line.end(&mut lines);
    lines
}

/// The address of a result as postal lines, top to bottom, the way its
/// country writes an address. Empty for a result that is no address. That
/// is one of another layer than an address, an interpolation or a POI, one that
/// states neither a street nor a place its house number belongs to, an
/// interpolation's whole range, which is no one house, and one in no
/// country og has a template for.
pub(crate) fn address_lines(layer: Layer, address: &Address, context: &Context) -> Vec<String> {
    let addressed = match layer {
        Layer::Address | Layer::Interpolation => address.housenumber.is_some(),
        // A POI on a street with no number is still found by these lines.
        Layer::Poi => true,
        _ => false,
    };
    if !addressed || address.street.is_none() && address.place.is_none() {
        return Vec::new();
    }
    let Some(country) = &context.country else {
        return Vec::new();
    };
    let Some(code) = &country.code else {
        return Vec::new();
    };
    // The default row `ZZ` is no country's code, so no result selects it.
    let Some(template) = templates().get(code.as_str()).filter(|_| code != "ZZ") else {
        return Vec::new();
    };
    render(template, &parts(address, context, country, code))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(name: &str, code: Option<&str>) -> Option<ContextArea> {
        Some(ContextArea {
            id: format!("osm:relation:{}", name.len()),
            name: name.to_string(),
            code: code.map(str::to_string),
            translations: None,
            record_id: 0,
            local_name: None,
            wikidata: None,
        })
    }

    fn some(value: &str) -> Option<String> {
        Some(value.to_string())
    }

    fn stated(number: &str, street: &str, locality: &str, postcode: &str) -> Address {
        Address {
            housenumber: some(number),
            street: some(street),
            locality: some(locality),
            postcode: some(postcode),
            ..Address::default()
        }
    }

    fn within(country: &str, code: &str) -> Context {
        Context {
            country: area(country, Some(code)),
            ..Context::default()
        }
    }

    fn lines(address: &Address, context: &Context) -> Vec<String> {
        address_lines(Layer::Address, address, context)
    }

    #[test]
    fn a_country_writes_the_number_before_the_street_or_after_it() {
        let address = stated("10", "King Street West", "Toronto", "M5H 1A1");
        let canada = Context {
            region: area("Ontario", Some("CA-ON")),
            ..within("Canada", "CA")
        };
        assert_eq!(
            lines(&address, &canada),
            ["10 King Street West", "Toronto, ON M5H 1A1", "Canada"]
        );
        let address = stated("12", "Unter den Linden", "Berlin", "10117");
        assert_eq!(
            lines(&address, &within("Deutschland", "DE")),
            ["Unter den Linden 12", "10117 Berlin", "Deutschland"]
        );
        let address = stated("3", "Calle de Alcalá", "Madrid", "28014");
        assert_eq!(
            lines(&address, &within("España", "ES")),
            ["Calle de Alcalá, 3", "28014 Madrid", "España"]
        );
    }

    #[test]
    fn a_country_writes_the_postcode_before_the_town_after_it_or_below_it() {
        let address = stated("2", "Avenue de Monte-Carlo", "Monaco", "98000");
        assert_eq!(
            lines(&address, &within("Monaco", "MC")),
            ["2 Avenue de Monte-Carlo", "98000 Monaco", "Monaco"]
        );
        let address = stated("40", "Rosedale Road", "Auckland", "0632");
        assert_eq!(
            lines(&address, &within("New Zealand", "NZ")),
            ["40 Rosedale Road", "Auckland 0632", "New Zealand"]
        );
        let address = stated("10", "Downing Street", "London", "SW1A 2AA");
        assert_eq!(
            lines(&address, &within("United Kingdom", "GB")),
            ["10 Downing Street", "London", "SW1A 2AA", "United Kingdom"]
        );
    }

    #[test]
    fn a_unit_is_the_line_above_the_street() {
        let address = Address {
            unit: some("Flat 2"),
            ..stated("40", "Rosedale Road", "Auckland", "0632")
        };
        let context = within("New Zealand", "NZ");
        assert_eq!(
            lines(&address, &context),
            ["Flat 2", "40 Rosedale Road", "Auckland 0632", "New Zealand"]
        );
        let bare = Address {
            unit: some("1"),
            ..address
        };
        assert_eq!(
            lines(&bare, &context),
            ["Unit 1", "40 Rosedale Road", "Auckland 0632", "New Zealand"],
            "as the label writes a bare unit"
        );
    }

    #[test]
    fn a_region_is_its_code_where_the_template_asks_for_one_and_og_knows_it() {
        let address = stated("10", "King Street West", "Toronto", "M5H 1A1");
        let canada = |region: Option<ContextArea>| Context {
            region,
            ..within("Canada", "CA")
        };
        assert_eq!(
            lines(&address, &canada(area("Ontario", None))),
            ["10 King Street West", "Toronto, Ontario M5H 1A1", "Canada"],
            "no code, so the name"
        );
        assert_eq!(
            lines(&address, &canada(area("Ontario", Some("US-ON")))),
            ["10 King Street West", "Toronto, Ontario M5H 1A1", "Canada"],
            "a code of another country is not this region's short code"
        );
        assert_eq!(
            lines(&address, &canada(None)),
            ["10 King Street West", "Toronto, M5H 1A1", "Canada"],
            "no region leaves one separator"
        );
        let in_ontario = canada(area("Ontario", Some("CA-ON")));
        for (region, line) in [
            ("ON", "Toronto, ON M5H 1A1"),
            ("ontario", "Toronto, ON M5H 1A1"),
            ("Québec", "Toronto, Québec M5H 1A1"),
        ] {
            let address = Address {
                region: some(region),
                ..address.clone()
            };
            assert_eq!(
                lines(&address, &in_ontario),
                ["10 King Street West", line, "Canada"],
                "stated {region}"
            );
        }
        let rome = Context {
            district: area("Roma", Some("IT-RM")),
            ..within("Italia", "IT")
        };
        assert_eq!(
            lines(&stated("1", "Via del Corso", "Roma", "00186"), &rome),
            ["Via del Corso 1", "00186 Roma RM", "Italia"]
        );
    }

    #[test]
    fn a_part_the_record_does_not_state_comes_from_its_context() {
        let address = Address {
            housenumber: some("12"),
            street: some("Ponsonby Road"),
            ..Address::default()
        };
        let context = Context {
            neighbourhood: area("Three Lamps", None),
            sublocality: area("Ponsonby", None),
            locality: area("Auckland", None),
            ..within("New Zealand", "NZ")
        };
        assert_eq!(
            lines(&address, &context),
            ["12 Ponsonby Road", "Ponsonby", "Auckland", "New Zealand"]
        );
        let stated = Address {
            areas: vec!["Freemans Bay".to_string(), "Block 3".to_string()],
            locality: some("Tāmaki Makaurau"),
            ..address
        };
        assert_eq!(
            lines(&stated, &context),
            [
                "12 Ponsonby Road",
                "Freemans Bay",
                "Tāmaki Makaurau",
                "New Zealand"
            ],
            "what the record states wins"
        );
        let brazil = Context {
            neighbourhood: area("Bela Vista", None),
            locality: area("São Paulo", None),
            region: area("São Paulo", Some("BR-SP")),
            ..within("Brasil", "BR")
        };
        let address = Address {
            housenumber: some("1578"),
            street: some("Avenida Paulista"),
            postcode: some("01310-200"),
            ..Address::default()
        };
        assert_eq!(
            lines(&address, &brazil),
            [
                "Avenida Paulista, 1578, Bela Vista",
                "São Paulo - SP",
                "01310-200",
                "Brasil"
            ]
        );
    }

    #[test]
    fn text_around_an_empty_field_is_dropped_with_it() {
        let hungary = within("Magyarország", "HU");
        assert_eq!(
            lines(&stated("10", "Váci utca", "Budapest", "1052"), &hungary),
            ["1052 Budapest", "Váci utca 10.", "Magyarország"]
        );
        let no_number = Address {
            housenumber: None,
            postcode: None,
            ..stated("10", "Váci utca", "Budapest", "1052")
        };
        assert_eq!(
            address_lines(Layer::Poi, &no_number, &hungary),
            ["Budapest", "Váci utca", "Magyarország"]
        );
        let bangladesh = within("Bangladesh", "BD");
        let address = Address {
            postcode: None,
            ..stated("5", "Mirpur Road", "Dhaka", "1205")
        };
        assert_eq!(
            lines(&address, &bangladesh),
            ["5 Mirpur Road", "Dhaka", "Bangladesh"]
        );
        let place = Address {
            housenumber: some("7"),
            place: some("Oberdorf"),
            ..Address::default()
        };
        assert_eq!(
            lines(&place, &within("Deutschland", "DE")),
            ["Oberdorf 7", "Deutschland"]
        );
    }

    #[test]
    fn a_part_is_written_once_where_a_template_asks_for_it_twice() {
        let address = Address {
            unit: some("Flat 2"),
            ..stated("4", "Norzin Lam", "Thimphu", "11001")
        };
        assert_eq!(
            lines(&address, &within("Bhutan", "BT")),
            ["Flat 2", "Norzin Lam 4", "Thimphu 11001", "Bhutan"]
        );
        let address = Address {
            housenumber: some("8"),
            street: some("Rizal Avenue"),
            areas: vec!["Poblacion".to_string()],
            ..Address::default()
        };
        assert_eq!(
            lines(&address, &within("Philippines", "PH")),
            ["8 Rizal Avenue, Poblacion", "Philippines"]
        );
    }

    #[test]
    fn only_an_address_in_a_country_with_a_template_has_lines() {
        let address = stated("10", "King Street West", "Toronto", "M5H 1A1");
        let canada = within("Canada", "CA");
        let expected = ["10 King Street West", "Toronto, M5H 1A1", "Canada"];
        for layer in Layer::ALL {
            let got = address_lines(layer, &address, &canada);
            if matches!(layer, Layer::Address | Layer::Interpolation | Layer::Poi) {
                assert_eq!(got, expected, "{layer:?}");
            } else {
                assert_eq!(got, Vec::<String>::new(), "{layer:?}");
            }
        }
        let none = Vec::<String>::new();
        assert_eq!(lines(&address, &Context::default()), none, "no country");
        let uncoded = Context {
            country: area("Canada", None),
            ..Context::default()
        };
        assert_eq!(lines(&address, &uncoded), none, "no country code");
        for code in ["XX", "ZZ", "ca", ""] {
            assert_eq!(lines(&address, &within("Nowhere", code)), none, "{code:?}");
        }
        let unplaced = Address {
            street: None,
            ..address.clone()
        };
        assert_eq!(lines(&unplaced, &canada), none, "no street and no place");
        let unnumbered = Address {
            housenumber: None,
            ..address
        };
        for (layer, expected) in [
            (Layer::Address, none.clone()),
            (Layer::Interpolation, none.clone()),
            (
                Layer::Poi,
                vec![
                    "King Street West".to_string(),
                    "Toronto, M5H 1A1".to_string(),
                    "Canada".to_string(),
                ],
            ),
        ] {
            assert_eq!(
                address_lines(layer, &unnumbered, &canada),
                expected,
                "{layer:?}"
            );
        }
        assert_eq!(lines(&Address::default(), &canada), none);
    }

    #[test]
    fn every_template_writes_each_part_at_most_once_and_no_empty_line() {
        let address = Address {
            unit: some("Flat 2"),
            areas: vec!["Suburb".to_string()],
            region: some("State"),
            ..stated("1", "Street", "Locality", "Postcode")
        };
        for (code, template) in templates() {
            let context = Context {
                district: area("County", Some(&format!("{code}-CO"))),
                subregion: area("Area", None),
                region: area("State", Some(&format!("{code}-ST"))),
                ..within("Country", code)
            };
            let country = context.country.as_ref().expect("country");
            let lines = render(template, &parts(&address, &context, country, code));
            assert!(!lines.is_empty(), "{code}");
            for line in &lines {
                assert_eq!(line.trim(), line, "{code}");
                assert!(!line.is_empty() && !line.contains("  "), "{code} {line:?}");
            }
            let text = lines.join("\n");
            for value in [
                "Flat 2", "Street", "Suburb", "Locality", "County", "CO", "Area", "State", "ST",
                "Postcode", "Country",
            ] {
                let count = text.matches(value).count();
                assert!(count <= 1, "{code} writes {value} {count} times: {lines:?}");
            }
        }
    }
}
