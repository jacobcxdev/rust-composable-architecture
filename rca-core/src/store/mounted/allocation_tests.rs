//! Isolated measurements of the actual private routed representations.
#[cfg(feature = "test-util")]
use super::SendInspection;
use super::{
    ActionTask, Apply, Arc, BorrowPath, ChildInput, Command, CommandIngress, CommitSender,
    DeferredQueue, Delivery, Feature, FeatureContext, Installed, InstalledNode, MountedApply,
    MountedCall, Mutex, OptionalPath, ProjectedRead, ProjectedUpdate, ReadInput, ReduceContext,
    RootDelivery, Shared, SharedCall, SharedEffects, SharedEndpoint, SharedInstalled, SharedNode,
    SharedReply, SharedRoute, TaskAccessError, TaskAccessReason, TaskProgress, TaskRequest,
    TimerKind, TypedSend, UpdateCall, VecDeque, Weak,
};
#[cfg(feature = "local")]
use super::{LocalCall, LocalInstalled, LocalNode, LocalRoute, local};
use crate::effect::CancellationIdentity;
use crate::effect::leaves::EffectLeaves;
use crate::effect::lineage::{ControlWake, LineageState};
use crate::effect::registry::{PreparedTask, TaskRegistry};
use crate::store::carriers::EffectLeaf;
use crate::store::command::Provenance;
use crate::store::scope::{ScopeIdentity, ScopeRegistry, ScopeSlot, ScopeTransitionBatch};
use crate::{DependencyValues, Effect, MonotonicInstant};
use std::cell::Cell;

/// Keeps the selected child independent of the measured routing representation.
struct Root {
    /// The optional state borrowed by real mounted calls.
    child: Option<usize>,
}
/// Declares zero, one or several effects to expose mounted work storage costs.
struct Child(Cell<usize>);
impl Feature for Child {
    type State = usize;
    type Action = usize;
    fn reduce(
        &mut self,
        state: &mut usize,
        action: &usize,
        context: &mut FeatureContext<'_, usize, usize>,
    ) {
        self.0.set(self.0.get() + 1);
        *state += 1;
        for _ in 0..*action {
            context.effect(Effect::send(0));
        }
    }
}
#[cfg(feature = "local")]
/// Exercises the Local representation with non-Send feature ownership.
struct LocalChild(std::rc::Rc<Cell<usize>>);
#[cfg(feature = "local")]
impl local::Feature for LocalChild {
    type State = usize;
    type Action = usize;
    fn reduce(
        &mut self,
        state: &mut usize,
        action: &usize,
        context: &mut local::FeatureContext<'_, usize, usize>,
    ) {
        self.0.set(self.0.get() + 1);
        *state += 1;
        for _ in 0..*action {
            context.effect(local::Effect::send(0));
        }
    }
}

/// Reports allocations and bytes added within one uninterrupted profiler session.
fn delta(before: &dhat::HeapStats, after: &dhat::HeapStats) -> (u64, u64) {
    (
        after.total_blocks - before.total_blocks,
        after.total_bytes - before.total_bytes,
    )
}

/// Measures actual routed registration after preparing ancestry, then checks complete release.
fn registration_cost<F: Future<Output = ()>>(
    depth: usize,
    scope: super::ScopeToken,
    register: impl FnOnce(&mut TaskRegistry, PreparedTask, Provenance) -> F,
) -> (u64, u64) {
    let initial = dhat::HeapStats::get();
    let (cost, clone_cost) = {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let lineage = LineageState::new(Arc::downgrade(&control));
        let generations: Vec<_> = (0..depth)
            .map(|id| registry.register_generation(CancellationIdentity::local(id).in_scope(scope)))
            .collect();
        let prepared = registry.prepare(scope, Arc::clone(&lineage), &generations);
        let provenance = Provenance::Effect {
            scope,
            lineage: Arc::clone(&lineage),
            task: prepared.sender_token(),
            generations: prepared.generation_tokens(),
            operations: prepared.operation_ancestry(),
        };

        // This isolates the removed expression's cost, not a copied registration
        // algorithm or a claim about end-to-end CPU time.
        let before = dhat::HeapStats::get();
        let copy = std::hint::black_box(provenance.clone());
        let clone_cost = delta(&before, &dhat::HeapStats::get());
        assert_eq!(clone_cost.0, u64::from(depth > 0));
        drop(copy);

        let before = dhat::HeapStats::get();
        let future = register(&mut registry, prepared, provenance);
        let cost = delta(&before, &dhat::HeapStats::get());
        lineage.finish_registration();
        drop(future);
        assert_eq!(registry.process_control().terminal.len(), 1);
        for generation in &generations {
            registry.release_generation(generation);
        }
        (cost, clone_cost)
    };
    let released = dhat::HeapStats::get();
    assert_eq!(released.curr_blocks, initial.curr_blocks);
    assert_eq!(released.curr_bytes, initial.curr_bytes);
    eprintln!("mounted provenance depth={depth}: removed clone expression={clone_cost:?}");
    cost
}

