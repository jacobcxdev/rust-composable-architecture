//! Checks keyed collection ownership, routing, and shared child-reducer behaviour.
//!
//! Literal orders and independent counters check collection and mapping costs.
//! Runtime scenarios stop their Stores before the manual host joins its threads.

use std::backtrace::BacktraceStatus;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rca_core::{
    DuplicateId, Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
    MissingChildPolicy, ReduceContext, Reducer, Store, for_each,
};

mod support;

#[cfg(not(miri))]
#[expect(
    dead_code,
    reason = "this host uses only the child launcher with explicit backtrace settings"
)]
#[path = "../src/allocation_test/child.rs"]
mod subprocess;

use support::ManualRuntime;

/// Holds a move-only value so collection operations cannot rely on cloning rows.
#[derive(Debug, Eq, PartialEq)]
struct Row {
    value: String,
}

/// Copies the visible order for comparison with a literal expected sequence.
fn ids(rows: &KeyedState<u64, Row>) -> Vec<u64> {
    rows.ids().copied().collect()
}

#[test]
fn keyed_state_preserves_unique_ordered_rows() {
    let mut rows = KeyedState::new();
    rows.insert(
        2_u64,
        Row {
            value: String::from("two"),
        },
    );
    rows.insert(
        1_u64,
        Row {
            value: String::from("one"),
        },
    );

    assert_eq!(ids(&rows), [2, 1]);
    assert!(rows.contains_id(&2));
    assert!(!rows.contains_id(&3));

    rows.move_index(1, 0);
    assert_eq!(ids(&rows), [1, 2]);
    rows.get_mut(&2).expect("row 2 exists").value.push('!');
    assert_eq!(rows.get(&2).expect("row 2 exists").value, "two!");
    assert_eq!(rows[&1].value, "one");
}

#[test]
#[should_panic(expected = "duplicate KeyedState ID")]
fn insert_panics_for_duplicate_id() {
    let mut rows = KeyedState::new();
    rows.insert(1, ());
    rows.insert(1, ());
}

#[test]
fn try_insert_recovers_duplicate_ownership_without_debug_bounds() {
    /// Omits `Debug` and `Clone` to keep duplicate recovery bounds observable.
    struct OpaqueId(u8);
    /// Holds the rejected allocation without adding display or clone support.
    struct OpaqueState(String);

    impl PartialEq for OpaqueId {
        fn eq(&self, other: &Self) -> bool {
            self.0 == other.0
        }
    }

    impl Eq for OpaqueId {}

    impl std::hash::Hash for OpaqueId {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            std::hash::Hash::hash(&self.0, state);
        }
    }

    let mut rows = KeyedState::new();
    rows.insert(OpaqueId(7), OpaqueState(String::from("original")));
    let duplicate = rows
        .try_insert(OpaqueId(7), OpaqueState(String::from("duplicate")))
        .expect_err("duplicate ID is rejected");

    assert_eq!(
        duplicate.to_string(),
        format!("duplicate KeyedState ID\n{}", duplicate.backtrace())
    );
    let (id, state) = duplicate.into_parts();
    assert_eq!(id.0, 7);
    assert_eq!(state.0, "duplicate");
    assert_eq!(
        rows.get(&OpaqueId(7)).expect("original remains").0,
        "original"
    );
}

