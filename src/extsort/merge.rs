use std::sync::Arc;

use super::{
    CHECK_EVERY_ITEMS, Spill,
    block::{Block, Cursor, Failure, Prefetch, Run, Stream},
    scratch::{Owner, Scratch},
};
use crate::{
    error::Result,
    memory::{Budget, Reservation},
};

const PREFETCH_THREADS: usize = 4;
const MIN_READ_BYTES: usize = 64 << 10;

#[derive(Debug)]
struct LoserTree {
    tree: Vec<usize>,
}

impl LoserTree {
    fn new(sources: usize, beats: impl Fn(usize, usize) -> bool) -> Self {
        let mut tree = vec![0; sources.max(1)];
        if sources > 1 {
            let mut winners = vec![0; 2 * sources];
            for source in 0..sources {
                winners[sources + source] = source;
            }
            for node in (1..sources).rev() {
                let (left, right) = (winners[2 * node], winners[2 * node + 1]);
                let (winner, loser) = if beats(right, left) {
                    (right, left)
                } else {
                    (left, right)
                };
                winners[node] = winner;
                tree[node] = loser;
            }
            tree[0] = winners[1];
        }
        Self { tree }
    }

    fn winner(&self) -> usize {
        self.tree[0]
    }

    fn replay(&mut self, source: usize, beats: impl Fn(usize, usize) -> bool) {
        let sources = self.tree.len();
        let mut winner = source;
        let mut node = (sources + source) / 2;
        while node >= 1 {
            if beats(self.tree[node], winner) {
                std::mem::swap(&mut self.tree[node], &mut winner);
            }
            node /= 2;
        }
        self.tree[0] = winner;
    }
}

#[derive(Debug)]
pub struct Merge<T> {
    prefetch: Prefetch,
    blocks: Vec<Option<Block>>,
    heads: Vec<Option<T>>,
    tree: LoserTree,
    failed: bool,
    _buffers: Reservation,
}

impl<T: Spill + Ord> Merge<T> {
    pub(super) fn open(
        scratch: &Arc<Scratch>,
        runs: Vec<Run>,
        budget: &Arc<Budget>,
    ) -> Result<Self> {
        let buffers = Reservation::forced(budget, runs.iter().map(Run::read_bytes).sum());
        let sources = runs.len();
        let cursors = runs.into_iter().map(Cursor::consume).collect();
        let prefetch = Prefetch::start(scratch, cursors, PREFETCH_THREADS);
        let mut merge = Self {
            prefetch,
            blocks: (0..sources).map(|_| None).collect(),
            heads: (0..sources).map(|_| None).collect(),
            tree: LoserTree { tree: Vec::new() },
            failed: false,
            _buffers: buffers,
        };
        for source in 0..sources {
            merge.heads[source] = merge.pull(source)?;
        }
        merge.tree = LoserTree::new(sources, |left, right| beats(&merge.heads, left, right));
        Ok(merge)
    }

    #[cfg(test)]
    pub(super) fn buffer_bytes(&self) -> usize {
        self._buffers.bytes()
    }

    fn pull(&mut self, source: usize) -> Result<Option<T>> {
        loop {
            if let Some(block) = &mut self.blocks[source] {
                if let Some(item) = block.next() {
                    return item.map(Some);
                }
                self.blocks[source] = None;
            }
            match self.prefetch.next(source)? {
                Some(block) => self.blocks[source] = Some(block),
                None => return Ok(None),
            }
        }
    }
}

fn beats<T: Ord>(heads: &[Option<T>], left: usize, right: usize) -> bool {
    match (&heads[left], &heads[right]) {
        (Some(a), Some(b)) => (a, left) < (b, right),
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => left < right,
    }
}

