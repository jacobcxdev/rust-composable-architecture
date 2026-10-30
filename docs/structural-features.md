# Structural features for headless applications

For the end-to-end workflow, start with the [RCA authoring guide](guides/authoring.md).
This page explains the detailed installation, routing and ownership contracts.

A Feature defines synchronous domain behaviour and declares follow-up work.
Every `Reducer` is a Feature automatically. Both use `Store::new`, `Store::builder`
and the corresponding `TestStore` constructors. `Reducer::reduce` returns effects.
Direct `Feature::update` implementations can also declare tasks with typed state access.

Static composition, such as `Scope` and `for_each`, requires the narrower `Reducer`
contract. Its state and selectors can borrow local values during reduction.
Installed children accept either form and keep their own task lifetime.

A mounted Feature also has an installation identity, private captures and work
tied to that installation.
The parent owns the child State. Input routing does not transfer that ownership.
RCA supplies no renderer or native event-loop adapter.

Import shared authoring types directly from `rca`, or from `rca_core` in a
runtime-neutral integration crate. This includes `Feature`, `EntryMount`,
`Fold`, `InputGate` and `RemovalSite`. Owner-thread alternatives live under
`local`; shared policy and identity types remain at the root. See the
[API path guide](support.md#public-api-paths-in-010) for supported import paths.

## Choose the route that matches the work

| Need | Recommended route | Important distinction |
| --- | --- | --- |
| Record a meaningful domain event | `TaskContext::send` to the captured Feature | A mounted child input does not require a parent action case. |
| Read current policy or publish task progress | `TaskContext::read` or `update` | The actor orders each call. A suspension does not retain a State borrow. |
| Give an external producer limited input authority | `CommandPort<C>` | The port sends one command type. It does not expose State reads or updates. |
| Send an owned outcome to declared ancestors | `Output<O>` | Handlers receive the same owned value through an explicit route. This is not a broadcast. |
| Read contributions from installed descendants | `Fold<R, K>` | The committed roster determines membership and order. The read does not dispatch actions. |
| Wait for a named set of tasks | `Operation::capture` returns an `OperationWait` | The captured wait excludes later generations. The reusable operation handle can capture again. |

The [uploader](../examples/composition-search/docs/uploader.md) uses one task for
fetch, processing and upload. Progress uses serial updates. Domain completion
uses an owned outcome. Separate actions for every internal task step are not
required.

## Install once, then acquire the installed instance

`OptionalMount` declares an optional child. `EntryMount` supplies a Feature for
each keyed entry. Child recipes declare further children, outputs and fold
contributions. `RootMount` provides explicit root installation hooks.

`MountIdentity` separates the business key, replacement identity and
configuration identity. Replacement creates a new installation. Configuration
changes invalidate previously acquired authority without necessarily rebuilding
the Feature. Keyed reordering preserves the retained entries.

After `Store::ready`, `Store::acquire(&site)` returns the current child context
or absence. The context stays bound to that installation and configuration.
An old callback cannot update a replacement with the same business key.
Reacquisition starts new work. It is not recovery for an obsolete continuation.

Restored State can already contain children at startup. Installation constructs
their Features before the Initial hooks run. Initial factories run in
child-before-parent order. Returned task bodies start only when the runtime
polls them. Loading saved State does not resume former asynchronous work.

The [startup order tests](../rca-core/tests/mount_acquisition/initial_order.rs)
exercise these boundaries. The [keyed roster tests](../rca-core/tests/fold_keyed.rs)
exercise reordering and replacement.
[`Store::scope`](composition.md#scoped-store-handles) is a projection with root
lifetime, not an equivalent installation handle.

The [compatibility tests](../rca-test/tests/legacy_feature_compatibility.rs)
exercise both routes without treating static scopes as mounted authority.
The [five-level plugin fixture](../rca-core/tests/depth_five_plugins.rs) sends
10,000 private commands without adding ancestor action cases. The
[recursive jobs guide](recursive-jobs.md) compares recursive and flat ownership.

## Separate captured inputs from current State

A task owns its captured inputs and clients. Those values do not refresh after
an await. A subsequent `context.read` observes current State through the actor.
An application can explicitly read a current client stored in that State.
It does not implicitly replace the task's captured dependency container.

Eager State arguments exist before a Store constructor runs. Dependency
configuration inside that constructor cannot affect their prior construction.
The [provider example](../rca-core/tests/feature_provider_handover.rs) uses an
explicit scoped factory for construction, then distinguishes captured clients
from later State reads. The [session refresh example](../rca-test/tests/session_client_refresh.rs)
also preserves request identity across a client change.

`on_change` compares selected values across a committed transaction. It is not
an event for every transient assignment. `on_initial` represents installation,
not an ordinary value change. `ReactionChange` distinguishes the owned initial
selection from the previous and current selections of a changed transaction.
[Reaction tests](../rca-core/tests/changed_reactions.rs)
describe the exact pairing and callback order.

## Keep input admission, outcomes and reads separate

A `CommandPort` captures send-only authority. `try_send` succeeds at queue
admission, not at commit. Its receipt supplies the later commit result.
`send` waits for admission and commit. Neither result means that all resulting
work has physically finished. [Command tests](../rca-core/tests/command_port.rs)
cover owned payload recovery and stale destinations.

Closing an input gate rejects ordinary input. It does not remove the Feature,
cancel its tasks or remove its fold contribution. Only the captured direct
parent's `FeatureContext` can change the gate. A control port bypasses the input
bit but does not let a child reopen itself. Its bypass does not transfer to
ordinary follow-up work.

An `Output` declaration names its source and explicit ancestor handlers.
The declaration owner controls callback lifetime. An output is not a root-action
fallback or a permission to mutate an unrelated Store. The
[authorisation recipe](../examples/composition-search/docs/authorisation.md)
carries an owned reply endpoint. The requester rechecks its own lifetime through
its captured context before it updates State.

`FoldKey` defines an owned identity value and ordered reduction with
`ControlFlow`. Ordinary contributions follow preorder. A declared transform
groups only its subtree. A `Break` ends that group, whose transform still runs.
Fold therefore does not require commutative or associative combination.
The [fold tests](../rca-core/tests/fold.rs) and
[shape tests](../rca-core/tests/fold_shape.rs) show the actual ordering.
The [capacity fixture](../rca-core/tests/fold_input_gate.rs) checks the literal
`5 → 3 → 0` result as domain values change while both declarations stay installed.

## Keep thread-affine work on its owner

The optional `rca::local` surface accepts non-`Send` State, actions and tasks.
Its runtime polls and destroys them on their owner thread. Cross-thread wakes
notify that owner. They do not transfer the payload to the waking thread.

Use `local::Effect` with a Local reducer or Feature. It supports owned non-`Send`
operations and futures, action mapping, merge and concatenation, timers, typed
cancellation and fallible work. `send_at` preserves an absolute runtime deadline;
`delay` defers the programme rather than only its final action. Named operations
and cancellation keys keep their portable identity requirements.

`without_inherited_cancellation` removes inherited cancellation keys, not root
task, installation or Store ownership. `try_run_reported` retains the original
typed cause and reporting-delivery evidence without requiring local captures
to become `Send`. Neither API detaches work. Shared and Local effects are
distinct types; share synchronous domain logic rather than implicitly converting
between their asynchronous leaves.

Local `observe` delivers owned projections without requiring `Clone`.
`observe_distinct` additionally requires `Clone + PartialEq` to suppress unchanged
values. Both selectors run on the owner thread; dropping an observation ends
its registration, not the Store or work already admitted to it.

The [local worker](../rca-core/tests/local_worker_replacement.rs) keeps an `Rc`
interpreter through suspension, current-State reads and updates, then rejects
its old context after replacement. The [local mount examples](../rca-core/tests/local_mount.rs)
show independent child actions and nested installation. These examples supply
no platform event loop. [Synchronous local reducers](local-composition.md) are
a separate use case and require no asynchronous host.

## Assert transactions and finish external cleanup explicitly

TestStore asserts transactions and task-produced inputs, not observation
notifications. Independent snapshots preserve history. An aliased `Rc<Cell<_>>`
does not preserve historical values. The
[owned importer](../examples/composition-search/docs/owned-importer.md) shows
separate domain, private and shared expectations.

Writes through an application-owned `Rc<RefCell<_>>` alias bypass RCA's
transaction boundary. They do not automatically trigger reactions, publication
or TestStore assertions. Use checked context updates for changes that need
those guarantees, and independent snapshots for historical expectations.

An independently acquired child context can identify the original action for
`receive_mounted`. A mismatching matcher does not consume that pending action.
An exact-source mismatch retains sticky assertion debt; a later match permits
cleanup, not assertion recovery. Wrong-source matching creates no such debt.
The [mounted reception tests](../rca-test/tests/mounted_frontier.rs) exercise this
contract for both portable and local Stores. Latest-value observation can
coalesce changes. It cannot replace exhaustive transaction assertions.

Logical retirement rejects stale work. Cancellation requests termination.
Neither operation proves physical release of an external resource.
Descendant declaration payloads retire child-first before successor construction.
Mount sites do not keep those declarations alive. An expired site rejects
acquisition without occupying mailbox capacity; an already-admitted acquisition
still checks its original lifetime and cannot retarget a successor.
The [receipt guide](receipt-lifetimes.md) separates commit, completion and
destruction. Document sessions and managed servers have explicit cleanup joins.

The [timing guide](guides/debounce-throttle.md) covers debounce and throttle.
[Predicate waits](../rca-core/tests/predicate_wait.rs) observe a State condition,
not causal task completion. Native controls remain host-owned, as described in
[integration boundaries](integration-boundaries.md).
