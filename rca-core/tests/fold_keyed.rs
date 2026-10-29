//! Dynamic keyed order and independent optional descendants use one real roster.
use rca_core::{
    EntryMount, Feature, FeatureContext, Fold, FoldKey, KeyedState, MountChildren, MountIdentity,
    MountSite, OptionalMount, Runtime, Store, TaskAccessReason, TaskProgress,
};
use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, Weak};
#[path = "fold/support.rs"]
mod support;
use support::Driver;

/// Records contribution order in an owned value without extra trait bounds.
struct Owned(Vec<usize>);
/// Concatenates contributions so reordering cannot hide behind a commutative sum.
struct Ordered;
impl FoldKey for Ordered {
    type Value = Owned;
    fn identity() -> Owned {
        Owned(Vec::new())
    }
    fn reduce(mut accumulator: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
        accumulator.0.extend(next.0);
        ControlFlow::Continue(accumulator)
    }
}
/// Makes the mixed optional and keyed roster a single mounted subtree.
struct Root(Option<Panel>);
/// Places optional siblings before and after the keyed rows.
struct Panel {
    before: Option<usize>,
    rows: KeyedState<u8, Row>,
    after: Option<usize>,
}
/// Lets a row's configuration change independently of its optional leaf's identity.
struct Row {
    value: usize,
    leaf: Option<usize>,
    replacement: u64,
    configuration: u64,
}
/// Creates a fresh row with its own leaf and initial generations.
fn row(value: usize, leaf: usize) -> Row {
    Row {
        value,
        leaf: Some(leaf),
        replacement: 0,
        configuration: 0,
    }
}
/// Changes order, child identity, configuration and keyed membership independently.
enum Action {
    Reverse,
    Leaf,
    Refresh,
    Remove,
    Insert,
}
/// Applies roster edits to existing keyed state through the public collection API.
struct RootFeature;
impl Feature for RootFeature {
    type State = Root;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Root,
        action: &Action,
        _: &mut FeatureContext<'_, Root, Action>,
    ) {
        let rows = &mut state.0.as_mut().unwrap().rows;
        match action {
            Action::Reverse => rows.reverse(),
            Action::Leaf => {
                let row = rows.get_mut(&1).unwrap();
                row.leaf = Some(9);
                row.replacement += 1;
            }
            Action::Refresh => rows.get_mut(&1).unwrap().configuration += 1,
            Action::Remove => {
                rows.remove(&1);
            }
            Action::Insert => rows.insert(1, row(8, 9)),
        }
    }
}
/// Keeps the panel installed while its descendants change.
struct PanelFeature;
impl Feature for PanelFeature {
    type State = Panel;
    type Action = ();
    fn reduce(&mut self, _: &mut Panel, (): &(), _: &mut FeatureContext<'_, Panel, ()>) {}
}
/// Makes row replacement observable without adding reducer effects.
struct RowFeature;
impl Feature for RowFeature {
    type State = Row;
    type Action = ();
    fn reduce(&mut self, _: &mut Row, (): &(), _: &mut FeatureContext<'_, Row, ()>) {}
}
/// Keeps each optional leaf independently mounted beneath its row.
struct LeafFeature;
impl Feature for LeafFeature {
    type State = usize;
    type Action = ();
    fn reduce(&mut self, _: &mut usize, (): &(), _: &mut FeatureContext<'_, usize, ()>) {}
}
/// Exposes the latest row declarations without retaining their Store or callbacks.
type RowSites = Arc<Mutex<BTreeMap<u8, MountSite<Root, Row, (), Action>>>>;
/// Keeps leaf declarations separate so row and leaf invalidation can be compared.
type LeafSites = Arc<Mutex<BTreeMap<u8, MountSite<Root, usize, (), Action>>>>;

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One roster sequence preserves independent optional and keyed contribution lifetimes"
)]
fn mixed_optional_keyed_roster_reorders_retains_and_replaces_independent_contributions() {
    let driver = Driver::default();
    let fold = Fold::<Root, Ordered>::new();
    let declared = fold.clone();
    let row_sites: RowSites = Arc::default();
    let leaf_sites: LeafSites = Arc::default();
    let recipes = Arc::new(Mutex::new(Vec::new()));
    let captures = Arc::new(Mutex::new(BTreeMap::<u8, Weak<()>>::new()));
    let declared_rows = std::sync::Arc::clone(&row_sites);
    let declared_leaves = std::sync::Arc::clone(&leaf_sites);
    let declared_recipes = std::sync::Arc::clone(&recipes);
    let declared_captures = std::sync::Arc::clone(&captures);
    let panel = OptionalMount::new(
        |root: &Root| root.0.as_ref(),
        |root: &mut Root| root.0.as_mut(),
        |root: &Root| {
            root.0.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| PanelFeature,
    );
    let (builder, _) = Store::feature_builder(
        Root(Some(Panel {
            before: Some(2),
            rows: [(1, row(3, 4)), (2, row(5, 6))].into_iter().collect(),
            after: Some(7),
        })),
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(panel, move |_, children| {
        children.contribute(&declared, |_| Owned(vec![1]));
        let before = OptionalMount::new(
            |panel: &Panel| panel.before.as_ref(),
            |panel: &mut Panel| panel.before.as_mut(),
            |panel: &Panel| {
                panel.before.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| LeafFeature,
        );
        let before_fold = declared.clone();
        children.optional_with_children(before, move |_, children| {
            children.contribute(&before_fold, |value| Owned(vec![*value]));
        });
        let keyed_fold = declared.clone();
        let row_sites = std::sync::Arc::clone(&declared_rows);
        let leaf_sites = std::sync::Arc::clone(&declared_leaves);
        let recipes = std::sync::Arc::clone(&declared_recipes);
        let captures = std::sync::Arc::clone(&declared_captures);
        children.keyed_with_children(
            |panel| &panel.rows,
            |panel| &mut panel.rows,
            move |id, _| {
                let id = *id;
                assert!(
                    captures
                        .lock()
                        .unwrap()
                        .get(&id)
                        .is_none_or(|capture| capture.upgrade().is_none()),
                    "retired row callback drains before successor construction"
                );
                let fold = keyed_fold.clone();
                let row_sites = std::sync::Arc::clone(&row_sites);
                let leaf_sites = std::sync::Arc::clone(&leaf_sites);
                let recipes = std::sync::Arc::clone(&recipes);
                let captures = std::sync::Arc::clone(&captures);
                (
                    EntryMount::new(RowFeature).configuration(|row| row.configuration),
                    move |_: &Row, children: &mut MountChildren<'_, Root, Action, Row, ()>| {
                        recipes.lock().unwrap().push(id);
                        row_sites
                            .lock()
                            .unwrap()
                            .insert(id, children.site().clone());
                        let capture = Arc::new(());
                        captures
                            .lock()
                            .unwrap()
                            .insert(id, Arc::downgrade(&capture));
                        children.contribute(&fold, move |row| {
                            let _ = &capture;
                            Owned(vec![row.value])
                        });
                        let leaf = OptionalMount::new(
                            |row: &Row| row.leaf.as_ref(),
                            |row: &mut Row| row.leaf.as_mut(),
                            |row: &Row| {
                                row.leaf.map(|_| MountIdentity {
                                    key: (),
                                    replacement: row.replacement,
                                    configuration: 0,
                                })
                            },
                            |_| LeafFeature,
                        );
                        let leaf_fold = fold.clone();
                        let site = children.optional_with_children(leaf, move |_, children| {
                            children.contribute(&leaf_fold, |value| Owned(vec![*value]));
                        });
                        leaf_sites.lock().unwrap().insert(id, site);
                    },
                )
            },
        );
        let after = OptionalMount::new(
            |panel: &Panel| panel.after.as_ref(),
            |panel: &mut Panel| panel.after.as_mut(),
            |panel: &Panel| {
                panel.after.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| LeafFeature,
        );
        let after_fold = declared.clone();
        children.optional_with_children(after, move |_, children| {
            children.contribute(&after_fold, |value| Owned(vec![*value]));
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(
        driver.finish(store.fold(&fold)).unwrap().0,
        [1, 2, 3, 4, 5, 6, 7]
    );
    let row_site = row_sites.lock().unwrap()[&1].clone();
    let leaf_site = leaf_sites.lock().unwrap()[&1].clone();
    let row_view = driver
        .finish(store.acquire_fold(&fold, &row_site))
        .unwrap()
        .unwrap();
    let leaf_view = driver
        .finish(store.acquire_fold(&fold, &leaf_site))
        .unwrap()
        .unwrap();
    drop(driver.finish(store.send(Action::Reverse)).unwrap());
    assert_eq!(
        driver.finish(store.fold(&fold)).unwrap().0,
        [1, 2, 5, 6, 3, 4, 7]
    );
    assert_eq!(driver.finish(row_view.read()).unwrap().0, [3, 4]);
    assert_eq!(*recipes.lock().unwrap(), [1, 2]);
    drop(driver.finish(store.send(Action::Leaf)).unwrap());
    assert_eq!(
        driver.finish(store.fold(&fold)).unwrap().0,
        [1, 2, 5, 6, 3, 9, 7]
    );
    assert_eq!(driver.finish(row_view.read()).unwrap().0, [3, 9]);
    let Err(error) = driver.finish(leaf_view.read()) else {
        panic!("retired leaf view")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    drop(driver.finish(store.send(Action::Refresh)).unwrap());
    let Err(error) = driver.finish(row_view.read()) else {
        panic!("refreshed row view")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(*recipes.lock().unwrap(), [1, 2]);
    assert_eq!(
        driver.finish(store.fold(&fold)).unwrap().0,
        [1, 2, 5, 6, 3, 9, 7]
    );
    let refreshed = driver
        .finish(store.acquire_fold(&fold, &row_site))
        .unwrap()
        .unwrap();
    drop(driver.finish(store.send(Action::Remove)).unwrap());
    assert_eq!(driver.finish(store.fold(&fold)).unwrap().0, [1, 2, 5, 6, 7]);
    assert!(captures.lock().unwrap()[&1].upgrade().is_none());
    let Err(error) = driver.finish(refreshed.read()) else {
        panic!("removed row view")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    drop(driver.finish(store.send(Action::Insert)).unwrap());
    assert_eq!(
        driver.finish(store.fold(&fold)).unwrap().0,
        [1, 2, 5, 6, 8, 9, 7]
    );
    assert_eq!(*recipes.lock().unwrap(), [1, 2, 1]);
    let Err(error) = driver.finish(store.acquire_fold(&fold, &row_site)) else {
        panic!("retired keyed declaration")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    let fresh_site = row_sites.lock().unwrap()[&1].clone();
    let fresh = driver
        .finish(store.acquire_fold(&fold, &fresh_site))
        .unwrap()
        .unwrap();
    assert_eq!(driver.finish(fresh.read()).unwrap().0, [8, 9]);
    drop(store);
    driver.turn();
    assert!(
        captures
            .lock()
            .unwrap()
            .values()
            .all(|capture| capture.upgrade().is_none())
    );
}
