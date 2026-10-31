# Contributing to RCA

RCA is a headless-first architecture for composable, testable Rust programs.
Changes must preserve its ownership, cancellation and deterministic testing contracts.

## Proposing a change

For suspected vulnerabilities, use the private route in [SECURITY.md](SECURITY.md).
Do not put security details or credentials in public issues or pull requests.

For ordinary defects, include the revision, toolchain, enabled features and smallest
reproduction. Explain the expected result and the actual result. For API or runtime
changes, describe the headless workload and the contract that needs to change.
Check the [support policy](docs/support.md) before making compatibility claims.

Keep each pull request focused on one coherent change. Preserve applicable licence
and attribution notices. Explain any new dependency and its feature, platform and
licence effects. Do not include build output, credentials or temporary work files.
Keep required reproducibility evidence separate from disposable output.

Use signed Conventional Commits. Keep the subject shorter than 50 characters.
Use a past-tense summary, such as `fix: preserved cancellation ordering`.
After a blank line, list significant changes as bullets.

Use the [pull-request template](.github/pull_request_template.md) to record the
reason, compatibility effects and checks. State exactly which checks did not run.
Assign pull requests to `jacobcxdev`.

Before each push, compare the README, repository guidance, examples and workflow
documentation with the code. Correct obsolete instructions, versions and links.
Keep private repository references and temporary development-status notes out of public documentation.

The [CODEOWNERS file](.github/CODEOWNERS) names the responsible reviewer.
It does not establish required approval or branch protection. Maintainers must
check those remote settings separately before release. Local test results do not
prove that hosted checks passed. Code review does not authorise publication or
release actions.

## Repository protection

The default branch requires a pull request, verified commit signatures, resolved
review discussions and every required CI check from GitHub Actions.
The branch must include the current base revision. Administrators cannot bypass
these rules. Force pushes and deletion of the default branch are blocked.
Other published branches also reject force pushes.
Published tags cannot be changed or deleted.

Merge commits preserve the signed review commits. Squash, rebase and automatic
merge are disabled. The sole maintainer cannot approve their own pull request,
so GitHub does not require a numerical approval count.
The source-review requirements below still apply.

Actions defaults to read-only tokens and cannot approve pull requests.
External contributors require workflow approval. Remote actions must come from
the approved sources and use full commit SHAs. Documentation signing retains
its separate protected environment.

## Toolchain and formatting

The repository pins Rust, rustfmt, Clippy and `rust-src` through `rust-toolchain.toml`.
Compiler snapshots need `rust-src` for standard-library source excerpts.
The current toolchain is Rust 1.99.0. System installations of Cargo or rustc can
bypass rustup and ignore this file. Put the pinned tools first in `PATH`.
Select the compiler and documentation generator by absolute path, including when a compiler wrapper is active.

```sh
export PATH="$(dirname "$(rustup which --toolchain 1.99.0 cargo)"):$PATH"
export RUSTC="$(rustup which --toolchain 1.99.0 rustc)"
export RUSTDOC="$(rustup which --toolchain 1.99.0 rustdoc)"
rustup run 1.99.0 rustc --version
python3 scripts/check_rust_format.py
```

### Build memory safety

For local macOS builds, run repository compilation through the Cargo memory watchdog.
Use a new receipt path for each run:

```sh
mkdir -p target
python3 scripts/guard_cargo_memory.py --receipt target/clippy-memory.json -- \
  clippy --locked --workspace --all-targets --all-features -- -D warnings
```

The watchdog selects the pinned Cargo, rustc and rustdoc by absolute path.
It uses one Cargo job and disables compiler wrappers that can launch work in a separate cache daemon.
It permits only one guarded build per user at a time.

Every 250 milliseconds, it measures the owned process group's physical footprint, including compressed memory.
It stops that group at an observed eight GiB, on elevated memory pressure, or below four GiB of estimated host headroom.
Startup requires headroom for both the build budget and the host reserve.
Missing measurements also stop the build.

This is a polling watchdog, not a kernel-enforced memory cap.
Memory can grow between samples. File-backed pages in the headroom estimate are not necessarily immediately available.
The guard does not contain deliberate daemonisation or survive its own forced termination.

