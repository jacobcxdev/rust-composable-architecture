//! Runs two-source search with captured input, live settings and named cancellation.
//!
//! Run with `cargo run -p rca-example-composition-search --example named_operations`.
//! The retaining host below is proof machinery, not required application architecture.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;

use rca::{Feature, FeatureContext, OperationPolicy, Runtime, RuntimeFuture, Store, Task};

/// Separates editable search settings from results supplied by independent task generations.
#[derive(Debug, Default)]
struct SearchState {
    /// Retains the latest edited or submitted query, independently of captured task input.
    query: String,
    /// Supplies the live setting that each task reads after its response arrives.
    limit: usize,
    /// Owns successful responses accepted since the latest start cleared the list.
    results: Vec<SearchResult>,
    /// Records a client failure until the next start clears it.
    failure: Option<&'static str>,
}

/// Preserves a response's captured query, source identity and later live-setting read.
#[derive(Debug, PartialEq, Eq)]
struct SearchResult {
    /// Owns the submitted query rather than a later edit to the search state.
    query: String,
    /// Distinguishes the first and second client calls within an attempt.
    source: usize,
    /// Records the limit read after the response, without filtering the demonstration value.
    live_limit: usize,
    /// Owns the original client response without cloning its string allocation.
    value: String,
}

/// Starts named task generations or edits settings without starting new work.
enum SearchAction {
    /// Starts two client calls and clears the previous results and failure.
    Start(
        /// Supplies the query that each new task captures independently.
        String,
        /// Controls replacement for the first sibling, while the second always coexists.
        OperationPolicy,
    ),
    /// Changes visible settings while existing tasks retain their captured query.
    Edit(
        /// Replaces the query displayed in state, not an existing task's input.
        String,
        /// Replaces the live limit for later task reads.
        usize,
    ),
}

/// Declares two independently awaited tasks for each search attempt.
///
/// Controlled responses stand in for client calls. Each start consumes two prepared responses.
/// An exhausted queue is a demonstration setup error and causes a panic.
/// The authoring pattern is the reducer, not the manual host.
struct SearchFeature {
    /// Transfers prepared client futures into tasks in submission and source order.
    responses: VecDeque<controlled::Response>,
}

impl Feature for SearchFeature {
    type State = SearchState;
    type Action = SearchAction;

    fn reduce(
        &mut self,
        state: &mut SearchState,
        action: &SearchAction,
        context: &mut FeatureContext<'_, SearchState, SearchAction>,
    ) {
        match action {
            SearchAction::Edit(query, limit) => {
                state.query.clone_from(query);
                state.limit = *limit;
            }
            SearchAction::Start(query, policy) => {
                state.query.clone_from(query);
                state.failure = None; // A new attempt clears the previous failure.
                state.results.clear();
                for source in 0..2 {
                    let query = query.clone(); // Input belongs to this request.
                    let response = self.responses.pop_front().expect("prepared client call");
                    // Replace once, then coexist: replacing twice would cancel
                    // the fresh first sibling. These are two distinct generations.
                    let policy = if source == 0 {
                        *policy
                    } else {
                        OperationPolicy::Coexist
                    };
                    context.task(Task::run_named(
                        "search",
                        policy,
                        move |context| async move {
                            let result = response.await;
                            let live_limit = context
                                .read(|state: &SearchState| state.limit)
                                .await
                                .expect("live search");
                            let _receipt = context
                                .update(move |state| match result {
                                    Ok(value) => state.results.push(SearchResult {
                                        query,
                                        source,
                                        live_limit,
                                        value,
                                    }),
                                    Err(failure) => state.failure = Some(failure),
                                })
                                .await
                                .expect("live search result");
                        },
                    ));
                }
            }
        }
    }
}

/// Retains observed task counts, destruction counts and the Store's owned final state.
#[derive(Debug)]
struct Trace {
    /// Records real active-generation counts at the selected schedule boundaries.
    active_generations: Vec<usize>,
    /// Records destruction of each prepared response, including cancelled attempts.
    response_drops: Vec<usize>,
    /// Owns the state transferred out of the Store at shutdown.
    final_state: SearchState,
}

