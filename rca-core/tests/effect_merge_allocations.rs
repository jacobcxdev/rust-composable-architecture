//! Checks small, dense and sparse merge construction costs.

#![cfg(feature = "allocation-measurement")]

use std::hint::black_box;

use rca_core::Effect;

#[global_allocator]
/// Counts construction storage in isolated, synchronous windows.
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

/// Checks a construction budget without counting destruction or diagnostic output.
fn assert_merge_cost<E>(
    label: &str,
    make: impl FnOnce() -> E,
    blocks: u64,
    allocated_slots: usize,
    live_slots: usize,
) {
    let profiler = dhat::Profiler::builder().testing().build();
    let effect = black_box(make());
    let stats = dhat::HeapStats::get();
    let element_bytes = u64::try_from(size_of::<E>()).expect("effect size fits in u64");
    assert!(
        stats.total_blocks <= blocks,
        "{label}: {} blocks",
        stats.total_blocks
    );
    assert!(
        stats.total_bytes <= u64::try_from(allocated_slots).expect("budget fits") * element_bytes,
        "{label}: {} allocated bytes",
        stats.total_bytes
    );
    assert!(
        stats.max_bytes <= live_slots * size_of::<E>(),
        "{label}: {} peak bytes",
        stats.max_bytes
    );
    drop(effect);
    drop(profiler);
    eprintln!(
        "{label}: element_bytes={element_bytes} blocks={} bytes={} peak={}",
        stats.total_blocks, stats.total_bytes, stats.max_bytes
    );
}

/// Prevents geometric growth from retaining and copying avoidable dense capacity.
fn dense_merges_bound_vector_growth() {
    for (width, blocks, slots) in [(13, 2, 25), (64, 4, 124), (100, 5, 224), (1_000, 8, 2_020)] {
        assert_merge_cost(
            "shared dense",
            || Effect::merge(std::iter::repeat_with(|| Effect::send(())).take(width)),
            blocks,
            slots,
            width,
        );
        #[cfg(feature = "local")]
        assert_merge_cost(
            "local dense",
            || {
                rca_core::local::Effect::merge(
                    std::iter::repeat_with(|| rca_core::local::Effect::send(())).take(width),
                )
            },
            blocks,
            slots,
            width,
        );
    }
}

/// Keeps speculative and peak storage bounded when most source effects are empty.
fn sparse_merges_preserve_bounded_storage() {
    for (count, retained, blocks, slots) in [(1_000, 0, 0, 0), (130, 2, 1, 12)] {
        assert_merge_cost(
            "shared sparse",
            || {
                Effect::merge((0..count).map(|index| {
                    if index < retained {
                        Effect::send(())
                    } else {
                        Effect::none()
                    }
                }))
            },
            blocks,
            slots,
            slots,
        );
        #[cfg(feature = "local")]
        assert_merge_cost(
            "local sparse",
            || {
                rca_core::local::Effect::merge((0..count).map(|index| {
                    if index < retained {
                        rca_core::local::Effect::send(())
                    } else {
                        rca_core::local::Effect::none()
                    }
                }))
            },
            blocks,
            slots,
            slots,
        );
    }
}

/// Keeps ordinary small-vector growth when filtering removes the lower size hint.
fn filtered_and_unknown_merges_bound_growth() {
    // Each row gives width, allocations, finite-upper slots/peak, and unknown-upper slots/peak.
    for (width, blocks, finite_slots, finite_peak, unknown_slots, unknown_peak) in [
        (0, 0, 0, 0, 0, 0),
        (1, 1, 1, 1, 1, 1),
        (2, 2, 5, 4, 5, 4),
        (3, 2, 5, 4, 5, 4),
        (4, 2, 5, 4, 5, 4),
        (12, 4, 29, 16, 29, 16),
        (13, 4, 29, 16, 29, 16),
        (64, 6, 125, 64, 125, 64),
        (100, 7, 225, 100, 253, 128),
        (1_000, 10, 2_021, 1_000, 2_045, 1_024),
    ] {
        assert_merge_cost(
            "filtered",
            || Effect::merge((0..width).map(|_| Effect::send(())).filter(|_| true)),
            blocks,
            finite_slots,
            finite_peak,
        );
        assert_merge_cost(
            "unknown upper",
            || {
                let mut values = 0..width;
                Effect::merge(std::iter::from_fn(move || {
                    values.next().map(|_| Effect::send(()))
                }))
            },
            blocks,
            unknown_slots,
            unknown_peak,
        );
    }
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[
    (
        "all_non_empty_exact_size_widths_allocate_once",
        all_non_empty_exact_size_widths_allocate_once,
    ),
    (
        "dense_merges_bound_vector_growth",
        dense_merges_bound_vector_growth,
    ),
    (
        "sparse_merges_preserve_bounded_storage",
        sparse_merges_preserve_bounded_storage,
    ),
    (
        "filtered_and_unknown_merges_bound_growth",
        filtered_and_unknown_merges_bound_growth,
    ),
];

include!("support/allocation_main.rs");
