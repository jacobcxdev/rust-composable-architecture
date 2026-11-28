# Observation contracts

The fixture uses `Store::observe` to read the initial value and one publication per paced update. The expected trace is `[0, 3, 1, 1, 4]`. The second `1` matters: the observer must not silently skip it. After the observer leaves, another update sets the final state to `9`.

The [controls](tests.rs) omit a repeated value, publish the previous value, or leave the observer registered. Each must fail for its own reason. This test does not establish what a slow consumer sees during a burst.

With the `allocation-measurement` feature, `observation-allocations` counts process-wide Rust allocations through registration, updates, unregistration and teardown. The [report tests](reporter_tests.rs) reject incomplete or inconsistent measurements.

Run the tests from the repository root with a process deadline. On macOS, use the [Cargo memory guard](../../../CONTRIBUTING.md#build-memory-safety).

```sh
cargo test --locked --all-features --manifest-path tests/runtime-contracts/observation/candidate/Cargo.toml
```
