//! Checks effect construction, task ownership and ordered cancellation boundaries.

#![expect(
    clippy::duplicate_mod,
    reason = "independently reusable test hosts each own the same private clock module"
)]

#[cfg(not(miri))]
#[path = "../src/allocation_test/child.rs"]
mod child;

#[path = "support/thread_runtime.rs"]
mod support;

#[expect(dead_code, reason = "sequence tests manually poll runtime tasks")]
#[path = "support/mod.rs"]
mod manual_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use rca_core::{
    ActionPath, ActionTask, Effect, EffectSender, OptionalStatePath, Reduce, ReduceContext,
    Reducer, Store, if_some,
};

use support::ThreadRuntime;

/// An owned action that intentionally supplies neither `Clone` nor `Debug`.
struct MoveOnly {
    _value: String,
}

#[test]
fn effect_language_accepts_move_only_actions_and_generators() {
    let effects = Effect::merge([
        Effect::send(MoveOnly {
            _value: String::from("immediate"),
        }),
        Effect::send_after(
            Duration::from_millis(5),
            MoveOnly {
                _value: String::from("delayed"),
            },
        ),
        Effect::interval(Duration::from_millis(10), || MoveOnly {
            _value: String::from("tick"),
        }),
    ])
    .cancellable_in_flight(7_u64);

    assert!(format!("{effects:?}").starts_with("Effect::CancellableInFlight"));
}

