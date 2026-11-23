//! How far a town mapped only as a point reaches, as far as the addresses
//! that name it and at least half way to the nearest other town point.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering as Atomic},
};

use rayon::prelude::*;
use rstar::{RTree, primitives::GeomWithData};

use crate::{
    error::Result, parse::lattice::dictionary_key, util::geo::METRES_PER_DEGREE,
    util::geo::haversine_m,
};

use super::{boundary::TownDot, checkpoint::NamedPoint};

/// Votes measured side by side.
const VOTE_BATCH: usize = 1 << 16;

type Peer = GeomWithData<[f64; 2], usize>;

/// Sets the reach of each town point in `dots`, in whole metres, whose names, as
/// [`dictionary_key`] spells them, are `names`. A town reaches as far as
/// the farthest address that names it and lies nearer to it than to any
/// other town point of its kind or a bigger one. An address past the half
/// way line to such a town is that town's, or names a town of this name by
/// mistake. Every town reaches at least half way to the nearest other town
/// point, so a town no address names still holds the houses nearest it.
pub(super) fn measure_reach(
    dots: &mut [TownDot],
    names: &[Vec<String>],
    votes: impl Iterator<Item = Result<NamedPoint>>,
) -> Result<()> {
    let mut by_name = HashMap::<&str, Vec<usize>>::new();
    for (dot, keys) in names.iter().enumerate() {
        for key in keys {
            let held = by_name.entry(key.as_str()).or_default();
            if held.last() != Some(&dot) {
                held.push(dot);
            }
        }
    }
    let all = RTree::bulk_load(
        dots.iter()
            .enumerate()
            .map(|(index, dot)| Peer::new(dot.point, index))
            .collect(),
    );
    // A tree of the towns of each kind and every bigger kind, by rank.
    let mut ranks = dots
        .iter()
        .map(|dot| dot.settlement.rank)
        .collect::<Vec<_>>();
    ranks.sort_unstable();
    ranks.dedup();
    let peers = ranks
        .iter()
        .map(|&rank| {
            let tree = RTree::bulk_load(
                dots.iter()
                    .enumerate()
                    .filter(|(_, dot)| dot.settlement.rank <= rank)
                    .map(|(index, dot)| Peer::new(dot.point, index))
                    .collect(),
            );
            (rank, tree)
        })
        .collect::<Vec<_>>();
    let reach = dots
        .par_iter()
        .enumerate()
        .map(|(index, dot)| {
            let half_way = nearest_m(&all, dot.point, |other| other != index)
                .map_or(0.0, |metres| metres / 2.0);
            AtomicU64::new(half_way.to_bits())
        })
        .collect::<Vec<_>>();
    let measure = |vote: &NamedPoint| {
        let Some(key) = dictionary_key(&vote.name) else {
            return;
        };
        let Some(named) = by_name.get(key.as_str()) else {
            return;
        };
        let [lon, lat] = vote.point;
        let Some((town, metres)) = named
            .iter()
            .map(|&town| {
                (
                    town,
                    haversine_m(lon, lat, dots[town].point[0], dots[town].point[1]),
                )
            })
            .min_by(|left, right| left.1.total_cmp(&right.1))
        else {
            return;
        };
        let rank = dots[town].settlement.rank;
        let Some((_, peers)) = peers.iter().find(|(at, _)| *at == rank) else {
            return;
        };
        if nearer_than(peers, vote.point, metres, |other| other != town) {
            return;
        }
        // Positive floats order as their bits do.
        reach[town].fetch_max(metres.to_bits(), Atomic::Relaxed);
    };
    let mut batch = Vec::with_capacity(VOTE_BATCH);
    for vote in votes {
        batch.push(vote?);
        if batch.len() == VOTE_BATCH {
            batch.par_iter().for_each(measure);
            batch.clear();
        }
    }
    batch.par_iter().for_each(measure);
    for (dot, reach) in dots.iter_mut().zip(reach) {
        dot.reach_m = f64::from_bits(reach.into_inner()).ceil();
    }
    Ok(())
}

/// Metres to the nearest point of `tree` that `accept` takes.
fn nearest_m(
    tree: &RTree<Peer>,
    [lon, lat]: [f64; 2],
    accept: impl Fn(usize) -> bool,
) -> Option<f64> {
    let mut best = None::<f64>;
    for (peer, distance_2) in tree.nearest_neighbor_iter_with_distance_2([lon, lat]) {
        if best.is_some_and(|best| distance_2 > degrees_within(lat, best).powi(2)) {
            break;
        }
        if !accept(peer.data) {
            continue;
        }
        let [peer_lon, peer_lat] = *peer.geom();
        let metres = haversine_m(lon, lat, peer_lon, peer_lat);
        if best.is_none_or(|best| metres < best) {
            best = Some(metres);
        }
    }
    best
}

