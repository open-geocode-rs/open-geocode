use std::path::{Path, PathBuf};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::{
    compat::encode_id,
    context::{AdminContextTuple, RecordContext},
    limits::Limits,
    pack::Writer,
    record::{
        AddressComponents, AddressRecord, LocationPrecision, ObjectType, PlaceLayer, PlaceRecord,
        PoiRecord, Record, SourceProvenance, point_geometry,
    },
    runtime::{AppState, build_router},
};

/// Removes the Pack when a test ends, even on a failed assertion.
struct Fixture {
    pack: PathBuf,
    router: Router,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.pack);
        let _ = std::fs::remove_dir_all(self.pack.with_extension("jobs"));
    }
}

fn fixture() -> Fixture {
    let pack = std::env::temp_dir().join(format!(
        "open-geocode-compat-google-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    write_pack(&pack);
    let state = AppState::open(&pack, &pack.with_extension("jobs"), Limits::DEFAULT).expect("pack");
    let router = build_router(state, None, None);
    Fixture { pack, router }
}

/// Canada > Ontario > Golden Horseshoe > York > Toronto > Old Toronto >
/// St. Lawrence, holding an address with every part stated and a bakery.
fn write_pack(dir: &Path) {
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
                        geometry: point_geometry(-79.0, 43.0),
                        source: SourceProvenance::osm(ObjectType::Relation, id),
                        prominence: Default::default(),
                        wikidata: None,
                    },
                ),
                None,
            )
            .expect("place")
    };
    let tiers = [
        (PlaceLayer::Country, place(PlaceLayer::Country, "Canada", 1)),
        (PlaceLayer::Region, place(PlaceLayer::Region, "Ontario", 2)),
        (
            PlaceLayer::SubRegion,
            place(PlaceLayer::SubRegion, "Golden Horseshoe", 3),
        ),
        (PlaceLayer::District, place(PlaceLayer::District, "York", 4)),
        (
            PlaceLayer::Locality,
            place(PlaceLayer::Locality, "Toronto", 5),
        ),
        (
            PlaceLayer::SubLocality,
            place(PlaceLayer::SubLocality, "Old Toronto", 6),
        ),
        (
            PlaceLayer::Neighbourhood,
            place(PlaceLayer::Neighbourhood, "St. Lawrence", 7),
        ),
    ];
    writer.set_country_codes(
        [(
            tiers[0].1,
            crate::parse::lattice::CountryCode::parse("CA").expect("code"),
        )]
        .into_iter()
        .collect(),
    );
    writer.set_subdivision_codes([(tiers[1].1, "CA-ON".to_string())].into_iter().collect());
    writer.set_language_names(
        [(tiers[0].1, vec![("de".to_string(), "Kanada".to_string())])]
            .into_iter()
            .collect(),
    );
    let context = Some(RecordContext {
        admin_context: AdminContextTuple::from_tiers(tiers),
        flags: 0,
        zone: None,
    });
    let address = AddressComponents {
        areas: vec!["Old Town".to_string(), "Market".to_string()],
        number: "10".to_string(),
        street: Some("King Street".to_string()),
        place: None,
        unit: Some("4B".to_string()),
        locality: Some("City of Toronto".to_string()),
        region: Some("ON".to_string()),
        postcode: Some("M5V 1A1".to_string()),
        country: Some("Canada".to_string()),
    };
    writer
        .write(
            &AddressRecord {
                access: Vec::new(),
                disputed: false,
                street_postcodes: Vec::new(),
                address: address.clone(),
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
                    unit: None,
                    areas: Vec::new(),
                    locality: None,
                    region: None,
                    country: None,
                    ..address
                }),
                geometry: point_geometry(-79.005, 43.0),
                location_precision: LocationPrecision::Centroid,
                source: SourceProvenance::osm(ObjectType::Way, 11),
                prominence: Default::default(),
                wikidata: None,
                brand: None,
                details: None,
                aliases: Vec::new(),
            }
            .into(),
            context,
        )
        .expect("poi");
    writer.finish().expect("finish");
}

struct Reply {
    status: StatusCode,
    body: Value,
}

async fn send(router: &Router, request: Request<Body>) -> Reply {
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&bytes)));
    Reply { status, body }
}

async fn get(router: &Router, uri: &str) -> Reply {
    send(
        router,
        Request::get(uri).body(Body::empty()).expect("request"),
    )
    .await
}

async fn post(router: &Router, uri: &str, body: Value) -> Reply {
    post_with(router, uri, &[], body).await
}

async fn post_with(router: &Router, uri: &str, headers: &[(&str, &str)], body: Value) -> Reply {
    let mut request = Request::post(uri).header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    send(
        router,
        request.body(Body::from(body.to_string())).expect("request"),
    )
    .await
}

fn google_error(code: u16, status: &str, message: &str) -> Value {
    json!({"error": {"code": code, "message": message, "status": status}})
}

const SEARCH_TEXT: &str = "/compat/google/v1/places:searchText";
const AUTOCOMPLETE: &str = "/compat/google/v1/places:autocomplete";

