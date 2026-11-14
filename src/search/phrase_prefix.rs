//! A phrase whose last word is still being typed, matched and scored as
//! tantivy's `PhrasePrefixQuery` does. Its scorer reads every completion at
//! each document of the head, so "rue de la" on a planet costs millions of
//! documents times 1,024 completions. This one keeps the completions in one
//! queue by document and skips the head and the queue past each other.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, binary_heap::PeekMut};

use tantivy::{
    DocId, DocSet, Score, SegmentReader, TERMINATED, TantivyError, Term,
    fieldnorm::FieldNormReader,
    postings::{Postings, SegmentPostings},
    query::{Bm25Weight, EmptyScorer, EnableScoring, Explanation, Query, Scorer, Weight},
    schema::IndexRecordOption,
};

/// The words of `head` in a row, then a word that starts with `prefix`, one
/// of the first `max_expansions` such words of each segment's dictionary.
#[derive(Clone, Debug)]
pub(super) struct PhrasePrefix {
    head: Vec<Term>,
    prefix: Term,
    max_expansions: u32,
}

impl PhrasePrefix {
    pub(super) fn new(head: Vec<Term>, prefix: Term, max_expansions: u32) -> Self {
        assert!(!head.is_empty(), "a phrase prefix needs a head word");
        assert!(
            head.iter().all(|term| term.field() == prefix.field()),
            "a phrase prefix reads one field"
        );
        Self {
            head,
            prefix,
            max_expansions,
        }
    }
}

impl Query for PhrasePrefix {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        let field = self.prefix.field();
        let entry = enable_scoring.schema().get_field_entry(field);
        let has_positions = entry
            .field_type()
            .get_index_record_option()
            .is_some_and(IndexRecordOption::has_positions);
        if !has_positions {
            return Err(TantivyError::SchemaError(format!(
                "Applied phrase prefix query on field {:?}, which does not have positions \
                 indexed",
                entry.name()
            )));
        }
        // Tantivy scores a one word head 1 whatever the boost, and a longer
        // one as the head phrase's BM25 from the searcher's statistics.
        let bm25 = match enable_scoring {
            EnableScoring::Enabled { searcher, .. } if self.head.len() > 1 => {
                Some(Bm25Weight::for_terms(searcher, &self.head)?)
            }
            _ => None,
        };
        Ok(Box::new(PhrasePrefixWeight {
            query: self.clone(),
            bm25,
        }))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        for term in &self.head {
            visitor(term, true);
        }
    }
}

struct PhrasePrefixWeight {
    query: PhrasePrefix,
    bm25: Option<Bm25Weight>,
}

impl PhrasePrefixWeight {
    fn phrase_scorer(
        &self,
        reader: &SegmentReader,
        boost: Score,
    ) -> tantivy::Result<Option<PhrasePrefixScorer>> {
        let field = self.query.prefix.field();
        let index = reader.inverted_index(field)?;
        let head_len = self.query.head.len() as u32;
        let mut head = Vec::with_capacity(self.query.head.len());
        for (at, term) in (0..).zip(&self.query.head) {
            match index.read_postings(term, IndexRecordOption::WithFreqsAndPositions)? {
                Some(postings) => head.push((head_len - at, postings)),
                None => return Ok(None),
            }
        }
        let prefix = self.query.prefix.serialized_value_bytes();
        let mut stream = index.terms().range().ge(prefix).into_stream()?;
        let mut suffixes = Vec::new();
        while (suffixes.len() as u32) < self.query.max_expansions
            && stream.advance()
            && stream.key().starts_with(prefix)
        {
            suffixes.push(index.read_postings_from_terminfo(
                stream.value(),
                IndexRecordOption::WithFreqsAndPositions,
            )?);
        }
        let scoring = match &self.bm25 {
            Some(bm25) => Some((
                bm25.boost_by(boost),
                reader
                    .fieldnorms_readers()
                    .get_field(field)?
                    .unwrap_or_else(|| FieldNormReader::constant(reader.max_doc(), 1)),
            )),
            None => None,
        };
        Ok(Some(PhrasePrefixScorer::new(head, suffixes, scoring)))
    }
}

