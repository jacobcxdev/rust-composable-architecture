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

Use the [composition decision table](composition.md#choose-the-smallest-composition-model)
before adding mounts. Ordinary reducer composition already supplies child lifetimes
and cancellation. Mounts add installed instances and their typed capabilities.

## Terms used in this guide

- **State** holds the domain values that the parent owns. A child borrows only its selected part during a transaction.
- **Feature instance** holds behaviour and private captures. Those captures are not automatically part of a TestStore state assertion.
- **Declaration** describes how the Store selects, constructs and connects a child.
- **Installation** is one live instance created from that declaration. An *incarnation* means one such lifetime, including its replacement identity.
- **Authority** is permission to access that captured installation. It ends when the relevant Store, installation or configuration ends.
- **Mount site** names a declaration without keeping its callbacks or child alive. Acquisition resolves the current installation into a typed context.

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

These routes are alternatives for specific needs, not a checklist for every feature.
A root action is appropriate when the root owns the domain decision.
A private child command avoids adding ancestor action cases solely to transport that input.
An output transfers an owned result along its declared ancestor route.
A fold answers a question about current descendants, such as their available capacity.
Neither an output nor a fold creates a second state owner.

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

| Change | Identity to use | Result |
| --- | --- | --- |
| The same worker moves within a list | Keep its business key and entry | The existing installation remains live. A list position is not its identity. |
| A different business entity occupies the slot | Change `key` | The previous installation ends. |
| A worker restarts with the same business ID | Change `replacement` | The Store constructs a fresh Feature instance. Old contexts cannot reach it. |
| Policy changes, but the Feature's private cache must survive | Change `configuration` | The Feature stays installed, but old contexts lose authority. New work needs fresh acquisition. |

The markers describe domain events, not every state edit. Leave them unchanged
for ordinary progress updates. For an optional child, removing and restoring an
equal value inside one callback is not visible as two separate transactions.
If that sequence must represent a new lifetime, change its replacement marker.

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

## Choose a reaction, an observation or an explicit update

Use the current reducer or Feature callback for a state change caused directly
by its input. Use `TaskContext::update` for a later change made by that task.
Both preserve the Store's serial transaction boundary.

When follow-up work depends on changes from several input routes, use `on_change`.
It compares selected values across a transaction, not one particular action. For example, a worker can
react to a changed endpoint regardless of which command changed it.
The selector must return an independent value. A cloned mutable handle does not
preserve the value from before the transaction.

Use `on_initial` for work tied to a new installation. If an `on_change` registration
also needs one initial call, select `InitialPolicy::Run`.
`InitialPolicy::Skip` limits that registration to later unequal transaction endpoints.
If one transaction changes a value and restores it, `on_change` sees no change.
Configuration refresh does not replay initial work.

Use `observe` or `observe_distinct` to deliver current values to an external
consumer, such as a status display. Slow consumers can miss intermediate values.
Neither observation nor a reaction is an audit log of every assignment.
Use TestStore transaction assertions for exhaustive tests. Use a domain-owned
event log when the application must retain every business event.

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

## Choose input closure, removal or owned extraction

Close an input gate to stop ordinary new commands while retaining the child,
its state, tasks and fold contribution. Remove the child from parent state when
its lifetime must end. Neither operation proves that an external service stopped.

For removal that must return the exact owned child, declare a removable child
and retain its `RemovalSite`. The surviving parent's `TaskContext::remove_child`
also requires the captured child context. This prevents a delayed removal from
selecting a replacement merely because it has the same key.

The `before` callback borrows the still-owned child. After retirement, the `after`
callback receives `RemovedChild` and the surviving parent context.
Use `into_inner` to retain plain owned data. This discards the special transfer
authority, not the data. Use `into_output` inside that callback to declare an
owned outcome through an eligible parent-sourced output route.
The removed child's old context cannot send that outcome instead.

The `RemovedChild` capability cannot cross an await or escape the synchronous
callback. `into_output` declares follow-up work, not successful delivery.
Its delivery callback and the provider's cleanup still need their own handling.
Checked stack removal through `stack_removable` accepts only the last entry,
unlike the suffix removal of `StackAction::PopFrom`.

The [owned removal tests](../rca-core/tests/removed_child.rs) exercise exact payload
ownership, stale targets, parent-owned output and rejected transfer. The
[Local tests](../rca-core/tests/local_removed_child.rs) cover non-`Send` values.

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
