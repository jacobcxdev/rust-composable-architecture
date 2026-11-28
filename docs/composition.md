# Manual composition

RCA composes reducers with borrowing paths, ordered tuples, and Store-owned
child lifetimes. Required, optional, enum-case, and keyed composition work
without macros from `rca-core` and the umbrella `rca` crate.

## Choose the smallest composition model

When child work returns actions through effects, start with reducer composition.
Each child borrows its part of the parent state during a synchronous call.
This is *static composition*: Rust knows the reducer types and routes at compile time.
It does not mean that state, child membership or asynchronous work must be static.

| State relationship | Use | Lifetime decision |
| --- | --- | --- |
| One required child field | `scope` | The child shares its parent's lifetime. |
| A required child that can be replaced | `scope_identified` | A changed identity ends the previous child lifetime. |
| An optional child | `if_some` | Absence ends the child lifetime. Use `if_some_identified` for replacement while still present. |
| One active enum case | `if_case` | Leaving the case ends its lifetime. Use `if_case_identified` for replacement within the same case. |
| Independently removable children with stable IDs | `KeyedState` and `for_each` | Moving an entry preserves it. Removal and reinsertion create a new lifetime. |
| One ordered path whose removal discards a suffix | `StackState` and `stack` | Use a stack only when suffix removal matches the domain. See [recursive jobs](recursive-jobs.md). |

Some children need their own Feature instance, typed task access, commands,
outputs or fold contributions. Use [installed children](structural-features.md)
for those capabilities.
An installation is a Store-owned instance with a tracked lifetime, not another actor.
Both models share the parent state and serial transaction order. Both support
asynchronous work and reject output from ended child lifetimes.

`for_each` reuses one child reducer across all entries. Put per-entry domain values
in child state, not mutable fields on that shared reducer.
An `EntryMount` can instead construct a separate Feature instance for each entry.
Choose that form for per-instance captures or installed capabilities, not merely
because the application has multiple children.

The [keyed policy tests](../crates/rca-test/tests/keyed_policy.rs) show the shared-reducer
choice. The [installed roster tests](../crates/rca-core/tests/fold_keyed.rs) show separate
entry instances, retained order and replacement.

## Contracts

Store ingress owns each action. During one serial reduction transaction, the
Store lends `&Action` to the reducer tree. Matching child and parent reducers
therefore observe the same value without requiring `Action: Clone`, `Sync`,
`Debug`, or `Eq`. State remains exclusively mutably borrowed by the Store actor.

`combine((first, second, ...))` invokes tuple members in source order and merges
their effects in that order. Tuple arities 1 through 12 are implemented. Every
member must have exactly the same State and Action domains; there is no arity
zero because an empty tuple cannot establish those domains.

This order controls synchronous state changes and effect declarations. It does
not make asynchronous effects finish in that order. If later work depends on
earlier completion, use explicit sequencing.

`scope(state_path, action_path, child)` lifts one required child reducer into a
parent domain. On a matching action it borrows the child state, runs the child,
and maps owned child effect outputs back into parent actions. Put Scope before
the parent in `combine` when child-before-parent handling is required. A
nonmatching action and a matching action returning `Effect::none()` both take
the no-effect fast path.

`StatePath` holds immutable and mutable borrowing projections. `ActionPath`
holds a borrowing extractor and an owned embedder. `OptionalStatePath`,
`CaseStatePath`, `KeyedStatePath`, and `KeyedActionPath` provide equivalent
borrowed projections for lifecycle and keyed reducers. Paths use ordinary
closures and static dispatch; they do not use reflection or retain reduction
borrows.

The Scope embedder must be `Clone + Send + Sync + 'static` because an owned
child effect can emit more than one parent action after reduction. This bound is
on the small embedding closure, not on parent actions, child actions, state, the
extractor, or the child reducer. Empty child effects do not clone the embedder.

