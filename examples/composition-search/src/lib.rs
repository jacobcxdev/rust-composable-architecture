//! Builds a representative application from RCA's public composition APIs.
//!
//! [`handwritten_reducer`] and [`generated_reducer`] compose the same child
//! reducers. The integration tests compare their ordered actions and lifetimes.
//! Search results are fixed demonstration data, not network responses. Each
//! result set assigns row IDs by position and replaces the previous collection.
//!
//! Detail composition uses presence-only [`if_some`]: replacing a present detail
//! preserves its lifetime. Opening an existing row restarts the local timer.
//! Applications that need replacement to end child authority can use
//! [`rca::if_some_identified`].
//!
//! The runnable `named_operations` example adds a two-source typed-task search.
//! Its repository guide, `examples/composition-search/docs/named-operations.md`,
//! covers captured input, live reads, replacement and cancel-and-await semantics.
//! The `authorisation` example and `docs/authorisation.md` add bounded typed
//! request/reply with owned responses and post-await scope validation.
//! The `uploader` example and `docs/uploader.md` show linear asynchronous work,
//! live policy reads, scoped progress and exhaustive owned completion.
//! `docs/session-refresh.md` links the executable provider-handover and optimistic
//! reply traces, distinguishing captured clients from ordered current-service reads.

use std::time::Duration;

use rca::{
    ActionPath, ActionPaths, DependencyKey, Effect, KeyedAction, KeyedActionPath, KeyedState,
    KeyedStatePath, OptionalStatePath, ReduceContext, Reducer, StatePath, StatePaths, combine,
    for_each, if_some, reducers, scope, with_dependency,
};

/// Delays a search reply until the next illustrative asynchronous step.
///
/// The integration tests advance virtual time. This is not a service deadline.
const SEARCH_DELAY: Duration = Duration::from_millis(5);
/// Keeps detail work pending long enough for the virtual-time removal trace.
const DETAIL_DELAY: Duration = Duration::from_millis(20);
/// Separates row work from immediate actions in the virtual-time trace.
const ROW_WORK_DELAY: Duration = Duration::from_millis(15);

/// Owns independently scoped children and their ordered action trace.
#[derive(Debug, StatePaths)]
pub struct AppState {
    /// Retains the root query independently of result-row lifetimes.
    pub search: SearchState,
    /// Tracks one presence-only child whose removal retires its timer.
    #[rca(optional)]
    pub detail: Option<DetailState>,
    /// Holds result rows in their visible order.
    #[rca(keyed)]
    pub rows: KeyedState<u64, RowState>,
    /// Records consumer-visible actions for the conformance example.
    pub events: Vec<TraceEvent>,
}

/// Retains the query independently of cancellable delayed responses.
#[derive(Debug)]
pub struct SearchState {
    /// Stores the most recent query.
    pub query: String,
}

/// Records timer deliveries under one presence-only child lifetime.
#[derive(Debug)]
pub struct DetailState {
    /// Identifies the row presented by this detail feature.
    pub row_id: u64,
    /// Counts delivered timer events.
    pub timer_ticks: usize,
}

/// Owns a row's nested counter and scoped work progress.
#[derive(Debug, StatePaths)]
pub struct RowState {
    /// Owns one fixed response title for the current result set.
    pub title: String,
    /// Holds nested favorite state.
    pub favorite: FavoriteState,
    /// Counts completed row work.
    pub completed_work: usize,
}

/// Counts delivered nested actions independently for each row.
#[derive(Debug)]
pub struct FavoriteState {
    /// Counts favorite actions.
    pub count: usize,
}

/// Routes all root actions without a `Clone` bound.
#[derive(Debug, ActionPaths)]
pub enum AppAction {
    /// Routes a search action.
    Search(
        /// The action borrowed by the root search reducer.
        SearchAction,
    ),
    /// Routes an optional detail action.
    Detail(
        /// The action routed only while a detail is present.
        DetailAction,
    ),
    /// Routes an action to one keyed row.
    #[rca(keyed)]
    Row(
        /// The row identifier paired with its nested action.
        KeyedAction<u64, RowAction>,
    ),
    /// Presents detail for one row.
    OpenDetail(
        /// The existing result row to present.
        u64,
    ),
    /// Dismisses current detail.
    CloseDetail,
    /// Removes one keyed result row.
    RemoveRow(
        /// The row whose removal also retires scoped work and observation.
        u64,
    ),
}

/// Routes search feature actions.
#[derive(Debug)]
pub enum SearchAction {
    /// Replaces the query and starts a delayed request.
    Query(
        /// The owned input copied into query state.
        String,
    ),
    /// Delivers deterministic result titles.
    Results(
        /// The titles used to replace rows in response order.
        Vec<String>,
    ),
}

