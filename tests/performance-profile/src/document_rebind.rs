//! Fixed two-order keyed document replacement with independently joined old work.
//!
//! Both schedules retire the old key, retain its last value and load a replacement.
//! One releases the old backend first; the other releases the producer first.
//! Closing must wait for both, without changing the replacement or its sibling.
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::future::{Future, ready};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Waker};
use std::time::Duration;

use rca_core::{
    Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, ReduceContext, Reducer,
    ReducerExt, SharedKeyCache, SharedKeyIdentity, SharedLoadContext, SharedReader,
    SharedReaderKey, Store,
};
use rca_persistence::RevisionBackend;
use rca_persistence::{
    Backend, BackendSession, BackendSnapshot, Codec, CompareSave, DocumentKeyError,
    DocumentSubscription, SharedDocument, SharedDocumentAction, SharedDocumentOwner,
    SharedDocumentSession, SharedDocumentState, SharedProducer, SharedUpdate,
};
use rca_test::{Completion, controlled_future};

use crate::host::Host;
use crate::measurement::Measurement;

#[cfg(test)]
#[path = "document_rebind_controls.rs"]
pub(crate) mod controls;

// Fixed two-row schedules; independent scheduler and outer-drive bounds, no sleeps.
const SCHEDULER_BUDGET: usize = 8192;
const OUTER_BUDGET: usize = 8193;
const DEADLINE: Duration = Duration::from_secs(5);
const NAME: &str = "document-keyed-rebind-deterministic";

#[derive(Clone, Copy, Debug)]
pub(crate) struct Config;

