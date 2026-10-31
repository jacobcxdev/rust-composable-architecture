//! Defines operational RCA errors.
//!
//! Immediate admission errors retain the unsent action for recovery. Formatting
//! those errors reports the cause without inspecting the action. Other errors
//! describe lost authority or failed completion and do not return an action.
//!
//! Backtraces record error construction, subject to the standard library's
//! capture settings. They do not identify when a remote actor first stopped.

use std::backtrace::Backtrace;
use std::fmt;

/// Configuration failures checked before a Store starts accepting work.
#[derive(Debug)]
enum StoreBuildErrorKind {
    /// A bounded mailbox must have room for at least one command.
    ZeroCapacity,
    /// A test gate is attached or still reserved by unfinished work.
    #[cfg(feature = "test-util")]
    AssertionGateAttached,
}

/// Reports invalid store configuration.
pub struct StoreBuildError {
    /// The rejected configuration, independent of diagnostic formatting.
    kind: StoreBuildErrorKind,
    /// Construction site, captured only when standard-library settings enable it.
    backtrace: Backtrace,
}

impl StoreBuildError {
    /// Records a test gate that is attached or awaiting inspection/retirement.
    #[cfg(feature = "test-util")]
    pub(crate) fn assertion_gate_attached() -> Self {
        Self {
            kind: StoreBuildErrorKind::AssertionGateAttached,
            backtrace: Backtrace::capture(),
        }
    }

    /// Records a zero-capacity mailbox rejected by the builder.
    pub(crate) fn zero_capacity() -> Self {
        Self {
            kind: StoreBuildErrorKind::ZeroCapacity,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns whether mailbox capacity was zero.
    #[must_use]
    pub fn is_zero_capacity(&self) -> bool {
        matches!(self.kind, StoreBuildErrorKind::ZeroCapacity)
    }

    /// Returns whether the test gate is attached or reserved by unfinished work.
    ///
    /// An inspection or retirement can retain the reservation after its actor
    /// detaches. This applies to assertion and transaction gates.
    #[cfg(feature = "test-util")]
    #[must_use]
    pub fn is_assertion_gate_attached(&self) -> bool {
        matches!(self.kind, StoreBuildErrorKind::AssertionGateAttached)
    }

    /// Returns captured construction backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Debug for StoreBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreBuildError")
            .field("kind", &self.kind)
            .field("backtrace", &self.backtrace)
            .finish()
    }
}

impl fmt::Display for StoreBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            #[cfg(feature = "test-util")]
            StoreBuildErrorKind::AssertionGateAttached => {
                write!(
                    f,
                    "test gate is still attached or reserved by unfinished work\n{}",
                    self.backtrace
                )
            }
            StoreBuildErrorKind::ZeroCapacity => {
                write!(
                    f,
                    "store mailbox capacity must exceed zero\n{}",
                    self.backtrace
                )
            }
        }
    }
}

impl std::error::Error for StoreBuildError {}

/// Reports failure to obtain a successful Store completion.
///
/// The producing operation defines the cause. In particular, an
/// [`crate::ActionReceipt`] can return this error when an admitted scoped command
/// loses its scope while the root Store remains live. This error alone proves
/// neither root shutdown nor destruction of all runtime-owned work.
pub struct StoreStopped {
    /// Site where the missing completion or authority was observed.
    backtrace: Backtrace,
}

impl StoreStopped {
    /// Captures the caller's failed completion or authority check.
    pub(crate) fn new() -> Self {
        Self {
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns captured construction backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Debug for StoreStopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreStopped")
            .field("backtrace", &self.backtrace)
            .finish()
    }
}

impl fmt::Display for StoreStopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "store operation ended before successful completion\n{}",
            self.backtrace
        )
    }
}

impl std::error::Error for StoreStopped {}

/// Distinguishes delivery cancellation, input closure and root Store loss.
#[derive(Debug)]
enum EffectSendErrorKind {
    /// The captured task or installation no longer permits this delivery.
    Cancelled,
    /// The destination rejects ordinary input without ending its installation.
    InputClosed,
    /// The root actor cannot acknowledge the requested delivery.
    StoreStopped,
}

/// Reports failed action delivery from an effect.
///
/// The action is not recoverable from this error. Input closure differs from
/// task cancellation and does not itself remove the destination Feature.
pub struct EffectSendError {
    /// The authority check or acknowledgement that prevented delivery.
    kind: EffectSendErrorKind,
    /// Site where the failure was returned, not the origin of actor shutdown.
    backtrace: Backtrace,
}

