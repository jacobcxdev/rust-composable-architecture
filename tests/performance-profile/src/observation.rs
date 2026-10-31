//! Exact observation traces with bounded driving and independently checked owner release.
//!
//! Selection, equality, cloning and delivery are counted separately. Receipt
//! boxes and fixture counters are included in their measured phases; the output
//! is not an isolated cost for any one observation operation.

use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use rca_core::{ActionTask, Effect, Observation, ReduceContext, Reducer, Store, StoreStopped};

use crate::host::Host;
use crate::measurement::{ExecutorCache, Measurement};
#[cfg(test)]
use crate::observation_controls::{Fault, SemanticCase};

/// Fixed storage for the largest admitted subscriber width.
const MAX_SUBSCRIBERS: usize = 64;
/// A cooperative native-drive safety limit, not a performance threshold.
const DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
/// Validated observation policy, host choice, work size and drive limits.
pub(crate) struct Config {
    name: &'static str,
    size: usize,
    roots: u64,
    distinct: bool,
    coalesced: bool,
    native: bool,
    task_budget: usize,
    outer_budget: usize,
    expected: Expected,
}

#[derive(Clone, Copy, Debug)]
/// Independent totals for even root counts with the selected value `revision / 2`.
struct Expected {
    checksum: u64,
    originals: u64,
    clones: u64,
    equality: u64,
    created: u64,
    per_deliveries: u64,
    deliveries: u64,
    sum: u64,
    work: Counts,
}

impl Expected {
    /// Calculates expected work without consulting any runtime counter.
    fn new(n: u64, r: u64, distinct: bool, coalesced: bool) -> Result<Self, &'static str> {
        let half = r / 2;
        let originals = mul(n, add(r, 1)?)?;
        let clones = if distinct { mul(n, add(half, 1)?)? } else { 0 };
        let per_deliveries = if coalesced {
            2
        } else {
            add(if distinct { half } else { r }, 1)?
        };
        let equality = if distinct { mul(n, r)? } else { 0 };
        let cached = if distinct { n } else { 0 };
        let work_originals = mul(n, r)?;
        let work_clones = if distinct { mul(n, half)? } else { 0 };
        Ok(Self {
            checksum: mul(r, add(r, 1)?)? / 2,
            originals,
            clones,
            equality,
            created: add(originals, clones)?,
            per_deliveries,
            deliveries: mul(n, per_deliveries)?,
            sum: if coalesced {
                half
            } else if distinct {
                mul(half, add(half, 1)?)? / 2
            } else {
                mul(half, half)?
            },
            work: Counts {
                selected_originals: work_originals,
                selected_clones: work_clones,
                selected_dropped: add(work_originals, work_clones)?,
                selector_calls: work_originals,
                equality_calls: equality,
                deliveries: mul(n, per_deliveries - 1)?,
                registrations: 0,
                selector_captures_dropped: 0,
                selected_live: cached,
                selector_captures_live: n,
            },
        })
    }

    /// Includes initial delivery and any distinct observer's retained comparison value.
    fn cold(self) -> Counts {
        let n = self.work.selector_captures_live;
        let cached = self.work.selected_live;
        Counts {
            selected_originals: n,
            selected_clones: cached,
            selected_dropped: n,
            selector_calls: n,
            equality_calls: 0,
            deliveries: n,
            registrations: n,
            selector_captures_dropped: 0,
            selected_live: cached,
            selector_captures_live: n,
        }
    }
}

/// Rejects arithmetic overflow while validating caller-supplied work sizes.
fn add(a: u64, b: u64) -> Result<u64, &'static str> {
    a.checked_add(b).ok_or("observation count overflow")
}
/// Rejects arithmetic overflow while calculating expected work and poll limits.
fn mul(a: u64, b: u64) -> Result<u64, &'static str> {
    a.checked_mul(b).ok_or("observation count overflow")
}

/// Gives finite ready work ample task and client polls without an unbounded drive.
fn budgets(n: u64, r: u64) -> Result<(usize, usize), &'static str> {
    let s = add(mul(128, add(add(mul(r, add(n, 1)?)?, n)?, 1)?)?, 2048)?;
    let c = add(mul(2, s)?, 16)?;
    Ok((
        usize::try_from(s).map_err(|_| "observation poll-budget overflow")?,
        usize::try_from(c).map_err(|_| "observation poll-budget overflow")?,
    ))
}

impl Config {
    /// Rejects unavailable hosts, unsupported widths, odd roots and oversized work.
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let name = args.next().ok_or("expected WORKLOAD SIZE ITERATIONS")?;
        let (name, distinct, coalesced, native) = match name.as_str() {
            "observe-plain-native" => ("observe-plain-native", false, false, true),
            "observe-plain-deterministic" => ("observe-plain-deterministic", false, false, false),
            "observe-distinct-native" => ("observe-distinct-native", true, false, true),
            "observe-distinct-deterministic" => {
                ("observe-distinct-deterministic", true, false, false)
            }
            "observe-coalesced-native" => ("observe-coalesced-native", false, true, true),
            "observe-coalesced-deterministic" => {
                ("observe-coalesced-deterministic", false, true, false)
            }
            "observe-distinct-coalesced-native" => {
                ("observe-distinct-coalesced-native", true, true, true)
            }
            "observe-distinct-coalesced-deterministic" => (
                "observe-distinct-coalesced-deterministic",
                true,
                true,
                false,
            ),
            _ => return Err("unknown observation workload"),
        };
        if !native && !cfg!(feature = "deterministic") {
            return Err("deterministic workloads require the deterministic feature");
        }
        let n = crate::workloads::integer(args.next())?;
        let r = crate::workloads::integer(args.next())?;
        if !matches!(n, 0 | 1 | 16 | 64)
            || r == 0
            || r > 1_000_000
            || r % 2 != 0
            || args.next().is_some()
        {
            return Err(
                "observation requires SIZE=0,1,16,64 and positive even ITERATIONS<=1000000",
            );
        }
        let (task_budget, outer_budget) = budgets(n, r)?;
        Ok(Self {
            name,
            size: usize::try_from(n).map_err(|_| "observation size overflow")?,
            roots: r,
            distinct,
            coalesced,
            native,
            task_budget,
            outer_budget,
            expected: Expected::new(n, r, distinct, coalesced)?,
        })
    }
}

#[derive(Default)]
/// Counts real callbacks and destruction; relaxed loads are not scheduling signals.
struct Probes {
    originals: AtomicU64,
    clones: AtomicU64,
    equality: AtomicU64,
    dropped: AtomicU64,
    selectors: AtomicU64,
    captures_created: AtomicU64,
    captures_dropped: AtomicU64,
    state_dropped: AtomicU64,
    reducer_dropped: AtomicU64,
    reducing: AtomicBool,
}

