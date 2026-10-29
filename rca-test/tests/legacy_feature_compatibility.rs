//! Observable compatibility, not equality of private revisions or task IDs.
//!
//! Legacy effects (including `ReducerFeature`) complete through an action;
//! typed tasks complete through an update. Each needs its own `TestStore` receive.
//! Legacy scoped actions and mounted updates also retain their distinct errors.

use std::future::poll_fn;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Waker};

use futures_executor::block_on;
use futures_util::FutureExt;
use rca_core::{
    ActionPath, Effect, Feature, FeatureContext, MountIdentity, OptionalMount, OptionalStatePath,
    ReduceContext, Reducer, ReducerFeature, Store, Task, TaskAccessReason, TaskProgress,
};
use rca_test::{Completion, ControlledFuture, DeterministicRuntime, TestStore, controlled_future};

#[derive(Clone, Copy, Debug)]
enum Profile {
    Legacy,
    Adapted,
    Typed,
}
const PROFILES: [Profile; 3] = [Profile::Legacy, Profile::Adapted, Profile::Typed];

#[derive(Debug)]
enum Action {
    Add(usize),
    Start,
    Response(usize),
}

#[derive(Default)]
/// Separates client entry, pending polls and capture disposal across all profiles.
struct Probes {
    entered: AtomicUsize,
    pending: AtomicUsize,
    dropped: AtomicUsize,
}
struct Capture(Arc<Probes>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
struct Client {
    result: ControlledFuture<usize>,
    capture: Capture,
}
impl Client {
    /// Counts a real pending poll before returning the controlled result.
    async fn complete(self) -> usize {
        self.capture.0.entered.fetch_add(1, Ordering::SeqCst);
        let mut result = std::pin::pin!(self.result);
        let value = poll_fn(|cx| {
            let poll = result.as_mut().poll(cx);
            if poll.is_pending() {
                self.capture.0.pending.fetch_add(1, Ordering::SeqCst);
            }
            poll
        })
        .await
        .unwrap();
        drop(self.capture);
        value
    }
}
fn client() -> (Client, Completion<usize>, Arc<Probes>) {
    let (complete, result) = controlled_future();
    let probes = Arc::new(Probes::default());
    (
        Client {
            result,
            capture: Capture(Arc::clone(&probes)),
        },
        complete,
        probes,
    )
}

struct Legacy(Option<Client>);
impl Reducer for Legacy {
    type State = usize;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut usize,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Add(value) => *state += value,
            Action::Response(value) => *state = *value,
            Action::Start => {
                let client = self.0.take().unwrap();
                return Effect::run(move |sender| async move {
                    let value = client.complete().await;
                    sender.send(Action::Response(value)).await.unwrap();
                });
            }
        }
        Effect::none()
    }
}
struct Typed(Option<Client>);
impl Feature for Typed {
    type State = usize;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut usize,
        action: &Action,
        context: &mut FeatureContext<'_, usize, Action>,
    ) {
        match action {
            Action::Add(value) => *state += value,
            Action::Response(value) => *state = *value,
            Action::Start => {
                let client = self.0.take().unwrap();
                context.task(Task::run(move |context| async move {
                    let value = client.complete().await;
                    context.update(move |state| *state = value).await.unwrap();
                }));
            }
        }
    }
}
impl Profile {
    /// Keeps each public construction path under the same assertion boundary.
    fn test_store(self, client: Option<Client>) -> TestStore<usize, Action> {
        match self {
            Self::Legacy => TestStore::new(0, Legacy(client)),
            Self::Adapted => {
                TestStore::feature_builder(0, ReducerFeature::new(Legacy(client))).build()
            }
            Self::Typed => TestStore::feature_builder(0, Typed(client)).build(),
        }
    }
    /// Uses a plain deterministic host for post-cancellation ownership checks.
    fn store(self, client: Client, host: &DeterministicRuntime) -> Store<usize, Action> {
        match self {
            Self::Legacy => Store::new(0, Legacy(Some(client)), host.runtime()),
            Self::Adapted => {
                Store::feature_builder(0, ReducerFeature::new(Legacy(Some(client))), host.runtime())
                    .build()
                    .unwrap()
            }
            Self::Typed => Store::feature_builder(0, Typed(Some(client)), host.runtime())
                .build()
                .unwrap(),
        }
    }
    /// Matches the action or update emitted by the selected feature kind.
    async fn receive(self, store: &mut TestStore<usize, Action>, expected: usize) {
        match self {
            Self::Typed => {
                let _ = store
                    .receive_update(move |state| *state = expected)
                    .await
                    .unwrap();
            }
            Self::Legacy | Self::Adapted => {
                let _ = store
                    .receive_with(
                        |action| matches!(action, Action::Response(17)),
                        move |state| *state = expected,
                    )
                    .await
                    .unwrap();
            }
        }
    }
}

