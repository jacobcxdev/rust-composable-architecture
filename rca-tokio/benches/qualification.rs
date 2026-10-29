//! Measures fixed Store workloads with explicit timing and allocation boundaries.
//!
//! Native rows exclude host construction and up to 20 warm-up samples. The timed
//! operation includes its producer tasks, joins, payload allocation, and any
//! Store construction or shutdown inside the closure. Latency describes an outer
//! sample. Throughput counts the declared inner operations. Final state checks
//! sit outside those intervals, except for Stores destroyed within a sample.
//! Observation handoff and pending-work assertions are measured. Deterministic
//! rows include runtime construction and teardown, but exclude warm-up samples.
//!
//! Stable scenario names retain historical meanings: `saturated` means a
//! capacity-one mailbox, `small_state` still owns a 64 KiB backing buffer, and
//! `timer/wake` processes an expired deadline. None proves native timer latency.
//! `task_wakes` counts polls, not wake calls. Native rows use an instrumented
//! driver, not the production Tokio adapter. Their scope includes the enclosing
//! workload future and RCA futures, but excludes direct producer tasks.
//! Native poll deltas bracket the sample loop with cumulative snapshots; polls
//! crossing a boundary follow the monitor's accounting, not per-operation attribution.
//! Deterministic timer rows count only the final advance, not registration polls.
//! Neither smoke results nor these unlike poll scopes establish relative speed.

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rca_core::{
    Effect, MonotonicInstant, ReduceContext, Reducer, Runtime, RuntimeDriver, RuntimeFuture, Store,
};
use rca_test::{DEFAULT_STEP_BUDGET, DeterministicRuntime};
use tokio_metrics::TaskMonitor;

#[cfg(feature = "qualification-dhat")]
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

#[cfg(feature = "qualification-dhat")]
/// Owns the optional process-wide allocation recording interval.
type HeapProfiler = dhat::Profiler;

#[cfg(not(feature = "qualification-dhat"))]
#[derive(Debug)]
/// Keeps the measurement interface unchanged when allocation recording is disabled.
struct HeapProfiler;

#[derive(Debug, Default)]
/// Holds allocation totals from the measured interval, or zeros without DHAT.
struct HeapMeasurement {
    /// Counts allocation blocks recorded after profiler startup, not currently live blocks.
    allocations: u64,
    /// Sums requested bytes across those blocks, including bytes later freed.
    allocated_bytes: u64,
    /// Reports the largest recorded live allocation total, not process resident memory.
    peak_bytes: u64,
}

/// Fixes payload and backing-state size for comparable workload definitions.
const LARGE_BYTES: usize = 64 * 1_024;
/// Bounds untimed warm-up while preserving the same operation and cumulative state.
const WARM_UP_LIMIT: usize = 20;
// Matches the production adapter's bounded native sleep chunks.
const MAX_SLEEP_CHUNK: Duration = Duration::from_hours(8_760);

/// Defines the accepted CLI labels used by the external qualification scripts.
///
/// Labels remain stable even where the module documents narrower workload meanings.
const SCENARIOS: &[&str] = &[
    "external_commit/current/1_producer/empty",
    "external_commit/multi/1_producer/empty",
    "external_commit/multi/2_producers/empty",
    "external_commit/multi/4_producers/empty",
    "external_commit/multi/8_producers/empty",
    "external_commit/multi/1_producer/saturated",
    "external_commit/multi/2_producers/saturated",
    "external_commit/multi/4_producers/saturated",
    "external_commit/multi/8_producers/saturated",
    "mailbox/multi/half",
    "mailbox/multi/full",
    "independent_stores/multi/1",
    "independent_stores/multi/2",
    "independent_stores/multi/4",
    "independent_stores/multi/8",
    "payload/small_action_small_state",
    "payload/large_action",
    "payload/large_state",
    "effect/immediate",
    "effect/async",
    "observation/fanout/1",
    "observation/fanout/8",
    "observation/fanout/64",
    "observation/slow",
    "observation/handoff",
    "observation/cancel",
    "cancellation/insertion/1",
    "cancellation/insertion/16",
    "cancellation/insertion/64",
    "cancellation/replacement/16",
    "cancellation/churn/64",
    "action_task/fanout/1",
    "action_task/fanout/16",
    "action_task/fanout/64",
    "action_task/finish",
    "action_task/cancel",
    "timer/insert/1",
    "timer/insert/100",
    "timer/insert/1000",
    "timer/wake",
    "timer/remove",
    "query/multi",
    "shutdown/graceful",
    "shutdown/abortive",
    "deterministic/ready/1",
    "deterministic/ready/16",
    "deterministic/ready/256",
    "deterministic/timer/1",
    "deterministic/timer/1000",
    "deterministic/budget",
];

fn main() {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.as_slice() == ["--list"] {
        for scenario in SCENARIOS {
            println!("{scenario}");
        }
        return;
    }
    if arguments.as_slice() == ["--idle"] {
        idle_store();
        return;
    }
    if arguments.len() != 2 {
        eprintln!("usage: qualification <scenario> <samples>");
        std::process::exit(2);
    }

    let scenario = arguments[0].as_str();
    if !SCENARIOS.contains(&scenario) {
        eprintln!("unknown qualification scenario: {scenario}");
        std::process::exit(2);
    }
    let samples = arguments[1]
        .parse::<usize>()
        .ok()
        .filter(|samples| *samples > 0)
        .unwrap_or_else(|| {
            eprintln!("samples must be a positive integer");
            std::process::exit(2);
        });

    let measurement = run(scenario, samples);
    measurement.print(scenario);
}

