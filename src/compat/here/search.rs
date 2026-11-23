//! Geocoding and Search v7: `/v1/geocode`, `/v1/revgeocode`,
//! `/v1/multi-revgeocode`, `/v1/autosuggest`, `/v1/autocomplete`,
//! `/v1/discover`, `/v1/browse` and `/v1/lookup`.

use axum::{
    body::Bytes,
    extract::{OriginalUri, RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use serde::Serialize;

use crate::{
    api::{ForwardCollection, Highlight, PartMatch, ReverseCollection},
    compat::{Args, decode_id, native},
    http::bounds::MAX_SEARCH_LIMIT,
    record::Layer,
    runtime::AppState,
    util::geo::haversine_m,
    util::{fold::search_key, geo::EARTH_RADIUS_M},
};

use super::{
    super::route::{self, Corridor},
    Area, Gs7Error, LatLng, Params, Rid,
    category::{Filter, Listed},
    item::{Feature, Item, ParsedTerm, Parsing, item},
    media_type, respond,
};

/// The most suggestions og's `/autocomplete` gives in one answer.
const MAX_AUTOCOMPLETE_LIMIT: usize = 20;

const GEOCODE_TYPES: [&str; 7] = [
    "address",
    "area",
    "city",
    "houseNumber",
    "place",
    "postalCode",
    "street",
];
const REVGEOCODE_TYPES: [&str; 6] = ["address", "area", "city", "houseNumber", "place", "street"];
const AUTOCOMPLETE_TYPES: [&str; 6] = [
    "address",
    "area",
    "city",
    "houseNumber",
    "postalCode",
    "street",
];

#[derive(Debug, Serialize)]
pub(super) struct Items {
    pub(super) items: Vec<Item>,
}

/// An autosuggest or discover answer, paged when the request set `offset`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    items: Vec<Item>,
    /// Required on autosuggest, absent from discover. og suggests no
    /// refinements of the query's words, so it is always empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    query_terms: Option<[(); 0]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<usize>,
}

fn features(collection: &ForwardCollection) -> Vec<Feature> {
    collection.features.iter().map(Feature::from).collect()
}

fn reverse_features(collection: &ReverseCollection) -> Vec<Feature> {
    collection.features.iter().map(Feature::from).collect()
}

fn args(fields: Vec<(&'static str, String)>) -> Args {
    fields
        .into_iter()
        .fold(Args::new(), |args, (name, value)| args.set(name, value))
}

/// HERE's qualified query `qq`, written `{sub-parameter}={string}[;...]`.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Qualified {
    pub(super) country: Option<String>,
    pub(super) state: Option<String>,
    pub(super) county: Option<String>,
    pub(super) city: Option<String>,
    pub(super) district: Option<String>,
    pub(super) street: Option<String>,
    pub(super) house_number: Option<String>,
    pub(super) postal_code: Option<String>,
}

impl Qualified {
    const PARTS: [&str; 8] = [
        "country",
        "state",
        "county",
        "city",
        "district",
        "street",
        "houseNumber",
        "postalCode",
    ];

    pub(super) fn parse(value: &str) -> Result<Self, Gs7Error> {
        let mut qualified = Self::default();
        for part in value.split(';').filter(|part| !part.trim().is_empty()) {
            let Some((name, text)) = part.split_once('=') else {
                return Err(Gs7Error::illegal(
                    "qq",
                    value,
                    "Provide {sub-parameter}={string}[;{sub-parameter}={string}]*",
                ));
            };
            let text = Some(text.trim().to_string()).filter(|text| !text.is_empty());
            let slot = match name.trim() {
                "country" => &mut qualified.country,
                "state" => &mut qualified.state,
                "county" => &mut qualified.county,
                "city" => &mut qualified.city,
                "district" => &mut qualified.district,
                "street" => &mut qualified.street,
                "houseNumber" => &mut qualified.house_number,
                "postalCode" => &mut qualified.postal_code,
                unknown => {
                    return Err(Gs7Error::illegal(
                        "qq",
                        value,
                        super::supported(unknown, &Self::PARTS),
                    ));
                }
            };
            *slot = text;
        }
        Ok(qualified)
    }

    /// Each field given, by HERE's name of it.
    fn fields(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("country", &self.country),
            ("state", &self.state),
            ("county", &self.county),
            ("city", &self.city),
            ("district", &self.district),
            ("street", &self.street),
            ("houseNumber", &self.house_number),
            ("postalCode", &self.postal_code),
        ]
        .into_iter()
        .filter_map(|(name, value)| value.as_deref().map(|value| (name, value)))
    }

    /// The country as an og filter, when it is given as a code. A country
    /// given by name is searched as text instead.
    fn country_code(&self) -> Option<&'static str> {
        let country = self.country.as_deref()?.trim();
        match country.len() {
            2 => crate::iso::alpha3(&country.to_ascii_uppercase()).and_then(crate::iso::alpha2_of),
            3 => crate::iso::alpha2_of(country),
            _ => None,
        }
    }
}

/// A forward geocode as HERE asks it, free text, qualified or both.
#[derive(Debug, Default)]
pub(super) struct Geocode {
    pub(super) q: Option<String>,
    pub(super) qualified: Qualified,
    pub(super) countries: Vec<&'static str>,
    pub(super) at: Option<LatLng>,
    pub(super) limit: usize,
}

impl Geocode {
    pub(super) fn conflicting_country(&self) -> bool {
        !self.countries.is_empty()
            && self
                .qualified
                .country_code()
                .is_some_and(|code| !self.countries.contains(&code))
    }

