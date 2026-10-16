//! The response contract of `/search`, `/autocomplete` and `/reverse`
//! (ADR 0022).
//!
//! Every response is one GeoJSON FeatureCollection: a `geocoding` block that
//! says what was asked, then one Feature per answer. Forward features carry a
//! `score`, reverse features a `distance_m`, and nothing else differs. The
//! types here are the only definition of that shape. The contract tests
//! compare real answers with the committed examples in
//! `contract/responses/`, so a change to the shape lands with its examples.
//!
//! The same features are the result columns of `batch-geocode`
//! ([`flat_columns`]), so a CSV row and a JSON answer never drift apart
//! either.

use std::collections::HashMap;

use anyhow::{Context as _, Result};
use serde::Serialize;

use crate::{
    labels,
    pack::{PackReader, RecordId, RecordPointPrecision, RecordSource, RecordSummary},
    record::{AddressComponents, InterpolationAddressComponents, Layer, OsmObjectType},
    reverse::{ReverseGeocodeResponse, ReverseMatchKind, ReversePointPrecision},
    search::TextSearchHit,
};

/// A forward answer: `/search` and `/autocomplete`.
pub type ForwardCollection = FeatureCollection<ForwardGeocoding, Forward>;
/// A reverse answer: `/reverse`.
pub type ReverseCollection = FeatureCollection<ReverseGeocoding, Reverse>;
pub type ForwardFeature = Feature<Forward>;
pub type ReverseFeature = Feature<Reverse>;

/// A GeoJSON FeatureCollection of geocoding answers, best or most precise
/// first.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FeatureCollection<G, R> {
    #[serde(rename = "type")]
    pub kind: FeatureCollectionType,
    /// What was asked, the contract version and the data attribution.
    pub geocoding: G,
    pub features: Vec<Feature<R>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum FeatureCollectionType {
    FeatureCollection,
}

/// The contract version. It changes only when a field changes meaning or
/// goes away; new optional fields keep it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Version {
    #[serde(rename = "1")]
    V1,
}

/// The licence every answer is given under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Attribution {
    #[serde(rename = "© OpenStreetMap contributors, ODbL")]
    OpenStreetMap,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ForwardGeocoding {
    pub version: Version,
    /// The query text as received, trimmed.
    pub query: String,
    pub attribution: Attribution,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReverseGeocoding {
    pub version: Version,
    /// The point asked about.
    pub point: LonLat,
    pub attribution: Attribution,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LonLat {
    pub lon: f64,
    pub lat: f64,
}

/// One answer: a record of the Pack, or a house number placed on one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Feature<R> {
    #[serde(rename = "type")]
    pub kind: FeatureType,
    pub geometry: Point,
    pub properties: Properties<R>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum FeatureType {
    Feature,
}

/// A GeoJSON Point, `[lon, lat]`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Point {
    #[serde(rename = "type")]
    pub kind: PointType,
    pub coordinates: [f64; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PointType {
    Point,
}

/// What a feature is, where it lies and where it comes from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Properties<R> {
    /// Stable source id, for example `osm:node:123`.
    pub id: String,
    /// Row of the record in this Pack. Not stable across Pack builds.
    pub record_id: RecordId,
    pub layer: Layer,
    pub accuracy: Accuracy,
    /// The thing itself: a POI's or place's name, a street's name, or an
    /// address's house number and street (`12 King Street West`).
    pub name: String,
    /// One line to show, the name followed by where it is.
    pub label: String,
    /// A POI's category, for example `amenity:cafe`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// The address parts the record itself states, and only those.
    #[serde(skip_serializing_if = "Address::is_empty")]
    pub address: Address,
    /// The areas the record lies in, from the Pack's boundaries.
    #[serde(skip_serializing_if = "Context::is_empty")]
    pub context: Context,
    pub source: RecordSource,
    #[serde(flatten)]
    pub rank: R,
}

/// The field only forward features carry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Forward {
    /// Text relevance: higher is better, comparable only within one answer.
    pub score: f32,
}

/// The field only reverse features carry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Reverse {
    /// Metres from the point asked about to the feature's geometry, for the
    /// features found by distance. The areas around the point carry none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_m: Option<f64>,
}

