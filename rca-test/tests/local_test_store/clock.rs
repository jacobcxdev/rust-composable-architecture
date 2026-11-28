//! Local client waits and actor timers retain their original `Rc` action payloads.

use std::cell::OnceCell;
use std::rc::Rc;
use std::time::Duration;

use futures_executor::block_on;
use rca_core::local::{Clock, Effect, Reducer};
use rca_core::{DependencyKey, ReduceContext};
use rca_test::local::TestStore;

/// Uses the clock factory to configure the fixture without a live fallback.
struct Configured;

impl DependencyKey for Configured {
    type Value = ();
    fn live_value() {
        panic!("explicit clock configuration required");
    }
}

/// Carries the same allocation through independently scheduled response paths.
#[derive(Debug)]
enum Action {
    Start(Rc<usize>),
    Client(Rc<usize>),
    Actor(Rc<usize>),
}

/// Receives its clock at build time and emits client and actor responses.
struct Timed(Rc<OnceCell<Clock>>);

impl Reducer for Timed {
    type State = Vec<usize>;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Vec<usize>,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start(value) => {
                let wait = self.0.get().unwrap().sleep(Duration::from_millis(10));
                let client = Rc::clone(value);
                Effect::merge([
                    Effect::future(async move {
                        wait.await;
                        Some(Action::Client(client))
                    }),
                    Effect::send_after(Duration::from_millis(15), Action::Actor(Rc::clone(value))),
                ])
            }
            Action::Client(value) | Action::Actor(value) => {
                state.push(**value);
                Effect::none()
            }
        }
    }
}

#[test]
fn actual_local_client_and_actor_share_virtual_time_without_releasing_actions() {
    block_on(async {
        let clock = Rc::new(OnceCell::new());
        let configured = Rc::clone(&clock);
        let mut store = TestStore::builder(Vec::new(), Timed(Rc::clone(&clock)))
            .with_clock_dependency::<Configured>(move |clock| {
                configured.set(clock).unwrap();
            })
            .build();
        let value = Rc::new(8);
        let started = store.send(Action::Start(Rc::clone(&value))).await.unwrap();
        store.advance(Duration::from_millis(9)).await.unwrap();
        let error = store
            .receive(|_| true)
            .await
            .expect_err("neither deadline has elapsed");
        assert!(error.is_no_received_action());
        store.advance(Duration::from_millis(1)).await.unwrap();
        let client = store
            .receive_with(
                |action| matches!(action, Action::Client(actual) if Rc::ptr_eq(actual, &value)),
                |state| state.push(8),
            )
            .await
            .unwrap();
        store.advance(Duration::from_millis(5)).await.unwrap();
        let actor = store
            .receive_with(
                |action| matches!(action, Action::Actor(actual) if Rc::ptr_eq(actual, &value)),
                |state| state.push(8),
            )
            .await
            .unwrap();
        actor.finish().await.unwrap();
        client.finish().await.unwrap();
        started.finish().await.unwrap();
        assert_eq!(store.now(), clock.get().unwrap().now());
        store.finish().await.unwrap();
        assert_eq!(Rc::strong_count(&value), 1);
    });
}