fn bakery_id() -> String {
    encode_id("osm:way:11")
}

fn address_id() -> String {
    encode_id("osm:node:10")
}

fn bakery_components() -> Value {
    json!([
        {"longText": "12", "shortText": "12", "types": ["street_number"]},
        {"longText": "King Street", "shortText": "King Street", "types": ["route"]},
        {"longText": "St. Lawrence", "shortText": "St. Lawrence", "types": ["neighborhood", "political"]},
        {"longText": "Old Toronto", "shortText": "Old Toronto", "types": ["sublocality", "political"]},
        {"longText": "Toronto", "shortText": "Toronto", "types": ["locality", "political"]},
        {"longText": "York", "shortText": "York", "types": ["administrative_area_level_3", "political"]},
        {"longText": "Golden Horseshoe", "shortText": "Golden Horseshoe", "types": ["administrative_area_level_2", "political"]},
        {"longText": "Ontario", "shortText": "ON", "types": ["administrative_area_level_1", "political"]},
        {"longText": "Canada", "shortText": "CA", "types": ["country", "political"]},
        {"longText": "M5V 1A1", "shortText": "M5V 1A1", "types": ["postal_code"]},
    ])
}

fn bakery_place() -> Value {
    let id = bakery_id();
    json!({
        "name": format!("places/{id}"),
        "id": id,
        "displayName": {"text": "Queen Bakery"},
        "types": ["bakery", "point_of_interest", "establishment"],
        "primaryType": "bakery",
        "formattedAddress": "12 King Street, Toronto, M5V 1A1",
        "postalAddress": {
            "regionCode": "CA",
            "postalCode": "M5V 1A1",
            "administrativeArea": "Ontario",
            "locality": "Toronto",
            "sublocality": "Old Toronto",
            "addressLines": ["12 King Street"],
        },
        "addressComponents": bakery_components(),
        "location": {"latitude": 43.0, "longitude": -79.005},
    })
}