#[test]
fn keyed_insertion_hashes_each_input_once_and_recovers_exact_duplicate() {
    /// Compares only the value while labelling and counting each supplied key.
    #[derive(Debug)]
    struct MeasuredId {
        value: u8,
        label: &'static str,
        hashes: Arc<AtomicUsize>,
    }

    impl PartialEq for MeasuredId {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    impl Eq for MeasuredId {}

    impl Hash for MeasuredId {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.hashes.fetch_add(1, Ordering::Relaxed);
            self.value.hash(state);
        }
    }

    let hashes = Arc::new(AtomicUsize::new(0));
    let id = |value, label| MeasuredId {
        value,
        label,
        hashes: Arc::clone(&hashes),
    };
    let mut rows = KeyedState::with_capacity(3);
    rows.insert(id(1, "first"), String::from("one"));
    rows.insert(id(2, "second"), String::from("two"));
    hashes.store(0, Ordering::Relaxed);
    rows.try_insert(id(3, "third"), String::from("three"))
        .expect("new ID is inserted");
    assert_eq!(hashes.load(Ordering::Relaxed), 1);

    hashes.store(0, Ordering::Relaxed);
    let (duplicate, value) = rows
        .try_insert(id(1, "rejected"), String::from("replacement"))
        .expect_err("equal ID is rejected")
        .into_parts();
    assert_eq!(hashes.load(Ordering::Relaxed), 1);
    assert_eq!(duplicate.label, "rejected");
    assert_eq!(value, "replacement");
    assert_eq!(
        rows.iter()
            .map(|(id, value)| (id.label, value.as_str()))
            .collect::<Vec<_>>(),
        [("first", "one"), ("second", "two"), ("third", "three")]
    );
}

#[test]
fn keyed_insertion_preserves_colliding_ids_across_growth() {
    /// Gives distinct keys one hash so equality must still distinguish them.
    #[derive(Debug, Eq, PartialEq)]
    struct CollidingId(u8);

    impl Hash for CollidingId {
        fn hash<H: Hasher>(&self, state: &mut H) {
            // Distinct IDs deliberately share one hash to exercise equality lookup.
            0_u8.hash(state);
        }
    }

    let mut rows = KeyedState::new();
    for value in 0..32 {
        rows.insert(CollidingId(value), value);
    }
    for value in 0..32 {
        assert_eq!(rows.get(&CollidingId(value)), Some(&value));
        let (_, rejected) = rows
            .try_insert(CollidingId(value), 255)
            .expect_err("existing colliding ID remains unique")
            .into_parts();
        assert_eq!(rejected, 255);
    }
    assert_eq!(rows.remove(&CollidingId(7)), Some(7));
    rows.insert(CollidingId(7), 70);
    assert_eq!(rows.get(&CollidingId(7)), Some(&70));
    assert_eq!(rows.len(), 32);
    assert_eq!(
        rows.ids().map(|id| id.0).collect::<Vec<_>>(),
        (0..7).chain(8..32).chain([7]).collect::<Vec<_>>()
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "test launches a subprocess, which Miri isolation forbids"
)]
fn duplicate_construction_captures_an_enabled_backtrace() {
    // Configure capture before its process-wide setting can be cached.
    // The shared launcher checks exact execution and owns the child watchdog.
    #[cfg(not(miri))]
    if subprocess::run_in_child_with_env(
        "duplicate_construction_captures_an_enabled_backtrace",
        &[("RUST_LIB_BACKTRACE", "1"), ("RUST_BACKTRACE", "1")],
    ) {
        return;
    }

    let mut rows = KeyedState::new();
    rows.insert(1_u8, String::from("original"));
    let duplicate = rows
        .try_insert(1, String::from("duplicate"))
        .expect_err("duplicate ID is rejected");
    assert_eq!(duplicate.backtrace().status(), BacktraceStatus::Captured);
}

#[test]
fn duplicate_id_implements_error_for_debug_payloads() {
    /// Makes the standard error bound a compile-time consumer check.
    fn assert_error<T: std::error::Error>() {}

    /// Owns a distinct allocation whose address can be checked after rejection.
    #[derive(Debug, Eq, Hash, PartialEq)]
    struct Id(Box<str>);
    /// Distinguishes the rejected allocation from the stored value.
    #[derive(Debug)]
    struct Payload(Box<str>);

    assert_error::<DuplicateId<u8, String>>();

    let mut rows = KeyedState::new();
    rows.insert(Id("route".into()), Payload("original".into()));
    let id = Id("route".into());
    let payload = Payload("rejected".into());
    let id_allocation = id.0.as_ptr();
    let payload_allocation = payload.0.as_ptr();
    let duplicate = rows.try_insert(id, payload).unwrap_err();
    let error: &dyn std::error::Error = &duplicate;
    let rendered = format!("{error:?}");
    assert!(rendered.contains("DuplicateId"));
    assert!(rendered.contains("route"));
    assert!(rendered.contains("rejected"));
    assert!(!rendered.contains("original"));
    assert!(error.source().is_none());

    let (id, payload) = duplicate.into_parts();
    assert_eq!(id.0.as_ptr(), id_allocation);
    assert_eq!(payload.0.as_ptr(), payload_allocation);
    assert_eq!(payload.0.as_ref(), "rejected");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows.get(&id).unwrap().0.as_ref(), "original");
}

