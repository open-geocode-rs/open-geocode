//! Build memory accounting.
//!
//! One [`Budget`] is shared by every large buffer of a build: sort
//! buffers (counted by capacity, so `Vec` growth is included), merge read
//! buffers, the text indexing buffers and the parser's lattice cache. A
//! buffer reserves bytes before it grows; when the pool is full its owner
//! spills to disk instead. Buffers that can spill hold a [`Spillable`]
//! reservation, and each of them may hold a fair share of what the buffers
//! that cannot spill leave, as DataFusion's `FairSpillPool` does: a sorter
//! open alone may take the whole pool, and four take a quarter each.
//!
//! The limit is derived from the memory the process can use
//! ([`derived_budget`]), within [`MIN_BUDGET_BYTES`] and
//! [`MAX_DERIVED_BUDGET_BYTES`], unless `--memory-budget-mb` sets it. The pool also
//! refuses to grow once the heap itself, counted by the global allocator
//! ([`crate::alloc`]), would pass a ceiling below the memory that was
//! available, so memory nobody reserved (boundaries, maps, Tantivy's own)
//! makes the tracked buffers spill sooner instead of the machine paging.
//! The build report records the peaks.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crate::{alloc, resources::Resources};

/// The share of usable memory the derived budget takes. The rest is headroom
/// for what the pool does not track and for the page cache that keeps the
/// Pack and the input resident: DuckDB takes 80% of memory, ClickHouse
/// spills at half, GNU sort leaves a quarter.
const BUDGET_PERCENT: u64 = 50;
/// The heap may grow by at most this share of the memory available when the
/// pool was last sampled before tracked buffers must spill.
const HEAP_CEILING_PERCENT: u64 = 85;
/// The smallest derived budget: below it, sorters spill so often that a
/// build drowns in tiny run files.
pub const MIN_BUDGET_BYTES: usize = 256 << 20;
/// The largest derived budget. Past it a build gets slower, not faster:
/// sorting, hashing and indexing in buffers far larger than the CPU caches
/// cost more than the spill they save. Germany with a 6.2 GiB budget took
/// 57, 63 and 278 s to scan, join nodes and write records, against 49, 46
/// and 194 s with 1 GiB, on either allocator, while spilling 9.8 GB instead
/// of 15.0 GB to an SSD that was never the bottleneck.
pub const MAX_DERIVED_BUDGET_BYTES: usize = 1 << 30;

/// The budget of a build that expects `untracked_bytes` of memory outside
/// the pool: half of what it can use, between [`MIN_BUDGET_BYTES`] and
/// [`MAX_DERIVED_BUDGET_BYTES`]. A small machine gets less, so the build
/// spills sooner instead of paging.
pub fn derived_budget(resources: &Resources, untracked_bytes: u64) -> usize {
    let usable = resources
        .available_memory_bytes
        .saturating_sub(untracked_bytes);
    let budget = u128::from(usable) * u128::from(BUDGET_PERCENT) / 100;
    usize::try_from(budget)
        .unwrap_or(usize::MAX)
        .clamp(MIN_BUDGET_BYTES, MAX_DERIVED_BUDGET_BYTES)
}

#[derive(Debug)]
pub struct Budget {
    limit: usize,
    used: AtomicUsize,
    /// Bytes reserved by buffers that can spill.
    spillable_used: AtomicUsize,
    /// Buffers that can spill and are open.
    spillable: AtomicUsize,
    peak: AtomicUsize,
    /// The most bytes the heap may hold before reservations are refused.
    heap_ceiling: AtomicUsize,
}