    /// og's `/geocode` parameters, or `None` when nothing is asked, with blank
    /// text and no qualified part.
    ///
    /// A street with no free text is og's structured geocode, the address
    /// fields apart. Anything else is one text. og's structured form has no
    /// field for a district or a county, and text loses none of them.
    pub(super) fn og_fields(&self) -> Option<Vec<(&'static str, String)>> {
        let qq = &self.qualified;
        let q = self.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
        if self.conflicting_country() {
            return None;
        }
        let code = qq.country_code();
        let country_name = qq.country.as_deref().filter(|_| code.is_none());
        let mut fields = Vec::new();
        let number_and_street = [qq.house_number.as_deref(), qq.street.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        if q.is_none()
            && qq.street.is_some()
            && qq.district.is_none()
            && qq.county.is_none()
            && country_name.is_none()
        {
            fields.push(("address", number_and_street));
            let parts = [
                ("locality", &qq.city),
                ("region", &qq.state),
                ("postcode", &qq.postal_code),
            ];
            for (name, value) in parts {
                if let Some(value) = value {
                    fields.push((name, value.clone()));
                }
            }
        } else {
            let text = [
                q,
                Some(number_and_street.as_str()),
                qq.district.as_deref(),
                qq.city.as_deref(),
                qq.county.as_deref(),
                qq.state.as_deref(),
                qq.postal_code.as_deref(),
                country_name,
            ]
            .into_iter()
            .flatten()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join(", ");
            if text.is_empty() {
                return None;
            }
            fields.push(("q", text));
            if let Some(at) = self.at {
                fields.push(("focus", at.og_point()));
            }
        }
        if let Some(code) = code {
            fields.push(("country", code.to_string()));
        } else if !self.countries.is_empty() {
            fields.push(("country", self.countries.join(",")));
        }
        fields.push(("limit", self.limit.min(MAX_SEARCH_LIMIT).to_string()));
        Some(fields)
    }
}

pub(super) async fn geocode(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
) -> Response {
    respond(&request_id, geocode_answer(&state, query.as_deref()).await)
}

async fn geocode_answer(state: &AppState, query: Option<&str>) -> Result<Items, Gs7Error> {
    let params = Params::parse(query)?;
    super::check_show(params.get("show"), &super::GEOCODE_SHOW)?;
    let qualified = params
        .get("qq")
        .map(Qualified::parse)
        .transpose()?
        .unwrap_or_default();
    if params.get("q").is_none() && params.get("qq").is_none() {
        return Err(
            Gs7Error::new(StatusCode::BAD_REQUEST, "Missing parameter 'q' or 'qq'")
                .action("Provide 'q', 'qq' or both"),
        );
    }
    let countries = match params.get("in").map(Area::parse).transpose()? {
        None => Vec::new(),
        Some(Area::Countries(countries)) => countries,
        Some(_) => {
            return Err(Gs7Error::illegal(
                "in",
                params.get("in").unwrap_or_default(),
                "Provide countryCode:{countryCode}[,{countryCode}]*",
            ));
        }
    };
    let at = params
        .get("at")
        .map(|at| LatLng::parse("at", at))
        .transpose()?;
    let limit = params.limit(20, 100)?;
    let types = params.types(&GEOCODE_TYPES)?;
    let request = Geocode {
        q: params.get("q").map(str::to_string),
        qualified,
        countries,
        at,
        // A `types` filter drops some of what og finds, so og is asked for
        // all it may give.
        limit: if types.is_some() {
            MAX_SEARCH_LIMIT
        } else if params
            .get("q")
            .is_some_and(|text| text.bytes().any(|c| c.is_ascii_digit()))
        {
            limit.max(20)
        } else {
            limit
        },
    };
    let Some(fields) = request.og_fields() else {
        return Ok(Items { items: Vec::new() });
    };
    let mut collection = native::geocode(state, args(fields).maybe("lang", og_lang(&params)))
        .await
        .map_err(Gs7Error::of)?;
    prefer_postcode_correction(&mut collection);
    collection
        .features
        .retain(|feature| item(&Feature::from(feature)).is_any_of(types.as_deref()));
    if collection.features.is_empty()
        && types.as_deref().is_some_and(|types| types == ["place"])
        && request.qualified.fields().next().is_none()
        && let Some(text) = request.q.as_deref()
    {
        let normalize = crate::util::text::normalize_for_compare;
        let asked = normalize(text);
        let args = Args::new()
            .set("q", text)
            .set("limit", request.limit.to_string())
            .maybe("lang", og_lang(&params))
            .maybe("focus", at.map(|point| point.og_point()))
            .maybe(
                "country",
                (!request.countries.is_empty()).then(|| request.countries.join(",")),
            );
        let mut places = native::places(state, args).await.map_err(Gs7Error::of)?;
        places.features.retain(|feature| {
            feature.properties.common.layer == Layer::Poi
                && normalize(&feature.properties.common.name) == asked
        });
        collection = places;
    }
    crate::compat::best_matches(&mut collection.features);
    let parsing =
        shows(&params, "parsing").then(|| parsing(state, request.q.as_deref(), &request.qualified));
    let time_zone = shows(&params, "tz");
    let items = features(&collection)
        .iter()
        .map(|feature| {
            item(feature)
                .distance_from(at)
                .with_time_zone(feature, time_zone)
                .with_name_scores(request.qualified.district.as_deref(), request.q.as_deref())
        })
        .take(limit)
        .map(|mut item| {
            item.parsing.clone_from(&parsing);
            item
        })
        .collect();
    Ok(Items { items })
}

/// How og reads `q`, each run of words of one label a part, by HERE's name
/// of the part, and each `qq` field a part of its own, where it stands in
/// the field. A word og reads as none of these is left out.
fn parsing(state: &AppState, q: Option<&str>, qualified: &Qualified) -> Parsing {
    use crate::parse::types::Label;
    let mut parsing = Parsing::new();
    if let Some(q) = q
        && let Some(parsed) = state.searcher().best_parse(q)
    {
        let mut written = Vec::new();
        let mut start = None;
        for (at, (byte, character)) in q.char_indices().enumerate() {
            match (character.is_alphanumeric(), start) {
                (true, None) => start = Some((at, byte)),
                (false, Some((from, from_byte))) => {
                    written.push((from, at, &q[from_byte..byte]));
                    start = None;
                }
                _ => {}
            }
        }
        if let Some((from, from_byte)) = start {
            written.push((from, q.chars().count(), &q[from_byte..]));
        }
        if written.len() == parsed.tokens.len() {
            let name = |label: Label| match label {
                Label::HouseNumber => Some("houseNumber"),
                Label::Unit => Some("secondaryUnits"),
                Label::Street => Some("street"),
                Label::Locality => Some("city"),
                Label::Region => Some("state"),
                Label::Postcode => Some("postalCode"),
                Label::Country => Some("country"),
                Label::Other => None,
            };
            let mut index = 0;
            while index < written.len() {
                let label = parsed.tokens[index].label;
                let mut last = index;
                while last + 1 < written.len() && parsed.tokens[last + 1].label == label {
                    last += 1;
                }
                if let Some(field) = name(label) {
                    let (from, to) = (written[index].0, written[last].1);
                    parsing.entry(field).or_default().push(ParsedTerm {
                        start: from,
                        end: to,
                        value: q.chars().skip(from).take(to - from).collect(),
                        qq: None,
                    });
                }
                index = last + 1;
            }
        }
    }
    for (field, value) in qualified.fields() {
        parsing.entry(field).or_default().push(ParsedTerm {
            start: 0,
            end: value.chars().count(),
            value: value.to_string(),
            qq: Some(field),
        });
    }
    parsing
}

// A uniquely identified house can correct the stated postcode when the
// alternative is falling back to its street.
fn prefer_postcode_correction(collection: &mut ForwardCollection) {
    if !collection.features.first().is_some_and(|first| {
        first.properties.common.layer == Layer::Street
            && first.properties.matched.as_ref().is_some_and(|matched| {
                matched.housenumber == Some(PartMatch::Unmatched)
                    && matched.street == Some(PartMatch::Matched)
                    && matched.locality == Some(PartMatch::Matched)
                    && matched.country == Some(PartMatch::Matched)
                    && matched.postcode == Some(PartMatch::Unmatched)
            })
    }) {
        return;
    }
    let mut houses = collection
        .features
        .iter()
        .enumerate()
        .filter(|(_, feature)| {
            let properties = &feature.properties;
            properties.common.layer == Layer::Address
                && properties.common.address.postcode.is_some()
                && properties.matched.as_ref().is_some_and(|matched| {
                    matched.housenumber == Some(PartMatch::Matched)
                        && matched.street == Some(PartMatch::Matched)
                        && matched.locality == Some(PartMatch::Matched)
                        && matched.country == Some(PartMatch::Matched)
                        && [matched.region, matched.unit]
                            .into_iter()
                            .all(|part| part != Some(PartMatch::Unmatched))
                })
        });
    if let Some((index, house)) = houses.next()
        && houses.next().is_none()
        && house
            .properties
            .matched
            .as_ref()
            .is_some_and(|matched| matched.postcode == Some(PartMatch::Unmatched))
    {
        let house = collection.features.remove(index);
        collection.features[0] = house;
    }
}

/// A reverse geocode as HERE asks it.
#[derive(Debug)]
pub(super) struct Reverse {
    pub(super) center: LatLng,
    /// `in=circle`'s radius, a hard filter on the distance.
    pub(super) radius_m: Option<f64>,
    /// og's layers when HERE's `types` or `bearing` asks for kinds og keeps
    /// apart. Empty for an answer of any kind.
    pub(super) layers: &'static [&'static str],
    pub(super) types: Option<Vec<String>>,
    pub(super) limit: usize,
    /// og's `lang`, from HERE's `lang`.
    pub(super) lang: Option<String>,
    /// Whether `show=tz` asks for each item's time zone.
    pub(super) time_zone: bool,
}

