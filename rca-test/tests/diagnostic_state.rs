//! Checks that diagnostics reveal metadata without running work or exposing domain values.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_executor::block_on;
use rca_core::{Effect, MonotonicInstant, ReduceContext, Reducer};
use rca_test::{CaptureKey, DeterministicRuntime, TestStore};

#[test]
fn runtime_diagnostics_do_not_poll_ready_work_or_advance_pending_time() {
    let runtime = DeterministicRuntime::with_step_budget(8);
    let driver = runtime.runtime();
    let started = Arc::new(AtomicUsize::new(0));
    let first_poll = Arc::clone(&started);
    let completed = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&completed);
    let clock = driver.clock();
    runtime.spawn_task(Box::pin(async move {
        first_poll.fetch_add(1, Ordering::SeqCst);
        clock.sleep(Duration::from_secs(3)).await;
        observed.fetch_add(1, Ordering::SeqCst);
    }));

    assert_eq!(format!("{driver:?}"), "Runtime(DeterministicDriver { .. })");
    let ready = format!("{runtime:?}");
    assert!(ready.contains("step_budget: 8"));
    assert!(ready.contains("ready_task_ids: [0]"));
    assert!(ready.contains("pending_sleeps: []"));
    assert_eq!(started.load(Ordering::SeqCst), 0);
    assert!(runtime.pending_sleeps().is_empty());
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.now(), MonotonicInstant::ZERO);

    runtime.run_until_stalled().unwrap();
    assert_eq!(started.load(Ordering::SeqCst), 1);
    let sleeps = runtime.pending_sleeps();
    assert_eq!(sleeps.len(), 1);
    let pending = format!("{runtime:?}");
    assert!(pending.contains("ready_task_ids: []"));
    assert!(pending.contains("pending_sleeps: [PendingSleep"));
    assert_eq!(runtime.pending_sleeps(), sleeps);
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.now(), MonotonicInstant::ZERO);

    runtime.advance(Duration::from_secs(3)).unwrap();
    let finished = format!("{runtime:?}");
    assert!(finished.contains("ready_task_ids: []"));
    assert!(finished.contains("pending_sleeps: []"));
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    runtime.assert_no_pending_time().unwrap();
}

/// Holds text that diagnostic output must not expose.
struct SecretState(String);

/// Starts pending work or marks its delayed completion.
enum Action {
    /// Schedules the delayed completion.
    Start,
    /// Ends the scheduled work without changing the secret.
    Done,
}

/// Counts reducer calls to detect work triggered by diagnostic formatting.
struct Feature(Arc<AtomicUsize>);

impl Reducer for Feature {
    type State = SecretState;
    type Action = Action;

    fn reduce(
        &mut self,
        _: &mut SecretState,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        self.0.fetch_add(1, Ordering::SeqCst);
        match action {
            Action::Start => Effect::send_after(Duration::from_secs(5), Action::Done),
            Action::Done => Effect::none(),
        }
    }
}

/// Declares a captured secret that diagnostics must not render.
struct SecretCapture;

impl CaptureKey for SecretCapture {
    type Value = String;
    type Error = Infallible;
}

#[test]
fn store_diagnostics_hide_domain_and_capture_values_without_running_callbacks() {
    let commits = Arc::new(AtomicUsize::new(0));
    let captures = Arc::new(AtomicUsize::new(0));
    let capture_calls = Arc::clone(&captures);
    let builder = TestStore::projected_builder(
        SecretState("private-capture-value".to_owned()),
        Feature(Arc::clone(&commits)),
        |_| (),
    )
    .with_capture::<SecretCapture>(move |state| {
        capture_calls.fetch_add(1, Ordering::SeqCst);
        Ok(state.0.clone())
    })
    .unwrap();
    let before = captures.load(Ordering::SeqCst);
    let configured = format!("{builder:?}");
    assert!(configured.starts_with("TestStoreBuilder {"));
    assert!(configured.contains("mode: Exhaustive"));
    assert!(!configured.contains("private-capture-value"));
    assert_eq!(captures.load(Ordering::SeqCst), before);
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    let mut store = builder.build();

    block_on(async {
        let before = captures.load(Ordering::SeqCst);
        let ready = format!("{store:?}");
        assert!(ready.starts_with("TestStore {"));
        assert!(!ready.contains("private-capture-value"));
        assert_eq!(captures.load(Ordering::SeqCst), before);
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        let task = store
            .send_with_captures(
                Action::Start,
                |()| {},
                |expected| {
                    let before = format!("{expected:?}");
                    assert_eq!(before, "CaptureExpectations { declarations: 1, .. }");
                    expected.set::<SecretCapture>("private-capture-value".to_owned())?;
                    assert_eq!(format!("{expected:?}"), before);
                    Ok(())
                },
            )
            .await
            .unwrap();
        let before = captures.load(Ordering::SeqCst);
        let pending = format!("{store:?} {task:?}");
        assert!(pending.contains("TestActionTask { task: ActionTask::Live"));
        assert!(!pending.contains("private-capture-value"));
        assert_eq!(captures.load(Ordering::SeqCst), before);
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert!(!task.is_finished());
        assert!(!task.is_cancelled());
        task.cancel().await.unwrap();
        task.finish().await.unwrap();
        assert!(task.is_cancelled());
        assert!(task.is_finished());
        assert!(format!("{task:?}").contains("task: ActionTask::Live"));
        store.finish().await.unwrap();
        assert_eq!(commits.load(Ordering::SeqCst), 1);
    });
}
