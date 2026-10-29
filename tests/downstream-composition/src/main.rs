//! Checks optional and keyed composition through renamed, packaged RCA dependencies.
//!
//! The executable uses paused time. Its delay illustrates cancellation, not elapsed latency.
//! Inline tests check handwritten and generated paths through complete state projections.

use std::time::Duration;

#[cfg(test)]
use architecture::{ActionPath, KeyedActionPath, KeyedStatePath, OptionalStatePath, combine};
use architecture::{
    ActionPaths, Effect, KeyedAction, KeyedState, ReduceContext, Reducer, StatePaths, Store,
    TokioRuntime, for_each, if_some, reducers,
};

/// Separates pending child work from completion under controlled time.
const CHILD_DELAY: Duration = Duration::from_millis(10);

/// Owns independent child state and counts every root action.
#[derive(Debug, StatePaths)]
struct AppState {
    /// Holds a presence-scoped child whose work ends when this value becomes None.
    #[rca(optional)]
    detail: Option<ChildState>,
    /// Owns rows whose insertion identities define separate child lifetimes.
    #[rca(keyed)]
    rows: KeyedState<u64, ChildState>,
    /// Counts admitted actions, including outputs from surviving children.
    audit_count: usize,
}

/// Records direct updates separately from delayed completions without requiring Clone.
#[derive(Debug, Default)]
struct ChildState {
    /// Counts immediate Increment actions.
    count: usize,
    /// Counts WorkFinished actions that reached this child lifetime.
    finished_work: usize,
}

/// Routes child actions and removes optional or keyed membership.
#[derive(Debug, ActionPaths)]
enum AppAction {
    /// Routes an action to the current optional child.
    Detail(#[doc = "The owned child action."] ChildAction),
    /// Routes an action to the row selected by its key.
    #[rca(keyed)]
    Row(#[doc = "The row key and owned child action."] KeyedAction<u64, ChildAction>),
    /// Removes the optional child and its managed work.
    CloseDetail,
    /// Removes one keyed child and its managed work.
    RemoveRow(#[doc = "The row key to remove."] u64),
}

/// Separates immediate mutation, delayed work admission and delayed completion.
#[derive(Debug)]
enum ChildAction {
    /// Increases the immediate counter.
    Increment,
    /// Schedules one completion after the controlled delay.
    StartWork,
    /// Records a completion in the still-current child.
    WorkFinished,
}

/// Mutates one child and returns delayed work to its composition lifetime.
struct ChildReducer;

impl Reducer for ChildReducer {
    type State = ChildState;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            ChildAction::Increment => state.count += 1,
            ChildAction::StartWork => {
                return Effect::send_after(CHILD_DELAY, ChildAction::WorkFinished);
            }
            ChildAction::WorkFinished => state.finished_work += 1,
        }
        Effect::none()
    }
}

/// Removes membership after child routing without managing cancellation itself.
struct ParentReducer;

impl Reducer for ParentReducer {
    type State = AppState;
    type Action = AppAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            AppAction::CloseDetail => state.detail = None,
            AppAction::RemoveRow(id) => {
                state.rows.remove(id);
            }
            AppAction::Detail(_) | AppAction::Row(_) => {}
        }
        Effect::none()
    }
}

/// Counts root actions after the child and parent reducers run.
struct AuditReducer;

impl Reducer for AuditReducer {
    type State = AppState;
    type Action = AppAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.audit_count += 1;
        Effect::none()
    }
}

/// Creates one optional child and two rows with distinct fixed fixture keys.
fn initial_state() -> AppState {
    AppState {
        detail: Some(ChildState::default()),
        rows: [(1, ChildState::default()), (2, ChildState::default())]
            .into_iter()
            .collect(),
        audit_count: 0,
    }
}

