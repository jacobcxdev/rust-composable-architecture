//! Provides caller-owned change tracing and lifecycle observation.
//!
//! The `diagnostics` feature adds [`ReducerExt::trace_changes`](crate::ReducerExt::trace_changes).
//! Capture independent snapshots: shared interior-mutable handles do not freeze prior state.
//! Each debug reduction captures before and after, compares semantic equality, and calls
//! the sink before Store commit acknowledgement. This is not post-commit logging or
//! effect-completion observation. No logger, executor, or global sink is installed.
//! [`checkpoint()`] observes caller-labelled operations with borrowed snapshots and
//! success/error metadata. [`checkpoint_disabled`] and the explicit disabled
//! reducer choice execute their underlying operation without observation bounds.
//!
//! Without debug assertions, only the change-traced reducer runs. Its wrapper still owns
//! capture and sink resources until dropped; its storage is not erased. Panics
//! propagate without rollback guarantees, including after state mutation but before
//! the returned effect can reach Store.
//! Snapshot destructors also run before the effect leaves the wrapper. Their
//! panics can prevent effect delivery even after the sink returns normally.
//!
//! [`LifecycleEvent`], [`LifecycleKind`] and [`CancellationCause`] describe lifecycle
//! facts without retaining State, routes, tasks or completion leases. Register a
//! callback with [`StoreBuilder::with_lifecycle_diagnostics`] or the corresponding
//! Feature/local builder method. Unlike change tracing,
//! lifecycle delivery runs in both debug and release builds. It reports actual
//! installation, retirement, configuration, cancellation and normal-stop facts,
//! not value differences or work completion. No logger or history is retained.
//!
//! The actor invokes this callback synchronously outside framework locks. It may
//! copy the small metadata value, but must not block waiting for the same actor.
//! Callback panics propagate without rollback, and delivery is not promised
//! during unwinding or forced destruction of an unpolled or suspended actor.
//!
//! [`StoreBuilder::with_lifecycle_diagnostics`]: crate::StoreBuilder::with_lifecycle_diagnostics
//!
//! # Examples
//!
//! Count lifecycle facts without formatting or requiring a `Sync` callback:
//!
//! ```
//! use rca_core::{Effect, Reduce, Runtime, Store, StoreBuildError};
//! use std::cell::Cell;
//!
//! fn instrument(runtime: Runtime) -> Result<Store<usize, ()>, StoreBuildError> {
//!     let calls = Cell::new(0);
//!     Store::builder(0, Reduce::new(|_: &mut usize, _: &(), _| Effect::none()), runtime)
//!         .with_lifecycle_diagnostics(move |_event| calls.set(calls.get() + 1))
//!         .build()
//! }
//! ```
//!
//! Convenient Debug output can be sent to a caller-selected destination:
//!
//! ```
//! # use rca_core::{Effect, ReduceContext, Reducer, ReducerExt};
//! # struct Counter;
//! # impl Reducer for Counter {
//! #     type State = usize;
//! #     type Action = usize;
//! #     fn reduce(
//! #         &mut self,
//! #         state: &mut usize,
//! #         action: &usize,
//! #         _: &mut ReduceContext<'_>,
//! #     ) -> Effect<usize> {
//! #         *state += action;
//! #         Effect::none()
//! #     }
//! # }
//! let reducer = Counter.trace_changes(|state| *state, |change| {
//!     eprintln!("{}", change.format());
//! });
//! ```
//!
//! Custom rendering controls every displayed domain value, without Debug fallback:
//!
//! ```
//! # use rca_core::{Effect, ReduceContext, Reducer, ReducerExt};
//! # struct Feature;
//! # impl Reducer for Feature {
//! #     type State = String;
//! #     type Action = String;
//! #     fn reduce(
//! #         &mut self,
//! #         state: &mut String,
//! #         action: &String,
//! #         _: &mut ReduceContext<'_>,
//! #     ) -> Effect<String> {
//! #         state.clone_from(action);
//! #         Effect::none()
//! #     }
//! # }
//! let reducer = Feature.trace_changes(Clone::clone, |change| {
//!     let redacted = change.format_with(
//!         |_| "[redacted action]".to_owned(),
//!         |_| "[redacted state]".to_owned(),
//!     );
//!     eprintln!("{redacted}");
//! });
//! ```

use core::fmt;

use crate::{Effect, ReduceContext, Reducer};

