//! One ordinary save across a dependency, shared cache and keyed document owner.
//!
//! A changed edit schedules one delayed save; an equal edit and an unrelated
//! action schedule none. Local publication, physical persistence and the final
//! acknowledgement are checked separately while the sibling remains unchanged.
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Waker};
use std::time::Duration;

use rca_core::{
    DependencyKey, Effect, Issue, IssueSeverity, KeyedAction, KeyedActionPath, KeyedState,
    KeyedStatePath, ReduceContext, Reducer, ReducerExt, SharedAccessError, SharedKey,
    SharedKeyCache, SharedKeyIdentity, SharedReader, Store, on_change,
};
use rca_persistence::{
    Backend, BackendSession, Codec, DocumentKey, SharedDocument, SharedDocumentAction,
    SharedDocumentOwner, SharedDocumentSession, SharedDocumentState,
};
use rca_test::{Completion, controlled_future};

use crate::host::Host;
use crate::measurement::Measurement;

#[cfg(test)]
#[path = "joint_document_controls.rs"]
mod controls;

const SCHEDULER_BUDGET: usize = 8192;
const OUTER_BUDGET: usize = 8193;
const DEADLINE: Duration = Duration::from_secs(5);
const DELAY: Duration = Duration::from_millis(10);
const NAME: &str = "joint-document-deterministic";

pub(crate) struct Config;
impl Config {
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        if args.next().as_deref() != Some(NAME)
            || args.next().as_deref() != Some("2")
            || args.next().as_deref() != Some("1")
            || args.next().is_some()
        {
            return Err("joint document requires deterministic mode, SIZE=2 and ITERATIONS=1");
        }
        Ok(Self)
    }
}

#[derive(Default)]
struct Probes {
    loads: AtomicUsize,
    saves: AtomicUsize,
    writes: AtomicUsize,
    lookups: AtomicUsize,
    hits: AtomicUsize,
    hit_seeds: AtomicUsize,
    edits: AtomicUsize,
    equal: AtomicUsize,
    unrelated: AtomicUsize,
    reactions: AtomicUsize,
    timers: AtomicUsize,
    saved: AtomicUsize,
    issues: AtomicUsize,
    created: AtomicUsize,
    completed: AtomicUsize,
    dropped: AtomicUsize,
    closes: AtomicUsize,
    selectors: AtomicUsize,
    parent_reductions: AtomicUsize,
    child_reductions: AtomicUsize,
    document_reductions: AtomicUsize,
}
fn count(value: &AtomicUsize) -> usize {
    value.load(Ordering::Relaxed)
}
fn bump(value: &AtomicUsize) {
    value.fetch_add(1, Ordering::Relaxed);
}
fn check(condition: bool, message: &'static str) -> Result<(), &'static str> {
    if condition { Ok(()) } else { Err(message) }
}
/// Drives the document actor to quiescence without advancing its clock.
fn settle(host: &Host) -> Result<(), &'static str> {
    match host {
        Host::Deterministic(runtime) => runtime
            .run_until_stalled()
            .map(|_| ())
            .map_err(|_| "joint scheduler budget exhausted"),
        Host::Native(_) => Err("joint document requires deterministic host"),
    }
}

