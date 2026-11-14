//! Public keyed reaction work and allocations, without elapsed-time comparisons.
//! Scalar, disjoint keyed paths are this fixture, not a promise about all legal paths.

use rca_core::{
    EntryMount, Feature, FeatureContext, InitialPolicy, KeyedState, MountChildren, MountSite,
    Runtime, Store, Task,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[path = "fold/support.rs"]
mod support;
use support::Driver;

#[cfg(feature = "allocation-measurement")]
#[global_allocator]
/// Records requested and retained allocations in the single active allocation assay.
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Keeps native runs large enough to expose work proportional to sibling count.
#[cfg(not(miri))]
const WIDTH: usize = 10_000;
// Miri checks the same ownership and reaction phases with enough siblings to
// distinguish no, one and all hooks; native runs retain the full scale witness.
#[cfg(miri)]
/// Preserves the same no/one/all-hook ownership cases at interpreter scale.
const WIDTH: usize = 16;
#[cfg(not(feature = "allocation-measurement"))]
/// Defines the explicit repeated-input witness, separate from phase measurement.
const INPUTS: usize = 10_000;

/// Selects no reactions, one reaction at key zero, or reactions on every row.
#[derive(Clone, Copy, Debug)]
enum Hooks {
    None,
    One,
    All,
}

impl Hooks {
    /// Chooses installed hooks without inspecting the runtime's observed work.
    fn includes(self, key: usize) -> bool {
        matches!(self, Self::All) || matches!(self, Self::One) && key == 0
    }

    /// Supplies the expected number of selectors independently of measured counters.
    fn count(self) -> usize {
        match self {
            Self::None => 0,
            Self::One => 1,
            Self::All => WIDTH,
        }
    }
}

#[derive(Default)]
/// Counts public callbacks and child destruction in a fixed reporting order.
struct Counts {
    reads: AtomicUsize,
    writes: AtomicUsize,
    constructors: AtomicUsize,
    recipes: AtomicUsize,
    selectors: AtomicUsize,
    factories: AtomicUsize,
    bodies: AtomicUsize,
    root_reductions: AtomicUsize,
    child_reductions: AtomicUsize,
    child_drops: AtomicUsize,
}

impl Counts {
    /// Copies callback counts without allocating or changing them.
    ///
    /// Every profile instruments the same callbacks. These counts do not measure
    /// machine instructions or uninstrumented execution speed.
    fn snapshot(&self) -> [usize; 10] {
        [
            &self.reads,
            &self.writes,
            &self.constructors,
            &self.recipes,
            &self.selectors,
            &self.factories,
            &self.bodies,
            &self.root_reductions,
            &self.child_reductions,
            &self.child_drops,
        ]
        .map(|value| value.load(Ordering::Relaxed))
    }
}

/// Keeps scalar keyed state and its independent callback counters together.
struct Rows {
    values: KeyedState<usize, usize>,
    counts: Arc<Counts>,
}

/// Counts each actual shared projection before returning the keyed state.
fn read_rows(rows: &Rows) -> &KeyedState<usize, usize> {
    rows.counts.reads.fetch_add(1, Ordering::Relaxed);
    &rows.values
}

/// Counts each actual mutable projection before returning the keyed state.
fn write_rows(rows: &mut Rows) -> &mut KeyedState<usize, usize> {
    rows.counts.writes.fetch_add(1, Ordering::Relaxed);
    &mut rows.values
}

/// Counts root actions without changing child values.
struct Root(Arc<Counts>);

impl Feature for Root {
    type State = Rows;
    type Action = ();

    fn update(&mut self, _: &mut Rows, (): &(), _: &mut FeatureContext<'_, Rows, ()>) {
        self.0.root_reductions.fetch_add(1, Ordering::Relaxed);
    }
}

/// Increments one row and records when its mounted feature is destroyed.
struct Child(Arc<Counts>);

impl Feature for Child {
    type State = usize;
    type Action = ();

    fn update(&mut self, state: &mut usize, (): &(), _: &mut FeatureContext<'_, usize, ()>) {
        self.0.child_reductions.fetch_add(1, Ordering::Relaxed);
        *state += 1;
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        self.0.child_drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// Identifies the real mounted row at key zero.
type Site = MountSite<Rows, usize, (), ()>;

/// Keeps callback deltas and optional allocation deltas for one measured phase.
struct Cost {
    work: [usize; 10],
    #[cfg(feature = "allocation-measurement")]
    blocks: u64,
    #[cfg(feature = "allocation-measurement")]
    bytes: u64,
}

impl std::fmt::Debug for Cost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut cost = f.debug_struct("Cost");
        cost.field("work", &self.work);
        #[cfg(feature = "allocation-measurement")]
        cost.field("blocks", &self.blocks)
            .field("bytes", &self.bytes);
        cost.finish()
    }
}

/// Measures the supplied public operation, including its explicit driver work.
fn measure<T>(counts: &Counts, run: impl FnOnce() -> T) -> (T, Cost) {
    let before = counts.snapshot();
    #[cfg(feature = "allocation-measurement")]
    let heap = dhat::HeapStats::get();
    let result = run();
    let after = counts.snapshot();
    #[cfg(feature = "allocation-measurement")]
    let current = dhat::HeapStats::get();
    (
        result,
        Cost {
            work: std::array::from_fn(|index| after[index] - before[index]),
            #[cfg(feature = "allocation-measurement")]
            blocks: current.total_blocks - heap.total_blocks,
            #[cfg(feature = "allocation-measurement")]
            bytes: current.total_bytes - heap.total_bytes,
        },
    )
}

/// Installs every row and returns the actual key-zero site after startup completes.
fn fixture(hooks: Hooks, driver: &Driver, counts: &Arc<Counts>) -> (Store<Rows, ()>, Site) {
    let site = Arc::new(Mutex::new(None));
    let saved = Arc::clone(&site);
    let made = Arc::clone(counts);
    let store = Store::builder(
        Rows {
            values: (0..WIDTH).map(|key| (key, 0)).collect(),
            counts: Arc::clone(counts),
        },
        Root(Arc::clone(counts)),
        Runtime::new(driver.clone()),
    )
    .keyed_with_children(read_rows, write_rows, move |key, _| {
        let key = *key;
        made.constructors.fetch_add(1, Ordering::Relaxed);
        let mut entry = EntryMount::new(Child(Arc::clone(&made)));
        if hooks.includes(key) {
            let selected = Arc::clone(&made);
            let reaction = Arc::clone(&made);
            entry = entry.on_change(
                move |value| {
                    selected.selectors.fetch_add(1, Ordering::Relaxed);
                    *value
                },
                InitialPolicy::Skip,
                move |_, _| {
                    reaction.factories.fetch_add(1, Ordering::Relaxed);
                    let body = Arc::clone(&reaction);
                    Task::run(move |_| async move {
                        body.bodies.fetch_add(1, Ordering::Relaxed);
                    })
                },
            );
        }
        let saved = Arc::clone(&saved);
        let counts = Arc::clone(&made);
        (
            entry,
            move |_: &usize, children: &mut MountChildren<'_, Rows, (), usize, ()>| {
                counts.recipes.fetch_add(1, Ordering::Relaxed);
                if key == 0 {
                    *saved.lock().unwrap() = Some(children.site().clone());
                }
            },
        )
    })
    .build()
    .unwrap();
    driver.finish(store.ready()).unwrap();
    let site = site.lock().unwrap().take().expect("key zero recipe ran");
    (store, site)
}

