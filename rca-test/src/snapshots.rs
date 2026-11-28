//! Composes fallible captures, codecs and explicitly authorised reference writes.
//!
//! These helpers never discover files, inspect the environment, spawn tasks or
//! capture providers. Callers supply independent captures, reference bytes,
//! redaction, writer authority and a timeout future. Comparisons cover exactly
//! the selected capture, not omitted domain fields or aliased historical state.
//!
//! # Comparison and recording
//!
//! Exact byte comparison precedes artefact redaction. Lossy recorded references
//! need a compatible caller-selected canonical projection/codec to replay; no
//! implicit normalisation makes unequal bytes equal. Identity redaction is useful
//! for already-safe canonical data. Inline references remain ordinary Insta
//! assertions; this module never edits source files.
//!
//! Request query ordering, duplicate preservation, body pretty-printing, newline
//! and Unicode policy belong to caller projections/codecs. None are inferred.
//! Reference reads can be ordinary fallible caller operations before `check`;
//! keep their typed read error distinct from the returned snapshot failure.
//! Options are explicit values, not inherited global or task-local settings.
//!
//! Failure types preserve arbitrary typed causes without requiring `Error`.
//! Their `std::error::Error::source` therefore returns `None`; inspect enum
//! variants or use the typed cause accessors instead.
//!
//! # Examples
//!
//! This capture copies the selected bytes. Later input changes cannot alter them.
//!
//! ```
//! use rca_test::snapshots::SnapshotStrategy;
//!
//! let mut strategy = SnapshotStrategy::new(
//!     |value: &str| Ok::<_, ()>(value.as_bytes().to_vec()),
//!     |expected: &[u8], actual: &[u8]| Ok::<_, ()>(expected == actual),
//! );
//! let mut input = String::from("safe");
//! let captured = strategy.capture(input.as_str()).unwrap();
//! input.clear();
//!
//! assert!(strategy.compare(captured.as_slice(), b"safe").unwrap());
//! ```

use std::collections::BTreeSet;
use std::fmt;
use std::future::poll_fn;
use std::panic::Location;
use std::task::Poll;

/// Combines caller-selected capture and comparison operations.
pub struct SnapshotStrategy<C, M> {
    /// Retains caller-owned capture state without boxing or erasing its type.
    capture: C,
    /// Stays separate so a pullback changes only the capture input.
    compare: M,
}

impl<C, M> fmt::Debug for SnapshotStrategy<C, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotStrategy").finish_non_exhaustive()
    }
}

impl<C, M> SnapshotStrategy<C, M> {
    /// Creates a strategy without invoking either operation.
    pub const fn new(capture: C, compare: M) -> Self {
        Self { capture, compare }
    }

    /// Captures a selected value without imposing domain trait bounds.
    ///
    /// The callback must preserve independence from later input changes.
    /// Cloning a shared pointer does not by itself guarantee historical state.
    ///
    /// # Errors
    /// Preserves the capture operation's typed error.
    ///
    /// # Panics
    /// Propagates capture callback panics.
    pub fn capture<I: ?Sized, V, E>(&mut self, input: &I) -> Result<V, E>
    where
        C: FnMut(&I) -> Result<V, E>,
    {
        (self.capture)(input)
    }

    /// Compares selected values without invoking capture or rendering.
    ///
    /// # Errors
    /// Preserves the comparator's typed error.
    ///
    /// # Panics
    /// Propagates comparator panics.
    pub fn compare<V: ?Sized, E>(&mut self, expected: &V, actual: &V) -> Result<bool, E>
    where
        M: FnMut(&V, &V) -> Result<bool, E>,
    {
        (self.compare)(expected, actual)
    }