/// Owns selected graph data without retaining an implicit live model.
mod capture;
/// Observes explicit synchronous operations without an ambient registry.
mod checkpoint;
/// Limits displayed text without deciding semantic equality.
mod diff;
/// Delivers actor lifecycle facts through scoped observer borrows.
mod lifecycle;
/// Renders selected graphs through caller-controlled structural views.
mod structural;

pub(crate) use lifecycle::LifecycleCall;

#[doc(inline)]
pub use capture::{
    CaptureError, CaptureLimits, CapturedEdge, CapturedGraph, CapturedNode, NodeCapture,
    ReferenceKind, capture_graph,
};

#[doc(inline)]
pub use checkpoint::{Checkpoint, CheckpointOutcome, checkpoint, checkpoint_disabled};

#[doc(inline)]
pub use structural::{
    DiagnosticError, DiagnosticLimits, StructuralDiff, StructuralToken, StructuralView, ViewChange,
    diff_graph_views, write_graph_view,
};

/// Describes a lifecycle fact without retaining runtime authority.
///
/// All fields are inert scalar metadata. Copying an event retains no State,
/// Feature, route, task, completion lease or permission to access a Store.
/// A borrowed event need not outlive its synchronous observer call; copy the
/// value when retaining metadata. This type has no lifetime parameter or public
/// constructor and does not itself register an observer or emit events.
///
/// # Observation semantics
///
/// A lifecycle fact is not a transaction acknowledgement or work-completion
/// signal. Its version is the last irreversible commit stamp reached at
/// observation, not a stamp promised by the fact. Structural installation,
/// retirement and configuration changes precede the current transaction's stamp;
/// they can be observed even when that transaction subsequently fails. Compiler
/// cancellation follows the stamp, while control and stop observations leave it
/// unchanged. None of these observations promises rollback.
///
/// When used by a lifecycle observer, delivery is synchronous on the actor.
/// Blocking the observer blocks that actor; do not synchronously wait for the
/// same actor. Reentrant admission is queued rather than recursively executed.
/// Observer panics propagate, so later facts are not guaranteed. Observers are
/// not invoked from destructors or during unwinding. Lifecycle observations are
/// independent of the debug-only snapshot capture performed by [`TraceChanges`].
///
/// Lifecycle ordering follows actor execution, not wall-clock ordering across
/// Stores. Subtree retirement is child before parent in declared sibling order;
/// a replacement's retirement precedes its fresh installation. Installation is
/// parent before child. Reordering alone does not imply installation or retirement.
/// No complete history is implied: destruction, unwinding, or a cancelled actor
/// future that never resumes need not produce terminal observations. Missing
/// retirement metadata does not mean authority survives Store closure.
///
/// # Examples
///
/// Retaining a metadata copy does not retain the observer's event borrow:
///
/// ```
/// use rca_core::diagnostics::LifecycleEvent;
///
/// fn retain_metadata(event: &LifecycleEvent) -> LifecycleEvent {
///     *event
/// }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecycleEvent {
    /// Last completed commit stamp, not a promised version for this fact.
    last_committed_version: u64,
    /// Scalar metadata with no retained runtime authority.
    kind: LifecycleKind,
}

impl LifecycleEvent {
    /// Returns the last irreversible commit stamp reached when this fact was observed.
    ///
    /// This is not a promised commit version for the reported change. Structural
    /// facts may carry the previous version even when the current transaction
    /// later fails. The stamp is local to its Store.
    #[must_use]
    pub const fn last_committed_version(&self) -> u64 {
        self.last_committed_version
    }

    /// Returns the observed fact as an independent metadata value.
    #[must_use]
    pub const fn kind(&self) -> LifecycleKind {
        self.kind
    }
}

