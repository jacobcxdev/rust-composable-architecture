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

The repository pins Rust, rustfmt and Clippy through `rust-toolchain.toml`.
The current toolchain is Rust 1.98.0. System installations of Cargo or rustc can
bypass rustup and ignore this file. Put the pinned tools first in `PATH`, so
nested compiler processes also use the correct version.

```sh
export PATH="$(dirname "$(rustup which --toolchain 1.98.0 cargo)"):$PATH"
rustup run 1.98.0 rustc --version
python3 scripts/check_rust_format.py
rustup run 1.98.0 cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
```

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
`clippy::missing_docs_in_private_items`. Rust 1.98 Clippy skips the private check
for an entire test-harness build. Include a normal library build, not only tests.
For a focused all-features check, run:

```sh
rustup run 1.98.0 cargo clippy --locked --workspace --all-features --lib -- -D warnings -D missing_docs -D clippy::missing_docs_in_private_items
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
rustup run 1.98.0 cargo hack clippy --locked --workspace --exclude rca-size-probe --feature-powerset --lib -- -D warnings -D missing_docs -D clippy::missing_docs_in_private_items
```

These feature checks do not replace the platform, transport or Loom checks.

These lints do not establish that an explanation is useful or correct. They also
omit some generated, hidden, tuple-field, local-item and trait-implementation code.
Review those contracts directly. Do not duplicate inherited trait documentation
unless the implementation adds a distinct requirement. Existing Errors, Panics,
Safety and empty-documentation checks remain necessary. Empty section headings
can satisfy a lint, but cannot satisfy a contract review.

The independent consumer fixtures run Cargo offline with their own lockfiles.
Before the workspace tests, download those dependencies:

```sh
rustup run 1.98.0 cargo fetch --locked --manifest-path rca-core/tests/fixtures/modular-dependencies/Cargo.toml
rustup run 1.98.0 cargo fetch --locked --manifest-path rca-core/tests/fixtures/rc07-owned-snapshot/Cargo.toml
rustup run 1.98.0 cargo test --locked --workspace --all-features
rustup run 1.98.0 cargo test --locked --workspace --all-features --doc
export RUSTDOCFLAGS='-D warnings'
rustup run 1.98.0 cargo doc --locked -p futures-core --no-deps
rustup run 1.98.0 cargo doc --locked -p rca-event-listener --all-features --no-deps
rustup run 1.98.0 cargo doc --locked --workspace --all-features --no-deps
```

Start with focused checks during development. Before completion, run checks for the
affected integration boundaries. CI adds feature, platform, compiler-contract and
dependency checks. Release qualification also requires applicable performance and
100% executable-production coverage evidence. Test-only code has a separate audit scope.

The full Python tooling suite includes a real performance-tool preflight.
It requires `cargo-bloat` 0.12.1 in the checkout's local tool directory.
From the repository root, install that prerequisite before running the suite:

```sh
rustup run 1.98.0 cargo install cargo-bloat --version 0.12.1 --locked --root target/task4-tools
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/tests
```

The prerequisite check does not run a performance capture or establish a timing baseline.

If a check cannot run, report the missing evidence and its cause. Do not replace
an unmet requirement with a completion claim.

CI and publication first build selected dependency documentation for inherited
API links. See [documentation publication](docs/documentation-publishing.md) for the
complete build order and its
signing, secret-access and owner-approval requirements.