    /// Projects input fallibly at capture time, preserving the comparator.
    ///
    /// Construction invokes neither callback. Each capture projects the input
    /// first. A projection error skips capture and retains its distinct cause.
    /// Capture propagates callback and projected-value destructor panics.
    ///
    /// # Examples
    ///
    /// ```
    /// use rca_test::snapshots::SnapshotStrategy;
    ///
    /// struct Job {
    ///     attempts: u32,
    /// }
    ///
    /// let mut strategy = SnapshotStrategy::new(
    ///     |attempts: &u32| Ok::<_, ()>(*attempts),
    ///     |expected: &u32, actual: &u32| Ok::<_, ()>(expected == actual),
    /// )
    /// .try_pullback(|job: &Job| Ok::<_, ()>(job.attempts));
    ///
    /// let attempts = strategy.capture(&Job { attempts: 2 }).unwrap();
    /// assert!(strategy.compare(&2, &attempts).unwrap());
    /// ```
    pub fn try_pullback<I: ?Sized, V, O, P, E>(
        self,
        mut project: impl FnMut(&I) -> Result<V, P>,
    ) -> SnapshotStrategy<impl FnMut(&I) -> Result<O, PullbackError<P, E>>, M>
    where
        C: FnMut(&V) -> Result<O, E>,
    {
        let mut capture = self.capture;
        SnapshotStrategy::new(
            move |input: &I| {
                let value = project(input).map_err(PullbackError::Projection)?;
                let result = capture(&value).map_err(PullbackError::Capture);
                // Retain the result if the projected value's destructor panics.
                drop(value);
                result
            },
            self.compare,
        )
    }
}

/// Preserves distinct projection and capture causes without rendering them.
pub enum PullbackError<P, E> {
    /// The input projection failed before capture.
    Projection(P),
    /// Capture failed after successful projection.
    Capture(E),
}

impl<P, E> fmt::Debug for PullbackError<P, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Projection(_) => "Projection(..)",
            Self::Capture(_) => "Capture(..)",
        })
    }
}

impl<P, E> fmt::Display for PullbackError<P, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Projection(_) => "Snapshot projection failed.",
            Self::Capture(_) => "Snapshot capture failed.",
        })
    }
}

impl<P, E> std::error::Error for PullbackError<P, E> {}

/// Supplies explicit per-operation codec options and selected or detected format.
pub struct CodecContext<C, F> {
    /// Caller-owned options; reference values are not implicitly deep-copied.
    pub context: C,
    /// Selected output format or decoder-detected input format.
    pub format: F,
}

impl<C, F> fmt::Debug for CodecContext<C, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodecContext").finish_non_exhaustive()
    }
}

/// Chooses whether failed decoding restores only the format value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatRollback {
    /// Retains format detection even when later typed decoding fails.
    KeepDetected,
    /// Restores the cloned format on error, but never rolls back context.
    OnError,
}

/// Substitutes encoding and decoding through caller-selected operations.
///
/// Callbacks define the byte format and can mutate their supplied context.
/// The codec adds no normalisation, I/O or implicit format detection.
///
/// # Examples
///
/// ```
/// use rca_test::snapshots::{Codec, CodecContext};
///
/// let mut codec = Codec::new(
///     |value: &u8, _: &mut CodecContext<(), ()>| Ok::<_, ()>(vec![*value]),
///     |bytes: &[u8], _: &mut CodecContext<(), ()>| match bytes {
///         [value] => Ok(*value),
///         _ => Err("expected one byte"),
///     },
/// );
/// let mut context = CodecContext {
///     context: (),
///     format: (),
/// };
/// let bytes = codec.encode(&7, &mut context).unwrap();
///
/// assert_eq!(codec.decode(&bytes, &mut context).unwrap(), 7);
/// ```
pub struct Codec<E, D> {
    /// Retains encoding state independently from decoding state.
    encode: E,
    /// Retains the concrete decoder, including non-`Send` caller state.
    decode: D,
}

impl<E, D> fmt::Debug for Codec<E, D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Codec").finish_non_exhaustive()
    }
}

/// Identifies which conversion failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecOperation {
    /// Encoding a selected value failed.
    Encode,
    /// Parsing or typed decoding failed.
    Decode,
}