/// Identifies an observed lifecycle change or cancellation request.
///
/// Lifetime numbers identify incarnations within one Store; root is zero. They
/// are not globally comparable, domain keys, addresses or usable capabilities.
/// Callers combining observations from multiple Stores must label their source.
/// Configuration numbers are Store-assigned revisions, not user-supplied markers.
/// New kinds may be added; match with a wildcard for unrecognised variants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LifecycleKind {
    /// Reports actual installation of a fresh lifetime.
    ///
    /// Equal identity reuses an existing lifetime without another installation.
    /// Explicit replacement creates a fresh lifetime even when public State and
    /// domain keys compare equal. An unobservable remove/reinsert within one
    /// reducer call does not manufacture a replacement.
    Installed {
        /// Store-local incarnation number; zero identifies the root.
        lifetime: u64,
        /// Parent incarnation, or `None` for the root.
        parent: Option<u64>,
        /// Configuration revision when known; legacy mounts use `None`, not invented zero.
        configuration: Option<u64>,
    },
    /// Reports authority removal, not physical destruction of retained work.
    ///
    /// Repeated stale retirement produces no new fact. Store-wide destruction
    /// need not enumerate retired children; closure invalidates their authority
    /// without requiring individual observations.
    Retired {
        /// Store-local incarnation whose authority was removed.
        lifetime: u64,
    },
    /// Reports configuration refresh while preserving the lifetime and Feature cache.
    ///
    /// Old route authority is invalidated and the new revision installed before
    /// this fact is observed. This is not a retirement followed by installation.
    ConfigurationChanged {
        /// Store-local incarnation retained across the configuration change.
        lifetime: u64,
        /// Configuration revision before refresh.
        previous: u64,
        /// Configuration revision after refresh.
        current: u64,
    },
    /// Reports an applied cancellation request, not completed work.
    ///
    /// A request can affect no work. It neither counts released bodies nor
    /// identifies cancellation keys or named-operation generations. Runtime
    /// futures may remain physically retained, and operation waits may remain
    /// pending after this fact.
    CancellationRequested {
        /// Actor path that applied the request.
        cause: CancellationCause,
        /// Known Store-local scope, or `None` for aggregate control or stop requests.
        scope: Option<u64>,
    },
    /// Reports known ingress closure during normal actor finalisation.
    ///
    /// This delayed observation is not the wall-clock instant of closure. It
    /// precedes the actor-stop cancellation and stopped facts on that normal path.
    IngressClosed,
    /// Reports normal finalisation, not graceful success or physical work completion.
    ///
    /// Actor-owned destruction and issue closure have returned. Runtime-owned
    /// effect bodies may still exist. Destruction or unwinding alone does not
    /// guarantee this observation, including dropping a never-polled actor.
    ActorStopped,
}

/// Identifies why the actor applied a cancellation request.
///
/// Causes describe requests, not successful cancellation, business outcomes or
/// release of runtime-owned work. New causes may be added; match with a wildcard
/// for unrecognised variants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CancellationCause {
    /// Requests cancellation for a retired scope after authority removal.
    ScopeRetirement,
    /// Applies an explicit cancellation declaration or ordered operation-cancel request.
    Explicit,
    /// Cancels an old generation before registering replacement work.
    Replacement,
    /// Applies previously pending cancellation during an actor control pass.
    ///
    /// One aggregate fact describes that pass, not an ordering among tasks.
    /// Repeated wakes without a newly applied request produce no new fact.
    Control,
    /// Applies Store-wide invalidation observed during normal actor finalisation.
    ///
    /// This does not promise graceful shutdown or destruction of every retained
    /// runtime future. No callback is implied from an actor destructor.
    ActorStop,
}

/// Wraps a reducer with debug-only snapshot capture and change delivery.
///
/// Snapshot values need neither `Send` nor `Sync`: they exist only during the
/// synchronous reduction. The wrapper stores only the reducer, capture, and sink.
pub struct TraceChanges<R, Capture, Sink> {
    /// Executes the original reduction and owns its unchanged effect result.
    reducer: R,
    /// Captures independent values in debug builds; remains owned in release builds.
    #[cfg_attr(
        not(debug_assertions),
        expect(
            dead_code,
            reason = "release retains capture ownership without invocation"
        )
    )]
    capture: Capture,
    /// Receives borrowed snapshots synchronously; remains owned in release builds.
    #[cfg_attr(
        not(debug_assertions),
        expect(
            dead_code,
            reason = "release retains sink ownership without invocation"
        )
    )]
    sink: Sink,
}

impl<R, Capture, Sink> TraceChanges<R, Capture, Sink> {
    /// Retains callbacks without capturing state or invoking user code.
    pub(crate) const fn new(reducer: R, capture: Capture, sink: Sink) -> Self {
        Self {
            reducer,
            capture,
            sink,
        }
    }
}

impl<R, Capture, Sink> fmt::Debug for TraceChanges<R, Capture, Sink> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TraceChanges").finish_non_exhaustive()
    }
}

