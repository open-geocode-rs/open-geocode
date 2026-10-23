//! The response contract checked against what the code produces (ADR 0022):
//! real `/search`, `/autocomplete` and `/reverse` answers from the router on
//! a small Pack are the committed examples in `contract/responses/`, and the
//! geocode batch audit columns are the committed
//! `contract/batch-audit-columns.txt`. A change to the shape of an answer
//! fails here until its examples are regenerated, with
//! `OG_UPDATE_CONTRACT=1 cargo test contract`, and committed with it.

use std::path::{Path, PathBuf};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use geojson::{Geometry, GeometryValue};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::{
    batch,
    confidence::{FEATURES, Model},
    context::{AdminContextTuple, RecordContext},
    pack::{RecordId, Writer},
    record::{
        AddressComponents, AddressRecord, InterpolationAddressComponents, InterpolationRange,
        InterpolationRecord, LocationPrecision, ObjectType, PlaceLayer, PlaceRecord, PoiRecord,
        Record, SourceProvenance, StreetRecord, point_geometry,
    },
    runtime::{AppState, build_router},
};

/// Set to write the live answers and columns over the committed files.
const UPDATE: &str = "OG_UPDATE_CONTRACT";

/// Check `text` against the committed file `contract/<file>`, or write it
/// there when [`UPDATE`] is set.
fn assert_committed(file: &str, text: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("contract")
        .join(file);
    if std::env::var_os(UPDATE).is_some() {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("contract directory");
        std::fs::write(&path, text)
            .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
        return;
    }
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .replace("\r\n", "\n");
    assert!(
        committed == text,
        "contract/{file} differs from what the code produces. If the change is meant, run \
         `{UPDATE}=1 cargo test contract` and commit the result.\n{text}"
    );
}

/// Check one answer against its committed example.
fn assert_example(name: &str, body: &Value) {
    let mut text = serde_json::to_string_pretty(body).expect("answer serializes");
    text.push('\n');
    assert_committed(&format!("responses/{name}.json"), &text);
}

/// The committed contract is the one the code generates, and the one the
/// server actually hands out at `/openapi.json`.
#[tokio::test]
async fn committed_openapi_is_the_served_contract() {
    let document = crate::http::openapi::document().expect("spec serializes");
    assert_committed("openapi.json", &document);

    let pack = contract_pack();
    let router = build_router(
        AppState::open(&pack).expect("open pack"),
        Some(&pack.join("demo")),
        Some(&pack.join("basemap.pmtiles")),
    );
    let response = router
        .oneshot(
            Request::get("/openapi.json")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK);
    let served = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    assert_eq!(
        String::from_utf8(served.to_vec()).expect("spec is UTF-8"),
        document,
        "/openapi.json differs from the committed contract"
    );
    let _ = std::fs::remove_dir_all(&pack);
}

/// The committed contract survives release-please rewriting its version.
///
/// `release-please-config.json` lists `contract/openapi.json` as an extra
/// file, and release-please rewrites a JSON file by parsing it and
/// re-serializing with `JSON.stringify(value, null, 2)`. If the committed
/// text were not already exactly that, every release would reformat the file
/// and the drift test above would fail on main until someone regenerated it.
///
/// Keeping the file a fixed point of that transform is what makes the two
/// safe together. The risk is integral floats: a bound written `-180.0` in
/// Rust would serialize as `-180.0` but come back `-180`. Today none survive
/// serialization, and this notices if one ever does.
#[test]
fn the_contract_survives_a_release_version_bump() {
    let document = crate::http::openapi::document().expect("spec serializes");
    let value: Value = serde_json::from_str(&document).expect("spec is JSON");
    let mut restringified = serde_json::to_string_pretty(&value).expect("re-serializes");
    restringified.push('\n');
    assert_eq!(
        restringified, document,
        "the contract is not a fixed point of parse-then-serialize, so a release \
         would reformat it"
    );
}

