//! Scoped admission and publication costs through required and identified child views.
//!
//! Move-only inputs prove deferred embedding. Numeric and heap-backed identities
//! distinguish observation growth from identity-copying costs.

#![cfg(feature = "allocation-measurement")]

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(dead_code, reason = "this allocation test manually polls the actor")]
mod support;

use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use rca_core::{
    ActionPath, Effect, OptionalStatePath, ReduceContext, Reducer, RuntimeFuture, ScopedStore,
    StatePath, Store,
};

#[global_allocator]
/// Tracks the single scoped assay in a process isolated from imported controls.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Separates stable child identity from its accumulated action total.
struct ChildState {
    id: u64,
    total: u64,
}

/// Holds independent required and optional children for the two admission controls.
struct RootState {
    required: ChildState,
    optional: Option<ChildState>,
}

#[derive(Debug)]
/// Carries a move-only value without requiring `Sync` from child inputs.
struct ChildAction(Cell<u64>);

/// Routes each child input to its corresponding stored state.
enum RootAction {
    Required(ChildAction),
    Optional(ChildAction),
}

/// Accumulates only committed child inputs, leaving identities unchanged.
struct Counter;

impl Reducer for Counter {
    type State = RootState;
    type Action = RootAction;

    fn reduce(
        &mut self,
        state: &mut RootState,
        action: &RootAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<RootAction> {
        match action {
            RootAction::Required(action) => state.required.total += action.0.get(),
            RootAction::Optional(action) => {
                state.optional.as_mut().expect("present child").total += action.0.get();
            }
        }
        Effect::none()
    }
}

/// Measures one actor poll, excluding admission and receipt destruction.
fn actor_commit_cost<State, Action>(
    store: &Store<State, Action>,
    actor: &mut RuntimeFuture,
    context: &mut Context<'_>,
    action: Action,
) -> (u64, u64)
where
    State: Send + 'static,
    Action: Send + 'static,
{
    // Admission and receipt destruction stay outside the actor-only window.
    let receipt = store.try_send(action).unwrap();
    let before = dhat::HeapStats::get();
    assert!(actor.as_mut().poll(context).is_pending());
    let after = dhat::HeapStats::get();
    let task = futures_executor::block_on(receipt.finish()).expect("no-op commits");
    futures_executor::block_on(task.finish());
    (
        after.total_blocks - before.total_blocks,
        after.total_bytes - before.total_bytes,
    )
}

/// Compares warmed publication with zero, one and 32 real child observations.
fn identified_publication_costs<State, Action, Child, Input>(
    store: &Store<State, Action>,
    child: &ScopedStore<Child, Input>,
    actor: &mut RuntimeFuture,
    context: &mut Context<'_>,
    action: impl Fn() -> Action,
) -> [(u64, u64, usize); 3]
where
    State: Send + 'static,
    Action: Send + 'static,
    Child: Send + 'static,
    Input: Send + 'static,
{
    let projections = Arc::new(AtomicUsize::new(0));
    let mut observations = Vec::with_capacity(32);
    let mut costs = [(0, 0, 0); 3];
    for (index, count) in [0, 1, 32].into_iter().enumerate() {
        while observations.len() < count {
            let before = projections.load(Ordering::Acquire);
            let projected = Arc::clone(&projections);
            let registration = child.observe(move |_| {
                projected.fetch_add(1, Ordering::AcqRel);
            });
            let mut registration = std::pin::pin!(registration);
            assert!(registration.as_mut().poll(context).is_pending());
            assert!(actor.as_mut().poll(context).is_pending());
            let Poll::Ready(Ok(mut values)) = registration.as_mut().poll(context) else {
                panic!("actor must register the scoped observation");
            };
            assert_eq!(projections.load(Ordering::Acquire) - before, 1);
            assert_eq!(futures_executor::block_on(values.next()), Some(()));
            observations.push(values);
        }

        // Warm the publication slots before measuring their replacement. Unit
        // values are ZST, so erased result storage cannot hide identity costs.
        let before = projections.load(Ordering::Acquire);
        let _ = actor_commit_cost(store, actor, context, action());
        assert_eq!(projections.load(Ordering::Acquire) - before, count);
        let before = projections.load(Ordering::Acquire);
        let (blocks, bytes) = actor_commit_cost(store, actor, context, action());
        let projected = projections.load(Ordering::Acquire) - before;
        assert_eq!(projected, count);
        costs[index] = (blocks, bytes, projected);

        for values in &mut observations {
            let mut next = std::pin::pin!(values.next());
            assert!(next.as_mut().poll(context).is_pending());
            assert!(actor.as_mut().poll(context).is_pending());
            assert_eq!(next.as_mut().poll(context), Poll::Ready(Some(())));
        }
    }
    costs
}

/// Counts copies of a heap-backed identity independently of allocator statistics.
struct CountedIdentity {
    value: String,
    clones: Arc<AtomicUsize>,
}

impl Clone for CountedIdentity {
    fn clone(&self) -> Self {
        self.clones.fetch_add(1, Ordering::AcqRel);
        Self {
            value: self.value.clone(),
            clones: Arc::clone(&self.clones),
        }
    }
}

impl PartialEq for CountedIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for CountedIdentity {}

impl Hash for CountedIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

/// Runs identical publication controls with a real heap-backed scoped identity.
fn string_identity_costs() -> ([(u64, u64, usize); 3], usize) {
    let runtime = support::ManualRuntime::new();
    let clones = Arc::new(AtomicUsize::new(0));
    let value = String::from("heap-backed scoped identity");
    let expected_len = value.len();
    let reducer = rca_core::Reduce::new(|_: &mut CountedIdentity, &(): &(), _| Effect::none());
    let store = Store::new(
        CountedIdentity {
            value,
            clones: Arc::clone(&clones),
        },
        reducer,
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    let child = {
        let registration = store.scope_identified(
            StatePath::new(
                |state: &CountedIdentity| state,
                |state: &mut CountedIdentity| state,
            ),
            rca_core::identity_action_path(),
            |identity| identity,
        );
        let mut registration = std::pin::pin!(registration);
        assert!(registration.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(Ok(child)) = registration.as_mut().poll(&mut context) else {
            panic!("actor must register the String identity");
        };
        child
    };
    assert_eq!(clones.load(Ordering::Acquire), 1);
    let read_result = {
        let mut read = std::pin::pin!(child.read(|child| child.value.len()));
        assert!(read.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        read.as_mut().poll(&mut context)
    };
    assert!(matches!(read_result, Poll::Ready(Ok(len)) if len == expected_len));
    let costs = identified_publication_costs(&store, &child, &mut actor, &mut context, || ());
    (costs, clones.load(Ordering::Acquire))
}

/// Checks that scoped admission reuses its embedding without per-send closures.
#[expect(
    clippy::too_many_lines,
    reason = "one profiler session keeps construction, admission and semantic checks in phase order"
)]
fn scoped_admission_reuses_embedding_without_allocating_a_per_send_closure() {
    if allocation_test::run_in_child(
        "scoped_admission_reuses_embedding_without_allocating_a_per_send_closure",
    ) {
        return;
    }

    let runtime = support::ManualRuntime::new();
    let store = Store::builder(
        RootState {
            required: ChildState { id: 1, total: 0 },
            optional: Some(ChildState { id: 2, total: 0 }),
        },
        Counter,
        runtime.runtime(),
    )
    .mailbox_capacity(1)
    .build()
    .expect("valid store");
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());

    let embeddings = Arc::new(AtomicUsize::new(0));
    // Track scope construction and later admission in one process-wide session.
    // Store/runtime allocations made before this point are not retained totals.
    let profiler = dhat::Profiler::builder().testing().build();
    let required_before = dhat::HeapStats::get();
    let required_embeddings = Arc::clone(&embeddings);
    let required = store.scope(
        StatePath::new(
            |state: &RootState| &state.required,
            |state: &mut RootState| &mut state.required,
        ),
        ActionPath::new(
            |action: &RootAction| match action {
                RootAction::Required(child) => Some(child),
                RootAction::Optional(_) => None,
            },
            move |action| {
                required_embeddings.fetch_add(1, Ordering::AcqRel);
                RootAction::Required(action)
            },
        ),
    );
    let required_after = dhat::HeapStats::get();
    let optional_before = dhat::HeapStats::get();
    let optional_embeddings = Arc::clone(&embeddings);
    let optional = {
        let registration = store.scope_some_identified(
            OptionalStatePath::new(
                |state: &RootState| state.optional.as_ref(),
                |state: &mut RootState| state.optional.as_mut(),
            ),
            ActionPath::new(
                |action: &RootAction| match action {
                    RootAction::Optional(child) => Some(child),
                    RootAction::Required(_) => None,
                },
                move |action| {
                    optional_embeddings.fetch_add(1, Ordering::AcqRel);
                    RootAction::Optional(action)
                },
            ),
            |state: &ChildState| &state.id,
        );
        let mut registration = std::pin::pin!(registration);
        assert!(registration.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(child) = registration.as_mut().poll(&mut context) else {
            panic!("actor must acknowledge scope registration");
        };
        child.expect("present identified child")
    };
    let optional_after = dhat::HeapStats::get();

    for child in [&required, &optional] {
        let receipt = child.try_send(ChildAction(Cell::new(0))).unwrap();
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let task = futures_executor::block_on(receipt.finish()).expect("warm-up commit");
        futures_executor::block_on(task.finish());
    }
    assert_eq!(embeddings.load(Ordering::Acquire), 2);
    let warmed_idle = dhat::HeapStats::get();

    // Isolate bounded admission from actor work after warming both scopes.
    let mut admission_blocks = [0; 2];
    let mut admission_bytes = [0; 2];
    for (index, child) in [&required, &optional].into_iter().enumerate() {
        let before_embeddings = embeddings.load(Ordering::Acquire);
        let before = dhat::HeapStats::get();
        let receipt = child.try_send(ChildAction(Cell::new(7))).unwrap();
        let admitted = dhat::HeapStats::get();
        admission_blocks[index] = admitted.total_blocks - before.total_blocks;
        admission_bytes[index] = admitted.total_bytes - before.total_bytes;
        assert_eq!(embeddings.load(Ordering::Acquire), before_embeddings);

        let full = child
            .try_send(ChildAction(Cell::new(11)))
            .expect_err("second action must be returned when the mailbox is full");
        assert!(full.is_full());
        let recovered = full.into_action();
        assert_eq!(recovered.0.get(), 11);
        assert_eq!(embeddings.load(Ordering::Acquire), before_embeddings);

        assert!(actor.as_mut().poll(&mut context).is_pending());
        let task = futures_executor::block_on(receipt.finish()).expect("measured action commits");
        futures_executor::block_on(task.finish());
        assert_eq!(embeddings.load(Ordering::Acquire), before_embeddings + 1);

        let receipt = child.try_send(recovered).expect("recovered action fits");
        assert_eq!(embeddings.load(Ordering::Acquire), before_embeddings + 1);
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let task = futures_executor::block_on(receipt.finish()).expect("recovered action commits");
        futures_executor::block_on(task.finish());
        assert_eq!(embeddings.load(Ordering::Acquire), before_embeddings + 2);
    }
    let completed_idle = dhat::HeapStats::get();
    let publication_costs =
        identified_publication_costs(&store, &optional, &mut actor, &mut context, || {
            RootAction::Optional(ChildAction(Cell::new(0)))
        });
    let (string_costs, string_clones) = string_identity_costs();
    drop(profiler);

    let shutdown = store.shutdown_into_state();
    let mut shutdown = std::pin::pin!(shutdown);
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    let Poll::Ready(state) = shutdown.as_mut().poll(&mut context) else {
        panic!("actor must return final state");
    };
    let state = state.expect("clean shutdown");
    assert_eq!(state.required.total, 18);
    assert_eq!(state.optional.expect("child remains present").total, 18);
    assert_eq!(embeddings.load(Ordering::Acquire), 6);

    // Bytes are diagnostic only: do not freeze allocator or private layouts.
    // Identified construction includes actor registration and mailbox waiting.
    for (name, before, after) in [
        ("required", required_before, required_after),
        ("identified_optional", optional_before, optional_after),
    ] {
        eprintln!(
            "scoped construction {name}: blocks={} bytes={} retained_blocks_delta={} retained_bytes_delta={}",
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
            after.curr_blocks as i128 - before.curr_blocks as i128,
            after.curr_bytes as i128 - before.curr_bytes as i128,
        );
    }
    for (name, stats) in [("warmed", warmed_idle), ("completed", completed_idle)] {
        eprintln!(
            "scoped tracked idle {name}: blocks={} bytes={}",
            stats.curr_blocks, stats.curr_bytes,
        );
    }
    eprintln!(
        "scoped admission: required_blocks={} required_bytes={} identified_optional_blocks={} identified_optional_bytes={}",
        admission_blocks[0], admission_bytes[0], admission_blocks[1], admission_bytes[1],
    );
    for (identity, costs) in [("u64", publication_costs), ("String", string_costs)] {
        let base_blocks = costs[0].0;
        assert!(
            costs.iter().all(|(blocks, _, _)| *blocks == base_blocks),
            "warmed {identity} observations must not add per-publication allocations: {costs:?}",
        );
    }
    for (count, (blocks, bytes, projected)) in [0, 1, 32].into_iter().zip(publication_costs) {
        eprintln!(
            "scoped publication: observations={count} actor_blocks={blocks} actor_bytes={bytes} projections={projected}",
        );
    }
    for (count, (blocks, bytes, projected)) in [0, 1, 32].into_iter().zip(string_costs) {
        eprintln!(
            "String scoped publication: observations={count} actor_blocks={blocks} actor_bytes={bytes} projections={projected}",
        );
    }
    eprintln!("String scoped identity clones: {string_clones}");
    assert_eq!(string_clones, 1, "only acquisition needs owned identity");

    // One commit reply and one non-zero-sized, move-only child action.
    assert!(
        admission_blocks.into_iter().all(|blocks| blocks <= 2),
        "scoped admission allocated required={} and identified optional={} blocks; budget is 2 each",
        admission_blocks[0],
        admission_blocks[1],
    );
}

/// Preserves the exact names of the isolated allocation cases.
const CASES: &[(&str, fn())] = &[(
    "scoped_admission_reuses_embedding_without_allocating_a_per_send_closure",
    scoped_admission_reuses_embedding_without_allocating_a_per_send_closure,
)];

include!("support/allocation_main.rs");
