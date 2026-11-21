use std::{num::NonZeroUsize, path::PathBuf};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{Area, Context, attribution, place_line};
use crate::{
    api::Attribution,
    compat::encode_parts,
    context::{AdminContextTuple, RecordContext},
    limits::Limits,
    pack::Writer,
    record::{
        AddressComponents, AddressRecord, Brand, Contact, DerivedSourceProvenance, Entrance,
        InterpolationAddressComponents, InterpolationRange, InterpolationRecord, Layer,
        LocationPrecision, ObjectType, PlaceLayer, PlaceRecord, PoiDetails, PoiRecord,
        PostcodeRecord, Record, SourceProvenance, point_geometry,
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
/// The vendor id og issues for an address of the fixture: its og id, its
/// layer and its label, since the catalogue holds no address.
fn address_vendor_id(og_id: &str) -> String {
    let label = match og_id {
        "osm:node:10" => "10 King Street, Unit 2, Toronto, M5V 1A1",
        _ => panic!("{og_id} is no address of the fixture"),
    };
    encode_parts(og_id, Some(Layer::Address), label)
}

fn write_pack() -> Pack {
    let dir = std::env::temp_dir().join(format!(
        "open-geocode-compat-mapbox-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let mut writer = Writer::create(&dir).expect("writer");
    // Each area lies in the areas written before it, as a build gives it.
    let place = |writer: &mut Writer, layer, name: &str, id, lon, within: &[(PlaceLayer, u64)]| {
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
                        // Canada's item, which Mapbox writes on its layer.
                        wikidata: (id == 1).then_some(16),
                    },
                ),
                (!within.is_empty()).then(|| RecordContext {
                    admin_context: AdminContextTuple::from_tiers(within.iter().copied()),
                    flags: 0,
                    zone: None,
                }),
            )
            .expect("place")
    };
    let country = place(&mut writer, PlaceLayer::Country, "Canada", 1, -80.0, &[]);
    let in_country = [(PlaceLayer::Country, country)];
    let region = place(
        &mut writer,
        PlaceLayer::Region,
        "Ontario",
        2,
        -79.5,
        &in_country,
    );
    let in_region = [in_country[0], (PlaceLayer::Region, region)];
    let locality = place(
        &mut writer,
        PlaceLayer::Locality,
        "Toronto",
        3,
        -79.2,
        &in_region,
    );
    // Drawn as an outline, so og knows its box.
    writer.set_extents(vec![(
        locality,
        [-793_000_000, 429_000_000, -789_000_000, 431_000_000],
    )]);
    let neighbourhood = place(
        &mut writer,
        PlaceLayer::Neighbourhood,
        "Downtown",
        4,
        -79.1,
        &[in_region[0], in_region[1], (PlaceLayer::Locality, locality)],
    );
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
    // The postcode the addresses state, as a build derives it from them.
    writer
        .write(
            &PostcodeRecord {
                postcode: "M5V 1A1".to_string(),
                country: crate::parse::lattice::CountryCode::parse("CA"),
                geometry: point_geometry(-79.0, 43.0),
                source: DerivedSourceProvenance::osm_address_records(2),
            }
            .into(),
            context.clone(),
        )
        .expect("postcode");
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
                access: vec![Entrance {
                    at: [-790_001_000, 430_001_000],
                    kind: "main".to_string(),
                }],
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
                details: PoiDetails {
                    contact: Some(Contact {
                        phone: Some("+1 416-555-0111".to_string()),
                        website: Some("https://queencafe.example/".to_string()),
                        email: Some("hello@queencafe.example".to_string()),
                    }),
                    opening_hours: Some("Mo-Fr 08:00-18:00; Sa 09:00-13:00".to_string()),
                    extent: Some([-790_052_000, 429_999_000, -790_048_000, 430_001_000]),
                    access: vec![Entrance {
                        at: [-790_050_000, 430_001_000],
                        kind: "yes".to_string(),
                    }],
                }
                .kept(),
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

/// The `mapbox_id` of a context area.
fn id(og_id: &str) -> String {
    encode_parts(og_id, None, "")
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
            "wikidata_id": "Q16",
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
    let address = address_vendor_id("osm:node:10");
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
                    "full_address": "10 King Street, Unit 2, Toronto, Ontario M5V 1A1, Canada",
                    "name": "10 King Street",
                    // og's own point of the address, a known address point.
                    "coordinates": {
                        "longitude": -79.0,
                        "latitude": 43.0,
                        "accuracy": "point",
                        "routable_points": [
                            {"name": "entrance", "longitude": -79.0001, "latitude": 43.0001},
                        ],
                    },
                    "place_formatted": "Toronto, Ontario M5V 1A1, Canada",
                    // The query states the number and street, both matched.
                    // Unstated parts are not in the input, and the country
                    // og fills in is inferred.
                    "match_code": {
                        "address_number": "matched",
                        "street": "matched",
                        "postcode": "unmatched",
                        "place": "unmatched",
                        "region": "unmatched",
                        "country": "inferred",
                        "confidence": "exact",
                    },
                    "context": merged(areas(), json!({
                        "address": {
                            "mapbox_id": address,
                            "name": "10 King Street",
                            "address_number": "10",
                            "street_name": "King Street",
                        },
                        // The address states the flat, so it is the flat's record.
                        "secondary_address": {"mapbox_id": address, "name": "2"},
                        "street": {"mapbox_id": address, "name": "King Street"},
                        // The postcode record og derived from its addresses.
                        "postcode": {
                            "mapbox_id": encode_parts(
                                "derived:osm:postcode:ca:M5V%201A1",
                                Some(Layer::Postcode),
                                "M5V 1A1",
                            ),
                            "name": "M5V 1A1",
                        },
                    })),
                },
            }],
            "attribution": OSM,
        })
    );
}

