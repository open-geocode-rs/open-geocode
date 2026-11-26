use std::collections::{BTreeMap, HashSet};

use crate::{error::Result, record::Layer, util::geo::haversine_m};

use crate::{
    pack::{Reader, RecordId},
    parse::lattice::CountryCode,
};

use super::{
    BIAS_BANDS, TextAutocompleteOptions, TextSearchHit, TextSearcher, autocomplete_number,
    autocomplete_query_tokens, name_starts_as, search_key,
};

const REACH_BANDS_M: [f64; 7] = [
    50_000.0,
    100_000.0,
    200_000.0,
    500_000.0,
    1_000_000.0,
    2_000_000.0,
    5_000_000.0,
];

const REACH_IMPORTANCE: [u8; 7] = [81, 91, 101, 115, 125, 135, 148];

const HOME_DEPTH: usize = 3;

pub(crate) const FAME_GROUP: u8 = 30;

const OTHER_NAME_WIDTH: usize = 5;

pub(super) const TYPO_WIDTH: usize = 6;

const PART_TYPED_WORLDWIDE: u8 = 158;

const WEAK_ROW: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Out,
    Reach,
    Home,
    Here,
}

fn reach_band(importance: u8) -> usize {
    REACH_IMPORTANCE
        .iter()
        .position(|top| importance <= *top)
        .unwrap_or(REACH_BANDS_M.len())
}

fn distance_band(distance_m: f64) -> usize {
    REACH_BANDS_M
        .iter()
        .position(|band| distance_m <= *band)
        .unwrap_or(REACH_BANDS_M.len())
}

fn is_branch_category(category: &str) -> bool {
    category.starts_with("shop:")
        || [
            "amenity:cafe",
            "amenity:fast_food",
            "amenity:restaurant",
            "amenity:bar",
            "amenity:pub",
            "amenity:bank",
            "amenity:fuel",
            "amenity:pharmacy",
        ]
        .contains(&category)
}

pub(super) fn is_famed(layer: &str) -> bool {
    is_place(layer) || layer == Layer::Poi.as_str()
}

fn is_place(layer: &str) -> bool {
    [
        Layer::Country,
        Layer::Region,
        Layer::District,
        Layer::Locality,
        Layer::Neighbourhood,
        Layer::Place,
    ]
    .iter()
    .any(|place| place.as_str() == layer)
}

pub(crate) fn ranks(options: &TextAutocompleteOptions) -> bool {
    let tokens = search_key(options.query.trim())
        .map(|key| autocomplete_query_tokens(&key))
        .unwrap_or_default();
    !tokens.is_empty()
        && autocomplete_number(&tokens).is_none()
        && options.layer.as_deref().is_none_or(is_place)
}

struct Ranked {
    hit: TextSearchHit,
    begins: bool,
    other_only: bool,
    tier: Tier,
    fame: Option<u8>,
    whole: bool,
    own_whole: bool,
    weak: Option<Tier>,
    depth: usize,
}

