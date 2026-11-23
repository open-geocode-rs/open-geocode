//! The response contract of `/geocode`, `/places`, `/autocomplete` and `/reverse`
//! (ADR 0022).
//!
//! Every response is one GeoJSON FeatureCollection: a `geocoding` block that
//! says what was asked, then one Feature per answer. Forward features carry a
//! `score`, from a calibrated Pack a `confidence`, and a `distance_m` from
//! the request's `focus` when it gives one; reverse features a `distance_m`
//! from the queried point; nothing else differs. The types here are the only
//! definition of that shape. The contract tests
//! compare real answers with the committed examples in
//! `contract/responses/`, so a change to the shape lands with its examples.
//!
//! The same features are the result columns of `geocode batch`
//! ([`flat_columns`]), so a CSV row and a JSON answer never drift apart
//! either.

use std::collections::{BTreeMap, HashMap};

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    labels,
    pack::{Reader, RecordId, Summary},
    record::{
        AccessPoint, AddressComponents, Brand, Contact, InterpolationAddressComponents, Layer,
        OSM_DATASET, ObjectType, PlaceLayer, RoadClass,
    },
    records, reverse,
    reverse::{GeocodeResponse, MatchKind},
    search::TextSearchHit,
};

// Forward and reverse are written out rather than shared through a generic:
// a generic reached through a type alias is transparent to the schema
// generator, which then emitted one `FeatureCollection` for both and
// described each endpoint with the other's fields. Construction is still
// shared, by [`common`] and the builders below.
/// A GeoJSON FeatureCollection answering `/geocode`, `/places` or
/// `/autocomplete`.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ForwardCollection {
    #[serde(rename = "type")]
    pub kind: FeatureCollectionType,
    /// The query, API version and the credits of the data sets the answer
    /// comes from.
    pub geocoding: ForwardGeocoding,
    pub features: Vec<ForwardFeature>,
}

// See [`ForwardCollection`] for why the two directions are written out.
/// A GeoJSON FeatureCollection answering `/reverse`.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ReverseCollection {
    #[serde(rename = "type")]
    pub kind: FeatureCollectionType,
    /// The queried point, API version and the credits of the data sets the
    /// answer comes from.
    pub geocoding: ReverseGeocoding,
    pub features: Vec<ReverseFeature>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub enum FeatureCollectionType {
    FeatureCollection,
}

/// API version. Changes only when a field is removed or changes meaning.
/// New optional fields keep the version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub enum Version {
    #[serde(rename = "1")]
    V1,
}

/// A data set an answer comes from and how its licence asks it to be
/// credited. OpenStreetMap is always first: every answer's areas and labels
/// come from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Attribution {
    /// Whom to credit, such as `© OpenStreetMap contributors`, or the data
    /// set's name when its licence asks for no credit or is not known.
    #[schema(example = "© OpenStreetMap contributors")]
    pub text: String,
    /// The licence's name, such as `ODbL 1.0` or `CC BY 4.0`. Absent when
    /// the data set's source states none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub licence: Option<String>,
    /// The licence's terms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// The data set a result comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Source {
    /// `osm`, or an imported data set such as `openaddresses:nz/countrywide`.
    #[schema(example = "osm")]
    pub dataset: String,
}

/// The credits of an answer whose results are `results`. They are
/// OpenStreetMap, each other data set a result comes from, and the timezone
/// boundaries when a result carries a timezone.
pub(crate) fn attribution<'a>(
    pack: &Reader,
    results: impl Iterator<Item = &'a Common>,
) -> Vec<Attribution> {
    let mut named = vec![OSM_DATASET];
    let mut zoned = false;
    for result in results {
        let dataset = result.source.dataset.as_str();
        if !named.contains(&dataset) {
            named.push(dataset);
        }
        zoned |= result.timezone.is_some();
    }
    let credit = |licence: crate::licences::Licence, dataset: &str| Attribution {
        text: licence.attribution.unwrap_or_else(|| dataset.to_string()),
        licence: Some(licence.name),
        url: licence.url,
    };
    named
        .into_iter()
        .map(|dataset| match pack.records().licence(dataset) {
            Some(licence) => credit(licence, dataset),
            None => Attribution {
                text: dataset.to_string(),
                licence: None,
                url: None,
            },
        })
        .chain(zoned.then(|| credit(crate::licences::timezones(), "timezones")))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ForwardGeocoding {
    pub version: Version,
    /// Query as received, trimmed, with each run of whitespace collapsed to
    /// one space.
    pub query: String,
    pub attribution: Vec<Attribution>,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ReverseGeocoding {
    pub version: Version,
    /// Queried point.
    pub point: LonLat,
    pub attribution: Vec<Attribution>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, ToSchema)]
pub struct LonLat {
    pub lon: f64,
    pub lat: f64,
}

/// One forward geocoding result.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ForwardFeature {
    #[serde(rename = "type")]
    pub kind: FeatureType,
    pub geometry: Point,
    pub properties: ForwardProperties,
}

/// One reverse geocoding result: the address or street at the point, or an
/// area containing it.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ReverseFeature {
    #[serde(rename = "type")]
    pub kind: FeatureType,
    pub geometry: Point,
    pub properties: ReverseProperties,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub enum FeatureType {
    Feature,
}

