#![cfg(feature = "test-util")]

use super::{poll, support};
use rca_core::diagnostics::{CancellationCause, LifecycleKind};
use rca_core::testing::TransactionGate;
use rca_core::{Effect, Reduce, RuntimeFuture, Store};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Entry {
    InspectionReturned,
    PayloadDrop,
    StateDrop,
    Lifecycle(LifecycleKind),
}

/// Stores the exact retirement order without allocating inside Drop callbacks.
#[derive(Default)]
struct Trace {
    entries: [Option<Entry>; 16],
    length: usize,
}

/// Separates inspection state, payload disposal, State disposal and reduction.
#[derive(Default)]
struct Probe {
    trace: Mutex<Trace>,
    /// Marks only the interval inside the public inspection callback.
    inspecting: AtomicBool,
    payload_drops: AtomicUsize,
    state_drops: AtomicUsize,
    reductions: AtomicUsize,
}

impl Probe {
    /// Copies one inspection event into the fixed buffer without allocating.
    fn record(&self, entry: Entry) {
        let mut trace = self.trace.lock().unwrap();
        let index = trace.length;
        trace.entries[index] = Some(entry);
        trace.length += 1;
    }

    /// Allocates the ordered result outside Drop callbacks, then resets the trace.
    fn take(&self) -> Vec<Entry> {
        let mut trace = self.trace.lock().unwrap();
        let entries = trace.entries[..trace.length]
            .iter()
            .map(|entry| entry.unwrap())
            .collect();
        *trace = Trace::default();
        entries
    }
}

struct State(Arc<Probe>);

impl Drop for State {
    fn drop(&mut self) {
        self.0.state_drops.fetch_add(1, Ordering::SeqCst);
        self.0.record(Entry::StateDrop);
    }
}

/// Re-enters the real actor and gate when an inspected payload is destroyed.
struct Action {
    probe: Arc<Probe>,
    gate: TransactionGate<State, Action>,
    /// The destructor polls this actor after leaving the inspection callback.
    actor: Arc<Mutex<RuntimeFuture>>,
}

impl Drop for Action {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        assert!(!self.probe.inspecting.load(Ordering::SeqCst));
        // Public re-entry would deadlock if the retired payload were dropped
        // under the gate lock. Its boundary must still reserve retirement.
        assert!(self.gate.boundary().is_some());
        assert!(self.gate.has_pending());
        let mut actor = self
            .actor
            .try_lock()
            .expect("inspection retirement must destroy the payload outside actor polling");
        assert!(poll(actor.as_mut()).is_pending());
        assert_eq!(self.probe.state_drops.load(Ordering::SeqCst), 0);
        assert_eq!(
            self.probe.take(),
            [Entry::InspectionReturned],
            "no terminal callback before payload destruction returns"
        );
        self.probe.payload_drops.fetch_add(1, Ordering::SeqCst);
        self.probe.record(Entry::PayloadDrop);
    }
}

/// Marks the interval in which the public inspection callback holds the payload.
struct InspectionActive(Arc<Probe>);

impl Drop for InspectionActive {
    fn drop(&mut self) {
        self.0.inspecting.store(false, Ordering::SeqCst);
        self.0.record(Entry::InspectionReturned);
    }
}