pub(crate) fn rank(
    searcher: &TextSearcher,
    options: &TextAutocompleteOptions,
    hits: Vec<TextSearchHit>,
    famous: Vec<TextSearchHit>,
    caller: &super::Caller,
    limit: usize,
) -> Result<Vec<TextSearchHit>> {
    let pack = &searcher.pack;
    let records = pack.records();
    let key = search_key(options.query.trim()).unwrap_or_default();
    let tokens = autocomplete_query_tokens(&key);
    let typed = tokens.join(" ");
    let width = super::typed_width(&typed);
    let mut around = Vec::new();
    for &id in &caller.areas {
        let summary = pack.record_summary(id)?;
        let own = std::iter::once(summary.name.as_str())
            .chain(pack.language_names(id))
            .any(|name| name_starts_as(name, &tokens, false));
        if own {
            let hit = TextSearchHit::of_record(id, summary);
            if searcher.in_scope(options, &hit)? {
                around.push(hit);
            }
        }
    }
    let mut seen = HashSet::new();
    let hits = hits
        .into_iter()
        .chain(around)
        .chain(famous)
        .filter(|hit| seen.insert(hit.record_id))
        .collect::<Vec<_>>();
    let distance_of = |hit: &TextSearchHit| {
        options
            .focus
            .zip(hit.record.point)
            .map(|([lon, lat], point)| haversine_m(lon, lat, point.lon, point.lat))
    };
    let near = hits
        .iter()
        .filter(|hit| {
            hit.record.layer == Layer::Poi.as_str()
                && distance_of(hit).is_some_and(|distance| distance <= REACH_BANDS_M[0])
        })
        .filter_map(|hit| search_key(&hit.record.name))
        .collect::<HashSet<_>>();
    let mut ranked = Vec::with_capacity(hits.len());
    for hit in hits {
        let distance = distance_of(&hit);
        let is_poi = hit.record.layer == Layer::Poi.as_str();
        let alt_names = match is_poi || is_place(&hit.record.layer) {
            true => match records.record(hit.record_id)? {
                crate::record::Record::Poi(record) => record.alt_names,
                crate::record::Record::Place(_, record) => record.alt_names,
                _ => Vec::new(),
            },
            false => Vec::new(),
        };
        let names = std::iter::once(hit.record.name.as_str())
            .chain(pack.language_names(hit.record_id))
            .chain(alt_names.iter().map(String::as_str))
            .collect::<Vec<_>>();
        let mut areas = Vec::new();
        let first_named = || {
            names.iter().any(|name| {
                search_key(name).is_some_and(|name| {
                    name.split_whitespace()
                        .any(|word| super::types_word(&tokens[0], word, false))
                })
            })
        };
        if tokens.len() > 1
            && is_famed(&hit.record.layer)
            && first_named()
            && !named_then_placed(&names, &hit.record.label, &[], &tokens)
            && let Some(context) = pack.boundary_context(hit.record_id)?
        {
            for entry in context.admin_context.entries() {
                areas.extend(pack.language_names(entry.record_id).map(str::to_string));
            }
        }
        let chain = is_poi
            && (hit.record.brand.is_some()
                || hit
                    .record
                    .category
                    .as_deref()
                    .is_some_and(is_branch_category)
                || distance.is_some_and(|distance| distance > REACH_BANDS_M[0])
                    && names
                        .iter()
                        .filter_map(|name| search_key(name))
                        .any(|name| {
                            near.iter().any(|near| {
                                name == *near
                                    || name
                                        .strip_prefix(near.as_str())
                                        .is_some_and(|rest| rest.starts_with(' '))
                            })
                        }));
        let importance = match is_famed(&hit.record.layer) && !chain {
            true => records
                .prominence(hit.record_id)?
                .map_or(0, |prominence| prominence.importance),
            false => 0,
        };
        let own_whole = super::name_words(&hit.record.name).is_some_and(|name| name == typed);
        let mut weak = None;
        let (begins, other_only, whole, own_whole, tier, fame) = match importance {
            0 => (
                true,
                false,
                own_whole,
                own_whole,
                legacy_tier(&hit, distance, own_whole),
                None,
            ),
            known => {
                let starts = |at: usize, name: &str| {
                    (at == 0 || width >= OTHER_NAME_WIDTH) && name_starts_as(name, &tokens, false)
                        || width >= TYPO_WIDTH && name_starts_as(name, &tokens, true)
                };
                let whole_at = names
                    .iter()
                    .position(|name| super::name_words(name).is_some_and(|name| name == typed));
                let whole = whole_at.is_some();
                let translated = names.len() - alt_names.len();
                let own_whole = whole_at.is_some_and(|at| at < translated);
                let covered = names
                    .iter()
                    .enumerate()
                    .filter(|(at, name)| starts(*at, name))
                    .filter_map(|(_, name)| search_key(name))
                    .any(|name| 2 * width >= super::typed_width(&name));
                let ends = width >= OTHER_NAME_WIDTH
                    && names[..translated].iter().any(|name| {
                        search_key(name).is_some_and(|name| {
                            name.strip_suffix(typed.as_str())
                                .is_some_and(|head| head.ends_with(' '))
                        })
                    });
                let begins = whole
                    || ends
                    || names.iter().enumerate().any(|(at, name)| starts(at, name))
                    || named_then_placed(&names, &hit.record.label, &areas, &tokens);
                let begins_own = own_whole
                    || names[..translated]
                        .iter()
                        .enumerate()
                        .any(|(at, name)| starts(at, name))
                    || named_then_placed(&names[..translated], &hit.record.label, &areas, &tokens);
                let home =
                    caller.country.is_some() && country_of(pack, hit.record_id)? == caller.country;
                let known_for = match whole || known > PART_TYPED_WORLDWIDE {
                    true => known,
                    false => known.min(REACH_IMPORTANCE[REACH_IMPORTANCE.len() - 1]),
                };
                let reach = reach_tier(known_for, distance, home);
                let tier = match (distance, reach) {
                    _ if begins_own && caller.areas.contains(&hit.record_id) => Tier::Here,
                    (Some(distance), _) if own_whole && distance <= REACH_BANDS_M[0] => Tier::Here,
                    (Some(distance), Tier::Reach | Tier::Home) if !whole && !covered => {
                        match distance <= REACH_BANDS_M[0] {
                            true => Tier::Reach,
                            false => Tier::Out,
                        }
                    }
                    (_, tier) => tier,
                };
                weak = (begins && tier == Tier::Out && reach == Tier::Home).then_some(reach);
                (
                    begins,
                    begins && !begins_own,
                    whole,
                    own_whole,
                    tier,
                    Some(known),
                )
            }
        };
        let depth = match caller.areas.contains(&hit.record_id) {
            true => usize::MAX - 1 - fineness(&hit.record.layer),
            false => usize::MAX,
        };
        ranked.push(Ranked {
            hit,
            begins,
            other_only,
            tier,
            fame,
            whole,
            own_whole,
            weak,
            depth,
        });
    }

    let ranked = merge_items(pack, ranked)?;
    let best = ranked
        .iter()
        .filter(|entry| entry.begins && entry.tier > Tier::Out)
        .filter_map(|entry| entry.fame)
        .max()
        .unwrap_or_default();
    let promoted = |entry: &Ranked| {
        entry.tier > Tier::Out
            && entry.whole
            && entry.fame.is_some_and(|fame| best - fame < FAME_GROUP)
    };
    let own_promoted = ranked
        .iter()
        .any(|entry| promoted(entry) && entry.own_whole);
    let mut buckets: BTreeMap<(bool, bool, bool, Tier), Vec<Ranked>> = BTreeMap::new();
    for entry in ranked {
        buckets
            .entry((
                entry.begins && !(own_promoted && entry.other_only),
                entry.tier > Tier::Out,
                entry.tier == Tier::Here || promoted(&entry) && (entry.own_whole || !own_promoted),
                entry.tier,
            ))
            .or_default()
            .push(entry);
    }

    let mut out = Vec::new();
    for entries in buckets.into_values().rev() {
        let (mut famed, legacy): (Vec<_>, Vec<_>) =
            entries.into_iter().partition(|entry| entry.fame.is_some());
        famed.sort_by_key(|entry| std::cmp::Reverse(entry.fame));
        let mut grouped = Vec::with_capacity(famed.len());
        let mut leader = None;
        let mut group = 0usize;
        for entry in famed {
            let fame = entry.fame.unwrap_or_default();
            match leader {
                Some(top) if top - fame < FAME_GROUP => {}
                _ => {
                    leader = Some(fame);
                    group += 1;
                }
            }
            grouped.push((group, entry));
        }
        grouped.sort_by_key(|(group, entry)| {
            (
                *group,
                !entry.whole,
                entry.depth,
                std::cmp::Reverse(entry.fame),
            )
        });
        out.extend(grouped.into_iter().map(|(_, entry)| entry).chain(legacy));
    }
    if let Some(at) = out
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.weak.is_some())
        .max_by_key(|(at, entry)| (entry.weak, entry.fame, std::cmp::Reverse(*at)))
        .map(|(at, _)| at)
        && at > WEAK_ROW
    {
        let entry = out.remove(at);
        out.insert(WEAK_ROW, entry);
    }
    let mut out = out.into_iter().map(|entry| entry.hit).collect::<Vec<_>>();
    out.truncate(limit);
    Ok(out)
}

