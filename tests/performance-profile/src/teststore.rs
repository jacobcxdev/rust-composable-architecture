//! Profiles exhaustive `TestStore` boundaries with independently counted owned values.
//!
//! Normal and allocation builds use the same operations. Construction, useful
//! work, validation and teardown have separate measurement endpoints. Failure
//! workloads keep the panic and failed Store alive until validation completes.

use std::cell::Cell;
use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::task::{Context, Poll, Wake, Waker};

use rca_core::{Effect, ReduceContext, Reducer};
use rca_test::{CaptureKey, TestStore};

use crate::measurement::Measurement;

/// Bounds client polls independently of the scheduler's task budget.
const OUTER_POLLS: usize = 1024;
/// Bounds each internal scheduler drive, not the complete client operation.
const STEP_BUDGET: usize = 4096;

/// Records whether a pending operation requested another poll.
struct WakeFlag(AtomicBool);
impl Wake for WakeFlag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

/// Drives one pinned operation without supplying another runtime or advancing time.
///
/// Pending without a wake and exhaustion of the outer poll budget are failures.
/// Blocking polls still require the separate child-process watchdog.
fn poll_test_operation<F: Future>(future: F) -> Result<F::Output, &'static str> {
    let flag = Arc::new(WakeFlag(AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&flag));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    for _ in 0..OUTER_POLLS {
        flag.0.store(false, Ordering::Release);
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Ok(output);
        }
        if !flag.0.swap(false, Ordering::AcqRel) {
            return Err("TestStore operation returned unwoken Pending");
        }
    }
    Err("TestStore operation outer poll budget exhausted")
}

// Each index names one counter in the fixed NAMES and expected-count schemas.
const ROOTS: usize = 0;
const RESPONSES: usize = 1;
const LAST: usize = 2;
const CHECKSUM: usize = 3;
const REDUCTIONS: usize = 4;
const MATCHED: usize = 5;
const UPDATES: usize = 6;
const BOUNDARIES: usize = 7;
const TASKS: usize = 8;
const STATE_CLONES: usize = 9;
const STATE_EQ: usize = 10;
const CAPTURES: usize = 11;
const REV_CLONES: usize = 12;
const REV_EQ: usize = 13;
const SETS: usize = 14;
const ACTIONS: usize = 15;
const ACTION_DROPS: usize = 16;
const BYTES: usize = 17;
const BYTE_DROPS: usize = 18;
const REVISIONS: usize = 19;
const REV_DROPS: usize = 20;
const RENDERERS: usize = 21;
/// Defines the order of the scalar facts in output and independent expectations.
const NAMES: [&str; 22] = [
    "roots",
    "responses",
    "last_sequence",
    "sequence_checksum",
    "reductions",
    "matched_receives",
    "expected_updates",
    "boundaries",
    "task_finishes",
    "state_clones",
    "state_equalities",
    "capture_calls",
    "revision_clones",
    "revision_equalities",
    "declared_sets",
    "actions_created",
    "actions_dropped",
    "bytes_created",
    "bytes_dropped",
    "revisions_created",
    "revisions_dropped",
    "renderer_calls",
];

/// Counts real operations and ownership changes, including assertion overhead.
struct Probe([AtomicU64; 22]);
impl Probe {
    /// Creates the single counter owner before the construction endpoint.
    fn new() -> Arc<Self> {
        Arc::new(Self(std::array::from_fn(|_| AtomicU64::new(0))))
    }
    /// Records one event in the fixed schema without allocating.
    fn add(&self, index: usize, value: u64) {
        self.0[index].fetch_add(value, Ordering::SeqCst);
    }
    /// Copies scalar facts without retaining any measured value.
    fn counts(&self) -> [u64; 22] {
        std::array::from_fn(|index| self.0[index].load(Ordering::SeqCst))
    }
}

/// Counts deep payload copies and destruction separately from state snapshots.
struct OwnedBytes {
    /// Contains a sequence-dependent pattern checked byte by byte after work.
    value: Vec<u8>,
    /// Retains the counter owner until this payload is destroyed.
    probe: Arc<Probe>,
}
impl OwnedBytes {
    /// Creates one counted payload with a deterministic pattern.
    fn new(size: usize, sequence: u64, probe: &Arc<Probe>) -> Self {
        probe.add(BYTES, 1);
        Self {
            value: (0..size)
                .map(|j| {
                    u8::try_from(sequence.checked_add(u64::try_from(j).unwrap()).unwrap() % 251)
                        .unwrap()
                })
                .collect(),
            probe: Arc::clone(probe),
        }
    }
}
impl Clone for OwnedBytes {
    fn clone(&self) -> Self {
        self.probe.add(BYTES, 1);
        Self {
            value: self.value.clone(),
            probe: Arc::clone(&self.probe),
        }
    }
}
impl Drop for OwnedBytes {
    fn drop(&mut self) {
        self.probe.add(BYTE_DROPS, 1);
    }
}

/// Records ordered commits while exposing the cost of full-state snapshots.
struct State {
    /// Counts externally sent Start actions.
    roots: u64,
    /// Counts their effect-emitted Response actions.
    responses: u64,
    /// Requires responses to arrive once and in sequence.
    last_sequence: u64,
    /// Retains the original response payload, not the expected-state copy.
    bytes: OwnedBytes,
}
impl Clone for State {
    fn clone(&self) -> Self {
        self.bytes.probe.add(STATE_CLONES, 1);
        Self {
            roots: self.roots,
            responses: self.responses,
            last_sequence: self.last_sequence,
            bytes: self.bytes.clone(),
        }
    }
}
impl PartialEq for State {
    fn eq(&self, other: &Self) -> bool {
        self.bytes.probe.add(STATE_EQ, 1);
        self.roots == other.roots
            && self.responses == other.responses
            && self.last_sequence == other.last_sequence
            && self.bytes.value == other.bytes.value
    }
}
impl Eq for State {}
impl fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("State([opaque])")
    }
}

