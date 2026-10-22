//! Turning source rows and baseline output into fixture rows.
//!
//! Sources tokenize addresses their own way, so both directions go through
//! [`search_key`], the key og matches text by. A source token is normalized on its own and
//! every word it yields keeps its label; since the key never joins across
//! whitespace, that is the same as normalizing the whole address and carrying
//! labels by character span. A
//! baseline's output is aligned back to the gold tokens character by
//! character, so a parser that splits, joins or drops punctuation differently
//! is still scored on og's tokens.

use anyhow::{Context, Result, bail, ensure};

use super::{
    fixture::TaggedRow,
    mapping::{Mapped, Scheme},
};
use crate::{
    parse::types::{Label, Tagged},
    util::fold::search_key,
};

/// A raw `lang<TAB>country<TAB>token/label ...` row in a source's own labels,
/// as a fixture row. `None` when no token survives normalization.
pub fn normalize_raw_line(line: &str, scheme: Scheme) -> Result<Option<TaggedRow>> {
    let mut columns = line.split('\t');
    let (Some(lang), Some(country), Some(fields), None) = (
        columns.next(),
        columns.next(),
        columns.next(),
        columns.next(),
    ) else {
        bail!("expected three tab-separated columns");
    };
    let mut row = TaggedRow {
        lang: lang.to_string(),
        country: country.to_lowercase(),
        tokens: Vec::new(),
        breaks: Vec::new(),
    };
    let mut part_ended = false;
    for field in fields.split(' ').filter(|field| !field.is_empty()) {
        let Some((token, label)) = field.rsplit_once('/') else {
            bail!("field {field:?} is not token/label");
        };
        let mapped = scheme
            .map(label)
            .with_context(|| format!("unknown {scheme} label {label:?}"))?;
        let label = match mapped {
            Mapped::Label(label) => label,
            Mapped::Separator => {
                part_ended = !row.tokens.is_empty();
                continue;
            }
        };
        for word in search_key(token).iter().flat_map(|text| text.split(' ')) {
            if part_ended {
                row.breaks.push(row.tokens.len());
                part_ended = false;
            }
            row.tokens.push(Tagged {
                token: word.to_string(),
                label,
            });
        }
    }
    Ok((!row.tokens.is_empty()).then_some(row))
}

/// The label a baseline gave each gold token, from its `(label, value)`
/// components in its own scheme. Each gold token takes the label most of its
/// characters were given after a longest-common-subsequence alignment of the
/// two character streams, ties going to the earlier character; a token none
/// of whose characters the baseline returned is [`Label::Other`].
pub fn align_components(
    gold: &TaggedRow,
    components: &[(String, String)],
    scheme: Scheme,
) -> Result<Vec<Label>> {
    let mut predicted = Vec::new();
    for (label, value) in components {
        let label = match scheme
            .map(label)
            .with_context(|| format!("unknown {scheme} label {label:?}"))?
        {
            Mapped::Label(label) => label,
            Mapped::Separator => continue,
        };
        if let Some(text) = search_key(value) {
            predicted.extend(
                text.chars()
                    .filter(|character| *character != ' ')
                    .map(|character| (character, label)),
            );
        }
    }
    let gold_chars = gold
        .tokens
        .iter()
        .enumerate()
        .flat_map(|(index, tagged)| {
            tagged
                .token
                .chars()
                .map(move |character| (character, index))
        })
        .collect::<Vec<_>>();

    let pairs = if gold_chars.len() == predicted.len()
        && gold_chars
            .iter()
            .zip(&predicted)
            .all(|((left, _), (right, _))| left == right)
    {
        (0..gold_chars.len()).map(|index| (index, index)).collect()
    } else {
        lcs_pairs(
            &gold_chars
                .iter()
                .map(|(character, _)| *character)
                .collect::<Vec<_>>(),
            &predicted
                .iter()
                .map(|(character, _)| *character)
                .collect::<Vec<_>>(),
        )
    };

    let mut votes = vec![[0usize; Label::ALL.len()]; gold.tokens.len()];
    let mut first = vec![None::<Label>; gold.tokens.len()];
    for (gold_index, predicted_index) in pairs {
        let token = gold_chars[gold_index].1;
        let label = predicted[predicted_index].1;
        votes[token][label as usize] += 1;
        first[token].get_or_insert(label);
    }
    Ok(votes
        .iter()
        .zip(first)
        .map(|(counts, first)| {
            let Some(first) = first else {
                return Label::Other;
            };
            let best = counts.iter().copied().max().unwrap_or_default();
            if counts[first as usize] == best {
                first
            } else {
                Label::ALL
                    .into_iter()
                    .find(|label| counts[*label as usize] == best)
                    .unwrap_or(first)
            }
        })
        .collect())
}

