//! Tests polling against `TestStore`'s virtual clock, not elapsed wall-clock time.
//!
//! Queued replies keep queries pending until each case supplies a result. Closed
//! reply channels prove cancellation; unexpected calls exhaust the fixture queue.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_channel::{Receiver, Sender};
use futures_executor::block_on;
use rca_example_mcmd_server::polling::{
    Action, Polling, QueryClient, QueryError, QueryFuture, QueryKey, State,
};
use rca_test::TestStore;

type Response = Result<u32, QueryError>;

#[derive(Default)]
struct Client {
    calls: Mutex<Vec<String>>,
    responses: Mutex<VecDeque<Receiver<Response>>>,
}

impl Client {
    fn queue(&self) -> Sender<Response> {
        let (sender, receiver) = async_channel::bounded(1);
        self.responses.lock().unwrap().push_back(receiver);
        sender
    }

    fn assert_calls(&self, expected: &[&str]) {
        assert_eq!(*self.calls.lock().unwrap(), expected);
    }
}

impl QueryClient for Client {
    fn poll(&self, server_id: String) -> QueryFuture {
        self.calls.lock().unwrap().push(server_id);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected query call");
        Box::pin(async move { response.recv().await.expect("query response retained") })
    }
}

fn state() -> State {
    State {
        server_id: "s1".into(),
        active: false,
        querying: false,
        player_count: 0,
        failures: 0,
        last_error: None,
    }
}

fn store(state: State, client: &Arc<Client>) -> TestStore<State, Action> {
    TestStore::builder(state, Polling)
        .with_dependency::<QueryKey>(Arc::clone(client) as Arc<dyn QueryClient>)
        .build()
}

// Missing leading output, wrong timer cadence or omitted result mutation must fail.
#[test]
fn leading_poll_and_five_second_cadence() {
    block_on(async {
        let client = Arc::new(Client::default());
        let first = client.queue();
        let second = client.queue();
        let mut store = store(state(), &client);
        let _ = store
            .send_with(Action::Start, |s| s.active = true)
            .await
            .unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Tick, |s| s.querying = true)
            .await
            .unwrap();
        store.assert_state(|s| assert!(s.querying)).await.unwrap();
        client.assert_calls(&["s1"]);
        first.try_send(Ok(3)).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Polled(Ok(3)),
                |s| {
                    s.querying = false;
                    s.player_count = 3;
                },
            )
            .await
            .unwrap();
        store.advance(Duration::from_secs(4)).await.unwrap();
        assert!(
            store
                .receive(|_| true)
                .await
                .unwrap_err()
                .is_no_received_action()
        );
        store.advance(Duration::from_secs(1)).await.unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Tick, |s| s.querying = true)
            .await
            .unwrap();
        second.try_send(Ok(5)).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Polled(Ok(5)),
                |s| {
                    s.querying = false;
                    s.player_count = 5;
                },
            )
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Stop, |s| s.active = false)
            .await
            .unwrap();
        store.finish().await.unwrap();
        client.assert_calls(&["s1", "s1"]);
    });
}

async fn receive_tick(store: &mut TestStore<State, Action>) {
    let _ = store
        .receive_with(|a| *a == Action::Tick, |s| s.querying = true)
        .await
        .unwrap();
    store.assert_state(|s| assert!(s.querying)).await.unwrap();
}

// Duplicate timers or lack of the in-flight guard would create unexpected calls.
#[test]
fn duplicate_start_and_pending_ticks_do_not_accumulate_queries() {
    block_on(async {
        let client = Arc::new(Client::default());
        let first = client.queue();
        let mut store = store(state(), &client);
        let _ = store
            .send_with(Action::Start, |s| s.active = true)
            .await
            .unwrap();
        receive_tick(&mut store).await;
        let _ = store.send(Action::Start).await.unwrap();
        store.advance(Duration::from_secs(15)).await.unwrap();
        for _ in 0..3 {
            let _ = store.receive(|a| *a == Action::Tick).await.unwrap();
        }
        assert!(
            store
                .receive(|_| true)
                .await
                .unwrap_err()
                .is_no_received_action()
        );
        client.assert_calls(&["s1"]);
        first.try_send(Ok(7)).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Polled(Ok(7)),
                |s| {
                    s.querying = false;
                    s.player_count = 7;
                },
            )
            .await
            .unwrap();
        let second = client.queue();
        store.advance(Duration::from_secs(5)).await.unwrap();
        receive_tick(&mut store).await;
        client.assert_calls(&["s1", "s1"]);
        second.try_send(Ok(8)).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Polled(Ok(8)),
                |s| {
                    s.querying = false;
                    s.player_count = 8;
                },
            )
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Stop, |s| s.active = false)
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

