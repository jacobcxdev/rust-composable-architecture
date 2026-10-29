//! Supplies owner-thread storage to the common observation registry.
//!
//! Projections, policies and values may contain `Rc` or other non-`Send` data.
//! They remain on the Local actor; shared signals contain only wake and version
//! metadata. Publication, typed takes and teardown use the common implementation.

use std::sync::Arc;

use super::{
    ErasedObservation, ObservationId, ObservationRegistryCore, ObservationSelection,
    ObservationSignal, PublishPolicyCore, RegisterObservation, RegistrationReply, ScopeToken,
    SelectedChild, TakeReply, WeakHandleScope,
};

/// Projects an owner-local value without borrowing root state in the result.
type Selector<State, Value> = Box<dyn Fn(&State) -> Value>;
/// Owns a local comparison callback that can update its previous-value slot.
type DistinctDecision<Value> = Box<dyn Fn(&mut Option<Value>, &Value) -> bool>;

/// Keeps non-transferable entries exclusively on their Local actor.
pub(crate) type ObservationRegistry<State> =
    ObservationRegistryCore<State, dyn ErasedObservation<State>>;
/// Selects publication rules without requiring transferable values or callbacks.
pub(crate) type PublishPolicy<Value> = PublishPolicyCore<Value, DistinctDecision<Value>>;

impl<Value> PublishPolicy<Value> {
    /// Owns a local comparison callback that must accept the initial value.
    pub(crate) fn distinct_by(
        decide: impl Fn(&mut Option<Value>, &Value) -> bool + 'static,
    ) -> Self {
        Self::Distinct {
            previous: None,
            decide: Box::new(decide),
        }
    }
}

impl<Value: Clone + PartialEq + 'static> PublishPolicy<Value> {
    /// Suppresses equal values while retaining one owner-local comparison clone.
    pub(crate) fn distinct() -> Self {
        Self::distinct_by(super::distinct)
    }
}

impl<State: 'static> ObservationRegistry<State> {
    /// Takes a Local value through the common typed-slot implementation.
    pub(super) fn take<Value: 'static>(&mut self, id: ObservationId) -> TakeReply<Value> {
        self.take_entry::<Value>(id)
    }
}

/// Runs a non-transferable observation operation inside its Local actor.
pub(crate) trait ObservationCommand<State> {
    /// Consumes the command with current state and an actor-borrowed root or validated child view.
    fn execute(
        self: Box<Self>,
        state: &State,
        registry: &mut ObservationRegistry<State>,
        commit_version: u64,
        scope: ScopeToken,
        handle: Option<WeakHandleScope>,
        validation: &SelectedChild<'_>,
    );
}

impl<State: 'static, Value: 'static, Select> ObservationCommand<State>
    for RegisterObservation<Value, Select, DistinctDecision<Value>>
where
    Select: ObservationSelection<State, Value> + 'static,
{
    fn execute(
        self: Box<Self>,
        state: &State,
        registry: &mut ObservationRegistry<State>,
        commit_version: u64,
        scope: ScopeToken,
        handle: Option<WeakHandleScope>,
        validation: &SelectedChild<'_>,
    ) {
        (*self).register(
            state,
            registry,
            commit_version,
            scope,
            handle,
            validation,
            |entry| Box::new(entry),
        );
    }
}

/// Prepares a Local root projection that publishes every selected value.
pub(crate) fn new_registration<State: 'static, Value: 'static>(
    selector: impl Fn(&State) -> Value + 'static,
    signal: Arc<ObservationSignal>,
) -> (
    Box<dyn ObservationCommand<State>>,
    futures_channel::oneshot::Receiver<RegistrationReply<Value>>,
) {
    new_registration_with_policy(selector, signal, PublishPolicy::always())
}

/// Prepares a Local root projection that suppresses consecutive equal values.
pub(crate) fn new_distinct_registration<State: 'static, Value: Clone + PartialEq + 'static>(
    selector: impl Fn(&State) -> Value + 'static,
    signal: Arc<ObservationSignal>,
) -> (
    Box<dyn ObservationCommand<State>>,
    futures_channel::oneshot::Receiver<RegistrationReply<Value>>,
) {
    new_registration_with_policy(selector, signal, PublishPolicy::distinct())
}

