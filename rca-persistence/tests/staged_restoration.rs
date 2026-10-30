//! Restores a document and selection without publishing an incomplete session.
//!
//! The fixture supplies application policy for two stages and two admitted requests.
//! RCA provides actor updates and owned tasks, not this restoration policy.
//! Literal byte inputs and independent ownership counters check each publication boundary.

use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_channel::{Receiver, Sender};
use futures_executor::block_on;
use rca_core::{Feature, FeatureContext, Task, TaskAccessError, TaskContext};
use rca_persistence::{Backend, Codec};
use rca_test::{Completion, ControlledFuture, TestStore, controlled_future};

/// Publishes only after byte decoding and document hydration both succeed.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps literal invalid boundaries beside the complete two-stage publication schedule"
)]
fn restoration_publishes_only_after_both_stages_validate() {
    for immediate in [false, true] {
        block_on(async {
            let (mut store, mut controls) = fixture(
                1,
                &[Checkpoint {
                    document: 8,
                    page: 2,
                }],
            );
            let mut value = Some(prepared(8, 9, 80));
            let drops = Arc::clone(&value.as_ref().unwrap().ticket.drops);
            if immediate {
                controls
                    .loads
                    .pop_front()
                    .unwrap()
                    .complete(Ok(Some(vec![1, 8, 2, 0])))
                    .unwrap();
                controls
                    .hydration
                    .pop_front()
                    .unwrap()
                    .complete(Ok(value.take().unwrap()))
                    .unwrap();
            }
            let work = store
                .send_with(Action::Restore, |s| {
                    s.request = 1;
                    s.stage = Stage::Loading;
                })
                .await
                .unwrap();
            if !immediate {
                assert!(controls.io.lock().unwrap().load_pending > 0);
                controls
                    .loads
                    .pop_front()
                    .unwrap()
                    .complete(Ok(Some(vec![1, 8, 2, 0])))
                    .unwrap();
            }
            let _hydrating = store
                .receive_update(|s| s.stage = Stage::Hydrating)
                .await
                .unwrap();
            if !immediate {
                assert!(controls.io.lock().unwrap().hydrate_pending > 0);
                store
                    .assert_state(|s| assert_eq!(snapshot(s).visible, (7, 1, 0)))
                    .await
                    .unwrap();
                controls
                    .hydration
                    .pop_front()
                    .unwrap()
                    .complete(Ok(value.take().unwrap()))
                    .unwrap();
            }
            let _published = store
                .receive_update(|s| {
                    s.stage = Stage::Idle;
                    s.visible = (8, 2, 80);
                })
                .await
                .unwrap();
            work.finish().await.unwrap();
            assert!(matches!(
                controls.reports.try_recv().unwrap().unwrap(),
                Disposition::Applied
            ));
            assert_eq!(controls.permits.len(), 2);
            assert_eq!(controls.io.lock().unwrap().saves, 0);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            store.finish().await.unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(store);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        });
    }
    for (response, failure) in [
        (Ok(None), None),
        (Err(Failure::Load), Some(Failure::Load)),
        (Ok(Some(vec![])), Some(Failure::Decode)),
        (Ok(Some(vec![1, 8, 2])), Some(Failure::Decode)),
        (Ok(Some(vec![1, 8, 2, 0, 0])), Some(Failure::Decode)),
        (Ok(Some(vec![2, 8, 2, 0])), Some(Failure::Decode)),
        (Ok(Some(vec![1, 0, 2, 0])), Some(Failure::Decode)),
        (Ok(Some(vec![1, 8, 0, 0])), Some(Failure::Decode)),
    ] {
        block_on(async {
            let (mut store, mut controls) = fixture(1, &[]);
            let work = store
                .send_with(Action::Restore, |s| {
                    s.request = 1;
                    s.stage = Stage::Loading;
                })
                .await
                .unwrap();
            controls
                .loads
                .pop_front()
                .unwrap()
                .complete(response)
                .unwrap();
            let _settled = store
                .receive_update(|s| {
                    s.stage = Stage::Idle;
                    s.failure = failure;
                })
                .await
                .unwrap();
            work.finish().await.unwrap();
            match (failure, controls.reports.try_recv().unwrap().unwrap()) {
                (None, Disposition::Missing) => {}
                (Some(expected), Disposition::Failed(actual, None)) => assert_eq!(actual, expected),
                unexpected => panic!("wrong boundary outcome: {unexpected:?}"),
            }
            assert!(controls.io.lock().unwrap().hydrated.is_empty());
            assert_eq!(controls.permits.len(), 2);
            store.finish().await.unwrap();
        });
    }
    for value in [prepared(9, 9, 81), prepared(8, 1, 82), prepared(8, 0, 83)] {
        block_on(async {
            let (mut store, mut controls) = fixture(
                1,
                &[Checkpoint {
                    document: 8,
                    page: 2,
                }],
            );
            let address = std::ptr::from_ref(value.ticket.as_ref());
            let drops = Arc::clone(&value.ticket.drops);
            let work = store
                .send_with(Action::Restore, |s| {
                    s.request = 1;
                    s.stage = Stage::Loading;
                })
                .await
                .unwrap();
            controls
                .loads
                .pop_front()
                .unwrap()
                .complete(Ok(Some(vec![1, 8, 2, 0])))
                .unwrap();
            let _hydrating = store
                .receive_update(|s| s.stage = Stage::Hydrating)
                .await
                .unwrap();
            controls
                .hydration
                .pop_front()
                .unwrap()
                .complete(Ok(value))
                .unwrap();
            let _invalid = store
                .receive_update(|s| {
                    s.stage = Stage::Idle;
                    s.failure = Some(Failure::InvalidHydration);
                })
                .await
                .unwrap();
            work.finish().await.unwrap();
            let Disposition::Failed(Failure::InvalidHydration, Some(returned)) =
                controls.reports.try_recv().unwrap().unwrap()
            else {
                panic!("invalid hydration must return its original prepared value");
            };
            assert!(std::ptr::eq(
                std::ptr::from_ref(returned.ticket.as_ref()),
                address
            ));
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(returned);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(controls.reports.is_empty());
            assert_eq!(controls.permits.len(), 2);
            store.finish().await.unwrap();
        });
    }
}

