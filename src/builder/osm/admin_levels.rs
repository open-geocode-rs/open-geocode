//! Which layer each OSM admin level is, per country, from libpostal's
//! boundary tables (`resources/boundaries/osm/<cc>.yaml`, MIT, see
//! `NOTICE`), vendored as `parse/lattice/data/admin_levels.tsv`.
//!
//! The table says what an admin level of a country is (Germany's 5, 6 and 7
//! are districts, 8 cities), what a `designation` is (a British unitary
//! authority is a district), and what single boundaries are, by OSM id
//! (London is a city) or by level within another boundary (a level 10 inside
//! Berlin is a neighbourhood). It is data, read the same way for every
//! country; a level or a country it does not list keeps the layer
//! [`super::boundary`] gives it.

use std::{
    collections::{BTreeSet, HashMap},
    sync::OnceLock,
};

use crate::record::{ObjectType, PlaceLayer};

const TABLE: &str = include_str!("../../parse/lattice/data/admin_levels.tsv");

/// What the table says of a boundary: its layer, or that it is no area of
/// the Pack (`None`).
pub(super) type Verdict = Option<PlaceLayer>;

/// libpostal's table, keyed by lower-case country code.
#[derive(Debug, Default)]
pub(super) struct AdminLevels {
    levels: HashMap<(String, u8), Verdict>,
    designations: HashMap<(String, String), Verdict>,
    ids: HashMap<(ObjectType, i64), (String, Verdict)>,
    /// `(country, object, level, verdict)`: a boundary of `level` inside
    /// `object` is `verdict`.
    within: Vec<(String, (ObjectType, i64), u8, Verdict)>,
}

/// The table, read once.
pub(super) fn admin_levels() -> &'static AdminLevels {
    static LEVELS: OnceLock<AdminLevels> = OnceLock::new();
    LEVELS.get_or_init(|| AdminLevels::parse(TABLE))
}

impl AdminLevels {
    fn parse(text: &str) -> Self {
        let mut table = Self::default();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let [country, kind, key, component] = line.split('\t').collect::<Vec<_>>()[..] else {
                continue;
            };
            let Some(verdict) = verdict(component) else {
                continue;
            };
            let country = country.to_string();
            match kind {
                "level" => {
                    if let Ok(level) = key.parse() {
                        table.levels.insert((country, level), verdict);
                    }
                }
                "designation" => {
                    table
                        .designations
                        .insert((country, key.to_string()), verdict);
                }
                "id" => {
                    if let Some(object) = object(key) {
                        table.ids.insert(object, (country, verdict));
                    }
                }
                "within" => {
                    if let Some((inside, level)) = key.split_once(' ')
                        && let (Some(inside), Ok(level)) = (object(inside), level.parse())
                    {
                        table.within.push((country, inside, level, verdict));
                    }
                }
                _ => {}
            }
        }
        table
    }

    /// What the table says of one boundary, most specific rule first: its
    /// own id, its level inside another boundary (`inside` tells whether it
    /// lies in a given object), its designation, its level. `None` when the
    /// table says nothing of it.
    /// Every country the table lists, with the levels it names and what it
    /// calls each. For tests that must cover the whole table rather than the
    /// handful of countries a fixture reaches.
    #[cfg(test)]
    pub(super) fn levels_by_country(&self) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut by: std::collections::BTreeMap<String, Vec<u8>> = Default::default();
        for ((country, level), verdict) in &self.levels {
            if verdict.is_some() {
                by.entry(country.clone()).or_default().push(*level);
            }
        }
        for levels in by.values_mut() {
            levels.sort_unstable();
        }
        by
    }

    /// Whether the table gives `country` a level it calls a town: a country
    /// it describes only down to its regions (the UAE and its emirates) has
    /// no locality tier to fill, and no ranking can invent one.
    #[cfg(test)]
    pub(super) fn names_a_locality(&self, country: &str) -> bool {
        self.levels
            .iter()
            .any(|((at, _), verdict)| at == country && *verdict == Some(PlaceLayer::Locality))
    }

    pub(super) fn verdict(
        &self,
        boundary: Boundary<'_>,
        mut inside: impl FnMut((ObjectType, i64)) -> bool,
    ) -> Option<Verdict> {
        if let Some((_, verdict)) = self.ids.get(&boundary.object) {
            return Some(*verdict);
        }
        let country = boundary.country?;
        let level = boundary.admin_level?;
        if let Some((.., verdict)) = self
            .within
            .iter()
            .find(|(within, object, at, _)| within == country && *at == level && inside(*object))
        {
            return Some(*verdict);
        }
        if let Some(designation) = boundary.designation
            && let Some(verdict) = self
                .designations
                .get(&(country.to_string(), designation.to_string()))
        {
            return Some(*verdict);
        }
        self.levels.get(&(country.to_string(), level)).copied()
    }

    /// The objects the `within` rules of `countries` name, which must be
    /// among the boundaries for those rules to apply.
    pub(super) fn within_objects<'a>(
        &'a self,
        countries: &'a BTreeSet<String>,
    ) -> impl Iterator<Item = (ObjectType, i64)> + 'a {
        self.within
            .iter()
            .filter(|(country, ..)| countries.contains(country))
            .map(|(_, object, ..)| *object)
    }

    /// The ways and relations the id rules of `countries` name, written as
    /// `relation:65606`, to report the ones a build does not find: an id
    /// the table names that is gone from OSM is a rule that no longer
    /// applies.
    pub(super) fn listed_areas<'a>(
        &'a self,
        countries: &'a BTreeSet<String>,
    ) -> impl Iterator<Item = (ObjectType, i64)> + 'a {
        self.ids
            .iter()
            .filter(|(object, (country, _))| {
                countries.contains(country) && object.0 != ObjectType::Node
            })
            .map(|(object, _)| *object)
            .chain(self.within_objects(countries))
    }
}

