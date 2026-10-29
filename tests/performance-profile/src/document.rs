//! Coalesced document saves with separate physical-work and delivery ownership.
//!
//! Each burst holds its first save while later edits arrive. The workload then
//! checks the first and latest writes, the flush receipt and complete teardown.
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_channel::{Receiver, Sender};
use rca_core::{ActionTask, Effect, ReduceContext, Reducer, Store};
use rca_persistence::{Action, Backend, Codec, Document, LoadStatus, Operation, State};

use crate::host::Host;
use crate::measurement::{ExecutorCache, Measurement};

#[cfg(test)]
#[path = "document_controls.rs"]
mod controls;

// These finite dimensions and per-drive limits are the reviewed H contract.
const DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
pub(crate) struct Config {
    name: &'static str,
    /// Edits admitted while the first save in each burst is held.
    b: u64,
    /// Number of complete bursts.
    i: u64,
    /// Bytes in every payload, including its eight-byte tag.
    p: usize,
    native: bool,
    /// Deterministic task-poll budget, derived from the bounded dimensions.
    scheduler: usize,
    /// Separate client-drive poll budget; not a fresh budget for each task poll.
    outer: usize,
}
impl Config {
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let (name, p, native) = match args.next().as_deref() {
            Some("document-coalesced-native") => ("document-coalesced-native", 4096, true),
            Some("document-coalesced-deterministic") => {
                ("document-coalesced-deterministic", 4096, false)
            }
            Some("document-coalesced-8-native") => ("document-coalesced-8-native", 8, true),
            Some("document-coalesced-8-deterministic") => {
                ("document-coalesced-8-deterministic", 8, false)
            }
            Some("document-coalesced-65536-native") => {
                ("document-coalesced-65536-native", 65536, true)
            }
            Some("document-coalesced-65536-deterministic") => {
                ("document-coalesced-65536-deterministic", 65536, false)
            }
            _ => return Err("unknown document workload"),
        };
        if !native && !cfg!(feature = "deterministic") {
            return Err("deterministic workloads require the deterministic feature");
        }
        let b = crate::workloads::integer(args.next())?;
        let i = crate::workloads::integer(args.next())?;
        if !(1..=20).contains(&i)
            || if p == 4096 {
                !(matches!(b, 1 | 2 | 16 | 64) || b == 1000 && i == 1)
            } else {
                b != 2 || i != 1
            }
            || args.next().is_some()
        {
            return Err(
                "document requires SIZE=1,2,16,64 and ITERATIONS=1..20, or 1000,1; P8/P65536 require 2,1",
            );
        }
        let scheduler = i
            .checked_mul(b.checked_add(3).ok_or("document budget overflow")?)
            .and_then(|n| n.checked_add(2))
            .and_then(|n| n.checked_mul(64))
            .and_then(|n| n.checked_add(4096))
            .ok_or("document budget overflow")?;
        let outer = scheduler.checked_add(1).ok_or("document budget overflow")?;
        Ok(Self {
            name,
            b,
            i,
            p,
            native,
            scheduler: usize::try_from(scheduler).map_err(|_| "document budget overflow")?,
            outer: usize::try_from(outer).map_err(|_| "document budget overflow")?,
        })
    }
}

#[derive(Default)]
struct Probes {
    originals: AtomicU64,
    clones: AtomicU64,
    dropped: AtomicU64,
    reductions: AtomicU64,
    loads: AtomicU64,
    saves: AtomicU64,
    encodes: AtomicU64,
    decodes: AtomicU64,
    physical_started: AtomicU64,
    physical_dropped: AtomicU64,
    waiter_started: AtomicU64,
    waiter_dropped: AtomicU64,
    fail_encode: AtomicBool,
}
fn count(value: &AtomicU64) -> u64 {
    value.load(Ordering::Relaxed)
}
fn bump(value: &AtomicU64) {
    value.fetch_add(1, Ordering::Relaxed);
}
fn bytes(tag: u64, p: usize) -> Vec<u8> {
    let mut bytes = vec![0; p];
    bytes[..8].copy_from_slice(&tag.to_le_bytes());
    for (j, byte) in bytes.iter_mut().enumerate().skip(8) {
        *byte = u8::try_from((tag + u64::try_from(j).expect("bounded payload")) % 251)
            .expect("modulo 251");
    }
    bytes
}
fn tag(bytes: &[u8], p: usize) -> Result<u64, &'static str> {
    if bytes.len() != p || p < 8 {
        return Err("document encoded length invalid");
    }
    let tag = u64::from_le_bytes(bytes[..8].try_into().expect("eight bytes"));
    for (j, byte) in bytes.iter().enumerate().skip(8) {
        if u64::from(*byte) != (tag + u64::try_from(j).expect("bounded payload")) % 251 {
            return Err("document encoded byte invalid");
        }
    }
    Ok(tag)
}
fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().map(|b| u64::from(*b)).sum()
}

