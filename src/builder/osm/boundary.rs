use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use geo::{
    Area, BoundingRect, Centroid, Contains, InteriorPoint, LineString, MultiPolygon, Polygon, Rect,
};
use rayon::prelude::*;
use rstar::{AABB, RTree, RTreeObject};

use crate::{
    builder::report::BuilderReport,
    context::{AdminContextTuple, CONTEXT_FLAG_AMBIGUOUS_ADMIN, RecordContext, TierSet},
    enclosing::AreaTags,
    pack::RecordId,
    parse::lattice::CountryCode,
    record::{
        Entrance, Extent, Layer, LocationPrecision, ObjectType, PlaceLayer, PlaceRecord,
        Prominence, Record, SourceProvenance, place_rank, point_geometry,
    },
    util::{
        geo::{EARTH_RADIUS_M, point_lon_lat},
        text::normalize_for_compare,
    },
};

use super::{
    admin_levels::{self, admin_levels},
    containment::{SHAPES_LABEL, ShapeRef, ShapeWriter, Shapes, encode_shape, narrowest_extent},
    place::place_layer,
    poi::{PoiCandidate, outline},
    street::is_settlement,
    tags::{OsmTags, alternate_names, item, language_names, name_tags, prominence},
    timezone::Timezones,
};

const BOUNDARY_TAG_KEYS: [&str; 12] = [
    "ISO3166-1:alpha2",
    "ISO3166-1:alpha3",
    "ISO3166-2",
    "admin_level",
    "boundary",
    "country_code",
    "default_language",
    "designation",
    "license_plate_code",
    "place",
    "wikidata",
    "wikipedia",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundaryRelationStub {
    pub object_id: i64,
    pub members: Vec<BoundaryRelationMember>,
    pub label: Option<i64>,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundaryRelationMember {
    pub way_id: i64,
    pub role: BoundaryMemberRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundaryMemberRole {
    Outer,
    Inner,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Vertex {
    pub node_id: i64,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BoundaryWay {
    pub object_id: i64,
    pub tags: BTreeMap<String, String>,
    pub vertices: Vec<Vertex>,
}

#[derive(Debug)]
pub(crate) struct BoundaryIndex {
    boundaries: Vec<AcceptedBoundary>,
    tree: RTree<BoundaryBox>,
    shapes: Arc<Shapes>,
    dots: RTree<TownDot>,
    by_record: HashMap<RecordId, usize>,
    by_source: HashMap<(ObjectType, i64), usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct TownDot {
    pub(crate) record_id: RecordId,
    pub(crate) point: [f64; 2],
    pub(crate) name: String,
    pub(crate) settlement: Settlement,
    pub(crate) reach_m: f64,
    country: Option<RecordId>,
}

impl TownDot {
    pub(crate) fn new(
        record_id: RecordId,
        point: [f64; 2],
        name: String,
        settlement: Settlement,
    ) -> Self {
        Self {
            record_id,
            point,
            name,
            settlement,
            reach_m: 0.0,
            country: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Settlement {
    pub(crate) rank: u8,
    pub(crate) prominence: Prominence,
}

impl Settlement {
    pub(crate) fn max(self, other: Self) -> Self {
        if other.cmp_importance(self).is_lt() {
            other
        } else {
            self
        }
    }

    fn cmp_importance(self, other: Self) -> Ordering {
        self.rank
            .cmp(&other.rank)
            .then(other.prominence.cmp(&self.prominence))
    }
}

const ENVELOPE_SLACK: f64 = 1.000_001;

impl RTreeObject for TownDot {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        let [lon, lat] = self.point;
        let angle = self.reach_m / EARTH_RADIUS_M * ENVELOPE_SLACK;
        let tall = angle.to_degrees();
        let (south, north) = ((lat - tall).max(-90.0), (lat + tall).min(90.0));
        if lat.abs() + tall >= 90.0 {
            return AABB::from_corners([-180.0, south], [180.0, north]);
        }
        let wide = (angle.sin() / lat.to_radians().cos()).asin().to_degrees();
        AABB::from_corners([lon - wide, south], [lon + wide, north])
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SourceContext<'a> {
    pub country: Option<&'a str>,
    pub region: Option<&'a str>,
    pub district: Option<&'a str>,
    pub locality: Option<&'a str>,
    pub neighbourhood: Option<&'a str>,
    pub place: Option<&'a str>,
}

#[derive(Debug)]
struct AcceptedBoundary {
    record_id: RecordId,
    layer: PlaceLayer,
    tiers: TierSet,
    name: String,
    admin_level: Option<u8>,
    inferred_country_record_id: Option<RecordId>,
    source_object_type: ObjectType,
    source_object_id: i64,
    bounds: Rect<f64>,
    shape: ShapeRef,
    area: f64,
    interior_point: Option<[f64; 2]>,
    town: Option<Settlement>,
}

#[derive(Debug, Clone)]
struct BoundaryBox {
    envelope: AABB<[f64; 2]>,
    boundary_id: usize,
}

#[derive(Clone)]
struct BuiltBoundary {
    layer: PlaceLayer,
    admin_level: Option<u8>,
    place_type: Option<String>,
    name: String,
    alt_names: Vec<String>,
    language_names: Vec<(String, String)>,
    prominence: Prominence,
    wikidata: Option<u32>,
    place_layer: Option<PlaceLayer>,
    designation: Option<String>,
    inferred_country_code: Option<String>,
    subdivision_code: Option<String>,
    area_tags: AreaTags,
    plate_code: Option<String>,
    default_language: Option<String>,
    shape: ShapeRef,
    bounds: Rect<f64>,
    extent: Extent,
    area: f64,
    representative_point: [f64; 2],
    interior_point: Option<[f64; 2]>,
    /// Whether every outer way closed into a ring. A relation one of whose
    /// rings is left open, such as Russia's mainland, keeps the rings that
    /// closed (Kaliningrad) and covers only them.
    rings_closed: bool,
    settled: bool,
    town: Option<u8>,
    point_names: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct BoundaryCandidate {
    source_object_type: ObjectType,
    source_object_id: i64,
    label: Option<i64>,
    boundary: BuiltBoundary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContextOrigin {
    Boundary(u32),
    DerivedCountry(String),
}

pub(crate) struct BoundarySet {
    candidates: Vec<BoundaryCandidate>,
    derived_countries: Vec<(String, PlaceRecord)>,
    unmatched_admin_ids: Vec<String>,
    shapes: Arc<Shapes>,
}

struct Draft {
    candidate: BoundaryCandidate,
    block: Vec<u8>,
}

pub(crate) struct BoundaryBuilder {
    candidates: Vec<BoundaryCandidate>,
    shapes: ShapeWriter,
    heap_bytes: usize,
    vertices: u64,
}

impl BoundaryBuilder {
    pub(crate) fn new(scratch: &Arc<crate::extsort::Scratch>) -> crate::error::Result<Self> {
        Ok(Self {
            candidates: Vec::new(),
            shapes: ShapeWriter::create(scratch, SHAPES_LABEL)?,
            heap_bytes: 0,
            vertices: 0,
        })
    }

    pub(crate) fn add_ways(&mut self, ways: &[BoundaryWay]) -> crate::error::Result<()> {
        let drafts = ways
            .par_iter()
            .filter_map(|way| {
                let (boundary, block) = boundary_from_way(way)?;
                Some(Draft {
                    candidate: BoundaryCandidate {
                        source_object_type: ObjectType::Way,
                        source_object_id: way.object_id,
                        label: None,
                        boundary,
                    },
                    block,
                })
            })
            .collect::<Vec<_>>();
        self.write(drafts)
    }

    pub(crate) fn add_relations(
        &mut self,
        relations: &[BoundaryRelationStub],
        member_line: impl Fn(i64) -> crate::error::Result<Option<Vec<Vertex>>> + Sync,
    ) -> crate::error::Result<()> {
        let drafts = relations
            .par_iter()
            .map(|stub| {
                Ok(
                    boundary_from_relation(stub, &member_line)?.map(|(boundary, block)| Draft {
                        candidate: BoundaryCandidate {
                            source_object_type: ObjectType::Relation,
                            source_object_id: stub.object_id,
                            label: stub.label,
                            boundary,
                        },
                        block,
                    }),
                )
            })
            .collect::<crate::error::Result<Vec<_>>>()?;
        self.write(drafts.into_iter().flatten().collect())
    }

    fn write(&mut self, drafts: Vec<Draft>) -> crate::error::Result<()> {
        for Draft {
            mut candidate,
            block,
        } in drafts
        {
            candidate.boundary.shape = self.shapes.write(&block)?;
            self.vertices += u64::from(shape_vertices(&block));
            self.heap_bytes += candidate.heap_bytes();
            self.candidates.push(candidate);
        }
        Ok(())
    }

    pub(crate) fn heap_bytes(&self) -> usize {
        self.heap_bytes
    }

    pub(crate) fn vertices(&self) -> u64 {
        self.vertices
    }

    pub(crate) fn finish(self) -> crate::error::Result<BoundarySet> {
        let shapes = Arc::new(self.shapes.finish()?);
        let mut candidates = self.candidates;
        let unmatched_admin_ids = assign_layers_by_containment(&mut candidates, &shapes);
        let derived_countries = derived_country_records(&candidates);
        Ok(BoundarySet {
            candidates,
            derived_countries,
            unmatched_admin_ids,
            shapes,
        })
    }
}

fn shape_vertices(block: &[u8]) -> u32 {
    block.get(..4).map_or(0, |count| {
        u32::from_le_bytes(count.try_into().expect("4 bytes"))
    })
}

impl BoundaryCandidate {
    fn heap_bytes(&self) -> usize {
        let boundary = &self.boundary;
        let text = |value: &Option<String>| value.as_ref().map_or(0, String::capacity);
        std::mem::size_of::<Self>()
            + boundary.name.capacity()
            + boundary.alt_names.capacity() * std::mem::size_of::<String>()
            + boundary
                .alt_names
                .iter()
                .map(String::capacity)
                .sum::<usize>()
            + boundary.point_names.capacity() * std::mem::size_of::<String>()
            + boundary
                .point_names
                .iter()
                .map(String::capacity)
                .sum::<usize>()
            + text(&boundary.place_type)
            + text(&boundary.designation)
            + text(&boundary.inferred_country_code)
            + text(&boundary.subdivision_code)
            + text(&boundary.area_tags.admin_level)
            + text(&boundary.area_tags.boundary)
            + text(&boundary.area_tags.place)
            + text(&boundary.plate_code)
            + text(&boundary.default_language)
            + boundary.language_names.capacity() * std::mem::size_of::<(String, String)>()
            + boundary
                .language_names
                .iter()
                .map(|(code, name)| code.capacity() + name.capacity())
                .sum::<usize>()
    }
}

fn covers_point(shapes: &Shapes, boundary: &BuiltBoundary, [lon, lat]: [f64; 2]) -> bool {
    let (min, max) = (boundary.bounds.min(), boundary.bounds.max());
    (min.x..=max.x).contains(&lon)
        && (min.y..=max.y).contains(&lat)
        && shapes.covers(boundary.shape, lon, lat)
}

impl BoundarySet {
    pub(crate) fn heap_bytes(&self) -> usize {
        self.candidates
            .iter()
            .map(BoundaryCandidate::heap_bytes)
            .sum()
    }

    pub(crate) fn shapes(&self) -> &Arc<Shapes> {
        &self.shapes
    }

    pub(crate) fn unmatched_admin_ids(&self) -> &[String] {
        &self.unmatched_admin_ids
    }

    pub(crate) fn len(&self) -> usize {
        self.candidates.len()
    }

    pub(crate) fn place_records(&self, report: &mut BuilderReport) -> Vec<(ContextOrigin, Record)> {
        let mut records = Vec::new();
        for (code, record) in &self.derived_countries {
            report.accept_place(PlaceLayer::Country);
            records.push((
                ContextOrigin::DerivedCountry(code.clone()),
                Record::Place(PlaceLayer::Country, record.clone()),
            ));
        }
        for (index, candidate) in self.candidates.iter().enumerate() {
            let boundary = &candidate.boundary;
            if boundary.admin_level.is_some() {
                report.accept_place(boundary.layer);
            } else {
                report.accept_place_area(boundary.layer);
            }
            records.push((
                ContextOrigin::Boundary(index as u32),
                Record::Place(
                    boundary.layer,
                    PlaceRecord {
                        name: boundary.name.clone(),
                        alt_names: boundary
                            .alt_names
                            .iter()
                            .chain(&boundary.point_names)
                            .cloned()
                            .collect(),
                        place_type: match (boundary.admin_level, &boundary.place_type) {
                            (Some(level), _) => format!("admin_level:{level}"),
                            (None, Some(place)) => place.clone(),
                            (None, None) => "area".to_string(),
                        },
                        geometry: point_geometry(
                            boundary.representative_point[0],
                            boundary.representative_point[1],
                        ),
                        source: SourceProvenance::osm(
                            candidate.source_object_type,
                            candidate.source_object_id,
                        ),
                        prominence: boundary.prominence,
                        wikidata: boundary.wikidata,
                    },
                ),
            ));
        }
        records
    }

    pub(crate) fn place_links(&mut self) -> PlaceLinks {
        let mut links = PlaceLinks {
            labels: HashMap::new(),
            items: HashMap::new(),
            names: HashMap::new(),
            matched: HashMap::new(),
            brought: HashMap::new(),
            waiting: Vec::new(),
            strongest: vec![None; self.candidates.len()],
            lookup_bytes: 0,
            brought_list_bytes: 0,
        };
        for (index, candidate) in self.candidates.iter_mut().enumerate() {
            let layer = candidate.boundary.layer;
            if let Some(label) = candidate.label.take() {
                links.labels.entry((label, layer)).or_insert(index);
            }
            if let Some(item) = candidate.boundary.wikidata {
                links.items.entry((item, layer)).or_default().push(index);
            }
            links
                .names
                .entry((normalize_for_compare(&candidate.boundary.name), layer))
                .or_default()
                .push(index);
        }
        links.lookup_bytes = links.measure_lookups();
        links
    }

    pub(crate) fn match_place_point(
        &self,
        links: &mut PlaceLinks,
        at: u64,
        layer: PlaceLayer,
        place: &PlaceRecord,
    ) {
        let found = links.find(
            layer,
            place,
            |index, point| self.holds(index, point),
            |index, point| covers_point(&self.shapes, &self.candidates[index].boundary, point),
        );
        match (found, place.wikidata) {
            (Some((index, link)), item) => {
                links.matched.insert(at, (index, link));
                if let Some(item) = item {
                    let list = links.brought.entry((item, layer)).or_default();
                    let capacity = list.capacity();
                    list.push(index);
                    links.brought_list_bytes +=
                        (list.capacity() - capacity) * std::mem::size_of::<usize>();
                }
            }
            (None, Some(item)) => {
                if let Some(point) = point_lon_lat(&place.geometry) {
                    links.waiting.push((at, item, layer, point));
                }
            }
            (None, None) => {}
        }
    }

    pub(crate) fn group_place_points(&self, links: &mut PlaceLinks) {
        for (at, item, layer, point) in std::mem::take(&mut links.waiting) {
            let held = links.brought.get(&(item, layer)).and_then(|boundaries| {
                boundaries
                    .iter()
                    .filter(|&&index| self.holds(index, point))
                    .min()
                    .copied()
            });
            if let Some(index) = held {
                links.matched.insert(at, (index, PlaceLink::Item));
            }
        }
    }

    pub(crate) fn merge_place_point(
        &mut self,
        links: &mut PlaceLinks,
        at: u64,
        place: &PlaceRecord,
    ) -> bool {
        let Some(&(index, link)) = links.matched.get(&at) else {
            return false;
        };
        let boundary = &mut self.candidates[index].boundary;
        if links.strongest[index].is_none_or(|held| held.link < link)
            && let Some(point) = point_lon_lat(&place.geometry)
        {
            links.strongest[index] = Some(Strongest {
                link,
                point,
                item: place.wikidata,
            });
        }
        for name in std::iter::once(&place.name).chain(&place.alt_names) {
            let folded = name.to_lowercase();
            let known = std::iter::once(&boundary.name)
                .chain(&boundary.alt_names)
                .chain(&boundary.point_names)
                .any(|known| known.to_lowercase() == folded);
            if !known {
                boundary.point_names.push(name.clone());
            }
        }
        boundary.prominence = boundary.prominence.max(place.prominence);
        boundary.settled |= is_settlement(&place.place_type);
        if let Some(rank) = town_rank(&place.place_type) {
            boundary.town = Some(boundary.town.map_or(rank, |held| held.min(rank)));
        }
        true
    }

    pub(crate) fn finish_place_merges(&mut self, links: PlaceLinks) {
        for (candidate, strongest) in self.candidates.iter_mut().zip(links.strongest) {
            if let Some(Strongest { point, item, .. }) = strongest {
                candidate.boundary.representative_point = point;
                candidate.boundary.wikidata = candidate.boundary.wikidata.or(item);
            }
        }
    }

    fn holds(&self, index: usize, [lon, lat]: [f64; 2]) -> bool {
        let bounds = self.candidates[index].boundary.bounds;
        let (min, max) = (bounds.min(), bounds.max());
        (min.x..=max.x).contains(&lon) && (min.y..=max.y).contains(&lat)
    }

    pub(crate) fn is_settled(&self, index: usize) -> bool {
        self.candidates[index].boundary.settled
    }

    pub(crate) fn context_names(
        &self,
        boundary_record_ids: &[RecordId],
        country_record_ids: &HashMap<String, RecordId>,
    ) -> HashMap<RecordId, Vec<String>> {
        let names = |name: &String, alt_names: &[String]| {
            std::iter::once(name)
                .chain(alt_names)
                .cloned()
                .collect::<Vec<_>>()
        };
        let boundaries =
            self.candidates
                .iter()
                .zip(boundary_record_ids)
                .map(|(candidate, record_id)| {
                    let boundary = &candidate.boundary;
                    (*record_id, names(&boundary.name, &boundary.alt_names))
                });
        let countries = self.derived_countries.iter().filter_map(|(code, record)| {
            Some((
                *country_record_ids.get(code)?,
                names(&record.name, &record.alt_names),
            ))
        });
        boundaries.chain(countries).collect()
    }

    pub(crate) fn area_tags(
        &self,
        boundary_record_ids: &[RecordId],
    ) -> BTreeMap<RecordId, AreaTags> {
        self.candidates
            .iter()
            .zip(boundary_record_ids)
            .map(|(candidate, record_id)| (*record_id, candidate.boundary.area_tags.clone()))
            .collect()
    }

    pub(crate) fn coverage(&self) -> crate::builder::addresses::Coverage {
        let mut coverage = crate::builder::addresses::Coverage::default();
        for candidate in &self.candidates {
            let boundary = &candidate.boundary;
            if boundary.layer == PlaceLayer::Country
                && let Some(code) = &boundary.inferred_country_code
            {
                coverage.countries.insert(code.to_ascii_uppercase());
            }
            if let Some(code) = &boundary.subdivision_code {
                coverage.subdivisions.insert(code.clone());
            }
        }
        let whole = coverage.countries.clone();
        coverage.subdivisions.retain(|code| {
            !code
                .split('-')
                .next()
                .is_some_and(|country| whole.contains(country))
        });
        coverage
    }

    pub(crate) fn area(&self, code: &str) -> Option<impl Fn([f64; 2]) -> bool + '_> {
        let region = code.contains('-');
        let boundaries = self
            .candidates
            .iter()
            .map(|candidate| &candidate.boundary)
            .filter(|boundary| {
                if region {
                    boundary
                        .subdivision_code
                        .as_deref()
                        .is_some_and(|held| held.eq_ignore_ascii_case(code))
                } else {
                    boundary.layer == PlaceLayer::Country
                        && boundary
                            .inferred_country_code
                            .as_deref()
                            .is_some_and(|held| held.eq_ignore_ascii_case(code))
                }
            })
            .collect::<Vec<_>>();
        let closed = boundaries.iter().all(|boundary| boundary.rings_closed);
        (!boundaries.is_empty() && closed).then_some(move |point| {
            boundaries
                .iter()
                .any(|boundary| covers_point(&self.shapes, boundary, point))
        })
    }

    pub(crate) fn country_codes(
        &self,
        boundary_record_ids: &[RecordId],
        country_record_ids: &HashMap<String, RecordId>,
    ) -> HashMap<RecordId, CountryCode> {
        let boundaries = self
            .candidates
            .iter()
            .zip(boundary_record_ids)
            .filter(|(candidate, _)| candidate.boundary.layer == PlaceLayer::Country)
            .filter_map(|(candidate, record_id)| {
                let code = candidate.boundary.inferred_country_code.as_deref()?;
                Some((*record_id, CountryCode::parse(code)?))
            });
        let derived = country_record_ids
            .iter()
            .filter_map(|(code, record_id)| Some((*record_id, CountryCode::parse(code)?)));
        boundaries.chain(derived).collect()
    }

    pub(crate) fn language_names(
        &self,
        boundary_record_ids: &[RecordId],
    ) -> HashMap<RecordId, crate::pack::LanguageNames> {
        self.candidates
            .iter()
            .zip(boundary_record_ids)
            .filter(|(candidate, _)| !candidate.boundary.language_names.is_empty())
            .map(|(candidate, record_id)| (*record_id, candidate.boundary.language_names.clone()))
            .collect()
    }

    pub(crate) fn subdivision_codes(
        &self,
        boundary_record_ids: &[RecordId],
    ) -> HashMap<RecordId, String> {
        self.candidates
            .iter()
            .zip(boundary_record_ids)
            .filter_map(|(candidate, record_id)| {
                let code = candidate.boundary.subdivision_code.clone()?;
                Some((*record_id, code))
            })
            .collect()
    }

    pub(crate) fn plate_codes(
        &self,
        boundary_record_ids: &[RecordId],
    ) -> HashMap<RecordId, String> {
        self.candidates
            .iter()
            .zip(boundary_record_ids)
            .filter_map(|(candidate, record_id)| {
                Some((*record_id, candidate.boundary.plate_code.clone()?))
            })
            .collect()
    }

    pub(crate) fn default_languages(
        &self,
        boundary_record_ids: &[RecordId],
    ) -> HashMap<RecordId, String> {
        self.candidates
            .iter()
            .zip(boundary_record_ids)
            .filter_map(|(candidate, record_id)| {
                Some((*record_id, candidate.boundary.default_language.clone()?))
            })
            .collect()
    }

    pub(crate) fn extents(&self, boundary_record_ids: &[RecordId]) -> Vec<(RecordId, Extent)> {
        self.candidates
            .iter()
            .zip(boundary_record_ids)
            .map(|(candidate, record_id)| (*record_id, candidate.boundary.extent))
            .collect()
    }

    pub(crate) fn region_groups(
        &self,
        boundary_record_ids: &[RecordId],
    ) -> HashMap<RecordId, Vec<RecordId>> {
        let regions = self
            .candidates
            .iter()
            .zip(boundary_record_ids)
            // By tier group, not the exact tier: Japan maps a group of regions
            // at 3 and its prefectures at 4, which are the region and its
            // sub-region, and a group has to pair with the prefectures inside
            // it.
            .filter(|(candidate, _)| {
                candidate.boundary.layer.record_layer() == Layer::Region
                    && candidate.boundary.admin_level.is_some()
            })
            .map(|(candidate, record_id)| {
                (&candidate.boundary, candidate.boundary.area, *record_id)
            })
            .collect::<Vec<_>>();
        regions
            .par_iter()
            .filter_map(|(region, area, record_id)| {
                let groups = regions
                    .iter()
                    .filter(|(group, group_area, _)| {
                        group_area > area
                            && !same_text(&group.name, &region.name)
                            && covers_point(&self.shapes, group, region.representative_point)
                    })
                    .map(|(_, _, group_id)| *group_id)
                    .collect::<Vec<_>>();
                (!groups.is_empty()).then_some((*record_id, groups))
            })
            .collect()
    }

    pub(crate) fn into_index(
        self,
        boundary_record_ids: &[RecordId],
        country_record_ids: &HashMap<String, RecordId>,
    ) -> BoundaryIndex {
        let accepted = self
            .candidates
            .into_iter()
            .zip(boundary_record_ids)
            .map(|(candidate, record_id)| {
                let boundary = candidate.boundary;
                AcceptedBoundary {
                    record_id: *record_id,
                    layer: boundary.layer,
                    tiers: boundary_tiers(&boundary),
                    area: boundary.area,
                    interior_point: boundary.interior_point,
                    bounds: boundary.bounds,
                    inferred_country_record_id: boundary
                        .inferred_country_code
                        .as_deref()
                        .and_then(|code| country_record_ids.get(code).copied()),
                    town: [
                        boundary.town,
                        boundary.place_type.as_deref().and_then(town_rank),
                        boundary.area_tags.place.as_deref().and_then(town_rank),
                    ]
                    .into_iter()
                    .flatten()
                    .min()
                    .map(|rank| Settlement {
                        rank,
                        prominence: boundary.prominence,
                    }),
                    name: boundary.name,
                    admin_level: boundary.admin_level,
                    source_object_type: candidate.source_object_type,
                    source_object_id: candidate.source_object_id,
                    shape: boundary.shape,
                }
            })
            .collect();
        BoundaryIndex::new(accepted, self.shapes)
    }
}

pub(crate) fn record_context(
    index: &BoundaryIndex,
    record: &Record,
    own: Option<OwnArea>,
) -> Option<RecordContext> {
    let [lon, lat] = record.display_point()?;
    let source = match record {
        Record::Address(record) => SourceContext {
            country: record.address.country.as_deref(),
            region: record.address.region.as_deref(),
            locality: record.address.locality.as_deref(),
            neighbourhood: record.address.areas.last().map(String::as_str),
            place: record.address.place.as_deref(),
            ..SourceContext::default()
        },
        Record::Interpolation(record) => SourceContext {
            country: record.address.country.as_deref(),
            region: record.address.region.as_deref(),
            locality: record.address.locality.as_deref(),
            place: record.address.place.as_deref(),
            ..SourceContext::default()
        },
        Record::Poi(record) => record
            .address
            .as_ref()
            .map(|address| SourceContext {
                country: address.country.as_deref(),
                region: address.region.as_deref(),
                locality: address.locality.as_deref(),
                neighbourhood: address.areas.last().map(String::as_str),
                place: address.place.as_deref(),
                ..SourceContext::default()
            })
            .unwrap_or_default(),
        Record::Place(_, record) => SourceContext {
            place: Some(&record.name),
            ..SourceContext::default()
        },
        Record::Street(_) | Record::Postcode(_) => SourceContext::default(),
    };
    Some(index.context_around(lon, lat, &source, own))
}

pub(crate) fn boundary_tags(tags: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    BOUNDARY_TAG_KEYS
        .into_iter()
        .filter_map(|key| Some((key.to_string(), tags.get(key)?.clone())))
        .chain(name_tags(tags))
        .collect()
}

impl RTreeObject for BoundaryBox {
    type Envelope = AABB<[f64; 2]>;

    fn envelope(&self) -> Self::Envelope {
        self.envelope
    }
}

impl BoundaryIndex {
    pub(super) fn census_codes(
        &self,
        subdivisions: &HashMap<RecordId, String>,
        area_names: &HashMap<RecordId, Vec<String>>,
        gazetteer: &super::tiger::Gazetteer,
    ) -> HashMap<RecordId, String> {
        let mut codes: HashMap<_, _> = subdivisions
            .iter()
            .filter_map(|(&id, iso)| Some((id, gazetteer.states.get(iso)?.clone())))
            .collect();
        for county in &gazetteer.counties {
            let context = self.context_for_point(county.lon, county.lat, &SourceContext::default());
            let tuple = &context.admin_context;
            let Some(state) = tuple
                .get(PlaceLayer::Region)
                .and_then(|id| subdivisions.get(&id))
            else {
                continue;
            };
            if state.strip_prefix("US-") != Some(county.state.as_str()) {
                continue;
            }
            for tier in [PlaceLayer::District, PlaceLayer::SubRegion] {
                let Some(id) = tuple.get(tier) else {
                    continue;
                };
                if area_names.get(&id).is_some_and(|names| {
                    names
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(&county.name))
                }) {
                    codes.insert(id, county.code.clone());
                }
            }
        }
        codes
    }

    pub(crate) fn timezones(&self, zones: &Timezones) -> (Vec<(RecordId, u16)>, u64) {
        let zoned = self
            .boundaries
            .par_iter()
            .filter_map(|boundary| {
                let block = self.shapes.block(boundary.shape)?;
                Some((boundary.record_id, zones.of_area(block, boundary.bounds)?))
            })
            .collect();
        (zoned, self.boundaries.len() as u64)
    }

    fn new(boundaries: Vec<AcceptedBoundary>, shapes: Arc<Shapes>) -> Self {
        let boxes = boundaries
            .iter()
            .enumerate()
            .map(|(boundary_id, boundary)| BoundaryBox {
                envelope: envelope_from_rect(boundary.bounds),
                boundary_id,
            })
            .collect::<Vec<_>>();
        let by_record = boundaries
            .iter()
            .enumerate()
            .map(|(at, boundary)| (boundary.record_id, at))
            .collect();
        let by_source = boundaries
            .iter()
            .enumerate()
            .map(|(at, boundary)| ((boundary.source_object_type, boundary.source_object_id), at))
            .collect();
        Self {
            boundaries,
            tree: RTree::bulk_load(boxes),
            shapes,
            dots: RTree::new(),
            by_record,
            by_source,
        }
    }

    pub(crate) fn add_towns(
        &mut self,
        named: impl IntoIterator<Item = (RecordId, Settlement)>,
        dots: Vec<TownDot>,
    ) {
        let at = self
            .boundaries
            .iter()
            .enumerate()
            .map(|(index, boundary)| (boundary.record_id, index))
            .collect::<HashMap<_, _>>();
        for (record_id, settlement) in named {
            if let Some(boundary) = at.get(&record_id).map(|index| &mut self.boundaries[*index]) {
                boundary.town = Some(
                    boundary
                        .town
                        .map_or(settlement, |held| held.max(settlement)),
                );
            }
        }
        let dots = dots
            .into_iter()
            .map(|mut dot| {
                let [lon, lat] = dot.point;
                let covered = self.covered(lon, lat);
                dot.country = country_of(&best_of_each_tier(&covered, &SourceContext::default()).0);
                dot
            })
            .collect();
        self.dots = RTree::bulk_load(dots);
    }

    pub(crate) fn town_reach(&self) -> Vec<(RecordId, u32)> {
        let mut reach = self
            .dots
            .iter()
            .map(|dot| (dot.record_id, dot.reach_m as u32))
            .collect::<Vec<_>>();
        reach.sort_unstable();
        reach
    }

    pub(crate) fn settlement_areas(&self) -> impl Iterator<Item = (RecordId, u8, [f64; 2])> + '_ {
        self.boundaries.iter().filter_map(|boundary| {
            Some((
                boundary.record_id,
                boundary.town?.rank,
                boundary.interior_point?,
            ))
        })
    }

    pub(crate) fn areas_at(&self, lon: f64, lat: f64) -> impl Iterator<Item = RecordId> + '_ {
        self.covered(lon, lat)
            .into_iter()
            .filter(|boundary| boundary.layer > PlaceLayer::Country)
            .map(|boundary| boundary.record_id)
    }

    pub(crate) fn smallest_area_at(
        &self,
        lon: f64,
        lat: f64,
        holds: impl Fn(RecordId) -> bool,
    ) -> Option<RecordId> {
        self.covered(lon, lat)
            .into_iter()
            .filter(|boundary| boundary.layer > PlaceLayer::Country && holds(boundary.record_id))
            .min_by(|left, right| left.area.total_cmp(&right.area))
            .map(|boundary| boundary.record_id)
    }

    pub(crate) fn area_context(
        &self,
        tier: PlaceLayer,
        record: &Record,
        source: &SourceProvenance,
        area: Option<OwnArea>,
        mut context: RecordContext,
    ) -> Option<RecordContext> {
        let [lon, lat] = record.display_point()?;
        let own = self
            .by_source
            .get(&(source.object_type, source.object_id))
            .map_or_else(
                || Rect::new((lon, lat), (lon, lat)),
                |&at| self.boundaries[at].bounds,
            );
        if let Some([inner_lon, inner_lat]) = area.and_then(|area| area.point)
            && [inner_lon, inner_lat] != [lon, lat]
        {
            context.admin_context = self
                .context_for_point(inner_lon, inner_lat, &SourceContext::default())
                .admin_context;
        }
        context.admin_context = context.admin_context.above(tier);
        context.admin_context.retain(|entry| {
            context.enclosing.contains(&entry.record_id)
                && self
                    .by_record
                    .get(&entry.record_id)
                    .is_none_or(|&at| holds(self.boundaries[at].bounds, own))
        });
        context.flags = crate::context::CONTEXT_FLAG_OUTER_ONLY;
        Some(context)
    }

    pub(crate) fn context_for_point(
        &self,
        lon: f64,
        lat: f64,
        source_context: &SourceContext<'_>,
    ) -> RecordContext {
        self.context_around(lon, lat, source_context, None)
    }

    pub(crate) fn own_area(&self, origin: u32) -> Option<OwnArea> {
        self.boundaries
            .get(usize::try_from(origin).ok()?)
            .map(|boundary| OwnArea {
                area: boundary.area,
                point: boundary.interior_point,
            })
    }

    pub(crate) fn context_around(
        &self,
        lon: f64,
        lat: f64,
        source_context: &SourceContext<'_>,
        own: Option<OwnArea>,
    ) -> RecordContext {
        if (self.boundaries.is_empty() && self.dots.size() == 0)
            || !lon.is_finite()
            || !lat.is_finite()
        {
            return RecordContext::default();
        }

        let at = self.covering(lon, lat, source_context);
        let around = own
            .and_then(|own| own.point)
            .filter(|point| *point != [lon, lat])
            .map(|[lon, lat]| self.covering(lon, lat, source_context));
        let (covered, derived_country) = match around {
            Some(around) => (around.covered, around.derived_country),
            None => (at.covered, at.derived_country),
        };
        RecordContext {
            admin_context: at.tuple,
            flags: at.flags,
            enclosing: enclosing(covered, derived_country, own.map(|own| own.area)),
            zone: None,
        }
    }

    fn covered(&self, lon: f64, lat: f64) -> Vec<&AcceptedBoundary> {
        let envelope = AABB::from_point([lon, lat]);
        self.tree
            .locate_in_envelope_intersecting(envelope)
            .map(|boundary_box| &self.boundaries[boundary_box.boundary_id])
            .filter(|boundary| self.shapes.covers(boundary.shape, lon, lat))
            .collect()
    }

    fn covering(&self, lon: f64, lat: f64, source_context: &SourceContext<'_>) -> Covering<'_> {
        let covered = self.covered(lon, lat);
        let (best, flags) = best_of_each_tier(&covered, source_context);
        let mut tuple = match self.town(&covered, &best, source_context, [lon, lat]) {
            Some(town) => self.around_town(&town, &covered, &best, source_context),
            None => tiered(&best),
        };
        let mut derived_country = None;
        if tuple.country_record_id().is_none()
            && let Some(country) = inferred_country(&best)
        {
            tuple.set(PlaceLayer::Country, country);
            derived_country = Some(country);
        }

        Covering {
            covered,
            tuple,
            flags,
            derived_country,
        }
    }

    fn town<'a>(
        &'a self,
        covered: &[&'a AcceptedBoundary],
        best: &Best<'a>,
        source_context: &SourceContext<'_>,
        [lon, lat]: [f64; 2],
    ) -> Option<Town<'a>> {
        let named = |name: &str| {
            source_context
                .locality
                .is_some_and(|locality| same_text(locality, name))
        };
        let tiered = best[PlaceLayer::Locality as usize];
        let areas = covered
            .iter()
            .filter(|boundary| tiered.is_none_or(|tiered| boundary.area >= tiered.area))
            .filter_map(|boundary| {
                Some(Candidate {
                    town: Town::Area(boundary),
                    named: named(&boundary.name),
                    settlement: boundary.town?,
                    size: boundary.area,
                })
            });
        let is_dot = |dot: &TownDot| {
            dot.point == [lon, lat] && source_context.place.is_some_and(|place| place == dot.name)
        };
        let mut reaching = match tiered {
            Some(_) => Vec::new(),
            None => self.dots_reaching(lon, lat, best).collect::<Vec<_>>(),
        };
        if reaching.iter().any(|(dot, _)| is_dot(dot)) {
            reaching.clear();
        }
        let dots = reaching.into_iter().map(|(dot, share)| Candidate {
            town: Town::Dot(dot),
            named: named(&dot.name),
            settlement: dot.settlement,
            size: share,
        });
        let town = [
            areas.min_by(Candidate::cmp_alike),
            dots.min_by(Candidate::cmp_alike),
        ]
        .into_iter()
        .flatten()
        .min_by(Candidate::cmp_across)?
        .town;
        (tiered.map(|tiered| tiered.record_id) != Some(town.record_id())).then_some(town)
    }

    fn dots_reaching<'a>(
        &'a self,
        lon: f64,
        lat: f64,
        best: &Best<'_>,
    ) -> impl Iterator<Item = (&'a TownDot, f64)> {
        let country = country_of(best);
        [lon, lon + 360.0, lon - 360.0]
            .into_iter()
            .flat_map(move |turned| {
                self.dots
                    .locate_in_envelope_intersecting(AABB::from_point([turned, lat]))
            })
            .filter(move |dot| dot.country == country)
            .filter_map(move |dot| {
                let distance = crate::util::geo::haversine_m(lon, lat, dot.point[0], dot.point[1]);
                let share = distance / dot.reach_m;
                (share <= 1.0).then_some((dot, share))
            })
    }

    fn lies_inside(&self, inner: &AcceptedBoundary, outer: &AcceptedBoundary) -> bool {
        let covers = |area: &AcceptedBoundary, point: Option<[f64; 2]>| {
            point.map(|[lon, lat]| self.shapes.covers(area.shape, lon, lat))
        };
        match (
            covers(outer, inner.interior_point),
            covers(inner, outer.interior_point),
        ) {
            (Some(outer_holds), Some(inner_holds)) => {
                outer_holds
                    && inner.area < outer.area
                    && (!inner_holds || inner.area * 2.0 < outer.area)
            }
            _ => inner.area < outer.area,
        }
    }

    fn around_town<'a>(
        &self,
        town: &Town<'_>,
        covered: &[&'a AcceptedBoundary],
        best: &Best<'_>,
        source_context: &SourceContext<'_>,
    ) -> AdminContextTuple {
        let mut slots: Best<'a> = Default::default();
        let keep = |slots: &mut Best<'a>, tier: PlaceLayer, boundary: &'a AcceptedBoundary| {
            let slot = &mut slots[tier as usize];
            if slot
                .is_none_or(|held| compare_boundary(tier, boundary, held, source_context).is_lt())
            {
                *slot = Some(boundary);
            }
        };
        let (mut above, mut inside) = (Vec::new(), Vec::new());
        for boundary in covered {
            if boundary.record_id == town.record_id() {
                continue;
            }
            let layer = boundary.layer;
            match town.is_inside(boundary) {
                _ if layer <= PlaceLayer::Region || boundary.admin_level.is_none() => {
                    keep(&mut slots, layer, boundary);
                }
                Some(false) if layer >= PlaceLayer::Locality => above.push(*boundary),
                Some(true) if layer <= PlaceLayer::Locality => inside.push(*boundary),
                _ => keep(&mut slots, layer, boundary),
            }
        }
        for (movers, tiers, outward) in [(above, ABOVE_TOWN, true), (inside, INSIDE_TOWN, false)] {
            let depth = |boundary: &AcceptedBoundary| {
                movers
                    .iter()
                    .filter(|other| {
                        if outward {
                            self.lies_inside(other, boundary)
                        } else {
                            self.lies_inside(boundary, other)
                        }
                    })
                    .count()
            };
            let mut depths = movers
                .iter()
                .map(|boundary| (depth(boundary), *boundary))
                .collect::<Vec<_>>();
            depths.sort_by_key(|(depth, _)| *depth);
            let free = tiers
                .into_iter()
                .filter(|tier| slots[*tier as usize].is_none())
                .collect::<Vec<_>>();
            let mut groups = depths.chunk_by(|(left, _), (right, _)| left == right);
            for tier in free {
                let Some(group) = groups.next() else { break };
                slots[tier as usize] = group
                    .iter()
                    .map(|(_, boundary)| *boundary)
                    .min_by(|left, right| compare_side_by_side(tier, left, right, source_context));
            }
        }
        let mut tuple = AdminContextTuple::from_tiers(
            PlaceLayer::ALL
                .into_iter()
                .flat_map(|tier| slots[tier as usize].map(|boundary| (tier, boundary.record_id))),
        );
        if let Town::Area(area) = town {
            for tier in area.tiers.iter() {
                if tier <= PlaceLayer::Region
                    && best[tier as usize].map(|held| held.record_id) == Some(area.record_id)
                {
                    tuple.set(tier, area.record_id);
                }
            }
        }
        tuple.set(PlaceLayer::Locality, town.record_id());
        tuple
    }
}

fn compare_side_by_side(
    tier: PlaceLayer,
    left: &AcceptedBoundary,
    right: &AcceptedBoundary,
    source_context: &SourceContext<'_>,
) -> Ordering {
    let named = |boundary: &AcceptedBoundary| {
        source_value_for_layer(tier, source_context)
            .is_some_and(|value| same_text(value, &boundary.name))
    };
    named(right)
        .cmp(&named(left))
        .then(right.admin_level.cmp(&left.admin_level))
        .then_with(|| compare_boundary(tier, left, right, source_context))
}

struct Candidate<'a> {
    town: Town<'a>,
    named: bool,
    settlement: Settlement,
    size: f64,
}

impl Candidate<'_> {
    fn cmp_alike(left: &Self, right: &Self) -> Ordering {
        right
            .named
            .cmp(&left.named)
            .then(left.settlement.rank.cmp(&right.settlement.rank))
            .then(left.size.total_cmp(&right.size))
            .then(left.town.record_id().cmp(&right.town.record_id()))
    }

    fn cmp_across(left: &Self, right: &Self) -> Ordering {
        right
            .named
            .cmp(&left.named)
            .then(left.settlement.cmp_importance(right.settlement))
            .then(left.town.is_dot().cmp(&right.town.is_dot()))
    }
}

fn best_of_each_tier<'a>(
    covered: &[&'a AcceptedBoundary],
    source_context: &SourceContext<'_>,
) -> (Best<'a>, u16) {
    let mut flags = 0;
    let mut best: Best<'a> = Default::default();
    for boundary in covered {
        let slot = &mut best[boundary.layer as usize];
        match slot {
            Some(held) => {
                flags |= CONTEXT_FLAG_AMBIGUOUS_ADMIN;
                if compare_boundary(boundary.layer, boundary, held, source_context).is_lt() {
                    *slot = Some(boundary);
                }
            }
            None => *slot = Some(boundary),
        }
    }
    (best, flags)
}

type Best<'a> = [Option<&'a AcceptedBoundary>; PlaceLayer::ALL.len()];

fn tiered(best: &Best<'_>) -> AdminContextTuple {
    AdminContextTuple::from_tiers(PlaceLayer::ALL.into_iter().flat_map(|tier| {
        best[tier as usize]
            .into_iter()
            .flat_map(move |boundary| boundary.tiers.iter().map(|at| (at, boundary.record_id)))
    }))
}

enum Town<'a> {
    Area(&'a AcceptedBoundary),
    Dot(&'a TownDot),
}

impl Town<'_> {
    fn record_id(&self) -> RecordId {
        match self {
            Self::Area(area) => area.record_id,
            Self::Dot(dot) => dot.record_id,
        }
    }

    fn is_dot(&self) -> bool {
        matches!(self, Self::Dot(_))
    }

    fn is_inside(&self, unit: &AcceptedBoundary) -> Option<bool> {
        match self {
            Self::Area(area) => Some(unit.area < area.area),
            Self::Dot(dot) => unit_rank(unit).map(|rank| rank >= dot.settlement.rank),
        }
    }
}

const ABOVE_TOWN: [PlaceLayer; 3] = [
    PlaceLayer::SubDistrict,
    PlaceLayer::District,
    PlaceLayer::SubRegion,
];
const INSIDE_TOWN: [PlaceLayer; 3] = [
    PlaceLayer::SubLocality,
    PlaceLayer::Neighbourhood,
    PlaceLayer::SubNeighbourhood,
];

fn unit_rank(unit: &AcceptedBoundary) -> Option<u8> {
    unit.admin_level.map(|level| level.saturating_mul(2))
}

pub(crate) fn town_rank(place_type: &str) -> Option<u8> {
    TOWN_KINDS
        .contains(&place_type)
        .then(|| place_rank(place_type))
        .flatten()
}

const TOWN_KINDS: [&str; 4] = ["city", "town", "village", "hamlet"];

#[derive(Debug, Clone, Copy)]
pub(crate) struct OwnArea {
    area: f64,
    point: Option<[f64; 2]>,
}

struct Covering<'a> {
    covered: Vec<&'a AcceptedBoundary>,
    tuple: AdminContextTuple,
    flags: u16,
    derived_country: Option<RecordId>,
}

fn enclosing(
    mut covered: Vec<&AcceptedBoundary>,
    derived_country: Option<RecordId>,
    larger_than: Option<f64>,
) -> Vec<RecordId> {
    let source = |boundary: &AcceptedBoundary| {
        let object_type = source_type_rank(boundary.source_object_type);
        (object_type, boundary.source_object_id)
    };
    covered.sort_by(|left, right| {
        (left.area.total_cmp(&right.area)).then_with(|| source(left).cmp(&source(right)))
    });
    covered.retain(|boundary| larger_than.is_none_or(|own| boundary.area > own));
    covered
        .iter()
        .map(|boundary| boundary.record_id)
        .chain(derived_country)
        .collect()
}

pub(crate) fn has_area_tags(tags: &BTreeMap<String, String>) -> bool {
    area_parts(tags).is_some()
}

pub(crate) fn relation_member_role(value: &str) -> Option<BoundaryMemberRole> {
    match normalize_for_compare(value).as_str() {
        "" | "outer" => Some(BoundaryMemberRole::Outer),
        "inner" => Some(BoundaryMemberRole::Inner),
        _ => None,
    }
}

pub(crate) fn is_label_role(value: &str) -> bool {
    normalize_for_compare(value) == "label"
}

pub(crate) fn required_boundary_way_ids(relations: &[BoundaryRelationStub]) -> HashSet<i64> {
    relations
        .iter()
        .flat_map(|relation| relation.members.iter().map(|member| member.way_id))
        .collect()
}

fn inferred_country(best: &Best<'_>) -> Option<RecordId> {
    best[PlaceLayer::Region as usize].and_then(|region| region.inferred_country_record_id)
}

fn country_of(best: &Best<'_>) -> Option<RecordId> {
    best[PlaceLayer::Country as usize]
        .map(|country| country.record_id)
        .or_else(|| inferred_country(best))
}

fn derived_country_records(candidates: &[BoundaryCandidate]) -> Vec<(String, PlaceRecord)> {
    let actual_country_codes = candidates
        .iter()
        .filter(|candidate| candidate.boundary.layer == PlaceLayer::Country)
        .filter_map(|candidate| candidate.boundary.inferred_country_code.clone())
        .collect::<HashSet<_>>();

    let mut sources = BTreeMap::new();
    for candidate in candidates {
        if candidate.boundary.layer != PlaceLayer::Region
            || candidate.boundary.admin_level.is_none()
        {
            continue;
        }
        let Some(code) = &candidate.boundary.inferred_country_code else {
            continue;
        };
        if actual_country_codes.contains(code) || sources.contains_key(code) {
            continue;
        }
        sources.insert(
            code.clone(),
            PlaceRecord {
                name: country_name_from_code(code).to_string(),
                alt_names: Vec::new(),
                place_type: format!("derived_country:{code}"),
                geometry: point_geometry(
                    candidate.boundary.representative_point[0],
                    candidate.boundary.representative_point[1],
                ),
                source: SourceProvenance::osm(
                    candidate.source_object_type,
                    candidate.source_object_id,
                ),
                prominence: Prominence::default(),
                wikidata: None,
            },
        );
    }
    sources.into_iter().collect()
}

pub(crate) fn may_be_area(way: &BoundaryWay) -> bool {
    way.vertices.len() >= 4
        && ends(&way.vertices).is_some_and(|(first, last)| first == last)
        && has_area_tags(&way.tags)
}

fn boundary_from_way(way: &BoundaryWay) -> Option<(BuiltBoundary, Vec<u8>)> {
    if !may_be_area(way) {
        return None;
    }
    let parts = area_parts(&way.tags)?;
    let polygon = Polygon::new(ring_line_string(&way.vertices), Vec::new());
    if polygon.unsigned_area() <= f64::EPSILON {
        return None;
    }
    built(parts, &way.tags, &MultiPolygon::new(vec![polygon]), true)
}

fn boundary_from_relation(
    stub: &BoundaryRelationStub,
    member_line: &impl Fn(i64) -> crate::error::Result<Option<Vec<Vertex>>>,
) -> crate::error::Result<Option<(BuiltBoundary, Vec<u8>)>> {
    let Some(parts) = area_parts(&stub.tags) else {
        return Ok(None);
    };
    let Some((geometry, rings_closed)) = member_lines(stub, member_line)?.and_then(rings_geometry)
    else {
        return Ok(None);
    };
    Ok(built(parts, &stub.tags, &geometry, rings_closed))
}

pub(crate) fn place_relations(
    relations: &[BoundaryRelationStub],
    member_line: impl Fn(i64) -> crate::error::Result<Option<Vec<Vertex>>> + Sync,
    entrances: &HashMap<i64, String>,
) -> crate::error::Result<Vec<PoiCandidate>> {
    relations
        .par_iter()
        .filter(|stub| area_parts(&stub.tags).is_none())
        .map(|stub| {
            let Some(lines) = member_lines(stub, &member_line)? else {
                return Ok(None);
            };
            let nodes = lines
                .outer
                .iter()
                .chain(&lines.inner)
                .flatten()
                .map(|vertex| vertex.node_id)
                .collect();
            let mut seen = HashSet::new();
            let entrances = lines
                .outer
                .iter()
                .chain(&lines.inner)
                .flatten()
                .filter(|vertex| seen.insert(vertex.node_id))
                .filter_map(|vertex| {
                    let kind = entrances.get(&vertex.node_id)?;
                    Entrance::of_tag(Some(kind), vertex.lon, vertex.lat)
                })
                .collect();
            let Some((geometry, _)) = rings_geometry(lines) else {
                return Ok(None);
            };
            let Some(point) = geometry.interior_point() else {
                return Ok(None);
            };
            Ok(Some(PoiCandidate {
                object_type: ObjectType::Relation,
                object_id: stub.object_id,
                lat: point.y(),
                lon: point.x(),
                location_precision: LocationPrecision::Centroid,
                tags: stub.tags.clone(),
                outline: Some(outline(&geometry)),
                nodes,
                entrances,
            }))
        })
        .filter_map(crate::error::Result::transpose)
        .collect()
}

struct MemberLines {
    outer: Vec<Vec<Vertex>>,
    inner: Vec<Vec<Vertex>>,
}

fn member_lines(
    stub: &BoundaryRelationStub,
    member_line: &impl Fn(i64) -> crate::error::Result<Option<Vec<Vertex>>>,
) -> crate::error::Result<Option<MemberLines>> {
    let mut outer_segments = Vec::new();
    let mut inner_segments = Vec::new();
    for member in &stub.members {
        let Some(line) = member_line(member.way_id)? else {
            return Ok(None);
        };
        match member.role {
            BoundaryMemberRole::Outer => outer_segments.push(line),
            BoundaryMemberRole::Inner => inner_segments.push(line),
        }
    }

    Ok(Some(MemberLines {
        outer: outer_segments,
        inner: inner_segments,
    }))
}

fn rings_geometry(lines: MemberLines) -> Option<(MultiPolygon<f64>, bool)> {
    let (outer_rings, outer_closed) = stitch_rings(lines.outer);
    if outer_rings.is_empty() {
        return None;
    }
    let (inner_rings, _) = stitch_rings(lines.inner);
    Some((
        multipolygon_from_rings(outer_rings, inner_rings)?,
        outer_closed,
    ))
}

fn built(
    parts: AreaParts,
    tags: &BTreeMap<String, String>,
    geometry: &MultiPolygon<f64>,
    rings_closed: bool,
) -> Option<(BuiltBoundary, Vec<u8>)> {
    let AreaParts {
        layer,
        admin_level,
        place_type,
        name,
        alt_names,
        place_layer,
        designation,
    } = parts;
    let representative_point = representative_point(geometry)?;
    let bounds = geometry.bounding_rect()?;
    let mut block = Vec::new();
    encode_shape(geometry, &mut block);
    let extent = narrowest_extent(&block)?;
    Some((
        BuiltBoundary {
            layer,
            admin_level,
            place_type,
            name,
            alt_names,
            language_names: language_names(tags),
            prominence: prominence(tags),
            wikidata: item(tags, "wikidata"),
            place_layer,
            designation,
            inferred_country_code: country_code_from_tags(tags, layer),
            subdivision_code: subdivision_code_from_tags(tags, layer),
            area_tags: AreaTags::from_osm(tags),
            plate_code: tags.cleaned("license_plate_code"),
            default_language: tags.cleaned("default_language"),
            shape: ShapeRef::default(),
            bounds,
            extent,
            area: geometry.unsigned_area(),
            representative_point,
            interior_point: geometry
                .interior_point()
                .map(|point| [point.x(), point.y()]),
            rings_closed,
            settled: false,
            town: None,
            point_names: Vec::new(),
        },
        block,
    ))
}

fn multipolygon_from_rings(
    outer_rings: Vec<Vec<Vertex>>,
    inner_rings: Vec<Vec<Vertex>>,
) -> Option<MultiPolygon<f64>> {
    let mut outers = Vec::new();
    for ring in outer_rings {
        let polygon = Polygon::new(ring_line_string(&ring), Vec::new());
        if polygon.unsigned_area() > f64::EPSILON {
            outers.push((polygon, Vec::new()));
        }
    }
    if outers.is_empty() {
        return None;
    }

    for ring in inner_rings {
        let interior = ring_line_string(&ring);
        let Some(first) = interior.points().next() else {
            continue;
        };
        if let Some((_, interiors)) = outers.iter_mut().find(|(outer, _)| outer.contains(&first)) {
            interiors.push(interior);
        }
    }

    let polygons = outers
        .into_iter()
        .map(|(outer, interiors)| Polygon::new(outer.exterior().clone(), interiors))
        .collect::<Vec<_>>();
    Some(MultiPolygon::new(polygons))
}

fn ring_line_string(ring: &[Vertex]) -> LineString<f64> {
    LineString::from(
        ring.iter()
            .map(|vertex| (vertex.lon, vertex.lat))
            .collect::<Vec<_>>(),
    )
}

fn stitch_rings(mut segments: Vec<Vec<Vertex>>) -> (Vec<Vec<Vertex>>, bool) {
    segments.retain(|segment| segment.len() >= 2);
    let mut rings = Vec::new();
    let mut closed = true;

    while let Some(mut ring) = segments.pop() {
        loop {
            if ring.len() >= 4 && ends(&ring).is_some_and(|(first, last)| first == last) {
                rings.push(ring);
                break;
            }

            let Some(index) = segments.iter().position(|segment| can_join(&ring, segment)) else {
                closed = false;
                break;
            };
            let segment = segments.swap_remove(index);
            join_segment(&mut ring, segment);
        }
    }

    (rings, closed)
}

fn ends(line: &[Vertex]) -> Option<(i64, i64)> {
    Some((line.first()?.node_id, line.last()?.node_id))
}

fn can_join(ring: &[Vertex], segment: &[Vertex]) -> bool {
    let (Some((ring_first, ring_last)), Some((segment_first, segment_last))) =
        (ends(ring), ends(segment))
    else {
        return false;
    };
    ring_last == segment_first
        || ring_last == segment_last
        || ring_first == segment_last
        || ring_first == segment_first
}

fn join_segment(ring: &mut Vec<Vertex>, mut segment: Vec<Vertex>) {
    let (ring_first, ring_last) = ends(ring).expect("ring has ends");
    let (segment_first, segment_last) = ends(&segment).expect("segment has ends");

    if ring_last == segment_first {
        ring.extend(segment.into_iter().skip(1));
    } else if ring_last == segment_last {
        segment.reverse();
        ring.extend(segment.into_iter().skip(1));
    } else if ring_first == segment_last {
        segment.pop();
        segment.extend(ring.iter().copied());
        *ring = segment;
    } else if ring_first == segment_first {
        segment.reverse();
        segment.pop();
        segment.extend(ring.iter().copied());
        *ring = segment;
    }
}

struct AreaParts {
    layer: PlaceLayer,
    admin_level: Option<u8>,
    place_type: Option<String>,
    name: String,
    alt_names: Vec<String>,
    place_layer: Option<PlaceLayer>,
    designation: Option<String>,
}

fn area_parts(tags: &BTreeMap<String, String>) -> Option<AreaParts> {
    admin_boundary_parts(tags).or_else(|| place_area_parts(tags))
}

fn admin_boundary_parts(tags: &BTreeMap<String, String>) -> Option<AreaParts> {
    if tags.cleaned("boundary").as_deref() != Some("administrative") {
        return None;
    }
    let admin_level = tags.cleaned("admin_level")?.parse::<u8>().ok()?;
    let layer = admin_level_layer(admin_level)?;
    let name = tags.cleaned("name")?;
    let mut alt_names = alternate_names(tags, &name);
    let codes = [
        tags.cleaned("ISO3166-1:alpha3")
            .filter(|_| admin_level <= COUNTRY_ADMIN_LEVEL),
        tags.cleaned("ISO3166-2").and_then(|code| {
            let (_, subdivision) = code.split_once('-')?;
            subdivision
                .chars()
                .all(|character| character.is_ascii_alphabetic())
                .then(|| subdivision.to_string())
        }),
    ];
    for code in codes.into_iter().flatten() {
        if code != name && !alt_names.contains(&code) {
            alt_names.push(code);
        }
    }
    Some(AreaParts {
        layer,
        admin_level: Some(admin_level),
        place_type: None,
        alt_names,
        name,
        place_layer: tags
            .cleaned("place")
            .and_then(|place| place_layer(&place))
            .filter(|layer| *layer != PlaceLayer::Place),
        designation: tags.cleaned("designation"),
    })
}

fn place_area_parts(tags: &BTreeMap<String, String>) -> Option<AreaParts> {
    let place = tags.cleaned("place")?;
    let layer = place_layer(&place)?;
    let name = tags.cleaned("name")?;
    Some(AreaParts {
        layer,
        admin_level: None,
        place_type: Some(place),
        alt_names: alternate_names(tags, &name),
        name,
        place_layer: None,
        designation: None,
    })
}

fn country_code_from_tags(tags: &BTreeMap<String, String>, layer: PlaceLayer) -> Option<String> {
    if layer == PlaceLayer::Country {
        return tags
            .cleaned("ISO3166-1:alpha2")
            .or_else(|| tags.cleaned("country_code"))
            .map(|value| value.to_ascii_uppercase());
    }

    if layer == PlaceLayer::Region {
        let iso = tags.cleaned("ISO3166-2")?;
        let (country, _) = iso.split_once('-')?;
        if country.len() == 2
            && country
                .chars()
                .all(|character| character.is_ascii_alphabetic())
        {
            return Some(country.to_ascii_uppercase());
        }
    }

    None
}

fn subdivision_code_from_tags(
    tags: &BTreeMap<String, String>,
    layer: PlaceLayer,
) -> Option<String> {
    (layer != PlaceLayer::Country)
        .then(|| tags.cleaned("ISO3166-2"))
        .flatten()
        .filter(|code| code.contains('-'))
        .map(|code| code.to_ascii_uppercase())
}

fn country_name_from_code(code: &str) -> &str {
    match code {
        "CA" => "Canada",
        "US" => "United States",
        _ => code,
    }
}

/// The levels that are areas of the Pack at all. Outside it an `admin_level`
/// is a supranational union (1) or past anything OSM ranks (12 and up). A
/// union ranked as an area took the country tier of the country its centre
/// lies in, and pushed that country's every level one tier down: France
/// became a region, its departments localities and its communes
/// neighbourhoods, so its streets were labelled with their region.
const RANKED_ADMIN_LEVELS: std::ops::RangeInclusive<u8> = COUNTRY_ADMIN_LEVEL..=11;

/// The provisional layer for an admin level, before containment is known.
///
/// `admin_level` is not comparable between countries: Australia puts its
/// suburbs and localities at 9 and uses no 8 at all, Germany fills 8, 9 and 10,
/// and a level 6 is a county in most of Poland but a city in Warsaw. Mapping
/// fixed numbers to fixed layers therefore drops whole layers in some
/// countries, so this is only a starting guess that
/// [`assign_layers_by_containment`] corrects once the set is known.
///
/// An off-convention level has no guess to give. It is held at the innermost
/// tier, which is where it would end up if nothing else placed it, and
/// [`assign_layers_by_containment`] moves it to a tier of its own.
fn admin_level_layer(admin_level: u8) -> Option<PlaceLayer> {
    if !RANKED_ADMIN_LEVELS.contains(&admin_level) {
        return None;
    }
    Some(conventional_admin_layer(admin_level).unwrap_or(PlaceLayer::SubNeighbourhood))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Want {
    layer: Option<PlaceLayer>,
    stated: bool,
}

fn anchor_level(levels: &BTreeMap<u8, Want>) -> Option<u8> {
    levels
        .iter()
        .find(|(_, want)| want.layer == Some(PlaceLayer::Locality))
        .or_else(|| {
            levels
                .iter()
                .find(|(_, want)| want.layer.is_none_or(|layer| layer > PlaceLayer::District))
        })
        .map(|(level, _)| *level)
}

fn folded_levels(levels: &BTreeMap<u8, Want>, locality_at: usize) -> BTreeSet<u8> {
    let Some(anchor) = anchor_level(levels) else {
        return BTreeSet::new();
    };
    let above = levels.range(..anchor).count();
    levels
        .range(..anchor)
        .filter(|(_, want)| want.layer.is_none())
        .take(above.saturating_sub(locality_at))
        .map(|(level, _)| *level)
        .collect()
}

const ADMIN_LAYER_ORDER: [PlaceLayer; 9] = PlaceLayer::ADMIN;

const MIN_PLACE_TAGGED_BOUNDARIES: usize = 10;
const PLACE_TAG_AGREEMENT: f64 = 0.8;

fn assign_layers_by_containment(
    candidates: &mut Vec<BoundaryCandidate>,
    shapes: &Shapes,
) -> Vec<String> {
    let (countries, country_ids) = Countries::intern(country_of_each_boundary(candidates, shapes));
    let (verdicts, unmatched) = table_layers(candidates, shapes, &countries, &country_ids);
    let keep = verdicts
        .iter()
        .map(|verdict| !matches!(verdict, Some(None)))
        .collect::<Vec<_>>();
    let mut kept = keep.iter();
    candidates.retain(|_| *kept.next().expect("one verdict per candidate"));
    let country_ids = country_ids
        .into_iter()
        .zip(&keep)
        .filter_map(|(country, keep)| keep.then_some(country))
        .collect::<Vec<_>>();
    let verdicts = verdicts
        .into_iter()
        .filter(|verdict| !matches!(verdict, Some(None)))
        .map(Option::flatten)
        .collect::<Vec<_>>();
    let tagged = tagged_level_layers(candidates, &country_ids);

    let mut wanted: BTreeMap<CountryId, BTreeMap<u8, Want>> = BTreeMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let Some(level) = candidate.boundary.admin_level else {
            continue;
        };
        let country = country_ids[index];
        let stated = verdicts[index].or_else(|| tagged.get(&(country, level)).copied());
        let want = Want {
            layer: stated.or_else(|| conventional_admin_layer(level)),
            stated: stated.is_some(),
        };
        let slot = wanted
            .entry(country)
            .or_default()
            .entry(level)
            .or_insert(want);
        if let (Some(have), Some(layer)) = (slot.layer, want.layer)
            && layer < have
        {
            *slot = want;
        }
    }

    const LOCALITY_AT: usize = 5;
    let mut assigned: BTreeMap<(CountryId, u8), PlaceLayer> = BTreeMap::new();
    for (country, all_levels) in &wanted {
        let folded = folded_levels(all_levels, LOCALITY_AT);
        let levels = &all_levels
            .iter()
            .filter(|(level, _)| !folded.contains(level))
            .map(|(level, want)| (*level, *want))
            .collect::<BTreeMap<_, _>>();
        let anchor = anchor_level(levels);
        let anchor_index = anchor.and_then(|level| levels.keys().position(|at| *at == level));

        let mut next = 0usize;
        for (index, (&level, want)) in levels.iter().enumerate() {
            let innermost = ADMIN_LAYER_ORDER.len() - 1;
            let cap = innermost.saturating_sub(levels.len() - index - 1);
            let at = if Some(index) == anchor_index {
                next.max(LOCALITY_AT.min(cap))
            } else {
                let at = match want.layer {
                    Some(layer) => ADMIN_LAYER_ORDER
                        .iter()
                        .position(|tier| *tier == layer)
                        .unwrap_or(next)
                        .max(next),
                    None => next,
                };
                let contested = levels
                    .values()
                    .skip(index + 1)
                    .filter_map(|want| want.layer)
                    .filter_map(|layer| ADMIN_LAYER_ORDER.iter().position(|tier| *tier == layer))
                    .filter(|wanted| *wanted <= at)
                    .count();
                let at = next.max(at - contested.min(at));
                let at = match anchor_index {
                    Some(anchor) if index < anchor => {
                        at.min(next.max(LOCALITY_AT.saturating_sub(anchor - index)))
                    }
                    _ => at,
                };
                if at > cap { cap.max(next) } else { at }
            }
            .min(innermost);
            assigned.insert((*country, level), ADMIN_LAYER_ORDER[at]);
            next = at + 1;
        }
        for &level in &folded {
            if let Some(layer) = levels
                .range(level..)
                .next()
                .and_then(|(inner, _)| assigned.get(&(*country, *inner)).copied())
            {
                assigned.insert((*country, level), layer);
            }
        }
    }

    for (index, candidate) in candidates.iter_mut().enumerate() {
        let Some(level) = candidate.boundary.admin_level else {
            continue;
        };
        if let Some(layer) = assigned.get(&(country_ids[index], level)) {
            candidate.boundary.layer = *layer;
        }
    }
    unmatched
}

fn table_layers(
    candidates: &[BoundaryCandidate],
    shapes: &Shapes,
    countries: &Countries,
    country_ids: &[CountryId],
) -> (Vec<Option<Option<PlaceLayer>>>, Vec<String>) {
    let table = admin_levels();
    let lowered = countries.lowercased();
    let present = lowered.iter().flatten().cloned().collect::<BTreeSet<_>>();
    let by_object = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            (
                (candidate.source_object_type, candidate.source_object_id),
                index,
            )
        })
        .collect::<HashMap<_, _>>();
    let verdicts = candidates
        .iter()
        .zip(country_ids)
        .map(|(candidate, country)| {
            let country = country.and_then(|id| lowered[usize::from(id)].as_deref());
            let boundary = admin_levels::Boundary {
                object: (candidate.source_object_type, candidate.source_object_id),
                country,
                admin_level: candidate.boundary.admin_level,
                designation: candidate.boundary.designation.as_deref(),
            };
            let point = candidate.boundary.representative_point;
            table.verdict(boundary, |object| {
                by_object
                    .get(&object)
                    .is_some_and(|index| covers_point(shapes, &candidates[*index].boundary, point))
            })
        })
        .collect();
    let mut unmatched = table
        .listed_areas(&present)
        .filter(|object| !by_object.contains_key(object))
        .map(admin_levels::object_key)
        .collect::<Vec<_>>();
    unmatched.sort_unstable();
    unmatched.dedup();
    (verdicts, unmatched)
}