/// Runs the finite schedule and checks real task completion and response ownership.
///
/// Every count and wait comes from the Store endpoint. The host retains actual runtime-owned tasks.
///
/// # Panics
///
/// Panics without a current Tokio runtime or if the demonstrated ownership and scheduling contracts fail.
fn run_trace() -> Trace {
    let host = controlled::Host::new();
    let mut responses = VecDeque::new();
    let mut releases = Vec::new();
    let mut drops = Vec::new();
    // Five two-source attempts cover coexistence, replacement, failure and retry.
    for _ in 0..10 {
        let (response, release, destroyed) = controlled::response();
        responses.push_back(response);
        releases.push(release);
        drops.push(destroyed);
    }
    let store = Store::feature_builder(
        SearchState::default(),
        SearchFeature { responses },
        Runtime::new(host.clone()),
    )
    .build()
    .expect("search store");
    let mut actor = host.take_one();
    let counts = generation_trace(&host, &mut actor, &store, &drops, &releases);
    let addresses = result_trace(&host, &mut actor, &store, &releases);
    let mut shutdown = Box::pin(store.shutdown_into_state());
    assert!(controlled::poll(shutdown.as_mut()).is_pending());
    assert!(controlled::poll(actor.as_mut()).is_ready());
    let Poll::Ready(Ok(final_state)) = controlled::poll(shutdown.as_mut()) else {
        panic!("owned state handoff")
    };
    assert_eq!(
        final_state.results.len(),
        addresses.len(),
        "both successful responses must survive the state handoff"
    );
    for (result, address) in final_state.results.iter().zip(addresses) {
        assert_eq!(result.value.as_ptr() as usize, address);
    }
    let response_drops: Vec<_> = drops.iter().map(|n| n.load(Ordering::SeqCst)).collect();
    assert_eq!(response_drops, [1; 10]);
    assert_eq!(counts, [0, 2, 4, 6, 2, 2, 1, 0]);
    Trace {
        active_generations: counts,
        response_drops,
        final_state,
    }
}

