# RCA documentation

Rust Composable Architecture (RCA) helps you build and test the logic of a Rust program.
It does not draw a user interface. You can use it in a command-line tool, a background service, or an application with a separate interface.

| What brings you here? | Start here |
| --- | --- |
| **Learn about RCA.** Does it fit my project? | [Run a small example](#start-here), see [supported platforms and features](support.md), or read the [roadmap](roadmap.md). |
| **Build with RCA.** How do I write my application? | [Choose a task](#guides-by-task), [find a type or method](#api-reference-and-support), or [use the optional coding skills](#use-rca-with-a-coding-agent). |
| **Work on RCA.** How do I change the library? | [Start contributing](#contribute-and-release). This route covers library tests, benchmarks, documentation, and releases. |

## Start here

Consider a counter. Its **state** is the current number. An **action** says what happened, such as a request to add one.
A **reducer** describes how that action changes the number. A **Store** owns the state and processes actions in order.

1. [Run the counter](../README.md#quick-start). On success, the counter exits without printing. The build tools can still print messages.
2. [Write a feature](usage/authoring.md). A feature describes one part of your program, such as the counter.
3. [Test your feature](usage/testing.md). Check the state after an action without waiting for a real clock or calling a server.
4. [Combine features](usage/composition.md). Give each counter its own identity so an action reaches the intended counter.

If you are new to Rust, [Hello, Cargo!](https://doc.rust-lang.org/book/ch01-03-hello-cargo.html) introduces its build and package tool.
You do not need experience with Swift or another architecture library.

## Guides by task

| I want to… | Read |
| --- | --- |
| Decide where a value belongs | [Choose feature state, a dependency, or shared state](usage/authoring.md#where-does-this-value-belong) |
| Give several items the same behaviour but different state | [Compose child features](usage/composition.md) |
| Start or stop a child independently of its parent | [Install a child feature](usage/structural-features.md) |
| Represent jobs that create more jobs | [Choose a tree or a stack](usage/recursive-jobs.md) |
| Work with a value that must stay on one thread | [Use local composition or a Local Store](usage/local-composition.md) |
| Replace a real service in a test | [Supply a dependency](usage/dependencies.md) or [test a request or stream](usage/external-clients.md) |
| Share one mutable value between owners | [Share state](usage/shared-state.md) |
| Save edits and load them after a restart | [Persist a document](usage/persistence.md) |
| Pass ownership of an open file or another resource | [Transfer an owned resource](usage/owned-resources.md) |
| Limit how often a request starts | [Debounce or throttle requests](usage/debounce-throttle.md) |
| Read state or receive updates | [Observe a value or a list of IDs](usage/runtime.md#observation) |
| Reduce the cost of work in my application | [Measure application performance](usage/performance.md) |
| Wait for the right result or stop the Store | [Choose a receipt](usage/receipt-lifetimes.md) or [shut down the Store](usage/runtime.md#shutdown-and-host-lifetime) |
| Check state that cannot be copied for a test | [Test a projection](usage/testing.md) or [derive a snapshot](usage/snapshot-derive.md) |
| Find out why an action changed state | [Trace a change](usage/diagnostics.md) |
| Connect a user interface, database, or notification source | [Decide what the host must do](usage/integration-boundaries.md) |

For working code, see the [runnable examples and tested recipes](../examples/README.md).
The [authoring decision table](usage/authoring.md#find-a-decision) also links design questions to specific APIs.

## API reference and support

The API reference describes the Rust types, methods, and functions your code can call.
Cargo records the package versions it selected in your application's `Cargo.lock`.

- **For a published version**, open the docs.rs link for the package you use. Select the version from `Cargo.lock`.
- **For a branch or tag**, look for the matching build in the [documentation catalogue](https://docs.jacobcx.dev/).
- **For this checkout**, use the source links below or [build a local API reference](development/documentation-publishing.md#build-a-local-preview).

If the catalogue has no build for your revision, open the source at that revision or build the local reference.
If a crate or version is not on docs.rs, use the source column below.
For example, the [`v0.0.1` preview](https://github.com/jacobcxdev/rust-composable-architecture/tree/v0.0.1) exposes only the snapshot API, not the Store in this guide.
A branch can change before its documentation updates. To identify the exact build, compare the `commitSha` in its `docs-meta.json` with your Git commit.
A Git tag names a revision. It does not publish a package or make its reference available on docs.rs.

| What you need | Source in this revision | docs.rs after publication |
| --- | --- | --- |
| The usual application imports, including dependencies and shared state | [`rca`](../crates/rca/src/lib.rs) | [`rca`](https://docs.rs/rca) |
| The same API under the long package name | [`rust-composable-architecture`](../crates/rust-composable-architecture/src/lib.rs) | [`rust-composable-architecture`](https://docs.rs/rust-composable-architecture) |
| Core types for state changes, effects, runtimes, and children | [`rca-core`](../crates/rca-core/src/lib.rs) | [`rca-core`](https://docs.rs/rca-core) |
| Connect a Store to an existing Tokio runtime | [`rca-tokio`](../crates/rca-tokio/src/lib.rs) | [`rca-tokio`](https://docs.rs/rca-tokio) |
| Control time and check every expected change in a test | [`rca-test`](../crates/rca-test/src/lib.rs) | [`rca-test`](https://docs.rs/rca-test) |
| Load and save documents | [`rca-persistence`](../crates/rca-persistence/src/lib.rs) | [`rca-persistence`](https://docs.rs/rca-persistence) |
| Generate Rust code for common RCA patterns | [`rca-macros`](../crates/rca-macros/src/lib.rs) | [`rca-macros`](https://docs.rs/rca-macros) |

Start with `rca`. Dependencies and shared state need no extra package or feature.
For optional integrations, see the [Cargo features](support.md#features) and [public import paths](support.md#public-api-paths).

## Contribute and release

| What are you changing? | Start here |
| --- | --- |
| The library or a test | [Build and review a change](../CONTRIBUTING.md), then [choose the checks for it](development/qualification.md). |
| What callers can compile | [Test a compiler contract](development/compiler-contract-testing.md). For generated sequences of child actions, see the [keyed collection fuzzer](../fuzz/README.md). |
| The cost of using RCA | [Benchmark the library](development/benchmarking.md). To measure your own program instead, see [application performance](usage/performance.md). |
| The guides or API reference | [Review documentation](../CONTRIBUTING.md#documentation-acceptance) or [preview and publish documentation](development/documentation-publishing.md). |
| A crate release | [Prepare and publish packages](development/releasing.md). Package and documentation publication use separate workflows. |

## Use RCA with a coding agent

A skill gives a coding agent instructions and examples for a particular task.
The optional Codex plugin bundles two skills. It adds no dependency to your application.

| Task | Skill | Example request |
| --- | --- | --- |
| Build an application with RCA | [`$rca:rca`](../plugins/rca/skills/rca/SKILL.md) | Test that a removed worker cannot send a result to its replacement. |
| Change RCA itself | [`$rca:rca-contribute`](../plugins/rca/skills/rca-contribute/SKILL.md) | Which tests catch a change to the order of cancellation and shutdown? |

Use the skills from the same revision as your code. The [`v0.0.1` preview](https://github.com/jacobcxdev/rust-composable-architecture/tree/v0.0.1) contains neither the plugin nor standalone skills.

For a published version with skills, set `RCA_TAG` in your shell to the matching [release tag](https://github.com/jacobcxdev/rust-composable-architecture/tags), including the leading `v`.
Run these commands with the Codex CLI:

```sh
codex plugin marketplace add jacobcxdev/rust-composable-architecture --ref "$RCA_TAG"
codex plugin add rca@rca-tools
```

For a Git dependency, use the commit from `Cargo.lock` instead of the tag.
If your checkout contains `.agents/plugins/marketplace.json`, you can install from the checkout instead.
From its root, use `codex plugin marketplace add .` as the first command.

To switch releases, remove the old plugin and catalogue first:

```sh
codex plugin remove rca@rca-tools
codex plugin marketplace remove rca-tools
```

Set `RCA_TAG` to the new tag. Repeat the two installation commands.
The plugin changes the agent's guidance, not your application's dependencies.
The [changelog](../CHANGELOG.md) lists deprecations and migration steps.

For an agent that reads standalone skills, install the [skill directories](../plugins/rca/skills) from the same revision instead.
Their names are `$rca` and `$rca-contribute`. Use the plugin or the standalone skills, not both.