/// Whether `show` asks for `value`, one of the enrichments HERE adds to
/// each item only when asked.
pub(super) fn shows(params: &Params, value: &str) -> bool {
    params
        .get("show")
        .is_some_and(|show| show.split(',').any(|shown| shown.trim() == value))
}

/// og's `lang`, from HERE's. HERE renders in one language picked from its
/// list, so og names in the first. og reads a region or script it holds no
/// name in as its base language (`de` of `de-CH`), so the code goes as sent:
/// cutting it would lose `zh-Hant` from `zh-Hans`.
pub(super) fn og_lang(params: &Params) -> Option<String> {
    first_language(params.get("lang")?)
}

/// The first code of a HERE `lang` list.
pub(super) fn first_language(list: &str) -> Option<String> {
    list.split(',')
        .map(str::trim)
        .find(|code| !code.is_empty())
        .map(str::to_string)
}

impl Reverse {
    fn read(params: &Params, max_limit: usize) -> Result<Self, Gs7Error> {
        let (center, radius_m) = location(params.get("at"), params.get("in"))?;
        super::check_show(params.get("show"), &super::REVGEOCODE_SHOW)?;
        let types = params.types(&REVGEOCODE_TYPES)?;
        let bearing = params.int("bearing", 0, 359)?;
        if bearing.is_some()
            && let Some(types) = &types
            && types.iter().any(|kind| kind != "street")
        {
            return Err(Gs7Error::illegal(
                "types",
                params.get("types").unwrap_or_default(),
                "With 'bearing', the only accepted value is 'street'",
            ));
        }
        // og has no heading to match, but HERE's bearing answers streets
        // only, and that og can do.
        let layers: &'static [&'static str] = match (bearing, types.as_deref()) {
            (Some(_), _) => &["street"],
            (None, Some([kind])) => match kind.as_str() {
                "street" => &["street"],
                "houseNumber" => &["address"],
                // HERE's address results are houses and streets.
                "address" => &["address", "street"],
                "place" => &["poi"],
                "city" => &["locality"],
                _ => &[],
            },
            _ => &[],
        };
        Ok(Self {
            center,
            radius_m,
            layers,
            types: if bearing.is_some() {
                Some(vec!["street".to_string()])
            } else {
                types
            },
            limit: params.limit(1, max_limit)?,
            lang: og_lang(params),
            time_zone: shows(params, "tz"),
        })
    }

    /// og's reverse parameters, for the nearest of `layer` or, with none,
    /// for the point's answer.
    pub(super) fn og_fields(&self, layer: Option<&str>) -> Vec<(&'static str, String)> {
        let mut fields = vec![
            ("lon", self.center.lng.to_string()),
            ("lat", self.center.lat.to_string()),
            (
                "radius",
                crate::compat::radius(
                    self.radius_m
                        .unwrap_or(crate::compat::NEAREST_HOUSE_RADIUS_M),
                ),
            ),
        ];
        if let Some(layer) = layer {
            fields.push(("layer", layer.to_string()));
            // og answers the nearest of one kind as many times as asked.
            fields.push(("limit", self.limit.to_string()));
        }
        if let Some(lang) = &self.lang {
            fields.push(("lang", lang.clone()));
        }
        fields
    }
}

