# Upstream provenance

`rca-async-channel` is an RCA fork of [`async-channel` 2.5.0](https://crates.io/crates/async-channel/2.5.0) from [smol-rs/async-channel](https://github.com/smol-rs/async-channel).
It is not a smol-rs release. Crediting the original author does not imply that the author maintains or endorses this fork.

- Source archive: `async-channel-2.5.0.crate`.
- Archive SHA-256: `924ed96dd52d1b75e9c1a3e6275715fd320f5f9439fb5a4a11fa51f4221158d2`.
- The archive records upstream commit `35a63c456aaa1906015f5a825e7e35505a749afa`. This record is not an independent check of the Git repository.
- Original author: Stjepan Glavina. The manifest retains the original attribution verbatim.
- Licence: `Apache-2.0 OR MIT`. The fork keeps the archive copies of `LICENSE-APACHE` and `LICENSE-MIT` unchanged.
- `Cargo.toml.upstream` contains the exact bytes of the archive's `Cargo.toml.orig`. Cargo generates a new `Cargo.toml.orig` for the fork package.
- The fork notice precedes the original README. The original `CHANGELOG.md` describes upstream releases only.

## Fork changes

The fork keeps Rust edition 2021 and the library name `async_channel`. The minimum supported Rust version is 1.100.0.
RCA changed the package name, version, repository and description. The code examples in `src/lib.rs` match the upstream archive byte for byte.

- The [manifest](Cargo.toml) pins the dependency key `event-listener-strategy` to the matching `rca-event-listener-strategy` release. Development builds use a local path. Cargo forwards the same features as before.
- One private helper replaces four identical overflow checks. The previous count still determines whether to abort. The change preserves atomic operations and reference-count ordering.
- Subprocess tests pass boundary values to the helper. They check which values return and which abort. They do not construct a channel with an impossible endpoint count.
- Weak endpoints use Rust 1.99's `try_update` to increase the count. The update closure and memory ordering remain the same. With `portable-atomic`, the fork uses the equivalent `fetch_update` operation.
- New tests cover channel ownership, weak endpoints, closure and transport behaviour. The upstream sender-count assertions now read the sender count instead of the receiver count.
- For Cargo 1.100, the manifest states the existing crate-level lint policy. The lint settings did not change.
- The `Sender::closed` documentation now covers explicit closure. Internal documentation explains endpoint ownership and retry state.
- Polling a completed send future reports why its payload is no longer available.
- If one waker panics during closure, four stack-owned guards preserve notification order for the remaining wait sets. They propagate the original panic without allocating or requiring `std`.

The fork keeps ordinary channel ordering and ownership. The cleanup that runs during a panic and the notification dependencies differ from upstream.
RCA must qualify those changes for coverage, platform support and release.
