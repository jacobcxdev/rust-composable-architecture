//! Uploads owned data with captured clients and live, incarnation-checked policy.
//!
//! Run with `cargo run -p rca-example-composition-search --example uploader`.
//! The finite client below controls I/O readiness, never RCA's execution path.

use std::future::poll_fn;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Poll, Waker};

use rca::{
    DependencyKey, Feature, FeatureContext, MountIdentity, OptionalMount, Output, OutputDelivery,
    OutputFlow, PredicateWaitOutcome, Store, Task, TaskAccessError, TaskContext, TokioRuntime,
};

/// State for one mounted upload incarnation.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Upload {
    /// Business key, which remains stable across replacements in this example.
    id: u64,
    /// Replacement identity that distinguishes successive owners of the same key.
    incarnation: u64,
    /// Maximum byte count sampled by the task before it starts an upload.
    limit: usize,
    /// Fetched byte count from the last accepted progress update.
    fetched: usize,
    /// Uploaded byte count from the last accepted completion.
    uploaded: usize,
}

impl Upload {
    /// Creates an idle incarnation of the example's fixed business key.
    ///
    /// Key 7 is an arbitrary domain identity, not an RCA requirement. The initial
    /// two-byte limit makes truncation visible with the prepared five-byte payload.
    fn new(incarnation: u64) -> Self {
        Self {
            id: 7,
            incarnation,
            limit: 2,
            fetched: 0,
            uploaded: 0,
        }
    }
}

/// Owned completion data from the controlled upload endpoint.
///
/// The receipt deliberately lacks `Clone` so its delivery demonstrates ownership
/// transfer. Its prepared token is a sample identifier, not a credential.
#[derive(Debug)]
struct Receipt {
    /// Identifies the captured client that performed the upload.
    client: &'static str,
    /// Owns the prepared completion identifier without copying its allocation.
    token: String,
    /// Number of bytes recorded by the controlled endpoint.
    bytes: usize,
}

/// Root state that owns the optional upload and delivered receipts.
#[derive(Debug)]
struct State {
    /// Mounted child state, absent after removal.
    upload: Option<Upload>,
    /// Receipts consumed by the root output handler.
    receipts: Vec<Receipt>,
}

impl State {
    /// Starts the first upload incarnation with no delivered receipts.
    fn new() -> Self {
        Self {
            upload: Some(Upload::new(0)),
            receipts: Vec::new(),
        }
    }
}

/// External domain events, separate from progress updates and completion outputs.
enum Action {
    /// Changes the byte limit of the current upload, if present.
    Policy(
        /// New byte limit for subsequent policy reads.
        usize,
    ),
    /// Installs, replaces or removes the optional child state.
    SetUpload(
        /// Next child state, or `None` to retire the current owner.
        Option<Upload>,
    ),
}

/// Applies policy and lifecycle events without starting I/O in the reducer.
struct Root;
impl Feature for Root {
    type State = State;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut FeatureContext<'_, State, Action>,
    ) {
        match action {
            Action::Policy(limit) => {
                if let Some(upload) = &mut state.upload {
                    upload.limit = *limit;
                }
            }
            Action::SetUpload(upload) => state.upload.clone_from(upload),
        }
    }
}

/// Accepts owned completion commands for the mounted upload.
///
/// The automatic task uses progress updates and outputs instead. This command
/// path supports the example's admission and rejected-ownership checks.
struct Worker;
impl Feature for Worker {
    type State = Upload;
    type Action = Receipt;
    fn reduce(
        &mut self,
        state: &mut Upload,
        action: &Receipt,
        _: &mut FeatureContext<'_, Upload, Receipt>,
    ) {
        state.uploaded = action.bytes;
    }
}

/// Dependency key for the client captured when an upload task starts.
struct UploadClient;
impl DependencyKey for UploadClient {
    type Value = Client;
}

