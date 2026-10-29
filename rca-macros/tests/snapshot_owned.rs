//! Checks generated snapshot values, ownership, formatting and unwind behaviour.
//!
//! Fixture declarations model capture shapes; the assertions define their contracts.

use std::{cell::Cell, marker::PhantomData, rc::Rc};

use rca::Snapshot;
use rca_macros::Snapshot;

/// Checks exact Debug output and error propagation with a non-empty expected value.
fn assert_debug_contract<T: std::fmt::Debug + ?Sized>(value: &T, expected: &str) {
    /// Accepts at most the stored byte count before returning a formatting error.
    struct LimitedWriter(usize);
    impl std::fmt::Write for LimitedWriter {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            if s.len() > self.0 {
                return Err(std::fmt::Error);
            }
            self.0 -= s.len();
            Ok(())
        }
    }

    assert_eq!(format!("{value:?}"), expected);
    for budget in [0, expected.len() - 1] {
        assert_eq!(
            std::fmt::write(&mut LimitedWriter(budget), format_args!("{value:?}")),
            Err(std::fmt::Error)
        );
    }
}

/// Checks cloning and formatting alongside each fixture's field-specific assertions.
fn assert_value_contract<T: Clone + Eq + std::fmt::Debug>(value: &T, expected: &str) {
    assert_eq!(&value.clone(), value);
    assert_debug_contract(value, expected);
}

/// Records clone order and distinguishes source drops from completed clone drops.
struct CloneTrace {
    /// Field values observed by each attempted clone.
    order: std::cell::RefCell<Vec<usize>>,
    /// Causes the second field's clone to panic until the recovery check.
    fail: Cell<bool>,
    /// Completed clones destroyed during unwinding or explicit cleanup.
    cloned_drops: Cell<usize>,
    /// Original fields destroyed when the source is dropped.
    source_drops: Cell<usize>,
}

impl CloneTrace {
    /// Starts an empty trace with the second-field failure enabled.
    fn new() -> Self {
        Self {
            order: std::cell::RefCell::new(Vec::new()),
            fail: Cell::new(true),
            cloned_drops: Cell::new(0),
            source_drops: Cell::new(0),
        }
    }

    /// Creates an original field that reports clone attempts and destruction here.
    fn value(&self, value: usize) -> CloneProbe<'_> {
        CloneProbe {
            value,
            trace: self,
            cloned: false,
        }
    }
}

/// Reports generated clone behaviour without sharing the field value itself.
struct CloneProbe<'a> {
    /// Distinguishes the first and second fields in the oracle.
    value: usize,
    /// Borrows instrumentation that outlives the source and its clones.
    trace: &'a CloneTrace,
    /// Distinguishes completed clones from original fields during destruction.
    cloned: bool,
}

impl Clone for CloneProbe<'_> {
    fn clone(&self) -> Self {
        self.trace.order.borrow_mut().push(self.value);
        assert!(
            !(self.trace.fail.get() && self.value == 2),
            "clone second field"
        );
        Self {
            value: self.value,
            trace: self.trace,
            cloned: true,
        }
    }
}

impl Drop for CloneProbe<'_> {
    fn drop(&mut self) {
        let drops = if self.cloned {
            &self.trace.cloned_drops
        } else {
            &self.trace.source_drops
        };
        drops.set(drops.get() + 1);
    }
}

/// Checks partial-clone cleanup, source preservation and recovery on generated values.
fn assert_partial_clone<T: Clone>(
    source: T,
    trace: &CloneTrace,
    values: impl Fn(&T) -> (usize, usize),
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.clone()));
    let Err(panic) = result else {
        panic!("second field clone must fail");
    };
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"clone second field"));
    assert_eq!(*trace.order.borrow(), [1, 2]);
    assert_eq!(trace.cloned_drops.get(), 1);
    assert_eq!(trace.source_drops.get(), 0);
    assert_eq!(values(&source), (1, 2));

    trace.fail.set(false);
    let recovered = source.clone();
    assert_eq!(*trace.order.borrow(), [1, 2, 1, 2]);
    assert_eq!(values(&recovered), (1, 2));
    assert_eq!(values(&source), (1, 2));
    assert_eq!(trace.cloned_drops.get(), 1);
    assert_eq!(trace.source_drops.get(), 0);
    drop(recovered);
    assert_eq!(trace.cloned_drops.get(), 3);
    drop(source);
    assert_eq!(trace.source_drops.get(), 2);
    assert_eq!(trace.cloned_drops.get(), 3);
}

