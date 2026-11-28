# Keyed child contracts

A parent routes actions to two children with different step sizes. The test changes one child, then replaces it under the same key. The other child must retain its own state.

The [expected outputs](common.rs) are independent of the reducer. Each send waits for its causal effects to finish. The parent must see `[(1,10), (2,100), (1,14), (1,7), (2,110)]` in that order.

The [controls](tests.rs) must reject shared step sizes, output sent to the wrong child, and a replacement that retains the previous step. An unrelated error does not count.

The [mounted Store policy](mounted.md) describes ownership checks and allocation measurements for empty, small and large collections.

Run the tests from the repository root with a process deadline. On macOS, use the [Cargo memory guard](../../../CONTRIBUTING.md#build-memory-safety).

```sh
cargo test --locked --manifest-path tests/runtime-contracts/keyed-policy/candidate/Cargo.toml
```