/// Routes optional detail actions.
#[derive(Debug)]
pub enum DetailAction {
    /// Starts the scoped detail timer after presentation commits.
    StartTimer,
    /// Counts one detail timer delivery.
    TimerFired,
}

/// Routes one keyed row action.
#[derive(Debug, ActionPaths)]
pub enum RowAction {
    /// Starts work under a row-local cancellation identity.
    StartWork,
    /// Completes one row-local work item.
    WorkFinished,
    /// Routes nested favorite state.
    Favorite(
        /// The action borrowed by this row's nested counter.
        FavoriteAction,
    ),
}

/// Routes one nested favorite action.
#[derive(Debug)]
pub enum FavoriteAction {
    /// Records one favorite action.
    Toggle,
}

/// Records the ordered conformance trace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TraceEvent {
    /// Records a typed query.
    SearchTyped,
    /// Records delivered results.
    SearchResults,
    /// Records detail presentation.
    DetailOpened,
    /// Records row work start.
    RowWorkStarted(
        /// The row that accepted the start action.
        u64,
    ),
    /// Records a nested favorite action.
    RowFavorited(
        /// The row whose nested counter changed.
        u64,
    ),
    /// Records keyed row departure.
    RowRemoved(
        /// The row removed from the keyed collection.
        u64,
    ),
    /// Records detail departure.
    DetailClosed,
    /// Records surviving row work completion.
    RowWorkFinished(
        /// The surviving row that accepted completion.
        u64,
    ),
}

/// Supplies deterministic search result titles.
#[derive(Clone, Debug)]
pub struct SearchApi {
    /// Retains the titles copied into each independent response.
    results: Vec<String>,
}

impl SearchApi {
    /// Creates a client returning the supplied titles for every query.
    #[must_use]
    pub fn fixed(results: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            results: results
                .into_iter()
                .map(|result| result.as_ref().to_owned())
                .collect(),
        }
    }

    /// Supplies the executable's fixed data without external I/O.
    fn live() -> Self {
        Self::fixed(["rust", "composition"])
    }

    /// Owns a response without consuming the reusable dependency's fixed titles.
    fn results(&self, _query: &str) -> Vec<String> {
        self.results.clone()
    }
}

/// Resolves the search API through Store-owned dependency overlays.
#[derive(Debug, DependencyKey)]
#[dependency(value = SearchApi, live = SearchApi::live())]
pub struct SearchApiKey;

/// Creates the initial state for both reducer variants.
#[must_use]
pub fn initial_state() -> AppState {
    AppState {
        search: SearchState {
            query: String::new(),
        },
        detail: None,
        rows: KeyedState::new(),
        events: Vec::new(),
    }
}

/// Builds the handwritten public-combinator control reducer.
///
/// The paths are explicit so the integration tests can compare their behaviour
/// with the derived paths in [`generated_reducer`].
#[must_use]
pub fn handwritten_reducer() -> impl Reducer<State = AppState, Action = AppAction> {
    let row = scope(
        StatePath::new(
            |state: &RowState| &state.favorite,
            |state: &mut RowState| &mut state.favorite,
        ),
        ActionPath::new(
            |action: &RowAction| match action {
                RowAction::Favorite(action) => Some(action),
                RowAction::StartWork | RowAction::WorkFinished => None,
            },
            RowAction::Favorite,
        ),
        FavoriteReducer,
    );
    let row = combine((row, RowReducer));
    let rows = for_each(
        AppReducer,
        KeyedStatePath::new(
            |state: &AppState| &state.rows,
            |state: &mut AppState| &mut state.rows,
        ),
        KeyedActionPath::new(
            |action: &AppAction| match action {
                AppAction::Row(action) => Some(action),
                _ => None,
            },
            AppAction::Row,
        ),
        row,
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
                _ => None,
            },
            AppAction::Detail,
        ),
        DetailReducer,
    );
    combine((
        scope(
            StatePath::new(
                |state: &AppState| &state.search,
                |state: &mut AppState| &mut state.search,
            ),
            ActionPath::new(
                |action: &AppAction| match action {
                    AppAction::Search(action) => Some(action),
                    _ => None,
                },
                AppAction::Search,
            ),
            with_dependency::<SearchApiKey, _>(SearchApi::live(), SearchReducer),
        ),
        detail,
    ))
}