/// Models mutable external state whose captured value must not retain its handles.
struct Quota {
    /// Current value read by the custom capture callback.
    value: Rc<Cell<usize>>,
    /// Counts captures and provides a deterministic callback-failure trigger.
    reads: Rc<Cell<usize>>,
}

/// Captures the current quota and records exactly one callback invocation.
///
/// # Panics
///
/// Panics when the capture counter overflows, as required by the failure test.
fn capture_quota(value: &Quota) -> usize {
    let reads = value
        .reads
        .get()
        .checked_add(1)
        .expect("snapshot capture counter overflow");
    value.reads.set(reads);
    value.value.get()
}

#[derive(Snapshot)]
struct Importer {
    progress: usize,
    history: Vec<String>,
    bookkeeping: usize,
    #[snapshot(with = capture_quota, value = usize)]
    quota: Quota,
    #[snapshot(redact)]
    label: String,
    #[snapshot(omit = "Task lifetime is asserted separately")]
    task: Rc<()>,
}

#[test]
fn capture_and_expected_edits_are_independent_without_retaining_authority() {
    let quota = Rc::new(Cell::new(7));
    let reads = Rc::new(Cell::new(0));
    let task = Rc::new(());
    let mut source = Importer {
        progress: 1,
        history: vec!["first".into()],
        bookkeeping: 3,
        quota: Quota {
            value: Rc::clone(&quota),
            reads: Rc::clone(&reads),
        },
        label: "secret-a".into(),
        task: Rc::clone(&task),
    };
    let captured = source.snapshot();
    assert_eq!(reads.get(), 1);
    source.progress = 2;
    source.history.push("second".into());
    source.bookkeeping = 4;
    quota.set(9);
    let later = source.snapshot();
    assert_eq!(reads.get(), 2);
    assert_eq!(
        (captured.progress, captured.bookkeeping, captured.quota),
        (1, 3, 7)
    );
    assert_eq!(captured.history, ["first"]);
    assert_value_contract(
        &captured,
        "ImporterSnapshot { progress: 1, history: [\"first\"], bookkeeping: 3, quota: 7, label: \"<redacted>\" }",
    );
    let mut different = captured.clone();
    different.progress = 2;
    assert_ne!(captured, different);
    let mut different = captured.clone();
    different.history.push("different".into());
    assert_ne!(captured, different);
    let mut different = captured.clone();
    different.bookkeeping = 4;
    assert_ne!(captured, different);
    let mut different = captured.clone();
    different.quota = 8;
    assert_ne!(captured, different);
    let mut different = captured.clone();
    different.label.push_str("-different");
    assert_ne!(captured, different);
    assert_eq!(format!("{captured:?}"), format!("{different:?}"));
    assert_eq!(captured.label, "secret-a");
    assert_eq!(reads.get(), 2);
    assert_eq!((later.progress, later.bookkeeping, later.quota), (2, 4, 9));
    let mut expected = captured.clone();
    expected.history.push("expected only".into());
    expected.quota = 99;
    expected.bookkeeping = 88;
    assert_eq!(source.history, ["first", "second"]);
    assert_eq!((source.bookkeeping, quota.get()), (4, 9));
    assert_eq!(captured.history, ["first"]);
    assert_eq!(Rc::strong_count(&task), 2);
    assert_eq!(Rc::strong_count(&quota), 2);
    assert!(Rc::ptr_eq(&source.task, &task));
    // A callback failure must propagate without changing source state or
    // retaining authority, in both debug and release profiles.
    reads.set(usize::MAX);
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.snapshot()));
    let _panic = failure.expect_err("quota capture failure must propagate");
    assert_eq!(reads.get(), usize::MAX);
    assert_eq!(source.history, ["first", "second"]);
    assert_eq!(
        (source.progress, source.bookkeeping, quota.get()),
        (2, 4, 9)
    );
    assert_eq!(Rc::strong_count(&task), 2);
    assert_eq!(Rc::strong_count(&quota), 2);
    reads.set(0);
    assert_eq!(source.snapshot().history, ["first", "second"]);
    assert_eq!(reads.get(), 1);
    drop(source);
    assert_eq!(Rc::strong_count(&task), 1);
    assert_eq!(Rc::strong_count(&quota), 1);
    assert_eq!(captured.quota, 7);
}

