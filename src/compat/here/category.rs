use serde::Serialize;

use super::Gs7Error;
use crate::categories;

struct Category {
    id: &'static str,
    name: &'static str,
    og: &'static [&'static str],
}

const fn category(id: &'static str, name: &'static str, og: &'static [&'static str]) -> Category {
    Category { id, name, og }
}

const CATEGORIES: &[Category] = &[
    category("100-1000-0000", "Restaurant", &["amenity restaurant"]),
    category("100-1000-0009", "Fast Food", &["amenity fast_food"]),
    category("100-1100-0010", "Coffee Shop", &["amenity cafe"]),
    category(
        "200-2000-0011",
        "Bar or Pub",
        &["amenity bar", "amenity pub"],
    ),
    category("200-2000-0012", "Night Club", &["amenity nightclub"]),
    category("200-2100-0019", "Cinema", &["amenity cinema"]),
    category("200-2200-0020", "Performing Arts", &["amenity theatre"]),
    category("200-2300-0021", "Casino", &["amenity casino"]),
    category(
        "300-3000-0023",
        "Tourist Attraction",
        &["tourism attraction"],
    ),
    category("300-3000-0024", "Gallery", &["tourism gallery"]),
    category("300-3100-0000", "Museum", &["tourism museum"]),
    category(
        "300-3200-0000",
        "Religious Place",
        &["amenity place_of_worship"],
    ),
    category("400-4000-4581", "Airport", &["aeroway aerodrome"]),
    category("400-4100-0035", "Train Station", &["railway station"]),
    category("400-4100-0036", "Bus Station", &["amenity bus_station"]),
    category("500-5000-0053", "Hotel", &["tourism hotel"]),
    category("500-5000-0054", "Motel", &["tourism motel"]),
    category("500-5100-0055", "Hostel", &["tourism hostel"]),
    category("500-5100-0056", "Campground", &["tourism camp_site"]),
    category("500-5100-0057", "Guest House", &["tourism guest_house"]),
    category("550-5510-0202", "Park-Recreation Area", &["leisure park"]),
    category("550-5520-0207", "Amusement Park", &["tourism theme_park"]),
    category("550-5520-0208", "Zoo", &["tourism zoo"]),
    category("600-6000-0061", "Convenience Store", &["shop convenience"]),
    category("600-6100-0062", "Shopping Mall", &["shop mall"]),
    category(
        "600-6200-0063",
        "Department Store",
        &["shop department_store"],
    ),
    category("600-6300-0066", "Grocery", &["shop supermarket"]),
    category(
        "600-6300-0244",
        "Bakery and Baked Goods Store",
        &["shop bakery"],
    ),
    category("600-6300-0363", "Butcher", &["shop butcher"]),
    category("600-6400-0070", "Pharmacy", &["amenity pharmacy"]),
    category("600-6700-0087", "Bookstore", &["shop books"]),
    category("600-6900-0355", "Florist", &["shop florist"]),
    category("700-7000-0107", "Bank", &["amenity bank"]),
    category("700-7300-0111", "Police Station", &["amenity police"]),
    category(
        "700-7300-0113",
        "Fire Department",
        &["amenity fire_station"],
    ),
    category("700-7450-0114", "Post Office", &["amenity post_office"]),
    category(
        "700-7600-0116",
        "Petrol-Gasoline Station",
        &["amenity fuel"],
    ),
    category(
        "700-7600-0322",
        "EV Charging Station for passenger cars",
        &["amenity charging_station"],
    ),
    category("700-7850-0122", "Car Repair", &["shop car_repair"]),
    category(
        "800-8000-0154",
        "Dentist-Dental Office",
        &["amenity dentist"],
    ),
    category(
        "800-8000-0155",
        "Family-General Practice Physicians",
        &["amenity doctors"],
    ),
    category("800-8000-0159", "Hospital", &["amenity hospital"]),
    category("800-8000-0162", "Veterinarian", &["amenity veterinary"]),
    category("800-8100-0163", "City Hall", &["amenity townhall"]),
    category(
        "800-8100-0164",
        "Embassy",
        &["amenity embassy", "office diplomatic"],
    ),
    category("800-8100-0170", "Courthouse", &["amenity courthouse"]),
    category(
        "800-8200-0173",
        "Higher Education",
        &["amenity college", "amenity university"],
    ),
    category("800-8250-0000", "School", &["amenity school"]),
    category("800-8300-0175", "Library", &["amenity library"]),
    category("800-8500-0178", "Parking Lot", &["amenity parking"]),
    category("800-8600-0182", "Swimming Pool", &["leisure swimming_pool"]),
    category(
        "800-8600-0191",
        "Fitness-Health Club",
        &["leisure fitness_centre"],
    ),
    category("800-8600-0193", "Golf Course", &["leisure golf_course"]),
    category("400-4100-0042", "Bus Stop", &["highway bus_stop"]),
    category(
        "600-6600-0077",
        "Home Improvement",
        &["shop doityourself", "shop hardware"],
    ),
    category("600-6600-0080", "Furniture Store", &["shop furniture"]),
    category(
        "600-6800-0000",
        "Clothing and Accessories",
        &["shop clothes"],
    ),
    category(
        "600-6900-0094",
        "Sporting Goods Store",
        &["shop sports", "shop outdoor"],
    ),
    category("600-6900-0097", "Pet Supply", &["shop pet"]),
    category("600-6900-0103", "Gift, Antique and Art", &["shop gift"]),
    category(
        "600-6900-0246",
        "Bicycle and Bicycle Accessories Shop",
        &["shop bicycle"],
    ),
    category("600-6900-0356", "Jeweler", &["shop jewelry"]),
    category(
        "600-6950-0000",
        "Hair and Beauty",
        &["shop beauty", "shop hairdresser"],
    ),
    category(
        "700-7400-0133",
        "Travel Agent-Ticketing",
        &["shop travel_agency", "office travel_agent"],
    ),
    category(
        "700-7460-0115",
        "Tourist Information",
        &["tourism information"],
    ),
    category(
        "700-7851-0117",
        "Rental Car Agency",
        &["amenity car_rental"],
    ),
    category(
        "800-8000-0158",
        "Medical Services-Clinics",
        &["amenity clinic"],
    ),
    category(
        "800-8100-0169",
        "Civic-Community Center",
        &["amenity community_centre"],
    ),
    category("800-8100-0171", "Government Office", &["office government"]),
];

