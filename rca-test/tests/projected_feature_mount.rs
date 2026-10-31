//! Mounted feature tests retain non-`Sync` projected state and private feature data.

use std::cell::Cell;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures_executor::block_on;
use rca_core::{
    Feature, FeatureContext, MountIdentity, MountSite, OptionalMount, Store, StoreBuilder, Task,
};
use rca_test::{DeterministicRuntime, TestStore, TestStoreBuilder};

/// `Cell` fields make accidental `Sync` requirements visible at compile time.
struct State {
    value: Cell<usize>,
    child: Option<Cell<usize>>,
}
struct Action(Cell<usize>);
/// Its private counter distinguishes two identical public actions.
struct Root(Cell<usize>);
impl Feature for Root {
    type State = State;
    type Action = Action;
    fn update(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut FeatureContext<'_, State, Action>,
    ) {
        self.0.set(self.0.get() + 1);
        state
            .value
            .set(state.value.get() + action.0.get() + self.0.get());
    }
}
struct Child;
impl Feature for Child {
    type State = Cell<usize>;
    type Action = ();
    fn update(
        &mut self,
        _: &mut Cell<usize>,
        (): &(),
        _: &mut FeatureContext<'_, Cell<usize>, ()>,
    ) {
    }
}
fn state() -> State {
    State {
        value: Cell::new(0),
        child: Some(Cell::new(2)),
    }
}
fn snapshot(state: &State) -> (usize, Option<usize>) {
    (state.value.get(), state.child.as_ref().map(Cell::get))
}
/// Retains private construction state and exposes each actual child installation.
fn descriptor(constructions: Arc<AtomicUsize>) -> OptionalMount<State, Child, ()> {
    let private = Cell::new(0);
    OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            private.set(private.get() + 1);
            constructions.fetch_add(1, Ordering::Relaxed);
            Child
        },
    )
}

// Catches a reducer-only adapter, accidental State/Feature Sync bounds, eager
// installation, duplicate attachment and loss of private Feature state.
#[test]
fn projected_feature_mount_preserves_gated_startup_and_private_state() {
    fn send<T: Send>(_: &T) {}
    fn send_sync<T: Send + Sync>(_: &T) {}
    block_on(async {
        let constructions = Arc::new(AtomicUsize::new(0));
        let initials = Arc::new(AtomicUsize::new(0));
        let initial_count = Arc::clone(&initials);
        let mount =
            descriptor(Arc::clone(&constructions)).on_initial(Cell::get, move |change, _| {
                initial_count.fetch_add(1, Ordering::Relaxed);
                let rca_core::ReactionChange::Initial { current } = change else {
                    panic!("Initial hook")
                };
                Task::run(move |context| async move {
                    context
                        .update(move |child: &mut Cell<usize>| child.set(child.get() + current))
                        .await
                        .unwrap();
                })
            });
        let (builder, site) = TestStore::projected_builder(state(), Root(Cell::new(0)), snapshot)
            .mount_with_site(mount);
        send(&builder);
        send_sync(&site);
        let mut store = builder.build();
        assert_eq!(constructions.load(Ordering::Relaxed), 0);
        assert_eq!(initials.load(Ordering::Relaxed), 0);
        let initial = store.receive_structural(|_| {}).await.unwrap();
        assert_eq!(constructions.load(Ordering::Relaxed), 1);
        assert_eq!(initials.load(Ordering::Relaxed), 1);
        let _ = store
            .receive_update(|expected| expected.1 = Some(4))
            .await
            .unwrap();
        initial.finish().await.unwrap();
        store
            .send_with(Action(Cell::new(3)), |expected| expected.0 = 4)
            .await
            .unwrap()
            .finish()
            .await
            .unwrap();
        store
            .send_with(Action(Cell::new(3)), |expected| expected.0 = 9)
            .await
            .unwrap()
            .finish()
            .await
            .unwrap();
        assert_eq!(constructions.load(Ordering::Relaxed), 1);
        assert_eq!(initials.load(Ordering::Relaxed), 1);
        store.finish().await.unwrap();
        drop(site);
    });
}

// Catches returning a placeholder or a site from a discarded second declaration.
#[test]
fn deferred_attachment_site_acquires_the_installed_production_child() {
    block_on(async {
        let runtime = DeterministicRuntime::new();
        let constructions = Arc::new(AtomicUsize::new(0));
        let (attach, site) = StoreBuilder::<State, Action, Root>::defer_mount(descriptor(
            Arc::clone(&constructions),
        ));
        assert_eq!(constructions.load(Ordering::Relaxed), 0);
        let store = attach(Store::builder(
            state(),
            Root(Cell::new(0)),
            runtime.runtime(),
        ))
        .build()
        .unwrap();
        runtime.run(store.ready()).await.unwrap().unwrap();
        let child = runtime
            .run(store.acquire(&site))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            runtime.run(child.read(Cell::get)).await.unwrap().unwrap(),
            2
        );
        assert_eq!(constructions.load(Ordering::Relaxed), 1);
        drop(store);
        runtime
            .run(child.read(Cell::get))
            .await
            .unwrap()
            .unwrap_err();
    });
}

// Catches a site retaining the declaration owner or leaking its deferred closure.
#[test]
fn retained_site_does_not_keep_dropped_builder_captures_alive() {
    type Builder = TestStoreBuilder<State, Action, Root, (usize, Option<usize>)>;
    type Site = MountSite<State, Cell<usize>, (), Action>;
    let resource = Arc::new(());
    let weak = Arc::downgrade(&resource);
    let mount = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            let () = resource.as_ref();
            Child
        },
    );
    let (builder, site): (Builder, Site) =
        TestStore::projected_builder(state(), Root(Cell::new(0)), snapshot).mount_with_site(mount);
    assert!(weak.upgrade().is_some());
    drop(builder);
    assert!(weak.upgrade().is_none());
    drop(site);
}
