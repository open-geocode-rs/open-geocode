use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
};

use geo::{Area, BoundingRect, Centroid, Contains, LineString, MultiPolygon, Point, Polygon, Rect};
use rayon::prelude::*;
use rstar::{AABB, RTree, RTreeObject};

use crate::{
    builder::report::BuilderReport,
    context::{AdminContextTuple, CONTEXT_FLAG_AMBIGUOUS_ADMIN, RecordContext},
    pack::RecordId,
    parse::lattice::CountryCode,
    record::{
        Layer, OsmObjectType, PlaceLayer, PlaceRecord, Record, SourceProvenance, point_geometry,
    },
    util::text::normalize_for_compare,
};

use super::{
    admin_levels::{self, admin_levels},
    containment::AreaShape,
    place::place_layer,
    tags::{OsmTags, alternate_names, name_tags},
};

/// Tags a boundary keeps after the scan besides its names.
const BOUNDARY_TAG_KEYS: [&str; 8] = [
    "ISO3166-1:alpha2",
    "ISO3166-1:alpha3",
    "ISO3166-2",
    "admin_level",
    "boundary",
    "country_code",
    "designation",
    "place",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundaryRelationStub {
    pub object_id: i64,
    pub members: Vec<BoundaryRelationMember>,
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

/// A resolved vertex of a boundary member way. Rings are stitched on node ids.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Vertex {
    pub node_id: i64,
    pub lat: f64,
    pub lon: f64,
}

/// A boundary-tagged way whose vertices all resolved.
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
    name: String,
    /// `None` for a place drawn as an area.
    admin_level: Option<u8>,
    inferred_country_record_id: Option<RecordId>,
    source_object_type: OsmObjectType,
    source_object_id: i64,
    bounds: Rect<f64>,
    shape: AreaShape,
    area: f64,
}

#[derive(Debug, Clone)]
struct BoundaryBox {
    envelope: AABB<[f64; 2]>,
    boundary_id: usize,
}

/// An area records lie in: an admin boundary, or a place drawn as a closed
/// way or a multipolygon (`place=island`, `place=suburb`).
struct BuiltBoundary {
    layer: PlaceLayer,
    /// `None` for a place area, which takes its layer from its `place` tag
    /// and stays out of the admin level ranking.
    admin_level: Option<u8>,
    /// The `place` tag of a place area, its record's place type.
    place_type: Option<String>,
    name: String,
    alt_names: Vec<String>,
    /// The layer its `place` tag names, if any (`place=city` is a locality).
    place_layer: Option<PlaceLayer>,
    /// Its `designation` tag (`unitary_authority`), which libpostal's table
    /// reads in some countries.
    designation: Option<String>,
    inferred_country_code: Option<String>,
    geometry: MultiPolygon<f64>,
    representative_point: [f64; 2],
}

struct BoundaryCandidate {
    source_object_type: OsmObjectType,
    source_object_id: i64,
    boundary: BuiltBoundary,
}

/// Where a context record came from, so its final record id can be wired into
/// the boundary index after records are sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContextOrigin {
    Boundary(u32),
    DerivedCountry(String),
}

/// Admin boundaries built from the input, before record ids are known.
pub(crate) struct BoundarySet {
    candidates: Vec<BoundaryCandidate>,
    derived_countries: Vec<(String, PlaceRecord)>,
    /// The ways and relations libpostal's admin table names for the
    /// countries of the input that the input does not have.
    unmatched_admin_ids: Vec<String>,
}

impl BoundarySet {
    /// The objects libpostal's admin table names that the input lacks: a
    /// rule for an id OSM no longer has applies to nothing.
    pub(crate) fn unmatched_admin_ids(&self) -> &[String] {
        &self.unmatched_admin_ids
    }

    pub(crate) fn len(&self) -> usize {
        self.candidates.len()
    }

