//! Shared and Local observations of the same public Store lifecycle.
//!
//! Each phase counts its own operations; owners retained between phases make
//! these request totals unsuitable as independent leak checks. Warmups and the
//! final report vector remain outside each phase's measurement.

use futures_executor::block_on;

use super::{Allocations, measured};

/// Keeps the two family workloads identical without erasing their public types.
macro_rules! phases {
    ($name:ident, $core:path, $test:path) => {
        mod $name {
            use super::*;
            use $core as core_api;
            use $test as test_api;

            pub(super) struct Counter;
            impl core_api::Reducer for Counter {
                type State = usize;
                type Action = bool;
                fn reduce(
                    &mut self,
                    state: &mut usize,
                    start: &bool,
                    _: &mut rca_core::ReduceContext<'_>,
                ) -> core_api::Effect<bool> {
                    *state += 1;
                    if *start {
                        core_api::Effect::send(false)
                    } else {
                        core_api::Effect::none()
                    }
                }
            }

            /// Executes each named phase once, retaining task owners until their finish phase.
            pub(super) fn run() -> Vec<(&'static str, Allocations)> {
                let (mut store, construction) = measured(|| test_api::TestStore::new(0, Counter));
                let (start, send) =
                    measured(|| block_on(store.send_with(true, |state| *state = 1)).unwrap());
                let ((), query) = measured(|| {
                    block_on(store.assert_state(|state| assert_eq!(*state, 1))).unwrap()
                });
                let (received, receive) = measured(|| {
                    block_on(store.receive_with(|action| !*action, |state| *state = 2)).unwrap()
                });
                let ((), read) = measured(|| {
                    block_on(store.assert_state(|state| assert_eq!(*state, 2))).unwrap()
                });
                let ((), tasks) = measured(|| {
                    block_on(received.finish()).unwrap();
                    block_on(start.finish()).unwrap();
                });
                let ((), finish) = measured(|| block_on(store.finish()).unwrap());
                let ((), close) = measured(|| drop(store));
                vec![
                    ("construct", construction),
                    ("send", send),
                    ("gated_query", query),
                    ("receive", receive),
                    ("ordered_read", read),
                    ("task_finish", tasks),
                    ("finish", finish),
                    ("drop", close),
                ]
            }

            /// Returns Store size/alignment, then builder size/alignment for this fixture.
            pub(super) fn layouts() -> (usize, usize, usize, usize) {
                (
                    size_of::<test_api::TestStore<usize, bool>>(),
                    align_of::<test_api::TestStore<usize, bool>>(),
                    size_of::<test_api::TestStoreBuilder<usize, bool, Counter>>(),
                    align_of::<test_api::TestStoreBuilder<usize, bool, Counter>>(),
                )
            }
        }
    };
}
phases!(shared, rca_core, rca_test);
phases!(local, rca_core::local, rca_test::local);

#[test]
fn current_shared_and_local_public_host_phases_have_explicit_costs() {
    // This is a current-epoch paired observation, not a historic before/after claim.
    drop(shared::run());
    drop(local::run());
    for (family, phases, layouts) in [
        ("shared", shared::run(), shared::layouts()),
        ("local", local::run(), local::layouts()),
    ] {
        eprintln!("host family={family} layouts={layouts:?}");
        for (phase, cost) in phases {
            eprintln!(
                "host family={family} phase={phase} allocations={} bytes={} reallocations={}",
                cost.calls, cost.bytes, cost.reallocations
            );
        }
    }
}