/// Counts each original, comparison clone and final destruction separately.
struct Selected {
    payload: u64,
    probes: Arc<Probes>,
}
impl Clone for Selected {
    fn clone(&self) -> Self {
        self.probes.clones.fetch_add(1, Ordering::Relaxed);
        Self {
            payload: self.payload,
            probes: Arc::clone(&self.probes),
        }
    }
}
impl PartialEq for Selected {
    fn eq(&self, other: &Self) -> bool {
        self.probes.equality.fetch_add(1, Ordering::Relaxed);
        self.payload == other.payload
    }
}
impl Drop for Selected {
    fn drop(&mut self) {
        self.probes.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Keeps a separately witnessed owner inside the real registered selector.
struct Capture {
    probes: Arc<Probes>,
    owner: Arc<()>,
}
impl Capture {
    /// Records selector construction before registration is acknowledged.
    fn new(probes: &Arc<Probes>, owner: Arc<()>) -> Self {
        probes.captures_created.fetch_add(1, Ordering::Relaxed);
        Self {
            probes: Arc::clone(probes),
            owner,
        }
    }
    /// Selects paired equal revisions, with one optional incorrect-payload control.
    fn select(&self, state: &State, wrong: bool) -> Selected {
        self.probes.selectors.fetch_add(1, Ordering::Relaxed);
        self.probes.originals.fetch_add(1, Ordering::Relaxed);
        Selected {
            payload: if wrong && state.revision == 2 {
                2
            } else {
                state.revision / 2
            },
            probes: Arc::clone(&self.probes),
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.probes.captures_dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Records actual reduction order and checksum independently of send receipts.
struct State {
    revision: u64,
    reductions: u64,
    checksum: u64,
    valid: bool,
    probes: Arc<Probes>,
}
impl Drop for State {
    fn drop(&mut self) {
        self.probes.state_dropped.fetch_add(1, Ordering::Relaxed);
    }
}
/// Supplies an ordinal so missing, reordered or repeated roots change the state oracle.
enum Action {
    Increment(u64),
}
/// Applies one counted root and exposes reducer destruction independently of state.
struct Counter(Arc<Probes>);
impl Drop for Counter {
    fn drop(&mut self) {
        self.0.reducer_dropped.fetch_add(1, Ordering::Relaxed);
    }
}
impl Reducer for Counter {
    type State = State;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        self.0.reducing.store(true, Ordering::Relaxed);
        let Action::Increment(ordinal) = *action;
        state.valid &= ordinal == state.revision + 1;
        state.revision += 1;
        state.reductions += 1;
        state.checksum += ordinal;
        self.0.reducing.store(false, Ordering::Relaxed);
        Effect::none()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Summarises values actually consumed by one subscriber.
struct SubscriberSummary {
    deliveries: u64,
    sum: u64,
    last: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Combines cumulative work counts with live value and selector-owner counts.
struct Counts {
    selected_originals: u64,
    selected_clones: u64,
    selected_dropped: u64,
    selector_calls: u64,
    equality_calls: u64,
    deliveries: u64,
    registrations: u64,
    selector_captures_dropped: u64,
    selected_live: u64,
    selector_captures_live: u64,
}
impl Counts {
    /// Formats phase counters after the measurement has ended.
    fn json(self) -> String {
        format!(
            concat!(
                "{{\"selected_originals\":{},\"selected_clones\":{},\"selected_dropped\":{},",
                "\"selector_calls\":{},\"equality_calls\":{},\"deliveries\":{},",
                "\"registrations\":{},\"selector_captures_dropped\":{},",
                "\"selected_live\":{},\"selector_captures_live\":{}}}"
            ),
            self.selected_originals,
            self.selected_clones,
            self.selected_dropped,
            self.selector_calls,
            self.equality_calls,
            self.deliveries,
            self.registrations,
            self.selector_captures_dropped,
            self.selected_live,
            self.selector_captures_live
        )
    }
    /// Subtracts cumulative work while retaining the current live-owner counts.
    fn since(self, cold: Self) -> Self {
        Self {
            selected_originals: self.selected_originals - cold.selected_originals,
            selected_clones: self.selected_clones - cold.selected_clones,
            selected_dropped: self.selected_dropped - cold.selected_dropped,
            selector_calls: self.selector_calls - cold.selector_calls,
            equality_calls: self.equality_calls - cold.equality_calls,
            deliveries: self.deliveries - cold.deliveries,
            registrations: self.registrations - cold.registrations,
            selector_captures_dropped: self.selector_captures_dropped
                - cold.selector_captures_dropped,
            selected_live: self.selected_live,
            selector_captures_live: self.selector_captures_live,
        }
    }
}

/// Keeps an admitted shutdown result alive when a bounded client drive is rejected.
type Extraction = Pin<Box<dyn Future<Output = Result<State, StoreStopped>> + Send>>;
/// Keeps an admitted root receipt outside the temporarily borrowed client future.
type ReceiptFinish = Pin<Box<dyn Future<Output = Result<ActionTask, StoreStopped>> + Send>>;
/// Owns the runtime, admitted operations and independent probes through cleanup.
struct Fixture {
    /// Temporarily moves out while the host drives a future borrowing this fixture.
    host: Option<Host>,
    store: Option<Store<State, Action>>,
    /// Remains available for callbacks after shutdown takes the main handle.
    cleanup_store: Option<Store<State, Action>>,
    /// Must be resumed after rejection, never replaced by a second shutdown request.
    extraction: Option<Extraction>,
    /// Retains the actual admitted receipt until its commit result is consumed.
    receipt: Option<ReceiptFinish>,
    /// Retains causal work until completion or acknowledged cancellation.
    task: Option<ActionTask>,
    observations: [Option<Observation<Selected>>; MAX_SUBSCRIBERS],
    /// Checks selector captures without keeping them alive.
    registration_owners: [Option<Weak<()>>; MAX_SUBSCRIBERS],
    /// Independently checks disposal of runtime-owned task state.
    host_owner: Weak<()>,
    probes: Arc<Probes>,
    rows: [SubscriberSummary; MAX_SUBSCRIBERS],
    registrations: u64,
    roots: u64,
    completed: u64,
    cancelled: u64,
    closed_next: u64,
    cold: Counts,
    work: Counts,
    #[cfg(test)]
    fault: Option<Fault>,
    #[cfg(test)]
    retained_value: Option<Selected>,
    #[cfg(test)]
    retained_capture: Option<Arc<()>>,
    #[cfg(test)]
    cleanup_receipts_settled: u64,
    #[cfg(test)]
    /// Preserves a callback-enqueued receipt that the rejected client did not consume.
    handoff_receipt: Option<ReceiptFinish>,
}

impl Fixture {
    /// Constructs empty observation slots and a reducer on the selected real host.
    fn new(config: Config) -> Result<Self, &'static str> {
        let (host, runtime, host_owner) = Host::new(config.native, config.task_budget)?;
        let probes = Arc::new(Probes::default());
        let store = Store::new(
            State {
                revision: 0,
                reductions: 0,
                checksum: 0,
                valid: true,
                probes: Arc::clone(&probes),
            },
            Counter(Arc::clone(&probes)),
            runtime,
        );
        Ok(Self {
            host: Some(host),
            cleanup_store: Some(store.clone()),
            store: Some(store),
            extraction: None,
            receipt: None,
            task: None,
            observations: std::array::from_fn(|_| None),
            registration_owners: std::array::from_fn(|_| None),
            host_owner,
            probes,
            rows: [SubscriberSummary::default(); MAX_SUBSCRIBERS],
            registrations: 0,
            roots: 0,
            completed: 0,
            cancelled: 0,
            closed_next: 0,
            cold: Counts::default(),
            work: Counts::default(),
            #[cfg(test)]
            fault: None,
            #[cfg(test)]
            retained_value: None,
            #[cfg(test)]
            retained_capture: None,
            #[cfg(test)]
            cleanup_receipts_settled: 0,
            #[cfg(test)]
            handoff_receipt: None,
        })
    }
    /// Samples operation and live-owner counts after an acknowledged drive boundary.
    fn counts(&self) -> Counts {
        let p = &self.probes;
        let originals = p.originals.load(Ordering::Relaxed);
        let clones = p.clones.load(Ordering::Relaxed);
        let dropped = p.dropped.load(Ordering::Relaxed);
        let captures = p.captures_dropped.load(Ordering::Relaxed);
        Counts {
            selected_originals: originals,
            selected_clones: clones,
            selected_dropped: dropped,
            selector_calls: p.selectors.load(Ordering::Relaxed),
            equality_calls: p.equality.load(Ordering::Relaxed),
            deliveries: self.rows.iter().map(|r| r.deliveries).sum(),
            registrations: self.registrations,
            selector_captures_dropped: captures,
            selected_live: originals + clones - dropped,
            selector_captures_live: p.captures_created.load(Ordering::Relaxed) - captures,
        }
    }
    /// Registers each selector and consumes its initial value before measuring work.
    async fn register_initial(&mut self, config: Config) -> Result<(), &'static str> {
        for j in 0..config.size {
            #[cfg(test)]
            if self.fault == Some(Fault::OmittedSubscriber) && j == config.size - 1 {
                continue;
            }
            let owner = Arc::new(());
            self.registration_owners[j] = Some(Arc::downgrade(&owner));
            #[cfg(test)]
            if self.fault == Some(Fault::RetainedRegistrationOwner) && j == 0 {
                self.retained_capture = Some(Arc::clone(&owner));
            }
            let capture = Capture::new(&self.probes, owner);
            #[cfg(test)]
            let wrong = self.fault == Some(Fault::WrongSelectedPayload);
            #[cfg(not(test))]
            let wrong = false;
            let selector = move |state: &State| {
                // Keep the independently witnessed capture, not only its probe Arc.
                let _owner = &capture.owner;
                capture.select(state, wrong)
            };
            let store = self.store.as_ref().ok_or("observation Store absent")?;
            self.observations[j] = Some(
                if config.distinct {
                    store.observe_distinct(selector).await
                } else {
                    store.observe(selector).await
                }
                .map_err(|_| "observation registration stopped")?,
            );
            self.registrations += 1;
            self.take(j, 0).await?;
        }
        self.cold = self.counts();
        self.validate_cold(config)
    }
    /// Requires every requested registration before any root action is sent.
    fn validate_cold(&self, config: Config) -> Result<(), &'static str> {
        if self.cold != config.expected.cold() || self.roots != 0 {
            return Err("observation cold counters invalid");
        }
        Ok(())
    }
    /// Consumes one exact payload and normally drops it before returning.
    async fn take(&mut self, j: usize, expected: u64) -> Result<(), &'static str> {
        let value = self.observations[j]
            .as_mut()
            .ok_or("observation subscriber absent")?
            .next()
            .await
            .ok_or("observation unexpectedly closed")?;
        if value.payload != expected {
            return Err("observation selected payload invalid");
        }
        self.rows[j].deliveries += 1;
        self.rows[j].sum += value.payload;
        self.rows[j].last = value.payload;
        #[cfg(test)]
        if self.fault == Some(Fault::RetainedValue) && expected == 1 {
            self.retained_value = Some(value);
            return Ok(());
        }
        drop(value);
        Ok(())
    }
    /// Admits one ordinal and retains its receipt through bounded-drive rejection.
    async fn commit(&mut self, ordinal: u64) -> Result<(), &'static str> {
        // Receipt/terminal futures stay outside the borrowed client so drive
        // rejection cannot discard their admitted results. This one receipt
        // box per root is explicit fixed harness overhead.
        let receipt = self
            .store
            .as_ref()
            .ok_or("observation Store absent")?
            .try_send(Action::Increment(ordinal))
            .map_err(|_| "observation root admission failed")?;
        self.receipt = Some(Box::pin(receipt.finish()));
        self.finish_receipt().await
    }
    /// Separately acknowledges commit, causal completion and absence of cancellation.
    async fn finish_receipt(&mut self) -> Result<(), &'static str> {
        let committed = self
            .receipt
            .as_mut()
            .expect("admitted receipt")
            .as_mut()
            .await;
        self.receipt = None;
        self.task = Some(committed.map_err(|_| "observation root stopped")?);
        self.roots += 1;
        let task = self.task.as_ref().expect("just committed task");
        task.finish().await;
        if task.is_cancelled() {
            self.cancelled += 1;
            return Err("observation root cancelled");
        }
        if !task.is_finished() {
            return Err("observation causal task unfinished");
        }
        self.completed += 1;
        self.task = None;
        Ok(())
    }
    /// Applies every root, draining on each chosen publication or once after coalescing.
    async fn work(&mut self, config: Config) -> Result<(), &'static str> {
        for k in 1..=config.roots {
            #[cfg(test)]
            if self.fault == Some(Fault::MissingRoot) && k == config.roots {
                break;
            }
            self.commit(k).await?;
            let drain = !config.coalesced && (!config.distinct || k % 2 == 0);
            #[cfg(test)]
            let drain = drain || (self.fault == Some(Fault::UnexpectedCoalescedDrain) && k == 1);
            if drain {
                for j in 0..config.size {
                    self.take(j, k / 2).await?;
                }
            }
        }
        // Reject missing work before awaiting a final distinct publication.
        if self.roots != config.roots {
            return Err("observation root trace invalid");
        }
        if config.coalesced {
            for j in 0..config.size {
                self.take(j, config.roots / 2).await?;
            }
        }
        self.work = self.counts().since(self.cold);
        Ok(())
    }
    /// Cross-checks committed state, delivered values, callback counts and live owners.
    async fn validate(&mut self, config: Config) -> Result<(u64, u64, u64), &'static str> {
        let (revision, reductions, checksum, valid) = self
            .store
            .as_ref()
            .ok_or("observation Store absent")?
            .read(|s| (s.revision, s.reductions, s.checksum, s.valid))
            .await
            .map_err(|_| "observation read stopped")?;
        if !valid
            || revision != config.roots
            || reductions != config.roots
            || checksum != config.expected.checksum
            || self.roots != config.roots
            || self.completed != config.roots
            || self.cancelled != 0
        {
            return Err("observation root trace invalid");
        }
        for row in &self.rows[..config.size] {
            if *row
                != (SubscriberSummary {
                    deliveries: config.expected.per_deliveries,
                    sum: config.expected.sum,
                    last: config.roots / 2,
                })
            {
                return Err("observation subscriber trace invalid");
            }
        }
        let counts = self.counts();
        if counts.selected_originals != config.expected.originals
            || counts.selector_calls != config.expected.originals
            || counts.selected_clones != config.expected.clones
            || counts.equality_calls != config.expected.equality
            || counts.deliveries != config.expected.deliveries
        {
            return Err("observation work counters invalid");
        }
        let cached = if config.distinct {
            u64::try_from(config.size).expect("validated subscriber width")
        } else {
            0
        };
        if counts.selected_live != cached
            || counts.selected_dropped != config.expected.created - cached
        {
            return Err("observation phase value lifetime invalid");
        }
        if counts.selector_captures_live
            != u64::try_from(config.size).expect("validated subscriber width")
            || counts.selector_captures_dropped != 0
        {
            return Err("observation phase capture lifetime invalid");
        }
        if self.work != config.expected.work {
            return Err("observation work counters invalid");
        }
        Ok((revision, reductions, checksum))
    }
    /// Creates at most one shutdown future and keeps it pinned outside driven clients.
    fn begin_extraction(&mut self) {
        if self.extraction.is_none()
            && let Some(store) = self.store.take()
        {
            // Pin outside the driven client. A rejected drive only borrows
            // this future: admission cannot be observed separately, so it
            // must be resumed, never replaced/resubmitted.
            self.extraction = Some(Box::pin(store.shutdown_into_state()));
        }
    }
    /// Settles admitted receipts, extracts state and optionally checks observer closure.
    async fn extract_and_close(
        &mut self,
        check_closed: bool,
    ) -> Result<(u64, u64, u64), &'static str> {
        #[cfg(test)]
        let settling_receipt = self.receipt.is_some();
        if let Some(receipt) = self.receipt.as_mut() {
            self.task = Some(
                receipt
                    .as_mut()
                    .await
                    .map_err(|_| "observation cleanup receipt stopped")?,
            );
            self.receipt = None;
            if let Some(task) = self.task.as_ref()
                && !task.is_finished()
            {
                task.cancel();
            }
        }
        if let Some(task) = self.task.as_ref() {
            task.finish().await;
            #[cfg(test)]
            assert!(task.is_finished());
        }
        #[cfg(test)]
        if settling_receipt {
            self.cleanup_receipts_settled += 1;
        }
        #[cfg(test)]
        if let Some(receipt) = self.handoff_receipt.as_mut() {
            self.task = Some(
                receipt
                    .as_mut()
                    .await
                    .map_err(|_| "reentry cleanup receipt stopped")?,
            );
            self.handoff_receipt = None;
            let task = self.task.as_ref().unwrap();
            if !task.is_finished() {
                task.cancel();
            }
            task.finish().await;
            assert!(task.is_finished());
            self.cleanup_receipts_settled += 1;
        }
        self.task = None;
        self.receipt = None;
        let extracted = self
            .extraction
            .as_mut()
            .ok_or("observation extraction absent")?
            .as_mut()
            .await;
        self.extraction = None;
        let state = extracted.map_err(|_| "observation extraction stopped")?;
        let scalar = (state.revision, state.reductions, state.checksum);
        drop(state);
        if check_closed {
            for observation in self.observations.iter_mut().flatten() {
                if observation.next().await.is_some() {
                    return Err("observation closed next yielded value");
                }
                self.closed_next += 1;
            }
        }
        Ok(scalar)
    }
    /// Drops acknowledged observation, store and host handles after extraction.
    fn drop_handles(&mut self) {
        self.observations = std::array::from_fn(|_| None);
        self.cleanup_store = None;
        self.store = None;
        self.task = None;
        self.host = None;
    }
    /// Collects independent drop, weak-reference and final-probe ownership evidence.
    fn owner_facts(&self) -> OwnerFacts {
        let c = self.counts();
        let stopped = self.host.is_none()
            && self.store.is_none()
            && self.cleanup_store.is_none()
            && self.extraction.is_none();
        #[cfg(test)]
        let stopped = stopped && self.handoff_receipt.is_none();
        OwnerFacts {
            state_dropped: self.probes.state_dropped.load(Ordering::Relaxed) == 1,
            reducer_dropped: self.probes.reducer_dropped.load(Ordering::Relaxed) == 1,
            host_task_dropped: self.host_owner.upgrade().is_none(),
            registration_owners_released: self
                .registration_owners
                .iter()
                .flatten()
                .all(|w| w.upgrade().is_none()),
            value_owners_released: c.selected_live == 0,
            probe_owners_released: Arc::strong_count(&self.probes) == 1,
            host_stopped: stopped,
        }
    }
    /// Checks normal closure and disposes handles after bounded state extraction.
    fn teardown(&mut self, config: Config) -> Result<(u64, u64, u64), &'static str> {
        self.begin_extraction();
        let host = self.host.take().ok_or("observation Host absent")?;
        let result =
            host.run_ready_bounded(self.extract_and_close(true), DEADLINE, config.outer_budget);
        self.host = Some(host);
        let scalar = result??;
        self.drop_handles();
        Ok(scalar)
    }
    /// Releases deliberate fault owners and resumes retained operations after rejection.
    fn cleanup(&mut self, config: Config) -> Result<(), &'static str> {
        #[cfg(test)]
        {
            self.retained_value = None;
            self.retained_capture = None;
        }
        if self.store.is_none() && self.extraction.is_none() {
            self.drop_handles();
            return self.owner_facts().validate();
        }
        self.observations = std::array::from_fn(|_| None);
        if let Some(task) = self.task.as_ref()
            && !task.is_finished()
        {
            task.cancel();
        }
        self.begin_extraction();
        let host = self.host.take().ok_or("observation cleanup Host absent")?;
        let result =
            host.run_ready_bounded(self.extract_and_close(false), DEADLINE, config.outer_budget);
        self.host = Some(host);
        result??;
        self.drop_handles();
        self.owner_facts().validate()
    }
}

