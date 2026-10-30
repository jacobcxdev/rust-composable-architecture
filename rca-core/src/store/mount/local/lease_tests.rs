//! Checks real local declaration leases, callback shutdown and installed authority.

use super::*;
use crate::store::transaction::tests::Host;
use crate::store::transaction::{self, Mutation};
use std::cell::{Cell as Counter, OnceCell};
use std::task::{Context, Poll, Waker};

/// Keeps a uniquely owned non-`Send` capture while structural turns run.
struct Root(Rc<()>);
impl local::Feature for Root {
    type State = Option<Rc<Counter<usize>>>;
    type Action = Rc<()>;
    fn update(
        &mut self,
        _: &mut Self::State,
        _: &Self::Action,
        _: &mut local::FeatureContext<'_, Self::State, Self::Action>,
    ) {
        assert_eq!(Rc::strong_count(&self.0), 1);
    }
}
/// Provides a local child with non-`Send` state and actions.
struct Child;
impl local::Feature for Child {
    type State = Rc<Counter<usize>>;
    type Action = Rc<u8>;
    fn update(
        &mut self,
        _: &mut Self::State,
        _: &Self::Action,
        _: &mut local::FeatureContext<'_, Self::State, Self::Action>,
    ) {
    }
}
/// Names the real declaration cell whose payload borrow the probe inspects.
type TestCell = Cell<Option<Rc<Counter<usize>>>, Rc<()>, Child, ()>;
/// Checks callback and destructor access without owning the declaration payload.
struct Probe {
    /// Resolves the declaration after the callback captures have been installed.
    cell: Rc<OnceCell<LocalWeak<TestCell>>>,
    /// Counts each callback capture's destructor independently of its invocation.
    drops: Rc<Counter<usize>>,
}
impl Probe {
    /// Requires an available mutable borrow and a payload currently leased out.
    fn check(&self) {
        let cell = self.cell.get().unwrap().upgrade().unwrap();
        let guard = cell
            .payload
            .try_borrow_mut()
            .expect("user callback/drop runs outside declaration borrow");
        assert!(guard.is_none());
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.check();
        self.drops.set(self.drops.get() + 1);
    }
}

// Catches shared-only repairs that leave real Rc declaration callbacks borrowed.
#[expect(
    clippy::too_many_lines,
    reason = "keep callback phases, exact panic checks and final capture destruction in one scenario"
)]
#[test]
fn local_declaration_callbacks_and_panic_destruction_release_refcell_borrow() {
    /// Fails only after the initial factory enters dependency resolution.
    struct PanicDependency;
    impl crate::DependencyKey for PanicDependency {
        type Value = ();
        fn live_value() {
            panic!("dependency default panic")
        }
    }
    for phase in 0..=6 {
        let cell_slot = Rc::new(OnceCell::new());
        let drops = Rc::new(Counter::new(0));
        let probe = || Probe {
            cell: std::rc::Rc::clone(&cell_slot),
            drops: std::rc::Rc::clone(&drops),
        };
        let identity_probe = probe();
        let constructor_probe = probe();
        let selector_probe = probe();
        let factory_probe = probe();
        let descriptor = LocalOptionalMount::new(
            if phase == 6 {
                |_| panic!("path selector panic")
            } else {
                Option::as_ref
            },
            Option::as_mut,
            move |state: &Option<Rc<Counter<usize>>>| {
                identity_probe.check();
                assert_ne!(phase, 1, "identity panic");
                state.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| {
                constructor_probe.check();
                assert_ne!(phase, 2, "constructor panic");
                Child
            },
        )
        .on_initial(
            move |state| {
                selector_probe.check();
                assert_ne!(phase, 3, "selected value panic");
                Rc::clone(state)
            },
            move |_, mut context| {
                factory_probe.check();
                assert_ne!(phase, 4, "factory panic");
                if phase == 5 {
                    context.dependency::<PanicDependency>();
                }
                local::Task::run(|_| async {})
            },
        );
        let LocalOptionalMount {
            path,
            identity,
            make,
            initial,
            reactions,
        } = descriptor;
        let descriptor = Descriptor {
            path,
            identity,
            make,
            initial,
            reactions,
        };
        let cell = Rc::new(Cell {
            node: ScopeRegistry::node(&mut ScopeSlot::new()),
            binding: OnceLock::new(),
            payload: RefCell::new(Some(Declaration {
                descriptor,
                current: None,
                installed: None,
                initial_pending: false,
            })),
        });
        cell_slot.set(Rc::downgrade(&cell)).unwrap();
        let mut host = Host::<_, _, Local>::new(
            Some(Rc::new(Counter::new(1))),
            Box::new(local::Driver(Root(Rc::new(())))),
        );
        let lifecycle = std::sync::Arc::clone(&host.lifecycle);
        let owner = Owner(std::rc::Rc::clone(&cell));
        owner.bind(&host.ingress, &lifecycle);
        host.mounts.push(Box::new(owner));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(transaction::commit(host.parts(), Mutation::Structural));
        }));
        if phase == 0 {
            assert!(result.is_ok());
        } else {
            let panic = result.expect_err("selected callback phase must panic");
            let message = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
            let expected = [
                "",
                "assertion `left != right` failed: identity panic",
                "assertion `left != right` failed: constructor panic",
                "assertion `left != right` failed: selected value panic",
                "assertion `left != right` failed: factory panic",
                "dependency default panic",
                "path selector panic",
            ][phase];
            assert_eq!(
                message.and_then(|message| message.lines().next()),
                Some(expected)
            );
        }
        drop(host);
        assert_eq!(
            drops.get(),
            4,
            "phase {phase}: capture destruction is exactly once"
        );
        assert!(cell.payload.borrow().is_none());
    }
}

