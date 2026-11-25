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
//! The build report records the peaks.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crate::{alloc, resources::Resources};

/// The heap may grow by at most this share of the memory available when the
/// pool was last sampled before tracked buffers must spill.
const HEAP_CEILING_PERCENT: u64 = 85;
const MIN_FAIR_SHARE_BYTES: usize = 16 << 20;
pub const MIN_BUDGET_BYTES: usize = 256 << 20;
pub const MAX_DERIVED_BUDGET_BYTES: usize = 1 << 30;

#[derive(Debug)]
pub struct Budget {
    limit: usize,
    used: AtomicUsize,
    spillable_used: AtomicUsize,
    spillable: AtomicUsize,
    peak: AtomicUsize,
    stage_peak: AtomicUsize,
    heap_ceiling: AtomicUsize,
    heap_limit: AtomicUsize,
}

impl Budget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
            spillable_used: AtomicUsize::new(0),
            spillable: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            stage_peak: AtomicUsize::new(0),
            heap_ceiling: AtomicUsize::new(usize::MAX),
            heap_limit: AtomicUsize::new(usize::MAX),
        })
    }

    pub fn unlimited() -> Arc<Self> {
        Self::new(usize::MAX)
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }

    pub fn stage_peak(&self) -> usize {
        self.stage_peak.load(Ordering::Relaxed)
    }

    pub fn reset_stage_peak(&self) {
        self.stage_peak.store(self.used(), Ordering::Relaxed);
    }

    pub fn sample(&self, resources: &Resources) {
        let Some(live) = alloc::live_bytes() else {
            return;
        };
        let room = resources.available_memory_bytes / 100 * HEAP_CEILING_PERCENT;
        let ceiling = usize::try_from((live as u64).saturating_add(room)).unwrap_or(usize::MAX);
        self.heap_ceiling.store(
            ceiling.min(self.heap_limit.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
    }

    pub fn limit_heap(&self, bytes: usize) {
        self.heap_limit.store(bytes, Ordering::Relaxed);
        self.heap_ceiling.fetch_min(bytes, Ordering::Relaxed);
    }

    /// The bytes one buffer that can spill may hold: a fair share of what
    /// the buffers that cannot spill leave of the pool, and at least
    /// [`MIN_FAIR_SHARE_BYTES`]. A full pool still lets each buffer grow to
    /// the floor, so the floor is capped at half a fair share of the whole
    /// pool. All floors together go at most half the limit over it.
    pub fn fair_share(&self) -> usize {
        let unspillable = self
            .used()
            .saturating_sub(self.spillable_used.load(Ordering::Relaxed));
        let spillable = self.spillable.load(Ordering::Relaxed).max(1);
        let floor = (self.limit / spillable / 2).min(MIN_FAIR_SHARE_BYTES);
        (self.limit.saturating_sub(unspillable) / spillable).max(floor)
    }

    fn try_reserve(&self, bytes: usize, spillable: bool) -> bool {
        if let Some(live) = alloc::live_bytes()
            && live.saturating_add(bytes) > self.heap_ceiling.load(Ordering::Relaxed)
        {
            return false;
        }
        if bytes == 0 {
            return self.used() <= self.limit;
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
                    raise(&self.peak, next);
                    raise(&self.stage_peak, next);
                    if spillable {
                        self.spillable_used.fetch_add(bytes, Ordering::Relaxed);
                    }
                    return true;
                }
                Err(current) => used = current,
            }
        }
    }

    fn force_reserve(&self, bytes: usize, spillable: bool) {
        let next = self
            .used
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        raise(&self.peak, next);
        raise(&self.stage_peak, next);
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

fn raise(peak: &AtomicUsize, value: usize) {
    if peak.load(Ordering::Relaxed) < value {
        peak.fetch_max(value, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub struct Reservation {
    budget: Arc<Budget>,
    bytes: usize,
    spillable: bool,
}

impl Reservation {
    pub fn empty(budget: &Arc<Budget>) -> Self {
        Self {
            budget: Arc::clone(budget),
            bytes: 0,
            spillable: false,
        }
    }

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

    pub fn take(&mut self) -> Self {
        let bytes = std::mem::take(&mut self.bytes);
        Self {
            budget: Arc::clone(&self.budget),
            bytes,
            spillable: self.spillable,
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.release_all();
    }
}

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

    pub fn share(&self, cap: usize) -> usize {
        self.reservation.budget.fair_share().min(cap).max(1)
    }

    pub fn reservation(&mut self) -> &mut Reservation {
        &mut self.reservation
    }

    pub fn bytes(&self) -> usize {
        self.reservation.bytes()
    }

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
        assert_eq!(second.share(usize::MAX), 500);
        drop(kept);
        drop(second);
        assert_eq!(budget.used(), 200);
    }

    #[test]
    fn a_pool_held_by_buffers_that_cannot_spill_leaves_a_floor() {
        let budget = Budget::new(1 << 30);
        let _node_store = Reservation::forced(&budget, 1 << 30);
        let first = Spillable::new(&budget);
        assert_eq!(first.share(usize::MAX), MIN_FAIR_SHARE_BYTES);
        assert_eq!(first.share(1_000), 1_000);
        let small = Budget::new(1_000);
        let _fixed = Reservation::forced(&small, 1_000);
        let buffers = [Spillable::new(&small), Spillable::new(&small)];
        assert_eq!(buffers[0].share(usize::MAX), 250);
    }

    #[test]
    fn a_taken_reservation_keeps_its_bytes_until_dropped() {
        let budget = Budget::new(1_000);
        let mut buffer = Spillable::new(&budget);
        assert!(buffer.reservation().try_grow(300));
        let handed = buffer.reservation().take();
        assert_eq!((buffer.bytes(), handed.bytes()), (0, 300));
        assert_eq!(budget.used(), 300);
        assert_eq!(buffer.share(usize::MAX), 1_000);
        budget.reset_stage_peak();
        drop(handed);
        assert_eq!(budget.used(), 0);
        assert_eq!(budget.stage_peak(), 300);
        budget.reset_stage_peak();
        assert_eq!(budget.stage_peak(), 0);
    }
}
