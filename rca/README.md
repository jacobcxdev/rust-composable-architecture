# Rust Composable Architecture

This preview provides `Snapshot`, a trait for recording independent state in
tests and inspecting it after the live state changes.

```rust
use rca::Snapshot;

let mut items = vec![String::from("queued")];
let before = items.snapshot();
items[0].push_str(" and started");
assert_eq!(before, ["queued"]);
```

RCA 0.1.0 adds the Store, reducers, effects, and deterministic tests.
[Follow the implementation review](https://github.com/jacobcxdev/rust-composable-architecture/pull/1).