/// Mounts the optional upload under its business key and incarnation.
///
/// A new incarnation retires the previous owner's capabilities. The constant
/// configuration identity keeps byte-limit changes from replacing the worker.
fn mount() -> OptionalMount<State, Worker, u64> {
    OptionalMount::new(
        |state: &State| state.upload.as_ref(),
        |state: &mut State| state.upload.as_mut(),
        |state: &State| {
            state.upload.as_ref().map(|upload| MountIdentity {
                key: upload.id,
                replacement: upload.incarnation,
                configuration: 0,
            })
        },
        |_| Worker,
    )
}

/// Starts one upload task when the child incarnation is installed.
///
/// The factory captures the injected client once. The shared declaration slot
/// supplies the output bound before installation, not a queue of completions.
/// The task reports upload or output failures, then discards recovered receipts.
///
/// # Panics
///
/// The installation factory panics if the output is not bound before it runs.
fn automatic(output: Arc<OnceLock<Output<Receipt>>>) -> OptionalMount<State, Worker, u64> {
    mount().on_initial(
        |upload| upload.id,
        move |_, mut capture| {
            let client = capture.dependency::<UploadClient>().clone();
            let output = output
                .get()
                .expect("output bound before installation")
                .clone();
            Task::run(move |context| async move {
                match upload(&context, client).await {
                    Ok(receipt) => match context.output(&output, receipt).await {
                        Ok(OutputDelivery::Consumed { .. }) => {}
                        Ok(OutputDelivery::Unhandled { value, .. }) => {
                            eprintln!("upload receipt had no recipient: {value:?}");
                        }
                        Err(error) => eprintln!("upload receipt delivery stopped: {error}"),
                    },
                    Err(error) => eprintln!("upload stopped before completion: {error}"),
                }
            })
        },
    )
}

/// Uploads owned bytes with short serial progress updates and a live policy read.
///
/// The captured client remains fixed. A fresh read neither replaces the client
/// nor renews the context's authority. The read and external I/O are not atomic.
///
/// # Errors
///
/// Returns an access error if a progress update or policy read fails. A rejected
/// final update discards the receipt but does not undo the external request.
///
/// # Panics
///
/// Panics if a controlled client mutex is poisoned.
async fn upload(
    context: &TaskContext<Upload, Receipt>,
    client: Client,
) -> Result<Receipt, TaskAccessError> {
    let mut bytes = client.fetch.wait().await;
    let fetched = bytes.len();
    context.update(move |state| state.fetched = fetched).await?;
    // Policy is current at this read, not atomically checked with external I/O.
    let limit = context.read(|state| state.limit).await?;
    bytes.truncate(limit);
    let receipt = client.upload(bytes).await;
    let uploaded = receipt.bytes;
    // This example discards the receipt if progress is rejected. The external
    // request has already happened; rejection does not undo it.
    context
        .update(move |state| state.uploaded = uploaded)
        .await?;
    Ok(receipt)
}

/// Runs one prepared upload, removes its owner and prints the delivered receipt.
///
/// # Panics
///
/// Panics if runtime setup, Store construction or an expected lifecycle operation
/// fails. These failures stop this standalone example rather than trigger recovery.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let client = Client::new("captured-A");
    client.fetch.release(b"hello".to_vec());
    client.token.release("upload-7".into());
    let output = Arc::new(OnceLock::new());
    let (builder, site) =
        Store::feature_builder(State::new(), Root, TokioRuntime::current().unwrap())
            .with_dependency::<UploadClient>(client)
            .mount_with_site(automatic(Arc::clone(&output)));
    let (builder, receipt) = builder.output::<Receipt>(&site, |handlers| {
        handlers.at_root(|state, receipt, _| {
            state.receipts.push(receipt);
            OutputFlow::Consumed
        });
    });
    output.set(receipt).unwrap();
    let store = builder.build().unwrap();
    store.ready().await.unwrap();
    assert_eq!(
        store.wait_while(|state| state.receipts.is_empty()).await,
        PredicateWaitOutcome::Satisfied
    );
    // Policy and removal are ordinary domain events, not UI lifecycle hooks.
    let _policy = store.send(Action::Policy(3)).await.unwrap();
    let _removal = store.send(Action::SetUpload(None)).await.unwrap();
    // Do not restore automatically here: each new incarnation needs fresh I/O.
    let state = store.shutdown_into_state().await.unwrap();
    for receipt in state.receipts {
        println!(
            "{} uploaded {} bytes: {}",
            receipt.client, receipt.bytes, receipt.token
        );
    }
}