#[cfg(test)]
/// Runs a real-work fault, then proves rejection did not leave its fixture owned.
pub(crate) fn control(fault: Fault, native: bool) -> Result<(), &'static str> {
    let name = if fault == Fault::UnexpectedCoalescedDrain {
        if native {
            "observe-coalesced-native"
        } else {
            "observe-coalesced-deterministic"
        }
    } else if native {
        "observe-distinct-native"
    } else {
        "observe-distinct-deterministic"
    };
    let config = Config::parse(
        [
            name,
            if fault == Fault::OmittedSubscriber {
                "64"
            } else {
                "1"
            },
            "2",
        ]
        .into_iter()
        .map(str::to_owned),
    )?;
    let mut fixture = Fixture::new(config)?;
    fixture.fault = Some(fault);
    let result = (|| {
        let registration = profile_observation_registration(&mut fixture, config);
        if fault == Fault::OmittedSubscriber {
            assert_eq!(registration, Err("observation cold counters invalid"));
            omitted_subscriber_work(&mut fixture, config)?;
            return validate_fixture(&mut fixture, config).map(|_| ());
        }
        registration?;
        profile_observation_work(&mut fixture, config)?;
        let checked = validate_fixture(&mut fixture, config)?;
        if fault == Fault::SkippedTeardown {
            fixture.owner_facts().validate()?;
        }
        finish_fixture(&mut fixture, config, checked)?;
        Ok(())
    })();
    if fault == Fault::OmittedSubscriber {
        assert_eq!(fixture.roots, 2);
        assert_eq!(fixture.completed, 2);
        assert_eq!(fixture.cancelled, 0);
    }
    // Rejection is not cleanup: release the deliberate owners and settle the
    // externally pinned terminal future through one separate bounded call.
    fixture.cleanup(config)?;
    fixture.owner_facts().validate()?;
    let counts = fixture.counts();
    assert_eq!(counts.selected_live, 0);
    assert_eq!(
        counts.selected_dropped,
        counts.selected_originals + counts.selected_clones
    );
    assert_eq!(counts.selector_captures_dropped, counts.registrations);
    result
}

