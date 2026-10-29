//! Profiles exact-byte comparison and ordered capture with a fixed polling schedule.
//!
//! Byte buffers and case names are constructed before useful work. Ordered
//! batches include their endpoint, factory, future and value allocations in work.
//! Direct polling needs no Store or executor. Counters include the fixture's
//! own work and do not represent general executor scheduling costs.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::task::{Context, Poll, Wake, Waker};

use rca_test::snapshots::{SnapshotNamespace, capture_ordered};

use crate::measurement::Measurement;

/// Holds a validated workload name and its bounded dimensions.
#[derive(Clone, Copy)]
pub(crate) struct Config {
    /// Selects byte comparison or ordered capture from the fixed CLI names.
    name: &'static str,
    /// Counts bytes per comparison or cases per ordered batch.
    size: usize,
    /// Counts independent comparisons or ordered batches.
    iterations: u64,
}

impl Config {
    /// Rejects unknown names, extra arguments and dimensions outside the finite matrix.
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let name = match args.next().as_deref() {
            Some("snapshot-bytes") => "snapshot-bytes",
            Some("snapshot-ordered") => "snapshot-ordered",
            _ => return Err("unknown snapshot workload"),
        };
        let size = decimal(args.next())?;
        let iterations = decimal(args.next())?;
        if args.next().is_some() {
            return Err("unexpected snapshot argument");
        }
        let valid = if name == "snapshot-bytes" {
            matches!(size, 0 | 8 | 4096 | 65536) && (1..=256).contains(&iterations)
        } else {
            matches!(size, 0 | 1 | 16 | 64) && (1..=16).contains(&iterations)
        };
        if !valid {
            return Err("snapshot dimensions out of bounds");
        }
        Ok(Self {
            name,
            size: usize::try_from(size).map_err(|_| "snapshot size overflow")?,
            iterations,
        })
    }
}

/// Reads unsigned ASCII decimal input without signs, whitespace or overflow.
fn decimal(value: Option<String>) -> Result<u64, &'static str> {
    let value = value.ok_or("missing snapshot dimension")?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("snapshot dimensions must be unsigned decimal integers");
    }
    value.parse().map_err(|_| "snapshot dimension overflow")
}

/// Names the first thirteen probe slots in their JSON and oracle order.
const COUNT_NAMES: [&str; 13] = [
    "outer_polls",
    "timeout_polls",
    "case_polls",
    "factory_entries",
    "factories_created",
    "factories_dropped",
    "futures_created",
    "futures_dropped",
    "values_created",
    "values_dropped",
    "wakes",
    "finish_calls",
    "finish_owners_dropped",
];
/// Records actual callback invocations separately from endpoint wake attempts.
const WAKER_CALLBACKS: usize = 13;
/// Accumulates the byte sum after each ordered result passes its association check.
const DIGEST: usize = 14;

/// Shares thirteen work counters, one wake callback count and one byte digest.
///
/// Numeric slots follow `COUNT_NAMES`, then `WAKER_CALLBACKS` and `DIGEST`.
/// Accepted CLI dimensions keep every total within `u64`.
struct Probe([AtomicU64; 15]);

impl Probe {
    /// Starts all observed work and ownership counts at zero.
    fn new() -> Arc<Self> {
        Arc::new(Self(std::array::from_fn(|_| AtomicU64::new(0))))
    }

    /// Adds an observed event to its fixed slot.
    fn add(&self, index: usize, value: u64) {
        self.0[index].fetch_add(value, Ordering::SeqCst);
    }

    /// Reads one observed event total without resetting it.
    fn get(&self, index: usize) -> u64 {
        self.0[index].load(Ordering::SeqCst)
    }

    /// Copies only the work counters into the checked report.
    fn counts(&self) -> [u64; 13] {
        std::array::from_fn(|index| self.get(index))
    }
}

impl Wake for Probe {
    fn wake(self: Arc<Self>) {
        self.add(WAKER_CALLBACKS, 1);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.add(WAKER_CALLBACKS, 1);
    }
}

/// Counts destruction of one factory, future, value or finish owner.
struct Owner {
    /// Keeps the counters alive until this owner is destroyed.
    probe: Arc<Probe>,
    /// Selects the matching destruction counter in `COUNT_NAMES`.
    dropped: usize,
}