/// Preserves the current request through every tested old-stage completion order.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps both stale-stage completion orders and current failure assertions in one causal schedule"
)]
fn out_of_order_stages_preserve_the_current_attempt_even_when_it_fails() {
    for old_hydrating in [false, true] {
        for old_first in [false, true] {
            for current_fails in [false, true] {
                block_on(async {
                    let mut planned = vec![];
                    if old_hydrating {
                        planned.push(Checkpoint {
                            document: 8,
                            page: 2,
                        });
                    }
                    planned.push(Checkpoint {
                        document: 9,
                        page: 3,
                    });
                    let (mut store, mut controls) = fixture(2, &planned);
                    let old_work = store
                        .send_with(Action::Restore, |s| {
                            s.request = 1;
                            s.stage = Stage::Loading;
                        })
                        .await
                        .unwrap();
                    let load = controls.loads.pop_front().unwrap();
                    let mut old = Some(if old_hydrating {
                        load.complete(Ok(Some(vec![1, 8, 2, 0]))).unwrap();
                        let _hydrating = store
                            .receive_update(|s| s.stage = Stage::Hydrating)
                            .await
                            .unwrap();
                        OldWait::Hydration(
                            controls.hydration.pop_front().unwrap(),
                            prepared(8, 9, 80),
                        )
                    } else {
                        OldWait::Load(load)
                    });
                    let current_work = store
                        .send_with(Action::Restore, |s| {
                            s.request = 2;
                            s.stage = Stage::Loading;
                        })
                        .await
                        .unwrap();
                    let busy = store
                        .send_with(Action::Restore, |s| s.busy = 1)
                        .await
                        .unwrap();
                    busy.finish().await.unwrap();
                    assert_eq!(controls.io.lock().unwrap().load_started, 2);
                    assert_eq!(controls.permits.len(), 0);
                    if old_first {
                        receive_stale(&mut store, &controls, old.take().unwrap()).await;
                    }
                    controls
                        .loads
                        .pop_front()
                        .unwrap()
                        .complete(Ok(Some(vec![1, 9, 3, 0])))
                        .unwrap();
                    let _hydrating = store
                        .receive_update(|s| s.stage = Stage::Hydrating)
                        .await
                        .unwrap();
                    controls
                        .hydration
                        .pop_front()
                        .unwrap()
                        .complete(if current_fails {
                            Err(Failure::Hydrate)
                        } else {
                            Ok(prepared(9, 10, 90))
                        })
                        .unwrap();
                    let _current = store
                        .receive_update(|s| {
                            s.stage = Stage::Idle;
                            if current_fails {
                                s.failure = Some(Failure::Hydrate);
                            } else {
                                s.visible = (9, 3, 90);
                            }
                        })
                        .await
                        .unwrap();
                    current_work.finish().await.unwrap();
                    match controls.reports.try_recv().unwrap().unwrap() {
                        Disposition::Applied if !current_fails => {}
                        Disposition::Failed(Failure::Hydrate, None) if current_fails => {}
                        unexpected => panic!("wrong current outcome: {unexpected:?}"),
                    }
                    if !old_first {
                        receive_stale(&mut store, &controls, old.take().unwrap()).await;
                    }
                    old_work.finish().await.unwrap();
                    assert_eq!(controls.io.lock().unwrap().hydrated, planned);
                    assert!(controls.reports.is_empty());
                    assert_eq!(controls.permits.len(), 2);
                    store.finish().await.unwrap();
                });
            }
        }
    }
}

