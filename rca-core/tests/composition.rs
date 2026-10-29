//! Checks path ownership, conditional trait bounds and child-first composition.

mod support;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rca_core::{
    ActionPath, CaseStatePath, Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
    OptionalStatePath, ReduceContext, Reducer, Scope, StatePath, Store, combine,
};
use rca_core::{CloneActionPath, ComputedPath, identity_action_path};

use support::ManualRuntime;

/// Holds a mutable child without requiring the parent to implement `Clone`.
struct ParentState {
    child: String,
}

/// Distinguishes a routed, owned payload from an unrelated parent action.
enum ParentAction {
    Child(Vec<u8>),
    Parent,
}

#[test]
fn computed_composition_releases_intermediate_on_inner_failure() {
    use std::cell::RefCell;

    /// Keeps the original allocation alive through both extraction failures.
    struct Root(Box<u8>);

    /// Counts release of the temporary value created by the outer path.
    struct Intermediate<'a> {
        value: Box<u8>,
        drops: &'a Cell<usize>,
    }

    impl Drop for Intermediate<'_> {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    for unwind in [false, true] {
        let root = Root(Box::new(7_u8));
        let root_address = std::ptr::from_ref(&*root.0);
        let error = Box::new(19_u8);
        let error_address = std::ptr::from_ref(&*error);
        let error = RefCell::new(Some(error));
        let drops = Cell::new(0);
        let outer_calls = Cell::new(0);
        let inner_calls = Cell::new(0);
        let outer = ComputedPath::new(
            |root: &Root| {
                outer_calls.set(outer_calls.get() + 1);
                Ok::<_, Box<u8>>(Intermediate {
                    value: Box::new(*root.0),
                    drops: &drops,
                })
            },
            |value: Intermediate<'_>| Root(Box::new(*value.value)),
        );
        let inner = ComputedPath::new(
            |value: &Intermediate<'_>| {
                inner_calls.set(inner_calls.get() + 1);
                assert_eq!(*value.value, 7);
                assert_eq!(drops.get(), 0);
                assert!(!unwind, "deliberate inner extraction unwind");
                Err::<u8, _>(error.borrow_mut().take().unwrap())
            },
            |value| Intermediate {
                value: Box::new(value),
                drops: &drops,
            },
        );
        let path = outer.then(inner);
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| path.extract(&root)));

        if unwind {
            let panic = outcome.expect_err("inner extraction panic must propagate");
            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"deliberate inner extraction unwind")
            );
            assert!(error.borrow().is_some());
        } else {
            let returned = outcome
                .expect("inner extraction error must not panic")
                .expect_err("inner extraction error must propagate");
            assert_eq!(*returned, 19);
            assert_eq!(std::ptr::from_ref(&*returned), error_address);
            assert!(error.borrow().is_none());
        }
        assert_eq!((outer_calls.get(), inner_calls.get()), (1, 1));
        assert_eq!(drops.get(), 1);
        assert_eq!(*root.0, 7);
        assert_eq!(std::ptr::from_ref(&*root.0), root_address);
    }
}

#[test]
fn paths_borrow_without_clone() {
    let state = StatePath::new(
        |state: &ParentState| &state.child,
        |state: &mut ParentState| &mut state.child,
    );
    let action = ActionPath::new(
        |action: &ParentAction| match action {
            ParentAction::Child(bytes) => Some(bytes),
            ParentAction::Parent => None,
        },
        ParentAction::Child,
    );

    let mut parent = ParentState {
        child: "old".into(),
    };
    state.get_mut(&mut parent).push_str(" value");
    assert_eq!(state.get(&parent), "old value");

    let input = ParentAction::Child(vec![1, 2, 3]);
    assert_eq!(action.extract(&input).unwrap(), &[1, 2, 3]);
    assert!(matches!(
        action.embed(vec![4]),
        ParentAction::Child(bytes) if bytes == [4]
    ));
    assert!(action.extract(&ParentAction::Parent).is_none());
}

/// Exercises both absent and present mutable child projections.
struct OptionalParent {
    child: Option<Vec<u8>>,
}

