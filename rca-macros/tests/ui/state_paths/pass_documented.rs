//! Checks that public path derives provide documentation for generated items.
#![deny(missing_docs)]

use rca::{KeyedAction, KeyedState};
use rca_macros::{ActionPaths, StatePaths};

/// Supplies each named-field projection.
#[derive(StatePaths)]
pub struct Fields {
    /// Stores a required value.
    pub value: u8,
    /// Stores an optional child.
    #[rca(optional)]
    pub child: Option<u8>,
    /// Stores children by identity.
    #[rca(keyed)]
    pub rows: KeyedState<u8, u16>,
}

/// Supplies single-field and aggregate state cases.
#[derive(StatePaths)]
pub enum State {
    /// Stores one unnamed value.
    Single(u8),
    /// Stores one named value.
    Named {
        /// Stores the payload.
        payload: u16,
    },
    /// Stores two values in declaration order.
    Pair(u8, u16),
    /// Stores no payload.
    Idle,
}

/// Supplies ordinary, keyed and aggregate action cases.
#[derive(ActionPaths)]
pub enum Action {
    /// Carries one value.
    Single(u8),
    /// Carries an identified child action.
    #[rca(keyed)]
    Row(KeyedAction<u8, u16>),
    /// Carries two values in declaration order.
    Pair(u8, u16),
    /// Carries no payload.
    Idle,
}

/// Exercises a generic state namespace alias.
#[derive(StatePaths)]
pub struct GenericState<T> {
    /// Stores the generic value.
    pub value: T,
}

/// Exercises a generic action namespace alias.
#[derive(ActionPaths)]
pub enum GenericAction<T> {
    /// Carries the generic value.
    Value(T),
}

/// Exercises a state namespace with lifetimes and constants only.
#[derive(StatePaths)]
pub struct BorrowedState<'a, const N: usize> {
    /// Borrows the array without storing its elements in the namespace.
    pub bytes: &'a [u8; N],
}

/// Exercises an action namespace with lifetimes and constants only.
#[derive(ActionPaths)]
pub enum BorrowedAction<'a, const N: usize> {
    /// Carries a borrowed array.
    Bytes(&'a [u8; N]),
}

fn main() {
    let _ = FieldsPaths::value();
    let _ = FieldsPaths::child();
    let _ = FieldsPaths::rows();
    let _ = StatePaths::single();
    let _ = StatePaths::payload();
    let _ = StatePaths::pair();
    let _ = StatePaths::idle();
    let _ = ActionPaths::single();
    let _ = ActionPaths::row();
    let _ = ActionPaths::pair();
    let _ = ActionPaths::idle();
    let _ = GenericStatePaths::<String>::value();
    let _ = GenericActionPaths::<String>::value();
    let _ = BorrowedStatePaths::<2>::bytes();
    let _ = BorrowedActionPaths::<2>::bytes();
}
