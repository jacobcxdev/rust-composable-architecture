//! Authorises an optional detail through a typed output and an owned reply.
//!
//! Run with `cargo run -p rca-example-composition-search --example authorisation`.
//! The request owns its reply channel. A reply supplies data, not authority:
//! the captured task context checks the original incarnation before mutation.
//! `docs/authorisation.md` explains the independent request and inbox bounds.

use std::sync::Arc;
use std::time::Duration;

use rca::{
    Feature, FeatureContext, MountIdentity, MountSite, OperationPolicy, OptionalMount, Output,
    OutputDelivery, OutputError, OutputFlow, RootMount, Store, Task, TaskAccessError, TaskContext,
    TokioRuntime,
};
use tokio::sync::{Semaphore, mpsc, oneshot};

/// Groups each detail's independent attempts for explicit cancellation and completion waits.
const AUTHORISE: &str = "authorise-detail";
/// Identifies the demonstration detail across replacement incarnations.
const DETAIL_ID: u64 = 7;
/// Gives the respondent thirty seconds after output delivery, not task admission.
const RESPONSE_WINDOW: Duration = Duration::from_secs(30);

/// Transfers approval data without cloning or diagnostic disclosure.
///
/// The example deliberately omits `Clone` and `Debug`. Reporting never prints credentials.
struct Decision(
    /// Owns the credential data transferred from the respondent into detail state.
    String,
);

/// Rejects a request before the respondent produces any approval data.
#[derive(Debug)]
enum ReplyError {
    /// The respondent's inbox cannot accept another request immediately.
    Busy,
}

/// Transfers one decision or a respondent's immediate rejection.
type Outcome = Result<Decision, ReplyError>;
/// Owns the single response opportunity, including recovery after failed delivery.
struct Reply(
    /// Retains the sender until the first response attempt, whether delivery succeeds or fails.
    Option<oneshot::Sender<Outcome>>,
);

impl Reply {
    /// Sends once, returning the original outcome if delivery is no longer possible.
    ///
    /// # Errors
    ///
    /// Returns the unchanged outcome after an earlier attempt or receiver destruction.
    fn respond(&mut self, outcome: Outcome) -> Result<(), Outcome> {
        match self.0.take() {
            Some(sender) => sender.send(outcome),
            None => Err(outcome),
        }
    }
}

/// Carries display data and an owned endpoint, not permission to change state.
struct Request {
    /// Identifies the displayed detail, but does not identify its mounted incarnation.
    detail_id: u64,
    /// Owns the requester's single reply channel, independently of its state authority.
    reply: Reply,
}

/// Separates capacity, delivery, reply and scoped-access failures without exposing approval data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestError {
    /// The admission limit or respondent inbox rejects another request immediately.
    Busy,
    /// No output handler accepts ownership of the request.
    NoRecipient,
    /// The respondent drops its reply channel without a decision.
    Abandoned,
    /// The reply window expires before the receiver yields an outcome.
    TimedOut,
    /// The original task context cannot complete a read, output or state update.
    Access(
        /// Preserves the operation's access reason and progress without remapping them.
        TaskAccessError,
    ),
}

/// Reports ordinary completion outside state without exposing decision contents.
type Report = Arc<dyn Fn(Result<(), RequestError>) + Send + Sync>;

/// Distinguishes rejection, abandonment and expiration of the reply window.
///
/// # Errors
///
/// Returns `Busy` for rejection, `Abandoned` for sender destruction, or `TimedOut` at expiration.
///
/// # Panics
///
/// Panics if polled without a current Tokio runtime with time enabled.
async fn wait_reply(
    receive: oneshot::Receiver<Outcome>,
    window: Duration,
) -> Result<Decision, RequestError> {
    match tokio::time::timeout(window, receive).await {
        Ok(Ok(Ok(decision))) => Ok(decision),
        Ok(Ok(Err(ReplyError::Busy))) => Err(RequestError::Busy),
        Ok(Err(_)) => Err(RequestError::Abandoned),
        Err(_) => Err(RequestError::TimedOut),
    }
}

