//! Callback and unwind ordering through the borrowed production kernel.
use super::tests::panic_message;
use super::*;
use crate::store::scope::{ScopeIdentity, ScopeSlot};
use crate::store::transaction::tests::Host;
use crate::{Reduce, TaskSendError};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

/// Returns feature presence, or None while the installed-node guard is held.
type AvailabilityProbe = Rc<dyn Fn() -> Option<bool>>;

thread_local! {
    // Replies are delivered on this test's serial actor thread. The callback
    // itself retains only a weak node, including for the local Rc carrier.
    // None means the node guard is held; Some records whether the Feature is restored.
    static AVAILABLE: RefCell<Option<AvailabilityProbe>> = RefCell::new(None);
}
#[derive(Clone, Copy, PartialEq, Debug)]
/// Selects the one deliberate failure point in an otherwise identical schedule.
enum Phase {
    Success,
    Missing,
    SelectorPanic,
    ReducerPanic,
    HandlerPanic,
    InputDropPanic,
}
#[derive(Default)]
/// Observes callback order without keeping the installed node alive.
struct Events {
    /// Selector, reducer, issue handler, Feature drop, capture drop, wake, input drop.
    counts: [AtomicUsize; 7],
    /// Counts observations made while the node guard is unavailable.
    guarded: AtomicUsize,
    /// Records restoration before the deliberately panicking input destructor.
    restored_at_input_drop: AtomicUsize,
    /// Records released task captures at terminal reply publication.
    captures_at_terminal_wake: AtomicUsize,
    /// Records destroyed inputs at terminal reply publication.
    inputs_at_terminal_wake: AtomicUsize,
}
impl Events {
    /// Records one event and checks its guard boundary through the weak probe.
    fn observe(&self, event: usize) {
        self.counts[event].fetch_add(1, Ordering::SeqCst);
        if AVAILABLE
            .with_borrow(|probe| probe.as_ref().unwrap()())
            .is_none()
        {
            self.guarded.fetch_add(1, Ordering::SeqCst);
        }
    }
    /// Reads one counter without invoking the availability probe again.
    fn count(&self, event: usize) -> usize {
        self.counts[event].load(Ordering::SeqCst)
    }

    /// Checks input and task cleanup before the host is dropped.
    fn assert_input_cleanup(&self, phase: Phase) {
        assert_eq!(
            self.count(0),
            if matches!(phase, Phase::Success | Phase::Missing) {
                2
            } else {
                1
            }
        );
        assert_eq!(
            self.count(6),
            self.count(0),
            "each entered input drops once"
        );
        assert_eq!(self.count(5), self.count(0), "one terminal wake per input");
        assert_eq!(
            self.count(4),
            self.count(1),
            "each declared task capture drops once, unpolled"
        );
    }
}
impl Wake for Events {
    fn wake(self: Arc<Self>) {
        self.captures_at_terminal_wake
            .store(self.count(4), Ordering::SeqCst);
        self.inputs_at_terminal_wake
            .store(self.count(6), Ordering::SeqCst);
        self.observe(5);
    }
}
/// Holds selected state and the schedule chosen before admission.
struct State {
    /// Accumulates feature-local increments to reveal cache restoration.
    child: usize,
    /// The installed child scope.
    token: Option<ScopeToken>,
    /// Controls selector absence or deliberate selector panic.
    phase: Phase,
    /// Shared observations that outlive both host and installed Feature.
    events: Arc<Events>,
}
/// Enters the actual projection boundary and applies the selected failure mode.
fn select(state: &mut State) -> Option<&mut usize> {
    state.events.observe(0);
    assert_ne!(state.phase, Phase::SelectorPanic, "selector panic");
    if state.phase == Phase::Missing {
        None
    } else {
        Some(&mut state.child)
    }
}
/// Observes exact input destruction, including a deliberate destructor panic.
struct Input {
    /// Selects reducer and input-drop behaviour independently of later state changes.
    phase: Phase,
    /// Records destruction before terminal publication.
    events: Arc<Events>,
}
impl Drop for Input {
    fn drop(&mut self) {
        self.events.observe(6);
        if self.phase == Phase::InputDropPanic {
            let restored = AVAILABLE.with_borrow(|probe| probe.as_ref().unwrap()()) == Some(true);
            self.events
                .restored_at_input_drop
                .store(usize::from(restored), Ordering::SeqCst);
            panic!("input drop panic");
        }
    }
}
/// Counts cleanup of declared task work that the fixture never polls.
struct Capture(Arc<Events>);
impl Drop for Capture {
    fn drop(&mut self) {
        self.0.observe(4);
    }
}
/// Owns a cache that must survive successful and absent projections.
struct Child {
    /// Distinguishes the original Feature from a reconstructed replacement.
    cache: usize,
    /// Records reducer entry and Feature destruction.
    events: Arc<Events>,
}
impl Drop for Child {
    fn drop(&mut self) {
        self.events.observe(3);
    }
}
impl Feature for Child {
    type State = usize;
    type Action = Input;
    fn update(
        &mut self,
        state: &mut usize,
        action: &Input,
        context: &mut FeatureContext<'_, usize, Input>,
    ) {
        self.events.observe(1);
        self.cache += 1;
        *state += self.cache;
        let capture = Capture(Arc::clone(&self.events));
        context.task(crate::Task::run(move |_| async move {
            drop(capture);
        }));
        assert_ne!(action.phase, Phase::ReducerPanic, "reducer panic");
        context
            .reducer_context()
            .report_issue(crate::Issue::new("lease", "callback"))
            .unwrap();
    }
}
#[cfg(feature = "local")]
/// Forces the Local witness to retain genuinely non-Send feature state.
struct LocalChild {
    /// Shares the same observation and reduction schedule as Shared.
    child: Child,
    /// Prevents accidental Send compatibility in the fixture.
    _local: Rc<()>,
}
#[cfg(feature = "local")]
impl local::Feature for LocalChild {
    type State = usize;
    type Action = Input;
    fn update(
        &mut self,
        state: &mut usize,
        action: &Input,
        context: &mut local::FeatureContext<'_, usize, Input>,
    ) {
        self.child.events.observe(1);
        self.child.cache += 1;
        *state += self.child.cache;
        let capture = Capture(Arc::clone(&self.child.events));
        context.task(local::Task::run(move |_| async move {
            drop(capture);
        }));
        assert_ne!(action.phase, Phase::ReducerPanic, "reducer panic");
        context
            .reducer_context()
            .report_issue(crate::Issue::new("lease", "callback"))
            .unwrap();
    }
}

