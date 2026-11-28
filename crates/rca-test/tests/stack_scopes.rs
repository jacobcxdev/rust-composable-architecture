//! Exercises actual stack scope authority and deterministic effect retirement.

/// Exercises the bounded job-tree recipe described in `docs/recursive-jobs.md`.
mod finite_jobs {
    use super::{ChildAction, Route, drive, reducer, scope};
    use rca_core::{
        ActionPath, Effect, EffectSender, ReduceContext, Reducer, ScopedStore, StackAction,
        StackId, StackState, StatePath, Store, stack,
    };
    use rca_test::DeterministicRuntime;
    use std::{convert::Infallible, sync::mpsc, time::Duration};

    /// Caps this fixture's admitted depth, not the library's recursion depth.
    const MAX_DEPTH: usize = 3;
    /// Bounds input size before recursively constructing the fixture tree.
    const MAX_NODES: usize = 7;
    /// Limits each fixture node to two children.
    const MAX_CHILDREN: u8 = 2;

    /// Stores local work and child lifetimes without sharing state between jobs.
    struct Job {
        value: u8,
        children: StackState<Job>,
    }

    /// Decodes preorder `(value, child count)` pairs without importing runtime IDs.
    ///
    /// Rejects excessive size, width or depth, and incomplete or trailing input.
    fn decode_job(mut input: &[u8]) -> Result<Job, &'static str> {
        if input.len() > 2 * MAX_NODES {
            return Err("job input too large");
        }
        let job = decode_node(&mut input, 1)?;
        if !input.is_empty() {
            return Err("trailing job input");
        }
        Ok(job)
    }

    /// Consumes one bounded subtree and leaves any following node pairs unread.
    fn decode_node(input: &mut &[u8], depth: usize) -> Result<Job, &'static str> {
        let [value, count, rest @ ..] = *input else {
            return Err("incomplete job input");
        };
        let (value, count) = (*value, *count);
        if count > MAX_CHILDREN {
            return Err("too many child jobs");
        }
        if depth == MAX_DEPTH && count != 0 {
            return Err("job depth exceeded");
        }
        *input = rest;
        let mut job = Job {
            value,
            children: StackState::new(),
        };
        for _ in 0..count {
            job.children.push(decode_node(input, depth + 1)?);
        }
        Ok(job)
    }

    // Missing validation would admit unbounded recursive routing/drop or partial input.
    #[test]
    fn decoder_rejects_depth_width_size_and_incomplete_or_trailing_input() {
        for (input, expected) in [
            (&[][..], "incomplete job input"),
            (&[1][..], "incomplete job input"),
            (&[1, 1][..], "incomplete job input"),
            (&[1, 3, 2, 0, 3, 0, 4, 0][..], "too many child jobs"),
            (&[1, 1, 2, 1, 3, 1, 4, 0][..], "job depth exceeded"),
            (&[1, 0, 2, 0][..], "trailing job input"),
            (&[0; 2 * MAX_NODES + 1][..], "job input too large"),
        ] {
            match decode_job(input) {
                Ok(_) => panic!("accepted {input:?}"),
                Err(error) => assert_eq!(error, expected, "input: {input:?}"),
            }
        }
    }

    // Rejecting the boundary or losing a child changes these hand-derived values.
    #[test]
    fn decoder_accepts_and_drops_the_full_three_level_seven_node_tree() {
        let tree = decode_job(&[1, 2, 2, 2, 3, 0, 4, 0, 5, 2, 6, 0, 7, 0]).unwrap();
        assert_eq!(tree.value, 1);
        assert_eq!(tree.children.len(), 2);
        let left = tree.children.values().next().unwrap();
        let right = tree.children.values().nth(1).unwrap();
        assert_eq!((left.value, right.value), (2, 5));
        assert_eq!(
            left.children
                .values()
                .map(|job| job.value)
                .collect::<Vec<_>>(),
            [3, 4]
        );
        assert_eq!(
            right
                .children
                .values()
                .map(|job| job.value)
                .collect::<Vec<_>>(),
            [6, 7]
        );
        assert!(
            left.children
                .values()
                .chain(right.children.values())
                .all(|job| job.children.is_empty())
        );
        drop(tree);
    }

    /// Has no `Clone` implementation, so rejection must return the original allocation.
    struct OwnedValue(u8);

    /// Boxes recursive routes and owned input while keeping dynamic pushes impossible.
    enum JobAction {
        Children(Box<StackAction<Infallible, JobAction>>),
        Start,
        Tick,
        Record(Box<OwnedValue>),
        Capture(mpsc::Sender<EffectSender<JobAction>>),
    }

    /// Owns only one job's value, timer and captured output handle.
    struct JobReducer;
    impl Reducer for JobReducer {
        type State = Job;
        type Action = JobAction;

        fn reduce(
            &mut self,
            state: &mut Job,
            action: &JobAction,
            _: &mut ReduceContext<'_>,
        ) -> Effect<JobAction> {
            match action {
                JobAction::Start => Effect::send_after(Duration::from_secs(1), JobAction::Tick),
                JobAction::Tick => {
                    state.value += 1;
                    Effect::none()
                }
                JobAction::Record(value) => {
                    state.value = value.0;
                    Effect::none()
                }
                JobAction::Capture(captured) => {
                    let captured = captured.clone();
                    Effect::run(move |sender| async move {
                        captured.send(sender).unwrap();
                        std::future::pending::<()>().await;
                    })
                }
                JobAction::Children(_) => Effect::none(),
            }
        }
    }

    /// Adds one statically composed level; decoded input determines the finite depth.
    fn children(
        child: impl Reducer<State = Job, Action = JobAction> + Send,
    ) -> impl Reducer<State = Job, Action = JobAction> + Send {
        stack(
            JobReducer,
            StatePath::new(|job: &Job| &job.children, |job: &mut Job| &mut job.children),
            ActionPath::new(
                |action: &JobAction| match action {
                    JobAction::Children(action) => Some(action.as_ref()),
                    _ => None,
                },
                |action| JobAction::Children(Box::new(action)),
            ),
            |input: &Infallible, _: &mut ReduceContext<'_>| match *input {},
            child,
        )
    }

    /// Wraps one child action without copying its owned payload.
    fn child(id: StackId, action: JobAction) -> JobAction {
        JobAction::Children(Box::new(StackAction::Element { id, action }))
    }

    /// Acquires a child job handle with the stack entry's current lifetime.
    fn job_scope(
        runtime: &DeterministicRuntime,
        store: &Store<Job, JobAction>,
        id: StackId,
    ) -> ScopedStore<Job, JobAction> {
        drive(
            runtime,
            store.scope_stack(
                StatePath::new(|job: &Job| &job.children, |job: &mut Job| &mut job.children),
                ActionPath::new(
                    |action: &JobAction| match action {
                        JobAction::Children(action) => Some(action.as_ref()),
                        _ => None,
                    },
                    |action| JobAction::Children(Box::new(action)),
                ),
                id,
            ),
        )
        .unwrap()
    }

    // A lost nested map, eager timer or shared node state changes the literal results.
    #[test]
    fn recursive_and_flat_stacks_deliver_delayed_work_to_independent_jobs() {
        let runtime = DeterministicRuntime::new();
        let tree = decode_job(&[0, 2, 10, 0, 20, 1, 30, 0]).unwrap();
        let ids: Vec<_> = tree.children.ids().cloned().collect();
        let grandchild = tree
            .children
            .get(&ids[1])
            .unwrap()
            .children
            .ids()
            .next()
            .unwrap()
            .clone();
        // The composition is deliberately unrolled to the admitted three job levels.
        let store = Store::new(tree, children(children(JobReducer)), runtime.runtime());
        let sibling = job_scope(&runtime, &store, ids[0].clone());
        let branch = job_scope(&runtime, &store, ids[1].clone());
        let nested_tasks = [
            drive(&runtime, sibling.send(JobAction::Start)).unwrap(),
            drive(
                &runtime,
                branch.send(child(grandchild.clone(), JobAction::Start)),
            )
            .unwrap(),
        ];

        let mut flat = StackState::new();
        let first = flat.push(Route { value: 10 });
        let second = flat.push(Route { value: 30 });
        let flat_store = Store::new(flat, reducer(), runtime.runtime());
        let first = scope(&runtime, &flat_store, first);
        let second = scope(&runtime, &flat_store, second);
        let flat_tasks = [
            drive(&runtime, first.send(ChildAction::Start)).unwrap(),
            drive(&runtime, second.send(ChildAction::Start)).unwrap(),
        ];
        assert_eq!(drive(&runtime, sibling.read(|job| job.value)).unwrap(), 10);
        let lookup = grandchild.clone();
        assert_eq!(
            drive(
                &runtime,
                branch.read(move |job| job.children.get(&lookup).unwrap().value)
            )
            .unwrap(),
            30
        );
        assert_eq!(drive(&runtime, first.read(|job| job.value)).unwrap(), 10);
        assert_eq!(drive(&runtime, second.read(|job| job.value)).unwrap(), 30);
        assert!(
            nested_tasks
                .iter()
                .chain(&flat_tasks)
                .all(|task| !task.is_finished())
        );
        runtime.advance(Duration::from_secs(1)).unwrap();
        assert_eq!(drive(&runtime, sibling.read(|job| job.value)).unwrap(), 11);
        assert_eq!(
            drive(
                &runtime,
                branch.read(move |job| (job.value, job.children.get(&grandchild).unwrap().value))
            )
            .unwrap(),
            (20, 31)
        );
        assert_eq!(drive(&runtime, store.read(|job| job.value)).unwrap(), 0);
        assert_eq!(drive(&runtime, first.read(|job| job.value)).unwrap(), 11);
        assert_eq!(drive(&runtime, second.read(|job| job.value)).unwrap(), 31);
        assert!(
            nested_tasks
                .iter()
                .chain(&flat_tasks)
                .all(rca_core::ActionTask::is_finished)
        );
        drive(&runtime, store.shutdown()).unwrap();
        drive(&runtime, flat_store.shutdown()).unwrap();
    }

    // Missing ancestor retirement would admit the old output or consume its owned input.
    #[test]
    fn ancestor_retirement_rejects_descendant_work_and_recovers_exact_owned_input() {
        let runtime = DeterministicRuntime::new();
        let tree = decode_job(&[0, 2, 10, 0, 20, 1, 30, 0]).unwrap();
        let ids: Vec<_> = tree.children.ids().cloned().collect();
        let grandchild = tree
            .children
            .get(&ids[1])
            .unwrap()
            .children
            .ids()
            .next()
            .unwrap()
            .clone();
        let store = Store::new(tree, children(children(JobReducer)), runtime.runtime());
        let sibling = job_scope(&runtime, &store, ids[0].clone());
        let branch = job_scope(&runtime, &store, ids[1].clone());
        let sibling_timer = drive(&runtime, sibling.send(JobAction::Start)).unwrap();
        let descendant_timer = drive(
            &runtime,
            branch.send(child(grandchild.clone(), JobAction::Start)),
        )
        .unwrap();
        let (capture, output) = mpsc::channel();
        let work = drive(
            &runtime,
            branch.send(child(grandchild.clone(), JobAction::Capture(capture))),
        )
        .unwrap();
        runtime.run_until_stalled().unwrap();
        let output = output.try_recv().unwrap();
        drive(
            &runtime,
            output.send(JobAction::Record(Box::new(OwnedValue(41)))),
        )
        .unwrap();
        let lookup = grandchild.clone();
        assert_eq!(
            drive(
                &runtime,
                branch.read(move |job| job.children.get(&lookup).unwrap().value)
            )
            .unwrap(),
            41
        );

        let _ = drive(
            &runtime,
            store.send(JobAction::Children(Box::new(StackAction::PopFrom {
                id: ids[1].clone(),
            }))),
        )
        .unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(work.is_finished());
        assert!(descendant_timer.is_finished());
        assert!(!sibling_timer.is_finished());
        assert!(
            drive(&runtime, output.send(JobAction::Tick))
                .unwrap_err()
                .is_cancelled()
        );
        assert!(
            drive(&runtime, branch.read(|job| job.value))
                .unwrap_err()
                .is_scope_ended()
        );

        let payload = Box::new(OwnedValue(99));
        let address = std::ptr::from_ref(payload.as_ref());
        let rejected = branch
            .try_send(child(grandchild.clone(), JobAction::Record(payload)))
            .unwrap_err();
        assert!(rejected.is_scope_ended());
        let JobAction::Children(action) = rejected.into_action() else {
            panic!("original route returned")
        };
        let StackAction::Element {
            id,
            action: JobAction::Record(payload),
        } = *action
        else {
            panic!("original payload returned")
        };
        assert_eq!(id, grandchild);
        assert_eq!(std::ptr::from_ref(payload.as_ref()), address);
        assert_eq!(payload.0, 99);
        runtime.advance(Duration::from_secs(1)).unwrap();
        assert!(sibling_timer.is_finished());
        assert_eq!(drive(&runtime, sibling.read(|job| job.value)).unwrap(), 11);
        assert_eq!(
            drive(&runtime, store.read(|job| job.children.len())).unwrap(),
            1
        );
        drive(&runtime, store.shutdown()).unwrap();
    }
}