/// Every `$ref` in the contract resolves, and every schema in it is reachable.
///
/// A schema that is only ever reached through a parameter is not collected
/// automatically, so a dangling `$ref` is the failure this guards: the spec
/// would still parse, and would still be wrong.
#[test]
fn every_schema_reference_resolves() {
    let document = crate::http::openapi::document().expect("spec serializes");
    let spec: Value = serde_json::from_str(&document).expect("spec is JSON");
    let defined: std::collections::BTreeSet<String> = spec["components"]["schemas"]
        .as_object()
        .expect("components.schemas")
        .keys()
        .cloned()
        .collect();

    let mut referenced = std::collections::BTreeSet::new();
    collect_refs(&spec, &mut referenced);

    let dangling: Vec<_> = referenced.difference(&defined).collect();
    assert!(dangling.is_empty(), "unresolved $ref: {dangling:?}");
    let unused: Vec<_> = defined.difference(&referenced).collect();
    assert!(unused.is_empty(), "schemas nothing points at: {unused:?}");
}

/// The contract never admits a null, because the API never sends one: an
/// absent value is an absent key.
#[test]
fn the_contract_admits_no_nulls() {
    let document = crate::http::openapi::document().expect("spec serializes");
    assert!(
        !document.contains("\"null\""),
        "the contract still allows a null somewhere"
    );
}

fn collect_refs(value: &Value, found: &mut std::collections::BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref")
                && let Some(name) = reference.strip_prefix("#/components/schemas/")
            {
                found.insert(name.to_string());
            }
            for item in map.values() {
                collect_refs(item, found);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_refs(item, found);
            }
        }
        _ => {}
    }
}

#[test]
fn committed_audit_columns_are_the_batch_columns() {
    assert_committed(batch::AUDIT_COLUMNS_FILE, &batch::audit_columns_text());
}

/// Write the contract Pack where `OG_CONTRACT_PACK` points, for serving it
/// by hand:
///
/// ```text
/// OG_CONTRACT_PACK=/tmp/contract-pack cargo test write_the_contract_pack -- --ignored
/// open-geocode serve --pack /tmp/contract-pack --bind 127.0.0.1:8080
/// ```
///
/// Ignored because it writes outside the temp dir it cleans up, and exists
/// so the committed examples can be checked against a running server.
#[test]
#[ignore = "writes a Pack to OG_CONTRACT_PACK for serving by hand"]
fn write_the_contract_pack() {
    let path = std::env::var_os("OG_CONTRACT_PACK")
        .map(PathBuf::from)
        .expect("set OG_CONTRACT_PACK to where the Pack should be written");
    write_pack(&path);
    println!("wrote the contract Pack to {}", path.display());
}

