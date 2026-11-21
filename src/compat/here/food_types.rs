//! HERE's food types, the cuisines a place's `foodTypes` name, and the
//! kinds of food OpenStreetMap's `cuisine` tag lists that each one is.
//!
//! A `cuisine` value is the food type of the same whole name: its words are
//! the value's with underscores as spaces, read without regard to case or
//! to a plural `s` (`ice_cream` is Ice Cream, `burger` is Burgers). A common
//! value HERE files as a variation of one country's kitchen, or spells
//! otherwise, is listed in [`NAMED`], each reviewed by hand. A part of a
//! variation's name alone names nothing: `chicken` is Chicken, not
//! Japanese-Yakitori/Chicken.

/// Common `cuisine` values and the food type each one is, where no food
/// type has the value's own name.
const NAMED: [(&str, &str); 9] = [
    ("sushi", "203-026"),
    ("ramen", "203-060"),
    ("tapas", "311-034"),
    ("kosher", "800-079"),
    ("cantonese", "201-010"),
    ("szechuan", "201-009"),
    ("sichuan", "201-009"),
    ("crepe", "800-068"),
    ("pastry", "800-069"),
];

/// Every food type, as HERE's food type system lists them: its id and name.
const FOOD_TYPES: [(&str, &str); 189] = [
    ("101-000", "American"),
    ("101-001", "American-Californian"),
    ("101-002", "American-Southwestern"),
    ("101-003", "American-Barbecue/Southern"),
    ("101-004", "American-Creole"),
    ("101-020", "American-Southern"),
    ("101-039", "American-Native American"),
    ("101-040", "American-Soul Food"),
    ("101-070", "American-Cajun"),
    ("102-000", "Mexican"),
    ("102-005", "Mexican-Yucateca"),
    ("102-006", "Mexican-Oaxaquena"),
    ("102-007", "Mexican-Veracruzana"),
    ("102-008", "Mexican-Poblana"),
    ("103-000", "Canadian"),
    ("150-000", "Australian"),
    ("151-000", "Hawaiian/Polynesian"),
    ("152-000", "Caribbean"),
    ("153-000", "Cuban"),
    ("200-000", "Asian"),
    ("201-000", "Chinese"),
    ("201-009", "Chinese-Szechuan"),
    ("201-010", "Chinese-Cantonese"),
    ("201-041", "Chinese-Shanghai"),
    ("201-042", "Chinese-Beijing"),
    ("201-043", "Chinese-Hunan/Hubei"),
    ("201-044", "Chinese-Jiangsu/Zhejiang"),
    ("201-045", "Chinese-Shandong"),
    ("201-046", "Chinese-Northeastern"),
    ("201-047", "Chinese-Inner Mongolian"),
    ("201-048", "Chinese-Yunnan/Guizhou"),
    ("201-049", "Chinese-Taiwanese"),
    ("201-050", "Chinese-Guangxi"),
    ("201-051", "Chinese-Jiangxi"),
    ("201-052", "Chinese-Northwestern"),
    ("201-053", "Chinese-Porridge"),
    ("201-054", "Chinese-Islamic"),
    ("201-055", "Chinese-Hot Pot"),
    ("202-000", "Indian"),
    ("202-011", "Indian-Tandoori"),
    ("202-012", "Indian-Punjabi"),
    ("202-013", "Indian-Rajasthani"),
    ("202-014", "Indian-Mughlai"),
    ("202-015", "Indian-Bengali"),
    ("202-016", "Indian-Goan"),
    ("202-017", "Indian-Jain"),
    ("202-018", "Indian-Konkani"),
    ("202-019", "Indian-Gujarati"),
    ("202-020", "Indian-Parsi"),
    ("202-021", "Indian-South Indian"),
    ("202-022", "Indian-Maharashtrian"),
    ("202-023", "Indian-North Indian"),
    ("202-024", "Indian-Malvani"),
    ("202-025", "Indian-Hyderabadi"),
    ("203-000", "Japanese"),
    ("203-010", "Japanese-Fish/Other Seafood"),
    ("203-011", "Japanese-Unagi/Anago"),
    ("203-026", "Japanese-Sushi"),
    ("203-027", "Japanese-Sushi Train"),
    ("203-030", "Japanese-Hibachi"),
    ("203-031", "Japanese-Yakitori/Chicken"),
    ("203-032", "Japanese-Yakiniku"),
    ("203-033", "Japanese-Jingisukan"),
    ("203-040", "Japanese-Rice Bowl"),
    ("203-045", "Japanese-Curry"),
    ("203-050", "Japanese-Tempura/Other Fried Food"),
    ("203-051", "Japanese-Okonomiyaki"),
    ("203-052", "Japanese-Takoyaki"),
    ("203-053", "Japanese-Tonkatsu"),
    ("203-060", "Japanese-Ramen"),
    ("203-061", "Japanese-Udon/Other Noodles"),
    ("203-065", "Japanese-Gyoza"),
    ("203-070", "Japanese-Sukiyaki"),
    ("203-071", "Japanese-Shabushabu"),
    ("203-072", "Japanese-Chanko"),
    ("203-080", "Japanese-Kaiseki"),
    ("203-081", "Japanese-Izakaya"),
    ("204-000", "Southeast Asian"),
    ("205-000", "Thai"),
    ("206-000", "Vietnamese"),
    ("207-000", "Korean"),
    ("208-000", "Pakistani"),
    ("209-000", "Malaysian"),
    ("210-000", "Bruneian"),
    ("211-000", "Indonesian"),
    ("212-000", "Filipino"),
    ("250-000", "Middle Eastern"),
    ("251-000", "Azerbaijani"),
    ("252-000", "Turkish"),
    ("253-000", "Lebanese"),
    ("254-000", "Yemeni"),
    ("255-000", "Burmese"),
    ("256-000", "Cambodian"),
    ("257-000", "Singaporean"),
    ("258-000", "Sri Lankan"),
    ("259-000", "Tibetan"),
    ("300-000", "European"),
    ("301-000", "French"),
    ("301-027", "French-Alsatian"),
    ("301-028", "French-Auvergnate"),
    ("301-029", "French-Basque"),
    ("301-030", "French-Corse"),
    ("301-031", "French-Lyonnaise"),
    ("301-032", "French-Provencale"),
    ("301-033", "French-Sud-ouest"),
    ("302-000", "German"),
    ("303-000", "Greek"),
    ("304-000", "Italian"),
    ("305-000", "Irish"),
    ("306-000", "Austrian"),
    ("307-000", "Belgian"),
    ("308-000", "British Isles"),
    ("309-000", "Dutch"),
    ("310-000", "Swiss"),
    ("311-000", "Spanish"),
    ("311-034", "Spanish-Tapas"),
    ("313-000", "Portuguese"),
    ("314-000", "Maltese"),
    ("315-000", "Sicilian"),
    ("350-000", "Scandinavian"),
    ("351-000", "Finnish"),
    ("352-000", "Swedish"),
    ("353-000", "Norwegian"),
    ("354-000", "Danish"),
    ("370-000", "East European"),
    ("371-000", "Hungarian"),
    ("372-000", "Mediterranean"),
    ("373-000", "Baltic"),
    ("374-000", "Belorusian"),
    ("375-000", "Ukrainian"),
    ("376-000", "Polish"),
    ("377-000", "Russian"),
    ("378-000", "Bohemian"),
    ("379-000", "Balkan"),
    ("380-000", "Caucasian"),
    ("381-000", "Romanian"),
    ("382-000", "Armenian"),
    ("400-000", "South American"),
    ("401-000", "Surinamese"),
    ("402-000", "Venezuelan"),
    ("403-000", "Latin American"),
    ("404-000", "Argentinean"),
    ("405-000", "Chilean"),
    ("406-000", "Brazilian"),
    ("406-035", "Brazilian-Baiana"),
    ("406-036", "Brazilian-Capixaba"),
    ("406-037", "Brazilian-Mineira"),
    ("406-038", "Brazilian-Bakery"),
    ("407-000", "Peruvian"),
    ("500-000", "African"),
    ("501-000", "Moroccan"),
    ("502-000", "Egyptian"),
    ("503-000", "Ethiopian"),
    ("504-000", "Seychellois"),
    ("505-000", "South African"),
    ("506-000", "North African"),
    ("600-000", "Oceanic"),
    ("800-050", "Fast Food"),
    ("800-056", "Steak House"),
    ("800-057", "Pizza"),
    ("800-058", "Snacks and Beverages"),
    ("800-059", "Hot Dogs"),
    ("800-060", "Sandwich"),
    ("800-061", "Breakfast"),
    ("800-062", "Chicken"),
    ("800-063", "Ice Cream"),
    ("800-064", "International"),
    ("800-065", "Continental"),
    ("800-066", "Fusion"),
    ("800-067", "Burgers"),
    ("800-068", "Creperie"),
    ("800-069", "Pastries"),
    ("800-071", "Fondue"),
    ("800-072", "Brunch"),
    ("800-073", "Bistro"),
    ("800-074", "BrewPub"),
    ("800-075", "Seafood"),
    ("800-076", "Vegan"),
    ("800-077", "Vegetarian"),
    ("800-078", "Grill"),
    ("800-079", "Jewish/Kosher"),
    ("800-080", "Soup"),
    ("800-081", "Lunch"),
    ("800-082", "Dinner"),
    ("800-083", "Natural/Healthy"),
    ("800-084", "Organic"),
    ("800-085", "Noodles"),
    ("800-086", "Halal"),
    ("800-087", "Kebab"),
];

