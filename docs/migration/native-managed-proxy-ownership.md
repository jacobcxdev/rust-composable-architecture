# Managed proxy native cleanup receipt

The test-only adapter in
[`managed_proxy_socket.rs`](../../examples/mcmd-server/tests/managed_proxy_socket.rs)
exercises the existing managed client and independently driven owner. Work began
at `ef935bfc9d1d3062622bd64559af5ff9727e7e37`. Production code is unchanged.

The client retains a pre-acquired IPv4 loopback TCP socket independently of its
event stream. A controlled asynchronous stop can fail without releasing that
socket, or drop it before acknowledging success. Three traces establish:

- Pending and failed stop retain a usable socket; explicit retry releases it
  before the reducer commits `Stopped`.
- Cancelling the RCA stop reply and closing the Store do not cancel admitted
  cleanup. The independently driven owner finishes with no pending recovery.
- Failed cleanup after Store closure returns the exact error and pending
  recovery. Explicit retry releases the retained socket.

Usability requires bidirectional byte exchange. Release requires a nonempty peer
read returning `Ok(0)`, not an acknowledgement, timeout or error. Every trace
retains an independent `Arc<Client>` through that read, preventing owner or
recovery destruction from concealing a missed stop. Event-stream closure is
checked separately.

Connect, accept, reads and writes have two-second bounds. Each asynchronous trace
has a 15-second watchdog; this cannot interrupt non-yielding synchronous work.
Verification adds an external command deadline, and CI bounds the socket step
at ten minutes.

## Verification

Rust 1.98.0 on `aarch64-apple-darwin`, locked offline dependencies:

- Deliberately acknowledging success without dropping the socket failed all
  three tests specifically at the independent peer EOF assertion (exit 101).
  The mutation was removed.
- The example package passed 82 tests before final lint-only adjustments.
- Strict package Clippy with all targets/features and `-D warnings` passed on
  final source. An initial invocation selected Rust 1.94 and failed the MSRV
  check; explicitly pinning `RUSTUP_TOOLCHAIN` corrected the toolchain selection.
- Parent independently ran both socket suites on final source: eight passed.
- Formatting, diff checks and workflow `actionlint` passed. Independent source
  review found no actionable issues.

The focused command was:

```sh
cargo test --offline --locked -p rca-example-mcmd-server \
  --test proxy_socket --test managed_proxy_socket -- --test-threads=1
```

| Local evidence | SHA-256 |
| --- | --- |
| `/tmp/rca-managed-socket-negative.log` | `8650b39d40b56630b6a897489ee76368ebb3d061832b5dd4f966b8ea8ace469e` |
| `/tmp/rca-managed-socket-package.log` | `829970e2c5b4e165139b7624e8281bde7331f8d9fd54b7513907d2fe1a1c71e5` |
| `/tmp/rca-managed-socket-clippy-final.log` | `7fad2e90bbcdcb08183cf5dd205d04c4daf8ddda7a855fdb492332803c05426d` |
| `/tmp/rca-managed-socket-parent.log` | `7ef2703837ad8dcde20a989efe2aa11e1d59ffeca23d625f18d6a8a39c501946` |
| Final test source | `a463ccc9314cb8072d81a2eb8a52111ca140a6d169279eaa5cdea5b2d9cea631` |

The native CI matrix now explicitly runs both socket suites. Configuration is
not evidence of successful execution on another platform.

### Native Linux ARM64 follow-up

The same eight tests passed at signed commit
`5588917ab62937c408f527f2c01deff05b3d8ad0` on native Linux ARM64 in the local
OrbStack VM. This was not cross-compilation or an emulated x86 run. The pinned
image was
`rust@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922`:
Debian 12, glibc 2.36, Rust/Cargo 1.98.0, host
`aarch64-unknown-linux-gnu`, Linux `7.0.14-orbstack-00380-ga7e0a2dc9535`.

A clean Git archive was extracted inside the container. Fresh private Cargo and
target directories, four build jobs, disabled incremental compilation and empty
compiler wrappers/flags prevented reuse of host build outputs. Locked dependency
fetch had a 600-second deadline; the offline focused command above added
`--target aarch64-unknown-linux-gnu` and a 900-second deadline. It exited zero
without retries. Archive comparison then confirmed tracked source was unchanged.
The disposable container and its private build/cache directories were removed
automatically; evidence remains at `/tmp/rca-linux-sockets-5588917.9hgvtr`.

| Evidence file in that directory | SHA-256 |
| --- | --- |
| `source.tar` | `e6018657262027f8a25ed5878fb79b8c41e5c7808142583089f1fe913380406c` |
| `run.sh` | `48082e89d630445aa1db25911ab42688384ad65939db6461ae3b3153bbcdba10` |
| `run.log` | `b4d9fcbbda26a010e1ac01eb7fa216423e42c2203ddb56e71ea6e95ce59a7c50` |
| `tests.log` | `e482e5f9ad8112f94ef5ebed352c6b31df87b53e984c1d47313f9df2e0cbf289` |
| `source-comparison.log` (empty, success) | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |

This follow-up qualifies these socket traces on Linux ARM64 only. Native Windows,
Linux x86-64 and macOS x86-64 execution are not established by it.

## Keyed native composition follow-up

At source baseline `289e5a50cfce48e64f2578c51661bf87c6958b51`, the socket
fixture gained one trace through `managed_servers::compose`. Two configured
owners retain separate pre-acquired sockets and stop gates. Existing single-owner
test bodies remain unchanged; private helpers accept an identity and distinct
probe bytes, and a test-only client dispatches by server identity.

