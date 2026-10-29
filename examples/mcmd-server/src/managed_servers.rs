//! Composes keyed servers with independently driven asynchronous proxy cleanup.
//!
//! The configured roster is fixed. Removal requires confirmed backend and proxy
//! shutdown; restoration reuses the same external owner. Applications must drive
//! and join every returned owner and retain failed cleanup recovery. State holds
//! no owners. Child identities and internal completion actions are trusted, as
//! in the separate RAII server example; arbitrary state restoration is not validated.

use std::{collections::BTreeMap, fmt, sync::Arc};

use rca_core::{
    ActionPath, DependencyKey, Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
    ReduceContext, Reducer, StatePath, for_each, scope,
};

use crate::{backend, managed_proxy, polling, proxy};

/// Holds independently identified server lifetimes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct State {
    /// Contains the currently present configured servers.
    pub servers: KeyedState<String, ServerState>,
}

/// Holds backend, polling and managed proxy state without external owners.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerState {
    /// Tracks backend lifecycle acknowledgements.
    pub backend: backend::State,
    /// Tracks polling lifecycle and observations.
    pub polling: polling::State,
    /// Tracks asynchronous proxy cleanup acknowledgements.
    pub proxy: managed_proxy::State,
}

impl ServerState {
    /// Creates matching child identities without starting external work.
    fn stopped(id: String) -> Self {
        Self {
            backend: backend::State {
                server_id: id.clone(),
                target: backend::Target::Stopped,
                phase: backend::Phase::Stopped,
                last_error: None,
            },
            polling: polling::State {
                server_id: id,
                active: false,
                querying: false,
                player_count: 0,
                failures: 0,
                last_error: None,
            },
            proxy: managed_proxy::State::default(),
        }
    }

    /// Requires confirmed shutdown and no desired backend or polling work.
    fn removable(&self) -> bool {
        self.backend.phase == backend::Phase::Stopped
            && self.backend.target == backend::Target::Stopped
            && !self.polling.active
            && !self.polling.querying
            && self.proxy.phase == managed_proxy::Phase::Stopped
    }
}

/// Routes lifecycle commands and trusted internal child messages.
#[derive(Debug, Eq, PartialEq)]
pub enum ServerAction {
    /// Carries backend requests, results and delegates.
    Backend(backend::Action),
    /// Carries internal polling messages.
    Polling(polling::Action),
    /// Carries proxy commands and internal messages.
    Proxy(managed_proxy::Action),
}

/// Changes safe local membership or routes a present server's action.
#[derive(Debug, Eq, PartialEq)]
pub enum Action {
    /// Removes only a fully stopped server; otherwise does nothing.
    Remove(String),
    /// Restores an absent configured server using its existing external owner.
    Restore(String),
    /// Routes an action to an existing server.
    Server(KeyedAction<String, ServerAction>),
}

/// Identifies a duplicate configured server identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateId(
    /// Contains the repeated identity, before any external owners are constructed.
    pub String,
);

impl fmt::Display for DuplicateId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "duplicate server identity: {}", self.0)
    }
}

impl std::error::Error for DuplicateId {}

/// Restricts local membership to the fixed, independently owned server roster.
struct Membership {
    /// Shares the configured identities with the server coordinator.
    controls: Arc<BTreeMap<String, managed_proxy::Control>>,
}

impl Reducer for Membership {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Remove(id) if state.servers.get(id).is_some_and(ServerState::removable) => {
                state.servers.remove(id);
            }
            Action::Restore(id)
                if self.controls.contains_key(id) && !state.servers.contains_id(id) =>
            {
                state
                    .servers
                    .insert(id.clone(), ServerState::stopped(id.clone()));
            }
            _ => {}
        }
        Effect::none()
    }
}

/// Selects one server's control only for the current synchronous reduction.
struct SelectedControl;

impl DependencyKey for SelectedControl {
    type Value = managed_proxy::Control;

    fn live_value() -> Self::Value {
        panic!("managed server coordinator must select its proxy control")
    }
}

/// Uses the selected control without storing one mutable reducer per server.
struct Proxy;

impl Reducer for Proxy {
    type State = managed_proxy::State;
    type Action = managed_proxy::Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        managed_proxy::ManagedProxy::new(context.dependency::<SelectedControl>().clone())
            .reduce(state, action, context)
    }
}

/// Coordinates persistent child scopes while selecting the correct external owner.
struct Server<B, P, X> {
    /// Maps stable server identities to their independently driven owners.
    controls: Arc<BTreeMap<String, managed_proxy::Control>>,
    /// Applies backend lifecycle requests and results.
    backend: B,
    /// Applies polling controls in the same transaction as backend changes.
    polling: P,
    /// Applies proxy commands under the selected control override.
    proxy: X,
}