// Wrapping failure counts, clearing the last good count, or retaining an error on success fails.
#[test]
fn typed_errors_saturate_and_success_clears_failure_state() {
    block_on(async {
        for (error, diagnostic) in [
            (QueryError::ConnectionFailed, "ConnectionFailed"),
            (
                QueryError::Protocol("bad \"packet\"\n".into()),
                "Protocol(\"bad \\\"packet\\\"\\n\")",
            ),
            (
                QueryError::Other("query unavailable".into()),
                "Other(\"query unavailable\")",
            ),
        ] {
            for (initial_failures, expected_failures) in [(0, 1), (u32::MAX, u32::MAX)] {
                let client = Arc::new(Client::default());
                let failed = client.queue();
                let succeeded = client.queue();
                let mut initial = state();
                initial.player_count = 9;
                initial.failures = initial_failures;
                let mut store = store(initial, &client);
                let _ = store
                    .send_with(Action::Start, |s| s.active = true)
                    .await
                    .unwrap();
                receive_tick(&mut store).await;
                failed.try_send(Err(error.clone())).unwrap();
                let expected_error = error.clone();
                let _ = store
                    .receive_with(
                        move |a| *a == Action::Polled(Err(expected_error.clone())),
                        move |s| {
                            s.querying = false;
                            s.failures = expected_failures;
                            s.last_error = Some(diagnostic.into());
                        },
                    )
                    .await
                    .unwrap();
                store.advance(Duration::from_secs(5)).await.unwrap();
                receive_tick(&mut store).await;
                succeeded.try_send(Ok(4)).unwrap();
                let _ = store
                    .receive_with(
                        |a| *a == Action::Polled(Ok(4)),
                        |s| {
                            s.querying = false;
                            s.player_count = 4;
                            s.failures = 0;
                            s.last_error = None;
                        },
                    )
                    .await
                    .unwrap();
                let _ = store
                    .send_with(Action::Stop, |s| s.active = false)
                    .await
                    .unwrap();
                store.finish().await.unwrap();
                client.assert_calls(&["s1", "s1"]);
            }
        }
    });
}

// Removing cancellation leaves the old response receiver alive after Stop.
#[test]
fn stop_drops_pending_query_and_restart_uses_new_lifetime() {
    block_on(async {
        let client = Arc::new(Client::default());
        let old = client.queue();
        let new = client.queue();
        let mut store = store(state(), &client);
        let old_task = store
            .send_with(Action::Start, |s| s.active = true)
            .await
            .unwrap();
        receive_tick(&mut store).await;
        let _ = store
            .send_with(Action::Stop, |s| {
                s.active = false;
                s.querying = false;
            })
            .await
            .unwrap();
        store.assert_state(|s| assert!(!s.active)).await.unwrap();
        assert!(
            old.is_closed(),
            "stopped query must release its response receiver"
        );
        assert!(old_task.is_finished(), "stopped generation must finish");
        assert!(old.try_send(Ok(99)).is_err());
        let _ = store
            .send_with(Action::Start, |s| s.active = true)
            .await
            .unwrap();
        receive_tick(&mut store).await;
        new.try_send(Ok(2)).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Polled(Ok(2)),
                |s| {
                    s.querying = false;
                    s.player_count = 2;
                },
            )
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Stop, |s| s.active = false)
            .await
            .unwrap();
        store.advance(Duration::from_secs(30)).await.unwrap();
        store.finish().await.unwrap();
        client.assert_calls(&["s1", "s1"]);
    });
}