/// Retains a codec cause and operation without implicit payload formatting.
pub struct CodecFailure<E> {
    /// Owns the original cause without requiring formatting or error traits.
    cause: E,
    /// Distinguishes conversion direction without inspecting the cause.
    operation: CodecOperation,
    /// Captures the public call site before invoking caller code.
    location: &'static Location<'static>,
}
impl<E> CodecFailure<E> {
    /// Returns the original typed cause.
    pub const fn cause(&self) -> &E {
        &self.cause
    }
    /// Returns the failed conversion direction.
    pub const fn operation(&self) -> CodecOperation {
        self.operation
    }
    /// Returns the conversion caller's location.
    pub const fn location(&self) -> &'static Location<'static> {
        self.location
    }
    /// Recovers the original typed cause by ownership.
    pub fn into_cause(self) -> E {
        self.cause
    }
}
impl<E> fmt::Debug for CodecFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodecFailure")
            .field("operation", &self.operation)
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}
impl<E> fmt::Display for CodecFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Snapshot {:?} failed at {}.",
            self.operation, self.location
        )
    }
}
impl<E> std::error::Error for CodecFailure<E> {}

impl<E, D> Codec<E, D> {
    /// Selects conversion operations without invoking either one.
    pub const fn new(encode: E, decode: D) -> Self {
        Self { encode, decode }
    }
    /// Encodes using only the supplied context and format.
    ///
    /// # Errors
    /// Retains the caller operation's typed encoding cause.
    ///
    /// # Panics
    /// Propagates encoder panics without restoring context or format.
    #[track_caller]
    pub fn encode<V: ?Sized, C, F, X>(
        &mut self,
        value: &V,
        context: &mut CodecContext<C, F>,
    ) -> Result<Vec<u8>, CodecFailure<X>>
    where
        E: FnMut(&V, &mut CodecContext<C, F>) -> Result<Vec<u8>, X>,
    {
        let location = Location::caller();
        (self.encode)(value, context).map_err(|cause| CodecFailure {
            cause,
            operation: CodecOperation::Encode,
            location,
        })
    }
    /// Decodes while retaining detected format, including after typed failure.
    ///
    /// # Errors
    /// Retains parse or typed-decode causes exactly as supplied by the decoder.
    ///
    /// # Panics
    /// Propagates decoder panics without restoring context or format.
    #[track_caller]
    pub fn decode<V, C, F, X>(
        &mut self,
        bytes: &[u8],
        context: &mut CodecContext<C, F>,
    ) -> Result<V, CodecFailure<X>>
    where
        D: FnMut(&[u8], &mut CodecContext<C, F>) -> Result<V, X>,
    {
        let location = Location::caller();
        (self.decode)(bytes, context).map_err(|cause| CodecFailure {
            cause,
            operation: CodecOperation::Decode,
            location,
        })
    }
    /// Decodes with explicit format-only rollback policy.
    ///
    /// Context mutations and external callback effects are never rolled back.
    /// Format cloning must be independent to provide historical restoration.
    /// Rollback applies to returned errors, not panics.
    ///
    /// # Errors
    /// Preserves the decoder cause; restores format on error when selected.
    ///
    /// # Panics
    /// Propagates panics from the decoder, format cloning or format destruction.
    #[track_caller]
    pub fn decode_with<V, C, F: Clone, X>(
        &mut self,
        bytes: &[u8],
        context: &mut CodecContext<C, F>,
        rollback: FormatRollback,
    ) -> Result<V, CodecFailure<X>>
    where
        D: FnMut(&[u8], &mut CodecContext<C, F>) -> Result<V, X>,
    {
        let previous = (rollback == FormatRollback::OnError).then(|| context.format.clone());
        let result = self.decode(bytes, context);
        if result.is_err() {
            if let Some(previous) = previous {
                context.format = previous;
            }
        } else {
            // Keep the decoded value owned while the unused format copy drops.
            drop(previous);
        }
        result
    }
}

