//! Implements one document's load, save, failure and lifetime state machine.

use std::backtrace::{Backtrace, BacktraceStatus};
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use rca_core::{Effect, ReduceContext, Reducer};

use crate::document_transitions::{Request, Transitions};
use crate::receipt::{Reply, channel, deliver};
use crate::{FlushOutcome, Receipt};

/// Converts a document value to and from backend bytes.
pub trait Codec<V>: Send + Sync + 'static {
    /// Original typed encoding or decoding failure.
    type Error: Error + Send + Sync + 'static;

    /// Encodes one owned effect's snapshot synchronously.
    ///
    /// # Errors
    /// Returns the codec's original error when the value cannot be encoded.
    fn encode(&self, value: &V) -> Result<Vec<u8>, Self::Error>;

    /// Decodes bytes obtained by a successful backend load.
    ///
    /// # Errors
    /// Returns the codec's original error for invalid or unsupported bytes.
    fn decode(&self, bytes: &[u8]) -> Result<V, Self::Error>;
}

/// Performs runtime-specific loading and saving without selecting a runtime here.
///
/// A successful save defines this backend's persistence guarantee. Dropping its
/// future need not stop external work. The owner must provide backend-specific
/// quiescence before another document can write the same resource.
pub trait Backend: Send + Sync + 'static {
    /// Original typed backend failure.
    type Error: Error + Send + Sync + 'static;

    /// Loads bytes, returning `None` for missing storage.
    ///
    /// # Errors
    /// Returns the backend's original load failure.
    fn load(&self) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send;

    /// Saves the supplied encoded snapshot.
    ///
    /// # Errors
    /// Returns the backend's original save failure.
    fn save(&self, bytes: Vec<u8>) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Identifies the document's current loading phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadStatus {
    /// No successful load has completed.
    Unloaded,
    /// An active load is awaiting delivery.
    Loading,
    /// Loading succeeded, permitting edits and barriers.
    Ready,
}

/// Identifies the boundary that produced a failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    /// Backend loading.
    Load,
    /// Decoding loaded bytes.
    Decode,
    /// Encoding a snapshot.
    Encode,
    /// Backend saving.
    Save,
}

/// Identifies a load or save attempt independently of document revisions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Attempt(
    /// Checked sequence number within one paired document lifetime.
    pub(crate) u64,
);

impl Attempt {
    /// Returns the identifier, increasing for every attempt in this document.
    #[must_use]
    pub const fn identifier(self) -> u64 {
        self.0
    }
}

/// Shares a typed failure while hiding document values and error messages.
///
/// Display includes a stack trace when Rust's backtrace settings enable capture.
/// Traces can contain function names and source paths, but never format the cause.
#[derive(Clone)]
pub struct Failure {
    /// Backend or codec boundary that returned the error.
    operation: Operation,
    /// Local save revision; loading and decoding have no save revision.
    revision: Option<u64>,
    /// Original cause and capture site share one allocation across all failure clones.
    source: Arc<dyn Cause>,
}

/// Exposes diagnostics without erasing the original error's downcast identity.
pub(super) trait Cause: Send + Sync {
    /// Borrows the original concrete error rather than the private capture carrier.
    fn error(&self) -> &(dyn Error + Send + Sync + 'static);

    /// Borrows the trace captured once when this cause crossed a persistence boundary.
    fn backtrace(&self) -> &Backtrace;
}

/// Stores a concrete error beside its trace in one shared allocation.
struct Captured<E> {
    /// Original error returned by the backend or codec.
    error: E,
    /// Optional stack captured at the document boundary, not at each clone.
    backtrace: Backtrace,
}

impl<E: Error + Send + Sync + 'static> Cause for Captured<E> {
    fn error(&self) -> &(dyn Error + Send + Sync + 'static) {
        &self.error
    }

    fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

/// Retains an original error and its capture site in one shared allocation.
pub(super) fn capture(error: impl Error + Send + Sync + 'static) -> Arc<dyn Cause> {
    Arc::new(Captured {
        error,
        backtrace: Backtrace::capture(),
    })
}

impl Failure {
    /// Records a boundary failure while retaining its original concrete error.
    pub(crate) fn new(
        operation: Operation,
        revision: Option<u64>,
        error: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            operation,
            revision,
            source: capture(error),
        }
    }

