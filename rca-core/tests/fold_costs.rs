#![cfg(feature = "allocation-measurement")]
//! Finite public-path allocation assay and isolated dispatch-container controls.
//! Container timings are not an end-to-end performance winner claim.
use rca_core::{FoldKey, KeyedState};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;
#[cfg(feature = "local")]
#[path = "fold/local_driver.rs"]
mod local_driver;
#[path = "fold/support.rs"]
mod support;

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Records elapsed call time and allocation deltas around one operation.
fn measure<T>(f: impl FnOnce() -> T) -> (T, (u64, u64, u128)) {
    let before = dhat::HeapStats::get();
    let start = Instant::now();
    let result = f();
    let elapsed = start.elapsed().as_nanos();
    let after = dhat::HeapStats::get();
    (
        result,
        (
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
            elapsed,
        ),
    )
}
/// Defines the same public fold workload for Shared and Local carriers.
macro_rules! public_case {
    () => {
        use super::{Arc, AtomicUsize, Cell, ControlFlow, FoldKey, KeyedState, Ordering, measure};
        use domain::{EntryMount, Feature, FeatureContext, Fold, MountChildren, Runtime, Store};

        struct Owned(Cell<usize>);
        struct Sum;
        impl FoldKey for Sum {
            type Value = Owned;

            fn identity() -> Owned {
                Owned(Cell::new(0))
            }

            fn reduce(acc: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
                acc.0.set(acc.0.get() + next.0.get());
                ControlFlow::Continue(acc)
            }
        }

        type Rows = KeyedState<usize, usize>;
        enum Action {
            Reverse,
            Remove,
            Insert,
        }

        struct Root;
        impl Feature for Root {
            type State = Rows;
            type Action = Action;

            fn update(
                &mut self,
                rows: &mut Rows,
                action: &Action,
                _: &mut FeatureContext<'_, Rows, Action>,
            ) {
                match action {
                    Action::Reverse => rows.reverse(),
                    Action::Remove => {
                        rows.remove(&0);
                    }
                    Action::Insert => rows.insert(0, 1),
                }
            }
        }

        struct Child;
        impl Feature for Child {
            type State = usize;
            type Action = ();

            fn update(&mut self, _: &mut usize, _: &(), _: &mut FeatureContext<'_, usize, ()>) {}
        }

        /// Measures a real keyed fold before and after roster changes.
        pub(super) fn public_width(width: usize) -> (u64, u64) {
            let domain = module_path!();
            let driver = Driver::default();
            let calls = Arc::new(AtomicUsize::new(0));
            let fold = Fold::<Rows, Sum>::new();
            let selected = Arc::clone(&calls);
            let declared = fold.clone();
            let (store, construction) = measure(|| {
                let rows: Rows = (0..width).map(|id| (id, 1)).collect();
                let store = Store::builder(rows, Root, Runtime::new(driver.clone()))
                    .keyed_with_children(
                        |rows| rows,
                        |rows| rows,
                        move |_, _| {
                            let fold = declared.clone();
                            let calls = Arc::clone(&selected);
                            (
                                EntryMount::new(Child),
                                move |_: &usize,
                                      children: &mut MountChildren<'_, Rows, Action, usize, ()>| {
                                    let calls = Arc::clone(&calls);
                                    children.contribute(&fold, move |value| {
                                        calls.fetch_add(1, Ordering::Relaxed);
                                        Owned(Cell::new(*value))
                                    });
                                },
                            )
                        },
                    )
                    .build()
                    .unwrap();
                driver.finish(store.ready()).unwrap();
                store
            });
            let (cold, cold_cost) = measure(|| driver.finish(store.fold(&fold)).unwrap());
            assert_eq!(cold.0.get(), width);
            let (warm, warm_cost) = measure(|| driver.finish(store.fold(&fold)).unwrap());
            assert_eq!(warm.0.get(), width);
            assert_eq!(calls.load(Ordering::Relaxed), width * 2);
            let (_, reorder) = measure(|| drop(driver.finish(store.send(Action::Reverse)).unwrap()));
            let (ordered, reordered_read) = measure(|| driver.finish(store.fold(&fold)).unwrap());
            assert_eq!(ordered.0.get(), width);
            if width != 0 {
                let (_, removal) = measure(|| drop(driver.finish(store.send(Action::Remove)).unwrap()));
                let (remaining, removal_read) =
                    measure(|| driver.finish(store.fold(&fold)).unwrap());
                assert_eq!(remaining.0.get(), width - 1);
                let (_, insertion) =
                    measure(|| drop(driver.finish(store.send(Action::Insert)).unwrap()));
                let (restored, insertion_read) =
                    measure(|| driver.finish(store.fold(&fold)).unwrap());
                assert_eq!(restored.0.get(), width);
                eprintln!(
                    "fold public domain={domain} width={width} remove={removal:?} read_removed={removal_read:?} reinsert={insertion:?} read_reinserted={insertion_read:?}"
                );
            }
            let (_, teardown) = measure(|| {
                drop(store);
                driver.turn();
            });
            let retained = dhat::HeapStats::get();
            drop(fold);
            let dropped = dhat::HeapStats::get();
            let retained_channel = (
                retained.curr_blocks - dropped.curr_blocks,
                retained.curr_bytes - dropped.curr_bytes,
            );
            eprintln!(
                "fold public domain={domain} width={width} construction={construction:?} cold={cold_cost:?} warm={warm_cost:?} reorder={reorder:?} reordered_read={reordered_read:?} teardown={teardown:?} retained_channel_after_store_drop={retained_channel:?}; units=(blocks,bytes,ns), includes deterministic driver"
            );
            (warm_cost.0, warm_cost.1)
        }
    };
}
mod shared {
    use super::support::Driver;
    use rca_core as domain;
    public_case!();
}
#[cfg(feature = "local")]
mod local {
    use super::local_driver::Driver;
    use rca_core::local as domain;
    public_case!();
}