impl Owner {
    /// Counts creation now and destruction when the returned owner drops.
    fn new(probe: &Arc<Probe>, created: usize, dropped: usize) -> Self {
        probe.add(created, 1);
        Self {
            probe: Arc::clone(probe),
            dropped,
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.probe.add(self.dropped, 1);
    }
}

/// Associates an ordered case index with its separately counted payload owner.
struct Value {
    /// Encodes the case index as eight little-endian bytes.
    bytes: [u8; 8],
    /// Records destruction after capture, result checking or cancellation.
    _owner: Owner,
}

/// Holds one case's externally supplied value and latest polling waker.
struct Slot {
    /// Prevents the controlled sender from filling a case after its future drops.
    alive: bool,
    /// Transfers one payload from the controlled sender to the capture future.
    value: Option<Value>,
    /// Receives the next endpoint wake, then leaves the slot before invocation.
    waker: Option<Waker>,
}

/// Waits for one manually supplied payload without an executor or channel.
struct Case {
    /// Shares the endpoint with the controlled completion loop.
    slot: Rc<RefCell<Slot>>,
    /// Counts polls and destruction of this future.
    owner: Owner,
}

impl Future for Case {
    type Output = Result<Value, ()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.owner.probe.add(2, 1);
        let mut slot = self.slot.borrow_mut();
        if let Some(value) = slot.value.take() {
            Poll::Ready(Ok(value))
        } else {
            slot.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        let mut slot = self.slot.borrow_mut();
        slot.alive = false;
        slot.value = None;
        slot.waker = None;
    }
}

/// Counts deadline polls without expiring during successful capture batches.
struct Timeout(Arc<Probe>);

impl Future for Timeout {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        self.0.add(1, 1);
        Poll::Pending
    }
}

/// Checks every result's input position and releases all returned payloads.
///
/// The byte sum is secondary to exact length and byte checks. Missing or
/// reordered zero-valued cases cannot pass merely because their sums agree.
fn finish_values(values: Vec<Value>, expected: usize) -> Result<u64, &'static str> {
    if values.len() != expected {
        return Err("ordered snapshot value count mismatch");
    }
    let mut digest = 0_u64;
    for (index, value) in values.iter().enumerate() {
        if value.bytes != u64::try_from(index).unwrap().to_le_bytes() {
            return Err("ordered snapshot bytes or association mismatch");
        }
        for byte in value.bytes {
            digest = digest
                .checked_add(u64::from(byte))
                .ok_or("snapshot digest overflow")?;
        }
    }
    drop(values);
    Ok(digest)
}

/// Checks observed work and destruction counts for the reverse-completion schedule.
///
/// Each batch polls all cases once, then the remaining cases after each value.
/// This gives `cases * (cases + 3) / 2` case polls, not a general cost guarantee.
fn validate_counts(actual: &[u64; 13], cases: u64, batches: u64) -> Result<(), &'static str> {
    let values = cases
        .checked_mul(batches)
        .ok_or("snapshot count overflow")?;
    let outer = cases
        .checked_add(1)
        .and_then(|n| n.checked_mul(batches))
        .ok_or("snapshot count overflow")?;
    let polls = cases
        .checked_mul(cases.checked_add(3).ok_or("snapshot count overflow")?)
        .and_then(|n| n.checked_div(2))
        .and_then(|n| n.checked_mul(batches))
        .ok_or("snapshot count overflow")?;
    let expected = [
        outer, outer, polls, values, values, values, values, values, values, values, values,
        batches, batches,
    ];
    if *actual != expected {
        return Err("ordered snapshot count or owner mismatch");
    }
    Ok(())
}

/// Measures compare-only calls under distinct names in one real snapshot namespace.
///
/// Buffer construction, full byte checks and namespace destruction remain outside
/// this function. Name reservation by `compare` remains inside the measured work.
#[inline(never)]
fn profile_snapshot_bytes(
    namespace: &mut SnapshotNamespace,
    names: &[String],
    reference: &[u8],
    actual: &[u8],
) -> Result<u64, &'static str> {
    let mut comparisons = 0_u64;
    for name in names {
        namespace
            .compare(name, Some(reference), actual)
            .map_err(|_| "byte snapshot comparison failed")?;
        comparisons = comparisons
            .checked_add(1)
            .ok_or("snapshot count overflow")?;
    }
    Ok(comparisons)
}

