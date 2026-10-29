//! Checks trace ordering, rendered output and owned effects through real Stores.
//!
//! Release builds retain the ownership and delivery checks but omit trace callbacks.

#![cfg(feature = "diagnostics")]

mod support;

use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rca_core::diagnostics::{Change, format_snapshot_mismatch};
use rca_core::{
    ActionPath, Effect, ReduceContext, Reducer, ReducerExt, Scope, StatePath, Store, combine,
};

use support::ManualRuntime;

/// Move-only domain state: tracing must not require `Clone` or `Debug`.
struct State {
    value: usize,
}

/// Move-only input that applies one arithmetic change.
struct Action {
    delta: usize,
}

/// Local captured value that counts semantic comparisons independently of rendering.
struct Snapshot {
    /// `Rc` prevents an accidental `Send` requirement on diagnostic snapshots.
    value: Rc<usize>,
    comparisons: Arc<AtomicUsize>,
}

impl PartialEq for Snapshot {
    fn eq(&self, other: &Self) -> bool {
        self.comparisons.fetch_add(1, Ordering::SeqCst);
        self.value == other.value
    }
}

/// Records reduction between the two capture callbacks.
struct Counter(Arc<Mutex<Vec<&'static str>>>);

impl Reducer for Counter {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        // Context diagnostics expose the turn clock, not dependency or scope data.
        assert_eq!(
            format!("{context:?}"),
            format!("ReduceContext {{ now: {:?}, .. }}", context.now())
        );
        self.0.lock().unwrap().push("reduce");
        state.value += action.delta;
        Effect::none()
    }
}

#[test]
fn independent_local_snapshots_observe_move_only_domains_in_order() {
    let runtime = ManualRuntime::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let records = Arc::new(Mutex::new(Vec::new()));
    let comparisons = Arc::new(AtomicUsize::new(0));
    let capture_events = Arc::clone(&events);
    let capture_comparisons = Arc::clone(&comparisons);
    let sink_events = Arc::clone(&events);
    let sink_records = Arc::clone(&records);
    let mut invocation = 0;
    let reducer = Counter(Arc::clone(&events)).trace_changes(
        move |state| {
            capture_events.lock().unwrap().push("capture");
            Snapshot {
                value: Rc::new(state.value),
                comparisons: Arc::clone(&capture_comparisons),
            }
        },
        move |change: Change<'_, Action, Snapshot>| {
            invocation += 1;
            sink_events.lock().unwrap().push("sink");
            // Debug must not require domain Debug or perform equality again.
            let debug = format!("{change:?}");
            assert!(debug.contains("Change"));
            sink_records.lock().unwrap().push((
                invocation,
                change.action().delta,
                *change.before().value,
                *change.after().value,
                change.is_changed(),
            ));
        },
    );
    assert!(format!("{reducer:?}").contains("TraceChanges"));
    let store = Store::new(State { value: 3 }, reducer, runtime.runtime());
    runtime.start_all();
    futures_executor::block_on(async {
        store
            .send(Action { delta: 2 })
            .await
            .unwrap()
            .finish()
            .await;
        store
            .send(Action { delta: 0 })
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(store.shutdown_into_state().await.unwrap().value, 5);
    });
    assert!(runtime.join_all().into_iter().all(|result| result.is_ok()));
    if cfg!(debug_assertions) {
        assert_eq!(
            *events.lock().unwrap(),
            [
                "capture", "reduce", "capture", "sink", "capture", "reduce", "capture", "sink"
            ]
        );
        assert_eq!(
            *records.lock().unwrap(),
            [(1, 2, 3, 5, true), (2, 0, 5, 5, false)]
        );
        assert_eq!(comparisons.load(Ordering::SeqCst), 2);
    } else {
        assert_eq!(*events.lock().unwrap(), ["reduce", "reduce"]);
        assert!(records.lock().unwrap().is_empty());
        assert_eq!(comparisons.load(Ordering::SeqCst), 0);
    }
}

