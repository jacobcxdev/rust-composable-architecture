#![deny(rustdoc::broken_intra_doc_links)]
#![forbid(unsafe_code)]

//! Adapts an active Tokio executor to RCA's runtime-neutral scheduler.

use std::any::Any;
use std::backtrace::Backtrace;
use std::error::Error;
use std::fmt;
use std::time::Duration;

use rca_core::{MonotonicInstant, Runtime, RuntimeDriver, RuntimeFuture};

/// Bounds each wait below Tokio's roughly two-year timer-wheel horizon.
const MAX_SLEEP_CHUNK: Duration = Duration::from_hours(8_760);

/// Constructs RCA runtimes backed by the active Tokio runtime.
#[derive(Debug)]
pub struct TokioRuntime;

impl TokioRuntime {
    /// Adapts the active Tokio runtime.
    ///
    /// The adapter uses Tokio's clock, including paused test time. It does not
    /// keep the host runtime running after the host is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error when called outside a Tokio runtime or when the active
    /// runtime has no timer driver. Detecting a missing timer driver catches a
    /// Tokio panic, so the process's panic hook runs before the error is returned.
    ///
    /// # Aborts
    ///
    /// A missing timer driver aborts the process in `panic = "abort"` builds.
    /// Enable Tokio's timer driver before calling this method in those builds.
    ///
    /// # Examples
    ///
    /// ```
    /// let host = tokio::runtime::Builder::new_current_thread()
    ///     .enable_time()
    ///     .build()?;
    /// let _entered = host.enter();
    /// let runtime = rca_tokio::TokioRuntime::current()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn current() -> Result<Runtime, TokioRuntimeError> {
        TokioDriver::current().map(Runtime::new)
    }
}

/// Reports why an active Tokio runtime could not be adapted.
#[derive(Debug)]
pub struct TokioRuntimeError {
    /// Distinguishes missing context from missing timers while retaining the cause.
    kind: TokioRuntimeErrorKind,
    /// Records the construction failure when backtrace capture is enabled.
    backtrace: Backtrace,
}

impl TokioRuntimeError {
    /// Preserves Tokio's missing-context error and the adapter's call site.
    fn runtime_missing(source: tokio::runtime::TryCurrentError) -> Self {
        Self {
            kind: TokioRuntimeErrorKind::RuntimeMissing(source),
            backtrace: Backtrace::capture(),
        }
    }

    /// Preserves the caught timer-probe message and the adapter's call site.
    fn timer_missing(message: String) -> Self {
        Self {
            kind: TokioRuntimeErrorKind::TimerMissing(TimerProbeError(message)),
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns whether no Tokio runtime was active.
    #[must_use]
    pub const fn is_runtime_missing(&self) -> bool {
        matches!(self.kind, TokioRuntimeErrorKind::RuntimeMissing(_))
    }

    /// Returns whether the active Tokio runtime had no timer driver.
    #[must_use]
    pub const fn is_timer_missing(&self) -> bool {
        matches!(self.kind, TokioRuntimeErrorKind::TimerMissing(_))
    }

    /// Returns the backtrace captured when adapter construction failed.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Display for TokioRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            TokioRuntimeErrorKind::RuntimeMissing(_) => f.write_str("no Tokio runtime is active"),
            TokioRuntimeErrorKind::TimerMissing(_) => {
                f.write_str("active Tokio runtime has no timer driver")
            }
        }
    }
}

impl Error for TokioRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.kind {
            TokioRuntimeErrorKind::RuntimeMissing(source) => Some(source),
            TokioRuntimeErrorKind::TimerMissing(source) => Some(source),
        }
    }
}

/// Keeps construction failures distinct without exposing Tokio's panic payload.
#[derive(Debug)]
enum TokioRuntimeErrorKind {
    /// No Tokio handle was available in the calling context.
    RuntimeMissing(tokio::runtime::TryCurrentError),
    /// The active host rejected construction of a timer.
    TimerMissing(TimerProbeError),
}

/// Makes a caught timer-probe message available through the error source chain.
#[derive(Debug)]
struct TimerProbeError(String);

