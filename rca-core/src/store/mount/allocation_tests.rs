//! Phase-local measurements; this is not an alternative transaction kernel.

use super::{
    AcquireCall, Acquisition, Arc, Batch, Carriers, Command, Declaration, DeclarationCell,
    DeclarationOwner, Descriptor, Feature, Lifecycle, MountConstruction, MountDeclaration,
    MountIdentity, MountSite, Mutex, OnceLock, OptionalMount, Pin, ReactionChange, ReduceContext,
    ReplyGuard, RootDeclaration, RootInitial, RootMount, ScopeNodeId, ScopeRegistry, ScopeSlot,
    ScopeToken, ScopeTransitionBatch, Shared, Task, TaskAccessError, TaskContext, TypedAcquire,
    Weak, attach, attach_descriptor, nested, reaction,
};
use crate::effect::lineage::ControlWake;
use crate::store::ingress::CommandIngress;
use crate::store::scope::ScopeAuthority;
use crate::{DependencyValues, MonotonicInstant};
use std::mem::{size_of, size_of_val};
use std::num::NonZeroUsize;
use std::task::{Context, Poll, Waker};

/// Counts blocks and requested bytes allocated while the operation runs.
///
/// Callers run under the isolated DHAT profiler. Returned values remain alive
/// after the sample, so their later disposal is outside this measurement.
pub(super) fn measure<T>(operation: impl FnOnce() -> T) -> (T, (u64, u64)) {
    let before = dhat::HeapStats::get();
    let value = operation();
    let after = dhat::HeapStats::get();
    (
        value,
        (
            after.total_blocks - before.total_blocks,
            after.total_bytes - before.total_bytes,
        ),
    )
}

/// Polls one manually scheduled step without measuring executor wake delivery.
fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Keeps non-zero Feature storage in otherwise minimal mount measurements.
struct Child(std::cell::Cell<usize>);
impl Feature for Child {
    type State = usize;
    type Action = u8;
    fn update(&mut self, _: &mut usize, _: &u8, _: &mut crate::FeatureContext<'_, usize, u8>) {
        self.0.set(self.0.get() + 1);
    }
}