/// Rejects queued values and closure after allowing an actor-owned take to finish.
fn check_observation_quiet(
    host: &Host,
    observation: &mut rca_core::Observation<[u32; 2]>,
) -> Result<(), &'static str> {
    let mut next = std::pin::pin!(observation.next());
    let mut context = Context::from_waker(Waker::noop());
    check(
        next.as_mut().poll(&mut context).is_pending(),
        "joint unexpected observation or closure",
    )?;
    // A queued value can first return Pending while its actor Take is in flight.
    settle(host)?;
    check(
        next.as_mut().poll(&mut context).is_pending(),
        "joint unexpected observation or closure",
    )
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
/// Counts release of an admitted backend delivery, including cancellation.
struct Delivery(Arc<Probes>);
impl Drop for Delivery {
    fn drop(&mut self) {
        bump(&self.0.dropped);
    }
}
struct MemoryState {
    value: u32,
    closed: bool,
    /// The next physical value and its independently held delivery completion.
    pending: Option<(u32, Completion<Result<(), StorageError>>)>,
}
#[derive(Clone)]
struct Memory {
    state: Arc<Mutex<MemoryState>>,
    probes: Arc<Probes>,
}
impl Memory {
    fn new(value: u32, probes: &Arc<Probes>) -> Self {
        Self {
            state: Arc::new(Mutex::new(MemoryState {
                value,
                closed: false,
                pending: None,
            })),
            probes: Arc::clone(probes),
        }
    }
    /// Commits the physical value before waking its potentially cancelled receiver.
    fn complete_save(&self) -> Result<(), &'static str> {
        let completion = {
            let mut state = self.state.lock().unwrap();
            let (value, completion) = state.pending.take().ok_or("joint physical save absent")?;
            state.value = value;
            completion
        };
        bump(&self.probes.writes);
        bump(&self.probes.completed);
        // A cancelled delivery cannot erase physical completion during cleanup.
        let _ = completion.complete(Ok(()));
        Ok(())
    }
}
impl Backend for Memory {
    type Error = StorageError;
    #[expect(
        clippy::unused_async_trait_impl,
        reason = "backend admission and delivery side effects occur when polled, not when created"
    )]
    async fn load(&self) -> Result<Option<Vec<u8>>, StorageError> {
        let state = self.state.lock().unwrap();
        if state.closed {
            return Err(StorageError("closed backend"));
        }
        bump(&self.probes.loads);
        bump(&self.probes.created);
        let _delivery = Delivery(Arc::clone(&self.probes));
        bump(&self.probes.completed);
        Ok(Some(state.value.to_le_bytes().to_vec()))
    }
    async fn save(&self, bytes: Vec<u8>) -> Result<(), StorageError> {
        let value = Binary.decode(&bytes)?;
        let (completion, future) = controlled_future();
        {
            let mut state = self.state.lock().unwrap();
            if state.closed || state.pending.is_some() {
                return Err(StorageError("invalid save admission"));
            }
            state.pending = Some((value, completion));
        }
        bump(&self.probes.saves);
        bump(&self.probes.created);
        let _delivery = Delivery(Arc::clone(&self.probes));
        future
            .await
            .map_err(|_| StorageError("save delivery ended"))?
    }
}
impl BackendSession for Memory {
    #[expect(
        clippy::unused_async_trait_impl,
        reason = "the backend closes when its future is polled, not when created"
    )]
    async fn close(&self) -> Result<(), StorageError> {
        let mut state = self.state.lock().unwrap();
        if !state.closed {
            bump(&self.probes.closes);
        }
        state.closed = true;
        // This ordinary-only fixture releases physical debt before closing.
        // An error explicitly does not claim quiescence.
        if state.pending.is_some() {
            Err(StorageError("physical save still pending"))
        } else {
            Ok(())
        }
    }
}

type DocumentRows = KeyedState<u8, SharedDocumentState<u32>>;
type DocumentAction = KeyedAction<u8, SharedDocumentAction<u32>>;
type DocumentPath = KeyedStatePath<
    DocumentRows,
    u8,
    SharedDocumentState<u32>,
    fn(&DocumentRows) -> &DocumentRows,
    fn(&mut DocumentRows) -> &mut DocumentRows,
>;
type DocumentActions = KeyedActionPath<
    DocumentAction,
    u8,
    SharedDocumentAction<u32>,
    fn(&DocumentAction) -> Option<&DocumentAction>,
    fn(DocumentAction) -> DocumentAction,
