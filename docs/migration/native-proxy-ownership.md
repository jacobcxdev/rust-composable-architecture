# Local proxy socket ownership receipt

Historical excerpts use `<owner-home>` for the original user's home directory.
They abbreviate local machine paths and are not current copy-and-paste commands.
Raw receipts are retained unchanged.

This test-only acceptance control exercises the existing `ProxyClient` seam in
`examples/mcmd-server/tests/proxy_socket.rs`. It does not implement a production
TCP adapter or change RCA's public API. The implementation started from
`3a84c03595ddb09d58bec146a431a2ce504b48f9`.

Each session owns one end of an ephemeral IPv4 loopback connection. The listener
closes immediately after acceptance. Dropping the session closes its socket and
then sends a release acknowledgement. The independent peer subsequently reads a
nonempty buffer and requires `Ok(0)`. A timeout, `WouldBlock`, unexpected data or
another I/O error is not accepted as EOF.

The five tests cover:

- Cancellation while an acquired session is withheld behind the readiness gate.
- Session stream completion: socket release and peer EOF occur while `Ended`
  remains intercepted before reduction.
- Keyed removal, fresh replacement and sibling isolation. Actual socket writes
  prove that the surviving sibling and replacement remain usable.
- Ordinary `Store` shutdown, independently observed socket EOF and explicit join
  of the test-owned Tokio host. The shutdown acknowledgement alone is not used
  as proof of resource closure.
- A negative control retaining an extra socket handle after acknowledgement.
  The EOF check rejects it; dropping the extra handle then permits EOF.

The socket is acquired by the harness before the readiness gate. This proves
ownership cancellation at the adapter seam, not cancellation-safe DNS or connect,
network-protocol handling, remote asynchronous shutdown, or production daemon
behaviour. The controlled session stream is not a TCP parser. This is local
application-adapter evidence exercised through RCA, not general resource-cleanup
qualification for arbitrary clients.

Reads, writes and coordination use three-second watchdogs; the deliberately open
negative-control peer uses a 100 ms read deadline. Owned host joins are protected
by the external 180-second command deadline rather than a claim that native
thread join itself is bounded. Tests use no external endpoints.

## Verification

Rust/Cargo 1.98.0, `aarch64-apple-darwin`, offline locked dependencies. All commands
ran in the worktree, with the pinned toolchain directory first in `PATH`,
`RUSTUP_TOOLCHAIN=1.98.0`, empty `RUSTC_WRAPPER` and
`RUSTC_WORKSPACE_WRAPPER`, and a separate target directory. The command prefix was:

```sh
PATH=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin:$PATH \
RUSTUP_TOOLCHAIN=1.98.0 RUSTC_WRAPPER= RUSTC_WORKSPACE_WRAPPER= \
CARGO_TARGET_DIR=/tmp/rca-native-proxy-target \
/opt/homebrew/bin/timeout --kill-after=5s 180s \
<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin/cargo \
test --offline --locked -p rca-example-mcmd-server --test proxy_socket
```

Removing `--test proxy_socket` ran the complete example package suite. Strict
lint used target directory `/tmp/rca-native-proxy-lint` and arguments
`clippy --offline --locked -p rca-example-mcmd-server --all-targets --all-features -- -D warnings`.

| Receipt | Result | Log SHA-256 |
| --- | --- | --- |
| `/tmp/rca-native-proxy-red.log` | Expected exit 101: acknowledgement-only implementation incorrectly accepted the retained handle | `c317ccf2cee1cb0fd80520516ae6bf40b3ef5cb31a1b166a25aa83a399596f3b` |
| `/tmp/rca-native-proxy-package.log` | Exit 0, 58 tests passed | `5ddbdebda2bceac975fbd9ecd48313b4d426de32e71999db4fd1ff4dc9d3ded0` |
| `/tmp/rca-native-proxy-final.log` | Exit 0, all five socket tests passed after lint-only adjustments | `187a882ecd99c85b8827da7511ccc4d30fff18be638c335e10a62bc433aa5d2a` |
| `/tmp/rca-native-proxy-clippy-final.log` | Exit 0, strict package lint | `6b5ec335e1f11337588aa7dc81d0023f3426f9576379bbec0e3dbc17ce96ca56` |

The initial lint run failed on seven test-only style diagnostics and is retained
at `/tmp/rca-native-proxy-clippy.log`; those diagnostics were corrected. The core
build script's existing `rust-objcopy` missing `@rpath/libLLVM.dylib` warning also
remains visible in the build logs. Pinned `rustfmt --edition 2024 --check` for the
new Rust file and `git diff --check` passed.

The frozen test file SHA-256 is
`2409ce5e905f048cfff7274dee12738186e6bb3361c3f40f1c79e526271da982`.
Independent source and receipt review found no actionable issues. Parent
independently reran all five focused socket tests; they passed. Parent log:
`/tmp/rca-native-proxy-parent.log`, SHA-256
`c73d8a3197a382cf6872d4a835435fe2a2b7d2db836cc5bcf8f14ad7860490db`.
No performance or non-macOS qualification is claimed.
