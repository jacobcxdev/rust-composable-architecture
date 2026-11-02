// Modified for Rust Composable Architecture; see UPSTREAM.md for provenance and changes.

//! Intrusive linked list-based implementation of `event-listener`.
//!
//! Listeners own their nodes; this list holds pointers into their pinned storage.
//! A mutex, critical section or spinlock protects every node and list update.
//! Owners unlink their nodes before releasing that storage.
//!
//! Notified nodes form a prefix. `next` points to the first unnotified node,
//! or is `None` when all nodes are notified. Registration may temporarily use
//! `NotifiedTaken`, but restores or removes that node before releasing the lock.
//! Task clones, wakes and drops run outside the lock. Tag generation runs inside
//! it and must not re-enter the event; discarded tags are dropped after unlocking.

#[cfg(test)]
use crate::notify::GenericNotify;
use crate::notify::{Internal, Notification};
use crate::sync::atomic::Ordering;
use crate::sync::cell::{Cell, UnsafeCell};
use crate::{RegisterResult, State, Task, TaskRef};

use alloc::vec::Vec;
use arrayvec::ArrayVec;

#[cfg(feature = "critical-section")]
use core::cell::RefCell;

use core::marker::PhantomPinned;
use core::pin::Pin;
use core::ptr::NonNull;

/// Protects externally owned listener nodes with the selected locking backend.
pub(super) struct List<T>(
    /// libstd-based implementation uses a normal Mutex to secure the data.
    #[cfg(all(feature = "std", not(feature = "critical-section")))]
    crate::sync::Mutex<Inner<T>>,
    /// Critical-section-based implementation uses a CS cell that wraps a RefCell.
    #[cfg(feature = "critical-section")]
    critical_section::Mutex<RefCell<Inner<T>>>,
    /// Spinlock implementation uses a spinlock.
    #[cfg(not(any(feature = "std", feature = "critical-section")))]
    spinlock::Spinlock<Inner<T>>,
);

/// Holds linked node addresses and counts under the list lock.
struct Inner<T> {
    /// The head of the linked list.
    head: Option<NonNull<Link<T>>>,

    /// The tail of the linked list.
    tail: Option<NonNull<Link<T>>>,

    /// The first unnotified listener.
    next: Option<NonNull<Link<T>>>,

    /// Total number of listeners.
    len: usize,

    /// The number of notified listeners.
    notified: usize,
}

/// Owns tasks until the list lock has been released.
///
/// The queue needs no heap allocation for up to 32 tasks.
/// This fixed capacity trades stack space for allocation-free small broadcasts;
/// larger broadcasts spill into `overflow`. It is not a notification limit.
struct DeferredTasks {
    /// Stores the first 32 deferred tasks without allocating heap storage.
    inline: ArrayVec<Task, 32>,
    /// Retains later tasks in insertion order after the inline buffer fills.
    overflow: Vec<Task>,
}

impl DeferredTasks {
    /// Creates an empty queue without allocating heap storage.
    fn new() -> Self {
        Self {
            inline: ArrayVec::new(),
            overflow: Vec::new(),
        }
    }

    /// Retains a task for a wake after the caller releases the list lock.
    fn push(&mut self, task: Task) {
        if let Err(full) = self.inline.try_push(task) {
            self.overflow.push(full.element());
        }
    }

    /// Drains queued tasks, preserving remaining wakes during one callback panic.
    fn wake_all(&mut self) {
        // Overflow is populated only after the inline buffer is full. With
        // zero or one task, no broadcast iterator or unwind drain is needed.
        if self.inline.len() <= 1 {
            if let Some(task) = self.inline.pop() {
                task.wake();
            }
            return;
        }

        /// Finishes the remaining wakes if the current wake panics.
        struct WakeOnDrop<I: Iterator<Item = Task>>(I);

        impl<I: Iterator<Item = Task>> Drop for WakeOnDrop<I> {
            fn drop(&mut self) {
                for task in self.0.by_ref() {
                    task.wake();
                }
            }
        }

        let mut tasks = WakeOnDrop(self.inline.drain(..).chain(self.overflow.drain(..)));
        // If one callback panics, the guard still wakes the remaining tasks.
        // A second callback panic during unwinding follows Rust's abort semantics.
        for task in tasks.0.by_ref() {
            task.wake();
        }
    }
}

impl Drop for DeferredTasks {
    fn drop(&mut self) {
        // This owner lives outside with_inner, so even a tag-producer panic
        // releases the list lock before waking earlier committed tasks.
        self.wake_all();
    }
}