/// GeoJSON Point. `coordinates` is `[lon, lat]`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, ToSchema)]
pub struct Point {
    #[serde(rename = "type")]
    pub kind: PointType,
    pub coordinates: [f64; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub enum PointType {
    Point,
}

// Not a response type of its own: flattened into [`ForwardProperties`] and
// [`ReverseProperties`], which add the field each direction ranks by, so both
// are one flat object on the wire.
/// Properties shared by forward and reverse results.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Common {
    #[serde(skip)]
    pub(crate) tiger_range: Option<crate::tiger::Range>,
    #[serde(skip)]
    pub(crate) tiger_block: Option<String>,
    #[serde(skip)]
    pub(crate) tiger_block_2010: Option<String>,
    #[serde(skip)]
    pub(crate) tiger_block_left: Option<String>,
    #[serde(skip)]
    pub(crate) tiger_block_right: Option<String>,
    /// Stable ID from the source data, such as `osm:node:123`. Store it to
    /// find the record again: it outlives a Pack rebuild.
    pub id: String,
    pub layer: Layer,
    pub accuracy: Accuracy,
    /// House number and street for an address (`12 King Street West`), or
    /// the name of a street, area or POI.
    pub name: String,
    /// ISO code of a result that is an area: on a country its ISO 3166-1
    /// alpha-2 code, on an area below it its ISO 3166-2 code, as
    /// `context` gives them. Absent on any other result, and on an area
    /// the source data gives no code.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "MC")]
    pub code: Option<String>,
    /// One-line display label: `name` followed by its location. With
    /// `lang`, each part that names the result or one of its areas is that
    /// name in the language asked, as `name` and `context` are.
    pub label: String,
    /// POI category, such as `amenity:cafe`. Present only on POIs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// The class of road a street is, the most important of its ways, such
    /// as `motorway` or `residential`. Present only on streets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub road_class: Option<RoadClass>,
    /// The chain a place belongs to, when its name may not say: "Tankstation
    /// Zanoli" is an Esso. Present only on places.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brand: Option<Brand>,
    /// How to reach a place: its phone number, website and email, each as
    /// its source writes it. og rewrites none and does not check that one
    /// still answers. Present only on POIs that state at least one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contact: Option<Contact>,
    /// When a place is open, in OpenStreetMap's `opening_hours` syntax as
    /// its source writes it. og does not parse it, so it never says whether
    /// the place is open now. Present only on POIs that state it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "Mo-Fr 08:00-18:00; Sa 09:00-13:00")]
    pub opening_hours: Option<String>,
    /// The box around a result that covers more than a point, as `[west,
    /// south, east, north]` in degrees. It holds an area's outline, a
    /// street's line, or the outline a POI is drawn with. It is the
    /// narrowest box, so for a result that crosses the antimeridian `west`
    /// is greater than `east`, and only then. Absent on a result that is
    /// only a point.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = json!([7.4090279, 43.7247599, 7.4398704, 43.7519311]))]
    pub bbox: Option<[f64; 4]>,
    /// The IANA name of the timezone the result lies in, such as
    /// `Europe/Monaco`. Its offset from UTC is the reader's to compute,
    /// as it changes with the date. An area's is the zone it lies wholly in,
    /// and any other result's is the zone at its point. Absent on an area
    /// that reaches into two zones, where the timezone boundaries name no
    /// zone or two, and on every result of a Pack built without them.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "Europe/Monaco")]
    pub timezone: Option<String>,
    /// The kinds of food a place serves, as OpenStreetMap's `cuisine` tag
    /// lists them, in lower case and in order. Absent on a result that
    /// states none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(example = json!(["pizza", "italian"]))]
    pub cuisine: Vec<String>,
    /// The building an address's source draws it as, and its name when it
    /// has one. Absent on an address mapped as a point on no building, and
    /// on every other result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub building: Option<crate::record::Building>,
    /// For an address that states no postcode, the one postcode the other
    /// addresses of its street state, when they all state the same one. It
    /// is where the address most likely lies, and `address.postcode` stays
    /// what the source states. Absent on every other result.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "94124")]
    pub street_postcode: Option<String>,
    #[serde(skip)]
    pub(crate) zcta_postcode: Option<String>,
    /// The points to route to when going to the result, which are the
    /// entrances OpenStreetMap maps for it. They are the nodes tagged
    /// `entrance` on the outline of the building or site an address or a
    /// POI is drawn with, or the result's own point when its node is so
    /// tagged. Absent when OpenStreetMap maps none, and on every other
    /// layer.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(example = json!([{"lon": 7.4263, "lat": 43.7396, "kind": "main"}]))]
    pub access: Vec<AccessPoint>,
    /// The address as the lines of a postal address, top to bottom, the way
    /// the result's country writes one. The country's address template
    /// says which parts go on which line and in what order. Each part is
    /// what `address` states or, where it states none, the area of
    /// `context`, and a part og has nothing for is left out. A region is
    /// its ISO 3166-2 short code where the template asks for one. The last
    /// line names the country as `context.country` does, so `lang` names it
    /// too. Present on an address with its house number, and on a POI that
    /// states its street. Absent on every other layer, and on a result in
    /// no country og has a template for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(example = json!(["10 King Street West", "Toronto, ON M5H 1A1", "Canada"]))]
    pub address_lines: Vec<String>,
    /// Address parts stated by the source record.
    #[serde(skip_serializing_if = "Address::is_empty")]
    pub address: Address,
    /// Administrative areas containing the result.
    #[serde(skip_serializing_if = "Context::is_empty")]
    pub context: Context,
    pub source: Source,
    /// The result's name in each further language `lang` asks for, by the
    /// code as asked. Present only when `lang` names more than one language
    /// and the result has a name in at least one of the others.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = json!({"de": "Genf"}))]
    pub translations: Option<BTreeMap<String, String>>,
    /// The record behind the result, for the names `lang` asks for.
    #[serde(skip)]
    pub record_id: Option<RecordId>,
    /// The result's local name, when `lang` renamed it.
    #[serde(skip)]
    pub local_name: Option<String>,
}

/// Properties of a forward geocoding result.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ForwardProperties {
    #[serde(flatten)]
    pub common: Common,
    /// Text relevance. Higher is better. Comparable only within one
    /// response: use `confidence` to judge a single result.
    pub score: f32,
    /// Probability, from 0 to 1, that this is the address the query asks
    /// for. Of results at 0.8, about 8 in 10 are correct. Present only when
    /// the Pack is calibrated and the query states a house number, the
    /// queries calibration measures. `0` on a `fallback` result.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 0, maximum = 1)]
    pub confidence: Option<f32>,
    /// `true` when no match was found and the result is the most precise
    /// area the query names instead: its postcode, a town or other area,
    /// its region or its country. `layer` says which. Absent otherwise.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[schema(value_type = Option<bool>)]
    pub fallback: bool,
    /// Distance from the request's `focus` point in meters. Present only
    /// when the request gives `focus`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_m: Option<f64>,
    /// How each address part the query states matched the result. Present
    /// on a `/geocode` result when the query states a part. A place found
    /// because the whole query is one of its names states the part it is.
    /// Absent on `/places` and `/autocomplete`. A place is sought by its
    /// name and a suggestion by what is typed so far, which no address
    /// reading judges.
    #[serde(rename = "match", skip_serializing_if = "Option::is_none")]
    pub matched: Option<QueryMatch>,
    /// The parts of `label` the typed text matched, in order, none
    /// overlapping. Present on `/autocomplete` results, the only ones found
    /// by text still being typed. A place suggested for the kind the text
    /// names is marked where the typed words lie in its label too. Absent
    /// when no typed word can be placed in the label.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(value_type = Option<Vec<Highlight>>)]
    pub highlights: Vec<Highlight>,
}

/// A range of a result's `label` that the typed text matched.
///
/// `start` and `end` count Unicode scalar values from the start of the
/// label, `end` exclusive. They are not bytes and not UTF-16 code units, so
/// a client converts them to the offsets its strings use.
///
/// Search matches typed words against the record, not against the label,
/// so each typed word is placed by finding it again in the label, cut into
/// words and folded as the index folds text. `elysee` marks `Élysée`. A
/// word still being typed marks only the letters typed so far, and a word
/// matched through a typo marks the whole label word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Highlight {
    pub start: usize,
    pub end: usize,
}

/// How each address part a query states matched one result. A part the
/// query does not state is absent, whatever the result holds for it. Every
/// result fills in parts the query left out, so naming them says nothing.
/// The query's parts are the ones its reading labels, so no query states a
/// district. A `country` filter states the country too. A query that is
/// wholly one of a place's names states the part that place is, its
/// `locality` for a town or a county, its `region`, `postcode`, `country`
/// or `street`, and that part is `matched`.
///
/// What judges each part is listed below.
///
/// - `housenumber`: `matched` when the result states the number.
///   `partial` when the number is placed on a range or between its
///   neighbours, or the result states the number the stated one starts
///   with (`407` for `407a`). `unmatched` when a street or an area answers
///   for it.
/// - `street`: the query's street words against the words of the result's
///   street, as search counts typos. `matched` when the street has each
///   word, the word an abbreviation stands for, or its abbreviation
///   (`Street` for a street written `St`), and the query writes every word
///   of the street but its type. `partial` when a word is a typo away or
///   missing, or the street has a word the query leaves out (`South Main
///   Street` for `Main St`). `unmatched` when it has none of them or the
///   result has no street.
/// - `unit`: `matched` when the result states the unit, else `unmatched`.
/// - `locality`: `matched` when an area the result lies in has the name.
///   `partial` when the name is read through a typo, is the postal town of
///   the stated postcode, or is a settlement mapped as a point beside the
///   result, and on a town or county result whose name has a word the
///   query leaves out (`East Providence` for `Providence`). `unmatched` when the result lies elsewhere or the Pack does
///   not know the name.
/// - `region` and `country`: `matched` when the result lies in the one
///   named, or in a country the `country` filter names. `partial` when the
///   name is read through a typo. `unmatched` when the Pack does not know
///   the name.
/// - `postcode`: `matched` when the result or its street states the
///   postcode. `partial` when one starts the other, or the result only
///   lies where the postcode does. `unmatched` when the result states
///   another or nothing places it.
///
/// On a `fallback` result the parts the area answers for are judged, and
/// every other stated part is `unmatched`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct QueryMatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub housenumber: Option<PartMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub street: Option<PartMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<PartMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locality: Option<PartMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<PartMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub postcode: Option<PartMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<PartMatch>,
}

