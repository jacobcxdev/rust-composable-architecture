//! Runtime checks for single-key descendant discovery, refresh and retirement.

use super::*;
use crate::{FeatureContext, KeyedState, Runtime, RuntimeDriver, RuntimeFuture, Store};
use std::cell::Cell;
use std::task::{Context, Poll, Waker};

mod collection;
#[cfg(feature = "diagnostics")]
mod lifecycle;
#[cfg(feature = "local")]
mod local_actor;
mod parent_path;
mod retirement_closure;
mod structural_projection;

/// Holds real actor tasks for explicit, single-threaded polling.
///
/// The clock stays at zero. Elapsed sleeps complete, future waits stay pending,
/// and each yield requests a wake before completing on its second poll.
#[derive(Clone, Default)]
struct Driver(Arc<Mutex<Vec<RuntimeFuture>>>);
impl fmt::Debug for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NestedDriver")
    }
}
impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.lock().push(task);
    }
    fn now(&self) -> crate::MonotonicInstant {
        crate::MonotonicInstant::from_elapsed(std::time::Duration::ZERO)
    }
    fn sleep_until(&self, deadline: crate::MonotonicInstant) -> RuntimeFuture {
        if deadline <= self.now() {
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(std::future::pending())
        }
    }
    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |context| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                context.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }
}
impl Driver {
    /// Polls the current batch outside the queue lock; newly spawned tasks wait.
    fn turn(&self) {
        let tasks = std::mem::take(&mut *self.0.lock());
        for mut task in tasks {
            if poll(task.as_mut()).is_pending() {
                self.0.lock().push(task);
            }
        }
    }
}

#[test]
fn driver_honours_frozen_clock() {
    let driver = Driver::default();
    let mut elapsed = driver.sleep_until(crate::MonotonicInstant::ZERO);
    assert!(
        poll(elapsed.as_mut()).is_ready(),
        "elapsed deadline must be ready"
    );

    for deadline in [
        crate::MonotonicInstant::from_elapsed(std::time::Duration::from_nanos(1)),
        crate::MonotonicInstant::MAX,
    ] {
        let mut future = driver.sleep_until(deadline);
        assert!(poll(future.as_mut()).is_pending());
        assert_eq!(driver.now(), crate::MonotonicInstant::ZERO);
    }
}

#[test]
fn driver_yields_and_wakes_before_completing() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts real wake calls separately from the manually polled queue.
    struct CountWake(AtomicUsize);
    impl std::task::Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let driver = Driver::default();
    let wake = Arc::new(CountWake(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&wake));
    let context = &mut Context::from_waker(&waker);
    let mut yielding = driver.yield_now();

    assert_eq!(wake.0.load(Ordering::SeqCst), 0);
    assert!(
        yielding.as_mut().poll(context).is_pending(),
        "yield must offer a scheduling turn"
    );
    assert_eq!(wake.0.load(Ordering::SeqCst), 1);
    assert!(yielding.as_mut().poll(context).is_ready());
    assert_eq!(wake.0.load(Ordering::SeqCst), 1);
}

