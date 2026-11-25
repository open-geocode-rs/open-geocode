use std::{collections::BTreeSet, path::Path, sync::Arc};

use crate::error::Result;
use serde::Serialize;

use crate::{
    labels,
    pack::{self, ContextRecord, RecordId},
    record::{AddressComponents, AddressRecord, InterpolationAddressComponents, Layer, Record},
    spatial_index,
};

/// How far reverse looks for the nearest house, street or place when the
/// caller names no distance. Past it, a house says less about where the
/// point is than the areas holding it.
pub const DEFAULT_RADIUS_M: f64 = 30.0;
/// The farthest a caller may ask reverse to look. Candidates are read cell
/// by cell, so the cost grows with the square of the radius.
pub const MAX_RADIUS_M: f64 = 1_000.0;
const CONTEXT_RADIUS_M: f64 = 50_000.0;
const CANDIDATE_LIMIT: usize = 16;
const POSTCODE_CANDIDATE_LIMIT: usize = 6;

#[derive(Debug)]
pub struct PackReverseGeocoder {
    pack: Arc<pack::Reader>,
    spatial: spatial_index::Reader,
}

#[derive(Debug, Clone, Copy)]
pub struct GeocodeOptions {
    pub lon: f64,
    pub lat: f64,
    /// Answer only with a record of this layer, and list no other area.
    pub layer: Option<Layer>,
    /// How far to look for the nearest house, street or place, in metres.
    /// Areas hold the point, so it does not bound them.
    pub radius_m: f64,
}

