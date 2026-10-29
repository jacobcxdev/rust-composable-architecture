//! Measures the actual transaction freeze between two callback boundaries.
//!
//! The first reaction-entry lookup starts the sample. The first initial selector
//! ends it. Fixture construction, actor polling and reporting are not all part
//! of that callback-bounded interval.
use super::{ParentState, State, descriptor};
use crate::store::mount::{
    Carriers, Ingress, Lifecycle, MountDeclaration, MountPosition, RootDeclaration, RootInitial,
    ScopeRegistry, ScopeToken, ScopeTransitionBatch, Shared, initial_hook, nested,
};
use crate::{Feature, Runtime, RuntimeDriver, RuntimeFuture, Task};
use parking_lot::Mutex;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};

/// Records one allocator delta between callbacks on the manually polled actor.
#[derive(Default)]
struct Sample {
    /// Prevents later metadata queries from replacing the start boundary.
    started: AtomicBool,
    /// Prevents later initial selectors from replacing the end boundary.
    finished: AtomicBool,
    /// Cumulative allocations before the first metadata query continues.
    start_blocks: AtomicU64,
    /// Cumulative requested bytes at the same start boundary.
    start_bytes: AtomicU64,
    /// Allocations between the first metadata query and first initial selector.
    blocks: AtomicU64,
    /// Requested bytes over the same callback-bounded interval.
    bytes: AtomicU64,
}
impl Sample {
    /// Starts the sample once, on the serial actor callback path.
    fn begin(&self) {
        if !self.started.swap(true, Ordering::SeqCst) {
            let stats = dhat::HeapStats::get();
            self.start_blocks
                .store(stats.total_blocks, Ordering::SeqCst);
            self.start_bytes.store(stats.total_bytes, Ordering::SeqCst);
        }
    }
    /// Ends the sample once, after metadata lookup started it.
    fn end(&self) {
        if !self.finished.swap(true, Ordering::SeqCst) {
            let stats = dhat::HeapStats::get();
            self.blocks.store(
                stats.total_blocks - self.start_blocks.load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
            self.bytes.store(
                stats.total_bytes - self.start_bytes.load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
        }
    }
    /// Requires both callback boundaries before returning blocks and requested bytes.
    fn result(&self) -> (u64, u64) {
        assert!(self.started.load(Ordering::SeqCst));
        assert!(self.finished.load(Ordering::SeqCst));
        (
            self.blocks.load(Ordering::SeqCst),
            self.bytes.load(Ordering::SeqCst),
        )
    }
}

/// Delegates real non-keyed declarations while observing their first metadata lookup.
///
/// These root and optional declarations have no entry stamp. The wrapper does
/// not copy the transaction freeze algorithm or invoke initial selectors itself.
struct Probe {
    /// Owns the real root or optional declaration used by the actor.
    inner: Box<dyn MountDeclaration<State, ()> + Send>,
    /// Shares the two callback boundaries with the actual initial selectors.
    sample: Arc<Sample>,
}
impl MountDeclaration<State, ()> for Probe {
    fn can_discover(&self) -> bool {
        self.inner.can_discover()
    }
    fn depth(&self) -> usize {
        self.inner.depth()
    }
    fn position(&self) -> MountPosition<'_> {
        let inner = self.inner.position();
        MountPosition {
            depth: inner.depth,
            parent: inner.parent,
            entry_stamp: None,
        }
    }
    fn installed_scope(&self) -> Option<ScopeToken> {
        self.inner.installed_scope()
    }
    fn bind(&self, ingress: &Arc<Ingress<State, ()>>, lifecycle: &Arc<Lifecycle>) {
        self.inner.bind(ingress, lifecycle);
    }
    fn reconcile(
        &self,
        state: &State,
        scopes: &mut ScopeRegistry,
        transitions: &mut ScopeTransitionBatch,
        mounted: &mut Vec<Box<<Shared as Carriers<State, ()>>::Node>>,
        parent: ScopeToken,
        discovered: &mut Vec<Box<<Shared as Carriers<State, ()>>::Mount>>,
        retire_descendants: &mut nested::Descendants<'_, State, (), Shared>,
        #[cfg(feature = "diagnostics")] lifecycle: Option<crate::diagnostics::LifecycleCall<'_>>,
    ) {
        self.inner.reconcile(
            state,
            scopes,
            transitions,
            mounted,
            parent,
            discovered,
            retire_descendants,
            #[cfg(feature = "diagnostics")]
            lifecycle,
        );
    }
    fn reaction_entry(
        &self,
    ) -> Option<(
        ScopeToken,
        &dyn super::super::ReactionRunner<State, (), Shared>,
    )> {
        self.sample.begin();
        self.inner.reaction_entry()
    }
}

/// Polls queued work manually with a frozen clock and cooperative yields.
///
/// No fixture task advances time. Future deadlines stay pending, while elapsed
/// deadlines complete immediately. This is not a wake-driven executor probe.
#[derive(Clone, Default)]
struct Driver(
    /// Shares queued tasks between runtime clones without polling under this lock.
    Arc<Mutex<Vec<RuntimeFuture>>>,
);
impl fmt::Debug for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InitialCostDriver")
    }
}
impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.lock().push(task);
    }
    fn now(&self) -> crate::MonotonicInstant {
        crate::MonotonicInstant::ZERO
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
    /// Polls one detached task batch, then requeues pending tasks outside user polling.
    fn turn(&self) {
        let tasks = std::mem::take(&mut *self.0.lock());
        for mut task in tasks {
            if task
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
            {
                self.0.lock().push(task);
            }
        }
    }
}

