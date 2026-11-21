use std::{path::PathBuf, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
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
        AddressComponents, AddressRecord, Contact, Entrance, InterpolationAddressComponents,
        InterpolationRange, InterpolationRecord, Layer, LocationPrecision, ObjectType, PlaceLayer,
        PlaceRecord, PoiDetails, PoiRecord, Record, SourceProvenance, StreetRecord, point_geometry,
    },
    runtime::{AppState, build_router},
};

/// A Pack in its own temporary directory, removed with its jobs when the
/// test ends, failed or not.
#[derive(Debug)]
pub(super) struct TestPack(PathBuf);

impl Drop for TestPack {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        let _ = std::fs::remove_dir_all(self.0.with_extension("jobs"));
    }
}

impl TestPack {
    pub(super) fn router(&self) -> Router {
        build_router(self.state(Limits::DEFAULT), None, None)
    }

    /// A server on the Pack. Each keeps its jobs in the one directory, so
    /// two see the same jobs.
    pub(super) fn state(&self, limits: Limits) -> AppState {
        AppState::open(&self.0, &self.0.with_extension("jobs"), limits).expect("open pack")
    }
}

/// The vendor id og issues for an address of the fixture: its og id, its
/// layer and its label, since the catalogue holds no address.
fn address_vendor_id(og_id: &str) -> String {
    let label = match og_id {
        "osm:node:10" => "10 King Street, Unit 5, M5V 1A1",
        "osm:node:20" => "20 Queen Street, Toronto City, ON, M5H 2N2",
        _ => panic!("{og_id} is no address of the fixture"),
    };
    encode_parts(og_id, Some(Layer::Address), label)
}

fn line(from: [f64; 2], to: [f64; 2]) -> geojson::Geometry {
    geojson::Geometry::new(geojson::GeometryValue::LineString {
        coordinates: vec![from.to_vec().into(), to.to_vec().into()],
    })
}

/// Canada > Ontario > Toronto Division > Toronto > Old Toronto >
/// Entertainment District. Holds an address with only a unit and a postcode
/// stated and an entrance, one outside every area stating its own town,
/// region and areas, a POI with two phone numbers, a website, weekly hours
/// and an entrance, a street and an interpolation range. Calibrated, every confidence
/// one half.
pub(super) fn pack(name: &str) -> TestPack {
    let dir = std::env::temp_dir().join(format!(
        "open-geocode-compat-here-{name}-{}",
        std::process::id()
    ));
    let pack = TestPack(dir.clone());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(dir.with_extension("jobs"));
    let mut writer = Writer::create(&dir).expect("writer");
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
    let country = place(PlaceLayer::Country, "Canada", 1);
    let region = place(PlaceLayer::Region, "Ontario", 2);
    let locality = place(PlaceLayer::Locality, "Toronto", 3);
    let district = place(PlaceLayer::District, "Toronto Division", 4);
    let sublocality = place(PlaceLayer::SubLocality, "Old Toronto", 5);
    let neighbourhood = place(PlaceLayer::Neighbourhood, "Entertainment District", 6);
    writer.set_country_codes(
        [(
            country,
            crate::parse::lattice::CountryCode::parse("CA").expect("code"),
        )]
        .into_iter()
        .collect(),
    );
    writer.set_subdivision_codes([(region, "CA-ON".to_string())].into_iter().collect());
    writer.set_language_names(
        [(
            country,
            vec![
                ("fr".to_string(), "Canada".to_string()),
                ("de".to_string(), "Kanada".to_string()),
                ("sr".to_string(), "Канада".to_string()),
                ("sr-Latn".to_string(), "Kanada".to_string()),
            ],
        )]
        .into_iter()
        .collect(),
    );
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
    let king = AddressComponents {
        areas: Vec::new(),
        number: "10".to_string(),
        street: Some("King Street".to_string()),
        place: None,
        unit: Some("5".to_string()),
        locality: None,
        region: None,
        postcode: Some("M5V 1A1".to_string()),
        country: None,
    };
    let address = |address: AddressComponents, lon, id, access| {
        Record::from(AddressRecord {
            access,
            disputed: false,
            street_postcodes: Vec::new(),
            address,
            geometry: point_geometry(lon, 43.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, id),
        })
    };
    writer
        .write(
            &address(
                king.clone(),
                -79.0,
                10,
                Entrance::of_tag(Some("main"), -79.00001, 43.00001)
                    .into_iter()
                    .collect(),
            ),
            context.clone(),
        )
        .expect("address");
    let queen = AddressComponents {
        areas: vec!["Downtown".to_string(), "Core".to_string()],
        number: "20".to_string(),
        street: Some("Queen Street".to_string()),
        unit: None,
        locality: Some("Toronto City".to_string()),
        region: Some("ON".to_string()),
        postcode: Some("M5H 2N2".to_string()),
        ..king.clone()
    };
    // Outside every boundary og knows, its stated parts are all it has.
    writer
        .write(&address(queen, -79.002, 20, Vec::new()), None)
        .expect("address");
    writer
        .write(
            &PoiRecord {
                alt_names: Vec::new(),
                name: "Queen Bakery".to_string(),
                category: "shop:bakery".to_string(),
                // States a town and area of its own, beside og's. The
                // areas og found win HERE's admin tiers.
                address: Some(AddressComponents {
                    number: "12".to_string(),
                    unit: None,
                    locality: Some("Old Town".to_string()),
                    areas: vec!["Harbourfront".to_string()],
                    ..king
                }),
                geometry: point_geometry(-79.005, 43.0),
                location_precision: LocationPrecision::Centroid,
                source: SourceProvenance::osm(ObjectType::Way, 11),
                prominence: Default::default(),
                wikidata: None,
                brand: None,
                details: PoiDetails {
                    contact: Contact::stated(
                        Some("+1 416-555-0111;+1 416-555-0112".to_string()),
                        Some("https://queenbakery.example/".to_string()),
                        None,
                    ),
                    opening_hours: Some("Mo-Fr 08:00-18:00; Sa 09:00-13:00".to_string()),
                    extent: None,
                    access: Entrance::of_tag(Some("yes"), -79.00502, 43.00002)
                        .into_iter()
                        .collect(),
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
            &StreetRecord {
                other_ways: Vec::new(),
                road_class: crate::record::RoadClass::Residential,
                alt_names: Vec::new(),
                locality: None,
                name: "King Street".to_string(),
                geometry: line([-79.0, 43.0], [-79.0, 43.001]),
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
                geometry: line([-79.01, 43.0], [-79.01, 43.001]),
                representative_point: [-79.01, 43.0005],
                source: SourceProvenance::osm(ObjectType::Way, 13),
            }
            .into(),
            context,
        )
        .expect("interpolation");
    writer.finish().expect("finish");
    let reader = crate::pack::Reader::open(&dir).expect("pack");
    Model::new([0.0; FEATURES], Default::default())
        .save(&reader)
        .expect("confidence model");
    pack
}

pub(super) struct Reply {
    pub(super) status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    pub(super) fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| {
            panic!(
                "not JSON ({}): {}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    pub(super) fn text(&self) -> String {
        String::from_utf8(self.body.clone()).expect("UTF-8")
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }
}

pub(super) async fn send(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<(&str, &str)>,
) -> Reply {
    send_with(
        router,
        method,
        uri,
        &[],
        body.map(|(media_type, body)| (media_type, body.as_bytes().to_vec())),
    )
    .await
}

async fn send_with(
    router: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<(&str, Vec<u8>)>,
) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-request-id", "rid-1")
        .header(header::HOST, "og.test");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    if let Some((media_type, _)) = &body {
        request = request.header(header::CONTENT_TYPE, *media_type);
    }
    let body = body.map_or_else(Body::empty, |(_, body)| Body::from(body));
    let response = router
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("router response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body")
        .to_vec();
    Reply {
        status,
        headers,
        body,
    }
}

pub(super) async fn get(router: &Router, uri: &str) -> Reply {
    send(router, "GET", uri, None).await
}

/// The HERE address of the King Street address, every tier from og's
/// context, the unit and postcode from the record.
fn king_address() -> Value {
    json!({
        "label": "10 King Street, Unit 5, Toronto, ON M5V 1A1, Canada",
        "countryCode": "CAN",
        "countryName": "Canada",
        "stateCode": "ON",
        "state": "Ontario",
        "county": "Toronto Division",
        "city": "Toronto",
        "district": "Old Toronto",
        "subdistrict": "Entertainment District",
        "street": "King Street",
        "postalCode": "M5V 1A1",
        "houseNumber": "10",
        "unit": "5",
    })
}

fn king(extra: Value) -> Value {
    let mut item = json!({
        "title": "10 King Street, Unit 5, Toronto, ON M5V 1A1, Canada",
        "id": address_vendor_id("osm:node:10"),
        "resultType": "houseNumber",
        "houseNumberType": "PA",
        "address": king_address(),
        "position": {"lat": 43.0, "lng": -79.0},
        "access": [{"lat": 43.00001, "lng": -79.00001}],
    });
    let Value::Object(extra) = extra else {
        panic!("extra fields are an object");
    };
    for (key, value) in extra {
        item[key] = value;
    }
    item
}

fn queen_address() -> Value {
    json!({
        "label": "20 Queen Street, Toronto City, ON, M5H 2N2",
        "state": "ON",
        "city": "Toronto City",
        "district": "Downtown",
        "subdistrict": "Core",
        "street": "Queen Street",
        "postalCode": "M5H 2N2",
        "houseNumber": "20",
    })
}

fn bakery(distance: Option<i64>) -> Value {
    let mut item = json!({
        "title": "Queen Bakery",
        "id": encode_parts("osm:way:11", Some(Layer::Poi), ""),
        "resultType": "place",
        "address": {
            "label": "Queen Bakery, 12 King Street, Old Town, M5V 1A1",
            "countryCode": "CAN",
            "countryName": "Canada",
            "stateCode": "ON",
        "state": "Ontario",
            "county": "Toronto Division",
            "city": "Toronto",
            "district": "Old Toronto",
            "subdistrict": "Entertainment District",
            "street": "King Street",
            "postalCode": "M5V 1A1",
            "houseNumber": "12",
        },
        "position": {"lat": 43.0, "lng": -79.005},
        "access": [{"lat": 43.00002, "lng": -79.00502}],
        "contacts": [{
            "phone": [{"value": "+1 416-555-0111"}, {"value": "+1 416-555-0112"}],
            "www": [{"value": "https://queenbakery.example/"}],
        }],
        "openingHours": [{
            "text": ["Mon-Fri: 08:00 - 18:00", "Sat: 09:00 - 13:00"],
            "structured": [
                {"start": "T080000", "duration": "PT10H00M", "recurrence": "FREQ:DAILY;BYDAY:MO,TU,WE,TH,FR"},
                {"start": "T090000", "duration": "PT04H00M", "recurrence": "FREQ:DAILY;BYDAY:SA"},
            ],
        }],
        "categories": [{
            "id": "600-6300-0244",
            "name": "Bakery and Baked Goods Store",
            "primary": true,
        }],
    });
    if let Some(distance) = distance {
        item["distance"] = distance.into();
    }
    item
}

/// The og layer of a HERE address slot an area of the fixture fills.
fn slot_layer(slot: &str) -> &'static str {
    match slot {
        "district" => "neighbourhood",
        "city" => "locality",
        "county" => "district",
        "state" => "region",
        _ => "country",
    }
}

fn area(id: &str, title: &str, kind: (&str, &str, &str), slot: &str) -> Value {
    let (result_type, type_key, type_value) = kind;
    let mut item = json!({
        "title": title,
        "id": encode_parts(id, Layer::parse(slot_layer(slot)), title),
        "resultType": result_type,
        "address": {"label": title},
        "position": {"lat": 43.0, "lng": -79.0},
    });
    item[type_key] = type_value.into();
    item["address"][slot] = title.into();
    item
}

/// An area item that names itself by its own ISO code too.
fn coded(mut item: Value, slot: &str, code: &str) -> Value {
    item["address"][slot] = code.into();
    item
}

fn here_error(status: u16, title: &str, cause: Option<&str>, action: Option<&str>) -> Value {
    let mut body = json!({
        "status": status,
        "title": title,
        "correlationId": "rid-1",
        "requestId": "rid-1",
    });
    if let Some(cause) = cause {
        body["cause"] = cause.into();
    }
    if let Some(action) = action {
        body["action"] = action.into();
    }
    body
}

#[tokio::test]
async fn geocode_answers_here_items() {
    let pack = pack("geocode");
    let router = pack.router();

    let reply = get(
        &router,
        "/compat/here/v1/geocode?q=10+King+Street&apiKey=ignored",
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("x-request-id"), Some("rid-1"));
    assert_eq!(
        reply.json(),
        json!({"items": [king(json!({"scoring": {
            "queryScore": 1.0,
            "fieldScore": {"streets": [1.0], "houseNumber": 1.0},
        }}))]})
    );

    // `at` is og's focus and gives each item its distance from it.
    let reply = get(
        &router,
        "/compat/here/v1/geocode?q=10+King+Street&at=43.0,-79.005&in=countryCode:CAN",
    )
    .await;
    assert_eq!(
        reply.json(),
        json!({"items": [king(json!({"distance": 407, "scoring": {
            "queryScore": 1.0,
            "fieldScore": {"streets": [1.0], "houseNumber": 1.0, "country": 1.0},
        }}))]})
    );

    // A country filter og honours. Nothing of the Pack lies in the US.
    let reply = get(
        &router,
        "/compat/here/v1/geocode?q=10+King+Street&in=countryCode:USA",
    )
    .await;
    assert_eq!(reply.json(), json!({"items": []}));

    // Whitespace asks nothing, and nothing is answered.
    let reply = get(&router, "/compat/here/v1/geocode?q=+").await;
    assert_eq!(reply.json(), json!({"items": []}));
}

#[tokio::test]
async fn geocode_reads_qualified_queries() {
    let pack = pack("geocode-qq");
    let router = pack.router();
    let scored = json!({"scoring": {
        "queryScore": 1.0,
        "fieldScore": {"streets": [1.0], "houseNumber": 1.0, "postalCode": 1.0, "country": 1.0},
    }});

    // A street with no text is og's structured geocode.
    let structured = "/compat/here/v1/geocode?qq=houseNumber%3D10%3Bstreet%3DKing+Street%3BpostalCode%3DM5V+1A1%3Bcountry%3DCAN";
    let reply = get(&router, structured).await;
    assert_eq!(reply.json(), json!({"items": [king(scored.clone())]}));

    // A country code in `qq` filters as `in` does.
    let elsewhere =
        "/compat/here/v1/geocode?qq=houseNumber%3D10%3Bstreet%3DKing+Street%3Bcountry%3DUSA";
    assert_eq!(get(&router, elsewhere).await.json(), json!({"items": []}));

    // Text and qualified parts together are one text.
    let hybrid = "/compat/here/v1/geocode?q=10+King&qq=street%3DStreet%3BpostalCode%3DM5V+1A1";
    let reply = get(&router, hybrid).await;
    let mut unfiltered = scored;
    unfiltered["scoring"]["fieldScore"]
        .as_object_mut()
        .expect("field scores")
        .remove("country");
    assert_eq!(reply.json()["items"][0], king(unfiltered));

    let reply = get(&router, "/compat/here/v1/geocode?qq=floor%3D3").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json(),
        here_error(
            400,
            "Illegal input for parameter 'qq'",
            Some("Actual parameter value: 'floor=3'"),
            Some(
                "Unsupported value: 'floor'. Supported values: 'country', 'state', 'county', \
                 'city', 'district', 'street', 'houseNumber', 'postalCode'"
            ),
        )
    );
}

