//! Exercises application-owned request, event and external-URL clients without real I/O.

use std::cell::Cell;
use std::future::poll_fn;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::{Arc, mpsc};

use futures_core::Stream;
use futures_executor::block_on;
use rca_core::{
    ActionPath, CaseStatePath, DependencyKey, Effect, EffectSender, KeyedAction, KeyedActionPath,
    KeyedState, KeyedStatePath, OptionalStatePath, ReduceContext, Reducer, ReducerExt, ScopedStore,
    Store,
};
use rca_test::{
    Completion, ControlCancelled, DeterministicRuntime, StreamControl, TryControlledSendError,
    controlled_future, controlled_stream,
};

/// An owned successful payload, deliberately neither `Clone`, `Debug` nor `Sync`.
struct Reply(Cell<u64>);

/// An owned domain denial with the same deliberately limited trait bounds.
struct Denied(Cell<u64>);

/// Separates a domain result from cancellation of its controlled source.
type RequestResult = Result<Reply, Denied>;

/// Owns one application request without borrowing reducer context.
type Response<T> = Pin<Box<dyn Future<Output = Result<T, ControlCancelled>> + Send>>;

/// Owns a bounded source of domain events and its terminal cancellation.
type Events = Pin<Box<dyn Stream<Item = Result<Reply, ControlCancelled>> + Send>>;

/// Application-owned endpoints, not a universal HTTP or URL framework.
struct Clients {
    request: Arc<dyn Fn(u8) -> Response<RequestResult> + Send + Sync>,
    open_url: Arc<dyn Fn(String) -> Response<Result<(), Denied>> + Send + Sync>,
    events: Arc<dyn Fn(u8) -> Events + Send + Sync>,
}

/// Injects the explicit controlled endpoints used by every scenario.
struct ClientsKey;
impl DependencyKey for ClientsKey {
    type Value = Clients;
}

/// Reports actual endpoint arguments and returns their external completion controls.
enum Call {
    Request(u8, Completion<RequestResult>),
    OpenUrl(String, Completion<Result<(), Denied>>),
    Subscribe(u8, StreamControl<Reply>),
}

/// Builds fresh endpoints with two buffered event slots and no real network access.
fn fixture() -> (Clients, mpsc::Receiver<Call>) {
    let (calls, received) = mpsc::channel();
    let requests = calls.clone();
    let urls = calls.clone();
    let client = Clients {
        request: Arc::new(move |feature| {
            let (completion, future) = controlled_future();
            requests.send(Call::Request(feature, completion)).unwrap();
            Box::pin(future)
        }),
        open_url: Arc::new(move |url| {
            let (completion, future) = controlled_future();
            urls.send(Call::OpenUrl(url, completion)).unwrap();
            Box::pin(future)
        }),
        events: Arc::new(move |feature| {
            let (control, stream) = controlled_stream(NonZeroUsize::new(2).unwrap());
            calls.send(Call::Subscribe(feature, control)).unwrap();
            Box::pin(stream)
        }),
    };
    (client, received)
}

/// Records domain results, source cancellation and graceful EOF as different outcomes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    Response(u64),
    RequestDenied(u64),
    UrlOpened,
    UrlDenied(u64),
    Event(u64),
    RequestCancelled,
    UrlCancelled,
    EventsCancelled,
    EventsClosed,
}

/// Holds one child's endpoint identity and its ordered, owned result history.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FeatureState {
    id: u8,
    outcomes: Vec<Outcome>,
}

/// Reuses identity 7 with an empty history to expose stale delivery after replacement.
fn initial() -> FeatureState {
    FeatureState {
        id: 7,
        outcomes: Vec::new(),
    }
}

/// Carries endpoint requests and owned replies without extra payload trait bounds.
enum FeatureAction {
    Request,
    OpenUrl(String),
    Subscribe,
    Response(Result<RequestResult, ControlCancelled>),
    UrlResult(Result<Result<(), Denied>, ControlCancelled>),
    Event(Result<Reply, ControlCancelled>),
    EventsClosed,
}

/// Converts application endpoint work into scoped effects and observable commits.
struct Feature {
    /// Retains historical senders so tests can try delivery after their owner ends.
    outputs: mpsc::Sender<EffectSender<FeatureAction>>,
}

