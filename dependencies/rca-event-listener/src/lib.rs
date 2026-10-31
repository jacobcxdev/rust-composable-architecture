// Modified for Rust Composable Architecture; see UPSTREAM.md for provenance and changes.

//! Notify async tasks or threads.
//!
//! This is a synchronization primitive similar to [eventcounts] invented by Dmitry Vyukov.
//!
//! You can use this crate to turn non-blocking data structures into async or blocking data
//! structures. See a [simple mutex] implementation that exposes an async and a blocking interface
//! for acquiring locks.
//!
//! [eventcounts]: https://www.1024cores.net/home/lock-free-algorithms/eventcounts
//! [simple mutex]: https://github.com/smol-rs/event-listener/blob/master/examples/mutex.rs
//!
//! # Examples
//!
//! Wait until another thread sets a boolean flag:
//!
#![cfg_attr(feature = "std", doc = "```")]
#![cfg_attr(not(feature = "std"), doc = "```no_compile")]
//! # #[cfg(not(target_family = "wasm"))] { // Listener::wait is unavailable on WASM
//! use std::sync::atomic::{AtomicBool, Ordering};
//! use std::sync::Arc;
//! use std::thread;
//! use std::time::Duration;
//! use event_listener::{Event, Listener};
//!
//! let flag = Arc::new(AtomicBool::new(false));
//! let event = Arc::new(Event::new());
//!
//! // Spawn a thread that will set the flag after 1 second.
//! thread::spawn({
//!     let flag = flag.clone();
//!     let event = event.clone();
//!     move || {
//!         // Wait for a second.
//!         thread::sleep(Duration::from_secs(1));
//!
//!         // Set the flag.
//!         flag.store(true, Ordering::SeqCst);
//!
//!         // Notify all listeners that the flag has been set.
//!         event.notify(usize::MAX);
//!     }
//! });
//!
//! // Wait until the flag is set.
//! loop {
//!     // Check the flag.
//!     if flag.load(Ordering::SeqCst) {
//!         break;
//!     }
//!
//!     // Start listening for events.
//!     let mut listener = event.listen();
//!
//!     // Check the flag again after creating the listener.
//!     if flag.load(Ordering::SeqCst) {
//!         break;
//!     }
//!
//!     // Wait for a notification and continue the loop.
//!     listener.wait();
//! }
//! # }
//! ```
//!
//! # Features
//!
//! - The `std` feature (enabled by default) enables the use of the Rust standard library. Disable it for `no_std`
//!   support.
//!
//! - The `critical-section` feature enables usage of the [`critical-section`] crate to enable a
//!   more efficient implementation of `event-listener` for `no_std` platforms.
//!
//! - The `portable-atomic` feature enables the use of the [`portable-atomic`] crate to provide
//!   atomic operations on platforms that don't support them.
//!
//! In production environments, at least one of `std` or `critical-section` should be
//! enabled. This ensures that the internal locking mechanism has a critical section of some
//! kind to fall back on. Otherwise, it falls back to a spinlock implementation. This
//! implementation is [dangerous] to rely on.
//!
//! [`critical-section`]: https://crates.io/crates/critical-section
//! [`portable-atomic`]: https://crates.io/crates/portable-atomic
//! [dangerous]: https://matklad.github.io/2020/01/02/spinlocks-considered-harmful.html

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::multiple_bound_locations)] // This is a WONTFIX issue with pin-project-lite
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]
#![doc(
    html_favicon_url = "https://raw.githubusercontent.com/smol-rs/smol/master/assets/images/logo_fullsize_transparent.png"
)]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/smol-rs/smol/master/assets/images/logo_fullsize_transparent.png"
)]

#[cfg(not(feature = "std"))]
extern crate alloc;
#[cfg(feature = "std")]
extern crate std as alloc;

#[path = "intrusive.rs"]
mod sys;

mod notify;

#[cfg(not(feature = "std"))]
use alloc::boxed::Box;

use core::borrow::Borrow;
use core::fmt;
use core::future::Future;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::ptr;
use core::task::{Context, Poll, Waker};

#[cfg(all(feature = "std", not(target_family = "wasm")))]
use {
    parking::{Parker, Unparker},
    std::time::{Duration, Instant},
};

use sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use sync::Arc;

#[cfg(not(loom))]
use sync::WithMut;

use notify::NotificationPrivate;
pub use notify::{IntoNotification, Notification};

/// Inner state of [`Event`].
struct Inner<T> {
    /// Snapshot of the number of notified listeners.
    notified: AtomicUsize,

    /// Inner queue of event listeners.
    ///
    /// Every backend uses an intrusive linked list. A mutex, critical section or
    /// spinlock protects its links and listener states.
    list: sys::List<T>,
}

impl<T> Inner<T> {
    /// Creates an empty listener list with no notified entries.
    fn new() -> Self {
        Self {
            notified: AtomicUsize::new(0),
            list: sys::List::new(),
        }
    }
}

/// A synchronization primitive for notifying async tasks and threads.
///
/// Listeners can be registered using [`Event::listen()`]. There are two ways to notify listeners:
///
/// 1. [`Event::notify()`] notifies a number of listeners.
/// 2. [`Event::notify_additional()`] notifies a number of previously unnotified listeners.
///
/// If there are no active listeners at the time a notification is sent, it simply gets lost.
///
/// There are two ways for a listener to wait for a notification:
///
/// 1. In an asynchronous manner using `.await`.
/// 2. In a blocking manner by calling [`EventListener::wait()`] on it.
///
/// If a notified listener is dropped without receiving a notification, dropping will notify
/// another active listener. Whether one *additional* listener will be notified depends on what
/// kind of notification was delivered.
///
/// Listeners are registered and notified in the first-in first-out fashion, ensuring fairness.
pub struct Event<T = ()> {
    /// A pointer to heap-allocated inner state.
    ///
    /// This pointer is initially null and gets lazily initialized on first use. Semantically, it
    /// is an `Arc<Inner>` so it's important to keep in mind that it contributes to the [`Arc`]'s
    /// reference count.
    inner: AtomicPtr<Inner<T>>,
}