/// Captures each batch in reverse completion order and checks input-order results.
///
/// Endpoint, factory, future, value and result allocations stay inside useful work.
/// Finish checks and drops each batch's values before the next batch starts.
#[inline(never)]
fn profile_ordered_capture(config: Config, probe: &Arc<Probe>) -> Result<(), &'static str> {
    // All per-batch endpoints, factory/pending/value/result vectors and Boxes
    // deliberately stay in useful work: their allocation is an R14 target.
    for _ in 0..config.iterations {
        let mut endpoints = Vec::with_capacity(config.size);
        let mut cases = Vec::with_capacity(config.size);
        for _ in 0..config.size {
            let slot = Rc::new(RefCell::new(Slot {
                alive: true,
                value: None,
                waker: None,
            }));
            endpoints.push(Rc::clone(&slot));
            let owner = Owner::new(probe, 4, 5);
            let probe = Arc::clone(probe);
            cases.push(move || {
                probe.add(3, 1);
                drop(owner);
                Case {
                    slot,
                    owner: Owner::new(&probe, 6, 7),
                }
            });
        }
        // Finish has one owner even for empty batches; no result history survives.
        let finish_owner = Owner {
            probe: Arc::clone(probe),
            dropped: 12,
        };
        let mut future = std::pin::pin!(capture_ordered(
            cases,
            Timeout(Arc::clone(probe)),
            move |values| {
                finish_owner.probe.add(11, 1);
                let digest = finish_values(values, config.size)?;
                finish_owner.probe.add(DIGEST, digest);
                drop(finish_owner);
                Ok(())
            }
        ));
        let waker = Waker::from(Arc::clone(probe));
        probe.add(0, 1);
        let first = future.as_mut().poll(&mut Context::from_waker(&waker));
        if config.size == 0 {
            match first {
                Poll::Ready(Ok(result)) => result?,
                _ => return Err("empty ordered snapshot did not finish"),
            }
        } else {
            if first.is_ready() {
                return Err("ordered snapshot completed before input");
            }
            for index in (0..config.size).rev() {
                let endpoint = &endpoints[index];
                let wake = {
                    let mut slot = endpoint.borrow_mut();
                    if !slot.alive || slot.value.is_some() {
                        return Err("ordered snapshot endpoint unavailable");
                    }
                    slot.value = Some(Value {
                        bytes: u64::try_from(index).unwrap().to_le_bytes(),
                        _owner: Owner::new(probe, 8, 9),
                    });
                    slot.waker
                        .take()
                        .ok_or("ordered snapshot missing wake obligation")?
                };
                // This counts an observed endpoint wake, including noop Wakers;
                // callbacks are separately observed, never inferred from polls.
                probe.add(10, 1);
                wake.wake();
                probe.add(0, 1);
                match future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                {
                    Poll::Ready(Ok(result)) if index == 0 => result?,
                    Poll::Pending if index > 0 => {}
                    _ => return Err("ordered snapshot completion schedule mismatch"),
                }
            }
        }
        // Dropping the pinned future and endpoint/Waker vectors is bounded by C.
    }
    Ok(())
}