use futures_executor::block_on;
use rca_core::{
    ActionPath, CaseStatePath, Dismiss, Effect, EffectSender, KeyedAction, KeyedActionPath,
    KeyedState, KeyedStatePath, OptionalStatePath, ReduceContext, Reducer, ScopedStore,
    StackAction, StackEncoding, StackId, StackState, StatePath, Store, for_each, if_case, if_some,
    stack,
};
use rca_test::DeterministicRuntime;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// Gives each stack entry an independent value for delivery assertions.
struct Route {
    value: u8,
}

/// Starts delayed work or captures output and dismissal handles for later use.
enum ChildAction {
    Start,
    Tick,
    Capture(mpsc::Sender<EffectSender<ChildAction>>),
    CaptureDismiss(Arc<Mutex<Option<Dismiss>>>),
    Replay(Dismiss),
}

/// Exercises normal routing, same-ID replacement and decoded-state installation.
enum Action {
    Path(StackAction<u8, ChildAction>),
    Recreate(StackId),
    Restore(StackEncoding),
}

/// Installs decoded paths only after the entire input passes validation.
struct Parent;
impl Reducer for Parent {
    type State = StackState<Route>;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if let Action::Recreate(id) = action {
            state.try_pop_from(id).unwrap();
            state
                .try_push(id.clone(), Route { value: 40 })
                .unwrap_or_else(|_| panic!("replacement ID was removed"));
        }
        if let Action::Restore(input) = action {
            let decoded = StackState::decode_with(input.clone(), |route| {
                match (route.tag.as_str(), route.version, route.payload.as_slice()) {
                    ("route", 1, [value]) => Ok(Route { value: *value }),
                    _ => Err("invalid route"),
                }
            });
            if let Ok(decoded) = decoded {
                *state = decoded;
            }
        }
        Effect::none()
    }
}
/// Owns one route's timer, output sender and nearest-entry dismissal target.
struct Child;
impl Reducer for Child {
    type State = Route;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut Route,
        action: &ChildAction,
        context: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Start => Effect::send_after(Duration::from_secs(1), ChildAction::Tick),
            ChildAction::Tick => {
                state.value += 1;
                Effect::none()
            }
            ChildAction::Capture(captured) => {
                let captured = captured.clone();
                Effect::run(move |sender| async move {
                    captured.send(sender).unwrap();
                    std::future::pending::<()>().await;
                })
            }
            ChildAction::CaptureDismiss(captured) => {
                *captured.lock().unwrap() = Some(context.dismiss());
                Effect::none()
            }
            ChildAction::Replay(dismiss) => Effect::concatenate([
                Effect::none(),
                Effect::merge([
                    Effect::none(),
                    dismiss
                        .effect()
                        .cancellable("close")
                        .without_inherited_cancellation(),
                ]),
            ]),
        }
    }
}