ordinary_test_items! {
    #[test]
    fn initial_cost_driver_honours_its_frozen_clock() {
        let driver = Driver::default();
        let mut elapsed = driver.sleep_until(crate::MonotonicInstant::ZERO);
        let context = &mut Context::from_waker(Waker::noop());
        assert!(elapsed.as_mut().poll(context).is_ready(), "elapsed deadline must be ready");

        for deadline in [
            crate::MonotonicInstant::from_elapsed(std::time::Duration::from_nanos(1)),
            crate::MonotonicInstant::MAX,
        ] {
            let mut future = driver.sleep_until(deadline);
            assert!(future.as_mut().poll(context).is_pending());
            assert_eq!(driver.now(), crate::MonotonicInstant::ZERO);
        }
    }

    #[test]
    fn initial_cost_driver_yields_before_completing() {
        let driver = Driver::default();
        let mut yielding = driver.yield_now();
        let context = &mut Context::from_waker(Waker::noop());
        assert!(yielding.as_mut().poll(context).is_pending(), "yield must offer a scheduling turn");
        assert!(yielding.as_mut().poll(context).is_ready());
    }
}

/// Supplies a root feature with no reducer work for initial-hook measurements.
struct Root;
impl Feature for Root {
    type State = State;
    type Action = ();
    fn reduce(&mut self, _: &mut State, (): &(), _: &mut crate::FeatureContext<'_, State, ()>) {}
}

/// Compares actual flat and empty-recipe initial freezes for 1 and 64 siblings.
pub(super) fn run() {
    for siblings in [1, 64] {
        let mut paired = Vec::new();
        for recipe in [false, true] {
            let sample = Arc::new(Sample::default());
            let root_sample = Arc::clone(&sample);
            let root = RootDeclaration::<State, ()> {
                payload: Mutex::new(Some(RootInitial {
                    hook: Some(initial_hook(
                        move |_: &State| {
                            root_sample.end();
                        },
                        |_, _| Task::run(|_| async {}),
                    )),
                    reactions: Vec::new(),
                    scope: None,
                    pending: true,
                    configuration: None,
                    current_configuration: None,
                })),
            };
            let driver = Driver::default();
            let mut builder = crate::Store::feature_builder(
                Some(ParentState { child: Some(1) }),
                Root,
                Runtime::new(driver.clone()),
            )
            .with_root_mount(Box::new(Probe {
                inner: Box::new(root),
                sample: Arc::clone(&sample),
            }));
            for _ in 0..siblings {
                let child_sample = Arc::clone(&sample);
                let descriptor = descriptor().on_initial(
                    move |_| {
                        child_sample.end();
                    },
                    |_, _| Task::run(|_| async {}),
                );
                let (owner, _) = if recipe {
                    super::super::children::attach::<State, (), _, _>(descriptor, |_, _| {})
                } else {
                    super::super::attach::<State, (), _, _>(descriptor)
                };
                builder = builder.with_mount(Box::new(Probe {
                    inner: owner,
                    sample: Arc::clone(&sample),
                }));
            }
            let store = builder.build().unwrap();
            let mut ready = std::pin::pin!(store.ready());
            assert!(
                ready
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            driver.turn();
            assert!(matches!(
                ready.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                Poll::Ready(Ok(()))
            ));
            let result = sample.result();
            paired.push(result);
            eprintln!(
                "ACTUAL Initial freeze siblings={siblings} empty-recipes={recipe}: blocks/bytes={result:?}; first reaction-entry query through first selector entry; roster stores indices/scopes, authority is constructed lazily by reactions"
            );
            drop(store);
            driver.turn();
        }
        // Same scopes/hooks and flat depth; only the opted-in depth stack adds
        // storage. This excludes declaration construction and reconciliation.
        assert_eq!(paired[1].0, paired[0].0 + 1);
    }
}
