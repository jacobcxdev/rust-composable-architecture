#![cfg(feature = "local")]

//! Removal history inspection uses the same owned-output assertion frontier.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use futures_executor::block_on;
use futures_util::FutureExt;
use rca_core::local::{Feature, FeatureContext, OptionalMount};
use rca_core::{MountIdentity, OutputFlow};
use rca_test::local::TestStore;

/// Keeps parent presence distinct from the optional child inside that parent.
struct State {
    #[expect(
        clippy::option_option,
        reason = "Parent and child mount presence are independent optional states"
    )]
    parent: Option<Option<Rc<Cell<usize>>>>,
    /// Retains the exact removed value after its owned output is consumed.
    outcome: Option<Rc<Cell<usize>>>,
}
struct Root;
impl Feature for Root {
    type State = State;
    type Action = ();
    fn update(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {}
}
struct Parent;
impl Feature for Parent {
    type State = Option<Rc<Cell<usize>>>;
    type Action = ();
    fn update(
        &mut self,
        _: &mut Self::State,
        (): &(),
        _: &mut FeatureContext<'_, Self::State, ()>,
    ) {
    }
}
struct Child;
impl Feature for Child {
    type State = Rc<Cell<usize>>;
    type Action = ();
    fn update(
        &mut self,
        _: &mut Self::State,
        (): &(),
        _: &mut FeatureContext<'_, Self::State, ()>,
    ) {
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One ordered scenario retains ownership, history inspection and assertion acknowledgement"
)]
fn output_origin_keeps_the_pending_owned_value_and_assertion_boundary() {
    for failed_match in [false, true] {
        block_on(async {
            let value = Rc::new(Cell::new(7));
            let identity = Rc::as_ptr(&value);
            let sites = Rc::new(RefCell::new(None));
            let export = Rc::clone(&sites);
            let (builder, parent_site) = TestStore::projected_builder(
                State {
                    parent: Some(Some(value)),
                    outcome: None,
                },
                Root,
                |state: &State| {
                    (
                        state.parent.as_ref().is_some_and(Option::is_some),
                        state.outcome.as_ref().map(|value| value.get()),
                    )
                },
            )
            .mount_with_children(
                OptionalMount::new(
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
                ),
                move |_, children| {
                    *export.borrow_mut() = Some(children.optional_removable(
                        |state| state,
                        |state| state,
                        |state| {
                            state.as_ref().map(|_| MountIdentity {
                                key: (),
                                replacement: 0,
                                configuration: 0,
                            })
                        },
                        |_| Child,
                    ));
                },
            );
            let (builder, output) = builder.output::<Rc<Cell<usize>>>(&parent_site, |handlers| {
                handlers.at_root(|state, value, _| {
                    state.outcome = Some(value);
                    OutputFlow::Consumed
                });
            });
            let (builder, wrong) = builder.output::<Rc<Cell<usize>>>(&parent_site, |_| {});
            let mut store = builder.build();
            store
                .receive_structural(|_| {})
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            let (child_site, removal) = sites.borrow_mut().take().unwrap();
            let parent = store.acquire(&parent_site).await.unwrap().unwrap();
            let child = store.acquire(&child_site).await.unwrap().unwrap();
            let history = Rc::new(RefCell::new(None));
            let saved = Rc::clone(&history);
            let transferred = output.clone();
            let mut removing = Box::pin(parent.remove_child(
                &removal,
                &child,
                |_| {},
                move |_, removed, context| {
                    *saved.borrow_mut() = Some(removed.origin().clone());
                    removed
                        .into_output(
                            context,
                            &transferred,
                            |value| value,
                            |result| {
                                result.unwrap();
                            },
                        )
                        .unwrap();
                },
            ));
            assert!(removing.as_mut().now_or_never().is_none());
            let boundary = store
                .receive_update(|expected| expected.0 = false)
                .await
                .unwrap();
            let removal_task = removing.await.unwrap();
            let pending = store.finish().await.unwrap_err();
            assert!(pending.finish_diagnostics().unwrap().has_pending_action());
            if failed_match {
                let mismatch = store
                    .receive_output(
                        &output,
                        |_| false,
                        |_| panic!("mismatch must not acknowledge"),
                    )
                    .await
                    .expect_err("Output mismatch must retain its assertion boundary");
                assert!(mismatch.is_action_mismatch());
            }
            let origin = history.borrow_mut().clone().unwrap();
            assert_eq!(store.output_origin(&wrong), None);
            assert_eq!(store.output_origin(&output), Some(origin.clone()));
            assert_eq!(store.output_origin(&output), Some(origin));
            let received = store
                .receive_output(
                    &output,
                    |value| Rc::as_ptr(value) == identity && value.get() == 7,
                    |expected| expected.1 = Some(7),
                )
                .await;
            if failed_match {
                assert!(received.unwrap_err().is_incomplete_assertion());
                store
                    .assert_state(|state| {
                        assert_eq!(
                            (
                                state.parent.as_ref().is_some_and(Option::is_some),
                                state.outcome.as_ref().map(|value| value.get()),
                            ),
                            (false, Some(7))
                        );
                    })
                    .await
                    .unwrap();
            } else {
                received.unwrap().finish().await.unwrap();
            }
            assert_eq!(store.output_origin(&output), None);
            removal_task.finish().await;
            boundary.finish().await.unwrap();
            if failed_match {
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(store);
                }))
                .expect_err("Drop must enforce the failed output matcher");
                assert!(
                    panic.downcast_ref::<String>().unwrap().starts_with(
                        "TestStore teardown failed: TestStore incomplete assertion for transaction boundary at "
                    )
                );
            } else {
                store.finish().await.unwrap();
            }
        });
    }
}
