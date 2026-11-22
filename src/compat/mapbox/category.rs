//! Search Box v1 category search: `category/{id}` and `list/category`.
//!
//! og knows a place's kind as its map tag (`amenity:cafe`). The list below
//! gives the kinds callers ask for most a canonical id, a name and an icon,
//! and is what `list/category` answers. `category/{id}` also takes any id
//! whose words name a kind in og's own catalog, such as `swimming_pool`.

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
};
use serde::Serialize;

use super::{Api, Params, error, searchbox};
use crate::{
    categories,
    compat::{Args, VendorError, native},
    runtime::AppState,
};

/// The most results one category search answers.
const MAX_LIMIT: usize = 25;
/// Metres to a degree of latitude, for `radius`, which is in degrees.
const METRES_PER_DEGREE: f64 = 111_320.0;

/// A kind of place under a canonical id.
struct Category {
    id: &'static str,
    name: &'static str,
    /// The Maki icon.
    icon: &'static str,
    /// og's categories of the kind, as the catalog writes them.
    og: &'static [&'static str],
}

/// A kind after every kind it holds, so a place lists its closest kind first.
const CATEGORIES: &[Category] = &[
    category("coffee", "Coffee", "cafe", &["amenity cafe"]),
    category("cafe", "Cafe", "cafe", &["amenity cafe"]),
    category("coffee_shop", "Coffee Shop", "cafe", &["amenity cafe"]),
    category(
        "restaurant",
        "Restaurant",
        "restaurant",
        &["amenity restaurant"],
    ),
    category(
        "fast_food",
        "Fast Food",
        "fast-food",
        &["amenity fast_food"],
    ),
    category("bar", "Bar", "bar", &["amenity bar"]),
    category("pub", "Pub", "beer", &["amenity pub"]),
    category(
        "ice_cream",
        "Ice Cream",
        "ice-cream",
        &["amenity ice_cream"],
    ),
    category(
        "food",
        "Food",
        "restaurant",
        &[
            "amenity cafe",
            "amenity fast_food",
            "amenity food_court",
            "amenity ice_cream",
            "amenity restaurant",
        ],
    ),
    category("stadium", "Stadium", "stadium", &["leisure stadium"]),
    category(
        "sports",
        "Sports",
        "pitch",
        &["leisure pitch", "leisure sports_centre", "leisure stadium"],
    ),
    category(
        "food_and_drink",
        "Food and Drink",
        "restaurant",
        &[
            "amenity bar",
            "amenity cafe",
            "amenity fast_food",
            "amenity food_court",
            "amenity ice_cream",
            "amenity pub",
            "amenity restaurant",
        ],
    ),
    category("bakery", "Bakery", "bakery", &["shop bakery"]),
    category(
        "supermarket",
        "Supermarket",
        "grocery",
        &["shop supermarket"],
    ),
    category(
        "grocery",
        "Grocery",
        "grocery",
        &["shop convenience", "shop greengrocer", "shop supermarket"],
    ),
    category("shopping_mall", "Shopping Mall", "shop", &["shop mall"]),
    category("shopping", "Shopping", "shop", &["shop"]),
    category("gas_station", "Gas Station", "fuel", &["amenity fuel"]),
    category(
        "charging_station",
        "Charging Station",
        "charging-station",
        &["amenity charging_station"],
    ),
    category(
        "parking_lot",
        "Parking Lot",
        "parking",
        &["amenity parking"],
    ),
    category("bank", "Bank", "bank", &["amenity bank"]),
    category("pharmacy", "Pharmacy", "pharmacy", &["amenity pharmacy"]),
    category("hospital", "Hospital", "hospital", &["amenity hospital"]),
    category("dentist", "Dentist", "dentist", &["amenity dentist"]),
    category("hotel", "Hotel", "lodging", &["tourism hotel"]),
    category(
        "lodging",
        "Lodging",
        "lodging",
        &[
            "tourism guest_house",
            "tourism hostel",
            "tourism hotel",
            "tourism motel",
        ],
    ),
    category("museum", "Museum", "museum", &["tourism museum"]),
    category(
        "tourist_attraction",
        "Tourist Attraction",
        "attraction",
        &["tourism attraction"],
    ),
    category("park", "Park", "park", &["leisure park"]),
    category("cinema", "Cinema", "cinema", &["amenity cinema"]),
    category("theatre", "Theatre", "theatre", &["amenity theatre"]),
    category("school", "School", "school", &["amenity school"]),
    category(
        "university",
        "University",
        "college",
        &["amenity university"],
    ),
    category("library", "Library", "library", &["amenity library"]),
    category(
        "post_office",
        "Post Office",
        "post",
        &["amenity post_office"],
    ),
    category(
        "police_station",
        "Police Station",
        "police",
        &["amenity police"],
    ),
    category(
        "fire_station",
        "Fire Station",
        "fire-station",
        &["amenity fire_station"],
    ),
    category(
        "place_of_worship",
        "Place of Worship",
        "place-of-worship",
        &["amenity place_of_worship"],
    ),
    category("airport", "Airport", "airport", &["aeroway aerodrome"]),
    category(
        "railway_station",
        "Railway Station",
        "rail",
        &["railway station"],
    ),
];