/// Index pairs of one longest common subsequence of `left` and `right`.
fn lcs_pairs(left: &[char], right: &[char]) -> Vec<(usize, usize)> {
    let width = right.len() + 1;
    // lengths[i * width + j]: LCS length of left[i..] and right[j..].
    let mut lengths = vec![0u32; (left.len() + 1) * width];
    for i in (0..left.len()).rev() {
        for j in (0..right.len()).rev() {
            lengths[i * width + j] = if left[i] == right[j] {
                lengths[(i + 1) * width + j + 1] + 1
            } else {
                lengths[(i + 1) * width + j].max(lengths[i * width + j + 1])
            };
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < left.len() && j < right.len() {
        if left[i] == right[j] {
            pairs.push((i, j));
            i += 1;
            j += 1;
        } else if lengths[(i + 1) * width + j] >= lengths[i * width + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs
}

/// A baseline's labels for a gold row, as a prediction row: the gold tokens
/// and part boundaries with the baseline's labels.
pub fn prediction_row(gold: &TaggedRow, labels: &[Label]) -> Result<TaggedRow> {
    ensure!(
        labels.len() == gold.tokens.len(),
        "{} labels for {} tokens",
        labels.len(),
        gold.tokens.len()
    );
    Ok(gold.relabelled(labels))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn components(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(label, value)| (label.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn libpostal_rows_normalize_and_keep_part_boundaries() {
        let row = normalize_raw_line(
            "nl\tnl\tvan/road Bockel/road de/road Kokstr./road 8/house_number |/FSEP 5331AC/postcode Kerkdriel/city |/FSEP Nederland/country",
            Scheme::Libpostal,
        )
        .expect("row")
        .expect("tokens");
        assert_eq!(
            row.to_line(),
            "nl\tnl\tvan/street bockel/street de/street kokstr/street 8/house_number , 5331ac/postcode kerkdriel/locality , nederland/country"
        );
    }

    #[test]
    fn a_token_normalization_splits_keeps_its_label_on_every_word() {
        let row = normalize_raw_line(
            "pl\tpl\tSmętna/road 8/house_number ,/SEP PL-31-343/postcode Kraków/city",
            Scheme::Libpostal,
        )
        .expect("row")
        .expect("tokens");
        assert_eq!(
            row.to_line(),
            "pl\tpl\tsmetna/street 8/house_number , pl/postcode 31/postcode 343/postcode krakow/locality"
        );
    }

    #[test]
    fn leading_trailing_and_repeated_separators_collapse() {
        let row = normalize_raw_line(
            "en\tus\t,/SEP main/road -/road ,/SEP |/FSEP boston/city ,/SEP",
            Scheme::Libpostal,
        )
        .expect("row")
        .expect("tokens");
        assert_eq!(row.to_line(), "en\tus\tmain/street , boston/locality");
    }

    #[test]
    fn a_dotted_capital_i_stays_in_its_word() {
        // Lowercased, "İ" is "i" and a combining dot: an accent, never a
        // word break.
        let row = normalize_raw_line("tr\tch\tİsviçre/country", Scheme::Libpostal)
            .expect("row")
            .expect("tokens");
        assert_eq!(row.to_line(), "tr\tch\tisvicre/country");
        assert!(TaggedRow::parse_line(&row.to_line()).is_ok());
    }

    #[test]
    fn unknown_labels_and_empty_rows() {
        assert!(normalize_raw_line("en\tus\tmain/street", Scheme::Libpostal).is_err());
        assert!(normalize_raw_line("en\tus\tmain", Scheme::Libpostal).is_err());
        assert_eq!(
            normalize_raw_line("en\tus\t-/road ,/SEP", Scheme::Libpostal).expect("row"),
            None
        );
    }

    #[test]
    fn identical_text_aligns_token_for_token() {
        let gold = TaggedRow::parse_line(
            "en\tus\t30/house_number w/street 26th/street st/street , new/locality york/locality",
        )
        .expect("gold");
        let labels = align_components(
            &gold,
            &components(&[
                ("house_number", "30"),
                ("road", "w 26th st"),
                ("city", "new york"),
            ]),
            Scheme::Libpostal,
        )
        .expect("labels");
        assert_eq!(labels, gold.labels());
    }

    #[test]
    fn differently_tokenized_output_aligns_by_character() {
        // The baseline glued "26 th" and dropped "york".
        let gold = TaggedRow::parse_line(
            "en\tus\t26/street th/street st/street new/locality york/locality",
        )
        .expect("gold");
        let labels = align_components(
            &gold,
            &components(&[("road", "26th st."), ("state", "new")]),
            Scheme::Libpostal,
        )
        .expect("labels");
        assert_eq!(
            labels,
            vec![
                Label::Street,
                Label::Street,
                Label::Street,
                Label::Region,
                Label::Other
            ]
        );
    }

    #[test]
    fn a_token_takes_the_label_most_of_its_characters_got() {
        let gold = TaggedRow::parse_line("en\tgb\tsw1a2aa/postcode").expect("gold");
        let labels = align_components(
            &gold,
            &components(&[("road", "sw"), ("postcode", "1a2aa")]),
            Scheme::Libpostal,
        )
        .expect("labels");
        assert_eq!(labels, vec![Label::Postcode]);
    }

    #[test]
    fn lcs_pairs_finds_a_longest_common_subsequence() {
        let left = "abcbdab".chars().collect::<Vec<_>>();
        let right = "bdcaba".chars().collect::<Vec<_>>();
        let pairs = lcs_pairs(&left, &right);
        assert_eq!(pairs.len(), 4);
        assert!(pairs.iter().all(|&(i, j)| left[i] == right[j]));
        assert!(
            pairs
                .windows(2)
                .all(|pair| pair[0].0 < pair[1].0 && pair[0].1 < pair[1].1)
        );
    }
}
