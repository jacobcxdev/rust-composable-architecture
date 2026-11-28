# Install and use child features

An installed child can receive a command without making its parent handle that command.
Imagine a pool of upload workers. Each worker reports progress while its upload runs.
If the pool replaces a worker, a late reply from the old upload must not change its replacement.

Start with [reducer composition](composition.md#choose-the-smallest-composition-model). It already handles child lifetimes and cancellation.
If workers need private commands, install them as children.
Installation also gives each worker its own feature instance and a way to contribute to a pool total.
Both approaches share the parent Store. Neither creates a second Store or transfers ownership of the child's state.

If this is your first feature, start with [writing a feature](authoring.md).
For a working example without a network service or user interface, [run the uploader](../../examples/composition-search/docs/uploader.md).

## Terms used in this guide

- A **feature** defines how to change state and which work to start. Every `Reducer` also implements `Feature`.
- A **feature instance** holds the behaviour for one worker. It can own private values, such as a cache. Those values are separate from the state that TestStore compares.
- A **transaction** is one unit of work that the Store processes before it starts the next. It can change several values together. During a transaction, the child borrows its part of the parent's state.
- A **mount** describes how the Store finds a child's state, constructs the instance, and connects it to its parent. Each installation starts one lifetime, also called an *incarnation*.
- A **mount site** identifies the declaration. Keeping the site does not keep the child or its callbacks alive.
- A **context** gives a task access to a particular installation. The Store can end that access without changing the worker's business ID.

`Reducer::reduce` returns effects. A direct implementation of `Feature::update` can also declare tasks with typed access to state.
Both forms work with `Store::new`, `Store::builder`, and their `TestStore` equivalents.
An installed child accepts either form. Static composition with `Scope` or `for_each` requires a reducer.
Its state and selector functions can borrow local values during the call.

For shared Stores, import types such as `Feature`, `EntryMount`, and `Fold` from `rca`.
An integration crate that does not select a runtime can import them from `rca_core`.
The `local` module contains the alternatives for a single owner thread. The same policy and identity types remain at the crate root.
See [supported import paths](../support.md#public-api-paths).

## Install once, then acquire the installed instance

For one worker that can be absent, use `OptionalMount`. This is the path to a context for that optional worker:

1. Describe how to select the worker's state.
2. Supply a constructor for its feature. A child can declare its own children, outputs, and contributions.
3. Call `mount_with_site` on the builder. Keep the optional child's site along with the returned builder.
4. Build the Store. Wait for `store.ready().await` before you request a child.
5. Use `store.acquire(&site).await` to request the context. If the site is live but empty, the result is `Ok(None)`.
6. If the result contains a context, give it to the task that needs it. A stopped Store or an expired declaration returns an error.

The context remembers *which* worker accepted the task. If that worker leaves, its old context cannot update a replacement.
Acquire a fresh context for a new operation. Do not acquire one to deliver a late reply from the previous operation.
[`Store::scope`](composition.md#scoped-store-handles) gives a view into state for the lifetime of the root. It does not identify an installed child.

For an optional child, `MountIdentity` describes three kinds of change:

| Change to the worker | What to change | What the Store does |
| --- | --- | --- |
| Put a different worker in this slot | Change `key` | Ends the previous installation. |
| Restart the worker under the same ID | Change `replacement` | Constructs a new instance. The old context cannot reach it. |
| Change policy but keep its private cache | Change `configuration` | Keeps the instance but rejects its old contexts. New tasks need a fresh context. |

Leave these markers unchanged for ordinary progress updates.
Suppose one callback removes an optional child and restores an equal value before it returns.
The Store does not see two transactions. If the restored child needs a new lifetime, change its replacement marker.

For a collection of workers with feature-owned tasks, use the builder's `keyed` method.
Return an `EntryMount` for each entry. Each worker's task receives its own context.
If an external caller needs a specific worker, use `keyed_with_children`.
Keep `children.site().clone()` from that worker's setup. At the start of the operation, acquire that site.
Deliver the eventual reply through the original context. The [worker pool test](../../crates/rca-core/tests/fold_keyed.rs) shows how to retain sites.

The [keyed collection](composition.md#ordered-keyed-state) tracks each entry's lifetime. Reordering the entries preserves that lifetime.
To replace a worker under the same key, remove the old entry and insert the new one.
Editing a field named `replacement` in the worker's state does not start a new lifetime.
Use `EntryMount::configuration` to refresh its contexts without replacing the feature.

For work that starts with the root installation, use `RootMount`.
If saved state already contains workers, the Store constructs those features before it calls the startup callbacks.
It calls child startup callbacks before their parent's callback. A returned task starts only after the runtime polls it.
Restoring state does not resume the old upload.

The [startup test](../../crates/rca-core/tests/mount_acquisition/initial_order.rs) shows the order.
The [worker pool test](../../crates/rca-core/tests/fold_keyed.rs) shows the effect of replacement and reordering.
The [composition tests](../../crates/rca-test/tests/legacy_feature_compatibility.rs) compare installed children with scoped reducers.

## Choose the route that matches the work

| What does the worker need? | Use | What it means |
| --- | --- | --- |
| Deliver a child action, such as an upload result | `TaskContext::send` | The installed child handles its own action. No parent action case is necessary. |
| Read a current limit or report progress | `TaskContext::read` or `update` | The Store orders each call. A task never keeps a borrow of the state while it waits. |
| Let a producer send one type of command | `CommandPort<C>` | The producer cannot read the worker's state or write to it directly. |
| Give a result to a parent or another ancestor | `Output<O>` | Only the declared route receives the owned value. RCA does not broadcast it. |
| Find the capacity of the workers still present | `Fold<R, K>` | A fold combines contributions in committed order. It does not send an action. |
| Wait for the current set of named tasks | `Operation::capture` | Its `OperationWait` excludes later generations. Capture again to wait on the new set. |

Use a parent action for a decision the parent owns. Both shared and owner-thread Stores can send private inputs directly to an installed child.
A private input does not run ancestor action handlers. An explicitly integrated action route does.
The [nested plugin example](../../crates/rca-core/tests/depth_five_plugins.rs) sends 10,000 private commands without routing them through parent actions.
See [recursive jobs](recursive-jobs.md) to compare nested ownership with a flat list.

The [uploader example](../../examples/composition-search/docs/uploader.md) uses one task to fetch, process, and upload a file.
Progress changes state. A final outcome reports the result. The example does not need an action for each internal step.

## Separate captured inputs from current State

A task keeps the inputs and services it takes at startup. If the application changes clients during an upload, that upload keeps its original client.
If a task needs the latest policy, use `context.read` after the wait. This reads current state, not a new set of dependencies.

Rust evaluates the initial state argument before it calls a Store constructor.
A dependency override inside that constructor cannot change a value the application already created.
The [provider example](../../crates/rca-core/tests/feature_provider_handover.rs) uses a function to construct state inside the intended dependency scope.
The [session example](../../crates/rca-test/tests/session_client_refresh.rs) keeps each request with the client that started it.

## Choose a reaction, an observation or an explicit update

For a change directly caused by an input, update state in the current reducer or feature callback.
For a later change from an upload task, use `TaskContext::update`. The Store processes both changes as transactions.

A **reaction** starts work in response to the selected state. For example, several commands can change a worker's endpoint. Each change requires a new connection.
`on_change` compares the selected value before and after a transaction.
If a transaction changes the endpoint and changes it back, there is no difference to report.

Select an independent value, such as an owned address. A clone of a shared, mutable handle does not preserve the earlier address.

Use `on_initial` for work that starts at installation. A configuration change does not repeat initial work.
For an initial call to an `on_change` handler, select `InitialPolicy::Run`.
With `InitialPolicy::Skip`, the handler sees only later changes.
`ReactionChange` distinguishes the initial, owned value from the previous and current values for a change.
The [reaction tests](../../crates/rca-core/tests/changed_reactions.rs) show which values the callbacks receive, and in what order.

An **observation** delivers current values to an external reader, such as a status display.
Use `observe` or `observe_distinct` for this purpose. A slow reader can miss intermediate values.
Neither observations nor reactions record every assignment.
For exhaustive tests, assert transactions with TestStore. If the application must retain every business event, keep an explicit event log.

## Keep input admission, outcomes and reads separate

A command has two distinct milestones. **Admission** means that the queue accepted it. **Commit** means that the Store processed it.

`CommandPort::try_send` returns a receipt at admission. The receipt supplies the later commit result.
`send` waits for both steps. Neither result means that the upload finished.
The [command tests](../../crates/rca-core/tests/command_port.rs) show how to recover an unsent value and detect a replaced child.

An `InputGate` controls whether a worker accepts ordinary commands.
Closing it leaves the worker, its tasks, and its contribution to a fold in place.
Only the captured direct parent's `FeatureContext` can change the gate. A control port can bypass the gate.
It cannot let a child reopen its own gate. Ordinary follow-up work does not inherit the bypass.

An output identifies its source and the ancestors that can handle it. The declaration owner controls how long the handlers live.
Each handler can pass the owned value to the next declared handler. There is no fallback root action or right to change another Store.
The [authorisation example](../../examples/composition-search/docs/authorisation.md) passes a way to reply as part of the output.
Before it updates state, the requester uses its original context to make sure that its installation still exists.

### Choose a query or a maintained aggregate

For an occasional total, call `Store::fold`. For example, a scheduler can ask how many upload slots remain.
The Store computes the answer at the point where it processes the read. It does not store the result or reserve a slot.
If a worker leaves or another replaces it, the old installation no longer contributes.
Closing a worker's input gate alone leaves its contribution intact.

`FoldKey` supplies a starting value and a rule for combining each contribution.
A fold visits parents before their children, in the committed child order.
A transform combines one subtree as a group. A `ControlFlow::Break` stops that group, but its transform still runs.
The combination does not need to give the same result in a different order or grouping.
The [fold examples](../../crates/rca-core/tests/fold.rs) and [nested examples](../../crates/rca-core/tests/fold_shape.rs) show the order.

In the [capacity example](../../crates/rca-core/tests/fold_input_gate.rs), two workers with three and two slots report five together.
As their capacities change, the total falls to three and then zero. Both workers remain installed.

To query only one pool, use `store.acquire_fold(&capacity, &pool_site).await?`. If the site is live but the pool is absent, it returns `None`.
With `Some(view)`, `view.read().await?` includes that pool and its descendants, not its siblings.
Reordering preserves the view. Replacement or a configuration change invalidates it, even with the same pool key.
The [keyed fold example](../../crates/rca-core/tests/fold_keyed.rs) shows how to acquire another view.

If readers need updates whenever the total changes, maintain the total in parent state instead.
Update it on installation, replacement, removal, and changes to capacity. Use explicit updates, outputs, or reactions to do that.
Readers can then observe the total. An observation does not subscribe to a fold.

## Choose input closure, removal or owned extraction

If the worker must remain in the pool but accept no new jobs, close its input gate.
If its lifetime must end, remove the worker from parent state.
Neither action proves that an external upload stopped.

If the parent needs the value it removed, declare a removable child. Retain the declaration's `RemovalSite`.
The parent's `TaskContext::remove_child` also requires the context for that particular child.
A delayed request cannot remove a replacement with the same key.

The `before` callback can borrow the child before removal. After removal, the `after` callback receives a `RemovedChild` and the surviving parent's context.

- To keep the owned data, use `into_inner`. This gives up the special permission to transfer it through an output.
- To declare an output from the surviving parent, use `into_output` inside the callback. The removed child's context cannot send this result.
- Keep `RemovedChild` inside the synchronous callback. It cannot survive an `.await`. Declaring the output does not prove delivery or external cleanup.

For a stack, `stack_removable` can extract only the last entry. In contrast, `StackAction::PopFrom` removes the specified entry and everything after it.
The [removal examples](../../crates/rca-core/tests/removed_child.rs) cover ownership, stale targets, and rejected transfers.
The [owner-thread examples](../../crates/rca-core/tests/local_removed_child.rs) cover values that cannot cross threads.

## Keep thread-affine work on its owner

Some workers hold values that Rust cannot send to another thread, such as `Rc`.
Enable the `local` feature to use `rca::local` for their state, actions, and tasks.
The runtime runs and drops each task on its owner thread. Another thread can wake the owner, but cannot take the task's values.
The application supplies the event loop.

Use `local::Effect` to describe work for a local reducer or feature. It supports tasks that can fail, timers, and typed cancellation.
It can map actions, combine effects, or run effects in sequence.
`send_at` uses an absolute deadline on the runtime's clock. `delay` defers the effect itself, not just its final action.
Names for operations and cancellation keys still need portable identities.

`without_inherited_cancellation` removes inherited cancellation keys. The root task, installation, and Store still own the work.
`try_run_reported` retains the original error type and the result of reporting it. Neither operation detaches work from its owner.
Shared and local effects have different types. Share synchronous decision logic instead of converting one effect into the other.

Local `observe` returns owned values without a `Clone` bound. Local `observe_distinct` adds `Clone + PartialEq` to suppress equal values.
Both evaluate their selectors on the owner thread. Dropping the observation ends its registration, not the Store or its accepted work.

The [local worker example](../../crates/rca-core/tests/local_worker_replacement.rs) keeps an `Rc` across a wait and rejects stale updates after replacement.
The [local mount examples](../../crates/rca-core/tests/local_mount.rs) use private actions and nested children.
If the worker needs only a synchronous reducer call, [local composition](local-composition.md) needs no asynchronous host.

## Assert transactions and finish external cleanup explicitly

TestStore checks state changes and actions from tasks, not deliveries to an observer.
For a history of progress, take an independent snapshot. A second handle to the same `Rc<Cell<_>>` sees later writes too.
If application code writes through a second `Rc<RefCell<_>>` handle, it bypasses the Store.
Such writes do not automatically notify readers, trigger reactions, or produce TestStore assertions.
The [importer example](../../examples/composition-search/docs/owned-importer.md) distinguishes domain state, private values, and shared values.

An independently acquired context can match the original child action in `receive_mounted`.
If the context names another source, the assertion does not consume the pending action or record an incomplete assertion.
If the source matches but the expected action does not, the action stays pending and TestStore records the failed assertion.
Matching the action later permits cleanup. It does not make the earlier assertion pass.
The [mounted action tests](../../crates/rca-test/tests/mounted_frontier.rs) show both shared and owner-thread behaviour.

When the Store retires a child, it stops accepting that child's work. It retires descendant declarations before their parent and constructs replacements afterwards.
Keeping a mount site does not keep a declaration alive.
An expired site rejects a request for a context without taking up queue space.
If the queue already accepted the request, the Store still checks the original lifetime. The request cannot switch to the replacement.

Cancellation requests a stop. It does not prove that a device or service finished cleanup.
Likewise, a [wait for a state condition](../../crates/rca-core/tests/predicate_wait.rs) is not a wait for the task that caused it.
For external work, wait for the host's own completion signal.
See [receipt lifetimes](receipt-lifetimes.md), [replacing a document session](../../crates/rca-persistence/tests/shared_document_session_replacement.rs), and [host responsibilities](integration-boundaries.md).
For tasks that start only after input settles, see [debounce and throttle](debounce-throttle.md).
