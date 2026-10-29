#![cfg(feature = "allocation-measurement")]
//! Compares lifecycle delivery phases with and without an enabled sink.

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(
    dead_code,
    reason = "this allocation target only uses manual scheduling"
)]
#[path = "support/mod.rs"]
mod support;

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

#[cfg(feature = "diagnostics")]
use rca_core::diagnostics::{CancellationCause, LifecycleEvent, LifecycleKind};
use rca_core::{
    Effect, Feature, FeatureContext, MountIdentity, OptionalMount, RuntimeFuture, Store,
};

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Keeps printed phase names aligned with the nine measured costs.
const PHASES: [&str; 9] = [
    "constructor",
    "sink_box",
    "build",
    "first_installation",
    "ordinary_16",
    "configuration",
    "equal_key_replacement",
    "explicit_cancellation",
    "last_owner_stop",
];

#[derive(Default)]
/// Retains final state and child disposal after the Store is gone.
struct Counts {
    roots_dropped: AtomicUsize,
    children_built: AtomicUsize,
    children_dropped: AtomicUsize,
    final_reductions: AtomicUsize,
    final_checksum: AtomicUsize,
    final_configuration: AtomicU64,
    final_replacement: AtomicU64,
}

struct State {
    child: Option<()>,
    configuration: u64,
    replacement: u64,
    reductions: usize,
    checksum: usize,
    counts: Arc<Counts>,
}

impl Drop for State {
    fn drop(&mut self) {
        self.counts
            .final_reductions
            .store(self.reductions, Ordering::SeqCst);
        self.counts
            .final_checksum
            .store(self.checksum, Ordering::SeqCst);
        self.counts
            .final_configuration
            .store(self.configuration, Ordering::SeqCst);
        self.counts
            .final_replacement
            .store(self.replacement, Ordering::SeqCst);
        self.counts.roots_dropped.fetch_add(1, Ordering::SeqCst);
    }
}

enum Action {
    Noop,
    Configure,
    Replace,
    Cancel,
}

struct Root;

impl Feature for Root {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut FeatureContext<'_, State, Action>,
    ) {
        let tag = match action {
            Action::Noop => 1,
            Action::Configure => {
                state.configuration = 9001;
                2
            }
            Action::Replace => {
                state.replacement += 1;
                3
            }
            // An empty cancellation is still a real compiler-applied request.
            Action::Cancel => {
                context.effect(Effect::cancel("allocation-probe"));
                4
            }
        };
        state.reductions += 1;
        state.checksum += state.reductions * tag;
    }
}

struct Child(Arc<Counts>);

impl Feature for Child {
    type State = ();
    type Action = ();

