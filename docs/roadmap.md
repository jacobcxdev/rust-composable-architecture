# RCA roadmap

## A shared core for hosted and embedded programs

The goal is to use the same state, actions, and update logic on a host and on a microcontroller.
The shared core will use `#![no_std]` and will not need a heap allocator.
Each host or device will carry out the commands that the shared logic returns.
Existing hosted programs will keep their current APIs.

The first example will run one feature with two child instances on a host and a Cortex-M microcontroller.
Both must produce the same state changes and commands for the same actions.
A late result for a removed child must not reach its replacement, even if both use the same ID.
The comparison will report flash use and peak RAM use separately, including stack and queue space.

This is planned work, not a release requirement. The [support policy](support.md#targets) lists the supported targets.

## Model checks: child lifetimes first

Removing a child ends its [lifetime](composition.md#lifecycle-composition), even if a replacement has the same ID.
A [Store test](../rca-test/tests/shared_keyed_composition.rs) puts an old result behind a replacement in the mailbox.
The Store rejects the old result before reduction. The replacement can still commit new work.
This test covers a particular event order. It does not prove that every possible order is safe.

One option is to check a small TLA+ model against the public Store API.
For example, the model can queue an old result and a fresh result after a child ID changes hands.
The Store must reject the old result and deliver the fresh one. The expected outcome must come from the model, not from the Store.
This check tests delivery order. It does not prove that every task eventually finishes.

Application authors own their domain rules and the mapping between model events and application actions.
For a worker limit, [`Effect::future`](../rca-core/tests/direct_future.rs) drops the completed future before it delivers the result.
That order matters when processing the result can start another worker.
For a storage guarantee, an [action commit](receipt-lifetimes.md) does not prove that a database saved the change.
Use the database result for that claim.
The next step is to decide whether cancellation or shutdown needs a model of its own.
Compare the cost of maintaining the model with the errors it can find beyond existing tests.
If it adds no useful coverage, keep the targeted regression tests. Applications do not need a model checker or a new runtime API.
