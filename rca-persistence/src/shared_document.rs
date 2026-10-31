//! Bridges document persistence, reload authority and explicitly owned producer sessions.
//!
//! Plain and revisioned constructors share the ordinary Document's private
//! revision/barrier transitions. Backend revisions, local edit revisions, load
//! attempts and subscription lifetimes remain different identities. Retain the
//! external session before moving the reducer into a Store, and close it after
//! stopping its owner; delivery cancellation alone never proves physical join.

use std::any::Any;
use std::backtrace::{Backtrace, BacktraceStatus};
use std::error::Error;
use std::fmt;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Wake, Waker};

use rca_core::{
    Effect, ReduceContext, Reducer, ScopedStore, SharedAccessError, SharedKey, SharedKeyIdentity,
    SharedLoadContext, SharedReaderKey, SharedSubscribeContext, SharedSubscriber, WeakScopedStore,
};

use crate::document::{Cause, capture};
use crate::document_transitions::{Request, Transitions};
use crate::receipt::{Reply, Resolution, channel, deliver};
use crate::revision::{
    BackendSession, BackendSnapshot, CompareSave, NoBackendRevision, RevisionBackend,
};
use crate::{
    Attempt, Codec, Failure, Operation, Receipt, SharedProducer, SharedSubscription, SharedUpdate,
    SharedUpdates, State,
};

/// Owns backend or producer work independently of the reducer and its state.
type OwnedFuture<Output> = Pin<Box<dyn Future<Output = Output> + Send + 'static>>;
/// Copies a value according to the owner's snapshot policy.
type Capture<Value> = dyn Fn(&Value) -> Value + Send + Sync;
/// Keeps the store alive while an external owner submits document actions.
type DocumentRoute<Value, Revision> =
    ScopedStore<SharedDocumentState<Value, Revision>, SharedDocumentAction<Value, Revision>>;
/// Lets keys submit actions without extending the store's lifetime.
type WeakDocumentRoute<Value, Revision> =
    WeakScopedStore<SharedDocumentState<Value, Revision>, SharedDocumentAction<Value, Revision>>;
/// Delivers one load result after the actor commits the corresponding state.
type LoadReply<Value> = async_channel::Sender<Result<Option<Value>, DocumentKeyError>>;
/// Transfers one producer effect from a borrowed action into the actor.
type PendingEffect<Value, Revision> =
    Mutex<Option<Box<Effect<SharedDocumentAction<Value, Revision>>>>>;
/// Starts a read using the backend and codec retained by the state's binding.
type Load<Value, Revision> =
    dyn Fn() -> OwnedFuture<Result<Loaded<Value, Revision>, Failure>> + Send + Sync;
/// Saves an owned value using its backend comparison and local acknowledgement revisions.
type Save<Value, Revision> = dyn Fn(Value, Revision, u64) -> OwnedFuture<Result<Saved<Value, Revision>, Failure>>
    + Send
    + Sync;
/// Decodes plain bytes and pairs them with the private unversioned marker.
type PlainDecode<Value, Revision> =
    dyn Fn(Option<Vec<u8>>) -> Loaded<Value, Revision> + Send + Sync;
/// Decodes only snapshots with the backend's actual comparison capability.
enum Decode<Value, Revision> {
    /// Plain snapshots carry bytes but no external comparison identity.
    Plain(
        /// Produces the private plain-storage marker with the decoded bytes.
        Box<PlainDecode<Value, Revision>>,
    ),
    /// Revisioned snapshots must supply their concrete comparison identity.
    Revisioned(
        /// Preserves that identity beside raw and decoded evidence.
        Box<dyn Fn(BackendSnapshot<Revision>) -> Loaded<Value, Revision> + Send + Sync>,
    ),
}

/// Retains raw and decoded load evidence until the actor accepts or rejects it.
#[derive(Clone)]
struct Loaded<Value, Revision> {
    /// Backend comparison identity, or the zero-sized marker for plain storage.
    revision: Revision,
    /// Exact loaded bytes, absent when storage has no value.
    bytes: Option<Vec<u8>>,
    /// Decoded value or original codec failure, independent of storage presence.
    decoded: Result<Option<Value>, Failure>,
}

/// Distinguishes a successful write from an atomic comparison failure.
enum Saved<Value, Revision> {
    /// Carries the new backend identity, or the plain storage marker.
    Committed(
        /// Identity returned by the successful write.
        Revision,
    ),
    /// Retains the external value that prevented the write.
    Conflict(
        /// Original conflicting backend evidence.
        SharedConflict<Value, Revision>,
    ),
}

/// Retains actual conflicting backend revision, raw bytes and decoded evidence.
pub struct SharedConflict<Value, Revision> {
    /// Backend identity observed by the failed comparison or conflicting reload.
    revision: Revision,
    /// Original external bytes, including bytes that the codec cannot decode.
    bytes: Option<Vec<u8>>,
    /// Decoded external value or its original codec failure.
    decoded: Result<Option<Value>, Failure>,
}

impl<Value, Revision> SharedConflict<Value, Revision> {
    /// Returns the conflicting backend identity, not the local edit revision.
    #[must_use]
    pub const fn revision(&self) -> &Revision {
        &self.revision
    }

    /// Returns exact external bytes, or absence.
    #[must_use]
    pub fn bytes(&self) -> Option<&[u8]> {
        self.bytes.as_deref()
    }

    /// Returns decoded external value when storage was present and decoding succeeded.
    #[must_use]
    pub fn value(&self) -> Option<&Value> {
        self.decoded.as_ref().ok().and_then(Option::as_ref)
    }

    /// Returns an original decode failure without discarding the raw evidence.
    #[must_use]
    pub fn decode_error(&self) -> Option<&Failure> {
        self.decoded.as_ref().err()
    }
}

impl<Value: Clone, Revision: Clone> Clone for SharedConflict<Value, Revision> {
    fn clone(&self) -> Self {
        Self {
            revision: self.revision.clone(),
            bytes: self.bytes.clone(),
            decoded: self.decoded.clone(),
        }
    }
}

impl<Value, Revision> fmt::Debug for SharedConflict<Value, Revision> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedConflict").finish_non_exhaustive()
    }
}

/// Identifies a failed atomic comparison; inspect the document's retained conflict evidence.
#[derive(Clone, Copy, Debug)]
pub struct ConflictDetected;

impl fmt::Display for ConflictDetected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("backend revision changed before save")
    }
}
impl Error for ConflictDetected {}

/// Holds independently fenced reload state alongside the authoritative local document state.
pub struct SharedDocumentState<Value, Revision = NoBackendRevision> {
    /// Resolves this state's backend and codec, never an interpreter's logical ID.
    binding: Weak<Binding<Value, Revision>>,
    /// Identifies the external owner responsible for admission and physical joins.
    session: Weak<SessionInner>,
    /// Owns the value, edit revisions and persistence barriers.
    document: State<Value>,
    /// Latest reload attempt whose completion can still change this state.
    loading: Option<Attempt>,
    /// Latest successfully committed save attempt.
    last_save: Option<Attempt>,
    /// Save identity captured when the current reload began.
    load_started_after: Option<Attempt>,
    /// External snapshot held until an overlapping save resolves its comparison.
    deferred: Option<(Attempt, Loaded<Value, Revision>)>,
    /// Failure from the latest accepted load or decode.
    load_error: Option<Failure>,
    /// Loaded or saved comparison identity, with the private marker for plain storage.
    backend_revision: Option<Revision>,
    /// Hides the plain storage marker from the public comparison-identity accessor.
    revisioned: bool,
    /// External evidence preserved while local edits remain unacknowledged.
    conflict: Option<SharedConflict<Value, Revision>>,
    /// Distinguishes an accepted value from an unmaterialised seed.
    present: bool,
    /// One caller awaiting the current reload, with its cancellation authority.
    reply: Option<PendingLoad<Value>>,
    /// Weak cache subscribers updated after the actor accepts a value.
    subscribers: Vec<ReferenceSubscription<Value>>,
    /// Weak producer registrations cancelled when this state is aborted.
    producers: Vec<Weak<Registration>>,
}

/// Prepares user-defined clones and comparisons before the final cancellation locks.
struct PreparedLoad<Value, Revision> {
    /// A newer successful save makes this independent reload obsolete.
    superseded: bool,
    /// Local edits or a correlated save prevent replacement of the local value.
    has_pending: bool,
    /// A different backend revision conflicts with pending local edits.
    conflicted: bool,
    /// Snapshot retained until the overlapping save reports its result.
    deferred: Option<(Attempt, Loaded<Value, Revision>)>,
    /// Prepared conflict replacement, leaving old evidence owned by the caller.
    conflict: Replacement<Option<SharedConflict<Value, Revision>>>,
    /// Prepared failure replacement, dropped outside the cancellation locks.
    error: Option<Failure>,
    /// Prepared external value or absence, distinct from retaining the old value.
    accepted: Replacement<Option<Value>>,
    /// Comparison identity paired with the prepared external value.
    backend_revision: Option<Revision>,
}

/// Distinguishes no update from replacing a field with an optional empty value.
enum Replacement<Value> {
    /// Leaves the current field unchanged.
    Keep,
    /// Exchanges the prepared value with the old field under a commit lock.
    Replace(
        /// New field value, then the displaced value after the exchange.
        Value,
    ),
}

impl<Value, Revision> SharedDocumentState<Value, Revision> {
    /// Returns the current retained value, including while reloads are pending.
    #[must_use]
    pub fn value(&self) -> &Value {
        self.document.value()
    }

    /// Returns the current local edit revision.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.document.revision()
    }

    /// Returns only a genuinely acknowledged local revision.
    #[must_use]
    pub fn acknowledged_revision(&self) -> Option<u64> {
        self.document.acknowledged_revision()
    }

    /// Reports whether the latest started reload still owns completion authority.
    #[must_use]
    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
            && self.reply.as_ref().is_none_or(|pending| {
                pending.token.is_active()
                    && pending.authority.as_ref().is_none_or(|authority| {
                        authority
                            .upgrade()
                            .is_some_and(|registration| registration.active.load(Ordering::Acquire))
                    })
            })
    }

    /// Returns the latest authoritative load's original typed failure.
    #[must_use]
    pub const fn load_error(&self) -> Option<&Failure> {
        self.load_error.as_ref()
    }

    /// Returns the latest acknowledged or loaded backend identity when supported.
    #[must_use]
    pub const fn backend_revision(&self) -> Option<&Revision> {
        if self.revisioned {
            self.backend_revision.as_ref()
        } else {
            None
        }
    }

    /// Returns external evidence retained without overwriting pending local edits.
    #[must_use]
    pub const fn conflict(&self) -> Option<&SharedConflict<Value, Revision>> {
        self.conflict.as_ref()
    }

    /// Returns the existing persistence state and barrier diagnostics.
    #[must_use]
    pub const fn document(&self) -> &State<Value> {
        &self.document
    }
}

impl<Value, Revision> fmt::Debug for SharedDocumentState<Value, Revision> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedDocumentState")
            .field("loading", &self.is_loading())
            .finish_non_exhaustive()
    }
}

/// Requests shared document operations without exposing completion constructors.
pub struct SharedDocumentAction<Value, Revision = NoBackendRevision>(
    /// Separates public commands from completions that only this module constructs.
    SharedKind<Value, Revision>,
);

/// Routes user commands, key requests and private I/O completions through one actor.
enum SharedKind<Value, Revision> {
    /// Reads storage and replaces any older reload attempt.
    Reload,
    /// Retires the latest reload without claiming physical I/O completion.
    CancelLoad,
    /// Commits an edit and captures its persistence barrier together.
    SetAndFlush {
        /// Value copied into state and subsequently into a save snapshot.
        value: Value,
        /// Acknowledges the captured edit revision after a successful save.
        reply: Reply,
    },
    /// Captures a barrier, optionally preventing further edits.
    Flush {
        /// Permanently closes logical editing when true.
        closing: bool,
        /// Acknowledges the revision present when this action is reduced.
        reply: Reply,
    },
    /// Aborts whichever document currently occupies this route.
    Abort,
    /// Stops only the document owned by an external owner's original session.
    OwnerClose {
        /// Remains distinct if a live required route now contains replacement state.
        session: Weak<SessionInner>,
    },
    /// Starts a key load only for its original session and live caller.
    KeyLoad {
        /// Exact session that issued the key.
        session: Weak<SessionInner>,
        /// Cancellation authority shared with the caller's load future.
        token: Arc<LoadToken>,
        /// Receives the accepted value, absence or load failure.
        reply: LoadReply<Value>,
        /// Copies the actor-owned value for this key's consumer.
        capture: Arc<Capture<Value>>,
    },
    /// Retires a dropped key load without affecting a newer load.
    KeyCancel {
        /// Exact session that issued the cancelled load.
        session: Weak<SessionInner>,
        /// Identifies the load to cancel, even if the route remains live.
        token: Arc<LoadToken>,
    },
    /// Saves a value captured by a key before its future is first polled.
    KeySave {
        /// Exact session that issued the key.
        session: Weak<SessionInner>,
        /// Owned snapshot captured at the public save call.
        value: Value,
        /// Reports persistence of the committed local revision.
        reply: Reply,
    },
    /// Registers a weak shared-cache subscriber at an actor commit boundary.
    Subscribe {
        /// Exact session that owns the requested subscription.
        session: Weak<SessionInner>,
        /// Registration that fails if the action is discarded before admission.
        pending: PendingRegistration<Value>,
    },
    /// Transfers one external producer's delivery effect into the actor.
    Observe {
        /// Exact session that owns the producer and its physical join.
        session: Weak<SessionInner>,
        /// One-use handoff that fails if the action is discarded.
        pending: PendingProducer<Value, Revision>,
    },
    /// Applies a producer snapshot or requests a new read when its snapshot is stale.
    ExternalLoad {
        /// Exact session under which the producer was admitted.
        session: Weak<SessionInner>,
        /// Registration that can revoke this delivery before the final commit.
        registration: Weak<Registration>,
        /// Acknowledges this update before the producer delivers its next update.
        reply: LoadReply<Value>,
        /// Copies values for the producer's acknowledgement.
        capture: Arc<Capture<Value>>,
        /// Observed snapshot and registration-time save epoch, or a reload hint.
        snapshot: Option<(u64, BackendSnapshot<Option<Revision>>)>,
    },
    /// Publishes the producer's final status after all delivered updates.
    ProducerTerminal {
        /// Exact session under which the producer was admitted.
        session: Weak<SessionInner>,
        /// Result whose destructor closes a discarded terminal delivery.
        terminal: ProducerTerminal,
    },
    /// Completes one reload while retaining its actual resource owner.
    Loaded {
        /// Keeps the backend binding alive until the action is reduced or discarded.
        _session: Arc<SessionInner>,
        /// Excludes older or cancelled reload completions.
        attempt: Attempt,
        /// Raw and decoded backend evidence, or the original load failure.
        result: Result<Loaded<Value, Revision>, Failure>,
    },
    /// Completes one save while retaining its actual resource owner.
    Saved {
        /// Keeps the backend binding alive until the action is reduced or discarded.
        _session: Arc<SessionInner>,
        /// Identifies the save independently of its local edit revision.
        attempt: Attempt,
        /// Local revision represented by the saved snapshot.
        revision: u64,
        /// Successful backend identity, comparison conflict or original failure.
        result: Result<Saved<Value, Revision>, Failure>,
    },
}

/// Owns the acknowledgement and authority for one key or producer reload.
struct PendingLoad<Value> {
    /// Revoked when the caller drops its future or a newer load supersedes it.
    token: Arc<LoadToken>,
    /// Receives one result after the final actor commit check.
    reply: LoadReply<Value>,
    /// Copies the accepted value without borrowing actor state across an await.
    capture: Arc<Capture<Value>>,
    /// Additional producer authority, absent for an ordinary key load.
    authority: Option<Weak<Registration>>,
}

