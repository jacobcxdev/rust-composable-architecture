//! Diagnostics through the actual private nested actor compiler gate.
use super::*;
use crate::diagnostics::{CancellationCause, LifecycleEvent, LifecycleKind};

/// Requires admission to suspend, then completes it with exactly one actor turn.
fn finish<F: Future>(driver: &Driver, future: F) -> F::Output {
    let mut future = Box::pin(future);
    assert!(poll(future.as_mut()).is_pending());
    driver.turn();
    match poll(future.as_mut()) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("finite actor request remained pending"),
    }
}

/// Checks installation metadata before returning the new lifetime for later assertions.
fn installed(event: LifecycleEvent, expected_parent: Option<u64>, revision: u64) -> u64 {
    assert_eq!(event.last_committed_version(), revision);
    let LifecycleKind::Installed {
        lifetime,
        parent,
        configuration: Some(0),
    } = event.kind()
    else {
        panic!("actual native installation must carry its stored configuration");
    };
    assert_eq!(parent, expected_parent);
    lifetime
}

/// Checks the complete retirement, replacement and cancellation event sequence.
fn replacement(events: &[LifecycleEvent], old: (u64, u64), revision: u64) -> (u64, u64) {
    assert_eq!(events.len(), 6);
    assert!(
        events
            .iter()
            .all(|event| event.last_committed_version() == revision),
        "expected prior commit {revision}, got {events:?}"
    );
    assert_eq!(events[0].kind(), LifecycleKind::Retired { lifetime: old.1 });
    assert_eq!(events[1].kind(), LifecycleKind::Retired { lifetime: old.0 });
    let parent = installed(events[2], Some(0), revision);
    let child = installed(events[3], Some(parent), revision);
    assert_ne!(parent, old.0);
    assert_ne!(child, old.1);
    for (event, scope) in events[4..].iter().zip([old.1, old.0]) {
        assert_eq!(
            event.kind(),
            LifecycleKind::CancellationRequested {
                cause: CancellationCause::ScopeRetirement,
                scope: Some(scope),
            }
        );
    }
    (parent, child)
}

#[test]
fn actual_nested_retirement_is_child_first_and_legacy_leave_does_not_duplicate_it() {
    let driver = Driver::default();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let make_trace = Arc::clone(&trace);
    let recipe_trace = Arc::clone(&trace);
    let (owner, parent_site) =
        super::super::nested::attach_selected_key::<State, RootAction, RowFeature, _, _>(
            |state| &state.rows,
            |state| &mut state.rows,
            "one".to_owned(),
            move |_| RowFeature(Arc::clone(&make_trace)),
            move |_, children| {
                let trace = Arc::clone(&recipe_trace);
                let _ = children.optional(OptionalMount::new(
                    |row: &Row| row.child.as_ref(),
                    |row: &mut Row| row.child.as_mut(),
                    |row: &Row| {
                        row.child.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    move |_| ChildFeature {
                        cache: Cell::new(0),
                        trace: Arc::clone(&trace),
                    },
                ));
            },
        );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let store = RootMount::new(RootFeature)
        .builder(
            State {
                rows: [(
                    "one".to_owned(),
                    Row {
                        child: Some(Cell::new(4)),
                    },
                )]
                .into_iter()
                .collect(),
                configuration: 0,
            },
            Runtime::new(driver.clone()),
        )
        .with_mount(owner)
        .with_lifecycle_diagnostics(move |event| sink.lock().push(*event))
        .build()
        .unwrap();
    driver.turn();
    let initial = std::mem::take(&mut *events.lock());
    assert_eq!(initial.len(), 3);
    assert_eq!(installed(initial[0], None, 0), 0);
    let parent = installed(initial[1], Some(0), 0);
    let child = installed(initial[2], Some(parent), 0);
    drop(finish(&driver, store.send(RootAction::Replace)).unwrap());
    // The native installation is structural commit 1; its own facts precede it.
    let next = replacement(&std::mem::take(&mut *events.lock()), (parent, child), 1);
    let parent = finish(&driver, store.acquire(&parent_site))
        .unwrap()
        .unwrap();
    drop(finish(&driver, parent.send(RowAction::Leave)).unwrap());
    replacement(&std::mem::take(&mut *events.lock()), next, 2);
    drop(finish(&driver, store.send(RootAction::Noop)).unwrap());
    assert!(
        events.lock().is_empty(),
        "an unchanged roster emits no lifetime fact"
    );
    drop(store);
    driver.turn();
}