#[test]
fn synchronous_mutations_match_through_all_public_constructors() {
    block_on(async {
        for profile in PROFILES {
            let mut store = profile.test_store(None);
            store
                .send_with(Action::Add(3), |state| *state = 3)
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            store
                .send_with(Action::Add(2), |state| *state = 5)
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            store
                .assert_state(|state| assert_eq!(*state, 5))
                .await
                .unwrap();
            store.finish().await.unwrap();
        }
    });
}

#[test]
fn ready_and_observed_pending_results_commit_once_and_finish_their_lineage() {
    block_on(async {
        for profile in PROFILES {
            for ready in [true, false] {
                let (client, complete, probes) = client();
                let mut complete = Some(complete);
                if ready {
                    complete.take().unwrap().complete(17).unwrap();
                }
                let mut store = profile.test_store(Some(client));
                let task = store.send(Action::Start).await.unwrap();
                assert_eq!(probes.entered.load(Ordering::SeqCst), 1, "{profile:?}");
                if ready {
                    assert_eq!(probes.pending.load(Ordering::SeqCst), 0);
                } else {
                    assert!(
                        probes.pending.load(Ordering::SeqCst) > 0,
                        "client actually returned Pending: {profile:?}"
                    );
                    assert!(!task.is_finished());
                    complete.take().unwrap().complete(17).unwrap();
                }
                profile.receive(&mut store, 17).await;
                task.finish().await.unwrap();
                assert_eq!(probes.dropped.load(Ordering::SeqCst), 1);
                store
                    .assert_state(|state| assert_eq!(*state, 17))
                    .await
                    .unwrap();
                store.finish().await.unwrap();
            }
        }
    });
}

#[test]
fn cancellation_before_client_poll_destroys_the_owned_capture_without_a_commit() {
    for profile in PROFILES {
        let host = DeterministicRuntime::new();
        let (client, complete, probes) = client();
        let store = profile.store(client, &host);
        block_on(host.run(store.ready())).unwrap().unwrap();
        let task = block_on(host.run(store.send(Action::Start)))
            .unwrap()
            .unwrap();
        // The deterministic host returns the committed receipt before polling
        // newly spawned work. Prove that boundary instead of assuming it.
        assert_eq!(probes.entered.load(Ordering::SeqCst), 0, "{profile:?}");
        assert_eq!(probes.dropped.load(Ordering::SeqCst), 0);
        task.cancel();
        host.run_until_stalled().unwrap();
        assert!(task.is_cancelled());
        assert!(task.is_finished());
        assert_eq!(probes.entered.load(Ordering::SeqCst), 0);
        assert_eq!(probes.pending.load(Ordering::SeqCst), 0);
        assert_eq!(probes.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(complete.complete(17).unwrap_err().into_inner(), 17);
        assert_eq!(
            block_on(host.run(store.read(|state| *state)))
                .unwrap()
                .unwrap(),
            0
        );
    }
}

#[test]
fn omitted_completion_and_rejected_matcher_remain_pending_until_the_right_receive() {
    block_on(async {
        for profile in PROFILES {
            let (client, complete, _) = client();
            complete.complete(17).unwrap();
            let mut store = profile.test_store(Some(client));
            let task = store.send(Action::Start).await.unwrap();
            assert!(store.finish().await.is_err());
            store
                .assert_state(|state| assert_eq!(*state, 0))
                .await
                .unwrap();
            assert!(
                store.finish().await.is_err(),
                "inspection cannot acknowledge debt"
            );
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&calls);
            store
                .receive(move |_| {
                    observed.fetch_add(1, Ordering::SeqCst);
                    false
                })
                .await
                .unwrap_err();
            // An update rejects the wrong kind without entering an action
            // matcher; an effect's wrong matcher leaves its action available.
            assert_eq!(
                calls.load(Ordering::SeqCst),
                usize::from(!matches!(profile, Profile::Typed))
            );
            assert!(store.finish().await.is_err());
            profile.receive(&mut store, 17).await;
            task.finish().await.unwrap();
            store.finish().await.unwrap();
        }
    });
}