/// Counts deep copies separately from original values and observes every release.
struct Payload {
    bytes: Vec<u8>,
    probes: Arc<Probes>,
}
impl Payload {
    fn new(bytes: Vec<u8>, probes: &Arc<Probes>) -> Self {
        bump(&probes.originals);
        Self {
            bytes,
            probes: Arc::clone(probes),
        }
    }
}
impl Clone for Payload {
    fn clone(&self) -> Self {
        bump(&self.probes.clones);
        Self {
            bytes: self.bytes.clone(),
            probes: Arc::clone(&self.probes),
        }
    }
}
impl Drop for Payload {
    fn drop(&mut self) {
        bump(&self.probes.dropped);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlledError {
    #[cfg(test)]
    Load,
    #[cfg(test)]
    Save,
    Encode,
    Decode,
    AdmissionClosed,
    DeliveryClosed,
}
impl fmt::Display for ControlledError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "controlled {self:?}")
    }
}
impl Error for ControlledError {}

struct CountedCodec {
    probes: Arc<Probes>,
    p: usize,
    _owner: Arc<()>,
}
impl Codec<Payload> for CountedCodec {
    type Error = ControlledError;
    fn encode(&self, value: &Payload) -> Result<Vec<u8>, Self::Error> {
        bump(&self.probes.encodes);
        if self.probes.fail_encode.swap(false, Ordering::Relaxed) {
            return Err(ControlledError::Encode);
        }
        tag(&value.bytes, self.p).map_err(|_| ControlledError::Encode)?;
        Ok(value.bytes.clone())
    }
    fn decode(&self, bytes: &[u8]) -> Result<Payload, Self::Error> {
        bump(&self.probes.decodes);
        tag(bytes, self.p).map_err(|_| ControlledError::Decode)?;
        Ok(Payload::new(bytes.to_vec(), &self.probes))
    }
}

