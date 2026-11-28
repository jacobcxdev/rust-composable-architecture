# rca-persistence

Add this crate alongside `rca` to save and restore documents. Supply a codec
and a backend. The optional `file` feature provides a file backend for a Tokio runtime.
Store shutdown does not flush writes. Before shutdown, submit the document's close action.
Wait for its receipt to succeed.

See the [persistent counter](../../examples/persistent-counter/README.md).
For a published version, select it in the [API reference](https://docs.rs/rca-persistence).