>;
fn document_path() -> DocumentPath {
    KeyedStatePath::new(|s| s, |s| s)
}
fn document_actions() -> DocumentActions {
    KeyedActionPath::new(|action| Some(action), |a| a)
}
struct DocumentParent(Arc<Probes>);
impl Reducer for DocumentParent {
    type State = DocumentRows;
    type Action = DocumentAction;
    fn reduce(
        &mut self,
        _: &mut DocumentRows,
        _: &DocumentAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<DocumentAction> {
        bump(&self.0.document_reductions);
        Effect::none()
    }
}
#[derive(Clone)]
/// Supplies the editor with the shared cache and primary/sibling document keys.
struct Client {
    cache: SharedKeyCache,
    keys: [DocumentKey<u32>; 2],
}
struct DocumentsKey;
impl DependencyKey for DocumentsKey {
    type Value = Client;
}
/// Owns the separate document actor and the resources retained by its editor client.
struct Documents {
    store: Option<Store<DocumentRows, DocumentAction>>,
    sessions: Vec<SharedDocumentSession>,
    owners: Vec<SharedDocumentOwner<u32>>,
    cache: Option<SharedKeyCache>,
    backends: Vec<Memory>,
    backend_owners: [Weak<Mutex<MemoryState>>; 2],
}
impl Documents {
    fn new(runtime: rca_core::Runtime, probes: Arc<Probes>) -> Self {
        let backends = vec![Memory::new(7, &probes), Memory::new(20, &probes)];
        let backend_owners = std::array::from_fn(|i| Arc::downgrade(&backends[i].state));
        let (primary, interpreter) = SharedDocument::new(7, backends[0].clone(), Binary);
        let (sibling, resource) = SharedDocument::new(20, backends[1].clone(), Binary);
        let sessions = vec![interpreter.session(), resource.session()];
        let store = Store::new(
            [(1, primary), (2, sibling)].into_iter().collect(),
            DocumentParent(probes).for_each(document_path(), document_actions(), interpreter),
            runtime,
        );
        Self {
            store: Some(store),
            sessions,
            owners: Vec::new(),
            cache: Some(SharedKeyCache::new()),
            backends,
            backend_owners,
        }
    }
    /// Acquires both child routes before creating the editor's dependency value.
    async fn client(&mut self) -> Result<Client, &'static str> {
        for (slot, id) in [1, 2].into_iter().enumerate() {
            let route = self
                .store
                .as_ref()
                .unwrap()
                .scope_keyed(document_path(), document_actions(), id)
                .await
                .map_err(|_| "joint scope failed")?;
            self.owners.push(SharedDocumentOwner::new(
                route,
                |v: &u32| *v,
                self.sessions[slot].clone(),
            ));
        }
        Ok(Client {
            cache: self.cache.as_ref().unwrap().clone(),
            keys: std::array::from_fn(|i| {
                self.owners[i].key(SharedKeyIdentity::new::<Binary>(
                    format!("joint-row-{i}"),
                    "binary",
                ))
            }),
        })
    }
    async fn snapshot(&self) -> Result<[(u32, u64, Option<u64>); 2], &'static str> {
        self.store
            .as_ref()
            .unwrap()
            .read(|state| {
                [1, 2].map(|id| {
                    let row = state.get(&id).unwrap();
                    (*row.value(), row.revision(), row.acknowledged_revision())
                })
            })
            .await
            .map_err(|_| "joint document read failed")
    }
    /// Completes pending writes, closes sessions, then shuts down the document actor.
    async fn cleanup(&mut self) -> Result<(), &'static str> {
        for backend in &self.backends {
            if backend.state.lock().unwrap().pending.is_some() {
                backend.complete_save()?;
            }
        }
        for owner in &self.owners {
            owner
                .close()
                .await
                .map_err(|_| "joint owner close failed")?;
        }
        for session in &self.sessions {
            session
                .close()
                .await
                .map_err(|_| "joint session close failed")?;
        }
        self.owners.clear();
        self.cache = None;
        self.sessions.clear();
        if let Some(store) = self.store.take() {
            drop(
                store
                    .shutdown_into_state()
                    .await
                    .map_err(|_| "joint document shutdown failed")?,
            );
        }
        self.backends.clear();
        Ok(())
    }
    /// Checks backend ownership independently of empty pending-work queues.
    fn owners_released(&self) -> bool {
        self.backend_owners
            .iter()
            .all(|owner| owner.strong_count() == 0)
    }
}

