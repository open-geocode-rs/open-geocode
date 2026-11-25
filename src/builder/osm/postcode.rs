use std::collections::BTreeMap;

use rayon::prelude::*;
use xxhash_rust::xxh3::xxh3_64;

use crate::{
    parse::lattice::CountryCode,
    record::{DerivedSourceProvenance, PostcodeRecord, point_geometry},
    util::geo::haversine_m,
};

use super::spill::Fields;

const SAMPLE_POINTS: usize = 16;

#[derive(Debug, Clone, Default)]
pub(crate) struct Accumulator {
    index: BTreeMap<(Option<CountryCode>, String), usize>,
    groups: Vec<Group>,
    last: Option<(Option<CountryCode>, String, Option<usize>)>,
}

#[derive(Debug, Clone, Default)]
struct Group {
    record_count: u64,
    sample: Vec<(u64, [f64; 2])>,
}

impl Accumulator {
    pub(crate) fn accept(
        &mut self,
        country: Option<CountryCode>,
        postcode: Option<&str>,
        [lon, lat]: [f64; 2],
    ) {
        let Some(stated) = postcode else {
            return;
        };
        let at = match &self.last {
            Some((last_country, last_stated, at))
                if *last_country == country && last_stated == stated =>
            {
                *at
            }
            _ => {
                let at = clean_postcode(stated).map(|postcode| {
                    let next = self.groups.len();
                    let at = *self.index.entry((country, postcode)).or_insert(next);
                    if at == next {
                        self.groups.push(Group::default());
                    }
                    at
                });
                self.last = Some((country, stated.to_string(), at));
                at
            }
        };
        let Some(at) = at else {
            return;
        };
        let group = &mut self.groups[at];
        group.record_count += 1;
        group.offer((hash_point(lon, lat), [lon, lat]));
    }

    pub(crate) fn into_records(self) -> impl Iterator<Item = PostcodeRecord> {
        let points = self
            .groups
            .par_iter()
            .map(Group::medoid)
            .collect::<Vec<_>>();
        let groups = self.groups;
        self.index
            .into_iter()
            .map(move |((country, postcode), at)| {
                groups[at].to_record(country, postcode, points[at])
            })
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.index.len()
    }
}

#[derive(Debug, Default)]
pub(crate) struct Street {
    pub(super) postcodes: Vec<String>,
    stated: Option<String>,
}

impl Street {
    pub(crate) fn lends_to(&mut self, address: &Fields<'_>) -> bool {
        if address.subject().is_none() {
            return false;
        }
        match address.postcode {
            Some(postcode) => {
                if self.stated.as_deref() != Some(postcode) {
                    if let Some(cleaned) = clean_postcode(postcode)
                        && let Err(at) = self.postcodes.binary_search(&cleaned)
                    {
                        self.postcodes.insert(at, cleaned);
                    }
                    self.stated = Some(postcode.to_string());
                }
                false
            }
            None => !self.postcodes.is_empty(),
        }
    }
}

impl Group {
    fn offer(&mut self, point: (u64, [f64; 2])) {
        if self.sample.len() == SAMPLE_POINTS && point.0 >= self.sample[SAMPLE_POINTS - 1].0 {
            return;
        }
        let at = self.sample.partition_point(|kept| kept.0 < point.0);
        if self.sample.get(at).is_some_and(|kept| kept.0 == point.0) {
            return;
        }
        self.sample.insert(at, point);
        self.sample.truncate(SAMPLE_POINTS);
    }

    fn medoid(&self) -> [f64; 2] {
        let distance_sum = |[lon, lat]: [f64; 2]| -> f64 {
            self.sample
                .iter()
                .map(|(_, [other_lon, other_lat])| haversine_m(lon, lat, *other_lon, *other_lat))
                .sum()
        };
        self.sample
            .iter()
            .map(|(_, point)| (distance_sum(*point), *point))
            .min_by(|left, right| left.0.total_cmp(&right.0))
            .map(|(_, point)| point)
            .expect("a postcode group has at least one member")
    }

    fn to_record(
        &self,
        country: Option<CountryCode>,
        postcode: String,
        [lon, lat]: [f64; 2],
    ) -> PostcodeRecord {
        PostcodeRecord {
            postcode,
            country,
            geometry: point_geometry(lon, lat),
            source: DerivedSourceProvenance::osm_address_records(self.record_count),
        }
    }
}