#[test]
fn ordered_remove_and_swap_remove_have_distinct_order_contracts() {
    let make_rows = || {
        [(1_u64, "one"), (2, "two"), (3, "three")]
            .into_iter()
            .map(|(id, value)| {
                (
                    id,
                    Row {
                        value: String::from(value),
                    },
                )
            })
            .collect::<KeyedState<_, _>>()
    };

    let mut ordered = make_rows();
    assert_eq!(ordered.remove(&2).expect("row 2 exists").value, "two");
    assert_eq!(ids(&ordered), [1, 3]);

    let mut swapped = make_rows();
    assert_eq!(swapped.swap_remove(&1).expect("row 1 exists").value, "one");
    assert_eq!(ids(&swapped), [3, 2]);
    assert!(swapped.remove(&9).is_none());
    assert!(swapped.swap_remove(&9).is_none());
}

#[test]
#[should_panic(expected = "KeyedState source index out of bounds")]
fn move_index_panics_for_invalid_source() {
    let mut rows = [(1_u8, ())].into_iter().collect::<KeyedState<_, _>>();
    rows.move_index(1, 0);
}

#[test]
#[should_panic(expected = "KeyedState destination index out of bounds")]
fn move_index_panics_for_invalid_destination() {
    let mut rows = [(1_u8, ())].into_iter().collect::<KeyedState<_, _>>();
    rows.move_index(0, 1);
}

#[test]
fn retain_can_mutate_values_and_preserves_survivor_order() {
    let mut rows = [(3_u8, 30_u8), (1, 10), (2, 20)]
        .into_iter()
        .collect::<KeyedState<_, _>>();

    rows.retain(|id, value| {
        *value += 1;
        *id != 1
    });

    assert_eq!(
        rows.iter()
            .map(|(id, value)| (*id, *value))
            .collect::<Vec<_>>(),
        [(3, 31), (2, 21)]
    );
}

#[test]
fn clear_removes_every_entry_and_allows_reuse() {
    let mut rows = [(1_u8, 10_u8), (2, 20)]
        .into_iter()
        .collect::<KeyedState<_, _>>();

    rows.clear();
    assert!(rows.is_empty());
    assert_eq!(rows.len(), 0);
    rows.insert(3, 30);
    assert_eq!(rows.iter().collect::<Vec<_>>(), [(&3, &30)]);
}

#[test]
fn collection_iterators_preserve_order_and_exact_sizes() {
    let mut rows = [(4_u8, 40_u8), (2, 20), (7, 70)]
        .into_iter()
        .collect::<KeyedState<_, _>>();

    assert_eq!(rows.ids().len(), 3);
    assert_eq!(rows.values().len(), 3);
    assert_eq!(rows.iter().len(), 3);
    assert_eq!((&rows).into_iter().len(), 3);
    assert_eq!(rows.values().copied().collect::<Vec<_>>(), [40, 20, 70]);
    assert_eq!(
        (&rows)
            .into_iter()
            .map(|(id, value)| (*id, *value))
            .collect::<Vec<_>>(),
        [(4, 40), (2, 20), (7, 70)]
    );

    assert_eq!(rows.values_mut().len(), 3);
    for value in rows.values_mut() {
        *value += 1;
    }
    assert_eq!(rows.iter_mut().len(), 3);
    assert_eq!((&mut rows).into_iter().len(), 3);
    for (_, value) in &mut rows {
        *value += 1;
    }

    assert_eq!(
        rows.into_iter().collect::<Vec<_>>(),
        [(4, 42), (2, 22), (7, 72)]
    );
}