#[test]
fn wrong_comparison_leaves_sticky_debt_without_resetting_the_live_state() {
    block_on(async {
        for profile in PROFILES {
            let (client, complete, _) = client();
            complete.complete(17).unwrap();
            let mut store = profile.test_store(Some(client));
            let _task = store.send(Action::Start).await.unwrap();
            let panic = AssertUnwindSafe(profile.receive(&mut store, 99))
                .catch_unwind()
                .await
                .expect_err("wrong comparison must panic");
            let message = panic
                .downcast_ref::<String>()
                .expect("snapshot mismatch has a String diagnostic");
            let operation = if matches!(profile, Profile::Typed) {
                "receive_update"
            } else {
                "receive"
            };
            assert!(
                message.starts_with("TestStore state mismatch at "),
                "{message}"
            );
            assert!(
                message.contains(&format!(" during {operation}:")),
                "{message}"
            );
            store
                .assert_state(|state| assert_eq!(*state, 17))
                .await
                .unwrap();
            assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
            assert!(
                store
                    .send(Action::Add(1))
                    .await
                    .unwrap_err()
                    .is_incomplete_assertion()
            );
            let panic = catch_unwind(AssertUnwindSafe(|| drop(store)))
                .expect_err("Drop must retain the failed comparison");
            let message = panic
                .downcast_ref::<String>()
                .expect("teardown failure has a String diagnostic");
            assert!(
                message.starts_with(&format!(
                    "TestStore teardown failed: TestStore incomplete assertion for {operation} at "
                )),
                "{message}"
            );
        }
    });
}