impl Budget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
            spillable_used: AtomicUsize::new(0),
            spillable: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            heap_ceiling: AtomicUsize::new(usize::MAX),
        })
    }

    /// A budget that never refuses, for writers used outside a build.
    pub fn unlimited() -> Arc<Self> {
        Self::new(usize::MAX)
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// Most bytes reserved at once.
    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }

    /// Let the heap grow by a share of what `resources` found available.
    /// Called when a build starts and again at stage boundaries, by when
    /// other processes and the page cache have moved.
    pub fn sample(&self, resources: &Resources) {
        let Some(live) = alloc::live_bytes() else {
            return;
        };
        let room = resources.available_memory_bytes / 100 * HEAP_CEILING_PERCENT;
        let ceiling = (live as u64).saturating_add(room);
        self.heap_ceiling.store(
            usize::try_from(ceiling).unwrap_or(usize::MAX),
            Ordering::Relaxed,
        );
    }

    /// The bytes one buffer that can spill may hold: a fair share of what
    /// the buffers that cannot spill leave of the pool.
    pub fn fair_share(&self) -> usize {
        let unspillable = self
            .used()
            .saturating_sub(self.spillable_used.load(Ordering::Relaxed));
        self.limit.saturating_sub(unspillable) / self.spillable.load(Ordering::Relaxed).max(1)
    }

    /// Reserve `bytes` if they fit, in the pool and under the heap ceiling.
    fn try_reserve(&self, bytes: usize, spillable: bool) -> bool {
        if let Some(live) = alloc::live_bytes()
            && live.saturating_add(bytes) > self.heap_ceiling.load(Ordering::Relaxed)
        {
            return false;
        }
        let mut used = self.used.load(Ordering::Relaxed);
        loop {
            let Some(next) = used.checked_add(bytes).filter(|next| *next <= self.limit) else {
                return false;
            };
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => {
                    self.peak.fetch_max(next, Ordering::Relaxed);
                    if spillable {
                        self.spillable_used.fetch_add(bytes, Ordering::Relaxed);
                    }
                    return true;
                }
                Err(current) => used = current,
            }
        }
    }

    /// Reserve `bytes` even past the limit: the minimum a caller needs to make
    /// progress, such as a single item after spilling everything else.
    fn force_reserve(&self, bytes: usize, spillable: bool) {
        let next = self
            .used
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        self.peak.fetch_max(next, Ordering::Relaxed);
        if spillable {
            self.spillable_used.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    fn release(&self, bytes: usize, spillable: bool) {
        self.used.fetch_sub(bytes, Ordering::Relaxed);
        if spillable {
            self.spillable_used.fetch_sub(bytes, Ordering::Relaxed);
        }
    }
}

/// Bytes held from a budget, returned when dropped.
#[derive(Debug)]
pub struct Reservation {
    budget: Arc<Budget>,
    bytes: usize,
    spillable: bool,
}

impl Reservation {
    /// A reservation of a buffer that cannot spill.
    pub fn empty(budget: &Arc<Budget>) -> Self {
        Self {
            budget: Arc::clone(budget),
            bytes: 0,
            spillable: false,
        }
    }

    /// A reservation that is granted regardless of the limit.
    pub fn forced(budget: &Arc<Budget>, bytes: usize) -> Self {
        let mut reservation = Self::empty(budget);
        reservation.force_grow(bytes);
        reservation
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn try_grow(&mut self, bytes: usize) -> bool {
        let granted = self.budget.try_reserve(bytes, self.spillable);
        if granted {
            self.bytes += bytes;
        }
        granted
    }

    pub fn force_grow(&mut self, bytes: usize) {
        self.budget.force_reserve(bytes, self.spillable);
        self.bytes += bytes;
    }

    pub fn release_all(&mut self) {
        self.budget.release(self.bytes, self.spillable);
        self.bytes = 0;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// The reservation of a buffer that can spill. While it lives the pool's
/// fair share is split with it.
#[derive(Debug)]
pub struct Spillable {
    reservation: Reservation,
}

impl Spillable {
    pub fn new(budget: &Arc<Budget>) -> Self {
        budget.spillable.fetch_add(1, Ordering::Relaxed);
        Self {
            reservation: Reservation {
                budget: Arc::clone(budget),
                bytes: 0,
                spillable: true,
            },
        }
    }

    /// The bytes this buffer may hold now, at most `cap`.
    pub fn share(&self, cap: usize) -> usize {
        self.reservation.budget.fair_share().min(cap).max(1)
    }

    pub fn reservation(&mut self) -> &mut Reservation {
        &mut self.reservation
    }

    pub fn bytes(&self) -> usize {
        self.reservation.bytes()
    }

    /// Keep what is reserved as a buffer that no longer spills or grows,
    /// such as a sorter's sorted buffer being read.
    pub fn into_reservation(mut self) -> Reservation {
        let bytes = self.reservation.bytes();
        self.reservation.release_all();
        Reservation::forced(&self.reservation.budget, bytes)
    }
}

impl Drop for Spillable {
    fn drop(&mut self) {
        self.reservation
            .budget
            .spillable
            .fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_respect_the_limit_and_track_the_peak() {
        let budget = Budget::new(100);
        let mut first = Reservation::empty(&budget);
        assert!(first.try_grow(60));
        let mut second = Reservation::empty(&budget);
        assert!(!second.try_grow(50));
        assert!(second.try_grow(40));
        assert_eq!(budget.used(), 100);
        drop(first);
        assert_eq!(budget.used(), 40);
        second.force_grow(80);
        assert_eq!(budget.used(), 120);
        assert_eq!(budget.peak(), 120);
        drop(second);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn spillable_buffers_split_what_the_others_leave() {
        let budget = Budget::new(1_000);
        let _fixed = Reservation::forced(&budget, 200);
        let mut first = Spillable::new(&budget);
        assert_eq!(first.share(usize::MAX), 800);
        assert!(first.reservation().try_grow(300));
        let second = Spillable::new(&budget);
        assert_eq!(second.share(usize::MAX), 400);
        assert_eq!(second.share(100), 100);
        let kept = first.into_reservation();
        assert_eq!(kept.bytes(), 300);
        // The kept buffer no longer spills, so it is taken off the pool first.
        assert_eq!(second.share(usize::MAX), 500);
        drop(kept);
        drop(second);
        assert_eq!(budget.used(), 200);
    }

    #[test]
    fn the_derived_budget_is_half_of_what_is_left_within_bounds() {
        let available = |gib: u64| Resources {
            usable_cpus: 8,
            total_memory_bytes: 32 << 30,
            available_memory_bytes: gib << 30,
            ..Resources::default()
        };
        assert_eq!(derived_budget(&available(2), 1 << 30), 512 << 20);
        assert_eq!(
            derived_budget(&available(16), 2 << 30),
            MAX_DERIVED_BUDGET_BYTES
        );
        assert_eq!(derived_budget(&available(16), 20 << 30), MIN_BUDGET_BYTES);
    }
}