impl QueryMatch {
    /// Whether the query states no part.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// How one stated address part matched a result, best first.
///
/// - `matched`: the result has the value the query states, after
///   normalisation such as case, accents and abbreviations.
/// - `partial`: the result's value differs and was accepted as the same.
/// - `unmatched`: the result does not have the part, has another value, or
///   nothing says it has the stated one.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PartMatch {
    Matched,
    Partial,
    Unmatched,
}

/// Properties of a reverse geocoding result.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ReverseProperties {
    #[serde(flatten)]
    pub common: Common,
    /// Distance from the queried point in meters. Present only on the
    /// address or street result, not on areas.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_m: Option<f64>,
}

// From the Pack's point precision: `point` is rooftop, `estimated` is
// interpolated, `representative_point` is street, a way or relation
// `centroid` is centroid, and a node standing for something larger is
// approximate.
/// How precisely the coordinates locate the result, most precise first.
///
/// - `rooftop`: the address or POI itself.
/// - `interpolated`: a house number placed between known neighbours.
/// - `street`: a point on the street.
/// - `centroid`: the centre of a mapped area.
/// - `approximate`: a point standing for something larger, such as a town
///   or postcode.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Accuracy {
    Rooftop,
    Interpolated,
    Street,
    Centroid,
    Approximate,
}

impl Accuracy {
    /// The accuracy of a record's own point.
    pub fn of(precision: records::PointPrecision, source: &records::Source) -> Self {
        match precision {
            records::PointPrecision::Point => Self::Rooftop,
            records::PointPrecision::Estimated => Self::Interpolated,
            records::PointPrecision::RepresentativePoint => Self::Street,
            records::PointPrecision::Centroid => match source.object_type {
                Some(ObjectType::Way | ObjectType::Relation) => Self::Centroid,
                Some(ObjectType::Node | ObjectType::Row) | None => Self::Approximate,
            },
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rooftop => "rooftop",
            Self::Interpolated => "interpolated",
            Self::Street => "street",
            Self::Centroid => "centroid",
            Self::Approximate => "approximate",
        }
    }
}

/// Address parts stated by the source record. Parts the record omits are
/// absent, not filled in from `context`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct Address {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub housenumber: Option<String>,
    #[serde(skip)]
    pub(crate) block_number_len: Option<std::num::NonZeroU16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub street: Option<String>,
    /// Named place the house number belongs to where there is no street
    /// (OSM `addr:place`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
    /// Areas below the locality, outermost first (OSM `addr:suburb`,
    /// `addr:quarter`, `addr:neighbourhood`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub areas: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub postcode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// `street` split into its parts as written. Absent when the street has
    /// no type or direction to split off, such as `Broadway`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub street_parts: Option<StreetParts>,
}

/// The parts of a street name, as written: `W Main St N` is the prefix `W`,
/// the name `Main`, the type `St` and the suffix `N`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct StreetParts {
    /// Direction before the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// The street's own name, without its type and directions.
    pub name: String,
    /// Street type, such as `Street` or `Ave`.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Direction after the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
}

impl Address {
    pub(crate) fn block_parts(&self) -> Option<(&str, Option<&str>)> {
        let number = self.housenumber.as_deref()?;
        let (block, house) = number.split_at_checked(usize::from(self.block_number_len?.get()))?;
        if house.is_empty() {
            Some((block, None))
        } else {
            Some((
                block,
                Some(house.strip_prefix('-').filter(|house| !house.is_empty())?),
            ))
        }
    }

    /// Fills the parts read from `street`.
    pub fn with_parts(mut self) -> Self {
        self.street_parts = self
            .street
            .as_deref()
            .and_then(crate::road::StreetParts::of)
            .map(|parts| StreetParts {
                prefix: parts.prefix.map(str::to_string),
                name: parts.name,
                kind: parts.kind.map(str::to_string),
                suffix: parts.suffix.map(str::to_string),
            });
        self
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Only a postcode, as a postcode record states it.
    pub fn postcode(postcode: &str) -> Self {
        Self {
            postcode: stated(Some(postcode)),
            ..Self::default()
        }
    }
}

impl From<&AddressComponents> for Address {
    fn from(address: &AddressComponents) -> Self {
        Self {
            housenumber: stated(Some(&address.number)),
            block_number_len: address.block_number_len,
            unit: stated(address.unit.as_deref()),
            street: stated(address.street.as_deref()),
            place: stated(address.place.as_deref()),
            areas: address
                .areas
                .iter()
                .filter(|area| !area.trim().is_empty())
                .cloned()
                .collect(),
            locality: stated(address.locality.as_deref()),
            region: stated(address.region.as_deref()),
            postcode: stated(address.postcode.as_deref()),
            country: stated(address.country.as_deref()),
            ..Self::default()
        }
    }
}

impl From<&InterpolationAddressComponents> for Address {
    fn from(address: &InterpolationAddressComponents) -> Self {
        Self {
            street: stated(address.street.as_deref()),
            place: stated(address.place.as_deref()),
            locality: stated(address.locality.as_deref()),
            region: stated(address.region.as_deref()),
            postcode: stated(address.postcode.as_deref()),
            country: stated(address.country.as_deref()),
            ..Self::default()
        }
    }
}

fn stated(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

// The `sub` tiers exist for countries that map levels between the
// conventional ones: Germany's three `state_district` levels would otherwise
// lose two.
/// Areas containing the result, one per tier, most specific first. A tier
/// the country does not map is absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct Context {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neighbourhood: Option<ContextArea>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sublocality: Option<ContextArea>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locality: Option<ContextArea>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdistrict: Option<ContextArea>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub district: Option<ContextArea>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subregion: Option<ContextArea>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<ContextArea>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<ContextArea>,
    /// Named area outside the administrative levels, usually an island.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<ContextArea>,
}

impl Context {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Its areas, smallest first.
    pub fn areas(&self) -> impl Iterator<Item = &ContextArea> {
        [
            &self.neighbourhood,
            &self.sublocality,
            &self.locality,
            &self.subdistrict,
            &self.district,
            &self.subregion,
            &self.region,
            &self.country,
            &self.place,
        ]
        .into_iter()
        .flatten()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ContextArea {
    /// Stable ID from the source data, as in a result's `id`.
    pub id: String,
    pub name: String,
    /// ISO code of the area. On `country` its ISO 3166-1 alpha-2 code, such
    /// as `NZ`. On an area below it, its ISO 3166-2 code, such as `CA-ON`,
    /// when the source data gives one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "NZ")]
    pub code: Option<String>,
    /// The area's name in each further language `lang` asks for, by the
    /// code as asked. Present only when `lang` names more than one language
    /// and the area has a name in at least one of the others.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = json!({"de": "Genf"}))]
    pub translations: Option<BTreeMap<String, String>>,
    /// The area's record, for the names `lang` asks for.
    #[serde(skip)]
    pub record_id: RecordId,
    /// The area's local name, when `lang` renamed it.
    #[serde(skip)]
    pub local_name: Option<String>,
    /// The area's Wikidata item, for the vendors that write one.
    #[serde(skip)]
    pub wikidata: Option<u32>,
    /// The code on the area's vehicle plates (`SIM`), for the vendors that
    /// write one.
    #[serde(skip)]
    pub plate_code: Option<String>,
    #[serde(skip)]
    pub census_code: Option<String>,
}

impl Point {
    fn new(lon: f64, lat: f64) -> Self {
        Self {
            kind: PointType::Point,
            coordinates: [lon, lat],
        }
    }

    pub fn lon(&self) -> f64 {
        self.coordinates[0]
    }

    pub fn lat(&self) -> f64 {
        self.coordinates[1]
    }
}

impl ForwardFeature {
    fn new(lon: f64, lat: f64, properties: ForwardProperties) -> Self {
        Self {
            kind: FeatureType::Feature,
            geometry: Point::new(lon, lat),
            properties,
        }
    }

    pub fn lon(&self) -> f64 {
        self.geometry.lon()
    }

    pub fn lat(&self) -> f64 {
        self.geometry.lat()
    }
}

impl ReverseFeature {
    fn new(lon: f64, lat: f64, properties: ReverseProperties) -> Self {
        Self {
            kind: FeatureType::Feature,
            geometry: Point::new(lon, lat),
            properties,
        }
    }

    pub fn lon(&self) -> f64 {
        self.geometry.lon()
    }

    pub fn lat(&self) -> f64 {
        self.geometry.lat()
    }
}

/// The areas records lie in, each read from the Pack once per answer: the
/// hits of one query mostly share them.
#[derive(Debug)]
pub struct Contexts<'a> {
    pack: &'a Reader,
    areas: HashMap<RecordId, Option<ContextArea>>,
    /// Every name of each area, as search compares them.
    keys: HashMap<RecordId, Vec<String>>,
}

impl<'a> Contexts<'a> {
    pub fn new(pack: &'a Reader) -> Self {
        Self {
            pack,
            areas: HashMap::new(),
            keys: HashMap::new(),
        }
    }