impl<T> List<T> {
    /// Create a new, empty event listener list.
    pub(super) fn new() -> Self {
        let inner = Inner {
            head: None,
            tail: None,
            next: None,
            len: 0,
            notified: 0,
        };

        #[cfg(feature = "critical-section")]
        {
            Self(critical_section::Mutex::new(RefCell::new(inner)))
        }

        #[cfg(all(not(feature = "critical-section"), feature = "std"))]
        {
            Self(crate::sync::Mutex::new(inner))
        }

        #[cfg(not(any(feature = "std", feature = "critical-section")))]
        {
            Self(spinlock::Spinlock::new(inner))
        }
    }

    /// Get the total number of listeners without blocking.
    #[cfg(all(feature = "std", not(feature = "critical-section")))]
    pub(crate) fn try_total_listeners(&self) -> Option<usize> {
        self.0.try_lock().ok().map(|list| list.len)
    }

    /// Get the total number of listeners without blocking.
    #[cfg(feature = "critical-section")]
    pub(crate) fn try_total_listeners(&self) -> Option<usize> {
        critical_section::with(|cs| self.0.borrow(cs).try_borrow().ok().map(|list| list.len))
    }

    /// Get the total number of listeners without blocking.
    #[cfg(not(any(feature = "std", feature = "critical-section")))]
    pub(crate) fn try_total_listeners(&self) -> Option<usize> {
        self.0.try_with(|list| list.len)
    }

    /// Get the total number of listeners with blocking.
    #[cfg(all(feature = "std", not(feature = "critical-section")))]
    pub(crate) fn total_listeners(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len
    }

    /// Get the total number of listeners with blocking.
    #[cfg(all(feature = "critical-section", feature = "std"))]
    pub(crate) fn total_listeners(&self) -> usize {
        critical_section::with(|cs| self.0.borrow(cs).borrow().len)
    }
}

impl<T> crate::Inner<T> {
    /// Updates the list under its mutex, publishing committed counts before unlock.
    ///
    /// Publication also occurs when the callback unwinds.
    #[cfg(all(feature = "std", not(feature = "critical-section")))]
    fn with_inner<R>(&self, f: impl FnOnce(&mut Inner<T>) -> R) -> R {
        struct ListLock<'a, 'b, T> {
            lock: crate::sync::MutexGuard<'a, Inner<T>>,
            inner: &'b crate::Inner<T>,
        }

        impl<T> Drop for ListLock<'_, '_, T> {
            fn drop(&mut self) {
                // Publish the committed count before unlocking, including on unwind.
                update_notified(&self.inner.notified, &self.lock);
            }
        }

        let mut list = ListLock {
            inner: self,
            lock: self.list.0.lock().unwrap_or_else(|e| e.into_inner()),
        };
        f(&mut list.lock)
    }

    /// Updates the list under its selected lock, publishing counts before release.
    ///
    /// Publication also occurs when the callback unwinds.
    #[cfg(any(not(feature = "std"), feature = "critical-section"))]
    fn with_inner<R>(&self, f: impl FnOnce(&mut Inner<T>) -> R) -> R {
        struct ListWrapper<'a, T> {
            inner: &'a crate::Inner<T>,
            list: &'a mut Inner<T>,
        }