/// Retains one admission permit through delivery, reply and scoped mutation.
///
/// A reply supplies data only. The final update uses the original detail context.
/// Cancellation drops the permit and receiver, but an already-queued request can remain in the inbox.
///
/// # Errors
///
/// Returns a capacity, delivery, reply or scoped-access failure at the corresponding stage.
///
/// # Panics
///
/// Panics if reply waiting begins without a current Tokio runtime with time enabled.
async fn authorise(
    context: TaskContext<DetailState, DetailAction>,
    output: Output<Request>,
    limit: Arc<Semaphore>,
    window: Duration,
) -> Result<(), RequestError> {
    // Immediate rejection is application policy. Keep this permit through update.
    let _permit = limit
        .try_acquire_owned()
        .map_err(|_unavailable| RequestError::Busy)?;
    let detail_id = context
        .read(|state| state.id)
        .await
        .map_err(RequestError::Access)?;
    let (send, receive) = oneshot::channel();
    let request = Request {
        detail_id,
        reply: Reply(Some(send)),
    };
    match context.output(&output, request).await {
        Ok(OutputDelivery::Consumed { .. }) => {}
        Ok(OutputDelivery::Unhandled { value, .. }) => {
            drop(value);
            return Err(RequestError::NoRecipient);
        }
        Err(OutputError::Returned { value, error }) => {
            drop(value);
            return Err(RequestError::Access(error));
        }
        Err(OutputError::Failed { error }) => return Err(RequestError::Access(error)),
    }
    let decision = wait_reply(receive, window).await?;
    // The response is data. Only this captured incarnation can authorise mutation.
    context
        .update(move |state| state.approval = Some(decision))
        .await
        .map_err(RequestError::Access)?;
    Ok(())
}

/// Keeps approval private to one mounted detail incarnation.
struct DetailState {
    /// Keeps the business identifier stable across replacements of this detail.
    id: u64,
    /// Distinguishes successive incarnations with the same business identifier.
    replacement: u64,
    /// Owns approval data accepted through this incarnation's task context.
    approval: Option<Decision>,
}

/// Requests approval through a caller-selected output route and reply window.
enum DetailAction {
    /// Starts an independent attempt without replacing an earlier attempt.
    Request(
        /// Routes the owned request to the parent's respondent handler.
        Output<Request>,
        /// Limits the reply wait after the output handler accepts the request.
        Duration,
    ),
}

/// Starts independent attempts while the semaphore enforces application capacity.
struct DetailFeature {
    /// Shares the admission bound across attempts and replacement incarnations.
    limit: Arc<Semaphore>,
    /// Receives ordinary completion results, but not cancellation through future destruction.
    report: Report,
}

impl Feature for DetailFeature {
    type State = DetailState;
    type Action = DetailAction;

    fn reduce(
        &mut self,
        _: &mut DetailState,
        action: &DetailAction,
        context: &mut FeatureContext<'_, DetailState, DetailAction>,
    ) {
        let DetailAction::Request(output, window) = action;
        let output = output.clone();
        let window = *window;
        let limit = Arc::clone(&self.limit);
        let report = Arc::clone(&self.report);
        context.task(Task::run_named(
            AUTHORISE,
            OperationPolicy::Coexist,
            move |context| async move {
                // Ordinary errors are reported outside State, including ScopeEnded.
                // Cancellation drops this future and need not invoke the callback.
                report(authorise(context, output, limit, window).await);
            },
        ));
    }
}

/// Owns the optional detail whose identity controls mounted task authority.
struct AppState {
    /// Retains the current incarnation, or no requester when the detail is absent.
    detail: Option<DetailState>,
}

/// Changes the parent-owned lifetime of the optional requester.
enum AppAction {
    /// Invalidates the current incarnation while preserving its business identifier.
    ReplaceDetail,
}

/// Replaces a present detail's incarnation and clears its previous approval.
struct AppFeature;

impl Feature for AppFeature {
    type State = AppState;
    type Action = AppAction;

    fn reduce(
        &mut self,
        state: &mut AppState,
        _: &AppAction,
        _: &mut FeatureContext<'_, AppState, AppAction>,
    ) {
        if let Some(detail) = &mut state.detail {
            detail.replacement += 1;
            detail.approval = None;
        }
    }
}

