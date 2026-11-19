//! Compares selected typed structures and writes explicitly redacted graph views.
//!
//! This small format is not reflection or a universal pretty-printer. Callers
//! project independently captured leaves into fields, cases, sequences, maps
//! and represented values. Omitting a value does not earn exhaustive coverage.
//! Actual graph equality and selected view equality are separate report axes:
//! neither redaction nor equal representation can hide a selected leaf change.
//!
//! Work units charge library traversal/matching attempts and hook dispatch, not
//! CPU time inside user equality, projection, redaction or writer hooks. Those
//! hooks, their allocations and the construction/destruction of returned views
//! remain caller-controlled and potentially unbounded. Panics propagate.

use core::fmt;
use std::backtrace::Backtrace;

use super::{CapturedGraph, CapturedNode};

/// Selects typed values and shape without implicit domain rendering.
///
/// Values and keys may be owned or borrowed. No `Clone`, `Debug`, `Send`, `Sync`
/// or `'static` bound is imposed by writing. Comparison adds only the explicit
/// equality bounds of its entrypoint. Field labels and case names are selected
/// identities, compared before redaction; map order is not semantic. Sequence
/// and field order are semantic. `Represented` compares only its selected inner
/// view, never the rendered text. Reference indices belong to one capture.
/// A single-node graph suffices for a plain tree.
#[derive(Clone)]
pub enum StructuralView<Key, Value> {
    /// Selects one typed value.
    Value(Value),
    /// Selects unique, ordered field identities and their values.
    Fields(Vec<(String, Self)>),
    /// Selects a case identity and an optional payload.
    Case(String, Option<Box<Self>>),
    /// Selects values in semantic order.
    Sequence(Vec<Self>),
    /// Selects unique typed keys; entry order affects writing only.
    Map(Vec<(Key, Self)>),
    /// Selects an explicit alternate representation, not additional coverage.
    Represented(Box<Self>),
    /// Refers to a capture-local node; repeats and cycles are finite.
    Reference(usize),
}

impl<K, V> fmt::Debug for StructuralView<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Value(_) => "Value",
            Self::Fields(_) => "Fields",
            Self::Case(_, _) => "Case",
            Self::Sequence(_) => "Sequence",
            Self::Map(_) => "Map",
            Self::Represented(_) => "Represented",
            Self::Reference(_) => "Reference",
        };
        f.debug_struct(name).finish_non_exhaustive()
    }
}

/// Borrows one selected token for caller-controlled rendering and redaction.
///
/// Every label, key and value passes through this hook before being written.
/// There is no `Debug` fallback. Returned text is used verbatim; the caller owns
/// escaping and redaction policy. Debug of the token itself exposes no data.
pub enum StructuralToken<'a, Key, Value> {
    /// Borrows a field label or case name.
    Label(&'a str),
    /// Borrows a typed map key.
    Key(&'a Key),
    /// Borrows a typed selected value.
    Value(&'a Value),
}

impl<K, V> fmt::Debug for StructuralToken<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StructuralToken").finish_non_exhaustive()
    }
}

/// Bounds library work, nested descent and written Unicode scalars separately.
///
/// Root depth is zero. Each structural child or followed reference adds one.
/// Zero work prevents projection. Zero output prevents redaction dispatch and
/// writing, including empty rendered tokens. Diff does not use the output cap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiagnosticLimits {
    /// Maximum charged traversal steps and callback dispatches.
    work: usize,
    /// Maximum child or reference depth, with each root at zero.
    depth: usize,
    /// Maximum Unicode scalars accepted by successful writer calls.
    output: usize,
}

impl DiagnosticLimits {
    /// Creates explicit limits, including zero limits.
    #[must_use]
    pub const fn new(work: usize, depth: usize, output: usize) -> Self {
        Self {
            work,
            depth,
            output,
        }
    }
}

