//! `/compat/precisely` driven through og's router on a small Pack.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use geojson::{Geometry, GeometryValue};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::{
    compat::{decode_id, encode_parts},
    confidence::{FEATURES, Model},
    context::{AdminContextTuple, RecordContext},
    iso,
    limits::Limits,
    pack::Writer,
    record::{
        AddressComponents, AddressRecord, DerivedSourceProvenance, InterpolationAddressComponents,
        InterpolationRange, InterpolationRecord, Layer, LocationPrecision, ObjectType, PlaceLayer,
        PlaceRecord, PoiRecord, PostcodeRecord, Record, SourceProvenance, point_geometry,
    },
    runtime::{AppState, build_router},
};

/// A Pack and its job directory, removed when the test ends, failed or not.
struct Fixture {
    pack: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.pack);
        let _ = std::fs::remove_dir_all(self.pack.with_extension("jobs"));
    }
}

impl Fixture {
    fn new(name: &str) -> Self {
        let pack = std::env::temp_dir().join(format!(
            "open-geocode-precisely-{name}-{}",
            std::process::id()
        ));
        let fixture = Self { pack };
        let _ = std::fs::remove_dir_all(fixture.pack.with_extension("jobs"));
        write_pack(&fixture.pack);
        fixture
    }

    fn router(&self) -> Router {
        self.router_with(Limits::DEFAULT)
    }

    fn router_with(&self, limits: Limits) -> Router {
        build_router(self.state(limits), None, None)
    }

    fn state(&self, limits: Limits) -> AppState {
        AppState::open(&self.pack, &self.pack.with_extension("jobs"), limits).expect("state")
    }
}

/// Canada > Ontario > Toronto, with every area tier a Precisely address has
/// a slot for, and an address, a bakery and a numbered range in it.
/// The vendor id og issues for an address of the fixture: its og id, its
/// layer and its label, since the catalogue holds no address.
fn address_vendor_id(og_id: &str) -> String {
    let label = match og_id {
        "osm:node:10" => "100 King Street West, Unit 5, ON, M5X 1A9",
        _ => panic!("{og_id} is no address of the fixture"),
    };
    encode_parts(og_id, Some(Layer::Address), label)
}

fn write_pack(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
    let mut writer = Writer::create(dir).expect("writer");
    let mut place = |layer, name: &str, id| {
        writer
            .write(
                &Record::Place(
                    layer,
                    PlaceRecord {
                        alt_names: Vec::new(),
                        name: name.to_string(),
                        place_type: "admin_level:8".to_string(),
                        geometry: point_geometry(-79.38, 43.65),
                        source: SourceProvenance::osm(ObjectType::Relation, id),
                        prominence: Default::default(),
                        wikidata: None,
                    },
                ),
                None,
            )
            .expect("place")
    };
    let country = place(PlaceLayer::Country, "Canada", 1);
    let region = place(PlaceLayer::Region, "Ontario", 2);
    let district = place(PlaceLayer::District, "Toronto Division", 3);
    let locality = place(PlaceLayer::Locality, "Toronto", 4);
    let sublocality = place(PlaceLayer::SubLocality, "Old Toronto", 5);
    let neighbourhood = place(PlaceLayer::Neighbourhood, "Financial District", 6);
    writer.set_country_codes(
        [(
            country,
            crate::parse::lattice::CountryCode::parse("CA").expect("code"),
        )]
        .into_iter()
        .collect(),
    );
    writer.set_subdivision_codes([(region, "CA-ON".to_string())].into_iter().collect());
    let context = Some(RecordContext {
        admin_context: AdminContextTuple::from_tiers([
            (PlaceLayer::Country, country),
            (PlaceLayer::Region, region),
            (PlaceLayer::District, district),
            (PlaceLayer::Locality, locality),
            (PlaceLayer::SubLocality, sublocality),
            (PlaceLayer::Neighbourhood, neighbourhood),
        ]),
        flags: 0,
        zone: None,
    });
    let address = AddressComponents {
        areas: Vec::new(),
        number: "100".to_string(),
        street: Some("King Street West".to_string()),
        place: None,
        unit: Some("5".to_string()),
        locality: None,
        region: Some("ON".to_string()),
        postcode: Some("M5X 1A9".to_string()),
        country: None,
    };
    writer
        .write(
            &AddressRecord {
                access: Vec::new(),
                disputed: false,
                street_postcodes: Vec::new(),
                address: address.clone(),
                geometry: point_geometry(-79.3817, 43.6493),
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
                    number: "120".to_string(),
                    unit: None,
                    ..address
                }),
                geometry: point_geometry(-79.3830, 43.6490),
                location_precision: LocationPrecision::Centroid,
                source: SourceProvenance::osm(ObjectType::Way, 11),
                prominence: Default::default(),
                wikidata: None,
                brand: None,
                details: None,
                aliases: Vec::new(),
            }
            .into(),
            context.clone(),
        )
        .expect("poi");
    let interpolation_context = context.clone();
    writer
        .write(
            &InterpolationRecord {
                address: InterpolationAddressComponents {
                    street: Some("King Street West".to_string()),
                    place: None,
                    locality: Some("Toronto".to_string()),
                    region: None,
                    postcode: None,
                    country: None,
                },
                interpolation: InterpolationRange {
                    kind: "even".to_string(),
                    start: 200,
                    end: 298,
                    step: 2,
                },
                anchor_node_ids: [200, 298],
                geometry: Geometry::new(GeometryValue::LineString {
                    coordinates: vec![vec![-79.39, 43.648].into(), vec![-79.391, 43.6475].into()],
                }),
                representative_point: [-79.3905, 43.64775],
                source: SourceProvenance::osm(ObjectType::Way, 13),
            }
            .into(),
            interpolation_context,
        )
        .expect("interpolation");
    writer
        .write(
            &Record::Postcode(PostcodeRecord {
                postcode: "M5X 1A9".to_string(),
                country: Some(crate::parse::lattice::CountryCode::parse("CA").expect("code")),
                geometry: point_geometry(-79.3815, 43.6492),
                source: DerivedSourceProvenance::osm_address_records(1),
            }),
            context,
        )
        .expect("postcode");
    writer.finish().expect("finish");
    // Calibrated, so answers carry the confidence `score` is made from.
    let pack = crate::pack::Reader::open(dir).expect("pack");
    Model::new([0.1; FEATURES], Default::default())
        .save(&pack)
        .expect("confidence model");
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| {
            panic!(
                "not JSON ({}): {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).expect("UTF-8")
    }
}

async fn send(router: &Router, request: Request<Body>) -> Reply {
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    Reply {
        status,
        headers,
        body,
    }
}

