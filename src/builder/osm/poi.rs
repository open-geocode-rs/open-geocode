//! Named points of interest.
//!
//! What counts as a place comes from iD tagging schema presets
//! (`catalog/places.json`, filtered by `catalog/curation.toml`), not a key
//! list here: a new kind of place counts once mappers get a preset for it.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::OnceLock,
};

use geo::MultiPolygon;
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Rectangle},
};
use serde::Deserialize;

use crate::{
    record::{
        AddressComponents, AddressRecord, Brand, LocationPrecision, ObjectType, PoiRecord, Record,
        SourceProvenance, point_geometry,
    },
    util::fold::search_key,
};

use super::{
    address::address_components,
    containment,
    emitted::Emitted,
    tags::{OsmTags, alternate_names, item, name_tags, prominence},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Drawn {
    Point,
    Area,
}

impl Drawn {
    pub(crate) const fn of(object_type: ObjectType) -> Self {
        match object_type {
            ObjectType::Node => Self::Point,
            _ => Self::Area,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Catalog {
    keys: Vec<String>,
    presets: BTreeMap<String, CatalogPreset>,
}

#[derive(Debug, Deserialize)]
struct CatalogPreset {
    tags: BTreeMap<String, String>,
    geometry: Vec<String>,
    #[serde(default = "default_match_score")]
    match_score: f64,
    #[serde(default)]
    needs_wikidata: bool,
    /// Never a place: an object it matches best is none.
    #[serde(default)]
    never: bool,
}

const fn default_match_score() -> f64 {
    1.0
}

#[derive(Debug)]
struct Preset {
    key: String,
    tags: Vec<(String, String)>,
    point: bool,
    area: bool,
    match_score: f64,
    needs_wikidata: bool,
    never: bool,
}

// iD's `key=*` matches any value but `no`.
const ANY: &str = "*";

#[derive(Debug)]
struct Presets {
    all: Vec<Preset>,
    by_tag: HashMap<(String, String), Vec<usize>>,
    keys: HashSet<String>,
    kinds: HashSet<String>,
}

fn presets() -> &'static Presets {
    static PRESETS: OnceLock<Presets> = OnceLock::new();
    PRESETS.get_or_init(|| {
        let catalog: Catalog = serde_json::from_str(include_str!("../../../catalog/places.json"))
            .expect("catalog/places.json is a place catalog");
        let mut presets = Presets {
            all: Vec::new(),
            by_tag: HashMap::new(),
            keys: HashSet::new(),
            kinds: catalog.keys.iter().cloned().collect(),
        };
        for (id, preset) in catalog.presets {
            let index = presets.all.len();
            for (key, value) in &preset.tags {
                presets.keys.insert(key.clone());
                presets
                    .by_tag
                    .entry((key.clone(), value.clone()))
                    .or_default()
                    .push(index);
            }
            let has = |drawing: &str| preset.geometry.iter().any(|kind| kind == drawing);
            presets.all.push(Preset {
                key: id.split('/').next().unwrap_or_default().to_string(),
                tags: preset.tags.into_iter().collect(),
                point: has("point") || has("vertex"),
                area: has("area") || has("line"),
                match_score: preset.match_score,
                needs_wikidata: preset.needs_wikidata,
                never: preset.never,
            });
        }
        presets
    })
}

pub(crate) fn is_place_key(key: &str) -> bool {
    presets().keys.contains(key)
}

/// iD's order: most tags of a stated value, then most tags, then match
/// score. Counted wins a tie with never counted (bench beside a bakery is the
/// bakery). Best never counted means no place (`shop=vacant` is no `shop=*`).
fn place_preset(tags: &BTreeMap<String, String>, drawn: Drawn) -> Option<&'static Preset> {
    let presets = presets();
    let linked = tags.contains_key("wikidata");
    let mut best: Option<((usize, usize, f64, bool), usize)> = None;
    for (key, value) in tags {
        let candidates = [
            presets.by_tag.get(&(key.clone(), value.clone())),
            presets.by_tag.get(&(key.clone(), ANY.to_string())),
        ];
        for &index in candidates.into_iter().flatten().flatten() {
            let preset = &presets.all[index];
            let drawn_so = match drawn {
                Drawn::Point => preset.point,
                Drawn::Area => preset.area,
            };
            let tagged = preset.tags.iter().all(|(key, wanted)| {
                tags.get(key).is_some_and(|value| match wanted.as_str() {
                    ANY => value != "no",
                    wanted => value == wanted,
                })
            });
            if !drawn_so || !tagged || (preset.needs_wikidata && !linked) {
                continue;
            }
            let stated = preset.tags.iter().filter(|(_, value)| value != ANY).count();
            let rank = (stated, preset.tags.len(), preset.match_score, !preset.never);
            let better = best.is_none_or(|(best_rank, best_index)| {
                rank.partial_cmp(&best_rank) == Some(std::cmp::Ordering::Greater)
                    || (rank == best_rank && index < best_index)
            });
            if better {
                best = Some((rank, index));
            }
        }
    }
    let (_, index) = best?;
    let preset = &presets.all[index];
    (!preset.never).then_some(preset)
}

fn poi_category(tags: &BTreeMap<String, String>, drawn: Drawn) -> Option<String> {
    let preset = place_preset(tags, drawn)?;
    let value = tags.get(&preset.key)?;
    Some(format!("{}:{value}", preset.key))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PoiCandidate {
    pub object_type: ObjectType,
    pub object_id: i64,
    pub lat: f64,
    pub lon: f64,
    pub location_precision: LocationPrecision,
    /// The tags [`poi_tags`] keeps.
    pub tags: BTreeMap<String, String>,
    /// See [`Identity::outline`].
    pub outline: Option<Vec<u8>>,
    /// See [`Identity::nodes`].
    pub nodes: Vec<i64>,
}

pub(crate) fn is_poi(tags: &BTreeMap<String, String>, drawn: Drawn) -> bool {
    poi_name(tags).is_some() && poi_category(tags, drawn).is_some()
}

pub(crate) fn count_left_out(tags: &BTreeMap<String, String>, drawn: Drawn, out: &mut Emitted) {
    if poi_name(tags).is_none() || poi_category(tags, drawn).is_some() {
        return;
    }
    let kinds = &presets().kinds;
    if let Some((key, value)) = tags.iter().find(|(key, _)| kinds.contains(key.as_str())) {
        out.report.leave_out_place(key, value);
    }
}

/// What tells a place from another of its name, kept until places resolve.
/// The Pack stores none of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Identity {
    /// Its outline as a [`containment`] shape block; none for a point or a line.
    pub outline: Option<Vec<u8>>,
    /// The nodes it is drawn with, sorted: a node's own id.
    pub nodes: Vec<i64>,
    pub websites: Vec<String>,
    pub phones: Vec<String>,
    pub refs: Vec<String>,
}

/// A place as it waits to resolve.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Place {
    pub record: PoiRecord,
    pub identity: Identity,
}

/// The shape block of `geometry`, for [`Identity::outline`].
pub(crate) fn outline(geometry: &MultiPolygon<f64>) -> Vec<u8> {
    let mut block = Vec::new();
    containment::encode_shape(geometry, &mut block);
    block
}

/// Entity resolution blocked by name, on exact keys, never distance: same
/// name and distance mostly join different things (chalets 9B and 9D, two
/// pubs facing each other, both sides of a bus stop). Two records are one
/// place when nothing conflicts and one holds: the same Wikidata item or
/// kind with one's point inside the other's outline; the same kind and
/// address; the same kind drawn with a shared node, as a canal in pieces.
/// A shared item alone is not enough: an organisation tags every office
/// with its own. A wrong merge hides a real place, a missed one only shows
/// it twice. Each joins the leader, never another member (center
/// clustering), so a run cannot chain. Merged ids stay as aliases.
pub(crate) fn resolve(mut places: Vec<Place>) -> Vec<PoiRecord> {
    if places.len() < 2 {
        return places.into_iter().map(|place| place.record).collect();
    }
    let drawn = |place: &PoiRecord| match place.source.object_type {
        ObjectType::Relation => 2,
        ObjectType::Way => 1,
        ObjectType::Node | ObjectType::Row => 0,
    };
    places.sort_by(|left, right| {
        let (left, right) = (&left.record, &right.record);
        right
            .prominence
            .cmp(&left.prominence)
            .then(drawn(right).cmp(&drawn(left)))
            .then(left.source.object_type.cmp(&right.source.object_type))
            .then(left.source.object_id.cmp(&right.source.object_id))
    });
    let mut leaders: Vec<Place> = Vec::new();
    let mut addresses = HashMap::<(String, AddressKey), Vec<usize>>::new();
    let mut nodes = HashMap::<i64, Vec<usize>>::new();
    // A chain's name group holds thousands of copies: the boxes keep each
    // test to the outlines that could cover it.
    let mut outlines = RTree::<GeomWithData<Rectangle<[f64; 2]>, usize>>::new();
    let mut points = RTree::<GeomWithData<[f64; 2], usize>>::new();
    for place in places {
        let Some(point) = point_of(&place.record) else {
            leaders.push(place);
            continue;
        };
        let address = place.record.address.as_ref().and_then(address_key);
        let extent = place.identity.outline.as_deref().map(containment::extent);
        let mut near = address
            .as_ref()
            .and_then(|key| addresses.get(&(place.record.category.clone(), key.clone())))
            .into_iter()
            .flatten()
            .chain(
                place
                    .identity
                    .nodes
                    .iter()
                    .filter_map(|node| nodes.get(node))
                    .flatten(),
            )
            .copied()
            .chain(
                outlines
                    .locate_all_at_point(&point)
                    .map(|leader| leader.data),
            )
            .chain(extent.into_iter().flat_map(|[west, south, east, north]| {
                points
                    .locate_in_envelope(&AABB::from_corners([west, south], [east, north]))
                    .map(|leader| leader.data)
            }))
            .collect::<Vec<_>>();
        near.sort_unstable();
        near.dedup();
        match near
            .into_iter()
            .find(|&leader| one_place(&leaders[leader], &place, point))
        {
            Some(at) => {
                let leader = &mut leaders[at];
                let unaddressed = leader.record.address.is_none();
                for (own, theirs) in [
                    (&mut leader.identity.websites, place.identity.websites),
                    (&mut leader.identity.phones, place.identity.phones),
                    (&mut leader.identity.refs, place.identity.refs),
                ] {
                    if own.is_empty() {
                        *own = theirs;
                    }
                }
                merge(&mut leader.record, place.record);
                // An address the leader takes from a member is its own now:
                // a later copy stating it joins.
                if unaddressed
                    && let Some(key) = leader.record.address.as_ref().and_then(address_key)
                {
                    addresses
                        .entry((leader.record.category.clone(), key))
                        .or_default()
                        .push(at);
                }
            }
            None => {
                let index = leaders.len();
                if let Some(key) = address {
                    addresses
                        .entry((place.record.category.clone(), key))
                        .or_default()
                        .push(index);
                }
                for &node in &place.identity.nodes {
                    nodes.entry(node).or_default().push(index);
                }
                if let Some([west, south, east, north]) = extent {
                    outlines.insert(GeomWithData::new(
                        Rectangle::from_corners([west, south], [east, north]),
                        index,
                    ));
                }
                points.insert(GeomWithData::new(point, index));
                leaders.push(place);
            }
        }
    }
    leaders.into_iter().map(|place| place.record).collect()
}

/// Whether `member`, at `point`, is the place `leader` is.
fn one_place(leader: &Place, member: &Place, point: [f64; 2]) -> bool {
    if conflict(leader, member) {
        return false;
    }
    let (own, theirs) = (&leader.record, &member.record);
    let kind = own.category == theirs.category;
    let item = own.wikidata.is_some() && own.wikidata == theirs.wikidata;
    let covers = |outline: &Option<Vec<u8>>, [lon, lat]: [f64; 2]| {
        outline
            .as_deref()
            .is_some_and(|block| containment::covers(block, lon, lat))
    };
    let within = || {
        covers(&leader.identity.outline, point)
            || point_of(own).is_some_and(|at| covers(&member.identity.outline, at))
    };
    let addressed = || {
        let key = own.address.as_ref().and_then(address_key);
        key.is_some() && key == theirs.address.as_ref().and_then(address_key)
    };
    let touch = || {
        member
            .identity
            .nodes
            .iter()
            .any(|node| leader.identity.nodes.binary_search(node).is_ok())
    };
    ((kind || item) && within()) || (kind && (addressed() || touch()))
}

/// Two records that state different addresses, items, websites, phones or
/// refs are two places, whatever else they share: chains share a name, an
/// organisation its item.
fn conflict(leader: &Place, member: &Place) -> bool {
    let (own, theirs) = (&leader.record, &member.record);
    let addresses = own.address.as_ref().zip(theirs.address.as_ref());
    let items = own.wikidata.zip(theirs.wikidata);
    let disjoint = |own: &[String], theirs: &[String]| {
        !own.is_empty() && !theirs.is_empty() && !own.iter().any(|value| theirs.contains(value))
    };
    addresses.is_some_and(|(own, theirs)| !same_address(own, theirs))
        || items.is_some_and(|(own, theirs)| own != theirs)
        || disjoint(&leader.identity.websites, &member.identity.websites)
        || disjoint(&leader.identity.phones, &member.identity.phones)
        || disjoint(&leader.identity.refs, &member.identity.refs)
}

/// What an address puts its house at, as search spells it: house number,
/// street or place, unit. An address of the block scheme names no street, so
/// its areas stand in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AddressKey {
    number: String,
    street: Option<String>,
    place: Option<String>,
    unit: Option<String>,
    areas: Vec<String>,
}

