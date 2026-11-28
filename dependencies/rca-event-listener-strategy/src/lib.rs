// SPDX-Licenser-Identifier: MIT OR Apache-2.0
// Modified for Rust Composable Architecture; see UPSTREAM.md for provenance and changes.
//! Shares one listener implementation between asynchronous and blocking callers.
//!
//! RCA maintains the `rca-event-listener-strategy` package, a fork of `event-listener-strategy` 0.5.4.
//! It is not a smol-rs release. See the [fork provenance] for its author and changes.
//! The Rust import is `event_listener_strategy`. After publication, use
//! `cargo add rca-event-listener-strategy --rename event-listener-strategy` to select this package.
//!
//! [fork provenance]: https://github.com/jacobcxdev/rust-composable-architecture/blob/e704e8bc61e19036138dbb565f764866797a0036/dependencies/rca-event-listener-strategy/UPSTREAM.md
//!
//! Implement [`EventListenerFuture`] with the re-exported [`event_listener`] types.
//! Wrap the listener in [`FutureWrapper`] to await it. On non-Wasm targets with the
//! default `std` feature, call `EventListenerFuture::wait` on the listener to block a thread instead.
//!
//! # Examples
//!
//! Register a listener before notifying it, then let the caller choose how to wait.
//! The asynchronous path needs no blocking support.
//!
//! ```
//! use event_listener_strategy::{
//!     event_listener::{Event, EventListener}, EventListenerFuture, FutureWrapper, Strategy,
//! };
//! use std::pin::Pin;
//! use std::task::Poll;
//!
//! struct Next(Option<EventListener>);
//!
//! impl EventListenerFuture for Next {
//!     type Output = ();
//!
//!     fn poll_with_strategy<'a, S: Strategy<'a>>(
//!         mut self: Pin<&mut Self>,
//!         strategy: &mut S,
//!         context: &mut S::Context,
//!     ) -> Poll<Self::Output> {
//!         strategy.poll(&mut self.0, context)
//!     }
//! }
//!
//! async fn notified() {
//!     let event = Event::new();
//!     let listener = Next(Some(event.listen()));
//!     event.notify(1);
//!     FutureWrapper::new(listener).await;
//! }
//! # futures_lite::future::block_on(notified());
//! ```
//!
//! On a native thread with `std` enabled, call `listener.wait()` instead of
//! awaiting the wrapper. Do not block an executor thread that must deliver the notification.

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(future_incompatible, missing_docs)]
#![doc(
    html_favicon_url = "https://raw.githubusercontent.com/smol-rs/smol/master/assets/images/logo_fullsize_transparent.png"
)]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/smol-rs/smol/master/assets/images/logo_fullsize_transparent.png"
)]

use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use event_listener::{EventListener, Listener};

#[doc(hidden)]
pub use pin_project_lite::pin_project;

#[doc(no_inline)]
pub use event_listener;

