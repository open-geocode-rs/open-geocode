use std::{num::NonZeroUsize, path::PathBuf};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{attribution, split_label};
use crate::{
    api::{Accuracy, Address, Attribution, Common, Context, Source},
    compat::encode_id,
    context::{AdminContextTuple, RecordContext},
    limits::Limits,
    pack::Writer,
    record::{
        AddressComponents, AddressRecord, Brand, InterpolationAddressComponents,
        InterpolationRange, InterpolationRecord, Layer, LocationPrecision, ObjectType, PlaceLayer,
        PlaceRecord, PoiRecord, Record, SourceProvenance, point_geometry,
    },
    runtime::{AppState, build_router},
};

/// A Pack directory removed when the test ends, pass or fail.
struct Pack(PathBuf);

impl Drop for Pack {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        let _ = std::fs::remove_dir_all(self.0.with_extension("jobs"));
    }
}

/// An address with a unit, a cafe of a chain and an interpolation range in
/// Canada > Ontario > Toronto > Downtown.
fn write_pack() -> Pack {
    let dir = std::env::temp_dir().join(format!(
        "open-geocode-compat-mapbox-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut writer = Writer::create(&dir).expect("writer");
    let place = |writer: &mut Writer, layer, name: &str, id, lon| {
        writer
            .write(
                &Record::Place(
                    layer,
                    PlaceRecord {
                        alt_names: Vec::new(),
                        name: name.to_string(),
                        place_type: "admin_level:8".to_string(),
                        geometry: point_geometry(lon, 43.0),
                        source: SourceProvenance::osm(ObjectType::Relation, id),
                        prominence: Default::default(),
                        wikidata: None,
                    },
                ),
                None,
            )
            .expect("place")
    };
    let country = place(&mut writer, PlaceLayer::Country, "Canada", 1, -80.0);
    let region = place(&mut writer, PlaceLayer::Region, "Ontario", 2, -79.5);
    let locality = place(&mut writer, PlaceLayer::Locality, "Toronto", 3, -79.2);
    let neighbourhood = place(&mut writer, PlaceLayer::Neighbourhood, "Downtown", 4, -79.1);
    writer.set_country_codes(
        [(
            country,
            crate::parse::lattice::CountryCode::parse("CA").expect("code"),
        )]
        .into_iter()
        .collect(),
    );
    writer.set_subdivision_codes([(region, "CA-ON".to_string())].into_iter().collect());
    let names = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(code, name)| ((*code).to_string(), (*name).to_string()))
            .collect::<Vec<_>>()
    };
    writer.set_language_names(
        [
            (country, names(&[("de", "Kanada"), ("fr", "Canada")])),
            (region, names(&[("fr", "Ontario")])),
        ]
        .into_iter()
        .collect(),
    );
    let context = Some(RecordContext {
        admin_context: AdminContextTuple::from_tiers([
            (PlaceLayer::Country, country),
            (PlaceLayer::Region, region),
            (PlaceLayer::Locality, locality),
            (PlaceLayer::Neighbourhood, neighbourhood),
        ]),
        flags: 0,
        zone: None,
    });
    let king = AddressComponents {
        areas: Vec::new(),
        number: "10".to_string(),
        street: Some("King Street".to_string()),
        place: None,
        unit: Some("2".to_string()),
        locality: Some("Toronto".to_string()),
        region: None,
        postcode: Some("M5V 1A1".to_string()),
        country: None,
    };
    writer
        .write(
            &AddressRecord {
                access: Vec::new(),
                disputed: false,
                street_postcodes: Vec::new(),
                address: king.clone(),
                geometry: point_geometry(-79.0, 43.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, 10),
            }
            .into(),
            context.clone(),
        )
        .expect("address");
    writer
        .write(
            &PoiRecord {
                alt_names: Vec::new(),
                name: "Queen Cafe".to_string(),
                category: "amenity:cafe".to_string(),
                address: Some(AddressComponents {
                    number: "12".to_string(),
                    unit: Some("A".to_string()),
                    ..king
                }),
                geometry: point_geometry(-79.005, 43.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, 11),
                prominence: Default::default(),
                wikidata: None,
                brand: Some(Brand {
                    name: "Queen Coffee".to_string(),
                    wikidata: None,
                }),
                details: None,
                aliases: Vec::new(),
            }
            .into(),
            context.clone(),
        )
        .expect("poi");
    writer
        .write(
            &InterpolationRecord {
                address: InterpolationAddressComponents {
                    street: Some("Queen Street".to_string()),
                    place: None,
                    locality: Some("Toronto".to_string()),
                    region: None,
                    postcode: None,
                    country: None,
                },
                interpolation: InterpolationRange {
                    kind: "even".to_string(),
                    start: 2,
                    end: 98,
                    step: 2,
                },
                anchor_node_ids: [2, 98],
                geometry: geojson::Geometry::new(geojson::GeometryValue::LineString {
                    coordinates: vec![vec![-79.01, 43.0].into(), vec![-79.01, 43.001].into()],
                }),
                representative_point: [-79.01, 43.0005],
                source: SourceProvenance::osm(ObjectType::Way, 13),
            }
            .into(),
            context,
        )
        .expect("interpolation");
    writer.finish().expect("finish");
    Pack(dir)
}