#[test]
fn redaction_changes_only_debug_not_equality() {
    #[derive(Snapshot)]
    struct Secret(#[snapshot(redact)] String);
    let a = Secret("a".into()).snapshot();
    let b = Secret("b".into()).snapshot();
    assert_ne!(a, b);
    assert_eq!(format!("{a:?}"), "SecretSnapshot(\"<redacted>\")");
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    assert_value_contract(&a, "SecretSnapshot(\"<redacted>\")");
    let mut edited = a.clone();
    edited.0.push_str("-edited");
    assert_ne!(a, edited);
    assert_eq!(a.0, "a");
    assert_eq!(format!("{a:?}"), format!("{edited:?}"));
}

#[test]
fn redacted_values_need_equality_but_not_debug_for_comparisons() {
    #[derive(Clone, PartialEq, Eq)]
    struct SecretValue(usize);
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "snapshot capture callbacks receive the source field by reference"
    )]
    fn capture(value: &usize) -> SecretValue {
        SecretValue(*value)
    }
    #[derive(Snapshot)]
    struct Secret(#[snapshot(with = capture, value = SecretValue, redact)] usize);
    let a = Secret(1).snapshot();
    let b = Secret(2).snapshot();
    assert_ne!(a, b);
    assert_eq!(a, a.clone());
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    assert_value_contract(&a, "SecretSnapshot(\"<redacted>\")");
}

#[test]
fn all_omitted_named_fields_do_not_retain_handles() {
    #[derive(Snapshot)]
    struct Authority {
        #[snapshot(omit = "Ownership is checked explicitly")]
        handle: Rc<()>,
    }
    let source = Authority {
        handle: Rc::new(()),
    };
    let value = source.snapshot();
    assert_eq!(value, AuthoritySnapshot {});
    assert_value_contract(&value, "AuthoritySnapshot");
    assert_eq!(Rc::strong_count(&source.handle), 1);
}

struct MoveOnly(usize);
impl Snapshot for MoveOnly {
    type Value = usize;
    fn snapshot(&self) -> usize {
        self.0
    }
}

fn owned_label(value: &&str) -> String {
    (*value).into()
}

#[derive(Snapshot)]
struct Local<'a, T, const N: usize> {
    item: T,
    #[snapshot(with = owned_label, value = String)]
    label: &'a str,
    bytes: [u8; N],
    #[snapshot(omit = "Local marker is not historical state")]
    marker: PhantomData<Rc<()>>,
}

#[test]
fn value_bounds_and_lifetime_depend_only_on_captured_values() {
    fn transferable<T: Send + Sync + 'static + Clone + std::fmt::Debug + Eq>(_: &T) {}
    let captured = {
        let label = String::from("borrowed");
        Local {
            item: MoveOnly(5),
            label: &label,
            bytes: [],
            marker: PhantomData,
        }
        .snapshot()
    };
    transferable(&captured);
    assert_value_contract(
        &captured,
        "LocalSnapshot { item: 5, label: \"borrowed\", bytes: [] }",
    );
    let mut different = captured.clone();
    different.item = 6;
    assert_ne!(captured, different);
    let mut different = captured.clone();
    different.label.push_str("-edited");
    assert_ne!(captured, different);
    assert_eq!(captured.label, "borrowed");
    assert_eq!(captured, captured.clone());
    assert_eq!(
        (captured.item, captured.label.as_str(), captured.bytes),
        (5, "borrowed", [])
    );
}

struct BareValue;
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "snapshot capture callbacks receive the source field by reference"
)]
fn bare(_: &usize) -> BareValue {
    BareValue
}

#[test]
fn capture_does_not_require_any_optional_output_trait() {
    #[derive(Snapshot)]
    struct Bare(#[snapshot(with = bare, value = BareValue, redact)] usize);
    let captured = Bare(0).snapshot();
    let _: &BareValue = &captured.0;
    assert_eq!(format!("{captured:?}"), "BareSnapshot(\"<redacted>\")");
    assert_debug_contract(&captured, "BareSnapshot(\"<redacted>\")");
    let optional_traits = BareSnapshot(String::from("kept"));
    assert_value_contract(&optional_traits, "BareSnapshot(\"<redacted>\")");
    let mut changed = optional_traits.clone();
    changed.0.push_str("-edited");
    assert_ne!(optional_traits, changed);
    assert_eq!(optional_traits.0, "kept");
}

#[derive(Snapshot)]
enum Event<T> {
    Idle,
    Tuple(
        #[snapshot(omit = "Authority checked separately")] Rc<()>,
        T,
        String,
    ),
    Named {
        value: T,
        #[snapshot(redact)]
        secret: String,
    },
    #[expect(
        clippy::empty_enum_variants_with_brackets,
        reason = "the derive must preserve empty named variant syntax"
    )]
    Empty {},
    #[expect(
        clippy::empty_enum_variants_with_brackets,
        reason = "the derive must preserve empty tuple variant syntax"
    )]
    EmptyTuple(),
    #[cfg(any())]
    Disabled(MissingType),
}