fn tagged_level_layers(
    candidates: &[BoundaryCandidate],
    countries: &[CountryId],
) -> BTreeMap<(CountryId, u8), PlaceLayer> {
    let mut votes: BTreeMap<(CountryId, u8), BTreeMap<PlaceLayer, usize>> = BTreeMap::new();
    for (candidate, country) in candidates.iter().zip(countries) {
        let boundary = &candidate.boundary;
        let Some(level) = boundary
            .admin_level
            .filter(|level| *level > COUNTRY_ADMIN_LEVEL)
        else {
            continue;
        };
        if let Some(layer) = boundary.place_layer {
            *votes
                .entry((*country, level))
                .or_default()
                .entry(layer)
                .or_default() += 1;
        }
    }
    votes
        .into_iter()
        .filter_map(|(key, layers)| {
            let total = layers.values().sum::<usize>();
            let (layer, count) = layers.into_iter().max_by_key(|(_, count)| *count)?;
            (total >= MIN_PLACE_TAGGED_BOUNDARIES
                && count as f64 >= PLACE_TAG_AGREEMENT * total as f64)
                .then_some((key, layer))
        })
        .collect()
}

fn conventional_admin_layer(admin_level: u8) -> Option<PlaceLayer> {
    match admin_level {
        0..=2 => Some(PlaceLayer::Country),
        4 => Some(PlaceLayer::Region),
        6 => Some(PlaceLayer::District),
        8 => Some(PlaceLayer::Locality),
        10 => Some(PlaceLayer::Neighbourhood),
        _ => None,
    }
}

