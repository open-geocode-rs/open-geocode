//! Today's query parser, [`QueryPlan::parse`], as a [`Tagger`], so the parse
//! benchmark can score it against the gold rows and the baselines.
//!
//! The plan holds words by role but not their positions, so the positions
//! are recovered here by retracing its steps on each comma-separated part
//! with the parser's own predicates, and then checked against the plan: a
//! query whose retrace disagrees with the plan gets every token
//! [`Label::Other`] rather than a guess. The roles map as:
//!
//! - the house number to `house_number`;
//! - the required words of the subject part to `street`, whether they name a
//!   street or what is sought ("tim hortons");
//! - unit text, and words with digits after the street type ("fl36"), to
//!   `unit`;
//! - a context part with a digit to `postcode` from its first word with a
//!   digit on, and `region` before that ("on m5c 2a1"); any other context
//!   part to `locality`.
//!
//! The parser needs commas to tell a place from what is sought: without them
//! every word but the house number and unit text is `street`.

use crate::{
    parse::types::{Label, Tagged, Tagger},
    search::{
        QueryPlan, is_postcode_part, is_unit_designator, is_unit_part, is_unit_value_token,
        search_query_variants, split_unit_words,
    },
    text_index::{direction_word, is_direction, is_street_type, normalize_index_text},
};

/// [`QueryPlan::parse`] as a tagger.
#[derive(Debug, Default, Clone, Copy)]
pub struct LegacyTagger;

impl Tagger for LegacyTagger {
    fn tag(&self, query: &str) -> Vec<Tagged> {
        let tokens = normalize_index_text(query)
            .map(|text| text.split(' ').map(str::to_string).collect::<Vec<_>>())
            .unwrap_or_default();
        let labels = legacy_labels(query)
            .filter(|labels| labels.len() == tokens.len())
            .unwrap_or_else(|| vec![Label::Other; tokens.len()]);
        tokens
            .into_iter()
            .zip(labels)
            .map(|(token, label)| Tagged { token, label })
            .collect()
    }
}

/// One label per normalized token of the query, or `None` when the parser
/// finds nothing to search or the retrace disagrees with its plan.
fn legacy_labels(query: &str) -> Option<Vec<Label>> {
    let normalized_parts = query
        .split(',')
        .filter_map(normalize_index_text)
        .map(|part| part.split(' ').count())
        .collect::<Vec<_>>();
    // The variant the search tries first: normalized, abbreviations expanded
    // and fractional house numbers joined ("993_5"), word for word otherwise.
    let variant = search_query_variants(query).into_iter().next()?;
    let plan = QueryPlan::parse(&variant)?;
    let parts = variant
        .split(',')
        .map(|part| part.split_whitespace().collect::<Vec<_>>())
        .filter(|words| !words.is_empty())
        .collect::<Vec<_>>();
    if parts.len() != normalized_parts.len() {
        return None;
    }
    let subject = parts
        .iter()
        .position(|words| !split_unit_words(words).0.is_empty())?;

    let mut context = plan.context.iter();
    let mut labels = Vec::new();
    for (index, (words, &normalized)) in parts.iter().zip(&normalized_parts).enumerate() {
        let part_labels = if index < subject || (index > subject && is_unit_part(words)) {
            vec![Label::Unit; words.len()]
        } else {
            let unit = unit_positions(words)?;
            let identifying = (0..words.len())
                .filter(|position| !unit[*position])
                .collect::<Vec<_>>();
            let identifying_words = identifying
                .iter()
                .map(|position| words[*position].to_string())
                .collect::<Vec<_>>();
            let mut part_labels = vec![Label::Unit; words.len()];
            let roles = if index == subject {
                subject_roles(&plan, &identifying_words)?
            } else {
                if context.next() != Some(&identifying_words) {
                    return None;
                }
                context_roles(&identifying_words)
            };
            for (position, role) in identifying.into_iter().zip(roles) {
                part_labels[position] = role;
            }
            part_labels
        };
        // A joined fraction ("993_5") is one word of the variant and one
        // normalized token per number.
        let mut covered = 0;
        for (word, label) in words.iter().zip(part_labels) {
            let count = word.matches('_').count() + 1;
            labels.extend(std::iter::repeat_n(label, count));
            covered += count;
        }
        if covered != normalized {
            return None;
        }
    }
    (context.next().is_none()).then_some(labels)
}

/// Which words of a part are unit text, as [`split_unit_words`] reads it: a
/// unit designator and the value with a digit after it ("unit 602"). Checked
/// against that function, so the two cannot drift apart.
fn unit_positions(words: &[&str]) -> Option<Vec<bool>> {
    let mut unit = vec![false; words.len()];
    let mut index = 0;
    while index < words.len() {
        if is_unit_designator(words[index])
            && words
                .get(index + 1)
                .is_some_and(|value| is_unit_value_token(value))
        {
            unit[index] = true;
            unit[index + 1] = true;
            index += 2;
        } else {
            index += 1;
        }
    }
    let (identifying, unit_words) = split_unit_words(words);
    let pick = |wanted: bool| {
        words
            .iter()
            .zip(&unit)
            .filter(|(_, is_unit)| **is_unit == wanted)
            .map(|(word, _)| word.to_string())
            .collect::<Vec<_>>()
    };
    (pick(false) == identifying && pick(true) == unit_words).then_some(unit)
}

