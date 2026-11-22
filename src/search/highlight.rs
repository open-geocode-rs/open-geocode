//! Where the words typed into `/autocomplete` lie in a suggestion's label.
//!
//! The text index matches typed words against a record's subject and
//! context text, and knows no position in the label, which is composed
//! from the record's parts. So each typed word is found again in the label,
//! cut and folded as the index cuts and folds its text
//! ([`search_spans`]), by the rule the search matched it with. That is in
//! order where the search asked for the words in order, else word by word
//! as [`super::types_word`] says a label has a typed word, and a typo away
//! only where the search allowed typos. A label word no typed word matches so is
//! never marked.

use std::cell::OnceCell;

use levenshtein_automata::{DFA, Distance};

use super::{Completion, allowed_edits, first_chars, stands_for, typo_distance};
use crate::{
    api::Highlight,
    parse::lattice::{automaton_builder, forms_both_ways},
    util::fold::{Span, search_spans},
};

/// What one autocomplete search matched its hits by.
pub(super) struct Highlighter<'a> {
    /// The house number, matched whole against a record's own.
    number: Option<&'a str>,
    words: &'a [String],
    /// The other forms of each word, read once for every label.
    forms: Vec<Vec<String>>,
    /// Whether the last word may be cut short by the typing.
    typing: bool,
    /// Whether the search asked for the words side by side, in order.
    in_order: bool,
    typos: bool,
    /// The automaton of the last word cut short and a typo away, built
    /// when a label first needs it.
    cut_typo: OnceCell<Option<DFA>>,
}

impl<'a> Highlighter<'a> {
    pub(super) fn new(
        completion: Completion,
        number: Option<&'a str>,
        words: &'a [String],
        typing: bool,
    ) -> Self {
        Self {
            number,
            words,
            forms: words.iter().map(|word| forms_both_ways(word)).collect(),
            typing,
            in_order: matches!(
                completion,
                Completion::InOrder | Completion::CommonCompletions
            ),
            typos: completion == Completion::Typos,
            cut_typo: OnceCell::new(),
        }
    }

    /// The parts of `label` the typed words matched, in order, none
    /// overlapping. Empty when no typed word is found in it.
    pub(super) fn ranges(&self, label: &str) -> Vec<Highlight> {
        let spans = search_spans(label);
        let mut marks = Vec::new();
        if let Some(number) = self.number
            && let Some(span) = spans.iter().find(|span| span.key == number)
        {
            marks.push((span.start, span.end()));
        }
        if !(self.in_order && self.phrase(&spans, &mut marks)) {
            for at in 0..self.words.len() {
                self.word(at, &spans, &mut marks);
            }
        }
        marks.sort_unstable();
        let mut ranges = Vec::<Highlight>::with_capacity(marks.len());
        for (start, end) in marks {
            match ranges.last_mut() {
                Some(last) if start <= last.end => last.end = last.end.max(end),
                _ => ranges.push(Highlight { start, end }),
            }
        }
        ranges
    }

    fn cut(&self, at: usize) -> bool {
        self.typing && at + 1 == self.words.len()
    }

    /// The first run of label words that are the typed words in order, the
    /// last begun where it may be cut short. `false` when the label has no
    /// such run, as one laid out otherwise than the subject text.
    fn phrase(&self, spans: &[Span], marks: &mut Vec<(usize, usize)>) -> bool {
        let words = self.words;
        if words.is_empty() {
            return false;
        }
        let is = |at: usize, span: &Span| match self.cut(at) {
            true => span.key.starts_with(words[at].as_str()),
            false => span.key == words[at],
        };
        let Some(run) = spans
            .windows(words.len())
            .find(|run| run.iter().enumerate().all(|(at, span)| is(at, span)))
        else {
            return false;
        };
        for (at, span) in run.iter().enumerate() {
            marks.push((span.start, self.typed_end(at, span)));
        }
        true
    }

    /// Where the typed word `at` ends in a label word it starts or is.
    fn typed_end(&self, at: usize, span: &Span) -> usize {
        match self.cut(at) {
            true => span.end_of(self.words[at].chars().count()),
            false => span.end(),
        }
    }

    /// Marks each label word the typed word `at` is, as written or in
    /// another form, begun when it may be cut short, and when none is and
    /// typos are allowed, the nearest a typo away.
    fn word(&self, at: usize, spans: &[Span], marks: &mut Vec<(usize, usize)>) {
        let word = self.words[at].as_str();
        let before = marks.len();
        for span in spans {
            if span.key == word || stands_for(word, &self.forms[at], &span.key) {
                marks.push((span.start, span.end()));
            } else if self.cut(at) && span.key.starts_with(word) {
                marks.push((span.start, self.typed_end(at, span)));
            }
        }
        if marks.len() > before || !self.typos {
            return;
        }
        let near = spans
            .iter()
            .filter_map(|span| Some((typo_distance(word, &span.key)?, span)))
            .collect::<Vec<_>>();
        if let Some(fewest) = near.iter().map(|(edits, _)| *edits).min() {
            marks.extend(
                near.iter()
                    .filter(|(edits, _)| *edits == fewest)
                    .map(|(_, span)| (span.start, span.end())),
            );
            return;
        }
        if !self.cut(at) {
            return;
        }
        let Some(dfa) = self.cut_typo.get_or_init(|| {
            allowed_edits(word).map(|most| automaton_builder(most).build_prefix_dfa(word))
        }) else {
            return;
        };
        for span in spans {
            if span.key.starts_with(first_chars(word))
                && matches!(dfa.eval(&span.key), Distance::Exact(_))
            {
                marks.push((span.start, self.typed_end(at, span)));
            }
        }
    }
}
