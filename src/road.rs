//! Whether two street names on nearby records name one road. The sources a
//! Pack mixes spell one road differently: one's "Roosevelt Boulevard" is
//! another's "E ROOSEVELT BLVD", and "Pražská třída" is "Pražská tř.". A
//! search reads roads this way when it pairs house-number neighbours and
//! joins the pieces of a street answer; a Pack build can read them alike.

use crate::{
    parse::lattice,
    text_index::{direction_word, is_street_type},
    util::fold::search_key,
};

/// How far apart two pieces of one road may lie and still join: farther
/// than the gaps between the address points along most roads, nearer than
/// two roads of one name in one town usually are.
pub(crate) const ROAD_LINK_M: f64 = 1_000.0;

/// The fewest letters of a type glued onto a name that split it off: the
/// tables list one-letter endings that end many names.
const MIN_GLUED_TYPE: usize = 3;

/// A street name read for comparing with another: its words, and its
/// direction apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoadName {
    words: Vec<String>,
    direction: Option<&'static str>,
}

const DIRECTIONS: [&str; 8] = [
    "north",
    "south",
    "east",
    "west",
    "northeast",
    "northwest",
    "southeast",
    "southwest",
];

fn direction_of(word: &str) -> Option<&'static str> {
    direction_word(word).or_else(|| DIRECTIONS.iter().find(|full| **full == word).copied())
}

impl RoadName {
    /// `name` read as a road, `None` when it has no words. A type glued
    /// onto the name is a word of its own ("hauptstr" is "haupt str").
    pub(crate) fn of(name: &str) -> Option<Self> {
        let mut words = Vec::new();
        for word in search_key(name)?.split_whitespace() {
            match lattice::glued_stem(word) {
                Some(stem) if word.len() - stem.len() >= MIN_GLUED_TYPE => {
                    words.push(stem.to_string());
                    words.push(word[stem.len()..].to_string());
                }
                _ => words.push(word.to_string()),
            }
        }
        // A direction leads or ends the name; it is kept apart only when a
        // word that names the road is left ("North Street" stays a name).
        let names = |words: &[String]| {
            words
                .iter()
                .any(|word| !is_street_type(lattice::full_form(word).unwrap_or(word)))
        };
        let mut direction = None;
        if words.len() > 1
            && let Some(found) = direction_of(&words[0])
            && names(&words[1..])
        {
            direction = Some(found);
            words.remove(0);
        }
        if words.len() > 1
            && let Some(found) = words.last().and_then(|word| direction_of(word))
            && names(&words[..words.len() - 1])
            && direction.is_none_or(|stated| stated == found)
        {
            direction = Some(found);
            words.pop();
        }
        (!words.is_empty()).then_some(Self { words, direction })
    }

    /// Whether `self` and `other` name one road: word for word alike, an
    /// abbreviation alike with its full form; a direction only one states
    /// does not tell them apart, two different ones do.
    pub(crate) fn same_road(&self, other: &Self) -> bool {
        if let (Some(mine), Some(theirs)) = (self.direction, other.direction)
            && mine != theirs
        {
            return false;
        }
        self.words.len() == other.words.len()
            && self
                .words
                .iter()
                .zip(&other.words)
                .all(|(mine, theirs)| same_word(mine, theirs))
    }
}

fn same_word(mine: &str, theirs: &str) -> bool {
    if mine == theirs {
        return true;
    }
    let (mine_full, theirs_full) = (lattice::other_forms(mine), lattice::other_forms(theirs));
    mine_full.iter().any(|form| form == theirs)
        || theirs_full.iter().any(|form| form == mine)
        || mine_full.iter().any(|form| theirs_full.contains(form))
}

/// Whether the street names `left` and `right` name one road (see
/// [`RoadName::same_road`]).
pub(crate) fn same_road(left: &str, right: &str) -> bool {
    match (RoadName::of(left), RoadName::of(right)) {
        (Some(left), Some(right)) => left.same_road(&right),
        _ => false,
    }
}

/// Whether records named `left` at `left_at` and `right` at `right_at`
/// (longitude, latitude) lie on one road: one road by name, at most
/// `within_m` apart.
pub(crate) fn same_road_near(
    left: &str,
    left_at: [f64; 2],
    right: &str,
    right_at: [f64; 2],
    within_m: f64,
) -> bool {
    planar_m(left_at, right_at) <= within_m && same_road(left, right)
}

