//! Actor-driven checks for a parent projection that hides state or closes the root.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Selects what the next armed parent projection does before child discovery.
#[derive(Clone, Copy)]
enum ParentRead {
    Live,
    Hidden,
    Close,
}

/// Carries the last Store owner so a real projection can close the actor.
struct State {
    parent: Option<ParentState>,
    owner: Arc<Mutex<Option<Store<State, ()>>>>,
    /// Defers the chosen read action until construction or reduction arms it.
    armed: Arc<AtomicBool>,
    read: ParentRead,
}

struct ParentState {
    child: Option<Cell<usize>>,
}

/// Applies the armed boundary action before returning the actual parent state.
fn select_parent(state: &State) -> Option<&ParentState> {
    if state.armed.load(Ordering::SeqCst) {
        match state.read {
            ParentRead::Live => {}
            ParentRead::Hidden => return None,
            ParentRead::Close => {
                let last_owner = state.owner.lock().take();
                drop(last_owner);
            }
        }
    }
    state.parent.as_ref()
}

struct Root;
impl Feature for Root {
    type State = State;
    type Action = ();

    fn reduce(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {}
}

/// Arms the next identity read and requests a real scope leave in one reduction.
struct Parent(Arc<AtomicBool>);
impl Feature for Parent {
    type State = ParentState;
    type Action = ();

    fn reduce(
        &mut self,
        _: &mut ParentState,
        (): &(),
        context: &mut FeatureContext<'_, ParentState, ()>,
    ) {
        self.0.store(true, Ordering::SeqCst);
        let context = context.reducer_context();
        context.leave(context.scope_token());
    }
}

/// Counts disposal without retaining the feature or recipe being observed.
struct DropWatch(Arc<AtomicUsize>);
impl Drop for DropWatch {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Separates parent ownership from the recipe's independently counted capture.
struct ObservedParent(DropWatch);
impl Feature for ObservedParent {
    type State = ParentState;
    type Action = ();

    fn reduce(
        &mut self,
        _: &mut ParentState,
        (): &(),
        _: &mut FeatureContext<'_, ParentState, ()>,
    ) {
        let _ = &self.0;
    }
}

/// Counts child disposal even when its initial callback must not run.
struct Child(Arc<AtomicUsize>);
impl Drop for Child {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl Feature for Child {
    type State = Cell<usize>;
    type Action = ();