struct ChildState {
    id: u8,
    draft: u32,
    revision: u64,
    saved: Option<u64>,
    reader: Option<SharedReader<u32>>,
}
type EditorState = KeyedState<u8, ChildState>;
enum ChildAction {
    Open,
    Loaded(SharedReader<u32>),
    Edit(u32),
    Flush,
    Saved,
}
enum Action {
    Row(KeyedAction<u8, ChildAction>),
    Unrelated,
}
fn row(id: u8, action: ChildAction) -> Action {
    Action::Row(KeyedAction::new(id, action))
}
fn editor_state() -> EditorState {
    [(1, 7), (2, 20)]
        .into_iter()
        .map(|(id, draft)| {
            (
                id,
                ChildState {
                    id,
                    draft,
                    revision: 0,
                    saved: None,
                    reader: None,
                },
            )
        })
        .collect()
}
/// Copies each editor's identity, draft, revision and acknowledged revision.
fn private_snapshot(state: &EditorState) -> Vec<(u8, u32, u64, Option<u64>)> {
    [1, 2]
        .map(|id| {
            let row = state.get(&id).unwrap();
            (id, row.draft, row.revision, row.saved)
        })
        .to_vec()
}
/// Reads owned values, not cloned reader handles that could hide later changes.
fn shared_snapshot(state: &EditorState) -> Result<Vec<Option<u32>>, SharedAccessError> {
    [1, 2]
        .iter()
        .map(|id| {
            state
                .get(id)
                .unwrap()
                .reader
                .as_ref()
                .map(|r| r.read(|v| *v))
                .transpose()
        })
        .collect()
}
struct Editor(Arc<Probes>);
impl Reducer for Editor {
    type State = EditorState;
    type Action = Action;
    fn reduce(
        &mut self,
        _: &mut EditorState,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        bump(&self.0.parent_reductions);
        if matches!(action, Action::Unrelated) {
            bump(&self.0.unrelated);
        }
        Effect::none()
    }
}
struct Child(Arc<Probes>);
impl Reducer for Child {
    type State = ChildState;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut ChildState,
        action: &ChildAction,
        context: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        bump(&self.0.child_reductions);
        match action {
            ChildAction::Open => {
                bump(&self.0.lookups);
                let client = context.dependency::<DocumentsKey>().clone();
                let key = client.keys[usize::from(state.id - 1)].clone();
                return Effect::future(async move {
                    Some(ChildAction::Loaded(
                        client
                            .cache
                            .reader(key, || 0)
                            .await
                            .expect("joint cache load"),
                    ))
                });
            }
            ChildAction::Loaded(reader) => state.reader = Some(reader.clone()),
            ChildAction::Edit(value) if *value != state.draft => {
                state.draft = *value;
                state.revision += 1;
                bump(&self.0.edits);
            }
            ChildAction::Edit(_) => bump(&self.0.equal),
            ChildAction::Flush => {
                bump(&self.0.timers);
                bump(&self.0.lookups);
                let key =
                    context.dependency::<DocumentsKey>().keys[usize::from(state.id - 1)].clone();
                let value = state.draft;
                return Effect::future(async move {
                    key.save(&value).await.expect("joint key save");
                    Some(ChildAction::Saved)
                });
            }
            ChildAction::Saved => {
                state.saved = Some(state.revision);
                bump(&self.0.saved);
                context
                    .report_issue(
                        Issue::new("joint.document.saved", "primary revision 1 persisted")
                            .with_severity(IssueSeverity::Note),
                    )
                    .expect("joint issue accepted");
                bump(&self.0.issues);
            }
        }
        Effect::none()
    }
}
struct SaveAfterEdit(Arc<Probes>);
impl Reducer for SaveAfterEdit {
    type State = ChildState;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        _: &mut ChildState,
        _: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        bump(&self.0.reactions);
        Effect::send_after(DELAY, ChildAction::Flush)
    }
}
fn editor_reducer(probes: Arc<Probes>) -> impl Reducer<State = EditorState, Action = Action> {
    let reactions = Arc::clone(&probes);
    Editor(Arc::clone(&probes)).for_each(
        KeyedStatePath::new(|s: &EditorState| s, |s| s),
        KeyedActionPath::new(
            |a: &Action| match a {
                Action::Row(row) => Some(row),
                Action::Unrelated => None,
            },
            Action::Row,
        ),
        on_change(
            Child(probes),
            |state: &ChildState| state.revision,
            move |_, _| SaveAfterEdit(Arc::clone(&reactions)),
        ),
    )
}

