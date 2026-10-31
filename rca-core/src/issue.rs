//! Reports structured issues through explicit endpoints and reporter selections.
//!
//! Issues are recoverable observations, not replacements for typed domain errors
//! or invariant panics. Reporting is disabled unless a handler or test recorder
//! is installed. Enabled endpoints allocate shared storage and box the handler;
//! non-zero-sized handler captures allocate. Test recording additionally allocates
//! queue storage and clones owned messages on admission and inspection.
//!
//! Graceful shutdown admits actor-owned destructor reports before sealing.
//! Abortive closure does not guarantee successful reporting from effects destroyed
//! later by the runtime or from future external work: these receive `Closed`.

use std::borrow::Cow;
#[cfg(feature = "test-util")]
use std::collections::VecDeque;
use std::fmt;
use std::panic::Location;
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(feature = "test-util")]
mod collector;
mod operation;
mod reporters;
#[cfg(feature = "test-util")]
mod stub;

#[cfg(feature = "test-util")]
#[doc(inline)]
pub use collector::IssueCollector;

#[cfg(feature = "test-util")]
#[doc(inline)]
pub use stub::EndpointStub;

#[doc(inline)]
pub use reporters::IssueReporters;

#[doc(inline)]
pub use operation::{
    IssueCallback, IssueContext, IssueDelivery, IssueDeliveryFailure, OperationReport,
};

/// Classifies a diagnostic without asserting operation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IssueSeverity {
    /// Informational diagnostic evidence.
    Note,
    /// A recoverable condition that deserves attention.
    Warning,
    /// An error diagnostic, independent of the operation's typed result.
    Error,
}

/// Describes a recoverable observation with its original source location.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Issue {
    /// Stable application-defined issue identifier.
    pub code: &'static str,
    /// Human-readable issue details.
    pub message: Cow<'static, str>,
    /// Diagnostic importance, independent of assertion or delivery policy.
    pub severity: IssueSeverity,
    /// Optional caller-supplied, owned representation of the original cause.
    pub cause: Option<Cow<'static, str>>,
    /// Original issue construction site.
    pub location: &'static Location<'static>,
}

impl Issue {
    /// Captures a warning diagnostic at the caller's source location.
    ///
    /// Starts without a cause representation. Severity does not assert an
    /// operation failed or change the endpoint's diagnostic admission policy.
    #[track_caller]
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            code,
            message: message.into(),
            severity: IssueSeverity::Warning,
            cause: None,
            location: Location::caller(),
        }
    }

    /// Sets diagnostic severity without changing its construction location.
    #[must_use]
    pub const fn with_severity(mut self, severity: IssueSeverity) -> Self {
        self.severity = severity;
        self
    }

    /// Attaches caller-supplied text describing a cause, not an owned error value.
    #[must_use]
    pub fn with_cause(mut self, cause: impl Into<Cow<'static, str>>) -> Self {
        self.cause = Some(cause.into());
        self
    }
}

/// Preserves an issue rejected by a disabled or closed endpoint.
#[derive(Debug)]
pub struct IssueReportError {
    /// Original rejected payload, available for recovery without reconstruction.
    issue: Issue,
    /// Distinguishes a sealed endpoint from reporting that was never enabled.
    closed: bool,
}

impl IssueReportError {
    /// Returns whether reporting was never enabled.
    #[must_use]
    pub const fn is_disabled(&self) -> bool {
        !self.closed
    }

    /// Returns whether the owning Store has sealed reporting.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Returns the original rejected issue without changing its source location.
    #[must_use]
    pub fn into_issue(self) -> Issue {
        self.issue
    }
}

impl fmt::Display for IssueReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.closed {
            "issue reporter is closed"
        } else {
            "issue reporting is disabled"
        })
    }
}

impl std::error::Error for IssueReportError {}

/// Handler shared by an endpoint; calls run outside the admission lock.
pub(crate) type IssueHandler = Box<dyn Fn(&Issue) + Send + Sync>;

