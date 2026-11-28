# RCA examples

Start with the counter. Then choose the example that answers your next question.
Each application runs without a network service.

On macOS, run the commands through the [Cargo memory guard](../CONTRIBUTING.md#build-memory-safety).
Pass the arguments after `cargo` to the guard after `--`, as in the [counter quick start](../README.md#quick-start).

| Application | What to look for | How to run it from the repository root |
| --- | --- | --- |
| [Counter](counter/src/main.rs) | Send one action, read the committed value, and shut down the Store. | `cargo run --manifest-path examples/counter/Cargo.toml` |
| [Search](search/src/main.rs) | Inject a search client. Replace an earlier request when the query changes. | `cargo run --manifest-path examples/search/Cargo.toml` |
| [Timer](timer/src/main.rs) | Observe ticks and wait for cancellation. The test advances a clock without waiting for real time. | `cargo run --manifest-path examples/timer/Cargo.toml` |
| [Search with child features](composition-search/src/main.rs) | Observe one row. Removing the row ends its observation. Compare [handwritten and generated composition](composition-search/src/lib.rs). | `cargo run -p rca-example-composition-search` |
| [Persistent counter](persistent-counter/README.md) | Save a value to a file. Start the application again and read the saved value. | Use the linked instructions with a file you can replace. |

## Worked recipes

These four programs share one Cargo package. To run one, replace `NAME` with its target from the table:

```sh
cargo run -p rca-example-composition-search --example NAME
```

| If you need to… | Guide | Target |
| --- | --- | --- |
| Replace one search without cancelling another | [Named search tasks](composition-search/docs/named-operations.md) | `named_operations` |
| Send a reply to the child that made the request | [Child authorisation](composition-search/docs/authorisation.md) | `authorisation` |
| Keep test history without keeping a live resource | [Owned importer](composition-search/docs/owned-importer.md) | `owned_importer` |
| Report progress and reject a result from a replaced worker | [Uploader](composition-search/docs/uploader.md) | `uploader` |

Three further guides walk through integration tests instead of separate applications:

- [Group refresh](composition-search/docs/refresh-all.md): send to a fixed list of children, even when one mailbox is full.
- [Session changes](composition-search/docs/session-refresh.md): decide whether a task uses its original client or the current service.
- [Document restoration](composition-search/docs/staged-restoration.md): restore a document and its selected page together.