/// Rejects old continuations after replacement even when request identifiers repeat.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps old and fresh same-ID requests, owned completion and abandonment in one schedule"
)]
fn replacement_fences_late_owned_completion_and_abandonment_is_explicit() {
    for old_hydrating in [false, true] {
        for abandoned in [false, true] {
            block_on(async {
                let mut planned = vec![];
                if old_hydrating {
                    planned.push(Checkpoint {
                        document: 8,
                        page: 2,
                    });
                }
                planned.push(Checkpoint {
                    document: 9,
                    page: 3,
                });
                let (mut store, mut controls) = fixture(2, &planned);
                let old_work = store
                    .send_with(Action::Restore, |s| {
                        s.request = 1;
                        s.stage = Stage::Loading;
                    })
                    .await
                    .unwrap();
                let load = controls.loads.pop_front().unwrap();
                let old = if old_hydrating {
                    load.complete(Ok(Some(vec![1, 8, 2, 0]))).unwrap();
                    let _hydrating = store
                        .receive_update(|s| s.stage = Stage::Hydrating)
                        .await
                        .unwrap();
                    OldWait::Hydration(controls.hydration.pop_front().unwrap(), prepared(8, 9, 80))
                } else {
                    OldWait::Load(load)
                };
                let replaced = store
                    .send_with(Action::ReplaceDocument, |s| {
                        s.epoch = 1;
                        s.request = 0;
                        s.stage = Stage::Idle;
                        s.visible = (7, 1, 99);
                    })
                    .await
                    .unwrap();
                replaced.finish().await.unwrap();
                assert_eq!(
                    controls.permits.len(),
                    1,
                    "replacement has not destroyed the old wait"
                );
                let fresh_work = store
                    .send_with(Action::Restore, |s| {
                        s.request = 1;
                        s.stage = Stage::Loading;
                    })
                    .await
                    .unwrap();
                assert_eq!(controls.permits.len(), 0);
                receive_stale(&mut store, &controls, old).await;
                old_work.finish().await.unwrap();
                assert_eq!(
                    controls.permits.len(),
                    1,
                    "only the old helper has returned"
                );
                controls
                    .loads
                    .pop_front()
                    .unwrap()
                    .complete(Ok(Some(vec![1, 9, 3, 0])))
                    .unwrap();
                let _hydrating = store
                    .receive_update(|s| s.stage = Stage::Hydrating)
                    .await
                    .unwrap();
                let reply = controls.hydration.pop_front().unwrap();
                if abandoned {
                    drop(reply);
                } else {
                    reply.complete(Ok(prepared(9, 10, 90))).unwrap();
                }
                let _settled = store
                    .receive_update(|s| {
                        s.stage = Stage::Idle;
                        if abandoned {
                            s.failure = Some(Failure::Abandoned);
                        } else {
                            s.visible = (9, 3, 90);
                        }
                    })
                    .await
                    .unwrap();
                fresh_work.finish().await.unwrap();
                match controls.reports.try_recv().unwrap().unwrap() {
                    Disposition::Applied if !abandoned => {}
                    Disposition::Failed(Failure::Abandoned, None) if abandoned => {}
                    unexpected => panic!("wrong fresh outcome: {unexpected:?}"),
                }
                assert!(controls.reports.is_empty());
                assert_eq!(controls.permits.len(), 2);
                assert_eq!(controls.io.lock().unwrap().hydrated, planned);
                store.finish().await.unwrap();
            });
        }
    }
}