/// Builds captured callbacks whose storage cannot disappear as zero-sized values.
fn descriptor() -> OptionalMount<Option<usize>, Child, u64> {
    // Non-zero captures make descriptor storage visible; they do not allocate themselves.
    let identity = std::hint::black_box(7_u64);
    let seed = std::hint::black_box(11_usize);
    OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        move |state: &Option<usize>| {
            state.map(|_| MountIdentity {
                key: identity,
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child(std::cell::Cell::new(seed)),
    )
}

/// Measures real declaration and acquisition operations for either carrier.
///
/// This is not a transaction host: the custom allocation harness omits that
/// test helper. Each sample names its own phase, not an additive actor total.
#[expect(
    clippy::too_many_lines,
    reason = "keep acquisition, dispatch, teardown and allocation measurements in one scenario"
)]
#[expect(
    clippy::needless_pass_by_value,
    reason = "the assay owns state until all access and teardown measurements finish"
)]
pub(super) fn exercise<S, C, F, T>(
    label: &str,
    state: S,
    owner: Box<C::Mount>,
    acquire: impl Fn(Arc<()>) -> F,
) where
    C: Carriers<S, ()>,
    F: Future<Output = Result<Option<T>, TaskAccessError>>,
{
    let (sender, receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
    let ingress = Arc::new(CommandIngress::<Command<S, (), C>>::new(
        sender,
        ControlWake::new(),
    ));
    let (lifecycle, lifecycle_new) = measure(|| Arc::new(Lifecycle::new()));
    let (mut scopes, scope_new) = measure(|| ScopeRegistry::new(&lifecycle));
    let ((), binding) = measure(|| {
        owner.bind(&ingress, &lifecycle);
    });
    assert_eq!(lifecycle_new.0, 2);
    assert_eq!(scope_new, (0, 0));
    let (mut mounts, empty_mounts) = measure(Vec::<Box<C::Mount>>::new);
    let ((), first_mount_slot) = measure(|| mounts.push(owner));
    let (mut mounted, empty_nodes) = measure(Vec::<Box<C::Node>>::new);
    let mut transitions = ScopeTransitionBatch::default();
    let parent = scopes.root();
    let mut discovered = Vec::new();
    let ((), reconcile) = measure(|| {
        mounts[0].reconcile(
            &state,
            &mut scopes,
            &mut transitions,
            &mut mounted,
            parent,
            &mut discovered,
            &mut nested::Descendants::empty(),
            #[cfg(feature = "diagnostics")]
            None,
        );
    });
    let ((), warm_reconcile) = measure(|| {
        mounts[0].reconcile(
            &state,
            &mut scopes,
            &mut transitions,
            &mut mounted,
            parent,
            &mut discovered,
            &mut nested::Descendants::empty(),
            #[cfg(feature = "diagnostics")]
            None,
        );
    });
    assert_eq!(mounted.len(), 1);

    // Same cold/warm mailbox, with one completed acquisition between samples.
    let mut acquisitions = [((0, 0), (0, 0), (0, 0), (0, 0)); 2];
    let mut future_bytes = 0;
    let mut call_bytes = 0;
    for costs in &mut acquisitions {
        let (future, creation) = measure(|| acquire(scopes.root_identity()));
        future_bytes = size_of_val(&future);
        let mut future = std::pin::pin!(future);
        let (pending, admission) = measure(|| poll(future.as_mut()));
        assert!(pending.is_pending());
        let Command::Acquire(call) = receiver.try_recv().unwrap() else {
            panic!("acquisition command");
        };
        call_bytes = size_of_val(&*call);
        let ((), execution) = measure(|| call.execute(&scopes, &lifecycle));
        let (result, completion) = measure(|| poll(future.as_mut()));
        let Poll::Ready(Ok(Some(endpoint))) = result else {
            panic!("installed endpoint");
        };
        std::hint::black_box(&endpoint);
        drop(endpoint);
        *costs = (creation, admission, execution, completion);
    }

    #[cfg(feature = "test-util")]
    {
        // Phase assay of the actual boxed acquisition call and shared diagnostic lane.
        // Preparation uses ingress above; only the subsequent slot phases are measured.
        let gate = crate::testing::TransactionGateCore::<S, (), C>::new();
        let binding = gate.attach().unwrap();
        gate.begin(
            crate::testing::PendingTransaction::Structural,
            crate::testing::ActorDiagnostics::new(
                crate::testing::RuntimeSnapshot {
                    commit_version: 0,
                    live_task_ids: Vec::new(),
                    cancellation_count: 0,
                    timer_deadlines: Vec::new(),
                    observation_count: 0,
                },
                Vec::new(),
                0,
            ),
            false,
        );
        let boundary = gate.boundary().unwrap();
        let mut query_costs = [((0, 0), (0, 0), (0, 0), (0, 0), (0, 0)); 2];
        let mut query_bytes = 0;
        for costs in &mut query_costs {
            let mut future = std::pin::pin!(acquire(scopes.root_identity()));
            assert!(poll(future.as_mut()).is_pending());
            let Command::Acquire(call) = receiver.try_recv().unwrap() else {
                panic!("actual acquisition call");
            };
            let (admission, creation) = measure(|| gate.acquire(boundary, call));
            assert_eq!(creation, (0, 0), "no outer query or future box");
            let mut admission = std::pin::pin!(admission);
            let (admitted, enqueue) = measure(|| poll(admission.as_mut()));
            assert!(admitted.is_ready());
            let (query, take) = measure(|| gate.take_query().unwrap());
            query_bytes = size_of_val(&query);
            assert_eq!(take, (0, 0));
            let ((), execute) = measure(|| query.execute(&state, &scopes, &mounts, &lifecycle));
            let (result, complete) = measure(|| poll(future.as_mut()));
            let Poll::Ready(Ok(Some(endpoint))) = result else {
                panic!("diagnostic acquisition endpoint");
            };
            drop(endpoint);
            *costs = (creation, enqueue, take, execute, complete);
        }
        eprintln!(
            "{label} actual acquisition query cold/warm (wrap, admit, take, execute, complete)={query_costs:?}; query inline bytes={query_bytes}"
        );
        drop(binding);
    };

    let scope = mounts[0]
        .reaction_entry()
        .map(|(scope, _)| scope)
        .expect("fixture declares an Initial hook");
    let (root_authority, root_authority_cost) = measure(|| scopes.authority(scopes.root()));
    let (child_authority, child_authority_cost) = measure(|| scopes.authority(scope));
    let (_, root_clone) = measure(|| root_authority.clone());
    let (_, child_clone) = measure(|| child_authority.clone());
    drop((root_authority, child_authority));

    // Container assay of the current freeze expression, NOT a production commit result.
    // Keep the exact iterator shape: Vec::from/with_capacity would change growth policy.
    let (roster, roster_storage) = measure(|| {
        mounts
            .iter()
            .enumerate()
            .filter_map(|(index, mount)| mount.reaction_entry().map(|(scope, _)| (index, scope)))
            .collect::<Vec<_>>()
    });
    let roster_capacity = roster.capacity();
    let mut dependencies = DependencyValues::new();
    let mut frame = reaction::Frame::default();
    let mut work = Vec::<Batch<S, (), C>>::new();
    let mut reaction_cost = (0, 0);
    for (index, scope) in roster {
        let context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        )
        .in_scope(scope);
        // The live protocol owns the Initial hook, routed batch and work append.
        let ((), cost) = measure(|| {
            mounts[index]
                .reaction_entry()
                .expect("frozen reaction entry remains selected")
                .1
                .run(&state, scope, context, &mut frame, &mut work);
        });
        reaction_cost = cost;
    }
    assert_eq!(work.len(), 1);
    assert!(mounts[0].reaction_entry().is_none());
    let (empty_roster, no_hook_roster) = measure(|| {
        mounts
            .iter()
            .enumerate()
            .filter_map(|(index, mount)| mount.reaction_entry().map(|(scope, _)| (index, scope)))
            .collect::<Vec<_>>()
    });
    assert!(empty_roster.is_empty());
    eprintln!(
        "{label} blocks/requested bytes: registry={scope_new:?}; lifecycle Arc={lifecycle_new:?}; bind={binding:?}; empty declaration/node Vec={empty_mounts:?}/{empty_nodes:?}; first declaration Vec slot={first_mount_slot:?}; reconcile cold/warm={reconcile:?}/{warm_reconcile:?}"
    );
    eprintln!(
        "{label} acquisition cold/warm (create, first poll/admit, execute+endpoint, finish)={acquisitions:?}; outer future inline bytes={future_bytes}; admitted concrete call bytes={call_bytes}"
    );
    eprintln!(
        "{label} authority MICRO-ASSAY root/child={root_authority_cost:?}/{child_authority_cost:?}; clone root/child={root_clone:?}/{child_clone:?}; reaction freeze CONTAINER ASSAY={roster_storage:?}, capacity={roster_capacity}; live Initial hook+routed batch+work append={reaction_cost:?}, work capacity={}; consumed-hook roster={no_hook_roster:?}",
        work.capacity()
    );
    eprintln!(
        "{label} inline bytes: declaration Vec={}, node Vec={}, command={}, Batch={}, roster entry={}",
        size_of_val(&mounts),
        size_of_val(&mounted),
        size_of::<Command<S, (), C>>(),
        size_of::<Batch<S, (), C>>(),
        size_of::<(usize, ScopeToken)>()
    );
    drop((work, mounts, mounted, ingress, receiver, lifecycle, scopes));
}

