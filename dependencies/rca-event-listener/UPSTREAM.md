# Upstream provenance

`rca-event-listener` 0.1.0 is an independent Rust Composable Architecture fork of [`event-listener` 5.4.2](https://crates.io/crates/event-listener/5.4.2), from [smol-rs/event-listener](https://github.com/smol-rs/event-listener). It is not an upstream smol-rs release; retaining the original authors does not imply their maintenance or endorsement of the fork.

- Source archive: `event-listener-5.4.2.crate`.
- Archive SHA-256: `5a23add41df1562121a9393cb065eab5146a1242410f23a644851e90cfd669d2`.
- Upstream commit recorded by the archive: `77004ac77dcef7884bf71a99c276a50f83a7fa4e`. This is archive metadata, not independent verification of the Git repository.
- Original authors: Stjepan Glavina and John Nunley; their manifest attribution is retained verbatim.
- Licence: `Apache-2.0 OR MIT`. `LICENSE-APACHE` and `LICENSE-MIT` are unchanged from the archive.
- `Cargo.toml.upstream` is the archive's original `Cargo.toml.orig`, preserved byte-for-byte. Cargo generates the fork's own `Cargo.toml.orig` during packaging.
- The original README remains intact after the fork notice. The original `CHANGELOG.md` describes upstream history, not fork releases.

## Fork changes

- Renamed the package and changed its version, repository, description and supported Rust minimum to 1.98.0. Retained edition 2021 and library name `event_listener`.
- Added `arrayvec` 0.7.8 for bounded inline deferred callback storage.
- Changed `src/lib.rs` and `src/intrusive.rs` to defer relevant waker operations until after notifier-lock release, drain deferred wakes safely during panic, publish accurate notification-count snapshots, correct the empty-list sentinel, and simplify private helper/trait code. This is not a claim of arbitrary tagged-callback reentrancy.
- Restricted private `GenericNotify` and `TagProducer` helpers in `src/notify.rs` to tests.
- Moved effective minimum/additional notification counting into the sealed notification policy, preserving cancellation flags, wrapper composition and callback ordering.
- Restricted negative-count validation to signed numeric implementations; all twelve numeric conversions retain checked overflow behaviour.
- Kept `Debug` non-blocking under the critical-section backend and simplified boolean option queries for the supported compiler's lint checks.
- Added notification-snapshot and deadline regressions, callback/panic/cancellation contracts, first-use/TLS tests, contended broadcast diagnostics, tagged async/blocking handoffs and bounded racing Loom models. These are ordinary Cargo test targets; their scope is documented in `tests/README.md`.
- Strengthened callback, wake, stack-slot and tag-panic tests to check the original diagnostic. Kept the legacy notification tests outside `cfg(loom)`, alongside the other native contract hosts; dedicated Loom models remain separate.
- Separated nested blocking waits from the cached parker and added return/unwind regressions. Corrected registration and notification guarantees, with a bounded Loom registration/recheck litmus.
- Added fork/provenance notices without removing upstream attribution.

All retained implementation and configuration surface belongs to the fork's maintenance and qualification scope. Upstream provenance alone is not evidence of coverage, platform support or release readiness for these modified sources.