/// Classifies a selected-view change without retaining private domain data.
///
/// Entries describe encountered changes, not unique paths or exhaustive state
/// coverage. No names, keys or values are retained in this metadata report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewChange {
    /// Selected shapes differ.
    Shape,
    /// Typed selected values differ.
    Value,
    /// A field was added.
    FieldInserted,
    /// A field was removed.
    FieldRemoved,
    /// Relative order of common field identities changed.
    FieldOrder,
    /// Case identity or payload presence changed.
    Case,
    /// Sequence length changed.
    SequenceLength,
    /// A typed map key was added.
    MapInserted,
    /// A typed map key was removed.
    MapRemoved,
    /// Selected references have different logical origins.
    Reference,
}

/// Separates actual captured semantics from selected representation changes.
#[derive(Debug)]
pub struct StructuralDiff {
    /// Inequality of captured values and topology before view projection.
    semantic_changed: bool,
    /// Selected-view differences, with no retained names, keys or values.
    changes: Vec<ViewChange>,
}

impl StructuralDiff {
    /// Reports inequality of actual captured origins, leaves or ordered edges.
    #[must_use]
    pub const fn semantic_changed(&self) -> bool {
        self.semantic_changed
    }

    /// Reports inequality of typed selected representation, not rendered text.
    #[must_use]
    pub fn view_changed(&self) -> bool {
        !self.changes.is_empty()
    }

    /// Borrows metadata-only selected changes in traversal order.
    #[must_use]
    pub fn changes(&self) -> &[ViewChange] {
        &self.changes
    }
}

/// Classifies incomplete work without retaining caller-owned domain data.
#[derive(Debug)]
enum ErrorKind {
    /// The next charged operation exceeds the work budget.
    Work,
    /// A child or reference exceeds the permitted depth.
    Depth,
    /// Redaction or writing exceeds the Unicode-scalar budget.
    Output,
    /// A selected identity repeats or a reference lies outside the graph.
    InvalidView,
    /// The writer rejected a scalar; its original formatting error is retained.
    Writer(fmt::Error),
}

/// Reports incomplete diagnostics without claiming equality or rolling back output.
#[derive(Debug)]
pub struct DiagnosticError {
    /// Failure class and, for writer errors, the retained cause.
    kind: ErrorKind,
    /// Work charged before the failure.
    work: usize,
    /// Scalars accepted by successful writer calls before the failure.
    output: usize,
    /// Failure-site trace, captured only when an error is created.
    backtrace: Backtrace,
}

impl DiagnosticError {
    /// Reports exhaustion before a library operation or hook dispatch.
    #[must_use]
    pub const fn is_work_limit(&self) -> bool {
        matches!(self.kind, ErrorKind::Work)
    }

    /// Reports an attempted descent beyond the allowed depth.
    #[must_use]
    pub const fn is_depth_limit(&self) -> bool {
        matches!(self.kind, ErrorKind::Depth)
    }

    /// Reports exhaustion of the Unicode-scalar output budget.
    #[must_use]
    pub const fn is_output_limit(&self) -> bool {
        matches!(self.kind, ErrorKind::Output)
    }

    /// Reports an invalid reference or duplicate selected field/key identity.
    #[must_use]
    pub const fn is_invalid_view(&self) -> bool {
        matches!(self.kind, ErrorKind::InvalidView)
    }

    /// Reports a preserved upstream formatting failure.
    #[must_use]
    pub const fn is_writer_failure(&self) -> bool {
        matches!(self.kind, ErrorKind::Writer(_))
    }

    /// Returns charged work units before failure.
    #[must_use]
    pub const fn work_used(&self) -> usize {
        self.work
    }

    /// Returns successfully written Unicode scalars before failure.
    ///
    /// A failing writer can retain a partial prefix from its last call. That
    /// prefix is not included because the call did not report success.
    #[must_use]
    pub const fn output_written(&self) -> usize {
        self.output
    }

    /// Borrows the captured failure backtrace.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Display for DiagnosticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Structural diagnostics failed: {:?} (work {}, output {}).\n{}",
            self.kind, self.work, self.output, self.backtrace
        )
    }
}

