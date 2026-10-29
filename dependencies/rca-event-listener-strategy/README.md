# rca-event-listener-strategy

This is the Rust Composable Architecture fork of `event-listener-strategy` 0.5.4, not an upstream smol-rs release. Original authors and licences are retained; upstream authorship does not imply maintenance or endorsement of this fork. See [UPSTREAM.md](UPSTREAM.md) for provenance and changes.

The supported Rust minimum is 1.98.0; the edition remains 2021. Use the preserved library name through a dependency alias:

```toml
event-listener-strategy = { package = "rca-event-listener-strategy", version = "=0.1.0" }
```

The original README follows. Its badges, package links and release instructions refer to upstream, not this fork.

---

# event-listener-strategy

[![Build](https://github.com/smol-rs/event-listener-strategy/workflows/CI/badge.svg)](
https://github.com/smol-rs/event-listener-strategy/actions)
[![License](https://img.shields.io/badge/license-Apache--2.0_OR_MIT-blue.svg)](
https://github.com/smol-rs/event-listener-strategy)
[![Cargo](https://img.shields.io/crates/v/event-listener-strategy.svg)](
https://crates.io/crates/event-listener-strategy)
[![Documentation](https://docs.rs/event-listener-strategy/badge.svg)](
https://docs.rs/event-listener-strategy)

A strategy for using the [`event-listener`] crate in both blocking and non-blocking contexts.

One of the stand-out features of the [`event-listener`] crate is the ability to use it in both
asynchronous and synchronous contexts. However, sometimes using it like this causes a lot of
boilerplate to be duplicated. This crate aims to reduce that boilerplate by providing an `EventListenerFuture` trait that implements both blocking and non-blocking functionality.

[`event-listener`]: https://docs.rs/event-listener

# Examples

```
use event_listener::{Event, EventListener};
use event_listener_strategy::{EventListenerFuture, FutureWrapper, Strategy};

use std::pin::Pin;
use std::task::Poll;
use std::thread;
use std::sync::Arc;

// A future that waits three seconds for an event to be fired.
fn wait_three_seconds() -> WaitThreeSeconds {
    let event = Event::new();
    let listener = event.listen();

    thread::spawn(move || {
        thread::sleep(std::time::Duration::from_secs(3));
        event.notify(1);
    });

    WaitThreeSeconds { listener }
}

struct WaitThreeSeconds {
    listener: Pin<Box<EventListener>>,
}

impl EventListenerFuture for WaitThreeSeconds {
    type Output = ();

    fn poll_with_strategy<'a, S: Strategy<'a>>(
        mut self: Pin<&'a mut Self>,
        strategy: &mut S,
        context: &mut S::Context,
    ) -> Poll<Self::Output> {
        strategy.poll(self.listener.as_mut(), context)
    }
}

// Use the future in a blocking context.
let future = wait_three_seconds();
future.wait();

// Use the future in a non-blocking context.
futures_lite::future::block_on(async {
    let future = FutureWrapper::new(wait_three_seconds());
    future.await;
});
```

## License

Licensed under either of

 * Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or https://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or https://opensource.org/licenses/MIT)

at your option.

#### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