/// Retains the original stage completion until a newer request becomes current.
enum OldWait {
    /// Holds a load that has not yet decoded its checkpoint.
    Load(Completion<Load>),
    /// Holds an owned document whose hydration has not yet reached the actor.
    Hydration(Completion<Hydrated>, PreparedDocument),
}

/// Completes stale work and checks its unchanged state and original owned result.
async fn receive_stale(
    store: &mut TestStore<State, Action, Snapshot>,
    controls: &Controls,
    old: OldWait,
) {
    let ownership = match old {
        OldWait::Load(reply) => {
            reply.complete(Ok(Some(vec![1, 8, 2, 0]))).unwrap();
            None
        }
        OldWait::Hydration(reply, value) => {
            let ownership = (
                std::ptr::from_ref(value.ticket.as_ref()),
                Arc::clone(&value.ticket.drops),
            );
            reply.complete(Ok(value)).unwrap();
            Some(ownership)
        }
    };
    let _stale = store.receive_update(|_| {}).await.unwrap();
    let Disposition::Stale(returned) = controls.reports.try_recv().unwrap().unwrap() else {
        panic!("a stale actor disposition is still one observed no-op commit");
    };
    match (ownership, returned) {
        (None, None) => {}
        (Some((address, drops)), Some(value)) => {
            assert!(std::ptr::eq(
                std::ptr::from_ref(value.ticket.as_ref()),
                address
            ));
            assert_eq!(value.ticket.value.get(), 80);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(value);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
        unexpected => panic!("stale completion lost original ownership: {unexpected:?}"),
    }
}

/// Identifies the exact application boundary that rejected restoration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Failure {
    /// Storage returned a load error.
    Load,
    /// Bytes did not contain a valid version-one checkpoint.
    Decode,
    /// Document hydration returned an error.
    Hydrate,
    /// The prepared document did not match the checkpoint or requested page.
    InvalidHydration,
    /// The test dropped a stage completion without a response.
    Abandoned,
    /// The read-only fixture received an unexpected save.
    Unsupported,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Failure {}

/// Selects one nonzero document identifier and a one-based page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Checkpoint {
    /// Application document identifier encoded in one byte.
    document: u8,
    /// One-based page encoded as a little-endian integer.
    page: u16,
}

/// Decodes a four-byte, version-one application checkpoint.
struct Binary;
impl Codec<Checkpoint> for Binary {
    type Error = Failure;
    fn encode(&self, value: &Checkpoint) -> Result<Vec<u8>, Failure> {
        let [low, high] = value.page.to_le_bytes();
        Ok(vec![1, value.document, low, high])
    }
    fn decode(&self, bytes: &[u8]) -> Result<Checkpoint, Failure> {
        let [1, document, low, high] = bytes else {
            return Err(Failure::Decode);
        };
        let page = u16::from_le_bytes([*low, *high]);
        if *document == 0 || page == 0 {
            return Err(Failure::Decode);
        }
        Ok(Checkpoint {
            document: *document,
            page,
        })
    }
}

