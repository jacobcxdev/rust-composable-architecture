# Diagnostics, issues and state history

Choose the tool for the fact you need to observe. Diagnostic output does not
replace a typed result, a transaction assertion or a cleanup acknowledgement.

## Which tool answers this question?

| Question | Tool | Boundary |
| --- | --- | --- |
| What did this reducer change? | `ReducerExt::trace_changes` | Debug-only before/after snapshots around reduction |
| What did this synchronous operation change or return? | `diagnostics::checkpoint` | Debug-only snapshots and success/error metadata |
| Which child lifetime changed or cancellation request ran? | `StoreBuilder::with_lifecycle_diagnostics` | Lifecycle metadata in debug and release builds |
| What recoverable condition needs reporting? | `IssueReporter` or `IssueReporters` | Explicit delivery, independent of the operation's result |
| What is the latest selected state? | `Store::observe` | Values can coalesce, so this is not a complete history |
| Did each expected transaction occur? | `TestStore` | Exhaustive assertions for the selected state and declared captures |
| How can I inspect a recursive or aliased structure? | `CapturedGraph` and structural diagnostics | Caller-selected values, identities and limits |

The `diagnostics` Cargo feature enables the first, second, third and last rows.
Issue reporting and ordinary observations belong to the runtime-neutral core.
`TestStore` requires `test-util` through the facade, or a direct `rca-test`
dependency. None of these tools installs a global logger.

## Change tracing is not an application event log

`trace_changes` wraps a `Reducer`, not an arbitrary direct `Feature` implementation.
It captures selected state before and after reduction and compares semantic
equality. Its callback runs before Store commit acknowledgement and effect
execution. The [diagnostics examples](../../rca-core/src/diagnostics.rs) show
ordinary output and custom redaction.

Capture independent values. Cloning an `Arc<Mutex<_>>` or `Shared<T>` handle does
not preserve the earlier contents. Use the same independence rules as
[test snapshots](testing.md).

Without debug assertions, the traced reducer and checkpoint operation still run,
but their diagnostic capture and callbacks do not. Required assertions and
application side effects therefore belong in the operation, not the diagnostic
callback. Callback captures still occupy storage and have destructors in release
builds.

For direct Feature tasks, assertions belong at their real transaction boundaries.
Lifecycle callbacks describe child lifetimes and applied cancellation requests,
not individual task completion or arbitrary state differences. If the application
requires durable production history, use a host-owned logging or audit policy.
An observation can skip intermediate values, and a debug trace can disappear
in release builds.

## Keep diagnostics separate from success and cleanup

Lifecycle events contain metadata, not live State or permission to access a child.
Retaining an event does not retain the child or its tasks. A cancellation event
does not mean that a remote request rolled back or that its provider stopped.
Use the appropriate [receipt or cleanup wait](../receipt-lifetimes.md) for that
requirement.

Lifecycle callbacks execute synchronously outside framework locks. They must not
block on the same actor. Delivery is not guaranteed during unwinding or forced
destruction of an actor that the host no longer polls. The
[lifecycle tests](../../rca-core/tests/lifecycle_diagnostics.rs) distinguish
retirement events from retained work.

Diagnostic panics propagate. A panic after mutation does not roll state back and
can prevent the resulting effect from reaching the Store. Keep rendering and
reporting callbacks small and predictable. The
[trace tests](../../rca-core/tests/diagnostics.rs) and
[checkpoint tests](../../rca-core/tests/diagnostics_checkpoint.rs) cover the
different panic stages.

## Is an issue the same as an error?

No. An `Issue` records a condition for a diagnostic consumer. `IssueSeverity::Error`
does not change a successful return value into an error. Use a typed `Result`
for a failure that the caller must handle.

Issue delivery is disabled until a handler or recorder is installed. Production
Store builders can install `with_issue_handler`. An owned reporter can be
captured by asynchronous work without retaining Store ownership. Reporting can
return the original issue because the endpoint is disabled or closed.

If a client needs an explicit choice of several endpoints, use `IssueReporters`.
Its scoped selection is lexical, not ambient state. Passing `None` inherits the
current selection. Passing an empty selection explicitly disables delivery.
Clone the selected endpoints into work that must retain the choice.

TestStore records issues for assertions. Use `expect_issue` to consume the expected
issue, then finish the test. A printed message is not an assertion. The
[issue tests](../../rca-test/tests/issues.rs) show synchronous and asynchronous
reports, matching and shutdown boundaries.

## Redaction must not weaken equality

Custom renderers decide which labels and values appear in text. Redacted text can
look identical for two semantically different values. Tests must compare the
actual selected values, not their rendered strings.

`CapturedGraph` represents selected values and logical connections, including
aliases and cycles. It is not reflection and does not inspect arbitrary private
fields. Omitted fields remain untested unless another assertion covers them.
The [owned-resource guide](owned-resources.md) explains independent graph capture.

Structural output uses explicit work, depth and output limits. Those limits
bound library traversal and output, not arbitrary work inside application
callbacks. A truncated rendering does not prove complete equality or complete
history. The [structural diagnostics tests](../../rca-core/tests/structural_diagnostics.rs)
cover bounded rendering, redaction and semantic comparison.
