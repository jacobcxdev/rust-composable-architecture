//! Command acquisition obeys bounded admission without retaining Store ownership.

#[expect(dead_code, reason = "admission tests manually poll the actor")]
mod support;

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll, Waker};

#[cfg(feature = "test-util")]
use rca_core::testing::TransactionGate;
use rca_core::{
    CommandSite, Feature, FeatureContext, MountIdentity, OptionalMount, RootMount, RuntimeFuture,
    Store, TaskAccessReason, TaskProgress,
};

use support::ManualRuntime;

/// Records root reduction without retaining an external Store owner.
struct Root {
    callbacks: Arc<AtomicUsize>,
    _owner: Arc<()>,
}

impl Feature for Root {
    type State = Option<usize>;
    type Action = ();

    fn update(
        &mut self,
        _: &mut Self::State,
        (): &(),
        _: &mut FeatureContext<'_, Self::State, ()>,
    ) {
        self.callbacks.fetch_add(1, Ordering::SeqCst);
    }
}

/// Records child reduction separately from command mapping.
struct Child {
    callbacks: Arc<AtomicUsize>,
    _owner: Arc<()>,
}

impl Feature for Child {
    type State = usize;
    type Action = usize;

    fn update(
        &mut self,
        state: &mut usize,
        action: &usize,
        _: &mut FeatureContext<'_, usize, usize>,
    ) {
        self.callbacks.fetch_add(1, Ordering::SeqCst);
        *state += action;
    }
}

/// Separates the actor, sole Store owner and weak command declaration.
struct Fixture {
    runtime: ManualRuntime,
    store: Store<Option<usize>, ()>,
    site: CommandSite<Option<usize>, usize>,
    actor: RuntimeFuture,
    callbacks: Arc<AtomicUsize>,
    mappings: Arc<AtomicUsize>,
    owner: Weak<()>,
}

/// Installs one real command declaration with a one-slot root mailbox.
fn fixture() -> Fixture {
    let runtime = ManualRuntime::new();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let mappings = Arc::new(AtomicUsize::new(0));
    let owner = Arc::new(());
    let weak_owner = Arc::downgrade(&owner);
    let descriptor = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<usize>| {
            state.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        {
            let callbacks = Arc::clone(&callbacks);
            let owner = Arc::clone(&owner);
            move |_| Child {
                callbacks: Arc::clone(&callbacks),
                _owner: Arc::clone(&owner),
            }
        },
    );
    let (builder, mount, site) = RootMount::new(Root {
        callbacks: Arc::clone(&callbacks),
        _owner: Arc::clone(&owner),
    })
    .builder(Some(7), runtime.runtime())
    .mailbox_capacity(1)
    .mount_with_command(descriptor, {
        let mappings = Arc::clone(&mappings);
        move |command: usize| {
            std::hint::black_box(&owner);
            mappings.fetch_add(1, Ordering::SeqCst);
            command
        }
    });
    let store = builder.build().unwrap();
    let mut actor = runtime.take_only_queued_task();
    assert!(poll(actor.as_mut()).is_pending());
    let mut ready = Box::pin(store.ready());
    assert!(matches!(poll(ready.as_mut()), Poll::Ready(Ok(()))));
    drop(ready);
    drop(mount);
    assert_eq!(callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(mappings.load(Ordering::SeqCst), 0);
    Fixture {
        runtime,
        store,
        site,
        actor,
        callbacks,
        mappings,
        owner: weak_owner,
    }
}

/// Polls only the selected future; the test owns every actor turn.
fn poll<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[cfg(feature = "test-util")]
#[test]
fn idle_gate_command_acquisition_uses_ordinary_admission() {
    let mut fixture = fixture();
    let gate = TransactionGate::new();
    assert!(gate.boundary().is_none());
    let mut acquisition = Box::pin(gate.command_port(&fixture.store, &fixture.site));
    assert!(poll(acquisition.as_mut()).is_pending());
    assert_eq!(fixture.callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.mappings.load(Ordering::SeqCst), 0);
    assert!(poll(fixture.actor.as_mut()).is_pending());
    let Poll::Ready(Ok(Some(port))) = poll(acquisition.as_mut()) else {
        panic!("an idle gate must acquire the current real command route");
    };
    drop(acquisition);
    assert!(gate.boundary().is_none());
    assert!(fixture.owner.upgrade().is_some());

    drop(fixture.store);
    assert!(poll(fixture.actor.as_mut()).is_ready());
    drop(fixture.actor);
    assert!(fixture.owner.upgrade().is_none());
    assert_eq!(fixture.callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.mappings.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.runtime.spawn_count(), 1);
    assert!(fixture.runtime.take_queued_tasks().is_empty());
    drop(port);
}

#[test]
fn pending_command_acquisition_rejects_final_store_owner_loss() {
    let mut fixture = fixture();
    let queued = fixture.store.try_send(()).unwrap();
    assert!(fixture.store.try_send(()).unwrap_err().is_full());
    let mut acquisition = Box::pin(fixture.store.command_port(&fixture.site));
    assert!(poll(acquisition.as_mut()).is_pending());
    assert_eq!(fixture.callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.mappings.load(Ordering::SeqCst), 0);

    drop(fixture.store);
    let Poll::Ready(Err(error)) = poll(acquisition.as_mut()) else {
        panic!("pending admission must return no port after final Store owner loss");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    drop(acquisition);
    assert!(poll(fixture.actor.as_mut()).is_ready());
    drop(fixture.actor);
    assert!(matches!(
        poll(Box::pin(queued.finish()).as_mut()),
        Poll::Ready(Err(_))
    ));
    assert!(fixture.owner.upgrade().is_none());
    assert_eq!(fixture.callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.mappings.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.runtime.spawn_count(), 1);
    assert!(fixture.runtime.take_queued_tasks().is_empty());
}

#[cfg(feature = "test-util")]
#[test]
fn idle_gate_pending_command_acquisition_rejects_final_store_owner_loss() {
    let mut fixture = fixture();
    let gate = TransactionGate::new();
    let queued = fixture.store.try_send(()).unwrap();
    assert!(fixture.store.try_send(()).unwrap_err().is_full());
    assert!(gate.boundary().is_none());
    let mut acquisition = Box::pin(gate.command_port(&fixture.store, &fixture.site));
    assert!(poll(acquisition.as_mut()).is_pending());
    assert!(gate.boundary().is_none());
    assert_eq!(fixture.callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.mappings.load(Ordering::SeqCst), 0);

    drop(fixture.store);
    let Poll::Ready(Err(error)) = poll(acquisition.as_mut()) else {
        panic!("an idle gate must return no port after final Store owner loss");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    drop(acquisition);
    assert!(gate.boundary().is_none());
    assert!(poll(fixture.actor.as_mut()).is_ready());
    drop(fixture.actor);
    assert!(matches!(
        poll(Box::pin(queued.finish()).as_mut()),
        Poll::Ready(Err(_))
    ));
    assert!(fixture.owner.upgrade().is_none());
    assert_eq!(fixture.callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.mappings.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.runtime.spawn_count(), 1);
    assert!(fixture.runtime.take_queued_tasks().is_empty());
}