impl GeocodeOptions {
    pub const fn at(lon: f64, lat: f64) -> Self {
        Self {
            lon,
            lat,
            layer: None,
            radius_m: DEFAULT_RADIUS_M,
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
        Ok(Self { pack, spatial })
    }

    /// The `limit` POIs nearest `[lon, lat]` within `radius_m` whose category
    /// `accept` keeps, at a point `within` keeps, nearest first; `None` past
    /// `budget` POIs tested.
    pub fn nearest_pois(
        &self,
        [lon, lat]: [f64; 2],
        radius_m: f64,
        limit: usize,
        budget: u64,
        within: impl Fn(f64, f64) -> bool,
        accept: impl Fn(&str) -> bool,
    ) -> Result<Option<Vec<RecordId>>> {
        let records = self.pack.records();
        Ok(self
            .spatial
            .poi_candidates_where(lon, lat, radius_m, limit, budget, |record_id, lon, lat| {
                Ok(within(lon, lat) && records.poi_category_is(record_id, &accept)?)
            })?
            .map(|candidates| {
                candidates
                    .into_iter()
                    .map(|candidate| candidate.record_id)
                    .collect()
            }))
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

    /// The `limit` results of `layer` nearest the point, nearest first, each
    /// labelled by its own areas. A point lies in one area of each layer, so
    /// an area layer answers at most one.
    pub fn reverse_nearest(
        &self,
        options: GeocodeOptions,
        layer: Layer,
        limit: usize,
    ) -> Result<Vec<GeocodeResponse>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (lon, lat) = (options.lon, options.lat);
        // Each result is labelled by its own areas, in a context of its own.
        let fresh = || (Context::default(), Vec::<RecordId>::new());
        let found = match layer {
            Layer::Address => self
                .spatial
                .address_candidates(lon, lat, options.radius_m, limit)?
                .iter()
                .map(|candidate| {
                    let (mut context, mut ids) = fresh();
                    self.address_at(options, candidate, &mut context, &mut ids)
                })
                .collect::<Result<Vec<_>>>()?,
            Layer::Poi => self
                .spatial
                .poi_candidates(lon, lat, options.radius_m, limit)?
                .iter()
                .map(|candidate| {
                    let (mut context, mut ids) = fresh();
                    self.poi_at(options, candidate, &mut context, &mut ids)
                })
                .collect::<Result<Vec<_>>>()?,
            Layer::Street => self
                .spatial
                .segment_candidates(lon, lat, Layer::Street, options.radius_m, limit)?
                .iter()
                .map(|candidate| {
                    let (mut context, mut ids) = fresh();
                    self.street_at(options, candidate, &mut context, &mut ids)
                })
                .collect::<Result<Vec<_>>>()?,
            Layer::Interpolation => {
                let mut found = self
                    .spatial
                    .segment_candidates(lon, lat, Layer::Interpolation, options.radius_m, limit)?
                    .iter()
                    .map(|candidate| {
                        let (mut context, mut ids) = fresh();
                        self.estimated_at(options, candidate, &mut context, &mut ids)
                    })
                    .collect::<Result<Vec<_>>>()?;
                if found.iter().all(Option::is_none) {
                    let (mut context, mut ids) = fresh();
                    found.push(self.tiger_estimate(options, &mut context, &mut ids)?);
                }
                found
            }
            Layer::Country
            | Layer::District
            | Layer::Locality
            | Layer::Neighbourhood
            | Layer::Place
            | Layer::Postcode
            | Layer::Region => {
                let options = GeocodeOptions {
                    layer: Some(layer),
                    ..options
                };
                return Ok(self
                    .reverse(options)?
                    .result
                    .map_or_else(Vec::new, |result| {
                        vec![GeocodeResponse {
                            lon,
                            lat,
                            result: Some(result),
                        }]
                    }));
            }
        };
        Ok(found
            .into_iter()
            .flatten()
            .map(|mut result| {
                // Its areas label it, and are no answers of their own.
                result.evidence.context_record_ids.clear();
                GeocodeResponse {
                    lon,
                    lat,
                    result: Some(result),
                }
            })
            .collect())
    }

    pub(crate) fn reverse_addresses_where(
        &self,
        options: GeocodeOptions,
        limit: usize,
        accept: impl FnMut(RecordId) -> Result<bool>,
    ) -> Result<Vec<GeocodeResponse>> {
        let (lon, lat) = (options.lon, options.lat);
        self.spatial
            .address_candidates_where(lon, lat, options.radius_m, limit, accept)?
            .iter()
            .filter_map(|point| {
                self.address_at(options, point, &mut Context::default(), &mut Vec::new())
                    .transpose()
            })
            .map(|found| {
                found.map(|mut result| {
                    result.evidence.context_record_ids.clear();
                    GeocodeResponse {
                        lon,
                        lat,
                        result: Some(result),
                    }
                })
            })
            .collect()
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
        let candidates = self.spatial.address_candidates(
            options.lon,
            options.lat,
            options.radius_m,
            POSTCODE_CANDIDATE_LIMIT,
        )?;
        let Some(first) = candidates.first() else {
            return Ok(None);
        };
        let candidate = self
            .corroborated_zip(first, &candidates[1..])?
            .unwrap_or(first);
        self.address_at(options, candidate, context, context_record_ids)
    }

    fn corroborated_zip<'a>(
        &self,
        first: &spatial_index::PointCandidate,
        other: &'a [spatial_index::PointCandidate],
    ) -> Result<Option<&'a spatial_index::PointCandidate>> {
        if other.len() < 2 {
            return Ok(None);
        }
        let records = self.pack.records();
        let Record::Address(first_record) = records.record(first.record_id)? else {
            return Ok(None);
        };
        let address = &first_record.address;
        if address.country.as_deref().is_some_and(|country| {
            let code = if country.len() == 2 {
                Some(country)
            } else {
                crate::iso::alpha2_of(country).or_else(|| crate::iso::alpha2_of_name(country))
            };
            code.is_some_and(|code| !code.eq_ignore_ascii_case("US"))
        }) {
            return Ok(None);
        }
        let Some(zip) = address
            .postcode
            .as_deref()
            .filter(|zip| zip.len() == 5 && zip.bytes().all(|byte| byte.is_ascii_digit()))
        else {
            return Ok(None);
        };
        let Some(street) = address.street.as_deref() else {
            return Ok(None);
        };
        let mut plain = Vec::new();
        let mut extended = BTreeSet::new();
        for candidate in other {
            if candidate.distance_m > first.distance_m + DEFAULT_RADIUS_M {
                break;
            }
            let Record::Address(record) = records.record(candidate.record_id)? else {
                continue;
            };
            let same_house = record.address.number == address.number
                && record.address.unit == address.unit
                && record
                    .address
                    .street
                    .as_deref()
                    .is_some_and(|name| crate::search::road::same_road(name, street))
                && record.address.locality == address.locality
                && record.address.region == address.region;
            if !same_house {
                continue;
            }
            let Some(postcode) = record.address.postcode.as_deref() else {
                continue;
            };
            if postcode.len() == 5 && postcode.bytes().all(|byte| byte.is_ascii_digit()) {
                plain.push((postcode.to_string(), candidate));
            } else if let Some((base, addon)) = postcode.split_once('-')
                && base.len() == 5
                && addon.len() == 4
                && base
                    .bytes()
                    .chain(addon.bytes())
                    .all(|byte| byte.is_ascii_digit())
            {
                extended.insert(base.to_string());
            }
        }
        // A ZIP+4 can break a disagreement only when a separate point for
        // the same house gives its five-digit ZIP and no other ZIP+4 competes.
        let Some(preferred) = extended
            .first()
            .filter(|preferred| extended.len() == 1 && preferred.as_str() != zip)
        else {
            return Ok(None);
        };
        Ok(plain
            .into_iter()
            .find_map(|(postcode, candidate)| (postcode == *preferred).then_some(candidate)))
    }

    /// The address result of `candidate`, the address found near the point.
    fn address_at(
        &self,
        options: GeocodeOptions,
        candidate: &spatial_index::PointCandidate,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<GeocodeResult>> {
        let record = self.pack.records().record(candidate.record_id)?;
        let Record::Address(AddressRecord { address, .. }) = &record else {
            return Ok(None);
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
            options.radius_m,
            CANDIDATE_LIMIT,
        )? {
            crate::stop::check()?;
            if let Some(result) =
                self.estimated_at(options, &candidate, context, context_record_ids)?
            {
                return Ok(Some(result));
            }
        }
        self.tiger_estimate(options, context, context_record_ids)
    }

    fn tiger_estimate(
        &self,
        options: GeocodeOptions,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<GeocodeResult>> {
        if self.pack.tiger_ranges.is_empty() {
            return Ok(None);
        }
        let radius = options.radius_m.min(40.0);
        // Looking farther than the nearest street could put someone across
        // a junction on a different road to fetch a range.
        let Some(candidate) = self
            .spatial
            .segment_candidates(options.lon, options.lat, Layer::Street, radius, 1)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let Some(left) = self
            .spatial
            .left_of_line(&candidate, options.lon, options.lat)?
        else {
            return Ok(None);
        };
        let mut ranges = self
            .pack
            .tiger_ranges
            .for_street(candidate.record_id)
            .filter_map(|range| {
                let is_left = left == (range.start < range.end);
                if is_left != (range.side == b'L') {
                    return None;
                }
                Some((range, range.number_at(candidate.fraction)?))
            });
        let Some((range, number)) = ranges.next() else {
            return Ok(None);
        };
        if ranges.next().is_some() {
            return Ok(None);
        }
        let Some(street) = self.pack.street(candidate.record_id)? else {
            return Ok(None);
        };
        let Some([lon, lat]) = self.spatial.point_along_line(
            candidate.record_id,
            range.fraction(number).unwrap_or(candidate.fraction),
        )?
        else {
            return Ok(None);
        };
        let distance = crate::search::road::planar_m([options.lon, options.lat], [lon, lat]);
        if distance > radius {
            return Ok(None);
        }
        self.enrich_record_context(candidate.record_id, context, context_record_ids)?;
        context.street = Some(street.name.clone());
        context.postcode = range.postcode().or_else(|| {
            self.pack
                .tiger_side_zips
                .postcode(range.side_key(range.side))
        });
        self.enrich_context(options, context, context_record_ids)?;
        Ok(Some(GeocodeResult {
            match_kind: MatchKind::EstimatedAddress,
            label: compose_label(&format!("{number} {}", street.name), context),
            primary_record_id: Some(candidate.record_id),
            id: Some(range.id(number)),
            layer: Some(Layer::Interpolation.as_str().into()),
            distance_m: Some(distance),
            point: Some(Point {
                lon,
                lat,
                precision: PointPrecision::Estimated,
            }),
            context: context.clone(),
            evidence: Evidence {
                street_record_id: Some(candidate.record_id),
                context_record_ids: context_record_ids.clone(),
                ..Evidence::default()
            },
        }))
    }

    /// The estimated address of `candidate`, a range found near the point,
    /// at the number along it the point is nearest.
    fn estimated_at(
        &self,
        options: GeocodeOptions,
        candidate: &spatial_index::SegmentCandidate,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<GeocodeResult>> {
        let Some(interpolation) = self.pack.interpolation(candidate.record_id)? else {
            return Ok(None);
        };
        let number = interpolation.interpolation.number_at(candidate.fraction);
        self.enrich_record_context(candidate.record_id, context, context_record_ids)?;
        apply_interpolation_context(context, &interpolation.address);
        self.enrich_context(options, context, context_record_ids)?;
        let primary = labels::estimated_address_name(number, &interpolation.address)
            .unwrap_or_else(|| format!("{} {}", number, interpolation.name()));

        Ok(Some(GeocodeResult {
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
        }))
    }

    fn nearest_street(
        &self,
        options: GeocodeOptions,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<GeocodeResult>> {
        let Some(candidate) = self
            .spatial
            .segment_candidates(options.lon, options.lat, Layer::Street, options.radius_m, 1)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        self.street_at(options, &candidate, context, context_record_ids)
    }

    /// The street result of `candidate`, a street found near the point.
    fn street_at(
        &self,
        options: GeocodeOptions,
        candidate: &spatial_index::SegmentCandidate,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<GeocodeResult>> {
        let Some(street) = self.pack.street(candidate.record_id)? else {
            return Ok(None);
        };

        set_if_missing(&mut context.street, street.name.clone());
        self.enrich_record_context(candidate.record_id, context, context_record_ids)?;
        self.enrich_context(options, context, context_record_ids)?;

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
            .poi_candidates(options.lon, options.lat, options.radius_m, 1)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        self.poi_at(options, &candidate, context, context_record_ids)
    }

    /// The POI result of `candidate`, a POI found near the point.
    fn poi_at(
        &self,
        options: GeocodeOptions,
        candidate: &spatial_index::PointCandidate,
        context: &mut Context,
        context_record_ids: &mut Vec<RecordId>,
    ) -> Result<Option<GeocodeResult>> {
        let record = self.pack.records().record(candidate.record_id)?;
        let Record::Poi(poi) = &record else {
            return Ok(None);
        };

        self.enrich_record_context(candidate.record_id, context, context_record_ids)?;
        if let Some(address) = &poi.address {
            apply_address_context(context, address);
        }
        self.enrich_context(options, context, context_record_ids)?;

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
        self.enrich_context(options, context, context_record_ids)?;
        for &record_id in context_record_ids.iter() {
            let Some(record) = self.pack.context_record(record_id)? else {
                continue;
            };
            if record.layer != layer.as_str() {
                continue;
            }
            return Ok(Some(GeocodeResult {
                match_kind: MatchKind::ContextOnly,
                label: compose_label(&record.name, context),
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
        self.enrich_context(options, context, context_record_ids)?;
        // The answer is the area its label leads with, the first found of
        // the most precise tier, so its id and layer name what it says.
        let mut primary: Option<(usize, RecordId, ContextRecord)> = None;
        for &record_id in context_record_ids.iter() {
            let Some(record) = self.pack.context_record(record_id)? else {
                continue;
            };
            let Some(rank) = LABEL_TIERS.iter().position(|tier| *tier == record.layer) else {
                continue;
            };
            if primary.as_ref().is_none_or(|(best, ..)| rank < *best) {
                primary = Some((rank, record_id, record));
            }
        }
        let Some((_, primary_record_id, record)) = primary else {
            return Ok(None);
        };
        let label = context_only_label(context).unwrap_or_else(|| record.label.clone());

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
            crate::stop::check()?;
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

/// The area tiers a point's label names, most precise first, as
/// [`context_only_label`] reads them.
const LABEL_TIERS: [&str; 7] = [
    "place",
    "neighbourhood",
    "locality",
    "district",
    "region",
    "postcode",
    "country",
];

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
        assert_eq!(result.id.as_deref(), Some("osm:node:10"));

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn reverse_uses_a_zip_corroborated_by_a_separate_zip_plus_four() {
        let temp_dir = temp_pack_dir("postal-conflict");
        let mut writer = Writer::create(&temp_dir).expect("writer");
        for (id, longitude, number, zip) in [
            (100, -77.0, "10", "20001"),
            (101, -77.00002, "11", "20001-1111"),
            (102, -77.00003, "10", "20002"),
            (103, -77.00010, "10", "20002-4214"),
        ] {
            let mut record = address_record();
            record.address.number = number.into();
            record.address.locality = Some("Washington".into());
            record.address.region = Some("DC".into());
            record.address.country = Some("US".into());
            record.address.postcode = Some(zip.into());
            record.geometry = point_geometry(longitude, 38.9);
            record.source = SourceProvenance::osm(ObjectType::Node, id);
            writer.write(&record.into(), None).expect("address");
        }
        writer.finish().expect("finish");
        let geocoder = PackReverseGeocoder::open(&temp_dir).expect("geocoder");
        let result = geocoder
            .reverse(GeocodeOptions::at(-77.0, 38.9))
            .expect("reverse")
            .result
            .expect("result");
        assert_eq!(result.id.as_deref(), Some("osm:node:102"));
        assert_eq!(result.context.postcode.as_deref(), Some("20002"));
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
                    ..Default::default()
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
                    details: None,
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
                    details: None,
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
                    details: None,
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
            building: None,
            access: Vec::new(),
            disputed: false,
            street_postcodes: Vec::new(),
            address: AddressComponents {
                block_number_len: None,
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