    /// Place records for every boundary and derived country.
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
                        alt_names: boundary.alt_names.clone(),
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
                    },
                ),
            ));
        }
        records
    }

    /// Names of the records admin context tuples can point at, by record id:
    /// each area's `name` first, then its alternate names.
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

    /// The country code of each country record, by record id: the country
    /// boundaries that state one, and the derived countries.
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

    /// The groups of regions each admin region lies in, by record id: the
    /// larger admin regions of another name covering its representative
    /// point ("関東地方" of "千葉県"). A record keeps the smaller region as
    /// its own, yet queries name either.
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
                (
                    &candidate.boundary,
                    candidate.boundary.geometry.unsigned_area(),
                    *record_id,
                )
            })
            .collect::<Vec<_>>();
        regions
            .par_iter()
            .filter_map(|(region, area, record_id)| {
                let point = Point::new(
                    region.representative_point[0],
                    region.representative_point[1],
                );
                let groups = regions
                    .iter()
                    .filter(|(group, group_area, _)| {
                        group_area > area
                            && !same_text(&group.name, &region.name)
                            && group.geometry.contains(&point)
                    })
                    .map(|(_, _, group_id)| *group_id)
                    .collect::<Vec<_>>();
                (!groups.is_empty()).then_some((*record_id, groups))
            })
            .collect()
    }

    /// Index the boundaries under their final record ids. Edge indexes of
    /// large areas are built in parallel.
    pub(crate) fn into_index(
        self,
        boundary_record_ids: &[RecordId],
        country_record_ids: &HashMap<String, RecordId>,
    ) -> BoundaryIndex {
        let accepted = self
            .candidates
            .into_par_iter()
            .zip(boundary_record_ids)
            .filter_map(|(candidate, record_id)| {
                let boundary = candidate.boundary;
                let bounds = boundary.geometry.bounding_rect()?;
                Some(AcceptedBoundary {
                    record_id: *record_id,
                    layer: boundary.layer,
                    area: boundary.geometry.unsigned_area(),
                    bounds,
                    inferred_country_record_id: boundary
                        .inferred_country_code
                        .as_deref()
                        .and_then(|code| country_record_ids.get(code).copied()),
                    name: boundary.name,
                    admin_level: boundary.admin_level,
                    source_object_type: candidate.source_object_type,
                    source_object_id: candidate.source_object_id,
                    shape: AreaShape::new(boundary.geometry),
                })
            })
            .collect();
        BoundaryIndex::new(accepted)
    }
}

/// Admin context for a record, from the boundaries covering its display point.
/// Values the source data states (an `addr:city`, a place's own name) win ties
/// between overlapping boundaries.
pub(crate) fn record_context(index: &BoundaryIndex, record: &Record) -> Option<RecordContext> {
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
    Some(index.context_for_point(lon, lat, source))
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
    fn new(boundaries: Vec<AcceptedBoundary>) -> Self {
        let boxes = boundaries
            .iter()
            .enumerate()
            .map(|(boundary_id, boundary)| BoundaryBox {
                envelope: envelope_from_rect(boundary.bounds),
                boundary_id,
            })
            .collect::<Vec<_>>();
        Self {
            boundaries,
            tree: RTree::bulk_load(boxes),
        }
    }

    pub(crate) fn context_for_point(
        &self,
        lon: f64,
        lat: f64,
        source_context: SourceContext<'_>,
    ) -> RecordContext {
        if self.boundaries.is_empty() || !lon.is_finite() || !lat.is_finite() {
            return RecordContext::default();
        }

        let envelope = AABB::from_point([lon, lat]);
        let mut covered = Vec::new();
        for boundary_box in self.tree.locate_in_envelope_intersecting(&envelope) {
            let boundary = &self.boundaries[boundary_box.boundary_id];
            if boundary.shape.covers(lon, lat) {
                covered.push(boundary);
            }
        }

        // One pass over the covering boundaries, keeping the best of each
        // tier, rather than one filtering pass per tier: this runs for every
        // record of the build, and a pass per tier allocated a vector for
        // each of the ten even though all but two or three are empty.
        let mut flags = 0;
        let mut best: [Option<&AcceptedBoundary>; PlaceLayer::ALL.len()] = Default::default();
        for boundary in &covered {
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

        let mut tuple = AdminContextTuple::from_tiers(
            PlaceLayer::ALL
                .into_iter()
                .filter_map(|tier| Some((tier, best[tier as usize]?.record_id))),
        );
        if tuple.country_record_id().is_none()
            && let Some(country) = best[PlaceLayer::Region as usize]
                .and_then(|region| region.inferred_country_record_id)
        {
            tuple.set(PlaceLayer::Country, country);
        }

        RecordContext {
            admin_context: tuple,
            flags,
        }
    }
}

/// Whether the tags make an area records can lie in: an admin boundary, or
/// a named place of a layer the Pack has (see [`area_parts`]).
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

pub(crate) fn required_boundary_way_ids(relations: &[BoundaryRelationStub]) -> HashSet<i64> {
    relations
        .iter()
        .flat_map(|relation| relation.members.iter().map(|member| member.way_id))
        .collect()
}

/// Build boundary polygons from resolved boundary ways and relations.
/// `member_lines` holds the resolved vertices of every relation member way,
/// `None` when a member could not be fully resolved. `check` is asked every
/// [`CHECK_EVERY`] boundaries whether to go on, and its error is returned.
pub(crate) fn build_boundaries(
    ways: &[BoundaryWay],
    relations: &[BoundaryRelationStub],
    member_lines: &HashMap<i64, Option<&[Vertex]>>,
    check: impl Fn() -> crate::error::Result<()>,
) -> crate::error::Result<BoundarySet> {
    let mut candidates = Vec::new();
    for (index, way) in ways.iter().enumerate() {
        if index % CHECK_EVERY == 0 {
            check()?;
        }
        if let Some(boundary) = boundary_from_way(way) {
            candidates.push(BoundaryCandidate {
                source_object_type: OsmObjectType::Way,
                source_object_id: way.object_id,
                boundary,
            });
        }
    }
    for (index, stub) in relations.iter().enumerate() {
        if index % CHECK_EVERY == 0 {
            check()?;
        }
        if let Some(boundary) = boundary_from_relation(stub, member_lines) {
            candidates.push(BoundaryCandidate {
                source_object_type: OsmObjectType::Relation,
                source_object_id: stub.object_id,
                boundary,
            });
        }
    }
    // Layers come from how the boundaries nest, which is only knowable once
    // every candidate is in hand.
    check()?;
    let unmatched_admin_ids = assign_layers_by_containment(&mut candidates);
    let derived_countries = derived_country_records(&candidates);
    Ok(BoundarySet {
        candidates,
        derived_countries,
        unmatched_admin_ids,
    })
}

/// Boundaries assembled between two checks of whether the build goes on.
const CHECK_EVERY: usize = 256;

/// Region boundaries whose ISO 3166-2 code names a country that has no
/// country boundary in the input get a derived country record.
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
            },
        );
    }
    sources.into_iter().collect()
}