impl std::error::Error for DiagnosticError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ErrorKind::Writer(error) => Some(error),
            _ => None,
        }
    }
}

/// Charges library work before dispatch and output after successful writes.
struct Budget {
    /// Independent work, depth and output limits for this operation.
    limits: DiagnosticLimits,
    /// Completed charges; never greater than the work limit.
    work: usize,
    /// Scalars accepted by the writer; never greater than the output limit.
    output: usize,
}

impl Budget {
    /// Starts an operation without charging work or creating a backtrace.
    fn new(limits: DiagnosticLimits) -> Self {
        Self {
            limits,
            work: 0,
            output: 0,
        }
    }

    /// Captures the current counters and a trace without caller-owned values.
    fn error(&self, kind: ErrorKind) -> DiagnosticError {
        DiagnosticError {
            kind,
            work: self.work,
            output: self.output,
            backtrace: Backtrace::capture(),
        }
    }

    /// Reserves one unit before the corresponding operation or callback.
    fn charge(&mut self) -> Result<(), DiagnosticError> {
        if self.work == self.limits.work {
            return Err(self.error(ErrorKind::Work));
        }
        self.work += 1;
        Ok(())
    }

    /// Rejects depth beyond the cap without charging a second work unit.
    fn depth(&self, depth: usize) -> Result<(), DiagnosticError> {
        if depth > self.limits.depth {
            Err(self.error(ErrorKind::Depth))
        } else {
            Ok(())
        }
    }

    /// Charges a child edge before checking its incremented depth.
    fn descend(&mut self, depth: usize) -> Result<usize, DiagnosticError> {
        self.charge()?;
        // Depth starts at zero and every increment consumes work first;
        // this successful charge guarantees depth is below usize::MAX.
        let next = depth + 1;
        self.depth(next)?;
        Ok(next)
    }

    /// Writes one scalar at a time without scanning unaccepted text first.
    fn write(
        &mut self,
        text: &str,
        writer: &mut (impl fmt::Write + ?Sized),
    ) -> Result<(), DiagnosticError> {
        // Incremental scalar iteration never scans the whole returned string.
        for scalar in text.chars() {
            if self.output == self.limits.output {
                return Err(self.error(ErrorKind::Output));
            }
            self.charge()?;
            let mut utf8 = [0; 4];
            writer
                .write_str(scalar.encode_utf8(&mut utf8))
                .map_err(|error| self.error(ErrorKind::Writer(error)))?;
            self.output += 1;
        }
        Ok(())
    }
}

