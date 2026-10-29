//! Direct, synchronous issue-reporting profiling inputs.
//!
//! Owned payloads are built before measurement of the first and remaining reports.
//! Callback and recovery byte scans stay inside those phases. Independent expected
//! streams are checked afterwards; all owners drop before the final allocation snapshot.

use std::borrow::Cow;
use std::panic::Location;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use rca_core::{
    Issue, IssueCallback, IssueContext, IssueDelivery, IssueDeliveryFailure, IssueReporter,
    IssueReporters, IssueSeverity,
};

use crate::measurement::Measurement;

const PAYLOAD: usize = 4096;
// Fixed endpoint accounting capacity for the admitted finite fan-out widths.
const MAX_ENDPOINTS: usize = 64;
const ORDER_SEED: u64 = 0xcbf2_9ce4_8422_2325;
const ORDER_PRIME: u64 = 0x100_0000_01b3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Direct,
    Owned,
    OwnedDisabled,
    Borrowed,
    BorrowedFailed,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Config {
    mode: Mode,
    name: &'static str,
    size: usize,
    iterations: usize,
}

impl Config {
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let name = args.next().ok_or("missing issue workload")?;
        let (mode, name) = match name.as_str() {
            "issue-direct" => (Mode::Direct, "issue-direct"),
            "issue-owned" => (Mode::Owned, "issue-owned"),
            "issue-owned-disabled" => (Mode::OwnedDisabled, "issue-owned-disabled"),
            "issue-borrowed" => (Mode::Borrowed, "issue-borrowed"),
            "issue-borrowed-failed" => (Mode::BorrowedFailed, "issue-borrowed-failed"),
            _ => return Err("unknown issue workload"),
        };
        let size_text = args.next().ok_or("missing issue size")?;
        let iterations_text = args.next().ok_or("missing issue iterations")?;
        let size = parse_positive(&size_text)?;
        let iterations = parse_positive(&iterations_text)?;
        if args.next().is_some() {
            return Err("extra issue arguments");
        }
        if !(1..=1000).contains(&iterations) {
            return Err("issue iterations outside 1..=1000");
        }
        if (mode == Mode::Direct && size != 1)
            || (mode != Mode::Direct && !matches!(size, 0 | 1 | 16 | 64))
        {
            return Err("unsupported issue size");
        }
        Ok(Self {
            mode,
            name,
            size,
            iterations,
        })
    }
}

fn parse_positive(value: &str) -> Result<usize, &'static str> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("invalid issue dimension");
    }
    value.parse().map_err(|_| "issue dimension overflow")
}

/// Records ordered callback or recovery facts without retaining delivered issues.
#[derive(Clone, Copy, Debug)]
struct Stream {
    count: u64,
    bytes: u64,
    order: u64,
    per_endpoint: [u64; MAX_ENDPOINTS],
}

impl Default for Stream {
    fn default() -> Self {
        Self {
            count: 0,
            bytes: 0,
            order: ORDER_SEED,
            per_endpoint: [0; MAX_ENDPOINTS],
        }
    }
}