#[test]
fn optional_paths_project_presence_and_absence() {
    let path = OptionalStatePath::new(
        |state: &OptionalParent| state.child.as_ref(),
        |state: &mut OptionalParent| state.child.as_mut(),
    );
    let mut parent = OptionalParent { child: None };

    assert!(path.get(&parent).is_none());
    assert!(path.get_mut(&mut parent).is_none());

    parent.child = Some(vec![1]);
    path.get_mut(&mut parent).unwrap().push(2);
    assert_eq!(path.get(&parent).unwrap(), &[1, 2]);
}

/// Distinguishes matching and non-matching state cases without copying payloads.
enum CaseParent {
    Child(Vec<u8>),
    Parent,
}

#[test]
fn case_paths_project_matching_cases() {
    let path = CaseStatePath::new(
        |state: &CaseParent| match state {
            CaseParent::Child(child) => Some(child),
            CaseParent::Parent => None,
        },
        |state: &mut CaseParent| match state {
            CaseParent::Child(child) => Some(child),
            CaseParent::Parent => None,
        },
    );
    let mut matching = CaseParent::Child(vec![1]);
    let mut other = CaseParent::Parent;

    path.get_mut(&mut matching).unwrap().push(2);
    assert_eq!(path.get(&matching).unwrap(), &[1, 2]);
    assert!(path.get(&other).is_none());
    assert!(path.get_mut(&mut other).is_none());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the four move-only state path clone contracts in one bounded fixture"
)]
fn cloned_state_paths_preserve_move_only_storage() {
    /// Deliberately omits `Clone` so path cloning cannot copy the allocation.
    struct Payload(Box<u8>);

    /// Stores an always-present child.
    struct Required(Payload);

    /// Stores a child that can be absent.
    struct Optional(Option<Payload>);

    /// Stores either the child or an unrelated case.
    enum Case {
        Child(Payload),
        Other,
    }

    /// Stores the child under a stable key.
    struct Keyed(KeyedState<u8, Payload>);

    let calls = Rc::new(Cell::new(0));
    let read = Rc::clone(&calls);
    let write = Rc::clone(&calls);
    let path = StatePath::new(
        move |state: &Required| {
            read.set(read.get() + 1);
            &state.0
        },
        move |state: &mut Required| {
            write.set(write.get() + 1);
            &mut state.0
        },
    );
    assert_eq!(Rc::strong_count(&calls), 3);
    let cloned = path.clone();
    assert_eq!(Rc::strong_count(&calls), 5);
    drop(path);
    assert_eq!(Rc::strong_count(&calls), 3);
    let mut state = Required(Payload(Box::new(1)));
    let allocation = std::ptr::from_ref(state.0.0.as_ref());
    *cloned.get_mut(&mut state).0 = 11;
    assert_eq!(*cloned.get(&state).0, 11);
    assert_eq!(
        std::ptr::from_ref(cloned.get(&state).0.as_ref()),
        allocation
    );
    drop(cloned);
    assert_eq!(Rc::strong_count(&calls), 1);
    assert_eq!(calls.get(), 3);

    let calls = Rc::new(Cell::new(0));
    let read = Rc::clone(&calls);
    let write = Rc::clone(&calls);
    let path = OptionalStatePath::new(
        move |state: &Optional| {
            read.set(read.get() + 1);
            state.0.as_ref()
        },
        move |state: &mut Optional| {
            write.set(write.get() + 1);
            state.0.as_mut()
        },
    );
    assert_eq!(Rc::strong_count(&calls), 3);
    let cloned = path.clone();
    assert_eq!(Rc::strong_count(&calls), 5);
    drop(path);
    assert_eq!(Rc::strong_count(&calls), 3);
    let payload = Payload(Box::new(2));
    let allocation = std::ptr::from_ref(payload.0.as_ref());
    let mut state = Optional(Some(payload));
    *cloned.get_mut(&mut state).unwrap().0 = 12;
    assert_eq!(*cloned.get(&state).unwrap().0, 12);
    assert_eq!(
        std::ptr::from_ref(cloned.get(&state).unwrap().0.as_ref()),
        allocation
    );
    let mut absent = Optional(None);
    assert!(cloned.get(&absent).is_none());
    assert!(cloned.get_mut(&mut absent).is_none());
    drop(cloned);
    assert_eq!(Rc::strong_count(&calls), 1);
    assert_eq!(calls.get(), 5);

    let calls = Rc::new(Cell::new(0));
    let read = Rc::clone(&calls);
    let write = Rc::clone(&calls);
    let path = CaseStatePath::new(
        move |state: &Case| {
            read.set(read.get() + 1);
            match state {
                Case::Child(value) => Some(value),
                Case::Other => None,
            }
        },
        move |state: &mut Case| {
            write.set(write.get() + 1);
            match state {
                Case::Child(value) => Some(value),
                Case::Other => None,
            }
        },
    );
    assert_eq!(Rc::strong_count(&calls), 3);
    let cloned = path.clone();
    assert_eq!(Rc::strong_count(&calls), 5);
    drop(path);
    assert_eq!(Rc::strong_count(&calls), 3);
    let payload = Payload(Box::new(3));
    let allocation = std::ptr::from_ref(payload.0.as_ref());
    let mut state = Case::Child(payload);
    *cloned.get_mut(&mut state).unwrap().0 = 13;
    assert_eq!(*cloned.get(&state).unwrap().0, 13);
    assert_eq!(
        std::ptr::from_ref(cloned.get(&state).unwrap().0.as_ref()),
        allocation
    );
    let mut other = Case::Other;
    assert!(cloned.get(&other).is_none());
    assert!(cloned.get_mut(&mut other).is_none());
    drop(cloned);
    assert_eq!(Rc::strong_count(&calls), 1);
    assert_eq!(calls.get(), 5);

    let calls = Rc::new(Cell::new(0));
    let read = Rc::clone(&calls);
    let write = Rc::clone(&calls);
    let path = KeyedStatePath::new(
        move |state: &Keyed| {
            read.set(read.get() + 1);
            &state.0
        },
        move |state: &mut Keyed| {
            write.set(write.get() + 1);
            &mut state.0
        },
    );
    assert_eq!(Rc::strong_count(&calls), 3);
    let cloned = path.clone();
    assert_eq!(Rc::strong_count(&calls), 5);
    drop(path);
    assert_eq!(Rc::strong_count(&calls), 3);
    let payload = Payload(Box::new(4));
    let allocation = std::ptr::from_ref(payload.0.as_ref());
    let mut state = Keyed([(7, payload)].into_iter().collect());
    *cloned.get_mut(&mut state).get_mut(&7).unwrap().0 = 14;
    let rows = cloned.get(&state);
    assert_eq!(rows.ids().copied().collect::<Vec<_>>(), [7]);
    assert_eq!(*rows.get(&7).unwrap().0, 14);
    assert_eq!(
        std::ptr::from_ref(rows.get(&7).unwrap().0.as_ref()),
        allocation
    );
    drop(cloned);
    assert_eq!(Rc::strong_count(&calls), 1);
    assert_eq!(calls.get(), 2);
}

