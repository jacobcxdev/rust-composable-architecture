//! Owns isolated issue capture and the authority to close its admission queue.

use std::collections::VecDeque;

use super::{Issue, IssueRecorder, IssueReporter};

/// Collects isolated issues under one owned closing authority.
///
/// Available only with `test-util`. A fresh recorder supplies the existing
/// admission mutex and FIFO queue; no parallel endpoint or queue is introduced.
/// Reporter clones retain delivery capability, not authority to close collection.
/// Dropping an unfinished collector seals it without asserting an expectation.
/// Its abandoned queue and owned issues are released outside the admission lock,
/// even when reporting handles remain alive.
///
/// ```
/// use rca_core::{Issue, IssueCollector};
/// let collector = IssueCollector::new();
/// let reporter = collector.reporter();
/// reporter.report(Issue::new("known", "captured")).unwrap();
/// let issues = collector.finish();
/// assert_eq!(issues[0].code, "known");
/// assert!(reporter.report(Issue::new("late", "recovered"))
///     .unwrap_err().is_closed());
/// ```
#[derive(Debug)]
pub struct IssueCollector {
    /// Retains closing authority until `finish` takes it or Drop seals admission.
    recorder: Option<IssueRecorder>,
}

impl IssueCollector {
    /// Creates a fresh isolated collector.
    ///
    /// Allocates the shared recorder endpoint. Admission allocates queue storage
    /// and clones owned payloads under the existing recorder policy.
    #[must_use]
    pub fn new() -> Self {
        Self {
            recorder: Some(IssueRecorder::new()),
        }
    }

    /// Returns a reporting handle without closing authority.
    ///
    /// The handle can be passed directly or included in an explicit
    /// [`super::IssueReporters`] selection. It does not install an ambient reporter.
    ///
    /// # Panics
    /// Panics if internal collector ownership invariants are violated.
    #[must_use]
    pub fn reporter(&self) -> IssueReporter {
        self.recorder.as_ref().expect("active collector").reporter()
    }

    /// Seals admission and takes all captured issues in FIFO order.
    ///
    /// Closing and taking the queue happen under the same admission mutex. Reports
    /// racing this boundary either appear in the returned queue or recover a closed
    /// error. Conversion to the returned vector happens outside the lock and can
    /// allocate; it moves queued issues without cloning their payloads.
    /// This consumes the sole close authority, so subsequent Drop does not reseal.
    ///
    /// # Panics
    /// Panics if internal recorder invariants or mutex integrity are violated.
    #[must_use]
    pub fn finish(mut self) -> Vec<Issue> {
        let recorder = self.recorder.take().expect("active collector");
        close_and_take(recorder).into_iter().collect()
    }
}

/// Seals the collector's endpoint and detaches its queue before releasing the lock.
fn close_and_take(recorder: IssueRecorder) -> VecDeque<Issue> {
    let reporter = recorder.into_reporter();
    let mut admission = reporter
        .endpoint
        .as_ref()
        .expect("collector endpoint")
        .state
        .lock()
        .expect("issue admission lock must not be poisoned");
    admission.closed = true;
    admission.recorded.take().expect("collector queue")
}

impl Default for IssueCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for IssueCollector {
    fn drop(&mut self) {
        if let Some(recorder) = self.recorder.take() {
            drop(close_and_take(recorder));
        }
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use super::{Issue, IssueCollector};

    #[test]
    fn dropped_collector_releases_recording_storage_from_retained_reporters() {
        let collector = IssueCollector::new();
        let reporter = collector.reporter();
        reporter
            .report(Issue::new("abandoned", String::from("owned diagnostic")))
            .unwrap();

        drop(collector);

        let admission = reporter.endpoint.as_ref().unwrap().state.lock().unwrap();
        assert!(admission.closed);
        assert!(
            admission
                .recorded
                .as_ref()
                .is_none_or(|queue| queue.capacity() == 0),
            "closed reporter retained the abandoned queue and its owned issues"
        );
    }
}
}
