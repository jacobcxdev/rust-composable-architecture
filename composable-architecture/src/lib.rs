#![doc = include_str!("../../README.md")]
// #![cfg_attr(docsrs, feature(doc_auto_cfg))] // feature removed in 1.92.0
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(unsafe_code)]
#![allow(missing_docs)]
#![allow(dead_code)]

#[doc(no_inline)]
pub use derive_macros::*;
#[doc(inline)]
pub use effects::{Interval, Task};
pub use reducer::Reducer;
pub use store::{testing::TestClock, testing::TestStore, Store};
pub mod dependencies;

#[path = "../../about/mod.rs"]
pub mod about;

/// A convenience trait for reducer effect handles.
///
/// The “real” effect API lives in [`crate::effects::Effects`]. This `composable::Effects` trait exists
/// purely to make reducer signatures ergonomic while enforcing a `'static` bound.
///
/// In practice, most reducers should accept `send` as:
///
/// ```rust
/// # use composable::{Effects, Reducer};
/// # #[derive(Clone, Debug, PartialEq)]
/// # enum Action { A }
/// # #[derive(Clone, Debug, Default, PartialEq)]
/// # struct State;
/// impl Reducer for State {
///     type Action = Action;
///     type Output = Self;
///
///     fn reduce(&mut self, action: Action, send: impl Effects<Action>) {
///         let _ = (action, send);
///     }
/// }
/// ```
///
/// ## Scoping effects to child actions
///
/// A parent reducer can “scope” its effects down to child actions as long as the parent action type
/// can be constructed from the child action type.
///
/// ```rust
/// use composable::Effects;
///
/// #[derive(Clone, Debug, PartialEq)]
/// enum ChildAction {
///     Ping,
/// }
///
/// #[derive(Clone, Debug, PartialEq)]
/// enum ParentAction {
///     Child(ChildAction),
/// }
///
/// impl From<ChildAction> for ParentAction {
///     fn from(value: ChildAction) -> Self {
///         ParentAction::Child(value)
///     }
/// }
///
/// fn child(send: impl Effects<ChildAction>) {
///     send.action(ChildAction::Ping);
/// }
///
/// fn parent(send: impl Effects<ParentAction>) {
///     child(send.scope());
/// }
/// ```
///
/// ## Scoping effects to keyed child actions
///
/// Keyed scoping routes effects back through the parent action type while preserving a key
/// identifying which child instance should handle the follow-up action.
///
/// ```rust
/// use composable::{Effects, Keyed};
///
/// #[derive(Clone, Debug, PartialEq)]
/// struct Id(u32);
///
/// #[derive(Clone, Debug, PartialEq)]
/// enum ChildAction {
///     Pong,
/// }
///
/// #[derive(Clone, Debug, PartialEq)]
/// enum ParentAction {
///     Child(Keyed<Id, ChildAction>),
/// }
///
/// impl From<Keyed<Id, ChildAction>> for ParentAction {
///     fn from(value: Keyed<Id, ChildAction>) -> Self {
///         ParentAction::Child(value)
///     }
/// }
///
/// fn child(send: impl Effects<ChildAction>) {
///     send.action(ChildAction::Pong);
/// }
///
/// fn parent(send: impl Effects<ParentAction>) {
///     child(send.scope_keyed(Id(1)));
/// }
/// ```
pub trait Effects<Action>: effects::Effects<Action = Action> + 'static {}

/// Until actual [trait aliases] are stabilized this [work around] allows the trait shown above
/// to be used anywhere that the [original trait] can.
///
/// [trait aliases]: https://github.com/rust-lang/rust/issues/63063
/// [work around]: https://github.com/rust-lang/rust/issues/41517#issuecomment-1100644808
/// [original trait]: crate::effects::Effects
impl<T, Action> Effects<Action> for T where T: effects::Effects<Action = Action> + 'static {}

pub mod derive_macros;
pub mod effects;
pub mod keyed;
mod reducer;
mod store;
pub use keyed::{Keyed, KeyedState};