#[tokio::test]
async fn geocode_types_keep_the_kinds_asked() {
    let pack = pack("geocode-types");
    let router = pack.router();
    let uri = |types: &str| format!("/compat/here/v1/geocode?q=Queen+Street+Toronto&types={types}");

    let street = get(&router, &uri("street")).await.json();
    let items = street["items"].as_array().expect("items");
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0]["id"],
        encode_parts(
            "osm:way:13:interp:2-98",
            Some(Layer::Interpolation),
            "Queen Street 2-98 even, Toronto",
        )
    );
    // A range found by its street alone is a street, as no number was asked.
    assert_eq!(items[0]["resultType"], "street");
    assert_eq!(items[0].get("houseNumberType"), None);

    let numbers = get(&router, &uri("houseNumber")).await.json();
    assert_eq!(
        numbers,
        json!({"items": [{
            "title": "20 Queen Street, Toronto City, ON, M5H 2N2",
            "id": address_vendor_id("osm:node:20"),
            "resultType": "houseNumber",
            "houseNumberType": "PA",
            "address": queen_address(),
            "position": {"lat": 43.0, "lng": -79.002},
            // "Toronto" is a word of "Toronto City", a partial match, so
            // the city counts half.
            "scoring": {"queryScore": 0.75, "fieldScore": {"streets": [1.0], "city": 0.5}},
        }]})
    );

    assert_eq!(
        get(&router, &uri("place")).await.json(),
        json!({"items": []})
    );
    let reply = get(&router, &uri("street,building")).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json()["action"],
        "Unsupported value: 'building'. Supported values: 'address', 'area', 'city', \
         'houseNumber', 'place', 'postalCode', 'street'"
    );
}

#[tokio::test]
async fn revgeocode_answers_the_nearest_first() {
    let pack = pack("revgeocode");
    let router = pack.router();

    let reply = get(&router, "/compat/here/v1/revgeocode?at=43.0,-79.0").await;
    assert_eq!(
        reply.json(),
        json!({"items": [king(json!({"distance": 0}))]})
    );

    // og's areas at the point follow, each in its own address slot.
    let reply = get(
        &router,
        "/compat/here/v1/revgeocode?at=43.0,-79.0&types=area&limit=100",
    )
    .await;
    let locality = |kind| ("locality", "localityType", kind);
    let admin = |kind| ("administrativeArea", "administrativeAreaType", kind);
    assert_eq!(
        reply.json(),
        json!({"items": [
            area("osm:relation:6", "Entertainment District", locality("district"), "district"),
            area("osm:relation:5", "Old Toronto", locality("city"), "city"),
            area("osm:relation:4", "Toronto Division", admin("county"), "county"),
            coded(area("osm:relation:2", "Ontario", admin("state"), "state"), "stateCode", "ON"),
            coded(area("osm:relation:1", "Canada", admin("country"), "countryName"), "countryCode", "CAN"),
        ]})
    );

    // A circle is a hard filter on the distance. The address 8 m away is
    // outside 5 m, and the areas hold the point.
    let reply = get(
        &router,
        "/compat/here/v1/revgeocode?in=circle:43.0,-79.0001;r=5&limit=2",
    )
    .await;
    assert_eq!(
        reply.json(),
        json!({"items": [
            area("osm:relation:6", "Entertainment District", locality("district"), "district"),
            area("osm:relation:5", "Old Toronto", locality("city"), "city"),
        ]})
    );
    let reply = get(
        &router,
        "/compat/here/v1/revgeocode?in=circle:43.0,-79.0001;r=9",
    )
    .await;
    assert_eq!(
        reply.json(),
        json!({"items": [king(json!({"distance": 8}))]})
    );

    // A bearing answers streets only.
    let reply = get(
        &router,
        "/compat/here/v1/revgeocode?at=43.0005,-79.0&bearing=90",
    )
    .await;
    let items = reply.json()["items"].clone();
    assert_eq!(
        items[0]["id"],
        encode_parts("osm:way:12", Some(Layer::Street), "")
    );
    assert_eq!(items[0]["resultType"], "street");
    assert_eq!(items[0]["address"]["street"], "King Street");

    let reply = get(
        &router,
        "/compat/here/v1/revgeocode?at=43.0,-79.0&in=circle:43,-79;r=5",
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json(),
        here_error(
            400,
            "Parameters 'at' and 'in' are mutually exclusive",
            None,
            Some("Provide either 'at' or 'in=circle', not both"),
        )
    );
    let reply = get(&router, "/compat/here/v1/revgeocode?at=91,0").await;
    assert_eq!(
        reply.json(),
        here_error(
            400,
            "Illegal input for parameter 'at'",
            Some("Actual parameter value: '91,0'"),
            Some("Provide {latitude},{longitude}"),
        )
    );
}

