# Upstream provenance

`rca-event-listener` is an RCA fork of [`event-listener` 5.4.2](https://crates.io/crates/event-listener/5.4.2) from [smol-rs/event-listener](https://github.com/smol-rs/event-listener).
It is not a smol-rs release. Crediting the original authors does not imply that they maintain or endorse this fork.

- Source archive: `event-listener-5.4.2.crate`.
- Archive SHA-256: `5a23add41df1562121a9393cb065eab5146a1242410f23a644851e90cfd669d2`.
- The archive records upstream commit `77004ac77dcef7884bf71a99c276a50f83a7fa4e`. This record is not an independent check of the Git repository.
- Original authors: Stjepan Glavina and John Nunley. The manifest retains the original attribution verbatim.
- Licence: `Apache-2.0 OR MIT`. The fork keeps the archive copies of `LICENSE-APACHE` and `LICENSE-MIT` unchanged.
- `Cargo.toml.upstream` contains the exact bytes of the archive's `Cargo.toml.orig`. Cargo generates a new `Cargo.toml.orig` for the fork package.
- The fork notice precedes the original README. The original `CHANGELOG.md` describes upstream releases only.

## Fork changes

The fork keeps Rust edition 2021 and the library name `event_listener`. The minimum supported Rust version is 1.100.0.
RCA changed the package name, version, repository and description.

- The fork uses `arrayvec` 0.7.8 for bounded inline storage of deferred callbacks.
- In `src/lib.rs` and `src/intrusive.rs`, the notifier releases its lock before it runs deferred waker operations. It drains deferred wakes even during a panic. This does not guarantee that a tag callback can call any notifier method before the callback returns.
- The fork corrected notification counts and the value that marks an empty list. It also simplified private helpers and traits.
- Dropping a completed, discarded or expired listener no longer locks the list. A listener that still occupies a slot keeps its cancellation handoff and callback order.
- `listener!` uses Rust's safe `pin!` macro to pin the slot on the stack before registration.
- Only tests use the private `GenericNotify` and `TagProducer` helpers in `src/notify.rs`.
- The sealed notification policy counts minimum and additional deliveries. It keeps the same cancellation flags, wrapper composition and callback order.
- Only signed numeric types need a check for negative counts. All twelve numeric conversions retain their overflow checks.
- Panic messages explain count overflow, a missing slot after insertion, and a timeout without a deadline.
- Under the critical-section backend, `Debug` does not block. The fork simplified boolean option queries to satisfy the compiler's lint checks.
- Tests cover notification counts, deadlines, callbacks, panics, cancellation, first use and thread-local storage. They cover broadcast contention and handoff of tagged notifications to asynchronous and blocking waiters. Loom models explore races under fixed limits.
- Callback, wake, stack-slot and tag-panic tests check the original diagnostic. The legacy notification tests run outside `cfg(loom)`. The dedicated Loom models run separately.
- Nested blocking waits no longer use the cached parker. Regression tests cover return and unwinding. The documentation clarifies registration and notification guarantees. A bounded Loom model checks that a listener does not miss a wake when it rechecks its condition after registration.

The [test guide](tests/README.md) describes the scope of these Cargo targets. RCA maintains and qualifies the fork's implementation and configuration.
The upstream tests alone do not establish coverage, platform support or release readiness for the fork.