impl Config {
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        if args.next().as_deref() != Some(NAME)
            || args.next().as_deref() != Some("2")
            || args.next().as_deref() != Some("1")
            || args.next().is_some()
        {
            return Err("document rebind requires deterministic mode, SIZE=2 and ITERATIONS=1");
        }
        Ok(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Control {
    None,
    #[cfg(test)]
    WrongFreshValue,
    #[cfg(test)]
    ChangeSibling,
    #[cfg(test)]
    ReleaseBoth,
    #[cfg(test)]
    RetainBackend,
}

#[derive(Debug, Eq, PartialEq)]
struct Failure {
    /// The first failed workload or teardown check.
    original: &'static str,
    /// A later cleanup failure, without replacing the original error.
    cleanup: Option<&'static str>,
}

#[derive(Debug)]
struct StorageError(&'static str);
impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl Error for StorageError {}

struct Binary;
impl Codec<u32> for Binary {
    type Error = StorageError;
    fn encode(&self, value: &u32) -> Result<Vec<u8>, StorageError> {
        Ok(value.to_le_bytes().to_vec())
    }
    fn decode(&self, bytes: &[u8]) -> Result<u32, StorageError> {
        bytes
            .try_into()
            .map(u32::from_le_bytes)
            .map_err(|_| StorageError("invalid u32 bytes"))
    }
}

#[derive(Default)]
struct Probes {
    loads: AtomicUsize,
    saves: AtomicUsize,
    plain_loads: AtomicUsize,
    released: AtomicUsize,
    deliveries: AtomicUsize,
    delivery_drops: AtomicUsize,
    joins: AtomicUsize,
    join_drops: AtomicUsize,
    cancellations: AtomicUsize,
    seed_valid: AtomicUsize,
    hit_seed_calls: AtomicUsize,
}
fn count(value: &AtomicUsize) -> usize {
    value.load(Ordering::Relaxed)
}
/// Counts delivery release even when retiring the key cancels its future.
struct Delivery(Arc<Probes>);
impl Drop for Delivery {
    fn drop(&mut self) {
        self.0.delivery_drops.fetch_add(1, Ordering::Relaxed);
    }
}
/// Tracks the producer join independently of the backend's pending load.
struct ProducerDebt(Arc<Probes>);
impl Drop for ProducerDebt {
    fn drop(&mut self) {
        self.0.join_drops.fetch_add(1, Ordering::Relaxed);
    }
}
type Load = Completion<Result<BackendSnapshot<u64>, StorageError>>;
struct MemoryState {
    closed: bool,
    loads: VecDeque<Load>,
    load_calls: usize,
}
#[derive(Clone)]
/// Retains physical load completions until the chosen release order permits them.
struct Memory {
    state: Arc<Mutex<MemoryState>>,
    changed: async_channel::Sender<()>,
    notifications: async_channel::Receiver<()>,
    probes: Arc<Probes>,
}
impl Memory {
    fn new(probes: &Arc<Probes>) -> Self {
        let (changed, notifications) = async_channel::bounded(1);
        Self {
            state: Arc::new(Mutex::new(MemoryState {
                closed: false,
                loads: VecDeque::with_capacity(1),
                load_calls: 0,
            })),
            changed,
            notifications,
            probes: Arc::clone(probes),
        }
    }
    /// Releases one physical load even if its retired delivery rejects the result.
    fn complete(&self, revision: u64, value: u32) -> Result<(), &'static str> {
        let completion = self
            .state
            .lock()
            .unwrap()
            .loads
            .pop_front()
            .ok_or("rebind physical load absent")?;
        // The old delivery may be cancelled. Physical completion still removes real debt.
        let _ = completion.complete(Ok(BackendSnapshot {
            revision,
            bytes: Some(value.to_le_bytes().to_vec()),
        }));
        self.probes.released.fetch_add(1, Ordering::Relaxed);
        let _ = self.changed.try_send(());
        Ok(())
    }
    /// Releases all remaining physical debt during rejection cleanup.
    fn release_all(&self) {
        let pending = std::mem::take(&mut self.state.lock().unwrap().loads);
        for completion in pending {
            let _ = completion.complete(Err(StorageError("cleanup")));
            self.probes.released.fetch_add(1, Ordering::Relaxed);
        }
        let _ = self.changed.try_send(());
    }
}
impl Backend for Memory {
    type Error = StorageError;
    fn load(&self) -> impl Future<Output = Result<Option<Vec<u8>>, StorageError>> {
        self.probes.plain_loads.fetch_add(1, Ordering::Relaxed);
        ready(Err(StorageError("plain load outside rebind workload")))
    }
    fn save(&self, _: Vec<u8>) -> impl Future<Output = Result<(), StorageError>> {
        self.probes.saves.fetch_add(1, Ordering::Relaxed);
        ready(Err(StorageError("save outside rebind workload")))
    }
}
impl RevisionBackend for Memory {
    type Revision = u64;
    async fn load_revisioned(&self) -> Result<BackendSnapshot<u64>, StorageError> {
        let (completion, future) = controlled_future();
        {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return Err(StorageError("closed backend"));
            }
            state.load_calls += 1;
            state.loads.push_back(completion);
        }
        self.probes.loads.fetch_add(1, Ordering::Relaxed);
        self.probes.deliveries.fetch_add(1, Ordering::Relaxed);
        let delivery = Delivery(Arc::clone(&self.probes));
        let result = future
            .await
            .map_err(|_| StorageError("load delivery ended"))?;
        drop(delivery);
        result
    }
    fn compare_and_save(
        &self,
        _: u64,
        _: Vec<u8>,
    ) -> impl Future<Output = Result<CompareSave<u64>, StorageError>> {
        self.probes.saves.fetch_add(1, Ordering::Relaxed);
        ready(Err(StorageError("CAS save outside rebind workload")))
    }
}
impl BackendSession for Memory {
    async fn close(&self) -> Result<(), StorageError> {
        loop {
            {
                let mut state = self.state.lock().unwrap();
                state.closed = true;
                if state.loads.is_empty() {
                    return Ok(());
                }
            }
            self.notifications
                .recv()
                .await
                .map_err(|_| StorageError("close channel ended"))?;
        }
    }
}

type Rows = KeyedState<u8, SharedDocumentState<u32, u64>>;
type RowAction = KeyedAction<u8, SharedDocumentAction<u32, u64>>;
type RowsPath = KeyedStatePath<
    Rows,
    u8,
    SharedDocumentState<u32, u64>,
    fn(&Rows) -> &Rows,
    fn(&mut Rows) -> &mut Rows,
>;
type RowPath = KeyedActionPath<
    Action,
    u8,
    SharedDocumentAction<u32, u64>,
    fn(&Action) -> Option<&RowAction>,
    fn(RowAction) -> Action,
>;
enum Action {
    Row(RowAction),
    Recreate,
}
struct Parent {
    replacement: Option<SharedDocumentState<u32, u64>>,
}
impl Reducer for Parent {
    type State = Rows;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Rows,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if matches!(action, Action::Recreate) {
            state.remove(&1);
            state.insert(1, self.replacement.take().expect("one replacement"));
        }
        Effect::none()
    }
}
fn rows_path() -> RowsPath {
    KeyedStatePath::new(|state: &Rows| state, |state| state)
}
fn row_path() -> RowPath {
    KeyedActionPath::new(
        |action: &Action| match action {
            Action::Row(row) => Some(row),
            Action::Recreate => None,
        },
        Action::Row,
    )
}
fn check(condition: bool, message: &'static str) -> Result<(), &'static str> {
    if condition { Ok(()) } else { Err(message) }
}
/// Drives admitted deterministic work to quiescence without advancing time.
fn settle(host: &Host) -> Result<(), &'static str> {
    match host {
        Host::Deterministic(runtime) => runtime
            .run_until_stalled()
            .map(|_| ())
            .map_err(|_| "rebind scheduler budget exhausted"),
        Host::Native(_) => Err("rebind requires deterministic host"),
    }
}
/// Polls once without driving the host, to check an unfinished causal boundary.
fn pending<F: Future + ?Sized>(
    future: std::pin::Pin<&mut F>,
    message: &'static str,
) -> Result<(), &'static str> {
    check(
        future
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending(),
        message,
    )
}
/// Completes the producer debt exactly once, independently of the backend load.
fn release_join(
    join: &mut Option<Completion<Result<(), StorageError>>>,
) -> Result<(), &'static str> {
    join.take()
        .ok_or("rebind producer completion absent")?
        .complete(Ok(()))
        .map_err(|_| "rebind producer join dropped before release")
}