    /// The areas the record's boundary context names. A record outside
    /// every boundary has none.
    pub fn of(&mut self, record_id: RecordId) -> Result<Context> {
        let Some(context) = self.pack.boundary_context(record_id)? else {
            return Ok(Context::default());
        };
        let tuple = &context.admin_context;
        Ok(Context {
            neighbourhood: self.area(tuple.get(PlaceLayer::Neighbourhood))?,
            sublocality: self.area(tuple.get(PlaceLayer::SubLocality))?,
            locality: self.area(tuple.get(PlaceLayer::Locality))?,
            subdistrict: self.area(tuple.get(PlaceLayer::SubDistrict))?,
            district: self.area(tuple.get(PlaceLayer::District))?,
            subregion: self.area(tuple.get(PlaceLayer::SubRegion))?,
            region: self.area(tuple.get(PlaceLayer::Region))?,
            country: self.area(tuple.get(PlaceLayer::Country))?,
            place: self.area(tuple.get(PlaceLayer::Place))?,
        })
    }

    /// Whether `stated` is area `record_id`'s ISO code or the short code
    /// after its country's (`CA` of `US-CA`). A code is written one way in
    /// every language, so it stays as the source writes it.
    fn is_code(&self, record_id: RecordId, stated: &str) -> bool {
        self.code(record_id).is_some_and(|code| {
            code.eq_ignore_ascii_case(stated)
                || code
                    .split_once('-')
                    .is_some_and(|(_, short)| short.eq_ignore_ascii_case(stated))
        })
    }

    /// Whether `stated` is a name of area `record_id`, in any language.
    fn names_area(&mut self, record_id: RecordId, stated: &str) -> Result<bool> {
        let Some(stated) = crate::util::fold::search_key(stated) else {
            return Ok(false);
        };
        if !self.keys.contains_key(&record_id) {
            let names = self.pack.records().names(record_id)?;
            let keys = names
                .iter()
                .map(String::as_str)
                .chain(self.pack.language_names(record_id))
                .filter_map(crate::util::fold::search_key)
                .collect();
            self.keys.insert(record_id, keys);
        }
        Ok(self.keys[&record_id].contains(&stated))
    }

    /// An area's ISO code: a country's own, else its subdivision code.
    fn code(&self, record_id: RecordId) -> Option<String> {
        self.pack
            .country_code(record_id)
            .map(|code| code.as_str().to_string())
            .or_else(|| self.pack.subdivision_code(record_id).map(str::to_string))
    }