    fn reduce(&mut self, (): &mut (), (): &(), _: &mut FeatureContext<'_, (), ()>) {}
}

impl Drop for Child {
    fn drop(&mut self) {
        self.0.children_dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
/// Stores callback events in fixed space outside the allocator comparison.
struct Events {
    #[cfg(feature = "diagnostics")]
    values: [Option<LifecycleEvent>; 32],
    length: usize,
}

#[cfg(feature = "diagnostics")]
impl Events {
    /// Appends one event without growing a callback-owned collection.
    fn record(&mut self, event: &LifecycleEvent) {
        self.values[self.length] = Some(*event);
        self.length += 1;
    }
}

#[derive(Clone, Copy)]
/// Counts allocations and delivered events for one named phase.
struct Cost {
    blocks: u64,
    bytes: u64,
    events: usize,
}

/// Carries the final state oracle and all phase costs out of the workload.
struct Report {
    costs: [Cost; 9],
    last_event_revision: Option<u64>,
    reductions: usize,
    checksum: usize,
    configuration: u64,
    replacement: u64,
    causal_completed: usize,
}

/// Measures one phase without charging caller buffers or output formatting.
///
/// Callback writes use fixed storage and cannot grow a `Vec`.
fn measured<T>(events: &Mutex<Events>, run: impl FnOnce() -> T) -> (T, Cost) {
    let event_before = events.lock().unwrap().length;
    let before = dhat::HeapStats::get();
    let result = run();
    let after = dhat::HeapStats::get();
    let event_after = events.lock().unwrap().length;
    (
        result,
        Cost {
            blocks: after.total_blocks - before.total_blocks,
            bytes: after.total_bytes - before.total_bytes,
            events: event_after - event_before,
        },
    )
}

/// Polls once without running queued actor work.
fn poll<T>(future: Pin<&mut (impl Future<Output = T> + ?Sized)>) -> Poll<T> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Requires an actor reply after one deliberate actor poll.
fn ordered<T>(actor: &mut RuntimeFuture, request: impl Future<Output = T>) -> T {
    let mut request = std::pin::pin!(request);
    assert!(poll(request.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    match poll(request.as_mut()) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("missing actor-ordered reply"),
    }
}

/// Commits one real action and checks its causal task before the next phase.
fn committed(actor: &mut RuntimeFuture, store: &Store<State, Action>, action: Action) -> usize {
    let task = ordered(actor, store.send(action)).unwrap();
    let mut finish = std::pin::pin!(task.finish());
    assert!(poll(finish.as_mut()).is_ready());
    assert!(task.is_finished());
    assert!(!task.is_cancelled());
    1
}

#[expect(
    clippy::too_many_lines,
    reason = "one ordered workload preserves exact allocation windows and validates their literal event trace"
)]
/// Runs the same nineteen-action lifecycle workload for both sink modes.
fn workload(with_sink: bool) -> Report {
    // Identical caller setup is excluded, including diagnostics-off runs.
    // The build row includes real provider spawn/queue costs, not just library
    // allocations. The same ManualRuntime and spawn count control both modes.
    let runtime = support::ManualRuntime::new();
    let events = Arc::new(Mutex::new(Events::default()));
    let counts = Arc::new(Counts::default());
    let (builder, constructor) = measured(&events, || {
        let factory = Arc::clone(&counts);
        let mount = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state: &State| {
                state.child.as_ref().map(|()| MountIdentity {
                    key: (),
                    replacement: state.replacement,
                    configuration: state.configuration,
                })
            },
            move |()| {
                factory.children_built.fetch_add(1, Ordering::SeqCst);
                Child(Arc::clone(&factory))
            },
        );
        Store::feature_builder(
            State {
                child: Some(()),
                configuration: 41,
                replacement: 0,
                reductions: 0,
                checksum: 0,
                counts: Arc::clone(&counts),
            },
            Root,
            runtime.runtime(),
        )
        .mount(mount)
    });
    let (builder, sink_box) = measured(&events, || {
        #[cfg(feature = "diagnostics")]
        if with_sink {
            let sink = Arc::clone(&events);
            return builder.with_lifecycle_diagnostics(move |event| {
                sink.lock().unwrap().record(event);
            });
        }
        builder
    });
    let (store, build) = measured(&events, || builder.build().unwrap());
    let mut actor = runtime.take_only_queued_task();
    let ((), installation) = measured(&events, || assert!(poll(actor.as_mut()).is_pending()));
    let mut causal_completed = 0;
    let ((), ordinary) = measured(&events, || {
        for _ in 0..16 {
            causal_completed += committed(&mut actor, &store, Action::Noop);
        }
    });
    let ((), configuration) = measured(&events, || {
        causal_completed += committed(&mut actor, &store, Action::Configure);
    });
    let ((), replacement) = measured(&events, || {
        causal_completed += committed(&mut actor, &store, Action::Replace);
    });
    let ((), cancellation) = measured(&events, || {
        causal_completed += committed(&mut actor, &store, Action::Cancel);
    });
    let ((), stop) = measured(&events, || {
        drop(store);
        assert!(poll(actor.as_mut()).is_ready());
        drop(actor);
    });
    assert_eq!(counts.roots_dropped.load(Ordering::SeqCst), 1);
    assert_eq!(counts.children_built.load(Ordering::SeqCst), 2);
    assert_eq!(counts.children_dropped.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.spawn_count(), 1, "no hidden runtime work");
    let reductions = counts.final_reductions.load(Ordering::SeqCst);
    let checksum = counts.final_checksum.load(Ordering::SeqCst);
    let final_configuration = counts.final_configuration.load(Ordering::SeqCst);
    let final_replacement = counts.final_replacement.load(Ordering::SeqCst);
    assert_eq!(
        (reductions, checksum, final_configuration, final_replacement),
        (19, 300, 9001, 1)
    );
    assert_eq!(causal_completed, 19);