/// Selects local recording policy independently of ambient environment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordMode {
    /// Never writes references or actual artefacts, even with a writer supplied.
    CompareOnly,
    /// Records missing references only; existing mismatches still fail.
    Missing,
    /// Records missing or mismatching references, leaving matches untouched.
    Failed,
    /// Records every reference and always requires update acknowledgement.
    All,
}

/// Identifies an explicitly authorised artefact destination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactKind {
    /// The recorded reference, requiring a compare-and-write operation.
    Reference,
    /// The redacted actual value accompanying an existing mismatch.
    Actual,
}

/// Represents two validated logical path components, never an ambient filesystem root.
#[derive(Debug, Eq, PartialEq)]
pub struct SnapshotPath(String);
impl SnapshotPath {
    /// Returns the validated logical namespace/name key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Supplies explicit authority for redacted reference and artefact writes.
///
/// Implementations map logical keys to isolated destinations, distinguish kinds,
/// prevent symlink/root escapes and atomically compare reference contents with
/// `expected` before replacing them. Conflicts must fail, never overwrite user
/// edits. This contract also applies across separately constructed namespaces.
/// No implementation or filesystem capability is supplied implicitly.
pub trait ReferenceWriter {
    /// The original permission, conflict or I/O failure type.
    type Error;
    /// Writes redacted bytes under the explicitly selected logical destination.
    ///
    /// `expected` is the exact previously read reference, not an emitted artefact.
    /// It is `None` for actual artefacts or an absent reference.
    ///
    /// # Errors
    /// Returns permission, conflict or I/O failure without erasing its type.
    fn write(
        &mut self,
        path: &SnapshotPath,
        kind: ArtifactKind,
        expected: Option<&[u8]>,
        bytes: &[u8],
    ) -> Result<(), Self::Error>;
}

/// Borrows the selected reference and actual bytes for one comparison.
#[derive(Clone, Copy)]
pub struct SnapshotRequest<'a> {
    /// A portable lowercase ASCII component, unique within the namespace owner.
    pub name: &'a str,
    /// Previously read exact bytes; `None` differs from an empty reference.
    pub reference: Option<&'a [u8]>,
    /// The exact selected capture or codec output.
    pub actual: &'a [u8],
    /// Explicit recording policy.
    pub mode: RecordMode,
}
impl fmt::Debug for SnapshotRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotRequest")
            .field("mode", &self.mode)
            .field("has_reference", &self.reference.is_some())
            .finish_non_exhaustive()
    }
}

/// Identifies a mandatory comparison, authority or typed callback failure.
pub enum SnapshotFailureKind<R, W> {
    /// Namespace or case name was not a portable component.
    InvalidName,
    /// This namespace owner already reserved the case name.
    Collision,
    /// No reference was supplied in compare-only mode.
    Missing,
    /// Exact selected bytes differed.
    Mismatch,
    /// Recording requires explicit writer authority.
    WriterRequired,
    /// Recording succeeded; review and a verifying rerun are still required.
    UpdateRequired,
    /// Redaction failed before any write.
    Redaction(R),
    /// The authorised writer failed.
    Write(W),
}
impl<R, W> fmt::Debug for SnapshotFailureKind<R, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidName => "InvalidName",
            Self::Collision => "Collision",
            Self::Missing => "Missing",
            Self::Mismatch => "Mismatch",
            Self::WriterRequired => "WriterRequired",
            Self::UpdateRequired => "UpdateRequired",
            Self::Redaction(_) => "Redaction(..)",
            Self::Write(_) => "Write(..)",
        })
    }
}