/// og's reverse answer as HERE's items. An area og finds the point in has
/// no distance; it lies at the point, inside any circle.
pub(super) fn reverse_items(
    features: &[Feature],
    radius_m: Option<f64>,
    types: Option<&[String]>,
    limit: usize,
    time_zone: bool,
) -> Vec<Item> {
    features
        .iter()
        .map(|feature| {
            item(feature)
                .without_scoring()
                .with_time_zone(feature, time_zone)
        })
        .filter(|item| {
            radius_m.is_none_or(|radius| {
                item.distance
                    .is_none_or(|distance| distance as f64 <= radius)
            })
        })
        .filter(|item| item.is_any_of(types))
        .take(limit)
        .collect()
}

/// The point to reverse geocode, from exactly one of `at` or `in=circle`.
fn location(at: Option<&str>, area: Option<&str>) -> Result<(LatLng, Option<f64>), Gs7Error> {
    match (at, area) {
        (Some(at), None) => Ok((LatLng::parse("at", at)?, None)),
        (None, Some(value)) => match Area::parse(value)? {
            Area::Circle { center, radius_m } => Ok((center, Some(radius_m))),
            _ => Err(Gs7Error::illegal(
                "in",
                value,
                "Provide circle:{latitude},{longitude};r={radius}",
            )),
        },
        (Some(_), Some(_)) => Err(Gs7Error::new(
            StatusCode::BAD_REQUEST,
            "Parameters 'at' and 'in' are mutually exclusive",
        )
        .action("Provide either 'at' or 'in=circle', not both")),
        (None, None) => Err(Gs7Error::new(
            StatusCode::BAD_REQUEST,
            "Missing parameter 'at' or 'in'",
        )
        .action("Provide 'at' or 'in=circle'")),
    }
}

pub(super) async fn revgeocode(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
) -> Response {
    let answer = async {
        let params = Params::parse(query.as_deref())?;
        let request = Reverse::read(&params, 100)?;
        reverse_lookup(&state, &request).await
    };
    respond(&request_id, answer.await.map(|items| Items { items }))
}

/// The answer of each og layer HERE's types ask for, nearest first.
async fn reverse_lookup(state: &AppState, request: &Reverse) -> Result<Vec<Item>, Gs7Error> {
    let mut features = Vec::new();
    if request.layers.is_empty() {
        let collection = native::reverse(state, args(request.og_fields(None)))
            .await
            .map_err(Gs7Error::of)?;
        features = reverse_features(&collection);
    }
    for layer in request.layers {
        let collection = native::reverse(state, args(request.og_fields(Some(layer))))
            .await
            .map_err(Gs7Error::of)?;
        features.extend(reverse_features(&collection));
    }
    if request.layers.len() > 1 {
        features.sort_by(|left, right| left.distance_m().total_cmp(&right.distance_m()));
    }
    Ok(reverse_items(
        &features,
        request.radius_m,
        request.types.as_deref(),
        request.limit,
        request.time_zone,
    ))
}

/// One answer of a multi reverse geocode, its items or why it has none.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum MultiResult {
    Items { id: String, items: Vec<Item> },
    Error { id: String, error: MultiError },
}

#[derive(Debug, Serialize)]
struct Multi {
    results: Vec<MultiResult>,
}

#[derive(Debug, Serialize)]
struct MultiError {
    title: String,
}

/// Sub-requests one call may carry.
const MAX_MULTI: usize = 100;

