use std::{collections::BTreeSet, path::Path, sync::Arc};

use crate::error::Result;
use serde::Serialize;

use crate::{
    areas,
    context::AdminContextTuple,
    labels,
    pack::{self, ContextRecord, RecordId},
    record::{
        AddressComponents, AddressRecord, InterpolationAddressComponents, Layer, PlaceLayer, Record,
    },
    spatial_index,
};

const ADDRESS_RADIUS_M: f64 = 30.0;
const INTERPOLATION_RADIUS_M: f64 = 30.0;
const STREET_RADIUS_M: f64 = 30.0;
const CONTEXT_RADIUS_M: f64 = 50_000.0;
const CANDIDATE_LIMIT: usize = 16;
/// The tiers [`Context`] has a slot for, finest first. A `Sub` tier shares
/// its record layer with the tier above but is not that tier: the
/// sub-locality Ville-Haute is no locality of its own.
const TIERS: [PlaceLayer; 6] = [
    PlaceLayer::Neighbourhood,
    PlaceLayer::Locality,
    PlaceLayer::District,
    PlaceLayer::Region,
    PlaceLayer::Country,
    PlaceLayer::Place,
];

#[derive(Debug)]
pub struct PackReverseGeocoder {
    pack: Arc<pack::Reader>,
    spatial: spatial_index::Reader,
    /// `None` for a Pack built before it kept its areas.
    areas: Option<areas::Reader>,
}

#[derive(Debug, Clone, Copy)]
pub struct GeocodeOptions {
    pub lon: f64,
    pub lat: f64,
    /// Answer only with a record of this layer, and list no other area.
    pub layer: Option<Layer>,
}