The new trace proves these contracts together:

- Pending and failed alpha cleanup prevent membership removal. Exhaustive
  assertions preserve both children's complete state; both sockets still pass
  bidirectional byte exchanges after each attempted removal.
- Retrying alpha cleanup produces independent peer EOF before the keyed
  `Stopped` action is allowed to commit. Removing alpha leaves beta's state,
  event stream and native socket intact.
- Beta then stops with its own EOF witness. Both independently driven owners
  finish after Store closure with no pending recovery or retained error.

Independent client Arcs remain alive through all EOF checks. This closes the
selected-resource-versus-sibling integration gap between the earlier controlled
keyed tests and single-owner native tests; it is not a new library capability.

Verification on native aarch64 macOS with explicit Rust/Cargo 1.98.0:

- The new trace passed. An acknowledgement-only mutation failed at alpha's
  independent EOF read after its two-second I/O timeout (exit 101).
- A separate mutation also released beta after successful alpha cleanup. The
  trace rejected it when beta's round-trip found its socket missing (exit 101).
  This failure is a retained-resource assertion, not an additional EOF witness.
- Both mutations were restored. All 83 example-package tests passed with zero
  failures, ignores or compiler warnings. Strict all-target/all-feature package
  Clippy, workspace formatting and whitespace checks passed.
- Mutation commands had 120-second external deadlines; the final package suite
  had 240 seconds and Clippy 180 seconds. Existing per-I/O and async watchdog
  bounds also apply. Independent read-only review found no actionable issues
  and confirmed mutation restoration and preservation of existing test bodies.

| Local evidence | SHA-256 |
| --- | --- |
| Final `managed_proxy_socket.rs` | `5015871926957c89ddda85f48943623664c704f58469f50fe98a55aee48d1424` |
| `/tmp/rca-keyed-native-ack-negative.log` | `b975e63d32757df7aca504a1c652fcb8dc80fbc9f61fd04ebbb1a8584315ea24` |
| `/tmp/rca-keyed-native-sibling-negative.log` | `fe298f1b2e0ce3dafc443498fe07239897707330177931d62bc5292383ee56b1` |
| `/tmp/rca-keyed-native-package.log` | `dc8db9d2e71b64f6e505dee5c7057497e516665491960ccc76501694365ea9e1` |
| `/tmp/rca-keyed-native-clippy.log` | `24b257af61b8bb3db362c166e8a5c9232ecf8b29945e6aae20894f948ba343dd` |

The existing native CI step selects this test binary. Earlier Linux receipts
above cover the earlier eight socket tests only; the following run includes the
new keyed trace. No full-workspace rerun was needed for this test-only change;
no production code, dependency or public API changed.

### Keyed trace on native Linux ARM64

All nine socket tests passed at clean signed
`a43c7f6596317cfaa415734c7dec3eacf327ce13`, including the new keyed trace.
The run used the same pinned ARM64 image and native OrbStack environment
identified above: Debian 12, glibc 2.36, Rust/Cargo 1.98.0 and Linux
`7.0.14-orbstack-00380-ga7e0a2dc9535`. Host and image architecture were checked
as ARM64; rustc independently reported `aarch64-unknown-linux-gnu`.

The clean revision's Git archive was extracted inside a new disposable container.
Only the evidence directory was mounted. Cargo home and target were fresh private
directories, with four jobs, incremental compilation disabled and empty compiler
wrappers/flags. Locked fetch had a 600-second deadline; the existing two-socket-suite
command had a 900-second deadline and added `--target aarch64-unknown-linux-gnu`.
It ran offline with one test thread and exited zero without retries or warnings.

Run timestamps are `2026-10-01T10:22:54Z` to `2026-10-01T10:23:06Z`, not a
performance measurement. Archive comparison passed with empty output; a
separate host-side `git archive` reproduced the recorded archive hash.
The container was absent after its successful outer process exit, confirming
normal removal of its private build/cache directories. Evidence remains in
`/tmp/rca-linux-keyed-a43c7f6.mO46FP`.

| Evidence file | SHA-256 |
| --- | --- |
| `source.tar` | `3df9708b88eb8ef412ecfd588da6894a85611be04bcc2ed69a4baaf5f92a873d` |
| `run.sh` | `184ac69f21a692c3be86e949d8b22dbdb194c148cb92c6508064f0dc329f749b` |
| `launch.sh` | `9a467a1e7db69220081b926f09fe7ff488cf06ab487cf69dec287163c7fd7d4d` |
| `run.log` | `8cefeaa5641afd77c5a2fc25f7e986e61f7baa723a77f732ea49ed09a1954d2d` |
| `tests.log` | `9f8019b3502b69af48ef8c55084460f065154673d48235dcff3d944cc71cd12a` |
| `source-comparison.log` | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |

This extends the keyed native proof to Linux ARM64 only. Native Windows,
Linux x86-64 and macOS x86-64 execution remain unproved.

## Limits

This qualifies controlled local TCP cleanup through the managed adapter seam,
not asynchronous acquisition, a production proxy, remote cleanup,
durable recovery, process-crash recovery or complete application
migration. It complements the separate
[local RAII socket receipt](native-proxy-ownership.md); neither contract replaces
the other. No performance or general cleanup equivalence claim follows.