impl<R, Capture, Sink, Snapshot> Reducer for TraceChanges<R, Capture, Sink>
where
    R: Reducer,
    Capture: Fn(&R::State) -> Snapshot,
    Sink: for<'a> FnMut(Change<'a, R::Action, Snapshot>),
    Snapshot: PartialEq,
{
    type State = R::State;
    type Action = R::Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        #[cfg(debug_assertions)]
        let before = (self.capture)(state);
        let effect = self.reducer.reduce(state, action, context);
        #[cfg(debug_assertions)]
        {
            let after = (self.capture)(state);
            let changed = before != after;
            (self.sink)(Change {
                action,
                before: &before,
                after: &after,
                changed,
            });
            // Keep the effect local until snapshot cleanup succeeds. A panic
            // after moving it into the return value would leak its payload.
            drop(after);
            drop(before);
            effect
        }
        #[cfg(not(debug_assertions))]
        effect
    }
}

/// Borrows one action and its independent before-and-after snapshots.
///
/// This event cannot outlive its synchronous sink call. Semantic inequality is
/// established before delivery, independent of formatting. Debug shows metadata
/// only; domain values are never included implicitly.
pub struct Change<'a, Action, Snapshot> {
    /// Original action borrowed for this reduction, never copied for diagnostics.
    action: &'a Action,
    /// Independent value captured before the reducer runs.
    before: &'a Snapshot,
    /// Independent value captured after the reducer returns its effect.
    after: &'a Snapshot,
    /// Cached semantic comparison, independent of any rendered representation.
    changed: bool,
}

impl<'a, Action, Snapshot> Change<'a, Action, Snapshot> {
    /// Returns the action borrowed during this reduction.
    #[must_use]
    pub const fn action(&self) -> &'a Action {
        self.action
    }

    /// Returns the snapshot captured before reduction.
    #[must_use]
    pub const fn before(&self) -> &'a Snapshot {
        self.before
    }

    /// Returns the snapshot captured after reduction.
    #[must_use]
    pub const fn after(&self) -> &'a Snapshot {
        self.after
    }

    /// Reports semantic inequality without comparing snapshots again.
    #[must_use]
    pub const fn is_changed(&self) -> bool {
        self.changed
    }

    /// Renders bounded action output and state changes using Debug.
    ///
    /// Use [`Self::format_with`] when values need redaction. Debug bounds apply
    /// only here, not to the reducer, capture, or structured sink.
    ///
    /// # Panics
    ///
    /// Panics if a domain Debug implementation panics.
    #[must_use]
    pub fn format(&self) -> String
    where
        Action: fmt::Debug,
        Snapshot: fmt::Debug,
    {
        self.format_with(
            |action| format!("{action:#?}"),
            |snapshot| format!("{snapshot:#?}"),
        )
    }

    /// Renders diagnostics exclusively through caller-supplied domain renderers.
    ///
    /// The action renderer runs once; the snapshot renderer runs twice only
    /// for semantic changes. Unchanged state says `state unchanged`. Identical
    /// rendered snapshots still report semantic inequality, without fallback.
    /// Action output shows at most sixteen lines and 240 Unicode scalar values
    /// per displayed line, including prefixes and truncation markers. State uses
    /// a bounded contextual diff. These bounds limit output, not renderer work
    /// or allocation. Newline-only differences remain visible.
    ///
    /// # Panics
    ///
    /// Panics if a supplied renderer or its destructor panics.
    #[must_use]
    pub fn format_with(
        &self,
        action: impl FnOnce(&Action) -> String,
        snapshot: impl Fn(&Snapshot) -> String,
    ) -> String {
        let mut output = diff::action(&action(self.action));
        if self.changed {
            diff::append_excerpt(
                &mut output,
                &snapshot(self.before),
                &snapshot(self.after),
                "before",
                "after",
            );
        } else {
            output.push_str("state unchanged");
        }
        // A renderer destructor may panic even when no snapshots were rendered.
        drop(snapshot);
        output
    }
}

impl<Action, Snapshot> fmt::Debug for Change<'_, Action, Snapshot> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Change")
            .field("is_changed", &self.changed)
            .finish_non_exhaustive()
    }
}

/// Formats bounded context around semantically unequal rendered snapshots.
///
/// The caller must already have established semantic inequality. Identical
/// strings retain an explicit inequality sentence; rendered equality never
/// decides domain equality. Labels are `expected` and `actual` for `TestStore`.
/// Each side shows up to sixteen changed lines and three context lines on
/// either side. Displayed lines contain at most 240 Unicode scalar values,
/// including prefixes and truncation markers. These limits do not bound input
/// scanning or the temporary line-index storage.
#[must_use]
pub fn format_snapshot_mismatch(expected: &str, actual: &str) -> String {
    diff::excerpt(expected, actual, "expected", "actual")
}