#[tokio::test]
async fn multi_revgeocode_answers_each_sub_request() {
    let pack = pack("multi-revgeocode");
    let router = pack.router();
    let body = "id=1&at=43.0,-79.0\nid=2&at=95,0\n\nid=1&at=0,0\nid=3&in=circle:43.0,-79.0;r=10\n";
    let multi = |media_type: &'static str, body: String| {
        let router = router.clone();
        async move {
            send(
                &router,
                "POST",
                "/compat/here/v1/multi-revgeocode",
                Some((media_type, &body)),
            )
            .await
        }
    };

    let reply = multi("text/plain; charset=UTF-8", body.to_string()).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json(),
        json!({"results": [
            {"id": "1", "items": [king(json!({"distance": 0}))]},
            {"id": "2", "error": {"title": "Illegal input for parameter 'at'"}},
            {"id": "3", "items": [king(json!({"distance": 0}))]},
        ]})
    );

    let reply = multi("application/json", "[]".to_string()).await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let reply = multi("text/plain", "at=43,-79".to_string()).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json()["title"], "Illegal input for parameter 'id'");
    let lines = |count: usize| {
        (0..count)
            .map(|index| format!("id={index}&at=43,-79"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let reply = multi("text/plain", lines(101)).await;
    assert_eq!(reply.json()["title"], "Too many sub-requests: 101");
    let reply = multi("application/x-www-form-urlencoded", lines(100)).await;
    assert_eq!(reply.json()["results"].as_array().map(Vec::len), Some(100));
}

#[tokio::test]
async fn autosuggest_autocomplete_and_discover() {
    let pack = pack("suggest");
    let router = pack.router();
    let queen = json!({
        "title": "20 Queen Street, Toronto City, ON, M5H 2N2",
        "id": address_vendor_id("osm:node:20"),
        "resultType": "houseNumber",
        "houseNumberType": "PA",
        "address": queen_address(),
    });
    // og's marks of "Queen" in each label: the title holds them where it
    // is the label or starts it.
    let marked = |mut item: Value, at: usize| {
        let mark = json!([{"start": at, "end": at + 5}]);
        item["highlights"] = json!({"title": mark, "address": {"label": mark}});
        item
    };
    let queen = marked(queen, 3);
    let mut queen_near = queen.clone();
    queen_near["position"] = json!({"lat": 43.0, "lng": -79.002});
    queen_near["distance"] = 163.into();
    let bakery_near = marked(bakery(Some(407)), 0);

    let reply = get(&router, "/compat/here/v1/autosuggest?q=Queen&at=43.0,-79.0").await;
    assert_eq!(
        reply.json(),
        json!({"items": [bakery_near.clone(), queen_near], "queryTerms": []})
    );

    // Paging fields appear only with `offset`, and `nextOffset` only before
    // the last page.
    let page = "/compat/here/v1/autosuggest?q=Queen&at=43.0,-79.0&limit=1&offset=";
    let reply = get(&router, &format!("{page}0")).await;
    assert_eq!(
        reply.json(),
        json!({
            "items": [bakery_near],
            "queryTerms": [],
            "offset": 0,
            "nextOffset": 1,
            "count": 1,
            "limit": 1,
        })
    );
    let reply = get(&router, &format!("{page}1")).await.json();
    assert_eq!(reply["offset"], 1);
    assert_eq!(reply.get("nextOffset"), None);
    assert_eq!(reply["items"][0]["id"], address_vendor_id("osm:node:20"));

    let reply = get(&router, "/compat/here/v1/autosuggest?q=Queen").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json(),
        here_error(
            400,
            "Missing parameter 'at', 'in=circle' or 'in=bbox'",
            None,
            Some(
                "Provide one of 'at', 'in=circle' or 'in=bbox'; 'in=countryCode' needs 'at' beside it"
            ),
        )
    );
    let reply = get(&router, "/compat/here/v1/autosuggest?q=+&at=43,-79").await;
    assert_eq!(reply.json(), json!({"items": [], "queryTerms": []}));

    // A page reaching past what og answers is cut short, not refused.
    for uri in [
        "/compat/here/v1/autosuggest?q=Queen&at=43,-79&limit=21",
        "/compat/here/v1/autosuggest?q=Queen&at=43,-79&limit=10&offset=15",
        "/compat/here/v1/discover?q=Queen&at=43,-79&limit=51",
    ] {
        let reply = get(&router, uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
    }
    for path in ["discover", "autosuggest"] {
        let uri = format!("/compat/here/v1/{path}?q=Queen&at=43,-79");
        let form = "application/x-www-form-urlencoded";
        let reply = send(&router, "POST", &uri, Some((form, "limit=1"))).await;
        assert_eq!(
            reply.json()["title"],
            "Parameter 'limit' is not allowed in the request body"
        );
        let reply = send(&router, "POST", &uri, Some((form, ""))).await;
        assert_eq!(reply.status, StatusCode::OK, "{path}");
    }

    // Autocomplete is addresses alone, with no position, titled from the
    // largest area down, its marks following "Queen" there.
    let mut suggested = queen.clone();
    suggested["title"] = json!("M5H 2N2, Toronto City, 20 Queen Street");
    suggested["highlights"]["title"] = json!([{"start": 26, "end": 31}]);
    let reply = get(&router, "/compat/here/v1/autocomplete?q=Queen").await;
    assert_eq!(reply.json(), json!({"items": [suggested]}));
    let reply = get(&router, "/compat/here/v1/autocomplete?q=Queen&types=street").await;
    assert_eq!(reply.json(), json!({"items": []}));
    let reply = get(&router, "/compat/here/v1/autocomplete?q=Queen&limit=21").await;
    assert_eq!(reply.json()["title"], "Illegal input for parameter 'limit'");
    let reply = get(&router, "/compat/here/v1/autocomplete?q=Queen&limit=20").await;
    assert_eq!(reply.status, StatusCode::OK);

    // Discover is og's place search. A box holds the bakery, one beside it
    // does not; a circle is the distance from its center.
    let discover = |area: &str| format!("/compat/here/v1/discover?q=Queen+Bakery&in={area}");
    let reply = get(&router, &discover("bbox:-79.01,42.99,-79.0,43.01")).await;
    assert_eq!(reply.json(), json!({"items": [bakery(None)]}));
    let reply = get(&router, &discover("bbox:-78.9,42.99,-78.8,43.01")).await;
    assert_eq!(reply.json(), json!({"items": []}));
    let reply = get(&router, &discover("circle:43.0,-79.0;r=500")).await;
    assert_eq!(reply.json(), json!({"items": [bakery(Some(407))]}));
    let reply = get(&router, &discover("circle:43.0,-79.0;r=400")).await;
    assert_eq!(reply.json(), json!({"items": []}));
}

#[tokio::test]
async fn browse_answers_the_places_around_a_point_by_kind_and_name() {
    let pack = pack("browse");
    let router = pack.router();
    let base = "/compat/here/v1/browse";
    let found = json!({"items": [bakery(Some(407))]});
    for query in [
        "at=43,-79",
        "at=43,-79&categories=600-6300-0244",
        "at=43,-79&categories=600-6300,100",
        "at=43,-79&categories=600,!100-1000-0009&limit=50",
        "at=43,-79&name=queen%20bak",
        "at=43,-79&in=countryCode:CAN",
        "at=43,-79&in=circle:43,-79.005;r=100",
        "at=43,-79&in=bbox:-79.1,42.9,-78.9,43.1",
        "at=43,-79&chains=!1566&foodTypes=!800-067",
    ] {
        let reply = get(&router, &format!("{base}?{query}")).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}");
        assert_eq!(reply.json(), found, "{query}");
    }
    let reply = send(&router, "POST", &format!("{base}?at=43,-79"), None).await;
    assert_eq!(reply.json(), found);

    // Another kind, a kind og has none of, the kind left out, another name,
    // another country and outside the area.
    for query in [
        "at=43,-79&categories=100-1000-0000",
        "at=43,-79&categories=999-9999-9999",
        "at=43,-79&categories=600,!600-6300-0244",
        "at=43,-79&name=pharmacy",
        "at=43,-79&in=countryCode:FRA",
        "at=43,-79&in=circle:43,-79;r=100",
        "at=43,-79&in=bbox:0,0,1,1",
    ] {
        let reply = get(&router, &format!("{base}?{query}")).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}");
        assert_eq!(reply.json(), json!({"items": []}), "{query}");
    }
    // og's places carry no chain or food type, so asking for one keeps none,
    // and a page past og's 50 results is cut short.
    for query in [
        "at=43,-79&chains=1566",
        "at=43,-79&foodTypes=800-067",
        "at=43,-79&limit=20&offset=40",
    ] {
        let reply = get(&router, &format!("{base}?{query}")).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}");
        assert!(
            reply.json()["items"].as_array().is_some_and(Vec::is_empty),
            "{query}"
        );
    }
    let reply = get(&router, &format!("{base}?at=43,-79&limit=100")).await;
    assert_eq!(reply.json(), found);

    let reply = get(&router, &format!("{base}?at=43,-79&offset=0&limit=1")).await;
    assert_eq!(
        reply.json(),
        json!({"items": [bakery(Some(407))], "offset": 0, "count": 1, "limit": 1})
    );

    let reply = get(&router, &format!("{base}?categories=100")).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json(),
        here_error(400, "Missing parameter 'at'", None, Some("Provide 'at'"))
    );
    let reply = get(&router, &format!("{base}?at=43,-79&categories=coffee")).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json(),
        here_error(
            400,
            "Illegal input for parameter 'categories'",
            Some("Actual parameter value: 'coffee'"),
            Some("'coffee' is not a category id, such as 100-1000-0009"),
        )
    );
}

#[tokio::test]
async fn lookup_finds_what_a_search_returned() {
    let pack = pack("lookup");
    let router = pack.router();
    let found = get(&router, "/compat/here/v1/discover?q=Queen+Bakery&at=43,-79")
        .await
        .json();
    let id = found["items"][0]["id"].as_str().expect("id").to_string();

    let reply = get(&router, &format!("/compat/here/v1/lookup?id={id}")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json(), bakery(None));

    for id in [
        encode_parts("osm:way:999", Some(Layer::Poi), ""),
        "not-an-id".to_string(),
    ] {
        let reply = get(&router, &format!("/compat/here/v1/lookup?id={id}")).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(
            reply.json(),
            here_error(
                404,
                "Requested id could not be found",
                Some(&format!("Actual parameter value: '{id}'")),
                None,
            )
        );
    }
    let reply = get(&router, "/compat/here/v1/lookup").await;
    assert_eq!(
        reply.json(),
        here_error(400, "Missing parameter 'id'", None, Some("Provide 'id'"))
    );
}

