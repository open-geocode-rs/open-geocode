use super::*;

#[derive(Debug)]
struct NamedGroup {
    label: Label,
    places: Vec<Place>,
}

#[derive(Debug, Default)]
pub(super) struct NamedPlaces {
    country: Option<String>,
    groups: Vec<NamedGroup>,
}

impl NamedPlaces {
    fn is_empty(&self) -> bool {
        self.country.is_none() && self.groups.is_empty()
    }

    fn has(&self, label: Label) -> bool {
        self.groups.iter().any(|group| group.label == label)
    }

    fn holds(
        &self,
        structured: &StructuredIndex,
        point: [f64; 2],
        country: Option<&str>,
        level: Level,
    ) -> bool {
        if let (Some(named), Some(country)) = (&self.country, country)
            && !named.eq_ignore_ascii_case(country)
        {
            return false;
        }
        let lon = (point[0] * 1e7).round() as i32;
        let lat = (point[1] * 1e7).round() as i32;
        let cell = structured::cell(lon, lat);
        self.groups
            .iter()
            .filter(|group| level.keeps(group.label))
            .all(|group| {
                group
                    .places
                    .iter()
                    .any(|place| structured.place_holds(place, cell, lon, lat))
            })
    }

    fn levels(&self) -> Vec<Level> {
        let mut levels = vec![Level::All];
        if self.has(Label::Postcode) && self.has(Label::Locality) {
            levels.extend([Level::Postcode, Level::Town]);
        }
        levels.push(Level::Country);
        levels
    }

    fn keep<T>(
        &self,
        structured: &StructuredIndex,
        items: &mut Vec<T>,
        point_of: impl Fn(&T) -> Option<[f64; 2]>,
        country_of: impl Fn(&T) -> Option<String>,
    ) {
        if self.is_empty() {
            return;
        }
        let holds = |item: &T, level: Level| {
            point_of(item).is_some_and(|point| {
                self.holds(structured, point, country_of(item).as_deref(), level)
            })
        };
        for level in self.levels() {
            if items.iter().any(|item| holds(item, level)) {
                items.retain(|item| holds(item, level));
                return;
            }
        }
        items.clear();
    }
}

#[derive(Debug, Clone, Copy)]
enum Level {
    All,
    Postcode,
    Town,
    Country,
}

impl Level {
    fn keeps(self, label: Label) -> bool {
        match self {
            Self::All => matches!(label, Label::Postcode | Label::Locality),
            Self::Postcode => label == Label::Postcode,
            Self::Town => label == Label::Locality,
            Self::Country => false,
        }
    }
}

fn placeholder_postcode(words: &str) -> bool {
    let digits = words.chars().filter(|character| !character.is_whitespace());
    let mut any = false;
    for character in digits {
        if character != '0' {
            return false;
        }
        any = true;
    }
    any
}

fn hit_country(hit: &TextSearchHit) -> Option<String> {
    if let Some(country) = hit.features.country {
        return Some(country.as_str().to_string());
    }
    hit.record
        .address
        .country
        .as_deref()
        .filter(|country| country.len() == 2)
        .map(str::to_string)
}

impl TextSearcher {
    pub(super) fn written_country(&self, pieces: &[Tagged]) -> Option<(String, Range<usize>)> {
        let dictionaries = self.tagger.dictionaries();
        let covered = |country: &CountryCode| dictionaries.country_weight(*country) > 0.0;
        let country_of = |pieces: &[Tagged]| {
            let words = pieces
                .iter()
                .map(|piece| piece.token.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            dictionary_key(&words)
                .and_then(|key| dictionaries.get(NameField::Country, &key))
                .and_then(|entry| entry.country)
                .filter(covered)
        };
        let mut ends = vec![pieces.len()];
        if pieces.len() > 1
            && matches!(
                pieces[pieces.len() - 1].label,
                Label::Postcode | Label::Country
            )
        {
            ends.push(pieces.len() - 1);
        }
        for end in ends {
            for take in (1..=3.min(end)).rev() {
                let window = &pieces[end - take..end];
                if window.iter().any(|piece| {
                    matches!(
                        piece.label,
                        Label::Region
                            | Label::Postcode
                            | Label::Street
                            | Label::HouseNumber
                            | Label::Unit
                    )
                }) {
                    continue;
                }
                if let Some(country) = country_of(window) {
                    return Some((country.as_str().to_ascii_uppercase(), end - take..end));
                }
            }
        }
        None
    }

    pub(super) fn named_places(
        &self,
        structured: &StructuredIndex,
        reading: &Reading,
    ) -> NamedPlaces {
        let pieces = &reading.pieces;
        let (stated, stated_at) = match self.written_country(pieces) {
            Some((country, at)) => (Some(country), at),
            None => (None, pieces.len()..pieces.len()),
        };
        let mut groups = Vec::<NamedGroup>::new();
        let mut at = 0;
        while at < pieces.len() {
            let label = pieces[at].label;
            let mut end = at + 1;
            while end < pieces.len() && pieces[end].label == label && !stated_at.contains(&end) {
                end += 1;
            }
            let run = at..end;
            at = end;
            if !matches!(label, Label::Locality | Label::Region | Label::Postcode)
                || stated_at.contains(&run.start)
            {
                continue;
            }
            let words = pieces[run]
                .iter()
                .map(|piece| piece.token.clone())
                .collect::<Vec<_>>();
            let key = words.join(" ");
            if label == Label::Postcode && placeholder_postcode(&key) {
                continue;
            }
            let mut places = structured.places(&key, &[], false);
            if places.is_empty() && words.len() > 1 && label == Label::Postcode {
                places = structured.places(&words.concat(), &[], false);
            }
            if !places.is_empty() {
                groups.push(NamedGroup { label, places });
            }
        }
        let mut votes = HashMap::<String, usize>::new();
        if let Some(country) = &stated {
            *votes.entry(country.clone()).or_default() += 1;
        }
        for group in &groups {
            let mut seen = HashSet::new();
            for place in group
                .places
                .iter()
                .filter(|place| !place.country.is_empty())
            {
                let country = place.country.to_ascii_uppercase();
                if seen.insert(country.clone()) {
                    *votes.entry(country).or_default() += 1;
                }
            }
        }
        let best = votes.values().copied().max().unwrap_or(0);
        let mut top = votes
            .into_iter()
            .filter(|(_, count)| *count == best)
            .map(|(country, _)| country);
        let country = match (top.next(), top.next()) {
            (Some(country), None) => Some(country),
            _ => None,
        };
        if let Some(country) = &country {
            for group in &mut groups {
                group
                    .places
                    .retain(|place| place.country.eq_ignore_ascii_case(country));
            }
            groups.retain(|group| !group.places.is_empty());
        }
        NamedPlaces { country, groups }
    }

    pub(super) fn structured_named(
        structured: &StructuredIndex,
        named: &NamedPlaces,
        found: &mut Vec<StructuredFound>,
    ) {
        named.keep(
            structured,
            found,
            |found| Some(found.point),
            |found| hit_country(&found.hit),
        );
    }

    pub(super) fn pool_named(structured: &StructuredIndex, named: &NamedPlaces, pool: &mut Pool) {
        named.keep(
            structured,
            &mut pool.candidates,
            |candidate| {
                candidate
                    .hit
                    .record
                    .point
                    .map(|point| [point.lon, point.lat])
            },
            |candidate| hit_country(&candidate.hit),
        );
    }
}