/// Shared admission state and a handler installed once before Store execution.
struct Endpoint {
    /// Serialises queue insertion with closure, never user handler execution.
    state: Mutex<Admission>,
    /// Immutable after installation, allowing concurrent and re-entrant calls.
    handler: OnceLock<IssueHandler>,
}

/// State whose queue and closure flag must be observed under the same lock.
struct Admission {
    /// Monotonic flag rejecting all reports admitted after closure.
    closed: bool,
    /// Optional FIFO evidence, inserted before an admitted handler is invoked.
    #[cfg(feature = "test-util")]
    recorded: Option<VecDeque<Issue>>,
}

/// Shares an issue endpoint without retaining Store or ingress authority.
///
/// The default reporter is disabled and allocates nothing, including on clone.
/// An installed handler retains its own captures independently of Store ownership.
#[derive(Clone, Default)]
pub struct IssueReporter {
    /// Shared endpoint only; `None` is the allocation-free disabled state.
    endpoint: Option<Arc<Endpoint>>,
}

impl IssueReporter {
    /// Creates a standalone endpoint that invokes an explicitly supplied handler.
    ///
    /// Allocates shared endpoint storage and boxes the handler; non-zero-sized
    /// captures allocate. This endpoint has no owning Store to seal it. Its clones
    /// share the handler, whose invocations may overlap or reenter. Constructing it
    /// does not install it in any ambient context or change a Store's reporter.
    #[must_use]
    pub fn new(handler: impl Fn(&Issue) + Send + Sync + 'static) -> Self {
        Self::enabled(Box::new(handler))
    }

    /// Reports endpoint presence, including endpoints already sealed.
    #[cfg(feature = "test-util")]
    pub(crate) const fn endpoint_enabled(&self) -> bool {
        self.endpoint.is_some()
    }

    /// Creates a non-recording endpoint with its sole handler already installed.
    pub(crate) fn enabled(handler: IssueHandler) -> Self {
        let reporter = Self::new_endpoint(false);
        reporter.set_handler(handler);
        reporter
    }

    /// Allocates an open endpoint, optionally retaining FIFO test evidence.
    fn new_endpoint(recording: bool) -> Self {
        #[cfg(not(feature = "test-util"))]
        let _ = recording;
        Self {
            endpoint: Some(Arc::new(Endpoint {
                state: Mutex::new(Admission {
                    closed: false,
                    #[cfg(feature = "test-util")]
                    recorded: recording.then(VecDeque::new),
                }),
                handler: OnceLock::new(),
            })),
        }
    }

    /// Installs the endpoint's sole handler, panicking on a second installation.
    pub(crate) fn set_handler(&self, handler: IssueHandler) {
        assert!(
            self.endpoint
                .as_ref()
                .expect("enabled endpoint")
                .handler
                .set(handler)
                .is_ok(),
            "issue recorder must belong to only one Store"
        );
    }

    /// Admits an issue, then invokes the configured handler outside the endpoint lock.
    ///
    /// Recorded issues enter the queue before handler execution. Closure does not
    /// wait for admitted handlers. Handler invocations can overlap and reenter.
    ///
    /// # Errors
    /// Returns the original issue when reporting is disabled or closed.
    ///
    /// # Panics
    /// Propagates a handler panic. An admitted recorded issue remains available.
    pub fn report(&self, issue: Issue) -> Result<(), IssueReportError> {
        let Some(endpoint) = &self.endpoint else {
            return Err(IssueReportError {
                issue,
                closed: false,
            });
        };
        {
            let mut state = endpoint
                .state
                .lock()
                .expect("issue admission lock must not be poisoned");
            if state.closed {
                return Err(IssueReportError {
                    issue,
                    closed: true,
                });
            }
            #[cfg(feature = "test-util")]
            if let Some(recorded) = &mut state.recorded {
                recorded.push_back(issue.clone());
            }
            #[cfg(not(feature = "test-util"))]
            let _ = &mut state;
        }
        if let Some(handler) = endpoint.handler.get() {
            handler(&issue);
        }
        Ok(())
    }

