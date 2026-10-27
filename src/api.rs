//! The response contract of `/search`, `/autocomplete` and `/reverse`
//! (ADR 0022).
//!
//! Every response is one GeoJSON FeatureCollection: a `geocoding` block that
//! says what was asked, then one Feature per answer. Forward features carry a
//! `score` and, from a calibrated Pack, a `confidence`; reverse features a
//! `distance_m`; nothing else differs. The types here are the only definition of that shape. The contract tests
//! compare real answers with the committed examples in
//! `contract/responses/`, so a change to the shape lands with its examples.
//!
//! The same features are the result columns of `geocode batch`
//! ([`flat_columns`]), so a CSV row and a JSON answer never drift apart
//! either.

use std::collections::HashMap;

use crate::error::{Error, Result};
use serde::Serialize;
use utoipa::ToSchema;

use crate::{
    labels,
    pack::{Reader, RecordId, Source, Summary},
    record::{AddressComponents, InterpolationAddressComponents, Layer, ObjectType, PlaceLayer},
    records, reverse,
    reverse::{GeocodeResponse, MatchKind},
    search::TextSearchHit,
};

// Forward and reverse are written out rather than shared through a generic:
// a generic reached through a type alias is transparent to the schema
// generator, which then emitted one `FeatureCollection` for both and
// described each endpoint with the other's fields. Construction is still
// shared, by [`common`] and the builders below.
/// A GeoJSON FeatureCollection answering `/search`, `/autocomplete` or
/// `/geocode`.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ForwardCollection {
    #[serde(rename = "type")]
    pub kind: FeatureCollectionType,
    /// The query, API version and data attribution.
    pub geocoding: ForwardGeocoding,
    pub features: Vec<ForwardFeature>,
}

// See [`ForwardCollection`] for why the two directions are written out.
/// A GeoJSON FeatureCollection answering `/reverse`.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ReverseCollection {
    #[serde(rename = "type")]
    pub kind: FeatureCollectionType,
    /// The queried point, API version and data attribution.
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

/// Attribution required by the data licence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub enum Attribution {
    #[serde(rename = "© OpenStreetMap contributors, ODbL")]
    OpenStreetMap,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ForwardGeocoding {
    pub version: Version,
    /// Query as received, trimmed, with each run of whitespace collapsed to
    /// one space.
    pub query: String,
    pub attribution: Attribution,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ReverseGeocoding {
    pub version: Version,
    /// Queried point.
    pub point: LonLat,
    pub attribution: Attribution,
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
    /// Stable ID from the source data, such as `osm:node:123`.
    pub id: String,
    /// Row of the record in this Pack. Changes between Pack builds.
    pub record_id: RecordId,
    pub layer: Layer,
    pub accuracy: Accuracy,
    /// House number and street for an address (`12 King Street West`), or
    /// the name of a street, area or POI.
    pub name: String,
    /// One-line display label: `name` followed by its location.
    pub label: String,
    /// POI category, such as `amenity:cafe`. Present only on POIs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Address parts stated by the source record.
    #[serde(skip_serializing_if = "Address::is_empty")]
    pub address: Address,
    /// Administrative areas containing the result.
    #[serde(skip_serializing_if = "Context::is_empty")]
    pub context: Context,
    pub source: Source,
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
    /// the Pack is calibrated. `0` on a `fallback` result.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 0, maximum = 1)]
    pub confidence: Option<f32>,
    /// `true` when no match was found and the result is the most precise
    /// area the query names instead: its postcode, a town or other area,
    /// its region or its country. `layer` says which. Absent otherwise.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[schema(value_type = Option<bool>)]
    pub fallback: bool,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, ToSchema)]
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
    pub fn of(precision: records::PointPrecision, source: &Source) -> Self {
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
}

