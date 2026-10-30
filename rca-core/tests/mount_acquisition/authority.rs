use super::{Driver, poll};
use rca_core::{
    Feature, FeatureContext, MountIdentity, OptionalMount, RootMount, Runtime, TaskAccessReason,
    TaskContext, TaskProgress, TaskSendError,
};
use std::cell::Cell;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::Poll;

/// Separates child refresh, root refresh and replacement generations.
struct State {
    child: Option<Cell<usize>>,
    /// Refreshes the child without replacing its Feature.
    configuration: u64,
    /// Refreshes the root authority while retaining the same child.
    root_configuration: u64,
    /// Replaces the child Feature and permanently retires older contexts.
    replacement: u64,
    selections: Arc<AtomicUsize>,
}
enum Action {
    Configuration(u64),
    RootConfiguration(u64),
    Replace,
    Remove,
    Insert,
    Policy(usize),
}
struct Root;
impl Feature for Root {
    type State = State;
    type Action = Action;
    fn update(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut FeatureContext<'_, State, Action>,
    ) {
        match action {
            Action::Configuration(value) => state.configuration = *value,
            Action::RootConfiguration(value) => state.root_configuration = *value,
            Action::Replace => state.replacement += 1,
            Action::Remove => state.child = None,
            Action::Insert => state.child = Some(Cell::new(4)),
            Action::Policy(value) => state.child.as_ref().unwrap().set(*value),
        }
    }
}
#[derive(Debug)]
enum ChildAction {
    BumpCache,
    ReportCache,
}
/// Keeps Feature cache outside projected State to expose retention and replacement.
struct Child {
    cache: Cell<usize>,
    drops: Arc<AtomicUsize>,
}
impl Drop for Child {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Feature for Child {
    type State = Cell<usize>;
    type Action = ChildAction;
    fn update(
        &mut self,
        state: &mut Cell<usize>,
        action: &ChildAction,
        _: &mut FeatureContext<'_, Cell<usize>, ChildAction>,
    ) {
        match action {
            ChildAction::BumpCache => self.cache.set(self.cache.get() + 1),
            ChildAction::ReportCache => state.set(self.cache.get()),
        }
    }
}
/// Drives one public operation for at most eight manual turns.
fn finish<F: Future>(driver: &Driver, future: F) -> F::Output {
    let mut future = Box::pin(future);
    for _ in 0..8 {
        if let Poll::Ready(value) = poll(future.as_mut()) {
            return value;
        }
        driver.turn();
    }
    panic!("bounded deterministic operation did not finish");
}
/// Checks stale authority before any read, write or send selector enters.
fn stale(driver: &Driver, child: &TaskContext<Cell<usize>, ChildAction>, selections: &AtomicUsize) {
    selections.store(0, Ordering::SeqCst);
    let read = finish(driver, child.read(|_| panic!("stale read callback"))).unwrap_err();
    assert_eq!(
        (read.reason(), read.progress()),
        (TaskAccessReason::ScopeEnded, TaskProgress::NotStarted)
    );
    let update = finish(driver, child.update(|_| panic!("stale update callback"))).unwrap_err();
    assert_eq!(
        (update.reason(), update.progress()),
        (TaskAccessReason::ScopeEnded, TaskProgress::NotStarted)
    );
    let send = finish(driver, child.send(ChildAction::BumpCache)).unwrap_err();
    assert!(
        matches!(send, TaskSendError::Rejected { action: ChildAction::BumpCache, error } if error.reason() == TaskAccessReason::ScopeEnded && error.progress() == TaskProgress::NotStarted)
    );
    assert_eq!(selections.load(Ordering::SeqCst), 0);
}

// Catches configuration refresh replacing F, old authority revival, and stale selection.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One actor schedule distinguishes refresh, queued replacement, removal and stale authority."
)]
fn leaf_and_root_configuration_refresh_retain_cache_and_permanently_retire_authority() {
    let driver = Driver::default();
    let selections = Arc::new(AtomicUsize::new(0));
    let constructions = Arc::new(AtomicUsize::new(0));
    let initials = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let makes = std::sync::Arc::clone(&constructions);
    let dropped = std::sync::Arc::clone(&drops);
    let initial_calls = std::sync::Arc::clone(&initials);
    let mount = OptionalMount::new(
        |state: &State| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.child.as_ref()
        },
        |state: &mut State| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.child.as_mut()
        },
        |state: &State| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: state.replacement,
                configuration: state.configuration,
            })
        },
        move |_| {
            makes.fetch_add(1, Ordering::SeqCst);
            Child {
                cache: Cell::new(0),
                drops: std::sync::Arc::clone(&dropped),
            }
        },
    )
    .on_initial(Cell::get, move |_, _| {
        initial_calls.fetch_add(1, Ordering::SeqCst);
        rca_core::Task::run(|_| async {})
    });
    let (builder, site) = RootMount::new(Root)
        .configuration(|state: &State| state.root_configuration)
        .builder(
            State {
                child: Some(Cell::new(4)),
                configuration: 0,
                root_configuration: 0,
                replacement: 0,
                selections: std::sync::Arc::clone(&selections),
            },
            Runtime::new(driver.clone()),
        )
        .mount_with_site(mount);
    let store = builder.build().unwrap();
    finish(&driver, store.ready()).unwrap();
    let original = finish(&driver, store.acquire(&site)).unwrap().unwrap();
    let _ = finish(&driver, original.send(ChildAction::BumpCache)).unwrap();
    let _ = finish(&driver, store.send(Action::Configuration(1))).unwrap();
    stale(&driver, &original, &selections);
    let current = finish(&driver, store.acquire(&site)).unwrap().unwrap();
    let _ = finish(&driver, current.send(ChildAction::ReportCache)).unwrap();
    assert_eq!(finish(&driver, current.read(Cell::get)).unwrap(), 1);
    assert_eq!(constructions.load(Ordering::SeqCst), 1);
    assert_eq!(initials.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let _ = finish(&driver, store.send(Action::Configuration(0))).unwrap();
    stale(&driver, &original, &selections); // X -> Y -> X cannot revive the first capture.
    stale(&driver, &current, &selections);
    let current = finish(&driver, store.acquire(&site)).unwrap().unwrap();
    let _ = finish(&driver, store.send(Action::RootConfiguration(1))).unwrap();
    stale(&driver, &current, &selections); // Ancestor configuration, not leaf lifetime.
    let current = finish(&driver, store.acquire(&site)).unwrap().unwrap();
    let _ = finish(&driver, current.send(ChildAction::ReportCache)).unwrap();
    assert_eq!(finish(&driver, current.read(Cell::get)).unwrap(), 1);
    assert_eq!(
        initials.load(Ordering::SeqCst),
        1,
        "root/leaf refresh is not Initial"
    );
    let _ = finish(&driver, store.send(Action::Policy(12))).unwrap();
    assert_eq!(finish(&driver, current.read(Cell::get)).unwrap(), 12);
    // Both requests enter the mailbox while the original child is live. The
    // earlier parent transaction must retire its authority before child entry.
    let mut replace = Box::pin(store.send(Action::Replace));
    assert!(poll(replace.as_mut()).is_pending());
    let mut queued = Box::pin(current.send(ChildAction::BumpCache));
    assert!(poll(queued.as_mut()).is_pending());
    assert_eq!(constructions.load(Ordering::SeqCst), 1);
    let _ = finish(&driver, replace).unwrap();
    assert!(matches!(
        finish(&driver, queued),
        Err(TaskSendError::Rejected { action: ChildAction::BumpCache, error })
            if error.reason() == TaskAccessReason::ScopeEnded
                && error.progress() == TaskProgress::NotStarted
    ));
    stale(&driver, &current, &selections);
    assert_eq!(constructions.load(Ordering::SeqCst), 2);
    assert_eq!(initials.load(Ordering::SeqCst), 2);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let replacement = finish(&driver, store.acquire(&site)).unwrap().unwrap();
    let _ = finish(&driver, replacement.send(ChildAction::ReportCache)).unwrap();
    assert_eq!(finish(&driver, replacement.read(Cell::get)).unwrap(), 0);
    let mut remove = Box::pin(store.send(Action::Remove));
    assert!(poll(remove.as_mut()).is_pending());
    let mut queued = Box::pin(replacement.send(ChildAction::ReportCache));
    assert!(poll(queued.as_mut()).is_pending());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let _ = finish(&driver, remove).unwrap();
    assert!(matches!(
        finish(&driver, queued),
        Err(TaskSendError::Rejected { action: ChildAction::ReportCache, error })
            if error.reason() == TaskAccessReason::ScopeEnded
                && error.progress() == TaskProgress::NotStarted
    ));
    stale(&driver, &replacement, &selections);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    selections.store(0, Ordering::SeqCst);
    assert!(finish(&driver, store.acquire(&site)).unwrap().is_none());
    assert_eq!(selections.load(Ordering::SeqCst), 0);
    let _ = finish(&driver, store.send(Action::Insert)).unwrap();
    stale(&driver, &replacement, &selections);
    assert_eq!(constructions.load(Ordering::SeqCst), 3);
    assert_eq!(initials.load(Ordering::SeqCst), 3);
    finish(&driver, store.shutdown()).unwrap();
}