async fn post(router: &Router, path: &str, body: &Value) -> Reply {
    let request = Request::post(format!("/compat/precisely{path}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, "Bearer not-checked")
        .body(Body::from(body.to_string()))
        .expect("request");
    send(router, request).await
}

async fn get(router: &Router, path: &str) -> Reply {
    send(router, Request::get(path).body(Body::empty()).expect("get")).await
}

fn errors(status: &str, message: &str) -> Value {
    json!({"errors": [{"status": status, "errorMessage": message}]})
}

/// og's own answer for the same lookup.
async fn native(router: &Router, path: &str) -> Value {
    let reply = get(router, path).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    reply.json()
}

/// The Precisely route a result came back through, which decides where its
/// distance goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Geocode,
    Reverse,
    Lookup,
    Autocomplete,
}

/// The og values a Precisely result carries. Each one og gave is named in
/// `seen`, so a test can tell that every slot was reached.
fn assert_carries(
    route: Route,
    feature: &Value,
    result: &Value,
    seen: &mut BTreeSet<&'static str>,
) {
    let properties = &feature["properties"];
    let address = &properties["address"];
    let context = &properties["context"];
    let carried = &result["address"];
    let mut slot = |name: &'static str, og: &Value| {
        if !og.is_null() {
            seen.insert(name);
        }
    };
    assert_eq!(
        decode_id(
            result["customFields"]["PB_KEY"]
                .as_str()
                .unwrap_or_else(|| panic!("no PB_KEY on {result} for {feature}"))
        )
        .map(|named| named.id().to_string())
        .as_deref(),
        properties["id"].as_str()
    );
    assert_eq!(carried["formattedAddress"], properties["label"]);
    assert_eq!(
        result["location"]["feature"]["geometry"]["coordinates"],
        feature["geometry"]["coordinates"]
    );
    assert_eq!(
        result["location"]["explanation"]["additionalMetadata"]["SOURCE"],
        properties["source"]["dataset"]
    );
    // With no address asked there is nothing to score, and Precisely
    // writes 0.
    match (route, properties["confidence"].as_f64()) {
        (Route::Reverse | Route::Lookup, _) => assert_eq!(result["score"], 0, "{result}"),
        (_, Some(confidence)) => {
            assert_eq!(result["score"], json!((confidence * 100.0).round() as u8));
        }
        (_, None) => assert_eq!(result.get("score"), None),
    }

    slot("address_lines", &properties["address_lines"]);
    assert_eq!(
        result.get("addressLines"),
        properties.get("address_lines"),
        "{feature} as {result}"
    );

    // Each part og judged is described, `EXACT` only for its `matched`.
    let judged = &properties["match"];
    slot("match", judged);
    let description = &result["explanation"]["addressMatch"]["description"];
    // Found by a point or a key, each part is judged by how it was found:
    // the street, city and state the record holds `EXACT`, and by a key its
    // postal parts too.
    if matches!(route, Route::Reverse | Route::Lookup) {
        let address = &result["address"];
        let held = |held: bool| if held { "EXACT" } else { "NONE" };
        let postal = route == Route::Lookup && !address["postalCode"].is_null();
        for entry in description.as_array().expect("a description") {
            let label = entry["label"].as_str().unwrap_or_default();
            let expected = match label {
                "street" | "streetType" | "streetDirectional" => {
                    held(!properties["address"]["street"].is_null())
                }
                "city" => held(!address["city"]["longName"].is_null()),
                "admin1" => held(!address["admin1"].is_null()),
                "postalCode" | "postalCodeExt" | "unitMatch" => held(postal),
                _ => "NONE",
            };
            assert_eq!(entry["matchType"], expected, "{label} in {result}");
        }
    } else {
        assert_judged(route, feature, result, judged, description);
    }

    let distance = &properties["distance_m"];
    let (value, unit) = match route {
        Route::Geocode | Route::Lookup => (Value::Null, Value::Null),
        Route::Reverse => {
            slot("reverse distance", distance);
            let fields = &result["customFields"];
            let value = fields["REVERSE_GEOCODE_DISTANCE"]
                .as_str()
                .map(|value| json!(value.parse::<f64>().expect("a number")))
                .unwrap_or(Value::Null);
            (value, fields["REVERSE_GEOCODE_DISTANCE_UNIT"].clone())
        }
        Route::Autocomplete => {
            slot("prediction distance", distance);
            (
                result["distance"]["value"].clone(),
                result["distance"]["distanceUnit"].clone(),
            )
        }
    };
    // An autocomplete asked with no origin measures from nowhere, and
    // Precisely writes 0.
    let distance = match (route, distance) {
        (Route::Autocomplete, Value::Null) => &json!(0.0),
        _ => distance,
    };
    assert_eq!(&value, distance, "{feature} as {result}");
    let metres = match route {
        Route::Reverse => json!("METERS"),
        _ => json!("METER"),
    };
    assert_eq!(
        unit,
        if distance.is_null() {
            Value::Null
        } else {
            metres
        },
        "{feature} as {result}"
    );

    // An area result is its own region or country, which its context does
    // not name.
    let own = |layer: &str| {
        if properties["layer"] == layer {
            json!({"name": properties["name"], "code": properties["code"]})
        } else {
            Value::Null
        }
    };
    let or = |area: &Value, own: Value| if area.is_null() { own } else { area.clone() };
    let region = or(&context["region"], own("region"));
    let country = or(&context["country"], own("country"));
    if properties["layer"] == "region" || properties["layer"] == "country" {
        slot("area code", &properties["code"]);
    }
    let pairs = [
        (&address["housenumber"], &carried["addressNumber"]),
        (&address["street"], &carried["street"]),
        (&address["unit"], &carried["unit"]),
        (&address["postcode"], &carried["postalCode"]),
        (&region["name"], &carried["admin1"]["longName"]),
        (&context["locality"]["name"], &carried["city"]["longName"]),
        (&context["district"]["name"], &carried["admin2"]["longName"]),
        (
            &context["sublocality"]["name"],
            &carried["suburb"]["longName"],
        ),
        (
            &context["neighbourhood"]["name"],
            &carried["neighborhood"]["longName"],
        ),
        (&country["name"], &carried["country"]["name"]),
    ];
    for (og, precisely) in pairs {
        assert_eq!(og, precisely, "{feature} as {result}");
    }
    // The region's own code is its short name; the region the record states
    // stands in when og knows no code.
    let short = region["code"]
        .as_str()
        .and_then(|code| code.split_once('-'))
        .map(|(_, own)| json!(own))
        .unwrap_or_else(|| address["region"].clone());
    assert_eq!(
        carried["admin1"]["shortName"], short,
        "{feature} as {result}"
    );
    // The country's code, which a region's ISO 3166-2 code starts with when
    // og names no country.
    let code = country["code"]
        .as_str()
        .or_else(|| {
            region["code"]
                .as_str()?
                .split_once('-')
                .map(|(code, _)| code)
        })
        .expect("country code");
    assert_eq!(carried["country"]["isoAlpha2Code"], code, "{result}");
    assert_eq!(
        carried["country"]["isoAlpha3Code"].as_str(),
        iso::alpha3(code)
    );
    assert_eq!(
        carried["country"]["isoNumericCode"].as_str(),
        iso::numeric(code)
    );
}

/// The address result, `exact` the parts og matched, every other part
/// Precisely lists `NONE`, in Precisely's order.
fn address_result(score: &Value, exact: &[&str]) -> Value {
    let description = [
        "placeName",
        "addressNumber",
        "admin1",
        "admin2",
        "city",
        "neighborhood",
        "suburb",
        "street",
        "postalCode",
        "streetType",
        "postalCodeExt",
        "streetDirectional",
        "unitMatch",
    ]
    .iter()
    .map(|label| {
        // The street is asked whole, so its type and direction agree too.
        let street_part =
            matches!(*label, "streetType" | "streetDirectional") && exact.contains(&"street");
        let kind = if exact.contains(label) || street_part {
            "EXACT"
        } else {
            "NONE"
        };
        json!({"label": label, "matchType": kind})
    })
    .collect::<Vec<_>>();
    json!({
        "score": score,
        "address": {
            "formattedAddress": "100 King Street West, Unit 5, ON, M5X 1A9",
            "formattedStreetAddress": "100 King Street West, Unit 5",
            "formattedLocationAddress": "ON, M5X 1A9",
            "addressNumber": "100",
            "street": "King Street West",
            "unit": "5",
            "neighborhood": {"longName": "Financial District"},
            "suburb": {"longName": "Old Toronto"},
            "city": {"longName": "Toronto"},
            "admin2": {"longName": "Toronto Division"},
            "admin1": {"longName": "Ontario", "shortName": "ON"},
            "postalCode": "M5X 1A9",
            "country": {
                "name": "Canada",
                "isoAlpha2Code": "CA",
                "isoAlpha3Code": "CAN",
                "isoNumericCode": "124",
            },
        },
        "location": {
            "explanation": {
                "type": "ADDRESS_POINT",
                "additionalMetadata": {"SOURCE": "osm"},
            },
            "feature": {
                "type": "Feature",
                "properties": {"crsName": "epsg:4326"},
                "geometry": {"type": "Point", "coordinates": [-79.3817, 43.6493]},
            },
        },
        "addressLines": ["Unit 5", "100 King Street West", "Toronto, ON M5X 1A9", "Canada"],
        "explanation": {"addressMatch": {"type": "ADDRESS", "description": description}},
        "customFields": restated(json!({
            "PB_KEY": address_vendor_id("osm:node:10"),
            "MATCH_TYPE": "ADDRESS",
            "GEOHASH": "dpz83dh58762",
        })),
    })
}

/// A result to a request whose `clientLocale` is `en_US`, which restates
/// the language.
fn in_english(mut result: Value) -> Value {
    result["customFields"]["LANGUAGE"] = json!("en");
    result
}

/// The custom fields that restate the King Street West address's parts,
/// with `fields` besides. Outside the US og writes the street's type as
/// the address writes it, and its one name for a town as each of the
/// town's names.
fn restated(mut fields: Value) -> Value {
    let parts = json!({
        "ADDRLINE_SHORT": "100 King Street West, Unit 5",
        "LASTLINE_SHORT": "ON, M5X 1A9",
        "CITY_SHORT": "Toronto",
        "NAME_CITY": "Toronto",
        "PREF_CITY": "Toronto",
        "NAME": "King",
        "NAME_SHORT": "King",
        "TYPE_SHORT": "Street",
        "THOROUGHFARE_TYPE": "Street",
        "POST_THOROUGHFARE_TYPE": "Street",
        "INTERSECTION": "F",
    });
    for (key, value) in parts.as_object().expect("an object") {
        fields[key] = value.clone();
    }
    fields
}

/// The street's type and direction are judged apart from its name: each
/// agrees, neither stating one included, or not.
#[tokio::test]
async fn geocode_judges_a_streets_type_and_direction_apart() {
    let fixture = Fixture::new("street-parts");
    let router = fixture.router();
    let judged = |line: &'static str| {
        let router = router.clone();
        async move {
            let reply = post(
                &router,
                "/v1/geocode",
                &json!({"addresses": [{"addressLines": [line], "country": "CAN"}]}),
            )
            .await;
            let description = reply.json()["responses"][0]["results"][0]["explanation"]["addressMatch"]["description"].clone();
            let of = |label: &str| {
                description
                    .as_array()
                    .and_then(|parts| parts.iter().find(|part| part["label"] == label))
                    .map(|part| part["matchType"].clone())
            };
            (of("street"), of("streetType"), of("streetDirectional"))
        }
    };
    assert_eq!(
        judged("100 King Street West, Toronto").await,
        (
            Some(json!("EXACT")),
            Some(json!("EXACT")),
            Some(json!("EXACT"))
        )
    );
    // `St` is the type written short, and no direction is asked.
    assert_eq!(judged("100 King St, Toronto").await.1, Some(json!("EXACT")));
    assert_eq!(judged("100 King St, Toronto").await.2, Some(json!("NONE")));
}

