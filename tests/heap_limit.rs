use std::{alloc::System, hint::black_box};

use open_geocode::{
    alloc::{self, CountingAlloc},
    memory::{Budget, Reservation},
};

#[global_allocator]
static ALLOCATOR: CountingAlloc<System> = CountingAlloc::new(System);

#[test]
fn heap_nobody_reserved_makes_the_budget_refuse_past_its_heap_limit() {
    let budget = Budget::new(1 << 30);
    let live = alloc::live_bytes().expect("counted");
    budget.limit_heap(live + (64 << 20));
    let mut held = Reservation::empty(&budget);
    assert!(held.try_grow(32 << 20));
    let untracked = black_box(vec![1u8; 128 << 20]);
    assert!(!held.try_grow(1 << 20), "the heap is past its limit");
    drop(untracked);
    assert!(held.try_grow(1 << 20));
}
