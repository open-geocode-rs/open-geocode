use crate::{
    api::{Accuracy, Address, Common, ForwardFeature, ReverseFeature},
    builder::osm::DISPUTED_HOUSE_RADIUS_M,
    record::{Building, Layer},
    text_index::compact_postcode,
    util::fold::search_key,
    util::geo::haversine_m,
};

pub(crate) trait Answer {
    fn house(&self) -> Option<House>;
    fn building(&mut self) -> &mut Option<Building>;
}

impl Answer for ForwardFeature {
    fn house(&self) -> Option<House> {
        House::of(&self.properties.common, self.geometry.coordinates)
    }

    fn building(&mut self) -> &mut Option<Building> {
        &mut self.properties.common.building
    }
}

impl Answer for ReverseFeature {
    fn house(&self) -> Option<House> {
        House::of(&self.properties.common, self.geometry.coordinates)
    }

    fn building(&mut self) -> &mut Option<Building> {
        &mut self.properties.common.building
    }
}

fn take_building(kept: &mut Option<Building>, other: &mut Option<Building>) {
    if kept.is_none() {
        *kept = other.take();
    }
}

pub(crate) fn one_per_house<A: Answer>(answers: &mut Vec<A>) {
    let mut kept: Vec<(A, Option<House>)> = Vec::with_capacity(answers.len());
    for answer in std::mem::take(answers) {
        let house = answer.house();
        let copy = house.as_ref().and_then(|house| {
            kept.iter()
                .position(|(_, held)| held.as_ref().is_some_and(|held| held.is_same(house)))
        });
        let mut answer = answer;
        match copy {
            Some(at) => {
                let held = kept[at].1.as_ref();
                if house
                    .as_ref()
                    .zip(held)
                    .is_some_and(|(house, held)| house.outranks(held))
                {
                    let (mut dropped, _) = std::mem::replace(&mut kept[at], (answer, house));
                    take_building(kept[at].0.building(), dropped.building());
                } else {
                    take_building(kept[at].0.building(), answer.building());
                }
            }
            None => kept.push((answer, house)),
        }
    }
    *answers = kept.into_iter().map(|(answer, _)| answer).collect();
}

pub(crate) fn ask_again(
    asked: usize,
    ask: usize,
    held: usize,
    kept: usize,
    most: usize,
) -> Option<usize> {
    let more = (ask * 2).min(most);
    (kept < asked && held >= ask && more > ask).then_some(more)
}

pub(crate) struct House {
    key: (String, String, Option<String>),
    point: [f64; 2],
    postcode: Option<String>,
    locality: Option<String>,
    accuracy: Accuracy,
    stated: usize,
}

impl House {
    fn of(common: &Common, point: [f64; 2]) -> Option<Self> {
        if common.layer != Layer::Address {
            return None;
        }
        Self::new(&common.address, common.accuracy, point)
    }

    fn new(address: &Address, accuracy: Accuracy, point: [f64; 2]) -> Option<Self> {
        let street = search_key(address.street.as_deref()?)?;
        let number = search_key(address.housenumber.as_deref()?)?;
        let unit = address.unit.as_deref().and_then(search_key);
        let stated = [
            &address.locality,
            &address.region,
            &address.postcode,
            &address.country,
        ]
        .iter()
        .filter(|part| part.is_some())
        .count();
        Some(Self {
            key: (street, number, unit),
            point,
            postcode: address.postcode.as_deref().and_then(compact_postcode),
            locality: address.locality.as_deref().and_then(search_key),
            accuracy,
            stated,
        })
    }

    fn is_same(&self, other: &Self) -> bool {
        fn differ(left: Option<&String>, right: Option<&String>) -> bool {
            left.zip(right).is_some_and(|(left, right)| left != right)
        }
        let apart = match (&self.postcode, &other.postcode) {
            (Some(left), Some(right)) => left != right,
            _ => differ(self.locality.as_ref(), other.locality.as_ref()),
        };
        self.key == other.key
            && !apart
            && haversine_m(self.point[0], self.point[1], other.point[0], other.point[1])
                <= DISPUTED_HOUSE_RADIUS_M
    }

