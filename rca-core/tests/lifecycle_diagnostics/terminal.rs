use super::{poll, support};
use rca_core::diagnostics::{CancellationCause, LifecycleEvent, LifecycleKind};
use rca_core::{Feature, FeatureContext, RootMount, RuntimeFuture, Store, TaskAccessReason};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Entry {
    Lifecycle(LifecycleKind),
    StateDrop,
    SinkDrop { state_drops: usize },
}

/// Records callback and destructor order without allocating during shutdown.
#[derive(Default)]
struct Trace {
    entries: [Option<Entry>; 32],
    length: usize,
}

/// Tracks State and sink disposal independently of terminal event delivery.
#[derive(Default)]
struct Probe {
    trace: Mutex<Trace>,
    state_drops: AtomicUsize,
    sink_drops: AtomicUsize,
    armed: AtomicBool,
}

impl Probe {
    /// Copies one trace entry into the fixed buffer without allocating.
    fn record(&self, entry: Entry) {
        let mut trace = self.trace.lock().unwrap();
        let index = trace.length;
        trace.entries[index] = Some(entry);
        trace.length += 1;
    }

    /// Allocates the ordered result outside sink callbacks, then resets the trace.
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

struct Product(Arc<Probe>);

impl Drop for Product {
    fn drop(&mut self) {
        self.0.state_drops.fetch_add(1, Ordering::SeqCst);
        self.0.record(Entry::StateDrop);
    }
}

struct State {
    product: Box<Product>,
}

struct Idle;

impl Feature for Idle {
    type State = State;
    type Action = ();

