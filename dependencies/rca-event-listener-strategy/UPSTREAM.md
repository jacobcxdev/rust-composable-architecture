# Upstream provenance

`rca-event-listener-strategy` 0.1.0 is an independent Rust Composable Architecture fork of [`event-listener-strategy` 0.5.4](https://crates.io/crates/event-listener-strategy/0.5.4), from [smol-rs/event-listener-strategy](https://github.com/smol-rs/event-listener-strategy). It is not an upstream smol-rs release; retaining the original author does not imply their maintenance or endorsement of the fork.

- Source archive: `event-listener-strategy-0.5.4.crate`.
- Archive SHA-256: `8be9f3dfaaffdae2972880079a491a1a8bb7cbed0b8dd7a347f668b4150a3b93`.
- Upstream commit recorded by the archive: `f533c437e8a6561e9b625c3ee036a8ea7d19375c`. This is archive metadata, not independent verification of the Git repository.
- Original author: John Nunley; the manifest attribution is retained verbatim.
- Licence: `Apache-2.0 OR MIT`. `LICENSE-APACHE` and `LICENSE-MIT` are unchanged from the archive.
- `Cargo.toml.upstream` is the archive's original `Cargo.toml.orig`, preserved byte-for-byte. Cargo generates the fork's own `Cargo.toml.orig` during packaging.
- The original README remains intact after the fork notice. The original `CHANGELOG.md` describes upstream history, not fork releases.

## Fork changes

- Renamed the package and changed its version, repository, description and supported Rust minimum to 1.99.0. Retained edition 2021 and library name `event_listener_strategy`.
- Redirected the existing `event-listener` dependency key to package `rca-event-listener`, exact version `=0.1.0`, with a workspace-local path for development. Feature forwarding is unchanged.
- Added public contract tests for pinned projections, borrowed wrapper output, listener handoff and blocking/non-blocking polling, including the safe trait's unusual one-shot Pending fallback.
- Fixed `easy_wrapper!` to keep generic defaults on the wrapper declaration, not its generated `impl` headers. A public consumer checks default and explicit type arguments with asynchronous and blocking execution.
- Documented internal pinning, listener consumption and the blocking wait loop. Clarified the built-in strategies' `None` panic boundary.
- Added fork/provenance notices, crate-qualified documentation links and standard-library/WebAssembly gates around affected documentation examples.

The waiting protocol is retained, but the macro correction, tests and notifier dependency are deliberate fork changes. Upstream provenance alone is not evidence of coverage, platform support or release readiness for the forked dependency graph.