// SAFETY: The atomic pointer owns an Arc reference and only publishes initialised
// state. The list lock serialises access to tags; moving a tag requires T: Send.
unsafe impl<T: Send> Send for Event<T> {}
unsafe impl<T: Send> Sync for Event<T> {}

impl<T> core::panic::UnwindSafe for Event<T> {}
impl<T> core::panic::RefUnwindSafe for Event<T> {}

impl<T> fmt::Debug for Event<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_inner() {
            Some(inner) => {
                let notified_count = inner.notified.load(Ordering::Relaxed);
                let total_count = match inner.list.try_total_listeners() {
                    Some(total_count) => total_count,
                    None => {
                        return f
                            .debug_tuple("Event")
                            .field(&format_args!("<locked>"))
                            .finish()
                    }
                };

                f.debug_struct("Event")
                    .field("listeners_notified", &notified_count)
                    .field("listeners_total", &total_count)
                    .finish()
            }
            None => f
                .debug_tuple("Event")
                .field(&format_args!("<uninitialized>"))
                .finish(),
        }
    }
}

impl Default for Event {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Event<T> {
    /// Creates a new `Event` with a tag type.
    ///
    /// Tagging cannot be implemented efficiently on `no_std`, so this is only available when the
    /// `std` feature is enabled.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::<usize>::with_tag();
    /// ```
    #[cfg(all(feature = "std", not(loom)))]
    #[inline]
    pub const fn with_tag() -> Self {
        Self {
            inner: AtomicPtr::new(ptr::null_mut()),
        }
    }
    /// Creates a new `Event` with a tag type.
    #[cfg(all(feature = "std", loom))]
    #[inline]
    pub fn with_tag() -> Self {
        Self {
            inner: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Tell whether any listeners are currently notified.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::{Event, Listener};
    ///
    /// let event = Event::new();
    /// let listener = event.listen();
    /// assert!(!event.is_notified());
    ///
    /// event.notify(1);
    /// assert!(event.is_notified());
    /// ```
    #[inline]
    pub fn is_notified(&self) -> bool {
        self.try_inner()
            .is_some_and(|inner| inner.notified.load(Ordering::Acquire) > 0)
    }

    /// Returns a guard listening for a notification.
    ///
    /// This method allocates and pins the listener on the heap, then registers
    /// it under the event's list lock.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    /// let listener = event.listen();
    /// ```
    ///
    /// # Caveats
    ///
    /// To avoid the listener's heap allocation, use the [`listener!`] macro to register a
    /// stack-based listener instead.
    #[cold]
    pub fn listen(&self) -> EventListener<T> {
        // SAFETY: Event owns the raw Arc reference returned by inner(). Prevent
        // this temporary Arc from releasing it; the listener gets its own clone.
        let inner = ManuallyDrop::new(unsafe { Arc::from_raw(self.inner()) });

        // Allocate the listener on the heap and insert it.
        let mut listener = Box::pin(InnerListener {
            event: Arc::clone(&inner),
            listener: None,
        });
        listener.as_mut().listen();

        // Return the listener.
        EventListener { listener }
    }

    /// Notifies a number of active listeners.
    ///
    /// The number is allowed to be zero or exceed the current number of listeners.
    ///
    /// The [`Notification`] trait is used to define what kind of notification is delivered.
    /// The default implementation (implemented on `usize`) is a notification that only notifies
    /// *at least* the specified number of listeners.
    ///
    /// In certain cases, this function emits a `SeqCst` fence before notifying listeners.
    ///
    /// This function returns the number of [`EventListener`]s that were notified by this call.
    ///
    /// # Caveats
    ///
    /// Every backend acquires the list lock and commits notifications before
    /// returning. The return value counts new notifications from this call,
    /// not listeners that still hold a notification when the call returns.
    /// A listener or a re-entrant wake callback can consume a notification first.
    ///
    /// # Examples
    ///
    /// Use the default notification strategy:
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    ///
    /// // This notification gets lost because there are no listeners.
    /// event.notify(1);
    ///
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    /// let listener3 = event.listen();
    ///
    /// // Notifies two listeners.
    /// //
    /// // Listener queueing is fair, which means `listener1` and `listener2`
    /// // get notified here since they start listening before `listener3`.
    /// event.notify(2);
    /// ```
    ///
    /// Notify without emitting a `SeqCst` fence. This uses the [`relaxed`] notification strategy.
    /// This is equivalent to calling [`Event::notify_relaxed()`].
    ///
    /// [`relaxed`]: IntoNotification::relaxed
    ///
    /// ```
    /// use event_listener::{IntoNotification, Event};
    /// use std::sync::atomic::{self, Ordering};
    ///
    /// let event = Event::new();
    ///
    /// // This notification gets lost because there are no listeners.
    /// event.notify(1.relaxed());
    ///
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    /// let listener3 = event.listen();
    ///
    /// // We should emit a fence manually when using relaxed notifications.
    /// atomic::fence(Ordering::SeqCst);
    ///
    /// // Notifies two listeners.
    /// //
    /// // Listener queueing is fair, which means `listener1` and `listener2`
    /// // get notified here since they start listening before `listener3`.
    /// event.notify(2.relaxed());
    /// ```
    ///
    /// Notify additional listeners. In contrast to [`Event::notify()`], this method will notify `n`
    /// *additional* listeners that were previously unnotified. This uses the [`additional`]
    /// notification strategy. This is equivalent to calling [`Event::notify_additional()`].
    ///
    /// [`additional`]: IntoNotification::additional
    ///
    /// ```
    /// use event_listener::{IntoNotification, Event};
    ///
    /// let event = Event::new();
    ///
    /// // This notification gets lost because there are no listeners.
    /// event.notify(1.additional());
    ///
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    /// let listener3 = event.listen();
    ///
    /// // Notifies two listeners.
    /// //
    /// // Listener queueing is fair, which means `listener1` and `listener2`
    /// // get notified here since they start listening before `listener3`.
    /// event.notify(1.additional());
    /// event.notify(1.additional());
    /// ```
    ///
    /// Notifies with the [`additional`] and [`relaxed`] strategies at the same time. This is
    /// equivalent to calling [`Event::notify_additional_relaxed()`].
    ///
    /// ```
    /// use event_listener::{IntoNotification, Event};
    /// use std::sync::atomic::{self, Ordering};
    ///
    /// let event = Event::new();
    ///
    /// // This notification gets lost because there are no listeners.
    /// event.notify(1.additional().relaxed());
    ///
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    /// let listener3 = event.listen();
    ///
    /// // We should emit a fence manually when using relaxed notifications.
    /// atomic::fence(Ordering::SeqCst);
    ///
    /// // Notifies two listeners.
    /// //
    /// // Listener queueing is fair, which means `listener1` and `listener2`
    /// // get notified here since they start listening before `listener3`.
    /// event.notify(1.additional().relaxed());
    /// event.notify(1.additional().relaxed());
    /// ```
    #[inline]
    pub fn notify(&self, notify: impl IntoNotification<Tag = T>) -> usize {
        let notify = notify.into_notification();

        // Make sure the notification comes after whatever triggered it.
        notify.fence(notify::Internal::new());

        // SAFETY: The borrowed Event retains the initialised allocation.
        let inner = unsafe { &*self.inner() };
        inner.notify(notify)
    }

    /// Return a reference to the inner state if it has been initialized.
    #[inline]
    fn try_inner(&self) -> Option<&Inner<T>> {
        let inner = self.inner.load(Ordering::Acquire);
        // SAFETY: Acquire observes initialisation; the borrowed Event owns the
        // allocation until this reference expires. Null represents unused state.
        unsafe { inner.as_ref() }
    }

    /// Returns a raw, initialized pointer to the inner state.
    ///
    /// This returns a raw pointer instead of reference because `from_raw`
    /// requires raw/mut provenance: <https://github.com/rust-lang/rust/pull/67339>.
    fn inner(&self) -> *const Inner<T> {
        let mut inner = self.inner.load(Ordering::Acquire);

        // If this is the first use, initialize the state.
        if inner.is_null() {
            // Allocate the state on the heap.
            let new = Arc::new(Inner::<T>::new());

            // Convert the state to a raw pointer.
            let new = Arc::into_raw(new) as *mut Inner<T>;

            // Replace the null pointer with the new state pointer.
            inner = self
                .inner
                .compare_exchange(inner, new, Ordering::AcqRel, Ordering::Acquire)
                .unwrap_or_else(|x| x);

            // Check if the old pointer value was indeed null.
            if inner.is_null() {
                // If yes, then use the new state pointer.
                inner = new;
            } else {
                // If not, that means a concurrent operation has initialized the state.
                // In that case, use the old pointer and deallocate the new one.
                // SAFETY: This losing pointer owns the Arc reference just
                // converted above and was never published to another caller.
                unsafe {
                    drop(Arc::from_raw(new));
                }
            }
        }

        inner
    }

    /// Get the number of listeners currently listening to this [`Event`].
    ///
    /// This call returns the number of [`EventListener`]s that are currently listening to
    /// this event. It does this by acquiring the internal event lock and reading the listener
    /// count. Therefore it is only available for `std`-enabled platforms.
    ///
    /// # Caveats
    ///
    /// This function returns just a snapshot of the number of listeners at this point in time.
    /// Due to the nature of multi-threaded CPUs, it is possible that this number will be
    /// inaccurate by the time that this function returns.
    ///
    /// It is possible for the actual number to change at any point. Therefore, the number should
    /// only ever be used as a hint.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    ///
    /// assert_eq!(event.total_listeners(), 0);
    ///
    /// let listener1 = event.listen();
    /// assert_eq!(event.total_listeners(), 1);
    ///
    /// let listener2 = event.listen();
    /// assert_eq!(event.total_listeners(), 2);
    ///
    /// drop(listener1);
    /// drop(listener2);
    /// assert_eq!(event.total_listeners(), 0);
    /// ```
    #[cfg(feature = "std")]
    #[inline]
    pub fn total_listeners(&self) -> usize {
        if let Some(inner) = self.try_inner() {
            inner.list.total_listeners()
        } else {
            0
        }
    }
}

impl Event<()> {
    /// Creates a new [`Event`].
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    /// ```
    #[inline]
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self {
            inner: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Creates a new [`Event`].
    #[inline]
    #[cfg(loom)]
    pub fn new() -> Self {
        Self {
            inner: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Notifies a number of active listeners without emitting a `SeqCst` fence.
    ///
    /// The number is allowed to be zero or exceed the current number of listeners.
    ///
    /// In contrast to [`Event::notify_additional()`], this method only makes sure *at least* `n`
    /// listeners among the active ones are notified.
    ///
    /// Unlike [`Event::notify()`], this method does not emit a `SeqCst` fence.
    ///
    /// This method only works for untagged events. In other cases, it is recommended to instead
    /// use [`Event::notify()`] like so:
    ///
    /// ```
    /// use event_listener::{IntoNotification, Event};
    /// let event = Event::new();
    ///
    /// // Old way:
    /// event.notify_relaxed(1);
    ///
    /// // New way:
    /// event.notify(1.relaxed());
    /// ```
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::{Event, IntoNotification};
    /// use std::sync::atomic::{self, Ordering};
    ///
    /// let event = Event::new();
    ///
    /// // This notification gets lost because there are no listeners.
    /// event.notify_relaxed(1);
    ///
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    /// let listener3 = event.listen();
    ///
    /// // We should emit a fence manually when using relaxed notifications.
    /// atomic::fence(Ordering::SeqCst);
    ///
    /// // Notifies two listeners.
    /// //
    /// // Listener queueing is fair, which means `listener1` and `listener2`
    /// // get notified here since they start listening before `listener3`.
    /// event.notify_relaxed(2);
    /// ```
    #[inline]
    pub fn notify_relaxed(&self, n: usize) -> usize {
        self.notify(n.relaxed())
    }

    /// Notifies a number of active and still unnotified listeners.
    ///
    /// The number is allowed to be zero or exceed the current number of listeners.
    ///
    /// In contrast to [`Event::notify()`], this method will notify `n` *additional* listeners that
    /// were previously unnotified.
    ///
    /// This method emits a `SeqCst` fence before notifying listeners.
    ///
    /// This method only works for untagged events. In other cases, it is recommended to instead
    /// use [`Event::notify()`] like so:
    ///
    /// ```
    /// use event_listener::{IntoNotification, Event};
    /// let event = Event::new();
    ///
    /// // Old way:
    /// event.notify_additional(1);
    ///
    /// // New way:
    /// event.notify(1.additional());
    /// ```
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    ///
    /// let event = Event::new();
    ///
    /// // This notification gets lost because there are no listeners.
    /// event.notify_additional(1);
    ///
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    /// let listener3 = event.listen();
    ///
    /// // Notifies two listeners.
    /// //
    /// // Listener queueing is fair, which means `listener1` and `listener2`
    /// // get notified here since they start listening before `listener3`.
    /// event.notify_additional(1);
    /// event.notify_additional(1);
    /// ```
    #[inline]
    pub fn notify_additional(&self, n: usize) -> usize {
        self.notify(n.additional())
    }

    /// Notifies a number of active and still unnotified listeners without emitting a `SeqCst`
    /// fence.
    ///
    /// The number is allowed to be zero or exceed the current number of listeners.
    ///
    /// In contrast to [`Event::notify()`], this method will notify `n` *additional* listeners that
    /// were previously unnotified.
    ///
    /// Unlike [`Event::notify_additional()`], this method does not emit a `SeqCst` fence.
    ///
    /// This method only works for untagged events. In other cases, it is recommended to instead
    /// use [`Event::notify()`] like so:
    ///
    /// ```
    /// use event_listener::{IntoNotification, Event};
    /// let event = Event::new();
    ///
    /// // Old way:
    /// event.notify_additional_relaxed(1);
    ///
    /// // New way:
    /// event.notify(1.additional().relaxed());
    /// ```
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::Event;
    /// use std::sync::atomic::{self, Ordering};
    ///
    /// let event = Event::new();
    ///
    /// // This notification gets lost because there are no listeners.
    /// event.notify(1);
    ///
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    /// let listener3 = event.listen();
    ///
    /// // We should emit a fence manually when using relaxed notifications.
    /// atomic::fence(Ordering::SeqCst);
    ///
    /// // Notifies two listeners.
    /// //
    /// // Listener queueing is fair, which means `listener1` and `listener2`
    /// // get notified here since they start listening before `listener3`.
    /// event.notify_additional_relaxed(1);
    /// event.notify_additional_relaxed(1);
    /// ```
    #[inline]
    pub fn notify_additional_relaxed(&self, n: usize) -> usize {
        self.notify(n.additional().relaxed())
    }
}

impl<T> Drop for Event<T> {
    #[inline]
    fn drop(&mut self) {
        self.inner.with_mut(|&mut inner| {
            // If the state pointer has been initialized, drop it.
            if !inner.is_null() {
                // SAFETY: Exclusive access ends Event's one raw Arc ownership.
                // Heap listeners retain their own references to the allocation.
                unsafe {
                    drop(Arc::from_raw(inner));
                }
            }
        })
    }
}

/// A handle that is listening to an [`Event`].
///
/// This trait represents a type waiting for a notification from an [`Event`]. See the
/// [`EventListener`] type for more documentation on this trait's usage.
pub trait Listener<T = ()>: Future<Output = T> + __sealed::Sealed {
    /// Blocks until a notification is received.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::{Event, Listener};
    ///
    /// let event = Event::new();
    /// let mut listener = event.listen();
    ///
    /// // Notify `listener`.
    /// event.notify(1);
    ///
    /// // Receive the notification.
    /// listener.wait();
    /// ```
    #[cfg(all(feature = "std", not(target_family = "wasm")))]
    fn wait(self) -> T;

    /// Blocks until a notification is received or a timeout is reached.
    ///
    /// Returns `Some` if a notification was received.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use event_listener::{Event, Listener};
    ///
    /// let event = Event::new();
    /// let mut listener = event.listen();
    ///
    /// // There are no notification so this times out.
    /// assert!(listener.wait_timeout(Duration::from_secs(1)).is_none());
    /// ```
    #[cfg(all(feature = "std", not(target_family = "wasm")))]
    fn wait_timeout(self, timeout: Duration) -> Option<T>;

    /// Blocks until a notification is received or a deadline is reached.
    ///
    /// Returns `Some(tag)` if a notification was received, or `None` on timeout.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use event_listener::{Event, Listener};
    ///
    /// let event = Event::new();
    /// let mut listener = event.listen();
    ///
    /// // There are no notification so this times out.
    /// assert!(listener.wait_deadline(Instant::now() + Duration::from_secs(1)).is_none());
    /// ```
    #[cfg(all(feature = "std", not(target_family = "wasm")))]
    fn wait_deadline(self, deadline: Instant) -> Option<T>;

    /// Drops this listener and discards its notification (if any) without notifying another
    /// active listener.
    ///
    /// Returns `true` if a notification was discarded.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::{Event, Listener};
    ///
    /// let event = Event::new();
    /// let mut listener1 = event.listen();
    /// let mut listener2 = event.listen();
    ///
    /// event.notify(1);
    ///
    /// assert!(listener1.discard());
    /// assert!(!listener2.discard());
    /// ```
    fn discard(self) -> bool;

    /// Returns `true` if this listener listens to the given `Event`.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::{Event, Listener};
    ///
    /// let event = Event::new();
    /// let listener = event.listen();
    ///
    /// assert!(listener.listens_to(&event));
    /// ```
    fn listens_to(&self, event: &Event<T>) -> bool;

    /// Returns `true` if both listeners listen to the same `Event`.
    ///
    /// # Examples
    ///
    /// ```
    /// use event_listener::{Event, Listener};
    ///
    /// let event = Event::new();
    /// let listener1 = event.listen();
    /// let listener2 = event.listen();
    ///
    /// assert!(listener1.same_event(&listener2));
    /// ```
    fn same_event(&self, other: &Self) -> bool;
}

/// Implement the `Listener` trait using the underlying `InnerListener`.
macro_rules! forward_impl_to_listener {
    ($gen:ident => $ty:ty) => {
        impl<$gen> crate::Listener<$gen> for $ty {
            #[cfg(all(feature = "std", not(target_family = "wasm")))]
            fn wait(mut self) -> $gen {
                self.listener_mut().wait_internal(None).unwrap()
            }

            #[cfg(all(feature = "std", not(target_family = "wasm")))]
            fn wait_timeout(mut self, timeout: std::time::Duration) -> Option<$gen> {
                self.listener_mut()
                    .wait_internal(std::time::Instant::now().checked_add(timeout))
            }

            #[cfg(all(feature = "std", not(target_family = "wasm")))]
            fn wait_deadline(mut self, deadline: std::time::Instant) -> Option<$gen> {
                self.listener_mut().wait_internal(Some(deadline))
            }

            fn discard(mut self) -> bool {
                self.listener_mut().discard()
            }

            #[inline]
            fn listens_to(&self, event: &Event<$gen>) -> bool {
                core::ptr::eq::<Inner<$gen>>(
                    &*self.listener().event,
                    event.inner.load(core::sync::atomic::Ordering::Acquire),
                )
            }

            #[inline]
            fn same_event(&self, other: &$ty) -> bool {
                core::ptr::eq::<Inner<$gen>>(&*self.listener().event, &*other.listener().event)
            }
        }

        impl<$gen> Future for $ty {
            type Output = $gen;

            #[inline]
            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<$gen> {
                self.listener_mut().poll_internal(cx)
            }
        }
    };
}

/// A guard waiting for a notification from an [`Event`].
///
/// There are two ways for a listener to wait for a notification:
///
/// 1. In an asynchronous manner using `.await`.
/// 2. In a blocking manner by calling [`EventListener::wait()`] on it.
///
/// If a notified listener is dropped without receiving a notification, dropping will notify
/// another active listener. Whether one *additional* listener will be notified depends on what
/// kind of notification was delivered.
///
/// See the [`Listener`] trait for the functionality exposed by this type.
///
/// This structure allocates the listener on the heap.
pub struct EventListener<T = ()> {
    /// Pinned node with its own shared reference to the event storage.
    listener: Pin<Box<InnerListener<T, Arc<Inner<T>>>>>,
}

// SAFETY: The pinned node stays allocated when this handle moves. The list lock
// protects its state; extracting a tag or polling requires exclusive access.
unsafe impl<T: Send> Send for EventListener<T> {}
unsafe impl<T: Send> Sync for EventListener<T> {}

impl<T> core::panic::UnwindSafe for EventListener<T> {}
impl<T> core::panic::RefUnwindSafe for EventListener<T> {}
impl<T> Unpin for EventListener<T> {}

impl<T> fmt::Debug for EventListener<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventListener").finish_non_exhaustive()
    }
}

impl<T> EventListener<T> {
    /// Borrows the pinned node without moving it or cloning its event reference.
    #[inline]
    fn listener(&self) -> &InnerListener<T, Arc<Inner<T>>> {
        &self.listener
    }

    /// Reborrows the node's pin for polling, waiting or removal.
    #[inline]
    fn listener_mut(&mut self) -> Pin<&mut InnerListener<T, Arc<Inner<T>>>> {
        self.listener.as_mut()
    }
}

forward_impl_to_listener! { T => EventListener<T> }

/// Create a stack-based event listener for an [`Event`].
///
/// [`EventListener`] allocates the listener on the heap. While this works for most use cases, in
/// practice this heap allocation can be expensive for repeated uses. This method allows for
/// allocating the listener on the stack instead.
///
/// There are limitations to using this macro instead of the [`EventListener`] type, however.
/// Firstly, it is significantly less flexible. The listener is locked to the current stack
/// frame, meaning that it can't be returned or put into a place where it would go out of
/// scope. For instance, this will not work:
///
/// ```compile_fail
/// use event_listener::{Event, Listener, listener};
///
/// fn get_listener(event: &Event) -> impl Listener {
///     listener!(event => cant_return_this);
///     cant_return_this
/// }
/// ```
///
/// In addition, the types involved in creating this listener are not able to be named. Therefore
/// it cannot be used in hand-rolled futures or similar structures.
///
/// The type created by this macro implements [`Listener`], allowing it to be used in cases where
/// [`EventListener`] would normally be used.
///
/// ## Example
///
/// To use this macro, replace cases where you would normally use this...
///
/// ```no_compile
/// let listener = event.listen();
/// ```
///
/// ...with this:
///
/// ```no_compile
/// listener!(event => listener);
/// ```
///
/// Here is the top level example from this crate's documentation, but using [`listener`] instead
/// of [`EventListener`].
///
#[cfg_attr(feature = "std", doc = "```")]
#[cfg_attr(not(feature = "std"), doc = "```no_compile")]
/// # #[cfg(not(target_family = "wasm"))] { // Listener::wait is unavailable on WASM
/// use std::sync::atomic::{AtomicBool, Ordering};
/// use std::sync::Arc;
/// use std::thread;
/// use std::time::Duration;
/// use event_listener::{Event, listener, IntoNotification, Listener};
///
/// let flag = Arc::new(AtomicBool::new(false));
/// let event = Arc::new(Event::new());
///
/// // Spawn a thread that will set the flag after 1 second.
/// thread::spawn({
///     let flag = flag.clone();
///     let event = event.clone();
///     move || {
///         // Wait for a second.
///         thread::sleep(Duration::from_secs(1));
///
///         // Set the flag.
///         flag.store(true, Ordering::SeqCst);
///
///         // Notify all listeners that the flag has been set.
///         event.notify(usize::MAX);
///     }
/// });
///
/// // Wait until the flag is set.
/// loop {
///     // Check the flag.
///     if flag.load(Ordering::SeqCst) {
///         break;
///     }
///
///     // Start listening for events.
///     // NEW: Changed to a stack-based listener.
///     listener!(event => listener);
///
///     // Check the flag again after creating the listener.
///     if flag.load(Ordering::SeqCst) {
///         break;
///     }
///
///     // Wait for a notification and continue the loop.
///     listener.wait();
/// }
/// # }
/// ```
#[macro_export]
macro_rules! listener {
    ($event:expr => $listener:ident) => {
        let mut $listener = $crate::__private::StackSlot::new(&$event);
        // SAFETY: We shadow $listener so it can't be moved after.
        let mut $listener = unsafe { $crate::__private::Pin::new_unchecked(&mut $listener) };
        #[allow(unused_mut)]
        let mut $listener = $listener.listen();
    };
}

pin_project_lite::pin_project! {
    #[project(!Unpin)]
    #[project = ListenerProject]
    struct InnerListener<T, B: Borrow<Inner<T>>>
    where
        B: Unpin,
    {
        // Retains the shared list or borrows it for the backing slot's lifetime.
        event: B,

        // The inner state of the listener.
        //
        // This is None before insertion and after completion, discard or timeout.
        // An occupied slot remains pinned until the list unlinks it.
        #[pin]
        listener: Option<sys::Listener<T>>,
    }

    impl<T, B: Borrow<Inner<T>>> PinnedDrop for InnerListener<T, B>
    where
        B: Unpin,
    {
        fn drop(mut this: Pin<&mut Self>) {
            let this = this.project();
            // Completed listeners are already unlinked and need no list access.
            if this.listener.is_some() {
                (*this.event).borrow().remove(this.listener, true);
            }
        }
    }
}

// SAFETY: The list lock protects the pinned node and its tag. B preserves access
// to that list across the permitted move or shared reference; T crosses threads
// only through exclusive extraction or a locked list operation.
unsafe impl<T: Send, B: Borrow<Inner<T>> + Unpin + Send> Send for InnerListener<T, B> {}
unsafe impl<T: Send, B: Borrow<Inner<T>> + Unpin + Sync> Sync for InnerListener<T, B> {}

impl<T, B: Borrow<Inner<T>> + Unpin> InnerListener<T, B> {
    /// Insert this listener into the linked list.
    #[inline]
    fn listen(self: Pin<&mut Self>) {
        let this = self.project();
        (*this.event).borrow().insert(this.listener);
    }

    /// Wait until the provided deadline.
    #[cfg(all(feature = "std", not(target_family = "wasm")))]
    fn wait_internal(mut self: Pin<&mut Self>, deadline: Option<Instant>) -> Option<T> {
        fn parker_and_task() -> (Parker, Task) {
            let parker = Parker::new();
            let unparker = parker.unparker();
            (parker, Task::Unparker(unparker))
        }

        crate::sync::thread_local! {
            /// Cached pair, borrowed exclusively so nested waits cannot share wake tokens.
            static PARKER: core::cell::RefCell<(Parker, Task)> =
                core::cell::RefCell::new(parker_and_task());
        }

        // Try to borrow the thread-local parker/unparker pair.
        PARKER
            .try_with({
                let this = self.as_mut();
                |cached| {
                    let pair = cached.try_borrow_mut().ok()?;
                    let (parker, unparker) = &*pair;
                    Some(this.wait_with_parker(deadline, parker, unparker.as_task_ref()))
                }
            })
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                // Nested waits and TLS teardown cannot use the cached pair.
                let (parker, unparker) = parking::pair();
                self.as_mut()
                    .wait_with_parker(deadline, &parker, TaskRef::Unparker(&unparker))
            })
    }

    /// Wait until the provided deadline using the specified parker/unparker pair.
    #[cfg(all(feature = "std", not(target_family = "wasm")))]
    fn wait_with_parker(
        self: Pin<&mut Self>,
        deadline: Option<Instant>,
        parker: &Parker,
        unparker: TaskRef<'_>,
    ) -> Option<T> {
        let mut this = self.project();
        let inner = (*this.event).borrow();

        // Set the listener's state to `Task`.
        if let Some(tag) = inner.register(this.listener.as_mut(), unparker).notified() {
            // We were already notified, so we don't need to park.
            return Some(tag);
        }

        // Wait until a notification is received or the timeout is reached.
        loop {
            match deadline {
                None => parker.park(),

                #[cfg(loom)]
                Some(_deadline) => {
                    panic!("parking does not support timeouts under loom");
                }

                #[cfg(not(loom))]
                Some(deadline) => {
                    // Make sure we're not timed out already.
                    let now = Instant::now();
                    if now >= deadline {
                        // Remove our entry and check if we were notified.
                        return inner
                            .remove(this.listener.as_mut(), false)
                            .expect("We never removed ourself from the list")
                            .notified();
                    }
                    parker.park_deadline(deadline);
                }
            }

            // See if we were notified.
            if let Some(tag) = inner.register(this.listener.as_mut(), unparker).notified() {
                return Some(tag);
            }
        }
    }

    /// Drops this listener and discards its notification (if any) without notifying another
    /// active listener.
    fn discard(self: Pin<&mut Self>) -> bool {
        let this = self.project();
        (*this.event)
            .borrow()
            .remove(this.listener, false)
            .is_some_and(|state| state.is_notified())
    }

    /// Poll this listener for a notification.
    fn poll_internal(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let this = self.project();
        let inner = (*this.event).borrow();

        // Try to register the listener.
        match inner
            .register(this.listener, TaskRef::Waker(cx.waker()))
            .notified()
        {
            Some(tag) => {
                // We were already notified, so we don't need to park.
                Poll::Ready(tag)
            }

            None => {
                // We're now waiting for a notification.
                Poll::Pending
            }
        }
    }
}

/// The state of a listener.
enum State<T> {
    /// The listener was just created.
    Created,