fn hash_point(lon: f64, lat: f64) -> u64 {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&lon.to_le_bytes());
    bytes[8..].copy_from_slice(&lat.to_le_bytes());
    xxh3_64(&bytes)
}

fn clean_postcode(value: &str) -> Option<String> {
    let cleaned = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase();
    if cleaned.is_empty()
        || !cleaned
            .chars()
            .any(|character| character.is_ascii_alphanumeric())
    {
        None
    } else {
        Some(cleaned)
    }
}

#[cfg(test)]
mod tests {
    use geojson::GeometryValue;

    use super::*;

    #[test]
    fn repeated_postcodes_count_under_their_own_country() {
        let nl = CountryCode::parse("NL");
        let mut accumulator = Accumulator::default();
        accumulator.accept(nl, Some("1011 ab"), [4.9, 52.37]);
        accumulator.accept(nl, Some("1011 ab"), [4.91, 52.37]);
        accumulator.accept(None, Some("1011 ab"), [4.92, 52.37]);
        accumulator.accept(None, Some("--"), [4.93, 52.37]);
        accumulator.accept(None, Some("--"), [4.94, 52.37]);
        accumulator.accept(nl, Some("1011AB"), [4.95, 52.37]);
        accumulator.accept(nl, Some("1011 AB"), [4.96, 52.37]);

        let counts = accumulator
            .into_records()
            .map(|record| (record.country, record.postcode, record.source.record_count))
            .collect::<Vec<_>>();
        assert_eq!(
            counts,
            vec![
                (None, "1011 AB".to_string(), 1),
                (nl, "1011 AB".to_string(), 3),
                (nl, "1011AB".to_string(), 1),
            ]
        );
    }

    #[test]
    fn derives_postcode_record_from_accepted_addresses() {
        let mut accumulator = Accumulator::default();
        accumulator.accept(None, Some("m5v 2t6"), [-79.4, 43.6]);
        accumulator.accept(None, Some("M5V   2T6"), [-79.2, 43.8]);

        assert_eq!(accumulator.len(), 1);
        let record = accumulator.into_records().next().expect("record");
        assert_eq!(record.id(), "derived:osm:postcode:M5V%202T6");
        assert_eq!(record.label(), "M5V 2T6");
        assert_eq!(record.source.derived_from, "accepted_address_records");
        assert_eq!(record.source.record_count, 2);
        match record.geometry.value {
            GeometryValue::Point { coordinates } => {
                assert!([[-79.4, 43.6], [-79.2, 43.8]].contains(&[coordinates[0], coordinates[1]]));
            }
            other => panic!("expected Point, got {}", other.type_name()),
        }
    }

    #[test]
    fn postcode_point_is_a_member_and_ignores_a_stray_address_whatever_the_order() {
        let mut points: Vec<(f64, f64)> = (0..9)
            .map(|step| (-79.39 + step as f64 * 0.001, 43.65))
            .collect();
        points.push((151.2, -33.9));
        let point_of = |reversed: bool| {
            let mut accumulator = Accumulator::default();
            let mut ordered = points.clone();
            if reversed {
                ordered.reverse();
            }
            for (lon, lat) in ordered {
                accumulator.accept(None, Some("M5V 2T6"), [lon, lat]);
            }
            let record = accumulator.into_records().next().expect("record");
            assert_eq!(record.source.record_count, 10);
            match record.geometry.value {
                GeometryValue::Point { coordinates } => (coordinates[0], coordinates[1]),
                other => panic!("expected Point, got {}", other.type_name()),
            }
        };

        let point = point_of(false);
        assert!(points.contains(&point));
        assert!((point.0 - -79.386).abs() < 0.002 && point.1 == 43.65);
        assert_eq!(point_of(true), point);
    }

    #[test]
    fn keeps_a_bounded_sample_per_postcode() {
        let mut accumulator = Accumulator::default();
        for step in 0..100 {
            accumulator.accept(None, Some("M5V 2T6"), [-79.0 + step as f64 * 0.001, 43.0]);
        }
        let group = &accumulator.groups[accumulator.index[&(None, "M5V 2T6".to_string())]];
        assert_eq!(group.sample.len(), SAMPLE_POINTS);
        assert_eq!(group.record_count, 100);
    }
}