/// Separates setup, reads, unchanged updates, changed input, completion and teardown.
fn phases(hooks: Hooks) {
    let driver = Driver::default();
    let counts = Arc::new(Counts::default());
    let ((store, site), construction) = measure(&counts, || fixture(hooks, &driver, &counts));
    assert_eq!(counts.constructors.load(Ordering::Relaxed), WIDTH);
    assert_eq!(counts.recipes.load(Ordering::Relaxed), WIDTH);
    assert_eq!(counts.selectors.load(Ordering::Relaxed), 0);
    assert_eq!(counts.factories.load(Ordering::Relaxed), 0);
    let (child, acquisition) = measure(&counts, || {
        driver.finish(store.acquire(&site)).unwrap().unwrap()
    });
    let (value, read) = measure(&counts, || {
        driver.finish(child.read(|value| *value)).unwrap()
    });
    assert_eq!(value, 0);
    assert_eq!(acquisition.work[4], 0);
    assert_eq!(read.work[4], 0);
    let (unchanged, no_op) = measure(&counts, || {
        driver.finish(child.update(|value| *value)).unwrap()
    });
    let (value, _, task) = unchanged.into_parts();
    assert_eq!(value, 0);
    assert!(task.is_finished());
    assert_eq!(no_op.work[4], hooks.count() * 2);
    assert_eq!(no_op.work[5], 0);
    drop(task);
    let (task, changed_send) = measure(&counts, || driver.finish(child.send(())).unwrap());
    assert_eq!(changed_send.work[4], hooks.count() * 2);
    assert_eq!(changed_send.work[5], usize::from(hooks.count() != 0));
    assert_eq!(
        changed_send.work[6], 0,
        "commit does not poll deferred reaction work"
    );
    let ((), causal_finish) = measure(&counts, || {
        driver.finish(task.finish());
        // Include terminal-control harvest here rather than in the warm mutation.
        driver.turn();
    });
    assert!(task.is_finished());
    assert!(!task.is_cancelled());
    drop(task);
    let (unchanged, warm_no_op) = measure(&counts, || {
        driver.finish(child.update(|value| *value)).unwrap()
    });
    let (value, _, task) = unchanged.into_parts();
    assert_eq!(value, 1);
    assert!(task.is_finished());
    assert_eq!(warm_no_op.work[4], hooks.count() * 2);
    assert_eq!(warm_no_op.work[5], 0);
    assert_eq!(counts.constructors.load(Ordering::Relaxed), WIDTH);
    assert_eq!(counts.recipes.load(Ordering::Relaxed), WIDTH);
    assert_eq!(counts.root_reductions.load(Ordering::Relaxed), 0);
    assert_eq!(counts.child_reductions.load(Ordering::Relaxed), 1);
    assert_eq!(
        counts.bodies.load(Ordering::Relaxed),
        usize::from(hooks.count() != 0)
    );
    drop(task);
    let ((), teardown) = measure(&counts, || {
        drop(store);
        driver.turn();
    });
    assert_eq!(counts.child_drops.load(Ordering::Relaxed), WIDTH);
    assert_eq!(
        driver
            .finish(child.read(|value| *value))
            .expect_err("the child is unavailable after teardown")
            .reason(),
        rca_core::TaskAccessReason::StoreClosed
    );
    drop((child, site));
    eprintln!(
        "changed costs hooks={hooks:?} width={WIDTH} construction={construction:?} acquire={acquisition:?} read={read:?} no_op={no_op:?} changed_send={changed_send:?} causal_finish={causal_finish:?} warm_no_op={warm_no_op:?} teardown={teardown:?}; work=[keyed_reads,keyed_writes,constructors,recipes,selectors,factories,bodies,root_reductions,child_reductions,child_drops]; allocation phases include public protocol and deterministic driver, not capture-only"
    );
}