    /// The listener has received a notification.
    ///
    /// The `bool` is `true` if this was an "additional" notification.
    Notified {
        /// Whether or not this is an "additional" notification.
        additional: bool,

        /// The tag associated with the notification.
        tag: T,
    },

    /// A task is waiting for a notification.
    Task(Task),

    /// Empty hole used to replace a notified listener.
    NotifiedTaken,
}

impl<T> State<T> {
    /// Reports committed notification, including a tag already moved out.
    fn is_notified(&self) -> bool {
        matches!(self, Self::Notified { .. } | Self::NotifiedTaken)
    }

    /// Return the notification tag if this state still owns one.
    #[cfg(all(feature = "std", not(target_family = "wasm"), not(loom)))]
    fn notified(self) -> Option<T> {
        match self {
            Self::Notified { tag, .. } => Some(tag),
            _ => None,
        }
    }
}

/// The result of registering a listener.
enum RegisterResult<T> {
    /// The listener was already notified.
    Notified(T),

    /// The listener has been registered.
    Registered,

    /// The listener was never inserted into the list.
    NeverInserted,
}

impl<T> RegisterResult<T> {
    /// Whether or not the listener was notified.
    ///
    /// Panics if the listener was never inserted into the list.
    fn notified(self) -> Option<T> {
        match self {
            Self::Notified(tag) => Some(tag),
            Self::Registered => None,
            Self::NeverInserted => panic!("{}", NEVER_INSERTED_PANIC),
        }
    }
}

/// A task that can be woken up.
enum Task {
    /// A waker that wakes up a future.
    Waker(Waker),