#[cfg(test)]
/// Completes all roots for 63 registrations so a missing subscriber cannot hide as missing work.
fn omitted_subscriber_work(fixture: &mut Fixture, config: Config) -> Result<(), &'static str> {
    let host = fixture.host.take().unwrap();
    let result = host.run_ready_bounded(
        async {
            for ordinal in 1..=config.roots {
                fixture.commit(ordinal).await?;
                if ordinal % 2 == 0 {
                    for j in 0..config.size {
                        if fixture.observations[j].is_some() {
                            fixture.take(j, ordinal / 2).await?;
                        }
                    }
                }
            }
            fixture.work = fixture.counts().since(fixture.cold);
            let scalar = fixture
                .store
                .as_ref()
                .unwrap()
                .read(|s| (s.revision, s.reductions, s.checksum, s.valid))
                .await
                .map_err(|_| "omitted subscriber read stopped")?;
            assert_eq!(scalar, (2, 2, 3, true));
            assert_eq!(
                (fixture.roots, fixture.completed, fixture.cancelled),
                (2, 2, 0)
            );
            assert_eq!(fixture.registrations, 63);
            for row in &fixture.rows[..63] {
                assert_eq!(
                    *row,
                    SubscriberSummary {
                        deliveries: 2,
                        sum: 1,
                        last: 1
                    }
                );
            }
            Ok::<(), &'static str>(())
        },
        DEADLINE,
        config.outer_budget,
    );
    fixture.host = Some(host);
    result?
}