impl<B, P, X> Reducer for Server<B, P, X>
where
    B: Reducer<State = ServerState, Action = ServerAction>,
    P: Reducer<State = ServerState, Action = ServerAction>,
    X: Reducer<State = ServerState, Action = ServerAction>,
{
    type State = ServerState;
    type Action = ServerAction;

    fn reduce(
        &mut self,
        state: &mut ServerState,
        action: &ServerAction,
        context: &mut ReduceContext<'_>,
    ) -> Effect<ServerAction> {
        if matches!(action, ServerAction::Polling(_)) {
            return self.polling.reduce(state, action, context);
        }

        let wake = ServerAction::Backend(backend::Action::RequestStart);
        let (proxy, action, transfer) = if let ServerAction::Proxy(_) = action {
            // for_each shares this reducer; select a handle for this reduction,
            // rather than binding every keyed child to one fixed external owner.
            let control = self
                .controls
                .get(&state.backend.server_id)
                .expect("server identities must remain configured and stable")
                .clone();
            let effect = context
                .with_dependency_override::<SelectedControl, _>(control, |context| {
                    self.proxy.reduce(state, action, context)
                });
            if matches!(
                action,
                ServerAction::Proxy(managed_proxy::Action::Event(
                    proxy::ProxyEvent::JoinObserved
                ))
            ) && state.proxy.phase == managed_proxy::Phase::Listening
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
        // Backend work outlives the proxy event that requested it, but retains
        // the server's structural lifetime and ordinary Store ownership.
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
            .reduce(state, &ServerAction::Polling(control), context);

        if before == backend::Phase::Stopping && state.backend.phase == backend::Phase::Stopped {
            state.polling.player_count = 0;
            state.polling.failures = 0;
            state.polling.last_error = None;
        }

        Effect::merge([proxy, polling, backend])
    }
}

/// Creates keyed state, persistent child scopes and one external owner per configuration.
///
/// Owners are returned in input order without starting work. Drive them separately
/// from RCA, then join them and retain any pending recovery after store shutdown.
/// Direct actions for missing children retain RCA's programming-error contract.
///
/// # Errors
///
/// Returns [`DuplicateId`] before constructing any owners if identities repeat.
#[expect(
    clippy::needless_pass_by_value,
    reason = "constructor distributes shared ownership to independently driven owners"
)]
pub fn compose(
    configs: impl IntoIterator<Item = proxy::Config>,
    client: Arc<dyn managed_proxy::Client>,
) -> Result<
    (
        State,
        impl Reducer<State = State, Action = Action>,
        Vec<managed_proxy::Owner>,
    ),
    DuplicateId,
> {
    let configs: Vec<_> = configs.into_iter().collect();
    let mut ids = std::collections::BTreeSet::new();
    for config in &configs {
        if !ids.insert(&config.server_id) {
            return Err(DuplicateId(config.server_id.clone()));
        }
    }

    let mut state = State::default();
    let mut controls = BTreeMap::new();
    let mut owners = Vec::with_capacity(configs.len());
    for config in configs {
        let id = config.server_id.clone();
        let (control, owner) = managed_proxy::channel(config, Arc::clone(&client));
        state
            .servers
            .insert(id.clone(), ServerState::stopped(id.clone()));
        controls.insert(id, control);
        owners.push(owner);
    }

    let controls = Arc::new(controls);
    let server = Server {
        controls: Arc::clone(&controls),
        backend: scope(
            StatePath::new(
                |s: &ServerState| &s.backend,
                |s: &mut ServerState| &mut s.backend,
            ),
            ActionPath::new(
                |a: &ServerAction| match a {
                    ServerAction::Backend(a) => Some(a),
                    _ => None,
                },
                ServerAction::Backend,
            ),
            backend::Backend,
        ),
        polling: scope(
            StatePath::new(
                |s: &ServerState| &s.polling,
                |s: &mut ServerState| &mut s.polling,
            ),
            ActionPath::new(
                |a: &ServerAction| match a {
                    ServerAction::Polling(a) => Some(a),
                    _ => None,
                },
                ServerAction::Polling,
            ),
            polling::Polling,
        ),
        proxy: scope(
            StatePath::new(
                |s: &ServerState| &s.proxy,
                |s: &mut ServerState| &mut s.proxy,
            ),
            ActionPath::new(
                |a: &ServerAction| match a {
                    ServerAction::Proxy(a) => Some(a),
                    _ => None,
                },
                ServerAction::Proxy,
            ),
            Proxy,
        ),
    };

    let reducer = for_each(
        Membership { controls },
        KeyedStatePath::new(|s: &State| &s.servers, |s: &mut State| &mut s.servers),
        KeyedActionPath::new(
            |a: &Action| match a {
                Action::Server(a) => Some(a),
                _ => None,
            },
            Action::Server,
        ),
        server,
    );
    Ok((state, reducer, owners))
}