/// Defines a named [`Future`] around an [`EventListenerFuture`].
///
/// Use the generated private `_new` constructor inside the wrapper's module.
/// The generated `wait` method requires this crate's `std` feature and a non-Wasm target.
/// In a portable library, gate the method and each blocking call with the same
/// conditions. A feature named in the macro input belongs to the calling crate:
/// declare it there and forward it to this dependency's `std` feature.
///
/// # Examples
///
/// This native example uses the default features. It needs no runtime.
///
#[cfg_attr(all(feature = "std", not(target_family = "wasm")), doc = "```")]
#[cfg_attr(
    not(all(feature = "std", not(target_family = "wasm"))),
    doc = "```ignore"
)]
/// mod my_future {
///     use event_listener_strategy::{easy_wrapper, EventListenerFuture, Strategy};
///     use std::pin::Pin;
///     use std::task::Poll;
///
///     struct MyFuture;
///
///     impl EventListenerFuture for MyFuture {
///         type Output = ();
///
///         fn poll_with_strategy<'a, S: Strategy<'a>>(
///             self: Pin<&mut Self>,
///             _strategy: &mut S,
///             _context: &mut S::Context,
///         ) -> Poll<Self::Output> {
///             Poll::Ready(())
///         }
///     }
///
///     easy_wrapper! {
///         /// A future that completes on its first poll.
///         pub struct MyFutureWrapper(MyFuture => ());
///         /// Wait for it.
///         pub wait();
///     }
///
///     impl MyFutureWrapper {
///         /// Create a new instance of the future.
///         pub fn new() -> Self {
///             Self::_new(MyFuture)
///         }
///     }
/// }
///
/// use my_future::MyFutureWrapper;
///
/// fn main() {
///     MyFutureWrapper::new().wait();
/// }
///
/// // Use the future in a non-blocking context.
/// async fn run() {
///     MyFutureWrapper::new().await;
/// }
/// ```
#[macro_export]
macro_rules! easy_wrapper {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident

        $(<
            $( $lifetime:lifetime $(: $lifetime_bound:lifetime)? ),* $(,)?
            $( $generics:ident
                $(: $generics_bound:path)?
                $(: ?$generics_unsized_bound:path)?
                $(: $generics_lifetime_bound:lifetime)?
                $(= $generics_default:ty)?
            ),* $(,)?
        >)?

        ($inner:ty => $output:ty)

        $(where
            $( $where_clause_ty:ty
                $(: $where_clause_bound:path)?
                $(: ?$where_clause_unsized_bound:path)?
                $(: $where_clause_lifetime_bound:lifetime)?
            ),* $(,)?
        )?

        ;

        $(#[$wait_meta:meta])*
        $wait_vis: vis wait();
    ) => {
        $crate::pin_project! {
            $(#[$meta])*
            $vis struct $name $(<
                $( $lifetime $(: $lifetime_bound)? ),*
                $( $generics
                    $(: $generics_bound)?
                    $(: ?$generics_unsized_bound)?
                    $(: $generics_lifetime_bound)?
                    $(= $generics_default)?
                ),*
            >)? $(
                where
                $( $where_clause_ty
                    $(: $where_clause_bound)?
                    $(: ?$where_clause_unsized_bound)?
                    $(: $where_clause_lifetime_bound)?
                ),*
            )? {
                // Pins the strategy-aware future while exposing ordinary Future polling.
                #[pin]
                _inner: $crate::FutureWrapper<$inner>
            }
        }

        impl $(<
            $( $lifetime $(: $lifetime_bound)? ,)*
            $( $generics
                $(: $generics_bound)?
                $(: ?$generics_unsized_bound)?
                $(: $generics_lifetime_bound)?
            ),*
        >)? $name $(<
            $( $lifetime ,)*
            $( $generics ),*
        >)? $(
            where
            $( $where_clause_ty
                $(: $where_clause_bound)?
                $(: ?$where_clause_unsized_bound)?
                $(: $where_clause_lifetime_bound)?
            ),*
        )? {
            /// Wraps the inner future without polling or changing its state.
            #[inline]
            fn _new(inner: $inner) -> Self {
                Self {
                    _inner: $crate::FutureWrapper::new(inner)
                }
            }

            $(#[$wait_meta])*
            #[inline]
            $wait_vis fn wait(self) -> $output {
                use $crate::EventListenerFuture;
                self._inner.into_inner().wait()
            }

            /// Polls the same pinned future with the caller's selected waiting strategy.
            pub(crate) fn poll_with_strategy<'__strategy, __S: $crate::Strategy<'__strategy>>(
                self: ::core::pin::Pin<&mut Self>,
                strategy: &mut __S,
                context: &mut __S::Context,
            ) -> ::core::task::Poll<$output> {
                self.project()._inner.get_pin_mut().poll_with_strategy(strategy, context)
            }
        }

        impl $(<
            $( $lifetime $(: $lifetime_bound)? ,)*
            $( $generics
                $(: $generics_bound)?
                $(: ?$generics_unsized_bound)?
                $(: $generics_lifetime_bound)?
            ),*
        >)? ::core::future::Future for $name $(
            <
                $( $lifetime ,)*
                $( $generics ),*
            >
        )? $(
            where
            $( $where_clause_ty
                $(: $where_clause_bound)?
                $(: ?$where_clause_unsized_bound)?
                $(: $where_clause_lifetime_bound)?
            ),*
        )? {
            type Output = $output;

            #[inline]
            fn poll(
                self: ::core::pin::Pin<&mut Self>,
                context: &mut ::core::task::Context<'_>
            ) -> ::core::task::Poll<Self::Output> {
                self.project()._inner.poll(context)
            }
        }
    };
}

/// A future that runs using the [`event-listener`](crate::event_listener) crate.
///
/// This is similar to the [`Future`] trait from libstd, with one notable difference: it takes
/// a strategy that tells it whether to operate in a blocking or non-blocking context. The
/// `poll_with_strategy` method is the equivalent of the `poll` method in this regard; it uses
/// the [`Strategy`] trait to determine how to poll the future.
///
/// From here, there are two additional things one can do with this trait:
///
/// - With `std` on non-Wasm targets, `wait` blocks the current thread until the future is ready.
/// - The [`FutureWrapper`] type, which implements [`Future`] and uses the [`NonBlocking`]
///   strategy to poll the future.
pub trait EventListenerFuture {
    /// The type of value produced on completion.
    type Output;

    /// Poll the future using the provided strategy.
    ///
    /// Use [`Strategy::poll`] to wait for an active listener. This lets the
    /// caller choose whether to block its thread or return control to the executor.
    fn poll_with_strategy<'a, S: Strategy<'a>>(
        self: Pin<&mut Self>,
        strategy: &mut S,
        context: &mut S::Context,
    ) -> Poll<Self::Output>;

    /// Wait for the future to complete, blocking the current thread.
    ///
    /// This method requires the `std` feature and a non-Wasm target.
    ///
    /// The blocking strategy waits inside `Strategy::poll` and returns `Ready`.
    /// An implementation that returns `Pending` itself is polled again immediately,
    /// so it can cause a hot loop instead of waiting for a notification.
    #[cfg(all(feature = "std", not(target_family = "wasm")))]
    #[cfg_attr(docsrs, doc(cfg(all(feature = "std", not(target_family = "wasm")))))]
    fn wait(self) -> Self::Output
    where
        Self: Sized,
    {
        let mut this = core::pin::pin!(self);

        loop {
            if let Poll::Ready(res) = this
                .as_mut()
                .poll_with_strategy(&mut Blocking::default(), &mut ())
            {
                return res;
            }
        }
    }
}

pin_project_lite::pin_project! {
    /// A wrapper around an [`EventListenerFuture`] that implements [`Future`].
    ///
    /// [`Future`]: core::future::Future
    #[derive(Debug, Clone)]
    pub struct FutureWrapper<F: ?Sized> {
        // The wrapped future, structurally pinned with this wrapper.
        #[pin]
        inner: F,
    }
}

impl<F: EventListenerFuture> FutureWrapper<F> {
    /// Create a new `FutureWrapper` from the provided future.
    #[inline]
    pub fn new(inner: F) -> Self {
        Self { inner }
    }

    /// Consume the `FutureWrapper`, returning the inner future.
    #[inline]
    pub fn into_inner(self) -> F {
        self.inner
    }
}

impl<F: ?Sized> FutureWrapper<F> {
    /// Get a reference to the inner future.
    #[inline]
    pub fn get_ref(&self) -> &F {
        &self.inner
    }

    /// Get a mutable reference to the inner future.
    #[inline]
    pub fn get_mut(&mut self) -> &mut F {
        &mut self.inner
    }

    /// Get a pinned mutable reference to the inner future.
    #[inline]
    pub fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut F> {
        self.project().inner
    }

    /// Get a pinned reference to the inner future.
    #[inline]
    pub fn get_pin_ref(self: Pin<&Self>) -> Pin<&F> {
        self.project_ref().inner
    }
}

impl<F: EventListenerFuture> From<F> for FutureWrapper<F> {
    #[inline]
    fn from(inner: F) -> Self {
        Self { inner }
    }
}

impl<F: EventListenerFuture + ?Sized> Future for FutureWrapper<F> {
    type Output = F::Output;

    #[inline]
    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        self.project()
            .inner
            .poll_with_strategy(&mut NonBlocking::default(), context)
    }
}

/// A strategy for polling an [`EventListenerFuture`] or an [`EventListener`].
///
/// This trait is used by the [`EventListenerFuture::poll_with_strategy`] method to determine
/// how to poll the future. It can also be used standalone, by calling the [`Strategy::wait`]
/// method.
///
/// [`EventListenerFuture::poll_with_strategy`]: EventListenerFuture::poll_with_strategy
/// [`EventListener`]: crate::event_listener::EventListener
///
/// # Examples
///
/// ```
/// use event_listener_strategy::{
///    event_listener::{Event, EventListener},
///    EventListenerFuture, Strategy, NonBlocking
/// };
/// # #[cfg(all(feature = "std", not(target_family = "wasm")))]
/// use event_listener_strategy::Blocking;
/// use std::pin::Pin;
///
/// async fn wait_on<'a, S: Strategy<'a>>(evl: EventListener, strategy: &mut S) {
///     strategy.wait(evl).await;
/// }
///
/// # futures_lite::future::block_on(async {
/// let ev = Event::new();
/// // Block on the future.
/// # #[cfg(all(feature = "std", not(target_family = "wasm")))]
/// # {
/// let listener = ev.listen();
/// ev.notify(1);
///
/// wait_on(listener, &mut Blocking::default()).await;
/// # }
///
/// // Poll the future.
/// let listener = ev.listen();
/// ev.notify(1);
///
/// wait_on(listener, &mut NonBlocking::default()).await;
/// # });
/// ```
pub trait Strategy<'a> {
    /// The context needed to poll the future.
    type Context: ?Sized;

    /// The future returned by the [`Strategy::wait`] method.
    type Future: Future + 'a;

    /// Poll the event listener until it is ready.
    ///
    /// Pass an active listener in `Some`. The built-in strategies retain a pending
    /// listener and remove it after consuming its notification.
    ///
    /// # Panics
    ///
    /// The built-in strategies panic if `event_listener` is `None`.
    fn poll<T, L: Listener<T> + Unpin>(
        &mut self,
        event_listener: &mut Option<L>,
        context: &mut Self::Context,
    ) -> Poll<T>;

    /// Wait for the event listener to become ready.
    fn wait(&mut self, evl: EventListener) -> Self::Future;
}