// A physical request remains independent of the cancelled delivery future.
/// Keeps physical request debt alive even after its reply receiver is cancelled.
struct Physical {
    probes: Arc<Probes>,
    notify: Sender<()>,
}
impl Physical {
    fn new(probes: &Arc<Probes>, notify: &Sender<()>) -> Self {
        bump(&probes.physical_started);
        Self {
            probes: Arc::clone(probes),
            notify: notify.clone(),
        }
    }
}
impl Drop for Physical {
    fn drop(&mut self) {
        bump(&self.probes.physical_dropped);
        // One bounded notification suffices: join rechecks all scalar guards.
        let _ = self.notify.try_send(());
    }
}
/// Tracks the backend delivery future, independently of its physical request.
struct Waiter {
    probes: Arc<Probes>,
    notify: Sender<()>,
}
impl Waiter {
    fn new(probes: &Arc<Probes>, notify: &Sender<()>) -> Self {
        bump(&probes.waiter_started);
        Self {
            probes: Arc::clone(probes),
            notify: notify.clone(),
        }
    }
}
impl Drop for Waiter {
    fn drop(&mut self) {
        bump(&self.probes.waiter_dropped);
        let _ = self.notify.try_send(());
    }
}
/// Transfers physical ownership to the controller while retaining a reply route.
enum Request {
    Load {
        reply: Sender<Result<Option<Vec<u8>>, ControlledError>>,
        _physical: Physical,
    },
    Save {
        bytes: Vec<u8>,
        reply: Sender<Result<(), ControlledError>>,
        _physical: Physical,
    },
}
struct ControlledBackend {
    requests: Sender<Request>,
    notify: Sender<()>,
    probes: Arc<Probes>,
    _owner: Arc<()>,
}
impl Backend for ControlledBackend {
    type Error = ControlledError;
    async fn load(&self) -> Result<Option<Vec<u8>>, Self::Error> {
        bump(&self.probes.loads);
        let _waiter = Waiter::new(&self.probes, &self.notify);
        let (reply, receiver) = async_channel::bounded(1);
        self.requests
            .send(Request::Load {
                reply,
                _physical: Physical::new(&self.probes, &self.notify),
            })
            .await
            .map_err(|_| ControlledError::AdmissionClosed)?;
        receiver
            .recv()
            .await
            .map_err(|_| ControlledError::DeliveryClosed)?
    }
    async fn save(&self, bytes: Vec<u8>) -> Result<(), Self::Error> {
        bump(&self.probes.saves);
        let _waiter = Waiter::new(&self.probes, &self.notify);
        let (reply, receiver) = async_channel::bounded(1);
        self.requests
            .send(Request::Save {
                bytes,
                reply,
                _physical: Physical::new(&self.probes, &self.notify),
            })
            .await
            .map_err(|_| ControlledError::AdmissionClosed)?;
        receiver
            .recv()
            .await
            .map_err(|_| ControlledError::DeliveryClosed)?
    }
}
/// Holds one physical operation until the workload explicitly completes it.
struct Controller {
    requests: Receiver<Request>,
    notify: Receiver<()>,
    probes: Arc<Probes>,
    held: Option<Request>,
    storage: Option<Vec<u8>>,
    writes: u64,
    write_sum: u64,
    saved_bytes: u64,
    loaded_bytes: u64,
    p: usize,
    fenced: bool,
}
impl Controller {
    async fn receive(&mut self) -> Result<(), &'static str> {
        if self.held.is_some() {
            return Err("document controller already holds request");
        }
        self.held = Some(
            self.requests
                .recv()
                .await
                .map_err(|_| "document request admission closed")?,
        );
        Ok(())
    }
    fn held_tag(&self) -> Result<u64, &'static str> {
        match self.held.as_ref() {
            Some(Request::Save { bytes, .. }) => tag(bytes, self.p),
            _ => Err("document expected held save"),
        }
    }
    fn load(
        &mut self,
        result: Result<Option<Vec<u8>>, ControlledError>,
    ) -> Result<(), &'static str> {
        let Some(Request::Load { reply, _physical }) = self.held.take() else {
            return Err("document expected load");
        };
        if let Ok(Some(bytes)) = &result {
            self.loaded_bytes += u64::try_from(bytes.len()).expect("bounded bytes");
        }
        reply
            .try_send(result)
            .map_err(|_| "document load delivery rejected")?;
        Ok(())
    }
    fn save(
        &mut self,
        expected: u64,
        result: Result<(), ControlledError>,
    ) -> Result<(), &'static str> {
        if self.held_tag()? != expected {
            return Err("document save tag invalid");
        }
        let Some(Request::Save {
            bytes,
            reply,
            _physical,
        }) = self.held.take()
        else {
            return Err("document expected save");
        };
        if result.is_ok() {
            self.writes += 1;
            self.write_sum += expected;
            self.saved_bytes += u64::try_from(bytes.len()).expect("bounded bytes");
            self.storage = Some(bytes);
        }
        reply
            .try_send(result)
            .map_err(|_| "document save delivery rejected")?;
        Ok(())
    }
    /// Returns outstanding physical requests and delivery futures, in that order.
    fn pending(&self) -> (u64, u64) {
        (
            count(&self.probes.physical_started) - count(&self.probes.physical_dropped),
            count(&self.probes.waiter_started) - count(&self.probes.waiter_dropped),
        )
    }
    /// Stops new requests without pretending that already held work has finished.
    fn fence(&mut self) {
        self.fenced = true;
        self.requests.close();
    }
    /// Requires closed admission, then waits for both ownership debts to clear.
    async fn join(&self) -> Result<(), &'static str> {
        if !self.fenced {
            return Err("document physical join requires admission fence");
        }
        while self.pending() != (0, 0) {
            self.notify
                .recv()
                .await
                .map_err(|_| "document physical join notification closed")?;
        }
        Ok(())
    }
    /// Waits for delivery cancellation without releasing held physical work.
    async fn wait_delivery_drop(&self) -> Result<(), &'static str> {
        while self.pending().1 != 0 {
            self.notify
                .recv()
                .await
                .map_err(|_| "document delivery drop notification closed")?;
        }
        Ok(())
    }
}