/// Counts scalar capture copies and comparisons independently of full-state checks.
struct OwnedRevision {
    /// Captured external commit count.
    roots: u64,
    /// Captured effect-response commit count.
    responses: u64,
    /// Records this capture's clone, equality and destruction events.
    probe: Arc<Probe>,
}
impl OwnedRevision {
    /// Creates one counted capture without reading the current Store.
    fn new(roots: u64, responses: u64, probe: &Arc<Probe>) -> Self {
        probe.add(REVISIONS, 1);
        Self {
            roots,
            responses,
            probe: Arc::clone(probe),
        }
    }
}
impl Clone for OwnedRevision {
    fn clone(&self) -> Self {
        self.probe.add(REV_CLONES, 1);
        Self::new(self.roots, self.responses, &self.probe)
    }
}
impl PartialEq for OwnedRevision {
    fn eq(&self, other: &Self) -> bool {
        self.probe.add(REV_EQ, 1);
        self.roots == other.roots && self.responses == other.responses
    }
}
impl Eq for OwnedRevision {}
impl fmt::Debug for OwnedRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OwnedRevision([opaque])")
    }
}
impl Drop for OwnedRevision {
    fn drop(&mut self) {
        self.probe.add(REV_DROPS, 1);
    }
}
/// Names the independent scalar capture checked beside every full-state boundary.
struct RevisionCapture;
impl CaptureKey for RevisionCapture {
    type Value = OwnedRevision;
    type Error = Infallible;
}

/// Moves one non-Clone payload from an external action through its effect response.
///
/// Each Cell permits one take through the reducer's borrowed action. The shared
/// probe records both action lifetimes without owning either action.
enum Action {
    Start {
        sequence: u64,
        bytes: Cell<Option<OwnedBytes>>,
        probe: Arc<Probe>,
    },
    Response {
        sequence: u64,
        bytes: Cell<Option<OwnedBytes>>,
        probe: Arc<Probe>,
    },
}
impl Action {
    /// Counts one external action before its payload enters the Store.
    fn start(sequence: u64, bytes: OwnedBytes, probe: &Arc<Probe>) -> Self {
        probe.add(ACTIONS, 1);
        Self::Start {
            sequence,
            bytes: Cell::new(Some(bytes)),
            probe: Arc::clone(probe),
        }
    }
}
impl Drop for Action {
    fn drop(&mut self) {
        let (Self::Start { probe, .. } | Self::Response { probe, .. }) = self;
        probe.add(ACTION_DROPS, 1);
    }
}
/// Moves payloads through ordered responses and keeps a reducer-lifetime witness.
struct Feature {
    /// Records real reductions independently of expected-state updates.
    probe: Arc<Probe>,
    /// Its Weak witness must expire after Store teardown.
    _owner: Arc<()>,
}
impl Reducer for Feature {
    type State = State;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        self.probe.add(REDUCTIONS, 1);
        match action {
            Action::Start {
                sequence, bytes, ..
            } => {
                assert_eq!(*sequence, state.roots.checked_add(1).unwrap());
                let bytes = bytes.take().expect("root payload taken once");
                state.roots = *sequence;
                self.probe.add(ROOTS, 1);
                self.probe.add(CHECKSUM, *sequence);
                self.probe.add(ACTIONS, 1);
                Effect::send(Action::Response {
                    sequence: *sequence,
                    bytes: Cell::new(Some(bytes)),
                    probe: Arc::clone(&self.probe),
                })
            }
            Action::Response {
                sequence, bytes, ..
            } => {
                assert_eq!(*sequence, state.responses.checked_add(1).unwrap());
                assert_eq!(*sequence, state.last_sequence.checked_add(1).unwrap());
                state.bytes = bytes.take().expect("response payload taken once");
                state.responses = *sequence;
                state.last_sequence = *sequence;
                self.probe.add(RESPONSES, 1);
                self.probe.0[LAST].store(*sequence, Ordering::SeqCst);
                Effect::none()
            }
        }
    }
}

/// Builds the real exhaustive Store and returns a non-owning reducer witness.
fn fixture(size: usize, budget: usize, probe: &Arc<Probe>) -> (TestStore<State, Action>, Weak<()>) {
    fixture_with_renderer(size, budget, probe, render_state)
}

/// Renders proportional structure without including private payload bytes.
fn render_state(state: &State) -> String {
    // Render proportional opaque structure, never the private payload bytes.
    let digest: u64 = state.bytes.value.iter().map(|byte| u64::from(*byte)).sum();
    format!(
        "roots={} responses={} last={} digest={}\n{}",
        state.roots,
        state.responses,
        state.last_sequence,
        digest,
        "owned byte: [opaque]\n".repeat(state.bytes.value.len())
    )
}

/// Installs independently counted captures and a renderer used only on failure.
fn fixture_with_renderer(
    size: usize,
    budget: usize,
    probe: &Arc<Probe>,
    render: fn(&State) -> String,
) -> (TestStore<State, Action>, Weak<()>) {
    let owner = Arc::new(());
    let weak = Arc::downgrade(&owner);
    let store = TestStore::builder(
        State {
            roots: 0,
            responses: 0,
            last_sequence: 0,
            bytes: OwnedBytes::new(size, 0, probe),
        },
        Feature {
            probe: Arc::clone(probe),
            _owner: owner,
        },
    )
    .with_step_budget(budget)
    .with_capture::<RevisionCapture>(|state| {
        state.bytes.probe.add(CAPTURES, 1);
        Ok(OwnedRevision::new(
            state.roots,
            state.responses,
            &state.bytes.probe,
        ))
    })
    .unwrap()
    .with_snapshot_renderer(move |state| {
        state.bytes.probe.add(RENDERERS, 1);
        render(state)
    })
    .build();
    (store, weak)
}

#[derive(Clone, Copy)]
/// Selects one bounded workload before construction or measurement starts.
pub(crate) struct Config {
    /// Distinguishes useful pairs, cold idle construction and expected failure.
    name: &'static str,
    /// Number of bytes in each independently owned payload.
    size: usize,
    /// Pair count, or the required single iteration for idle and mismatch.
    iterations: u64,
}
impl Config {
    /// Rejects unknown workloads and dimensions outside the finite workload matrix.
    ///
    /// # Errors
    /// Returns an error for malformed, missing, extra or unsupported arguments.
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let name = match args.next().as_deref() {
            Some("teststore-owned") => "teststore-owned",
            Some("teststore-idle") => "teststore-idle",
            Some("teststore-mismatch") => "teststore-mismatch",
            _ => return Err("unknown TestStore workload"),
        };
        let number = |value: Option<String>| {
            let value = value.ok_or("missing TestStore dimension")?;
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("invalid TestStore dimension");
            }
            value
                .parse::<u64>()
                .map_err(|_| "TestStore dimension overflow")
        };
        let size = usize::try_from(number(args.next())?).map_err(|_| "TestStore size overflow")?;
        let iterations = number(args.next())?;
        if args.next().is_some()
            || !matches!(size, 0 | 8 | 4096 | 65536)
            || !(1..=256).contains(&iterations)
            || (name == "teststore-idle" && iterations != 1)
            || (name == "teststore-mismatch" && (size == 0 || iterations != 1))
        {
            return Err("TestStore dimensions outside finite bounds");
        }
        Ok(Self {
            name,
            size,
            iterations,
        })
    }
    /// Excludes the cold idle control from useful pair counts.
    fn pairs(self) -> u64 {
        if self.name == "teststore-idle" {
            0
        } else {
            self.iterations
        }
    }
}