/// Keeps old, sibling and replacement resources alive until both debts are joined.
struct Scenario {
    store: Option<Store<Rows, Action>>,
    sessions: [Option<SharedDocumentSession>; 3],
    backends: Option<[Memory; 3]>,
    backend_owners: [Weak<Mutex<MemoryState>>; 3],
    cache: Option<SharedKeyCache>,
    owners: [Option<SharedDocumentOwner<u32, u64>>; 3],
    /// Current binding, retained old handle, sibling and fresh cache-hit handle.
    readers: [Option<SharedReader<u32>>; 4],
    subscription: Option<DocumentSubscription>,
    endpoint: Option<SharedProducer<(), StorageError>>,
    join: Option<Completion<Result<(), StorageError>>>,
    identity: SharedKeyIdentity,
    probes: Arc<Probes>,
    #[cfg(test)]
    control: Control,
    #[cfg(test)]
    retained: Option<Memory>,
}
impl Scenario {
    fn new(runtime: rca_core::Runtime, control: Control) -> Self {
        #[cfg(not(test))]
        let _ = control;
        let probes = Arc::new(Probes::default());
        let backends = std::array::from_fn(|_| Memory::new(&probes));
        let backend_owners = backends
            .each_ref()
            .map(|backend| Arc::downgrade(&backend.state));
        let (old, interpreter) = SharedDocument::revisioned(7, backends[0].clone(), Binary);
        let (sibling, sibling_resource) =
            SharedDocument::revisioned(20, backends[1].clone(), Binary);
        let (replacement, fresh_resource) =
            SharedDocument::revisioned(40, backends[2].clone(), Binary);
        let sessions = [
            Some(interpreter.session()),
            Some(sibling_resource.session()),
            Some(fresh_resource.session()),
        ];
        let store = Store::new(
            [(1, old), (2, sibling)].into_iter().collect(),
            Parent {
                replacement: Some(replacement),
            }
            .for_each(rows_path(), row_path(), interpreter),
            runtime,
        );
        Self {
            store: Some(store),
            sessions,
            backends: Some(backends),
            backend_owners,
            cache: Some(SharedKeyCache::new()),
            owners: std::array::from_fn(|_| None),
            readers: std::array::from_fn(|_| None),
            subscription: None,
            endpoint: None,
            join: None,
            identity: SharedKeyIdentity::new::<Binary>("chain02-row1", "binary"),
            probes,
            #[cfg(test)]
            control,
            #[cfg(test)]
            retained: None,
        }
    }
    async fn owner(
        &self,
        id: u8,
        session: usize,
    ) -> Result<SharedDocumentOwner<u32, u64>, &'static str> {
        Ok(SharedDocumentOwner::new(
            self.store
                .as_ref()
                .ok_or("rebind store absent")?
                .scope_keyed(rows_path(), row_path(), id)
                .await
                .map_err(|_| "rebind scope failed")?,
            |value: &u32| *value,
            self.sessions[session]
                .as_ref()
                .ok_or("rebind session absent")?
                .clone(),
        ))
    }
    /// Loads the old row and sibling before the measured replacement schedules.
    async fn cold(&mut self, host: &Host) -> Result<(), &'static str> {
        self.owners[0] = Some(self.owner(1, 0).await?);
        self.owners[1] = Some(self.owner(2, 1).await?);
        let cache = self.cache.as_ref().unwrap();
        let backends = self.backends.as_ref().unwrap();
        let mut initial = Box::pin(cache.reader(
            self.owners[0].as_ref().unwrap().key(self.identity.clone()),
            || 0,
        ));
        pending(initial.as_mut(), "rebind initial load did not suspend")?;
        settle(host)?;
        backends[0].complete(1, 7)?;
        let current = initial
            .await
            .map_err(|_| "rebind initial cache load failed")?;
        settle(host)?;
        self.readers[1] = Some(current.clone());
        self.readers[0] = Some(current);
        let mut sibling = Box::pin(
            cache.reader(
                self.owners[1]
                    .as_ref()
                    .unwrap()
                    .key(SharedKeyIdentity::new::<Binary>("chain02-row2", "binary")),
                || 0,
            ),
        );
        pending(sibling.as_mut(), "rebind sibling cold load did not suspend")?;
        settle(host)?;
        backends[1].complete(1, 20)?;
        self.readers[2] = Some(
            sibling
                .await
                .map_err(|_| "rebind sibling cache load failed")?,
        );
        settle(host)?;
        check(
            count(&self.probes.loads) == 2 && count(&self.probes.saves) == 0,
            "rebind cold counts invalid",
        )
    }
    /// Checks fresh, retained-old and sibling views through separate live handles.
    fn values(&self, sibling: u32) -> Result<(), &'static str> {
        let read = |i: usize| {
            self.readers[i]
                .as_ref()
                .ok_or("rebind reader absent")?
                .read(|value| *value)
                .map_err(|_| "rebind reader access failed")
        };
        check(
            read(0)? == 42 && read(3)? == 42,
            "rebind fresh value invalid",
        )?;
        check(read(1)? == 7, "rebind retained old value changed")?;
        check(
            read(2)? == sibling,
            "rebind sibling changed without its own action",
        )
    }
    async fn root(&self, sibling: u32, revision: u64) -> Result<(), &'static str> {
        let actual = self
            .store
            .as_ref()
            .ok_or("rebind store absent")?
            .read(|state| {
                (
                    *state.get(&1).unwrap().value(),
                    state.get(&1).unwrap().backend_revision().copied(),
                    *state.get(&2).unwrap().value(),
                    state.get(&2).unwrap().backend_revision().copied(),
                )
            })
            .await
            .map_err(|_| "rebind root read failed")?;
        check(
            actual == (42, Some(2), sibling, Some(revision)),
            "rebind root snapshot invalid",
        )
    }
    #[expect(
        clippy::too_many_lines,
        reason = "keeps both causal physical-release schedules and their authority checks together"
    )]
    async fn work(&mut self, host: &Host, producer_first: bool) -> Result<(), &'static str> {
        let (completion, future) = controlled_future();
        self.join = Some(completion);
        self.probes.joins.fetch_add(1, Ordering::Relaxed);
        let debt = ProducerDebt(Arc::clone(&self.probes));
        let cancelling = Arc::clone(&self.probes);
        let mut endpoint = None;
        self.subscription = Some(
            self.owners[0]
                .as_ref()
                .unwrap()
                .observe_changes::<StorageError, _, _, _>(|producer| {
                    endpoint = Some(producer);
                    (
                        SharedUpdate::Present(()),
                        move || {
                            cancelling.cancellations.fetch_add(1, Ordering::Relaxed);
                        },
                        async move {
                            let result = future
                                .await
                                .map_err(|_| StorageError("producer delivery ended"))?;
                            drop(debt);
                            result
                        },
                    )
                })
                .map_err(|_| "rebind producer registration failed")?,
        );
        self.endpoint = endpoint;
        settle(host)?;
        check(
            self.subscription.as_ref().unwrap().status().is_active(),
            "rebind producer not active",
        )?;
        check(
            self.backends.as_ref().unwrap()[0]
                .state
                .lock()
                .unwrap()
                .loads
                .len()
                == 1,
            "rebind producer initial load absent",
        )?;
        let old_key = self.owners[0].as_ref().unwrap().key(self.identity.clone());
        let _ = self
            .store
            .as_ref()
            .unwrap()
            .send(Action::Recreate)
            .await
            .map_err(|_| "rebind replacement failed")?;
        settle(host)?;
        check(
            old_key
                .load(SharedLoadContext::UserInitiated)
                .await
                .is_err_and(|error| error.is_ended()),
            "rebind retired key retained authority",
        )?;
        check(
            self.subscription
                .as_ref()
                .unwrap()
                .status()
                .error()
                .is_some_and(DocumentKeyError::is_ended),
            "rebind retired subscription not ended",
        )?;
        check(
            matches!(
                self.endpoint
                    .as_ref()
                    .unwrap()
                    .publish(SharedUpdate::Present(())),
                Err(SharedUpdate::Present(()))
            ),
            "rebind retired producer published",
        )?;
        check(
            count(&self.probes.cancellations) == 1,
            "rebind cancellation count invalid",
        )?;
        self.owners[2] = Some(self.owner(1, 2).await?);
        let cache = self.cache.as_ref().unwrap();
        cache.reset().map_err(|_| "rebind cache reset failed")?;
        let seeded = Arc::clone(&self.probes);
        let mut replacing = Box::pin(cache.load_reader(
            self.readers[0].as_mut().unwrap(),
            self.owners[2].as_ref().unwrap().key(self.identity.clone()),
            move |value| {
                seeded
                    .seed_valid
                    .store(usize::from(*value == 7), Ordering::Relaxed);
                40
            },
        ));
        pending(
            replacing.as_mut(),
            "rebind replacement load did not suspend",
        )?;
        settle(host)?;
        let fresh_value = 42;
        #[cfg(test)]
        let fresh_value = if self.control == Control::WrongFreshValue {
            43
        } else {
            fresh_value
        };
        self.backends.as_ref().unwrap()[2].complete(2, fresh_value)?;
        replacing.await.map_err(|_| "rebind load_reader failed")?;
        settle(host)?;
        check(
            count(&self.probes.seed_valid) == 1,
            "rebind seed lost original reference",
        )?;
        let hit_called = Arc::clone(&self.probes);
        self.readers[3] = Some(
            cache
                .reader(
                    self.owners[2].as_ref().unwrap().key(self.identity.clone()),
                    move || {
                        hit_called.hit_seed_calls.fetch_add(1, Ordering::Relaxed);
                        0
                    },
                )
                .await
                .map_err(|_| "rebind fresh cache hit failed")?,
        );
        check(
            count(&self.probes.hit_seed_calls) == 0
                && self.backends.as_ref().unwrap()[2]
                    .state
                    .lock()
                    .unwrap()
                    .load_calls
                    == 1,
            "rebind fresh cache hit admitted provider",
        )?;
        #[cfg(test)]
        if self.control == Control::ChangeSibling {
            let _ = self
                .store
                .as_ref()
                .unwrap()
                .send(Action::Row(KeyedAction::new(
                    2,
                    SharedDocumentAction::reload(),
                )))
                .await
                .map_err(|_| "rebind sibling control send failed")?;
            settle(host)?;
            self.backends.as_ref().unwrap()[1].complete(2, 21)?;
            settle(host)?;
        }
        self.values(20)?;

        // Neither the cancelled delivery nor either independent release alone
        // proves that the retired resource has finished closing.
        let mut closing = Box::pin(self.owners[0].as_ref().unwrap().close());
        pending(
            closing.as_mut(),
            "rebind close finished before either release",
        )?;
        settle(host)?;
        pending(
            closing.as_mut(),
            "rebind close finished before either release",
        )?;
        if producer_first {
            release_join(&mut self.join)?;
        } else {
            self.backends.as_ref().unwrap()[0].complete(99, 99)?;
        }
        #[cfg(test)]
        if self.control == Control::ReleaseBoth {
            if producer_first {
                self.backends.as_ref().unwrap()[0].complete(99, 99)?;
            } else {
                release_join(&mut self.join)?;
            }
        }
        settle(host)?;
        pending(
            closing.as_mut(),
            "rebind close finished before independent release",
        )?;
        self.values(20)?;
        self.root(20, 1).await?;
        let _ = self
            .store
            .as_ref()
            .unwrap()
            .send(Action::Row(KeyedAction::new(
                2,
                SharedDocumentAction::reload(),
            )))
            .await
            .map_err(|_| "rebind sibling reload failed")?;
        settle(host)?;
        self.backends.as_ref().unwrap()[1].complete(3, 23)?;
        settle(host)?;
        self.values(23)?;
        if producer_first {
            self.backends.as_ref().unwrap()[0].complete(99, 99)?;
        } else {
            release_join(&mut self.join)?;
        }
        closing.await.map_err(|_| "rebind retired close failed")?;
        self.values(23)?;
        self.root(23, 3).await
    }
    async fn validate(&self) -> Result<(), &'static str> {
        self.values(23)?;
        self.root(23, 3).await?;
        check(
            count(&self.probes.loads) == 5
                && count(&self.probes.saves) == 0
                && count(&self.probes.plain_loads) == 0
                && count(&self.probes.cancellations) == 1
                && count(&self.probes.deliveries) == 5
                && count(&self.probes.joins) == 1,
            "rebind operation counts invalid",
        )?;
        for (backend, expected) in self.backends.as_ref().unwrap().iter().zip([2, 2, 1]) {
            let state = backend.state.lock().unwrap();
            check(
                state.load_calls == expected && state.loads.is_empty(),
                "rebind per-resource counts invalid",
            )?;
        }
        Ok(())
    }
    /// Releases both debts, closes every session and rejects retained backend owners.
    async fn cleanup(&mut self, host: &Host) -> Result<(), &'static str> {
        if let Some(subscription) = &self.subscription {
            subscription.cancel();
        }
        if self.join.is_some() {
            release_join(&mut self.join)?;
        }
        if let Some(backends) = &self.backends {
            for backend in backends {
                backend.release_all();
            }
        }
        settle(host)?;
        for owner in self.owners.iter().flatten() {
            owner
                .close()
                .await
                .map_err(|_| "rebind owner cleanup failed")?;
        }
        // A rejected preparation may not have attached every explicit session yet.
        for session in self.sessions.iter().flatten() {
            session
                .close()
                .await
                .map_err(|_| "rebind session cleanup failed")?;
        }
        if let Some(backends) = &self.backends {
            for backend in backends {
                let state = backend.state.lock().unwrap();
                check(
                    state.closed && state.loads.is_empty(),
                    "rebind physical cleanup incomplete",
                )?;
            }
        }
        self.endpoint = None;
        self.subscription = None;
        self.owners = std::array::from_fn(|_| None);
        self.readers = std::array::from_fn(|_| None);
        self.cache = None;
        self.sessions = std::array::from_fn(|_| None);
        if let Some(store) = self.store.take() {
            drop(
                store
                    .shutdown_into_state()
                    .await
                    .map_err(|_| "rebind shutdown failed")?,
            );
        }
        self.backends = None;
        settle(host)?;
        check(
            count(&self.probes.loads) == count(&self.probes.released)
                && count(&self.probes.deliveries) == count(&self.probes.delivery_drops)
                && count(&self.probes.joins) == count(&self.probes.join_drops),
            "rebind physical or delivery owner retained",
        )?;
        check(
            self.backend_owners
                .iter()
                .all(|owner| owner.strong_count() == 0),
            "rebind backend owner retained",
        )
    }
}