/// Serialises caller cancellation with the last load-commit check.
struct LoadToken {
    /// Fast cancellation check before work and under the commit lock.
    active: AtomicBool,
    /// Prevents cancellation from racing the accepted-value exchange.
    commit: Mutex<()>,
}

impl LoadToken {
    /// Creates authority for a single new load.
    fn new() -> Self {
        Self {
            active: AtomicBool::new(true),
            commit: Mutex::new(()),
        }
    }

    /// Observes cancellation before work and under the final commit lock.
    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Changes authority only while no load can commit through this token.
    fn cancel(&self) {
        let _commit = self.commit.lock().expect("document load token poisoned");
        self.active.store(false, Ordering::Release);
    }
}

/// Transfers one producer effect without requiring its stream to implement Clone.
struct PendingProducer<Value, Revision> {
    /// Fails admission if this handoff is discarded before actor validation.
    registration: Arc<Registration>,
    /// Effect taken once from the borrowed private action.
    effect: PendingEffect<Value, Revision>,
}

impl<Value, Revision> Drop for PendingProducer<Value, Revision> {
    fn drop(&mut self) {
        if self.registration.status().is_pending() {
            self.registration
                .fail(DocumentKeyError::new(KeyErrorKind::Ended));
        }
    }
}

/// Retains the final producer result until actor validation or delivery teardown.
struct ProducerTerminal {
    /// Receives the terminal status or an Ended failure if delivery is lost.
    registration: Arc<Registration>,
    /// Original producer result, not a physical-join acknowledgement.
    outcome: Result<(), DocumentKeyError>,
}

impl Drop for ProducerTerminal {
    fn drop(&mut self) {
        if self.registration.active.load(Ordering::Acquire) {
            self.registration
                .fail(DocumentKeyError::new(KeyErrorKind::Ended));
        }
    }
}

/// Ends the registration when a delivery task stops before its terminal handoff.
struct ProducerDelivery {
    /// Registration whose status survives task cancellation.
    registration: Arc<Registration>,
    /// True only after the actor accepts the terminal action into its queue.
    completed: bool,
}

impl Drop for ProducerDelivery {
    fn drop(&mut self) {
        if !self.completed {
            self.registration
                .fail(DocumentKeyError::new(KeyErrorKind::Ended));
        }
    }
}

/// Separates routing, persistence and producer failures without exposing exhaustive variants.
#[derive(Clone, Copy, Debug)]
enum KeyErrorKind {
    /// The original route or delivery task no longer exists.
    Ended,
    /// Immediate actor admission failed because its queue was full.
    Full,
    /// The original session no longer accepts requests.
    Closed,
    /// The route now refers to a state owned by another session.
    WrongSession,
    /// A newer reload replaced this caller's request.
    Superseded,
    /// The caller or registration explicitly revoked delivery.
    Cancelled,
    /// Loading or decoding failed, or a snapshot used the wrong revision mode.
    Load,
    /// The captured persistence barrier failed.
    Save,
    /// The shared value was busy or poisoned during publication.
    Access,
    /// An external revision conflicts with pending local edits.
    Conflict,
    /// The external producer reported a failure.
    Producer,
}

/// Reports typed attachment, reload, persistence or subscription-delivery failure.
///
/// Display includes a stack trace when Rust's backtrace settings enable capture.
/// It never formats an original cause. Explicit cause access can reveal sensitive data.
#[derive(Clone)]
pub struct DocumentKeyError {
    /// Stable failure category inspected through the public predicates.
    kind: KeyErrorKind,
    /// Shares captured diagnostics without allocating for a disabled local trace.
    context: KeyErrorContext,
}

/// Shares diagnostics without allocating a second owner around a typed cause.
#[derive(Clone)]
enum KeyErrorContext {
    /// A local lifecycle result has a capture site but no external cause.
    Trace(
        /// Captured trace shared by clones; disabled or unsupported capture needs no owner.
        Option<Arc<Backtrace>>,
    ),
    /// An external failure stores its original type beside the trace.
    Cause(
        /// Original error and trace share this single allocation.
        Arc<dyn Cause>,
    ),
}

impl DocumentKeyError {
    /// Records a local routing or lifecycle failure without an external cause.
    fn new(kind: KeyErrorKind) -> Self {
        let trace = Backtrace::capture();
        Self {
            kind,
            context: KeyErrorContext::Trace(
                (trace.status() == BacktraceStatus::Captured).then(|| Arc::new(trace)),
            ),
        }
    }

    /// Retains a typed boundary failure without formatting its potentially sensitive payload.
    fn caused(kind: KeyErrorKind, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            kind,
            context: KeyErrorContext::Cause(capture(source)),
        }
    }

    /// Borrows the trace captured once at the original failure boundary.
    fn backtrace(&self) -> Option<&Backtrace> {
        match &self.context {
            KeyErrorContext::Trace(trace) => trace.as_deref(),
            KeyErrorContext::Cause(cause) => Some(cause.backtrace()),
        }
    }

    /// Reports loss of the original captured route, without renewing its generation.
    #[must_use]
    pub const fn is_ended(&self) -> bool {
        matches!(self.kind, KeyErrorKind::Ended)
    }

    /// Reports immediate admission pressure rather than successful registration.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        matches!(self.kind, KeyErrorKind::Full)
    }

    /// Reports a fenced or incorrectly paired document session.
    #[must_use]
    pub const fn is_session(&self) -> bool {
        matches!(self.kind, KeyErrorKind::Closed | KeyErrorKind::WrongSession)
    }

    /// Reports a load superseded by a more recently started load.
    #[must_use]
    pub const fn is_superseded(&self) -> bool {
        matches!(self.kind, KeyErrorKind::Superseded)
    }

    /// Reports explicit delivery cancellation, not physical quiescence.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.kind, KeyErrorKind::Cancelled)
    }

    /// Reports Busy or Poisoned access without silently discarding the delivery failure.
    #[must_use]
    pub const fn is_shared_access(&self) -> bool {
        matches!(self.kind, KeyErrorKind::Access)
    }

    /// Reports external revision evidence conflicting with pending local edits.
    #[must_use]
    pub const fn is_conflict(&self) -> bool {
        matches!(self.kind, KeyErrorKind::Conflict)
    }

    /// Returns the original typed cause when the failure crossed an external boundary.
    #[must_use]
    pub fn error(&self) -> Option<&(dyn Error + Send + Sync + 'static)> {
        match &self.context {
            KeyErrorContext::Trace(_) => None,
            KeyErrorContext::Cause(cause) => Some(cause.error()),
        }
    }
}

impl fmt::Debug for DocumentKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentKeyError")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for DocumentKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "document key {:?} failed", self.kind)?;
        if let Some(backtrace) = self.backtrace()
            && backtrace.status() == BacktraceStatus::Captured
        {
            write!(f, "\n{backtrace}")?;
        }
        Ok(())
    }
}

impl Error for DocumentKeyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.error().map(|source| source as _)
    }
}

/// Records one registration's progress, preserving the first terminal result.
#[derive(Clone, Debug)]
enum SubscriptionPhase {
    /// The actor has not yet validated the queued registration.
    Pending,
    /// The actor accepted the registration and can deliver further values.
    Active {
        /// Distinguishes observed storage absence from a stored optional value.
        present: bool,
    },
    /// All producer updates and the normal terminal result reached the actor.
    Finished,
    /// Delivery was explicitly cancelled.
    Cancelled,
    /// Retains the first registration, access or producer failure.
    Failed(
        /// Original terminal failure shared across status snapshots.
        DocumentKeyError,
    ),
}

/// Exposes asynchronous registration and delivery results through the document owner.
#[derive(Clone, Debug)]
pub struct DocumentSubscriptionStatus(
    /// Snapshot of the registration phase, independent of subsequent changes.
    SubscriptionPhase,
);

impl DocumentSubscriptionStatus {
    /// Reports a registration admitted but not yet validated at actor commit.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        matches!(self.0, SubscriptionPhase::Pending)
    }

    /// Reports accepted registration with no terminal outcome.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self.0, SubscriptionPhase::Active { .. })
    }

    /// Reports explicit observed absence, distinct from stored optional None.
    #[must_use]
    pub const fn is_absent(&self) -> bool {
        matches!(self.0, SubscriptionPhase::Active { present: false })
    }

    /// Reports explicit cancellation rather than normal producer drainage.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        matches!(self.0, SubscriptionPhase::Cancelled)
    }

    /// Reports normal terminal drainage rather than failure or cancellation.
    #[must_use]
    pub const fn is_finished(&self) -> bool {
        matches!(self.0, SubscriptionPhase::Finished)
    }

    /// Returns the original registration, producer or delivery failure.
    #[must_use]
    pub const fn error(&self) -> Option<&DocumentKeyError> {
        if let SubscriptionPhase::Failed(error) = &self.0 {
            Some(error)
        } else {
            None
        }
    }
}

/// Couples publication authority, inspectable status and once-only provider cancellation.
struct Registration {
    /// Fast rejection flag, changed with the terminal status under the commit lock.
    active: AtomicBool,
    /// Serialises terminal transitions with shared-value publication and load acceptance.
    commit: Mutex<()>,
    /// First terminal result is permanent, including during reentrant teardown.
    phase: Mutex<SubscriptionPhase>,
    /// Cancellation handle taken before any user callback runs.
    provider: Mutex<Option<SharedSubscription>>,
}

impl Registration {
    /// Creates a pending registration that owns publication authority until revoked.
    fn new() -> Arc<Self> {
        Arc::new(Self {
            active: AtomicBool::new(true),
            commit: Mutex::new(()),
            phase: Mutex::new(SubscriptionPhase::Pending),
            provider: Mutex::new(None),
        })
    }

    /// Copies status without cloning an original user error or invoking its methods.
    fn status(&self) -> DocumentSubscriptionStatus {
        DocumentSubscriptionStatus(
            self.phase
                .lock()
                .expect("document subscription poisoned")
                .clone(),
        )
    }

    /// Updates a nonterminal phase and drops displaced data outside the status lock.
    fn update(&self, phase: SubscriptionPhase) {
        let previous = {
            let mut current = self.phase.lock().expect("document subscription poisoned");
            if matches!(
                *current,
                SubscriptionPhase::Finished
                    | SubscriptionPhase::Cancelled
                    | SubscriptionPhase::Failed(_)
            ) {
                return;
            }
            std::mem::replace(&mut *current, phase)
        };
        drop(previous);
    }

    /// Commits a failure before cancelling the external provider.
    fn fail(&self, failure: DocumentKeyError) {
        self.terminate(SubscriptionPhase::Failed(failure));
        self.cancel_provider();
    }

    /// Revokes publication before cancelling the external provider.
    fn cancel(&self) {
        self.terminate(SubscriptionPhase::Cancelled);
        self.cancel_provider();
    }

    /// Records normal delivery completion before releasing the external provider.
    fn finish(&self) {
        self.terminate(SubscriptionPhase::Finished);
        self.cancel_provider();
    }

    /// Claims provider cancellation once and invokes it outside every registration lock.
    fn cancel_provider(&self) {
        let provider = self
            .provider
            .lock()
            .expect("document provider poisoned")
            .take();
        if let Some(provider) = provider {
            provider.cancel();
        }
    }

    /// Revokes authority and preserves the first terminal cause before user destructors run.
    fn terminate(&self, phase: SubscriptionPhase) {
        let discarded = {
            let _commit = self.commit.lock().expect("document commit poisoned");
            let mut current = self.phase.lock().expect("document subscription poisoned");
            // Commit the cause with its delivery fence so teardown cannot replace
            // it with a synthetic Ended failure.
            self.active.store(false, Ordering::Release);
            if matches!(
                *current,
                SubscriptionPhase::Finished
                    | SubscriptionPhase::Cancelled
                    | SubscriptionPhase::Failed(_)
            ) {
                phase
            } else {
                std::mem::replace(&mut *current, phase)
            }
        };
        drop(discarded);
    }

    /// Exchanges a shared value only while both session and registration remain active.
    ///
    /// Preparation and displaced-value destruction occur outside the commit locks.
    fn publish<Value>(
        &self,
        session: &SessionInner,
        subscriber: &SharedSubscriber<Value>,
        mut value: Value,
    ) -> Result<bool, SharedAccessError> {
        let Some(publication) = subscriber.prepare_publication() else {
            return Ok(false);
        };
        let result = {
            let _session = session
                .commit
                .lock()
                .expect("document session commit poisoned");
            let _commit = self.commit.lock().expect("document commit poisoned");
            if self.active.load(Ordering::Acquire) && session.open.load(Ordering::Acquire) {
                publication.try_replace(&mut value)
            } else {
                Ok(false)
            }
        };
        drop(value);
        result
    }
}

/// Retains an unvalidated subscription and fails it if actor admission is lost.
struct PendingRegistration<Value> {
    /// Authority and status that survive the borrowed action.
    registration: Arc<Registration>,
    /// Weak shared-cache endpoint, which does not retain its value owner.
    subscriber: SharedSubscriber<Value>,
    /// Copies the value current at the registration's actor commit.
    capture: Arc<Capture<Value>>,
}

impl<Value> Drop for PendingRegistration<Value> {
    fn drop(&mut self) {
        if self.registration.status().is_pending() {
            self.registration
                .fail(DocumentKeyError::new(KeyErrorKind::Ended));
        }
    }
}

/// Links an accepted document subscription to its weak shared-cache endpoint.
struct ReferenceSubscription<Value> {
    /// Registration owner can disappear independently of the document state.
    registration: Weak<Registration>,
    /// Endpoint that can reject publication after its cache reference ends.
    subscriber: SharedSubscriber<Value>,
    /// Copies each accepted document value for this subscriber.
    capture: Arc<Capture<Value>>,
}

/// Owns cancellation and inspectable asynchronous outcomes for one document subscription.
#[derive(Clone)]
pub struct DocumentSubscription {
    /// Shared once-only cancellation, including final-handle destruction.
    cancellation: SharedSubscription,
    /// Inspectable status retained independently of provider lifetime.
    registration: Arc<Registration>,
}

impl DocumentSubscription {
    /// Cancels once across every alias without claiming producer join.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Reads pending, active, absent or typed terminal status.
    #[must_use]
    pub fn status(&self) -> DocumentSubscriptionStatus {
        self.registration.status()
    }
}

impl fmt::Debug for DocumentSubscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentSubscription")
            .finish_non_exhaustive()
    }
}

/// Retains an explicit captured document route outside feature State.
///
/// The capture closure must produce an independent snapshot; Clone alone does
/// not imply independence. Keys retain only weak route/session references. The
/// final owner of this captured route ends attachment even if the root lives.
///
/// # Examples
///
/// Retain the exact session before constructing the Store. Load through the
/// key, await the original save receipt through `SharedKey::save`, then close
/// the owner before constructing a replacement session for the same storage.
///
/// ```
/// use rca_core::{ActionPath, Runtime, SharedKey, SharedKeyIdentity, SharedKeyCache,
///     StatePath, Store};
/// use rca_persistence::{BackendSession, Codec, SharedDocument, SharedDocumentAction,
///     SharedDocumentOwner, SharedDocumentState};
///
/// async fn persist<B: BackendSession, C: Codec<u32>>(runtime: Runtime, backend: B, codec: C) {
///     let (state, reducer) = SharedDocument::new(0, backend, codec);
///     let session = reducer.session();
///     let store = Store::new(state, reducer, runtime);
///     let route = store.scope(
///         StatePath::new(|s: &SharedDocumentState<u32>| s, |s| s),
///         ActionPath::new(|a: &SharedDocumentAction<u32>| Some(a), |a| a),
///     );
///     let owner = SharedDocumentOwner::new(route, |value: &u32| *value, session);
///     // Include the backend/resource identity when the cache serves multiple stores.
///     let key = owner.key(SharedKeyIdentity::new::<C>("settings/session-1", "v1"));
///     let cache = SharedKeyCache::new();
///     let shared = cache.shared(key.clone(), || 0).await.unwrap();
///     key.save(&42).await.unwrap();
///     assert_eq!(shared.read(|value| *value).unwrap(), 42);
///     owner.close().await.unwrap();
///     store.shutdown().await.unwrap();
/// }
/// ```
pub struct SharedDocumentOwner<Value, Revision = NoBackendRevision> {
    /// Strong captured route used only by this owner, not by its keys.
    route: DocumentRoute<Value, Revision>,
    /// Snapshot policy shared with keys and subscriptions.
    capture: Arc<Capture<Value>>,
    /// Exact physical-work owner paired with the captured document state.
    session: SharedDocumentSession,
}

