use std::{path::PathBuf, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::{
    compat::{encode_id, iso},
    confidence::{FEATURES, Model},
    context::{AdminContextTuple, RecordContext},
    limits::Limits,
    pack::Writer,
    record::{
        AddressComponents, AddressRecord, InterpolationAddressComponents, InterpolationRange,
        InterpolationRecord, LocationPrecision, ObjectType, PlaceLayer, PlaceRecord, PoiRecord,
        Record, SourceProvenance, StreetRecord, point_geometry,
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

fn line(from: [f64; 2], to: [f64; 2]) -> geojson::Geometry {
    geojson::Geometry::new(geojson::GeometryValue::LineString {
        coordinates: vec![from.to_vec().into(), to.to_vec().into()],
    })
}

/// Canada > Ontario > Toronto Division > Toronto > Old Toronto >
/// Entertainment District. Holds an address with only a unit and a postcode
/// stated, one outside every area stating its own town, region and areas,
/// a POI, a street and an interpolation range. Calibrated, every confidence
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
    let address = |address: AddressComponents, lon, id| {
        Record::from(AddressRecord {
            disputed: false,
            street_postcodes: Vec::new(),
            address,
            geometry: point_geometry(lon, 43.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, id),
        })
    };
    writer
        .write(&address(king.clone(), -79.0, 10), context.clone())
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
        .write(&address(queen, -79.002, 20), None)
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
        "label": "10 King Street, Unit 5, M5V 1A1",
        "countryCode": "CAN",
        "countryName": "Canada",
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
        "title": "10 King Street, Unit 5, M5V 1A1",
        "id": encode_id("osm:node:10"),
        "resultType": "houseNumber",
        "houseNumberType": "PA",
        "address": king_address(),
        "position": {"lat": 43.0, "lng": -79.0},
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
        "id": encode_id("osm:way:11"),
        "resultType": "place",
        "address": {
            "label": "Queen Bakery, 12 King Street, Old Town, M5V 1A1",
            "countryCode": "CAN",
            "countryName": "Canada",
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
    });
    if let Some(distance) = distance {
        item["distance"] = distance.into();
    }
    item
}

fn area(id: &str, title: &str, kind: (&str, &str, &str), slot: &str) -> Value {
    let (result_type, type_key, type_value) = kind;
    let mut item = json!({
        "title": title,
        "id": encode_id(id),
        "resultType": result_type,
        "address": {"label": title},
        "position": {"lat": 43.0, "lng": -79.0},
    });
    item[type_key] = type_value.into();
    item["address"][slot] = title.into();
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
        json!({"items": [king(json!({"scoring": {"queryScore": 0.5}}))]})
    );

    // `at` is og's focus and gives each item its distance from it.
    let reply = get(
        &router,
        "/compat/here/v1/geocode?q=10+King+Street&at=43.0,-79.005&in=countryCode:CAN",
    )
    .await;
    assert_eq!(
        reply.json(),
        json!({"items": [king(json!({"distance": 407, "scoring": {"queryScore": 0.5}}))]})
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
    let scored = json!({"scoring": {"queryScore": 0.5}});

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
    assert_eq!(reply.json()["items"][0], king(scored));

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
    assert_eq!(items[0]["id"], encode_id("osm:way:13:interp:2-98"));
    // A range found by its street alone is a street, as no number was asked.
    assert_eq!(items[0]["resultType"], "street");
    assert_eq!(items[0].get("houseNumberType"), None);

    let numbers = get(&router, &uri("houseNumber")).await.json();
    assert_eq!(
        numbers,
        json!({"items": [{
            "title": "20 Queen Street, Toronto City, ON, M5H 2N2",
            "id": encode_id("osm:node:20"),
            "resultType": "houseNumber",
            "houseNumberType": "PA",
            "address": queen_address(),
            "position": {"lat": 43.0, "lng": -79.002},
            "scoring": {"queryScore": 0.5},
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
            area("osm:relation:2", "Ontario", admin("state"), "state"),
            area("osm:relation:1", "Canada", admin("country"), "countryName"),
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
    assert_eq!(items[0]["id"], encode_id("osm:way:12"));
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
        "id": encode_id("osm:node:20"),
        "resultType": "houseNumber",
        "houseNumberType": "PA",
        "address": queen_address(),
    });
    let mut queen_near = queen.clone();
    queen_near["position"] = json!({"lat": 43.0, "lng": -79.002});
    queen_near["distance"] = 163.into();

    let reply = get(&router, "/compat/here/v1/autosuggest?q=Queen&at=43.0,-79.0").await;
    assert_eq!(
        reply.json(),
        json!({"items": [bakery(Some(407)), queen_near], "queryTerms": []})
    );

    // Paging fields appear only with `offset`, and `nextOffset` only before
    // the last page.
    let page = "/compat/here/v1/autosuggest?q=Queen&at=43.0,-79.0&limit=1&offset=";
    let reply = get(&router, &format!("{page}0")).await;
    assert_eq!(
        reply.json(),
        json!({
            "items": [bakery(Some(407))],
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
    assert_eq!(reply["items"][0]["id"], encode_id("osm:node:20"));

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

    // Autocomplete is addresses alone, with no position.
    let reply = get(&router, "/compat/here/v1/autocomplete?q=Queen").await;
    assert_eq!(reply.json(), json!({"items": [queen]}));
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

    for id in [encode_id("osm:way:999"), "not-an-id".to_string()] {
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

    let reply = get(
        &router,
        "/compat/here/v1/browse?at=43,-79&categories=100-1000-0000",
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
        reply.json(),
        here_error(
            501,
            "Not Implemented",
            Some("This server does not serve /compat/here/v1/browse"),
            None,
        )
    );
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
/// unchanged, across each endpoint and every layer the Pack holds.
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
    ];
    let mut layers = std::collections::BTreeSet::new();
    for (native, here) in pairs {
        let og = get(&router, native).await.json();
        let items = get(&router, &here).await.json()["items"].clone();
        let features = og["features"].as_array().expect("features");
        let items = items.as_array().expect("items");
        assert_eq!(features.len(), items.len(), "{here} dropped a result");
        assert!(!features.is_empty(), "{native} found nothing");
        for (feature, item) in features.iter().zip(items) {
            let properties = &feature["properties"];
            layers.insert(properties["layer"].as_str().unwrap_or_default().to_string());
            assert_fidelity(properties, &feature["geometry"]["coordinates"], item, &here);
        }
    }
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
    assert_eq!(item["id"], encode_id(id), "{at}");
    let title = if layer == "poi" {
        &properties["name"]
    } else {
        &properties["label"]
    };
    assert_eq!(&item["title"], title, "{at}");
    assert_eq!(item["address"]["label"], properties["label"], "{at}");
    assert_eq!(item["position"]["lat"], coordinates[1], "{at}");
    assert_eq!(item["position"]["lng"], coordinates[0], "{at}");
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
    assert_eq!(
        slot("district"),
        area("sublocality")
            .or_else(|| text(&address["areas"][0]))
            .or_else(|| own("neighbourhood")),
        "{at}"
    );
    assert_eq!(
        slot("subdistrict"),
        area("neighbourhood").or_else(|| text(&address["areas"][1])),
        "{at}"
    );
    assert_eq!(
        slot("countryName"),
        area("country").or_else(|| own("country")),
        "{at}"
    );
    assert_eq!(
        slot("countryCode"),
        context["country"]["code"]
            .as_str()
            .and_then(iso::alpha3)
            .map(str::to_string),
        "{at}"
    );
    match properties["distance_m"].as_f64() {
        Some(distance) => assert_eq!(item["distance"], distance.round() as i64, "{at}"),
        None if uri.contains("revgeocode") => assert_eq!(item.get("distance"), None, "{at}"),
        None => {}
    }
    if uri.contains("/v1/geocode") {
        assert_eq!(
            item["scoring"]["queryScore"], properties["confidence"],
            "{at}"
        );
    } else {
        assert_eq!(item.get("scoring"), None, "{at}");
    }
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
             &outputColumns=addressPostalCode,houseNumberType,bogus,title"
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
            "a1|1|1|M5V 1A1|PA|10 King Street, Unit 5, M5V 1A1",
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
        "title": "10 King Street, Unit 5, M5V 1A1",
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
            "g1;1;1;(43,-79);10 King Street, Unit 5, M5V 1A1;10 King Street, Unit 5, M5V 1A1",
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

    let reply = get(&router, "/compat/here/v7/batch/notifications").await;
    assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(reply.json()["code"], "E6005010");
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