#[cfg(test)]
/// Installs a waiter, requires `Pending`, then drops only the temporary next future.
fn pending_next(
    observation: &mut Observation<Selected>,
    waker: &std::task::Waker,
) -> Result<(), &'static str> {
    let mut next = std::pin::pin!(observation.next());
    if next
        .as_mut()
        .poll(&mut std::task::Context::from_waker(waker))
        .is_pending()
    {
        Ok(())
    } else {
        Err("observation expected pending next")
    }
}

#[cfg(test)]
/// Dispatches the finite callback and waiter schedules outside profiling records.
pub(crate) fn semantic_control(case: SemanticCase, native: bool) -> Result<(), &'static str> {
    match case {
        SemanticCase::EqualCancelledNext => equal_cancelled_next(native),
        SemanticCase::MovedWaiter => moved_waiter(native),
        SemanticCase::SelectorReentry => selector_reentry(native),
        SemanticCase::CallbackDetach => callback_detach(native),
    }
}

#[cfg(test)]
/// Uses one subscriber and two profile roots, with room for a third callback root.
fn tiny_config(native: bool, distinct: bool) -> Config {
    let name = match (native, distinct) {
        (true, true) => "observe-distinct-native",
        (true, false) => "observe-plain-native",
        (false, true) => "observe-distinct-deterministic",
        (false, false) => "observe-plain-deterministic",
    };
    let mut config = Config::parse([name, "1", "2"].into_iter().map(str::to_owned)).unwrap();
    // Semantic callbacks enqueue at most three roots, not profile roots.
    (config.task_budget, config.outer_budget) = budgets(1, 3).unwrap();
    config
}

#[cfg(test)]
/// Checks equal-value suppression and reuse after cancelling a pending next call.
fn equal_cancelled_next(native: bool) -> Result<(), &'static str> {
    let config = tiny_config(native, true);
    let mut fixture = Fixture::new(config)?;
    profile_observation_registration(&mut fixture, config)?;
    let host = fixture.host.take().unwrap();
    let result = host.run_ready_bounded(
        async {
            fixture.commit(1).await?;
            pending_next(
                fixture.observations[0].as_mut().unwrap(),
                std::task::Waker::noop(),
            )?;
            fixture.commit(2).await?;
            fixture.take(0, 1).await?;
            pending_next(
                fixture.observations[0].as_mut().unwrap(),
                std::task::Waker::noop(),
            )?;
            Ok::<(), &'static str>(())
        },
        DEADLINE,
        config.outer_budget,
    );
    fixture.host = Some(host);
    if let Err(e) = result.and_then(|r| r) {
        fixture.cleanup(config)?;
        return Err(e);
    }
    let counts = fixture.counts();
    assert_eq!(
        (
            counts.selector_calls,
            counts.equality_calls,
            counts.selected_clones,
            counts.deliveries
        ),
        (3, 2, 2, 2)
    );
    fixture.teardown(config)?;
    fixture.owner_facts().validate()?;
    assert_eq!(fixture.counts().selected_dropped, 5);
    Ok(())
}

#[cfg(test)]
/// Counts actual wakes while weak references detect retained, replaced waiters.
struct WakeCount(AtomicU64);
#[cfg(test)]
impl std::task::Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
/// Moves an observation, replaces its waiter and checks both publication and closure wakes.
fn moved_waiter(native: bool) -> Result<(), &'static str> {
    let config = tiny_config(native, false);
    let mut fixture = Fixture::new(config)?;
    profile_observation_registration(&mut fixture, config)?;
    let first = Arc::new(WakeCount(AtomicU64::new(0)));
    let first_weak = Arc::downgrade(&first);
    let a = std::task::Waker::from(Arc::clone(&first));
    let first_baseline = Arc::strong_count(&first);
    pending_next(fixture.observations[0].as_mut().unwrap(), &a)?;
    let mut moved = Box::new(fixture.observations[0].take().unwrap());
    let second = Arc::new(WakeCount(AtomicU64::new(0)));
    let second_weak = Arc::downgrade(&second);
    let b = std::task::Waker::from(Arc::clone(&second));
    let second_baseline = Arc::strong_count(&second);
    pending_next(&mut moved, &b)?;
    assert_eq!(Arc::strong_count(&first), first_baseline);
    drop(a);
    drop(first);
    assert!(first_weak.upgrade().is_none());
    let before = second.0.load(Ordering::Relaxed);
    let host = fixture.host.take().unwrap();
    let result = host.run_ready_bounded(
        async {
            fixture.commit(1).await?;
            assert!(second.0.load(Ordering::Relaxed) > before);
            let value = moved.next().await.ok_or("moved observer closed")?;
            assert_eq!(value.payload, 0);
            drop(value);
            pending_next(&mut moved, &b)?;
            let before_close = second.0.load(Ordering::Relaxed);
            fixture.begin_extraction();
            let state = fixture.extract_and_close(false).await?;
            assert_eq!(state, (1, 1, 1));
            assert!(second.0.load(Ordering::Relaxed) > before_close);
            assert!(moved.next().await.is_none());
            Ok::<(), &'static str>(())
        },
        DEADLINE,
        config.outer_budget,
    );
    fixture.host = Some(host);
    drop(moved);
    assert_eq!(Arc::strong_count(&second), second_baseline);
    drop(b);
    drop(second);
    assert!(second_weak.upgrade().is_none());
    if let Err(e) = result.and_then(|r| r) {
        fixture.cleanup(config)?;
        return Err(e);
    }
    fixture.drop_handles();
    fixture.owner_facts().validate()?;
    Ok(())
}

#[cfg(test)]
/// Registers one custom selector without replacing the real observation path.
async fn register_control_selector(
    fixture: &mut Fixture,
    distinct: bool,
    selector: impl Fn(&State) -> Selected + Send + 'static,
) -> Result<(), &'static str> {
    let store = fixture.store.as_ref().unwrap();
    fixture.observations[0] = Some(
        if distinct {
            store.observe_distinct(selector).await
        } else {
            store.observe(selector).await
        }
        .map_err(|_| "control registration stopped")?,
    );
    fixture.registrations = 1;
    Ok(())
}

#[cfg(test)]
/// Runs the successful selector-triggered root handoff.
fn selector_reentry(native: bool) -> Result<(), &'static str> {
    selector_reentry_case(native, false)
}