/// Measures root reaction storage and lowering without builder or actor setup.
fn root_initial_leaf() {
    let seed = std::hint::black_box(17_usize);
    let (descriptor, descriptor_cost) = measure(|| {
        RootMount::new(Child(std::cell::Cell::new(0))).on_initial(
            move |state| *state + seed,
            |change, _| {
                let ReactionChange::Initial { current } = change else {
                    unreachable!("Initial-only fixture")
                };
                Task::run(move |_| async move {
                    std::hint::black_box(current);
                })
            },
        )
    });
    let RootMount(MountConstruction {
        feature,
        initial,
        reactions,
        configuration,
    }) = descriptor;
    // Actual root declaration representation; builder/runtime setup is deliberately excluded.
    let (declaration, record_cost) = measure(|| {
        Box::new(RootDeclaration::<usize, u8> {
            payload: Mutex::new(Some(RootInitial {
                hook: initial,
                reactions,
                scope: None,
                pending: true,
                configuration,
                current_configuration: None,
            })),
        })
    });
    let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
    let mut transitions = ScopeTransitionBatch::default();
    let mut mounted = Vec::new();
    let parent = scopes.root();
    declaration.reconcile(
        &1,
        &mut scopes,
        &mut transitions,
        &mut mounted,
        parent,
        &mut Vec::new(),
        &mut nested::Descendants::empty(),
        #[cfg(feature = "diagnostics")]
        None,
    );
    let scope = declaration.reaction_entry().unwrap().0;
    let (authority, authority_cost) = measure(|| scopes.authority(scope));
    drop(authority);
    let mut dependencies = DependencyValues::new();
    let mut frame = reaction::Frame::default();
    let mut work = Vec::new();
    let context = ReduceContext::new(
        &mut dependencies,
        MonotonicInstant::ZERO,
        &mut scopes,
        &mut transitions,
    )
    .in_scope(scope);
    let ((), initial_cost) = measure(|| {
        declaration
            .reaction_entry()
            .expect("frozen reaction entry remains selected")
            .1
            .run(&1, scope, context, &mut frame, &mut work);
    });
    assert!(matches!(work.as_slice(), [Batch::Root(_)]));
    assert!(declaration.reaction_entry().is_none());
    eprintln!(
        "root Initial leaf blocks/requested bytes: captured descriptor={descriptor_cost:?}; erased-record backing Box={record_cost:?}; root authority MICRO-ASSAY={authority_cost:?}; live hook+root batch+work append={initial_cost:?}; not a builder/actor/commit total"
    );
    drop((work, declaration, feature));
}

