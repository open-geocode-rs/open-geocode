use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet, hash_map::Entry},
    path::Path,
};

use anyhow::{Context, Result};
use geo::{
    Area, BoundingRect, Centroid, Contains, Covers, LineString, MultiPolygon, Point, Polygon, Rect,
};
use geojson::GeometryValue;
use osmpbf::Element;
use rstar::{AABB, RTree, RTreeObject};

use crate::{
    builder::report::BuilderReport,
    context::{AdminContextTuple, CONTEXT_FLAG_AMBIGUOUS_ADMIN},
    pack::{PackWriter, RecordId, RecordWriter},
    record::{
        AddressRecord, InterpolationRecord, OsmObjectType, PlaceLayer, PlaceRecord, PostcodeRecord,
        SourceProvenance, StreetRecord, point_geometry,
    },
};

use super::pbf::element_reader_with_progress;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundaryWayStub {
    pub object_id: i64,
    pub node_refs: Vec<i64>,
    pub tags: BTreeMap<String, String>,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BoundaryContextAssignment {
    pub admin_context: AdminContextTuple,
    pub flags: u16,
}

#[derive(Debug, Clone)]
struct AcceptedBoundary {
    record_id: RecordId,
    layer: PlaceLayer,
    name: String,
    admin_level: u8,
    inferred_country_record_id: Option<RecordId>,
    source_object_type: OsmObjectType,
    source_object_id: i64,
    geometry: MultiPolygon<f64>,
    area: f64,
}

#[derive(Debug, Clone)]
struct BoundaryBox {
    envelope: AABB<[f64; 2]>,
    boundary_id: usize,
}

struct BuiltBoundary {
    layer: PlaceLayer,
    admin_level: u8,
    name: String,
    inferred_country_code: Option<String>,
    geometry: MultiPolygon<f64>,
    representative_point: [f64; 2],
}

struct BoundaryCandidate {
    source_object_type: OsmObjectType,
    source_object_id: i64,
    boundary: BuiltBoundary,
}

#[derive(Clone)]
struct InferredCountrySource {
    code: String,
    name: String,
    representative_point: [f64; 2],
    source_object_type: OsmObjectType,
    source_object_id: i64,
}

pub(crate) struct BoundaryContextRecordWriter<'a> {
    inner: &'a mut PackWriter,
    boundary_index: &'a BoundaryIndex,
}

impl<'a> BoundaryContextRecordWriter<'a> {
    pub(crate) fn new(inner: &'a mut PackWriter, boundary_index: &'a BoundaryIndex) -> Self {
        Self {
            inner,
            boundary_index,
        }
    }

    fn write_context(
        &mut self,
        record_id: RecordId,
        point: Option<[f64; 2]>,
        source_context: SourceContext<'_>,
    ) {
        let Some([lon, lat]) = point else {
            return;
        };
        let assignment = self
            .boundary_index
            .context_for_point(lon, lat, source_context);
        self.inner.write_boundary_context(
            record_id,
            assignment.admin_context,
            None,
            assignment.flags,
        );
    }
}

