use std::collections::{BTreeMap, HashMap};
use std::hint::black_box;

#[allow(dead_code)]
mod legacy;

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

fn measure<T>(label: &str, count: usize, build: impl FnOnce() -> T, check: impl FnOnce(&T)) {
    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    assert_eq!(
        before.curr_blocks, 0,
        "measurement starts without tracked blocks"
    );
    assert_eq!(
        before.curr_bytes, 0,
        "measurement starts without tracked bytes"
    );
    assert_eq!(
        before.max_bytes, 0,
        "measurement starts without a prior peak"
    );
    let rows = black_box(build());
    let after = dhat::HeapStats::get();
    check(&rows);
    drop(rows);
    let dropped = dhat::HeapStats::get();
    assert_eq!(
        dropped.curr_blocks, before.curr_blocks,
        "collection leaked blocks"
    );
    assert_eq!(
        dropped.curr_bytes, before.curr_bytes,
        "collection leaked bytes"
    );
    drop(profiler);
    println!(
        "{label},{count},{},{},{},{},{}",
        std::mem::size_of::<T>(),
        after.total_blocks - before.total_blocks,
        after.total_bytes - before.total_bytes,
        after.curr_bytes - before.curr_bytes,
        after.max_bytes
    );
}

fn main() {
    check_order();
    println!("storage,count,inline_bytes,allocations,allocated_bytes,retained_bytes,peak_bytes");
    for count in [0_usize, 1, 10, 1_000, 100_000] {
        measure(
            "rca_reserved",
            count,
            || {
                let mut rows = rca_core::KeyedState::with_capacity(count);
                for id in 0..count {
                    rows.insert(id, id);
                }
                rows
            },
            |rows| {
                assert_eq!(rows.len(), count);
                for id in 0..count {
                    assert_eq!(rows.get(&id), Some(&id));
                }
                assert!(rows.ids().copied().eq(0..count));
            },
        );
        measure(
            "legacy_hash_reserved",
            count,
            || {
                let mut rows =
                    legacy::KeyedState::<usize, usize, _>::new(HashMap::with_capacity(count));
                for id in 0..count {
                    rows.insert(id, id);
                }
                rows
            },
            |rows| {
                assert_eq!(rows.len(), count);
                for id in 0..count {
                    assert_eq!(rows.get(&id), Some(&id));
                }
            },
        );
        measure(
            "legacy_indexmap_reserved",
            count,
            || {
                let mut rows = legacy::KeyedState::<usize, usize, _>::new(
                    indexmap::IndexMap::with_capacity(count),
                );
                for id in 0..count {
                    rows.insert(id, id);
                }
                rows
            },
            |rows| {
                assert_eq!(rows.len(), count);
                for id in 0..count {
                    assert_eq!(rows.get(&id), Some(&id));
                }
                assert!(rows.keys().copied().eq(0..count));
            },
        );
        measure(
            "legacy_btree",
            count,
            || {
                let mut rows = legacy::KeyedState::<usize, usize, _>::new(BTreeMap::new());
                for id in 0..count {
                    rows.insert(id, id);
                }
                rows
            },
            |rows| {
                assert_eq!(rows.len(), count);
                for id in 0..count {
                    assert_eq!(rows.get(&id), Some(&id));
                }
            },
        );
    }
}

// Outside profiling: distinguish insertion order from sorted-key order and
// check that order-preserving removal followed by reinsertion appends the ID.
fn check_order() {
    let mut current = rca_core::KeyedState::new();
    let mut legacy = legacy::KeyedState::<usize, usize, _>::new(indexmap::IndexMap::new());
    for id in [7, 2, 9] {
        current.insert(id, id);
        legacy.insert(id, id);
    }
    assert!(current.ids().copied().eq([7, 2, 9]));
    assert!(legacy.keys().copied().eq([7, 2, 9]));
    assert_eq!(current.remove(&2), Some(2));
    assert_eq!(legacy.shift_remove(&2), Some(2));
    assert!(current.ids().copied().eq([7, 9]));
    assert!(legacy.keys().copied().eq([7, 9]));
    current.insert(2, 2);
    legacy.insert(2, 2);
    assert!(current.ids().copied().eq([7, 9, 2]));
    assert!(legacy.keys().copied().eq([7, 9, 2]));
}