impl<T: Spill + Ord> Iterator for Merge<T> {
    type Item = Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.heads.is_empty() {
            return None;
        }
        let winner = self.tree.winner();
        let item = self.heads[winner].take()?;
        match self.pull(winner) {
            Ok(next) => self.heads[winner] = next,
            Err(error) => {
                self.failed = true;
                return Some(Err(error));
            }
        }
        let heads = &self.heads;
        self.tree
            .replay(winner, |left, right| beats(heads, left, right));
        Some(Ok(item))
    }
}

pub(super) fn fan_in(runs: &[Run], share_bytes: usize) -> usize {
    let widest = runs
        .iter()
        .map(Run::read_bytes)
        .max()
        .unwrap_or(0)
        .max(MIN_READ_BYTES);
    (share_bytes / widest).max(2)
}

pub(super) fn premerge_width(runs: usize, fan_in: usize) -> usize {
    match (runs - 1) % (fan_in - 1) + 1 {
        1 => fan_in,
        width => width,
    }
}

pub(super) fn premerge<T: Spill + Ord>(
    scratch: &Arc<Scratch>,
    owner: &Arc<Owner>,
    budget: &Arc<Budget>,
    mut runs: Vec<Run>,
    fan_in: usize,
) -> Result<Vec<Run>> {
    while runs.len() > fan_in {
        let width = premerge_width(runs.len(), fan_in);
        runs.sort_by_key(|run| std::cmp::Reverse(run.bytes));
        let group = runs.split_off(runs.len() - width);
        runs.push(
            merge_into::<T>(scratch, owner, budget, group).map_err(crate::error::Error::from)?,
        );
    }
    Ok(runs)
}

pub(super) fn merge_into<T: Spill + Ord>(
    scratch: &Arc<Scratch>,
    owner: &Arc<Owner>,
    budget: &Arc<Budget>,
    runs: Vec<Run>,
) -> std::result::Result<Run, Failure> {
    let depth = runs.iter().map(|run| run.depth).max().unwrap_or(0) + 1;
    let fan_in = runs.len();
    let mut stream = Stream::<T>::new(scratch, owner);
    stream.set_depth(depth);
    for (written, item) in Merge::<T>::open(scratch, runs, budget)?.enumerate() {
        if written % CHECK_EVERY_ITEMS == 0 {
            scratch.check_cancelled()?;
        }
        stream.push(&item?)?;
    }
    owner.merged(fan_in, depth);
    stream.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_loser_tree_yields_every_source_in_order() {
        for sources in 1..12 {
            let mut lists = (0..sources)
                .map(|source| {
                    (0..5)
                        .map(|step| (step * 7 + source * 3) % 20)
                        .collect::<std::collections::VecDeque<_>>()
                })
                .map(|mut list| {
                    list.make_contiguous().sort();
                    list
                })
                .collect::<Vec<_>>();
            let mut heads = lists
                .iter_mut()
                .map(|list| list.pop_front())
                .collect::<Vec<_>>();
            let mut tree = LoserTree::new(sources, |left, right| beats(&heads, left, right));
            let mut out = Vec::new();
            while let Some(item) = heads[tree.winner()].take() {
                let winner = tree.winner();
                out.push(item);
                heads[winner] = lists[winner].pop_front();
                let view = &heads;
                tree.replay(winner, |left, right| beats(view, left, right));
            }
            let mut expected = (0..sources)
                .flat_map(|source| (0..5).map(move |step| (step * 7 + source * 3) % 20))
                .collect::<Vec<_>>();
            expected.sort();
            assert_eq!(out, expected, "{sources} sources");
        }
    }

    #[test]
    fn premerges_leave_full_merges_after_the_first() {
        assert_eq!(premerge_width(6, 3), 2);
        assert_eq!(premerge_width(5, 3), 3);
        assert_eq!(premerge_width(9, 4), 3);
        assert_eq!(premerge_width(7, 4), 4);
        let mut runs = 1_000;
        let mut merges = 0;
        while runs > 100 {
            runs -= premerge_width(runs, 100) - 1;
            merges += 1;
        }
        assert_eq!(runs, 100);
        assert_eq!(merges, 10);
    }
}
