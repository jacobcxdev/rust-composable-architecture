//! Refreshes captured recipients from one canonical parent document.

use std::cell::Cell;
use std::sync::Arc;

use futures_executor::block_on;
use rca_core::{
    CommandPort, Feature, FeatureContext, MountIdentity, MountSite, OptionalMount, RootMount,
    Store, Task, TaskAccessReason, TaskContext, TaskProgress, TaskSendError, TryCommandError,
};
use rca_test::{ControlledFuture, DeterministicRuntime, TestStore, controlled_future};

// Catches short-circuiting on Full, live reacquisition, reordered reports and
// reconstructing rejected payloads instead of returning the original allocation.
#[test]
fn full_stale_and_live_recipients_are_attempted_in_captured_order() {
    block_on(async {
        let full = recipient(None).await;
        let stale = recipient(None).await;
        let live = recipient(None).await;
        let document = document(1, "canonical");
        let identity = Arc::as_ptr(&document.text).cast::<u8>() as usize;
        let (release, wait) = controlled_future();
        let (mut parent, tickets) = parent(
            document,
            vec![full.port.clone(), stale.port.clone(), live.port.clone()],
            wait,
        );
        let refresh = parent.send(Action::Refresh).await.unwrap();
        assert!(!refresh.is_finished());
        let _queued = full.store.try_send(RecipientAction::Noop).unwrap();
        let _replacement = stale
            .host
            .run(stale.store.send(RecipientAction::Replace))
            .await
            .unwrap()
            .unwrap();
        parent
            .send_with(Action::Notification(notification(2, "newer")), |s| {
                s.document = (2, "newer".to_owned());
            })
            .await
            .unwrap()
            .finish()
            .await
            .unwrap();
        release.complete(()).unwrap();
        parent
            .finish()
            .await
            .expect_err("refresh still owes receipts");
        // Only later owners are driven. A full first owner cannot hold them up.
        live.host.run_until_stalled().unwrap();
        assert_eq!(
            live.snapshot().await,
            (vec![(1, "canonical".to_owned(), identity)], 0)
        );
        stale.host.run_until_stalled().unwrap();
        let _report = parent
            .receive_update(|s| {
                s.report = Some(vec![
                    Status::Full,
                    Status::Rejected(TaskAccessReason::ScopeEnded, TaskProgress::NotStarted),
                    Status::Committed,
                ]);
                s.recovered = vec![
                    (tickets[0], 1, "canonical".to_owned()),
                    (tickets[1], 1, "canonical".to_owned()),
                ];
            })
            .await
            .unwrap();
        refresh.finish().await.unwrap();
        parent.finish().await.unwrap();
        assert_eq!(stale.snapshot().await, (vec![], 0));
        assert_eq!(full.snapshot().await, (vec![], 0));
    });
}

// Catches mistaking a cloned port for an independent backend/mailbox.
#[test]
fn cloned_handles_share_capacity_but_fresh_recipients_do_not() {
    block_on(async {
        let shared = recipient(None).await;
        let independent = recipient(None).await;
        let (release, wait) = controlled_future();
        let (mut parent, tickets) = parent(
            document(1, "one"),
            vec![
                shared.port.clone(),
                shared.port.clone(),
                independent.port.clone(),
            ],
            wait,
        );
        let refresh = parent.send(Action::Refresh).await.unwrap();
        release.complete(()).unwrap();
        parent
            .finish()
            .await
            .expect_err("receipts need destination actors");
        independent.host.run_until_stalled().unwrap();
        shared.host.run_until_stalled().unwrap();
        let _report = parent
            .receive_update(|s| {
                s.report = Some(vec![Status::Committed, Status::Full, Status::Committed]);
                s.recovered = vec![(tickets[1], 1, "one".to_owned())];
            })
            .await
            .unwrap();
        refresh.finish().await.unwrap();
        parent.finish().await.unwrap();
        assert_eq!(shared.snapshot().await.0.len(), 1);
        assert_eq!(independent.snapshot().await.0.len(), 1);
    });
}