`Effect::map` is the owned output boundary used by Scope. It preserves `none`,
immediate sends, runtime tasks, merges, concatenation, cancellation, delayed
sends, intervals, cancellable work, replacement semantics, source order,
cancellation identity, and task lineage. Only emitted child actions become
owned parent actions through the embedder.

Composition remains statically dispatched below the Store's existing erased
root reducer. Composition adds no child dispatch object, lock, scheduler hop,
or runtime dependency to `rca-core`.

## Lexical dependency scopes

`with_dependency::<Key>(value, reducer)` installs a typed feature-local value
only while that reducer executes. Nested wrappers shadow their parent for the
same key. Sibling and parent reducers retain their own view, including when a
child panics: the borrowed context restores its previous layer during unwind.

For overrides that apply to one action or a whole Store, see
[which dependency value the feature receives](guides/dependencies.md#which-dependency-value-does-the-feature-receive).
An effect needs an owned client after this scope ends. The
[external-client recipe](guides/external-clients.md) shows where to capture it.

## Scoped Store handles

`ScopedStore<ChildState, ChildAction>` is a typed view over the same root actor,
mailbox, and state. It does not create another actor or copy child state, and it
does not expose parent State or Action types. Its `send`, `try_send`, `read`,
`observe`, and `observe_distinct` operations retain root mailbox order.

`Store::scope` creates a required child view sharing the root lifetime.
`scope_identified`, `scope_some`, `scope_some_identified`, `scope_case`,
`scope_case_identified`, and `scope_keyed` register the current child generation
at their mailbox position. These registered constructors are async because the
root actor is the identity authority.

The free `scope(...)` function composes behaviour. `Store::scope(...)` only creates
a handle that routes calls to behaviour already present in the Store.
Creating that handle does not install a reducer or add child-lifetime tracking.
Use a registered scoped constructor for a replaceable static child. Use
`Store::acquire` with a mount site for an [installed child](structural-features.md#install-once-then-acquire-the-installed-instance).

Optional, case, identified, and keyed handles end permanently when their child
is removed or its identity changes. Reinsertion creates another generation and
does not revive an old handle. Moving a keyed entry preserves its entry identity;
removing and reinserting an equal key does not. Clones of one handle share its
generation and closure signal.

A queued command keeps the child action, the function that builds the parent
action, and the child's generation. Before it runs the reducer, the actor
checks that the handle still refers to the current child.

For example, the queue can already contain a request to remove the child when
`try_send` accepts a child action. The removal runs first. The receipt then
returns `StoreStopped`, even though the root Store remains open. Neither the
parent-action function nor the reducer runs for that child action.

An immediate rejection instead returns `TryScopedSendError` with the unsent
action. It distinguishes a full queue, a child that left and a stopped Store.
Awaited scoped operations use `ScopedStoreEnded` to distinguish a child that
left from a stopped Store.

Scoped observations evaluate only while their generation is live. `observe`
publishes after every eligible commit. `observe_distinct` stores an actor-owned
comparison value and suppresses equal projections. Both emit an initial value
and use the existing one-slot latest-value protocol, so a slow consumer
coalesces commits instead of accumulating an unbounded queue. Invalidation
closes the observation without publishing a value from the departed or a later
reinserted generation.

Distinctness suppresses delivery, not selector execution. Each active selector
runs at registration and every eligible commit, including commits originating
from a task update. Selectors can read arbitrary state and captured values;
RCA does not infer property dependencies or skip evaluation because a different
child received an action. Keep projections small and avoid expensive work inside
selectors. Collection structural revisions track membership and order, not
changes to every contained value, so they cannot safely gate value observation.

For a list of worker IDs in display or processing order, use
[`observe_ordered_ids`](runtime.md#observation). It reports the ordered IDs,
not the workers' progress or lifetimes. Suppose one transaction removes a
worker and inserts another with the same ID at the same position. The old
generation's handle ends, but the list of IDs does not change. To follow
progress, observe the child value instead.

## Lifecycle composition

`if_some(parent, state, action, child)` and
`if_case(parent, state, action, child)` run a matching child before the parent.
They then reconcile presence after the parent has had a chance to remove or
replace the child. Entering a child mounts a fresh Store-local generation;
leaving it invalidates that generation and all descendants before returned
effects launch.

Presence defines the default optional or case lifetime. Use
`if_some_identified` or `if_case_identified` when a value can be replaced while
remaining present and an explicit identity should end the old lifetime.
Required `scope_identified` provides the same replacement-aware contract for a
required child. Unidentified required `scope` shares the parent lifetime.

Effects returned by a child automatically carry its lifetime provenance.
Leaving a child rejects queued stale outputs, cancels descendant tasks and
timers, closes descendant observations and handles, and discards work returned
by the transaction that removed it. Equal user cancellation IDs in sibling
structural scopes remain isolated.

A direct, unprovenanced child action sent to absent optional, case, or keyed
state is a programming error handled by
[`MissingChildPolicy`](../crates/rca-core/src/composition/missing_child.rs). The default
`ReportAndBase` reports one required `rca.missing_child` issue with
`IssueSeverity::Error`, skips the child, and reduces the base (parent) once,
but only after diagnostic delivery is accepted.

Enable ordinary Store reporting with `Store::builder(...).with_issue_handler(...)`.
Disabled or closed reporting rejects delivery and panics with the original
`IssueReportError` before the base runs; a reporter panic also propagates before
base reduction. Select `.missing_child_policy(MissingChildPolicy::FailFast)` on
the composition wrapper to panic immediately without reducing child or base.
TestStore installs a required recorder: consume the issue with
`expect_issue(|issue| issue.code == "rca.missing_child").await` before `finish`.

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

A queued output from an old child generation is different: the Store rejects
it before reducer entry, without a missing-child issue or base reduction, and
remains usable. The [policy tests](../crates/rca-test/tests/missing_children.rs) cover
both direct missing-child input and stale output.

## Ordered keyed state

`KeyedState<Id, State>` stores one state per unique ID with stable visible
order. Its backing is private. Public operations include borrowed lookup,
insertion, ordered removal, swap removal, movement, retention, clearing, and
borrowed or owned iteration. Normal use requires neither `Id` nor `State` to be
`Clone`; duplicate `try_insert` returns the owned ID and state in
`DuplicateId`.

Each collection owns a private lineage and each insertion receives a private
entry revision. Removing and reinserting an equal ID creates a new lifetime.
Lineage allocation is deferred until first insertion and retained after clearing;
never-used empty collections and their clones allocate no lineage.
Moving an entry preserves its lifetime. Assigning an independently constructed
collection replaces every lifetime. A conditional collection clone preserves
existing lineage and revisions while sharing future revision allocation.

`for_each(parent, state, action, child)` scopes one sequential child reducer
across a `KeyedState`. One action targets at most one child. The child runs
before the parent, then `ForEach` reconciles entry identity and leaves departed
scopes before effects launch. The child reducer is not cloned or constructed
per ID; mutable fields inside it are intentionally shared across entries.

Put per-child policy and progress in child state when they must vary by ID.
The shared reducer reads and updates the selected child's values, so policy
changes remain visible to exhaustive state assertions. Reinsert fresh child
state to reset those values; removing and reinserting an ID retires that entry's
effects without cancelling siblings. The executable
`crates/rca-test/tests/keyed_policy.rs` example qualifies this
pattern with distinct sequence policies and delayed outputs. It does not provide
per-entry reducer objects or arbitrary heterogeneous reducer types.

`Id` must implement `Eq + Hash + Clone + Send + Sync + 'static`. Warm matching
and unrelated empty-effect paths do not clone IDs. An effect output must own a
parent action, so each emitted child output clones its compact ID exactly once.
Use stable, cheap identifiers.

Each keyed collection exposes a crate-private lineage and structural revision to
`ForEach`. Reconciliation validates its Store-local structural token, then
compares the last fully reconciled stamp. Matching and unrelated actions whose
structure did not change use O(1) stamp checks before and after parent reduction
and allocate nothing when warmed. Any mount, lineage, or revision change forces
an authoritative O(n) entry refresh, including changes made by an earlier
reducer in a `combine` tuple or by direct whole-collection assignment.

Structural insertion, removal, movement, retention, and clearing increment the
private revision in O(1) in addition to their collection operation. A
conditional clone preserves lineage and shares future revision allocation;
independently built or divergently cloned collections receive distinguishable
stamps. A mismatched cached stamp is invalidated before full-scan mutation, so a
caught user-code panic cannot make a later exact-clone restoration skip repair.

## Example

This required counter runs child logic before parent accounting. The paths use
ordinary closures and pattern matching.

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

The [required-child doctest](../crates/rca-core/src/composition/scope.rs) shows the parent reading state after the child updates it.
The shared deterministic, Tokio current-thread, and Tokio multi-thread
conformance scenario additionally proves a non-Clone input is borrowed by both
reducers and a child-owned effect output is mapped back into the parent.

## Derived paths and tuple composition

With the `macros` feature, the `StatePaths` and `ActionPaths` derives generate the same static
field/action path constructors as the handwritten closures above. Annotate
optional and keyed members with `#[rca(optional)]` and `#[rca(keyed)]`.
`reducers![first, second]` is the macro spelling of ordered `combine`.

Generated action-path embedders expose `Clone` because a mapped effect may
emit more than one parent action. That bound applies to the small noncapturing
embedder closure, not to the action enum. Derived and handwritten paths compose
through `scope`, `if_some`, and `for_each` without `Action: Clone`.
The [search application](../examples/composition-search/src/lib.rs) shows both forms.
Its [tests](../examples/composition-search/tests/app.rs) use the same inputs to compare their child lifetimes.

To generate the parent domains and their routing, use `#[rca::reducer(...)]` with `body(generated)`.
In the generated state, `#[reducer(child = child)] child: _` selects the child's state type.
In the generated action, `#[reducer(child = child)] Child(_)` selects its action type.
The [complete required-child example](../tests/downstream-packaged/tests/reducer_synthesis.rs) declares the child reducer, binds it once, and sends two inputs.
Compare its generated routing with the [handwritten required counter](#example).

Constructible path namespaces implement `Debug` without bounds on domain types.
Namespaces with type parameters are aliases to isolated, uninhabited marker types.
They have no values or `Debug` implementation. Their associated path methods,
generic defaults and domain-derived variance do not require domain `Debug`,
`Send` or `Sync` bounds.

Use their associated methods rather than tuple construction
or patterns. The derives reserve sibling modules beginning `__rca_path_namespace_`
for private storage. Application code must not depend on that storage.

An action variant supports `#[rca(keyed)]`, not `#[rca(optional)]`.
Optional annotations belong on state fields. The
[owned-resource guide](guides/owned-resources.md) explains resources carried by
borrowed actions and one-shot payload slots.

## Keyed example

This extract routes one row action with handwritten paths. It omits the row types and both reducers.
See the [complete search reducer](../examples/composition-search/src/lib.rs) and its [application manifest](../examples/composition-search/Cargo.toml).

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

Structural keyed reconciliation remains O(n), while unchanged matching and
unrelated reconciliation use O(1) stamp checks. Scope lookup is average O(1)
plus collision-chain length; concrete identity equality remains authoritative.
Leaving M registry nodes is average O(M) with deterministic iterative postorder.
These are algorithmic bounds, not a numerical performance comparison.

This guide describes reducer composition, not the full capability set.
Stack and presentation state, persistence, owner-local Stores, debounce and
bounded retry have separate APIs. The [structural Feature guide](structural-features.md)
maps mounted child input, task updates, ancestor output and folds to their
intended uses. Native UI and networking policy remain application or provider
responsibilities. State mutation stays serial within each Store.

Performance measurements need matching workloads, source revisions and host
conditions. Historical local timings do not qualify a later release.
See the [qualification summary](maintainers/qualification.md) for the current evidence boundary.