/// A strategy that uses polling to efficiently wait for an event.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct NonBlocking<'a> {
    /// The type `&'a mut &'a T` is invariant over `'a`, like `Context` is.
    ///
    /// We used to just use `Context` here, but then `Context` became `!Send`
    /// and `!Sync`, making all of the futures that use this type `!Send` and
    /// `!Sync` as well. So we just take the lifetime invariance and none of
    /// the downsides.
    _marker: PhantomData<&'a mut &'a ()>,
}

impl<'a> Strategy<'_> for NonBlocking<'a> {
    type Context = Context<'a>;
    type Future = EventListener;

    #[inline]
    fn wait(&mut self, evl: EventListener) -> Self::Future {
        evl
    }

    #[inline]
    fn poll<T, L: Listener<T> + Unpin>(
        &mut self,
        event_listener: &mut Option<L>,
        context: &mut Self::Context,
    ) -> Poll<T> {
        let poll = Pin::new(
            event_listener
                .as_mut()
                .expect("`event_listener` should never be `None`"),
        )
        .poll(context);
        if poll.is_ready() {
            *event_listener = None;
        }
        poll
    }
}

/// A strategy that blocks the current thread until the event is signalled.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg(all(feature = "std", not(target_family = "wasm")))]
pub struct Blocking {
    /// Keeps construction behind `Default` without carrying runtime state.
    _private: (),
}

