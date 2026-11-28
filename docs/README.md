# RCA documentation

## Start here

1. [Run the counter](../README.md#quick-start). Send one action and read the new state.
2. [Write a feature](guides/authoring.md). Learn what the Store owns and when to use a reducer or a task.
3. [Test the result](guides/testing.md). Check state changes and follow-up actions without a network connection or a real timer.
4. [Compose child features](composition.md). Route an action to the right child, even after the children change order.

If you already use RCA, the [authoring decision table](guides/authoring.md#find-a-decision) links each design question to its API.

## Use RCA with a coding agent

The optional Codex plugin contains two skills. It adds no dependency to a Rust application.

| Task | Skill | Example request |
| --- | --- | --- |
| Build an application **with** RCA | [`$rca:rca`](../plugins/rca/skills/rca/SKILL.md) | Test that a removed worker cannot update its replacement, even when both use the same ID. |
| Change **RCA itself** | [`$rca:rca-contribute`](../plugins/rca/skills/rca-contribute/SKILL.md) | Review a cancellation change. Which contracts and tests distinguish the old behavior from the new one? |

For a published crate, find its resolved version in the application's `Cargo.lock`.
Set `RCA_TAG` to the matching [release tag](https://github.com/jacobcxdev/rust-composable-architecture/tags), including the leading `v`. Then install:

```sh
codex plugin marketplace add jacobcxdev/rust-composable-architecture --ref "$RCA_TAG"
codex plugin add rca@rca-tools
```

For an RCA checkout, use `codex plugin marketplace add .` from its root instead of the first command.
This selects guidance for that checkout, including changes that do not yet have a tag.
The plugin and both skills ship with RCA. When you upgrade RCA, select guidance from the same revision.
See the [changelog](../CHANGELOG.md) for deprecations and migration steps.

To switch from one pinned release to another, remove the old plugin and its catalog first:

```sh
codex plugin remove rca@rca-tools
codex plugin marketplace remove rca-tools
```

Set `RCA_TAG` to the new tag and repeat the two install commands. This changes the agent's guidance, not the application's dependencies.
For a Git dependency, use its resolved commit from `Cargo.lock` in place of the tag.

For agents that read standalone skills, install the desired [skill directories](../plugins/rca/skills) from the same revision instead.
The standalone names are `$rca` and `$rca-contribute`. Use either the plugin or standalone skills, not both.

## Guides by task

| What do you need to do? | Where to look |
| --- | --- |
| Route an action to a child or change the order of keyed children | [Reducer composition](composition.md) |
| Give a child its own lifetime or send a result to its parent | [Installed children](structural-features.md) |
| Choose between a flat stack and a tree of jobs | [Recursive jobs](recursive-jobs.md) |
| Keep a value on its owner thread | [Local composition and the Local Store](local-composition.md) |
| Choose between state, a shared value and a dependency | [Dependencies and shared state](guides/dependencies-and-shared-state.md) |
| Connect a service or transfer ownership of a resource | [External clients](guides/external-clients.md) and [owned resources](guides/owned-resources.md) |
| Limit how often a request starts | [Debounce and throttle](guides/debounce-throttle.md) |
| Read current state or observe changes to a value or a list of child IDs | [Observation](runtime.md#observation) |
| Cancel work, wait for it to finish or shut down the Store | [Receipt lifetimes](receipt-lifetimes.md) and [shutdown](runtime.md#shutdown-and-host-lifetime) |
| Write a deterministic test or keep test history for state that cannot be cloned | [Testing](guides/testing.md) and [owned snapshots](snapshot-derive.md) |
| Trace a state change or report an issue | [Diagnostics](guides/diagnostics.md) |
| Connect a native UI, database or notification source | [Host responsibilities](integration-boundaries.md) |

## Examples

| Example | What it shows |
| --- | --- |
| [Counter](../examples/counter/src/main.rs) | Send one action and read the result. |
| [Search](../examples/search/src/main.rs) | Use an injected client for an asynchronous request. |
| [Timer](../examples/timer/src/main.rs) | Observe ticks and cancel the timer. |
| [Composition search](../examples/composition-search/src/main.rs) | Observe a child until its parent removes it. |
| [Persistent counter](../examples/persistent-counter/README.md) | Save a value and read it on the next run. |
| [Server lifecycle](../examples/mcmd-server/README.md) | Test how servers start, stop and use a proxy. This example is not a runnable daemon. |

### Worked examples

| Task | Example |
| --- | --- |
| Replace one search without cancelling another | [Named search tasks](../examples/composition-search/docs/named-operations.md) |
| Send a reply to the child that made the request | [Child authorization](../examples/composition-search/docs/authorisation.md) |
| Keep test history without keeping the session permit alive | [Importer history](../examples/composition-search/docs/owned-importer.md) |
| Report upload progress and reject a result from a replaced worker | [Headless uploader](../examples/composition-search/docs/uploader.md) |
| Try to refresh a fixed list of children | [Group refresh](../examples/composition-search/docs/refresh-all.md) |
| Keep a captured client or read the current service | [Session changes](../examples/composition-search/docs/session-refresh.md) |
| Restore a document and its selected page together | [Document restoration](../examples/composition-search/docs/staged-restoration.md) |

## API reference and support

Use the [`rca` package](../rca/Cargo.toml) for the [main library](../rca/src/lib.rs).
The support policy lists [Cargo features](support.md#features) and [public import paths](support.md#public-api-paths).

| For this API | Read this crate |
| --- | --- |
| Application imports and optional features | [`rca`](../rca/src/lib.rs) |
| State changes, effects, runtime contracts and child identity | [`rca-core`](../rca-core/src/lib.rs) |
| An existing Tokio runtime | [`rca-tokio`](../rca-tokio/src/lib.rs) |
| A test Store, a clock you control and exhaustive assertions | [`rca-test`](../rca-test/src/lib.rs) |
| Documents, storage backends and recovery | [`rca-persistence`](../rca-persistence/src/lib.rs) |
| Macros for dependencies, paths, snapshots and reducer composition | [`rca-macros`](../rca-macros/src/lib.rs) |

For searchable HTML in a local checkout, use the [documentation build instructions](documentation-publishing.md).
On macOS, use the [build memory guard](../CONTRIBUTING.md#build-memory-safety) for each Cargo command.
After the build, open `target/doc/rca/index.html`.

The [support policy](support.md#targets) lists current targets. The [roadmap](roadmap.md) describes plans for embedded devices.

## Contribute and release

1. [Build and review a change](../CONTRIBUTING.md).
2. [Choose checks for the change](qualification.md). Use [compiler contracts](compiler-contract-testing.md) for compile-time behaviour and the [keyed fuzzer](../fuzz/README.md) for generated lifecycle tests.
   See [runtime measurements](performance/method.md), [composition measurements](performance/composition/README.md), and [compile and size fixtures](../benches/compile/README.md) for benchmarks.
3. [Publish the crates](releasing.md) or [publish the API documentation](documentation-publishing.md). These use separate workflows and approvals.