#[test]
fn cloned_action_paths_preserve_owned_payloads_and_routes() {
    /// Keeps payload identity observable across embedding and extraction.
    struct Payload(Box<u8>);

    /// Keeps a move-only key independent from the move-only payload.
    struct Id(Box<u8>);

    /// Exercises matching and non-matching routes without `Clone` bounds.
    enum Parent {
        Child(KeyedAction<Id, Payload>),
        Other,
    }

    let calls = Rc::new(Cell::new(0));
    let extract = Rc::clone(&calls);
    let embed = Rc::clone(&calls);
    let outer = ActionPath::new(
        move |root: &Option<Result<Payload, ()>>| {
            extract.set(extract.get() + 1);
            root.as_ref()
        },
        move |value| {
            embed.set(embed.get() + 1);
            Some(value)
        },
    );
    let extract = Rc::clone(&calls);
    let embed = Rc::clone(&calls);
    let inner = ActionPath::new(
        move |root: &Result<Payload, ()>| {
            extract.set(extract.get() + 1);
            root.as_ref().ok()
        },
        move |value| {
            embed.set(embed.get() + 1);
            Ok(value)
        },
    );
    let path = outer.then(inner);
    assert_eq!(Rc::strong_count(&calls), 5);
    let cloned = path.clone();
    assert_eq!(Rc::strong_count(&calls), 9);
    drop(path);
    assert_eq!(Rc::strong_count(&calls), 5);
    let payload = Payload(Box::new(7));
    let allocation = std::ptr::from_ref(payload.0.as_ref());
    let root = cloned.embed(payload);
    let borrowed = cloned.extract(&root).unwrap();
    assert_eq!(*borrowed.0, 7);
    assert_eq!(std::ptr::from_ref(borrowed.0.as_ref()), allocation);
    assert!(cloned.extract(&None).is_none());
    assert!(cloned.extract(&Some(Err(()))).is_none());
    drop(cloned);
    assert_eq!(Rc::strong_count(&calls), 1);
    assert_eq!(calls.get(), 7);

    let calls = Rc::new(Cell::new(0));
    let extract = Rc::clone(&calls);
    let embed = Rc::clone(&calls);
    let path = KeyedActionPath::new(
        move |root: &Parent| {
            extract.set(extract.get() + 1);
            match root {
                Parent::Child(value) => Some(value),
                Parent::Other => None,
            }
        },
        move |value| {
            embed.set(embed.get() + 1);
            Parent::Child(value)
        },
    );
    assert_eq!(Rc::strong_count(&calls), 3);
    let cloned = path.clone();
    assert_eq!(Rc::strong_count(&calls), 5);
    drop(path);
    assert_eq!(Rc::strong_count(&calls), 3);
    let id = Id(Box::new(5));
    let payload = Payload(Box::new(9));
    let id_allocation = std::ptr::from_ref(id.0.as_ref());
    let payload_allocation = std::ptr::from_ref(payload.0.as_ref());
    let root = cloned.embed(id, payload);
    let (id, payload) = cloned.extract(&root).unwrap();
    assert_eq!((*id.0, *payload.0), (5, 9));
    assert_eq!(std::ptr::from_ref(id.0.as_ref()), id_allocation);
    assert_eq!(std::ptr::from_ref(payload.0.as_ref()), payload_allocation);
    assert!(cloned.extract(&Parent::Other).is_none());
    let Parent::Child(action) = root else {
        panic!("keyed route must retain its case");
    };
    let (id, payload) = action.into_parts();
    assert_eq!((*id.0, *payload.0), (5, 9));
    assert_eq!(std::ptr::from_ref(id.0.as_ref()), id_allocation);
    assert_eq!(std::ptr::from_ref(payload.0.as_ref()), payload_allocation);
    drop(cloned);
    assert_eq!(Rc::strong_count(&calls), 1);
    assert_eq!(calls.get(), 3);
}

