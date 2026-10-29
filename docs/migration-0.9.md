# Migration to the 0.9 composition design

This guide describes the composition design developed under the unpublished
`0.9.0-alpha.1` candidate and retained in the new `0.1.0` package line. Its filename
preserves existing links, not a published version sequence. Neither candidate
has been published; consult the [changelog](../CHANGELOG.md) and
[qualification summary](qualification.md) before adoption.

## Exhaustive TestStore steps

The current pre-stable TestStore compares state after every committed action.
`send(action)` and `receive(matcher)` now mean unchanged state. Use
`send_with(action, |expected| ...)` and `receive_with(matcher, |expected| ...)`
for expected mutations. Full-state construction requires independent
`Clone + Debug + PartialEq` state. Move-only domains use `projected` or
`projected_builder` with a named, independent snapshot covering their relevant
fields; production state and actions do not acquire comparison bounds.

`finish` checks all work and shuts down on success. Normal Drop also checks and
panics after cleanup if anything remains. During unwinding Drop only aborts,
preserving the original panic. Once admitted, an interrupted assertion remains
an obligation: later steps and finish reject it without recovery or rebasing.
Non-exhaustive action mode permits explicit skip-as-cancel only; state and
teardown checks still apply. Shared aliases and incomplete custom equality or
projections cannot establish full historical-state coverage.