#[tokio::test]
async fn every_og_value_lands_in_its_mapbox_slot() {
    let pack = write_pack();
    let router = router(&pack);
    let mut met = std::collections::BTreeSet::new();
    let geocoding = "/compat/mapbox/search/geocode/v6";
    let search_box = "/compat/mapbox/search/searchbox/v1";
    for (native, compat) in [
        (
            "/geocode?q=10%20King%20Street".to_string(),
            format!("{geocoding}/forward?q=10%20King%20Street&autocomplete=false"),
        ),
        (
            "/geocode?q=Queen%20Street".to_string(),
            format!("{geocoding}/forward?q=Queen%20Street&autocomplete=false"),
        ),
        (
            "/geocode?q=Toronto".to_string(),
            format!("{geocoding}/forward?q=Toronto&autocomplete=false"),
        ),
        (
            "/reverse?lon=-79.0&lat=43.0".to_string(),
            format!("{geocoding}/reverse?longitude=-79.0&latitude=43.0"),
        ),
        (
            "/places?q=Queen%20Cafe&focus=-79.0,43.0".to_string(),
            format!("{search_box}/forward?q=Queen%20Cafe&types=poi&proximity=-79.0,43.0"),
        ),
        (
            "/geocode?q=10%20King%20Street&focus=-79.0,43.0".to_string(),
            format!("{search_box}/forward?q=10%20King%20Street&types=address&proximity=-79.0,43.0"),
        ),
        (
            "/reverse?lon=-79.0&lat=43.0".to_string(),
            format!("{search_box}/reverse?longitude=-79.0&latitude=43.0"),
        ),
    ] {
        let searchbox = compat.starts_with(search_box);
        let reverse = native.starts_with("/reverse");
        let (_, og) = get(&router, &native).await;
        let (status, mapbox) = get(&router, &compat).await;
        assert_eq!(status, StatusCode::OK, "{compat}");
        let og_features = og["features"].as_array().expect("features");
        let features = mapbox["features"].as_array().expect("features");
        assert!(!og_features.is_empty(), "{native}");
        assert_eq!(features.len(), og_features.len(), "{compat}");
        for (og_feature, feature) in og_features.iter().zip(features) {
            let og = &og_feature["properties"];
            let properties = &feature["properties"];
            let layer = og["layer"].as_str().expect("layer");
            let at = format!("{compat}: {}", og["id"]);
            let own = encode_parts(
                og["id"].as_str().expect("id"),
                Layer::parse(layer),
                og["label"].as_str().expect("label"),
            );
            if !searchbox {
                assert_eq!(feature["id"], own, "{at}");
            }
            assert_eq!(properties["mapbox_id"], own, "{at}");
            if layer == "address" {
                met.insert("address id");
            }
            assert_eq!(properties["name"], og["name"], "{at}");
            assert_eq!(
                feature["geometry"]["coordinates"], og_feature["geometry"]["coordinates"],
                "{at}"
            );
            let coordinates = &properties["coordinates"];
            let is_address = layer == "address"
                || (layer == "interpolation" && !og["address"]["housenumber"].is_null());
            let accuracy = match (is_address, og["accuracy"].as_str()) {
                (false, _) => None,
                (true, Some("rooftop")) => Some("point"),
                (true, Some("centroid")) => Some("rooftop"),
                (true, Some("interpolated")) => Some("interpolated"),
                (true, Some("street")) if searchbox => Some("street"),
                (true, _) => None,
            };
            assert_eq!(coordinates["accuracy"].as_str(), accuracy, "{at}");
            let entrances = og["access"]
                .as_array()
                .map(|access| {
                    access
                        .iter()
                        .map(|point| {
                            json!({"name": "entrance", "longitude": point["lon"], "latitude": point["lat"]})
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            assert_eq!(
                coordinates["routable_points"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
                entrances,
                "{at}"
            );
            if !entrances.is_empty() {
                met.insert(if searchbox {
                    "access searchbox"
                } else {
                    "access geocoding"
                });
            }
            let boxed = searchbox
                || matches!(
                    layer,
                    "country" | "region" | "postcode" | "district" | "locality" | "neighbourhood"
                );
            match boxed {
                true => assert_eq!(properties["bbox"], og["bbox"], "{at}"),
                false => assert_eq!(properties["bbox"], Value::Null, "{at}"),
            }
            if boxed && !og["bbox"].is_null() {
                met.insert(if searchbox {
                    "bbox searchbox"
                } else {
                    "bbox geocoding"
                });
            }
            if searchbox {
                // Mapbox measures distance from `proximity` only.
                let distance = if reverse {
                    &Value::Null
                } else {
                    &og["distance_m"]
                };
                assert_eq!(&properties["distance"], distance, "{at}");
                if !distance.is_null() {
                    met.insert("distance");
                }
                let metadata = &properties["metadata"];
                for key in ["phone", "website", "email"] {
                    assert_eq!(metadata[key], og["contact"][key], "{at}");
                    if !og["contact"][key].is_null() {
                        met.insert(key);
                    }
                }
                if og["opening_hours"] == "Mo-Fr 08:00-18:00; Sa 09:00-13:00" {
                    let period = |day: u8, open: &str, close: &str| json!({"open": {"day": day, "time": open}, "close": {"day": day, "time": close}});
                    let mut periods = (1..=5)
                        .map(|day| period(day, "0800", "1800"))
                        .collect::<Vec<_>>();
                    periods.push(period(6, "0900", "1300"));
                    assert_eq!(metadata["open_hours"], json!({"periods": periods}), "{at}");
                    met.insert("opening_hours");
                } else {
                    assert_eq!(og["opening_hours"], Value::Null, "{at}");
                    assert_eq!(metadata["open_hours"], Value::Null, "{at}");
                }
            } else {
                assert_eq!(properties["distance"], Value::Null, "{at}");
                assert_eq!(properties["metadata"], Value::Null, "{at}");
            }
            let matched = &og["match"];
            if !searchbox && !reverse && is_address {
                assert!(matched.is_object(), "{at}");
                let word = |part: &Value| match part.as_str() {
                    Some("matched") => "matched",
                    _ => "unmatched",
                };
                let number = match (matched["housenumber"].as_str(), og["accuracy"].as_str()) {
                    (Some("partial"), Some("interpolated")) => "plausible",
                    (part, _) => word(&json!(part)),
                };
                let country =
                    match matched["country"].is_null() && !og["context"]["country"].is_null() {
                        true => "inferred",
                        false => word(&matched["country"]),
                    };
                let changed =
                    |part: &str| matched[part].as_str().is_some_and(|part| part != "matched");
                let others = ["street", "unit", "locality", "postcode", "country"]
                    .into_iter()
                    .filter(|part| changed(part))
                    .count();
                let confidence =
                    match (changed("housenumber") && number != "plausible") || changed("region") {
                        true => "low",
                        false => ["exact", "high", "medium", "low"][others.min(3)],
                    };
                let mut expected = json!({
                    "address_number": number,
                    "street": word(&matched["street"]),
                    "postcode": word(&matched["postcode"]),
                    "place": word(&matched["locality"]),
                    "region": word(&matched["region"]),
                    "country": country,
                    "confidence": confidence,
                });
                if !matched["unit"].is_null() {
                    expected["secondary_address"] = json!(word(&matched["unit"]));
                }
                assert_eq!(properties["match_code"], expected, "{at}");
                met.insert("match");
            } else {
                assert_eq!(properties["match_code"], Value::Null, "{at}");
            }
            let context = &properties["context"];
            let parts = &og["address"];
            if is_address {
                assert_eq!(
                    context["address"]["address_number"], parts["housenumber"],
                    "{at}"
                );
                assert_eq!(context["address"]["street_name"], parts["street"], "{at}");
                assert_eq!(context["street"]["name"], parts["street"], "{at}");
            }
            if !searchbox && layer != "poi" {
                assert_eq!(context["secondary_address"]["name"], parts["unit"], "{at}");
            }
            assert_eq!(context["postcode"]["name"], parts["postcode"], "{at}");
            let key = if searchbox { "id" } else { "mapbox_id" };
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
                    context[slot][key],
                    id(area["id"].as_str().expect("id")),
                    "{at}"
                );
                assert_eq!(context[slot]["name"], area["name"], "{at}");
            }
            if !og["context"]["country"].is_null() {
                assert_eq!(
                    context["country"]["country_code"], og["context"]["country"]["code"],
                    "{at}"
                );
            }
            if let Some(code) = og["context"]["region"]["code"].as_str() {
                assert_eq!(context["region"]["region_code_full"], code, "{at}");
            }
            // An area result carries its own code in its own layer.
            match (layer, og["code"].as_str()) {
                ("country", Some(code)) => {
                    assert_eq!(context["country"]["country_code"], code, "{at}");
                    met.insert("country code");
                }
                ("region", Some(code)) => {
                    assert_eq!(context["region"]["region_code_full"], code, "{at}");
                    let short = code.split_once('-').map(|(_, short)| short);
                    assert_eq!(context["region"]["region_code"].as_str(), short, "{at}");
                    met.insert("region code");
                }
                (_, code) => assert_eq!(code, None, "{at}"),
            }
        }
    }

    // A suggestion carries the distance and metadata a retrieve does.
    let (_, og) = get(&router, "/autocomplete?q=Queen%20Ca&focus=-79.0,43.0").await;
    let (status, suggested) = get(
        &router,
        &format!("{search_box}/suggest?q=Queen%20Ca&session_token=t&proximity=-79.0,43.0"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let cafe = og["features"]
        .as_array()
        .and_then(|features| {
            features
                .iter()
                .find(|feature| feature["properties"]["name"] == "Queen Cafe")
        })
        .expect("og suggests the cafe");
    let suggestion = suggested["suggestions"]
        .as_array()
        .and_then(|all| {
            all.iter()
                .find(|suggestion| suggestion["name"] == "Queen Cafe")
        })
        .expect("Mapbox suggests the cafe");
    assert_eq!(suggestion["distance"], cafe["properties"]["distance_m"]);
    assert!(!suggestion["distance"].is_null());
    assert_eq!(
        suggestion["metadata"]["phone"],
        cafe["properties"]["contact"]["phone"]
    );
    assert_eq!(suggestion["metadata"]["periods"], Value::Null);
    assert_eq!(
        suggestion["metadata"]["open_hours"]["periods"]
            .as_array()
            .map(Vec::len),
        Some(6)
    );

    let every = [
        "access geocoding",
        "access searchbox",
        "address id",
        "bbox geocoding",
        "bbox searchbox",
        "country code",
        "distance",
        "email",
        "match",
        "opening_hours",
        "phone",
        "region code",
        "website",
    ];
    assert_eq!(met.into_iter().collect::<Vec<_>>(), every);
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
    // The town is its own layer, held by the region and country, and holds
    // none of the smaller areas.
    let held = areas();
    assert_eq!(
        body["features"][2]["properties"]["context"],
        json!({
            "place": {"mapbox_id": id("osm:relation:3"), "name": "Toronto"},
            "region": held["region"],
            "country": held["country"],
        })
    );
    assert_eq!(
        body["features"][2]["properties"]["place_formatted"],
        "Ontario, Canada"
    );
    assert_eq!(
        body["features"][2]["properties"]["full_address"],
        "Toronto, Ontario, Canada"
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
        assert_eq!(
            body["features"][0]["id"],
            address_vendor_id("osm:node:10"),
            "{query}"
        );
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
    let poi = encode_parts("osm:node:11", Some(Layer::Poi), "");
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
    assert_eq!(
        batch[0]["features"][0]["id"],
        address_vendor_id("osm:node:10")
    );
    assert_eq!(
        batch[1],
        json!({"type": "FeatureCollection", "features": [], "attribution": OSM})
    );
    assert_eq!(batch[2]["features"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        batch[2]["features"][0]["id"],
        address_vendor_id("osm:node:10")
    );
    assert_eq!(
        batch[3]["features"][0]["id"],
        address_vendor_id("osm:node:10")
    );
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
            "Longitude must be a number between -180 and 180",
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
            "BBox is not valid. Must be an array of format [minX, minY, maxX, maxY]",
        ),
        (
            format!("{base}/forward?q=a&limit=11"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "limit must be a number between 1 and 10",
        ),
        (
            format!("{base}/reverse?longitude=-79&latitude=43&types=address&limit=6"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "limit must be a number between 1 and 5",
        ),
        (
            format!("{base}/reverse?longitude=-79&latitude=43&country=canada"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "Stack \"canada\" is not a known stack.",
        ),
        (
            format!("{base}/forward?q=a&autocomplete=maybe"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "autocomplete must be true or false, not \"maybe\"",
        ),
        (
            format!("{base}/reverse?longitude=-79&latitude=43&limit=2"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "limit must be combined with a single type parameter when reverse geocoding",
        ),
        (
            format!("{base}/reverse?longitude=-79"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "Latitude must be a number between -90 and 90",
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
async fn forward_address_filters_include_mapped_house_number_ranges() {
    let pack = write_pack();
    let router = router(&pack);
    for route in [
        "/compat/mapbox/search/geocode/v6/forward?autocomplete=false",
        "/compat/mapbox/search/geocode/v6/forward?autocomplete=true",
        "/compat/mapbox/search/searchbox/v1/forward?auto_complete=false",
        "/compat/mapbox/search/searchbox/v1/forward?auto_complete=true",
    ] {
        let uri = format!("{route}&q=50%20Queen%20Street%2C%20Toronto&types=address&limit=1");
        let (status, body) = get(&router, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        let features = body["features"].as_array().expect("features");
        assert_eq!(features.len(), 1, "{uri}: {body}");
        assert_eq!(
            features[0]["properties"]["name"], "50 Queen Street",
            "{uri}"
        );
        assert_eq!(
            features[0]["properties"]["feature_type"], "address",
            "{uri}"
        );
    }
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
    let poi = encode_parts("osm:node:11", Some(Layer::Poi), "");
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
    // og's contact and the hours it reads, Monday 0800 to 1800 and so on.
    let open = |day: u8, from: &str, to: &str| json!({"open": {"day": day, "time": from}, "close": {"day": day, "time": to}});
    let metadata = json!({
        "phone": "+1 416-555-0111",
        "website": "https://queencafe.example/",
        "email": "hello@queencafe.example",
        "open_hours": {"periods": [
            open(1, "0800", "1800"),
            open(2, "0800", "1800"),
            open(3, "0800", "1800"),
            open(4, "0800", "1800"),
            open(5, "0800", "1800"),
            open(6, "0900", "1300"),
        ]},
    });
    assert_eq!(
        body,
        json!({
            "suggestions": [{
                "name": "Queen Cafe",
                "mapbox_id": poi,
                "feature_type": "poi",
                "address": "12 King Street",
                "full_address": "12 King Street, Unit A, Toronto, Ontario M5V 1A1, Canada",
                "place_formatted": "Toronto, Ontario M5V 1A1, Canada",
                "context": context,
                "language": "en",
                "maki": "cafe",
                "brand": ["Queen Coffee"],
                "poi_category": ["cafe", "coffee", "coffee shop", "food", "food and drink"],
                "poi_category_ids": ["cafe", "coffee", "coffee_shop", "food", "food_and_drink"],
                "metadata": metadata,
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
                    "full_address": "12 King Street, Unit A, Toronto, Ontario M5V 1A1, Canada",
                    "place_formatted": "Toronto, Ontario M5V 1A1, Canada",
                    "context": context,
                    "coordinates": {
                        "longitude": -79.005,
                        "latitude": 43.0,
                        "routable_points": [
                            {"name": "entrance", "longitude": -79.005, "latitude": 43.0001},
                        ],
                    },
                    "bbox": [-79.0052, 42.9999, -79.0048, 43.0001],
                    "maki": "cafe",
                    "brand": ["Queen Coffee"],
                    "poi_category": ["cafe", "coffee", "coffee shop", "food", "food and drink"],
                    "poi_category_ids": ["cafe", "coffee", "coffee_shop", "food", "food_and_drink"],
                    "metadata": metadata,
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
        (
            "q=10%20King%20Street",
            vec![address_vendor_id("osm:node:10")],
        ),
        ("q=Queen%20Cafe&types=address", vec![]),
        // og's /places.
        (
            "q=Queen%20Cafe&types=poi",
            vec![encode_parts("osm:node:11", Some(Layer::Poi), "")],
        ),
        // og's /autocomplete, which has both.
        (
            "q=Queen%20Caf&auto_complete=true&types=poi",
            vec![encode_parts("osm:node:11", Some(Layer::Poi), "")],
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
    let address = address_vendor_id("osm:node:10");
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

    // Without a type the point's areas follow the house, as many as asked.
    let count = |limit: &str| {
        let router = router.clone();
        let uri = format!(
            "/compat/mapbox/search/searchbox/v1/reverse?longitude=-79.0&latitude=43.0{limit}"
        );
        async move {
            let (status, body) = get(&router, &uri).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body["features"].as_array().map(Vec::len).expect("features")
        }
    };
    assert!(count("").await > 1);
    assert_eq!(count("&limit=1").await, 1);
    assert_eq!(count("&limit=2").await, 2);
}

#[tokio::test]
async fn search_box_category_answers_the_places_of_one_kind() {
    let pack = write_pack();
    let router = router(&pack);
    let base = "/compat/mapbox/search/searchbox/v1/category";
    let names = |body: &Value| {
        body["features"]
            .as_array()
            .expect("features")
            .iter()
            .map(|feature| feature["properties"]["name"].clone())
            .collect::<Vec<_>>()
    };

    let (status, body) = get(&router, &format!("{base}/coffee?proximity=-79,43")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["type"], "FeatureCollection");
    assert_eq!(body["attribution"], OSM);
    let properties = &body["features"][0]["properties"];
    assert_eq!(properties["name"], "Queen Cafe");
    assert_eq!(
        properties["mapbox_id"],
        encode_parts("osm:node:11", Some(Layer::Poi), "")
    );
    assert_eq!(properties["feature_type"], "poi");
    assert_eq!(properties["maki"], "cafe");
    assert_eq!(
        properties["poi_category"],
        json!(["cafe", "coffee", "coffee shop", "food", "food and drink"])
    );
    assert_eq!(
        properties["poi_category_ids"],
        json!(["cafe", "coffee", "coffee_shop", "food", "food_and_drink"])
    );
    assert_eq!(
        body["features"][0]["geometry"],
        json!({"type": "Point", "coordinates": [-79.005, 43.0]})
    );

    // A wider kind, an id of the catalog's own words, and no point to search around.
    for uri in [
        format!("{base}/food_and_drink?proximity=-79,43&radius=0.1&limit=25"),
        format!("{base}/caf%C3%A9?proximity=-79,43"),
        format!("{base}/coffee"),
        format!("{base}/coffee?proximity=ip&country=ca&bbox=-80,42,-78,44"),
    ] {
        let (status, body) = get(&router, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(names(&body), [json!("Queen Cafe")], "{uri}");
    }

    // Another kind, a point too far, another country, and the kind excluded.
    for uri in [
        format!("{base}/gas_station?proximity=-79,43"),
        format!("{base}/coffee?proximity=-70,43&radius=1"),
        format!("{base}/coffee?country=fr"),
        format!("{base}/coffee?bbox=0,0,1,1"),
        format!("{base}/coffee?proximity=-79,43&poi_category_exclusions=food_and_drink"),
    ] {
        let (status, body) = get(&router, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(names(&body), Vec::<Value>::new(), "{uri}");
    }
}

#[tokio::test]
async fn search_box_lists_the_categories_it_searches_by() {
    let pack = write_pack();
    let router = router(&pack);
    let (status, body) = get(
        &router,
        "/compat/mapbox/search/searchbox/v1/list/category?language=fr",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["listItems"].as_array().expect("listItems");
    let coffee = items
        .iter()
        .find(|item| item["canonical_id"] == "coffee")
        .expect("coffee");
    assert_eq!(coffee["name"], "Coffee");
    assert_eq!(coffee["icon"], "cafe");
    assert_eq!(coffee["version"], env!("CARGO_PKG_VERSION"));
    assert!(uuid::Uuid::parse_str(coffee["uuid"].as_str().expect("uuid")).is_ok());
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    // Every listed id answers a category search.
    for item in items {
        let id = item["canonical_id"].as_str().expect("id");
        let (status, _) = get(
            &router,
            &format!("/compat/mapbox/search/searchbox/v1/category/{id}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{id}");
    }
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
            "Proximity must be an array in the form [lon, lat]",
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
            format!("{base}/category/no_such_kind_of_place"),
            StatusCode::NOT_FOUND,
            "Not Found",
        ),
        (
            format!("{base}/category/coffee?limit=26"),
            StatusCode::BAD_REQUEST,
            "limit must be a whole number from 1 to 25",
        ),
        (
            format!("{base}/category/coffee?radius=1"),
            StatusCode::BAD_REQUEST,
            "radius needs proximity",
        ),
        (
            format!("{base}/category/coffee?proximity=-79,43&radius=11"),
            StatusCode::BAD_REQUEST,
            "radius must be a number from 0.00001 to 10",
        ),
        (
            format!("{base}/category/coffee?proximity=1"),
            StatusCode::BAD_REQUEST,
            "Proximity must be an array in the form [lon, lat]",
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

#[test]
fn the_place_line_keeps_distinct_layers_and_omits_missing_parts() {
    let mut context = Context {
        place: Some(Area::named(None, "New York")),
        region: Some(Area::named(None, "New York")),
        postcode: Some(Area::named(None, "10118")),
        country: Some(Area::named(None, "United States")),
        ..Context::default()
    };
    assert_eq!(
        place_line(Layer::Address, &context).as_deref(),
        Some("New York, New York 10118, United States")
    );
    context.region = None;
    assert_eq!(
        place_line(Layer::Address, &context).as_deref(),
        Some("New York, 10118, United States")
    );
    context.place = None;
    context.country = None;
    assert_eq!(place_line(Layer::Address, &context), None);
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
            "wikidata_id": "Q16",
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

/// What a filter asks is honoured or refused, never dropped with a 200.
#[tokio::test]
async fn filters_are_honoured_or_refused() {
    let pack = write_pack();
    let router = router(&pack);
    let geocoding = "/compat/mapbox/search/geocode/v6";
    let search_box = "/compat/mapbox/search/searchbox/v1";
    let names = |body: &Value, key: &str| {
        body[key]
            .as_array()
            .expect("list")
            .iter()
            .map(|item| item.get("properties").unwrap_or(item)["name"].clone())
            .collect::<Vec<_>>()
    };

    // One area type alone is that area at the point.
    let (status, body) = get(
        &router,
        &format!("{geocoding}/reverse?longitude=-79.0&latitude=43.0&types=region"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(names(&body, "features"), ["Ontario"]);

    // A point in another country answers nothing.
    for uri in [
        format!("{geocoding}/reverse?longitude=-79.0&latitude=43.0&country=fr"),
        format!("{search_box}/reverse?longitude=-79.0&latitude=43.0&country=fr"),
    ] {
        let (status, body) = get(&router, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["features"], json!([]), "{uri}");
    }

    // Typed text completes, as Mapbox's default `autocomplete` does.
    let (status, body) = get(&router, &format!("{geocoding}/forward?q=Toro")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(names(&body, "features").first(), Some(&json!("Toronto")));
    // A whole address stays the geocode, whose match_code says the town
    // it names is not the one it lies in.
    let (status, body) = get(
        &router,
        &format!("{geocoding}/forward?q=10%20King%20Street,%20Ottawa"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let code = &body["features"][0]["properties"]["match_code"];
    assert_eq!(code["address_number"], "matched", "{body}");
    assert_eq!(code["place"], "unmatched", "{body}");

    // A category keeps only places of it.
    let (status, body) = get(
        &router,
        &format!("{search_box}/suggest?q=Queen&session_token=t&poi_category=bank"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["suggestions"], json!([]));

    // What og cannot honour is refused.
    for (uri, expected, message) in [
        (
            format!("{search_box}/suggest?q=Queen&session_token=t&route=abc"),
            StatusCode::BAD_REQUEST,
            "route is not supported by this server",
        ),
        (
            format!("{geocoding}/forward?q=Queen&limit=11"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "limit must be a number between 1 and 10",
        ),
    ] {
        let (status, body) = get(&router, &uri).await;
        assert_eq!(status, expected, "{uri}");
        assert_eq!(body["message"], message, "{uri}");
    }
}