#[cfg(test)]
/// Preserves both admitted receipts when a selector re-enters and a drive is rejected.
fn selector_reentry_case(native: bool, reject_handoff: bool) -> Result<(), &'static str> {
    let config = tiny_config(native, false);
    let mut fixture = Fixture::new(config)?;
    let (sender, receiver) = async_channel::bounded(1);
    let store = fixture.cleanup_store.as_ref().unwrap().clone();
    let owner = Arc::new(());
    fixture.registration_owners[0] = Some(Arc::downgrade(&owner));
    let capture = Capture::new(&fixture.probes, owner);
    let sent = AtomicBool::new(false);
    let selector = move |state: &State| {
        assert!(!capture.probes.reducing.load(Ordering::Relaxed));
        if state.revision == 1 && !sent.swap(true, Ordering::Relaxed) {
            sender
                .try_send(store.try_send(Action::Increment(2)).unwrap())
                .unwrap();
        }
        capture.select(state, false)
    };
    let release = Arc::new(AtomicBool::new(false));
    let held = Arc::clone(&release);
    let host = fixture.host.take().unwrap();
    let result = host.run_ready_bounded(
        async {
            register_control_selector(&mut fixture, false, selector).await?;
            fixture.take(0, 0).await?;
            if reject_handoff {
                let receipt = fixture
                    .store
                    .as_ref()
                    .unwrap()
                    .try_send(Action::Increment(1))
                    .unwrap();
                fixture.receipt = Some(Box::pin(async move {
                    let task = receipt.finish().await?;
                    std::future::poll_fn(|_| {
                        if held.load(Ordering::Relaxed) {
                            std::task::Poll::Ready(())
                        } else {
                            std::task::Poll::Pending
                        }
                    })
                    .await;
                    Ok(task)
                }));
                fixture.finish_receipt().await?;
            } else {
                fixture.commit(1).await?;
            }
            let receipt = receiver
                .recv()
                .await
                .map_err(|_| "reentry receipt absent")?;
            fixture.receipt = Some(Box::pin(receipt.finish()));
            fixture.finish_receipt().await?;
            fixture.take(0, 1).await?;
            let scalar = fixture
                .store
                .as_ref()
                .unwrap()
                .read(|s| (s.revision, s.reductions, s.checksum))
                .await
                .map_err(|_| "reentry read stopped")?;
            assert_eq!(scalar, (2, 2, 3));
            assert_eq!(fixture.counts().selector_calls, 3);
            Ok::<(), &'static str>(())
        },
        DEADLINE,
        config.outer_budget,
    );
    fixture.host = Some(host);
    receiver.close();
    if reject_handoff {
        assert_eq!(receiver.len(), 1);
        assert!(fixture.receipt.is_some());
    }
    if let Ok(receipt) = receiver.try_recv() {
        fixture.handoff_receipt = Some(Box::pin(receipt.finish()));
    }
    drop(receiver);
    if let Err(e) = result.and_then(|r| r) {
        if reject_handoff {
            assert!(
                fixture.handoff_receipt.is_some(),
                "queued admitted receipt discarded"
            );
        }
        release.store(true, Ordering::Relaxed);
        fixture.cleanup(config)?;
        if reject_handoff {
            assert_eq!(fixture.cleanup_receipts_settled, 2);
            assert!(
                fixture.receipt.is_none()
                    && fixture.handoff_receipt.is_none()
                    && fixture.task.is_none()
            );
            fixture.owner_facts().validate()?;
        }
        return Err(e);
    }
    fixture.teardown(config)?;
    fixture.owner_facts().validate()
}

#[cfg(all(test, feature = "deterministic"))]
#[test]
fn selector_reentry_queue_owned_receipt_survives_rejected_drive() {
    assert_eq!(
        selector_reentry_case(false, true),
        Err("deterministic ready drive quiescent pending")
    );
}

#[cfg(test)]
/// Checks callback completion, early cancellation and store closure as separate schedules.
fn callback_detach(native: bool) -> Result<(), &'static str> {
    for kind in 0..3 {
        callback_case(native, kind)?;
    }
    Ok(())
}

#[cfg(test)]
/// Runs kind 0 to completion, kind 1 after callback cancellation, or kind 2 through closure.
fn callback_case(native: bool, kind: u8) -> Result<(), &'static str> {
    let config = tiny_config(native, true);
    let mut fixture = Fixture::new(config)?;
    let (sender, receiver) = async_channel::bounded(1);
    let owner = Arc::new(());
    fixture.registration_owners[0] = Some(Arc::downgrade(&owner));
    let capture = Capture::new(&fixture.probes, owner);
    let host = fixture.host.take().unwrap();
    let registration = host.run_ready_bounded(
        register_control_selector(&mut fixture, true, move |s| capture.select(s, false)),
        DEADLINE,
        config.outer_budget,
    );
    fixture.host = Some(host);
    if let Err(error) = registration.and_then(|result| result) {
        fixture.cleanup(config)?;
        return Err(error);
    }
    let store = fixture.cleanup_store.as_ref().unwrap().clone();
    let calls = Arc::new(AtomicU64::new(0));
    let callback_calls = Arc::clone(&calls);
    let probes = Arc::clone(&fixture.probes);
    let observation = fixture.observations[0].take().unwrap();
    // Keep this owning future outside the driven client so a
    // rejected client cannot discard an admitted observation.
    let mut callback = Some(Box::pin(observation.on_change(move |value| {
        assert_eq!(value.payload, 1);
        assert!(!probes.reducing.load(Ordering::Relaxed));
        callback_calls.fetch_add(1, Ordering::Relaxed);
        sender
            .try_send(store.try_send(Action::Increment(3)).unwrap())
            .unwrap();
    })));
    if kind == 1 {
        drop(callback.take());
    }
    let host = fixture.host.take().unwrap();
    let result = host.run_ready_bounded(
        async {
            if kind == 1 {
                fixture.commit(1).await?;
                assert_eq!(fixture.counts().selector_calls, 1);
            } else if kind == 0 {
                let callback = callback.as_mut().unwrap();
                let noop = std::task::Waker::noop();
                assert!(
                    callback
                        .as_mut()
                        .poll(&mut std::task::Context::from_waker(noop))
                        .is_pending()
                );
                fixture.commit(1).await?;
                assert!(
                    callback
                        .as_mut()
                        .poll(&mut std::task::Context::from_waker(noop))
                        .is_pending()
                );
                fixture.commit(2).await?;
                assert!(callback.as_mut().await);
                assert_eq!(calls.load(Ordering::Relaxed), 1);
                let receipt = receiver
                    .recv()
                    .await
                    .map_err(|_| "callback receipt absent")?;
                fixture.receipt = Some(Box::pin(receipt.finish()));
                fixture.finish_receipt().await?;
                assert_eq!(fixture.counts().selector_calls, 3);
                let store = fixture.store.as_ref().unwrap();
                let scalar = store
                    .read(|s| (s.revision, s.reductions, s.checksum))
                    .await
                    .map_err(|_| "callback read stopped")?;
                assert_eq!(scalar, (3, 3, 6));
            } else {
                fixture.commit(1).await?;
                fixture.commit(2).await?;
                // Closure must suppress a queued changed value's unpolled callback.
                assert_eq!(fixture.counts().selector_calls, 3);
                fixture.begin_extraction();
                assert_eq!(fixture.extract_and_close(false).await?, (2, 2, 3));
                assert!(!callback.as_mut().unwrap().as_mut().await);
            }
            assert_eq!(calls.load(Ordering::Relaxed), u64::from(kind == 0));
            Ok::<(), &'static str>(())
        },
        DEADLINE,
        config.outer_budget,
    );
    fixture.host = Some(host);
    drop(callback);
    receiver.close();
    drop(receiver);
    drop(calls);
    if let Err(e) = result.and_then(|r| r) {
        fixture.cleanup(config)?;
        return Err(e);
    }
    if kind == 2 {
        fixture.drop_handles();
        return fixture.owner_facts().validate();
    }
    fixture.teardown(config)?;
    fixture.owner_facts().validate()
}