#[inline(never)]
/// Separates local publication, physical save completion and causal acknowledgement.
async fn profile_joint_document_work(
    host: &Host,
    documents: &Documents,
    editor: &Store<EditorState, Action>,
    observation: &mut rca_core::Observation<[u32; 2]>,
    probes: &Probes,
) -> Result<[u32; 2], &'static str> {
    let editing = editor
        .send(row(1, ChildAction::Edit(8)))
        .await
        .map_err(|_| "joint edit failed")?;
    editor
        .send(row(1, ChildAction::Edit(8)))
        .await
        .map_err(|_| "joint equal edit failed")?
        .finish()
        .await;
    editor
        .send(Action::Unrelated)
        .await
        .map_err(|_| "joint unrelated action failed")?
        .finish()
        .await;
    settle(host)?;
    check(
        count(&probes.reactions) == 1 && count(&probes.saves) == 0,
        "joint on-change or pre-deadline counts invalid",
    )?;
    let Host::Deterministic(runtime) = host else {
        return Err("joint deterministic runtime absent");
    };
    runtime
        .advance(DELAY)
        .map_err(|_| "joint timer drive failed")?;
    settle(host)?;
    check(
        documents.snapshot().await? == [(8, 1, Some(0)), (20, 0, Some(0))],
        "joint pre-ack document snapshot invalid",
    )?;
    let shared = editor
        .read(shared_snapshot)
        .await
        .map_err(|_| "joint pre-ack cache read failed")?
        .map_err(|_| "joint pre-ack cache access failed")?;
    check(
        shared == [Some(8), Some(20)]
            && documents.backends[0].state.lock().unwrap().value == 7
            && !editing.is_finished(),
        "joint acknowledged before physical save",
    )?;
    let observed = observation
        .next()
        .await
        .ok_or("joint changed observation absent")?;
    check(
        observed == [8, 20],
        "joint observation changed sibling or omitted edit",
    )?;
    documents.backends[0].complete_save()?;
    settle(host)?;
    editing.finish().await;
    check(editing.is_finished(), "joint causal work unfinished")?;
    check_observation_quiet(host, observation)?;
    Ok(observed)
}