/// `ISO3166-1:alpha2` is only tagged on country boundaries, so every other
/// boundary takes the country code of a country boundary that covers its
/// representative point.
///
/// An extract is often cut inside one country and keeps no usable country
/// polygon at all — a Tasmanian extract has only the fragments of Australia's
/// coastline that fall in the window. Every boundary then resolves to `None`,
/// which is the right answer: the whole extract is treated as one country, so
/// the layers get filled consistently. Falling back to a per-boundary guess
/// here would split the set and let an off-convention level take a layer that
/// a conventional one holds elsewhere in the same country.
fn country_of_each_boundary(
    candidates: &[BoundaryCandidate],
    shapes: &Shapes,
) -> Vec<Option<String>> {
    let country_boundaries = candidates
        .iter()
        .filter(|candidate| {
            candidate
                .boundary
                .admin_level
                .is_some_and(|level| level <= COUNTRY_ADMIN_LEVEL)
        })
        .filter_map(|candidate| {
            candidate
                .boundary
                .inferred_country_code
                .clone()
                .map(|code| (&candidate.boundary, code))
        })
        .collect::<Vec<_>>();

    // A Geofabrik sub-national extract holds no level 2 boundary at all, so
    // there is no country polygon to read a code off. The codes the other
    // boundaries carry (from `ISO3166-2`) are then the only ones there are,
    // and without them every boundary groups as `None` and libpostal's table
    // is skipped for the whole extract -- the country's levels all fall back
    // to the convention, which is wrong wherever the country departs from it.
    let distinct = if country_boundaries.is_empty() {
        candidates
            .iter()
            .filter_map(|candidate| candidate.boundary.inferred_country_code.as_deref())
            .collect::<BTreeSet<_>>()
    } else {
        country_boundaries
            .iter()
            .map(|(_, code)| code.as_str())
            .collect::<BTreeSet<_>>()
    };
    if distinct.len() <= 1 {
        let only = distinct.into_iter().next().map(str::to_string);
        return candidates.iter().map(|_| only.clone()).collect();
    }

    candidates
        .par_iter()
        .map(|candidate| {
            if let Some(code) = &candidate.boundary.inferred_country_code {
                return Some(code.clone());
            }
            let point = candidate.boundary.representative_point;
            country_boundaries
                .iter()
                .find(|(country, _)| covers_point(shapes, country, point))
                .map(|(_, code)| code.clone())
        })
        .collect()
}