#[test]
#[should_panic(expected = "duplicate KeyedState ID")]
fn from_iterator_panics_for_duplicate_ids() {
    let _ = [(1_u8, 10_u8), (1, 20)]
        .into_iter()
        .collect::<KeyedState<_, _>>();
}

#[test]
#[should_panic(expected = "duplicate KeyedState ID")]
fn extend_panics_for_duplicate_ids() {
    let mut rows = [(1_u8, 10_u8)].into_iter().collect::<KeyedState<_, _>>();
    rows.extend([(2, 20), (1, 30)]);
}

#[test]
fn clone_preserves_public_values_and_can_mutate_independently() {
    let rows = [(1_u8, String::from("one"))]
        .into_iter()
        .collect::<KeyedState<_, _>>();
    let mut left = rows.clone();
    let mut right = rows.clone();

    left.insert(2, String::from("left"));
    right.insert(2, String::from("right"));

    assert_eq!(left.get(&2).map(String::as_str), Some("left"));
    assert_eq!(right.get(&2).map(String::as_str), Some("right"));
    assert_eq!(rows.len(), 1);
}

#[test]
fn equality_is_ordered_and_ignores_hidden_identity() {
    let first = [(1_u8, 10_u8), (2, 20)]
        .into_iter()
        .collect::<KeyedState<_, _>>();
    let independent = [(1_u8, 10_u8), (2, 20)]
        .into_iter()
        .collect::<KeyedState<_, _>>();
    let different_order = [(2_u8, 20_u8), (1, 10)]
        .into_iter()
        .collect::<KeyedState<_, _>>();
    let matching_prefix = [(1_u8, 10_u8)].into_iter().collect::<KeyedState<_, _>>();
    let mut reinserted = first.clone();
    let value = reinserted.remove(&1).expect("row 1 exists");
    reinserted.insert(1, value);
    reinserted.move_index(1, 0);

    assert_eq!(first, independent);
    assert_eq!(first, reinserted);
    assert_ne!(first, different_order);
    assert_ne!(first, matching_prefix);
    assert_ne!(matching_prefix, first);
}

#[test]
fn default_and_zero_capacity_are_empty() {
    let default = KeyedState::<u8, u8>::default();
    let reserved = KeyedState::<u8, u8>::with_capacity(0);

    assert!(rca_core::KeyedState::is_empty(&default));
    assert!(rca_core::KeyedState::is_empty(&reserved));
    assert_eq!(reserved.ids().len(), 0);
}

#[test]
fn debug_is_available_conditionally() {
    let rows = [(1_u8, 10_u8)].into_iter().collect::<KeyedState<_, _>>();
    let rendered = format!("{rows:?}");

    assert!(rendered.contains("KeyedState"));
    assert!(rendered.contains('1'));
    assert!(rendered.contains("10"));
}

/// Supplies a non-cloneable child action for borrowed path extraction.
#[derive(Debug, Eq, PartialEq)]
enum RowAction {
    Tap,
}

/// Separates matching child input from a non-matching parent action.
#[derive(Debug, Eq, PartialEq)]
enum AppAction {
    Row(KeyedAction<u64, RowAction>),
    Parent,
}

#[test]
fn keyed_action_owns_parts_and_maps_only_action() {
    let action = KeyedAction::new(String::from("row-7"), vec![1_u8, 2]);

    assert_eq!(action.id(), "row-7");
    assert_eq!(action.action(), &[1, 2]);

    let mapped = action.map(|bytes| bytes.len());
    assert_eq!(mapped.into_parts(), (String::from("row-7"), 2));
}