const fn category(
    id: &'static str,
    name: &'static str,
    icon: &'static str,
    og: &'static [&'static str],
) -> Category {
    Category { id, name, icon, og }
}

/// og's categories under a canonical id. A listed id names its own. Any
/// other names what its words name in og's catalog, in any language the
/// catalog has, and `None` when they name no kind of place.
pub(super) fn og_categories(id: &str) -> Option<Vec<&'static str>> {
    let id = id.trim().to_ascii_lowercase();
    if let Some(listed) = CATEGORIES.iter().find(|category| category.id == id) {
        return Some(listed.og.to_vec());
    }
    let named = categories::presets_for_word(&id.replace('_', " "));
    (!named.is_empty()).then_some(named)
}

/// The listed kinds a place of og's `category` (`amenity:cafe`) is, as their
/// names and their canonical ids, closest kind first.
/// The listed kinds a place of og's `category` is, as a place states them:
/// their names in lower case, in the order of their names, and their ids
/// beside them.
pub(super) fn kinds(category: &str) -> Option<(Vec<String>, Vec<&'static str>)> {
    let mut listed = CATEGORIES
        .iter()
        .filter(|listed| categories::holds(listed.og, category))
        .map(|listed| (listed.name.to_lowercase(), listed.id))
        .collect::<Vec<_>>();
    listed.sort();
    listed.dedup_by(|a, b| a.0 == b.0);
    (!listed.is_empty()).then(|| listed.into_iter().unzip())
}

/// `list/category`'s answer.
#[derive(Debug, Serialize)]
pub(super) struct List {
    #[serde(rename = "listItems")]
    list_items: Vec<Item>,
    attribution: &'static str,
    version: &'static str,
}

#[derive(Debug, Serialize)]
struct Item {
    canonical_id: &'static str,
    icon: &'static str,
    name: &'static str,
    version: &'static str,
    uuid: String,
}