fn router_with(pack: &Pack, limits: Limits) -> Router {
    build_router(
        AppState::open(&pack.0, &pack.0.with_extension("jobs"), limits).expect("open pack"),
        None,
        None,
    )
}

fn router(pack: &Pack) -> Router {
    router_with(pack, Limits::DEFAULT)
}

async fn send(router: &Router, method: &str, uri: &str, body: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let response = router
        .clone()
        .oneshot(
            request
                .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).expect("JSON body"))
}

async fn get(router: &Router, uri: &str) -> (StatusCode, Value) {
    send(router, "GET", uri, None).await
}

const OSM: &str =
    "© OpenStreetMap contributors, ODbL 1.0 (https://www.openstreetmap.org/copyright)";

fn id(og_id: &str) -> String {
    encode_id(og_id)
}

/// The Geocoding context every Toronto record has.
fn areas() -> Value {
    json!({
        "neighborhood": {"mapbox_id": id("osm:relation:4"), "name": "Downtown"},
        "place": {"mapbox_id": id("osm:relation:3"), "name": "Toronto"},
        "region": {
            "mapbox_id": id("osm:relation:2"),
            "name": "Ontario",
            "region_code": "ON",
            "region_code_full": "CA-ON",
        },
        "country": {
            "mapbox_id": id("osm:relation:1"),
            "name": "Canada",
            "country_code": "CA",
            "country_code_alpha_3": "CAN",
        },
    })
}

fn merged(mut base: Value, extra: Value) -> Value {
    let Value::Object(extra) = extra else {
        panic!("{extra} is no object");
    };
    for (key, value) in extra {
        base[key] = value;
    }
    base
}

#[tokio::test]
async fn geocoding_forward_answers_an_address_in_mapbox_shape() {
    let pack = write_pack();
    let router = router(&pack);
    let (status, body) = get(
        &router,
        "/compat/mapbox/search/geocode/v6/forward?q=10%20King%20Street&access_token=pk.x&language=en&worldview=us&permanent=true",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let address = id("osm:node:10");
    assert_eq!(
        body,
        json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "id": address,
                "geometry": {"type": "Point", "coordinates": [-79.0, 43.0]},
                "properties": {
                    "mapbox_id": address,
                    "feature_type": "address",
                    "full_address": "10 King Street, Unit 2, Toronto, M5V 1A1",
                    "name": "10 King Street",
                    "coordinates": {"longitude": -79.0, "latitude": 43.0, "accuracy": "rooftop"},
                    "place_formatted": "Toronto, M5V 1A1",
                    "context": merged(areas(), json!({
                        "address": {
                            "mapbox_id": address,
                            "name": "10 King Street",
                            "address_number": "10",
                            "street_name": "King Street",
                        },
                        "secondary_address": {"name": "2"},
                        "street": {"mapbox_id": address, "name": "King Street"},
                        "postcode": {"name": "M5V 1A1"},
                    })),
                },
            }],
            "attribution": OSM,
        })
    );
}