pub(super) async fn multi_revgeocode(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let answer = async {
        let accepted = media_type(&headers).is_some_and(|media_type| {
            media_type == "text/plain" || media_type == "application/x-www-form-urlencoded"
        });
        if !accepted {
            return Err(Gs7Error::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Unsupported Media Type",
            )
            .action("Send the body as text/plain or application/x-www-form-urlencoded"));
        }
        let shared = Params::parse(query.as_deref())?;
        let limit = shared.limit(1, 10)?;
        let body = std::str::from_utf8(&body).map_err(|_| {
            Gs7Error::new(StatusCode::BAD_REQUEST, "Malformed request body")
                .cause("The body is not UTF-8")
        })?;
        let lines = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        if lines.is_empty() {
            return Err(Gs7Error::new(StatusCode::BAD_REQUEST, "Empty request body")
                .action("Send one sub-request per line, such as id=1&at=50.11,8.68"));
        }
        if lines.len() > MAX_MULTI {
            return Err(Gs7Error::new(
                StatusCode::BAD_REQUEST,
                format!("Too many sub-requests: {}", lines.len()),
            )
            .action(format!("Send at most {MAX_MULTI} sub-requests per call")));
        }
        let mut results: Vec<MultiResult> = Vec::with_capacity(lines.len());
        for line in lines {
            let params = Params::parse(Some(line))?;
            let id = sub_request_id(&params, line)?;
            if results.iter().any(|result| match result {
                MultiResult::Items { id: seen, .. } | MultiResult::Error { id: seen, .. } => {
                    *seen == id
                }
            }) {
                continue;
            }
            let answer = match Reverse::read(&params, 10) {
                Ok(mut request) => {
                    request.limit = limit;
                    reverse_lookup(&state, &request).await
                }
                Err(error) => Err(error),
            };
            results.push(match answer {
                Ok(items) => MultiResult::Items { id, items },
                Err(error) => MultiResult::Error {
                    id,
                    error: MultiError { title: error.title },
                },
            });
        }
        Ok(Multi { results })
    };
    respond(&request_id, answer.await)
}

/// A sub-request's `id`, at most 64 of `[A-Za-z0-9_-]`.
fn sub_request_id(params: &Params, line: &str) -> Result<String, Gs7Error> {
    let id = params.get("id").unwrap_or_default();
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if valid {
        Ok(id.to_string())
    } else {
        Err(Gs7Error::illegal(
            "id",
            line,
            "Give every sub-request an id of at most 64 letters, digits, '_' or '-'",
        ))
    }
}

/// Where autosuggest and discover search, from `at`, `in=circle` or
/// `in=bbox`, and `in=countryCode` beside `at`.
#[derive(Debug, Default)]
struct Near {
    center: Option<LatLng>,
    area: Option<Area>,
    fields: Vec<(&'static str, String)>,
}

impl Near {
    fn read(params: &Params, required: bool) -> Result<Self, Gs7Error> {
        let at = params
            .get("at")
            .map(|at| LatLng::parse("at", at))
            .transpose()?;
        let area = params.get("in").map(Area::parse).transpose()?;
        let mut near = Self {
            center: at,
            ..Self::default()
        };
        match &area {
            Some(Area::Countries(countries)) => {
                if required && at.is_none() {
                    return Err(missing_location());
                }
                near.fields.push(("country", countries.join(",")));
            }
            Some(_) if at.is_some() => {
                return Err(Gs7Error::new(
                    StatusCode::BAD_REQUEST,
                    "Parameters 'at', 'in=circle' and 'in=bbox' are mutually exclusive",
                )
                .action("Provide only one of them"));
            }
            Some(Area::Circle { center, radius_m }) => {
                near.center = Some(*center);
                // The circle's bounding box narrows og's search; the
                // distance check on the answer makes it the circle.
                near.fields.push(("bbox", circle_bbox(*center, *radius_m)));
            }
            Some(Area::Bbox([west, south, east, north])) => {
                near.fields
                    .push(("bbox", format!("{west},{south},{east},{north}")));
            }
            None if required && at.is_none() => return Err(missing_location()),
            None => {}
        }
        if let Some(center) = near.center {
            near.fields.push(("focus", center.og_point()));
        }
        near.area = area;
        Ok(near)
    }

    fn admits(&self, item: &Item) -> bool {
        match self.area {
            Some(Area::Circle { center, radius_m }) => item.position.is_some_and(|point| {
                haversine_m(center.lng, center.lat, point.lng, point.lat) <= radius_m
            }),
            _ => true,
        }
    }

