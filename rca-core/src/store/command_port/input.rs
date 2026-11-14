//! One owned request lends its typed payload only during synchronous dispatch.

use super::super::mounted::MountedApply;
#[cfg(feature = "test-util")]
use super::super::mounted::{SendInspection, SendSlot};
use super::super::task_context::{EnteredSend, TaskAccessError, TypedSend};
#[cfg(feature = "test-util")]
use std::sync::Arc;

/// Lets dispatch defer its single mapping call until after child projection.
pub(in crate::store) type MappedInput<'a, X> = dyn FnMut() -> X + 'a;

/// Owns the unentered command and terminal reply across admission and inspection.
pub(in crate::store) struct Request<C> {
    /// Direct storage when diagnostic borrowing is disabled.
    #[cfg(not(feature = "test-util"))]
    pending: Option<TypedSend<C>>,
    /// Shared slot that keeps the actor's input recoverable during inspection.
    #[cfg(feature = "test-util")]
    pending: Arc<SendSlot<C>>,
}

impl<C> Request<C> {
    /// Records terminal progress for a held diagnostic transaction.
    #[cfg(feature = "test-util")]
    pub(in crate::store) fn attach_terminal(
        &mut self,
        terminal: super::super::task_context::TerminalReceipt,
    ) {
        self.pending.attach_terminal(terminal);
    }

    /// Takes ownership of an admitted request without mapping its command.
    pub(in crate::store) fn new(input: TypedSend<C>) -> Self {
        #[cfg(not(feature = "test-util"))]
        {
            Self {
                pending: Some(input),
            }
        }
        #[cfg(feature = "test-util")]
        {
            Self {
                pending: Arc::new(SendSlot::new(input)),
            }
        }
    }

    /// Extracts the one request when the enclosing call rejects or enters it.
    fn take(&mut self) -> TypedSend<C> {
        // The enclosing call consumes itself on reject/apply. Borrowed extraction
        // keeps its final cleanup identical with and without publication guards.
        #[cfg(not(feature = "test-util"))]
        {
            self.pending
                .take()
                .expect("consuming call owns pending input")
        }
        #[cfg(feature = "test-util")]
        {
            self.pending.take()
        }
    }

    /// Returns the original command with a pre-entry rejection.
    pub(in crate::store) fn reject(&mut self, error: TaskAccessError) {
        self.take().reject(error);
    }

    /// Enters dispatch and drops unused input before publishing a failure.
    ///
    /// Dispatch may map once or reject without mapping. User panics propagate;
    /// the entered reply owner publishes failure after input/callback cleanup.
    pub(in crate::store) fn apply<X, W: ?Sized>(
        &mut self,
        mapper: impl FnMut(C) -> X,
        dispatch: impl FnOnce(&mut MappedInput<'_, X>) -> MountedApply<(), W>,
    ) -> MountedApply<EnteredSend<C>, W> {
        // Admission has completed, but no selector or mapper has run. The
        // declaration order keeps payload/callback destruction before terminal
        // publication on unwind, including an unconsumed selector-None input.
        let (value, reply) = self.take().enter();
        let mut mapping = Mapping {
            value: Some(value),
            mapper,
        };
        let result = dispatch(&mut || mapping.map());
        drop(mapping);
        match result {
            MountedApply::Rejected(error) => {
                reply.fail(error);
                MountedApply::Rejected(error)
            }
            MountedApply::Applied { work, scope, .. } => {
                MountedApply::Applied { reply, work, scope }
            }
        }
    }

    /// Lends the diagnostic slot without borrowing its command.
    #[cfg(feature = "test-util")]
    pub(in crate::store) fn slot(&self) -> &Arc<SendSlot<C>> {
        &self.pending
    }

    /// Borrows input through a guard that restores it when inspection ends.
    #[cfg(feature = "test-util")]
    pub(in crate::store) fn inspect(slot: Arc<SendSlot<C>>) -> SendInspection<C> {
        SendInspection::take(slot)
    }
}

/// Keeps the unmapped payload and callback together for ordered unwind cleanup.
struct Mapping<C, M> {
    /// Becomes empty at mapper entry, so failure cannot return a consumed input.
    value: Option<C>,
    /// Actor-owned conversion invoked only after destination projection.
    mapper: M,
}

impl<C, M> Mapping<C, M> {
    /// Consumes the input on dispatch's sole mapping call.
    fn map<X>(&mut self) -> X
    where
        M: FnMut(C) -> X,
    {
        (self.mapper)(self.value.take().expect("command maps once"))
    }
}
