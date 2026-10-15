//! The response contract checked against what the code produces (ADR 0022):
//! real `/search`, `/autocomplete` and `/reverse` answers from the router on
//! a small Pack are the committed examples in `contract/responses/`, and the
//! batch-geocode audit columns are the committed
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
    context::{AdminContextTuple, RecordContext},
    pack::{PackWriter, RecordId},
    record::{
        AddressComponents, AddressRecord, InterpolationAddressComponents, InterpolationRange,
        InterpolationRecord, LocationPrecision, OsmObjectType, PlaceLayer, PlaceRecord, PoiRecord,
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

#[test]
fn committed_audit_columns_are_the_batch_columns() {
    assert_committed(batch::AUDIT_COLUMNS_FILE, &batch::audit_columns_text());
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
        &pack.join("demo"),
        &pack.join("basemap.pmtiles"),
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

    for (name, uri) in [
        ("search-street", "/search?q=King%20Street"),
        ("search-locality", "/search?q=Toronto"),
    ] {
        assert_example(name, &get(&router, uri).await);
    }
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

    let _ = std::fs::remove_dir_all(&pack);
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
    let mut writer = PackWriter::create(dir).expect("writer");
    let place = |writer: &mut PackWriter, layer, name: &str, id| {
        writer
            .write(
                &Record::Place(
                    layer,
                    PlaceRecord {
                        alt_names: Vec::new(),
                        name: name.to_string(),
                        place_type: "admin_level:8".to_string(),
                        geometry: point_geometry(-79.0, 43.0),
                        source: SourceProvenance::osm(OsmObjectType::Relation, id),
                    },
                ),
                None,
            )
            .expect("place")
    };
    let country = place(&mut writer, PlaceLayer::Country, "Canada", 1);
    let region = place(&mut writer, PlaceLayer::Region, "Ontario", 2);
    let locality = place(&mut writer, PlaceLayer::Locality, "Toronto", 3);
    let context = Some(RecordContext {
        admin_context: AdminContextTuple {
            country_record_id: Some(country),
            region_record_id: Some(region),
            locality_record_id: Some(locality),
            ..AdminContextTuple::default()
        },
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
                address: king.clone(),
                geometry: point_geometry(-79.0, 43.0),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(OsmObjectType::Node, 10),
            }
            .into(),
            context,
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
                source: SourceProvenance::osm(OsmObjectType::Way, 11),
            }
            .into(),
            context,
        )
        .expect("poi");
    writer
        .write(
            &StreetRecord {
                alt_names: Vec::new(),
                name: "King Street".to_string(),
                geometry: line(-79.0, 43.0, -79.0, 43.001),
                representative_point: [-79.0, 43.0005],
                source: SourceProvenance::osm(OsmObjectType::Way, 12),
            }
            .into(),
            context,
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
                source: SourceProvenance::osm(OsmObjectType::Way, 13),
            }
            .into(),
            context,
        )
        .expect("interpolation");
    writer.finish().expect("finish");
    Ids { address }
}

fn line(from_lon: f64, from_lat: f64, to_lon: f64, to_lat: f64) -> Geometry {
    Geometry::new(GeometryValue::LineString {
        coordinates: vec![vec![from_lon, from_lat].into(), vec![to_lon, to_lat].into()],
    })
}