    fn update(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {}
}

/// Selects mutually exclusive callback and sink-destructor failure points.
#[derive(Clone, Copy, Eq, PartialEq)]
enum PanicAt {
    Never,
    Callback,
    CaptureDrop,
}

/// Records lifecycle facts, then fails at the selected terminal boundary.
struct Sink {
    probe: Arc<Probe>,
    panic_at: PanicAt,
}

impl Sink {
    fn record(&mut self, event: &LifecycleEvent) {
        self.probe.record(Entry::Lifecycle(event.kind()));
        if self.panic_at == PanicAt::Callback
            && event.kind() == LifecycleKind::IngressClosed
            && self.probe.armed.load(Ordering::SeqCst)
        {
            // The trace mutex is no longer held when user code panics.
            panic!("terminal lifecycle callback");
        }
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.probe.sink_drops.fetch_add(1, Ordering::SeqCst);
        self.probe.record(Entry::SinkDrop {
            state_drops: self.probe.state_drops.load(Ordering::SeqCst),
        });
        if self.panic_at == PanicAt::CaptureDrop
            && self.probe.armed.load(Ordering::SeqCst)
            && !std::thread::panicking()
        {
            // Callback and capture failure modes are mutually exclusive.
            panic!("terminal lifecycle capture drop");
        }
    }
}

/// Records the original Product address for the extraction identity check.
struct Fixture {
    _runtime: support::ManualRuntime,
    store: Store<State, ()>,
    actor: RuntimeFuture,
    probe: Arc<Probe>,
    /// Compares the extracted Product with the allocation made before build.
    product_address: *const Product,
}

/// Installs one native root before arming the selected sink failure.
fn fixture(panic_at: PanicAt) -> Fixture {
    let runtime = support::ManualRuntime::new();
    let probe = Arc::new(Probe::default());
    let state = State {
        product: Box::new(Product(Arc::clone(&probe))),
    };
    let product_address = std::ptr::from_ref(state.product.as_ref());
    let mut sink = Sink {
        probe: Arc::clone(&probe),
        panic_at,
    };
    let store = RootMount::new(Idle)
        .builder(state, runtime.runtime())
        .with_lifecycle_diagnostics(move |event| sink.record(event))
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    assert!(poll(actor.as_mut()).is_pending());
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
    probe.armed.store(true, Ordering::SeqCst);
    Fixture {
        _runtime: runtime,
        store,
        actor,
        probe,
        product_address,
    }
}

/// Runs normal shutdown or extraction through one comparable result type.
async fn terminal(store: Store<State, ()>, extract: bool) -> Result<Option<State>, ()> {
    if extract {
        store
            .shutdown_into_state()
            .await
            .map(Some)
            .map_err(|_shutdown_error| ())
    } else {
        store
            .shutdown()
            .await
            .map(|()| None)
            .map_err(|_shutdown_error| ())
    }
}

/// Returns the exact normal terminal facts for both shutdown variants.
fn terminal_entries() -> [Entry; 3] {
    [
        Entry::Lifecycle(LifecycleKind::IngressClosed),
        Entry::Lifecycle(LifecycleKind::CancellationRequested {
            cause: CancellationCause::ActorStop,
            scope: None,
        }),
        Entry::Lifecycle(LifecycleKind::ActorStopped),
    ]
}

#[test]
fn shutdown_destroys_state_and_sink_before_its_successful_reply() {
    let mut fixture = fixture(PanicAt::Never);
    let mut request = Box::pin(fixture.store.shutdown());
    assert!(poll(request.as_mut()).is_pending());
    assert!(poll(fixture.actor.as_mut()).is_ready());
    assert!(matches!(poll(request.as_mut()), Poll::Ready(Ok(()))));
    let events = fixture.probe.take();
    assert_eq!(events[0], Entry::StateDrop);
    assert_eq!(&events[1..4], &terminal_entries());
    assert_eq!(events[4], Entry::SinkDrop { state_drops: 1 });
    assert_eq!(events.len(), 5);
    assert!(matches!(
        poll(Box::pin(fixture.store.shutdown()).as_mut()),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(fixture.probe.state_drops.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.probe.sink_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn extraction_preserves_the_original_allocation_through_sink_destruction() {
    let mut fixture = fixture(PanicAt::Never);
    let mut request = Box::pin(fixture.store.clone().shutdown_into_state());
    assert!(poll(request.as_mut()).is_pending());
    assert!(poll(fixture.actor.as_mut()).is_ready());
    let Poll::Ready(Ok(state)) = poll(request.as_mut()) else {
        panic!("state extraction must succeed after normal finalisation");
    };
    let events = fixture.probe.take();
    assert_eq!(&events[..3], &terminal_entries());
    assert_eq!(events[3], Entry::SinkDrop { state_drops: 0 });
    assert_eq!(events.len(), 4);
    assert_eq!(
        std::ptr::from_ref(state.product.as_ref()),
        fixture.product_address
    );
    assert_eq!(fixture.probe.state_drops.load(Ordering::SeqCst), 0);
    drop(state);
    assert_eq!(fixture.probe.state_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn callback_or_capture_panic_prevents_both_terminal_success_replies() {
    for extract in [false, true] {
        for panic_at in [PanicAt::Callback, PanicAt::CaptureDrop] {
            let mut fixture = fixture(panic_at);
            let mut request = Box::pin(terminal(fixture.store.clone(), extract));
            assert!(poll(request.as_mut()).is_pending());
            let failure = catch_unwind(AssertUnwindSafe(|| poll(fixture.actor.as_mut())))
                .expect_err("configured terminal failure");
            let message = failure.downcast_ref::<&str>().copied().unwrap();
            assert_eq!(
                message,
                if panic_at == PanicAt::Callback {
                    "terminal lifecycle callback"
                } else {
                    "terminal lifecycle capture drop"
                }
            );
            drop(fixture.actor);
            assert!(matches!(poll(request.as_mut()), Poll::Ready(Err(()))));
            let mut ready = Box::pin(fixture.store.ready());
            assert!(matches!(
                poll(ready.as_mut()),
                Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::ActorFailed
            ));
            let events = fixture.probe.take();
            let lifecycle: Vec<_> = events
                .iter()
                .copied()
                .filter(|entry| matches!(entry, Entry::Lifecycle(_)))
                .collect();
            if panic_at == PanicAt::Callback {
                assert_eq!(lifecycle, [terminal_entries()[0]]);
            } else {
                assert_eq!(lifecycle, terminal_entries());
                assert!(events.contains(&Entry::SinkDrop {
                    state_drops: usize::from(!extract)
                }));
            }
            assert_eq!(fixture.probe.state_drops.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.probe.sink_drops.load(Ordering::SeqCst), 1);
        }
    }
}

#[test]
fn abandoning_extraction_before_or_after_handoff_drops_state_once() {
    for abandon_before_actor in [false, true] {
        let mut fixture = fixture(PanicAt::Never);
        let mut request = Box::pin(fixture.store.clone().shutdown_into_state());
        assert!(poll(request.as_mut()).is_pending());
        if abandon_before_actor {
            drop(request);
            assert!(poll(fixture.actor.as_mut()).is_ready());
        } else {
            assert!(poll(fixture.actor.as_mut()).is_ready());
            assert_eq!(fixture.probe.state_drops.load(Ordering::SeqCst), 0);
            drop(request);
        }
        let events = fixture.probe.take();
        assert_eq!(&events[..3], &terminal_entries());
        assert_eq!(events[3], Entry::SinkDrop { state_drops: 0 });
        assert_eq!(events[4], Entry::StateDrop);
        assert_eq!(events.len(), 5);
        assert_eq!(fixture.probe.state_drops.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.probe.sink_drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn terminal_callback_failure_does_not_overwrite_prior_abortive_closure() {
    let mut fixture = fixture(PanicAt::Callback);
    let mut closed = Box::pin(fixture.store.closed());
    assert!(poll(closed.as_mut()).is_pending());
    let mut access = Box::pin(fixture.store.ready());
    let mut extraction = Box::pin(fixture.store.shutdown_into_state());
    assert!(poll(extraction.as_mut()).is_pending());
    drop(extraction);
    let failure = catch_unwind(AssertUnwindSafe(|| poll(fixture.actor.as_mut())))
        .expect_err("terminal callback failure after final owner loss");
    assert_eq!(
        failure.downcast_ref::<&str>().copied(),
        Some("terminal lifecycle callback")
    );
    drop(fixture.actor);
    assert!(matches!(
        poll(access.as_mut()),
        Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::StoreClosed
    ));
    assert_eq!(fixture.probe.state_drops.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.probe.sink_drops.load(Ordering::SeqCst), 1);
    let lifecycle: Vec<_> = fixture
        .probe
        .take()
        .into_iter()
        .filter(|entry| matches!(entry, Entry::Lifecycle(_)))
        .collect();
    assert_eq!(lifecycle, [terminal_entries()[0]]);
    assert!(poll(closed.as_mut()).is_ready());
}
