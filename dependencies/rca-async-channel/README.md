# rca-async-channel

This is the Rust Composable Architecture fork of `async-channel` 2.5.0, not an upstream smol-rs release. Original authors and licences are retained; upstream authorship does not imply maintenance or endorsement of this fork. See [UPSTREAM.md](UPSTREAM.md) for provenance and changes.

The supported Rust minimum is 1.99.0; the edition remains 2021. Use the preserved library name through a dependency alias:

```toml
async-channel = { package = "rca-async-channel", version = "=0.1.0" }
```

The original README follows. Its badges, package links and release instructions refer to upstream, not this fork.

---

# async-channel

[![Build](https://github.com/smol-rs/async-channel/actions/workflows/ci.yml/badge.svg)](
https://github.com/smol-rs/async-channel/actions)
[![License](https://img.shields.io/badge/license-Apache--2.0_OR_MIT-blue.svg)](
https://github.com/smol-rs/async-channel)
[![Cargo](https://img.shields.io/crates/v/async-channel.svg)](
https://crates.io/crates/async-channel)
[![Documentation](https://docs.rs/async-channel/badge.svg)](
https://docs.rs/async-channel)

An async multi-producer multi-consumer channel, where each message can be received by only
one of all existing consumers.

There are two kinds of channels:

1. Bounded channel with limited capacity.
2. Unbounded channel with unlimited capacity.

A channel has the `Sender` and `Receiver` side. Both sides are cloneable and can be shared
among multiple threads.

When all `Sender`s or all `Receiver`s are dropped, the channel becomes closed. When a
channel is closed, no more messages can be sent, but remaining messages can still be received.

The channel can also be closed manually by calling `Sender::close()` or
`Receiver::close()`.

## Examples

```rust
let (s, r) = async_channel::unbounded();

assert_eq!(s.send("Hello").await, Ok(()));
assert_eq!(r.recv().await, Ok("Hello"));
```

## License

Licensed under either of

 * Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

#### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
