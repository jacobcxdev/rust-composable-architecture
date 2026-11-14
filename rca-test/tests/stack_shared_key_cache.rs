//! Cached settings outlive stack entries without renewing permission to send output.

use std::convert::Infallible;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};

use futures_executor::block_on;
use rca_core::{
    ActionPath, Effect, EffectSender, ReduceContext, Reducer, ScopedStore, SharedKey,
    SharedKeyCache, SharedKeyIdentity, SharedLoadContext, SharedReader, SharedReaderKey,
    SharedSubscribeContext, SharedSubscriber, StackAction, StackId, StackState, StatePath, Store,
    stack,
};
use rca_test::DeterministicRuntime;

/// Loads shared settings with one subscription whose release the test can observe.
struct SettingsKey(Arc<AtomicUsize>);

/// Counts release of the real cache entry's backend resource.
struct Subscription(Arc<AtomicUsize>);
impl Drop for Subscription {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl SharedReaderKey for SettingsKey {
    type Value = u32;
    type Error = Infallible;
    type Subscription = Subscription;

    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<u32>("stack-settings", "v1")
    }

    fn load(
        &self,
        _: SharedLoadContext<'_, u32>,
    ) -> impl Future<Output = Result<Option<u32>, Infallible>> + Send {
        std::future::ready(Ok(None))
    }

    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<u32>,
    ) -> Result<Subscription, Infallible> {
        Ok(Subscription(Arc::clone(&self.0)))
    }
}
impl SharedKey for SettingsKey {
    fn save(&self, _: &u32) -> impl Future<Output = Result<(), Infallible>> + Send {
        std::future::ready(Ok(()))
    }
}

/// Captures an effect sender or applies one independently supplied value.
enum ChildAction {
    Start(mpsc::Sender<EffectSender<ChildAction>>),
    Set(u32),
}

/// Routes stack actions or replaces an entry while retaining its logical ID.
enum Action {
    Path(StackAction<u32, ChildAction>),
    Recreate(StackId),
}

/// Recreates the selected entry without changing its value.
struct Parent;
impl Reducer for Parent {
    type State = StackState<u32>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if let Action::Recreate(id) = action {
            let equal_value = *state.get(id).unwrap();
            state.try_pop_from(id).unwrap();
            state
                .try_push(id.clone(), equal_value)
                .unwrap_or_else(|_| panic!("removed ID is reusable"));
        }
        Effect::none()
    }
}

/// Keeps settings alive without owning the lifetime of any stack entry.
struct Client {
    settings: SharedReader<u32>,
}
impl Reducer for Client {
    type State = u32;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut u32,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Set(value) => {
                *state = *value;
                Effect::none()
            }
            ChildAction::Start(captured) => {
                let captured = captured.clone();
                let settings = self.settings.clone();
                Effect::run(move |sender| async move {
                    sender
                        .send(ChildAction::Set(settings.read(|value| *value).unwrap()))
                        .await
                        .unwrap();
                    captured.send(sender).unwrap();
                    std::future::pending::<()>().await;
                })
            }
        }
    }
}

/// Separates stack actions from the parent's same-ID replacement operation.
fn extract(action: &Action) -> Option<&StackAction<u32, ChildAction>> {
    match action {
        Action::Path(action) => Some(action),
        Action::Recreate(_) => None,
    }
}

/// Acquires the currently installed entry, not an earlier use of its ID.
async fn scope(
    store: &Store<StackState<u32>, Action>,
    id: StackId,
) -> ScopedStore<u32, ChildAction> {
    store
        .scope_stack(
            StatePath::new(|state: &StackState<u32>| state, |state| state),
            ActionPath::new(extract, Action::Path),
            id,
        )
        .await
        .unwrap()
}

// Sharing a settings owner must neither revoke siblings nor renew retired structural output.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps same-ID retirement and actual last-resource release in one causal trace"
)]
fn cached_settings_outlive_equal_stack_reinsertion_without_renewing_old_authority() {
    let runtime = DeterministicRuntime::new();
    let cache = SharedKeyCache::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let settings = block_on(cache.shared(SettingsKey(Arc::clone(&drops)), || 7)).unwrap();
    let retained = settings.reader().unwrap();
    let mut state = StackState::new();
    let sibling_id = state.push(0);
    let selected_id = state.push(0);
    let reducer = stack(
        Parent,
        StatePath::new(|state: &StackState<u32>| state, |state| state),
        ActionPath::new(extract, Action::Path),
        |value: &u32, _: &mut ReduceContext<'_>| *value,
        Client {
            settings: retained.clone(),
        },
    );
    let store = Store::new(state, reducer, runtime.runtime());
    let driver = runtime.clone();
    let in_task = settings.clone();
    let task_drops = Arc::clone(&drops);
    block_on(driver.run(async move {
        let sibling = scope(&store, sibling_id).await;
        let old = scope(&store, selected_id.clone()).await;
        let (selected_capture, selected_output) = mpsc::channel();
        let old_work = old
            .send(ChildAction::Start(selected_capture))
            .await
            .unwrap();
        let (sibling_capture, sibling_output) = mpsc::channel();
        let sibling_work = sibling
            .send(ChildAction::Start(sibling_capture))
            .await
            .unwrap();
        runtime.run_until_stalled().unwrap();
        let retired_sender = selected_output.try_recv().unwrap();
        let live_sibling = sibling_output.try_recv().unwrap();
        let mut old_observation = old.observe(|value| *value).await.unwrap();
        assert_eq!(old_observation.next().await, Some(7));
        assert_eq!(sibling.read(|value| *value).await.unwrap(), 7);

        // Settings-only mutation does not change either structural generation.
        in_task
            .try_update(
                |value| {
                    *value = 9;
                    Ok::<_, ()>(())
                },
                || {},
            )
            .unwrap();
        retired_sender.send(ChildAction::Set(7)).await.unwrap();
        live_sibling.send(ChildAction::Set(11)).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(old.read(|value| *value).await.unwrap(), 7);
        assert_eq!(sibling.read(|value| *value).await.unwrap(), 11);

        store
            .send(Action::Recreate(selected_id.clone()))
            .await
            .unwrap()
            .finish()
            .await;
        runtime.run_until_stalled().unwrap();
        old_work.finish().await;
        assert!(
            retired_sender
                .send(ChildAction::Set(99))
                .await
                .unwrap_err()
                .is_cancelled()
        );
        assert!(old.read(|value| *value).await.unwrap_err().is_scope_ended());
        assert_eq!(old_observation.next().await, None);
        assert!(!sibling_work.is_finished());
        assert_eq!(task_drops.load(Ordering::SeqCst), 0);

        let fresh = scope(&store, selected_id).await;
        assert_eq!(fresh.read(|value| *value).await.unwrap(), 7);
        let (fresh_capture, fresh_output) = mpsc::channel();
        let fresh_work = fresh.send(ChildAction::Start(fresh_capture)).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(fresh.read(|value| *value).await.unwrap(), 9);
        fresh_output
            .try_recv()
            .unwrap()
            .send(ChildAction::Set(13))
            .await
            .unwrap();
        live_sibling.send(ChildAction::Set(12)).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(fresh.read(|value| *value).await.unwrap(), 13);
        assert_eq!(sibling.read(|value| *value).await.unwrap(), 12);
        store.shutdown().await.unwrap();
        fresh_work.finish().await;
        sibling_work.finish().await;
    }))
    .unwrap();
    drop(driver);
    drop(cache);
    drop(settings);
    assert_eq!(retained.read(|value| *value).unwrap(), 9);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(retained);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