impl Reducer for Feature {
    type State = FeatureState;
    type Action = FeatureAction;

    fn reduce(
        &mut self,
        state: &mut FeatureState,
        action: &FeatureAction,
        context: &mut ReduceContext<'_>,
    ) -> Effect<FeatureAction> {
        match action {
            FeatureAction::Request => {
                // Obtain owned work now; never capture the borrowed reducer context.
                let response = (context.dependency::<ClientsKey>().request)(state.id);
                let outputs = self.outputs.clone();
                return Effect::run(move |sender| async move {
                    outputs.send(sender.clone()).unwrap();
                    // Rejection ends this one-shot effect; never retry via root input.
                    let _ = sender.send(FeatureAction::Response(response.await)).await;
                });
            }
            FeatureAction::OpenUrl(url) => {
                let response = (context.dependency::<ClientsKey>().open_url)(url.clone());
                return Effect::run(move |sender| async move {
                    // Rejection ends this one-shot effect; never retry via root input.
                    let _ = sender.send(FeatureAction::UrlResult(response.await)).await;
                });
            }
            FeatureAction::Subscribe => {
                let mut events = (context.dependency::<ClientsKey>().events)(state.id);
                let outputs = self.outputs.clone();
                return Effect::run(move |sender| async move {
                    outputs.send(sender.clone()).unwrap();
                    while let Some(event) = poll_fn(|cx| events.as_mut().poll_next(cx)).await {
                        let cancelled = event.is_err();
                        if sender.send(FeatureAction::Event(event)).await.is_err() || cancelled {
                            return;
                        }
                    }
                    // EOF output is terminal, including when its authority has ended.
                    let _ = sender.send(FeatureAction::EventsClosed).await;
                });
            }
            FeatureAction::Response(result) => state.outcomes.push(match result {
                Ok(Ok(reply)) => Outcome::Response(reply.0.get()),
                Ok(Err(denied)) => Outcome::RequestDenied(denied.0.get()),
                Err(ControlCancelled) => Outcome::RequestCancelled,
            }),
            FeatureAction::UrlResult(result) => state.outcomes.push(match result {
                Ok(Ok(())) => Outcome::UrlOpened,
                Ok(Err(denied)) => Outcome::UrlDenied(denied.0.get()),
                Err(ControlCancelled) => Outcome::UrlCancelled,
            }),
            FeatureAction::Event(result) => state.outcomes.push(match result {
                Ok(reply) => Outcome::Event(reply.0.get()),
                Err(ControlCancelled) => Outcome::EventsCancelled,
            }),
            FeatureAction::EventsClosed => state.outcomes.push(Outcome::EventsClosed),
        }
        Effect::none()
    }
}

/// Builds a child with a separate channel for observing its output authority.
fn feature() -> (Feature, mpsc::Receiver<EffectSender<FeatureAction>>) {
    let (outputs, received) = mpsc::channel();
    (Feature { outputs }, received)
}