/// Adds bounded host drives and a final host-owner check to one release schedule.
struct Fixture {
    scenario: Scenario,
    host: Option<Host>,
    host_owner: Weak<()>,
}
impl Fixture {
    fn new(control: Control) -> Result<Self, &'static str> {
        let (host, runtime, host_owner) = Host::new(false, SCHEDULER_BUDGET)?;
        Ok(Self {
            scenario: Scenario::new(runtime, control),
            host: Some(host),
            host_owner,
        })
    }
    fn cold(&mut self) -> Result<(), &'static str> {
        let host = self.host.as_ref().unwrap();
        host.run_ready_bounded(self.scenario.cold(host), DEADLINE, OUTER_BUDGET)?
    }
    fn work(&mut self, producer_first: bool) -> Result<(), &'static str> {
        let host = self.host.as_ref().unwrap();
        host.run_ready_bounded(
            self.scenario.work(host, producer_first),
            DEADLINE,
            OUTER_BUDGET,
        )?
    }
    fn validate(&self) -> Result<(), &'static str> {
        self.host.as_ref().unwrap().run_ready_bounded(
            self.scenario.validate(),
            DEADLINE,
            OUTER_BUDGET,
        )?
    }
    fn cleanup(&mut self) -> Result<(), &'static str> {
        if let Some(host) = &self.host {
            host.run_ready_bounded(self.scenario.cleanup(host), DEADLINE, OUTER_BUDGET)??;
        }
        self.host = None;
        check(
            self.host_owner.strong_count() == 0,
            "rebind host task retained",
        )
    }
}