#[derive(Debug, Default)]
struct Countries {
    codes: Vec<String>,
}

type CountryId = Option<u16>;

impl Countries {
    fn intern(codes: Vec<Option<String>>) -> (Self, Vec<CountryId>) {
        let mut countries = Self::default();
        let mut ids = Vec::with_capacity(codes.len());
        for code in codes {
            ids.push(code.and_then(|code| countries.id_of(&code)));
        }
        (countries, ids)
    }

    fn id_of(&mut self, code: &str) -> Option<u16> {
        if let Some(at) = self.codes.iter().position(|held| held == code) {
            return u16::try_from(at).ok();
        }
        let at = u16::try_from(self.codes.len()).ok()?;
        self.codes.push(code.to_string());
        Some(at)
    }

    fn lowercased(&self) -> Vec<Option<String>> {
        self.codes
            .iter()
            .map(|code| Some(code.to_ascii_lowercase()))
            .collect()
    }
}

fn boundary_tiers(boundary: &BuiltBoundary) -> TierSet {
    let mut tiers = TierSet::of(boundary.layer);
    // The tag is what the ranking reads to place a level in the first place,
    // so it is a second role only where it disagrees with what the level
    // itself means: Berlin's relation is an admin level 4, a state
    // everywhere else in Germany, tagged `place=city`. A suburb tagged
    // `place=suburb` says nothing its level did not already say, and taking
    // it as a second role would file every one of them under two tiers.
    let Some(conventional) = boundary.admin_level.and_then(conventional_admin_layer) else {
        return tiers;
    };
    if let Some(tagged) = boundary.place_layer
        && tagged != PlaceLayer::Place
        && tagged.record_layer() != conventional.record_layer()
        && tagged.record_layer() != boundary.layer.record_layer()
    {
        tiers.insert(tagged);
    }
    tiers
}