    fn reduce(
        &mut self,
        _: &mut Cell<usize>,
        (): &(),
        _: &mut FeatureContext<'_, Cell<usize>, ()>,
    ) {
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one installed-parent schedule keeps both child declaration forms and closure outcomes together"
)]
/// Runs both child declaration forms across live, hidden and closed parent reads.
fn parent_path_after_declaration(read: ParentRead, removable: bool) {
    let driver = Driver::default();
    let owner = Arc::new(Mutex::new(None));
    let armed = Arc::new(AtomicBool::new(false));
    let recipes = Arc::new(AtomicUsize::new(0));
    let identities = Arc::new(AtomicUsize::new(0));
    let constructions = Arc::new(AtomicUsize::new(0));
    let initials = Arc::new(AtomicUsize::new(0));
    let child_drops = Arc::new(AtomicUsize::new(0));
    let parent_armed = Arc::clone(&armed);
    let parent = OptionalMount::new(
        select_parent,
        |state: &mut State| state.parent.as_mut(),
        |_: &State| {
            Some(MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Parent(Arc::clone(&parent_armed)),
    );
    let (builder, _) = Store::feature_builder(
        State {
            parent: Some(ParentState {
                child: Some(Cell::new(1)),
            }),
            owner: Arc::clone(&owner),
            armed: Arc::clone(&armed),
            read,
        },
        Root,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(parent, {
        let armed = Arc::clone(&armed);
        let recipes = Arc::clone(&recipes);
        let identities = Arc::clone(&identities);
        let constructions = Arc::clone(&constructions);
        let initials = Arc::clone(&initials);
        let child_drops = Arc::clone(&child_drops);
        move |_, children| {
            recipes.fetch_add(1, Ordering::SeqCst);
            let identity_count = Arc::clone(&identities);
            let make_count = Arc::clone(&constructions);
            let drops = Arc::clone(&child_drops);
            let identity = move |_: &ParentState| {
                identity_count.fetch_add(1, Ordering::SeqCst);
                Some(MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            };
            let make = move |_: &Cell<usize>| {
                make_count.fetch_add(1, Ordering::SeqCst);
                Child(Arc::clone(&drops))
            };
            if removable {
                let _ = children.optional_removable(
                    |parent: &ParentState| &parent.child,
                    |parent: &mut ParentState| &mut parent.child,
                    identity,
                    make,
                );
            } else {
                let initial = Arc::clone(&initials);
                children.optional(
                    OptionalMount::new(
                        |parent: &ParentState| parent.child.as_ref(),
                        |parent: &mut ParentState| parent.child.as_mut(),
                        identity,
                        make,
                    )
                    .on_initial(Cell::get, move |_, _| {
                        initial.fetch_add(1, Ordering::SeqCst);
                        crate::Task::run(|_| async {})
                    }),
                );
            }
            // The parent is installed before its recipe creates the child.
            armed.store(true, Ordering::SeqCst);
        }
    });
    let store = builder
        .build()
        .expect("root and parent are initially present");
    *owner.lock() = Some(store);
    driver.turn();

    assert_eq!(recipes.load(Ordering::SeqCst), 1);
    let installed = matches!(read, ParentRead::Live);
    assert_eq!(identities.load(Ordering::SeqCst), usize::from(installed));
    assert_eq!(constructions.load(Ordering::SeqCst), usize::from(installed));
    assert_eq!(
        initials.load(Ordering::SeqCst),
        usize::from(installed && !removable)
    );
    assert_eq!(child_drops.load(Ordering::SeqCst), 0);
    assert_eq!(owner.lock().is_none(), matches!(read, ParentRead::Close));

    let last_owner = owner.lock().take();
    drop(last_owner);
    driver.turn();
    assert_eq!(child_drops.load(Ordering::SeqCst), usize::from(installed));
    assert!(driver.0.lock().is_empty());
}

#[test]
fn composed_optional_parent_read_stops_child_identity_when_hidden_or_closed() {
    for removable in [false, true] {
        for read in [ParentRead::Live, ParentRead::Hidden, ParentRead::Close] {
            parent_path_after_declaration(read, removable);
        }
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Keeps closure, deferred discovery, replacement and exact disposal in one actor schedule."
)]
fn parent_lookup_closure_after_construction_suppresses_descendant_recipe() {
    for read in [ParentRead::Live, ParentRead::Hidden, ParentRead::Close] {
        let driver = Driver::default();
        let owner = Arc::new(Mutex::new(None));
        let armed = Arc::new(AtomicBool::new(false));
        let parent_makes = Arc::new(AtomicUsize::new(0));
        let parent_drops = Arc::new(AtomicUsize::new(0));
        let recipe_calls = Arc::new(AtomicUsize::new(0));
        let recipe_drops = Arc::new(AtomicUsize::new(0));
        let child_makes = Arc::new(AtomicUsize::new(0));
        let child_drops = Arc::new(AtomicUsize::new(0));
        let replacement = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let identity_replacement = Arc::clone(&replacement);
        let make_armed = Arc::clone(&armed);
        let make_count = Arc::clone(&parent_makes);
        let parent_disposal = Arc::clone(&parent_drops);
        let parent = OptionalMount::new(
            select_parent,
            |state: &mut State| state.parent.as_mut(),
            move |_: &State| {
                Some(MountIdentity {
                    key: (),
                    replacement: identity_replacement.load(Ordering::SeqCst),
                    configuration: 0,
                })
            },
            move |_| {
                make_count.fetch_add(1, Ordering::SeqCst);
                make_armed.store(true, Ordering::SeqCst);
                ObservedParent(DropWatch(Arc::clone(&parent_disposal)))
            },
        );
        let recipe_count = Arc::clone(&recipe_calls);
        let capture = DropWatch(Arc::clone(&recipe_drops));
        let child_count = Arc::clone(&child_makes);
        let child_disposal = Arc::clone(&child_drops);
        let (builder, parent_site) = Store::feature_builder(
            State {
                parent: Some(ParentState {
                    child: Some(Cell::new(1)),
                }),
                owner: Arc::clone(&owner),
                armed: Arc::clone(&armed),
                read,
            },
            Root,
            Runtime::new(driver.clone()),
        )
        .mount_with_children(parent, move |_, children| {
            let _ = &capture;
            recipe_count.fetch_add(1, Ordering::SeqCst);
            let make = Arc::clone(&child_count);
            let drops = Arc::clone(&child_disposal);
            children.optional(OptionalMount::new(
                |parent: &ParentState| parent.child.as_ref(),
                |parent: &mut ParentState| parent.child.as_mut(),
                |_: &ParentState| {
                    Some(MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |_| {
                    make.fetch_add(1, Ordering::SeqCst);
                    Child(Arc::clone(&drops))
                },
            ));
        });
        let store = builder.build().expect("parent starts present");
        *owner.lock() = Some(store);
        driver.turn();

        let live = matches!(read, ParentRead::Live);
        assert_eq!(parent_makes.load(Ordering::SeqCst), 1);
        assert_eq!(recipe_calls.load(Ordering::SeqCst), usize::from(live));
        assert_eq!(child_makes.load(Ordering::SeqCst), usize::from(live));
        let open = !matches!(read, ParentRead::Close);
        assert_eq!(owner.lock().is_some(), open);
        if open {
            assert_eq!(parent_drops.load(Ordering::SeqCst), 0);
            assert_eq!(recipe_drops.load(Ordering::SeqCst), 0);
        }
        assert_eq!(child_drops.load(Ordering::SeqCst), 0);

        if matches!(read, ParentRead::Hidden) {
            let store = owner.lock().as_ref().unwrap().clone();
            let mut acquire = Box::pin(store.acquire(&parent_site));
            assert!(poll(acquire.as_mut()).is_pending());
            driver.turn();
            let Poll::Ready(Ok(Some(parent))) = poll(acquire.as_mut()) else {
                panic!("installed parent retains its authority while selection is hidden");
            };
            for (marker, expected_calls) in [(0, 1), (0, 1), (1, 1), (1, 2)] {
                // A replacement constructor hides the path again. The following
                // transaction reveals that same incarnation without replacing it.
                replacement.store(marker, Ordering::SeqCst);
                armed.store(false, Ordering::SeqCst);
                let mut send = Box::pin(store.send(()));
                assert!(poll(send.as_mut()).is_pending());
                driver.turn();
                assert!(matches!(poll(send.as_mut()), Poll::Ready(Ok(_))));

                assert_eq!(recipe_calls.load(Ordering::SeqCst), expected_calls);
                assert_eq!(child_makes.load(Ordering::SeqCst), expected_calls);
                let replacements = usize::try_from(marker).unwrap();
                assert_eq!(parent_makes.load(Ordering::SeqCst), 1 + replacements);
                assert_eq!(parent_drops.load(Ordering::SeqCst), replacements);
                assert_eq!(child_drops.load(Ordering::SeqCst), replacements);

                let mut read = Box::pin(parent.read(|state| state.child.as_ref().unwrap().get()));
                assert!(poll(read.as_mut()).is_pending());
                driver.turn();
                if marker == 0 {
                    assert!(matches!(poll(read.as_mut()), Poll::Ready(Ok(1))));
                } else {
                    assert!(matches!(
                        poll(read.as_mut()),
                        Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::ScopeEnded
                    ));
                }
            }
        }

        let last_owner = owner.lock().take();
        drop(last_owner);
        driver.turn();
        let replaced = matches!(read, ParentRead::Hidden);
        assert_eq!(
            parent_drops.load(Ordering::SeqCst),
            1 + usize::from(replaced)
        );
        assert_eq!(recipe_drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            child_drops.load(Ordering::SeqCst),
            if replaced { 2 } else { usize::from(live) }
        );
        assert!(driver.0.lock().is_empty());
    }
}

#[test]
fn parent_leave_then_identity_close_discards_unvisited_child() {
    let driver = Driver::default();
    let owner = Arc::new(Mutex::new(None));
    let armed = Arc::new(AtomicBool::new(false));
    let identity_closes = Arc::new(AtomicUsize::new(0));
    let child_identities = Arc::new(AtomicUsize::new(0));
    let child_makes = Arc::new(AtomicUsize::new(0));
    let child_drops = Arc::new(AtomicUsize::new(0));
    let parent_armed = Arc::clone(&armed);
    let closed = Arc::clone(&identity_closes);
    let parent = OptionalMount::new(
        select_parent,
        |state: &mut State| state.parent.as_mut(),
        move |state: &State| {
            if state.armed.load(Ordering::SeqCst) {
                let last_owner = state.owner.lock().take();
                drop(last_owner);
                closed.fetch_add(1, Ordering::SeqCst);
            }
            Some(MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Parent(Arc::clone(&parent_armed)),
    );
    let identities = Arc::clone(&child_identities);
    let makes = Arc::clone(&child_makes);
    let drops = Arc::clone(&child_drops);
    let (builder, parent_site) = Store::feature_builder(
        State {
            parent: Some(ParentState {
                child: Some(Cell::new(1)),
            }),
            owner: Arc::clone(&owner),
            armed: Arc::clone(&armed),
            read: ParentRead::Live,
        },
        Root,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(parent, move |_, children| {
        let identities = Arc::clone(&identities);
        let makes = Arc::clone(&makes);
        let drops = Arc::clone(&drops);
        children.optional(OptionalMount::new(
            |parent: &ParentState| parent.child.as_ref(),
            |parent: &mut ParentState| parent.child.as_mut(),
            move |_: &ParentState| {
                identities.fetch_add(1, Ordering::SeqCst);
                Some(MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| {
                makes.fetch_add(1, Ordering::SeqCst);
                Child(Arc::clone(&drops))
            },
        ));
    });
    let store = builder.build().expect("parent and child start installed");
    driver.turn();
    assert_eq!(child_identities.load(Ordering::SeqCst), 1);
    assert_eq!(child_makes.load(Ordering::SeqCst), 1);
    let mut acquire = Box::pin(store.acquire(&parent_site));
    assert!(poll(acquire.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(parent))) = poll(acquire.as_mut()) else {
        panic!("installed parent context");
    };
    drop(acquire);
    *owner.lock() = Some(store);

    let mut leave = Box::pin(parent.send(()));
    assert!(poll(leave.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(task)) = poll(leave.as_mut()) else {
        panic!("parent action commits before its identity closes the Store");
    };
    driver.turn();
    assert!(armed.load(Ordering::SeqCst));
    assert_eq!(identity_closes.load(Ordering::SeqCst), 1);
    assert!(owner.lock().is_none());
    assert_eq!(child_identities.load(Ordering::SeqCst), 1);
    assert_eq!(child_makes.load(Ordering::SeqCst), 1);
    assert_eq!(child_drops.load(Ordering::SeqCst), 1);
    assert!(task.is_finished());
    assert!(driver.0.lock().is_empty());
}
