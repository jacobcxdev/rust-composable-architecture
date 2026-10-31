//! Qualifies optional capabilities without treating absence as lookup failure.

use std::sync::Arc;

use rca_core::{DependencyKey, Effect, ReduceContext, Reducer, with_dependency};
use rca_test::TestStore;

/// Deliberately lacks `Clone`. Effects retain its shared handle.
struct Reporter {
    endpoint: &'static str,
}

/// Treats `None` as a valid default in both live and test environments.
struct ReporterKey;

impl DependencyKey for ReporterKey {
    type Value = Option<Arc<Reporter>>;

    fn live_value() -> Self::Value {
        None
    }

    fn test_value() -> Self::Value {
        None
    }
}

/// Creates a client whose endpoint identifies the selected dependency layer.
fn reporter(endpoint: &'static str) -> Arc<Reporter> {
    Arc::new(Reporter { endpoint })
}

/// Separates synchronous selections from reports delivered by effects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct State {
    selected: Vec<Option<&'static str>>,
    reported: Vec<&'static str>,
}

/// Selects an optional client or delivers its report.
enum Action {
    Read,
    Disable,
    Select,
    Reported(&'static str),
}

/// Starts report work only when the selected optional client is present.
struct Probe;

impl Reducer for Probe {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Disable => context
                .with_dependency_override::<ReporterKey, _>(None, |context| {
                    self.reduce(state, &Action::Read, context)
                }),
            Action::Select => context
                .with_dependency_override::<ReporterKey, _>(Some(reporter("action")), |context| {
                    self.reduce(state, &Action::Read, context)
                }),
            Action::Read => {
                let selected = context.dependency::<ReporterKey>();
                state
                    .selected
                    .push(selected.as_ref().map(|client| client.endpoint));
                match selected {
                    None => Effect::none(),
                    Some(client) => {
                        let client = Arc::clone(client);
                        Effect::run(move |sender| async move {
                            let _ = sender.send(Action::Reported(client.endpoint)).await;
                        })
                    }
                }
            }
            Action::Reported(endpoint) => {
                state.reported.push(endpoint);
                Effect::none()
            }
        }
    }
}

/// Checks a selection and consumes exactly one report for a present client.
///
/// For an absent client, the caller's exhaustive finish rejects unexpected work.
async fn read(
    store: &mut TestStore<State, Action>,
    action: Action,
    expected: Option<&'static str>,
) {
    let _ = store
        .send_with(action, move |state| state.selected.push(expected))
        .await
        .expect("selection commit");
    if let Some(endpoint) = expected {
        let _ = store
            .receive_with(
                move |action| matches!(action, Action::Reported(value) if *value == endpoint),
                move |state| state.reported.push(endpoint),
            )
            .await
            .expect("captured report commit");
    }
}

/// Checks that absent defaults and explicit test `None` values disable reports.
#[test]
fn absent_default_and_explicit_test_none_disable_optional_work() {
    futures_executor::block_on(async {
        let mut absent = TestStore::new(State::default(), Probe);
        read(&mut absent, Action::Read, None).await;
        absent.finish().await.expect("no absent work");

        let mut enabled = TestStore::builder(State::default(), Probe)
            .with_store_dependency::<ReporterKey>(Some(reporter("store")))
            .build();
        read(&mut enabled, Action::Read, Some("store")).await;
        enabled.finish().await.expect("store work complete");

        let mut disabled = TestStore::builder(
            State::default(),
            with_dependency::<ReporterKey, _>(Some(reporter("feature")), Probe),
        )
        .with_store_dependency::<ReporterKey>(Some(reporter("store")))
        .with_dependency::<ReporterKey>(None)
        .build();
        read(&mut disabled, Action::Read, None).await;
        read(&mut disabled, Action::Select, None).await;
        disabled
            .finish()
            .await
            .expect("test None masked every layer");
    });
}

/// Checks temporary selection, restored absence and restored presence after overrides.
#[test]
fn optional_overlays_mask_restore_and_capture_selected_handles() {
    futures_executor::block_on(async {
        let mut feature_disabled = TestStore::builder(
            State::default(),
            with_dependency::<ReporterKey, _>(None, Probe),
        )
        .with_store_dependency::<ReporterKey>(Some(reporter("store")))
        .build();
        read(&mut feature_disabled, Action::Read, None).await;
        read(&mut feature_disabled, Action::Select, Some("action")).await;
        read(&mut feature_disabled, Action::Read, None).await;
        feature_disabled
            .finish()
            .await
            .expect("feature None restored");

        let mut feature_enabled = TestStore::builder(
            State::default(),
            with_dependency::<ReporterKey, _>(Some(reporter("feature")), Probe),
        )
        .with_store_dependency::<ReporterKey>(Some(reporter("store")))
        .build();
        read(&mut feature_enabled, Action::Read, Some("feature")).await;
        read(&mut feature_enabled, Action::Disable, None).await;
        read(&mut feature_enabled, Action::Read, Some("feature")).await;
        feature_enabled
            .finish()
            .await
            .expect("feature Some restored");
    });
}