/// Boxes an owner-local projection with its selected publication policy.
pub(crate) fn new_registration_with_policy<State: 'static, Value: 'static>(
    selector: impl Fn(&State) -> Value + 'static,
    signal: Arc<ObservationSignal>,
    policy: PublishPolicy<Value>,
) -> (
    Box<dyn ObservationCommand<State>>,
    futures_channel::oneshot::Receiver<RegistrationReply<Value>>,
) {
    let (reply, receive) = futures_channel::oneshot::channel();
    let selector: Selector<State, Value> = Box::new(selector);
    (
        Box::new(RegisterObservation {
            selector,
            policy,
            signal,
            reply,
        }),
        receive,
    )
}

#[cfg(all(test, feature = "local"))]
ordinary_test_items! {
mod tests {
    use std::cell::{Cell, RefCell};
    use std::marker::PhantomData;
    use std::rc::Rc;
    use std::sync::{Arc, Weak};
    use std::task::{Context, Poll, Wake, Waker};

    use futures_util::FutureExt as _;

    use super::*;
    use crate::observation::local::Observation;
    use crate::observation::{TakeFuture, TakeOperation, TakeOutcome};
    use crate::store::Local;
    use crate::store::scope::{HandleRegistry, ScopeRegistry};

    #[test]
    fn checked_local_observation_keeps_rc_values_and_rejects_changed_identity() {
        use crate::store::observation::CheckedSelector;
        use crate::store::scope::{
            HandleRegistrationCommandCore, HandleRegistryCore, HandleSelector, HandleSignal,
            ScopeIdentity, TypedHandleSelector,
        };

        let selected = Rc::new(Cell::new(0_usize));
        let identity_selection = Rc::clone(&selected);
        let mut handles: HandleRegistryCore<dyn HandleSelector<[Rc<usize>; 2]>> =
            HandleRegistryCore::new();
        let handle_signal = HandleSignal::new(Weak::new());
        let (reply, receive) = async_channel::bounded(1);
        let mut state = [Rc::new(7), Rc::new(8)];
        handles.register(
            &state,
            HandleRegistrationCommandCore {
                identity: Box::new(TypedHandleSelector::new(move |state: &[Rc<usize>; 2]| {
                    let index = identity_selection.get();
                    Some((ScopeIdentity::new(index), &state[index]))
                })),
                signal: Arc::clone(&handle_signal),
                reply,
            },
        );
        let handle = receive.try_recv().unwrap().unwrap();
        let projections = Rc::new(Cell::new(0));
        let projected = Rc::clone(&projections);
        let selector = CheckedSelector::new(move |_: &[Rc<usize>; 2], selected| {
            let child = selected.downcast::<Rc<usize>>();
            projected.set(projected.get() + 1);
            Rc::clone(child)
        });
        let signal = ObservationSignal::new(Weak::new());
        let (reply, receive) = futures_channel::oneshot::channel();
        let command = Box::new(RegisterObservation {
            selector,
            policy: PublishPolicy::always(),
            signal: Arc::clone(&signal),
            reply,
        });
        let mut registry = ObservationRegistry::new();
        handles.with_validation(&state, &handle, |validation| {
            command.execute(
                &state,
                &mut registry,
                0,
                ScopeRegistry::for_test().1.root(),
                Some(handle.downgrade()),
                validation,
            );
        });
        let initial = receive.now_or_never().unwrap().unwrap();
        assert_eq!(*initial.initial, 7);
        state[0] = Rc::new(9);
        registry.publish(&state, 1, &mut handles);
        assert!(matches!(
            registry.take::<Rc<usize>>(initial.id),
            TakeReply::Value { version: 1, value } if *value == 9
        ));
        selected.set(1);
        registry.publish(&state, 2, &mut handles);
        assert!(matches!(
            registry.take::<Rc<usize>>(initial.id),
            TakeReply::Closed
        ));
        assert!(handle.is_ended());
        assert!(!signal.is_active());
        assert_eq!(projections.get(), 2);
    }

    struct RegistryTake<State, Value> {
        registry: Rc<RefCell<ObservationRegistry<State>>>,
        id: ObservationId,
        value: PhantomData<fn() -> Value>,
    }

    impl<State: 'static, Value: 'static> TakeOperation<Value, Local>
        for RegistryTake<State, Value>
    {
        fn take(self: Box<Self>) -> TakeFuture<Value, Local> {
            let reply = self.registry.borrow_mut().take(self.id);
            let (send, receive) = futures_channel::oneshot::channel();
            let _ = send.send(TakeOutcome::<Value, Local> {
                reply,
                operation: self,
            });
            TakeFuture::Receiving { receive }
        }
    }

    fn register<State: 'static, Value: 'static>(
        state: &State,
        selector: impl Fn(&State) -> Value + 'static,
        policy: PublishPolicy<Value>,
    ) -> (
        Rc<RefCell<ObservationRegistry<State>>>,
        Observation<Value>,
        Arc<ObservationSignal>,
    ) {
        let signal = ObservationSignal::new(Weak::new());
        let mut registry = ObservationRegistry::new();
        let (command, reply) = new_registration_with_policy(selector, Arc::clone(&signal), policy);
        command.execute(
            state,
            &mut registry,
            0,
            ScopeRegistry::for_test().1.root(),
            None,
            &SelectedChild::root(state),
        );
        let reply = reply.now_or_never().unwrap().unwrap();
        let registry = Rc::new(RefCell::new(registry));
        let observation = Observation::new(
            reply.id,
            Arc::clone(&signal),
            reply.initial,
            reply.version,
            Box::new(RegistryTake {
                registry: Rc::clone(&registry),
                id: reply.id,
                value: PhantomData,
            }),
        );
        (registry, observation, signal)
    }

    struct OwnerDrop {
        owner: std::thread::ThreadId,
        drops: Rc<Cell<usize>>,
    }

    impl Drop for OwnerDrop {
        fn drop(&mut self) {
            assert_eq!(std::thread::current().id(), self.owner);
            self.drops.set(self.drops.get() + 1);
        }
    }

    #[test]
    fn local_selector_equality_and_value_drops_stay_on_owner() {
        let owner = std::thread::current().id();
        let selector_drops = Rc::new(Cell::new(0));
        let value_drops = Rc::new(Cell::new(0));
        let equality_calls = Rc::new(Cell::new(0));
        let selector_guard = OwnerDrop {
            owner,
            drops: Rc::clone(&selector_drops),
        };
        let values = Rc::clone(&value_drops);
        let equalities = Rc::clone(&equality_calls);
        let policy = PublishPolicy::distinct_by(move |previous, value: &(usize, OwnerDrop)| {
            assert_eq!(std::thread::current().id(), owner);
            equalities.set(equalities.get() + 1);
            // This policy deliberately does not retain a clone of the value.
            assert!(previous.is_none());
            value.0.is_multiple_of(2)
        });
        let (registry, mut observation, _) = register(
            &0,
            move |value: &usize| {
                let _ = &selector_guard;
                assert_eq!(std::thread::current().id(), owner);
                (
                    *value,
                    OwnerDrop {
                        owner,
                        drops: Rc::clone(&values),
                    },
                )
            },
            policy,
        );
        drop(observation.next().now_or_never().unwrap().unwrap());
        registry.borrow_mut().publish(&1, 1, &mut HandleRegistry::new());
        registry.borrow_mut().publish(&2, 2, &mut HandleRegistry::new());
        registry.borrow_mut().publish(&4, 3, &mut HandleRegistry::new());
        assert_eq!(
            value_drops.get(),
            3,
            "initial, rejected and replaced values dropped on owner",
        );
        assert_eq!(equality_calls.get(), 4);
        assert_eq!(observation.next().now_or_never().unwrap().unwrap().0, 4);
        assert_eq!(value_drops.get(), 4);
        drop(observation);
        registry.borrow_mut().remove_inactive();
        assert!(registry.borrow().is_empty());
        assert_eq!(selector_drops.get(), 1);
        assert_eq!(Rc::strong_count(&equality_calls), 1);
    }

    #[test]
    fn local_distinct_registration_keeps_latest_changed_rc_value() {
        let signal = ObservationSignal::new(Weak::new());
        let mut registry = ObservationRegistry::new();
        let (command, reply) = new_distinct_registration(
            |state: &Rc<usize>| Rc::clone(state),
            Arc::clone(&signal),
        );
        let state = Rc::new(3);
        command.execute(
            &state,
            &mut registry,
            0,
            ScopeRegistry::for_test().1.root(),
            None,
            &SelectedChild::root(&state),
        );
        let reply = reply.now_or_never().unwrap().unwrap();
        registry.publish(&Rc::new(3), 1, &mut HandleRegistry::new());
        assert!(matches!(
            registry.take::<Rc<usize>>(reply.id),
            TakeReply::Empty { version: 0 },
        ));
        registry.publish(&Rc::new(7), 2, &mut HandleRegistry::new());
        assert!(matches!(
            registry.take::<Rc<usize>>(reply.id),
            TakeReply::Value { version: 2, value } if *value == 7
        ));
    }

    #[test]
    fn local_on_change_reenters_registry_after_cancelling_registration() {
        let (registry, observation, signal) = register(
            &0,
            |value: &usize| Rc::new(*value),
            PublishPolicy::always(),
        );
        registry.borrow_mut().publish(&1, 1, &mut HandleRegistry::new());
        let entered = Rc::new(Cell::new(false));
        let observed = Rc::clone(&entered);
        let registry_in_callback = Rc::clone(&registry);
        assert_eq!(
            observation
                .on_change(move |value| {
                    assert!(!signal.is_active());
                    assert_eq!(*value, 1);
                    registry_in_callback.borrow_mut().remove_inactive();
                    assert!(registry_in_callback.borrow().is_empty());
                    observed.set(true);
                })
                .now_or_never(),
            Some(true),
        );
        assert!(entered.get());
    }

    #[test]
    fn local_on_change_panic_retires_registration_and_drops_capture() {
        let (registry, observation, signal) = register(
            &0,
            |value: &usize| Rc::new(*value),
            PublishPolicy::always(),
        );
        registry.borrow_mut().publish(&1, 1, &mut HandleRegistry::new());
        let drops = Rc::new(Cell::new(0));
        let guard = OwnerDrop {
            owner: std::thread::current().id(),
            drops: Rc::clone(&drops),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observation
                .on_change(move |_| {
                    let _ = &guard;
                    panic!("local callback panic witness");
                })
                .now_or_never()
        }));
        result.as_ref().expect_err("local callback must panic");
        assert!(!signal.is_active());
        assert_eq!(drops.get(), 1);
        registry.borrow_mut().remove_inactive();
        assert!(registry.borrow().is_empty());
    }

    #[test]
    fn local_metadata_wake_crosses_threads_without_payload_or_polling() {
        struct CountWake(std::sync::atomic::AtomicUsize);

        impl Wake for CountWake {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let (registry, mut observation, signal) = register(
            &0,
            |value: &usize| Rc::new(*value),
            PublishPolicy::always(),
        );
        assert_eq!(observation.next().now_or_never().unwrap().as_deref(), Some(&0));
        let wakes = Arc::new(CountWake(std::sync::atomic::AtomicUsize::new(0)));
        let task_waker = Waker::from(Arc::clone(&wakes));
        let mut cx = Context::from_waker(&task_waker);
        let mut next = Box::pin(observation.next());
        assert!(matches!(next.as_mut().poll(&mut cx), Poll::Pending));
        std::thread::spawn(move || signal.deactivate())
            .join()
            .unwrap();
        assert!(wakes.0.load(std::sync::atomic::Ordering::Relaxed) > 0);
        assert!(matches!(next.as_mut().poll(&mut cx), Poll::Ready(None)));
        drop(next);
        registry.borrow_mut().remove_inactive();
        assert!(registry.borrow().is_empty());
    }
}
}
