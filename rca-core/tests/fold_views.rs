//! Captured subtree reads never retarget a replaced or refreshed declaration.
use rca_core::{
    Feature, FeatureContext, Fold, FoldKey, MountIdentity, OptionalMount, Runtime, Store,
    TaskAccessReason, TaskProgress,
};
use std::cell::Cell;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
#[path = "fold/support.rs"]
mod support;
use support::Driver;

/// Keeps returned values owned and non-`Sync` without adding clone or debug bounds.
struct Owned(Cell<usize>);
/// Shows that a Shared fold key need not itself implement `Send` or `Sync`.
struct Sum(std::marker::PhantomData<std::rc::Rc<()>>);
impl FoldKey for Sum {
    type Value = Owned;
    fn identity() -> Owned {
        Owned(Cell::new(0))
    }
    fn reduce(accumulator: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
        accumulator.0.set(accumulator.0.get() + next.0.get());
        ControlFlow::Continue(accumulator)
    }
}
/// Changes child identity and configuration separately while counting state selections.
struct State {
    leaf: Option<usize>,
    replacement: u64,
    configuration: u64,
    selections: Arc<AtomicUsize>,
}
/// Exercises each way a captured child can become stale or absent.
enum Action {
    Refresh,
    Replace,
    Hide,
    Show,
}
/// Applies lifecycle changes without replacing the fold channel.
struct Root;
impl Feature for Root {
    type State = State;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut FeatureContext<'_, State, Action>,
    ) {
        match action {
            Action::Refresh => state.configuration += 1,
            Action::Replace => {
                state.leaf = Some(8);
                state.replacement += 1;
            }
            Action::Hide => state.leaf = None,
            Action::Show => state.leaf = Some(9),
        }
    }
}
/// Holds the child state whose callback capture is observed through a weak pointer.
struct Leaf;
impl Feature for Leaf {
    type State = usize;
    type Action = ();
    fn reduce(&mut self, _: &mut usize, (): &(), _: &mut FeatureContext<'_, usize, ()>) {}
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One captured-subtree sequence compares refresh, replacement, absence and foreign-root rejection"
)]
fn captured_subtree_rejects_refresh_replacement_absence_and_other_root_before_selection() {
    let driver = Driver::default();
    let fold = Fold::<State, Sum>::new();
    let declared = fold.clone();
    let selections = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(Mutex::new(Vec::new()));
    let declared_captures = std::sync::Arc::clone(&captures);
    let mount = OptionalMount::new(
        |root: &State| {
            root.selections.fetch_add(1, Ordering::SeqCst);
            root.leaf.as_ref()
        },
        |root: &mut State| root.leaf.as_mut(),
        |root: &State| {
            root.leaf.map(|_| MountIdentity {
                key: (),
                replacement: root.replacement,
                configuration: root.configuration,
            })
        },
        |_| Leaf,
    );
    let (builder, site) = Store::feature_builder(
        State {
            leaf: Some(2),
            replacement: 0,
            configuration: 0,
            selections: std::sync::Arc::clone(&selections),
        },
        Root,
        Runtime::new(driver.clone()),
    )
    .mount_with_children(mount, move |_, children| {
        let capture = Arc::new(());
        declared_captures
            .lock()
            .unwrap()
            .push(Arc::downgrade(&capture));
        children.contribute(&declared, move |value| {
            let _ = &capture;
            Owned(Cell::new(*value))
        });
    });
    let other = Store::feature(
        State {
            leaf: None,
            replacement: 0,
            configuration: 0,
            selections: Arc::new(AtomicUsize::new(0)),
        },
        Root,
        Runtime::new(driver.clone()),
    );
    driver.finish(other.ready()).unwrap();
    let Err(error) = driver.finish(other.acquire_fold(&fold, &site)) else {
        panic!("an unbuilt declaration has no root authority")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(selections.load(Ordering::SeqCst), 0);
    assert!(captures.lock().unwrap().is_empty());

    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let before = selections.load(Ordering::SeqCst);
    let first = driver
        .finish(store.acquire_fold(&fold, &site))
        .unwrap()
        .unwrap();
    let view_debug = format!("{first:?}");
    assert!(view_debug.contains("FoldView"));
    assert!(!view_debug.contains("Owned") && !view_debug.contains("Cell"));
    assert_eq!(
        selections.load(Ordering::SeqCst),
        before,
        "acquisition is selector-free"
    );
    assert_eq!(driver.finish(first.read()).unwrap().0.get(), 2);

    drop(driver.finish(store.send(Action::Refresh)).unwrap());
    let before = selections.load(Ordering::SeqCst);
    let Err(error) = driver.finish(first.read()) else {
        panic!("stale configuration")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(selections.load(Ordering::SeqCst), before);
    let refreshed = driver
        .finish(store.acquire_fold(&fold, &site))
        .unwrap()
        .unwrap();
    assert_eq!(driver.finish(refreshed.read()).unwrap().0.get(), 2);

    drop(driver.finish(store.send(Action::Replace)).unwrap());
    let before = selections.load(Ordering::SeqCst);
    let Err(error) = driver.finish(refreshed.read()) else {
        panic!("stale replacement")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(selections.load(Ordering::SeqCst), before);
    let replacement = driver
        .finish(store.acquire_fold(&fold, &site))
        .unwrap()
        .unwrap();
    assert_eq!(driver.finish(replacement.read()).unwrap().0.get(), 8);

    let Err(error) = driver.finish(other.acquire_fold(&fold, &site)) else {
        panic!("wrong root")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(driver.finish(other.fold(&fold)).unwrap().0.get(), 0);

    drop(driver.finish(store.send(Action::Hide)).unwrap());
    assert!(
        driver
            .finish(store.acquire_fold(&fold, &site))
            .unwrap()
            .is_none()
    );
    let Err(error) = driver.finish(other.acquire_fold(&fold, &site)) else {
        panic!("wrong root while absent")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    let Err(error) = driver.finish(replacement.read()) else {
        panic!("removed subtree")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    drop(driver.finish(store.send(Action::Show)).unwrap());
    let fresh = driver
        .finish(store.acquire_fold(&fold, &site))
        .unwrap()
        .unwrap();
    assert_eq!(driver.finish(fresh.read()).unwrap().0.get(), 9);

    drop(store);
    driver.turn();
    assert!(
        captures
            .lock()
            .unwrap()
            .iter()
            .all(|capture| capture.upgrade().is_none()),
        "retained views own metadata, not callbacks"
    );
    let Err(error) = driver.finish(fresh.read()) else {
        panic!("closed root")
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    driver.finish(other.shutdown()).unwrap();
}