impl Weight for PhrasePrefixWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        Ok(match self.phrase_scorer(reader, boost)? {
            Some(scorer) => Box::new(scorer),
            None => Box::new(EmptyScorer),
        })
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let does_not_match =
            || TantivyError::InvalidArgument(format!("Document #({doc}) does not match"));
        let mut scorer = self
            .phrase_scorer(reader, 1.0)?
            .ok_or_else(does_not_match)?;
        if scorer.seek(doc) != doc {
            return Err(does_not_match());
        }
        Ok(Explanation::new("Phrase Prefix Scorer", scorer.score()))
    }
}

struct PhrasePrefixScorer {
    /// Each head word with how far its positions shift to where the
    /// completion stands, rarest first so skipping starts on the shortest.
    head: Vec<(u32, SegmentPostings)>,
    suffixes: Vec<SegmentPostings>,
    /// Completions past the current document, by document.
    queue: BinaryHeap<Reverse<(DocId, usize)>>,
    /// Completions on the current document, out of the queue until it moves.
    here: Vec<usize>,
    doc: DocId,
    scoring: Option<(Bm25Weight, FieldNormReader)>,
    phrase_count: u32,
    head_positions: Vec<u32>,
    positions: Vec<u32>,
}

impl PhrasePrefixScorer {
    fn new(
        mut head: Vec<(u32, SegmentPostings)>,
        suffixes: Vec<SegmentPostings>,
        scoring: Option<(Bm25Weight, FieldNormReader)>,
    ) -> Self {
        head.sort_by_key(|(_, postings)| postings.size_hint());
        let queue = suffixes
            .iter()
            .enumerate()
            .filter(|(_, postings)| postings.doc() != TERMINATED)
            .map(|(at, postings)| Reverse((postings.doc(), at)))
            .collect();
        let mut scorer = Self {
            head,
            suffixes,
            queue,
            here: Vec::new(),
            doc: 0,
            scoring,
            phrase_count: 0,
            head_positions: Vec::new(),
            positions: Vec::new(),
        };
        scorer.doc = scorer.find(0);
        scorer
    }

    fn find(&mut self, mut target: DocId) -> DocId {
        loop {
            let head = self.seek_head(target);
            if head == TERMINATED {
                return TERMINATED;
            }
            let suffix = self.seek_suffixes(head);
            if suffix == TERMINATED {
                return TERMINATED;
            }
            if suffix > head {
                target = suffix;
                continue;
            }
            while let Some(top) = self.queue.peek_mut() {
                if top.0.0 != head {
                    break;
                }
                self.here.push(PeekMut::pop(top).0.1);
            }
            if self.head_phrase() && self.completes() {
                return head;
            }
            target = head + 1;
        }
    }

    /// The first document at or after `target` holding every head word.
    fn seek_head(&mut self, mut target: DocId) -> DocId {
        'agree: loop {
            for (_, postings) in &mut self.head {
                let doc = seek(postings, target);
                if doc == TERMINATED {
                    return TERMINATED;
                }
                if doc > target {
                    target = doc;
                    continue 'agree;
                }
            }
            return target;
        }
    }

    /// The first document at or after `target` holding a completion.
    fn seek_suffixes(&mut self, target: DocId) -> DocId {
        for at in self.here.drain(..) {
            let doc = seek(&mut self.suffixes[at], target);
            if doc != TERMINATED {
                self.queue.push(Reverse((doc, at)));
            }
        }
        // A rare head leaves most completions behind each of its documents.
        // Past n / log n sifts, one pass and a rebuild cost less.
        let len = self.queue.len();
        let mut sifts = len / (len.max(1).ilog2() as usize + 1);
        loop {
            let Some(mut top) = self.queue.peek_mut() else {
                return TERMINATED;
            };
            let Reverse((doc, at)) = *top;
            if doc >= target {
                return doc;
            }
            if sifts == 0 {
                break;
            }
            sifts -= 1;
            let doc = self.suffixes[at].seek(target);
            if doc == TERMINATED {
                PeekMut::pop(top);
            } else {
                *top = Reverse((doc, at));
            }
        }
        let mut entries = std::mem::take(&mut self.queue).into_vec();
        entries.retain_mut(|Reverse((doc, at))| {
            *doc = seek(&mut self.suffixes[*at], target);
            *doc != TERMINATED
        });
        self.queue = entries.into();
        self.queue.peek().map_or(TERMINATED, |top| top.0.0)
    }

    /// Whether the head words stand in a row on this document. Counts them
    /// as tantivy's phrase scorer does, for BM25.
    fn head_phrase(&mut self) -> bool {
        let (first, rest) = self.head.split_first_mut().expect("head is never empty");
        first
            .1
            .positions_with_offset(first.0, &mut self.head_positions);
        for (shift, postings) in rest {
            postings.positions_with_offset(*shift, &mut self.positions);
            retain_common(&mut self.head_positions, &self.positions);
        }
        self.phrase_count = self.head_positions.len() as u32;
        !self.head_positions.is_empty()
    }

    /// Whether a completion on this document follows the head phrase.
    fn completes(&mut self) -> bool {
        self.here.iter().any(|&at| {
            self.suffixes[at].positions(&mut self.positions);
            any_common(&self.head_positions, &self.positions)
        })
    }
}

