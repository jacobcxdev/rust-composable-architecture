//! Composes identified server lifetimes without claiming remote resource teardown.
//!
//! `Forget` drops local state and cancels descendant RCA work. It does not stop
//! a remote backend or undo a pending external operation. Production owners must
//! coordinate external shutdown first or retain separate reconciliation authority.
//! Add establishes matching child identities; direct state edits are not validated.

use rca_core::{
    Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, ReduceContext, Reducer,
    for_each,
};

use crate::{proxy, server};

/// Holds ordered, independently identified server state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct State {
    /// Owns one local feature lifetime per server identity.
    pub servers: KeyedState<String, server::State>,
}

/// Changes local membership or routes an existing server's action.
#[derive(Debug, Eq, PartialEq)]
pub enum Action {
    /// Adds a stopped server, preserving an existing equal-key lifetime.
    Add(proxy::Config),
    /// Forgets local state and cancels RCA work, without stopping remote resources.
    Forget(String),
    /// Routes an action to an existing server.
    Server(KeyedAction<String, server::Action>),
}

/// Changes local membership without claiming external backend cleanup.
struct Membership;

impl Reducer for Membership {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Add(config) if !state.servers.contains_id(&config.server_id) => {
                state
                    .servers
                    .insert(config.server_id.clone(), server::State::new(config.clone()));
            }
            Action::Forget(id) => {
                state.servers.remove(id);
            }
            _ => {}
        }
        Effect::none()
    }
}

/// Creates one child reducer shared sequentially across keyed server states.
///
/// Direct actions targeting missing children retain RCA's programming-error
/// contract. Obsolete effect outputs instead carry invalidated provenance and
/// are rejected before entering the reducer.
#[must_use]
pub fn reducer() -> impl Reducer<State = State, Action = Action> {
    for_each(
        Membership,
        KeyedStatePath::new(|s: &State| &s.servers, |s: &mut State| &mut s.servers),
        KeyedActionPath::new(
            |a: &Action| match a {
                Action::Server(a) => Some(a),
                _ => None,
            },
            Action::Server,
        ),
        server::reducer(),
    )
}