#[inline(never)]
/// Measures real send/receive pairs, their assertions and causal task completion.
///
/// The returned pointer and checksum describe the final moved payload. Neither
/// retains the payload or supplies the expected-state copy.
fn profile_teststore_pairs(
    store: &mut TestStore<State, Action>,
    config: Config,
    probe: &Arc<Probe>,
) -> Result<Option<(usize, u64)>, &'static str> {
    let mut payload = None;
    for sequence in 1..=config.pairs() {
        let bytes = OwnedBytes::new(config.size, sequence, probe);
        let checksum = bytes
            .value
            .iter()
            .try_fold(0_u64, |sum, byte| sum.checked_add(u64::from(*byte)))
            .ok_or("owned payload checksum overflow")?;
        payload = Some((bytes.value.as_ptr() as usize, checksum));
        let send = poll_test_operation(store.send_with_captures(
            Action::start(sequence, bytes, probe),
            |state| {
                probe.add(UPDATES, 1);
                state.roots = sequence;
            },
            |captures| {
                probe.add(SETS, 1);
                captures.set::<RevisionCapture>(OwnedRevision::new(
                    sequence,
                    sequence.checked_sub(1).unwrap(),
                    probe,
                ))
            },
        ))?
        .map_err(|_| "TestStore send failed")?;
        probe.add(BOUNDARIES, 1);
        let receive = poll_test_operation(store.receive_with_captures(
            |action| {
                let matched = matches!(
                    action,
                    Action::Response { sequence: actual, .. } if *actual == sequence
                );
                if matched {
                    probe.add(MATCHED, 1);
                }
                matched
            },
            |state| {
                probe.add(UPDATES, 1);
                state.responses = sequence;
                state.last_sequence = sequence;
                state.bytes = OwnedBytes::new(config.size, sequence, probe);
            },
            |captures| {
                probe.add(SETS, 1);
                captures.set::<RevisionCapture>(OwnedRevision::new(sequence, sequence, probe))
            },
        ))?
        .map_err(|_| "TestStore receive failed")?;
        probe.add(BOUNDARIES, 1);
        for task in [&send, &receive] {
            poll_test_operation(task.finish())?
                .map_err(|_| "TestStore causal task finish failed")?;
            if !task.is_finished() || task.is_cancelled() {
                return Err("TestStore causal task not normally complete");
            }
            probe.add(TASKS, 1);
        }
        drop((send, receive));
    }
    Ok(payload)
}

#[derive(Clone, Copy, Debug)]
/// Retains scalar validation results after all measured owners are destroyed.
struct Facts {
    /// Final independent operation and ownership counters.
    counts: [u64; 22],
    /// Sum of every byte checked in the final actual payload.
    byte_checksum: u64,
    /// Actual payload length checked through a borrowed Store read.
    byte_len: usize,
    /// Records that the final borrowed read checked each payload byte.
    bytes_verified: bool,
    /// Requires both reducer and counter owners to expire after teardown.
    owners_released: bool,
}
impl Facts {
    /// Rejects any count, payload or ownership result that differs from the contract.
    fn validate(self, config: Config) -> Result<(), &'static str> {
        let (counts, sequence) = if config.name == "teststore-mismatch" {
            (FAILURE_COUNTS, 1)
        } else {
            (expected_counts(config.pairs()), config.pairs())
        };
        if self.counts != counts
            || self.byte_len != config.size
            || self.byte_checksum != expected_byte_checksum(config.size, sequence)
            || !self.bytes_verified
            || !self.owners_released
        {
            return Err("TestStore semantic or ownership facts mismatch");
        }
        Ok(())
    }
    /// Formats scalar facts only after allocation measurement ends.
    fn json(self) -> String {
        let fields: Vec<_> = NAMES
            .iter()
            .zip(self.counts)
            .map(|(name, value)| format!("\"{name}\":{value}"))
            .collect();
        format!(
            "{{{},\"byte_checksum\":{},\"byte_len\":{},\"bytes_verified\":{}}}",
            fields.join(","),
            self.byte_checksum,
            self.byte_len,
            self.bytes_verified
        )
    }
}

/// Computes the expected checksum from dimensions, not observed payload bytes.
fn expected_byte_checksum(size: usize, sequence: u64) -> u64 {
    (0..size)
        .try_fold(0_u64, |sum, index| {
            sum.checked_add(sequence.checked_add(u64::try_from(index).unwrap()).unwrap() % 251)
        })
        .unwrap()
}

/// Defines final counts from pair count, including validation and teardown work.
fn expected_counts(i: u64) -> [u64; 22] {
    let twice = i.checked_mul(2).unwrap();
    let four = i.checked_mul(4).unwrap();
    let clones = four.checked_add(1).unwrap();
    let bytes = i.checked_mul(6).unwrap().checked_add(2).unwrap();
    let revisions = i.checked_mul(10).unwrap().checked_add(3).unwrap();
    [
        i,
        i,
        i,
        i.checked_mul(i.checked_add(1).unwrap()).unwrap() / 2,
        twice,
        i,
        twice,
        twice,
        twice,
        clones,
        twice,
        four.checked_add(2).unwrap(),
        clones,
        clones,
        twice,
        twice,
        twice,
        bytes,
        bytes,
        revisions,
        revisions,
        0,
    ]
}