/// Dispatches a listed scenario with a positive measured sample count.
///
/// CLI validation establishes both preconditions before this private dispatch.
fn run(scenario: &str, samples: usize) -> Measurement {
    if let Some(rest) = scenario.strip_prefix("external_commit/") {
        return external_commit(rest, samples);
    }
    if let Some(fill) = scenario.strip_prefix("mailbox/multi/") {
        return mailbox(fill, samples);
    }
    if let Some(count) = scenario.strip_prefix("independent_stores/multi/") {
        return independent_stores(parse_count(count), samples);
    }
    if let Some(kind) = scenario.strip_prefix("payload/") {
        return payload(kind, samples);
    }
    if let Some(kind) = scenario.strip_prefix("effect/") {
        return effect(kind, samples);
    }
    if let Some(kind) = scenario.strip_prefix("observation/") {
        return observation(kind, samples);
    }
    if let Some(kind) = scenario.strip_prefix("cancellation/") {
        return cancellation(kind, samples);
    }
    if let Some(kind) = scenario.strip_prefix("action_task/") {
        return action_task(kind, samples);
    }
    if let Some(kind) = scenario.strip_prefix("timer/") {
        return timer(kind, samples);
    }
    if scenario == "query/multi" {
        return query(samples);
    }
    if let Some(kind) = scenario.strip_prefix("shutdown/") {
        return shutdown(kind, samples);
    }
    if let Some(kind) = scenario.strip_prefix("deterministic/") {
        return deterministic(kind, samples);
    }
    unreachable!("scenario was validated")
}

/// Stores the results for one scenario's measured loop.
struct Measurement {
    /// Counts outer measured invocations, not each action or notification within them.
    samples: usize,
    /// Counts declared workload units across samples, such as actions or notifications.
    operations: usize,
    /// Divides declared operations by elapsed time for the complete measured loop.
    throughput_per_second: f64,
    /// Reports outer-sample latency in nanoseconds at the upper-indexed 50th percentile.
    p50: u64,
    /// Reports outer-sample latency in nanoseconds at the upper-indexed 95th percentile.
    p95: u64,
    /// Reports outer-sample latency in nanoseconds at the upper-indexed 99th percentile.
    p99: u64,
    /// Copies allocation blocks from `HeapMeasurement`, or zero without recording.
    allocations: u64,
    /// Copies cumulative requested bytes, not bytes retained after completion.
    allocated_bytes: u64,
    /// Retains the historical field name for poll counts with scenario-specific scope.
    task_wakes: u64,
    /// Copies the recorded peak live bytes, excluding allocations before profiler startup.
    peak_bytes: u64,
}

impl Measurement {
    /// Emits the existing JSON schema for a validated scenario label.
    ///
    /// The fixed labels contain no characters that require JSON escaping.
    fn print(&self, scenario: &str) {
        println!(
            concat!(
                "{{\"scenario\":\"{}\",\"samples\":{},\"operations\":{},",
                "\"throughput_per_second\":{:.3},",
                "\"latency_ns\":{{\"p50\":{},\"p95\":{},\"p99\":{}}},",
                "\"allocations\":{},\"allocated_bytes\":{},",
                "\"task_wakes\":{},\"peak_bytes\":{}}}"
            ),
            scenario,
            self.samples,
            self.operations,
            self.throughput_per_second,
            self.p50,
            self.p95,
            self.p99,
            self.allocations,
            self.allocated_bytes,
            self.task_wakes,
            self.peak_bytes,
        );
    }
}

/// Measures completed async operations after their identical warm-up invocations.
///
/// `operations_per_sample` declares throughput units, not additional repetitions.
/// The closure owns its completion boundary. Poll snapshots exclude sorting and final validation.
async fn measure_async<F>(
    monitor: &TaskMonitor,
    samples: usize,
    operations_per_sample: usize,
    mut operation: F,
) -> Measurement
where
    F: AsyncFnMut(),
{
    for _ in 0..samples.min(WARM_UP_LIMIT) {
        operation().await;
    }

    let mut latencies = Vec::with_capacity(samples);
    let profiler = start_heap_profiler();
    let polls_before = monitor.cumulative().total_poll_count;
    let total_start = Instant::now();
    for _ in 0..samples {
        let start = Instant::now();
        operation().await;
        latencies.push(duration_nanos(start.elapsed()));
    }
    let elapsed = total_start.elapsed();
    let polls = monitor
        .cumulative()
        .total_poll_count
        .saturating_sub(polls_before);
    finish_measurement(
        samples,
        operations_per_sample,
        latencies,
        elapsed,
        polls,
        profiler,
    )
}