fn address_key(address: &AddressComponents) -> Option<AddressKey> {
    let key = |value: &Option<String>| value.as_deref().and_then(search_key);
    let (street, place) = (key(&address.street), key(&address.place));
    let areas = match (&street, &place) {
        (None, None) => address
            .areas
            .iter()
            .filter_map(|area| search_key(area))
            .collect(),
        _ => Vec::new(),
    };
    if street.is_none() && place.is_none() && areas.is_empty() {
        return None;
    }
    Some(AddressKey {
        number: search_key(&address.number)?.replace(' ', ""),
        street,
        place,
        unit: key(&address.unit),
        areas,
    })
}

/// A postcode or town only one of two states says nothing; two that differ
/// tell the same street and number in two towns apart.
fn same_address(own: &AddressComponents, theirs: &AddressComponents) -> bool {
    let agree = |own: &Option<String>, theirs: &Option<String>| {
        let key = |value: &Option<String>| {
            value
                .as_deref()
                .and_then(search_key)
                .map(|value| value.replace(' ', ""))
        };
        own.is_none() || theirs.is_none() || key(own) == key(theirs)
    };
    address_key(own) == address_key(theirs)
        && agree(&own.postcode, &theirs.postcode)
        && agree(&own.locality, &theirs.locality)
}