impl<Value, Revision> SharedDocumentOwner<Value, Revision> {
    /// Pairs an exact captured route, explicit independent capture and its reducer session.
    pub fn new(
        route: DocumentRoute<Value, Revision>,
        capture: impl Fn(&Value) -> Value + Send + Sync + 'static,
        session: SharedDocumentSession,
    ) -> Self {
        Self {
            route,
            capture: Arc::new(capture),
            session,
        }
    }

    /// Creates a weak typed key for this exact document owner and resource identity.
    #[must_use]
    pub fn key(&self, identity: SharedKeyIdentity) -> DocumentKey<Value, Revision> {
        DocumentKey {
            route: self.route.downgrade(),
            session: Arc::downgrade(&self.session.inner),
            capture: Arc::clone(&self.capture),
            identity,
            registrations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Reads live subscription outcomes even when the cache erases its retained guard.
    ///
    /// # Panics
    /// Panics if the registration metadata lock was poisoned.
    #[must_use]
    pub fn subscription_statuses(
        &self,
        key: &DocumentKey<Value, Revision>,
    ) -> Vec<DocumentSubscriptionStatus> {
        if !Weak::ptr_eq(&key.session, &Arc::downgrade(&self.session.inner)) {
            return Vec::new();
        }
        let registrations: Vec<_> = {
            let mut registrations = key
                .registrations
                .lock()
                .expect("document key subscriptions poisoned");
            registrations.retain(|registration| registration.strong_count() > 0);
            registrations.iter().filter_map(Weak::upgrade).collect()
        };
        registrations
            .iter()
            .map(|registration| registration.status())
            .collect()
    }
}

impl<Value: Clone + Send + 'static, Revision: Clone + Eq + Send + 'static>
    SharedDocumentOwner<Value, Revision>
{
    /// Attaches bounded change notifications to real backend reloads on this captured route.
    ///
    /// Both present and absent notifications request a fresh backend observation;
    /// they are hints, not replacement storage values. Initial notification,
    /// coalesced latest notification and terminal outcome remain distinct. The
    /// start callback supplies actual producer join, retained by this session.
    ///
    /// # Errors
    /// Rejects fenced admission or an ended/full original route. Later actor
    /// rejection and typed delivery/producer errors remain visible on the guard.
    ///
    /// # Panics
    /// Propagates a start/cancellation callback panic or poisoned control lock.
    /// If `start` panics before returning its join, it remains responsible for
    /// any work it has started. The session cannot join work it never received.
    pub fn observe_changes<ProducerError, Start, Cancel, Join>(
        &self,
        start: Start,
    ) -> Result<DocumentSubscription, DocumentKeyError>
    where
        ProducerError: Error + Send + Sync + 'static,
        Start: FnOnce(SharedProducer<(), ProducerError>) -> (SharedUpdate<()>, Cancel, Join),
        Cancel: FnOnce() + Send + 'static,
        Join: Future<Output = Result<(), ProducerError>> + Send + 'static,
    {
        self.observe_updates(start, |_| None)
    }

    /// Attaches independently captured backend snapshots, without replacing them
    /// with later reads. Use `Present(snapshot)` even for revisioned absence:
    /// `snapshot.bytes = None` retains the deletion's genuine revision.
    /// Plain documents require `revision = None`; revisioned documents require
    /// `Some(revision)`. An incompatible observation terminates with a typed error.
    /// Initial, bounded latest, terminal outcome and physical join stay separate.
    /// If a successful local save intervenes after registration starts, untagged
    /// queued snapshots cannot prove freshness and are revalidated by backend
    /// reads. This remains necessary for later snapshots on that registration;
    /// register again to establish a new non-ambiguous capture boundary.
    ///
    /// # Errors
    /// Returns immediate admission failures. Later failures are on the guard.
    ///
    /// # Panics
    /// Propagates callback panics or poisoned control locks.
    /// If `start` panics before returning its join, it remains responsible for
    /// any work it has started. The session cannot join work it never received.
    pub fn observe_snapshots<ProducerError, Start, Cancel, Join>(
        &self,
        start: Start,
    ) -> Result<DocumentSubscription, DocumentKeyError>
    where
        ProducerError: Error + Send + Sync + 'static,
        Start: FnOnce(
            SharedProducer<BackendSnapshot<Option<Revision>>, ProducerError>,
        ) -> (
            SharedUpdate<BackendSnapshot<Option<Revision>>>,
            Cancel,
            Join,
        ),
        Cancel: FnOnce() + Send + 'static,
        Join: Future<Output = Result<(), ProducerError>> + Send + 'static,
    {
        self.observe_updates(start, |update| {
            Some(match update {
                SharedUpdate::Present(snapshot) => snapshot,
                SharedUpdate::Absent => BackendSnapshot {
                    revision: None,
                    bytes: None,
                },
            })
        })
    }

    /// Reserves the physical join before transferring the producer's delivery effect.
    fn observe_updates<Item, ProducerError, Start, Cancel, Join, Map>(
        &self,
        start: Start,
        map: Map,
    ) -> Result<DocumentSubscription, DocumentKeyError>
    where
        Item: Send + 'static,
        ProducerError: Error + Send + Sync + 'static,
        Start: FnOnce(SharedProducer<Item, ProducerError>) -> (SharedUpdate<Item>, Cancel, Join),
        Cancel: FnOnce() + Send + 'static,
        Join: Future<Output = Result<(), ProducerError>> + Send + 'static,
        Map: Fn(SharedUpdate<Item>) -> Option<BackendSnapshot<Option<Revision>>> + Send + 'static,
    {
        let admission = self.session.admit()?;
        let captured_after = self.session.inner.saved_epoch.load(Ordering::Acquire);
        let (mut updates, join) = SharedUpdates::activate(start);
        let registration = Registration::new();
        *registration
            .provider
            .lock()
            .expect("document provider poisoned") = Some(updates.subscription());
        admission.attach(
            &registration,
            Box::pin(async move { join.await.map_err(SessionFailure::new) }),
        );
        let cancelling = Arc::clone(&registration);
        let cancellation = SharedSubscription::new(move || cancelling.cancel());
        let mut delivery = ProducerDelivery {
            registration: Arc::clone(&registration),
            completed: false,
        };
        let session = Arc::downgrade(&self.session.inner);
        let capture = Arc::clone(&self.capture);
        let effect = Effect::run(move |sender| async move {
            let outcome = loop {
                match updates.next().await {
                    Ok(Some(update)) => {
                        if !delivery.registration.active.load(Ordering::Acquire) {
                            return;
                        }
                        let (reply, response) = async_channel::bounded(1);
                        if sender
                            .send(SharedDocumentAction(SharedKind::ExternalLoad {
                                session: Weak::clone(&session),
                                registration: Arc::downgrade(&delivery.registration),
                                reply,
                                capture: Arc::clone(&capture),
                                snapshot: map(update).map(|snapshot| (captured_after, snapshot)),
                            }))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        match response.recv().await {
                            // A newer explicit reload owns the observation now. Its
                            // result cannot be overwritten by this old producer turn.
                            Ok(Ok(_)) => {}
                            Ok(Err(error)) if error.is_superseded() => {}
                            Ok(Err(error)) => break Err(error),
                            Err(_) => return,
                        }
                    }
                    Ok(None) => break Ok(()),
                    Err(error) => {
                        break Err(error.into_failure().map_or_else(
                            || DocumentKeyError::new(KeyErrorKind::Cancelled),
                            |error| DocumentKeyError::caused(KeyErrorKind::Producer, error),
                        ));
                    }
                }
            };
            let terminal = ProducerTerminal {
                registration: Arc::clone(&delivery.registration),
                outcome,
            };
            if sender
                .send(SharedDocumentAction(SharedKind::ProducerTerminal {
                    session,
                    terminal,
                }))
                .await
                .is_ok()
            {
                delivery.completed = true;
            }
            drop(delivery);
        });
        let action = SharedDocumentAction(SharedKind::Observe {
            session: Arc::downgrade(&self.session.inner),
            pending: PendingProducer {
                registration: Arc::clone(&registration),
                effect: Mutex::new(Some(Box::new(effect))),
            },
        });
        let _ = self.route.try_send(action).map_err(|error| {
            DocumentKeyError::new(if error.is_full() {
                KeyErrorKind::Full
            } else {
                KeyErrorKind::Ended
            })
        })?;
        Ok(DocumentSubscription {
            cancellation,
            registration,
        })
    }

    /// Fences this attachment, aborts actor delivery and joins its exact external session.
    ///
    /// This is not a flush or rollback; persist with a captured receipt first.
    /// If the captured route now contains a replacement document, that document
    /// is not aborted. This owner still joins its own original session.
    ///
    /// # Errors
    /// Returns original physical join failures after all joins finish. An ended
    /// captured scope requires no replacement route and never renews authority.
    ///
    /// # Panics
    /// Has the same callback, join and lock panic contract as
    /// [`SharedDocumentSession::close`].
    pub async fn close(&self) -> Result<(), SessionCloseError> {
        self.session.inner.fence();
        let _ = self
            .route
            .send(SharedDocumentAction(SharedKind::OwnerClose {
                session: Arc::downgrade(&self.session.inner),
            }))
            .await;
        self.session.close().await
    }
}
impl<Value, Revision> fmt::Debug for SharedDocumentOwner<Value, Revision> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedDocumentOwner")
            .finish_non_exhaustive()
    }
}

/// Routes key operations through a weak captured document owner, never a logical lookup.
///
/// Subscription setup admits an actor action; it does not acknowledge delivery.
/// In that action's turn, the actor recaptures its current present value and
/// installs the registration without an intervening actor action. This closes
/// the cache's load/register gap for the actor's current present value, not for
/// every intermediate backend event. Backend observation has its own continuity
/// contract. Absence preserves the cached value and is exposed in subscription
/// status; it does not restore the seed or deliver a deletion value.
///
/// Inspect [`SharedDocumentOwner::subscription_statuses`] for post-admission
/// failures such as busy value access. Admission failure returns directly from
/// subscription setup. Neither outcome proves physical backend join; the
/// explicit session retains that responsibility.
pub struct DocumentKey<Value, Revision = NoBackendRevision> {
    /// Captured route that cannot keep the store alive or acquire a replacement generation.
    route: WeakDocumentRoute<Value, Revision>,
    /// Exact session that owns this key's resource binding.
    session: Weak<SessionInner>,
    /// Copies values before save admission and after load acceptance.
    capture: Arc<Capture<Value>>,
    /// Caller-supplied cache identity, separate from session and scope identities.
    identity: SharedKeyIdentity,
    /// Weak registrations used to inspect guards retained inside a shared cache.
    registrations: Arc<Mutex<Vec<Weak<Registration>>>>,
}

impl<Value, Revision> Clone for DocumentKey<Value, Revision> {
    fn clone(&self) -> Self {
        Self {
            route: self.route.clone(),
            session: Weak::clone(&self.session),
            capture: Arc::clone(&self.capture),
            identity: self.identity.clone(),
            registrations: Arc::clone(&self.registrations),
        }
    }
}
impl<Value, Revision> fmt::Debug for DocumentKey<Value, Revision> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocumentKey").finish_non_exhaustive()
    }
}

/// Revokes a key load when its future ends without receiving a result.
struct LoadCancellation<Value, Revision> {
    /// Weak route for the best-effort cancellation action.
    route: WeakDocumentRoute<Value, Revision>,
    /// Prevents cancellation from affecting a replacement document session.
    session: Weak<SessionInner>,
    /// Immediate commit authority, revoked even if the actor queue is full.
    token: Arc<LoadToken>,
    /// Suppresses cancellation after the original result reaches its caller.
    completed: bool,
}

impl<Value, Revision> Drop for LoadCancellation<Value, Revision> {
    fn drop(&mut self) {
        if !self.completed {
            self.token.cancel();
            if let Some(route) = self.route.upgrade() {
                let _ = route.try_send(SharedDocumentAction(SharedKind::KeyCancel {
                    session: Weak::clone(&self.session),
                    token: Arc::clone(&self.token),
                }));
            }
        }
    }
}

impl<Value, Revision> DocumentKey<Value, Revision> {
    /// Resolves the original route only while its original session remains open.
    fn route(&self) -> Result<DocumentRoute<Value, Revision>, DocumentKeyError> {
        let route = self
            .route
            .upgrade()
            .ok_or_else(|| DocumentKeyError::new(KeyErrorKind::Ended))?;
        let session = self
            .session
            .upgrade()
            .ok_or_else(|| DocumentKeyError::new(KeyErrorKind::Closed))?;
        if !session.open.load(Ordering::Acquire) {
            return Err(DocumentKeyError::new(KeyErrorKind::Closed));
        }
        Ok(route)
    }
}

impl<Value: Clone + Send + 'static, Revision: Clone + Eq + Send + 'static> SharedReaderKey
    for DocumentKey<Value, Revision>
{
    type Value = Value;
    type Error = DocumentKeyError;
    type Subscription = DocumentSubscription;

    fn identity(&self) -> SharedKeyIdentity {
        self.identity.clone()
    }

    fn load(
        &self,
        _: SharedLoadContext<'_, Value>,
    ) -> impl Future<Output = Result<Option<Value>, DocumentKeyError>> + Send {
        let route = self.route();
        let session = Weak::clone(&self.session);
        let capture = Arc::clone(&self.capture);
        let token = Arc::new(LoadToken::new());
        let mut cancellation = LoadCancellation {
            route: self.route.clone(),
            session: Weak::clone(&session),
            token: Arc::clone(&token),
            completed: false,
        };
        async move {
            let route = route?;
            let (reply, response) = async_channel::bounded(1);
            let _ = route
                .send(SharedDocumentAction(SharedKind::KeyLoad {
                    session,
                    token,
                    reply,
                    capture,
                }))
                .await
                .map_err(|_ended| DocumentKeyError::new(KeyErrorKind::Ended))?;
            drop(route);
            let result = response
                .recv()
                .await
                .map_err(|_closed| DocumentKeyError::new(KeyErrorKind::Ended))?;
            cancellation.completed = true;
            drop(cancellation);
            result
        }
    }

    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        subscriber: SharedSubscriber<Value>,
    ) -> Result<DocumentSubscription, DocumentKeyError> {
        let route = self.route()?;
        let registration = Registration::new();
        let cancelling = Arc::clone(&registration);
        let cancellation = SharedSubscription::new(move || cancelling.cancel());
        let pending = PendingRegistration {
            registration: Arc::clone(&registration),
            subscriber,
            capture: Arc::clone(&self.capture),
        };
        let action = SharedDocumentAction(SharedKind::Subscribe {
            session: Weak::clone(&self.session),
            pending,
        });
        let _ = route.try_send(action).map_err(|error| {
            DocumentKeyError::new(if error.is_full() {
                KeyErrorKind::Full
            } else {
                KeyErrorKind::Ended
            })
        })?;
        let mut registrations = self
            .registrations
            .lock()
            .expect("document key subscriptions poisoned");
        registrations.retain(|registration| registration.strong_count() > 0);
        registrations.push(Arc::downgrade(&registration));
        drop(registrations);
        Ok(DocumentSubscription {
            cancellation,
            registration,
        })
    }
}