    /// Returns the failed boundary.
    #[must_use]
    pub const fn operation(&self) -> Operation {
        self.operation
    }

    /// Returns the attempted save revision, or `None` for loading/decoding.
    #[must_use]
    pub const fn revision(&self) -> Option<u64> {
        self.revision
    }

    /// Returns the original error for typed inspection without cloning its payload.
    ///
    /// Unlike this failure's formatting, explicitly formatting the returned
    /// source can reveal sensitive data. Cloned failures share this same error.
    #[must_use]
    pub fn error(&self) -> &(dyn Error + Send + Sync + 'static) {
        self.source.error()
    }
}

impl fmt::Debug for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Failure")
            .field("operation", &self.operation)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "document {:?} failed", self.operation)?;
        let backtrace = self.source.backtrace();
        if backtrace.status() == BacktraceStatus::Captured {
            write!(f, "\n{backtrace}")?;
        }
        Ok(())
    }
}

impl Error for Failure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.error())
    }
}

/// Identifies the one completion currently allowed to change document bookkeeping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Active {
    /// Reads storage without a local snapshot revision.
    Load(
        /// Identity that excludes stale load completions.
        Attempt,
    ),
    /// Encodes and saves one captured local revision.
    Save {
        /// Identity that excludes stale completions, including retries of one revision.
        attempt: Attempt,
        /// Local revision represented by the saved snapshot.
        revision: u64,
    },
}

impl Active {
    /// Returns the completion identity shared by the operation and its codec work.
    pub(crate) const fn attempt(self) -> Attempt {
        match self {
            Self::Load(attempt) | Self::Save { attempt, .. } => attempt,
        }
    }
}

/// Retains one accepted barrier until its captured revision succeeds or fails.
pub(crate) struct Barrier {
    /// Revision and close intent captured when the action was reduced.
    pub(crate) outcome: FlushOutcome,
    /// Sender moved into the acknowledgement effect after state commits.
    pub(crate) reply: Reply,
}

/// Holds a document's value and inspectable, actor-owned coordination state.
///
/// The explicit default is initially unloaded. An acknowledged revision of
/// `None` distinguishes a missing/unmaterialised default from persisted revision
/// zero. Fields are private; update this state only through its paired reducer.
pub struct State<V> {
    /// Latest value, including edits newer than an in-flight save.
    pub(crate) value: V,
    /// Monotonically increasing local edit revision.
    pub(crate) revision: u64,
    /// Latest revision confirmed by a load or save; absent storage has none.
    pub(crate) acknowledged_revision: Option<u64>,
    /// Latest local edit not yet acknowledged, distinct from an unwritten default.
    pub(crate) edited_revision: Option<u64>,
    /// Initial loading phase that gates edits and persistence barriers.
    pub(crate) load_status: LoadStatus,
    /// Completion identity owned by the current load or save effect.
    pub(crate) active: Option<Active>,
    /// Last recoverable error, which suspends automatic saving until an explicit retry.
    pub(crate) failure: Option<Failure>,
    /// Accepted replies in arrival order, with nondecreasing captured revisions.
    pub(crate) barriers: Vec<Barrier>,
    /// Permanent edit fence installed by an accepted close request.
    pub(crate) closing: bool,
    /// Whether the fenced final revision has been acknowledged.
    pub(crate) closed: bool,
    /// Permanent lifetime fence installed by abort, independent of physical I/O completion.
    pub(crate) aborted: bool,
}

impl<V> State<V> {
    /// Returns the current value, including the explicit default before loading.
    #[must_use]
    pub const fn value(&self) -> &V {
        &self.value
    }

    /// Returns the current edit revision.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns the latest loaded/saved revision, or `None` before materialisation.
    #[must_use]
    pub const fn acknowledged_revision(&self) -> Option<u64> {
        self.acknowledged_revision
    }

    /// Returns the current loading phase.
    #[must_use]
    pub const fn load_status(&self) -> LoadStatus {
        self.load_status
    }

