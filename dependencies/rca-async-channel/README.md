# rca-async-channel

Rust Composable Architecture (RCA) maintains this fork of `async-channel` 2.5.0.
It is not a smol-rs release. The manifest credits the upstream authors, and the original licences still apply.
This credit does not imply that the upstream authors maintain or endorse the fork.
[UPSTREAM.md](UPSTREAM.md) records the source and the changes in RCA.

The fork requires Rust 1.100.0 or later and uses Rust edition 2021.
To keep the upstream library name in your code, add the fork with an alias:

```sh
cargo add rca-async-channel --rename async-channel
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
