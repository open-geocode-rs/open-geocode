use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
    process::{Command, Stdio},
    sync::{
        LazyLock,
        mpsc::{self, RecvTimeoutError},
    },
    time::Duration,
};

use serde::Deserialize;

use crate::{
    error::{Error, Result},
    licences,
    record::{
        AddressComponents, Brand, Contact, LocationPrecision, PoiDetails, PoiRecord,
        SourceProvenance, point_geometry,
    },
    util::{fold::search_key, geo::haversine_m},
};

use super::{Identity, Place, normalized_phone, normalized_website};

const STAC: &str = "https://stac.overturemaps.org/";
const ASSETS: &str = "https://overturemaps-us-west-2.s3.us-west-2.amazonaws.com/";
const CALENDAR: &str = "https://docs.overturemaps.org/release-calendar.json";
pub(in crate::builder) const PUBLICATION_POLICY: u8 = 4;
const SHARED_PIN_MIN_ROWS: u32 = 25;
const VERIFIED_PIN_TOLERANCE_M: f64 = 15.0;

#[derive(Debug, Clone, Default, serde::Serialize, Deserialize, PartialEq, Eq)]
pub struct Report {
    pub release: String,
    pub candidates: u64,
    pub unknown_licence: u64,
    pub closed_or_low_confidence: u64,
    pub unsupported_category: u64,
    pub invalid: u64,
    #[serde(default)]
    pub uncorroborated: u64,
    #[serde(default)]
    pub shared_pin: u64,
    pub duplicates: u64,
    pub kept: u64,
}