fn boundary_from_way(way: &BoundaryWay) -> Option<BuiltBoundary> {
    let AreaParts {
        layer,
        admin_level,
        place_type,
        name,
        alt_names,
        place_layer,
        designation,
    } = area_parts(&way.tags)?;
    let inferred_country_code = country_code_from_tags(&way.tags, layer);
    let closed = ends(&way.vertices).is_some_and(|(first, last)| first == last);
    if way.vertices.len() < 4 || !closed {
        return None;
    }
    let polygon = Polygon::new(ring_line_string(&way.vertices), Vec::new());
    if polygon.unsigned_area() <= f64::EPSILON {
        return None;
    }
    let geometry = MultiPolygon::new(vec![polygon]);
    let representative_point = representative_point(&geometry)?;
    Some(BuiltBoundary {
        layer,
        admin_level,
        place_type,
        name,
        alt_names,
        place_layer,
        designation,
        inferred_country_code,
        geometry,
        representative_point,
    })
}

fn boundary_from_relation(
    stub: &BoundaryRelationStub,
    member_lines: &HashMap<i64, Option<&[Vertex]>>,
) -> Option<BuiltBoundary> {
    let AreaParts {
        layer,
        admin_level,
        place_type,
        name,
        alt_names,
        place_layer,
        designation,
    } = area_parts(&stub.tags)?;
    let inferred_country_code = country_code_from_tags(&stub.tags, layer);
    let mut outer_segments = Vec::new();
    let mut inner_segments = Vec::new();
    for member in &stub.members {
        let line = member_lines
            .get(&member.way_id)
            .copied()
            .flatten()?
            .to_vec();
        match member.role {
            BoundaryMemberRole::Outer => outer_segments.push(line),
            BoundaryMemberRole::Inner => inner_segments.push(line),
        }
    }

    let outer_rings = stitch_rings(outer_segments);
    if outer_rings.is_empty() {
        return None;
    }
    let inner_rings = stitch_rings(inner_segments);
    let geometry = multipolygon_from_rings(outer_rings, inner_rings)?;
    let representative_point = representative_point(&geometry)?;
    Some(BuiltBoundary {
        layer,
        admin_level,
        place_type,
        name,
        alt_names,
        place_layer,
        designation,
        inferred_country_code,
        geometry,
        representative_point,
    })
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

fn stitch_rings(mut segments: Vec<Vec<Vertex>>) -> Vec<Vec<Vertex>> {
    segments.retain(|segment| segment.len() >= 2);
    let mut rings = Vec::new();

    while let Some(mut ring) = segments.pop() {
        loop {
            if ring.len() >= 4 && ends(&ring).is_some_and(|(first, last)| first == last) {
                rings.push(ring);
                break;
            }

            let Some(index) = segments.iter().position(|segment| can_join(&ring, segment)) else {
                break;
            };
            let segment = segments.swap_remove(index);
            join_segment(&mut ring, segment);
        }
    }

    rings
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

/// What the tags of an area say about it.
struct AreaParts {
    layer: PlaceLayer,
    admin_level: Option<u8>,
    place_type: Option<String>,
    name: String,
    alt_names: Vec<String>,
    place_layer: Option<PlaceLayer>,
    designation: Option<String>,
}

/// An admin boundary, or else a named place of a layer the Pack has drawn as
/// an area: an island, a city or a district mapped as a closed way or a
/// multipolygon rather than as a node.
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
    // The codes a country or a region is written as: "DEU", and "NW" of
    // "DE-NW" or "ON" of "CA-ON".
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

fn country_name_from_code(code: &str) -> &str {
    match code {
        "CA" => "Canada",
        "US" => "United States",
        _ => code,
    }
}

/// The levels that are areas of the Pack at all. Outside it an `admin_level`
/// is a supranational union (1) or past anything OSM ranks (12 and up).
const RANKED_ADMIN_LEVELS: std::ops::RangeInclusive<u8> = 0..=11;

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

/// What one level of a country wants: the tier, and whether that is stated
/// by libpostal's table or the level's own `place` tags rather than only the
/// convention for the number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Want {
    layer: Option<PlaceLayer>,
    stated: bool,
}

/// Layers that admin boundaries are ranked into, outermost first. `Place` is
/// left out: it belongs to place nodes, not to administrative boundaries.
const ADMIN_LAYER_ORDER: [PlaceLayer; 9] = PlaceLayer::ADMIN;

/// Boundaries of one level and country whose `place` tag names a layer: at
/// least this many...
const MIN_PLACE_TAGGED_BOUNDARIES: usize = 10;
/// ...and at least this share of them naming the same one, for the level to
/// take that layer.
const PLACE_TAG_AGREEMENT: f64 = 0.8;

/// Give every boundary its layer, per country and admin level.
///
/// A level whose boundaries say what they are wins first: when enough of
/// them carry a `place` tag and nearly all of those name one layer, the
/// whole level takes it. Japan puts its cities, towns, villages and Tokyo's
/// special wards at level 7 and tags many `place=city`, `town` or `village`,
/// and the wards of its designated cities at level 8 with `place=suburb`, so
/// 7 is the locality layer and 8 lies below it. Counting the whole level
/// keeps neighbouring boundaries of one level in one layer.
///
/// The default numbers carry real meaning where a country follows the
/// convention, so the levels tags say nothing about keep them: a level 8
/// stays a locality, a level 10 stays a neighbourhood. The off-convention
/// levels move into whichever layers a country leaves empty. An Australian
/// extract has every suburb at level 9 and nothing at 8, so 9 becomes the
/// locality layer that address queries match on; a German extract fills 8, 9
/// and 10, so 8 keeps locality and 9 settles below it.
///
/// Nominatim arrives at the same behaviour from the other side, with a default
/// number-to-rank table plus a per-country override list, and a rank that
/// shifts when a boundary of the same rank already contains it. Filling gaps
/// and reading tags needs no table to maintain, so a country nobody has
/// written an override for still gets a locality layer.
///
/// Gaps are counted **per country**, because a level means different things on
/// either side of a border, and per country rather than per individual chain so
/// that one sparsely mapped region cannot classify differently to its
/// neighbours, the usual way a relative-depth scheme goes wrong.
fn assign_layers_by_containment(candidates: &mut Vec<BoundaryCandidate>) -> Vec<String> {
    let countries = country_of_each_boundary(candidates);
    let (verdicts, unmatched) = table_layers(candidates, &countries);
    // A boundary the table says is no area of the Pack (a group of regions)
    // goes, and the rest are ranked without it.
    let keep = verdicts
        .iter()
        .map(|verdict| !matches!(verdict, Some(None)))
        .collect::<Vec<_>>();
    let mut kept = keep.iter();
    candidates.retain(|_| *kept.next().expect("one verdict per candidate"));
    let countries = countries
        .into_iter()
        .zip(&keep)
        .filter_map(|(country, keep)| keep.then_some(country))
        .collect::<Vec<_>>();
    let verdicts = verdicts
        .into_iter()
        .filter(|verdict| !matches!(verdict, Some(None)))
        .map(Option::flatten)
        .collect::<Vec<_>>();
    let tagged = tagged_level_layers(candidates, &countries);

    // What each country's levels want, before any of them compete. A level
    // the table names, whose boundaries tag what they are, or that follows
    // the convention has a preference; the rest have none and take whatever
    // their depth earns them.
    let mut wanted: BTreeMap<Option<String>, BTreeMap<u8, Want>> = BTreeMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let Some(level) = candidate.boundary.admin_level else {
            continue;
        };
        let country = countries[index].clone();
        // Whether the tier is stated -- by the table or by the level's own
        // tags -- or only the convention for the number. A stated tier
        // outranks a conventional one when the two want the same place.
        let stated = verdicts[index].or_else(|| tagged.get(&(country.clone(), level)).copied());
        let want = Want {
            layer: stated.or_else(|| conventional_admin_layer(level)),
            stated: stated.is_some(),
        };
        let slot = wanted
            .entry(country)
            .or_default()
            .entry(level)
            .or_insert(want);
        // One level, one tier: a level whose boundaries disagree keeps the
        // outermost reading, so neighbouring boundaries stay in one tier.
        if let (Some(have), Some(layer)) = (slot.layer, want.layer)
            && layer < have
        {
            *slot = want;
        }
    }

    // Hand each country's levels a tier, outermost level first, never going
    // back up. A level takes the tier it wants when that tier is still free
    // and no deeper level has already passed it; otherwise it takes the next
    // free tier below the last one handed out.
    //
    // There is no slot to compete for any more: with a sub-tier under each
    // conventional one there are eight, which covers the deepest hierarchy
    // the table maps (Germany's nine levels, three of them state_district).
    // A country deeper than that runs out at the innermost tier, where the
    // chain keeps the outermost of the pair rather than dropping it.
    const LOCALITY_AT: usize = 5;
    let mut assigned: BTreeMap<(Option<String>, u8), PlaceLayer> = BTreeMap::new();
    for (country, levels) in &wanted {
        // One level of each country anchors the locality tier, the tier an
        // address query matches a town on. It is the outermost level the
        // table calls a city, else the outermost below the district --
        // Australia's level 9 is tagged a suburb and is still the town an
        // address names, and without an anchor an Australian Pack answers no
        // structured query at all.
        let anchor = levels
            .iter()
            .find(|(_, want)| want.layer == Some(PlaceLayer::Locality))
            .or_else(|| {
                levels
                    .iter()
                    .find(|(_, want)| want.layer.is_none_or(|layer| layer > PlaceLayer::District))
            })
            .map(|(level, _)| *level);
        let anchor_index = anchor.and_then(|level| levels.keys().position(|at| *at == level));

        let mut next = 0usize;
        for (index, (&level, want)) in levels.iter().enumerate() {
            // Keep one tier for each level still to come, so an outer level's
            // preference cannot eat the ladder and strand the rest at the
            // innermost tier. Taiwan calls its level 4 a city, which alone
            // would spend six tiers before its five deeper levels are placed.
            let cap = ADMIN_LAYER_ORDER.len() - 1 - (levels.len() - index - 1);
            let at = if Some(index) == anchor_index {
                next.max(LOCALITY_AT.min(cap))
            } else {
                let at = match want.layer {
                    // Respect the preference only when it does not go
                    // backwards; a country that maps two levels onto one
                    // conventional tier puts the second in its sub-tier.
                    Some(layer) => ADMIN_LAYER_ORDER
                        .iter()
                        .position(|tier| *tier == layer)
                        .unwrap_or(next)
                        .max(next),
                    None => next,
                };
                // Yield a tier for every deeper level wanting this one or
                // shallower, so the innermost of them keeps the tier they all
                // name. Australia calls both its 9 and its 10 a suburb; 10 is
                // the neighbourhood and 9 the sub-locality above it, where
                // before one of the two was dropped.
                let contested = levels
                    .values()
                    .skip(index + 1)
                    .filter_map(|want| want.layer)
                    .filter_map(|layer| ADMIN_LAYER_ORDER.iter().position(|tier| *tier == layer))
                    .filter(|wanted| *wanted <= at)
                    .count();
                let at = next.max(at - contested.min(at));
                // Leave the anchor room to reach the locality tier: an outer
                // level that settles too deep would push it past its own.
                let at = match anchor_index {
                    Some(anchor) if index < anchor => {
                        at.min(next.max(LOCALITY_AT.saturating_sub(anchor - index)))
                    }
                    _ => at,
                };
                if at > cap { cap.max(next) } else { at }
            };
            assigned.insert((country.clone(), level), ADMIN_LAYER_ORDER[at]);
            next = at + 1;
        }
    }

    for (index, candidate) in candidates.iter_mut().enumerate() {
        let Some(level) = candidate.boundary.admin_level else {
            continue;
        };
        if let Some(layer) = assigned.get(&(countries[index].clone(), level)) {
            candidate.boundary.layer = *layer;
        }
    }
    unmatched
}