        impl<T> Drop for ListWrapper<'_, T> {
            fn drop(&mut self) {
                // This guard drops before the critical section or spinlock guard.
                update_notified(&self.inner.notified, self.list);
            }
        }

        #[cfg(feature = "critical-section")]
        return critical_section::with(move |cs| {
            let mut list = self.list.0.borrow_ref_mut(cs);
            let wrapper = ListWrapper {
                inner: self,
                list: &mut *list,
            };

            f(wrapper.list)
        });

        #[cfg(not(feature = "critical-section"))]
        self.list.0.with(move |list| {
            let wrapper = ListWrapper {
                inner: self,
                list: &mut *list,
            };

            f(wrapper.list)
        })
    }

    /// Add a new listener to the list.
    ///
    /// Callers provide an empty slot and keep it pinned until removal. Heap
    /// listeners start empty; `StackSlot::listen` rejects occupied slots.
    pub(crate) fn insert(&self, mut listener: Pin<&mut Option<Listener<T>>>) {
        self.with_inner(|inner| {
            listener.as_mut().set(Some(Listener {
                link: UnsafeCell::new(Link {
                    state: Cell::new(State::Created),
                    prev: Cell::new(inner.tail),
                    next: Cell::new(None),
                }),
                _pin: PhantomPinned,
            }));
            let listener = listener.as_pin_mut().unwrap();

            {
                let entry_guard = listener.link.get();
                // SAFETY: We are locked, so we can access the inner `link`.
                let entry = unsafe { entry_guard.deref() };

                // Replace the tail with the new entry.
                match inner.tail.replace(entry.into()) {
                    None => inner.head = Some(entry.into()),
                    Some(t) => {
                        // SAFETY: The tail remains pinned and linked under this lock.
                        unsafe { t.as_ref().next.set(Some(entry.into())) };
                    }
                };
            }

            // If there are no unnotified entries, this is the first one.
            if inner.next.is_none() {
                inner.next = inner.tail;
            }

            // Bump the entry count.
            inner.len += 1;
        });
    }

    /// Remove a listener from the list.
    pub(crate) fn remove(
        &self,
        listener: Pin<&mut Option<Listener<T>>>,
        propagate: bool,
    ) -> Option<State<T>> {
        let mut discarded_tag = None;
        let result = {
            let mut tasks = DeferredTasks::new();
            self.with_inner(|inner| {
                let state = inner.remove(listener)?;
                if propagate {
                    if let State::Notified { additional, tag } = state {
                        if additional || inner.notified == 0 {
                            if let Some(entry) = inner.next {
                                inner.notify_entry(entry, additional, tag, &mut tasks);
                            } else {
                                discarded_tag = Some(tag);
                            }
                        } else {
                            discarded_tag = Some(tag);
                        }
                        return Some(State::NotifiedTaken);
                    }
                }
                Some(state)
            })
        };
        drop(discarded_tag);
        result
    }

    /// Notifies a number of entries.
    #[cold]
    pub(crate) fn notify(&self, mut notify: impl Notification<Tag = T>) -> usize {
        let mut tasks = DeferredTasks::new();
        // Borrow the notification so its tag/closure is also dropped outside the lock.
        self.with_inner(|inner| inner.notify(&mut notify, &mut tasks))
    }

    /// Register a task to be notified when the event is triggered.
    ///
    /// Returns the notification tag, a pending registration, or an absent slot.
    /// Cloning a replacement task may run user code, so the state is rechecked
    /// after cloning outside the lock.
    pub(crate) fn register(
        &self,
        mut listener: Pin<&mut Option<Listener<T>>>,
        task: TaskRef<'_>,
    ) -> RegisterResult<T> {
        let mut new_task = None;
        loop {
            let mut old_task = None;
            let result = self.with_inner(|inner| {
                let entry_guard = match listener.as_mut().as_pin_mut() {
                    Some(listener) => listener.link.get(),
                    None => return Some(RegisterResult::NeverInserted),
                };
                // SAFETY: We are locked, so we can access the inner `link`.
                let entry = unsafe { entry_guard.deref() };

                match entry.state.replace(State::NotifiedTaken) {
                    State::Notified { tag, .. } => {
                        inner.remove(listener.as_mut());
                        Some(RegisterResult::Notified(tag))
                    }
                    State::Task(other_task) => {
                        // TaskRef compares Waker identity or returns false for
                        // unparker pairs; it never invokes a user callback here.
                        if task.will_wake(other_task.as_task_ref()) {
                            entry.state.set(State::Task(other_task));
                            Some(RegisterResult::Registered)
                        } else if let Some(new_task) = new_task.take() {
                            entry.state.set(State::Task(new_task));
                            old_task = Some(other_task);
                            Some(RegisterResult::Registered)
                        } else {
                            entry.state.set(State::Task(other_task));
                            None
                        }
                    }
                    state => {
                        if let Some(new_task) = new_task.take() {
                            entry.state.set(State::Task(new_task));
                            Some(RegisterResult::Registered)
                        } else {
                            entry.state.set(state);
                            None
                        }
                    }
                }
            });
            drop(old_task);
            if let Some(result) = result {
                return result;
            }
            // Clone only when needed, outside the lock; then recheck notification state.
            new_task = Some(task.into_task());
        }
    }
}