#[expect(
    clippy::too_many_lines,
    reason = "keep mounted storage, work and owner teardown measurements in one assay"
)]
/// Measures real mounted storage and work without changing their execution paths.
pub(crate) fn mounted_storage_and_work_costs() {
    const NAME: &str = "store::mounted::allocation_tests::mounted_storage_and_work_costs";
    if crate::allocation_test::run_in_child(NAME) {
        return;
    }
    let profiler = dhat::Profiler::builder().testing().build();
    let mut dependencies = DependencyValues::new();
    let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
    let mut transitions = ScopeTransitionBatch::default();
    let mut slot = ScopeSlot::new();
    let token = ReduceContext::new(
        &mut dependencies,
        MonotonicInstant::ZERO,
        &mut scopes,
        &mut transitions,
    )
    .mount(ReduceContext::node(&mut slot), ScopeIdentity::new(()));
    let path =
        OptionalPath::<Root, usize>::new(|root| root.child.as_ref(), |root| root.child.as_mut());
    let before = dhat::HeapStats::get();
    let mut installed: Vec<Box<dyn InstalledNode + Send>> = std::hint::black_box(Vec::new());
    let empty = dhat::HeapStats::get();
    assert_eq!(delta(&before, &empty), (0, 0));
    let before = dhat::HeapStats::get();
    let node = SharedNode::new(Child(Cell::new(0)), token, 0);
    let node_cost = delta(&before, &dhat::HeapStats::get());
    let before = dhat::HeapStats::get();
    assert_eq!(node.scope(), token);
    let first_lock_cost = delta(&before, &dhat::HeapStats::get());
    assert_eq!(
        first_lock_cost,
        (0, 0),
        "an actor-exclusive node guard needs no cold allocation"
    );
    let before = dhat::HeapStats::get();
    assert_eq!(node.scope(), token);
    let warm_lock_cost = delta(&before, &dhat::HeapStats::get());
    assert_eq!(warm_lock_cost, (0, 0));
    let route = SharedRoute::<Root, u8, Child> {
        node: Arc::downgrade(&node.owned),
        path,
        token,
        configuration: 0,
        ingress: Weak::new(),
        authority: scopes.authority(token),
    };
    let mut shared_registration = [0; 3];
    for depth in [0, 1, 4, 16] {
        let run: <SharedEffects as EffectLeaves<usize>>::Run =
            Box::new(|_| Box::pin(std::future::pending()));
        let future: <SharedEffects as EffectLeaves<usize>>::Future =
            Box::pin(std::future::pending());
        let leaves: [EffectLeaf<usize, SharedEffects>; 3] = [
            EffectLeaf::Send(0),
            EffectLeaf::Run(run),
            EffectLeaf::Future(future),
        ];
        let costs = leaves.map(|leaf| {
            registration_cost(depth, token, |registry, prepared, provenance| {
                route.register_leaf(registry, prepared, leaf, &Weak::new(), provenance)
            })
        });
        if depth == 0 {
            shared_registration = costs.map(|cost| cost.0);
        }
        assert_eq!(costs.map(|cost| cost.0), shared_registration);
        eprintln!("shared mounted send/run/future registration depth={depth}: {costs:?}");
    }
    let before = dhat::HeapStats::get();
    let endpoint = node
        .downgrade()
        .context::<Root, u8>(path, Weak::new(), &scopes);
    let endpoint_cost = delta(&before, &dhat::HeapStats::get());
    let before = dhat::HeapStats::get();
    let record: Box<dyn InstalledNode + Send> = Box::new(node);
    let record_cost = delta(&before, &dhat::HeapStats::get());
    let before = dhat::HeapStats::get();
    installed.push(record);
    let roster_cost = delta(&before, &dhat::HeapStats::get());
    let before = dhat::HeapStats::get();
    let (request, receive) = TypedSend::channel(7_usize);
    let typed_reply_cost = delta(&before, &dhat::HeapStats::get());
    let before = dhat::HeapStats::get();
    let input = ChildInput::typed(request);
    let typed_slot_cost = delta(&before, &dhat::HeapStats::get());
    assert_eq!(typed_slot_cost.0, u64::from(cfg!(feature = "test-util")));
    let before = dhat::HeapStats::get();
    let call = Box::new(SharedCall::new(route.clone(), input));
    let typed_call_cost = delta(&before, &dhat::HeapStats::get());
    assert_eq!(typed_call_cost.0, 1, "existing mounted call box only");
    #[cfg(feature = "test-util")]
    {
        let before = dhat::HeapStats::get();
        call.publish_input();
        let publication = delta(&before, &dhat::HeapStats::get());
        let before = dhat::HeapStats::get();
        let inspection = SendInspection::take(Arc::clone(call.input.slot().unwrap()));
        assert_eq!(std::hint::black_box(*inspection.action()), 7);
        drop(inspection);
        let inspect_restore = delta(&before, &dhat::HeapStats::get());
        assert_eq!((publication, inspect_restore), ((0, 0), (0, 0)));
        eprintln!(
            "shared weak publication={publication:?}; slot-only take/matcher/restore={inspect_restore:?}"
        );
    };
    let before = dhat::HeapStats::get();
    call.reject(TaskAccessError::new(
        TaskAccessReason::Cancelled,
        TaskProgress::NotStarted,
    ));
    let withdrawal_reply = delta(&before, &dhat::HeapStats::get());
    assert_eq!(withdrawal_reply, (0, 0));
    #[cfg(feature = "test-util")]
    assert!(route.node.upgrade().unwrap().pending.lock().is_none());
    drop(receive);
    #[cfg(not(feature = "test-util"))]
    assert_eq!(
        std::mem::size_of::<SharedInstalled<Child>>(),
        std::mem::size_of::<Mutex<Installed<Child>>>()
    );
    eprintln!(
        "mounted typed frontier test-util={}: reply blocks/bytes={typed_reply_cost:?}; slot={typed_slot_cost:?}; call={typed_call_cost:?}; installed wrapper/old inner bytes={}/{}; typed input/request bytes={}/{}",
        cfg!(feature = "test-util"),
        std::mem::size_of::<SharedInstalled<Child>>(),
        std::mem::size_of::<Mutex<Installed<Child>>>(),
        std::mem::size_of::<ChildInput<usize>>(),
        std::mem::size_of::<TypedSend<usize>>()
    );
    let mut costs = [(0, 0); 3];
    let mut command_costs = [(0, 0); 3];
    let mut root = Root { child: Some(0) };
    for (index, count) in [0, 1, 3].into_iter().enumerate() {
        let before = dhat::HeapStats::get();
        let call = Box::new(SharedCall::new(
            route.clone(),
            ChildInput::effect(count, CommitSender::disconnected()),
        ));
        let command_cost = delta(&before, &dhat::HeapStats::get());
        command_costs[index] = command_cost;
        assert_eq!(
            command_cost.0,
            1 + u64::from(cfg!(feature = "test-util")),
            "command box and test-util owned slot; shared ancestry clones without allocation"
        );
        let context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );
        let before = dhat::HeapStats::get();
        let applied = call.apply(&mut root, context, None);
        costs[index] = delta(&before, &dhat::HeapStats::get());
        assert!(matches!(applied, MountedApply::Applied { .. }));
        drop(applied);
    }
    let root_effects = VecDeque::from([Effect::<u8>::send(1), Effect::send(2), Effect::send(3)]);
    let before = dhat::HeapStats::get();
    let root_queue = <RootDelivery as Delivery<u8, SharedEffects, Root, u8, Shared>>::sequence(
        &RootDelivery,
        root_effects,
    );
    let root_queue_cost = delta(&before, &dhat::HeapStats::get());
    let effects = VecDeque::from([Effect::send(1), Effect::send(2), Effect::send(3)]);
    let before = dhat::HeapStats::get();
    let child_queue = route.sequence(effects);
    let child_queue_cost = delta(&before, &dhat::HeapStats::get());
    let before = dhat::HeapStats::get();
    let child_timer = route.timer(TimerKind::OneShot(1));
    let child_timer_cost = delta(&before, &dhat::HeapStats::get());
    let before = dhat::HeapStats::get();
    let root_timer = <RootDelivery as Delivery<u8, SharedEffects, Root, u8, Shared>>::timer(
        &RootDelivery,
        TimerKind::OneShot(1),
    );
    let root_timer_cost = delta(&before, &dhat::HeapStats::get());
    assert_eq!(root_queue_cost, (0, 0));
    assert_eq!(root_timer_cost, (0, 0));
    assert_eq!(
        child_queue_cost.0, 4,
        "three programme boxes and one heterogeneous queue allocation; ancestry is shared"
    );
    assert_eq!(child_timer_cost.0, 1, "timer box; ancestry is shared");

    #[cfg(feature = "local")]
    {
        let cache = std::rc::Rc::new(Cell::new(0));
        let before = dhat::HeapStats::get();
        let local_node = LocalNode::new(LocalChild(cache), token, 0);
        let local_node_cost = delta(&before, &dhat::HeapStats::get());
        #[cfg(not(feature = "test-util"))]
        assert_eq!(
            std::mem::size_of::<LocalInstalled<LocalChild>>(),
            std::mem::size_of::<std::cell::RefCell<Installed<LocalChild>>>()
        );
        eprintln!(
            "local installed wrapper/old inner bytes={}/{}",
            std::mem::size_of::<LocalInstalled<LocalChild>>(),
            std::mem::size_of::<std::cell::RefCell<Installed<LocalChild>>>()
        );
        let local_route = LocalRoute::<Root, u8, LocalChild> {
            node: std::rc::Rc::downgrade(&local_node.owned),
            path,
            token,
            configuration: 0,
            ingress: Weak::new(),
            authority: scopes.authority(token),
        };
        for depth in [0, 1, 4, 16] {
            let run: <local::LocalEffects as EffectLeaves<usize>>::Run =
                Box::new(|_| Box::pin(std::future::pending()));
            let future: <local::LocalEffects as EffectLeaves<usize>>::Future =
                Box::pin(std::future::pending());
            let leaves: [EffectLeaf<usize, local::LocalEffects>; 3] = [
                EffectLeaf::Send(0),
                EffectLeaf::Run(run),
                EffectLeaf::Future(future),
            ];
            let costs = leaves.map(|leaf| {
                registration_cost(depth, token, |registry, prepared, provenance| {
                    local_route.register_leaf(registry, prepared, leaf, &Weak::new(), provenance)
                })
            });
            assert_eq!(costs.map(|cost| cost.0), shared_registration);
            eprintln!("local mounted send/run/future registration depth={depth}: {costs:?}");
        }
        let before = dhat::HeapStats::get();
        let local_endpoint = local_node.context::<Root, u8>(
            path,
            Weak::new(),
            Provenance::External { handle: None },
            scopes.authority(token),
        );
        let local_endpoint_cost = delta(&before, &dhat::HeapStats::get());
        let (request, receive) = TypedSend::channel(7_usize);
        let before = dhat::HeapStats::get();
        let input = ChildInput::typed(request);
        let slot_cost = delta(&before, &dhat::HeapStats::get());
        assert_eq!(slot_cost.0, u64::from(cfg!(feature = "test-util")));
        let before = dhat::HeapStats::get();
        let call = Box::new(LocalCall::new(local_route.clone(), input));
        let call_cost = delta(&before, &dhat::HeapStats::get());
        assert_eq!(call_cost.0, 1);
        #[cfg(feature = "test-util")]
        {
            let before = dhat::HeapStats::get();
            call.publish_input();
            let publication = delta(&before, &dhat::HeapStats::get());
            let before = dhat::HeapStats::get();
            let inspection = SendInspection::take(Arc::clone(call.input.slot().unwrap()));
            assert_eq!(std::hint::black_box(*inspection.action()), 7);
            drop(inspection);
            let inspect_restore = delta(&before, &dhat::HeapStats::get());
            assert_eq!((publication, inspect_restore), ((0, 0), (0, 0)));
            eprintln!(
                "local weak publication={publication:?}; slot-only take/matcher/restore={inspect_restore:?}"
            );
        };
        let before = dhat::HeapStats::get();
        call.reject(TaskAccessError::new(
            TaskAccessReason::Cancelled,
            TaskProgress::NotStarted,
        ));
        let withdrawal_reply = delta(&before, &dhat::HeapStats::get());
        assert_eq!(withdrawal_reply, (0, 0));
        #[cfg(feature = "test-util")]
        assert!(local_node.owned.pending.borrow().is_none());
        eprintln!(
            "local typed frontier slot={slot_cost:?}; call={call_cost:?}; withdrawal/reply={withdrawal_reply:?}"
        );
        drop(receive);
        let mut local_costs = [(0, 0); 3];
        for (index, count) in [0, 1, 3].into_iter().enumerate() {
            let call = Box::new(LocalCall::new(
                local_route.clone(),
                ChildInput::effect(count, CommitSender::disconnected()),
            ));
            let context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut transitions,
            );
            let before = dhat::HeapStats::get();
            let applied = call.apply(&mut root, context, None);
            local_costs[index] = delta(&before, &dhat::HeapStats::get());
            drop(applied);
        }
        assert_eq!(
            [local_costs[0].0, local_costs[1].0, local_costs[2].0],
            [1, 2, 3]
        );
        eprintln!(
            "local allocation blocks/bytes: Rc node={local_node_cost:?}, endpoint={local_endpoint_cost:?}, apply zero/one/many={local_costs:?}"
        );
        drop((local_endpoint, local_node));
    };
    eprintln!(
        "mounted allocation blocks/bytes: node={node_cost:?}, endpoint={endpoint_cost:?}, record={record_cost:?}, first roster={roster_cost:?}, command={command_costs:?}, apply zero/one/many={costs:?}"
    );
    eprintln!(
        "mounted layout bytes: empty roster={}, record pointer={}, root command={}, root queue={}, routed queue={}",
        std::mem::size_of_val(&installed),
        std::mem::size_of::<Box<dyn InstalledNode + Send>>(),
        std::mem::size_of::<Command<Root, u8>>(),
        std::mem::size_of::<VecDeque<Effect<u8>>>(),
        std::mem::size_of::<DeferredQueue<Root, u8, Shared>>()
    );
    eprintln!(
        "deferred allocation blocks/bytes: root queue={root_queue_cost:?}, child three-item queue={child_queue_cost:?}, root timer={root_timer_cost:?}, child timer={child_timer_cost:?}"
    );
    eprintln!(
        "shared node guard control blocks/bytes: first={first_lock_cost:?}, warm={warm_lock_cost:?}; Installed<Child> bytes={}, bare Child bytes={}, Option<Child> bytes={}",
        std::mem::size_of::<Installed<Child>>(),
        std::mem::size_of::<Child>(),
        std::mem::size_of::<Option<Child>>()
    );
    assert_eq!(
        costs[0].0, 1,
        "empty child work adds no box; only existing erased reply allocation"
    );
    assert_eq!(
        costs[1].0, 2,
        "one child effect adds one work box to its reply"
    );
    assert_eq!(
        costs[2].0, 3,
        "many child effects additionally allocate Work spill storage"
    );
    drop((root_queue, child_queue, root_timer, child_timer));
    drop(endpoint);
    drop(installed);
    drop(profiler);
}

