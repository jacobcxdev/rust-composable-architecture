# Timer policy contracts

These tests use a virtual clock. They check which output the caller sees at each deadline. They do not measure how accurately an operating system schedules a timer.

- [Trailing debounce](common.rs) replaces a pending output. Inputs at times zero and three must produce only the second output at time seven.
- [Trailing throttle](throttle_common.rs) keeps a planned deadline. A late wake must not shift subsequent windows. A reset must cancel pending work.
- [Leading debounce](leading_common.rs) checks immediate delivery and the inclusive boundary between windows. A reset must clear the previous window.

Each control changes one rule. An isolated child process must fail with the expected diagnostic. A timeout, unrelated failure or oversized diagnostic does not count.

Run the tests from the repository root with a process deadline. On macOS, use the [Cargo memory guard](../../../CONTRIBUTING.md#build-memory-safety).

```sh
cargo test --locked --manifest-path tests/runtime-contracts/timers/candidate/Cargo.toml
```