fn seek(postings: &mut SegmentPostings, target: DocId) -> DocId {
    // Seeking backwards trips a debug assertion in tantivy.
    if postings.doc() < target {
        postings.seek(target)
    } else {
        postings.doc()
    }
}

/// Keeps in `left` what `right` also holds. Both are sorted.
fn retain_common(left: &mut Vec<u32>, right: &[u32]) {
    let mut at = 0;
    left.retain(|position| {
        while at < right.len() && right[at] < *position {
            at += 1;
        }
        at < right.len() && right[at] == *position
    });
}

fn any_common(left: &[u32], right: &[u32]) -> bool {
    let (mut l, mut r) = (0, 0);
    while l < left.len() && r < right.len() {
        match left[l].cmp(&right[r]) {
            std::cmp::Ordering::Less => l += 1,
            std::cmp::Ordering::Greater => r += 1,
            std::cmp::Ordering::Equal => return true,
        }
    }
    false
}

impl DocSet for PhrasePrefixScorer {
    fn advance(&mut self) -> DocId {
        if self.doc != TERMINATED {
            self.doc = self.find(self.doc + 1);
        }
        self.doc
    }

    fn seek(&mut self, target: DocId) -> DocId {
        if self.doc < target {
            self.doc = self.find(target);
        }
        self.doc
    }

    fn doc(&self) -> DocId {
        self.doc
    }

    fn size_hint(&self) -> u32 {
        let suffixes = self
            .suffixes
            .iter()
            .map(|postings| postings.size_hint())
            .fold(0u32, u32::saturating_add);
        self.head
            .iter()
            .map(|(_, postings)| postings.size_hint())
            .fold(suffixes, u32::min)
    }
}

