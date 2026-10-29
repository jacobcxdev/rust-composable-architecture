//! Imports owned rows while keeping history separate from live resource authority.
//!
//! Run with `cargo run -p rca-example-composition-search --example owned_importer`.
//! The finite endpoint has one prepared response per generation. Replacements
//! use a fresh generation rather than restarting the same prepared request.
//! Snapshots own history, redact credential formatting and omit the live lease.
//! `docs/owned-importer.md` explains the separate shared-quota assertion.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::task::Poll;

use rca::{
    DependencyKey, Feature, FeatureContext, MountIdentity, OptionalMount, PredicateWaitOutcome,
    ReactionChange, Shared, Snapshot, Store, Task, TaskAccessError, TaskContext, TokioRuntime,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

/// Records committed progress without retaining the import future.
#[derive(Snapshot)]
struct Progress {
    /// The number of rows committed by the successful batch.
    rows: usize,
    /// Distinguishes a completed client result from a pending import.
    finished: bool,
}

/// Preserves client rejection separately from a dropped reply endpoint.
#[derive(Debug, Snapshot)]
enum ImportFailure {
    /// The endpoint returned a domain failure without spending quota.
    Rejected(
        /// The owned explanation retained in snapshot history.
        String,
    ),
    /// The response sender disappeared before supplying its prepared result.
    Abandoned,
}

/// Owns one mounted-session lease and the data used to build independent history.
#[derive(Snapshot)]
struct ImportSession {
    /// Selects a prepared response and distinguishes same-key replacement.
    generation: u64,
    /// Records the client's committed result, not task destruction.
    progress: Progress,
    /// Owns the initial marker and any rows moved from the client.
    history: Vec<String>,
    /// Counts committed client results, including rejection and abandonment.
    attempts: usize,
    /// Retains a typed failure instead of treating it as empty success.
    failure: Option<ImportFailure>,
    /// Remains in snapshot data but is hidden from snapshot Debug formatting.
    #[snapshot(redact)]
    credential: String,
    /// Shares the application's quota, asserted by a separate scalar capture.
    #[snapshot(omit = "Quota is independently asserted by the declared Quota capture")]
    quota: Shared<usize>,
    /// Keeps session capacity occupied until state replacement or removal.
    #[snapshot(omit = "The mounted-session lease is asserted by physical permit return")]
    _lease: OwnedSemaphorePermit,
}

/// Owns the optional session whose presence controls managed import work.
#[derive(Snapshot)]
struct App {
    /// Removal drops the lease and retires the mounted import task.
    session: Option<ImportSession>,
}

/// Selects a prepared fresh generation or removes the current session.
enum Action {
    /// Drops the current session before acquiring the successor's lease.
    Replace(
        /// A fresh generation with one prepared client response.
        u64,
    ),
    /// Retires the current session without installing a successor.
    Remove,
}

/// Owns the session capacity and shared quota, independently of imported history.
struct Root {
    /// The private capacity-one pool, never closed by this example.
    slots: Arc<Semaphore>,
    /// The shared budget retained independently of any session lifetime.
    quota: Shared<usize>,
}

impl Root {
    /// Creates one session slot and enough quota for the finite trace.
    fn new() -> Self {
        Self {
            // This private, never-closed pool admits one mounted session.
            slots: Arc::new(Semaphore::new(1)),
            quota: Shared::new(12),
        }
    }

    /// Acquires the lease only after the previous session releases it.
    ///
    /// # Panics
    ///
    /// Panics if another session retains the sole permit or the pool is closed.
    fn session(&self, generation: u64) -> ImportSession {
        ImportSession {
            generation,
            progress: Progress {
                rows: 0,
                finished: false,
            },
            history: vec!["queued".into()],
            attempts: 0,
            failure: None,
            credential: "demonstration credential".into(),
            quota: self.quota.clone(),
            _lease: Arc::clone(&self.slots)
                .try_acquire_owned()
                .expect("the old session must release this exclusive lease first"),
        }
    }
}

impl Feature for Root {
    type State = App;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut App,
        action: &Action,
        _: &mut FeatureContext<'_, App, Action>,
    ) {
        drop(state.session.take());
        if let Action::Replace(generation) = action {
            state.session = Some(self.session(*generation));
        }
    }
}