pub(crate) use route_cost::route_and_roster_costs;

/// Separates route shape costs from the surrounding actor and application state.
mod route_cost {
    use super::{
        ActionTask, Apply, Arc, BorrowPath, Cell, Command, CommandIngress, Feature, FeatureContext,
        OptionalPath, ProjectedRead, ProjectedUpdate, ReadInput, ScopeIdentity, ScopeRegistry,
        ScopeSlot, SharedCall, SharedEndpoint, SharedNode, SharedReply, SharedRoute, TaskRequest,
        UpdateCall, delta,
    };
    use crate::store::mounted::erased_path::SharedPath;
    use crate::store::mounted::path::ComposedPath;
    use crate::store::task_context::CommitReply;
    use std::mem::{size_of, size_of_val};
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    /// Returns the actual operation result alongside its allocation delta.
    fn measure<T>(run: impl FnOnce() -> T) -> (T, (u64, u64)) {
        let before = dhat::HeapStats::get();
        let value = run();
        (value, delta(&before, &dhat::HeapStats::get()))
    }
    /// Samples one future step without installing an executor or wake recorder.
    fn poll<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }
    /// Counts actual selector calls through a preconstructed nested state tree.
    struct Tree {
        /// The next projection level, absent at the measured leaf.
        child: Option<Box<Tree>>,
        /// Supplies read and update results independent of allocation counts.
        value: usize,
        /// Counts immutable selector entry across all levels.
        reads: Arc<AtomicUsize>,
        /// Counts mutable selector entry across all levels.
        writes: Arc<AtomicUsize>,
    }
    /// Builds application state before the route-operation measurement intervals.
    fn tree(depth: usize, reads: &Arc<AtomicUsize>, writes: &Arc<AtomicUsize>) -> Tree {
        Tree {
            child: (depth > 1).then(|| Box::new(tree(depth - 1, reads, writes))),
            value: 7,
            reads: std::sync::Arc::clone(reads),
            writes: std::sync::Arc::clone(writes),
        }
    }
    /// Counts one intermediate selector without allocating route metadata.
    fn step() -> OptionalPath<Tree, Tree> {
        OptionalPath::new(
            |state: &Tree| {
                state.reads.fetch_add(1, Ordering::Relaxed);
                state.child.as_deref()
            },
            |state: &mut Tree| {
                state.writes.fetch_add(1, Ordering::Relaxed);
                state.child.as_deref_mut()
            },
        )
    }
    /// Counts the final selector separately from intermediate path steps.
    fn leaf() -> OptionalPath<Tree, usize> {
        OptionalPath::new(
            |state: &Tree| {
                state.reads.fetch_add(1, Ordering::Relaxed);
                Some(&state.value)
            },
            |state: &mut Tree| {
                state.writes.fetch_add(1, Ordering::Relaxed);
                Some(&mut state.value)
            },
        )
    }
    /// Traverses the chosen depth within one non-preemptive selector call.
    fn direct<const D: usize>() -> OptionalPath<Tree, usize> {
        OptionalPath::new(
            |mut state: &Tree| {
                state.reads.fetch_add(1, Ordering::Relaxed);
                for _ in 1..D {
                    state = state.child.as_deref()?;
                }
                Some(&state.value)
            },
            |mut state: &mut Tree| {
                state.writes.fetch_add(1, Ordering::Relaxed);
                for _ in 1..D {
                    state = state.child.as_deref_mut()?;
                }
                Some(&mut state.value)
            },
        )
    }
    // One token per intermediate projection; leaf makes total depth tokens + 1.
    macro_rules! composed {
        () => { leaf() };
        (x $($rest:tt)*) => { ComposedPath::new(step(), composed!($($rest)*)) };
    }
    /// Builds positive-depth ancestry with one shared owner per composed level.
    fn erased_parent(depth: usize) -> SharedPath<Tree, Tree> {
        if depth == 1 {
            SharedPath::new(step())
        } else {
            SharedPath::new(ComposedPath::new(erased_parent(depth - 1), step()))
        }
    }
    /// Supplies the real endpoint's child domain without unrelated reducer work.
    struct RouteFeature;
    impl Feature for RouteFeature {
        type State = usize;
        type Action = u8;
        fn reduce(&mut self, _: &mut usize, _: &u8, _: &mut FeatureContext<'_, usize, u8>) {}
    }

    #[expect(
        clippy::too_many_lines,
        reason = "keep route construction, selection and teardown measurements in one assay"
    )]
    /// Measures construction and cold/warm read-update stages for one route shape.
    fn route<P>(label: &str, depth: usize, selectors: usize, make: impl FnOnce() -> P)
    where
        P: BorrowPath<Tree, State = usize> + Send + Sync + 'static,
    {
        let (path, path_construction) = measure(make);
        let reads = Arc::new(AtomicUsize::new(0));
        let writes = Arc::new(AtomicUsize::new(0));
        let mut state = tree(depth, &reads, &writes);
        let (lifecycle, mut scopes) = ScopeRegistry::for_test();
        // Readiness is route-assay setup, outside every measured interval.
        lifecycle.mark_ready();
        let mut token = scopes.root();
        for _ in 0..depth {
            let node = ScopeRegistry::node(&mut ScopeSlot::new());
            token = scopes.mount(token, node, ScopeIdentity::new(()));
        }
        let (sender, receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
        let ingress = Arc::new(CommandIngress::<Command<Tree, ()>>::new(
            sender,
            crate::effect::lineage::ControlWake::new(),
        ));
        let node = SharedNode::new(RouteFeature, token, 0);
        let (authority, authority_cost) = measure(|| scopes.authority(token));
        let route = SharedRoute::<Tree, (), RouteFeature, P> {
            node: Arc::downgrade(&node.owned),
            path: path.clone(),
            token,
            configuration: 0,
            ingress: Arc::downgrade(&ingress),
            authority,
        };
        let (_, path_clone) = measure(|| std::hint::black_box(path.clone()));
        let (_, route_clone) = measure(|| std::hint::black_box(route.clone()));
        let weak_node = node.downgrade();
        let (context, endpoint) = measure(|| {
            weak_node.context::<Tree, ()>(path.clone(), Arc::downgrade(&ingress), &scopes)
        });
        assert_eq!(
            reads.load(Ordering::Relaxed),
            0,
            "route acquisition does not select State"
        );

        let live_checks = Cell::new(0);
        let (selected, structural) = measure(|| {
            path.read_structural(&state, &|| {
                live_checks.set(live_checks.get() + 1);
                true
            })
        });
        assert_eq!(selected, Some(&7));
        assert_eq!(reads.swap(0, Ordering::Relaxed), selectors);
        assert_eq!(live_checks.get(), selectors - 1);

        let mut samples = [([(0, 0); 4], [(0, 0); 4]); 2];
        let mut read_bytes = 0;
        let mut update_bytes = 0;
        for (index, sample) in samples.iter_mut().enumerate() {
            let (future, creation) = measure(|| context.read(|value| *value));
            let mut future = std::pin::pin!(future);
            let (pending, admission) = measure(|| poll(future.as_mut()));
            assert!(pending.is_pending());
            assert_eq!(reads.load(Ordering::Relaxed), 0);
            let Command::Task {
                request: TaskRequest::Read(call),
                authority,
                ..
            } = receiver.try_recv().unwrap()
            else {
                panic!("read projection");
            };
            assert!(scopes.accepts(&authority));
            read_bytes = size_of_val(&*call);
            let ((), execution) = measure(|| call.execute(&state));
            assert_eq!(reads.swap(0, Ordering::Relaxed), selectors);
            let (result, completion) = measure(|| poll(future.as_mut()));
            assert!(matches!(result, Poll::Ready(Ok(value)) if value == 7 + index));
            sample.0 = [creation, admission, execution, completion];

            let (future, creation) = measure(|| {
                context.update(|value| {
                    *value += 1;
                    *value
                })
            });
            let mut future = std::pin::pin!(future);
            let (pending, admission) = measure(|| poll(future.as_mut()));
            assert!(pending.is_pending());
            assert_eq!(writes.load(Ordering::Relaxed), 0);
            let Command::Task {
                request: TaskRequest::Update(call),
                authority,
                ..
            } = receiver.try_recv().unwrap()
            else {
                panic!("update projection");
            };
            assert!(scopes.accepts(&authority));
            update_bytes = size_of_val(&*call);
            let (applied, execution) = measure(|| call.apply(&mut state));
            assert_eq!(writes.swap(0, Ordering::Relaxed), selectors);
            let Apply::Applied(mut reply) = applied else {
                panic!("selected update");
            };
            // Leaf assay, not a commit: complete the actual reply solely to drain it.
            reply.mark_committed(1);
            reply.complete(ActionTask::completed());
            let (result, completion) = measure(|| poll(future.as_mut()));
            assert!(matches!(
                result,
                Poll::Ready(Ok(value)) if *value.value() == 8 + index && value.revision() == 1
            ));
            sample.1 = [creation, admission, execution, completion];
        }
        eprintln!(
            "route {label} depth={depth} selectors/read/update={selectors}: path construction={path_construction:?}; authority control={authority_cost:?}; path clone={path_clone:?}; route clone={route_clone:?}; actual weak-node context including authority={endpoint:?}; structural={structural:?}, live checks={}; cold/warm (read[create/admit/execute/finish], update[same])={samples:?}",
            live_checks.get()
        );
        eprintln!(
            "route {label} depth={depth} exact inline bytes: path={}, SharedRoute={}, SharedEndpoint={}, SharedCall={}, ProjectedRead={}, ProjectedUpdate={}, actual erased read/update={read_bytes}/{update_bytes}",
            size_of::<P>(),
            size_of::<SharedRoute<Tree, (), RouteFeature, P>>(),
            size_of::<SharedEndpoint<Tree, (), RouteFeature, P>>(),
            size_of::<SharedCall<Tree, (), RouteFeature, P>>(),
            size_of::<ProjectedRead<P, dyn ReadInput<usize> + Send>>(),
            size_of::<ProjectedUpdate<P, dyn UpdateCall<usize, Reply = SharedReply> + Send>>()
        );
    }

    /// Runs route shapes and existing roster assays under one isolated profiler.
    pub(crate) fn route_and_roster_costs() {
        const NAME: &str = "store::mounted::allocation_tests::route_and_roster_costs";
        if crate::allocation_test::run_in_child(NAME) {
            return;
        }
        let profiler = dhat::Profiler::builder().testing().build();
        route("direct", 1, 1, direct::<1>);
        route("composed-degenerate", 1, 1, || composed!());
        route("direct", 5, 1, direct::<5>);
        route("composed", 5, 5, || composed!(x x x x));
        route("direct", 20, 1, direct::<20>);
        route(
            "composed",
            20,
            20,
            || composed!(x x x x x x x x x x x x x x x x x x x),
        );
        route("shared-parent", 5, 5, || {
            ComposedPath::new(erased_parent(4), leaf())
        });
        route("shared-parent", 20, 20, || {
            ComposedPath::new(erased_parent(19), leaf())
        });
        for siblings in [1, 64] {
            let (parent, owner) = measure(|| SharedPath::new(step()));
            let mut paths = Vec::with_capacity(siblings);
            let ((), clones) = measure(|| {
                for _ in 0..siblings {
                    paths.push(ComposedPath::new(parent.clone(), leaf()));
                }
            });
            let ((), sibling_drop) = measure(|| drop(paths));
            let ((), last_owner_drop) = measure(|| drop(parent));
            assert_eq!(owner.0, 1);
            assert_eq!(clones, (0, 0));
            eprintln!(
                "shared parent metadata siblings={siblings}: owner={owner:?}, preallocated sibling routes={clones:?}, sibling drop={sibling_drop:?}, last owner drop={last_owner_drop:?}"
            );
            #[cfg(feature = "local")]
            {
                use crate::store::mounted::erased_path::LocalPath;
                let (parent, owner) = measure(|| LocalPath::new(step()));
                let mut paths = Vec::with_capacity(siblings);
                let ((), clones) = measure(|| {
                    for _ in 0..siblings {
                        paths.push(ComposedPath::new(parent.clone(), leaf()));
                    }
                });
                let ((), sibling_drop) = measure(|| drop(paths));
                let ((), last_owner_drop) = measure(|| drop(parent));
                assert_eq!(owner.0, 1);
                assert_eq!(clones, (0, 0));
                eprintln!(
                    "local parent metadata siblings={siblings}: owner={owner:?}, preallocated sibling routes={clones:?}, sibling drop={sibling_drop:?}, last owner drop={last_owner_drop:?}"
                );
            }
        }
        crate::store::mount::allocation_tests::nested_roster_costs();
        crate::store::mount::children_costs::run();
        drop(profiler);
    }
}
