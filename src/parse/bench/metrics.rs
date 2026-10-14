//! Parse accuracy over gold rows: whole-row and per-token accuracy, and
//! per-label precision, recall and F1 over tokens.

use serde::Serialize;

use crate::parse::types::Label;

/// Counts for one system over a set of rows. Every gold token is scored;
/// a token a system did not return counts as wrong and as a miss of its gold
/// label, and predicts no label.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub rows: usize,
    /// Rows with every token labelled correctly.
    pub rows_correct: usize,
    /// Rows the system tokenized differently from the gold or failed on,
    /// scored as all wrong.
    pub rows_misaligned: usize,
    pub tokens: usize,
    pub tokens_correct: usize,
    /// Per label, indexed by `Label as usize`.
    pub true_positives: [usize; Label::ALL.len()],
    pub false_positives: [usize; Label::ALL.len()],
    pub false_negatives: [usize; Label::ALL.len()],
}

impl Counts {
    /// Score one row. `predicted` is `None` when the system failed on the row
    /// or its tokens did not match the gold tokens.
    pub fn add_row(&mut self, gold: &[Label], predicted: Option<&[Label]>) {
        self.rows += 1;
        self.tokens += gold.len();
        let predicted = predicted.filter(|predicted| predicted.len() == gold.len());
        let Some(predicted) = predicted else {
            self.rows_misaligned += 1;
            for label in gold {
                self.false_negatives[*label as usize] += 1;
            }
            return;
        };
        let mut all_correct = true;
        for (&gold, &predicted) in gold.iter().zip(predicted) {
            if gold == predicted {
                self.tokens_correct += 1;
                self.true_positives[gold as usize] += 1;
            } else {
                all_correct = false;
                self.false_negatives[gold as usize] += 1;
                self.false_positives[predicted as usize] += 1;
            }
        }
        if all_correct {
            self.rows_correct += 1;
        }
    }

    pub fn merge(&mut self, other: &Counts) {
        self.rows += other.rows;
        self.rows_correct += other.rows_correct;
        self.rows_misaligned += other.rows_misaligned;
        self.tokens += other.tokens;
        self.tokens_correct += other.tokens_correct;
        for index in 0..Label::ALL.len() {
            self.true_positives[index] += other.true_positives[index];
            self.false_positives[index] += other.false_positives[index];
            self.false_negatives[index] += other.false_negatives[index];
        }
    }

    pub fn full_parse_accuracy(&self) -> f64 {
        ratio(self.rows_correct, self.rows)
    }

    pub fn token_accuracy(&self) -> f64 {
        ratio(self.tokens_correct, self.tokens)
    }

    /// Gold tokens with this label.
    pub fn support(&self, label: Label) -> usize {
        self.true_positives[label as usize] + self.false_negatives[label as usize]
    }

    pub fn label(&self, label: Label) -> LabelScore {
        let index = label as usize;
        let tp = self.true_positives[index];
        let precision = ratio(tp, tp + self.false_positives[index]);
        let recall = ratio(tp, tp + self.false_negatives[index]);
        let f1 = if precision + recall > 0.0 {
            2.0 * precision * recall / (precision + recall)
        } else {
            0.0
        };
        LabelScore {
            label: label.name(),
            support: self.support(label),
            predicted: tp + self.false_positives[index],
            precision,
            recall,
            f1,
        }
    }

    pub fn summary(&self) -> Summary {
        Summary {
            rows: self.rows,
            rows_correct: self.rows_correct,
            rows_misaligned: self.rows_misaligned,
            tokens: self.tokens,
            tokens_correct: self.tokens_correct,
            full_parse_accuracy: self.full_parse_accuracy(),
            token_accuracy: self.token_accuracy(),
            labels: Label::ALL
                .into_iter()
                .map(|label| self.label(label))
                .filter(|score| score.support > 0 || score.predicted > 0)
                .collect(),
        }
    }
}

/// Precision, recall and F1 of one label. A label never predicted has
/// precision 0, and one never in the gold has recall 0.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LabelScore {
    pub label: &'static str,
    pub support: usize,
    pub predicted: usize,
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub rows: usize,
    pub rows_correct: usize,
    pub rows_misaligned: usize,
    pub tokens: usize,
    pub tokens_correct: usize,
    pub full_parse_accuracy: f64,
    pub token_accuracy: f64,
    pub labels: Vec<LabelScore>,
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Label::*;

    #[test]
    fn a_fully_correct_row_counts_once_and_every_token() {
        let mut counts = Counts::default();
        counts.add_row(
            &[HouseNumber, Street, Locality],
            Some(&[HouseNumber, Street, Locality]),
        );
        assert_eq!(counts.rows_correct, 1);
        assert_eq!(counts.tokens_correct, 3);
        assert_eq!(counts.full_parse_accuracy(), 1.0);
        assert_eq!(counts.label(Street).f1, 1.0);
    }

    #[test]
    fn one_wrong_token_fails_the_row_and_scores_both_labels() {
        let mut counts = Counts::default();
        counts.add_row(&[Street, Street, Locality], Some(&[Street, Street, Street]));
        assert_eq!(counts.rows_correct, 0);
        assert_eq!(counts.token_accuracy(), 2.0 / 3.0);
        let street = counts.label(Street);
        assert_eq!(street.precision, 2.0 / 3.0);
        assert_eq!(street.recall, 1.0);
        assert!((street.f1 - 0.8).abs() < 1e-12);
        let locality = counts.label(Locality);
        assert_eq!(
            (locality.precision, locality.recall, locality.f1),
            (0.0, 0.0, 0.0)
        );
        assert_eq!(locality.support, 1);
    }

    #[test]
    fn a_misaligned_row_is_all_misses_and_no_predictions() {
        let mut counts = Counts::default();
        counts.add_row(&[Street, Locality], None);
        counts.add_row(&[Street], Some(&[Street, Street]));
        assert_eq!(counts.rows_misaligned, 2);
        assert_eq!(counts.tokens, 3);
        assert_eq!(counts.tokens_correct, 0);
        assert_eq!(counts.false_negatives[Street as usize], 2);
        assert_eq!(counts.label(Street).predicted, 0);
    }

    #[test]
    fn merged_counts_equal_counts_over_all_rows() {
        let rows: [(&[Label], &[Label]); 3] = [
            (&[HouseNumber, Street], &[HouseNumber, Street]),
            (&[Postcode, Locality], &[Postcode, Region]),
            (&[Country], &[Other]),
        ];
        let mut whole = Counts::default();
        let mut first = Counts::default();
        let mut second = Counts::default();
        for (index, (gold, predicted)) in rows.iter().enumerate() {
            whole.add_row(gold, Some(predicted));
            if index == 0 {
                first.add_row(gold, Some(predicted));
            } else {
                second.add_row(gold, Some(predicted));
            }
        }
        first.merge(&second);
        assert_eq!(first, whole);
        assert_eq!(whole.full_parse_accuracy(), 1.0 / 3.0);
    }

    #[test]
    fn empty_counts_score_zero_not_nan() {
        let counts = Counts::default();
        assert_eq!(counts.full_parse_accuracy(), 0.0);
        assert_eq!(counts.label(Unit).f1, 0.0);
        assert!(counts.summary().labels.is_empty());
    }
}