// Catches local header lookup borrowing payload and same-turn recursive acquisition.
#[test]
fn local_reentrant_acquisition_queues_until_payload_restored() {
    /// Keeps the same-actor acquisition pending until the ordered mailbox call executes.
    type Pending = Pin<
        Box<
            dyn Future<
                Output = Result<
                    Option<local::TaskContext<Rc<Counter<usize>>, Rc<u8>>>,
                    TaskAccessError,
                >,
            >,
        >,
    >;
    let header = Rc::new(OnceCell::<(
        LocalMountSite<Option<Rc<Counter<usize>>>, Rc<Counter<usize>>, Rc<u8>, Rc<()>>,
        Arc<()>,
    )>::new());
    let pending = Rc::new(RefCell::new(None::<Pending>));
    let callback_header = std::rc::Rc::clone(&header);
    let callback_pending = std::rc::Rc::clone(&pending);
    let descriptor = LocalOptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        move |state: &Option<Rc<Counter<usize>>>| {
            let (site, root) = callback_header.get().unwrap();
            let mut foreign = Box::pin(site.acquire(Arc::new(())));
            assert!(
                matches!(foreign.as_mut().poll(&mut Context::from_waker(Waker::noop())), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::ScopeEnded)
            );
            let mut own: Pending = Box::pin(site.acquire(std::sync::Arc::clone(root)));
            assert!(
                own.as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            *callback_pending.borrow_mut() = Some(own);
            state.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (owner, site) = attach_local(descriptor);
    let mut host = Host::<_, _, Local>::new(
        Some(Rc::new(Counter::new(1))),
        Box::new(local::Driver(Root(Rc::new(())))),
    );
    let lifecycle = std::sync::Arc::clone(&host.lifecycle);
    owner.bind(&host.ingress, &lifecycle);
    header
        .set((site, host.scopes.root_identity()))
        .as_ref()
        .expect("local mount header is installed once");
    host.mounts.push(owner);
    drop(transaction::commit(host.parts(), Mutation::Structural));
    let Command::Acquire(call) = host.receiver.try_recv().unwrap() else {
        panic!("ordered local acquisition")
    };
    call.execute(&host.scopes, &lifecycle);
    let mut pending = pending.borrow_mut().take().unwrap();
    assert!(matches!(
        pending
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(Some(_)))
    ));
}

// The local carrier uses the same live-phase checks without Send callbacks or
// a second transaction implementation.
#[test]
fn local_closure_between_callback_phases_prevents_initial_registration() {
    use crate::store::lifecycle::CloseState;
    for close_at in 0..4 {
        let mut host = Host::<_, _, Local>::new(
            Some(Rc::new(Counter::new(1))),
            Box::new(local::Driver(Root(Rc::new(())))),
        );
        let lifecycle = std::sync::Arc::clone(&host.lifecycle);
        let calls = Rc::new(RefCell::new(Vec::new()));
        let enter = |phase| {
            let lifecycle = std::sync::Arc::clone(&lifecycle);
            let calls = std::rc::Rc::clone(&calls);
            move || {
                calls.borrow_mut().push(phase);
                if phase == close_at {
                    lifecycle.close(CloseState::Abortive);
                }
            }
        };
        let identity = enter(0);
        let make = enter(1);
        let select = enter(2);
        let factory = enter(3);
        let descriptor = LocalOptionalMount::new(
            Option::as_ref,
            Option::as_mut,
            move |state: &Option<Rc<Counter<usize>>>| {
                identity();
                state.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| {
                make();
                Child
            },
        )
        .on_initial(
            move |state| {
                select();
                std::rc::Rc::clone(state)
            },
            move |_, _| {
                factory();
                local::Task::run(|_| async { panic!("closed Initial body") })
            },
        );
        let (owner, _site) = attach_local(descriptor);
        owner.bind(&host.ingress, &lifecycle);
        host.mounts.push(owner);
        let turn = transaction::commit(host.parts(), Mutation::Structural);
        assert!(turn.launches.into_iter().next().is_none());
        assert_eq!(*calls.borrow(), (0..=close_at).collect::<Vec<_>>());
        assert!(lifecycle.is_closed());
    }
}

#[test]
fn local_committed_handler_requires_its_live_installed_scope_and_root() {
    let descriptor = LocalOptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<Rc<Counter<usize>>>| {
            state.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (owner, site) = attach_local(descriptor);
    let mut host = Host::<_, _, Local>::new(
        Some(Rc::new(Counter::new(1))),
        Box::new(local::Driver(Root(Rc::new(())))),
    );
    let registration = site.handler_registration().upgrade().unwrap();
    let root = host.scopes.root();
    assert!(registration.freeze_handler_route(&host.scopes).is_none());

    owner.bind(&host.ingress, &host.lifecycle);
    host.mounts.push(owner);
    assert!(registration.freeze_handler_route(&host.scopes).is_none());

    drop(transaction::commit(host.parts(), Mutation::Structural));
    let installed = host.mounts[0].installed_scope().unwrap();
    let frozen = registration
        .freeze_handler_route(&host.scopes)
        .map(|route| route.registration().scope);
    assert_eq!(frozen, Some(installed));
    assert_ne!(frozen, Some(root));

    let foreign = Host::<_, _, Local>::new(None, Box::new(local::Driver(Root(Rc::new(())))));
    assert!(registration.freeze_handler_route(&foreign.scopes).is_none());

    host.state = None;
    drop(transaction::commit(host.parts(), Mutation::Structural));
    assert!(!host.scopes.is_current(installed));
    assert!(registration.freeze_handler_route(&host.scopes).is_none());

    host.state = Some(Rc::new(Counter::new(2)));
    drop(transaction::commit(host.parts(), Mutation::Structural));
    let replacement = host.mounts[0].installed_scope().unwrap();
    let frozen = registration
        .freeze_handler_route(&host.scopes)
        .map(|route| route.registration().scope);
    assert_eq!(frozen, Some(replacement));
    assert_ne!(frozen, Some(installed));

    let lifecycle = Arc::downgrade(&host.lifecycle);
    drop(host);
    assert!(lifecycle.upgrade().is_none());
    assert!(registration.freeze_handler_route(&foreign.scopes).is_none());
}

#[test]
fn local_closed_path_selection_leaves_no_committed_handler_node() {
    use crate::store::lifecycle::{CloseState, Lifecycle};

    struct State {
        /// Remains selected even after the path callback closes the actor.
        child: Option<Rc<Counter<usize>>>,
        /// Lets the read selector close the real actor lifetime during reconciliation.
        lifecycle: Option<Arc<Lifecycle>>,
    }
    struct ClosingRoot;
    impl local::Feature for ClosingRoot {
        type State = State;
        type Action = Rc<()>;
        fn update(
            &mut self,
            _: &mut State,
            _: &Rc<()>,
            _: &mut local::FeatureContext<'_, State, Rc<()>>,
        ) {
        }
    }
    /// Closes the actor before returning an otherwise valid selected child.
    fn read(state: &State) -> Option<&Rc<Counter<usize>>> {
        state
            .lifecycle
            .as_ref()
            .unwrap()
            .close(CloseState::Abortive);
        state.child.as_ref()
    }
    fn write(state: &mut State) -> Option<&mut Rc<Counter<usize>>> {
        state.child.as_mut()
    }

    let descriptor = LocalOptionalMount::new(
        read,
        write,
        |state: &State| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (owner, site) = attach_local(descriptor);
    let mut host = Host::<_, _, Local>::new(
        State {
            child: Some(Rc::new(Counter::new(1))),
            lifecycle: None,
        },
        Box::new(local::Driver(ClosingRoot)),
    );
    host.state.lifecycle = Some(Arc::clone(&host.lifecycle));
    owner.bind(&host.ingress, &host.lifecycle);
    host.mounts.push(owner);
    let registration = site.handler_registration().upgrade().unwrap();

    drop(transaction::commit(host.parts(), Mutation::Structural));
    let installed = host.mounts[0].installed_scope().unwrap();
    assert!(host.lifecycle.is_closed());
    assert!(host.scopes.is_current(installed));
    assert!(host.mounted.is_empty());
    assert!(registration.freeze_handler_route(&host.scopes).is_none());

    // Shutdown drops the real owner while its retained header remains readable.
    drop(host.mounts.remove(0));
    assert!(host.scopes.is_current(installed));
    assert!(registration.installed().is_none());
    assert!(registration.freeze_handler_route(&host.scopes).is_none());
}