/// Deliberately lacks `Debug` so path formatting cannot require payload formatting.
struct Hidden(&'static str);

#[test]
fn path_debug_output_hides_closures() {
    let hidden = Hidden("state closure capture");
    let state = StatePath::new(
        move |state: &ParentState| {
            let _ = hidden.0;
            &state.child
        },
        |state: &mut ParentState| &mut state.child,
    );
    let hidden = Hidden("optional closure capture");
    let optional = OptionalStatePath::new(
        move |state: &OptionalParent| {
            let _ = hidden.0;
            state.child.as_ref()
        },
        |state: &mut OptionalParent| state.child.as_mut(),
    );
    let hidden = Hidden("case closure capture");
    let case = CaseStatePath::new(
        move |state: &CaseParent| {
            let _ = hidden.0;
            match state {
                CaseParent::Child(child) => Some(child),
                CaseParent::Parent => None,
            }
        },
        |state: &mut CaseParent| match state {
            CaseParent::Child(child) => Some(child),
            CaseParent::Parent => None,
        },
    );
    let hidden = Hidden("action closure capture");
    let action = ActionPath::new(
        move |action: &ParentAction| {
            let _ = hidden.0;
            match action {
                ParentAction::Child(child) => Some(child),
                ParentAction::Parent => None,
            }
        },
        ParentAction::Child,
    );

    let rendered = format!("{state:?} {optional:?} {case:?} {action:?}");
    assert_eq!(
        rendered,
        "StatePath { .. } OptionalStatePath { .. } CaseStatePath { .. } ActionPath { .. }"
    );
    assert!(!rendered.contains("closure capture"));

    let keyed_state = KeyedStatePath::new(
        |state: &KeyedState<u8, Hidden>| state,
        |state: &mut KeyedState<u8, Hidden>| state,
    );
    let keyed_action = KeyedActionPath::new(
        |action: &KeyedAction<u8, Hidden>| Some(action),
        std::convert::identity,
    );
    let composed = action.then(identity_action_path());
    assert_eq!(format!("{composed:?}"), "ComposedActionPath { .. }");
    let composed_embedding = CloneActionPath::clone_embedder(&composed);
    let lifted = composed.lift_optional();
    let lifted_embedding = CloneActionPath::clone_embedder(&lifted);
    let computed = ComputedPath::new(
        |(): &()| Ok::<_, Hidden>(Hidden("computed closure capture")),
        |_: Hidden| (),
    );

    let rendered = format!(
        "{keyed_state:?} {keyed_action:?} {lifted:?} {composed_embedding:?} {lifted_embedding:?} {computed:?}"
    );
    assert_eq!(
        rendered,
        "KeyedStatePath { .. } KeyedActionPath { .. } LiftedActionPath { .. } ComposedActionEmbedding { .. } LiftedActionEmbedding { .. } ComputedPath { .. }"
    );
    assert!(!rendered.contains("closure capture"));
}

/// Requires implicit copying rather than calling `Clone` explicitly.
fn copy_twice<T: Copy>(value: T) -> (T, T) {
    (value, value)
}

#[test]
fn paths_are_copy_when_their_closures_are_copy() {
    let state = StatePath::new(
        |state: &ParentState| &state.child,
        |state: &mut ParentState| &mut state.child,
    );
    let optional = OptionalStatePath::new(
        |state: &OptionalParent| state.child.as_ref(),
        |state: &mut OptionalParent| state.child.as_mut(),
    );
    let case = CaseStatePath::new(
        |state: &CaseParent| match state {
            CaseParent::Child(child) => Some(child),
            CaseParent::Parent => None,
        },
        |state: &mut CaseParent| match state {
            CaseParent::Child(child) => Some(child),
            CaseParent::Parent => None,
        },
    );
    let action = ActionPath::new(
        |action: &ParentAction| match action {
            ParentAction::Child(child) => Some(child),
            ParentAction::Parent => None,
        },
        ParentAction::Child,
    );

    let _ = copy_twice(state);
    let _ = copy_twice(optional);
    let _ = copy_twice(case);
    let _ = copy_twice(action);
}

/// Records the first component while borrowing the shared action.
struct First;

/// Records the second component while borrowing the same action.
struct Second;

/// Records execution order for comparison with the expected literal sequence.
struct CombinedState {
    calls: Vec<&'static str>,
}

/// Carries an owned payload that combined reducers must borrow.
enum CombinedAction {
    Run(Vec<u8>),
}

impl Reducer for First {
    type State = CombinedState;
    type Action = CombinedAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let CombinedAction::Run(bytes) = action;
        assert_eq!(bytes, &[1]);
        state.calls.push("first");
        Effect::none()
    }
}

