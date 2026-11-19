//! Compares handwritten and generated composition with controlled virtual time.
//!
//! Complete event and state expectations supplement equivalence: two equally
//! incorrect compositions must not pass merely because they match each other.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rca::{KeyedAction, TestStore};
use rca_example_composition_search::{
    AppAction, FavoriteAction, RowAction, SearchAction, SearchApi, SearchApiKey, TraceEvent,
    generated_reducer, handwritten_reducer, initial_state,
};

/// Owns the selected history without retaining live child resources.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Snapshot {
    /// Records reducer order and which child lifetime produced each event.
    events: Vec<TraceEvent>,
    /// Retains the latest submitted query independently of later mutations.
    query: String,
    /// Stores ordered IDs, titles, favourite counts and completed-work counts.
    rows: Vec<(u64, String, usize, usize)>,
    /// Records the presented row and its ticks, or completed dismissal.
    detail: Option<(u64, usize)>,
}

/// Projects state into independently owned values for exhaustive assertions.
fn snapshot(state: &rca_example_composition_search::AppState) -> Snapshot {
    Snapshot {
        events: state.events.clone(),
        query: state.search.query.clone(),
        rows: state
            .rows
            .iter()
            .map(|(id, row)| {
                (
                    *id,
                    row.title.clone(),
                    row.favorite.count,
                    row.completed_work,
                )
            })
            .collect(),
        detail: state
            .detail
            .as_ref()
            .map(|detail| (detail.row_id, detail.timer_ticks)),
    }
}

#[test]
fn same_key_reinsertion_owns_fresh_work() {
    run_reinsertion(handwritten_reducer());
    run_reinsertion(generated_reducer());
}

