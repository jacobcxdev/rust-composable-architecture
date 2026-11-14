//! Owned history, explicit shared reads, and omitted authority witnesses.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use rca_core::{KeyedState, MonotonicInstant, Snapshot};

struct Authority(Rc<Cell<usize>>);

impl Drop for Authority {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

struct Importer {
    progress: usize,
    history: Vec<String>,
    bookkeeping: usize,
    quota: Rc<Cell<usize>>,
    task: Authority,
}

#[derive(Clone, Debug, PartialEq)]
struct ImportHistory {
    progress: usize,
    history: Vec<String>,
    bookkeeping: usize,
    quota: usize,
}

impl Snapshot for Importer {
    type Value = ImportHistory;

    fn snapshot(&self) -> Self::Value {
        // Task authority is deliberately omitted and checked separately below.
        ImportHistory {
            progress: self.progress.snapshot(),
            history: self.history.snapshot(),
            bookkeeping: self.bookkeeping.snapshot(),
            quota: self.quota.get(),
        }
    }
}

#[test]
fn manual_history_is_independent_in_both_directions_without_retaining_authority() {
    let dropped = Rc::new(Cell::new(0));
    let quota = Rc::new(Cell::new(12));
    let mut source = Importer {
        progress: 1,
        history: vec!["queued".into()],
        bookkeeping: 3,
        quota: Rc::clone(&quota),
        task: Authority(Rc::clone(&dropped)),
    };
    let first = source.snapshot();
    assert_eq!(Rc::strong_count(&quota), 2);
    assert_eq!(Rc::strong_count(&source.task.0), 2);
    source.progress = 2;
    source.history.push("imported".into());
    source.bookkeeping = 4;
    quota.set(11);
    let second = source.snapshot();
    assert_eq!(
        first,
        ImportHistory {
            progress: 1,
            history: vec!["queued".into()],
            bookkeeping: 3,
            quota: 12,
        }
    );
    assert_eq!(
        second,
        ImportHistory {
            progress: 2,
            history: vec!["queued".into(), "imported".into()],
            bookkeeping: 4,
            quota: 11,
        }
    );
    let mut expected = first.clone();
    expected.progress = 99;
    expected.history[0] = "expected only".into();
    expected.bookkeeping = 0;
    expected.quota = 0;
    assert_eq!(first.history, ["queued"]);
    assert_eq!(source.history, ["queued", "imported"]);
    assert_eq!(
        (source.progress, source.bookkeeping, quota.get()),
        (2, 4, 11)
    );
    assert_eq!(dropped.get(), 0);
    drop(source);
    assert_eq!(dropped.get(), 1);
    assert_eq!(Rc::strong_count(&quota), 1);
    assert_eq!(second.quota, 11);
}

struct Borrowed<'a>(&'a str);

impl Snapshot for Borrowed<'_> {
    type Value = String;

    fn snapshot(&self) -> Self::Value {
        self.0.to_owned()
    }
}

#[test]
fn manual_borrowed_source_can_leave_owned_history_after_its_lifetime() {
    let history = {
        let local = String::from("local");
        Borrowed(&local).snapshot()
    };
    assert_eq!(history, "local");
}

// Neither source nor captured value implements Clone, Copy, Debug, Default or
// equality. The captured value is also !Send/!Sync; its new Rc is not a source alias.
struct LocalSource(usize);
struct LocalValue(Rc<usize>);

impl Snapshot for LocalSource {
    type Value = LocalValue;

    fn snapshot(&self) -> Self::Value {
        LocalValue(Rc::new(self.0))
    }
}

#[test]
fn recursive_containers_do_not_impose_consumer_bounds_on_values() {
    let source = [LocalSource(3), LocalSource(5)];
    let values = source.snapshot();
    assert_eq!((*values[0].0, *values[1].0), (3, 5));
    let empty: [LocalSource; 0] = [];
    let _: [LocalValue; 0] = empty.snapshot();
    let values = vec![Some(Ok::<_, LocalSource>(Box::new(LocalSource(7)))), None].snapshot();
    match &values[0] {
        Some(Ok(value)) => assert_eq!(*value.0, 7),
        _ => panic!("expected captured owned value"),
    }
    let failure = Err::<LocalSource, _>(LocalSource(9)).snapshot();
    match failure {
        Err(value) => assert_eq!(*value.0, 9),
        Ok(_) => panic!("expected captured error value"),
    }
    let absent: Option<LocalSource> = None;
    assert!(absent.snapshot().is_none());
    assert_eq!(*(LocalSource(1),).snapshot().0.0, 1);
    let pair = (LocalSource(2), LocalSource(3)).snapshot();
    assert_eq!((*pair.0.0, *pair.1.0), (2, 3));
    let triple = (LocalSource(4), LocalSource(5), LocalSource(6)).snapshot();
    assert_eq!((*triple.0.0, *triple.1.0, *triple.2.0), (4, 5, 6));
    let four = (
        LocalSource(7),
        LocalSource(8),
        LocalSource(9),
        LocalSource(10),
    )
        .snapshot();
    assert_eq!((*four.0.0, *four.1.0, *four.2.0, *four.3.0), (7, 8, 9, 10));
}

