# Contributing to RCA

RCA is a headless library for composable, testable Rust programs.
Changes must preserve its ownership, cancellation and deterministic testing contracts.

- Before compiling on macOS, read [build memory safety](#build-memory-safety).
- For a code change, start with [source review](#reviewing-a-change) and [required checks](#documentation-and-checks).
- For changes to a guide or public API, use the [documentation checklist](#documentation-acceptance).
- For release work, use the [qualification guide](docs/maintainers/qualification.md) and the [crate release guide](docs/maintainers/releasing.md).

## Where things live

| Task | Start here |
| --- | --- |
| Change a library | [`crates/`](crates) contains the RCA packages. Start with the [package guide](docs/README.md#api-reference-and-support) to find the owner of an API. |
| Change a transport dependency | [`dependencies/`](dependencies) contains three upstream forks. Each fork records its origin in `UPSTREAM.md`. |
| Write or test an application | [`examples/`](examples/README.md) contains runnable applications. [`docs/`](docs/README.md) contains the application guides. |
| Test a contract | Each crate keeps its own tests. [`tests/`](tests) contains independent consumers and runtime fixtures. The [keyed fuzzer](fuzz/README.md) tests generated action sequences. |
| Measure a change | [Choose a benchmark](docs/performance/README.md) for the RCA operation or build cost you changed. To measure your own application, use the [app performance guide](docs/guides/performance.md). |
| Change a guide or skill | [`plugins/rca/skills/`](plugins/rca/skills) contains the application and contributor skills. [Check the documentation and standalone skills](#documentation-acceptance). |
| Run repository checks | [`scripts/`](scripts) contains the [macOS build guard](#build-memory-safety), [package checks](scripts/verify-packaged-downstream.sh), and other tools used by [CI](.github/workflows/ci.yml). |
| Maintain a release | [`docs/maintainers/`](docs/maintainers) contains procedures for compiler contracts, qualification, crate releases, and documentation publication. |

## Proposing a change

For suspected vulnerabilities, use the private route in [SECURITY.md](SECURITY.md).
Do not put security details or credentials in public issues or pull requests.

For ordinary defects, include the revision, toolchain, enabled features and the
smallest example that reproduces the problem. Give the expected and actual results.
For API or runtime changes, describe the headless workload and the contract that
needs to change. Before making a compatibility claim, check the [support policy](docs/support.md).

Keep each pull request focused on one coherent change. Preserve applicable licence
and attribution notices. Explain any new dependency and its feature, platform and
licence effects. Do not include build output, credentials or temporary work files.
Commit reproducible workloads, expected compiler diagnostics, and small test inputs that reproduce a specific defect.
Write benchmark output and command transcripts to ignored output directories. Keep runner profiles and release evidence outside Git. The controller tracks each approved profile's digest, not its contents.

Use signed Conventional Commits. Keep the subject shorter than 50 characters.
Use a past-tense summary, such as `fix: preserved cancellation ordering`.
After a blank line, list significant changes as bullets.

Use the [pull-request template](.github/pull_request_template.md) to record the
reason, compatibility effects and checks. State exactly which checks did not run.
Assign pull requests to `jacobcxdev`.

Before each push, compare the README, repository guidance, examples and workflow
documentation with the code. Correct obsolete instructions, versions and links.
Use relative links for guides in this repository. Keep release history and migration notes in the [changelog](CHANGELOG.md).
Describe a host by its requirements, not its private name, account path or network address.
Keep private repository links and temporary development notes out of public guides.

The [CODEOWNERS file](.github/CODEOWNERS) names the responsible reviewer.
It does not establish required approval or branch protection. Maintainers must
check those remote settings separately before release. Local test results do not
prove that hosted checks passed. Code review does not authorise publication or
release actions.

## Repository protection

Use `dev/next` for changes to the next version. Promote reviewed changes to `main` through a pull request.
Crate publication requires a signed tag on the reviewed tip of `main`.
Both branches use the same [CI selection rules](#how-ci-selects-checks). Their [documentation channels](docs/maintainers/documentation-publishing.md) are separate.

Before release, check these settings in GitHub:

- On the default branch, require pull requests, verified commit signatures, resolved discussions, and all required GitHub Actions checks.
  Require the current base revision. Block administrator bypass, force pushes, and deletion.
- Block force pushes to other published branches. Prevent changes to published tags, including deletion.
- Use merge commits to preserve signed review commits. Disable squash, rebase, and automatic merging.
  A sole maintainer cannot approve their own pull request. Do not require a numerical approval count in that configuration.
  The [source-review requirements](#reviewing-a-change) still apply.
- Use read-only workflow tokens by default. Prevent Actions from approving pull requests.
  Require workflow approval for external contributors. Pin remote actions from approved sources to full commit SHAs.
- Use the separate [documentation environment](docs/maintainers/documentation-publishing.md#configure-automatic-publication) for signing.

## Toolchain and formatting

The repository pins Rust, rustfmt, Clippy and `rust-src` through `rust-toolchain.toml`.
Compiler snapshots need `rust-src` for standard-library source excerpts.
System installations of Cargo or rustc can bypass rustup and ignore this file. Put the pinned tools first in `PATH`.
Select the compiler and documentation generator by absolute path, including when a compiler wrapper is active.
In a POSIX shell with Python 3.11 or later, set `toolchain` once for the commands in this guide:

```sh
toolchain=$(python3 -c 'import tomllib; print(tomllib.load(open("rust-toolchain.toml", "rb"))["toolchain"]["channel"])')
export PATH="$(dirname "$(rustup which --toolchain "$toolchain" cargo)"):$PATH"
export RUSTC="$(rustup which --toolchain "$toolchain" rustc)"
export RUSTDOC="$(rustup which --toolchain "$toolchain" rustdoc)"
rustup run "$toolchain" rustc --version
python3 scripts/check_rust_format.py
```

### Development and release profiles

Use `cargo check` for quick compiler feedback. Use `cargo test` to test behavior.
The workspace uses Cargo's default development settings: no optimization and full debug information.
Cargo keeps incremental build data for workspace members.
Tests for `rca-core` use a separate profile setting to limit compiler memory.

Keep the same target directory between edits so Cargo can reuse unchanged output.
For benchmark and release work, use the existing benchmark and release profiles.
Applications that depend on RCA [choose their own release profile](docs/guides/authoring.md#choose-a-release-profile).

### Build memory safety

For local macOS builds, run repository compilation through the Cargo memory watchdog.
Use Python 3.11 or later with `os.waitid` support. Apple's bundled Python 3.9 does not provide this function.
Use a new receipt path for each run:

```sh
mkdir -p target
python3 scripts/guard_cargo_memory.py --receipt target/clippy-memory.json -- \
  clippy --locked --workspace --all-targets --all-features -- -D warnings
```

The watchdog selects the pinned Cargo, rustc and rustdoc by absolute path.
It uses one Cargo job. It disables Rust compiler wrappers except for the local cache described below.
It permits only one guarded build per user at a time.

Every 250 milliseconds, it measures the owned process group's physical footprint, including compressed memory.
Its default ceiling is eight GiB. For a bounded Cargo build, `--memory-limit-gib 4` selects a stricter four-GiB ceiling.
The option accepts integers from one through eight. It does not change the four-GiB host reserve, pressure checks or cleanup.
The watchdog stops the group at its selected ceiling, on elevated memory pressure, or below the host reserve.
Startup requires headroom for both the build budget and the host reserve.
Missing measurements also stop the build.

This is a polling watchdog, not a kernel-enforced memory cap.
Memory can grow between samples. File-backed pages in the headroom estimate are not necessarily immediately available.
The guard does not contain deliberate daemonisation or survive its own forced termination.

The composition controller uses the same watchdog for command groups, including child capture scripts.
Its receipts distinguish natural completion from forced cleanup.
Slow host telemetry does not delay memory checks.
For canonical qualification, choose an explicit timeout. Use the fixed `--qualify-performance` route.
The route accepts the same stricter `--memory-limit-gib` ceiling for the complete candidate/reference capture.
The default and maximum remain eight GiB. The host reserve remains four GiB.
Both result files and the completion receipt must record the same selected ceiling.
The composition controller also accepts this option for the complete Mac capture, including both comparison modes.
The [performance commands](docs/performance/method.md#commands) show the full invocation.

Do not run a controller that creates separate process groups locally until each compiler process has equivalent protection.
Use an OS-limited remote job for large or unfamiliar builds.
Windows Job Objects and Linux container memory limits can enforce aggregate limits independently of the controller.

After a memory abort, inspect the receipt and reduce the build's memory demand.
Do not bypass the guard, raise its limit or retry the same command without a supported correction.
Keep measured benchmark profiles unchanged when adjusting correctness-only test profiles.
Run the Cargo commands below only in a guarded job or a job with an OS memory limit.

These safeguards protect shared developer machines. GitHub-hosted CI uses disposable runners with separate memory allocations and job timeouts.
CI runs Cargo directly. It also tests the watchdog on macOS, but does not use the local watchdog limits for compilation.

### Use kache for local builds

The memory watchdog can use kache 0.26.3 for native `build` and `check` commands in the RCA checkout.
Apps that depend on RCA do not need kache.
If kache or a valid proof is missing, the watchdog runs Cargo without a Rust compiler cache.
The receipt records `kache` or `off` in `rust_compiler_cache`. The watchdog never retries a failed build.

The watchdog looks for an existing local store in `~/Library/Caches/kache`.
If your local store is elsewhere, set `KACHE_CACHE_DIR` to its absolute path.
With kache on your `PATH`, run the proof once for each command:

```sh
python3 scripts/prove_guarded_kache.py --command build
python3 scripts/prove_guarded_kache.py --command check
```

Each proof checks that the watchdog counts and stops a real compiler on a cache miss.
It also checks that a cache hit restores identical output without compiling the library again.
The watchdog reads the proofs from `~/Library/Caches/rust-composable-architecture/`.
If the kernel, compiler, kache, guard or cache policy changes, repeat the proof.

Continue to use the same build command:

```sh
mkdir -p target
python3 scripts/guard_cargo_memory.py --receipt target/build-memory.json -- build --locked
```

The watchdog uses the local kache store without remote access or background cleanup.
This mode uses kache instead of Rust incremental compilation.
The watchdog skips kache for custom compiler settings, cross targets, manifests outside the checkout, and the `test` and `bench` commands.
Release qualification also skips kache.
For repeated source edits in one checkout, set `CARGO_INCREMENTAL=1` to keep Rust incremental compilation.
For the same sources in a fresh checkout, kache can restore compiled crates from the local store.
Keep the same mode throughout an edit session. A change of mode can make Cargo rebuild crates.
To run without kache for any reason, set `KACHE_DISABLED=1`.

On macOS, kache records the source root as `/kache/workspace` in debug information.
If LLDB cannot find the cached source, map this path to the checkout you want to debug:

```text
(lldb) settings set target.source-map /kache/workspace /absolute/path/to/checkout
```

### Style and lint checks

Rustfmt follows the [Rust Style Guide](https://doc.rust-lang.org/style-guide/).
Cargo supplies each package's edition for parsing and formatting. RCA packages
use edition 2024. The maintained transport forks use edition 2021.
The repository requires LF line endings. It does not require nightly formatting features.

If you run rustfmt directly, supply the edition from the owning package manifest.
For shared source fragments, use the edition of the packages that include them.

Cargo does not discover every `include!` fragment or standalone compiler fixture.
The formatting script checks every tracked and non-ignored Rust file directly.
It includes standalone examples, compiler fixtures, comparison workloads and
maintained forks. It requires Python 3.11 or later, Git and rustup.
The script reads package editions and selects the pinned rustfmt. Shared comparison
fragments use their consumers' edition. It never changes source files.

Formatting changes to rejection fixtures can move diagnostic locations. Such changes
need fresh compiler checks and review of diagnostic snapshots. Formatter success
does not prove readable logical grouping or compliance with all Rust guidance.

The workspace lint policy lives in `Cargo.toml`. The Clippy command checks workspace
targets, not every standalone compiler fixture or comparison package. For changes
outside the workspace, also run the relevant commands from
[CI](.github/workflows/ci.yml) and the fixture's owning test runner.
An all-features result does not replace minimal-feature or platform checks.

## Readable code

- Separate distinct logical stages with blank lines. Keep closely related statements together.
- Prefer names that explain purpose. Do not replace meaningful names with abbreviations to save space.
- Keep functions and modules focused on coherent responsibilities. Avoid arbitrary size limits and unnecessary wrappers.
- Keep formatting changes separate from behaviour changes.
- Explain necessary complexity. Remove complexity only when the contracts remain intact.

Rustfmt controls layout. Reviewers still need to assess logical grouping and clarity.
Use the [Microsoft Rust guidance](https://microsoft.github.io/rust-guidelines/)
and [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).
Record specific reasons for departures. Do not enable every Clippy restriction without reviewing its effect.

The release audit uses the Microsoft source guidelines at
[`08e06684`](https://github.com/microsoft/rust-guidelines/tree/08e06684582930d87a70a7620e8d39a94953118a/src/guidelines).
Use the individual source pages at that revision. Generated summaries and installed
skills can lag behind those pages. Keep managed skills unchanged during project work.
Before updating this baseline, review the guidance changes and their effects on RCA.
Guidance does not replace evidence about ownership, compatibility or performance.

## Reviewing a change

Reading source is not a completed review. Each affected unit needs explicit questions
and reasoned answers. A unit can be a function, type or module, together with its
callers, tests and contracts.

1. Establish the intended behaviour and invariants.
2. Challenge correctness with boundary inputs, state transitions and interleavings.
3. Trace ownership and cleanup through errors, cancellation, panic and drop.
4. Compare the factoring with simpler alternatives that preserve the same contracts.
5. Check names, logical stages, applicable Rust guidance and documentation.
6. Identify avoidable work and measure plausible optimisations on representative workloads.
7. Record findings, evidence and reasons to retain or change the code.
8. After changes, repeat the questions for the revised unit and affected callers.

Continue until the affected scope has no actionable findings or unanswered required
questions. High-risk changes need an independent challenge. Agent agreement is not
proof of correctness. A review with no findings is not proof of global optimality.

## Documentation and checks

Use `//!` for module documentation and `///` for item documentation. Explain purpose,
invariants and relevant errors, panics or safety requirements. Do not repeat names
or types without adding useful information. Define unfamiliar terms and use short,
direct sentences. The release audit applies the Simple English guidance to public
and private documentation.

Documentation presence needs both `missing_docs` and
`clippy::missing_docs_in_private_items`. Include a normal library build, not only
tests, so documentation checks cover the production library.
On macOS, pass `clippy` and its arguments to the [memory watchdog](#build-memory-safety).
On an isolated runner with a memory limit, run:

```sh
rustup run "$toolchain" cargo clippy --locked --workspace --all-features --lib -- -D warnings -D missing_docs -D clippy::missing_docs_in_private_items
```

CI checks every combination of each library's features, using cargo-hack 0.6.45.
It excludes only the binary-only size probe from this library command.
The existing whole-workspace feature check still covers that binary.
Test-only bodies retain their separate source review.

On an isolated runner with a memory limit, use the command below to check each library package separately.
The macOS memory watchdog does not support `cargo hack`. If you have only a local Mac, use CI for this check.
RCA uses Cargo resolver 3. A normal `--lib` build does not enable features supplied only by development dependencies.
Keep `--locked`. Do not add `--no-dev-deps`: it rewrites manifests and requires a different lockfile.

```sh
rustup run "$toolchain" cargo hack clippy --locked --workspace --exclude rca-size-probe --feature-powerset --lib -- -D warnings -D missing_docs -D clippy::missing_docs_in_private_items
```

These feature checks do not replace the platform, transport or Loom checks.

These lints do not establish that an explanation is useful or correct. They also
omit some generated, hidden, tuple-field, local-item and trait-implementation code.
Review those contracts directly. Do not duplicate inherited trait documentation
unless the implementation adds a distinct requirement. Existing Errors, Panics,
Safety and empty-documentation checks remain necessary. Empty section headings
can satisfy a lint, but cannot satisfy a contract review.

### How CI selects checks

CI can reuse a passing run for unchanged inputs. It compares both sides of renames and checks whether the build reads a changed document.

| Change | Checks |
| --- | --- |
| Only recognized guides or agent skills | Check the documentation links and skills. Check dependencies against current security advisories. |
| Root README, alone or with those documents | Also build the package archives and run the downstream consumers, including both README examples. |
| Rust, CI, tooling, a benchmark protocol, an executable file or an unknown path | Run full CI. |

Tags, manual runs and changes without an applicable passing base run require full CI.
The `CI passed` job checks the outcome of every selected job. A skipped measurement is not a fresh performance result.
Branch protection and [release qualification](docs/maintainers/qualification.md) are separate requirements.

### Documentation acceptance

A documented symbol is not enough. Review the consumer's decisions as well as
the signature. For each affected capability, answer these questions in the API
docs or a linked guide:

1. What problem does it solve, and what is the usual starting choice?
2. When does the caller need an alternative, and what does that alternative cost or restrict?
3. Who owns the State, work and runtime? Which borrows and thread bounds matter?
4. What happens on success, rejection, cancellation, panic and drop?
5. Does a returned handle prove admission, commit, completion, durability or none of these?
6. Which test asserts the behaviour, and which real example demonstrates it?

Explain terms before relying on them. Include common mistakes, not only valid
calls. Keep decision guidance reachable from the README and the
[authoring index](docs/guides/authoring.md#find-a-decision). Keep detailed contracts
beside the API, with links instead of repeated descriptions that can diverge.

For a guide or agent skill, start with the offline link and package check. It needs Git, Ruby, and Lychee 0.24.2 on your `PATH`.

```sh
ruby scripts/check_public_guidance.rb
```

This checks local links, section links, and references in standalone copies of both skills.
It does not install the Codex plugin. For changed examples or API docs, run the affected doctests and build rustdoc with warnings denied.
Compare the advice with current source and tests. A successful build or link check cannot show whether a reader can follow the advice.

The independent consumer fixtures run Cargo offline with their own lockfiles.
On an isolated runner with a memory limit, download their dependencies before the workspace tests.
On macOS, run each compilation through the [memory watchdog](#build-memory-safety), not the block below.

```sh
rustup run "$toolchain" cargo fetch --locked --manifest-path crates/rca-core/tests/fixtures/modular-dependencies/Cargo.toml
rustup run "$toolchain" cargo fetch --locked --manifest-path crates/rca-core/tests/fixtures/rc07-owned-snapshot/Cargo.toml
rustup run "$toolchain" cargo test --locked --workspace --all-features
rustup run "$toolchain" cargo test --locked --workspace --all-features --doc
```

Then [build the API reference for this checkout](docs/maintainers/documentation-publishing.md#build-a-local-preview).
That recipe treats rustdoc warnings as errors and links the guides to the checkout's commit.

Start with [checks for the changed behaviour](docs/maintainers/qualification.md#start-with-the-changed-behaviour) during development. Before completion, run checks for the
affected integration boundaries. CI adds feature, platform, compiler-contract and
dependency checks. Release qualification also requires applicable performance and
100% executable-production coverage evidence. Test-only code has a separate audit scope.

### Coverage evidence

CI captures selected runtime tests on isolated Linux runners. This keeps large
compiler jobs off shared developer machines. Each group selects one package and
one feature configuration. Results remain separate because enabled features can
change implementation.

| Group | Selected tests |
| --- | --- |
| `core-minimal` | Core unit, actor, Local Store, command-port and removed-child tests with `local`, without `test-util` |
| `core-rich` | Core unit, mounted access, paths, composition and captured-modification tests with the rich Core features |
| `core-removal` | Removed-child, stack-removal, missing-child and ephemeral tests with the rich Core features |
| `core-timers-rich` | Timers, transactions, task receipts, named tasks, Local runtime and cold-listener registration tests with the rich Core features |
| `core-timers-minimal` | Core timer and transaction tests with `local` only |
| `core-reactions` | Changed reactions, reducer change callbacks, keyed collections, stack and shared-update tests with the rich Core features |
| `core-fold-commands` | Folds, recipe commands, command ports, input gates and acquisition-admission tests with the rich Core features |
| `core-observation-operations` | Observations, named operations, shared values, weak handles, captured graphs and owned snapshots with the rich Core features |
| `core-terminal` | Clocks, debounce, throttle, Stores, effect-send ownership, lifecycle, errors and terminal unwind with the rich Core features |
| `core-services` | Dependencies, application clients, issues, diagnostics, presentation and fallible effects with the rich Core features |
| `macros` | Macro unit and integration tests, plus compiler-stage profiles from the proc-macro library |
| `test-store` | Unit tests and ordinary native integration tests with `local` |
| `persistence-file` | Unit and integration tests for documents, file backends, restoration, key caches, subscriptions, shared sessions and quiescence with `file` |
| `tokio` | Tokio unit tests and host lifecycle, clock and panic tests |
| `listener-std` | Listener unit and integration tests with the standard-library backend |
| `listener-critical-section` | Listener unit and integration tests with the critical-section backend, without `std` |
| `listener-spin` | Listener backend-contention integration test without default features or `critical-section` |
| `listener-std-critical-section` | Listener public-surface integration tests with default features and `critical-section` |
| `strategy-std` | Listener strategy unit and integration tests with `std` |
| `channel-overflow-std` | Channel unit, bounded, unbounded and public-contract tests with default features, including unit-host overflow subprocesses |
| `channel-overflow-nostd` | The same channel hosts without default features, including unit-host overflow subprocesses |

The `test-store` group excludes the separate compiler-contract and panic-process hosts.
`compiler_contracts` uses a compiler harness, not libtest.
`panic_subprocess` has intentional failing and watchdog-killed children; the separate runtime-conformance suite checks their outcomes.

The `persistence-file` group runs the host-qualified unreadable-file test with `--include-ignored`.
Only its native file host receives this option.
The Linux runner must deny opening a mode-000 regular file.
The test fails if the host does not enforce that permission boundary.

The `listener-spin` group captures the fallback only for regression evidence.
It does not expand supported production configurations.
The listener recommends `std` or `critical-section` for production use.
Its contention window does not prove that a particular compare-and-swap retry or spin branch ran.
Those paths need positive native counters.

The two channel-overflow groups require `x86_64-unknown-linux-gnu`. They run the
full library test host with `--nocapture --test-threads=1`. Only these
groups add `-C llvm-args=-runtime-counter-relocation` and use the runtime profile
pattern `current-%p%c.profraw` for continuous counters across intentional aborts.
The library host requires six profiles: one parent and five boundary children. Capture checks
three successful exits and two `SIGABRT` outcomes, and rejects LLVM profiling
errors or warnings. Expected panic diagnostics remain in the retained output.
The integration hosts use the same compiler configuration and retain their own profiles.
They do not run the library's overflow subprocesses.
These are separate compiler configurations, not benchmark configurations.
Successful capture alone does not establish positive overflow-branch counters
or execution of the no-std abort and drop bodies.
Release review must check those source-bound records and retain any zeros or missing mappings.

The capture uses the repository toolchain and its matching LLVM tools. Dependency build outputs
are cached separately for each group. CI rebuilds workspace crates. Each artefact
records source and tool hashes, Cargo arguments, compiler configuration, test output,
executable objects, raw profiles and the LLVM export. Failed captures retain their available evidence.
Download required evidence before its seven-day retention period ends.

The `macros` group also retains the proc-macro dynamic library from Cargo's build record.
It exports compiler-stage profiles against that exact library, separately from test runtime profiles.
The capture checks each direct macro entry and each entry that a facade re-exports.
Each entry must record execution in its own source.
Passing macro unit tests alone do not prove that compiler-stage entry functions ran.
The same instrumented library also processes three existing rejection fixtures through direct compiler commands.
These cover malformed compiler-recovery identifiers and missing application dependencies without another Cargo build.
Each command must exit naturally with status 1 and the exact expected diagnostics.
Each command must produce a fresh profile with the original library's module signature.
Capture retains the commands, environments, source and library hashes, diagnostics and profile records.
These expected rejections do not satisfy the separate successful-build or macro-entry checks.
Macro test hosts remove `RUSTC_BOOTSTRAP` and the unstable branch flag from their child compiler environment.
This preserves stable compiler diagnostics. Their already-built parent binaries and the initial compiler-stage capture retain branch instrumentation.
Some macro tests run child Cargo and trybuild commands. Their host profile directories can contain
profiles from compilers with different flags; retain those inputs, but do not count them as
same-configuration production coverage. The separate compiler-stage export is the macro-entry check.

Capture requires Python 3.11 or later, the pinned `llvm-tools-preview` component
and at least 12 GiB of free disk space.
If a GitHub-hosted Linux runner lacks this space, CI removes only its unused Android SDK.
It keeps source files and Cargo caches.
If a disk check finds less than two GiB
free, capture stops the active command. Each parent test binary must report passing
tests without a test-name filter. Listener tests also run exact-filtered child
processes for isolation. Capture retains their summaries and checks them separately
from the full parent run. Listener integration parents run one test at a time so
child output does not overlap. Each test keeps its own concurrency checks.
Existing ignored tests remain visible in the retained summary.

The `core-terminal` effects host also runs one parent test at a time, with captured output visible.
Capture requires all eighteen successful exact-filtered child tests and nineteen process profiles, including the parent.
The parent still runs its full test list. Each child runs the same instrumented executable and retains its own profile.

The `core-timers-rich` lifecycle registration host uses the same isolation pattern.
Capture requires the complete parent test list, two successful exact-filtered children and three process profiles.
The separate assertion-listener host does not create child test processes.

A successful capture is not a 100% production-coverage result. Its raw data includes
test code and dependencies, and the selected groups do not cover every RCA package
or configuration. Release review must relate counts to executable production code,
retain uncovered and unmapped bodies, and audit test-only code separately. Do not
combine incompatible configurations or replace missing counts with passing-test totals.

On an isolated Linux host with the pinned tools installed:

```sh
python3 scripts/capture_coverage.py --group core-minimal --output target/coverage-evidence/core-minimal
python3 scripts/capture_coverage.py --group core-rich --output target/coverage-evidence/core-rich
```

Use a clean checkout and a new output directory for each capture. The helper rejects
macOS. It does not replace the local Cargo memory watchdog or limit a Linux host's
aggregate memory. Use an isolated runner or an OS-limited job. Instrumentation is
for coverage only, not native performance measurement.

Run process-wide allocation measurements in a `harness = false` test target.
Reuse the existing synchronous allocation runner. An isolated libtest subprocess
still has a receiver thread, even with `--test-threads=1`. That thread can add
unrelated allocations to the measured interval. Keep workload worker allocations
inside the measurement. Do not replace process-wide counts with thread-only counts.
Packaged crates must include their runner support without paths to sibling checkouts.

The full Python tooling suite checks for `cargo-bloat` 0.12.1 in the checkout's local tool directory.
On an isolated runner with an OS memory limit, install the tool from the repository root:

```sh
rustup run "$toolchain" cargo install cargo-bloat --version 0.12.1 --locked --root target/performance-tools
```

Run the tooling tests on that runner. The macOS watchdog does not support `cargo install`.
If you have only a shared Mac, use a remote job with an OS memory limit for the full suite.

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/tests
```

The prerequisite check does not run a performance capture or establish a timing baseline.

If a check cannot run, report the missing evidence and its cause. Do not replace
an unmet requirement with a completion claim.

CI and publication first build selected dependency documentation for inherited API links.
See [documentation publication](docs/maintainers/documentation-publishing.md) for the build order and signing requirements.

To publish the Rust packages, follow the [crate release guide](docs/maintainers/releasing.md).
Crates and documentation use separate workflows and credentials. After configuration, the documentation workflow publishes successful builds automatically.