#[test]
fn keyed_action_implements_conditional_common_traits() {
    use std::collections::BTreeSet;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let first = KeyedAction::new(1_u8, 2_u8);
    let second = KeyedAction::new(1_u8, 3_u8);
    let copy = first;

    assert_eq!(copy, first);
    assert!(first < second);
    assert!(format!("{first:?}").contains("KeyedAction"));

    let mut ordered = BTreeSet::new();
    assert!(ordered.insert(first));
    assert!(ordered.insert(second));

    let mut hasher = DefaultHasher::new();
    copy.hash(&mut hasher);
    let first_hash = hasher.finish();
    let mut hasher = DefaultHasher::new();
    first.hash(&mut hasher);
    assert_eq!(first_hash, hasher.finish());
}

#[test]
fn keyed_action_path_borrows_id_and_action() {
    let path = KeyedActionPath::new(
        |action: &AppAction| match action {
            AppAction::Row(action) => Some(action),
            AppAction::Parent => None,
        },
        AppAction::Row,
    );
    let input = AppAction::Row(KeyedAction::new(7, RowAction::Tap));

    let (id, action) = path.extract(&input).expect("row action matches");
    assert_eq!(*id, 7);
    assert!(matches!(action, RowAction::Tap));
    assert!(path.extract(&AppAction::Parent).is_none());
}

#[test]
fn keyed_action_path_embeds_owned_id_and_action_without_clone_bounds() {
    /// Requires the path to move the identifier into the parent action.
    struct MoveOnlyId(String);
    /// Requires the path to move the payload into the parent action.
    struct MoveOnlyAction(Vec<u8>);
    /// Holds both moved inputs behind the keyed action boundary.
    enum MoveOnlyParent {
        Child(KeyedAction<MoveOnlyId, MoveOnlyAction>),
    }

    let path = KeyedActionPath::new(
        |action: &MoveOnlyParent| match action {
            MoveOnlyParent::Child(action) => Some(action),
        },
        MoveOnlyParent::Child,
    );

    let embedded = path.embed(
        MoveOnlyId(String::from("child")),
        MoveOnlyAction(vec![4, 5]),
    );
    let MoveOnlyParent::Child(action) = embedded;
    let (id, action) = action.into_parts();
    assert_eq!(id.0, "child");
    assert_eq!(action.0, [4, 5]);
}

#[test]
fn keyed_state_path_borrows_collection_and_mutates_child_state() {
    /// Keeps the projected collection behind an ordinary parent field.
    struct AppState {
        rows: KeyedState<u64, Row>,
    }

    let path = KeyedStatePath::new(
        |state: &AppState| &state.rows,
        |state: &mut AppState| &mut state.rows,
    );
    let mut state = AppState {
        rows: [(
            7,
            Row {
                value: String::from("seven"),
            },
        )]
        .into_iter()
        .collect(),
    };

    path.get_mut(&mut state)
        .get_mut(&7)
        .expect("row 7 exists")
        .value
        .push('!');
    assert_eq!(
        path.get(&state).get(&7).expect("row 7 exists").value,
        "seven!"
    );
}

#[test]
fn keyed_paths_are_send_without_requiring_sync() {
    use std::cell::Cell;

    /// Checks the owned path boundary without asking it to support shared access.
    fn assert_send<T: Send>(_: T) {}

    /// Supplies the borrowed collection while each closure captures a `Cell`.
    struct State {
        rows: KeyedState<u8, u8>,
    }

    let state_marker = Cell::new(0_u8);
    let state_path = KeyedStatePath::new(
        move |state: &State| {
            state_marker.set(state_marker.get() + 1);
            &state.rows
        },
        |state: &mut State| &mut state.rows,
    );
    let action_marker = Cell::new(0_u8);
    let action_path = KeyedActionPath::new(
        move |action: &AppAction| {
            action_marker.set(action_marker.get() + 1);
            match action {
                AppAction::Row(action) => Some(action),
                AppAction::Parent => None,
            }
        },
        AppAction::Row,
    );

    assert_send(state_path);
    assert_send(action_path);
}

/// Records child work that the parent must observe before removing the row.
struct ForEachRow {
    delete_count: usize,
}