/// Compares container lookup work without fold authority or callback traversal.
fn dispatch_containers(width: usize) {
    let owners: Vec<_> = (0..width).map(|_| Arc::new(())).collect();
    let entries: Vec<_> = owners
        .iter()
        .enumerate()
        .map(|(id, owner)| (id, Arc::downgrade(owner)))
        .collect();
    let mut linear = entries.clone();
    let (mut tree, tree_construction) = measure(|| {
        entries
            .iter()
            .map(|(id, weak)| (*id, Weak::clone(weak)))
            .collect::<BTreeMap<_, _>>()
    });
    let (mut sorted, sorted_construction) = measure(|| entries.clone());
    let mut prune_checks = 0_u64;
    let mut linear_comparisons = 0_u64;
    let ((), linear_cost) = measure(|| {
        for id in 0..width {
            // Exact old prototype work: every lookup first scans ALL weak
            // entries with retain, then upgrades candidates before comparing.
            linear.retain(|(_, cell)| {
                prune_checks += 1;
                cell.strong_count() != 0
            });
            let found = linear.iter().find_map(|(current, cell)| {
                linear_comparisons += 1;
                let live = cell.upgrade()?;
                (*current == id).then_some(live)
            });
            std::hint::black_box(found.unwrap());
        }
    });
    let mut sorted_comparisons = 0_u64;
    let ((), sorted_cost) = measure(|| {
        for id in 0..width {
            let slot = entries
                .binary_search_by(|(current, _)| {
                    sorted_comparisons += 1;
                    current.cmp(&id)
                })
                .unwrap();
            std::hint::black_box(entries[slot].1.upgrade().unwrap());
        }
    });
    let ((), tree_cost) = measure(|| {
        for id in 0..width {
            std::hint::black_box(tree.get(&id).unwrap().upgrade().unwrap());
        }
    });
    assert_eq!(prune_checks, (width as u64) * (width as u64));
    assert_eq!(linear_comparisons, (width as u64) * (width as u64 + 1) / 2);
    assert_eq!(linear_cost.0, 0);
    assert_eq!(sorted_cost.0, 0);
    assert_eq!(tree_cost.0, 0);
    eprintln!(
        "fold dispatch width={width} old_vec_retain_checks={prune_checks} old_vec_find_comparisons={linear_comparisons} sorted_comparisons={sorted_comparisons} old_vec={linear_cost:?} sorted_vec={sorted_cost:?} btree={tree_cost:?}; lookup-only containers, no authority or callback traversal"
    );
    if width != 0 {
        let id = width / 2;
        let ((), tree_churn) = measure(|| {
            for _ in 0..32 {
                let value = tree.remove(&id).unwrap();
                assert!(tree.insert(id, value).is_none());
            }
        });
        let ((), sorted_churn) = measure(|| {
            for _ in 0..32 {
                let index = sorted.binary_search_by_key(&id, |(key, _)| *key).unwrap();
                let value = sorted.remove(index);
                sorted.insert(index, value);
            }
        });
        eprintln!(
            "fold dispatch width={width} btree_construct={tree_construction:?} sorted_construct={sorted_construction:?} middle_remove_reinsert_32_btree={tree_churn:?} middle_remove_reinsert_32_sorted={sorted_churn:?} sorted_shifted_entries={}",
            64 * (width - id - 1)
        );
    }
}

#[test]
fn finite_public_reads_and_actual_vec_prune_work() {
    let _profiler = if let Some(path) = std::env::var_os("RCA_FOLD_ALLOCATION_PROFILE") {
        let path = std::path::PathBuf::from(path);
        assert!(
            !path.exists(),
            "allocation evidence must not be overwritten"
        );
        eprintln!("Fold retained allocation profile: {}", path.display());
        dhat::Profiler::builder()
            .file_name(path)
            .trim_backtraces(None)
            .build()
    } else {
        dhat::Profiler::builder().testing().build()
    };
    let mut shared_costs = Vec::new();
    #[cfg(feature = "local")]
    let mut local_costs = Vec::new();
    for width in [0, 1, 3, 16, 256, 1_000, 10_000] {
        shared_costs.push(shared::public_width(width));
        #[cfg(feature = "local")]
        local_costs.push(local::public_width(width));
        dispatch_containers(width);
    }
    assert!(
        shared_costs.iter().all(|cost| *cost == shared_costs[0]),
        "synchronous Shared reads allocate no per-row owned routes: {shared_costs:?}"
    );
    #[cfg(feature = "local")]
    assert!(
        local_costs.iter().all(|cost| *cost == local_costs[0]),
        "synchronous Local reads allocate no per-row owned routes: {local_costs:?}"
    );
    #[cfg(feature = "test-util")]
    eprintln!(
        "fold test-util optional typed gate field bytes={}",
        std::mem::size_of::<Option<rca_core::testing::TransactionGate<(), ()>>>()
    );
}