#[test]
fn explicit_scalar_and_time_snapshots_preserve_values() {
    macro_rules! same {
        ($($value:expr),+ $(,)?) => {$(assert_eq!($value.snapshot(), $value);)+};
    }
    same!(
        (),
        true,
        'x',
        1_u8,
        2_u16,
        3_u32,
        4_u64,
        5_u128,
        6_usize,
        -1_i8,
        -2_i16,
        -3_i32,
        -4_i64,
        -5_i128,
        -6_isize,
        1.25_f32,
        2.5_f64,
        String::from("owned"),
        Duration::from_millis(37),
        MonotonicInstant::from_elapsed(Duration::from_secs(3))
    );
    assert!(f64::NAN.snapshot().is_nan());
}

#[derive(Eq, Hash, PartialEq)]
struct WorkerId(u8);

impl Snapshot for WorkerId {
    type Value = u8;

    fn snapshot(&self) -> Self::Value {
        // Deliberately colliding projection: the roster must retain both entries.
        0
    }
}

struct Worker {
    progress: usize,
    authority: Authority,
}

impl Snapshot for Worker {
    type Value = usize;

    fn snapshot(&self) -> Self::Value {
        self.progress
    }
}

#[test]
fn keyed_roster_keeps_order_collisions_and_history_without_live_child_authority() {
    // Empty rosters need no Hash/equality/Clone requirements on identifiers.
    let empty = KeyedState::<LocalSource, LocalSource>::new().snapshot();
    assert!(empty.is_empty());
    let drops = Rc::new(Cell::new(0));
    let worker = |progress| Worker {
        progress,
        authority: Authority(Rc::clone(&drops)),
    };
    let mut source = KeyedState::new();
    source.insert(WorkerId(1), worker(10));
    source.insert(WorkerId(2), worker(20));
    let first = source.snapshot();
    assert_eq!(first, [(0, 10), (0, 20)]);
    assert_eq!(Rc::strong_count(&drops), 3);
    assert_eq!(
        Rc::strong_count(&source.values().next().unwrap().authority.0),
        3
    );
    source.move_index(0, 1);
    assert_eq!(source.snapshot(), [(0, 20), (0, 10)]);
    drop(source.remove(&WorkerId(1)).unwrap());
    assert_eq!(drops.get(), 1);
    source.insert(WorkerId(1), worker(10));
    assert_eq!(source.snapshot(), [(0, 20), (0, 10)]);
    let mut expected = first.clone();
    expected[0].1 = 99;
    expected.reverse();
    assert_eq!(first, [(0, 10), (0, 20)]);
    assert_eq!(source.snapshot(), [(0, 20), (0, 10)]);
    drop(source);
    assert_eq!(drops.get(), 3);
    assert_eq!(Rc::strong_count(&drops), 1);
    assert_eq!(first.len(), 2);
}

#[test]
fn explicit_cyclic_graph_projection_is_finite_owned_data_not_graph_traversal() {
    struct Node {
        id: usize,
        label: String,
        next: Option<Rc<RefCell<Node>>>,
    }
    struct Graph(Rc<RefCell<Node>>);
    impl Snapshot for Graph {
        type Value = (usize, String, Option<usize>);

        fn snapshot(&self) -> Self::Value {
            let root = self.0.borrow();
            // Explicit policy: root values and one neighbour ID, not its handle.
            (
                root.id,
                root.label.snapshot(),
                root.next.as_ref().map(|next| next.borrow().id),
            )
        }
    }
    let root = Rc::new(RefCell::new(Node {
        id: 1,
        label: "root".into(),
        next: None,
    }));
    let child = Rc::new(RefCell::new(Node {
        id: 2,
        label: "child".into(),
        next: Some(Rc::clone(&root)),
    }));
    root.borrow_mut().next = Some(Rc::clone(&child));
    let graph = Graph(Rc::clone(&root));
    let history = graph.snapshot();
    assert_eq!(history, (1, "root".into(), Some(2)));
    assert_eq!((Rc::strong_count(&root), Rc::strong_count(&child)), (3, 2));
    root.borrow_mut().label = "changed".into();
    let mut expected = history.clone();
    expected.1 = "expected only".into();
    assert_eq!(history.1, "root");
    assert_eq!(root.borrow().label, "changed");
    // Break the deliberate source cycle; captured history keeps neither node alive.
    root.borrow_mut().next = None;
    child.borrow_mut().next = None;
    drop(graph);
    assert_eq!((Rc::strong_count(&root), Rc::strong_count(&child)), (1, 1));
    drop(root);
    drop(child);
    assert_eq!(history.2, Some(2));
}

#[test]
fn deliberately_bad_custom_snapshot_can_compile_but_aliases_mutable_history() {
    struct Bad(Rc<RefCell<Vec<usize>>>);
    impl Snapshot for Bad {
        type Value = Rc<RefCell<Vec<usize>>>;

        fn snapshot(&self) -> Self::Value {
            Rc::clone(&self.0)
        }
    }
    // This is a semantic counterexample, not a recommended implementation.
    let source = Bad(Rc::new(RefCell::new(vec![1])));
    let history = source.snapshot();
    source.0.borrow_mut().push(2);
    assert_eq!(*history.borrow(), [1, 2]);
    history.borrow_mut().push(3);
    assert_eq!(*source.0.borrow(), [1, 2, 3]);
}