/// Supplies the mounted state type without synthetic progress actions.
struct Import;
impl Feature for Import {
    type State = ImportSession;
    type Action = ();

    fn reduce(
        &mut self,
        _: &mut ImportSession,
        (): &(),
        _: &mut FeatureContext<'_, ImportSession, ()>,
    ) {
    }
}

/// Requires each Store builder to inject its concrete prepared endpoint.
struct ImportClient;
impl DependencyKey for ImportClient {
    type Value = Client;
}

/// Starts one owned import when a new session incarnation is installed.
fn mount() -> OptionalMount<App, Import, ()> {
    OptionalMount::new(
        |app: &App| app.session.as_ref(),
        |app: &mut App| app.session.as_mut(),
        |app: &App| {
            app.session.as_ref().map(|session| MountIdentity {
                key: (),
                replacement: session.generation,
                configuration: 0,
            })
        },
        |_| Import,
    )
    .on_initial(
        |session| session.generation,
        |change, mut capture| {
            let (ReactionChange::Initial {
                current: generation,
            }
            | ReactionChange::Changed {
                current: generation,
                ..
            }) = change;
            let client = capture.dependency::<ImportClient>().clone();
            Task::run(move |context| async move {
                if let Err(error) = import(&context, client, generation).await {
                    eprintln!("import access stopped: {error}");
                }
            })
        },
    )
}

/// Moves the client result into the captured session through one ordered update.
///
/// # Errors
///
/// Returns a task-access failure from the ordered update.
/// Client failures instead become committed `ImportFailure` state.
///
/// # Panics
///
/// Panics if the prepared generation is missing or a client mutex is poisoned.
/// The private quota must remain unpoisoned and positive before a successful import.
async fn import(
    context: &TaskContext<ImportSession, ()>,
    client: Client,
    generation: u64,
) -> Result<(), TaskAccessError> {
    let result = client.import(generation).await;
    let _committed = context
        .update(move |session| {
            session.attempts += 1;
            session.progress.finished = true;
            match result {
                Ok(batch) => {
                    session.progress.rows = batch.rows.len();
                    session.history.extend(batch.rows);
                    // The finite endpoint prepares at most two batches against
                    // twelve units. Each successful batch spends one unit.
                    session
                        .quota
                        .try_update(
                            |quota| {
                                *quota -= 1;
                                Ok::<_, Infallible>(())
                            },
                            || {},
                        )
                        .expect("the private quota is never poisoned");
                }
                Err(error) => session.failure = Some(error),
            }
        })
        .await?;
    Ok(())
}

/// Owns imported rows until the session update accepts them.
#[derive(Debug)]
struct Batch {
    /// The owned rows moved into session history without a second copy.
    rows: Vec<String>,
}

/// Carries either owned rows or the endpoint's explicit domain failure.
type Wire = Result<Vec<String>, ImportFailure>;

/// Shares a finite endpoint whose receivers each belong to one generation.
///
/// Clones share this endpoint. `Client::new` creates independent responses.
#[derive(Clone)]
struct Client {
    /// Each call removes its receiver before awaiting the prepared result.
    responses: Arc<Mutex<BTreeMap<u64, Response>>>,
}

/// Retains one generation's reply receiver and its pending-poll evidence.
struct Response {
    /// Owns the one-shot result channel for this generation.
    receive: oneshot::Receiver<Wire>,
    /// Counts actual suspension polls, independently of elapsed time.
    pending: Arc<AtomicUsize>,
}

/// Supplies one result and observes whether its receiver actually suspended.
struct Reply {
    /// Transfers one owned result or returns it after receiver destruction.
    send: oneshot::Sender<Wire>,
    /// Shares the receiver's suspension count with the scenario driver.
    pending: Arc<AtomicUsize>,
}