// Catches wrong request arguments, swallowed URL denial, reordered/lost events and overflow.
#[test]
fn root_clients_preserve_results_order_backpressure_and_graceful_source_shutdown() {
    let host = DeterministicRuntime::new();
    let (client, calls) = fixture();
    let (feature, _outputs) = feature();
    let store = Store::builder(initial(), feature, host.runtime())
        .with_dependency::<ClientsKey>(client)
        .build()
        .unwrap();
    let driver = host.clone();
    block_on(driver.run(async move {
        let request = store.send(FeatureAction::Request).await.unwrap();
        let Call::Request(7, completion) = calls.try_recv().unwrap() else {
            panic!("selected request");
        };
        completion.complete(Ok(Reply(Cell::new(11)))).unwrap();
        request.finish().await;
        let url = store
            .send(FeatureAction::OpenUrl(String::from(
                "https://example.invalid/help",
            )))
            .await
            .unwrap();
        let Call::OpenUrl(target, completion) = calls.try_recv().unwrap() else {
            panic!("URL request");
        };
        assert_eq!(target, "https://example.invalid/help");
        completion.complete(Err(Denied(Cell::new(403)))).unwrap();
        url.finish().await;
        let subscription = store.send(FeatureAction::Subscribe).await.unwrap();
        let Call::Subscribe(7, events) = calls.try_recv().unwrap() else {
            panic!("selected subscription");
        };
        // The deterministic effect has not consumed anything yet: two slots are full.
        events.try_send(Reply(Cell::new(23))).unwrap();
        events.try_send(Reply(Cell::new(31))).unwrap();
        let overflow = events.try_send(Reply(Cell::new(47))).unwrap_err();
        assert!(matches!(&overflow, TryControlledSendError::Full(_)));
        assert_eq!(overflow.into_inner().0.get(), 47);
        events.close();
        subscription.finish().await;
        assert_eq!(
            store.read(|s| s.outcomes.clone()).await.unwrap(),
            vec![
                Outcome::Response(11),
                Outcome::UrlDenied(403),
                Outcome::Event(23),
                Outcome::Event(31),
                Outcome::EventsClosed,
            ]
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Catches conflating source cancellation with a denied domain operation or graceful EOF.
#[test]
fn root_clients_report_denial_and_control_cancellation_separately() {
    let host = DeterministicRuntime::new();
    let (client, calls) = fixture();
    let (feature, _outputs) = feature();
    let store = Store::builder(initial(), feature, host.runtime())
        .with_dependency::<ClientsKey>(client)
        .build()
        .unwrap();
    let driver = host.clone();
    block_on(driver.run(async move {
        let task = store.send(FeatureAction::Request).await.unwrap();
        let Call::Request(7, completion) = calls.try_recv().unwrap() else {
            panic!("request");
        };
        completion.complete(Err(Denied(Cell::new(401)))).unwrap();
        task.finish().await;
        let task = store
            .send(FeatureAction::OpenUrl(String::from(
                "https://example.invalid/allowed",
            )))
            .await
            .unwrap();
        let Call::OpenUrl(target, completion) = calls.try_recv().unwrap() else {
            panic!("allowed URL");
        };
        assert_eq!(target, "https://example.invalid/allowed");
        completion.complete(Ok(())).unwrap();
        task.finish().await;
        let task = store.send(FeatureAction::Request).await.unwrap();
        let Call::Request(7, completion) = calls.try_recv().unwrap() else {
            panic!("request");
        };
        completion.cancel();
        task.finish().await;
        let task = store
            .send(FeatureAction::OpenUrl(String::from(
                "https://example.invalid",
            )))
            .await
            .unwrap();
        let Call::OpenUrl(_, completion) = calls.try_recv().unwrap() else {
            panic!("URL");
        };
        completion.cancel();
        task.finish().await;
        let task = store.send(FeatureAction::Subscribe).await.unwrap();
        let Call::Subscribe(7, events) = calls.try_recv().unwrap() else {
            panic!("events");
        };
        events.cancel();
        task.finish().await;
        assert_eq!(
            store.read(|s| s.outcomes.clone()).await.unwrap(),
            vec![
                Outcome::RequestDenied(401),
                Outcome::UrlOpened,
                Outcome::RequestCancelled,
                Outcome::UrlCancelled,
                Outcome::EventsCancelled,
            ]
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

/// Selects the three composition families checked with the same replacement trace.
#[derive(Clone, Copy)]
enum Family {
    Optional,
    Case,
    Keyed,
}

/// Supplies a case-owned child and an explicit absent case.
enum Screen {
    Off,
    On(FeatureState),
}

/// Keeps all families present so untouched siblings can be checked independently.
struct ParentState {
    optional: Option<FeatureState>,
    screen: Screen,
    rows: KeyedState<u8, FeatureState>,
    root_inputs: usize,
}

/// Routes child work and performs explicit removal, replacement and root input.
enum ParentAction {
    Optional(FeatureAction),
    Case(FeatureAction),
    Row(KeyedAction<u8, FeatureAction>),
    Remove(Family),
    Insert(Family),
    RootInput,
}

/// Changes child lifetimes without synthesising endpoint results.
struct Parent;
impl Reducer for Parent {
    type State = ParentState;
    type Action = ParentAction;
    fn reduce(
        &mut self,
        state: &mut ParentState,
        action: &ParentAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ParentAction> {
        match action {
            ParentAction::Remove(Family::Optional) => state.optional = None,
            ParentAction::Remove(Family::Case) => state.screen = Screen::Off,
            ParentAction::Remove(Family::Keyed) => {
                state.rows.remove(&7);
            }
            ParentAction::Insert(Family::Optional) => state.optional = Some(initial()),
            ParentAction::Insert(Family::Case) => state.screen = Screen::On(initial()),
            ParentAction::Insert(Family::Keyed) => state.rows.insert(7, initial()),
            ParentAction::RootInput => state.root_inputs += 1,
            _ => {}
        }
        Effect::none()
    }
}

// Named function-pointer shapes keep the concrete path signatures readable.
// Each alias below names the corresponding borrow or owned action conversion.
type ReadChild = fn(&ParentState) -> Option<&FeatureState>;
type MutateChild = fn(&mut ParentState) -> Option<&mut FeatureState>;
type ExtractChild = fn(&ParentAction) -> Option<&FeatureAction>;
type EmbedChild = fn(FeatureAction) -> ParentAction;
type ReadRows = fn(&ParentState) -> &KeyedState<u8, FeatureState>;
type MutateRows = fn(&mut ParentState) -> &mut KeyedState<u8, FeatureState>;
type ExtractRow = fn(&ParentAction) -> Option<&KeyedAction<u8, FeatureAction>>;
type EmbedRow = fn(KeyedAction<u8, FeatureAction>) -> ParentAction;

/// Borrows only the optional child for reduction and scoped reads.
fn optional_state() -> OptionalStatePath<ParentState, FeatureState, ReadChild, MutateChild> {
    OptionalStatePath::new(
        |s: &ParentState| s.optional.as_ref(),
        |s: &mut ParentState| s.optional.as_mut(),
    )
}

/// Routes only actions addressed to the optional child.
fn optional_action() -> ActionPath<ParentAction, FeatureAction, ExtractChild, EmbedChild> {
    ActionPath::new(
        |a| match a {
            ParentAction::Optional(a) => Some(a),
            _ => None,
        },
        ParentAction::Optional,
    )
}

/// Borrows the case child only while the parent remains in its `On` case.
fn case_state() -> CaseStatePath<ParentState, FeatureState, ReadChild, MutateChild> {
    CaseStatePath::new(
        |s| match &s.screen {
            Screen::On(s) => Some(s),
            Screen::Off => None,
        },
        |s| match &mut s.screen {
            Screen::On(s) => Some(s),
            Screen::Off => None,
        },
    )
}

/// Routes only actions addressed to the case child.
fn case_action() -> ActionPath<ParentAction, FeatureAction, ExtractChild, EmbedChild> {
    ActionPath::new(
        |a| match a {
            ParentAction::Case(a) => Some(a),
            _ => None,
        },
        ParentAction::Case,
    )
}

/// Borrows the keyed collection without conflating keys and insertion lifetimes.
fn keyed_state() -> KeyedStatePath<ParentState, u8, FeatureState, ReadRows, MutateRows> {
    KeyedStatePath::new(|s| &s.rows, |s| &mut s.rows)
}

/// Preserves the row key while routing an owned child action.
fn keyed_action() -> KeyedActionPath<ParentAction, u8, FeatureAction, ExtractRow, EmbedRow> {
    KeyedActionPath::new(
        |a| match a {
            ParentAction::Row(a) => Some(a),
            _ => None,
        },
        ParentAction::Row,
    )
}

/// Captures the selected child's current lifetime through the public Store API.
async fn scope(
    store: &Store<ParentState, ParentAction>,
    family: Family,
) -> ScopedStore<FeatureState, FeatureAction> {
    match family {
        Family::Optional => store
            .scope_some(optional_state(), optional_action())
            .await
            .unwrap(),
        Family::Case => store.scope_case(case_state(), case_action()).await.unwrap(),
        Family::Keyed => store
            .scope_keyed(keyed_state(), keyed_action(), 7)
            .await
            .unwrap(),
    }
}

/// Checks old work, same-identity replacement, new work and untouched siblings in order.
#[expect(
    clippy::too_many_lines,
    reason = "keeps historical work and same-identity replacement in causal order"
)]
fn check_owned_child(family: Family) {
    let host = DeterministicRuntime::new();
    let (client, calls) = fixture();
    let (child, outputs) = feature();
    let optional = Feature {
        outputs: child.outputs.clone(),
    };
    let case = Feature {
        outputs: child.outputs.clone(),
    };
    let reducer = Parent
        .if_some(optional_state(), optional_action(), optional)
        .if_case(case_state(), case_action(), case)
        .for_each(keyed_state(), keyed_action(), child);
    let store = Store::builder(
        ParentState {
            optional: Some(initial()),
            screen: Screen::On(initial()),
            rows: [(7, initial())].into_iter().collect(),
            root_inputs: 0,
        },
        reducer,
        host.runtime(),
    )
    .with_dependency::<ClientsKey>(client)
    .build()
    .unwrap();
    let driver = host.clone();
    block_on(driver.run(async move {
        let old = scope(&store, family).await;
        let request = old.send(FeatureAction::Request).await.unwrap();
        let Call::Request(7, completion) = calls.try_recv().unwrap() else {
            panic!("old request");
        };
        let url = old
            .send(FeatureAction::OpenUrl(String::from(
                "https://example.invalid/old",
            )))
            .await
            .unwrap();
        let Call::OpenUrl(target, url_completion) = calls.try_recv().unwrap() else {
            panic!("old URL");
        };
        assert_eq!(target, "https://example.invalid/old");
        let subscription = old.send(FeatureAction::Subscribe).await.unwrap();
        let Call::Subscribe(7, events) = calls.try_recv().unwrap() else {
            panic!("old events");
        };
        host.run_until_stalled().unwrap();
        let stale_request = outputs.try_recv().unwrap();
        let stale_events = outputs.try_recv().unwrap();
        events.try_send(Reply(Cell::new(11))).unwrap();
        host.run_until_stalled().unwrap();
        assert_eq!(
            old.read(|s| s.outcomes.clone()).await.unwrap(),
            vec![Outcome::Event(11)]
        );
        store
            .send(ParentAction::Remove(family))
            .await
            .unwrap()
            .finish()
            .await;
        host.run_until_stalled().unwrap();
        request.finish().await;
        url.finish().await;
        subscription.finish().await;
        store
            .send(ParentAction::Insert(family))
            .await
            .unwrap()
            .finish()
            .await;
        let fresh = scope(&store, family).await;
        assert!(old.read(|s| s.id).await.unwrap_err().is_scope_ended());
        let rejected = completion
            .complete(Err(Denied(Cell::new(403))))
            .unwrap_err()
            .into_inner();
        let Err(denied) = rejected else {
            panic!("late domain denial retained");
        };
        assert_eq!(denied.0.get(), 403);
        let rejected = url_completion
            .complete(Err(Denied(Cell::new(451))))
            .unwrap_err()
            .into_inner();
        let Err(denied) = rejected else {
            panic!("late URL denial retained");
        };
        assert_eq!(denied.0.get(), 451);
        let rejected = events.try_send(Reply(Cell::new(23))).unwrap_err();
        assert!(matches!(
            &rejected,
            TryControlledSendError::ReceiverDropped(_)
        ));
        assert_eq!(rejected.into_inner().0.get(), 23);
        assert!(
            stale_request
                .send(FeatureAction::Response(Ok(Ok(Reply(Cell::new(31))))))
                .await
                .unwrap_err()
                .is_cancelled()
        );
        assert!(
            stale_events
                .send(FeatureAction::Event(Ok(Reply(Cell::new(47)))))
                .await
                .unwrap_err()
                .is_cancelled()
        );
        assert!(fresh.read(|s| s.outcomes.is_empty()).await.unwrap());
        // Fresh root input remains authorised; removal revokes only historical scoped work.
        store
            .send(ParentAction::RootInput)
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(store.read(|s| s.root_inputs).await.unwrap(), 1);
        let task = fresh.send(FeatureAction::Request).await.unwrap();
        let Call::Request(7, completion) = calls.try_recv().unwrap() else {
            panic!("fresh request");
        };
        completion.complete(Ok(Reply(Cell::new(53)))).unwrap();
        task.finish().await;
        let task = fresh
            .send(FeatureAction::OpenUrl(String::from(
                "https://example.invalid/fresh",
            )))
            .await
            .unwrap();
        let Call::OpenUrl(target, completion) = calls.try_recv().unwrap() else {
            panic!("fresh URL");
        };
        assert_eq!(target, "https://example.invalid/fresh");
        completion.complete(Ok(())).unwrap();
        task.finish().await;
        let task = fresh.send(FeatureAction::Subscribe).await.unwrap();
        let Call::Subscribe(7, events) = calls.try_recv().unwrap() else {
            panic!("fresh events");
        };
        events.try_send(Reply(Cell::new(59))).unwrap();
        events.close();
        task.finish().await;
        assert_eq!(
            fresh.read(|s| s.outcomes.clone()).await.unwrap(),
            vec![
                Outcome::Response(53),
                Outcome::UrlOpened,
                Outcome::Event(59),
                Outcome::EventsClosed
            ]
        );
        assert!(
            store
                .read(move |state| match family {
                    Family::Optional =>
                        matches!(&state.screen, Screen::On(s) if s.outcomes.is_empty())
                            && state.rows.get(&7).unwrap().outcomes.is_empty(),
                    Family::Case =>
                        state.optional.as_ref().unwrap().outcomes.is_empty()
                            && state.rows.get(&7).unwrap().outcomes.is_empty(),
                    Family::Keyed =>
                        state.optional.as_ref().unwrap().outcomes.is_empty()
                            && matches!(&state.screen, Screen::On(s) if s.outcomes.is_empty()),
                })
                .await
                .unwrap()
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Missing lifetime fencing would route late client output into a replacement child.
#[test]
fn optional_client_work_cannot_reach_replacement() {
    check_owned_child(Family::Optional);
}
#[test]
fn case_client_work_cannot_reach_replacement() {
    check_owned_child(Family::Case);
}
#[test]
fn keyed_client_work_cannot_reach_same_id_replacement() {
    check_owned_child(Family::Keyed);
}

// Catches retaining pending endpoint receivers after Store shutdown.
#[test]
fn shutdown_rejects_pending_client_completion_subscription_and_output() {
    let host = DeterministicRuntime::new();
    let (client, calls) = fixture();
    let (feature, outputs) = feature();
    let store = Store::builder(initial(), feature, host.runtime())
        .with_dependency::<ClientsKey>(client)
        .build()
        .unwrap();
    let driver = host.clone();
    block_on(driver.run(async move {
        let request = store.send(FeatureAction::Request).await.unwrap();
        let Call::Request(7, completion) = calls.try_recv().unwrap() else {
            panic!("request");
        };
        let url = store
            .send(FeatureAction::OpenUrl(String::from(
                "https://example.invalid/pending",
            )))
            .await
            .unwrap();
        let Call::OpenUrl(target, url_completion) = calls.try_recv().unwrap() else {
            panic!("pending URL");
        };
        assert_eq!(target, "https://example.invalid/pending");
        let subscription = store.send(FeatureAction::Subscribe).await.unwrap();
        let Call::Subscribe(7, events) = calls.try_recv().unwrap() else {
            panic!("subscription");
        };
        host.run_until_stalled().unwrap();
        let output = outputs.try_recv().unwrap();
        store.shutdown().await.unwrap();
        // Shutdown revokes output immediately; executor progress drops suspended input work.
        host.run_until_stalled().unwrap();
        request.finish().await;
        url.finish().await;
        subscription.finish().await;
        let rejected = completion
            .complete(Ok(Reply(Cell::new(11))))
            .unwrap_err()
            .into_inner();
        let Ok(reply) = rejected else {
            panic!("shutdown preserves the rejected domain reply");
        };
        assert_eq!(reply.0.get(), 11);
        assert!(
            url_completion
                .complete(Ok(()))
                .unwrap_err()
                .into_inner()
                .is_ok()
        );
        let rejected = events.try_send(Reply(Cell::new(23))).unwrap_err();
        assert!(matches!(
            &rejected,
            TryControlledSendError::ReceiverDropped(_)
        ));
        assert_eq!(rejected.into_inner().0.get(), 23);
        assert!(output.send(FeatureAction::EventsClosed).await.is_err());
    }))
    .unwrap();
}