#[test]
fn effect_construction_maps_non_send_outputs_before_shared_delivery() {
    use std::rc::Rc;

    use rca_core::{MonotonicInstant, OperationPolicy};

    #[cfg(not(miri))]
    if child::run_in_child("effect_construction_maps_non_send_outputs_before_shared_delivery") {
        return;
    }

    let factories = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&factories);
    let immediate = Rc::new(1_usize);
    let retained = Rc::downgrade(&immediate);
    let effect = Effect::concatenate([
        Effect::send(immediate),
        Effect::future(async { Some(Rc::new(2)) }),
        Effect::run(move |_| async move {
            called.fetch_add(1, Ordering::Relaxed);
        }),
        Effect::send_after(Duration::ZERO, Rc::new(3)),
        Effect::send_at(MonotonicInstant::ZERO, Rc::new(4)),
        Effect::cancel("unrelated"),
    ])
    .delay(Duration::ZERO)
    .cancellable("ordinary")
    .cancellable_in_flight("replacement")
    .without_inherited_cancellation()
    .named("named", OperationPolicy::Coexist)
    .map(|value| *value);
    assert!(
        retained.upgrade().is_none(),
        "immediate mapping consumes Rc"
    );
    assert_eq!(factories.load(Ordering::Relaxed), 0);

    // The generator remains Send; only its completed output is local.
    let interval = Effect::interval(Duration::from_secs(1), || Rc::new(5_usize)).map(|v| *v);
    assert_eq!(format!("{interval:?}"), "Effect::Interval");
    drop(interval);

    let mut initial = Some(effect);
    let reducer = Reduce::new(move |state: &mut Vec<usize>, action: &usize, _| {
        state.push(*action);
        initial.take().unwrap_or_default()
    });
    let runtime = ThreadRuntime::new();
    let store = Store::new(Vec::new(), reducer, runtime.runtime());
    futures_executor::block_on(async {
        store.send(0).await.expect("start commit").finish().await;
        assert_eq!(store.read(Clone::clone).await.unwrap(), [0, 1, 2, 3, 4]);
        assert_eq!(factories.load(Ordering::Relaxed), 1);
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn effect_construction_keeps_borrowed_actions_local() {
    use rca_core::{MonotonicInstant, OperationPolicy};

    /// Constructs local effects without extending the borrowed action's lifetime.
    fn borrowed(value: &str) -> Effect<&str> {
        Effect::concatenate([
            Effect::send(value),
            Effect::send_after(Duration::from_secs(1), value),
            Effect::send_at(MonotonicInstant::MAX, value),
            Effect::cancel("unrelated"),
        ])
        .delay(Duration::from_secs(1))
        .cancellable("ordinary")
        .cancellable_in_flight("replacement")
        .without_inherited_cancellation()
        .named("named", OperationPolicy::Coexist)
    }

    let mut value = String::from("local action");
    let effect = borrowed(&value);
    assert_eq!(format!("{effect:?}"), "Effect::Named");
    drop(effect);
    value.clear();
    assert!(value.is_empty());
}

#[test]
fn effect_construction_accepts_fallible_non_send_domains() {
    use std::rc::Rc;

    use rca_core::{EffectError, Issue, IssueReporter};

    #[cfg(not(miri))]
    if child::run_in_child("effect_construction_accepts_fallible_non_send_domains") {
        return;
    }

    let handled = Arc::new(AtomicUsize::new(0));
    let reported = Arc::clone(&handled);
    let recovered = Arc::clone(&handled);
    let effect = Effect::merge([
        Effect::<Rc<usize>>::try_run(
            |_| async { Err(EffectError::Failed(Rc::new(6_usize))) },
            move |cause, _| {
                recovered.fetch_add(*cause, Ordering::Relaxed);
                std::future::ready(())
            },
        ),
        Effect::<Rc<usize>>::try_run_reported(
            |_| async { Err(EffectError::Failed(Rc::new(7_usize))) },
            IssueReporter::default(),
            |_| Issue::new("effect.test", "retained typed failure"),
            move |report| {
                let cause = report.result.expect_err("the operation failed");
                reported.fetch_add(*cause, Ordering::Relaxed);
                assert_eq!(report.deliveries.len(), 1);
            },
        ),
    ])
    .map(|value| *value);

    let mut initial = Some(effect);
    let reducer = Reduce::new(move |state: &mut Vec<usize>, action: &usize, _| {
        state.push(*action);
        initial.take().unwrap_or_default()
    });
    let runtime = ThreadRuntime::new();
    let store = Store::new(Vec::new(), reducer, runtime.runtime());
    futures_executor::block_on(async {
        store.send(0).await.expect("start commit").finish().await;
        assert_eq!(store.read(Clone::clone).await.unwrap(), [0]);
        assert_eq!(handled.load(Ordering::Relaxed), 13);
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn effect_debug_output_omits_domain_payloads() {
    let effect = Effect::concatenate([
        Effect::send(MoveOnly {
            _value: String::from("private action"),
        }),
        Effect::cancel(9_u64),
        Effect::run(|_sender| async {}),
    ]);

    let debug = format!("{effect:?}");
    assert_eq!(debug, "Effect::Concatenate(3)");
    assert!(!debug.contains("private action"));
}

#[test]
#[should_panic(expected = "interval period must be non-zero")]
fn zero_period_panics() {
    let _ = Effect::<()>::interval(Duration::ZERO, || ());
}

/// Gives each generation in the expected causal action log a distinct value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TreeAction {
    Start,
    Child,
    Grandchild,
}

/// Emits a child and grandchild without starting unrelated work.
struct Tree;

impl Reducer for Tree {
    type State = Vec<TreeAction>;
    type Action = TreeAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.push(*action);
        match action {
            TreeAction::Start => Effect::run(|sender| async move {
                sender.send(TreeAction::Child).await.expect("child commit");
            }),
            TreeAction::Child => Effect::send(TreeAction::Grandchild),
            TreeAction::Grandchild => Effect::none(),
        }
    }
}

#[test]
fn action_task_finishes_after_descendant_effects_commit() {
    #[cfg(not(miri))]
    if child::run_in_child("action_task_finishes_after_descendant_effects_commit") {
        return;
    }

    let runtime = ThreadRuntime::new();
    let store = Store::new(Vec::new(), Tree, runtime.runtime());

    futures_executor::block_on(async {
        let task = store.send(TreeAction::Start).await.expect("start commit");
        task.finish().await;

        assert!(task.is_finished());
        assert_eq!(
            store.read(Clone::clone).await.expect("ordered read"),
            [TreeAction::Start, TreeAction::Child, TreeAction::Grandchild]
        );
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// Keeps one root task unfinished until the test releases its asynchronous gate.
struct GateReducer {
    gate: async_channel::Receiver<()>,
}

impl Reducer for GateReducer {
    type State = ();
    type Action = ();

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let gate = self.gate.clone();
        Effect::run(move |_sender| async move {
            let _ = gate.recv().await;
        })
    }
}

#[test]
fn cancelling_one_finish_wait_does_not_change_lineage() {
    #[cfg(not(miri))]
    if child::run_in_child("cancelling_one_finish_wait_does_not_change_lineage") {
        return;
    }

    let (release, gate) = async_channel::bounded(1);
    let runtime = ThreadRuntime::new();
    let store = Store::new((), GateReducer { gate }, runtime.runtime());

    futures_executor::block_on(async {
        let task = store.send(()).await.expect("start commit");
        let mut first_wait = Box::pin(task.finish());
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(matches!(
            std::future::Future::poll(first_wait.as_mut(), &mut context),
            Poll::Pending
        ));
        drop(first_wait);

        assert!(!task.is_cancelled());
        release.send(()).await.expect("release effect");
        task.finish().await;
        assert!(task.is_finished());
        task.cancel();
        assert!(!task.is_cancelled());
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// Separates a sender's owning action from forbidden output after completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SenderAction {
    Start,
    Late,
}

/// Exports one sender, then lets its owning effect finish immediately.
struct CaptureSender {
    capture: Option<async_channel::Sender<EffectSender<SenderAction>>>,
}

impl Reducer for CaptureSender {
    type State = Vec<SenderAction>;
    type Action = SenderAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.push(*action);
        match action {
            SenderAction::Start => {
                let capture = self.capture.take().expect("capture used once");
                Effect::run(move |sender| async move {
                    capture.send(sender).await.expect("capture sender");
                })
            }
            SenderAction::Late => Effect::none(),
        }
    }
}

#[test]
fn sender_rejects_actions_after_owning_task_completes() {
    #[cfg(not(miri))]
    if child::run_in_child("sender_rejects_actions_after_owning_task_completes") {
        return;
    }

    let (capture, captured) = async_channel::bounded(1);
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        Vec::new(),
        CaptureSender {
            capture: Some(capture),
        },
        runtime.runtime(),
    );

    futures_executor::block_on(async {
        let task = store.send(SenderAction::Start).await.expect("start commit");
        let sender = captured.recv().await.expect("captured sender");
        task.finish().await;

        let error = sender
            .send(SenderAction::Late)
            .await
            .expect_err("completed sender must reject output");
        assert!(error.is_cancelled());
        assert_eq!(
            store.read(Clone::clone).await.expect("ordered read"),
            [SenderAction::Start]
        );
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// Selects one of two unrelated root tasks.
#[derive(Clone, Copy)]
enum GateAction {
    First,
    Second,
}

/// Allows one root to remain pending while the other is cancelled.
struct IndependentGates {
    first: async_channel::Receiver<()>,
    second: async_channel::Receiver<()>,
}

impl Reducer for IndependentGates {
    type State = ();
    type Action = GateAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let gate = match action {
            GateAction::First => self.first.clone(),
            GateAction::Second => self.second.clone(),
        };
        Effect::run(move |_sender| async move {
            let _ = gate.recv().await;
        })
    }
}

#[test]
fn root_cancel_does_not_cancel_unrelated_lineage() {
    #[cfg(not(miri))]
    if child::run_in_child("root_cancel_does_not_cancel_unrelated_lineage") {
        return;
    }

    let (_release_first, first_gate) = async_channel::bounded(1);
    let (release_second, second_gate) = async_channel::bounded(1);
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        (),
        IndependentGates {
            first: first_gate,
            second: second_gate,
        },
        runtime.runtime(),
    );

    futures_executor::block_on(async {
        let first = store.send(GateAction::First).await.expect("first commit");
        let second = store.send(GateAction::Second).await.expect("second commit");
        first.cancel();
        first.finish().await;

        assert!(first.is_cancelled());
        assert!(!second.is_cancelled());
        assert!(!second.is_finished());

        release_second.send(()).await.expect("release second");
        second.finish().await;
        assert!(second.is_finished());
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// Distinguishes cancellation identity types, generations and visible output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CancellationAction {
    StartU8,
    StartU16,
    StartShared,
    StartOld,
    StartReplacement,
    StartStale,
    CancelU8,
    CancelShared,
    Output(u8),
}

/// Holds each generation behind its own gate so cancellation order is observable.
struct CancellationReducer {
    u8_gate: async_channel::Receiver<()>,
    u16_gate: async_channel::Receiver<()>,
    shared_gates: std::collections::VecDeque<async_channel::Receiver<()>>,
    replacement_gates: std::collections::VecDeque<async_channel::Receiver<()>>,
}

impl CancellationReducer {
    /// Emits only after a successful release, not when the test drops its sender.
    fn gated_output(gate: async_channel::Receiver<()>, value: u8) -> Effect<CancellationAction> {
        Effect::run(move |sender| async move {
            if gate.recv().await.is_ok() {
                let _ = sender.send(CancellationAction::Output(value)).await;
            }
        })
    }
}

impl Reducer for CancellationReducer {
    type State = Vec<u8>;
    type Action = CancellationAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            CancellationAction::StartU8 => {
                Self::gated_output(self.u8_gate.clone(), 8).cancellable(1_u8)
            }
            CancellationAction::StartU16 => {
                Self::gated_output(self.u16_gate.clone(), 16).cancellable(1_u16)
            }
            CancellationAction::StartShared => {
                Self::gated_output(self.shared_gates.pop_front().expect("shared gate"), 7)
                    .cancellable(7_u64)
            }
            CancellationAction::StartOld => Self::gated_output(
                self.replacement_gates
                    .pop_front()
                    .expect("old replacement gate"),
                1,
            )
            .cancellable(9_u64),
            CancellationAction::StartReplacement => Self::gated_output(
                self.replacement_gates
                    .pop_front()
                    .expect("new replacement gate"),
                2,
            )
            .cancellable_in_flight(9_u64),
            CancellationAction::StartStale => Effect::merge([
                Effect::send(CancellationAction::Output(42)).cancellable(3_u64),
                Effect::cancel(3_u64),
            ]),
            CancellationAction::CancelU8 => Effect::cancel(1_u8),
            CancellationAction::CancelShared => Effect::cancel(7_u64),
            CancellationAction::Output(value) => {
                state.push(*value);
                Effect::none()
            }
        }
    }
}

/// Keeps every cancellation gate alive until the Store has shut down.
struct CancellationFixture {
    store: Store<Vec<u8>, CancellationAction>,
    runtime: ThreadRuntime,
    _release_u8: async_channel::Sender<()>,
    release_u16: async_channel::Sender<()>,
    _release_shared: Vec<async_channel::Sender<()>>,
    release_replacement: Vec<async_channel::Sender<()>>,
}

impl CancellationFixture {
    /// Supplies independent gates for typed identities and replacement generations.
    fn new() -> Self {
        let (release_u8, u8_gate) = async_channel::bounded(1);
        let (release_u16, u16_gate) = async_channel::bounded(1);
        let (release_shared_1, shared_gate_1) = async_channel::bounded(1);
        let (release_shared_2, shared_gate_2) = async_channel::bounded(1);
        let (release_old, old_gate) = async_channel::bounded(1);
        let (release_new, new_gate) = async_channel::bounded(1);
        let runtime = ThreadRuntime::new();
        let store = Store::new(
            Vec::new(),
            CancellationReducer {
                u8_gate,
                u16_gate,
                shared_gates: [shared_gate_1, shared_gate_2].into(),
                replacement_gates: [old_gate, new_gate].into(),
            },
            runtime.runtime(),
        );
        Self {
            store,
            runtime,
            _release_u8: release_u8,
            release_u16,
            _release_shared: vec![release_shared_1, release_shared_2],
            release_replacement: vec![release_old, release_new],
        }
    }

    /// Joins the runtime after the caller has completed Store shutdown.
    fn finish(self) {
        self.runtime.join_all();
    }
}

#[test]
fn equal_cancellation_values_of_different_types_are_isolated() {
    #[cfg(not(miri))]
    if child::run_in_child("equal_cancellation_values_of_different_types_are_isolated") {
        return;
    }

    let fixture = CancellationFixture::new();

    futures_executor::block_on(async {
        let first = fixture
            .store
            .send(CancellationAction::StartU8)
            .await
            .expect("u8 start");
        let second = fixture
            .store
            .send(CancellationAction::StartU16)
            .await
            .expect("u16 start");
        fixture
            .store
            .send(CancellationAction::CancelU8)
            .await
            .expect("u8 cancel")
            .finish()
            .await;
        fixture.release_u16.send(()).await.expect("release u16");
        first.finish().await;
        second.finish().await;

        assert_eq!(fixture.store.read(Clone::clone).await.expect("state"), [16]);
        fixture.store.shutdown().await.expect("clean shutdown");
    });
    fixture.finish();
}

#[test]
fn cancellable_keeps_multiple_generations_live_until_cancelled() {
    #[cfg(not(miri))]
    if child::run_in_child("cancellable_keeps_multiple_generations_live_until_cancelled") {
        return;
    }

    let fixture = CancellationFixture::new();

    futures_executor::block_on(async {
        let first = fixture
            .store
            .send(CancellationAction::StartShared)
            .await
            .expect("first start");
        let second = fixture
            .store
            .send(CancellationAction::StartShared)
            .await
            .expect("second start");
        fixture
            .store
            .send(CancellationAction::CancelShared)
            .await
            .expect("shared cancel")
            .finish()
            .await;
        first.finish().await;
        second.finish().await;

        assert!(
            fixture
                .store
                .read(Clone::clone)
                .await
                .expect("state")
                .is_empty()
        );
        fixture.store.shutdown().await.expect("clean shutdown");
    });
    fixture.finish();
}

#[test]
fn cancellable_in_flight_replaces_all_old_generations() {
    #[cfg(not(miri))]
    if child::run_in_child("cancellable_in_flight_replaces_all_old_generations") {
        return;
    }

    let fixture = CancellationFixture::new();

    futures_executor::block_on(async {
        let old = fixture
            .store
            .send(CancellationAction::StartOld)
            .await
            .expect("old start");
        let replacement = fixture
            .store
            .send(CancellationAction::StartReplacement)
            .await
            .expect("replacement start");
        fixture.release_replacement[1]
            .send(())
            .await
            .expect("release replacement");
        old.finish().await;
        replacement.finish().await;

        assert_eq!(fixture.store.read(Clone::clone).await.expect("state"), [2]);
        fixture.store.shutdown().await.expect("clean shutdown");
    });
    fixture.finish();
}

#[test]
fn cancellation_rejects_output_registered_before_launch() {
    #[cfg(not(miri))]
    if child::run_in_child("cancellation_rejects_output_registered_before_launch") {
        return;
    }

    let fixture = CancellationFixture::new();

    futures_executor::block_on(async {
        fixture
            .store
            .send(CancellationAction::StartStale)
            .await
            .expect("stale start")
            .finish()
            .await;

        assert!(
            fixture
                .store
                .read(Clone::clone)
                .await
                .expect("state")
                .is_empty()
        );
        fixture.store.shutdown().await.expect("clean shutdown");
    });
    fixture.finish();
}

/// Keeps sequence and merge cancellation identities distinct.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum StructuredKey {
    Sequence,
    Left,
    Right,
    Before,
    After,
}

/// Gives each sequence or merge position a distinct expected log entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StructuredAction {
    StartSequence,
    StartEmptySequence,
    CancelNestedSequence,
    StartMerge,
    BeforeRegistration,
    AfterRegistration,
    CancelSequence,
    CancelLeft,
    First,
    Inner,
    Suffix,
    Left,
    Right,
    BeforeOutput,
    AfterOutput,
}

/// Combines cancellation with sequential and concurrent child work.
struct StructuredReducer {
    sequence: async_channel::Receiver<()>,
    left: async_channel::Receiver<()>,
    right: async_channel::Receiver<()>,
}

impl StructuredReducer {
    /// Releases one selected child without starting its siblings or suffix.
    fn gated(
        gate: async_channel::Receiver<()>,
        output: StructuredAction,
    ) -> Effect<StructuredAction> {
        Effect::run(move |sender| async move {
            if gate.recv().await.is_ok() {
                let _ = sender.send(output).await;
            }
        })
    }
}

impl Reducer for StructuredReducer {
    type State = Vec<StructuredAction>;
    type Action = StructuredAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.push(*action);
        match action {
            StructuredAction::StartSequence => Effect::concatenate([
                Self::gated(self.sequence.clone(), StructuredAction::First)
                    .cancellable(StructuredKey::Sequence),
                Effect::send(StructuredAction::Suffix),
            ]),
            StructuredAction::StartEmptySequence => Effect::concatenate([
                Effect::concatenate([]),
                Effect::send(StructuredAction::First),
                Effect::concatenate([]),
                Effect::send(StructuredAction::Suffix),
            ]),
            StructuredAction::CancelNestedSequence => Effect::concatenate([
                Effect::merge([
                    Effect::cancel(StructuredKey::Sequence),
                    Effect::concatenate([Effect::send(StructuredAction::Inner)]),
                ]),
                Effect::send(StructuredAction::Suffix),
            ])
            .cancellable(StructuredKey::Sequence),
            StructuredAction::StartMerge => Effect::merge([
                Self::gated(self.left.clone(), StructuredAction::Left)
                    .cancellable(StructuredKey::Left),
                Self::gated(self.right.clone(), StructuredAction::Right)
                    .cancellable(StructuredKey::Right),
            ]),
            StructuredAction::BeforeRegistration => Effect::merge([
                Effect::cancel(StructuredKey::Before),
                Effect::send(StructuredAction::BeforeOutput).cancellable(StructuredKey::Before),
            ]),
            StructuredAction::AfterRegistration => Effect::merge([
                Effect::send(StructuredAction::AfterOutput).cancellable(StructuredKey::After),
                Effect::cancel(StructuredKey::After),
            ]),
            StructuredAction::CancelSequence => Effect::cancel(StructuredKey::Sequence),
            StructuredAction::CancelLeft => Effect::cancel(StructuredKey::Left),
            StructuredAction::First
            | StructuredAction::Inner
            | StructuredAction::Suffix
            | StructuredAction::Left
            | StructuredAction::Right
            | StructuredAction::BeforeOutput
            | StructuredAction::AfterOutput => Effect::none(),
        }
    }
}

#[test]
fn cancelled_concatenate_child_never_starts_suffix() {
    #[cfg(not(miri))]
    if child::run_in_child("cancelled_concatenate_child_never_starts_suffix") {
        return;
    }

    let (release_sequence, sequence_gate) = async_channel::bounded(1);
    let (_release_left, left_gate) = async_channel::bounded(1);
    let (_release_right, right_gate) = async_channel::bounded(1);
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        Vec::new(),
        StructuredReducer {
            sequence: sequence_gate,
            left: left_gate,
            right: right_gate,
        },
        runtime.runtime(),
    );

    futures_executor::block_on(async {
        let sequence = store
            .send(StructuredAction::StartSequence)
            .await
            .expect("sequence start");
        store
            .send(StructuredAction::CancelSequence)
            .await
            .expect("sequence cancel")
            .finish()
            .await;
        release_sequence.send(()).await.expect("release sequence");
        sequence.finish().await;

        assert_eq!(
            store.read(Clone::clone).await.expect("state"),
            [
                StructuredAction::StartSequence,
                StructuredAction::CancelSequence,
            ]
        );
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn cancelling_keyed_merge_child_leaves_sibling_live() {
    #[cfg(not(miri))]
    if child::run_in_child("cancelling_keyed_merge_child_leaves_sibling_live") {
        return;
    }

    let (_release_sequence, sequence_gate) = async_channel::bounded(1);
    let (_release_left, left_gate) = async_channel::bounded(1);
    let (release_right, right_gate) = async_channel::bounded(1);
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        Vec::new(),
        StructuredReducer {
            sequence: sequence_gate,
            left: left_gate,
            right: right_gate,
        },
        runtime.runtime(),
    );

    futures_executor::block_on(async {
        let merged = store
            .send(StructuredAction::StartMerge)
            .await
            .expect("merge start");
        store
            .send(StructuredAction::CancelLeft)
            .await
            .expect("left cancel")
            .finish()
            .await;
        release_right.send(()).await.expect("release right");
        merged.finish().await;

        assert_eq!(
            store.read(Clone::clone).await.expect("state"),
            [
                StructuredAction::StartMerge,
                StructuredAction::CancelLeft,
                StructuredAction::Right,
            ]
        );
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn cancellation_and_registration_follow_source_order() {
    #[cfg(not(miri))]
    if child::run_in_child("cancellation_and_registration_follow_source_order") {
        return;
    }

    let (_release_sequence, sequence_gate) = async_channel::bounded(1);
    let (_release_left, left_gate) = async_channel::bounded(1);
    let (_release_right, right_gate) = async_channel::bounded(1);
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        Vec::new(),
        StructuredReducer {
            sequence: sequence_gate,
            left: left_gate,
            right: right_gate,
        },
        runtime.runtime(),
    );

    futures_executor::block_on(async {
        store
            .send(StructuredAction::BeforeRegistration)
            .await
            .expect("before start")
            .finish()
            .await;
        store
            .send(StructuredAction::AfterRegistration)
            .await
            .expect("after start")
            .finish()
            .await;

        assert_eq!(
            store.read(Clone::clone).await.expect("state"),
            [
                StructuredAction::BeforeRegistration,
                StructuredAction::BeforeOutput,
                StructuredAction::AfterRegistration,
            ]
        );
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn nested_sequences_finish_after_empty_or_cancelled_children() {
    for (start, expected) in [
        (
            StructuredAction::StartEmptySequence,
            vec![
                StructuredAction::StartEmptySequence,
                StructuredAction::First,
                StructuredAction::Suffix,
            ],
        ),
        (
            StructuredAction::CancelNestedSequence,
            vec![StructuredAction::CancelNestedSequence],
        ),
    ] {
        let (release_sequence, sequence_gate) = async_channel::bounded(1);
        let (release_left, left_gate) = async_channel::bounded(1);
        let (release_right, right_gate) = async_channel::bounded(1);
        let runtime = manual_support::ManualRuntime::new();
        let store = Store::new(
            Vec::new(),
            StructuredReducer {
                sequence: sequence_gate,
                left: left_gate,
                right: right_gate,
            },
            runtime.runtime(),
        );
        let mut actor = runtime.take_only_queued_task();
        let receipt = store.try_send(start).expect("nested sequence admission");
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let mut receipt = std::pin::pin!(receipt.finish());
        let Poll::Ready(Ok(sequence)) = receipt.as_mut().poll(&mut cx) else {
            panic!("nested sequence action must commit in the first actor poll");
        };

        // Empty children must not obstruct either real send. The cancelled
        // nested sequence must launch no work, including Inner or Suffix.
        let send_count = if start == StructuredAction::StartEmptySequence {
            2
        } else {
            0
        };
        for _ in 0..send_count {
            let mut send = runtime.take_only_queued_task();
            assert!(send.as_mut().poll(&mut cx).is_pending());
            assert!(actor.as_mut().poll(&mut cx).is_pending());
            assert!(send.as_mut().poll(&mut cx).is_ready());
            assert!(actor.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!(runtime.spawn_count(), 1 + send_count);
        assert!(sequence.is_finished());
        let mut finished = std::pin::pin!(sequence.finish());
        assert!(finished.as_mut().poll(&mut cx).is_ready());

        let mut shutdown = std::pin::pin!(store.shutdown_into_state());
        assert!(shutdown.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_ready());
        let Poll::Ready(Ok(state)) = shutdown.as_mut().poll(&mut cx) else {
            panic!("completed actor must acknowledge state extraction");
        };
        assert_eq!(state, expected);
        assert!(release_sequence.is_closed());
        assert!(release_left.is_closed());
        assert!(release_right.is_closed());
    }
}

/// Reports each child's entry before waiting for that child's separate release.
struct OrderedSequence {
    first_started: async_channel::Sender<()>,
    second_started: async_channel::Sender<()>,
    first_gate: async_channel::Receiver<()>,
    second_gate: async_channel::Receiver<()>,
}

impl Reducer for OrderedSequence {
    type State = ();
    type Action = ();

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let first_started = self.first_started.clone();
        let first_gate = self.first_gate.clone();
        let second_started = self.second_started.clone();
        let second_gate = self.second_gate.clone();
        Effect::concatenate([
            Effect::run(move |_sender| async move {
                first_started.send(()).await.expect("first start signal");
                let _ = first_gate.recv().await;
            }),
            Effect::run(move |_sender| async move {
                second_started.send(()).await.expect("second start signal");
                let _ = second_gate.recv().await;
            }),
        ])
    }
}

#[test]
fn concatenate_starts_only_one_child_at_a_time() {
    #[cfg(not(miri))]
    if child::run_in_child("concatenate_starts_only_one_child_at_a_time") {
        return;
    }

    let (first_started, first_start) = async_channel::bounded(1);
    let (second_started, second_start) = async_channel::bounded(1);
    let (release_first, first_gate) = async_channel::bounded(1);
    let (release_second, second_gate) = async_channel::bounded(1);
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        (),
        OrderedSequence {
            first_started,
            second_started,
            first_gate,
            second_gate,
        },
        runtime.runtime(),
    );

    futures_executor::block_on(async {
        let sequence = store.send(()).await.expect("sequence commit");
        first_start.recv().await.expect("first child started");
        assert!(
            second_start.try_recv().is_err(),
            "second child must remain unstarted"
        );

        release_first.send(()).await.expect("release first");
        second_start.recv().await.expect("second child started");
        release_second.send(()).await.expect("release second");
        sequence.finish().await;
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// Keeps both merge children pending so root cancellation must retire both.
struct RootMerge {
    first: async_channel::Receiver<()>,
    second: async_channel::Receiver<()>,
}

impl Reducer for RootMerge {
    type State = ();
    type Action = ();

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let first = self.first.clone();
        let second = self.second.clone();
        Effect::merge([
            Effect::run(move |_sender| async move {
                let _ = first.recv().await;
            }),
            Effect::run(move |_sender| async move {
                let _ = second.recv().await;
            }),
        ])
    }
}

#[test]
fn cancelling_merge_root_cancels_every_child() {
    #[cfg(not(miri))]
    if child::run_in_child("cancelling_merge_root_cancels_every_child") {
        return;
    }

    let (_release_first, first) = async_channel::bounded(1);
    let (_release_second, second) = async_channel::bounded(1);
    let runtime = ThreadRuntime::new();
    let store = Store::new((), RootMerge { first, second }, runtime.runtime());

    futures_executor::block_on(async {
        let merged = store.send(()).await.expect("merge commit");
        merged.cancel();
        merged.finish().await;

        assert!(merged.is_cancelled());
        assert!(merged.is_finished());
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// Separates the old generation, its replacement and replacement output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SequenceReplacementAction {
    StartOld,
    StartReplacement,
    Output,
}

/// Replaces a blocked sequence with another generation of the same identity.
struct SequenceReplacement {
    old: async_channel::Receiver<()>,
    replacement: async_channel::Receiver<()>,
}

impl Reducer for SequenceReplacement {
    type State = Vec<SequenceReplacementAction>;
    type Action = SequenceReplacementAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            SequenceReplacementAction::StartOld => {
                let old = self.old.clone();
                Effect::concatenate([Effect::run(move |_sender| async move {
                    let _ = old.recv().await;
                })])
                .cancellable(71_u64)
            }
            SequenceReplacementAction::StartReplacement => {
                let replacement = self.replacement.clone();
                Effect::run(move |sender| async move {
                    if replacement.recv().await.is_ok() {
                        let _ = sender.send(SequenceReplacementAction::Output).await;
                    }
                })
                .cancellable_in_flight(71_u64)
            }
            SequenceReplacementAction::Output => {
                state.push(*action);
                Effect::none()
            }
        }
    }
}

#[test]
fn stale_sequence_completion_cannot_release_replacement_generation() {
    #[cfg(not(miri))]
    if child::run_in_child("stale_sequence_completion_cannot_release_replacement_generation") {
        return;
    }

    let (_release_old, old) = async_channel::bounded(1);
    let (release_replacement, replacement) = async_channel::bounded(1);
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        Vec::new(),
        SequenceReplacement { old, replacement },
        runtime.runtime(),
    );

    futures_executor::block_on(async {
        let old = store
            .send(SequenceReplacementAction::StartOld)
            .await
            .expect("old sequence commit");
        let replacement = store
            .send(SequenceReplacementAction::StartReplacement)
            .await
            .expect("replacement commit");
        release_replacement
            .send(())
            .await
            .expect("release replacement");
        old.finish().await;
        replacement.finish().await;

        assert_eq!(
            store.read(Clone::clone).await.expect("state"),
            [SequenceReplacementAction::Output]
        );
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// Records factory entry, not just polling, after same-program cancellation.
struct DeferredRunFactory {
    calls: Arc<AtomicUsize>,
}

impl Reducer for DeferredRunFactory {
    type State = ();
    type Action = ();

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let calls = Arc::clone(&self.calls);
        Effect::merge([
            Effect::run(move |_sender| {
                calls.fetch_add(1, Ordering::Relaxed);
                async {}
            })
            .cancellable(93_u64),
            Effect::cancel(93_u64),
        ])
    }
}

#[test]
fn cancellation_before_launch_prevents_run_factory_invocation() {
    #[cfg(not(miri))]
    if child::run_in_child("cancellation_before_launch_prevents_run_factory_invocation") {
        return;
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        (),
        DeferredRunFactory {
            calls: Arc::clone(&calls),
        },
        runtime.runtime(),
    );

    futures_executor::block_on(async {
        store.send(()).await.expect("action commit").finish().await;
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// Records whether a cancelled child's grandchild ever reaches the reducer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DescendantCancellationAction {
    Start,
    Child,
    Cancel,
    Grandchild,
}

/// Holds a descendant effect after its parent action has already committed.
struct DescendantCancellation {
    child_started: async_channel::Sender<()>,
    child_gate: async_channel::Receiver<()>,
}

impl Reducer for DescendantCancellation {
    type State = Vec<DescendantCancellationAction>;
    type Action = DescendantCancellationAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.push(*action);
        match action {
            DescendantCancellationAction::Start => {
                Effect::send(DescendantCancellationAction::Child).cancellable(101_u64)
            }
            DescendantCancellationAction::Child => {
                let started = self.child_started.clone();
                let gate = self.child_gate.clone();
                Effect::run(move |sender| async move {
                    started.send(()).await.expect("child start signal");
                    if gate.recv().await.is_ok() {
                        let _ = sender.send(DescendantCancellationAction::Grandchild).await;
                    }
                })
            }
            DescendantCancellationAction::Cancel => Effect::cancel(101_u64),
            DescendantCancellationAction::Grandchild => Effect::none(),
        }
    }
}

#[test]
fn keyed_generation_is_inherited_by_effect_action_descendants() {
    #[cfg(not(miri))]
    if child::run_in_child("keyed_generation_is_inherited_by_effect_action_descendants") {
        return;
    }

    let (child_started, started) = async_channel::bounded(1);
    let (release_child, child_gate) = async_channel::bounded(1);
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        Vec::new(),
        DescendantCancellation {
            child_started,
            child_gate,
        },
        runtime.runtime(),
    );

    futures_executor::block_on(async {
        let root = store
            .send(DescendantCancellationAction::Start)
            .await
            .expect("root commit");
        started.recv().await.expect("child effect started");
        store
            .send(DescendantCancellationAction::Cancel)
            .await
            .expect("cancel commit")
            .finish()
            .await;
        release_child.send(()).await.expect("release child");
        root.finish().await;

        assert_eq!(
            store.read(Clone::clone).await.expect("state"),
            [
                DescendantCancellationAction::Start,
                DescendantCancellationAction::Child,
                DescendantCancellationAction::Cancel,
            ]
        );
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn concatenate_waits_for_every_live_merge_child_before_starting_suffix() {
    for first_finished in [0, 1] {
        let (release_left, left) = async_channel::bounded(1);
        let (release_right, right) = async_channel::bounded(1);
        let releases = [release_left, release_right];
        let gates = [left, right];
        let suffix_starts = Arc::new(AtomicUsize::new(0));
        let starts = Arc::clone(&suffix_starts);
        let reducer = Reduce::new(move |(): &mut (), (): &(), _context| {
            let merged = Effect::merge(gates.iter().map(|gate| {
                let gate = gate.clone();
                Effect::run(move |_| async move {
                    gate.recv().await.expect("release merged child");
                })
            }));
            let starts = Arc::clone(&starts);
            Effect::concatenate([
                merged,
                Effect::run(move |_| {
                    starts.fetch_add(1, Ordering::SeqCst);
                    async {}
                }),
            ])
        });
        let runtime = manual_support::ManualRuntime::new();
        let store = Store::new((), reducer, runtime.runtime());
        let mut actor = runtime.take_only_queued_task();
        let receipt = store.try_send(()).expect("sequence admission");
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let mut receipt = std::pin::pin!(receipt.finish());
        let Poll::Ready(Ok(task)) = receipt.as_mut().poll(&mut cx) else {
            panic!("sequence action must commit in the first actor poll");
        };

        let [mut first, mut second]: [rca_core::RuntimeFuture; 2] = runtime
            .take_queued_tasks()
            .try_into()
            .unwrap_or_else(|_| panic!("merge must start exactly two child effects"));
        if first_finished == 1 {
            std::mem::swap(&mut first, &mut second);
        }
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(second.as_mut().poll(&mut cx).is_pending());
        assert_eq!(runtime.spawn_count(), 3);
        assert_eq!(suffix_starts.load(Ordering::SeqCst), 0);
        assert!(!task.is_finished());
        let mut finished = std::pin::pin!(task.finish());
        assert!(finished.as_mut().poll(&mut cx).is_pending());

        releases[first_finished]
            .try_send(())
            .expect("release first child");
        assert!(first.as_mut().poll(&mut cx).is_ready());
        drop(first);
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        // Harvesting one child must not release the sequence's remaining work.
        assert!(second.as_mut().poll(&mut cx).is_pending());
        assert_eq!(runtime.spawn_count(), 3, "the suffix must remain unspawned");
        assert_eq!(suffix_starts.load(Ordering::SeqCst), 0);
        assert!(!task.is_finished());
        assert!(finished.as_mut().poll(&mut cx).is_pending());

        releases[1 - first_finished]
            .try_send(())
            .expect("release second child");
        assert!(second.as_mut().poll(&mut cx).is_ready());
        drop(second);
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        assert_eq!(runtime.spawn_count(), 4);
        assert_eq!(suffix_starts.load(Ordering::SeqCst), 0);
        assert!(!task.is_finished());
        let mut suffix = runtime.take_only_queued_task();
        assert!(suffix.as_mut().poll(&mut cx).is_ready());
        drop(suffix);
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        assert_eq!(suffix_starts.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.spawn_count(), 4);
        assert!(task.is_finished());
        assert!(finished.as_mut().poll(&mut cx).is_ready());
        assert!(runtime.take_queued_tasks().is_empty());

        let mut shutdown = std::pin::pin!(store.shutdown_into_state());
        assert!(shutdown.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_ready());
        assert!(matches!(
            shutdown.as_mut().poll(&mut cx),
            Poll::Ready(Ok(()))
        ));
        drop(actor);
        assert!(releases.iter().all(async_channel::Sender::is_closed));
        assert_eq!(Arc::strong_count(&suffix_starts), 1);
        assert!(runtime.take_queued_tasks().is_empty());
    }
}

#[test]
fn cancelling_admitted_lineage_during_reduction_stops_sequence_successors() {
    /// Marks the reducer entry that cancels its own real root task.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Action {
        Start,
        Continue,
        Successor,
    }

    let (release, gate) = async_channel::bounded(1);
    let owned_task = Arc::new(std::sync::Mutex::new(None::<ActionTask>));
    let successor_starts = Arc::new(AtomicUsize::new(0));
    let task_for_reducer = Arc::clone(&owned_task);
    let starts_for_reducer = Arc::clone(&successor_starts);
    let reducer = Reduce::new(move |state: &mut Vec<Action>, action: &Action, _context| {
        state.push(*action);
        match action {
            Action::Start => {
                let gate = gate.clone();
                Effect::run(move |sender| async move {
                    gate.recv().await.expect("release admitted effect");
                    let _ = sender.send(Action::Continue).await;
                })
            }
            Action::Continue => {
                task_for_reducer
                    .lock()
                    .expect("owned task lock")
                    .as_ref()
                    .expect("acquired task")
                    .cancel();
                let starts = Arc::clone(&starts_for_reducer);
                Effect::concatenate([
                    Effect::run(move |_| {
                        starts.fetch_add(1, Ordering::SeqCst);
                        async {}
                    }),
                    Effect::send(Action::Successor),
                ])
            }
            Action::Successor => Effect::none(),
        }
    });
    let runtime = manual_support::ManualRuntime::new();
    let store = Store::new(Vec::new(), reducer, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let receipt = store.try_send(Action::Start).expect("start admission");
    let mut cx = Context::from_waker(std::task::Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut receipt = std::pin::pin!(receipt.finish());
    let Poll::Ready(Ok(task)) = receipt.as_mut().poll(&mut cx) else {
        panic!("start must commit in the first actor poll");
    };
    *owned_task.lock().expect("owned task lock") = Some(task.clone());

    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut cx).is_pending());
    release.try_send(()).expect("release effect");
    assert!(effect.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(effect.as_mut().poll(&mut cx).is_ready());
    drop(effect);
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(task.is_cancelled());
    assert!(task.is_finished());
    let mut finished = std::pin::pin!(task.finish());
    assert!(finished.as_mut().poll(&mut cx).is_ready());
    assert_eq!(successor_starts.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.spawn_count(), 2, "no successor effect may start");

    let mut shutdown = std::pin::pin!(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    let Poll::Ready(Ok(state)) = shutdown.as_mut().poll(&mut cx) else {
        panic!("actor must return committed state");
    };
    assert_eq!(state, [Action::Start, Action::Continue]);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the retained cross-scope effect and retirement schedule form one observable case"
)]
fn retiring_outer_scope_preserves_retained_leaf_but_stops_sequence_suffix() {
    /// Separates retained child work from the outer sequence's forbidden suffix.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Action {
        CaptureA,
        StartB,
        RemoveB,
        PingA,
        Suffix,
    }
    /// Keeps child presence and the exact admitted action log observable together.
    struct State {
        a: Option<usize>,
        b: Option<()>,
        actions: Vec<Action>,
    }

    let (release_a, gate_a) = async_channel::bounded(1);
    let retained = Arc::new(std::sync::Mutex::new(None::<Effect<Action>>));
    let a_child = Reduce::new(
        move |state: &mut usize, action: &Action, _context| match action {
            Action::CaptureA => {
                let gate = gate_a.clone();
                Effect::run(move |_| async move {
                    gate.recv().await.expect("release A's live leaf");
                })
            }
            Action::PingA => {
                *state += 1;
                Effect::none()
            }
            Action::StartB | Action::RemoveB | Action::Suffix => Effect::none(),
        },
    );
    let mut a = if_some(
        Reduce::new(|_: &mut State, _: &Action, _| Effect::none()),
        OptionalStatePath::new(
            |state: &State| state.a.as_ref(),
            |state: &mut State| state.a.as_mut(),
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::CaptureA | Action::PingA => Some(action),
                Action::StartB | Action::RemoveB | Action::Suffix => None,
            },
            std::convert::identity,
        ),
        a_child,
    );
    let retained_for_b = Arc::clone(&retained);
    let b_child = Reduce::new(move |(): &mut (), action: &Action, _context| match action {
        Action::StartB => Effect::concatenate([
            retained_for_b
                .lock()
                .expect("retained effect lock")
                .take()
                .expect("A effect retained before B starts"),
            Effect::send(Action::Suffix),
        ]),
        Action::CaptureA | Action::RemoveB | Action::PingA | Action::Suffix => Effect::none(),
    });
    let mut b = if_some(
        Reduce::new(|state: &mut State, action: &Action, _| {
            if *action == Action::RemoveB {
                state.b = None;
            }
            Effect::none()
        }),
        OptionalStatePath::new(
            |state: &State| state.b.as_ref(),
            |state: &mut State| state.b.as_mut(),
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::StartB => Some(action),
                Action::CaptureA | Action::RemoveB | Action::PingA | Action::Suffix => None,
            },
            std::convert::identity,
        ),
        b_child,
    );
    let reducer = Reduce::new(move |state: &mut State, action: &Action, context| {
        state.actions.push(*action);
        let a_effect = a.reduce(state, action, context);
        if *action == Action::CaptureA {
            *retained.lock().expect("retained effect lock") = Some(a_effect);
            return Effect::none();
        }
        Effect::merge([a_effect, b.reduce(state, action, context)])
    });
    let runtime = manual_support::ManualRuntime::new();
    let store = Store::new(
        State {
            a: Some(0),
            b: Some(()),
            actions: Vec::new(),
        },
        reducer,
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(std::task::Waker::noop());

    let capture = store.try_send(Action::CaptureA).expect("capture admission");
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut capture = std::pin::pin!(capture.finish());
    assert!(matches!(capture.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    assert_eq!(runtime.spawn_count(), 1, "retaining A starts no work");

    let start = store.try_send(Action::StartB).expect("B admission");
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut start = std::pin::pin!(start.finish());
    let Poll::Ready(Ok(sequence)) = start.as_mut().poll(&mut cx) else {
        panic!("B sequence must commit");
    };
    let mut a_leaf = runtime.take_only_queued_task();
    assert!(a_leaf.as_mut().poll(&mut cx).is_pending());
    assert!(!sequence.is_finished());

    let remove = store
        .try_send(Action::RemoveB)
        .expect("B removal admission");
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut remove = std::pin::pin!(remove.finish());
    assert!(matches!(remove.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    assert!(!sequence.is_finished(), "A's retained leaf remains live");
    release_a.try_send(()).expect("release retained A leaf");
    assert!(a_leaf.as_mut().poll(&mut cx).is_ready());
    drop(a_leaf);
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(sequence.is_finished(), "all sequence work must finish");
    assert!(
        !sequence.is_cancelled(),
        "B retirement does not cancel A's lineage"
    );
    let mut finished = std::pin::pin!(sequence.finish());
    assert!(finished.as_mut().poll(&mut cx).is_ready());
    assert_eq!(runtime.spawn_count(), 2, "B suffix starts no work");

    let ping = store.try_send(Action::PingA).expect("A remains admitted");
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut ping = std::pin::pin!(ping.finish());
    assert!(matches!(ping.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));

    let mut shutdown = std::pin::pin!(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    let Poll::Ready(Ok(state)) = shutdown.as_mut().poll(&mut cx) else {
        panic!("actor must return scoped state");
    };
    assert_eq!(state.a, Some(1));
    assert!(state.b.is_none());
    assert_eq!(
        state.actions,
        [
            Action::CaptureA,
            Action::StartB,
            Action::RemoveB,
            Action::PingA
        ]
    );
}