fn named_then_placed(names: &[&str], label: &str, areas: &[String], tokens: &[String]) -> bool {
    let words = |text: &str| {
        search_key(text)
            .map(|key| {
                key.split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let named = names
        .iter()
        .flat_map(|name| words(name))
        .collect::<Vec<_>>();
    let mut placed = words(label);
    placed.extend(areas.iter().flat_map(|area| words(area)));
    let last = tokens.len().saturating_sub(1);
    tokens.len() > 1
        && named
            .iter()
            .any(|word| super::types_word(&tokens[0], word, false))
        && tokens.iter().enumerate().all(|(at, typed)| {
            named
                .iter()
                .chain(&placed)
                .any(|word| super::types_word(typed, word, at == last))
        })
}

fn merge_items(pack: &Reader, ranked: Vec<Ranked>) -> Result<Vec<Ranked>> {
    let mut kept: Vec<Ranked> = Vec::with_capacity(ranked.len());
    'entries: for mut entry in ranked {
        for held in &mut kept {
            let Some(swap) = twin(pack, &held.hit, &entry.hit)? else {
                continue;
            };
            if swap {
                std::mem::swap(&mut held.hit, &mut entry.hit);
            }
            let own = held.begins && !held.other_only || entry.begins && !entry.other_only;
            held.begins |= entry.begins;
            held.other_only = held.begins && !own;
            held.whole |= entry.whole;
            held.own_whole |= entry.own_whole;
            held.tier = held.tier.max(entry.tier);
            held.fame = held.fame.max(entry.fame);
            continue 'entries;
        }
        kept.push(entry);
    }
    Ok(kept)
}

pub(crate) fn merge_twins(pack: &Reader, hits: Vec<TextSearchHit>) -> Result<Vec<TextSearchHit>> {
    let mut kept: Vec<TextSearchHit> = Vec::with_capacity(hits.len());
    'hits: for hit in hits {
        for held in &mut kept {
            if let Some(swap) = twin(pack, held, &hit)? {
                if swap {
                    *held = hit;
                }
                continue 'hits;
            }
        }
        kept.push(hit);
    }
    Ok(kept)
}

fn twin(pack: &Reader, held: &TextSearchHit, entry: &TextSearchHit) -> Result<Option<bool>> {
    let records = pack.records();
    let (held_place, entry_place) = (is_place(&held.record.layer), is_place(&entry.record.layer));
    let poi = |hit: &TextSearchHit| hit.record.layer == Layer::Poi.as_str();
    let branch = |hit: &TextSearchHit| {
        hit.record.brand.is_some()
            || hit
                .record
                .category
                .as_deref()
                .is_some_and(is_branch_category)
    };
    if !(held_place || poi(held)) || !(entry_place || poi(entry)) || branch(held) || branch(entry) {
        return Ok(None);
    }
    let item = |hit: &TextSearchHit| records.wikidata(hit.record_id);
    if let (Some(left), Some(right)) = (item(held)?, item(entry)?)
        && left == right
    {
        return Ok(Some(match held_place && entry_place {
            true => fineness(&entry.record.layer) > fineness(&held.record.layer),
            false => names_count(pack, entry)? > names_count(pack, held)?,
        }));
    }
    let outer = |hit: &TextSearchHit| hit.record.layer == Layer::District.as_str();
    let town = |hit: &TextSearchHit| hit.record.layer == Layer::Locality.as_str();
    let (district, locality, swap) = match (outer(held), town(held), outer(entry), town(entry)) {
        (true, _, _, true) => (held, entry, true),
        (_, true, true, _) => (entry, held, false),
        _ => return Ok(None),
    };
    let near = district
        .record
        .point
        .zip(locality.record.point)
        .is_some_and(|(a, b)| haversine_m(a.lon, a.lat, b.lon, b.lat) <= REACH_BANDS_M[0]);
    if !near {
        return Ok(None);
    }
    let names = |hit: &TextSearchHit| -> Result<HashSet<String>> {
        let alt_names = match records.record(hit.record_id)? {
            crate::record::Record::Place(_, record) => record.alt_names,
            _ => Vec::new(),
        };
        Ok(std::iter::once(hit.record.name.as_str())
            .chain(pack.language_names(hit.record_id))
            .chain(alt_names.iter().map(String::as_str))
            .filter_map(search_key)
            .collect())
    };
    if names(district)?.is_disjoint(&names(locality)?) {
        return Ok(None);
    }
    let inside = pack
        .boundary_context(locality.record_id)?
        .is_some_and(|context| {
            context
                .admin_context
                .entries()
                .iter()
                .any(|area| area.record_id == district.record_id)
        });
    Ok(inside.then_some(swap))
}

fn names_count(pack: &Reader, hit: &TextSearchHit) -> Result<usize> {
    let alt_names = match pack.records().record(hit.record_id)? {
        crate::record::Record::Poi(record) => record.alt_names.len(),
        crate::record::Record::Place(_, record) => record.alt_names.len(),
        _ => 0,
    };
    Ok(pack.language_names(hit.record_id).count() + alt_names)
}

fn fineness(layer: &str) -> usize {
    [
        Layer::Country,
        Layer::Region,
        Layer::District,
        Layer::Neighbourhood,
        Layer::Locality,
    ]
    .iter()
    .position(|place| place.as_str() == layer)
    .unwrap_or(0)
}

pub(crate) fn country_of(pack: &Reader, record_id: RecordId) -> Result<Option<CountryCode>> {
    if let Some(code) = pack.country_code(record_id) {
        return Ok(Some(code));
    }
    Ok(pack.boundary_context(record_id)?.and_then(|context| {
        context
            .admin_context
            .entries()
            .iter()
            .find_map(|entry| pack.country_code(entry.record_id))
    }))
}

fn reach_tier(importance: u8, distance: Option<f64>, same_country: bool) -> Tier {
    let reach = reach_band(importance);
    let at = match distance {
        Some(distance) => distance_band(distance) + usize::from(!same_country),
        None => REACH_BANDS_M.len(),
    }
    .min(REACH_BANDS_M.len());
    if at + HOME_DEPTH - 1 <= reach {
        Tier::Home
    } else if at <= reach {
        Tier::Reach
    } else {
        Tier::Out
    }
}

fn legacy_tier(hit: &TextSearchHit, distance: Option<f64>, whole: bool) -> Tier {
    let Some(distance) = distance else {
        return Tier::Out;
    };
    let near = usize::from(super::focus_band(distance)) < BIAS_BANDS;
    let own = hit.named_place && whole && hit.record.layer != Layer::Neighbourhood.as_str();
    match (near, own) {
        (true, true) => Tier::Here,
        (true, false) => Tier::Reach,
        _ => Tier::Out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shop_or_cafe_is_a_branch_and_a_monument_is_not() {
        assert!(is_branch_category("shop:supermarket"));
        assert!(is_branch_category("amenity:cafe"));
        assert!(!is_branch_category("historic:monument"));
        assert!(!is_branch_category("aeroway:aerodrome"));
    }

    #[test]
    fn a_place_better_known_reaches_farther() {
        assert_eq!(reach_band(77), 0);
        assert_eq!(reach_band(114), 3);
        assert_eq!(reach_band(144), 6);
        assert_eq!(reach_band(150), REACH_BANDS_M.len());
    }

    #[test]
    fn a_caller_deep_inside_a_places_reach_is_at_home() {
        assert_eq!(reach_tier(144, Some(168_000.0), false), Tier::Home);
        assert_eq!(reach_tier(144, Some(650_000.0), false), Tier::Reach);
        assert_eq!(reach_tier(143, Some(650_000.0), true), Tier::Home);
        assert_eq!(reach_tier(77, Some(600_000.0), true), Tier::Out);
        assert_eq!(reach_tier(150, Some(17_000_000.0), false), Tier::Reach);
        assert_eq!(reach_tier(169, Some(5_570_000.0), false), Tier::Reach);
        assert_eq!(reach_tier(169, None, false), Tier::Reach);
        assert_eq!(reach_tier(144, None, false), Tier::Out);
    }
}