impl GeocodeOptions {
    pub const fn at(lon: f64, lat: f64) -> Self {
        Self {
            lon,
            lat,
            layer: None,
        }
    }
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
    Poi,
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
        let areas = areas::Reader::open(pack.container())?;
        Ok(Self {
            pack,
            spatial,
            areas,
        })
    }

    pub fn reverse(&self, options: GeocodeOptions) -> Result<GeocodeResponse> {
        let mut context = Context::default();
        let mut context_record_ids = Vec::new();

        if let Some(layer) = options.layer {
            let mut result =
                self.of_layer(layer, options, &mut context, &mut context_record_ids)?;
            // Resolved areas still label it, but are no answers of their own.
            if let Some(result) = &mut result {
                result.evidence.context_record_ids.clear();
            }
            return Ok(GeocodeResponse {
                lon: options.lon,
                lat: options.lat,
                result,
            });
        }

        let mut result = self.explicit_address(options, &mut context, &mut context_record_ids)?;
        if result.is_none() {
            crate::stop::check()?;
            result = self.estimated_address(options, &mut context, &mut context_record_ids)?;
        }
        if result.is_none() {
            crate::stop::check()?;
            result = self.nearest_street(options, &mut context, &mut context_record_ids)?;
        }
        if result.is_none() {
            crate::stop::check()?;
            result = self.context_only(options, &mut context, &mut context_record_ids)?;
        }

        Ok(GeocodeResponse {
            lon: options.lon,
            lat: options.lat,
            result,
        })
    }

    fn of_layer(
        &self,
        layer: Layer,
        options: GeocodeOptions,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<GeocodeResult>> {
        match layer {
            Layer::Address => self.explicit_address(options, context, context_record_ids),
            Layer::Interpolation => self.estimated_address(options, context, context_record_ids),
            Layer::Street => self.nearest_street(options, context, context_record_ids),
            Layer::Poi => self.nearest_poi(options, context, context_record_ids),
            Layer::Country
            | Layer::District
            | Layer::Locality
            | Layer::Neighbourhood
            | Layer::Place
            | Layer::Postcode
            | Layer::Region => self.area(layer, options, context, context_record_ids),
        }
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
        let record = self.pack.records().record(candidate.record_id)?;
        let Record::Address(AddressRecord { address, .. }) = &record else {
            return Ok(None);
        };

        let chain =
            self.name_record_areas(candidate.record_id, options, context, context_record_ids)?;
        apply_address_context(context, address);
        self.enrich_context(options, context, context_record_ids, &chain)?;

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
            crate::stop::check()?;
            let Some(interpolation) = self.pack.interpolation(candidate.record_id)? else {
                continue;
            };
            let number = interpolation.interpolation.number_at(candidate.fraction);
            let chain =
                self.name_record_areas(candidate.record_id, options, context, context_record_ids)?;
            apply_interpolation_context(context, &interpolation.address);
            self.enrich_context(options, context, context_record_ids, &chain)?;
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
        let chain =
            self.name_record_areas(candidate.record_id, options, context, context_record_ids)?;
        self.enrich_context(options, context, context_record_ids, &chain)?;

        Ok(Some(GeocodeResult {
            match_kind: MatchKind::NearestStreet,
            label: compose_label(&street.name, context),
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

    fn nearest_poi(
        &self,
        options: GeocodeOptions,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<GeocodeResult>> {
        let Some(candidate) = self
            .spatial
            .poi_candidates(options.lon, options.lat, ADDRESS_RADIUS_M, 1)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let record = self.pack.records().record(candidate.record_id)?;
        let Record::Poi(poi) = &record else {
            return Ok(None);
        };

        let chain =
            self.name_record_areas(candidate.record_id, options, context, context_record_ids)?;
        if let Some(address) = &poi.address {
            apply_address_context(context, address);
        }
        self.enrich_context(options, context, context_record_ids, &chain)?;

        Ok(Some(GeocodeResult {
            match_kind: MatchKind::Poi,
            label: compose_label(&poi.name, context),
            primary_record_id: Some(candidate.record_id),
            id: Some(record.id()),
            layer: Some(Layer::Poi.as_str().to_string()),
            distance_m: Some(candidate.distance_m),
            point: Some(Point {
                lon: candidate.lon,
                lat: candidate.lat,
                precision: PointPrecision::Point,
            }),
            context: context.clone(),
            evidence: Evidence {
                context_record_ids: context_record_ids.clone(),
                ..Evidence::default()
            },
        }))
    }

    fn area(
        &self,
        layer: Layer,
        options: GeocodeOptions,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<GeocodeResult>> {
        let chain = self.held(options)?;
        self.name_areas(&chain, context, context_record_ids)?;
        self.enrich_context(options, context, context_record_ids, &chain)?;
        for &record_id in context_record_ids.iter() {
            let Some(record) = self.pack.context_record(record_id)? else {
                continue;
            };
            if record.layer != layer.as_str() {
                continue;
            }
            // A town is labelled with the areas holding it, not with the
            // neighbourhood the point is in.
            let label = match tier_of(layer) {
                Some(tier) => compose_label(&record.name, &coarser(context, tier)),
                None => compose_label(&record.name, context),
            };
            return Ok(Some(GeocodeResult {
                match_kind: MatchKind::ContextOnly,
                label,
                primary_record_id: Some(record_id),
                id: Some(record.id),
                layer: Some(record.layer),
                distance_m: None,
                point: record.point.map(context_point),
                context: context.clone(),
                evidence: Evidence {
                    context_record_ids: context_record_ids.clone(),
                    ..Evidence::default()
                },
            }));
        }
        Ok(None)
    }

    fn context_only(
        &self,
        options: GeocodeOptions,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<GeocodeResult>> {
        let chain = self.held(options)?;
        self.name_areas(&chain, context, context_record_ids)?;
        self.enrich_context(options, context, context_record_ids, &chain)?;
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

    /// Fill the slots `chain` left empty from the nearest area points within
    /// reach, each the nearest of its layer.
    ///
    /// Inside some area, a point names an area drawn only as a point, and
    /// only one lying in areas that all hold the point too. The nearest
    /// point of a layer is otherwise often of an area across a border, or of
    /// a town whose shape the point is outside of. A village drawn only as a
    /// point still names the point beside it. A postcode has no shape, so
    /// its nearest point stands.
    fn enrich_context(
        &self,
        options: GeocodeOptions,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
        chain: &AdminContextTuple,
    ) -> Result<()> {
        let mut seen = context_record_ids.iter().copied().collect::<BTreeSet<_>>();
        for candidate in self.spatial.context_candidates(
            options.lon,
            options.lat,
            CONTEXT_RADIUS_M,
            CANDIDATE_LIMIT,
        )? {
            crate::stop::check()?;
            if !seen.insert(candidate.record_id) {
                continue;
            }
            if !chain.is_empty()
                && candidate.layer != Layer::Postcode
                && !self.lies_within(candidate.record_id, chain)?
            {
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

    /// The chain of the areas holding the point; empty when the Pack keeps
    /// no areas.
    fn held(&self, options: GeocodeOptions) -> Result<AdminContextTuple> {
        match &self.areas {
            Some(areas) => areas.chain_at(options.lon, options.lat),
            None => Ok(AdminContextTuple::default()),
        }
    }

    /// Whether the record lies in at least one area, and only in areas of
    /// `chain`. An area keeps no chain of its own, so one whose shape misses
    /// the point never passes, even when its centre lies outside it.
    fn lies_within(&self, record_id: RecordId, chain: &AdminContextTuple) -> Result<bool> {
        let Some(own) = self.pack.boundary_context(record_id)? else {
            return Ok(false);
        };
        let entries = own.admin_context.entries();
        Ok(!entries.is_empty()
            && entries.iter().all(|entry| {
                chain
                    .entries()
                    .iter()
                    .any(|held| held.record_id == entry.record_id)
            }))
    }

    /// Name the areas of the record answering at the point: those the build
    /// found it in, else those holding the point. Returns the chain named.
    fn name_record_areas(
        &self,
        record_id: RecordId,
        options: GeocodeOptions,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<AdminContextTuple> {
        let chain = match self.pack.boundary_context(record_id)? {
            Some(own) if !own.admin_context.is_empty() => own.admin_context,
            _ => self.held(options)?,
        };
        self.name_areas(&chain, context, context_record_ids)?;
        Ok(chain)
    }

    /// Put each area of `chain` in the slot of its tier, finest first, and
    /// list it once.
    fn name_areas(
        &self,
        chain: &AdminContextTuple,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<()> {
        let mut seen = context_record_ids.iter().copied().collect::<BTreeSet<_>>();
        for tier in TIERS {
            let Some(record_id) = chain.get(tier) else {
                continue;
            };
            let Some(record) = self.pack.context_record(record_id)? else {
                continue;
            };
            if let Some(slot) = slot(context, tier)
                && set_if_missing(slot, record.name)
                && seen.insert(record_id)
            {
                context_record_ids.push(record_id);
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

fn tier_of(layer: Layer) -> Option<PlaceLayer> {
    match layer {
        Layer::Neighbourhood => Some(PlaceLayer::Neighbourhood),
        Layer::Locality => Some(PlaceLayer::Locality),
        Layer::District => Some(PlaceLayer::District),
        Layer::Region => Some(PlaceLayer::Region),
        Layer::Country => Some(PlaceLayer::Country),
        Layer::Place => Some(PlaceLayer::Place),
        Layer::Address | Layer::Interpolation | Layer::Street | Layer::Poi | Layer::Postcode => {
            None
        }
    }
}

/// The slot of a tier of [`TIERS`]; `None` for a `Sub` tier.
fn slot(context: &mut Context, tier: PlaceLayer) -> Option<&mut Option<String>> {
    match tier {
        PlaceLayer::Neighbourhood => Some(&mut context.neighbourhood),
        PlaceLayer::Locality => Some(&mut context.locality),
        PlaceLayer::District => Some(&mut context.district),
        PlaceLayer::Region => Some(&mut context.region),
        PlaceLayer::Country => Some(&mut context.country),
        PlaceLayer::Place => Some(&mut context.place),
        PlaceLayer::SubRegion
        | PlaceLayer::SubDistrict
        | PlaceLayer::SubLocality
        | PlaceLayer::SubNeighbourhood => None,
    }
}

/// The admin areas of `context` that hold an area of `tier`, to label it:
/// a locality is named with its district and country, not with the
/// neighbourhood the point is in.
fn coarser(context: &Context, tier: PlaceLayer) -> Context {
    let mut held = Context {
        street: None,
        postcode: None,
        ..context.clone()
    };
    for finer in TIERS {
        if (finer >= tier || finer == PlaceLayer::Place)
            && let Some(slot) = slot(&mut held, finer)
        {
            *slot = None;
        }
    }
    held
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
            InterpolationRange, LocationPrecision, ObjectType, PlaceLayer, PlaceRecord, PoiRecord,
            SourceProvenance, StreetRecord, point_geometry,
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
            .reverse(GeocodeOptions::at(-79.0, 43.0))
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
            .reverse(GeocodeOptions::at(-79.00001, 43.0005))
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
            .reverse(GeocodeOptions::at(-79.00001, 43.0005))
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
                    admin_context: AdminContextTuple::from_tiers([(
                        PlaceLayer::Locality,
                        locality_id,
                    )]),
                    flags: 0,
                }),
            )
            .expect("address");
        writer.finish().expect("finish");

        let geocoder = PackReverseGeocoder::open(&temp_dir).expect("geocoder");
        let response = geocoder
            .reverse(GeocodeOptions::at(-79.0, 43.0))
            .expect("reverse");
        let result = response.result.expect("result");

        assert_eq!(result.context.locality.as_deref(), Some("Toronto"));
        assert_eq!(result.evidence.context_record_ids, vec![locality_id]);

        let _ = fs::remove_dir_all(temp_dir);
    }

    /// A sub-locality's record is of the locality layer, yet it is no
    /// locality: Ville-Haute is a quarter of the commune of Luxembourg. The
    /// locality an address names is the commune.
    #[test]
    fn a_sub_locality_takes_no_locality_slot() {
        let temp_dir = temp_pack_dir("sub-locality");
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let place = |name: &str, id| PlaceRecord {
            name: name.to_string(),
            source: SourceProvenance::osm(ObjectType::Relation, id),
            ..place_record()
        };
        let commune = writer
            .write(
                &Record::Place(PlaceLayer::Locality, place("Luxembourg", 407_489)),
                None,
            )
            .expect("commune");
        let quarter = writer
            .write(
                &Record::Place(PlaceLayer::SubLocality, place("Ville-Haute", 533_345)),
                None,
            )
            .expect("quarter");
        let mut address = address_record();
        address.address.locality = None;
        writer
            .write(
                &address.into(),
                Some(RecordContext {
                    admin_context: AdminContextTuple::from_tiers([
                        (PlaceLayer::Locality, commune),
                        (PlaceLayer::SubLocality, quarter),
                    ]),
                    flags: 0,
                }),
            )
            .expect("address");
        writer.finish().expect("finish");

        let geocoder = PackReverseGeocoder::open(&temp_dir).expect("geocoder");
        let result = geocoder
            .reverse(GeocodeOptions::at(-79.0, 43.0))
            .expect("reverse")
            .result
            .expect("result");

        assert_eq!(result.context.locality.as_deref(), Some("Luxembourg"));
        assert_eq!(result.evidence.context_record_ids, vec![commune]);

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
    fn reverse_answers_with_the_address_beside_a_poi_not_the_poi() {
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
                    source: SourceProvenance::osm(ObjectType::Node, 40),
                    prominence: Default::default(),
                    wikidata: None,
                    brand: None,
                    aliases: Vec::new(),
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
                    source: SourceProvenance::osm(ObjectType::Way, 41),
                    prominence: Default::default(),
                    wikidata: None,
                    brand: None,
                    aliases: Vec::new(),
                }
                .into(),
                None,
            )
            .expect("bakery");
        let mut address = address_record();
        address.geometry = point_geometry(-79.0, 43.00005);
        address.location_precision = LocationPrecision::Centroid;
        address.source = SourceProvenance::osm(ObjectType::Way, 41);
        writer.write(&address.into(), None).expect("address");
        writer.finish().expect("finish");

        let geocoder = PackReverseGeocoder::open(&temp_dir).expect("geocoder");
        let result = geocoder
            .reverse(GeocodeOptions::at(-79.0, 43.0))
            .expect("reverse")
            .result
            .expect("result");

        assert_eq!(result.match_kind, MatchKind::ExplicitAddress);
        assert_eq!(result.id.as_deref(), Some("osm:way:41"));
        assert_eq!(result.layer.as_deref(), Some("address"));
        assert_eq!(result.label, "10 Queen Street, Toronto, Ontario, Canada");
        assert_eq!(result.evidence.explicit_address_record_id, Some(3));

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn a_layer_answers_with_its_nearest_record_and_lists_no_other_area() {
        let temp_dir = temp_pack_dir("layer");
        let mut writer = Writer::create(&temp_dir).expect("writer");
        let locality_id = writer
            .write(&Record::Place(PlaceLayer::Locality, place_record()), None)
            .expect("place");
        writer.write(&street_record().into(), None).expect("street");
        writer
            .write(
                &PoiRecord {
                    alt_names: Vec::new(),
                    name: "Queen Bakery".to_string(),
                    category: "shop:bakery".to_string(),
                    address: Some(address_record().address),
                    geometry: point_geometry(-79.0, 43.0),
                    location_precision: LocationPrecision::Point,
                    source: SourceProvenance::osm(ObjectType::Node, 41),
                    prominence: Default::default(),
                    wikidata: None,
                    brand: None,
                    aliases: Vec::new(),
                }
                .into(),
                None,
            )
            .expect("bakery");
        let mut address = address_record();
        address.geometry = point_geometry(-79.0, 43.00005);
        writer.write(&address.into(), None).expect("address");
        writer.finish().expect("finish");

        let geocoder = PackReverseGeocoder::open(&temp_dir).expect("geocoder");
        let at = |layer| {
            geocoder
                .reverse(GeocodeOptions {
                    layer: Some(layer),
                    ..GeocodeOptions::at(-79.0, 43.0)
                })
                .expect("reverse")
                .result
        };

        let street = at(Layer::Street).expect("street");
        assert_eq!(street.match_kind, MatchKind::NearestStreet);
        assert_eq!(street.id.as_deref(), Some("osm:way:30"));
        assert!(street.evidence.context_record_ids.is_empty());

        let address = at(Layer::Address).expect("address");
        assert_eq!(address.id.as_deref(), Some("osm:node:10"));
        assert_eq!(address.layer.as_deref(), Some("address"));

        let poi = at(Layer::Poi).expect("poi");
        assert_eq!(poi.match_kind, MatchKind::Poi);
        assert_eq!(poi.id.as_deref(), Some("osm:node:41"));
        assert!(poi.label.starts_with("Queen Bakery, "), "{}", poi.label);

        let locality = at(Layer::Locality).expect("locality");
        assert_eq!(locality.primary_record_id, Some(locality_id));
        assert_eq!(locality.layer.as_deref(), Some("locality"));

        assert!(at(Layer::Interpolation).is_none());
        assert!(at(Layer::Country).is_none());

        let _ = fs::remove_dir_all(temp_dir);
    }

    fn address_record() -> AddressRecord {
        AddressRecord {
            disputed: false,
            street_postcodes: Vec::new(),
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
            source: SourceProvenance::osm(ObjectType::Node, 10),
        }
    }

    fn place_record() -> PlaceRecord {
        PlaceRecord {
            alt_names: Vec::new(),
            name: "Toronto".to_string(),
            place_type: "admin_level:8".to_string(),
            geometry: point_geometry(-79.0, 43.0),
            source: SourceProvenance::osm(ObjectType::Relation, 99),
            prominence: Default::default(),
            wikidata: None,
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
            source: SourceProvenance::osm(ObjectType::Way, 20),
        }
    }

    fn street_record() -> StreetRecord {
        StreetRecord {
            other_ways: Vec::new(),
            road_class: crate::record::RoadClass::Residential,
            alt_names: Vec::new(),
            locality: None,
            name: "Queen Street".to_string(),
            geometry: line_geometry(),
            representative_point: [-79.0, 43.0005],
            source: SourceProvenance::osm(ObjectType::Way, 30),
        }
    }

    fn line_geometry() -> Geometry {
        Geometry::new(GeometryValue::LineString {
            coordinates: vec![vec![-79.0, 43.0].into(), vec![-79.0, 43.001].into()],
        })
    }
}
