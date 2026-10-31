//! Owner-thread Stores and work for values that need not implement `Send`.
//!
//! The caller supplies an owner-thread [`Runtime`]. State, actions and asynchronous
//! work can contain values such as [`std::rc::Rc`]. The runtime polls and drops that
//! work on its owner thread. Local handles cannot move their payloads to another thread.
//!
//! [`Store`], [`Reducer`], [`Effect`] and [`Task`] are distinct from the root types.
//! They share the same transaction and cancellation rules. Shared supporting types,
//! such as [`crate::ReduceContext`] and [`crate::InputGate`], keep their root paths.
//! Local keyed-child keys need not implement `Send` or `Sync`. Optional-child
//! identity keys retain both bounds. Dependency values and [`crate::IssueReporter`]
//! handlers also retain their thread-safety requirements.
//! Local execution does not interrupt a blocking callback.
//!
//! # Examples
//!
//! ```
//! use std::cell::Cell;
//! use std::rc::Rc;
//!
//! use rca_core::local::{Effect, Reduce, Runtime, Store};
//! use rca_core::ReduceContext;
//!
//! // The caller supplies the runtime that owns this non-Send state.
//! fn counter(runtime: Runtime) -> Store<Rc<Cell<usize>>, ()> {
//!     let reducer = Reduce::new(|count: &mut Rc<Cell<usize>>, _: &(), _: &mut ReduceContext<'_>| {
//!         count.set(count.get() + 1);
//!         Effect::none()
//!     });
//!
//!     Store::new(Rc::new(Cell::new(0)), reducer, runtime)
//! }
//! ```

#[doc(inline)]
pub use crate::observation::local::{Observation, PredicateWait};
#[doc(inline)]
pub use crate::runtime::local::{
    Clock, ClockDriver, ClockTimer, Runtime, RuntimeDriver, RuntimeFuture,
};
#[doc(inline)]
pub use crate::store::local::{
    CommandPort, CommandReceipt, CommandSite, Effect, EffectSender, EntryMount, Feature,
    FeatureContext, Fold, FoldView, MountChildren, MountSite, Operation, OptionalMount, Output,
    OutputHandlers, Reduce, Reducer, RemovalSite, RemovedChild, RemovedOutputError, RootMount,
    Store, StoreBuilder, Task, TaskContext, TryCommandError,
};