/// Records coexistence, replacement and cancellation before releasing any client response.
///
/// Captured waits retain only their original generation set.
/// Cancellation requests do not prove destruction until the bodies run.
///
/// # Panics
///
/// Panics if Store access fails or the controlled task counts, waits or destruction checks disagree.
fn generation_trace(
    host: &controlled::Host,
    actor: &mut RuntimeFuture,
    store: &Store<SearchState, SearchAction>,
    drops: &[Arc<AtomicUsize>],
    releases: &[controlled::Release],
) -> Vec<usize> {
    let operation = store.operation("search");
    let mut counts = vec![
        controlled::ordered(actor, operation.status())
            .unwrap()
            .active_count(),
    ];
    let mut bodies = Vec::new();

    for (query, policy) in [
        ("old", OperationPolicy::Coexist),
        ("coexisting", OperationPolicy::Coexist),
    ] {
        let _receipt =
            controlled::ordered(actor, store.send(SearchAction::Start(query.into(), policy)))
                .unwrap();
        bodies.extend(host.take_pair());
        for body in &mut bodies {
            assert!(controlled::poll(body.as_mut()).is_pending());
        }
        counts.push(
            controlled::ordered(actor, operation.status())
                .unwrap()
                .active_count(),
        );
    }
    // Capture four current generations BEFORE the replacement pair exists.
    let mut old_wait = Box::pin(controlled::ordered(actor, operation.capture()).unwrap());
    assert!(controlled::poll(old_wait.as_mut()).is_pending());
    let _receipt = controlled::ordered(
        actor,
        store.send(SearchAction::Start(
            "replacement".into(),
            OperationPolicy::Replace,
        )),
    )
    .unwrap();
    let mut replacement = host.take_pair();
    for body in &mut replacement {
        assert!(controlled::poll(body.as_mut()).is_pending());
    }
    counts.push(
        controlled::ordered(actor, operation.status())
            .unwrap()
            .active_count(),
    );
    assert_eq!(counts, [0, 2, 4, 6]);
    assert!(releases[..6].iter().all(controlled::Release::is_waiting));
    assert!(controlled::poll(old_wait.as_mut()).is_pending());
    assert!(drops.iter().take(6).all(|n| n.load(Ordering::SeqCst) == 0));
    for body in &mut bodies {
        assert!(controlled::poll(body.as_mut()).is_ready());
    }
    assert_eq!(controlled::poll(old_wait.as_mut()), Poll::Ready(Ok(())));
    for body in &mut replacement {
        assert!(controlled::poll(body.as_mut()).is_pending());
    }
    counts.push(
        controlled::ordered(actor, operation.status())
            .unwrap()
            .active_count(),
    );

    // Cancel-and-await means capture, request cancellation, then await that set.
    let mut cancelled = Box::pin(controlled::ordered(actor, operation.capture()).unwrap());
    controlled::ordered(actor, operation.cancel()).unwrap();
    counts.push(
        controlled::ordered(actor, operation.status())
            .unwrap()
            .active_count(),
    );
    assert!(controlled::poll(cancelled.as_mut()).is_pending());
    assert_eq!(drops[4].load(Ordering::SeqCst), 0);
    assert_eq!(drops[5].load(Ordering::SeqCst), 0);
    assert!(controlled::poll(replacement[0].as_mut()).is_ready());
    assert!(controlled::poll(cancelled.as_mut()).is_pending());
    counts.push(
        controlled::ordered(actor, operation.status())
            .unwrap()
            .active_count(),
    );
    assert!(controlled::poll(replacement[1].as_mut()).is_ready());
    assert_eq!(controlled::poll(cancelled.as_mut()), Poll::Ready(Ok(())));
    counts.push(
        controlled::ordered(actor, operation.status())
            .unwrap()
            .active_count(),
    );

    counts
}

/// Checks captured input, live settings and retry ownership through actual task updates.
///
/// Returns the two successful retry allocations' addresses for the final state-handoff check.
///
/// # Panics
///
/// Panics if Store access fails or the controlled response and completion checks disagree.
fn result_trace(
    host: &controlled::Host,
    actor: &mut RuntimeFuture,
    store: &Store<SearchState, SearchAction>,
    releases: &[controlled::Release],
) -> [usize; 2] {
    let operation = store.operation("search");
    let _receipt = controlled::ordered(
        actor,
        store.send(SearchAction::Start(
            "captured".into(),
            OperationPolicy::Coexist,
        )),
    )
    .unwrap();
    let mut attempt = host.take_pair();
    for body in &mut attempt {
        assert!(controlled::poll(body.as_mut()).is_pending());
    }
    assert!(releases[6..8].iter().all(controlled::Release::is_waiting));
    let _receipt = controlled::ordered(
        actor,
        store.send(SearchAction::Edit("edited later".into(), 3)),
    )
    .unwrap();
    let value = String::from("owned first response");
    let address = value.as_ptr() as usize;
    releases[6].complete(Ok(value));
    releases[7].complete(Err("offline"));
    for body in &mut attempt {
        controlled::finish_body(actor, body);
    }
    controlled::ordered(
        actor,
        store.read(move |state| {
            assert_eq!(state.query, "edited later");
            assert_eq!(state.failure, Some("offline"));
            assert_eq!(state.results.len(), 1);
            assert_eq!(state.results[0].query, "captured");
            assert_eq!(state.results[0].live_limit, 3);
            assert_eq!(state.results[0].value.as_ptr() as usize, address);
        }),
    )
    .unwrap();

    let _receipt = controlled::ordered(
        actor,
        store.send(SearchAction::Start(
            "retry".into(),
            OperationPolicy::Replace,
        )),
    )
    .unwrap();
    controlled::ordered(
        actor,
        store.read(|state| {
            assert_eq!(state.failure, None);
            assert!(state.results.is_empty());
        }),
    )
    .unwrap();
    let mut retry = host.take_pair();
    for body in &mut retry {
        assert!(controlled::poll(body.as_mut()).is_pending());
    }
    assert!(releases[8..].iter().all(controlled::Release::is_waiting));
    let values = [String::from("primary"), String::from("secondary")];
    let addresses = values.each_ref().map(|value| value.as_ptr() as usize);
    for (release, value) in releases[8..].iter().zip(values) {
        release.complete(Ok(value));
    }
    let mut finished = Box::pin(controlled::ordered(actor, operation.capture()).unwrap());
    assert!(controlled::poll(finished.as_mut()).is_pending());
    for body in &mut retry {
        controlled::finish_body(actor, body);
    }
    assert_eq!(controlled::poll(finished.as_mut()), Poll::Ready(Ok(())));
    assert_eq!(
        controlled::ordered(actor, operation.status())
            .unwrap()
            .active_count(),
        0
    );
    addresses
}