/// Tracks unique ownership without requiring `Clone` or `Sync`.
///
/// Allocation identity and drop counts do not claim external resource cleanup.
#[derive(Debug)]
struct Ticket {
    /// Marker that identifies the original owned object.
    value: Cell<u8>,
    /// Independent counter that survives the ticket.
    drops: Arc<AtomicUsize>,
}
impl Drop for Ticket {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}
/// Owns hydrated document data before atomic publication.
#[derive(Debug)]
struct PreparedDocument {
    /// Identity returned by the hydration boundary.
    document: u8,
    /// Available one-based pages in the returned document.
    pages: u16,
    /// Move-only ownership witness that must survive rejection unchanged.
    ticket: Box<Ticket>,
}
/// Creates one uniquely owned result and its observable drop counter.
fn prepared(document: u8, pages: u16, token: u8) -> PreparedDocument {
    PreparedDocument {
        document,
        pages,
        ticket: Box::new(Ticket {
            value: Cell::new(token),
            drops: Arc::new(AtomicUsize::new(0)),
        }),
    }
}

/// Exposes the current restoration stage without publishing partial data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    /// No current request is waiting for a stage response.
    Idle,
    /// Storage has not yet supplied and decoded a checkpoint.
    Loading,
    /// A valid checkpoint is waiting for its prepared document.
    Hydrating,
}
/// Holds application state and a separately owned visible document.
struct State {
    /// Structural generation that prevents reuse of old request identifiers.
    epoch: u64,
    /// Latest admitted request within the current generation.
    request: u64,
    /// Phase belonging only to the current request.
    stage: Stage,
    /// Failure belonging only to the current request.
    failure: Option<Failure>,
    /// Last fully validated document presented to the application.
    visible: PreparedDocument,
    /// One-based selection within the visible document.
    page: u16,
    /// Number of rejected requests while both slots were occupied.
    busy: usize,
}
/// Projects readable state while the actual document retains move-only ownership.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Snapshot {
    /// Current structural generation.
    epoch: u64,
    /// Latest admitted request in that generation.
    request: u64,
    /// Current request's phase.
    stage: Stage,
    /// Current request's failure, if any.
    failure: Option<Failure>,
    /// Visible document identifier, selected page and ownership marker.
    visible: (u8, u16, u8),
    /// Requests rejected by the two-slot admission limit.
    busy: usize,
}
/// Copies observable fields without cloning the owned document.
fn snapshot(s: &State) -> Snapshot {
    Snapshot {
        epoch: s.epoch,
        request: s.request,
        stage: s.stage,
        failure: s.failure,
        visible: (s.visible.document, s.page, s.visible.ticket.value.get()),
        busy: s.busy,
    }
}
/// Starts restoration or replaces the application document lifetime.
enum Action {
    /// Requests restoration if one of the two slots is available.
    Restore,
    /// Starts a fresh generation without destroying old stage work.
    ReplaceDocument,
}
/// Reports the actor's decision and returns any rejected owned document.
#[derive(Debug)]
enum Disposition {
    /// Both stages succeeded and the actor published the new document.
    Applied,
    /// Storage contained no checkpoint, so visible state stayed unchanged.
    Missing,
    /// A current request failed, possibly retaining its rejected prepared document.
    Failed(Failure, Option<PreparedDocument>),
    /// A newer request or generation superseded this original result.
    Stale(Option<PreparedDocument>),
}
/// Distinguishes application decisions from rejected actor access.
type Report = Result<Disposition, TaskAccessError>;

/// Returns one finite request slot when its owned task ends.
struct Permit(Sender<()>);
impl Drop for Permit {
    fn drop(&mut self) {
        // A closed application need not reuse its slots; never wait during Drop.
        let _closed_or_full = self.0.try_send(());
    }
}
/// Supplies application restoration policy over RCA's typed task and actor APIs.
struct Restoration {
    /// Controlled source of checkpoint bytes.
    backend: Storage,
    /// Controlled source of owned prepared documents.
    hydration: Hydration,
    /// Two shared admission slots, independent of request identity.
    permits: Receiver<()>,
    /// Endpoint retained by each admitted request until its task ends.
    release: Sender<()>,
    /// Bounded report sink observed independently by each test.
    report: Arc<dyn Fn(Report) + Send + Sync>,
}
impl Feature for Restoration {
    type State = State;
    type Action = Action;
    fn update(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut FeatureContext<'_, State, Action>,
    ) {
        match action {
            Action::ReplaceDocument => {
                state.epoch += 1;
                state.request = 0;
                state.stage = Stage::Idle;
                state.failure = None;
                state.visible = prepared(7, 9, 99);
                state.page = 1;
            }
            Action::Restore => {
                if self.permits.try_recv().is_err() {
                    state.busy += 1;
                    return;
                }
                let permit = Permit(self.release.clone());
                state.request += 1;
                state.stage = Stage::Loading;
                state.failure = None;
                let identity = (state.epoch, state.request);
                let backend = self.backend.clone();
                let hydration = self.hydration.clone();
                let report = Arc::clone(&self.report);
                context.task(Task::run(move |context| async move {
                    let _permit = permit;
                    report(restore(context, identity, backend, hydration).await);
                }));
            }
        }
    }
}

