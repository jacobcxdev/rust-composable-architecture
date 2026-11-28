# Upstream provenance

`rca-event-listener-strategy` is an RCA fork of [`event-listener-strategy` 0.5.4](https://crates.io/crates/event-listener-strategy/0.5.4) from [smol-rs/event-listener-strategy](https://github.com/smol-rs/event-listener-strategy).
It is not a smol-rs release. Crediting the original author does not imply that the author maintains or endorses this fork.

- Source archive: `event-listener-strategy-0.5.4.crate`.
- Archive SHA-256: `8be9f3dfaaffdae2972880079a491a1a8bb7cbed0b8dd7a347f668b4150a3b93`.
- The archive records upstream commit `f533c437e8a6561e9b625c3ee036a8ea7d19375c`. This record is not an independent check of the Git repository.
- Original author: John Nunley. The manifest retains the original attribution verbatim.
- Licence: `Apache-2.0 OR MIT`. The fork keeps the archive copies of `LICENSE-APACHE` and `LICENSE-MIT` unchanged.
- `Cargo.toml.upstream` contains the exact bytes of the archive's `Cargo.toml.orig`. Cargo generates a new `Cargo.toml.orig` for the fork package.
- The fork notice precedes the original README. The original `CHANGELOG.md` describes upstream releases only.

## Fork changes

The fork keeps Rust edition 2021 and the library name `event_listener_strategy`. The minimum supported Rust version is 1.100.0.
RCA changed the package name, version, repository and description.

- The [manifest](Cargo.toml) pins the dependency key `event-listener` to the matching `rca-event-listener` release. Development builds use a local path. Cargo forwards the same features as before.
- Public tests cover pinned projections, borrowed wrapper output, listener handoff, and blocking and non-blocking polls. One test exercises a safe implementation that returns Pending once before completion.
- `easy_wrapper!` now keeps generic defaults on the wrapper declaration instead of copying them into generated `impl` headers. A consumer exercises default and explicit type arguments in asynchronous and blocking code.
- The documentation explains pinning, when each strategy consumes a listener, and how the blocking wait loop works. It also explains which calls panic on `None`.
- The blocking wait uses Rust's safe `pin!` macro to keep the future on the stack.
- The fork adds source notices and crate-qualified links. Documentation examples now specify their standard-library and WebAssembly requirements.

The fork keeps the upstream waiting protocol. The macro fix, tests and notification dependency differ from upstream.
RCA must qualify those changes for coverage, platform support and release.