#[expect(
    clippy::too_many_lines,
    reason = "one enum fixture covers every generated shape and partial-clone path"
)]
#[test]
fn enum_case_identity_shapes_and_compacted_fields_are_preserved() {
    assert_eq!(Event::<MoveOnly>::Idle.snapshot(), EventSnapshot::Idle);
    let idle = Event::<MoveOnly>::Idle.snapshot();
    assert_value_contract(&idle, "EventSnapshot::Idle");
    let source = Event::Tuple(Rc::new(()), MoveOnly(8), "x".into());
    let Event::Tuple(authority, _, _) = &source else {
        panic!("tuple source")
    };
    let value = source.snapshot();
    assert_eq!(Rc::strong_count(authority), 1);
    assert_eq!(value, EventSnapshot::Tuple(8, "x".into()));
    assert_eq!(format!("{value:?}"), "EventSnapshot::Tuple(8, \"x\")");
    assert_value_contract(&value, "EventSnapshot::Tuple(8, \"x\")");
    assert_ne!(value, EventSnapshot::Tuple(7, "x".into()));
    assert_ne!(value, EventSnapshot::Tuple(8, "different".into()));
    assert_ne!(idle, value);
    let mut edited_tuple = value.clone();
    let EventSnapshot::Tuple(_, text) = &mut edited_tuple else {
        panic!("cloned tuple changed variant")
    };
    text.push_str("-edited");
    assert_ne!(value, edited_tuple);
    assert_eq!(format!("{value:?}"), "EventSnapshot::Tuple(8, \"x\")");
    let named = Event::Named {
        value: MoveOnly(9),
        secret: "hidden".into(),
    }
    .snapshot();
    assert_eq!(
        format!("{named:?}"),
        "EventSnapshot::Named { value: 9, secret: \"<redacted>\" }"
    );
    assert_value_contract(
        &named,
        "EventSnapshot::Named { value: 9, secret: \"<redacted>\" }",
    );
    assert_ne!(
        named,
        EventSnapshot::Named {
            value: 10,
            secret: "hidden".into(),
        }
    );
    let different_secret = EventSnapshot::Named {
        value: 9,
        secret: "different".into(),
    };
    assert_ne!(named, different_secret);
    assert_eq!(format!("{named:?}"), format!("{different_secret:?}"));
    assert_ne!(value, named);
    let mut edited_named = named.clone();
    let EventSnapshot::Named { secret, .. } = &mut edited_named else {
        panic!("cloned named value changed variant")
    };
    secret.push_str("-edited");
    assert_ne!(named, edited_named);
    assert_eq!(
        named,
        EventSnapshot::Named {
            value: 9,
            secret: "hidden".into(),
        }
    );
    assert_eq!(
        Event::<MoveOnly>::Empty {}.snapshot(),
        EventSnapshot::Empty {}
    );
    assert_eq!(
        Event::<MoveOnly>::EmptyTuple().snapshot(),
        EventSnapshot::EmptyTuple()
    );
    let empty = Event::<MoveOnly>::Empty {}.snapshot();
    let empty_tuple = Event::<MoveOnly>::EmptyTuple().snapshot();
    assert_value_contract(&empty, "EventSnapshot::Empty");
    assert_value_contract(&empty_tuple, "EventSnapshot::EmptyTuple");
    assert_ne!(empty, empty_tuple);

    for named in [false, true] {
        let trace = CloneTrace::new();
        let source: EventSnapshot<CloneProbe<'_>, CloneProbe<'_>, CloneProbe<'_>, CloneProbe<'_>> =
            if named {
                EventSnapshot::Named {
                    value: trace.value(1),
                    secret: trace.value(2),
                }
            } else {
                EventSnapshot::Tuple(trace.value(1), trace.value(2))
            };
        assert_partial_clone(source, &trace, |value| match (named, value) {
            (false, EventSnapshot::Tuple(first, second))
            | (
                true,
                EventSnapshot::Named {
                    value: first,
                    secret: second,
                },
            ) => (first.value, second.value),
            _ => panic!("clone changed the source variant"),
        });
    }

    let empty_variants: [EventSnapshot<
        CloneProbe<'_>,
        CloneProbe<'_>,
        CloneProbe<'_>,
        CloneProbe<'_>,
    >; 3] = [
        EventSnapshot::Idle,
        EventSnapshot::Empty {},
        EventSnapshot::EmptyTuple(),
    ];
    for source in empty_variants {
        assert_eq!(
            std::mem::discriminant(&source.clone()),
            std::mem::discriminant(&source),
        );
    }
}

