use std::future::Future;
#[cfg(feature = "deterministic")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
#[cfg(feature = "deterministic")]
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use rca_core::Runtime;
#[cfg(feature = "deterministic")]
use rca_test::DeterministicRuntime;
use rca_tokio::TokioRuntime;

#[cfg(feature = "deterministic")]
/// Records whether the current outer poll requested another poll.
struct ReadyWake(AtomicBool);

#[cfg(feature = "deterministic")]
impl Wake for ReadyWake {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

/// Owns one runtime, including tasks that outlive an individual client future.
pub(crate) enum Host {
    Native(tokio::runtime::Runtime),
    #[cfg(feature = "deterministic")]
    Deterministic(DeterministicRuntime),
}

impl Host {
    /// Builds a host and returns a weak witness that must expire at host teardown.
    pub(crate) fn new(
        native: bool,
        budget: usize,
    ) -> Result<(Self, Runtime, Weak<()>), &'static str> {
        let owner = Arc::new(());
        let weak = Arc::downgrade(&owner);
        // Host-owned teardown witness, never useful work.
        let witness = async move {
            std::future::pending::<()>().await;
            drop(owner);
        };
        if native {
            let host = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .map_err(|_| "Tokio host construction failed")?;
            let runtime = {
                let _entered = host.enter();
                TokioRuntime::current().map_err(|_| "Tokio RCA adapter construction failed")?
            };
            drop(host.spawn(witness));
            Ok((Self::Native(host), runtime, weak))
        } else {
            #[cfg(feature = "deterministic")]
            {
                let host = DeterministicRuntime::with_step_budget(budget);
                host.spawn_task(Box::pin(witness));
                let runtime = host.runtime();
                Ok((Self::Deterministic(host), runtime, weak))
            }
            #[cfg(not(feature = "deterministic"))]
            {
                let _ = budget;
                Err("deterministic workloads require the deterministic feature")
            }
        }
    }

    /// Runs with the normal native watchdog or deterministic runnable-task budget.
    pub(crate) fn run<F>(&self, future: F) -> Result<F::Output, &'static str>
    where
        F: Future + Send,
        F::Output: Send,
    {
        self.run_bounded(future, Duration::from_secs(300))
    }

    /// Uses a native deadline; deterministic execution uses its configured task budget.
    ///
    /// The deterministic path can wait for an external wake. Use `run_ready_bounded`
    /// when a self-contained workload must reject quiescence instead of waiting.
    pub(crate) fn run_bounded<F>(
        &self,
        future: F,
        deadline: Duration,
    ) -> Result<F::Output, &'static str>
    where
        F: Future + Send,
        F::Output: Send,
    {
        match self {
            Self::Native(host) => host
                .block_on(async { tokio::time::timeout(deadline, future).await })
                .map_err(|_| "native host deadline exceeded"),
            #[cfg(feature = "deterministic")]
            Self::Deterministic(host) => futures_executor::block_on(host.run(future))
                .map_err(|_| "deterministic drive budget exhausted"),
        }
    }