impl Address {
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ContextArea {
    /// Stable ID from the source data, as in a result's `id`.
    pub id: String,
    pub name: String,
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
}

impl<'a> Contexts<'a> {
    pub fn new(pack: &'a Reader) -> Self {
        Self {
            pack,
            areas: HashMap::new(),
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
            .map(|record| ContextArea {
                id: record.id,
                name: record.name,
            });
        self.areas.insert(record_id, area.clone());
        Ok(area)
    }
}

/// The answer to `/search` or `/autocomplete`: one feature per hit, in the
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
    Ok(ForwardCollection {
        kind: FeatureCollectionType::FeatureCollection,
        geocoding: ForwardGeocoding {
            version: Version::V1,
            query,
            attribution: Attribution::OpenStreetMap,
        },
        features,
    })
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
    let properties = ForwardProperties {
        common: common(contexts, hit.record_id, hit.record, accuracy)?,
        score: hit.score,
        confidence: hit.confidence,
        fallback: hit.fallback,
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
        if let Some(id) = &result.id {
            summary.id = id.clone();
        }
        if let Some(layer) = &result.layer {
            summary.layer = layer.clone();
        }
        summary.label = result.label.clone();
        let properties = ReverseProperties {
            common: common(&mut contexts, record_id, summary, accuracy)?,
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

    Ok(ReverseCollection {
        kind: FeatureCollectionType::FeatureCollection,
        geocoding: ReverseGeocoding {
            version: Version::V1,
            point: LonLat {
                lon: response.lon,
                lat: response.lat,
            },
            attribution: Attribution::OpenStreetMap,
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
    Ok(Common {
        id: summary.id,
        record_id,
        layer,
        accuracy,
        name: summary.name,
        label: summary.label,
        category: summary.category,
        address: summary.address,
        context: contexts.of(record_id)?,
        source: summary.source,
    })
}

/// The result columns of one forward feature, by name, in the order
/// `geocode batch` writes them. With no feature the same columns come back
/// empty, so this is also the one list of column names.
pub fn flat_columns(feature: Option<&ForwardFeature>) -> Vec<(&'static str, String)> {
    let properties = feature.map(|feature| &feature.properties);
    let field = |value: &dyn Fn(&ForwardProperties) -> Option<String>| {
        properties.and_then(value).unwrap_or_default()
    };
    let area = |area: fn(&Context) -> &Option<ContextArea>| {
        properties
            .and_then(|properties| area(&properties.common.context).as_ref())
            .map(|area| area.name.clone())
            .unwrap_or_default()
    };
    let coordinate = |index: usize| {
        feature
            .map(|feature| format!("{:.7}", feature.geometry.coordinates[index]))
            .unwrap_or_default()
    };
    vec![
        ("score", field(&|p| Some(p.score.to_string()))),
        (
            "confidence",
            field(&|p| p.confidence.map(|confidence| confidence.to_string())),
        ),
        (
            "record_id",
            field(&|p| Some(p.common.record_id.to_string())),
        ),
        ("id", field(&|p| Some(p.common.id.clone()))),
        (
            "layer",
            field(&|p| Some(p.common.layer.as_str().to_string())),
        ),
        (
            "accuracy",
            field(&|p| Some(p.common.accuracy.as_str().to_string())),
        ),
        ("name", field(&|p| Some(p.common.name.clone()))),
        ("label", field(&|p| Some(p.common.label.clone()))),
        ("category", field(&|p| p.common.category.clone())),
        ("lat", coordinate(1)),
        ("lon", coordinate(0)),
        (
            "address_housenumber",
            field(&|p| p.common.address.housenumber.clone()),
        ),
        ("address_unit", field(&|p| p.common.address.unit.clone())),
        (
            "address_street",
            field(&|p| p.common.address.street.clone()),
        ),
        ("address_place", field(&|p| p.common.address.place.clone())),
        (
            "address_areas",
            field(&|p| Some(p.common.address.areas.join("; "))),
        ),
        (
            "address_locality",
            field(&|p| p.common.address.locality.clone()),
        ),
        (
            "address_region",
            field(&|p| p.common.address.region.clone()),
        ),
        (
            "address_postcode",
            field(&|p| p.common.address.postcode.clone()),
        ),
        (
            "address_country",
            field(&|p| p.common.address.country.clone()),
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
        (
            "source_dataset",
            field(&|p| Some(p.common.source.dataset.clone())),
        ),
        (
            "source_object_type",
            field(&|p| {
                p.common
                    .source
                    .object_type
                    .map(|kind| labels::object_type_name(kind).to_string())
            }),
        ),
        (
            "source_object_id",
            field(&|p| p.common.source.object_id.map(|id| id.to_string())),
        ),
        (
            "source_derived_from",
            field(&|p| p.common.source.derived_from.clone()),
        ),
        (
            "source_record_count",
            field(&|p| p.common.source.record_count.map(|count| count.to_string())),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn source(object_type: Option<ObjectType>) -> Source {
        Source {
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