/// A Pack holding the handful of records the examples are built from, in a
/// directory of its own so tests can run side by side.
fn contract_pack() -> PathBuf {
    let pack = std::env::temp_dir().join(format!(
        "open-geocode-contract-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    write_pack(&pack);
    pack
}

#[tokio::test]
async fn responses_match_the_committed_examples() {
    let pack = std::env::temp_dir().join(format!(
        "open-geocode-contract-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let ids = write_pack(&pack);
    let router = build_router(
        AppState::open(&pack).expect("open pack"),
        Some(&pack.join("demo")),
        Some(&pack.join("basemap.pmtiles")),
    );

    let address = get(&router, "/search?q=10%20King%20Street").await;
    assert_example("search-address", &address);
    assert_eq!(address["type"], "FeatureCollection");
    assert_eq!(address["geocoding"]["query"], "10 King Street");
    let top = &address["features"][0];
    assert_eq!(top["geometry"]["coordinates"], json!([-79.0, 43.0]));
    let properties = &top["properties"];
    assert_eq!(properties["layer"], "address");
    assert_eq!(properties["accuracy"], "rooftop");
    assert_eq!(properties["name"], "10 King Street");
    assert_eq!(
        properties["address"],
        json!({"housenumber": "10", "street": "King Street", "postcode": "M5V 1A1"})
    );
    assert_eq!(
        properties["context"],
        json!({
            "locality": {"id": "osm:relation:3", "name": "Toronto"},
            "region": {"id": "osm:relation:2", "name": "Ontario"},
            "country": {"id": "osm:relation:1", "name": "Canada"},
        })
    );
    assert!(properties["score"].is_number());
    let confidence = properties["confidence"].as_f64().expect("a confidence");
    assert!((0.0..=1.0).contains(&confidence), "{confidence}");
    assert!(properties.get("distance_m").is_none());

    let poi = get(&router, "/search?q=Queen%20Bakery").await;
    assert_example("search-poi", &poi);
    let properties = &poi["features"][0]["properties"];
    assert_eq!(properties["layer"], "poi");
    assert_eq!(properties["name"], "Queen Bakery");
    assert_eq!(properties["category"], "shop:bakery");
    assert_eq!(properties["accuracy"], "centroid");

    let estimated = get(&router, "/search?q=50%20Queen%20Street").await;
    assert_example("search-interpolation", &estimated);
    let properties = &estimated["features"][0]["properties"];
    assert_eq!(properties["layer"], "interpolation");
    assert_eq!(properties["accuracy"], "interpolated");
    assert_eq!(properties["name"], "50 Queen Street");
    assert_eq!(properties["address"]["housenumber"], "50");

    // An address the Pack lacks, in a town it holds, is answered with the
    // town, and the answer says so; an answer that matches says nothing.
    let fallback = get(&router, "/search?q=99%20Nowhere%20Lane%2C%20Toronto").await;
    assert_example("search-fallback", &fallback);
    let properties = &fallback["features"][0]["properties"];
    assert_eq!(properties["fallback"], true);
    assert_eq!(properties["layer"], "locality");
    assert_eq!(properties["name"], "Toronto");
    assert!(
        address["features"][0]["properties"]
            .get("fallback")
            .is_none()
    );

    for (name, uri) in [
        ("search-street", "/search?q=King%20Street"),
        ("search-locality", "/search?q=Toronto"),
    ] {
        assert_example(name, &get(&router, uri).await);
    }
    // A caller's location ranks ties and a box filters: the street the
    // box holds answers, and one wholly outside it does not.
    let near = get(
        &router,
        "/search?q=King%20Street&focus=-79.0,43.0&bbox=-79.01,42.99,-78.99,43.01",
    )
    .await;
    assert_example("search-near", &near);
    assert!(!near["features"].as_array().expect("features").is_empty());
    let far = get(&router, "/search?q=King%20Street&bbox=10,10,11,11").await;
    assert_eq!(far["features"], json!([]));
    let (status, crossed) = get_error(&router, "/search?q=king&bbox=170,-10,-170,10").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(crossed["error_code"], "invalid_coordinate");
    assert_eq!(crossed["invalid_params"][0]["name"], "bbox");

    let suggestions = get(&router, "/autocomplete?q=Quee").await;
    assert_example("autocomplete", &suggestions);
    assert!(
        !suggestions["features"]
            .as_array()
            .expect("features")
            .is_empty()
    );

    // On the address: the address, then the areas it lies in, precise to
    // broad.
    let on_address = get(&router, "/reverse?lon=-79.0&lat=43.0").await;
    assert_example("reverse-address", &on_address);
    assert_eq!(
        on_address["geocoding"]["point"],
        json!({"lon": -79.0, "lat": 43.0})
    );
    let features = on_address["features"].as_array().expect("features");
    let layers = features
        .iter()
        .map(|feature| feature["properties"]["layer"].as_str().expect("layer"))
        .collect::<Vec<_>>();
    assert_eq!(layers, ["address", "locality", "region", "country"]);
    assert_eq!(features[0]["properties"]["record_id"], ids.address);
    assert_eq!(features[0]["properties"]["distance_m"], 0.0);
    assert!(features[1]["properties"].get("distance_m").is_none());
    assert!(features[0]["properties"].get("score").is_none());

    // Off every address but on the range: an estimated house number.
    let on_range = get(&router, "/reverse?lon=-79.0102&lat=43.0005").await;
    assert_example("reverse-interpolation", &on_range);
    let properties = &on_range["features"][0]["properties"];
    assert_eq!(properties["accuracy"], "interpolated");
    assert_eq!(
        properties["address"]["housenumber"],
        properties["name"]
            .as_str()
            .expect("name")
            .split(' ')
            .next()
            .expect("number")
    );

    // Far from everything but the areas.
    let context_only = get(&router, "/reverse?lon=-79.2&lat=43.2").await;
    assert_example("reverse-areas", &context_only);

    let nothing = get(&router, "/reverse?lon=10.0&lat=10.0").await;
    assert_example("reverse-nothing", &nothing);
    assert_eq!(nothing["features"], json!([]));

    // `/geocode` answers with the same collection as `/search`, holding the
    // one address that fits, so a client parses one shape for both.
    let structured = get(
        &router,
        "/geocode?address=10%20King%20Street&postcode=M5V%201A1",
    )
    .await;
    assert_example("geocode-address", &structured);
    assert_eq!(structured["type"], "FeatureCollection");
    assert_eq!(
        structured["features"].as_array().expect("features").len(),
        1
    );
    assert_eq!(structured["features"][0]["properties"]["layer"], "address");

    // Nothing fits: an empty collection, not an error and not a null.
    let unmatched = get(
        &router,
        "/geocode?address=10%20King%20Street&postcode=V6B%201A1",
    )
    .await;
    assert_example("geocode-nothing", &unmatched);
    assert!(
        unmatched["features"]
            .as_array()
            .expect("features")
            .is_empty()
    );

    // Errors are part of the contract too: a client branches on `error_code`
    // and shows `detail`, so both are committed like any answer.
    let (status, empty_query) = get_error(&router, "/search?q=").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_example("error-invalid-query", &empty_query);
    assert_eq!(empty_query["error_code"], "invalid_query");
    assert_eq!(empty_query["type"], "about:blank");
    assert_eq!(empty_query["request_id"], "contract-example");
    assert!(empty_query.get("instance").is_none());

    let (status, out_of_range) = get_error(&router, "/reverse?lon=-79&lat=999").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_example("error-invalid-coordinate", &out_of_range);
    assert_eq!(out_of_range["error_code"], "invalid_coordinate");
    assert_eq!(out_of_range["invalid_params"][0]["name"], "lat");

    let (status, unknown_layer) = get_error(&router, "/search?q=king&layer=bogus").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_example("error-malformed-parameter", &unknown_layer);
    assert_eq!(unknown_layer["error_code"], "malformed_parameter");

    // A country code that is not two letters is a typo, and is rejected like
    // an unknown layer rather than silently matching nothing.
    let (status, bad_country) = get_error(&router, "/search?q=king&country=Canada").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_country["error_code"], "malformed_parameter");
    assert_eq!(bad_country["invalid_params"][0]["name"], "country");

    // A well-formed country the Pack does not cover is not an error: it is
    // an empty answer.
    let elsewhere = get(&router, "/search?q=king&country=FR").await;
    assert_eq!(
        elsewhere["features"].as_array().expect("features").len(),
        0,
        "a country the Pack does not hold answers with no features"
    );

    // The country the contract Pack is built from finds the same places the
    // unfiltered query does, so the filter narrows without changing ranking.
    let unfiltered = get(&router, "/search?q=king").await;
    let filtered = get(&router, "/search?q=king&country=CA").await;
    assert_eq!(unfiltered["features"], filtered["features"]);

    let _ = std::fs::remove_dir_all(&pack);
}

/// A request that is expected to fail, returning its status and problem body.
/// The request id is fixed so the committed example is stable.
async fn get_error(router: &Router, uri: &str) -> (StatusCode, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::get(uri)
                .header("x-request-id", "contract-example")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("router response");
    let status = response.status();
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/problem+json"),
        "{uri} should be served as a problem document"
    );
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    (
        status,
        serde_json::from_slice(&body).expect("problem is JSON"),
    )
}

async fn get(router: &Router, uri: &str) -> Value {
    let response = router
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).expect("request"))
        .await
        .expect("router response");
    assert_eq!(response.status(), StatusCode::OK, "{uri}");
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    serde_json::from_slice(&body).expect("response body is JSON")
}

