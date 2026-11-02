//! Checks dependency precedence, restoration and ownership across reducer scopes.

mod support;

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use rca_core::{DependencyKey, Effect, ReduceContext, Reducer, Store, combine, with_dependency};

use support::ManualRuntime;

/// Provides distinguishable values for each dependency layer.
struct EndpointKey;

impl DependencyKey for EndpointKey {
    type Value = &'static str;

    fn live_value() -> Self::Value {
        "default"
    }
}

/// Records which dependency each reducer observed.
#[derive(Default)]
struct State {
    /// Associates the probe name with its selected endpoint.
    values: HashMap<&'static str, &'static str>,
}

/// Triggers all reducers in the composed feature.
enum Action {
    /// Records the dependency selected during this reduction.
    Read,
}

/// Labels one observation within a composed reducer.
struct Probe {
    /// Identifies the layer being checked.
    name: &'static str,
}

/// Reads an operation-local override inside a feature dependency layer.
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
        let explicit = context.with_dependency_override::<EndpointKey, _>("explicit", |context| {
            *context.dependency::<EndpointKey>()
        });
        state.values.insert("explicit", explicit);
        Effect::none()
    }
}

/// Checks that unwinding restores the feature's dependency layer.
struct UnwindProbe;

impl Reducer for UnwindProbe {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let result = catch_unwind(AssertUnwindSafe(|| {
            context.with_dependency_override::<EndpointKey, _>("temporary", |context| {
                assert_eq!(*context.dependency::<EndpointKey>(), "temporary");
                panic!("dependency override unwind probe");
            });
        }));
        let panic = result.expect_err("the deliberate override panic must escape");
        assert_eq!(
            panic.downcast_ref::<&str>().copied(),
            Some("dependency override unwind probe")
        );
        state
            .values
            .insert("after-unwind", *context.dependency::<EndpointKey>());
        Effect::none()
    }
}

impl Probe {
    /// Names a probe without changing its dependency lookup.
    const fn named(name: &'static str) -> Self {
        Self { name }
    }
}

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
            .insert(self.name, *context.dependency::<EndpointKey>());
        Effect::none()
    }
}

/// Checks operation-local precedence and restoration after the intended panic.
#[test]
fn explicit_override_wins_and_unwind_restores_feature_layer() {
    let runtime = ManualRuntime::new();
    let reducer =
        with_dependency::<EndpointKey, _>("feature", combine((ExplicitProbe, UnwindProbe)));
    let store = Store::builder(State::default(), reducer, runtime.runtime())
        .with_dependency::<EndpointKey>("store")
        .build()
        .expect("valid Store");
    runtime.start_all();

    futures_executor::block_on(async {
        store
            .send(Action::Read)
            .await
            .expect("read commit")
            .finish()
            .await;
        store
            .read(|state| {
                assert_eq!(state.values["explicit"], "explicit");
                assert_eq!(state.values["after-unwind"], "feature");
            })
            .await
            .expect("state read");
        store.shutdown().await.expect("Store shutdown");
    });
    for result in runtime.join_all() {
        result.expect("actor thread must finish without panic");
    }
}

/// Supplies an owned service handle so capture identity and release are observable.
struct ServiceKey;

impl DependencyKey for ServiceKey {
    type Value = Arc<str>;

    fn live_value() -> Self::Value {
        Arc::from("default")
    }
}

/// Keeps root and effect observations distinct.
#[derive(Default)]
struct CaptureState {
    /// Records the root dependency selected after the feature's effect was created.
    root: Option<Arc<str>>,
    /// Retains the exact service handle captured by the feature's effect.
    effect: Option<Arc<str>>,
}

/// Orders feature capture, root lookup and asynchronous output.
enum CaptureAction {
    /// Captures the feature dependency without running the effect yet.
    Start,
    /// Reads the root dependency before the queued effect starts.
    ReadRoot,
    /// Returns the handle selected by the feature.
    Output(Arc<str>),
}

/// Captures the feature dependency before later root activity can change context.
struct CaptureFeature {
    /// Confirms that the effect's output was committed.
    completed: mpsc::Sender<()>,
}

impl Reducer for CaptureFeature {
    type State = CaptureState;
    type Action = CaptureAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        if !matches!(action, CaptureAction::Start) {
            return Effect::none();
        }
        let selected = Arc::clone(context.dependency::<ServiceKey>());
        let completed = self.completed.clone();
        Effect::run(move |sender| async move {
            sender
                .send(CaptureAction::Output(selected))
                .await
                .expect("captured output commit");
            completed
                .send(())
                .expect("completion receiver remains live");
        })
    }
}

/// Records root values and effect outputs without applying a feature override.
struct CaptureParent;

impl Reducer for CaptureParent {
    type State = CaptureState;
    type Action = CaptureAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            CaptureAction::Start => {}
            CaptureAction::ReadRoot => {
                state.root = Some(Arc::clone(context.dependency::<ServiceKey>()));
            }
            CaptureAction::Output(value) => state.effect = Some(Arc::clone(value)),
        }
        Effect::none()
    }
}