impl EffectSendError {
    /// Records a live destination whose ordinary input gate is closed.
    pub(crate) fn input_closed() -> Self {
        Self {
            kind: EffectSendErrorKind::InputClosed,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns whether ordinary input was closed without cancelling this task.
    #[must_use]
    pub fn is_input_closed(&self) -> bool {
        matches!(self.kind, EffectSendErrorKind::InputClosed)
    }

    /// Records revoked task or installation delivery authority.
    pub(crate) fn cancelled() -> Self {
        Self {
            kind: EffectSendErrorKind::Cancelled,
            backtrace: Backtrace::capture(),
        }
    }

    /// Records loss of the root actor's delivery acknowledgement.
    pub(crate) fn store_stopped() -> Self {
        Self {
            kind: EffectSendErrorKind::StoreStopped,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns whether task cancellation or an ended scope revoked this delivery.
    ///
    /// An ended scope can reject delivery while the task itself is not cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self.kind, EffectSendErrorKind::Cancelled)
    }

    /// Returns whether owning store stopped.
    #[must_use]
    pub fn is_store_stopped(&self) -> bool {
        matches!(self.kind, EffectSendErrorKind::StoreStopped)
    }

    /// Returns captured construction backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Debug for EffectSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EffectSendError")
            .field("kind", &self.kind)
            .field("backtrace", &self.backtrace)
            .finish()
    }
}

impl fmt::Display for EffectSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.kind {
            EffectSendErrorKind::Cancelled => "effect action delivery was cancelled",
            EffectSendErrorKind::InputClosed => "mounted input is closed",
            EffectSendErrorKind::StoreStopped => "store stopped before effect action delivery",
        };
        write!(f, "{message}\n{}", self.backtrace)
    }
}

impl std::error::Error for EffectSendError {}

/// Rejection reasons for immediate root-mailbox admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrySendErrorKind {
    /// No capacity was available at the admission attempt.
    Full,
    /// The mailbox no longer accepts commands.
    StoreStopped,
}

/// Reports immediate action admission failure.
///
/// The action was not admitted. Recover it with [`Self::into_action`], or borrow
/// it with [`Self::action`]. Dropping the error drops the action. Formatting never
/// requires or calls the action's `Debug` implementation.
pub struct TrySendError<Action> {
    /// Whether retry needs capacity or a different live Store.
    kind: TrySendErrorKind,
    /// Original rejected value, moved here without cloning or type erasure.
    action: Action,
    /// Site of the failed admission attempt.
    backtrace: Backtrace,
}

impl<Action> TrySendError<Action> {
    /// Returns ownership when immediate admission finds no capacity.
    pub(crate) fn full(action: Action) -> Self {
        Self {
            kind: TrySendErrorKind::Full,
            action,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns ownership when root admission authority has ended.
    pub(crate) fn store_stopped(action: Action) -> Self {
        Self {
            kind: TrySendErrorKind::StoreStopped,
            action,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns whether Store ingress was full.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.kind == TrySendErrorKind::Full
    }

    /// Returns whether Store authority ended.
    #[must_use]
    pub fn is_store_stopped(&self) -> bool {
        self.kind == TrySendErrorKind::StoreStopped
    }

    /// Borrows the unsent action.
    #[must_use]
    pub const fn action(&self) -> &Action {
        &self.action
    }

    /// Returns the unsent action.
    #[must_use]
    pub fn into_action(self) -> Action {
        self.action
    }

    /// Returns captured construction backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl<Action> fmt::Debug for TrySendError<Action> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrySendError")
            .field("kind", &self.kind)
            .field("backtrace", &self.backtrace)
            .finish_non_exhaustive()
    }
}

impl<Action> fmt::Display for TrySendError<Action> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.kind {
            TrySendErrorKind::Full => "store ingress is full",
            TrySendErrorKind::StoreStopped => "store stopped before action admission",
        };
        write!(f, "{message}\n{}", self.backtrace)
    }
}

impl<Action> std::error::Error for TrySendError<Action> {}

/// Distinguishes child-scope invalidation from failure of its root Store.
#[derive(Debug)]
enum ScopedStoreEndedKind {
    /// This installation ended, even if the root Store is still live.
    ScopeEnded,
    /// Retains the root failure for standard error-chain inspection.
    StoreStopped(StoreStopped),
}

/// Reports required scoped Store closure before requested work completed.
///
/// A root failure is available through [`std::error::Error::source`]. A replaced
/// or removed child has no underlying root failure. The old scope does not become
/// valid again if a child is installed later.
pub struct ScopedStoreEnded {
    /// Lost child authority or the retained root failure.
    kind: ScopedStoreEndedKind,
    /// Scoped failure boundary, separate from a retained root error's trace.
    backtrace: Backtrace,
}

