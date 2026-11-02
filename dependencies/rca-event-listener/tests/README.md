# Regression suite

From the RCA workspace root:

```sh
cargo test -p rca-event-listener --tests
cargo test -p rca-event-listener --no-default-features --tests
cargo test -p rca-event-listener --features critical-section,portable-atomic --tests
RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=2 LOOM_MAX_BRANCHES=10000 cargo test -p rca-event-listener --features loom --test loom --test racing_loom -- --test-threads=1
MIRIFLAGS="-Zmiri-strict-provenance" cargo +nightly miri test -p rca-event-listener --tests -- --test-threads=1
```

The normal targets run outside Loom. The two Loom targets run within its model;
select them explicitly when setting `cfg(loom)`. The added racing models use two
threads, a 1,000-branch bound and preemption bound two, with no internal time or
permutation cutoff. They do not prove arbitrary schedules.

Callback, public-surface, panic, blocking and first-use tests require `std` APIs
and a native host. They still exercise the critical-section backend when that
feature is enabled. The host dev dependency supplies its critical-section
implementation. No-std builds retain the upstream notify tests. The native-only
tests are not claimed as Wasm validation; existing upstream Wasm tests remain.
Panic recovery tests require unwinding rather than panic-abort execution.

Native callback and panic regressions run in bounded child processes. Each child
must match its exact test name; output is inherited and a timed-out child is
killed and reaped. Miri executes those same bodies directly because it cannot
spawn a process, so use an external timeout for Miri in CI.

The first-use target has its own scheduling allocator. It checks ownership of
the current implementation's two competing initial allocations, not a promised
public allocation budget. It must remain a separate executable. Its TLS test
proves waits and cleanup succeed during teardown; entering a particular TLS
fallback branch is platform-dependent and requires a separate coverage witness.

The wake-panic matrix covers 1, 2, 32, 33 and 40 listeners and panic positions
before, within and after inline storage. The tag-panic matrix covers the
two-listener partial-commit case and 40-listener indices 0, 1, 31, 32, 33 and 39.
Every successful case checks recovery and releases its listeners normally.

The backend-contention target uses a separate scheduling allocator to hold a
real overflow allocation during broadcast. It verifies non-blocking diagnostics
and delivery while registration contends, on mutex and spinlock backends. Its
finite scheduling window alone does not prove that a particular private CAS or
spin branch executed; that requires native coverage counters. The critical-section
backend instead runs a same-thread allocator callback that checks diagnostics
while the list has an active mutable borrow. Tagged generic contracts also check
diagnostics, waker replacement, expiry and blocking handoff, including the
critical-section backend.

Notification-policy contracts distinguish minimum from additional delivery with
outstanding notifications, nested wrappers, tag generation, cancellation and
partial tag panic. The basic count contract also runs without the std feature.
These semantic checks do not establish every generic instantiation or backend's
coverage; those require separate source-bound counter reports.
