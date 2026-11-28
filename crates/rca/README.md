# rca

Build headless Rust applications from state, actions, and effects. A Store processes
state changes in order. Your application supplies the runtime and any user interface.

Start with the [counter and its complete manifest](../../README.md#quick-start).
Then [write a feature](../../docs/usage/authoring.md). This package has no default features.
Use `tokio` to run a Store on an existing Tokio runtime, or `test-util` for deterministic tests.

For a published version, select it in the [API reference](https://docs.rs/rca).
On GitHub, select the matching release tag for guides and examples from that version.