impl<T> Inner<T> {
    /// Unlinks a pinned node without dropping its returned state payload.
    fn remove(&mut self, mut listener: Pin<&mut Option<Listener<T>>>) -> Option<State<T>> {
        let state = {
            let entry_guard = listener.as_mut().as_pin_mut()?.link.get();
            // SAFETY: The caller holds the list lock; the occupied slot is pinned
            // and belongs to this list until the links below have been removed.
            let entry = unsafe { entry_guard.deref() };

            let prev = entry.prev.get();
            let next = entry.next.get();

            // Unlink from the previous entry.
            match prev {
                None => self.head = next,
                Some(p) => unsafe {
                    // SAFETY: The linked predecessor remains pinned under this lock.
                    p.as_ref().next.set(next);
                },
            }

            // Unlink from the next entry.
            match next {
                None => self.tail = prev,
                Some(n) => unsafe {
                    // SAFETY: The linked successor remains pinned under this lock.
                    n.as_ref().prev.set(prev);
                },
            }

            // If this was the first unnotified entry, update the next pointer.
            if self.next == Some(entry.into()) {
                self.next = next;
            }

            // State is not structurally pinned. Take its payload without moving
            // the node. This scope ends Loom's access guard before Pin::set.
            entry.state.replace(State::Created)
        };
        listener.set(None);

        // Finish unlink bookkeeping before any notification callback can run.
        if state.is_notified() {
            self.notified -= 1;
        }
        self.len -= 1;

        Some(state)
    }

    /// Commits notifications and defers task wakes until the caller unlocks.
    #[cold]
    fn notify(
        &mut self,
        notify: &mut impl Notification<Tag = T>,
        tasks: &mut DeferredTasks,
    ) -> usize {
        let mut n = notify.count(self.notified, Internal::new());
        let is_additional = notify.is_additional(Internal::new());

        let original_count = n;
        while n > 0 {
            n -= 1;

            // Notify the next entry.
            match self.next {
                None => return original_count - n - 1,

                Some(e) => {
                    // Tag generation remains non-reentrant, but a panic must not skip an entry.
                    let tag = notify.next_tag(Internal::new());
                    self.notify_entry(e, is_additional, tag, tasks);
                }
            }
        }

        original_count - n
    }

    /// Notifies the next unnotified node and defers its task wake.
    fn notify_entry(
        &mut self,
        entry: NonNull<Link<T>>,
        additional: bool,
        tag: T,
        tasks: &mut DeferredTasks,
    ) {
        // SAFETY: The caller holds the list lock and selected its first unnotified
        // entry, which remains pinned until removal. Its state is Created or Task:
        // the notified prefix excludes it, and NotifiedTaken never survives unlock.
        // Replacing that state therefore cannot drop an earlier tag under the lock.
        let entry = unsafe { entry.as_ref() };
        self.next = entry.next.get();
        let previous = entry.state.replace(State::Notified { additional, tag });
        self.notified += 1;
        if let State::Task(task) = previous {
            tasks.push(task);
        }
    }
}

/// Publishes the committed notification count for lock-free readers.
fn update_notified<T>(slot: &crate::sync::atomic::AtomicUsize, list: &Inner<T>) {
    slot.store(list.notified, Ordering::Release);
}

/// Owns pinned storage for one listener registered in an event's list.
pub(crate) struct Listener<T> {
    /// The inner link in the linked list.
    ///
    /// # Safety
    ///
    /// Access requires the list lock, regardless of the selected backend.
    link: UnsafeCell<Link<T>>,

    /// Linked nodes stay at one address until removal drops them in place.
    _pin: PhantomPinned,
}

/// Stores node state and neighbour addresses protected by the list lock.
struct Link<T> {
    /// The current state of the listener.
    state: Cell<State<T>>,

    /// The previous link in the linked list.
    prev: Cell<Option<NonNull<Link<T>>>>,

    /// The next link in the linked list.
    next: Cell<Option<NonNull<Link<T>>>>,
}

/// Dangerous spinlock for when neither std nor critical-section are enabled.
#[cfg(not(any(feature = "std", feature = "critical-section")))]
mod spinlock {
    use crate::sync::atomic::{AtomicBool, Ordering};
    use crate::sync::cell::UnsafeCell;

    /// A spinlock.
    pub(super) struct Spinlock<T> {
        /// Is this spinlock locked?
        locked: AtomicBool,

        /// The data contained within the lock.
        data: UnsafeCell<T>,
    }

    impl<T> Spinlock<T> {
        /// Create a new [`Spinlock`].
        #[inline]
        pub(super) fn new(data: T) -> Self {
            Self {
                locked: AtomicBool::new(false),
                data: UnsafeCell::new(data),
            }
        }

