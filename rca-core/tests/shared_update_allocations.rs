//! Compares allocation counts for equivalent fallible and infallible shared updates.

#![cfg(feature = "allocation-measurement")]

use std::cell::Cell;
use std::hint::black_box;

use rca_core::Shared;

/// Counts whole-process allocations only inside the explicit profiler intervals.
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

#[test]
fn infallible_and_fallible_updates_do_equal_work_without_allocation() {
    let shared = Shared::new(Some(0_u64));
    let projection = shared.project(Option::as_ref, Option::as_mut).unwrap();
    let mutations = Cell::new(0);
    let publications = Cell::new(0);
    // Darwin's standard mutex initialises its boxed primitive on first access.
    // Exclude that existing reference setup from the repeated mutation interval.
    shared.read(|_| ()).unwrap();
    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    for _ in 0..1_024 {
        black_box(
            shared
                .try_update(
                    |root| {
                        mutations.set(mutations.get() + 1);
                        *root.as_mut().unwrap() += 1;
                        Ok::<_, std::convert::Infallible>(*root)
                    },
                    || {
                        black_box(shared.read(|root| *root).unwrap());
                        publications.set(publications.get() + 1);
                    },
                )
                .unwrap(),
        );
        black_box(
            projection
                .try_update(
                    |value| {
                        mutations.set(mutations.get() + 1);
                        *value += 1;
                        Ok::<_, std::convert::Infallible>(*value)
                    },
                    || {
                        black_box(projection.read(|value| *value).unwrap());
                        publications.set(publications.get() + 1);
                    },
                )
                .unwrap(),
        );
    }
    let legacy = dhat::HeapStats::get();
    for _ in 0..1_024 {
        black_box(
            shared
                .update(
                    |root| {
                        mutations.set(mutations.get() + 1);
                        *root.as_mut().unwrap() += 1;
                        *root
                    },
                    || {
                        black_box(shared.read(|root| *root).unwrap());
                        publications.set(publications.get() + 1);
                    },
                )
                .unwrap(),
        );
        black_box(
            projection
                .update(
                    |value| {
                        mutations.set(mutations.get() + 1);
                        *value += 1;
                        *value
                    },
                    || {
                        black_box(projection.read(|value| *value).unwrap());
                        publications.set(publications.get() + 1);
                    },
                )
                .unwrap(),
        );
    }
    let current = dhat::HeapStats::get();
    drop(profiler);
    assert_eq!(legacy.total_blocks - before.total_blocks, 0);
    assert_eq!(legacy.total_bytes - before.total_bytes, 0);
    assert_eq!(current.total_blocks - legacy.total_blocks, 0);
    assert_eq!(current.total_bytes - legacy.total_bytes, 0);
    assert_eq!(mutations.get(), 4_096);
    assert_eq!(publications.get(), 4_096);
    assert_eq!(shared.read(|value| *value).unwrap(), Some(4_096));
    println!(
        "shared={} projection={} legacy=0blocks/0bytes update=0blocks/0bytes mutations=4096 publications=4096 final=4096",
        std::mem::size_of_val(&shared),
        std::mem::size_of_val(&projection)
    );
}
