# Counter contracts

Each of the 20 warm-up actions and 10,000 measured actions must commit once. An independent check verifies the final state after extraction and host shutdown.

The default executable checks the work without timing it. Pass `--measure` for timings. Pass `--measure-lifecycle` to time construction, one committed action, state extraction and teardown.

With the `allocation-measurement` feature, `--measure-allocations` reports process-wide Rust allocations during construction, warm-up, measured work and teardown. The controls run in separate processes. These counts do not measure resident memory. Use a build without this feature for timings.

The [minimal counter](candidate/src/bin/minimal-counter.rs) runs without measurement code. The [concurrent ingress fixture](concurrent-ingress.md) distinguishes mailbox admission from a committed action.

Run the tests from the repository root with a process deadline. On macOS, use the [Cargo memory guard](../../../CONTRIBUTING.md#build-memory-safety).

```sh
cargo test --locked --all-features --manifest-path tests/runtime-contracts/counter/candidate/Cargo.toml
```