#[cfg(test)]
/// Builds the explicit-path control without depending on generated path helpers.
fn handwritten_reducer() -> impl Reducer<State = AppState, Action = AppAction> {
    let rows = for_each(
        ParentReducer,
        KeyedStatePath::new(
            |state: &AppState| &state.rows,
            |state: &mut AppState| &mut state.rows,
        ),
        KeyedActionPath::new(
            |action: &AppAction| match action {
                AppAction::Row(action) => Some(action),
                AppAction::Detail(_) | AppAction::CloseDetail | AppAction::RemoveRow(_) => None,
            },
            AppAction::Row,
        ),
        ChildReducer,
    );
    let detail = if_some(
        rows,
        OptionalStatePath::new(
            |state: &AppState| state.detail.as_ref(),
            |state: &mut AppState| state.detail.as_mut(),
        ),
        ActionPath::new(
            |action: &AppAction| match action {
                AppAction::Detail(action) => Some(action),
                AppAction::Row(_) | AppAction::CloseDetail | AppAction::RemoveRow(_) => None,
            },
            AppAction::Detail,
        ),
        ChildReducer,
    );
    combine((detail, AuditReducer))
}

/// Builds the same reducer order with generated paths and a reducer tuple.
fn generated_reducer() -> impl Reducer<State = AppState, Action = AppAction> {
    let rows = for_each(
        ParentReducer,
        AppStatePaths::rows(),
        AppActionPaths::row(),
        ChildReducer,
    );
    let detail = if_some(
        rows,
        AppStatePaths::detail(),
        AppActionPaths::detail(),
        ChildReducer,
    );
    reducers![detail, AuditReducer]
}