/// What one boundary is, as the table reads it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Boundary<'a> {
    pub object: (ObjectType, i64),
    /// Lower-case country code.
    pub country: Option<&'a str>,
    pub admin_level: Option<u8>,
    pub designation: Option<&'a str>,
}

/// The layer of a libpostal component, `Some(None)` for one that is no area
/// of the Pack, and `None` for a component the Pack has no layer for.
fn verdict(component: &str) -> Option<Verdict> {
    Some(match component {
        "country" => Some(PlaceLayer::Country),
        "state" => Some(PlaceLayer::Region),
        "state_district" => Some(PlaceLayer::District),
        "city" => Some(PlaceLayer::Locality),
        "city_district" | "suburb" => Some(PlaceLayer::Neighbourhood),
        // A group of regions ("関東地方") is a region too: queries name it,
        // so its names must be known, and a record keeps the smaller region
        // it lies in, its prefecture, as its own.
        "country_region" => Some(PlaceLayer::Region),
        "-" => None,
        _ => return None,
    })
}

fn object(key: &str) -> Option<(ObjectType, i64)> {
    let (kind, id) = key.split_once(':')?;
    let kind = match kind {
        "node" => ObjectType::Node,
        "way" => ObjectType::Way,
        "relation" => ObjectType::Relation,
        _ => return None,
    };
    Some((kind, id.parse().ok()?))
}

/// Writes an object as the table does: `relation:65606`.
pub(super) fn object_key((kind, id): (ObjectType, i64)) -> String {
    format!("{}:{id}", crate::labels::object_type_name(kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boundary(country: &str, level: u8) -> Boundary<'_> {
        Boundary {
            object: (ObjectType::Relation, 1),
            country: Some(country),
            admin_level: Some(level),
            designation: None,
        }
    }

    #[test]
    fn levels_are_read_per_country() {
        let table = admin_levels();
        let never = |_| false;
        assert_eq!(
            table.verdict(boundary("de", 6), never),
            Some(Some(PlaceLayer::District))
        );
        assert_eq!(
            table.verdict(boundary("de", 8), never),
            Some(Some(PlaceLayer::Locality))
        );
        assert_eq!(
            table.verdict(boundary("jp", 7), never),
            Some(Some(PlaceLayer::Locality))
        );
        assert_eq!(table.verdict(boundary("zz", 8), never), None);
    }

    #[test]
    fn specific_rules_win_over_levels() {
        let table = admin_levels();
        // London is a city whatever its level.
        let london = Boundary {
            object: (ObjectType::Relation, 65606),
            ..boundary("gb", 5)
        };
        assert_eq!(
            table.verdict(london, |_| false),
            Some(Some(PlaceLayer::Locality))
        );
        // A level 10 inside Berlin is a neighbourhood.
        let berlin = (ObjectType::Relation, 62422);
        assert_eq!(
            table.verdict(boundary("de", 10), |object| object == berlin),
            Some(Some(PlaceLayer::Neighbourhood))
        );
        let unitary = Boundary {
            designation: Some("unitary_authority"),
            ..boundary("gb", 8)
        };
        assert_eq!(
            table.verdict(unitary, |_| false),
            Some(Some(PlaceLayer::District))
        );
        assert_eq!(object_key((ObjectType::Relation, 65606)), "relation:65606");
    }
}
