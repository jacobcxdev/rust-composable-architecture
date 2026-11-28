# Stream and future contracts

The fixture sends an empty stream or the values `[3, 1, 4, 2]` through an RCA effect. It checks committed order, the terminal action, producer exhaustion and stream destruction. A rejected delivery or cancellation after a partial result must not look like successful completion.

The [ownership tests](candidate/src/ownership.rs) hold a future pending. Explicit cancellation must release that future. Dropping its observation handles must not cancel it.

The default executable runs the finite streams. With `--ready-future`, it checks 64 serial operations, their results, poll counts, destruction and causal completion. The [future outcome tests](outcome_native_tests.rs) reject incorrect results and premature completion.

With the `allocation-measurement` feature, `--measure-ready-future-allocations` measures allocations in the ready workload. The separate `common-outcome-allocations` binary includes the result, destruction and checkpoint of each operation. Neither report measures resident memory or elapsed time.

Run the tests from the repository root with a process deadline. On macOS, use the [Cargo memory guard](../../../CONTRIBUTING.md#build-memory-safety).

```sh
cargo test --locked --all-features --manifest-path tests/runtime-contracts/stream/candidate/Cargo.toml
```