    /// Returns the revision owned by an active encoding/save effect.
    #[must_use]
    pub fn saving_revision(&self) -> Option<u64> {
        match self.active {
            Some(Active::Save { revision, .. }) => Some(revision),
            None | Some(Active::Load(_)) => None,
        }
    }

    /// Returns the active load/save attempt identifier.
    #[must_use]
    pub fn active_attempt(&self) -> Option<Attempt> {
        self.active.map(Active::attempt)
    }

    /// Returns the last failure suspending automatic saving or loading.
    #[must_use]
    pub const fn failure(&self) -> Option<&Failure> {
        self.failure.as_ref()
    }

    /// Returns outstanding barriers, including waiters whose receipt was dropped.
    #[must_use]
    pub fn pending_barrier_count(&self) -> usize {
        self.barriers.len()
    }

    /// Returns whether close permanently fenced edits, including after successful closure.
    #[must_use]
    pub const fn is_closing(&self) -> bool {
        self.closing
    }

    /// Returns whether the final close revision was successfully acknowledged.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Returns whether this document lifetime was explicitly aborted.
    #[must_use]
    pub const fn is_aborted(&self) -> bool {
        self.aborted
    }
}

impl<V> fmt::Debug for State<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("State")
            .field("revision", &self.revision)
            .field("acknowledged_revision", &self.acknowledged_revision)
            .field("load_status", &self.load_status)
            .field("active", &self.active)
            .field("failure", &self.failure)
            .field("pending_barrier_count", &self.barriers.len())
            .field("closing", &self.closing)
            .field("closed", &self.closed)
            .field("aborted", &self.aborted)
            .finish_non_exhaustive()
    }
}

/// Requests an operation without exposing internal completion construction.
///
/// Actions are borrowed during reduction, not consumed through a mutex envelope.
/// Retaining an original barrier action retains a reply sender and can delay
/// [`crate::ReceiptError::Abandoned`] if the acknowledgement effect is cancelled.
pub struct Action<V>(
    /// Public request or private completion admitted by the paired reducer.
    pub(crate) Kind<V>,
);

/// Separates caller requests from completions that only owned effects can construct.
pub(crate) enum Kind<V> {
    /// Starts or replaces the initial load.
    Load,
    /// Replaces the latest value after loading and before a lifetime fence.
    Set(V),
    /// Captures a revision and retains its acknowledgement sender.
    Barrier {
        /// Whether to fence future edits as well as wait for persistence.
        closing: bool,
        /// Retained by both the borrowed action and its accepted barrier.
        reply: Reply,
    },
    /// Cancels named delivery and permanently rejects later work.
    Abort,
    /// Delivers a backend load and its optional decoded snapshot.
    Loaded {
        /// Identity of the load that produced this result.
        attempt: Attempt,
        /// Missing storage retains the explicit default.
        result: Result<Option<V>, Failure>,
    },
    /// Delivers a completed encoding/save attempt.
    Saved {
        /// Identity of the save that produced this result.
        attempt: Attempt,
        /// Revision captured when the save began.
        revision: u64,
        /// Original encoding or backend failure, if any.
        result: Result<(), Failure>,
    },
}

impl<V> Action<V> {
    /// Loads storage, replacing a pending load; does nothing once ready or fenced.
    #[must_use]
    pub const fn load() -> Self {
        Self(Kind::Load)
    }

    /// Sets a value once ready, coalescing saves; ignored before loading or after fencing.
    ///
    /// # Panics
    /// Reduction panics if the revision counter is exhausted; it never wraps.
    #[must_use]
    pub const fn set(value: V) -> Self {
        Self(Kind::Set(value))
    }

    /// Captures a persistence barrier for the revision at action commit.
    ///
    /// The receipt returns `NotLoaded`, `Closing` or `Aborted` when not accepted,
    /// and `Persistence` on a failed save. A failure is retried explicitly using
    /// the latest value. A clean materialised revision requires no backend I/O.
    #[must_use = "submit the action and await its receipt to confirm persistence"]
    pub fn flush() -> (Self, Receipt) {
        Self::barrier(false)
    }

