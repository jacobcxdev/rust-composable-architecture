//! Failed Local output matching retains the payload and its assertion obligation.

use std::cell::{Cell, OnceCell};
use std::rc::Rc;

use futures_executor::block_on;
use futures_util::FutureExt;
use rca_core::local::{Feature, FeatureContext, OptionalMount, Output, Task};
use rca_core::{MountIdentity, OutputFlow};
use rca_test::local::{CaptureKey, TestStore};

/// A move-only output with independently observed allocation and destruction.
struct Owned {
    /// Original allocation whose identity must survive failed matching.
    value: Rc<usize>,
    /// Counts output destruction independently of root-handler calls.
    drops: Rc<Cell<usize>>,
}

impl Drop for Owned {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

/// Holds a mounted child and the value committed by its root output handler.
struct State {
    /// Presence controls the optional mount's lifetime.
    child: Option<()>,
    /// Owns the output only after a successful match releases its handler.
    value: Option<Owned>,
}

/// Leaves mutations to the mounted child's output handler.
struct Root;

impl Feature for Root {
    type State = State;
    type Action = ();
    fn update(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {}
}

/// Supplies a Local mounted feature whose initial task emits the test output.
struct Child;

impl Feature for Child {
    type State = ();
    type Action = Rc<str>;
    fn update(&mut self, (): &mut (), _: &Rc<str>, _: &mut FeatureContext<'_, (), Rc<str>>) {}
}

/// Declares a separate projection of the committed output's owned allocation.
struct Captured;

impl CaptureKey for Captured {
    type Value = Option<Rc<usize>>;
    type Error = Rc<Cell<usize>>;
}

/// Projects immutable values without requiring the live output to implement `Clone`.
fn snapshot(state: &State) -> (bool, Option<usize>) {
    (
        state.child.is_some(),
        state.value.as_ref().map(|value| *value.value),
    )
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One ordered output schedule preserves matcher, snapshot, capture and teardown assertions"
)]
fn local_mount_initial_output_uses_common_snapshot_and_capture_boundary() {
    block_on(async {
        let drops = Rc::new(Cell::new(0));
        let calls = Rc::new(Cell::new(0));
        let payload = Rc::new(12);
        let output_cell = Rc::new(OnceCell::<Output<Owned>>::new());
        let initial_output = Rc::clone(&output_cell);
        let mut owned = Some(Owned {
            value: Rc::clone(&payload),
            drops: Rc::clone(&drops),
        });
        let mount = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.map(|()| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |()| Child,
        )
        .on_initial(
            |()| (),
            move |_, _| {
                let value = owned.take();
                let output = initial_output.get().unwrap().clone();
                Task::run(move |context| async move {
                    if let Some(value) = value {
                        context.output(&output, value).await.unwrap();
                    }
                })
            },
        );
        let (builder, site) = TestStore::projected_builder(
            State {
                child: Some(()),
                value: None,
            },
            Root,
            snapshot,
        )
        .mount_with_site(mount);
        let builder = builder
            .with_capture::<Captured>(|state| {
                Ok(state.value.as_ref().map(|value| Rc::clone(&value.value)))
            })
            .unwrap();
        let observed = Rc::clone(&calls);
        let (builder, output) = builder.output::<Owned>(&site, |handlers| {
            handlers.at_root(move |state, value, _| {
                observed.set(observed.get() + 1);
                state.value = Some(value);
                OutputFlow::Consumed
            });
        });
        let (builder, wrong) = builder.output::<Owned>(&site, |_| {});
        output_cell.set(output.clone()).unwrap();
        let mut store = builder.build();
        let initial = store.receive_structural(|_| {}).await.unwrap();
        let error = store
            .receive_output(
                &wrong,
                |_| panic!("wrong declaration"),
                |_| panic!("wrong snapshot"),
            )
            .await
            .expect_err("foreign output declaration cannot inspect this payload");
        assert!(error.is_no_received_action());
        let error = store
            .receive_output(&output, |_| false, |_| panic!("unmatched expectation"))
            .await
            .expect_err("mismatch retains the original output");
        assert!(error.is_action_mismatch());
        let panic = std::panic::AssertUnwindSafe(store.receive_output(
            &output,
            |_| panic!("matcher"),
            |_| panic!("unreached"),
        ))
        .catch_unwind()
        .await
        .unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"matcher"));
        assert_eq!(calls.get(), 0);
        assert_eq!(drops.get(), 0);
        let error = store
            .receive_output_with_captures(
                &output,
                |value| Rc::ptr_eq(&value.value, &payload),
                |expected| expected.1 = Some(12),
                |values| values.set::<Captured>(Some(Rc::clone(&payload))),
            )
            .await
            .unwrap_err();
        assert!(error.is_incomplete_assertion());
        initial.finish().await.unwrap();
        assert_eq!(calls.get(), 1);
        store
            .assert_state(|state| assert_eq!(snapshot(state), (true, Some(12))))
            .await
            .unwrap();
        assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
            .expect_err("teardown must report the incomplete output boundary");
        assert!(panic.downcast_ref::<String>().unwrap().starts_with(
            "TestStore teardown failed: TestStore incomplete assertion for transaction boundary at "
        ));
        assert_eq!(drops.get(), 1);
    });
}
