//! Effect checks separate timer readiness from permission to publish an output.
//!
//! Closing a release channel opens the second gate only after cancellation or
//! replacement commits. Slow hosts therefore cannot race a wall-clock sleep.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use async_channel::Receiver;
use rca_core::{DependencyKey, Effect, ReduceContext, Reducer, Store};

use super::RuntimeFactory;

/// Exercises delayed execution without using elapsed time as a race barrier.
const EFFECT_DELAY: Duration = Duration::from_millis(4);

/// Drives independently asserted ordering, cancellation and capture scenarios.
#[derive(Clone)]
enum Action {
    /// Starts two outputs whose relative order is unspecified.
    Parallel,
    /// Registers equal-valued cancellation IDs with different Rust types.
    StartTyped(Receiver<()>),
    /// Cancels only the byte-typed ID.
    CancelU8,
    /// Registers the task that a later action will replace.
    StartOld(Receiver<()>),
    /// Replaces the old task before either output gate opens.
    StartReplacement(Receiver<()>),
    /// Registers then cancels a queued output in one reduction.
    StartStale,
    /// Registers two distinct cancellation lineages behind one gate.
    StartUnrelated(Receiver<()>),
    /// Cancels one of the two unrelated lineages.
    CancelLeft,
    /// Captures a dependency and emits it after a fixed first output.
    Ordered,
    /// Cancels an ID before starting fresh work with that ID.
    CancelThenStart,
    /// Starts a causal child action carrying the output gate.
    StartTree(Receiver<()>),
    /// Records child entry and creates its own delayed descendant.
    Descendant(Receiver<()>),
    /// Samples externally mutable dependency state during reduction.
    StartCaptured(Receiver<()>),
    /// Samples the dependency again while replacing the old delayed task.
    ReplaceCaptured(Receiver<()>),
    /// Records only outputs that survived admission and authority checks.
    Output(u8),
}

/// Supplies a fixed dependency for source-order checks.
struct CapturedValue;

impl DependencyKey for CapturedValue {
    type Value = u8;
}

/// Exposes later mutation so the check can distinguish capture from late lookup.
struct DynamicCapturedValue;

impl DependencyKey for DynamicCapturedValue {
    type Value = Arc<AtomicU8>;
}

/// Builds the scenario's effects and records their delivered outputs.
struct Effects;

/// Delays execution and then holds output until the scenario closes its gate.
///
/// Real time may elapse before cancellation commits; channel closure, not the
/// delay, establishes when output may become runnable.
fn delayed_output(value: u8, release: &Receiver<()>) -> Effect<Action> {
    let release = release.clone();
    Effect::run(move |sender| async move {
        release
            .recv()
            .await
            .expect_err("release closes without a value");
        sender
            .send(Action::Output(value))
            .await
            .expect("effect output");
    })
    .delay(EFFECT_DELAY)
}

impl Reducer for Effects {
    type State = Vec<u8>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Parallel => Effect::merge([
                Effect::run(|sender| async move {
                    sender.send(Action::Output(1)).await.expect("effect output");
                }),
                Effect::run(|sender| async move {
                    sender.send(Action::Output(2)).await.expect("effect output");
                }),
            ]),
            Action::StartTyped(release) => Effect::merge([
                delayed_output(8, release).cancellable(1_u8),
                delayed_output(16, release).cancellable(1_u16),
            ]),
            Action::CancelU8 => Effect::cancel(1_u8),
            Action::StartOld(release) => delayed_output(1, release).cancellable(9_u64),
            Action::StartReplacement(release) => {
                delayed_output(2, release).cancellable_in_flight(9_u64)
            }
            Action::StartStale => Effect::merge([
                Effect::send(Action::Output(42)).cancellable(3_u64),
                Effect::cancel(3_u64),
            ]),
            Action::StartUnrelated(release) => Effect::merge([
                delayed_output(3, release).cancellable("left"),
                delayed_output(4, release).cancellable("right"),
            ]),
            Action::CancelLeft => Effect::cancel("left"),
            Action::Ordered => {
                let captured = *context.dependency::<CapturedValue>();
                Effect::concatenate([
                    Effect::send(Action::Output(5)),
                    Effect::send(Action::Output(captured)),
                ])
            }
            Action::CancelThenStart => Effect::merge([
                Effect::cancel(4_u64),
                Effect::send(Action::Output(7)).cancellable(4_u64),
            ]),
            Action::StartTree(release) => Effect::send(Action::Descendant(release.clone())),
            Action::Descendant(release) => {
                state.push(30);
                delayed_output(31, release)
            }
            Action::StartCaptured(release) => {
                let captured = context
                    .dependency::<DynamicCapturedValue>()
                    .load(Ordering::Acquire);
                delayed_output(captured, release).cancellable(5_u64)
            }
            Action::ReplaceCaptured(release) => {
                let captured = context
                    .dependency::<DynamicCapturedValue>()
                    .load(Ordering::Acquire);
                delayed_output(captured, release).cancellable_in_flight(5_u64)
            }
            Action::Output(value) => {
                state.push(*value);
                Effect::none()
            }
        }
    }
}

/// Checks both merged outputs without imposing an order on parallel work.
pub(super) async fn parallel_effect_return(factory: impl RuntimeFactory) {
    let store = Store::new(Vec::new(), Effects, factory.runtime());

    store
        .send(Action::Parallel)
        .await
        .expect("parallel start")
        .finish()
        .await;

    let mut values = store.read(Clone::clone).await.expect("effect state");
    values.sort_unstable();
    assert_eq!(values, [1, 2]);
    store.shutdown().await.expect("clean shutdown");
}