/// List the categories to search by (Mapbox Search Box v1)
///
/// Names are English whatever `language` asks for. og's catalog knows a
/// kind's words in many languages but not which language each word is.
pub(super) async fn list(
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Json<List>, VendorError> {
    Params::from_query(Api::SearchBox, query)?;
    let version = env!("CARGO_PKG_VERSION");
    Ok(Json(List {
        list_items: CATEGORIES
            .iter()
            .map(|category| Item {
                canonical_id: category.id,
                icon: category.icon,
                name: category.name,
                version,
                // An item's `uuid` is new on every request by Mapbox's contract.
                uuid: uuid::Uuid::new_v4().to_string(),
            })
            .collect(),
        // The list is og's own, so it credits no data source.
        attribution: "",
        version,
    }))
}

/// Search places of one category (Mapbox Search Box v1)
///
/// With `proximity` the answer is the places around it, nearest first, as
/// far as `radius` when it is sent. Without it the answer is the best known
/// places of `bbox` and `country`, or of the whole Pack. `proximity=ip`
/// counts as none, since og does not locate its callers.
pub(super) async fn search(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<Json<searchbox::Collection>, VendorError> {
    let params = Params::from_query(Api::SearchBox, query)?;
    let categories = og_categories(&id).ok_or_else(|| error(StatusCode::NOT_FOUND, "Not Found"))?;
    if let Some(limit) = params.get("limit") {
        let within = limit
            .parse::<usize>()
            .is_ok_and(|limit| (1..=MAX_LIMIT).contains(&limit));
        if !within {
            return Err(Api::SearchBox.invalid(format!(
                "limit must be a whole number from 1 to {MAX_LIMIT}"
            )));
        }
    }
    let radius = params.get("radius").map(radius_metres).transpose()?;
    if radius.is_some() && params.focus(Api::SearchBox)?.is_none() {
        return Err(Api::SearchBox.invalid("radius needs proximity"));
    }
    let excluded = params
        .get("poi_category_exclusions")
        .map(|value| {
            value
                .split(',')
                .filter_map(og_categories)
                .flatten()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let args = Args::new()
        .maybe("limit", params.get("limit"))
        .maybe("country", params.get("country"))
        .maybe("focus", params.focus(Api::SearchBox)?)
        .maybe("radius", radius.map(|metres| metres.to_string()))
        .maybe("bbox", params.bbox(Api::SearchBox)?)
        .set("lang", searchbox::language(&params));
    let mut collection = native::of_categories(&state, categories, args)
        .await
        .map_err(|problem| Api::SearchBox.failed(problem))?;
    collection.features.retain(|feature| {
        !feature
            .properties
            .common
            .category
            .as_deref()
            .is_some_and(|category| categories::holds(&excluded, category))
    });
    Ok(Json(searchbox::Collection::forward(&collection, None)))
}

/// `radius`, degrees around `proximity`, as metres.
fn radius_metres(value: &str) -> Result<f64, VendorError> {
    value
        .parse::<f64>()
        .ok()
        .filter(|degrees| (0.00001..=10.0).contains(degrees))
        .map(|degrees| degrees * METRES_PER_DEGREE)
        .ok_or_else(|| Api::SearchBox.invalid("radius must be a number from 0.00001 to 10"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_category_is_one_og_has() {
        for listed in CATEGORIES {
            for category in listed.og {
                assert!(categories::is_known(category), "{} {category}", listed.id);
            }
        }
    }

    #[test]
    fn an_id_names_its_listed_kind_or_what_its_words_name() {
        assert_eq!(og_categories("gas_station"), Some(vec!["amenity fuel"]));
        assert_eq!(og_categories("Coffee"), Some(vec!["amenity cafe"]));
        assert!(
            og_categories("swimming_pool").is_some_and(|named| !named.is_empty()),
            "the catalog's own words"
        );
        assert_eq!(og_categories("no_such_kind_of_place"), None);
    }

    #[test]
    fn a_place_lists_its_kinds_by_name_in_lower_case() {
        assert_eq!(
            kinds("amenity:cafe"),
            Some((
                ["cafe", "coffee", "coffee shop", "food", "food and drink"]
                    .map(str::to_string)
                    .to_vec(),
                vec!["cafe", "coffee", "coffee_shop", "food", "food_and_drink"]
            ))
        );
        assert_eq!(
            kinds("leisure:stadium"),
            Some((
                ["sports", "stadium"].map(str::to_string).to_vec(),
                vec!["sports", "stadium"]
            ))
        );
        assert_eq!(
            kinds("shop:florist"),
            Some((vec!["shopping".to_string()], vec!["shopping"]))
        );
        assert_eq!(kinds("craft:brewery"), None);
    }
}