/// Every og value of every result lands in its Mapbox slot unchanged.
#[tokio::test]
async fn every_og_value_lands_in_its_mapbox_slot() {
    let pack = write_pack();
    let router = router(&pack);
    for (native, compat) in [
        (
            "/geocode?q=10%20King%20Street",
            "/compat/mapbox/search/geocode/v6/forward?q=10%20King%20Street",
        ),
        (
            "/geocode?q=Queen%20Street",
            "/compat/mapbox/search/geocode/v6/forward?q=Queen%20Street",
        ),
        (
            "/reverse?lon=-79.0&lat=43.0",
            "/compat/mapbox/search/geocode/v6/reverse?longitude=-79.0&latitude=43.0",
        ),
    ] {
        let (_, og) = get(&router, native).await;
        let (status, mapbox) = get(&router, compat).await;
        assert_eq!(status, StatusCode::OK, "{compat}");
        let og_features = og["features"].as_array().expect("features");
        let features = mapbox["features"].as_array().expect("features");
        assert!(!og_features.is_empty(), "{native}");
        assert_eq!(features.len(), og_features.len(), "{compat}");
        for (og, feature) in og_features.iter().zip(features) {
            let og = &og["properties"];
            let properties = &feature["properties"];
            let own = id(og["id"].as_str().expect("id"));
            assert_eq!(feature["id"], own);
            assert_eq!(properties["mapbox_id"], own);
            assert_eq!(properties["name"], og["name"]);
            assert_eq!(properties["full_address"], og["label"]);
            assert_eq!(
                feature["geometry"]["coordinates"],
                og_feature_coordinates(og_features, og)
            );
            let accuracy = &properties["coordinates"]["accuracy"];
            match og["layer"].as_str() {
                Some("address" | "interpolation" | "street") => {
                    assert_eq!(accuracy, &og["accuracy"]);
                }
                _ => assert_eq!(accuracy, &Value::Null),
            }
            let context = &properties["context"];
            let parts = &og["address"];
            if og["layer"] == "address" {
                assert_eq!(context["address"]["address_number"], parts["housenumber"]);
                assert_eq!(context["address"]["street_name"], parts["street"]);
                assert_eq!(context["street"]["name"], parts["street"]);
            }
            if og["layer"] != "poi" {
                assert_eq!(context["secondary_address"]["name"], parts["unit"]);
            }
            assert_eq!(context["postcode"]["name"], parts["postcode"]);
            for (tier, slot) in [
                ("neighbourhood", "neighborhood"),
                ("locality", "place"),
                ("region", "region"),
                ("country", "country"),
            ] {
                let area = &og["context"][tier];
                if area.is_null() {
                    continue;
                }
                assert_eq!(
                    context[slot]["mapbox_id"],
                    id(area["id"].as_str().expect("id"))
                );
                assert_eq!(context[slot]["name"], area["name"]);
            }
            if !og["context"]["country"].is_null() {
                assert_eq!(
                    context["country"]["country_code"],
                    og["context"]["country"]["code"]
                );
            }
        }
    }
}

fn og_feature_coordinates(features: &[Value], properties: &Value) -> Value {
    features
        .iter()
        .find(|feature| feature["properties"]["id"] == properties["id"])
        .map(|feature| feature["geometry"]["coordinates"].clone())
        .expect("feature")
}