// Catches external input bypassing bounded validation or mutating an aliased
// historical document, and treating an empty recipient group as an error.
#[test]
fn bounded_notifications_replace_canonical_data_and_empty_refresh_completes() {
    block_on(async {
        for (revision, text) in [
            (0, "valid".to_owned()),
            (1, String::new()),
            (1, "x".repeat(65)),
            (1, "é".repeat(33)),
        ] {
            Notification::parse(revision, text).expect_err("invalid external input");
        }
        let (release, wait) = controlled_future();
        let (mut parent, _) = parent(document(1, "before"), vec![], wait);
        let _notification = parent
            .send_with(Action::Notification(notification(2, "after")), |s| {
                s.document = (2, "after".to_owned());
            })
            .await
            .unwrap();
        let refresh = parent.send(Action::Refresh).await.unwrap();
        release.complete(()).unwrap();
        let _report = parent
            .receive_update(|s| s.report = Some(vec![]))
            .await
            .unwrap();
        refresh.finish().await.unwrap();
        parent.finish().await.unwrap();
    });
}

// Catches sending after a cancelled suspension, retracting already admitted
// commands, or claiming destination task completion from a commit receipt.
#[test]
fn cancellation_and_commit_only_completion_have_distinct_boundaries() {
    for cancel_before_admission in [true, false] {
        block_on(async {
            let (child_release, child_wait) = controlled_future();
            let destination = recipient(Some(child_wait)).await;
            let (release, wait) = controlled_future();
            let (mut parent, _) =
                parent(document(1, "owned"), vec![destination.port.clone()], wait);
            let refresh = parent.send(Action::Refresh).await.unwrap();
            if cancel_before_admission {
                refresh.cancel().await.unwrap();
                release
                    .complete(())
                    .expect_err("cancelled waiter returns its payload");
                refresh.finish().await.unwrap();
                assert_eq!(destination.snapshot().await, (vec![], 0));
                drop(child_release);
            } else {
                release.complete(()).unwrap();
                parent
                    .finish()
                    .await
                    .expect_err("destination has not committed");
                destination.host.run_until_stalled().unwrap();
                let _report = parent
                    .receive_update(|s| s.report = Some(vec![Status::Committed]))
                    .await
                    .unwrap();
                refresh.finish().await.unwrap();
                // The parent has joined commits, not each destination's causal task.
                assert_eq!(destination.snapshot().await.1, 0);
                child_release.complete(()).unwrap();
                destination.host.run_until_stalled().unwrap();
                assert_eq!(destination.snapshot().await.1, 1);
            }
            parent.finish().await.unwrap();
        });
    }
}

// Catches retracting admitted work when its caller stops awaiting receipts.
#[test]
fn cancellation_after_admission_does_not_retract_destination_work() {
    block_on(async {
        let destination = recipient(None).await;
        let (release, wait) = controlled_future();
        let (mut parent, _) = parent(
            document(1, "admitted"),
            vec![destination.port.clone()],
            wait,
        );
        let refresh = parent.send(Action::Refresh).await.unwrap();
        release.complete(()).unwrap();
        parent
            .finish()
            .await
            .expect_err("waiting for admitted receipt");
        refresh.cancel().await.unwrap();
        refresh.finish().await.unwrap();
        // No destination actor was driven before cancellation. Its accepted
        // envelope still executes, even though the original receipt was dropped.
        let received = destination.snapshot().await.0;
        assert_eq!(received[0].0, 1);
        assert_eq!(received[0].1, "admitted");
        parent.finish().await.unwrap();
    });
}

/// A cloned document shares one immutable text allocation across recipients.
#[derive(Clone)]
struct Document {
    revision: u64,
    text: Arc<str>,
}

fn document(revision: u64, text: &str) -> Document {
    Document {
        revision,
        text: Arc::from(text),
    }
}

// This application accepts at most 64 UTF-8 bytes per external notification.
// Validate before allocating an actor input, not after admitting unbounded data.
#[derive(Debug)]
struct Notification {
    revision: u64,
    text: String,
}

impl Notification {
    /// Rejects invalid external text before the actor receives an input.
    fn parse(revision: u64, text: String) -> Result<Self, String> {
        if revision == 0 || text.is_empty() || text.len() > 64 {
            return Err(text);
        }
        Ok(Self { revision, text })
    }
}

