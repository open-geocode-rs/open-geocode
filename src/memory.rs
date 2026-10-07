//! Build memory accounting.
//!
//! One [`MemoryBudget`] is shared by every large buffer of a build: sort
//! buffers (counted by capacity, so `Vec` growth is included), merge read
//! buffers and the text indexing buffers. A buffer reserves bytes before it
//! grows; when the pool is full its owner spills to disk instead. The budget
//! is the `--memory-budget-mb` limit, and the build report records the peak.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Debug)]
pub struct MemoryBudget {
    limit: usize,
    used: AtomicUsize,
    peak: AtomicUsize,
}

impl MemoryBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
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

    /// Reserve `bytes` if they fit.
    pub fn try_reserve(&self, bytes: usize) -> bool {
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
                    return true;
                }
                Err(current) => used = current,
            }
        }
    }

    /// Reserve `bytes` even past the limit: the minimum a caller needs to make
    /// progress, such as a single item after spilling everything else.
    pub fn force_reserve(&self, bytes: usize) {
        let next = self
            .used
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        self.peak.fetch_max(next, Ordering::Relaxed);
    }

    pub fn release(&self, bytes: usize) {
        self.used.fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// Bytes held from a budget, returned when dropped.
#[derive(Debug)]
pub struct Reservation {
    budget: Arc<MemoryBudget>,
    bytes: usize,
}

impl Reservation {
    pub fn empty(budget: &Arc<MemoryBudget>) -> Self {
        Self {
            budget: Arc::clone(budget),
            bytes: 0,
        }
    }

    /// A reservation that is granted regardless of the limit.
    pub fn forced(budget: &Arc<MemoryBudget>, bytes: usize) -> Self {
        budget.force_reserve(bytes);
        Self {
            budget: Arc::clone(budget),
            bytes,
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn try_grow(&mut self, bytes: usize) -> bool {
        let granted = self.budget.try_reserve(bytes);
        if granted {
            self.bytes += bytes;
        }
        granted
    }

    pub fn force_grow(&mut self, bytes: usize) {
        self.budget.force_reserve(bytes);
        self.bytes += bytes;
    }

    pub fn release_all(&mut self) {
        self.budget.release(self.bytes);
        self.bytes = 0;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_respect_the_limit_and_track_the_peak() {
        let budget = MemoryBudget::new(100);
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
}
