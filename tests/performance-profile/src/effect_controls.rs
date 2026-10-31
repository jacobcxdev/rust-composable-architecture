//! Breaks real effect work and ownership to challenge the profiling oracles.
//!
//! These tests do not produce performance samples. The incomplete-work timeout
//! proves non-completion, not an elapsed-time cost or a scheduling order.

use super::*;

/// Independent omissions and corruptions that a checksum alone cannot all detect.
#[derive(Clone, Copy)]
enum Fault {
    Missing,
    /// Preserves the expected sum while repeating and omitting response identities.
    Duplicate,
    WrongMap,
    /// Commits a response but retains an unfinished causal branch.
    Incomplete,
    MissedTimer,
}

/// Uses the real reducer path while substituting exactly one faulty effect shape.
struct Broken(Fault, Config);

impl Reducer for Broken {
    type State = State;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.record(action, self.1);
        if !action.root {
            return Effect::none();
        }
        let response = |value| Action {
            value,
            remaining: 0,
            root: false,
            mapped: 0,
        };
        match self.0 {
            Fault::Missing => Effect::future(async { None }),
            // 2 + 2 + 5 has the same sum as 2 + 3 + 4; membership must reject it.
            Fault::Duplicate => Effect::merge([
                Effect::send(response(2)),
                Effect::send(response(2)),
                Effect::send(response(5)),
            ]),
            Fault::WrongMap => {
                Effect::future(async move { Some(response(2)) }).map(|mut output| {
                    output.mapped = 2;
                    output
                })
            }
            Fault::Incomplete => Effect::run(move |sender| async move {
                sender.send(response(2)).await.unwrap();
                std::future::pending::<()>().await;
            }),
            Fault::MissedTimer => Effect::none(),
        }
    }
}

/// Replaces the store on the same host; only state/work validation uses this adapter.
/// Its original reducer-owner probe is not an ownership oracle for the replacement.
fn broken(name: &str, size: &str, fault: Fault) -> (Adapter, Config) {
    let config = Config::parse([name, size, "1"].into_iter().map(str::to_owned)).unwrap();
    let mut adapter = Adapter::new(config).unwrap();
    let runtime = match &adapter.host {
        Host::Native(host) => {
            let _entered = host.enter();
            TokioRuntime::current().unwrap()
        }
        #[cfg(feature = "deterministic")]
        Host::Deterministic(host) => host.runtime(),
    };
    adapter.store = Store::builder(
        State {
            ordered: true,
            identities_valid: true,
            mapped_valid: true,
            seen: vec![false; usize::try_from(config.responses_per_root).unwrap()],
            ..State::default()
        },
        Broken(fault, config),
        runtime,
    )
    .build()
    .unwrap();
    (adapter, config)
}

#[test]
fn real_missing_duplicate_wrong_map_and_missed_timer_outputs_are_rejected() {
    for (name, size, fault) in [
        ("future-chain-native", "1", Fault::Missing),
        ("merge-dense-native", "3", Fault::Duplicate),
        ("map-deferred-native", "1", Fault::WrongMap),
        ("timers-at-native", "1", Fault::MissedTimer),
    ] {
        let (mut adapter, config) = broken(name, size, fault);
        profile_useful_work(&mut adapter, config).unwrap();
        assert!(adapter.validate_state(config).is_err(), "accepted {name}");
    }
}

#[test]
fn real_incomplete_branch_is_pending_and_rejected_without_host_timeout() {
    let (mut adapter, config) = broken("run-chain-native", "1", Fault::Incomplete);
    let task = adapter
        .host
        .run(adapter.store.send(Action {
            value: 1,
            remaining: 1,
            root: true,
            mapped: 0,
        }))
        .unwrap()
        .unwrap();
    adapter.roots = 1;
    assert!(
        adapter
            .host
            .run(async { tokio::time::timeout(Duration::from_millis(10), task.finish()).await })
            .unwrap()
            .is_err()
    );
    assert!(!task.is_finished());
    assert!(adapter.validate_state(config).is_err());
}

#[test]
fn actual_retained_future_and_mapper_captures_fail_owner_validation() {
    for name in ["future-chain-native", "map-deferred-native"] {
        let config = Config::parse([name, "1", "1"].into_iter().map(str::to_owned)).unwrap();
        let mut adapter = Adapter::new(config).unwrap();
        let owner = adapter.reducer_owner.upgrade().unwrap();
        let retained = if name.starts_with("future") {
            Effect::future(async move {
                std::hint::black_box(&owner);
                std::future::pending::<Option<Action>>().await
            })
        } else {
            Effect::future(async { None }).map(move |action: Action| {
                std::hint::black_box(&owner);
                action
            })
        };
        profile_useful_work(&mut adapter, config).unwrap();
        let state = adapter.validate_state(config).unwrap();
        assert!(adapter.teardown(state).unwrap().validate(config).is_err());
        drop(retained);
    }
}

#[cfg(feature = "deterministic")]
#[test]
fn unadvanced_actor_timer_remains_pending_after_bounded_drain() {
    let config = Config::parse(
        ["timers-at-deterministic", "1", "1"]
            .into_iter()
            .map(str::to_owned),
    )
    .unwrap();
    let mut adapter = Adapter::new(config).unwrap();
    let task = adapter
        .host
        .run(adapter.store.send(Action {
            value: 1,
            remaining: 1,
            root: true,
            mapped: 0,
        }))
        .unwrap()
        .unwrap();
    adapter.roots = 1;
    let Host::Deterministic(host) = &adapter.host else {
        panic!("wrong host")
    };
    host.run_until_stalled().unwrap();
    let mut finish = std::pin::pin!(task.finish());
    assert!(
        finish
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    assert!(!task.is_finished());
    assert!(!host.pending_sleeps().is_empty());
    assert!(adapter.validate_state(config).is_err());
}

#[cfg(feature = "deterministic")]
#[test]
fn self_waking_stalled_task_exhausts_exact_poll_budget() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let host = DeterministicRuntime::with_step_budget(7);
    let polls = Arc::new(AtomicUsize::new(0));
    let witness = Arc::clone(&polls);
    host.spawn_task(Box::pin(std::future::poll_fn(move |context| {
        witness.fetch_add(1, Ordering::Relaxed);
        context.waker().wake_by_ref();
        std::task::Poll::<()>::Pending
    })));
    let error = host.run_until_stalled().unwrap_err();
    assert_eq!(error.polls(), 7);
    assert_eq!(error.step_budget(), 7);
    assert_eq!(polls.load(Ordering::Relaxed), 7);
}