struct Ids {
    address: RecordId,
}

/// Canada > Ontario > Toronto, an address, a POI with an address, a street
/// and an interpolation range in Toronto.
fn write_pack(dir: &PathBuf) -> Ids {
    let _ = std::fs::remove_dir_all(dir);
    let mut writer = Writer::create(dir).expect("writer");
    let place = |writer: &mut Writer, layer, name: &str, id| {
        writer
            .write(
                &Record::Place(
                    layer,
                    PlaceRecord {
                        alt_names: Vec::new(),
                        name: name.to_string(),
                        place_type: "admin_level:8".to_string(),
                        geometry: point_geometry(-79.0, 43.0),
                        source: SourceProvenance::osm(ObjectType::Relation, id),
                    },
                ),
                None,
            )
            .expect("place")
    };
    let country = place(&mut writer, PlaceLayer::Country, "Canada", 1);
    let region = place(&mut writer, PlaceLayer::Region, "Ontario", 2);
    let locality = place(&mut writer, PlaceLayer::Locality, "Toronto", 3);
    // The country record's ISO code, as a build derives it from the boundary.
    // Without it no record carries a country and the `country` filter, which
    // matches on the code, would find nothing.
    writer.set_country_codes(
        [(
            country,
            crate::parse::lattice::CountryCode::parse("CA").expect("code"),
        )]
        .into_iter()
        .collect(),
    );
    let context = Some(RecordContext {
        admin_context: AdminContextTuple::from_tiers([
            (PlaceLayer::Country, country),
            (PlaceLayer::Region, region),
            (PlaceLayer::Locality, locality),
        ]),
        flags: 0,
    });
    let king = AddressComponents {
        areas: Vec::new(),
        number: "10".to_string(),
        street: Some("King Street".to_string()),
        place: None,
        unit: None,
        locality: None,
        region: None,
        postcode: Some("M5V 1A1".to_string()),
        country: None,
    };
    let address = writer
        .write(
            &AddressRecord {
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
                name: "Queen Bakery".to_string(),
                category: "shop:bakery".to_string(),
                address: Some(AddressComponents {
                    number: "12".to_string(),
                    ..king
                }),
                geometry: point_geometry(-79.005, 43.0),
                location_precision: LocationPrecision::Centroid,
                source: SourceProvenance::osm(ObjectType::Way, 11),
                prominence: Default::default(),
            }
            .into(),
            context.clone(),
        )
        .expect("poi");
    writer
        .write(
            &StreetRecord {
                alt_names: Vec::new(),
                locality: None,
                name: "King Street".to_string(),
                geometry: line(-79.0, 43.0, -79.0, 43.001),
                representative_point: [-79.0, 43.0005],
                source: SourceProvenance::osm(ObjectType::Way, 12),
            }
            .into(),
            context.clone(),
        )
        .expect("street");
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
                geometry: line(-79.01, 43.0, -79.01, 43.001),
                representative_point: [-79.01, 43.0005],
                source: SourceProvenance::osm(ObjectType::Way, 13),
            }
            .into(),
            context.clone(),
        )
        .expect("interpolation");
    writer.finish().expect("finish");
    // Calibrated, so the examples show what a calibrated Pack answers. The
    // weights are made up: the fit has its own tests.
    let pack = crate::pack::Reader::open(dir).expect("pack");
    Model::new(CONTRACT_WEIGHTS, Default::default())
        .save(&pack)
        .expect("confidence model");
    Ids { address }
}

/// A plausible model: an answer is likelier right on a strict pass, with
/// its number, in its postcode and well clear of the next.
const CONTRACT_WEIGHTS: [f64; FEATURES] = [
    1.5, -1.0, -1.5, -0.8, -1.5, -1.0, -0.7, -2.0, -1.5, 0.8, -2.5, -0.2, 0.5, -2.0, -0.3, 1.0,
    -1.5, 0.3, -0.3, -1.0, -0.6, 1.2, -1.5,
];

fn line(from_lon: f64, from_lat: f64, to_lon: f64, to_lat: f64) -> Geometry {
    Geometry::new(GeometryValue::LineString {
        coordinates: vec![vec![from_lon, from_lat].into(), vec![to_lon, to_lat].into()],
    })
}
