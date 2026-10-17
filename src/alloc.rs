//! Heap accounting for the global allocator.
//!
//! [`CountingAlloc`] wraps an allocator and keeps a count of the bytes live
//! on the heap, so the build can hold its tracked buffers back when memory
//! nobody reserved (boundaries, maps, Tantivy's own allocations) grows, and
//! report the most the heap held. Each thread counts into a local delta and
//! adds it to the shared count once it passes [`FLUSH_BYTES`], so the count
//! costs no shared cache line on the common path; it is behind by at most
//! that much per thread.
//!
//! The binary installs it over mimalloc:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOCATOR: CountingAlloc<mimalloc::MiMalloc> = CountingAlloc::new(mimalloc::MiMalloc);
//! ```

use std::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering},
};

/// Bytes a thread allocates or frees before it adds them to the shared count.
const FLUSH_BYTES: isize = 256 << 10;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    // A `Cell` of a plain integer has no destructor, so reading it never
    // allocates or registers anything, as an allocator requires.
    static DELTA: Cell<isize> = const { Cell::new(0) };
}

/// An allocator that counts the bytes live on the heap.
#[derive(Debug)]
pub struct CountingAlloc<A> {
    inner: A,
}

impl<A> CountingAlloc<A> {
    pub const fn new(inner: A) -> Self {
        Self { inner }
    }
}

fn count(bytes: isize) {
    let flushed = DELTA.try_with(|delta| {
        let next = delta.get() + bytes;
        if next.abs() < FLUSH_BYTES {
            delta.set(next);
            0
        } else {
            delta.set(0);
            next
        }
    });
    // A thread being torn down has no local delta left; count directly.
    let flush = flushed.unwrap_or(bytes);
    if flush != 0 {
        let live = LIVE.fetch_add(flush, Ordering::Relaxed) + flush;
        PEAK.fetch_max(live.max(0) as usize, Ordering::Relaxed);
    }
}

// SAFETY: every call is passed through to `inner` unchanged; the counting
// touches only atomics and a destructor-free thread local, never the heap.
unsafe impl<A: GlobalAlloc> GlobalAlloc for CountingAlloc<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let pointer = unsafe { self.inner.alloc(layout) };
        if !pointer.is_null() {
            INSTALLED.store(true, Ordering::Relaxed);
            count(layout.size() as isize);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        let pointer = unsafe { self.inner.alloc_zeroed(layout) };
        if !pointer.is_null() {
            INSTALLED.store(true, Ordering::Relaxed);
            count(layout.size() as isize);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: as for `alloc`.
        unsafe { self.inner.dealloc(pointer, layout) };
        count(-(layout.size() as isize));
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as for `alloc`.
        let moved = unsafe { self.inner.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            count(new_size as isize - layout.size() as isize);
        }
        moved
    }
}

/// Bytes live on the heap, or `None` when the counting allocator is not the
/// global allocator (as in the library's tests).
pub fn live_bytes() -> Option<usize> {
    INSTALLED
        .load(Ordering::Relaxed)
        .then(|| LIVE.load(Ordering::Relaxed).max(0) as usize)
}

/// The most bytes the heap held at once, when counted.
pub fn peak_bytes() -> Option<usize> {
    INSTALLED
        .load(Ordering::Relaxed)
        .then(|| PEAK.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_flushes_its_count_once_past_the_threshold() {
        // The library's tests run on the system allocator, so the counting
        // is driven directly.
        let before = LIVE.load(Ordering::Relaxed);
        std::thread::spawn(move || {
            count(FLUSH_BYTES / 2);
            assert_eq!(LIVE.load(Ordering::Relaxed), before);
            count(FLUSH_BYTES);
            count(-(FLUSH_BYTES / 2) * 3);
        })
        .join()
        .expect("thread");
        assert_eq!(LIVE.load(Ordering::Relaxed), before);
        assert!(PEAK.load(Ordering::Relaxed) >= (FLUSH_BYTES / 2 * 3) as usize);
    }
}