fn point_of(place: &PoiRecord) -> Option<[f64; 2]> {
    crate::util::geo::point_lon_lat(&place.geometry)
}

fn merge(leader: &mut PoiRecord, member: PoiRecord) {
    if member.source.object_type != ObjectType::Row {
        leader
            .aliases
            .push((member.source.object_type, member.source.object_id));
    }
    leader.aliases.extend(member.aliases);
    if leader.address.is_none() {
        leader.address = member.address;
    }
    if leader.wikidata.is_none() {
        leader.wikidata = member.wikidata;
    }
    if leader.brand.is_none() {
        leader.brand = member.brand;
    }
    leader.prominence = leader.prominence.max(member.prominence);
    for name in std::iter::once(member.name).chain(member.alt_names) {
        let known = std::iter::once(&leader.name)
            .chain(&leader.alt_names)
            .any(|known| known.to_lowercase() == name.to_lowercase());
        if !known {
            leader.alt_names.push(name);
        }
    }
}

fn brand(tags: &BTreeMap<String, String>) -> Option<Brand> {
    Some(Brand {
        name: tags.cleaned("brand")?,
        wikidata: item(tags, "brand:wikidata"),
    })
}

/// The tags a POI keeps: its names, its category tags, its Wikidata and
/// Wikipedia links, what tells it from another of its name, and, when they
/// state a valid address, its `addr:*` tags.
pub(crate) fn poi_tags(
    tags: &BTreeMap<String, String>,
    address_tags: Option<&BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    tags.iter()
        .filter(|(key, _)| {
            matches!(
                key.as_str(),
                "wikidata"
                    | "wikipedia"
                    | "brand"
                    | "brand:wikidata"
                    | "website"
                    | "contact:website"
                    | "phone"
                    | "contact:phone"
                    | "ref"
            ) || is_place_key(key)
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .chain(name_tags(tags))
        .chain(
            address_tags
                .into_iter()
                .flatten()
                .map(|(key, value)| (key.clone(), value.clone())),
        )
        .collect()
}

/// Emit a POI from its kept tags; their `addr:*` tags are already validated.
pub(crate) fn emit_poi(candidate: &PoiCandidate, out: &mut Emitted) {
    let tags = &candidate.tags;
    let drawn = Drawn::of(candidate.object_type);
    let (Some(name), Some(category)) = (poi_name(tags), poi_category(tags, drawn)) else {
        return;
    };
    let address = address_components(tags).ok();
    let source = SourceProvenance::osm(candidate.object_type, candidate.object_id);
    let geometry = point_geometry(candidate.lon, candidate.lat);
    // Address search reads addresses alone: without this it misses every
    // house only a POI states.
    if let Some(address) = &address {
        let record = AddressRecord {
            disputed: false,
            street_postcodes: Vec::new(),
            address: address.clone(),
            geometry: geometry.clone(),
            location_precision: candidate.location_precision,
            source: source.clone(),
        };
        out.report.accept_address_with_tags(&record, Some(tags));
        out.records.push(Record::Address(record));
    }
    let record = PoiRecord {
        alt_names: alternate_names(tags, &name),
        name,
        category,
        address,
        geometry,
        location_precision: candidate.location_precision,
        source,
        prominence: prominence(tags),
        wikidata: item(tags, "wikidata"),
        brand: brand(tags),
        aliases: Vec::new(),
    };
    out.report.accept_poi(&record);
    out.places.push(Place {
        record,
        identity: Identity {
            outline: candidate.outline.clone(),
            nodes: {
                let mut nodes = candidate.nodes.clone();
                nodes.sort_unstable();
                nodes.dedup();
                nodes
            },
            websites: values(tags, &["website", "contact:website"], |value| {
                let value = value.to_lowercase();
                let value = value
                    .strip_prefix("https://")
                    .or_else(|| value.strip_prefix("http://"))
                    .unwrap_or(&value);
                let value = value.strip_prefix("www.").unwrap_or(value);
                Some(value.trim_end_matches('/').to_string())
            }),
            // The last nine digits: one number written with its country
            // code and without.
            phones: values(tags, &["phone", "contact:phone"], |value| {
                let digits = value
                    .chars()
                    .filter(char::is_ascii_digit)
                    .collect::<Vec<_>>();
                let tail = &digits[digits.len().saturating_sub(9)..];
                Some(tail.iter().collect())
            }),
            refs: values(tags, &["ref"], search_key),
        },
    });
}

/// The values of `keys`, each as `spelled` spells it: one tag may hold
/// several, split by `;`.
fn values(
    tags: &BTreeMap<String, String>,
    keys: &[&str],
    spelled: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut values = keys
        .iter()
        .filter_map(|key| tags.get(*key))
        .flat_map(|value| value.split(';'))
        .filter_map(|value| spelled(value.trim()))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    values.sort_unstable();
    values.dedup();
    values
}

/// House numbers and ref codes in `name` are no names.
fn poi_name(tags: &BTreeMap<String, String>) -> Option<String> {
    ["name", "brand"].into_iter().find_map(|key| {
        tags.cleaned(key)
            .filter(|name| name.chars().any(char::is_alphabetic))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn emit(pairs: &[(&str, &str)]) -> Emitted {
        let mut out = Emitted::default();
        emit_poi(
            &PoiCandidate {
                object_type: ObjectType::Node,
                object_id: 7,
                lat: 43.65,
                lon: -79.38,
                location_precision: LocationPrecision::Point,
                tags: poi_tags(&tags(pairs), None),
                outline: None,
                nodes: vec![7],
            },
            &mut out,
        );
        out
    }

    const LAT: f64 = 53.2;

    /// Longitude `m` metres east of 6.56 at [`LAT`].
    fn east(m: f64) -> f64 {
        6.56 + m / 66_660.0
    }

    fn place(id: i64, category: &str, lon: f64) -> Place {
        Place {
            record: PoiRecord {
                alt_names: Vec::new(),
                name: "De Pindol".to_string(),
                category: category.to_string(),
                address: None,
                geometry: point_geometry(lon, LAT),
                location_precision: LocationPrecision::Point,
                source: SourceProvenance::osm(ObjectType::Node, id),
                prominence: Default::default(),
                wikidata: None,
                brand: None,
                aliases: Vec::new(),
            },
            identity: Identity {
                nodes: vec![id],
                ..Identity::default()
            },
        }
    }

    /// `place` drawn as a way: a square `half` metres each side of its point,
    /// on nodes `id * 10 ..`.
    fn outlined(mut place: Place, half: f64) -> Place {
        let [lon, lat] = point_of(&place.record).expect("a point");
        let (dx, dy) = (half / 66_660.0, half / 111_320.0);
        let ring = vec![
            (lon - dx, lat - dy),
            (lon + dx, lat - dy),
            (lon + dx, lat + dy),
            (lon - dx, lat + dy),
            (lon - dx, lat - dy),
        ];
        let id = place.record.source.object_id;
        place.record.source = SourceProvenance::osm(ObjectType::Way, id);
        place.record.location_precision = LocationPrecision::Centroid;
        place.identity.outline = Some(outline(&MultiPolygon::new(vec![geo::Polygon::new(
            ring.into(),
            Vec::new(),
        )])));
        place.identity.nodes = (0..4).map(|node| id * 10 + node).collect();
        place
    }

    fn at(mut place: Place, number: &str, street: &str) -> Place {
        place.record.address = Some(AddressComponents {
            number: number.to_string(),
            street: Some(street.to_string()),
            place: None,
            unit: None,
            locality: None,
            areas: Vec::new(),
            region: None,
            postcode: None,
            country: None,
        });
        place
    }

    fn postcode(mut place: Place, postcode: &str) -> Place {
        if let Some(address) = &mut place.record.address {
            address.postcode = Some(postcode.to_string());
        }
        place
    }

    fn item(mut place: Place, item: u32) -> Place {
        place.record.wikidata = Some(item);
        place
    }

    /// Each resolved place by id, with the ids merged into it.
    fn ids(resolved: Vec<PoiRecord>) -> Vec<(i64, Vec<(ObjectType, i64)>)> {
        let mut ids = resolved
            .into_iter()
            .map(|place| (place.source.object_id, place.aliases))
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    #[track_caller]
    fn assert_apart(places: Vec<Place>) {
        let mut apart = places
            .iter()
            .map(|place| (place.record.source.object_id, Vec::new()))
            .collect::<Vec<_>>();
        apart.sort();
        assert_eq!(ids(resolve(places)), apart);
    }

    #[test]
    fn near_copies_of_one_name_with_nothing_shared_stay_apart() {
        let chalets = vec![
            at(place(1, "tourism:chalet", east(0.0)), "9B", "Hoofdweg"),
            at(place(2, "tourism:chalet", east(25.0)), "9D", "Hoofdweg"),
        ];
        assert_apart(chalets);
        let stops = vec![
            place(1, "highway:bus_stop", east(0.0)),
            place(2, "highway:bus_stop", east(5.0)),
        ];
        assert_apart(stops);
    }

    #[test]
    fn one_kind_at_one_address_is_one_place_however_far_apart() {
        let atm = |id: i64, lon: f64| at(place(id, "amenity:atm", lon), "26", "Vismarkt");
        assert_eq!(
            ids(resolve(vec![atm(1, east(0.0)), atm(2, east(1_000.0))])),
            [(1, vec![(ObjectType::Node, 2)])]
        );
        // Spelling as search spells it.
        assert_eq!(
            ids(resolve(vec![
                atm(1, east(0.0)),
                at(place(2, "amenity:atm", east(3.0)), "26 ", "VISMARKT"),
            ])),
            [(1, vec![(ObjectType::Node, 2)])]
        );
        // A postcode only one states says nothing; two that differ do.
        assert_eq!(
            ids(resolve(vec![
                postcode(atm(1, east(0.0)), "9711 KT"),
                atm(2, east(3.0)),
            ])),
            [(1, vec![(ObjectType::Node, 2)])]
        );
        let towns = vec![
            postcode(atm(1, east(0.0)), "9711 KT"),
            postcode(atm(2, east(3.0)), "9401 AA"),
        ];
        assert_apart(towns);
        let kinds = vec![
            atm(1, east(0.0)),
            at(place(2, "amenity:bank", east(3.0)), "26", "Vismarkt"),
        ];
        assert_apart(kinds);
        let numbers = vec![
            atm(1, east(0.0)),
            at(place(2, "amenity:atm", east(3.0)), "28", "Vismarkt"),
        ];
        assert_apart(numbers);
    }

    #[test]
    fn an_organisations_item_on_each_of_its_offices_keeps_them_apart() {
        let offices = vec![
            item(
                at(place(1, "office:government", east(0.0)), "1", "Guyotplein"),
                7,
            ),
            item(
                at(
                    place(2, "office:government", east(400.0)),
                    "4",
                    "Zuiderpark",
                ),
                7,
            ),
            item(place(3, "office:government", east(900.0)), 7),
        ];
        assert_apart(offices);
    }

    #[test]
    fn a_point_inside_an_outline_of_its_name_is_that_place() {
        let synagogue = |id| place(id, "amenity:place_of_worship", east(0.0));
        let building = outlined(synagogue(10), 20.0);
        assert_eq!(
            ids(resolve(vec![synagogue(1), building.clone()])),
            [(10, vec![(ObjectType::Node, 1)])]
        );
        // The node leads when it is better known; the outline joins it.
        let mut known = item(synagogue(1), 5);
        known.record.prominence.linked = true;
        assert_eq!(
            ids(resolve(vec![known, building.clone()])),
            [(1, vec![(ObjectType::Way, 10)])]
        );
        assert_apart(vec![
            place(1, "amenity:place_of_worship", east(25.0)),
            building,
        ]);
        // A hotel and the restaurant inside it are two.
        let restaurant = vec![
            place(1, "amenity:restaurant", east(5.0)),
            outlined(place(10, "tourism:hotel", east(0.0)), 20.0),
        ];
        assert_apart(restaurant);
        // One item is one place whatever kind each copy says it is.
        assert_eq!(
            ids(resolve(vec![
                item(place(1, "amenity:university", east(5.0)), 7),
                item(
                    outlined(place(10, "building:university", east(0.0)), 20.0),
                    7
                ),
            ])),
            [(10, vec![(ObjectType::Node, 1)])]
        );
        // So far east a degree is narrow and the cells of a grid were not.
        let tokyo = |mut place: Place, lon: f64| {
            place.record.geometry = point_geometry(lon, 35.68);
            place
        };
        assert_eq!(
            ids(resolve(vec![
                outlined(tokyo(synagogue(10), 139.7), 20.0),
                tokyo(synagogue(1), 139.700_2),
            ])),
            [(10, vec![(ObjectType::Node, 1)])]
        );
    }

    #[test]
    fn pieces_of_one_line_that_meet_are_one_place() {
        let piece = |id: i64, lon: f64, nodes: Vec<i64>| {
            let mut piece = place(id, "waterway:canal", lon);
            piece.record.source = SourceProvenance::osm(ObjectType::Way, id);
            piece.identity.nodes = nodes;
            piece
        };
        assert_eq!(
            ids(resolve(vec![
                piece(1, east(0.0), vec![100, 101, 102]),
                piece(2, east(800.0), vec![102, 103]),
            ])),
            [(1, vec![(ObjectType::Way, 2)])]
        );
        let apart_pieces = vec![
            piece(1, east(0.0), vec![100, 101]),
            piece(2, east(800.0), vec![102, 103]),
        ];
        assert_apart(apart_pieces);
    }

    #[test]
    fn copies_stating_different_websites_or_refs_stay_apart() {
        let station = |id: i64, website: &str| {
            let mut station = at(place(id, "amenity:fuel", east(id as f64)), "2", "Peizerweg");
            station.identity.websites = vec![website.to_string()];
            station
        };
        let stations = vec![station(1, "shell.nl/a"), station(2, "shell.nl/b")];
        assert_apart(stations);
        assert_eq!(
            ids(resolve(vec![
                station(1, "shell.nl/a"),
                station(2, "shell.nl/a")
            ])),
            [(1, vec![(ObjectType::Node, 2)])]
        );
        let reffed = |id: i64, reference: &str| {
            let mut copy = place(id, "amenity:fuel", east(0.0));
            copy.identity.refs = vec![reference.to_string()];
            copy
        };
        let building = outlined(reffed(10, "7"), 20.0);
        let refs = vec![reffed(1, "8"), building];
        assert_apart(refs);
        let items = vec![
            item(place(1, "amenity:fuel", east(0.0)), 1),
            item(outlined(place(10, "amenity:fuel", east(0.0)), 20.0), 2),
        ];
        assert_apart(items);
    }

    #[test]
    fn an_address_a_leader_takes_from_a_copy_is_its_own() {
        let cafe = |id, lon| place(id, "amenity:cafe", lon);
        assert_eq!(
            ids(resolve(vec![
                outlined(cafe(10, east(0.0)), 20.0),
                at(cafe(1, east(0.0)), "82", "Dorpstraat"),
                at(cafe(2, east(1_000.0)), "82", "Dorpstraat"),
            ])),
            [(10, vec![(ObjectType::Node, 1), (ObjectType::Node, 2)])]
        );
    }

    #[test]
    fn a_place_resolves_alone_and_without_a_point_stays() {
        assert_eq!(ids(resolve(Vec::new())), []);
        let mut lost = place(1, "amenity:cafe", east(0.0));
        lost.record.geometry = geojson::Geometry::new(geojson::GeometryValue::LineString {
            coordinates: Vec::new(),
        });
        assert_eq!(
            ids(resolve(vec![
                lost,
                at(place(2, "amenity:cafe", east(0.0)), "1", "A")
            ])),
            [(1, Vec::new()), (2, Vec::new())]
        );
    }

    #[test]
    fn categorizes_by_the_preset_its_tags_match_best() {
        let point = |pairs: &[(&str, &str)]| poi_category(&tags(pairs), Drawn::Point);
        let area = |pairs: &[(&str, &str)]| poi_category(&tags(pairs), Drawn::Area);
        // A school's building preset scores below the school's.
        assert_eq!(
            area(&[("building", "school"), ("amenity", "school")]).as_deref(),
            Some("amenity:school")
        );
        assert_eq!(
            point(&[("man_made", "lighthouse")]).as_deref(),
            Some("man_made:lighthouse")
        );
        assert_eq!(
            point(&[("highway", "bus_stop")]).as_deref(),
            Some("highway:bus_stop")
        );
        assert_eq!(
            point(&[("natural", "peak")]).as_deref(),
            Some("natural:peak")
        );
        assert_eq!(point(&[("natural", "water"), ("water", "lake")]), None);
        assert_eq!(
            area(&[("natural", "water"), ("water", "lake")]).as_deref(),
            Some("natural:water")
        );
    }

    #[test]
    fn excludes_noise_unnamed_and_unlisted_objects() {
        let drawn_named = |pairs: &[(&str, &str)], drawn| {
            let mut tags = tags(pairs);
            tags.insert("name".to_string(), "Example".to_string());
            is_poi(&tags, drawn)
        };
        let named = |pairs: &[(&str, &str)]| drawn_named(pairs, Drawn::Point);
        assert!(named(&[("amenity", "cafe")]));
        assert!(!named(&[("amenity", "bench")]));
        assert!(!named(&[("amenity", "waste_basket")]));
        assert!(!named(&[("leisure", "picnic_table")]));
        assert!(!named(&[("shop", "vacant")]));
        assert!(!named(&[("aeroway", "runway")]));
        assert!(!named(&[("railway", "level_crossing")]));
        assert!(named(&[("railway", "station")]));
        assert!(named(&[("railway", "halt")]));
        assert!(!named(&[
            ("tourism", "information"),
            ("information", "board")
        ]));
        assert!(named(&[
            ("tourism", "information"),
            ("information", "office")
        ]));
        assert!(!named(&[("highway", "residential")]));
        assert!(!named(&[("landuse", "residential")]));
        // A bench next to a shop is still the shop.
        assert!(named(&[("amenity", "bench"), ("shop", "bakery")]));
        assert!(!drawn_named(&[("building", "yes")], Drawn::Area));
        assert!(drawn_named(
            &[("building", "yes"), ("wikidata", "Q1")],
            Drawn::Area
        ));

        assert!(
            !is_poi(&tags(&[("amenity", "cafe")]), Drawn::Point),
            "unnamed"
        );
        assert!(!is_poi(
            &tags(&[("amenity", "cafe"), ("name", "  ")]),
            Drawn::Point
        ));
        assert!(!is_poi(
            &tags(&[("amenity", "cafe"), ("name", "12")]),
            Drawn::Point
        ));
    }

    #[test]
    fn keeps_only_poi_and_valid_address_tags() {
        let source = tags(&[
            ("name", "Tim Hortons"),
            ("amenity", "cafe"),
            ("cuisine", "coffee_shop"),
            ("addr:housenumber", "123"),
        ]);
        let address = tags(&[
            ("addr:housenumber", "123"),
            ("addr:street", "King Street West"),
        ]);
        // `cuisine` is kept: presets tell kinds of restaurant apart by it.
        assert_eq!(
            poi_tags(&source, None),
            tags(&[
                ("name", "Tim Hortons"),
                ("amenity", "cafe"),
                ("cuisine", "coffee_shop")
            ])
        );
        assert_eq!(poi_tags(&source, Some(&address)).len(), 5);
    }

    #[test]
    fn emits_its_address_as_an_address_and_itself_as_a_poi() {
        let mut out = Emitted::default();
        let source = tags(&[("name", " Tim  Hortons "), ("amenity", "cafe")]);
        let address = tags(&[
            ("addr:housenumber", "123"),
            ("addr:street", "King Street West"),
            ("addr:postcode", "M5V 1A1"),
        ]);
        emit_poi(
            &PoiCandidate {
                object_type: ObjectType::Way,
                object_id: 9,
                lat: 43.65,
                lon: -79.38,
                location_precision: LocationPrecision::Centroid,
                tags: poi_tags(&source, Some(&address)),
                outline: None,
                nodes: vec![90, 91],
            },
            &mut out,
        );
        let ([Record::Address(address)], [Place { record, identity }]) =
            (out.records.as_slice(), out.places.as_slice())
        else {
            panic!(
                "an address and a POI, got {:?} {:?}",
                out.records, out.places
            );
        };
        assert_eq!(address.id(), "osm:way:9");
        assert_eq!(address.address.number, "123");
        assert_eq!(address.location_precision, LocationPrecision::Centroid);
        assert_eq!(record.id(), "osm:way:9");
        assert_eq!(record.name, "Tim Hortons");
        assert_eq!(record.category, "amenity:cafe");
        assert_eq!(record.label(), "Tim Hortons, 123 King Street West, M5V 1A1");
        assert_eq!(out.report.accepted.by_layer.get("poi"), Some(&1));
        assert_eq!(out.report.accepted.by_layer.get("address"), Some(&1));
        assert_eq!(out.report.accepted.pois_with_address, 1);
        assert_eq!(
            identity,
            &Identity {
                nodes: vec![90, 91],
                ..Identity::default()
            }
        );

        let plain = emit(&[("name", "Riverdale Farm"), ("tourism", "attraction")]);
        let [Place { record, .. }] = plain.places.as_slice() else {
            panic!("one POI record");
        };
        assert_eq!(record.address, None);
        assert_eq!(record.prominence, crate::record::Prominence::default());
    }

    #[test]
    fn keeps_what_tells_a_place_from_another_of_its_name() {
        let out = emit(&[
            ("name", "Shell"),
            ("amenity", "fuel"),
            (
                "website",
                "https://www.Shell.nl/stations/;http://shell.nl/stations",
            ),
            ("contact:website", "shell.nl/peize"),
            ("phone", "+31 50 123 4567"),
            ("contact:phone", "050-1234567"),
            ("ref", " A12 "),
        ]);
        let [Place { identity, .. }] = out.places.as_slice() else {
            panic!("one POI record");
        };
        assert_eq!(
            identity,
            &Identity {
                outline: None,
                nodes: vec![7],
                websites: vec!["shell.nl/peize".into(), "shell.nl/stations".into()],
                phones: vec!["501234567".into()],
                refs: vec!["a12".into()],
            }
        );
        let plain = emit(&[("name", "Shell"), ("amenity", "fuel"), ("phone", "n/a")]);
        assert_eq!(plain.places[0].identity.phones, Vec::<String>::new());
    }

    #[test]
    fn keeps_what_shows_how_widely_a_poi_is_known() {
        let known = emit(&[
            ("name", "Royal Ontario Museum"),
            ("name:fr", "Musee royal de l'Ontario"),
            ("name:zh", "皇家安大略博物馆"),
            ("tourism", "museum"),
            ("wikidata", "Q1347123"),
        ]);
        let [Place { record, .. }] = known.places.as_slice() else {
            panic!("one POI record");
        };
        assert_eq!(
            record.prominence,
            crate::record::Prominence {
                linked: true,
                languages: 2,
                importance: 0,
            }
        );
    }
}