    fn area(&mut self, record_id: Option<RecordId>) -> Result<Option<ContextArea>> {
        let Some(record_id) = record_id else {
            return Ok(None);
        };
        if let Some(area) = self.areas.get(&record_id) {
            return Ok(area.clone());
        }
        let area = self
            .pack
            .context_record(record_id)?
            .map(|record| {
                Ok::<_, crate::Error>(ContextArea {
                    id: record.id,
                    name: record.name,
                    code: self.code(record_id),
                    translations: None,
                    record_id,
                    local_name: None,
                    wikidata: self.pack.records().wikidata(record_id)?,
                    plate_code: self.pack.plate_code(record_id).map(str::to_string),
                    census_code: self.pack.census_code(record_id).map(str::to_string),
                })
            })
            .transpose()?;
        self.areas.insert(record_id, area.clone());
        Ok(area)
    }
}

/// The answer to `/geocode`, `/places` or `/autocomplete`: one feature per hit, in the
/// order search ranked them.
pub fn forward_collection(
    pack: &Reader,
    query: String,
    hits: Vec<TextSearchHit>,
) -> Result<ForwardCollection> {
    let mut contexts = Contexts::new(pack);
    let features = hits
        .into_iter()
        .map(|hit| forward_feature(&mut contexts, hit))
        .collect::<Result<Vec<_>>>()?;
    let attribution = attribution(
        pack,
        features.iter().map(|feature| &feature.properties.common),
    );
    Ok(ForwardCollection {
        kind: FeatureCollectionType::FeatureCollection,
        geocoding: ForwardGeocoding {
            version: Version::V1,
            query,
            attribution,
        },
        features,
    })
}

/// The languages a request asks names in, which `lang` gives as
/// comma-separated language codes, such as `fr,de`. The first names each
/// result and area, and the others give their `translations`. Repeats
/// count once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Languages(Vec<String>);

/// Most distinct languages one request may ask names in. Each one adds a
/// name to every result and every area of it, so the cap bounds the size of
/// an answer.
pub const MAX_LANGUAGES: usize = 20;

/// Why `lang` cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguagesError {
    /// The first code that is not a language code.
    NotACode(String),
    /// More than [`MAX_LANGUAGES`] distinct codes, with how many.
    TooMany(usize),
}

impl Languages {
    /// Reads `lang`. A language code is two or three letters, then optional
    /// subtags of letters and digits, such as `de`, `zh-Hant` or `pt-BR`.
    /// Repeats are dropped before the cap counts, keeping the first spelling.
    pub fn parse(lang: &str) -> std::result::Result<Self, LanguagesError> {
        let mut codes = Vec::<String>::new();
        for code in lang
            .split(',')
            .map(str::trim)
            .filter(|code| !code.is_empty())
        {
            if !is_language_code(code) {
                return Err(LanguagesError::NotACode(code.to_string()));
            }
            if !codes.iter().any(|known| known.eq_ignore_ascii_case(code)) {
                codes.push(code.to_string());
            }
        }
        if codes.len() > MAX_LANGUAGES {
            return Err(LanguagesError::TooMany(codes.len()));
        }
        Ok(Self(codes))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn codes(&self) -> &[String] {
        &self.0
    }
}

fn is_language_code(code: &str) -> bool {
    let mut parts = code.split(['-', '_']);
    let language = parts.next().unwrap_or_default();
    (2..=3).contains(&language.len())
        && language.bytes().all(|byte| byte.is_ascii_alphabetic())
        && parts.all(|part| {
            !part.is_empty()
                && part.len() <= 8
                && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
}

/// Names a result or area in `languages`. The first gives `name`, or the
/// local name stays when it has none in that language. A renamed one keeps
/// its local name in `local_name`. Each other language it has a name in goes
/// into `translations`.
fn localize_names(
    pack: &Reader,
    record_id: RecordId,
    languages: &Languages,
    name: &mut String,
    local_name: &mut Option<String>,
    translations: &mut Option<BTreeMap<String, String>>,
) {
    let Some((first, others)) = languages.codes().split_first() else {
        return;
    };
    if let Some(named) = pack.language_name(record_id, first)
        && named != name.as_str()
    {
        *local_name = Some(std::mem::replace(name, named.to_string()));
    }
    let found = others
        .iter()
        .filter_map(|code| {
            Some((
                code.clone(),
                pack.language_name(record_id, code)?.to_string(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    *translations = (!found.is_empty()).then_some(found);
}

/// Names a result and its areas in `languages`, and rewrites its label to
/// match. Returns how each part of the old label moved (see [`relabel`]).
fn localize_common(pack: &Reader, common: &mut Common, languages: &Languages) -> Vec<Moved> {
    if let Some(record_id) = common.record_id {
        localize_names(
            pack,
            record_id,
            languages,
            &mut common.name,
            &mut common.local_name,
            &mut common.translations,
        );
    }
    let context = &mut common.context;
    for area in [
        &mut context.neighbourhood,
        &mut context.sublocality,
        &mut context.locality,
        &mut context.subdistrict,
        &mut context.district,
        &mut context.subregion,
        &mut context.region,
        &mut context.country,
        &mut context.place,
    ]
    .into_iter()
    .flatten()
    {
        localize_names(
            pack,
            area.record_id,
            languages,
            &mut area.name,
            &mut area.local_name,
            &mut area.translations,
        );
    }
    // A part the source states that names an area was written as the area
    // (see [`name_stated_parts`]), so it is renamed with it.
    let address = &mut common.address;
    for part in [
        &mut address.locality,
        &mut address.region,
        &mut address.country,
        &mut address.place,
    ]
    .into_iter()
    .flatten()
    .chain(&mut address.areas)
    {
        if let Some(area) = context
            .areas()
            .find(|area| area.local_name.as_deref() == Some(part.as_str()))
        {
            part.clone_from(&area.name);
        }
    }
    common.address_lines =
        crate::address_lines::address_lines(common.layer, &common.address, &common.context);
    let context = &common.context;
    let renamed = std::iter::once((common.local_name.as_deref(), common.name.as_str()))
        .chain(
            [
                &context.neighbourhood,
                &context.sublocality,
                &context.locality,
                &context.subdistrict,
                &context.district,
                &context.subregion,
                &context.region,
                &context.country,
                &context.place,
            ]
            .into_iter()
            .flatten()
            .map(|area| (area.local_name.as_deref(), area.name.as_str())),
        )
        .filter_map(|(local, named)| Some((local?.to_string(), named.to_string())))
        .collect::<Vec<_>>();
    let (label, moved) = relabel(&common.label, &renamed);
    common.label = label;
    moved
}

/// Where a part of an old label went in a new one, in characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Moved {
    start: usize,
    end: usize,
    /// Where the part starts in the new label, `None` when it was renamed.
    to: Option<usize>,
}

/// Separates the parts of a label.
const LABEL_SEPARATOR: &str = ", ";

/// `label` with each part that is a local name of `renamed` replaced by its
/// new name, and how each old part moved. A part is the label's text
/// between separators; the parts that name the result and its areas are
/// their names as written.
fn relabel(label: &str, renamed: &[(String, String)]) -> (String, Vec<Moved>) {
    let mut parts = Vec::new();
    let mut moved = Vec::new();
    let (mut start, mut to) = (0, 0);
    for part in label.split(LABEL_SEPARATOR) {
        let length = part.chars().count();
        let new = renamed
            .iter()
            .find(|(local, _)| local == part)
            .map(|(_, named)| named.as_str());
        moved.push(Moved {
            start,
            end: start + length,
            to: new.is_none().then_some(to),
        });
        let written = new.unwrap_or(part);
        to += written.chars().count() + LABEL_SEPARATOR.chars().count();
        start += length + LABEL_SEPARATOR.chars().count();
        parts.push(written);
    }
    (parts.join(LABEL_SEPARATOR), moved)
}

/// `highlights` moved with their label's parts. A highlight in a renamed
/// part is dropped: the typed text matched the local name, not the new one.
fn move_highlights(highlights: &mut Vec<Highlight>, moved: &[Moved]) {
    highlights.retain_mut(|highlight| {
        let Some(part) = moved
            .iter()
            .find(|part| part.start <= highlight.start && highlight.end <= part.end)
        else {
            return false;
        };
        let Some(to) = part.to else {
            return false;
        };
        let shift = |offset: usize| offset - part.start + to;
        (highlight.start, highlight.end) = (shift(highlight.start), shift(highlight.end));
        true
    });
}

/// Names every result and context area of a forward answer in the
/// languages `lang` asked for.
pub fn localize_forward(pack: &Reader, collection: &mut ForwardCollection, languages: &Languages) {
    for feature in &mut collection.features {
        let moved = localize_common(pack, &mut feature.properties.common, languages);
        move_highlights(&mut feature.properties.highlights, &moved);
    }
}

/// [`localize_forward`] for a reverse answer.
pub fn localize_reverse(pack: &Reader, collection: &mut ReverseCollection, languages: &Languages) {
    for feature in &mut collection.features {
        localize_common(pack, &mut feature.properties.common, languages);
    }
}

/// Sets each result's distance from `focus`, when the request gave one.
pub fn measure_from(collection: &mut ForwardCollection, focus: Option<[f64; 2]>) {
    let Some([lon, lat]) = focus else {
        return;
    };
    for feature in &mut collection.features {
        let [feature_lon, feature_lat] = feature.geometry.coordinates;
        let metres = crate::spatial_index::haversine_m(lon, lat, feature_lon, feature_lat);
        // To the centimetre: finer is noise next to the point's own precision.
        feature.properties.distance_m = Some((metres * 100.0).round() / 100.0);
    }
}

/// One search hit as a feature. The hit's summary already holds what search
/// placed or estimated (an interpolated house number, a street's centre
/// line), so nothing is read again but its context.
pub fn forward_feature(contexts: &mut Contexts, hit: TextSearchHit) -> Result<ForwardFeature> {
    let point = hit
        .record
        .point
        .ok_or_else(|| Error::Inconsistent(format!("search hit {} has no point", hit.record.id)))?;
    let accuracy = Accuracy::of(point.precision, &hit.record.source);
    let mut common = common(contexts, hit.record_id, hit.record, accuracy)?;
    let mut highlights = hit.highlights;
    let moved = name_stated_parts(contexts, &mut common)?;
    if !moved.is_empty() {
        move_highlights(&mut highlights, &moved);
    }
    let properties = ForwardProperties {
        common,
        score: hit.score,
        confidence: hit.confidence,
        fallback: hit.fallback,
        distance_m: None,
        matched: (!hit.matched.is_empty()).then_some(hit.matched),
        highlights,
    };
    Ok(ForwardFeature::new(point.lon, point.lat, properties))
}

/// The answer to `/reverse`, converted from what reverse geocoding returns.
///
/// This is the one seam between [`crate::reverse`] and the contract. The
/// features are, precise to broad:
///
/// 1. the primary result, if there is one, at the point reverse chose for it
///    (the address point, the house number placed on its range, the closest
///    point of the street, or the area's own point), with its distance;
/// 2. one feature per context area reverse resolved
///    (`evidence.context_record_ids`), each at its own point and without a
///    distance: the postcode, the admin areas from neighbourhood to country,
///    then the place area, which stands outside the admin levels and may be
///    an islet or Honshu.
///
/// Reverse geocoding's own label is kept for the primary result. Its accuracy
/// maps `point` and `context` to the accuracy of the record's own point (so
/// an address drawn as a building is `centroid` here as in search),
/// `estimated` to `interpolated` and `street` to `street`.
///
/// An estimated address states its house number only in its label, which
/// reverse writes as `"{number} {street}, ..."`; the number is read back from
/// there.
pub fn reverse_collection(pack: &Reader, response: &GeocodeResponse) -> Result<ReverseCollection> {
    let mut contexts = Contexts::new(pack);
    let mut features = Vec::new();
    let mut primary_record_id = None;
    if let Some(result) = &response.result
        && let Some(record_id) = result.primary_record_id
    {
        primary_record_id = Some(record_id);
        let mut summary = pack.record_summary(record_id)?;
        let own_point = summary
            .point
            .ok_or_else(|| Error::Inconsistent(format!("record {record_id} has no point")))?;
        let (lon, lat) = result
            .point
            .as_ref()
            .map_or((own_point.lon, own_point.lat), |point| {
                (point.lon, point.lat)
            });
        let accuracy = match result.point.as_ref().map(|point| point.precision) {
            Some(reverse::PointPrecision::Estimated) => Accuracy::Interpolated,
            Some(reverse::PointPrecision::Street) => Accuracy::Street,
            Some(reverse::PointPrecision::Point | reverse::PointPrecision::Context) | None => {
                Accuracy::of(own_point.precision, &summary.source)
            }
        };
        if result.match_kind == MatchKind::EstimatedAddress {
            let number = estimated_number(&result.label)?;
            if result
                .id
                .as_deref()
                .is_some_and(|id| id.starts_with("derived:tiger:"))
            {
                summary.name = format!("{number} {}", summary.name);
                summary.address.housenumber = Some(number.to_string());
                summary.address.street.clone_from(&result.context.street);
                summary
                    .address
                    .postcode
                    .clone_from(&result.context.postcode);
                summary.source.dataset = crate::tiger::DATASET.into();
            } else {
                let interpolation = pack.interpolation(record_id)?.ok_or_else(|| {
                    Error::Inconsistent(format!("record {record_id} is not an interpolation"))
                })?;
                summary.name = labels::estimated_address_name(number, &interpolation.address)
                    .unwrap_or_else(|| format!("{number} {}", interpolation.name()));
                summary.address = Address {
                    housenumber: Some(number.to_string()),
                    ..Address::from(&interpolation.address)
                };
            }
        }
        if let Some(id) = &result.id {
            summary.id.clone_from(id);
        }
        if let Some(layer) = &result.layer {
            summary.layer.clone_from(layer);
        }
        summary.label.clone_from(&result.label);
        let mut common = common(&mut contexts, record_id, summary, accuracy)?;
        name_stated_parts(&mut contexts, &mut common)?;
        let properties = ReverseProperties {
            common,
            distance_m: result.distance_m,
        };
        features.push(ReverseFeature::new(lon, lat, properties));
    }

    let mut areas = Vec::new();
    for &record_id in response
        .result
        .iter()
        .flat_map(|result| &result.evidence.context_record_ids)
    {
        if Some(record_id) == primary_record_id || areas.iter().any(|(id, _)| *id == record_id) {
            continue;
        }
        areas.push((record_id, pack.record_summary(record_id)?));
    }
    areas.sort_by_key(|(_, summary)| breadth(&summary.layer));
    for (record_id, summary) in areas {
        let point = summary.point.ok_or_else(|| {
            Error::Inconsistent(format!("context record {record_id} has no point"))
        })?;
        let accuracy = Accuracy::of(point.precision, &summary.source);
        let properties = ReverseProperties {
            common: common(&mut contexts, record_id, summary, accuracy)?,
            distance_m: None,
        };
        features.push(ReverseFeature::new(point.lon, point.lat, properties));
    }

    let attribution = attribution(
        pack,
        features.iter().map(|feature| &feature.properties.common),
    );
    Ok(ReverseCollection {
        kind: FeatureCollectionType::FeatureCollection,
        geocoding: ReverseGeocoding {
            version: Version::V1,
            point: LonLat {
                lon: response.lon,
                lat: response.lat,
            },
            attribution,
        },
        features,
    })
}

/// The house number at the start of an estimated address's label.
fn estimated_number(label: &str) -> Result<u32> {
    label
        .split_whitespace()
        .next()
        .and_then(|number| number.parse().ok())
        .ok_or_else(|| {
            Error::Inconsistent(format!(
                "estimated address label {label:?} starts with no number"
            ))
        })
}

/// Order of context areas: precise to broad, the place area last.
fn breadth(layer: &str) -> usize {
    const ORDER: [Layer; 7] = [
        Layer::Postcode,
        Layer::Neighbourhood,
        Layer::Locality,
        Layer::District,
        Layer::Region,
        Layer::Country,
        Layer::Place,
    ];
    ORDER
        .iter()
        .position(|candidate| candidate.as_str() == layer)
        .unwrap_or(ORDER.len())
}

/// The fields both directions share, read once from the record.
fn common(
    contexts: &mut Contexts,
    record_id: RecordId,
    summary: Summary,
    accuracy: Accuracy,
) -> Result<Common> {
    let layer = Layer::parse(&summary.layer).ok_or_else(|| {
        Error::Inconsistent(format!(
            "record {record_id} has unknown layer {}",
            summary.layer
        ))
    })?;
    let tiger_estimate = summary.source.dataset == crate::tiger::DATASET;
    let tiger_range = if tiger_estimate {
        summary
            .address
            .housenumber
            .as_deref()
            .and_then(|number| number.parse().ok())
            .and_then(|number| {
                contexts
                    .pack
                    .tiger_ranges
                    .for_street(record_id)
                    .find(|range| range.id(number) == summary.id)
            })
    } else {
        None
    };
    let address = summary.address.with_parts();
    let context = contexts.of(record_id)?;
    let stored_address = layer == Layer::Address
        && !summary.id.starts_with("derived:")
        && accuracy != Accuracy::Interpolated;
    let zcta_postcode =
        (stored_address && address.postcode.is_none() && summary.street_postcode.is_none())
            .then(|| contexts.pack.tiger_zcta.postcode(record_id))
            .flatten();
    let side = |direction| {
        tiger_range.and_then(|range| {
            contexts
                .pack
                .tiger_side_blocks
                .code(range.side_key(direction))
        })
    };
    let tiger_block = if stored_address {
        contexts.pack.tiger_blocks.code(record_id)
    } else {
        tiger_range.and_then(|range| side(range.side))
    };
    let tiger_block_2010 = stored_address
        .then(|| contexts.pack.tiger_historical.code(record_id))
        .flatten();
    let tiger_block_left = side(b'L');
    let tiger_block_right = side(b'R');
    Ok(Common {
        tiger_range,
        tiger_block,
        tiger_block_2010,
        tiger_block_left,
        tiger_block_right,
        id: summary.id,
        code: layer
            .is_context()
            .then(|| contexts.code(record_id))
            .flatten(),
        layer,
        accuracy,
        name: summary.name,
        label: summary.label,
        category: summary.category,
        road_class: summary.road_class,
        brand: summary.brand,
        contact: summary.contact,
        opening_hours: summary.opening_hours,
        bbox: if tiger_estimate { None } else { summary.bbox },
        timezone: if tiger_estimate {
            None
        } else {
            summary.timezone
        },
        cuisine: summary.cuisine,
        street_postcode: summary.street_postcode,
        zcta_postcode,
        building: summary.building,
        access: summary.access,
        address_lines: crate::address_lines::address_lines(layer, &address, &context),
        address,
        context,
        source: Source {
            dataset: summary.source.dataset,
        },
        translations: None,
        record_id: Some(record_id),
        local_name: None,
    })
}

/// Writes each place the source states, its town, region, country or a
/// named part of a town, as the area of the result's context it names, so
/// one answer never names a place two ways: an address tagged `Vienna` in
/// Wien says `Wien`, and with `lang` says what the area does. A code (`CA`,
/// `AT`) is the same in every language and stays. The label
/// follows, and how its parts moved is returned (see [`relabel`]), none when
/// nothing was renamed. A part that names no area of the context stays as
/// the source wrote it.
fn name_stated_parts(contexts: &mut Contexts, common: &mut Common) -> Result<Vec<Moved>> {
    let areas = common
        .context
        .areas()
        .map(|area| (area.record_id, area.name.clone()))
        .collect::<Vec<_>>();
    let mut renamed = Vec::new();
    let address = &mut common.address;
    let parts = [
        &mut address.locality,
        &mut address.region,
        &mut address.country,
        &mut address.place,
    ]
    .into_iter()
    .flatten()
    .chain(&mut address.areas);
    for part in parts {
        for (record_id, name) in &areas {
            if part == name || contexts.is_code(*record_id, part) {
                break;
            }
            if contexts.names_area(*record_id, part)? {
                renamed.push((std::mem::replace(part, name.clone()), name.clone()));
                break;
            }
        }
    }
    if renamed.is_empty() {
        return Ok(Vec::new());
    }
    common.address_lines =
        crate::address_lines::address_lines(common.layer, &common.address, &common.context);
    let (label, moved) = relabel(&common.label, &renamed);
    common.label = label;
    Ok(moved)
}

/// The result columns of one forward feature, by name, in the order
/// `geocode batch` writes them. With no feature the same columns come back
/// empty, so this is also the one list of column names.
pub fn flat_columns(feature: Option<&ForwardFeature>) -> Vec<(&'static str, String)> {
    flat(
        feature.map(|feature| (&feature.properties.common, feature.geometry.coordinates)),
        feature.map(|feature| feature.properties.score.to_string()),
        feature.and_then(|feature| feature.properties.confidence.map(|value| value.to_string())),
    )
}

/// [`flat_columns`] for a reverse feature. `score` and `confidence` rank
/// forward results only, so they stay empty.
pub fn reverse_flat_columns(feature: Option<&ReverseFeature>) -> Vec<(&'static str, String)> {
    flat(
        feature.map(|feature| (&feature.properties.common, feature.geometry.coordinates)),
        None,
        None,
    )
}

fn flat(
    feature: Option<(&Common, [f64; 2])>,
    score: Option<String>,
    confidence: Option<String>,
) -> Vec<(&'static str, String)> {
    let common = feature.map(|(common, _)| common);
    let field =
        |value: &dyn Fn(&Common) -> Option<String>| common.and_then(value).unwrap_or_default();
    let area = |area: fn(&Context) -> &Option<ContextArea>| {
        common
            .and_then(|common| area(&common.context).as_ref())
            .map(|area| area.name.clone())
            .unwrap_or_default()
    };
    let coordinate = |index: usize| {
        feature
            .map(|(_, coordinates)| format!("{:.7}", coordinates[index]))
            .unwrap_or_default()
    };
    vec![
        ("score", score.unwrap_or_default()),
        ("confidence", confidence.unwrap_or_default()),
        ("id", field(&|p| Some(p.id.clone()))),
        ("layer", field(&|p| Some(p.layer.as_str().to_string()))),
        (
            "accuracy",
            field(&|p| Some(p.accuracy.as_str().to_string())),
        ),
        ("name", field(&|p| Some(p.name.clone()))),
        ("label", field(&|p| Some(p.label.clone()))),
        ("category", field(&|p| p.category.clone())),
        (
            "brand",
            field(&|p| p.brand.as_ref().map(|brand| brand.name.clone())),
        ),
        (
            "road_class",
            field(&|p| p.road_class.map(|class| class.as_str().to_string())),
        ),
        ("lat", coordinate(1)),
        ("lon", coordinate(0)),
        (
            "bbox",
            field(&|p| {
                p.bbox
                    .map(|bbox| bbox.map(|side| format!("{side:.7}")).join(","))
            }),
        ),
        ("timezone", field(&|p| p.timezone.clone())),
        (
            "access",
            field(&|p| {
                let points = p
                    .access
                    .iter()
                    .map(|point| format!("{:.7},{:.7},{}", point.lon, point.lat, point.kind));
                Some(points.collect::<Vec<_>>().join("; "))
            }),
        ),
        (
            "address_housenumber",
            field(&|p| p.address.housenumber.clone()),
        ),
        ("address_unit", field(&|p| p.address.unit.clone())),
        ("address_street", field(&|p| p.address.street.clone())),
        ("address_place", field(&|p| p.address.place.clone())),
        (
            "address_areas",
            field(&|p| Some(p.address.areas.join("; "))),
        ),
        ("address_locality", field(&|p| p.address.locality.clone())),
        ("address_region", field(&|p| p.address.region.clone())),
        ("address_postcode", field(&|p| p.address.postcode.clone())),
        ("address_country", field(&|p| p.address.country.clone())),
        (
            "address_lines",
            field(&|p| Some(p.address_lines.join("; "))),
        ),
        (
            "context_neighbourhood",
            area(|context| &context.neighbourhood),
        ),
        ("context_locality", area(|context| &context.locality)),
        ("context_district", area(|context| &context.district)),
        ("context_region", area(|context| &context.region)),
        ("context_country", area(|context| &context.country)),
        ("context_place", area(|context| &context.place)),
        ("source_dataset", field(&|p| Some(p.source.dataset.clone()))),
    ]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_label_names_its_parts_as_lang_does_and_its_highlights_follow() {
        let renamed = [("Monaco".to_string(), "Монако-Вилль".to_string())];
        let (label, moved) = relabel("Casino, Place du Casino, Monaco, 98000, MC", &renamed);
        assert_eq!(label, "Casino, Place du Casino, Монако-Вилль, 98000, MC");
        let mut highlights = vec![
            Highlight { start: 0, end: 3 },
            // "Mon" of the local name, which the label no longer writes.
            Highlight { start: 25, end: 28 },
            Highlight { start: 33, end: 38 },
        ];
        move_highlights(&mut highlights, &moved);
        assert_eq!(
            highlights,
            [
                Highlight { start: 0, end: 3 },
                Highlight { start: 39, end: 44 }
            ]
        );
    }

    /// Twenty distinct codes, the cap, are read in the order given.
    const TWENTY: [&str; MAX_LANGUAGES] = [
        "ar", "bg", "cs", "da", "de", "el", "en", "es", "et", "fi", "fr", "he", "hu", "it", "ja",
        "ko", "nl", "pl", "pt", "ru",
    ];

    #[test]
    fn lang_takes_up_to_twenty_distinct_languages() {
        let languages = Languages::parse(&TWENTY.join(",")).expect("twenty is the cap");
        assert_eq!(languages.codes(), TWENTY);

        let twenty_one = format!("{},sv", TWENTY.join(","));
        assert_eq!(
            Languages::parse(&twenty_one),
            Err(LanguagesError::TooMany(21))
        );
    }

    #[test]
    fn repeats_drop_before_the_cap_counts_and_the_first_spelling_stays() {
        let repeated = format!("{},DE,de,Ar,ar", TWENTY.join(","));
        let languages = Languages::parse(&repeated).expect("repeats count once");
        assert_eq!(languages.codes(), TWENTY);

        let languages = Languages::parse("pt-BR, de,PT-br,fr,De").expect("repeats count once");
        assert_eq!(languages.codes(), ["pt-BR", "de", "fr"]);
    }

    #[test]
    fn a_malformed_code_is_named_even_past_the_cap() {
        let twenty_one = format!("{},sv,d3", TWENTY.join(","));
        assert_eq!(
            Languages::parse(&twenty_one),
            Err(LanguagesError::NotACode("d3".to_string()))
        );
    }

    #[test]
    fn an_answer_credits_openstreetmap_and_each_imported_data_set_it_holds() {
        use crate::record::{AddressRecord, LocationPrecision, SourceProvenance, point_geometry};
        let dir = std::env::temp_dir().join(format!(
            "open-geocode-api-attribution-{}",
            uuid::Uuid::new_v4()
        ));
        let mut writer = crate::pack::Writer::create(&dir).expect("writer");
        writer
            .write(
                &AddressRecord {
                    building: None,
                    access: Vec::new(),
                    disputed: false,
                    street_postcodes: Vec::new(),
                    address: AddressComponents {
                        block_number_len: None,
                        areas: Vec::new(),
                        number: "1".to_string(),
                        street: Some("Avenida de Mayo".to_string()),
                        place: None,
                        unit: None,
                        locality: None,
                        region: None,
                        postcode: None,
                        country: None,
                    },
                    geometry: point_geometry(-58.38, -34.6),
                    location_precision: LocationPrecision::Point,
                    source: SourceProvenance::imported(
                        "openaddresses:ar/c/city_of_buenos_aires",
                        7,
                    ),
                }
                .into(),
                None,
            )
            .expect("address");
        writer.finish().expect("finish");
        let pack = std::sync::Arc::new(Reader::open(&dir).expect("pack"));
        let geocoder =
            reverse::PackReverseGeocoder::from_pack(std::sync::Arc::clone(&pack)).expect("reverse");
        let response = geocoder
            .reverse(reverse::GeocodeOptions::at(-58.38, -34.6))
            .expect("answer");
        let collection = reverse_collection(&pack, &response).expect("collection");
        let _ = std::fs::remove_dir_all(&dir);
        let credits = serde_json::to_value(&collection.geocoding.attribution).expect("json");
        assert_eq!(
            credits,
            json!([
                {
                    "text": "© OpenStreetMap contributors",
                    "licence": "ODbL 1.0",
                    "url": "https://www.openstreetmap.org/copyright"
                },
                {
                    "text": "OpenAddresses ar/c/city_of_buenos_aires",
                    "licence": "CC BY 2.5 AR",
                    "url": "https://creativecommons.org/licenses/by/2.5/ar"
                }
            ])
        );
    }

    fn source(object_type: Option<ObjectType>) -> records::Source {
        records::Source {
            dataset: "osm".to_string(),
            object_type,
            object_id: object_type.map(|_| 1),
            derived_from: None,
            record_count: None,
        }
    }

    #[test]
    fn accuracy_says_how_the_point_relates_to_the_thing() {
        let node = source(Some(ObjectType::Node));
        let way = source(Some(ObjectType::Way));
        let derived = source(None);
        for (precision, source, accuracy) in [
            (records::PointPrecision::Point, &node, Accuracy::Rooftop),
            (
                records::PointPrecision::Estimated,
                &derived,
                Accuracy::Interpolated,
            ),
            (
                records::PointPrecision::RepresentativePoint,
                &way,
                Accuracy::Street,
            ),
            (records::PointPrecision::Centroid, &way, Accuracy::Centroid),
            (
                records::PointPrecision::Centroid,
                &node,
                Accuracy::Approximate,
            ),
            (
                records::PointPrecision::Centroid,
                &derived,
                Accuracy::Approximate,
            ),
        ] {
            assert_eq!(Accuracy::of(precision, source), accuracy, "{precision:?}");
        }
    }

    #[test]
    fn address_keeps_only_what_the_record_states() {
        let address = Address::from(&AddressComponents {
            block_number_len: None,
            number: "12".to_string(),
            street: Some("King Street West".to_string()),
            place: None,
            unit: Some(" ".to_string()),
            locality: Some(String::new()),
            areas: vec!["Old Town".to_string(), String::new()],
            region: None,
            postcode: Some("M5V 1A1".to_string()),
            country: None,
        });
        assert_eq!(
            serde_json::to_value(&address).expect("address JSON"),
            json!({
                "housenumber": "12",
                "street": "King Street West",
                "areas": ["Old Town"],
                "postcode": "M5V 1A1",
            })
        );
        assert!(Address::default().is_empty());
    }

    #[test]
    fn estimated_number_is_read_from_the_head_of_the_label() {
        assert_eq!(
            estimated_number("50 Queen Street, Toronto").expect("number"),
            50
        );
        assert!(estimated_number("Queen Street, Toronto").is_err());
    }

    #[test]
    fn context_areas_run_precise_to_broad_with_the_place_last() {
        let layers = ["country", "place", "postcode", "locality", "neighbourhood"];
        let mut sorted = layers.to_vec();
        sorted.sort_by_key(|layer| breadth(layer));
        assert_eq!(
            sorted,
            ["postcode", "neighbourhood", "locality", "country", "place"]
        );
    }
}
