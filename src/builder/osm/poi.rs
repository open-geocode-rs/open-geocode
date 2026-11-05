//! Named points of interest.
//!
//! A node or way is a POI when it has a name and its tags match a preset of
//! the iD tagging schema that `catalog/places.json` counts as a place (see
//! `catalog/curation.toml`): the schema OSM's editor offers mappers, so a
//! new kind of place counts once mappers have a preset for it, with no list
//! of keys here. A POI that also states a valid address carries it, and the
//! address is emitted as an address record of its own: address search reads
//! addresses alone.

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

/// How an object is drawn, as a preset names the drawings it applies to.
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
    /// The keys whose values say what kind of place an object is.
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

/// A counted preset, as matching reads it.
#[derive(Debug)]
struct Preset {
    /// The key its id starts with, whose value names its category.
    key: String,
    tags: Vec<(String, String)>,
    point: bool,
    area: bool,
    match_score: f64,
    needs_wikidata: bool,
    never: bool,
}

/// The value a preset tag of any value matches: any but `no`.
const ANY: &str = "*";

#[derive(Debug)]
struct Presets {
    all: Vec<Preset>,
    /// Each preset by every `(key, value)` it tests, `value` being [`ANY`]
    /// for a tag of any value.
    by_tag: HashMap<(String, String), Vec<usize>>,
    /// Every key a preset tests: what the scan keeps of a POI's tags.
    keys: HashSet<String>,
    /// The keys whose values say what kind of place an object is.
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

/// Whether `key` is one a place preset tests.
pub(crate) fn is_place_key(key: &str) -> bool {
    presets().keys.contains(key)
}

/// The preset `tags`, drawn as `drawn`, match best, when it is counted as a
/// place: of the presets whose every tag the object has, the one testing
/// the most tags of a stated value, then the most tags, then of the highest
/// match score; a counted preset wins a tie with one never counted (a bench
/// beside a bakery is the bakery). `None` when no preset matches or the
/// best is never counted (`shop=vacant` is no `shop=*`).
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

/// `key:value` of the tag of the preset `tags` match that names its kind,
/// such as `amenity:cafe`.
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

/// Count `tags`, a named object drawn as `drawn`, in the report as left out
/// when one of its keys is a key places are tagged by and no counted preset
/// matches it: by its first such tag.
pub(crate) fn count_left_out(tags: &BTreeMap<String, String>, drawn: Drawn, out: &mut Emitted) {
    if poi_name(tags).is_none() || poi_category(tags, drawn).is_some() {
        return;
    }
    let kinds = &presets().kinds;
    if let Some((key, value)) = tags.iter().find(|(key, _)| kinds.contains(key.as_str())) {
        out.report.leave_out_place(key, value);
    }
}

/// The chain `brand=` and `brand:wikidata=` name.
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
    // The links are kept for the POI's prominence and importance.
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
    // The address the place stands at is an address of its own: address
    // search reads addresses alone, and would otherwise miss every house
    // that only a POI states.
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
    };
    out.report.accept_poi(&record);
    out.records.push(Record::Poi(record));
}

/// The number of the Wikidata item tag `key` names: 1 for `Q1`. A value of
/// several items, or none, names none.
fn item(tags: &BTreeMap<String, String>, key: &str) -> Option<u32> {
    tags.cleaned(key)?
        .strip_prefix('Q')?
        .parse()
        .ok()
        .filter(|item| *item > 0)
}

/// A name with at least one letter: house numbers and ref codes in `name` are
/// not names. A place with none is named by its brand, which is what its
/// sign says.
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
        // A preset for areas only matches no point.
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
        // A named house is a place only when Wikidata knows it.
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