        /// Try to acquire a spinlock, run a closure, then release.
        #[inline]
        pub(super) fn try_with<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
            if self
                .locked
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                let _guard = CallOnDrop(|| self.locked.store(false, Ordering::Release));
                // SAFETY: We have the lock held.
                Some(f(unsafe { self.data.get().deref_mut() }))
            } else {
                None
            }
        }

        /// Run an operation with the spinlock held.
        #[inline]
        pub(super) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
            loop {
                if self
                    .locked
                    .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    let _guard = CallOnDrop(|| self.locked.store(false, Ordering::Release));
                    // SAFETY: We have the lock held.
                    return f(unsafe { self.data.get().deref_mut() });
                }

                while self.locked.load(Ordering::Relaxed) {
                    core::hint::spin_loop();
                }
            }
        }
    }

    /// Releases the spinlock on normal return and unwinding.
    struct CallOnDrop<F: FnMut()>(F);
    impl<F: FnMut()> Drop for CallOnDrop<F> {
        #[inline]
        fn drop(&mut self) {
            (self.0)();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::pin;

    #[cfg(target_family = "wasm")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    macro_rules! make_listeners {
        ($($id:ident),*) => {
            $(
                let $id = Option::<Listener<()>>::None;
                pin!($id);
            )*
        };
    }

    #[test]
    fn insert() {
        let inner = crate::Inner::new();
        make_listeners!(listen1, listen2, listen3);

        // Register the listeners.
        inner.insert(listen1.as_mut());
        inner.insert(listen2.as_mut());
        inner.insert(listen3.as_mut());

        assert_eq!(inner.list.try_total_listeners(), Some(3));

        // Remove one.
        assert!(matches!(inner.remove(listen2, false), Some(State::Created)));
        assert_eq!(inner.list.try_total_listeners(), Some(2));

        // Remove another.
        assert!(matches!(inner.remove(listen1, false), Some(State::Created)));
        assert_eq!(inner.list.try_total_listeners(), Some(1));

        // Private test slots have no owner that unlinks them during drop.
        assert!(matches!(inner.remove(listen3, false), Some(State::Created)));
        assert_eq!(inner.list.try_total_listeners(), Some(0));
    }

    #[cfg(feature = "critical-section")]
    #[test]
    fn dropping_a_completed_listener_does_not_access_the_list() {
        use core::future::Future;
        use core::task::{Context, Waker};

        let event = crate::Event::new();
        let mut listener = event.listen();
        let inner = crate::sync::Arc::clone(&listener.listener().event);
        let mut context = Context::from_waker(Waker::noop());
        assert!(Pin::new(&mut listener).poll(&mut context).is_pending());
        assert_eq!(event.notify(1), 1);
        assert!(Pin::new(&mut listener).poll(&mut context).is_ready());

        critical_section::with(|cs| {
            // An unnecessary list access fails immediately instead of blocking.
            let _list = inner.list.0.borrow_ref_mut(cs);
            drop(listener);
        });
        assert_eq!(inner.list.try_total_listeners(), Some(0));
    }

    #[test]
    fn drop_non_notified() {
        let inner = crate::Inner::new();
        make_listeners!(listen1, listen2, listen3);

        // Register the listeners.
        inner.insert(listen1.as_mut());
        inner.insert(listen2.as_mut());
        inner.insert(listen3.as_mut());

        // Notify one.
        inner.notify(GenericNotify::new(1, false, || ()));

        // Remove one.
        inner.remove(listen3, true);

        // Remove the rest.
        inner.remove(listen1, true);
        inner.remove(listen2, true);
    }

    #[test]
    fn dynamic_policy_preserves_outstanding_notifications_during_cancellation() {
        for (additional, expected_new) in [(false, 0), (true, 1)] {
            let inner = crate::Inner::new();
            make_listeners!(first, second, third);
            inner.insert(first.as_mut());
            inner.insert(second.as_mut());
            inner.insert(third.as_mut());
            assert_eq!(inner.notify(GenericNotify::new(1, false, || ())), 1);
            assert_eq!(
                inner.notify(GenericNotify::new(1, additional, || ())),
                expected_new
            );

            let cancelled = inner.remove(second, true).unwrap();
            let remaining = inner.remove(third, false).unwrap();
            if additional {
                assert!(matches!(cancelled, State::NotifiedTaken));
                assert!(matches!(
                    remaining,
                    State::Notified {
                        additional: true,
                        ..
                    }
                ));
            } else {
                assert!(matches!(cancelled, State::Created));
                assert!(matches!(remaining, State::Created));
            }
            assert!(matches!(
                inner.remove(first, false),
                Some(State::Notified {
                    additional: false,
                    ..
                })
            ));
            assert_eq!(inner.list.try_total_listeners(), Some(0));
        }
    }
}