#[derive(Clone, Debug, PartialEq)]
struct Child {
    id: u8,
    value: usize,
}
enum ParentAction {
    Remove,
    Restore,
    Child(Box<usize>),
}
struct Parent;
impl Reducer for Parent {
    type State = Option<Child>;
    type Action = ParentAction;
    fn reduce(
        &mut self,
        state: &mut Option<Child>,
        action: &ParentAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ParentAction> {
        match action {
            ParentAction::Remove => *state = None,
            ParentAction::Restore => *state = Some(Child { id: 7, value: 100 }),
            ParentAction::Child(value) => state.as_mut().unwrap().value += **value,
        }
        Effect::none()
    }
}
struct ChildFeature;
impl Feature for ChildFeature {
    type State = Child;
    type Action = Box<usize>;
    fn reduce(
        &mut self,
        state: &mut Child,
        action: &Box<usize>,
        _: &mut FeatureContext<'_, Child, Box<usize>>,
    ) {
        state.value += **action;
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One paired legacy/typed schedule checks removal, equal-ID reinsertion and late-result rejection"
)]
fn removed_and_equal_id_reinserted_children_reject_the_old_client_result() {
    let host = DeterministicRuntime::new();
    let initial = || Some(Child { id: 7, value: 0 });
    let legacy = Store::new(initial(), Parent, host.runtime());
    let old = block_on(host.run(legacy.scope_some_identified(
        OptionalStatePath::new(
            |state: &Option<Child>| state.as_ref(),
            |state: &mut Option<Child>| state.as_mut(),
        ),
        ActionPath::new(
            |action: &ParentAction| match action {
                ParentAction::Child(value) => Some(value),
                _ => None,
            },
            ParentAction::Child,
        ),
        |child| &child.id,
    )))
    .unwrap()
    .unwrap();
    let mount = OptionalMount::new(
        |state: &Option<Child>| state.as_ref(),
        |state: &mut Option<Child>| state.as_mut(),
        |state: &Option<Child>| {
            state.as_ref().map(|child| MountIdentity {
                key: child.id,
                replacement: 0,
                configuration: 0,
            })
        },
        |_| ChildFeature,
    );
    let (builder, site) =
        Store::feature_builder(initial(), ReducerFeature::new(Parent), host.runtime())
            .mount_with_site(mount);
    let typed = builder.build().unwrap();
    block_on(host.run(typed.ready())).unwrap().unwrap();
    let context = block_on(host.run(typed.acquire(&site)))
        .unwrap()
        .unwrap()
        .unwrap();
    let (complete_legacy, late_legacy) = controlled_future();
    let (complete_typed, late_typed) = controlled_future();
    let mut late_legacy = std::pin::pin!(late_legacy);
    let mut late_typed = std::pin::pin!(late_typed);
    let mut cx = Context::from_waker(Waker::noop());
    assert!(late_legacy.as_mut().poll(&mut cx).is_pending());
    assert!(late_typed.as_mut().poll(&mut cx).is_pending());
    for store in [&legacy, &typed] {
        let _ = block_on(host.run(store.send(ParentAction::Remove)))
            .unwrap()
            .unwrap();
        let _ = block_on(host.run(store.send(ParentAction::Restore)))
            .unwrap()
            .unwrap();
    }
    let value = Box::new(17);
    let address = std::ptr::from_ref(&*value);
    complete_legacy.complete(value).unwrap();
    complete_typed.complete(Box::new(17)).unwrap();
    let rejected = old.try_send(block_on(late_legacy).unwrap()).unwrap_err();
    assert!(rejected.is_scope_ended());
    assert_eq!(std::ptr::from_ref(&*rejected.into_action()), address);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let value = block_on(late_typed).unwrap();
    let rejected = block_on(host.run(context.update(move |child| {
        observed.fetch_add(1, Ordering::SeqCst);
        child.value += *value;
    })))
    .unwrap()
    .unwrap_err();
    assert_eq!(rejected.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(rejected.progress(), TaskProgress::NotStarted);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    for store in [&legacy, &typed] {
        assert_eq!(
            block_on(host.run(store.read(Clone::clone)))
                .unwrap()
                .unwrap(),
            Some(Child { id: 7, value: 100 })
        );
    }
    // Fresh authority remains usable: rejecting the old result is not closure
    // of the new equal-ID incarnation or of the root Store.
    let fresh_legacy = block_on(host.run(legacy.scope_some_identified(
        OptionalStatePath::new(
            |state: &Option<Child>| state.as_ref(),
            |state: &mut Option<Child>| state.as_mut(),
        ),
        ActionPath::new(
            |action: &ParentAction| match action {
                ParentAction::Child(value) => Some(value),
                _ => None,
            },
            ParentAction::Child,
        ),
        |child| &child.id,
    )))
    .unwrap()
    .unwrap();
    let _ = block_on(host.run(fresh_legacy.send(Box::new(1))))
        .unwrap()
        .unwrap();
    let fresh = block_on(host.run(typed.acquire(&site)))
        .unwrap()
        .unwrap()
        .unwrap();
    block_on(host.run(fresh.update(|child| child.value += 1)))
        .unwrap()
        .unwrap();
    for store in [&legacy, &typed] {
        assert_eq!(
            block_on(host.run(store.read(Clone::clone)))
                .unwrap()
                .unwrap(),
            Some(Child { id: 7, value: 101 })
        );
    }
}