#[derive(Deserialize)]
struct Calendar {
    releases: Vec<Release>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Release {
    data_version: String,
    schema_version: Option<String>,
}

#[derive(Deserialize)]
struct Collection {
    links: Vec<Link>,
}

#[derive(Deserialize)]
struct Link {
    rel: String,
    href: String,
}

#[derive(Deserialize)]
struct Item {
    bbox: [f64; 4],
    assets: Assets,
}

#[derive(Deserialize)]
struct Assets {
    aws: Asset,
}

#[derive(Deserialize)]
struct Asset {
    href: String,
}

pub(in crate::builder) fn latest_release() -> Result<String> {
    let status = Command::new("duckdb")
        .args(["-no-agent", "-version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .map_err(|error| duckdb_start_error(&error))?;
    if !status.success() {
        return Err(Error::BadInput(format!(
            "DuckDB version check failed ({status})"
        )));
    }
    current_release(json(&agent(), CALENDAR)?)
}

fn current_release(calendar: Calendar) -> Result<String> {
    calendar
        .releases
        .into_iter()
        .filter(|release| {
            release.schema_version.as_deref() == Some("v2.0.0")
                && release.data_version.len() == 12
                && release
                    .data_version
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'.'))
        })
        .map(|release| release.data_version)
        .max()
        .ok_or_else(|| {
            Error::BadInput(
                "Overture has no current Places release with supported schema v2.0.0".into(),
            )
        })
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .timeout_connect(Some(Duration::from_secs(15)))
        .build()
        .into()
}

fn json<T: serde::de::DeserializeOwned>(agent: &ureq::Agent, url: &str) -> Result<T> {
    let fetch = |error: ureq::Error| Error::Fetch {
        url: url.into(),
        message: error.to_string(),
    };
    agent
        .get(url)
        .call()
        .map_err(&fetch)?
        .body_mut()
        .read_json()
        .map_err(fetch)
}

fn intersects(a: &[f64; 4], b: &[f64; 4]) -> bool {
    a[0] <= b[2] && a[2] >= b[0] && a[1] <= b[3] && a[3] >= b[1]
}

fn assets(
    release: &str,
    bounds: &[[f64; 4]],
    check: &impl Fn() -> Result<()>,
) -> Result<Vec<String>> {
    let agent = agent();
    let prefix = format!("{STAC}{release}/places/place/");
    let asset_prefix = format!("{ASSETS}release/{release}/theme=places/type=place/");
    let collection: Collection = json(&agent, &format!("{prefix}collection.json"))?;
    let mut count = 0;
    let mut urls = Vec::new();
    for link in collection
        .links
        .into_iter()
        .filter(|link| link.rel == "item")
    {
        count += 1;
        check()?;
        if !link.href.starts_with(&prefix) {
            return Err(Error::BadInput(
                "Overture returned a Places item outside the release".into(),
            ));
        }
        let item: Item = json(&agent, &link.href)?;
        if !bounds.iter().any(|query| intersects(query, &item.bbox)) {
            continue;
        }
        let url = item.assets.aws.href;
        if !url.strip_prefix(&asset_prefix).is_some_and(|key| {
            key.ends_with(".parquet")
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-_".contains(&b))
        }) {
            return Err(Error::BadInput(
                "Overture returned an invalid Places asset".into(),
            ));
        }
        urls.push(url);
    }
    if count == 0 {
        return Err(Error::BadInput(format!(
            "Overture {release} has no Places assets"
        )));
    }
    urls.sort_unstable();
    urls.dedup();
    Ok(urls)
}

fn query(urls: &[String], bounds: &[[f64; 4]]) -> String {
    let urls = urls
        .iter()
        .map(|url| format!("'{url}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let filter = bounds.iter().map(|[west, south, east, north]| {
        format!("(bbox.xmax >= {west} AND bbox.xmin <= {east} AND bbox.ymax >= {south} AND bbox.ymin <= {north})")
    }).collect::<Vec<_>>().join(" OR ");
    format!(
        "SET threads=1;
         SET memory_limit='256MiB';
         SET read_ahead_depth=0;
         SET enable_progress_bar=false;
         INSTALL httpfs;
         LOAD httpfs;
         SET http_timeout=30;
         INSTALL spatial;
         LOAD spatial;
         COPY (
             WITH shared_pins AS (
                 SELECT bbox.xmin AS x, bbox.ymin AS y
                 FROM read_parquet([{urls}])
                 WHERE {filter}
                 GROUP BY x, y
                 HAVING count(*) >= {SHARED_PIN_MIN_ROWS}
                    AND min(hash(lower(trim(names.primary)))) <> max(hash(lower(trim(names.primary))))
             )
             SELECT id, ST_X(geometry) AS lon, ST_Y(geometry) AS lat, names, basic_category,
                    confidence, operating_status, shared_pins.x IS NOT NULL AS shared_pin,
                    list_transform(sources, lambda source: {{
                        'property': source.property, 'dataset': source.dataset, 'licence': source.license
                    }}) AS sources,
                    addresses[1] AS address, brand, phones[1] AS phone, websites[1] AS website, emails[1] AS email
             FROM read_parquet([{urls}]) AS places
             LEFT JOIN shared_pins ON places.bbox.xmin = shared_pins.x AND places.bbox.ymin = shared_pins.y
             WHERE {filter}
         ) TO STDOUT (FORMAT JSON);"
    )
}

fn duckdb_start_error(error: &std::io::Error) -> Error {
    Error::BadInput(format!(
        "--overture needs the DuckDB CLI (duckdb on PATH): {error}"
    ))
}

fn read_places(
    sql: &str,
    scratch: &Path,
    check: &impl Fn() -> Result<()>,
    mut row: impl FnMut(RawPlace) -> Result<()>,
) -> Result<()> {
    let mut child = Command::new("duckdb")
        .args(["-batch", "-bail", "-no-init", "-jsonlines", "-c", sql])
        .current_dir(scratch)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| duckdb_start_error(&error))?;
    let output = child.stdout.take().expect("DuckDB output is piped");
    std::thread::scope(|scope| {
        let (send, received) = mpsc::sync_channel(64);
        scope.spawn(move || {
            for line in BufReader::new(output).lines() {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let result: Result<()> = (|| {
            loop {
                check()?;
                match received.recv_timeout(Duration::from_millis(200)) {
                    Ok(line) => row(serde_json::from_str(&line?)?)?,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            Ok(())
        })();
        drop(received);
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait();
        result?;
        let status = status?;
        if !status.success() {
            return Err(Error::BadInput(format!(
                "DuckDB failed reading Overture Places ({status})"
            )));
        }
        Ok(())
    })
}

pub(in crate::builder) fn import(
    release: &str,
    bounds: &[[f64; 4]],
    scratch: &Path,
    verified: &VerifiedPlaces,
    check: impl Fn() -> Result<()>,
    mut emit: impl FnMut(Place) -> Result<()>,
) -> Result<Report> {
    let mut report = Report {
        release: release.into(),
        ..Report::default()
    };
    let urls = assets(release, bounds, &check)?;
    if !urls.is_empty() {
        read_places(&query(&urls, bounds), scratch, &check, |row| {
            if let Some(place) = row.place(bounds, verified, &mut report) {
                emit(place)?;
            }
            Ok(())
        })?;
    }
    Ok(report)
}

#[derive(Default)]
pub(in crate::builder) struct VerifiedPlaces(BTreeMap<uuid::Uuid, Verification>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerificationFile {
    release: String,
    places: Vec<Verification>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Verification {
    id: uuid::Uuid,
    name: String,
    lon: f64,
    lat: f64,
    evidence: String,
}

impl VerifiedPlaces {
    pub(in crate::builder) fn load(path: &Path, release: &str) -> Result<Self> {
        let failed = |error: &dyn std::fmt::Display| {
            Error::BadInput(format!(
                "Overture verification file {}: {error}",
                path.display()
            ))
        };
        let file = File::open(path).map_err(|error| failed(&error))?;
        let input: VerificationFile =
            serde_json::from_reader(BufReader::new(file)).map_err(|error| failed(&error))?;
        if input.release != release {
            return Err(failed(&format!(
                "expected release {release}, found {}",
                input.release
            )));
        }
        let mut places = BTreeMap::new();
        for row in input.places {
            let id = row.id;
            let evidence = row.evidence.parse::<ureq::http::Uri>().ok();
            if !evidence.is_some_and(|uri| {
                matches!(uri.scheme_str(), Some("http" | "https")) && uri.host().is_some()
            }) || !(-180.0..=180.0).contains(&row.lon)
                || !(-90.0..=90.0).contains(&row.lat)
                || search_key(&row.name).is_none()
            {
                return Err(failed(&format!("invalid evidence for {id}")));
            }
            if places.insert(id, row).is_some() {
                return Err(failed(&format!("duplicate evidence for {id}")));
            }
        }
        Ok(Self(places))
    }

    fn confirms(&self, id: &uuid::Uuid, name: &str, lon: f64, lat: f64) -> bool {
        self.0.get(id).is_some_and(|review| {
            search_key(name) == search_key(&review.name)
                && haversine_m(lon, lat, review.lon, review.lat) <= VERIFIED_PIN_TOLERANCE_M
        })
    }
}

#[derive(Deserialize)]
struct RawPlace {
    id: String,
    lon: Option<f64>,
    lat: Option<f64>,
    names: Option<Names>,
    basic_category: Option<String>,
    confidence: Option<f64>,
    operating_status: Option<String>,
    shared_pin: bool,
    sources: Option<Vec<Source>>,
    address: Option<RawAddress>,
    brand: Option<RawBrand>,
    phone: Option<String>,
    website: Option<String>,
    email: Option<String>,
}

#[derive(Deserialize)]
struct Source {
    property: Option<String>,
    dataset: Option<String>,
    licence: Option<String>,
}

impl Source {
    fn root_is(&self, dataset: &str) -> bool {
        self.property.as_deref() == Some("") && self.dataset.as_deref() == Some(dataset)
    }

    fn overrides_store_location(&self) -> bool {
        self.dataset.as_deref() != Some("AllThePlaces")
            && self.property.as_deref().is_some_and(|property| {
                [
                    "/geometry",
                    "/properties/addresses",
                    "/properties/names",
                    "/properties/brand",
                ]
                .iter()
                .any(|prefix| {
                    property == *prefix
                        || property
                            .strip_prefix(prefix)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                })
            })
    }
}

#[derive(Deserialize)]
struct Names {
    primary: Option<String>,
    common: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct RawBrand {
    names: Option<Names>,
    wikidata: Option<String>,
}

fn names_brand(name: &str, brand: &RawBrand) -> bool {
    search_key(name)
        .zip(
            brand
                .names
                .as_ref()
                .and_then(|names| search_key(names.primary.as_deref()?)),
        )
        .is_some_and(|(name, brand)| {
            name.match_indices(&brand).any(|(start, _)| {
                let end = start + brand.len();
                (start == 0 || name.as_bytes()[start - 1] == b' ')
                    && (end == name.len() || name.as_bytes()[end] == b' ')
            })
        })
}

#[derive(Deserialize)]
struct RawAddress {
    country: Option<String>,
    freeform: Option<String>,
    locality: Option<String>,
    region: Option<String>,
    postcode: Option<String>,
}

fn dataset(sources: &[Source]) -> Option<String> {
    let mut names = Vec::new();
    let mut root = false;
    for source in sources {
        let dataset = source.dataset.as_deref()?;
        licences::overture_source(dataset, source.licence.as_deref()?)?;
        root |= source.property.as_deref() == Some("");
        names.push(dataset);
    }
    names.sort_unstable();
    names.dedup();
    root.then(|| format!("{}{}", licences::OVERTURE_PREFIX, names.join("+")))
}

impl RawAddress {
    fn has_building_number(&self) -> bool {
        static NUMBERED: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(concat!(
                r"(?xi)",
                r"^\p{Nd}+(?:[-/]\p{Nd}+)*\p{L}?(?:\s+|\s*,\s*)\p{L}",
                r"|[\p{Han}\p{Hiragana}\p{Katakana}].*\p{Nd}+(?:[\p{Pd}ー]\p{Nd}+|丁目\s*\p{Nd}+|番(?:地|号|$))",
            ))
            .expect("building number patterns are valid")
        });
        static TRAILING: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(
                r"(?i)(\p{L}+)\.?(?:\s+|\s*,\s*)(?:(?:nro\.|no\.|n[°º]|#)\s*)?\p{Nd}+(?:[-/]\p{Nd}+)*\p{L}?$",
            )
                .expect("trailing building number pattern is valid")
        });
        let Some(address) = self.freeform.as_deref().map(str::trim) else {
            return false;
        };
        if !address.chars().any(char::is_alphabetic) {
            return false;
        }
        NUMBERED.is_match(address)
            || address
                .match_indices(',')
                .map(|(end, _)| &address[..end])
                .chain(std::iter::once(address))
                .take(2)
                .any(|prefix| {
                    TRAILING.captures(prefix.trim()).is_some_and(|number| {
                        ![
                            "airport", "terminal", "gate", "floor", "level", "unit", "suite", "box",
                        ]
                        .iter()
                        .any(|part| number[1].eq_ignore_ascii_case(part))
                    })
                })
    }

    fn components(self) -> Option<AddressComponents> {
        let country = self
            .country
            .filter(|code| matches!(code.as_str(), "US" | "CA" | "GB" | "AU" | "NZ"))?;
        let freeform = self.freeform?;
        let (number, street) = freeform.trim().split_once(char::is_whitespace)?;
        let street = street.trim();
        if !number.bytes().all(|b| b.is_ascii_digit())
            || street.is_empty()
            || street.contains([',', '#', '/'])
        {
            return None;
        }
        Some(AddressComponents {
            number: number.into(),
            street: Some(street.into()),
            place: None,
            unit: None,
            locality: self.locality,
            areas: Vec::new(),
            region: self.region,
            postcode: self.postcode,
            country: Some(country),
            block_number_len: None,
        })
    }
}

impl Names {
    fn names(self) -> Option<(String, Vec<String>)> {
        let name = self.primary?;
        let name = name.trim();
        search_key(name)?;
        let mut alternate = self
            .common
            .into_iter()
            .flat_map(BTreeMap::into_values)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.eq_ignore_ascii_case(name) && search_key(value).is_some())
            .collect::<Vec<_>>();
        alternate.sort_unstable();
        alternate.dedup();
        Some((name.into(), alternate))
    }
}

impl RawPlace {
    fn place(
        self,
        bounds: &[[f64; 4]],
        verified: &VerifiedPlaces,
        report: &mut Report,
    ) -> Option<Place> {
        let Some((lon, lat)) = self.lon.zip(self.lat) else {
            report.candidates += 1;
            report.invalid += 1;
            return None;
        };
        if !bounds.iter().any(|b| intersects(b, &[lon, lat, lon, lat])) {
            return None;
        }
        report.candidates += 1;
        let sources = self.sources.as_deref().unwrap_or_default();
        let Some(dataset) = dataset(sources) else {
            report.unknown_licence += 1;
            return None;
        };
        if !self
            .confidence
            .is_some_and(|score| (0.77..=1.0).contains(&score))
            || !matches!(self.operating_status.as_deref(), None | Some("open"))
        {
            report.closed_or_low_confidence += 1;
            return None;
        }
        let Some(category) = self.basic_category.as_deref().and_then(category) else {
            report.unsupported_category += 1;
            return None;
        };
        let id = uuid::Uuid::parse_str(&self.id).ok();
        let pair = self.names.and_then(Names::names).zip(id);
        let Some(((name, alt_names), id)) = pair else {
            report.invalid += 1;
            return None;
        };
        let reviewed = verified.confirms(&id, &name, lon, lat);
        if self.confidence.is_some_and(|score| score < 0.80)
            && !(reviewed && sources.iter().any(|source| source.root_is("Foursquare")))
        {
            report.closed_or_low_confidence += 1;
            return None;
        }
        if !reviewed {
            if self.shared_pin {
                report.shared_pin += 1;
                return None;
            }
            let store_locator = sources.iter().any(|source| source.root_is("AllThePlaces"))
                && !sources.iter().any(Source::overrides_store_location)
                && self
                    .brand
                    .as_ref()
                    .is_some_and(|brand| names_brand(&name, brand))
                && self
                    .address
                    .as_ref()
                    .is_some_and(RawAddress::has_building_number);
            if !store_locator {
                report.uncorroborated += 1;
                return None;
            }
        }
        let identity = Identity {
            phones: self
                .phone
                .as_deref()
                .and_then(normalized_phone)
                .into_iter()
                .collect(),
            websites: self
                .website
                .as_deref()
                .and_then(normalized_website)
                .into_iter()
                .collect(),
            ..Identity::default()
        };
        let brand = self.brand.and_then(|brand| {
            let name = brand.names?.primary?;
            search_key(&name)?;
            let wikidata = brand
                .wikidata
                .as_deref()
                .and_then(|id| id.strip_prefix('Q')?.parse().ok());
            Some(Brand { name, wikidata })
        });
        let mut source = SourceProvenance::imported(dataset, crate::record::gers_object_id(&id));
        source.set_gers(id.to_string());
        Some(Place {
            identity,
            record: PoiRecord {
                name,
                alt_names,
                category: category.into(),
                address: self.address.and_then(RawAddress::components),
                geometry: point_geometry(lon, lat),
                location_precision: LocationPrecision::Point,
                source,
                prominence: Default::default(),
                wikidata: None,
                brand,
                details: PoiDetails::stated(
                    Contact::stated(self.phone, self.website, self.email),
                    None,
                ),
                aliases: Vec::new(),
            },
        })
    }
}

fn category(value: &str) -> Option<&'static str> {
    Some(match value {
        "restaurant"
        | "casual_eatery"
        | "pizza_restaurant"
        | "american_restaurant"
        | "chinese_restaurant"
        | "indian_restaurant"
        | "italian_restaurant"
        | "japanese_restaurant"
        | "mexican_restaurant"
        | "thai_restaurant"
        | "vietnamese_restaurant"
        | "seafood_restaurant" => "amenity:restaurant",
        "cafe" | "coffee_shop" => "amenity:cafe",
        "fast_food" | "fast_food_restaurant" => "amenity:fast_food",
        "bar" | "cocktail_bar" | "wine_bar" => "amenity:bar",
        "pub" => "amenity:pub",
        "ice_cream_shop" => "amenity:ice_cream",
        "bakery" => "shop:bakery",
        "supermarket" | "grocery_store" | "food_and_beverage_store" => "shop:supermarket",
        "convenience_store" => "shop:convenience",
        "clothing_store" | "fashion_and_apparel_store" => "shop:clothes",
        "bookstore" | "book_store" => "shop:books",
        "florist" => "shop:florist",
        "hardware_store" => "shop:hardware",
        "department_store" => "shop:department_store",
        "shopping_mall" => "shop:mall",
        "hotel" => "tourism:hotel",
        "motel" => "tourism:motel",
        "hostel" => "tourism:hostel",
        "museum" => "tourism:museum",
        "art_gallery" => "tourism:gallery",
        "public_library" | "library" => "amenity:library",
        "hospital" => "amenity:hospital",
        "pharmacy" => "amenity:pharmacy",
        "dental_clinic" | "dentist" => "amenity:dentist",
        "urgent_care_clinic" | "medical_clinic" => "amenity:clinic",
        "school" | "elementary_school" | "high_school" => "amenity:school",
        "college_university" | "university" => "amenity:university",
        "bank" | "bank_or_credit_union" => "amenity:bank",
        "post_office" => "amenity:post_office",
        "gas_station" => "amenity:fuel",
        "cinema" | "movie_theater" => "amenity:cinema",
        "theater" | "performing_arts_theater" => "amenity:theatre",
        "park" => "leisure:park",
        "playground" => "leisure:playground",
        "fitness_center" | "gym" => "leisure:fitness_centre",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{
        builder::osm::resolve_pois,
        pack::{
            Reader, Writer,
            ids::{self, Catalogue},
        },
        record::{ObjectType, Record},
        test_support::TempDir,
    };

    use super::*;

    const BOUNDS: [[f64; 4]; 1] = [[-123.0, 47.0, -122.0, 48.0]];
    const GERS: &str = "99003ee6-e75b-4dd6-8a8a-53a5a716c50d";

    fn feature() -> Value {
        json!({
            "id": GERS,
            "lon": -122.32,
            "lat": 47.6,
            "names": {"primary": "North Star Cafe", "common": {"fr": "Café North Star"}},
            "basic_category": "coffee_shop",
            "confidence": 0.95,
            "operating_status": "open",
            "shared_pin": false,
            "sources": [
                {"property": "", "dataset": "meta", "licence": "CDLA-Permissive-2.0"},
                {"property": "/properties/phones", "dataset": "Foursquare", "licence": "Apache-2.0"}
            ],
            "phone": "+12065550111"
        })
    }

    fn decode(row: Value, report: &mut Report) -> Option<Place> {
        decode_with(row, &reviewed(), report)
    }

    fn decode_with(row: Value, verified: &VerifiedPlaces, report: &mut Report) -> Option<Place> {
        serde_json::from_value::<RawPlace>(row)
            .unwrap()
            .place(&BOUNDS, verified, report)
    }

    fn reviewed() -> VerifiedPlaces {
        let id = uuid::Uuid::parse_str(GERS).unwrap();
        VerifiedPlaces(BTreeMap::from([(
            id,
            Verification {
                id,
                name: "North Star Cafe".into(),
                lon: -122.32,
                lat: 47.6,
                evidence: "https://example.org/locations/north-star".into(),
            },
        )]))
    }

    #[test]
    fn the_release_calendar_selects_the_latest_supported_release() {
        let calendar = serde_json::from_value(json!({"releases": [
            {"dataVersion": "2026-10-21.0", "schemaVersion": "v2.0.0"},
            {"dataVersion": "2026-11-18.0", "schemaVersion": "v2.0.0"},
            {"dataVersion": "2026-12-16.0", "schemaVersion": null},
            {"dataVersion": "2027-01-20.0", "schemaVersion": "v3.0.0"},
            {"dataVersion": "latest", "schemaVersion": "v2.0.0"}
        ]}))
        .unwrap();
        assert_eq!(current_release(calendar).unwrap(), "2026-11-18.0");
    }

    #[test]
    fn open_licence_is_required_for_every_contributor() {
        let mut report = Report::default();
        let accepted = decode(feature(), &mut report).unwrap();
        let dataset = &accepted.record.source.dataset;
        let licence = licences::of(dataset).unwrap();
        assert_eq!(licence.name, "Apache-2.0 AND CDLA-Permissive-2.0");
        assert_eq!(
            licence.attribution.as_deref(),
            Some("Overture Maps Foundation, Foursquare, Meta")
        );
        for source in [
            json!({"property": "/phones", "dataset": "Foursquare", "licence": "CC-BY-NC-4.0"}),
            json!({"property": "/phones", "dataset": "Foursquare"}),
            json!({"property": "/phones", "dataset": "Unknown", "licence": "Apache-2.0"}),
        ] {
            let mut row = feature();
            row["sources"][1] = source;
            assert!(decode(row, &mut report).is_none());
        }
        let mut row = feature();
        row["sources"][0]["property"] = json!("/names");
        assert!(decode(row, &mut report).is_none());
        assert_eq!(report.unknown_licence, 4);
    }

    #[test]
    fn places_filters_remove_closed_low_confidence_outside_and_unmapped_records() {
        let mut report = Report::default();
        for (key, value) in [
            ("operating_status", json!("permanently_closed")),
            ("confidence", json!(0.79)),
            ("lon", json!(-125.0)),
            ("basic_category", json!("professional_service")),
        ] {
            let mut row = feature();
            row[key] = value;
            assert!(decode(row, &mut report).is_none());
        }
        assert_eq!(report.closed_or_low_confidence, 2);
        assert_eq!(report.unsupported_category, 1);
        assert_eq!(report.candidates, 3);
    }

    #[test]
    fn confidence_and_secondary_contact_fields_do_not_corroborate_places() {
        let mut report = Report::default();
        let empty = VerifiedPlaces::default();
        let mut row = feature();
        row["confidence"] = json!(1.0);
        row["website"] = json!("https://example.org/locations/north-star");
        assert!(decode_with(row.clone(), &empty, &mut report).is_none());
        row["shared_pin"] = json!(true);
        assert!(decode_with(row.clone(), &empty, &mut report).is_none());
        assert!(decode(row, &mut report).is_some());
        assert_eq!(report.uncorroborated, 1);
        assert_eq!(report.shared_pin, 1);
    }

    #[test]
    fn foursquare_floor_requires_matching_reviewed_identity_and_location() {
        let mut row = feature();
        row["confidence"] = json!(0.77);
        let mut report = Report::default();
        assert!(decode(row.clone(), &mut report).is_none());
        row["sources"] = json!([
            {"property": "", "dataset": "Foursquare", "licence": "Apache-2.0"}
        ]);
        assert!(decode(row.clone(), &mut report).is_some());
        assert!(decode_with(row.clone(), &VerifiedPlaces::default(), &mut report).is_none());
        row["sources"]
            .as_array_mut()
            .unwrap()
            .push(json!({"property": "", "dataset": "AllThePlaces", "licence": "CC0-1.0"}));
        row["brand"] = json!({"names": {"primary": "North Star Cafe"}});
        row["address"] = json!({"country": "US", "freeform": "10 Pike Street"});
        assert!(decode_with(row.clone(), &VerifiedPlaces::default(), &mut report).is_none());
        assert!(decode(row.clone(), &mut report).is_some());
        for (key, value) in [
            ("confidence", json!(0.769)),
            ("names", json!({"primary": "Another Cafe"})),
            ("lon", json!(-122.31)),
        ] {
            let mut changed = row.clone();
            changed[key] = value;
            assert!(decode(changed, &mut report).is_none());
        }
    }

    #[test]
    fn first_party_store_locator_admission_requires_an_address_and_applies_the_pin_check() {
        let empty = VerifiedPlaces::default();
        let mut report = Report::default();
        let mut row = feature();
        row["confidence"] = json!(0.8);
        row["sources"][0] =
            json!({"property": "", "dataset": "AllThePlaces", "licence": "CC0-1.0"});
        assert!(decode_with(row.clone(), &empty, &mut report).is_none());
        row["address"] = json!({"freeform": "10 Pike Street", "country": "US"});
        assert!(decode_with(row.clone(), &empty, &mut report).is_none());
        row["brand"] = json!({"names": {"primary": "Star"}});
        assert!(decode_with(row.clone(), &empty, &mut report).is_some());
        for address in [
            "Seattle-Tacoma Airport",
            "Heathrow Airport Terminal 5",
            "Heathrow Airport, Gate #5",
        ] {
            let mut no_street = row.clone();
            no_street["address"]["freeform"] = json!(address);
            assert!(
                decode_with(no_street, &empty, &mut report).is_none(),
                "{address}"
            );
        }
        for (country, address) in [
            ("BR", "Rua Gomes de Carvalho 1575"),
            ("IT", "Via della Valtiera, 5"),
            ("ID", "Jl. Ahmad Wahid No.235, RT.13"),
            ("JP", "東京都台東区浅草２－２８－１"),
        ] {
            let mut overseas = row.clone();
            overseas["address"] = json!({"country": country, "freeform": address});
            assert!(
                decode_with(overseas, &empty, &mut report).is_some(),
                "{address}"
            );
        }
        let mut unrelated = row.clone();
        unrelated["brand"] = json!({"names": {"primary": "Tar"}});
        assert!(decode_with(unrelated, &empty, &mut report).is_none());
        let mut overridden = row.clone();
        overridden["sources"][1]["property"] = json!("/properties/addresses/0");
        assert!(decode_with(overridden, &empty, &mut report).is_none());
        row["shared_pin"] = json!(true);
        assert!(decode_with(row, &empty, &mut report).is_none());
    }

    #[test]
    fn reviews_are_release_scoped_and_duplicate_ids_fail() {
        let temp = TempDir::new("overture-reviewed");
        let path = temp.join("review.json");
        let entry = json!({
            "id": GERS, "name": "North Star Cafe", "lon": -122.32, "lat": 47.6,
            "evidence": "https://example.org/locations/north-star"
        });
        let mut file = json!({"release": "2026-11-18.0", "places": [entry]});
        std::fs::write(&path, file.to_string()).unwrap();
        assert!(VerifiedPlaces::load(&path, "2026-11-18.0").is_ok());
        assert!(VerifiedPlaces::load(&path, "2026-10-21.0").is_err());
        file["places"].as_array_mut().unwrap().push(entry);
        std::fs::write(&path, file.to_string()).unwrap();
        assert!(VerifiedPlaces::load(&path, "2026-11-18.0").is_err());
    }

    #[test]
    fn same_named_branches_need_corroboration_and_osm_wins_a_match() {
        let imported = decode(feature(), &mut Report::default()).unwrap();
        let mut osm = imported.clone();
        osm.record.source = SourceProvenance::osm(ObjectType::Node, 42);
        osm.record.alt_names.clear();
        let similar = resolve_pois(vec![imported.clone(), osm.clone()]);
        assert_eq!(similar, [osm.record.clone()]);
        osm.identity.phones.clear();
        assert_eq!(resolve_pois(vec![imported, osm]).len(), 2);
    }

    #[test]
    fn conflicting_contact_fields_keep_addressless_branches_separate() {
        let mut row = feature();
        row["brand"] = json!({"names": {"primary": "North Star Cafe"}, "wikidata": "Q123"});
        let imported = decode(row, &mut Report::default()).unwrap();
        let mut osm = imported.clone();
        osm.record.source = SourceProvenance::osm(ObjectType::Node, 42);
        for (leader, member) in [
            (
                Identity {
                    phones: vec!["123456789".into()],
                    ..Identity::default()
                },
                imported.identity.clone(),
            ),
            (
                Identity {
                    websites: vec!["example.org/branch-a".into()],
                    ..Identity::default()
                },
                Identity {
                    websites: vec!["example.org/branch-b".into()],
                    ..Identity::default()
                },
            ),
            (
                Identity {
                    refs: vec!["1234".into()],
                    ..Identity::default()
                },
                Identity {
                    refs: vec!["5678".into()],
                    ..Identity::default()
                },
            ),
        ] {
            osm.identity = leader;
            let mut imported = imported.clone();
            imported.identity = member;
            assert_eq!(resolve_pois(vec![imported, osm.clone()]).len(), 2);
        }
        assert_eq!(normalized_phone("n/a"), None);
        assert_eq!(
            normalized_phone("+1 (206) 555-0111").as_deref(),
            Some("065550111")
        );
    }

    #[test]
    fn colocated_branded_places_tolerate_street_spelling_but_not_different_units() {
        let mut row = feature();
        row["brand"] = json!({"names": {"primary": "North Star Cafe"}, "wikidata": "Q123"});
        row["address"] = json!({"country": "US", "freeform": "10 South Street"});
        let mut imported = decode(row, &mut Report::default()).unwrap();
        let mut osm = imported.clone();
        osm.record.source = SourceProvenance::osm(ObjectType::Node, 42);
        let address = osm.record.address.as_mut().unwrap();
        address.street = Some("South St".into());
        address.unit = Some("1".into());
        assert_eq!(
            resolve_pois(vec![imported.clone(), osm.clone()]),
            [osm.record.clone()]
        );
        imported.record.address.as_mut().unwrap().unit = Some("2".into());
        assert_eq!(resolve_pois(vec![imported, osm]).len(), 2);
    }

    #[test]
    fn gers_id_and_licences_round_trip_through_pack_search_and_the_id_catalogue() {
        let mut feature = feature();
        feature["address"] =
            json!({"freeform": "987 Unique End Lane", "country": "US", "locality": "Seattle"});
        let place = decode(feature, &mut Report::default()).unwrap();
        let record = Record::Poi(place.record.clone());
        let mut sampler = crate::parse::lattice::train::Sampler::default();
        let before = sampler.to_bytes();
        sampler.offer(0, None, &record);
        assert_eq!(sampler.to_bytes(), before);
        let indexed =
            crate::text_index::Document::from_record(0, &record, &[], &Default::default(), None);
        assert!(indexed.address_numbers.is_empty());
        assert!(indexed.streets.is_empty());
        assert!(indexed.context_text.contains("seattle"));
        let id = format!("overture:places:{GERS}");
        let temp = TempDir::new("overture-poi");
        let root = temp.join("pack");
        let mut writer = Writer::create(&root).unwrap();
        writer.write(&record, None).unwrap();
        writer.finish().unwrap();
        let pack = Reader::open(&root).unwrap();
        pack.verify().unwrap();
        let summary = pack.record_summary(0).unwrap();
        assert_eq!(summary.id, id);
        assert_eq!(summary.name, "North Star Cafe");
        assert_eq!(summary.address.street.as_deref(), Some("Unique End Lane"));
        let names =
            crate::parse::lattice::dictionary::Dictionaries::open(pack.container()).unwrap();
        assert!(
            names
                .get(
                    crate::parse::lattice::dictionary::Field::Street,
                    "unique end lane"
                )
                .is_none()
        );
        assert_eq!(summary.category.as_deref(), Some("amenity:cafe"));
        assert_eq!(pack.records().record(0).unwrap(), Record::Poi(place.record));
        assert_eq!(
            ids::parse(&id).unwrap(),
            Some((ObjectType::Row, summary.source.object_id.unwrap()))
        );
        assert_eq!(
            pack.records().licence(&summary.source.dataset),
            licences::of(&summary.source.dataset)
        );
        let searcher = crate::search::TextSearcher::open(&root).unwrap();
        assert!(searcher.structured_keys(0).unwrap().streets.is_empty());
        let hits = searcher
            .places()
            .search(&crate::search::TextSearchOptions {
                query: "North Star Cafe".into(),
                limit: 1,
                layer: Some("poi".into()),
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .unwrap();
        assert_eq!(hits[0].record.id, id);
        let catalogue = Catalogue::open(pack.container()).unwrap().unwrap();
        assert_eq!(catalogue.find_id(pack.records(), &id).unwrap(), [0]);
        let compact = format!(
            "overture:places:{}",
            GERS.replace('-', "").to_ascii_uppercase()
        );
        assert_eq!(catalogue.find_id(pack.records(), &compact).unwrap(), [0]);
    }
}