#[test]
#[cfg(feature = "diagnostics")]
fn diagnostics_accept_manual_snapshot_without_test_store_value_bounds() {
    struct DiagnosticSource(usize);
    #[derive(PartialEq)]
    struct DiagnosticValue(Rc<usize>);
    impl Snapshot for DiagnosticSource {
        type Value = DiagnosticValue;

        fn snapshot(&self) -> Self::Value {
            DiagnosticValue(Rc::new(self.0))
        }
    }
    let mut source = DiagnosticSource(2);
    let mut changes = Vec::new();
    rca_core::diagnostics::checkpoint(
        &mut source,
        "manual snapshot",
        Snapshot::snapshot,
        |state| {
            state.0 = 4;
            Ok::<_, ()>(())
        },
        |event| changes.push((*event.before().0, *event.after().0, event.is_changed())),
    )
    .unwrap();
    assert_eq!(source.0, 4);
    if cfg!(debug_assertions) {
        assert_eq!(changes, [(2, 4, true)]);
    } else {
        assert_eq!(changes, [] as [(usize, usize, bool); 0]);
    }
}

struct CaptureTrace {
    order: RefCell<Vec<usize>>,
    fail: Cell<bool>,
    output_drops: Cell<usize>,
    source_drops: Cell<usize>,
}

impl CaptureTrace {
    fn new() -> Self {
        Self {
            order: RefCell::new(Vec::new()),
            fail: Cell::new(true),
            output_drops: Cell::new(0),
            source_drops: Cell::new(0),
        }
    }

    fn sources(&self) -> [CaptureProbe<'_>; 3] {
        [1, 2, 3].map(|value| CaptureProbe { value, trace: self })
    }
}

struct CaptureProbe<'a> {
    value: usize,
    trace: &'a CaptureTrace,
}

struct CapturedProbe<'a> {
    value: usize,
    trace: &'a CaptureTrace,
}

impl<'a> Snapshot for CaptureProbe<'a> {
    type Value = CapturedProbe<'a>;

    fn snapshot(&self) -> Self::Value {
        self.trace.order.borrow_mut().push(self.value);
        assert!(
            !(self.trace.fail.get() && self.value == 2),
            "capture second element"
        );
        CapturedProbe {
            value: self.value,
            trace: self.trace,
        }
    }
}

impl Drop for CaptureProbe<'_> {
    fn drop(&mut self) {
        self.trace
            .source_drops
            .set(self.trace.source_drops.get() + 1);
    }
}

impl Drop for CapturedProbe<'_> {
    fn drop(&mut self) {
        self.trace
            .output_drops
            .set(self.trace.output_drops.get() + 1);
    }
}

// A container that retries, skips or reorders elements, returns partial history,
// or leaks completed outputs when a later element panics must fail here.
fn capture_after_element_failure<T: Snapshot>(source: &T, trace: &CaptureTrace) -> T::Value {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.snapshot()));
    let Err(panic) = result else {
        panic!("second element capture must fail");
    };
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"capture second element")
    );
    assert_eq!(*trace.order.borrow(), [1, 2]);
    assert_eq!(trace.output_drops.get(), 1);
    assert_eq!(trace.source_drops.get(), 0);

    trace.fail.set(false);
    let recovered = source.snapshot();
    assert_eq!(*trace.order.borrow(), [1, 2, 1, 2, 3]);
    assert_eq!(trace.output_drops.get(), 1);
    assert_eq!(trace.source_drops.get(), 0);
    recovered
}

#[test]
fn array_capture_unwind_drops_only_completed_outputs() {
    let trace = CaptureTrace::new();
    let source = trace.sources();
    let recovered = capture_after_element_failure(&source, &trace);
    assert_eq!(source.each_ref().map(|value| value.value), [1, 2, 3]);
    assert_eq!(recovered.each_ref().map(|value| value.value), [1, 2, 3]);
    drop(recovered);
    assert_eq!(trace.output_drops.get(), 4);
    assert_eq!(trace.source_drops.get(), 0);
    drop(source);
    assert_eq!(trace.source_drops.get(), 3);
    assert_eq!(trace.output_drops.get(), 4);
}

#[test]
fn vec_capture_unwind_drops_only_completed_outputs() {
    let trace = CaptureTrace::new();
    let source = Vec::from(trace.sources());
    let recovered = capture_after_element_failure(&source, &trace);
    assert_eq!(
        source.iter().map(|value| value.value).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        recovered
            .iter()
            .map(|value| value.value)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    drop(recovered);
    assert_eq!(trace.output_drops.get(), 4);
    assert_eq!(trace.source_drops.get(), 0);
    drop(source);
    assert_eq!(trace.source_drops.get(), 3);
    assert_eq!(trace.output_drops.get(), 4);
}