/// Builds the derived-path and generated-tuple equivalent reducer.
///
/// It keeps the same reducer order and dependency override as
/// [`handwritten_reducer`], without repeating the borrowing paths.
#[must_use]
pub fn generated_reducer() -> impl Reducer<State = AppState, Action = AppAction> {
    let row = combine((
        scope(
            RowStatePaths::favorite(),
            RowActionPaths::favorite(),
            FavoriteReducer,
        ),
        RowReducer,
    ));
    let rows = for_each(
        AppReducer,
        AppStatePaths::rows(),
        AppActionPaths::row(),
        row,
    );
    let detail = if_some(
        rows,
        AppStatePaths::detail(),
        AppActionPaths::detail(),
        DetailReducer,
    );
    reducers![
        scope(
            AppStatePaths::search(),
            AppActionPaths::search(),
            with_dependency::<SearchApiKey, _>(SearchApi::live(), SearchReducer),
        ),
        detail,
    ]
}

/// Replaces delayed search work while retaining the latest query.
struct SearchReducer;

impl Reducer for SearchReducer {
    type State = SearchState;
    type Action = SearchAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            SearchAction::Query(query) => {
                state.query.clone_from(query);
                let results = context.dependency::<SearchApiKey>().results(query);
                Effect::send_after(SEARCH_DELAY, SearchAction::Results(results))
                    .cancellable_in_flight(SearchCancellation)
            }
            SearchAction::Results(_) => Effect::none(),
        }
    }
}

/// Owns child membership and records actions after child reduction.
struct AppReducer;

impl Reducer for AppReducer {
    type State = AppState;
    type Action = AppAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            AppAction::Search(SearchAction::Query(_)) => state.events.push(TraceEvent::SearchTyped),
            AppAction::Search(SearchAction::Results(results)) => {
                state.events.push(TraceEvent::SearchResults);
                state.rows = results
                    .iter()
                    .enumerate()
                    .map(|(index, title)| {
                        (
                            u64::try_from(index + 1).expect("result index fits u64"),
                            RowState {
                                title: title.clone(),
                                favorite: FavoriteState { count: 0 },
                                completed_work: 0,
                            },
                        )
                    })
                    .collect();
            }
            AppAction::OpenDetail(id) => {
                state.detail = state.rows.get(id).map(|_| DetailState {
                    row_id: *id,
                    timer_ticks: 0,
                });
                if state.detail.is_some() {
                    state.events.push(TraceEvent::DetailOpened);
                    return Effect::send(AppAction::Detail(DetailAction::StartTimer));
                }
            }
            AppAction::CloseDetail => {
                state.detail = None;
                state.events.push(TraceEvent::DetailClosed);
            }
            AppAction::RemoveRow(id) => {
                if state.rows.remove(id).is_some() {
                    state.events.push(TraceEvent::RowRemoved(*id));
                }
            }
            AppAction::Row(action) => match action.action() {
                RowAction::StartWork => state.events.push(TraceEvent::RowWorkStarted(*action.id())),
                RowAction::WorkFinished => {
                    state.events.push(TraceEvent::RowWorkFinished(*action.id()));
                }
                RowAction::Favorite(FavoriteAction::Toggle) => {
                    state.events.push(TraceEvent::RowFavorited(*action.id()));
                }
            },
            AppAction::Detail(DetailAction::StartTimer | DetailAction::TimerFired) => {}
        }
        Effect::none()
    }
}

/// Starts one timer under the currently present detail's lifetime.
struct DetailReducer;

impl Reducer for DetailReducer {
    type State = DetailState;
    type Action = DetailAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            DetailAction::StartTimer => {
                return Effect::send_after(DETAIL_DELAY, DetailAction::TimerFired)
                    .cancellable_in_flight(DetailTimerCancellation);
            }
            DetailAction::TimerFired => state.timer_ticks += 1,
        }
        Effect::none()
    }
}

/// Counts nested actions without scheduling work.
struct FavoriteReducer;

impl Reducer for FavoriteReducer {
    type State = FavoriteState;
    type Action = FavoriteAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            FavoriteAction::Toggle => state.count += 1,
        }
        Effect::none()
    }
}

/// Keeps equal work keys independent through each row's scope.
struct RowReducer;

impl Reducer for RowReducer {
    type State = RowState;
    type Action = RowAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            RowAction::StartWork => Effect::send_after(ROW_WORK_DELAY, RowAction::WorkFinished)
                .cancellable_in_flight(RowWorkCancellation),
            RowAction::WorkFinished => {
                state.completed_work += 1;
                Effect::none()
            }
            RowAction::Favorite(FavoriteAction::Toggle) => Effect::none(),
        }
    }
}

/// Replaces the previous delayed search response.
#[derive(Eq, Hash, PartialEq)]
struct SearchCancellation;

/// Replaces work only within the selected row's lifetime.
#[derive(Eq, Hash, PartialEq)]
struct RowWorkCancellation;

/// Restarts the timer within the currently present detail.
#[derive(Eq, Hash, PartialEq)]
struct DetailTimerCancellation;