#[tokio::test]
async fn lookup_finds_a_record_the_catalogue_does_not_hold() {
    let pack = pack("lookup-label");
    let router = pack.router();
    let found = get(&router, "/compat/here/v1/geocode?q=50+Queen+Street+Toronto")
        .await
        .json();
    let range = &found["items"][0];
    assert_eq!(range["houseNumberType"], "interpolated");
    let id = range["id"].as_str().expect("id");
    assert_eq!(
        decode_id(id).map(|named| named.id().to_string()).as_deref(),
        Some("osm:way:13:interp:2-98")
    );
    let reply = get(&router, &format!("/compat/here/v1/lookup?id={id}")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["id"], range["id"]);
    assert_eq!(reply.json()["title"], range["title"]);

    // An imported row renumbered by a newer release of its data: the
    // record with the label the id carries.
    let Some(crate::compat::OgRef::Labelled { label, .. }) = decode_id(id) else {
        panic!("{id} carries no label");
    };
    let renumbered = encode_parts(
        "openaddresses:ca/on/toronto:row:999999",
        Some(Layer::Interpolation),
        &label,
    );
    let reply = get(&router, &format!("/compat/here/v1/lookup?id={renumbered}")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["title"], range["title"]);

    let unknown = encode_parts(
        "openaddresses:ca/on/toronto:row:1",
        Some(Layer::Address),
        "Nowhere Lane, Atlantis",
    );
    let reply = get(&router, &format!("/compat/here/v1/lookup?id={unknown}")).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn errors_take_here_shapes() {
    let pack = pack("errors");
    let router = pack.router();

    let reply = get(&router, "/compat/here/v1/geocode").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json(),
        here_error(
            400,
            "Missing parameter 'q' or 'qq'",
            None,
            Some("Provide 'q', 'qq' or both")
        )
    );
    for (limit, status) in [
        ("0", 400),
        ("1", 200),
        ("100", 200),
        ("101", 400),
        ("x", 400),
    ] {
        let reply = get(
            &router,
            &format!("/compat/here/v1/geocode?q=King&limit={limit}"),
        )
        .await;
        assert_eq!(reply.status.as_u16(), status, "limit={limit}");
    }
    let reply = get(&router, "/compat/here/v1/geocode?q=King&limit=101").await;
    assert_eq!(
        reply.json(),
        here_error(
            400,
            "Illegal input for parameter 'limit'",
            Some("Actual parameter value: '101'"),
            Some("Provide an integer from 1 to 100"),
        )
    );
    let reply = get(&router, "/compat/here/v1/geocode?q=King&q=Queen").await;
    assert_eq!(
        reply.json(),
        here_error(
            400,
            "Duplicate parameter 'q'",
            None,
            Some("Send each parameter once")
        )
    );
    let reply = get(&router, "/compat/here/v1/geocode?q=King&in=countryCode:XXX").await;
    assert_eq!(
        reply.json(),
        here_error(
            400,
            "Illegal input for parameter 'in'",
            Some("Actual parameter value: 'countryCode:XXX'"),
            Some("Unsupported country code: 'XXX'. Provide ISO 3166-1 alpha-3 codes"),
        )
    );
    // og's own refusal, named by the HERE parameter that became og's.
    let long = "a".repeat(300);
    let reply = get(&router, &format!("/compat/here/v1/geocode?q={long}")).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json()["title"], "Illegal input for parameter 'q'");
    assert_eq!(reply.json()["requestId"], "rid-1");

    // og keeps no store for signals, so the route is not served.
    let reply = send(
        &router,
        "POST",
        "/compat/here/v1/signals",
        Some(("text/plain", "version=1&action=here:gs:action:view")),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let reply = get(&router, "/compat/here/v1/nowhere").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        reply.json(),
        here_error(
            404,
            "Not Found",
            Some("No endpoint at /compat/here/v1/nowhere"),
            None
        )
    );
    let reply = send(&router, "POST", "/compat/here/v1/geocode?q=King", None).await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        reply.json(),
        here_error(405, "Method Not Allowed", None, None)
    );
    let reply = send(&router, "DELETE", "/compat/here/v1/lookup", None).await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);

    // The batch API's own shape, which has a code and no request id.
    let reply = get(&router, "/compat/here/v7/nowhere").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        reply.json(),
        json!({
            "title": "Not found",
            "status": 404,
            "code": "E6004040",
            "cause": "No resource at /compat/here/v7/nowhere for this method",
            "action": "Check the URL and method",
            "correlationId": "rid-1",
        })
    );
}

/// Fidelity. Every og value of every answer lands in its HERE slot
/// unchanged, across each endpoint and every layer the Pack holds, and every
/// slot og has data for is filled at least once, so a slot that goes empty
/// fails here.
#[tokio::test]
async fn every_og_value_lands_in_its_here_slot() {
    let pack = pack("fidelity");
    let router = pack.router();
    let world = "in=bbox:-180,-90,180,90";
    let pairs = [
        (
            "/geocode?q=10+King+Street",
            "/compat/here/v1/geocode?q=10+King+Street".to_string(),
        ),
        (
            "/geocode?q=Queen+Street+Toronto",
            "/compat/here/v1/geocode?q=Queen+Street+Toronto".to_string(),
        ),
        (
            "/geocode?q=King+Street",
            "/compat/here/v1/geocode?q=King+Street".to_string(),
        ),
        (
            "/reverse?lon=-79&lat=43",
            "/compat/here/v1/revgeocode?at=43,-79&limit=100".to_string(),
        ),
        (
            "/places?q=Queen+Bakery",
            format!("/compat/here/v1/discover?q=Queen+Bakery&{world}"),
        ),
        (
            "/autocomplete?q=Queen",
            format!("/compat/here/v1/autosuggest?q=Queen&{world}"),
        ),
        (
            "/autocomplete?q=10+King",
            "/compat/here/v1/autocomplete?q=10+King".to_string(),
        ),
        (
            "/places?q=Queen+Bakery",
            "/compat/here/v1/browse?at=43,-79&name=Queen+Bakery".to_string(),
        ),
        (
            "/places?ids=osm:way:11",
            format!(
                "/compat/here/v1/lookup?id={}",
                encode_parts("osm:way:11", Some(Layer::Poi), "")
            ),
        ),
    ];
    let mut layers = std::collections::BTreeSet::new();
    let mut filled = std::collections::BTreeSet::new();
    for (native, here) in pairs {
        let og = get(&router, native).await.json();
        let reply = get(&router, &here).await;
        assert_eq!(reply.status, StatusCode::OK, "{here}");
        let reply = reply.json();
        let items = match reply.get("items") {
            Some(items) => items.as_array().expect("items").clone(),
            None => vec![reply],
        };
        let features = og["features"].as_array().expect("features");
        assert_eq!(features.len(), items.len(), "{here} dropped a result");
        assert!(!features.is_empty(), "{native} found nothing");
        for (feature, item) in features.iter().zip(&items) {
            let properties = &feature["properties"];
            layers.insert(properties["layer"].as_str().unwrap_or_default().to_string());
            assert_fidelity(properties, &feature["geometry"]["coordinates"], item, &here);
            filled.extend(slots_filled(item));
        }
    }
    assert_eq!(
        filled.into_iter().collect::<Vec<_>>(),
        [
            "access",
            "address.countryCode",
            "address.stateCode",
            "contacts",
            "distance",
            "highlights.address.label",
            "highlights.title",
            "mapView",
            "openingHours",
            "scoring.fieldScore",
            "scoring.queryScore",
        ]
    );
    assert_eq!(
        layers.into_iter().collect::<Vec<_>>(),
        [
            "address",
            "country",
            "district",
            "interpolation",
            "locality",
            "neighbourhood",
            "poi",
            "region",
            "street"
        ]
    );
}