/// Records when the mapped response releases its payload, relative to trace callbacks.
struct TracedEffectOwner {
    events: Arc<Mutex<Vec<&'static str>>>,
    drops: Arc<AtomicUsize>,
}

impl Drop for TracedEffectOwner {
    fn drop(&mut self) {
        self.events.lock().unwrap().push("drop");
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Carries either the initial input or the single owned effect response.
struct TracedChildAction {
    delta: usize,
    /// Only the response owns this probe, which also prevents a second effect.
    owner: Option<TracedEffectOwner>,
}

/// Wraps the child response so the action mapper participates in the event oracle.
struct TracedParentAction(TracedChildAction);

/// Emits one owned response and records each child reduction.
struct TracedChild {
    events: Arc<Mutex<Vec<&'static str>>>,
    drops: Arc<AtomicUsize>,
}

impl Reducer for TracedChild {
    type State = usize;
    type Action = TracedChildAction;

    fn reduce(
        &mut self,
        state: &mut usize,
        action: &TracedChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<TracedChildAction> {
        self.events.lock().unwrap().push("child");
        *state += action.delta;
        if action.owner.is_none() {
            let response = TracedChildAction {
                delta: 1,
                owner: Some(TracedEffectOwner {
                    events: Arc::clone(&self.events),
                    drops: Arc::clone(&self.drops),
                }),
            };
            Effect::future(async move { Some(response) })
        } else {
            Effect::none()
        }
    }
}

/// Accumulates the updated child value after each child reduction.
struct TracedParent(Arc<Mutex<Vec<&'static str>>>);

#[test]
fn disabled_trace_preserves_concrete_reducer_and_owned_effect_delivery() {
    let runtime = ManualRuntime::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    // The concrete type stays unchanged; neither reducer nor action needs
    // Clone, Debug, PartialEq or diagnostic capture bounds.
    let reducer: TracedChild = TracedChild {
        events: Arc::clone(&events),
        drops: Arc::clone(&drops),
    }
    .trace_changes_disabled();
    assert!(events.lock().unwrap().is_empty());
    let store = Store::new(3, reducer, runtime.runtime());
    runtime.start_all();
    futures_executor::block_on(async {
        let task = store
            .send(TracedChildAction {
                delta: 2,
                owner: None,
            })
            .await
            .unwrap();
        runtime.wait_for_queued_task();
        runtime.start_all();
        task.finish().await;
        assert_eq!(store.shutdown_into_state().await.unwrap(), 6);
    });
    assert!(runtime.join_all().into_iter().all(|result| result.is_ok()));
    assert_eq!(*events.lock().unwrap(), ["child", "child", "drop"]);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

impl Reducer for TracedParent {
    type State = (usize, usize);
    type Action = TracedParentAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        self.0.lock().unwrap().push("parent");
        state.1 += state.0;
        Effect::none()
    }
}

#[test]
fn outer_trace_encloses_child_and_parent_before_mapped_effect_delivery() {
    let runtime = ManualRuntime::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let records = Arc::new(Mutex::new(Vec::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    let capture_events = Arc::clone(&events);
    let sink_events = Arc::clone(&events);
    let sink_records = Arc::clone(&records);
    let sink_drops = Arc::clone(&drops);
    let embed_events = Arc::clone(&events);
    let child = Scope::new(
        StatePath::new(|state: &(usize, usize)| &state.0, |state| &mut state.0),
        ActionPath::new(
            |action: &TracedParentAction| Some(&action.0),
            move |action| {
                embed_events.lock().unwrap().push("embed");
                TracedParentAction(action)
            },
        ),
        TracedChild {
            events: Arc::clone(&events),
            drops: Arc::clone(&drops),
        },
    );
    let reducer = combine((child, TracedParent(Arc::clone(&events)))).trace_changes(
        move |state| {
            capture_events.lock().unwrap().push("capture");
            *state
        },
        move |change| {
            assert_eq!(sink_drops.load(Ordering::SeqCst), 0);
            sink_events.lock().unwrap().push("sink");
            sink_records.lock().unwrap().push((
                change.action().0.delta,
                *change.before(),
                *change.after(),
                change.is_changed(),
            ));
        },
    );
    let store = Store::new((3, 0), reducer, runtime.runtime());
    runtime.start_all();
    futures_executor::block_on(async {
        let task = store
            .send(TracedParentAction(TracedChildAction {
                delta: 2,
                owner: None,
            }))
            .await
            .unwrap();
        runtime.wait_for_queued_task();
        runtime.start_all();
        task.finish().await;
        assert_eq!(store.shutdown_into_state().await.unwrap(), (6, 11));
    });
    assert!(runtime.join_all().into_iter().all(|result| result.is_ok()));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    if cfg!(debug_assertions) {
        assert_eq!(
            *events.lock().unwrap(),
            [
                "capture", "child", "parent", "capture", "sink", "embed", "capture", "child",
                "parent", "capture", "sink", "drop"
            ]
        );
        assert_eq!(
            *records.lock().unwrap(),
            [(2, (3, 0), (5, 5), true), (1, (5, 5), (6, 11), true)]
        );
    } else {
        assert_eq!(
            *events.lock().unwrap(),
            ["child", "parent", "embed", "child", "parent", "drop"]
        );
        assert!(records.lock().unwrap().is_empty());
    }
}

/// Replaces text without effects so rendering cases isolate trace output.
struct Text;

impl Reducer for Text {
    type State = String;
    type Action = String;

    fn reduce(
        &mut self,
        state: &mut String,
        action: &String,
        _: &mut ReduceContext<'_>,
    ) -> Effect<String> {
        state.clone_from(action);
        Effect::none()
    }
}

/// Runs one committed text change and joins the actor before returning sink output.
#[cfg(debug_assertions)]
fn rendered(
    before: String,
    after: String,
    sink: impl for<'a> FnMut(Change<'a, String, String>) -> String + Send + 'static,
) -> String {
    let runtime = ManualRuntime::new();
    let output = Arc::new(Mutex::new(String::new()));
    let sink_output = Arc::clone(&output);
    let mut sink = sink;
    let reducer = Text.trace_changes(Clone::clone, move |change| {
        *sink_output.lock().unwrap() = sink(change);
    });
    let store = Store::new(before, reducer, runtime.runtime());
    runtime.start_all();
    futures_executor::block_on(async {
        store.send(after).await.unwrap().finish().await;
        store.shutdown().await.unwrap();
    });
    assert!(runtime.join_all().into_iter().all(|result| result.is_ok()));
    output.lock().unwrap().clone()
}

#[test]
#[cfg(debug_assertions)]
fn convenient_format_renders_debug_values_and_before_after_labels() {
    let output = rendered("old".into(), "new".into(), |change| change.format());
    assert_eq!(
        output,
        "action:\n  1 | \"new\"\nbefore:\n- 1 | \"old\"\nafter:\n+ 1 | \"new\"\nLater differences may be omitted."
    );
}

#[test]
#[cfg(debug_assertions)]
fn unchanged_state_never_calls_its_renderer() {
    let action_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&action_calls);
    let output = rendered("same".into(), "same".into(), move |change| {
        assert!(!change.is_changed());
        change.format_with(
            |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                "safe action".into()
            },
            |_| panic!("unchanged state must not render"),
        )
    });
    assert_eq!(output, "action:\n  1 | safe action\nstate unchanged");
    assert_eq!(action_calls.load(Ordering::SeqCst), 1);
}

#[test]
#[cfg(debug_assertions)]
fn semantic_change_survives_identical_redaction_without_debug_fallback() {
    let state_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&state_calls);
    let output = rendered(
        "secret before".into(),
        "secret after".into(),
        move |change| {
            assert!(change.is_changed());
            assert!(!format!("{change:?}").contains("secret"));
            change.format_with(
                |_| "[redacted action]".into(),
                |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    "[redacted state]".into()
                },
            )
        },
    );
    assert_eq!(
        output,
        "action:\n  1 | [redacted action]\nSnapshots differ semantically, but their rendered representations are identical."
    );
    assert!(!output.contains("secret"));
    assert_eq!(state_calls.load(Ordering::SeqCst), 2);
}

#[test]
#[cfg(debug_assertions)]
fn action_and_state_rendering_bound_unicode_lines_and_preserve_newlines() {
    let line = "界".repeat(300);
    let text = (0..20)
        .map(|_| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let output = rendered("old".into(), text, |change| {
        change.format_with(Clone::clone, Clone::clone)
    });
    let (action, state) = output.split_once("before:\n").unwrap();
    assert_eq!(
        action.lines().filter(|line| line.contains(" | ")).count(),
        16
    );
    assert!(action.contains("4 action lines omitted"));
    assert!(state.contains("4 changed lines omitted"));
    for line in output.lines().filter(|line| line.contains(" | ")) {
        assert!(
            line.chars().count() <= 240,
            "{} scalars",
            line.chars().count()
        );
    }
    assert!(output.contains("[line truncated]"));
    let output = rendered("a".into(), "a\n".into(), |change| {
        change.format_with(Clone::clone, Clone::clone)
    });
    assert!(output.contains("  2 | \n"));
    assert!(output.contains("+ 2 | \n"));
}

#[test]
fn shared_mismatch_keeps_semantic_inequality_and_expected_actual_labels() {
    assert_eq!(
        format_snapshot_mismatch("same", "same"),
        "Snapshots differ semantically, but their rendered representations are identical."
    );
    assert_eq!(
        format_snapshot_mismatch("a", "a\n"),
        "expected:\n  1 | a\nactual:\n  1 | a\n+ 2 | \nLater differences may be omitted."
    );
}

/// Separates reducer entry from effect polling when a diagnostic callback panics.
struct EffectReducer {
    reductions: Arc<AtomicUsize>,
    deliveries: Arc<AtomicUsize>,
}

impl Reducer for EffectReducer {
    type State = usize;
    type Action = bool;

    fn reduce(
        &mut self,
        state: &mut usize,
        action: &bool,
        _: &mut ReduceContext<'_>,
    ) -> Effect<bool> {
        self.reductions.fetch_add(1, Ordering::SeqCst);
        *state += 1;
        if *action {
            let deliveries = Arc::clone(&self.deliveries);
            Effect::run(move |sender| async move {
                deliveries.fetch_add(1, Ordering::SeqCst);
                sender.send(false).await.unwrap();
            })
            .cancellable(7_u8)
        } else {
            Effect::none()
        }
    }
}

#[test]
fn original_effect_delivers_descendant_actions_through_store() {
    let runtime = ManualRuntime::new();
    let reductions = Arc::new(AtomicUsize::new(0));
    let deliveries = Arc::new(AtomicUsize::new(0));
    let reducer = EffectReducer {
        reductions: Arc::clone(&reductions),
        deliveries: Arc::clone(&deliveries),
    }
    .trace_changes(|state| *state, |_| {});
    let store = Store::new(0, reducer, runtime.runtime());
    runtime.start_all();
    futures_executor::block_on(async {
        let task = store.send(true).await.unwrap();
        runtime.wait_for_queued_task();
        runtime.start_all();
        task.finish().await;
        assert_eq!(store.shutdown_into_state().await.unwrap(), 2);
    });
    assert_eq!(reductions.load(Ordering::SeqCst), 2);
    assert_eq!(deliveries.load(Ordering::SeqCst), 1);
    assert!(runtime.join_all().into_iter().all(|result| result.is_ok()));
}

#[test]
fn original_cancellable_effect_keeps_its_cancellation_identity() {
    /// Holds delivery behind a real channel until named cancellation ends the effect.
    struct Gated(async_channel::Receiver<()>);

    impl Reducer for Gated {
        type State = usize;
        type Action = u8;

        fn reduce(
            &mut self,
            state: &mut usize,
            action: &u8,
            _: &mut ReduceContext<'_>,
        ) -> Effect<u8> {
            match action {
                0 => {
                    let gate = self.0.clone();
                    Effect::run(move |sender| async move {
                        gate.recv().await.unwrap();
                        sender.send(2).await.unwrap();
                    })
                    .cancellable(7_u8)
                }
                1 => Effect::cancel(7_u8),
                _ => {
                    *state += 1;
                    Effect::none()
                }
            }
        }
    }
    let runtime = ManualRuntime::new();
    let (_release, gate) = async_channel::bounded(1);
    let store = Store::new(
        0,
        Gated(gate).trace_changes(|state| *state, |_| {}),
        runtime.runtime(),
    );
    runtime.start_all();
    futures_executor::block_on(async {
        let task = store.send(0).await.unwrap();
        runtime.wait_for_queued_task();
        runtime.start_all();
        store.send(1).await.unwrap().finish().await;
        task.finish().await;
        assert!(task.is_finished());
        assert_eq!(store.shutdown_into_state().await.unwrap(), 0);
    });
    assert!(runtime.join_all().into_iter().all(|result| result.is_ok()));
}

#[test]
#[cfg(debug_assertions)]
fn diagnostic_panics_propagate_and_post_reduction_panics_prevent_effect_delivery() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::task::{Context, Waker};

    for stage in ["before", "after", "sink"] {
        let runtime = ManualRuntime::new();
        let reductions = Arc::new(AtomicUsize::new(0));
        let deliveries = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let sinks = Arc::new(AtomicUsize::new(0));
        let capture_calls = Arc::clone(&captures);
        let sink_calls = Arc::clone(&sinks);
        let reducer = EffectReducer {
            reductions: Arc::clone(&reductions),
            deliveries: Arc::clone(&deliveries),
        }
        .trace_changes(
            move |state| {
                let call = capture_calls.fetch_add(1, Ordering::SeqCst);
                assert!(
                    !(stage == "before" || stage == "after" && call == 1),
                    "capture panic"
                );
                *state
            },
            move |_| {
                sink_calls.fetch_add(1, Ordering::SeqCst);
                panic!("sink panic");
            },
        );
        let store = Store::new(0, reducer, runtime.runtime());
        let mut actor = runtime.take_only_queued_task();
        let mut context = Context::from_waker(Waker::noop());
        let send = store.send(true);
        let mut send = std::pin::pin!(send);
        assert!(send.as_mut().poll(&mut context).is_pending());
        catch_unwind(AssertUnwindSafe(|| actor.as_mut().poll(&mut context))).unwrap_err();
        assert_eq!(
            reductions.load(Ordering::SeqCst),
            usize::from(stage != "before")
        );
        assert_eq!(
            captures.load(Ordering::SeqCst),
            if stage == "before" { 1 } else { 2 }
        );
        assert_eq!(sinks.load(Ordering::SeqCst), usize::from(stage == "sink"));
        assert_eq!(deliveries.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.spawn_count(), 1);
        drop(actor);
        assert!(matches!(
            send.as_mut().poll(&mut context),
            std::task::Poll::Ready(Err(_))
        ));
    }
}

#[test]
fn captures_are_owned_until_wrapper_drop_including_release_builds() {
    /// Counts closure disposal even when release builds never invoke the callbacks.
    struct Probe(Arc<AtomicUsize>);

    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let capture_probe = Probe(Arc::clone(&drops));
    let sink_probe = Probe(Arc::clone(&drops));
    let reducer = Text.trace_changes(
        move |state| {
            std::hint::black_box(&capture_probe);
            state.clone()
        },
        move |_| {
            std::hint::black_box(&sink_probe);
        },
    );
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(reducer);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}