// First polling fixes queue order, not the child incarnation. The actor captures
// the endpoint at its read position; a later mutation may retire it immediately.
#[test]
fn acquisitions_before_and_after_replacement_capture_their_actor_positions() {
    let driver = Driver::default();
    let selections = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let dropped = std::sync::Arc::clone(&drops);
    let mount = OptionalMount::new(
        |state: &State| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.child.as_ref()
        },
        |state: &mut State| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.child.as_mut()
        },
        |state: &State| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: state.replacement,
                configuration: state.configuration,
            })
        },
        move |_| Child {
            cache: Cell::new(0),
            drops: std::sync::Arc::clone(&dropped),
        },
    );
    let (builder, site) = RootMount::new(Root)
        .builder(
            State {
                child: Some(Cell::new(4)),
                configuration: 0,
                root_configuration: 0,
                replacement: 0,
                selections: std::sync::Arc::clone(&selections),
            },
            Runtime::new(driver.clone()),
        )
        .mount_with_site(mount);
    let store = builder.build().unwrap();
    finish(&driver, store.ready()).unwrap();
    let mut before = Box::pin(store.acquire(&site));
    let mut replacement = Box::pin(store.send(Action::Replace));
    let mut after = Box::pin(store.acquire(&site));
    assert!(poll(before.as_mut()).is_pending());
    assert!(poll(replacement.as_mut()).is_pending());
    assert!(poll(after.as_mut()).is_pending());
    let before = finish(&driver, before).unwrap().unwrap();
    let _ = finish(&driver, replacement).unwrap();
    let after = finish(&driver, after).unwrap().unwrap();
    stale(&driver, &before, &selections);
    assert_eq!(
        drops.load(Ordering::SeqCst),
        1,
        "old endpoint does not retain Feature"
    );
    assert_eq!(finish(&driver, after.read(Cell::get)).unwrap(), 4);
    // Dropping an admitted response cannot manufacture a mutation or frontier.
    let mut abandoned = Box::pin(store.acquire(&site));
    assert!(poll(abandoned.as_mut()).is_pending());
    drop(abandoned);
    driver.turn();
    let _ = finish(&driver, after.send(ChildAction::BumpCache)).unwrap();
    let _ = finish(&driver, after.send(ChildAction::ReportCache)).unwrap();
    assert_eq!(finish(&driver, after.read(Cell::get)).unwrap(), 1);
    drop(store);
    driver.turn();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}