#[derive(Clone, Copy, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent scalar ownership witnesses, not state-machine flags"
)]
/// Keeps independently measured ownership facts separate instead of inferring one from another.
struct OwnerFacts {
    state_dropped: bool,
    reducer_dropped: bool,
    host_task_dropped: bool,
    registration_owners_released: bool,
    value_owners_released: bool,
    probe_owners_released: bool,
    host_stopped: bool,
}
impl OwnerFacts {
    /// Rejects any retained state, reducer, selector, selected value or host owner.
    fn validate(self) -> Result<(), &'static str> {
        if !self.state_dropped
            || !self.reducer_dropped
            || !self.host_task_dropped
            || !self.registration_owners_released
            || !self.value_owners_released
            || !self.probe_owners_released
            || !self.host_stopped
        {
            return Err("observation final owners invalid");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
/// Copies validated work and lifetime evidence without retaining measured owners.
struct Outcome {
    roots: u64,
    completed: u64,
    cancelled: u64,
    revision: u64,
    reductions: u64,
    checksum: u64,
    cold: Counts,
    work: Counts,
    total: Counts,
    rows: [SubscriberSummary; MAX_SUBSCRIBERS],
    closed_next: u64,
    state_extracted: bool,
    owners: OwnerFacts,
}
impl Outcome {
    /// Checks the final record independently of the live fixture's earlier checks.
    fn validate(self, config: Config) -> Result<(), &'static str> {
        self.owners.validate()?;
        let e = config.expected;
        let n = u64::try_from(config.size).expect("validated subscriber width");
        if !self.state_extracted
            || self.roots != config.roots
            || self.completed != config.roots
            || self.cancelled != 0
            || self.revision != config.roots
            || self.reductions != config.roots
            || self.checksum != e.checksum
            || self.closed_next != n
        {
            return Err("observation final trace invalid");
        }
        let c = self.total;
        if c.selected_originals != e.originals
            || c.selected_clones != e.clones
            || c.selector_calls != e.originals
            || c.equality_calls != e.equality
            || c.selected_dropped != e.created
            || c.selected_live != 0
            || c.deliveries != e.deliveries
            || c.registrations != n
            || c.selector_captures_dropped != n
            || c.selector_captures_live != 0
        {
            return Err("observation final counters invalid");
        }
        if self.cold != e.cold() || self.work != e.work {
            return Err("observation final phase counters invalid");
        }
        for row in &self.rows[..config.size] {
            if *row
                != (SubscriberSummary {
                    deliveries: e.per_deliveries,
                    sum: e.sum,
                    last: config.roots / 2,
                })
            {
                return Err("observation subscriber trace invalid");
            }
        }
        Ok(())
    }
    /// Serialises checked scalar evidence after owners and allocation accounting finish.
    fn json(self, config: Config, allocations: &str) -> String {
        let mut rows = String::new();
        for (i, row) in self.rows[..config.size].iter().enumerate() {
            if i != 0 {
                rows.push(',');
            }
            write!(rows, "[{},{},{}]", row.deliveries, row.sum, row.last)
                .expect("String write succeeds");
        }
        let c = self.total;
        let o = self.owners;
        format!(
            concat!(
                "{{\"schema\":\"rca-observation-performance-profile-v1\",",
                "\"workload\":\"{}\",\"size\":{},\"iterations\":{},\"feature_profile\":\"{}\",",
                "\"features\":{{\"deterministic\":{},\"allocations\":{}}},",
                "\"drive_contract\":\"ready-bounded-v1\",",
                "\"bounds\":{{\"scheduler_task_polls\":{},\"outer_drive_polls\":{},",
                "\"native_deadline_seconds\":10}},\"warmup_roots\":0,",
                "\"roots_committed\":{},\"root_commit_success\":{},\"root_cancelled\":{},",
                "\"responses_committed\":0,\"reducer_calls\":{},\"causal_completed\":{},",
                "\"final_revision\":{},\"root_checksum\":{},\"selected_originals\":{},",
                "\"selected_clones\":{},\"selected_created\":{},\"selected_dropped\":{},",
                "\"selected_live\":{},\"selector_calls\":{},\"equality_calls\":{},",
                "\"deliveries\":{},\"registrations\":{},\"registration_accepted\":{},",
                "\"registration_rejected\":0,\"selector_captures_dropped\":{},",
                "\"closed_next\":{},\"subscribers\":[{}],",
                "\"counters\":{{\"cold\":{},\"work\":{}}},",
                "\"state_extracted\":{},\"state_dropped\":{},\"reducer_dropped\":{},",
                "\"host_task_dropped\":{},\"registration_owners_released\":{},",
                "\"value_owners_released\":{},\"owners_released\":{},",
                "\"host_stopped\":{},\"allocations\":{}}}"
            ),
            config.name,
            config.size,
            config.roots,
            if cfg!(feature = "deterministic") {
                "deterministic-enabled"
            } else {
                "minimal-production"
            },
            cfg!(feature = "deterministic"),
            cfg!(feature = "allocations"),
            config.task_budget,
            config.outer_budget,
            self.roots,
            self.roots,
            self.cancelled,
            self.reductions,
            self.completed,
            self.revision,
            self.checksum,
            c.selected_originals,
            c.selected_clones,
            c.selected_originals + c.selected_clones,
            c.selected_dropped,
            c.selected_live,
            c.selector_calls,
            c.equality_calls,
            c.deliveries,
            c.registrations,
            c.registrations,
            c.selector_captures_dropped,
            self.closed_next,
            rows,
            self.cold.json(),
            self.work.json(),
            self.state_extracted,
            o.state_dropped,
            o.reducer_dropped,
            o.host_task_dropped,
            o.registration_owners_released,
            o.value_owners_released,
            o.probe_owners_released,
            o.host_stopped,
            allocations
        )
    }
}

#[inline(never)]
/// Gives profiler tools a distinct boundary for registration and initial delivery.
fn profile_observation_registration(
    fixture: &mut Fixture,
    config: Config,
) -> Result<(), &'static str> {
    let host = fixture.host.take().ok_or("observation Host absent")?;
    let result = host.run_ready_bounded(
        fixture.register_initial(config),
        DEADLINE,
        config.outer_budget,
    );
    fixture.host = Some(host);
    result?
}

#[inline(never)]
/// Gives profiler tools a distinct boundary for root commits and selected deliveries.
fn profile_observation_work(fixture: &mut Fixture, config: Config) -> Result<(), &'static str> {
    let host = fixture.host.take().ok_or("observation Host absent")?;
    let result = host.run_ready_bounded(fixture.work(config), DEADLINE, config.outer_budget);
    fixture.host = Some(host);
    result?
}

/// Uses an acknowledged store read to check completed work outside its measured interval.
fn validate_fixture(
    fixture: &mut Fixture,
    config: Config,
) -> Result<(u64, u64, u64), &'static str> {
    let host = fixture.host.take().ok_or("observation Host absent")?;
    let result = host.run_ready_bounded(fixture.validate(config), DEADLINE, config.outer_budget);
    fixture.host = Some(host);
    result?
}

/// Compares the extracted state with the validated state and checks all final owners.
fn finish_fixture(
    fixture: &mut Fixture,
    config: Config,
    checked: (u64, u64, u64),
) -> Result<Outcome, &'static str> {
    let extracted = fixture.teardown(config)?;
    let outcome = Outcome {
        roots: fixture.roots,
        completed: fixture.completed,
        cancelled: fixture.cancelled,
        revision: checked.0,
        reductions: checked.1,
        checksum: checked.2,
        cold: fixture.cold,
        work: fixture.work,
        total: fixture.counts(),
        rows: fixture.rows,
        closed_next: fixture.closed_next,
        state_extracted: extracted == checked,
        owners: fixture.owner_facts(),
    };
    outcome.validate(config)?;
    Ok(outcome)
}

/// Emits a successful record only after exact-work validation and final-owner release.
pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    let mut measurement = Measurement::new(if config.native {
        Some(ExecutorCache::Native)
    } else {
        None
    });
    let mut fixture = Fixture::new(config)?;
    measurement.construction_finished();
    let result = (|| {
        profile_observation_registration(&mut fixture, config)?;
        measurement.cold_finished();
        profile_observation_work(&mut fixture, config)?;
        measurement.work_finished();
        let checked = validate_fixture(&mut fixture, config)?;
        measurement.validation_finished();
        finish_fixture(&mut fixture, config, checked)
    })();
    if let Err(error) = result {
        let cleanup = fixture.cleanup(config);
        if let Err(cleanup_error) = cleanup {
            eprintln!(
                "observation original error: {error}; cleanup error: {cleanup_error}; outstanding owners: {:?}",
                fixture.owner_facts()
            );
            return Err("observation drive and cleanup failed");
        }
        return Err(error);
    }
    let outcome = result?;
    // This drops weak control blocks, their storage, and the final probe owner.
    drop(fixture);
    let allocations = measurement
        .finish()?
        .map_or_else(|| "null".to_owned(), |r| r.json());
    Ok(outcome.json(config, &allocations))
}