impl<Value: Clone + Send + 'static, Revision: Clone + Eq + Send + 'static> SharedKey
    for DocumentKey<Value, Revision>
{
    fn save(&self, value: &Value) -> impl Future<Output = Result<(), DocumentKeyError>> + Send {
        let value = (self.capture)(value);
        let route = self.route();
        let session = Weak::clone(&self.session);
        async move {
            let route = route?;
            let (reply, receipt) = channel();
            let _ = route
                .send(SharedDocumentAction(SharedKind::KeySave {
                    session,
                    value,
                    reply,
                }))
                .await
                .map_err(|_ended| DocumentKeyError::new(KeyErrorKind::Ended))?;
            drop(route);
            receipt
                .await
                .map(|_| ())
                .map_err(|error| DocumentKeyError::caused(KeyErrorKind::Save, error))
        }
    }
}

impl<Value, Revision> SharedDocumentAction<Value, Revision> {
    /// Starts a reload whose completion supersedes every older reload.
    #[must_use]
    pub const fn reload() -> Self {
        Self(SharedKind::Reload)
    }

    /// Retires the latest reload without acknowledging persistence or rolling back I/O.
    #[must_use]
    pub const fn cancel_load() -> Self {
        Self(SharedKind::CancelLoad)
    }

    /// Commits an edit and its persistence barrier in the same actor action.
    #[must_use = "submit the action and await its captured-revision receipt"]
    pub fn set_and_flush(value: Value) -> (Self, Receipt) {
        let (reply, receipt) = channel();
        (Self(SharedKind::SetAndFlush { value, reply }), receipt)
    }

    /// Captures a persistence barrier for the currently committed local revision.
    #[must_use = "submit the action and await its persistence receipt"]
    pub fn flush() -> (Self, Receipt) {
        Self::barrier(false)
    }

    /// Fences edits and acknowledges the final local revision, not physical quiescence.
    #[must_use = "submit the action and await its persistence receipt"]
    pub fn close() -> (Self, Receipt) {
        Self::barrier(true)
    }

    /// Permanently aborts delivery; the external session still owes physical joins.
    ///
    /// Pending/new barriers resolve with `Aborted` after publication. While the
    /// session remains open, terminal acknowledgements exclude inherited named
    /// cancellation identities, including the save being aborted. Root task
    /// cancellation, scope removal, shutdown and explicit wrappers still own
    /// their delivery.
    #[must_use]
    pub const fn abort() -> Self {
        Self(SharedKind::Abort)
    }

    /// Pairs a logical flush or close command with its captured-revision acknowledgement.
    fn barrier(closing: bool) -> (Self, Receipt) {
        let (reply, receipt) = channel();
        (Self(SharedKind::Flush { closing, reply }), receipt)
    }
}

impl<Value, Revision> fmt::Debug for SharedDocumentAction<Value, Revision> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedDocumentAction")
            .finish_non_exhaustive()
    }
}

/// Retains one original physical-join failure without formatting its payload.
///
/// Display includes the join failure's capture site when backtrace capture is enabled.
#[derive(Clone)]
pub struct SessionFailure {
    /// Original join failure and capture site shared across close waiters.
    source: Arc<dyn Cause>,
}

impl SessionFailure {
    /// Retains one physical-join failure without evaluating its formatting methods.
    fn new(error: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: capture(error),
        }
    }

    /// Returns the original typed join failure for explicit inspection.
    #[must_use]
    pub fn error(&self) -> &(dyn Error + Send + Sync + 'static) {
        self.source.error()
    }
}

impl fmt::Debug for SessionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionFailure").finish_non_exhaustive()
    }
}

impl fmt::Display for SessionFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("physical session join failed")?;
        let backtrace = self.source.backtrace();
        if backtrace.status() == BacktraceStatus::Captured {
            write!(f, "\n{backtrace}")?;
        }
        Ok(())
    }
}

impl Error for SessionFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.error())
    }
}

/// Retains all failed joins after every admitted join has finished.
#[derive(Clone, Debug)]
pub struct SessionCloseError {
    /// All failed joins in admission order, after every join has completed.
    failures: Vec<SessionFailure>,
}

impl SessionCloseError {
    /// Returns every original failure; no failed join is treated as success.
    #[must_use]
    pub fn failures(&self) -> &[SessionFailure] {
        &self.failures
    }
}

impl fmt::Display for SessionCloseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("document session did not close successfully")?;
        for failure in &self.failures {
            write!(f, "\n{failure}")?;
        }
        Ok(())
    }
}

impl Error for SessionCloseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.failures.first().map(|failure| failure as _)
    }
}

/// Retains one actual physical join until it produces an outcome.
enum Debt {
    /// Pending work, replaced and dropped outside the session lock when ready.
    Pending(
        /// The actual backend or producer join, never a cancellation acknowledgement.
        OwnedFuture<Result<(), SessionFailure>>,
    ),
    /// Completed result retained while any other join remains pending.
    Completed(
        /// Original join outcome, shared with the cached result after every join completes.
        Result<(), SessionFailure>,
    ),
}

/// Serialises admission, shared polling and the cached terminal close result.
struct SessionControl {
    /// Actual joins, temporarily moved to the one active close poller.
    debts: Vec<Debt>,
    /// Providers cancelled before their physical joins are polled.
    cancellations: Vec<Weak<Registration>>,
    /// Start callbacks whose join futures are not yet attached.
    admitting: usize,
    /// Prevents concurrent close waiters from polling the same join future.
    polling: bool,
    /// Preserves readiness that another waiter can consume before this poll finishes.
    woke_while_polling: bool,
    /// Distinct close-task notifications, invoked only after releasing this lock.
    waiters: Vec<Waker>,
    /// Permanent result published only after admissions and every physical join finish.
    outcome: Option<Result<(), SessionCloseError>>,
}

/// Keeps resource ownership and admission fences outside reducer state.
struct SessionInner {
    /// Serialises the closing fence with shared publication and accepted reloads.
    commit: Mutex<()>,
    /// Strong resource binding used by weak document states and effect completions.
    _binding: Option<Arc<dyn Any + Send + Sync>>,
    /// Once false, no key, producer or reload can acquire new commit authority.
    open: AtomicBool,
    /// Counts successful saves so older producer snapshots trigger fresh reads.
    saved_epoch: AtomicU64,
    /// Owns pending joins, admission reservations and close notifications.
    control: Mutex<SessionControl>,
}

impl SessionInner {
    /// Closes commit admission without running callbacks or joining physical work.
    fn fence(&self) {
        let _commit = self
            .commit
            .lock()
            .expect("document session commit poisoned");
        self.open.store(false, Ordering::Release);
    }
}

/// Forwards backend readiness to all close waiters without retaining the session.
struct SessionWake(
    /// Weak owner makes a backend-retained waker inert after session destruction.
    Weak<SessionInner>,
);
impl Wake for SessionWake {
    fn wake(self: Arc<Self>) {
        if let Some(session) = self.0.upgrade() {
            let waiters = {
                let mut control = session.control.lock().expect("document session poisoned");
                control.woke_while_polling |= control.polling;
                std::mem::take(&mut control.waiters)
            };
            for waiter in waiters {
                waiter.wake();
            }
        }
    }
}

/// Reserves a physical join while a producer's start callback runs outside locks.
struct Admission {
    /// Session that cannot finish closing until this reservation is released.
    session: Arc<SessionInner>,
}

impl Admission {
    /// Transfers the actual join, then cancels its provider if close raced the callback.
    fn attach(
        self,
        registration: &Arc<Registration>,
        future: OwnedFuture<Result<(), SessionFailure>>,
    ) {
        let closed = {
            let mut control = self
                .session
                .control
                .lock()
                .expect("document session poisoned");
            control.debts.push(Debt::Pending(future));
            control.cancellations.push(Arc::downgrade(registration));
            !self.session.open.load(Ordering::Acquire)
        };
        if closed {
            registration.cancel();
        }
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        let waiters = {
            let mut control = self
                .session
                .control
                .lock()
                .expect("document session poisoned");
            control.admitting -= 1;
            control.woke_while_polling |= control.polling;
            std::mem::take(&mut control.waiters)
        };
        for waiter in waiters {
            waiter.wake();
        }
    }
}

/// Owns exact backend and producer join obligations outside feature State.
///
/// Retain this handle before moving the reducer into a Store. It never owns a
/// Store, scoped route or shared value. Closing permanently fences attachment
/// admission, requests cancellations, and polls every actual join outside locks.
/// Dropping a close waiter leaves pending join futures in this owner. Dropping
/// the owner itself never manufactures successful completion.
#[derive(Clone)]
pub struct SharedDocumentSession {
    /// Shared admission state and physical work, never a store or document value.
    inner: Arc<SessionInner>,
}

impl SharedDocumentSession {
    /// Reserves one producer start while admission remains open.
    fn admit(&self) -> Result<Admission, DocumentKeyError> {
        let mut control = self
            .inner
            .control
            .lock()
            .expect("document session poisoned");
        if !self.inner.open.load(Ordering::Acquire) {
            return Err(DocumentKeyError::new(KeyErrorKind::Closed));
        }
        control.admitting = control
            .admitting
            .checked_add(1)
            .expect("document admissions exhausted");
        Ok(Admission {
            session: Arc::clone(&self.inner),
        })
    }

    /// Retains the backend's actual close future before any producer can be admitted.
    fn new<Storage: BackendSession>(
        backend: Arc<Storage>,
        binding: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Self {
        let close = Box::pin(async move { backend.close().await.map_err(SessionFailure::new) });
        Self {
            inner: Arc::new(SessionInner {
                commit: Mutex::new(()),
                _binding: binding,
                open: AtomicBool::new(true),
                saved_epoch: AtomicU64::new(0),
                control: Mutex::new(SessionControl {
                    debts: vec![Debt::Pending(close)],
                    cancellations: Vec::new(),
                    admitting: 0,
                    polling: false,
                    woke_while_polling: false,
                    waiters: Vec::new(),
                    outcome: None,
                }),
            }),
        }
    }

    /// Permanently closes admission and waits for all actual admitted producer/backend work.
    ///
    /// Stop the attached document owner first. This is quiescence, not a flush:
    /// a cancelled save may still have changed storage and requires fresh reload.
    /// Multiple waiters share obligations; dropping any waiter loses no debt.
    ///
    /// # Errors
    /// Returns all original join failures after every join completes. A failed
    /// backend close does not manufacture successful quiescence.
    ///
    /// # Panics
    /// Panics if a session lock is poisoned or a cancellation callback, join poll,
    /// destructor or waiter notification panics. This session cannot be resumed
    /// after such a panic. A panic never acknowledges successful completion.
    pub async fn close(&self) -> Result<(), SessionCloseError> {
        self.inner.fence();
        let wake = Waker::from(Arc::new(SessionWake(Arc::downgrade(&self.inner))));
        poll_fn(|context| {
            let waiter = context.waker().clone();
            let (mut debts, cancellations) = {
                let mut control = self
                    .inner
                    .control
                    .lock()
                    .expect("document session poisoned");
                if let Some(outcome) = &control.outcome {
                    return Poll::Ready(outcome.clone());
                }
                if !control.waiters.iter().any(|old| old.will_wake(&waiter)) {
                    control.waiters.push(waiter);
                }
                if control.polling {
                    return Poll::Pending;
                }
                control.polling = true;
                control.woke_while_polling = false;
                (
                    std::mem::take(&mut control.debts),
                    std::mem::take(&mut control.cancellations),
                )
            };
            for cancellation in cancellations {
                if let Some(cancellation) = cancellation.upgrade() {
                    cancellation.cancel();
                }
            }
            let mut context = Context::from_waker(&wake);
            for debt in &mut debts {
                if let Debt::Pending(future) = debt
                    && let Poll::Ready(outcome) = future.as_mut().poll(&mut context)
                {
                    // Future destructors also run outside the session guard.
                    drop(std::mem::replace(debt, Debt::Completed(outcome)));
                }
            }
            let finished = debts.iter().all(|debt| matches!(debt, Debt::Completed(_)));
            let mut outcome = finished.then(|| {
                let failures: Vec<_> = debts
                    .iter()
                    .filter_map(|debt| match debt {
                        Debt::Completed(Err(failure)) => Some(failure.clone()),
                        _ => None,
                    })
                    .collect();
                if failures.is_empty() {
                    Ok(())
                } else {
                    Err(SessionCloseError { failures })
                }
            });
            let waiters = {
                let mut control = self
                    .inner
                    .control
                    .lock()
                    .expect("document session poisoned");
                // A start callback can finish while these futures are polled.
                // Preserve its newly admitted debt and never publish early join.
                if control.admitting > 0 || !control.debts.is_empty() {
                    outcome = None;
                }
                if outcome.is_none() {
                    debts.append(&mut control.debts);
                    std::mem::swap(&mut control.debts, &mut debts);
                }
                control.polling = false;
                control.outcome.clone_from(&outcome);
                let notified = std::mem::take(&mut control.woke_while_polling);
                if outcome.is_some() || notified {
                    std::mem::take(&mut control.waiters)
                } else {
                    Vec::new()
                }
            };
            // The cached outcome now owns all failures. Retired join storage
            // and any empty admission buffer are released outside the lock.
            drop(debts);
            for waiter in waiters {
                waiter.wake();
            }
            outcome.map_or(Poll::Pending, Poll::Ready)
        })
        .await
    }
}

impl fmt::Debug for SharedDocumentSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedDocumentSession")
            .finish_non_exhaustive()
    }
}

/// Interprets one authoritative document state machine through explicit backend capabilities.
pub struct SharedDocument<Value, Revision = NoBackendRevision> {
    /// Shares the ordinary document's revision and barrier rules.
    transitions: Transitions,
    /// Resource binding used by states created from this reducer.
    binding: Arc<Binding<Value, Revision>>,
    /// External lifetime owner that must survive the reducer when joins remain pending.
    session: SharedDocumentSession,
}

/// Erases backend and codec types while preserving a state's exact resource pairing.
struct Binding<Value, Revision> {
    /// Starts a backend read and retains its raw bytes alongside the decoded result.
    load: Box<Load<Value, Revision>>,
    /// Saves an owned snapshot using the corresponding backend comparison identity.
    save: Box<Save<Value, Revision>>,
    /// Decodes an external snapshot without another backend read.
    decode: Decode<Value, Revision>,
}

