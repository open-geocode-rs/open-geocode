use std::{collections::BTreeSet, path::Path, sync::Arc};

use anyhow::Result;
use serde::Serialize;

use crate::{
    labels,
    pack::{self, ContextRecord, RecordId},
    record::{AddressComponents, InterpolationAddressComponents, Layer, PoiRecord, Record},
    spatial_index,
};

const ADDRESS_RADIUS_M: f64 = 30.0;
const INTERPOLATION_RADIUS_M: f64 = 30.0;
const STREET_RADIUS_M: f64 = 30.0;
const CONTEXT_RADIUS_M: f64 = 50_000.0;
const CANDIDATE_LIMIT: usize = 16;

#[derive(Debug)]
pub struct PackReverseGeocoder {
    pack: Arc<pack::Reader>,
    spatial: spatial_index::Reader,
}

#[derive(Debug, Clone, Copy)]
pub struct GeocodeOptions {
    pub lon: f64,
    pub lat: f64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct GeocodeResponse {
    pub lon: f64,
    pub lat: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<GeocodeResult>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct GeocodeResult {
    pub match_kind: MatchKind,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_record_id: Option<RecordId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_m: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<Point>,
    pub context: Context,
    pub evidence: Evidence,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    ExplicitAddress,
    EstimatedAddress,
    NearestStreet,
    ContextOnly,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Point {
    pub lon: f64,
    pub lat: f64,
    pub precision: PointPrecision,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PointPrecision {
    Point,
    Estimated,
    Street,
    Context,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Context {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub street: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub postcode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neighbourhood: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub district: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Evidence {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explicit_address_record_id: Option<RecordId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interpolation_record_id: Option<RecordId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub street_record_id: Option<RecordId>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_record_ids: Vec<RecordId>,
}

impl PackReverseGeocoder {
    pub fn open(pack_path: impl AsRef<Path>) -> Result<Self> {
        Self::from_pack(Arc::new(pack::Reader::open(pack_path)?))
    }

    /// Open the spatial index using an existing shared pack reader.
    pub fn from_pack(pack: Arc<pack::Reader>) -> Result<Self> {
        let spatial = spatial_index::Reader::open(pack.container(), pack.records().clone())?;
        Ok(Self { pack, spatial })
    }

    pub fn reverse(&self, options: GeocodeOptions) -> Result<GeocodeResponse> {
        let mut context = Context::default();
        let mut context_record_ids = Vec::new();

        let mut result = self.explicit_address(options, &mut context, &mut context_record_ids)?;
        if result.is_none() {
            result = self.estimated_address(options, &mut context, &mut context_record_ids)?;
        }
        if result.is_none() {
            result = self.nearest_street(options, &mut context, &mut context_record_ids)?;
        }
        if result.is_none() {
            result = self.context_only(options, &mut context, &mut context_record_ids)?;
        }

        Ok(GeocodeResponse {
            lon: options.lon,
            lat: options.lat,
            result,
        })
    }

    fn explicit_address(
        &self,
        options: GeocodeOptions,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<GeocodeResult>> {
        let Some(candidate) = self
            .spatial
            .address_candidates(options.lon, options.lat, ADDRESS_RADIUS_M, 1)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        // A POI that carries an address answers as that address.
        let record = self.pack.records().record(candidate.record_id)?;
        let address = match &record {
            Record::Address(record) => &record.address,
            Record::Poi(PoiRecord {
                address: Some(address),
                ..
            }) => address,
            _ => return Ok(None),
        };

        self.enrich_record_context(candidate.record_id, context, context_record_ids)?;
        apply_address_context(context, address);
        self.enrich_context(options, context, context_record_ids)?;

        Ok(Some(GeocodeResult {
            match_kind: MatchKind::ExplicitAddress,
            label: labels::address_label(address),
            primary_record_id: Some(candidate.record_id),
            id: Some(record.id()),
            layer: Some(record.layer().as_str().to_string()),
            distance_m: Some(candidate.distance_m),
            point: Some(Point {
                lon: candidate.lon,
                lat: candidate.lat,
                precision: PointPrecision::Point,
            }),
            context: context.clone(),
            evidence: Evidence {
                explicit_address_record_id: Some(candidate.record_id),
                context_record_ids: context_record_ids.clone(),
                ..Evidence::default()
            },
        }))
    }

    fn estimated_address(
        &self,
        options: GeocodeOptions,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<GeocodeResult>> {
        for candidate in self.spatial.segment_candidates(
            options.lon,
            options.lat,
            Layer::Interpolation,
            INTERPOLATION_RADIUS_M,
            CANDIDATE_LIMIT,
        )? {
            let Some(interpolation) = self.pack.interpolation(candidate.record_id)? else {
                continue;
            };
            let number = interpolation.interpolation.number_at(candidate.fraction);
            self.enrich_record_context(candidate.record_id, context, context_record_ids)?;
            apply_interpolation_context(context, &interpolation.address);
            self.enrich_context(options, context, context_record_ids)?;
            let primary = labels::estimated_address_name(number, &interpolation.address)
                .unwrap_or_else(|| format!("{} {}", number, interpolation.name()));

            return Ok(Some(GeocodeResult {
                match_kind: MatchKind::EstimatedAddress,
                label: compose_label(&primary, context),
                primary_record_id: Some(candidate.record_id),
                id: Some(interpolation.id()),
                layer: Some("interpolation".to_string()),
                distance_m: Some(candidate.distance_m),
                point: Some(Point {
                    lon: candidate.closest_lon,
                    lat: candidate.closest_lat,
                    precision: PointPrecision::Estimated,
                }),
                context: context.clone(),
                evidence: Evidence {
                    interpolation_record_id: Some(candidate.record_id),
                    context_record_ids: context_record_ids.clone(),
                    ..Evidence::default()
                },
            }));
        }

        Ok(None)
    }

    fn nearest_street(
        &self,
        options: GeocodeOptions,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<GeocodeResult>> {
        let Some(candidate) = self
            .spatial
            .segment_candidates(options.lon, options.lat, Layer::Street, STREET_RADIUS_M, 1)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let Some(street) = self.pack.street(candidate.record_id)? else {
            return Ok(None);
        };

        set_if_missing(&mut context.street, street.name.clone());
        self.enrich_record_context(candidate.record_id, context, context_record_ids)?;
        self.enrich_context(options, context, context_record_ids)?;

        Ok(Some(GeocodeResult {
            match_kind: MatchKind::NearestStreet,
            label: compose_label(&street.label(), context),
            primary_record_id: Some(candidate.record_id),
            id: Some(street.id()),
            layer: Some("street".to_string()),
            distance_m: Some(candidate.distance_m),
            point: Some(Point {
                lon: candidate.closest_lon,
                lat: candidate.closest_lat,
                precision: PointPrecision::Street,
            }),
            context: context.clone(),
            evidence: Evidence {
                street_record_id: Some(candidate.record_id),
                context_record_ids: context_record_ids.clone(),
                ..Evidence::default()
            },
        }))
    }

    fn context_only(
        &self,
        options: GeocodeOptions,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<GeocodeResult>> {
        self.enrich_context(options, context, context_record_ids)?;
        let Some(primary_record_id) = context_record_ids.first().copied() else {
            return Ok(None);
        };
        let Some(record) = self.pack.context_record(primary_record_id)? else {
            return Ok(None);
        };
        let Some(label) = context_only_label(context).or_else(|| Some(record.label.clone())) else {
            return Ok(None);
        };

        Ok(Some(GeocodeResult {
            match_kind: MatchKind::ContextOnly,
            label,
            primary_record_id: Some(primary_record_id),
            id: Some(record.id),
            layer: Some(record.layer),
            distance_m: None,
            point: record.point.map(context_point),
            context: context.clone(),
            evidence: Evidence {
                context_record_ids: context_record_ids.clone(),
                ..Evidence::default()
            },
        }))
    }

    fn enrich_context(
        &self,
        options: GeocodeOptions,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<()> {
        let mut seen = context_record_ids.iter().copied().collect::<BTreeSet<_>>();
        for candidate in self.spatial.context_candidates(
            options.lon,
            options.lat,
            CONTEXT_RADIUS_M,
            CANDIDATE_LIMIT,
        )? {
            if !seen.insert(candidate.record_id) {
                continue;
            }
            if let Some(record) = self.pack.context_record(candidate.record_id)?
                && apply_context_record(context, &record)
            {
                context_record_ids.push(candidate.record_id);
            }
        }
        Ok(())
    }

    fn enrich_record_context(
        &self,
        record_id: RecordId,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<()> {
        let Some(boundary_context) = self.pack.boundary_context(record_id)? else {
            return Ok(());
        };
        let mut seen = context_record_ids.iter().copied().collect::<BTreeSet<_>>();
        for context_record_id in boundary_context.admin_context.parent_record_ids() {
            if !seen.insert(context_record_id) {
                continue;
            }
            if let Some(record) = self.pack.context_record(context_record_id)?
                && apply_context_record(context, &record)
            {
                context_record_ids.push(context_record_id);
            }
        }
        Ok(())
    }
}

fn apply_address_context(context: &mut Context, address: &AddressComponents) {
    set_option_if_missing(&mut context.street, address.street.clone());
    set_option_if_missing(&mut context.place, address.place.clone());
    set_option_if_missing(&mut context.postcode, address.postcode.clone());
    set_option_if_missing(&mut context.locality, address.locality.clone());
    set_option_if_missing(&mut context.region, address.region.clone());
    set_option_if_missing(&mut context.country, address.country.clone());
}

fn apply_interpolation_context(context: &mut Context, address: &InterpolationAddressComponents) {
    set_option_if_missing(&mut context.street, address.street.clone());
    set_option_if_missing(&mut context.place, address.place.clone());
    set_option_if_missing(&mut context.postcode, address.postcode.clone());
    set_option_if_missing(&mut context.locality, address.locality.clone());
    set_option_if_missing(&mut context.region, address.region.clone());
    set_option_if_missing(&mut context.country, address.country.clone());
}

fn apply_context_record(context: &mut Context, record: &ContextRecord) -> bool {
    match record.layer.as_str() {
        "postcode" => set_option_if_missing(&mut context.postcode, record.postcode.clone()),
        "neighbourhood" => set_if_missing(&mut context.neighbourhood, record.name.clone()),
        "locality" => set_if_missing(&mut context.locality, record.name.clone()),
        "district" => set_if_missing(&mut context.district, record.name.clone()),
        "region" => set_if_missing(&mut context.region, record.name.clone()),
        "country" => set_if_missing(&mut context.country, record.name.clone()),
        "place" => set_if_missing(&mut context.place, record.name.clone()),
        _ => false,
    }
}

fn compose_label(primary: &str, context: &Context) -> String {
    let mut parts = vec![primary.to_string()];
    for part in [
        context.place.as_deref(),
        context.neighbourhood.as_deref(),
        context.locality.as_deref(),
        context.district.as_deref(),
        context.region.as_deref(),
        context.postcode.as_deref(),
        context.country.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if !parts.iter().any(|existing| same_text(existing, part)) {
            parts.push(part.to_string());
        }
    }
    parts.join(", ")
}

fn context_only_label(context: &Context) -> Option<String> {
    [
        context.place.as_deref(),
        context.neighbourhood.as_deref(),
        context.locality.as_deref(),
        context.district.as_deref(),
        context.region.as_deref(),
        context.postcode.as_deref(),
        context.country.as_deref(),
    ]
    .into_iter()
    .flatten()
    .next()
    .map(|primary| compose_label(primary, context))
}

fn context_point(point: crate::pack::Point) -> Point {
    Point {
        lon: point.lon,
        lat: point.lat,
        precision: PointPrecision::Context,
    }
}

fn set_option_if_missing(target: &mut Option<String>, value: Option<String>) -> bool {
    if let Some(value) = value {
        set_if_missing(target, value)
    } else {
        false
    }
}

fn set_if_missing(target: &mut Option<String>, value: String) -> bool {
    if target.is_none() && !value.trim().is_empty() {
        *target = Some(value);
        true
    } else {
        false
    }
}

fn same_text(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use geojson::{Geometry, GeometryValue};

    use crate::{
        context::AdminContextTuple,
        pack::{RecordContext, Writer},
        record::{
            AddressRecord, InterpolationRange, LocationPrecision, OsmObjectType, PlaceLayer,
            PlaceRecord, Record, SourceProvenance, StreetRecord, point_geometry,
        },
    };

    use super::*;

    #[test]
    fn reverse_prefers_explicit_address() {
        let temp_dir = temp_pack_dir("explicit");
        let mut writer = Writer::create(&temp_dir).expect("writer");
        writer.write(&street_record().into(), None).expect("street");
        writer
            .write(&address_record().into(), None)
            .expect("address");
        writer.finish().expect("finish");

        let geocoder = PackReverseGeocoder::open(&temp_dir).expect("geocoder");
        let response = geocoder
            .reverse(GeocodeOptions {
                lon: -79.0,
                lat: 43.0,
            })
            .expect("reverse");
        let result = response.result.expect("result");

        assert_eq!(result.match_kind, MatchKind::ExplicitAddress);
        assert_eq!(result.primary_record_id, Some(1));
        assert_eq!(result.evidence.explicit_address_record_id, Some(1));

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn reverse_estimates_address_from_interpolation() {
        let temp_dir = temp_pack_dir("interpolation");
        let mut writer = Writer::create(&temp_dir).expect("writer");
        writer
            .write(&interpolation_record().into(), None)
            .expect("interpolation");
        writer.finish().expect("finish");

        let geocoder = PackReverseGeocoder::open(&temp_dir).expect("geocoder");
        let response = geocoder
            .reverse(GeocodeOptions {
                lon: -79.00001,
                lat: 43.0005,
            })
            .expect("reverse");
        let result = response.result.expect("result");

        assert_eq!(result.match_kind, MatchKind::EstimatedAddress);
        assert!(result.label.starts_with("50 Queen Street"));
        assert_eq!(result.evidence.interpolation_record_id, Some(0));

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn reverse_falls_back_to_street() {
        let temp_dir = temp_pack_dir("street");
        let mut writer = Writer::create(&temp_dir).expect("writer");
        writer.write(&street_record().into(), None).expect("street");
        writer.finish().expect("finish");

        let geocoder = PackReverseGeocoder::open(&temp_dir).expect("geocoder");
        let response = geocoder
            .reverse(GeocodeOptions {
                lon: -79.00001,
                lat: 43.0005,
            })
            .expect("reverse");
        let result = response.result.expect("result");

        assert_eq!(result.match_kind, MatchKind::NearestStreet);
        assert!(result.label.starts_with("Queen Street"));
        assert_eq!(result.evidence.street_record_id, Some(0));

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn reverse_prefers_materialized_boundary_context_over_source_locality() {
        let temp_dir = temp_pack_dir("boundary-context");
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let locality_id = writer
            .write(&Record::Place(PlaceLayer::Locality, place_record()), None)
            .expect("place");
        let mut address = address_record();
        address.address.locality = Some("North York".to_string());
        writer
            .write(
                &address.into(),
                Some(RecordContext {
                    admin_context: AdminContextTuple {
                        locality_record_id: Some(locality_id),
                        ..AdminContextTuple::default()
                    },
                    flags: 0,
                }),
            )
            .expect("address");
        writer.finish().expect("finish");

        let geocoder = PackReverseGeocoder::open(&temp_dir).expect("geocoder");
        let response = geocoder
            .reverse(GeocodeOptions {
                lon: -79.0,
                lat: 43.0,
            })
            .expect("reverse");
        let result = response.result.expect("result");

        assert_eq!(result.context.locality.as_deref(), Some("Toronto"));
        assert_eq!(result.evidence.context_record_ids, vec![locality_id]);

        let _ = fs::remove_dir_all(temp_dir);
    }

    fn temp_pack_dir(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "open-geocode-reverse-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn reverse_answers_with_the_address_of_a_poi_but_not_its_name() {
        let temp_dir = temp_pack_dir("poi");
        let mut writer = Writer::create(&temp_dir).expect("writer");
        writer.write(&street_record().into(), None).expect("street");
        // An unaddressed POI on top of the query point is not an address.
        writer
            .write(
                &PoiRecord {
                    alt_names: Vec::new(),
                    name: "Corner Bench Park".to_string(),
                    category: "leisure:park".to_string(),
                    address: None,
                    geometry: point_geometry(-79.0, 43.0),
                    location_precision: LocationPrecision::Point,
                    source: SourceProvenance::osm(OsmObjectType::Node, 40),
                }
                .into(),
                None,
            )
            .expect("park");
        writer
            .write(
                &PoiRecord {
                    alt_names: Vec::new(),
                    name: "Queen Bakery".to_string(),
                    category: "shop:bakery".to_string(),
                    address: Some(address_record().address),
                    geometry: point_geometry(-79.0, 43.00005),
                    location_precision: LocationPrecision::Centroid,
                    source: SourceProvenance::osm(OsmObjectType::Way, 41),
                }
                .into(),
                None,
            )
            .expect("bakery");
        writer.finish().expect("finish");

        let geocoder = PackReverseGeocoder::open(&temp_dir).expect("geocoder");
        let result = geocoder
            .reverse(GeocodeOptions {
                lon: -79.0,
                lat: 43.0,
            })
            .expect("reverse")
            .result
            .expect("result");

        assert_eq!(result.match_kind, MatchKind::ExplicitAddress);
        assert_eq!(result.id.as_deref(), Some("osm:way:41"));
        assert_eq!(result.layer.as_deref(), Some("poi"));
        assert_eq!(result.label, "10 Queen Street, Toronto, Ontario, Canada");
        assert_eq!(result.evidence.explicit_address_record_id, Some(2));

        let _ = fs::remove_dir_all(temp_dir);
    }

    fn address_record() -> AddressRecord {
        AddressRecord {
            address: AddressComponents {
                areas: Vec::new(),
                number: "10".to_string(),
                street: Some("Queen Street".to_string()),
                place: None,
                unit: None,
                locality: Some("Toronto".to_string()),
                region: Some("Ontario".to_string()),
                postcode: None,
                country: Some("Canada".to_string()),
            },
            geometry: point_geometry(-79.0, 43.0),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(OsmObjectType::Node, 10),
        }
    }

    fn place_record() -> PlaceRecord {
        PlaceRecord {
            alt_names: Vec::new(),
            name: "Toronto".to_string(),
            place_type: "admin_level:8".to_string(),
            geometry: point_geometry(-79.0, 43.0),
            source: SourceProvenance::osm(OsmObjectType::Relation, 99),
        }
    }

    fn interpolation_record() -> crate::record::InterpolationRecord {
        crate::record::InterpolationRecord {
            address: InterpolationAddressComponents {
                street: Some("Queen Street".to_string()),
                place: None,
                locality: Some("Toronto".to_string()),
                region: Some("Ontario".to_string()),
                postcode: None,
                country: Some("Canada".to_string()),
            },
            interpolation: InterpolationRange {
                kind: "even".to_string(),
                start: 2,
                end: 98,
                step: 2,
            },
            anchor_node_ids: [2, 98],
            geometry: line_geometry(),
            representative_point: [-79.0, 43.0005],
            source: SourceProvenance::osm(OsmObjectType::Way, 20),
        }
    }

    fn street_record() -> StreetRecord {
        StreetRecord {
            alt_names: Vec::new(),
            name: "Queen Street".to_string(),
            geometry: line_geometry(),
            representative_point: [-79.0, 43.0005],
            source: SourceProvenance::osm(OsmObjectType::Way, 30),
        }
    }

    fn line_geometry() -> Geometry {
        Geometry::new(GeometryValue::LineString {
            coordinates: vec![vec![-79.0, 43.0].into(), vec![-79.0, 43.001].into()],
        })
    }
}