#[derive(Snapshot)]
struct Unit;
#[derive(Snapshot)]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "the derive must preserve empty named struct syntax"
)]
struct Empty {}
#[derive(Snapshot)]
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "the derive must preserve empty tuple struct syntax"
)]
struct EmptyTuple();
#[derive(Snapshot)]
enum Never {}

#[test]
fn empty_shapes_have_no_dummy_fields() {
    fn accepts_never(_: Option<<Never as Snapshot>::Value>) {}
    assert_eq!(Unit.snapshot(), UnitSnapshot);
    assert_eq!(Empty {}.snapshot(), EmptySnapshot {});
    assert_eq!(EmptyTuple().snapshot(), EmptyTupleSnapshot());
    assert_value_contract(&Unit.snapshot(), "UnitSnapshot");
    assert_value_contract(&Empty {}.snapshot(), "EmptySnapshot");
    assert_value_contract(&EmptyTuple().snapshot(), "EmptyTupleSnapshot");
    accepts_never(None);
    assert_value_contract(&Option::<Never>::None.snapshot(), "None");
    assert_value_contract(&Result::<u8, Never>::Ok(7).snapshot(), "Ok(7)");
}

#[derive(Debug, PartialEq)]
struct Owned {
    count: usize,
}
type OwnedAlias = Owned;
#[derive(Snapshot)]
#[snapshot(into = OwnedAlias)]
struct Custom {
    count: usize,
    #[snapshot(omit = "Handle is asserted separately")]
    handle: Rc<()>,
}

#[test]
fn named_output_uses_the_existing_named_struct_alias() {
    let source = Custom {
        count: 4,
        handle: Rc::new(()),
    };
    assert_eq!(source.snapshot(), Owned { count: 4 });
    assert_eq!(Rc::strong_count(&source.handle), 1);
}

trait Family {
    type Part;
}
#[derive(Snapshot)]
struct Associated<T>
where
    Self: Family,
{
    part: <Self as Family>::Part,
    #[snapshot(omit = "Type marker is not historical state")]
    marker: PhantomData<T>,
}
impl<T> Family for Associated<T> {
    type Part = usize;
}

#[test]
fn associated_payloads_rewrite_self_without_claiming_recursion() {
    let source = Associated::<Rc<()>> {
        part: 17,
        marker: PhantomData,
    };
    assert_eq!(source.snapshot().part, 17);
    let captured = source.snapshot();
    assert_value_contract(&captured, "AssociatedSnapshot { part: 17 }");
    let mut different = captured.clone();
    different.part = 18;
    assert_ne!(captured, different);
}

#[test]
fn custom_captures_run_once_in_declaration_order() {
    struct Read {
        label: &'static str,
        order: Rc<std::cell::RefCell<Vec<&'static str>>>,
    }
    fn capture(value: &Read) -> String {
        value.order.borrow_mut().push(value.label);
        value.label.into()
    }
    #[derive(Snapshot)]
    struct Ordered {
        #[snapshot(with = capture, value = String)]
        first: Read,
        #[snapshot(with = capture, value = String)]
        second: Read,
    }
    let order = Rc::new(std::cell::RefCell::new(Vec::new()));
    let source = Ordered {
        first: Read {
            label: "first",
            order: Rc::clone(&order),
        },
        second: Read {
            label: "second",
            order: Rc::clone(&order),
        },
    };
    let captured = source.snapshot();
    assert_eq!(*order.borrow(), ["first", "second"]);
    assert_eq!(
        (captured.first.as_str(), captured.second.as_str()),
        ("first", "second")
    );
    assert_value_contract(
        &captured,
        "OrderedSnapshot { first: \"first\", second: \"second\" }",
    );
    let mut different = captured.clone();
    different.first.push_str("-edited");
    assert_ne!(captured, different);
    let mut different = captured.clone();
    different.second.push_str("-edited");
    assert_ne!(captured, different);
    assert_eq!(
        (captured.first.as_str(), captured.second.as_str()),
        ("first", "second")
    );
    assert_eq!(*order.borrow(), ["first", "second"]);

    // The second real callback fails after the first has produced a value.
    // A derive that reorders, retries or hides that failure breaks these checks.
    let first_order = Rc::new(std::cell::RefCell::new(Vec::new()));
    let second_order = Rc::new(std::cell::RefCell::new(Vec::new()));
    let blocked = Ordered {
        first: Read {
            label: "first",
            order: Rc::clone(&first_order),
        },
        second: Read {
            label: "second",
            order: Rc::clone(&second_order),
        },
    };
    let held = second_order.borrow();
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| blocked.snapshot()));
    let _panic = failure.expect_err("second capture borrow failure must propagate");
    assert_eq!(*first_order.borrow(), ["first"]);
    assert!(held.is_empty());
    drop(held);
    let recovered = blocked.snapshot();
    assert_eq!(
        (recovered.first.as_str(), recovered.second.as_str()),
        ("first", "second")
    );
    assert_eq!(*first_order.borrow(), ["first", "first"]);
    assert_eq!(*second_order.borrow(), ["second"]);

    let trace = CloneTrace::new();
    let source = OrderedSnapshot {
        first: trace.value(1),
        second: trace.value(2),
    };
    assert_partial_clone(source, &trace, |value| {
        (value.first.value, value.second.value)
    });
}