/// Retains failure kind, logical path and call site without rendering private bytes.
pub struct SnapshotFailure<R, W> {
    /// Keeps callback causes typed and owned rather than rendering their payloads.
    kind: SnapshotFailureKind<R, W>,
    /// Exists only when both logical components have passed validation.
    path: Option<SnapshotPath>,
    /// Retains the original comparison or recording call site.
    location: &'static Location<'static>,
}
impl<R, W> SnapshotFailure<R, W> {
    /// Returns the mandatory outcome or typed callback cause.
    pub const fn kind(&self) -> &SnapshotFailureKind<R, W> {
        &self.kind
    }
    /// Recovers the outcome or typed callback cause by ownership.
    pub fn into_kind(self) -> SnapshotFailureKind<R, W> {
        self.kind
    }
    /// Returns the logical path, absent when the namespace or case name is invalid.
    pub const fn path(&self) -> Option<&SnapshotPath> {
        self.path.as_ref()
    }
    /// Returns the snapshot operation's caller location.
    pub const fn location(&self) -> &'static Location<'static> {
        self.location
    }
}
impl<R, W> fmt::Debug for SnapshotFailure<R, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotFailure")
            .field("kind", &self.kind)
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}
impl<R, W> fmt::Display for SnapshotFailure<R, W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Snapshot {:?} at {}.", self.kind, self.location)
    }
}
impl<R, W> std::error::Error for SnapshotFailure<R, W> {}

/// Owns a local collision domain without any global or thread-local registry.
#[derive(Debug)]
pub struct SnapshotNamespace {
    /// Validated once at construction; never interpreted as an absolute path.
    name: String,
    /// Retains valid names after failures to prevent reuse within this owner.
    reserved: BTreeSet<String>,
}
impl SnapshotNamespace {
    /// Creates an explicit namespace without performing any I/O.
    ///
    /// Components contain lowercase ASCII letters, digits, hyphens or underscores;
    /// portable device names are rejected. Failed operations retain reservations.
    ///
    /// # Errors
    /// Returns `InvalidName` for empty, unsafe or non-portable names.
    #[track_caller]
    pub fn new(name: &str) -> Result<Self, SnapshotFailure<(), ()>> {
        if !valid_component(name) {
            return Err(SnapshotFailure {
                kind: SnapshotFailureKind::InvalidName,
                path: None,
                location: Location::caller(),
            });
        }
        Ok(Self {
            name: name.to_owned(),
            reserved: BTreeSet::new(),
        })
    }

    /// Compares exact bytes without redaction or writer authority.
    ///
    /// Reserves valid names, even when the reference is missing or mismatches.
    /// `None` differs from an empty reference; no bytes are normalised or recorded.
    ///
    /// # Examples
    ///
    /// ```
    /// use rca_test::snapshots::SnapshotNamespace;
    ///
    /// let mut namespace = SnapshotNamespace::new("suite")?;
    /// namespace.compare("case", Some(b"exact\n"), b"exact\n")?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// Returns invalid or duplicate names, missing references or unequal bytes.
    #[track_caller]
    pub fn compare(
        &mut self,
        name: &str,
        reference: Option<&[u8]>,
        actual: &[u8],
    ) -> Result<(), SnapshotFailure<(), ()>> {
        let location = Location::caller();
        self.reserve_name(name, location)?;
        if reference == Some(actual) {
            return Ok(());
        }
        let path = SnapshotPath(format!("{}/{}", self.name, name));
        Err(SnapshotFailure {
            kind: if reference.is_none() {
                SnapshotFailureKind::Missing
            } else {
                SnapshotFailureKind::Mismatch
            },
            path: Some(path),
            location,
        })
    }