impl Reducer for Second {
    type State = CombinedState;
    type Action = CombinedAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let CombinedAction::Run(bytes) = action;
        assert_eq!(bytes, &[1]);
        state.calls.push("second");
        Effect::none()
    }
}

#[test]
fn combine_lends_one_action_in_source_order() {
    let runtime = ManualRuntime::new();
    let store = Store::new(
        CombinedState { calls: vec![] },
        combine((First, Second)),
        runtime.runtime(),
    );
    let receipt = store
        .try_send(CombinedAction::Run(vec![1]))
        .expect("combined action fits");
    runtime.start_all();

    futures_executor::block_on(async {
        receipt
            .await
            .expect("combined action commits")
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| state.calls.clone())
                .await
                .expect("combined state read"),
            ["first", "second"]
        );
        store.shutdown().await.expect("combined store shuts down");
    });
}

/// Separates invocation count from the child payload's observed length.
struct ScopedChildState {
    bytes_seen: usize,
    calls: usize,
}

/// Records what the parent observes after child reduction.
struct ScopedAppState {
    child: ScopedChildState,
    parent_saw: bool,
}

/// Carries the child payload without requiring `Clone`.
enum ScopedChildAction {
    Data(Vec<u8>),
}

/// Separates routed child input from unrelated parent input.
enum ScopedAppAction {
    Child(ScopedChildAction),
    Parent,
}

/// Handles one child visit without producing an effect that needs embedding.
struct ScopedChild;

impl Reducer for ScopedChild {
    type State = ScopedChildState;
    type Action = ScopedChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let ScopedChildAction::Data(bytes) = action;
        state.calls += 1;
        state.bytes_seen = bytes.len();
        Effect::none()
    }
}

/// Checks that the child's update is visible during the same parent reduction.
struct ScopedParent;