/// Checks the completed turn before the caller polls its terminal reply.
fn assert_turn_result<T>(phase: Phase, events: &Events, result: std::thread::Result<T>) {
    assert_eq!(
        events.inputs_at_terminal_wake.load(Ordering::SeqCst),
        1,
        "input destruction precedes terminal wake, including rejection"
    );

    let expected_panic = match phase {
        Phase::Success | Phase::Missing => {
            drop(result.unwrap());
            assert_eq!(events.count(3), 0, "Feature restored before terminal reply");
            return;
        }
        Phase::SelectorPanic => "assertion `left != right` failed: selector panic",
        Phase::ReducerPanic => "assertion `left != right` failed: reducer panic",
        Phase::HandlerPanic => "assertion `left != right` failed: issue handler panic",
        Phase::InputDropPanic => "input drop panic",
    };
    let error = result.err().expect("the selected callback must panic");
    assert_eq!(panic_message(&*error).lines().next(), Some(expected_panic));
    assert_eq!(events.count(3), usize::from(phase != Phase::InputDropPanic));

    if phase == Phase::InputDropPanic {
        assert_eq!(
            events.restored_at_input_drop.load(Ordering::SeqCst),
            1,
            "Feature restored before input destruction"
        );
        assert_eq!(
            events.captures_at_terminal_wake.load(Ordering::SeqCst),
            1,
            "context capture drops before terminal wake"
        );
    }
}