#[tokio::test]
async fn geocoding_reverse_lists_the_areas_holding_the_point() {
    let pack = write_pack();
    let router = router(&pack);
    let (status, body) = get(
        &router,
        "/compat/mapbox/search/geocode/v6/reverse?longitude=-79.0&latitude=43.0&country=ca",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let types = body["features"]
        .as_array()
        .expect("features")
        .iter()
        .map(|feature| feature["properties"]["feature_type"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        types,
        ["address", "neighborhood", "place", "region", "country"]
    );
    // og's reverse areas carry no context of their own. The area is its
    // own layer.
    assert_eq!(
        body["features"][2]["properties"]["context"],
        json!({"place": {"mapbox_id": id("osm:relation:3"), "name": "Toronto"}})
    );
    assert_eq!(
        body["features"][2]["properties"].get("place_formatted"),
        None
    );

    let (status, body) = get(
        &router,
        "/compat/mapbox/search/geocode/v6/reverse?longitude=-79.0&latitude=43.0&types=region,country",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let names = body["features"]
        .as_array()
        .expect("features")
        .iter()
        .map(|feature| feature["properties"]["name"].clone())
        .collect::<Vec<_>>();
    assert_eq!(names, ["Ontario", "Canada"]);

    let (status, body) = get(
        &router,
        "/compat/mapbox/search/geocode/v6/reverse?longitude=-79.0&latitude=43.0&types=address&limit=5",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["features"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["features"][0]["properties"]["name"], "10 King Street");
}

#[tokio::test]
async fn geocoding_structured_input_is_og_structured_geocode() {
    let pack = write_pack();
    let router = router(&pack);
    for query in [
        "address_number=10&street=King%20Street&place=Toronto&postcode=M5V%201A1&country=CA",
        "address_line1=10%20King%20Street&country=CAN",
    ] {
        let (status, body) = get(
            &router,
            &format!("/compat/mapbox/search/geocode/v6/forward?{query}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{query}");
        assert_eq!(body["features"][0]["id"], id("osm:node:10"), "{query}");
    }

    // A country og has no data in is a hard filter, as Mapbox's code is.
    let (status, body) = get(
        &router,
        "/compat/mapbox/search/geocode/v6/forward?address_line1=10%20King%20Street&country=NZ",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["features"], json!([]));

    // Areas alone are searched as the text they spell.
    let (status, body) = get(
        &router,
        "/compat/mapbox/search/geocode/v6/forward?place=Toronto&types=place",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["features"][0]["id"], id("osm:relation:3"));
    assert_eq!(body["features"][0]["properties"]["feature_type"], "place");
}

#[tokio::test]
async fn geocoding_forward_takes_a_mapbox_id_as_q() {
    let pack = write_pack();
    let router = router(&pack);
    let poi = id("osm:node:11");
    let (status, body) = get(
        &router,
        &format!("/compat/mapbox/search/geocode/v6/forward?q={poi}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["features"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["features"][0]["id"], poi);
}

#[tokio::test]
async fn geocoding_batch_answers_each_query_in_order() {
    let pack = write_pack();
    let router = router(&pack);
    let (status, body) = send(
        &router,
        "POST",
        "/compat/mapbox/search/geocode/v6/batch?access_token=pk.x&permanent=true",
        Some(
            r#"[
                {"q": "10 King Street", "types": ["address"], "limit": 1},
                {"q": "zzqx wvkj"},
                {"longitude": -79.0, "latitude": 43.0, "types": "address"},
                {"address_line1": "10 King Street", "place": "Toronto"}
            ]"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let batch = body["batch"].as_array().expect("batch");
    assert_eq!(batch.len(), 4);
    assert_eq!(batch[0]["features"][0]["id"], id("osm:node:10"));
    assert_eq!(
        batch[1],
        json!({"type": "FeatureCollection", "features": [], "attribution": OSM})
    );
    assert_eq!(batch[2]["features"].as_array().map(Vec::len), Some(1));
    assert_eq!(batch[2]["features"][0]["id"], id("osm:node:10"));
    assert_eq!(batch[3]["features"][0]["id"], id("osm:node:10"));
    // Each answer is what the same GET answers.
    let (_, single) = get(
        &router,
        "/compat/mapbox/search/geocode/v6/forward?q=10%20King%20Street&types=address&limit=1",
    )
    .await;
    assert_eq!(batch[0], single);
}

#[tokio::test]
async fn geocoding_batch_refuses_bad_input_whole() {
    let pack = write_pack();
    let limits = Limits {
        max_batch_items: NonZeroUsize::new(2).expect("two"),
        ..Limits::DEFAULT
    };
    let router = router_with(&pack, limits);
    let batch = "/compat/mapbox/search/geocode/v6/batch";
    for (body, message) in [
        (
            r#"[{"q": "a"}, {"q": "b"}, {"q": "c"}]"#,
            "Batch queries must include 2 queries or less",
        ),
        (
            r#"{"q": "a"}"#,
            "Batch body must be a JSON array of query objects",
        ),
        (
            r#"["a"]"#,
            "Each batch query must be a JSON object of parameters",
        ),
        (
            r#"[{"q": "a", "types": ["moon"]}]"#,
            "Type \"moon\" is not a known type. Must be one of: country, region, place, \
             district, postcode, locality, neighborhood, and address",
        ),
        (
            r#"[{"q": "a"}, {"longitude": 500, "latitude": 0}]"#,
            "coordinate out of range",
        ),
    ] {
        let (status, error) = send(&router, "POST", batch, Some(body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(error, json!({"message": message}), "{body}");
    }
    let (status, error) = send(&router, "POST", batch, Some(r#"[{"limit": 1}]"#)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error, json!({"message": "Not Found"}));
}

#[tokio::test]
async fn geocoding_errors_are_mapbox_errors() {
    let pack = write_pack();
    let router = router(&pack);
    let base = "/compat/mapbox/search/geocode/v6";
    for (uri, status, message) in [
        (
            format!("{base}/forward"),
            StatusCode::NOT_FOUND,
            "Not Found",
        ),
        (
            format!("{base}/forward?q=%20&limit=3"),
            StatusCode::NOT_FOUND,
            "Not Found",
        ),
        (
            format!("{base}/forward?q=a&types=moon"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "Type \"moon\" is not a known type. Must be one of: country, region, place, \
             district, postcode, locality, neighborhood, and address",
        ),
        (
            format!("{base}/forward?q=a&bbox=1,2"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "bbox must be 4 comma-separated numbers, min_lon,min_lat,max_lon,max_lat",
        ),
        (
            format!("{base}/forward?q=a&limit=51"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "requested result count exceeds the maximum",
        ),
        (
            format!("{base}/reverse?longitude=-79&latitude=43&limit=2"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "limit must be combined with a single type parameter when reverse geocoding",
        ),
        (
            format!("{base}/reverse?longitude=-79"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "could not parse query parameters",
        ),
        (
            format!("{base}/nothing"),
            StatusCode::NOT_FOUND,
            "Not Found",
        ),
        (
            "/compat/mapbox".to_string(),
            StatusCode::NOT_FOUND,
            "Not Found",
        ),
    ] {
        let (got, body) = get(&router, &uri).await;
        assert_eq!(got, status, "{uri}");
        assert_eq!(body, json!({"message": message}), "{uri}");
    }
    // Mapbox documents no 405: a wrong method is an unknown endpoint.
    let (status, body) = send(&router, "POST", &format!("{base}/forward?q=a"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"message": "Not Found"}));
    let (status, body) = get(&router, &format!("{base}/batch")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"message": "Not Found"}));
}

#[tokio::test]
async fn search_box_suggest_then_retrieve() {
    let pack = write_pack();
    let router = router(&pack);
    let (status, body) = get(
        &router,
        "/compat/mapbox/search/searchbox/v1/suggest?q=Queen%20Cafe&session_token=s&types=poi&proximity=ip",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let poi = id("osm:node:11");
    let context = json!({
        "address": {"name": "12 King Street", "address_number": "12", "street_name": "King Street"},
        "street": {"name": "King Street"},
        "neighborhood": {"id": id("osm:relation:4"), "name": "Downtown"},
        "postcode": {"name": "M5V 1A1"},
        "place": {"id": id("osm:relation:3"), "name": "Toronto"},
        "region": {
            "id": id("osm:relation:2"),
            "name": "Ontario",
            "region_code": "ON",
            "region_code_full": "CA-ON",
        },
        "country": {
            "id": id("osm:relation:1"),
            "name": "Canada",
            "country_code": "CA",
            "country_code_alpha_3": "CAN",
        },
    });
    assert_eq!(
        body,
        json!({
            "suggestions": [{
                "name": "Queen Cafe",
                "mapbox_id": poi,
                "feature_type": "poi",
                "address": "12 King Street",
                "full_address": "12 King Street, Unit A, Toronto, M5V 1A1",
                "place_formatted": "Toronto, M5V 1A1",
                "context": context,
                "maki": "cafe",
                "brand": ["Queen Coffee"],
            }],
            "attribution": OSM,
        })
    );

    let (status, body) = get(
        &router,
        &format!("/compat/mapbox/search/searchbox/v1/retrieve/{poi}?session_token=s"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {"type": "Point", "coordinates": [-79.005, 43.0]},
                "properties": {
                    "name": "Queen Cafe",
                    "mapbox_id": poi,
                    "feature_type": "poi",
                    "address": "12 King Street",
                    "full_address": "12 King Street, Unit A, Toronto, M5V 1A1",
                    "place_formatted": "Toronto, M5V 1A1",
                    "context": context,
                    "coordinates": {"longitude": -79.005, "latitude": 43.0},
                    "maki": "cafe",
                    "brand": ["Queen Coffee"],
                },
            }],
            "attribution": OSM,
        })
    );
}

#[tokio::test]
async fn search_box_forward_picks_the_og_search_of_the_same_meaning() {
    let pack = write_pack();
    let router = router(&pack);
    let base = "/compat/mapbox/search/searchbox/v1/forward";
    for (query, expected) in [
        // og's /geocode, which has addresses and no places.
        ("q=10%20King%20Street", vec![id("osm:node:10")]),
        ("q=Queen%20Cafe&types=address", vec![]),
        // og's /places.
        ("q=Queen%20Cafe&types=poi", vec![id("osm:node:11")]),
        // og's /autocomplete, which has both.
        (
            "q=Queen%20Caf&auto_complete=true&types=poi",
            vec![id("osm:node:11")],
        ),
    ] {
        let (status, body) = get(&router, &format!("{base}?{query}")).await;
        assert_eq!(status, StatusCode::OK, "{query}");
        let ids = body["features"]
            .as_array()
            .expect("features")
            .iter()
            .map(|feature| {
                feature["properties"]["mapbox_id"]
                    .as_str()
                    .expect("id")
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, expected, "{query}");
    }
}

#[tokio::test]
async fn search_box_reverse_keys_context_by_id() {
    let pack = write_pack();
    let router = router(&pack);
    let (status, body) = get(
        &router,
        "/compat/mapbox/search/searchbox/v1/reverse?longitude=-79.0&latitude=43.0&types=address",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let address = id("osm:node:10");
    assert_eq!(
        body["features"][0]["properties"]["context"]["address"],
        json!({
            "id": address,
            "name": "10 King Street",
            "address_number": "10",
            "street_name": "King Street",
        })
    );
    assert_eq!(
        body["features"][0]["properties"]["context"].get("secondary_address"),
        None
    );
    assert_eq!(body["features"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn search_box_errors_are_mapbox_errors() {
    let pack = write_pack();
    let router = router(&pack);
    let base = "/compat/mapbox/search/searchbox/v1";
    for (uri, status, message) in [
        (
            format!("{base}/suggest"),
            StatusCode::BAD_REQUEST,
            "q is required",
        ),
        (
            format!("{base}/forward?q="),
            StatusCode::BAD_REQUEST,
            "q is required",
        ),
        (
            format!("{base}/suggest?q=a&types=moon"),
            StatusCode::BAD_REQUEST,
            "Type \"moon\" is not a known type. Must be one of: country, region, place, \
             district, postcode, locality, neighborhood, and address",
        ),
        (
            format!("{base}/suggest?q=a&proximity=1"),
            StatusCode::BAD_REQUEST,
            "focus must be 2 comma-separated numbers, lon,lat",
        ),
        (
            format!("{base}/retrieve/bm90LW9ncw"),
            StatusCode::NOT_FOUND,
            "Not Found",
        ),
        (
            format!("{base}/retrieve/{}", id("osm:node:999")),
            StatusCode::NOT_FOUND,
            "Not Found",
        ),
        (
            format!("{base}/retrieve/{}", id("osm:moon:1")),
            StatusCode::NOT_FOUND,
            "Not Found",
        ),
        (
            format!("{base}/category/coffee"),
            StatusCode::NOT_IMPLEMENTED,
            "Not Implemented",
        ),
        (
            format!("{base}/list/category"),
            StatusCode::NOT_IMPLEMENTED,
            "Not Implemented",
        ),
        (
            format!("{base}/details"),
            StatusCode::NOT_FOUND,
            "Not Found",
        ),
    ] {
        let (got, body) = get(&router, &uri).await;
        assert_eq!(got, status, "{uri}");
        assert_eq!(body, json!({"message": message}), "{uri}");
    }
}

fn common(layer: Layer, name: &str, label: &str, unit: Option<&str>) -> Common {
    Common {
        id: "osm:node:1".to_string(),
        layer,
        accuracy: Accuracy::Rooftop,
        name: name.to_string(),
        label: label.to_string(),
        category: None,
        road_class: None,
        brand: None,
        contact: None,
        opening_hours: None,
        bbox: None,
        timezone: None,
        access: Vec::new(),
        address_lines: Vec::new(),
        address: Address {
            unit: unit.map(str::to_string),
            ..Address::default()
        },
        context: Context::default(),
        source: Source {
            dataset: "osm".to_string(),
        },
        translations: None,
        record_id: None,
        local_name: None,
    }
}

#[test]
fn the_label_is_cut_at_og_label_parts() {
    let cut = |layer, name, label, unit, poi_address| {
        split_label(&common(layer, name, label, unit), poi_address)
    };
    let some = |text: &str| Some(text.to_string());
    assert_eq!(
        cut(
            Layer::Address,
            "1 A St",
            "1 A St, Unit 2, Town, 1000",
            Some("2"),
            None
        ),
        (some("1 A St, Unit 2, Town, 1000"), some("Town, 1000"))
    );
    assert_eq!(
        cut(
            Layer::Poi,
            "Cafe",
            "Cafe, 1 A St, Town",
            None,
            Some("1 A St")
        ),
        (some("1 A St, Town"), some("Town"))
    );
    assert_eq!(
        cut(Layer::Poi, "Cafe", "Cafe, Town", None, None),
        (some("Town"), some("Town"))
    );
    assert_eq!(
        cut(Layer::Country, "Canada", "Canada", None, None),
        (some("Canada"), None)
    );
    // A label that does not start with the name is not cut.
    assert_eq!(
        cut(Layer::Street, "A St", "Other, Town", None, None),
        (some("Other, Town"), None)
    );
    assert_eq!(
        cut(Layer::Poi, "Cafe", "Other, Town", None, None),
        (None, None)
    );
}

#[test]
fn attribution_credits_og_sources() {
    let credit = |text: &str, licence: Option<&str>, url: Option<&str>| Attribution {
        text: text.to_string(),
        licence: licence.map(str::to_string),
        url: url.map(str::to_string),
    };
    assert_eq!(
        attribution(&[
            credit(
                "© OpenStreetMap contributors",
                Some("ODbL 1.0"),
                Some("https://www.openstreetmap.org/copyright")
            ),
            credit("OpenAddresses x", Some("https://l"), Some("https://l")),
            credit("plain", None, None),
        ]),
        "© OpenStreetMap contributors, ODbL 1.0 (https://www.openstreetmap.org/copyright); \
         OpenAddresses x (https://l); plain"
    );
}

/// `language` names results in its first language. With more than one, each
/// layer Mapbox translates lists every language asked, the first too.
#[tokio::test]
async fn geocoding_language_names_layers_and_translates_them() {
    let pack = write_pack();
    let router = router(&pack);
    let forward = "/compat/mapbox/search/geocode/v6/forward?q=10%20King%20Street&types=address";

    let (status, local) = get(&router, forward).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        local["features"][0]["properties"]["context"]["country"]["name"],
        "Canada"
    );

    let (_, german) = get(&router, &format!("{forward}&language=de")).await;
    let country = &german["features"][0]["properties"]["context"]["country"];
    assert_eq!(country["name"], "Kanada");
    assert!(country.get("translations").is_none());

    let both_languages = |de: &str, fr: &str| {
        json!({
            "de": {"language": "de", "name": de},
            "fr": {"language": "fr", "name": fr},
        })
    };
    let (_, both) = get(&router, &format!("{forward}&language=de,fr")).await;
    let context = &both["features"][0]["properties"]["context"];
    assert_eq!(
        context["country"],
        json!({
            "mapbox_id": id("osm:relation:1"),
            "name": "Kanada",
            "country_code": "CA",
            "country_code_alpha_3": "CAN",
            "translations": both_languages("Kanada", "Canada"),
        })
    );
    // og has no German name for Ontario and no name in either language for
    // Toronto or Downtown, so each keeps its local name in that language.
    assert_eq!(context["region"]["name"], "Ontario");
    assert_eq!(
        context["region"]["translations"],
        both_languages("Ontario", "Ontario")
    );
    assert_eq!(
        context["place"]["translations"],
        both_languages("Toronto", "Toronto")
    );
    assert_eq!(
        context["neighborhood"]["translations"],
        both_languages("Downtown", "Downtown")
    );
    // Mapbox translates only these five layers.
    for layer in ["address", "street", "postcode", "secondary_address"] {
        assert!(context[layer].get("translations").is_none(), "{layer}");
    }

    // A later language og holds no name in reads the local name, not the
    // first language's.
    let (_, italian) = get(&router, &format!("{forward}&language=de,it")).await;
    assert_eq!(
        italian["features"][0]["properties"]["context"]["country"]["translations"],
        json!({
            "de": {"language": "de", "name": "Kanada"},
            "it": {"language": "it", "name": "Canada"},
        })
    );

    // An area result is its own layer, translated as any other.
    let (_, ontario) = get(
        &router,
        "/compat/mapbox/search/geocode/v6/forward?q=Ontario&types=region&language=fr,de",
    )
    .await;
    assert_eq!(
        ontario["features"][0]["properties"]["context"]["region"]["translations"],
        json!({
            "fr": {"language": "fr", "name": "Ontario"},
            "de": {"language": "de", "name": "Ontario"},
        })
    );

    let (_, reversed) = get(
        &router,
        "/compat/mapbox/search/geocode/v6/reverse?longitude=-79.0&latitude=43.0&language=de,fr",
    )
    .await;
    assert_eq!(
        reversed["features"][0]["properties"]["context"]["country"]["translations"]["fr"]["name"],
        "Canada"
    );

    let (status, batch) = send(
        &router,
        "POST",
        "/compat/mapbox/search/geocode/v6/batch",
        Some(r#"[{"q": "10 King Street", "types": "address", "language": "de,fr"}]"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(batch["batch"][0], both);

    for (language, message) in [
        ("d3", "\"d3\" is not a valid language code"),
        (
            "de,DE",
            "options.language should be a list of unique language codes",
        ),
        (
            "aa,ab,ae,af,ak,am,an,ar,as,av,ay,az,ba,be,bg,bh,bi,bm,bn,bo,br",
            "options.language should be a list of no more than 20 languages",
        ),
    ] {
        let (status, body) = get(&router, &format!("{forward}&language={language}")).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{language}");
        assert_eq!(body, json!({"message": message}), "{language}");
    }
}

/// Search Box names results in the first language it is sent and never
/// translates.
#[tokio::test]
async fn search_box_names_results_in_its_first_language() {
    let pack = write_pack();
    let router = router(&pack);
    let (status, body) = get(
        &router,
        "/compat/mapbox/search/searchbox/v1/forward?q=10%20King%20Street&language=de,fr",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let country = &body["features"][0]["properties"]["context"]["country"];
    assert_eq!(country["name"], "Kanada");
    assert!(country.get("translations").is_none());
}