    fn admits_coordinate_result(&self, item: &Item) -> bool {
        match &self.area {
            Some(Area::Countries(countries)) => item
                .address
                .country_code
                .and_then(crate::iso::alpha2_of)
                .is_some_and(|code| countries.contains(&code)),
            Some(Area::Bbox([west, south, east, north])) => item.position.is_some_and(|point| {
                (*west..=*east).contains(&point.lng) && (*south..=*north).contains(&point.lat)
            }),
            _ => self.admits(item),
        }
    }
}

fn missing_location() -> Gs7Error {
    Gs7Error::new(
        StatusCode::BAD_REQUEST,
        "Missing parameter 'at', 'in=circle' or 'in=bbox'",
    )
    .action("Provide one of 'at', 'in=circle' or 'in=bbox'; 'in=countryCode' needs 'at' beside it")
}

/// The box around a circle, in og's `min_lon,min_lat,max_lon,max_lat`,
/// clamped to the globe. og's box cannot cross the antimeridian.
fn circle_bbox(center: LatLng, radius_m: f64) -> String {
    let angular = radius_m / EARTH_RADIUS_M;
    let lat_span = angular.to_degrees();
    let min_lat = (center.lat - lat_span).max(-90.0);
    let max_lat = (center.lat + lat_span).min(90.0);
    let (min_lon, max_lon) = if min_lat == -90.0 || max_lat == 90.0 {
        (-180.0, 180.0)
    } else {
        let lon_span = (angular.sin() / center.lat.to_radians().cos())
            .min(1.0)
            .asin()
            .to_degrees();
        let west = center.lng - lon_span;
        let east = center.lng + lon_span;
        if west < -180.0 || east > 180.0 {
            (-180.0, 180.0)
        } else {
            (west, east)
        }
    };
    format!("{min_lon},{min_lat},{max_lon},{max_lat}")
}

/// `offset` paging over what og answered. Answers the page, and the paging
/// fields HERE sends only when `offset` was asked.
fn page(items: Vec<Item>, offset: Option<usize>, limit: usize, query_terms: bool) -> Page {
    let start = offset.unwrap_or(0);
    let total = items.len();
    let items = items
        .into_iter()
        .skip(start)
        .take(limit)
        .collect::<Vec<_>>();
    let count = items.len();
    Page {
        items,
        query_terms: query_terms.then_some([]),
        offset,
        next_offset: offset
            .filter(|_| total > start + limit)
            .map(|_| start + limit),
        count: offset.map(|_| count),
        limit: offset.map(|_| limit),
    }
}

/// How many results og is asked for. That is a page past `offset`, and one
/// more to tell whether another page follows, up to the most og gives to one
/// query. A page past those is cut short or empty, as HERE answers a page
/// past the results it has.
fn fetch(offset: Option<usize>, limit: usize, max: usize) -> usize {
    match offset {
        Some(offset) => offset + limit + 1,
        None => limit,
    }
    .min(max)
}

/// HERE's widest corridor either side of a route, and the one it takes
/// when `route` names no width.
const MAX_ROUTE_WIDTH_M: u32 = 50_000;
const DEFAULT_ROUTE_WIDTH_M: u32 = 1_000;
/// The most points a route may have.
const MAX_ROUTE_POINTS: usize = 2_000;

/// `route`, a flexible polyline to search along with an optional width,
/// `{polyline};w={metres}`, from the query string or the form body a POST
/// may carry, which may hold nothing else.
fn corridor(params: &Params, body: &Bytes) -> Result<Option<Corridor>, Gs7Error> {
    let fields = serde_urlencoded::from_bytes::<Vec<(String, String)>>(body).map_err(|_| {
        Gs7Error::new(StatusCode::BAD_REQUEST, "Invalid request body")
            .action("Send the body as application/x-www-form-urlencoded")
    })?;
    let mut route = params.get("route").map(str::to_string);
    for (name, value) in fields {
        if name != "route" {
            return Err(Gs7Error::new(
                StatusCode::BAD_REQUEST,
                format!("Parameter '{name}' is not allowed in the request body"),
            )
            .action("Send every parameter but 'route' in the query string"));
        }
        if route.replace(value).is_some() {
            return Err(
                Gs7Error::new(StatusCode::BAD_REQUEST, "Duplicate parameter 'route'")
                    .action("Send 'route' once in either the query string or the body"),
            );
        }
    }
    let Some(value) = route else {
        return Ok(None);
    };
    let illegal = |reason: &str| Gs7Error::illegal("route", &value, reason);
    let (line, width) = match value.split_once(";w=") {
        Some((line, width)) => (
            line,
            width
                .parse::<u32>()
                .map_err(|_| illegal("The width 'w' is a whole number of metres"))?,
        ),
        None => (value.as_str(), DEFAULT_ROUTE_WIDTH_M),
    };
    if width > MAX_ROUTE_WIDTH_M {
        return Err(illegal("A route must not be wider than 50000 meters"));
    }
    let points =
        route::flexible_polyline(line).map_err(|_| illegal("The route is no flexible polyline"))?;
    if points.len() > MAX_ROUTE_POINTS {
        return Err(illegal("A route must not hold more than 2000 points"));
    }
    Corridor::new(points, f64::from(width))
        .map(Some)
        .ok_or_else(|| illegal("A route must hold at least two points"))
}

/// og's fields that narrow a search to the box around a route.
fn route_fields(corridor: Option<&Corridor>) -> Option<(&'static str, String)> {
    corridor.map(|corridor| {
        let [west, south, east, north] = corridor.bbox();
        ("bbox", format!("{west},{south},{east},{north}"))
    })
}

/// The items that lie along the route, each at its distance along the route
/// plus its distance off it, as HERE measures a result found along one.
fn along(items: Vec<Item>, corridor: Option<&Corridor>) -> Vec<Item> {
    let Some(corridor) = corridor else {
        return items;
    };
    items
        .into_iter()
        .filter_map(|mut item| {
            let position = item.position?;
            let reach = corridor.reach([position.lng, position.lat])?;
            item.distance = Some((reach.along_m + reach.off_m).round() as i64);
            Some(item)
        })
        .collect()
}

pub(super) async fn autosuggest(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let answer = async {
        let params = Params::parse(query.as_deref())?;
        let corridor = corridor(&params, &body)?;
        let q = required_q(&params)?;
        let near = Near::read(&params, true)?;
        let limit = params.limit(20, 100)?;
        let offset = params.int("offset", 0, 99)?;
        if q.trim().is_empty() {
            return Ok(page(Vec::new(), offset, limit, true));
        }
        let mut fields = vec![
            ("q", q.to_string()),
            (
                "limit",
                match corridor {
                    Some(_) => MAX_AUTOCOMPLETE_LIMIT,
                    None => fetch(offset, limit, MAX_AUTOCOMPLETE_LIMIT),
                }
                .to_string(),
            ),
        ];
        fields.extend(near.fields.iter().cloned());
        fields.extend(route_fields(corridor.as_ref()));
        let collection = native::autocomplete(&state, args(fields).maybe("lang", og_lang(&params)))
            .await
            .map_err(Gs7Error::of)?;
        let mut items = query_items(&state, &headers, &uri, &params, q, &collection);
        items.extend(along(
            features(&collection)
                .iter()
                .map(|feature| {
                    item(feature)
                        .without_scoring()
                        .with_details(feature)
                        .with_highlights(feature)
                        .distance_from(near.center)
                        .with_time_zone(feature, shows(&params, "tz"))
                })
                .filter(|item| near.admits(item))
                .collect(),
            corridor.as_ref(),
        ));
        Ok(page(items, offset, limit, true))
    };
    respond(&request_id, answer.await)
}

/// The searches HERE offers ahead of the places: a `categoryQuery` for each
/// category whose name the typed word begins, in the language asked, and a
/// `chainQuery` for a chain the query names whole, whose name is its own in
/// every language. Each runs as og's own `/discover`.
fn query_items(
    state: &AppState,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    params: &Params,
    q: &str,
    collection: &ForwardCollection,
) -> Vec<Item> {
    let language = og_lang(params);
    let path = uri.path();
    let prefix = path.find("/v1/").map_or("", |at| &path[..at]);
    let href = |title: &str| {
        let mut query = vec![("q", title.to_string())];
        for name in ["at", "in", "limit", "lang"] {
            if let Some(value) = params.get(name) {
                query.push((name, value.to_string()));
            }
        }
        format!(
            "{}{prefix}/v1/discover?{}",
            state.origin(headers),
            serde_urlencoded::to_string(&query).unwrap_or_default()
        )
    };
    let mut items = super::category::begun_by(q, language.as_deref())
        .into_iter()
        .map(|(id, name, start, end)| {
            Item::query(
                "categoryQuery",
                name.to_string(),
                crate::compat::encode_parts(&format!("here:category:{id}"), None, name),
                href(name),
                vec![Highlight { start, end }],
            )
        })
        .collect::<Vec<_>>();
    let typed = search_key(q);
    let mut chains = Vec::new();
    for feature in &collection.features {
        if let Some(brand) = &feature.properties.common.brand
            && search_key(&brand.name) == typed
            && !chains.contains(&brand.name)
        {
            chains.push(brand.name.clone());
        }
    }
    for name in chains {
        let length = name.chars().count();
        items.push(Item::query(
            "chainQuery",
            name.clone(),
            crate::compat::encode_parts(&format!("here:chain:{name}"), None, &name),
            href(&name),
            vec![Highlight {
                start: 0,
                end: length,
            }],
        ));
    }
    items
}

pub(super) async fn autocomplete(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
) -> Response {
    let answer = async {
        let params = Params::parse(query.as_deref())?;
        let q = required_q(&params)?;
        let near = Near::read(&params, false)?;
        let limit = params.limit(5, 20)?;
        let types = params.types(&AUTOCOMPLETE_TYPES)?;
        if q.trim().is_empty() {
            return Ok(Items { items: Vec::new() });
        }
        // og suggests places and addresses in one list; HERE's autocomplete
        // is addresses alone, so og is asked for all it gives and the places
        // are dropped.
        let mut fields = vec![
            ("q", q.to_string()),
            ("limit", MAX_AUTOCOMPLETE_LIMIT.to_string()),
        ];
        fields.extend(near.fields.iter().cloned());
        let collection = native::autocomplete(&state, args(fields).maybe("lang", og_lang(&params)))
            .await
            .map_err(Gs7Error::of)?;
        let items = features(&collection)
            .iter()
            .map(|feature| {
                item(feature)
                    .without_scoring()
                    .with_highlights(feature)
                    .titled_country_first(feature)
                    .distance_from(near.center)
            })
            .filter(|item| near.admits(item))
            .filter(|item| item.result_type != "place")
            .filter(|item| item.is_any_of(types.as_deref()))
            .take(limit)
            .map(Item::without_position)
            .collect();
        Ok(Items { items })
    };
    respond(&request_id, answer.await)
}

pub(super) async fn discover(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let answer = async {
        let params = Params::parse(query.as_deref())?;
        let corridor = corridor(&params, &body)?;
        let q = required_q(&params)?;
        let near = Near::read(&params, true)?;
        let limit = params.limit(20, 100)?;
        let offset = params.int("offset", 0, 99)?;
        if q.trim().is_empty() {
            return Ok(page(Vec::new(), offset, limit, false));
        }
        if let Some([lon, lat]) = super::super::position::parse(q) {
            let fields = vec![
                ("lon", lon.to_string()),
                ("lat", lat.to_string()),
                ("layer", "address".to_string()),
                ("radius", crate::reverse::MAX_RADIUS_M.to_string()),
            ];
            let collection = native::reverse(&state, args(fields).maybe("lang", og_lang(&params)))
                .await
                .map_err(Gs7Error::of)?;
            let items = reverse_features(&collection)
                .iter()
                .map(|feature| {
                    item(feature)
                        .without_scoring()
                        .with_details(feature)
                        .distance_from(near.center)
                        .with_time_zone(feature, shows(&params, "tz"))
                })
                .filter(|item| near.admits_coordinate_result(item))
                .collect();
            return Ok(page(along(items, corridor.as_ref()), offset, limit, false));
        }
        // Along a route og is asked for all it gives, since the corridor
        // drops what lies off it.
        let ask = match corridor {
            Some(_) => MAX_SEARCH_LIMIT,
            None => fetch(offset, limit, MAX_SEARCH_LIMIT),
        };
        let mut fields = vec![("q", q.to_string()), ("limit", ask.to_string())];
        fields.extend(near.fields.iter().cloned());
        fields.extend(route_fields(corridor.as_ref()));
        let collection = native::search(&state, args(fields).maybe("lang", og_lang(&params)))
            .await
            .map_err(Gs7Error::of)?;
        let items = features(&collection)
            .iter()
            .map(|feature| {
                item(feature)
                    .without_scoring()
                    .with_details(feature)
                    .distance_from(near.center)
                    .with_time_zone(feature, shows(&params, "tz"))
            })
            .filter(|item| near.admits(item))
            .collect();
        Ok(page(along(items, corridor.as_ref()), offset, limit, false))
    };
    respond(&request_id, answer.await)
}

/// Places around a point, by kind and by name (HERE Browse)
///
/// The answer is the places around `at`, nearest first, as far as og looks
/// around a point. `in=circle` searches that circle instead, and `in=bbox`
/// that box. `categories` keeps the kinds og has under a HERE id, and `name`
/// the places whose name holds its words, and `foodTypes` the places
/// serving them. og's places carry no HERE chain, so asking for one keeps
/// none. `route` keeps the places along it.
pub(super) async fn browse(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> Response {
    let answer = async {
        let params = Params::parse(query.as_deref())?;
        let corridor = corridor(&params, &body)?;
        let at = params.get("at").ok_or_else(|| {
            Gs7Error::new(StatusCode::BAD_REQUEST, "Missing parameter 'at'").action("Provide 'at'")
        })?;
        let at = LatLng::parse("at", at)?;
        let area = params.get("in").map(Area::parse).transpose()?;
        let limit = params.limit(20, 100)?;
        let offset = params.int("offset", 0, 99)?;
        let filter = Filter::parse(params.get("categories"))?;
        let name = params.get("name").and_then(search_key);
        // og's places carry no HERE chain, so a list that keeps only places
        // of some keeps none og holds. One that only excludes keeps them all.
        if !Listed::parse(params.get("chains")).included.is_empty() {
            return Ok(page(Vec::new(), offset, limit, false));
        }
        let food = Listed::parse(params.get("foodTypes"));
        let categories = filter.og_categories();
        if categories.is_empty() {
            return Ok(page(Vec::new(), offset, limit, false));
        }
        // What the filters drop comes out of og's answer, so og is asked for
        // all one search gives.
        let mut fields = vec![("limit", MAX_SEARCH_LIMIT.to_string())];
        match area {
            Some(Area::Countries(countries)) => {
                fields.push(("country", countries.join(",")));
                fields.push(("focus", at.og_point()));
            }
            Some(Area::Circle { center, radius_m }) => {
                fields.push(("focus", center.og_point()));
                fields.push(("radius", radius_m.to_string()));
            }
            Some(Area::Bbox([west, south, east, north])) => {
                fields.push(("bbox", format!("{west},{south},{east},{north}")));
            }
            None => fields.push(("focus", at.og_point())),
        }
        // Along a route the corridor's box is searched in place of the
        // circle around `at`.
        if let Some(field) = route_fields(corridor.as_ref()) {
            fields.retain(|(name, _)| *name != "radius");
            fields.push(field);
        }
        let collection = native::of_categories(
            &state,
            categories,
            args(fields).maybe("lang", og_lang(&params)),
        )
        .await
        .map_err(Gs7Error::of)?;
        let mut items = features(&collection)
            .iter()
            .map(|feature| {
                item(feature)
                    .without_scoring()
                    .with_details(feature)
                    .distance_from(Some(at))
                    .with_time_zone(feature, shows(&params, "tz"))
            })
            .filter(|item| filter.admits(&item.categories))
            .filter(|item| super::food_types::admits(&food, &item.food_types))
            .filter(|item| {
                name.as_deref().is_none_or(|name| {
                    search_key(&item.title).is_some_and(|title| title.contains(name))
                })
            })
            .collect::<Vec<_>>();
        items = along(items, corridor.as_ref());
        // A circle or a box is searched from its own middle, and Browse ranks
        // by the distance from `at`, or along a route.
        items.sort_by_key(|item| item.distance);
        Ok(page(items, offset, limit, false))
    };
    respond(&request_id, answer.await)
}

fn required_q(params: &Params) -> Result<&str, Gs7Error> {
    params.get("q").ok_or_else(|| {
        Gs7Error::new(StatusCode::BAD_REQUEST, "Missing parameter 'q'").action("Provide 'q'")
    })
}

pub(super) async fn lookup(
    State(state): State<AppState>,
    Rid(request_id): Rid,
    RawQuery(query): RawQuery,
) -> Response {
    let answer = async {
        let params = Params::parse(query.as_deref())?;
        let id = params.get("id").ok_or_else(|| {
            Gs7Error::new(StatusCode::BAD_REQUEST, "Missing parameter 'id'").action("Provide 'id'")
        })?;
        let not_found = || {
            Gs7Error::new(StatusCode::NOT_FOUND, "Requested id could not be found")
                .cause(format!("Actual parameter value: '{id}'"))
        };
        let og_id = decode_id(id).ok_or_else(not_found)?;
        let feature = native::by_id(&state, &og_id, og_lang(&params).as_deref())
            .await
            .map_err(Gs7Error::of)?
            .ok_or_else(not_found)?;
        let feature = Feature::from(&feature);
        Ok(item(&feature)
            .without_scoring()
            .with_details(&feature)
            .with_time_zone(&feature, shows(&params, "tz")))
    };
    respond(&request_id, answer.await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_geography_reaches_geocode() {
        let request = Geocode {
            qualified: Qualified::parse(
                "street=Main Street;district=West End;county=Orange;country=United States",
            )
            .unwrap(),
            ..Geocode::default()
        };
        let fields = request.og_fields().unwrap();
        assert_eq!(
            fields[0],
            (
                "q",
                "Main Street, West End, Orange, United States".to_string()
            )
        );
    }

    #[test]
    fn a_qualified_country_cannot_expand_the_requested_area() {
        let mut request = Geocode {
            q: Some("Main Street".to_string()),
            qualified: Qualified::parse("country=USA").unwrap(),
            countries: vec!["CA"],
            ..Geocode::default()
        };
        assert!(request.og_fields().is_none());

        request.countries.push("US");
        assert!(
            request
                .og_fields()
                .unwrap()
                .contains(&("country", "US".to_string()))
        );
    }

    #[test]
    fn circle_candidate_bounds_cover_the_dateline() {
        let center = LatLng {
            lat: 0.0,
            lng: 179.99,
        };
        let bbox = circle_bbox(center, 5_000.0);
        assert!(bbox.starts_with("-180,"));
        assert!(bbox.split(',').nth(2).is_some_and(|east| east == "180"));
        let center = LatLng {
            lat: 0.0,
            lng: 10.0,
        };
        assert!(!circle_bbox(center, 5_000.0).starts_with("-180,"));
    }
}