#[test]
fn nested_derived_capture_matches_an_independent_manual_projection() {
    #[derive(Snapshot)]
    enum Phase {
        Idle,
        Running { count: usize },
    }
    #[derive(Snapshot)]
    struct Nested {
        phase: Phase,
        recent: Vec<String>,
    }
    struct Manual<'a>(&'a Nested);
    impl Snapshot for Manual<'_> {
        type Value = (Option<usize>, Vec<String>);
        fn snapshot(&self) -> Self::Value {
            (
                match self.0.phase {
                    Phase::Idle => None,
                    Phase::Running { count } => Some(count),
                },
                self.0.recent.clone(),
            )
        }
    }
    for phase in [Phase::Idle, Phase::Running { count: 4 }] {
        let source = Nested {
            phase,
            recent: vec!["history".into()],
        };
        let manual = Manual(&source).snapshot();
        let derived = source.snapshot();
        match &derived.phase {
            PhaseSnapshot::Idle => {
                assert_value_contract(&derived.phase, "PhaseSnapshot::Idle");
                assert_value_contract(
                    &derived,
                    "NestedSnapshot { phase: PhaseSnapshot::Idle, recent: [\"history\"] }",
                );
                assert_ne!(derived.phase, PhaseSnapshot::Running { count: 4 });
            }
            PhaseSnapshot::Running { .. } => {
                assert_value_contract(&derived.phase, "PhaseSnapshot::Running { count: 4 }");
                assert_value_contract(
                    &derived,
                    "NestedSnapshot { phase: PhaseSnapshot::Running { count: 4 }, recent: [\"history\"] }",
                );
                assert_ne!(derived.phase, PhaseSnapshot::Running { count: 5 });
                assert_ne!(derived.phase, PhaseSnapshot::Idle);
            }
        }
        let mut changed_phase = derived.clone();
        changed_phase.phase = PhaseSnapshot::Running { count: 5 };
        assert_ne!(derived, changed_phase);
        let mut changed_history = derived.clone();
        changed_history.recent.push("edited clone".into());
        assert_ne!(derived, changed_history);
        assert_eq!(derived.recent, ["history"]);
        assert_eq!(source.recent, ["history"]);
        let count = match derived.phase {
            PhaseSnapshot::Idle => None,
            PhaseSnapshot::Running { count } => Some(count),
        };
        assert_eq!((count, derived.recent), manual);
    }
}

#[test]
fn custom_capture_preserves_declared_value_and_destination_coercions() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    trait Value {
        fn value(&self) -> usize;
    }
    struct Payload(usize, Arc<AtomicUsize>);
    impl Value for Payload {
        fn value(&self) -> usize {
            self.0
        }
    }
    impl Drop for Payload {
        fn drop(&mut self) {
            self.1.fetch_add(1, Ordering::Relaxed);
        }
    }
    #[expect(
        clippy::unnecessary_box_returns,
        reason = "the boxed concrete payload tests coercion to the declared trait object value"
    )]
    fn capture_object(value: &(usize, Arc<AtomicUsize>)) -> Box<Payload> {
        Box::new(Payload(value.0, Arc::clone(&value.1)))
    }
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "snapshot capture callbacks receive the source field by reference"
    )]
    #[expect(
        clippy::unnecessary_box_returns,
        reason = "the boxed array tests coercion to the declared slice value"
    )]
    #[expect(
        clippy::ref_option,
        reason = "snapshot capture callbacks receive the source field by reference"
    )]
    fn capture_bytes(value: &Option<u8>) -> Box<[u8; 2]> {
        Box::new([value.expect("snapshot byte capture failed"), 9])
    }
    struct Destination {
        object: Box<dyn Value>,
        bytes: Box<[u8]>,
    }
    #[derive(Snapshot)]
    #[snapshot(into = Destination)]
    struct Source {
        #[snapshot(with = capture_object, value = Box<dyn Value + Send>)]
        object: (usize, Arc<AtomicUsize>),
        #[snapshot(with = capture_bytes, value = Box<[u8]>)]
        bytes: Option<u8>,
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let mut source = Source {
        object: (7, Arc::clone(&drops)),
        bytes: None,
    };
    assert!(std::panic::catch_unwind(|| source.snapshot()).is_err());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert_eq!(Arc::strong_count(&drops), 2);
    source.bytes = Some(5);
    let captured = source.snapshot();
    assert_eq!(captured.object.value(), 7);
    assert_eq!(captured.bytes.as_ref(), [5, 9]);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    drop(captured);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    assert_eq!(Arc::strong_count(&drops), 2);
}

