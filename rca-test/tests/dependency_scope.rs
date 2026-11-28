//! Checks dependency precedence, isolated test defaults and owned effect captures.

use std::collections::HashMap;
use std::sync::Arc;

use rca_core::{DependencyKey, Effect, ReduceContext, Reducer, combine, with_dependency};
use rca_test::TestStore;

mod generated_async_client {
    //! Exercises generated defaults through a client that returns an owned future.

    use std::{pin::Pin, sync::Arc};

    use rca_core::{DependencyKey as _, Effect, ReduceContext, Reducer};
    use rca_macros::{DependencyClient, DependencyKey};
    use rca_test::TestStore;

    /// Owns a response future after the dependency lookup ends.
    type Response = Pin<Box<dyn Future<Output = String> + Send>>;

    /// Combines an explicitly supplied endpoint with a generated test default.
    #[derive(Clone, DependencyClient)]
    struct Client {
        load: Arc<dyn Fn(String) -> Response + Send + Sync>,
        #[dependency(test = Arc::new(|| 5))]
        count: Arc<dyn Fn() -> usize + Send + Sync>,
    }

    /// Rejects any accidental selection of the live client.
    #[derive(DependencyKey)]
    #[dependency(value = Client, live = panic!("live client must not be used"), test = Client::test_value())]
    struct ClientKey;

    /// Records the immediate count and the later response separately.
    #[derive(Clone, Debug, Default, PartialEq)]
    struct State {
        result: Option<String>,
        count: usize,
    }

    /// Carries owned input and output across the effect boundary.
    enum Action {
        Load(String),
        Loaded(String),
    }

    /// Captures the selected client before the response future runs.
    struct Feature;

    impl Reducer for Feature {
        type State = State;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut State,
            action: &Action,
            context: &mut ReduceContext<'_>,
        ) -> Effect<Action> {
            match action {
                Action::Load(name) => {
                    let client = context.dependency::<ClientKey>().clone();
                    let name = name.clone();
                    state.count = (client.count)();
                    Effect::future(async move { Some(Action::Loaded((client.load)(name).await)) })
                }
                Action::Loaded(result) => {
                    state.result = Some(result.clone());
                    Effect::none()
                }
            }
        }
    }

    /// Checks generated defaults and an owned asynchronous response together.
    #[test]
    fn generated_owned_async_client_runs_through_exhaustive_test_store() {
        futures_executor::block_on(async {
            // Removing the explicit test expression or selecting live defaults hits the sentinel.
            assert_eq!((ClientKey::test_value().count)(), 5);
            let prefix = Arc::new(String::from("loaded:"));
            let client = Client {
                load: Arc::new(move |name| {
                    let prefix = Arc::clone(&prefix);
                    Box::pin(async move { format!("{prefix}{name}") })
                }),
                ..Client::test_value()
            };
            let mut store = TestStore::builder(State::default(), Feature)
                .with_dependency::<ClientKey>(client)
                .build();
            let _task = store
                .send_with(Action::Load(String::from("item")), |state| state.count = 5)
                .await
                .unwrap();
            let _task = store
                .receive_with(
                    |action| matches!(action, Action::Loaded(result) if result == "loaded:item"),
                    |state| state.result = Some(String::from("loaded:item")),
                )
                .await
                .unwrap();
            store.finish().await.unwrap();
        });
    }
}

/// Has no test default and panics if the live fallback runs.
struct EndpointKey;

/// Checks that a missing test default fails before the live fallback runs.
#[test]
fn missing_test_default_never_uses_live_value() {
    let panic = std::panic::catch_unwind(|| {
        futures_executor::block_on(async {
            let mut store = TestStore::new(State::default(), Probe("read"));
            let _ = store
                .send_with(Action::Read, |state| {
                    state.values.insert("read", "default");
                })
                .await
                .unwrap();
            store.finish().await.unwrap();
        });
    })
    .expect_err("missing test dependency must fail before using live value");
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(message.contains("missing test dependency"), "{message}");
    assert!(message.contains("EndpointKey"), "{message}");
}

impl DependencyKey for EndpointKey {
    type Value = &'static str;

    fn live_value() -> Self::Value {
        panic!("live factory executed")
    }
}

/// Records each reducer's selected endpoint under a separate label.
#[derive(Clone, Debug, Default, PartialEq)]
struct State {
    values: HashMap<&'static str, &'static str>,
}