impl ScopedStoreEnded {
    /// Records invalidation of this specific child-scope incarnation.
    pub(crate) fn scope_ended() -> Self {
        Self {
            kind: ScopedStoreEndedKind::ScopeEnded,
            backtrace: Backtrace::capture(),
        }
    }

    /// Adds scoped context while retaining the original root error as its cause.
    pub(crate) fn store_stopped(cause: StoreStopped) -> Self {
        Self {
            kind: ScopedStoreEndedKind::StoreStopped(cause),
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns whether this specific scope ended.
    #[must_use]
    pub fn is_scope_ended(&self) -> bool {
        matches!(self.kind, ScopedStoreEndedKind::ScopeEnded)
    }

    /// Returns whether root Store authority ended.
    #[must_use]
    pub fn is_store_stopped(&self) -> bool {
        matches!(self.kind, ScopedStoreEndedKind::StoreStopped(_))
    }

    /// Returns captured construction backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Debug for ScopedStoreEnded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopedStoreEnded")
            .field("kind", &self.kind)
            .field("backtrace", &self.backtrace)
            .finish()
    }
}

impl fmt::Display for ScopedStoreEnded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ScopedStoreEndedKind::ScopeEnded => {
                write!(
                    f,
                    "scoped store ended before work completed\n{}",
                    self.backtrace
                )
            }
            ScopedStoreEndedKind::StoreStopped(cause) => write!(
                f,
                "root store stopped before scoped work completed\n{}\n{cause}",
                self.backtrace
            ),
        }
    }
}

impl std::error::Error for ScopedStoreEnded {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ScopedStoreEndedKind::ScopeEnded => None,
            ScopedStoreEndedKind::StoreStopped(cause) => Some(cause),
        }
    }
}

/// Rejection reasons before immediate scoped action admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TryScopedSendErrorKind {
    /// Root mailbox capacity was unavailable for this attempt.
    Full,
    /// This specific child scope has ended.
    ScopeEnded,
    /// The root Store no longer accepts the command.
    StoreStopped,
}

/// Reports failed immediate action admission through a scoped Store.
///
/// The action was not admitted and can be recovered with [`Self::into_action`].
/// Dropping the error drops the action. Formatting omits the action and adds no
/// `Debug` bound. A full mailbox can become available; an ended scope does not
/// regain authority when another child is installed.
pub struct TryScopedSendError<Action> {
    /// Capacity, child lifetime or root lifetime prevented admission.
    kind: TryScopedSendErrorKind,
    /// Original action that the caller still owns through this error.
    action: Action,
    /// Site of the failed scoped admission attempt.
    backtrace: Backtrace,
}

impl<Action> TryScopedSendError<Action> {
    /// Returns ownership when the root mailbox has no immediate capacity.
    pub(crate) fn full(action: Action) -> Self {
        Self {
            kind: TryScopedSendErrorKind::Full,
            action,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns ownership when the selected child incarnation has ended.
    pub(crate) fn scope_ended(action: Action) -> Self {
        Self {
            kind: TryScopedSendErrorKind::ScopeEnded,
            action,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns ownership when the root no longer accepts commands.
    pub(crate) fn store_stopped(action: Action) -> Self {
        Self {
            kind: TryScopedSendErrorKind::StoreStopped,
            action,
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns whether scoped ingress was full.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.kind == TryScopedSendErrorKind::Full
    }

    /// Returns whether the specific scope ended.
    #[must_use]
    pub fn is_scope_ended(&self) -> bool {
        self.kind == TryScopedSendErrorKind::ScopeEnded
    }

    /// Returns whether root Store authority ended.
    #[must_use]
    pub fn is_store_stopped(&self) -> bool {
        self.kind == TryScopedSendErrorKind::StoreStopped
    }

    /// Borrows the unsent action.
    #[must_use]
    pub const fn action(&self) -> &Action {
        &self.action
    }

    /// Returns the unsent action.
    #[must_use]
    pub fn into_action(self) -> Action {
        self.action
    }

    /// Returns captured construction backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl<Action> fmt::Debug for TryScopedSendError<Action> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TryScopedSendError")
            .field("kind", &self.kind)
            .field("backtrace", &self.backtrace)
            .finish_non_exhaustive()
    }
}

impl<Action> fmt::Display for TryScopedSendError<Action> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self.kind {
            TryScopedSendErrorKind::Full => "scoped store ingress is full",
            TryScopedSendErrorKind::ScopeEnded => "scoped store ended before action admission",
            TryScopedSendErrorKind::StoreStopped => {
                "root store stopped before scoped action admission"
            }
        };
        write!(f, "{message}\n{}", self.backtrace)
    }
}

impl<Action> std::error::Error for TryScopedSendError<Action> {}

/// Reports incomplete graceful Store shutdown.
///
/// The actor stopped before acknowledging the graceful shutdown request. This
/// error does not acknowledge destruction of work retained by the runtime.
pub struct ShutdownError {
    /// Site where graceful shutdown lost its actor acknowledgement.
    backtrace: Backtrace,
}

impl ShutdownError {
    /// Records failure to obtain a graceful-shutdown acknowledgement.
    pub(crate) fn store_stopped() -> Self {
        Self {
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns whether Store authority ended before graceful shutdown.
    ///
    /// This is currently the only shutdown failure, so this returns `true`.
    #[must_use]
    pub const fn is_store_stopped(&self) -> bool {
        true
    }

    /// Returns captured construction backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Debug for ShutdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShutdownError")
            .field("backtrace", &self.backtrace)
            .finish()
    }
}

impl fmt::Display for ShutdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "store stopped before graceful shutdown\n{}",
            self.backtrace
        )
    }
}