#[expect(
    clippy::too_many_lines,
    reason = "one fixed fixture keeps measurement boundaries, exact oracles and unconditional cleanup together"
)]
pub(crate) fn run(_: Config) -> Result<String, &'static str> {
    let mut measurement = Measurement::new(None);
    let (host, runtime, host_owner) = Host::new(false, SCHEDULER_BUDGET)?;
    let probes = Arc::new(Probes::default());
    let mut documents = Documents::new(runtime.clone(), Arc::clone(&probes));
    let mut editor = None;
    let mut observation = None;
    let issues = Arc::new(Mutex::new(Vec::new()));
    let mut observed = [[0; 2]; 2];
    measurement.construction_finished();

    // Cache misses, editor construction and initial observation are cold work.
    let result = (|| {
        host.run_ready_bounded(
            async {
                let client = documents.client().await?;
                let received = Arc::clone(&issues);
                editor = Some(
                    Store::builder(
                        editor_state(),
                        editor_reducer(Arc::clone(&probes)),
                        runtime.clone(),
                    )
                    .with_dependency::<DocumentsKey>(client.clone())
                    .with_issue_handler(move |issue| received.lock().unwrap().push(issue.clone()))
                    .build()
                    .map_err(|_| "joint editor construction failed")?,
                );
                let store = editor.as_ref().unwrap();
                for id in [1, 2] {
                    store
                        .send(row(id, ChildAction::Open))
                        .await
                        .map_err(|_| "joint open failed")?
                        .finish()
                        .await;
                }
                let hit = client
                    .cache
                    .reader(client.keys[0].clone(), || {
                        bump(&probes.hit_seeds);
                        99
                    })
                    .await
                    .map_err(|_| "joint cache hit failed")?;
                check(
                    hit.read(|value| *value)
                        .map_err(|_| "joint hit access failed")?
                        == 7,
                    "joint cache hit value invalid",
                )?;
                bump(&probes.hits);
                let selected = Arc::clone(&probes);
                observation = Some(
                    documents
                        .store
                        .as_ref()
                        .unwrap()
                        .observe_distinct(move |state| {
                            bump(&selected.selectors);
                            [1, 2].map(|id| *state.get(&id).unwrap().value())
                        })
                        .await
                        .map_err(|_| "joint observation registration failed")?,
                );
                observed[0] = observation
                    .as_mut()
                    .unwrap()
                    .next()
                    .await
                    .ok_or("joint initial observation absent")?;
                check(
                    observed[0] == [7, 20]
                        && count(&probes.loads) == 2
                        && count(&probes.saves) == 0
                        // KeyLoad, Loaded and Subscribe for each cache miss.
                        && count(&probes.document_reductions) == 6,
                    "joint cold values or counts invalid",
                )
            },
            DEADLINE,
            OUTER_BUDGET,
        )??;
        measurement.cold_finished();

        observed[1] = host.run_ready_bounded(
            profile_joint_document_work(
                &host,
                &documents,
                editor.as_ref().unwrap(),
                observation.as_mut().unwrap(),
                &probes,
            ),
            DEADLINE,
            OUTER_BUDGET,
        )??;
        measurement.work_finished();

        host.run_ready_bounded(
            async {
                let store = editor.as_ref().unwrap();
                check(
                    store
                        .read(private_snapshot)
                        .await
                        .map_err(|_| "joint private validation read failed")?
                        == [(1, 8, 1, Some(1)), (2, 20, 0, None)],
                    "joint private outcome invalid",
                )?;
                check(
                    store
                        .read(shared_snapshot)
                        .await
                        .map_err(|_| "joint shared validation read failed")?
                        .map_err(|_| "joint shared validation access failed")?
                        == [Some(8), Some(20)],
                    "joint shared outcome invalid",
                )?;
                check(
                    documents.snapshot().await? == [(8, 1, Some(1)), (20, 0, Some(0))],
                    "joint acknowledged snapshot invalid",
                )?;
                for (backend, expected) in documents.backends.iter().zip([8, 20]) {
                    let state = backend.state.lock().unwrap();
                    check(
                        state.value == expected && state.pending.is_none(),
                        "joint backend outcome invalid",
                    )?;
                }
                let received = issues.lock().unwrap();
                check(
                    received.len() == 1
                        && received[0].code == "joint.document.saved"
                        && received[0].message == "primary revision 1 persisted"
                        && received[0].severity == IssueSeverity::Note,
                    "joint issue outcome invalid",
                )?;
                check(
                    count(&probes.parent_reductions) == 9
                        && count(&probes.child_reductions) == 8
                        // Cold six, followed by KeySave and Saved.
                        && count(&probes.document_reductions) == 8
                        && count(&probes.selectors) == 3,
                    "joint reducer or selector work invalid",
                )?;
                check(
                    count(&probes.loads) == 2
                        && count(&probes.saves) == 1
                        && count(&probes.writes) == 1
                        && count(&probes.lookups) == 3
                        && count(&probes.hits) == 1
                        && count(&probes.hit_seeds) == 0
                        && count(&probes.edits) == 1
                        && count(&probes.equal) == 1
                        && count(&probes.unrelated) == 1
                        && count(&probes.reactions) == 1
                        && count(&probes.timers) == 1
                        && count(&probes.saved) == 1
                        && count(&probes.issues) == 1,
                    "joint logical work counts invalid",
                )
            },
            DEADLINE,
            OUTER_BUDGET,
        )??;
        measurement.validation_finished();
        Ok(())
    })();

    // Cleanup is separately bounded and runs even after a rejected work oracle.
    observation = None;
    let cleanup = host
        .run_ready_bounded(
            async {
                for backend in &documents.backends {
                    if backend.state.lock().unwrap().pending.is_some() {
                        backend.complete_save()?;
                    }
                }
                if let Some(store) = editor.take() {
                    drop(
                        store
                            .shutdown_into_state()
                            .await
                            .map_err(|_| "joint editor shutdown failed")?,
                    );
                }
                documents.cleanup().await
            },
            DEADLINE,
            OUTER_BUDGET,
        )
        .and_then(|result| result);
    drop(observation);
    if let Err(original) = result {
        if let Err(error) = cleanup {
            eprintln!("joint cleanup after {original}: {error}");
        }
        return Err(original);
    }
    cleanup?;
    check(documents.owners_released(), "joint backend owner retained")?;
    check(
        count(&probes.created) == 3
            && count(&probes.completed) == 3
            && count(&probes.dropped) == 3
            && count(&probes.closes) == 2
            // Each document owner closes through one final Abort action.
            && count(&probes.document_reductions) == 10,
        "joint physical teardown counts invalid",
    )?;
    let counts = [
        count(&probes.loads),
        count(&probes.saves),
        count(&probes.writes),
        count(&probes.lookups),
        count(&probes.hits),
        count(&probes.hit_seeds),
        count(&probes.edits),
        count(&probes.equal),
        count(&probes.unrelated),
        count(&probes.reactions),
        count(&probes.timers),
        count(&probes.saved),
        count(&probes.issues),
        count(&probes.created),
        count(&probes.completed),
        count(&probes.dropped),
        count(&probes.closes),
        count(&probes.selectors),
        count(&probes.parent_reductions),
        count(&probes.child_reductions),
        count(&probes.document_reductions),
    ];
    drop((documents, editor, runtime, probes, issues));
    drop(host);
    check(host_owner.strong_count() == 0, "joint host task retained")?;
    drop(host_owner);
    let allocations = measurement
        .finish()?
        .map_or_else(|| "null".to_owned(), |record| record.json());
    let names = [
        "loads",
        "saves",
        "writes",
        "dependency_lookups",
        "cache_hits",
        "cache_hit_seed_calls",
        "changed_edits",
        "equal_edits",
        "unrelated_actions",
        "on_change_reactions",
        "timer_actions",
        "save_success_actions",
        "issues",
        "physical_created",
        "physical_completed",
        "delivery_dropped",
        "backend_closes",
        "selector_calls",
        "parent_reductions",
        "child_reductions",
        "document_reductions",
    ];
    let counters = names
        .into_iter()
        .zip(counts)
        .map(|(name, value)| format!("\"{name}\":{value}"))
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!(
        "{{\"schema\":\"rca-joint-document-performance-profile-v1\",\"workload\":\"{NAME}\",\"size\":2,\"iterations\":1,\"private_values\":[8,20],\"shared_values\":[8,20],\"backend_values\":[8,20],\"observed\":{observed:?},{counters},\"pending_physical\":0,\"owners_released\":true,\"host_task_dropped\":true,\"causal_finished\":true,\"allocations\":{allocations}}}"
    ))
}