impl fmt::Display for TimerProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for TimerProbeError {}

/// Retains a host handle and one immutable clock origin across cloned adapters.
#[derive(Clone, Debug)]
struct TokioDriver {
    /// Selects the host for task submission, clock reads and timer registration.
    handle: tokio::runtime::Handle,
    /// Marks zero elapsed time; every clone copies the same timestamp.
    origin: tokio::time::Instant,
}

impl TokioDriver {
    /// Checks the current host's timer support before capturing its clock origin.
    fn current() -> Result<Self, TokioRuntimeError> {
        let handle =
            tokio::runtime::Handle::try_current().map_err(TokioRuntimeError::runtime_missing)?;
        // This synchronous call already runs in the context checked above.
        // Tokio exposes no stable fallible query for timer-driver availability.
        let timer_probe = std::panic::catch_unwind(|| {
            drop(tokio::time::sleep(Duration::ZERO));
        });
        if let Err(payload) = timer_probe {
            return Err(TokioRuntimeError::timer_missing(panic_message(&*payload)));
        }
        let origin = tokio::time::Instant::now();
        Ok(Self { handle, origin })
    }

    /// Reads the retained host's clock, even when another host is currently active.
    fn elapsed(&self) -> Duration {
        let _entered = self.handle.enter();
        tokio::time::Instant::now().saturating_duration_since(self.origin)
    }
}

impl RuntimeDriver for TokioDriver {
    fn spawn(&self, task: RuntimeFuture) {
        // Dropping the join handle detaches the task; host shutdown still drops it.
        drop(self.handle.spawn(task));
    }

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::from_elapsed(self.elapsed())
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        let driver = self.clone();
        Box::pin(async move {
            loop {
                let remaining = deadline.elapsed().saturating_sub(driver.elapsed());
                if remaining.is_zero() {
                    return;
                }
                let sleep = {
                    let _entered = driver.handle.enter();
                    tokio::time::sleep(remaining.min(MAX_SLEEP_CHUNK))
                };
                sleep.await;
            }
        })
    }

    fn yield_now(&self) -> RuntimeFuture {
        Box::pin(tokio::task::yield_now())
    }
}