impl Client {
    /// Prepares independent one-shot replies for the supplied generation identifiers.
    ///
    /// The finite callers supply distinct identifiers. A duplicate replaces its
    /// earlier prepared channel, so it does not create another usable generation.
    fn new(generations: impl IntoIterator<Item = u64>) -> (Self, BTreeMap<u64, Reply>) {
        let mut responses = BTreeMap::new();
        let mut replies = BTreeMap::new();
        for generation in generations {
            let (send, receive) = oneshot::channel();
            let pending = Arc::new(AtomicUsize::new(0));
            responses.insert(
                generation,
                Response {
                    receive,
                    pending: Arc::clone(&pending),
                },
            );
            replies.insert(generation, Reply { send, pending });
        }
        (
            Self {
                responses: Arc::new(Mutex::new(responses)),
            },
            replies,
        )
    }

    /// Consumes one prepared response without retaining the endpoint lock across await.
    ///
    /// # Errors
    ///
    /// Returns the client's rejection or `Abandoned` if the sender disappears.
    ///
    /// # Panics
    ///
    /// Panics if the generation has no unused response or the mutex is poisoned.
    async fn import(&self, generation: u64) -> Result<Batch, ImportFailure> {
        let mut response = self
            .responses
            .lock()
            .unwrap()
            .remove(&generation)
            .expect("each installed generation has one prepared client call");
        let result = poll_fn(|cx| match Pin::new(&mut response.receive).poll(cx) {
            Poll::Pending => {
                response.pending.fetch_add(1, Ordering::SeqCst);
                Poll::Pending
            }
            Poll::Ready(value) => Poll::Ready(value),
        })
        .await;
        result
            .unwrap_or(Err(ImportFailure::Abandoned))
            .map(|rows| Batch { rows })
    }
}