/// Emits one record after work, value, ownership and allocation checks pass.
///
/// Construction, useful work, checking and teardown retain separate measurement
/// boundaries. JSON formatting begins only after the allocation session ends.
/// The dispatcher supplies a configuration accepted by `Config::parse`.
pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    let mut measurement = Measurement::new(None);
    let (schema, facts) = if config.name == "snapshot-bytes" {
        let reference: Vec<_> = (0..config.size)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect();
        let actual: Vec<_> = (0..config.size)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect();
        let independent = config.size == 0 || reference.as_ptr() != actual.as_ptr();
        let names: Vec<_> = (0..config.iterations)
            .map(|index| format!("case_{index}"))
            .collect();
        let mut namespace =
            SnapshotNamespace::new("perf").map_err(|_| "invalid snapshot namespace")?;
        measurement.construction_finished();
        let comparisons = profile_snapshot_bytes(&mut namespace, &names, &reference, &actual)?;
        measurement.work_finished();
        if comparisons != config.iterations
            || !independent
            || reference.len() != config.size
            || actual.len() != config.size
        {
            return Err("byte snapshot work mismatch");
        }
        let mut digest = 0_u64;
        for (index, (&expected, &observed)) in reference.iter().zip(&actual).enumerate() {
            if expected != u8::try_from(index % 251).unwrap() || observed != expected {
                return Err("byte snapshot bytes mismatch");
            }
            digest = digest
                .checked_add(u64::from(observed))
                .ok_or("snapshot digest overflow")?;
        }
        for (index, name) in names.iter().enumerate() {
            if *name != format!("case_{index}") {
                return Err("byte snapshot name mismatch");
            }
        }
        digest = digest
            .checked_mul(comparisons)
            .ok_or("snapshot digest overflow")?;
        measurement.validation_finished();
        drop((namespace, names, reference, actual));
        (
            "rca-snapshot-bytes-profile-v1",
            Facts::Bytes {
                comparisons,
                digest,
                independent,
            },
        )
    } else {
        let probe = Probe::new();
        let weak = Arc::downgrade(&probe);
        measurement.construction_finished();
        profile_ordered_capture(config, &probe)?;
        measurement.work_finished();
        let counts = probe.counts();
        let cases = u64::try_from(config.size).map_err(|_| "snapshot size overflow")?;
        validate_counts(&counts, cases, config.iterations)?;
        let digest = probe.get(DIGEST);
        let expected = cases
            .checked_mul(cases.saturating_sub(1))
            .and_then(|n| n.checked_div(2))
            .and_then(|n| n.checked_mul(config.iterations))
            .ok_or("snapshot digest overflow")?;
        if digest != expected {
            return Err("ordered snapshot digest mismatch");
        }
        let callbacks = probe.get(WAKER_CALLBACKS);
        if callbacks != if cases == 0 { 0 } else { config.iterations } {
            return Err("ordered snapshot wake callback mismatch");
        }
        measurement.validation_finished();
        drop(probe);
        if weak.upgrade().is_some() {
            return Err("ordered snapshot probe retained");
        }
        drop(weak);
        (
            "rca-ordered-capture-profile-v1",
            Facts::Ordered {
                counts,
                digest,
                callbacks,
            },
        )
    };
    let allocations = measurement
        .finish()?
        .map_or_else(|| "null".to_owned(), |record| record.json());
    let facts = facts.json(config.size);
    Ok(format!(
        "{{\"schema\":\"{schema}\",\"workload\":\"{}\",\"size\":{},\"iterations\":{},\"drive_contract\":\"direct-controlled-poll-v1\",\"features\":{{\"deterministic\":true,\"allocations\":{}}},\"core_features\":\"diagnostics,test-util\",\"owners_released\":true,\"facts\":{facts},\"allocations\":{allocations}}}",
        config.name,
        config.size,
        config.iterations,
        cfg!(feature = "allocations")
    ))
}

/// Retains checked scalar results after all measured owners have been released.
enum Facts {
    /// Reports compare-only work over separately allocated byte buffers.
    Bytes {
        /// Counts successful comparisons under distinct reserved names.
        comparisons: u64,
        /// Sums all checked bytes across every comparison.
        digest: u64,
        /// Records distinct buffer addresses, except for the valid empty case.
        independent: bool,
    },
    /// Reports the fixed direct-poll schedule and its observed owner lifetimes.
    Ordered {
        /// Uses the exact field order in `COUNT_NAMES`.
        counts: [u64; 13],
        /// Sums the checked little-endian bytes from every ordered batch.
        digest: u64,
        /// Counts real waker callbacks, not endpoint wake attempts.
        callbacks: u64,
    },
}