/// Loads, validates and hydrates a checkpoint before requesting atomic publication.
///
/// Identity is checked after each stage. Rejected owned values return in the report.
async fn restore(
    context: TaskContext<State, Action>,
    identity: (u64, u64),
    backend: Storage,
    hydration: Hydration,
) -> Report {
    let checkpoint = match backend.load().await {
        Ok(Some(bytes)) => match Binary.decode(&bytes) {
            Ok(checkpoint) => checkpoint,
            Err(failure) => {
                return settle(&context, identity, Disposition::Failed(failure, None), None).await;
            }
        },
        Ok(None) => return settle(&context, identity, Disposition::Missing, None).await,
        Err(failure) => {
            return settle(&context, identity, Disposition::Failed(failure, None), None).await;
        }
    };
    let current = context
        .update(move |s| {
            if (s.epoch, s.request) != identity {
                return false;
            }
            s.stage = Stage::Hydrating;
            true
        })
        .await?;
    if !current.value() {
        return Ok(Disposition::Stale(None));
    }
    match hydration.resolve(checkpoint).await {
        Ok(prepared)
            if prepared.document == checkpoint.document && prepared.pages >= checkpoint.page =>
        {
            settle(
                &context,
                identity,
                Disposition::Applied,
                Some((checkpoint.page, prepared)),
            )
            .await
        }
        Ok(prepared) => {
            settle(
                &context,
                identity,
                Disposition::Failed(Failure::InvalidHydration, Some(prepared)),
                None,
            )
            .await
        }
        Err(failure) => settle(&context, identity, Disposition::Failed(failure, None), None).await,
    }
}

/// Publishes only the current request and returns stale ownership unchanged.
async fn settle(
    context: &TaskContext<State, Action>,
    identity: (u64, u64),
    disposition: Disposition,
    prepared: Option<(u16, PreparedDocument)>,
) -> Report {
    let committed = context
        .update(move |s| {
            if (s.epoch, s.request) != identity {
                let value = prepared
                    .map(|(_, value)| value)
                    .or_else(|| match disposition {
                        Disposition::Failed(_, value) | Disposition::Stale(value) => value,
                        Disposition::Applied | Disposition::Missing => None,
                    });
                return Disposition::Stale(value);
            }
            s.stage = Stage::Idle;
            s.failure = match &disposition {
                Disposition::Failed(failure, _) => Some(*failure),
                _ => None,
            };
            if let Some((page, value)) = prepared {
                s.visible = value;
                s.page = page;
            }
            disposition
        })
        .await?;
    Ok(committed.into_parts().0)
}

