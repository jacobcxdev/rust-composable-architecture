# Regression suite

Run from the RCA workspace root. On macOS, run stable tests through the
[build memory guard](../../../CONTRIBUTING.md#build-memory-safety). Give each run a new receipt:

```sh
mkdir -p target
run_dir=$(mktemp -d target/listener-tests.XXXXXXXX)
python3 scripts/guard_cargo_memory.py --receipt "$run_dir/stable.json" -- test --locked -p rca-event-listener --tests
```

On an isolated Linux runner with an OS memory limit for the whole job, check
each stable configuration separately:

```sh
rustup run 1.100.0 cargo test --locked -p rca-event-listener --tests
rustup run 1.100.0 cargo test --locked -p rca-event-listener --no-default-features --tests
rustup run 1.100.0 cargo test --locked -p rca-event-listener --features critical-section,portable-atomic --tests
RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=2 LOOM_MAX_BRANCHES=10000 rustup run 1.100.0 cargo test --locked -p rca-event-listener --features loom --test loom --test racing_loom -- --test-threads=1
```

For Miri, use a separate Linux job with the same memory limit and an external
timeout. Install the `miri` component of `nightly-2026-08-23` there. The macOS
guard does not support Miri. CI uses this nightly for `rca-core`; those tests
do not run the listener fork's own suite.

```sh
MIRIFLAGS="-Zmiri-strict-provenance" rustup run nightly-2026-08-23 cargo miri test --locked -p rca-event-listener --tests -- --test-threads=1
```

## Where the tests run

When you set `cfg(loom)`, select the two Loom targets shown above. The other
targets run outside Loom. The race models use two threads, at most 1,000
branches and at most two preemptions. They set no internal time limit or limit
on the number of schedules. Their results apply only within those bounds.

The callback, public API, panic, blocking and first-use tests need `std` and a
native host. With the `critical-section` feature, they exercise that backend
using the host's development dependency. Without `std`, the upstream notify
tests still run. The upstream Wasm tests are separate. Native tests do not
establish Wasm support. Panic recovery tests need unwinding, not panic-abort.

Native callback and panic tests run in child processes. Each child runs one
test by its exact name and inherits the parent's output streams. At timeout,
the parent kills the child and waits for it to exit. Miri cannot start a child
process and runs those same test bodies directly. In CI, set an external
timeout for Miri.

## What the tests check

Keep the first-use target in a separate executable. It uses its own allocator
to control when two initial allocations compete and checks who owns each one.
The public API does not promise a particular allocation count. Another test
checks that waits and cleanup succeed during thread-local storage (TLS)
teardown. The platform determines which TLS fallback runs. Check coverage
separately before claiming that a particular branch ran.

The wake-panic tests use 1, 2, 32, 33 and 40 listeners. They trigger panics
before, within and after inline storage. The tag-panic tests cover a partial
commit with two listeners. With 40 listeners, they trigger panics at indices
0, 1, 31, 32, 33 and 39. Each passing case checks recovery and releases its
listeners normally.

The backend-contention test uses a separate allocator to pause a real overflow
allocation during broadcast. On the mutex and spinlock backends, it checks that
diagnostics do not block and delivery succeeds while another thread registers.
Only native coverage counters can show whether the test reached a specific
compare-and-swap or spin branch. On the critical-section backend, an allocator
callback checks diagnostics on the same thread during a mutable borrow of the
list. Tests with tagged notifications check diagnostics, waker replacement,
expiry and handoff to blocking waiters, including on the critical-section backend.

The notification tests distinguish a minimum total from a request for
additional deliveries. They cover outstanding notifications, nested wrappers,
tag generation, cancellation and a panic partway through tagging.
The basic count test also runs without the `std` feature. Passing these tests
does not show which lines ran for each generic type or backend.
For that, use coverage counters from the exact source under test.