fn notification(revision: u64, text: &str) -> Notification {
    Notification::parse(revision, text.to_owned()).unwrap()
}

// Neither Clone nor Sync. The stable allocation identifies the original command
// returned on Full or pre-entry rejection; the immutable document is shared.
struct Refresh {
    ticket: Box<Cell<u8>>,
    document: Document,
}

impl Refresh {
    fn identity(&self) -> usize {
        std::ptr::from_ref(self.ticket.as_ref()) as usize
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    Committed,
    Full,
    Rejected(TaskAccessReason, TaskProgress),
    Failed(TaskAccessReason, TaskProgress),
}

/// Retains failed original commands separately from their status report.
struct State {
    document: Document,
    report: Option<Vec<Status>>,
    recovered: Vec<Refresh>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    document: (u64, String),
    report: Option<Vec<Status>>,
    recovered: Vec<(usize, u64, String)>,
}

/// Records recovered command identities without moving their owned tickets.
fn snapshot(state: &State) -> Snapshot {
    Snapshot {
        document: (state.document.revision, state.document.text.to_string()),
        report: state.report.clone(),
        recovered: state
            .recovered
            .iter()
            .map(|r| {
                (
                    r.identity(),
                    r.document.revision,
                    r.document.text.to_string(),
                )
            })
            .collect(),
    }
}

enum Action {
    Notification(Notification),
    Refresh,
}

/// Holds one ticket per captured recipient until the refresh task starts.
struct Parent {
    recipients: Vec<CommandPort<Refresh>>,
    #[expect(
        clippy::vec_box,
        reason = "stable original allocations witness owned command recovery"
    )]
    tickets: Vec<Box<Cell<u8>>>,
    wait: Option<ControlledFuture<()>>,
}

impl Feature for Parent {
    type State = State;
    type Action = Action;

    fn update(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut FeatureContext<'_, State, Action>,
    ) {
        match action {
            Action::Notification(input) => {
                // External storage is not observed: this synchronous actor turn
                // alone replaces canonical data, captured history stays owned.
                state.document = document(input.revision, &input.text);
            }
            Action::Refresh => {
                let batch: Vec<_> = self
                    .recipients
                    .iter()
                    .cloned()
                    .zip(std::mem::take(&mut self.tickets))
                    .map(|(port, ticket)| {
                        (
                            port,
                            Refresh {
                                ticket,
                                document: state.document.clone(),
                            },
                        )
                    })
                    .collect();
                let wait = self.wait.take().expect("finite recipe starts one refresh");
                context.task(Task::run(
                    move |context: TaskContext<State, Action>| async move {
                        if wait.await.is_err() {
                            return;
                        }
                        let mut attempts = Vec::with_capacity(batch.len());
                        for (port, command) in batch {
                            attempts.push(port.try_send(command));
                        }
                        let mut report = Vec::with_capacity(attempts.len());
                        let mut recovered = Vec::new();
                        for attempt in attempts {
                            match attempt {
                                Err(TryCommandError::Full(command)) => {
                                    report.push(Status::Full);
                                    recovered.push(command);
                                }
                                Err(TryCommandError::Rejected { command, error }) => {
                                    report.push(Status::Rejected(error.reason(), error.progress()));
                                    recovered.push(command);
                                }
                                Ok(receipt) => match receipt.committed().await {
                                    Ok(_destination_work) => report.push(Status::Committed),
                                    Err(TaskSendError::Rejected { action, error }) => {
                                        report.push(Status::Rejected(
                                            error.reason(),
                                            error.progress(),
                                        ));
                                        recovered.push(action);
                                    }
                                    Err(TaskSendError::Failed { error }) => {
                                        report
                                            .push(Status::Failed(error.reason(), error.progress()));
                                    }
                                },
                            }
                        }
                        // Root cancellation/retirement ends this operation. Never
                        // retry through root input or reconstruct rejected commands.
                        let _stopped = context
                            .update(move |state| {
                                state.report = Some(report);
                                state.recovered = recovered;
                            })
                            .await;
                    },
                ));
            }
        }
    }
}