impl Facts {
    /// Formats checked facts outside the measured allocation session.
    fn json(&self, size: usize) -> String {
        let common = "\"roots\":0,\"responses\":0,\"tasks\":0,\"redactor_calls\":0,\"writer_calls\":0,\"bytes_verified\":true";
        match self {
            Self::Bytes {
                comparisons,
                digest,
                independent,
            } => format!(
                "{{{common},\"comparisons\":{comparisons},\"reserved_cases\":{comparisons},\"byte_len\":{size},\"byte_digest\":{digest},\"independent_buffers\":{independent},\"capture_calls\":0}}"
            ),
            Self::Ordered {
                counts,
                digest,
                callbacks,
            } => {
                let fields: Vec<_> = COUNT_NAMES
                    .iter()
                    .zip(counts)
                    .map(|(name, value)| format!("\"{name}\":{value}"))
                    .collect();
                format!(
                    "{{{common},{},\"byte_len\":8,\"byte_digest\":{digest},\"waker_callbacks\":{callbacks}}}",
                    fields.join(",")
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Uses literal counts, malformed values and real cancellation/error boundaries.

    use super::*;
    use rca_test::snapshots::SnapshotFailureKind;
    use std::cell::Cell;
    use std::future::{Future, pending, ready};
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    #[test]
    fn oracle_rejects_each_missing_count() {
        // C=3/I=1: outer4, timeout4, cases9, factories/futures/values3, wake3, finish1.
        let valid = [4, 4, 9, 3, 3, 3, 3, 3, 3, 3, 3, 1, 1];
        validate_counts(&valid, 3, 1).unwrap();
        for index in 0..valid.len() {
            let mut broken = valid;
            broken[index] -= 1;
            assert!(
                validate_counts(&broken, 3, 1).is_err(),
                "accepted broken count {index}"
            );
        }
    }

    #[test]
    fn ordered_byte_oracle_rejects_missing_value_wrong_order_and_last_byte() {
        assert!(
            finish_values(Vec::new(), 1).is_err(),
            "missing zero-valued case accepted"
        );
        let probe = Probe::new();
        for bytes in [[1, 0, 0, 0, 0, 0, 0, 0], [0, 0, 0, 0, 0, 0, 0, 1]] {
            let value = Value {
                bytes,
                _owner: Owner::new(&probe, 8, 9),
            };
            assert!(finish_values(vec![value], 1).is_err());
        }
        assert_eq!(probe.get(8), probe.get(9));
    }

    #[test]
    fn controlled_schedule_balances_owners_without_executor() {
        let probe = Probe::new();
        let weak = Arc::downgrade(&probe);
        profile_ordered_capture(
            Config {
                name: "snapshot-ordered",
                size: 3,
                iterations: 1,
            },
            &probe,
        )
        .unwrap();
        assert_eq!(probe.counts(), [4, 4, 9, 3, 3, 3, 3, 3, 3, 3, 3, 1, 1]);
        assert_eq!(probe.get(DIGEST), 3);
        assert_eq!(probe.get(WAKER_CALLBACKS), 1);
        drop(probe);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn byte_failures_are_typed_reserved_and_opaque() {
        let mut namespace = SnapshotNamespace::new("perf").unwrap();
        let failure = namespace
            .compare("last", Some(b"private-secret-a"), b"private-secret-b")
            .unwrap_err();
        assert!(matches!(failure.kind(), SnapshotFailureKind::Mismatch));
        assert_eq!(failure.path().unwrap().as_str(), "perf/last");
        assert!(!format!("{failure} {failure:?}").contains("private-secret"));
        assert!(matches!(
            namespace
                .compare("last", Some(b"same"), b"same")
                .unwrap_err()
                .kind(),
            SnapshotFailureKind::Collision
        ));
        assert!(matches!(
            namespace.compare("missing", None, b"").unwrap_err().kind(),
            SnapshotFailureKind::Missing
        ));
        namespace.compare("empty", Some(b""), b"").unwrap();
        for name in ["", "Case", "a/b", "..", "con", "com1", "lpt9", "é"] {
            for _ in 0..2 {
                let failure = namespace.compare(name, Some(b""), b"").unwrap_err();
                assert!(matches!(failure.kind(), SnapshotFailureKind::InvalidName));
                assert!(failure.path().is_none());
            }
            assert!(SnapshotNamespace::new(name).is_err());
        }
    }

    /// Counts destruction even when the owning closure or future never runs.
    struct Guard(Rc<Cell<u64>>);

    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn unpolled_drop_and_ready_timeout_never_start_factories() {
        for poll in [false, true] {
            let entries = Cell::new(0);
            let factories = Rc::new(Cell::new(0));
            let finishes = Rc::new(Cell::new(0));
            let finish_owner = Guard(Rc::clone(&finishes));
            let cases = (0..3)
                .map(|_| {
                    let owner = Guard(Rc::clone(&factories));
                    let entries = &entries;
                    move || {
                        drop(owner);
                        entries.set(entries.get() + 1);
                        ready(Ok::<_, ()>(0))
                    }
                })
                .collect();
            let mut future = Box::pin(capture_ordered(cases, ready(()), move |_| {
                drop(finish_owner);
                panic!("finish after timeout");
            }));
            if poll {
                let result = future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()));
                let Poll::Ready(Err(failure)) = result else {
                    panic!("timeout must be ready");
                };
                assert!(failure.is_timeout());
            }
            drop(future);
            assert_eq!(entries.get(), 0);
            assert_eq!(factories.get(), 3);
            assert_eq!(finishes.get(), 1);
        }
    }

    #[test]
    fn pending_timeout_and_drop_destroy_cases_and_close_endpoints() {
        for timeout in [false, true] {
            let entries = Cell::new(0);
            let case_drops = Rc::new(Cell::new(0));
            let finish_drops = Rc::new(Cell::new(0));
            let mut senders = Vec::new();
            let mut cases = Vec::new();
            for _ in 0..3 {
                let (sender, receiver) = async_channel::bounded::<u8>(1);
                senders.push(sender);
                let owner = Guard(Rc::clone(&case_drops));
                let entries = &entries;
                cases.push(move || {
                    entries.set(entries.get() + 1);
                    async move {
                        let value = receiver.recv().await;
                        drop(owner);
                        value
                    }
                });
            }
            let (sender, receiver) = async_channel::bounded(1);
            let finish_owner = Guard(Rc::clone(&finish_drops));
            let mut future = Box::pin(capture_ordered(
                cases,
                async move {
                    receiver.recv().await.unwrap();
                },
                move |_| {
                    drop(finish_owner);
                    panic!("finish after disposal");
                },
            ));
            assert!(
                future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(entries.get(), 3);
            if timeout {
                sender.try_send(()).unwrap();
                let result = future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()));
                let Poll::Ready(Err(failure)) = result else {
                    panic!("timeout must be ready");
                };
                assert!(failure.is_timeout());
            }
            drop(future);
            assert_eq!(case_drops.get(), 3);
            assert_eq!(finish_drops.get(), 1);
            for sender in senders {
                assert!(sender.try_send(1).is_err());
            }
        }
    }

    #[test]
    fn indexed_error_preserves_cause_and_destroys_siblings() {
        /// Retains the original error identity until its returned failure drops.
        struct Cause(Rc<()>);
        let cause = Rc::new(());
        let weak = Rc::downgrade(&cause);
        let case_drops = Rc::new(Cell::new(0));
        let finish_drops = Rc::new(Cell::new(0));
        let cases = (0..3)
            .map(|index| {
                let owner = Guard(Rc::clone(&case_drops));
                let cause = Rc::clone(&cause);
                move || async move {
                    let _owner = owner;
                    if index == 1 {
                        Err::<u8, _>(Cause(cause))
                    } else {
                        pending().await
                    }
                }
            })
            .collect();
        let finish_owner = Guard(Rc::clone(&finish_drops));
        let mut future = Box::pin(capture_ordered(cases, pending::<()>(), move |_| {
            drop(finish_owner);
            panic!("finish after error");
        }));
        let Poll::Ready(Err(failure)) = future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("capture error must be ready");
        };
        assert!(!failure.is_timeout());
        let (index, retained) = failure.cause().unwrap();
        assert_eq!(index, 1);
        assert!(Rc::ptr_eq(&retained.0, &cause));
        drop(future);
        assert_eq!(case_drops.get(), 3);
        assert_eq!(finish_drops.get(), 1);
        drop(cause);
        assert!(weak.upgrade().is_some());
        drop(failure);
        assert!(weak.upgrade().is_none());
    }
}