/// Prints the checked operation trace without treating elapsed time as a performance measurement.
///
/// # Panics
///
/// Panics if the finite trace's scheduling or ownership checks fail.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let trace = run_trace();
    println!("active generations: {:?}", trace.active_generations);
    println!("response destruction counts: {:?}", trace.response_drops);
    println!("owned final results: {:?}", trace.final_state.results);
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn named_search_trace_uses_real_task_ownership() {
        let trace = super::run_trace();
        assert_eq!(trace.final_state.failure, None);
        assert_eq!(
            trace.final_state.results,
            [
                super::SearchResult {
                    query: "retry".into(),
                    source: 0,
                    live_limit: 3,
                    value: "primary".into()
                },
                super::SearchResult {
                    query: "retry".into(),
                    source: 1,
                    live_limit: 3,
                    value: "secondary".into()
                },
            ]
        );
    }
}

mod controlled {
    //! Retains real runtime tasks and client replies for the executable's finite schedule.
    //!
    //! Applications normally use their existing runtime and async clients, not this manual scheduler.

    use rca::{Clock, MonotonicInstant, RuntimeDriver, RuntimeFuture, TokioRuntime};
    use std::fmt;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};

    /// Queues spawned tasks for manual polling while retaining the current Tokio clock.
    #[derive(Clone)]
    pub struct Host {
        /// Shares ownership of spawned tasks until the schedule explicitly takes them.
        queued: Arc<Mutex<Vec<RuntimeFuture>>>,
        /// Delegates time operations without automatically polling queued tasks.
        clock: Clock,
    }
    impl fmt::Debug for Host {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("RetainingHost").finish_non_exhaustive()
        }
    }
    impl Host {
        /// Creates an empty task queue inside the current Tokio runtime.
        ///
        /// # Panics
        ///
        /// Panics if no current Tokio runtime supplies the clock.
        pub fn new() -> Self {
            Self {
                queued: Arc::default(),
                clock: TokioRuntime::current().unwrap().clock(),
            }
        }
        /// Transfers the single queued actor future to the schedule.
        ///
        /// # Panics
        ///
        /// Panics if the queue is poisoned or does not contain exactly one future.
        pub fn take_one(&self) -> RuntimeFuture {
            let mut queued = self.queued.lock().unwrap();
            assert_eq!(queued.len(), 1);
            queued.pop().unwrap()
        }
        /// Transfers two queued task bodies in their original spawn order.
        ///
        /// # Panics
        ///
        /// Panics if the queue is poisoned or does not contain exactly two futures.
        pub fn take_pair(&self) -> [RuntimeFuture; 2] {
            let queued = std::mem::take(&mut *self.queued.lock().unwrap());
            assert_eq!(queued.len(), 2);
            let mut tasks = queued.into_iter();
            [tasks.next().unwrap(), tasks.next().unwrap()]
        }
    }
    impl RuntimeDriver for Host {
        fn spawn(&self, task: RuntimeFuture) {
            self.queued.lock().unwrap().push(task);
        }
        fn now(&self) -> MonotonicInstant {
            self.clock.now()
        }
        fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
            self.clock.sleep_until(deadline)
        }
        fn yield_now(&self) -> RuntimeFuture {
            Box::pin(tokio::task::yield_now())
        }
    }
    /// Polls once without scheduling another poll when the future wakes.
    ///
    /// The surrounding schedule must explicitly poll again after each relevant state change.
    pub fn poll<T>(future: Pin<&mut (impl Future<Output = T> + ?Sized)>) -> Poll<T> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }
    /// Alternates request and actor polls within this example's fixed handoff budget.
    ///
    /// # Panics
    ///
    /// Panics if the actor completes early or the request needs more than four request polls.
    pub fn ordered<T>(actor: &mut RuntimeFuture, request: impl Future<Output = T>) -> T {
        let mut request = Box::pin(request);
        // This finite trace permits four request/actor handoffs, not a retry policy.
        for _ in 0..4 {
            if let Poll::Ready(value) = poll(request.as_mut()) {
                return value;
            }
            assert!(poll(actor.as_mut()).is_pending());
        }
        panic!("finite actor request schedule exhausted")
    }
    /// Drives a released client task through its ordered read and update.
    ///
    /// # Panics
    ///
    /// Panics if the actor completes early or the body needs more than four body polls.
    pub fn finish_body(actor: &mut RuntimeFuture, body: &mut RuntimeFuture) {
        // A released response needs bounded live-read and update handoffs.
        for _ in 0..4 {
            if poll(body.as_mut()).is_ready() {
                return;
            }
            assert!(poll(actor.as_mut()).is_pending());
        }
        panic!("finite client/read/update schedule exhausted")
    }
    /// Holds one staged response and the waker from its latest pending poll.
    #[derive(Default)]
    struct Slot {
        /// Owns a supplied outcome until the response future takes it.
        result: Option<Result<String, &'static str>>,
        /// Retains the latest pending poll's waker until a release takes it.
        waker: Option<Waker>,
    }
    /// Waits for a controlled client outcome and records its own destruction.
    pub struct Response {
        /// Shares the outcome slot with the schedule's release handle.
        slot: Arc<Mutex<Slot>>,
        /// Counts this future's destruction, whether it completes or is cancelled.
        drops: Arc<AtomicUsize>,
    }
    /// Supplies an outcome without owning or polling the response future.
    pub struct Release(
        /// Keeps the outcome slot available even after the response future is destroyed.
        Arc<Mutex<Slot>>,
    );
    /// Creates a client future, its release handle and an independent destruction counter.
    pub fn response() -> (Response, Release, Arc<AtomicUsize>) {
        let slot = Arc::new(Mutex::new(Slot::default()));
        let drops = Arc::new(AtomicUsize::new(0));
        (
            Response {
                slot: Arc::clone(&slot),
                drops: Arc::clone(&drops),
            },
            Release(slot),
            drops,
        )
    }
    impl Future for Response {
        type Output = Result<String, &'static str>;
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut slot = self.slot.lock().unwrap();
            if let Some(result) = slot.result.take() {
                Poll::Ready(result)
            } else {
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
    impl Drop for Response {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Release {
        /// Reports a recorded pending poll with no outcome currently staged.
        ///
        /// This checks slot contents, not whether the response future is still alive.
        ///
        /// # Panics
        ///
        /// Panics if the shared slot's mutex is poisoned.
        pub fn is_waiting(&self) -> bool {
            let slot = self.0.lock().unwrap();
            slot.waker.is_some() && slot.result.is_none()
        }

        /// Stages an owned outcome and wakes the recorded waiter after unlocking.
        ///
        /// # Panics
        ///
        /// Panics if an unconsumed outcome already occupies the slot or its mutex is poisoned.
        pub fn complete(&self, result: Result<String, &'static str>) {
            let wake = {
                let mut slot = self.0.lock().unwrap();
                assert!(slot.result.replace(result).is_none());
                slot.waker.take()
            };
            if let Some(waker) = wake {
                waker.wake();
            }
        }
    }
}
