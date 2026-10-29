//! Exercises real declaration leases, header admission and outside-guard destruction.

use super::*;
use crate::store::transaction::tests::Host;
use crate::store::transaction::{self, Mutation};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

/// Leaves optional state unchanged during structural lease checks.
struct Root;
impl Feature for Root {
    type State = Option<usize>;
    type Action = ();
    fn reduce(
        &mut self,
        _: &mut Self::State,
        (): &(),
        _: &mut crate::FeatureContext<'_, Self::State, ()>,
    ) {
    }
}
/// Supplies a concrete installed child without additional reducer work.
struct Child;
impl Feature for Child {
    type State = usize;
    type Action = u8;
    fn reduce(&mut self, _: &mut usize, _: &u8, _: &mut crate::FeatureContext<'_, usize, u8>) {}
}
/// Names the actual declaration cell inspected by the guard probe.
type Cell = DeclarationCell<Option<usize>, (), Child, ()>;
/// Checks callback ownership without retaining the declaration payload itself.
struct Probe {
    cell: Arc<OnceLock<Weak<Cell>>>,
    drops: Arc<AtomicUsize>,
}
impl Probe {
    /// Requires an available guard and a payload leased out to the actor.
    fn check(&self) {
        let cell = self.cell.get().unwrap().upgrade().unwrap();
        let guard = cell
            .payload
            .try_lock()
            .expect("user callback/drop runs outside declaration guard");
        assert!(guard.is_none(), "actor owns the leased payload");
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.check();
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
/// Polls an explicitly driven step without testing executor wake delivery.
fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

// Catches holding a declaration guard across callbacks, unwind or final-owner drain.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keep all lease panic stages and disposal assertions in one ownership scenario"
)]
fn shared_declaration_callbacks_and_panic_destruction_run_with_available_guard() {
    /// Panics only when the reaction enters dependency resolution.
    struct PanicDependency;
    impl crate::DependencyKey for PanicDependency {
        type Value = ();
        fn live_value() {
            panic!("dependency default panic")
        }
    }
    for phase in 0..=6 {
        let cell_slot = Arc::new(OnceLock::new());
        let drops = Arc::new(AtomicUsize::new(0));
        let probe = || Probe {
            cell: std::sync::Arc::clone(&cell_slot),
            drops: std::sync::Arc::clone(&drops),
        };
        let identity_probe = probe();
        let constructor_probe = probe();
        let selector_probe = probe();
        let factory_probe = probe();
        let descriptor = OptionalMount::new(
            if phase == 6 {
                |_| panic!("path selector panic")
            } else {
                Option::as_ref
            },
            Option::as_mut,
            move |state: &Option<usize>| {
                identity_probe.check();
                assert_ne!(phase, 1, "identity panic");
                state.map(|_| MountIdentity {
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
                *state
            },
            move |_, mut context| {
                factory_probe.check();
                assert_ne!(phase, 4, "factory panic");
                if phase == 5 {
                    context.dependency::<PanicDependency>();
                }
                Task::run(|_| async {})
            },
        );
        let cell = Arc::new(DeclarationCell {
            node: ScopeRegistry::node(&mut ScopeSlot::new()),
            binding: OnceLock::new(),
            payload: Mutex::new(Some(Declaration {
                descriptor: descriptor.into(),
                current: None,
                installed: None,
                initial_pending: false,
                mapping: (),
            })),
        });
        cell_slot.set(Arc::downgrade(&cell)).unwrap();
        let mut host = Host::<_, _, Shared>::new(Some(1), Box::new(Root));
        let lifecycle = std::sync::Arc::clone(&host.lifecycle);
        let owner = DeclarationOwner(std::sync::Arc::clone(&cell));
        owner.bind(&host.ingress, &lifecycle);
        host.mounts.push(Box::new(owner));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(transaction::commit(host.parts(), Mutation::Structural));
        }));
        assert_eq!(outcome.is_err(), phase != 0);
        if let Err(payload) = outcome {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .expect("the injected panic carries text");
            let expected = [
                "no panic in the successful control",
                "assertion `left != right` failed: identity panic",
                "assertion `left != right` failed: constructor panic",
                "assertion `left != right` failed: selected value panic",
                "assertion `left != right` failed: factory panic",
                "dependency default panic",
                "path selector panic",
            ][phase];
            assert_eq!(message.lines().next(), Some(expected), "phase {phase}");
        }
        if phase == 0 {
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        }
        drop(host);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            4,
            "phase {phase}: each capture destroyed once"
        );
        assert!(
            cell.payload.lock().is_none(),
            "retained header reader cannot retain callbacks"
        );
    }
}