const COUNTRY_ADMIN_LEVEL: u8 = 2;

fn representative_point(geometry: &MultiPolygon<f64>) -> Option<[f64; 2]> {
    geometry.centroid().map(|point| [point.x(), point.y()])
}

fn compare_boundary(
    layer: PlaceLayer,
    left: &AcceptedBoundary,
    right: &AcceptedBoundary,
    source_context: &SourceContext<'_>,
) -> Ordering {
    let source_value = source_value_for_layer(layer, source_context);
    let left_match = source_value
        .map(|value| same_text(value, &left.name))
        .unwrap_or(false);
    let right_match = source_value
        .map(|value| same_text(value, &right.name))
        .unwrap_or(false);

    right_match
        .cmp(&left_match)
        .then_with(|| left.admin_level.is_none().cmp(&right.admin_level.is_none()))
        .then_with(|| {
            left.area
                .partial_cmp(&right.area)
                .unwrap_or(Ordering::Equal)
        })
        .then_with(|| left.admin_level.cmp(&right.admin_level))
        .then_with(|| {
            source_type_rank(left.source_object_type)
                .cmp(&source_type_rank(right.source_object_type))
        })
        .then_with(|| left.source_object_id.cmp(&right.source_object_id))
        .then_with(|| left.name.cmp(&right.name))
}

