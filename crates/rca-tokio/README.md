# rca-tokio

`TokioRuntime` runs RCA work on a Tokio runtime that your application owns.
Enable `tokio` on [`rca`](../rca/README.md) to import the adapter as `rca::TokioRuntime`.
The adapter does not start another runtime.

See the [counter example](../../examples/counter/src/main.rs).
For a published version, select it in the [API reference](https://docs.rs/rca-tokio).
