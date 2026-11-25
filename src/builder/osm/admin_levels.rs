use std::{
    collections::{BTreeSet, HashMap},
    sync::OnceLock,
};

use crate::record::{ObjectType, PlaceLayer};

const TABLE: &str = include_str!("../../parse/lattice/data/admin_levels.tsv");

pub(super) type Verdict = Option<PlaceLayer>;

#[derive(Debug, Default)]
pub(super) struct AdminLevels {
    levels: HashMap<(String, u8), Verdict>,
    designations: HashMap<(String, String), Verdict>,
    ids: HashMap<(ObjectType, i64), (String, Verdict)>,
    within: Vec<(String, (ObjectType, i64), u8, Verdict)>,
}

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

    pub(super) fn within_objects<'a>(
        &'a self,
        countries: &'a BTreeSet<String>,
    ) -> impl Iterator<Item = (ObjectType, i64)> + 'a {
        self.within
            .iter()
            .filter(|(country, ..)| countries.contains(country))
            .map(|(_, object, ..)| *object)
    }

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

#[derive(Debug, Clone, Copy)]
pub(super) struct Boundary<'a> {
    pub object: (ObjectType, i64),
    pub country: Option<&'a str>,
    pub admin_level: Option<u8>,
    pub designation: Option<&'a str>,
}

fn verdict(component: &str) -> Option<Verdict> {
    Some(match component {
        "country" => Some(PlaceLayer::Country),
        "state" => Some(PlaceLayer::Region),
        "state_district" => Some(PlaceLayer::District),
        "city" => Some(PlaceLayer::Locality),
        "city_district" | "suburb" => Some(PlaceLayer::Neighbourhood),
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
        let london = Boundary {
            object: (ObjectType::Relation, 65606),
            ..boundary("gb", 5)
        };
        assert_eq!(
            table.verdict(london, |_| false),
            Some(Some(PlaceLayer::Locality))
        );
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