/// Requests one dependency lookup from every composed reducer.
enum Action {
    Read,
}

/// Writes the selected endpoint under its own label.
struct Probe(&'static str);

impl Reducer for Probe {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state
            .values
            .insert(self.0, *context.dependency::<EndpointKey>());
        Effect::none()
    }
}

/// Reads inside an action-level override, below any test override.
struct ExplicitProbe;

impl Reducer for ExplicitProbe {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let selected = context.with_dependency_override::<EndpointKey, _>("explicit", |context| {
            *context.dependency::<EndpointKey>()
        });
        state.values.insert("explicit", selected);
        Effect::none()
    }
}

/// Checks test precedence over store, feature and action-level values.
#[test]
fn test_dependency_wins_over_every_production_layer() {
    futures_executor::block_on(async {
        let reducer = combine((
            Probe("sibling"),
            with_dependency::<EndpointKey, _>("feature", Probe("feature")),
            ExplicitProbe,
        ));
        let mut store = TestStore::builder(State::default(), reducer)
            .with_store_dependency::<EndpointKey>("store")
            .with_dependency::<EndpointKey>("test")
            .build();

        let _task = store
            .send_with(Action::Read, |state| {
                state.values = [
                    ("sibling", "test"),
                    ("feature", "test"),
                    ("explicit", "test"),
                ]
                .into_iter()
                .collect();
            })
            .await
            .expect("read commit");
        store
            .assert_state(|state| {
                assert_eq!(state.values["sibling"], "test");
                assert_eq!(state.values["feature"], "test");
                assert_eq!(state.values["explicit"], "test");
            })
            .await
            .expect("state assertion");
        store.finish().await.expect("clean finish");
    });
}

/// Gives each constructed client an allocation identity independent of its label.
struct Client {
    label: &'static str,
    identity: Arc<()>,
}

impl Client {
    /// Creates a client with a fresh identity.
    fn new(label: &'static str) -> Self {
        Self {
            label,
            identity: Arc::new(()),
        }
    }
}

/// Supplies distinct live and test clients without a global cache.
struct ClientKey;

impl DependencyKey for ClientKey {
    type Value = Client;

    fn live_value() -> Client {
        Client::new("live")
    }

    fn test_value() -> Client {
        Client::new("test")
    }
}

/// Records response labels and retains the latest client identity.
///
/// Value equality ignores allocation identity. The cache tests use `Arc::ptr_eq`
/// to check identity separately.
#[derive(Clone, Debug, Default, PartialEq)]
struct ClientState {
    labels: Vec<&'static str>,
    identity: Option<Arc<()>>,
}

/// Selects a dependency layer or delivers its captured identity.
enum ClientAction {
    Read,
    Explicit,
    Feature,
    Returned(&'static str, Arc<()>),
}

/// Moves the selected client's identity into an effect response.
struct ClientProbe;

impl Reducer for ClientProbe {
    type State = ClientState;
    type Action = ClientAction;