#[cfg(test)]
ordinary_test_items! {
mod callback_tests {
    use super::*;
    use crate::{DependencyValues, MonotonicInstant, store::scope::ScopeRegistry};
    use std::cell::RefCell;
    #[cfg(debug_assertions)]
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::rc::Rc;

    struct Counter<'a>(&'a RefCell<Vec<&'static str>>);

    impl Reducer for Counter<'_> {
        type State = usize;
        type Action = usize;

        fn reduce(
            &mut self,
            state: &mut usize,
            action: &usize,
            _: &mut ReduceContext<'_>,
        ) -> Effect<usize> {
            self.0.borrow_mut().push("reduce");
            *state += action;
            Effect::send(7)
        }
    }

    struct Snapshot<'a>(usize, &'a RefCell<Vec<&'static str>>);

    impl PartialEq for Snapshot<'_> {
        fn eq(&self, other: &Self) -> bool {
            self.1.borrow_mut().push("compare");
            self.0 == other.0
        }
    }

    struct Probe(Rc<RefCell<usize>>);

    impl Drop for Probe {
        fn drop(&mut self) {
            *self.0.borrow_mut() += 1;
        }
    }

    // Removing a phase, reordering delivery or erasing the effect breaks this.
    #[test]
    fn local_callbacks_preserve_order_effects_and_owned_resources() {
        use crate::ReducerExt;
        let events = Rc::new(RefCell::new(Vec::new()));
        let drops = Rc::new(RefCell::new(0));
        let offset = 3;
        let capture_probe = Probe(Rc::clone(&drops));
        let sink_probe = Probe(Rc::clone(&drops));
        let local_events = &*events;
        let borrowed_offset = &offset;
        let mut reducer = Counter(&events).trace_changes(
            move |state| {
                let _ = &capture_probe;
                local_events.borrow_mut().push("capture");
                Snapshot(state + borrowed_offset, local_events)
            },
            move |change| {
                let _ = &sink_probe;
                local_events.borrow_mut().push("sink");
                assert_eq!(
                    (*change.action(), change.before().0, change.after().0),
                    (2, 4, 6)
                );
                assert!(change.is_changed());
            },
        );
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );
        let mut state = 1;
        let effect = reducer.reduce(&mut state, &2, &mut context);
        assert_eq!(state, 3);
        assert!(matches!(effect.program, crate::effect::Program::Send(7)));
        assert_eq!(*drops.borrow(), 0);
        if cfg!(debug_assertions) {
            assert_eq!(
                *events.borrow(),
                ["capture", "reduce", "capture", "compare", "sink"]
            );
        } else {
            assert_eq!(*events.borrow(), ["reduce"]);
        }
        drop(reducer);
        assert_eq!(*drops.borrow(), 2);
    }

    // A sink panic propagates after mutation and retains wrapper-owned captures.
    #[test]
    #[cfg(debug_assertions)]
    fn borrowed_sink_panic_preserves_mutation_and_drops_resources() {
        use crate::ReducerExt;
        let events = Rc::new(RefCell::new(Vec::new()));
        let drops = Rc::new(RefCell::new(0));
        let sink_probe = Probe(Rc::clone(&drops));
        let expected = 3;
        let borrowed_expected = &expected;
        let mut reducer = Counter(&events).trace_changes(
            |state| *state,
            move |change| {
                let _ = &sink_probe;
                assert_eq!(*change.after(), *borrowed_expected);
                panic!("local sink");
            },
        );
        let mut dependencies = DependencyValues::new();
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut context_transitions,
        );
        let mut state = 1;
        catch_unwind(AssertUnwindSafe(|| {
            reducer.reduce(&mut state, &2, &mut context)
        }))
        .unwrap_err();
        assert_eq!(state, 3);
        assert_eq!(*events.borrow(), ["reduce"]);
        assert_eq!(*drops.borrow(), 0);
        drop(reducer);
        assert_eq!(*drops.borrow(), 1);
    }

    // Post-reduction panics must release the unreturned effect and its snapshots.
    #[test]
    #[cfg(debug_assertions)]
    fn snapshot_panics_release_effects_and_leave_parent_dependencies_unchanged() {
        use crate::{DependencyKey, ReducerExt};
        use std::cell::Cell;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Number;
        impl DependencyKey for Number {
            type Value = usize;
        }

        struct EffectProbe(Arc<AtomicUsize>);

        impl Drop for EffectProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        struct Emitting(Arc<AtomicUsize>);
        impl Reducer for Emitting {
            type State = usize;
            type Action = Option<EffectProbe>;

            fn reduce(
                &mut self,
                state: &mut usize,
                _: &Option<EffectProbe>,
                context: &mut ReduceContext<'_>,
            ) -> Effect<Option<EffectProbe>> {
                *state = *context.dependency::<Number>();
                Effect::send(Some(EffectProbe(Arc::clone(&self.0))))
            }
        }

        struct FallibleSnapshot<'a> {
            value: usize,
            stage: &'static str,
            drops: &'a Cell<usize>,
        }

        impl PartialEq for FallibleSnapshot<'_> {
            fn eq(&self, other: &Self) -> bool {
                assert_ne!(self.stage, "comparison", "snapshot comparison");
                self.value == other.value
            }
        }

        impl Drop for FallibleSnapshot<'_> {
            fn drop(&mut self) {
                self.drops.set(self.drops.get() + 1);
                assert!(
                    !(self.stage == "drop after" && self.value == 7
                        || self.stage == "drop before" && self.value == 1),
                    "snapshot destructor"
                );
            }
        }

        for stage in ["comparison", "drop after", "drop before"] {
            let effect_drops = Arc::new(AtomicUsize::new(0));
            let snapshot_drops = Cell::new(0);
            let sinks = Cell::new(0);
            let mut reducer = Emitting(Arc::clone(&effect_drops)).trace_changes(
                |state| FallibleSnapshot {
                    value: *state,
                    stage,
                    drops: &snapshot_drops,
                },
                |_| sinks.set(sinks.get() + 1),
            );
            let mut dependencies = DependencyValues::new();
            dependencies.set::<Number>(3);
            let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let mut transitions = crate::store::scope::ScopeTransitionBatch::default();
            let mut context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut transitions,
            );
            let mut state = 1;

            catch_unwind(AssertUnwindSafe(|| {
                context.with_dependency_override::<Number, _>(7, |child| {
                    reducer.reduce(&mut state, &None, child)
                })
            }))
            .unwrap_err();

            assert_eq!(state, 7);
            assert_eq!(*context.dependency::<Number>(), 3);
            assert_eq!(effect_drops.load(Ordering::Relaxed), 1);
            assert_eq!(snapshot_drops.get(), 2);
            assert_eq!(sinks.get(), usize::from(stage != "comparison"));
        }
    }
}
}

