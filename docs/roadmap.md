# RCA roadmap

These proposals describe future work. For what you can use now, see the [support policy](support.md#targets).

## A shared core for hosted and embedded programs

Today RCA needs the Rust standard library. A microcontroller without an operating system cannot run the current Store.
The goal is to reuse the same decision logic on a computer and a small device.

For example, an action can ask a controller to turn on a light. The shared logic updates its state and returns a command.
The device operates the light. A test on a computer can record the command without hardware.
The design calls for `#![no_std]`, which removes the dependency on Rust's standard library.
The core must also work without a heap allocator. These are separate requirements.
A heap allocator provides memory on request while the program runs. The design keeps the APIs of existing hosted programs.

The first planned example uses two instances of one child feature on a computer and a Cortex-M microcontroller.
Both must produce the same state and commands for the same sequence of actions.
If one child replaces another, a late result from the old child must not affect its replacement.
The comparison must report program size in flash and peak working memory, including the stack and queues.
This proposal does not promise a minimum device size or delay the first hosted release.

## Model checks: child lifetimes first

Two workers can use the same identifier at different times. [Removing a worker ends its lifetime](usage/composition.md#lifecycle-composition).
A [Store test](../crates/rca-test/tests/shared_keyed_composition.rs) queues a result from the old worker after its replacement arrives.
The Store rejects that result. The replacement can still update its own state.
The test covers one order of events, not every possible order.

One proposal uses **TLA+**, a language for describing allowed sequences of events independently of the Rust code.
For example, a model can describe which worker receives a message after its identifier changes hands.
The test can compare the Store's behaviour with the model's prediction.
This check covers delivery order. It does not prove that every task eventually finishes.

A model of the Store also cannot prove what an external system did.
For example, [an action can finish processing](usage/receipt-lifetimes.md) before a database saves the change.
The application needs the database result to establish that fact.
For example, [a worker releases its completed future before it delivers its result](../crates/rca-core/tests/direct_future.rs).
That order matters if the result starts another worker under the same resource limit.
Application authors decide which events represent their own rules.

The next decision is whether a cancellation or shutdown model finds errors that existing tests miss.
If a model adds no useful coverage, keep the targeted tests. An application does not need a model checker or a new runtime API.
