use std::thread::Thread;

use futures::executor::LocalSpawner;
use futures::future::RemoteHandle;
use futures::task::LocalSpawnExt;
use futures::{pin_mut, Stream, StreamExt};

use crate::dependencies::Dependency;
use crate::store::channel::WeakSender;

/// Asynchronous work being performed by a `Store`.
///
/// A [`Store`][`crate::Store`] uses a [Local Async Executor] to run its `Task`s.
///
/// [Local Async Executor]: https://maciej.codes/2022-06-09-local-async.html
///
/// # Cancellation
/// Dropping a `Task` cancels the underlying future (it will not be polled again).
#[doc(hidden)]
#[derive(Debug)]
#[must_use = "dropping a Task cancels the underlying future"]
pub struct Task {
    pub(crate) handle: Option<RemoteHandle<()>>,
    pub(crate) when: Option<std::time::Instant>,
}

impl Task {
    /// Detaches the task; leaving its [`Future`][`std::future`] running in the background.
    pub fn detach(self) {
        if let Some(handle) = self.handle {
            handle.forget()
        }
    }

    /// Cancels the task; meaning its [`Future`][`std::future`] won’t be polled again.
    pub fn cancel(self) {
        drop(self)
    }

    pub(crate) fn new<Action: 'static, S: Stream<Item = Action> + 'static>(stream: S) -> Self {
        // Only called by “root” `Effects`, so it will be the same `Action` as used by the `Store`
        let handle =
            Dependency::<Executor<Result<Action, Thread>>>::get().and_then(
                |executor| match executor.actions.upgrade() {
                    None => None,
                    Some(sender) => executor
                        .spawner
                        .spawn_local_with_handle(async move {
                            pin_mut!(stream);
                            while let Some(action) = stream.next().await {
                                sender.send(Ok(action));
                            }
                        })
                        .ok(),
                },
            );

        Task {
            // `handle` may be `None` if the store is shutting down and the sender has been dropped.
            handle,
            when: None,
        }
    }
}

/// Dependency injected into a store runtime to enable spawning effect tasks.
pub(crate) struct Executor<Action> {
    pub(crate) spawner: LocalSpawner,
    pub(crate) actions: WeakSender<Action>,
}

impl<Action> Executor<Action> {
    pub(crate) fn new(spawner: LocalSpawner, actions: WeakSender<Action>) -> Self {
        Self { spawner, actions }
    }
}
