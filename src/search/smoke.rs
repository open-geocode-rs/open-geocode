//! End-to-end checks of acceptance before the cut and of dropping query
//! words by label, on Packs written here.

use super::{
    tests::{address_record, temp_pack_path},
    *,
};
use crate::{pack::Writer, record::point_geometry};

fn geocode_with(
    searcher: &TextSearcher,
    address: &str,
    region: &str,
    limit: usize,
) -> Option<String> {
    searcher
        .geocode_address(AddressGeocodeOptions {
            address: address.to_string(),
            locality: None,
            region: Some(region.to_string()),
            postcode: None,
            limit,
            layer: None,
            countries: Vec::new(),
        })
        .expect("geocode")
        .map(|hit| hit.hit.record.id)
}

#[test]
fn an_acceptable_hit_ranked_past_the_limit_still_returns() {
    let temp_dir = temp_pack_path("smoke-accept-before-cut");
    let _ = std::fs::remove_dir_all(&temp_dir);
    let mut writer = Writer::create(&temp_dir).expect("writer");
    // Twelve addresses in a town named Ontario tie on the region a row
    // names, and sort ahead of the one address in it on their ids, but lie
    // in no region of the name.
    for id in 1..=12 {
        let mut record = address_record(
            &format!("osm:node:{id}"),
            "",
            "10",
            "King Street",
            Some("Ontario"),
            None,
        );
        record.address.region = Some("Quebec".to_string());
        record.geometry = point_geometry(-90.0 - id as f64, 43.0);
        writer.write(&record.into(), None).expect("decoy");
    }
    let mut right = address_record(
        "osm:node:99",
        "",
        "10",
        "King Street",
        Some("Kingston"),
        None,
    );
    right.address.region = Some("Ontario".to_string());
    right.geometry = point_geometry(-76.0, 43.0);
    writer.write(&right.into(), None).expect("address");
    writer.finish().expect("finish");
    let searcher = TextSearcher::open(&temp_dir).expect("searcher");

    for limit in [1, 10] {
        assert_eq!(
            geocode_with(&searcher, "10 King Street", "Ontario", limit).as_deref(),
            Some("osm:node:99"),
            "limit {limit}"
        );
    }
    let _ = std::fs::remove_dir_all(temp_dir);
}