    /// An unparker that wakes up a thread.
    #[cfg(all(feature = "std", not(target_family = "wasm")))]
    Unparker(Unparker),
}

impl Task {
    /// Borrows the wake target without cloning its handle.
    fn as_task_ref(&self) -> TaskRef<'_> {
        match self {
            Self::Waker(waker) => TaskRef::Waker(waker),
            #[cfg(all(feature = "std", not(target_family = "wasm")))]
            Self::Unparker(unparker) => TaskRef::Unparker(unparker),
        }
    }

    /// Consumes this registration and wakes its task.
    ///
    /// Callers must release the list lock before invoking user wakers.
    /// Panics from those callbacks propagate.
    fn wake(self) {
        match self {
            Self::Waker(waker) => waker.wake(),
            #[cfg(all(feature = "std", not(target_family = "wasm")))]
            Self::Unparker(unparker) => {
                unparker.unpark();
            }
        }
    }
}

/// A reference to a task.
#[derive(Clone, Copy)]
enum TaskRef<'a> {
    /// A waker that wakes up a future.
    Waker(&'a Waker),

    /// An unparker that wakes up a thread.
    #[cfg(all(feature = "std", not(target_family = "wasm")))]
    Unparker(&'a Unparker),
}

impl TaskRef<'_> {
    /// Tells if this task will wake up the other task.
    #[allow(unreachable_patterns)]
    fn will_wake(self, other: Self) -> bool {
        match (self, other) {
            (Self::Waker(a), Self::Waker(b)) => a.will_wake(b),
            #[cfg(all(feature = "std", not(target_family = "wasm")))]
            (Self::Unparker(_), Self::Unparker(_)) => {
                // TODO: Use unreleased will_unpark API.
                false
            }
            _ => false,
        }
    }

    /// Converts this task reference to a task by cloning.
    fn into_task(self) -> Task {
        match self {
            Self::Waker(waker) => Task::Waker(waker.clone()),
            #[cfg(all(feature = "std", not(target_family = "wasm")))]
            Self::Unparker(unparker) => Task::Unparker(unparker.clone()),
        }
    }
}

/// Shared diagnostic for operations on an absent listener registration.
const NEVER_INSERTED_PANIC: &str = "\
EventListener was not inserted into the linked list, make sure you're not polling \
EventListener/listener! after it has finished";

#[cfg(not(loom))]
/// Synchronization primitive implementation.
mod sync {
    #[cfg(not(feature = "portable-atomic"))]
    pub(super) use alloc::sync::Arc;
    #[cfg(not(feature = "portable-atomic"))]
    pub(super) use core::sync::atomic;

