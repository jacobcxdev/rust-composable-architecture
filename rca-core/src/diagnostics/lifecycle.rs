//! Lends the actor's lifecycle callback without retaining Store ownership.

use super::{LifecycleEvent, LifecycleKind};

/// Borrows the actor-owned callback for one synchronous phase.
pub(crate) struct LifecycleCall<'a> {
    /// Exclusive callback loan; reborrowing does not allocate or share ownership.
    sink: &'a mut dyn FnMut(&LifecycleEvent),
    /// Most recent committed version supplied by the caller for this phase.
    last_committed_version: u64,
}

#[cfg(test)]
ordinary_test_items! {
    mod tests {
        use super::*;
        use std::cell::Cell;

        struct ReentrantDestructor<'a>(LifecycleCall<'a>);

        impl Drop for ReentrantDestructor<'_> {
            fn drop(&mut self) {
                self.0.emit(LifecycleKind::ActorStopped);
            }
        }

        #[test]
        fn synchronous_user_destructor_reentry_never_invokes_sink_during_unwind() {
            let calls = Cell::new(0);
            let mut sink = |_: &LifecycleEvent| calls.set(calls.get() + 1);
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _destructor = ReentrantDestructor(LifecycleCall::new(&mut sink, 3));
                panic!("original user failure");
            }));
            assert_eq!(
                panic.unwrap_err().downcast_ref::<&str>(),
                Some(&"original user failure")
            );
            assert_eq!(calls.get(), 0);
            let mut sink = |_: &LifecycleEvent| calls.set(calls.get() + 1);
            drop(ReentrantDestructor(LifecycleCall::new(&mut sink, 3)));
            assert_eq!(calls.get(), 1, "normal synchronous emission remains enabled");
        }
    }
}

impl<'a> LifecycleCall<'a> {
    /// Borrows the sink with the caller's current committed-version stamp.
    pub(crate) fn new(
        sink: &'a mut dyn FnMut(&LifecycleEvent),
        last_committed_version: u64,
    ) -> Self {
        Self {
            sink,
            last_committed_version,
        }
    }

    /// Lends the same sink and stamp for a shorter synchronous operation.
    pub(crate) fn reborrow(&mut self) -> LifecycleCall<'_> {
        LifecycleCall::new(self.sink, self.last_committed_version)
    }

    /// Emits borrowed metadata unless the thread is already unwinding.
    ///
    /// A new panic from the sink propagates. Suppression only prevents a
    /// destructor from entering the sink during an existing unwind.
    pub(crate) fn emit(&mut self, kind: LifecycleKind) {
        // User destructors may call synchronous RCA APIs during an existing unwind.
        if !std::thread::panicking() {
            (self.sink)(&LifecycleEvent {
                last_committed_version: self.last_committed_version,
                kind,
            });
        }
    }
}
