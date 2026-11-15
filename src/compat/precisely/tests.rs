//! `/compat/precisely` driven through og's router on a small Pack.

use std::{
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
    compat::{decode_id, encode_id, iso},
    confidence::{FEATURES, Model},
    context::{AdminContextTuple, RecordContext},
    limits::Limits,
    pack::Writer,
    record::{
        AddressComponents, AddressRecord, InterpolationAddressComponents, InterpolationRange,
        InterpolationRecord, LocationPrecision, ObjectType, PlaceLayer, PlaceRecord, PoiRecord,
        Record, SourceProvenance, point_geometry,
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
    let interpolation_context = context;
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

/// Every og value of `feature` in its Precisely slot, unchanged.
fn assert_carries(feature: &Value, result: &Value) {
    let properties = &feature["properties"];
    let address = &properties["address"];
    let context = &properties["context"];
    let carried = &result["address"];
    assert_eq!(
        decode_id(result["customFields"]["PB_KEY"].as_str().expect("PB_KEY")).as_deref(),
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
    match properties["confidence"].as_f64() {
        Some(confidence) => assert_eq!(result["score"], json!((confidence * 100.0).round() as u8)),
        None => assert_eq!(result.get("score"), None),
    }
    let pairs = [
        (&address["housenumber"], &carried["addressNumber"]),
        (&address["street"], &carried["street"]),
        (&address["unit"], &carried["unit"]),
        (&address["postcode"], &carried["postalCode"]),
        (&context["region"]["name"], &carried["admin1"]["longName"]),
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
        (&context["country"]["name"], &carried["country"]["name"]),
        (
            &context["country"]["code"],
            &carried["country"]["isoAlpha2Code"],
        ),
    ];
    for (og, precisely) in pairs {
        assert_eq!(og, precisely, "{feature} as {result}");
    }
    // The region's own code is its short name; the region the record states
    // stands in when og knows no code.
    let short = context["region"]["code"]
        .as_str()
        .and_then(|code| code.split_once('-'))
        .map(|(_, own)| json!(own))
        .unwrap_or_else(|| address["region"].clone());
    assert_eq!(
        carried["admin1"]["shortName"], short,
        "{feature} as {result}"
    );
    let code = context["country"]["code"].as_str().expect("country code");
    assert_eq!(
        carried["country"]["isoAlpha3Code"].as_str(),
        iso::alpha3(code)
    );
    assert_eq!(
        carried["country"]["isoNumericCode"].as_str(),
        iso::numeric(code)
    );
}

fn address_result(score: &Value) -> Value {
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
        "explanation": {"addressMatch": {"type": "ADDRESS"}},
        "customFields": {"PB_KEY": encode_id("osm:node:10")},
    })
}

#[tokio::test]
async fn token_is_handed_out_without_checking_credentials() {
    let fixture = Fixture::new("token");
    let router = fixture.router();
    let token = |body: &'static str| {
        Request::post("/compat/precisely/auth/v2/token")
            .header(header::AUTHORIZATION, "Basic bm90OmNoZWNrZWQ=")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .expect("request")
    };

    let reply = send(
        &router,
        token("grant_type=client_credentials&scope=default"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let mut body = reply.json();
    let access_token = body["access_token"].take();
    assert_eq!(
        body,
        json!({"access_token": null, "token_type": "Bearer", "expires_in": 3600, "scope": "default"})
    );
    let access_token = access_token.as_str().expect("token").to_string();
    assert_eq!(access_token.len(), 64);
    assert!(access_token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let again = send(&router, token("grant_type=client_credentials")).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_ne!(again.json()["access_token"], json!(access_token));

    for (body, error, description) in [
        (
            "",
            "invalid_request",
            "Request body is not specified (DIS-AUTH-1019)",
        ),
        (
            "grant_type=password",
            "unsupported_grant_type",
            "Invalid grant_type (DIS-AUTH-1016)",
        ),
        (
            "grant_type=client_credentials&scope=admin",
            "invalid_scope",
            "Invalid scope value (DIS-AUTH-1021)",
        ),
    ] {
        let reply = send(&router, token(body)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            reply.json(),
            json!({"error": error, "error_description": description})
        );
    }
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
            {"addressId": "1", "status": "OK", "results": [address_result(&score)]},
            {"addressId": "2", "status": "ZERO_RESULTS", "results": []},
            {"addressId": "3", "status": "OK", "results": [address_result(&structured_score)]},
        ]})
    );
}