impl std::error::Error for ShutdownError {}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use super::{
        EffectSendError, ShutdownError, StoreBuildError, StoreStopped, TryScopedSendError,
        TrySendError,
    };

    // Deliberately not Debug: formatting an error must not inspect the unsent action.
    struct PrivateAction(&'static str);

    #[test]
    fn store_build_error_identifies_zero_capacity() {
        let error = StoreBuildError::zero_capacity();

        assert!(error.is_zero_capacity());
        assert!(error.to_string().starts_with("store mailbox capacity"));
    }

    #[test]
    fn store_stopped_has_stable_summary() {
        let error = StoreStopped::new();

        assert!(error.to_string().starts_with("store operation ended"));
    }

    #[test]
    fn effect_send_error_distinguishes_causes() {
        let cancelled = EffectSendError::cancelled();
        assert!(cancelled.is_cancelled());
        assert!(!cancelled.is_store_stopped());

        let stopped = EffectSendError::store_stopped();
        assert!(!stopped.is_cancelled());
        assert!(stopped.is_store_stopped());
    }

    #[test]
    fn shutdown_error_identifies_stopped_store() {
        let error = ShutdownError::store_stopped();

        assert!(error.is_store_stopped());
        assert!(error.to_string().starts_with("store stopped"));
    }

    #[test]
    fn try_send_error_formatting_preserves_cause_without_exposing_action() {
        let private = "private-unsent-action";
        for (error, kind, summary) in [
            (
                TrySendError::full(PrivateAction(private)),
                "Full",
                "store ingress is full",
            ),
            (
                TrySendError::store_stopped(PrivateAction(private)),
                "StoreStopped",
                "store stopped before action admission",
            ),
        ] {
            for debug in [format!("{error:?}"), format!("{error:#?}")] {
                assert!(debug.starts_with("TrySendError"));
                assert!(debug.contains(&format!("kind: {kind}")));
                assert!(debug.contains("backtrace"));
                assert!(!debug.contains(private));
            }
            let display = error.to_string();
            assert_eq!(display.lines().next(), Some(summary));
            assert!(display.ends_with(&error.backtrace().to_string()));
            assert!(!display.contains(private));
            assert_eq!(error.into_action().0, private);
        }
    }

    #[test]
    fn try_scoped_send_error_formatting_distinguishes_scope_from_root_without_payload() {
        let private = "private-scoped-action";
        for (error, kind, summary) in [
            (
                TryScopedSendError::full(PrivateAction(private)),
                "Full",
                "scoped store ingress is full",
            ),
            (
                TryScopedSendError::scope_ended(PrivateAction(private)),
                "ScopeEnded",
                "scoped store ended before action admission",
            ),
            (
                TryScopedSendError::store_stopped(PrivateAction(private)),
                "StoreStopped",
                "root store stopped before scoped action admission",
            ),
        ] {
            for debug in [format!("{error:?}"), format!("{error:#?}")] {
                assert!(debug.starts_with("TryScopedSendError"));
                assert!(debug.contains(&format!("kind: {kind}")));
                assert!(debug.contains("backtrace"));
                assert!(!debug.contains(private));
            }
            let display = error.to_string();
            assert_eq!(display.lines().next(), Some(summary));
            assert!(display.ends_with(&error.backtrace().to_string()));
            assert!(!display.contains(private));
            assert_eq!(error.into_action().0, private);
        }
    }
}
}