/// Preserves the distinction between absent storage, bytes and load failure.
type Load = Result<Option<Vec<u8>>, Failure>;
/// Returns one owned document or the hydration boundary's failure.
type Hydrated = Result<PreparedDocument, Failure>;
/// Records a finite stage schedule without using elapsed time as an oracle.
#[derive(Default)]
struct Io {
    /// Planned load futures in request admission order.
    loads: VecDeque<ControlledFuture<Load>>,
    /// Expected checkpoints and their separately controlled hydration futures.
    hydration: VecDeque<(Checkpoint, ControlledFuture<Hydrated>)>,
    /// Number of loads that actually entered the storage boundary.
    load_started: usize,
    /// Checkpoints actually submitted for hydration.
    hydrated: Vec<Checkpoint>,
    /// Observed pending polls that prove the load wait was exercised.
    load_pending: usize,
    /// Observed pending polls that prove the hydration wait was exercised.
    hydrate_pending: usize,
    /// Unexpected writes, which restoration must never issue.
    saves: usize,
}
/// Supplies finite load responses and rejects all writes.
#[derive(Clone)]
struct Storage(Arc<Mutex<Io>>);
impl Backend for Storage {
    type Error = Failure;
    async fn load(&self) -> Load {
        let mut response = {
            let mut io = self.0.lock().unwrap();
            io.load_started += 1;
            io.loads.pop_front().expect("finite planned load")
        };
        poll_fn(|cx| {
            let poll = Pin::new(&mut response).poll(cx);
            if poll.is_pending() {
                self.0.lock().unwrap().load_pending += 1;
            }
            poll
        })
        .await
        .unwrap_or(Err(Failure::Abandoned))
    }
    fn save(&self, _: Vec<u8>) -> impl Future<Output = Result<(), Failure>> + Send {
        self.0.lock().unwrap().saves += 1;
        std::future::ready(Err(Failure::Unsupported))
    }
}
/// Matches each hydration call to its planned checkpoint and owned result.
#[derive(Clone)]
struct Hydration(Arc<Mutex<Io>>);
impl Hydration {
    /// Records the checkpoint before awaiting its independently controlled response.
    async fn resolve(&self, checkpoint: Checkpoint) -> Hydrated {
        let mut response = {
            let mut io = self.0.lock().unwrap();
            let (expected, response) = io.hydration.pop_front().expect("finite planned hydration");
            assert_eq!(checkpoint, expected);
            io.hydrated.push(checkpoint);
            response
        };
        poll_fn(|cx| {
            let poll = Pin::new(&mut response).poll(cx);
            if poll.is_pending() {
                self.0.lock().unwrap().hydrate_pending += 1;
            }
            poll
        })
        .await
        .unwrap_or(Err(Failure::Abandoned))
    }
}
/// Owns external stage replies and independent observations of the application.
struct Controls {
    /// Replies that the test can complete before or after load polling.
    loads: VecDeque<Completion<Load>>,
    /// Replies that retain the original prepared-document ownership.
    hydration: VecDeque<Completion<Hydrated>>,
    /// Actor decisions, including returned stale or invalid documents.
    reports: Receiver<Report>,
    /// Observable free-slot count after each task lifecycle boundary.
    permits: Receiver<()>,
    /// Call history and pending-poll witnesses independent of actor state.
    io: Arc<Mutex<Io>>,
}
/// Builds at most two requests with independent stage controls and report slots.
fn fixture(
    loads: usize,
    hydration: &[Checkpoint],
) -> (TestStore<State, Action, Snapshot>, Controls) {
    assert!(loads <= 2 && hydration.len() <= 2, "finite fixture calls");
    let mut io = Io::default();
    let mut load_controls = VecDeque::new();
    let mut hydration_controls = VecDeque::new();
    for _ in 0..loads {
        let (completion, future) = controlled_future();
        io.loads.push_back(future);
        load_controls.push_back(completion);
    }
    for checkpoint in hydration {
        let (completion, future) = controlled_future();
        io.hydration.push_back((*checkpoint, future));
        hydration_controls.push_back(completion);
    }
    let io = Arc::new(Mutex::new(io));
    let (release, permits) = async_channel::bounded(2);
    release.try_send(()).unwrap();
    release.try_send(()).unwrap();
    let (reports, receive) = async_channel::bounded(2);
    let feature = Restoration {
        backend: Storage(Arc::clone(&io)),
        hydration: Hydration(Arc::clone(&io)),
        permits: permits.clone(),
        release,
        report: Arc::new(move |report| {
            reports
                .try_send(report)
                .expect("finite reports drained by each test");
        }),
    };
    let state = State {
        epoch: 0,
        request: 0,
        stage: Stage::Idle,
        failure: None,
        visible: prepared(7, 9, 0),
        page: 1,
        busy: 0,
    };
    (
        TestStore::projected_builder(state, feature, snapshot).build(),
        Controls {
            loads: load_controls,
            hydration: hydration_controls,
            reports: receive,
            permits,
            io,
        },
    )
}
