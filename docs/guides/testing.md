# Testing an RCA application

Use `TestStore` for application behaviour and a real host for provider integration.
`TestStore` runs the production Store with a deterministic scheduler. It pauses
incoming work at transaction boundaries so the test can assert each result.
A transaction is one actor operation that can change state or installed children.

Enable the facade's `test-util` feature, or depend on `rca-test` directly.
Both `Reducer` and direct `Feature` implementations use the same constructors.
Owner-local tests use `rca_test::local::TestStore` from `rca-test` with `local`.
The facade's `rca::local` namespace contains core APIs, not these test tools.
The Local choice changes transfer requirements, not assertion rules.

## Choose the state history

If state has an independent `Clone`, `Debug` and `PartialEq`, start with
`TestStore::new` or `TestStore::builder`. Each assertion compares the complete state.
Cloning an `Arc<Mutex<_>>` or `Rc<RefCell<_>>` does not make independent history:
the clone still observes later changes through the same mutable object.

| Requirement | Choice | Limit |
| --- | --- | --- |
| Compare the complete independently cloneable state | `new` or `builder` | `Clone` and equality must cover the intended state |
| State owns a file, task or another move-only value | `projected` or `projected_builder` | Only the selected owned snapshot is compared |
| Several tests need the same owned history shape | `Snapshot` or its optional derive | The caller still supplies `Snapshot::snapshot` to the projected constructor |
| Check an additional private or shared value at every assertion boundary | Builder `with_capture` and a `CaptureKey` | Captures cover declared values at selected boundaries, not every external write |
| Hide secrets in mismatch messages without ignoring their changes | `with_snapshot_renderer` or snapshot redaction | Rendering changes do not change equality |

A projected snapshot needs `Clone`, `Debug` and `PartialEq`, but the source state
does not. Snapshot values and their clones must remain independent of later
source mutations. Immutable shared values are acceptable when later edits use
copy-on-write rather than shared mutation.

The [snapshot guide](../snapshot-derive.md) explains capture, omission and
redaction. The [owned-snapshot tests](../../crates/rca-test/tests/owned_snapshots.rs)
show projected history and separate capture expectations. The
[move-only test](../../crates/rca-test/tests/test_store.rs) shows a scalar projection.
An omitted field is not tested automatically. Resource release and external
effects need their own assertions. Snapshot coverage is not a code-coverage metric.

## Send inputs and receive actual work

`send` supplies a new external action. `receive` accepts an action that the
application's effect already produced. Sending a fabricated response instead
does not test whether the original request produced or routed that response.

Plain `send` and `receive` assert that the selected snapshot stays unchanged.
If the expected result changes it, use `send_with` or `receive_with`. Their
closure edits an independent expected snapshot, not production state. Fields
that the closure leaves unchanged must also remain unchanged in production.

The [basic TestStore tests](../../crates/rca-test/tests/test_store.rs) define `Feature`,
`State` and `Action` for this sequence:

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

The extract belongs inside an async test body that returns `Result`.
The linked tests include the complete types and executor.
An action matcher borrows the real action. It need not clone the action or
require the entire action enum to implement equality.

Typed Feature work has its own assertion boundaries:

| Pending work | Assertion method |
| --- | --- |
| Root action from an effect | `receive` or `receive_with` |
| State update from a task | `receive_update` |
| Child installation or another structural transaction | `receive_structural` |
| Action sent through an acquired child context | `receive_mounted` |
| Input through a declared command port | `receive_command` |
| Value through a declared output | `receive_output` |

Choose the receive method by what the work **did**, not by the final state.
For example, a task that updates state directly does not send a response action.
Use `receive_update` for that update, even if the new state equals the old state.
If declared capture values also change, use the method's `*_with_captures` variant.

If a pending task update is cancelled before it runs, there is no state change to receive.
The [cancellation tests](../../crates/rca-test/tests/terminal_frontier.rs) check the reason instead.
This extract starts after the task submits its update:

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

`pending_boundary` identifies one pending operation in this TestStore. The token neither keeps the operation alive nor acknowledges it.
`receive_terminal` checks the actual error for that operation. A token from another Store cannot match.
For a rejected output, use `receive_output_terminal` to also match its declaration. See the [method contracts](../../crates/rca-test/src/store/api.rs).

