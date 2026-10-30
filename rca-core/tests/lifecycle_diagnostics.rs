#![cfg(feature = "diagnostics")]

#[expect(
    dead_code,
    reason = "manual host support is shared across integration targets"
)]
#[path = "support/mod.rs"]
mod support;

#[path = "lifecycle_diagnostics/cancellation.rs"]
mod cancellation;
#[path = "lifecycle_diagnostics/configuration.rs"]
mod configuration;
#[cfg(feature = "test-util")]
#[path = "lifecycle_diagnostics/inspection.rs"]
mod inspection;
#[path = "lifecycle_diagnostics/legacy.rs"]
mod legacy;
#[cfg(feature = "local")]
#[path = "lifecycle_diagnostics/local.rs"]
mod local;
#[cfg(feature = "test-util")]
#[path = "lifecycle_diagnostics/panic_progress.rs"]
mod panic_progress;
#[path = "lifecycle_diagnostics/root.rs"]
mod root;
#[path = "lifecycle_diagnostics/stop.rs"]
mod stop;
#[path = "lifecycle_diagnostics/terminal.rs"]
mod terminal;

use rca_core::diagnostics::{CancellationCause, LifecycleEvent, LifecycleKind};
use rca_core::{
    Feature, FeatureContext, MountIdentity, OperationPolicy, OptionalMount, RuntimeFuture, Store,
    Task, TaskAccessReason, TaskProgress,
};
use std::cell::Cell;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// Records a bounded event sequence without allocating during sink callbacks.
///
/// The tests keep each sequence below 32 events and drain it between phases.
#[derive(Default)]
struct Events {
    values: [Option<LifecycleEvent>; 32],
    length: usize,
}
impl Events {
    /// Copies an event into the fixed buffer without allocating in the sink.
    ///
    /// Each test phase must stay below the buffer's 32-event capacity.
    fn record(&mut self, event: &LifecycleEvent) {
        self.values[self.length] = Some(*event);
        self.length += 1;
    }
    /// Drains the buffer and resets its length for the next test phase.
    fn take(&mut self) -> ([Option<LifecycleEvent>; 32], usize) {
        let count = self.length;
        self.length = 0;
        (std::mem::take(&mut self.values), count)
    }
}