struct CountedDocument {
    document: Document<Payload, ControlledBackend, CountedCodec>,
    probes: Arc<Probes>,
    _owner: Arc<()>,
}
impl Reducer for CountedDocument {
    type State = State<Payload>;
    type Action = Action<Payload>;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        bump(&self.probes.reductions);
        self.document.reduce(state, action, context)
    }
}
type DocumentStore = Store<State<Payload>, Action<Payload>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Copies state and byte-validation results without retaining a payload owner.
struct Projection {
    tag: u64,
    checksum: u64,
    valid: Result<(), &'static str>,
    revision: u64,
    ack: Option<u64>,
    status: LoadStatus,
    saving: Option<u64>,
    attempt: Option<u64>,
    failure: Option<(Operation, Option<u64>)>,
    barriers: usize,
    closing: bool,
    closed: bool,
    aborted: bool,
}
fn project(state: &State<Payload>) -> Projection {
    let value = state.value();
    let decoded = tag(&value.bytes, value.bytes.len());
    Projection {
        tag: decoded.unwrap_or(0),
        checksum: checksum(&value.bytes),
        valid: decoded.map(|_| ()),
        revision: state.revision(),
        ack: state.acknowledged_revision(),
        status: state.load_status(),
        saving: state.saving_revision(),
        attempt: state
            .active_attempt()
            .map(rca_persistence::Attempt::identifier),
        failure: state.failure().map(|f| (f.operation(), f.revision())),
        barriers: state.pending_barrier_count(),
        closing: state.is_closing(),
        closed: state.is_closed(),
        aborted: state.is_aborted(),
    }
}
fn clean(projection: Projection, revision: u64, expected_tag: u64) -> Result<(), &'static str> {
    if projection.valid.is_err()
        || projection.tag != expected_tag
        || projection.revision != revision
        || projection.ack != Some(revision)
        || projection.status != LoadStatus::Ready
        || projection.saving.is_some()
        || projection.attempt.is_some()
        || projection.failure.is_some()
        || projection.barriers != 0
        || projection.closing
        || projection.closed
        || projection.aborted
    {
        return Err("document final projection invalid");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
/// Cumulative logical work; cold counts are subtracted before checking a burst.
struct Counts {
    roots: u64,
    responses: u64,
    reductions: u64,
    completed: u64,
    loads: u64,
    decodes: u64,
    encodes: u64,
    saves: u64,
    writes: u64,
    receipts: u64,
    clones: u64,
    dropped: u64,
}
impl Counts {
    fn minus(self, before: Self) -> Self {
        Self {
            roots: self.roots - before.roots,
            responses: self.responses - before.responses,
            reductions: self.reductions - before.reductions,
            completed: self.completed - before.completed,
            loads: self.loads - before.loads,
            decodes: self.decodes - before.decodes,
            encodes: self.encodes - before.encodes,
            saves: self.saves - before.saves,
            writes: self.writes - before.writes,
            receipts: self.receipts - before.receipts,
            clones: self.clones - before.clones,
            dropped: self.dropped - before.dropped,
        }
    }
    fn json(self) -> String {
        format!(
            "{{\"roots\":{},\"responses\":{},\"reductions\":{},\"completed\":{},\"loads\":{},\"decodes\":{},\"encodes\":{},\"saves\":{},\"writes\":{},\"receipts\":{},\"clones\":{},\"dropped\":{}}}",
            self.roots,
            self.responses,
            self.reductions,
            self.completed,
            self.loads,
            self.decodes,
            self.encodes,
            self.saves,
            self.writes,
            self.receipts,
            self.clones,
            self.dropped
        )
    }
}

/// Owns prepared inputs, admitted tasks and all resources needed for bounded cleanup.
struct Fixture {
    host: Option<Host>,
    store: Option<DocumentStore>,
    controller: Controller,
    probes: Arc<Probes>,
    /// Backend, codec and reducer owners, checked after their strong handles drop.
    owners: [Weak<()>; 3],
    host_owner: Weak<()>,
    config: Config,
    /// All burst inputs, allocated before the construction phase ends.
    inputs: VecDeque<Payload>,
    cold_input: Option<Vec<u8>>,
    first_task: Option<ActionTask>,
    task: Option<ActionTask>,
    roots: u64,
    completed: u64,
    receipts: u64,
    #[cfg(test)]
    fault: Option<controls::Fault>,
}
impl Fixture {
    fn new(config: Config) -> Result<Self, &'static str> {
        let probes = Arc::new(Probes::default());
        let backend_owner = Arc::new(());
        let codec_owner = Arc::new(());
        let reducer_owner = Arc::new(());
        let owners = [
            Arc::downgrade(&backend_owner),
            Arc::downgrade(&codec_owner),
            Arc::downgrade(&reducer_owner),
        ];
        let (requests, receiver) = async_channel::bounded(1);
        let (notify, notified) = async_channel::bounded(1);
        let backend = ControlledBackend {
            requests,
            notify,
            probes: Arc::clone(&probes),
            _owner: backend_owner,
        };
        let codec = CountedCodec {
            probes: Arc::clone(&probes),
            p: config.p,
            _owner: codec_owner,
        };
        let (state, document) =
            Document::new(Payload::new(bytes(0, config.p), &probes), backend, codec);
        let reducer = CountedDocument {
            document,
            probes: Arc::clone(&probes),
            _owner: reducer_owner,
        };
        let (host, runtime, host_owner) = Host::new(config.native, config.scheduler)?;
        let store = Store::new(state, reducer, runtime);
        let inputs = (1..=config.b * config.i)
            .map(|n| Payload::new(bytes(n, config.p), &probes))
            .collect();
        Ok(Self {
            host: Some(host),
            store: Some(store),
            controller: Controller {
                requests: receiver,
                notify: notified,
                probes: Arc::clone(&probes),
                held: None,
                storage: None,
                writes: 0,
                write_sum: 0,
                saved_bytes: 0,
                loaded_bytes: 0,
                p: config.p,
                fenced: false,
            },
            probes,
            owners,
            host_owner,
            config,
            inputs,
            cold_input: Some(bytes(7, config.p)),
            first_task: None,
            task: None,
            roots: 0,
            completed: 0,
            receipts: 0,
            #[cfg(test)]
            fault: None,
        })
    }
    /// Applies the selected host's deadline and independent outer-drive bound.
    fn drive<F>(&self, future: F) -> Result<F::Output, &'static str>
    where
        F: Future + Send,
        F::Output: Send,
    {
        self.host
            .as_ref()
            .ok_or("document host absent")?
            .run_ready_bounded(future, DEADLINE, self.config.outer)
    }
    fn counts(&self) -> Counts {
        let reductions = count(&self.probes.reductions);
        Counts {
            roots: self.roots,
            responses: reductions - self.roots,
            reductions,
            completed: self.completed,
            loads: count(&self.probes.loads),
            decodes: count(&self.probes.decodes),
            encodes: count(&self.probes.encodes),
            saves: count(&self.probes.saves),
            writes: self.controller.writes,
            receipts: self.receipts,
            clones: count(&self.probes.clones),
            dropped: count(&self.probes.dropped),
        }
    }
    fn commit(&mut self, action: Action<Payload>) -> Result<ActionTask, &'static str> {
        let task = self
            .drive(
                self.store
                    .as_ref()
                    .ok_or("document Store absent")?
                    .send(action),
            )?
            .map_err(|_| "document root stopped")?;
        self.roots += 1;
        Ok(task)
    }
    /// Counts completion only after rejecting cancellation and unfinished work.
    fn finish_task(&mut self, task: &ActionTask) -> Result<(), &'static str> {
        self.drive(task.finish())?;
        if !task.is_finished() || task.is_cancelled() {
            return Err("document success task unfinished or cancelled");
        }
        self.completed += 1;
        Ok(())
    }
    fn receive(&mut self) -> Result<(), &'static str> {
        self.host.as_ref().expect("host").run_ready_bounded(
            self.controller.receive(),
            DEADLINE,
            self.config.outer,
        )?
    }
    fn read(&self) -> Result<Projection, &'static str> {
        self.drive(
            self.store
                .as_ref()
                .ok_or("document Store absent")?
                .read(project),
        )?
        .map_err(|_| "document read stopped")
    }
    fn cold(&mut self) -> Result<Counts, &'static str> {
        self.first_task = Some(self.commit(Action::load())?);
        self.receive()?;
        self.controller.load(Ok(self.cold_input.take()))?;
        let task = self.first_task.take().expect("load task");
        self.finish_task(&task)?;
        clean(self.read()?, 0, 7)?;
        let cold = self.counts();
        if cold
            != (Counts {
                roots: 1,
                responses: 1,
                reductions: 2,
                completed: 1,
                loads: 1,
                decodes: 1,
                clones: 1,
                dropped: 2,
                ..Counts::default()
            })
        {
            return Err("document cold counters invalid");
        }
        Ok(cold)
    }
    fn flush(&mut self, revision: u64) -> Result<(), &'static str> {
        let (action, receipt) = Action::flush();
        let task = self.commit(action)?;
        self.finish_task(&task)?;
        let outcome = self.drive(receipt)?.map_err(|_| "document flush failed")?;
        if outcome.revision != revision || outcome.closing {
            return Err("document receipt revision invalid");
        }
        self.receipts += 1;
        Ok(())
    }
    /// Cancels delivery, releases physical work and checks owners after rejection.
    fn cleanup(&mut self) -> Result<(), &'static str> {
        if self.host.is_none() {
            return self.check_owners();
        }
        // Keep admitted work externally reachable when a ready drive rejects.
        self.controller.fence();
        if let Some(task) = &self.first_task {
            task.cancel();
        }
        if let Some(task) = &self.task {
            task.cancel();
        }
        if self.store.is_some() {
            let task = self.commit(Action::abort())?;
            self.drive(task.finish())?;
        }
        self.controller.held = None;
        while let Ok(request) = self.controller.requests.try_recv() {
            drop(request);
        }
        self.drive(self.controller.wait_delivery_drop())??;
        self.drive(self.controller.join())??;
        if let Some(task) = self.first_task.take() {
            self.drive(task.finish())?;
        }
        if let Some(task) = self.task.take() {
            self.drive(task.finish())?;
        }
        if let Some(store) = self.store.take() {
            drop(
                self.drive(store.shutdown_into_state())?
                    .map_err(|_| "document cleanup shutdown stopped")?,
            );
        }
        self.inputs.clear();
        self.cold_input = None;
        self.controller.storage = None;
        self.host = None;
        self.check_owners()
    }
    /// Requires zero task/resource owners and exact payload create/drop balance.
    fn check_owners(&self) -> Result<(), &'static str> {
        if self.controller.pending() != (0, 0)
            || self.owners.iter().any(|weak| weak.strong_count() != 0)
            || self.host_owner.strong_count() != 0
            || count(&self.probes.originals) + count(&self.probes.clones)
                != count(&self.probes.dropped)
        {
            return Err("document owners retained after teardown");
        }
        Ok(())
    }
    /// Joins successful work and checks that shutdown preserves the validated state.
    fn teardown(&mut self, projection: Projection) -> Result<(), &'static str> {
        self.controller.fence();
        self.drive(self.controller.join())??;
        let store = self.store.take().ok_or("document Store absent")?;
        let state = self
            .drive(store.shutdown_into_state())?
            .map_err(|_| "document shutdown stopped")?;
        let extracted = project(&state);
        drop(state);
        if extracted != projection {
            return Err("document extracted state differs");
        }
        self.controller.storage = None;
        self.host = None;
        self.check_owners()
    }
}

