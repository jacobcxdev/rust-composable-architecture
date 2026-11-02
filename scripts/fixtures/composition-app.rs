//! Keeps the historical lifecycle trace common to both measured product revisions.
//!
//! The current product uses a unit projection to adapt its exhaustive TestStore
//! constructor. Both products retain the same actions, waits and final assertions.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rca::{KeyedAction, TestStore};
use rca_example_composition_search::{
    AppAction, FavoriteAction, RowAction, SearchAction, SearchApi, SearchApiKey, TraceEvent,
    generated_reducer, handwritten_reducer, initial_state,
};

/// Records the final state and event order for each composition style.
#[derive(Debug, Eq, PartialEq)]
struct Snapshot {
    /// Lists the observed events in order.
    events: Vec<TraceEvent>,
    /// Keeps the current query after its earlier request is cancelled.
    query: String,
    /// Records each surviving row's ID, name, favourite count and completed work.
    rows: Vec<(u64, String, usize, usize)>,
}

/// Checks that both composition styles retain the same state and task lifetimes.
#[test]
fn handwritten_and_generated_composition_have_identical_lifetimes() {
    let handwritten = run_trace(handwritten_reducer());
    let generated = run_trace(generated_reducer());

    assert_eq!(handwritten, generated);
    assert_eq!(handwritten.query, "current");
    assert_eq!(handwritten.rows, [(1, String::from("first"), 1, 1)]);
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

/// Runs the shared action trace and checks cancellation and child removal.
#[expect(
    clippy::too_many_lines,
    reason = "one trace keeps its causal task assertions beside each lifecycle transition"
)]
fn run_trace<R>(reducer: R) -> Snapshot
where
    R: rca::Reducer<State = rca_example_composition_search::AppState, Action = AppAction>,
{
    let mut store = TestStore::builder(initial_state(), reducer)
        .with_dependency::<SearchApiKey>(SearchApi::fixed(["first", "second"]))
        .build();

    futures_executor::block_on(async {
        let stale_search = store
            .send(AppAction::Search(SearchAction::Query(String::from(
                "stale",
            ))))
            .await
            .expect("first search enters the Store");
        let current_search = store
            .send(AppAction::Search(SearchAction::Query(String::from(
                "current",
            ))))
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
            .receive(|action| matches!(action, AppAction::Search(SearchAction::Results(_))))
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
            .send(AppAction::OpenDetail(1))
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
            .send(AppAction::Row(KeyedAction::new(1, RowAction::StartWork)))
            .await
            .expect("first keyed row starts work");
        let second_row_work = store
            .send(AppAction::Row(KeyedAction::new(2, RowAction::StartWork)))
            .await
            .expect("second keyed row starts equal local work");
        let favorite = store
            .send(AppAction::Row(KeyedAction::new(
                1,
                RowAction::Favorite(FavoriteAction::Toggle),
            )))
            .await
            .expect("nested favorite is routed");
        favorite
            .finish()
            .await
            .expect("nested favorite action finishes");
        let remove_row = store
            .send(AppAction::RemoveRow(2))
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
            .send(AppAction::CloseDetail)
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
            .receive(|action| {
                matches!(
                    action,
                    AppAction::Row(keyed)
                        if *keyed.id() == 1 && matches!(keyed.action(), RowAction::WorkFinished)
                )
            })
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