/// Measures weak root-registration metadata and checks that owner disposal clears it.
fn root_removal_metadata<C, O>(
    label: &str,
    owner: O,
    installed: impl Fn() -> Option<(Arc<()>, ScopeToken)>,
    construction: (u64, u64),
    inline_bytes: usize,
) where
    C: Carriers<usize, ()>,
    O: MountDeclaration<usize, (), C>,
{
    let (sender, receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
    let ingress = Arc::new(CommandIngress::<Command<usize, (), C>>::new(
        sender,
        ControlWake::new(),
    ));
    let lifecycle = Arc::new(Lifecycle::new());
    let lifecycle_weak = Arc::downgrade(&lifecycle);
    let root = lifecycle.root_identity();
    let root_weak = Arc::downgrade(&root);
    let mut scopes = ScopeRegistry::new(&lifecycle);
    let mut transitions = ScopeTransitionBatch::default();
    let mut mounted = Vec::new();
    let mut discovered = Vec::new();
    let scope = scopes.root();
    assert!(installed().is_none());
    let ((), binding) = measure(|| owner.bind(&ingress, &lifecycle));
    assert!(installed().is_none());
    let mut reconcile = || {
        owner.reconcile(
            &0,
            &mut scopes,
            &mut transitions,
            &mut mounted,
            scope,
            &mut discovered,
            &mut nested::Descendants::empty(),
            #[cfg(feature = "diagnostics")]
            None,
        );
    };
    let ((), cold) = measure(&mut reconcile);
    let ((), warm) = measure(&mut reconcile);
    let (snapshot, lookup) = measure(&installed);
    let (identity, installed_scope) = snapshot.unwrap();
    assert!(Arc::ptr_eq(&identity, &root));
    assert_eq!(installed_scope, scope);
    drop(identity);
    assert!(mounted.is_empty());
    assert!(discovered.is_empty());
    assert_eq!(Arc::strong_count(&lifecycle), 1);
    let ((), teardown) = measure(|| drop(owner));
    assert!(installed().is_none());
    for cost in [binding, cold, warm, lookup, teardown] {
        assert_eq!(cost, (0, 0));
    }
    eprintln!(
        "{label} root removal metadata blocks/requested bytes: construction={construction:?}; bind={binding:?}; reconcile cold/warm={cold:?}/{warm:?}; installed snapshot={lookup:?}; owner teardown={teardown:?}; registration inline bytes={inline_bytes}"
    );
    // The surviving registration must retain neither actor lifetime nor root identity.
    drop((root, lifecycle, scopes, ingress, receiver));
    assert!(lifecycle_weak.upgrade().is_none());
    assert!(root_weak.upgrade().is_none());
}

/// Separates already-ready, newly waiting and closed readiness costs.
fn readiness() {
    let lifecycle = Arc::new(Lifecycle::new());
    let (_, creation_ready) = measure(|| Lifecycle::ready_weak(Arc::downgrade(&lifecycle)));
    let future = Lifecycle::ready_weak(Arc::downgrade(&lifecycle));
    let ready_bytes = size_of_val(&future);
    let mut future = std::pin::pin!(future);
    let (result, poll_ready) = measure(|| poll(future.as_mut()));
    assert!(matches!(result, Poll::Ready(Ok(()))));
    // Fresh event: pending readiness must not inherit the ready control's warm listener.
    let lifecycle = Arc::new(Lifecycle::new());
    lifecycle.pending_installation();
    let (future, creation_pending) = measure(|| Lifecycle::ready_weak(Arc::downgrade(&lifecycle)));
    let pending_bytes = size_of_val(&future);
    let mut future = std::pin::pin!(future);
    let (result, poll_pending) = measure(|| poll(future.as_mut()));
    assert!(result.is_pending());
    let ((), notify) = measure(|| lifecycle.mark_ready());
    let (result, finish) = measure(|| poll(future.as_mut()));
    assert!(matches!(result, Poll::Ready(Ok(()))));
    let closed = Arc::new(Lifecycle::new());
    closed.close(super::super::lifecycle::CloseState::Abortive);
    let mut closed = std::pin::pin!(Lifecycle::ready_weak(Arc::downgrade(&closed)));
    let (result, poll_closed) = measure(|| poll(closed.as_mut()));
    assert!(
        matches!(result, Poll::Ready(Err(error)) if error.reason() == crate::TaskAccessReason::StoreClosed)
    );
    eprintln!(
        "ready future blocks/requested bytes: create ready/pending={creation_ready:?}/{creation_pending:?}; poll ready/pending/closed={poll_ready:?}/{poll_pending:?}/{poll_closed:?}; notify={notify:?}; finish={finish:?}; inline ready/pending={ready_bytes}/{pending_bytes}"
    );
    assert_eq!(creation_ready, (0, 0));
    assert_eq!(poll_ready, (0, 0), "resolved readiness needs no listener");
    assert_eq!(poll_closed, (0, 0), "closed readiness needs no listener");
    assert_eq!(
        finish,
        (0, 0),
        "resolved pending wait needs no new listener"
    );
}

/// Runs phase assays in one isolated allocation-counting child process.
#[expect(
    clippy::too_many_lines,
    reason = "keep declaration, reconciliation and acquisition phases in one allocation assay"
)]
pub(crate) fn mount_declaration_and_acquisition_costs() {
    const NAME: &str = "store::mount::allocation_tests::mount_declaration_and_acquisition_costs";
    if crate::allocation_test::run_in_child(NAME) {
        return;
    }
    let profiler = dhat::Profiler::builder().testing().build();
    // Do not use counts from control construction as additive production totals.
    let (descriptor_control, descriptor_cost) = measure(descriptor);
    let (node, node_identity) = measure(|| ScopeRegistry::node(&mut ScopeSlot::new()));
    let (cell, cell_cost) = measure(|| {
        Arc::new(DeclarationCell::<Option<usize>, (), Child, u64> {
            node,
            binding: OnceLock::new(),
            payload: Mutex::new(Some(Declaration {
                descriptor: descriptor_control.into(),
                current: None,
                installed: None,
                initial_pending: false,
                mapping: (),
            })),
        })
    });
    let (site, erasure) = measure(|| {
        let erased: Arc<dyn Acquisition<Option<usize>, usize, u8, ()>> =
            Arc::<DeclarationCell<Option<usize>, (), Child, u64>>::clone(&cell);
        MountSite {
            declaration: Arc::downgrade(&erased),
        }
    });
    assert_eq!(
        Arc::as_ptr(&cell).cast::<()>(),
        Weak::as_ptr(&site.declaration).cast::<()>()
    );
    let (_, site_clone) = measure(|| site.clone());
    let (owner, owner_cost) = measure(|| Box::new(DeclarationOwner(std::sync::Arc::clone(&cell))));
    let (owner, owner_erasure) =
        measure(|| owner as Box<dyn MountDeclaration<Option<usize>, ()> + Send>);
    let root = Arc::new(());
    let (inner, inner_future) = measure(|| {
        std::sync::Arc::clone(&cell).acquire(
            std::sync::Arc::clone(&root),
            #[cfg(feature = "test-util")]
            None,
        )
    });
    let inner_future_bytes = size_of_val(inner.as_ref().get_ref());
    drop(inner);
    #[cfg(feature = "test-util")]
    {
        let gate = crate::testing::TransactionGateCore::new();
        let (gated, gated_cost) = measure(|| {
            std::sync::Arc::clone(&cell).acquire(std::sync::Arc::clone(&root), Some(gate))
        });
        assert_eq!(
            gated_cost, inner_future,
            "Some and None use the same one concrete future box"
        );
        eprintln!(
            "shared gated inner future Box={gated_cost:?}, concrete bytes={}",
            size_of_val(gated.as_ref().get_ref())
        );
        drop(gated);
    };
    let ((reply, receive), reply_channel) =
        measure(ReplyGuard::<Option<TaskContext<usize, u8>>>::channel);
    let (request, command_box) = measure(|| {
        Box::new(TypedAcquire {
            cell: Arc::downgrade(&cell),
            reply,
        })
    });
    let (command, command_erasure) = measure(|| Command::<Option<usize>, ()>::Acquire(request));
    let Command::Acquire(request) = command else {
        unreachable!("constructed acquisition")
    };
    request.close();
    drop(receive);
    eprintln!(
        "shared controls blocks/requested bytes: captured descriptor={descriptor_cost:?}; node identity={node_identity:?}; declaration Arc={cell_cost:?}; same-cell erasure+Weak={erasure:?}; site clone={site_clone:?}; owner Box={owner_cost:?}; owner erasure={owner_erasure:?}"
    );
    eprintln!(
        "shared isolated acquisition controls blocks/requested bytes: inner future Box={inner_future:?}, concrete bytes={inner_future_bytes}; reply channel={reply_channel:?}; command Box={command_box:?}; enum erasure={command_erasure:?}; typed call inline bytes={}",
        size_of::<TypedAcquire<Option<usize>, (), Child, u64>>()
    );
    drop((owner, site, cell));

    let (descriptor, base_descriptor) = measure(descriptor);
    let seed = std::hint::black_box(13_usize);
    let (descriptor, hook_descriptor) = measure(|| {
        descriptor.on_initial(
            move |state| *state + seed,
            |change, _| {
                let ReactionChange::Initial { current } = change else {
                    unreachable!("Initial-only fixture")
                };
                Task::run(move |_| async move {
                    std::hint::black_box(current);
                })
            },
        )
    });
    let ((owner, site), attachment) = measure(|| attach::<_, (), _, _>(descriptor));
    eprintln!(
        "shared actual descriptor/Initial registration/attach blocks/requested bytes={base_descriptor:?}/{hook_descriptor:?}/{attachment:?}"
    );
    exercise::<_, Shared, _, _>("shared", Some(1), owner, |root| site.acquire(root));
    #[cfg(feature = "local")]
    super::local::allocation_tests::run();
    root_initial_leaf();
    {
        use crate::store::output::OutputRegistration;
        use crate::store::removal::root::{RootOwner, RootRegistration};
        let (registration, construction) = measure(RootRegistration::<usize, ()>::new);
        root_removal_metadata::<Shared, _>(
            "shared",
            RootOwner(Arc::clone(&registration)),
            || registration.installed(),
            construction,
            size_of_val(&*registration),
        );
    };
    #[cfg(feature = "local")]
    {
        use crate::store::local::Local;
        use crate::store::output::local::OutputRegistration;
        use crate::store::removal::local::root::{RootOwner, RootRegistration};
        let (registration, construction) = measure(RootRegistration::<usize, ()>::new);
        root_removal_metadata::<Local, _>(
            "local",
            RootOwner(std::rc::Rc::clone(&registration)),
            || registration.installed(),
            construction,
            size_of_val(&*registration),
        );
    };
    readiness();
    eprintln!(
        "current inline bytes: descriptor={}, declaration payload={}, declaration cell={}, thin owner={}, erased owner={}, site={}, ScopeRegistry={}, ScopeAuthority={}, Lifecycle={}, Store={}, StoreBuilder={}, Actor={}, Turn={}, root descriptor={}, root declaration={}",
        size_of::<OptionalMount<Option<usize>, Child, u64>>(),
        size_of::<Declaration<Option<usize>, Child, u64>>(),
        size_of::<DeclarationCell<Option<usize>, (), Child, u64>>(),
        size_of::<DeclarationOwner<Option<usize>, (), Child, u64>>(),
        size_of::<Box<dyn MountDeclaration<Option<usize>, ()> + Send>>(),
        size_of::<MountSite<Option<usize>, usize, u8, ()>>(),
        size_of::<ScopeRegistry>(),
        size_of::<ScopeAuthority>(),
        size_of::<Lifecycle>(),
        size_of::<crate::Store<Option<usize>, ()>>(),
        size_of::<super::super::builder::StoreBuilder<Option<usize>, (), ()>>(),
        size_of::<super::super::actor::Actor<Option<usize>, ()>>(),
        size_of::<super::super::transaction::Turn>(),
        size_of::<RootMount<Child>>(),
        size_of::<RootDeclaration<usize, u8>>()
    );
    eprintln!(
        "field widths, NOT measured aggregate deltas: declaration Vec={}, optional erased root record={}, root configuration={}, optional lifecycle Weak={}, lifecycle OnceLock<Arc<()>>={}, ready flag={}, old ancestry entry={}, current ancestry entry={}",
        size_of::<Vec<Box<dyn MountDeclaration<Option<usize>, ()> + Send>>>(),
        size_of::<Option<Box<dyn MountDeclaration<Option<usize>, ()> + Send>>>(),
        size_of::<u64>(),
        size_of::<Option<Weak<Lifecycle>>>(),
        size_of::<OnceLock<Arc<()>>>(),
        size_of::<std::sync::atomic::AtomicBool>(),
        size_of::<(ScopeToken, ScopeNodeId, ScopeToken)>(),
        size_of::<(ScopeToken, ScopeToken, u64)>()
    );
    #[cfg(feature = "test-util")]
    super::nested::fold_gate::run_costs();
    drop(profiler);
}