/// Returns each ticket identity so the test can reject reconstructed commands.
fn parent(
    document: Document,
    recipients: Vec<CommandPort<Refresh>>,
    wait: ControlledFuture<()>,
) -> (TestStore<State, Action, Snapshot>, Vec<usize>) {
    let tickets: Vec<_> = recipients.iter().map(|_| Box::new(Cell::new(0))).collect();
    let identities = tickets
        .iter()
        .map(|ticket| std::ptr::from_ref(ticket.as_ref()) as usize)
        .collect();
    let feature = Parent {
        recipients,
        tickets,
        wait: Some(wait),
    };
    (
        TestStore::projected_builder(
            State {
                document,
                report: None,
                recovered: vec![],
            },
            feature,
            snapshot,
        )
        .build(),
        identities,
    )
}

#[derive(Default)]
struct ChildState {
    received: Vec<(u64, String, usize)>,
    completed: usize,
}

struct RecipientState {
    child: Option<ChildState>,
    replacement: u64,
}

enum RecipientAction {
    Noop,
    Replace,
}
struct RecipientParent;
impl Feature for RecipientParent {
    type State = RecipientState;
    type Action = RecipientAction;
    fn update(
        &mut self,
        state: &mut RecipientState,
        action: &RecipientAction,
        _: &mut FeatureContext<'_, RecipientState, RecipientAction>,
    ) {
        if matches!(action, RecipientAction::Replace) {
            state.replacement += 1;
            state.child = Some(ChildState::default());
        }
    }
}

struct Child(Option<ControlledFuture<()>>);
impl Feature for Child {
    type State = ChildState;
    type Action = Refresh;
    fn update(
        &mut self,
        state: &mut ChildState,
        action: &Refresh,
        context: &mut FeatureContext<'_, ChildState, Refresh>,
    ) {
        state.received.push((
            action.document.revision,
            action.document.text.to_string(),
            Arc::as_ptr(&action.document.text).cast::<u8>() as usize,
        ));
        if let Some(wait) = self.0.take() {
            context.task(Task::run(
                move |context: TaskContext<ChildState, Refresh>| async move {
                    if wait.await.is_ok() {
                        let _stopped = context.update(|state| state.completed += 1).await;
                    }
                },
            ));
        }
    }
}

/// Owns an independent scheduler and mounted mailbox, unlike a cloned port.
struct Recipient {
    host: DeterministicRuntime,
    store: Store<RecipientState, RecipientAction>,
    site: MountSite<RecipientState, ChildState, Refresh, RecipientAction>,
    port: CommandPort<Refresh>,
}

impl Recipient {
    /// Drives the destination actor before reading its installed child.
    async fn snapshot(&self) -> (Vec<(u64, String, usize)>, usize) {
        let child = self
            .host
            .run(self.store.acquire(&self.site))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        self.host
            .run(child.read(|state| (state.received.clone(), state.completed)))
            .await
            .unwrap()
            .unwrap()
    }
}

async fn recipient(mut wait: Option<ControlledFuture<()>>) -> Recipient {
    // Each call owns fresh State, declaration and scheduler. Only explicit port
    // clones share a mailbox; cloning a dependency container proves no isolation.
    let host = DeterministicRuntime::new();
    let mount = OptionalMount::new(
        |state: &RecipientState| state.child.as_ref(),
        |state: &mut RecipientState| state.child.as_mut(),
        |state: &RecipientState| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: state.replacement,
                configuration: 0,
            })
        },
        move |_| Child(wait.take()),
    );
    let (builder, site, commands) = RootMount::new(RecipientParent)
        .builder(
            RecipientState {
                child: Some(ChildState::default()),
                replacement: 0,
            },
            host.runtime(),
        )
        .mailbox_capacity(1)
        .mount_with_command(mount, |command| command);
    let store = builder.build().unwrap();
    host.run(store.ready()).await.unwrap().unwrap();
    let port = host
        .run(store.command_port(&commands))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    Recipient {
        host,
        store,
        site,
        port,
    }
}