    /// Drives ready work with a native deadline or deterministic task/outer-poll bounds.
    ///
    /// Every poll/destructor must return; external signals and pending timers are
    /// not driven. Errors drop the client, not admitted tasks or external owners.
    /// Release/cancel those owners before a separately bounded cleanup call.
    pub(crate) fn run_ready_bounded<F>(
        &self,
        future: F,
        deadline: Duration,
        outer_poll_budget: usize,
    ) -> Result<F::Output, &'static str>
    where
        F: Future + Send,
        F::Output: Send,
    {
        if outer_poll_budget == 0 {
            return Err("ready drive requires a positive outer poll budget");
        }
        match self {
            Self::Native(_) => self.run_bounded(future, deadline),
            #[cfg(feature = "deterministic")]
            Self::Deterministic(host) => {
                let wake = Arc::new(ReadyWake(AtomicBool::new(false)));
                let waker = Waker::from(Arc::clone(&wake));
                let mut context = Context::from_waker(&waker);
                // Preserve Drive's aggregate scheduler budget across outer polls.
                let mut drive = std::pin::pin!(host.run(future));
                for _ in 0..outer_poll_budget {
                    wake.0.store(false, Ordering::Release);
                    match drive.as_mut().poll(&mut context) {
                        Poll::Ready(Ok(value)) => return Ok(value),
                        Poll::Ready(Err(_)) => {
                            return Err("deterministic ready drive runnable task budget exhausted");
                        }
                        Poll::Pending => {
                            if !wake.0.swap(false, Ordering::AcqRel) {
                                return Err("deterministic ready drive quiescent pending");
                            }
                        }
                    }
                }
                Err("deterministic ready drive outer poll budget exhausted")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Host;
    use rca_core::{Effect, ReduceContext, Reducer, Store};
    use std::future::{pending, poll_fn};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;
    use std::time::Duration;

    const DEADLINE: Duration = Duration::from_secs(1);

    struct DropCount(Arc<AtomicUsize>);

    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn lanes() -> impl Iterator<Item = bool> {
        [true, false]
            .into_iter()
            .filter(|native| *native || cfg!(feature = "deterministic"))
    }

    #[test]
    fn ready_rejects_zero_budget_without_polling() {
        for native in lanes() {
            let (host, runtime, weak) = Host::new(native, 32).unwrap();
            let polls = AtomicUsize::new(0);
            let drops = Arc::new(AtomicUsize::new(0));
            let guard = DropCount(Arc::clone(&drops));
            let client = async {
                let _guard = guard;
                poll_fn(|_| {
                    polls.fetch_add(1, Ordering::SeqCst);
                    Poll::Ready(())
                })
                .await;
            };
            assert_eq!(
                host.run_ready_bounded(client, DEADLINE, 0),
                Err("ready drive requires a positive outer poll budget")
            );
            assert_eq!(polls.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(runtime);
            drop(host);
            assert!(weak.upgrade().is_none());
        }
    }

    #[test]
    fn ready_native_delegates_success_and_deadline() {
        let (host, runtime, weak) = Host::new(true, 32).unwrap();
        assert_eq!(host.run_ready_bounded(async { 42 }, DEADLINE, 1), Ok(42));
        let drops = Arc::new(AtomicUsize::new(0));
        let guard = DropCount(Arc::clone(&drops));
        assert_eq!(
            host.run_ready_bounded(
                async move {
                    let _guard = guard;
                    pending::<()>().await;
                },
                Duration::ZERO,
                1,
            ),
            Err("native host deadline exceeded")
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(runtime);
        drop(host);
        assert!(weak.upgrade().is_none());
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn ready_rejects_unwoken_pending() {
        for cap in [1, 3] {
            let (host, runtime, weak) = Host::new(false, 32).unwrap();
            let polls = AtomicUsize::new(0);
            let drops = Arc::new(AtomicUsize::new(0));
            let guard = DropCount(Arc::clone(&drops));
            let client = async {
                let _guard = guard;
                poll_fn(|_| {
                    polls.fetch_add(1, Ordering::SeqCst);
                    Poll::<()>::Pending
                })
                .await;
            };
            assert_eq!(
                host.run_ready_bounded(client, DEADLINE, cap),
                Err("deterministic ready drive quiescent pending")
            );
            // Drive checks the client before and after its ready batch, then
            // once more after listener registration closes the arrival race.
            assert_eq!(polls.load(Ordering::SeqCst), 3);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(runtime);
            drop(host);
            assert!(weak.upgrade().is_none());
        }
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn ready_rejects_open_empty_channel() {
        let (host, runtime, weak) = Host::new(false, 32).unwrap();
        let (sender, receiver) = async_channel::bounded::<Arc<()>>(1);
        let drops = Arc::new(AtomicUsize::new(0));
        let guard = DropCount(Arc::clone(&drops));
        assert_eq!(
            host.run_ready_bounded(
                async {
                    let _guard = guard;
                    receiver.recv().await.map(drop)
                },
                DEADLINE,
                3,
            ),
            Err("deterministic ready drive quiescent pending")
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(!sender.is_closed());
        let owner = Arc::new(());
        let channel_owner = Arc::downgrade(&owner);
        sender.try_send(owner).unwrap();
        drop(sender);
        host.run_ready_bounded(async { drop(receiver.recv().await.unwrap()) }, DEADLINE, 3)
            .unwrap();
        assert!(receiver.is_closed());
        drop(receiver);
        assert!(channel_owner.upgrade().is_none());
        drop(runtime);
        drop(host);
        assert!(weak.upgrade().is_none());
    }

    struct ResponseState {
        _owner: Arc<()>,
        roots: usize,
        responses: usize,
        reductions: usize,
    }

    struct ResponseReducer {
        _owner: Arc<()>,
        pending: bool,
        effect_drops: Arc<AtomicUsize>,
    }

    impl Reducer for ResponseReducer {
        type State = ResponseState;
        type Action = bool;

        fn reduce(
            &mut self,
            state: &mut ResponseState,
            response: &bool,
            _: &mut ReduceContext<'_>,
        ) -> Effect<bool> {
            state.reductions += 1;
            if *response {
                state.responses += 1;
                Effect::none()
            } else {
                state.roots += 1;
                let guard = DropCount(Arc::clone(&self.effect_drops));
                let stall = self.pending;
                Effect::future(async move {
                    let _guard = guard;
                    if stall {
                        pending::<()>().await;
                    }
                    Some(true)
                })
            }
        }
    }

    fn response_store(
        runtime: rca_core::Runtime,
        stall: bool,
        effect_drops: &Arc<AtomicUsize>,
    ) -> (
        Store<ResponseState, bool>,
        std::sync::Weak<()>,
        std::sync::Weak<()>,
    ) {
        let reducer_owner = Arc::new(());
        let state_owner = Arc::new(());
        let weak_reducer = Arc::downgrade(&reducer_owner);
        let weak_state = Arc::downgrade(&state_owner);
        let store = Store::builder(
            ResponseState {
                _owner: state_owner,
                roots: 0,
                responses: 0,
                reductions: 0,
            },
            ResponseReducer {
                _owner: reducer_owner,
                pending: stall,
                effect_drops: Arc::clone(effect_drops),
            },
            runtime,
        )
        .mailbox_capacity(1)
        .build()
        .unwrap();
        (store, weak_reducer, weak_state)
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn ready_rejects_unfinished_causal_task() {
        let (host, runtime, weak_host) = Host::new(false, 128).unwrap();
        let effect_drops = Arc::new(AtomicUsize::new(0));
        let (store, weak_reducer, weak_state) = response_store(runtime, true, &effect_drops);
        let client_drops = Arc::new(AtomicUsize::new(0));
        let guard = DropCount(Arc::clone(&client_drops));
        let mut acquired = None;
        assert_eq!(
            host.run_ready_bounded(
                async {
                    let _guard = guard;
                    acquired = Some(store.send(false).await.unwrap());
                    acquired.as_ref().unwrap().finish().await;
                },
                DEADLINE,
                32,
            ),
            Err("deterministic ready drive quiescent pending")
        );
        assert_eq!(client_drops.load(Ordering::SeqCst), 1);
        let task = acquired.take().unwrap();
        assert!(!task.is_finished());
        assert!(!task.is_cancelled());
        assert_eq!(effect_drops.load(Ordering::SeqCst), 0);
        assert_eq!(
            host.run_ready_bounded(
                store.read(|s| (s.roots, s.responses, s.reductions)),
                DEADLINE,
                32
            )
            .unwrap()
            .unwrap(),
            (1, 0, 1)
        );
        task.cancel();
        host.run_ready_bounded(task.finish(), DEADLINE, 32).unwrap();
        assert!(task.is_finished());
        assert!(task.is_cancelled());
        assert_eq!(effect_drops.load(Ordering::SeqCst), 1);
        drop(task);
        let state = host
            .run_ready_bounded(store.shutdown_into_state(), DEADLINE, 32)
            .unwrap()
            .unwrap();
        assert_eq!((state.roots, state.responses, state.reductions), (1, 0, 1));
        drop(state);
        assert!(weak_reducer.upgrade().is_none());
        assert!(weak_state.upgrade().is_none());
        drop(host);
        assert!(weak_host.upgrade().is_none());
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn ready_rejects_held_join_without_claiming_cleanup() {
        struct HeldJoin {
            sender: async_channel::Sender<()>,
            live: Arc<AtomicUsize>,
        }
        impl Drop for HeldJoin {
            fn drop(&mut self) {
                self.live.fetch_sub(1, Ordering::SeqCst);
                self.sender.try_send(()).unwrap();
            }
        }
        let (host, runtime, weak) = Host::new(false, 32).unwrap();
        let (sender, receiver) = async_channel::bounded::<()>(1);
        let physical_live = Arc::new(AtomicUsize::new(1));
        let physical = HeldJoin {
            sender,
            live: Arc::clone(&physical_live),
        };
        let client_drops = Arc::new(AtomicUsize::new(0));
        let client = DropCount(Arc::clone(&client_drops));
        let joins = AtomicUsize::new(0);
        assert_eq!(
            host.run_ready_bounded(
                async {
                    let _client = client;
                    receiver.recv().await.unwrap();
                    joins.fetch_add(1, Ordering::SeqCst);
                },
                DEADLINE,
                3
            ),
            Err("deterministic ready drive quiescent pending")
        );
        assert_eq!(client_drops.load(Ordering::SeqCst), 1);
        assert_eq!(physical_live.load(Ordering::SeqCst), 1);
        assert_eq!(joins.load(Ordering::SeqCst), 0);
        drop(physical); // Release the owner, complete its join, and fence admission.
        host.run_ready_bounded(
            async {
                receiver.recv().await.unwrap();
                joins.fetch_add(1, Ordering::SeqCst);
            },
            DEADLINE,
            3,
        )
        .unwrap();
        assert_eq!(physical_live.load(Ordering::SeqCst), 0);
        assert_eq!(joins.load(Ordering::SeqCst), 1);
        drop(receiver);
        drop(runtime);
        drop(host);
        assert!(weak.upgrade().is_none());
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn ready_caps_self_waking_client() {
        let (host, runtime, weak) = Host::new(false, 32).unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let guard = DropCount(Arc::clone(&drops));
        let polls = AtomicUsize::new(0);
        assert_eq!(
            host.run_ready_bounded(
                async {
                    let _guard = guard;
                    poll_fn(|cx| {
                        polls.fetch_add(1, Ordering::SeqCst);
                        cx.waker().wake_by_ref();
                        Poll::<()>::Pending
                    })
                    .await;
                },
                DEADLINE,
                3
            ),
            Err("deterministic ready drive outer poll budget exhausted")
        );
        // With no runnable tasks, each outer poll includes all three client
        // checks, including the post-registration readiness check.
        assert_eq!(polls.load(Ordering::SeqCst), 9);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let mut finite_polls = 0;
        assert_eq!(
            host.run_ready_bounded(
                poll_fn(|cx| {
                    finite_polls += 1;
                    if finite_polls == 9 {
                        Poll::Ready(42)
                    } else {
                        let owned = cx.waker().clone();
                        owned.wake();
                        Poll::Pending
                    }
                }),
                DEADLINE,
                3
            ),
            Ok(42)
        );
        assert_eq!(finite_polls, 9);
        drop(runtime);
        drop(host);
        assert!(weak.upgrade().is_none());
    }

    #[cfg(feature = "deterministic")]
    #[test]
    fn ready_preserves_aggregate_runnable_budget() {
        use std::sync::atomic::AtomicBool;
        let (host, runtime, weak) = Host::new(false, 7).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let task_stop = Arc::clone(&stop);
        let polls = Arc::new(AtomicUsize::new(0));
        let task_polls = Arc::clone(&polls);
        let drops = Arc::new(AtomicUsize::new(0));
        let guard = DropCount(Arc::clone(&drops));
        let (sender, receiver) = async_channel::bounded(1);
        let Host::Deterministic(driver) = &host else {
            panic!("deterministic host required")
        };
        driver.spawn_task(Box::pin(async move {
            let guard = guard;
            poll_fn(|cx| {
                task_polls.fetch_add(1, Ordering::SeqCst);
                if task_stop.load(Ordering::SeqCst) {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            drop(guard);
            sender.try_send(()).unwrap();
        }));
        let client_drops = Arc::new(AtomicUsize::new(0));
        let client = DropCount(Arc::clone(&client_drops));
        assert_eq!(
            host.run_ready_bounded(
                async {
                    let _client = client;
                    pending::<()>().await;
                },
                DEADLINE,
                32
            ),
            Err("deterministic ready drive runnable task budget exhausted")
        );
        assert_eq!(polls.load(Ordering::SeqCst), 6); // Parked Host witness consumes S's first poll.
        assert_eq!(client_drops.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        stop.store(true, Ordering::SeqCst);
        host.run_ready_bounded(async { receiver.recv().await.unwrap() }, DEADLINE, 32)
            .unwrap();
        assert_eq!(polls.load(Ordering::SeqCst), 7);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(receiver);
        drop(runtime);
        drop(host);
        assert!(weak.upgrade().is_none());
    }

    fn drive_response(native: bool) {
        let (host, runtime, weak_host) = Host::new(native, 128).unwrap();
        let effect_drops = Arc::new(AtomicUsize::new(0));
        let (store, weak_reducer, weak_state) = response_store(runtime, false, &effect_drops);
        host.run_ready_bounded(
            async {
                let task = store.send(false).await.unwrap();
                task.finish().await;
                assert!(task.is_finished());
                assert!(!task.is_cancelled());
            },
            DEADLINE,
            32,
        )
        .unwrap();
        assert_eq!(
            host.run_ready_bounded(
                store.read(|s| (s.roots, s.responses, s.reductions)),
                DEADLINE,
                32
            )
            .unwrap()
            .unwrap(),
            (1, 1, 2)
        );
        let state = host
            .run_ready_bounded(store.shutdown_into_state(), DEADLINE, 32)
            .unwrap()
            .unwrap();
        assert_eq!((state.roots, state.responses, state.reductions), (1, 1, 2));
        assert_eq!(effect_drops.load(Ordering::SeqCst), 1);
        assert!(weak_reducer.upgrade().is_none());
        drop(state);
        assert!(weak_state.upgrade().is_none());
        drop(host);
        assert!(weak_host.upgrade().is_none());
    }

    #[test]
    fn ready_drives_real_store_request_response() {
        for native in lanes() {
            drive_response(native);
        }
    }

    #[cfg(all(feature = "deterministic", feature = "allocations"))]
    fn retained_waker_control(retain: bool) {
        use crate::measurement::Measurement;
        let measurement = Measurement::new(None);
        let (host, runtime, weak) = Host::new(false, 32).unwrap();
        let mut slot = None;
        let drops = Arc::new(AtomicUsize::new(0));
        let guard = DropCount(Arc::clone(&drops));
        assert_eq!(
            host.run_ready_bounded(
                async {
                    let _guard = guard;
                    poll_fn(|cx| {
                        slot = Some(cx.waker().clone());
                        Poll::<()>::Pending
                    })
                    .await;
                },
                DEADLINE,
                3
            ),
            Err("deterministic ready drive quiescent pending")
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(slot.is_some());
        drop(drops);
        drop(runtime);
        drop(host);
        assert!(weak.upgrade().is_none());
        drop(weak);
        if retain {
            assert!(dhat::HeapStats::get().curr_blocks > 0);
            assert_eq!(
                measurement.finish().err(),
                Some("unexplained allocation retention after teardown")
            );
            slot.take();
        } else {
            slot.take();
            assert_eq!(dhat::HeapStats::get().curr_blocks, 0);
            assert_eq!(dhat::HeapStats::get().curr_bytes, 0);
            let record = measurement.finish().unwrap().unwrap();
            println!("{}", record.json());
        }
    }

    #[cfg(all(feature = "deterministic", feature = "allocations"))]
    #[test]
    fn ready_releases_owned_waker_on_failure() {
        if crate::tests::in_child("host::tests::ready_releases_owned_waker_on_failure") {
            retained_waker_control(false);
        }
    }

    #[cfg(all(feature = "deterministic", feature = "allocations"))]
    #[test]
    fn ready_rejects_retained_waker_on_failure() {
        if crate::tests::in_child("host::tests::ready_rejects_retained_waker_on_failure") {
            retained_waker_control(true);
        }
    }

    #[cfg(all(feature = "deterministic", feature = "allocations"))]
    #[test]
    fn ready_allocation_teardown_is_observed() {
        if !crate::tests::in_child("host::tests::ready_allocation_teardown_is_observed") {
            return;
        }
        let measurement = crate::measurement::Measurement::new(None);
        drive_response(false);
        assert_eq!(dhat::HeapStats::get().curr_blocks, 0);
        assert_eq!(dhat::HeapStats::get().curr_bytes, 0);
        let record = measurement.finish().unwrap().unwrap();
        println!("{}", record.json());
    }

    #[cfg(all(feature = "deterministic", feature = "allocations"))]
    #[test]
    fn ready_rejects_retained_unrelated_owner() {
        if !crate::tests::in_child("host::tests::ready_rejects_retained_unrelated_owner") {
            return;
        }
        let measurement = crate::measurement::Measurement::new(None);
        let owner = Arc::new([0_u8; 19]);
        let weak = Arc::downgrade(&owner);
        drive_response(false);
        assert!(dhat::HeapStats::get().curr_blocks > 0);
        assert_eq!(
            measurement.finish().err(),
            Some("unexplained allocation retention after teardown")
        );
        drop(owner);
        assert!(weak.upgrade().is_none());
    }
}