impl Stream {
    fn record(
        &mut self,
        ordinal: usize,
        endpoint: usize,
        digest: u64,
        size: usize,
    ) -> Result<(), &'static str> {
        let next = usize::try_from(self.count).map_err(|_| "issue count overflow")?;
        if ordinal != next / size + 1 || endpoint != next % size {
            return Err("issue stream endpoint order mismatch");
        }
        self.count = self.count.checked_add(1).ok_or("issue count overflow")?;
        self.bytes = self
            .bytes
            .checked_add(digest)
            .ok_or("issue digest overflow")?;
        self.per_endpoint[endpoint] += 1;
        for word in [ordinal as u64, endpoint as u64] {
            self.order = (self.order ^ word).wrapping_mul(ORDER_PRIME);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Facts {
    callback: Stream,
    recovery: Stream,
    accepted: u64,
    disabled: u64,
    failed: u64,
    error: Option<&'static str>,
}

/// Makes callback retention visible through identity, weak references and final drops.
struct EndpointOwner {
    index: usize,
    drops: Arc<AtomicUsize>,
}

impl Drop for EndpointOwner {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct FaultCounters {
    created: AtomicUsize,
    dropped: AtomicUsize,
}

/// Carries the original endpoint owner through a typed rejected delivery.
struct DeliveryFault {
    endpoint: usize,
    owner: Arc<EndpointOwner>,
    counters: Arc<FaultCounters>,
}

impl Drop for DeliveryFault {
    fn drop(&mut self) {
        self.counters.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

struct Cause {
    tag: u8,
}

/// Prebuilds both owned payloads and records the actual construction call site.
fn make_issue(ordinal: usize, payload: usize) -> (Issue, &'static Location<'static>) {
    let message: String = (0..payload)
        .map(|j| char::from(b'a' + u8::try_from((ordinal + j) % 26).expect("modulo 26")))
        .collect();
    let cause: String = (0..payload)
        .map(|j| char::from(b'A' + u8::try_from((ordinal + 3 * j) % 26).expect("modulo 26")))
        .collect();
    let line = line!() + 1;
    let issue = Issue::new("profile.issue", message)
        .with_severity(IssueSeverity::Error)
        .with_cause(cause);
    assert_eq!(issue.location.file(), file!());
    assert_eq!(issue.location.line(), line);
    let location = issue.location;
    (issue, location)
}

/// Checks every delivered byte and metadata field before recording its digest.
#[inline(never)]
fn observe_issue(
    issue: &Issue,
    ordinal: usize,
    payload: usize,
    location: &Location<'_>,
) -> Result<u64, &'static str> {
    if issue.code != "profile.issue"
        || issue.severity != IssueSeverity::Error
        || issue.location.file() != location.file()
        || issue.location.line() != location.line()
        || issue.location.column() != location.column()
        || !matches!(&issue.message, Cow::Owned(_))
        || !matches!(&issue.cause, Some(Cow::Owned(_)))
    {
        return Err("issue metadata mismatch");
    }
    let message = issue.message.as_bytes();
    let cause = issue
        .cause
        .as_ref()
        .ok_or("missing issue cause")?
        .as_bytes();
    if message.len() != payload || cause.len() != payload {
        return Err("issue payload length mismatch");
    }
    let mut digest = 0_u64;
    for j in 0..payload {
        if message[j] != b'a' + u8::try_from((ordinal + j) % 26).expect("modulo 26")
            || cause[j] != b'A' + u8::try_from((ordinal + 3 * j) % 26).expect("modulo 26")
        {
            return Err("issue payload byte mismatch");
        }
        digest = digest
            .checked_add(
                (j as u64 + 1) * u64::from(message[j])
                    + (payload as u64 + j as u64 + 1) * u64::from(cause[j]),
            )
            .ok_or("issue byte digest overflow")?;
    }
    Ok(digest)
}

/// Builds untimed expectations without calling the observed stream recorder.
fn expected_stream(iterations: usize, size: usize, payload: usize) -> Stream {
    let mut expected = Stream::default();
    for ordinal in 1..=iterations {
        let digest = (0..payload).fold(0_u64, |sum, j| {
            sum + (j as u64 + 1)
                * u64::from(b'a' + u8::try_from((ordinal + j) % 26).expect("modulo 26"))
                + (payload as u64 + j as u64 + 1)
                    * u64::from(b'A' + u8::try_from((ordinal + 3 * j) % 26).expect("modulo 26"))
        });
        for endpoint in 0..size {
            expected.count += 1;
            expected.bytes += digest;
            expected.per_endpoint[endpoint] += 1;
            expected.order = (expected.order ^ ordinal as u64).wrapping_mul(ORDER_PRIME);
            expected.order = (expected.order ^ endpoint as u64).wrapping_mul(ORDER_PRIME);
        }
    }
    expected
}

fn check_stream(actual: &Stream, expected: &Stream) -> Result<(), &'static str> {
    if actual.count != expected.count
        || actual.bytes != expected.bytes
        || actual.order != expected.order
        || actual.per_endpoint != expected.per_endpoint
    {
        return Err("issue stream mismatch");
    }
    Ok(())
}

fn callback_observation(
    facts: &Arc<Mutex<Facts>>,
    issue: &Issue,
    active: &Arc<AtomicUsize>,
    endpoint: usize,
    size: usize,
    payload: usize,
    location: &Location<'_>,
) {
    let ordinal = active.load(Ordering::SeqCst);
    let mut facts = facts.lock().expect("issue facts lock");
    if facts.error.is_some() {
        return;
    }
    match observe_issue(issue, ordinal, payload, location)
        .and_then(|digest| facts.callback.record(ordinal, endpoint, digest, size))
    {
        Ok(()) => {}
        Err(error) => facts.error = Some(error),
    }
}

fn recover(
    facts: &Arc<Mutex<Facts>>,
    issue: Issue,
    ordinal: usize,
    endpoint: usize,
    size: usize,
    payload: usize,
    location: &Location<'_>,
) -> Result<(), &'static str> {
    let digest = observe_issue(&issue, ordinal, payload, location)?;
    drop(issue);
    facts
        .lock()
        .map_err(|_| "issue facts lock")?
        .recovery
        .record(ordinal, endpoint, digest, size)?;
    Ok(())
}

#[inline(never)]
fn profile_issue_cold(
    inputs: &mut std::vec::IntoIter<Issue>,
    report: &mut impl FnMut(usize, Issue) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    report(1, inputs.next().ok_or("missing cold issue")?)
}

#[inline(never)]
fn profile_issue_reports(
    inputs: &mut std::vec::IntoIter<Issue>,
    iterations: usize,
    report: &mut impl FnMut(usize, Issue) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    for ordinal in 2..=iterations {
        report(ordinal, inputs.next().ok_or("missing steady issue")?)?;
    }
    Ok(())
}

/// Separates the first prebuilt report from steady reports and later validation.
fn drive(
    measurement: &mut Measurement,
    inputs: &mut std::vec::IntoIter<Issue>,
    iterations: usize,
    mut report: impl FnMut(usize, Issue) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    measurement.construction_finished();
    profile_issue_cold(inputs, &mut report)?;
    measurement.cold_finished();
    profile_issue_reports(inputs, iterations, &mut report)?;
    measurement.work_finished();
    if inputs.len() != 0 {
        return Err("unconsumed issue input");
    }
    Ok(())
}

fn owner(
    index: usize,
    drops: &Arc<AtomicUsize>,
    owners: &mut Vec<Arc<EndpointOwner>>,
    weak: &mut Vec<Weak<EndpointOwner>>,
) -> Arc<EndpointOwner> {
    let owner = Arc::new(EndpointOwner {
        index,
        drops: Arc::clone(drops),
    });
    weak.push(Arc::downgrade(&owner));
    owners.push(Arc::clone(&owner));
    owner
}

#[expect(
    clippy::too_many_arguments,
    reason = "the direct fixture passes existing probes without a validation framework"
)]
fn validate_facts(
    config: Config,
    payload: usize,
    constructed: usize,
    remaining: usize,
    facts: &Arc<Mutex<Facts>>,
    faults: &Arc<FaultCounters>,
    drops: &Arc<AtomicUsize>,
    weak: &[Weak<EndpointOwner>],
    result: Result<(), &'static str>,
) -> Result<(), &'static str> {
    let observed = *facts.lock().map_err(|_| "issue facts lock")?;
    let empty = config.size == 0;
    // An empty selection has one original-recovery slot, not one endpoint.
    let expected = expected_stream(config.iterations, config.size.max(1), payload);
    let total = (config.iterations * config.size.max(1)) as u64;
    let expected_callbacks = if empty || config.mode == Mode::OwnedDisabled {
        Stream::default()
    } else {
        expected
    };
    let expected_recovery =
        if empty || matches!(config.mode, Mode::OwnedDisabled | Mode::BorrowedFailed) {
            expected
        } else {
            Stream::default()
        };
    result.and_then(|()| {
        if constructed != config.iterations || remaining != 0 {
            return Err("issue input construction mismatch");
        }
        if let Some(error) = observed.error {
            return Err(error);
        }
        check_stream(&observed.callback, &expected_callbacks)?;
        check_stream(&observed.recovery, &expected_recovery)?;
        if observed.accepted
            != if empty || matches!(config.mode, Mode::OwnedDisabled | Mode::BorrowedFailed) {
                0
            } else {
                total
            }
            || observed.disabled
                != if empty || config.mode == Mode::OwnedDisabled {
                    total
                } else {
                    0
                }
            || observed.failed
                != if !empty && config.mode == Mode::BorrowedFailed {
                    total
                } else {
                    0
                }
            || faults.created.load(Ordering::SeqCst)
                != if config.mode == Mode::BorrowedFailed {
                    config.iterations * config.size
                } else {
                    0
                }
            || faults.dropped.load(Ordering::SeqCst) != faults.created.load(Ordering::SeqCst)
            || drops.load(Ordering::SeqCst) != 0
            || weak.iter().any(|witness| witness.upgrade().is_none())
        {
            return Err("issue totals or live ownership mismatch");
        }
        Ok(())
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "the measurement seam shares the direct fixture's existing probes"
)]
fn validate_run(
    measurement: &mut Measurement,
    config: Config,
    payload: usize,
    constructed: usize,
    inputs: &std::vec::IntoIter<Issue>,
    facts: &Arc<Mutex<Facts>>,
    faults: &Arc<FaultCounters>,
    drops: &Arc<AtomicUsize>,
    weak: &[Weak<EndpointOwner>],
    result: Result<(), &'static str>,
) -> Result<(), &'static str> {
    let valid = validate_facts(
        config,
        payload,
        constructed,
        inputs.len(),
        facts,
        faults,
        drops,
        weak,
        result,
    );
    if valid.is_ok() {
        measurement.validation_finished();
    }
    valid
}