fn assert_fidelity(properties: &Value, coordinates: &Value, item: &Value, uri: &str) {
    let text = |value: &Value| value.as_str().map(str::to_string);
    let layer = properties["layer"].as_str().unwrap_or_default();
    let address = &properties["address"];
    let context = &properties["context"];
    let own = |of: &str| (layer == of).then(|| text(&properties["name"])).flatten();
    let area = |tier: &str| text(&context[tier]["name"]);
    let slot = |name: &str| text(&item["address"][name]);
    let at = format!("{uri}: {}", properties["id"]);

    let id = properties["id"].as_str().unwrap_or_default();
    let label = properties["label"].as_str().unwrap_or_default();
    assert_eq!(
        item["id"],
        encode_parts(id, Layer::parse(layer), label),
        "{at}"
    );
    // A house or street is labelled by its postal lines, the house's own
    // line first; anything else by og's label.
    let lines = properties["address_lines"]
        .as_array()
        .map(|lines| lines.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let label = match (layer, lines.as_slice()) {
        ("address" | "interpolation" | "street", [_, _, ..]) => {
            let street = properties["address"]["street"].as_str().unwrap_or_default();
            let mut lines = lines.clone();
            if let Some(at) = lines.iter().position(|line| line.contains(street)) {
                let line = lines.remove(at);
                lines.insert(0, line);
            }
            json!(lines.join(", "))
        }
        _ => properties["label"].clone(),
    };
    // An `/autocomplete` suggestion has no point, no way in and no box, and
    // is titled from the largest area down to its own line.
    let suggestion = uri.contains("/v1/autocomplete?");
    let own_line = label.as_str().and_then(|label| label.split(", ").next());
    let title = if layer == "poi" {
        properties["name"].clone()
    } else if suggestion {
        let address = &item["address"];
        let mut parts = Vec::<&str>::new();
        for part in [
            address["countryName"].as_str(),
            address["postalCode"]
                .as_str()
                .filter(|_| item["resultType"] != "street"),
            address["city"].as_str(),
            own_line,
        ]
        .into_iter()
        .flatten()
        {
            if !parts.contains(&part) {
                parts.push(part);
            }
        }
        json!(parts.join(", "))
    } else {
        label.clone()
    };
    assert_eq!(item["title"], title, "{at}");
    assert_eq!(item["address"]["label"], label, "{at}");
    match suggestion {
        true => assert_eq!(item.get("position"), None, "{at}"),
        false => {
            assert_eq!(item["position"]["lat"], coordinates[1], "{at}");
            assert_eq!(item["position"]["lng"], coordinates[0], "{at}");
        }
    }
    assert_eq!(slot("houseNumber"), text(&address["housenumber"]), "{at}");
    assert_eq!(slot("unit"), text(&address["unit"]), "{at}");
    assert_eq!(slot("postalCode"), text(&address["postcode"]), "{at}");
    assert_eq!(
        slot("street"),
        text(&address["street"]).or_else(|| own("street")),
        "{at}"
    );
    assert_eq!(
        slot("city"),
        area("locality")
            .or_else(|| text(&address["locality"]))
            .or_else(|| own("locality")),
        "{at}"
    );
    assert_eq!(
        slot("state"),
        area("region")
            .or_else(|| text(&address["region"]))
            .or_else(|| own("region")),
        "{at}"
    );
    assert_eq!(
        slot("county"),
        area("district").or_else(|| own("district")),
        "{at}"
    );
    let (district, subdistrict) = match (area("sublocality"), area("neighbourhood")) {
        (Some(sublocality), neighbourhood) => (Some(sublocality), neighbourhood),
        (None, Some(neighbourhood)) => (Some(neighbourhood), None),
        (None, None) => (text(&address["areas"][0]), text(&address["areas"][1])),
    };
    assert_eq!(
        slot("district"),
        district.or_else(|| own("neighbourhood")),
        "{at}"
    );
    assert_eq!(slot("subdistrict"), subdistrict, "{at}");
    assert_eq!(
        slot("countryName"),
        area("country").or_else(|| own("country")),
        "{at}"
    );
    // An area's own code names it where no context area does: `US` on the
    // country, `US-RI` on the region.
    let own_code = |of: &str| (layer == of).then(|| text(&properties["code"])).flatten();
    assert_eq!(
        slot("countryCode"),
        text(&context["country"]["code"])
            .or_else(|| own_code("country"))
            .and_then(|code| iso::alpha3(&code))
            .map(str::to_string),
        "{at}"
    );
    assert_eq!(
        slot("stateCode"),
        text(&context["region"]["code"])
            .or_else(|| own_code("region"))
            .and_then(|code| code.split_once('-').map(|(_, own)| own.to_string())),
        "{at}"
    );
    // Contacts and hours are place details, which discover, browse, lookup
    // and autosuggest carry.
    let details = [
        "/v1/discover",
        "/v1/browse",
        "/v1/lookup",
        "/v1/autosuggest",
    ]
    .iter()
    .any(|route| uri.contains(route));
    let split = |value: &Value| {
        value
            .as_str()
            .map(|value| {
                value
                    .split(';')
                    .map(|value| json!({"value": value.trim()}))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let contact = &properties["contact"];
    match (details, contact.is_object()) {
        (true, true) => {
            let contacts = &item["contacts"][0];
            for (og, here) in [("phone", "phone"), ("website", "www"), ("email", "email")] {
                assert_eq!(
                    contacts.get(here).cloned().unwrap_or(json!([])),
                    json!(split(&contact[og])),
                    "{at} {here}"
                );
            }
        }
        _ => assert_eq!(item.get("contacts"), None, "{at}"),
    }
    let hours = properties["opening_hours"]
        .as_str()
        .and_then(super::hours::parse)
        .filter(|_| details);
    assert_eq!(
        item.get("openingHours").cloned(),
        hours.map(|hours| json!([hours])),
        "{at}"
    );
    match properties["distance_m"].as_f64() {
        Some(distance) => assert_eq!(item["distance"], distance.round() as i64, "{at}"),
        None if uri.contains("revgeocode") => assert_eq!(item.get("distance"), None, "{at}"),
        None => {}
    }
    // A place has no box in HERE's answers; anything else carries og's.
    match (layer, properties["bbox"].as_array()) {
        (_, Some(_)) if suggestion => assert_eq!(item.get("mapView"), None, "{at}"),
        ("poi", _) | (_, None) => assert_eq!(item.get("mapView"), None, "{at}"),
        (_, Some(bbox)) => assert_eq!(
            item["mapView"],
            json!({"west": bbox[0], "south": bbox[1], "east": bbox[2], "north": bbox[3]}),
            "{at}"
        ),
    }
    let access = properties["access"]
        .as_array()
        .map(|points| {
            points
                .iter()
                .map(|point| json!({"lat": point["lat"], "lng": point["lon"]}))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    match suggestion {
        true => assert_eq!(item.get("access"), None, "{at}"),
        false => assert_eq!(
            item.get("access").cloned().unwrap_or(json!([])),
            json!(access),
            "{at}"
        ),
    }
    if uri.contains("/v1/autosuggest") || uri.contains("/v1/autocomplete?") {
        let label = properties["highlights"].clone();
        let label = if label.is_null() { json!([]) } else { label };
        assert_eq!(item["highlights"]["address"]["label"], label, "{at}");
        // A suggestion's title is written anew, and its marks hold the
        // texts the label's marks hold.
        let texts = |text: &str, marks: &Value| {
            let chars = text.chars().collect::<Vec<_>>();
            let mut texts = marks
                .as_array()
                .expect("marks")
                .iter()
                .map(|mark| {
                    let (start, end) = (
                        mark["start"].as_u64().expect("start"),
                        mark["end"].as_u64().expect("end"),
                    );
                    chars[start as usize..end as usize]
                        .iter()
                        .collect::<String>()
                        .to_lowercase()
                })
                .collect::<Vec<_>>();
            texts.sort();
            texts
        };
        if suggestion {
            assert_eq!(
                texts(
                    item["title"].as_str().unwrap_or_default(),
                    &item["highlights"]["title"]
                ),
                texts(properties["label"].as_str().unwrap_or_default(), &label),
                "{at}"
            );
        }
        // Elsewhere the title holds the marks that lie in it when the label
        // starts with it.
        let title = item["title"].as_str().unwrap_or_default();
        let length = title.chars().count() as u64;
        let in_title = match item["address"]["label"].as_str().unwrap_or_default().starts_with(title) {
            true => label
                .as_array()
                .expect("marks")
                .iter()
                .filter(|mark| mark["start"].as_u64() < Some(length))
                .map(|mark| json!({"start": mark["start"], "end": mark["end"].as_u64().map(|end| end.min(length))}))
                .collect(),
            false => Vec::new(),
        };
        if !suggestion {
            assert_eq!(item["highlights"]["title"], json!(in_title), "{at}");
        }
    } else {
        assert_eq!(item.get("highlights"), None, "{at}");
    }
    if uri.contains("/v1/geocode") {
        // Each stated part as HERE scores a field: 1 matched, 0.5 partial,
        // left out unmatched; the query score is their mean with 0 for an
        // unmatched part.
        let score = |part: &Value| match part.as_str() {
            Some("matched") => Some(1.0),
            Some("partial") => Some(0.5),
            Some("unmatched") => Some(0.0),
            _ => None,
        };
        let matched = &properties["match"];
        let parts = [
            "country",
            "region",
            "locality",
            "street",
            "housenumber",
            "postcode",
            "unit",
        ]
        .map(|part| score(&matched[part]));
        let stated = parts.iter().flatten().collect::<Vec<_>>();
        let field = &item["scoring"]["fieldScore"];
        for (part, slot) in [
            ("country", "country"),
            ("region", "state"),
            ("locality", "city"),
            ("housenumber", "houseNumber"),
            ("postcode", "postalCode"),
            ("unit", "unit"),
        ] {
            let expected = score(&matched[part]).filter(|score| *score > 0.0);
            assert_eq!(field[slot].as_f64(), expected, "{at} {part}");
        }
        let street = score(&matched["street"]).filter(|score| *score > 0.0);
        assert_eq!(field["streets"][0].as_f64(), street, "{at} street");
        // A result og judged on no part takes its calibrated confidence, and
        // with neither there is no score.
        let query_score = match stated.is_empty() {
            false => Some(stated.iter().copied().sum::<f64>() / stated.len() as f64),
            true => properties["confidence"].as_f64(),
        };
        assert_eq!(item["scoring"]["queryScore"].as_f64(), query_score, "{at}");
    } else {
        assert_eq!(item.get("scoring"), None, "{at}");
    }
}

/// The slots of an item that hold a value, of those M2 fills from og.
fn slots_filled(item: &Value) -> Vec<&'static str> {
    let present = |value: &Value| match value {
        Value::Null => false,
        Value::Array(values) => !values.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
        _ => true,
    };
    [
        ("access", &item["access"]),
        ("address.countryCode", &item["address"]["countryCode"]),
        ("address.stateCode", &item["address"]["stateCode"]),
        ("contacts", &item["contacts"]),
        ("distance", &item["distance"]),
        (
            "highlights.address.label",
            &item["highlights"]["address"]["label"],
        ),
        ("highlights.title", &item["highlights"]["title"]),
        ("mapView", &item["mapView"]),
        ("openingHours", &item["openingHours"]),
        ("scoring.fieldScore", &item["scoring"]["fieldScore"]),
        ("scoring.queryScore", &item["scoring"]["queryScore"]),
    ]
    .into_iter()
    .filter(|(_, value)| present(value))
    .map(|(slot, _)| slot)
    .collect()
}

/// Creates a geocode job of `input` and waits for it to end. Answers its
/// id.
pub(super) async fn finished_job(router: &Router, input: &str) -> String {
    let created = send(
        router,
        "POST",
        "/compat/here/v7/batch/jobs",
        Some(("text/plain", input)),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    let id = created.json()["id"].as_str().expect("id").to_string();
    finished(router, &id).await;
    id
}

/// Polls a batch job until it ends, and answers its details.
async fn finished(router: &Router, id: &str) -> Value {
    for _ in 0..400 {
        let details = get(router, &format!("/compat/here/v7/batch/jobs/{id}"))
            .await
            .json();
        if !matches!(
            details["status"].as_str(),
            Some("inProgress" | "queued" | "pending")
        ) {
            return details;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("job {id} did not end");
}

/// A file's lines, the header first and the rest sorted. A job writes its
/// records in the order they finish.
fn sorted_lines(text: &str) -> Vec<String> {
    let mut lines = text.lines().map(str::to_string).collect::<Vec<_>>();
    lines[1..].sort();
    lines
}

const GEOCODE_HRN: &str = "hrn:here:service::olp-here:search-geocode-7";
const REVGEOCODE_HRN: &str = "hrn:here:service::olp-here:search-revgeocode-7";

#[tokio::test]
async fn a_geocode_batch_runs_on_og_jobs() {
    let pack = pack("batch-geocode");
    let router = pack.router();
    let input = "recId|q|country=CAN\n\
                 a1|10 King Street\n\
                 a2|zzzz qqqq xxxx\n\
                 bad id!|10 King Street\n\
                 a3|10 King Street|USA\n\
                 a4|\n";
    let created = send(
        &router,
        "POST",
        &format!(
            "/compat/here/v7/batch/jobs?serviceHrn={GEOCODE_HRN}&tag=nightly\
             &outputColumns=addressPostalCode%7ChouseNumberType%7Cbogus%7Ctitle"
        ),
        Some(("text/plain", input)),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    let created = created.json();
    let id = created["id"].as_str().expect("id").to_string();
    let uuid = id.strip_prefix("JOB-").expect("JOB- id").replace('-', "");
    let href = format!("http://og.test/compat/here/v7/batch/jobs/{id}");
    assert_eq!(created["serviceHrn"], GEOCODE_HRN);
    assert_eq!(created["tag"], "nightly");
    assert_eq!(created["billingTags"], json!([]));
    assert_eq!(created["href"], href);

    let details = finished(&router, &id).await;
    let created_at = details["created"].clone();
    let started_at = details["started"].clone();
    let ended_at = details["ended"].clone();
    assert!(created_at.as_str().is_some_and(|at| at.ends_with('Z')));
    assert_eq!(
        details,
        json!({
            "id": id,
            "serviceHrn": GEOCODE_HRN,
            "tag": "nightly",
            "billingTags": [],
            "status": "completed",
            "href": href,
            "created": created_at,
            "started": started_at,
            "ended": ended_at,
            "records": {
                "total": 5,
                "valid": 4,
                "invalid": 1,
                "processed": 4,
                "pending": 0,
                "succeeded": 3,
                "failed": 1,
            },
            "resultsHref": format!("{href}/results"),
            "errorsHref": format!("{href}/errors"),
            "outputType": "csv",
        })
    );

    // The run is an og job like any other, under the HERE job's UUID.
    let native = get(&router, &format!("/jobs/{uuid}")).await;
    assert_eq!(native.status, StatusCode::OK);
    let native = native.json();
    assert_eq!(native["endpoint"], "geocode");
    assert_eq!(native["status"], "completed");
    assert_eq!(
        native["summary"],
        json!({"total": 3, "succeeded": 3, "failed": 0})
    );

    let results = get(&router, &format!("/compat/here/v7/batch/jobs/{id}/results")).await;
    assert_eq!(results.status, StatusCode::OK);
    assert_eq!(results.header("content-type"), Some("text/plain"));
    let disposition = format!("attachment; filename=\"{id}-results.txt\"");
    assert_eq!(
        results.header("content-disposition"),
        Some(disposition.as_str())
    );
    assert_eq!(
        sorted_lines(&results.text()),
        [
            "recId|seqNumber|seqLength|addressPostalCode|houseNumberType|title",
            "a1|1|1|M5V 1A1|PA|10 King Street, Unit 5, Toronto, ON M5V 1A1, Canada",
            "a2|1|1|||",
            "a3|1|1|||",
        ]
    );

    let errors = get(&router, &format!("/compat/here/v7/batch/jobs/{id}/errors")).await;
    assert_eq!(errors.status, StatusCode::OK);
    let errors = errors.text();
    let lines = errors.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 3, "{errors}");
    assert_eq!(lines[0], "recId|errorCode|errorMessage");
    let message = |line: &str, prefix: &str| -> Value {
        let rest = line
            .strip_prefix(prefix)
            .unwrap_or_else(|| panic!("{line}"));
        serde_json::from_str(rest).expect("message is JSON")
    };
    assert_eq!(
        message(lines[1], "InputLine_4|999|"),
        json!({
            "status": 999,
            "recordId": "InputLine_4",
            "title": "Illegal input line",
            "cause": "The recId 'bad id!' holds a character outside [a-zA-Z0-9.,_-].",
            "action": "Please correct the recId and resubmit in new job.",
            "jobRecord": "bad id!|10 King Street",
        })
    );
    assert_eq!(
        message(lines[2], "a4|400|"),
        json!({
            "status": 400,
            "title": "Missing parameter 'q' or 'qq'",
            "action": "Provide 'q', 'qq' or qualified columns",
            "correlationId": "rid-1-6",
            "requestId": "rid-1-6",
        })
    );

    // A job that ended is neither started nor stopped again.
    for action in ["start", "stop"] {
        let uri = format!("/compat/here/v7/batch/jobs/{id}/{action}");
        let reply = send(&router, "PUT", &uri, None).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json()["status"], "completed");
    }

    let listed = get(
        &router,
        "/compat/here/v7/batch/jobs?tag=nightly&status=completed",
    )
    .await
    .json();
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["limit"], 100);
    assert_eq!(listed["items"][0]["id"], id);
    let listed = get(&router, "/compat/here/v7/batch/jobs?status=stopped").await;
    assert_eq!(
        listed.json(),
        json!({"limit": 100, "count": 0, "items": []})
    );

    let job = format!("/compat/here/v7/batch/jobs/{id}");
    let deleted = send(&router, "DELETE", &job, None).await;
    assert_eq!(
        deleted.json(),
        json!({"id": id, "serviceHrn": GEOCODE_HRN, "tag": "nightly", "status": "deleted"})
    );
    // Deleted, its record still reads by id; its results and og job are
    // gone.
    let details = get(&router, &job).await.json();
    assert_eq!(details["status"], "deleted");
    assert_eq!(details["resultsHref"], "");
    assert_eq!(details["ended"], ended_at);
    assert_eq!(details["records"]["succeeded"], 3);
    assert_eq!(details["records"]["processed"], 4);
    assert_eq!(
        get(&router, &format!("/jobs/{uuid}")).await.status,
        StatusCode::NOT_FOUND
    );
    let results = get(&router, &format!("{job}/results")).await;
    assert_eq!(results.status, StatusCode::NOT_FOUND);
    assert_eq!(results.json()["code"], "E6004042");
    let listed = get(&router, "/compat/here/v7/batch/jobs").await;
    assert_eq!(listed.json()["count"], 0);
}

#[tokio::test]
async fn a_revgeocode_batch_waits_for_its_start() {
    let pack = pack("batch-revgeocode");
    let router = pack.router();
    let input = "recId|at|in|limit\n\
                 r1|43.0,-79.0||\n\
                 r2||43.0,-79.0,1000|3\n\
                 r3|95,0||\n";
    let created = send(
        &router,
        "POST",
        &format!(
            "/compat/here/v7/batch/jobs?serviceHrn={REVGEOCODE_HRN}&startJob=false\
             &outputType=jsonl&outputFilter=title,address.city,distance&billingTags=ops1%2Bops2"
        ),
        Some(("text/plain", input)),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    let created = created.json();
    let id = created["id"].as_str().expect("id").to_string();
    let job = format!("/compat/here/v7/batch/jobs/{id}");
    assert_eq!(created["status"], "submitted");
    assert_eq!(created["billingTags"], json!(["ops1", "ops2"]));

    let details = get(&router, &job).await.json();
    assert_eq!(details["status"], "submitted");
    assert_eq!(details.get("started"), None);
    assert_eq!(details["resultsHref"], "");
    assert_eq!(details["errorsHref"], "");
    assert_eq!(details["outputType"], "jsonl");
    assert_eq!(
        details["records"],
        json!({
            "total": 3, "valid": 3, "invalid": 0, "processed": 1,
            "pending": 2, "succeeded": 0, "failed": 1,
        })
    );
    let results = get(&router, &format!("{job}/results")).await;
    assert_eq!(results.status, StatusCode::NOT_FOUND);
    assert_eq!(
        results.json(),
        json!({
            "title": "Results unavailable",
            "status": 404,
            "code": "E6004042",
            "cause": "The results of the job are unavailable at this time",
            "action": "Check that the job has completed and try again",
            "correlationId": "rid-1",
        })
    );
    let errors = get(&router, &format!("{job}/errors")).await;
    assert_eq!(errors.json()["code"], "E6004044");
    // Stopping a job that does not run does nothing.
    let stopped = send(&router, "PUT", &format!("{job}/stop"), None).await;
    assert_eq!(stopped.json()["status"], "submitted");

    let started = send(&router, "PUT", &format!("{job}/start"), None).await;
    assert_eq!(started.status, StatusCode::OK);
    let details = finished(&router, &id).await;
    assert_eq!(details["status"], "completed");
    assert_eq!(
        details["records"],
        json!({
            "total": 3, "valid": 3, "invalid": 0, "processed": 3,
            "pending": 0, "succeeded": 2, "failed": 1,
        })
    );

    let results = get(&router, &format!("{job}/results")).await;
    let mut lines = results
        .text()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("JSON line"))
        .collect::<Vec<_>>();
    lines.sort_by_key(|line| line["recId"].as_str().unwrap_or_default().to_string());
    let king = json!({
        "title": "10 King Street, Unit 5, Toronto, ON M5V 1A1, Canada",
        "address": {"city": "Toronto"},
        "distance": 0,
    });
    assert_eq!(
        lines,
        [
            json!({"recId": "r1", "results": [king.clone()]}),
            json!({"recId": "r2", "results": [
                king,
                {"title": "Entertainment District", "address": {}},
                {"title": "Old Toronto", "address": {"city": "Old Toronto"}},
            ]}),
        ]
    );
    let errors = get(&router, &format!("{job}/errors")).await.text();
    assert!(
        errors.starts_with("recId|errorCode|errorMessage\nr3|400|"),
        "{errors}"
    );
}

#[tokio::test]
async fn batch_input_takes_query_lines_and_gzip() {
    use std::io::Write;

    let pack = pack("batch-forms");
    let router = pack.router();
    // og's geocode is an address search. The bakery's name finds the street
    // it shares a word with.
    let lines = "q=10%20King%20Street&in=countryCode:CAN&recId=g1\nq=Queen%20Bakery&recId=g2\n";
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(lines.as_bytes()).expect("gzip");
    let created = send_with(
        &router,
        "POST",
        &format!("/compat/here/v7/batch/jobs?serviceHrn={GEOCODE_HRN}&outputDelimiter=%3B"),
        &[("content-encoding", "gzip")],
        Some(("text/plain", gzip.finish().expect("gzip"))),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    let id = created.json()["id"].as_str().expect("id").to_string();
    let details = finished(&router, &id).await;
    assert_eq!(details["records"]["succeeded"], 2);
    let results = get(&router, &format!("/compat/here/v7/batch/jobs/{id}/results")).await;
    assert_eq!(
        sorted_lines(&results.text()),
        [
            "recId;seqNumber;seqLength;position;addressLabel;title",
            "g1;1;1;(43,-79);10 King Street, Unit 5, Toronto, ON M5V 1A1, Canada;10 King Street, Unit 5, Toronto, ON M5V 1A1, Canada",
            "g2;1;1;(43.0005,-79.01);Queen Street 2-98 even, Toronto;Queen Street 2-98 even, Toronto",
        ]
    );
    let errors = get(&router, &format!("/compat/here/v7/batch/jobs/{id}/errors")).await;
    assert_eq!(errors.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn batch_errors_take_the_batch_shape() {
    let pack = pack("batch-errors");
    let router = pack.router();
    let batch_error = |status: u16, code: &str, title: &str, cause: &str, action: &str| {
        json!({
            "title": title,
            "status": status,
            "code": code,
            "cause": cause,
            "action": action,
            "correlationId": "rid-1",
        })
    };
    let not_found = batch_error(
        404,
        "E6004041",
        "Job not found",
        "Job with the specified ID could not be found",
        "Verify the job ID and try again",
    );
    for id in [
        "JOB-3fa85f64-5717-4562-b3fc-2c963f66afa6",
        "JOB-3FA85F64-5717-4562-B3FC-2C963F66AFA6",
        "..%2F..%2Fetc",
        "JOB-",
    ] {
        let job = format!("/compat/here/v7/batch/jobs/{id}");
        let reply = get(&router, &job).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{id}");
        assert_eq!(reply.json(), not_found, "{id}");
        let reply = send(&router, "PUT", &format!("{job}/start"), None).await;
        assert_eq!(reply.json(), not_found, "{id}");
    }

    let post = |query: String, body: &'static str| {
        let router = router.clone();
        async move {
            let uri = format!("/compat/here/v7/batch/jobs?{query}");
            send(&router, "POST", &uri, Some(("text/plain", body))).await
        }
    };
    let lookup = "serviceHrn=hrn:here:service::olp-here:search-lookup-7".to_string();
    let reply = post(lookup, "recId|id\n1|x\n").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        reply.json(),
        batch_error(
            404,
            "E6004043",
            "Service not found or is unsupported",
            "The Service with the specified HRN could not be found or is not supported at this time",
            "Check that the Service HRN was entered correctly",
        )
    );
    let reply = post("outputDelimiter=.".to_string(), "q\nKing\n").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json()["code"], "E6004005");
    let reply = post(format!("outputColumns={}", "a".repeat(1025)), "q\nKing\n").await;
    assert_eq!(reply.json()["code"], "E6004002");
    let reply = post(format!("outputColumns={}", "a".repeat(1024)), "q\nKing\n").await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let reply = post(String::new(), "recId|q\n").await;
    assert_eq!(
        reply.json(),
        batch_error(
            400,
            "E6004001",
            "Input for the Job is not defined.",
            "The upload holds no records",
            "Correct the request and try again",
        )
    );
    let reply = post("startJob=maybe".to_string(), "q\nKing\n").await;
    assert_eq!(
        reply.json()["title"],
        "Illegal input for parameter 'startJob'"
    );

    let reply = send(&router, "DELETE", "/compat/here/v7/batch/jobs", None).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json()["title"], "Missing parameter 'status'");
    let uri = "/compat/here/v7/batch/jobs?status=deleted";
    let reply = send(&router, "DELETE", uri, None).await;
    assert_eq!(reply.json()["title"], "Unsupported filtering status.");
    let reply = send(
        &router,
        "DELETE",
        "/compat/here/v7/batch/jobs?status=any",
        None,
    )
    .await;
    assert_eq!(reply.json()["total"], 1);

    // The batch API documents no 405: another method is its 404.
    let reply = send(&router, "PATCH", "/compat/here/v7/batch/jobs", None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.json()["code"], "E6004040");

    assert_eq!(
        get(&router, "/compat/here/v7/version").await.json(),
        json!({"apiVersion": "v7", "serviceVersion": env!("CARGO_PKG_VERSION")})
    );
    assert_eq!(
        get(&router, "/compat/here/v7/health").await.json(),
        json!({"status": "ok"})
    );
}

/// A server that answers every request 200 and keeps the request lines,
/// as a webhook endpoint does.
async fn hook_receiver() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = std::sync::Arc::clone(&calls);
    let app = Router::new().fallback(move |uri: axum::http::Uri| {
        let kept = std::sync::Arc::clone(&kept);
        async move {
            kept.lock().expect("calls").push(uri.to_string());
            StatusCode::OK
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{address}"), calls)
}

/// Waits for the receiver to have taken `count` calls.
async fn calls_taken(calls: &std::sync::Mutex<Vec<String>>, count: usize) -> Vec<String> {
    for _ in 0..400 {
        let taken = calls.lock().expect("calls").clone();
        if taken.len() >= count {
            return taken;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the webhook took {count} calls")
}

#[tokio::test]
async fn notifications_are_kept_and_called_when_a_job_ends() {
    let pack = pack("batch-notify");
    let (hook, calls) = hook_receiver().await;
    let allowed = || {
        pack.state(Limits::DEFAULT)
            .with_hook_hosts(vec![hook.trim_start_matches("http://").to_string()])
    };
    let refused = send(
        &pack.router(),
        "POST",
        "/compat/here/v7/batch/notifications",
        Some((
            "application/json",
            &json!({"config": {"url": format!("{hook}/done")}}).to_string(),
        )),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "no host allowed");
    let router = build_router(allowed(), None, None);
    let url = format!(
        "{hook}/done?id=${{JOB_ID}}&name=${{JOB_NAME}}&status=${{JOB_STATUS}}&ok=${{JOB_RECORDS_SUCCEEDED}}&failed=${{JOB_RECORDS_FAILED}}&total=${{JOB_RECORDS_TOTAL}}"
    );
    let body = json!({"type": "webHook", "config": {"url": url}}).to_string();
    let created = send(
        &router,
        "POST",
        "/compat/here/v7/batch/notifications",
        Some(("application/json", &body)),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    let created = created.json();
    let id = created["id"].as_str().expect("id").to_string();
    assert!(id.starts_with("NOTIFICATION-"));
    assert_eq!(
        created,
        json!({"id": id, "type": "webHook", "config": {"url": url}})
    );
    let path = format!("/compat/here/v7/batch/notifications/{id}");

    // Kept on disk. A second server on the same job directory reads it.
    let other = build_router(allowed(), None, None);
    assert_eq!(get(&other, &path).await.json(), created);
    assert_eq!(
        get(&other, "/compat/here/v7/batch/notifications")
            .await
            .json(),
        json!([created])
    );

    let reply = get(&router, &format!("{path}/test")).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
    assert_eq!(
        calls_taken(&calls, 1).await,
        [
            "/done?id=JOB-00000000-0000-0000-0000-000000000000&name=test&status=completed&ok=0&failed=0&total=0"
        ]
    );
    let used = get(&router, &path).await.json();
    assert!(
        used["lastUsedTime"]
            .as_str()
            .is_some_and(|at| at.ends_with('Z'))
    );

    let job = send(
        &router,
        "POST",
        &format!("/compat/here/v7/batch/jobs?tag=night%20run&notificationIds={id}"),
        Some((
            "text/plain",
            "recId|q\na1|10 King Street\na2|zzzz qqqq xxxx\n",
        )),
    )
    .await;
    assert_eq!(job.status, StatusCode::CREATED, "{}", job.text());
    let job = job.json()["id"].as_str().expect("job id").to_string();
    let details = finished(&router, &job).await;
    assert_eq!(details["notificationIds"], json!([id]));
    assert_eq!(
        calls_taken(&calls, 2).await[1],
        format!("/done?id={job}&name=night%20run&status=completed&ok=2&failed=0&total=2")
    );

    let replaced = send(
        &router,
        "PUT",
        &path,
        Some((
            "application/json",
            &json!({"type": "WEB_HOOK", "config": {"url": format!("{hook}/v2")}}).to_string(),
        )),
    )
    .await;
    assert_eq!(replaced.status, StatusCode::OK);
    assert_eq!(replaced.json()["config"]["url"], format!("{hook}/v2"));
    assert_eq!(replaced.json()["id"], id);

    let unknown = "NOTIFICATION-3fa85f64-5717-4562-b3fc-2c963f66afa6";
    let reply = send(
        &router,
        "POST",
        &format!("/compat/here/v7/batch/jobs?notificationIds={unknown}"),
        Some(("text/plain", "q\n10 King Street\n")),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json()["title"], "Notification not found");
    let six = ["a", "b", "c", "d", "e", "f"].join(",");
    let reply = send(
        &router,
        "POST",
        &format!("/compat/here/v7/batch/jobs?notificationIds={six}"),
        Some(("text/plain", "q\n10 King Street\n")),
    )
    .await;
    assert_eq!(reply.json()["title"], "Too many notifications");

    let reply = send(
        &router,
        "POST",
        "/compat/here/v7/batch/notifications",
        Some(("application/json", r#"{"config":{"url":"ftp://x.test"}}"#)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json()["title"],
        "Illegal input for field 'config.url'"
    );

    let reply = send(&router, "DELETE", &path, None).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let reply = get(&router, &path).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        reply.json(),
        json!({
            "title": "Notification not found",
            "status": 404,
            "code": "E6004044",
            "cause": "Notification with the specified ID could not be found",
            "action": "Verify the notification ID and try again",
            "correlationId": "rid-1",
        })
    );
    assert_eq!(
        get(&router, "/compat/here/v7/batch/notifications")
            .await
            .json(),
        json!([])
    );
    let reply = send(&router, "PATCH", &path, None).await;
    assert_eq!(reply.json()["code"], "E6004040");
}

#[tokio::test]
async fn a_start_with_no_free_slot_is_too_many_jobs() {
    let pack = pack("batch-busy");
    let state = pack.state(Limits::DEFAULT);
    let router = build_router(state.clone(), None, None);
    let slots = state.batch_slots();
    let taken = std::sync::Arc::clone(slots)
        .try_acquire_many_owned(u32::try_from(slots.available_permits()).expect("permits"))
        .expect("every slot");
    let too_many = json!({
        "title": "Too many concurrent jobs running",
        "status": 429,
        "code": "E6004291",
        "cause": "You have exceeded the number of jobs you may run concurrently",
        "action": "Stop or wait for your existing jobs to complete and then try starting the job again",
        "correlationId": "rid-1",
    });

    // Refused at once, the job is not kept.
    let input = Some(("text/plain", "q\n10 King Street\n"));
    let reply = send(&router, "POST", "/compat/here/v7/batch/jobs", input).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(reply.header("retry-after"), Some("1"));
    assert_eq!(reply.json(), too_many);
    let listed = get(&router, "/compat/here/v7/batch/jobs").await.json();
    assert_eq!(listed["count"], 0);

    // Refused a start, a submitted job stays submitted, and starts once a
    // slot frees.
    let uri = "/compat/here/v7/batch/jobs?startJob=false";
    let created = send(&router, "POST", uri, input).await.json();
    let id = created["id"].as_str().expect("id").to_string();
    let start = format!("/compat/here/v7/batch/jobs/{id}/start");
    let reply = send(&router, "PUT", &start, None).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(reply.json(), too_many);
    let details = get(&router, &format!("/compat/here/v7/batch/jobs/{id}")).await;
    assert_eq!(details.json()["status"], "submitted");
    drop(taken);
    assert_eq!(
        send(&router, "PUT", &start, None).await.status,
        StatusCode::OK
    );
    assert_eq!(finished(&router, &id).await["status"], "completed");
}

#[tokio::test]
async fn a_job_holds_the_servers_row_limit() {
    let pack = pack("batch-rows");
    let mut limits = Limits::DEFAULT;
    limits.max_job_rows = std::num::NonZeroUsize::new(2).expect("2");
    let router = build_router(pack.state(limits), None, None);
    let post = |input: &'static str| {
        let router = router.clone();
        async move {
            let body = Some(("text/plain", input));
            send(&router, "POST", "/compat/here/v7/batch/jobs", body).await
        }
    };

    let reply = post("q\nKing\nQueen\n").await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let reply = post("q\nKing\nQueen\nBakery\n").await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        reply.json(),
        json!({
            "title": "Input exceeds maximum record count limit.",
            "status": 413,
            "code": "E6004131",
            "cause": "The input holds more than 2 records",
            "action": "Split the job into smaller jobs",
            "correlationId": "rid-1",
        })
    );
    // Refused records are not looked up, and do not count against it.
    let reply = post("recId|q\nbad!|King\nok|\n1|King\n2|Queen\n").await;
    assert_eq!(reply.status, StatusCode::CREATED);
}

#[tokio::test]
async fn a_job_whose_og_job_is_gone_is_gone() {
    let pack = pack("batch-gone");
    let router = pack.router();
    let id = finished_job(&router, "q\n10 King Street\n").await;
    let uuid = id.strip_prefix("JOB-").expect("JOB- id").replace('-', "");
    let deleted = send(&router, "DELETE", &format!("/jobs/{uuid}"), None).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let reply = get(&router, &format!("/compat/here/v7/batch/jobs/{id}")).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.json()["code"], "E6004041");
}

/// HERE's `lang` names the address in its first language, read without its
/// region, and an area og holds no name in that language keeps its own.
#[tokio::test]
async fn lang_names_the_address_in_its_first_language() {
    let pack = pack("lang");
    let router = pack.router();
    let address = |reply: Reply| reply.json()["items"][0]["address"].clone();

    let german = address(
        get(
            &router,
            "/compat/here/v1/geocode?q=10+King+Street&lang=de-CH,fr",
        )
        .await,
    );
    assert_eq!(german["countryName"], "Kanada");
    assert_eq!(german["state"], "Ontario");

    let local = address(get(&router, "/compat/here/v1/geocode?q=10+King+Street").await);
    assert_eq!(local["countryName"], "Canada");

    // A script is a language of its own, so the code goes to og whole.
    for (lang, name) in [
        ("sr", "Канада"),
        ("sr-Latn", "Kanada"),
        ("sr-Cyrl", "Канада"),
    ] {
        let named = address(
            get(
                &router,
                &format!("/compat/here/v1/geocode?q=10+King+Street&lang={lang}"),
            )
            .await,
        );
        assert_eq!(named["countryName"], name, "{lang}");
    }

    let reversed = address(get(&router, "/compat/here/v1/revgeocode?at=43.0,-79.0&lang=de").await);
    assert_eq!(reversed["countryName"], "Kanada");

    let suggested = address(get(&router, "/compat/here/v1/autocomplete?q=10+King&lang=de").await);
    assert_eq!(suggested["countryName"], "Kanada");

    // Lookup answers the one item itself.
    let found = get(&router, "/compat/here/v1/discover?q=Queen+Bakery&at=43,-79")
        .await
        .json();
    let id = found["items"][0]["id"].as_str().expect("id").to_string();
    let looked_up = get(&router, &format!("/compat/here/v1/lookup?id={id}&lang=de")).await;
    assert_eq!(looked_up.json()["address"]["countryName"], "Kanada");
}

/// A batch record's `lang` column names its result as the endpoint's `lang`
/// would.
#[tokio::test]
async fn a_batch_record_takes_its_lang_column() {
    let pack = pack("batch-lang");
    let router = pack.router();
    let input = "recId|q|lang\n\
                 g1|10 King Street|de-CH,fr\n\
                 g2|10 King Street|\n";
    let created = send(
        &router,
        "POST",
        &format!(
            "/compat/here/v7/batch/jobs?serviceHrn={GEOCODE_HRN}\
             &outputColumns=addressCountryName"
        ),
        Some(("text/plain", input)),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    let id = created.json()["id"].as_str().expect("id").to_string();
    let details = finished(&router, &id).await;
    assert_eq!(details["records"]["succeeded"], 2);
    let results = get(&router, &format!("/compat/here/v7/batch/jobs/{id}/results")).await;
    assert_eq!(
        sorted_lines(&results.text()),
        [
            "recId|seqNumber|seqLength|addressCountryName",
            "g1|1|1|Kanada",
            "g2|1|1|Canada",
        ]
    );
}

/// A live answer converts from og's types and a batch job's from og's
/// JSON on disk; both must read the same.
#[tokio::test]
async fn a_feature_reads_alike_from_og_and_from_its_json() {
    use super::item::Feature;
    use crate::compat::{Args, native};

    let pack = pack("feature-alike");
    let state = pack.state(Limits::DEFAULT);
    let mut checked = 0;
    for q in [
        "10 King Street",
        "Queen Bakery",
        "Queen Street",
        "50 Queen Street",
        "Toronto",
    ] {
        let collection = native::geocode(&state, Args::new().set("q", q))
            .await
            .expect("geocode");
        for feature in &collection.features {
            let json = serde_json::to_value(feature).expect("JSON");
            let read: Feature = serde_json::from_value(json).expect("read");
            assert_eq!(Feature::from(feature), read, "{q}");
            checked += 1;
        }
    }
    let collection = native::reverse(&state, Args::new().set("lon", "-79.0").set("lat", "43.0"))
        .await
        .expect("reverse");
    for feature in &collection.features {
        let json = serde_json::to_value(feature).expect("JSON");
        let read: Feature = serde_json::from_value(json).expect("read");
        assert_eq!(Feature::from(feature), read);
        checked += 1;
    }
    assert!(checked > 5, "{checked}");
}

/// A route og cannot search along is refused, in the query string as in
/// the body, rather than answered as a search around nothing.
#[tokio::test]
async fn a_search_along_a_route_keeps_what_lies_by_it() {
    let pack = pack("route");
    let router = pack.router();
    // 55 m north of Queen Bakery, from 407 m west of it to 407 m east.
    let near = "BFkxumIv0niPAw-B";
    let far = "BFg4xsI__ooPw-BA";
    for path in ["discover", "autosuggest", "browse"] {
        let extra = if path == "browse" {
            ""
        } else {
            "&q=queen%20bakery"
        };
        let titles = |reply: &Reply| {
            reply.json()["items"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| item["title"].clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let query = get(
            &router,
            &format!("/compat/here/v1/{path}?at=43,-79&route={near}{extra}"),
        )
        .await;
        assert_eq!(query.status, StatusCode::OK, "{path}: {}", query.text());
        assert!(
            titles(&query).contains(&json!("Queen Bakery")),
            "{path}: {}",
            query.text()
        );
        let bakery = query.json()["items"]
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item["title"] == "Queen Bakery")
                    .cloned()
            })
            .expect("the bakery");
        // Along the route plus off it.
        let distance = bakery["distance"].as_i64().expect("a distance");
        assert!((455..=470).contains(&distance), "{path}: {distance}");
        let body = send(
            &router,
            "POST",
            &format!("/compat/here/v1/{path}?at=43,-79{extra}"),
            Some((
                "application/x-www-form-urlencoded",
                &format!("route={near};w=100"),
            )),
        )
        .await;
        assert_eq!(titles(&body), titles(&query), "{path}: in the body");
        let narrow = get(
            &router,
            &format!("/compat/here/v1/{path}?at=43,-79&route={near};w=50{extra}"),
        )
        .await;
        assert!(
            !titles(&narrow).contains(&json!("Queen Bakery")),
            "{path}: 55 m off a 50 m corridor"
        );
        let elsewhere = get(
            &router,
            &format!("/compat/here/v1/{path}?at=43,-79&route={far}{extra}"),
        )
        .await;
        assert_eq!(titles(&elsewhere), Vec::<Value>::new(), "{path}");
        for bad in [
            format!("{near};w=50001"),
            "B".to_string(),
            format!("{near};w=wide"),
        ] {
            let reply = get(
                &router,
                &format!("/compat/here/v1/{path}?at=43,-79&route={bad}{extra}"),
            )
            .await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{path}: {bad}");
            assert_eq!(reply.json()["title"], "Illegal input for parameter 'route'");
        }
    }
}