/// Roles of the subject part's identifying words: the house number, the
/// street, and words with digits after the street type, as
/// [`QueryPlan::parse`] splits them. `None` when they disagree with the plan.
fn subject_roles(plan: &QueryPlan, words: &[String]) -> Option<Vec<Label>> {
    let mut roles = Vec::with_capacity(words.len());
    let mut rest = words;
    if plan.house_number.is_some() {
        roles.push(Label::HouseNumber);
        rest = &words[1..];
    }
    let end = rest
        .iter()
        .enumerate()
        .position(|(index, word)| index > 0 && (is_street_type(word) || is_direction(word)));
    let mut street = Vec::new();
    for (index, word) in rest.iter().enumerate() {
        let after_type = end.is_some_and(|end| index > end);
        if after_type && word.chars().any(|character| character.is_ascii_digit()) {
            roles.push(Label::Unit);
        } else {
            roles.push(Label::Street);
            street.push(word.clone());
        }
    }
    if plan.house_number.is_some()
        && let Some(last) = street.last_mut()
        && let Some(direction) = direction_word(last)
    {
        *last = direction.to_string();
    }
    (street == plan.required).then_some(roles)
}

/// Roles of a context part's words: a part with a digit is a postcode from
/// its first word with a digit on, the words before it a region ("on m5c
/// 2a1"); any other part is a locality.
fn context_roles(words: &[String]) -> Vec<Label> {
    if !is_postcode_part(words) {
        return vec![Label::Locality; words.len()];
    }
    let first_digit = words
        .iter()
        .position(|word| word.chars().any(|character| character.is_ascii_digit()))
        .unwrap_or_default();
    (0..words.len())
        .map(|index| {
            if index < first_digit {
                Label::Region
            } else {
                Label::Postcode
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use Label::*;

    fn tags(query: &str) -> Vec<(String, Label)> {
        LegacyTagger
            .tag(query)
            .into_iter()
            .map(|tagged| (tagged.token, tagged.label))
            .collect()
    }

    fn labels(query: &str) -> Vec<Label> {
        tags(query).into_iter().map(|(_, label)| label).collect()
    }

    #[test]
    fn a_full_address_splits_into_its_parts() {
        assert_eq!(
            tags("30 w 26th st, new york, ny 10010"),
            vec![
                ("30".to_string(), HouseNumber),
                ("w".to_string(), Street),
                ("26th".to_string(), Street),
                ("st".to_string(), Street),
                ("new".to_string(), Locality),
                ("york".to_string(), Locality),
                ("ny".to_string(), Region),
                ("10010".to_string(), Postcode),
            ]
        );
    }

    #[test]
    fn tokens_are_the_normalized_query_whatever_the_expansion() {
        // "St" expands to "street" and "W" to "west" for the search, but the
        // tagger labels the query's own tokens.
        let tagged = tags("79 Wellington St. W FL36, Toronto");
        assert_eq!(
            tagged
                .iter()
                .map(|(token, _)| token.as_str())
                .collect::<Vec<_>>(),
            vec!["79", "wellington", "st", "w", "fl36", "toronto"]
        );
        assert_eq!(
            tagged
                .into_iter()
                .map(|(_, label)| label)
                .collect::<Vec<_>>(),
            vec![HouseNumber, Street, Street, Street, Unit, Locality]
        );
    }

    #[test]
    fn unit_text_is_unit_wherever_it_is() {
        assert_eq!(
            labels("unit 602, 100 king st, toronto"),
            vec![Unit, Unit, HouseNumber, Street, Street, Locality]
        );
        assert_eq!(
            labels("100 king st unit 5, toronto"),
            vec![HouseNumber, Street, Street, Unit, Unit, Locality]
        );
        assert_eq!(
            labels("10 main st, suite b, ottawa"),
            vec![HouseNumber, Street, Street, Unit, Unit, Locality]
        );
        assert_eq!(
            labels("6628 finch ave w 1"),
            vec![HouseNumber, Street, Street, Street, Unit]
        );
    }

    #[test]
    fn a_fraction_is_one_house_number_over_its_tokens() {
        assert_eq!(
            labels("993.5 bloor st w, toronto"),
            vec![HouseNumber, HouseNumber, Street, Street, Street, Locality]
        );
        assert_eq!(
            labels("1384 1/2 queen st"),
            vec![HouseNumber, HouseNumber, HouseNumber, Street, Street]
        );
    }

    #[test]
    fn without_commas_everything_but_number_and_unit_is_street() {
        // A word with a digit after the street type reads as a unit, even a
        // postcode.
        assert_eq!(
            labels("30 w 26th st new york ny 10010"),
            vec![
                HouseNumber,
                Street,
                Street,
                Street,
                Street,
                Street,
                Street,
                Unit
            ]
        );
        assert_eq!(labels("tim hortons"), vec![Street, Street]);
        assert_eq!(labels("highway 7, markham"), vec![Street, Street, Locality]);
    }

    #[test]
    fn a_trailing_direction_on_a_street_without_a_type_still_retraces() {
        assert_eq!(
            labels("15 the donway e"),
            vec![HouseNumber, Street, Street, Street]
        );
    }

    #[test]
    fn a_query_with_nothing_to_search_is_all_other() {
        assert_eq!(labels("unit 5"), vec![Other, Other]);
        assert!(labels("").is_empty());
        assert!(labels(" , ").is_empty());
    }
}