    #[cfg(feature = "portable-atomic")]
    pub(super) use portable_atomic_crate as atomic;
    #[cfg(feature = "portable-atomic")]
    pub(super) use portable_atomic_util::Arc;

    #[allow(unused)]
    #[cfg(all(feature = "std", not(feature = "critical-section"), not(loom)))]
    pub(super) use std::sync::{Mutex, MutexGuard};
    #[cfg(all(feature = "std", not(target_family = "wasm"), not(loom)))]
    pub(super) use std::thread_local;

    /// Adapts native exclusive access to Loom's callback-based atomic interface.
    pub(super) trait WithMut {
        /// The atomic storage exposed by an exclusive borrow.
        type Output;

        /// Runs `f` with exclusive access to the underlying atomic storage.
        fn with_mut<F, R>(&mut self, f: F) -> R
        where
            F: FnOnce(&mut Self::Output) -> R;
    }

    impl<T> WithMut for atomic::AtomicPtr<T> {
        type Output = *mut T;

        #[inline]
        fn with_mut<F, R>(&mut self, f: F) -> R
        where
            F: FnOnce(&mut Self::Output) -> R,
        {
            f(self.get_mut())
        }
    }

    /// Native cell types with the same access interface as Loom.
    pub(crate) mod cell {
        pub(crate) use core::cell::Cell;

