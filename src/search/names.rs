use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Naming {
    AsWritten,
    Whole,
    BarType,
    Misspelt,
    InPart,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Placing {
    Placed,
    ByLonger,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum PlacedBy {
    Whole,
    BarType,
    Unknown,
}

#[derive(Debug, Clone)]
struct Part {
    key: String,
    label: Label,
    areas: HashSet<RecordId>,
    named: Vec<RecordId>,
}

impl Part {
    fn names_anything(&self) -> bool {
        !self.areas.is_empty() || !self.named.is_empty()
    }
}

#[derive(Debug)]
pub(super) struct NameOrder {
    parts: Vec<Part>,
    found: Vec<TextSearchHit>,
    written: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Keys {
    unnamed: bool,
    placing: Placing,
    naming: Naming,
    placed_by: PlacedBy,
    alone: usize,
}

impl TextSearcher {
    pub(super) fn name_order(
        &self,
        query_text: &str,
        pieces: &[Tagged],
        filters: &Filters<'_>,
        focus: Option<[f64; 2]>,
    ) -> Result<Option<NameOrder>> {
        let mut parts = Vec::new();
        let mut found = Vec::<TextSearchHit>::new();
        let mut fields = query_fields(query_text, pieces).into_iter().peekable();
        while let Some(field) = fields.next() {
            if let Some(next) = fields.peek()
                && let Some((part, hits)) =
                    self.literal_comma_locality(&field, next, filters, focus)?
            {
                fields.next();
                for hit in hits {
                    if !found.iter().any(|held| held.record_id == hit.record_id) {
                        found.push(hit);
                    }
                }
                parts.push(part);
                continue;
            }
            let mut runs = Vec::new();
            for (words, label) in &field {
                runs.extend(self.part(words, *label, filters, focus)?);
            }
            let mut naming_whole = 0;
            for run in &runs {
                naming_whole += usize::from(self.names_whole(run)?);
            }
            let whole = match runs.len() > 1 {
                true => {
                    let words = field
                        .iter()
                        .flat_map(|(words, _)| words.iter().cloned())
                        .collect::<Vec<_>>();
                    let label = field
                        .iter()
                        .max_by_key(|(words, _)| words.len())
                        .map_or(Label::Other, |(_, label)| *label);
                    self.part(&words, label, filters, focus)?
                }
                false => None,
            };
            let field_parts = match whole {
                Some(whole) if naming_whole < 2 || self.names_whole(&whole)? => vec![whole],
                _ => runs,
            };
            for (part, hits) in field_parts {
                for hit in hits {
                    if !found.iter().any(|held| held.record_id == hit.record_id) {
                        found.push(hit);
                    }
                }
                parts.push(part);
            }
        }
        if !parts.iter().any(Part::names_anything) {
            return Ok(None);
        }
        Ok(Some(NameOrder {
            parts,
            found,
            written: format!(" {} ", exact_key(query_text).unwrap_or_default()),
        }))
    }

    fn literal_comma_locality(
        &self,
        left: &[(Vec<String>, Label)],
        right: &[(Vec<String>, Label)],
        filters: &Filters<'_>,
        focus: Option<[f64; 2]>,
    ) -> Result<Option<(Part, Vec<TextSearchHit>)>> {
        let words = |field: &[(Vec<String>, Label)]| {
            field
                .iter()
                .flat_map(|(words, _)| words.iter().cloned())
                .collect::<Vec<_>>()
        };
        let left = words(left);
        let right = words(right);
        let joined = [&left[..], &right[..]].concat();
        let Some(key) = lattice::dictionary_key(&joined.join(" ")) else {
            return Ok(None);
        };
        if !self
            .tagger
            .dictionaries()
            .contains(NameField::Locality, &key)
        {
            return Ok(None);
        }
        let (Some(first), Some(last)) = (search_key(&left.join(" ")), search_key(&right.join(" ")))
        else {
            return Ok(None);
        };
        let Some((part, hits)) = self.part(&joined, Label::Locality, filters, focus)? else {
            return Ok(None);
        };
        for hit in &hits {
            if hit.record.layer == Layer::Locality.as_str()
                && self
                    .pack
                    .records()
                    .names(hit.record_id)?
                    .iter()
                    .any(|name| {
                        name.split_once(',').is_some_and(|(a, b)| {
                            search_key(a).as_deref() == Some(&first)
                                && search_key(b).as_deref() == Some(&last)
                        })
                    })
            {
                return Ok(Some((part, hits)));
            }
        }
        Ok(None)
    }

    fn names_whole(&self, (part, hits): &(Part, Vec<TextSearchHit>)) -> Result<bool> {
        for hit in hits {
            if self.naming(&part.key, hit.record_id, &hit.record.layer)? == Naming::Whole {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn sought(&self, order: &NameOrder, hits: &[TextSearchHit]) -> Result<usize> {
        let records = hits.iter().chain(&order.found).collect::<Vec<_>>();
        let mut named = Vec::with_capacity(order.parts.len());
        for part in &order.parts {
            let mut by_part = part.named.clone();
            for hit in &records {
                if self.naming(&part.key, hit.record_id, &hit.record.layer)? <= Naming::Misspelt {
                    by_part.push(hit.record_id);
                }
            }
            named.push(by_part);
        }
        let names = |at: usize| order.parts[at].names_anything() || !named[at].is_empty();
        let mut first = None;
        for (at, part) in order.parts.iter().enumerate() {
            if !names(at) {
                continue;
            }
            first.get_or_insert(at);
            let mut holds = false;
            'others: for (other_at, other) in named.iter().enumerate() {
                if other_at == at {
                    continue;
                }
                for &record in other {
                    if self
                        .areas_holding(record)?
                        .iter()
                        .any(|area| part.areas.contains(area))
                    {
                        holds = true;
                        break 'others;
                    }
                }
            }
            if !holds {
                return Ok(at);
            }
        }
        Ok(first.unwrap_or_default())
    }

    fn part(
        &self,
        words: &[String],
        label: Label,
        filters: &Filters<'_>,
        focus: Option<[f64; 2]>,
    ) -> Result<Option<(Part, Vec<TextSearchHit>)>> {
        let Some(key) = search_key(&words.join(" ")) else {
            return Ok(None);
        };
        let dictionaries = self.tagger.dictionaries();
        let mut areas = HashSet::new();
        for spelling in spellings(&key) {
            if let Some(dictionary_key) = lattice::dictionary_key(&spelling) {
                areas.extend(
                    dictionaries
                        .areas(&dictionary_key)
                        .into_iter()
                        .chain(dictionaries.regions(&dictionary_key))
                        .map(RecordId::from),
                );
            }
            if let Some(code) = country_code(&spelling) {
                areas.extend(self.pack.countries_of(code));
            }
        }
        let mut hits = self.records_named(&key, filters, focus)?;
        if key.chars().any(|c| c.is_ascii_digit())
            && let Some(code) = crate::text_index::compact_postcode(&key)
        {
            for record in self.postcode_records(&[code], NAMED_WINDOW)? {
                let summary = self.pack.record_summary(record)?;
                if !hits.iter().any(|held| held.record_id == record) {
                    hits.push(TextSearchHit::of_record(record, summary));
                }
            }
        }
        let mut given = areas.iter().copied().collect::<Vec<_>>();
        given.sort();
        for record in given {
            if !hits.iter().any(|held| held.record_id == record) {
                hits.push(TextSearchHit::of_record(
                    record,
                    self.pack.record_summary(record)?,
                ));
            }
        }
        let mut named = Vec::with_capacity(hits.len());
        for hit in &hits {
            if is_area(&hit.record.layer) {
                areas.insert(hit.record_id);
            }
            named.push(hit.record_id);
        }
        Ok(Some((
            Part {
                key,
                label,
                areas,
                named,
            },
            hits,
        )))
    }

    fn records_named(
        &self,
        key: &str,
        filters: &Filters<'_>,
        focus: Option<[f64; 2]>,
    ) -> Result<Vec<TextSearchHit>> {
        let mut named = Vec::new();
        let mut keys = vec![key.to_string()];
        for kind in [Kind::Area, Kind::Street] {
            keys.extend(type_free(key, kind, self.tagger.dictionaries()));
        }
        for asked in keys {
            for (hit, _) in self.by_name(&asked, &asked, filters, focus, NAMED_WINDOW, &[])? {
                if self.naming(key, hit.record_id, &hit.record.layer)? <= Naming::BarType
                    && !named
                        .iter()
                        .any(|held: &TextSearchHit| held.record_id == hit.record_id)
                {
                    named.push(hit);
                }
            }
        }
        Ok(named)
    }

    fn naming(&self, key: &str, record: RecordId, layer: &str) -> Result<Naming> {
        let mut best = Naming::None;
        for spelling in spellings(key) {
            best = best.min(self.naming_as(&spelling, record, layer)?);
        }
        Ok(best)
    }

    fn is_code_of(&self, key: &str, record: RecordId, layer: &str) -> bool {
        if layer == Layer::Country.as_str() {
            return country_code(key)
                .is_some_and(|code| self.pack.countries_of(code).contains(&record));
        }
        layer == Layer::Region.as_str()
            && lattice::dictionary_key(key).is_some_and(|key| {
                self.tagger
                    .dictionaries()
                    .regions(&key)
                    .into_iter()
                    .any(|region| RecordId::from(region) == record)
            })
    }

    fn naming_as(&self, key: &str, record: RecordId, layer: &str) -> Result<Naming> {
        if self.is_code_of(key, record, layer) {
            return Ok(Naming::Whole);
        }
        let dictionaries = self.tagger.dictionaries();
        let part = key.split_whitespace().collect::<Vec<_>>();
        let kind = Kind::of(layer);
        let part_free = kind.map_or_else(Vec::new, |kind| type_free(key, kind, dictionaries));
        let mut best = Naming::None;
        let mut names = self.pack.records().names(record)?;
        if layer == Layer::Postcode.as_str() {
            names.push(self.pack.record_summary(record)?.name);
        }
        for name in names {
            let Some(name) = search_key(&name) else {
                continue;
            };
            let naming = if name == key {
                Naming::Whole
            } else if part_free.contains(&name)
                || kind.is_some_and(|kind| {
                    type_free(&name, kind, dictionaries)
                        .iter()
                        .any(|free| free == key)
                })
            {
                Naming::BarType
            } else if part.iter().all(|word| {
                name.split_whitespace()
                    .any(|own| own == *word || typo_distance(word, own).is_some())
            }) {
                match name.split_whitespace().count() == part.len() {
                    true => Naming::Misspelt,
                    false => Naming::InPart,
                }
            } else {
                Naming::None
            };
            best = best.min(naming);
        }
        Ok(best)
    }

    fn areas_holding(&self, record: RecordId) -> Result<Vec<RecordId>> {
        Ok(self
            .pack
            .boundary_context(record)?
            .map(|context| {
                context
                    .admin_context
                    .entries()
                    .iter()
                    .map(|entry| entry.record_id)
                    .collect()
            })
            .unwrap_or_default())
    }

    fn placing(
        &self,
        order: &NameOrder,
        sought: usize,
        record: RecordId,
    ) -> Result<(Placing, PlacedBy)> {
        let holding = self.areas_holding(record)?;
        let mut placing = Placing::Placed;
        let mut placed_by = PlacedBy::Whole;
        for (at, part) in order.parts.iter().enumerate() {
            if at == sought || !part.names_anything() {
                continue;
            }
            let own = coarseness(&self.pack.record_summary(record)?.layer);
            let mut could_hold = HashSet::new();
            for &area in &part.areas {
                let layer = self.pack.record_summary(area)?.layer;
                if coarseness(&layer) < own {
                    could_hold.insert(layer);
                }
            }
            let mut held_by_kind = false;
            for &area in &holding {
                held_by_kind |= could_hold.contains(&self.pack.record_summary(area)?.layer);
            }
            if !could_hold.is_empty() && !held_by_kind {
                placed_by = placed_by.max(PlacedBy::Unknown);
                continue;
            }
            let mut best = None::<Naming>;
            for &area in holding.iter().filter(|area| part.areas.contains(area)) {
                let layer = self.pack.record_summary(area)?.layer;
                let naming = self.naming(&part.key, area, &layer)?;
                best = Some(best.map_or(naming, |held| held.min(naming)));
            }
            if let Some(naming) = best {
                placed_by = placed_by.max(match naming {
                    Naming::Whole => PlacedBy::Whole,
                    _ => PlacedBy::BarType,
                });
                continue;
            }
            let longer = self.placing_by_longer(part, &holding)?;
            placing = placing.max(match longer {
                true => Placing::ByLonger,
                false => Placing::Not,
            });
        }
        Ok((placing, placed_by))
    }

    fn placing_by_longer(&self, part: &Part, holding: &[RecordId]) -> Result<bool> {
        let words = part.key.split_whitespace().collect::<Vec<_>>();
        for &area in holding {
            for name in self.pack.records().names(area)? {
                let Some(name) = search_key(&name) else {
                    continue;
                };
                let own = name.split_whitespace().collect::<Vec<_>>();
                if own.len() > words.len() && words.iter().all(|word| own.contains(word)) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn writes_a_name(&self, key: &str, hit: &TextSearchHit) -> Result<bool> {
        let dictionaries = self.tagger.dictionaries();
        let street = Kind::of(&hit.record.layer) == Some(Kind::Street);
        let written = key.split_whitespace().collect::<HashSet<_>>();
        for name in self.pack.records().names(hit.record_id)? {
            let Some(name) = search_key(&name) else {
                continue;
            };
            if name.split_whitespace().all(|word| {
                written.contains(word)
                    || (street
                        && dictionaries.is_street_type(word)
                        && !dictionaries.is_direction(word))
            }) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn written_as(&self, order: &NameOrder, record: RecordId) -> Result<bool> {
        Ok(self.pack.records().names(record)?.iter().any(|name| {
            exact_key(name).is_some_and(|name| order.written.contains(&format!(" {name} ")))
        }))
    }

    fn keys(&self, order: &NameOrder, sought: usize, hit: &TextSearchHit) -> Result<Keys> {
        let part = &order.parts[sought];
        let naming = match self.naming(&part.key, hit.record_id, &hit.record.layer)? {
            Naming::Whole if self.written_as(order, hit.record_id)? => Naming::AsWritten,
            naming => naming,
        };
        let (placing, placed_by) = self.placing(order, sought, hit.record_id)?;
        Ok(Keys {
            unnamed: naming == Naming::None,
            placing,
            naming,
            placed_by,
            alone: usize::MAX,
        })
    }

    pub(super) fn order_by_names(
        &self,
        order: &NameOrder,
        options: &TextSearchOptions,
        depth: Depth,
        hits: &mut Vec<TextSearchHit>,
    ) -> Result<()> {
        let sought = self.sought(order, hits)?;
        let searched = hits.len();
        for hit in &order.found {
            if !hits.iter().any(|held| held.record_id == hit.record_id) {
                hits.push(hit.clone());
            }
        }
        for hit in hits.iter_mut() {
            let part = &order.parts[sought];
            if self.naming(&part.key, hit.record_id, &hit.record.layer)? < Naming::None {
                hit.matched = self.judged_by_names(order, sought, hit)?;
            }
        }
        let first = match order.parts.len() {
            1 => {
                let part = &order.parts[sought];
                let mut whole = Vec::with_capacity(searched);
                for hit in &hits[..searched] {
                    let naming = self.naming(&part.key, hit.record_id, &hit.record.layer)?;
                    whole.push(naming > Naming::Whole);
                }
                let mut searched_hits = hits.drain(..searched).zip(whole).collect::<Vec<_>>();
                searched_hits.sort_by_key(|(_, rest)| *rest);
                hits.splice(0..0, searched_hits.into_iter().map(|(hit, _)| hit));
                searched
            }
            _ => 0,
        };
        let mut keyed = Vec::with_capacity(hits.len() - first);
        for (at, hit) in hits.drain(first..).enumerate() {
            keyed.push((self.keys(order, sought, &hit)?, at, hit));
        }
        keyed.sort_by_key(|(keys, at, _)| (order_key(*keys), *at));
        let shown = (effective_limit(options.limit) + 1).min(keyed.len());
        let tied = keyed[..shown]
            .windows(2)
            .any(|pair| !pair[0].0.unnamed && order_key(pair[0].0) == order_key(pair[1].0));
        let locality_in_country = order.parts.len() == 2
            && order.parts[1 - sought].label == Label::Country
            && matches!(
                keyed.as_slice(),
                [(first, _, locality), (second, _, area), ..]
                    if order_key(*first) == order_key(*second)
                        && locality.record.layer == Layer::Locality.as_str()
                        && matches!(
                            Layer::parse(&area.record.layer),
                            Some(Layer::District | Layer::Region)
                        )
            );
        if tied && order.parts.len() > 1 && !locality_in_country && depth == Depth::Query {
            let bare = self.search_at(
                &TextSearchOptions {
                    query: order.parts[sought].key.clone(),
                    ..options.clone()
                },
                Depth::Bare,
            )?;
            for (keys, _, hit) in &mut keyed {
                keys.alone = bare
                    .iter()
                    .position(|held| held.record_id == hit.record_id)
                    .unwrap_or(usize::MAX);
            }
            keyed.sort_by_key(|(keys, at, _)| (order_key(*keys), *at));
        }
        hits.extend(keyed.into_iter().map(|(_, _, hit)| hit));
        Ok(())
    }

    pub(super) fn answers_first(
        &self,
        order: &NameOrder,
        answer: &[TextSearchHit],
    ) -> Result<bool> {
        let Some(first) = answer.first() else {
            return Ok(false);
        };
        let sought = self.sought(order, answer)?;
        let ahead = |hit: &TextSearchHit| -> Result<_> {
            let (unnamed, placing, naming, placed_by, _) =
                order_key(self.keys(order, sought, hit)?);
            Ok((unnamed, placing, naming, placed_by))
        };
        let first_key = ahead(first)?;
        for hit in answer.iter().skip(1).chain(&order.found) {
            if hit.record_id != first.record_id && ahead(hit)? <= first_key {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn judged_by_names(
        &self,
        order: &NameOrder,
        sought: usize,
        hit: &TextSearchHit,
    ) -> Result<QueryMatch> {
        let mut matched = QueryMatch::default();
        let holding = self.areas_holding(hit.record_id)?;
        for (at, part) in order.parts.iter().enumerate() {
            let (how, layer) = if at == sought {
                let how = match self.naming(&part.key, hit.record_id, &hit.record.layer)? {
                    Naming::AsWritten | Naming::Whole => PartMatch::Matched,
                    Naming::BarType => match self.writes_a_name(&part.key, hit)? {
                        true => PartMatch::Matched,
                        false => PartMatch::Partial,
                    },
                    Naming::Misspelt | Naming::InPart => PartMatch::Partial,
                    Naming::None => PartMatch::Unmatched,
                };
                (how, Some(hit.record.layer.clone()))
            } else if let Some(&area) = holding.iter().find(|area| part.areas.contains(area)) {
                (
                    PartMatch::Matched,
                    Some(self.pack.record_summary(area)?.layer),
                )
            } else {
                let how = match self.placing_by_longer(part, &holding)? {
                    true => PartMatch::Partial,
                    false => PartMatch::Unmatched,
                };
                let mut named = Vec::new();
                for &area in &part.areas {
                    named.push(self.pack.record_summary(area)?.layer);
                }
                let layer = [
                    Layer::Postcode,
                    Layer::Locality,
                    Layer::Region,
                    Layer::Country,
                ]
                .into_iter()
                .find(|layer| named.iter().any(|named| named == layer.as_str()))
                .map(|layer| layer.as_str().to_string());
                (how, layer)
            };
            let slot = layer.as_deref().and_then(slot_label).unwrap_or(part.label);
            if let Some(slot) = part_of(&mut matched, slot) {
                *slot = Some(slot.map_or(how, |held| held.max(how)));
            }
        }
        Ok(matched)
    }
}

fn order_key(keys: Keys) -> (bool, Placing, Naming, PlacedBy, usize) {
    match keys.unnamed {
        true => (
            true,
            Placing::Placed,
            Naming::None,
            PlacedBy::Whole,
            usize::MAX,
        ),
        false => (false, keys.placing, keys.naming, keys.placed_by, keys.alone),
    }
}

fn spellings(key: &str) -> Vec<String> {
    let mut spellings = vec![key.to_string()];
    let words = key.split_whitespace().collect::<Vec<_>>();
    if words.len() > 1 && words.iter().all(|word| word.chars().count() == 1) {
        spellings.push(words.concat());
    }
    spellings
}

fn country_code(key: &str) -> Option<CountryCode> {
    if !key.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    match key.len() {
        2 => CountryCode::parse(key),
        3 => crate::iso::alpha2_of(&key.to_ascii_uppercase()).and_then(CountryCode::parse),
        _ => None,
    }
}

fn coarseness(layer: &str) -> usize {
    [
        Layer::Country,
        Layer::Region,
        Layer::District,
        Layer::Locality,
        Layer::Neighbourhood,
    ]
    .iter()
    .position(|area| area.as_str() == layer)
    .unwrap_or(usize::MAX)
}

fn slot_label(layer: &str) -> Option<Label> {
    [
        (Layer::Street, Label::Street),
        (Layer::Locality, Label::Locality),
        (Layer::Region, Label::Region),
        (Layer::Postcode, Label::Postcode),
        (Layer::Country, Label::Country),
    ]
    .into_iter()
    .find(|(own, _)| own.as_str() == layer)
    .map(|(_, label)| label)
}

const NAMED_WINDOW: usize = 50;

fn is_area(layer: &str) -> bool {
    [
        Layer::Country,
        Layer::Region,
        Layer::District,
        Layer::Locality,
        Layer::Neighbourhood,
        Layer::Place,
        Layer::Postcode,
    ]
    .iter()
    .any(|area| area.as_str() == layer)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Area,
    Street,
}

impl Kind {
    fn of(layer: &str) -> Option<Self> {
        if is_area(layer) {
            Some(Self::Area)
        } else if layer == Layer::Street.as_str() {
            Some(Self::Street)
        } else {
            None
        }
    }
}

fn type_free(key: &str, kind: Kind, dictionaries: &Dictionaries) -> Vec<String> {
    let mut free = match kind {
        Kind::Area => lattice::dictionary::admin_aliases(key),
        Kind::Street => {
            let words = key.split_whitespace().collect::<Vec<_>>();
            let mut free = Vec::new();
            if words.len() > 1 {
                for (at, word) in words.iter().enumerate() {
                    if dictionaries.is_street_type(word) && !dictionaries.is_direction(word) {
                        free.push(
                            words
                                .iter()
                                .enumerate()
                                .filter(|(other, _)| *other != at)
                                .map(|(_, word)| *word)
                                .collect::<Vec<_>>()
                                .join(" "),
                        );
                    }
                }
            }
            free
        }
    };
    free.retain(|free| free != key && !free.is_empty());
    free.sort();
    free.dedup();
    free
}

fn query_fields(query_text: &str, pieces: &[Tagged]) -> Vec<Vec<(Vec<String>, Label)>> {
    let tokens = Tokens::new(query_text);
    let breaks = match tokens.words.len() == pieces.len() {
        true => tokens.breaks,
        false => vec![false; pieces.len()],
    };
    let mut fields = Vec::<Vec<(Vec<String>, Label)>>::new();
    for (at, piece) in pieces.iter().enumerate() {
        if at == 0 || breaks.get(at).copied().unwrap_or(false) {
            fields.push(Vec::new());
        }
        let field = fields.last_mut().expect("a field");
        match field.last_mut() {
            Some((words, label)) if *label == piece.label => words.push(piece.token.clone()),
            _ => field.push((vec![piece.token.clone()], piece.label)),
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::{LazyLock, OnceLock},
    };

    use crate::{
        builder::{
            addresses::AddressFile,
            osm::{
                BuildOsmOptions, build_osm_pack,
                test_pbf::{TestElement, node, relation, way, write_pbf},
            },
        },
        calibration,
        search::{TextSearchOptions, TextSearcher},
        test_support::TempDir,
    };

    const NOTHING: &str = "Zqxjvbr";

    fn elements() -> Vec<Vec<TestElement>> {
        let (mut nodes, mut ways, mut relations) = (Vec::new(), Vec::new(), Vec::new());
        let mut next_node = 1;
        let mut square = |id: i64,
                          [lon0, lat0, lon1, lat1]: [f64; 4],
                          tags: &[(&'static str, &'static str)],
                          nodes: &mut Vec<TestElement>,
                          ways: &mut Vec<TestElement>,
                          relations: &mut Vec<TestElement>| {
            let first = next_node;
            for (lat, lon) in [(lat0, lon0), (lat0, lon1), (lat1, lon1), (lat1, lon0)] {
                nodes.push(node(next_node, lat, lon, &[]));
                next_node += 1;
            }
            ways.push(way(
                id,
                &[first, first + 1, first + 2, first + 3, first],
                &[],
            ));
            let mut tags = tags.to_vec();
            tags.extend([("boundary", "administrative"), ("type", "boundary")]);
            relations.push(relation(id, &[(id, "outer")], &tags));
        };
        let admin =
            |level: &'static str, name: &'static str| vec![("admin_level", level), ("name", name)];
        for (id, bounds, tags) in [
            (1, [0.0, 40.0, 10.0, 50.0], {
                let mut tags = admin("2", "United States");
                tags.push(("ISO3166-1:alpha2", "US"));
                tags
            }),
            (2, [1.0, 41.0, 9.0, 49.0], {
                let mut tags = admin("4", "Rhode Island");
                tags.push(("ISO3166-2", "US-RI"));
                tags
            }),
            (3, [1.0, 41.0, 3.0, 43.0], admin("6", "Bristol County")),
            (4, [3.0, 41.0, 5.0, 43.0], admin("6", "Washington County")),
            (5, [5.0, 41.0, 9.0, 43.0], admin("6", "Providence County")),
            (10, [1.1, 41.1, 1.9, 41.9], admin("8", "Bristol")),
            (11, [3.1, 41.1, 3.9, 41.9], admin("8", "Exeter")),
            (12, [4.1, 41.1, 4.9, 41.9], admin("8", "Hopkinton")),
            (13, [3.1, 42.1, 3.9, 42.9], admin("8", "Wakefield")),
            (14, [5.1, 41.1, 5.9, 41.9], admin("8", "Providence")),
            (15, [6.1, 41.1, 6.9, 41.9], admin("8", "East Providence")),
            (16, [7.1, 41.1, 7.9, 41.9], admin("8", "Cranston")),
            (17, [5.1, 42.1, 5.9, 42.9], {
                let mut tags = admin("8", "Գյումրի");
                tags.push(("name:en", "Gyumri"));
                tags
            }),
        ] {
            square(id, bounds, &tags, &mut nodes, &mut ways, &mut relations);
        }
        let mut street = |id: i64, name: &'static str, lon: f64, lat: f64| {
            let first = next_node;
            nodes.push(node(first, lat, lon, &[]));
            nodes.push(node(first + 1, lat, lon + 0.1, &[]));
            next_node += 2;
            ways.push(way(
                id,
                &[first, first + 1],
                &[("highway", "residential"), ("name", name)],
            ));
        };
        street(100, "Hope Street", 1.4, 41.5);
        street(101, "Exeter Road", 4.4, 41.5);
        street(102, "Merkholtz", 4.4, 41.6);
        street(103, "Town Wharf", 4.4, 41.7);
        street(104, "Mooresfield Road", 3.4, 42.5);
        street(105, "Main Street", 5.4, 41.5);
        street(106, "Main Street", 6.4, 41.5);
        street(107, "Cranston Street", 7.4, 41.5);
        street(108, "Cranston Street", 5.4, 41.6);
        street(109, "Providence Cranston Road", 3.4, 41.3);
        street(110, "Abovyan Street", 5.4, 42.5);
        street(111, "Quonset Point Road", 1.4, 41.8);
        street(112, "USA Avenue", 5.4, 41.8);
        street(113, "Providence Street", 7.4, 41.8);
        street(114, "Rue de la Grande Vingtaine", 1.4, 41.9);
        for (at, number) in ["1", "2", "3", "4", "5"].into_iter().enumerate() {
            nodes.push(node(
                800 + at as i64,
                41.5001,
                1.41 + at as f64 * 0.01,
                &[
                    ("addr:housenumber", number),
                    ("addr:street", "Hope Street"),
                    ("addr:postcode", "02809"),
                    ("addr:city", "Bristol"),
                ],
            ));
        }
        nodes.push(node(
            900,
            41.5,
            3.5,
            &[("place", "village"), ("name", "Merkholtz")],
        ));
        nodes.push(node(
            901,
            41.5,
            20.0,
            &[("place", "village"), ("name", "Quonset")],
        ));
        nodes.push(node(
            902,
            41.3,
            1.3,
            &[("place", "hamlet"), ("name", "Cà Baldino")],
        ));
        nodes.push(node(
            903,
            41.3,
            1.6,
            &[("place", "hamlet"), ("name", "Ca' Baldino")],
        ));
        nodes.push(node(
            904,
            41.85,
            1.5,
            &[
                ("place", "village"),
                ("name", "La Vingtaine de St. Nicolas"),
            ],
        ));
        nodes.push(node(
            905,
            41.86,
            1.5,
            &[("place", "village"), ("name", "Old Mill, Plainfield")],
        ));
        nodes.push(node(
            906,
            41.87,
            1.5,
            &[("place", "village"), ("name", "Plainfield")],
        ));
        vec![nodes, ways, relations]
    }

    static FIXTURE: LazyLock<TempDir> = LazyLock::new(|| {
        let dir = TempDir::new("names");
        let input = dir.join("fixture.osm.pbf");
        write_pbf(&input, &elements());
        build_osm_pack(&BuildOsmOptions {
            inputs: vec![input],
            pack: dir.join("pack"),
            ..BuildOsmOptions::default()
        })
        .expect("build");
        dir
    });

    fn fixture() -> PathBuf {
        FIXTURE.join("pack")
    }

    fn searcher() -> &'static TextSearcher {
        static SEARCHER: OnceLock<TextSearcher> = OnceLock::new();
        SEARCHER.get_or_init(|| TextSearcher::open(fixture()).expect("searcher"))
    }

    fn ask(searcher: &TextSearcher, query: &str) -> Vec<super::TextSearchHit> {
        searcher
            .search(&TextSearchOptions {
                query: query.to_string(),
                limit: 5,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search")
    }

    fn first(searcher: &TextSearcher, query: &str) -> Option<String> {
        ask(searcher, query)
            .into_iter()
            .next()
            .map(|hit| hit.record.id)
    }

    fn record(searcher: &TextSearcher, id: &str) -> (String, String) {
        let pack = &searcher.pack;
        (0..pack.records().record_count())
            .map(|record| pack.record_summary(record).expect("summary"))
            .find(|summary| summary.id == id)
            .map(|summary| (summary.layer, summary.name))
            .unwrap_or_else(|| panic!("no record {id}"))
    }

    fn answers(query: &str, expected: &str) {
        assert_eq!(
            first(searcher(), query).as_deref(),
            Some(expected),
            "{query}"
        );
    }

    #[test]
    fn a_town_its_state_holds_comes_before_a_road_named_bar_its_type() {
        answers("Exeter, RI", "osm:relation:11");
    }

    #[test]
    fn a_part_that_names_nothing_is_left_out() {
        answers(&format!("Exeter, {NOTHING}, RI"), "osm:relation:11");
    }

    #[test]
    fn the_county_and_the_state_both_hold_the_town() {
        answers("Bristol, Bristol County, RI", "osm:relation:10");
    }

    #[test]
    fn a_whole_name_comes_before_one_named_with_a_type_word_added() {
        answers("Bristol County, RI", "osm:relation:3");
        answers("Bristol, RI", "osm:relation:10");
    }

    #[test]
    fn the_state_is_where_though_written_first() {
        answers("RI, Cranston", "osm:relation:16");
    }

    #[test]
    fn the_postcode_is_the_finest_thing_named() {
        answers("Bristol, RI 02809", "derived:osm:postcode:us:02809");
    }

    #[test]
    fn a_town_holding_the_street_makes_the_street_sought() {
        answers("Cranston Street, Cranston", "osm:way:107");
        answers("Cranston Street, Providence", "osm:way:108");
    }

    #[test]
    fn a_road_is_named_bar_its_type_and_misspelt() {
        answers("Mooresfield, Wakefield", "osm:way:104");
        answers("Mooresfeild Road, Wakefield", "osm:way:104");
    }

    #[test]
    fn a_type_word_may_come_before_or_after_the_name() {
        answers("Town of Bristol, RI", "osm:relation:10");
        answers("Bristol town, RI", "osm:relation:10");
    }

    #[test]
    fn a_name_with_its_state_answers_as_the_name_alone() {
        let bare = first(searcher(), "Merkholtz");
        assert!(bare.is_some());
        assert_eq!(first(searcher(), "Merkholtz, RI"), bare);
    }

    #[test]
    fn the_street_in_the_town_named_comes_before_one_in_a_longer_name() {
        answers("Main Street, Providence", "osm:way:105");
        answers("Main Street, East Providence", "osm:way:106");
    }

    #[test]
    fn a_town_in_a_second_script_answers_to_either_name() {
        answers("Gyumri, RI", "osm:relation:17");
        answers("Գյումրի, RI", "osm:relation:17");
    }

    #[test]
    fn a_town_its_county_holds_comes_before_a_street_its_namesake_city_holds() {
        answers("Cranston, Providence", "osm:relation:16");
    }

    #[test]
    fn a_part_named_only_in_part_never_makes_another_where() {
        answers("Hopkinton, Wharf", "osm:relation:12");
    }

    #[test]
    fn a_comma_in_a_locality_name_stays_inside_it() {
        answers("Old Mill, Plainfield, US", "osm:node:905");
        answers("Plainfield, US", "osm:node:906");
    }

    #[test]
    fn a_field_naming_something_whole_is_one_name() {
        answers("Providence County, RI", "osm:relation:5");
    }

    #[test]
    fn a_country_answers_to_its_codes_and_initials() {
        for query in ["US", "U.S.A."] {
            answers(query, "osm:relation:1");
        }
        let hits = ask(searcher(), "USA");
        assert_eq!(hits[0].record.id, "osm:way:112");
        let country = hits
            .iter()
            .find(|hit| hit.record.id == "osm:relation:1")
            .expect("the country");
        assert_eq!(
            country.matched.country,
            Some(crate::api::PartMatch::Matched)
        );
        answers("Exeter, R.I.", "osm:relation:11");
        answers("Exeter, R I", "osm:relation:11");
        answers("Exeter, U.S.A.", "osm:relation:11");
        answers("Exeter, RI, USA", "osm:relation:11");
    }

    #[test]
    fn a_bare_name_keeps_the_order_the_search_gives() {
        let ids = |query: &str| {
            ask(searcher(), query)
                .into_iter()
                .map(|hit| hit.record.id)
                .collect::<Vec<_>>()
        };
        let bare = ids("Providence");
        let at = |id: &str| {
            bare.iter()
                .position(|held| held == id)
                .unwrap_or(usize::MAX)
        };
        assert_eq!(bare[0], "osm:relation:14", "{bare:?}");
        answers("La Vingtaine de St. Nicolas", "osm:node:904");
        assert!(at("osm:relation:15") < at("osm:way:113"), "{bare:?}");
    }

    #[test]
    fn a_name_written_as_asked_comes_before_one_alike_once_folded() {
        answers("Cà Baldino", "osm:node:902");
        answers("Ca' Baldino", "osm:node:903");
        answers("Cà Baldino, RI", "osm:node:902");
        let one = searcher()
            .search(&TextSearchOptions {
                query: "Cà Baldino".to_string(),
                limit: 1,
                layer: None,
                countries: Vec::new(),
                focus: None,
                bbox: None,
            })
            .expect("search");
        assert_eq!(one[0].record.id, "osm:node:902");
    }

    #[test]
    fn a_place_in_no_area_is_neither_in_nor_out_of_the_state() {
        answers("Quonset, RI", "osm:node:901");
    }

    #[test]
    fn each_part_is_judged_by_what_it_names() {
        use crate::api::PartMatch;
        let matched = |query: &str| ask(searcher(), query).remove(0).matched;
        let usa = matched("Exeter, USA");
        assert_eq!(usa.locality, Some(PartMatch::Matched));
        assert_eq!(usa.country, Some(PartMatch::Matched));
        assert_eq!(usa.region, None);
        let initials = matched("Exeter, R.I.");
        assert_eq!(initials.region, Some(PartMatch::Matched));
        assert_eq!(initials.unit, None);
        assert_eq!(
            matched("Town of Bristol, RI").locality,
            Some(PartMatch::Matched)
        );
        let county = ask(searcher(), "Providence")
            .into_iter()
            .find(|hit| hit.record.id == "osm:relation:5")
            .expect("the county");
        assert_ne!(county.matched.locality, Some(PartMatch::Matched));
        let elsewhere = matched("Hope Street, Exeter");
        assert_eq!(elsewhere.street, Some(PartMatch::Matched));
        assert_eq!(elsewhere.locality, Some(PartMatch::Unmatched));
    }

    fn areas_and_streets(searcher: &TextSearcher) -> Vec<(u64, String, String)> {
        let pack = &searcher.pack;
        (0..pack.records().record_count())
            .filter_map(|record| {
                let summary = pack.record_summary(record).expect("summary");
                (super::is_area(&summary.layer) || summary.layer == "street").then_some((
                    record,
                    summary.id,
                    summary.name,
                ))
            })
            .collect()
    }

    fn sweep<T: Sync>(cases: &[T], check: impl Fn(&T) -> Option<String> + Sync) -> Vec<String> {
        let threads = std::thread::available_parallelism().map_or(4, usize::from);
        std::thread::scope(|scope| {
            cases
                .chunks(cases.len().div_ceil(threads).max(1))
                .map(|chunk| scope.spawn(|| chunk.iter().filter_map(&check).collect::<Vec<_>>()))
                .collect::<Vec<_>>()
                .into_iter()
                .flat_map(|handle| handle.join().expect("thread"))
                .collect()
        })
    }

    #[test]
    fn where_a_record_lies_and_a_part_naming_nothing_never_change_the_answer() {
        let searcher = searcher();
        let mut cases = Vec::new();
        for (record, id, name) in areas_and_streets(searcher) {
            if record_layer(searcher, &id) == "postcode" {
                continue;
            }
            if first(searcher, &name).as_deref() != Some(id.as_str()) {
                continue;
            }
            let mut queries = vec![name.clone()];
            for area in searcher.areas_holding(record).expect("areas") {
                for area_name in searcher.pack.records().names(area).expect("names") {
                    queries.push(format!("{name}, {area_name}"));
                    queries.push(format!("{area_name}, {name}"));
                }
            }
            for query in queries {
                cases.push((format!("{query}, {NOTHING}"), id.clone()));
                cases.push((query, id.clone()));
            }
        }
        assert!(cases.len() > 50, "{} queries", cases.len());
        let wrong = sweep(&cases, |(query, id)| {
            let answer = first(searcher, query);
            (answer.as_deref() != Some(id.as_str()))
                .then(|| format!("{query:?} answers {answer:?}, not {id}"))
        });
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn a_street_with_its_town_keeps_its_answer_with_the_areas_holding_the_town() {
        let searcher = searcher();
        let streets = areas_and_streets(searcher)
            .into_iter()
            .filter(|(_, id, _)| record_layer(searcher, id) == "street")
            .collect::<Vec<_>>();
        let wrong = sweep(&streets, |(record, id, name)| {
            let holding = searcher.areas_holding(*record).expect("areas");
            let town = holding.iter().find(|area| {
                searcher.pack.record_summary(**area).expect("town").layer == "locality"
            })?;
            let town_name = searcher.pack.record_summary(*town).expect("town").name;
            let with_town = format!("{name}, {town_name}");
            if first(searcher, &with_town).as_deref() != Some(id.as_str()) {
                return Some(format!("{with_town:?} does not answer {id}"));
            }
            let wrong = searcher
                .areas_holding(*town)
                .expect("areas")
                .into_iter()
                .filter_map(|area| {
                    let area_name = searcher.pack.record_summary(area).expect("area").name;
                    let query = format!("{with_town}, {area_name}");
                    (first(searcher, &query).as_deref() != Some(id.as_str()))
                        .then(|| format!("{query:?} does not answer {id}"))
                })
                .collect::<Vec<_>>();
            (!wrong.is_empty()).then(|| wrong.join("\n"))
        });
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    fn record_layer(searcher: &TextSearcher, id: &str) -> String {
        record(searcher, id).0
    }

    fn rows(dir: &Path) -> PathBuf {
        let path = dir.join("rows.geojson");
        let rows = (0..5)
            .map(|at| {
                serde_json::json!({
                    "type": "Feature",
                    "properties": {"number": (at + 1).to_string(), "street": "Hope Street",
                                   "city": "Bristol", "postcode": "02809"},
                    "geometry": {"type": "Point", "coordinates": [1.41 + f64::from(at) * 0.01, 41.5001]},
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, rows).expect("rows");
        path
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).expect("dir");
        for entry in std::fs::read_dir(from).expect("read dir") {
            let entry = entry.expect("entry");
            let target = to.join(entry.file_name());
            if entry.file_type().expect("type").is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).expect("copy");
            }
        }
    }

    static CALIBRATED: LazyLock<TempDir> = LazyLock::new(|| {
        let dir = TempDir::new("names-cal");
        let pack = dir.join("pack");
        copy_dir(&fixture(), &pack);
        calibration::run(&calibration::Options {
            pack,
            files: vec![AddressFile {
                dataset: "rows".to_string(),
                path: rows(&dir),
                entry: None,
                area: None,
            }],
            rows: 5,
            variants: 3,
            seed: 1,
            radius_m: 100.0,
        })
        .expect("calibrate");
        dir
    });

    fn calibrated() -> &'static TextSearcher {
        static SEARCHER: OnceLock<TextSearcher> = OnceLock::new();
        SEARCHER.get_or_init(|| {
            let searcher = TextSearcher::open(CALIBRATED.join("pack")).expect("searcher");
            assert!(searcher.confidence_model().is_some(), "calibrated");
            searcher
        })
    }

    #[test]
    fn calibration_never_changes_an_answer_to_a_query_with_no_number() {
        let (plain, calibrated) = (searcher(), calibrated());
        let mut queries = Vec::new();
        for (_, id, name) in areas_and_streets(plain) {
            if record_layer(plain, &id) == "postcode" {
                continue;
            }
            queries.extend([
                name.clone(),
                format!("{name}, RI"),
                format!("{name} RI"),
                format!("{name}, USA"),
            ]);
        }
        let ids = |hits: &[super::TextSearchHit]| {
            hits.iter()
                .map(|hit| hit.record.id.clone())
                .collect::<Vec<_>>()
        };
        let wrong = sweep(&queries, |query| {
            let rated = ask(calibrated, query);
            let (plain_ids, rated_ids) = (ids(&ask(plain, query)), ids(&rated));
            if plain_ids != rated_ids {
                Some(format!("{query:?} {plain_ids:?} != {rated_ids:?}"))
            } else if rated.iter().any(|hit| hit.confidence.is_some()) {
                Some(format!("{query:?} carries a confidence"))
            } else {
                None
            }
        });
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn every_answer_to_a_numbered_query_carries_a_confidence() {
        let calibrated = calibrated();
        for query in [
            "1 Hope Street, Bristol",
            "3 Hope St",
            "2 Hope Street, Bristol, RI 02809",
            "9 Hope Street, Bristol",
            "7 Main Street, Providence",
            "02809, Bristol",
        ] {
            let hits = ask(calibrated, query);
            assert!(!hits.is_empty(), "{query}");
            assert!(
                hits.iter().all(|hit| hit.confidence.is_some()),
                "{query}: {:?}",
                hits.iter()
                    .map(|hit| (&hit.record.id, hit.confidence))
                    .collect::<Vec<_>>()
            );
        }
    }
}
