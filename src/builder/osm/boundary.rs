use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
};

use geo::{
    Area, BoundingRect, Centroid, Contains, Covers, LineString, MultiPolygon, Point, Polygon, Rect,
};
use rstar::{AABB, RTree, RTreeObject};

use crate::{
    builder::report::BuilderReport,
    context::{AdminContextTuple, CONTEXT_FLAG_AMBIGUOUS_ADMIN, RecordContext},
    pack::RecordId,
    parse::lattice::CountryCode,
    record::{OsmObjectType, PlaceLayer, PlaceRecord, Record, SourceProvenance, point_geometry},
    util::text::normalize_for_compare,
};

use super::tags::OsmTags;

/// Tags a boundary keeps after the scan.
const BOUNDARY_TAG_KEYS: [&str; 6] = [
    "ISO3166-1:alpha2",
    "ISO3166-2",
    "admin_level",
    "boundary",
    "country_code",
    "name",
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
}

impl BoundarySet {
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
            report.accept_place(boundary.layer);
            records.push((
                ContextOrigin::Boundary(index as u32),
                Record::Place(
                    boundary.layer,
                    PlaceRecord {
                        name: boundary.name.clone(),
                        place_type: format!("admin_level:{}", boundary.admin_level),
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

    /// Names of the records admin context tuples can point at, by record id.
    pub(crate) fn context_names(
        &self,
        boundary_record_ids: &[RecordId],
        country_record_ids: &HashMap<String, RecordId>,
    ) -> HashMap<RecordId, String> {
        let boundaries = self
            .candidates
            .iter()
            .zip(boundary_record_ids)
            .map(|(candidate, record_id)| (*record_id, candidate.boundary.name.clone()));
        let countries = self.derived_countries.iter().filter_map(|(code, record)| {
            Some((*country_record_ids.get(code)?, record.name.clone()))
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

    /// Index the boundaries under their final record ids.
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
                    area: boundary.geometry.unsigned_area(),
                    inferred_country_record_id: boundary
                        .inferred_country_code
                        .as_deref()
                        .and_then(|code| country_record_ids.get(code).copied()),
                    name: boundary.name,
                    admin_level: boundary.admin_level,
                    source_object_type: candidate.source_object_type,
                    source_object_id: candidate.source_object_id,
                    geometry: boundary.geometry,
                }
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
    ) -> RecordContext {
        if self.boundaries.is_empty() || !lon.is_finite() || !lat.is_finite() {
            return RecordContext::default();
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

        RecordContext {
            admin_context: tuple,
            flags,
        }
    }
}

pub(crate) fn has_admin_boundary_tags(tags: &BTreeMap<String, String>) -> bool {
    admin_boundary_parts(tags).is_some()
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
/// `None` when a member could not be fully resolved.
pub(crate) fn build_boundaries(
    ways: &[BoundaryWay],
    relations: &[BoundaryRelationStub],
    member_lines: &HashMap<i64, Option<&[Vertex]>>,
) -> BoundarySet {
    let mut candidates = Vec::new();
    for way in ways {
        if let Some(boundary) = boundary_from_way(way) {
            candidates.push(BoundaryCandidate {
                source_object_type: OsmObjectType::Way,
                source_object_id: way.object_id,
                boundary,
            });
        }
    }
    for stub in relations {
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
    assign_layers_by_containment(&mut candidates);
    let derived_countries = derived_country_records(&candidates);
    BoundarySet {
        candidates,
        derived_countries,
    }
}

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
        if candidate.boundary.layer != PlaceLayer::Region {
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
    let (layer, admin_level, name) = admin_boundary_parts(&way.tags)?;
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
        name,
        inferred_country_code,
        geometry,
        representative_point,
    })
}

fn boundary_from_relation(
    stub: &BoundaryRelationStub,
    member_lines: &HashMap<i64, Option<&[Vertex]>>,
) -> Option<BuiltBoundary> {
    let (layer, admin_level, name) = admin_boundary_parts(&stub.tags)?;
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
        name,
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

fn admin_boundary_parts(tags: &BTreeMap<String, String>) -> Option<(PlaceLayer, u8, String)> {
    if tags.cleaned("boundary").as_deref() != Some("administrative") {
        return None;
    }
    let admin_level = tags.cleaned("admin_level")?.parse::<u8>().ok()?;
    let layer = admin_level_layer(admin_level)?;
    let name = tags.cleaned("name")?;
    Some((layer, admin_level, name))
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

/// The provisional layer for an admin level, before containment is known.
///
/// `admin_level` is not comparable between countries: Australia puts its
/// suburbs and localities at 9 and uses no 8 at all, Germany fills 8, 9 and 10,
/// and a level 6 is a county in most of Poland but a city in Warsaw. Mapping
/// fixed numbers to fixed layers therefore drops whole layers in some
/// countries, so this is only a starting guess that
/// [`assign_layers_by_containment`] corrects once the set is known.
fn admin_level_layer(admin_level: u8) -> Option<PlaceLayer> {
    if let Some(layer) = conventional_admin_layer(admin_level) {
        return Some(layer);
    }
    // Off-convention levels are kept so that [`assign_layers_by_containment`]
    // can place them. Without this an Australian extract has no locality layer
    // at all, because every suburb sits at level 9. The provisional layer is
    // the innermost one, so a level that finds no free slot stays there.
    match admin_level {
        3..=11 => Some(PlaceLayer::Neighbourhood),
        _ => None,
    }
}

/// Layers that admin boundaries are ranked into, outermost first. `Place` is
/// left out: it belongs to place nodes, not to administrative boundaries.
const ADMIN_LAYER_ORDER: [PlaceLayer; 5] = [
    PlaceLayer::Country,
    PlaceLayer::Region,
    PlaceLayer::District,
    PlaceLayer::Locality,
    PlaceLayer::Neighbourhood,
];

/// Promote the levels the default mapping has no slot for into whichever
/// layers a country leaves empty.
///
/// The default numbers carry real meaning where a country follows the
/// convention, so they are kept: a level 8 stays a locality, a level 10 stays a
/// neighbourhood. Only the off-convention levels move, and only into a layer
/// that nothing else in that country fills. An Australian extract has every
/// suburb at level 9 and nothing at 8, so 9 becomes the locality layer that
/// address queries match on; a German extract fills 8, 9 and 10, so 8 keeps
/// locality and 9 settles below it.
///
/// Nominatim arrives at the same behaviour from the other side, with a default
/// number-to-rank table plus a per-country override list, and a rank that
/// shifts when a boundary of the same rank already contains it. Filling gaps
/// needs no table to maintain, so a country nobody has written an override for
/// still gets a locality layer.
///
/// Gaps are counted **per country**, because a level means different things on
/// either side of a border, and per country rather than per individual chain so
/// that one sparsely mapped region cannot classify differently to its
/// neighbours — the usual way a relative-depth scheme goes wrong.
fn assign_layers_by_containment(candidates: &mut [BoundaryCandidate]) {
    let countries = country_of_each_boundary(candidates);

    // Layers each country already fills with a conventional level, and the
    // off-convention levels waiting for a slot.
    let mut filled: BTreeMap<Option<String>, BTreeSet<PlaceLayer>> = BTreeMap::new();
    let mut unconventional: BTreeMap<Option<String>, BTreeSet<u8>> = BTreeMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let country = countries[index].clone();
        match conventional_admin_layer(candidate.boundary.admin_level) {
            Some(layer) => {
                filled.entry(country).or_default().insert(layer);
            }
            None => {
                unconventional
                    .entry(country)
                    .or_default()
                    .insert(candidate.boundary.admin_level);
            }
        }
    }

    // Walk each country's spare levels outermost first, handing each the next
    // empty layer below the deepest conventional one it sits under.
    let mut promoted: BTreeMap<(Option<String>, u8), PlaceLayer> = BTreeMap::new();
    for (country, levels) in &unconventional {
        let country_filled = filled.get(country).cloned().unwrap_or_default();
        for &level in levels {
            // The innermost conventional layer this level nests inside.
            let parent_depth = ADMIN_LAYER_ORDER
                .iter()
                .enumerate()
                .filter(|(_, layer)| {
                    country_filled.contains(layer)
                        && conventional_admin_layer_depth(layer).is_some_and(|depth| depth < level)
                })
                .map(|(index, _)| index)
                .max();
            // The first empty layer below it, so a promoted level never
            // displaces a conventional one.
            let slot = ADMIN_LAYER_ORDER
                .iter()
                .skip(parent_depth.map_or(0, |depth| depth + 1))
                .find(|layer| !country_filled.contains(layer))
                .copied()
                .unwrap_or(PlaceLayer::Neighbourhood);
            promoted.insert((country.clone(), level), slot);
        }
    }

    for (index, candidate) in candidates.iter_mut().enumerate() {
        let level = candidate.boundary.admin_level;
        if conventional_admin_layer(level).is_some() {
            continue;
        }
        if let Some(layer) = promoted.get(&(countries[index].clone(), level)) {
            candidate.boundary.layer = *layer;
        }
    }
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

/// The admin level a layer conventionally corresponds to, for comparing an
/// off-convention level against the layers around it.
fn conventional_admin_layer_depth(layer: &PlaceLayer) -> Option<u8> {
    match layer {
        PlaceLayer::Country => Some(2),
        PlaceLayer::Region => Some(4),
        PlaceLayer::District => Some(6),
        PlaceLayer::Locality => Some(8),
        PlaceLayer::Neighbourhood => Some(10),
        PlaceLayer::Place => None,
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
        .filter(|candidate| candidate.boundary.admin_level <= COUNTRY_ADMIN_LEVEL)
        .filter_map(|candidate| {
            candidate
                .boundary
                .inferred_country_code
                .clone()
                .map(|code| (&candidate.boundary.geometry, code))
        })
        .collect::<Vec<_>>();

    // With no more than one country in the extract there is nothing to tell
    // apart, so skip the point-in-polygon work entirely and group as one.
    let distinct = country_boundaries
        .iter()
        .map(|(_, code)| code.as_str())
        .collect::<BTreeSet<_>>();
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

    /// Builds one candidate per level inside a shared country boundary and
    /// returns the layer each one ends up in.
    fn layers_for_levels(levels: &[u8]) -> BTreeMap<u8, PlaceLayer> {
        let mut candidates = Vec::new();
        for (index, &level) in levels.iter().enumerate() {
            // Each level nests inside the one before it.
            let span = 10.0 - index as f64;
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
                    admin_level: level,
                    name: format!("level {level}"),
                    inferred_country_code: (level <= 2).then(|| "AU".to_string()),
                    representative_point: representative_point(&geometry).expect("point"),
                    geometry,
                },
            });
        }
        assign_layers_by_containment(&mut candidates);
        candidates
            .iter()
            .map(|candidate| (candidate.boundary.admin_level, candidate.boundary.layer))
            .collect()
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
        let set = build_boundaries(&[], &relations, &member_lines);
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
        assert_eq!(context.admin_context.region_record_id, Some(5));
        assert_eq!(context.admin_context.country_record_id, Some(4));
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