#[test]
fn custom_capture_value_rewrites_generic_source_self() {
    trait Projection {
        type Value;
    }
    #[derive(Snapshot)]
    struct Source<T>
    where
        Self: Projection<Value = usize>,
    {
        #[snapshot(with = Self::capture, value = <Self as Projection>::Value)]
        item: T,
    }
    impl<T> Projection for Source<T> {
        type Value = usize;
    }
    impl<T> Source<T> {
        fn capture(_: &T) -> usize {
            17
        }
    }
    assert_eq!(Source { item: Rc::new(()) }.snapshot().item, 17);
    let captured = Source { item: Rc::new(()) }.snapshot();
    assert_value_contract(&captured, "SourceSnapshot { item: 17 }");
    let mut different = captured.clone();
    different.item = 18;
    assert_ne!(captured, different);
}

#[test]
fn colliding_type_parameter_preserves_const_generic_array() {
    #[derive(Snapshot)]
    struct Source<Source, const N: usize> {
        item: Source,
        bytes: [u8; N],
    }
    let mut source = Source::<usize, 2> {
        item: 7,
        bytes: [3, 5],
    };
    let captured = source.snapshot();
    source.item = 11;
    source.bytes = [13, 17];
    assert_eq!((captured.item, captured.bytes), (7, [3, 5]));
    let later = source.snapshot();
    assert_eq!((later.item, later.bytes), (11, [13, 17]));
    assert_value_contract(&captured, "SourceSnapshot { item: 7, bytes: [3, 5] }");
    assert_ne!(captured, later);
    let mut different = captured.clone();
    different.item = 8;
    assert_ne!(captured, different);
    let mut different = captured.clone();
    different.bytes[0] = 6;
    assert_ne!(captured, different);
    assert_eq!((captured.item, captured.bytes), (7, [3, 5]));
}

#[test]
fn colliding_type_parameter_preserves_same_spelling_value_constant() {
    #[expect(
        non_upper_case_globals,
        reason = "the value must collide with the type name"
    )]
    const Source: usize = 2;

    #[derive(Snapshot)]
    struct Source<Source> {
        item: Source,
        bytes: [u8; Source],
    }
    let captured = Source {
        item: 19usize,
        bytes: [23, 29],
    }
    .snapshot();
    assert_eq!((captured.item, captured.bytes), (19, [23, 29]));
    assert_value_contract(&captured, "SourceSnapshot { item: 19, bytes: [23, 29] }");
    let mut different = captured.clone();
    different.item = 20;
    assert_ne!(captured, different);
    let mut different = captured.clone();
    different.bytes[0] = 24;
    assert_ne!(captured, different);
    assert_eq!((captured.item, captured.bytes), (19, [23, 29]));
}

#[test]
fn colliding_type_parameter_preserves_nested_const_function_projection() {
    trait Width {
        const LEN: usize;
    }
    struct Two;
    impl Width for Two {
        const LEN: usize = 2;
    }

    #[derive(Snapshot)]
    struct Source<Source> {
        item: Source,
        bytes: [u8; {
            const fn width<Source: Width>() -> usize {
                Source::LEN
            }
            width::<Two>()
        }],
    }
    let captured = Source {
        item: 31usize,
        bytes: [37, 41],
    }
    .snapshot();
    assert_eq!((captured.item, captured.bytes), (31, [37, 41]));
    assert_value_contract(&captured, "SourceSnapshot { item: 31, bytes: [37, 41] }");
    let mut different = captured.clone();
    different.item = 32;
    assert_ne!(captured, different);
    let mut different = captured.clone();
    different.bytes[0] = 38;
    assert_ne!(captured, different);
    assert_eq!((captured.item, captured.bytes), (31, [37, 41]));
}