/// Whether a point of `tree` that `accept` takes lies nearer than `metres`.
fn nearer_than(
    tree: &RTree<Peer>,
    [lon, lat]: [f64; 2],
    metres: f64,
    accept: impl Fn(usize) -> bool,
) -> bool {
    let bound = degrees_within(lat, metres).powi(2);
    tree.nearest_neighbor_iter_with_distance_2([lon, lat])
        .take_while(|(_, distance_2)| *distance_2 <= bound)
        .any(|(peer, _)| {
            let [peer_lon, peer_lat] = *peer.geom();
            accept(peer.data) && haversine_m(lon, lat, peer_lon, peer_lat) < metres
        })
}

/// The most degrees, in plain lon and lat, a point within `metres` of a
/// point at `lat` can lie away. A degree of longitude is shortest at the
/// pole-ward edge of the circle.
fn degrees_within(lat: f64, metres: f64) -> f64 {
    let lat_span = metres / METRES_PER_DEGREE;
    let edge = (lat.abs() + lat_span).min(89.9).to_radians();
    lat_span.hypot(lat_span / edge.cos())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{builder::osm::boundary::Settlement, record::Prominence};

    fn dot(id: u64, point: [f64; 2], rank: u8) -> TownDot {
        TownDot::new(
            id,
            point,
            format!("Town {id}"),
            Settlement {
                rank,
                prominence: Prominence::default(),
            },
        )
    }

    fn vote(name: &str, point: [f64; 2]) -> Result<NamedPoint> {
        Ok(NamedPoint {
            name: name.to_string(),
            point,
        })
    }

    fn keys(dots: &[TownDot]) -> Vec<Vec<String>> {
        dots.iter()
            .map(|dot| dictionary_key(&dot.name).into_iter().collect())
            .collect()
    }

    /// A tenth of a degree of latitude, in metres.
    const TENTH: f64 = METRES_PER_DEGREE / 10.0;

    #[test]
    fn a_town_no_address_names_reaches_half_way_to_the_next() {
        let mut dots = vec![dot(1, [0.0, 0.0], 16), dot(2, [0.0, 0.2], 20)];
        let names = keys(&dots);
        measure_reach(&mut dots, &names, std::iter::empty()).unwrap();
        for dot in &dots {
            assert!((dot.reach_m - TENTH).abs() < 50.0, "{}", dot.reach_m);
        }
    }

    #[test]
    fn a_town_reaches_its_farthest_address() {
        let mut dots = vec![dot(1, [0.0, 0.0], 16), dot(2, [0.0, 0.2], 20)];
        let names = keys(&dots);
        // Past the hamlet's half way line, as a smaller town cuts no city short.
        let votes = [vote("Town 1", [0.0, 0.15]), vote("Town 1", [0.0, -0.05])];
        measure_reach(&mut dots, &names, votes.into_iter()).unwrap();
        assert!(
            (dots[0].reach_m - 1.5 * TENTH).abs() < 50.0,
            "{}",
            dots[0].reach_m
        );
    }

    #[test]
    fn an_address_nearer_a_town_as_big_is_not_counted() {
        let mut dots = vec![dot(1, [0.0, 0.0], 18), dot(2, [0.0, 0.2], 18)];
        let names = keys(&dots);
        let votes = [vote("Town 1", [0.0, 0.15]), vote("town 1", [0.0, 0.08])];
        measure_reach(&mut dots, &names, votes.into_iter()).unwrap();
        assert!(
            (dots[0].reach_m - TENTH).abs() < 50.0,
            "{}",
            dots[0].reach_m
        );
    }

    #[test]
    fn an_address_counts_for_the_nearest_town_of_its_name() {
        let mut dots = vec![dot(1, [0.0, 0.0], 18), dot(2, [0.0, 0.2], 18)];
        dots[1].name = "Town 1".to_string();
        let names = keys(&dots);
        let votes = [vote("Town 1", [0.0, 0.33])];
        measure_reach(&mut dots, &names, votes.into_iter()).unwrap();
        assert!(
            (dots[0].reach_m - TENTH).abs() < 50.0,
            "{}",
            dots[0].reach_m
        );
        assert!(
            (dots[1].reach_m - 1.3 * TENTH).abs() < 50.0,
            "{}",
            dots[1].reach_m
        );
    }
}