/// The food types of a place's `cuisine` values, each once, in the order
/// of the values. A value no food type is named by is left out.
pub(super) fn of_cuisine(cuisine: &[String]) -> Vec<(&'static str, &'static str)> {
    let mut found = Vec::new();
    for value in cuisine {
        if let Some(food) = named(value)
            && !found.contains(&food)
        {
            found.push(food);
        }
    }
    found
}

/// Whether a place of these food types is kept by browse's `foodTypes`.
/// A base food type asked for keeps its variations too (`201-000` keeps
/// `201-009`), and one to leave out wins over one to keep.
pub(super) fn admits(
    listed: &super::category::Listed,
    of_place: &[super::category::ItemCategory],
) -> bool {
    let names = |asked: &str, id: &str| {
        asked == id
            || asked
                .strip_suffix("-000")
                .is_some_and(|base| id.split_once('-').is_some_and(|(own, _)| own == base))
    };
    let any = |asked: &[String]| {
        asked
            .iter()
            .any(|asked| of_place.iter().any(|food| names(asked, food.id)))
    };
    (listed.included.is_empty() || any(&listed.included)) && !any(&listed.excluded)
}

/// The food type `value` names: the one of its whole name, else the one
/// [`NAMED`] gives it.
fn named(value: &str) -> Option<(&'static str, &'static str)> {
    let value = value.trim().to_lowercase();
    let wanted = words(&value.replace('_', " "));
    if wanted.is_empty() {
        return None;
    }
    FOOD_TYPES
        .iter()
        .find(|(_, name)| words(name) == wanted)
        .or_else(|| {
            let (_, id) = NAMED.iter().find(|(listed, _)| *listed == value)?;
            FOOD_TYPES.iter().find(|(own, _)| own == id)
        })
        .copied()
}