Mount declarations do not construct children during `build`.
`receive_structural` lets the startup transaction run, including its initial hooks.
A final-value comparison alone cannot tell you whether that transaction took place.

## Choose exhaustive or focused assertions

Keep the default exhaustive mode for feature behaviour. Receive each pending
operation before the next input. This makes unexpected work a test failure.

For a test about one outcome within a larger scenario, choose explicitly:

| Intent | Choice | What happens to earlier work |
| --- | --- | --- |
| Assert every transition | Ordinary receive methods | Each transition needs an expected result |
| Reach one root action while preserving earlier production behaviour | `receive_until` | Earlier transactions run, and their snapshots become the new baseline |
| Execute one transition without asserting its changes | `receive_omitting` | The transaction runs and its snapshot becomes the new baseline |
| Deliberately prevent a pending mutation from running | `skip_received` with `Exhaustivity::NonExhaustive` | The pending command is cancelled before user code runs |

Focused receive methods work without changing the store's exhaustivity mode.
Use `OmissionReporting::Warn` to retain omission warnings for `take_omissions`.
If those warnings add no value to that test, use `Silent`. Neither choice
asserts the omitted changes. `skip_received` is not equivalent to omission,
and it cannot skip structural installation.

Focused methods retain production errors, earlier failed assertions and final
completion requirements. They do not wait for real I/O or advance virtual time.
The [focused receive tests](../../crates/rca-test/tests/focused_receiving.rs) and
[focused output tests](../../crates/rca-test/tests/focused_outputs.rs) cover these differences.

## Make dependencies and progress explicit

Inject controlled clients with `TestStore::builder(...).with_dependency::<Key>(value)`.
Use `with_store_dependency` instead if a feature or action should still be able to
override the client. See [which dependency value the feature receives](dependencies.md#which-dependency-value-does-the-feature-receive)
for lookup order and defaults.

If a client must share this TestStore's clock, use `with_clock_dependency`.
The builder constructs the client after it creates the deterministic runtime.
Advancing this clock does not advance an unrelated Tokio timer or wall clock.
The [client-clock tests](../../crates/rca-test/tests/client_clock.rs) include a wrong-clock control.

If an action already waits for an assertion, receive it before calling `advance`.
Otherwise, `advance` can reach its target time while a newly due action still waits for `receive`.
The assertion pauses the action, not the clock.
In the [virtual-time test](../../crates/rca-test/tests/test_store.rs), a response is due at one second.
After a single ten-second advance, the clock reads ten seconds before the test receives the response.
If an intermediate clock reading matters, advance to that deadline first.
Release controlled request or stream endpoints explicitly before expecting
their results. Do not use wall-clock sleeps to make a deterministic test pass.

The [external-client recipe](external-clients.md) shows requests, streaming,
denial, cancellation and bounded queues without real network access.
Its controlled success does not prove that a real provider follows the same
contract. Provider tests need the actual runtime, service and cleanup protocol.

A poll budget limits how many times the scheduler polls ready tasks, not how
long the test waits. If a request waits on a test client, complete the request
through that client. More polls cannot supply the missing result. Increase the
budget only for work you know will finish.

## Finish the scenario, not just the last action

A returned `TestActionTask` covers the work caused by its transaction.
Its `finish` does not release pending assertions or advance virtual time.
Receive its updates and responses before requiring that task to finish.

End the test with `TestStore::finish`. It checks pending assertions, actions,
tasks, timers, observations and reported issues before Store shutdown.
It polls ready work within the budget but does not wait for external progress.
Normal drop also checks completion, but explicit `finish` gives a useful
failure location. Catching a failed assertion does not erase the failure.

For long-lived work, exercise the application's cancellation or close path.
Then assert the resulting work and release owned observations. Finish cannot
silently turn an unfinished scenario into success. External producer cleanup
remains separate from RCA completion, as described in
[receipt lifetimes](../receipt-lifetimes.md).

Useful boundary cases include a cancelled request, same-ID child replacement,
a failed client, a retained old callback and final resource release. Those cases
answer different questions from a successful final-value assertion.