    /// Seals admission without waiting for handlers whose issues were admitted.
    pub(crate) fn close(&self) {
        if let Some(endpoint) = &self.endpoint {
            endpoint
                .state
                .lock()
                .expect("issue admission lock must not be poisoned")
                .closed = true;
        }
    }
}

impl fmt::Debug for IssueReporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssueReporter")
            .field("enabled", &self.endpoint.is_some())
            .finish_non_exhaustive()
    }
}

/// Records issues for the deterministic TestStore bridge.
///
/// Clones share one queue and admission state. Closing any Store attached to that
/// endpoint seals reporting for every clone. Use fresh recorders for independent
/// Store lifetimes; the TestStore bridge does this for each Store it creates.
/// An endpoint accepts only one handler installation, even when shared by Stores.
#[cfg(feature = "test-util")]
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct IssueRecorder {
    /// Enabled endpoint with a recording queue shared by recorder clones.
    reporter: IssueReporter,
}

#[cfg(feature = "test-util")]
impl IssueRecorder {
    /// Transfers endpoint ownership into a Store without another reference count.
    pub(crate) fn into_reporter(self) -> IssueReporter {
        self.reporter
    }

    /// Creates an initially empty Store-local recorder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            reporter: IssueReporter::new_endpoint(true),
        }
    }

    /// Returns this recorder's endpoint for Store construction.
    #[must_use]
    pub fn reporter(&self) -> IssueReporter {
        self.reporter.clone()
    }

    /// Clones the oldest issue without consuming it.
    ///
    /// # Panics
    /// Panics if internal recorder invariants or mutex integrity are violated.
    #[must_use]
    pub fn oldest(&self) -> Option<Issue> {
        self.reporter
            .endpoint
            .as_ref()
            .expect("recorder endpoint")
            .state
            .lock()
            .expect("issue admission lock must not be poisoned")
            .recorded
            .as_ref()
            .expect("recorder queue")
            .front()
            .cloned()
    }

    /// Removes the oldest issue after a successful outside-lock match.
    ///
    /// # Panics
    /// Panics if internal recorder invariants or mutex integrity are violated.
    #[must_use]
    pub fn consume_oldest(&mut self) -> Option<Issue> {
        self.reporter
            .endpoint
            .as_ref()
            .expect("recorder endpoint")
            .state
            .lock()
            .expect("issue admission lock must not be poisoned")
            .recorded
            .as_mut()
            .expect("recorder queue")
            .pop_front()
    }

    /// Captures current unconsumed issues for failure diagnostics.
    ///
    /// # Panics
    /// Panics if internal recorder invariants or mutex integrity are violated.
    #[must_use]
    pub fn snapshot(&self) -> Vec<Issue> {
        self.reporter
            .endpoint
            .as_ref()
            .expect("recorder endpoint")
            .state
            .lock()
            .expect("issue admission lock must not be poisoned")
            .recorded
            .as_ref()
            .expect("recorder queue")
            .iter()
            .cloned()
            .collect()
    }
}

#[cfg(feature = "test-util")]
impl Default for IssueRecorder {
    fn default() -> Self {
        Self::new()
    }
}

/// Seals an actor's endpoint after the fields preceding this guard are destroyed.
pub(crate) struct IssueCloseGuard(
    /// Endpoint authority only; retaining it does not retain the actor.
    pub(crate) IssueReporter,
);

impl Drop for IssueCloseGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

pin_project_lite::pin_project! {
    /// Seals reporting on completion, or after actor destruction if dropped early.
    ///
    /// Fields drop in declaration order. The pinned actor owns the whole Actor
    /// even before its first poll, so its fields and suspended borrowed futures
    /// finish destruction before the endpoint is sealed by the close guard.
    pub(crate) struct IssueLifetime<F> {
        // Actor-owned values may still report issues during their destruction.
        #[pin]
        pub(crate) actor: F,
        // Also closes after an unpolled or panicking actor is dropped.
        pub(crate) close: IssueCloseGuard,
    }
}

impl<F: Future<Output = ()>> Future for IssueLifetime<F> {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let this = self.project();
        let result = this.actor.poll(cx);
        if result.is_ready() {
            this.close.0.close();
        }
        result
    }
}