/// Preserves a work error unless cleanup also fails; never emits a success record.
fn reject(fixture: &mut Fixture, original: &'static str) -> &'static str {
    match fixture.cleanup() {
        Ok(()) => original,
        Err(cleanup) => {
            eprintln!(
                "document rejected: {original}; cleanup rejected: {cleanup}; outstanding physical/delivery={:?}; payload live={}; owners={:?}; host={}",
                fixture.controller.pending(),
                count(&fixture.probes.originals) + count(&fixture.probes.clones)
                    - count(&fixture.probes.dropped),
                fixture.owners.each_ref().map(Weak::strong_count),
                fixture.host_owner.strong_count()
            );
            "document rejection and cleanup failure"
        }
    }
}

#[inline(never)]
/// Holds each first save across its burst, then persists only the latest edit.
fn profile_document_work(fixture: &mut Fixture, config: Config) -> Result<(), &'static str> {
    let saves_per_burst = if config.b == 1 { 1 } else { 2 };
    for k in 1..=config.i {
        let a = (k - 1) * config.b + 1;
        let z = k * config.b;
        let input = fixture
            .inputs
            .pop_front()
            .ok_or("document action input absent")?;
        fixture.first_task = Some(fixture.commit(Action::set(input))?);
        fixture.receive()?;
        #[cfg(test)]
        if fixture.fault == Some(controls::Fault::WrongByte)
            && let Some(Request::Save { bytes, .. }) = &mut fixture.controller.held
        {
            bytes[8] ^= 1;
        }
        if fixture.controller.held_tag()? != a {
            return Err("document first save invalid");
        }
        if count(&fixture.probes.originals) + count(&fixture.probes.clones)
            - count(&fixture.probes.dropped)
            != config.b * config.i - a + 1
        {
            return Err("document held save retains payload snapshot");
        }
        #[cfg(test)]
        if fixture.fault == Some(controls::Fault::UnfinishedTask) {
            fixture.drive(
                fixture
                    .first_task
                    .as_ref()
                    .expect("held first task")
                    .finish(),
            )?;
            return Err("document unexpectedly finished held task");
        }
        for _ in a + 1..=z {
            let input = fixture
                .inputs
                .pop_front()
                .ok_or("document action input absent")?;
            let task = fixture.commit(Action::set(input))?;
            fixture.finish_task(&task)?;
        }
        if count(&fixture.probes.saves) != saves_per_burst * (k - 1) + 1
            || fixture.controller.pending() != (1, 1)
            || !fixture.controller.requests.is_empty()
        {
            return Err("document extra intermediate save");
        }
        #[cfg(test)]
        if fixture.fault == Some(controls::Fault::MissingSecondSave) {
            fixture.probes.fail_encode.store(true, Ordering::Relaxed);
        }
        fixture.controller.save(a, Ok(()))?;
        // A single edit is already the latest revision: it cannot coalesce.
        if config.b > 1 {
            fixture.receive()?;
            if count(&fixture.probes.originals) + count(&fixture.probes.clones)
                - count(&fixture.probes.dropped)
                != config.b * config.i - z + 1
            {
                return Err("document held latest save retains payload snapshot");
            }
            fixture.controller.save(z, Ok(()))?;
        }
        #[cfg(test)]
        if fixture.fault == Some(controls::Fault::CancelledTask) {
            fixture.first_task.as_ref().expect("first task").cancel();
        }
        let task = fixture.first_task.take().expect("first save task");
        fixture.finish_task(&task)?;
        if count(&fixture.probes.saves) != saves_per_burst * k
            || !fixture.controller.requests.is_empty()
        {
            return Err("document extra save");
        }
        fixture.flush(z)?;
    }
    Ok(())
}