#[tokio::test]
async fn search_text_answers_every_field_without_a_mask() {
    let fixture = fixture();
    let reply = post(
        &fixture.router,
        SEARCH_TEXT,
        json!({"textQuery": "Queen Bakery"}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, json!({"places": [bakery_place()]}));
}

#[tokio::test]
async fn search_text_reads_the_mask_from_header_then_dollar_fields_then_fields() {
    let fixture = fixture();
    let router = &fixture.router;
    let body = json!({"textQuery": "Queen Bakery"});
    let id = bakery_id();

    let header = post_with(
        router,
        &format!("{SEARCH_TEXT}?$fields=places.name&fields=places.location"),
        &[("X-Goog-FieldMask", "places.id,places.displayName.text")],
        body.clone(),
    )
    .await;
    assert_eq!(
        header.body,
        json!({"places": [{"id": id, "displayName": {"text": "Queen Bakery"}}]})
    );

    let dollar = post(
        router,
        &format!("{SEARCH_TEXT}?$fields=places.name&fields=places.location"),
        body.clone(),
    )
    .await;
    assert_eq!(
        dollar.body,
        json!({"places": [{"name": format!("places/{id}")}]})
    );

    let fields = post(
        router,
        &format!("{SEARCH_TEXT}?fields=places.location"),
        body.clone(),
    )
    .await;
    assert_eq!(
        fields.body,
        json!({"places": [{"location": {"latitude": 43.0, "longitude": -79.005}}]})
    );

    let star = post_with(router, SEARCH_TEXT, &[("X-Goog-FieldMask", "*")], body).await;
    assert_eq!(star.body, json!({"places": [bakery_place()]}));
}

#[tokio::test]
async fn search_text_maps_page_size_and_restriction() {
    let fixture = fixture();
    let router = &fixture.router;
    let mask = [("X-Goog-FieldMask", "places.id")];

    // proto3 JSON sends an integer as a string too; 0 is Google's default
    // and more than 20 is cut to 20, both of which og accepts.
    for size in [json!("1"), json!(0), json!(21), json!(20)] {
        let reply = post_with(
            router,
            SEARCH_TEXT,
            &mask,
            json!({"textQuery": "Queen Bakery", "pageSize": size}),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK, "{size}");
        assert_eq!(
            reply.body,
            json!({"places": [{"id": bakery_id()}]}),
            "{size}"
        );
    }
    let negative = post(
        router,
        SEARCH_TEXT,
        json!({"textQuery": "Queen Bakery", "maxResultCount": -1}),
    )
    .await;
    assert_eq!(negative.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        negative.body,
        google_error(
            400,
            "INVALID_ARGUMENT",
            "pageSize must not be negative, got -1"
        )
    );

    let inside = json!({"rectangle": {
        "low": {"latitude": 42.9, "longitude": -79.1},
        "high": {"latitude": 43.1, "longitude": -78.9},
    }});
    let outside = json!({"rectangle": {
        "low": {"latitude": 10, "longitude": 10},
        "high": {"latitude": 11, "longitude": 11},
    }});
    let found = post_with(
        router,
        SEARCH_TEXT,
        &mask,
        json!({"textQuery": "Queen Bakery", "locationRestriction": inside}),
    )
    .await;
    assert_eq!(found.body, json!({"places": [{"id": bakery_id()}]}));
    let none = post_with(
        router,
        SEARCH_TEXT,
        &mask,
        json!({"textQuery": "Queen Bakery", "locationRestriction": outside}),
    )
    .await;
    assert_eq!(none.status, StatusCode::OK);
    assert_eq!(none.body, json!({}));

    let biased = post_with(
        router,
        SEARCH_TEXT,
        &mask,
        json!({"textQuery": "Queen Bakery", "locationBias": {"circle": {
            "center": {"latitude": "43.0", "longitude": -79.0}, "radius": 500.0,
        }}}),
    )
    .await;
    assert_eq!(biased.body, json!({"places": [{"id": bakery_id()}]}));

    let crossing = post(
        router,
        SEARCH_TEXT,
        json!({"textQuery": "Queen Bakery", "locationRestriction": {"rectangle": {
            "low": {"latitude": -10, "longitude": 170},
            "high": {"latitude": 10, "longitude": -170},
        }}}),
    )
    .await;
    assert_eq!(
        crossing.body,
        google_error(
            400,
            "INVALID_ARGUMENT",
            "locationRestriction.rectangle crosses the antimeridian, which this server cannot restrict to"
        )
    );
    let both = post(
        router,
        SEARCH_TEXT,
        json!({"textQuery": "Queen Bakery", "locationRestriction": inside, "locationBias": inside}),
    )
    .await;
    assert_eq!(
        both.body,
        google_error(
            400,
            "INVALID_ARGUMENT",
            "locationBias and locationRestriction cannot both be set"
        )
    );
}

#[tokio::test]
async fn search_text_refuses_a_missing_query_and_a_bad_body() {
    let fixture = fixture();
    let router = &fixture.router;
    for body in [json!({}), json!({"textQuery": "  "})] {
        let reply = post(router, SEARCH_TEXT, body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            reply.body,
            google_error(400, "INVALID_ARGUMENT", "textQuery is required")
        );
    }
    let reply = send(
        router,
        Request::post(SEARCH_TEXT)
            .body(Body::from("{"))
            .expect("request"),
    )
    .await;
    assert_eq!(
        reply.body,
        google_error(
            400,
            "INVALID_ARGUMENT",
            "Invalid JSON payload received. EOF while parsing an object at line 1 column 1"
        )
    );
    // og's own refusal keeps its detail, in Google's envelope.
    let long = post(router, SEARCH_TEXT, json!({"textQuery": "a".repeat(257)})).await;
    assert_eq!(
        long.body,
        google_error(400, "INVALID_ARGUMENT", "q must be at most 256 characters")
    );
}

#[tokio::test]
async fn autocomplete_suggests_places_with_matches_and_distance() {
    let fixture = fixture();
    let router = &fixture.router;
    let id = bakery_id();
    let reply = post(
        router,
        AUTOCOMPLETE,
        json!({"input": "queen bak", "origin": {"latitude": 43.0, "longitude": -79.0}}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.body,
        json!({"suggestions": [{"placePrediction": {
            "place": format!("places/{id}"),
            "placeId": id,
            "text": {
                "text": "Queen Bakery, 12 King Street, Toronto, M5V 1A1",
                "matches": [{"endOffset": 9}],
            },
            "structuredFormat": {
                "mainText": {"text": "Queen Bakery", "matches": [{"endOffset": 9}]},
                "secondaryText": {"text": "12 King Street, Toronto, M5V 1A1"},
            },
            "types": ["bakery", "point_of_interest", "establishment"],
            "distanceMeters": 407,
        }}]})
    );

    let masked = post_with(
        router,
        AUTOCOMPLETE,
        &[("X-Goog-FieldMask", "suggestions.placePrediction.placeId")],
        json!({"input": "Queen Bakery"}),
    )
    .await;
    assert_eq!(
        masked.body,
        json!({"suggestions": [{"placePrediction": {"placeId": id}}]})
    );
}

#[tokio::test]
async fn autocomplete_maps_region_codes_restriction_and_cursor() {
    let fixture = fixture();
    let router = &fixture.router;
    let mask = [("X-Goog-FieldMask", "suggestions.placePrediction.placeId")];
    let found = json!({"suggestions": [{"placePrediction": {"placeId": bakery_id()}}]});
    let ask = |body: Value| post_with(router, AUTOCOMPLETE, &mask, body);

    assert_eq!(
        ask(json!({"input": "Queen Bakery", "includedRegionCodes": ["ca"]}))
            .await
            .body,
        found
    );
    assert_eq!(
        ask(json!({"input": "Queen Bakery", "includedRegionCodes": ["FR"]}))
            .await
            .body,
        json!({})
    );

    // The bakery is 407 m from this centre, so inside a 500 m circle and outside a
    // 300 m one although inside the box around it.
    let circle = |radius: f64| {
        json!({"input": "Queen Bakery", "locationRestriction": {"circle": {
            "center": {"latitude": 43.0, "longitude": -79.0}, "radius": radius,
        }}})
    };
    assert_eq!(ask(circle(500.0)).await.body, found);
    assert_eq!(ask(circle(300.0)).await.body, json!({}));
    assert_eq!(ask(circle(0.0)).await.body, json!({}));

    assert_eq!(
        ask(json!({"input": "Queen Bakery", "inputOffset": 5}))
            .await
            .body,
        found
    );
    let missing = post(router, AUTOCOMPLETE, json!({"input": ""})).await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        missing.body,
        google_error(400, "INVALID_ARGUMENT", "input is required")
    );
    let bad_code = post(
        router,
        AUTOCOMPLETE,
        json!({"input": "Queen", "includedRegionCodes": ["Canada"]}),
    )
    .await;
    assert_eq!(
        bad_code.body,
        google_error(
            400,
            "INVALID_ARGUMENT",
            "country \"Canada\" is not an ISO 3166-1 alpha-2 code, such as CA or NZ"
        )
    );
}

#[tokio::test]
async fn place_details_by_id_and_unknown_ids() {
    let fixture = fixture();
    let router = &fixture.router;
    let id = bakery_id();
    let reply = get(router, &format!("/compat/google/v1/places/{id}")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, bakery_place());

    let masked = get(
        router,
        &format!("/compat/google/v1/places/{id}?fields=id,displayName"),
    )
    .await;
    assert_eq!(
        masked.body,
        json!({"id": id, "displayName": {"text": "Queen Bakery"}})
    );

    let never_issued = encode_id("osm:way:999");
    for unknown in [never_issued.as_str(), "not-an-og-id", "b3NtOjE"] {
        let reply = get(router, &format!("/compat/google/v1/places/{unknown}")).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{unknown}");
        assert_eq!(
            reply.body,
            google_error(
                404,
                "NOT_FOUND",
                &format!("Place 'places/{unknown}' was not found.")
            )
        );
    }
}

fn address_result() -> Value {
    let id = address_id();
    json!({
        "place": format!("//places.googleapis.com/places/{id}"),
        "placeId": id,
        "location": {"latitude": 43.0, "longitude": -79.0},
        "granularity": "ROOFTOP",
        "formattedAddress": "10 King Street, Unit 4B, City of Toronto, ON, M5V 1A1, Canada",
        "postalAddress": {
            "regionCode": "CA",
            "postalCode": "M5V 1A1",
            "administrativeArea": "ON",
            "locality": "City of Toronto",
            "sublocality": "Old Town, Market",
            "addressLines": ["10 King Street", "4B"],
        },
        "addressComponents": [
            {"longText": "4B", "shortText": "4B", "types": ["subpremise"]},
            {"longText": "10", "shortText": "10", "types": ["street_number"]},
            {"longText": "King Street", "shortText": "King Street", "types": ["route"]},
            {"longText": "St. Lawrence", "shortText": "St. Lawrence", "types": ["neighborhood", "political"]},
            {"longText": "Old Toronto", "shortText": "Old Toronto", "types": ["sublocality", "political"]},
            {"longText": "Toronto", "shortText": "Toronto", "types": ["locality", "political"]},
            {"longText": "York", "shortText": "York", "types": ["administrative_area_level_3", "political"]},
            {"longText": "Golden Horseshoe", "shortText": "Golden Horseshoe", "types": ["administrative_area_level_2", "political"]},
            {"longText": "Ontario", "shortText": "ON", "types": ["administrative_area_level_1", "political"]},
            {"longText": "Canada", "shortText": "CA", "types": ["country", "political"]},
            {"longText": "M5V 1A1", "shortText": "M5V 1A1", "types": ["postal_code"]},
        ],
        "types": ["street_address"],
    })
}

#[tokio::test]
async fn geocode_address_in_the_path_and_as_fields() {
    let fixture = fixture();
    let router = &fixture.router;
    let one = json!({"results": [address_result()]});

    // `+` is a space in Google's path; `%2C` a comma.
    let path = get(
        router,
        "/compat/google/v4/geocode/address/10+King+Street%2C+Toronto",
    )
    .await;
    assert_eq!(path.status, StatusCode::OK);
    assert_eq!(path.body, one);

    let query = get(
        router,
        "/compat/google/v4/geocode/address?addressQuery=10%20King%20Street&fields=results.placeId",
    )
    .await;
    assert_eq!(query.body, json!({"results": [{"placeId": address_id()}]}));

    let fields = get(
        router,
        "/compat/google/v4/geocode/address?address.addressLines=10%20King%20Street\
         &address.postalCode=M5V%201A1&address.regionCode=CA",
    )
    .await;
    assert_eq!(fields.body, one);
    let elsewhere = get(
        router,
        "/compat/google/v4/geocode/address?address.addressLines=10%20King%20Street\
         &address.postalCode=V6B%201A1",
    )
    .await;
    assert_eq!(elsewhere.status, StatusCode::OK);
    assert_eq!(elsewhere.body, json!({}));

    let biased = get(
        router,
        "/compat/google/v4/geocode/address/10%20King%20Street?$fields=results.placeId\
         &locationBias.rectangle.low.latitude=42.9&locationBias.rectangle.low.longitude=-79.1\
         &locationBias.rectangle.high.latitude=43.1&locationBias.rectangle.high.longitude=-78.9",
    )
    .await;
    assert_eq!(biased.body, json!({"results": [{"placeId": address_id()}]}));

    let both = get(
        router,
        "/compat/google/v4/geocode/address/10%20King?address.locality=Toronto",
    )
    .await;
    assert_eq!(
        both.body,
        google_error(
            400,
            "INVALID_ARGUMENT",
            "Give the address as addressQuery or as address fields, not both"
        )
    );
    let neither = get(router, "/compat/google/v4/geocode/address").await;
    assert_eq!(
        neither.body,
        google_error(
            400,
            "INVALID_ARGUMENT",
            "addressQuery or address is required"
        )
    );
}

#[tokio::test]
async fn reverse_geocode_by_path_and_by_fields_with_filters() {
    let fixture = fixture();
    let router = &fixture.router;
    let mask = "fields=results.placeId,results.types,results.granularity";
    let path = get(
        router,
        &format!("/compat/google/v4/geocode/location/43.0,-79.0?{mask}"),
    )
    .await;
    assert_eq!(path.status, StatusCode::OK);
    let results = path.body["results"].as_array().expect("results");
    assert_eq!(
        results[0],
        json!({"placeId": address_id(), "granularity": "ROOFTOP", "types": ["street_address"]})
    );
    let fields = get(
        router,
        &format!(
            "/compat/google/v4/geocode/location?location.latitude=43.0&location.longitude=-79.0&{mask}"
        ),
    )
    .await;
    assert_eq!(fields.body, path.body);

    let localities = get(
        router,
        &format!("/compat/google/v4/geocode/location/43.0,-79.0?types=locality&{mask}"),
    )
    .await;
    assert_eq!(
        localities.body,
        json!({"results": [{
            "placeId": encode_id("osm:relation:6"),
            "granularity": "GEOMETRIC_CENTER",
            "types": ["locality", "political"],
        }]})
    );
    let rooftop = get(
        router,
        &format!("/compat/google/v4/geocode/location/43.0,-79.0?granularity=ROOFTOP&{mask}"),
    )
    .await;
    assert_eq!(rooftop.body, json!({"results": [results[0]]}));

    let nothing = get(router, "/compat/google/v4/geocode/location/10,10").await;
    assert_eq!(nothing.status, StatusCode::OK);
    assert_eq!(nothing.body, json!({}));

    for (uri, message) in [
        (
            "/compat/google/v4/geocode/location/43.0",
            "Invalid locationQuery \"43.0\": expected \"latitude,longitude\"",
        ),
        (
            "/compat/google/v4/geocode/location/91,0",
            "location.latitude must be within [-90, 90], got 91",
        ),
        (
            "/compat/google/v4/geocode/location?location.latitude=north",
            "Invalid value at 'location.latitude' (TYPE_DOUBLE), \"north\"",
        ),
        (
            "/compat/google/v4/geocode/location",
            "locationQuery or location is required",
        ),
    ] {
        let reply = get(router, uri).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(
            reply.body,
            google_error(400, "INVALID_ARGUMENT", message),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn place_geocoding_answers_one_bare_result() {
    let fixture = fixture();
    let router = &fixture.router;
    let id = bakery_id();
    let reply = get(router, &format!("/compat/google/v4/geocode/places/{id}")).await;
    assert_eq!(reply.status, StatusCode::OK);
    let mut expected = bakery_place();
    let fields = expected.as_object_mut().expect("a place");
    for field in ["name", "id", "displayName", "primaryType"] {
        fields.remove(field);
    }
    fields.insert(
        "place".to_string(),
        json!(format!("//places.googleapis.com/places/{id}")),
    );
    fields.insert("placeId".to_string(), json!(id));
    fields.insert("granularity".to_string(), json!("GEOMETRIC_CENTER"));
    assert_eq!(reply.body, expected);

    let unknown = encode_id("osm:node:404");
    let missing = get(
        router,
        &format!("/compat/google/v4/geocode/places/{unknown}"),
    )
    .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(
        missing.body,
        google_error(
            404,
            "NOT_FOUND",
            &format!("Place 'places/{unknown}' was not found.")
        )
    );
}

const SEARCH_NEARBY: &str = "/compat/google/v1/places:searchNearby";

fn around(radius: f64) -> Value {
    json!({"circle": {"center": {"latitude": 43.0, "longitude": -79.0}, "radius": radius}})
}

#[tokio::test]
async fn search_nearby_answers_the_places_inside_the_circle() {
    let fixture = fixture();
    let found = json!({"places": [bakery_place()]});
    for request in [
        json!({"locationRestriction": around(1000.0)}),
        json!({"locationRestriction": around(1000.0), "includedTypes": ["bakery"]}),
        json!({"locationRestriction": around(1000.0), "includedTypes": ["cafe", "establishment"]}),
        json!({
            "location_restriction": around(1000.0),
            "included_primary_types": ["bakery", "cafe"],
            "excluded_types": ["cafe"],
            "max_result_count": "1",
            "rank_preference": "DISTANCE",
        }),
        json!({
            "locationRestriction": around(1000.0),
            "includedTypes": ["point_of_interest"],
            "includedPrimaryTypes": ["bakery"],
        }),
    ] {
        let reply = post(&fixture.router, SEARCH_NEARBY, request.clone()).await;
        assert_eq!(reply.status, StatusCode::OK, "{request}");
        assert_eq!(reply.body, found, "{request}");
    }
    // Outside the circle, another type, a type og has no category for, and
    // the place's own type excluded.
    for request in [
        json!({"locationRestriction": around(100.0)}),
        json!({"locationRestriction": around(1000.0), "includedTypes": ["cafe"]}),
        json!({"locationRestriction": around(1000.0), "includedTypes": ["chinese_restaurant"]}),
        json!({"locationRestriction": around(1000.0), "excludedTypes": ["bakery"]}),
        json!({"locationRestriction": around(1000.0), "excludedPrimaryTypes": ["bakery"]}),
        json!({
            "locationRestriction": around(1000.0),
            "includedTypes": ["bakery"],
            "includedPrimaryTypes": ["cafe"],
        }),
    ] {
        let reply = post(&fixture.router, SEARCH_NEARBY, request.clone()).await;
        assert_eq!(reply.status, StatusCode::OK, "{request}");
        assert_eq!(reply.body, json!({}), "{request}");
    }

    let reply = post_with(
        &fixture.router,
        SEARCH_NEARBY,
        &[("x-goog-fieldmask", "places.displayName")],
        json!({"locationRestriction": around(1000.0), "languageCode": "en"}),
    )
    .await;
    assert_eq!(
        reply.body,
        json!({"places": [{"displayName": {"text": "Queen Bakery"}}]})
    );
}

#[tokio::test]
async fn search_nearby_refuses_what_google_refuses() {
    let fixture = fixture();
    for (request, message) in [
        (json!({}), "locationRestriction.circle is required"),
        (
            json!({"locationRestriction": {"rectangle": {}}}),
            "locationRestriction takes only a circle in Nearby Search",
        ),
        (
            json!({"locationRestriction": around(0.0)}),
            "locationRestriction.circle.radius must be within (0, 50000], got 0",
        ),
        (
            json!({"locationRestriction": around(50001.0)}),
            "locationRestriction.circle.radius must be within (0, 50000], got 50001",
        ),
        (
            json!({"locationRestriction": around(10.0), "maxResultCount": 21}),
            "maxResultCount must be within [1, 20], got 21",
        ),
        (
            json!({"locationRestriction": around(10.0), "maxResultCount": -1}),
            "maxResultCount must be within [1, 20], got -1",
        ),
        (
            json!({
                "locationRestriction": around(10.0),
                "includedTypes": ["cafe"],
                "excludedTypes": ["cafe"],
            }),
            "cafe is in both includedTypes and excludedTypes",
        ),
        (
            json!({"locationRestriction": around(10.0), "rankPreference": "NEAREST"}),
            "Invalid value at 'rank_preference' (type.googleapis.com/google.maps.places.v1.SearchNearbyRequest.RankPreference), \"NEAREST\"",
        ),
    ] {
        let reply = post(&fixture.router, SEARCH_NEARBY, request.clone()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{request}");
        assert_eq!(
            reply.body,
            google_error(400, "INVALID_ARGUMENT", message),
            "{request}"
        );
    }
}

#[tokio::test]
async fn unserved_methods_unknown_paths_and_wrong_methods() {
    let fixture = fixture();
    let router = &fixture.router;
    for (method, uri, name) in [
        (
            "GET",
            "/compat/google/v1/places/abc/photos/def/media",
            "places.photos.getMedia",
        ),
        (
            "POST",
            "/compat/google/v4/geocode/destinations",
            "searchDestinations",
        ),
    ] {
        let reply = send(
            router,
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::from("{}"))
                .expect("request"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED, "{uri}");
        assert_eq!(
            reply.body,
            google_error(
                501,
                "UNIMPLEMENTED",
                &format!("{name} is not implemented by this server.")
            )
        );
    }
    let not_found = google_error(404, "NOT_FOUND", "Method not found.");
    for (method, uri) in [
        ("GET", "/compat/google/v1/places:searchText"),
        ("POST", "/compat/google/v1/places/abc"),
        ("DELETE", "/compat/google/v4/geocode/location/1,1"),
        ("GET", "/compat/google/v1/places:aggregate"),
        ("GET", "/compat/google/v3/geocode/json"),
        ("GET", "/compat/google/v4beta/geocode/address/x"),
        ("GET", "/compat/google"),
    ] {
        let reply = send(
            router,
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method} {uri}");
        assert_eq!(reply.body, not_found, "{method} {uri}");
    }
}

/// Every og value of a result lands in its Google slot unchanged, through
/// every response shape that carries the result. og looks up only places
/// by id, so the address is reached by search.
#[tokio::test]
async fn every_og_value_lands_in_its_google_slot() {
    let fixture = fixture();
    let router = &fixture.router;
    let bakery = bakery_id();
    let cases = [
        (
            "/geocode?q=10%20King%20Street",
            None,
            "/compat/google/v4/geocode/address/10%20King%20Street".to_string(),
        ),
        (
            "/places?ids=osm:way:11",
            Some(format!("/compat/google/v1/places/{bakery}")),
            format!("/compat/google/v4/geocode/places/{bakery}"),
        ),
    ];
    for (native, place_uri, result_uri) in cases {
        let native = get(router, native).await.body;
        let feature = &native["features"][0];
        let properties = &feature["properties"];
        let og_id = properties["id"].as_str().expect("id");
        let id = encode_id(og_id);
        let name = properties["name"].as_str().expect("name");
        let label = properties["label"].as_str().expect("label");
        let coordinates = &feature["geometry"]["coordinates"];
        let reply = get(router, &result_uri).await.body;
        let result = reply.get("results").map_or(&reply, |results| &results[0]);
        let place = match &place_uri {
            Some(uri) => Some(get(router, uri).await.body),
            None => None,
        };
        let suggested = post(router, AUTOCOMPLETE, json!({"input": name}))
            .await
            .body;
        let prediction = &suggested["suggestions"][0]["placePrediction"];

        assert_eq!(result["placeId"], id, "{og_id}");
        assert_eq!(
            result["place"],
            format!("//places.googleapis.com/places/{id}")
        );
        assert_eq!(prediction["placeId"], id);
        assert_eq!(prediction["place"], format!("places/{id}"));
        assert_eq!(prediction["text"]["text"], label);
        assert_eq!(prediction["structuredFormat"]["mainText"]["text"], name);
        let rest = label
            .strip_prefix(&format!("{name}, "))
            .expect("a label starts with its name");
        assert_eq!(
            prediction["structuredFormat"]["secondaryText"]["text"],
            rest
        );
        let formatted = if properties["layer"] == "poi" {
            rest
        } else {
            label
        };
        assert_eq!(result["formattedAddress"], formatted);
        assert_eq!(result["location"]["longitude"], coordinates[0]);
        assert_eq!(result["location"]["latitude"], coordinates[1]);
        let granularity = match properties["accuracy"].as_str().expect("accuracy") {
            "rooftop" => "ROOFTOP",
            "centroid" => "GEOMETRIC_CENTER",
            other => panic!("the fixture has no {other} result"),
        };
        assert_eq!(result["granularity"], granularity);
        if let Some(place) = &place {
            assert_eq!(place["id"], id);
            assert_eq!(place["name"], format!("places/{id}"));
            assert_eq!(place["displayName"]["text"], name);
            assert_eq!(place["formattedAddress"], formatted);
            assert_eq!(place["location"], result["location"]);
            assert_eq!(place["addressComponents"], result["addressComponents"]);
            assert_eq!(place["postalAddress"], result["postalAddress"]);
        }

        let components = result["addressComponents"].as_array().expect("components");
        let component = |kind: &str| {
            components
                .iter()
                .find(|component| component["types"][0] == kind)
                .cloned()
                .unwrap_or(Value::Null)
        };
        let address = &properties["address"];
        let context = &properties["context"];
        let postal = &result["postalAddress"];
        assert_eq!(component("subpremise")["longText"], address["unit"]);
        assert_eq!(
            component("street_number")["longText"],
            address["housenumber"]
        );
        assert_eq!(component("route")["longText"], address["street"]);
        assert_eq!(component("postal_code")["longText"], address["postcode"]);
        assert_eq!(
            component("neighborhood")["longText"],
            context["neighbourhood"]["name"]
        );
        assert_eq!(
            component("sublocality")["longText"],
            context["sublocality"]["name"]
        );
        assert_eq!(
            component("locality")["longText"],
            context["locality"]["name"]
        );
        assert_eq!(
            component("administrative_area_level_2")["longText"],
            context["subregion"]["name"]
        );
        assert_eq!(
            component("administrative_area_level_3")["longText"],
            context["district"]["name"]
        );
        assert_eq!(
            component("administrative_area_level_1")["longText"],
            context["region"]["name"]
        );
        assert_eq!(component("country")["longText"], context["country"]["name"]);
        assert_eq!(
            component("country")["shortText"],
            context["country"]["code"]
        );

        assert_eq!(postal["regionCode"], context["country"]["code"]);
        assert_eq!(postal["postalCode"], address["postcode"]);
        let line = format!(
            "{} {}",
            address["housenumber"].as_str().expect("number"),
            address["street"].as_str().expect("street")
        );
        assert_eq!(postal["addressLines"][0], line);
        assert_eq!(
            postal["addressLines"].get(1).unwrap_or(&Value::Null),
            &address["unit"]
        );
        let stated_or = |part: &str, area: &str| match address.get(part) {
            Some(stated) => stated.clone(),
            None => context[area]["name"].clone(),
        };
        assert_eq!(postal["locality"], stated_or("locality", "locality"));
        assert_eq!(postal["administrativeArea"], stated_or("region", "region"));
        let areas = address["areas"].as_array().map(|areas| {
            areas
                .iter()
                .map(|area| area.as_str().expect("area"))
                .collect::<Vec<_>>()
                .join(", ")
        });
        assert_eq!(
            postal["sublocality"],
            areas.map_or_else(|| context["sublocality"]["name"].clone(), Value::from)
        );
    }
}

/// `languageCode` names areas in that language where og has the name, and
/// says so. An area og names only locally carries no `languageCode`.
#[tokio::test]
async fn language_code_names_areas_og_holds_in_that_language() {
    let fixture = fixture();
    let router = &fixture.router;
    let country = |reply: &Reply, at: &str| {
        reply
            .body
            .pointer(at)
            .and_then(Value::as_array)
            .and_then(|components| {
                components
                    .iter()
                    .find(|component| component["types"][0] == "country")
                    .cloned()
            })
            .expect("a country component")
    };

    let local = get(
        router,
        "/compat/google/v4/geocode/address?addressQuery=10%20King%20Street",
    )
    .await;
    assert_eq!(
        country(&local, "/results/0/addressComponents"),
        json!({"longText": "Canada", "shortText": "CA", "types": ["country", "political"]})
    );

    let german = get(
        router,
        "/compat/google/v4/geocode/address?addressQuery=10%20King%20Street&languageCode=de",
    )
    .await;
    assert_eq!(german.status, StatusCode::OK);
    assert_eq!(
        country(&german, "/results/0/addressComponents"),
        json!({
            "longText": "Kanada",
            "shortText": "CA",
            "types": ["country", "political"],
            "languageCode": "de",
        })
    );
    let locality = german.body["results"][0]["addressComponents"]
        .as_array()
        .and_then(|components| {
            components
                .iter()
                .find(|component| component["types"][0] == "locality")
        })
        .expect("a locality");
    assert_eq!(locality["longText"], "Toronto");
    assert!(locality.get("languageCode").is_none());

    let details = get(
        router,
        &format!("/compat/google/v1/places/{}?languageCode=de", bakery_id()),
    )
    .await;
    assert_eq!(
        country(&details, "/addressComponents")["longText"],
        "Kanada"
    );
    assert!(details.body["displayName"].get("languageCode").is_none());

    let searched = post(
        router,
        SEARCH_TEXT,
        json!({"textQuery": "Queen Bakery", "languageCode": "de"}),
    )
    .await;
    assert_eq!(
        country(&searched, "/places/0/addressComponents")["languageCode"],
        "de"
    );
    // Places answers in English when no language is named, and og holds no
    // English name here.
    let english = post(router, SEARCH_TEXT, json!({"textQuery": "Queen Bakery"})).await;
    assert_eq!(
        country(&english, "/places/0/addressComponents")["longText"],
        "Canada"
    );

    // A region og holds no name for reads its base language, and says the
    // language it read.
    let swiss = get(
        router,
        "/compat/google/v4/geocode/address?addressQuery=10%20King%20Street&languageCode=de-CH",
    )
    .await;
    assert_eq!(
        country(&swiss, "/results/0/addressComponents"),
        json!({
            "longText": "Kanada",
            "shortText": "CA",
            "types": ["country", "political"],
            "languageCode": "de",
        })
    );

    // Autocomplete reads the browser's language when the body names none, and
    // a header that names no language is no request to refuse.
    for accept in ["de-CH,de;q=0.9", "*", "not a language"] {
        let suggested = post_with(
            router,
            AUTOCOMPLETE,
            &[("Accept-Language", accept)],
            json!({"input": "Queen Bakery"}),
        )
        .await;
        assert_eq!(suggested.status, StatusCode::OK, "{accept}");
    }

    // `languageCode` is one code.
    let details = format!("/compat/google/v1/places/{}?languageCode=d3", bakery_id());
    for (code, uri) in [
        (
            "german",
            "/compat/google/v4/geocode/address?addressQuery=10%20King&languageCode=german",
        ),
        (
            "fr,de",
            "/compat/google/v4/geocode/address?addressQuery=10%20King&languageCode=fr,de",
        ),
        (
            "fr,de",
            "/compat/google/v4/geocode/location/43.0,-79.0?languageCode=fr,de",
        ),
        ("d3", details.as_str()),
    ] {
        let refused = get(router, uri).await;
        assert_eq!(
            refused.body,
            google_error(
                400,
                "INVALID_ARGUMENT",
                &format!("languageCode {code:?} is not a language code, such as de or zh-Hant")
            ),
            "{uri}"
        );
    }
    for body in [
        json!({"textQuery": "Queen Bakery", "languageCode": "fr,de"}),
        json!({"input": "Queen Bakery", "languageCode": "fr,de"}),
    ] {
        let path = if body.get("textQuery").is_some() {
            SEARCH_TEXT
        } else {
            AUTOCOMPLETE
        };
        let refused = post(router, path, body).await;
        assert_eq!(
            refused.body,
            google_error(
                400,
                "INVALID_ARGUMENT",
                "languageCode \"fr,de\" is not a language code, such as de or zh-Hant"
            ),
            "{path}"
        );
    }
}