fn check_released(
    weak: &[Weak<EndpointOwner>],
    drops: &Arc<AtomicUsize>,
    created: usize,
) -> Result<(), &'static str> {
    if weak.iter().any(|witness| witness.upgrade().is_some())
        || drops.load(Ordering::SeqCst) != created
    {
        return Err("issue owners retained");
    }
    Ok(())
}

fn check_fault(
    error: &DeliveryFault,
    owner: &Arc<EndpointOwner>,
    index: usize,
) -> Result<(), &'static str> {
    if error.endpoint != index || !Arc::ptr_eq(&error.owner, owner) {
        return Err("typed delivery owner mismatch");
    }
    Ok(())
}

pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    run_payload(config, PAYLOAD)
}

#[expect(
    clippy::too_many_lines,
    reason = "five concrete public paths share one measured teardown without a scenario framework"
)]
fn run_payload(config: Config, payload: usize) -> Result<String, &'static str> {
    let mut measurement = Measurement::new(None);
    let mut inputs = Vec::with_capacity(config.iterations);
    let mut line = None;
    for ordinal in 1..=config.iterations {
        let (issue, expected_line) = make_issue(ordinal, payload);
        if let Some(previous) = line {
            assert_eq!(
                previous, expected_line,
                "construction helper location changed"
            );
        }
        line = Some(expected_line);
        inputs.push(issue);
    }
    let line = line.expect("positive issue iterations");
    let constructed = inputs.len();
    let mut inputs = inputs.into_iter();
    let facts = Arc::new(Mutex::new(Facts::default()));
    let active = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let faults = Arc::new(FaultCounters {
        created: AtomicUsize::new(0),
        dropped: AtomicUsize::new(0),
    });
    let mut owners = Vec::new();
    let mut weak = Vec::new();
    let result = match config.mode {
        Mode::Direct => {
            let capture = owner(0, &drops, &mut owners, &mut weak);
            let observed = Arc::clone(&facts);
            let current = Arc::clone(&active);
            let reporter = IssueReporter::new(move |issue| {
                assert_eq!(capture.index, 0);
                callback_observation(&observed, issue, &current, 0, 1, payload, line);
            });
            let result = drive(
                &mut measurement,
                &mut inputs,
                config.iterations,
                |ordinal, issue| {
                    active.store(ordinal, Ordering::SeqCst);
                    reporter
                        .report(issue)
                        .map_err(|_| "direct issue rejected")?;
                    facts.lock().map_err(|_| "issue facts lock")?.accepted += 1;
                    Ok(())
                },
            );
            let result = validate_run(
                &mut measurement,
                config,
                payload,
                constructed,
                &inputs,
                &facts,
                &faults,
                &drops,
                &weak,
                result,
            );
            drop(reporter);
            result
        }
        Mode::Owned | Mode::OwnedDisabled => {
            let reporters: Vec<IssueReporter> = (0..config.size)
                .map(|index| {
                    if config.mode == Mode::OwnedDisabled {
                        return IssueReporter::default();
                    }
                    let capture = owner(index, &drops, &mut owners, &mut weak);
                    let observed = Arc::clone(&facts);
                    let current = Arc::clone(&active);
                    IssueReporter::new(move |issue| {
                        assert_eq!(capture.index, index);
                        callback_observation(
                            &observed,
                            issue,
                            &current,
                            index,
                            config.size,
                            payload,
                            line,
                        );
                    })
                })
                .collect();
            let selection = IssueReporters::new(reporters);
            let result = drive(
                &mut measurement,
                &mut inputs,
                config.iterations,
                |ordinal, issue| {
                    active.store(ordinal, Ordering::SeqCst);
                    let outcomes = selection.report(issue);
                    if outcomes.len() != config.size.max(1) {
                        return Err("owned outcome count mismatch");
                    }
                    for (index, outcome) in outcomes.into_iter().enumerate() {
                        match (config.mode, outcome) {
                            (_, Err(error))
                                if config.size == 0
                                    && error.is_disabled()
                                    && !error.is_closed() =>
                            {
                                recover(&facts, error.into_issue(), ordinal, 0, 1, payload, line)?;
                                facts.lock().map_err(|_| "issue facts lock")?.disabled += 1;
                            }
                            (Mode::Owned, Ok(())) => {
                                facts.lock().map_err(|_| "issue facts lock")?.accepted += 1;
                            }
                            (Mode::OwnedDisabled, Err(error))
                                if error.is_disabled() && !error.is_closed() =>
                            {
                                recover(
                                    &facts,
                                    error.into_issue(),
                                    ordinal,
                                    index,
                                    config.size,
                                    payload,
                                    line,
                                )?;
                                facts.lock().map_err(|_| "issue facts lock")?.disabled += 1;
                            }
                            _ => return Err("owned issue outcome mismatch"),
                        }
                    }
                    Ok(())
                },
            );
            let result = validate_run(
                &mut measurement,
                config,
                payload,
                constructed,
                &inputs,
                &facts,
                &faults,
                &drops,
                &weak,
                result,
            );
            drop(selection);
            result
        }
        Mode::Borrowed | Mode::BorrowedFailed => {
            let cause = Cause { tag: 17 };
            let cause_ref = &cause;
            let callbacks: Vec<Box<IssueCallback<'_, Cause, DeliveryFault>>> = (0..config.size)
                .map(|index| {
                    let capture = owner(index, &drops, &mut owners, &mut weak);
                    let observed = Arc::clone(&facts);
                    let current = Arc::clone(&active);
                    let fault_counters = Arc::clone(&faults);
                    Box::new(move |issue: &Issue, supplied: &Cause| {
                        assert!(std::ptr::eq(supplied, cause_ref));
                        assert_eq!(supplied.tag, 17);
                        assert_eq!(capture.index, index);
                        callback_observation(
                            &observed,
                            issue,
                            &current,
                            index,
                            config.size,
                            payload,
                            line,
                        );
                        if config.mode == Mode::BorrowedFailed {
                            fault_counters.created.fetch_add(1, Ordering::SeqCst);
                            Err(IssueDeliveryFailure::Failed(DeliveryFault {
                                endpoint: index,
                                owner: Arc::clone(&capture),
                                counters: Arc::clone(&fault_counters),
                            }))
                        } else {
                            Ok(())
                        }
                    }) as Box<IssueCallback<'_, Cause, DeliveryFault>>
                })
                .collect();
            let references: Vec<&IssueCallback<'_, Cause, DeliveryFault>> =
                callbacks.iter().map(|callback| callback.as_ref()).collect();
            let context = IssueContext::new(&references);
            let result = drive(
                &mut measurement,
                &mut inputs,
                config.iterations,
                |ordinal, issue| {
                    active.store(ordinal, Ordering::SeqCst);
                    let outcomes = context.report(issue, &cause);
                    if outcomes.len() != config.size.max(1) {
                        return Err("borrowed outcome count mismatch");
                    }
                    for (index, outcome) in outcomes.into_iter().enumerate() {
                        match (config.mode, outcome) {
                            (_, IssueDelivery::Disabled(issue)) if config.size == 0 => {
                                recover(&facts, issue, ordinal, 0, 1, payload, line)?;
                                facts.lock().map_err(|_| "issue facts lock")?.disabled += 1;
                            }
                            (Mode::Borrowed, IssueDelivery::Accepted) => {
                                facts.lock().map_err(|_| "issue facts lock")?.accepted += 1;
                            }
                            (Mode::BorrowedFailed, IssueDelivery::Failed { issue, error }) => {
                                check_fault(&error, &owners[index], index)?;
                                recover(&facts, issue, ordinal, index, config.size, payload, line)?;
                                facts.lock().map_err(|_| "issue facts lock")?.failed += 1;
                                drop(error);
                            }
                            _ => return Err("borrowed issue outcome mismatch"),
                        }
                    }
                    Ok(())
                },
            );
            let result = validate_run(
                &mut measurement,
                config,
                payload,
                constructed,
                &inputs,
                &facts,
                &faults,
                &drops,
                &weak,
                result,
            );
            drop(references);
            drop(callbacks);
            assert_eq!(cause.tag, 17, "borrowed cause mutated");
            result
        }
    };

    let observed = *facts.lock().map_err(|_| "issue facts lock")?;
    let valid = result;
    let created = owners.len();
    drop(owners);
    let released = check_released(&weak, &drops, created).is_ok();
    let dropped = drops.load(Ordering::SeqCst);
    let fault_created = faults.created.load(Ordering::SeqCst);
    let fault_dropped = faults.dropped.load(Ordering::SeqCst);
    drop(weak);
    drop(drops);
    drop(faults);
    drop(facts);
    drop(active);
    drop(inputs);
    let allocation_result = measurement.finish();
    valid?;
    if !released {
        return Err("issue owners retained");
    }
    let allocations = allocation_result?.map_or_else(|| "null".to_owned(), |record| record.json());
    let clones = if matches!(
        config.mode,
        Mode::Owned | Mode::OwnedDisabled | Mode::BorrowedFailed
    ) {
        (config.iterations * config.size.saturating_sub(1)) as u64
    } else {
        0
    };
    let feature_profile = if cfg!(feature = "deterministic") {
        "deterministic-enabled"
    } else {
        "minimal-production"
    };
    Ok(format!(
        "{{\"schema\":\"rca-issue-performance-profile-v1\",\"feature_profile\":\"{feature_profile}\",\"features\":{{\"deterministic\":{},\"allocations\":{}}},\"workload\":\"{}\",\"size\":{},\"iterations\":{},\"payload_bytes\":{},\"outcome\":\"validated\",\"cycles\":{},\"cold_reports\":1,\"steady_reports\":{},\"roots_committed\":0,\"responses_committed\":0,\"reducer_calls\":0,\"causal_completed\":0,\"callbacks\":{},\"accepted\":{},\"disabled\":{},\"failed\":{},\"callback_byte_digest\":{},\"callback_order_digest\":{},\"recovery_count\":{},\"recovery_byte_digest\":{},\"recovery_order_digest\":{},\"faults_created\":{},\"faults_dropped\":{},\"resources_created\":{},\"resources_dropped\":{},\"weak_expired\":{},\"inputs_constructed\":{},\"inputs_remaining\":0,\"owners_released\":true,\"source_expectations\":{{\"issue_clones\":{},\"owned_payload_copies\":{}}},\"phase_semantics\":{{\"cold\":\"first-report\",\"work\":\"remaining-reports\"}},\"driver_contract\":\"direct-synchronous-v1\",\"recorder_used\":false,\"allocations\":{}}}",
        cfg!(feature = "deterministic"),
        cfg!(feature = "allocations"),
        config.name,
        config.size,
        config.iterations,
        payload,
        config.iterations,
        config.iterations - 1,
        observed.callback.count,
        observed.accepted,
        observed.disabled,
        observed.failed,
        observed.callback.bytes,
        observed.callback.order,
        observed.recovery.count,
        observed.recovery.bytes,
        observed.recovery.order,
        fault_created,
        fault_dropped,
        created,
        dropped,
        released,
        constructed,
        clones,
        clones * 2,
        allocations
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::Location;

    #[cfg(feature = "allocations")]
    use crate::tests::in_child;

    #[cfg(feature = "allocations")]
    #[test]
    fn allocation_child_rejects_empty_selection() {
        let result = std::panic::catch_unwind(|| in_child("missing::issue_allocation_test"));
        let panic = result.expect_err("empty issue allocation selection must fail");
        let message = panic
            .downcast_ref::<String>()
            .expect("formatted diagnostic");
        assert!(message.starts_with(
            "isolated allocation test missing::issue_allocation_test did not pass exactly once:"
        ));
    }

    #[test]
    fn tiny_mixed_owned_preserves_order_and_full_recovery() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let first_events = Arc::clone(&events);
        let last_events = Arc::clone(&events);
        let reporters = IssueReporters::new([
            IssueReporter::new(move |_| first_events.lock().unwrap().push(0)),
            IssueReporter::default(),
            IssueReporter::new(move |_| last_events.lock().unwrap().push(2)),
        ]);
        let (issue, line) = make_issue(1, 8);
        let outcomes = reporters.report(issue);
        let [first, middle, last] = <[Result<(), _>; 3]>::try_from(outcomes).unwrap();
        assert!(first.is_ok());
        let rejected = middle.unwrap_err();
        assert!(rejected.is_disabled() && !rejected.is_closed());
        assert_eq!(
            observe_issue(&rejected.into_issue(), 1, 8, line).unwrap(),
            11_472
        );
        assert!(last.is_ok());
        assert_eq!(*events.lock().unwrap(), [0, 2]);
    }

    #[test]
    fn tiny_empty_and_default_paths_return_one_original() {
        let (direct, line) = make_issue(1, 8);
        let rejected = IssueReporter::default().report(direct).unwrap_err();
        assert!(rejected.is_disabled());
        assert!(observe_issue(&rejected.into_issue(), 1, 8, line).is_ok());
        let (owned, line) = make_issue(1, 8);
        let owned_buffers = (
            owned.message.as_ptr(),
            owned.cause.as_ref().unwrap().as_ptr(),
        );
        let [rejected] =
            <[Result<(), _>; 1]>::try_from(IssueReporters::default().report(owned)).unwrap();
        let recovered = rejected.unwrap_err().into_issue();
        assert_eq!(
            (
                recovered.message.as_ptr(),
                recovered.cause.as_ref().unwrap().as_ptr()
            ),
            owned_buffers
        );
        assert!(observe_issue(&recovered, 1, 8, line).is_ok());
        let (borrowed, line) = make_issue(1, 8);
        let borrowed_buffers = (
            borrowed.message.as_ptr(),
            borrowed.cause.as_ref().unwrap().as_ptr(),
        );
        let context = IssueContext::<Cause, DeliveryFault>::new(&[]);
        let [IssueDelivery::Disabled(recovered)] = <[IssueDelivery<DeliveryFault>; 1]>::try_from(
            context.report(borrowed, &Cause { tag: 17 }),
        )
        .ok()
        .unwrap() else {
            panic!("empty context did not recover exactly one issue")
        };
        assert_eq!(
            (
                recovered.message.as_ptr(),
                recovered.cause.as_ref().unwrap().as_ptr()
            ),
            borrowed_buffers
        );
        assert!(observe_issue(&recovered, 1, 8, line).is_ok());
    }

    #[test]
    fn empty_fanout_validator_requires_recovered_originals() {
        // Omitting the empty selection's single disabled outcome must fail,
        // even though there are no endpoint callbacks or endpoint owners.
        for mode in [
            Mode::Owned,
            Mode::OwnedDisabled,
            Mode::Borrowed,
            Mode::BorrowedFailed,
        ] {
            let config = Config {
                mode,
                name: "empty-control",
                size: 0,
                iterations: 2,
            };
            let facts = Arc::new(Mutex::new(Facts::default()));
            for ordinal in 1..=2 {
                let (issue, line) = make_issue(ordinal, 8);
                let recovered = if matches!(mode, Mode::Owned | Mode::OwnedDisabled) {
                    let [outcome] =
                        <[Result<(), _>; 1]>::try_from(IssueReporters::default().report(issue))
                            .unwrap();
                    let error = outcome.unwrap_err();
                    assert!(error.is_disabled() && !error.is_closed());
                    error.into_issue()
                } else {
                    let context = IssueContext::<Cause, DeliveryFault>::new(&[]);
                    let [IssueDelivery::Disabled(issue)] =
                        <[IssueDelivery<DeliveryFault>; 1]>::try_from(
                            context.report(issue, &Cause { tag: 17 }),
                        )
                        .ok()
                        .unwrap()
                    else {
                        panic!("empty borrowed selection did not recover its original");
                    };
                    issue
                };
                recover(&facts, recovered, ordinal, 0, 1, 8, line).unwrap();
                facts.lock().unwrap().disabled += 1;
            }
            let faults = Arc::new(FaultCounters {
                created: AtomicUsize::new(0),
                dropped: AtomicUsize::new(0),
            });
            let drops = Arc::new(AtomicUsize::new(0));
            let valid = *facts.lock().unwrap();
            assert_eq!(valid.recovery.count, 2);
            assert_eq!(valid.recovery.bytes, 23_080);
            let validate = || validate_facts(config, 8, 2, 0, &facts, &faults, &drops, &[], Ok(()));
            assert_eq!(validate(), Ok(()));
            for broken in [
                Facts {
                    disabled: 0,
                    ..valid
                },
                Facts {
                    recovery: Stream::default(),
                    ..valid
                },
                Facts {
                    callback: valid.recovery,
                    ..valid
                },
                Facts { failed: 1, ..valid },
            ] {
                *facts.lock().unwrap() = broken;
                assert!(validate().is_err());
            }
        }
    }

    #[test]
    fn endpoint_63_corruption_is_not_hidden_by_lower_slots() {
        let config =
            Config::parse(["issue-owned", "64", "2"].into_iter().map(str::to_owned)).unwrap();
        let expected = expected_stream(config.iterations, config.size, 8);
        assert_eq!(expected.count, 128);
        assert_eq!(expected.per_endpoint[config.size - 1], 2);
        let mut corrupted = expected;
        corrupted.per_endpoint[config.size - 1] = 1;
        assert!(check_stream(&corrupted, &expected).is_err());
    }

    #[test]
    fn tiny_borrowed_rejections_preserve_original_fault_and_cause() {
        let cause = Cause { tag: 17 };
        let drops = Arc::new(AtomicUsize::new(0));
        let owner = Arc::new(EndpointOwner { index: 2, drops });
        let fault_counters = Arc::new(FaultCounters {
            created: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        });
        let seen = std::cell::Cell::new(0);
        let cause_ref = &cause;
        let seen_ref = &seen;
        let accepted = |_: &Issue, supplied: &Cause| {
            assert!(std::ptr::eq(supplied, cause_ref));
            seen.set(seen.get() + 1);
            Ok::<_, IssueDeliveryFailure<DeliveryFault>>(())
        };
        let disabled = |_: &Issue, supplied: &Cause| {
            assert!(std::ptr::eq(supplied, cause_ref));
            seen.set(seen.get() + 1);
            Err::<(), _>(IssueDeliveryFailure::Disabled)
        };
        let failed_owner = Arc::clone(&owner);
        let failed_counters = Arc::clone(&fault_counters);
        let failed = move |_: &Issue, supplied: &Cause| {
            assert!(std::ptr::eq(supplied, cause_ref));
            seen_ref.set(seen_ref.get() + 1);
            failed_counters.created.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(IssueDeliveryFailure::Failed(DeliveryFault {
                endpoint: 2,
                owner: Arc::clone(&failed_owner),
                counters: Arc::clone(&failed_counters),
            }))
        };
        let refs: [&IssueCallback<'_, Cause, DeliveryFault>; 3] = [&accepted, &disabled, &failed];
        let context = IssueContext::new(&refs);
        let (issue, line) = make_issue(1, 8);
        let [a, b, c] =
            <[IssueDelivery<DeliveryFault>; 3]>::try_from(context.report(issue, &cause))
                .ok()
                .unwrap();
        assert!(matches!(a, IssueDelivery::Accepted));
        let IssueDelivery::Disabled(recovered) = b else {
            panic!("wrong disabled outcome")
        };
        assert!(observe_issue(&recovered, 1, 8, line).is_ok());
        let IssueDelivery::Failed {
            issue: recovered,
            error,
        } = c
        else {
            panic!("wrong failed outcome")
        };
        assert!(observe_issue(&recovered, 1, 8, line).is_ok());
        assert!(Arc::ptr_eq(&error.owner, &owner));
        assert_eq!(error.endpoint, 2);
        assert_eq!(seen.get(), 3);
        assert_eq!(cause.tag, 17);
        drop(error);
        assert_eq!(fault_counters.dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn real_retained_reporter_and_callback_keep_owners_live() {
        let drops = Arc::new(AtomicUsize::new(0));
        let owner = Arc::new(EndpointOwner {
            index: 0,
            drops: Arc::clone(&drops),
        });
        let weak = Arc::downgrade(&owner);
        let capture = Arc::clone(&owner);
        let reporter = IssueReporter::new(move |_| assert_eq!(capture.index, 0));
        let retained = reporter.clone();
        drop(reporter);
        drop(owner);
        assert!(weak.upgrade().is_some());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(check_released(std::slice::from_ref(&weak), &drops, 1).is_err());
        retained
            .report(Issue::new("retained", "handler still live"))
            .unwrap();
        drop(retained);
        assert!(check_released(std::slice::from_ref(&weak), &drops, 1).is_ok());
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        let borrowed_owner = Arc::new(EndpointOwner {
            index: 1,
            drops: Arc::clone(&drops),
        });
        let borrowed_weak = Arc::downgrade(&borrowed_owner);
        let capture = Arc::clone(&borrowed_owner);
        let callback = move |_: &Issue, _: &Cause| {
            assert_eq!(capture.index, 1);
            Ok::<_, IssueDeliveryFailure<DeliveryFault>>(())
        };
        let refs: [&IssueCallback<'_, Cause, DeliveryFault>; 1] = [&callback];
        let context = IssueContext::new(&refs);
        assert!(matches!(
            context
                .report(
                    Issue::new("borrowed", "handler still live"),
                    &Cause { tag: 17 }
                )
                .as_slice(),
            [IssueDelivery::Accepted]
        ));
        drop(borrowed_owner);
        assert!(borrowed_weak.upgrade().is_some());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(check_released(std::slice::from_ref(&borrowed_weak), &drops, 2).is_err());
        drop(callback);
        assert!(check_released(std::slice::from_ref(&borrowed_weak), &drops, 2).is_ok());
    }

    #[test]
    fn real_retained_delivery_fault_blocks_owner_release() {
        let drops = Arc::new(AtomicUsize::new(0));
        let owner = Arc::new(EndpointOwner {
            index: 0,
            drops: Arc::clone(&drops),
        });
        let weak = Arc::downgrade(&owner);
        let counters = Arc::new(FaultCounters {
            created: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        });
        let capture = Arc::clone(&owner);
        let fault_counters = Arc::clone(&counters);
        let callback = move |_: &Issue, _: &Cause| {
            fault_counters.created.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(IssueDeliveryFailure::Failed(DeliveryFault {
                endpoint: 0,
                owner: Arc::clone(&capture),
                counters: Arc::clone(&fault_counters),
            }))
        };
        let refs: [&IssueCallback<'_, Cause, DeliveryFault>; 1] = [&callback];
        let context = IssueContext::new(&refs);
        let [IssueDelivery::Failed { issue, error }] =
            <[IssueDelivery<DeliveryFault>; 1]>::try_from(
                context.report(Issue::new("fault", "owned"), &Cause { tag: 17 }),
            )
            .ok()
            .unwrap()
        else {
            panic!("expected original typed failure")
        };
        drop(issue);
        drop(callback);
        drop(owner);
        assert!(weak.upgrade().is_some());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(check_released(std::slice::from_ref(&weak), &drops, 1).is_err());
        drop(error);
        assert!(check_released(std::slice::from_ref(&weak), &drops, 1).is_ok());
        assert_eq!(counters.created.load(Ordering::SeqCst), 1);
        assert_eq!(counters.dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn independent_stream_oracle_rejects_each_corrupt_fact() {
        let expected = expected_stream(2, 3, 8);
        assert_eq!(expected.count, 6);
        assert_eq!(expected.bytes, 69_240);
        assert_eq!(expected.order, 0xfe85_e252_8a18_99aa);
        assert_eq!(expected.per_endpoint[..3], [2, 2, 2]);
        let mut actual = Stream::default();
        assert!(actual.record(1, 1, 11_472, 3).is_err());
        assert_eq!(actual.count, 0);
        actual.record(1, 0, 11_472, 3).unwrap();
        assert!(actual.record(1, 0, 11_472, 3).is_err());
        assert!(actual.record(2, 1, 11_608, 3).is_err());
        for (ordinal, endpoint, digest) in [
            (1, 1, 11_472),
            (1, 2, 11_472),
            (2, 0, 11_608),
            (2, 1, 11_608),
            (2, 2, 11_608),
        ] {
            actual.record(ordinal, endpoint, digest, 3).unwrap();
        }
        assert!(check_stream(&actual, &expected).is_ok());
        for mut wrong in [
            Stream {
                count: 5,
                ..expected
            },
            Stream {
                bytes: expected.bytes + 1,
                ..expected
            },
            Stream {
                order: expected.order ^ 1,
                ..expected
            },
            Stream {
                per_endpoint: [0; MAX_ENDPOINTS],
                ..expected
            },
        ] {
            assert!(check_stream(&wrong, &expected).is_err());
            wrong = expected;
            assert!(check_stream(&wrong, &expected).is_ok());
        }
        let (mut issue, line) = make_issue(1, 8);
        issue.location = Location::caller();
        assert!(observe_issue(&issue, 1, 8, line).is_err());
        let (mut issue, line) = make_issue(1, 8);
        issue.severity = IssueSeverity::Note;
        assert!(observe_issue(&issue, 1, 8, line).is_err());
    }

    #[test]
    fn production_validator_rejects_corrupt_stream_outcome_fault_and_input() {
        let config = Config {
            mode: Mode::BorrowedFailed,
            name: "issue-borrowed-failed",
            size: 1,
            iterations: 2,
        };
        let expected = expected_stream(2, 1, 8);
        let valid = Facts {
            callback: expected,
            recovery: expected,
            failed: 2,
            ..Facts::default()
        };
        let facts = Arc::new(Mutex::new(valid));
        let faults = Arc::new(FaultCounters {
            created: AtomicUsize::new(2),
            dropped: AtomicUsize::new(2),
        });
        let drops = Arc::new(AtomicUsize::new(0));
        let owner = Arc::new(EndpointOwner {
            index: 0,
            drops: Arc::clone(&drops),
        });
        let weak = [Arc::downgrade(&owner)];
        let validate = |remaining, result| {
            validate_facts(
                config, 8, 2, remaining, &facts, &faults, &drops, &weak, result,
            )
        };
        assert!(validate(0, Ok(())).is_ok());
        for wrong in [
            Facts {
                callback: Stream {
                    count: 1,
                    ..expected
                },
                ..valid
            },
            Facts {
                callback: Stream {
                    bytes: expected.bytes + 1,
                    ..expected
                },
                ..valid
            },
            Facts {
                callback: Stream {
                    order: expected.order ^ 1,
                    ..expected
                },
                ..valid
            },
            Facts {
                recovery: Stream {
                    count: 1,
                    ..expected
                },
                ..valid
            },
            Facts {
                recovery: Stream {
                    bytes: expected.bytes + 1,
                    ..expected
                },
                ..valid
            },
            Facts {
                recovery: Stream {
                    order: expected.order ^ 1,
                    ..expected
                },
                ..valid
            },
            Facts { failed: 1, ..valid },
        ] {
            *facts.lock().unwrap() = wrong;
            assert!(validate(0, Ok(())).is_err());
        }
        *facts.lock().unwrap() = valid;
        assert!(validate(1, Ok(())).is_err());
        assert!(validate(0, Err("report failed")).is_err());
        faults.dropped.store(1, Ordering::SeqCst);
        assert!(validate(0, Ok(())).is_err());
        faults.dropped.store(2, Ordering::SeqCst);
        drop(owner);
        assert!(validate(0, Ok(())).is_err());
    }

    #[test]
    fn corrupted_public_callback_returns_validation_error_and_releases_owner() {
        let config = Config {
            mode: Mode::Direct,
            name: "issue-direct",
            size: 1,
            iterations: 1,
        };
        let facts = Arc::new(Mutex::new(Facts::default()));
        let active = Arc::new(AtomicUsize::new(1));
        let faults = Arc::new(FaultCounters {
            created: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        });
        let drops = Arc::new(AtomicUsize::new(0));
        let owner = Arc::new(EndpointOwner {
            index: 0,
            drops: Arc::clone(&drops),
        });
        let weak = [Arc::downgrade(&owner)];
        let capture = Arc::clone(&owner);
        let observed = Arc::clone(&facts);
        let current = Arc::clone(&active);
        let (mut issue, location) = make_issue(1, 8);
        issue.message.to_mut().replace_range(..1, "?");
        let reporter = IssueReporter::new(move |issue| {
            assert_eq!(capture.index, 0);
            callback_observation(&observed, issue, &current, 0, 1, 8, location);
        });
        reporter.report(issue).unwrap();
        facts.lock().unwrap().accepted = 1;
        assert_eq!(
            validate_facts(config, 8, 1, 0, &facts, &faults, &drops, &weak, Ok(())),
            Err("issue payload byte mismatch")
        );
        drop(reporter);
        drop(owner);
        assert!(check_released(&weak, &drops, 1).is_ok());
    }

    #[test]
    fn production_fault_validator_checks_tag_and_owner_identity() {
        let drops = Arc::new(AtomicUsize::new(0));
        let owner = Arc::new(EndpointOwner {
            index: 0,
            drops: Arc::clone(&drops),
        });
        let other = Arc::new(EndpointOwner {
            index: 1,
            drops: Arc::clone(&drops),
        });
        let counters = Arc::new(FaultCounters {
            created: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        });
        let error = DeliveryFault {
            endpoint: 0,
            owner: Arc::clone(&owner),
            counters,
        };
        assert!(check_fault(&error, &owner, 0).is_ok());
        assert!(check_fault(&error, &other, 0).is_err());
        assert!(check_fault(&error, &owner, 1).is_err());
    }

    #[cfg(feature = "allocations")]
    #[test]
    fn retained_owned_issue_rejects_cache_none() {
        if !in_child("issues::tests::retained_owned_issue_rejects_cache_none") {
            return;
        }
        let measurement = Measurement::new(None);
        let retained = Box::new(Issue::new("retained", String::from("owned")));
        std::hint::black_box(&retained);
        assert!(measurement.finish().is_err());
        drop(retained);
    }

    #[cfg(feature = "allocations")]
    #[test]
    fn clean_owned_issue_qualifies_cache_none() {
        if !in_child("issues::tests::clean_owned_issue_qualifies_cache_none") {
            return;
        }
        let measurement = Measurement::new(None);
        drop(Box::new(Issue::new("clean", String::from("owned"))));
        assert!(measurement.finish().is_ok());
    }
}
