# Find out what changed in your app

A diagnostic tells you what RCA observed. It does not make an operation succeed.
For example, a log entry can tell you that the Store cancelled an upload. It cannot tell you that the remote server deleted the file.

## Which tool answers this question?

| Question | Tool | What it tells you |
| --- | --- | --- |
| What changed after one action? | `ReducerExt::trace_changes` | Compare selected values before and after the reducer processes the action. |
| What did a synchronous operation change or return? | `diagnostics::checkpoint` | Compare snapshots and report whether the operation returned success or an error. |
| Did the Store replace a child or apply a cancellation request? | `StoreBuilder::with_lifecycle_diagnostics` | Receive an event about that change, in debug and release builds. |
| How can a service report a recoverable problem? | `IssueReporter` or `IssueReporters` | Deliver a report without changing the result of the operation. |
| What is the latest value of part of the state? | `Store::observe` | Receive updates. A slow observer can miss intermediate values. |
| Did each expected state change occur? | `TestStore` | Assert changes to the selected state and any external values the test captures. |
| Which values or links changed in a graph? | `CapturedGraph` and its formatting tools | Compare the values and connections that the application includes. |

For change traces, checkpoints, lifecycle events, or graph diagnostics, enable the `diagnostics` Cargo feature on `rca`.
Issue reporting and state observation need no extra feature.
For `TestStore`, enable `test-util` on `rca` or depend directly on `rca-test`.
None of these tools installs a global logger.

## Trace a reducer or inspect one operation

A reducer processes one action and can return work for the Store to run.
`trace_changes` wraps a `Reducer`. It does not wrap a type that implements only `Feature`.
With Rust's debug assertions on, the wrapper takes snapshots before and after the reducer runs.
It compares the values, not the text that represents them. Its callback runs before the Store acknowledges the change and before the returned effect runs.
The [trace examples](../../crates/rca-core/src/diagnostics.rs) show ordinary output and how to hide sensitive fields.

A snapshot must preserve the earlier value. For example, cloning an `Arc<Mutex<_>>` or a `Shared<T>` handle still points to the same data.
Take an independent [snapshot](testing.md) instead.

For a synchronous operation with a `Result`, use a [checkpoint](../../crates/rca-core/src/diagnostics/checkpoint.rs).
Its callback reports two facts separately: whether the snapshot changed, and whether the operation returned an error.
If the callbacks return normally, the caller receives the original result.

Without debug assertions, the reducer and the checkpoint operation still run. Neither tool takes snapshots or calls its diagnostic callback.
Values captured by a callback still occupy storage. Rust still runs their destructors.
Keep required checks in the operation or its tests, not in a debug callback.

A panic in a diagnostic callback propagates to the caller. It does not undo an earlier change.
For example, a panic after a reducer changes state can prevent the Store from receiving its effect.
The [trace tests](../../crates/rca-core/tests/diagnostics.rs) and [checkpoint tests](../../crates/rca-core/tests/diagnostics_checkpoint.rs) distinguish these cases.

## Keep diagnostics separate from success and cleanup

A lifecycle event describes a change to a child or an applied cancellation request.
It does not contain live state or grant access to the child. Keeping the event does not keep the child or its tasks alive.
Wait for a [receipt or a cleanup signal](receipt-lifetimes.md) to establish that work ended.

The Store calls lifecycle callbacks directly, outside its locks.
Do not wait inside a callback for work that only the same Store can complete.
During a panic or forced destruction of a Store that the host no longer runs, RCA cannot guarantee delivery of all events.
The [lifecycle tests](../../crates/rca-core/tests/lifecycle_diagnostics.rs) show that removing a child does not prove that all its work ended.

To test a task on a direct `Feature`, assert changes at each update that the Store commits.
If you need a permanent record in production, arrange for the application to write it.
Neither an observation stream nor a debug trace provides a complete history.

## Is an issue the same as an error?

No. An `Issue` describes a condition for a reporting tool.
Even `IssueSeverity::Error` leaves the operation's return value unchanged. Return a typed `Result` for a failure the caller must handle.

By default, a production Store does not deliver issues. Use `with_issue_handler` on its builder to receive them.
An asynchronous task can retain a reporter without keeping the Store alive.
If the reporter is disabled or closed, its error can return the original issue.

To report to several recipients, pass an `IssueReporters` value to the client.
Its `with_scope` method passes the selection to a callback. It does not change a global setting.
Passing `None` keeps the current selection. Supplying an empty selection disables delivery.
For work that must keep this choice after the callback returns, clone the selected reporters.

In a test, `TestStore` records issues. Call `expect_issue` for a report the feature must send.
Then finish the test. Printed output does not satisfy this assertion.
The [issue tests](../../crates/rca-test/tests/issues.rs) show how to assert a report from a reducer or a task.

## Redaction must not weaken equality

Redaction hides selected details in the displayed text. Two different values can produce the same redacted text.
Compare the values themselves in tests.

`CapturedGraph` records only the values and connections that the caller selects.
It can represent two links to the same value, or a link back to an earlier node.
It does not inspect private fields automatically. An omitted field requires a separate assertion.
See [owned resources](owned-resources.md) for an example of the difference between a stored handle and an independent snapshot.

RCA limits how much of a graph it visits, how deeply it follows links, and how much text it produces.
Those limits do not bound work inside your own callbacks. A shortened display cannot prove that two graphs are equal.
The [structural diagnostics tests](../../crates/rca-core/tests/structural_diagnostics.rs) cover output limits, redaction, and comparison.