/// Checks captured handle identity, private debug output and final ownership release.
#[test]
fn effect_retains_selected_feature_handle_after_root_activity() {
    let runtime = ManualRuntime::new();
    let (completed, completion) = mpsc::channel();
    let selected: Arc<str> = Arc::from("private-feature-endpoint");
    let released = Arc::downgrade(&selected);
    let feature =
        with_dependency::<ServiceKey, _>(Arc::clone(&selected), CaptureFeature { completed });
    assert_eq!(Arc::strong_count(&selected), 2);
    let rendered = format!("{feature:?}");
    assert!(rendered.contains("ServiceKey"));
    assert!(rendered.contains("CaptureFeature"));
    assert!(!rendered.contains(selected.as_ref()));
    assert_eq!(Arc::strong_count(&selected), 2);
    assert_eq!(completion.try_recv(), Err(mpsc::TryRecvError::Empty));
    let reducer = combine((feature, CaptureParent));
    let store = Store::builder(CaptureState::default(), reducer, runtime.runtime())
        .with_dependency::<ServiceKey>(Arc::from("store"))
        .build()
        .expect("valid Store");
    runtime.start_all();

    futures_executor::block_on(async {
        let _task = store
            .send(CaptureAction::Start)
            .await
            .expect("start commit");
        store
            .send(CaptureAction::ReadRoot)
            .await
            .expect("root read commit")
            .finish()
            .await;
    });
    runtime.start_all();
    completion
        .recv_timeout(Duration::from_secs(5))
        .expect("captured effect completes");

    futures_executor::block_on(async {
        store
            .read(move |state| {
                assert_eq!(state.root.as_deref(), Some("store"));
                assert!(Arc::ptr_eq(state.effect.as_ref().unwrap(), &selected));
            })
            .await
            .expect("state read");
        store.shutdown().await.expect("Store shutdown");
    });
    for result in runtime.join_all() {
        result.expect("actor and effect threads must finish without panic");
    }
    drop(store);
    assert!(
        released.upgrade().is_none(),
        "captured service must be released"
    );
}

/// Keeps sibling, outer and inner dependency selections independent.
#[test]
fn feature_dependencies_are_lexical_and_inner_wins() {
    let runtime = ManualRuntime::new();
    let reducer = combine((
        Probe::named("sibling"),
        with_dependency::<EndpointKey, _>(
            "outer",
            combine((
                Probe::named("outer"),
                with_dependency::<EndpointKey, _>("inner", Probe::named("inner")),
            )),
        ),
    ));
    let store = Store::builder(State::default(), reducer, runtime.runtime())
        .with_dependency::<EndpointKey>("store")
        .build()
        .expect("valid Store");
    runtime.start_all();

    futures_executor::block_on(async {
        store
            .send(Action::Read)
            .await
            .expect("read commit")
            .finish()
            .await;
        store
            .read(|state| {
                assert_eq!(state.values["sibling"], "store");
                assert_eq!(state.values["outer"], "outer");
                assert_eq!(state.values["inner"], "inner");
            })
            .await
            .expect("state read");
        store.shutdown().await.expect("Store shutdown");
    });
    for result in runtime.join_all() {
        result.expect("actor thread must finish without panic");
    }
}

/// Checks unrelated-key fallback and restores an action override before sibling reduction.
#[test]
fn unrelated_dependency_layers_fall_through_without_leaking_to_siblings() {
    /// Identifies the dependency overridden by the inner feature and one operation.
    struct InnerKey;

    impl DependencyKey for InnerKey {
        type Value = &'static str;
    }

    /// Identifies a value available only from the root store.
    struct FallbackKey;

    impl DependencyKey for FallbackKey {
        type Value = &'static str;
    }

    let probe = rca_core::Reduce::new(
        |state: &mut State, _action: &Action, context: &mut ReduceContext<'_>| {
            state
                .values
                .insert("outer", *context.dependency::<EndpointKey>());
            state
                .values
                .insert("inner", *context.dependency::<InnerKey>());
            state
                .values
                .insert("fallback", *context.dependency::<FallbackKey>());
            context.with_dependency_override::<InnerKey, _>("action-inner", |context| {
                state
                    .values
                    .insert("action-inner", *context.dependency::<InnerKey>());
                state
                    .values
                    .insert("action-outer", *context.dependency::<EndpointKey>());
                state
                    .values
                    .insert("action-fallback", *context.dependency::<FallbackKey>());
            });
            state
                .values
                .insert("restored-inner", *context.dependency::<InnerKey>());
            Effect::none()
        },
    );
    let runtime = ManualRuntime::new();
    let reducer = combine((
        with_dependency::<EndpointKey, _>(
            "feature-outer",
            with_dependency::<InnerKey, _>("feature-inner", probe),
        ),
        Probe::named("sibling"),
    ));
    let store = Store::builder(State::default(), reducer, runtime.runtime())
        .with_dependency::<EndpointKey>("store-outer")
        .with_dependency::<FallbackKey>("store-fallback")
        .build()
        .expect("valid Store");
    runtime.start_all();

    futures_executor::block_on(async {
        store
            .send(Action::Read)
            .await
            .expect("read commit")
            .finish()
            .await;
        store
            .read(|state| {
                assert_eq!(state.values["outer"], "feature-outer");
                assert_eq!(state.values["inner"], "feature-inner");
                assert_eq!(state.values["fallback"], "store-fallback");
                assert_eq!(state.values["action-inner"], "action-inner");
                assert_eq!(state.values["action-outer"], "feature-outer");
                assert_eq!(state.values["action-fallback"], "store-fallback");
                assert_eq!(state.values["restored-inner"], "feature-inner");
                assert_eq!(state.values["sibling"], "store-outer");
            })
            .await
            .expect("state read");
        store.shutdown().await.expect("Store shutdown");
    });
    for result in runtime.join_all() {
        result.expect("actor thread must finish without panic");
    }
}