        /// This newtype around *mut T exists for interoperability with loom::cell::ConstPtr,
        /// which works as a guard and performs additional logic to track access scope.
        pub(crate) struct ConstPtr<T>(*mut T);
        impl<T> ConstPtr<T> {
            /// Borrows the value while the caller holds the list lock.
            ///
            /// # Safety
            /// The pointer must remain valid, with no conflicting mutable access
            /// for the returned reference's lifetime.
            pub(crate) unsafe fn deref(&self) -> &T {
                &*self.0
            }

            #[cfg(not(any(feature = "std", feature = "critical-section")))]
            /// Mutably borrows the value while the caller holds its spinlock.
            ///
            /// # Safety
            /// The pointer must remain valid and exclusively accessible for the
            /// returned reference's lifetime.
            pub(crate) unsafe fn deref_mut(&mut self) -> &mut T {
                &mut *self.0
            }
        }

        /// This UnsafeCell wrapper exists for interoperability with loom::cell::UnsafeCell, and
        /// only contains the interface that is needed for this crate.
        pub(crate) struct UnsafeCell<T>(core::cell::UnsafeCell<T>);

        impl<T> UnsafeCell<T> {
            /// Stores `data` without adding synchronisation.
            pub(crate) fn new(data: T) -> UnsafeCell<T> {
                UnsafeCell(core::cell::UnsafeCell::new(data))
            }