impl RecordWriter for BoundaryContextRecordWriter<'_> {
    fn write_address(&mut self, record: &AddressRecord) -> Result<RecordId> {
        let record_id = self.inner.write_address(record)?;
        self.write_context(
            record_id,
            point_coordinates(&record.geometry),
            SourceContext {
                country: record.address.country.as_deref(),
                region: record.address.region.as_deref(),
                locality: record.address.locality.as_deref(),
                place: record.address.place.as_deref(),
                ..SourceContext::default()
            },
        );
        Ok(record_id)
    }

    fn write_place(&mut self, record: &PlaceRecord, layer: PlaceLayer) -> Result<RecordId> {
        let record_id = self.inner.write_place(record, layer)?;
        self.write_context(
            record_id,
            point_coordinates(&record.geometry),
            SourceContext {
                place: Some(&record.name),
                ..SourceContext::default()
            },
        );
        Ok(record_id)
    }

    fn write_interpolation(&mut self, record: &InterpolationRecord) -> Result<RecordId> {
        let record_id = self.inner.write_interpolation(record)?;
        self.write_context(
            record_id,
            Some(record.representative_point),
            SourceContext {
                country: record.address.country.as_deref(),
                region: record.address.region.as_deref(),
                locality: record.address.locality.as_deref(),
                place: record.address.place.as_deref(),
                ..SourceContext::default()
            },
        );
        Ok(record_id)
    }

    fn write_street(&mut self, record: &StreetRecord) -> Result<RecordId> {
        let record_id = self.inner.write_street(record)?;
        self.write_context(
            record_id,
            Some(record.representative_point),
            SourceContext::default(),
        );
        Ok(record_id)
    }

    fn write_postcode(&mut self, record: &PostcodeRecord) -> Result<RecordId> {
        let record_id = self.inner.write_postcode(record)?;
        self.write_context(
            record_id,
            point_coordinates(&record.geometry),
            SourceContext::default(),
        );
        Ok(record_id)
    }

    fn write_rejection(&mut self, rejection: crate::record::RejectedRecord) -> Result<()> {
        self.inner.write_rejection(rejection)
    }
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
            .filter_map(|(boundary_id, boundary)| {
                let bbox = boundary.geometry.bounding_rect()?;
                Some(BoundaryBox {
                    envelope: envelope_from_rect(bbox),
                    boundary_id,
                })
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
    ) -> BoundaryContextAssignment {
        if self.boundaries.is_empty() || !lon.is_finite() || !lat.is_finite() {
            return BoundaryContextAssignment {
                admin_context: AdminContextTuple::default(),
                flags: 0,
            };
        }

        let point = Point::new(lon, lat);
        let envelope = AABB::from_point([lon, lat]);
        let mut covered = Vec::new();
        for boundary_box in self.tree.locate_in_envelope_intersecting(&envelope) {
            let boundary = &self.boundaries[boundary_box.boundary_id];
            if boundary.geometry.covers(&point) {
                covered.push(boundary);
            }
        }

        let mut flags = 0;
        let mut tuple = AdminContextTuple::default();
        for layer in [
            PlaceLayer::Country,
            PlaceLayer::Region,
            PlaceLayer::District,
            PlaceLayer::Locality,
            PlaceLayer::Neighbourhood,
            PlaceLayer::Place,
        ] {
            let layer_matches = covered
                .iter()
                .copied()
                .filter(|boundary| boundary.layer == layer)
                .collect::<Vec<_>>();
            if layer_matches.len() > 1 {
                flags |= CONTEXT_FLAG_AMBIGUOUS_ADMIN;
            }
            let Some(best) = choose_boundary(layer, &layer_matches, source_context) else {
                continue;
            };
            set_tuple_layer(&mut tuple, layer, best.record_id);
            if layer == PlaceLayer::Region && tuple.country_record_id.is_none() {
                tuple.country_record_id = best.inferred_country_record_id;
            }
        }

        BoundaryContextAssignment {
            admin_context: tuple,
            flags,
        }
    }
}

pub(crate) fn has_admin_boundary_tags(tags: &BTreeMap<String, String>) -> bool {
    admin_boundary_parts(tags).is_some()
}