/// Keeps the Store, request receiver and admission bound under one owner.
struct Application {
    /// Owns the parent actor and provides its shutdown boundary.
    store: Store<AppState, AppAction>,
    /// Locates the current detail incarnation for scoped task acquisition.
    site: MountSite<AppState, DetailState, DetailAction, AppAction>,
    /// Supplies the typed request route passed into each child action.
    output: Output<Request>,
    /// Owns queued requests, which can outlive their requester's cancellation.
    requests: mpsc::Receiver<Request>,
    /// Exposes the shared admission bound independently of inbox capacity.
    limit: Arc<Semaphore>,
}

/// Mounts the requester and installs its bounded, nonblocking parent recipient.
///
/// # Errors
///
/// Returns runtime acquisition, Store construction or readiness errors before handing off ownership.
async fn application(report: Report) -> Result<Application, Box<dyn std::error::Error>> {
    // One active attempt and one queued request are independent application bounds.
    let limit = Arc::new(Semaphore::new(1));
    let child_limit = Arc::clone(&limit);
    let mount = OptionalMount::new(
        |state: &AppState| state.detail.as_ref(),
        |state: &mut AppState| state.detail.as_mut(),
        |state: &AppState| {
            state.detail.as_ref().map(|detail| MountIdentity {
                key: detail.id,
                replacement: detail.replacement,
                configuration: 0,
            })
        },
        move |_| DetailFeature {
            limit: Arc::clone(&child_limit),
            report: Arc::clone(&report),
        },
    );
    let (builder, site) = RootMount::new(AppFeature)
        .builder(
            AppState {
                detail: Some(DetailState {
                    id: DETAIL_ID,
                    replacement: 0,
                    approval: None,
                }),
            },
            TokioRuntime::current()?,
        )
        .mailbox_capacity(1)
        .mount_with_site(mount);
    let (send, requests) = mpsc::channel(1);
    let (builder, output) = builder.output::<Request>(&site, |handlers| {
        handlers.at_root(move |_, request, _| match send.try_send(request) {
            Ok(()) => OutputFlow::Consumed,
            Err(mpsc::error::TrySendError::Closed(request)) => OutputFlow::Continue(request),
            Err(mpsc::error::TrySendError::Full(mut request)) => {
                // No decision payload exists yet. An already-ended requester needs no Busy.
                drop(request.reply.respond(Err(ReplyError::Busy)));
                OutputFlow::Consumed
            }
        });
    });
    let store = builder.build()?;
    store.ready().await?;
    Ok(Application {
        store,
        site,
        output,
        requests,
        limit,
    })
}