    /// Permanently fences edits and acknowledges the final revision on success.
    ///
    /// Repeated close joins a pending save or retries after failure. Requests
    /// before successful loading return `NotLoaded` without fencing or writing.
    #[must_use = "submit the action and await its receipt before shutdown"]
    pub fn close() -> (Self, Receipt) {
        Self::barrier(true)
    }

    /// Permanently aborts this lifetime without guaranteeing external I/O quiescence.
    ///
    /// Cancels named load/save delivery and resolves pending/new barriers with
    /// `Aborted`. Restart requires backend quiescence and a new document/load.
    /// Terminal acknowledgements run after publication and exclude inherited
    /// named cancellation identities, including the save being aborted. Root
    /// task cancellation, scope removal, shutdown and explicit wrappers still
    /// own their delivery.
    #[must_use]
    pub const fn abort() -> Self {
        Self(Kind::Abort)
    }

    /// Creates the request and receipt without changing document state or starting I/O.
    fn barrier(closing: bool) -> (Self, Receipt) {
        let (reply, receipt) = channel();
        (Self(Kind::Barrier { closing, reply }), receipt)
    }
}

impl<V> fmt::Debug for Action<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match &self.0 {
            Kind::Load => "Load",
            Kind::Set(_) => "Set(..)",
            Kind::Barrier { closing: false, .. } => "Flush(..)",
            Kind::Barrier { closing: true, .. } => "Close(..)",
            Kind::Abort => "Abort",
            Kind::Loaded { .. } => "Loaded(..)",
            Kind::Saved { .. } => "Saved(..)",
        })
    }
}

/// Coordinates one paired State's document lifetime with owned backend/codec captures.
///
/// Values need only `Clone + Send + 'static`, not `Sync`, `Debug` or `Serialize`.
/// Use exactly the state returned by `new`; do not reuse this reducer for a new
/// document lifetime. Attempts and revisions use checked arithmetic and panic
/// on exhaustion instead of admitting stale completions through wraparound.
/// A panic in user code ends this reducer's lifetime; its state is not recoverable.
pub struct Document<V, B, C> {
    /// Backend retained by the reducer and every effect that can perform I/O.
    backend: Arc<B>,
    /// Codec retained by effects independently of the reducer's lifetime.
    codec: Arc<C>,
    /// Runtime-independent coordination shared with the shared-document implementation.
    transitions: Transitions,
    /// Associates the value type without imposing its auto traits on this owner.
    value: PhantomData<fn() -> V>,
}

impl<V, B, C> fmt::Debug for Document<V, B, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Document")
            .field("next_attempt", &self.transitions.next_attempt)
            .finish_non_exhaustive()
    }
}

