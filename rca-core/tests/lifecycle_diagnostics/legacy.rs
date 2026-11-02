use super::{Events, ordered, poll, support};
use rca_core::diagnostics::{CancellationCause, LifecycleKind};
use rca_core::{ActionPath, Effect, OptionalStatePath, Reduce, ReducerExt, Store};
use std::cell::Cell;
use std::sync::{Arc, Mutex};

#[test]
fn composed_optional_scopes_report_real_insertions_and_stale_retirement_is_silent() {
    let runtime = support::ManualRuntime::new();
    let events = Arc::new(Mutex::new(Events::default()));
    let sink = Arc::clone(&events);
    let reducer = Reduce::new(|state: &mut Option<Cell<u8>>, action: &u8, _| {
        match action {
            1 => *state = None,
            2 => *state = Some(Cell::new(4)),
            _ => {}
        }
        Effect::none()
    })
    .if_some(
        OptionalStatePath::new(
            |state: &Option<Cell<u8>>| state.as_ref(),
            |state: &mut Option<Cell<u8>>| state.as_mut(),
        ),
        ActionPath::new(|action: &u8| (*action == 3).then_some(&()), |()| 3),
        Reduce::new(|_: &mut Cell<u8>, (): &(), _| Effect::none()),
    );
    let store = Store::builder(Some(Cell::new(4)), reducer, runtime.runtime())
        .with_lifecycle_diagnostics(move |event| sink.lock().unwrap().record(event))
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    assert!(poll(actor.as_mut()).is_pending());
    events.lock().unwrap().take();
    drop(ordered(&mut actor, store.send(0)).unwrap());
    let (installed, count) = events.lock().unwrap().take();
    assert_eq!(count, 1);
    let event = installed[0].unwrap();
    let LifecycleKind::Installed {
        lifetime: old,
        parent: Some(0),
        configuration: None,
    } = event.kind()
    else {
        panic!("legacy installation must not fabricate configuration metadata");
    };
    assert_eq!(event.last_committed_version(), 0);
    drop(ordered(&mut actor, store.send(0)).unwrap());
    assert_eq!(events.lock().unwrap().length, 0);
    drop(ordered(&mut actor, store.send(1)).unwrap());
    let (removed, count) = events.lock().unwrap().take();
    assert_eq!(count, 2);
    assert_eq!(
        removed[0].unwrap().kind(),
        LifecycleKind::Retired { lifetime: old }
    );
    assert_eq!(
        removed[1].unwrap().kind(),
        LifecycleKind::CancellationRequested {
            cause: CancellationCause::ScopeRetirement,
            scope: Some(old),
        }
    );
    assert_eq!(removed[0].unwrap().last_committed_version(), 2);
    assert_eq!(removed[1].unwrap().last_committed_version(), 2);
    drop(ordered(&mut actor, store.send(1)).unwrap());
    assert_eq!(events.lock().unwrap().length, 0);
    drop(ordered(&mut actor, store.send(2)).unwrap());
    let (installed, count) = events.lock().unwrap().take();
    assert_eq!(count, 1);
    assert!(
        matches!(installed[0].unwrap().kind(), LifecycleKind::Installed {
        lifetime, parent: Some(0), configuration: None,
    } if lifetime != old)
    );
    assert_eq!(installed[0].unwrap().last_committed_version(), 4);
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
}