    let costs = [
        constructor,
        sink_box,
        build,
        installation,
        ordinary,
        configuration,
        replacement,
        cancellation,
        stop,
    ];
    #[cfg(feature = "diagnostics")]
    let last_event_revision = with_sink.then(|| {
        let events = events.lock().unwrap();
        assert_eq!(events.length, 10);
        assert_eq!(costs.map(|cost| cost.events), [0, 0, 0, 2, 0, 1, 3, 1, 3]);
        let event = |index: usize| events.values[index].unwrap();
        assert!(matches!(
            event(0).kind(),
            LifecycleKind::Installed {
                lifetime: 0,
                parent: None,
                ..
            }
        ));
        let LifecycleKind::Installed {
            lifetime: old,
            parent: Some(0),
            configuration: Some(0),
        } = event(1).kind()
        else {
            panic!("actual native child installation");
        };
        assert_eq!(
            event(2).kind(),
            LifecycleKind::ConfigurationChanged {
                lifetime: old,
                previous: 0,
                current: 1
            }
        );
        assert_eq!(event(3).kind(), LifecycleKind::Retired { lifetime: old });
        let LifecycleKind::Installed {
            lifetime: fresh,
            parent: Some(0),
            configuration: Some(0),
        } = event(4).kind()
        else {
            panic!("actual equal-key replacement installation");
        };
        assert_ne!(old, fresh);
        assert_eq!(
            event(5).kind(),
            LifecycleKind::CancellationRequested {
                cause: CancellationCause::ScopeRetirement,
                scope: Some(old)
            }
        );
        assert_eq!(
            event(6).kind(),
            LifecycleKind::CancellationRequested {
                cause: CancellationCause::Explicit,
                scope: Some(0)
            }
        );
        assert_eq!(event(7).kind(), LifecycleKind::IngressClosed);
        assert_eq!(
            event(8).kind(),
            LifecycleKind::CancellationRequested {
                cause: CancellationCause::ActorStop,
                scope: None
            }
        );
        assert_eq!(event(9).kind(), LifecycleKind::ActorStopped);
        assert_eq!(
            std::array::from_fn::<_, 10, _>(|index| event(index).last_committed_version()),
            // Initial native Structural publication commits revision 1; the
            // nineteen ordinary actions then reach revision 20.
            [0, 0, 17, 18, 18, 18, 20, 20, 20, 20]
        );
        event(9).last_committed_version()
    });
    #[cfg(not(feature = "diagnostics"))]
    let last_event_revision = None;
    if !with_sink {
        assert!(costs.iter().all(|cost| cost.events == 0));
        assert_eq!((sink_box.blocks, sink_box.bytes), (0, 0));
    }
    Report {
        costs,
        last_event_revision,
        reductions,
        checksum,
        configuration: final_configuration,
        replacement: final_replacement,
        causal_completed,
    }
}

// Catches event allocation/formatting, a callback queue, and consumer storage
// accidentally charged to delivery. Off/on comparisons require separate runs;
// only enabled-sink versus enabled-no-sink equality is asserted in this binary.
#[test]
fn lifecycle_delivery_adds_no_phase_allocations_beyond_the_sink_box() {
    if allocation_test::run_in_child(
        "lifecycle_delivery_adds_no_phase_allocations_beyond_the_sink_box",
    ) {
        return;
    }

    let profiler = dhat::Profiler::builder().testing().build();
    // Warm real paths before retained rows; no formatting occurs under profiling.
    let _ = workload(false);
    #[cfg(feature = "diagnostics")]
    let _ = workload(true);
    let without = workload(false);
    #[cfg(feature = "diagnostics")]
    let enabled = workload(true);
    drop(profiler);

    let print = |mode: &str, report: &Report| {
        for (phase, cost) in PHASES.into_iter().zip(report.costs) {
            eprintln!(
                "diagnostics={} sink={mode} phase={phase} blocks={} bytes={} events={}",
                cfg!(feature = "diagnostics"),
                cost.blocks,
                cost.bytes,
                cost.events
            );
        }
        eprintln!(
            "diagnostics={} sink={mode} last_event_revision={:?}",
            cfg!(feature = "diagnostics"),
            report.last_event_revision
        );
        eprintln!(
            "diagnostics={} sink={mode} root_commits={} causal_completed={} checksum={} final_configuration={} final_replacement={} child_constructions=2 child_drops=2 root_drops=1 actor_spawns=1",
            cfg!(feature = "diagnostics"),
            report.reductions,
            report.causal_completed,
            report.checksum,
            report.configuration,
            report.replacement
        );
    };
    eprintln!(
        "layout diagnostics={} pointer={} state_inline={} state_align={} action_inline={} feature_builder_inline={} store_handle_inline={} erased_runtime_future_handle_inline={} counts_payload_inline={} event_buffer_mutex_payload_inline={} event_buffer_mutex_payload_align={} event_buffer_arc_handle_inline={}",
        cfg!(feature = "diagnostics"),
        std::mem::size_of::<usize>(),
        std::mem::size_of::<State>(),
        std::mem::align_of::<State>(),
        std::mem::size_of::<Action>(),
        std::mem::size_of::<rca_core::FeatureStoreBuilder<State, Action, Root>>(),
        std::mem::size_of::<Store<State, Action>>(),
        std::mem::size_of::<RuntimeFuture>(),
        std::mem::size_of::<Counts>(),
        std::mem::size_of::<Mutex<Events>>(),
        std::mem::align_of::<Mutex<Events>>(),
        std::mem::size_of::<Arc<Mutex<Events>>>()
    );
    print("none", &without);
    #[cfg(feature = "diagnostics")]
    {
        print("fixed", &enabled);
        for (index, phase) in PHASES.into_iter().enumerate() {
            if index != 1 {
                assert_eq!(
                    (enabled.costs[index].blocks, enabled.costs[index].bytes),
                    (without.costs[index].blocks, without.costs[index].bytes),
                    "enabled delivery allocated in {phase}"
                );
            }
        }
        assert_eq!(enabled.costs[1].blocks, 1, "one owned callback Box");
        assert_eq!(
            enabled.costs[1].bytes,
            u64::try_from(std::mem::size_of::<Arc<Mutex<Events>>>()).unwrap()
        );
        assert_eq!(enabled.last_event_revision, Some(20));
    }
}