impl<Value: Clone + Send + 'static> SharedDocument<Value> {
    /// Creates an honest unversioned document using explicit backend/session capabilities.
    ///
    /// This mode has no CAS, external revision identity or echo guarantee.
    #[must_use]
    pub fn new<Storage: BackendSession, Encoding: Codec<Value>>(
        seed: Value,
        backend: Storage,
        codec: Encoding,
    ) -> (SharedDocumentState<Value>, Self) {
        let backend = Arc::new(backend);
        let codec = Arc::new(codec);
        let snapshot_codec = Arc::clone(&codec);
        let decode = Decode::Plain(Box::new(move |bytes: Option<Vec<u8>>| Loaded {
            revision: NoBackendRevision(()),
            decoded: bytes
                .as_deref()
                .map(|bytes| snapshot_codec.decode(bytes))
                .transpose()
                .map_err(|error| Failure::new(Operation::Decode, None, error)),
            bytes,
        }));
        let session_backend = Arc::clone(&backend);
        let loading = Arc::clone(&backend);
        let decoding = Arc::clone(&codec);
        let load = Box::new(move || {
            let backend = Arc::clone(&loading);
            let codec = Arc::clone(&decoding);
            Box::pin(async move {
                let bytes = backend
                    .load()
                    .await
                    .map_err(|error| Failure::new(Operation::Load, None, error))?;
                let decoded = bytes
                    .as_deref()
                    .map(|bytes| codec.decode(bytes))
                    .transpose()
                    .map_err(|error| Failure::new(Operation::Decode, None, error));
                Ok(Loaded {
                    revision: NoBackendRevision(()),
                    bytes,
                    decoded,
                })
            }) as OwnedFuture<_>
        });
        let save = Box::new(move |value, _: NoBackendRevision, revision| {
            let backend = Arc::clone(&backend);
            let codec = Arc::clone(&codec);
            Box::pin(async move {
                let encoded = codec.encode(&value);
                drop(value);
                let bytes = encoded
                    .map_err(|error| Failure::new(Operation::Encode, Some(revision), error))?;
                backend
                    .save(bytes)
                    .await
                    .map_err(|error| Failure::new(Operation::Save, Some(revision), error))?;
                Ok(Saved::Committed(NoBackendRevision(())))
            }) as OwnedFuture<_>
        });
        Self::assembled(seed, load, save, decode, session_backend)
    }

    /// Creates a document using genuine atomic compare-and-save plus separate session completion.
    #[must_use]
    pub fn revisioned<Storage, Encoding>(
        seed: Value,
        backend: Storage,
        codec: Encoding,
    ) -> (
        SharedDocumentState<Value, Storage::Revision>,
        SharedDocument<Value, Storage::Revision>,
    )
    where
        Storage: RevisionBackend + BackendSession,
        Encoding: Codec<Value>,
    {
        let backend = Arc::new(backend);
        let codec = Arc::new(codec);
        let snapshot_codec = Arc::clone(&codec);
        let decode = Decode::Revisioned(Box::new(
            move |snapshot: BackendSnapshot<Storage::Revision>| Loaded {
                revision: snapshot.revision,
                decoded: snapshot
                    .bytes
                    .as_deref()
                    .map(|bytes| snapshot_codec.decode(bytes))
                    .transpose()
                    .map_err(|error| Failure::new(Operation::Decode, None, error)),
                bytes: snapshot.bytes,
            },
        ));
        let session_backend = Arc::clone(&backend);
        let loading = Arc::clone(&backend);
        let decoding = Arc::clone(&codec);
        let load = Box::new(move || {
            let backend = Arc::clone(&loading);
            let codec = Arc::clone(&decoding);
            Box::pin(async move {
                let BackendSnapshot { revision, bytes } = backend
                    .load_revisioned()
                    .await
                    .map_err(|error| Failure::new(Operation::Load, None, error))?;
                let decoded = bytes
                    .as_deref()
                    .map(|bytes| codec.decode(bytes))
                    .transpose()
                    .map_err(|error| Failure::new(Operation::Decode, None, error));
                Ok(Loaded {
                    revision,
                    bytes,
                    decoded,
                })
            }) as OwnedFuture<_>
        });
        let save = Box::new(move |value, expected: Storage::Revision, revision| {
            let backend = Arc::clone(&backend);
            let codec = Arc::clone(&codec);
            Box::pin(async move {
                let encoded = codec.encode(&value);
                drop(value);
                let bytes = encoded
                    .map_err(|error| Failure::new(Operation::Encode, Some(revision), error))?;
                match backend
                    .compare_and_save(expected, bytes)
                    .await
                    .map_err(|error| Failure::new(Operation::Save, Some(revision), error))?
                {
                    CompareSave::Saved(revision) => Ok(Saved::Committed(revision)),
                    CompareSave::Conflict(BackendSnapshot {
                        revision: external,
                        bytes,
                    }) => {
                        let decoded = bytes
                            .as_deref()
                            .map(|bytes| codec.decode(bytes))
                            .transpose()
                            .map_err(|error| {
                                Failure::new(Operation::Decode, Some(revision), error)
                            });
                        Ok(Saved::Conflict(SharedConflict {
                            revision: external,
                            bytes,
                            decoded,
                        }))
                    }
                }
            }) as OwnedFuture<_>
        });
        SharedDocument::assembled(seed, load, save, decode, session_backend)
    }
}

impl<Value: Send + 'static, Revision: Send + 'static> SharedDocument<Value, Revision> {
    /// Pairs seed state, erased backend operations and their external lifetime owner.
    fn assembled<Storage: BackendSession>(
        seed: Value,
        load: Box<Load<Value, Revision>>,
        save: Box<Save<Value, Revision>>,
        decode: Decode<Value, Revision>,
        backend: Arc<Storage>,
    ) -> (SharedDocumentState<Value, Revision>, Self) {
        let binding = Arc::new(Binding { load, save, decode });
        let session = SharedDocumentSession::new(
            backend,
            Some(Arc::clone(&binding) as Arc<dyn Any + Send + Sync>),
        );
        let reducer = Self {
            transitions: Transitions::new(),
            binding,
            session,
        };
        (reducer.state(seed), reducer)
    }

    /// Retains this reducer's exact backend/session ownership before Store construction.
    #[must_use]
    pub fn session(&self) -> SharedDocumentSession {
        self.session.clone()
    }

    /// Creates another explicitly bound state for this exact resource/session.
    ///
    /// Distinct child generations may instead use a new constructor's state and
    /// retain that constructor's session outside State. The child interpreter
    /// always uses the state's supplied binding, never a lookup by logical ID.
    #[must_use]
    pub fn state(&self, seed: Value) -> SharedDocumentState<Value, Revision> {
        SharedDocumentState {
            binding: Arc::downgrade(&self.binding),
            session: Arc::downgrade(&self.session.inner),
            document: Transitions::state(seed),
            loading: None,
            last_save: None,
            load_started_after: None,
            deferred: None,
            load_error: None,
            backend_revision: None,
            revisioned: matches!(&self.binding.decode, Decode::Revisioned(_)),
            conflict: None,
            present: false,
            reply: None,
            subscribers: Vec::new(),
            producers: Vec::new(),
        }
    }
}

impl<Value, Revision> fmt::Debug for SharedDocument<Value, Revision> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedDocument").finish_non_exhaustive()
    }
}

impl<Value: Clone + Send + 'static, Revision: Clone + Eq + Send + 'static>
    SharedDocument<Value, Revision>
{
    /// Requires the original live session, not merely a route with the same value type.
    fn validate_session(
        state: &SharedDocumentState<Value, Revision>,
        session: &Weak<SessionInner>,
    ) -> Result<Arc<SessionInner>, DocumentKeyError> {
        let session = session
            .upgrade()
            .ok_or_else(|| DocumentKeyError::new(KeyErrorKind::Closed))?;
        if !Weak::ptr_eq(&state.session, &Arc::downgrade(&session)) {
            return Err(DocumentKeyError::new(KeyErrorKind::WrongSession));
        }
        if !session.open.load(Ordering::Acquire) {
            return Err(DocumentKeyError::new(KeyErrorKind::Closed));
        }
        Ok(session)
    }

    /// Delivers one key response as an effect after the actor's state commit.
    fn respond(
        reply: LoadReply<Value>,
        result: Result<Option<Value>, DocumentKeyError>,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        Effect::run(move |_| async move {
            match reply.try_send(result) {
                Ok(()) | Err(async_channel::TrySendError::Closed(_)) => {}
                Err(async_channel::TrySendError::Full(_)) => panic!("document load resolved twice"),
            }
        })
    }

    /// Publishes a present value and removes ended or rejected cache registrations.
    fn publish(
        value: &Value,
        present: bool,
        session: &Weak<SessionInner>,
        subscribers: &mut Vec<ReferenceSubscription<Value>>,
    ) {
        subscribers.retain(|subscription| {
            let Some(registration) = subscription.registration.upgrade() else {
                return false;
            };
            if !registration.active.load(Ordering::Acquire) {
                return false;
            }
            let result = if present {
                let value = (subscription.capture)(value);
                if !registration.active.load(Ordering::Acquire) {
                    return false;
                }
                registration.publish(
                    &session.upgrade().expect("live session"),
                    &subscription.subscriber,
                    value,
                )
            } else {
                Ok(true)
            };
            match result {
                Ok(true) => registration.update(SubscriptionPhase::Active { present }),
                Ok(false) => registration.fail(DocumentKeyError::new(KeyErrorKind::Ended)),
                Err(error) => {
                    registration.fail(DocumentKeyError::caused(KeyErrorKind::Access, error));
                }
            }
            registration.active.load(Ordering::Acquire)
        });
    }

    /// Commits a local edit, updates cache references and captures its save barrier.
    fn set_and_flush(
        &mut self,
        state: &mut SharedDocumentState<Value, Revision>,
        value: &Value,
        reply: &Reply,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        let Some(expected) = state.backend_revision.as_ref() else {
            return Self::resolve(
                state,
                Transitions::unavailable_barrier(&state.document, reply),
            );
        };
        let before = state.document.revision();
        let mut requests = self.transitions.set(&mut state.document, value);
        requests.extend(self.transitions.barrier(&mut state.document, false, reply));
        if state.document.revision() != before {
            state.present = true;
            Self::publish(
                state.document.value(),
                state.present,
                &state.session,
                &mut state.subscribers,
            );
        }
        self.interpret(state, requests, expected)
    }

    /// Resolves barriers without attaching terminal acknowledgements to aborted save lifetimes.
    fn resolve(
        state: &SharedDocumentState<Value, Revision>,
        resolutions: Vec<Resolution>,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        let acknowledgement = deliver(resolutions);
        if state.document.is_aborted() {
            acknowledgement.without_inherited_cancellation()
        } else {
            acknowledgement
        }
    }

    /// Captures a loaded state's barrier before interpreting any resulting save.
    fn flush(
        &mut self,
        state: &mut SharedDocumentState<Value, Revision>,
        closing: bool,
        reply: &Reply,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        let Some(expected) = state.backend_revision.as_ref() else {
            return Self::resolve(
                state,
                Transitions::unavailable_barrier(&state.document, reply),
            );
        };
        let requests = self
            .transitions
            .barrier(&mut state.document, closing, reply);
        self.interpret(state, requests, expected)
    }

    /// Converts shared transition requests into resource-bound effects and receipt delivery.
    fn interpret(
        &self,
        state: &SharedDocumentState<Value, Revision>,
        requests: Vec<Request<Value>>,
        expected: &Revision,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        Effect::merge(requests.into_iter().map(|request| match request {
            Request::Save {
                attempt,
                revision,
                value,
            } => {
                let binding = state
                    .binding
                    .upgrade()
                    .expect("live session retains resource binding");
                let saving = (binding.save)(value, expected.clone(), revision);
                let session = state
                    .session
                    .upgrade()
                    .expect("admitted reduction retains session");
                Effect::future(async move {
                    Some(SharedDocumentAction(SharedKind::Saved {
                        _session: session,
                        attempt,
                        revision,
                        result: saving.await,
                    }))
                })
                .cancellable(self.transitions.save_id.clone())
            }
            Request::Resolve(resolutions) => Self::resolve(state, resolutions),
        }))
    }

    /// Starts the latest reload and resolves any superseded caller before its new completion.
    fn reload(
        &mut self,
        state: &mut SharedDocumentState<Value, Revision>,
        session: &Arc<SessionInner>,
        reply: Option<PendingLoad<Value>>,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        if state.document.closing || state.document.aborted || !session.open.load(Ordering::Acquire)
        {
            return reply.map_or_else(Effect::none, |reply| {
                Self::respond(
                    reply.reply,
                    Err(DocumentKeyError::new(KeyErrorKind::Closed)),
                )
            });
        }
        let previous = std::mem::replace(&mut state.reply, reply);
        let attempt = self.transitions.attempt();
        state.loading = Some(attempt);
        state.load_started_after = state.last_save;
        state.deferred = None;
        state.load_error = None;
        let binding = state
            .binding
            .upgrade()
            .expect("live session retains resource binding");
        let loading = (binding.load)();
        let session = Arc::clone(session);
        let loading = Effect::future(async move {
            Some(SharedDocumentAction(SharedKind::Loaded {
                _session: session,
                attempt,
                result: loading.await,
            }))
        })
        .cancellable_in_flight(self.transitions.load_id.clone());
        let previous = previous.map_or_else(Effect::none, |previous| {
            previous.token.cancel();
            Self::respond(
                previous.reply,
                Err(DocumentKeyError::new(KeyErrorKind::Superseded)),
            )
        });
        Effect::merge([previous, loading])
    }
}