/// Polls one scheduled step without claiming to test notification delivery.
fn poll<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}
/// Holds a child that can disappear independently of its keyed parent.
struct Row {
    child: Option<Cell<usize>>,
}
/// Separates keyed replacement from root configuration changes.
struct State {
    rows: KeyedState<String, Row>,
    configuration: u64,
}
/// Selects replacement, an unchanged transaction or configuration refresh.
enum RootAction {
    Replace,
    Noop,
    Refresh,
}
/// Exercises both an unchanged child and explicit retirement from its reducer.
enum RowAction {
    Noop,
    Leave,
}
/// Adds a literal amount through the acquired descendant endpoint.
enum ChildAction {
    Add(usize),
}
/// Applies root mutations before mount reconciliation.
struct RootFeature;
impl Feature for RootFeature {
    type State = State;
    type Action = RootAction;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &RootAction,
        _: &mut FeatureContext<'_, State, RootAction>,
    ) {
        if matches!(action, RootAction::Replace) {
            state.rows.remove(&"one".to_owned());
            state.rows.insert(
                "one".to_owned(),
                Row {
                    child: Some(Cell::new(4)),
                },
            );
        }
        if matches!(action, RootAction::Refresh) {
            state.configuration += 1;
        }
    }
}
/// Records parent disposal in the shared construction and retirement trace.
struct RowFeature(Arc<Mutex<Vec<&'static str>>>);
impl Drop for RowFeature {
    fn drop(&mut self) {
        self.0.lock().push("row drop");
    }
}
impl Feature for RowFeature {
    type State = Row;
    type Action = RowAction;
    fn reduce(
        &mut self,
        _: &mut Row,
        action: &RowAction,
        context: &mut FeatureContext<'_, Row, RowAction>,
    ) {
        if matches!(action, RowAction::Leave) {
            let context = context.reducer_context();
            let token = context.scope_token();
            context.leave(token);
        }
    }
}
/// Retains a per-incarnation cache and records disposal before parent replacement.
struct ChildFeature {
    cache: Cell<usize>,
    trace: Arc<Mutex<Vec<&'static str>>>,
}
impl Drop for ChildFeature {
    fn drop(&mut self) {
        self.trace.lock().push("child drop");
    }
}
impl Feature for ChildFeature {
    type State = Cell<usize>;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut Cell<usize>,
        action: &ChildAction,
        _: &mut FeatureContext<'_, Cell<usize>, ChildAction>,
    ) {
        let ChildAction::Add(value) = action;
        self.cache.set(self.cache.get() + 1);
        state.set(state.get() + value);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one actor schedule keeps subtree discovery, retirement and successor ordering together"
)]
fn actual_actor_discovers_optional_child_and_retires_subtree_before_successor() {
    let driver = Driver::default();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let sites = Arc::new(Mutex::new(Vec::new()));
    let make_trace = std::sync::Arc::clone(&trace);
    let recipe_trace = std::sync::Arc::clone(&trace);
    let recipe_sites = std::sync::Arc::clone(&sites);
    let captures = Arc::new(Mutex::new(Vec::new()));
    let recipe_captures = std::sync::Arc::clone(&captures);
    let retired_captures = std::sync::Arc::clone(&captures);
    let (owner, parent_site) =
        super::nested::attach_selected_key::<State, RootAction, RowFeature, _, _>(
            |state| &state.rows,
            |state| &mut state.rows,
            "one".to_owned(),
            move |_| {
                assert!(
                    retired_captures
                        .lock()
                        .iter()
                        .all(|capture: &Weak<()>| capture.upgrade().is_none()),
                    "descendant captures retire before successor constructor"
                );
                make_trace.lock().push("row make");
                RowFeature(std::sync::Arc::clone(&make_trace))
            },
            move |_, children| {
                recipe_trace.lock().push("recipe");
                let trace = std::sync::Arc::clone(&recipe_trace);
                let capture = Arc::new(());
                recipe_captures.lock().push(Arc::downgrade(&capture));
                let site = children.optional(OptionalMount::new(
                    |row: &Row| row.child.as_ref(),
                    |row: &mut Row| row.child.as_mut(),
                    |row: &Row| {
                        row.child.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    move |_| {
                        assert_eq!(Arc::strong_count(&capture), 1);
                        trace.lock().push("child make");
                        ChildFeature {
                            cache: Cell::new(0),
                            trace: std::sync::Arc::clone(&trace),
                        }
                    },
                ));
                recipe_sites.lock().push(site);
            },
        );
    let store = RootMount::new(RootFeature)
        .configuration(|state| state.configuration)
        .builder(
            State {
                rows: [(
                    "one".to_owned(),
                    Row {
                        child: Some(Cell::new(4)),
                    },
                )]
                .into_iter()
                .collect(),
                configuration: 0,
            },
            Runtime::new(driver.clone()),
        )
        .with_mount(owner)
        .build()
        .unwrap();
    driver.turn();
    assert_eq!(*trace.lock(), ["row make", "recipe", "child make"]);
    let old_site = sites.lock()[0].clone();
    let mut acquire = Box::pin(store.acquire(&old_site));
    assert!(poll(acquire.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(child))) = poll(acquire.as_mut()) else {
        panic!("installed nested context")
    };
    let mut send = Box::pin(child.send(ChildAction::Add(3)));
    assert!(poll(send.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(send.as_mut()), Poll::Ready(Ok(_))));
    let mut read = Box::pin(child.read(Cell::get));
    assert!(poll(read.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(read.as_mut()), Poll::Ready(Ok(7))));
    let mut parent = Box::pin(store.acquire(&parent_site));
    assert!(poll(parent.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(parent))) = poll(parent.as_mut()) else {
        panic!("parent context")
    };
    let mut noop = Box::pin(parent.send(RowAction::Noop));
    assert!(poll(noop.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(noop.as_mut()), Poll::Ready(Ok(_))));
    assert_eq!(sites.lock().len(), 1);
    let mut refresh = Box::pin(store.send(RootAction::Refresh));
    assert!(poll(refresh.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(refresh.as_mut()), Poll::Ready(Ok(_))));
    assert_eq!(
        sites.lock().len(),
        1,
        "configuration refresh retains the same child recipe and caches"
    );
    let mut stale_configuration =
        Box::pin(child.read(|_| panic!("ancestor configuration rejects before selector")));
    assert!(poll(stale_configuration.as_mut()).is_pending());
    driver.turn();
    assert!(
        matches!(poll(stale_configuration.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::ScopeEnded)
    );
    let mut reacquire = Box::pin(store.acquire(&old_site));
    assert!(poll(reacquire.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(child))) = poll(reacquire.as_mut()) else {
        panic!("explicit fresh child acquisition")
    };
    trace.lock().clear();
    let mut replace = Box::pin(store.send(RootAction::Replace));
    assert!(poll(replace.as_mut()).is_pending());
    let mut queued_acquire = Box::pin(store.acquire(&old_site));
    assert!(poll(queued_acquire.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(replace.as_mut()), Poll::Ready(Ok(_))));
    assert_eq!(
        *trace.lock(),
        ["child drop", "row drop", "row make", "recipe", "child make"]
    );
    assert_eq!(sites.lock().len(), 2);
    assert!(
        captures.lock()[0].upgrade().is_none(),
        "old site, context and queued acquisition retain no callback owner"
    );
    assert!(captures.lock()[1].upgrade().is_some());
    assert!(
        matches!(poll(queued_acquire.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::ScopeEnded)
    );
    let mut stale = Box::pin(child.read(|_| panic!("stale selector")));
    assert!(poll(stale.as_mut()).is_pending());
    driver.turn();
    assert!(
        matches!(poll(stale.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::ScopeEnded)
    );
    let mut noop = Box::pin(store.send(RootAction::Noop));
    assert!(poll(noop.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(noop.as_mut()), Poll::Ready(Ok(_))));
    assert_eq!(sites.lock().len(), 2);
    // A reducer can already have removed the old scope before reconciliation.
    // Its empty second leave must still drain payloads before the replacement.
    let mut fresh = Box::pin(store.acquire(&parent_site));
    assert!(poll(fresh.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(fresh))) = poll(fresh.as_mut()) else {
        panic!("fresh parent")
    };
    trace.lock().clear();
    let mut leave = Box::pin(fresh.send(RowAction::Leave));
    assert!(poll(leave.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(leave.as_mut()), Poll::Ready(Ok(_))));
    assert_eq!(
        *trace.lock(),
        ["child drop", "row drop", "row make", "recipe", "child make"]
    );
    assert_eq!(sites.lock().len(), 3);
    assert!(captures.lock()[1].upgrade().is_none());
    let mut shutdown = Box::pin(store.shutdown());
    assert!(poll(shutdown.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Ok(()))));
    driver.turn();
    assert!(driver.0.lock().is_empty());
}