impl Scorer for PhrasePrefixScorer {
    fn score(&mut self) -> Score {
        match &self.scoring {
            Some((bm25, fieldnorms)) => {
                bm25.score(fieldnorms.fieldnorm_id(self.doc), self.phrase_count)
            }
            None => 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::{
        DocAddress, Index, IndexWriter, Searcher, TantivyDocument,
        collector::{DocSetCollector, TopDocs},
        indexer::NoMergePolicy,
        query::{BooleanQuery, BoostQuery, Occur, PhrasePrefixQuery, TermQuery},
        schema::{Field, Schema, TEXT},
    };

    const CAP: u32 = 1_024;

    /// Two segments. The first holds 1,200 completions of "la" after a
    /// common head, so the cap cuts it. The second holds a cut one again.
    fn index() -> (Searcher, Field) {
        let mut schema = Schema::builder();
        let text = schema.add_text_field("text", TEXT);
        let index = Index::create_in_ram(schema.build());
        let mut writer: IndexWriter = index.writer_with_num_threads(1, 15_000_000).unwrap();
        writer.set_merge_policy(Box::new(NoMergePolicy));
        let first = (0..1_200)
            .map(|at| format!("rue de la{at:04}"))
            .chain(
                [
                    "rue de la",
                    "rue de rue de la0007",
                    "avenue rue de la0005 nord ouest",
                    "de rue la0001",
                    "rue de x la0002",
                    "rue la0003 de",
                    "rue de lac",
                ]
                .map(String::from),
            )
            .collect::<Vec<_>>();
        let second = ["rue de la1100", "rue de lac", "la rue de", "de la"].map(String::from);
        for segment in [&first[..], &second[..]] {
            for value in segment {
                let mut doc = TantivyDocument::default();
                doc.add_text(text, value);
                writer.add_document(doc).unwrap();
            }
            writer.commit().unwrap();
        }
        let searcher = index.reader().unwrap().searcher();
        assert_eq!(searcher.segment_readers().len(), 2);
        (searcher, text)
    }

    fn queries(field: Field, words: &[&str]) -> (Box<dyn Query>, Box<dyn Query>) {
        let terms = words
            .iter()
            .map(|word| Term::from_field_text(field, word))
            .collect::<Vec<_>>();
        let mut theirs = PhrasePrefixQuery::new(terms.clone());
        theirs.set_max_expansions(CAP);
        let (prefix, head) = terms.split_last().unwrap();
        (
            Box::new(theirs),
            Box::new(PhrasePrefix::new(head.to_vec(), prefix.clone(), CAP)),
        )
    }

    fn scored(searcher: &Searcher, query: &dyn Query) -> Vec<(DocAddress, u32)> {
        let mut hits = searcher
            .search(query, &TopDocs::with_limit(10_000))
            .unwrap()
            .into_iter()
            .map(|(score, address)| (address, score.to_bits()))
            .collect::<Vec<_>>();
        hits.sort();
        hits
    }

    #[test]
    fn matches_and_scores_as_tantivy_phrase_prefix() {
        let (searcher, text) = index();
        // First segment's 1,024 are "la" and la0000 to la1022, so 1,023
        // numbered rows match, with "rue de la", the repeated head and the
        // long row. Second segment has only la1100 and lac, so both count.
        let cases: [(&[&str], usize); 6] = [
            (&["rue", "de", "la"], 1_028),
            (&["de", "la"], 1_029),
            (&["rue", "de", "rue", "de", "l"], 1),
            (&["rue", "de", "la00"], 102),
            (&["rue", "de", "zz"], 0),
            (&["nord", "ouest", "x"], 0),
        ];
        let rue = || -> Box<dyn Query> {
            Box::new(TermQuery::new(
                Term::from_field_text(text, "rue"),
                IndexRecordOption::WithFreqs,
            ))
        };
        for (words, count) in cases {
            let (theirs, ours) = queries(text, words);
            let expected = scored(&searcher, theirs.as_ref());
            assert_eq!(expected.len(), count, "{words:?}");
            assert_eq!(scored(&searcher, ours.as_ref()), expected, "{words:?}");
            assert_eq!(
                searcher.search(ours.as_ref(), &DocSetCollector).unwrap(),
                searcher.search(theirs.as_ref(), &DocSetCollector).unwrap(),
                "{words:?}"
            );
            let boosted = |query: &dyn Query| BoostQuery::new(query.box_clone(), 2.5);
            assert_eq!(
                scored(&searcher, &boosted(ours.as_ref())),
                scored(&searcher, &boosted(theirs.as_ref())),
                "{words:?}"
            );
            // Driven by seeks from an intersection, not by advances.
            let within = |query: &dyn Query| {
                BooleanQuery::new(vec![(Occur::Must, rue()), (Occur::Must, query.box_clone())])
            };
            assert_eq!(
                scored(&searcher, &within(ours.as_ref())),
                scored(&searcher, &within(theirs.as_ref())),
                "{words:?}"
            );
        }
    }

    #[test]
    fn longer_head_scores_how_often_the_head_occurs() {
        let (searcher, text) = index();
        let (_, ours) = queries(text, &["rue", "de", "la0007"]);
        let hits = scored(&searcher, ours.as_ref());
        assert_eq!(hits.len(), 2);
        let (once, twice) = (f32::from_bits(hits[0].1), f32::from_bits(hits[1].1));
        assert!(twice > once, "{twice} after {once}");
    }
}
