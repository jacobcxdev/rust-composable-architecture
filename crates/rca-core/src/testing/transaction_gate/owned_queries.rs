//! Owned acquisition/Fold requests share the bounded diagnostic slot.
use super::*;
use std::task::{Context, Poll, Waker};

fn diagnostics() -> ActorDiagnostics {
    ActorDiagnostics::new(
        RuntimeSnapshot {
            commit_version: 0,
            live_task_ids: Vec::new(),
            cancellation_count: 0,
            timer_deadlines: Vec::new(),
            observation_count: 0,
        },
        Vec::new(),
        0,
    )
}

struct Probe<C: Carriers<(), ()>> {
    gate: TransactionGateCore<(), (), C>,
    rejected: Arc<Mutex<Vec<TaskAccessReason>>>,
}
impl<C: Carriers<(), ()>> Probe<C> {
    fn reject(self, error: TaskAccessError) {
        assert!(
            self.gate.shared.state.try_lock().is_ok(),
            "terminal callback outside gate lock"
        );
        assert_eq!(error.progress(), TaskProgress::NotStarted);
        self.rejected.lock().unwrap().push(error.reason());
    }
}
impl<C: Carriers<(), ()>> AcquireCall for Probe<C> {
    fn execute(
        self: Box<Self>,
        _: &crate::store::scope::ScopeRegistry,
        _: &crate::store::TestLifecycle,
    ) {
        panic!("retired acquisition must not execute");
    }
    fn reject(self: Box<Self>, error: TaskAccessError) {
        (*self).reject(error);
    }
}
impl<C: Carriers<(), ()>> FoldCall<(), (), C> for Probe<C> {
    fn execute(
        self: Box<Self>,
        (): &(),
        _: &crate::store::scope::ScopeRegistry,
        _: &[Box<C::Mount>],
        _: &crate::store::TestLifecycle,
    ) {
        panic!("retired fold must not execute");
    }
    fn reject(self: Box<Self>, error: TaskAccessError) {
        (*self).reject(error);
    }
}

macro_rules! owned_queries {
    ($name:ident, $family:ty, $leaf:ident, $submit:ident) => {
        #[test]
        fn $name() {
            let gate = TransactionGateCore::<(), (), $family>::new();
            let binding = gate.attach().unwrap();
            gate.begin(Pending::Structural, diagnostics(), false);
            let boundary = gate.boundary().unwrap();
            let rejected = Arc::new(Mutex::new(Vec::new()));
            let call = || -> Box<<$family as Carriers<(), ()>>::$leaf> {
                Box::new(Probe {
                    gate: gate.clone(),
                    rejected: rejected.clone(),
                })
            };
            let mut cx = Context::from_waker(Waker::noop());
            let query: Box<<$family as Carriers<(), ()>>::Query> = Box::new(|_: &()| {});
            let mut occupy = Box::pin(gate.admit_query(Some(boundary), GateQuery::State(query)));
            assert!(matches!(occupy.as_mut().poll(&mut cx), Poll::Ready(None)));
            let mut waiting = Box::pin(gate.$submit(boundary, call()));
            assert!(waiting.as_mut().poll(&mut cx).is_pending());
            assert_eq!(gate.shared.state.lock().unwrap().state_assertions.len(), 1);
            drop(waiting);
            assert_eq!(*rejected.lock().unwrap(), [TaskAccessReason::StoreClosed]);
            assert_eq!(gate.boundary(), Some(boundary));
            drop(gate.take_query().unwrap());

            let mut queued = Box::pin(gate.$submit(boundary, call()));
            assert!(queued.as_mut().poll(&mut cx).is_ready());
            assert_eq!(gate.shared.state.lock().unwrap().state_assertions.len(), 1);
            gate.clear();
            assert_eq!(
                *rejected.lock().unwrap(),
                [TaskAccessReason::StoreClosed, TaskAccessReason::StoreClosed]
            );
            gate.begin(Pending::Structural, diagnostics(), false);
            let next = gate.boundary().unwrap();
            assert_ne!(boundary, next);
            let mut stale = Box::pin(gate.$submit(boundary, call()));
            assert!(stale.as_mut().poll(&mut cx).is_ready());
            assert_eq!(
                *rejected.lock().unwrap(),
                [
                    TaskAccessReason::StoreClosed,
                    TaskAccessReason::StoreClosed,
                    TaskAccessReason::PhaseDenied
                ]
            );
            assert_eq!(gate.boundary(), Some(next));
            assert!(gate.take_query().is_none(), "lost boundary never retargets");
            drop(binding);
        }
    };
}

owned_queries!(
    shared_acquire_retirement_and_lost_boundary,
    Shared,
    Acquire,
    acquire
);
owned_queries!(shared_fold_retirement_and_lost_boundary, Shared, Fold, fold);
#[cfg(feature = "local")]
owned_queries!(
    local_acquire_retirement_and_lost_boundary,
    crate::store::Local,
    Acquire,
    acquire
);
#[cfg(feature = "local")]
owned_queries!(
    local_fold_retirement_and_lost_boundary,
    crate::store::Local,
    Fold,
    fold
);