    fn reduce(
        &mut self,
        state: &mut ClientState,
        action: &ClientAction,
        context: &mut ReduceContext<'_>,
    ) -> Effect<ClientAction> {
        match action {
            ClientAction::Returned(label, identity) => {
                state.labels.push(label);
                state.identity = Some(Arc::clone(identity));
                Effect::none()
            }
            ClientAction::Explicit => context
                .with_dependency_override::<ClientKey, _>(Client::new("explicit"), |context| {
                    self.reduce(state, &ClientAction::Read, context)
                }),
            ClientAction::Feature => with_dependency::<ClientKey, _>(
                Client::new("feature"),
                ClientProbe,
            )
            .reduce(state, &ClientAction::Read, context),
            ClientAction::Read => {
                let client = context.dependency::<ClientKey>();
                let label = client.label;
                let identity = Arc::clone(&client.identity);
                Effect::run(move |sender| async move {
                    let _ = sender.send(ClientAction::Returned(label, identity)).await;
                })
            }
        }
    }
}

/// Checks the response label without prescribing the client's allocation identity.
async fn read_client(
    store: &mut TestStore<ClientState, ClientAction>,
    action: ClientAction,
    label: &'static str,
) {
    let _ = store.send(action).await.unwrap();
    let _ = store
        .receive_with(
            move |action| matches!(action, ClientAction::Returned(actual, _) if *actual == label),
            move |state| {
                state.labels.push(label);
                state.identity = Some(Arc::new(()));
            },
        )
        .await
        .unwrap();
}

/// Checks cache reuse, override restoration and isolation between stores.
#[test]
fn test_defaults_are_cached_per_store_and_captured_by_effects() {
    futures_executor::block_on(async {
        let mut store = TestStore::new(ClientState::default(), ClientProbe);
        read_client(&mut store, ClientAction::Read, "test").await;

        // The assertion callback owns its capture, so return the identity through a channel.
        let (send, recv) = async_channel::bounded(1);
        store
            .assert_state(move |state| {
                send.try_send(Arc::clone(state.identity.as_ref().unwrap()))
                    .unwrap();
            })
            .await
            .unwrap();
        let first = recv.try_recv().unwrap();

        read_client(&mut store, ClientAction::Read, "test").await;
        let expected = Arc::clone(&first);
        store
            .assert_state(move |state| {
                assert!(Arc::ptr_eq(state.identity.as_ref().unwrap(), &expected));
            })
            .await
            .unwrap();

        read_client(&mut store, ClientAction::Explicit, "explicit").await;
        read_client(&mut store, ClientAction::Feature, "feature").await;
        read_client(&mut store, ClientAction::Read, "test").await;
        let expected = Arc::clone(&first);
        store
            .assert_state(move |state| {
                assert!(Arc::ptr_eq(state.identity.as_ref().unwrap(), &expected));
            })
            .await
            .unwrap();
        store.finish().await.unwrap();

        let mut other = TestStore::new(ClientState::default(), ClientProbe);
        read_client(&mut other, ClientAction::Read, "test").await;
        other
            .assert_state(move |state| {
                assert!(!Arc::ptr_eq(state.identity.as_ref().unwrap(), &first));
            })
            .await
            .unwrap();
        other.finish().await.unwrap();

        let host = rca_test::DeterministicRuntime::new();
        let live = rca_core::Store::builder(ClientState::default(), ClientProbe, host.runtime())
            .build()
            .unwrap();
        let root = host
            .run(live.send(ClientAction::Read))
            .await
            .unwrap()
            .unwrap();
        host.run(root.finish()).await.unwrap();
        host.run(live.read(|state| assert_eq!(state.labels, ["live"])))
            .await
            .unwrap()
            .unwrap();
        host.run(live.shutdown()).await.unwrap().unwrap();

        let mut values = rca_core::DependencyValues::new();
        assert_eq!(values.get_or_insert::<ClientKey>().label, "live");
    });
}

/// Checks that explicit production layers still override a cached test default.
#[test]
fn explicit_layers_still_override_test_defaults() {
    futures_executor::block_on(async {
        let mut root = TestStore::builder(ClientState::default(), ClientProbe)
            .with_store_dependency::<ClientKey>(Client::new("store"))
            .build();
        read_client(&mut root, ClientAction::Read, "store").await;
        read_client(&mut root, ClientAction::Explicit, "explicit").await;
        root.finish().await.unwrap();

        let mut feature = TestStore::builder(
            ClientState::default(),
            with_dependency::<ClientKey, _>(Client::new("feature"), ClientProbe),
        )
        .with_store_dependency::<ClientKey>(Client::new("store"))
        .build();
        read_client(&mut feature, ClientAction::Read, "feature").await;
        read_client(&mut feature, ClientAction::Explicit, "explicit").await;
        feature.finish().await.unwrap();
    });
}

/// Checks missing-default failures in projected and non-exhaustive stores.
#[test]
fn alternate_test_constructors_reject_missing_defaults() {
    for projected in [false, true] {
        let panic = std::panic::catch_unwind(|| {
            futures_executor::block_on(async {
                let mut store = if projected {
                    TestStore::projected(State::default(), Probe("read"), State::clone)
                } else {
                    TestStore::with_exhaustivity(
                        State::default(),
                        Probe("read"),
                        rca_test::Exhaustivity::NonExhaustive,
                    )
                };
                let _ = store
                    .send_with(Action::Read, |state| {
                        state.values.insert("read", "default");
                    })
                    .await
                    .unwrap();
                store.finish().await.unwrap();
            });
        })
        .expect_err("test constructor must reject missing defaults");
        let message = panic.downcast_ref::<String>().unwrap();
        assert!(message.contains("missing test dependency"), "{message}");
        assert!(message.contains("EndpointKey"), "{message}");
    }
}