pub(in crate::store) use nested_cost::nested_roster_costs;

/// Compares actual roster reconciliation with flat, empty and child recipes.
mod nested_cost {
    use super::{
        Arc, Command, CommandIngress, ControlWake, Descriptor, Feature, MountDeclaration,
        MountIdentity, NonZeroUsize, OptionalMount, ScopeRegistry, ScopeTransitionBatch, Shared,
        attach_descriptor, measure,
    };
    use crate::store::mounted::path::KeyedPath;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records selectors and per-sibling construction, recipe and disposal calls.
    struct Counts {
        collection: AtomicUsize,
        child_select: AtomicUsize,
        child_identity: AtomicUsize,
        parent_make: Vec<AtomicUsize>,
        child_make: Vec<AtomicUsize>,
        recipe: Vec<AtomicUsize>,
        parent_drop: Vec<AtomicUsize>,
        child_drop: Vec<AtomicUsize>,
    }
    impl Counts {
        /// Allocates counters before any measured roster operation.
        fn new(siblings: usize) -> Self {
            let counters = || (0..siblings).map(|_| AtomicUsize::new(0)).collect();
            Self {
                collection: AtomicUsize::new(0),
                child_select: AtomicUsize::new(0),
                child_identity: AtomicUsize::new(0),
                parent_make: counters(),
                child_make: counters(),
                recipe: counters(),
                parent_drop: counters(),
                child_drop: counters(),
            }
        }
        /// Flattens selectors, then five sibling groups, outside measured operations.
        fn snapshot(&self) -> Vec<usize> {
            let read = |value: &AtomicUsize| value.load(Ordering::Relaxed);
            let mut values = vec![
                read(&self.collection),
                read(&self.child_select),
                read(&self.child_identity),
            ];
            for counters in [
                &self.parent_make,
                &self.child_make,
                &self.recipe,
                &self.parent_drop,
                &self.child_drop,
            ] {
                values.extend(counters.iter().map(read));
            }
            values
        }
    }
    /// Connects optional child state to its independent sibling counters.
    struct Row {
        child: Option<usize>,
        index: usize,
        counts: Arc<Counts>,
    }
    /// Owns keyed rows and counts collection selection separately.
    struct State {
        rows: crate::KeyedState<u8, Row>,
        counts: Arc<Counts>,
    }
    /// Counts each immutable collection selection during reconciliation.
    fn rows(state: &State) -> &crate::KeyedState<u8, Row> {
        state.counts.collection.fetch_add(1, Ordering::Relaxed);
        &state.rows
    }
    /// Supplies the mutable projection required by the real keyed descriptor.
    fn rows_mut(state: &mut State) -> &mut crate::KeyedState<u8, Row> {
        &mut state.rows
    }
    /// Associates one installed parent with its disposal counter.
    struct Parent {
        index: usize,
        counts: Arc<Counts>,
    }
    impl Feature for Parent {
        type State = Row;
        type Action = ();
        fn update(&mut self, _: &mut Row, (): &(), _: &mut crate::FeatureContext<'_, Row, ()>) {}
    }
    impl Drop for Parent {
        fn drop(&mut self) {
            self.counts.parent_drop[self.index].fetch_add(1, Ordering::Relaxed);
        }
    }
    /// Associates one installed child with its disposal counter.
    struct Leaf {
        index: usize,
        counts: Arc<Counts>,
    }
    impl Feature for Leaf {
        type State = usize;
        type Action = ();
        fn update(&mut self, _: &mut usize, (): &(), _: &mut crate::FeatureContext<'_, usize, ()>) {
        }
    }
    impl Drop for Leaf {
        fn drop(&mut self) {
            self.counts.child_drop[self.index].fetch_add(1, Ordering::Relaxed);
        }
    }
    /// Counts construction of the selected parent incarnation.
    fn parent(row: &Row) -> Parent {
        row.counts.parent_make[row.index].fetch_add(1, Ordering::Relaxed);
        Parent {
            index: row.index,
            counts: std::sync::Arc::clone(&row.counts),
        }
    }
    /// Builds a child whose selection, identity and lifetime have separate counters.
    fn child(row: &Row) -> OptionalMount<Row, Leaf, ()> {
        let index = row.index;
        let counts = std::sync::Arc::clone(&row.counts);
        OptionalMount::new(
            |row: &Row| {
                row.counts.child_select.fetch_add(1, Ordering::Relaxed);
                row.child.as_ref()
            },
            |row: &mut Row| row.child.as_mut(),
            |row: &Row| {
                row.counts.child_identity.fetch_add(1, Ordering::Relaxed);
                row.child.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| {
                counts.child_make[index].fetch_add(1, Ordering::Relaxed);
                Leaf {
                    index,
                    counts: std::sync::Arc::clone(&counts),
                }
            },
        )
    }
    /// Selects whether reconciliation visits no recipe, an empty recipe or a child.
    #[derive(Clone, Copy, Debug)]
    enum Profile {
        Flat,
        EmptyRecipe,
        ChildRecipe,
    }
    /// Builds the requested production descriptor outside measured reconciliation.
    fn declaration(profile: Profile, key: u8) -> Box<dyn MountDeclaration<State, ()> + Send> {
        match profile {
            Profile::Flat => {
                // Same selected-key descriptor as production attach_selected_key,
                // with no RecipeOwner. This is descriptor setup, not reconciliation.
                let key = Arc::new(key);
                let path = KeyedPath::new(rows, rows_mut, std::sync::Arc::clone(&key));
                let descriptor = Descriptor {
                    path,
                    identity: Box::new(move |state: &State, live| {
                        let rows = rows(state);
                        if !live() {
                            return None;
                        }
                        rows.entry_stamp(&*key).map(|stamp| MountIdentity {
                            key: stamp.to_owned(),
                            replacement: 0,
                            configuration: 0,
                        })
                    }),
                    make: Box::new(parent),
                    initial: None,
                    reactions: Vec::new(),
                };
                Box::new(attach_descriptor::<State, (), Parent, _, _>(descriptor).0)
            }
            Profile::EmptyRecipe | Profile::ChildRecipe => {
                super::super::nested::attach_selected_key::<State, (), Parent, _, _>(
                    rows,
                    rows_mut,
                    key,
                    parent,
                    move |row, children| {
                        row.counts.recipe[row.index].fetch_add(1, Ordering::Relaxed);
                        if matches!(profile, Profile::ChildRecipe) {
                            let _site = children.optional(child(row));
                        }
                    },
                )
                .0
            }
        }
    }
    /// Runs eight fixed mutations and checks work counts independently of costs.
    #[expect(
        clippy::too_many_lines,
        reason = "keep each nested profile construction, access and teardown measurement together"
    )]
    fn run(profile: Profile, siblings: usize) {
        assert!(matches!(siblings, 2 | 64));
        let counts = Arc::new(Counts::new(siblings));
        let mut state = State {
            rows: (0..siblings)
                .map(|index| {
                    (
                        u8::try_from(index).unwrap(),
                        Row {
                            child: Some(7),
                            index,
                            counts: std::sync::Arc::clone(&counts),
                        },
                    )
                })
                .collect(),
            counts: std::sync::Arc::clone(&counts),
        };
        let (sender, _receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
        let ingress = Arc::new(CommandIngress::<Command<State, ()>>::new(
            sender,
            ControlWake::new(),
        ));
        let (lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut mounts = Vec::new();
        for key in 0..siblings {
            let owner = declaration(profile, u8::try_from(key).unwrap());
            owner.bind(&ingress, &lifecycle);
            mounts.push(owner);
        }
        let has_recipes = mounts.iter().any(|mount| mount.can_discover());
        assert_eq!(has_recipes, !matches!(profile, Profile::Flat));
        let mut mounted: Vec<Box<dyn crate::store::mounted::InstalledNode + Send>> = Vec::new();
        for phase in 0..8 {
            let ((), mutation) = measure(|| match phase {
                3 => {
                    state.rows.remove(&0);
                    state.rows.insert(
                        0,
                        Row {
                            child: Some(9),
                            index: 0,
                            counts: std::sync::Arc::clone(&counts),
                        },
                    );
                }
                5 => {
                    for index in 0..siblings {
                        let key = u8::try_from(index).unwrap();
                        state.rows.remove(&key);
                        state.rows.insert(
                            key,
                            Row {
                                child: Some(11),
                                index,
                                counts: std::sync::Arc::clone(&counts),
                            },
                        );
                    }
                }
                7 => state.rows.get_mut(&0).unwrap().child = None,
                _ => {}
            });
            let before_counts = counts.snapshot();
            let old_pointer = mounts.as_ptr();
            let old_capacity = mounts.capacity();
            let mut transitions = ScopeTransitionBatch::default();
            let ((), reconcile) = measure(|| {
                super::super::nested::reconcile_roster::<State, (), Shared>(
                    &mut mounts,
                    has_recipes,
                    &state,
                    &mut scopes,
                    &mut transitions,
                    &mut mounted,
                    #[cfg(feature = "diagnostics")]
                    None,
                );
            });
            // Existing transaction tail performs this prune; keep its cost separate.
            let ((), prune) = measure(|| {
                if !transitions.is_empty() {
                    mounted.retain(|node| scopes.is_current(node.scope()));
                }
            });
            let after_counts = counts.snapshot();
            let delta: Vec<_> = after_counts
                .iter()
                .zip(&before_counts)
                .map(|(after, before)| after - before)
                .collect();
            for sibling in 0..siblings {
                let replaced = phase == 5 || (phase == 3 && sibling == 0);
                let installed = phase == 0 || replaced;
                let child_removed = phase == 7 && sibling == 0;
                let has_child = matches!(profile, Profile::ChildRecipe);
                let expected = [
                    usize::from(installed),
                    usize::from(has_child && installed),
                    usize::from(has_recipes && installed),
                    usize::from(replaced),
                    usize::from(has_child && (replaced || child_removed)),
                ];
                for (group, expected) in expected.into_iter().enumerate() {
                    assert_eq!(
                        delta[3 + group * siblings + sibling],
                        expected,
                        "{profile:?}, phase {phase}, sibling {sibling}, counter group {group}"
                    );
                }
            }
            if matches!(profile, Profile::Flat) {
                assert_eq!(
                    mounts.as_ptr(),
                    old_pointer,
                    "no-recipe branch keeps backing storage"
                );
                assert_eq!(mounts.capacity(), old_capacity);
            }
            let expected = if matches!(profile, Profile::ChildRecipe) {
                2 * siblings
            } else {
                siblings
            };
            assert_eq!(mounts.len(), expected);
            assert_eq!(
                mounted.len(),
                expected - usize::from(phase == 7 && matches!(profile, Profile::ChildRecipe))
            );
            let totals: [usize; 5] = std::array::from_fn(|group| {
                delta[3 + group * siblings..3 + (group + 1) * siblings]
                    .iter()
                    .sum()
            });
            eprintln!(
                "roster {profile:?} siblings={siblings} phase={phase} [0 install,1/2 unchanged,3 replace first,4 unchanged,5 replace all,6 unchanged,7 remove first child]: input mutation={mutation:?}; ACTUAL reconcile_roster={reconcile:?}; node prune={prune:?}; roster len/cap={}/{}, old cap={old_capacity}, same backing={}; selector deltas [collection,child select,child identity]={:?}; totals [parent make,child make,recipe,parent drop,child drop]={totals:?}; individual sibling counters asserted",
                mounts.len(),
                mounts.capacity(),
                mounts.as_ptr() == old_pointer,
                &delta[..3],
            );
        }
    }
    /// Runs the same roster transitions for small and larger sibling sets.
    pub(in crate::store) fn nested_roster_costs() {
        for siblings in [2, 64] {
            run(Profile::Flat, siblings);
            run(Profile::EmptyRecipe, siblings);
            run(Profile::ChildRecipe, siblings);
        }
    }
}