    fn outranks(&self, held: &Self) -> bool {
        (self.accuracy, std::cmp::Reverse(self.stated))
            < (held.accuracy, std::cmp::Reverse(held.stated))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Copy {
        id: &'static str,
        address: Address,
        accuracy: Accuracy,
        point: [f64; 2],
        building: Option<Building>,
    }

    impl Answer for Copy {
        fn house(&self) -> Option<House> {
            House::new(&self.address, self.accuracy, self.point)
        }

        fn building(&mut self) -> &mut Option<Building> {
            &mut self.building
        }
    }

    fn copy(id: &'static str, accuracy: Accuracy, east: f64, address: Address) -> Copy {
        let metres_per_degree = 111_320.0 * 48.218_f64.to_radians().cos();
        Copy {
            id,
            address: Address {
                housenumber: address
                    .housenumber
                    .clone()
                    .or_else(|| Some("42".to_string())),
                street: Some("Heinestraße".to_string()),
                ..address
            },
            accuracy,
            point: [16.39 + east / metres_per_degree, 48.218],
            building: None,
        }
    }

    fn town(locality: &str, postcode: Option<&str>) -> Address {
        Address {
            locality: Some(locality.to_string()),
            postcode: postcode.map(str::to_string),
            ..Address::default()
        }
    }

    fn kept(mut copies: Vec<Copy>) -> Vec<&'static str> {
        one_per_house(&mut copies);
        copies.iter().map(|copy| copy.id).collect()
    }

    #[test]
    fn copies_of_a_house_answer_once_with_its_most_precise_point() {
        let outline = copy(
            "outline",
            Accuracy::Centroid,
            0.0,
            town("Wien", Some("1020")),
        );
        let point = copy(
            "point",
            Accuracy::Rooftop,
            116.0,
            town("Wien", Some("1020")),
        );
        let bare = copy("bare", Accuracy::Rooftop, 10.0, Address::default());
        let other = copy(
            "other",
            Accuracy::Rooftop,
            0.0,
            Address {
                housenumber: Some("44".to_string()),
                ..Address::default()
            },
        );
        assert_eq!(kept(vec![outline, other, point, bare]), ["point", "other"]);
    }

    #[test]
    fn the_copy_kept_takes_the_building_another_copy_is_drawn_as() {
        let named = || {
            Some(Building {
                name: Some("Google Building 41".to_string()),
            })
        };
        for outline_first in [true, false] {
            let mut outline = copy("outline", Accuracy::Centroid, 0.0, town("Wien", None));
            outline.building = named();
            let point = copy("point", Accuracy::Rooftop, 20.0, town("Wien", None));
            let mut copies = if outline_first {
                vec![outline, point]
            } else {
                vec![point, outline]
            };
            one_per_house(&mut copies);
            let [kept] = copies.as_mut_slice() else {
                panic!("one copy");
            };
            assert_eq!((kept.id, kept.building.clone()), ("point", named()));
        }
    }

    #[test]
    fn a_town_named_in_two_languages_is_one_house_by_its_postcode() {
        let vienna = copy(
            "vienna",
            Accuracy::Rooftop,
            0.0,
            town("Vienna", Some("1020")),
        );
        let wien = copy(
            "wien",
            Accuracy::Rooftop,
            15.0,
            Address {
                country: Some("AT".to_string()),
                ..town("Wien", Some("1020"))
            },
        );
        assert_eq!(kept(vec![vienna, wien]), ["wien"]);
    }

    #[test]
    fn houses_apart_by_what_they_state_or_by_distance_stay_apart() {
        let postcodes = vec![
            copy("1020", Accuracy::Rooftop, 0.0, town("Wien", Some("1020"))),
            copy("1200", Accuracy::Rooftop, 10.0, town("Wien", Some("1200"))),
        ];
        assert_eq!(kept(postcodes), ["1020", "1200"]);
        let towns = vec![
            copy("wien", Accuracy::Rooftop, 0.0, town("Wien", Some("1020"))),
            copy("graz", Accuracy::Rooftop, 10.0, town("Graz", None)),
        ];
        assert_eq!(kept(towns), ["wien", "graz"]);
        let units = vec![
            copy(
                "1",
                Accuracy::Rooftop,
                0.0,
                Address {
                    unit: Some("1".to_string()),
                    ..town("Wien", None)
                },
            ),
            copy(
                "2",
                Accuracy::Rooftop,
                0.0,
                Address {
                    unit: Some("2".to_string()),
                    ..town("Wien", None)
                },
            ),
        ];
        assert_eq!(kept(units), ["1", "2"]);
        let far = vec![
            copy("here", Accuracy::Rooftop, 0.0, town("Wien", Some("1020"))),
            copy(
                "there",
                Accuracy::Rooftop,
                DISPUTED_HOUSE_RADIUS_M + 10.0,
                town("Wien", Some("1020")),
            ),
        ];
        assert_eq!(kept(far), ["here", "there"]);
    }
}