impl<Value: Clone + Send + 'static, Revision: Clone + Eq + Send + 'static>
    SharedDocument<Value, Revision>
{
    /// Admits a live key load without allowing its cancelled future to commit later.
    fn key_load(
        &mut self,
        state: &mut SharedDocumentState<Value, Revision>,
        session: &Weak<SessionInner>,
        token: &Arc<LoadToken>,
        reply: &LoadReply<Value>,
        capture: &Arc<Capture<Value>>,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        let current = match Self::validate_session(state, session) {
            Ok(session) => session,
            Err(error) => return Self::respond(reply.clone(), Err(error)),
        };
        if !token.is_active() {
            return Self::respond(
                reply.clone(),
                Err(DocumentKeyError::new(KeyErrorKind::Cancelled)),
            );
        }
        self.reload(
            state,
            &current,
            Some(PendingLoad {
                token: Arc::clone(token),
                reply: reply.clone(),
                capture: Arc::clone(capture),
                authority: None,
            }),
        )
    }

    /// Cancels only the pending load whose session and token match this action.
    fn key_cancel(
        &mut self,
        state: &mut SharedDocumentState<Value, Revision>,
        session: &Weak<SessionInner>,
        token: &Arc<LoadToken>,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        if Self::validate_session(state, session).is_err()
            || !state
                .reply
                .as_ref()
                .is_some_and(|pending| Arc::ptr_eq(&pending.token, token))
        {
            return Effect::none();
        }
        state.loading = None;
        let reply = state.reply.take().expect("matched pending load");
        Effect::merge([
            Self::respond(
                reply.reply,
                Err(DocumentKeyError::new(KeyErrorKind::Cancelled)),
            ),
            Effect::cancel(self.transitions.load_id.clone()),
        ])
    }

    /// Validates producer admission and transfers its one delivery effect into the actor.
    fn observe(
        state: &mut SharedDocumentState<Value, Revision>,
        session: &Weak<SessionInner>,
        pending: &PendingProducer<Value, Revision>,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        if let Err(error) = Self::validate_session(state, session) {
            pending.registration.fail(error);
            return Effect::none();
        }
        if state.document.aborted || !pending.registration.active.load(Ordering::Acquire) {
            pending.registration.cancel();
            return Effect::none();
        }
        pending.registration.update(SubscriptionPhase::Active {
            present: state.present,
        });
        state.producers.push(Arc::downgrade(&pending.registration));
        pending
            .effect
            .lock()
            .expect("document producer handoff poisoned")
            .take()
            .map_or_else(Effect::none, |effect| *effect)
    }

    /// Accepts a producer snapshot or revalidates it after an intervening local save.
    fn external_load(
        &mut self,
        state: &mut SharedDocumentState<Value, Revision>,
        session: &Weak<SessionInner>,
        registration: &Weak<Registration>,
        reply: &LoadReply<Value>,
        capture: &Arc<Capture<Value>>,
        snapshot: Option<&(u64, BackendSnapshot<Option<Revision>>)>,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        let current = match Self::validate_session(state, session) {
            Ok(session) => session,
            Err(error) => return Self::respond(reply.clone(), Err(error)),
        };
        if state.document.aborted
            || !registration
                .upgrade()
                .is_some_and(|registration| registration.active.load(Ordering::Acquire))
        {
            return Self::respond(
                reply.clone(),
                Err(DocumentKeyError::new(KeyErrorKind::Cancelled)),
            );
        }
        let pending = PendingLoad {
            token: Arc::new(LoadToken::new()),
            reply: reply.clone(),
            capture: Arc::clone(capture),
            authority: Some(Weak::clone(registration)),
        };
        if let Some((captured_after, snapshot)) = snapshot {
            let binding = state
                .binding
                .upgrade()
                .expect("live session retains resource binding");
            match (&binding.decode, &snapshot.revision) {
                (Decode::Plain(decode), _) => {
                    self.external_snapshot(state, &current, pending, *captured_after, || {
                        decode(snapshot.bytes.clone())
                    })
                }
                (Decode::Revisioned(decode), Some(revision)) => {
                    self.external_snapshot(state, &current, pending, *captured_after, || {
                        decode(BackendSnapshot {
                            revision: revision.clone(),
                            bytes: snapshot.bytes.clone(),
                        })
                    })
                }
                (Decode::Revisioned(_), None) => Self::respond(
                    reply.clone(),
                    Err(DocumentKeyError::new(KeyErrorKind::Load)),
                ),
            }
        } else {
            self.reload(state, &current, Some(pending))
        }
    }

    /// Revalidates stale captures before decoding a correctly typed producer snapshot.
    fn external_snapshot(
        &mut self,
        state: &mut SharedDocumentState<Value, Revision>,
        session: &Arc<SessionInner>,
        pending: PendingLoad<Value>,
        captured_after: u64,
        decode: impl FnOnce() -> Loaded<Value, Revision>,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        if session.saved_epoch.load(Ordering::Acquire) != captured_after {
            return self.reload(state, session, Some(pending));
        }
        let previous = state.reply.replace(pending);
        let attempt = self.transitions.attempt();
        state.loading = Some(attempt);
        state.load_started_after = state.last_save;
        state.deferred = None;
        let loaded = decode();
        let previous = previous.map_or_else(Effect::none, |previous| {
            Self::respond(
                previous.reply,
                Err(DocumentKeyError::new(KeyErrorKind::Superseded)),
            )
        });
        let applied = Self::loaded(state, session, attempt, &Ok(loaded));
        Effect::merge([previous, applied])
    }

    /// Preserves the producer's first terminal result after exact-session validation.
    fn producer_terminal(
        state: &SharedDocumentState<Value, Revision>,
        session: &Weak<SessionInner>,
        terminal: &ProducerTerminal,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        if let Err(error) = Self::validate_session(state, session) {
            terminal.registration.fail(error);
        } else if terminal.registration.active.load(Ordering::Acquire) {
            match &terminal.outcome {
                Ok(()) => terminal.registration.finish(),
                Err(error) if error.is_cancelled() => terminal.registration.cancel(),
                Err(error) => terminal.registration.fail(error.clone()),
            }
        }
        Effect::none()
    }

    /// Captures the value at registration commit, not at the preceding key load.
    fn subscribe(
        state: &mut SharedDocumentState<Value, Revision>,
        session: &Weak<SessionInner>,
        pending: &PendingRegistration<Value>,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        if let Err(error) = Self::validate_session(state, session) {
            pending.registration.fail(error);
            return Effect::none();
        }
        if !pending.registration.active.load(Ordering::Acquire) {
            return Effect::none();
        }
        if state.document.aborted {
            pending
                .registration
                .fail(DocumentKeyError::new(KeyErrorKind::Ended));
            return Effect::none();
        }
        let captured = state
            .present
            .then(|| (pending.capture)(state.document.value()));
        if !pending.registration.active.load(Ordering::Acquire) {
            return Effect::none();
        }
        let result = captured.map_or(Ok(true), |value| {
            pending.registration.publish(
                &state.session.upgrade().expect("live session"),
                &pending.subscriber,
                value,
            )
        });
        match result {
            Ok(true) => {
                pending.registration.update(SubscriptionPhase::Active {
                    present: state.present,
                });
                state.subscribers.push(ReferenceSubscription {
                    registration: Arc::downgrade(&pending.registration),
                    subscriber: pending.subscriber.clone(),
                    capture: Arc::clone(&pending.capture),
                });
            }
            Ok(false) => pending
                .registration
                .fail(DocumentKeyError::new(KeyErrorKind::Ended)),
            Err(error) => pending
                .registration
                .fail(DocumentKeyError::caused(KeyErrorKind::Access, error)),
        }
        Effect::none()
    }

    /// Revokes delivery and resolves outstanding barriers without claiming a physical join.
    fn abort(
        &mut self,
        state: &mut SharedDocumentState<Value, Revision>,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        state.loading = None;
        let reply = state.reply.take().map_or_else(Effect::none, |reply| {
            Self::respond(
                reply.reply,
                Err(DocumentKeyError::new(KeyErrorKind::Cancelled)),
            )
        });
        for subscription in state.subscribers.drain(..) {
            if let Some(registration) = subscription.registration.upgrade() {
                registration.cancel();
            }
        }
        for producer in state.producers.drain(..) {
            if let Some(producer) = producer.upgrade() {
                producer.cancel();
            }
        }
        let resolutions = Transitions::abort(&mut state.document);
        Effect::merge([
            reply,
            Effect::merge([
                Effect::cancel(self.transitions.load_id.clone()),
                Effect::cancel(self.transitions.save_id.clone()),
                deliver(resolutions).without_inherited_cancellation(),
            ]),
        ])
    }

    /// Applies an independent reload using its original save-order observation.
    fn loaded(
        state: &mut SharedDocumentState<Value, Revision>,
        session: &SessionInner,
        attempt: Attempt,
        result: &Result<Loaded<Value, Revision>, Failure>,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        Self::loaded_correlated(state, session, attempt, result, false)
    }

    /// Prepares clones and comparisons before checking the final cancellation fences.
    fn prepare_load(
        state: &SharedDocumentState<Value, Revision>,
        attempt: Attempt,
        result: &Result<Loaded<Value, Revision>, Failure>,
        correlated: bool,
    ) -> PreparedLoad<Value, Revision> {
        let superseded = !correlated && state.load_started_after != state.last_save;
        let has_pending = Transitions::has_pending(&state.document) || correlated;
        // Every user Clone/Eq/capture and allocation precedes the final fence.
        let deferred = if !superseded && !correlated {
            result.as_ref().ok().and_then(|loaded| {
                state
                    .document
                    .saving_revision()
                    .filter(|_| {
                        state.revisioned
                            && Some(&loaded.revision) != state.backend_revision.as_ref()
                    })
                    .map(|_| (attempt, loaded.clone()))
            })
        } else {
            None
        };
        let external = result.as_ref().ok().filter(|loaded| {
            state.revisioned && Some(&loaded.revision) != state.backend_revision.as_ref()
        });
        let conflicted = !superseded && has_pending && external.is_some();
        let conflict = if deferred.is_some() || conflicted {
            external.map_or(Replacement::Keep, |loaded| {
                Replacement::Replace(Some(SharedConflict {
                    revision: loaded.revision.clone(),
                    bytes: loaded.bytes.clone(),
                    decoded: loaded.decoded.clone(),
                }))
            })
        } else if correlated
            || (!has_pending && result.as_ref().is_ok_and(|loaded| loaded.decoded.is_ok()))
        {
            Replacement::Replace(None)
        } else {
            Replacement::Keep
        };
        let error = match result {
            Err(failure) => Some(failure.clone()),
            Ok(loaded) => loaded.decoded.as_ref().err().cloned(),
        };
        let accepted = if !has_pending && !superseded {
            result
                .as_ref()
                .ok()
                .and_then(|loaded| loaded.decoded.as_ref().ok())
                .cloned()
                .map_or(Replacement::Keep, Replacement::Replace)
        } else {
            Replacement::Keep
        };
        let mut backend_revision = None;
        if let Ok(loaded) = result {
            Self::clone_revision(&mut backend_revision, &loaded.revision);
        }
        PreparedLoad {
            superseded,
            has_pending,
            conflicted,
            deferred,
            conflict,
            error,
            accepted,
            backend_revision,
        }
    }

    /// Preserves a revision type's allocation-reusing clone when replacing an existing identity.
    fn clone_revision(target: &mut Option<Revision>, revision: &Revision) {
        match target {
            Some(target) => target.clone_from(revision),
            None => *target = Some(revision.clone()),
        }
    }

    /// Commits prepared load state only while its session, registration and token permit it.
    ///
    /// Correlated snapshots resolve after the particular save that deferred them.
    fn loaded_correlated(
        state: &mut SharedDocumentState<Value, Revision>,
        session: &SessionInner,
        attempt: Attempt,
        result: &Result<Loaded<Value, Revision>, Failure>,
        correlated: bool,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        if state.loading != Some(attempt) {
            return Effect::none();
        }
        let PreparedLoad {
            superseded,
            has_pending,
            conflicted,
            mut deferred,
            mut conflict,
            mut error,
            mut accepted,
            mut backend_revision,
        } = Self::prepare_load(state, attempt, result, correlated);
        let token = state
            .reply
            .as_ref()
            .map(|pending| Arc::clone(&pending.token));
        let requires_authority = state
            .reply
            .as_ref()
            .is_some_and(|pending| pending.authority.is_some());
        let authority = state
            .reply
            .as_ref()
            .and_then(|pending| pending.authority.as_ref())
            .and_then(Weak::upgrade);

        // Lock order is session -> registration -> per-load token. Cancellation
        // takes only its own fence; callbacks/destructors never run under these.
        let session_guard = session
            .commit
            .lock()
            .expect("document session commit poisoned");
        let registration_guard = authority.as_ref().map(|registration| {
            registration
                .commit
                .lock()
                .expect("document commit poisoned")
        });
        let token_guard = token
            .as_ref()
            .map(|token| token.commit.lock().expect("document load token poisoned"));
        let authorised = session.open.load(Ordering::Acquire)
            && token.as_ref().is_none_or(|token| token.is_active())
            && (!requires_authority
                || authority
                    .as_ref()
                    .is_some_and(|registration| registration.active.load(Ordering::Acquire)));
        let waiting = authorised && deferred.is_some();
        if authorised && !superseded {
            if let Replacement::Replace(conflict) = &mut conflict {
                std::mem::swap(&mut state.conflict, conflict);
            }
            if waiting {
                std::mem::swap(&mut state.deferred, &mut deferred);
            } else {
                std::mem::swap(&mut state.load_error, &mut error);
                if let Replacement::Replace(value) = &mut accepted {
                    state.present = value.is_some();
                    Transitions::accept_external(&mut state.document, value);
                    std::mem::swap(&mut state.backend_revision, &mut backend_revision);
                }
            }
        }
        if !waiting {
            state.loading = None;
        }
        drop(token_guard);
        drop(registration_guard);
        drop(session_guard);
        // All displaced values, causes and deferred snapshots are still owned
        // by locals here and therefore drop only after the commit fences.
        if waiting {
            return Effect::none();
        }
        let pending = state.reply.take();
        if !authorised {
            return pending.map_or_else(Effect::none, |pending| {
                Self::respond(
                    pending.reply,
                    Err(DocumentKeyError::new(KeyErrorKind::Cancelled)),
                )
            });
        }
        if matches!(accepted, Replacement::Replace(_)) {
            Self::publish(
                state.document.value(),
                state.present,
                &state.session,
                &mut state.subscribers,
            );
        }
        pending.map_or_else(Effect::none, |pending| {
            Self::respond_loaded(state, pending, result, superseded, has_pending, conflicted)
        })
    }

    /// Reports the accepted local or external value without hiding conflicts or decode failures.
    fn respond_loaded(
        state: &SharedDocumentState<Value, Revision>,
        pending: PendingLoad<Value>,
        result: &Result<Loaded<Value, Revision>, Failure>,
        superseded: bool,
        has_pending: bool,
        conflicted: bool,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        let result = if superseded {
            Ok(Some((pending.capture)(state.document.value())))
        } else {
            match result {
                Err(failure) => Err(DocumentKeyError::caused(
                    KeyErrorKind::Load,
                    failure.clone(),
                )),
                Ok(_) if conflicted => Err(DocumentKeyError::new(KeyErrorKind::Conflict)),
                Ok(loaded) => match &loaded.decoded {
                    Err(failure) => Err(DocumentKeyError::caused(
                        KeyErrorKind::Load,
                        failure.clone(),
                    )),
                    Ok(_) if has_pending => Ok(Some((pending.capture)(state.document.value()))),
                    Ok(value) => Ok(value.as_ref().map(|value| (pending.capture)(value))),
                },
            }
        };
        Self::respond(pending.reply, result)
    }

    /// Resolves actionable requests when their state's physical session has ended.
    fn reject_closed(
        action: &SharedDocumentAction<Value, Revision>,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        match &action.0 {
            SharedKind::SetAndFlush { reply, .. }
            | SharedKind::Flush { reply, .. }
            | SharedKind::KeySave { reply, .. } => {
                deliver(vec![(reply.clone(), Err(crate::ReceiptError::Aborted))])
            }
            SharedKind::KeyLoad { reply, .. } | SharedKind::ExternalLoad { reply, .. } => {
                Self::respond(
                    reply.clone(),
                    Err(DocumentKeyError::new(KeyErrorKind::Closed)),
                )
            }
            SharedKind::Subscribe { pending, .. } => {
                pending
                    .registration
                    .fail(DocumentKeyError::new(KeyErrorKind::Closed));
                Effect::none()
            }
            SharedKind::Observe { pending, .. } => {
                pending
                    .registration
                    .fail(DocumentKeyError::new(KeyErrorKind::Closed));
                Effect::none()
            }
            SharedKind::ProducerTerminal { terminal, .. } => {
                terminal.registration.cancel();
                Effect::none()
            }
            _ => Effect::none(),
        }
    }

    /// Acknowledges the matching save before releasing its deferred external snapshot.
    fn saved(
        &mut self,
        state: &mut SharedDocumentState<Value, Revision>,
        session: &SessionInner,
        attempt: Attempt,
        revision: u64,
        result: &Result<Saved<Value, Revision>, Failure>,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        if !Transitions::is_saving(&state.document, attempt, revision) {
            return Effect::none();
        }
        let outcome = match result {
            Ok(Saved::Committed(backend_revision)) => {
                session
                    .saved_epoch
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                        epoch.checked_add(1)
                    })
                    .expect("document save epoch exhausted");
                Self::clone_revision(&mut state.backend_revision, backend_revision);
                state.last_save = Some(attempt);
                state.present = true;
                Ok(backend_revision)
            }
            Ok(Saved::Conflict(conflict)) => {
                state.conflict = Some(conflict.clone());
                Err(Failure::new(
                    Operation::Save,
                    Some(revision),
                    ConflictDetected,
                ))
            }
            Err(failure) => Err(failure.clone()),
        };
        match outcome {
            Ok(expected) => {
                let requests =
                    self.transitions
                        .saved(&mut state.document, attempt, revision, &Ok(()));
                let deferred = Self::release_deferred(state, session);
                Self::publish(
                    state.document.value(),
                    state.present,
                    &state.session,
                    &mut state.subscribers,
                );
                Effect::merge([deferred, self.interpret(state, requests, expected)])
            }
            Err(failure) => {
                let resolutions = Transitions::failed_save(&mut state.document, &failure);
                let deferred = Self::release_deferred(state, session);
                Effect::merge([deferred, Self::resolve(state, resolutions)])
            }
        }
    }

    /// Releases correlated load evidence only after its save updates acknowledgement and failure state.
    fn release_deferred(
        state: &mut SharedDocumentState<Value, Revision>,
        session: &SessionInner,
    ) -> Effect<SharedDocumentAction<Value, Revision>> {
        // This admitted completion owns the save that deferred this reload.
        // Consume it before interpreting any next save produced by the transition.
        state
            .deferred
            .take()
            .map_or_else(Effect::none, |(loading, loaded)| {
                Self::loaded_correlated(state, session, loading, &Ok(loaded), true)
            })
    }
}