    /// Compares exact bytes and performs only explicitly authorised redacted writes.
    ///
    /// Reserves the name before any callback, then compares without rendering.
    /// All successful recordings return `UpdateRequired`; callers must review and
    /// rerun in comparison mode. Existing mismatches may emit an actual artefact
    /// in recording modes only. Compare-only does not invoke redaction or writer.
    /// Writer failures can follow an already successful reference write; this
    /// function does not promise multi-artefact transactions or rollback.
    ///
    /// # Errors
    /// Returns invalid/colliding names, missing/mismatching references, absent
    /// writer authority, typed redaction/write errors or update acknowledgement.
    ///
    /// # Panics
    /// Propagates redaction, writer and callback cleanup panics.
    /// The valid case name remains reserved.
    /// Successful writes are not rolled back.
    #[track_caller]
    pub fn check<R, W: ReferenceWriter>(
        &mut self,
        request: SnapshotRequest<'_>,
        redact: impl FnOnce(&[u8]) -> Result<Vec<u8>, R>,
        writer: Option<&mut W>,
    ) -> Result<(), SnapshotFailure<R, W::Error>> {
        let location = Location::caller();
        let reservation = self.reserve_name(request.name, location);
        if reservation.is_err() {
            // The failure owns its path while the unused callback is destroyed.
            drop(redact);
            return reservation;
        }
        let matches = request.reference == Some(request.actual);
        let record = match request.mode {
            RecordMode::CompareOnly => false,
            RecordMode::Missing => request.reference.is_none(),
            RecordMode::Failed => !matches,
            RecordMode::All => true,
        };
        if matches && !record {
            return Ok(());
        }
        let path = SnapshotPath(format!("{}/{}", self.name, request.name));
        let fail = |kind, path| SnapshotFailure {
            kind,
            path: Some(path),
            location,
        };
        let actual_artifact = request.reference.is_some() && !matches;
        if request.mode == RecordMode::CompareOnly {
            let failure = fail(
                if request.reference.is_none() {
                    SnapshotFailureKind::Missing
                } else {
                    SnapshotFailureKind::Mismatch
                },
                path,
            );
            // Retain the owned failure while unused authority is destroyed.
            drop(redact);
            return Err(failure);
        }
        let Some(writer) = writer else {
            let failure = fail(SnapshotFailureKind::WriterRequired, path);
            drop(redact);
            return Err(failure);
        };
        let bytes = match redact(request.actual) {
            Ok(bytes) => bytes,
            Err(cause) => return Err(fail(SnapshotFailureKind::Redaction(cause), path)),
        };
        if record
            && let Err(cause) =
                writer.write(&path, ArtifactKind::Reference, request.reference, &bytes)
        {
            return Err(fail(SnapshotFailureKind::Write(cause), path));
        }
        if actual_artifact
            && let Err(cause) = writer.write(&path, ArtifactKind::Actual, None, &bytes)
        {
            return Err(fail(SnapshotFailureKind::Write(cause), path));
        }
        Err(fail(
            if record {
                SnapshotFailureKind::UpdateRequired
            } else {
                SnapshotFailureKind::Mismatch
            },
            path,
        ))
    }

    /// Claims a valid name before callbacks can write or observe external state.
    /// Invalid names are not retained; later failures do not release valid names.
    fn reserve_name<R, W>(
        &mut self,
        name: &str,
        location: &'static Location<'static>,
    ) -> Result<(), SnapshotFailure<R, W>> {
        if !valid_component(name) {
            return Err(SnapshotFailure {
                kind: SnapshotFailureKind::InvalidName,
                path: None,
                location,
            });
        }
        if !self.reserved.insert(name.to_owned()) {
            let path = SnapshotPath(format!("{}/{}", self.name, name));
            return Err(SnapshotFailure {
                kind: SnapshotFailureKind::Collision,
                path: Some(path),
                location,
            });
        }
        Ok(())
    }
}

/// Restricts logical keys to portable components, including Windows device rules.
/// This does not replace a writer's destination, symlink or conflict checks.
fn valid_component(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
        && !matches!(name, "con" | "prn" | "aux" | "nul")
        && !(name.len() == 4
            && (name.starts_with("com") || name.starts_with("lpt"))
            && matches!(name.as_bytes()[3], b'1'..=b'9'))
}