/// The distance in metres between two near points, on a plane at their
/// latitude: close enough at a road's scale and cheap enough for many.
pub(crate) fn planar_m(left: [f64; 2], right: [f64; 2]) -> f64 {
    let latitude = ((left[1] + right[1]) / 2.0).to_radians();
    let east = (left[0] - right[0]) * latitude.cos();
    let north = left[1] - right[1];
    (east * east + north * north).sqrt() * 111_195.0
}

/// The pieces of `pieces` (each a name and its points) that join into one
/// road: one by name, each within [`ROAD_LINK_M`] of another of its road.
/// Every piece is in one group; a group lists its pieces in their order.
pub(crate) fn road_groups(pieces: &[(Option<RoadName>, Vec<[f64; 2]>)]) -> Vec<Vec<usize>> {
    let mut group = (0..pieces.len()).collect::<Vec<_>>();
    fn root(group: &mut [usize], mut at: usize) -> usize {
        while group[at] != at {
            group[at] = group[group[at]];
            at = group[at];
        }
        at
    }
    for left in 0..pieces.len() {
        for right in left + 1..pieces.len() {
            let (Some(left_name), Some(right_name)) = (&pieces[left].0, &pieces[right].0) else {
                continue;
            };
            if root(&mut group, left) == root(&mut group, right) || !left_name.same_road(right_name)
            {
                continue;
            }
            let near = pieces[left].1.iter().any(|mine| {
                pieces[right]
                    .1
                    .iter()
                    .any(|theirs| planar_m(*mine, *theirs) <= ROAD_LINK_M)
            });
            if near {
                let (left_root, right_root) = (root(&mut group, left), root(&mut group, right));
                group[right_root.max(left_root)] = right_root.min(left_root);
            }
        }
    }
    let mut groups = Vec::<Vec<usize>>::new();
    let mut heads = Vec::<usize>::new();
    for at in 0..pieces.len() {
        let head = root(&mut group, at);
        match heads.iter().position(|held| *held == head) {
            Some(index) => groups[index].push(at),
            None => {
                heads.push(head);
                groups.push(vec![at]);
            }
        }
    }
    groups
}

/// The middle of a road's `points`: the one half way along the road, by
/// order along the longer side of their box, so it lies on the road.
/// `None` for none.
pub(crate) fn road_middle(points: &[[f64; 2]]) -> Option<[f64; 2]> {
    let first = points.first()?;
    let (mut west, mut south, mut east, mut north) = (first[0], first[1], first[0], first[1]);
    for point in points {
        (west, south) = (west.min(point[0]), south.min(point[1]));
        (east, north) = (east.max(point[0]), north.max(point[1]));
    }
    let wide = planar_m([west, south], [east, south]) >= planar_m([west, south], [west, north]);
    let axis = usize::from(!wide);
    let mut sorted = points.to_vec();
    sorted.sort_by(|left, right| left[axis].total_cmp(&right[axis]));
    Some(sorted[sorted.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_road_is_one_road_however_its_sources_spell_it() {
        assert!(same_road("Roosevelt Boulevard", "E ROOSEVELT BLVD"));
        assert!(same_road("Bear Valley Parkway South", "BEAR VALLEY PKWY"));
        assert!(same_road("N. Dale Mabry Hwy", "North Dale Mabry Highway"));
        assert!(same_road("Hauptstraße", "Haupt Straße"));
        assert!(same_road("Hauptstr.", "Hauptstraße"));
        assert!(same_road("Pražská tř.", "Pražská třída"));
        assert!(!same_road("N Main St", "S Main St"));
        assert!(!same_road("Roosevelt Boulevard", "Roosevelt Avenue"));
        assert!(!same_road("North Street", "South Street"));
        assert!(!same_road("Main Street", "Main Street Extension"));
    }

    #[test]
    fn near_pieces_of_a_road_join_and_far_ones_stay_apart() {
        let name = |text: &str| RoadName::of(text);
        let pieces = vec![
            (name("Roosevelt Boulevard"), vec![[-75.0102, 40.1066]]),
            (name("E ROOSEVELT BLVD"), vec![[-75.0034, 40.1043]]),
            (name("Roosevelt Boulevard"), vec![[-75.1479, 40.0204]]),
            (name("Bustleton Avenue"), vec![[-75.0050, 40.1050]]),
        ];
        assert_eq!(road_groups(&pieces), vec![vec![0, 1], vec![2], vec![3]]);
        let middle = road_middle(&[[0.0, 0.0], [0.002, 0.0001], [0.001, 0.0]]);
        assert_eq!(middle, Some([0.001, 0.0]));
    }
}