/// Composes the parent and route fixture through the real stack adapter.
fn reducer() -> impl Reducer<State = StackState<Route>, Action = Action> + Send {
    stack(
        Parent,
        StatePath::new(
            |state: &StackState<Route>| state,
            |state: &mut StackState<Route>| state,
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::Path(action) => Some(action),
                Action::Recreate(_) | Action::Restore(_) => None,
            },
            Action::Path,
        ),
        |value: &u8, _: &mut ReduceContext<'_>| Route { value: *value },
        Child,
    )
}

/// Runs one caller operation without advancing virtual time.
fn drive<F: Future + Send>(runtime: &DeterministicRuntime, future: F) -> F::Output
where
    F::Output: Send,
{
    block_on(runtime.run(future)).unwrap()
}

/// Acquires a route handle without reconstructing its private lifetime identity.
fn scope(
    runtime: &DeterministicRuntime,
    store: &Store<StackState<Route>, Action>,
    id: StackId,
) -> ScopedStore<Route, ChildAction> {
    drive(
        runtime,
        store.scope_stack(
            StatePath::new(
                |state: &StackState<Route>| state,
                |state: &mut StackState<Route>| state,
            ),
            ActionPath::new(
                |action: &Action| match action {
                    Action::Path(action) => Some(action),
                    Action::Recreate(_) | Action::Restore(_) => None,
                },
                Action::Path,
            ),
            id,
        ),
    )
    .unwrap()
}