/// Identifies timeout or one finite case's typed capture error.
pub struct CaptureFailure<E> {
    /// `None` represents timeout without manufacturing a caller error value.
    capture: Option<(usize, E)>,
    /// Records construction rather than a later executor's polling frame.
    location: &'static Location<'static>,
}
impl<E> CaptureFailure<E> {
    /// Returns whether the supplied deadline won.
    #[must_use]
    pub const fn is_timeout(&self) -> bool {
        self.capture.is_none()
    }
    /// Returns the input position and original typed capture cause, if any.
    pub fn cause(&self) -> Option<(usize, &E)> {
        self.capture.as_ref().map(|(index, cause)| (*index, cause))
    }
    /// Recovers the input position and original typed cause by ownership.
    pub fn into_cause(self) -> Option<(usize, E)> {
        self.capture
    }
    /// Returns the construction call site, not the later executor poll site.
    pub const fn location(&self) -> &'static Location<'static> {
        self.location
    }
}
impl<E> fmt::Debug for CaptureFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureFailure")
            .field("index", &self.capture.as_ref().map(|(index, _)| index))
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}
impl<E> fmt::Display for CaptureFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Snapshot capture failed: {self:?}.")
    }
}
impl<E> std::error::Error for CaptureFailure<E> {}

/// Captures finite cases lazily and invokes finish once, in input order.
///
/// No factory runs until first poll and a pending deadline. The deadline is
/// polled first on every turn, winning ready ties. All unfinished cases are
/// polled in input order; completion order does not change result association.
/// Timeout, capture error or drop destroys owned work and the finish closure,
/// so RCA cannot later compare or write. External work initiated by a caller
/// must implement its own cancellation; this does not revoke arbitrary external
/// callbacks. Keep all comparison/writer authority inside `finish`.
///
/// # Examples
///
/// ```
/// use std::future::{pending, ready};
///
/// use futures_executor::block_on;
/// use rca_test::snapshots::capture_ordered;
///
/// let cases = vec![|| ready(Ok::<_, ()>(7))];
/// let capture = capture_ordered(cases, pending::<()>(), |values| {
///     assert_eq!(values, [7]);
/// });
/// block_on(capture).unwrap();
/// ```
///
/// # Errors
/// Returns timeout or the original indexed capture cause without calling finish.
///
/// # Panics
/// Propagates caller factory, capture, deadline, finish and cleanup panics.
#[track_caller]
pub fn capture_ordered<C, F, T, V, E, R>(
    cases: Vec<C>,
    timeout: T,
    finish: impl FnOnce(Vec<V>) -> R,
) -> impl Future<Output = Result<R, CaptureFailure<E>>>
where
    C: FnOnce() -> F,
    F: Future<Output = Result<V, E>>,
    T: Future<Output = ()>,
{
    let location = Location::caller();
    async move {
        // Field order keeps deadline, factory, future and value cleanup ordered on cancellation.
        let mut owners = (Box::pin(timeout), Some(cases), Vec::new(), Vec::new());
        let result = poll_fn(|cx| {
            let (timeout, cases, pending, values) = &mut owners;
            if timeout.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err(CaptureFailure {
                    capture: None,
                    location,
                }));
            }
            if let Some(cases) = cases.take() {
                for capture in cases {
                    pending.push(Some(Box::pin(capture())));
                    values.push(None);
                }
            }
            let mut complete = true;
            for (index, future) in pending.iter_mut().enumerate() {
                if let Some(capture) = future {
                    match capture.as_mut().poll(cx) {
                        Poll::Pending => complete = false,
                        Poll::Ready(Ok(value)) => {
                            values[index] = Some(value);
                            *future = None;
                        }
                        Poll::Ready(Err(cause)) => {
                            return Poll::Ready(Err(CaptureFailure {
                                capture: Some((index, cause)),
                                location,
                            }));
                        }
                    }
                }
            }
            if complete {
                Poll::Ready(Ok(values.iter_mut().filter_map(Option::take).collect()))
            } else {
                Poll::Pending
            }
        })
        .await;
        // Keep completed values or the typed failure owned through cleanup.
        drop(owners);
        match result {
            Ok(values) => Ok(finish(values)),
            Err(error) => {
                drop(finish);
                Err(error)
            }
        }
    }
}