const GROUPS: &[Category] = &[
    category("550-5520-0000", "Leisure", &["leisure"]),
    category("600-6900-0000", "Consumer Goods", &["shop"]),
    category("700-7200-0000", "Commercial Services", &["office"]),
    category("700-7400-0000", "Consumer Services", &["craft"]),
    category(
        "800-8000-0000",
        "Hospital or Health Care Facility",
        &["healthcare"],
    ),
    category("900-9300-0000", "Building", &["building"]),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct ItemCategory {
    pub(super) id: &'static str,
    name: &'static str,
    /// Sent only when `true`, as HERE sends it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    primary: bool,
}

impl ItemCategory {
    pub(super) fn listed(entries: Vec<(&'static str, &'static str)>) -> Vec<Self> {
        entries
            .into_iter()
            .enumerate()
            .map(|(index, (id, name))| Self {
                id,
                name,
                primary: index == 0,
            })
            .collect()
    }
}

pub(super) fn of_place(category: &str) -> Vec<ItemCategory> {
    let found = |table: &'static [Category]| {
        table
            .iter()
            .filter(|listed| categories::holds(listed.og, category))
            .collect::<Vec<_>>()
    };
    let mut listed = found(CATEGORIES);
    if listed.is_empty() {
        listed = found(GROUPS);
    }
    ItemCategory::listed(
        listed
            .into_iter()
            .map(|listed| (listed.id, listed.name))
            .collect(),
    )
}

pub(super) fn begun_by(
    typed: &str,
    language: Option<&str>,
) -> Vec<(&'static str, &'static str, usize, usize)> {
    let typed = typed.trim().to_lowercase();
    let length = typed.chars().count();
    if length < crate::search::MIN_TYPO_CHARS || typed.contains(char::is_whitespace) {
        return Vec::new();
    }
    let english = language.is_none_or(|code| code == "en" || code.starts_with("en-"));
    let mut found = Vec::new();
    for listed in CATEGORIES {
        let name = match english {
            true => Some(listed.name),
            false => language
                .zip(listed.og.first())
                .and_then(|(code, kind)| categories::title(kind, code)),
        };
        let Some(name) = name else {
            continue;
        };
        let mut at = 0;
        for word in name.split(|c: char| c.is_whitespace() || c == '-') {
            if word.to_lowercase().starts_with(&typed) {
                found.push((listed.id, name, at, at + length));
                break;
            }
            at += word.chars().count() + 1;
        }
    }
    found.dedup_by_key(|(_, name, _, _)| *name);
    found
}

/// Whether `asked`, an id of any level, names the category `id`. A level 3
/// id ending in `0000` is the general kind of its level 2, so it names every
/// kind under that.
fn names(asked: &str, id: &str) -> bool {
    let general = asked
        .strip_suffix("-0000")
        .filter(|level_2| level_2.len() == 8);
    id == asked
        || id
            .strip_prefix(general.unwrap_or(asked))
            .is_some_and(|rest| rest.starts_with('-'))
}

/// A list of ids to keep and ids to leave out, as `categories`, `chains` and
/// `foodTypes` are written. An id after `!` is left out.
#[derive(Debug, Default)]
pub(super) struct Listed {
    pub(super) included: Vec<String>,
    pub(super) excluded: Vec<String>,
}

impl Listed {
    pub(super) fn parse(value: Option<&str>) -> Self {
        let mut listed = Self::default();
        for id in value
            .into_iter()
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            match id.strip_prefix('!') {
                Some(id) => listed.excluded.push(id.trim().to_string()),
                None => listed.included.push(id.to_string()),
            }
        }
        listed
    }
}

#[derive(Debug, Default)]
pub(super) struct Filter(Listed);

