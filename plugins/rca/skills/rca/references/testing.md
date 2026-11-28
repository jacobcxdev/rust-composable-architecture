# Deterministic tests

Use `TestStore::new` for state with independent, comparable clones. For move-only state, use `TestStore::projected` with an owned snapshot.
A cloned `Arc<Mutex<_>>` still sees later mutations. It is not a snapshot.
For owner-thread tests, import `rca_test::local::TestStore` from `rca-test` with its `local` feature.

Exercise the same reducer or feature as the application. Supply a controlled client with `TestStore::builder(...).with_dependency::<Key>(client)`.
For deadlines, use the test clock. Use `with_clock_dependency` for a client that needs the same clock.

1. Send the external input. Use `send_with` to describe the expected state change.
2. Release the pending client result or advance virtual time. A sleep does not establish that work started.
3. Receive what the feature produced. Use `receive_with` for an effect's action and state change, or `receive` for an unchanged snapshot.
   Sending a fabricated success action bypasses the effect and its routing.
4. For typed work, match its actual operation: `receive_update` for state updates or `receive_output` for declared outputs.
5. End long-lived work through the application. Then call `store.finish().await?` to detect unasserted transactions and unfinished work.

For example, this test sends `Start`. Only the feature can produce `Response`:

```rust
let mut store = TestStore::new(State::Idle, Feature);
let _start = store
    .send_with(Action::Start, |state| *state = State::Started)
    .await?;
let _response = store
    .receive_with(
        |action| *action == Action::Response,
        |state| *state = State::Responded,
    )
    .await?;
store.finish().await?;
```

The complete example, including the reducer, is in `crates/rca-test/tests/test_store.rs` in the matching revision.

For same-ID replacement, hold the original child's effect sender. Replace the child, then deliver through that sender.
Assert rejection and unchanged replacement state. Deliver a distinct result through the live child and assert that it succeeds.
Cancelling the old future alone does not exercise late delivery. Use a real `Store` with a `DeterministicRuntime` to control mailbox order.
The regression test in `crates/rca-test/tests/shared_keyed_composition.rs` shows that race.

For the other typed operations, see `docs/usage/testing.md`.
For progress, replacement, and a move-only result without a live service, see `examples/composition-search/docs/uploader.md`.