/// Runs rejection and replacement success, then checks physical session release.
///
/// # Panics
///
/// Panics if the prepared trace fails to build, admit, complete or release capacity.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let (client, mut replies) = Client::new([0, 1]);
    let root = Root::new();
    let slots = Arc::clone(&root.slots);
    let store = Store::feature_builder(
        App { session: None },
        root,
        TokioRuntime::current().unwrap(),
    )
    .with_dependency::<ImportClient>(client)
    .mount(mount())
    .build()
    .unwrap();
    store.ready().await.unwrap();
    // These are two explicit user requests, not an automatic retry policy.
    for (generation, result) in [
        (0, Err(ImportFailure::Rejected("invalid batch".into()))),
        (1, Ok(vec!["owned row".into()])),
    ] {
        let Reply { send, pending } = replies.remove(&generation).unwrap();
        send.send(result).unwrap();
        let _request = store.send(Action::Replace(generation)).await.unwrap();
        assert_eq!(
            store
                .wait_while(|app| {
                    app.session
                        .as_ref()
                        .is_some_and(|session| !session.progress.finished)
                })
                .await,
            PredicateWaitOutcome::Satisfied
        );
        let history = store.read(Snapshot::snapshot).await.unwrap();
        println!(
            "{history:?}; client Pending polls: {}",
            pending.load(Ordering::SeqCst)
        );
    }
    let _removed = store.send(Action::Remove).await.unwrap();
    store.shutdown().await.unwrap();
    assert_eq!(slots.available_permits(), 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_executor::block_on;
    use rca::{CaptureKey, SharedAccessError, TestStore};

    type History = <App as Snapshot>::Value;

    struct Quota;
    impl CaptureKey for Quota {
        type Value = usize;
        type Error = SharedAccessError;
    }

    struct Fixture {
        store: TestStore<App, Action, History>,
        quota: Shared<usize>,
        slots: Arc<Semaphore>,
        first: History,
    }

    fn fixture(client: Client) -> Fixture {
        let root = Root::new();
        let quota = root.quota.clone();
        let captured_quota = quota.clone();
        let slots = Arc::clone(&root.slots);
        let state = App {
            session: Some(root.session(0)),
        };
        let first = state.snapshot();
        let store = TestStore::projected_feature_builder(state, root, Snapshot::snapshot)
            .with_dependency::<ImportClient>(client)
            .mount(mount())
            .with_capture::<Quota>(move |_| captured_quota.read(Snapshot::snapshot))
            .unwrap()
            .build();
        Fixture {
            store,
            quota,
            slots,
            first,
        }
    }

    // Missing the application's post-await commit, capturing a live quota alias,
    // cloning the returned rows, or retaining the omitted lease breaks this trace.
    #[test]
    fn ready_and_pending_imports_keep_owned_history_and_declared_quota_independent() {
        for immediate in [true, false] {
            block_on(async {
                let (client, mut replies) = Client::new([0]);
                let Reply { send, pending } = replies.remove(&0).unwrap();
                let mut send = Some(send);
                let row = String::from("imported row");
                let address = row.as_ptr() as usize;
                let mut rows = Some(vec![row]);
                if immediate {
                    send.take().unwrap().send(Ok(rows.take().unwrap())).unwrap();
                }
                let Fixture {
                    mut store,
                    quota,
                    slots,
                    first,
                } = fixture(client);
                let initial = store.receive_structural(|_| {}).await.unwrap();
                if !immediate {
                    assert!(pending.load(Ordering::SeqCst) > 0);
                    send.take().unwrap().send(Ok(rows.take().unwrap())).unwrap();
                }
                let _committed = store
                    .receive_update_with_captures(
                        |expected| {
                            let session = expected.session.as_mut().unwrap();
                            session.progress.rows = 1;
                            session.progress.finished = true;
                            session.history.push("imported row".into());
                            session.attempts = 1;
                            assert_eq!(quota.read(|value| *value).unwrap(), 12);
                            assert_eq!(slots.available_permits(), 0);
                        },
                        |expected| expected.update::<Quota>(|quota: &mut usize| *quota = 11),
                    )
                    .await
                    .unwrap();
                initial.finish().await.unwrap();
                assert_eq!(quota.read(|value| *value).unwrap(), 11);
                assert_eq!(
                    slots.available_permits(),
                    0,
                    "success retains the session lease"
                );
                store
                    .assert_state(move |app| {
                        assert_eq!(
                            app.session.as_ref().unwrap().history[1].as_ptr() as usize,
                            address
                        );
                    })
                    .await
                    .unwrap();
                let historical = first.session.as_ref().unwrap();
                assert_eq!(historical.history, ["queued"]);
                assert_eq!((historical.progress.rows, historical.attempts), (0, 0));
                assert!(!format!("{first:?}").contains("demonstration credential"));
                let mut edited = first.clone();
                edited
                    .session
                    .as_mut()
                    .unwrap()
                    .history
                    .push("expected only".into());
                store
                    .assert_state(|app| {
                        assert_eq!(
                            app.session.as_ref().unwrap().history,
                            ["queued", "imported row"]
                        );
                    })
                    .await
                    .unwrap();
                let _removed = store
                    .send_with(Action::Remove, |expected| expected.session = None)
                    .await
                    .unwrap();
                store.finish().await.unwrap();
                assert_eq!(slots.available_permits(), 1);
                assert_eq!(historical.history, ["queued"]);
            });
        }
    }

    // Treating a rejected/abandoned client as an empty successful import would
    // incorrectly spend quota or omit the application's typed failure.
    #[test]
    fn client_failure_and_abandonment_commit_failure_without_spending_quota() {
        for abandoned in [false, true] {
            block_on(async {
                let (client, mut replies) = Client::new([0]);
                let reply = replies.remove(&0).unwrap();
                let Fixture {
                    mut store,
                    quota,
                    slots,
                    ..
                } = fixture(client);
                let initial = store.receive_structural(|_| {}).await.unwrap();
                assert!(reply.pending.load(Ordering::SeqCst) > 0);
                if abandoned {
                    drop(reply.send);
                } else {
                    reply
                        .send
                        .send(Err(ImportFailure::Rejected("invalid batch".into())))
                        .unwrap();
                }
                let _committed = store
                    .receive_update(move |expected| {
                        let session = expected.session.as_mut().unwrap();
                        session.progress.finished = true;
                        session.attempts = 1;
                        session.failure = Some(if abandoned {
                            ImportFailureSnapshot::Abandoned
                        } else {
                            ImportFailureSnapshot::Rejected("invalid batch".into())
                        });
                    })
                    .await
                    .unwrap();
                initial.finish().await.unwrap();
                assert_eq!(quota.read(|value| *value).unwrap(), 12);
                assert_eq!(slots.available_permits(), 0);
                store
                    .assert_state(|app| {
                        let session = app.session.as_ref().unwrap();
                        assert_eq!(session.history, ["queued"]);
                        assert_eq!(session.progress.rows, 0);
                    })
                    .await
                    .unwrap();
                let _removed = store
                    .send_with(Action::Remove, |expected| expected.session = None)
                    .await
                    .unwrap();
                store.finish().await.unwrap();
                assert_eq!(slots.available_permits(), 1);
            });
        }
    }

    // A retained old receiver, wrong incarnation, or history retaining the
    // state-owned lease would prevent the owned late return/fresh completion.
    #[test]
    fn same_key_replacement_retires_pending_import_and_releases_session_leases() {
        block_on(async {
            let (client, mut replies) = Client::new([0, 1]);
            let old = replies.remove(&0).unwrap();
            let fresh = replies.remove(&1).unwrap();
            let Fixture {
                mut store,
                quota,
                slots,
                first,
            } = fixture(client);
            let initial = store.receive_structural(|_| {}).await.unwrap();
            assert!(old.pending.load(Ordering::SeqCst) > 0);
            assert_eq!(slots.available_permits(), 0);
            let replacement = store
                .send_with(Action::Replace(1), |expected| {
                    expected.session.as_mut().unwrap().generation = 1;
                })
                .await
                .unwrap();
            initial.finish().await.unwrap();
            assert!(fresh.pending.load(Ordering::SeqCst) > 0);
            // Joining the old producer precedes this destruction proof. A permit
            // returned earlier by state Drop would not prove receiver destruction.
            let late = String::from("old row");
            let address = late.as_ptr();
            let Ok(returned) = old.send.send(Ok(vec![late])).unwrap_err() else {
                panic!("the original late success payload must be returned");
            };
            assert_eq!(returned[0].as_ptr(), address);
            assert_eq!(quota.read(|value| *value).unwrap(), 12);
            store
                .assert_state(|app| {
                    let session = app.session.as_ref().unwrap();
                    assert_eq!(session.generation, 1);
                    assert_eq!(session.history, ["queued"]);
                    assert_eq!(session.attempts, 0);
                })
                .await
                .unwrap();
            fresh.send.send(Ok(vec!["fresh row".into()])).unwrap();
            let _committed = store
                .receive_update_with_captures(
                    |expected| {
                        let session = expected.session.as_mut().unwrap();
                        session.progress.rows = 1;
                        session.progress.finished = true;
                        session.history.push("fresh row".into());
                        session.attempts = 1;
                    },
                    |expected| expected.set::<Quota>(11),
                )
                .await
                .unwrap();
            replacement.finish().await.unwrap();
            let _removed = store
                .send_with(Action::Remove, |expected| expected.session = None)
                .await
                .unwrap();
            store.finish().await.unwrap();
            assert_eq!(slots.available_permits(), 1);
            let historical = first.session.unwrap();
            assert_eq!(historical.history, ["queued"]);
            assert_eq!((historical.generation, historical.attempts), (0, 0));
        });
    }

    // Omitting quota from derived history cannot forgive the declared capture,
    // even when every private/history field has the correct expectation.
    #[test]
    fn omitted_or_wrong_quota_expectation_rejects_actual_client_completion() {
        for expected_quota in [None, Some(99)] {
            block_on(async {
                let (client, mut replies) = Client::new([0]);
                replies
                    .remove(&0)
                    .unwrap()
                    .send
                    .send(Ok(vec!["imported row".into()]))
                    .unwrap();
                let Fixture {
                    mut store, quota, ..
                } = fixture(client);
                let initial = store.receive_structural(|_| {}).await.unwrap();
                let error = store
                    .receive_update_with_captures(
                        |expected| {
                            let session = expected.session.as_mut().unwrap();
                            session.progress.rows = 1;
                            session.progress.finished = true;
                            session.history.push("imported row".into());
                            session.attempts = 1;
                        },
                        |expected| {
                            if let Some(value) = expected_quota {
                                expected.set::<Quota>(value)?;
                            }
                            Ok(())
                        },
                    )
                    .await
                    .unwrap_err();
                assert!(error.is_capture_mismatch());
                initial.finish().await.unwrap();
                assert_eq!(quota.read(|value| *value).unwrap(), 11);
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err()
                );
            });
        }
    }
}
