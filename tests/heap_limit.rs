use std::{alloc::System, hint::black_box, sync::Mutex};

use open_geocode::{
    alloc::{self, CountingAlloc},
    memory::{Budget, Reservation},
    resources::Resources,
};

#[global_allocator]
static ALLOCATOR: CountingAlloc<System> = CountingAlloc::new(System);

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

#[test]
fn heap_nobody_reserved_makes_the_budget_refuse_past_its_heap_limit() {
    let _alone = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|error| error.into_inner());
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

#[test]
fn a_sample_with_more_room_never_lifts_the_explicit_heap_limit() {
    let _alone = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let budget = Budget::new(1 << 30);
    let live = alloc::live_bytes().expect("counted");
    budget.limit_heap(live + (64 << 20));
    budget.sample(&Resources {
        available_memory_bytes: 64 << 30,
        ..Resources::default()
    });
    let mut held = Reservation::empty(&budget);
    assert!(held.try_grow(32 << 20));
    let untracked = black_box(vec![1u8; 128 << 20]);
    assert!(!held.try_grow(1 << 20), "the sample lifted the heap limit");
    drop(untracked);
    assert!(held.try_grow(1 << 20));
}