/// The parses of `query` and their plans, to say why an assertion failed.
fn explain(searcher: &TextSearcher, query: &str) -> String {
    searcher
        .tagger
        .parse(query)
        .iter()
        .map(|parsed| {
            let tokens = parsed
                .pieces
                .iter()
                .map(|piece| format!("{}/{:?}", piece.token, piece.label))
                .collect::<Vec<_>>()
                .join(" ");
            let plan = QueryPlan::from_tagged(&parsed.pieces, &[], |word| {
                searcher.tagger.dictionaries().describes_street(word)
            });
            format!("{tokens}\n  {plan:?}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first hits of each reading of `query` that `keeps` holds, searched
/// on its own through the passes as search does, with the reading. The
/// parser of a Pack this small often reads a word as no address part only
/// in its second or third reading, which search reaches only when the
/// first finds nothing.
fn hits_of_readings(
    searcher: &TextSearcher,
    query: &str,
    keeps: impl Fn(&Tagged) -> bool,
) -> Vec<(String, Vec<String>)> {
    let query_words = normalize_index_text(query)
        .iter()
        .flat_map(|text| text.split_whitespace())
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let mut readings = Vec::new();
    for parsed in searcher.tagger.parse(query) {
        if !parsed.pieces.iter().any(&keeps) {
            continue;
        }
        let plan = QueryPlan::from_tagged(&parsed.pieces, &[], |word| {
            searcher.tagger.dictionaries().describes_street(word)
        })
        .expect("plan");
        let mut ids = Vec::new();
        for pass in [Pass::Strict, Pass::Street, Pass::Lenient] {
            if !pass.searches_anew(&plan) {
                continue;
            }
            let (hits, _) = searcher
                .search_plan(
                    &plan,
                    pass,
                    &query_words,
                    Filters::default(),
                    10,
                    &mut |_, _, _| Ok(true),
                )
                .expect("search");
            if !hits.is_empty() {
                ids = hits.into_iter().map(|hit| hit.record.id).collect();
                break;
            }
        }
        let reading = parsed
            .pieces
            .iter()
            .map(|piece| format!("{}/{:?}", piece.token, piece.label))
            .collect::<Vec<_>>()
            .join(" ");
        readings.push((reading, ids));
    }
    readings
}

fn search_ids(searcher: &TextSearcher, query: &str) -> Vec<String> {
    searcher
        .search(TextSearchOptions {
            query: query.to_string(),
            limit: 10,
            layer: None,
            countries: Vec::new(),
        })
        .expect("search")
        .into_iter()
        .map(|hit| hit.record.id)
        .collect()
}

fn geocode_row(
    searcher: &TextSearcher,
    address: &str,
    locality: Option<&str>,
    region: Option<&str>,
    postcode: Option<&str>,
) -> Option<String> {
    searcher
        .geocode_address(AddressGeocodeOptions {
            address: address.to_string(),
            locality: locality.map(str::to_string),
            region: region.map(str::to_string),
            postcode: postcode.map(str::to_string),
            limit: 10,
            layer: None,
            countries: Vec::new(),
        })
        .expect("geocode")
        .map(|hit| hit.hit.record.id)
}

/// A locality boundary named `name`, as a build names it.
fn locality(writer: &mut Writer, name: &str, object_id: i64, [lon, lat]: [f64; 2]) -> RecordId {
    use crate::record::{ObjectType, PlaceLayer, PlaceRecord, SourceProvenance};

    writer
        .write(
            &Record::Place(
                PlaceLayer::Locality,
                PlaceRecord {
                    alt_names: Vec::new(),
                    name: name.to_string(),
                    place_type: "city".to_string(),
                    geometry: point_geometry(lon, lat),
                    source: SourceProvenance::osm(ObjectType::Relation, object_id),
                },
            ),
            None,
        )
        .expect("locality")
}

fn in_locality(record_id: RecordId) -> Option<pack::RecordContext> {
    use crate::{context::AdminContextTuple, record::PlaceLayer};

    Some(pack::RecordContext {
        admin_context: AdminContextTuple::from_tiers([(PlaceLayer::Locality, record_id)]),
        flags: 0,
    })
}

fn street(id: &str, name: &str, [lon, lat]: [f64; 2]) -> crate::record::StreetRecord {
    let mut street = super::tests::street_record(id, name);
    street.geometry = point_geometry(lon, lat);
    street.representative_point = [lon, lat];
    street
}

/// A suite designator, a stated area and a postcode around one address,
/// and the same street with a direction.
fn philadelphia(name: &str) -> (std::path::PathBuf, TextSearcher) {
    let temp_dir = temp_pack_path(name);
    let _ = std::fs::remove_dir_all(&temp_dir);
    let mut writer = Writer::create(&temp_dir).expect("writer");
    let mut roosevelt = address_record(
        "osm:node:1",
        "",
        "9501",
        "Roosevelt Boulevard",
        Some("Philadelphia"),
        Some("19114"),
    );
    roosevelt.address.region = Some("PA".to_string());
    roosevelt.geometry = point_geometry(-75.03, 40.08);
    writer.write(&roosevelt.into(), None).expect("roosevelt");
    let mut north = address_record(
        "osm:node:2",
        "",
        "9501",
        "Roosevelt Boulevard North",
        Some("Philadelphia"),
        Some("19114"),
    );
    north.address.region = Some("PA".to_string());
    north.geometry = point_geometry(-75.02, 40.09);
    writer.write(&north.into(), None).expect("roosevelt north");
    writer.finish().expect("finish");
    let searcher = TextSearcher::open(&temp_dir).expect("searcher");
    (temp_dir, searcher)
}

#[test]
fn a_suite_designator_does_not_zero_the_query() {
    let (temp_dir, searcher) = philadelphia("smoke-suite");
    let query = "9501 Roosevelt Blvd Ste 410 Philadelphia PA 19114";
    assert_eq!(
        search_ids(&searcher, query).first().map(String::as_str),
        Some("osm:node:1"),
        "{}",
        explain(&searcher, query)
    );
    let unplaced = hits_of_readings(&searcher, query, |piece| {
        piece.token == "ste" && piece.label == Label::Other
    });
    assert!(!unplaced.is_empty(), "{}", explain(&searcher, query));
    for (reading, ids) in unplaced {
        assert_eq!(
            ids.first().map(String::as_str),
            Some("osm:node:1"),
            "{reading}"
        );
    }
    assert_eq!(
        geocode_row(
            &searcher,
            "9501 Roosevelt Blvd Ste 410",
            Some("Philadelphia"),
            Some("PA"),
            Some("19114")
        )
        .as_deref(),
        Some("osm:node:1")
    );
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn a_street_type_or_direction_ranks_but_is_not_required() {
    let (temp_dir, searcher) = philadelphia("smoke-street-words");
    // The record without the direction still answers, below the one with
    // it: a full match ranks above a partial one.
    let query = "9501 Roosevelt Blvd N";
    assert_eq!(
        search_ids(&searcher, query),
        vec!["osm:node:2", "osm:node:1"],
        "{}",
        explain(&searcher, query)
    );
    assert_eq!(
        search_ids(&searcher, "9501 Roosevelt Blvd"),
        vec!["osm:node:1", "osm:node:2"]
    );
    // Without the type too.
    assert_eq!(
        search_ids(&searcher, "9501 Roosevelt").len(),
        2,
        "{}",
        explain(&searcher, "9501 Roosevelt")
    );
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn a_contradicting_direction_ranks_lower_and_a_street_of_list_words_stays_whole() {
    let temp_dir = temp_pack_path("smoke-north-road");
    let _ = std::fs::remove_dir_all(&temp_dir);
    let mut writer = Writer::create(&temp_dir).expect("writer");
    for (id, name, lon) in [
        ("osm:way:1", "North Road", -79.0),
        ("osm:way:2", "South Road", -79.1),
        ("osm:way:3", "North Street", -79.2),
    ] {
        writer
            .write(&street(id, name, [lon, 43.0]).into(), None)
            .expect("street");
    }
    for (id, name, lon) in [
        ("osm:node:4", "Main Street South", -79.3),
        ("osm:node:5", "Main Street North", -79.4),
    ] {
        let mut address = address_record(id, "", "7", name, None, None);
        address.geometry = point_geometry(lon, 43.0);
        writer.write(&address.into(), None).expect("address");
    }
    writer.finish().expect("finish");
    let searcher = TextSearcher::open(&temp_dir).expect("searcher");

    let plan = searcher
        .tagger
        .parse("North Road")
        .iter()
        .find_map(|parsed| {
            QueryPlan::from_tagged(&parsed.pieces, &[], |word| {
                searcher.tagger.dictionaries().describes_street(word)
            })
            .filter(|plan| plan.subject.len() == 2)
        })
        .expect("a plan of the street");
    assert!(plan.subject.iter().all(|word| word.required), "{plan:?}");
    assert_eq!(
        search_ids(&searcher, "North Road"),
        vec!["osm:way:1"],
        "{}",
        explain(&searcher, "North Road")
    );
    let query = "7 Main St N";
    assert_eq!(
        search_ids(&searcher, query),
        vec!["osm:node:5", "osm:node:4"],
        "{}",
        explain(&searcher, query)
    );
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn a_country_exonym_does_not_zero_the_query() {
    let temp_dir = temp_pack_path("smoke-exonym");
    let _ = std::fs::remove_dir_all(&temp_dir);
    let mut writer = Writer::create(&temp_dir).expect("writer");
    let mut address = address_record(
        "osm:node:1",
        "",
        "5",
        "Maximilianstraße",
        Some("München"),
        Some("80539"),
    );
    address.address.country = Some("Deutschland".to_string());
    address.geometry = point_geometry(11.58, 48.14);
    writer.write(&address.into(), None).expect("address");
    writer.finish().expect("finish");
    let searcher = TextSearcher::open(&temp_dir).expect("searcher");

    for (query, exonym) in [
        ("Maximilianstraße 5, München, Allemagne", "allemagne"),
        ("Maximilianstraße 5, München, Germany", "germany"),
    ] {
        assert_eq!(
            search_ids(&searcher, query).first().map(String::as_str),
            Some("osm:node:1"),
            "{}",
            explain(&searcher, query)
        );
        // Whether the parser reads it as a country, a place or nothing.
        let readings = hits_of_readings(&searcher, query, |piece| {
            piece.token == exonym
                && matches!(piece.label, Label::Other | Label::Country | Label::Locality)
        });
        assert!(!readings.is_empty(), "{}", explain(&searcher, query));
        for (reading, ids) in readings {
            assert_eq!(
                ids.first().map(String::as_str),
                Some("osm:node:1"),
                "{reading}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(temp_dir);
}

/// "Ponsonby Road" in Auckland, and a town that has none.
fn ponsonby(name: &str, christchurch_known: bool) -> (std::path::PathBuf, TextSearcher) {
    let temp_dir = temp_pack_path(name);
    let _ = std::fs::remove_dir_all(&temp_dir);
    let mut writer = Writer::create(&temp_dir).expect("writer");
    let auckland = locality(&mut writer, "Auckland", 100, [174.75, -36.85]);
    let mut names = vec![(auckland, vec!["Auckland".to_string()])];
    let christchurch =
        christchurch_known.then(|| locality(&mut writer, "Christchurch", 101, [172.63, -43.53]));
    names.extend(christchurch.map(|record_id| (record_id, vec!["Christchurch".to_string()])));
    writer.set_context_names(names.into_iter().collect());
    writer
        .write(
            &street("osm:way:1", "Ponsonby Road", [174.74, -36.86]).into(),
            in_locality(auckland),
        )
        .expect("ponsonby road");
    if let Some(christchurch) = christchurch {
        writer
            .write(
                &street("osm:way:2", "Colombo Street", [172.63, -43.53]).into(),
                in_locality(christchurch),
            )
            .expect("colombo street");
    }
    writer.finish().expect("finish");
    let searcher = TextSearcher::open(&temp_dir).expect("searcher");
    (temp_dir, searcher)
}

#[test]
fn a_known_locality_that_disagrees_rejects_in_every_pass() {
    let (temp_dir, searcher) = ponsonby("smoke-ponsonby-known", true);
    for query in ["Ponsonby Road Christchurch", "Ponsonby Road, Christchurch"] {
        assert_eq!(
            search_ids(&searcher, query),
            Vec::<String>::new(),
            "{}",
            explain(&searcher, query)
        );
    }
    assert_eq!(
        geocode_row(&searcher, "Ponsonby Road", Some("Christchurch"), None, None),
        None
    );
    assert_eq!(
        search_ids(&searcher, "Ponsonby Road, Auckland"),
        vec!["osm:way:1"]
    );
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn a_locality_unknown_to_the_pack_only_ranks() {
    let (temp_dir, searcher) = ponsonby("smoke-ponsonby-unknown", false);
    let query = "Ponsonby Road, Christchurch";
    assert_eq!(
        search_ids(&searcher, query),
        vec!["osm:way:1"],
        "{}",
        explain(&searcher, query)
    );
    assert_eq!(
        geocode_row(&searcher, "Ponsonby Road", Some("Christchurch"), None, None).as_deref(),
        Some("osm:way:1")
    );
    let _ = std::fs::remove_dir_all(temp_dir);
}

#[test]
fn a_query_of_unplaced_words_needs_most_of_them() {
    let temp_dir = temp_pack_path("smoke-unplaced-floor");
    let _ = std::fs::remove_dir_all(&temp_dir);
    let mut writer = Writer::create(&temp_dir).expect("writer");
    for (id, name, lon) in [
        ("osm:way:1", "Ulica Marszałka Piłsudskiego", 21.0),
        ("osm:way:2", "Ulica Marszałka Focha", 21.1),
        ("osm:way:3", "Ulica Długa", 21.2),
    ] {
        writer
            .write(&street(id, name, [lon, 52.2]).into(), None)
            .expect("street");
    }
    writer.finish().expect("finish");
    let searcher = TextSearcher::open(&temp_dir).expect("searcher");
    let plan = |words: &[&str]| QueryPlan {
        unplaced: words.iter().map(|word| word.to_string()).collect(),
        ..QueryPlan::default()
    };
    let ids = |plan: &QueryPlan| {
        let query = searcher.build_query(plan, Pass::Strict, Filters::default(), &HashSet::new());
        searcher
            .ranked_hits(&query, &Ranking::default(), 10, &mut |_| Ok(true))
            .expect("hits")
            .into_iter()
            .map(|hit| hit.record.id)
            .collect::<Vec<_>>()
    };

    assert_eq!(unplaced_floor(1), 1);
    assert_eq!(unplaced_floor(3), 2);
    assert_eq!(unplaced_floor(5), 4);
    assert_eq!(unplaced_floor(6), 5);
    assert_eq!(unplaced_floor(8), 6);
    // Two of three: the full match first, the one missing a word next, and
    // never the one with only "ulica".
    assert_eq!(
        ids(&plan(&["ulica", "marszałka", "piłsudskiego"])),
        vec!["osm:way:1", "osm:way:2"]
    );
    assert_eq!(ids(&plan(&["marszałka"])).len(), 2);
    assert_eq!(ids(&plan(&["ulica", "kościuszki"])).len(), 3);
    assert!(ids(&plan(&["ulica", "kościuszki", "wolności"])).is_empty());
    let _ = std::fs::remove_dir_all(temp_dir);
}

/// Equal-name ties broken toward the larger place.
mod place_rank {
    use std::collections::HashMap;

    use crate::{
        context::AdminContextTuple,
        pack::{RecordContext, Writer},
        record::{
            LocationPrecision, ObjectType, PlaceLayer, PlaceRecord, PoiRecord, Record,
            SourceProvenance, point_geometry,
        },
    };

    use super::*;

    /// New Zealand with Tasman, Canterbury and Hawke's Bay: a Richmond town
    /// in Tasman and a Richmond locality in Canterbury, a Hastings city in
    /// Hawke's Bay and a Hastings hamlet in Tasman, a suburb and a quarry
    /// named after them, and two bakeries. Every smaller place has the lower
    /// node id, which alone used to break the tie its way.
    fn write_pack(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("open-geocode-smoke-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut writer = Writer::create(&dir).expect("writer");
        let area = |writer: &mut Writer, layer, name: &str, level: u8, id| {
            writer
                .write(
                    &Record::Place(
                        layer,
                        PlaceRecord {
                            alt_names: Vec::new(),
                            name: name.to_string(),
                            place_type: format!("admin_level:{level}"),
                            geometry: point_geometry(173.0, -41.0),
                            source: SourceProvenance::osm(ObjectType::Relation, id),
                        },
                    ),
                    None,
                )
                .expect("area")
        };
        let country = area(&mut writer, PlaceLayer::Country, "New Zealand", 2, 1);
        let tasman = area(&mut writer, PlaceLayer::Region, "Tasman", 4, 2);
        let canterbury = area(&mut writer, PlaceLayer::Region, "Canterbury", 4, 3);
        let hawkes_bay = area(&mut writer, PlaceLayer::Region, "Hawke's Bay", 4, 4);
        writer.set_context_names(HashMap::from([
            (country, vec!["New Zealand".to_string()]),
            (tasman, vec!["Tasman".to_string()]),
            (canterbury, vec!["Canterbury".to_string()]),
            (hawkes_bay, vec!["Hawke's Bay".to_string()]),
        ]));
        let context = |region| {
            Some(RecordContext {
                admin_context: AdminContextTuple::from_tiers([
                    (PlaceLayer::Country, country),
                    (PlaceLayer::Region, region),
                ]),
                flags: 0,
            })
        };
        for (layer, name, place_type, id, region) in [
            (PlaceLayer::Locality, "Richmond", "locality", 10, canterbury),
            (PlaceLayer::Locality, "Richmond", "town", 11, tasman),
            (PlaceLayer::Locality, "Hastings", "hamlet", 20, tasman),
            (PlaceLayer::Locality, "Hastings", "city", 21, hawkes_bay),
            (
                PlaceLayer::Neighbourhood,
                "Hastings Central",
                "suburb",
                22,
                hawkes_bay,
            ),
        ] {
            writer
                .write(
                    &Record::Place(
                        layer,
                        PlaceRecord {
                            alt_names: Vec::new(),
                            name: name.to_string(),
                            place_type: place_type.to_string(),
                            geometry: point_geometry(173.0, -41.0),
                            source: SourceProvenance::osm(ObjectType::Node, id),
                        },
                    ),
                    context(region),
                )
                .expect("place");
        }
        for (name, category, id) in [
            ("Richmond Quarry", "landuse:quarry", 30),
            ("Tasman Bakery", "shop:bakery", 41),
            ("Tasman Bakery", "shop:bakery", 40),
        ] {
            writer
                .write(
                    &PoiRecord {
                        alt_names: Vec::new(),
                        name: name.to_string(),
                        category: category.to_string(),
                        address: None,
                        geometry: point_geometry(173.0, -41.0),
                        location_precision: LocationPrecision::Point,
                        source: SourceProvenance::osm(ObjectType::Node, id),
                    }
                    .into(),
                    context(tasman),
                )
                .expect("poi");
        }
        writer.finish().expect("finish");
        dir
    }

    fn search(searcher: &TextSearcher, query: &str, layer: Option<&str>) -> Vec<String> {
        searcher
            .search(TextSearchOptions {
                query: query.to_string(),
                limit: 5,
                layer: layer.map(str::to_string),
                countries: Vec::new(),
            })
            .expect("search")
            .into_iter()
            .map(|hit| hit.record.id)
            .collect()
    }

    #[test]
    fn a_name_alone_answers_the_larger_place() {
        let dir = write_pack("place-rank");
        let searcher = TextSearcher::open(&dir).expect("searcher");

        let richmond = search(&searcher, "Richmond", None);
        assert_eq!(richmond[0], "osm:node:11", "the town: {richmond:?}");
        assert_eq!(
            richmond[1], "osm:node:10",
            "then the locality: {richmond:?}"
        );
        let hastings = search(&searcher, "Hastings", None);
        assert_eq!(hastings[0], "osm:node:21", "the city: {hastings:?}");
        assert_eq!(hastings[1], "osm:node:20", "then the hamlet: {hastings:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_stated_area_outranks_the_larger_place() {
        let dir = write_pack("place-rank-area");
        let searcher = TextSearcher::open(&dir).expect("searcher");

        for (query, expected) in [
            ("Richmond Tasman", "osm:node:11"),
            ("Richmond Canterbury", "osm:node:10"),
            ("Hastings Hawke's Bay", "osm:node:21"),
            ("Hastings Tasman", "osm:node:20"),
        ] {
            let hits = search(&searcher, query, None);
            assert_eq!(
                hits.first().map(String::as_str),
                Some(expected),
                "{query}: {hits:?}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn records_that_are_not_places_keep_their_order() {
        let dir = write_pack("place-rank-poi");
        let searcher = TextSearcher::open(&dir).expect("searcher");

        assert_eq!(
            search(&searcher, "Tasman Bakery", Some("poi")),
            vec!["osm:node:40", "osm:node:41"]
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A postcode the record contradicts ranks the record down, never out.
mod postcode {
    use crate::{
        pack::Writer,
        record::{
            AddressComponents, AddressRecord, LocationPrecision, ObjectType, SourceProvenance,
            point_geometry,
        },
    };

    use super::*;

    fn address(object_id: i64, lon: f64, postcode: Option<&str>, street: &[&str]) -> AddressRecord {
        AddressRecord {
            address: AddressComponents {
                areas: Vec::new(),
                number: "10".to_string(),
                street: Some("King Street".to_string()),
                place: None,
                unit: None,
                locality: None,
                region: None,
                postcode: postcode.map(str::to_string),
                country: None,
            },
            street_postcodes: street.iter().map(|postcode| postcode.to_string()).collect(),
            geometry: point_geometry(lon, -33.8),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, object_id),
        }
    }

    fn ids(searcher: &TextSearcher, query: &str) -> Vec<String> {
        searcher
            .search(TextSearchOptions {
                query: query.to_string(),
                limit: 10,
                layer: None,
                countries: Vec::new(),
            })
            .expect("search")
            .into_iter()
            .map(|hit| hit.record.id)
            .collect()
    }

    #[test]
    fn a_contradicting_postcode_ranks_below_an_agreeing_one_and_stays() {
        let dir = std::env::temp_dir().join(format!(
            "open-geocode-smoke-postcode-rank-{}",
            uuid::Uuid::new_v4()
        ));
        let mut writer = Writer::create(&dir).expect("writer");
        for record in [
            address(1, 151.0, Some("2111"), &[]),
            address(2, 151.1, Some("2110"), &[]),
            // States none, but its street's addresses state 2110.
            address(3, 151.2, None, &["2110"]),
        ] {
            writer.write(&record.into(), None).expect("address");
        }
        writer.finish().expect("finish");
        let searcher = TextSearcher::open(&dir).expect("searcher");

        let agreeing = ids(&searcher, "10 King Street, 2110");
        assert_eq!(agreeing.len(), 3, "nothing is dropped: {agreeing:?}");
        assert_eq!(agreeing[2], "osm:node:1", "the 2111 address ranks last");
        let mut first = agreeing[..2].to_vec();
        first.sort();
        assert_eq!(first, ["osm:node:2", "osm:node:3"]);

        // The borrowed 2110 contradicts 2111 as a stated one does.
        let other = ids(&searcher, "10 King Street, 2111");
        assert_eq!(other.len(), 3, "{other:?}");
        assert_eq!(other[0], "osm:node:1");
        assert_eq!(
            ids(&searcher, "10 King Street, 2999").len(),
            3,
            "contradicted everywhere, still found"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