/// Runs one workload and emits facts only after validation and complete teardown.
///
/// Mismatch runs require an isolated child because they temporarily replace the
/// process panic hook. The original hook is restored before return or unwind.
///
/// # Errors
/// Returns an error when operation, count, payload or allocation checks fail.
pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    if config.name == "teststore-mismatch" {
        // This CLI/control runs in its own child. No hook formatting or I/O
        // contaminates the failure boundary; restore the original before exit.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_mismatch(config)));
        std::panic::set_hook(hook);
        return match result {
            Ok(result) => result,
            Err(payload) => std::panic::resume_unwind(payload),
        };
    }
    let mut measurement = Measurement::new(None);
    let probe = Probe::new();
    let probe_weak = Arc::downgrade(&probe);
    let (mut store, reducer_weak) = fixture(config.size, STEP_BUDGET, &probe);
    measurement.construction_finished();
    let payload = profile_teststore_pairs(&mut store, config, &probe)?;
    measurement.work_finished();
    let counts = probe.counts();
    let mut want = expected_counts(config.pairs());
    want[CAPTURES] -= 1;
    want[REV_CLONES] -= 1;
    want[REV_EQ] -= 1;
    want[REVISIONS] -= 2;
    want[REV_DROPS] -= 3;
    want[BYTE_DROPS] -= 2;
    if counts != want {
        return Err("TestStore useful-work counts or live owners mismatch");
    }
    let size = config.size;
    let pairs = config.pairs();
    poll_test_operation(store.assert_state(move |state| {
        assert_eq!(
            (state.roots, state.responses, state.last_sequence),
            (pairs, pairs, pairs)
        );
        assert_eq!(state.bytes.value.len(), size);
        if let Some((pointer, _)) = payload {
            assert_eq!(state.bytes.value.as_ptr() as usize, pointer);
        }
        let mut actual_sum = 0_u64;
        let mut expected_sum = 0_u64;
        for (j, byte) in state.bytes.value.iter().enumerate() {
            let expected =
                u8::try_from(pairs.checked_add(u64::try_from(j).unwrap()).unwrap() % 251).unwrap();
            assert_eq!(*byte, expected);
            actual_sum = actual_sum.checked_add(u64::from(*byte)).unwrap();
            expected_sum = expected_sum.checked_add(u64::from(expected)).unwrap();
        }
        assert_eq!(actual_sum, expected_sum);
        if let Some((_, checksum)) = payload {
            assert_eq!(actual_sum, checksum);
        }
    }))?
    .map_err(|_| "TestStore borrowed final state validation failed")?;
    measurement.validation_finished();
    poll_test_operation(store.finish())?.map_err(|_| "TestStore exhaustive finish failed")?;
    drop(store);
    let mut facts = Facts {
        counts: probe.counts(),
        byte_checksum: payload
            .map_or_else(|| expected_byte_checksum(size, 0), |(_, checksum)| checksum),
        byte_len: config.size,
        bytes_verified: true,
        owners_released: reducer_weak.upgrade().is_none(),
    };
    drop(reducer_weak);
    drop(probe);
    facts.owners_released &= probe_weak.upgrade().is_none();
    drop(probe_weak);
    facts.validate(config)?;
    let allocations = measurement
        .finish()?
        .map_or_else(|| "null".to_owned(), |record| record.json());
    Ok(format!(
        "{{\"schema\":\"rca-teststore-performance-profile-v1\",\"workload\":\"{}\",\"size\":{},\"iterations\":{},\"outcome\":\"{}\",\"drive_contract\":\"teststore-operation-ready-v1\",\"scheduler_budget_per_drive\":4096,\"outer_poll_cap_per_operation\":1024,\"feature_profile\":\"teststore-deterministic-diagnostics-test-util\",\"features\":{{\"deterministic\":true,\"allocations\":{}}},\"facts\":{},\"owners_released\":{},\"allocations\":{}}}",
        config.name,
        config.size,
        config.iterations,
        if config.pairs() == 0 {
            "idle-no-actions"
        } else {
            "exhaustive-owned-pairs"
        },
        cfg!(feature = "allocations"),
        facts.json(),
        facts.owners_released,
        allocations
    ))
}

/// Counts one external Response, no effect response and no completed boundary.
const FAILURE_COUNTS: [u64; 22] = [
    0, 1, 1, 0, 1, 0, 1, 0, 0, 3, 1, 3, 2, 2, 1, 1, 1, 6, 6, 6, 6, 2,
];
/// Records the failed boundary while the Store and assertion panic remain alive.
const FAILURE_LIVE_COUNTS: [u64; 22] = [
    // The capture batch owns its expected and actual revisions. Root mismatch
    // drops both, retaining only the preflight baseline and the debt marker.
    0, 1, 1, 0, 1, 0, 1, 0, 0, 3, 1, 3, 2, 2, 1, 1, 1, 6, 3, 6, 5, 2,
];

#[inline(never)]
/// Retains the actual assertion panic after one deliberately incorrect expected byte.
fn profile_teststore_mismatch(
    store: &mut TestStore<State, Action>,
    size: usize,
    probe: &Arc<Probe>,
) -> Result<Box<dyn std::any::Any + Send>, &'static str> {
    probe.add(ACTIONS, 1);
    let action = Action::Response {
        sequence: 1,
        bytes: Cell::new(Some(OwnedBytes::new(size, 1, probe))),
        probe: Arc::clone(probe),
    };
    // Keep the store outside this catch. Unwind only the first failed operation.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        poll_test_operation(store.send_with_captures(
            action,
            |state| {
                probe.add(UPDATES, 1);
                state.responses = 1;
                state.last_sequence = 1;
                state.bytes = OwnedBytes::new(size, 1, probe);
                *state.bytes.value.last_mut().unwrap() ^= 1;
            },
            |captures| {
                probe.add(SETS, 1);
                captures.set::<RevisionCapture>(OwnedRevision::new(0, 1, probe))
            },
        ))
    }))
    .err()
    .ok_or("TestStore mismatch did not panic")
}