/// Controlled two-step endpoint for ready and pending I/O responses.
///
/// Clones share the endpoint. Fresh [`Client::new`] calls create independent
/// endpoints. Each gate serves one waiter and one prepared response, not concurrent
/// uploads. This client controls readiness without replacing RCA's execution path.
#[derive(Clone)]
struct Client {
    /// Identity copied into each receipt to identify the captured client.
    name: &'static str,
    /// Transfers the prepared fetched bytes to the task.
    fetch: Gate<Vec<u8>>,
    /// Delays completion after the endpoint records the uploaded bytes.
    token: Gate<String>,
    /// Owns recorded payloads so checks can observe requests after task retirement.
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Client {
    /// Creates independent response slots and an empty request history.
    fn new(name: &'static str) -> Self {
        Self {
            name,
            fetch: Gate::new(),
            token: Gate::new(),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Records owned bytes before it waits for the prepared completion identifier.
    ///
    /// Cancellation does not remove the recorded request. The returned receipt
    /// owns the identifier received from the token gate.
    ///
    /// # Panics
    ///
    /// Panics if a client mutex is poisoned.
    async fn upload(&self, bytes: Vec<u8>) -> Receipt {
        let count = bytes.len();
        self.requests.lock().unwrap().push(bytes);
        Receipt {
            client: self.name,
            token: self.token.wait().await,
            bytes: count,
        }
    }
}

/// Shared response slot for one active waiter in this controlled example.
///
/// Clones share the slot and waker. The example never starts competing waiters,
/// so this support type does not provide general channel semantics.
struct Gate<T>(
    /// Shared reply state, locked only for synchronous polling and bookkeeping.
    Arc<Mutex<Reply<T>>>,
);

/// Response ownership and observations for a controlled wait.
struct Reply<T> {
    /// Prepared response that the next ready poll takes by value.
    value: Option<T>,
    /// Latest waker registered by the single pending waiter.
    waker: Option<Waker>,
    /// Number of polls that observed no prepared response, not a waiter count.
    pending: usize,
    /// Number of started waits whose leases were dropped after completion or cancellation.
    dropped: usize,
}
impl<T> Clone for Gate<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}
impl<T> Gate<T> {
    /// Creates an empty response slot with no polling or destruction observations.
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Reply {
            value: None,
            waker: None,
            pending: 0,
            dropped: 0,
        })))
    }
    /// Prepares one response and wakes the waiter outside the mutex.
    ///
    /// # Panics
    ///
    /// Panics if an unconsumed response already occupies the slot or the mutex
    /// is poisoned. The example treats either case as a programming failure.
    fn release(&self, value: T) {
        let waker = {
            let mut reply = self.0.lock().unwrap();
            assert!(
                reply.value.replace(value).is_none(),
                "one prepared response"
            );
            reply.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
    /// Waits for the prepared response and transfers its ownership to the caller.
    ///
    /// The first poll creates a lease. Completion or cancellation drops that
    /// lease, records destruction and clears the registered waker.
    ///
    /// # Panics
    ///
    /// Panics if the reply mutex is poisoned.
    async fn wait(&self) -> T {
        let _lease = Lease(self.clone());
        poll_fn(|cx| {
            let mut reply = self.0.lock().unwrap();
            if let Some(value) = reply.value.take() {
                return Poll::Ready(value);
            }
            reply.pending += 1;
            reply.waker = Some(cx.waker().clone());
            Poll::Pending
        })
        .await
    }
}
/// Records destruction and clears the waker for one started wait.
struct Lease<T>(
    /// Keeps the shared reply state available until the wait ends.
    Gate<T>,
);
impl<T> Drop for Lease<T> {
    fn drop(&mut self) {
        let mut reply = self.0.0.lock().unwrap();
        reply.dropped += 1;
        reply.waker = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_executor::block_on;
    use rca::{DeterministicRuntime, TaskAccessReason, TaskProgress, TaskSendError, TestStore};
    use std::pin::pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Context;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Snapshot {
        upload: Option<Upload>,
        receipts: Vec<(&'static str, String, usize)>,
    }
    fn snapshot(state: &State) -> Snapshot {
        Snapshot {
            upload: state.upload.clone(),
            receipts: state
                .receipts
                .iter()
                .map(|receipt| (receipt.client, receipt.token.clone(), receipt.bytes))
                .collect(),
        }
    }
    fn fixture(client: Client) -> (TestStore<State, Action, Snapshot>, Output<Receipt>) {
        let output = Arc::new(OnceLock::new());
        let (builder, site) = TestStore::projected_feature_builder(State::new(), Root, snapshot)
            .with_dependency::<UploadClient>(client)
            .mount_with_site(automatic(Arc::clone(&output)));
        let (builder, receipt) = builder.output::<Receipt>(&site, |handlers| {
            handlers.at_root(|state, receipt, _| {
                state.receipts.push(receipt);
                OutputFlow::Consumed
            });
        });
        output.set(receipt.clone()).unwrap();
        (builder.build(), receipt)
    }

    #[test]
    fn separate_clients_do_not_share_responses_or_requests() {
        let first = Client::new("first");
        let second = Client::new("second");
        let mut waiting = pin!(second.upload(b"second body".to_vec()));
        assert!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        // A clone is a handle to the same endpoint, not a fresh test backend.
        first.clone().token.release("first receipt".into());
        let receipt = block_on(first.upload(b"first body".to_vec()));
        assert_eq!(receipt.token, "first receipt");
        assert!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        second.token.release("second receipt".into());
        assert_eq!(block_on(waiting).token, "second receipt");
        assert_eq!(*first.requests.lock().unwrap(), [b"first body".to_vec()]);
        assert_eq!(*second.requests.lock().unwrap(), [b"second body".to_vec()]);
    }

    // Catches captured policy, synthetic responses, cloned ownership and skipped
    // progress commits. Both readiness modes use the same client and task body.
    #[test]
    fn ready_and_pending_uploads_keep_the_client_but_read_live_policy() {
        for immediate in [true, false] {
            block_on(async {
                let client = Client::new("captured-A");
                let mut token = Some(String::from("original-upload-token"));
                let address = token.as_ref().unwrap().as_ptr() as usize;
                if immediate {
                    client.fetch.release(b"hello".to_vec());
                    client.token.release(token.take().unwrap());
                }
                let (mut store, output) = fixture(client.clone());
                let initial = store.receive_structural(|_| {}).await.unwrap();
                if !immediate {
                    assert!(client.fetch.0.lock().unwrap().pending > 0);
                    let _policy = store
                        .send_with(Action::Policy(4), |expected| {
                            expected.upload.as_mut().unwrap().limit = 4
                        })
                        .await
                        .unwrap();
                    client.fetch.release(b"hello".to_vec());
                }
                assert!(
                    store.finish().await.is_err(),
                    "unasserted work cannot finish"
                );
                assert!(
                    store
                        .receive(|_| panic!("update is not an action"))
                        .await
                        .is_err()
                );
                let _fetched = store
                    .receive_update(|expected| expected.upload.as_mut().unwrap().fetched = 5)
                    .await
                    .unwrap();
                if !immediate {
                    // Receiving a commit does not promise the task continuation
                    // has run. Drive ready work without acknowledging progress.
                    assert!(store.finish().await.is_err());
                    assert!(client.token.0.lock().unwrap().pending > 0);
                    client.token.release(token.take().unwrap());
                }
                let bytes = if immediate { 2 } else { 4 };
                let _uploaded = store
                    .receive_update(move |expected| {
                        expected.upload.as_mut().unwrap().uploaded = bytes
                    })
                    .await
                    .unwrap();
                assert!(
                    store.finish().await.is_err(),
                    "output remains independently owed"
                );
                let _delivered = store
                    .receive_output(
                        &output,
                        move |receipt| {
                            receipt.client == "captured-A"
                                && receipt.token.as_ptr() as usize == address
                                && receipt.bytes == bytes
                        },
                        move |expected| {
                            expected.receipts.push((
                                "captured-A",
                                "original-upload-token".into(),
                                bytes,
                            ))
                        },
                    )
                    .await
                    .unwrap();
                initial.finish().await.unwrap();
                store.finish().await.unwrap();
                assert_eq!(
                    *client.requests.lock().unwrap(),
                    [if immediate {
                        b"he".to_vec()
                    } else {
                        b"hell".to_vec()
                    }]
                );
                assert_eq!(client.fetch.0.lock().unwrap().dropped, 1);
                assert_eq!(client.token.0.lock().unwrap().dropped, 1);
            });
        }
    }

    // A mismatch cannot commit. A later cleanup match commits the original receipt
    // but still returns IncompleteAssertion and cannot erase the earlier failure.
    #[test]
    fn wrong_output_match_keeps_debt_after_owned_receipt_cleanup() {
        block_on(async {
            let client = Client::new("captured-A");
            client.fetch.release(b"hello".to_vec());
            let token = String::from("original-upload-token");
            let address = token.as_ptr() as usize;
            client.token.release(token);
            let (mut store, output) = fixture(client.clone());
            let initial = store.receive_structural(|_| {}).await.unwrap();
            let fetched = store
                .receive_update(|expected| expected.upload.as_mut().unwrap().fetched = 5)
                .await
                .unwrap();
            let uploaded = store
                .receive_update(|expected| expected.upload.as_mut().unwrap().uploaded = 2)
                .await
                .unwrap();
            let mismatch = store
                .receive_output(
                    &output,
                    move |receipt| {
                        assert_eq!(receipt.token.as_ptr() as usize, address);
                        false
                    },
                    |_| panic!("wrong receipt cannot commit"),
                )
                .await
                .unwrap_err();
            assert!(mismatch.is_action_mismatch());
            store
                .assert_state(|state| {
                    assert!(state.receipts.is_empty());
                    assert_eq!(state.upload.as_ref().unwrap().uploaded, 2);
                })
                .await
                .unwrap();
            let cleanup = store
                .receive_output(
                    &output,
                    move |receipt| {
                        receipt.client == "captured-A"
                            && receipt.token.as_ptr() as usize == address
                            && receipt.bytes == 2
                    },
                    |expected| {
                        expected
                            .receipts
                            .push(("captured-A", "original-upload-token".into(), 2));
                    },
                )
                .await
                .unwrap_err();
            assert!(cleanup.is_incomplete_assertion());
            initial.finish().await.unwrap();
            fetched.finish().await.unwrap();
            uploaded.finish().await.unwrap();
            store
                .assert_state(move |state| {
                    assert_eq!(state.receipts.len(), 1);
                    assert_eq!(state.receipts[0].token.as_ptr() as usize, address);
                })
                .await
                .unwrap();
            assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
            drop((initial, fetched, uploaded, output));
            let teardown = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
                .expect_err("Drop must retain the failed output match");
            assert!(
                teardown.downcast_ref::<String>().unwrap().starts_with(
                    "TestStore teardown failed: TestStore incomplete assertion for transaction boundary at "
                )
            );
            assert_eq!(*client.requests.lock().unwrap(), [b"he".to_vec()]);
            assert_eq!(client.fetch.0.lock().unwrap().dropped, 1);
            assert_eq!(client.token.0.lock().unwrap().dropped, 1);
        });
    }

    // Catches accidental authority renewal after await, selector/mutator entry,
    // lost rejected action ownership and cancellation mistaken for destruction.
    #[test]
    fn same_key_replacement_rejects_a_suspended_external_upload_and_old_commands() {
        block_on(async {
            let runtime = DeterministicRuntime::new();
            let (builder, site) = Store::feature_builder(State::new(), Root, runtime.runtime())
                .mailbox_capacity(1)
                .mount_with_site(mount());
            let store = builder.build().unwrap();
            runtime.run(store.ready()).await.unwrap().unwrap();
            let old = runtime
                .run(store.acquire(&site))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let client = Client::new("old-client");
            let mut continuation = pin!(upload(&old, client.clone()));
            assert!(
                continuation
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(client.fetch.0.lock().unwrap().pending, 1);
            let _removal = runtime
                .run(store.send(Action::SetUpload(None)))
                .await
                .unwrap()
                .unwrap();
            let _replacement = runtime
                .run(store.send(Action::SetUpload(Some(Upload::new(1)))))
                .await
                .unwrap()
                .unwrap();
            client.fetch.release(b"old bytes".to_vec());
            let error = runtime
                .run(continuation.as_mut())
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            assert_eq!(client.fetch.0.lock().unwrap().dropped, 1);
            assert!(client.requests.lock().unwrap().is_empty());
            let calls = Arc::new(AtomicUsize::new(0));
            let read_calls = Arc::clone(&calls);
            assert_eq!(
                runtime
                    .run(old.read(move |_| {
                        read_calls.fetch_add(1, Ordering::SeqCst);
                    }))
                    .await
                    .unwrap()
                    .unwrap_err()
                    .reason(),
                TaskAccessReason::ScopeEnded
            );
            let write_calls = Arc::clone(&calls);
            assert_eq!(
                runtime
                    .run(old.update(move |_| {
                        write_calls.fetch_add(1, Ordering::SeqCst);
                    }))
                    .await
                    .unwrap()
                    .unwrap_err()
                    .reason(),
                TaskAccessReason::ScopeEnded
            );
            let receipt = Receipt {
                client: "old-client",
                token: "owned rejection".into(),
                bytes: 99,
            };
            let address = receipt.token.as_ptr();
            let Err(TaskSendError::Rejected { action, error }) =
                runtime.run(old.send(receipt)).await.unwrap()
            else {
                panic!("old incarnation must return its action")
            };
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
            assert_eq!(action.token.as_ptr(), address);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            let fresh = runtime
                .run(store.acquire(&site))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(
                runtime
                    .run(fresh.read(|state| (
                        state.id,
                        state.incarnation,
                        state.fetched,
                        state.uploaded
                    )))
                    .await
                    .unwrap()
                    .unwrap(),
                (7, 1, 0, 0)
            );
            runtime.run(store.shutdown()).await.unwrap().unwrap();
        });
    }

    // Retirement fences the final commit, not the request already sent outside RCA.
    #[test]
    fn replacement_during_upload_keeps_the_request_but_rejects_old_progress() {
        block_on(async {
            let runtime = DeterministicRuntime::new();
            let (builder, site) = Store::feature_builder(State::new(), Root, runtime.runtime())
                .mount_with_site(mount());
            let store = builder.build().unwrap();
            runtime.run(store.ready()).await.unwrap().unwrap();
            let old = runtime
                .run(store.acquire(&site))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let client = Client::new("old-client");
            client.fetch.release(b"old bytes".to_vec());
            let mut continuation = pin!(upload(&old, client.clone()));
            // Admit fetched progress and the policy read separately. The caller
            // owns this future, so retirement cannot cancel it as managed work.
            for _ in 0..2 {
                assert!(
                    continuation
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                runtime.run_until_stalled().unwrap();
            }
            assert!(
                continuation
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(client.token.0.lock().unwrap().pending, 1);
            assert_eq!(*client.requests.lock().unwrap(), [b"ol".to_vec()]);
            let _replacement = runtime
                .run(store.send(Action::SetUpload(Some(Upload::new(1)))))
                .await
                .unwrap()
                .unwrap();
            client.token.release("late completion".into());
            let error = runtime
                .run(continuation.as_mut())
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            assert_eq!(client.fetch.0.lock().unwrap().dropped, 1);
            assert_eq!(client.token.0.lock().unwrap().dropped, 1);
            assert_eq!(*client.requests.lock().unwrap(), [b"ol".to_vec()]);
            let state = runtime
                .run(store.shutdown_into_state())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(state.upload, Some(Upload::new(1)));
            assert!(state.receipts.is_empty());
        });
    }

    // Catches a later expectation accidentally forgiving a wrong progress value.
    #[test]
    fn wrong_progress_expectation_keeps_its_debt() {
        let client = Client::new("captured-A");
        client.fetch.release(b"hello".to_vec());
        let (mut store, _) = fixture(client);
        let _initial = block_on(store.receive_structural(|_| {})).unwrap();
        let mismatch = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            block_on(store.receive_update(|expected| {
                expected.upload.as_mut().unwrap().fetched = 99;
            }))
        }));
        assert!(mismatch.is_err(), "incorrect progress must fail comparison");
        assert!(block_on(store.finish()).is_err());
        assert!(
            block_on(store.receive_update(|expected| {
                expected.upload.as_mut().unwrap().fetched = 5;
            }))
            .is_err(),
            "later correction cannot erase failed assertion"
        );
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err());
    }

    // Catches abandoning live client futures when the mounted owner retires.
    #[test]
    fn removing_a_managed_upload_destroys_its_pending_client_future() {
        block_on(async {
            let client = Client::new("captured-A");
            let (mut store, _) = fixture(client.clone());
            let initial = store.receive_structural(|_| {}).await.unwrap();
            assert!(client.fetch.0.lock().unwrap().pending > 0);
            assert_eq!(client.fetch.0.lock().unwrap().dropped, 0);
            let _removal = store
                .send_with(Action::SetUpload(None), |expected| expected.upload = None)
                .await
                .unwrap();
            initial.finish().await.unwrap();
            assert_eq!(client.fetch.0.lock().unwrap().dropped, 1);
            assert!(client.fetch.0.lock().unwrap().waker.is_none());
            client.fetch.release(b"late response".to_vec());
            assert!(client.requests.lock().unwrap().is_empty());
            store.finish().await.unwrap();
        });
    }

    // Catches unbounded admission and loss of the exact unadmitted action.
    #[test]
    fn full_mailbox_keeps_owned_completion_until_shutdown_rejects_it() {
        block_on(async {
            let runtime = DeterministicRuntime::new();
            let (builder, site) = Store::feature_builder(State::new(), Root, runtime.runtime())
                .mailbox_capacity(1)
                .mount_with_site(mount());
            let store = builder.build().unwrap();
            runtime.run(store.ready()).await.unwrap().unwrap();
            let context = runtime
                .run(store.acquire(&site))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let _queued = store.try_send(Action::Policy(1)).unwrap();
            let value = Receipt {
                client: "captured-A",
                token: "pending owned completion".into(),
                bytes: 2,
            };
            let address = value.token.as_ptr();
            let mut pending = pin!(context.send(value));
            assert!(
                pending
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            drop(store);
            let Err(TaskSendError::Rejected { action, error }) =
                runtime.run(pending).await.unwrap()
            else {
                panic!("unadmitted completion must remain owned")
            };
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            assert_eq!(action.token.as_ptr(), address);
            runtime.run_until_stalled().unwrap();
        });
    }
}