            /// Returns a raw access wrapper without acquiring the list lock.
            ///
            /// Dereferencing the wrapper requires the caller to uphold its safety contract.
            pub(crate) fn get(&self) -> ConstPtr<T> {
                ConstPtr(self.0.get())
            }
        }
    }
}

#[cfg(loom)]
/// Synchronization primitive implementation.
mod sync {
    pub(super) use loom::sync::{atomic, Arc, Mutex, MutexGuard};
    pub(super) use loom::{cell, thread_local};
}

const _: () = {
    const fn _assert_send<T: Send>() {}
    const fn _assert_sync<T: Sync>() {}

    _assert_send::<crate::__private::StackSlot<'_, ()>>();
    _assert_sync::<crate::__private::StackSlot<'_, ()>>();
    _assert_send::<crate::__private::StackListener<'_, '_, ()>>();
    _assert_sync::<crate::__private::StackListener<'_, '_, ()>>();
    _assert_send::<Event<()>>();
    _assert_sync::<Event<()>>();
    _assert_send::<EventListener<()>>();
    _assert_sync::<EventListener<()>>();
};

#[doc(hidden)]
mod __sealed {
    use super::{__private::StackListener, EventListener};

    pub trait Sealed {}
    impl<T> Sealed for EventListener<T> {}
    impl<T> Sealed for StackListener<'_, '_, T> {}
}

/// Semver exempt module.
#[doc(hidden)]
pub mod __private {
    pub use core::pin::Pin;