/// Measures synchronous operations and sums their reported scheduler polls.
///
/// Each closure defines its poll scope. Timer rows return only advance polls.
fn measure_sync<F>(samples: usize, operations_per_sample: usize, mut operation: F) -> Measurement
where
    F: FnMut() -> u64,
{
    for _ in 0..samples.min(WARM_UP_LIMIT) {
        let _ = operation();
    }

    let mut latencies = Vec::with_capacity(samples);
    let profiler = start_heap_profiler();
    let mut task_wakes = 0_u64;
    let total_start = Instant::now();
    for _ in 0..samples {
        let start = Instant::now();
        task_wakes = task_wakes.saturating_add(operation());
        latencies.push(duration_nanos(start.elapsed()));
    }
    let elapsed = total_start.elapsed();
    finish_measurement(
        samples,
        operations_per_sample,
        latencies,
        elapsed,
        task_wakes,
        profiler,
    )
}

/// Stops allocation recording before sorting latencies and calculating summary metrics.
///
/// Callers provide nonempty latencies and an operation total that fits in `u32`.
fn finish_measurement(
    samples: usize,
    operations_per_sample: usize,
    mut latencies: Vec<u64>,
    elapsed: Duration,
    task_wakes: u64,
    profiler: Option<HeapProfiler>,
) -> Measurement {
    let heap = finish_heap_profiling(profiler);
    latencies.sort_unstable();
    let operations = samples.saturating_mul(operations_per_sample);
    let throughput_per_second =
        f64::from(u32::try_from(operations).expect("qualification operation count fits in u32"))
            / elapsed.as_secs_f64();
    Measurement {
        samples,
        operations,
        throughput_per_second,
        p50: percentile(&latencies, 50),
        p95: percentile(&latencies, 95),
        p99: percentile(&latencies, 99),
        allocations: heap.allocations,
        allocated_bytes: heap.allocated_bytes,
        task_wakes,
        peak_bytes: heap.peak_bytes,
    }
}

#[cfg(feature = "qualification-dhat")]
/// Starts DHAT only when `RCA_QUALIFY_ALLOCATIONS` requests allocation recording.
///
/// `RCA_DHAT_FILE` selects the optional report path. Host setup and warm-up precede this call.
fn start_heap_profiler() -> Option<HeapProfiler> {
    std::env::var_os("RCA_QUALIFY_ALLOCATIONS")?;
    let builder = dhat::Profiler::builder();
    let builder = match std::env::var_os("RCA_DHAT_FILE") {
        Some(path) => builder.file_name(path),
        None => builder,
    };
    Some(builder.build())
}

#[cfg(not(feature = "qualification-dhat"))]
/// Rejects allocation requests when the binary lacks the DHAT feature.
fn start_heap_profiler() -> Option<HeapProfiler> {
    assert!(
        std::env::var_os("RCA_QUALIFY_ALLOCATIONS").is_none(),
        "allocation qualification requires --features qualification-dhat"
    );
    None
}

#[cfg(feature = "qualification-dhat")]
/// Captures heap totals before dropping the profiler and writing its report.
fn finish_heap_profiling(profiler: Option<HeapProfiler>) -> HeapMeasurement {
    let heap = profiler.as_ref().map(|_| dhat::HeapStats::get());
    drop(profiler);
    heap.map_or_else(HeapMeasurement::default, |stats| HeapMeasurement {
        allocations: stats.total_blocks,
        allocated_bytes: stats.total_bytes,
        peak_bytes: stats.max_bytes as u64,
    })
}

#[cfg(not(feature = "qualification-dhat"))]
/// Supplies explicit zero allocation fields for non-recording runs.
fn finish_heap_profiling(_profiler: Option<HeapProfiler>) -> HeapMeasurement {
    HeapMeasurement::default()
}

/// Selects the historical upper-indexed percentile from nonempty sorted latencies.
///
/// The index is `ceil((len - 1) * percent / 100)`, not nearest rank.
/// Callers supply 50, 95, or 99. With two samples, p50 selects the larger sample.
fn percentile(sorted: &[u64], percent: usize) -> u64 {
    let index = sorted
        .len()
        .saturating_sub(1)
        .saturating_mul(percent)
        .div_ceil(100);
    sorted[index]
}

/// Converts elapsed time to JSON-compatible nanoseconds, saturating unrepresentable durations.
fn duration_nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// Parses a numeric component from an already validated scenario label.
fn parse_count(value: &str) -> usize {
    value.parse().expect("validated scenario count")
}

/// Includes untimed mutations when calculating the final-state oracle.
fn iterations_with_warm_up(samples: usize) -> usize {
    samples
        .checked_add(samples.min(WARM_UP_LIMIT))
        .expect("sample count plus warm-up fits in usize")
}

/// Checks committed outputs through an ordered Store query.
///
/// Most callers run this after measurement. Per-sample Store lifecycles include its cost.
async fn assert_count(store: &Store<BenchState, Action>, expected: usize) {
    assert_eq!(
        store
            .read(|state| state.count)
            .await
            .expect("final Store state"),
        expected,
        "completed work must produce the exact state count"
    );
}

/// Owns native execution and the shared instrumentation for one qualification row.
///
/// Each workload closes its Stores before this host drops. Driver handles do not own the runtime.
struct QualificationHost {
    /// Owns the worker threads and timer driver used by the workload.
    host: tokio::runtime::Runtime,
    /// Shares a clock origin and poll monitor with all Stores in this row.
    driver: QualificationDriver,
}

impl QualificationHost {
    fn current() -> Self {
        Self::new(false)
    }

