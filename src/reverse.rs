use std::{collections::BTreeSet, path::Path};

use anyhow::Result;
use serde::Serialize;

use crate::{
    pack::{ContextRecord, PackReader, RecordId},
    record::{AddressComponents, InterpolationAddressComponents, InterpolationRange},
    spatial_index::{PackSpatialIndexReader, SpatialLayer},
};

const ADDRESS_RADIUS_M: f64 = 30.0;
const INTERPOLATION_RADIUS_M: f64 = 30.0;
const STREET_RADIUS_M: f64 = 30.0;
const CONTEXT_RADIUS_M: f64 = 50_000.0;
const CANDIDATE_LIMIT: usize = 16;

#[derive(Debug)]
pub struct PackReverseGeocoder {
    pack: PackReader,
    spatial: PackSpatialIndexReader,
}

#[derive(Debug, Clone, Copy)]
pub struct ReverseGeocodeOptions {
    pub lon: f64,
    pub lat: f64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ReverseGeocodeResponse {
    pub lon: f64,
    pub lat: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ReverseGeocodeResult>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct ReverseGeocodeResult {
    pub match_kind: ReverseMatchKind,
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
    pub point: Option<ReversePoint>,
    pub context: ReverseContext,
    pub evidence: ReverseEvidence,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReverseMatchKind {
    ExplicitAddress,
    EstimatedAddress,
    NearestStreet,
    ContextOnly,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReversePoint {
    pub lon: f64,
    pub lat: f64,
    pub precision: ReversePointPrecision,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReversePointPrecision {
    Point,
    Estimated,
    Street,
    Context,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ReverseContext {
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
pub struct ReverseEvidence {
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
        let pack_path = pack_path.as_ref();
        Ok(Self {
            pack: PackReader::open(pack_path)?,
            spatial: PackSpatialIndexReader::open(pack_path)?,
        })
    }

    pub fn reverse(&self, options: ReverseGeocodeOptions) -> Result<ReverseGeocodeResponse> {
        let mut context = ReverseContext::default();
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

        Ok(ReverseGeocodeResponse {
            lon: options.lon,
            lat: options.lat,
            result,
        })
    }

    fn explicit_address(
        &self,
        options: ReverseGeocodeOptions,
        context: &mut ReverseContext,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<ReverseGeocodeResult>> {
        let Some(candidate) = self
            .spatial
            .point_candidates(
                options.lon,
                options.lat,
                SpatialLayer::Address,
                ADDRESS_RADIUS_M,
                1,
            )
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let Some(address) = self.pack.address(candidate.record_id)? else {
            return Ok(None);
        };

        self.enrich_record_context(candidate.record_id, context, context_record_ids)?;
        apply_address_context(context, &address.address);
        self.enrich_context(options, context, context_record_ids)?;

        Ok(Some(ReverseGeocodeResult {
            match_kind: ReverseMatchKind::ExplicitAddress,
            label: address.label,
            primary_record_id: Some(candidate.record_id),
            id: Some(address.id),
            layer: Some("address".to_string()),
            distance_m: Some(candidate.distance_m),
            point: Some(ReversePoint {
                lon: candidate.lon,
                lat: candidate.lat,
                precision: ReversePointPrecision::Point,
            }),
            context: context.clone(),
            evidence: ReverseEvidence {
                explicit_address_record_id: Some(candidate.record_id),
                context_record_ids: context_record_ids.clone(),
                ..ReverseEvidence::default()
            },
        }))
    }

    fn estimated_address(
        &self,
        options: ReverseGeocodeOptions,
        context: &mut ReverseContext,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<ReverseGeocodeResult>> {
        for candidate in self.spatial.segment_candidates(
            options.lon,
            options.lat,
            SpatialLayer::Interpolation,
            INTERPOLATION_RADIUS_M,
            CANDIDATE_LIMIT,
        ) {
            let Some(interpolation) = self.pack.interpolation(candidate.record_id)? else {
                continue;
            };
            let number = estimated_number(&interpolation.interpolation, candidate.fraction);
            self.enrich_record_context(candidate.record_id, context, context_record_ids)?;
            apply_interpolation_context(context, &interpolation.address);
            self.enrich_context(options, context, context_record_ids)?;
            let primary = estimated_primary_label(number, &interpolation.address)
                .unwrap_or_else(|| format!("{} {}", number, interpolation.name));

            return Ok(Some(ReverseGeocodeResult {
                match_kind: ReverseMatchKind::EstimatedAddress,
                label: compose_label(&primary, context),
                primary_record_id: Some(candidate.record_id),
                id: Some(interpolation.id),
                layer: Some("interpolation".to_string()),
                distance_m: Some(candidate.distance_m),
                point: Some(ReversePoint {
                    lon: candidate.closest_lon,
                    lat: candidate.closest_lat,
                    precision: ReversePointPrecision::Estimated,
                }),
                context: context.clone(),
                evidence: ReverseEvidence {
                    interpolation_record_id: Some(candidate.record_id),
                    context_record_ids: context_record_ids.clone(),
                    ..ReverseEvidence::default()
                },
            }));
        }

        Ok(None)
    }

    fn nearest_street(
        &self,
        options: ReverseGeocodeOptions,
        context: &mut ReverseContext,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<ReverseGeocodeResult>> {
        let Some(candidate) = self
            .spatial
            .segment_candidates(
                options.lon,
                options.lat,
                SpatialLayer::Street,
                STREET_RADIUS_M,
                1,
            )
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

        Ok(Some(ReverseGeocodeResult {
            match_kind: ReverseMatchKind::NearestStreet,
            label: compose_label(&street.label, context),
            primary_record_id: Some(candidate.record_id),
            id: Some(street.id),
            layer: Some("street".to_string()),
            distance_m: Some(candidate.distance_m),
            point: Some(ReversePoint {
                lon: candidate.closest_lon,
                lat: candidate.closest_lat,
                precision: ReversePointPrecision::Street,
            }),
            context: context.clone(),
            evidence: ReverseEvidence {
                street_record_id: Some(candidate.record_id),
                context_record_ids: context_record_ids.clone(),
                ..ReverseEvidence::default()
            },
        }))
    }

    fn context_only(
        &self,
        options: ReverseGeocodeOptions,
        context: &mut ReverseContext,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<ReverseGeocodeResult>> {
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

        Ok(Some(ReverseGeocodeResult {
            match_kind: ReverseMatchKind::ContextOnly,
            label,
            primary_record_id: Some(primary_record_id),
            id: Some(record.id),
            layer: Some(record.layer),
            distance_m: None,
            point: record.point.map(context_point),
            context: context.clone(),
            evidence: ReverseEvidence {
                context_record_ids: context_record_ids.clone(),
                ..ReverseEvidence::default()
            },
        }))
    }

    fn enrich_context(
        &self,
        options: ReverseGeocodeOptions,
        context: &mut ReverseContext,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<()> {
        let mut seen = context_record_ids.iter().copied().collect::<BTreeSet<_>>();
        for candidate in self.spatial.context_candidates(
            options.lon,
            options.lat,
            CONTEXT_RADIUS_M,
            CANDIDATE_LIMIT,
        ) {
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
        context: &mut ReverseContext,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<()> {
        let Some(boundary_context) = self.pack.boundary_context(record_id)? else {
            return Ok(());
        };
        let mut seen = context_record_ids.iter().copied().collect::<BTreeSet<_>>();
        let admin_ids = boundary_context
            .admin_context
            .into_iter()
            .flat_map(|tuple| tuple.parent_record_ids());
        let postcode_ids = boundary_context.postcode_record_id.into_iter();
        for context_record_id in admin_ids.chain(postcode_ids) {
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

fn apply_address_context(context: &mut ReverseContext, address: &AddressComponents) {
    set_option_if_missing(&mut context.street, address.street.clone());
    set_option_if_missing(&mut context.place, address.place.clone());
    set_option_if_missing(&mut context.postcode, address.postcode.clone());
    set_option_if_missing(&mut context.locality, address.locality.clone());
    set_option_if_missing(&mut context.region, address.region.clone());
    set_option_if_missing(&mut context.country, address.country.clone());
}

fn apply_interpolation_context(
    context: &mut ReverseContext,
    address: &InterpolationAddressComponents,
) {
    set_option_if_missing(&mut context.street, address.street.clone());
    set_option_if_missing(&mut context.place, address.place.clone());
    set_option_if_missing(&mut context.postcode, address.postcode.clone());
    set_option_if_missing(&mut context.locality, address.locality.clone());
    set_option_if_missing(&mut context.region, address.region.clone());
    set_option_if_missing(&mut context.country, address.country.clone());
}

fn apply_context_record(context: &mut ReverseContext, record: &ContextRecord) -> bool {
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

fn estimated_number(range: &InterpolationRange, fraction: f64) -> u32 {
    let fraction = fraction.clamp(0.0, 1.0);
    let span = range.end.saturating_sub(range.start);
    if span == 0 || range.step == 0 {
        return range.start;
    }
    let raw = range.start as f64 + fraction * span as f64;
    let step_index = ((raw - range.start as f64) / range.step as f64).round() as u32;
    (range.start + step_index * range.step).min(range.end)
}

fn estimated_primary_label(
    number: u32,
    address: &InterpolationAddressComponents,
) -> Option<String> {
    address
        .street
        .as_deref()
        .or(address.place.as_deref())
        .map(|street_or_place| format!("{number} {street_or_place}"))
}

fn compose_label(primary: &str, context: &ReverseContext) -> String {
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

fn context_only_label(context: &ReverseContext) -> Option<String> {
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

fn context_point(point: crate::pack::RecordPoint) -> ReversePoint {
    ReversePoint {
        lon: point.lon,
        lat: point.lat,
        precision: ReversePointPrecision::Context,
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
        builder::report::BuilderReport,
        context::AdminContextTuple,
        pack::{PackWriter, RecordWriter},
        record::{
            AddressRecord, LocationPrecision, OsmObjectType, PlaceLayer, PlaceRecord,
            SourceProvenance, StreetRecord, point_geometry,
        },
    };

    use super::*;

    #[test]
    fn reverse_prefers_explicit_address() {
        let temp_dir = temp_pack_dir("explicit");
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer.write_street(&street_record()).expect("street");
        writer.write_address(&address_record()).expect("address");
        let mut report = BuilderReport::default();
        writer.finish(&mut report).expect("finish");

        let geocoder = PackReverseGeocoder::open(&temp_dir).expect("geocoder");
        let response = geocoder
            .reverse(ReverseGeocodeOptions {
                lon: -79.0,
                lat: 43.0,
            })
            .expect("reverse");
        let result = response.result.expect("result");

        assert_eq!(result.match_kind, ReverseMatchKind::ExplicitAddress);
        assert_eq!(result.primary_record_id, Some(1));
        assert_eq!(result.evidence.explicit_address_record_id, Some(1));

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn reverse_estimates_address_from_interpolation() {
        let temp_dir = temp_pack_dir("interpolation");
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer
            .write_interpolation(&interpolation_record())
            .expect("interpolation");
        let mut report = BuilderReport::default();
        writer.finish(&mut report).expect("finish");

        let geocoder = PackReverseGeocoder::open(&temp_dir).expect("geocoder");
        let response = geocoder
            .reverse(ReverseGeocodeOptions {
                lon: -79.00001,
                lat: 43.0005,
            })
            .expect("reverse");
        let result = response.result.expect("result");

        assert_eq!(result.match_kind, ReverseMatchKind::EstimatedAddress);
        assert!(result.label.starts_with("50 Queen Street"));
        assert_eq!(result.evidence.interpolation_record_id, Some(0));

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn reverse_falls_back_to_street() {
        let temp_dir = temp_pack_dir("street");
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        writer.write_street(&street_record()).expect("street");
        let mut report = BuilderReport::default();
        writer.finish(&mut report).expect("finish");

        let geocoder = PackReverseGeocoder::open(&temp_dir).expect("geocoder");
        let response = geocoder
            .reverse(ReverseGeocodeOptions {
                lon: -79.00001,
                lat: 43.0005,
            })
            .expect("reverse");
        let result = response.result.expect("result");

        assert_eq!(result.match_kind, ReverseMatchKind::NearestStreet);
        assert!(result.label.starts_with("Queen Street"));
        assert_eq!(result.evidence.street_record_id, Some(0));

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn reverse_prefers_materialized_boundary_context_over_source_locality() {
        let temp_dir = temp_pack_dir("boundary-context");
        let mut writer = PackWriter::create(&temp_dir).expect("writer");
        let locality_id = writer
            .write_place(&place_record(), PlaceLayer::Locality)
            .expect("place");
        let mut address = address_record();
        address.address.locality = Some("North York".to_string());
        let address_id = writer.write_address(&address).expect("address");
        writer.write_boundary_context(
            address_id,
            AdminContextTuple {
                locality_record_id: Some(locality_id),
                ..AdminContextTuple::default()
            },
            None,
            0,
        );
        let mut report = BuilderReport::default();
        writer.finish(&mut report).expect("finish");

        let geocoder = PackReverseGeocoder::open(&temp_dir).expect("geocoder");
        let response = geocoder
            .reverse(ReverseGeocodeOptions {
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

    fn address_record() -> AddressRecord {
        AddressRecord {
            id: "osm:node:10".to_string(),
            label: "10 Queen Street, Toronto".to_string(),
            name: "10 Queen Street".to_string(),
            address: AddressComponents {
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
            id: "osm:relation:99".to_string(),
            label: "Toronto".to_string(),
            name: "Toronto".to_string(),
            place_type: "admin_level:8".to_string(),
            geometry: point_geometry(-79.0, 43.0),
            source: SourceProvenance::osm(OsmObjectType::Relation, 99),
        }
    }

    fn interpolation_record() -> crate::record::InterpolationRecord {
        crate::record::InterpolationRecord {
            id: "osm:way:20:interp:2-98".to_string(),
            label: "Queen Street 2-98 even".to_string(),
            name: "Queen Street".to_string(),
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
            anchor_ids: vec!["osm:node:2".to_string(), "osm:node:98".to_string()],
            geometry: line_geometry(),
            representative_point: [-79.0, 43.0005],
            source: SourceProvenance::osm(OsmObjectType::Way, 20),
        }
    }

    fn street_record() -> StreetRecord {
        StreetRecord {
            id: "osm:way:30".to_string(),
            label: "Queen Street".to_string(),
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
