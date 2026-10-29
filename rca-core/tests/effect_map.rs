//! Measures eager mapping costs; effect map unit tests check the resulting shapes.

/// Counts allocations only in the explicitly selected mapping assay.
#[cfg(feature = "allocation-measurement")]
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

#[cfg(feature = "allocation-measurement")]
#[test]
fn eager_effect_mapping_does_not_allocate() {
    use std::hint::black_box;
    use std::time::Duration;

    use rca_core::{Effect, MonotonicInstant};

    // Construct payloads/cancellation identity before the measured mapping work.
    let effects = [
        Effect::<u8>::none(),
        Effect::send(7),
        Effect::send_after(Duration::from_secs(2), 8),
        Effect::send_at(MonotonicInstant::MAX, 9),
        Effect::cancel(10_u8),
    ];
    let mut allocations = [(0, 0); 5];
    let profiler = dhat::Profiler::builder().testing().build();
    for (index, effect) in effects.into_iter().enumerate() {
        let before = dhat::HeapStats::get();
        let mapped = black_box(black_box(effect).map(u16::from));
        let after = dhat::HeapStats::get();
        allocations[index] = (
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
        );
        drop(mapped);
    }
    drop(profiler);

    println!("eager map allocations (blocks, bytes): {allocations:?}");
    assert_eq!(allocations, [(0, 0); 5]);

    #[cfg(feature = "local")]
    local_eager_mapping_does_not_allocate();
}

#[cfg(all(feature = "allocation-measurement", feature = "local"))]
/// Repeats the cost assay with a local mapper and checks its call and owner counts.
fn local_eager_mapping_does_not_allocate() {
    use std::cell::Cell;
    use std::hint::black_box;
    use std::rc::Rc;
    use std::time::Duration;

    use rca_core::{MonotonicInstant, local};

    let effects = [
        local::Effect::<u8>::none(),
        local::Effect::send(7),
        local::Effect::send_after(Duration::from_secs(2), 8),
        local::Effect::send_at(MonotonicInstant::MAX, 9),
        local::Effect::cancel(10_u8),
    ];
    let calls = Rc::new(Cell::new(0));
    let mut allocations = [(0, 0); 5];
    let profiler = dhat::Profiler::builder().testing().build();

    for (index, effect) in effects.into_iter().enumerate() {
        let called = Rc::clone(&calls);
        let before = dhat::HeapStats::get();
        let mapped = black_box(black_box(effect).map(move |value| {
            called.set(called.get() + 1);
            u16::from(value)
        }));
        let after = dhat::HeapStats::get();
        allocations[index] = (
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
        );
        drop(mapped);
    }
    drop(profiler);

    println!("local eager map allocations (blocks, bytes): {allocations:?}");
    assert_eq!(allocations, [(0, 0); 5]);
    assert_eq!(calls.get(), 3);
    assert_eq!(Rc::strong_count(&calls), 1);
}
