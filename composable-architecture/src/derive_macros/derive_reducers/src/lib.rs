#![forbid(unsafe_code)]

//! Implementation crate for `#[derive(composable::RecursiveReducer)]`.
//!
//! The public-facing documentation lives in `composable::derive_macros`, but this crate is where
//! the proc-macro expansion is produced.
//!
//! ## What gets generated
//!
//! The derive generates an `impl composable::Reducer for <Type>` whose `reduce` does:
//!
//! 1. Call `<Self as composable::RecursiveReducer>::reduce(self, action.clone(), send.clone())`
//! 2. Attempt to route the action into each child reducer field/variant by using `TryInto`.
//! 3. When routing succeeds, it forwards effects using either:
//!    - `send.scope()` for standard children, or
//!    - `send.scope_keyed(key)` for keyed children (see below).
//!
//! Ordering matters: the parent’s `RecursiveReducer::reduce` runs *before* any derived child routing.
//!
//! ## Keyed children
//!
//! A “keyed child” is a dynamic collection of child states keyed by an identifier (tabs, rows, etc).
//! The derive recognises keyed fields/variants (see `util::is_keyed_state`) and routes actions of
//! the form `composable::Keyed<K, ChildAction>` to exactly one child state selected by the key.
//!
//! The parent `Action` must have an unambiguous conversion route from `Keyed<K, ChildAction>`
//! (typically a dedicated enum variant).

use proc_macro::TokenStream;
use syn::{parse_macro_input, Data, DeriveInput};

mod enums;
mod structs;
mod util;

/// ## Compiler Errors
///
/// The are a few common mistakes that will produce well-known compiler errors
///
///
///
/// ### the trait bound `xxxx::State: composable::RecursiveReducer` is not satisfied
///
/// ```sh
/// | #[derive(RecursiveReducer)]
/// |          ^^^^^^^^^^^^^^^^ the trait `composable::RecursiveReducer` is not implemented for `State`
/// |
/// = note: this error originates in the derive macro `RecursiveReducer`
/// ```
///
/// **Cause**: You haven't yet written an `impl RecursiveReducer` for the type you added `#[derive(RecursiveReducer)]` to.
///
/// <br />
///
///
/// ### conflicting implementation for `State`
///
/// ```sh
/// | #[derive(RecursiveReducer)]
/// |          ^^^^^^^^^^^^^^^^ conflicting implementation for `State`
/// ...
/// | impl Reducer for State {
/// | ---------------------- first implementation here
/// |
/// = note: this error originates in the derive macro `RecursiveReducer`
/// ```
///
/// **Cause**: You declared an `impl Reducer`, perhaps out of habit, rather than an `impl RecursiveReducer`.
///
/// <br />
///
/// ### the trait bound `…: composable::Reducer` is not satisfied
///
/// ```sh
/// | #[derive(RecursiveReducer)]
/// |          ^^^^^^^^^^^^^^^^ the trait `composable::Reducer` is not implemented for `…`
/// |
/// = help: the following other types implement trait `composable::Reducer`:
///           ⋮
/// = note: this error originates in the derive macro `RecursiveReducer`
/// ```
///
/// where `…`  is replaced with the type of one of the struct's fields in the error message.
///
/// **Cause**: A `#[reducer(skip)]` attribute is missing.
///
/// <br />
///
/// ### type mismatch resolving `<impl Effects<Action = Action> as Effects>::Action == Action`
///
/// ```sh
/// | #[derive(RecursiveReducer)]
/// |          ^^^^^^^^^^^^^^^^ expected `child::Action`, found `parent::Action`
/// |
/// = note: `parent::Action` and `child::Action` have similar names, but are actually distinct types
/// ```
///
/// **Cause**: The parent `Action` cannot be converted *from* the child action type.
///
/// **Fix**:
/// - Ensure the parent action enum has a variant holding the child action type.
/// - Derive (or implement) `From<ChildAction> for ParentAction`.
///
/// <br />
///
/// ### the trait bound `parent::Action: TryInto<child::Action>` is not satisfied
///
/// ```sh
/// | #[derive(RecursiveReducer)]
/// |          ^^^^^^^^^^^^^^^^ the trait `composable::From<parent::Action>` is not implemented for `child::Action`
/// |
/// ```
///
/// **Cause**: The derived reducer attempts `action.clone().try_into()` when routing into children.
/// If there is no conversion route from the parent action to that child action, routing cannot occur.
///
/// **Fix**:
/// - Ensure the parent action enum has a variant holding the child action type.
/// - Derive (or implement) `TryInto<ChildAction> for ParentAction` (the crate re-exports
///   `derive_more::TryInto` to generate these conversions).
///
/// <br />
///
/// ### conflicting implementations of `From<ChildAction> for ParentAction`
///
/// ```sh
/// error[E0119]: conflicting implementations of trait `From<child::Action>` for type `parent::Action`
/// ```
///
/// **Cause**: The parent `Action` enum has *multiple* variants with the *same* payload type, and
/// `derive_more::From` would need to generate the same `impl From<T> for ParentAction` more than once.
///
/// **Fix**:
/// - Prefer distinct action wrapper types (or distinct child action types) per variant, or
/// - Implement `From`/`TryInto` manually for that action type instead of deriving.
///
#[proc_macro_derive(RecursiveReducer, attributes(reducer))]
pub fn derive_recursive_reducers(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    match input.data {
        Data::Struct(data) => structs::derive_macro(input.ident, data),
        Data::Enum(data) => enums::derive_macro(input.ident, data),
        _ => panic!("untagged unions are not supported"),
    }
}
