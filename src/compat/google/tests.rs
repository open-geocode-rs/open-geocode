use std::{path::Path, sync::LazyLock};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::{
    compat::encode_parts,
    context::{AdminContextTuple, RecordContext},
    limits::Limits,
    pack::Writer,
    record::{
        AddressComponents, AddressRecord, Contact, Entrance, Layer, LocationPrecision, ObjectType,
        PlaceLayer, PlaceRecord, PoiDetails, PoiRecord, Record, SourceProvenance, point_geometry,
    },
    runtime::{AppState, build_router},
    test_support::TempDir,
};

static TORONTO: LazyLock<TempDir> = LazyLock::new(|| {
    let dir = TempDir::new("compat-google");
    write_pack(&dir.join("pack"));
    dir
});

static US: LazyLock<TempDir> = LazyLock::new(|| {
    let dir = TempDir::new("compat-google-us");
    write_us_pack(&dir.join("pack"));
    dir
});

struct Fixture {
    _jobs: TempDir,
    router: Router,
}

fn address_vendor_id(og_id: &str) -> String {
    let label = match og_id {
        "osm:node:10" => "10 King Street, Unit 4B, City of Toronto, ON, M5V 1A1, Canada",
        _ => panic!("{og_id} is no address of the fixture"),
    };
    encode_parts(og_id, Some(Layer::Address), label)
}

fn fixture_over(pack: &Path) -> Fixture {
    let jobs = TempDir::new("compat-google-jobs");
    let state = AppState::open(pack, &jobs, Limits::DEFAULT).expect("pack");
    Fixture {
        _jobs: jobs,
        router: build_router(state, None, None),
    }
}

fn fixture() -> Fixture {
    fixture_over(&TORONTO.join("pack"))
}

fn us_fixture() -> Fixture {
    fixture_over(&US.join("pack"))
}

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
        enclosing: Vec::new(),
    });
    let address = AddressComponents {
        block_number_len: None,
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
                building: None,
                access: vec![
                    Entrance {
                        at: [-790_001_000, 430_001_000],
                        kind: "main".to_string(),
                    },
                    Entrance {
                        at: [-790_002_000, 429_999_000],
                        kind: "service".to_string(),
                    },
                ],
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
                details: Some(Box::new(PoiDetails {
                    contact: Some(Contact {
                        fax: None,
                        phone: Some("+1 416-555-0111".to_string()),
                        website: Some("https://queenbakery.example/".to_string()),
                        email: Some("hello@queenbakery.example".to_string()),
                    }),
                    opening_hours: Some("Mo-Fr 07:00-18:00; Sa 08:00-14:00".to_string()),
                    extent: Some([-790_051_000, 429_995_000, -790_049_000, 430_005_000]),
                    access: vec![Entrance {
                        at: [-790_050_000, 429_995_000],
                        kind: "main".to_string(),
                    }],
                    // Google has a restaurant type for sandwiches, none for
                    // coffee.
                    cuisine: vec!["coffee_shop".to_string(), "french".to_string()],
                })),
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

async fn get_with(router: &Router, uri: &str, headers: &[(&str, &str)]) -> Reply {
    let mut request = Request::get(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    send(router, request.body(Body::empty()).expect("request")).await
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
    encode_parts("osm:way:11", Some(Layer::Poi), "")
}

fn address_id() -> String {
    address_vendor_id("osm:node:10")
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
        "attributions": [{"provider": "© OpenStreetMap contributors", "providerUri": "https://www.openstreetmap.org/copyright"}],
        "types": ["bakery", "french_restaurant", "food", "store", "point_of_interest", "establishment"],
        "primaryType": "bakery",
        "formattedAddress": "12 King Street, Toronto, ON M5V 1A1, Canada",
        "shortFormattedAddress": "12 King Street, Toronto, ON M5V 1A1",
        "postalAddress": {
            "regionCode": "CA",
            "postalCode": "M5V 1A1",
            "administrativeArea": "Ontario",
            "locality": "Toronto",
            "sublocality": "Old Toronto",
            "addressLines": ["12 King Street"],
        },
        "addressComponents": bakery_components(),
        "plusCode": {"globalCode": "87M22X2W+22", "compoundCode": "2X2W+22 Toronto, ON, CAN"},
        "location": {"latitude": 43.0, "longitude": -79.005},
        "viewport": {
            "low": {"latitude": 42.9995, "longitude": -79.0051},
            "high": {"latitude": 43.0005, "longitude": -79.0049},
        },
        "internationalPhoneNumber": "+1 416-555-0111",
        "websiteUri": "https://queenbakery.example/",
        "addressDescriptor": {"areas": [
            {
                "name": format!("places/{}", encode_parts("osm:relation:7", None, "St. Lawrence")),
                "placeId": encode_parts("osm:relation:7", None, "St. Lawrence"),
                "displayName": {"text": "St. Lawrence"},
                "containment": "WITHIN",
            },
            {
                "name": format!("places/{}", encode_parts("osm:relation:6", None, "Old Toronto")),
                "placeId": encode_parts("osm:relation:6", None, "Old Toronto"),
                "displayName": {"text": "Old Toronto"},
                "containment": "WITHIN",
            },
        ]},
        "regularOpeningHours": {"periods": bakery_hours()},
        "entrances": [{
            "location": {"latitude": 42.9995, "longitude": -79.005},
            "tags": ["PREFERRED"],
        }],
    })
}