See the `rca-test` crate examples and the
[current TestStore contract](runtime.md#teststore). The original qualification
checkpoint is retained as historical local evidence, not a current release
receipt. The older 0.7-to-0.8 guide records that historical transition, not this
revised contract.

## Borrow actions instead of owning or cloning them

Reducers now receive a borrowed action for the whole serial reduction
transaction. Replace an owned action parameter and any routing clone with a
borrowed match:

```text
before: fn reduce(&mut self, state: &mut State, action: Action, ...) -> Effect<Action>
after:  fn reduce(&mut self, state: &mut State, action: &Action, ...) -> Effect<Action>
```

The after shape compiles against `Reducer`. Parent and child reducers inspect
the same `&Action`; only an effect output must become an owned action. Do not
add `Action: Clone` to preserve old routing. `scope`, `if_some`, `if_case`, and
`for_each` use their path embedder to map owned effect outputs.

Moving a unique resource from an action into state is a separate concern: an
ordinary field cannot move out of `&Action`. The
[owned-action guide](migration/owned-actions.md) describes effect-owned resources
and explicit one-shot payload slots, including their ordering and testing costs.
Move-only admission alone does not establish equivalent ownership ergonomics.

## Replace prototype recursion with explicit composition

`RecursiveReducer` is not part of this source tree. There is no compatibility
shim. Replace a recursive derive or router with the public combinators that
make state/action ownership and lifetime boundaries explicit:

```rust
use rca::{ActionPath, OptionalStatePath, StatePath, combine, if_some, scope};

let child = scope(child_state, child_action, ChildReducer);
let reducer = if_some(ParentReducer, detail_state, detail_action, child);
let reducer = combine((reducer, AnalyticsReducer));
```

For normal fields, `StatePath` and `ActionPath` are the compiling handwritten
shape. Use `OptionalStatePath` with `if_some`, a case path with `if_case`, and
`KeyedStatePath` plus `KeyedActionPath` with `for_each`; do not reintroduce a
dynamic reducer graph or a custom action router.

## Replace keyed wrappers

Store keyed feature state in `KeyedState<Id, ChildState>` and route it with
`KeyedAction<Id, ChildAction>`:

```rust
use rca::{KeyedAction, KeyedState};

struct State {
    rows: KeyedState<u64, RowState>,
}

enum Action {
    Row(KeyedAction<u64, RowAction>),
}
```

`for_each(parent, keyed_state_path, keyed_action_path, child)` provides the
row routing and child lifetime. Equal cancellation values in different keyed
rows are isolated by their structural scopes. Removing row 2 ends only row
2's work; row 1 stays live.

## Let child lifetimes cancel their work

An optional/case child leaving state, or a keyed entry departing, automatically
cancels its descendant tasks, timers, observations, and scoped handles. A
reducer can still use `cancellable` or `cancellable_in_flight` for replacement
within one live child, but it does not need a manual cancellation action just
to cover removal. The representative
`examples/composition-search` test starts a detail timer and two equal-ID row
tasks, closes/removes their owners, and retains the causal tasks to prove the
departed work finishes while the surviving row completes normally.

## Use Store-scoped access and typed dependencies

Replace callbacks, direct state access, and global/guard dependency lookup with
Store APIs. A live handle is typed and cannot outlive its child generation:

```rust
let row = store.scope_keyed(rows_path, row_action_path, row_id).await?;
let value = row.read(|state| state.favorite.count).await?;
let updates = row.observe_distinct(|state| state.favorite.count).await?;
row.send(RowAction::Favorite(FavoriteAction::Toggle)).await?;
```

Install whole-store values on `Store::builder`, test values with
`TestStore::builder(...).with_dependency::<Key>(value)`, and local feature
overrides with `with_dependency::<Key, _>(value, reducer)`. The latter is
lexical: it affects only that reducer while it runs and does not become ambient
effect context. Reducers explicitly capture owned dependency values or handles
before returning effects; those captures retain the selected feature values.

TestStore no longer falls back to `DependencyKey::live_value` for missing keys.
Inject each value or implement `DependencyKey::test_value` with a deterministic
default; its default implementation panics with the missing key's type name.
This applies to projected and non-exhaustive tests too. Ordinary Store defaults
remain live. Tests intentionally exercising live clients must inject them
explicitly. Derived keys can add `test = expression` alongside `value` and `live`
for a reusable test default. Omitting `test` retains missing-test failure; it
does not reuse the live expression.

## Reduce path boilerplate when a derive is clearer

Constructible path namespaces implement `Debug` without requiring domain types to
implement it. Remove manual `Debug` implementations for these namespaces to
avoid conflicting implementations.
Unit namespaces format as their names. Lifetime/const namespaces include their
phantom field. Namespaces with type parameters are now aliases to isolated,
uninhabited marker types. Their path methods, generic defaults and domain-derived
variance remain available, without domain `Debug`, `Send` or `Sync` bounds.

These generic namespaces are type-level path containers with no values and no
`Debug` implementation. Remove namespace `Debug` bounds, including those
required by `Option<StatePaths<T>>` or `Result<_, StatePaths<T>>` formatting.
Tuple construction and tuple patterns are no longer supported for namespaces
with type parameters; use their associated path methods directly. The derives
reserve sibling module names beginning `__rca_path_namespace_` for isolated
storage. Do not depend on that storage, its type name or its private fields.
This pre-stable representation change deliberately favours hygienic, bound-free
path operations over preserving incidental marker construction syntax. Non-generic
and lifetime/const-only namespace representations are unchanged.

`#[derive(StatePaths)]` emits field path constructors and
`#[derive(ActionPaths)]` emits action path constructors. Mark optional and
keyed state fields with `#[rca(optional)]` and `#[rca(keyed)]`, respectively.
Mark keyed action variants with `#[rca(keyed)]`; action variants do not support
`#[rca(optional)]`. The opaque
generated action embedders expose the `Clone` bound required by effect mapping;
this is a bound on the non-capturing embedder, never on the action enum.
`reducers![first, second]` is the macro spelling of ordered `combine`.

The generated form below compiles through the same public combinators as the
handwritten form:

```rust
let row = scope(RowStatePaths::favorite(), RowActionPaths::favorite(), FavoriteReducer);
let rows = for_each(AppReducer, AppStatePaths::rows(), AppActionPaths::row(), row);
let app = if_some(rows, AppStatePaths::detail(), AppActionPaths::detail(), DetailReducer);
```

## Missing-child root sends are programmer errors

Direct root sends bypass scoped-handle generation validation. Sending an
unprovenanced child action while its optional/case/keyed state is absent is a
programming error governed by
[`MissingChildPolicy`](../rca-core/src/composition/missing_child.rs), not a
stale-output rejection. The default `ReportAndBase` reports one required
`rca.missing_child` issue with `IssueSeverity::Error`, skips the child, and reduces
the base (parent) once after diagnostic delivery is accepted.

Enable ordinary Store reporting with `Store::builder(...).with_issue_handler(...)`.
Disabled or closed reporting rejects delivery and panics with the original
`IssueReportError` before base reduction; reporter panics propagate before the
base runs too. Select `.missing_child_policy(MissingChildPolicy::FailFast)` on
the composition wrapper for an immediate panic without reducing child or base.
These panics close that Store. TestStore installs a required recorder: consume
the issue with `expect_issue(|issue| issue.code == "rca.missing_child").await`
before `finish`, which rejects unconsumed issues.

The exact optional/case issue message (or `FailFast` panic message) is:

```text
{combinator} received child action `{ChildAction}` in parent action `{ParentAction}` while child state `{ChildState}` was absent from parent state `{ParentState}`
```

For keyed state, the exact message is:

```text
ForEach received keyed child action `{ChildAction}` with ID type `{Id}` in parent action `{ParentAction}` while child state `{ChildState}` was absent from KeyedState in parent state `{ParentState}`
```

The placeholders are Rust type names. Obtain an optional, case, or keyed
`ScopedStore` first and call its `send`: a handle that has already ended
returns `ScopedStoreEnded` before reducer entry instead of triggering these
root-send diagnostics. Queued outputs from an old child generation are also
rejected before reducer entry, without a missing-child issue or base reduction;
the Store remains usable. The [policy tests](../rca-test/tests/missing_children.rs)
cover this distinction, including removal and same-ID reinsertion.

## Return owned state from a completed job

For the legacy `into_inner` use case, current RCA provides
`Store::shutdown_into_state(self)`. Await the job's required causal tasks or
other completion conditions first. Then await extraction and convert the returned
state into the application's output. Extraction requires `State: Send + 'static`,
not `Clone`, `Default` or `Sync`. It does not reproduce legacy extraction of
a Send output from non-Send state.

Extraction cancels pending effects instead of draining them. It closes scoped
handles and observations, then returns owned state after actor cleanup.
Cleanup requests cancellation without waiting for every runtime task wrapper
or external asynchronous cleanup operation. Such resources need their own
completion protocol.

The first processed terminal command wins, including requests from other
Store owners. Extraction is not idempotent: a losing caller receives
`StoreStopped`. Dropping an admitted extraction future does not retract its
command. Unclaimed state is dropped rather than returned to another caller.
Ordinary `shutdown` still drops state instead of transferring it.

The runnable finite-job example in `Store::shutdown_into_state` and the
[terminal extraction contract](runtime.md#terminal-owned-state-extraction)
describe this ownership boundary separately from legacy best-effort progress.
Its original qualification checkpoint remains historical local evidence.