impl<Value: Clone + Send + 'static, Revision: Clone + Eq + Send + 'static> Reducer
    for SharedDocument<Value, Revision>
{
    type State = SharedDocumentState<Value, Revision>;
    type Action = SharedDocumentAction<Value, Revision>;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        if let SharedKind::OwnerClose { session } = &action.0
            && !Weak::ptr_eq(&state.session, session)
        {
            return Effect::none();
        }
        let aborting = matches!(&action.0, SharedKind::Abort | SharedKind::OwnerClose { .. });
        let Some(retained_session) = state.session.upgrade() else {
            return if aborting {
                self.abort(state)
            } else {
                Self::reject_closed(action)
            };
        };
        if !retained_session.open.load(Ordering::Acquire) && !aborting {
            return Self::reject_closed(action);
        }
        let effect = match &action.0 {
            SharedKind::Reload => self.reload(state, &retained_session, None),
            SharedKind::CancelLoad => {
                state.loading = None;
                let reply = state.reply.take().map_or_else(Effect::none, |reply| {
                    Self::respond(
                        reply.reply,
                        Err(DocumentKeyError::new(KeyErrorKind::Cancelled)),
                    )
                });
                Effect::merge([reply, Effect::cancel(self.transitions.load_id.clone())])
            }
            SharedKind::KeyLoad {
                session,
                token,
                reply,
                capture,
            } => self.key_load(state, session, token, reply, capture),
            SharedKind::KeyCancel { session, token } => self.key_cancel(state, session, token),
            SharedKind::KeySave {
                session,
                value,
                reply,
            } => {
                if Self::validate_session(state, session).is_err() {
                    return deliver(vec![(reply.clone(), Err(crate::ReceiptError::Aborted))]);
                }
                self.set_and_flush(state, value, reply)
            }
            SharedKind::Observe { session, pending } => Self::observe(state, session, pending),
            SharedKind::ExternalLoad {
                session,
                registration,
                reply,
                capture,
                snapshot,
            } => self.external_load(
                state,
                session,
                registration,
                reply,
                capture,
                snapshot.as_ref(),
            ),
            SharedKind::ProducerTerminal { session, terminal } => {
                Self::producer_terminal(state, session, terminal)
            }
            SharedKind::Subscribe { session, pending } => Self::subscribe(state, session, pending),
            SharedKind::SetAndFlush { value, reply } => self.set_and_flush(state, value, reply),
            SharedKind::Flush { closing, reply } => self.flush(state, *closing, reply),
            SharedKind::Abort | SharedKind::OwnerClose { .. } => self.abort(state),
            SharedKind::Loaded {
                attempt, result, ..
            } => Self::loaded(state, &retained_session, *attempt, result),
            SharedKind::Saved {
                attempt,
                revision,
                result,
                ..
            } => self.saved(state, &retained_session, *attempt, *revision, result),
        };
        // Keep the actual state-bound resource alive through user callbacks and
        // effect construction. Final owners/destructors drop outside all fences.
        drop(retained_session);
        effect
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::future::{poll_fn, ready};
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Wake, Waker};

    use rca_core::{Reduce, Store};
    use rca_test::DeterministicRuntime;

    use super::{DocumentKeyError, KeyErrorKind, SharedDocument, SharedDocumentSession};
    use crate::{Backend, BackendSession};

    /// Measures actual close ownership and error storage in the existing isolated process.
    #[cfg(feature = "file")]
    pub(crate) fn measure_session_allocations() {
        // Matches the former public marker solely to compare its optional niche.
        enum LegacyNoBackendRevision {}
        println!(
            "plain revision marker={} optional_marker={} legacy_marker={} legacy_optional_marker={}",
            std::mem::size_of::<crate::NoBackendRevision>(),
            std::mem::size_of::<Option<crate::NoBackendRevision>>(),
            std::mem::size_of::<LegacyNoBackendRevision>(),
            std::mem::size_of::<Option<LegacyNoBackendRevision>>(),
        );
        for producer_count in [0, 1, 16, 256] {
            let before = dhat::HeapStats::get();
            let backend = Arc::new(ControlledClose {
                ready: AtomicBool::new(true),
                ..ControlledClose::default()
            });
            let session = SharedDocumentSession::new(Arc::clone(&backend), None);
            for _ in 0..producer_count {
                let registration = super::Registration::new();
                session
                    .admit()
                    .unwrap()
                    .attach(&registration, Box::pin(ready(Ok(()))));
            }
            let admitted = dhat::HeapStats::get();
            let future_size = {
                let mut close = std::pin::pin!(session.close());
                let size = std::mem::size_of_val(close.as_ref().get_ref());
                assert!(matches!(
                    close.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                    Poll::Ready(Ok(()))
                ));
                size
            };
            let closed = dhat::HeapStats::get();
            let retained_debt_capacity = session.inner.control.lock().unwrap().debts.capacity();
            assert_eq!(retained_debt_capacity, 0);
            drop(session);
            drop(backend);
            let released = dhat::HeapStats::get();
            assert_eq!(released.curr_blocks, before.curr_blocks);
            assert_eq!(released.curr_bytes, before.curr_bytes);
            println!(
                "shared document joins={producer_count} close_future_size={future_size} admitted_bytes={} closed_bytes={} close_allocations={} retained_debt_capacity={retained_debt_capacity}",
                admitted.curr_bytes - before.curr_bytes,
                closed.curr_bytes - before.curr_bytes,
                closed.total_blocks - admitted.total_blocks,
            );
        }

        for caused in [false, true] {
            let before = dhat::HeapStats::get();
            let error = if caused {
                DocumentKeyError::caused(KeyErrorKind::Load, std::fmt::Error)
            } else {
                DocumentKeyError::new(KeyErrorKind::Cancelled)
            };
            let created = dhat::HeapStats::get();
            let clone = error.clone();
            let cloned = dhat::HeapStats::get();
            assert_eq!(cloned.total_blocks, created.total_blocks);
            drop((error, clone));
            let released = dhat::HeapStats::get();
            assert_eq!(released.curr_blocks, before.curr_blocks);
            assert_eq!(released.curr_bytes, before.curr_bytes);
            println!(
                "document key error caused={caused} size={} result_size={} created_blocks={} created_bytes={} clone_blocks={}",
                std::mem::size_of::<DocumentKeyError>(),
                std::mem::size_of::<Result<Option<u32>, DocumentKeyError>>(),
                created.total_blocks - before.total_blocks,
                created.total_bytes - before.total_bytes,
                cloned.total_blocks - created.total_blocks,
            );
        }

        let before = dhat::HeapStats::get();
        let failure = super::SessionFailure::new(std::fmt::Error);
        let created = dhat::HeapStats::get();
        let clone = failure.clone();
        assert!(std::ptr::eq(failure.error(), clone.error()));
        let cloned = dhat::HeapStats::get();
        assert_eq!(cloned.total_blocks, created.total_blocks);
        drop((failure, clone));
        let released = dhat::HeapStats::get();
        assert_eq!(released.curr_blocks, before.curr_blocks);
        assert_eq!(released.curr_bytes, before.curr_bytes);
        println!(
            "session failure size={} created_blocks={} created_bytes={} clone_blocks={}",
            std::mem::size_of::<super::SessionFailure>(),
            created.total_blocks - before.total_blocks,
            created.total_bytes - before.total_bytes,
            cloned.total_blocks - created.total_blocks,
        );

        measure_key_future_sizes();
    }

    /// Measures public futures without admitting a load or save to the backend.
    #[cfg(feature = "file")]
    fn measure_key_future_sizes() {
        use rca_core::{
            ActionPath, SharedKey, SharedKeyIdentity, SharedLoadContext, SharedReaderKey, StatePath,
        };

        let runtime = DeterministicRuntime::with_step_budget(64);
        let before = dhat::HeapStats::get();
        let (state, document) = SharedDocument::new(
            vec![7],
            ControlledClose {
                ready: AtomicBool::new(true),
                ..ControlledClose::default()
            },
            Bytes,
        );
        let created = dhat::HeapStats::get();
        println!(
            "shared document constructor blocks={} bytes={} debt_size={} state_size={}",
            created.total_blocks - before.total_blocks,
            created.total_bytes - before.total_bytes,
            std::mem::size_of::<super::Debt>(),
            std::mem::size_of_val(&state),
        );
        let session = document.session();
        let store = Store::new(state, document, runtime.runtime());
        let owner = super::SharedDocumentOwner::new(
            store.scope(
                StatePath::new(
                    |state: &super::SharedDocumentState<Vec<u8>>| state,
                    |state| state,
                ),
                ActionPath::new(
                    |action: &super::SharedDocumentAction<Vec<u8>>| Some(action),
                    |action| action,
                ),
            ),
            Clone::clone,
            session,
        );
        let key = owner.key(SharedKeyIdentity::new::<Bytes>("future-layout", "v1"));
        let load = key.load(SharedLoadContext::UserInitiated);
        let value = Vec::new();
        let save = key.save(&value);
        let close = owner.close();
        println!(
            "document key public futures load={} save={} owner_close={} action={}",
            std::mem::size_of_val(&load),
            std::mem::size_of_val(&save),
            std::mem::size_of_val(&close),
            std::mem::size_of::<super::SharedDocumentAction<Vec<u8>>>(),
        );
        drop((load, save, close));
        futures_executor::block_on(runtime.run(async {
            owner.close().await.unwrap();
            store.shutdown().await.unwrap();
        }))
        .unwrap();
    }

    /// Checks redacted error traces and source identity in fresh child processes.
    #[cfg(feature = "file")]
    pub(crate) fn check_error_backtraces(enabled: bool) {
        use std::error::Error;

        for caused in [false, true] {
            let error = if caused {
                DocumentKeyError::caused(KeyErrorKind::Load, io::Error::other("private key cause"))
            } else {
                DocumentKeyError::new(KeyErrorKind::Cancelled)
            };
            let clone = error.clone();
            let displayed = error.to_string();
            crate::document::tests::assert_display_rejects_writes(
                &error,
                if caused {
                    "document key Load failed"
                } else {
                    "document key Cancelled failed"
                },
                enabled,
            );
            assert_eq!(displayed, clone.to_string());
            assert!(!displayed.contains("private key cause"));
            assert!(!format!("{error:?}").contains("private key cause"));
            assert_eq!(error.source().is_some(), caused);
            if caused {
                assert!(std::ptr::eq(error.error().unwrap(), clone.error().unwrap()));
                assert_eq!(
                    error
                        .source()
                        .unwrap()
                        .downcast_ref::<io::Error>()
                        .unwrap()
                        .to_string(),
                    "private key cause"
                );
            }
            assert_eq!(displayed.lines().count() > 1, enabled);
        }

        let failure = super::SessionFailure::new(io::Error::other("private join cause"));
        let clone = failure.clone();
        let displayed = failure.to_string();
        crate::document::tests::assert_display_rejects_writes(
            &failure,
            "physical session join failed",
            enabled,
        );
        assert_eq!(displayed, clone.to_string());
        assert!(!displayed.contains("private join cause"));
        assert!(!format!("{failure:?}").contains("private join cause"));
        assert!(std::ptr::eq(failure.error(), clone.error()));
        assert_eq!(
            failure
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .to_string(),
            "private join cause"
        );
        assert_eq!(displayed.lines().count() > 1, enabled);
        let session_error = super::SessionCloseError {
            failures: vec![failure],
        };
        crate::document::tests::assert_display_rejects_writes(
            &session_error,
            "document session did not close successfully",
            true,
        );
    }

    // Private protocol witness using an actual public subscription; not a public race schedule.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "keeps the real subscription, cancellation and rejected-value ownership in one trace"
    )]
    fn cancelled_registration_rejects_retained_publication_without_replacing_reader() {
        use futures_executor::block_on;
        use rca_core::{
            ActionPath, SharedKeyCache, SharedKeyIdentity, SharedLoadContext, SharedReaderKey,
            SharedSubscribeContext, SharedSubscriber, StatePath,
        };

        #[derive(Clone)]
        struct Value {
            number: u8,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Value {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct Encoding;
        impl crate::Codec<Value> for Encoding {
            type Error = io::Error;
            fn encode(&self, value: &Value) -> Result<Vec<u8>, Self::Error> {
                Ok(vec![value.number])
            }
            fn decode(&self, bytes: &[u8]) -> Result<Value, Self::Error> {
                let [number] = bytes else {
                    return Err(io::Error::other("expected one byte"));
                };
                Ok(Value {
                    number: *number,
                    drops: Arc::new(AtomicUsize::new(0)),
                })
            }
        }
        struct ReferenceKey(Arc<Mutex<Option<SharedSubscriber<Value>>>>);
        impl SharedReaderKey for ReferenceKey {
            type Value = Value;
            type Error = std::convert::Infallible;
            type Subscription = ();

            fn identity(&self) -> SharedKeyIdentity {
                SharedKeyIdentity::new::<Self>("cancelled-publication", "value")
            }
            fn load(
                &self,
                _: SharedLoadContext<'_, Value>,
            ) -> impl Future<Output = Result<Option<Value>, Self::Error>> + Send {
                ready(Ok(None))
            }
            fn subscribe(
                &self,
                _: SharedSubscribeContext,
                subscriber: SharedSubscriber<Value>,
            ) -> Result<(), Self::Error> {
                *self.0.lock().unwrap() = Some(subscriber);
                Ok(())
            }
        }

        let runtime = DeterministicRuntime::with_step_budget(64);
        let backend = ControlledClose {
            ready: AtomicBool::new(true),
            ..ControlledClose::default()
        };
        let io_calls = Arc::clone(&backend.load_save_calls);
        let (state, reducer) = SharedDocument::new(
            Value {
                number: 7,
                drops: Arc::new(AtomicUsize::new(0)),
            },
            backend,
            Encoding,
        );
        let session = reducer.session();
        let store = Store::new(state, reducer, runtime.runtime());
        let owner = super::SharedDocumentOwner::new(
            store.scope(
                StatePath::new(
                    |state: &super::SharedDocumentState<Value>| state,
                    |state| state,
                ),
                ActionPath::new(
                    |action: &super::SharedDocumentAction<Value>| Some(action),
                    |action| action,
                ),
            ),
            Clone::clone,
            session.clone(),
        );
        let key = owner.key(SharedKeyIdentity::new::<Encoding>(
            "cancelled-publication",
            "document",
        ));
        let subscriber = Arc::new(Mutex::new(None));
        let cached_drops = Arc::new(AtomicUsize::new(0));
        let rejected_drops = Arc::new(AtomicUsize::new(0));
        let cache = SharedKeyCache::new();
        let reader = block_on(
            cache.reader(ReferenceKey(Arc::clone(&subscriber)), || Value {
                number: 3,
                drops: Arc::clone(&cached_drops),
            }),
        )
        .unwrap();
        let subscriber = subscriber.lock().unwrap().take().unwrap();
        let guard = key
            .subscribe(SharedSubscribeContext::UserInitiated, subscriber.clone())
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(guard.status().is_active());
        assert_eq!(reader.read(|value| value.number).unwrap(), 3);
        guard.cancel();
        assert!(guard.status().is_cancelled());
        assert!(session.inner.open.load(Ordering::Acquire));
        assert!(subscriber.prepare_publication().is_some());

        // Continue the actual private publication protocol after its real guard
        // was revoked. No active flag, registration or subscriber is fabricated.
        assert!(
            !guard
                .registration
                .publish(
                    &session.inner,
                    &subscriber,
                    Value {
                        number: 9,
                        drops: Arc::clone(&rejected_drops)
                    },
                )
                .unwrap()
        );
        assert_eq!(rejected_drops.load(Ordering::SeqCst), 1);
        assert_eq!(cached_drops.load(Ordering::SeqCst), 0);
        assert_eq!(reader.read(|value| value.number).unwrap(), 3);
        assert!(guard.status().is_cancelled());
        assert!(!guard.status().is_active());
        assert!(guard.status().error().is_none());
        guard.cancel();
        runtime.run_until_stalled().unwrap();
        assert_eq!(rejected_drops.load(Ordering::SeqCst), 1);
        block_on(runtime.run(async {
            owner.close().await.unwrap();
            store.shutdown().await.unwrap();
        }))
        .unwrap();
        drop(reader);
        assert_eq!(cached_drops.load(Ordering::SeqCst), 1);
        assert_eq!(rejected_drops.load(Ordering::SeqCst), 1);
        assert_eq!(io_calls.load(Ordering::SeqCst), 0);
        assert!(guard.status().is_cancelled());
    }

    #[derive(Default)]
    struct ControlledClose {
        ready: AtomicBool,
        polls: AtomicUsize,
        load_save_calls: Arc<AtomicUsize>,
        wake: Mutex<Option<Waker>>,
    }

    impl Backend for ControlledClose {
        type Error = io::Error;

        fn load(&self) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send {
            // Unexpected load/save calls remain observable and fail.
            self.load_save_calls.fetch_add(1, Ordering::SeqCst);
            ready(Err(io::Error::other("load unsupported")))
        }

        fn save(&self, _: Vec<u8>) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.load_save_calls.fetch_add(1, Ordering::SeqCst);
            ready(Err(io::Error::other("save unsupported")))
        }
    }

    impl BackendSession for ControlledClose {
        fn close(&self) -> impl Future<Output = Result<(), Self::Error>> + Send {
            poll_fn(|context| {
                self.polls.fetch_add(1, Ordering::SeqCst);
                *self.wake.lock().unwrap() = Some(context.waker().clone());
                if self.ready.load(Ordering::SeqCst) {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            })
        }
    }

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn orphaned_state_still_accepts_public_abort_with_a_distinct_interpreter() {
        let backend = ControlledClose {
            ready: AtomicBool::new(true),
            ..ControlledClose::default()
        };
        let calls = Arc::clone(&backend.load_save_calls);
        let (state, document) = SharedDocument::new(vec![7], backend, Bytes);
        let session = document.session();
        let weak = Arc::downgrade(&session.inner);
        futures_executor::block_on(session.close()).unwrap();
        drop(document);
        drop(session);
        assert!(weak.upgrade().is_none());

        // Resources belong to the state, not whichever interpreter reduces it.
        // This is real owner teardown, without dismantling a completion action.
        let other_backend = ControlledClose {
            ready: AtomicBool::new(true),
            ..ControlledClose::default()
        };
        let other_calls = Arc::clone(&other_backend.load_save_calls);
        let (_, interpreter) = SharedDocument::new(vec![11], other_backend, Bytes);
        let other_session = interpreter.session();
        let runtime = DeterministicRuntime::with_step_budget(64);
        let store = Store::new(state, interpreter, runtime.runtime());
        futures_executor::block_on(runtime.run(async {
            store
                .send(super::SharedDocumentAction::reload())
                .await
                .unwrap()
                .finish()
                .await;
            store
                .read(|state| {
                    assert_eq!(state.value(), &[7]);
                    assert_eq!(state.revision(), 0);
                    assert!(!state.is_loading());
                    assert!(!state.document().is_aborted());
                })
                .await
                .unwrap();
            store
                .send(super::SharedDocumentAction::abort())
                .await
                .unwrap()
                .finish()
                .await;
            store
                .read(|state| {
                    assert_eq!(state.value(), &[7]);
                    assert_eq!(state.revision(), 0);
                    assert_eq!(state.acknowledged_revision(), None);
                    assert!(!state.is_loading());
                    assert!(state.document().is_aborted());
                })
                .await
                .unwrap();
            store.shutdown().await.unwrap();
            other_session.close().await.unwrap();
        }))
        .unwrap();
        assert!(weak.upgrade().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(other_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn retained_backend_waker_is_inert_after_closed_session_teardown() {
        let backend = Arc::new(ControlledClose::default());
        let session = SharedDocumentSession::new(Arc::clone(&backend), None);
        let weak_session = Arc::downgrade(&session.inner);
        let notifications = Arc::new(WakeCount::default());
        let waker = Waker::from(Arc::clone(&notifications));
        let mut context = Context::from_waker(&waker);
        let mut close = Box::pin(session.close());

        assert!(close.as_mut().poll(&mut context).is_pending());
        let retained = backend.wake.lock().unwrap().as_ref().unwrap().clone();
        retained.wake_by_ref();
        assert_eq!(notifications.0.load(Ordering::SeqCst), 1);

        backend.ready.store(true, Ordering::SeqCst);
        assert!(matches!(
            close.as_mut().poll(&mut context),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(backend.polls.load(Ordering::SeqCst), 2);
        drop(close);
        drop(session);
        assert!(weak_session.upgrade().is_none());

        // This is the actual backend-supplied waker, retained across successful
        // close and final session destruction, not a constructed empty Weak.
        let before = notifications.0.load(Ordering::SeqCst);
        retained.wake_by_ref();
        retained.wake();
        assert_eq!(notifications.0.load(Ordering::SeqCst), before);
        assert_eq!(backend.polls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn duplicate_load_response_panics_and_preserves_first_result() {
        let (reply, response) = async_channel::bounded(1);
        reply.try_send(Ok(Some(7_u32))).unwrap();
        let mut effect = Some(SharedDocument::<u32>::respond(reply, Ok(Some(11))).map(|_| ()));
        let runtime = DeterministicRuntime::with_step_budget(64);
        let reducer = Reduce::new(move |(): &mut (), (): &(), _| effect.take().unwrap());
        let store = Store::new((), reducer, runtime.runtime());
        let admission = store.try_send(()).unwrap();

        // Inject a second response into a genuinely full reply channel.
        let panic = catch_unwind(AssertUnwindSafe(|| runtime.run_until_stalled()))
            .expect_err("duplicate load response must panic");
        drop(admission);
        drop(store);
        drop(runtime);

        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"document load resolved twice")
        );
        assert_eq!(response.try_recv().unwrap().unwrap(), Some(7));
    }

    struct Bytes;

    impl crate::Codec<Vec<u8>> for Bytes {
        type Error = io::Error;

        fn encode(&self, value: &Vec<u8>) -> Result<Vec<u8>, Self::Error> {
            Ok(value.clone())
        }

        fn decode(&self, bytes: &[u8]) -> Result<Vec<u8>, Self::Error> {
            Ok(bytes.to_vec())
        }
    }

    #[test]
    fn session_validation_rejects_a_closed_but_still_owned_session() {
        let backend = ControlledClose {
            ready: AtomicBool::new(true),
            ..ControlledClose::default()
        };
        let calls = Arc::clone(&backend.load_save_calls);
        let (state, reducer) = SharedDocument::new(vec![7], backend, Bytes);
        let session = reducer.session();
        let weak = Arc::downgrade(&session.inner);
        SharedDocument::validate_session(&state, &weak).unwrap();

        // Exercise the private validator after actual close, not a public race
        // between the reducer's outer admission check and this recheck.
        futures_executor::block_on(session.close()).unwrap();
        assert!(weak.upgrade().is_some());
        let Err(error) = SharedDocument::validate_session(&state, &weak) else {
            panic!("closed session passed validation");
        };
        assert!(matches!(error.kind, KeyErrorKind::Closed));
        assert!(error.is_session());
        assert_eq!(state.value(), &[7]);
        assert_eq!(state.revision(), 0);
        assert!(!state.is_loading());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(reducer);
        drop(session);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn reload_rejects_closed_retained_session_before_backend_admission() {
        let backend = ControlledClose {
            ready: AtomicBool::new(true),
            ..ControlledClose::default()
        };
        let calls = Arc::clone(&backend.load_save_calls);
        let (mut state, mut reducer) = SharedDocument::new(vec![7], backend, Bytes);
        let session = reducer.session();
        let retained = Arc::clone(&session.inner);
        assert!(retained.open.load(Ordering::Acquire));

        // This schedules the private helper boundary, not a public reducer race.
        // No owner is attached; real close fences the same session retained by
        // reduction without changing the document's loading or closing state.
        futures_executor::block_on(session.close()).unwrap();
        assert!(!retained.open.load(Ordering::Acquire));
        drop(reducer.reload(&mut state, &retained, None));

        assert_eq!(state.value(), &[7]);
        assert_eq!(state.revision(), 0);
        assert_eq!(state.acknowledged_revision(), None);
        assert_eq!(state.backend_revision(), None);
        assert!(!state.is_loading());
        assert!(!state.document().is_aborted());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "keeps cancellation phases, delivery and physical join in one lifecycle trace"
    )]
    fn terminal_fence_preserves_cancellation_during_delivery_teardown() {
        use futures_executor::block_on;
        use rca_core::{ActionPath, Reducer, StatePath};
        use rca_test::controlled_future;

        use super::{
            SharedDocumentAction, SharedDocumentOwner, SharedDocumentState, SharedKind,
            SubscriptionPhase,
        };
        use crate::SharedUpdate;

        let runtime = DeterministicRuntime::with_step_budget(64);
        let backend = ControlledClose {
            ready: AtomicBool::new(true),
            ..ControlledClose::default()
        };
        let (state, mut reducer) = SharedDocument::new(vec![7], backend, Bytes);
        let session = reducer.session();
        let delivered = Arc::new(AtomicUsize::new(0));
        let delivering = Arc::clone(&delivered);
        let reducer = Reduce::new(
            move |state: &mut SharedDocumentState<Vec<u8>>,
                  action: &SharedDocumentAction<Vec<u8>>,
                  context| {
                if matches!(
                    &action.0,
                    SharedKind::ExternalLoad { .. } | SharedKind::ProducerTerminal { .. }
                ) {
                    delivering.fetch_add(1, Ordering::SeqCst);
                }
                reducer.reduce(state, action, context)
            },
        );
        let store = Store::new(state, reducer, runtime.runtime());
        let owner = SharedDocumentOwner::new(
            store.scope(
                StatePath::new(|state: &SharedDocumentState<Vec<u8>>| state, |state| state),
                ActionPath::new(
                    |action: &SharedDocumentAction<Vec<u8>>| Some(action),
                    |action| action,
                ),
            ),
            Clone::clone,
            session,
        );
        runtime.run_until_stalled().unwrap();

        let (completion, join) = controlled_future::<Result<(), io::Error>>();
        let join_owner = Arc::new(());
        let weak_join = Arc::downgrade(&join_owner);
        let payload = Arc::new(());
        let weak_payload = Arc::downgrade(&payload);
        let cancellations = Arc::new(AtomicUsize::new(0));
        let cancelling = Arc::clone(&cancellations);
        let callback_owner = Arc::new(());
        let weak_callback = Arc::downgrade(&callback_owner);
        let callback_registration =
            Arc::new(Mutex::new(None::<std::sync::Weak<super::Registration>>));
        let cancellation_registration = Arc::clone(&callback_registration);
        let mappings = Arc::new(AtomicUsize::new(0));
        let mapping = Arc::clone(&mappings);
        let mut endpoint = None;
        let guard = owner
            .observe_updates(
                |producer| {
                    endpoint = Some(producer);
                    (
                        SharedUpdate::Present(payload),
                        move || {
                            let registration = cancellation_registration
                                .lock()
                                .unwrap()
                                .as_ref()
                                .unwrap()
                                .upgrade()
                                .unwrap();
                            drop(registration.commit.try_lock().unwrap());
                            drop(registration.phase.try_lock().unwrap());
                            assert!(registration.status().is_cancelled());
                            registration.cancel();
                            cancelling.fetch_add(1, Ordering::SeqCst);
                            drop(callback_owner);
                        },
                        async move {
                            let result = join.await.map_err(io::Error::other)?;
                            drop(join_owner);
                            result
                        },
                    )
                },
                move |_| {
                    mapping.fetch_add(1, Ordering::SeqCst);
                    None
                },
            )
            .unwrap();
        let endpoint = endpoint.unwrap();
        let weak_registration = Arc::downgrade(&guard.registration);
        *callback_registration.lock().unwrap() = Some(std::sync::Weak::clone(&weak_registration));

        // The ordered read finishes in the actor batch that admits Observe.
        // Newly spawned delivery belongs to the next deterministic task batch.
        assert_eq!(
            block_on(runtime.run(store.read(|state| state.producers.len())))
                .unwrap()
                .unwrap(),
            1
        );
        assert!(guard.status().is_active());
        assert_eq!(mappings.load(Ordering::SeqCst), 0);
        assert!(weak_payload.upgrade().is_some());
        assert!(weak_callback.upgrade().is_some());

        // The cause and fence now commit together. Delivery can run before
        // provider cancellation, but cannot replace Cancelled with Ended.
        assert_eq!(cancellations.load(Ordering::SeqCst), 0);
        guard.registration.terminate(SubscriptionPhase::Cancelled);
        runtime.run_until_stalled().unwrap();

        assert_eq!(mappings.load(Ordering::SeqCst), 0);
        assert_eq!(delivered.load(Ordering::SeqCst), 0);
        assert!(weak_payload.upgrade().is_none());
        assert!(weak_callback.upgrade().is_none());
        assert!(guard.status().is_cancelled());
        assert!(guard.status().error().is_none());
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        assert!(
            endpoint
                .publish(SharedUpdate::Present(Arc::new(())))
                .is_err()
        );
        assert!(!endpoint.finish());
        assert_eq!(
            block_on(runtime.run(store.read(|state| (
                state.value().clone(),
                state.revision(),
                state.is_loading(),
                state.load_error().is_some(),
            ))))
            .unwrap()
            .unwrap(),
            (vec![7], 0, false, false)
        );

        guard.cancel();
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        assert!(weak_join.upgrade().is_some());
        let mut closing = Box::pin(owner.close());
        let mut context = Context::from_waker(Waker::noop());
        assert!(closing.as_mut().poll(&mut context).is_pending());
        runtime.run_until_stalled().unwrap();
        assert!(closing.as_mut().poll(&mut context).is_pending());
        assert!(weak_join.upgrade().is_some());
        completion.complete(Ok(())).unwrap();
        assert!(matches!(
            closing.as_mut().poll(&mut context),
            Poll::Ready(Ok(()))
        ));
        drop(closing);
        assert!(weak_join.upgrade().is_none());
        block_on(runtime.run(store.shutdown())).unwrap().unwrap();
        guard.cancel();
        assert!(guard.status().is_cancelled());
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        drop(guard);
        assert!(weak_registration.upgrade().is_none());
    }

    #[test]
    fn completed_registration_survives_late_delivery_teardown() {
        let registration = super::Registration::new();
        let delivery = super::ProducerDelivery {
            registration: Arc::clone(&registration),
            completed: false,
        };

        registration.finish();
        drop(delivery);
        registration.cancel();

        assert!(!registration.active.load(Ordering::Acquire));
        assert!(registration.status().is_finished());
        assert!(registration.status().error().is_none());
    }

    #[test]
    fn original_failure_survives_late_delivery_teardown() {
        let registration = super::Registration::new();
        let delivery = super::ProducerDelivery {
            registration: Arc::clone(&registration),
            completed: false,
        };

        registration.fail(DocumentKeyError::caused(
            KeyErrorKind::Producer,
            io::Error::from_raw_os_error(73),
        ));
        drop(delivery);
        registration.cancel();

        assert!(!registration.active.load(Ordering::Acquire));
        let status = registration.status();
        let failure = status.error().unwrap();
        assert!(!failure.is_ended());
        assert_eq!(
            failure
                .error()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(73)
        );
    }

    #[test]
    fn earlier_ended_survives_reentrant_discarded_failure_drop() {
        #[derive(Debug)]
        struct ReentrantFailure {
            registration: std::sync::Weak<super::Registration>,
            drops: Arc<AtomicUsize>,
        }

        impl std::fmt::Display for ReentrantFailure {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("reentrant failure")
            }
        }

        impl std::error::Error for ReentrantFailure {}

        impl Drop for ReentrantFailure {
            fn drop(&mut self) {
                let registration = self.registration.upgrade().unwrap();
                drop(registration.commit.try_lock().unwrap());
                drop(registration.phase.try_lock().unwrap());
                assert!(registration.status().error().unwrap().is_ended());
                registration.cancel();
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }

        let registration = super::Registration::new();
        let drops = Arc::new(AtomicUsize::new(0));
        registration.fail(DocumentKeyError::new(KeyErrorKind::Ended));
        registration.fail(DocumentKeyError::caused(
            KeyErrorKind::Producer,
            ReentrantFailure {
                registration: Arc::downgrade(&registration),
                drops: Arc::clone(&drops),
            },
        ));
        registration.finish();
        registration.cancel();

        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(!registration.active.load(Ordering::Acquire));
        assert!(registration.status().error().unwrap().is_ended());
    }
}