/// Cleans both schedules and retains the original error plus the first cleanup error.
fn reject(fixtures: &mut [Fixture; 2], original: &'static str) -> Failure {
    let mut cleanup = None;
    for fixture in fixtures {
        #[cfg(test)]
        {
            fixture.scenario.retained = None;
        }
        if let Err(error) = fixture.cleanup() {
            cleanup.get_or_insert(error);
        }
    }
    Failure { original, cleanup }
}

#[inline(never)]
fn profile_document_rebind_work(fixtures: &mut [Fixture; 2]) -> Result<(), &'static str> {
    for (fixture, producer_first) in fixtures.iter_mut().zip([false, true]) {
        fixture.work(producer_first)?;
    }
    Ok(())
}

/// Measures both schedules with cold setup, work, validation and cleanup separated.
fn run_inner(_: Config, controls: [Control; 2]) -> Result<String, Failure> {
    let mut measurement = Measurement::new(None);
    let make = |error| Failure {
        original: error,
        cleanup: None,
    };
    let mut fixtures = [
        Fixture::new(controls[0]).map_err(make)?,
        Fixture::new(controls[1]).map_err(make)?,
    ];
    measurement.construction_finished();

    // Initial cache misses belong to cold work, not replacement work.
    for index in 0..2 {
        if let Err(error) = fixtures[index].cold() {
            return Err(reject(&mut fixtures, error));
        }
    }
    measurement.cold_finished();

    if let Err(error) = profile_document_rebind_work(&mut fixtures) {
        return Err(reject(&mut fixtures, error));
    }
    measurement.work_finished();

    for index in 0..2 {
        if let Err(error) = fixtures[index].validate() {
            return Err(reject(&mut fixtures, error));
        }
    }
    measurement.validation_finished();

    #[cfg(test)]
    for fixture in &mut fixtures {
        if fixture.scenario.control == Control::RetainBackend {
            fixture.scenario.retained =
                Some(fixture.scenario.backends.as_ref().unwrap()[0].clone());
        }
    }
    for index in 0..2 {
        if let Err(error) = fixtures[index].cleanup() {
            return Err(reject(&mut fixtures, error));
        }
    }
    drop(fixtures);
    let allocations = measurement
        .finish()
        .map_err(make)?
        .map_or_else(|| "null".to_owned(), |record| record.json());
    Ok(format!(
        "{{\"schema\":\"rca-document-rebind-performance-profile-v1\",\"workload\":\"{NAME}\",\"size\":2,\"iterations\":1,\"schedules\":2,\"features\":{{\"persistence\":true,\"deterministic\":true,\"allocations\":{}}},\"drive_contract\":\"ready-bounded-v1\",\"bounds\":{{\"scheduler_task_polls\":{SCHEDULER_BUDGET},\"outer_drive_polls\":{OUTER_BUDGET}}},\"cold_loads\":4,\"work_loads\":6,\"loads\":10,\"saves\":0,\"cancellations\":2,\"fresh_value\":42,\"fresh_revision\":2,\"sibling_value\":23,\"sibling_revision\":3,\"retained_old_value\":7,\"physical_loads_created\":10,\"physical_loads_released\":10,\"deliveries_created\":10,\"deliveries_dropped\":10,\"producer_joins_created\":2,\"producer_joins_dropped\":2,\"pending_physical\":0,\"owners_released\":true,\"host_task_dropped\":true,\"allocations\":{allocations}}}",
        cfg!(feature = "allocations")
    ))
}

pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    run_inner(config, [Control::None; 2]).map_err(|failure| {
        if let Some(cleanup) = failure.cleanup {
            eprintln!(
                "document rebind rejected: {}; cleanup rejected: {cleanup}",
                failure.original
            );
        }
        failure.original
    })
}