    fn multi() -> Self {
        Self::new(true)
    }

    /// Selects current-thread execution or four Tokio workers outside measurement.
    fn new(multi: bool) -> Self {
        let host = if multi {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .expect("multi-thread qualification runtime")
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("current-thread qualification runtime")
        };
        let handle = host.handle().clone();
        let origin = {
            let _entered = handle.enter();
            Arc::new(tokio::time::Instant::now())
        };
        Self {
            host,
            driver: QualificationDriver {
                handle,
                origin,
                monitor: TaskMonitor::new(),
            },
        }
    }

    /// Shares the instrumented driver without creating another Tokio host.
    fn runtime(&self) -> Runtime {
        Runtime::new(self.driver.clone())
    }

    /// Creates a bounded Store with the fixed backing-state allocation.
    ///
    /// Lifecycle workloads call this inside measurement. Other rows call it before warm-up.
    fn store(&self, capacity: usize) -> Store<BenchState, Action> {
        Store::builder(BenchState::new(), BenchReducer, self.runtime())
            .mailbox_capacity(capacity)
            .build()
            .expect("valid qualification Store")
    }

    /// Instruments the outer workload future as well as the driver-spawned futures.
    fn measure<F>(&self, future: F) -> Measurement
    where
        F: Future<Output = Measurement>,
    {
        self.host.block_on(self.driver.monitor.instrument(future))
    }

    /// Runs setup or validation without instrumenting its enclosing future.
    ///
    /// Store work that it triggers still uses the instrumented driver.
    fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.host.block_on(future)
    }
}

#[derive(Clone)]
/// Adapts Tokio with poll instrumentation for qualification, not production use.
struct QualificationDriver {
    /// Schedules onto the owning host without extending the host's lifetime.
    handle: tokio::runtime::Handle,
    /// Keeps one monotonic origin across driver clones and timer futures.
    origin: Arc<tokio::time::Instant>,
    /// Aggregates RCA futures and the outer workload, but not direct producer tasks.
    monitor: TaskMonitor,
}

impl QualificationDriver {
    /// Reads elapsed time within the timer context of the owning host.
    fn elapsed(&self) -> Duration {
        let _entered = self.handle.enter();
        tokio::time::Instant::now().saturating_duration_since(*self.origin)
    }
}

impl fmt::Debug for QualificationDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QualificationDriver")
            .finish_non_exhaustive()
    }
}

impl RuntimeDriver for QualificationDriver {
    fn spawn(&self, task: RuntimeFuture) {
        drop(self.handle.spawn(self.monitor.instrument(task)));
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

/// Retains fixed state storage across samples, including rows named `small_state`.
struct BenchState {
    /// Counts committed outputs or the first byte of each large payload.
    count: usize,
    /// Owns 64 KiB in every row. Only `LargeState` mutates the complete buffer.
    large: Box<[u8]>,
}

impl BenchState {
    /// Allocates the fixed backing buffer and starts the output count at zero.
    fn new() -> Self {
        Self {
            count: 0,
            large: vec![0; LARGE_BYTES].into_boxed_slice(),
        }
    }
}

/// Selects private workload commands whose effects are explicit in `BenchReducer`.
enum Action {
    Commit,
    LargePayload(Box<[u8]>),
    LargeState,
    Immediate,
    Async,
    Output,
    Pending(usize),
    Replacement(usize),
    Cancel,
    TaskFanout(usize),
    TaskPending,
    TreeStart,
    TreeChild,
    FarTimers(usize),
    TimerWake,
    Block(Arc<BlockingGate>),
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
/// Gives each Store one shared cancellation group for insertion and replacement workloads.
enum CancellationKey {
    Work,
}

/// Applies qualification commands and returns their measured effects.
struct BenchReducer;

impl Reducer for BenchReducer {
    type State = BenchState;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Commit | Action::Output => {
                state.count = state.count.wrapping_add(1);
                Effect::none()
            }
            Action::LargePayload(payload) => {
                state.count = state.count.wrapping_add(usize::from(payload[0]));
                Effect::none()
            }
            Action::LargeState => {
                for byte in state.large.as_mut() {
                    *byte = byte.wrapping_add(1);
                }
                Effect::none()
            }
            Action::Immediate | Action::TreeChild => Effect::send(Action::Output),
            Action::Async => Effect::run(|sender| async move {
                sender.send(Action::Output).await.expect("effect output");
            }),
            Action::Pending(count) => Effect::merge((0..*count).map(|_| {
                Effect::run(|_sender| std::future::pending()).cancellable(CancellationKey::Work)
            })),
            Action::Replacement(count) => {
                Effect::merge((0..*count).map(|_| Effect::run(|_sender| std::future::pending())))
                    .cancellable_in_flight(CancellationKey::Work)
            }
            Action::Cancel => Effect::cancel(CancellationKey::Work),
            Action::TaskFanout(count) => {
                Effect::merge((0..*count).map(|_| Effect::run(|_sender| async {})))
            }
            Action::TaskPending => Effect::run(|_sender| std::future::pending()),
            Action::TreeStart => Effect::send(Action::TreeChild),
            Action::FarTimers(count) => Effect::merge((0..*count).map(|_| {
                Effect::send_after(Duration::MAX, Action::Output).cancellable(CancellationKey::Work)
            })),
            Action::TimerWake => Effect::send_after(Duration::ZERO, Action::Output),
            Action::Block(gate) => {
                gate.enter();
                Effect::none()
            }
        }
    }
}

/// Blocks a reducer until the benchmark has established mailbox occupancy.
#[derive(Default)]
struct BlockingGate {
    /// Keeps entry acknowledgement and release under one mutex.
    state: Mutex<GateState>,
    /// Wakes both the benchmark driver and blocked actor when state changes.
    changed: Condvar,
}

/// One-shot handshake state for a single blocked actor.
#[derive(Default)]
struct GateState {
    /// Acknowledges that the actor entered its blocking reducer.
    entered: bool,
    /// Permits that reducer to return, including during fixture unwinding.
    open: bool,
}

/// Opens the actor gate before a failed fixture can drop its Tokio host.
struct OpenGateOnDrop<'a>(&'a BlockingGate);