#[tokio::main(flavor = "current_thread", start_paused = true)]
/// Checks scoped reads, observation and cancellation through the packaged runtime adapter.
///
/// # Errors
/// Returns runtime, admission or state-access failures.
///
/// # Panics
/// Panics when the packaged APIs violate an asserted fixture contract.
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = Store::new(
        initial_state(),
        generated_reducer(),
        TokioRuntime::current()?,
    );
    let detail = store
        .scope_some(AppStatePaths::detail(), AppActionPaths::detail())
        .await?;
    let row = store
        .scope_keyed(AppStatePaths::rows(), AppActionPaths::row(), 1)
        .await?;
    let mut row_counts = row.observe_distinct(|state| state.count).await?;
    assert_eq!(row_counts.next().await, Some(0));

    detail.send(ChildAction::Increment).await?.finish().await;
    row.send(ChildAction::Increment).await?.finish().await;
    assert_eq!(detail.read(|state| state.count).await?, 1);
    assert_eq!(row_counts.next().await, Some(1));
    store.send(AppAction::CloseDetail).await?.finish().await;
    assert!(
        detail
            .send(ChildAction::Increment)
            .await
            .expect_err("the removed detail rejects new actions")
            .is_scope_ended()
    );

    let work = row.send(ChildAction::StartWork).await?;
    let active_timers = store.runtime_snapshot().await?.timer_deadlines;
    assert_eq!(active_timers.len(), 1);
    store.send(AppAction::RemoveRow(1)).await?.finish().await;
    let cancelled_timers = store.runtime_snapshot().await?.timer_deadlines;
    assert!(cancelled_timers.is_empty());
    work.finish().await;
    assert!(work.is_finished());
    assert!(
        row.send(ChildAction::Increment)
            .await
            .expect_err("the removed row rejects new actions")
            .is_scope_ended()
    );
    assert_eq!(row_counts.next().await, None);

    drop(row_counts);
    store.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Checks complete state history without adding Clone to application state or actions.

    use super::*;
    use architecture::TestStore;

    /// Owns every state field for exhaustive TestStore comparisons.
    #[derive(Clone, Debug, PartialEq)]
    struct Snapshot {
        /// Captures the optional child's immediate and completed-work counts.
        detail: Option<(usize, usize)>,
        /// Captures each row key and both counters in insertion order.
        rows: Vec<(u64, usize, usize)>,
        /// Captures the root's complete action count.
        audit_count: usize,
    }

    /// Projects all state without sharing mutable storage with the tested reducer.
    fn snapshot(state: &AppState) -> Snapshot {
        Snapshot {
            detail: state
                .detail
                .as_ref()
                .map(|child| (child.count, child.finished_work)),
            rows: state
                .rows
                .iter()
                .map(|(id, child)| (*id, child.count, child.finished_work))
                .collect(),
            audit_count: state.audit_count,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handwritten_paths_route_optional_and_keyed_children() {
        let mut store = TestStore::projected(initial_state(), handwritten_reducer(), snapshot);
        store
            .send_with(AppAction::Detail(ChildAction::Increment), |state| {
                state.detail = Some((1, 0));
                state.audit_count += 1;
            })
            .await
            .expect("detail action commits")
            .finish()
            .await
            .expect("detail action finishes");
        store
            .send_with(
                AppAction::Row(KeyedAction::new(1, ChildAction::Increment)),
                |state| {
                    state.rows[0].1 = 1;
                    state.audit_count += 1;
                },
            )
            .await
            .expect("row action commits")
            .finish()
            .await
            .expect("row action finishes");
        store
            .assert_state(|state| {
                assert_eq!(state.detail.as_ref().map(|child| child.count), Some(1));
                assert_eq!(state.rows.get(&1).map(|child| child.count), Some(1));
                assert_eq!(state.audit_count, 2);
            })
            .await
            .expect("handwritten composition state is observable");
        store.finish().await.expect("handwritten store finishes");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn derived_composition_cancels_only_departed_lifetimes() {
        let mut store = TestStore::projected(initial_state(), generated_reducer(), snapshot);
        let detail_work = store
            .send_with(AppAction::Detail(ChildAction::StartWork), |state| {
                state.audit_count += 1
            })
            .await
            .expect("detail work starts");
        let first_row_work = store
            .send_with(
                AppAction::Row(KeyedAction::new(1, ChildAction::StartWork)),
                |state| state.audit_count += 1,
            )
            .await
            .expect("first row work starts");
        let second_row_work = store
            .send_with(
                AppAction::Row(KeyedAction::new(2, ChildAction::StartWork)),
                |state| state.audit_count += 1,
            )
            .await
            .expect("second row work starts");

        store
            .send_with(AppAction::CloseDetail, |state| {
                state.detail = None;
                state.audit_count += 1;
            })
            .await
            .expect("detail removal commits")
            .finish()
            .await
            .expect("detail removal finishes");
        detail_work
            .finish()
            .await
            .expect("departed detail work is cancelled");
        store
            .send_with(AppAction::RemoveRow(2), |state| {
                state.rows.remove(1);
                state.audit_count += 1;
            })
            .await
            .expect("second row removal commits")
            .finish()
            .await
            .expect("second row removal finishes");
        second_row_work
            .finish()
            .await
            .expect("departed row work is cancelled");

        store
            .advance(CHILD_DELAY)
            .await
            .expect("surviving row delay advances");
        store
            .receive_with(
                |action| {
                    matches!(
                        action,
                        AppAction::Row(keyed)
                            if *keyed.id() == 1
                                && matches!(keyed.action(), ChildAction::WorkFinished)
                    )
                },
                |state| {
                    state.rows[0].2 = 1;
                    state.audit_count += 1;
                },
            )
            .await
            .expect("surviving row output is delivered")
            .finish()
            .await
            .expect("surviving row output finishes");
        first_row_work
            .finish()
            .await
            .expect("surviving row work finishes");
        store
            .assert_state(|state| {
                assert!(state.detail.is_none());
                assert!(!state.rows.contains_id(&2));
                assert_eq!(state.rows.get(&1).map(|child| child.finished_work), Some(1));
            })
            .await
            .expect("lifecycle state is observable");
        store.finish().await.expect("derived store finishes");
    }
}