// A global cancellation or public-ID-only scope identity breaks these outcomes.
#[test]
fn inclusive_pop_retires_suffix_timers_outputs_readers_but_preserves_prefix() {
    let runtime = DeterministicRuntime::new();
    let mut state = StackState::new();
    let a = state.push(Route { value: 1 });
    let b = state.push(Route { value: 10 });
    let c = state.push(Route { value: 20 });
    let store = Store::new(state, reducer(), runtime.runtime());
    let prefix = scope(&runtime, &store, a);
    let middle = scope(&runtime, &store, b.clone());
    let tail = scope(&runtime, &store, c);
    let mut middle_reader = drive(&runtime, middle.observe(|route| route.value)).unwrap();
    let mut tail_reader = drive(&runtime, tail.observe(|route| route.value)).unwrap();
    assert_eq!(drive(&runtime, middle_reader.next()), Some(10));
    assert_eq!(drive(&runtime, tail_reader.next()), Some(20));
    let prefix_timer = drive(&runtime, prefix.send(ChildAction::Start)).unwrap();
    let middle_timer = drive(&runtime, middle.send(ChildAction::Start)).unwrap();
    let tail_timer = drive(&runtime, tail.send(ChildAction::Start)).unwrap();
    let (capture, output) = mpsc::channel();
    let middle_work = drive(&runtime, middle.send(ChildAction::Capture(capture.clone()))).unwrap();
    let tail_work = drive(&runtime, tail.send(ChildAction::Capture(capture))).unwrap();
    runtime.run_until_stalled().unwrap();
    let middle_output = output.try_recv().unwrap();
    let tail_output = output.try_recv().unwrap();
    let _ = drive(
        &runtime,
        store.send(Action::Path(StackAction::PopFrom { id: b })),
    )
    .unwrap();
    runtime.run_until_stalled().unwrap();
    for task in [&middle_timer, &tail_timer, &middle_work, &tail_work] {
        assert!(task.is_finished());
    }
    assert!(!prefix_timer.is_finished());
    assert!(
        drive(&runtime, middle_output.send(ChildAction::Tick))
            .unwrap_err()
            .is_cancelled()
    );
    assert!(
        drive(&runtime, tail_output.send(ChildAction::Tick))
            .unwrap_err()
            .is_cancelled()
    );
    assert_eq!(drive(&runtime, middle_reader.next()), None);
    assert_eq!(drive(&runtime, tail_reader.next()), None);
    assert!(
        drive(&runtime, middle.read(|route| route.value))
            .unwrap_err()
            .is_scope_ended()
    );
    assert!(
        drive(&runtime, tail.send(ChildAction::Tick))
            .unwrap_err()
            .is_scope_ended()
    );
    runtime.advance(Duration::from_secs(1)).unwrap();
    assert!(prefix_timer.is_finished());
    assert_eq!(
        drive(&runtime, prefix.read(|route| route.value)).unwrap(),
        2
    );
    assert_eq!(drive(&runtime, store.read(StackState::len)).unwrap(), 1);
    drive(&runtime, store.shutdown()).unwrap();
}