impl Filter {
    pub(super) fn parse(value: Option<&str>) -> Result<Self, Gs7Error> {
        let listed = Listed::parse(value);
        let well_formed = |id: &String| {
            let mut parts = id.split('-');
            let digits =
                |part: &str, count| part.len() == count && part.bytes().all(|b| b.is_ascii_digit());
            parts.next().is_some_and(|part| digits(part, 3))
                && parts.clone().count() <= 2
                && parts.all(|part| digits(part, 4))
        };
        match listed
            .included
            .iter()
            .chain(&listed.excluded)
            .find(|id| !well_formed(id))
        {
            Some(id) => Err(Gs7Error::illegal(
                "categories",
                value.unwrap_or_default(),
                format!("'{id}' is not a category id, such as 100-1000-0009"),
            )),
            None => Ok(Self(listed)),
        }
    }

    pub(super) fn og_categories(&self) -> Vec<&'static str> {
        if self.0.included.is_empty() {
            return categories::keys();
        }
        let mut found = Vec::new();
        for listed in CATEGORIES.iter().chain(GROUPS) {
            if self.0.included.iter().any(|asked| names(asked, listed.id)) {
                for category in listed.og {
                    if !found.contains(category) {
                        found.push(*category);
                    }
                }
            }
        }
        found
    }

    pub(super) fn admits(&self, of_place: &[ItemCategory]) -> bool {
        let any = |asked: &[String]| {
            asked
                .iter()
                .any(|asked| of_place.iter().any(|category| names(asked, category.id)))
        };
        (self.0.included.is_empty() || any(&self.0.included)) && !any(&self.0.excluded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_category_is_one_og_has() {
        for listed in CATEGORIES.iter().chain(GROUPS) {
            for category in listed.og {
                assert!(categories::is_known(category), "{} {category}", listed.id);
            }
        }
    }

    #[test]
    fn a_typed_word_offers_the_categories_whose_names_it_begins() {
        assert_eq!(
            begun_by("rest", None),
            [("100-1000-0000", "Restaurant", 0, 4)]
        );
        let stores = begun_by("Store", None);
        assert!(
            stores.contains(&("600-6600-0080", "Furniture Store", 10, 15)),
            "{stores:?}"
        );
        assert!(
            stores
                .iter()
                .all(|(_, name, from, to)| &name[*from..*to] == "Store")
        );
        assert!(begun_by("res", None).is_empty(), "too short to mean a kind");
        assert!(begun_by("zzzz", None).is_empty());
        assert!(
            begun_by("bäck", Some("de"))
                .iter()
                .any(|(id, name, from, to)| *id == "600-6300-0244"
                    && name.starts_with("Bäck")
                    && (*from, *to) == (0, 4)),
            "{:?}",
            begun_by("bäck", Some("de"))
        );
    }

    #[test]
    fn an_id_of_any_level_names_the_kinds_under_it() {
        assert!(names("100", "100-1000-0009"));
        assert!(names("100-1000", "100-1000-0009"));
        assert!(names("100-1000-0009", "100-1000-0009"));
        assert!(names("100-1000-0000", "100-1000-0009"));
        assert!(names("700-7600-0000", "700-7600-0116"));
        assert!(!names("100-1100", "100-1000-0009"));
        assert!(!names("100-1000-0001", "100-1000-0009"));
        assert!(!names("10", "100-1000-0009"));
    }

    #[test]
    fn a_filter_keeps_and_leaves_out_by_id() {
        let filter = Filter::parse(Some("100, !100-1000-0009")).expect("filter");
        assert_eq!(
            filter.og_categories(),
            ["amenity restaurant", "amenity fast_food", "amenity cafe"]
        );
        assert!(filter.admits(&of_place("amenity:cafe")));
        assert!(!filter.admits(&of_place("amenity:fast_food")));
        assert!(!filter.admits(&of_place("shop:bakery")));
        assert!(!filter.admits(&of_place("craft:brewery")));
        assert_eq!(of_place("craft:brewery")[0].id, "700-7400-0000");
        assert_eq!(of_place("shop:music")[0].id, "600-6900-0000");
        assert_eq!(of_place("shop:bicycle")[0].id, "600-6900-0246");
        assert_eq!(
            of_place("shop:bicycle").len(),
            1,
            "an exact kind takes no group"
        );
        assert!(of_place("amenity:bench").is_empty());

        let any = Filter::parse(None).expect("filter");
        assert!(any.admits(&of_place("craft:brewery")));
        assert!(
            Filter::parse(Some("999-9999-9999"))
                .expect("filter")
                .og_categories()
                .is_empty()
        );
        assert!(Filter::parse(Some("coffee")).is_err());
        assert!(Filter::parse(Some("100-10")).is_err());
    }

    #[test]
    fn a_place_states_its_own_kind_as_primary() {
        let stated = serde_json::to_value(of_place("amenity:pub")).expect("JSON");
        assert_eq!(
            stated,
            serde_json::json!([{"id": "200-2000-0011", "name": "Bar or Pub", "primary": true}])
        );
        assert!(of_place("amenity:bench").is_empty());
    }
}
