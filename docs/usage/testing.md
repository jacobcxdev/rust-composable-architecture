# Test an RCA application

`TestStore` lets a test control each step of a feature that uses the real Store.
The test supplies an input, states what must change, and receives any result that the feature produces.
If these terms are new, read the [counter example and basic model](authoring.md#the-basic-model).

Enable the `test-util` feature on `rca`, or add `rca-test` as a test dependency.
For state that must stay on one thread, enable `local` on `rca-test` and import `rca_test::local::TestStore`.
The `rca::local` module does not contain this test tool. Both forms use the same assertion rules.
The [local Store test](../../crates/rca-test/tests/local_test_store.rs) shows a task that keeps an `Rc` on its owner thread.

## Send inputs and receive actual work

Imagine a request with three states: idle, started, and responded.
Sending `Action::Start` starts the request. A successful effect sends `Action::Response` back to the Store.
**Send** an input from the outside. **Receive** a result from the feature.
If the test sends the response itself, a missing or wrongly routed effect can go undetected.

The [basic TestStore tests](../../crates/rca-test/tests/test_store.rs) define the state, actions, and reducer.
This extract belongs inside an asynchronous test that returns `Result`:

```rust
let mut store = TestStore::new(State::Idle, Feature);
let _start = store
    .send_with(Action::Start, |state| *state = State::Started)
    .await?;
let _response = store
    .receive_with(
        |action| *action == Action::Response,
        |state| *state = State::Responded,
    )
    .await?;
store.finish().await?;
```

`send_with` and `receive_with` change the **expected** state, independently of the real state.
Each field that you leave alone must stay unchanged in the real state.
Plain `send` and `receive` expect no change to the state under comparison.
The action matcher borrows the actual action. It does not require a clone or equality for the whole action type.

A **transaction** is one Store operation, such as a state update or the installation of a child.
The test controls which pending transactions can run. For a typed `Feature`, choose the assertion by the work it does:

| Work from the feature | Assertion |
| --- | --- |
| An effect sends an action to the root Store | `receive` or `receive_with` |
| A task changes state directly | `receive_update` |
| The Store installs children or processes another structural change | `receive_structural` |
| An installed child's context sends an action | `receive_mounted` |
| A declared command port receives input | `receive_command` |
| A declared output delivers a value | `receive_output` |

For example, a direct task update needs `receive_update` even if the final state equals the old state.
If expected values outside state also change, use the method's `*_with_captures` variant.
For [installed children](structural-features.md), `build` declares the children but does not construct them.
`receive_structural` lets the startup transaction run, including the initial hooks.

### Receive a cancellation instead of a state change

If the test cancels a task before its pending update runs, there is no new state to receive.
Save the pending operation's token first. The [cancellation test](../../crates/rca-test/tests/terminal_frontier.rs) then checks the actual result:

```rust
let boundary = store.pending_boundary().expect("the update is pending");
task.cancel().await?;
task.finish().await?;
store
    .receive_terminal(boundary, |error| {
        error.reason() == rca::TaskAccessReason::Cancelled
            && error.progress() == rca::TaskProgress::NotStarted
    })
    .await?;
```

The token identifies one pending operation in this TestStore. It neither keeps the operation alive nor acknowledges it.
A token from another Store cannot match. For a rejected output, `receive_output_terminal` also matches the declared output.
See the [test method contracts](../../crates/rca-test/src/store/api.rs).

## Choose the state history

The default TestStore keeps a copy of state at each assertion. It needs `Clone`, `Debug`, and `PartialEq`.
A clone must preserve the earlier value even after the application changes its state.
Cloning an `Arc<Mutex<_>>` or `Rc<RefCell<_>>` copies a handle to the **same** mutable value, not its history.

| What must the test compare? | Use | What this checks |
| --- | --- | --- |
| Complete state that you can copy independently | `TestStore::new` or `builder` | All state covered by its clone and equality operations |
| Selected values, perhaps from state that owns a file | `projected` or `projected_builder` | Only the owned record your function returns |
| The same record in several tests | `Snapshot::snapshot` with a projected constructor | Fields that your [snapshot](snapshot-derive.md) captures |
| Private or shared values outside that record | `with_capture` and a `CaptureKey` | Declared values at assertion boundaries, not every outside write |
| Changes to a secret without printing the secret | `with_snapshot_renderer` or snapshot redaction | Equality still includes the value |

A projected record needs `Clone`, `Debug`, and `PartialEq`. The original state does not.
Sharing immutable data is safe for this purpose if later edits make an independent copy.
Changes through a shared mutable handle can invalidate both the original record and its clones.

The [owned-snapshot tests](../../crates/rca-test/tests/owned_snapshots.rs) compare selected fields and separate captures.
The [move-only test](../../crates/rca-test/tests/test_store.rs) records a number without cloning the resource that owns it.
If the test omits a resource, assert its release separately. Comparing state does not check code coverage or an external side effect.

## Choose exhaustive or focused assertions

By default, receive each pending operation before the next input. Unexpected work fails the test.
For a test about one result inside a larger scenario, choose what happens to earlier work:

| Intent | Choice | Effect |
| --- | --- | --- |
| Assert every transition | Ordinary receive methods | Each transition needs an expected result. |
| Find a specific root action | `receive_until` | Earlier transactions run. Their state becomes the next starting point. |
| Run one transaction without asserting its changes | `receive_omitting` | It runs and supplies the next starting point. |
| Stop a pending mutation instead of running it | `skip_received` with `Exhaustivity::NonExhaustive` | The pending command never runs user code. This cannot skip child installation. |

`receive_until` and `receive_omitting` work in exhaustive mode.
`OmissionReporting::Warn` records the omitted assertions for `take_omissions`. `Silent` does not record a warning.
Neither option asserts the omitted changes. Previous failed assertions, runtime errors, and completion requirements still apply.
Focused methods do not advance the test clock or wait for an external reply.
See [focused actions](../../crates/rca-test/tests/focused_receiving.rs) and [focused outputs](../../crates/rca-test/tests/focused_outputs.rs).

## Make dependencies and progress explicit

In this test, a **controlled client** stands in for a service. The test decides when the client returns a response.
Supply it with `TestStore::builder(...).with_dependency::<Key>(value)`.
If a feature or action must still be able to override that client, use `with_store_dependency` instead.
See the [dependency lookup order](dependencies.md#which-dependency-value-does-the-feature-receive).

The test clock starts from a known time. It moves when the test calls `advance`, not when time passes on the host.
If a shared client uses a deadline, supply the test clock with `with_clock_dependency`.
The builder creates the client after the runtime exists. An unrelated Tokio timer will not advance.
The [client-clock tests](../../crates/rca-test/tests/client_clock.rs) demonstrate the difference.
For a local Store, keep its clock on the owner thread. The [local builder example](../../crates/rca-test/src/store/local.rs) shows how.

If an action already waits for an assertion, receive it before `advance`.
An action that becomes due **during** an advance can still await its assertion after the clock reaches the requested time.
In the [virtual-time test](../../crates/rca-test/tests/test_store.rs), a response becomes due at one second.
After a single ten-second advance, the clock reads ten seconds before the test receives the response.
If the time at one second matters, advance to that deadline first.

A **poll** gives a task a chance to run. A poll budget limits this count, not elapsed time.
If the request needs an external response, release that response through the test client.
More polls or a real sleep cannot produce it. Only increase the budget for work that you know will finish.

The [external-client example](external-clients.md) covers requests, streams, refusal, cancellation, and a full queue.
To check a real service, run a separate integration test with that service, its runtime, and its cleanup procedure.

## Finish the scenario, not just the last action

A returned `TestActionTask` covers work started by its transaction and its descendants.
Its `finish` does not acknowledge pending assertions or advance the clock. Receive the responses and updates before waiting for the task.

End the test with `TestStore::finish`. It checks outstanding assertions, actions, tasks, timers, observations, and reported issues before shutdown.
It runs ready work up to the poll budget, but does not wait for external progress.
Dropping the TestStore normally also checks completion. An explicit `finish` identifies the failing line.
Catching a failed assertion does not erase it.

For long-lived work, exercise your application's cancellation or close operation. Assert the outcome. Release any observations you own.
A [finished RCA task](receipt-lifetimes.md) does not prove that an external service released its resources.