// Equal final public IDs must not renew old output, readers or dismiss clients.
#[test]
fn same_id_reinsertion_revokes_old_authority_and_keeps_replacement_and_sibling_live() {
    let runtime = DeterministicRuntime::new();
    let mut state = StackState::new();
    let a = state.push(Route { value: 1 });
    let b = state.push(Route { value: 10 });
    let store = Store::new(state, reducer(), runtime.runtime());
    let prefix = scope(&runtime, &store, a);
    let old = scope(&runtime, &store, b.clone());
    let captured = Arc::new(Mutex::new(None));
    let _ = drive(
        &runtime,
        old.send(ChildAction::CaptureDismiss(Arc::clone(&captured))),
    )
    .unwrap();
    let dismiss = captured.lock().unwrap().take().unwrap();
    let (capture, output) = mpsc::channel();
    let work = drive(&runtime, old.send(ChildAction::Capture(capture))).unwrap();
    runtime.run_until_stalled().unwrap();
    let old_output = output.try_recv().unwrap();
    let _ = drive(&runtime, store.send(Action::Recreate(b.clone()))).unwrap();
    runtime.run_until_stalled().unwrap();
    assert!(work.is_finished());
    assert!(
        drive(&runtime, old_output.send(ChildAction::Tick))
            .unwrap_err()
            .is_cancelled()
    );
    assert!(
        drive(&runtime, old.read(|route| route.value))
            .unwrap_err()
            .is_scope_ended()
    );
    let replacement = scope(&runtime, &store, b);
    let replay = drive(&runtime, replacement.send(ChildAction::Replay(dismiss))).unwrap();
    drive(&runtime, replay.finish());
    assert_eq!(
        drive(&runtime, replacement.read(|route| route.value)).unwrap(),
        40
    );
    let _ = drive(&runtime, replacement.send(ChildAction::Tick)).unwrap();
    assert_eq!(
        drive(&runtime, replacement.read(|route| route.value)).unwrap(),
        41
    );
    assert_eq!(
        drive(&runtime, prefix.read(|route| route.value)).unwrap(),
        1
    );
    drive(&runtime, store.shutdown()).unwrap();
}