/// Checks sticky failure before disposing the failed Store and its retained panic.
fn run_mismatch(config: Config) -> Result<String, &'static str> {
    let mut measurement = Measurement::new(None);
    let probe = Probe::new();
    let probe_weak = Arc::downgrade(&probe);
    let (mut store, reducer_weak) = fixture(config.size, STEP_BUDGET, &probe);
    measurement.construction_finished();
    let mut panic = None;
    let mut error = None;
    let size = config.size;
    let validation = (|| -> Result<(), &'static str> {
        panic = Some(profile_teststore_mismatch(&mut store, config.size, &probe)?);
        // The payload, store and both assertion obligations remain live here.
        measurement.work_finished();
        #[cfg(test)]
        tests::inject_mismatch_failure(&probe, &reducer_weak);
        let message = panic
            .as_ref()
            .unwrap()
            .downcast_ref::<String>()
            .ok_or("unexpected mismatch panic payload")?;
        if !message.contains("TestStore state mismatch")
            || !message.contains("full-state coverage")
            || probe.counts() != FAILURE_LIVE_COUNTS
            || reducer_weak.upgrade().is_none()
        {
            return Err("TestStore failure-return facts or live owners mismatch");
        }
        poll_test_operation(store.assert_state(move |state| {
            #[cfg(test)]
            tests::inject_borrowed_validation_unwind();
            assert_eq!(
                (state.roots, state.responses, state.last_sequence),
                (0, 1, 1)
            );
            assert_eq!(state.bytes.value.len(), size);
            for (j, byte) in state.bytes.value.iter().enumerate() {
                assert_eq!(*byte, u8::try_from((1 + j) % 251).unwrap());
            }
            assert_eq!(
                state.bytes.value.iter().map(|b| u64::from(*b)).sum::<u64>(),
                expected_byte_checksum(size, 1)
            );
        }))?
        .map_err(|_| "TestStore failed-state borrowed validation failed")?;
        error = Some(
            poll_test_operation(store.finish())?
                .map_or_else(Ok, |()| Err("failed assertion became complete"))?,
        );
        if !error.as_ref().unwrap().is_incomplete_assertion()
            || probe.counts() != FAILURE_LIVE_COUNTS
        {
            return Err("TestStore mismatch obligation was not sticky");
        }
        measurement.validation_finished();
        Ok(())
    })();
    // This second unwind reports deliberate incomplete teardown, not recovery.
    let disposal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)));
    // Preserve a primary Result rejection after controlled sticky-store disposal.
    validation?;
    let disposal = disposal
        .err()
        .ok_or("TestStore failed Drop did not unwind")?;
    let disposal_message = disposal
        .downcast_ref::<String>()
        .ok_or("unexpected Drop panic payload")?;
    if !disposal_message.contains("TestStore teardown failed")
        || !disposal_message.contains("incomplete")
    {
        return Err("unexpected TestStore disposal failure");
    }
    drop((panic, error, disposal));
    let mut facts = Facts {
        counts: probe.counts(),
        byte_checksum: expected_byte_checksum(size, 1),
        byte_len: size,
        bytes_verified: true,
        owners_released: reducer_weak.upgrade().is_none(),
    };
    drop((reducer_weak, probe));
    facts.owners_released &= probe_weak.upgrade().is_none();
    drop(probe_weak);
    facts.validate(config)?;
    let allocations = measurement
        .finish()?
        .map_or_else(|| "null".to_owned(), |record| record.json());
    Ok(format!(
        "{{\"schema\":\"rca-teststore-failure-profile-v1\",\"workload\":\"teststore-mismatch\",\"size\":{size},\"iterations\":1,\"outcome\":\"expected-state-mismatch\",\"phase_semantics\":{{\"work\":\"failure-return\"}},\"drive_contract\":\"teststore-operation-ready-v1\",\"scheduler_budget_per_drive\":4096,\"outer_poll_cap_per_operation\":1024,\"feature_profile\":\"teststore-deterministic-diagnostics-test-util\",\"features\":{{\"deterministic\":true,\"allocations\":{}}},\"commits\":1,\"effect_responses\":0,\"failure_live\":{{\"bytes\":3,\"revisions\":1}},\"panic_retained_at_failure\":true,\"store_retained_at_failure\":true,\"sticky_incomplete\":true,\"expected_drop_unwind\":true,\"facts\":{},\"owners_released\":{},\"allocations\":{}}}",
        cfg!(feature = "allocations"),
        facts.json(),
        facts.owners_released,
        allocations
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    //! Challenges real failure, ownership and poll-budget boundaries in isolated children.

    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::pin::Pin;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::task::{Context, Poll};

    /// Checks cleanup after rejection without keeping measured owners alive.
    struct FailureWitnesses {
        /// Must expire after the failed workload leaves its measurement scope.
        probe: Weak<Probe>,
        /// Must expire when the failed Store destroys its reducer.
        reducer: Weak<()>,
    }
    std::thread_local! {
        // 1 corrupts the actual guard fact; 2 panics in the actual borrowed read.
        static MISMATCH_FAULT: Cell<u8> = const { Cell::new(0) };
        static MISMATCH_WITNESSES: std::cell::RefCell<Option<FailureWitnesses>> = const { std::cell::RefCell::new(None) };
    }
    /// Distinguishes the injected validation panic from teardown failures.
    struct ValidationFault;

    /// Corrupts an actual scalar fact or records owners before the unwind control.
    pub(super) fn inject_mismatch_failure(probe: &Arc<Probe>, reducer: &Weak<()>) {
        let fault = MISMATCH_FAULT.get();
        if fault != 0 {
            MISMATCH_WITNESSES.set(Some(FailureWitnesses {
                probe: Arc::downgrade(probe),
                reducer: reducer.clone(),
            }));
        }
        if fault == 1 {
            probe.add(REDUCTIONS, 1);
        }
    }

    /// Unwinds inside the real borrowed validation callback when requested.
    pub(super) fn inject_borrowed_validation_unwind() {
        if MISMATCH_FAULT.get() == 2 {
            std::panic::panic_any(ValidationFault);
        }
    }

    /// Requires the original rejection, released owners and restored panic hook.
    fn check_mismatch_rejection(fault: u8) {
        let previous = std::panic::take_hook();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        std::panic::set_hook(Box::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
        MISMATCH_FAULT.set(fault);
        let config = Config::parse(
            ["teststore-mismatch", "8", "1"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| run(config)));
        MISMATCH_FAULT.set(0);
        let witnesses = MISMATCH_WITNESSES.take().unwrap();
        let owners_released =
            witnesses.probe.upgrade().is_none() && witnesses.reducer.upgrade().is_none();
        drop(witnesses);
        let sentinel = catch_unwind(|| panic!("restored rejection hook sentinel"));
        let hook_calls = calls.load(Ordering::SeqCst);
        std::panic::set_hook(previous);
        assert!(owners_released, "rejected mismatch retained owners");
        assert!(sentinel.is_err());
        if fault == 1 {
            assert!(
                matches!(
                    result,
                    Ok(Err(
                        "TestStore failure-return facts or live owners mismatch"
                    ))
                ),
                "primary rejection was replaced or a success record emitted"
            );
        } else {
            assert!(
                matches!(result, Err(ref payload) if payload.is::<ValidationFault>()),
                "original validation unwind was replaced or a success record emitted"
            );
        }
        assert_eq!(hook_calls, 1, "rejected mismatch failed to restore hook");
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn mismatch_guard_rejection_preserves_error_owners_and_hook() {
        if in_control_child(
            "teststore::tests::mismatch_guard_rejection_preserves_error_owners_and_hook",
        ) {
            check_mismatch_rejection(1);
        }
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn mismatch_validation_unwind_preserves_payload_owners_and_hook() {
        if in_control_child(
            "teststore::tests::mismatch_validation_unwind_preserves_payload_owners_and_hook",
        ) {
            check_mismatch_rejection(2);
        }
    }

    /// Requires exactly one successful isolated test without leaving temporary files.
    fn in_control_child(name: &str) -> bool {
        crate::tests::in_child(name)
    }

    #[test]
    #[should_panic(expected = "did not pass exactly once")]
    fn control_child_rejects_empty_selection() {
        in_control_child("teststore::tests::missing_control");
    }

    /// Commits one real root with an independently declared scalar capture.
    fn send_one(
        store: &mut TestStore<State, Action>,
        probe: &Arc<Probe>,
    ) -> rca_test::TestActionTask {
        poll_test_operation(store.send_with_captures(
            Action::start(1, OwnedBytes::new(8, 1, probe), probe),
            |state| {
                probe.add(UPDATES, 1);
                state.roots = 1;
            },
            |captures| {
                probe.add(SETS, 1);
                captures.set::<RevisionCapture>(OwnedRevision::new(1, 0, probe))
            },
        ))
        .unwrap()
        .unwrap()
    }

    /// Receives the queued response and constructs its expected payload independently.
    fn receive_one(
        store: &mut TestStore<State, Action>,
        probe: &Arc<Probe>,
    ) -> rca_test::TestActionTask {
        poll_test_operation(store.receive_with_captures(
            |action| matches!(action, Action::Response { sequence: 1, .. }),
            |state| {
                probe.add(UPDATES, 1);
                state.responses = 1;
                state.last_sequence = 1;
                state.bytes = OwnedBytes::new(8, 1, probe);
            },
            |captures| {
                probe.add(SETS, 1);
                captures.set::<RevisionCapture>(OwnedRevision::new(1, 1, probe))
            },
        ))
        .unwrap()
        .unwrap()
    }

    /// Requires every counted action, payload and capture to be destroyed.
    fn balanced(probe: &Arc<Probe>, reducer: Weak<()>) {
        let counts = probe.counts();
        assert_eq!(counts[BYTES], counts[BYTE_DROPS]);
        assert_eq!(counts[ACTIONS], counts[ACTION_DROPS]);
        assert_eq!(counts[REVISIONS], counts[REV_DROPS]);
        assert!(reducer.upgrade().is_none());
        drop(reducer);
    }

    /// Requires the intentional teardown failure, not an unrelated destructor panic.
    fn drop_failed_store(store: TestStore<State, Action>) {
        let panic = catch_unwind(AssertUnwindSafe(|| drop(store)))
            .expect_err("an unfinished Store must reject teardown");
        let message = panic
            .downcast_ref::<String>()
            .expect("teardown failure has a formatted diagnostic");
        assert!(
            message.starts_with("TestStore teardown failed:"),
            "unexpected teardown panic: {message}"
        );
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn success_and_idle_keep_renderer_lazy_and_balance_owned_values() {
        if !in_control_child(
            "teststore::tests::success_and_idle_keep_renderer_lazy_and_balance_owned_values",
        ) {
            return;
        }
        for name in ["teststore-owned", "teststore-idle"] {
            let config = Config::parse([name, "8", "1"].into_iter().map(str::to_owned)).unwrap();
            let record = run(config).unwrap();
            let record: serde_json::Value = serde_json::from_str(&record).unwrap();
            assert_eq!(record["facts"]["renderer_calls"], 0);
            assert_eq!(
                record["facts"]["bytes_created"],
                if name == "teststore-idle" { 2 } else { 8 }
            );
            assert_eq!(record["owners_released"], true);
        }
    }

    #[test]
    fn unpolled_send_drops_payload_and_callback_guard_without_assertion_work() {
        if !in_control_child(
            "teststore::tests::unpolled_send_drops_payload_and_callback_guard_without_assertion_work",
        ) {
            return;
        }
        let probe = Probe::new();
        let (mut store, reducer) = fixture(8, STEP_BUDGET, &probe);
        let guard = Arc::new(());
        let weak = Arc::downgrade(&guard);
        let future = store.send_with_captures(
            Action::start(1, OwnedBytes::new(8, 1, &probe), &probe),
            move |_| drop(guard),
            |_| Ok(()),
        );
        drop(future);
        assert!(weak.upgrade().is_none());
        let counts = probe.counts();
        assert_eq!(
            (
                counts[ROOTS],
                counts[REDUCTIONS],
                counts[STATE_CLONES],
                counts[CAPTURES],
                counts[REV_CLONES]
            ),
            (0, 0, 1, 1, 0)
        );
        assert_eq!((counts[ACTIONS], counts[ACTION_DROPS]), (1, 1));
        poll_test_operation(store.finish()).unwrap().unwrap();
        drop(store);
        balanced(&probe, reducer);
    }

    #[test]
    fn matcher_and_gated_send_reject_before_new_capture_or_expected_updates() {
        if !in_control_child(
            "teststore::tests::matcher_and_gated_send_reject_before_new_capture_or_expected_updates",
        ) {
            return;
        }
        let probe = Probe::new();
        let (mut store, reducer) = fixture(8, STEP_BUDGET, &probe);
        let send = send_one(&mut store, &probe);
        let before = probe.counts();
        let error = poll_test_operation(store.receive_with_captures(
            |_| false,
            |_| panic!("rejected matcher ran update"),
            |_| panic!("rejected matcher ran captures"),
        ))
        .unwrap()
        .unwrap_err();
        assert!(error.is_action_mismatch());
        assert_eq!(probe.counts(), before);
        drop(error);
        let error = poll_test_operation(store.send_with_captures(
            Action::start(2, OwnedBytes::new(8, 2, &probe), &probe),
            |_| panic!("gated send ran update"),
            |_| panic!("gated send ran captures"),
        ))
        .unwrap()
        .unwrap_err();
        assert!(error.is_unreceived_action());
        for index in [
            ROOTS,
            RESPONSES,
            REDUCTIONS,
            UPDATES,
            SETS,
            CAPTURES,
            STATE_CLONES,
            STATE_EQ,
            REV_CLONES,
            REV_EQ,
        ] {
            assert_eq!(probe.counts()[index], before[index]);
        }
        drop(error);
        let receive = receive_one(&mut store, &probe);
        for task in [&send, &receive] {
            poll_test_operation(task.finish()).unwrap().unwrap();
            assert!(task.is_finished() && !task.is_cancelled());
        }
        drop((send, receive));
        poll_test_operation(store.finish()).unwrap().unwrap();
        drop(store);
        balanced(&probe, reducer);
    }

    #[test]
    fn omitted_receive_is_unfinished_and_failed_drop_releases_pending_owners() {
        if !in_control_child(
            "teststore::tests::omitted_receive_is_unfinished_and_failed_drop_releases_pending_owners",
        ) {
            return;
        }
        let probe = Probe::new();
        let (mut store, reducer) = fixture(8, STEP_BUDGET, &probe);
        let task = send_one(&mut store, &probe);
        let error = poll_test_operation(store.finish()).unwrap().unwrap_err();
        assert!(error.finish_diagnostics().is_some());
        assert!(!task.is_finished());
        drop_failed_store(store);
        drop((task, error));
        balanced(&probe, reducer);
    }

    #[test]
    fn omitted_declaration_is_typed_sticky_and_does_not_render_root() {
        if !in_control_child(
            "teststore::tests::omitted_declaration_is_typed_sticky_and_does_not_render_root",
        ) {
            return;
        }
        let probe = Probe::new();
        let (mut store, reducer) = fixture(8, STEP_BUDGET, &probe);
        let error = poll_test_operation(store.send_with_captures(
            Action::start(1, OwnedBytes::new(8, 1, &probe), &probe),
            |state| state.roots = 1,
            |_| Ok(()),
        ))
        .unwrap()
        .unwrap_err();
        assert!(error.is_capture_mismatch());
        assert_eq!(probe.counts()[ROOTS], 1);
        assert_eq!(probe.counts()[STATE_EQ], 0);
        assert_eq!(probe.counts()[RENDERERS], 0);
        let finish = poll_test_operation(store.finish()).unwrap().unwrap_err();
        assert!(finish.is_incomplete_assertion());
        drop_failed_store(store);
        drop((error, finish));
        balanced(&probe, reducer);
    }

    #[test]
    fn semantic_root_mismatch_stays_sticky_even_with_identical_rendering() {
        if !in_control_child(
            "teststore::tests::semantic_root_mismatch_stays_sticky_even_with_identical_rendering",
        ) {
            return;
        }
        for corrupt in 0..4 {
            let probe = Probe::new();
            let (mut store, reducer) =
                fixture_with_renderer(8, STEP_BUDGET, &probe, |_| "opaque state".to_owned());
            probe.add(ACTIONS, 1);
            let action = Action::Response {
                sequence: 1,
                bytes: Cell::new(Some(OwnedBytes::new(8, 1, &probe))),
                probe: Arc::clone(&probe),
            };
            let panic = catch_unwind(AssertUnwindSafe(|| {
                poll_test_operation(store.send_with_captures(
                    action,
                    |state| {
                        state.responses = 1;
                        state.last_sequence = 1;
                        state.bytes = OwnedBytes::new(8, 1, &probe);
                        match corrupt {
                            0 => state.roots = 1,
                            1 => state.responses = 0,
                            2 => state.last_sequence = 0,
                            _ => state.bytes.value[7] ^= 1,
                        }
                    },
                    |captures| captures.set::<RevisionCapture>(OwnedRevision::new(0, 1, &probe)),
                ))
            }))
            .unwrap_err();
            let message = panic.downcast_ref::<String>().unwrap();
            assert!(message.contains("TestStore state mismatch"));
            assert!(message.contains("Snapshots differ semantically"));
            assert_eq!(probe.counts()[RENDERERS], 2);
            let finish = poll_test_operation(store.finish()).unwrap().unwrap_err();
            assert!(finish.is_incomplete_assertion());
            drop_failed_store(store);
            drop((panic, finish));
            balanced(&probe, reducer);
        }
    }

    #[test]
    fn scheduler_budget_error_is_distinct_from_outer_poller_rejection() {
        if !in_control_child(
            "teststore::tests::scheduler_budget_error_is_distinct_from_outer_poller_rejection",
        ) {
            return;
        }
        let probe = Probe::new();
        let (mut store, reducer) = fixture(8, 1, &probe);
        let error = poll_test_operation(store.send_with_captures(
            Action::start(1, OwnedBytes::new(8, 1, &probe), &probe),
            |state| state.roots = 1,
            |captures| captures.set::<RevisionCapture>(OwnedRevision::new(1, 0, &probe)),
        ))
        .unwrap()
        .unwrap_err();
        assert!(error.drive_error().unwrap().is_step_budget_exhausted());
        let finish = poll_test_operation(store.finish()).unwrap().unwrap_err();
        assert!(finish.is_incomplete_assertion() || finish.drive_error().is_some());
        drop_failed_store(store);
        drop((error, finish));
        balanced(&probe, reducer);
    }

    #[cfg(feature = "allocations")]
    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn allocation_endpoint_rejects_retained_task_weak_and_probe_owners() {
        if !in_control_child(
            "teststore::tests::allocation_endpoint_rejects_retained_task_weak_and_probe_owners",
        ) {
            return;
        }
        for retained in 0..3 {
            let mut measurement = Measurement::new(None);
            let probe = Probe::new();
            let weak = Arc::downgrade(&probe);
            let (mut store, reducer) = fixture(8, STEP_BUDGET, &probe);
            measurement.construction_finished();
            let send = send_one(&mut store, &probe);
            let receive = receive_one(&mut store, &probe);
            for task in [&send, &receive] {
                poll_test_operation(task.finish()).unwrap().unwrap();
            }
            drop(receive);
            measurement.work_finished();
            measurement.validation_finished();
            poll_test_operation(store.finish()).unwrap().unwrap();
            drop(store);
            drop(reducer);
            let task = if retained == 0 {
                Some(send)
            } else {
                drop(send);
                None
            };
            let owner = if retained == 2 {
                Some(probe)
            } else {
                drop(probe);
                None
            };
            let witness = if retained == 1 {
                Some(weak)
            } else {
                drop(weak);
                None
            };
            assert!(measurement.finish().is_err());
            drop((task, owner, witness));
        }
    }

    /// Exposes exact outer-poll, wake and destruction counts without a scheduler.
    struct Controlled {
        /// Counts each poll of this same pinned future.
        polls: Arc<AtomicUsize>,
        /// Requires rejection or completion to destroy the future exactly once.
        drops: Arc<AtomicUsize>,
        /// Requests another poll only for the wake-aware control.
        wakes: bool,
        /// Selects the successful poll; zero means that no poll completes.
        ready_at: usize,
    }
    impl Future for Controlled {
        type Output = usize;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<usize> {
            let count = self.polls.fetch_add(1, Ordering::SeqCst) + 1;
            if count == self.ready_at {
                return Poll::Ready(count);
            }
            if self.wakes {
                cx.waker().wake_by_ref();
            }
            Poll::Pending
        }
    }
    impl Drop for Controlled {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn semantic_oracle_rejects_each_corrupt_count_bytes_and_owner() {
        let config = Config {
            name: "teststore-owned",
            size: 8,
            iterations: 1,
        };
        let valid = Facts {
            counts: [
                1, 1, 1, 1, 2, 1, 2, 2, 2, 5, 2, 6, 5, 5, 2, 2, 2, 8, 8, 13, 13, 0,
            ],
            byte_checksum: 36,
            byte_len: 8,
            bytes_verified: true,
            owners_released: true,
        };
        valid.validate(config).unwrap();
        for index in 0..22 {
            let mut broken = valid;
            broken.counts[index] += 1;
            assert!(
                broken.validate(config).is_err(),
                "accepted corruption at {index}"
            );
        }
        assert!(
            Facts {
                bytes_verified: false,
                ..valid
            }
            .validate(config)
            .is_err()
        );
        assert!(
            Facts {
                owners_released: false,
                ..valid
            }
            .validate(config)
            .is_err()
        );
        assert!(
            Facts {
                byte_checksum: 37,
                ..valid
            }
            .validate(config)
            .is_err()
        );
        assert!(
            Facts {
                byte_len: 7,
                ..valid
            }
            .validate(config)
            .is_err()
        );
    }

    #[test]
    fn failure_oracle_accepts_only_exact_mismatch_and_balanced_owners() {
        let config = Config {
            name: "teststore-mismatch",
            size: 8,
            iterations: 1,
        };
        let valid = Facts {
            counts: [
                0, 1, 1, 0, 1, 0, 1, 0, 0, 3, 1, 3, 2, 2, 1, 1, 1, 6, 6, 6, 6, 2,
            ],
            byte_checksum: 36,
            byte_len: 8,
            bytes_verified: true,
            owners_released: true,
        };
        valid.validate(config).unwrap();
        for index in 0..22 {
            let mut broken = valid;
            broken.counts[index] += 1;
            assert!(
                broken.validate(config).is_err(),
                "accepted corruption at {index}"
            );
        }
        for broken in [
            Facts {
                byte_checksum: 37,
                ..valid
            },
            Facts {
                byte_len: 7,
                ..valid
            },
            Facts {
                bytes_verified: false,
                ..valid
            },
            Facts {
                owners_released: false,
                ..valid
            },
        ] {
            assert!(broken.validate(config).is_err());
        }
    }

    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn mismatch_cleanup_restores_child_panic_hook() {
        if !in_control_child("teststore::tests::mismatch_cleanup_restores_child_panic_hook") {
            return;
        }
        let hook = std::panic::take_hook();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        std::panic::set_hook(Box::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }));
        let config = Config::parse(
            ["teststore-mismatch", "8", "1"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        let record = run(config).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let sentinel = catch_unwind(|| panic!("restored hook sentinel"));
        assert!(sentinel.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        std::panic::set_hook(hook);
        let record: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert_eq!(record["sticky_incomplete"], true);
        assert_eq!(record["expected_drop_unwind"], true);
        assert_eq!(record["owners_released"], true);
    }

    #[cfg(feature = "allocations")]
    #[cfg_attr(not(feature = "allocations"), test)]
    pub(crate) fn failure_allocation_endpoint_rejects_retained_panic_payload() {
        if !in_control_child(
            "teststore::tests::failure_allocation_endpoint_rejects_retained_panic_payload",
        ) {
            return;
        }
        let mut measurement = Measurement::new(None);
        let probe = Probe::new();
        let (mut store, reducer) = fixture(8, STEP_BUDGET, &probe);
        measurement.construction_finished();
        let panic = profile_teststore_mismatch(&mut store, 8, &probe).unwrap();
        measurement.work_finished();
        assert_eq!(probe.counts(), FAILURE_LIVE_COUNTS);
        let error = poll_test_operation(store.finish()).unwrap().unwrap_err();
        assert!(error.is_incomplete_assertion());
        measurement.validation_finished();
        let disposal = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
        drop((error, disposal));
        balanced(&probe, reducer);
        drop(probe);
        assert!(measurement.finish().is_err());
        drop(panic);
    }

    #[test]
    fn operation_poller_preserves_one_future_and_releases_rejections() {
        for (wakes, ready_at, want, count) in [
            (
                false,
                0,
                Err("TestStore operation returned unwoken Pending"),
                1,
            ),
            (
                true,
                0,
                Err("TestStore operation outer poll budget exhausted"),
                1024,
            ),
            (true, 1024, Ok(1024), 1024),
            (false, 1, Ok(1), 1),
        ] {
            let polls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let future = Controlled {
                polls: Arc::clone(&polls),
                drops: Arc::clone(&drops),
                wakes,
                ready_at,
            };
            assert_eq!(poll_test_operation(future), want);
            assert_eq!(polls.load(Ordering::SeqCst), count);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
    }
}
