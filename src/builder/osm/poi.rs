//! Named points of interest.
//!
//! What counts as a place comes from iD tagging schema presets
//! (`catalog/places.json`, filtered by `catalog/curation.toml`), not a key
//! list here: a new kind of place counts once mappers get a preset for it.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::OnceLock,
};

use serde::Deserialize;

use crate::record::{
    AddressRecord, Brand, LocationPrecision, ObjectType, PoiRecord, Record, SourceProvenance,
    point_geometry,
};

use super::{
    address::address_components,
    emitted::Emitted,
    tags::{OsmTags, alternate_names, name_tags, prominence},
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

// A cafe mapped as a point and its building lies within a building of itself.
const SAME_PLACE_M: f64 = 50.0;
// Parts of a campus or park. Chain branches never meet here: they link the
// chain as `brand:wikidata`, not `wikidata`.
const SAME_ITEM_M: f64 = 2_000.0;
// One cell spans [`SAME_PLACE_M`]: a near leader is in this cell or a neighbour.
const CELL_DEGREES: f64 = SAME_PLACE_M / 111_320.0;

/// Entity resolution blocked by name. Same Wikidata item, or same kind
/// within [`SAME_PLACE_M`]: a hotel and its restaurant stay two. Each joins
/// the leader, never another member (center clustering), so a run of near
/// ones cannot chain a place along a street. Merged ids stay as aliases.
pub(crate) fn resolve(mut places: Vec<PoiRecord>) -> Vec<PoiRecord> {
    if places.len() < 2 {
        return places;
    }
    let drawn = |place: &PoiRecord| match place.source.object_type {
        ObjectType::Relation => 2,
        ObjectType::Way => 1,
        ObjectType::Node | ObjectType::Row => 0,
    };
    places.sort_by(|left, right| {
        right
            .prominence
            .cmp(&left.prominence)
            .then(drawn(right).cmp(&drawn(left)))
            .then(left.source.object_type.cmp(&right.source.object_type))
            .then(left.source.object_id.cmp(&right.source.object_id))
    });
    let mut leaders: Vec<PoiRecord> = Vec::new();
    let mut cells = HashMap::<(i64, i64), Vec<usize>>::new();
    let mut items = HashMap::<u32, usize>::new();
    for place in places {
        let Some(point) = point_of(&place) else {
            leaders.push(place);
            continue;
        };
        let near = |leader: &PoiRecord, within: f64| {
            point_of(leader).is_some_and(|at| {
                crate::spatial_index::haversine_m(point[0], point[1], at[0], at[1]) <= within
            })
        };
        let by_item = place
            .wikidata
            .and_then(|item| items.get(&item).copied())
            .filter(|&leader| near(&leaders[leader], SAME_ITEM_M));
        let by_kind = || {
            let (row, column) = cell(point);
            (row - 1..=row + 1)
                .flat_map(|row| (column - 1..=column + 1).map(move |column| (row, column)))
                .filter_map(|at| cells.get(&at))
                .flatten()
                .copied()
                .filter(|&leader| {
                    let leader = &leaders[leader];
                    leader.category == place.category
                        && !(leader.wikidata.is_some()
                            && place.wikidata.is_some()
                            && leader.wikidata != place.wikidata)
                        && near(leader, SAME_PLACE_M)
                })
                .min()
        };
        match by_item.or_else(by_kind) {
            Some(leader) => merge(&mut leaders[leader], place),
            None => {
                let index = leaders.len();
                cells.entry(cell(point)).or_default().push(index);
                if let Some(item) = place.wikidata {
                    items.entry(item).or_insert(index);
                }
                leaders.push(place);
            }
        }
    }
    leaders
}

fn point_of(place: &PoiRecord) -> Option<[f64; 2]> {
    crate::util::geo::point_lon_lat(&place.geometry)
}

fn cell([lon, lat]: [f64; 2]) -> (i64, i64) {
    let width = CELL_DEGREES / lat.to_radians().cos().max(0.01);
    (
        (lat / CELL_DEGREES).floor() as i64,
        (lon / width).floor() as i64,
    )
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
/// Wikipedia links and, when they state a valid address, its `addr:*` tags.
pub(crate) fn poi_tags(
    tags: &BTreeMap<String, String>,
    address_tags: Option<&BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    tags.iter()
        .filter(|(key, _)| {
            matches!(
                key.as_str(),
                "wikidata" | "wikipedia" | "brand" | "brand:wikidata"
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
    out.records.push(Record::Poi(record));
}

/// Several items in one value name none.
fn item(tags: &BTreeMap<String, String>, key: &str) -> Option<u32> {
    tags.cleaned(key)?
        .strip_prefix('Q')?
        .parse()
        .ok()
        .filter(|item| *item > 0)
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
            },
            &mut out,
        );
        out
    }

    #[test]
    fn places_of_one_name_merge_when_they_are_one_place_and_never_chain() {
        let place = |id: i64, category: &str, lon: f64, wikidata: Option<u32>| PoiRecord {
            alt_names: Vec::new(),
            name: "De Pindol".to_string(),
            category: category.to_string(),
            address: None,
            geometry: point_geometry(lon, 51.4),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, id),
            prominence: Default::default(),
            wikidata,
            brand: None,
            aliases: Vec::new(),
        };
        let metres = |m: f64| 5.0 + m / 70_000.0;
        let ids = |resolved: Vec<PoiRecord>| {
            let mut ids = resolved
                .into_iter()
                .map(|place| (place.source.object_id, place.aliases))
                .collect::<Vec<_>>();
            ids.sort();
            ids
        };

        assert_eq!(
            ids(resolve(vec![
                place(1, "amenity:cafe", metres(0.0), None),
                place(2, "amenity:cafe", metres(3.0), None),
            ])),
            [(1, vec![(ObjectType::Node, 2)])]
        );
        assert_eq!(
            resolve(vec![
                place(1, "tourism:hotel", metres(0.0), None),
                place(2, "amenity:restaurant", metres(4.0), None),
            ])
            .len(),
            2
        );
        assert_eq!(
            resolve(vec![
                place(1, "amenity:university", metres(0.0), Some(7)),
                place(2, "building:university", metres(900.0), Some(7)),
            ])
            .len(),
            1
        );
        // No chaining: third is 80 m from the leader.
        assert_eq!(
            ids(resolve(vec![
                place(1, "amenity:cafe", metres(0.0), None),
                place(2, "amenity:cafe", metres(40.0), None),
                place(3, "amenity:cafe", metres(80.0), None),
            ])),
            [(1, vec![(ObjectType::Node, 2)]), (3, Vec::new())]
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
            },
            &mut out,
        );
        let [Record::Address(address), Record::Poi(record)] = out.records.as_slice() else {
            panic!("an address and a POI, got {:?}", out.records);
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

        let plain = emit(&[("name", "Riverdale Farm"), ("tourism", "attraction")]);
        let [Record::Poi(record)] = plain.records.as_slice() else {
            panic!("one POI record");
        };
        assert_eq!(record.address, None);
        assert_eq!(record.prominence, crate::record::Prominence::default());
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
        let [Record::Poi(record)] = known.records.as_slice() else {
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
