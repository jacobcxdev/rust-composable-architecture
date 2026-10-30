//! Checks exact merge-vector capacity for the supported small composition widths.

#![cfg(feature = "allocation-measurement")]

use std::hint::black_box;

use rca_core::Effect;

#[global_allocator]
/// Counts construction storage; this binary contains only one test.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Checks that every non-empty exact-size merge allocates once.
fn all_non_empty_exact_size_widths_allocate_once() {
    for width in 1..=12 {
        drop(black_box(Effect::merge(
            std::iter::repeat_with(|| Effect::send(())).take(width),
        )));
    }

    let profiler = dhat::Profiler::builder().testing().build();
    for width in 1..=12 {
        let before = dhat::HeapStats::get();
        let effect = black_box(Effect::merge(
            std::iter::repeat_with(|| Effect::send(())).take(width),
        ));
        let after = dhat::HeapStats::get();
        assert_eq!(
            after.total_blocks - before.total_blocks,
            1,
            "merge width {width} must allocate its effect vector once"
        );
        assert_eq!(
            after.total_bytes - before.total_bytes,
            u64::try_from(width * size_of::<Effect<()>>()).expect("measured width fits in u64"),
            "merge width {width} must allocate exact effect-vector capacity"
        );
        drop(effect);
    }
    drop(profiler);
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[(
    "all_non_empty_exact_size_widths_allocate_once",
    all_non_empty_exact_size_widths_allocate_once,
)];

include!("support/allocation_main.rs");