// Only carrier construction differs. Every case still calls Host::next, which
// borrows the real production transaction; no reduction/commit logic is copied.
macro_rules! witnesses {
    ($name:ident, $carrier:ty, $driver:expr, $node:ident, $feature:expr, $weak:expr, $available:expr, $context:expr) => {
        #[test]
        fn $name() {
            // Catches reply publication before context cleanup when input destruction panics.
            for phase in [
                Phase::Success,
                Phase::Missing,
                Phase::SelectorPanic,
                Phase::ReducerPanic,
                Phase::HandlerPanic,
                Phase::InputDropPanic,
            ] {
                let events = Arc::new(Events::default());
                let mut slot = ScopeSlot::new();
                let reduce = move |state: &mut State, _: &(), context: &mut ReduceContext<'_>| {
                    state.token = Some(context.mount(
                        ReduceContext::node(&mut slot),
                        ScopeIdentity::new(()),
                    ));
                };
                let mut host = Host::<State, (), $carrier>::new(
                    State {
                        child: 0,
                        token: None,
                        phase,
                        events: Arc::clone(&events),
                    },
                    ($driver)(reduce),
                );
                drop(host.action(()));
                let token = host.state.token.unwrap();
                let node = $node::new(
                    ($feature)(Child {
                        cache: 0,
                        events: Arc::clone(&events),
                    }),
                    token,
                    0,
                );
                let weak = ($weak)(&node.owned);
                AVAILABLE.with_borrow_mut(|slot| *slot = Some(Rc::new(move || ($available)(&weak))));
                let child = ($context)(&node, &host);
                host.mounted.push(Box::new(node));

                let handler_events = Arc::clone(&events);
                host.issues = crate::IssueReporter::new(move |_| {
                    handler_events.observe(2);
                    assert_ne!(phase, Phase::HandlerPanic, "issue handler panic");
                });
                let waker = Waker::from(Arc::clone(&events));
                let mut cx = Context::from_waker(&waker);
                let mut send = Box::pin(child.send(Input {
                    phase,
                    events: Arc::clone(&events),
                }));
                assert!(send.as_mut().poll(&mut cx).is_pending());
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.next()));
                assert_turn_result(phase, &events, result);
                match phase {
                    Phase::Success => {
                        assert!(matches!(send.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
                    }
                    Phase::Missing => assert!(matches!(
                        send.as_mut().poll(&mut cx),
                        Poll::Ready(Err(TaskSendError::Failed { error }))
                            if error.reason() == TaskAccessReason::ScopeEnded
                                && error.progress() == TaskProgress::MayHaveRun
                    )),
                    _ => assert!(matches!(
                        send.as_mut().poll(&mut cx),
                        Poll::Ready(Err(TaskSendError::Failed { error }))
                            if error.progress() == TaskProgress::MayHaveRun
                    )),
                }

                if matches!(phase, Phase::Success | Phase::Missing) {
                    // Both success and absent projection must restore the same
                    // cached Feature, without retrying its previous action.
                    host.state.phase = Phase::Success;
                    let mut next = Box::pin(child.send(Input {
                        phase: Phase::Success,
                        events: Arc::clone(&events),
                    }));
                    assert!(next.as_mut().poll(&mut cx).is_pending());
                    drop(host.next());
                    assert!(matches!(next.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
                    assert_eq!(host.state.child, if phase == Phase::Missing { 1 } else { 3 });
                }

                events.assert_input_cleanup(phase);
                drop(host);
                assert_eq!(events.count(3), 1, "Feature drops exactly once");
                assert_eq!(
                    events.guarded.load(Ordering::SeqCst),
                    0,
                    "no callback or cleanup runs guarded: {phase:?}"
                );
                AVAILABLE.with_borrow_mut(|slot| *slot = None);
            }
        }
    };
}

/// Adapts root setup to the Shared production transaction driver.
fn shared_driver(
    mut reduce: impl FnMut(&mut State, &(), &mut ReduceContext<'_>) + Send + 'static,
) -> Box<<Shared as Carriers<State, ()>>::Driver> {
    Box::new(Reduce::new(
        move |state: &mut State, action: &(), context: &mut ReduceContext<'_>| {
            reduce(state, action, context);
            crate::Effect::none()
        },
    ))
}
#[cfg(feature = "local")]
/// Adapts root setup without imposing Send on the Local closure.
fn local_driver(
    mut reduce: impl FnMut(&mut State, &(), &mut ReduceContext<'_>) + 'static,
) -> Box<<local::Local as Carriers<State, ()>>::Driver> {
    Box::new(local::Driver(local::Reduce::new(
        move |state: &mut State, action: &(), context: &mut ReduceContext<'_>| {
            reduce(state, action, context);
            local::Effect::none()
        },
    )))
}
witnesses!(
    shared_feature_lease_callbacks_and_unwind,
    Shared,
    shared_driver,
    SharedNode,
    |child| child,
    Arc::downgrade,
    |weak: &Weak<SharedInstalled<Child>>| match weak.upgrade() {
        Some(node) => node.node.try_lock().map(|node| node.feature.is_some()),
        None => Some(false),
    },
    |node: &SharedNode<Child>, host: &Host<State, (), Shared>| node
        .downgrade()
        .context::<State, ()>(
            OptionalPath::new(|root| Some(&root.child), select),
            std::sync::Weak::clone(&host.weak),
            &host.scopes
        )
);
#[cfg(feature = "local")]
witnesses!(
    local_feature_lease_callbacks_and_unwind,
    local::Local,
    local_driver,
    LocalNode,
    |child| LocalChild {
        child,
        _local: Rc::new(())
    },
    Rc::downgrade,
    |weak: &std::rc::Weak<LocalInstalled<LocalChild>>| match weak.upgrade() {
        Some(node) => node
            .node
            .try_borrow_mut()
            .ok()
            .map(|node| node.feature.is_some()),
        None => Some(false),
    },
    |node: &LocalNode<LocalChild>, host: &Host<State, (), local::Local>| node.context::<State, ()>(
        OptionalPath::new(|root| Some(&root.child), select),
        std::sync::Weak::clone(&host.weak),
        Provenance::External { handle: None },
        host.scopes.authority(host.state.token.unwrap())
    )
);