#[cfg(test)]
/// Mutates copied results only after a complete valid run to challenge final-record checks.
pub(crate) fn scalar_corruption_control(native: bool) -> Result<(), &'static str> {
    let config = tiny_config(native, true);
    let mut fixture = Fixture::new(config)?;
    profile_observation_registration(&mut fixture, config)?;
    profile_observation_work(&mut fixture, config)?;
    let checked = validate_fixture(&mut fixture, config)?;
    let valid = finish_fixture(&mut fixture, config, checked)?;
    valid.validate(config)?;
    for kind in 0..22 {
        let mut broken = valid;
        match kind {
            0 => broken.total.selector_calls -= 1,
            1 => broken.total.equality_calls -= 1,
            2 => broken.total.selected_clones -= 1,
            3 => broken.total.deliveries -= 1,
            4 => broken.rows[0].sum += 1,
            5 => broken.rows[0].last += 1,
            6 => broken.state_extracted = false,
            7 => broken.total.selected_dropped -= 1,
            8 => broken.total.selected_live += 1,
            9 => broken.owners.reducer_dropped = false,
            10 => broken.owners.host_task_dropped = false,
            11 => broken.owners.registration_owners_released = false,
            12 => broken.owners.state_dropped = false,
            13 => broken.owners.value_owners_released = false,
            14 => broken.owners.probe_owners_released = false,
            15 => broken.roots -= 1,
            16 => broken.completed -= 1,
            17 => broken.checksum += 1,
            18 => broken.total.selector_captures_dropped -= 1,
            19 => broken.total.selected_originals -= 1,
            20 => broken.cold.selector_calls -= 1,
            21 => broken.work.selected_clones -= 1,
            _ => unreachable!(),
        }
        assert!(
            broken.validate(config).is_err(),
            "accepted scalar corruption {kind}: {broken:?}"
        );
    }
    Ok(())
}

#[cfg(all(test, feature = "allocations"))]
/// Holds a real selector owner past teardown so allocation and weak-owner checks must fail.
pub(crate) fn retained_owner_control() -> Result<(), &'static str> {
    let config = tiny_config(!cfg!(feature = "deterministic"), true);
    let mut measurement = Measurement::new(if config.native {
        Some(ExecutorCache::Native)
    } else {
        None
    });
    let mut fixture = Fixture::new(config)?;
    measurement.construction_finished();
    profile_observation_registration(&mut fixture, config)?;
    measurement.cold_finished();
    let retained = fixture.registration_owners[0]
        .as_ref()
        .unwrap()
        .upgrade()
        .unwrap();
    let weak = Arc::downgrade(&retained);
    profile_observation_work(&mut fixture, config)?;
    measurement.work_finished();
    validate_fixture(&mut fixture, config)?;
    measurement.validation_finished();
    fixture.teardown(config)?;
    assert_eq!(
        fixture.owner_facts().validate(),
        Err("observation final owners invalid")
    );
    assert_eq!(fixture.counts().selected_live, 0);
    assert_eq!(fixture.counts().selector_captures_dropped, 1);
    drop(fixture);
    let error = measurement
        .finish()
        .err()
        .ok_or("retained observation owner falsely accepted")?;
    assert_eq!(
        error,
        if config.native {
            "unowned allocation retention after teardown"
        } else {
            "unexplained allocation retention after teardown"
        }
    );
    drop(retained);
    assert!(weak.upgrade().is_none());
    drop(weak);
    Ok(())
}

#[cfg(all(test, feature = "deterministic"))]
/// Rejects a drive while an actual terminal result or receipt remains externally owned.
pub(crate) fn admitted_results_control() -> Result<(), &'static str> {
    for terminal in [true, false] {
        let config = tiny_config(false, true);
        let mut fixture = Fixture::new(config)?;
        profile_observation_registration(&mut fixture, config)?;
        let release = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&release);
        if terminal {
            fixture.begin_extraction();
            let admitted = fixture.extraction.take().unwrap();
            fixture.extraction = Some(Box::pin(async move {
                let state = admitted.await?;
                // Retain the actual terminal result outside the borrowed client.
                std::future::poll_fn(|_| {
                    if held.load(Ordering::Relaxed) {
                        std::task::Poll::Ready(())
                    } else {
                        std::task::Poll::Pending
                    }
                })
                .await;
                Ok(state)
            }));
        } else {
            let receipt = fixture
                .store
                .as_ref()
                .unwrap()
                .try_send(Action::Increment(1))
                .unwrap();
            fixture.receipt = Some(Box::pin(async move {
                let task = receipt.finish().await?;
                std::future::poll_fn(|_| {
                    if held.load(Ordering::Relaxed) {
                        std::task::Poll::Ready(())
                    } else {
                        std::task::Poll::Pending
                    }
                })
                .await;
                Ok(task)
            }));
        }
        let host = fixture.host.take().unwrap();
        let result = if terminal {
            host.run_ready_bounded(
                fixture.extract_and_close(false),
                DEADLINE,
                config.outer_budget,
            )
            .map(|_| ())
        } else {
            host.run_ready_bounded(
                async { fixture.receipt.as_mut().unwrap().as_mut().await },
                DEADLINE,
                config.outer_budget,
            )
            .map(|_| ())
        };
        fixture.host = Some(host);
        assert_eq!(result, Err("deterministic ready drive quiescent pending"));
        assert_eq!(fixture.probes.state_dropped.load(Ordering::Relaxed), 0);
        assert!(!fixture.owner_facts().host_task_dropped);
        if terminal {
            assert!(fixture.extraction.is_some());
        } else {
            assert!(fixture.receipt.is_some());
        }
        release.store(true, Ordering::Relaxed);
        fixture.cleanup(config)?;
        fixture.owner_facts().validate()?;
    }
    Ok(())
}

#[cfg(all(test, feature = "deterministic"))]
#[test]
fn partially_registered_capture_owners_release_after_drive_rejection() {
    let config = Config::parse(
        ["observe-distinct-deterministic", "16", "2"]
            .into_iter()
            .map(str::to_owned),
    )
    .unwrap();
    let mut fixture = Fixture::new(config).unwrap();
    let host = fixture.host.take().unwrap();
    let error = host.run_ready_bounded(fixture.register_initial(config), DEADLINE, 1);
    fixture.host = Some(host);
    assert_eq!(
        error,
        Err("deterministic ready drive outer poll budget exhausted")
    );
    let created = fixture.probes.captures_created.load(Ordering::Relaxed);
    let acknowledged = fixture.registrations;
    assert!(created > acknowledged);
    assert!(acknowledged < config.size as u64);
    assert_eq!(fixture.roots, 0);
    fixture.cleanup(config).unwrap();
    fixture.owner_facts().validate().unwrap();
    assert_eq!(fixture.counts().selector_captures_live, 0);
    assert_eq!(fixture.counts().registrations, acknowledged);
    assert_eq!(
        fixture.probes.captures_dropped.load(Ordering::Relaxed),
        created
    );
    assert_eq!(
        fixture.counts().selected_originals + fixture.counts().selected_clones,
        fixture.counts().selected_dropped
    );
}
