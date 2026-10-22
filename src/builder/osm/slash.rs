//! House numbers written "N/M", read from the addresses around them.
//!
//! The slash means different things in different places: New Zealand writes
//! a flat as UNIT/NUMBER ("10/1367" is flat 10 of 1367), Poland writes a
//! building spanning several numbers ("55/73") or standing on a corner. No
//! tag says which, and `addr:country` is seldom set, so the reading comes from
//! the plain numbers on the same street: a unit is nowhere near them, while
//! both ends of a range are. Where one address cannot tell, the clear readings
//! of its street, then of its grid cell, decide.
//!
//! A flat read as UNIT/NUMBER becomes number `M` with unit `N`: found by its
//! building's number, ranked by its unit, still found as written (see
//! [`crate::text_index::flat_number_term`]), and never house `N` when a
//! missing number is placed between its neighbours. Every other reading keeps
//! the number as written.

use crate::{builder::report::SlashNumberCounts, error::Result, spatial_index::haversine_m};

use super::spill::PendingAddress;

/// Plain numbers further away than this are on another stretch of the street.
const NEIGHBOUR_RADIUS_M: f64 = 250.0;
/// Plain numbers an address is compared with, nearest first.
const NEIGHBOURS: usize = 8;
/// One side misses the street's numbers when it is further from them than
/// this, and than `OFF_STREET_RATIO` times the other side.
const OFF_STREET_GAP: u32 = 10;
const OFF_STREET_RATIO: u32 = 3;
/// Clear readings a street, then a cell, needs before it settles an address
/// its neighbours leave open, and the share of them that must agree.
const STREET_VOTES: usize = 2;
const CELL_VOTES: usize = 5;
const AGREEMENT: f64 = 0.8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    UnitFirst,
    NumberFirst,
}

/// What the neighbours of one address say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fit {
    Clear(Reading),
    /// Both sides meet the street's numbers: a range, unless its street says
    /// otherwise.
    Both,
    Alone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slash<'a> {
    left: u32,
    right: u32,
    unit: &'a str,
    number: &'a str,
}

/// "10/1367", "1b/567" or "3/12A": digits, an optional letter, then the same
/// again after the slash. Anything else ("12 1/2", "12-14/3") is not one.
fn parse_slash(number: &str) -> Option<Slash<'_>> {
    let (unit, rest) = number.split_once('/')?;
    let (unit, rest) = (unit.trim(), rest.trim());
    Some(Slash {
        left: leading_number(unit)?,
        right: leading_number(rest)?,
        unit,
        number: rest,
    })
}

/// The number of "12" or "12A".
fn leading_number(value: &str) -> Option<u32> {
    let digits = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let suffix = &value[digits..];
    if digits == 0 || suffix.chars().count() > 1 || suffix.chars().any(|c| !c.is_alphabetic()) {
        return None;
    }
    value[..digits].parse().ok()
}

fn fit(slash: Slash<'_>, at: [f64; 2], plain: &[(u32, [f64; 2])]) -> Fit {
    let mut near = plain
        .iter()
        .map(|(number, [lon, lat])| (haversine_m(at[0], at[1], *lon, *lat), *number))
        .filter(|(distance, _)| *distance <= NEIGHBOUR_RADIUS_M)
        .collect::<Vec<_>>();
    if near.is_empty() {
        return Fit::Alone;
    }
    near.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    near.truncate(NEIGHBOURS);
    // Each side against its nearest number: the two ends of a range each meet
    // the numbers on their own side of the building.
    let miss = |side: u32| {
        near.iter()
            .map(|(_, number)| side.abs_diff(*number))
            .min()
            .expect("near is not empty")
    };
    let (left, right) = (miss(slash.left), miss(slash.right));
    let off = |bad: u32, good: u32| bad > OFF_STREET_GAP.max(OFF_STREET_RATIO * good);
    if off(left, right) {
        // A unit's building is one of the street's numbers. A range starting
        // the street ("2/14" before 18 and 20) meets none between its ends,
        // as its building holds them.
        let building_among = near
            .iter()
            .any(|(_, number)| (slash.left..=slash.right).contains(number));
        return if building_among {
            Fit::Clear(Reading::UnitFirst)
        } else {
            Fit::Both
        };
    }
    if off(right, left) {
        return Fit::Clear(Reading::NumberFirst);
    }
    Fit::Both
}

#[derive(Debug, Default, Clone, Copy)]
struct Votes {
    unit_first: usize,
    number_first: usize,
}

impl Votes {
    fn add(&mut self, fit: Fit) {
        match fit {
            Fit::Clear(Reading::UnitFirst) => self.unit_first += 1,
            Fit::Clear(Reading::NumberFirst) => self.number_first += 1,
            Fit::Both | Fit::Alone => {}
        }
    }