/// Searches in encounter order, charging each equality call before dispatch.
fn seen<T: PartialEq>(
    items: &[T],
    value: &T,
    budget: &mut Budget,
) -> Result<bool, DiagnosticError> {
    for item in items {
        budget.charge()?;
        if item == value {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Projects every captured node, then validates all views before output.
fn prepare<'a, O, L, K: Eq, V>(
    graph: &'a CapturedGraph<O, L>,
    budget: &mut Budget,
    project: &mut impl FnMut(&'a CapturedNode<O, L>) -> StructuralView<K, V>,
) -> Result<Vec<StructuralView<K, V>>, DiagnosticError> {
    let mut views = Vec::new();
    let result = (|| {
        for node in graph.nodes() {
            budget.charge()?; // Projection dispatch, including each graph node.
            views.push(project(node));
        }
        // Successful validation empties this borrowed stack. Keep its capacity
        // across nodes, then release it before rendering or returning owned views.
        let mut pending = Vec::with_capacity(1);
        for view in &views {
            validate(view, views.len(), budget, &mut pending)?;
        }
        drop(pending);
        Ok(())
    })();
    match result {
        Ok(()) => Ok(views),
        Err(error) => {
            // Keep the error local if a selected value panics during cleanup.
            drop(views);
            Err(error)
        }
    }
}

/// Checks selected identities, reference bounds and depth without rendering.
///
/// The caller supplies an empty stack, which successful validation empties.
/// An error leaves only borrowed entries for the caller to discard. References
/// are checked here, then followed with separate depth charges by each consumer.
fn validate<'a, K: Eq, V>(
    view: &'a StructuralView<K, V>,
    nodes: usize,
    budget: &mut Budget,
    pending: &mut Vec<(&'a StructuralView<K, V>, usize)>,
) -> Result<(), DiagnosticError> {
    // Root depth is zero; descend checks every child depth before enqueueing.
    pending.push((view, 0));
    while let Some((view, depth)) = pending.pop() {
        budget.charge()?;
        match view {
            StructuralView::Value(_) => {}
            StructuralView::Reference(index) => {
                if *index >= nodes {
                    return Err(budget.error(ErrorKind::InvalidView));
                }
            }
            StructuralView::Fields(fields) => {
                for (index, (name, child)) in fields.iter().enumerate() {
                    for (prior, _) in &fields[..index] {
                        budget.charge()?;
                        if prior == name {
                            return Err(budget.error(ErrorKind::InvalidView));
                        }
                    }
                    pending.push((child, budget.descend(depth)?));
                }
            }
            StructuralView::Map(entries) => {
                for (index, (key, child)) in entries.iter().enumerate() {
                    for (prior, _) in &entries[..index] {
                        budget.charge()?;
                        if prior == key {
                            return Err(budget.error(ErrorKind::InvalidView));
                        }
                    }
                    pending.push((child, budget.descend(depth)?));
                }
            }
            StructuralView::Sequence(children) => {
                for child in children {
                    pending.push((child, budget.descend(depth)?));
                }
            }
            StructuralView::Case(_, child) => {
                if let Some(child) = child {
                    pending.push((child, budget.descend(depth)?));
                }
            }
            StructuralView::Represented(child) => pending.push((child, budget.descend(depth)?)),
        }
    }
    Ok(())
}

/// Writes a small selected format through explicit redaction and bounded traversal.
///
/// Fields/maps use `{label: value}`, sequences use `[value]`, cases use
/// `case(payload)` or `case`, and represented values use their inner view.
/// References use `&index(view)` on first encounter and `*index` on repeats.
/// Root is implicitly registered as index zero. These indices are local output
/// markers, never cross-capture identities. Caller text is used verbatim.
/// All node projections and selected identities are validated before output.
/// Limits cover work/depth and incremental Unicode output, not hook internals.
/// The writer can be concrete or a borrowed `dyn fmt::Write` trait object.
///
/// # Examples
/// ```
/// use rca_core::diagnostics::{capture_graph, CaptureLimits, NodeCapture,
///     DiagnosticLimits, StructuralView, write_graph_view};
/// let graph = capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(7, vec![]))?;
/// let mut text = String::new();
/// write_graph_view(&graph, DiagnosticLimits::new(100, 4, 40),
///     |node| StructuralView::<(), _>::Value(node.leaves()),
///     |_| "[private]".to_owned(), &mut text)?;
/// assert_eq!(text, "[private]");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
/// Returns errors for work/depth/output exhaustion, duplicate field/key
/// identities, invalid references or upstream writer failure. Written prefixes
/// are retained, including prefixes written by a failing writer call.
///
/// # Panics
/// Caller projection, equality, redaction and writer panics propagate.
/// Selected-value and owned-callback destructors run before return; their panics
/// also propagate.
pub fn write_graph_view<'a, O, L, K: Eq, V>(
    graph: &'a CapturedGraph<O, L>,
    limits: DiagnosticLimits,
    mut project: impl FnMut(&'a CapturedNode<O, L>) -> StructuralView<K, V>,
    mut redact: impl FnMut(StructuralToken<'_, K, V>) -> String,
    writer: &mut (impl fmt::Write + ?Sized),
) -> Result<(), DiagnosticError> {
    let mut views = Vec::new();
    let result = (|| {
        let mut budget = Budget::new(limits);
        views = prepare(graph, &mut budget, &mut project)?;
        let mut pending = vec![WriteTask::View(&views[0], 0)];
        let mut visited = vec![0];
        while let Some(task) = pending.pop() {
            budget.charge()?;
            match task {
                WriteTask::Text(text) => budget.write(text, writer)?,
                WriteTask::Token(token) => {
                    if budget.output == limits.output {
                        return Err(budget.error(ErrorKind::Output));
                    }
                    budget.charge()?; // Redaction dispatch.
                    budget.write(&redact(token), writer)?;
                }
                WriteTask::View(view, depth) => {
                    // Root is zero; descend checked every other queued depth.
                    match view {
                        StructuralView::Value(value) => {
                            pending.push(WriteTask::Token(StructuralToken::Value(value)));
                        }
                        StructuralView::Fields(fields) => {
                            pending.push(WriteTask::Text("}"));
                            for (index, (name, child)) in fields.iter().enumerate().rev() {
                                push_entry(
                                    &mut pending,
                                    child,
                                    budget.descend(depth)?,
                                    StructuralToken::Label(name),
                                    index,
                                );
                            }
                            pending.push(WriteTask::Text("{"));
                        }
                        StructuralView::Map(entries) => {
                            pending.push(WriteTask::Text("}"));
                            for (index, (key, child)) in entries.iter().enumerate().rev() {
                                push_entry(
                                    &mut pending,
                                    child,
                                    budget.descend(depth)?,
                                    StructuralToken::Key(key),
                                    index,
                                );
                            }
                            pending.push(WriteTask::Text("{"));
                        }
                        StructuralView::Sequence(children) => {
                            pending.push(WriteTask::Text("]"));
                            for (index, child) in children.iter().enumerate().rev() {
                                pending.push(WriteTask::View(child, budget.descend(depth)?));
                                if index != 0 {
                                    pending.push(WriteTask::Text(", "));
                                }
                            }
                            pending.push(WriteTask::Text("["));
                        }
                        StructuralView::Case(name, child) => {
                            if let Some(child) = child {
                                pending.push(WriteTask::Text(")"));
                                pending.push(WriteTask::View(child, budget.descend(depth)?));
                                pending.push(WriteTask::Text("("));
                            }
                            pending.push(WriteTask::Token(StructuralToken::Label(name)));
                        }
                        StructuralView::Represented(child) => {
                            pending.push(WriteTask::View(child, budget.descend(depth)?));
                        }
                        StructuralView::Reference(index) => {
                            budget.charge()?; // Reference traversal.
                            if seen(&visited, index, &mut budget)? {
                                budget.write("*", writer)?;
                                budget.write(&index.to_string(), writer)?;
                            } else {
                                visited.push(*index);
                                budget.write("&", writer)?;
                                budget.write(&index.to_string(), writer)?;
                                budget.write("(", writer)?;
                                pending.push(WriteTask::Text(")"));
                                pending
                                    .push(WriteTask::View(&views[*index], budget.descend(depth)?));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    })();
    // A cleanup panic must drop a prepared error before it enters the return slot.
    drop(views);
    drop(redact);
    drop(project);
    result
}

/// Holds borrowed output work in reverse order for iterative traversal.
enum WriteTask<'a, K, V> {
    /// Expands a selected view whose depth was checked before insertion.
    View(&'a StructuralView<K, V>, usize),
    /// Writes fixed punctuation, never caller-owned text.
    Text(&'static str),
    /// Passes a caller-selected value through the redaction hook.
    Token(StructuralToken<'a, K, V>),
}

/// Queues an entry backwards so its separator and label precede its value.
fn push_entry<'a, K, V>(
    pending: &mut Vec<WriteTask<'a, K, V>>,
    child: &'a StructuralView<K, V>,
    depth: usize,
    token: StructuralToken<'a, K, V>,
    index: usize,
) {
    pending.push(WriteTask::View(child, depth));
    pending.push(WriteTask::Text(": "));
    pending.push(WriteTask::Token(token));
    if index != 0 {
        pending.push(WriteTask::Text(", "));
    }
}

/// Compares captured topology and leaves with charged, cycle-safe pair traversal.
fn semantic_diff<O: Eq, L: PartialEq>(
    left: &CapturedGraph<O, L>,
    right: &CapturedGraph<O, L>,
    budget: &mut Budget,
) -> Result<bool, DiagnosticError> {
    let mut changed = left.nodes().len() != right.nodes().len();
    // Root depth is zero; descend checks every edge depth before enqueueing.
    let mut pending = vec![((0, 0), 0)];
    let mut visited = Vec::new();
    while let Some((pair @ (a, b), depth)) = pending.pop() {
        budget.charge()?; // Every pair traversal, including repeated pairs.
        if seen(&visited, &pair, budget)? {
            continue;
        }
        visited.push(pair);
        let left = &left.nodes()[a];
        let right = &right.nodes()[b];
        budget.charge()?;
        changed |= left.origin() != right.origin();
        budget.charge()?;
        changed |= left.leaves() != right.leaves();
        changed |= left.edges().len() != right.edges().len();
        for (a, b) in left.edges().iter().zip(right.edges()) {
            budget.charge()?; // Edge matching/equality dispatch.
            changed |= a.label() != b.label();
            pending.push(((a.target(), b.target()), budget.descend(depth)?));
        }
    }
    Ok(changed)
}

/// Compares captured semantics and selected typed shape without rendering.
///
/// Actual graph origins/leaves/ordered edges are compared by a charged iterative
/// pair walk, not an unbounded call to graph `PartialEq`. Selected fields align
/// by identity, with common-field order changes reported separately. Maps align
/// by typed keys, independent of order. Cases preserve payload absence versus
/// a present explicit `None` case. Every selected reference uses logical origins
/// rather than comparing capture-local indices. Repeated pairs terminate.
/// Reports are metadata only; omitting a private field cannot hide a change in
/// the actual captured leaves. Equality hooks remain internally unbounded.
/// Output limits are unused because this function emits no text.
///
/// # Examples
/// ```
/// use rca_core::diagnostics::{capture_graph, CaptureLimits, NodeCapture,
///     DiagnosticLimits, StructuralView, diff_graph_views};
/// let before = capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(7, vec![]))?;
/// let after = capture_graph(0, CaptureLimits::new(1, 0), |_| NodeCapture::new(8, vec![]))?;
/// let report = diff_graph_views(&before, &after, DiagnosticLimits::new(100, 0, 0),
///     |_| StructuralView::<(), ()>::Fields(vec![]))?;
/// assert!(report.semantic_changed());
/// assert!(!report.view_changed()); // Deliberate omission is not equality credit.
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
/// Returns an error, never an equal/incomplete report, on work/depth exhaustion,
/// duplicate field/key identities or invalid references.
///
/// # Panics
/// Caller equality and projection panics propagate.
/// Selected-value and owned-callback destructors run before return; their panics
/// also propagate.
pub fn diff_graph_views<'a, O: Eq, L: PartialEq, K: Eq, V: PartialEq>(
    left: &'a CapturedGraph<O, L>,
    right: &'a CapturedGraph<O, L>,
    limits: DiagnosticLimits,
    mut project: impl FnMut(&'a CapturedNode<O, L>) -> StructuralView<K, V>,
) -> Result<StructuralDiff, DiagnosticError> {
    let mut a = Vec::new();
    let mut b = Vec::new();
    let result = (|| {
        let mut budget = Budget::new(limits);
        let semantic_changed = semantic_diff(left, right, &mut budget)?;
        a = prepare(left, &mut budget, &mut project)?;
        b = prepare(right, &mut budget, &mut project)?;
        let mut report = StructuralDiff {
            semantic_changed,
            changes: Vec::new(),
        };
        // Root depth is zero; descend checks child and reference depths before enqueueing.
        let mut pending = vec![(&a[0], &b[0], 0)];
        let mut visited = vec![(0, 0)];
        while let Some((a_view, b_view, depth)) = pending.pop() {
            budget.charge()?;
            match (a_view, b_view) {
                (StructuralView::Value(a), StructuralView::Value(b)) => {
                    budget.charge()?;
                    if a != b {
                        report.changes.push(ViewChange::Value);
                    }
                }
                (StructuralView::Fields(a), StructuralView::Fields(b)) => {
                    align(a, b, &mut pending, depth, &mut budget, &mut report, true)?;
                }
                (StructuralView::Map(a), StructuralView::Map(b)) => {
                    align(a, b, &mut pending, depth, &mut budget, &mut report, false)?;
                }
                (StructuralView::Sequence(a), StructuralView::Sequence(b)) => {
                    if a.len() != b.len() {
                        report.changes.push(ViewChange::SequenceLength);
                    }
                    for (a, b) in a.iter().zip(b) {
                        pending.push((a, b, budget.descend(depth)?));
                    }
                }
                (StructuralView::Case(a, a_child), StructuralView::Case(b, b_child)) => {
                    budget.charge()?;
                    if a != b || a_child.is_some() != b_child.is_some() {
                        report.changes.push(ViewChange::Case);
                    }
                    if let (Some(a), Some(b)) = (a_child, b_child) {
                        pending.push((a, b, budget.descend(depth)?));
                    }
                }
                (StructuralView::Represented(a), StructuralView::Represented(b)) => {
                    pending.push((a, b, budget.descend(depth)?));
                }
                (StructuralView::Reference(a_index), StructuralView::Reference(b_index)) => {
                    budget.charge()?;
                    budget.charge()?; // Logical origin equality hook.
                    if left.nodes()[*a_index].origin() != right.nodes()[*b_index].origin() {
                        report.changes.push(ViewChange::Reference);
                    }
                    let pair = (*a_index, *b_index);
                    if !seen(&visited, &pair, &mut budget)? {
                        visited.push(pair);
                        pending.push((&a[*a_index], &b[*b_index], budget.descend(depth)?));
                    }
                }
                _ => report.changes.push(ViewChange::Shape),
            }
        }
        Ok(report)
    })();
    // Preserve reverse view order while the report or error remains a local owner.
    drop(b);
    drop(a);
    drop(project);
    result
}

/// Borrows paired views at a checked depth without owning their domain data.
type ViewPair<'a, K, V> = (&'a StructuralView<K, V>, &'a StructuralView<K, V>, usize);

/// Aligns unique identities and records field order separately from map order.
///
/// Linear matching preserves `Eq`-only keys. Every scan is charged before
/// equality; adding a hash index would impose a new bound on callers.
fn align<'a, Identity: Eq, K, V>(
    left: &'a [(Identity, StructuralView<K, V>)],
    right: &'a [(Identity, StructuralView<K, V>)],
    pending: &mut Vec<ViewPair<'a, K, V>>,
    depth: usize,
    budget: &mut Budget,
    report: &mut StructuralDiff,
    fields: bool,
) -> Result<(), DiagnosticError> {
    let mut matched = Vec::new();
    let mut previous = None;
    let mut reordered = false;
    for (identity, a) in left {
        budget.charge()?;
        let mut found = None;
        for (index, (candidate, b)) in right.iter().enumerate() {
            budget.charge()?; // Each matching attempt and identity equality hook.
            if identity == candidate {
                found = Some((index, b));
                break;
            }
        }
        if let Some((index, b)) = found {
            reordered |= previous.is_some_and(|prior| prior > index);
            previous = Some(index);
            matched.push(index);
            pending.push((a, b, budget.descend(depth)?));
        } else {
            report.changes.push(if fields {
                ViewChange::FieldRemoved
            } else {
                ViewChange::MapRemoved
            });
        }
    }
    for index in 0..right.len() {
        budget.charge()?;
        if !seen(&matched, &index, budget)? {
            report.changes.push(if fields {
                ViewChange::FieldInserted
            } else {
                ViewChange::MapInserted
            });
        }
    }
    if fields && reordered {
        report.changes.push(ViewChange::FieldOrder);
    }
    Ok(())
}
