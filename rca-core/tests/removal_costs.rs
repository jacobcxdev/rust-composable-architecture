#![cfg(all(feature = "allocation-measurement", feature = "test-util"))]
//! Matched public removal phases; actor phases explicitly include driver overhead.

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(feature = "local")]
#[path = "fold/local_driver.rs"]
mod local_driver;
#[path = "fold/support.rs"]
mod support;

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Counts requested allocations within one phase; it does not measure live bytes.
fn measure<T>(run: impl FnOnce() -> T) -> (T, (u64, u64)) {
    let before = dhat::HeapStats::get();
    let result = run();
    let after = dhat::HeapStats::get();
    (
        result,
        (
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
        ),
    )
}

/// Non-Clone payload: its live Box address and final drop must survive transfer.
struct Owned {
    value: Box<Cell<usize>>,
    drops: Arc<AtomicUsize>,
}
impl Drop for Owned {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
/// The descendants form a separate keyed collection inside the removable row.
struct ChildState {
    owned: Owned,
    descendants: rca_core::KeyedState<usize, ()>,
}
type Rows = rca_core::KeyedState<u8, ChildState>;
struct State {
    parent: Option<Rows>,
}

/// Matches one direct root mutation against three typed removal outcomes.
#[derive(Clone, Copy, Debug)]
enum Case {
    RootBaseline,
    Plain,
    Consumed,
    Rejected,
}

macro_rules! assay {
    ($carrier:literal) => {
        use super::*;
        use rca_core::{MountIdentity, OutputFlow, testing::TransactionKind};

        // RootBaseline removes the same row directly; the field is its Box address.
        struct Root(usize);
        impl Feature for Root {
            type State = State;
            type Action = ();
            fn update(
                &mut self,
                state: &mut State,
                _: &(),
                _: &mut FeatureContext<'_, State, ()>,
            ) {
                let value = state.parent.as_mut().unwrap().remove(&1).unwrap();
                assert_eq!((&raw const *value.owned.value) as usize, self.0);
                assert_eq!(value.owned.value.get(), 7);
                drop(value);
            }
        }
        struct Parent;
        impl Feature for Parent {
            type State = Rows;
            type Action = ();
            fn update(&mut self, _: &mut Rows, _: &(), _: &mut FeatureContext<'_, Rows, ()>) {}
        }
        struct Child;
        impl Feature for Child {
            type State = ChildState;
            type Action = ();
            fn update(
                &mut self,
                _: &mut ChildState,
                _: &(),
                _: &mut FeatureContext<'_, ChildState, ()>,
            ) {
            }
        }
        // A drop count proves every installed descendant retired before teardown.
        struct Descendant(Arc<AtomicUsize>);
        impl Drop for Descendant {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        impl Feature for Descendant {
            type State = ();
            type Action = ();
            fn update(&mut self, _: &mut (), _: &(), _: &mut FeatureContext<'_, (), ()>) {}
        }

        /// Runs the same cardinality and outcome matrix for this carrier.
        pub(super) fn exercise() {
            for descendants in [0, 1, 8] {
                for case in [Case::RootBaseline, Case::Plain, Case::Consumed, Case::Rejected] {
                    run(descendants, case);
                }
            }
        }

        #[expect(clippy::too_many_lines, reason = "One removal cost assay preserves descendant disposal, transfer and teardown measurement boundaries")]
        fn run(descendants: usize, case: Case) {
            let driver = Driver::default();
            let gate = TransactionGate::<State, ()>::new();
            let drops = Arc::new(AtomicUsize::new(0));
            let retired = Arc::new(AtomicUsize::new(0));
            let value = Box::new(Cell::new(7));
            let identity = (&raw const *value) as usize;
            let rows = [(
                1,
                ChildState {
                    owned: Owned {
                        value,
                        drops: Arc::clone(&drops),
                    },
                    descendants: (0..descendants).map(|id| (id, ())).collect(),
                },
            )]
            .into_iter()
            .collect();
            // The recipe exports the actual child site and removal declaration.
            let sites = Arc::new(Mutex::new(None));
            let export = Arc::clone(&sites);
            let retired_feature = Arc::clone(&retired);
            let (builder, parent_site) = Store::builder(
                State { parent: Some(rows) },
                Root(identity),
                Runtime::new(driver.clone()),
            )
                .transaction_gate(gate.clone())
                .mount_with_children(OptionalMount::new(
                    |state: &State| state.parent.as_ref(),
                    |state: &mut State| state.parent.as_mut(),
                    |state: &State| {
                        state.parent.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| Parent,
                ), move |_, children| {
                    let export = Arc::clone(&export);
                    let retired = Arc::clone(&retired_feature);
                    children.keyed_removable(
                        |rows| rows,
                        |rows| rows,
                        move |_, _| {
                            let export = Arc::clone(&export);
                            let retired = Arc::clone(&retired);
                            (
                                EntryMount::new(Child),
                                move |_: &ChildState,
                                      children: &mut MountChildren<'_, State, (), ChildState, ()>,
                                      removal| {
                                    *export.lock().unwrap() =
                                        Some((children.site().clone(), removal));
                                    let retired = Arc::clone(&retired);
                                    children.keyed(
                                        |state| &state.descendants,
                                        |state| &mut state.descendants,
                                        move |_, _| {
                                            EntryMount::new(Descendant(Arc::clone(&retired)))
                                        },
                                    );
                                },
                            )
                        },
                    );
                });
            // The root output handler consumes only the successful transfer.
            let handled = Arc::new(AtomicUsize::new(0));
            let handler = Arc::clone(&handled);
            let (builder, output) = builder.output::<Owned>(&parent_site, |handlers| {
                handlers.at_root(move |_, value, _| {
                    assert_eq!((&raw const *value.value) as usize, identity);
                    assert_eq!(value.value.get(), 7);
                    handler.fetch_add(1, Ordering::SeqCst);
                    drop(value);
                    OutputFlow::Consumed
                });
            });
            let store = builder.build().unwrap();
            driver.turn();
            let startup = gate.boundary().unwrap();
            assert!(gate.release_pending());
            driver.turn();
            assert!(gate.acknowledge(startup));
            driver.finish(store.ready()).unwrap();
            let (child_site, removal) = sites.lock().unwrap().take().unwrap();
            let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
            let child = driver.finish(store.acquire(&child_site)).unwrap().unwrap();
            let (_, idle_driver) = measure(|| driver.turn());
            assert_eq!(idle_driver.0, 1);
            // Plain and foreign-declaration refusal recover the original row.
            let recovered = Arc::new(AtomicUsize::new(0));
            let recovered_callback = Arc::clone(&recovered);
            // An output from another Store is a real public declaration, not a fake token.
            let foreign_driver = Driver::default();
            let (foreign_builder, foreign_site) = Store::builder(
                State {
                    parent: Some(Rows::new()),
                },
                Root(identity),
                Runtime::new(foreign_driver.clone()),
            )
                .mount_with_site(OptionalMount::new(
                    |state: &State| state.parent.as_ref(),
                    |state: &mut State| state.parent.as_mut(),
                    |state: &State| {
                        state.parent.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| Parent,
                ));
            let (foreign_builder, foreign) = foreign_builder.output::<Owned>(&foreign_site, |_| {});
            // A distinct existing driver keeps foreign setup out of measured actor turns.
            let foreign_store = foreign_builder.build().unwrap();
            foreign_driver.finish(foreign_store.ready()).unwrap();
            assert!(
                foreign_driver
                    .finish(foreign_store.acquire(&foreign_site))
                    .unwrap()
                    .is_some()
            );
            let transferred = if matches!(case, Case::Rejected) { foreign } else { output.clone() };

            // Publication owns one terminal-receipt Arc for each selected gated boundary.
            let task = if matches!(case, Case::RootBaseline) {
                let (future, creation) = measure(|| store.send(()));
                let mut future = std::pin::pin!(future);
                let (pending, admission) = measure(|| support::poll(future.as_mut()));
                assert!(pending.is_pending());
                let (_, publication) = measure(|| driver.turn());
                let boundary = gate.boundary().unwrap();
                assert_eq!(gate.pending_kind(), Some(TransactionKind::Action));
                let (_, release) = measure(|| assert!(gate.release_pending()));
                let (_, commit) = measure(|| driver.turn());
                let (done, completion) = measure(|| support::poll(future.as_mut()));
                let std::task::Poll::Ready(Ok(task)) = done else {
                    panic!("root committed")
                };
                assert_eq!((creation, release, completion), ((0, 0), (0, 0), (0, 0)));
                assert_eq!((admission.0, publication.0), (1, 4));
                assert_eq!(commit.0, if descendants == 8 { 23 } else { 15 });
                assert!(gate.acknowledge(boundary));
                task
            } else {
                let (future, creation) = measure(|| {
                    parent.remove_child(
                        &removal,
                        &child,
                        |_| {},
                        move |_, removed, context| {
                            if matches!(case, Case::Plain) {
                                let value = removed.into_inner();
                                assert_eq!((&raw const *value.owned.value) as usize, identity);
                                assert_eq!(value.owned.value.get(), 7);
                                recovered_callback.fetch_add(1, Ordering::SeqCst);
                                drop(value);
                            } else {
                                let result = removed.into_output(
                                    context,
                                    &transferred,
                                    |value| value.owned,
                                    |result| {
                                        result.unwrap();
                                    },
                                );
                                if matches!(case, Case::Rejected) {
                                    let (removed, error) = result.unwrap_err().into_parts();
                                    assert_eq!(
                                        error.reason(),
                                        rca_core::TaskAccessReason::ScopeEnded
                                    );
                                    let value = removed.into_inner();
                                    assert_eq!((&raw const *value.owned.value) as usize, identity);
                                    assert_eq!(value.owned.value.get(), 7);
                                    recovered_callback.fetch_add(1, Ordering::SeqCst);
                                    drop(value);
                                } else {
                                    result.unwrap();
                                }
                            }
                        },
                    )
                });
                let mut future = std::pin::pin!(future);
                let (pending, admission) = measure(|| support::poll(future.as_mut()));
                assert!(pending.is_pending());
                let (_, publication) = measure(|| driver.turn());
                let boundary = gate.boundary().unwrap();
                assert_eq!(gate.pending_kind(), Some(TransactionKind::Update));
                let (_, release) = measure(|| assert!(gate.release_pending()));
                let (_, commit) = measure(|| driver.turn());
                let (done, completion) = measure(|| support::poll(future.as_mut()));
                let std::task::Poll::Ready(Ok(task)) = done else {
                    panic!("removal committed")
                };
                assert_eq!((creation, release, completion), ((0, 0), (0, 0), (0, 0)));
                assert_eq!((admission.0, publication.0), (4, 7));
                // Observed whole-driver budgets, not a claim of production-only cost.
                let expected = if descendants == 8 { 29 } else { 21 };
                assert_eq!(
                    commit.0,
                    expected + if matches!(case, Case::Consumed) { 12 } else { 0 }
                );
                assert!(gate.acknowledge(boundary));
                task
            };
            assert_eq!(retired.load(Ordering::SeqCst), descendants);
            if matches!(case, Case::Consumed) {
                let (_, pending_output) = measure(|| {
                    driver.turn();
                    driver.turn();
                });
                assert_eq!(gate.pending_kind(), Some(TransactionKind::Output));
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                assert_eq!(handled.load(Ordering::SeqCst), 0);
                let boundary = gate.boundary().unwrap();
                let (origin, origin_inspection) = measure(|| gate.output_origin(&output));
                assert!(origin.is_some());
                let (matched, value_inspection) = measure(|| {
                    gate.inspect_output(&output, |value| {
                        (&raw const *value.value) as usize == identity
                    })
                });
                assert_eq!(matched, Some(true));
                let (_, release) = measure(|| assert!(gate.release_pending()));
                let (_, consumed) = measure(|| driver.turn());
                // Typed inspections reuse the boolean reservation; output admission
                // no longer recaptures the source authority's Vec and Arc ancestry.
                assert_eq!((origin_inspection, value_inspection), ((0, 0), (0, 0)));
                assert_eq!((pending_output.0, consumed.0), (16, 12));
                assert_eq!(release, (0, 0));
                assert!(gate.acknowledge(boundary));
            }
            driver.finish(task.finish());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(
                handled.load(Ordering::SeqCst),
                usize::from(matches!(case, Case::Consumed))
            );
            assert_eq!(
                recovered.load(Ordering::SeqCst),
                usize::from(matches!(case, Case::Plain | Case::Rejected))
            );
            assert!(gate.pending_kind().is_none());
            drop(foreign_store);
            foreign_driver.turn();
            drop(store);
            driver.turn();
        }
    };
}

mod shared {
    use super::support::Driver;
    use rca_core::testing::TransactionGate;
    use rca_core::{
        EntryMount, Feature, FeatureContext, MountChildren, OptionalMount, Runtime, Store,
    };
    assay!("Shared");
}
#[cfg(feature = "local")]
mod local {
    use super::local_driver::Driver;
    use rca_core::local::{
        EntryMount, Feature, FeatureContext, MountChildren, OptionalMount, Runtime, Store,
    };
    use rca_core::testing::local::TransactionGate;
    assay!("Local");
}

#[test]
fn owning_removal_phase_costs() {
    let profiler = dhat::Profiler::builder().testing().build();
    shared::exercise();
    #[cfg(feature = "local")]
    local::exercise();
    drop(profiler);
}