/// Passes the retired deadline before completing work for the same key's successor.
fn run_reinsertion<R>(reducer: R)
where
    R: rca::Reducer<State = rca_example_composition_search::AppState, Action = AppAction>
        + Send
        + 'static,
{
    let mut store = TestStore::projected_builder(initial_state(), reducer, snapshot)
        .with_dependency::<SearchApiKey>(SearchApi::fixed(["row"]))
        .build();
    futures_executor::block_on(async {
        let _ = store
            .send_with(
                AppAction::Search(SearchAction::Query("first".into())),
                |s| {
                    s.query = "first".into();
                    s.events.push(TraceEvent::SearchTyped);
                },
            )
            .await
            .unwrap();
        store.advance(Duration::from_millis(10)).await.unwrap();
        let _ = store
            .receive_with(
                |a| matches!(a, AppAction::Search(SearchAction::Results(_))),
                |s| {
                    s.rows = vec![(1, "row".into(), 0, 0)];
                    s.events.push(TraceEvent::SearchResults);
                },
            )
            .await
            .unwrap();
        let old = store
            .send_with(
                AppAction::Row(KeyedAction::new(1, RowAction::StartWork)),
                |s| {
                    s.events.push(TraceEvent::RowWorkStarted(1));
                },
            )
            .await
            .unwrap();
        let _ = store
            .send_with(AppAction::RemoveRow(1), |s| {
                s.rows.clear();
                s.events.push(TraceEvent::RowRemoved(1));
            })
            .await
            .unwrap();
        store
            .assert_state(|s| assert!(s.rows.is_empty()))
            .await
            .unwrap();
        assert!(
            old.is_finished(),
            "removal must finish old work before its deadline"
        );
        old.finish().await.unwrap();

        let _ = store
            .send_with(
                AppAction::Search(SearchAction::Query("replacement".into())),
                |s| {
                    s.query = "replacement".into();
                    s.events.push(TraceEvent::SearchTyped);
                },
            )
            .await
            .unwrap();
        store.advance(Duration::from_millis(10)).await.unwrap();
        let _ = store
            .receive_with(
                |a| matches!(a, AppAction::Search(SearchAction::Results(_))),
                |s| {
                    s.rows = vec![(1, "row".into(), 0, 0)];
                    s.events.push(TraceEvent::SearchResults);
                },
            )
            .await
            .unwrap();
        let fresh = store
            .send_with(
                AppAction::Row(KeyedAction::new(1, RowAction::StartWork)),
                |s| {
                    s.events.push(TraceEvent::RowWorkStarted(1));
                },
            )
            .await
            .unwrap();
        // The removed lifetime's deadline passes before the replacement's deadline.
        store.advance(Duration::from_millis(5)).await.unwrap();
        store
            .assert_state(|s| assert_eq!(s.rows.get(&1).unwrap().completed_work, 0))
            .await
            .unwrap();
        assert!(
            !fresh.is_finished(),
            "old deadline must not complete replacement work"
        );
        store.advance(Duration::from_millis(10)).await.unwrap();
        let _ = store.receive_with(
            |a| matches!(a, AppAction::Row(row) if *row.id() == 1 && matches!(row.action(), RowAction::WorkFinished)),
            |s| {
                s.rows[0].3 = 1;
                s.events.push(TraceEvent::RowWorkFinished(1));
            },
        ).await.unwrap();
        fresh.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn handwritten_and_generated_composition_have_identical_lifetimes() {
    let handwritten = run_trace(handwritten_reducer());
    let generated = run_trace(generated_reducer());

    assert_eq!(handwritten, generated);
    assert_eq!(handwritten.query, "current");
    assert_eq!(
        handwritten.rows,
        [(1, String::from("first"), 1, 1)],
        "only the surviving row remains, with its favourite and work counts"
    );
    assert_eq!(
        handwritten.events,
        [
            TraceEvent::SearchTyped,
            TraceEvent::SearchTyped,
            TraceEvent::SearchResults,
            TraceEvent::DetailOpened,
            TraceEvent::RowWorkStarted(1),
            TraceEvent::RowWorkStarted(2),
            TraceEvent::RowFavorited(1),
            TraceEvent::RowRemoved(2),
            TraceEvent::DetailClosed,
            TraceEvent::RowWorkFinished(1),
        ]
    );
}

/// Checks cancellation, sibling isolation and final state through public endpoints.
///
/// The final capture reads fields independently of `snapshot`, so projection
/// mistakes cannot silently substitute for the concrete final-state oracle.
#[expect(
    clippy::too_many_lines,
    reason = "one trace keeps its causal task assertions beside each lifecycle transition"
)]
fn run_trace<R>(reducer: R) -> Snapshot
where
    R: rca::Reducer<State = rca_example_composition_search::AppState, Action = AppAction>
        + Send
        + 'static,
{
    let mut store = TestStore::projected_builder(initial_state(), reducer, snapshot)
        .with_dependency::<SearchApiKey>(SearchApi::fixed(["first", "second"]))
        .build();

    futures_executor::block_on(async {
        let stale_search = store
            .send_with(
                AppAction::Search(SearchAction::Query(String::from("stale"))),
                |state| {
                    state.query = String::from("stale");
                    state.events.push(TraceEvent::SearchTyped);
                },
            )
            .await
            .expect("first search enters the Store");
        let current_search = store
            .send_with(
                AppAction::Search(SearchAction::Query(String::from("current"))),
                |state| {
                    state.query = String::from("current");
                    state.events.push(TraceEvent::SearchTyped);
                },
            )
            .await
            .expect("second search replaces the first");
        stale_search
            .finish()
            .await
            .expect("replaced search work finishes");
        assert!(stale_search.is_finished());
        store
            .advance(Duration::from_millis(10))
            .await
            .expect("search delay advances");
        let search_results = store
            .receive_with(
                |action| matches!(action, AppAction::Search(SearchAction::Results(_))),
                |state| {
                    state.rows = vec![
                        (1, String::from("first"), 0, 0),
                        (2, String::from("second"), 0, 0),
                    ];
                    state.events.push(TraceEvent::SearchResults);
                },
            )
            .await
            .expect("only the current search response is delivered");
        search_results
            .finish()
            .await
            .expect("search result action finishes");
        current_search
            .finish()
            .await
            .expect("current search work finishes");
        store
            .assert_state(|state| {
                assert_eq!(
                    state
                        .rows
                        .iter()
                        .map(|(_, row)| row.title.as_str())
                        .collect::<Vec<_>>(),
                    ["first", "second"],
                    "test dependency must override the feature-local live value"
                );
            })
            .await
            .expect("test dependency results remain observable");

        let open_detail = store
            .send_with(AppAction::OpenDetail(1), |state| {
                state.detail = Some((1, 0));
                state.events.push(TraceEvent::DetailOpened);
            })
            .await
            .expect("detail opens");
        let detail_timer = store
            .receive(|action| {
                matches!(
                    action,
                    AppAction::Detail(rca_example_composition_search::DetailAction::StartTimer)
                )
            })
            .await
            .expect("detail timer starts through the mounted child");
        let first_row_work = store
            .send_with(
                AppAction::Row(KeyedAction::new(1, RowAction::StartWork)),
                |state| state.events.push(TraceEvent::RowWorkStarted(1)),
            )
            .await
            .expect("first keyed row starts work");
        let second_row_work = store
            .send_with(
                AppAction::Row(KeyedAction::new(2, RowAction::StartWork)),
                |state| state.events.push(TraceEvent::RowWorkStarted(2)),
            )
            .await
            .expect("second keyed row starts equal local work");
        let favorite = store
            .send_with(
                AppAction::Row(KeyedAction::new(
                    1,
                    RowAction::Favorite(FavoriteAction::Toggle),
                )),
                |state| {
                    state.rows[0].2 = 1;
                    state.events.push(TraceEvent::RowFavorited(1));
                },
            )
            .await
            .expect("nested favorite is routed");
        favorite
            .finish()
            .await
            .expect("nested favorite action finishes");
        let remove_row = store
            .send_with(AppAction::RemoveRow(2), |state| {
                state.rows.remove(1);
                state.events.push(TraceEvent::RowRemoved(2));
            })
            .await
            .expect("second keyed row departs");
        remove_row
            .finish()
            .await
            .expect("second keyed row removal finishes");
        second_row_work
            .finish()
            .await
            .expect("departed keyed row work is cancelled");
        assert!(second_row_work.is_finished());
        assert!(!first_row_work.is_finished());
        let close_detail = store
            .send_with(AppAction::CloseDetail, |state| {
                state.detail = None;
                state.events.push(TraceEvent::DetailClosed);
            })
            .await
            .expect("detail closes before its timer fires");
        close_detail.finish().await.expect("detail close finishes");
        detail_timer
            .finish()
            .await
            .expect("departed detail timer is cancelled");
        assert!(detail_timer.is_finished());
        open_detail
            .finish()
            .await
            .expect("detail presentation lineage finishes");
        store
            .advance(Duration::from_millis(30))
            .await
            .expect("surviving row work advances");
        let surviving_row_work = store
            .receive_with(
                |action| {
                    matches!(
                        action,
                        AppAction::Row(keyed)
                            if *keyed.id() == 1 && matches!(keyed.action(), RowAction::WorkFinished)
                    )
                },
                |state| {
                    state.rows[0].3 = 1;
                    state.events.push(TraceEvent::RowWorkFinished(1));
                },
            )
            .await
            .expect("only work from the surviving keyed row is delivered");
        surviving_row_work
            .finish()
            .await
            .expect("surviving row completion finishes");
        first_row_work
            .finish()
            .await
            .expect("surviving keyed row work finishes");
        assert!(first_row_work.is_finished());

        let snapshot = Arc::new(Mutex::new(None));
        let capture = Arc::clone(&snapshot);
        store
            .assert_state(move |state| {
                let rows = state
                    .rows
                    .iter()
                    .map(|(id, row)| {
                        (
                            *id,
                            row.title.clone(),
                            row.favorite.count,
                            row.completed_work,
                        )
                    })
                    .collect();
                *capture.lock().expect("test snapshot lock") = Some(Snapshot {
                    events: state.events.clone(),
                    query: state.search.query.clone(),
                    rows,
                    detail: state
                        .detail
                        .as_ref()
                        .map(|detail| (detail.row_id, detail.timer_ticks)),
                });
                assert!(state.detail.is_none());
            })
            .await
            .expect("state remains readable");
        store.finish().await.expect("all surviving work finishes");
        snapshot
            .lock()
            .expect("test snapshot lock")
            .take()
            .expect("state assertion captured a snapshot")
    })
}
