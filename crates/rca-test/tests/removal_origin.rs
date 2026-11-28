//! Removal history inspection uses the same owned-output assertion frontier.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use futures_executor::block_on;
use futures_util::FutureExt;
use rca_core::{Feature, FeatureContext, MountIdentity, OptionalMount, OutputFlow};
use rca_test::TestStore;

/// Separates parent absence, child absence and the transferred removal output.
struct State {
    #[expect(
        clippy::option_option,
        reason = "Parent absence and child absence are independent nested mount states under test"
    )]
    parent: Option<Option<Box<Cell<usize>>>>,
    /// Retains the original child allocation after its output reaches the root.
    outcome: Option<Box<Cell<usize>>>,
}

struct Root;
impl Feature for Root {
    type State = State;
    type Action = ();
    fn update(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {}
}

struct Parent;
impl Feature for Parent {
    type State = Option<Box<Cell<usize>>>;
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
    type State = Box<Cell<usize>>;
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
    reason = "One removal-to-output schedule checks owned identity, origin inspection and acknowledgement"
)]
fn output_origin_keeps_the_pending_owned_value_and_assertion_boundary() {
    for failed_match in [false, true] {
        block_on(async {
            let value = Box::new(Cell::new(7));
            let identity = std::ptr::from_ref(&*value) as usize;
            let sites = Arc::new(Mutex::new(None));
            let export = Arc::clone(&sites);
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
                    *export.lock().unwrap() = Some(children.optional_removable(
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
            let (builder, output) = builder.output::<Box<Cell<usize>>>(&parent_site, |handlers| {
                handlers.at_root(|state, value, _| {
                    state.outcome = Some(value);
                    OutputFlow::Consumed
                });
            });
            let (builder, wrong) = builder.output::<Box<Cell<usize>>>(&parent_site, |_| {});
            let mut store = builder.build();
            store
                .receive_structural(|_| {})
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            let (child_site, removal) = sites.lock().unwrap().take().unwrap();
            let parent = store.acquire(&parent_site).await.unwrap().unwrap();
            let child = store.acquire(&child_site).await.unwrap().unwrap();
            let history = Arc::new(Mutex::new(None));
            let saved = Arc::clone(&history);
            let transferred = output.clone();
            let mut removing = Box::pin(parent.remove_child(
                &removal,
                &child,
                |_| {},
                move |_, removed, context| {
                    *saved.lock().unwrap() = Some(removed.origin().clone());
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
            let origin = history.lock().unwrap().clone().unwrap();
            assert_eq!(store.output_origin(&wrong), None);
            assert_eq!(store.output_origin(&output), Some(origin.clone()));
            assert_eq!(store.output_origin(&output), Some(origin));
            let received = store
                .receive_output(
                    &output,
                    |value| std::ptr::from_ref(&**value) as usize == identity && value.get() == 7,
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