    use super::{Event, Inner, InnerListener};
    use core::fmt;
    use core::future::Future;
    use core::task::{Context, Poll};

    pin_project_lite::pin_project! {
        /// Space on the stack where a stack-based listener can be allocated.
        #[doc(hidden)]
        #[project(!Unpin)]
        pub struct StackSlot<'ev, T> {
            // Pinned node that borrows the Event until this slot is dropped.
            #[pin]
            listener: InnerListener<T, &'ev Inner<T>>
        }
    }

    impl<T> fmt::Debug for StackSlot<'_, T> {
        #[inline]
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("StackSlot").finish_non_exhaustive()
        }
    }

    impl<T> core::panic::UnwindSafe for StackSlot<'_, T> {}
    impl<T> core::panic::RefUnwindSafe for StackSlot<'_, T> {}
    // SAFETY: The Event borrow retains the list. Pinning prevents linked storage
    // from moving, and the list lock protects its tag and registration state.
    unsafe impl<T: Send> Send for StackSlot<'_, T> {}
    unsafe impl<T: Send> Sync for StackSlot<'_, T> {}

    impl<'ev, T> StackSlot<'ev, T> {
        /// Create a new `StackSlot` on the stack.
        #[inline]
        #[doc(hidden)]
        pub fn new(event: &'ev Event<T>) -> Self {
            // SAFETY: The slot retains the Event borrow for this reference's
            // lifetime, so Event cannot release its raw Arc ownership first.
            let inner = unsafe { &*event.inner() };
            Self {
                listener: InnerListener {
                    event: inner,
                    listener: None,
                },
            }
        }

        /// Start listening on this `StackSlot`.
        ///
        /// # Panics
        ///
        /// Panics if an earlier listener still occupies this slot. Complete or
        /// discard that listener before listening again.
        #[inline]
        #[doc(hidden)]
        pub fn listen(mut self: Pin<&mut Self>) -> StackListener<'ev, '_, T> {
            // Dropping the borrowed StackListener does not remove its entry.
            // Reject reuse before insertion could overwrite a linked node or
            // invoke its task/tag destructor while holding the list lock.
            assert!(
                self.listener.listener.is_none(),
                "cannot listen again while a previous registration remains"
            );
            // Insert ourselves into the list.
            self.as_mut().project().listener.listen();

            // We are now listening.
            StackListener { slot: self }
        }
    }

    /// A stack-based `EventListener`.
    #[doc(hidden)]
    pub struct StackListener<'ev, 'stack, T> {
        slot: Pin<&'stack mut StackSlot<'ev, T>>,
    }

    impl<T> core::panic::UnwindSafe for StackListener<'_, '_, T> {}
    impl<T> core::panic::RefUnwindSafe for StackListener<'_, '_, T> {}
    impl<T> Unpin for StackListener<'_, '_, T> {}

    impl<T> fmt::Debug for StackListener<'_, '_, T> {
        #[inline]
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("StackListener").finish_non_exhaustive()
        }
    }

    impl<'ev, T> StackListener<'ev, '_, T> {
        #[inline]
        fn listener(&self) -> &InnerListener<T, &'ev Inner<T>> {
            &self.slot.listener
        }

        #[inline]
        fn listener_mut(&mut self) -> Pin<&mut InnerListener<T, &'ev Inner<T>>> {
            self.slot.as_mut().project().listener
        }
    }

    forward_impl_to_listener! { T => StackListener<'_, '_, T> }
}