/// What libpostal's admin table says of each boundary (see
/// [`admin_levels`]): a layer, `Some(None)` for no area of the Pack, `None`
/// when it says nothing. Also the objects the table names for the input's
/// countries that the input lacks.
fn table_layers(
    candidates: &[BoundaryCandidate],
    countries: &[Option<String>],
) -> (Vec<Option<Option<PlaceLayer>>>, Vec<String>) {
    let table = admin_levels();
    let present = countries
        .iter()
        .flatten()
        .map(|code| code.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
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
        .zip(countries)
        .map(|(candidate, country)| {
            let country = country.as_ref().map(|code| code.to_ascii_lowercase());
            let boundary = admin_levels::Boundary {
                object: (candidate.source_object_type, candidate.source_object_id),
                country: country.as_deref(),
                admin_level: candidate.boundary.admin_level,
                designation: candidate.boundary.designation.as_deref(),
            };
            let [lon, lat] = candidate.boundary.representative_point;
            table.verdict(boundary, |object| {
                by_object.get(&object).is_some_and(|index| {
                    candidates[*index]
                        .boundary
                        .geometry
                        .contains(&Point::new(lon, lat))
                })
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

/// The layer the `place` tags of each country's levels agree on, where they
/// do (see [`MIN_PLACE_TAGGED_BOUNDARIES`] and [`PLACE_TAG_AGREEMENT`]).
/// Country levels are left to their number.
fn tagged_level_layers(
    candidates: &[BoundaryCandidate],
    countries: &[Option<String>],
) -> BTreeMap<(Option<String>, u8), PlaceLayer> {
    let mut votes: BTreeMap<(Option<String>, u8), BTreeMap<PlaceLayer, usize>> = BTreeMap::new();
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
                .entry((country.clone(), level))
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

/// The layer a level maps to when a country follows the usual convention.
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
fn country_of_each_boundary(candidates: &[BoundaryCandidate]) -> Vec<Option<String>> {
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
                .map(|code| (&candidate.boundary.geometry, code))
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
        .iter()
        .map(|candidate| {
            if let Some(code) = &candidate.boundary.inferred_country_code {
                return Some(code.clone());
            }
            let point = Point::new(
                candidate.boundary.representative_point[0],
                candidate.boundary.representative_point[1],
            );
            country_boundaries
                .iter()
                .find(|(geometry, _)| geometry.contains(&point))
                .map(|(_, code)| code.clone())
        })
        .collect()
}

/// Boundaries at or above this level are countries whatever their depth.
const COUNTRY_ADMIN_LEVEL: u8 = 2;

fn representative_point(geometry: &MultiPolygon<f64>) -> Option<[f64; 2]> {
    geometry.centroid().map(|point| [point.x(), point.y()])
}

fn compare_boundary(
    layer: PlaceLayer,
    left: &AcceptedBoundary,
    right: &AcceptedBoundary,
    source_context: SourceContext<'_>,
) -> Ordering {
    let source_value = source_value_for_layer(layer, source_context);
    let left_match = source_value
        .map(|value| same_text(value, &left.name))
        .unwrap_or(false);
    let right_match = source_value
        .map(|value| same_text(value, &right.name))
        .unwrap_or(false);

    // An admin boundary of the layer wins over a place drawn as an area,
    // which fills the layers admin boundaries leave empty.
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
    source_context: SourceContext<'a>,
) -> Option<&'a str> {
    // A sub-tier is a subdivision of the tier above and the source address
    // states it in the same field, so both read one value.
    match layer {
        PlaceLayer::Country => source_context.country,
        PlaceLayer::Region | PlaceLayer::SubRegion => source_context.region,
        PlaceLayer::District | PlaceLayer::SubDistrict => source_context.district,
        PlaceLayer::Locality | PlaceLayer::SubLocality => source_context.locality,
        PlaceLayer::Neighbourhood | PlaceLayer::SubNeighbourhood => source_context.neighbourhood,
        PlaceLayer::Place => source_context.place,
    }
}

fn envelope_from_rect(rect: Rect<f64>) -> AABB<[f64; 2]> {
    AABB::from_corners([rect.min().x, rect.min().y], [rect.max().x, rect.max().y])
}

fn same_text(left: &str, right: &str) -> bool {
    normalize_for_compare(left) == normalize_for_compare(right)
}

fn source_type_rank(object_type: OsmObjectType) -> u8 {
    match object_type {
        OsmObjectType::Relation => 0,
        OsmObjectType::Way => 1,
        OsmObjectType::Node => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let index = BoundaryIndex::new(vec![toronto, oakville]);

        let assignment = index.context_for_point(-79.5, 43.5, SourceContext::default());

        assert_eq!(assignment.admin_context.locality_record_id(), Some(1));
        assert_eq!(assignment.flags, 0);
    }

    #[test]
    fn shared_edge_sets_ambiguous_flag_and_chooses_stable_boundary() {
        let left = square_boundary(10, "Left", PlaceLayer::Locality, -1.0, -1.0, 0.0, 1.0);
        let right = square_boundary(11, "Right", PlaceLayer::Locality, 0.0, -1.0, 1.0, 1.0);
        let index = BoundaryIndex::new(vec![right, left]);

        let assignment = index.context_for_point(0.0, 0.0, SourceContext::default());

        assert_eq!(assignment.admin_context.locality_record_id(), Some(10));
        assert_eq!(assignment.flags, CONTEXT_FLAG_AMBIGUOUS_ADMIN);
    }

    /// The ACT as it really is: a district at 7 and a suburb at 9, nested.
    /// A point in both must carry both -- the suburb reached no context at
    /// all while only the conventional tiers were looked for, so every
    /// Canberra address named its district and lost its suburb.
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
        let index = BoundaryIndex::new(vec![district, suburb]);

        let assignment = index.context_for_point(0.0, 0.0, SourceContext::default());

        let tuple = &assignment.admin_context;
        assert_eq!(tuple.get(PlaceLayer::Locality), Some(1), "the district");
        assert_eq!(tuple.get(PlaceLayer::SubLocality), Some(2), "the suburb");
        assert_eq!(tuple.entries().len(), 2, "both, not one: {tuple:?}");
    }

    /// Australia puts its suburbs at level 9 and uses no level 8, so without
    /// gap filling an Australian pack has no locality layer for an address
    /// query to match on.
    #[test]
    fn an_off_convention_level_fills_the_layer_its_country_leaves_empty() {
        let layers = layers_for_levels(&[2, 4, 6, 9]);
        assert_eq!(layers[&9], PlaceLayer::Locality, "level 9 fills locality");
        assert_eq!(layers[&4], PlaceLayer::Region, "conventional levels keep");
        assert_eq!(layers[&6], PlaceLayer::District);
    }

    /// The ACT's real shape: a district at 7, suburbs at 9, nothing at 6 or
    /// 8. Two spare levels in one country must take two layers -- before the
    /// filled set was updated as each was placed, both asked for the same one
    /// and the second fell to the innermost layer, leaving an address there
    /// with a neighbourhood and no locality.
    #[test]
    fn two_spare_levels_take_two_layers() {
        let layers = layers_for_levels(&[2, 4, 7, 9]);
        assert_eq!(layers[&4], PlaceLayer::Region);
        assert_eq!(layers[&7], PlaceLayer::Locality, "7 takes the first free");
        assert_eq!(layers[&9], PlaceLayer::Neighbourhood, "9 takes the next");
        assert_ne!(layers[&7], layers[&9], "two levels, two layers");
    }

    /// Germany fills 8, 9 and 10, so the conventional levels must keep their
    /// layers and 9 settle below the locality it nests inside.
    #[test]
    fn an_off_convention_level_never_displaces_a_conventional_one() {
        let layers = layers_for_levels(&[2, 4, 8, 9, 10]);
        assert_eq!(layers[&8], PlaceLayer::Locality, "8 keeps locality");
        assert_eq!(layers[&10], PlaceLayer::Neighbourhood, "10 keeps its layer");
        assert_ne!(layers[&9], PlaceLayer::Locality, "9 cannot take locality");
    }

    /// The pre-existing mapping has to survive unchanged, or every pack built
    /// from a country that follows the convention would shift.
    #[test]
    fn a_conventional_hierarchy_is_left_alone() {
        let layers = layers_for_levels(&[2, 4, 6, 8, 10]);
        assert_eq!(layers[&2], PlaceLayer::Country);
        assert_eq!(layers[&4], PlaceLayer::Region);
        assert_eq!(layers[&6], PlaceLayer::District);
        assert_eq!(layers[&8], PlaceLayer::Locality);
        assert_eq!(layers[&10], PlaceLayer::Neighbourhood);
    }

    /// Japan tags many of its level 7 cities `place=city`, `town` or
    /// `village` and the level 8 wards of its designated cities
    /// `place=suburb`: 7 is the locality layer, 8 lies below it, and the
    /// untagged levels keep their numbers' layers.
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
        // The wards sit below the locality and above the level 10
        // neighbourhoods. Both were the neighbourhood tier while there was
        // one for them to share, which dropped whichever lost the race.
        assert_eq!(layers[&8], PlaceLayer::SubLocality, "tagged wards");
        assert_eq!(layers[&10], PlaceLayer::Neighbourhood);
        assert_ne!(layers[&8], layers[&10], "one tier each");
    }

    /// Too few tagged boundaries, or tags that disagree, leave a level to
    /// its number.
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

    /// Every country whose levels include a town must fill the locality
    /// tier: that is the tier an address query matches a town on, so a
    /// country that fills none answers no structured query.
    ///
    /// The countries the table gives no town at all (the UAE is a country
    /// and its emirates, nothing below) are left out: no ranking can invent
    /// a tier the source data does not describe.
    ///
    /// This is a smoke test over the whole table, not a check that each
    /// country's tiers are the right ones -- only a fixture with ground
    /// truth can say that.
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

    /// The same, for a sub-national extract: a Geofabrik state download holds
    /// no level 2 boundary, so the country is known only from the region's
    /// ISO3166-2 tag. Without that fallback the whole table is skipped and
    /// every level of the extract falls back on the convention.
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

    /// Two levels of one country must never share a layer: the deeper one is
    /// then dropped to the innermost layer and the slot it wanted stays
    /// empty.
    #[test]
    fn every_country_gives_each_level_its_own_layer() {
        let mut clashes = Vec::new();
        for (country, levels) in admin_levels().levels_by_country() {
            if levels.len() < 2 || !admin_levels().names_a_locality(&country) {
                continue;
            }
            let layers = layers_for_levels_in(Some(&country), &levels);
            let mut seen: BTreeMap<PlaceLayer, u8> = BTreeMap::new();
            for (level, layer) in &layers {
                if let Some(first) = seen.insert(*layer, *level) {
                    clashes.push(format!("{country}: {first} and {level} both {layer:?}"));
                }
            }
        }
        assert!(
            clashes.is_empty(),
            "{} level clashes:\n  {}",
            clashes.len(),
            clashes.join("\n  ")
        );
    }

    /// The claim this change exists for: a country deeper than the
    /// conventional five keeps every level, each in a tier of its own.
    /// Germany maps nine, three of them `state_district` and two
    /// `city_district`; with five tiers four of the nine were dropped from
    /// every German address.
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

    /// Every country of the table keeps each of its levels in its own tier.
    /// Unsatisfiable while there were five, which is why the levels that
    /// lost the race were dropped rather than ranked.
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

    /// Builds one candidate per level inside a shared country boundary and
    /// returns the layer each one ends up in.
    fn layers_for_levels(levels: &[u8]) -> BTreeMap<u8, PlaceLayer> {
        layers_for_tagged_levels(
            &levels
                .iter()
                .map(|level| (*level, None, 1))
                .collect::<Vec<_>>(),
        )
    }

    /// Like [`layers_for_levels`], in a named country, so the table's
    /// per-country rows apply. Every boundary carries the code, which is what
    /// an extract with no level 2 boundary has to fall back on.
    fn layers_for_levels_in(country: Option<&str>, levels: &[u8]) -> BTreeMap<u8, PlaceLayer> {
        let tagged = levels
            .iter()
            .map(|level| (*level, None, 1))
            .collect::<Vec<_>>();
        layers_for_tagged_levels_in(country, &tagged)
    }

    /// Like [`layers_for_levels`], with `count` boundaries of each level and
    /// `place` tag layer. Every boundary of a level must end in one layer.
    fn layers_for_tagged_levels(
        levels: &[(u8, Option<PlaceLayer>, usize)],
    ) -> BTreeMap<u8, PlaceLayer> {
        layers_for_tagged_levels_in(None, levels)
    }

    fn layers_for_tagged_levels_in(
        country: Option<&str>,
        levels: &[(u8, Option<PlaceLayer>, usize)],
    ) -> BTreeMap<u8, PlaceLayer> {
        let mut candidates = Vec::new();
        let levels = levels
            .iter()
            .flat_map(|(level, place_layer, count)| {
                std::iter::repeat_n((*level, *place_layer), *count)
            })
            .collect::<Vec<_>>();
        for (index, &(level, place_layer)) in levels.iter().enumerate() {
            // Each level nests inside the one before it.
            let span = 10.0 / (index + 1) as f64;
            let ring = LineString::from(vec![
                (-span, -span),
                (span, -span),
                (span, span),
                (-span, span),
                (-span, -span),
            ]);
            let geometry = MultiPolygon::new(vec![Polygon::new(ring, Vec::new())]);
            candidates.push(BoundaryCandidate {
                source_object_type: OsmObjectType::Relation,
                source_object_id: index as i64 + 1,
                boundary: BuiltBoundary {
                    layer: admin_level_layer(level).expect("layer"),
                    admin_level: Some(level),
                    place_type: None,
                    name: format!("level {level}"),
                    alt_names: Vec::new(),
                    place_layer,
                    designation: None,
                    inferred_country_code: country
                        .map(str::to_ascii_uppercase)
                        .or_else(|| (level <= 2).then(|| "AU".to_string())),
                    representative_point: representative_point(&geometry).expect("point"),
                    geometry,
                },
            });
        }
        assign_layers_by_containment(&mut candidates);
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
        let index = BoundaryIndex::new(vec![region]);

        let assignment = index.context_for_point(-79.0, 43.0, SourceContext::default());

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
        let set = build_boundaries(&ways, &[], &HashMap::new(), || Ok(())).expect("boundaries");
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
            // Reversed relative to the ring direction.
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
            BTreeMap::from([
                ("boundary".to_string(), "administrative".to_string()),
                ("admin_level".to_string(), "4".to_string()),
                ("name".to_string(), name.to_string()),
                ("ISO3166-2".to_string(), iso.to_string()),
            ])
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
                tags: tags("Ontario", "CA-ON"),
            },
            BoundaryRelationStub {
                object_id: 101,
                members: members(&[1, 3]),
                tags: tags("Broken", "CA-QC"),
            },
        ];

        let member_lines = member_lines
            .iter()
            .map(|(way_id, line): (&i64, &Option<Vec<Vertex>>)| (*way_id, line.as_deref()))
            .collect();
        let set = build_boundaries(&[], &relations, &member_lines, || Ok(())).expect("boundaries");
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
        assert_eq!(report.accepted.place_nodes, 2);

        let index = set.into_index(&[5], &HashMap::from([("CA".to_string(), 4)]));
        let context = index.context_for_point(-75.0, 45.0, SourceContext::default());
        assert_eq!(context.admin_context.region_record_id(), Some(5));
        assert_eq!(context.admin_context.country_record_id(), Some(4));
    }

    #[test]
    fn parses_country_code_from_region_iso3166_2() {
        let tags = BTreeMap::from([
            ("boundary".to_string(), "administrative".to_string()),
            ("admin_level".to_string(), "4".to_string()),
            ("name".to_string(), "Ontario".to_string()),
            ("ISO3166-2".to_string(), "CA-ON".to_string()),
        ]);

        assert_eq!(
            country_code_from_tags(&tags, PlaceLayer::Region).as_deref(),
            Some("CA")
        );
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
            name: name.to_string(),
            admin_level: Some(8),
            inferred_country_record_id: None,
            source_object_type: OsmObjectType::Relation,
            source_object_id: record_id as i64,
            area: geometry.unsigned_area(),
            bounds: geometry.bounding_rect().expect("bounds"),
            shape: AreaShape::new(geometry),
        }
    }
}