// Catches replacing RC09's retirement await with gate.clear() followed by
// normal finalisation while an actual public inspection still owns a command.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One deferred-close schedule verifies query rejection, capture disposal and terminal event ordering."
)]
fn normal_stop_waits_for_inspection_and_payload_retirement_before_terminal_events() {
    struct QueryCapture(Arc<AtomicUsize>);
    impl Drop for QueryCapture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let runtime = support::ManualRuntime::new();
    let probe = Arc::new(Probe::default());
    let gate = TransactionGate::<State, Action>::new();
    let sink_probe = Arc::clone(&probe);
    let reducer = Reduce::new(|state: &mut State, _: &Action, _| {
        state.0.reductions.fetch_add(1, Ordering::SeqCst);
        Effect::none()
    });
    let store = Store::builder(State(Arc::clone(&probe)), reducer, runtime.runtime())
        .transaction_gate(gate.clone())
        .with_lifecycle_diagnostics(move |event| {
            sink_probe.record(Entry::Lifecycle(event.kind()));
        })
        .build()
        .unwrap();
    let actor = Arc::new(Mutex::new(runtime.take_only_queued_task()));
    assert!(poll(actor.lock().unwrap().as_mut()).is_pending());
    let installed = probe.take();
    assert_eq!(installed.len(), 1);
    assert!(matches!(
        installed[0],
        Entry::Lifecycle(LifecycleKind::Installed {
            lifetime: 0,
            parent: None,
            ..
        })
    ));

    // No root reservation: this real admitted action remains unreduced.
    let receipt = store
        .try_send(Action {
            probe: Arc::clone(&probe),
            gate: gate.clone(),
            actor: Arc::clone(&actor),
        })
        .unwrap_or_else(|_| panic!("root action must be admitted"));
    assert!(poll(actor.lock().unwrap().as_mut()).is_pending());
    assert!(gate.has_pending());
    let boundary = gate.boundary();
    let terminal_boundary = gate.pending_boundary().unwrap();
    assert!(boundary.is_some());
    assert_eq!(probe.reductions.load(Ordering::SeqCst), 0);

    gate.inspect_pending(move |action| {
        action.probe.inspecting.store(true, Ordering::SeqCst);
        let _active = InspectionActive(Arc::clone(&action.probe));
        drop(store); // The actual last root owner closes ingress.
        assert!(poll(action.actor.lock().unwrap().as_mut()).is_pending());
        let query_drops = Arc::new(AtomicUsize::new(0));
        let capture = QueryCapture(Arc::clone(&query_drops));
        let mut query = Box::pin(
            action
                .gate
                .assert_state(move |_| {
                    drop(capture);
                    panic!("deferred closure must reject diagnostic query admission");
                })
                .finish(),
        );
        assert!(matches!(poll(query.as_mut()), Poll::Ready(Err(_))));
        assert_eq!(query_drops.load(Ordering::SeqCst), 1);
        assert_eq!(action.gate.boundary(), boundary);
        assert!(action.gate.has_pending());
        assert_eq!(action.probe.payload_drops.load(Ordering::SeqCst), 0);
        assert_eq!(action.probe.state_drops.load(Ordering::SeqCst), 0);
        assert!(action.probe.take().is_empty());
    })
    .expect("the pending root action must remain inspectable");

    assert_eq!(probe.payload_drops.load(Ordering::SeqCst), 1);
    assert_eq!(probe.state_drops.load(Ordering::SeqCst), 0);
    assert_eq!(probe.reductions.load(Ordering::SeqCst), 0);
    assert!(!gate.has_pending());
    assert_eq!(gate.boundary(), boundary);
    assert_eq!(
        gate.match_terminal(&terminal_boundary, |error| {
            error.reason() == rca_core::TaskAccessReason::StoreClosed
                && error.progress() == rca_core::TaskProgress::NotStarted
        }),
        Some(true)
    );
    assert!(gate.acknowledge_terminal(&terminal_boundary));
    assert!(gate.boundary().is_none());
    let mut rejected = Box::pin(receipt.finish());
    assert!(matches!(poll(rejected.as_mut()), Poll::Ready(Err(_))));
    assert_eq!(probe.take(), [Entry::PayloadDrop]);

    assert!(poll(actor.lock().unwrap().as_mut()).is_ready());
    assert_eq!(
        probe.take(),
        [
            Entry::StateDrop,
            Entry::Lifecycle(LifecycleKind::IngressClosed),
            Entry::Lifecycle(LifecycleKind::CancellationRequested {
                cause: CancellationCause::ActorStop,
                scope: None,
            }),
            Entry::Lifecycle(LifecycleKind::ActorStopped),
        ]
    );
    assert_eq!(probe.state_drops.load(Ordering::SeqCst), 1);
    drop(actor);
    assert_eq!(probe.payload_drops.load(Ordering::SeqCst), 1);
    assert_eq!(probe.state_drops.load(Ordering::SeqCst), 1);
}