/// Checks reaction work and, in the isolated host, its allocation costs.
///
/// # Panics
/// Panics if reaction work or allocation counts violate the recorded contracts.
#[cfg_attr(not(feature = "allocation-measurement"), test)]
pub fn children_have_exact_reaction_work() {
    #[cfg(feature = "allocation-measurement")]
    let _profiler = dhat::Profiler::builder().testing().build();
    for hooks in [Hooks::None, Hooks::One, Hooks::All] {
        phases(hooks);
    }
}

#[cfg(not(feature = "allocation-measurement"))]
/// Checks each completed input without a profiler or elapsed-time comparison.
fn inputs(hooks: Hooks) {
    let driver = Driver::default();
    let counts = Arc::new(Counts::default());
    let (store, site) = fixture(hooks, &driver, &counts);
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    for _ in 0..INPUTS {
        let task = driver.finish(child.send(())).unwrap();
        driver.finish(task.finish());
        assert!(task.is_finished());
        assert!(!task.is_cancelled());
    }
    assert_eq!(driver.finish(child.read(|value| *value)).unwrap(), INPUTS);
    assert_eq!(
        counts.selectors.load(Ordering::Relaxed),
        2 * hooks.count() * INPUTS
    );
    let reactions = usize::from(hooks.count() != 0) * INPUTS;
    assert_eq!(counts.factories.load(Ordering::Relaxed), reactions);
    assert_eq!(counts.bodies.load(Ordering::Relaxed), reactions);
    assert_eq!(counts.constructors.load(Ordering::Relaxed), WIDTH);
    assert_eq!(counts.recipes.load(Ordering::Relaxed), WIDTH);
    assert_eq!(counts.root_reductions.load(Ordering::Relaxed), 0);
    assert_eq!(counts.child_reductions.load(Ordering::Relaxed), INPUTS);
    drop(store);
    driver.turn();
    assert_eq!(counts.child_drops.load(Ordering::Relaxed), WIDTH);
    eprintln!(
        "changed inputs hooks={hooks:?} width={WIDTH} inputs={INPUTS} final={INPUTS} work={:?}; normal-feature test harness, no dhat, includes setup/validation/teardown",
        counts.snapshot()
    );
}

#[cfg(not(feature = "allocation-measurement"))]
#[test]
#[ignore = "explicit finite 10K-input public-path cost witness"]
fn ten_thousand_inputs_without_hooks() {
    inputs(Hooks::None);
}

#[cfg(not(feature = "allocation-measurement"))]
#[test]
#[ignore = "explicit finite 10K-input public-path cost witness"]
fn ten_thousand_inputs_with_one_hook() {
    inputs(Hooks::One);
}

#[cfg(not(feature = "allocation-measurement"))]
#[test]
#[ignore = "explicit finite 10K-input public-path cost witness"]
fn ten_thousand_inputs_with_all_hooks() {
    inputs(Hooks::All);
}