/// Polls once without driving another task or registering a useful wake.
fn poll<T>(future: Pin<&mut (impl Future<Output = T> + ?Sized)>) -> Poll<T> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}
/// Admits a request, polls the real actor once, then reads its ordered reply.
fn ordered<T>(actor: &mut RuntimeFuture, request: impl Future<Output = T>) -> T {
    let mut request = Box::pin(request);
    assert!(poll(request.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    match poll(request.as_mut()) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("actor-ordered request did not complete"),
    }
}

struct RootState {
    child: Option<Cell<usize>>,
    selections: Arc<AtomicUsize>,
}
struct Root;
impl Feature for Root {
    type State = RootState;
    type Action = ();
    fn update(&mut self, _: &mut RootState, (): &(), _: &mut FeatureContext<'_, RootState, ()>) {}
}
struct Child;
impl Feature for Child {
    type State = Cell<usize>;
    type Action = ();
    fn update(
        &mut self,
        _: &mut Cell<usize>,
        (): &(),
        _: &mut FeatureContext<'_, Cell<usize>, ()>,
    ) {
    }
}
/// Counts disposal of a retained body capture after its runtime owner releases it.
struct BodyCapture(Arc<AtomicUsize>);
impl Drop for BodyCapture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

// Catches value-difference-based lifetime tracing, fabricated cancellation
// completion, stale selection, and structural events stamped after commit.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one ordered replacement schedule keeps retirement facts adjacent to retained-work assertions"
)]
fn equal_valued_replacement_reports_real_retirement_without_completing_retained_work() {
    let runtime = support::ManualRuntime::new();
    let events = Arc::new(Mutex::new(Events::default()));
    let sink_events = Arc::clone(&events);
    let calls = Cell::new(0); // The shared sink must not require Sync.
    let replacement = Arc::new(AtomicU64::new(0));
    let selected_replacement = Arc::clone(&replacement);
    let selections = Arc::new(AtomicUsize::new(0));
    let body_drops = Arc::new(AtomicUsize::new(0));
    let body_factories = Arc::new(AtomicUsize::new(0));
    let drops = Arc::clone(&body_drops);
    let factories = Arc::clone(&body_factories);
    let mount = OptionalMount::new(
        |state: &RootState| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.child.as_ref()
        },
        |state: &mut RootState| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.child.as_mut()
        },
        move |state: &RootState| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: selected_replacement.load(Ordering::SeqCst),
                configuration: 0,
            })
        },
        |_| Child,
    )
    .on_initial(Cell::get, move |_, _| {
        let capture = BodyCapture(Arc::clone(&drops));
        let factories = Arc::clone(&factories);
        Task::run_named("initial", OperationPolicy::Coexist, move |_| async move {
            factories.fetch_add(1, Ordering::SeqCst);
            let _capture = capture;
            std::future::pending::<()>().await;
        })
    });
    let (builder, site) = Store::builder(
        RootState {
            child: Some(Cell::new(4)),
            selections: Arc::clone(&selections),
        },
        Root,
        runtime.runtime(),
    )
    .mount_with_site(mount);
    let store = builder
        .with_lifecycle_diagnostics(move |event| {
            calls.set(calls.get() + 1);
            sink_events.lock().unwrap().record(event);
        })
        .build()
        .unwrap();
    assert_eq!(events.lock().unwrap().length, 0);
    let mut actor = runtime.take_only_queued_task();
    assert!(poll(actor.as_mut()).is_pending());
    let old_body = runtime.take_only_queued_task();
    let (installed, count) = events.lock().unwrap().take();
    assert_eq!(count, 2);
    assert!(matches!(
        installed[0].unwrap().kind(),
        LifecycleKind::Installed {
            lifetime: 0,
            parent: None,
            ..
        }
    ));
    let LifecycleKind::Installed {
        lifetime: old,
        parent: Some(0),
        configuration: Some(0),
    } = installed[1].unwrap().kind()
    else {
        panic!("actual restored child installation metadata");
    };
    assert_ne!(old, 0);
    assert_eq!(installed[1].unwrap().last_committed_version(), 0);
    let child = ordered(&mut actor, store.acquire(&site)).unwrap().unwrap();
    let operation = child.operation("initial");
    let mut captured = Box::pin(ordered(&mut actor, operation.capture()).unwrap());
    let unchanged = ordered(&mut actor, child.update(|state| state.get())).unwrap();
    assert_eq!(*unchanged.value(), 4);
    let prior_revision = unchanged.revision();
    assert_eq!(events.lock().unwrap().length, 0);

    // The explicit marker changes; public State and domain identity do not.
    replacement.store(1, Ordering::SeqCst);
    drop(ordered(&mut actor, store.send(())).unwrap());
    let replacement_body = runtime.take_only_queued_task();
    let (changed, count) = events.lock().unwrap().take();
    assert_eq!(count, 3);
    assert_eq!(
        changed[0].unwrap().kind(),
        LifecycleKind::Retired { lifetime: old }
    );
    let LifecycleKind::Installed {
        lifetime: fresh,
        parent: Some(0),
        configuration: Some(0),
    } = changed[1].unwrap().kind()
    else {
        panic!("replacement installation metadata");
    };
    assert_ne!(old, fresh);
    assert_eq!(
        changed[2].unwrap().kind(),
        LifecycleKind::CancellationRequested {
            cause: CancellationCause::ScopeRetirement,
            scope: Some(old)
        }
    );
    for event in changed[..count].iter().flatten() {
        assert_eq!(event.last_committed_version(), prior_revision);
    }
    assert_eq!(body_drops.load(Ordering::SeqCst), 0);
    assert_eq!(body_factories.load(Ordering::SeqCst), 0);
    assert!(poll(captured.as_mut()).is_pending());
    selections.store(0, Ordering::SeqCst);
    let error = ordered(&mut actor, child.read(|_| panic!("retired callback"))).unwrap_err();
    assert_eq!(
        (error.reason(), error.progress()),
        (TaskAccessReason::ScopeEnded, TaskProgress::NotStarted)
    );
    assert_eq!(selections.load(Ordering::SeqCst), 0);
    let fresh_child = ordered(&mut actor, store.acquire(&site)).unwrap().unwrap();
    assert_eq!(ordered(&mut actor, fresh_child.read(Cell::get)).unwrap(), 4);
    drop(ordered(&mut actor, store.send(())).unwrap());
    assert_eq!(events.lock().unwrap().length, 0);
    drop(old_body);
    assert_eq!(body_drops.load(Ordering::SeqCst), 1);
    assert_eq!(poll(captured.as_mut()), Poll::Ready(Ok(())));
    drop((replacement_body, store));
    assert!(poll(actor.as_mut()).is_ready());
}