#[cfg(all(test, feature = "allocation-measurement", feature = "diagnostics"))]
pub(crate) use diff::rendered_lines_need_no_scratch_allocations;

#[cfg(test)]
mod lifecycle_metadata_tests {
    use super::{CancellationCause, LifecycleEvent, LifecycleKind};

    // Keep metadata copyable and transferable after borrowed observation, with
    // no owned drop state. The size ceiling allows enum tags and scalar alignment;
    // it is a representation budget, not an ABI or runtime allocation guarantee.
    const _: () = {
        const fn inert<T: Copy + Eq + core::fmt::Debug + Send + Sync + 'static>() {}
        inert::<LifecycleEvent>();
        inert::<LifecycleKind>();
        inert::<CancellationCause>();
        assert!(core::mem::size_of::<LifecycleEvent>() <= 64);
        assert!(!core::mem::needs_drop::<LifecycleEvent>());
    };

    // Catches an accessor changing or losing the metadata that a caller copies
    // out of a temporary borrow. This does not simulate runtime event delivery.
    #[test]
    fn copied_lifecycle_metadata_outlives_the_event_borrow() {
        let retained = {
            let event = LifecycleEvent {
                last_committed_version: 41,
                kind: LifecycleKind::CancellationRequested {
                    cause: CancellationCause::ScopeRetirement,
                    scope: Some(7),
                },
            };
            let borrow = &event;
            *borrow
        };

        assert_eq!(retained.last_committed_version(), 41);
        assert_eq!(
            retained.kind(),
            LifecycleKind::CancellationRequested {
                cause: CancellationCause::ScopeRetirement,
                scope: Some(7),
            }
        );
    }
}