/// Checks that cancellation ID type participates in lineage identity.
pub(super) async fn typed_cancellation(factory: impl RuntimeFactory) {
    let (release, ready) = async_channel::bounded(1);
    let store = Store::new(Vec::new(), Effects, factory.runtime());
    let typed = store
        .send(Action::StartTyped(ready))
        .await
        .expect("typed start");
    store
        .send(Action::CancelU8)
        .await
        .expect("typed cancellation")
        .finish()
        .await;
    drop(release);
    factory
        .advance(EFFECT_DELAY)
        .await
        .expect("advance typed timer");
    typed.finish().await;

    assert_eq!(store.read(Clone::clone).await.expect("effect state"), [16]);
    store.shutdown().await.expect("clean shutdown");
}

/// Waits for old work to finish before allowing the replacement to publish.
pub(super) async fn in_flight_replacement(factory: impl RuntimeFactory) {
    let (release, ready) = async_channel::bounded(1);
    let store = Store::new(Vec::new(), Effects, factory.runtime());
    let old = store
        .send(Action::StartOld(ready.clone()))
        .await
        .expect("old start");
    let replacement = store
        .send(Action::StartReplacement(ready))
        .await
        .expect("replacement start");
    old.finish().await;
    drop(release);
    factory
        .advance(EFFECT_DELAY)
        .await
        .expect("advance replacement timer");
    replacement.finish().await;

    assert_eq!(store.read(Clone::clone).await.expect("effect state"), [2]);
    store.shutdown().await.expect("clean shutdown");
}

/// Checks cancellation of a queued output before that output can reduce.
pub(super) async fn stale_output_rejection(factory: impl RuntimeFactory) {
    let store = Store::new(Vec::new(), Effects, factory.runtime());

    store
        .send(Action::StartStale)
        .await
        .expect("stale start")
        .finish()
        .await;

    assert!(store.read(Vec::is_empty).await.expect("effect state"));
    store.shutdown().await.expect("clean shutdown");
}

/// Checks that cancelling one lineage leaves another lineage able to finish.
pub(super) async fn unrelated_lineages(factory: impl RuntimeFactory) {
    let (release, ready) = async_channel::bounded(1);
    let store = Store::new(Vec::new(), Effects, factory.runtime());
    let task = store
        .send(Action::StartUnrelated(ready))
        .await
        .expect("lineages start");
    store
        .send(Action::CancelLeft)
        .await
        .expect("cancel left")
        .finish()
        .await;
    drop(release);
    factory
        .advance(EFFECT_DELAY)
        .await
        .expect("advance unrelated timer");
    task.finish().await;

    assert_eq!(store.read(Clone::clone).await.expect("effect state"), [4]);
    store.shutdown().await.expect("clean shutdown");
}

/// Checks concatenation, reduction-time capture and cancel-before-start ordering.
pub(super) async fn source_order_and_dependency_capture(factory: impl RuntimeFactory) {
    let store = Store::builder(Vec::new(), Effects, factory.runtime())
        .with_dependency::<CapturedValue>(9)
        .build()
        .expect("dependency Store");

    store
        .send(Action::Ordered)
        .await
        .expect("ordered start")
        .finish()
        .await;
    store
        .send(Action::CancelThenStart)
        .await
        .expect("cancel-before-start order")
        .finish()
        .await;

    assert_eq!(
        store.read(Clone::clone).await.expect("effect state"),
        [5, 9, 7]
    );
    store.shutdown().await.expect("clean shutdown");
}

/// Waits for child entry, then checks that root cancellation reaches its descendant.
pub(super) async fn parent_and_descendant_cancellation(factory: impl RuntimeFactory) {
    let (release, ready) = async_channel::bounded(1);
    let store = Store::new(Vec::new(), Effects, factory.runtime());
    let mut states = store
        .observe(|state| state.contains(&30))
        .await
        .expect("descendant observation");
    assert_eq!(states.next().await, Some(false));
    let root = store
        .send(Action::StartTree(ready))
        .await
        .expect("tree start");
    while !states.next().await.expect("live descendant observation") {}

    root.cancel();
    root.finish().await;
    drop(release);
    factory
        .advance(EFFECT_DELAY)
        .await
        .expect("advance cancelled descendant");

    assert!(root.is_cancelled());
    assert_eq!(store.read(Clone::clone).await.expect("tree state"), [30]);
    drop(states);
    store.shutdown().await.expect("clean shutdown");
}

/// Distinguishes replacement-time capture from both the old and later values.
pub(super) async fn delayed_replacement_dependency_capture(factory: impl RuntimeFactory) {
    let (release, ready) = async_channel::bounded(1);
    let captured = Arc::new(AtomicU8::new(2));
    let store = Store::builder(Vec::new(), Effects, factory.runtime())
        .with_dependency::<DynamicCapturedValue>(Arc::clone(&captured))
        .build()
        .expect("dependency Store");
    let old = store
        .send(Action::StartCaptured(ready.clone()))
        .await
        .expect("captured start");
    captured.store(8, Ordering::Release);
    let replacement = store
        .send(Action::ReplaceCaptured(ready))
        .await
        .expect("captured replacement");
    old.finish().await;
    // Construction captured 8; later external mutation must not change output.
    captured.store(16, Ordering::Release);
    drop(release);
    factory
        .advance(EFFECT_DELAY)
        .await
        .expect("advance captured replacement");
    replacement.finish().await;

    assert_eq!(store.read(Clone::clone).await.expect("effect state"), [8]);
    store.shutdown().await.expect("clean shutdown");
}