/// Retains the parent's observation after the selected child is gone.
struct ForEachState {
    rows: KeyedState<u64, ForEachRow>,
    parent_observed_delete_count: Option<usize>,
}

/// Requests work that must run before parent removal.
enum ForEachRowAction {
    Delete,
}

/// Routes the deletion request through a keyed child action.
enum ForEachAction {
    Row(KeyedAction<u64, ForEachRowAction>),
}

/// Increments the selected row before control returns to the parent.
struct ForEachChild;

impl Reducer for ForEachChild {
    type State = ForEachRow;
    type Action = ForEachRowAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            ForEachRowAction::Delete => state.delete_count += 1,
        }
        Effect::none()
    }
}

/// Reads the child's change, then removes that same row.
struct ForEachParent;

impl Reducer for ForEachParent {
    type State = ForEachState;
    type Action = ForEachAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let ForEachAction::Row(action) = action;
        state.parent_observed_delete_count =
            state.rows.get(action.id()).map(|row| row.delete_count);
        state.rows.remove(action.id());
        Effect::none()
    }
}

#[test]
fn for_each_runs_target_before_parent_removal() {
    let state_path = KeyedStatePath::new(
        |state: &ForEachState| &state.rows,
        |state: &mut ForEachState| &mut state.rows,
    );
    let action_path = KeyedActionPath::new(
        |action: &ForEachAction| match action {
            ForEachAction::Row(action) => Some(action),
        },
        ForEachAction::Row,
    );
    let reducer = for_each(ForEachParent, state_path, action_path, ForEachChild);
    let runtime = ManualRuntime::new();
    let store = Store::new(
        ForEachState {
            rows: [
                (1, ForEachRow { delete_count: 0 }),
                (2, ForEachRow { delete_count: 0 }),
            ]
            .into_iter()
            .collect(),
            parent_observed_delete_count: None,
        },
        reducer,
        runtime.runtime(),
    );
    let receipt = store
        .try_send(ForEachAction::Row(KeyedAction::new(
            2,
            ForEachRowAction::Delete,
        )))
        .expect("keyed action fits");
    runtime.start_all();

    futures_executor::block_on(async {
        receipt.await.expect("keyed action commits").finish().await;
        let result = store
            .read(|state| {
                (
                    state.rows.contains_id(&1),
                    state.rows.contains_id(&2),
                    state.parent_observed_delete_count,
                )
            })
            .await
            .expect("keyed state remains readable");

        assert_eq!(result, (true, false, Some(1)));
        store.shutdown().await.expect("keyed store shuts down");
    });
}

/// Records row-local calls alongside the shared reducer's global sequence.
struct SharedRow {
    local_calls: usize,
    global_sequences: Vec<usize>,
}

/// Holds two independently mutable rows for the shared-reducer check.
struct SharedState {
    rows: KeyedState<u64, SharedRow>,
}

/// Advances both the selected row and the reducer-wide counter.
enum SharedRowAction {
    Call,
}

/// Selects which row receives a call to the shared child reducer.
enum SharedAction {
    Row(KeyedAction<u64, SharedRowAction>),
}

/// Holds one counter shared by all entries, not one reducer per row.
struct SharedChild {
    calls: usize,
}

impl Reducer for SharedChild {
    type State = SharedRow;
    type Action = SharedRowAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            SharedRowAction::Call => {
                self.calls += 1;
                state.local_calls += 1;
                state.global_sequences.push(self.calls);
            }
        }
        Effect::none()
    }
}

/// Leaves all mutations to the shared child reducer.
struct SharedParent;

