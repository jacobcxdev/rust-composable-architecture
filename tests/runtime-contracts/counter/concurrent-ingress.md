# Bounded concurrent sends

This fixture blocks a running reducer, then starts four producers. Each calls `try_send` with a different ID. The mailbox holds two actions.

Two sends must succeed. Their receipts must remain pending while the reducer is blocked. Two sends must return the original actions. Which producers succeed first is unspecified.

After the gate opens, the caller waits for both commits and retries each rejected action. The final state must contain each ID and its expected value exactly once.

Run the [fixture](candidate/src/bin/concurrent-ingress.rs) with `--bin concurrent-ingress` and a process deadline. On macOS, use the [Cargo memory guard](../../../CONTRIBUTING.md#build-memory-safety). A timeout is a failure, not a latency measurement.
