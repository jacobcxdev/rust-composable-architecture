# Compose child features

Composition lets one Store run several features that share its state.
For example, a search application can use the same child reducer for every row of results.
When a row receives an action, that reducer changes the row's state. The parent can then read the change.
Neither reducer needs a second Store.

If these terms are new, start with [state, actions, reducers, and effects](authoring.md#the-basic-model).
The [counter example](#example) shows a complete pair of reducers and the paths that connect them.
All the manual constructors in this guide work without macros. Import them from `rca`, or from `rca_core` for a core-only integration.

## Choose the smallest composition model

If a child's follow-up work returns actions, start with reducer composition.
Each child borrows its part of the parent state during a synchronous call.
This is *static composition*: Rust knows the reducer types and routes at compile time.
State, child membership, and asynchronous work can still change.
The child's *lifetime* is the period in which its work can update that particular child.

| State relationship | Use | Lifetime decision |
| --- | --- | --- |
| One required child field | `scope` | The child shares its parent's lifetime. |
| A required child that can be replaced | `scope_identified` | A changed identity ends the previous child lifetime. |
| An optional child | `if_some` | Absence ends the child lifetime. Use `if_some_identified` for replacement while still present. |
| One active enum case | `if_case` | Leaving the case ends its lifetime. Use `if_case_identified` for replacement within the same case. |
| Independently removable children with stable IDs | `KeyedState` and `for_each` | Moving an entry preserves it. Removal and reinsertion create a new lifetime. |
| A path where removing one child also removes all later children | `StackState` and `stack` | A stack preserves that removal order. See [recursive jobs](recursive-jobs.md). |

If a child needs its own feature object or private input, see [installed children](structural-features.md).
An installation is a feature instance that the Store owns for one lifetime.
An installed child can also accept typed updates from its tasks, send a result to an ancestor, or contribute to a group total.
Both models use the parent's state. The Store processes changes one at a time and rejects output from a child that has left.

`for_each` reuses one child reducer across all entries. Put values such as each row's progress in that row's state.
Mutable fields on the reducer belong to all rows. An `EntryMount` can instead construct a separate feature instance for each entry.
If each instance needs its own captured resources or an installed child's capabilities, choose that form.

The [keyed policy tests](../../crates/rca-test/tests/keyed_policy.rs) show the shared reducer.
The [installed worker tests](../../crates/rca-core/tests/fold_keyed.rs) show separate instances, order, and replacement.

## Contracts

The Store takes ownership of each incoming action. During an update, it lends the action to the reducers as `&Action`.
A matching child and its parent can therefore read the same value.
The action needs no `Clone`, `Sync`, `Debug`, or `Eq` implementation for this purpose.
Only the Store can lend mutable access to its state during the update.

`combine((first, second, ...))` runs the reducers from left to right and merges their effects in that order.
A tuple can contain one to twelve reducers. Every reducer in it must use the same state and action types.
An empty tuple has no reducer from which Rust can determine those types.

This order controls synchronous state changes and effect declarations. It does
not make asynchronous effects finish in that order. If later work depends on
earlier completion, use explicit sequencing.

`scope(state_path, action_path, child)` lifts one required child reducer into a
parent domain. On a matching action it borrows the child state, runs the child,
and wraps each action from the child's effect in a parent action.
If the parent needs to see the child's change, put the scope before the parent in `combine`.
If the action does not match, the scope returns no effect. The same applies to a child that returns `Effect::none()`.

`StatePath` holds two functions: one borrows the child state for reads and the other borrows it for changes.
`ActionPath` also holds two functions. Its *extractor* borrows a matching child action from the current parent action.
Its *embedder* wraps an owned child action in a new parent action.
`OptionalStatePath`, `CaseStatePath`, `KeyedStatePath`, and `KeyedActionPath` serve the corresponding optional, enum, and collection cases.
These paths use ordinary Rust closures. They do not inspect types at runtime or keep references to the current update.

The embedder must satisfy `Clone + Send + Sync + 'static`. A child effect can emit multiple actions after the call returns.
This requirement applies to the embedder, not to the state, actions, extractor, or child reducer.
An empty child effect does not clone the embedder. See [borrowed and thread-local values](local-composition.md) for their separate requirements.

The scope uses `Effect::map` to change only the actions that an effect emits.
It preserves the original effect, including sends, tasks, timers, merges, sequences, and cancellation.
It also preserves replacement, order, cancellation identity, and the relationships between parent and child tasks.
The Store calls its root feature through a common interface. Below that root, Rust knows which child reducer to call.
Composition adds no extra lock, task switch, child dispatch object, or runtime dependency.

## Lexical dependency scopes

`with_dependency::<Key>(value, reducer)` gives one reducer a specific value for a dependency.
That value applies only during the call. If wrappers use the same key, the innermost wrapper supplies the value.
Other reducers keep their own values. If a child panics and Rust unwinds the call, the context restores its previous value.

For overrides that apply to one action or a whole Store, see
[which dependency value the feature receives](dependencies.md#which-dependency-value-does-the-feature-receive).
An effect needs an owned client after this scope ends. The
[external-client recipe](external-clients.md) shows where to capture it.

## Scoped Store handles

`ScopedStore<ChildState, ChildAction>` gives a caller access to one child without exposing the parent state or action types.
It uses the root Store's task, queue, and state. It does not create a second Store or copy the child.
The root processes its `send`, `try_send`, `read`, `observe`, and `observe_distinct` requests in queue order.

`Store::scope` creates a view of a required child for the lifetime of the root Store.
`scope_identified`, `scope_some`, `scope_some_identified`, `scope_case`,
`scope_case_identified`, and `scope_keyed` register a view of a particular *generation*: one lifetime of a child.
These constructors are asynchronous because only the Store knows which child exists when it reaches the request.

The free `scope(...)` function composes behaviour. `Store::scope(...)` only creates
a handle that routes calls to behaviour already present in the Store.
Creating that handle does not install a reducer or add child-lifetime tracking.
Use a registered scoped constructor for a replaceable static child. Use
`Store::acquire` with a mount site for an [installed child](structural-features.md#install-once-then-acquire-the-installed-instance).

If the child leaves or changes identity, its registered handle ends permanently.
For example, moving a row preserves its handle. Removing that row and inserting a new row with the same key does not.
Clones of a handle refer to the same generation and close together.

A queued command keeps the child action, its embedder, and the child's generation.
Before the Store runs the reducer, it compares that generation with the current child's generation.

For example, the queue can already contain a request to remove the child when
`try_send` accepts a child action. The removal runs first. The receipt then
returns `StoreStopped`, even though the root Store remains open. Neither the
parent-action function nor the reducer runs for that child action.

If the Store rejects the send immediately, `TryScopedSendError` contains the unsent action.
It distinguishes a full queue, a child that left, and a stopped Store.
Awaited scoped operations use `ScopedStoreEnded` to distinguish a child that
left from a stopped Store.

An observation selects a value from the child. Both observation methods first produce the current value.
While that child is live, `observe` publishes the selected value after every eligible commit.
The Store keeps only the latest value for the reader. A slow reader can therefore miss intermediate values.
When the child leaves, its observation closes without publishing a value from its replacement.

`observe_distinct` runs the selector too, but publishes only if the result differs from the Store's previous comparison value.
Each active selector runs at registration and after every eligible commit, including task updates and actions for other children.
RCA cannot infer which fields or captured values a selector reads. Keep the calculation small.
The collection tracks changes to membership and order separately from edits to individual values.

For a list of worker IDs in display or processing order, use
[`observe_ordered_ids`](runtime.md#observation). It reports the ordered IDs,
not the workers' progress or lifetimes. Suppose one transaction removes a
worker and inserts another with the same ID at the same position. The old
generation's handle ends, but the list of IDs does not change. To follow
progress, observe the child value instead.

## Lifecycle composition

`if_some(parent, state, action, child)` and
`if_case(parent, state, action, child)` run a matching child before the parent.
After the parent runs, RCA checks whether the child still exists.
A new child starts a new generation in this Store.
If the old child left, its generation and all its descendants end before the Store starts the returned effects.

By default, an optional child lives as long as the value remains present.
An enum child lives as long as its case stays active.
If replacing a present value must end its lifetime, use `if_some_identified` or `if_case_identified`.
For a required child that can change identity, use `scope_identified`. The ordinary required `scope` shares its parent's lifetime.

Each child effect carries its generation. When that child leaves, the Store:

- Rejects queued output from that generation.
- Cancels its descendant tasks and timers.
- Closes its descendant observations and handles.
- Discards new work from that child in the transaction that removed it.

Siblings can reuse the same cancellation ID without cancelling each other's work.

An application can also send a child action directly through its parent.
Unlike an effect or scoped handle, that input does not identify a particular child lifetime.
If its optional, case, or keyed child is absent, [`MissingChildPolicy`](../../crates/rca-core/src/composition/missing_child.rs) handles the programming error.
The default policy, `ReportAndBase`, reports one `rca.missing_child` issue with `IssueSeverity::Error`.
Once the reporter accepts it, the Store skips the child and runs the parent once.

For an ordinary Store, enable reporting with `Store::builder(...).with_issue_handler(...)`.
If reporting is disabled or closed, the Store panics with the original `IssueReportError` before the parent runs.
A reporter panic also propagates before the parent runs.
To panic immediately without running either reducer, select `.missing_child_policy(MissingChildPolicy::FailFast)` on the wrapper.
TestStore installs a recorder. Before `finish`, consume the issue with `expect_issue(|issue| issue.code == "rca.missing_child").await`.

The optional and case diagnostic uses this message:

```text
{combinator} received child action `{ChildAction}` in parent action `{ParentAction}` while child state `{ChildState}` was absent from parent state `{ParentState}`
```

The keyed diagnostic uses this message:

```text
ForEach received keyed child action `{ChildAction}` with ID type `{Id}` in parent action `{ParentAction}` while child state `{ChildState}` was absent from KeyedState in parent state `{ParentState}`
```

The diagnostic names the relevant Rust types and composition wrapper.
These messages also identify `FailFast` panics.
Each policy panic closes the Store. A scoped handle rejects an ended child
before reducer entry instead of triggering a missing-child diagnostic.

A queued output from an old child generation is different. The Store rejects it before either reducer runs.
It reports no missing-child issue and remains usable. The [policy tests](../../crates/rca-test/tests/missing_children.rs) cover
both direct missing-child input and stale output.

## Ordered keyed state

`KeyedState<Id, State>` stores one state per unique ID and keeps entries in order.
For example, reordering rows changes their positions without changing which row an action addresses.
You can look up, insert, move, retain, remove, clear, or iterate over entries without copying them.
Removal can preserve order or swap in the last entry.
The collection itself does not require `Id: Clone` or `State: Clone`.
If an ID already exists, `try_insert` returns the unused ID and state in `DuplicateId`.

RCA tracks each collection's origin and each entry's lifetime separately from its business ID.
Moving an entry preserves its lifetime. Removing and reinserting an equal ID creates a new lifetime.
Replacing the collection with an independently constructed one ends every previous entry lifetime.
Cloning a collection preserves its existing entries' lifetimes. Later insertions in either copy receive distinct identities.
A new empty collection and its clones allocate no tracking identity until the first insertion. Clearing the collection retains that identity.

`for_each(parent, state, action, child)` uses one child reducer for all entries.
An action selects at most one entry. The child runs before the parent.
After the parent runs, `ForEach` ends the lifetimes of entries that left. Only then can the Store start effects from surviving entries.
The reducer itself stays the same object across calls for different IDs.

If policy or progress differs by ID, put it in child state. TestStore can then assert the change for each entry.
To reset those values and retire the entry's effects, remove the entry and insert fresh state.
This does not cancel work for its siblings. The [keyed policy example](../../crates/rca-test/tests/keyed_policy.rs) uses different rules and delayed results for each entry.
It uses one child reducer type, not a collection of different reducer types.

`for_each` requires `Id: Eq + Hash + Clone + Send + Sync + 'static`.
Once it has registered the entries, a matching or unrelated action that returns no effect does not clone any IDs.
Each action that a child effect emits needs its own ID for routing. It clones the ID once.
Prefer a small, stable ID.

Before and after the parent runs, RCA checks whether the collection or its structure changed.
For an unchanged collection that RCA already tracks, this check takes constant time and allocates nothing.
If the mount, collection origin, or structure changes, RCA scans the entries.
That includes changes made by an earlier reducer in a `combine` tuple.
The scan takes time proportional to the number of entries.

Each structural operation adds constant bookkeeping cost to the cost of the operation itself.
RCA distinguishes two clones that later change independently.
If user code panics during a scan, restoring an old clone does not let the next check skip unfinished work.

## Example

Suppose a parent records how many actions reached its counter. The count lives in the parent state's counter field.
Two paths connect them: one selects the counter's state and one converts between their action types.
The final line with `combine` puts the counter first. One `Increment` changes the count before the parent records the action.

```rust
use rca::{ActionPath, Effect, ReduceContext, Reducer, StatePath, combine, scope};

struct CounterState {
    count: usize,
}

struct AppState {
    counter: CounterState,
    counter_actions: usize,
}

enum CounterAction {
    Increment,
}

enum AppAction {
    Counter(CounterAction),
}

struct Counter;

impl Reducer for Counter {
    type State = CounterState;
    type Action = CounterAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            CounterAction::Increment => state.count += 1,
        }
        Effect::none()
    }
}

struct App;

impl Reducer for App {
    type State = AppState;
    type Action = AppAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            AppAction::Counter(_) => state.counter_actions += 1,
        }
        Effect::none()
    }
}

let counter_state = StatePath::new(
    |state: &AppState| &state.counter,
    |state: &mut AppState| &mut state.counter,
);
let counter_action = ActionPath::new(
    |action: &AppAction| match action {
        AppAction::Counter(action) => Some(action),
    },
    AppAction::Counter,
);

let reducer = combine((scope(counter_state, counter_action, Counter), App));
let _: &dyn Reducer<State = AppState, Action = AppAction> = &reducer;
```

The [required-child example](../../crates/rca-core/src/composition/scope.rs) shows the parent reading state after the child updates it.
The runtime tests also exercise this order with an input that does not implement `Clone`.
Both reducers borrow the input. A child effect can send an owned action back to the parent.

## Derived paths and tuple composition

With the `macros` Cargo feature, the `StatePaths` and `ActionPaths` derives generate constructors for the same paths.
Use `#[rca(optional)]` on an optional state field. Use `#[rca(keyed)]` on keyed state fields and matching action variants.
`reducers![first, second]` is the macro spelling of ordered `combine`.

A generated embedder implements `Clone` because its effect can emit more than one action.
Neither derived nor handwritten paths require `Action: Clone` for `scope`, `if_some`, or `for_each`.
The [search application](../../examples/composition-search/src/lib.rs) shows both forms.
Its [tests](../../examples/composition-search/tests/app.rs) use the same inputs to compare their child lifetimes.

To generate the parent domains and their routing, use `#[rca::reducer(...)]` with `body(generated)`.
In the generated state, `#[reducer(child = child)] child: _` selects the child's state type.
In the generated action, `#[reducer(child = child)] Child(_)` selects its action type.
The [complete required-child example](../../tests/downstream-packaged/tests/reducer_synthesis.rs) declares the child reducer, binds it once, and sends two inputs.
Compare its generated routing with the [handwritten required counter](#example).

The derives group path constructors on a generated type called a *path namespace*.
Use its associated methods instead of constructing or matching a tuple.
A namespace without type parameters implements `Debug` without requiring `Debug` on your state or action.
A namespace with a type parameter has no values or `Debug` implementation.
Its generated methods do not add `Debug`, `Send`, or `Sync` bounds to your types.
It preserves generic defaults and the borrowing relationships of those types.
Modules with names that start with `__rca_path_namespace_` are private to the derives. Do not use them in application code.

An action variant supports `#[rca(keyed)]`, not `#[rca(optional)]`.
Optional annotations belong on state fields. The
[owned-resource guide](owned-resources.md) explains resources carried by
borrowed actions and one-shot payload slots.

## Keyed example

This extract routes one row action with handwritten paths. It omits the row types and both reducers.
See the [complete search reducer](../../examples/composition-search/src/lib.rs) and its [application manifest](../../examples/composition-search/Cargo.toml).

```rust
use rca::{
    KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, ReducerExt,
};

struct RowsState {
    rows: KeyedState<u64, RowState>,
}

enum RowsAction {
    Row(KeyedAction<u64, RowAction>),
    Refresh,
}

let rows = KeyedStatePath::new(
    |state: &RowsState| &state.rows,
    |state: &mut RowsState| &mut state.rows,
);
let row_action = KeyedActionPath::new(
    |action: &RowsAction| match action {
        RowsAction::Row(action) => Some(action),
        RowsAction::Refresh => None,
    },
    RowsAction::Row,
);
let reducer = RowsReducer.for_each(rows, row_action, RowReducer);
```

`ForEach::new` and free `for_each` are equivalent to the extension method. The
free constructors are useful when inference or explicit reducer order is
clearer without extension syntax.

## Current limitations

Looking up a scope takes constant time on average. If IDs have the same hash, RCA compares them for equality.
Ending several registered lifetimes takes time proportional to their number on average.
RCA ends descendants before their parents in a predictable order without recursive calls.
These costs describe how work grows with input size. They do not predict latency on your machine.

This guide describes reducer composition, not the full capability set.
Stack and presentation state, persistence, owner-local Stores, debounce and
bounded retry have separate APIs. The [structural Feature guide](structural-features.md)
maps mounted child input, task updates, ancestor output and folds to their
intended uses. Native UI and networking policy remain application or provider
responsibilities. State mutation stays serial within each Store.

To measure the work your own application does with child state, see [application performance](performance.md).
