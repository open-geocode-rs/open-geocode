use std::{
    alloc::{GlobalAlloc, Layout},
    cell::Cell,
    sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering},
};

const FLUSH_BYTES: isize = 256 << 10;

static LIVE: AtomicIsize = AtomicIsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static STAGE_PEAK: AtomicUsize = AtomicUsize::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);

thread_local! {
    // A `Cell` of a plain integer has no destructor, so reading it never
    // allocates or registers anything, as an allocator requires.
    static DELTA: Cell<isize> = const { Cell::new(0) };
}

#[derive(Debug)]
pub struct CountingAlloc<A> {
    inner: A,
}

impl<A> CountingAlloc<A> {
    pub const fn new(inner: A) -> Self {
        Self { inner }
    }
}

#[inline]
fn mark_installed() {
    if !INSTALLED.load(Ordering::Relaxed) {
        INSTALLED.store(true, Ordering::Relaxed);
    }
}

#[inline]
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
        let live = (LIVE.fetch_add(flush, Ordering::Relaxed) + flush).max(0) as usize;
        // Read first, as for `INSTALLED`: the peaks rarely move.
        for peak in [&PEAK, &STAGE_PEAK] {
            if peak.load(Ordering::Relaxed) < live {
                peak.fetch_max(live, Ordering::Relaxed);
            }
        }
    }
}

// SAFETY: every call is passed through to `inner` unchanged; the counting
// touches only atomics and a destructor-free thread local, never the heap.
unsafe impl<A: GlobalAlloc> GlobalAlloc for CountingAlloc<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let pointer = unsafe { self.inner.alloc(layout) };
        if !pointer.is_null() {
            mark_installed();
            count(layout.size() as isize);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as for `alloc`.
        let pointer = unsafe { self.inner.alloc_zeroed(layout) };
        if !pointer.is_null() {
            mark_installed();
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

pub fn peak_bytes() -> Option<usize> {
    INSTALLED
        .load(Ordering::Relaxed)
        .then(|| PEAK.load(Ordering::Relaxed))
}

pub fn stage_peak_bytes() -> Option<usize> {
    INSTALLED
        .load(Ordering::Relaxed)
        .then(|| STAGE_PEAK.load(Ordering::Relaxed))
}

pub fn reset_stage_peak() {
    STAGE_PEAK.store(
        LIVE.load(Ordering::Relaxed).max(0) as usize,
        Ordering::Relaxed,
    );
}
