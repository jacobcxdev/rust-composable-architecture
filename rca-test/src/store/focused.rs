//! Executes explicitly omitted transactions while preserving strict assertion boundaries.

/// Selects diagnostics for transactions that a focused operation explicitly omits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OmissionReporting {
    /// Retains one warning per omitted transaction for `TestStore::take_omissions`.
    Warn,
    /// Adopts omitted transactions without constructing warning messages.
    Silent,
}

/// Shares focused operation logic without changing portable or owner-local bounds.
macro_rules! focused_api {
    ($core:ident, [$($send:ident)?]) => {
        impl<State, Action, Snapshot> TestStore<State, Action, Snapshot>
        where
            State: $($send +)? 'static,
            Action: $($send +)? 'static,
            Snapshot: Clone + fmt::Debug + PartialEq + $($send +)? 'static,
        {
            /// Removes explicit omission warnings without consuming production issues.
            ///
            /// Warnings describe selected snapshots only. They do not claim coverage
            /// for fields excluded by a projection. `finish` does not require consumption.
            pub fn take_omissions(&mut self) -> Vec<Issue> {
                std::mem::take(&mut self.omissions)
            }

            /// Executes one pending transaction without asserting its state changes.
            ///
            /// Unlike `skip_received`, this runs the original production command.
            /// It adopts the selected snapshot and declared captures after completion.
            /// Structural, update, output and mounted transactions use the same boundary.
            /// It neither advances virtual time nor waits for external progress.
            /// Existing unfinished assertions and terminal errors remain failures.
            ///
            /// # Errors
            ///
            /// Returns missing-work, incomplete-assertion, capture, production or budget failures.
            ///
            /// # Panics
            ///
            /// Propagates capture, comparison, rendering and destruction panics,
            /// keeping the interrupted assertion incomplete.
            #[track_caller]
            pub fn receive_omitting(
                &mut self,
                reporting: OmissionReporting,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? '_ {
                let origin = Location::caller();
                async move {
                    let mut polls = 0;
                    self.focused_ready(&mut polls, origin)?;
                    self.focused_step(
                        Some(reporting),
                        |assertions| Ok((None, assertions.declared.unchanged())),
                        &mut polls,
                        origin,
                        "receive_omitting",
                    )
                }
            }

            /// Executes earlier transactions, then strictly asserts the selected root action.
            ///
            /// `matcher` runs once per encountered root action. Other transaction kinds
            /// run without calling it. A false result selects omission, not cancellation.
            /// `update` edits the last adopted snapshot before the selected action runs.
            /// All work shares one poll budget. Virtual time never advances implicitly.
            ///
            /// # Errors
            ///
            /// Returns when no matching action is ready, or on capture, production,
            /// incomplete-assertion or budget failure. Completed omissions remain committed.
            ///
            /// # Panics
            ///
            /// Propagates callback panics. An incorrect selected expectation still panics.
            #[track_caller]
            pub fn receive_until<'a>(
                &'a mut self,
                matcher: impl FnMut(&Action) -> bool + $($send +)? 'a,
                reporting: OmissionReporting,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                self.receive_until_with_captures(matcher, reporting, update, |_| Ok(()))
            }

            /// Selects a root action with strict snapshot and declared-capture expectations.
            ///
            /// Earlier omitted transactions adopt their captured values. The selected
            /// action uses ordinary independent expectations. Failed assertions stay incomplete.
            ///
            /// # Errors
            ///
            /// Returns missing-work, capture, declaration, production, unfinished-assertion
            /// or budget failures.
            ///
            /// # Panics
            ///
            /// Propagates callback panics and incorrect snapshot expectations.
            #[track_caller]
            pub fn receive_until_with_captures<'a>(
                &'a mut self,
                mut matcher: impl FnMut(&Action) -> bool + $($send +)? 'a,
                reporting: OmissionReporting,
                update: impl FnOnce(&mut Snapshot) + $($send +)? 'a,
                captures: impl FnOnce(&mut CaptureExpectations<'_>) -> Result<(), TestStoreError>
                    + $($send +)? 'a,
            ) -> impl Future<Output = Result<TestActionTask, TestStoreError>> + $($send +)? 'a {
                let origin = Location::caller();
                async move {
                    let mut polls = 0;
                    loop {
                        self.focused_ready(&mut polls, origin)?;
                        if self.gate.inspect_pending(&mut matcher) == Some(true) {
                            return self.focused_step(
                                None,
                                move |assertions| {
                                    let expected = assertions.expected(update);
                                    let declared = assertions.declared.expected(captures)?;
                                    Ok((Some(expected), declared))
                                },
                                &mut polls,
                                origin,
                                "receive_until",
                            );
                        }
                        drop(self.focused_step(
                            Some(reporting),
                            |assertions| Ok((None, assertions.declared.unchanged())),
                            &mut polls,
                            origin,
                            "receive_until",
                        )?);
                    }
                }
            }

            /// Rejects prior debt before driving or inspecting the next pending transaction.
            fn focused_ready(
                &mut self,
                polls: &mut usize,
                origin: &'static Location<'static>,
            ) -> Result<(), TestStoreError> {
                if let Some(error) = self.assertions.incomplete_error() {
                    return Err(error);
                }
                if self.gate.has_failed_match() {
                    return Err(TestStoreError::incomplete_assertion(
                        "transaction boundary",
                        origin,
                    ));
                }

                self.runtime
                    .run_ready(std::future::pending::<()>(), polls)
                    .map_err(|error| self.drive_error(error, origin))?;
                if self.gate.pending_kind().is_none() {
                    return Err(if self.gate.boundary().is_some() {
                        TestStoreError::incomplete_assertion("transaction boundary", origin)
                    } else {
                        TestStoreError::no_received_action(origin)
                    });
                }

                Ok(())
            }

            /// Executes one selected boundary using the caller's remaining poll budget.
            fn focused_step(
                &mut self,
                omission: Option<OmissionReporting>,
                prepare: impl FnOnce(
                    &mut Assertions<State, Snapshot>,
                ) -> Result<(Option<Snapshot>, ExpectedCaptures<State>), TestStoreError>,
                polls: &mut usize,
                origin: &'static Location<'static>,
                operation: &'static str,
            ) -> Result<TestActionTask, TestStoreError> {
                // External declared changes predate this omission and remain assertions.
                if !self.assertions.declared.is_empty() {
                    let expected = self.assertions.declared.unchanged();
                    self.assertions.declared.begin(expected, origin, "preflight");
                    let batch = self.assertions.declared.batch();
                    let actual = self.focused_read(polls, origin, move |state| {
                        batch.capture(state, origin, "preflight")
                    })??;
                    DeclaredCaptures::compare(&actual)?;
                    self.assertions.declared.install(actual);
                    self.assertions.declared.complete();
                }

                let (kind, boundary) = self
                    .gate
                    .pending_kind()
                    .zip(self.gate.boundary())
                    .ok_or_else(|| TestStoreError::no_received_action(origin))?;
                let (expected, declared) = prepare(&mut self.assertions)?;
                let receipt = self
                    .gate
                    .release_pending_with_receipt()
                    .ok_or_else(|| TestStoreError::no_received_action(origin))?;
                if let Some(expected) = expected {
                    self.assertions.begin(expected, origin, operation);
                } else {
                    self.assertions.begin_omission(origin, operation);
                }
                self.assertions.declared.begin(declared, origin, operation);

                let task = self
                    .runtime
                    .run_ready(receipt.finish(), polls)
                    .map_err(|error| self.drive_error(error, origin))?
                    .ok_or_else(|| TestStoreError::incomplete_assertion(operation, origin))?
                    .map_err(|error| TestStoreError::store_stopped(error, origin))?;
                self.runtime
                    .run_ready(std::future::pending::<()>(), polls)
                    .map_err(|error| self.drive_error(error, origin))?;

                let capture = CaptureOwner::clone(&self.assertions.capture);
                let batch = self.assertions.declared.batch();
                let (actual, declared) = self.focused_read(polls, origin, move |state| {
                    (capture(state), batch.capture(state, origin, operation))
                })?;
                let declared = declared?;

                let warning = if omission == Some(OmissionReporting::Warn) {
                    let description = self.assertions.omission_description(&actual, &declared);
                    let mut issue = Issue::new(
                        "test_store.omitted_transaction",
                        format!("Omitted {kind:?} transaction: {description}"),
                    );
                    issue.location = origin;
                    Some(issue)
                } else {
                    None
                };

                // Retire only this successfully captured generation. A concurrent
                // task driver may expose its successor as soon as it is acknowledged.
                if omission.is_some() {
                    self.assertions.adopt_omission(actual, declared);
                    let _ = self.gate.acknowledge(boundary);
                } else {
                    let gate = &self.gate;
                    self.assertions.verify(
                        actual,
                        declared,
                        || gate.latest_diagnostics(),
                        || {
                            let _ = gate.acknowledge(boundary);
                        },
                    )?;
                }

                if let Some(warning) = warning {
                    self.omissions.push(warning);
                }
                self.runtime
                    .run_ready(std::future::pending::<()>(), polls)
                    .map_err(|error| self.drive_error(error, origin))?;
                Ok(TestActionTask::new(
                    task,
                    self.runtime.clone(),
                    origin,
                    self.gate.clone(),
                ))
            }

            /// Captures one actor-side value without resetting the budget or waiting externally.
            fn focused_read<Value: $($send +)? 'static>(
                &self,
                polls: &mut usize,
                origin: &'static Location<'static>,
                read: impl FnOnce(&State) -> Value + $($send +)? 'static,
            ) -> Result<Value, TestStoreError> {
                let captured = CaptureOwner::new(Mutex::new(None));
                let output = CaptureOwner::clone(&captured);
                let assertion = move |state: &State| {
                    let value = read(state);
                    *output
                        .lock()
                        .expect("snapshot transfer lock must not be poisoned") = Some(value);
                };
                // Both callers read before acknowledging their transaction boundary.
                let receipt = self.gate.assert_state(assertion).finish();
                self.runtime
                    .run_ready(receipt, polls)
                    .map_err(|error| self.drive_error(error, origin))?
                    .ok_or_else(|| TestStoreError::incomplete_assertion("focused capture", origin))?
                    .map_err(|error| TestStoreError::store_stopped(error, origin))?;
                Ok(captured
                    .lock()
                    .expect("snapshot transfer lock must not be poisoned")
                    .take()
                    .expect("successful actor read captures a value"))
            }
        }
    };
}

pub(super) use focused_api;