#[tokio::test]
async fn geocode_answers_each_address_in_order() {
    let fixture = Fixture::new("geocode");
    let router = fixture.router();
    let og = native(&router, "/geocode?q=100%20King%20Street%20West&limit=1").await;
    let score = json!(
        (og["features"][0]["properties"]["confidence"]
            .as_f64()
            .expect("calibrated")
            * 100.0)
            .round() as u8
    );

    // Parts with a street go to og's address fields, which score alike.
    let og = native(
        &router,
        "/geocode?address=100%20King%20Street%20West&locality=Toronto&postcode=M5X%201A9\
         &country=CA&limit=1",
    )
    .await;
    let structured_score = json!(
        (og["features"][0]["properties"]["confidence"]
            .as_f64()
            .expect("calibrated")
            * 100.0)
            .round() as u8
    );

    let reply = post(
        &router,
        "/v1/geocode",
        &json!({
            "preferences": {"maxResults": 1, "clientLocale": "en_US", "returnAllInfo": true},
            "addresses": [
                {"addressId": "1", "addressLines": ["100 King Street West"], "country": "CAN",
                 "city": "", "postalCode": ""},
                {"addressId": "2", "addressLines": ["99 Nowhere Lane"], "country": "124"},
                {"addressId": "3", "addressNumber": "100", "street": "King Street West",
                 "city": "Toronto", "postalCode": "M5X 1A9", "country": "CA"},
            ],
        }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json(),
        json!({"responses": [
            {"addressId": "1", "status": "OK", "results": [in_english(address_result(
                &score,
                &["addressNumber", "street"],
            ))]},
            {"addressId": "2", "status": "ZERO_RESULTS", "results": []},
            {"addressId": "3", "status": "OK", "results": [in_english(address_result(
                &structured_score,
                &["addressNumber", "street", "city", "postalCode"],
            ))]},
        ]})
    );
}

#[tokio::test]
async fn every_og_value_lands_in_its_precisely_slot() {
    let fixture = Fixture::new("fidelity");
    let router = fixture.router();
    let mut seen = BTreeSet::new();

    let og = native(
        &router,
        "/geocode?q=100%20King%20Street%20West%2C%20Toronto&limit=5",
    )
    .await;
    let features = og["features"].as_array().expect("features");
    let reply = post(
        &router,
        "/v1/geocode",
        &json!({"preferences": {"maxResults": 5},
                "addresses": [{"addressLines": ["100 King Street West, Toronto"], "country": "CA"}]}),
    )
    .await;
    let results = reply.json()["responses"][0]["results"].clone();
    let results = results.as_array().expect("results");
    assert_eq!(results.len(), features.len(), "{og}");
    for (feature, result) in features.iter().zip(results) {
        assert_carries(Route::Geocode, feature, result, &mut seen);
    }

    let og = native(&router, "/geocode?q=King%20Street%20West&limit=5").await;
    let features = og["features"].as_array().expect("features");
    assert_eq!(features.len(), 2, "{og}");
    assert_eq!(features[0]["properties"]["layer"], "interpolation", "{og}");
    let reply = post(
        &router,
        "/v1/geocode",
        &json!({"preferences": {"maxResults": 5},
                "addresses": [{"addressLines": ["King Street West"], "country": "CA"}]}),
    )
    .await;
    let results = reply.json()["responses"][0]["results"].clone();
    assert_eq!(results.as_array().map(Vec::len), Some(2));
    for (feature, result) in features.iter().zip(results.as_array().expect("results")) {
        assert_carries(Route::Geocode, feature, result, &mut seen);
    }
    // A range with no number asked is placed on its street, not at a house.
    assert_eq!(
        [
            &results[0]["location"]["explanation"]["type"],
            &results[0]["explanation"]["addressMatch"]["type"],
            &results[1]["location"]["explanation"]["type"],
        ],
        ["STREET_CENTER", "STREET", "ADDRESS_POINT"]
    );

    // The point's areas come back too, each its own region or country.
    let og = native(&router, "/reverse?lon=-79.3815&lat=43.6495").await;
    let features = og["features"].as_array().expect("features");
    let reply = post(
        &router,
        "/v1/reverse-geocode",
        &json!({"preferences": {"maxResults": 10,
                                "distance": {"value": 30, "distanceUnit": "METER"}},
                "locations": [{"longitude": -79.3815, "latitude": 43.6495, "country": "CAN"}]}),
    )
    .await;
    let results = reply.json()["responses"][0]["results"].clone();
    let results = results.as_array().expect("results");
    // The house is a few metres off the asked point, so a distance lost or
    // zeroed shows.
    assert!(
        features[0]["properties"]["distance_m"].as_f64() > Some(1.0),
        "{og}"
    );
    // The fixture's areas below the region name no country, so the asked
    // country keeps the house, the region and the country.
    assert_eq!(results.len(), 3, "{og}");
    for result in results {
        let key = result["customFields"]["PB_KEY"].as_str().expect("PB_KEY");
        let id = decode_id(key).expect("og's key").id().to_string();
        let feature = features
            .iter()
            .find(|feature| feature["properties"]["id"] == id.as_str())
            .unwrap_or_else(|| panic!("{id} is not in {og}"));
        assert_carries(Route::Reverse, feature, result, &mut seen);
    }

    for (native_uri, preferences) in [
        ("/autocomplete?q=Queen%20Bak&limit=1", json!({})),
        (
            "/autocomplete?q=Queen%20Bak&limit=1&focus=-79.3815,43.6495",
            json!({"originXY": [-79.3815, 43.6495]}),
        ),
    ] {
        let og = native(&router, native_uri).await;
        let reply = post(
            &router,
            "/v1/autocomplete",
            &json!({"preferences": preferences,
                    "address": {"addressLines": ["Queen Bak"], "country": "CAN"}}),
        )
        .await;
        let prediction = &reply.json()["response"]["predictions"][0];
        assert_carries(
            Route::Autocomplete,
            &og["features"][0],
            prediction,
            &mut seen,
        );
        assert_eq!(
            prediction["prediction"],
            og["features"][0]["properties"]["label"]
        );
    }

    // Every slot was reached, so none can go empty unseen.
    assert_eq!(
        seen.into_iter().collect::<Vec<_>>(),
        [
            "address_lines",
            "area code",
            "match",
            "prediction distance",
            "reverse distance"
        ]
    );
}

#[tokio::test]
async fn a_poi_says_what_it_is_and_where() {
    let fixture = Fixture::new("poi");
    let router = fixture.router();
    let reply = post(
        &router,
        "/v1/autocomplete",
        &json!({"preferences": {"maxResults": 1},
                "address": {"addressLines": ["Queen Bakery"], "country": "CAN"}}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    // The typed name marks the place's name whole, and no other part.
    let typed = [
        "placeName",
        "addressNumber",
        "admin1",
        "admin2",
        "city",
        "neighborhood",
        "suburb",
        "street",
        "postalCode",
        "streetType",
        "postalCodeExt",
        "streetDirectional",
    ]
    .map(|label| match label {
        "placeName" => json!({"label": label, "matchType": "EXACT",
                              "matchedSubstring": "QUEEN BAKERY"}),
        _ => json!({"label": label, "matchType": "NONE"}),
    });
    // og's autocomplete is not calibrated, so it carries no score.
    assert_eq!(
        reply.json(),
        json!({"response": {"status": "OK", "predictions": [{
            "prediction": "Queen Bakery, 120 King Street West, Toronto, ON, M5X 1A9",
            "address": {
                "formattedAddress": "Queen Bakery, 120 King Street West, Toronto, ON, M5X 1A9",
                "addressNumber": "120",
                "street": "King Street West",
                "placeName": "Queen Bakery",
                "neighborhood": {"longName": "Financial District"},
                "suburb": {"longName": "Old Toronto"},
                "city": {"longName": "Toronto"},
                "admin2": {"longName": "Toronto Division"},
                "admin1": {"longName": "Ontario", "shortName": "ON"},
                "postalCode": "M5X 1A9",
                "country": {"name": "Canada", "isoAlpha2Code": "CA", "isoAlpha3Code": "CAN",
                            "isoNumericCode": "124"},
            },
            "location": {
                "explanation": {"type": "POI", "designation": "CENTROID",
                                "additionalMetadata": {"SOURCE": "osm"}},
                "feature": {"type": "Feature", "properties": {"crsName": "epsg:4326"},
                            "geometry": {"type": "Point", "coordinates": [-79.383, 43.649]}},
            },
            "addressLines": ["120 King Street West", "Toronto, ON M5X 1A9", "Canada"],
            "explanation": {"addressMatch": {"type": "POI", "description": typed}},
            // A place has no number of its own to start a range, and with
            // no `originXY` the distance is negative zero.
            "customFields": {
                "PB_KEY": encode_parts("osm:way:11", Some(Layer::Poi), ""),
                "FORMATTED_STRING": "Queen Bakery, 120 King Street West, Toronto, ON, M5X 1A9",
                "MATCHED_FROM_ADDRESSNUMBER": "120",
                "MATCHED_FROM_STREETNAME": "King",
                "DISTANCE": "-0.0",
                "DISTANCE_UNIT": "METERS",
            },
            "distance": {"value": 0.0, "distanceUnit": "METER"},
        }]}})
    );

    let none = post(
        &router,
        "/v1/autocomplete",
        &json!({"address": {"addressLines": ["Zzyzx"], "country": "CAN"}}),
    )
    .await;
    assert_eq!(none.status, StatusCode::OK);
    assert_eq!(
        none.json(),
        json!({"response": {"status": "ZERO_RESULTS", "predictions": []}})
    );
}

#[tokio::test]
async fn a_failed_item_makes_the_answer_partial() {
    let fixture = Fixture::new("partial");
    let router = fixture.router();
    let reply = post(
        &router,
        "/v1/geocode",
        &json!({"addresses": [
            {"addressId": "1", "addressLines": ["100 King Street West"], "country": "ABC"},
            {"addressId": "2", "addressLines": ["99 Nowhere Lane"], "country": "CAN"},
            {"addressId": "3", "addressLines": [""], "country": "CAN"},
        ]}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        reply.json(),
        json!({"responses": [
            {"addressId": "1", "status": "INVALID_REQUEST",
             "errorMessage": "Not able to identify ABC country (DIS-LI-1016)"},
            {"addressId": "2", "status": "ZERO_RESULTS", "results": []},
            {"addressId": "3", "status": "INVALID_REQUEST",
             "errorMessage": "Request Body is not valid (DIS-LI-1018)"},
        ]})
    );
}

#[tokio::test]
async fn a_batch_holds_one_to_twenty_five_items() {
    let fixture = Fixture::new("cap");
    let router = fixture.router();
    let item = json!({"addressLines": ["99 Nowhere Lane"], "country": "CA"});

    let full = post(
        &router,
        "/v1/geocode",
        &json!({"addresses": vec![item.clone(); 25]}),
    )
    .await;
    assert_eq!(full.status, StatusCode::OK);
    assert_eq!(full.json()["responses"].as_array().map(Vec::len), Some(25));

    for path in ["/v1/geocode", "/v1/lookup", "/v1/reverse-geocode"] {
        let key = match path {
            "/v1/geocode" => "addresses",
            "/v1/lookup" => "keys",
            _ => "locations",
        };
        let over = post(&router, path, &json!({ key: vec![item.clone(); 26] })).await;
        assert_eq!(over.status, StatusCode::PAYLOAD_TOO_LARGE, "{path}");
        assert_eq!(
            over.json(),
            errors(
                "BATCH_SIZE_EXCEEDED",
                "Batch size must not exceed 25 (DIS-LI-1014)"
            )
        );
        let empty = post(&router, path, &json!({ key: [] })).await;
        assert_eq!(empty.status, StatusCode::BAD_REQUEST, "{path}");
        assert_eq!(
            empty.json(),
            errors("INVALID_REQUEST", "Request Body is not valid (DIS-LI-1018)")
        );
    }
}

#[tokio::test]
async fn a_bad_request_is_refused_whole() {
    let fixture = Fixture::new("refused");
    let router = fixture.router();
    let addresses = json!([{"addressLines": ["100 King Street West"], "country": "CA"}]);
    let invalid_max = errors(
        "INVALID_REQUEST",
        "Invalid value for maxResults (DIS-LI-1036)",
    );
    for (body, expected) in [
        (
            json!({"preferences": {"maxResults": 0}, "addresses": addresses}),
            invalid_max.clone(),
        ),
        // Past og's own limit, which og refuses for every item alike.
        (
            json!({"preferences": {"maxResults": 51}, "addresses": addresses}),
            invalid_max.clone(),
        ),
        (
            json!({"addresses": "100 King Street West"}),
            errors("INVALID_REQUEST", "Request Body is not valid (DIS-LI-1018)"),
        ),
    ] {
        let reply = post(&router, "/v1/geocode", &body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(reply.json(), expected, "{body}");
    }
    let at_limit = post(
        &router,
        "/v1/geocode",
        &json!({"preferences": {"maxResults": 50}, "addresses": addresses}),
    )
    .await;
    assert_eq!(at_limit.status, StatusCode::OK);

    let blank = send(
        &router,
        Request::post("/compat/precisely/v1/geocode")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(blank.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        blank.json(),
        errors(
            "INVALID_REQUEST",
            "Request body is not specified in the request (DIS-LI-1019)"
        )
    );

    let autocomplete = post(
        &router,
        "/v1/autocomplete",
        &json!({"preferences": {"maxResults": 21}, "address": {"addressLines": ["King"]}}),
    )
    .await;
    assert_eq!(autocomplete.status, StatusCode::BAD_REQUEST);
    assert_eq!(autocomplete.json(), invalid_max);
}

#[tokio::test]
async fn reverse_geocode_answers_each_point() {
    let fixture = Fixture::new("reverse");
    let router = fixture.router();
    let reply = post(
        &router,
        "/v1/reverse-geocode",
        &json!({"locations": [
            {"addressId": "a", "longitude": -79.3817, "latitude": 43.6493, "country": "CAN"},
            {"addressId": "b", "latitude": 43.6493, "country": "CAN"},
            {"addressId": "c", "longitude": -79.3817, "latitude": 91.0, "country": "CAN"},
            {"addressId": "d", "longitude": -79.3817, "latitude": 43.6493, "country": "USA"},
        ]}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    // A point judges no stated part, so the result scores 0, as Precisely
    // writes for no address asked. The street segment the point lies on
    // fixes the street, city and state, and not the number or postcode.
    let mut expected = address_result(&json!(0), &[]);
    let exact = [
        "admin1",
        "city",
        "street",
        "streetType",
        "streetDirectional",
    ];
    expected["explanation"]["addressMatch"]["description"] = json!(
        [
            "placeName", "addressNumber", "admin1", "admin2", "city", "neighborhood", "suburb",
            "street", "postalCode", "streetType", "postalCodeExt", "streetDirectional", "unitMatch",
        ]
        .iter()
        .map(|label| json!({"label": label, "matchType": if exact.contains(label) { "EXACT" } else { "NONE" }}))
        .collect::<Vec<_>>()
    );
    // No address was asked, and the point is the one asked, so the answer
    // names no kind of match and restates no geohash.
    let fields = expected["customFields"].as_object_mut().expect("an object");
    fields.remove("MATCH_TYPE");
    fields.remove("GEOHASH");
    // The house stands on the asked point.
    expected["customFields"]["REVERSE_GEOCODE_DISTANCE"] = json!("0");
    expected["customFields"]["REVERSE_GEOCODE_DISTANCE_UNIT"] = json!("METERS");
    assert_eq!(
        reply.json(),
        json!({"responses": [
            {"addressId": "a", "status": "OK", "results": [expected]},
            {"addressId": "b", "status": "INVALID_REQUEST",
             "errorMessage": "Request Body is not valid (DIS-LI-1018)"},
            {"addressId": "c", "status": "INVALID_REQUEST",
             "errorMessage": "Request Body is not valid (DIS-LI-1018)"},
            // A point in Canada has no answer in the United States.
            {"addressId": "d", "status": "ZERO_RESULTS", "results": []},
        ]})
    );
    // Past the search radius is no house or street. An area holds the
    // point, so it is within any radius.
    let near = post(
        &router,
        "/v1/reverse-geocode",
        &json!({"preferences": {"distance": {"value": 0.001, "distanceUnit": "METER"}},
                "locations": [{"longitude": -79.3816, "latitude": 43.6494, "country": "CAN"}]}),
    )
    .await;
    let near = near.json();
    let kinds = near["responses"][0]["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|result| result["explanation"]["addressMatch"]["type"].clone())
        .collect::<Vec<_>>();
    assert_eq!(kinds, [json!("ADMIN")], "{near}");

    // The house 55 m off is found within the documented 150 m, past og's
    // own default reach, and not within Precisely's default 5 m.
    let far = |preferences: Value| {
        let router = router.clone();
        async move {
            post(
                &router,
                "/v1/reverse-geocode",
                &json!({"preferences": preferences,
                        "locations": [{"longitude": -79.3817, "latitude": 43.6498,
                                       "country": "CAN"}]}),
            )
            .await
            .json()["responses"][0]["results"][0]["explanation"]["addressMatch"]["type"]
                .clone()
        }
    };
    assert_eq!(
        far(json!({"distance": {"value": 150, "distanceUnit": "METER"}})).await,
        "ADDRESS"
    );
    assert_eq!(far(json!({})).await, "ADMIN");
}

#[tokio::test]
async fn lookup_finds_a_record_by_the_pb_key_og_issued() {
    let fixture = Fixture::new("lookup");
    let router = fixture.router();
    let reply = post(
        &router,
        "/v1/lookup",
        &json!({"keys": [{"key": encode_parts("osm:way:11", Some(Layer::Poi), ""), "type": "PB_KEY", "country": "CAN"}]}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let og = native(&router, "/places?ids=osm:way:11").await;
    let body = reply.json();
    assert_eq!(body["responses"][0]["status"], "OK");
    assert_eq!(body["responses"][0].get("addressId"), None);
    assert_carries(
        Route::Lookup,
        &og["features"][0],
        &body["responses"][0]["results"][0],
        &mut BTreeSet::new(),
    );

    // The key of an address, which the catalogue does not hold, finds it.
    let reply = post(
        &router,
        "/v1/lookup",
        &json!({"keys": [{"key": address_vendor_id("osm:node:10"), "type": "PB_KEY", "country": "CAN"}]}),
    )
    .await;
    let found = &reply.json()["responses"][0];
    assert_eq!(found["status"], "OK");
    assert_eq!(
        found["results"][0]["address"]["formattedAddress"],
        "100 King Street West, Unit 5, ON, M5X 1A9"
    );
    assert_eq!(
        found["results"][0]["customFields"]["PB_KEY"],
        address_vendor_id("osm:node:10")
    );

    let reply = post(
        &router,
        "/v1/lookup",
        &json!({"keys": [
            {"key": encode_parts("osm:node:999", Some(Layer::Poi), ""), "type": "PB_KEY", "country": "CAN"},
            {"key": "P0000GL41OME", "type": "PB_KEY", "country": "USA"},
            {"key": "10023122", "type": "UPRN", "country": "GBR"},
            {"key": "10023122", "type": "POSTCODE", "country": "GBR"},
            {"key": " ", "type": "PB_KEY", "country": "CAN"},
        ]}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        reply.json(),
        json!({"responses": [
            {"status": "ZERO_RESULTS", "results": []},
            {"status": "ZERO_RESULTS", "results": []},
            {"status": "INVALID_REQUEST",
             "errorMessage": "Key type UPRN is not supported by this server"},
            {"status": "INVALID_REQUEST",
             "errorMessage": "Key type should be either PB_KEY, GNAF_PID, EIR_CODE, UPRN or UDPRN only (DIS-LI-1017)"},
            {"status": "INVALID_REQUEST",
             "errorMessage": "Lookup key cannot be null or empty (DIS-LI-1027)"},
        ]})
    );
}

#[tokio::test]
async fn lookup_keeps_the_country_of_each_key() {
    let fixture = Fixture::new("lookup-country");
    let router = fixture.router();
    let key = address_vendor_id("osm:node:10");
    let legacy = key.strip_prefix("og_").expect("prefix");
    let reply = post(
        &router,
        "/v1/lookup",
        &json!({"keys": [
            {"key": key, "type": "PB_KEY", "country": "CAN"},
            {"key": legacy, "type": "PB_KEY", "country": "ca"},
            {"key": key, "type": "PB_KEY", "country": "Canada"},
            {"key": key, "type": "PB_KEY", "country": "USA"},
            {"key": key, "type": "PB_KEY", "country": "ZZ"},
            {"key": key, "type": "PB_KEY"},
        ]}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    let body = reply.json();
    assert_eq!(
        body["responses"][0]["results"],
        body["responses"][1]["results"]
    );
    assert_eq!(
        body["responses"][0]["results"],
        body["responses"][2]["results"]
    );
    assert_eq!(body["responses"][0]["status"], "OK", "{body}");
    assert_eq!(
        body["responses"][3],
        json!({"status": "ZERO_RESULTS", "results": []})
    );
    for index in [4, 5] {
        assert_eq!(
            body["responses"][index]["status"], "INVALID_REQUEST",
            "{body}"
        );
        assert!(body["responses"][index].get("results").is_none(), "{body}");
    }
}

#[tokio::test]
async fn an_unknown_path_or_method_is_not_found() {
    let fixture = Fixture::new("missing");
    let router = fixture.router();
    let not_found = errors("NOT_FOUND", "No such resource on this server");
    for (method, path) in [
        ("POST", "/compat/precisely/v1/nowhere"),
        ("GET", "/compat/precisely/v1/geocode"),
        ("DELETE", "/compat/precisely/v2/bulk-geocode"),
        ("GET", "/compat/precisely/v1/bulk/jobs"),
        ("POST", "/compat/precisely/auth/v2/token"),
    ] {
        let reply = send(
            &router,
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {path}");
        assert_eq!(reply.json(), not_found, "{method} {path}");
    }
}

/// Upload a CSV the way Precisely's flow does, returning its file id.
async fn upload(router: &Router, csv: &str) -> String {
    let reply = post(router, "/v2/files/upload-url", &json!({"fileName": "test"})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let body = reply.json();
    let file_id = body["fileId"].as_str().expect("fileId").to_string();
    let url = body["url"].as_str().expect("url");
    let path = url
        .strip_prefix("http://localhost")
        .expect("a URL back at og");
    let put = send(
        router,
        Request::put(path)
            .header(header::CONTENT_TYPE, "text/csv")
            .body(Body::from(csv.to_string()))
            .expect("put"),
    )
    .await;
    assert_eq!(put.status, StatusCode::OK, "{}", put.text());
    file_id
}

async fn finished(router: &Router, job_id: &str) -> Value {
    for _ in 0..500 {
        let status = get(router, &format!("/compat/precisely/v2/bulk/jobs/{job_id}")).await;
        assert_eq!(status.status, StatusCode::OK, "{}", status.text());
        let status = status.json();
        if status["status"] != "RUNNING" {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("job {job_id} never finished");
}

#[tokio::test]
async fn an_upload_url_starts_at_the_public_url_the_operator_gives() {
    let fixture = Fixture::new("public-url");
    let state = fixture
        .state(Limits::DEFAULT)
        .with_public_url(Some("https://geo.example.com/og/".to_string()));
    let router = build_router(state, None, None);
    let reply = send(
        &router,
        Request::post("/compat/precisely/v2/files/upload-url")
            .header("x-forwarded-host", "evil.example")
            .header(header::HOST, "10.0.0.7:8080")
            .body(Body::from(r#"{"fileName": "test"}"#))
            .expect("request"),
    )
    .await;
    let body = reply.json();
    let file_id = body["fileId"].as_str().expect("fileId");
    assert_eq!(
        body["url"],
        format!("https://geo.example.com/og/compat/precisely/v2/files/{file_id}/content")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bulk_job_runs_on_og_jobs_and_downloads_as_precisely_csv() {
    let fixture = Fixture::new("bulk");
    let router = fixture.router();

    let reply = send(
        &router,
        Request::post("/compat/precisely/v2/files/upload-url")
            .header("x-forwarded-proto", "https")
            .header("x-forwarded-host", "og.example.com")
            .header(header::HOST, "10.0.0.7:8080")
            .body(Body::from(r#"{"fileName": "test"}"#))
            .expect("request"),
    )
    .await;
    let body = reply.json();
    let file_id = body["fileId"].as_str().expect("fileId");
    // A forwarded host is a header any client can send, so the URL takes
    // the host the client connected with.
    assert_eq!(
        body,
        json!({
            "fileId": file_id,
            "url": format!("http://10.0.0.7:8080/compat/precisely/v2/files/{file_id}/content"),
            "message": "This upload URL will be valid until the file expires.",
        })
    );

    let file_id = upload(
        &router,
        "\u{feff}addressId,Address,CITY,STATE,postalcode,COUNTRY\n\
         A1,100 King Street West,Toronto,ON,M5X 1A9,CAN\n\
         A2,99 Nowhere Lane,,,,CA\n\
         A3,100 King Street West,,,,ABC\n\
         A4,,,,,CA\n",
    )
    .await;
    assert_eq!(
        get(&router, "/compat/precisely/v2/files").await.json(),
        json!({"results": [{"fileId": file_id, "fileName": "test"}]})
    );

    let submitted = post(
        &router,
        "/v2/bulk-geocode",
        &json!({"fileId": file_id, "delimiter": ",", "preferences": {"maxResults": 1},
                "notifyEmails": ["ops@example.com"]}),
    )
    .await;
    assert_eq!(
        submitted.status,
        StatusCode::ACCEPTED,
        "{}",
        submitted.text()
    );
    let status_url = submitted.json()["statusUrl"]
        .as_str()
        .expect("statusUrl")
        .to_string();
    let job_id = status_url.rsplit('/').next().expect("job id").to_string();
    assert_eq!(
        status_url,
        format!("http://localhost/compat/precisely/v2/bulk/jobs/{job_id}")
    );
    // Precisely writes a job id as a hyphenated UUID; og's own is the same
    // UUID without hyphens.
    let og_id = uuid::Uuid::try_parse(&job_id)
        .expect("a UUID")
        .simple()
        .to_string();
    assert_eq!(
        uuid::Uuid::try_parse(&job_id)
            .expect("a UUID")
            .hyphenated()
            .to_string(),
        job_id
    );

    assert_eq!(
        finished(&router, &job_id).await,
        json!({
            "status": "COMPLETED",
            "downloadUrl": format!("http://localhost/compat/precisely/v2/bulk/jobs/{job_id}/output"),
        })
    );
    let og_job = native(&router, &format!("/jobs/{og_id}")).await;
    assert_eq!(og_job["status"], "completed");
    assert_eq!(
        og_job["summary"],
        json!({"total": 4, "succeeded": 2, "failed": 2})
    );

    let output = get(
        &router,
        &format!("/compat/precisely/v2/bulk/jobs/{job_id}/output"),
    )
    .await;
    assert_eq!(output.status, StatusCode::OK);
    assert_eq!(
        output.headers[header::CONTENT_TYPE],
        "text/csv; charset=utf-8"
    );
    let rows = output.text();
    let mut lines = rows.lines();
    assert_eq!(
        lines.next(),
        Some(super::result::Product::Geocode.columns().join(",").as_str())
    );
    let first = lines.next().expect("A1");
    let score = first.split(',').nth(3).expect("score");
    assert!(score.parse::<u8>().is_ok(), "{first}");
    assert_eq!(
        first,
        format!(
            "A1,OK,,{score},\"100 King Street West, Unit 5, ON, M5X 1A9\",\"100 King Street West, Unit 5\",\
             \"ON, M5X 1A9\",100,King Street West,5,,Financial District,Old Toronto,\
             Toronto,Toronto Division,Ontario,ON,M5X 1A9,,CAN,Canada,CA,124,43.6493,-79.3817,\
             epsg:4326,Point,ADDRESS_POINT,ADDRESS,\"{{\"\"pbKey\"\":\"\"{}\"\"}}\"",
            address_vendor_id("osm:node:10")
        )
    );
    let blank = ",".repeat(super::result::Product::Geocode.columns().len() - 4);
    assert_eq!(
        lines.collect::<Vec<_>>(),
        vec![
            format!("A2,ZERO_RESULTS,,{blank}"),
            format!(
                "A3,INVALID_REQUEST,Not able to identify requested country (DIS-LI-1016),{blank}"
            ),
            format!("A4,INVALID_REQUEST,Request Body is not valid (DIS-LI-1018),{blank}"),
        ]
    );

    assert_eq!(
        get(&router, "/compat/precisely/v2/bulk/jobs?status=all")
            .await
            .json(),
        json!({"jobs": []})
    );
    let cancel = send(
        &router,
        Request::delete(format!("/compat/precisely/v2/bulk/jobs/{job_id}"))
            .body(Body::empty())
            .expect("cancel"),
    )
    .await;
    assert_eq!(cancel.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        cancel.json(),
        errors(
            "BAD_REQUEST",
            "Job is COMPLETED and can no longer be canceled"
        )
    );

    let delete = || {
        Request::delete(format!("/compat/precisely/v2/files/{file_id}"))
            .body(Body::empty())
            .expect("delete")
    };
    let deleted = send(&router, delete()).await;
    assert_eq!(deleted.status, StatusCode::OK);
    assert_eq!(
        deleted.json(),
        json!({"status": "SUCCESS", "message": "File Deleted Successfully"})
    );
    assert_eq!(
        get(&router, "/compat/precisely/v2/files").await.json(),
        json!({"results": []})
    );
    let again = send(&router, delete()).await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        again.json(),
        errors("INVALID_REQUEST", "Invalid File Id (DIS-LI-1052)")
    );
    // The job outlives its input, as og keeps it.
    assert_eq!(finished(&router, &job_id).await["status"], "COMPLETED");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pipe_file_with_more_candidates_writes_a_row_each() {
    let fixture = Fixture::new("pipe");
    let router = fixture.router();
    let file_id = upload(
        &router,
        "addressid|address|country\nB1|King Street West|CA\n",
    )
    .await;
    let submitted = post(
        &router,
        "/v2/bulk-geocode",
        &json!({"fileId": file_id, "preferences": {"maxResults": 2}}),
    )
    .await;
    assert_eq!(
        submitted.status,
        StatusCode::ACCEPTED,
        "{}",
        submitted.text()
    );
    let job_id = submitted.json()["statusUrl"]
        .as_str()
        .and_then(|url| url.rsplit('/').next())
        .expect("job id")
        .to_string();
    assert_eq!(finished(&router, &job_id).await["status"], "COMPLETED");
    let output = get(
        &router,
        &format!("/compat/precisely/v2/bulk/jobs/{job_id}/output"),
    )
    .await
    .text();
    let ids = output
        .lines()
        .skip(1)
        .map(|line| line.split(',').take(2).collect::<Vec<_>>().join(","))
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["B1,OK", "B1,OK"]);
}

#[tokio::test]
async fn a_bulk_submit_precisely_would_refuse_is_refused() {
    let fixture = Fixture::new("bulk-refused");
    let router = fixture.router();
    let good = upload(&router, "addressid|address|country\n1|x|CA\n").await;
    let no_country = upload(&router, "addressid|address\n1|x\n").await;
    let twice = upload(&router, "addressid|address|country|Address\n1|x|CA|y\n").await;
    let empty = upload(&router, "").await;
    let missing = errors(
        "INVALID_REQUEST",
        "Mandatory input headers are missing in input file or the file is not UTF-8 encoded \
         (DIS-LI-1053)",
    );
    for (body, expected) in [
        (
            json!({"fileId": "8d907c8c-5b63-417f-b69f-2d33dbdd2fcd"}),
            errors("INVALID_REQUEST", "Invalid File Id (DIS-LI-1052)"),
        ),
        (
            json!({"fileId": "../../etc/passwd"}),
            errors("INVALID_REQUEST", "Invalid File Id (DIS-LI-1052)"),
        ),
        (
            json!({}),
            errors("INVALID_REQUEST", "Invalid File Id (DIS-LI-1052)"),
        ),
        (
            json!({"fileId": good, "delimiter": ":"}),
            errors(
                "INVALID_REQUEST",
                r"Delimiter must be one of the following: ',', ';', '\t', or '|'",
            ),
        ),
        (
            json!({"fileId": good, "preferences": {"maxResults": -1}}),
            errors(
                "INVALID_REQUEST",
                "Invalid value for maxResults (DIS-LI-1036)",
            ),
        ),
        (
            json!({"fileId": good, "notifyEmails": vec!["a@b.co"; 6]}),
            errors(
                "INVALID_REQUEST",
                "notifyEmails cannot contain more than 5 email addresses",
            ),
        ),
        (
            json!({"fileId": good, "notifyEmails": ["a@b.co", ""]}),
            errors(
                "INVALID_REQUEST",
                "notifyEmails cannot contain a null or empty email address",
            ),
        ),
        (
            json!({"fileId": good, "notifyEmails": ["not an email"]}),
            errors(
                "INVALID_REQUEST",
                "notifyEmails contains an invalid email address",
            ),
        ),
        (json!({"fileId": no_country}), missing.clone()),
        (json!({"fileId": empty}), missing),
        (
            json!({"fileId": twice}),
            errors(
                "INVALID_REQUEST",
                "Duplicate input headers found (DIS-LI-1054)",
            ),
        ),
    ] {
        let reply = post(&router, "/v2/bulk-geocode", &body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(reply.json(), expected, "{body}");
    }
    let five = post(
        &router,
        "/v2/bulk-geocode",
        &json!({"fileId": good, "notifyEmails": vec!["a@b.co"; 5]}),
    )
    .await;
    assert_eq!(five.status, StatusCode::ACCEPTED, "{}", five.text());

    let nameless = post(&router, "/v2/files/upload-url", &json!({"fileName": " "})).await;
    assert_eq!(nameless.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        nameless.json(),
        errors("INVALID_CLIENT_INPUT", "Request is not valid (DIS-LI-1018)")
    );
    for query in ["?status=done", "?page=2"] {
        let reply = get(&router, &format!("/compat/precisely/v2/bulk/jobs{query}")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(
            reply.json(),
            errors("INVALID_CLIENT_INPUT", "Request is not valid (DIS-LI-1018)")
        );
    }
    for id in ["180c0892-affc-4f7a-b1c3-d5e965a6eefb", "nope"] {
        let reply = get(&router, &format!("/compat/precisely/v2/bulk/jobs/{id}")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{id}");
        assert_eq!(
            reply.json(),
            errors("INVALID_REQUEST", "Invalid Job Id (DIS-LI-1051)")
        );
    }
    let put = send(
        &router,
        Request::put("/compat/precisely/v2/files/8d907c8c-5b63-417f-b69f-2d33dbdd2fcd/content")
            .body(Body::from("addressid|address|country\n"))
            .expect("put"),
    )
    .await;
    assert_eq!(put.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        put.json(),
        errors("INVALID_REQUEST", "Invalid File Id (DIS-LI-1052)")
    );
}

#[tokio::test]
async fn an_upload_may_be_as_large_as_a_job_upload() {
    let fixture = Fixture::new("upload-cap");
    let mut limits = Limits::DEFAULT;
    limits.max_job_upload_mib = std::num::NonZeroU64::new(1);
    let router = fixture.router_with(limits);
    let reply = post(&router, "/v2/files/upload-url", &json!({"fileName": "big"})).await;
    let file_id = reply.json()["fileId"].as_str().expect("fileId").to_string();
    let put = |bytes: usize| {
        Request::put(format!("/compat/precisely/v2/files/{file_id}/content"))
            .body(Body::from(vec![b'a'; bytes]))
            .expect("put")
    };
    assert_eq!(send(&router, put(1 << 20)).await.status, StatusCode::OK);
    let over = send(&router, put((1 << 20) + 1)).await;
    assert_eq!(over.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        over.json(),
        errors("INVALID_REQUEST", "The file is over 1048576 bytes")
    );
}

#[test]
fn a_country_is_read_as_any_iso_code() {
    for (sent, sent_on) in [
        ("USA", "US"),
        ("usa", "US"),
        ("840", "US"),
        ("036", "AU"),
        ("CA", "CA"),
        (" nz ", "nz"),
        // Not a code og knows. og decides, refusing all but two letters.
        ("ABC", "ABC"),
        ("999", "999"),
        ("Canada", "Canada"),
    ] {
        assert_eq!(super::country_code(sent), sent_on, "{sent}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_and_job_records_expire_with_og_jobs() {
    let fixture = Fixture::new("sweep");
    let state = fixture.state(Limits::DEFAULT);
    let router = build_router(state.clone(), None, None);
    let kept = upload(&router, "addressid|address|country\n1|99 Nowhere Lane|CA\n").await;
    let expired = upload(&router, "addressid|address|country\n").await;
    let files = state.jobs().dir().join("precisely/files");
    let record = files.join(format!("{expired}.json"));
    let mut stale =
        serde_json::from_slice::<Value>(&std::fs::read(&record).expect("record")).expect("JSON");
    stale["createdNs"] = json!(0);
    std::fs::write(&record, stale.to_string()).expect("write");
    // Past its retention it is gone at once, before any sweep.
    assert_eq!(
        get(&router, "/compat/precisely/v2/files").await.json(),
        json!({"results": [{"fileId": kept, "fileName": "test"}]})
    );

    let submitted = post(&router, "/v2/bulk-geocode", &json!({"fileId": kept})).await;
    let job_id = submitted.json()["statusUrl"]
        .as_str()
        .and_then(|url| url.rsplit('/').next())
        .expect("job id")
        .to_string();
    assert_eq!(finished(&router, &job_id).await["status"], "COMPLETED");
    let og_id = uuid::Uuid::try_parse(&job_id)
        .expect("a UUID")
        .simple()
        .to_string();
    let deleted = send(
        &router,
        Request::delete(format!("/jobs/{og_id}"))
            .body(Body::empty())
            .expect("delete"),
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);

    super::sweep(&state);
    let mut left = std::fs::read_dir(&files)
        .expect("files")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("name")
        })
        .collect::<Vec<_>>();
    left.sort();
    assert_eq!(left, vec![format!("{kept}.csv"), format!("{kept}.json")]);
    let jobs = std::fs::read_dir(state.jobs().dir().join("precisely/jobs"))
        .expect("jobs")
        .count();
    assert_eq!(jobs, 0);
    let gone = get(&router, &format!("/compat/precisely/v2/bulk/jobs/{job_id}")).await;
    assert_eq!(gone.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        gone.json(),
        errors("INVALID_REQUEST", "Invalid Job Id (DIS-LI-1051)")
    );
}

/// The standardized result verify gives for 100 King Street West, with
/// the score og's confidence makes.
fn verified_result(score: &Value, stated: &[&str], codes: [&str; 2]) -> Value {
    let description =
        address_result(score, stated)["explanation"]["addressMatch"]["description"].clone();
    json!({
        "score": score,
        "address": {
            "formattedAddress": "100 King Street West, Unit 5, ON, M5X 1A9",
            "formattedStreetAddress": "100 King Street West, Unit 5",
            "formattedLocationAddress": "ON, M5X 1A9",
            "addressNumber": "100",
            "street": "King Street West",
            "unit": "5",
            "neighborhood": {"longName": "Financial District"},
            "suburb": {"longName": "Old Toronto"},
            "city": {"longName": "Toronto"},
            "admin2": {"longName": "Toronto Division"},
            "admin1": {"longName": "Ontario", "shortName": "ON"},
            "postalCode": "M5X 1A9",
            "country": {"name": "Canada", "isoAlpha2Code": "CA", "isoAlpha3Code": "CAN",
                        "isoNumericCode": "124"},
        },
        "addressLines": ["Unit 5", "100 King Street West", "Toronto, ON M5X 1A9", "Canada"],
        "explanation": {"addressMatch": {"type": "ADDRESS", "description": description}},
        // Verify places nothing, so it restates no geohash.
        "customFields": restated(json!({
            "PB_KEY": address_vendor_id("osm:node:10"),
            "MATCH_TYPE": "ADDRESS",
            "SIMPLE_MATCH_CODE": codes[0],
            "EXTENDED_MATCH_CODE": codes[1],
        })),
    })
}

#[tokio::test]
async fn verify_answers_the_matched_address_standardized_without_a_point() {
    let fixture = Fixture::new("verify");
    let router = fixture.router();
    let og = native(
        &router,
        "/geocode?address=100%20King%20Street%20West&locality=Toronto&postcode=M5X%201A9\
         &country=CA&limit=1",
    )
    .await;
    let properties = &og["features"][0]["properties"];
    let confidence = properties["confidence"].as_f64().expect("calibrated");
    assert_eq!(
        properties["match"],
        json!({"housenumber": "matched", "street": "matched", "locality": "matched",
               "postcode": "matched", "country": "matched"})
    );
    let score = json!((confidence * 100.0).round() as u8);
    let stars = (confidence * 5.0).round() as u8;

    let reply = post(
        &router,
        "/v1/verify",
        &json!({"addresses": [
            {"addressId": "1", "addressNumber": "100", "street": "King Street West",
             "city": "Toronto", "postalCode": "M5X 1A9", "country": "CA"},
            {"addressId": "2", "addressLines": ["99 Nowhere Lane"], "country": "CAN"},
        ]}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json(),
        json!({"responses": [
            {"addressId": "1", "status": "OK", "results": [verified_result(
                &score,
                &["addressNumber", "street", "city", "postalCode", "country"],
                [
                    &format!("V4-P4S4A6T4R6Z4C4-{stars}"),
                    &format!("V4-D0M0B0U6F0G0P4S4Q0X0A6T4E0R6W0Z4Y0C4-{stars}"),
                ],
            )]},
            {"addressId": "2", "status": "ZERO_RESULTS", "results": []},
        ]})
    );

    // The code verify gives reads back through explain-addresscode.
    let explained = post(
        &router,
        "/v1/explain-addresscode",
        &json!({"codeType": "GlobalMatchCode",
                "codes": [{"code": format!("V4-P4S4A6T4R6Z4C4-{stars}")}]}),
    )
    .await;
    assert_eq!(explained.status, StatusCode::OK);
    assert!(
        explained.json()["responses"][0]["result"]["customerInsightSummary"]
            .as_str()
            .expect("summary")
            .starts_with("Address premise verified with ")
    );
}

#[tokio::test]
async fn explain_reads_a_global_match_code_back_as_words() {
    let fixture = Fixture::new("explain");
    let router = fixture.router();
    let reply = post(
        &router,
        "/v1/explain-addresscode",
        &json!({"codeType": "GlobalMatchCode",
                "codes": [{"id": "1", "code": "V4-P4S4A0T4R6Z4C4-5"}],
                "responseOptions": {"includeOfficialDescription": true}}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let body = reply.json();
    let generated = body["generatedAt"].as_str().expect("generatedAt");
    assert_eq!(
        generated.len(),
        "2025-12-18T13:48:29.842Z".len(),
        "{generated}"
    );
    let component = |label: &str, value: u8, meaning: &str| json!({"label": label, "value": value, "meaning": meaning});
    assert_eq!(
        body["responses"],
        json!([{"id": "1", "status": "OK", "result": {
            "codeType": "GlobalMatchCode",
            "customerInsightSummary":
                "Address premise verified with high confidence. Region/State appended from \
                 reference data.",
            "officialDescription": {
                "addressMatchStatus": {"symbol": "V4", "status": "Verified",
                    "verificationLevel": 4, "verificationLevelLabel": "Premise verified"},
                "componentMatchStatus": {
                    "A": component("Sub-city", 0, "Not found or empty"),
                    "C": component("Country", 4, "Verified without changes"),
                    "P": component("Premise/House Number", 4, "Verified without changes"),
                    "R": component("Region/State", 6, "Appended using reference data"),
                    "S": component("Street", 4, "Verified without changes"),
                    "T": component("City", 4, "Verified without changes"),
                    "Z": component("Postcode/Zip Code", 4, "Verified without changes"),
                },
                "matchConfidence": {"value": 5, "label": "High Confidence"},
            },
        }}])
    );

    // Without the option, no official description.
    let reply = post(
        &router,
        "/v1/explain-addresscode",
        &json!({"codeType": "GlobalMatchCode", "codes": [{"code": "V3-P1S4C4-2"}]}),
    )
    .await;
    assert_eq!(
        reply.json()["responses"],
        json!([{"status": "OK", "result": {"codeType": "GlobalMatchCode",
            "customerInsightSummary": "Address street verified with medium-low confidence. \
                Premise/House Number not identified in reference data."}}])
    );
}

#[tokio::test]
async fn explain_answers_each_code_it_cannot_read() {
    let fixture = Fixture::new("explain-errors");
    let router = fixture.router();
    let explain = |body: Value| {
        let router = router.clone();
        async move { post(&router, "/v1/explain-addresscode", &body).await }
    };
    let reply = explain(json!({"codeType": "GlobalMatchCode", "codes": [
        {"id": "1", "code": "V4-P4S4C4-5"},
        {"id": "2", "code": "V4-D0U4M0-5"},
        {"id": "3", "code": " "},
    ]}))
    .await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    let responses = reply.json()["responses"].clone();
    assert_eq!(responses[0]["status"], "OK");
    assert_eq!(
        responses[1],
        json!({"id": "2", "status": "INVALID_REQUEST", "errorMessage":
            "Invalid code format for GlobalMatchCode in the request (DIS-LI-1040): component \
             order violation - Company (M) must come before Sub-premise/Unit (U)"})
    );
    assert_eq!(
        responses[2],
        json!({"id": "3", "status": "INVALID_REQUEST",
               "errorMessage": "Code is null or blank (DIS-LI-1039)"})
    );

    // Every code malformed fails the request.
    let reply =
        explain(json!({"codeType": "GlobalMatchCode", "codes": [{"code": "V5-P4-5"}]})).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json(),
        errors(
            "INVALID_REQUEST",
            "Invalid code format for GlobalMatchCode in the request (DIS-LI-1040): verification \
             level must be 1-4"
        )
    );

    // A code type og issues none of is answered, item by item.
    let reply =
        explain(json!({"codeType": "ForwardResultCode", "codes": [{"code": "S8HPNTSCZA"}]})).await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        reply.json()["responses"],
        json!([{"status": "ERROR", "errorMessage":
            "ForwardResultCode is not explained by this server, which issues Global Match \
             Codes only"}])
    );

    let too_many = (0..26)
        .map(|_| json!({"code": "V4-P4-5"}))
        .collect::<Vec<_>>();
    for (body, status, message) in [
        (
            json!({"codes": [{"code": "V4-P4-5"}]}),
            ("INVALID_REQUEST", StatusCode::BAD_REQUEST),
            "CodeType is not provided in the request (DIS-LI-1037)",
        ),
        (
            json!({"codeType": "Morse", "codes": [{"code": "V4-P4-5"}]}),
            ("INVALID_REQUEST", StatusCode::BAD_REQUEST),
            "Invalid CodeType, it must be a valid codeType (DIS-LI-1038)",
        ),
        (
            json!({"codeType": "GlobalMatchCode", "codes": []}),
            ("INVALID_REQUEST", StatusCode::BAD_REQUEST),
            "Request Body is not valid (DIS-LI-1018)",
        ),
        (
            json!({"codeType": "GlobalMatchCode", "codes": too_many}),
            ("BATCH_SIZE_EXCEEDED", StatusCode::PAYLOAD_TOO_LARGE),
            "Batch size must not exceed 25 (DIS-LI-1014)",
        ),
    ] {
        let reply = explain(body).await;
        assert_eq!(reply.status, status.1, "{message}");
        assert_eq!(reply.json(), errors(status.0, message));
    }
}

#[tokio::test]
async fn proximity_search_finds_the_nearest_address() {
    let fixture = Fixture::new("proximity");
    let router = fixture.router();
    let og = native(&router, "/reverse?lon=-79.3816&lat=43.6494&layer=address").await;
    let metres = og["features"][0]["properties"]["distance_m"]
        .as_f64()
        .expect("distance");
    let search = |distance: Value, country: &str| {
        let router = router.clone();
        let body = json!({
            "preferences": {"dataset": "osm", "maxResults": 1, "distance": distance},
            "location": {"addressId": "7", "latitude": 43.6494, "longitude": -79.3816,
                         "country": country},
        });
        async move { post(&router, "/v1/proximity-search", &body).await }
    };
    let reply = search(json!({"value": 1, "distanceUnit": "KILOMETER"}), "CAN").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json(),
        json!({"response": {"addressId": "7", "status": "OK", "results": [{
            "distance": metres / 1000.0,
            "location": {"latitude": 43.6493, "longitude": -79.3817},
            "metadata": {
                "PBKEY": address_vendor_id("osm:node:10"),
                "FORMATTEDADDRESS": "100 King Street West, Unit 5, ON, M5X 1A9",
                "ADDRLINE1": "100 King Street West, Unit 5",
                "ADDNUMBER": "100",
                "STREETNAME": "King Street West",
                "CITY": "Toronto",
                "ADMIN2": "Toronto Division",
                "ADMIN1": "ON",
                "POSTALCODE": "M5X 1A9",
                "COUNTRY": "CAN",
            },
        }]}})
    );
    let reply = search(json!({"value": 100, "distanceUnit": "FOOT"}), "CA").await;
    let feet = reply.json()["response"]["results"][0]["distance"]
        .as_f64()
        .expect("feet");
    assert!((feet - metres / 0.3048).abs() < 1e-9, "{feet}");

    // Out of reach, or over the border, is nothing.
    for (distance, country) in [
        (json!({"value": 1, "distanceUnit": "METER"}), "CAN"),
        (json!({"value": 1, "distanceUnit": "KILOMETER"}), "USA"),
    ] {
        assert_eq!(
            search(distance, country).await.json(),
            json!({"response": {"addressId": "7", "status": "ZERO_RESULTS", "results": []}})
        );
    }

    for (body, message) in [
        (
            json!({"preferences": {}, "location": {"latitude": 1, "longitude": 1, "country": "CA"}}),
            "Dataset is not provided in the request (DIS-LI-1031)",
        ),
        (
            json!({"preferences": {"dataset": "premium-address-points"},
                   "location": {"latitude": 1, "longitude": 1, "country": "CA"}}),
            "Dataset premium-address-points is not held by this server, which searches the \
             osm and openaddresses addresses of its Pack (DIS-LI-1031)",
        ),
        (
            json!({"preferences": {"dataset": "osm", "maxResults": 3},
                   "location": {"latitude": 1, "longitude": 1, "country": "CA"}}),
            "Invalid value for maxResults, this server answers the one nearest address \
             (DIS-LI-1036)",
        ),
        (
            json!({"preferences": {"dataset": "osm"}, "location": {"latitude": 1, "longitude": 1}}),
            "Country is not provided in the request (DIS-LI-1015)",
        ),
        (
            json!({"preferences": {"dataset": "osm"}, "location": {"latitude": 1, "country": "CA"}}),
            "Request Body is not valid (DIS-LI-1018)",
        ),
        (
            json!({"preferences": {"dataset": "osm", "distance": {"value": 1, "distanceUnit": "CUBIT"}},
                   "location": {"latitude": 1, "longitude": 1, "country": "CA"}}),
            "Request Body is not valid (DIS-LI-1018)",
        ),
        (
            json!({"preferences": {"dataset": "osm", "maxResults": 0},
                   "location": {"latitude": 1, "longitude": 1, "country": "CA"}}),
            "Invalid value for maxResults (DIS-LI-1036)",
        ),
    ] {
        let reply = post(&router, "/v1/proximity-search", &body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{message}");
        assert_eq!(reply.json(), errors("INVALID_REQUEST", message));
    }
}

#[tokio::test]
async fn postal_city_completes_a_postcode_with_its_town() {
    let fixture = Fixture::new("postal-city");
    let router = fixture.router();
    let prediction = json!({
        "address": {
            "country": {"isoAlpha3Code": "CAN"},
            "admin1": {"longName": "Ontario", "shortName": "ON"},
            "admin2": {"longName": "Toronto Division"},
            "city": {"longName": "Toronto"},
            "postalCode": "M5X 1A9",
        },
        "location": {"feature": {"type": "Feature", "properties": {"crsName": "epsg:4326"},
            "geometry": {"type": "Point", "coordinates": [-79.3815, 43.6492]}}},
    });
    // With no type named, text with a digit is a postcode.
    for address in [
        json!({"type": "POSTAL", "postAddress": "M5X", "country": "CAN"}),
        json!({"postAddress": "m5x 1", "country": "CA"}),
    ] {
        let reply = post(
            &router,
            "/v1/autocomplete/postal-city",
            &json!({"preferences": {"maxResults": 5}, "address": address}),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.json(),
            json!({"response": {"status": "OK", "predictions": [prediction]}})
        );
    }
    let reply = post(
        &router,
        "/v1/autocomplete/postal-city",
        &json!({"address": {"type": "POSTAL", "postAddress": "H2X", "country": "CAN"}}),
    )
    .await;
    assert_eq!(
        reply.json(),
        json!({"response": {"status": "ZERO_RESULTS", "predictions": []}})
    );
    for (address, message) in [
        (
            json!({"type": "STREET", "postAddress": "M5X", "country": "CAN"}),
            "Type should be either POSTAL or CITY only (DIS-LI-1025)",
        ),
        (
            json!({"type": "POSTAL", "postAddress": "M5X"}),
            "Country is not provided in the request (DIS-LI-1015)",
        ),
        (
            json!({"type": "POSTAL", "postAddress": "", "country": "CAN"}),
            "Request Body is not valid (DIS-LI-1018)",
        ),
    ] {
        let reply = post(
            &router,
            "/v1/autocomplete/postal-city",
            &json!({ "address": address }),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{message}");
        assert_eq!(reply.json(), errors("INVALID_REQUEST", message));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bulk_verify_job_writes_verify_output() {
    let fixture = Fixture::new("bulk-verify");
    let router = fixture.router();
    let file_id = upload(
        &router,
        "addressId,address,city,admin1,postalCode,country\n\
         V1,100 King Street West,Toronto,,M5X 1A9,CA\n\
         V2,99 Nowhere Lane,,,,CA\n",
    )
    .await;
    let submitted = post(
        &router,
        "/v2/bulk-verify",
        &json!({"fileId": file_id, "delimiter": ","}),
    )
    .await;
    assert_eq!(
        submitted.status,
        StatusCode::ACCEPTED,
        "{}",
        submitted.text()
    );
    let status_url = submitted.json()["statusUrl"]
        .as_str()
        .expect("statusUrl")
        .to_string();
    let job_id = status_url.rsplit('/').next().expect("job id").to_string();
    assert_eq!(finished(&router, &job_id).await["status"], "COMPLETED");
    let output = get(
        &router,
        &format!("/compat/precisely/v2/bulk/jobs/{job_id}/output"),
    )
    .await;
    let rows = output.text();
    let mut lines = rows.lines();
    let header = lines.next().expect("header");
    assert_eq!(header, super::result::Product::Verify.columns().join(","));
    assert!(!header.contains("latitude") && !header.contains("longitude"));
    let first = lines.next().expect("V1");
    let score = first.split(',').nth(3).expect("score");
    let stars = (score.parse::<f64>().expect("score") / 20.0).round();
    assert_eq!(
        first,
        format!(
            "V1,OK,,{score},\"100 King Street West, Unit 5, ON, M5X 1A9\",\"100 King Street West, Unit 5\",\
             \"ON, M5X 1A9\",100,King Street West,5,,Financial District,Old Toronto,\
             Toronto,Toronto Division,Ontario,ON,M5X 1A9,,CAN,Canada,CA,124,,,,ADDRESS,\
             \"{{\"\"extendedMatchCode\"\":\"\"V4-D0M0B0U6F0G0P4S4Q0X0A6T4E0R6W0Z4Y0C4-{stars}\"\",\
             \"\"pbKey\"\":\"\"{}\"\",\"\"simpleMatchCode\"\":\"\"V4-P4S4A6T4R6Z4C4-{stars}\"\"}}\"",
            address_vendor_id("osm:node:10")
        )
    );
    let blank = ",".repeat(super::result::Product::Verify.columns().len() - 4);
    assert_eq!(
        lines.next(),
        Some(format!("V2,ZERO_RESULTS,,{blank}").as_str())
    );
}

#[tokio::test]
async fn express_autocomplete_v2_filters_and_biases_by_city_and_postcode() {
    let fixture = Fixture::new("filter-bias");
    let router = fixture.router();
    let address = json!({"addressLines": ["Queen Bak"], "country": "CAN"});
    let ask = |filter_bias: Value| {
        let router = router.clone();
        let address = address.clone();
        async move {
            post(
                &router,
                "/v2/express-autocomplete",
                &json!({"preferences": {"filterBias": filter_bias}, "address": address}),
            )
            .await
        }
    };
    let cities = |reply: &Value| {
        reply["response"]["predictions"]
            .as_array()
            .map(|predictions| {
                predictions
                    .iter()
                    .map(|p| p["address"]["city"]["longName"].clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let kept =
        ask(json!({"type": "filter", "fields": {"include": {"city": {"value": "toronto"}}}})).await;
    assert_eq!(kept.status, StatusCode::OK);
    let found = cities(&kept.json());
    assert!(
        !found.is_empty() && found.iter().all(|city| city == "Toronto"),
        "{}",
        kept.json()
    );
    let by_postcode =
        ask(json!({"type": "filter", "fields": {"include": {"postalCode": {"value": "M5"}}}}))
            .await;
    assert_eq!(cities(&by_postcode.json()), found, "{}", by_postcode.json());
    let dropped =
        ask(json!({"type": "filter", "fields": {"exclude": {"city": {"value": "Toronto"}}}})).await;
    assert_eq!(
        dropped.json()["response"]["status"],
        "ZERO_RESULTS",
        "{}",
        dropped.json()
    );
    let biased =
        ask(json!({"type": "bias", "fields": {"include": {"city": {"value": "Elsewhere"}}}})).await;
    assert_eq!(cities(&biased.json()), found, "a bias drops nothing");
    for refused in [
        json!({"type": "bias", "fields": {"exclude": {"city": {"value": "Toronto"}}}}),
        json!({"type": "filter", "fields": {"include": {"poiCategory": {"value": "5800"}}}}),
    ] {
        let reply = ask(refused.clone()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{refused}");
        assert_eq!(reply.json()["errors"][0]["status"], "INVALID_REQUEST");
    }
}

/// Files asked for within one second list in the order they were asked for.
#[tokio::test]
async fn files_list_in_the_order_they_were_uploaded() {
    let fixture = Fixture::new("file-order");
    let router = fixture.router();
    let mut ids = Vec::new();
    for _ in 0..6 {
        ids.push(upload(&router, "addressid|address|country\n").await);
    }
    let listed = get(&router, "/compat/precisely/v2/files").await.json()["results"]
        .as_array()
        .expect("results")
        .iter()
        .map(|file| file["fileId"].as_str().expect("fileId").to_string())
        .collect::<Vec<_>>();
    assert_eq!(listed, ids);
}

/// How a result found by a query describes the parts og judged.
fn assert_judged(
    route: Route,
    feature: &Value,
    result: &Value,
    judged: &Value,
    description: &Value,
) {
    // Autocomplete judges no part; it says which the typed text marked.
    if judged.is_null() && route != Route::Autocomplete {
        assert_eq!(description, &Value::Null, "{feature} as {result}");
    }
    for (part, label) in [
        ("housenumber", "addressNumber"),
        ("street", "street"),
        ("unit", "unitMatch"),
        ("locality", "city"),
        ("region", "admin1"),
        ("postcode", "postalCode"),
    ] {
        let Some(judged) = judged[part].as_str() else {
            continue;
        };
        let described = description
            .as_array()
            .and_then(|parts| parts.iter().find(|entry| entry["label"] == label))
            .unwrap_or_else(|| panic!("{label} is not described in {result}"));
        let kind = match judged {
            "matched" => "EXACT",
            "partial" => "PARTIAL",
            "unmatched" => "NONE",
            other => panic!("og judged {part} {other}"),
        };
        assert_eq!(described["matchType"], kind, "{part} of {result}");
    }
}