impl Reducer for SharedParent {
    type State = SharedState;
    type Action = SharedAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

#[test]
fn for_each_shares_one_sequential_child_reducer_across_entries() {
    let reducer = for_each(
        SharedParent,
        KeyedStatePath::new(
            |state: &SharedState| &state.rows,
            |state: &mut SharedState| &mut state.rows,
        ),
        KeyedActionPath::new(
            |action: &SharedAction| match action {
                SharedAction::Row(action) => Some(action),
            },
            SharedAction::Row,
        ),
        SharedChild { calls: 0 },
    );
    let runtime = ManualRuntime::new();
    let store = Store::new(
        SharedState {
            rows: [1_u64, 2]
                .into_iter()
                .map(|id| {
                    (
                        id,
                        SharedRow {
                            local_calls: 0,
                            global_sequences: Vec::new(),
                        },
                    )
                })
                .collect(),
        },
        reducer,
        runtime.runtime(),
    );
    let receipts = [1_u64, 2, 1].map(|id| {
        store
            .try_send(SharedAction::Row(KeyedAction::new(
                id,
                SharedRowAction::Call,
            )))
            .expect("shared child action fits")
    });
    runtime.start_all();

    futures_executor::block_on(async {
        for receipt in receipts {
            receipt
                .await
                .expect("shared child action commits")
                .finish()
                .await;
        }
        let trace = store
            .read(|state| {
                state
                    .rows
                    .iter()
                    .map(|(id, row)| (*id, row.local_calls, row.global_sequences.clone()))
                    .collect::<Vec<_>>()
            })
            .await
            .expect("shared child trace reads");

        assert_eq!(trace, [(1, 2, vec![1, 3]), (2, 1, vec![2])]);
        store
            .shutdown()
            .await
            .expect("shared child Store shuts down");
    });
}

/// Counts identifier clones without changing identifier equality or hashing.
struct CloneId {
    value: u64,
    clones: Arc<AtomicUsize>,
}

impl Clone for CloneId {
    fn clone(&self) -> Self {
        self.clones.fetch_add(1, Ordering::Relaxed);
        Self {
            value: self.value,
            clones: Arc::clone(&self.clones),
        }
    }
}

impl PartialEq for CloneId {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for CloneId {}

impl Hash for CloneId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

/// Counts the same emitted outputs at both parent and child levels.
struct MappingState {
    rows: KeyedState<CloneId, usize>,
    outputs: usize,
}

/// Starts two outputs or applies one of those outputs to the selected row.
enum MappingRowAction {
    Start,
    Output,
}

/// Separates initial mounting from the measured child-output mapping.
enum MappingAction {
    Row(KeyedAction<CloneId, MappingRowAction>),
    Warm,
}

/// Emits two independent outputs so each mapped identifier clone is counted.
struct MappingChild;

impl Reducer for MappingChild {
    type State = usize;
    type Action = MappingRowAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            MappingRowAction::Start => Effect::merge([
                Effect::send(MappingRowAction::Output),
                Effect::send(MappingRowAction::Output),
            ]),
            MappingRowAction::Output => {
                *state += 1;
                Effect::none()
            }
        }
    }
}

/// Counts mapped outputs independently of the selected child's state.
struct MappingParent;

impl Reducer for MappingParent {
    type State = MappingState;
    type Action = MappingAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        if matches!(action, MappingAction::Row(action) if matches!(action.action(), MappingRowAction::Output))
        {
            state.outputs += 1;
        }
        Effect::none()
    }
}