/// How the feature's coordinates relate to the thing it names.
///
/// | accuracy | Pack point |
/// | --- | --- |
/// | `rooftop` | the point of an address or POI (`point`) |
/// | `interpolated` | a house number placed along a range or between neighbours (`estimated`) |
/// | `street` | a point on a street or range line (`representative_point`, reverse `street`) |
/// | `centroid` | the centre of a drawn way or relation (`centroid`) |
/// | `approximate` | a node or derived point standing for something larger: a place node, a postcode |
///
/// Accuracies order from the most precise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
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
    pub fn of(precision: RecordPointPrecision, source: &RecordSource) -> Self {
        match precision {
            RecordPointPrecision::Point => Self::Rooftop,
            RecordPointPrecision::Estimated => Self::Interpolated,
            RecordPointPrecision::RepresentativePoint => Self::Street,
            RecordPointPrecision::Centroid => match source.object_type {
                Some(OsmObjectType::Way | OsmObjectType::Relation) => Self::Centroid,
                Some(OsmObjectType::Node) | None => Self::Approximate,
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

/// Address parts as the record states them. A part the record leaves out is
/// absent, never filled in from the areas around it: those are the
/// `context`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Address {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub housenumber: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub street: Option<String>,
    /// `addr:place`: what the house number is on where there is no street.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
    /// The areas below the locality the address names, outermost first
    /// (`addr:suburb`, `addr:quarter`, `addr:neighbourhood`).
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

/// The areas a record lies in, one per level: the admin areas most specific
/// first, then the place area.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Context {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neighbourhood: Option<ContextArea>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locality: Option<ContextArea>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub district: Option<ContextArea>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<ContextArea>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<ContextArea>,
    /// A named area outside the admin levels, mostly an island. It can be
    /// smaller than a locality or larger than a region (Honshu).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<ContextArea>,
}

impl Context {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextArea {
    /// The area's source id, as a feature's `id`.
    pub id: String,
    pub name: String,
}

impl<G, R> FeatureCollection<G, R> {
    fn new(geocoding: G, features: Vec<Feature<R>>) -> Self {
        Self {
            kind: FeatureCollectionType::FeatureCollection,
            geocoding,
            features,
        }
    }
}

impl<R> Feature<R> {
    fn new(lon: f64, lat: f64, properties: Properties<R>) -> Self {
        Self {
            kind: FeatureType::Feature,
            geometry: Point {
                kind: PointType::Point,
                coordinates: [lon, lat],
            },
            properties,
        }
    }

    pub fn lon(&self) -> f64 {
        self.geometry.coordinates[0]
    }

    pub fn lat(&self) -> f64 {
        self.geometry.coordinates[1]
    }
}

/// The areas records lie in, each read from the Pack once per answer: the
/// hits of one query mostly share them.
pub struct Contexts<'a> {
    pack: &'a PackReader,
    areas: HashMap<RecordId, Option<ContextArea>>,
}

impl<'a> Contexts<'a> {
    pub fn new(pack: &'a PackReader) -> Self {
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
        let tuple = context.admin_context;
        Ok(Context {
            neighbourhood: self.area(tuple.neighbourhood_record_id)?,
            locality: self.area(tuple.locality_record_id)?,
            district: self.area(tuple.district_record_id)?,
            region: self.area(tuple.region_record_id)?,
            country: self.area(tuple.country_record_id)?,
            place: self.area(tuple.place_record_id)?,
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
    pack: &PackReader,
    query: String,
    hits: Vec<TextSearchHit>,
) -> Result<ForwardCollection> {
    let mut contexts = Contexts::new(pack);
    let features = hits
        .into_iter()
        .map(|hit| forward_feature(&mut contexts, hit))
        .collect::<Result<Vec<_>>>()?;
    Ok(FeatureCollection::new(
        ForwardGeocoding {
            version: Version::V1,
            query,
            attribution: Attribution::OpenStreetMap,
        },
        features,
    ))
}

/// One search hit as a feature. The hit's summary already holds what search
/// placed or estimated (an interpolated house number, a street's centre
/// line), so nothing is read again but its context.
pub fn forward_feature(contexts: &mut Contexts, hit: TextSearchHit) -> Result<ForwardFeature> {
    let point = hit
        .record
        .point
        .with_context(|| format!("search hit {} has no point", hit.record.id))?;
    let accuracy = Accuracy::of(point.precision, &hit.record.source);
    let properties = properties(
        contexts,
        hit.record_id,
        hit.record,
        accuracy,
        Forward { score: hit.score },
    )?;
    Ok(Feature::new(point.lon, point.lat, properties))
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
pub fn reverse_collection(
    pack: &PackReader,
    response: &ReverseGeocodeResponse,
) -> Result<ReverseCollection> {
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
            .with_context(|| format!("record {record_id} has no point"))?;
        let (lon, lat) = result
            .point
            .as_ref()
            .map_or((own_point.lon, own_point.lat), |point| {
                (point.lon, point.lat)
            });
        let accuracy = match result.point.as_ref().map(|point| point.precision) {
            Some(ReversePointPrecision::Estimated) => Accuracy::Interpolated,
            Some(ReversePointPrecision::Street) => Accuracy::Street,
            Some(ReversePointPrecision::Point | ReversePointPrecision::Context) | None => {
                Accuracy::of(own_point.precision, &summary.source)
            }
        };
        if result.match_kind == ReverseMatchKind::EstimatedAddress {
            let number = estimated_number(&result.label)?;
            let interpolation = pack
                .interpolation(record_id)?
                .with_context(|| format!("record {record_id} is not an interpolation"))?;
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
        let properties = properties(
            &mut contexts,
            record_id,
            summary,
            accuracy,
            Reverse {
                distance_m: result.distance_m,
            },
        )?;
        features.push(Feature::new(lon, lat, properties));
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
        let point = summary
            .point
            .with_context(|| format!("context record {record_id} has no point"))?;
        let accuracy = Accuracy::of(point.precision, &summary.source);
        let properties = properties(
            &mut contexts,
            record_id,
            summary,
            accuracy,
            Reverse { distance_m: None },
        )?;
        features.push(Feature::new(point.lon, point.lat, properties));
    }

    Ok(FeatureCollection::new(
        ReverseGeocoding {
            version: Version::V1,
            point: LonLat {
                lon: response.lon,
                lat: response.lat,
            },
            attribution: Attribution::OpenStreetMap,
        },
        features,
    ))
}

/// The house number at the start of an estimated address's label.
fn estimated_number(label: &str) -> Result<u32> {
    label
        .split_whitespace()
        .next()
        .and_then(|number| number.parse().ok())
        .with_context(|| format!("estimated address label {label:?} starts with no number"))
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

fn properties<R>(
    contexts: &mut Contexts,
    record_id: RecordId,
    summary: RecordSummary,
    accuracy: Accuracy,
    rank: R,
) -> Result<Properties<R>> {
    let layer = Layer::parse(&summary.layer)
        .with_context(|| format!("record {record_id} has unknown layer {}", summary.layer))?;
    Ok(Properties {
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
        rank,
    })
}

/// The result columns of one forward feature, by name, in the order
/// `batch-geocode` writes them. With no feature the same columns come back
/// empty, so this is also the one list of column names.
pub fn flat_columns(feature: Option<&ForwardFeature>) -> Vec<(&'static str, String)> {
    let properties = feature.map(|feature| &feature.properties);
    let field = |value: &dyn Fn(&Properties<Forward>) -> Option<String>| {
        properties.and_then(value).unwrap_or_default()
    };
    let area = |area: fn(&Context) -> &Option<ContextArea>| {
        properties
            .and_then(|properties| area(&properties.context).as_ref())
            .map(|area| area.name.clone())
            .unwrap_or_default()
    };
    let coordinate = |index: usize| {
        feature
            .map(|feature| format!("{:.7}", feature.geometry.coordinates[index]))
            .unwrap_or_default()
    };
    vec![
        ("score", field(&|p| Some(p.rank.score.to_string()))),
        ("record_id", field(&|p| Some(p.record_id.to_string()))),
        ("id", field(&|p| Some(p.id.clone()))),
        ("layer", field(&|p| Some(p.layer.as_str().to_string()))),
        (
            "accuracy",
            field(&|p| Some(p.accuracy.as_str().to_string())),
        ),
        ("name", field(&|p| Some(p.name.clone()))),
        ("label", field(&|p| Some(p.label.clone()))),
        ("category", field(&|p| p.category.clone())),
        ("lat", coordinate(1)),
        ("lon", coordinate(0)),
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
            "context_neighbourhood",
            area(|context| &context.neighbourhood),
        ),
        ("context_locality", area(|context| &context.locality)),
        ("context_district", area(|context| &context.district)),
        ("context_region", area(|context| &context.region)),
        ("context_country", area(|context| &context.country)),
        ("context_place", area(|context| &context.place)),
        ("source_dataset", field(&|p| Some(p.source.dataset.clone()))),
        (
            "source_object_type",
            field(&|p| {
                p.source
                    .object_type
                    .map(|kind| labels::osm_object_type_name(kind).to_string())
            }),
        ),
        (
            "source_object_id",
            field(&|p| p.source.object_id.map(|id| id.to_string())),
        ),
        (
            "source_derived_from",
            field(&|p| p.source.derived_from.clone()),
        ),
        (
            "source_record_count",
            field(&|p| p.source.record_count.map(|count| count.to_string())),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn source(object_type: Option<OsmObjectType>) -> RecordSource {
        RecordSource {
            dataset: "osm".to_string(),
            object_type,
            object_id: object_type.map(|_| 1),
            derived_from: None,
            record_count: None,
        }
    }

    #[test]
    fn accuracy_says_how_the_point_relates_to_the_thing() {
        let node = source(Some(OsmObjectType::Node));
        let way = source(Some(OsmObjectType::Way));
        let derived = source(None);
        for (precision, source, accuracy) in [
            (RecordPointPrecision::Point, &node, Accuracy::Rooftop),
            (
                RecordPointPrecision::Estimated,
                &derived,
                Accuracy::Interpolated,
            ),
            (
                RecordPointPrecision::RepresentativePoint,
                &way,
                Accuracy::Street,
            ),
            (RecordPointPrecision::Centroid, &way, Accuracy::Centroid),
            (RecordPointPrecision::Centroid, &node, Accuracy::Approximate),
            (
                RecordPointPrecision::Centroid,
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