pub(crate) fn relation_member_role(value: &str) -> Option<BoundaryMemberRole> {
    match normalize(value).as_str() {
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

pub(crate) fn resolve_boundary_member_way_refs(
    input: &Path,
    required_way_ids: &HashSet<i64>,
) -> Result<HashMap<i64, Vec<i64>>> {
    let mut way_refs = HashMap::new();
    if required_way_ids.is_empty() {
        return Ok(way_refs);
    }

    let (reader, progress) = element_reader_with_progress(input, "2/7 resolve boundary ways")?;
    reader
        .for_each(|element| {
            if let Element::Way(way) = element
                && required_way_ids.contains(&way.id())
            {
                way_refs.insert(way.id(), way.refs().collect::<Vec<_>>());
            }
        })
        .with_context(|| format!("failed to resolve boundary ways from {}", input.display()))?;
    progress.finish_with_message("2/7 resolve boundary ways complete");

    Ok(way_refs)
}

pub(crate) fn write_boundary_records(
    way_stubs: &[BoundaryWayStub],
    relation_stubs: &[BoundaryRelationStub],
    relation_member_ways: &HashMap<i64, Vec<i64>>,
    node_locations: &HashMap<i64, (f64, f64)>,
    writer: &mut dyn RecordWriter,
    report: &mut BuilderReport,
) -> Result<BoundaryIndex> {
    let mut candidates = Vec::new();

    for stub in way_stubs {
        let Some(boundary) = boundary_from_way(stub, node_locations) else {
            continue;
        };
        candidates.push(BoundaryCandidate {
            source_object_type: OsmObjectType::Way,
            source_object_id: stub.object_id,
            boundary,
        });
    }

    for stub in relation_stubs {
        let Some(boundary) = boundary_from_relation(stub, relation_member_ways, node_locations)
        else {
            continue;
        };
        candidates.push(BoundaryCandidate {
            source_object_type: OsmObjectType::Relation,
            source_object_id: stub.object_id,
            boundary,
        });
    }

    let derived_country_record_ids = write_derived_country_records(&candidates, writer, report)?;

    let mut accepted = Vec::new();
    for candidate in candidates {
        let record_id = write_boundary_place_record(
            candidate.source_object_type,
            candidate.source_object_id,
            &candidate.boundary,
            writer,
            report,
        )?;
        accepted.push(accepted_boundary(
            record_id,
            candidate.source_object_type,
            candidate.source_object_id,
            candidate.boundary,
            &derived_country_record_ids,
        ));
    }

    Ok(BoundaryIndex::new(accepted))
}

fn write_derived_country_records(
    candidates: &[BoundaryCandidate],
    writer: &mut dyn RecordWriter,
    report: &mut BuilderReport,
) -> Result<HashMap<String, RecordId>> {
    let actual_country_codes = candidates
        .iter()
        .filter(|candidate| candidate.boundary.layer == PlaceLayer::Country)
        .filter_map(|candidate| candidate.boundary.inferred_country_code.clone())
        .collect::<HashSet<_>>();

    let mut sources = HashMap::new();
    for candidate in candidates {
        if candidate.boundary.layer != PlaceLayer::Region {
            continue;
        }
        let Some(code) = &candidate.boundary.inferred_country_code else {
            continue;
        };
        if actual_country_codes.contains(code) {
            continue;
        }
        match sources.entry(code.clone()) {
            Entry::Occupied(_) => {}
            Entry::Vacant(entry) => {
                entry.insert(InferredCountrySource {
                    code: code.clone(),
                    name: country_name_from_code(code).to_string(),
                    representative_point: candidate.boundary.representative_point,
                    source_object_type: candidate.source_object_type,
                    source_object_id: candidate.source_object_id,
                });
            }
        }
    }

    let mut record_ids = HashMap::new();
    let mut sources = sources.into_values().collect::<Vec<_>>();
    sources.sort_by(|left, right| left.code.cmp(&right.code));
    for source in sources {
        let record = PlaceRecord {
            name: source.name.clone(),
            place_type: format!("derived_country:{}", source.code),
            geometry: point_geometry(
                source.representative_point[0],
                source.representative_point[1],
            ),
            source: SourceProvenance::osm(source.source_object_type, source.source_object_id),
        };
        let record_id = writer.write_place(&record, PlaceLayer::Country)?;
        report.accept_place(PlaceLayer::Country);
        record_ids.insert(source.code, record_id);
    }

    Ok(record_ids)
}

fn write_boundary_place_record(
    object_type: OsmObjectType,
    object_id: i64,
    boundary: &BuiltBoundary,
    writer: &mut dyn RecordWriter,
    report: &mut BuilderReport,
) -> Result<RecordId> {
    let record = PlaceRecord {
        name: boundary.name.clone(),
        place_type: format!("admin_level:{}", boundary.admin_level),
        geometry: point_geometry(
            boundary.representative_point[0],
            boundary.representative_point[1],
        ),
        source: SourceProvenance::osm(object_type, object_id),
    };
    let record_id = writer.write_place(&record, boundary.layer)?;
    report.accept_place(boundary.layer);
    Ok(record_id)
}

fn accepted_boundary(
    record_id: RecordId,
    source_object_type: OsmObjectType,
    source_object_id: i64,
    boundary: BuiltBoundary,
    derived_country_record_ids: &HashMap<String, RecordId>,
) -> AcceptedBoundary {
    let area = boundary.geometry.unsigned_area();
    let inferred_country_record_id = boundary
        .inferred_country_code
        .as_deref()
        .and_then(|code| derived_country_record_ids.get(code).copied());
    AcceptedBoundary {
        record_id,
        layer: boundary.layer,
        name: boundary.name,
        admin_level: boundary.admin_level,
        inferred_country_record_id,
        source_object_type,
        source_object_id,
        geometry: boundary.geometry,
        area,
    }
}

fn boundary_from_way(
    stub: &BoundaryWayStub,
    node_locations: &HashMap<i64, (f64, f64)>,
) -> Option<BuiltBoundary> {
    let (layer, admin_level, name) = admin_boundary_parts(&stub.tags)?;
    let inferred_country_code = country_code_from_tags(&stub.tags, layer);
    let polygon = polygon_from_node_refs(&stub.node_refs, node_locations, Vec::new())?;
    let geometry = MultiPolygon::new(vec![polygon]);
    let representative_point = representative_point(&geometry)?;
    Some(BuiltBoundary {
        layer,
        admin_level,
        name,
        inferred_country_code,
        geometry,
        representative_point,
    })
}

fn boundary_from_relation(
    stub: &BoundaryRelationStub,
    relation_member_ways: &HashMap<i64, Vec<i64>>,
    node_locations: &HashMap<i64, (f64, f64)>,
) -> Option<BuiltBoundary> {
    let (layer, admin_level, name) = admin_boundary_parts(&stub.tags)?;
    let inferred_country_code = country_code_from_tags(&stub.tags, layer);
    let mut outer_segments = Vec::new();
    let mut inner_segments = Vec::new();
    for member in &stub.members {
        let refs = relation_member_ways.get(&member.way_id)?.clone();
        match member.role {
            BoundaryMemberRole::Outer => outer_segments.push(refs),
            BoundaryMemberRole::Inner => inner_segments.push(refs),
        }
    }

    let outer_rings = stitch_rings(outer_segments);
    if outer_rings.is_empty() {
        return None;
    }
    let inner_rings = stitch_rings(inner_segments);
    let geometry = multipolygon_from_rings(outer_rings, inner_rings, node_locations)?;
    let representative_point = representative_point(&geometry)?;
    Some(BuiltBoundary {
        layer,
        admin_level,
        name,
        inferred_country_code,
        geometry,
        representative_point,
    })
}

fn multipolygon_from_rings(
    outer_rings: Vec<Vec<i64>>,
    inner_rings: Vec<Vec<i64>>,
    node_locations: &HashMap<i64, (f64, f64)>,
) -> Option<MultiPolygon<f64>> {
    let mut outers = Vec::new();
    for ring in outer_rings {
        let exterior = line_string_from_node_refs(&ring, node_locations)?;
        let polygon = Polygon::new(exterior, Vec::new());
        if polygon.unsigned_area() > f64::EPSILON {
            outers.push((polygon, Vec::new()));
        }
    }
    if outers.is_empty() {
        return None;
    }

    for ring in inner_rings {
        let interior = line_string_from_node_refs(&ring, node_locations)?;
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

fn polygon_from_node_refs(
    node_refs: &[i64],
    node_locations: &HashMap<i64, (f64, f64)>,
    interiors: Vec<LineString<f64>>,
) -> Option<Polygon<f64>> {
    let exterior = line_string_from_node_refs(node_refs, node_locations)?;
    let polygon = Polygon::new(exterior, interiors);
    (polygon.unsigned_area() > f64::EPSILON).then_some(polygon)
}

fn line_string_from_node_refs(
    node_refs: &[i64],
    node_locations: &HashMap<i64, (f64, f64)>,
) -> Option<LineString<f64>> {
    if node_refs.len() < 4 || node_refs.first() != node_refs.last() {
        return None;
    }
    let coordinates = node_refs
        .iter()
        .map(|node_id| {
            let (lat, lon) = node_locations.get(node_id)?;
            Some((*lon, *lat))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(LineString::from(coordinates))
}

fn stitch_rings(mut segments: Vec<Vec<i64>>) -> Vec<Vec<i64>> {
    segments.retain(|segment| segment.len() >= 2);
    let mut rings = Vec::new();

    while let Some(mut ring) = segments.pop() {
        loop {
            if ring.len() >= 4 && ring.first() == ring.last() {
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

fn can_join(ring: &[i64], segment: &[i64]) -> bool {
    let Some((&ring_first, &ring_last)) = ring.first().zip(ring.last()) else {
        return false;
    };
    let Some((&segment_first, &segment_last)) = segment.first().zip(segment.last()) else {
        return false;
    };
    ring_last == segment_first
        || ring_last == segment_last
        || ring_first == segment_last
        || ring_first == segment_first
}

fn join_segment(ring: &mut Vec<i64>, mut segment: Vec<i64>) {
    let ring_first = *ring.first().expect("ring has first");
    let ring_last = *ring.last().expect("ring has last");
    let segment_first = *segment.first().expect("segment has first");
    let segment_last = *segment.last().expect("segment has last");

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

fn admin_boundary_parts(tags: &BTreeMap<String, String>) -> Option<(PlaceLayer, u8, String)> {
    if tag_value(tags, "boundary").as_deref() != Some("administrative") {
        return None;
    }
    let admin_level = tag_value(tags, "admin_level")?.parse::<u8>().ok()?;
    let layer = admin_level_layer(admin_level)?;
    let name = tag_value(tags, "name")?;
    Some((layer, admin_level, name))
}

fn country_code_from_tags(tags: &BTreeMap<String, String>, layer: PlaceLayer) -> Option<String> {
    if layer == PlaceLayer::Country {
        return tag_value(tags, "ISO3166-1:alpha2")
            .or_else(|| tag_value(tags, "country_code"))
            .map(|value| value.to_ascii_uppercase());
    }

    if layer == PlaceLayer::Region {
        let iso = tag_value(tags, "ISO3166-2")?;
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

fn admin_level_layer(admin_level: u8) -> Option<PlaceLayer> {
    match admin_level {
        2 => Some(PlaceLayer::Country),
        4 => Some(PlaceLayer::Region),
        6 => Some(PlaceLayer::District),
        8 => Some(PlaceLayer::Locality),
        10 => Some(PlaceLayer::Neighbourhood),
        _ => None,
    }
}

fn representative_point(geometry: &MultiPolygon<f64>) -> Option<[f64; 2]> {
    geometry.centroid().map(|point| [point.x(), point.y()])
}

fn choose_boundary<'a>(
    layer: PlaceLayer,
    boundaries: &[&'a AcceptedBoundary],
    source_context: SourceContext<'_>,
) -> Option<&'a AcceptedBoundary> {
    boundaries
        .iter()
        .copied()
        .min_by(|left, right| compare_boundary(layer, left, right, source_context))
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

    right_match
        .cmp(&left_match)
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
    match layer {
        PlaceLayer::Country => source_context.country,
        PlaceLayer::Region => source_context.region,
        PlaceLayer::District => source_context.district,
        PlaceLayer::Locality => source_context.locality,
        PlaceLayer::Neighbourhood => source_context.neighbourhood,
        PlaceLayer::Place => source_context.place,
    }
}

fn set_tuple_layer(tuple: &mut AdminContextTuple, layer: PlaceLayer, record_id: RecordId) {
    match layer {
        PlaceLayer::Country => tuple.country_record_id = Some(record_id),
        PlaceLayer::Region => tuple.region_record_id = Some(record_id),
        PlaceLayer::District => tuple.district_record_id = Some(record_id),
        PlaceLayer::Locality => tuple.locality_record_id = Some(record_id),
        PlaceLayer::Neighbourhood => tuple.neighbourhood_record_id = Some(record_id),
        PlaceLayer::Place => tuple.place_record_id = Some(record_id),
    }
}

fn envelope_from_rect(rect: Rect<f64>) -> AABB<[f64; 2]> {
    AABB::from_corners([rect.min().x, rect.min().y], [rect.max().x, rect.max().y])
}

fn point_coordinates(geometry: &geojson::Geometry) -> Option<[f64; 2]> {
    let GeometryValue::Point { coordinates } = &geometry.value else {
        return None;
    };
    let [lon, lat, ..] = coordinates.as_slice() else {
        return None;
    };
    Some([*lon, *lat])
}

fn tag_value(tags: &BTreeMap<String, String>, key: &str) -> Option<String> {
    tags.get(key).and_then(|value| clean_text(value))
}

fn clean_text(value: &str) -> Option<String> {
    let cleaned = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

fn normalize(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn same_text(left: &str, right: &str) -> bool {
    normalize(left) == normalize(right)
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

        assert_eq!(assignment.admin_context.locality_record_id, Some(1));
        assert_eq!(assignment.flags, 0);
    }

    #[test]
    fn shared_edge_sets_ambiguous_flag_and_chooses_stable_boundary() {
        let left = square_boundary(10, "Left", PlaceLayer::Locality, -1.0, -1.0, 0.0, 1.0);
        let right = square_boundary(11, "Right", PlaceLayer::Locality, 0.0, -1.0, 1.0, 1.0);
        let index = BoundaryIndex::new(vec![right, left]);

        let assignment = index.context_for_point(0.0, 0.0, SourceContext::default());

        assert_eq!(assignment.admin_context.locality_record_id, Some(10));
        assert_eq!(assignment.flags, CONTEXT_FLAG_AMBIGUOUS_ADMIN);
    }

    #[test]
    fn region_boundary_can_infer_country_from_iso3166_2() {
        let mut region =
            square_boundary(20, "Ontario", PlaceLayer::Region, -90.0, 40.0, -70.0, 50.0);
        region.inferred_country_record_id = Some(7);
        let index = BoundaryIndex::new(vec![region]);

        let assignment = index.context_for_point(-79.0, 43.0, SourceContext::default());

        assert_eq!(assignment.admin_context.country_record_id, Some(7));
        assert_eq!(assignment.admin_context.region_record_id, Some(20));
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
            admin_level: 8,
            inferred_country_record_id: None,
            source_object_type: OsmObjectType::Relation,
            source_object_id: record_id as i64,
            area: geometry.unsigned_area(),
            geometry,
        }
    }
}