#[test]
fn for_each_clones_id_once_per_emitted_parent_action_after_warm_mount() {
    let clones = Arc::new(AtomicUsize::new(0));
    let id = CloneId {
        value: 7,
        clones: Arc::clone(&clones),
    };
    let reducer = for_each(
        MappingParent,
        KeyedStatePath::new(
            |state: &MappingState| &state.rows,
            |state: &mut MappingState| &mut state.rows,
        ),
        KeyedActionPath::new(
            |action: &MappingAction| match action {
                MappingAction::Row(action) => Some(action),
                MappingAction::Warm => None,
            },
            MappingAction::Row,
        ),
        MappingChild,
    );
    let runtime = ManualRuntime::new();
    let store = Store::new(
        MappingState {
            rows: [(id, 0)].into_iter().collect(),
            outputs: 0,
        },
        reducer,
        runtime.runtime(),
    );
    let warm = store
        .try_send(MappingAction::Warm)
        .expect("warm action fits");
    runtime.start_all();

    futures_executor::block_on(async {
        warm.await.expect("warm action commits").finish().await;
        let warm_clones = clones.load(Ordering::Relaxed);
        let receipt = store
            .send(MappingAction::Row(KeyedAction::new(
                CloneId {
                    value: 7,
                    clones: Arc::clone(&clones),
                },
                MappingRowAction::Start,
            )))
            .await
            .expect("mapping start commits");
        store
            .read(|_| ())
            .await
            .expect("mapping launches are queued");
        runtime.start_all();
        receipt.finish().await;

        let output_clones = clones.load(Ordering::Relaxed) - warm_clones;
        eprintln!("keyed_id_clones/two_outputs: clones={output_clones}");
        assert_eq!(output_clones, 2);
        assert_eq!(
            store
                .read(|state| {
                    (
                        state.outputs,
                        *state.rows.values().next().expect("row remains"),
                    )
                })
                .await
                .expect("mapping result reads"),
            (2, 2)
        );
        store.shutdown().await.expect("mapping Store shuts down");
    });
}

/// Omits `Debug` so missing-key diagnostics must use type context instead.
#[derive(Clone, Eq, Hash, PartialEq)]
struct MissingId(u8);

/// Starts empty so the direct action cannot identify a live child.
struct MissingState {
    rows: KeyedState<MissingId, MissingRowState>,
}

/// Identifies the absent child state in the failure diagnostic.
struct MissingRowState;

/// Identifies the absent child's action type in the failure diagnostic.
enum MissingRowAction {
    Ping,
}

/// Sends a direct action without any effect-origin cancellation context.
enum MissingAction {
    Row(KeyedAction<MissingId, MissingRowAction>),
}

/// Supplies the child reducer type; the missing-key action must not reach it.
struct MissingChild;

impl Reducer for MissingChild {
    type State = MissingRowState;
    type Action = MissingRowAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

/// Supplies the parent reducer without introducing another panic source.
struct MissingParent;

impl Reducer for MissingParent {
    type State = MissingState;
    type Action = MissingAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

#[test]
fn for_each_direct_missing_key_fails_store_with_full_type_context() {
    let reducer = for_each(
        MissingParent,
        KeyedStatePath::new(
            |state: &MissingState| &state.rows,
            |state: &mut MissingState| &mut state.rows,
        ),
        KeyedActionPath::new(
            |action: &MissingAction| match action {
                MissingAction::Row(action) => Some(action),
            },
            MissingAction::Row,
        ),
        MissingChild,
    )
    .missing_child_policy(MissingChildPolicy::FailFast);
    let runtime = ManualRuntime::new();
    let store = Store::new(
        MissingState {
            rows: KeyedState::new(),
        },
        reducer,
        runtime.runtime(),
    );
    runtime.start_all();

    futures_executor::block_on(store.send(MissingAction::Row(KeyedAction::new(
        MissingId(7),
        MissingRowAction::Ping,
    ))))
    .expect_err("direct missing keyed action must fail the Store");
    futures_executor::block_on(store.closed());
    assert!(store.is_closed());

    let results = runtime.join_all();
    assert_eq!(results.len(), 1, "only the Store actor was started");
    let panic = results
        .into_iter()
        .next()
        .expect("one actor result exists")
        .expect_err("missing-key panic must escape the actor");
    let diagnostic = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&'static str>().copied())
        .expect("panic diagnostic must be text");
    assert!(diagnostic.contains("ForEach"));
    assert!(diagnostic.contains(std::any::type_name::<MissingId>()));
    assert!(diagnostic.contains(std::any::type_name::<MissingState>()));
    assert!(diagnostic.contains(std::any::type_name::<MissingRowState>()));
    assert!(diagnostic.contains(std::any::type_name::<MissingAction>()));
    assert!(diagnostic.contains(std::any::type_name::<MissingRowAction>()));
}
