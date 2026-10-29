//! One concrete orchestration source for portable and owner-local test stores.

/// Expands nominal ownership and transfer bounds without changing assertion policy.
macro_rules! test_store_api {
    ($core:ident, [$($send:ident)?], [$($sync:ident)?], $clock_example:literal, $renderer_example:literal) => {
        /// Stores heterogeneous configuration steps in their declaration order.
        /// Each step consumes its captures once after the deterministic clock exists.
        type ConfigureDependency<State, Action, R> = Box<
            dyn FnOnce(StoreBuilder<State, Action, R>, Clock) -> StoreBuilder<State, Action, R>
                $(+ $send)?,
        >;

        /// Tests one feature using production Store kernel and deterministic scheduling.
        ///
        /// Every send and receive compares an independent snapshot. Existing constructors
        /// compare full State using its `Clone` and `PartialEq` implementations; projected
        /// constructors compare only the selected snapshot. Clones must not share mutable
        /// data that would change historical expectations.
        ///
        /// Normal Drop checks unfinished assertions and RCA-owned work within the poll
        /// budget, without advancing time or waiting for external progress. It releases
        /// scheduler work before reporting failure. During unwinding it only cleans up.
        /// Prefer [`Self::finish`] for completion diagnostics at an explicit call site.
        ///
        /// Missing dependencies use [`rca_core::DependencyKey::test_value`], never an implicit live
        /// default. Inject values or implement deterministic test defaults. Explicit
        /// action, feature and Store-root values retain their existing precedence.
        ///
        /// # Panics
        ///
        /// Normal Drop panics when completion cannot be verified.
        pub struct TestStore<State: $($send +)? 'static, Action: $($send +)? 'static, Snapshot = State> {
            /// Production actor; assertions control its existing transaction gate.
            store: Store<State, Action>,
            /// Drives only this test's tasks and virtual time.
            runtime: DeterministicRuntime,
            /// Holds admitted mutations until their snapshot and capture obligations pass.
            gate: TransactionGate<State, Action>,
            /// Retains reported issues until the test explicitly consumes them.
            issues: IssueRecorder,
            /// Explicit omission warnings, separate from required production issues.
            omissions: Vec<Issue>,
            /// Controls explicit skipping, not snapshot or capture comparison.
            mode: Exhaustivity,
            /// Last verified values and any incomplete boundary obligations.
            assertions: Assertions<State, Snapshot>,
            /// Disables Drop verification only after every completion check succeeds.
            completed: bool,
            /// Records actor shutdown so a budget-limited cleanup can retry without repeating it.
            shutdown: bool,
        }

        /// Selects reducer or typed-feature construction without storing another closure.
        type FinishStore<State, Action, R> =
            fn(StoreBuilder<State, Action, R>) -> Result<Store<State, Action>, rca_core::StoreBuildError>;

        /// Configures one deterministic test store.
        ///
        /// Deferred constructors are owned but need not be shared. The portable builder
        /// is `Send` when its values are, but not `Sync`; the owner-local builder cannot
        /// move between threads. Built stores retain their corresponding inspection contract.
        pub struct TestStoreBuilder<State: $($send +)? 'static, Action: $($send +)? 'static, R, Snapshot = State> {
            /// Initial state, retained here until production actor construction.
            state: State,
            /// Reducer or typed feature selected by `construct` and `finish`.
            reducer: R,
            /// Deferred configuration, including dependency, mount and output declarations.
            dependencies: Vec<ConfigureDependency<State, Action, R>>,
            /// Requested effect-action accounting policy.
            mode: Exhaustivity,
            /// Maximum deterministic work per bounded driving operation.
            step_budget: usize,
            /// Initial snapshot and capture history, sampled before actor construction.
            assertions: Assertions<State, Snapshot>,
            /// Creates the matching production builder from owned state and behaviour.
            construct: fn(State, R, $core::Runtime) -> StoreBuilder<State, Action, R>,
            /// Finishes that builder using the same reducer or typed-feature path.
            finish: FinishStore<State, Action, R>,
        }

        impl<State, Action, R, Snapshot> TestStoreBuilder<State, Action, R, Snapshot>
        where
            State: $($send +)? 'static,
            Action: $($send +)? 'static,
            R: $($send +)? 'static,
        {
            /// Sets the production actor's bounded command mailbox capacity.
            ///
            /// Zero retains the production builder's invalid-capacity failure at `build`.
            #[must_use]
            pub fn mailbox_capacity(mut self, capacity: usize) -> Self {
                self.dependencies
                    .push(Box::new(move |builder, _| builder.mailbox_capacity(capacity)));
                self
            }

            /// Attaches an optional child with an explicit structural assertion boundary.
            ///
            /// Construction and Initial hooks run only when `receive_structural` releases
            /// the production startup transaction, never during this method or `build`.
            #[must_use]
            pub fn mount<Child, K>(self, descriptor: $core::OptionalMount<State, Child, K>) -> Self
            where
                Child: $core::Feature + $($send +)? 'static,
                Child::State: $($send +)? 'static,
                Child::Action: $($send +)? 'static,
                K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
            {
                self.mount_with_site(descriptor).0
            }

            /// Attaches one child and returns its weak production acquisition site.
            ///
            /// The builder owns the declaration immediately. Child construction and
            /// Initial hooks still wait for `receive_structural`, not `build`.
            pub fn mount_with_site<Child, K>(
                mut self,
                descriptor: $core::OptionalMount<State, Child, K>,
            ) -> (
                Self,
                $core::MountSite<State, Child::State, Child::Action, Action>,
            )
            where
                Child: $core::Feature + $($send +)? 'static,
                Child::State: $($send +)? 'static,
                Child::Action: $($send +)? 'static,
                K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
            {
                let (attach, site) = StoreBuilder::<State, Action, R>::defer_mount(descriptor);
                self.dependencies
                    .push(Box::new(move |builder, _| attach(builder)));
                (self, site)
            }

            /// Attaches a production child with its incarnation-owned recipe and callbacks.
            ///
            /// The builder owns the recipe now. Construction, discovery and Initial hooks
            /// wait for the structural transaction released by `receive_structural`.
            pub fn mount_with_children<Child, K>(
                mut self,
                descriptor: $core::OptionalMount<State, Child, K>,
                recipe: impl FnMut(
                    &Child::State,
                    &mut $core::MountChildren<'_, State, Action, Child::State, Child::Action>,
                ) + $($send +)? 'static,
            ) -> (
                Self,
                $core::MountSite<State, Child::State, Child::Action, Action>,
            )
            where
                Child: $core::Feature + $($send +)? 'static,
                Child::State: $($send +)? 'static,
                Child::Action: $($send +)? 'static,
                K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
            {
                let (attach, site) =
                    StoreBuilder::<State, Action, R>::defer_mount_with_children(descriptor, recipe);
                self.dependencies
                    .push(Box::new(move |builder, _| attach(builder)));
                (self, site)
            }

            /// Attaches one typed production output declaration without constructing a child.
            ///
            /// The returned output is weak. Dropping this builder releases its handler
            /// captures even when the output handle is retained.
            pub fn output<O: $($send +)? 'static>(
                mut self,
                source: &$core::MountSite<
                    State,
                    impl Sized + $($send +)? 'static,
                    impl Sized + $($send +)? 'static,
                    Action,
                >,
                configure: impl FnOnce(&mut $core::OutputHandlers<State, Action, O>),
            ) -> (Self, $core::Output<O>) {
                let (attach, output) = StoreBuilder::<State, Action, R>::defer_output(source, configure);
                self.dependencies
                    .push(Box::new(move |builder, _| attach(builder)));
                (self, output)
            }

            /// Declares an independently captured private or shared boundary obligation.
            ///
            /// Initial capture runs immediately. Configure external dependencies first.
            /// Ignored fields remain outside coverage; mutable aliases are not history.
            /// See [`CaptureKey`] for boundary-only accounting and external-writer limits.
            ///
            /// # Errors
            ///
            /// Returns duplicate declaration or the original typed capture failure.
            ///
            /// # Panics
            ///
            /// Propagates capture callback panics.
            #[track_caller]
            pub fn with_capture<K: CaptureKey>(
                mut self,
                capture: impl Fn(&State) -> Result<K::Value, K::Error> + $($send +)? $($sync +)? 'static,
            ) -> Result<Self, TestStoreError> {
                let declared = self.assertions
                    .declared
                    .declare::<K>(&self.state, capture, Location::caller());
                if let Err(error) = declared {
                    // Keep the original cause owned while the rejected builder releases captures.
                    drop(self);
                    return Err(error);
                }
                Ok(self)
            }

            /// Replaces the owned environment before selecting deterministic test defaults.
            ///
            /// Registrations apply in declaration order. Earlier explicit values are replaced;
            /// later overrides apply to this environment. Cached live defaults are discarded
            /// when the environment switches to Test; explicit values, including `None`, remain.
            #[must_use]
            pub fn with_dependency_values(mut self, values: DependencyValues) -> Self {
                self.dependencies.push(Box::new(move |builder, _| {
                    builder.with_dependency_values(values)
                }));
                self
            }

            /// Sets one production Store-root dependency.
            #[must_use]
            pub fn with_store_dependency<K: DependencyKey>(mut self, value: K::Value) -> Self {
                self.dependencies.push(Box::new(move |builder, _| {
                    builder.with_dependency::<K>(value)
                }));
                self
            }

            /// Sets one highest-priority test dependency.
            #[must_use]
            pub fn with_dependency<K: DependencyKey>(mut self, value: K::Value) -> Self {
                self.dependencies.push(Box::new(move |builder, _| {
                    builder.with_test_dependency::<K>(value)
                }));
                self
            }

            /// Constructs a highest-priority test dependency with this store's clock.
            ///
            /// Invokes `make` once during [`Self::build`], after the deterministic runtime
            /// exists. Registrations run in declaration order; the last test dependency
            /// for a key wins, including ordinary [`Self::with_dependency`] registrations.
            /// No clock dependency is installed implicitly. The owned factory need not
            /// support shared access: it is consumed on the thread that calls `build`.
            ///
            /// # Examples
            ///
            #[doc = $clock_example]
            #[must_use]
            pub fn with_clock_dependency<K: DependencyKey>(
                mut self,
                make: impl FnOnce(Clock) -> K::Value + $($send +)? 'static,
            ) -> Self {
                self.dependencies.push(Box::new(move |builder, clock| {
                    builder.with_test_dependency::<K>(make(clock))
                }));
                self
            }

            /// Customises snapshot mismatch rendering without changing equality or coverage.
            ///
            /// Called only for unequal snapshots, outside assertion locks. When configured,
            /// this replaces snapshot `Debug` rendering entirely, allowing secrets to be
            /// redacted. It does not redact runtime diagnostics or a renderer's own panic.
            /// The excerpt shows three context lines before and after the changed region,
            /// at most sixteen changed lines per side and 240 Unicode scalar values per
            /// displayed line. Initial renderer
            /// execution and its returned string allocation are not bounded. Runtime
            /// diagnostics retain their existing format, so the entire panic payload is
            /// not subject to these excerpt limits.
            ///
            /// # Examples
            ///
            #[doc = $renderer_example]
            ///
            /// # Panics
            ///
            /// A renderer panic propagates from a mismatching assertion. The pending
            /// assertion and last verified baseline are retained.
            #[must_use]
            pub fn with_snapshot_renderer(
                mut self,
                render: impl Fn(&Snapshot) -> String + $($send +)? $($sync +)? 'static,
            ) -> Self {
                self.assertions.render = Some(Box::new(render));
                self
            }

            /// Sets effect-action accounting without disabling snapshot comparisons.
            #[must_use]
            pub const fn with_exhaustivity(mut self, mode: Exhaustivity) -> Self {
                self.mode = mode;
                self
            }

            /// Sets the deterministic poll budget before runtime creation.
            ///
            /// # Panics
            ///
            /// Panics when `step_budget` is zero.
            #[must_use]
            pub fn with_step_budget(mut self, step_budget: usize) -> Self {
                assert!(
                    step_budget > 0,
                    "deterministic step budget must be positive"
                );
                self.step_budget = step_budget;
                self
            }

            /// Builds one deterministic test store.
            ///
            /// # Panics
            ///
            /// Panics if `step_budget` or mailbox capacity is zero, the Store configuration
            /// is invalid, or a deferred dependency constructor panics.
            #[must_use]
            pub fn build(self) -> TestStore<State, Action, Snapshot> {
                let runtime = DeterministicRuntime::with_step_budget(self.step_budget);
                let handle = runtime.runtime();
                let clock = handle.clock();
                let gate = TransactionGate::new();
                let issues = IssueRecorder::new();
                let mut builder = (self.construct)(self.state, self.reducer, handle);
                for configure in self.dependencies {
                    builder = configure(builder, clock.clone());
                }
                let builder = builder
                    .with_test_dependency_defaults()
                    .transaction_gate(gate.clone())
                    .issue_recorder(issues.clone());
                let store = (self.finish)(builder).expect("TestStore configuration must be valid");
                TestStore {
                    store,
                    runtime,
                    gate,
                    issues,
                    omissions: Vec::new(),
                    mode: self.mode,
                    assertions: self.assertions,
                    completed: false,
                    shutdown: false,
                }
            }
        }

        impl<State: $($send +)? 'static, Action: $($send +)? 'static, R, Snapshot> fmt::Debug
            for TestStoreBuilder<State, Action, R, Snapshot>
        {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct("TestStoreBuilder")
                    .field("step_budget", &self.step_budget)
                    .field("mode", &self.mode)
                    .finish_non_exhaustive()
            }
        }

        impl<State, Action> TestStore<State, Action>
        where
            State: Clone + fmt::Debug + PartialEq + $($send +)? 'static,
            Action: $($send +)? 'static,
        {
            /// Starts configuring an exhaustive deterministic test store.
            ///
            /// # Panics
            ///
            /// Propagates a panic from cloning the initial state.
            pub fn feature_builder<F>(state: State, feature: F) -> TestStoreBuilder<State, Action, F>
            where
                F: $core::Feature<State = State, Action = Action> + $($send +)? 'static,
            {
                let baseline = state.clone();
                TestStoreBuilder {
                    state,
                    reducer: feature,
                    dependencies: Vec::new(),
                    mode: Exhaustivity::Exhaustive,
                    step_budget: DEFAULT_STEP_BUDGET,
                    construct: StoreBuilder::feature,
                    finish: StoreBuilder::build_feature,
                    assertions: Assertions {
                        baseline: std::panic::AssertUnwindSafe(parking_lot::Mutex::new(baseline)),
                        pending: std::panic::AssertUnwindSafe(parking_lot::Mutex::new(None)),
                        capture: CaptureOwner::new(State::clone),
                        render: None,
                        coverage: Coverage::Full,
                        declared: DeclaredCaptures::default(),
                    },
                }
            }

            /// Starts configuring an exhaustive deterministic test store.
            ///
            /// # Panics
            ///
            /// Propagates a panic from cloning the initial state.
            pub fn builder<R>(state: State, reducer: R) -> TestStoreBuilder<State, Action, R>
            where
                R: Reducer<State = State, Action = Action> + $($send +)? 'static,
            {
                Self::configured_builder(
                    state,
                    reducer,
                    Exhaustivity::Exhaustive,
                    DEFAULT_STEP_BUDGET,
                )
            }

            /// Creates an exhaustive `TestStore` with default deterministic poll budget.
            ///
            /// # Panics
            ///
            /// Propagates a panic from cloning the initial state.
            #[must_use]
            pub fn new<R>(state: State, reducer: R) -> Self
            where
                R: Reducer<State = State, Action = Action> + $($send +)? 'static,
            {
                Self::builder(state, reducer).build()
            }

            /// Creates a `TestStore` with explicit exhaustivity mode.
            ///
            /// # Panics
            ///
            /// Propagates a panic from cloning the initial state.
            #[must_use]
            pub fn with_exhaustivity<R>(state: State, reducer: R, mode: Exhaustivity) -> Self
            where
                R: Reducer<State = State, Action = Action> + $($send +)? 'static,
            {
                Self::with_exhaustivity_and_step_budget(state, reducer, mode, DEFAULT_STEP_BUDGET)
            }

            /// Creates an exhaustive `TestStore` with explicit deterministic poll budget.
            ///
            /// # Panics
            ///
            /// Panics when `step_budget` is zero or cloning the initial state panics.
            #[must_use]
            pub fn with_step_budget<R>(state: State, reducer: R, step_budget: usize) -> Self
            where
                R: Reducer<State = State, Action = Action> + $($send +)? 'static,
            {
                Self::with_exhaustivity_and_step_budget(
                    state,
                    reducer,
                    Exhaustivity::Exhaustive,
                    step_budget,
                )
            }

            /// Creates a `TestStore` with explicit exhaustivity and deterministic poll budget.
            ///
            /// # Panics
            ///
            /// Panics when `step_budget` is zero or cloning the initial state panics.
            #[must_use]
            pub fn with_exhaustivity_and_step_budget<R>(
                state: State,
                reducer: R,
                mode: Exhaustivity,
                step_budget: usize,
            ) -> Self
            where
                R: Reducer<State = State, Action = Action> + $($send +)? 'static,
            {
                Self::configured_builder(state, reducer, mode, step_budget).build()
            }

            /// Reuses projection construction, then marks the snapshot as full-state coverage.
            fn configured_builder<R>(
                state: State,
                reducer: R,
                mode: Exhaustivity,
                step_budget: usize,
            ) -> TestStoreBuilder<State, Action, R>
            where
                R: Reducer<State = State, Action = Action> + $($send +)? 'static,
            {
                let mut builder = Self::projected_builder(state, reducer, State::clone)
                    .with_exhaustivity(mode)
                    .with_step_budget(step_budget);
                builder.assertions.coverage = Coverage::Full;
                builder
            }
        }

        impl<State, Action> TestStore<State, Action>
        where
            State: $($send +)? 'static,
            Action: $($send +)? 'static,
        {
            /// Configures a typed Feature exhaustive over an independent selected snapshot.
            ///
            /// Captures the initial snapshot immediately, before State enters the runtime.
            /// Only the snapshot requires Clone and equality; omitted State fields are
            /// not compared. The projection must not alias mutable historical state.
            ///
            /// # Panics
            ///
            /// Propagates a panic from the initial snapshot callback.
            pub fn projected_feature_builder<F, Snapshot>(
                state: State,
                feature: F,
                snapshot: impl Fn(&State) -> Snapshot + $($send +)? $($sync +)? 'static,
            ) -> TestStoreBuilder<State, Action, F, Snapshot>
            where
                F: $core::Feature<State = State, Action = Action> + $($send +)? 'static,
                Snapshot: Clone + fmt::Debug + PartialEq + $($send +)? 'static,
            {
                let baseline = snapshot(&state);
                TestStoreBuilder {
                    state,
                    reducer: feature,
                    construct: StoreBuilder::feature,
                    finish: StoreBuilder::build_feature,
                    dependencies: Vec::new(),
                    mode: Exhaustivity::Exhaustive,
                    step_budget: DEFAULT_STEP_BUDGET,
                    assertions: Assertions {
                        baseline: std::panic::AssertUnwindSafe(parking_lot::Mutex::new(baseline)),
                        pending: std::panic::AssertUnwindSafe(parking_lot::Mutex::new(None)),
                        capture: CaptureOwner::new(snapshot),
                        render: None,
                        coverage: Coverage::Projected,
                        declared: DeclaredCaptures::default(),
                    },
                }
            }

            /// Creates a store exhaustive over the selected independent snapshot.
            ///
            /// Omitted State fields are not compared. The projection must produce independent
            /// historical values, not aliases to mutable state.
            ///
            /// # Panics
            ///
            /// Propagates a panic from the initial snapshot callback.
            #[must_use]
            pub fn projected<R, Snapshot>(
                state: State,
                reducer: R,
                snapshot: impl Fn(&State) -> Snapshot + $($send +)? $($sync +)? 'static,
            ) -> TestStore<State, Action, Snapshot>
            where
                R: Reducer<State = State, Action = Action> + $($send +)? 'static,
                Snapshot: Clone + fmt::Debug + PartialEq + $($send +)? 'static,
            {
                Self::projected_builder(state, reducer, snapshot).build()
            }

            /// Configures a store exhaustive over the selected independent snapshot.
            ///
            /// Captures the initial snapshot immediately, before State enters the runtime.
            /// The callback must produce independent historical values; omitted State fields
            /// are not compared.
            ///
            /// # Panics
            ///
            /// Propagates a panic from the initial snapshot callback.
            pub fn projected_builder<R, Snapshot>(
                state: State,
                reducer: R,
                snapshot: impl Fn(&State) -> Snapshot + $($send +)? $($sync +)? 'static,
            ) -> TestStoreBuilder<State, Action, R, Snapshot>
            where
                R: Reducer<State = State, Action = Action> + $($send +)? 'static,
                Snapshot: Clone + fmt::Debug + PartialEq + $($send +)? 'static,
            {
                let baseline = snapshot(&state);
                TestStoreBuilder {
                    state,
                    reducer,
                    construct: Store::builder,
                    finish: StoreBuilder::build,
                    dependencies: Vec::new(),
                    mode: Exhaustivity::Exhaustive,
                    step_budget: DEFAULT_STEP_BUDGET,
                    assertions: Assertions {
                        baseline: std::panic::AssertUnwindSafe(parking_lot::Mutex::new(baseline)),
                        pending: std::panic::AssertUnwindSafe(parking_lot::Mutex::new(None)),
                        capture: CaptureOwner::new(snapshot),
                        render: None,
                        coverage: Coverage::Projected,
                        declared: DeclaredCaptures::default(),
                    },
                }
            }
        }

        crate::store::focused::focused_api!($core, [$($send)?]);

        impl<State, Action, Snapshot> TestStore<State, Action, Snapshot>
        where
            State: $($send +)? 'static,
            Action: $($send +)? 'static,
            Snapshot: Clone + fmt::Debug + PartialEq + $($send +)? 'static,
        {
            /// Sends an action with explicit independent declared-value expectations.
            ///
            /// Use [`CaptureExpectations::update`] to edit a key's owned historical value.
            /// [`crate::requiring_change`] optionally requires a delta in the wrapped root
            /// or capture expectation before admission; selected captures are not full-state coverage.
            ///
            /// # Errors
            ///
            /// Returns declaration, capture, comparison, admission or driving failures.
            ///
            /// # Panics
            ///
            /// Propagates user callback and root snapshot comparison panics.
            #[track_caller]
            pub fn send_with_captures<'a>(
                &'a mut self,
                action: Action,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
                captures: impl FnOnce(&mut CaptureExpectations<'_>) -> Result<(), TestStoreError> + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                let origin = Location::caller();
                async move {
                    let preflight = if let Some(error) = self.assertions.incomplete_error() {
                        Err(error)
                    } else if self.gate.has_pending() {
                        Err(TestStoreError::unreceived_action(origin))
                    } else if self.gate.boundary().is_some() {
                        Err(TestStoreError::incomplete_assertion("transaction boundary", origin))
                    } else {
                        self.preflight_captures(origin).await
                    };
                    if let Err(error) = preflight {
                        // Keep the cause owned while unused input destructors run.
                        drop(update);
                        drop(captures);
                        drop(action);
                        return Err(error);
                    }
                    let expected = self.assertions.expected(update);
                    let declared = match self.assertions.declared.expected(captures) {
                        Ok(declared) => declared,
                        Err(error) => {
                            drop(expected);
                            drop(action);
                            return Err(error);
                        }
                    };
                    assert!(
                        self.gate.reserve_root(),
                        "preflight excludes an active boundary"
                    );
                    self.assertions.begin(expected, origin, "send");
                    self.assertions.declared.begin(declared, origin, "send");
                    let receipt = self.store.try_send(action).map_err(|error| {
                        self.gate.cancel_root_reservation();
                        self.assertions
                            .pending
                            .get_mut()
                            .take();
                        self.assertions.declared.complete();
                        if error.is_full() {
                            TestStoreError::admission_full(origin)
                        } else {
                            TestStoreError::store_stopped_from_admission(origin)
                        }
                    })?;
                    let task = self
                        .runtime
                        .drive(receipt.into_future())
                        .await
                        .map_err(|error| self.drive_error(error, origin))?
                        .map_err(|error| TestStoreError::store_stopped(error, origin))?;
                    self.settle(origin)?;
                    self.verify_snapshot(self.gate.boundary()).await?;
                    self.settle(origin)?;
                    Ok(TestActionTask::new(
                        task,
                        self.runtime.clone(),
                        origin,
                        self.gate.clone(),
                    ))
                }
            }

            /// Receives an action with explicit independent declared-value expectations.
            ///
            /// Capture edits use [`CaptureExpectations::update`]. A panicking expectation
            /// callback after successful preflight leaves the original action gated for retry;
            /// caller side effects are not rolled back and earlier preflight debt is not cleared.
            /// Root action matcher mismatch or unwind remains retryable for compatibility.
            /// Typed output, mounted-action and command matchers instead retain failed-match debt.
            ///
            /// # Errors
            ///
            /// Returns declaration, capture, comparison, matching or driving failures.
            ///
            /// # Panics
            ///
            /// Propagates user callback and root snapshot comparison panics.
            #[track_caller]
            pub fn receive_with_captures<'a>(
                &'a mut self,
                matcher: impl FnOnce(&Action) -> bool + $($send +)? 'a,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
                captures: impl FnOnce(&mut CaptureExpectations<'_>) -> Result<(), TestStoreError> + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                let origin = Location::caller();
                async move {
                    let readiness = if let Some(error) = self.assertions.incomplete_error() {
                        Err(error)
                    } else {
                        self.settle(origin).map(|_| ())
                    };
                    if let Err(error) = readiness {
                        drop(update);
                        drop(captures);
                        drop(matcher);
                        return Err(error);
                    }
                    let preflight = match self.gate.inspect_pending(matcher) {
                        Some(true) => self.preflight_captures(origin).await,
                        Some(false) => Err(TestStoreError::action_mismatch(origin)),
                        None => Err(TestStoreError::no_received_action(origin)),
                    };
                    if let Err(error) = preflight {
                        drop(update);
                        drop(captures);
                        return Err(error);
                    }
                    let expected = self.assertions.expected(update);
                    let declared = match self.assertions.declared.expected(captures) {
                        Ok(declared) => declared,
                        Err(error) => {
                            drop(expected);
                            return Err(error);
                        }
                    };
                    let Some(receipt) = self.gate.release_pending_with_receipt() else {
                        let error = TestStoreError::no_received_action(origin);
                        drop(declared);
                        drop(expected);
                        return Err(error);
                    };
                    // An expectation can synchronously cancel the pending action.
                    // Only successful release admits this new assertion obligation.
                    self.assertions.begin(expected, origin, "receive");
                    self.assertions.declared.begin(declared, origin, "receive");
                    let task = self
                        .runtime
                        .drive(receipt.finish())
                        .await
                        .map_err(|error| self.drive_error(error, origin))?
                        .map_err(|error| TestStoreError::store_stopped(error, origin))?;
                    self.settle(origin)?;
                    self.verify_snapshot(self.gate.boundary()).await?;
                    self.settle(origin)?;
                    Ok(TestActionTask::new(
                        task,
                        self.runtime.clone(),
                        origin,
                        self.gate.clone(),
                    ))
                }
            }

            /// Receives one typed update, including an update that leaves state equal.
            ///
            /// # Errors
            ///
            /// Returns incomplete-boundary, runtime, or state-comparison failures.
            ///
            /// # Panics
            ///
            /// Panics when the expected snapshot differs from the live snapshot and
            /// propagates expectation callback panics.
            #[track_caller]
            pub fn receive_update<'a>(
                &'a mut self,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                self.receive_update_with_captures(update, |_| Ok(()))
            }

            /// Receives one typed update with independent declared-value expectations.
            ///
            /// # Errors
            ///
            /// Returns incomplete-boundary, declaration, runtime, or comparison failures.
            ///
            /// # Panics
            ///
            /// Panics on a snapshot mismatch and propagates expectation callback panics.
            #[track_caller]
            pub fn receive_update_with_captures<'a>(
                &'a mut self,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
                captures: impl FnOnce(&mut CaptureExpectations<'_>) -> Result<(), TestStoreError> + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                self.receive_mutation(
                    rca_core::testing::TransactionKind::Update,
                    "receive_update",
                    Location::caller(),
                    update,
                    captures,
                )
            }

            /// Receives one structural transaction, including a state-equal installation.
            ///
            /// The returned task has the causal lifetime described by
            /// [`Self::receive_structural_with_captures`].
            ///
            /// # Errors
            ///
            /// Returns wrong-kind, incomplete-boundary, runtime or comparison failures.
            ///
            /// # Panics
            ///
            /// Panics on a snapshot mismatch and propagates expectation callback panics.
            #[track_caller]
            pub fn receive_structural<'a>(
                &'a mut self,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                self.receive_structural_with_captures(update, |_| Ok(()))
            }

            /// Receives structural work with independent declared-value expectations.
            ///
            /// Uses the same assertion frontier and baseline as actions and updates.
            /// Committed work may launch before acknowledgement; subsequent mutations
            /// remain gated until this boundary's snapshot and captures are verified.
            ///
            /// The returned task tracks asynchronous work started by this transaction,
            /// including Initial hooks. If that work produces mutations, receive them
            /// before requiring `TestActionTask::finish` to succeed. Finishing the task
            /// neither releases pending assertion boundaries nor advances virtual time.
            ///
            /// # Errors
            ///
            /// Returns wrong-kind, incomplete-boundary, declaration, runtime or comparison failures.
            ///
            /// # Panics
            ///
            /// Panics on a snapshot mismatch and propagates expectation callback panics.
            #[track_caller]
            pub fn receive_structural_with_captures<'a>(
                &'a mut self,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
                captures: impl FnOnce(&mut CaptureExpectations<'_>) -> Result<(), TestStoreError> + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                self.receive_mutation(
                    rca_core::testing::TransactionKind::Structural,
                    "receive_structural",
                    Location::caller(),
                    update,
                    captures,
                )
            }

            /// Reads inert child history from this exact pending output declaration.
            ///
            /// This does not advance scheduling, take the owned output, or acknowledge
            /// assertion debt. Ordinary outputs and non-matching declarations return None.
            #[must_use]
            pub fn output_origin<O: $($send +)? 'static>(
                &self,
                output: &$core::Output<O>,
            ) -> Option<rca_core::RemovalOrigin> {
                self.gate.output_origin(output)
            }

            /// Receives an owned typed output through the existing assertion frontier.
            ///
            /// The matcher borrows the original value before release. A wrong output,
            /// mismatch or matcher panic leaves that value pending for cleanup. A failed
            /// exact-source matcher remains assertion debt even after a successful retry.
            ///
            /// # Errors
            ///
            /// Returns wrong-kind, mismatch, incomplete-boundary or driving failures.
            ///
            /// # Panics
            ///
            /// Propagates matcher, expectation and snapshot comparison panics.
            #[track_caller]
            pub fn receive_output<'a, O: $($send +)? 'static>(
                &'a mut self,
                output: &'a $core::Output<O>,
                matcher: impl FnOnce(&O) -> bool + $($send +)? 'a,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                self.receive_output_with_captures(output, matcher, update, |_| Ok(()))
            }

            /// Receives an output with independent declared-value expectations.
            ///
            /// Matching precedes capture preflight and release. After successful release,
            /// cancellation retains the same snapshot and capture debt as other receives.
            ///
            /// # Errors
            ///
            /// Returns matching, declaration, comparison or driving failures.
            ///
            /// # Panics
            ///
            /// Propagates matcher, expectation and snapshot comparison panics.
            #[track_caller]
            pub fn receive_output_with_captures<'a, O: $($send +)? 'static>(
                &'a mut self,
                output: &'a $core::Output<O>,
                matcher: impl FnOnce(&O) -> bool + $($send +)? 'a,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
                captures: impl FnOnce(&mut CaptureExpectations<'_>) -> Result<(), TestStoreError> + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                let origin = Location::caller();
                self.receive_mutation_checked(
                    rca_core::testing::TransactionKind::Output,
                    "receive_output",
                    origin,
                    move |gate| match gate.match_output(output, matcher) {
                        Some(true) => Ok(()),
                        Some(false) => Err(TestStoreError::action_mismatch(origin)),
                        None => Err(TestStoreError::no_received_action(origin)),
                    },
                    update,
                    captures,
                )
            }

            /// Receives an action from the exact installed child captured by `context`.
            /// Includes actions produced by child tasks, effects and timers.
            /// Matching borrows the original action. A valid-source mismatch or unwind
            /// retains its reply and sticky assertion debt; a later match permits cleanup only.
            /// Wrong kind or source does not invoke the matcher or create failed-match debt.
            ///
            /// # Errors
            ///
            /// Returns wrong-target, mismatch, typed authority, incomplete-boundary or driving failures.
            ///
            /// # Panics
            ///
            /// Propagates matcher, expectation and snapshot comparison panics.
            #[track_caller]
            pub fn receive_mounted<'a, CS: $($send +)? 'static, CA: $($send +)? 'static>(
                &'a mut self,
                context: &'a $core::TaskContext<CS, CA>,
                matcher: impl FnOnce(&CA) -> bool + $($send +)? 'a,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                self.receive_mounted_with_captures(context, matcher, update, |_| Ok(()))
            }

            /// Receives a mounted action with the full root snapshot and independent captures.
            ///
            /// # Errors
            ///
            /// Returns matching, authority, declaration, comparison or driving failures.
            ///
            /// # Panics
            ///
            /// Propagates matcher, expectation and snapshot comparison panics.
            #[track_caller]
            pub fn receive_mounted_with_captures<'a, CS: $($send +)? 'static, CA: $($send +)? 'static>(
                &'a mut self,
                context: &'a $core::TaskContext<CS, CA>,
                matcher: impl FnOnce(&CA) -> bool + $($send +)? 'a,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
                captures: impl FnOnce(&mut CaptureExpectations<'_>) -> Result<(), TestStoreError> + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                let origin = Location::caller();
                self.receive_mutation_checked(
                    rca_core::testing::TransactionKind::Action,
                    "receive_mounted",
                    origin,
                    move |gate| match gate.match_mounted(context, matcher) {
                        Ok(Some(true)) => Ok(()),
                        Ok(Some(false)) => Err(TestStoreError::action_mismatch(origin)),
                        Ok(None) => Err(TestStoreError::no_received_action(origin)),
                        Err(error) => Err(TestStoreError::task_access(error, origin)),
                    },
                    update,
                    captures,
                )
            }

            /// Receives the original command for this port before its mapper runs.
            /// Matching borrows the original command. A valid-source mismatch or unwind
            /// retains its reply and sticky assertion debt; a later match permits cleanup only.
            /// Wrong kind or source does not invoke the matcher or create failed-match debt.
            ///
            /// # Errors
            ///
            /// Returns wrong-target, mismatch, typed authority, incomplete-boundary or driving failures.
            ///
            /// # Panics
            ///
            /// Propagates matcher, expectation and snapshot comparison panics.
            #[track_caller]
            pub fn receive_command<'a, C: $($send +)? 'static>(
                &'a mut self,
                port: &'a $core::CommandPort<C>,
                matcher: impl FnOnce(&C) -> bool + $($send +)? 'a,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                self.receive_command_with_captures(port, matcher, update, |_| Ok(()))
            }

            /// Receives an original command with the full root snapshot and declared captures.
            ///
            /// # Errors
            ///
            /// Returns matching, authority, declaration, comparison or driving failures.
            ///
            /// # Panics
            ///
            /// Propagates matcher, expectation and snapshot comparison panics.
            #[track_caller]
            pub fn receive_command_with_captures<'a, C: $($send +)? 'static>(
                &'a mut self,
                port: &'a $core::CommandPort<C>,
                matcher: impl FnOnce(&C) -> bool + $($send +)? 'a,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
                captures: impl FnOnce(&mut CaptureExpectations<'_>) -> Result<(), TestStoreError> + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                let origin = Location::caller();
                self.receive_mutation_checked(
                    rca_core::testing::TransactionKind::Action,
                    "receive_command",
                    origin,
                    move |gate| match gate.match_command(port, matcher) {
                        Ok(Some(true)) => Ok(()),
                        Ok(Some(false)) => Err(TestStoreError::action_mismatch(origin)),
                        Ok(None) => Err(TestStoreError::no_received_action(origin)),
                        Err(error) => Err(TestStoreError::task_access(error, origin)),
                    },
                    update,
                    captures,
                )
            }

            /// Matches and acknowledges the terminal component of one exact boundary.
            ///
            /// Cancellation does not acknowledge a selected transaction. This operation
            /// accepts its actual production error without inventing a state expectation.
            /// Wrong-root, stale and non-terminal tokens invoke no matcher. A false or
            /// panicking matcher, or an earlier failed typed matcher, remains sticky debt.
            /// An already-admitted failed snapshot or capture expectation prevents this
            /// step from acknowledging the terminal component or releasing later work.
            ///
            /// # Errors
            ///
            /// Returns absent-boundary, mismatch, incomplete-assertion or driving errors.
            ///
            /// # Panics
            ///
            /// Propagates matcher panics while retaining the boundary.
            #[track_caller]
            pub fn receive_terminal<'a>(
                &'a mut self,
                boundary: rca_core::testing::TransactionBoundary,
                matcher: impl FnOnce(&rca_core::TaskAccessError) -> bool + $($send +)? 'a,
            ) -> impl Future<Output = Result<(), TestStoreError>> + $($send +)? 'a {
                let origin = Location::caller();
                async move {
                    if let Err(error) = self.settle(origin) {
                        drop(matcher);
                        return Err(error);
                    }
                    self.receive_terminal_checked(boundary, matcher, origin)?;
                    self.settle(origin)?;
                    Ok(())
                }
            }

            /// Acknowledges the terminal component of this exact output declaration.
            ///
            /// Historical matching remains available after scope or Store shutdown;
            /// it does not reacquire live authority or retain an owned output value.
            ///
            /// # Errors
            ///
            /// Returns absent-boundary, mismatch, incomplete-assertion or driving errors.
            ///
            /// # Panics
            ///
            /// Propagates matcher panics while retaining the boundary.
            #[track_caller]
            pub fn receive_output_terminal<'a, O: $($send +)? 'static>(
                &'a mut self,
                output: &'a $core::Output<O>,
                matcher: impl FnOnce(&rca_core::TaskAccessError) -> bool + $($send +)? 'a,
            ) -> impl Future<Output = Result<(), TestStoreError>> + $($send +)? 'a {
                let origin = Location::caller();
                async move {
                    let boundary = self.settle(origin).and_then(|_| {
                        self.gate
                            .output_boundary(output)
                            .ok_or_else(|| TestStoreError::no_received_action(origin))
                    });
                    let boundary = match boundary {
                        Ok(boundary) => boundary,
                        Err(error) => {
                            drop(matcher);
                            return Err(error);
                        }
                    };
                    self.receive_terminal_checked(boundary, matcher, origin)?;
                    self.settle(origin)?;
                    Ok(())
                }
            }

            /// Acknowledges a matched terminal receipt only when no earlier obligation remains.
            fn receive_terminal_checked(
                &mut self,
                boundary: rca_core::testing::TransactionBoundary,
                matcher: impl FnOnce(&rca_core::TaskAccessError) -> bool,
                origin: &'static Location<'static>,
            ) -> Result<(), TestStoreError> {
                let selected = self
                    .gate
                    .boundary()
                    .ok_or_else(|| TestStoreError::no_received_action(origin))?;
                match self.gate.match_terminal(&boundary, matcher) {
                    Some(true) => {}
                    Some(false) => return Err(TestStoreError::action_mismatch(origin)),
                    None => return Err(TestStoreError::no_received_action(origin)),
                }
                // A failed expected-success step keeps this same frontier held, even
                // when its actual terminal result matches a later cleanup expectation.
                if let Some(error) = self.assertions.incomplete_error() {
                    return Err(error);
                }
                if !self.gate.acknowledge_terminal(&boundary) {
                    return Err(TestStoreError::no_received_action(origin));
                }
                if self.gate.boundary() == Some(selected) {
                    return Err(TestStoreError::incomplete_assertion("transaction boundary", origin));
                }
                Ok(())
            }

            /// Reuses typed-mutation admission when the transaction kind is the only matcher.
            fn receive_mutation<'a>(
                &'a mut self,
                kind: rca_core::testing::TransactionKind,
                operation: &'static str,
                origin: &'static Location<'static>,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
                captures: impl FnOnce(&mut CaptureExpectations<'_>) -> Result<(), TestStoreError> + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                self.receive_mutation_checked(kind, operation, origin, |_| Ok(()), update, captures)
            }

            /// Matches before preflight, then installs debt only after successful release.
            fn receive_mutation_checked<'a>(
                &'a mut self,
                kind: rca_core::testing::TransactionKind,
                operation: &'static str,
                origin: &'static Location<'static>,
                validate: impl FnOnce(&TransactionGate<State, Action>) -> Result<(), TestStoreError> + $($send +)? 'a,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
                captures: impl FnOnce(&mut CaptureExpectations<'_>) -> Result<(), TestStoreError> + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                async move {
                    let readiness = if let Some(error) = self.assertions.incomplete_error() {
                        Err(error)
                    } else {
                        self.settle(origin).and_then(|_| {
                            if self.gate.pending_kind() == Some(kind) {
                                Ok(())
                            } else {
                                Err(TestStoreError::no_received_action(origin))
                            }
                        })
                    };
                    if let Err(error) = readiness {
                        drop(update);
                        drop(captures);
                        drop(validate);
                        return Err(error);
                    }
                    let preflight = match validate(&self.gate) {
                        Ok(()) => self.preflight_captures(origin).await,
                        Err(error) => Err(error),
                    };
                    if let Err(error) = preflight {
                        drop(update);
                        drop(captures);
                        return Err(error);
                    }
                    let expected = self.assertions.expected(update);
                    let declared = match self.assertions.declared.expected(captures) {
                        Ok(declared) => declared,
                        Err(error) => {
                            drop(expected);
                            return Err(error);
                        }
                    };
                    let Some(receipt) = self.gate.release_pending_with_receipt() else {
                        let error = TestStoreError::no_received_action(origin);
                        drop(declared);
                        drop(expected);
                        return Err(error);
                    };
                    self.assertions.begin(expected, origin, operation);
                    self.assertions
                        .declared
                        .begin(declared, origin, operation);
                    let task = self
                        .runtime
                        .drive(receipt.finish())
                        .await
                        .map_err(|error| self.drive_error(error, origin))?
                        .map_err(|error| TestStoreError::store_stopped(error, origin))?;
                    self.verify_snapshot(self.gate.boundary()).await?;
                    Ok(TestActionTask::new(
                        task,
                        self.runtime.clone(),
                        origin,
                        self.gate.clone(),
                    ))
                }
            }

            /// Checks explicit expected changes made outside a Store action.
            ///
            /// This checks selected boundary values, not every intervening mutation.
            /// See [`CaptureKey`] for independence and external-writer limitations.
            ///
            /// # Errors
            ///
            /// Returns declaration, capture, comparison or incomplete-boundary failures.
            ///
            /// # Panics
            ///
            /// Propagates caller capture, clone, comparison and expectation panics.
            #[track_caller]
            pub fn check_captures<'a>(
                &'a mut self,
                captures: impl FnOnce(&mut CaptureExpectations<'_>) -> Result<(), TestStoreError> + $($send +)? 'a,
            ) -> impl Future<Output = Result<(), TestStoreError>> + $($send +)? 'a {
                let origin = Location::caller();
                async move {
                    if let Some(error) = self.assertions.incomplete_error() {
                        drop(captures);
                        return Err(error);
                    }
                    let expected = self.assertions.declared.expected(captures)?;
                    if self.assertions.declared.is_empty() {
                        return Ok(());
                    }
                    self.assertions
                        .declared
                        .begin(expected, origin, "check_captures");
                    self.verify_captures(origin, "check_captures").await
                }
            }

            /// Returns configured effect-action exhaustivity mode.
            #[must_use]
            pub const fn exhaustivity(&self) -> Exhaustivity {
                self.mode
            }

            /// Returns current virtual monotonic time.
            #[must_use]
            pub fn now(&self) -> MonotonicInstant {
                self.runtime.now()
            }

            /// Sends an external action and asserts that the snapshot remains unchanged.
            ///
            /// # Errors
            ///
            /// Returns [`TestStoreError`] for incomplete assertions, unreceived output, admission failure,
            /// Store closure, or deterministic poll-budget exhaustion.
            ///
            /// # Panics
            ///
            /// Panics when the snapshot changes or a snapshot callback panics.
            #[track_caller]
            pub fn send(
                &mut self,
                action: Action,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? '_ {
                self.send_with(action, |_| {})
            }

            /// Sends an action and verifies its complete expected snapshot.
            ///
            /// `update` changes an independent copy of the last verified snapshot exactly once.
            /// Dropping the future after admission leaves an incomplete assertion in this store.
            ///
            /// # Errors
            ///
            /// Returns [`TestStoreError`] for incomplete assertions, pending output, admission,
            /// Store or drive failures.
            ///
            /// # Panics
            ///
            /// Panics if actual and expected snapshots differ or a user callback panics.
            #[track_caller]
            pub fn send_with<'a>(
                &'a mut self,
                action: Action,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                self.send_with_captures(action, update, |_| Ok(()))
            }

            /// Receives an effect action and asserts that the snapshot remains unchanged.
            ///
            /// # Errors
            ///
            /// Returns [`TestStoreError`] for incomplete assertions, absent or rejected actions,
            /// Store closure, or deterministic driving failures.
            ///
            /// # Panics
            ///
            /// Panics when the snapshot changes or a user callback panics.
            #[track_caller]
            pub fn receive<'a>(
                &'a mut self,
                matcher: impl FnOnce(&Action) -> bool + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                self.receive_with(matcher, |_| {})
            }

            /// Receives an effect action and verifies its complete expected snapshot.
            ///
            /// `update` runs once on an independent copy after the matcher accepts the action.
            /// Dropping the future after release leaves an incomplete assertion in this store.
            ///
            /// # Errors
            ///
            /// Returns [`TestStoreError`] for incomplete assertions, absent or rejected actions,
            /// Store or drive failures.
            ///
            /// # Panics
            ///
            /// Panics if actual and expected snapshots differ or a user callback panics.
            #[track_caller]
            pub fn receive_with<'a>(
                &'a mut self,
                matcher: impl FnOnce(&Action) -> bool + $($send +)? 'a,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                self.receive_with_captures(matcher, update, |_| Ok(()))
            }

            /// Explicitly skips a pending mutation before user code in non-exhaustive mode.
            ///
            /// Cancels the command without reducing it, then verifies that the snapshot is unchanged.
            /// Structural installation cannot be skipped; rejection leaves its boundary
            /// pending without running capture or expectation callbacks.
            /// Skipping does not clear failed matcher or expectation debt.
            ///
            /// # Errors
            ///
            /// Returns [`TestStoreError`] for incomplete assertions, exhaustive mode, absent actions,
            /// or deterministic driving failures.
            ///
            /// # Panics
            ///
            /// Panics if the snapshot changed or a snapshot callback panics.
            #[track_caller]
            pub fn skip_received(
                &mut self,
            ) -> impl Future<Output = Result<(), TestStoreError>> + $($send +)? '_ {
                let origin = Location::caller();
                async move {
                    if let Some(error) = self.assertions.incomplete_error() {
                        return Err(error);
                    }
                    if self.mode == Exhaustivity::Exhaustive {
                        return Err(TestStoreError::skip_requires_non_exhaustive(origin));
                    }
                    self.runtime
                        .run_until_stalled()
                        .map_err(|error| self.drive_error(error, origin))?;
                    if self.gate.pending_kind() == Some(rca_core::testing::TransactionKind::Structural) {
                        return Err(TestStoreError::no_received_action(origin));
                    }
                    self.preflight_captures(origin).await?;
                    let expected = self.assertions.expected(|_| {});
                    let declared = self.assertions.declared.unchanged();
                    if !self.gate.skip_pending() {
                        let error = TestStoreError::no_received_action(origin);
                        drop(declared);
                        drop(expected);
                        return Err(error);
                    }
                    self.assertions.begin(expected, origin, "skip_received");
                    self.assertions
                        .declared
                        .begin(declared, origin, "skip_received");
                    self.settle(origin)?;
                    self.verify_snapshot(None).await?;
                    Ok(())
                }
            }

            /// Advances virtual time without auto-releasing effect actions.
            ///
            /// # Errors
            ///
            /// Returns [`TestStoreError`] when an action already awaits receive or deterministic poll
            /// budget is exhausted.
            #[track_caller]
            pub fn advance(
                &mut self,
                duration: Duration,
            ) -> impl Future<Output = Result<DriveReport, TestStoreError>> + $($send +)? '_ {
                let origin = Location::caller();
                async move {
                    if self.gate.has_pending() {
                        return Err(TestStoreError::unreceived_action(origin));
                    }
                    self.runtime.start_advance(duration);
                    let report = match self.runtime.resume_advance() {
                        Ok(report) => report,
                        Err(error) => {
                            self.runtime.finish_advance();
                            return Err(self.drive_error(error, origin));
                        }
                    };
                    if !self.gate.has_pending() {
                        self.runtime.finish_advance();
                    }
                    Ok(report)
                }
            }

            /// Inspects current actor-owned state without requiring Clone or equality.
            ///
            /// # Errors
            ///
            /// Returns [`TestStoreError`] when deterministic driving fails or Store stopped.
            #[track_caller]
            pub fn assert_state(
                &self,
                assertion: impl FnOnce(&State) + $($send +)? 'static,
            ) -> impl Future<Output = Result<(), TestStoreError>> + $($send +)? '_ {
                let origin = Location::caller();
                async move {
                    if let Err(error) = self.settle(origin) {
                        drop(assertion);
                        return Err(error);
                    }
                    assertions::state_receipt(
                        self.gate.boundary().is_some(),
                        assertion,
                        |assertion| self.runtime.drive(self.gate.assert_state(assertion).finish()),
                        |assertion| self.runtime.drive(self.store.read(assertion)),
                    )
                    .await
                    .map_err(|error| self.drive_error(error, origin))?
                    .map_err(|error| TestStoreError::store_stopped(error, origin))?;
                    Ok(())
                }
            }

            /// Asserts exact state using method-local `Debug` and `PartialEq` bounds.
            ///
            /// # Errors
            ///
            /// Returns [`TestStoreError`] when deterministic driving fails or Store stopped.
            ///
            /// # Panics
            ///
            /// Panics when current state differs from `expected`.
            #[track_caller]
            pub fn assert_state_eq(
                &self,
                expected: State,
            ) -> impl Future<Output = Result<(), TestStoreError>> + $($send +)? '_
            where
                State: fmt::Debug + PartialEq,
            {
                self.assert_state(move |state| assert_eq!(state, &expected))
            }

            /// Verifies no RCA-owned work remains, then shuts Store down cleanly.
            ///
            /// Polls only ready work within one configured budget. Never advances virtual
            /// time or waits for external wakes. A failed call leaves Drop checking enabled.
            ///
            /// # Errors
            ///
            /// Returns [`TestStoreError`] for incomplete assertions, remaining work, Store closure,
            /// or deterministic driving failures.
            #[track_caller]
            pub fn finish(&mut self) -> impl Future<Output = Result<(), TestStoreError>> + $($send +)? '_ {
                let origin = Location::caller();
                async move { self.complete(origin) }
            }

            /// Matches and consumes the oldest recorded issue without advancing virtual time.
            ///
            /// Drives only currently ready work within the configured budget. The matcher
            /// runs outside all reporter locks. Cancellation before polling, mismatch and
            /// matcher panic leave the queue unchanged.
            ///
            /// # Errors
            ///
            /// Returns [`TestStoreError`] for a missing or mismatched issue or exhausted budget.
            ///
            /// # Panics
            ///
            /// Propagates matcher panics while retaining the recorded issue.
            #[track_caller]
            pub fn expect_issue<'a>(
                &'a mut self,
                matches: impl FnOnce(&Issue) -> bool + $($send +)? 'a,
            ) -> impl Future<Output = Result<Issue, TestStoreError>> + $($send +)? 'a {
                let origin = Location::caller();
                async move {
                    let mut polls = 0;
                    let issue = self
                        .runtime
                        .run_ready(std::future::pending::<()>(), &mut polls)
                        .map_err(|error| self.drive_error(error, origin))
                        .and_then(|_| {
                            self.issues
                                .oldest()
                                .ok_or_else(|| TestStoreError::no_issue(origin))
                        });
                    let issue = match issue {
                        Ok(issue) => issue,
                        Err(error) => {
                            drop(matches);
                            return Err(error);
                        }
                    };
                    if !matches(&issue) {
                        return Err(TestStoreError::issue_mismatch(issue, origin));
                    }
                    Ok(self
                        .issues
                        .consume_oldest()
                        .expect("exclusive TestStore consumer retains matched issue"))
                }
            }

        }

        impl<State: $($send +)? 'static, Action: $($send +)? 'static, Snapshot: $($send +)?> TestStore<State, Action, Snapshot> {
            /// Reads committed contributions without releasing or acknowledging a pending mutation.
            ///
            /// Construction is inert. First polling checks readiness and uses the exact
            /// held frontier, or ordinary ordered ingress when no frontier is held.
            ///
            /// # Errors
            ///
            /// Returns the production task-access cause or a deterministic driving failure.
            #[track_caller]
            pub fn fold<'a, K: rca_core::FoldKey>(
                &'a mut self,
                fold: &$core::Fold<State, K>,
            ) -> impl Future<Output = Result<K::Value, TestStoreError>> + $($send +)? 'a
            where
                K::Value: $($send +)? 'static,
            {
                let origin = Location::caller();
                let fold = fold.clone();
                async move {
                    self.gate
                        .validate_fold(&self.store)
                        .map_err(|error| TestStoreError::task_access(error, origin))?;
                    self.settle(origin)?;
                    let access = self.gate.fold(&self.store, &fold);
                    self.drive_task_access(access, origin).await
                }
            }

            /// Captures a subtree for ordinary or held-frontier diagnostic reads.
            ///
            /// Acquisition evaluates no contribution or selector and changes no baseline.
            /// The captured view keeps fixed incarnation/configuration authority.
            ///
            /// # Errors
            ///
            /// Returns authority, readiness, closure or deterministic driving failures.
            #[track_caller]
            pub fn acquire_fold<'a, K: rca_core::FoldKey, CS: $($send +)? 'static, CA: $($send +)? 'static>(
                &'a mut self,
                fold: &$core::Fold<State, K>,
                site: &$core::MountSite<State, CS, CA, Action>,
            ) -> impl Future<Output = Result<Option<$core::FoldView<K>>, TestStoreError>> + $($send +)? 'a
            where
                K::Value: $($send +)? 'static,
            {
                let origin = Location::caller();
                let fold = fold.clone();
                let site = site.clone();
                async move {
                    self.gate
                        .validate_acquisition(&self.store, &site)
                        .map_err(|error| TestStoreError::task_access(error, origin))?;
                    self.settle(origin)?;
                    let access = self.gate.acquire_fold(&self.store, &fold, &site);
                    self.drive_task_access(access, origin).await
                }
            }

            /// Reads a diagnostically acquired view without changing assertion debt.
            ///
            /// The view must have been acquired through this `TestStore`. Views acquired
            /// through an ordinary Store have no diagnostic gate and return `PhaseDenied`;
            /// use their ordinary `read` method outside a held mutation instead.
            ///
            /// # Errors
            ///
            /// Rejects foreign roots and stale authority before selectors, or reports
            /// closure, unsupported diagnostic views and deterministic driving failures.
            #[track_caller]
            pub fn fold_view<'a, K: rca_core::FoldKey>(
                &'a mut self,
                view: &$core::FoldView<K>,
            ) -> impl Future<Output = Result<K::Value, TestStoreError>> + $($send +)? 'a
            where
                K::Value: $($send +)? 'static,
            {
                let origin = Location::caller();
                let view = view.clone();
                async move {
                    self.gate
                        .validate_fold(&self.store)
                        .map_err(|error| TestStoreError::task_access(error, origin))?;
                    self.settle(origin)?;
                    let access = self.gate.fold_view(&self.store, &view);
                    self.drive_task_access(access, origin).await
                }
            }

            /// Acquires a current command port without releasing a pending mutation.
            ///
            /// First polling checks readiness before settling work; the actor revalidates
            /// the declaration at execution. The operation changes no assertion baseline.
            ///
            /// # Errors
            ///
            /// Returns the production task-access cause or a deterministic driving failure.
            #[track_caller]
            pub fn command_port<'a, C: $($send +)? 'static>(
                &'a mut self,
                site: &$core::CommandSite<State, C>,
            ) -> impl Future<Output = Result<Option<$core::CommandPort<C>>, TestStoreError>> + $($send +)? 'a {
                let origin = Location::caller();
                let site = site.clone();
                async move {
                    self.gate
                        .validate_command_acquisition(&self.store, &site)
                        .map_err(|error| TestStoreError::task_access(error, origin))?;
                    self.settle(origin)?;
                    let access = self.gate.command_port(&self.store, &site);
                    self.drive_task_access(access, origin).await
                }
            }

            /// Acquires a fresh child context without releasing a pending mutation.
            ///
            /// First polling checks real readiness before driving Initial work. Acquisition
            /// then revalidates the site at its actual actor position. It changes no baseline.
            ///
            /// # Errors
            ///
            /// Returns the production task-access cause or a deterministic driving failure.
            #[track_caller]
            pub fn acquire<'a, CS: $($send +)? 'static, CA: $($send +)? 'static>(
                &'a mut self,
                site: &$core::MountSite<State, CS, CA, Action>,
            ) -> impl Future<Output = Result<Option<$core::TaskContext<CS, CA>>, TestStoreError>> + $($send +)? 'a {
                let origin = Location::caller();
                let site = site.clone();
                async move {
                    self.gate
                        .validate_acquisition(&self.store, &site)
                        .map_err(|error| TestStoreError::task_access(error, origin))?;
                    self.settle(origin)?;
                    let acquisition = self.gate.acquire(&self.store, &site);
                    self.drive_task_access(acquisition, origin).await
                }
            }

            /// Drives production acquisition/read futures without changing assertion debt.
            /// The callers supply internal access futures, not user-defined future bodies.
            async fn drive_task_access<T, F>(
                &self,
                future: F,
                origin: &'static Location<'static>,
            ) -> Result<T, TestStoreError>
            where
                F: Future<Output = Result<T, rca_core::TaskAccessError>> $(+ $send)?,
                $(T: $send,)?
            {
                self.runtime
                    .drive(future)
                    .await
                    .map_err(|error| self.drive_error(error, origin))?
                    .map_err(|error| TestStoreError::task_access(error, origin))
            }
        }

        impl<State: $($send +)? 'static, Action: $($send +)? 'static, Snapshot> TestStore<State, Action, Snapshot> {
            /// Resumes an explicit advance, or polls ready work without advancing the clock.
            fn settle(&self, origin: &'static Location<'static>) -> Result<DriveReport, TestStoreError> {
                let report = if self.runtime.has_active_advance() {
                    match self.runtime.resume_advance() {
                        Ok(report) => report,
                        Err(error) => {
                            self.runtime.finish_advance();
                            return Err(self.drive_error(error, origin));
                        }
                    }
                } else {
                    self.runtime
                        .run_until_stalled()
                        .map_err(|error| self.drive_error(error, origin))?
                };
                if self.runtime.has_active_advance() && !self.gate.has_pending() {
                    self.runtime.finish_advance();
                }
                Ok(report)
            }
        }

        impl<State: $($send +)? 'static, Action: $($send +)? 'static, Snapshot> TestStore<State, Action, Snapshot> {
            /// Captures the selected live or terminal boundary without driving or acknowledging it.
            #[must_use]
            pub fn pending_boundary(&self) -> Option<rca_core::testing::TransactionBoundary> {
                self.gate.pending_boundary()
            }

            /// Checks debt and work, then shuts down using one shared poll budget.
            /// A retry after shutdown checks the remaining cleanup instead of shutting down twice.
            fn complete(&mut self, origin: &'static Location<'static>) -> Result<(), TestStoreError> {
                if self.completed {
                    return Ok(());
                }
                if let Some(error) = self.assertions.incomplete_error() {
                    return Err(error);
                }
                let mut polls = 0;
                self.runtime
                    .run_ready(std::future::pending::<()>(), &mut polls)
                    .map_err(|error| self.drive_error(error, origin))?;
                self.check_issues(origin)?;
                if self.gate.has_failed_match()
                    || (self.gate.boundary().is_some() && self.gate.pending_diagnostics().is_none())
                {
                    return Err(TestStoreError::incomplete_assertion("transaction boundary", origin));
                }
                if self.shutdown {
                    self.completed = true;
                    return Ok(());
                }
                let pending = self.gate.pending_diagnostics();
                let pending_action = pending.is_some();
                let diagnostics = if let Some(diagnostics) = pending {
                    diagnostics
                } else {
                    self.runtime
                        .run_ready(self.store.runtime_diagnostics(), &mut polls)
                        .map_err(|error| self.drive_error(error, origin))?
                        .ok_or_else(|| TestStoreError::store_stopped_from_shutdown(origin))?
                        .map_err(|error| TestStoreError::store_stopped(error, origin))?
                };
                let pending_sleeps = self.runtime.pending_sleeps();
                if pending_action
                    || has_unfinished_work(diagnostics.runtime())
                    || !pending_sleeps.is_empty()
                {
                    return Err(TestStoreError::unfinished(
                        FinishDiagnostics::new(
                            diagnostics,
                            pending_sleeps,
                            self.runtime.now(),
                            pending_action,
                        ),
                        origin,
                    ));
                }
                if !self.assertions.declared.is_empty() {
                    let expected = self.assertions.declared.unchanged();
                    self.assertions.declared.begin(expected, origin, "finish");
                    let batch = self.assertions.declared.batch();
                    let actual = self
                        .runtime
                        .run_ready(
                            self.store
                                .read(move |state| batch.capture(state, origin, "finish")),
                            &mut polls,
                        )
                        .map_err(|error| self.drive_error(error, origin))?
                        .ok_or_else(|| TestStoreError::store_stopped_from_shutdown(origin))?
                        .map_err(|error| TestStoreError::store_stopped(error, origin))??;
                    if let Err(error) = DeclaredCaptures::compare(&actual) {
                        drop(actual);
                        return Err(error);
                    }
                    self.assertions.declared.install(actual);
                    self.assertions.declared.complete();
                }
                self.runtime
                    .run_ready(self.store.shutdown(), &mut polls)
                    .map_err(|error| self.drive_error(error, origin))?
                    .ok_or_else(|| TestStoreError::store_stopped_from_shutdown(origin))?
                    .map_err(|_error| TestStoreError::store_stopped_from_shutdown(origin))?;
                self.shutdown = true;
                self.runtime
                    .run_ready(std::future::pending::<()>(), &mut polls)
                    .map_err(|error| self.drive_error(error, origin))?;
                self.check_issues(origin)?;
                self.completed = true;
                Ok(())
            }

            /// Reports unconsumed issues without removing them from the recorder.
            fn check_issues(&self, origin: &'static Location<'static>) -> Result<(), TestStoreError> {
                let issues = self.issues.snapshot();
                if issues.is_empty() {
                    Ok(())
                } else {
                    Err(TestStoreError::unconsumed(issues, origin))
                }
            }

            /// Preserves the typed driver cause and the last available actor diagnostics.
            fn drive_error(
                &self,
                error: crate::DriveError,
                origin: &'static Location<'static>,
            ) -> TestStoreError {
                TestStoreError::drive_with_snapshot(error, self.gate.latest_diagnostics(), origin)
            }
        }

        impl<State: $($send +)? 'static, Action: $($send +)? 'static, Snapshot> fmt::Debug
            for TestStore<State, Action, Snapshot>
        {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct("TestStore")
                    .field("runtime", &self.runtime)
                    .field("gate", &self.gate)
                    .field("mode", &self.mode)
                    .finish_non_exhaustive()
            }
        }

        impl<State: $($send +)? 'static, Action: $($send +)? 'static, Snapshot> Drop
            for TestStore<State, Action, Snapshot>
        {
            fn drop(&mut self) {
                /// Releases queued actors even when abort notification invokes a panicking waker.
                struct DiscardTasks<'a>(
                    /// Borrows the host; retaining a task handle must not bypass this cleanup.
                    &'a DeterministicRuntime,
                );

                impl Drop for DiscardTasks<'_> {
                    fn drop(&mut self) {
                        self.0.discard_all_tasks();
                    }
                }

                // A pre-existing panic must never poll more user work or cause a second
                // assertion panic. Inspection itself may invoke a panicking user future.
                let result = if std::thread::panicking() || self.completed {
                    None
                } else {
                    Some(std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || self.complete(Location::caller()),
                    )))
                };
                let cleanup = DiscardTasks(&self.runtime);
                self.store.abort_for_testing();
                drop(cleanup);
                match result {
                    Some(Ok(Err(error))) => panic!("TestStore teardown failed: {error}; {error:?}"),
                    Some(Err(panic)) => std::panic::resume_unwind(panic),
                    _ => {}
                }
            }
        }

        impl<State, Action, Snapshot> TestStore<State, Action, Snapshot>
        where
            State: $($send +)? 'static,
            Action: $($send +)? 'static,
            Snapshot: Clone + fmt::Debug + PartialEq + $($send +)? 'static,
        {
            /// Captures one admitted boundary and acknowledges it only after both checks pass.
            ///
            /// A skipped request supplies no boundary: its successor remains independently pending.
            pub(super) async fn verify_snapshot(
                &mut self,
                boundary: Option<u64>,
            ) -> Result<(), TestStoreError> {
                let obligation = self
                    .assertions
                    .pending
                    .get_mut()
                    .as_ref()
                    .expect("snapshot comparison requires an admitted obligation");
                let origin = obligation.origin;
                let operation = obligation.operation;
                let captured = CaptureOwner::new(Mutex::new(None));
                let output = CaptureOwner::clone(&captured);
                let capture = CaptureOwner::clone(&self.assertions.capture);
                let declarations = self.assertions.declared.batch();
                self.assert_state(move |state| {
                    let actual = capture(state);
                    let declared = declarations.capture(state, origin, operation);
                    *output
                        .lock()
                        .expect("snapshot transfer lock must not be poisoned") = Some((actual, declared));
                })
                .await?;
                let (actual, declared) = captured
                    .lock()
                    .expect("snapshot transfer lock must not be poisoned")
                    .take()
                    .expect("successful actor read must capture a snapshot");
                let declared = match declared {
                    Ok(declared) => declared,
                    Err(error) => {
                        drop(actual);
                        return Err(error);
                    }
                };
                let gate = &self.gate;
                self.assertions.verify(actual, declared, || gate.latest_diagnostics(), || {
                    if let Some(boundary) = boundary {
                        let _ = gate.acknowledge(boundary);
                    }
                })?;
                if boundary.is_some() && gate.boundary() == boundary {
                    return Err(TestStoreError::incomplete_assertion("transaction boundary", origin));
                }
                Ok(())
            }

            /// Verifies declared values without replacing the root snapshot or its boundary.
            pub(super) async fn verify_captures(
                &mut self,
                origin: &'static Location<'static>,
                operation: &'static str,
            ) -> Result<(), TestStoreError> {
                let captured = CaptureOwner::new(Mutex::new(None));
                let output = CaptureOwner::clone(&captured);
                let batch = self.assertions.declared.batch();
                self.assert_state(move |state| {
                    let actual = batch.capture(state, origin, operation);
                    *output
                        .lock()
                        .expect("capture transfer lock must not be poisoned") = Some(actual);
                })
                .await?;
                let actual = captured
                    .lock()
                    .expect("capture transfer lock must not be poisoned")
                    .take()
                    .expect("successful actor read must capture declarations")?;
                if let Err(error) = DeclaredCaptures::compare(&actual) {
                    drop(actual);
                    return Err(error);
                }
                self.assertions.declared.install(actual);
                self.assertions.declared.complete();
                Ok(())
            }

            /// Detects external declared-value changes before admitting another mutation.
            pub(super) async fn preflight_captures(
                &mut self,
                origin: &'static Location<'static>,
            ) -> Result<(), TestStoreError> {
                if self.assertions.declared.is_empty() {
                    return Ok(());
                }
                let expected = self.assertions.declared.unchanged();
                self.assertions
                    .declared
                    .begin(expected, origin, "preflight");
                self.verify_captures(origin, "preflight").await
            }
        }

    };
}

pub(super) use test_store_api;
