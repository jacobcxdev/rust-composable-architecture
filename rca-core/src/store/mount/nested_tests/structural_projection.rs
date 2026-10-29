//! Checks that closing the root stops later projection and keyed lookup callbacks.

use super::*;
use crate::store::mounted::path::ComposedPath;
use crate::store::mounted::path::KeyedPath;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Lets a parent projection release the last real Store owner.
struct ClosingState {
    row: RowState,
    owner: Arc<Mutex<Option<Store<ClosingState, ()>>>>,
}
struct RowState {
    child: Cell<usize>,
    selections: Arc<AtomicUsize>,
}
struct ClosingRoot;
impl Feature for ClosingRoot {
    type State = ClosingState;
    type Action = ();
    fn reduce(
        &mut self,
        _: &mut ClosingState,
        (): &(),
        _: &mut FeatureContext<'_, ClosingState, ()>,
    ) {
    }
}

#[test]
fn last_owner_dropped_by_parent_projection_prevents_child_projection_and_factory() {
    let driver = Driver::default();
    let owner = Arc::new(Mutex::new(None));
    let selections = Arc::new(AtomicUsize::new(0));
    let factories = Arc::new(AtomicUsize::new(0));
    let initial = Arc::new(AtomicUsize::new(0));
    let make_count = std::sync::Arc::clone(&factories);
    let initial_count = std::sync::Arc::clone(&initial);
    let path = ComposedPath::new(
        OptionalPath::new(
            |state: &ClosingState| {
                let last_owner = state.owner.lock().take();
                drop(last_owner);
                Some(&state.row)
            },
            |state: &mut ClosingState| Some(&mut state.row),
        ),
        OptionalPath::new(
            |row: &RowState| {
                row.selections.fetch_add(1, Ordering::SeqCst);
                Some(&row.child)
            },
            |row: &mut RowState| Some(&mut row.child),
        ),
    );
    let descriptor = Descriptor {
        path,
        identity: Box::new(|_: &ClosingState, _| {
            Some(MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        }),
        make: Box::new(move |_| {
            make_count.fetch_add(1, Ordering::SeqCst);
            ChildFeature {
                cache: Cell::new(0),
                trace: Arc::new(Mutex::new(Vec::new())),
            }
        }),
        initial: Some(Box::new(move |_, _| {
            initial_count.fetch_add(1, Ordering::SeqCst);
            Some(Task::run(|_| async {}))
        })),
        reactions: Vec::new(),
    };
    let (declaration, _) = attach_descriptor::<ClosingState, (), ChildFeature, _, _>(descriptor);
    let store = Store::feature_builder(
        ClosingState {
            row: RowState {
                child: Cell::new(1),
                selections: std::sync::Arc::clone(&selections),
            },
            owner: std::sync::Arc::clone(&owner),
        },
        ClosingRoot,
        Runtime::new(driver.clone()),
    )
    .with_mount(Box::new(declaration))
    .build()
    .unwrap();
    *owner.lock() = Some(store);
    driver.turn();
    assert!(owner.lock().is_none());
    assert_eq!(
        selections.load(Ordering::SeqCst),
        0,
        "closed between composed structural projections"
    );
    assert_eq!(factories.load(Ordering::SeqCst), 0);
    assert_eq!(initial.load(Ordering::SeqCst), 0);
}

/// Counts hash and equality work; all instances denote the same valid key.
struct CountedKey(Arc<AtomicUsize>);
impl PartialEq for CountedKey {
    fn eq(&self, _: &Self) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst);
        true
    }
}
impl Eq for CountedKey {}
impl Hash for CountedKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.fetch_add(1, Ordering::SeqCst);
        0_u8.hash(state);
    }
}
struct KeyedClosingState {
    rows: KeyedState<CountedKey, Cell<usize>>,
    owner: Arc<Mutex<Option<Store<KeyedClosingState, ()>>>>,
}
struct KeyedClosingRoot;
impl Feature for KeyedClosingRoot {
    type State = KeyedClosingState;
    type Action = ();
    fn reduce(
        &mut self,
        _: &mut Self::State,
        (): &(),
        _: &mut FeatureContext<'_, Self::State, ()>,
    ) {
    }
}
/// Closes the root before returning the collection used by the next lookup step.
fn close_and_select(state: &KeyedClosingState) -> &KeyedState<CountedKey, Cell<usize>> {
    drop(state.owner.lock().take());
    &state.rows
}
/// Checks both structural projection and identity lookup with the same close point.
fn keyed_selector_closure(identity: bool) {
    let driver = Driver::default();
    let owner = Arc::new(Mutex::new(None));
    let lookups = Arc::new(AtomicUsize::new(0));
    let factory = Arc::new(AtomicUsize::new(0));
    let make_count = std::sync::Arc::clone(&factory);
    let make = move |_: &Cell<usize>| {
        make_count.fetch_add(1, Ordering::SeqCst);
        ChildFeature {
            cache: Cell::new(0),
            trace: Arc::new(Mutex::new(Vec::new())),
        }
    };
    let declaration: Box<dyn MountDeclaration<KeyedClosingState, ()> + Send> = if identity {
        super::super::nested::attach_selected_key(
            close_and_select,
            |state| &mut state.rows,
            CountedKey(std::sync::Arc::clone(&lookups)),
            make,
            |_, _| {},
        )
        .0
    } else {
        let descriptor = Descriptor {
            path: KeyedPath::new(
                close_and_select,
                |state| &mut state.rows,
                Arc::new(CountedKey(std::sync::Arc::clone(&lookups))),
            ),
            identity: Box::new(|_: &KeyedClosingState, _| {
                Some(MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            }),
            make: Box::new(make),
            initial: None,
            reactions: Vec::new(),
        };
        Box::new(attach_descriptor::<KeyedClosingState, (), ChildFeature, _, _>(descriptor).0)
    };
    let store = Store::feature_builder(
        KeyedClosingState {
            rows: [(CountedKey(std::sync::Arc::clone(&lookups)), Cell::new(1))]
                .into_iter()
                .collect(),
            owner: std::sync::Arc::clone(&owner),
        },
        KeyedClosingRoot,
        Runtime::new(driver.clone()),
    )
    .with_mount(declaration)
    .build()
    .unwrap();
    *owner.lock() = Some(store);
    lookups.store(0, Ordering::SeqCst);
    driver.turn();
    assert!(owner.lock().is_none());
    assert_eq!(
        lookups.load(Ordering::SeqCst),
        0,
        "closed collection selector must not enter keyed lookup"
    );
    assert_eq!(factory.load(Ordering::SeqCst), 0);
}
#[test]
fn keyed_structural_projection_checks_closure_before_hash_lookup() {
    keyed_selector_closure(false);
}
#[test]
fn keyed_structural_identity_checks_closure_before_stamp_lookup() {
    keyed_selector_closure(true);
}