    /// The reading at least `least` clear readings agree on.
    fn settled(self, least: usize) -> Option<Reading> {
        let total = self.unit_first + self.number_first;
        if total < least {
            return None;
        }
        let agreed = |count: usize| count as f64 >= AGREEMENT * total as f64;
        if agreed(self.unit_first) {
            Some(Reading::UnitFirst)
        } else if agreed(self.number_first) {
            Some(Reading::NumberFirst)
        } else {
            None
        }
    }
}

/// Read the slash numbers of one grid cell's addresses, given in street
/// order, and leave them in street order again.
pub(super) fn read_cell(cell: &mut [PendingAddress], counts: &mut SlashNumberCounts) -> Result<()> {
    let mut groups = Vec::new();
    let mut start = 0;
    for index in 1..=cell.len() {
        if index == cell.len() || !cell[index].same_group(&cell[start]) {
            groups.push(start..index);
            start = index;
        }
    }

    let mut fits = vec![None; cell.len()];
    let mut street_votes = vec![Votes::default(); groups.len()];
    let mut cell_votes = Votes::default();
    for (group_index, group) in groups.iter().enumerate() {
        if !cell[group.clone()]
            .iter()
            .any(|pending| parse_slash(pending.fields().number).is_some())
        {
            continue;
        }
        let plain = cell[group.clone()]
            .iter()
            .filter_map(|pending| {
                Some((
                    leading_number(pending.fields().number.trim())?,
                    pending.point,
                ))
            })
            .collect::<Vec<_>>();
        for index in group.clone() {
            let fields = cell[index].fields();
            let Some(slash) = parse_slash(fields.number) else {
                continue;
            };
            let found = fit(slash, cell[index].point, &plain);
            street_votes[group_index].add(found);
            cell_votes.add(found);
            fits[index] = Some((group_index, found));
        }
    }
    if fits.iter().all(Option::is_none) {
        return Ok(());
    }

    let mut changed = false;
    for (index, fit) in fits.into_iter().enumerate() {
        let Some((group_index, fit)) = fit else {
            continue;
        };
        let reading = match fit {
            Fit::Clear(reading) => Some(reading),
            Fit::Both | Fit::Alone => street_votes[group_index]
                .settled(STREET_VOTES)
                .or_else(|| cell_votes.settled(CELL_VOTES))
                .or((fit == Fit::Both).then_some(Reading::NumberFirst)),
        };
        match reading {
            Some(Reading::UnitFirst) => {
                counts.unit_first += 1;
                let mut address = cell[index].address();
                let Slash { unit, number, .. } =
                    parse_slash(&address.number).expect("a slash number");
                let (unit, number) = (unit.to_string(), number.to_string());
                // An `addr:unit` tag names the flat as its mapper wrote it.
                address.unit.get_or_insert(unit);
                address.number = number;
                // The sort key holds the number, so a flat now sorts with
                // its building.
                cell[index] = cell[index].restated(&address);
                changed = true;
            }
            Some(Reading::NumberFirst) => counts.number_first += 1,
            None => counts.unresolved += 1,
        }
    }
    if changed {
        cell.sort();
    }
    Ok(())
}