/// Gives a whole stack an enum-case lifetime.
enum Choice {
    Active(StackState<Route>),
    Empty,
}

/// Holds independent stacks whose entries may share the same public ID.
struct Containers {
    optional: Option<StackState<Route>>,
    case: Choice,
    rows: KeyedState<u8, StackState<Route>>,
}

/// Selects optional (0), case (1), or the first keyed row (2) for removal.
enum ContainerAction {
    Optional(Action),
    Case(Action),
    Row(KeyedAction<u8, Action>),
    Remove(u8),
}

/// Removes one enclosing lifetime without changing neighbouring stacks.
struct ContainersParent;
impl Reducer for ContainersParent {
    type State = Containers;
    type Action = ContainerAction;
    fn reduce(
        &mut self,
        state: &mut Containers,
        action: &ContainerAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ContainerAction> {
        match action {
            ContainerAction::Remove(0) => state.optional = None,
            ContainerAction::Remove(1) => state.case = Choice::Empty,
            ContainerAction::Remove(2) => {
                state.rows.remove(&1);
            }
            _ => {}
        }
        Effect::none()
    }
}

/// Wraps independent stacks in optional, case and keyed parents.
fn container_reducer() -> impl Reducer<State = Containers, Action = ContainerAction> + Send {
    let keyed = for_each(
        ContainersParent,
        KeyedStatePath::new(
            |state: &Containers| &state.rows,
            |state: &mut Containers| &mut state.rows,
        ),
        KeyedActionPath::new(
            |action: &ContainerAction| match action {
                ContainerAction::Row(action) => Some(action),
                _ => None,
            },
            ContainerAction::Row,
        ),
        reducer(),
    );
    let case = if_case(
        keyed,
        CaseStatePath::new(
            |state: &Containers| match &state.case {
                Choice::Active(path) => Some(path),
                Choice::Empty => None,
            },
            |state: &mut Containers| match &mut state.case {
                Choice::Active(path) => Some(path),
                Choice::Empty => None,
            },
        ),
        ActionPath::new(
            |action: &ContainerAction| match action {
                ContainerAction::Case(action) => Some(action),
                _ => None,
            },
            ContainerAction::Case,
        ),
        reducer(),
    );
    if_some(
        case,
        OptionalStatePath::new(
            |state: &Containers| state.optional.as_ref(),
            |state: &mut Containers| state.optional.as_mut(),
        ),
        ActionPath::new(
            |action: &ContainerAction| match action {
                ContainerAction::Optional(action) => Some(action),
                _ => None,
            },
            ContainerAction::Optional,
        ),
        reducer(),
    )
}

/// Routes to optional (0), case (1), first row (2), or the surviving second row.
fn container_action(container: u8, id: StackId, action: ChildAction) -> ContainerAction {
    let action = Action::Path(StackAction::Element { id, action });
    match container {
        0 => ContainerAction::Optional(action),
        1 => ContainerAction::Case(action),
        2 => ContainerAction::Row(KeyedAction::new(1, action)),
        _ => ContainerAction::Row(KeyedAction::new(2, action)),
    }
}

// Each removed parent must end its actual stack descendants, not unrelated stacks.
#[test]
fn optional_case_and_keyed_parents_retire_stack_descendants_while_other_stacks_survive() {
    for removed in 0..3 {
        let runtime = DeterministicRuntime::new();
        let id = StackId::new();
        let path = || {
            let mut path = StackState::new();
            path.try_push(id.clone(), Route { value: 10 })
                .unwrap_or_else(|_| panic!("fresh stack accepts logical ID"));
            path
        };
        let store = Store::new(
            Containers {
                optional: Some(path()),
                case: Choice::Active(path()),
                rows: [(1, path()), (2, path())].into_iter().collect(),
            },
            container_reducer(),
            runtime.runtime(),
        );
        let mut work = Vec::new();
        let mut outputs = Vec::new();
        for container in 0..4 {
            let (send, receive) = mpsc::channel();
            work.push(
                drive(
                    &runtime,
                    store.send(container_action(
                        container,
                        id.clone(),
                        ChildAction::Capture(send),
                    )),
                )
                .unwrap(),
            );
            runtime.run_until_stalled().unwrap();
            outputs.push(receive.try_recv().unwrap());
        }
        let _ = drive(&runtime, store.send(ContainerAction::Remove(removed))).unwrap();
        runtime.run_until_stalled().unwrap();
        for (container, (task, output)) in work.iter().zip(&outputs).enumerate() {
            if container == usize::from(removed) {
                assert!(task.is_finished());
                assert!(
                    drive(&runtime, output.send(ChildAction::Tick))
                        .unwrap_err()
                        .is_cancelled()
                );
            } else {
                assert!(!task.is_finished());
                drive(&runtime, output.send(ChildAction::Tick)).unwrap();
            }
        }
        runtime.run_until_stalled().unwrap();
        let values = drive(
            &runtime,
            store.read(|state| {
                let first = |path: &StackState<Route>| path.values().next().unwrap().value;
                [
                    state.optional.as_ref().map(first),
                    match &state.case {
                        Choice::Active(path) => Some(first(path)),
                        Choice::Empty => None,
                    },
                    state.rows.get(&1).map(first),
                    state.rows.get(&2).map(first),
                ]
            }),
        )
        .unwrap();
        let expected = match removed {
            0 => [None, Some(11), Some(11), Some(11)],
            1 => [Some(11), None, Some(11), Some(11)],
            _ => [Some(11), Some(11), None, Some(11)],
        };
        assert_eq!(values, expected);
        drive(&runtime, store.shutdown()).unwrap();
    }
}

/// Lets the stack adapter own nested paths without adding parent-side behaviour.
struct Outer;
impl Reducer for Outer {
    type State = StackState<StackState<Route>>;
    type Action = StackAction<(), Action>;
    fn reduce(
        &mut self,
        _: &mut Self::State,
        _: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

// Actual stack-in-stack consumer: inner dismissal cannot pop its outer entry.
#[test]
fn nested_stack_dismisses_nearest_entry_and_outer_pop_retires_its_descendants() {
    let runtime = DeterministicRuntime::new();
    let mut inner = StackState::new();
    let inner_a = inner.push(Route { value: 1 });
    let inner_b = inner.push(Route { value: 10 });
    let mut outer = StackState::new();
    let outer_id = outer.push(inner);
    let outer_sibling = outer.push(StackState::new());
    let reducer = stack(
        Outer,
        StatePath::new(
            |state: &StackState<StackState<Route>>| state,
            |state: &mut StackState<StackState<Route>>| state,
        ),
        ActionPath::new(
            |action: &StackAction<(), Action>| Some(action),
            |action| action,
        ),
        |(): &(), _: &mut ReduceContext<'_>| StackState::new(),
        reducer(),
    );
    let store = Store::new(outer, reducer, runtime.runtime());
    let route = |id, action| StackAction::Element {
        id: outer_id.clone(),
        action: Action::Path(StackAction::Element { id, action }),
    };
    let captured = Arc::new(Mutex::new(None));
    let _ = drive(
        &runtime,
        store.send(route(
            inner_b.clone(),
            ChildAction::CaptureDismiss(Arc::clone(&captured)),
        )),
    )
    .unwrap();
    let dismiss = captured.lock().unwrap().take().unwrap();
    let task = drive(
        &runtime,
        store.send(route(inner_b, ChildAction::Replay(dismiss))),
    )
    .unwrap();
    drive(&runtime, task.finish());
    let lookup = outer_id.clone();
    assert_eq!(
        drive(
            &runtime,
            store.read(move |outer| (outer.len(), outer.get(&lookup).unwrap().len()))
        )
        .unwrap(),
        (2, 1)
    );
    let (send, receive) = mpsc::channel();
    let work = drive(
        &runtime,
        store.send(route(inner_a, ChildAction::Capture(send))),
    )
    .unwrap();
    runtime.run_until_stalled().unwrap();
    let old = receive.try_recv().unwrap();
    let _ = drive(&runtime, store.send(StackAction::PopFrom { id: outer_id })).unwrap();
    runtime.run_until_stalled().unwrap();
    assert!(work.is_finished());
    assert!(
        drive(&runtime, old.send(ChildAction::Tick))
            .unwrap_err()
            .is_cancelled()
    );
    // Inclusive outer removal also removes the following sibling, not a nested-only pop.
    assert_eq!(
        drive(
            &runtime,
            store.read(move |outer| (outer.len(), outer.get(&outer_sibling).is_some()))
        )
        .unwrap(),
        (0, false)
    );
    drive(&runtime, store.shutdown()).unwrap();
}

// Equal decoded values must not preserve old logical IDs or live authority.
#[test]
fn decoded_path_installation_retires_old_outputs_readers_and_timers() {
    use rca_core::EncodedStackRoute;
    let runtime = DeterministicRuntime::new();
    let mut state = StackState::new();
    let id = state.push(Route { value: 10 });
    let store = Store::new(state, reducer(), runtime.runtime());
    let old = scope(&runtime, &store, id.clone());
    let timer = drive(&runtime, old.send(ChildAction::Start)).unwrap();
    let mut reader = drive(&runtime, old.observe(|route| route.value)).unwrap();
    assert_eq!(drive(&runtime, reader.next()), Some(10));
    let (send, receive) = mpsc::channel();
    let work = drive(&runtime, old.send(ChildAction::Capture(send))).unwrap();
    runtime.run_until_stalled().unwrap();
    let output = receive.try_recv().unwrap();
    // Failure after a valid route leaves the live path and all authority intact.
    let _ = drive(
        &runtime,
        store.send(Action::Restore(StackEncoding {
            version: 1,
            routes: vec![
                EncodedStackRoute {
                    tag: "route".into(),
                    version: 1,
                    payload: vec![10],
                },
                EncodedStackRoute {
                    tag: "unknown".into(),
                    version: 1,
                    payload: vec![],
                },
            ],
        })),
    )
    .unwrap();
    assert_eq!(drive(&runtime, old.read(|route| route.value)).unwrap(), 10);
    assert!(!work.is_finished());
    assert!(!timer.is_finished());
    let _ = drive(
        &runtime,
        store.send(Action::Restore(StackEncoding {
            version: 1,
            routes: vec![EncodedStackRoute {
                tag: "route".into(),
                version: 1,
                payload: vec![10],
            }],
        })),
    )
    .unwrap();
    runtime.run_until_stalled().unwrap();
    assert!(work.is_finished());
    assert!(timer.is_finished());
    assert!(
        drive(&runtime, output.send(ChildAction::Tick))
            .unwrap_err()
            .is_cancelled()
    );
    assert!(
        drive(&runtime, old.read(|route| route.value))
            .unwrap_err()
            .is_scope_ended()
    );
    assert_eq!(drive(&runtime, reader.next()), None);
    let fresh = drive(
        &runtime,
        store.read(|path| path.ids().next().unwrap().clone()),
    )
    .unwrap();
    assert_ne!(fresh, id);
    let replacement = scope(&runtime, &store, fresh);
    let _ = drive(&runtime, replacement.send(ChildAction::Tick)).unwrap();
    assert_eq!(
        drive(&runtime, replacement.read(|route| route.value)).unwrap(),
        11
    );
    runtime.advance(Duration::from_secs(1)).unwrap();
    assert_eq!(
        drive(&runtime, replacement.read(|route| route.value)).unwrap(),
        11
    );
    drive(&runtime, store.shutdown()).unwrap();
}