fn validate_work(fixture: &Fixture, cold: Counts, config: Config) -> Result<Counts, &'static str> {
    let work = fixture.counts().minus(cold);
    validate_counts(work, config)?;
    let saves = config.i * if config.b == 1 { 1 } else { 2 };
    let write_sum = if config.b == 1 {
        config.i * (config.i + 1) / 2
    } else {
        config.b * config.i * config.i + config.i
    };
    if fixture.controller.write_sum != write_sum
        || fixture.controller.saved_bytes != saves * u64::try_from(config.p).expect("payload")
        || fixture.controller.loaded_bytes != u64::try_from(config.p).expect("payload")
        || count(&fixture.probes.physical_started) != saves + 1
        || count(&fixture.probes.physical_dropped) != saves + 1
        || count(&fixture.probes.waiter_started) != saves + 1
        || count(&fixture.probes.waiter_dropped) != saves + 1
    {
        return Err("document work counters invalid");
    }
    Ok(work)
}
fn validate_counts(work: Counts, config: Config) -> Result<(), &'static str> {
    let e = config.b * config.i;
    let saves = config.i * if config.b == 1 { 1 } else { 2 };
    let expected = Counts {
        roots: config.i * (config.b + 1),
        responses: saves,
        reductions: e + config.i + saves,
        completed: config.i * (config.b + 1),
        encodes: saves,
        saves,
        writes: saves,
        receipts: config.i,
        clones: e + saves,
        dropped: 2 * e + saves,
        ..Counts::default()
    };
    if work != expected {
        return Err("document work counters invalid");
    }
    Ok(())
}

pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    let mut measurement = Measurement::new(if config.native {
        Some(ExecutorCache::Native)
    } else {
        None
    });
    let mut fixture = Fixture::new(config)?;
    measurement.construction_finished();
    let prepared = fixture.cold();
    let cold = match prepared {
        Ok(cold) => cold,
        Err(error) => {
            return Err(reject(&mut fixture, error));
        }
    };
    measurement.cold_finished();
    if let Err(error) = profile_document_work(&mut fixture, config) {
        return Err(reject(&mut fixture, error));
    }
    measurement.work_finished();
    let checked = (|| {
        let work = validate_work(&fixture, cold, config)?;
        let projection = fixture.read()?;
        clean(projection, config.b * config.i, config.b * config.i)?;
        Ok((work, projection))
    })();
    let (work, projection) = match checked {
        Ok(values) => values,
        Err(error) => {
            return Err(reject(&mut fixture, error));
        }
    };
    measurement.validation_finished();
    if let Err(error) = fixture.teardown(projection) {
        return Err(reject(&mut fixture, error));
    }
    let originals = count(&fixture.probes.originals);
    let clones = count(&fixture.probes.clones);
    let dropped = count(&fixture.probes.dropped);
    let e = config.b * config.i;
    let saves = config.i * if config.b == 1 { 1 } else { 2 };
    if originals != e + 2 || clones != e + saves + 1 || dropped != 2 * e + saves + 3 {
        return Err("document final payload counts invalid");
    }
    let physical = count(&fixture.probes.physical_started);
    let delivery = count(&fixture.probes.waiter_started);
    let saved_bytes = fixture.controller.saved_bytes;
    let loaded_bytes = fixture.controller.loaded_bytes;
    let write_sum = fixture.controller.write_sum;
    drop(fixture);
    let allocations = measurement
        .finish()?
        .map_or_else(|| "null".to_owned(), |r| r.json());
    let feature_profile = if cfg!(feature = "deterministic") {
        "persistence-deterministic-enabled"
    } else {
        "persistence-production"
    };
    Ok(format!(
        "{{\"schema\":\"rca-document-performance-profile-v1\",\"feature_profile\":\"{feature_profile}\",\"features\":{{\"persistence\":true,\"deterministic\":{},\"allocations\":{}}},\"workload\":\"{}\",\"size\":{},\"iterations\":{},\"payload_bytes\":{},\"drive_contract\":\"ready-bounded-v1\",\"bounds\":{{\"scheduler_task_polls\":{},\"outer_drive_polls\":{},\"native_deadline_seconds\":5}},\"counters\":{{\"cold\":{},\"work\":{}}},\"final_tag\":{},\"final_revision\":{},\"acknowledged_revision\":{},\"checksum\":{},\"originals\":{originals},\"construction_originals\":{},\"work_entering_originals\":{},\"clones\":{clones},\"payload_dropped\":{dropped},\"payload_live\":0,\"write_tag_sum\":{write_sum},\"saved_bytes\":{saved_bytes},\"loaded_bytes\":{loaded_bytes},\"physical_created\":{physical},\"physical_dropped\":{physical},\"delivery_created\":{delivery},\"delivery_dropped\":{delivery},\"physical_joined\":true,\"pending_physical\":0,\"pending_delivery\":0,\"state_extracted\":true,\"owners_released\":true,\"host_task_dropped\":true,\"host_stopped\":true,\"allocations\":{allocations}}}",
        cfg!(feature = "deterministic"),
        cfg!(feature = "allocations"),
        config.name,
        config.b,
        config.i,
        config.p,
        config.scheduler,
        config.outer,
        cold.json(),
        work.json(),
        projection.tag,
        projection.revision,
        projection.ack.expect("validated acknowledgement"),
        projection.checksum,
        e + 1,
        e
    ))
}