fn bakery_hours() -> Value {
    let mut periods = (1..=5)
        .map(|day| json!({"open": {"day": day, "hour": 7, "minute": 0}, "close": {"day": day, "hour": 18, "minute": 0}}))
        .collect::<Vec<_>>();
    periods.push(json!({"open": {"day": 6, "hour": 8, "minute": 0}, "close": {"day": 6, "hour": 14, "minute": 0}}));
    Value::Array(periods)
}

#[tokio::test]
async fn search_text_answers_an_address_before_the_places_at_it() {
    let fixture = fixture();
    let reply = post_with(
        &fixture.router,
        SEARCH_TEXT,
        &[("X-Goog-FieldMask", "places.id,places.types")],
        json!({"textQuery": "10 King Street"}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.body["places"][0],
        json!({"id": address_id(), "types": ["street_address"]})
    );
    let reply = post_with(
        &fixture.router,
        SEARCH_TEXT,
        &[("X-Goog-FieldMask", "places.id")],
        json!({"textQuery": "Queen Bakery"}),
    )
    .await;
    assert_eq!(reply.body["places"][0], json!({"id": bakery_id()}));
}

#[tokio::test]
async fn search_text_answers_a_town_and_a_district_by_their_names() {
    let fixture = fixture();
    for (query, types) in [
        ("Toronto", json!(["locality", "political"])),
        ("York", json!(["political"])),
    ] {
        let reply = post_with(
            &fixture.router,
            SEARCH_TEXT,
            &[("X-Goog-FieldMask", "places.displayName,places.types")],
            json!({"textQuery": query}),
        )
        .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(
            reply.body["places"][0],
            json!({"displayName": {"text": query}, "types": types}),
            "{query}"
        );
    }
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

    let star = post_with(
        router,
        SEARCH_TEXT,
        &[("X-Goog-FieldMask", "*")],
        body.clone(),
    )
    .await;
    assert_eq!(star.body, json!({"places": [bakery_place()]}));

    let bare = post(router, SEARCH_TEXT, body).await;
    assert_eq!(bare.status, StatusCode::OK);
    assert_eq!(bare.body, json!({"places": [bakery_place()]}));
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
            "types": ["bakery", "french_restaurant", "food", "store", "point_of_interest", "establishment"],
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

    let never_issued = encode_parts("osm:way:999", Some(Layer::Poi), "");
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
        "place": format!("places/{id}"),
        "placeId": id,
        "plusCode": {"globalCode": "87M32222+22", "compoundCode": "2222+22 Toronto, ON, CAN"},
        "location": {"latitude": 43.0, "longitude": -79.0},
        "granularity": "ROOFTOP",
        "viewport": {
            "low": {"latitude": 43.0 - 0.00135, "longitude": -79.0 - 0.00135},
            "high": {"latitude": 43.0 + 0.00135, "longitude": -79.0 + 0.00135},
        },
        "formattedAddress": "Unit 4B, 10 King Street, City of Toronto, ON M5V 1A1, Canada",
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

    let incomplete_bias = get(
        router,
        "/compat/google/v4/geocode/address/10%20King?locationBias.rectangle.low.latitude=42.9",
    )
    .await;
    assert_eq!(incomplete_bias.status, StatusCode::BAD_REQUEST);

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
            "placeId": encode_parts("osm:relation:6", Some(Layer::Locality), ""),
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
    assert_eq!(
        nothing.body,
        json!({"plusCode": {"globalCode": "7F2G2222+22"}})
    );

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
    for field in [
        "name",
        "id",
        "shortFormattedAddress",
        "displayName",
        "primaryType",
        "internationalPhoneNumber",
        "websiteUri",
        "regularOpeningHours",
        "entrances",
        "addressDescriptor",
        "attributions",
    ] {
        fields.remove(field);
    }
    fields.insert("place".to_string(), json!(format!("places/{id}")));
    fields.insert("placeId".to_string(), json!(id));
    fields.insert("granularity".to_string(), json!("GEOMETRIC_CENTER"));
    assert_eq!(reply.body, expected);

    let unknown = encode_parts("osm:node:404", Some(Layer::Poi), "");
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
    let reply = post(
        &fixture.router,
        SEARCH_NEARBY,
        json!({"locationRestriction": around(1000.0), "includedTypes": ["chinese_restaurant"]}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, json!({}));
    for request in [
        json!({"locationRestriction": around(100.0)}),
        json!({"locationRestriction": around(1000.0), "includedTypes": ["cafe"]}),
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
async fn unknown_paths_and_wrong_methods() {
    let fixture = fixture();
    let router = &fixture.router;
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

#[tokio::test]
async fn every_og_value_lands_in_its_google_slot() {
    let fixture = fixture();
    let router = &fixture.router;
    let bakery = bakery_id();
    // og measures `distance_m` from `focus`, Google `distanceMeters` from
    // `origin`, so both are given the same point.
    let (focus, origin) = (
        "focus=-79.01,43.01",
        json!({"latitude": 43.01, "longitude": -79.01}),
    );
    let mut stated = std::collections::BTreeSet::new();
    let cases = [
        (
            "/geocode?q=10%20King%20Street,%20Toronto",
            Some(format!("/compat/google/v1/places/{}", address_id())),
            "/compat/google/v4/geocode/address/10%20King%20Street".to_string(),
            "10 king st",
        ),
        (
            "/places?ids=osm:way:11",
            Some(format!("/compat/google/v1/places/{bakery}")),
            format!("/compat/google/v4/geocode/places/{bakery}"),
            "queen bak",
        ),
    ];
    for (native, place_uri, result_uri, input) in cases {
        let native = get(router, native).await.body;
        let feature = &native["features"][0];
        let properties = &feature["properties"];
        for (field, _) in properties.as_object().expect("properties") {
            stated.insert(field.clone());
        }
        let og_id = properties["id"].as_str().expect("id");
        let name = properties["name"].as_str().expect("name");
        let label = properties["label"].as_str().expect("label");
        let id = encode_parts(
            og_id,
            Layer::parse(properties["layer"].as_str().expect("layer")),
            label,
        );
        let coordinates = &feature["geometry"]["coordinates"];
        let reply = get(router, &result_uri).await.body;
        let result = reply.get("results").map_or(&reply, |results| &results[0]);
        let place = match &place_uri {
            Some(uri) => Some(get(router, uri).await.body),
            None => None,
        };
        let suggested = post(
            router,
            AUTOCOMPLETE,
            json!({"input": input, "origin": origin}),
        )
        .await
        .body;
        let prediction = &suggested["suggestions"][0]["placePrediction"];
        let typed = get(
            router,
            &format!("/autocomplete?q={}&{focus}", input.replace(' ', "%20")),
        )
        .await
        .body;
        let typed = &typed["features"][0]["properties"];
        assert_eq!(typed["id"], og_id, "{input}");
        for (field, _) in typed.as_object().expect("properties") {
            stated.insert(field.clone());
        }
        let distance = typed["distance_m"].as_f64().expect("a distance");
        assert_eq!(
            prediction["distanceMeters"],
            json!(distance.round() as u64),
            "{og_id}"
        );
        let highlights = typed["highlights"].clone();
        assert!(
            highlights.as_array().is_some_and(|all| !all.is_empty()),
            "{input}"
        );
        let first = &highlights[0];
        let last = &highlights[highlights.as_array().map_or(0, Vec::len) - 1];
        assert_eq!(
            prediction["text"]["matches"][0]["endOffset"], last["end"],
            "{og_id}"
        );
        assert_eq!(
            prediction["text"]["matches"][0]
                .get("startOffset")
                .cloned()
                .unwrap_or_else(|| json!(0)),
            first["start"]
        );

        assert_eq!(result["placeId"], id, "{og_id}");
        assert_eq!(result["place"], format!("places/{id}"));
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
        let lines = properties["address_lines"]
            .as_array()
            .map(|lines| lines.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        let formatted =
            if lines.len() > 1 && matches!(properties["layer"].as_str(), Some("address" | "poi")) {
                lines.join(", ")
            } else if properties["layer"] == "poi" {
                rest.to_string()
            } else {
                label.to_string()
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
            let corner = |at: usize| match &properties["bbox"] {
                Value::Null => Value::Null,
                bbox => json!({"latitude": bbox[at + 1], "longitude": bbox[at]}),
            };
            assert_eq!(place["viewport"]["low"], corner(0), "{og_id}");
            assert_eq!(place["viewport"]["high"], corner(2), "{og_id}");
            if !properties["bbox"].is_null() {
                assert_eq!(result["viewport"], place["viewport"]);
            } else if properties["layer"] == "address" {
                for key in ["latitude", "longitude"] {
                    let location = result["location"][key].as_f64().expect("coordinate");
                    let low = result["viewport"]["low"][key].as_f64().expect("low corner");
                    let high = result["viewport"]["high"][key]
                        .as_f64()
                        .expect("high corner");
                    assert!(low < location && location < high, "{result}");
                }
            }
            let contact = &properties["contact"];
            assert_eq!(place["websiteUri"], contact["website"]);
            let phone = if place["internationalPhoneNumber"].is_null() {
                &place["nationalPhoneNumber"]
            } else {
                &place["internationalPhoneNumber"]
            };
            assert_eq!(phone, &contact["phone"]);
            let hours = properties["opening_hours"]
                .as_str()
                .and_then(|value| super::hours::parse(value, properties["timezone"].as_str()));
            assert_eq!(
                place["regularOpeningHours"],
                serde_json::to_value(hours).expect("hours"),
                "{og_id}"
            );
            assert_eq!(place["timeZone"]["id"], properties["timezone"]);
            let access = properties["access"].as_array().cloned().unwrap_or_default();
            let entrances = place["entrances"].as_array().cloned().unwrap_or_default();
            assert_eq!(entrances.len(), access.len(), "{og_id}");
            for (entrance, access) in entrances.iter().zip(&access) {
                assert_eq!(entrance["location"]["longitude"], access["lon"]);
                assert_eq!(entrance["location"]["latitude"], access["lat"]);
            }
        }

        let destination = match &place_uri {
            Some(_) => json!({"place": format!("places/{id}")}),
            None => json!({"addressQuery": {"addressQuery": name}}),
        };
        let destinations = post(router, DESTINATIONS, destination).await.body;
        let destination = &destinations["destinations"][0];
        let primary = &destination["primary"];
        assert_eq!(primary["placeId"], id);
        assert_eq!(primary["place"], format!("places/{id}"));
        assert_eq!(primary["displayName"]["text"], name);
        let expected = if properties["layer"] == "poi" {
            json!(format!("{name}, {}", formatted))
        } else {
            json!(formatted)
        };
        assert_eq!(primary["formattedAddress"], expected);
        assert_eq!(primary["location"], result["location"]);
        assert_eq!(primary["addressComponents"], result["addressComponents"]);
        assert_eq!(primary["postalAddress"], result["postalAddress"]);
        let access = properties["access"].as_array().cloned().unwrap_or_default();
        let entrances = destination["entrances"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(entrances.len(), access.len(), "{og_id}");
        for (entrance, access) in entrances.iter().zip(&access) {
            assert_eq!(entrance["location"]["longitude"], access["lon"]);
            assert_eq!(entrance["location"]["latitude"], access["lat"]);
            assert_eq!(entrance["place"], format!("places/{id}"));
            assert_eq!(
                entrance["tags"] == json!(["PREFERRED"]),
                access["kind"] == "main"
            );
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
        assert_eq!(
            component("administrative_area_level_1")["shortText"],
            json!(context["region"]["code"].as_str().map(|code| &code[3..]))
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
    for field in [
        "bbox",
        "contact",
        "opening_hours",
        "access",
        "distance_m",
        "highlights",
        "match",
        "address",
        "context",
    ] {
        assert!(stated.contains(field), "no case states {field}");
    }

    let ontario = get(router, "/compat/google/v4/geocode/address/Ontario")
        .await
        .body;
    assert_eq!(
        ontario["results"][0]["addressComponents"][0],
        json!({
            "longText": "Ontario",
            "shortText": "ON",
            "types": ["administrative_area_level_1", "political"],
        })
    );
}

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
    let english = post(router, SEARCH_TEXT, json!({"textQuery": "Queen Bakery"})).await;
    assert_eq!(
        country(&english, "/places/0/addressComponents")["longText"],
        "Canada"
    );

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

const PHOTO: &str = "photos/AbC123/media";

#[tokio::test]
async fn photo_media_checks_the_size_then_finds_no_photo_og_holds() {
    let fixture = fixture();
    let router = &fixture.router;
    let id = bakery_id();
    let photo =
        |place: &str, query: &str| format!("/compat/google/v1/places/{place}/{PHOTO}{query}");

    let reply = get(router, &photo(&id, "?maxWidthPx=400&skipHttpRedirect=true")).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.body,
        google_error(
            400,
            "INVALID_ARGUMENT",
            &format!("The photo resource 'places/{id}/photos/AbC123' is invalid.")
        )
    );

    let unknown = encode_parts("osm:way:999", Some(Layer::Poi), "");
    let reply = get(router, &photo(&unknown, "?maxHeightPx=400")).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        reply.body,
        google_error(
            404,
            "NOT_FOUND",
            &format!("Place 'places/{unknown}' was not found.")
        )
    );

    for (query, message) in [
        (
            "",
            "Neither maxWidthPx nor maxHeightPx is set, and at least one is required",
        ),
        (
            "?maxWidthPx=0",
            "maxWidthPx must be within [1, 4800], got 0",
        ),
        (
            "?maxWidthPx=10&maxHeightPx=4801",
            "maxHeightPx must be within [1, 4800], got 4801",
        ),
        (
            "?maxWidthPx=wide",
            "Invalid value at 'maxWidthPx' (TYPE_INT32), \"wide\"",
        ),
    ] {
        let reply = get(router, &photo(&unknown, query)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(
            reply.body,
            google_error(400, "INVALID_ARGUMENT", message),
            "{query}"
        );
    }
}

const DESTINATIONS: &str = "/compat/google/v4/geocode/destinations";

fn address_destination() -> Value {
    let id = address_id();
    let place = format!("places/{id}");
    let mut primary = address_result();
    let fields = primary.as_object_mut().expect("a result");
    for field in ["granularity", "types", "viewport", "plusCode"] {
        fields.remove(field);
    }
    fields.insert("place".to_string(), json!(place));
    fields.insert("displayName".to_string(), json!({"text": "10 King Street"}));
    fields.insert("primaryType".to_string(), json!("street_address"));
    fields.insert("types".to_string(), json!(["street_address"]));
    fields.insert("structureType".to_string(), json!("POINT"));
    json!({
        "primary": primary,
        "entrances": [
            {
                "location": {"latitude": 43.0001, "longitude": -79.0001},
                "tags": ["PREFERRED"],
                "place": place,
            },
            {
                "location": {"latitude": 42.9999, "longitude": -79.0002},
                "place": place,
            },
        ],
    })
}

fn bakery_destination() -> Value {
    let id = bakery_id();
    let mut primary = bakery_place();
    let fields = primary.as_object_mut().expect("a place");
    for field in [
        "name",
        "id",
        "shortFormattedAddress",
        "plusCode",
        "viewport",
        "internationalPhoneNumber",
        "websiteUri",
        "regularOpeningHours",
        "entrances",
        "addressDescriptor",
        "attributions",
    ] {
        fields.remove(field);
    }
    fields.insert("place".to_string(), json!(format!("places/{id}")));
    fields.insert("placeId".to_string(), json!(id));
    fields.insert(
        "formattedAddress".to_string(),
        json!("Queen Bakery, 12 King Street, Toronto, ON M5V 1A1, Canada"),
    );
    json!({
        "primary": primary,
        "entrances": [{
            "location": {"latitude": 42.9995, "longitude": -79.005},
            "tags": ["PREFERRED"],
            "place": format!("places/{id}"),
        }],
    })
}

#[tokio::test]
async fn destinations_by_place_address_and_location() {
    let fixture = fixture();
    let router = &fixture.router;

    let by_place = post(
        router,
        DESTINATIONS,
        json!({"place": format!("places/{}", bakery_id())}),
    )
    .await;
    assert_eq!(by_place.status, StatusCode::OK);
    assert_eq!(
        by_place.body,
        json!({"destinations": [bakery_destination()]})
    );

    let by_text = post(
        router,
        DESTINATIONS,
        json!({"addressQuery": {"addressQuery": "10 King Street"}}),
    )
    .await;
    assert_eq!(by_text.status, StatusCode::OK);
    assert_eq!(
        by_text.body["destinations"][0],
        address_destination(),
        "{}",
        by_text.body
    );

    let by_parts = post(
        router,
        DESTINATIONS,
        json!({"addressQuery": {"address": {
            "addressLines": ["10 King Street"],
            "locality": "Toronto",
            "regionCode": "CA",
        }}}),
    )
    .await;
    assert_eq!(by_parts.status, StatusCode::OK);
    assert_eq!(by_parts.body["destinations"][0], address_destination());

    let by_location = post(
        router,
        DESTINATIONS,
        json!({"locationQuery": {"location": {"latitude": 43.0, "longitude": -79.0001}}}),
    )
    .await;
    assert_eq!(by_location.status, StatusCode::OK);
    assert_eq!(
        by_location.body,
        json!({"destinations": [address_destination()]})
    );

    let masked = post_with(
        router,
        DESTINATIONS,
        &[("x-goog-fieldmask", "destinations.primary.place")],
        json!({"place": format!("places/{}", bakery_id())}),
    )
    .await;
    assert_eq!(
        masked.body,
        json!({"destinations": [{"primary": {"place": format!("places/{}", bakery_id())}}]})
    );
}

#[tokio::test]
async fn destinations_leave_out_areas_and_refuse_what_google_refuses() {
    let fixture = fixture();
    let router = &fixture.router;

    for request in [
        json!({"place": format!("places/{}", encode_parts("osm:relation:5", Some(Layer::Locality), ""))}),
        json!({"addressQuery": {"addressQuery": "Toronto"}}),
    ] {
        let reply = post(router, DESTINATIONS, request.clone()).await;
        assert_eq!(reply.status, StatusCode::OK, "{request}");
        assert_eq!(reply.body, json!({}), "{request}");
    }

    let unknown = encode_parts("osm:way:999", Some(Layer::Poi), "");
    let reply = post(
        router,
        DESTINATIONS,
        json!({"place": format!("places/{unknown}")}),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        reply.body,
        google_error(
            404,
            "NOT_FOUND",
            &format!("Place 'places/{unknown}' was not found.")
        )
    );

    for (request, message) in [
        (
            json!({}),
            "One of place, addressQuery or locationQuery is required",
        ),
        (
            json!({"place": "places/x", "addressQuery": {"addressQuery": "10 King Street"}}),
            "Give only one of place, addressQuery or locationQuery",
        ),
        (
            json!({"place": "x"}),
            "Invalid place \"x\": expected places/PLACE_ID",
        ),
        (
            json!({"addressQuery": {}}),
            "addressQuery or address is required",
        ),
        (
            json!({"locationQuery": {}}),
            "locationQuery.location is required",
        ),
        (
            json!({"locationQuery": {"location": {"latitude": 91, "longitude": 0}}}),
            "locationQuery.location.latitude must be within [-90, 90], got 91",
        ),
    ] {
        let reply = post(router, DESTINATIONS, request.clone()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{request}");
        assert_eq!(
            reply.body,
            google_error(400, "INVALID_ARGUMENT", message),
            "{request}"
        );
    }
}

#[tokio::test]
async fn search_text_along_a_route_keeps_the_places_by_it() {
    let fixture = fixture();
    let router = &fixture.router;
    let along = |polyline: &str| {
        json!({"textQuery": "Queen Bakery",
               "searchAlongRouteParameters": {"polyline": {"encodedPolyline": polyline}}})
    };
    let near = post(router, SEARCH_TEXT, along("cpmeGnsfaN?o}@")).await;
    assert_eq!(near.status, StatusCode::OK, "{}", near.body);
    assert_eq!(near.body["places"][0]["id"], bakery_id(), "{}", near.body);
    let far = post(router, SEARCH_TEXT, along("_wpkG~~ggNo}@?")).await;
    assert_eq!(far.body, json!({}));
    let bad = post(router, SEARCH_TEXT, along("_p~iF")).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.body);
}

#[tokio::test]
async fn filters_og_holds_no_data_for_keep_no_place_and_the_rest_apply() {
    let fixture = fixture();
    let router = &fixture.router;
    let refused = |what: &str| google_error(400, "INVALID_ARGUMENT", what);
    for (body, message) in [
        (
            json!({"textQuery": "Queen", "openNow": true}),
            "openNow is not supported by this server.",
        ),
        (
            json!({"textQuery": "Queen", "pageToken": "garbage"}),
            "Invalid pageToken \"garbage\".",
        ),
        (
            json!({"textQuery": "Queen", "rankPreference": "DISTANCE"}),
            "rankPreference DISTANCE needs a locationBias on this server.",
        ),
    ] {
        let reply = post(router, SEARCH_TEXT, body.clone()).await;
        assert_eq!(reply.body, refused(message), "{body}");
    }
    for body in [
        json!({"textQuery": "Queen", "minRating": 4.0}),
        json!({"textQuery": "Queen", "priceLevels": ["PRICE_LEVEL_MODERATE"]}),
        json!({"textQuery": "Queen", "evOptions": {"minimumChargingRateKw": 10}}),
        json!({"textQuery": "Queen", "includedType": "chinese_restaurant", "strictTypeFiltering": true}),
    ] {
        let reply = post(router, SEARCH_TEXT, body.clone()).await;
        assert_eq!(reply.status, StatusCode::OK, "{body}");
        assert_eq!(reply.body, json!({}), "{body}");
    }
    let ids = |reply: &Value, list: &str| {
        reply[list]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| item["id"].as_str().unwrap_or_default().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let strict = post(
        router,
        SEARCH_TEXT,
        json!({"textQuery": "King Street", "includedType": "bakery", "strictTypeFiltering": true}),
    )
    .await
    .body;
    assert_eq!(ids(&strict, "places"), [bakery_id()]);
    let cafes = post(
        router,
        SEARCH_TEXT,
        json!({"textQuery": "King Street", "includedType": "cafe", "strictTypeFiltering": true}),
    )
    .await
    .body;
    assert_eq!(cafes, json!({}));

    let autocomplete = |types: Value| {
        post(
            router,
            AUTOCOMPLETE,
            json!({"input": "queen bak", "includedPrimaryTypes": types}),
        )
    };
    let reply = autocomplete(json!(["chinese_restaurant"])).await;
    assert_eq!(reply.body, json!({}), "no place og holds is of the type");
    let bakeries = autocomplete(json!(["bakery"])).await.body;
    assert_eq!(
        bakeries["suggestions"][0]["placePrediction"]["placeId"],
        bakery_id()
    );
    let cities = autocomplete(json!(["(cities)"])).await.body;
    assert_eq!(cities, json!({}), "a bakery is no city");
}

fn write_us_pack(pack: &Path) {
    let mut writer = Writer::create(pack).expect("writer");
    let (lon, lat) = (-122.0846, 37.4218);
    let mut place = |layer, name: &str, id| {
        writer
            .write(
                &Record::Place(
                    layer,
                    PlaceRecord {
                        alt_names: Vec::new(),
                        name: name.to_string(),
                        place_type: "admin_level:8".to_string(),
                        geometry: point_geometry(lon, lat),
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
        (
            PlaceLayer::Country,
            place(PlaceLayer::Country, "United States", 1),
        ),
        (
            PlaceLayer::Region,
            place(PlaceLayer::Region, "California", 2),
        ),
        (
            PlaceLayer::SubRegion,
            place(PlaceLayer::SubRegion, "Santa Clara County", 3),
        ),
        (
            PlaceLayer::Locality,
            place(PlaceLayer::Locality, "Mountain View", 4),
        ),
    ];
    writer.set_country_codes(
        [(
            tiers[0].1,
            crate::parse::lattice::CountryCode::parse("US").expect("code"),
        )]
        .into_iter()
        .collect(),
    );
    writer.set_subdivision_codes([(tiers[1].1, "US-CA".to_string())].into_iter().collect());
    let context = Some(RecordContext {
        admin_context: AdminContextTuple::from_tiers(tiers),
        flags: 0,
        zone: None,
        enclosing: Vec::new(),
    });
    let address = AddressComponents {
        block_number_len: None,
        areas: Vec::new(),
        number: "1600".to_string(),
        street: Some("Amphitheatre Parkway".to_string()),
        place: None,
        unit: None,
        locality: Some("Santa Clara County".to_string()),
        region: Some("CA".to_string()),
        postcode: Some("94043".to_string()),
        country: None,
    };
    writer
        .write(
            &AddressRecord {
                building: None,
                access: Vec::new(),
                disputed: false,
                street_postcodes: Vec::new(),
                address: address.clone(),
                geometry: point_geometry(lon, lat),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, 10),
            }
            .into(),
            context.clone(),
        )
        .expect("address");
    writer
        .write(
            &AddressRecord {
                building: None,
                access: Vec::new(),
                disputed: false,
                street_postcodes: Vec::new(),
                address: address.clone(),
                geometry: point_geometry(lon - 0.0013, lat),
                location_precision: LocationPrecision::Centroid,
                source: SourceProvenance::osm(ObjectType::Way, 13),
            }
            .into(),
            context.clone(),
        )
        .expect("outline of the same house");
    writer
        .write(
            &AddressRecord {
                building: None,
                access: Vec::new(),
                disputed: false,
                street_postcodes: Vec::new(),
                address: AddressComponents {
                    number: "1601".to_string(),
                    ..address.clone()
                },
                geometry: point_geometry(lon + 0.001, lat),
                location_precision: LocationPrecision::Centroid,
                source: SourceProvenance::osm(ObjectType::Way, 12),
            }
            .into(),
            context.clone(),
        )
        .expect("outline address");
    writer
        .write(
            &PoiRecord {
                alt_names: Vec::new(),
                name: "Googleplex".to_string(),
                category: "office:company".to_string(),
                address: Some(address),
                geometry: point_geometry(lon, lat),
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
    write_landmarks(&mut writer);
    writer.finish().expect("finish");
}

#[tokio::test]
async fn a_us_address_is_written_as_google_writes_it_in_its_town() {
    let fixture = us_fixture();
    let reply = get(
        &fixture.router,
        "/compat/google/v4/geocode/address/1600%20Amphitheatre%20Parkway,%20Mountain%20View,%20CA",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let result = &reply.body["results"][0];
    let formatted = "1600 Amphitheatre Pkwy, Mountain View, CA 94043, USA";
    assert_eq!(result["formattedAddress"], formatted, "{result}");
    assert_eq!(
        result["postalAddress"]["addressLines"],
        json!(["1600 Amphitheatre Pkwy"])
    );
    assert_eq!(result["postalAddress"]["locality"], "Mountain View");
    let route = result["addressComponents"]
        .as_array()
        .expect("components")
        .iter()
        .find(|component| component["types"][0] == "route")
        .expect("a route");
    assert_eq!(
        route,
        &json!({"longText": "Amphitheatre Parkway", "shortText": "Amphitheatre Pkwy",
                "types": ["route"]})
    );

    let found = post(
        &fixture.router,
        "/compat/google/v1/places:searchText",
        json!({"textQuery": "Googleplex"}),
    )
    .await;
    let place = &found.body["places"][0];
    assert_eq!(place["displayName"]["text"], "Googleplex", "{}", found.body);
    assert_eq!(place["formattedAddress"], formatted);
    let details = get(
        &fixture.router,
        &format!(
            "/compat/google/v1/{}",
            place["name"].as_str().expect("name")
        ),
    )
    .await;
    assert_eq!(
        details.body["formattedAddress"], formatted,
        "{}",
        details.body
    );
}

#[tokio::test]
async fn here_writes_a_us_street_in_the_postal_form() {
    let fixture = us_fixture();
    let here = get(
        &fixture.router,
        "/compat/here/v1/geocode?q=1600%20Amphitheatre%20Parkway,%20Mountain%20View,%20CA",
    )
    .await;
    let address = &here.body["items"][0]["address"];
    assert_eq!(address["street"], "Amphitheatre Pkwy", "{}", here.body);
    assert!(
        address["label"]
            .as_str()
            .is_some_and(|label| label.starts_with("1600 Amphitheatre Pkwy"))
    );
}

#[tokio::test]
async fn a_house_held_twice_leaves_room_for_the_next_house() {
    let fixture = us_fixture();
    let here = get(
        &fixture.router,
        "/compat/here/v1/revgeocode?at=37.4218,-122.0852&types=address&limit=2",
    )
    .await;
    let numbers = here.body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["address"]["houseNumber"].clone())
        .collect::<Vec<_>>();
    assert_eq!(numbers, [json!("1600"), json!("1601")], "{}", here.body);
}

#[tokio::test]
async fn a_house_held_twice_is_answered_once() {
    let fixture = us_fixture();
    let router = &fixture.router;
    let query = "1600%20Amphitheatre%20Parkway,%20Mountain%20View,%20CA";
    let google = get(
        router,
        &format!("/compat/google/v4/geocode/address/{query}"),
    )
    .await;
    let results = google.body["results"].as_array().expect("results");
    assert_eq!(results.len(), 1, "{}", google.body);
    assert_eq!(results[0]["granularity"], "ROOFTOP");
    assert_eq!(
        results[0]["location"],
        json!({"latitude": 37.4218, "longitude": -122.0846})
    );

    let here = get(router, &format!("/compat/here/v1/geocode?q={query}")).await;
    assert_eq!(
        here.body["items"].as_array().expect("items").len(),
        1,
        "{}",
        here.body
    );
    let mapbox = get(
        router,
        &format!("/compat/mapbox/search/geocode/v6/forward?q={query}"),
    )
    .await;
    let features = mapbox.body["features"].as_array().expect("features");
    assert_eq!(features.len(), 1, "{}", mapbox.body);
    let precisely = post(
        router,
        "/compat/precisely/v1/geocode",
        json!({"preferences": {"maxResults": 5},
               "addresses": [{"addressLines": ["1600 Amphitheatre Parkway, Mountain View, CA"],
                              "country": "US"}]}),
    )
    .await;
    let candidates = precisely.body["responses"][0]["results"]
        .as_array()
        .expect("results");
    assert_eq!(candidates.len(), 1, "{}", precisely.body);

    let reverse = get(
        router,
        "/compat/google/v4/geocode/location/37.4219,-122.0849",
    )
    .await;
    let houses = reverse.body["results"]
        .as_array()
        .expect("results")
        .iter()
        .filter(|result| result["types"] == json!(["street_address"]))
        .filter(|result| {
            result["formattedAddress"]
                .as_str()
                .is_some_and(|text| text.starts_with("1600 "))
        })
        .count();
    assert_eq!(houses, 1, "{}", reverse.body);
}

#[tokio::test]
async fn reverse_answers_the_house_nearest_the_point() {
    let fixture = us_fixture();
    let reply = get(
        &fixture.router,
        "/compat/google/v4/geocode/location/37.4224,-122.0846",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(
        reply.body["results"][0]["formattedAddress"],
        "1600 Amphitheatre Pkwy, Mountain View, CA 94043, USA",
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn a_house_mapped_as_an_outline_is_a_rooftop() {
    let fixture = us_fixture();
    let reply = get(
        &fixture.router,
        "/compat/google/v4/geocode/address/1601%20Amphitheatre%20Parkway,%20Mountain%20View,%20CA",
    )
    .await;
    let result = &reply.body["results"][0];
    assert_eq!(
        result["formattedAddress"], "1601 Amphitheatre Pkwy, Mountain View, CA 94043, USA",
        "{result}"
    );
    assert_eq!(result["granularity"], "ROOFTOP", "{result}");
}

fn write_landmarks(writer: &mut Writer) {
    let mut poi = |name: &str, category: &str, lon: f64, id, prominence| {
        writer
            .write(
                &PoiRecord {
                    alt_names: Vec::new(),
                    name: name.to_string(),
                    category: category.to_string(),
                    address: None,
                    geometry: point_geometry(lon, 43.0),
                    location_precision: LocationPrecision::Centroid,
                    source: SourceProvenance::osm(ObjectType::Way, id),
                    prominence,
                    wikidata: None,
                    brand: None,
                    details: None,
                    aliases: Vec::new(),
                }
                .into(),
                None,
            )
            .expect("poi")
    };
    poi(
        "Lantern Cafe",
        "amenity:cafe",
        -79.0,
        21,
        Default::default(),
    );
    let known = crate::record::Prominence {
        importance: 50,
        linked: true,
        languages: 3,
    };
    let mall = poi("Queen Mall", "shop:mall", -79.0015, 22, known);
    poi(
        "Corner Shop",
        "shop:convenience",
        -79.0005,
        23,
        Default::default(),
    );
    poi(
        "Far Shop",
        "shop:convenience",
        -79.006,
        24,
        Default::default(),
    );
    writer.set_extents(vec![(
        mall,
        [-790_030_000, 429_990_000, -789_990_000, 430_010_000],
    )]);
}

#[tokio::test]
async fn a_place_is_described_by_its_best_known_neighbours() {
    let fixture = us_fixture();
    let router = &fixture.router;
    let cafe = encode_parts("osm:way:21", Some(Layer::Poi), "");
    let reply = get_with(
        router,
        &format!("/compat/google/v1/places/{cafe}"),
        &[("x-goog-fieldmask", "addressDescriptor")],
    )
    .await;
    let landmarks = reply.body["addressDescriptor"]["landmarks"]
        .as_array()
        .expect("landmarks")
        .clone();
    let names = landmarks
        .iter()
        .map(|landmark| landmark["displayName"]["text"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [json!("Queen Mall"), json!("Corner Shop")],
        "{}",
        reply.body
    );
    assert!(landmarks[0].get("spatialRelationship").is_none());
    assert!(landmarks[1].get("spatialRelationship").is_none());
    let distance = landmarks[1]["straightLineDistanceMeters"]
        .as_f64()
        .expect("a distance");
    assert!((40.0..42.0).contains(&distance), "{distance}");
    assert_eq!(
        landmarks[0]["types"],
        json!([
            "shopping_mall",
            "store",
            "point_of_interest",
            "establishment"
        ])
    );
    let plain = get_with(
        router,
        &format!("/compat/google/v1/places/{cafe}"),
        &[("x-goog-fieldmask", "displayName")],
    )
    .await;
    assert!(plain.body.get("addressDescriptor").is_none());
}

#[tokio::test]
async fn a_destination_does_not_infer_containment_from_an_extent() {
    let fixture = us_fixture();
    let router = &fixture.router;
    let cafe = encode_parts("osm:way:21", Some(Layer::Poi), "");
    let reply = post(
        router,
        DESTINATIONS,
        json!({"place": format!("places/{cafe}")}),
    )
    .await;
    let destination = &reply.body["destinations"][0];
    assert!(destination.get("containingPlaces").is_none());
    let landmarks = destination["landmarks"].as_array().expect("landmarks");
    assert_eq!(landmarks.len(), 2, "{}", reply.body);
    assert_eq!(landmarks[0]["place"]["displayName"]["text"], "Queen Mall");
    assert_eq!(landmarks[1]["place"]["displayName"]["text"], "Corner Shop");
    assert_eq!(
        landmarks[1]["relationalDescription"],
        json!({"text": "Near Corner Shop", "languageCode": "en"})
    );
    assert_eq!(landmarks[0]["tags"], json!(["ADDRESS"]));
    let german = post(
        router,
        DESTINATIONS,
        json!({"place": format!("places/{cafe}"), "languageCode": "de"}),
    )
    .await;
    assert!(
        german.body["destinations"][0]["landmarks"][0]
            .get("relationalDescription")
            .is_none()
    );
}