#[test]
fn colliding_type_parameter_preserves_local_enum_scope() {
    #[derive(Snapshot)]
    struct Source<Source> {
        bytes: [u8; {
            const WIDTH: usize = Source::Width as usize;
            #[cfg(test)]
            enum Source {
                Width = 2,
            }
            WIDTH
        }],
        item: Source,
    }
    let captured = Source {
        bytes: [43, 47],
        item: 53usize,
    }
    .snapshot();
    assert_eq!((captured.bytes, captured.item), ([43, 47], 53));
    assert_value_contract(&captured, "SourceSnapshot { bytes: [43, 47], item: 53 }");
    let mut different = captured.clone();
    different.item = 54;
    assert_ne!(captured, different);
    let mut different = captured.clone();
    different.bytes[0] = 44;
    assert_ne!(captured, different);
    assert_eq!((captured.bytes, captured.item), ([43, 47], 53));
}

#[test]
fn colliding_type_parameter_preserves_nested_expression_namespaces() {
    trait Width {
        const LEN: usize;
    }
    struct Two;
    impl Width for Two {
        const LEN: usize = 2;
    }
    mod values {
        #[expect(
            non_upper_case_globals,
            reason = "the value import must collide with the type parameter"
        )]
        pub const Source: usize = 0;
    }

    #[derive(Snapshot)]
    struct Source<Source> {
        item: Source,
        bytes: [u8; {
            const fn width<Source: Width>() -> usize {
                #[cfg(any())]
                enum Source {}
                use values::Source;

                let local_width = {
                    const WIDTH: usize = Source::Width as usize;
                    enum Source {
                        Width = 1,
                    }
                    WIDTH
                };
                Source::LEN + local_width + Source - 1
            }
            width::<Two>()
        }],
    }
    let captured = Source {
        item: 59usize,
        bytes: [61, 67],
    }
    .snapshot();
    assert_eq!((captured.item, captured.bytes), (59, [61, 67]));
    assert_value_contract(&captured, "SourceSnapshot { item: 59, bytes: [61, 67] }");
    let mut different = captured.clone();
    different.item = 60;
    assert_ne!(captured, different);
    let mut different = captured.clone();
    different.bytes[0] = 62;
    assert_ne!(captured, different);
    assert_eq!((captured.item, captured.bytes), (59, [61, 67]));
}

#[test]
fn colliding_type_parameter_preserves_imported_type_in_const_expression() {
    #[derive(Snapshot)]
    struct Source<Source> {
        item: Source,
        bytes: [u8; {
            mod size {
                pub struct Source;
                impl Source {
                    pub const LEN: usize = 2;
                }
            }
            use size::Source;
            core::mem::size_of::<Source>() + Source::LEN
        }],
    }
    let captured = Source {
        item: 71usize,
        bytes: [73, 79],
    }
    .snapshot();
    assert_eq!((captured.item, captured.bytes), (71, [73, 79]));
    assert_value_contract(&captured, "SourceSnapshot { item: 71, bytes: [73, 79] }");
    let mut different = captured.clone();
    different.item = 72;
    assert_ne!(captured, different);
    let mut different = captured.clone();
    different.bytes[0] = 74;
    assert_ne!(captured, different);
    assert_eq!((captured.item, captured.bytes), (71, [73, 79]));
}

#[test]
fn colliding_type_parameter_preserves_absolute_crate_path() {
    #[expect(
        non_camel_case_types,
        reason = "the type and its parameter must collide with the absolute crate path"
    )]
    #[derive(Snapshot)]
    #[snapshot(name = History)]
    struct std<std> {
        item: std,
        text: ::std::string::String,
    }
    let mut source = std {
        item: 7usize,
        text: "before".to_owned(),
    };
    let captured = source.snapshot();
    source.item = 11;
    source.text.push_str(" after");
    assert_eq!((captured.item, captured.text.as_str()), (7, "before"));
    let later = source.snapshot();
    assert_eq!((later.item, later.text.as_str()), (11, "before after"));
    assert_value_contract(&captured, "History { item: 7, text: \"before\" }");
    assert_ne!(captured, later);
    let mut different = captured.clone();
    different.item = 8;
    assert_ne!(captured, different);
    let mut different = captured.clone();
    different.text.push('!');
    assert_ne!(captured, different);
    assert_eq!((captured.item, captured.text.as_str()), (7, "before"));
}