The composition controller uses the same watchdog for command groups, including child capture scripts.
Its receipts distinguish natural completion from forced cleanup.
Slow host telemetry does not delay memory checks.
For canonical qualification, choose an explicit timeout. Use the fixed `--qualify-performance` route.
The [performance commands](docs/performance/method.md#commands) show the full invocation.

Do not run a controller that creates separate process groups locally until each compiler process has equivalent protection.
Use an OS-limited remote job for large or unfamiliar builds.
Windows Job Objects and Linux container memory limits can enforce aggregate limits independently of the controller.

After a memory abort, inspect the receipt and reduce the build's memory demand.
Do not bypass the guard, raise its limit or retry the same command without a supported correction.
Keep measured benchmark profiles unchanged when adjusting correctness-only test profiles.
The direct Cargo commands below describe commands inside a guarded or OS-limited job.

These safeguards protect shared developer machines. GitHub-hosted CI uses disposable runners with separate memory allocations and job timeouts.
CI runs Cargo directly. It also tests the watchdog on macOS, but does not use the local watchdog limits for compilation.

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
[`9585e288`](https://github.com/microsoft/rust-guidelines/tree/9585e288d21d2da994a0550c926a09b62d79f8bd/src/guidelines).
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
For a focused all-features check, run:

```sh
rustup run 1.99.0 cargo clippy --locked --workspace --all-features --lib -- -D warnings -D missing_docs -D clippy::missing_docs_in_private_items
```

CI checks every combination of each library's features, using cargo-hack 0.6.45.
It excludes only the binary-only size probe from this library command.
The existing whole-workspace feature check still covers that binary.
Test-only bodies retain their separate source review.

The command below checks each library package separately. RCA uses Cargo
resolver 3, so a normal `--lib` build does not enable features supplied only by
development dependencies. Keep `--locked`; do not add `--no-dev-deps`, which
rewrites manifests and requires a different lockfile.

```sh
rustup run 1.99.0 cargo hack clippy --locked --workspace --exclude rca-size-probe --feature-powerset --lib -- -D warnings -D missing_docs -D clippy::missing_docs_in_private_items
```

These feature checks do not replace the platform, transport or Loom checks.

These lints do not establish that an explanation is useful or correct. They also
omit some generated, hidden, tuple-field, local-item and trait-implementation code.
Review those contracts directly. Do not duplicate inherited trait documentation
unless the implementation adds a distinct requirement. Existing Errors, Panics,
Safety and empty-documentation checks remain necessary. Empty section headings
can satisfy a lint, but cannot satisfy a contract review.

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

Run affected doctests and build documentation with warnings denied. Resolve
local Markdown links and section links. Reviewers must also compare the claims
with current source and tests. A formatter, documentation lint or successful
build cannot prove that a recommendation is correct or understandable.

The independent consumer fixtures run Cargo offline with their own lockfiles.
Before the workspace tests, download those dependencies:

```sh
rustup run 1.99.0 cargo fetch --locked --manifest-path rca-core/tests/fixtures/modular-dependencies/Cargo.toml
rustup run 1.99.0 cargo fetch --locked --manifest-path rca-core/tests/fixtures/rc07-owned-snapshot/Cargo.toml
rustup run 1.99.0 cargo test --locked --workspace --all-features
rustup run 1.99.0 cargo test --locked --workspace --all-features --doc
export RUSTDOCFLAGS='-D warnings'
rustup run 1.99.0 cargo doc --locked -p futures-core --no-deps
rustup run 1.99.0 cargo doc --locked -p rca-event-listener --all-features --no-deps
rustup run 1.99.0 cargo doc --locked --workspace --all-features --no-deps
```

Start with focused checks during development. Before completion, run checks for the
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
| `core-minimal` | Core unit, actor and Local Store tests with `local`, without `test-util` |
| `core-rich` | Core unit and integration tests with diagnostics, streams, Local and test support |
| `core-removal` | Removed-child integration tests with the rich Core features |
| `tokio` | Tokio unit tests and host lifecycle, clock and panic tests |
| `listener-std` | Listener unit and integration tests with the standard-library backend |
| `listener-critical-section` | Listener unit and integration tests with the critical-section backend, without `std` |
| `strategy-std` | Listener strategy unit and integration tests with `std` |

The capture uses Rust 1.99.0 and its matching LLVM tools. Dependency build outputs
are cached separately for each group. CI rebuilds workspace crates. Each artefact
records source and tool hashes, Cargo arguments, compiler configuration, test output,
executable objects, raw profiles and the LLVM export. Failed captures retain their available evidence.
Download required evidence before its seven-day retention period ends.

Capture requires Python 3.11 or later, the pinned `llvm-tools-preview` component
and at least 12 GiB of free disk space. If a disk check finds less than two GiB
free, capture stops the active command. Each parent test binary must report passing
tests without a test-name filter. Listener tests also run exact-filtered child
processes for isolation. Capture retains their summaries and checks them separately
from the full parent run. Listener integration parents run one test at a time so
child output does not overlap. Each test keeps its own concurrency checks.
Existing ignored tests remain visible in the retained summary.

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

The full Python tooling suite includes a real performance-tool preflight.
It requires `cargo-bloat` 0.12.1 in the checkout's local tool directory.
From the repository root, install that prerequisite before running the suite:

```sh
rustup run 1.99.0 cargo install cargo-bloat --version 0.12.1 --locked --root target/task4-tools
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/tests
```

The prerequisite check does not run a performance capture or establish a timing baseline.

If a check cannot run, report the missing evidence and its cause. Do not replace
an unmet requirement with a completion claim.

CI and publication first build selected dependency documentation for inherited
API links. See [documentation publication](docs/documentation-publishing.md) for the
complete build order and its
signing, secret-access and owner-approval requirements.