// An inactive tick must not call the dependency, and an inactive result must not mutate data.
#[test]
fn inactive_actions_preserve_observations_without_work() {
    block_on(async {
        let client = Arc::new(Client::default());
        let mut initial = state();
        initial.player_count = 6;
        initial.failures = 2;
        initial.last_error = Some("previous failure".into());
        let mut store = store(initial, &client);
        let _ = store.send(Action::Stop).await.unwrap();
        let _ = store.send(Action::Tick).await.unwrap();
        let _ = store.send(Action::Polled(Ok(99))).await.unwrap();
        let _ = store
            .send(Action::Polled(Err(QueryError::Other("obsolete".into()))))
            .await
            .unwrap();
        store.advance(Duration::from_secs(30)).await.unwrap();
        store.finish().await.unwrap();
        client.assert_calls(&[]);
    });
}

// Stopping must retain observations and cancel future timer deliveries.
#[test]
fn stop_preserves_result_and_prevents_future_ticks() {
    block_on(async {
        let client = Arc::new(Client::default());
        let response = client.queue();
        let mut store = store(state(), &client);
        let _ = store
            .send_with(Action::Start, |s| s.active = true)
            .await
            .unwrap();
        receive_tick(&mut store).await;
        response.try_send(Ok(12)).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Polled(Ok(12)),
                |s| {
                    s.querying = false;
                    s.player_count = 12;
                },
            )
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Stop, |s| s.active = false)
            .await
            .unwrap();
        let _ = store.send(Action::Stop).await.unwrap();
        store.advance(Duration::from_secs(30)).await.unwrap();
        store.finish().await.unwrap();
        client.assert_calls(&["s1"]);
    });
}

// Cancelling the first environment must leave the second query and timer live.
#[test]
fn stores_have_independent_clients_and_cancellation() {
    block_on(async {
        let first = Arc::new(Client::default());
        let second = Arc::new(Client::default());
        let first_response = first.queue();
        let second_response = second.queue();
        let mut first_store = store(state(), &first);
        let mut second_state = state();
        second_state.server_id = "s2".into();
        let mut second_store = store(second_state, &second);
        let _ = first_store
            .send_with(Action::Start, |s| s.active = true)
            .await
            .unwrap();
        receive_tick(&mut first_store).await;
        let _ = second_store
            .send_with(Action::Start, |s| s.active = true)
            .await
            .unwrap();
        receive_tick(&mut second_store).await;
        let _ = first_store
            .send_with(Action::Stop, |s| {
                s.active = false;
                s.querying = false;
            })
            .await
            .unwrap();
        first_store.finish().await.unwrap();
        assert!(first_response.is_closed());
        assert!(!second_response.is_closed());
        second_response.try_send(Ok(21)).unwrap();
        let _ = second_store
            .receive_with(
                |a| *a == Action::Polled(Ok(21)),
                |s| {
                    s.querying = false;
                    s.player_count = 21;
                },
            )
            .await
            .unwrap();
        let next_response = second.queue();
        second_store.advance(Duration::from_secs(5)).await.unwrap();
        receive_tick(&mut second_store).await;
        next_response.try_send(Ok(22)).unwrap();
        let _ = second_store
            .receive_with(
                |a| *a == Action::Polled(Ok(22)),
                |s| {
                    s.querying = false;
                    s.player_count = 22;
                },
            )
            .await
            .unwrap();
        let _ = second_store
            .send_with(Action::Stop, |s| s.active = false)
            .await
            .unwrap();
        second_store.finish().await.unwrap();
        first.assert_calls(&["s1"]);
        second.assert_calls(&["s2", "s2"]);
    });
}

// Cancellation must reach queries descended from interval ticks, not only the leading send.
#[test]
fn stop_cancels_query_descended_from_interval_tick() {
    block_on(async {
        let client = Arc::new(Client::default());
        let leading = client.queue();
        let timed = client.queue();
        let mut store = store(state(), &client);
        let task = store
            .send_with(Action::Start, |s| s.active = true)
            .await
            .unwrap();
        receive_tick(&mut store).await;
        leading.try_send(Ok(3)).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Polled(Ok(3)),
                |s| {
                    s.querying = false;
                    s.player_count = 3;
                },
            )
            .await
            .unwrap();
        store.advance(Duration::from_secs(5)).await.unwrap();
        receive_tick(&mut store).await;
        client.assert_calls(&["s1", "s1"]);
        let _ = store
            .send_with(Action::Stop, |s| {
                s.active = false;
                s.querying = false;
            })
            .await
            .unwrap();
        store.finish().await.unwrap();
        assert!(timed.is_closed());
        assert!(timed.try_send(Ok(99)).is_err());
        assert!(task.is_finished());
    });
}
