//! Compiler-first checks of the common query/read branch against real actors.
//!
//! These deliberately exercise both ordered reads and held-boundary queries.
//! Trait checks use Send-only snapshots or Local Rc values; they do not infer
//! stronger public bounds from the shared recording fixture's mutex.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use futures_executor::block_on;

use super::state_receipt;

fn send<T: Send>(_: &T) {}
fn sync<T: Sync>(_: &T) {}
fn unwind_safe<T: std::panic::UnwindSafe + std::panic::RefUnwindSafe>(_: &T) {}

/// Runs the same live actor schedule with each family's concrete query types.
macro_rules! query_branches {
    ($name:ident, $core:path, $runtime:path, $gate:path, $state:ty, $check:expr) => {
        #[test]
        fn $name() {
            use $core as core;
            struct Counter;
            impl core::Feature for Counter {
                type State = $state;
                type Action = ();
                fn reduce(
                    &mut self,
                    state: &mut Self::State,
                    _: &(),
                    _: &mut core::FeatureContext<'_, Self::State, ()>,
                ) {
                    state.set(state.get() + 1);
                }
            }
            let runtime = <$runtime>::new();
            let gate = <$gate>::new();
            let store =
                core::Store::feature_builder(Default::default(), Counter, runtime.runtime())
                    .transaction_gate(gate.clone())
                    .build()
                    .unwrap();
            for gated in [false, true] {
                if gated {
                    assert!(gate.reserve_root());
                    let _task = block_on(runtime.drive(store.send(()))).unwrap().unwrap();
                    assert!(gate.boundary().is_some());
                }
                let observed = Arc::new(Mutex::new(None));
                let output = Arc::clone(&observed);
                let expected = Cell::new(usize::from(gated));
                let future = state_receipt(
                    gate.boundary().is_some(),
                    move |state: &$state| {
                        assert_eq!(state.get(), expected.get());
                        *output.lock().unwrap() = Some(Cell::new(state.get()));
                    },
                    |assertion| runtime.drive(gate.assert_state(assertion).finish()),
                    |assertion| runtime.drive(store.read(assertion)),
                );
                ($check)(&future);
                block_on(future).unwrap().unwrap();
                assert_eq!(
                    observed.lock().unwrap().take().unwrap().get(),
                    usize::from(gated)
                );
                if let Some(boundary) = gate.boundary() {
                    assert!(gate.acknowledge(boundary));
                }
            }
            block_on(runtime.drive(store.shutdown())).unwrap().unwrap();
        }
    };
}

query_branches!(
    shared_query_and_ordered_read_keep_send_only_snapshots,
    rca_core,
    crate::DeterministicRuntime,
    rca_core::testing::TransactionGate<Cell<usize>, ()>,
    Cell<usize>,
    send
);

#[cfg(feature = "local")]
query_branches!(
    local_query_and_ordered_read_keep_rc_state,
    rca_core::local,
    crate::local::DeterministicRuntime,
    rca_core::testing::local::TransactionGate<std::rc::Rc<Cell<usize>>, ()>,
    std::rc::Rc<Cell<usize>>,
    |_| {}
);

#[test]
fn completed_shared_test_store_remains_sync_with_send_only_snapshot() {
    use rca_core::{Effect, ReduceContext, Reducer};
    struct Counter;
    impl Reducer for Counter {
        type State = Cell<usize>;
        type Action = ();
        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut ReduceContext<'_>,
        ) -> Effect<()> {
            Effect::none()
        }
    }
    let mut store = crate::TestStore::new(Cell::new(0), Counter);
    unwind_safe(&store.assertions.baseline);
    unwind_safe(&store.assertions.pending);
    sync(&store);
    let future = store.assert_state(|state| assert_eq!(state.get(), 0));
    send(&future);
    block_on(future).unwrap();
    block_on(store.finish()).unwrap();
}

#[cfg(feature = "local")]
#[test]
fn local_actor_query_and_read_preserve_capture_value_and_original_cause() {
    use super::super::obligations::{DeclaredCaptures, Local};
    use rca_core::local::{Feature, FeatureContext, Store};
    use std::cell::RefCell;
    use std::panic::Location;
    use std::rc::Rc;

    struct Key;
    impl crate::local::CaptureKey for Key {
        type Value = Rc<usize>;
        type Error = Rc<Cell<usize>>;
    }
    struct Counter;
    impl Feature for Counter {
        type State = Rc<usize>;
        type Action = ();
        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    let runtime = crate::local::DeterministicRuntime::new();
    let gate = rca_core::testing::local::TransactionGate::new();
    let initial = Rc::new(7);
    let owner = std::thread::current().id();
    let store = Store::feature_builder(Rc::clone(&initial), Counter, runtime.runtime())
        .transaction_gate(gate.clone())
        .build()
        .unwrap();
    let failed = Rc::new(Cell::new(false));
    let cause = Rc::new(Cell::new(99));
    let mut declarations = DeclaredCaptures::<Rc<usize>, Local>::default();
    let fail = Rc::clone(&failed);
    let original = Rc::clone(&cause);
    declarations
        .declare::<Key>(
            &initial,
            move |state| {
                assert_eq!(std::thread::current().id(), owner);
                if fail.get() {
                    Err(Rc::clone(&original))
                } else {
                    Ok(Rc::clone(state))
                }
            },
            Location::caller(),
        )
        .unwrap();
    for gated in [false, true] {
        if gated {
            assert!(gate.reserve_root());
            let _task = block_on(runtime.drive(store.send(()))).unwrap().unwrap();
        }
        for fail in [false, true] {
            failed.set(fail);
            let expected = declarations.unchanged();
            declarations.begin(expected, Location::caller(), "assert_state");
            let batch = declarations.batch();
            let captured = Rc::new(RefCell::new(None));
            let output = Rc::clone(&captured);
            block_on(state_receipt(
                gate.boundary().is_some(),
                move |state: &Rc<usize>| {
                    assert_eq!(std::thread::current().id(), owner);
                    *output.borrow_mut() = Some((
                        Rc::clone(state),
                        batch.capture(state, Location::caller(), "assert_state"),
                    ));
                },
                |assertion| runtime.drive(gate.assert_state(assertion).finish()),
                |assertion| runtime.drive(store.read(assertion)),
            ))
            .unwrap()
            .unwrap();
            let (snapshot, result) = captured.borrow_mut().take().unwrap();
            assert!(Rc::ptr_eq(&snapshot, &initial));
            match result {
                Ok(values) => {
                    assert!(!fail);
                    DeclaredCaptures::compare(&values).unwrap();
                    declarations.install(values);
                }
                Err(error) => {
                    assert!(fail);
                    let recovered = error.into_capture_error::<Key>().unwrap();
                    assert!(Rc::ptr_eq(&recovered, &cause));
                }
            }
            declarations.complete();
        }
        if let Some(boundary) = gate.boundary() {
            assert!(gate.acknowledge(boundary));
        }
    }
    block_on(runtime.drive(store.shutdown())).unwrap().unwrap();
}
