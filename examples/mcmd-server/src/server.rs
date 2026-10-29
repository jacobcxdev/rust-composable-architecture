//! Coordinates backend, polling and proxy lifetimes within one reduction transaction.
//!
//! Construct state with matching, stable child identities and inject all clients.
//! Backend requests and proxy Start/Stop are external commands. Polling messages and backend
//! results/delegates are internal; arbitrary state restoration is not validated.
//! Cancellation drops local futures, not remote side effects.

use rca_core::{ActionPath, Effect, ReduceContext, Reducer, StatePath, scope};

use crate::{backend, polling, proxy};

/// Holds the three server features without task handles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct State {
    /// Owns external backend lifecycle policy.
    pub backend: backend::State,
    /// Retains query observations and polling lifecycle state.
    pub polling: polling::State,
    /// Owns local proxy acquisition and event delivery.
    pub proxy: proxy::State,
}

impl State {
    /// Creates a stopped server with matching child identities.
    #[must_use]
    pub fn new(config: proxy::Config) -> Self {
        let server_id = config.server_id.clone();
        Self {
            backend: backend::State {
                server_id: server_id.clone(),
                target: backend::Target::Stopped,
                phase: backend::Phase::Stopped,
                last_error: None,
            },
            polling: polling::State {
                server_id,
                active: false,
                querying: false,
                player_count: 0,
                failures: 0,
                last_error: None,
            },
            proxy: proxy::State {
                config,
                phase: proxy::Phase::Stopped,
                last_error: None,
            },
        }
    }
}

/// Routes backend requests and internal child messages.
#[derive(Debug, Eq, PartialEq)]
pub enum Action {
    /// Carries backend requests, results and delegates.
    Backend(backend::Action),
    /// Carries internal polling messages, not public lifecycle commands.
    Polling(polling::Action),
    /// Carries proxy lifecycle commands and internal messages.
    Proxy(proxy::Action),
}

/// Coordinates three persistent scopes without storing runtime task handles.
struct Server<B, P, X> {
    /// Applies backend requests and completed operation results.
    backend: B,
    /// Applies synchronous polling controls and internal query messages.
    polling: P,
    /// Applies local proxy commands before deciding whether to wake the backend.
    proxy: X,
}

impl<B, P, X> Reducer for Server<B, P, X>
where
    B: Reducer<State = State, Action = Action>,
    P: Reducer<State = State, Action = Action>,
    X: Reducer<State = State, Action = Action>,
{
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if matches!(action, Action::Polling(_)) {
            return self.polling.reduce(state, action, context);
        }

        let wake = Action::Backend(backend::Action::RequestStart);
        let (proxy, action, transfer) = if let Action::Proxy(_) = action {
            let effect = self.proxy.reduce(state, action, context);
            if matches!(
                action,
                Action::Proxy(proxy::Action::Event(proxy::ProxyEvent::JoinObserved))
            ) && state.proxy.phase == proxy::Phase::Listening
                && state.backend.phase == backend::Phase::Stopped
            {
                (effect, &wake, true)
            } else {
                return effect;
            }
        } else {
            (Effect::none(), action, false)
        };

        let before = state.backend.phase;
        let backend = self.backend.reduce(state, action, context);
        let backend = if transfer {
            backend.without_inherited_cancellation()
        } else {
            backend
        };

        let control = if state.backend.phase == backend::Phase::Running
            && state.backend.target == backend::Target::Running
        {
            polling::Action::Start
        } else {
            polling::Action::Stop
        };
        let polling = self
            .polling
            .reduce(state, &Action::Polling(control), context);

        // Preserve observations until the backend confirms it has stopped.
        if before == backend::Phase::Stopping && state.backend.phase == backend::Phase::Stopped {
            state.polling.player_count = 0;
            state.polling.failures = 0;
            state.polling.last_error = None;
        }

        // Compile cancellation before launching newly returned backend work.
        Effect::merge([proxy, polling, backend])
    }
}

/// Creates persistent child scopes and their synchronous coordinator.
#[must_use]
pub fn reducer() -> impl Reducer<State = State, Action = Action> {
    Server {
        backend: scope(
            StatePath::new(|s: &State| &s.backend, |s: &mut State| &mut s.backend),
            ActionPath::new(
                |a: &Action| match a {
                    Action::Backend(a) => Some(a),
                    _ => None,
                },
                Action::Backend,
            ),
            backend::Backend,
        ),
        polling: scope(
            StatePath::new(|s: &State| &s.polling, |s: &mut State| &mut s.polling),
            ActionPath::new(
                |a: &Action| match a {
                    Action::Polling(a) => Some(a),
                    _ => None,
                },
                Action::Polling,
            ),
            polling::Polling,
        ),
        proxy: scope(
            StatePath::new(|s: &State| &s.proxy, |s: &mut State| &mut s.proxy),
            ActionPath::new(
                |a: &Action| match a {
                    Action::Proxy(a) => Some(a),
                    _ => None,
                },
                Action::Proxy,
            ),
            proxy::Proxy,
        ),
    }
}