fn source_value_for_layer<'a>(
    layer: PlaceLayer,
    source_context: &SourceContext<'a>,
) -> Option<&'a str> {
    match layer {
        PlaceLayer::Country => source_context.country,
        PlaceLayer::Region | PlaceLayer::SubRegion => source_context.region,
        PlaceLayer::District | PlaceLayer::SubDistrict => source_context.district,
        PlaceLayer::Locality | PlaceLayer::SubLocality => source_context.locality,
        PlaceLayer::Neighbourhood | PlaceLayer::SubNeighbourhood => source_context.neighbourhood,
        PlaceLayer::Place => source_context.place,
    }
}

fn holds(outer: Rect<f64>, inner: Rect<f64>) -> bool {
    outer.min().x <= inner.min().x
        && outer.min().y <= inner.min().y
        && outer.max().x >= inner.max().x
        && outer.max().y >= inner.max().y
}

fn envelope_from_rect(rect: Rect<f64>) -> AABB<[f64; 2]> {
    AABB::from_corners([rect.min().x, rect.min().y], [rect.max().x, rect.max().y])
}

fn same_text(left: &str, right: &str) -> bool {
    normalize_for_compare(left) == normalize_for_compare(right)
}

fn source_type_rank(object_type: ObjectType) -> u8 {
    match object_type {
        ObjectType::Relation => 0,
        ObjectType::Way => 1,
        ObjectType::Node => 2,
        ObjectType::Row => 3,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PlaceLink {
    Name,
    Item,
    Label,
}

pub(crate) struct PlaceLinks {
    labels: HashMap<(i64, PlaceLayer), usize>,
    items: HashMap<(u32, PlaceLayer), Vec<usize>>,
    names: HashMap<(String, PlaceLayer), Vec<usize>>,
    matched: HashMap<u64, (usize, PlaceLink)>,
    brought: HashMap<(u32, PlaceLayer), Vec<usize>>,
    waiting: Vec<(u64, u32, PlaceLayer, [f64; 2])>,
    strongest: Vec<Option<Strongest>>,
    lookup_bytes: usize,
    brought_list_bytes: usize,
}

#[derive(Debug, Clone, Copy)]
struct Strongest {
    link: PlaceLink,
    point: [f64; 2],
    item: Option<u32>,
}

fn table_bytes<K, V>(map: &HashMap<K, V>) -> usize {
    map.capacity() * 8 / 7 * (std::mem::size_of::<(K, V)>() + 1)
}

impl PlaceLinks {
    pub(crate) fn is_empty(&self) -> bool {
        self.labels.is_empty() && self.items.is_empty() && self.names.is_empty()
    }

    fn measure_lookups(&self) -> usize {
        let word = std::mem::size_of::<usize>();
        table_bytes(&self.labels)
            + table_bytes(&self.items)
            + self
                .items
                .values()
                .map(|list| list.capacity() * word)
                .sum::<usize>()
            + table_bytes(&self.names)
            + self
                .names
                .iter()
                .map(|((name, _), list)| name.capacity() + list.capacity() * word)
                .sum::<usize>()
            + self.strongest.capacity() * std::mem::size_of::<Option<Strongest>>()
    }

    pub(crate) fn heap_bytes(&self) -> usize {
        self.lookup_bytes
            + table_bytes(&self.matched)
            + table_bytes(&self.brought)
            + self.brought_list_bytes
            + self.waiting.capacity() * std::mem::size_of::<(u64, u32, PlaceLayer, [f64; 2])>()
    }

    fn find(
        &self,
        layer: PlaceLayer,
        place: &PlaceRecord,
        holds: impl Fn(usize, [f64; 2]) -> bool,
        covers: impl Fn(usize, [f64; 2]) -> bool,
    ) -> Option<(usize, PlaceLink)> {
        let label = (place.source.object_type == ObjectType::Node)
            .then(|| self.labels.get(&(place.source.object_id, layer)))
            .flatten();
        if let Some(index) = label {
            return Some((*index, PlaceLink::Label));
        }
        let point = point_lon_lat(&place.geometry)?;
        let item = place
            .wikidata
            .and_then(|item| self.items.get(&(item, layer)))
            .and_then(|boundaries| boundaries.iter().find(|&&index| holds(index, point)));
        if let Some(&index) = item {
            return Some((index, PlaceLink::Item));
        }
        self.names
            .get(&(normalize_for_compare(&place.name), layer))?
            .iter()
            .find(|&&index| covers(index, point))
            .map(|&index| (index, PlaceLink::Name))
    }
}

pub(crate) enum SetItem {
    Candidate(Box<BoundaryCandidate>),
    DerivedCountry(String, Box<PlaceRecord>),
    Unmatched(String),
}

impl BoundarySet {
    pub(crate) fn save(
        &self,
        scratch: &std::sync::Arc<crate::extsort::Scratch>,
    ) -> crate::error::Result<crate::extsort::SpoolFile<SetItem>> {
        let mut spool = crate::extsort::Spool::create(scratch, "boundaries")?;
        for candidate in &self.candidates {
            spool.write(&SetItem::Candidate(Box::new(candidate.clone())))?;
        }
        for (code, record) in &self.derived_countries {
            spool.write(&SetItem::DerivedCountry(
                code.clone(),
                Box::new(record.clone()),
            ))?;
        }
        for id in &self.unmatched_admin_ids {
            spool.write(&SetItem::Unmatched(id.clone()))?;
        }
        spool.finish()
    }

    pub(crate) fn load(
        saved: &crate::extsort::SpoolFile<SetItem>,
        shapes: Arc<Shapes>,
    ) -> crate::error::Result<Self> {
        let mut set = Self {
            candidates: Vec::new(),
            derived_countries: Vec::new(),
            unmatched_admin_ids: Vec::new(),
            shapes,
        };
        for item in saved.reader()? {
            match item? {
                SetItem::Candidate(candidate) => set.candidates.push(*candidate),
                SetItem::DerivedCountry(code, record) => {
                    set.derived_countries.push((code, *record))
                }
                SetItem::Unmatched(id) => set.unmatched_admin_ids.push(id),
            }
        }
        let shapes = set
            .candidates
            .iter()
            .map(|candidate| candidate.boundary.shape);
        if !set.shapes.holds(shapes) {
            return Err(crate::error::Error::BadScratch {
                path: set.shapes.path().to_path_buf(),
                detail: "a saved boundary's shape lies past the end of the shape file",
            });
        }
        Ok(set)
    }
}

impl crate::extsort::Spill for SetItem {
    fn encode(&self, out: &mut Vec<u8>) {
        use crate::util::codec::{put_extent, put_i64, put_opt_str, put_str, put_u64};
        match self {
            Self::Candidate(candidate) => {
                out.push(0);
                out.push(object_type_code(candidate.source_object_type));
                put_i64(out, candidate.source_object_id);
                let boundary = &candidate.boundary;
                out.push(boundary.layer as u8);
                out.push(
                    boundary
                        .admin_level
                        .map_or(0, |level| level.saturating_add(1)),
                );
                put_opt_str(out, boundary.place_type.as_deref());
                put_str(out, &boundary.name);
                put_u64(out, boundary.alt_names.len() as u64);
                for name in &boundary.alt_names {
                    put_str(out, name);
                }
                put_u64(out, boundary.language_names.len() as u64);
                for (code, name) in &boundary.language_names {
                    put_str(out, code);
                    put_str(out, name);
                }
                out.push(boundary.prominence.to_byte());
                out.push(boundary.prominence.importance);
                put_u64(out, boundary.wikidata.map_or(0, u64::from));
                out.push(boundary.place_layer.map_or(u8::MAX, |layer| layer as u8));
                put_opt_str(out, boundary.designation.as_deref());
                put_opt_str(out, boundary.inferred_country_code.as_deref());
                put_opt_str(out, boundary.subdivision_code.as_deref());
                boundary.area_tags.put(out);
                put_opt_str(out, boundary.plate_code.as_deref());
                put_opt_str(out, boundary.default_language.as_deref());
                put_u64(out, boundary.shape.offset);
                put_u64(out, boundary.shape.bytes);
                let (min, max) = (boundary.bounds.min(), boundary.bounds.max());
                for value in [min.x, min.y, max.x, max.y, boundary.area]
                    .into_iter()
                    .chain(boundary.representative_point)
                {
                    out.extend_from_slice(&value.to_bits().to_le_bytes());
                }
                put_u64(out, boundary.point_names.len() as u64);
                for name in &boundary.point_names {
                    put_str(out, name);
                }
                out.push(u8::from(boundary.settled));
                out.push(boundary.town.map_or(0, |rank| rank.saturating_add(1)));
                match boundary.interior_point {
                    Some(point) => {
                        out.push(1);
                        for value in point {
                            out.extend_from_slice(&value.to_bits().to_le_bytes());
                        }
                    }
                    None => out.push(0),
                }
                put_extent(out, boundary.extent);
                out.push(u8::from(boundary.rings_closed));
            }
            Self::DerivedCountry(code, record) => {
                out.push(1);
                put_str(out, code);
                super::spill::encode_record(
                    out,
                    &Record::Place(PlaceLayer::Country, (**record).clone()),
                );
            }
            Self::Unmatched(id) => {
                out.push(2);
                put_str(out, id);
            }
        }
    }

    fn decode(input: &mut &[u8]) -> crate::error::Result<Self> {
        use crate::{
            error::Error,
            util::codec::{get_extent, get_i64, get_opt_string, get_string, get_u8, get_u64},
        };
        let layer = |code: u8| {
            PlaceLayer::ALL
                .get(usize::from(code))
                .copied()
                .ok_or(Error::Decode("unknown place layer"))
        };
        Ok(match get_u8(input)? {
            0 => {
                let source_object_type = object_type_of(get_u8(input)?)?;
                let source_object_id = get_i64(input)?;
                let boundary_layer = layer(get_u8(input)?)?;
                let admin_level = get_u8(input)?.checked_sub(1);
                let place_type = get_opt_string(input)?;
                let name = get_string(input)?;
                let alt_names = (0..get_u64(input)?)
                    .map(|_| get_string(input))
                    .collect::<crate::error::Result<Vec<_>>>()?;
                let language_names = (0..get_u64(input)?)
                    .map(|_| Ok((get_string(input)?, get_string(input)?)))
                    .collect::<crate::error::Result<Vec<_>>>()?;
                let prominence = Prominence::from_byte(get_u8(input)?, get_u8(input)?);
                let wikidata = match get_u64(input)? {
                    0 => None,
                    item => Some(
                        u32::try_from(item)
                            .map_err(|_| Error::Decode("Wikidata item is out of range"))?,
                    ),
                };
                let place_layer = match get_u8(input)? {
                    u8::MAX => None,
                    code => Some(layer(code)?),
                };
                let designation = get_opt_string(input)?;
                let inferred_country_code = get_opt_string(input)?;
                let subdivision_code = get_opt_string(input)?;
                let area_tags = AreaTags::get(input)?;
                let plate_code = get_opt_string(input)?;
                let default_language = get_opt_string(input)?;
                let shape = ShapeRef {
                    offset: get_u64(input)?,
                    bytes: get_u64(input)?,
                };
                let mut values = [0.0; 7];
                for value in &mut values {
                    *value = super::checkpoint::get_f64(input)?;
                }
                let [min_x, min_y, max_x, max_y, area, lon, lat] = values;
                let point_names = (0..get_u64(input)?)
                    .map(|_| get_string(input))
                    .collect::<crate::error::Result<Vec<_>>>()?;
                let flag = |input: &mut &[u8], what| match get_u8(input)? {
                    0 => Ok(false),
                    1 => Ok(true),
                    _ => Err(Error::Decode(what)),
                };
                let settled = flag(input, "a saved boundary's settled flag is not 0 or 1")?;
                let town = get_u8(input)?.checked_sub(1);
                let interior_point = if flag(
                    input,
                    "a saved boundary's interior point flag is not 0 or 1",
                )? {
                    Some([
                        super::checkpoint::get_f64(input)?,
                        super::checkpoint::get_f64(input)?,
                    ])
                } else {
                    None
                };
                let extent = get_extent(input)?;
                let rings_closed = flag(input, "a saved boundary's ring flag is not 0 or 1")?;
                Self::Candidate(Box::new(BoundaryCandidate {
                    source_object_type,
                    source_object_id,
                    label: None,
                    boundary: BuiltBoundary {
                        layer: boundary_layer,
                        admin_level,
                        place_type,
                        name,
                        alt_names,
                        language_names,
                        prominence,
                        wikidata,
                        place_layer,
                        designation,
                        inferred_country_code,
                        subdivision_code,
                        area_tags,
                        plate_code,
                        default_language,
                        shape,
                        bounds: Rect::new((min_x, min_y), (max_x, max_y)),
                        extent,
                        area,
                        representative_point: [lon, lat],
                        interior_point,
                        rings_closed,
                        settled,
                        town,
                        point_names,
                    },
                }))
            }
            1 => {
                let code = get_string(input)?;
                match super::spill::decode_record(input)? {
                    Record::Place(_, record) => Self::DerivedCountry(code, Box::new(record)),
                    _ => return Err(Error::Decode("a saved derived country is not a place")),
                }
            }
            2 => Self::Unmatched(get_string(input)?),
            _ => return Err(Error::Decode("unknown saved boundary item")),
        })
    }

    fn resident_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + match self {
                Self::Candidate(candidate) => candidate.heap_bytes(),
                Self::DerivedCountry(code, record) => code.len() + record.name.len() + 256,
                Self::Unmatched(id) => id.len(),
            }
    }
}

fn object_type_code(object_type: ObjectType) -> u8 {
    match object_type {
        ObjectType::Node => 0,
        ObjectType::Way => 1,
        ObjectType::Relation => 2,
        ObjectType::Row => 3,
    }
}

fn object_type_of(code: u8) -> crate::error::Result<ObjectType> {
    Ok(match code {
        0 => ObjectType::Node,
        1 => ObjectType::Way,
        2 => ObjectType::Relation,
        3 => ObjectType::Row,
        _ => return Err(crate::error::Error::Decode("unknown object type")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn dot_with_reach(point: [f64; 2], reach_m: f64) -> TownDot {
        let mut dot = TownDot::new(
            1,
            point,
            "Ashby".to_string(),
            Settlement {
                rank: 2,
                prominence: Prominence::default(),
            },
        );
        dot.reach_m = reach_m;
        dot
    }

    fn reaches(dot: TownDot, [lon, lat]: [f64; 2]) -> bool {
        let mut index = test_index(Vec::new());
        index.add_towns(Vec::new(), vec![dot]);
        index.dots_reaching(lon, lat, &Default::default()).count() == 1
    }

    #[test]
    fn a_town_reaches_a_point_due_north_at_its_reach() {
        assert!(reaches(dot_with_reach([0.0, 0.0], 11_120.0), [0.0, 0.1]));
    }

    #[test]
    fn a_town_near_the_pole_reaches_across_it() {
        assert!(reaches(
            dot_with_reach([0.0, 89.95], 20_000.0),
            [170.0, 89.95]
        ));
    }

    #[test]
    fn a_town_reaches_across_the_antimeridian() {
        assert!(reaches(
            dot_with_reach([179.99, 0.0], 5_000.0),
            [-179.99, 0.0]
        ));
        assert!(reaches(
            dot_with_reach([-179.99, 0.0], 5_000.0),
            [179.99, 0.0]
        ));
    }

    #[test]
    fn exact_polygon_chooses_real_boundary_after_bbox_overlap() {
        let toronto = square_boundary(1, "Toronto", PlaceLayer::Locality, -80.0, 43.0, -79.0, 44.0);
        let oakville = square_boundary(
            2,
            "Oakville",
            PlaceLayer::Locality,
            -80.0,
            42.0,
            -79.0,
            43.0,
        );
        let index = test_index(vec![toronto, oakville]);

        let assignment = index.context_for_point(-79.5, 43.5, &SourceContext::default());

        assert_eq!(assignment.admin_context.locality_record_id(), Some(1));
        assert_eq!(assignment.flags, 0);
    }

    #[test]
    fn shared_edge_sets_ambiguous_flag_and_chooses_stable_boundary() {
        let left = square_boundary(10, "Left", PlaceLayer::Locality, -1.0, -1.0, 0.0, 1.0);
        let right = square_boundary(11, "Right", PlaceLayer::Locality, 0.0, -1.0, 1.0, 1.0);
        let index = test_index(vec![right, left]);

        let assignment = index.context_for_point(0.0, 0.0, &SourceContext::default());

        assert_eq!(assignment.admin_context.locality_record_id(), Some(10));
        assert_eq!(assignment.flags, CONTEXT_FLAG_AMBIGUOUS_ADMIN);
    }

    #[test]
    fn one_tier_many_boundaries_picks_the_smallest_and_flags_it() {
        let big = square_boundary(1, "Big", PlaceLayer::Locality, -2.0, -2.0, 2.0, 2.0);
        let small = square_boundary(2, "Small", PlaceLayer::Locality, -1.0, -1.0, 1.0, 1.0);
        let mid = square_boundary(3, "Mid", PlaceLayer::Locality, -1.5, -1.5, 1.5, 1.5);

        for order in [vec![big, small, mid], {
            let big = square_boundary(1, "Big", PlaceLayer::Locality, -2.0, -2.0, 2.0, 2.0);
            let small = square_boundary(2, "Small", PlaceLayer::Locality, -1.0, -1.0, 1.0, 1.0);
            let mid = square_boundary(3, "Mid", PlaceLayer::Locality, -1.5, -1.5, 1.5, 1.5);
            vec![mid, small, big]
        }] {
            let index = test_index(order);
            let assignment = index.context_for_point(0.0, 0.0, &SourceContext::default());
            assert_eq!(
                assignment.admin_context.locality_record_id(),
                Some(2),
                "the smallest covering boundary, whatever the order"
            );
            assert_eq!(assignment.flags, CONTEXT_FLAG_AMBIGUOUS_ADMIN);
        }
    }

    #[test]
    fn a_dual_role_boundary_carries_both_tiers_to_a_point() {
        let germany = square_boundary(1, "Deutschland", PlaceLayer::Country, -9.0, -9.0, 9.0, 9.0);
        let mut berlin = square_boundary(2, "Berlin", PlaceLayer::Region, -1.0, -1.0, 1.0, 1.0);
        berlin.tiers = [PlaceLayer::Region, PlaceLayer::Locality]
            .into_iter()
            .collect();
        let index = test_index(vec![germany, berlin]);

        let assignment = index.context_for_point(0.0, 0.0, &SourceContext::default());

        let tuple = &assignment.admin_context;
        assert_eq!(tuple.get(PlaceLayer::Region), Some(2), "Berlin the state");
        assert_eq!(tuple.get(PlaceLayer::Locality), Some(2), "Berlin the city");
        assert_eq!(tuple.get(PlaceLayer::Country), Some(1));
        assert_eq!(tuple.entries().len(), 2, "one entry for Berlin: {tuple:?}");
    }

    #[test]
    fn only_a_tag_that_contradicts_the_level_is_a_second_tier() {
        let mut berlin = test_boundary(PlaceLayer::Region);
        berlin.admin_level = Some(4);
        berlin.place_layer = Some(PlaceLayer::Locality);
        assert_eq!(
            boundary_tiers(&berlin).iter().collect::<Vec<_>>(),
            [PlaceLayer::Region, PlaceLayer::Locality],
            "a state that is also a city"
        );

        let mut suburb = test_boundary(PlaceLayer::SubLocality);
        suburb.admin_level = Some(10);
        suburb.place_layer = Some(PlaceLayer::Neighbourhood);
        assert_eq!(
            boundary_tiers(&suburb).iter().collect::<Vec<_>>(),
            [PlaceLayer::SubLocality],
            "a suburb is one thing"
        );

        let mut area = test_boundary(PlaceLayer::Place);
        area.admin_level = None;
        area.place_layer = Some(PlaceLayer::Place);
        assert_eq!(
            boundary_tiers(&area).iter().collect::<Vec<_>>(),
            [PlaceLayer::Place],
            "an island is not also a city"
        );
    }

    #[test]
    fn a_saved_boundary_keeps_whether_its_rings_closed() {
        use crate::extsort::Spill;
        for rings_closed in [false, true] {
            let mut boundary = test_boundary(PlaceLayer::Region);
            boundary.rings_closed = rings_closed;
            boundary.settled = !rings_closed;
            boundary.point_names = vec!["Emerald City".to_string()];
            boundary.interior_point = rings_closed.then_some([0.5, -0.25]);
            let interior_point = boundary.interior_point;
            let mut bytes = Vec::new();
            SetItem::Candidate(Box::new(BoundaryCandidate {
                source_object_type: ObjectType::Relation,
                source_object_id: 60189,
                label: None,
                boundary,
            }))
            .encode(&mut bytes);
            let SetItem::Candidate(read) = SetItem::decode(&mut bytes.as_slice()).expect("decode")
            else {
                panic!("not a candidate");
            };
            assert_eq!(read.boundary.rings_closed, rings_closed);
            assert_eq!(read.boundary.settled, !rings_closed);
            assert_eq!(read.boundary.point_names, ["Emerald City"]);
            assert_eq!(read.boundary.interior_point, interior_point);
            assert_eq!(
                read.boundary.extent,
                [-10_000_000, -10_000_000, 10_000_000, 10_000_000]
            );
        }
        let mut bytes = Vec::new();
        SetItem::Candidate(Box::new(BoundaryCandidate {
            source_object_type: ObjectType::Relation,
            source_object_id: 60189,
            label: None,
            boundary: test_boundary(PlaceLayer::Region),
        }))
        .encode(&mut bytes);
        *bytes.last_mut().expect("flag") = 2;
        assert_eq!(
            SetItem::decode(&mut bytes.as_slice())
                .err()
                .map(|error| error.to_string()),
            Some(
                crate::error::Error::Decode("a saved boundary's ring flag is not 0 or 1")
                    .to_string()
            )
        );
    }

    #[test]
    fn a_merged_points_names_are_the_records_not_the_areas() {
        let (_dir, scratch) = test_scratch();
        let mut builder = BoundaryBuilder::new(&scratch).expect("builder");
        let vertex = |node_id, lon, lat| Vertex { node_id, lat, lon };
        let ring = vec![
            vertex(1, 0.0, 0.0),
            vertex(2, 1.0, 0.0),
            vertex(3, 1.0, 1.0),
            vertex(4, 0.0, 1.0),
            vertex(1, 0.0, 0.0),
        ];
        builder
            .add_relations(
                &[BoundaryRelationStub {
                    object_id: 900,
                    members: vec![BoundaryRelationMember {
                        way_id: 500,
                        role: BoundaryMemberRole::Outer,
                    }],
                    label: Some(10),
                    tags: boundary_tags(&BTreeMap::from([
                        ("boundary".to_string(), "administrative".to_string()),
                        ("admin_level".to_string(), "8".to_string()),
                        ("name".to_string(), "Seattle".to_string()),
                    ])),
                }],
                |_| Ok(Some(ring.clone())),
            )
            .expect("relations");
        let mut set = builder.finish().expect("boundaries");
        let mut links = set.place_links();
        let point = PlaceRecord {
            name: "Seattle".to_string(),
            alt_names: vec!["Emerald City".to_string()],
            place_type: "city".to_string(),
            geometry: point_geometry(0.3, 0.2),
            source: SourceProvenance::osm(ObjectType::Node, 10),
            prominence: Prominence::default(),
            wikidata: None,
        };
        set.match_place_point(&mut links, 0, PlaceLayer::Locality, &point);
        set.group_place_points(&mut links);
        assert!(set.merge_place_point(&mut links, 0, &point));
        set.finish_place_merges(links);

        let places = set.place_records(&mut BuilderReport::default());
        let Record::Place(_, seattle) = &places[0].1 else {
            panic!("a boundary is a place");
        };
        assert_eq!(seattle.alt_names, ["Emerald City"]);
        assert_eq!(
            set.context_names(&[7], &HashMap::new()),
            HashMap::from([(7, vec!["Seattle".to_string()])])
        );
    }

    fn test_boundary(layer: PlaceLayer) -> BuiltBoundary {
        let ring = LineString::from(vec![
            (-1.0, -1.0),
            (1.0, -1.0),
            (1.0, 1.0),
            (-1.0, 1.0),
            (-1.0, -1.0),
        ]);
        let geometry = MultiPolygon::new(vec![Polygon::new(ring, Vec::new())]);
        BuiltBoundary {
            layer,
            admin_level: Some(4),
            place_type: None,
            name: "Test".to_string(),
            alt_names: Vec::new(),
            language_names: Vec::new(),
            place_layer: None,
            designation: None,
            inferred_country_code: None,
            subdivision_code: None,
            area_tags: AreaTags::default(),
            plate_code: None,
            default_language: None,
            representative_point: representative_point(&geometry).expect("point"),
            interior_point: None,
            shape: ShapeRef::default(),
            bounds: geometry.bounding_rect().expect("bounds"),
            extent: [-10_000_000, -10_000_000, 10_000_000, 10_000_000],
            area: geometry.unsigned_area(),
            prominence: Default::default(),
            wikidata: None,
            rings_closed: true,
            settled: false,
            town: None,
            point_names: Vec::new(),
        }
    }

    #[test]
    fn a_point_carries_every_tier_it_lies_in() {
        let district = square_boundary(
            1,
            "Canberra Central",
            PlaceLayer::Locality,
            -1.0,
            -1.0,
            1.0,
            1.0,
        );
        let suburb = square_boundary(2, "Dickson", PlaceLayer::SubLocality, -0.5, -0.5, 0.5, 0.5);
        let index = test_index(vec![district, suburb]);

        let assignment = index.context_for_point(0.0, 0.0, &SourceContext::default());

        let tuple = &assignment.admin_context;
        assert_eq!(tuple.get(PlaceLayer::Locality), Some(1), "the district");
        assert_eq!(tuple.get(PlaceLayer::SubLocality), Some(2), "the suburb");
        assert_eq!(tuple.entries().len(), 2, "both, not one: {tuple:?}");
    }

    #[test]
    fn a_union_of_countries_is_no_area() {
        let tags = |level: &str| {
            [
                ("boundary", "administrative"),
                ("admin_level", level),
                ("name", "European Union"),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect::<BTreeMap<_, _>>()
        };
        assert!(area_parts(&tags("1")).is_none());
        assert!(area_parts(&tags("0")).is_none());
        assert_eq!(
            area_parts(&tags("2")).map(|parts| parts.layer),
            Some(PlaceLayer::Country)
        );
    }

    #[test]
    fn an_off_convention_level_fills_the_layer_its_country_leaves_empty() {
        let layers = layers_for_levels(&[2, 4, 6, 9]);
        assert_eq!(layers[&9], PlaceLayer::Locality, "level 9 fills locality");
        assert_eq!(layers[&4], PlaceLayer::Region, "conventional levels keep");
        assert_eq!(layers[&6], PlaceLayer::District);
    }

    #[test]
    fn two_spare_levels_take_two_layers() {
        let layers = layers_for_levels(&[2, 4, 7, 9]);
        assert_eq!(layers[&4], PlaceLayer::Region);
        assert_eq!(layers[&7], PlaceLayer::Locality, "7 takes the first free");
        assert_eq!(layers[&9], PlaceLayer::Neighbourhood, "9 takes the next");
        assert_ne!(layers[&7], layers[&9], "two levels, two layers");
    }

    #[test]
    fn an_off_convention_level_never_displaces_a_conventional_one() {
        let layers = layers_for_levels(&[2, 4, 8, 9, 10]);
        assert_eq!(layers[&8], PlaceLayer::Locality, "8 keeps locality");
        assert_eq!(layers[&10], PlaceLayer::Neighbourhood, "10 keeps its layer");
        assert_ne!(layers[&9], PlaceLayer::Locality, "9 cannot take locality");
    }

    #[test]
    fn a_conventional_hierarchy_is_left_alone() {
        let layers = layers_for_levels(&[2, 4, 6, 8, 10]);
        assert_eq!(layers[&2], PlaceLayer::Country);
        assert_eq!(layers[&4], PlaceLayer::Region);
        assert_eq!(layers[&6], PlaceLayer::District);
        assert_eq!(layers[&8], PlaceLayer::Locality);
        assert_eq!(layers[&10], PlaceLayer::Neighbourhood);
    }

    #[test]
    fn a_level_whose_place_tags_agree_takes_their_layer() {
        let layers = layers_for_tagged_levels(&[
            (2, None, 1),
            (4, Some(PlaceLayer::Region), 47),
            (6, None, 300),
            (7, Some(PlaceLayer::Locality), 20),
            (7, None, 100),
            (8, Some(PlaceLayer::Neighbourhood), 10),
            (8, None, 100),
            (10, None, 100),
        ]);
        assert_eq!(layers[&4], PlaceLayer::Region);
        assert_eq!(layers[&6], PlaceLayer::District);
        assert_eq!(layers[&7], PlaceLayer::Locality, "tagged cities");
        assert_eq!(layers[&8], PlaceLayer::SubLocality, "tagged wards");
        assert_eq!(layers[&10], PlaceLayer::Neighbourhood);
        assert_ne!(layers[&8], layers[&10], "one tier each");
    }

    #[test]
    fn scarce_or_split_place_tags_leave_a_level_to_its_number() {
        let layers = layers_for_tagged_levels(&[
            (2, None, 1),
            (4, None, 1),
            (8, Some(PlaceLayer::Neighbourhood), 9),
            (10, Some(PlaceLayer::Locality), 10),
            (10, Some(PlaceLayer::Neighbourhood), 10),
        ]);
        assert_eq!(layers[&8], PlaceLayer::Locality, "nine tags are too few");
        assert_eq!(layers[&10], PlaceLayer::Neighbourhood, "split tags");
    }

    #[test]
    fn every_country_in_the_table_fills_locality() {
        let mut empty = Vec::new();
        for (country, levels) in admin_levels().levels_by_country() {
            if levels.len() < 2 || !admin_levels().names_a_locality(&country) {
                continue;
            }
            let layers = layers_for_levels_in(Some(&country), &levels);
            if !layers.values().any(|layer| *layer == PlaceLayer::Locality) {
                empty.push(format!("{country} {levels:?} -> {layers:?}"));
            }
        }
        assert!(
            empty.is_empty(),
            "{} of the table's countries fill no locality:\n  {}",
            empty.len(),
            empty.join("\n  ")
        );
    }

    #[test]
    fn every_country_fills_locality_without_a_country_boundary() {
        let mut empty = Vec::new();
        for (country, levels) in admin_levels().levels_by_country() {
            let levels = levels
                .into_iter()
                .filter(|level| *level > COUNTRY_ADMIN_LEVEL)
                .collect::<Vec<_>>();
            if levels.len() < 2 || !admin_levels().names_a_locality(&country) {
                continue;
            }
            let layers = layers_for_levels_in(Some(&country), &levels);
            if !layers.values().any(|layer| *layer == PlaceLayer::Locality) {
                empty.push(format!("{country} {levels:?} -> {layers:?}"));
            }
        }
        assert!(
            empty.is_empty(),
            "{} countries fill no locality without a country boundary:\n  {}",
            empty.len(),
            empty.join("\n  ")
        );
    }

    #[test]
    fn a_deep_country_keeps_every_level() {
        let layers = layers_for_levels_in(Some("de"), &[2, 4, 5, 6, 7, 8, 9, 10, 11]);
        assert_eq!(layers.len(), 9, "nine levels");
        let tiers = layers.values().collect::<BTreeSet<_>>();
        assert_eq!(tiers.len(), 9, "nine tiers, nothing dropped: {layers:?}");
        assert_eq!(layers[&8], PlaceLayer::Locality, "the city stays the city");
        assert_eq!(layers[&2], PlaceLayer::Country);
        assert_eq!(layers[&4], PlaceLayer::Region);
    }

    #[test]
    fn a_country_with_unnamed_outer_levels_keeps_its_town_the_locality() {
        use PlaceLayer::*;

        assert_eq!(
            layers_for_levels_in(Some("fr"), &[2, 3, 4, 5, 6, 7, 8, 9, 10]),
            BTreeMap::from([
                (2, Country),
                (3, Region),
                (4, Region),
                (5, SubRegion),
                (6, District),
                (7, SubDistrict),
                (8, Locality),
                (9, SubLocality),
                (10, Neighbourhood),
            ])
        );
    }

    #[test]
    fn keeps_the_subdivision_code_of_a_province_ranked_below_the_region_tier() {
        let tags = BTreeMap::from([("ISO3166-2".to_string(), "ca-on".to_string())]);
        assert_eq!(
            subdivision_code_from_tags(&tags, PlaceLayer::SubRegion).as_deref(),
            Some("CA-ON")
        );
        assert_eq!(subdivision_code_from_tags(&tags, PlaceLayer::Country), None);
    }

    #[test]
    fn a_country_deeper_than_the_tiers_ends_on_the_innermost() {
        let levels = [2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
        let layers = layers_for_levels_in(Some("ca"), &levels);
        assert_eq!(layers.len(), levels.len(), "every level keeps a tier");
        assert_eq!(layers[&2], PlaceLayer::Country);
        assert_eq!(
            (layers[&3], layers[&4]),
            (PlaceLayer::Region, PlaceLayer::Region)
        );
        assert_eq!(layers[&8], PlaceLayer::Locality);
        let innermost = *ADMIN_LAYER_ORDER.last().expect("tiers");
        assert_eq!(
            (layers[&10], layers[&11]),
            (PlaceLayer::Neighbourhood, innermost)
        );
        let tiers = levels.iter().map(|level| layers[level]).collect::<Vec<_>>();
        assert!(
            tiers.windows(2).all(|pair| pair[0] <= pair[1]),
            "never back up: {tiers:?}"
        );
    }

    #[test]
    fn no_country_drops_a_level() {
        let mut dropped = Vec::new();
        for (country, levels) in admin_levels().levels_by_country() {
            let layers = layers_for_levels_in(Some(&country), &levels);
            let tiers = layers.values().collect::<BTreeSet<_>>();
            if tiers.len() != layers.len() {
                dropped.push(format!("{country} {levels:?} -> {layers:?}"));
            }
        }
        assert!(
            dropped.is_empty(),
            "{} countries drop a level:\n  {}",
            dropped.len(),
            dropped.join("\n  ")
        );
    }

    fn layers_for_levels(levels: &[u8]) -> BTreeMap<u8, PlaceLayer> {
        layers_for_tagged_levels(
            &levels
                .iter()
                .map(|level| (*level, None, 1))
                .collect::<Vec<_>>(),
        )
    }

    fn layers_for_levels_in(country: Option<&str>, levels: &[u8]) -> BTreeMap<u8, PlaceLayer> {
        let tagged = levels
            .iter()
            .map(|level| (*level, None, 1))
            .collect::<Vec<_>>();
        layers_for_tagged_levels_in(country, &tagged)
    }

    fn layers_for_tagged_levels(
        levels: &[(u8, Option<PlaceLayer>, usize)],
    ) -> BTreeMap<u8, PlaceLayer> {
        layers_for_tagged_levels_in(None, levels)
    }

    fn layers_for_tagged_levels_in(
        country: Option<&str>,
        levels: &[(u8, Option<PlaceLayer>, usize)],
    ) -> BTreeMap<u8, PlaceLayer> {
        let (_dir, scratch) = test_scratch();
        let mut shapes = ShapeWriter::create(&scratch, SHAPES_LABEL).expect("shape file");
        let mut candidates = Vec::new();
        let levels = levels
            .iter()
            .flat_map(|(level, place_layer, count)| {
                std::iter::repeat_n((*level, *place_layer), *count)
            })
            .collect::<Vec<_>>();
        for (index, &(level, place_layer)) in levels.iter().enumerate() {
            let span = 10.0 / (index + 1) as f64;
            let ring = LineString::from(vec![
                (-span, -span),
                (span, -span),
                (span, span),
                (-span, span),
                (-span, -span),
            ]);
            let geometry = MultiPolygon::new(vec![Polygon::new(ring, Vec::new())]);
            let (shape, bounds, area) = with_shape(&mut shapes, &geometry);
            candidates.push(BoundaryCandidate {
                source_object_type: ObjectType::Relation,
                source_object_id: index as i64 + 1,
                label: None,
                boundary: BuiltBoundary {
                    layer: admin_level_layer(level).expect("layer"),
                    admin_level: Some(level),
                    place_type: None,
                    name: format!("level {level}"),
                    alt_names: Vec::new(),
                    language_names: Vec::new(),
                    place_layer,
                    designation: None,
                    inferred_country_code: country
                        .map(str::to_ascii_uppercase)
                        .or_else(|| (level <= 2).then(|| "AU".to_string())),
                    subdivision_code: None,
                    area_tags: AreaTags::default(),
                    plate_code: None,
                    default_language: None,
                    representative_point: representative_point(&geometry).expect("point"),
                    interior_point: None,
                    shape,
                    bounds,
                    extent: [
                        bounds.min().x,
                        bounds.min().y,
                        bounds.max().x,
                        bounds.max().y,
                    ]
                    .map(|side| (side * 1e7) as i32),
                    area,
                    prominence: Default::default(),
                    wikidata: None,
                    rings_closed: true,
                    settled: false,
                    town: None,
                    point_names: Vec::new(),
                },
            });
        }
        let shapes = shapes.finish().expect("shapes");
        assign_layers_by_containment(&mut candidates, &shapes);
        let mut layers = BTreeMap::new();
        for candidate in &candidates {
            let layer = *layers
                .entry(candidate.boundary.admin_level.expect("admin"))
                .or_insert(candidate.boundary.layer);
            assert_eq!(layer, candidate.boundary.layer, "one layer per level");
        }
        layers
    }

    #[test]
    fn region_boundary_can_infer_country_from_iso3166_2() {
        let mut region =
            square_boundary(20, "Ontario", PlaceLayer::Region, -90.0, 40.0, -70.0, 50.0);
        region.inferred_country_record_id = Some(7);
        let index = test_index(vec![region]);

        let assignment = index.context_for_point(-79.0, 43.0, &SourceContext::default());

        assert_eq!(assignment.admin_context.country_record_id(), Some(7));
        assert_eq!(assignment.admin_context.region_record_id(), Some(20));
    }

    #[test]
    fn a_region_lies_in_the_larger_regions_covering_it() {
        let square = |object_id, level: &str, name: &str, iso: Option<&str>, size: f64| {
            let vertex = |node_id, lon, lat| Vertex { node_id, lat, lon };
            let base = object_id * 10;
            let mut tags = BTreeMap::from([
                ("boundary".to_string(), "administrative".to_string()),
                ("admin_level".to_string(), level.to_string()),
                ("name".to_string(), name.to_string()),
            ]);
            if let Some(iso) = iso {
                tags.insert("ISO3166-1:alpha2".to_string(), iso.to_string());
            }
            BoundaryWay {
                object_id,
                tags,
                vertices: vec![
                    vertex(base, 140.0, 35.0),
                    vertex(base + 1, 140.0 + size, 35.0),
                    vertex(base + 2, 140.0 + size, 35.0 + size),
                    vertex(base + 3, 140.0, 35.0 + size),
                    vertex(base, 140.0, 35.0),
                ],
            }
        };
        let ways = [
            square(1, "2", "日本", Some("JP"), 8.0),
            square(2, "3", "関東地方", None, 4.0),
            square(3, "4", "千葉県", None, 1.0),
        ];
        let (_dir, scratch) = test_scratch();
        let mut builder = BoundaryBuilder::new(&scratch).expect("builder");
        builder.add_ways(&ways).expect("ways");
        let set = builder.finish().expect("boundaries");
        let groups = set.region_groups(&[100, 101, 102]);
        assert_eq!(groups, HashMap::from([(102, vec![101])]));
    }

    #[test]
    fn builds_relation_polygons_from_stitched_member_ways_and_derives_countries() {
        let vertex = |node_id, lon, lat| Vertex { node_id, lat, lon };
        let member_lines = HashMap::from([
            (
                1,
                Some(vec![
                    vertex(10, -80.0, 40.0),
                    vertex(11, -70.0, 40.0),
                    vertex(12, -70.0, 50.0),
                ]),
            ),
            (
                2,
                Some(vec![
                    vertex(10, -80.0, 40.0),
                    vertex(13, -80.0, 50.0),
                    vertex(12, -70.0, 50.0),
                ]),
            ),
            (3, None),
        ]);
        let tags = |name: &str, iso: &str| {
            boundary_tags(&BTreeMap::from([
                ("boundary".to_string(), "administrative".to_string()),
                ("admin_level".to_string(), "4".to_string()),
                ("name".to_string(), name.to_string()),
                ("name:fr".to_string(), name.to_string()),
                ("ISO3166-2".to_string(), iso.to_string()),
                ("wikidata".to_string(), "Q1904".to_string()),
                ("wikipedia".to_string(), format!("en:{name}")),
                ("source".to_string(), "survey".to_string()),
            ]))
        };
        let members = |ids: &[i64]| {
            ids.iter()
                .map(|way_id| BoundaryRelationMember {
                    way_id: *way_id,
                    role: BoundaryMemberRole::Outer,
                })
                .collect()
        };
        let relations = [
            BoundaryRelationStub {
                object_id: 100,
                members: members(&[1, 2]),
                label: None,
                tags: tags("Ontario", "CA-ON"),
            },
            BoundaryRelationStub {
                object_id: 101,
                members: members(&[1, 3]),
                label: None,
                tags: tags("Broken", "CA-QC"),
            },
        ];

        let member_line = |way_id: i64| Ok(member_lines.get(&way_id).cloned().flatten());
        let (_dir, scratch) = test_scratch();
        let mut builder = BoundaryBuilder::new(&scratch).expect("builder");
        builder
            .add_relations(&relations, member_line)
            .expect("relations");
        let set = builder.finish().expect("boundaries");
        assert_eq!(
            set.len(),
            1,
            "a relation with an unresolved member is skipped"
        );
        let mut report = BuilderReport::default();
        let places = set.place_records(&mut report);
        assert_eq!(places.len(), 2);
        assert_eq!(places[0].0, ContextOrigin::DerivedCountry("CA".into()));
        assert_eq!(places[0].1.label(), "Canada");
        assert_eq!(places[1].0, ContextOrigin::Boundary(0));
        assert_eq!(places[1].1.id(), "osm:relation:100");
        let Record::Place(_, ontario) = &places[1].1 else {
            panic!("a boundary is a place");
        };
        assert_eq!(ontario.wikidata, Some(1904));
        assert_eq!(
            ontario.prominence,
            Prominence {
                importance: 0,
                linked: true,
                languages: 1,
            }
        );
        assert_eq!(report.accepted.place_nodes, 2);

        let index = set.into_index(&[5], &HashMap::from([("CA".to_string(), 4)]));
        let context = index.context_for_point(-75.0, 45.0, &SourceContext::default());
        assert_eq!(context.admin_context.region_record_id(), Some(5));
        assert_eq!(context.admin_context.country_record_id(), Some(4));
    }

    fn square_boundary(
        record_id: RecordId,
        name: &str,
        layer: PlaceLayer,
        min_x: f64,
        min_y: f64,
        max_x: f64,
        max_y: f64,
    ) -> AcceptedBoundary {
        let polygon = Polygon::new(
            LineString::from(vec![
                (min_x, min_y),
                (max_x, min_y),
                (max_x, max_y),
                (min_x, max_y),
                (min_x, min_y),
            ]),
            Vec::new(),
        );
        let geometry = MultiPolygon::new(vec![polygon]);
        AcceptedBoundary {
            record_id,
            layer,
            tiers: TierSet::of(layer),
            name: name.to_string(),
            admin_level: Some(8),
            inferred_country_record_id: None,
            source_object_type: ObjectType::Relation,
            source_object_id: record_id as i64,
            area: geometry.unsigned_area(),
            interior_point: None,
            town: None,
            bounds: geometry.bounding_rect().expect("bounds"),
            shape: ShapeRef::default(),
        }
    }

    fn test_scratch() -> (TempDir, Arc<crate::extsort::Scratch>) {
        let dir = TempDir::new("boundary");
        let scratch = crate::extsort::Scratch::create(dir.join("scratch")).expect("scratch");
        (dir, scratch)
    }

    fn with_shape(
        shapes: &mut ShapeWriter,
        geometry: &MultiPolygon<f64>,
    ) -> (ShapeRef, Rect<f64>, f64) {
        let mut block = Vec::new();
        encode_shape(geometry, &mut block);
        (
            shapes.write(&block).expect("shape"),
            geometry.bounding_rect().expect("bounds"),
            geometry.unsigned_area(),
        )
    }

    fn test_index(mut boundaries: Vec<AcceptedBoundary>) -> BoundaryIndex {
        let (_dir, scratch) = test_scratch();
        let mut shapes = ShapeWriter::create(&scratch, SHAPES_LABEL).expect("shape file");
        for boundary in &mut boundaries {
            let square = MultiPolygon::new(vec![boundary.bounds.to_polygon()]);
            boundary.shape = with_shape(&mut shapes, &square).0;
        }
        BoundaryIndex::new(boundaries, Arc::new(shapes.finish().expect("shapes")))
    }
}
