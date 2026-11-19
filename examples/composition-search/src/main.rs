//! Observes a keyed child until removing that child ends the subscription.
//!
//! Run with `cargo run -p rca-example-composition-search`.
//! The assertions check the actual Store observation and shutdown sequence.

use std::error::Error;

use rca::{Store, TokioRuntime};
use rca_example_composition_search::{
    AppAction, AppActionPaths, AppStatePaths, FavoriteAction, RowAction, SearchAction,
    generated_reducer, initial_state,
};

/// Runs the keyed-observation trace and waits for Store shutdown.
///
/// # Errors
///
/// Returns runtime, Store or scoped-access failures from the trace.
///
/// # Panics
///
/// Panics if committed values or observation lifetime differ from the trace.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let store = Store::new(
        initial_state(),
        generated_reducer(),
        TokioRuntime::current()?,
    );
    store
        .send(AppAction::Search(SearchAction::Query(String::from(
            "runtime",
        ))))
        .await?
        .finish()
        .await;

    let row = store
        .scope_keyed(AppStatePaths::rows(), AppActionPaths::row(), 1)
        .await?;
    let mut favorite_count = row.observe_distinct(|state| state.favorite.count).await?;
    assert_eq!(favorite_count.next().await, Some(0));
    row.send(RowAction::Favorite(FavoriteAction::Toggle))
        .await?
        .finish()
        .await;
    assert_eq!(row.read(|state| state.favorite.count).await?, 1);
    assert_eq!(favorite_count.next().await, Some(1));

    store.send(AppAction::RemoveRow(1)).await?.finish().await;
    assert_eq!(favorite_count.next().await, None);
    store.shutdown().await?;
    Ok(())
}