/// Copies a diagnostic before the caught panic payload is dropped.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "Tokio timer probe panicked without a message".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::future::poll_fn;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use rca_core::{
        Effect, MonotonicInstant, ReduceContext, Reducer, Runtime, RuntimeDriver, RuntimeFuture,
        Store,
    };
    use rca_test::conformance::{
        ConformanceError, ConformanceFuture, RuntimeFactory, run_all, run_composition,
    };

    use super::{TokioDriver, TokioRuntime};

    #[cfg(all(feature = "qualification-dhat", not(loom), not(miri)))]
    mod allocations {
        use rca_core::{MonotonicInstant, RuntimeDriver};

        use super::{TokioDriver, TokioRuntime};

        mod allocation_test {
            include!("../tests/support/allocation_test.rs");
        }

        #[global_allocator]
        static ALLOCATOR: dhat::Alloc = dhat::Alloc;

        #[test]
        fn driver_origin_needs_no_separate_allocation() {
            if allocation_test::run_in_child(
                "tests::allocations::driver_origin_needs_no_separate_allocation",
            ) {
                return;
            }
            let host = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("build Tokio runtime");
            let _entered = host.enter();
            // Initialise Tokio's thread-local state before measuring the adapter.
            drop(TokioRuntime::current().expect("active Tokio runtime"));

            let profiler = dhat::Profiler::builder().testing().build();
            let before = dhat::HeapStats::get();
            let driver = TokioDriver::current().expect("active Tokio runtime");
            let constructed = dhat::HeapStats::get();
            let clone = driver.clone();
            let cloned = dhat::HeapStats::get();
            let sleep = driver.sleep_until(MonotonicInstant::MAX);
            let sleep_size = size_of_val(sleep.as_ref().get_ref());
            let boxed = dhat::HeapStats::get();
            // Registration and host teardown belong to the conformance tests.
            // This budget covers the adapter's own construction and release.
            drop(sleep);
            drop(clone);
            drop(driver);
            let before_runtime = dhat::HeapStats::get();
            let runtime = TokioRuntime::current().expect("active Tokio runtime");
            let wrapped = dhat::HeapStats::get();
            drop(runtime);
            let released = dhat::HeapStats::get();
            drop(profiler);

            let delta = |before: &dhat::HeapStats, after: &dhat::HeapStats| {
                (
                    after.total_blocks - before.total_blocks,
                    after.total_bytes - before.total_bytes,
                )
            };
            let construction = delta(&before, &constructed);
            let cloning = delta(&constructed, &cloned);
            let boxing = delta(&cloned, &boxed);
            let wrapping = delta(&before_runtime, &wrapped);
            println!(
                "(blocks, bytes): driver={construction:?}, clone={cloning:?}, \
                 sleep={boxing:?}, runtime={wrapping:?}; \
                 driver_size={}, sleep_size={sleep_size}",
                size_of::<TokioDriver>(),
            );
            assert_eq!(construction, (0, 0));
            assert_eq!(cloning, (0, 0));
            assert_eq!(boxing, (1, u64::try_from(sleep_size).unwrap()));
            assert_eq!(wrapping.0, 1);
            assert_eq!((released.curr_blocks, released.curr_bytes), (0, 0));
        }
    }

    // Private diagnostic robustness, not a claim about Tokio's probe payloads.
    #[test]
    fn panic_diagnostics_borrow_payloads_and_return_owned_messages() {
        let messages: [String; 3] = {
            let borrowed = "borrowed timer diagnostic";
            let owned = String::from("owned timer diagnostic");
            let opaque = 17_u32;
            let messages = [
                super::panic_message(&borrowed),
                super::panic_message(&owned),
                super::panic_message(&opaque),
            ];
            assert_eq!(borrowed, "borrowed timer diagnostic");
            assert_eq!(owned, "owned timer diagnostic");
            assert_eq!(opaque, 17);
            messages
        };
        assert_eq!(
            messages,
            [
                "borrowed timer diagnostic",
                "owned timer diagnostic",
                "Tokio timer probe panicked without a message",
            ]
        );
    }

    #[test]
    fn construction_outside_tokio_fails() {
        let error = TokioRuntime::current().expect_err("no active runtime");

        assert!(error.is_runtime_missing());
        assert!(!error.is_timer_missing());
        let source = error.source().expect("retained Tokio construction cause");
        assert!(source.is::<tokio::runtime::TryCurrentError>());
        assert!(!source.to_string().is_empty());
        assert!(!error.to_string().is_empty());
        assert_ne!(error.to_string(), source.to_string());
        assert!(!error.backtrace().to_string().is_empty());
    }

    #[test]
    fn construction_without_timer_fails() {
        let host = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build Tokio runtime");
        let error = host.block_on(async {
            TokioRuntime::current().expect_err("Tokio runtime has no timer driver")
        });

        assert!(!error.is_runtime_missing());
        assert!(error.is_timer_missing());
        let source = error.source().expect("retained timer probe cause");
        let probe = source
            .downcast_ref::<super::TimerProbeError>()
            .expect("timer probe error type");
        assert!(!probe.0.is_empty());
        assert_eq!(source.to_string(), probe.0);
        assert!(!error.to_string().is_empty());
        assert_ne!(error.to_string(), source.to_string());
        assert!(!error.backtrace().to_string().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn spawn_does_not_poll_on_call_stack() {
        let driver = TokioDriver::current().expect("active Tokio runtime");
        let caller = std::thread::current().id();
        let inside = Arc::new(AtomicBool::new(true));
        let observed_inline = Arc::new(AtomicBool::new(false));
        let future_inside = Arc::clone(&inside);
        let future_observed = Arc::clone(&observed_inline);

        RuntimeDriver::spawn(
            &driver,
            Box::pin(poll_fn(move |_| {
                if std::thread::current().id() == caller && future_inside.load(Ordering::Acquire) {
                    future_observed.store(true, Ordering::Release);
                }
                Poll::Ready(())
            })),
        );
        inside.store(false, Ordering::Release);
        tokio::task::yield_now().await;

        assert!(!observed_inline.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cloned_driver_clock_is_shared_and_nondecreasing() {
        let driver = TokioDriver::current().expect("active Tokio runtime");
        let clone = driver.clone();
        let first = RuntimeDriver::now(&driver);
        drop(driver);

        tokio::time::advance(Duration::from_millis(5)).await;
        let second = RuntimeDriver::now(&clone);

        assert!(second >= first);
        assert_eq!(second.elapsed(), Duration::from_millis(5));
    }

    #[test]
    fn clock_and_sleep_use_the_original_host_when_another_is_active() {
        let original = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("build original Tokio runtime");
        let driver = original.block_on(async {
            let driver = TokioDriver::current().expect("active Tokio runtime");
            tokio::time::advance(Duration::from_secs(5)).await;
            driver
        });
        let clone = driver.clone();
        let mut sleep = driver.sleep_until(MonotonicInstant::from_elapsed(Duration::from_secs(10)));
        drop(driver);

        let other = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("build other Tokio runtime");
        let entered = other.enter();
        let other_now = tokio::time::Instant::now();
        assert_eq!(clone.now().elapsed(), Duration::from_secs(5));
        assert!(poll_once(sleep.as_mut()).is_pending());
        assert_eq!(tokio::time::Instant::now(), other_now);
        drop(entered);

        original.block_on(tokio::time::advance(Duration::from_secs(5)));
        let _entered = other.enter();
        assert_eq!(clone.now().elapsed(), Duration::from_secs(10));
        assert!(poll_once(sleep.as_mut()).is_ready());
        assert_eq!(tokio::time::Instant::now(), other_now);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn sleep_observes_deadline_without_completing_early() {
        let driver = TokioDriver::current().expect("active Tokio runtime");
        let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(10));
        let mut sleep = RuntimeDriver::sleep_until(&driver, deadline);

        assert!(poll_once(sleep.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(poll_once(sleep.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        sleep.await;
        RuntimeDriver::sleep_until(&driver, deadline).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn long_sleep_rearms_at_chunk_boundaries_without_completing_early() {
        struct WakeCount(AtomicUsize);
        impl std::task::Wake for WakeCount {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let driver = TokioDriver::current().expect("active Tokio runtime");
        let tail = Duration::from_secs(10);
        let deadline = MonotonicInstant::from_elapsed(super::MAX_SLEEP_CHUNK * 2 + tail);
        let mut sleep = RuntimeDriver::sleep_until(&driver, deadline);
        let notifications = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&notifications));
        assert!(
            sleep
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );

        for _ in 0..2 {
            tokio::time::advance(super::MAX_SLEEP_CHUNK).await;
            tokio::task::yield_now().await;
            assert!(notifications.0.swap(0, Ordering::Relaxed) > 0);
            assert!(
                sleep
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
        }

        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(
            sleep
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(
            sleep
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_ready()
        );
        assert_eq!(RuntimeDriver::now(&driver), deadline);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn yield_returns_pending_before_ready() {
        let driver = TokioDriver::current().expect("active Tokio runtime");
        let mut yielded = RuntimeDriver::yield_now(&driver);

        assert!(poll_once(yielded.as_mut()).is_pending());
        assert!(poll_once(yielded.as_mut()).is_ready());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_sleep_and_yield_is_harmless() {
        let driver = TokioDriver::current().expect("active Tokio runtime");

        drop(RuntimeDriver::sleep_until(&driver, MonotonicInstant::MAX));
        drop(RuntimeDriver::yield_now(&driver));
        tokio::task::yield_now().await;
    }

    #[test]
    fn host_teardown_drops_accepted_future_once() {
        let host = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build Tokio runtime");
        let driver = host.block_on(async { TokioDriver::current().expect("active Tokio runtime") });
        let drops = Arc::new(AtomicUsize::new(0));

        RuntimeDriver::spawn(
            &driver,
            Box::pin(DropProbe {
                drops: Arc::clone(&drops),
            }),
        );
        host.block_on(tokio::task::yield_now());
        assert_eq!(drops.load(Ordering::Acquire), 0);
        drop(driver);
        drop(host);

        assert_eq!(drops.load(Ordering::Acquire), 1);
    }

    #[test]
    fn host_teardown_closes_store() {
        let host = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build Tokio runtime");
        let (store, mut closed) = host.block_on(async {
            let runtime = TokioRuntime::current().expect("active Tokio runtime");
            let store = Store::new(0, Counter, runtime);
            let closed = Box::pin(store.closed());
            (store, closed)
        });

        drop(host);

        assert!(store.is_closed());
        assert!(poll_once(closed.as_mut()).is_ready());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn conformance_current_thread() {
        run_all(TokioFactory::current(true)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn conformance_multi_thread() {
        run_all(TokioFactory::current(false)).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn composition_current_thread() {
        run_composition(TokioFactory::current(false)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn composition_multi_thread() {
        run_composition(TokioFactory::current(false)).await;
    }

    fn poll_once(future: Pin<&mut (dyn Future<Output = ()> + Send)>) -> Poll<()> {
        let mut context = Context::from_waker(Waker::noop());
        future.poll(&mut context)
    }

    struct DropProbe {
        drops: Arc<AtomicUsize>,
    }

    impl Future for DropProbe {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct Counter;

    impl Reducer for Counter {
        type State = usize;
        type Action = ();

        fn reduce(
            &mut self,
            state: &mut Self::State,
            _action: &Self::Action,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            *state += 1;
            Effect::none()
        }
    }

    #[derive(Clone)]
    struct TokioFactory {
        driver: TokioDriver,
        paused: bool,
    }

    impl TokioFactory {
        fn current(paused: bool) -> Self {
            Self {
                driver: TokioDriver::current().expect("active Tokio runtime"),
                paused,
            }
        }
    }

    impl RuntimeFactory for TokioFactory {
        fn runtime(&self) -> Runtime {
            Runtime::new(self.driver.clone())
        }

        fn spawn_probe(&self, future: RuntimeFuture) {
            RuntimeDriver::spawn(&self.driver, future);
        }

        fn now_probe(&self) -> MonotonicInstant {
            RuntimeDriver::now(&self.driver)
        }

        fn sleep_probe(&self, deadline: MonotonicInstant) -> RuntimeFuture {
            RuntimeDriver::sleep_until(&self.driver, deadline)
        }

        fn yield_probe(&self) -> RuntimeFuture {
            RuntimeDriver::yield_now(&self.driver)
        }

        fn advance(&self, duration: Duration) -> ConformanceFuture<Result<(), ConformanceError>> {
            let paused = self.paused;
            Box::pin(async move {
                if paused {
                    tokio::time::advance(duration).await;
                } else {
                    tokio::time::sleep(duration).await;
                }
                Ok(())
            })
        }

        fn teardown_probe(&self) -> ConformanceFuture<()> {
            let multi = !self.paused;
            Box::pin(async move {
                std::thread::spawn(move || {
                    let host = if multi {
                        tokio::runtime::Builder::new_multi_thread()
                            .worker_threads(4)
                            .enable_time()
                            .build()
                            .expect("build multi-thread teardown runtime")
                    } else {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_time()
                            .build()
                            .expect("build current-thread teardown runtime")
                    };
                    let driver = host
                        .block_on(async { TokioDriver::current().expect("active Tokio runtime") });
                    let drops = Arc::new(AtomicUsize::new(0));
                    let store = Store::new(0, Counter, Runtime::new(driver.clone()));
                    let mut closed = Box::pin(store.closed());
                    RuntimeDriver::spawn(
                        &driver,
                        Box::pin(DropProbe {
                            drops: Arc::clone(&drops),
                        }),
                    );
                    host.block_on(tokio::task::yield_now());
                    drop(driver);
                    drop(host);
                    assert_eq!(drops.load(Ordering::Acquire), 1);
                    assert!(store.is_closed());
                    assert!(poll_once(closed.as_mut()).is_ready());
                })
                .join()
                .expect("teardown probe thread");
            })
        }
    }
}