#[cfg(all(feature = "std", not(target_family = "wasm")))]
impl Strategy<'_> for Blocking {
    type Context = ();
    type Future = Ready;

    #[inline]
    fn wait(&mut self, evl: EventListener) -> Self::Future {
        evl.wait();
        Ready { _private: () }
    }

    #[inline]
    fn poll<T, L: Listener<T> + Unpin>(
        &mut self,
        event_listener: &mut Option<L>,
        _context: &mut Self::Context,
    ) -> Poll<T> {
        let result = event_listener
            .take()
            .expect("`event_listener` should never be `None`")
            .wait();
        Poll::Ready(result)
    }
}

/// A future that is always ready.
#[cfg(feature = "std")]
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct Ready {
    /// Marks completion without retaining the listener that supplied it.
    _private: (),
}

#[cfg(feature = "std")]
impl Future for Ready {
    type Output = ();

    #[inline]
    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(())
    }
}

#[test]
fn send_and_sync() {
    fn assert_send_and_sync<T: Send + Sync>() {}

    #[cfg(all(feature = "std", not(target_family = "wasm")))]
    {
        assert_send_and_sync::<Blocking>();
        assert_send_and_sync::<Ready>();
    }

    assert_send_and_sync::<NonBlocking<'static>>();
    assert_send_and_sync::<FutureWrapper<()>>();
}