/// Demonstrates one owned approval, then replaces the requester and shuts down its Store.
///
/// # Errors
///
/// Returns setup, scoped-access, absent-detail, closed-inbox, reply-delivery or Store shutdown errors.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = application(Arc::new(|result| match result {
        Ok(()) => println!("Authorisation committed to its requester."),
        Err(error) => eprintln!("Authorisation ended: {error:?}"),
    }))
    .await?;
    let child = app
        .store
        .acquire(&app.site)
        .await?
        .ok_or("detail is absent")?;
    let task = child
        .send(DetailAction::Request(app.output.clone(), RESPONSE_WINDOW))
        .await?;
    let mut request = app.requests.recv().await.ok_or("respondent inbox closed")?;
    println!(
        "Received authorisation request for detail {}.",
        request.detail_id
    );
    if request
        .reply
        .respond(Ok(Decision(String::from("demonstration approval"))))
        .is_err()
    {
        return Err("requester no longer accepts the response".into());
    }
    task.finish().await;
    let approved = child
        .read(|state| {
            state
                .approval
                .as_ref()
                .is_some_and(|decision| !decision.0.is_empty())
        })
        .await?;
    println!(
        "Approved: {approved}; available request slots: {}.",
        app.limit.available_permits()
    );
    // Replacement keeps the business ID, but starts an unauthorised incarnation.
    let _replacement = app.store.send(AppAction::ReplaceDetail).await?;
    app.store.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    type Reports = Arc<Mutex<Vec<Result<(), RequestError>>>>;

    async fn recorded_application() -> (Application, Reports) {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let reported = Arc::clone(&reports);
        let app = application(Arc::new(move |result| {
            reported.lock().unwrap().push(result)
        }))
        .await
        .unwrap();
        (app, reports)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn closed_inbox_and_abandoned_reply_are_distinct() {
        let (mut app, _) = recorded_application().await;
        let child = app.store.acquire(&app.site).await.unwrap().unwrap();
        let pending = tokio::spawn(authorise(
            child.clone(),
            app.output.clone(),
            Arc::clone(&app.limit),
            RESPONSE_WINDOW,
        ));
        let request = app.requests.recv().await.unwrap();
        drop(request);
        assert_eq!(pending.await.unwrap(), Err(RequestError::Abandoned));
        assert_eq!(app.limit.available_permits(), 1);
        app.requests.close();
        assert_eq!(
            authorise(
                child,
                app.output.clone(),
                Arc::clone(&app.limit),
                RESPONSE_WINDOW
            )
            .await,
            Err(RequestError::NoRecipient)
        );
        assert_eq!(app.limit.available_permits(), 1);
        app.store.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn foreign_output_preserves_access_failure_instead_of_no_recipient() {
        let (app, _) = recorded_application().await;
        let (foreign, _) = recorded_application().await;
        let child = app.store.acquire(&app.site).await.unwrap().unwrap();
        let Err(RequestError::Access(error)) = authorise(
            child,
            foreign.output.clone(),
            Arc::clone(&app.limit),
            RESPONSE_WINDOW,
        )
        .await
        else {
            panic!("foreign output must retain its access failure");
        };
        assert_eq!(error.reason(), rca::TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), rca::TaskProgress::NotStarted);
        assert_eq!(app.limit.available_permits(), 1);
        app.store.shutdown().await.unwrap();
        foreign.store.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_request_releases_permit_but_can_leave_the_inbox_full() {
        let (mut app, reports) = recorded_application().await;
        let child = app.store.acquire(&app.site).await.unwrap().unwrap();
        let _first = child
            .send(DetailAction::Request(app.output.clone(), RESPONSE_WINDOW))
            .await
            .unwrap();
        // Observe the real queue without removing its actual endpoint-produced value.
        tokio::time::timeout(RESPONSE_WINDOW, async {
            while app.requests.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let operation = child.operation(AUTHORISE);
        let wait = operation.capture().await.unwrap();
        operation.cancel().await.unwrap();
        wait.await.unwrap();
        assert_eq!(app.limit.available_permits(), 1);
        assert!(
            reports.lock().unwrap().is_empty(),
            "cancellation is not an ordinary result"
        );
        let second = child
            .send(DetailAction::Request(app.output.clone(), RESPONSE_WINDOW))
            .await
            .unwrap();
        second.finish().await;
        assert_eq!(*reports.lock().unwrap(), [Err(RequestError::Busy)]);
        let mut stale = app.requests.recv().await.unwrap();
        let late = String::from("cancelled inbox request");
        let address = late.as_ptr();
        let Err(Ok(Decision(late))) = stale.reply.respond(Ok(Decision(late))) else {
            panic!("cancelled receiver must return the reply");
        };
        assert_eq!(late.as_ptr(), address);
        assert_eq!(app.limit.available_permits(), 1);
        app.store.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn same_id_replacement_uses_a_different_owned_reply() {
        let (mut app, reports) = recorded_application().await;
        let old = app.store.acquire(&app.site).await.unwrap().unwrap();
        let _first = old
            .send(DetailAction::Request(app.output.clone(), RESPONSE_WINDOW))
            .await
            .unwrap();
        let mut old_request = app.requests.recv().await.unwrap();
        let old_wait = old.operation(AUTHORISE).capture().await.unwrap();
        let _replacement = app.store.send(AppAction::ReplaceDetail).await.unwrap();
        old_wait.await.unwrap();
        assert_eq!(app.limit.available_permits(), 1);
        let new = app.store.acquire(&app.site).await.unwrap().unwrap();
        let second = new
            .send(DetailAction::Request(app.output.clone(), RESPONSE_WINDOW))
            .await
            .unwrap();
        let mut new_request = app.requests.recv().await.unwrap();
        assert_eq!(old_request.detail_id, new_request.detail_id);
        let late = String::from("old incarnation approval");
        let address = late.as_ptr();
        let Err(Ok(Decision(late))) = old_request.reply.respond(Ok(Decision(late))) else {
            panic!("old endpoint cannot answer new request");
        };
        assert_eq!(late.as_ptr(), address);
        assert!(new.read(|state| state.approval.is_none()).await.unwrap());
        assert!(!second.is_finished());
        assert!(
            new_request
                .reply
                .respond(Ok(Decision(String::from("new incarnation approval"))))
                .is_ok()
        );
        second.finish().await;
        assert!(new.read(|state| state.approval.is_some()).await.unwrap());
        assert_eq!(*reports.lock().unwrap(), [Ok(())]);
        app.store.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn externally_retained_request_revalidates_after_the_reply() {
        let (mut app, _) = recorded_application().await;
        let old = app.store.acquire(&app.site).await.unwrap().unwrap();
        let pending = tokio::spawn(authorise(
            old,
            app.output.clone(),
            Arc::clone(&app.limit),
            RESPONSE_WINDOW,
        ));
        let mut request = app.requests.recv().await.unwrap();
        let _replacement = app.store.send(AppAction::ReplaceDetail).await.unwrap();
        assert!(
            request
                .reply
                .respond(Ok(Decision(String::from("old external approval"))))
                .is_ok()
        );
        let Err(RequestError::Access(error)) = pending.await.unwrap() else {
            panic!("old captured authority must reject the update");
        };
        assert_eq!(error.reason(), rca::TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), rca::TaskProgress::NotStarted);
        let new = app.store.acquire(&app.site).await.unwrap().unwrap();
        assert!(new.read(|state| state.approval.is_none()).await.unwrap());
        assert_eq!(app.limit.available_permits(), 1);
        app.store.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_destroys_waiter_before_late_reply_recovery() {
        let (mut app, reports) = recorded_application().await;
        let child = app.store.acquire(&app.site).await.unwrap().unwrap();
        let _first = child
            .send(DetailAction::Request(app.output.clone(), RESPONSE_WINDOW))
            .await
            .unwrap();
        let mut request = app.requests.recv().await.unwrap();
        let wait = child.operation(AUTHORISE).capture().await.unwrap();
        let state = app.store.shutdown_into_state().await.unwrap();
        wait.await.unwrap();
        assert!(state.detail.unwrap().approval.is_none());
        assert!(reports.lock().unwrap().is_empty());
        assert_eq!(app.limit.available_permits(), 1);
        let late = String::from("approval after shutdown");
        let address = late.as_ptr();
        let Err(Ok(Decision(late))) = request.reply.respond(Ok(Decision(late))) else {
            panic!("destroyed waiter must return its late reply");
        };
        assert_eq!(late.as_ptr(), address);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn actual_request_timeout_releases_its_permit_and_returns_late_reply() {
        let (mut app, _) = recorded_application().await;
        let child = app.store.acquire(&app.site).await.unwrap().unwrap();
        let pending = tokio::spawn(authorise(
            child,
            app.output.clone(),
            Arc::clone(&app.limit),
            Duration::ZERO,
        ));
        let mut request = app.requests.recv().await.unwrap();
        assert_eq!(pending.await.unwrap(), Err(RequestError::TimedOut));
        assert_eq!(app.limit.available_permits(), 1);
        assert!(
            request
                .reply
                .respond(Ok(Decision(String::from("late timeout approval"))))
                .is_err()
        );
        app.store.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn child_request_moves_the_reply_into_its_original_incarnation() {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let reported = Arc::clone(&reports);
        let mut app = application(Arc::new(move |result| {
            reported.lock().unwrap().push(result);
        }))
        .await
        .unwrap();
        let child = app.store.acquire(&app.site).await.unwrap().unwrap();
        let _task = child
            .send(DetailAction::Request(app.output.clone(), RESPONSE_WINDOW))
            .await
            .unwrap();
        let mut request = app.requests.recv().await.unwrap();
        assert_eq!(request.detail_id, DETAIL_ID);
        let wait = child.operation(AUTHORISE).capture().await.unwrap();
        let credential = String::from("owned demonstration approval");
        let address = credential.as_ptr() as usize;
        assert!(request.reply.respond(Ok(Decision(credential))).is_ok());
        wait.await.unwrap();
        assert_eq!(
            child
                .read(|state| state.approval.as_ref().unwrap().0.as_ptr() as usize)
                .await
                .unwrap(),
            address,
        );
        assert_eq!(*reports.lock().unwrap(), [Ok(())]);
        assert_eq!(app.limit.available_permits(), 1);
        app.store.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn overlapping_request_is_busy_without_replacing_the_first() {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let reported = Arc::clone(&reports);
        let mut app = application(Arc::new(move |result| {
            reported.lock().unwrap().push(result);
        }))
        .await
        .unwrap();
        let child = app.store.acquire(&app.site).await.unwrap().unwrap();
        let first = child
            .send(DetailAction::Request(app.output.clone(), RESPONSE_WINDOW))
            .await
            .unwrap();
        let mut request = app.requests.recv().await.unwrap();
        let second = child
            .send(DetailAction::Request(app.output.clone(), RESPONSE_WINDOW))
            .await
            .unwrap();
        second.finish().await;
        assert_eq!(*reports.lock().unwrap(), [Err(RequestError::Busy)]);
        assert_eq!(
            child
                .operation(AUTHORISE)
                .status()
                .await
                .unwrap()
                .active_count(),
            1
        );
        assert!(!first.is_finished());
        assert!(
            request
                .reply
                .respond(Ok(Decision(String::from("first survives overlap"))))
                .is_ok()
        );
        first.finish().await;
        assert_eq!(*reports.lock().unwrap(), [Err(RequestError::Busy), Ok(())]);
        assert_eq!(app.limit.available_permits(), 1);
        app.store.shutdown().await.unwrap();
    }

    #[test]
    fn reply_transfers_the_original_and_returns_a_duplicate() {
        let (send, mut receive) = oneshot::channel();
        let mut reply = Reply(Some(send));
        let first = String::from("first demonstration approval");
        let first_address = first.as_ptr();
        assert!(reply.respond(Ok(Decision(first))).is_ok());
        let Ok(Ok(Decision(first))) = receive.try_recv() else {
            panic!("the original decision must reach its receiver");
        };
        assert_eq!(first.as_ptr(), first_address);
        let duplicate = String::from("distinct duplicate approval");
        let duplicate_address = duplicate.as_ptr();
        let Err(Ok(Decision(duplicate))) = reply.respond(Ok(Decision(duplicate))) else {
            panic!("a duplicate must return its own unsent decision");
        };
        assert_eq!(duplicate.as_ptr(), duplicate_address);
    }

    #[test]
    fn reply_returns_the_original_after_receiver_destruction() {
        let (send, receive) = oneshot::channel();
        let mut reply = Reply(Some(send));
        drop(receive);
        let late = String::from("late demonstration approval");
        let address = late.as_ptr();
        let Err(Ok(Decision(late))) = reply.respond(Ok(Decision(late))) else {
            panic!("a closed receiver must return the unsent decision");
        };
        assert_eq!(late.as_ptr(), address);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reply_timeout_closes_the_same_observed_pending_receiver() {
        let (send, mut receive) = oneshot::channel();
        let mut reply = Reply(Some(send));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(
            std::pin::Pin::new(&mut receive)
                .poll(&mut context)
                .is_pending()
        );
        assert!(matches!(
            wait_reply(receive, std::time::Duration::ZERO).await,
            Err(RequestError::TimedOut),
        ));
        let late = String::from("approval after reply deadline");
        let address = late.as_ptr();
        let Err(Ok(Decision(late))) = reply.respond(Ok(Decision(late))) else {
            panic!("the timeout must drop the actual receiver");
        };
        assert_eq!(late.as_ptr(), address);
    }
}