/// The addresses, given in street order, with their slash numbers read, in
/// street order: each grid cell is read whole, as its streets vote together.
pub(super) fn read_slash_numbers(
    addresses: impl Iterator<Item = Result<PendingAddress>>,
    counts: &mut SlashNumberCounts,
    mut emit: impl FnMut(PendingAddress) -> Result<()>,
) -> Result<()> {
    let mut cell: Vec<PendingAddress> = Vec::new();
    for address in addresses {
        let address = address?;
        if cell.first().is_some_and(|first| !first.same_cell(&address)) {
            read_cell(&mut cell, counts)?;
            cell.drain(..).try_for_each(&mut emit)?;
        }
        cell.push(address);
    }
    read_cell(&mut cell, counts)?;
    cell.drain(..).try_for_each(emit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{
        AddressComponents, AddressRecord, LocationPrecision, ObjectType, SourceProvenance,
    };

    fn pending(id: i64, number: &str, street: &str, lat: f64) -> PendingAddress {
        let record = AddressRecord {
            disputed: false,
            street_postcodes: Vec::new(),
            address: AddressComponents {
                number: number.to_string(),
                street: Some(street.to_string()),
                place: None,
                unit: None,
                locality: None,
                areas: Vec::new(),
                region: None,
                postcode: None,
                country: None,
            },
            geometry: geojson::Geometry::new(geojson::GeometryValue::Point {
                coordinates: vec![176.25, lat].into(),
            }),
            location_precision: LocationPrecision::Point,
            source: SourceProvenance::osm(ObjectType::Node, id),
        };
        PendingAddress::new(record, id as u64, 0).expect("pending")
    }

    /// Read one cell of `(number, street, lat)` and return each address's
    /// number and unit, in street order.
    fn read(addresses: &[(&str, &str, f64)]) -> (Vec<(String, Option<String>)>, SlashNumberCounts) {
        let mut cell = addresses
            .iter()
            .enumerate()
            .map(|(index, (number, street, lat))| pending(index as i64, number, street, *lat))
            .collect::<Vec<_>>();
        cell.sort();
        let mut counts = SlashNumberCounts::default();
        read_cell(&mut cell, &mut counts).expect("read");
        let numbers = cell
            .iter()
            .map(|pending| {
                let address = pending.address();
                (address.number, address.unit)
            })
            .collect();
        (numbers, counts)
    }

    #[test]
    fn parses_slash_numbers() {
        assert_eq!(
            parse_slash("10/1367"),
            Some(Slash {
                left: 10,
                right: 1367,
                unit: "10",
                number: "1367"
            })
        );
        assert_eq!(
            parse_slash("1b / 567").map(|s| (s.unit, s.number)),
            Some(("1b", "567"))
        );
        assert_eq!(parse_slash("3/12A").map(|s| s.right), Some(12));
        for number in ["12 1/2", "12-14/3", "1/2/34", "A/12", "10/", "/12", "12"] {
            assert_eq!(parse_slash(number), None, "{number}");
        }
    }

    #[test]
    fn a_flat_is_its_buildings_number_and_its_unit() {
        let (numbers, counts) = read(&[
            ("1345", "Eruera Street", -38.1380),
            ("10/1367", "Eruera Street", -38.1382),
            ("1367", "Eruera Street", -38.1383),
            ("1400", "Eruera Street", -38.1386),
        ]);
        assert_eq!(
            numbers,
            [
                ("1345".to_string(), None),
                ("1367".to_string(), None),
                ("1367".to_string(), Some("10".to_string())),
                ("1400".to_string(), None),
            ]
        );
        assert_eq!(
            counts,
            SlashNumberCounts {
                unit_first: 1,
                ..Default::default()
            }
        );
    }

    #[test]
    fn a_range_keeps_its_numbers() {
        // Marszałkowska 55/73 spans the numbers between its neighbours.
        let (numbers, counts) = read(&[
            ("53", "Marszałkowska", 52.2300),
            ("55/73", "Marszałkowska", 52.2302),
            ("75", "Marszałkowska", 52.2304),
            // A range starting the street meets no number between its ends.
            ("2/14", "Prosta", 52.2300),
            ("18", "Prosta", 52.2302),
        ]);
        assert!(
            numbers.iter().all(|(_, unit)| unit.is_none()),
            "{numbers:?}"
        );
        assert!(numbers.iter().any(|(number, _)| number == "55/73"));
        assert!(numbers.iter().any(|(number, _)| number == "2/14"));
        assert_eq!(
            counts,
            SlashNumberCounts {
                number_first: 2,
                ..Default::default()
            }
        );
    }

    #[test]
    fn a_street_settles_the_flats_its_neighbours_leave_open() {
        // Both sides of 2/7 miss the street's numbers alike; its street's
        // other flats are units.
        let (numbers, _) = read(&[
            ("120", "Opawa Road", -43.5500),
            ("1/124", "Opawa Road", -43.5501),
            ("2/124", "Opawa Road", -43.5502),
            ("124", "Opawa Road", -43.5503),
            ("2/7", "Opawa Road", -43.5504),
            ("128", "Opawa Road", -43.5505),
        ]);
        assert!(
            numbers.contains(&("7".to_string(), Some("2".to_string()))),
            "{numbers:?}"
        );
        assert!(numbers.contains(&("124".to_string(), Some("1".to_string()))));
    }

    #[test]
    fn a_number_with_no_neighbours_is_kept() {
        let (numbers, counts) = read(&[("10/1367", "Lonely Road", -38.0)]);
        assert_eq!(numbers, [("10/1367".to_string(), None)]);
        assert_eq!(
            counts,
            SlashNumberCounts {
                unresolved: 1,
                ..Default::default()
            }
        );
    }

    #[test]
    fn an_addr_unit_tag_wins_over_the_slash_unit() {
        let mut cell = vec![
            pending(1, "1367", "Eruera Street", -38.1383),
            pending(2, "10/1367", "Eruera Street", -38.1382),
        ];
        let mut address = cell[1].address();
        address.unit = Some("Flat 10".to_string());
        cell[1] = cell[1].restated(&address);
        cell.sort();
        read_cell(&mut cell, &mut SlashNumberCounts::default()).expect("read");
        let units = cell
            .iter()
            .map(|pending| pending.address().unit)
            .collect::<Vec<_>>();
        assert!(units.contains(&Some("Flat 10".to_string())), "{units:?}");
    }
}