impl<V, B, C> Document<V, B, C>
where
    V: Clone + Send + 'static,
    B: Backend,
    C: Codec<V>,
{
    /// Creates unloaded state and its exclusive document reducer from explicit dependencies.
    #[must_use]
    pub fn new(default: V, backend: B, codec: C) -> (State<V>, Self) {
        let state = Transitions::state(default);
        let document = Self {
            backend: Arc::new(backend),
            codec: Arc::new(codec),
            transitions: Transitions::new(),
            value: PhantomData,
        };
        (state, document)
    }

    /// Loads and decodes lazily, replacing earlier named load delivery.
    fn load(&self, attempt: Attempt) -> Effect<Action<V>> {
        let backend = Arc::clone(&self.backend);
        let codec = Arc::clone(&self.codec);
        Effect::future(async move {
            let result = match backend.load().await {
                Ok(Some(bytes)) => codec
                    .decode(&bytes)
                    .map(Some)
                    .map_err(|error| Failure::new(Operation::Decode, None, error)),
                Ok(None) => Ok(None),
                Err(error) => Err(Failure::new(Operation::Load, None, error)),
            };
            Some(Action(Kind::Loaded { attempt, result }))
        })
        .cancellable_in_flight(self.transitions.load_id.clone())
    }

    /// Encodes one owned snapshot before awaiting its backend save.
    fn save(&self, attempt: Attempt, revision: u64, value: V) -> Effect<Action<V>> {
        let backend = Arc::clone(&self.backend);
        let codec = Arc::clone(&self.codec);
        Effect::future(async move {
            let encoded = codec.encode(&value);
            drop(value); // No shared reference or value survives into the backend await.
            let result = match encoded {
                Ok(bytes) => backend
                    .save(bytes)
                    .await
                    .map_err(|error| Failure::new(Operation::Save, Some(revision), error)),
                Err(error) => Err(Failure::new(Operation::Encode, Some(revision), error)),
            };
            Some(Action(Kind::Saved {
                attempt,
                revision,
                result,
            }))
        })
        .cancellable(self.transitions.save_id.clone())
    }

    /// Commits coordination state, then translates requests into owned effects.
    fn handle(&mut self, state: &mut State<V>, action: &Action<V>) -> Effect<Action<V>> {
        let requests = match &action.0 {
            Kind::Load => {
                return Effect::merge(
                    self.transitions
                        .load(state)
                        .into_iter()
                        .map(|attempt| self.load(attempt)),
                );
            }
            Kind::Set(value) => self.transitions.set(state, value),
            Kind::Barrier { closing, reply } => self.transitions.barrier(state, *closing, reply),
            Kind::Abort => {
                let resolutions = Transitions::abort(state);
                return Effect::merge([
                    Effect::cancel(self.transitions.load_id.clone()),
                    Effect::cancel(self.transitions.save_id.clone()),
                    deliver(resolutions).without_inherited_cancellation(),
                ]);
            }
            Kind::Loaded { attempt, result } => {
                Transitions::loaded(state, *attempt, result);
                Vec::new()
            }
            Kind::Saved {
                attempt,
                revision,
                result,
            } => self.transitions.saved(state, *attempt, *revision, result),
        };
        Effect::merge(requests.into_iter().map(|request| match request {
            Request::Save {
                attempt,
                revision,
                value,
            } => self.save(attempt, revision, value),
            Request::Resolve(resolutions) => {
                let acknowledgement = deliver(resolutions);
                if state.aborted {
                    acknowledgement.without_inherited_cancellation()
                } else {
                    acknowledgement
                }
            }
        }))
    }
}

