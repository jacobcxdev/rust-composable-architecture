//! Checks generated clients across cancellation and replacement of an optional child.

#[expect(
    dead_code,
    reason = "only actor spawning and captured effect polling are used"
)]
mod support;

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rca_core::{
    ActionPath, Effect, EffectSender, OptionalStatePath, ReduceContext, Reducer, Store, if_some,
};
use rca_macros::DependencyClient;

/// Counts release of a non-cloneable endpoint error.
struct Failure(Arc<AtomicUsize>);

impl Drop for Failure {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Carries an owned typed error through the asynchronous client endpoint.
type Response = Pin<Box<dyn Future<Output = Result<usize, Failure>> + Send>>;

/// Mixes an ordinary member, a configured sync endpoint and an overridden async endpoint.
#[derive(Clone, DependencyClient)]
struct Client {
    /// Remains ordinary member data in the generated test value.
    #[dependency(member = String::from("mixed"))]
    label: String,
    /// Supplies the literal child state before asynchronous work begins.
    #[dependency(test = Arc::new(|| 7))]
    sync: Arc<dyn Fn() -> usize + Send + Sync>,
    /// Produces a caller-polled future with an owned error.
    load: Arc<dyn Fn() -> Response + Send + Sync>,
}

/// Separates request admission from an owned asynchronous response.
enum ChildAction {
    /// Calls both generated client endpoints.
    Start,
    /// Carries either the endpoint value or its non-cloneable error.
    Response(Result<usize, Failure>),
}

/// Replaces the optional child while keeping its old effect authority available.
enum Action {
    /// Routes work into the current optional child.
    Child(ChildAction),
    /// Cancels work belonging to the current child lifetime.
    Remove,
    /// Creates a replacement child with fresh state.
    Insert,
}

/// Captures an effect sender before the endpoint future becomes pending.
struct Child {
    /// Supplies generated defaults and the manually controlled asynchronous endpoint.
    client: Client,
    /// Retains old output authority separately from the effect future.
    sender: Arc<Mutex<Option<EffectSender<ChildAction>>>>,
}

impl Reducer for Child {
    type State = usize;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut usize,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Start => {
                *state = (self.client.sync)();
                let client = self.client.clone();
                let sender = Arc::clone(&self.sender);
                Effect::run(move |send| async move {
                    *sender.lock().unwrap() = Some(send.clone());
                    let result = (client.load)().await;
                    let _ = send.send(ChildAction::Response(result)).await;
                })
            }
            ChildAction::Response(result) => {
                *state = result.as_ref().copied().unwrap_or(99);
                Effect::none()
            }
        }
    }
}

/// Removes and replaces children without processing their responses itself.
struct Parent;

impl Reducer for Parent {
    type State = Option<usize>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Option<usize>,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Remove => *state = None,
            Action::Insert => *state = Some(0),
            Action::Child(_) => {}
        }
        Effect::none()
    }
}

/// Checks error disposal and stale output rejection after actual child cancellation.
#[test]
fn cancelled_mixed_client_future_drops_input_and_rejects_stale_scoped_output() {
    let ready = Arc::new(AtomicBool::new(false));
    let drops = Arc::new(AtomicUsize::new(0));
    let gate = Arc::clone(&ready);
    let owned_drops = Arc::clone(&drops);
    let client = Client {
        load: Arc::new(move || {
            let gate = Arc::clone(&gate);
            let mut failure = Some(Failure(Arc::clone(&owned_drops)));
            // The test polls this pending future directly, then cancels it.
            // Only a fresh future is created after the gate becomes ready.
            Box::pin(std::future::poll_fn(move |_| {
                if gate.load(Ordering::SeqCst) {
                    Poll::Ready(Err(failure.take().unwrap()))
                } else {
                    Poll::Pending
                }
            }))
        }),
        ..Client::test_value()
    };
    assert_eq!(client.label, "mixed");
    let sender = Arc::new(Mutex::new(None));
    let reducer = if_some(
        Parent,
        OptionalStatePath::new(
            |state: &Option<usize>| state.as_ref(),
            |state: &mut Option<usize>| state.as_mut(),
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::Child(action) => Some(action),
                _ => None,
            },
            Action::Child,
        ),
        Child {
            client: client.clone(),
            sender: Arc::clone(&sender),
        },
    );
    let runtime = support::ManualRuntime::new();
    let store = Store::new(Some(0), reducer, runtime.runtime());
    runtime.start_all();
    futures_executor::block_on(async {
        let task = store.send(Action::Child(ChildAction::Start)).await.unwrap();
        let mut effect = runtime.take_only_queued_task();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(effect.as_mut().poll(&mut cx).is_pending());
        assert_eq!(store.read(|state| *state).await.unwrap(), Some(7));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        store.send(Action::Remove).await.unwrap().finish().await;
        assert!(effect.as_mut().poll(&mut cx).is_ready());
        task.finish().await;
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        store.send(Action::Insert).await.unwrap().finish().await;

        // Even independently completed work cannot use the cancelled scope's sender.
        ready.store(true, Ordering::SeqCst);
        let stale_result = (client.load)().await;
        let stale_sender = sender.lock().unwrap().take().unwrap();
        let rejected = stale_sender
            .send(ChildAction::Response(stale_result))
            .await
            .unwrap_err();
        assert!(rejected.is_cancelled());
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert_eq!(store.read(|state| *state).await.unwrap(), Some(0));
        store.shutdown().await.unwrap();
    });
    for result in runtime.join_all() {
        result.expect("actor thread must finish without panic");
    }
    drop(store);
    drop(client);
    assert_eq!(
        Arc::strong_count(&ready),
        1,
        "endpoint captures must be released"
    );
    assert_eq!(
        Arc::strong_count(&drops),
        1,
        "error and endpoint owners must be released"
    );
}