impl Drop for OpenGateOnDrop<'_> {
    fn drop(&mut self) {
        self.0.open();
    }
}

impl BlockingGate {
    /// Acknowledges reducer entry, then waits for explicit or unwind release.
    fn enter(&self) {
        let mut state = self.state.lock().expect("gate lock");
        state.entered = true;
        self.changed.notify_all();
        while !state.open {
            state = self.changed.wait(state).expect("gate wait");
        }
    }

    /// Waits until occupancy can be changed without racing actor consumption.
    fn wait_until_entered(&self) {
        let mut state = self.state.lock().expect("gate lock");
        while !state.entered {
            state = self.changed.wait(state).expect("gate wait");
        }
    }

    /// Releases the blocked actor; repeated release is harmless.
    fn open(&self) {
        let mut state = self.state.lock().expect("gate lock");
        state.open = true;
        self.changed.notify_all();
    }
}

/// Waits for action commitment and all causal work, including cancellation completion.
///
/// Receipt completion alone does not prove that the reducer produced an output.
async fn send_and_finish(store: &Store<BenchState, Action>, action: Action) {
    store
        .send(action)
        .await
        .expect("qualification commit")
        .finish()
        .await;
}

/// Spawns concurrent producers that each complete their own burst serially.
///
/// Producer task allocation and joins belong to the caller's measured operation.
async fn send_group(store: &Store<BenchState, Action>, producers: usize, burst: usize) {
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..producers {
        let store = store.clone();
        tasks.spawn(async move {
            for _ in 0..burst {
                send_and_finish(&store, Action::Commit).await;
            }
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.expect("producer task");
    }
}

/// Measures producer contention with fixed mailbox capacity and per-producer burst size.
///
/// Capacity one does not guarantee admission backpressure, especially with one serial producer.
fn external_commit(configuration: &str, samples: usize) -> Measurement {
    let mut parts = configuration.split('/');
    let runtime = parts.next().expect("runtime");
    let producers = parse_count(
        parts
            .next()
            .expect("producer count")
            .split('_')
            .next()
            .expect("producer number"),
    );
    let load = parts.next().expect("load");
    let host = if runtime == "current" {
        QualificationHost::current()
    } else {
        QualificationHost::multi()
    };
    let capacity = if load == "saturated" { 1 } else { 64 };
    let burst = if load == "saturated" { 8 } else { 1 };
    let store = host.store(capacity);
    let measurement = host.measure(measure_async(
        &host.driver.monitor,
        samples,
        producers * burst,
        async || {
            send_group(&store, producers, burst).await;
        },
    ));
    host.block_on(assert_count(
        &store,
        iterations_with_warm_up(samples) * producers * burst,
    ));
    host.block_on(store.shutdown()).expect("clean shutdown");
    measurement
}

fn mailbox(fill: &str, samples: usize) -> Measurement {
    let host = QualificationHost::multi();
    let (capacity, queued) = if fill == "half" { (64, 32) } else { (32, 32) };
    host.measure(measure_async(
        &host.driver.monitor,
        samples,
        queued + 2,
        async || {
            let store = host.store(capacity);
            let gate = Arc::new(BlockingGate::default());
            let release = OpenGateOnDrop(&gate);
            let blocked = store
                .try_send(Action::Block(Arc::clone(&gate)))
                .expect("blocking action admission");
            gate.wait_until_entered();
            let mut receipts = Vec::with_capacity(queued);
            for _ in 0..queued {
                receipts.push(store.try_send(Action::Commit).expect("prefill admission"));
            }
            if fill == "full" {
                assert!(
                    matches!(store.try_send(Action::Commit), Err(error) if error.is_full()),
                    "the blocked actor must leave the prefilled mailbox full"
                );
            }
            let extra_store = store.clone();
            let (first_poll, polled) = tokio::sync::oneshot::channel();
            let extra = tokio::spawn(async move {
                let mut send = std::pin::pin!(extra_store.send(Action::Commit));
                std::future::poll_fn(|context| {
                    assert!(
                        send.as_mut().poll(context).is_pending(),
                        "the blocked actor cannot commit the extra action"
                    );
                    std::task::Poll::Ready(())
                })
                .await;
                first_poll.send(()).expect("first-poll acknowledgement");
                send.await.expect("extra sender commit").finish().await;
            });
            // The measured handshake proves an attempted send while the gate is
            // closed: Full admission waits here; half-full admission waits for commit.
            polled.await.expect("extra sender first poll");
            drop(release);
            blocked
                .finish()
                .await
                .expect("blocking action commit")
                .finish()
                .await;
            for receipt in receipts {
                receipt
                    .finish()
                    .await
                    .expect("queued action commit")
                    .finish()
                    .await;
            }
            extra.await.expect("extra sender");
            // This lifecycle row includes its final state check before destruction.
            assert_count(&store, queued + 1).await;
            store.shutdown().await.expect("clean shutdown");
        },
    ))
}

/// Commits once per Store per sample through separate producer tasks.
///
/// Stores share one host, clock origin, and poll monitor. Their state remains independent.
fn independent_stores(count: usize, samples: usize) -> Measurement {
    let host = QualificationHost::multi();
    let stores: Vec<_> = (0..count).map(|_| host.store(64)).collect();
    let measurement = host.measure(measure_async(
        &host.driver.monitor,
        samples,
        count,
        async || {
            let mut tasks = tokio::task::JoinSet::new();
            for store in &stores {
                let store = store.clone();
                tasks.spawn(async move { send_and_finish(&store, Action::Commit).await });
            }
            while let Some(result) = tasks.join_next().await {
                result.expect("independent Store task");
            }
        },
    ));
    host.block_on(async {
        for store in stores {
            assert_count(&store, iterations_with_warm_up(samples)).await;
            store.shutdown().await.expect("clean shutdown");
        }
    });
    measurement
}

/// Measures payload allocation or full-buffer mutation with fixed backing-state size.
///
/// Large actions allocate 64 KiB but the reducer reads only the first byte.
fn payload(kind: &str, samples: usize) -> Measurement {
    let host = QualificationHost::multi();
    let store = host.store(64);
    let measurement = host.measure(measure_async(
        &host.driver.monitor,
        samples,
        1,
        async || {
            let action = match kind {
                "small_action_small_state" => Action::Commit,
                "large_action" => Action::LargePayload(vec![1; LARGE_BYTES].into_boxed_slice()),
                "large_state" => Action::LargeState,
                _ => unreachable!("validated payload scenario"),
            };
            send_and_finish(&store, action).await;
        },
    ));
    host.block_on(async {
        let iterations = iterations_with_warm_up(samples);
        if kind == "large_state" {
            let expected = u8::try_from(iterations % 256).expect("byte remainder");
            assert!(
                store
                    .read(move |state| {
                        state.count == 0
                            && state.large.len() == LARGE_BYTES
                            && state.large.iter().all(|byte| *byte == expected)
                    })
                    .await
                    .expect("large-state result")
            );
        } else {
            assert_count(&store, iterations).await;
        }
    });
    host.block_on(store.shutdown()).expect("clean shutdown");
    measurement
}

/// Compares immediate and async output dispatch through the same completion boundary.
fn effect(kind: &str, samples: usize) -> Measurement {
    let host = QualificationHost::multi();
    let store = host.store(64);
    let measurement = host.measure(measure_async(
        &host.driver.monitor,
        samples,
        1,
        async || {
            send_and_finish(
                &store,
                if kind == "immediate" {
                    Action::Immediate
                } else {
                    Action::Async
                },
            )
            .await;
        },
    ));
    host.block_on(assert_count(&store, iterations_with_warm_up(samples)));
    host.block_on(store.shutdown()).expect("clean shutdown");
    measurement
}

/// Measures publication, coalescing, handoff, or subscription removal for one selected counter.
///
/// Fanout throughput counts deliveries. Other rows count committed updates.
fn observation(kind: &str, samples: usize) -> Measurement {
    let host = QualificationHost::multi();
    let store = host.store(64);
    let measurement = if let Some(count) = kind.strip_prefix("fanout/") {
        let count = parse_count(count);
        let mut observations = host.block_on(async {
            let mut observations = Vec::with_capacity(count);
            for _ in 0..count {
                let mut observation = store.observe(|state| state.count).await.expect("observe");
                assert_eq!(observation.next().await, Some(0));
                observations.push(observation);
            }
            observations
        });
        let mut expected = 0;
        host.measure(measure_async(
            &host.driver.monitor,
            samples,
            count.max(1),
            async || {
                send_and_finish(&store, Action::Commit).await;
                expected += 1;
                for observation in &mut observations {
                    // Each measured handoff checks the selected value, not only presence.
                    assert_eq!(observation.next().await, Some(expected));
                }
            },
        ))
    } else if kind == "slow" {
        let mut observation = host
            .block_on(store.observe(|state| state.count))
            .expect("observe");
        assert_eq!(host.block_on(observation.next()), Some(0));
        let measurement = host.measure(measure_async(
            &host.driver.monitor,
            samples,
            1,
            async || {
                send_and_finish(&store, Action::Commit).await;
            },
        ));
        assert_eq!(
            host.block_on(observation.next()),
            Some(iterations_with_warm_up(samples))
        );
        measurement
    } else if kind == "handoff" {
        let mut observation = host
            .block_on(store.observe(|state| state.count))
            .expect("observe");
        assert_eq!(host.block_on(observation.next()), Some(0));
        let mut expected = 0;
        host.measure(measure_async(
            &host.driver.monitor,
            samples,
            1,
            async || {
                send_and_finish(&store, Action::Commit).await;
                expected += 1;
                assert_eq!(observation.next().await, Some(expected));
            },
        ))
    } else {
        let mut expected = 0;
        host.measure(measure_async(
            &host.driver.monitor,
            samples,
            1,
            async || {
                let mut observation = store.observe(|state| state.count).await.expect("observe");
                assert_eq!(observation.next().await, Some(expected));
                drop(observation);
                send_and_finish(&store, Action::Commit).await;
                expected += 1;
            },
        ))
    };
    host.block_on(assert_count(&store, iterations_with_warm_up(samples)));
    host.block_on(store.shutdown()).expect("clean shutdown");
    measurement
}

/// Measures keyed insertion, replacement, and cancellation of pending effects.
///
/// Insertion and churn labels share the insert/cancel/drain lifecycle.
/// Replacement carries the last pending receipt across samples and cancels it after measurement.
fn cancellation(kind: &str, samples: usize) -> Measurement {
    let host = QualificationHost::multi();
    let store = host.store(256);
    let measurement = if let Some(count) = kind.strip_prefix("insertion/") {
        let count = parse_count(count);
        host.measure(measure_async(
            &host.driver.monitor,
            samples,
            count,
            async || {
                let pending = store
                    .send(Action::Pending(count))
                    .await
                    .expect("pending commit");
                assert!(!pending.is_finished(), "pending work must remain live");
                send_and_finish(&store, Action::Cancel).await;
                pending.finish().await;
            },
        ))
    } else if let Some(count) = kind.strip_prefix("replacement/") {
        let count = parse_count(count);
        let mut previous = None;
        let measurement = host.measure(measure_async(
            &host.driver.monitor,
            samples,
            count,
            async || {
                let replacement = store
                    .send(Action::Replacement(count))
                    .await
                    .expect("replacement commit");
                assert!(!replacement.is_finished(), "replacement must remain live");
                if let Some(previous) = previous.replace(replacement) {
                    previous.finish().await;
                }
            },
        ));
        host.block_on(async {
            send_and_finish(&store, Action::Cancel).await;
            if let Some(previous) = previous {
                previous.finish().await;
            }
        });
        measurement
    } else {
        let count = parse_count(kind.strip_prefix("churn/").expect("churn count"));
        host.measure(measure_async(
            &host.driver.monitor,
            samples,
            count,
            async || {
                let pending = store
                    .send(Action::Pending(count))
                    .await
                    .expect("pending commit");
                assert!(!pending.is_finished(), "pending work must remain live");
                send_and_finish(&store, Action::Cancel).await;
                pending.finish().await;
            },
        ))
    };
    host.block_on(assert_count(&store, 0));
    host.block_on(store.shutdown()).expect("clean shutdown");
    measurement
}

/// Measures empty-task fanout, a three-action tree, or pending-task cancellation.
///
/// Fanout receipt completion checks lifetime completion, not independent task cardinality.
fn action_task(kind: &str, samples: usize) -> Measurement {
    let host = QualificationHost::multi();
    let store = host.store(128);
    let measurement = if let Some(count) = kind.strip_prefix("fanout/") {
        let count = parse_count(count);
        host.measure(measure_async(
            &host.driver.monitor,
            samples,
            count,
            async || {
                send_and_finish(&store, Action::TaskFanout(count)).await;
            },
        ))
    } else if kind == "finish" {
        host.measure(measure_async(
            &host.driver.monitor,
            samples,
            3,
            async || {
                send_and_finish(&store, Action::TreeStart).await;
            },
        ))
    } else {
        host.measure(measure_async(
            &host.driver.monitor,
            samples,
            1,
            async || {
                let task = store
                    .send(Action::TaskPending)
                    .await
                    .expect("pending task commit");
                assert!(!task.is_finished(), "pending task must remain live");
                task.cancel();
                task.finish().await;
            },
        ))
    };
    let expected = if kind == "finish" {
        iterations_with_warm_up(samples)
    } else {
        0
    };
    host.block_on(assert_count(&store, expected));
    host.block_on(store.shutdown()).expect("clean shutdown");
    measurement
}

/// Measures timer registration with cancellation, or an already-expired deadline.
///
/// Insert rows include cancellation and draining. They do not measure isolated insertion cost.
fn timer(kind: &str, samples: usize) -> Measurement {
    let host = QualificationHost::multi();
    let store = host.store(256);
    let measurement = if let Some(count) = kind.strip_prefix("insert/") {
        let count = parse_count(count);
        host.measure(measure_async(
            &host.driver.monitor,
            samples,
            count,
            async || {
                let timers = store
                    .send(Action::FarTimers(count))
                    .await
                    .expect("timer insert commit");
                assert!(!timers.is_finished(), "future timers must remain live");
                timers.cancel();
                timers.finish().await;
            },
        ))
    } else if kind == "wake" {
        host.measure(measure_async(
            &host.driver.monitor,
            samples,
            1,
            async || {
                send_and_finish(&store, Action::TimerWake).await;
            },
        ))
    } else {
        host.measure(measure_async(
            &host.driver.monitor,
            samples,
            1,
            async || {
                let timer = store
                    .send(Action::FarTimers(1))
                    .await
                    .expect("timer insert commit");
                assert!(!timer.is_finished(), "future timer must remain live");
                send_and_finish(&store, Action::Cancel).await;
                timer.finish().await;
            },
        ))
    };
    let expected = if kind == "wake" {
        iterations_with_warm_up(samples)
    } else {
        0
    };
    host.block_on(assert_count(&store, expected));
    host.block_on(store.shutdown()).expect("clean shutdown");
    measurement
}

/// Measures an ordered state query without state mutation.
fn query(samples: usize) -> Measurement {
    let host = QualificationHost::multi();
    let store = host.store(64);
    let measurement = host.measure(measure_async(
        &host.driver.monitor,
        samples,
        1,
        async || {
            std::hint::black_box(store.read(|state| state.count).await.expect("state query"));
        },
    ));
    host.block_on(assert_count(&store, 0));
    host.block_on(store.shutdown()).expect("clean shutdown");
    measurement
}

/// Measures Store construction through acknowledged graceful or last-handle shutdown.
fn shutdown(kind: &str, samples: usize) -> Measurement {
    let host = QualificationHost::multi();
    host.measure(measure_async(
        &host.driver.monitor,
        samples,
        1,
        async || {
            let store = host.store(64);
            let closed = store.closed();
            if kind == "graceful" {
                store.shutdown().await.expect("graceful shutdown");
            } else {
                drop(store);
            }
            closed.await;
        },
    ))
}

/// Measures finite ready work, virtual timers, or exact step-budget exhaustion.
///
/// Each sample owns its runtime. Budget rows include error snapshot construction.
fn deterministic(kind: &str, samples: usize) -> Measurement {
    if let Some(count) = kind.strip_prefix("ready/") {
        let count = parse_count(count);
        return measure_sync(samples, count, || {
            let runtime = DeterministicRuntime::new();
            for _ in 0..count {
                runtime.spawn_task(Box::pin(async {}));
            }
            let report = runtime.run_until_stalled().expect("finite ready tasks");
            assert_eq!(report.polls(), count);
            report.polls() as u64
        });
    }
    if let Some(count) = kind.strip_prefix("timer/") {
        let count = parse_count(count);
        return measure_sync(samples, count, || {
            let runtime = DeterministicRuntime::with_step_budget(count.saturating_mul(3).max(16));
            let deadline = MonotonicInstant::from_elapsed(Duration::from_nanos(1));
            for _ in 0..count {
                let sleep = runtime.sleep_until(deadline);
                runtime.spawn_task(sleep);
            }
            let registration = runtime
                .run_until_stalled()
                .expect("timer registration within budget");
            assert_eq!(registration.polls(), count);
            let advance = runtime
                .advance(Duration::from_nanos(1))
                .expect("timer wake within budget");
            assert_eq!(advance.polls(), count);
            assert_eq!(advance.current_time(), deadline);
            advance.polls() as u64
        });
    }
    measure_sync(samples, DEFAULT_STEP_BUDGET, || {
        let runtime = DeterministicRuntime::new();
        runtime.spawn_task(Box::pin(InfiniteSelfWake));
        let error = runtime
            .run_until_stalled()
            .expect_err("infinite wake exhausts budget");
        assert_eq!(error.polls(), DEFAULT_STEP_BUDGET);
        assert_eq!(error.step_budget(), DEFAULT_STEP_BUDGET);
        assert_eq!(error.live_task_ids().len(), 1);
        assert_eq!(error.ready_task_ids().len(), 1);
        error.polls() as u64
    })
}

/// Keeps one task runnable until the deterministic poll budget is exhausted.
struct InfiniteSelfWake;

impl Future for InfiniteSelfWake {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
    }
}

/// Announces a live idle Store for external profiling, then closes it after ten seconds.
fn idle_store() {
    use std::io::Write as _;

    let host = QualificationHost::multi();
    let store = host.store(64);
    host.block_on(store.read(|_| ())).expect("Store start");
    println!("READY");
    std::io::stdout().flush().expect("flush readiness");
    std::thread::sleep(Duration::from_secs(10));
    host.block_on(store.shutdown()).expect("clean shutdown");
}

#[cfg(test)]
mod tests {
    #[test]
    fn mailbox_prefill_panic_releases_blocked_actor() {
        use super::*;

        let host = QualificationHost::multi();
        let store = host.store(1);
        let gate = Arc::new(BlockingGate::default());
        let blocked = store
            .try_send(Action::Block(Arc::clone(&gate)))
            .expect("blocking action admission");
        gate.wait_until_entered();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _release = OpenGateOnDrop(&gate);
            let _queued = store
                .try_send(Action::Commit)
                .expect("first prefill admission");
            let _overflow = store.try_send(Action::Commit).expect("prefill admission");
        }));
        let open_after_unwind = gate.state.lock().expect("gate lock").open;

        // Always release the real actor before asserting the failure boundary.
        // Without this cleanup, dropping the host waits on its blocked worker.
        gate.open();
        host.block_on(async {
            blocked
                .finish()
                .await
                .expect("blocking action commit")
                .finish()
                .await;
            assert_count(&store, 1).await;
            store.shutdown().await.expect("clean shutdown");
        });

        assert!(result.is_err(), "overflow must reproduce the prefill panic");
        assert!(
            open_after_unwind,
            "prefill unwind must release the actor gate"
        );
    }
}
