//! Actual Rc/RefCell declaration carrier, including non-Send captures and State.
//!
//! The caller owns the profiler. Isolated carrier construction costs are controls,
//! not additive estimates of a complete mount. Pre-existing captures are excluded.
use super::super::allocation_tests::{exercise, measure};
use super::{
    Acquisition, Arc, Cell, Command, Declaration, Descriptor, Local, LocalMountSite,
    LocalOptionalMount, LocalWeak, MountDeclaration, MountIdentity, OnceLock, OptionalPath, Owner,
    Rc, ReactionChange, RefCell, ReplyGuard, ScopeRegistry, ScopeSlot, TypedAcquire, attach_local,
    local,
};
use std::cell::Cell as Counter;
use std::mem::size_of;

/// Retains non-thread-safe reducer state in the actual local feature carrier.
struct Child(Rc<Counter<usize>>);
impl local::Feature for Child {
    type State = Rc<Counter<usize>>;
    type Action = Rc<u8>;
    fn update(
        &mut self,
        _: &mut Self::State,
        _: &Self::Action,
        _: &mut local::FeatureContext<'_, Self::State, Self::Action>,
    ) {
        self.0.set(self.0.get() + 1);
    }
}
/// Forces the local declaration and acquisition path to accept non-`Send` state.
type Root = Option<Rc<Counter<usize>>>;

/// Measures real local declaration, acquisition and callback ownership phases.
#[expect(
    clippy::too_many_lines,
    reason = "keep local declaration, reconciliation and acquisition phases in one allocation assay"
)]
pub(in crate::store::mount) fn run() {
    let descriptor_control = LocalOptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Root| {
            state.as_ref().map(|_| MountIdentity {
                key: 7_u64,
                replacement: 0,
                configuration: 0,
            })
        },
        |state: &Rc<Counter<usize>>| Child(std::rc::Rc::clone(state)),
    );
    let (node, node_identity) = measure(|| ScopeRegistry::node(&mut ScopeSlot::new()));
    let LocalOptionalMount {
        path,
        identity,
        make,
        initial,
        reactions,
    } = descriptor_control;
    let descriptor_control = Descriptor {
        path,
        identity,
        make,
        initial,
        reactions,
    };
    let (cell, cell_cost) = measure(|| {
        Rc::new(Cell::<Root, (), Child, u64> {
            node,
            binding: OnceLock::new(),
            payload: RefCell::new(Some(Declaration {
                descriptor: descriptor_control,
                current: None,
                installed: None,
                initial_pending: false,
            })),
        })
    });
    let (site, erasure) = measure(|| {
        let erased: Rc<dyn Acquisition<Root, Rc<Counter<usize>>, Rc<u8>, ()>> =
            Rc::<Cell<Root, (), Child, u64>>::clone(&cell);
        LocalMountSite {
            declaration: Rc::downgrade(&erased),
        }
    });
    assert_eq!(
        Rc::as_ptr(&cell).cast::<()>(),
        LocalWeak::as_ptr(&site.declaration).cast::<()>()
    );
    let (owner, owner_cost) = measure(|| Box::new(Owner(std::rc::Rc::clone(&cell))));
    let (owner, owner_erasure) = measure(|| owner as Box<dyn MountDeclaration<Root, (), Local>>);
    let root = Arc::new(());
    let (inner, inner_future) = measure(|| {
        std::rc::Rc::clone(&cell).acquire(
            std::sync::Arc::clone(&root),
            #[cfg(feature = "test-util")]
            None,
        )
    });
    let inner_future_bytes = std::mem::size_of_val(inner.as_ref().get_ref());
    drop(inner);
    #[cfg(feature = "test-util")]
    {
        let gate = crate::testing::TransactionGateCore::new();
        let (gated, gated_cost) =
            measure(|| std::rc::Rc::clone(&cell).acquire(std::sync::Arc::clone(&root), Some(gate)));
        assert_eq!(
            gated_cost, inner_future,
            "Some and None use the same one concrete future box"
        );
        eprintln!(
            "local gated inner future Box={gated_cost:?}, concrete bytes={}",
            std::mem::size_of_val(gated.as_ref().get_ref())
        );
        drop(gated);
    };
    let ((reply, receive), reply_channel) =
        measure(ReplyGuard::<Option<local::TaskContext<Rc<Counter<usize>>, Rc<u8>>>>::channel);
    let (request, command_box) = measure(|| {
        Box::new(TypedAcquire {
            cell: Rc::downgrade(&cell),
            reply,
        })
    });
    let (command, command_erasure) = measure(|| Command::<Root, (), Local>::Acquire(request));
    let Command::Acquire(request) = command else {
        unreachable!("constructed acquisition")
    };
    request.close();
    drop(receive);
    eprintln!(
        "local controls blocks/requested bytes: node identity={node_identity:?}; declaration Rc={cell_cost:?}; same-cell erasure+Weak={erasure:?}; owner Box={owner_cost:?}; owner erasure={owner_erasure:?}; inner future Box={inner_future:?}, concrete bytes={inner_future_bytes}; reply channel={reply_channel:?}; command Box={command_box:?}; enum erasure={command_erasure:?}"
    );
    drop((owner, site, cell));
    let cache = Rc::new(Counter::new(0));
    let identity = Rc::new(7_u64);
    let (descriptor, descriptor_cost) = measure(|| {
        LocalOptionalMount::new(
            Option::as_ref,
            Option::as_mut,
            move |state: &Root| {
                state.as_ref().map(|_| MountIdentity {
                    key: *identity,
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| Child(std::rc::Rc::clone(&cache)),
        )
    });
    let selected = Rc::new(Counter::new(13));
    let (descriptor, hook_descriptor) = measure(|| {
        descriptor.on_initial(
            move |_| std::rc::Rc::clone(&selected),
            |change, _| {
                let ReactionChange::Initial { current } = change else {
                    unreachable!("Initial-only fixture")
                };
                local::Task::run(move |_| async move {
                    std::hint::black_box(current);
                })
            },
        )
    });
    let ((owner, site), attachment) = measure(|| attach_local::<_, (), _, _>(descriptor));
    eprintln!(
        "local actual captured descriptor/Initial registration/attach blocks/requested bytes={descriptor_cost:?}/{hook_descriptor:?}/{attachment:?}; pre-existing Rc captures excluded"
    );
    exercise::<_, Local, _, _>("local", Some(Rc::new(Counter::new(1))), owner, |root| {
        site.acquire(root)
    });
    eprintln!(
        "local inline bytes: descriptor={}, payload={}, cell={}, thin owner={}, erased owner={}, site={}, typed acquire={}",
        size_of::<LocalOptionalMount<Root, Child, u64>>(),
        size_of::<Declaration<Root, Child, u64>>(),
        size_of::<Cell<Root, (), Child, u64>>(),
        size_of::<Owner<Root, (), Child, u64>>(),
        size_of::<Box<dyn MountDeclaration<Root, (), Local>>>(),
        size_of::<LocalMountSite<Root, Rc<Counter<usize>>, Rc<u8>, ()>>(),
        size_of::<TypedAcquire<Root, (), Child, u64, OptionalPath<Root, Rc<Counter<usize>>>>>()
    );
}
