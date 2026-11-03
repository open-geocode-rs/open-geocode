//! One answer of all candidates, chosen by what each one's own record
//! states against what the query names: its house number, its road, its
//! postcode and its town. The same fields hold the neighbours a number is
//! placed between and the stretch a street answer stands on.

use super::*;

/// The house number, road, postcodes and towns a query's best reading
/// names, as a record's own fields are compared with them.
#[derive(Debug, Default)]
pub(super) struct Named {
    number: Option<u32>,
    road: Option<road::RoadName>,
    /// Each postcode, its letters and digits only, upper case.
    postcodes: Vec<String>,
    /// Each town, as [`search_key`] folds it.
    towns: Vec<String>,
}

impl Named {
    pub(super) fn of(plan: &QueryPlan) -> Self {
        let parts = |label: Label| {
            plan.context
                .iter()
                .filter(move |part| part.label == label)
                .map(|part| part.words.join(" "))
        };
        let street = plan
            .subject
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            number: plan.house_number.as_ref().map(|number| number.base),
            road: road::RoadName::of(&street),
            postcodes: parts(Label::Postcode)
                .filter_map(|postcode| letters(&postcode))
                .collect(),
            towns: parts(Label::Locality)
                .filter_map(|town| search_key(&town))
                .collect(),
        }
    }

    /// Whether `postcode` is one the query names, or begins one ("92025"
    /// of "92025-7619") or is begun by one.
    fn postcode_is(&self, postcode: Option<&str>) -> bool {
        let Some(postcode) = postcode.and_then(letters) else {
            return false;
        };
        self.postcodes.iter().any(|named| {
            named.starts_with(postcode.as_str()) || postcode.starts_with(named.as_str())
        })
    }

    fn town_is(&self, locality: Option<&str>) -> bool {
        locality
            .and_then(search_key)
            .is_some_and(|town| self.towns.contains(&town))
    }

    /// Whether an address that states `postcode` and `locality` may be
    /// one of the query's places: it states the postcode or the town the
    /// query names, or it states neither field, or the query names neither.
    pub(super) fn holds(&self, postcode: Option<&str>, locality: Option<&str>) -> bool {
        (self.postcodes.is_empty() && self.towns.is_empty())
            || (postcode.is_none() && locality.is_none())
            || self.postcode_is(postcode)
            || self.town_is(locality)
    }

    /// What `hit`'s own record agrees with, in the order an answer is
    /// chosen by: it carries the house number on the road, lies on the
    /// road, states the postcode, states the town. The number and the road
    /// count only inside the query's places (see [`Self::holds`]): the same
    /// number on another road, or on a road of the name in another town, is
    /// no better than any other answer.
    fn agreement(&self, hit: &TextSearchHit) -> (bool, bool, bool, bool) {
        let address = &hit.record.address;
        let placed = self.holds(address.postcode.as_deref(), address.locality.as_deref());
        let number = placed
            && self.number.is_some_and(|number| {
                address.housenumber.as_deref().and_then(base_house_number) == Some(number)
            });
        let street = match hit.record.layer == Layer::Street.as_str() {
            true => Some(hit.record.name.as_str()),
            false => address.street.as_deref(),
        };
        let on_road = placed
            && self.road.as_ref().is_some_and(|road| {
                street
                    .and_then(road::RoadName::of)
                    .is_some_and(|other| other.same_road(road))
            });
        (
            number && on_road,
            on_road,
            self.postcode_is(address.postcode.as_deref()),
            self.town_is(address.locality.as_deref()),
        )
    }

    /// `hits` with those carrying the house number on the road first (see
    /// [`Self::agreement`]), each the same in the order they came. The road,
    /// postcode and town alone reorder nothing: an address whose locality
    /// is its postal town, not the town the query names, would go below
    /// the street.
    pub(super) fn choose(&self, hits: &mut [TextSearchHit]) {
        hits.sort_by_key(|hit| std::cmp::Reverse(self.agreement(hit).0));
    }
}

/// A postcode's letters and digits, upper case; `None` for none.
fn letters(postcode: &str) -> Option<String> {
    compact_postcode(postcode)
        .map(|postcode| postcode_letters(&postcode))
        .filter(|postcode| !postcode.is_empty())
}

impl TextSearcher {
    /// The street answers of `found` as one, at the middle of the stretch
    /// of the road inside the postcode the query names: the largest run of
    /// the addresses of `docs` that state it, each within
    /// [`road::ROAD_LINK_M`] of another (a postcode may hold two roads of
    /// the name). The street answer nearest that middle stands there and the
    /// others go; unchanged for a query with a house number (the text
    /// search may find the house), when no address states the postcode, or
    /// when no street answer lies within [`road::ROAD_LINK_M`] of it.
    pub(super) fn postcode_stretch(
        &self,
        plan: &QueryPlan,
        docs: &[DocId],
        found: &mut Vec<StructuredFound>,
    ) {
        let named = Named::of(plan);
        if named.number.is_some()
            || named.postcodes.is_empty()
            || !found.iter().any(|one| one.form == FORM_STREET)
        {
            return;
        }
        let inside = docs
            .iter()
            .filter(|&&doc| {
                self.doc_layer(doc) == Some(structured::LAYER_ADDRESS)
                    && named.postcode_is(Some(&self.doc_postcodes(doc).0))
            })
            .filter_map(|&doc| self.doc_point(doc))
            .map(|[lon, lat]| [dequantize(lon), dequantize(lat)])
            .take(STRETCH_POINTS)
            .collect::<Vec<_>>();
        let Some(middle) = road::road_middle(&largest_run(&inside)) else {
            return;
        };
        let nearest = found
            .iter()
            .enumerate()
            .filter(|(_, one)| one.form == FORM_STREET)
            .min_by(|(_, left), (_, right)| {
                road::planar_m(left.point, middle).total_cmp(&road::planar_m(right.point, middle))
            })
            .map(|(at, _)| at);
        let Some(nearest) =
            nearest.filter(|&at| road::planar_m(found[at].point, middle) <= road::ROAD_LINK_M)
        else {
            return;
        };
        let mut at = 0;
        found.retain(|one| {
            at += 1;
            one.form != FORM_STREET || at - 1 == nearest
        });
        if let Some(one) = found.iter_mut().find(|one| one.form == FORM_STREET) {
            one.point = middle;
            street_at(&mut one.hit.record, middle);
        }
    }
}

/// Address points a street's stretch is read from, at most.
const STRETCH_POINTS: usize = 2_000;

/// The largest run of `points`, each within [`road::ROAD_LINK_M`] of
/// another of it.
fn largest_run(points: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut run_of = vec![usize::MAX; points.len()];
    let mut runs = Vec::<Vec<[f64; 2]>>::new();
    for start in 0..points.len() {
        if run_of[start] != usize::MAX {
            continue;
        }
        let run = runs.len();
        run_of[start] = run;
        let mut stack = vec![start];
        let mut members = Vec::new();
        while let Some(at) = stack.pop() {
            members.push(points[at]);
            for other in 0..points.len() {
                if run_of[other] == usize::MAX
                    && road::planar_m(points[at], points[other]) <= road::ROAD_LINK_M
                {
                    run_of[other] = run;
                    stack.push(other);
                }
            }
        }
        runs.push(members);
    }
    runs.into_iter().max_by_key(Vec::len).unwrap_or_default()
}