impl Reducer for ScopedParent {
    type State = ScopedAppState;
    type Action = ScopedAppAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        if let ScopedAppAction::Child(ScopedChildAction::Data(bytes)) = action {
            assert_eq!(bytes, &[1, 2, 3]);
            assert_eq!(state.child.bytes_seen, 3, "child must reduce first");
            assert_eq!(state.child.calls, 1, "child must reduce once");
            state.parent_saw = true;
        }
        Effect::none()
    }
}

/// Detects unnecessary cloning of an effect embedder on the no-effect path.
struct CloneProbe(Arc<AtomicUsize>);

impl Clone for CloneProbe {
    fn clone(&self) -> Self {
        self.0.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(&self.0))
    }
}

#[test]
fn scope_runs_child_then_parent_without_cloning_action() {
    let runtime = ManualRuntime::new();
    let child_state = StatePath::new(
        |state: &ScopedAppState| &state.child,
        |state: &mut ScopedAppState| &mut state.child,
    );
    let embedder_clones = Arc::new(AtomicUsize::new(0));
    let probe = CloneProbe(Arc::clone(&embedder_clones));
    let child_action = ActionPath::new(
        |action: &ScopedAppAction| match action {
            ScopedAppAction::Child(action) => Some(action),
            ScopedAppAction::Parent => None,
        },
        move |action| {
            let _ = &probe;
            ScopedAppAction::Child(action)
        },
    );
    let reducer = combine((
        Scope::new(child_state, child_action, ScopedChild),
        ScopedParent,
    ));
    let store = Store::new(
        ScopedAppState {
            child: ScopedChildState {
                bytes_seen: 0,
                calls: 0,
            },
            parent_saw: false,
        },
        reducer,
        runtime.runtime(),
    );
    let receipt = store
        .try_send(ScopedAppAction::Child(ScopedChildAction::Data(vec![
            1, 2, 3,
        ])))
        .expect("scoped action fits");
    runtime.start_all();

    futures_executor::block_on(async {
        receipt.await.expect("scoped action commits").finish().await;
        assert!(
            store
                .read(|state| state.parent_saw && state.child.bytes_seen == 3)
                .await
                .expect("scoped state read")
        );
        store.shutdown().await.expect("scoped store shuts down");
    });
    assert_eq!(embedder_clones.load(Ordering::SeqCst), 0);
}

#[test]
fn scope_nonmatch_skips_child() {
    let runtime = ManualRuntime::new();
    let reducer = Scope::new(
        StatePath::new(
            |state: &ScopedAppState| &state.child,
            |state: &mut ScopedAppState| &mut state.child,
        ),
        ActionPath::new(
            |action: &ScopedAppAction| match action {
                ScopedAppAction::Child(action) => Some(action),
                ScopedAppAction::Parent => None,
            },
            ScopedAppAction::Child,
        ),
        ScopedChild,
    );
    let store = Store::new(
        ScopedAppState {
            child: ScopedChildState {
                bytes_seen: 0,
                calls: 0,
            },
            parent_saw: false,
        },
        reducer,
        runtime.runtime(),
    );
    let receipt = store
        .try_send(ScopedAppAction::Parent)
        .expect("nonmatching action fits");
    runtime.start_all();

    futures_executor::block_on(async {
        receipt
            .await
            .expect("nonmatching action commits")
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| state.child.calls)
                .await
                .expect("scoped state read"),
            0
        );
        store.shutdown().await.expect("scoped store shuts down");
    });
}

/// Uses a `Cell` action to expose accidental `Sync` bounds on scoped actions.
struct NonSyncReducer;

impl Reducer for NonSyncReducer {
    type State = u8;
    type Action = Cell<u8>;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        *state = action.get();
        Effect::none()
    }
}

/// Requires the composed reducer to be `Send` without requiring a `Sync` action.
fn assert_send_reducer<R>(_: &R)
where
    R: Reducer<State = u8, Action = Cell<u8>> + Send,
{
}

#[test]
fn scope_auto_traits_follow_stored_values() {
    let reducer = Scope::new(
        StatePath::new(|state: &u8| state, |state: &mut u8| state),
        ActionPath::new(|action: &Cell<u8>| Some(action), |action| action),
        NonSyncReducer,
    );

    assert_send_reducer(&reducer);
    assert_eq!(format!("{reducer:?}"), "Scope { .. }");
}