/// The words of a name, in lower case, each without a plural `s`.
fn words(name: &str) -> Vec<String> {
    name.split_whitespace()
        .map(|word| {
            let word = word.to_lowercase();
            match word.strip_suffix('s') {
                Some(stem) if stem.len() > 2 => stem.to_string(),
                _ => word,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(values: &[&str]) -> Vec<&'static str> {
        of_cuisine(
            &values
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
        )
        .into_iter()
        .map(|(id, _)| id)
        .collect()
    }

    #[test]
    fn a_cuisine_is_the_food_type_of_its_whole_name() {
        assert_eq!(ids(&["pizza", "italian"]), ["800-057", "304-000"]);
        assert_eq!(ids(&["ice_cream"]), ["800-063"]);
        assert_eq!(ids(&["burger"]), ["800-067"]);
        assert_eq!(ids(&["steak_house"]), ["800-056"]);
        assert_eq!(ids(&["chinese", "Chinese"]), ["201-000"], "each once");
    }

    /// A part of a variation's name names nothing of its own: chicken is
    /// HERE's Chicken, and fish, bakery and northwestern no kitchen of one
    /// country.
    #[test]
    fn a_part_of_a_variations_name_names_no_variation() {
        assert_eq!(ids(&["chicken"]), ["800-062"]);
        for value in ["fish", "bakery", "northwestern", "coffee_shop", ""] {
            assert!(ids(&[value]).is_empty(), "{value}");
        }
    }

    /// The most used OSM `cuisine` values, each beside the food type it is,
    /// none where HERE has none of that meaning.
    #[test]
    fn the_most_used_cuisines_are_their_food_types() {
        let expected = [
            ("pizza", Some("800-057")),
            ("burger", Some("800-067")),
            ("coffee_shop", None),
            ("italian", Some("304-000")),
            ("chinese", Some("201-000")),
            ("mexican", Some("102-000")),
            ("sandwich", Some("800-060")),
            ("chicken", Some("800-062")),
            ("kebab", Some("800-087")),
            ("japanese", Some("203-000")),
            ("regional", None),
            ("indian", Some("202-000")),
            ("american", Some("101-000")),
            ("sushi", Some("203-026")),
            ("asian", Some("200-000")),
            ("thai", Some("205-000")),
            ("ice_cream", Some("800-063")),
            ("french", Some("301-000")),
            ("greek", Some("303-000")),
            ("german", Some("302-000")),
            ("vietnamese", Some("206-000")),
            ("seafood", Some("800-075")),
            ("donut", None),
            ("steak_house", Some("800-056")),
            ("turkish", Some("252-000")),
            ("korean", Some("207-000")),
            ("spanish", Some("311-000")),
            ("bakery", None),
            ("breakfast", Some("800-061")),
            ("fish_and_chips", None),
            ("mediterranean", Some("372-000")),
            ("lebanese", Some("253-000")),
            ("tapas", Some("311-034")),
            ("noodle", Some("800-085")),
            ("barbecue", None),
            ("bubble_tea", None),
            ("crepe", Some("800-068")),
            ("portuguese", Some("313-000")),
            ("vegan", Some("800-076")),
            ("vegetarian", Some("800-077")),
            ("ramen", Some("203-060")),
            ("kosher", Some("800-079")),
            ("halal", Some("800-086")),
        ];
        for (value, id) in expected {
            assert_eq!(ids(&[value]).first().copied(), id, "{value}");
        }
    }
}