#[tokio::test]
async fn every_og_value_lands_in_its_precisely_slot() {
    let fixture = Fixture::new("fidelity");
    let router = fixture.router();

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
        assert_carries(feature, result);
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

    let og = native(&router, "/reverse?lon=-79.3817&lat=43.6493").await;
    let reply = post(
        &router,
        "/v1/reverse-geocode",
        &json!({"preferences": {"maxResults": 1},
                "locations": [{"longitude": -79.3817, "latitude": 43.6493, "country": "CAN"}]}),
    )
    .await;
    assert_carries(
        &og["features"][0],
        &reply.json()["responses"][0]["results"][0],
    );

    let og = native(&router, "/autocomplete?q=Queen%20Bak&limit=1").await;
    let reply = post(
        &router,
        "/v1/autocomplete",
        &json!({"address": {"addressLines": ["Queen Bak"], "country": "CAN"}}),
    )
    .await;
    let prediction = &reply.json()["response"]["predictions"][0];
    assert_carries(&og["features"][0], prediction);
    assert_eq!(
        prediction["prediction"],
        og["features"][0]["properties"]["label"]
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
            "explanation": {"addressMatch": {"type": "POI"}},
            "customFields": {"PB_KEY": encode_id("osm:way:11")},
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
             "errorMessage": "q must not be empty (DIS-LI-1018)"},
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
            {"addressId": "a", "longitude": -79.3817, "latitude": 43.6493, "country": "USA"},
            {"addressId": "b", "latitude": 43.6493, "country": "CAN"},
            {"addressId": "c", "longitude": -79.3817, "latitude": 91.0, "country": "CAN"},
        ]}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    let mut expected = address_result(&json!(null));
    expected.as_object_mut().expect("result").remove("score");
    assert_eq!(
        reply.json(),
        json!({"responses": [
            {"addressId": "a", "status": "OK", "results": [expected]},
            {"addressId": "b", "status": "INVALID_REQUEST",
             "errorMessage": "could not parse query parameters (DIS-LI-1018)"},
            {"addressId": "c", "status": "INVALID_REQUEST",
             "errorMessage": "coordinate out of range (DIS-LI-1018)"},
        ]})
    );
}

#[tokio::test]
async fn lookup_finds_a_record_by_the_pb_key_og_issued() {
    let fixture = Fixture::new("lookup");
    let router = fixture.router();
    let reply = post(
        &router,
        "/v1/lookup",
        &json!({"keys": [{"key": encode_id("osm:way:11"), "type": "PB_KEY", "country": "CAN"}]}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let og = native(&router, "/places?ids=osm:way:11").await;
    let body = reply.json();
    assert_eq!(body["responses"][0]["status"], "OK");
    assert_eq!(body["responses"][0].get("addressId"), None);
    assert_carries(&og["features"][0], &body["responses"][0]["results"][0]);

    let reply = post(
        &router,
        "/v1/lookup",
        &json!({"keys": [
            {"key": encode_id("osm:node:999"), "type": "PB_KEY", "country": "CAN"},
            // og looks places up by id, not address points.
            {"key": encode_id("osm:node:10"), "type": "PB_KEY", "country": "CAN"},
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
async fn what_og_lacks_is_not_implemented_and_the_rest_not_found() {
    let fixture = Fixture::new("missing");
    let router = fixture.router();
    let not_implemented = errors(
        "NOT_IMPLEMENTED",
        "This API is not implemented by this server",
    );
    for path in [
        "/v1/verify",
        "/v2/bulk-verify",
        "/v1/autocomplete/postal-city",
        "/v1/explain-addresscode",
        "/v1/proximity-search",
    ] {
        let reply = post(&router, path, &json!({})).await;
        assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED, "{path}");
        assert_eq!(reply.json(), not_implemented, "{path}");
    }
    let not_found = errors("NOT_FOUND", "No such resource on this server");
    for (method, path) in [
        ("POST", "/compat/precisely/v1/nowhere"),
        ("GET", "/compat/precisely/v1/geocode"),
        ("DELETE", "/compat/precisely/v2/bulk-geocode"),
        ("GET", "/compat/precisely/v1/bulk/jobs"),
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
    assert_eq!(
        body,
        json!({
            "fileId": file_id,
            "url": format!("https://og.example.com/compat/precisely/v2/files/{file_id}/content"),
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

    assert_eq!(
        finished(&router, &job_id).await,
        json!({
            "status": "COMPLETED",
            "downloadUrl": format!("http://localhost/compat/precisely/v2/bulk/jobs/{job_id}/output"),
        })
    );
    let og_job = native(&router, &format!("/jobs/{job_id}")).await;
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
        Some(super::result::COLUMNS.join(",").as_str())
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
            encode_id("osm:node:10")
        )
    );
    let blank = ",".repeat(super::result::COLUMNS.len() - 4);
    assert_eq!(
        lines.collect::<Vec<_>>(),
        vec![
            format!("A2,ZERO_RESULTS,,{blank}"),
            format!(
                "A3,INVALID_REQUEST,Not able to identify requested country (DIS-LI-1016),{blank}"
            ),
            format!("A4,INVALID_REQUEST,q must not be empty (DIS-LI-1018),{blank}"),
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
    stale["createdAt"] = json!(0);
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
    let deleted = send(
        &router,
        Request::delete(format!("/jobs/{job_id}"))
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