impl<V, B, C> Reducer for Document<V, B, C>
where
    V: Clone + Send + 'static,
    B: Backend,
    C: Codec<V>,
{
    type State = State<V>;
    type Action = Action<V>;

    fn reduce(
        &mut self,
        state: &mut State<V>,
        action: &Action<V>,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action<V>> {
        self.handle(state, action)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::convert::Infallible;
    use std::task::{Context, Poll, Waker};

    use super::*;
    use crate::ReceiptError;

    /// Measures the error owner in the existing isolated allocation process.
    #[cfg(feature = "file")]
    pub(crate) fn measure_failure_allocations() {
        let before = dhat::HeapStats::get();
        let failure = Failure::new(Operation::Decode, None, fmt::Error);
        let created = dhat::HeapStats::get();
        let clone = failure.clone();
        let cloned = dhat::HeapStats::get();
        assert!(std::ptr::eq(failure.error(), clone.error()));
        drop(failure);
        drop(clone);
        let released = dhat::HeapStats::get();
        assert_eq!(created.total_blocks - before.total_blocks, 1);
        assert_eq!(cloned.total_blocks, created.total_blocks);
        assert_eq!(released.curr_blocks, before.curr_blocks);
        assert_eq!(released.curr_bytes, before.curr_bytes);
        println!(
            "document failure size={} created_blocks={} created_bytes={} clone_blocks={}",
            std::mem::size_of::<Failure>(),
            created.total_blocks - before.total_blocks,
            created.total_bytes - before.total_bytes,
            cloned.total_blocks - created.total_blocks,
        );
    }

    /// Checks that diagnostics preserve writer errors before and after their summary.
    #[cfg(feature = "file")]
    pub(crate) fn assert_display_rejects_writes(
        value: &impl fmt::Display,
        summary: &str,
        has_details: bool,
    ) {
        /// Rejects the header or first detail line without relying on write boundaries.
        struct RejectingWriter {
            reject_header: bool,
            accepted: String,
        }

        impl fmt::Write for RejectingWriter {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                if self.reject_header {
                    return Err(fmt::Error);
                }
                if let Some(newline) = s.find('\n') {
                    self.accepted.push_str(&s[..newline]);
                    return Err(fmt::Error);
                }
                self.accepted.push_str(s);
                Ok(())
            }
        }

        let mut header = RejectingWriter {
            reject_header: true,
            accepted: String::new(),
        };
        assert_eq!(
            fmt::write(&mut header, format_args!("{value}")),
            Err(fmt::Error)
        );
        assert_eq!(header.accepted, "");
        if has_details {
            let mut details = RejectingWriter {
                reject_header: false,
                accepted: String::new(),
            };
            assert_eq!(
                fmt::write(&mut details, format_args!("{value}")),
                Err(fmt::Error)
            );
            assert_eq!(details.accepted, summary);
        }
    }

    /// Checks the public diagnostic contract under fresh-process backtrace settings.
    #[cfg(feature = "file")]
    pub(crate) fn check_failure_backtraces(enabled: bool) {
        for operation in [
            Operation::Load,
            Operation::Decode,
            Operation::Encode,
            Operation::Save,
        ] {
            let revision = match operation {
                Operation::Load | Operation::Decode => None,
                Operation::Encode | Operation::Save => Some(3),
            };
            let failure = Failure::new(operation, revision, std::io::Error::other("private-cause"));
            let clone = failure.clone();
            let displayed = failure.to_string();
            let summary = format!("document {operation:?} failed");
            assert_display_rejects_writes(&failure, &summary, enabled);
            assert!(displayed.starts_with(&summary));
            assert!(!displayed.contains("private-cause"));
            assert!(!format!("{failure:?}").contains("private-cause"));
            assert_eq!(displayed, clone.to_string());
            assert!(std::ptr::eq(failure.error(), clone.error()));
            assert_eq!(
                failure
                    .source()
                    .unwrap()
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .to_string(),
                "private-cause"
            );
            if enabled {
                assert!(
                    displayed.lines().count() > 1,
                    "enabled document diagnostics must include the captured stack"
                );
            } else {
                assert_eq!(displayed, summary);
            }
        }
    }

    struct Pending;

    impl Backend for Pending {
        type Error = Infallible;

        fn load(&self) -> impl Future<Output = Result<Option<Vec<u8>>, Infallible>> + Send {
            std::future::pending()
        }

        fn save(&self, _: Vec<u8>) -> impl Future<Output = Result<(), Infallible>> + Send {
            std::future::pending()
        }
    }

    struct Number;

    impl Codec<i64> for Number {
        type Error = std::io::Error;

        fn encode(&self, value: &i64) -> Result<Vec<u8>, Self::Error> {
            Ok(value.to_le_bytes().to_vec())
        }

        fn decode(&self, bytes: &[u8]) -> Result<i64, Self::Error> {
            bytes
                .try_into()
                .map(i64::from_le_bytes)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        }
    }

    impl Codec<String> for Number {
        type Error = std::string::FromUtf8Error;

        fn encode(&self, value: &String) -> Result<Vec<u8>, Self::Error> {
            Ok(value.as_bytes().to_vec())
        }

        fn decode(&self, bytes: &[u8]) -> Result<String, Self::Error> {
            String::from_utf8(bytes.to_vec())
        }
    }

    fn loaded(attempt: u64, value: i64) -> Action<i64> {
        Action(Kind::Loaded {
            attempt: Attempt(attempt),
            result: Ok(Some(value)),
        })
    }

    fn saved(attempt: u64, revision: u64) -> Action<i64> {
        Action(Kind::Saved {
            attempt: Attempt(attempt),
            revision,
            result: Ok(()),
        })
    }

    // Catches stale attempt, wrong operation and wrong revision delivery mutating active bookkeeping.
    #[test]
    fn completions_must_match_operation_attempt_and_revision() {
        let (mut state, mut document) = Document::new(7, Pending, Number);
        drop(document.handle(&mut state, &Action::load()));
        drop(document.handle(&mut state, &loaded(0, 99)));
        drop(document.handle(&mut state, &saved(1, 0)));
        assert_eq!(state.value(), &7);
        assert_eq!(state.load_status(), LoadStatus::Loading);
        assert_eq!(state.active_attempt(), Some(Attempt(1)));
        drop(document.handle(&mut state, &loaded(1, 11)));
        drop(document.handle(&mut state, &Action::set(23)));
        for action in [saved(1, 1), saved(2, 0), loaded(2, 99)] {
            drop(document.handle(&mut state, &action));
            assert_eq!(state.value(), &23);
            assert_eq!(state.acknowledged_revision(), Some(0));
            assert_eq!(state.saving_revision(), Some(1));
            assert_eq!(state.active_attempt(), Some(Attempt(2)));
        }
        drop(document.handle(&mut state, &saved(2, 1)));
        drop(document.handle(&mut state, &Action::set(37)));
        drop(document.handle(&mut state, &saved(2, 1)));
        assert_eq!(state.acknowledged_revision(), Some(1));
        assert_eq!(state.saving_revision(), Some(2));
        assert_eq!(state.active_attempt(), Some(Attempt(3)));
        drop(document.handle(&mut state, &saved(3, 2)));
        assert_eq!(state.acknowledged_revision(), Some(2));
        assert_eq!(state.active_attempt(), None);
    }

    // Catches old error completions replacing a newer retry's active state or resolving its barriers.
    #[test]
    fn old_failed_attempt_does_not_interrupt_retry_of_same_revision() {
        let (mut state, mut document) = Document::new(7, Pending, Number);
        drop(document.handle(&mut state, &Action::load()));
        drop(document.handle(&mut state, &loaded(1, 7)));
        drop(document.handle(&mut state, &Action::set(11)));
        let failure = Action(Kind::Saved {
            attempt: Attempt(2),
            revision: 1,
            result: Err(Failure::new(
                Operation::Save,
                Some(1),
                std::io::Error::other("secret"),
            )),
        });
        drop(document.handle(&mut state, &failure));
        let (action, receipt) = Action::flush();
        drop(document.handle(&mut state, &action));
        drop(document.handle(&mut state, &failure));
        assert!(state.failure().is_none());
        assert_eq!(state.active_attempt(), Some(Attempt(3)));
        assert_eq!(state.saving_revision(), Some(1));
        assert_eq!(state.pending_barrier_count(), 1);
        drop(document.handle(&mut state, &saved(3, 1)));
        assert_eq!(state.pending_barrier_count(), 0);
        let mut receipt = Box::pin(receipt);
        assert!(
            receipt
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending(),
            "retained original action owns a reply sender after discarded delivery"
        );
        drop(action);
        assert!(matches!(
            receipt
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(ReceiptError::Abandoned))
        ));
    }

    // Catches wraparound admitting old completions or silently losing edits.
    #[test]
    fn exhausted_counters_do_not_wrap_or_change_the_value() {
        let (mut state, mut document) = Document::new(7, Pending, Number);
        document.transitions.next_attempt = u64::MAX;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            document.handle(&mut state, &Action::load())
        }))
        .unwrap_err();
        assert_eq!(state.load_status(), LoadStatus::Unloaded);
        state.load_status = LoadStatus::Ready;
        state.revision = u64::MAX;
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            document.handle(&mut state, &Action::set(99))
        }))
        .unwrap_err();
        assert_eq!(state.value(), &7);
        assert_eq!(state.revision(), u64::MAX);
    }

    // Catches sensitive value/source formatting or accidental V/B/C Debug bounds.
    #[test]
    fn diagnostics_redact_values_and_original_errors() {
        let (mut state, document) =
            Document::new("private-document-value".to_owned(), Pending, Number);
        let failure = Failure::new(
            Operation::Decode,
            None,
            std::io::Error::other("private-error-value"),
        );
        state.failure = Some(failure.clone());
        let rendered = format!(
            "{state:?} {document:?} {:?} {:?} {failure:?} {failure}",
            Action::set("private-action-value"),
            ReceiptError::Persistence(failure.clone())
        );
        for sensitive in [
            "private-document-value",
            "private-error-value",
            "private-action-value",
        ] {
            assert!(!rendered.contains(sensitive));
        }
        assert_eq!(
            failure
                .source()
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .to_string(),
            "private-error-value"
        );
        let cloned = failure.clone();
        assert!(std::ptr::eq(failure.error(), cloned.error()));
    }
}