// Catches wrong-root validation entering a leased payload or recursive same-root dispatch.
#[test]
fn reentrant_acquisition_uses_header_and_waits_for_actor_restoration() {
    /// Retains the reentrant query until the actor restores its payload.
    type Pending = Pin<
        Box<dyn Future<Output = Result<Option<TaskContext<usize, u8>>, TaskAccessError>> + Send>,
    >;
    let header = Arc::new(OnceLock::<(MountSite<Option<usize>, usize, u8, ()>, Arc<()>)>::new());
    let pending = Arc::new(Mutex::new(None::<Pending>));
    let callback_header = std::sync::Arc::clone(&header);
    let callback_pending = std::sync::Arc::clone(&pending);
    let descriptor = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        move |state: &Option<usize>| {
            let (site, root) = callback_header.get().unwrap();
            let mut foreign = Box::pin(site.acquire(Arc::new(())));
            assert!(
                matches!(poll(foreign.as_mut()), Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::ScopeEnded)
            );
            let mut own: Pending = Box::pin(site.acquire(std::sync::Arc::clone(root)));
            assert!(
                own.as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            *callback_pending.lock() = Some(own);
            state.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (owner, site) = attach(descriptor);
    let mut host = Host::<_, _, Shared>::new(Some(1), Box::new(Root));
    let lifecycle = std::sync::Arc::clone(&host.lifecycle);
    owner.bind(&host.ingress, &lifecycle);
    header.set((site, host.scopes.root_identity())).unwrap();
    host.mounts.push(owner);
    drop(transaction::commit(host.parts(), Mutation::Structural));
    let Command::Acquire(call) = host.receiver.try_recv().unwrap() else {
        panic!("ordered acquisition")
    };
    call.execute(&host.scopes, &lifecycle);
    let mut pending = pending.lock().take().unwrap();
    assert!(matches!(
        pending
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(Some(_)))
    ));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One ordered replacement proves queued acquisition, header ownership and callback disposal."
)]
fn queued_acquisition_rejects_retired_nested_payload_with_a_retained_header() {
    /// Replaces the identity-bearing value at the earlier actor position.
    struct ReplaceRoot;
    impl Feature for ReplaceRoot {
        type State = Option<usize>;
        type Action = Option<usize>;
        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            _: &mut crate::FeatureContext<'_, Self::State, Self::Action>,
        ) {
            *state = *action;
        }
    }
    /// Records the nested constructor capture's disposal independently of its header.
    struct DropProbe(Arc<AtomicUsize>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let sites = Arc::new(Mutex::new(Vec::new()));
    let published = Arc::clone(&sites);
    let calls = Arc::new(AtomicUsize::new(0));
    let callbacks = Arc::clone(&calls);
    let descriptor = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<usize>| {
            state.map(|key| MountIdentity {
                key,
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (owner, _) = children::attach(descriptor, move |_, children| {
        let drops = Arc::new(AtomicUsize::new(0));
        let probe = DropProbe(Arc::clone(&drops));
        let identities = Arc::clone(&callbacks);
        let constructors = Arc::clone(&callbacks);
        let site = children.optional(OptionalMount::new(
            |state| Some(state),
            |state| Some(state),
            move |_: &usize| {
                identities.fetch_add(1, Ordering::SeqCst);
                Some(MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| {
                let _ = &probe;
                constructors.fetch_add(1, Ordering::SeqCst);
                Child
            },
        ));
        published.lock().push((site, drops));
    });
    let mut host = Host::<_, _, Shared>::new(Some(7), Box::new(ReplaceRoot));
    owner.bind(&host.ingress, &host.lifecycle);
    host.mounts.push(owner);
    drop(transaction::commit(host.parts(), Mutation::Structural));
    let (site, drops) = sites.lock()[0].clone();
    let header = site.fold_registration().upgrade().unwrap();
    let old_scope = header.installed().unwrap().1;
    let weak = Arc::downgrade(&header);

    // This models a Shared caller preempted after MountSite::acquire upgrades
    // its declaration but before Acquisition::acquire drops that header Arc.
    let before = std::thread::scope(|scope| {
        let (held, holding) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let reader = scope.spawn(move || {
            held.send(()).unwrap();
            // An earlier assertion drops `release` before the scope joins us.
            // Channel closure releases the header without a second panic.
            let _ = released.recv();
            drop(header);
        });
        holding.recv().unwrap();

        // Reserve the earlier actor head before admitting the acquisition behind it.
        let (reply, mut retirement) = crate::store::command::commit_channel();
        host.ingress
            .try_send(Command::Action(
                crate::store::command::ActionCommand::external(Some(9), reply),
            ))
            .unwrap_or_else(|_| panic!("replacement enters the empty mailbox"));
        let Command::Action(replace) = host.receiver.try_recv().unwrap() else {
            panic!("replacement is the earlier actor head");
        };
        let mut acquisition = Box::pin(site.acquire(host.scopes.root_identity()));
        assert!(poll(acquisition.as_mut()).is_pending());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(transaction::commit(
            host.parts(),
            Mutation::Action(replace, None),
        ));
        assert!(matches!(retirement.try_recv(), Ok(Some(Ok(_)))));
        assert_eq!(host.state, Some(9));
        assert!(!host.scopes.is_current(old_scope));
        assert_eq!(sites.lock().len(), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(weak.upgrade().is_some());
        host.lifecycle.access().unwrap();
        let before = calls.load(Ordering::SeqCst);
        let Command::Acquire(call) = host.receiver.try_recv().unwrap() else {
            panic!("acquisition remains behind the committed replacement");
        };
        call.execute(&host.scopes, &host.lifecycle);
        let Poll::Ready(Err(error)) = poll(acquisition.as_mut()) else {
            panic!("retired payload must reject rather than retarget the replacement");
        };
        assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
        assert_eq!(calls.load(Ordering::SeqCst), before);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(acquisition);
        release.send(()).unwrap();
        reader.join().unwrap();
        before
    });
    assert!(weak.upgrade().is_none());

    let (replacement, replacement_drops) = sites.lock()[1].clone();
    let mut acquisition = Box::pin(replacement.acquire(host.scopes.root_identity()));
    assert!(poll(acquisition.as_mut()).is_pending());
    let Command::Acquire(call) = host.receiver.try_recv().unwrap() else {
        panic!("fresh replacement acquisition enters the mailbox");
    };
    call.execute(&host.scopes, &host.lifecycle);
    assert!(matches!(
        poll(acquisition.as_mut()),
        Poll::Ready(Ok(Some(_)))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), before);
    assert_eq!(replacement_drops.load(Ordering::SeqCst), 0);
    drop(host);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(replacement_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn committed_handler_requires_its_live_installed_scope_and_root() {
    let descriptor = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<usize>| {
            state.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (owner, site) = attach(descriptor);
    let mut host = Host::<_, _, Shared>::new(Some(1), Box::new(Root));
    let registration = site.handler_registration().upgrade().unwrap();
    let output_registration = site.output_registration().upgrade().unwrap();
    let acquisition = site.fold_registration().upgrade().unwrap();
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
    let (installed_root, installed_scope) = output_registration.installed().unwrap();
    assert!(Arc::ptr_eq(&installed_root, &host.scopes.root_identity()));
    assert_eq!(installed_scope, installed);
    assert!(Arc::ptr_eq(
        &acquisition.root_identity().unwrap(),
        &host.scopes.root_identity()
    ));

    let foreign = Host::<_, _, Shared>::new(Some(1), Box::new(Root));
    assert!(registration.freeze_handler_route(&foreign.scopes).is_none());

    host.state = None;
    drop(transaction::commit(host.parts(), Mutation::Structural));
    assert!(!host.scopes.is_current(installed));
    assert!(registration.freeze_handler_route(&host.scopes).is_none());

    host.state = Some(2);
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
    // Temporary Shared readers retain the declaration header, not its lifecycle.
    assert!(output_registration.installed().is_none());
    assert!(acquisition.root_identity().is_none());
    assert!(registration.freeze_handler_route(&foreign.scopes).is_none());
}

#[test]
fn committed_read_after_concurrent_close_suppresses_first_selector() {
    use crate::store::lifecycle::CloseState;

    struct State {
        child: Option<usize>,
        selections: AtomicUsize,
    }
    struct Parent;
    impl Feature for Parent {
        type State = State;
        type Action = ();
        fn reduce(&mut self, _: &mut State, (): &(), _: &mut crate::FeatureContext<'_, State, ()>) {
        }
    }
    let descriptor = OptionalMount::new(
        |state: &State| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.child.as_ref()
        },
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (owner, site) = attach(descriptor);
    let mut host = Host::<_, _, Shared>::new(
        State {
            child: Some(7),
            selections: AtomicUsize::new(0),
        },
        Box::new(Parent),
    );
    owner.bind(&host.ingress, &host.lifecycle);
    host.mounts.push(owner);
    drop(transaction::commit(host.parts(), Mutation::Structural));
    let installed = host.mounts[0].installed_scope().unwrap();
    assert!(host.scopes.is_current(installed));
    let registration = site.fold_registration().upgrade().unwrap();
    host.state.selections.store(0, Ordering::SeqCst);

    // Another owner can close admission while the actor still owns this roster.
    let lifecycle = Arc::clone(&host.lifecycle);
    std::thread::spawn(move || lifecycle.close(CloseState::Abortive))
        .join()
        .unwrap();
    assert!(
        registration
            .read_committed(&host.state, &|| host.lifecycle.access().is_ok())
            .is_none()
    );
    assert_eq!(host.state.selections.load(Ordering::SeqCst), 0);
    assert_eq!(host.state.child, Some(7));
    assert!(host.scopes.is_current(installed));
}

#[test]
fn closed_path_selection_leaves_no_committed_handler_node() {
    use crate::store::lifecycle::{CloseState, Lifecycle};

    struct State {
        child: Option<usize>,
        lifecycle: Option<Arc<Lifecycle>>,
    }
    struct ClosingRoot;
    impl Feature for ClosingRoot {
        type State = State;
        type Action = ();
        fn reduce(&mut self, _: &mut State, (): &(), _: &mut crate::FeatureContext<'_, State, ()>) {
        }
    }
    fn read(state: &State) -> Option<&usize> {
        state
            .lifecycle
            .as_ref()
            .unwrap()
            .close(CloseState::Abortive);
        state.child.as_ref()
    }
    fn write(state: &mut State) -> Option<&mut usize> {
        state.child.as_mut()
    }

    let descriptor = OptionalMount::new(
        read,
        write,
        |state: &State| {
            state.child.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (owner, site) = attach(descriptor);
    let mut host = Host::<_, _, Shared>::new(
        State {
            child: Some(1),
            lifecycle: None,
        },
        Box::new(ClosingRoot),
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

/// Separates stable child identity from callback-controlled path visibility.
struct SelectableState {
    child: usize,
    visible: Arc<std::sync::atomic::AtomicBool>,
}

/// Changes visibility without replacing the child identity.
struct SelectableRoot;

impl Feature for SelectableRoot {
    type State = SelectableState;
    type Action = bool;

    fn reduce(
        &mut self,
        state: &mut SelectableState,
        action: &bool,
        _: &mut crate::FeatureContext<'_, SelectableState, bool>,
    ) {
        state.visible.store(*action, Ordering::SeqCst);
    }
}

/// Preserves identity while selectors can stop exposing the child.
fn selectable_mount(
    make: impl FnMut(&usize) -> Child + Send + 'static,
) -> OptionalMount<SelectableState, Child, ()> {
    OptionalMount::new(
        |state: &SelectableState| state.visible.load(Ordering::SeqCst).then_some(&state.child),
        |state| {
            state
                .visible
                .load(Ordering::SeqCst)
                .then_some(&mut state.child)
        },
        |_| {
            Some(MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        make,
    )
}

#[test]
fn child_hook_path_disappearance_after_constructor_skips_initial_callbacks() {
    for singleton in [true, false] {
        let visible = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let constructor_visible = Arc::clone(&visible);
        let selected = Arc::new(AtomicUsize::new(0));
        let selector_count = Arc::clone(&selected);
        let descriptor = selectable_mount(move |_| {
            constructor_visible.store(false, Ordering::SeqCst);
            Child
        });
        let select = move |state: &usize| {
            selector_count.fetch_add(1, Ordering::SeqCst);
            *state
        };
        let factory =
            |_: ReactionChange<usize>, _: ReactionContext<'_, usize, u8>| -> Task<usize, u8> {
                panic!("hidden child must not enter its initial factory")
            };
        let descriptor = if singleton {
            descriptor.on_initial(select, factory)
        } else {
            descriptor.on_change(select, InitialPolicy::Run, factory)
        };
        let (owner, _) = attach(descriptor);
        let mut host = Host::<_, _, Shared>::new(
            SelectableState { child: 7, visible },
            Box::new(SelectableRoot),
        );
        owner.bind(&host.ingress, &host.lifecycle);
        host.mounts.push(owner);

        drop(transaction::commit(host.parts(), Mutation::Structural));

        assert_eq!(selected.load(Ordering::SeqCst), 0);
        assert_eq!(host.mounted.len(), 1);
        assert_eq!(host.version, 1);
        host.lifecycle.access().unwrap();
    }
}

#[test]
fn child_hook_path_disappearance_after_earlier_factory_skips_later_initial() {
    let visible = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let factory_visible = Arc::clone(&visible);
    let factories = Arc::new(AtomicUsize::new(0));
    let first_factory = Arc::clone(&factories);
    let descriptor = selectable_mount(|_| Child)
        .on_initial(
            |state| *state,
            move |_, _| {
                first_factory.fetch_add(1, Ordering::SeqCst);
                factory_visible.store(false, Ordering::SeqCst);
                Task::run(|_| async {})
            },
        )
        .on_change(
            |_| panic!("hidden child must not enter a later initial selector"),
            InitialPolicy::Run,
            |_: ReactionChange<()>, _| panic!("hidden child has no current value"),
        );
    let (owner, _) = attach(descriptor);
    let mut host = Host::<_, _, Shared>::new(
        SelectableState { child: 7, visible },
        Box::new(SelectableRoot),
    );
    owner.bind(&host.ingress, &host.lifecycle);
    host.mounts.push(owner);

    drop(transaction::commit(host.parts(), Mutation::Structural));

    assert_eq!(factories.load(Ordering::SeqCst), 1);
    assert_eq!(host.version, 1);
    host.lifecycle.access().unwrap();
}

#[test]
fn child_hook_path_disappearance_after_capture_releases_previous_without_factory() {
    /// Counts disposal of a captured value whose matching after-value disappears.
    struct Selected {
        value: usize,
        drops: Arc<AtomicUsize>,
    }

    impl PartialEq for Selected {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    impl Drop for Selected {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    let selected = Arc::new(AtomicUsize::new(0));
    let selector_count = Arc::clone(&selected);
    let drops = Arc::new(AtomicUsize::new(0));
    let selected_drops = Arc::clone(&drops);
    let descriptor = selectable_mount(|_| Child).on_change(
        move |state| {
            selector_count.fetch_add(1, Ordering::SeqCst);
            Selected {
                value: *state,
                drops: Arc::clone(&selected_drops),
            }
        },
        InitialPolicy::Skip,
        |_, _| panic!("hidden child has no current value to compare"),
    );
    let (owner, _) = attach(descriptor);
    let mut host = Host::<_, _, Shared>::new(
        SelectableState {
            child: 7,
            visible: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        },
        Box::new(SelectableRoot),
    );
    owner.bind(&host.ingress, &host.lifecycle);
    host.mounts.push(owner);
    drop(transaction::commit(host.parts(), Mutation::Structural));
    assert_eq!(selected.load(Ordering::SeqCst), 0);

    let (turn, mut reply) = host.action(false);
    drop(turn);

    assert!(matches!(reply.try_recv(), Ok(Some(Ok(_)))));
    assert_eq!(selected.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(host.version, 2);
    host.lifecycle.access().unwrap();
}

#[test]
fn mount_selection_disappearance_before_construction_leaves_no_partial_installation() {
    let hide = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let identity_hide = Arc::clone(&hide);
    let identity_owner = Arc::new(());
    let identity_capture = Arc::clone(&identity_owner);
    let constructions = Arc::new(AtomicUsize::new(0));
    let constructor_count = Arc::clone(&constructions);
    let descriptor = OptionalMount::new(
        |state: &SelectableState| state.visible.load(Ordering::SeqCst).then_some(&state.child),
        |state| {
            state
                .visible
                .load(Ordering::SeqCst)
                .then_some(&mut state.child)
        },
        move |state: &SelectableState| {
            if identity_hide.load(Ordering::SeqCst) {
                state.visible.store(false, Ordering::SeqCst);
            }
            Some(MountIdentity {
                key: Arc::clone(&identity_capture),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            constructor_count.fetch_add(1, Ordering::SeqCst);
            Child
        },
    );
    let (owner, site) = attach(descriptor);
    let mut host = Host::<_, _, Shared>::new(
        SelectableState {
            child: 7,
            visible: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        },
        Box::new(SelectableRoot),
    );
    owner.bind(&host.ingress, &host.lifecycle);
    host.mounts.push(owner);

    drop(transaction::commit(host.parts(), Mutation::Structural));

    assert_eq!(constructions.load(Ordering::SeqCst), 0);
    assert!(host.mounted.is_empty());
    assert!(host.mounts[0].installed_scope().is_none());
    assert_eq!(Arc::strong_count(&identity_owner), 2);
    host.lifecycle.access().unwrap();
    let mut absent = Box::pin(site.acquire(host.scopes.root_identity()));
    assert!(poll(absent.as_mut()).is_pending());
    let Command::Acquire(call) = host.receiver.try_recv().unwrap() else {
        panic!("ordered acquisition of an absent declaration");
    };
    call.execute(&host.scopes, &host.lifecycle);
    assert!(matches!(poll(absent.as_mut()), Poll::Ready(Ok(None))));

    hide.store(false, Ordering::SeqCst);
    host.state.visible.store(true, Ordering::SeqCst);
    drop(transaction::commit(host.parts(), Mutation::Structural));

    assert_eq!(constructions.load(Ordering::SeqCst), 1);
    assert_eq!(host.mounted.len(), 1);
    let token = host.mounts[0].installed_scope().unwrap();
    assert!(host.scopes.is_current(token));
    let mut present = Box::pin(site.acquire(host.scopes.root_identity()));
    assert!(poll(present.as_mut()).is_pending());
    let Command::Acquire(call) = host.receiver.try_recv().unwrap() else {
        panic!("ordered acquisition after deferred construction");
    };
    call.execute(&host.scopes, &host.lifecycle);
    assert!(matches!(poll(present.as_mut()), Poll::Ready(Ok(Some(_)))));
    drop(host);
    assert_eq!(Arc::strong_count(&identity_owner), 1);
}
