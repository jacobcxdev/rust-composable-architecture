//! Checks callback guards and capture ownership through normal return, panic and retirement.

ordinary_test_items! {
    use super::*;
    use std::cell::Cell as Counter;
    use std::rc::Rc;

    /// Shares guard observations without panicking inside a destructor.
    type Check = Rc<GuardCheck>;

    /// Records every guard check and reports a failure after cleanup.
    struct GuardCheck {
        /// Returns whether the callback storage is currently available.
        available: Box<dyn Fn() -> bool>,
        /// Retains an earlier failed check even after the guard is released.
        all_available: Counter<bool>,
    }

    impl GuardCheck {
        /// Starts with no failed checks for one callback storage cell.
        fn new(available: impl Fn() -> bool + 'static) -> Self {
            Self {
                available: Box::new(available),
                all_available: Counter::new(true),
            }
        }

        /// Records the current guard state without introducing an unwind panic.
        fn record(&self) {
            let available = (self.available)();
            self.all_available.set(self.all_available.get() && available);
        }

        /// Requires every observed guard, including the final state, to be available.
        fn assert_available(&self) {
            self.record();
            assert!(self.all_available.get(), "callback storage guard remained held");
        }
    }

    /// Selects an owned value before or after the mutation.
    type Select = Box<dyn FnMut(&usize) -> Value>;
    /// Consumes old and new values outside the callback guard.
    type Factory = Box<dyn for<'a> FnMut(ReactionChange<Value>, ReduceContext<'a>)>;
    /// Stores the selector and reaction factory as one leased payload.
    type Callbacks = (Select, Factory);

    /// Carries a selected value, a failure phase and the shared guard probe.
    struct Value {
        current: usize,
        phase: usize,
        check: Check,
    }

    impl PartialEq for Value {
        fn eq(&self, other: &Self) -> bool {
            self.check.record();
            assert!(self.phase != 2, "equality panic");
            self.current == other.current
        }
    }

    impl Drop for Value {
        fn drop(&mut self) {
            self.check.record();
            if !std::thread::panicking() {
                assert!(!(self.phase == 4 && self.current == 4), "old value panic");
            }
        }
    }

    /// Checks guard availability and counts destruction of one retained callback.
    struct CallbackProbe(Check, Rc<Counter<usize>>);

    impl CallbackProbe {
        /// Records callback entry separately from eventual capture destruction.
        fn check(&self) {
            self.0.record();
        }
    }

    impl Drop for CallbackProbe {
        fn drop(&mut self) {
            self.0.record();
            self.1.set(self.1.get() + 1);
        }
    }

    /// Runs one initial or changed reaction and checks its exact failure phase.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "the fixture owns the callback cell and probe through the final destruction checks"
    )]
    fn exercise<C: Cell<Callbacks>>(cell: C, check: Check, phase: usize, initial: bool) {
        let drops = Rc::new(Counter::new(0));
        let select_probe = CallbackProbe(std::rc::Rc::clone(&check), std::rc::Rc::clone(&drops));
        let factory_probe = CallbackProbe(std::rc::Rc::clone(&check), std::rc::Rc::clone(&drops));
        let selected_check = std::rc::Rc::clone(&check);
        let mut calls = 0;
        let select: Select = Box::new(move |value| {
            select_probe.check();
            calls += 1;
            assert!(
                !((phase == 0 && calls == 1) || (phase == 1 && calls == 2)),
                "selector panic"
            );
            Value {
                current: *value,
                phase,
                check: std::rc::Rc::clone(&selected_check),
            }
        });
        let factory: Factory = Box::new(move |change, _| {
            factory_probe.check();
            assert!(phase != 3, "factory panic");
            drop(change);
        });

        cell.restore(&mut Some((select, factory)));
        let registration = Registration::<usize, (), Value, Select, Factory, C> {
            callbacks: cell.clone(),
            initial: if initial {
                InitialPolicy::Run
            } else {
                InitialPolicy::Skip
            },
            domain: PhantomData,
        };

        let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut dependencies = crate::DependencyValues::new();
        let mut transitions = super::super::scope::ScopeTransitionBatch::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let context = ReduceContext::new(
                &mut dependencies,
                crate::MonotonicInstant::ZERO,
                &mut scopes,
                &mut transitions,
            );
            if initial {
                assert!(registration.runs_initial());
                registration.initial(&4, context);
            } else {
                assert!(!registration.runs_initial());
                let captured = registration.capture(&4, &|| true).unwrap();
                captured(&5, context);
            }
        }));

        drop(registration);
        assert_eq!(drops.get(), 2, "both callback destructors run exactly once");
        assert!(cell.take().is_none());
        check.assert_available();

        let expected_panic = match phase {
            0 | 1 => Some("selector panic"),
            2 => Some("equality panic"),
            3 => Some("factory panic"),
            4 => Some("old value panic"),
            _ => None,
        };
        match result {
            Ok(()) => assert_eq!(expected_panic, None),
            Err(payload) => {
                let expected = expected_panic.expect("the normal scenario must not panic");
                assert_eq!(
                    payload.downcast_ref::<&str>().copied(),
                    Some(expected),
                    "reaction failed in an unexpected phase"
                );
            }
        }
    }

    #[test]
    fn shared_reaction_callbacks_equality_and_destruction_never_hold_callback_mutex() {
        for (initial, phase) in (0..=5)
            .map(|phase| (false, phase))
            .chain([0, 3, 4, 5].map(|phase| (true, phase)))
        {
            #[expect(
                clippy::arc_with_non_send_sync,
                reason = "exercise the Shared callback carrier with local destructor probes"
            )]
            let cell = Arc::new(Mutex::new(None::<Callbacks>));
            let checked = std::sync::Arc::clone(&cell);
            exercise(
                cell,
                Rc::new(GuardCheck::new(move || checked.try_lock().is_some())),
                phase,
                initial,
            );
        }
    }

    /// Retires callbacks while saved history and a strong header remain alive.
    fn retire_with_saved_capture<C: Cell<Callbacks>>(cell: &C, check: &Check) {
        let drops = Rc::new(Counter::new(0));
        let select_probe = CallbackProbe(Rc::clone(check), Rc::clone(&drops));
        let factory_probe = CallbackProbe(Rc::clone(check), Rc::clone(&drops));
        let selected_check = Rc::clone(check);
        let select: Select = Box::new(move |value| {
            select_probe.check();
            Value {
                current: *value,
                phase: 5,
                check: Rc::clone(&selected_check),
            }
        });
        let factory: Factory = Box::new(move |_, _| {
            factory_probe.check();
            panic!("a retired registration cannot run its factory");
        });
        cell.restore(&mut Some((select, factory)));
        let registration = Registration::<usize, (), Value, Select, Factory, C> {
            callbacks: cell.clone(),
            initial: InitialPolicy::Skip,
            domain: PhantomData,
        };

        let captured = registration.capture(&4, &|| true).unwrap();
        let weak = cell.downgrade();
        let upgraded = C::upgrade(&weak).unwrap();
        assert_eq!(drops.get(), 0);

        drop(registration);
        assert_eq!(drops.get(), 2, "retirement drops both callbacks exactly once");
        assert!(
            upgraded.take().is_none(),
            "a live header cannot retain callbacks"
        );
        assert_eq!(
            Rc::strong_count(check),
            2,
            "only the saved value retains a probe"
        );

        drop(captured);
        assert_eq!(
            Rc::strong_count(check),
            1,
            "disposing of history releases its probe"
        );
        drop(upgraded);
        assert_eq!(drops.get(), 2);
        check.assert_available();
    }

    #[test]
    fn shared_retirement_drops_callbacks_before_saved_history_and_upgraded_header() {
        #[expect(
            clippy::arc_with_non_send_sync,
            reason = "exercise the Shared callback carrier with local destructor probes"
        )]
        let cell = Arc::new(Mutex::new(None::<Callbacks>));
        let checked = Arc::clone(&cell);
        let check: Check = Rc::new(GuardCheck::new(move || checked.try_lock().is_some()));
        retire_with_saved_capture(&cell, &check);
    }
}

#[cfg(feature = "local")]
#[test]
fn local_reaction_callbacks_equality_and_destruction_never_hold_callback_borrow() {
    for (initial, phase) in (0..=5)
        .map(|phase| (false, phase))
        .chain([0, 3, 4, 5].map(|phase| (true, phase)))
    {
        let cell = Rc::new(std::cell::RefCell::new(None::<Callbacks>));
        let checked = std::rc::Rc::clone(&cell);
        exercise(
            cell,
            Rc::new(GuardCheck::new(move || checked.try_borrow_mut().is_ok())),
            phase,
            initial,
        );
    }
}

#[cfg(feature = "local")]
#[test]
fn local_retirement_drops_callbacks_before_saved_history_and_upgraded_header() {
    let cell = Rc::new(std::cell::RefCell::new(None::<Callbacks>));
    let checked = Rc::clone(&cell);
    let check: Check = Rc::new(GuardCheck::new(move || checked.try_borrow_mut().is_ok()));
    retire_with_saved_capture(&cell, &check);
}
