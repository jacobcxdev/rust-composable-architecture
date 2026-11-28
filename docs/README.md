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

The [`v0.0.1` preview](https://github.com/jacobcxdev/rust-composable-architecture/tree/v0.0.1) has no plugin or standalone skills. Use the source at that tag.
For a published crate whose tag includes the plugin, find the resolved version in your application's `Cargo.lock`.
Set `RCA_TAG` to the matching [release tag](https://github.com/jacobcxdev/rust-composable-architecture/tags), including the leading `v`. Then install:

```sh
codex plugin marketplace add jacobcxdev/rust-composable-architecture --ref "$RCA_TAG"
codex plugin add rca@rca-tools
```

If your checkout contains `.agents/plugins/marketplace.json`, run `codex plugin marketplace add .` from its root instead of the first command.
This selects guidance for that checkout, including changes that do not yet have a tag.
When you upgrade RCA, select guidance from the same revision.
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

Start with the [runnable examples](../examples/README.md). Each application teaches a different decision, from sending one action to restoring state from a file.
For scoped work, owned values, or multiple children, use the [worked recipes](../examples/README.md#worked-recipes).

## API reference and support

If you followed a link here from the API reference, return to that page to search the same revision.
The **Source** links below stay in the revision of this guide.

For hosted HTML, look for RCA and your branch or tag in the [documentation catalogue](https://docs.jacobcx.dev/).
A branch can move before its next build. To check a snapshot, compare its `docs-meta.json` field `commitSha` with your commit.
If the catalogue has no matching build, [build a local preview](maintainers/documentation-publishing.md#build-a-local-preview).
Open `index.html` in the output directory from that recipe.

For a published crate, the **Released API** column opens docs.rs. Select the version from your application's
`Cargo.lock`. The reference becomes available after docs.rs builds the package. A Git tag alone is not a crate release.

| What you need | Source in this revision | Released API on docs.rs |
| --- | --- | --- |
| Application imports, including service dependencies and shared state | [`rca`](../crates/rca/src/lib.rs) | [`rca`](https://docs.rs/rca) |
| The same application API under the long package name | [`rust-composable-architecture`](../crates/rust-composable-architecture/src/lib.rs) | [`rust-composable-architecture`](https://docs.rs/rust-composable-architecture) |
| State changes, effects, runtime contracts, and child identity | [`rca-core`](../crates/rca-core/src/lib.rs) | [`rca-core`](https://docs.rs/rca-core) |
| An existing Tokio runtime | [`rca-tokio`](../crates/rca-tokio/src/lib.rs) | [`rca-tokio`](https://docs.rs/rca-tokio) |
| A test Store, a clock you control, and exhaustive assertions | [`rca-test`](../crates/rca-test/src/lib.rs) | [`rca-test`](https://docs.rs/rca-test) |
| Documents, storage backends, and recovery | [`rca-persistence`](../crates/rca-persistence/src/lib.rs) | [`rca-persistence`](https://docs.rs/rca-persistence) |
| Macros for dependencies, paths, snapshots, and reducer composition | [`rca-macros`](../crates/rca-macros/src/lib.rs) | [`rca-macros`](https://docs.rs/rca-macros) |

The support policy lists [Cargo features](support.md#features) and [public import paths](support.md#public-api-paths).
Service dependencies and shared state need no feature. See [how to choose between them](guides/dependencies-and-shared-state.md).

The [support policy](support.md#targets) lists current targets. The [roadmap](roadmap.md) describes plans for embedded devices.

## Contribute and release

1. [Build and review a change](../CONTRIBUTING.md).
2. [Choose checks for the change](maintainers/qualification.md). Use [compiler contracts](maintainers/compiler-contract-testing.md) for compile-time behaviour and the [keyed fuzzer](../fuzz/README.md) for generated lifecycle tests.
   See [runtime measurements](performance/method.md), [composition measurements](performance/composition/README.md), and [compile and size fixtures](../benches/compile/README.md) for benchmarks.
3. [Publish the crates](maintainers/releasing.md) or [check a documentation build](maintainers/documentation-publishing.md#check-a-build). These use separate workflows.
