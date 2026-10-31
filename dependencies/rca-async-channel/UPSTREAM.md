# Upstream provenance

`rca-async-channel` 0.1.0 is an independent Rust Composable Architecture fork of [`async-channel` 2.5.0](https://crates.io/crates/async-channel/2.5.0), from [smol-rs/async-channel](https://github.com/smol-rs/async-channel). It is not an upstream smol-rs release; retaining the original author does not imply their maintenance or endorsement of the fork.

- Source archive: `async-channel-2.5.0.crate`.
- Archive SHA-256: `924ed96dd52d1b75e9c1a3e6275715fd320f5f9439fb5a4a11fa51f4221158d2`.
- Upstream commit recorded by the archive: `35a63c456aaa1906015f5a825e7e35505a749afa`. This is archive metadata, not independent verification of the Git repository.
- Original author: Stjepan Glavina; the manifest attribution is retained verbatim.
- Licence: `Apache-2.0 OR MIT`. `LICENSE-APACHE` and `LICENSE-MIT` are unchanged from the archive.
- `Cargo.toml.upstream` is the archive's original `Cargo.toml.orig`, preserved byte-for-byte. Cargo generates the fork's own `Cargo.toml.orig` during packaging.
- The original README remains intact after the fork notice. The original `CHANGELOG.md` describes upstream history, not fork releases.

## Fork changes

- Renamed the package and changed its version, repository, description and supported Rust minimum to 1.99.0. Retained edition 2021 and library name `async_channel`.
- Redirected the existing `event-listener-strategy` dependency key to package `rca-event-listener-strategy`, exact version `=0.1.0`, with a workspace-local path for development. Feature forwarding is unchanged.
- Consolidated four identical endpoint-count overflow checks into one private helper, preserving previous-count boundaries and atomic/Arc ordering. Added subprocess tests for both returning and aborting numeric boundaries; these do not fabricate channel state or claim public endpoint overflow.
- Use Rust 1.99's `try_update` for weak-endpoint increments, with unchanged closures and memory ordering. The `portable-atomic` feature retains that dependency's equivalent `fetch_update` operation.
- Added public channel ownership, weak-endpoint, closure and transport contract tests. Corrected upstream sender-count assertions that queried receiver counts.
- Added fork/provenance notices. Examples remain byte-for-byte upstream.
- Corrected `Sender::closed` documentation to include existing explicit-closure behaviour and documented internal endpoint ownership and retry state.
- Preserve close notifications for later wait sets when one waker panics. Four stack-owned guards retain notification order and propagate the original panic without allocating or requiring `std`.

Ordinary channel ordering and ownership are retained. Closure cleanup during unwinding and the transitive notifier graph are deliberate fork changes. Upstream provenance alone is not evidence of coverage, platform support or release readiness for that graph.
