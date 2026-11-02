//! Captures caller-selected values and logical graph edges without reflection.

use core::fmt;
use std::backtrace::Backtrace;

/// Bounds admitted nodes and declared edges in one capture.
///
/// These limits bound graph output, not callback work, allocation or termination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureLimits {
    /// Maximum admitted origins, including the root.
    nodes: usize,
    /// Maximum declared outgoing edges, including edges not yet followed.
    edges: usize,
}

impl CaptureLimits {
    /// Creates explicit node and edge budgets, including zero budgets.
    #[must_use]
    pub const fn new(nodes: usize, edges: usize) -> Self {
        Self { nodes, edges }
    }
}

/// Supplies independently captured leaves and ordered, labelled outgoing origins.
///
/// Capture private values inside their owning module. Omit fields deliberately:
/// omitted fields do not earn exhaustive comparison coverage. `Leaves` must not
/// retain mutable aliases that rewrite history; ownership or `Clone` alone does
/// not establish independence. The helper does not enforce a deep-copy contract.
#[derive(Clone)]
pub struct NodeCapture<Origin, Leaves> {
    /// Selected values whose independence remains the caller's responsibility.
    leaves: Leaves,
    /// Ordered outgoing labels and origins awaiting registration or lookup.
    edges: Vec<(String, Origin)>,
}

impl<Origin, Leaves> NodeCapture<Origin, Leaves> {
    /// Supplies selected leaves and edges in semantic order.
    #[must_use]
    pub fn new(leaves: Leaves, edges: Vec<(String, Origin)>) -> Self {
        Self { leaves, edges }
    }
}

impl<Origin, Leaves> fmt::Debug for NodeCapture<Origin, Leaves> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeCapture")
            .field("edge_count", &self.edges.len())
            .finish_non_exhaustive()
    }
}

/// Describes an edge's encounter during this capture, not runtime lifetime identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferenceKind {
    /// First encounter; the target was registered before following its edges.
    First,
    /// Target was previously captured and is no longer on the active path.
    Repeat,
    /// Target is on the active capture path, including a self-reference.
    Cycle,
}

/// Stores one selected edge using a capture-local target index.
#[derive(Clone)]
pub struct CapturedEdge {
    /// Caller-selected domain text, excluded from implicit Debug output.
    label: String,
    /// Valid index into the owning graph's registered nodes.
    target: usize,
    /// Encounter metadata that does not participate in semantic equality.
    kind: ReferenceKind,
}

impl CapturedEdge {
    /// Returns the caller-selected edge label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Returns an index into this graph's nodes, not another capture's nodes.
    #[must_use]
    pub const fn target(&self) -> usize {
        self.target
    }

    /// Returns capture traversal metadata, excluded from semantic equality.
    #[must_use]
    pub const fn kind(&self) -> ReferenceKind {
        self.kind
    }
}

impl fmt::Debug for CapturedEdge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturedEdge")
            .field("target", &self.target)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// Owns selected leaves and logical origin for one captured node.
#[derive(Clone)]
pub struct CapturedNode<Origin, Leaves> {
    /// Logical identity used to recognise repeated references during capture.
    origin: Origin,
    /// Selected domain values, never formatted implicitly.
    leaves: Leaves,
    /// Resolved outgoing references in the caller's declared order.
    edges: Vec<CapturedEdge>,
}

impl<Origin, Leaves> CapturedNode<Origin, Leaves> {
    /// Returns the caller's logical origin, not a private runtime generation.
    #[must_use]
    pub const fn origin(&self) -> &Origin {
        &self.origin
    }

    /// Borrows the selected captured values.
    #[must_use]
    pub const fn leaves(&self) -> &Leaves {
        &self.leaves
    }

    /// Borrows outgoing edges in caller-supplied semantic order.
    #[must_use]
    pub fn edges(&self) -> &[CapturedEdge] {
        &self.edges
    }
}

impl<Origin, Leaves> fmt::Debug for CapturedNode<Origin, Leaves> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturedNode")
            .field("edge_count", &self.edges.len())
            .finish_non_exhaustive()
    }
}

/// Owns a finite, caller-selected graph with no implicit live model handles.
///
/// Equality compares logical origins, selected leaves, ordered edge labels and
/// target origins. It ignores capture-local indices and traversal markers.
/// Aliased versus duplicated logical nodes are therefore distinct selected
/// topology. Logical origins must uniquely identify nodes within one capture;
/// equal origins across captures still have their selected leaves compared.
/// Neither origin equality nor visible equality proves the same runtime lifetime.
/// Equality uses an explicit work stack, so cycles do not cause recursive traversal.
/// Its visited-pair lookup can take quadratic work; no hashing bound is required.
///
/// `Clone` uses the ordinary `Origin: Clone, Leaves: Clone` bounds. Cloning an
/// aliased leaf does not make independent history. `Debug` contains metadata
/// only, with no `Origin: Debug` or `Leaves: Debug` requirement. Explicit
/// renderers decide how selected private values and labels are displayed.
///
/// Local capture requires no `Send`, `Sync`, `Clone`, `Debug` or `'static` bound.
/// Using a graph as a projected `rca-test` snapshot still requires
/// `Clone + Debug + PartialEq + Send + 'static`, but not `Sync`. Its capture and
/// renderer callbacks require `Send + Sync + 'static`; custom rendering does
/// not remove those bounds. Omitted fields retain projected, not full coverage.
#[derive(Clone)]
pub struct CapturedGraph<Origin, Leaves> {
    /// Nonempty registration order with the root at zero and all edge targets in range.
    nodes: Vec<CapturedNode<Origin, Leaves>>,
}

impl<Origin, Leaves> CapturedGraph<Origin, Leaves> {
    /// Borrows the root node registered first in this capture.
    #[must_use]
    pub fn root(&self) -> &CapturedNode<Origin, Leaves> {
        &self.nodes[0]
    }

    /// Borrows nodes in this capture's registration order.
    #[must_use]
    pub fn nodes(&self) -> &[CapturedNode<Origin, Leaves>] {
        &self.nodes
    }
}

impl<Origin: Eq, Leaves> CapturedGraph<Origin, Leaves> {
    /// Mutates expected selected values without changing origin or topology.
    ///
    /// # Panics
    ///
    /// Propagates panics from origin equality.
    pub fn leaves_mut(&mut self, origin: &Origin) -> Option<&mut Leaves> {
        self.nodes
            .iter_mut()
            .find(|node| node.origin == *origin)
            .map(|node| &mut node.leaves)
    }
}

impl<Origin, Leaves> fmt::Debug for CapturedGraph<Origin, Leaves> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturedGraph")
            .field("node_count", &self.nodes.len())
            .field(
                "edge_count",
                &self
                    .nodes
                    .iter()
                    .map(|node| node.edges.len())
                    .sum::<usize>(),
            )
            .finish_non_exhaustive()
    }
}

impl<Origin: Eq, Leaves: PartialEq> PartialEq for CapturedGraph<Origin, Leaves> {
    fn eq(&self, other: &Self) -> bool {
        if self.nodes.len() != other.nodes.len() {
            return false;
        }
        let mut pending = vec![(0, 0)];
        let mut visited = Vec::new();
        while let Some(pair @ (left_index, right_index)) = pending.pop() {
            if visited.contains(&pair) {
                continue;
            }
            visited.push(pair);
            let left = &self.nodes[left_index];
            let right = &other.nodes[right_index];
            if left.origin != right.origin
                || left.leaves != right.leaves
                || left.edges.len() != right.edges.len()
            {
                return false;
            }
            for (left_edge, right_edge) in left.edges.iter().zip(&right.edges) {
                if left_edge.label != right_edge.label {
                    return false;
                }
                pending.push((left_edge.target, right_edge.target));
            }
        }
        true
    }
}

impl<Origin: Eq, Leaves: Eq> Eq for CapturedGraph<Origin, Leaves> {}

/// Identifies the budget that rejected a node before it entered the graph.
#[derive(Debug)]
enum LimitKind {
    /// No capacity remains for another logical origin.
    Nodes,
    /// A callback's declared edges exceed the remaining edge budget.
    Edges,
}

/// Reports a capture budget failure without returning a partial graph.
#[derive(Debug)]
pub struct CaptureError {
    /// Budget that rejected the next node.
    kind: LimitKind,
    /// Caller-supplied limits for this capture.
    limits: CaptureLimits,
    /// Nodes already registered, excluding the rejected node.
    captured_nodes: usize,
    /// Edges already admitted, including pending traversal work.
    declared_edges: usize,
    /// Failure-site context, captured only when a budget rejects a node.
    backtrace: Backtrace,
}

impl CaptureError {
    /// Records the admitted work without retaining rejected domain values.
    fn new(
        kind: LimitKind,
        limits: CaptureLimits,
        captured_nodes: usize,
        declared_edges: usize,
    ) -> Self {
        Self {
            kind,
            limits,
            captured_nodes,
            declared_edges,
            backtrace: Backtrace::capture(),
        }
    }

    /// Reports exhaustion of the node budget.
    #[must_use]
    pub const fn is_node_limit(&self) -> bool {
        matches!(self.kind, LimitKind::Nodes)
    }

    /// Reports exhaustion of the edge budget.
    #[must_use]
    pub const fn is_edge_limit(&self) -> bool {
        matches!(self.kind, LimitKind::Edges)
    }

    /// Returns the configured budgets.
    #[must_use]
    pub const fn limits(&self) -> CaptureLimits {
        self.limits
    }

    /// Returns nodes admitted before failure, not rejected callback output.
    #[must_use]
    pub const fn captured_nodes(&self) -> usize {
        self.captured_nodes
    }

    /// Returns declared edges admitted before failure, including not-yet-followed edges.
    #[must_use]
    pub const fn declared_edges(&self) -> usize {
        self.declared_edges
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            LimitKind::Nodes => "node",
            LimitKind::Edges => "edge",
        };
        write!(
            f,
            "capture {kind} limit exhausted: {} nodes and {} declared edges admitted, limits {}/{}\n{}",
            self.captured_nodes,
            self.declared_edges,
            self.limits.nodes,
            self.limits.edges,
            self.backtrace
        )
    }
}

impl std::error::Error for CaptureError {}

/// Owns unvisited outgoing origins and releases them if capture exits early.
type PendingEdges<Origin> = std::vec::IntoIter<(String, Origin)>;

/// Captures selected logical nodes once each, with explicit finite graph budgets.
///
/// `capture_node` resolves caller-owned logical origins to live model values,
/// then returns independent selected leaves and outgoing origins. It is called
/// at most once per encountered origin, after checking the node budget; its
/// returned edges may still exceed the edge budget. Origins are registered
/// before their edges are followed, so repeats/cycles terminate without
/// reference-counted cycles.
/// No callback or implicit live reference is stored. Caller-selected origins
/// and leaves must themselves obey the intended history/privacy policy.
/// Capture is synchronous, not an atomic snapshot of independently mutating
/// external objects; arrange domain locking/consistency in the caller.
///
/// Node lookup uses equality, not hashing; capture can take quadratic lookup
/// work. Budgets bound admitted nodes/edges, not user callback work or its
/// allocations. Output is selected data, not a reflective or exhaustive view.
///
/// # Examples
///
/// ```
/// use std::cell::Cell;
/// use rca_core::diagnostics::{capture_graph, CaptureLimits, NodeCapture};
/// let private = Cell::new(3);
/// let before = capture_graph("model", CaptureLimits::new(1, 0), |_| {
///     NodeCapture::new(private.get(), vec![])
/// }).unwrap();
/// private.set(4);
/// assert_eq!(*before.root().leaves(), 3);
/// ```
///
/// # Errors
///
/// Returns [`CaptureError`] when admitting a node or its declared edges would
/// exceed a budget. The rejected node is not included in error accounting;
/// all partial storage is dropped and no partial graph is returned. A zero
/// node budget fails before calling the capture callback.
///
/// # Panics
///
/// Propagates panics from origin equality or the caller's capture callback.
/// Dropping rejected or repeated domain values can also propagate destructor panics.
/// The owned callback is dropped before returning either the graph or a budget error.
pub fn capture_graph<Origin: Eq, Leaves>(
    root: Origin,
    limits: CaptureLimits,
    mut capture_node: impl FnMut(&Origin) -> NodeCapture<Origin, Leaves>,
) -> Result<CapturedGraph<Origin, Leaves>, CaptureError> {
    // Retain either outcome while dropping the owned callback. In particular,
    // a callback destructor panic must not leak a graph or an error's backtrace.
    let result = (|| {
        let mut graph = CapturedGraph { nodes: Vec::new() };
        let mut declared_edges = 0;
        let root_edges = admit_node(
            root,
            limits,
            &mut graph,
            &mut declared_edges,
            &mut capture_node,
        )?;
        let mut pending = vec![(0, root_edges)];
        while let Some((source, edges)) = pending.last_mut() {
            let source = *source;
            let Some((label, origin)) = edges.next() else {
                pending.pop();
                continue;
            };
            let existing = graph.nodes.iter().position(|node| node.origin == origin);
            let (target, kind, next) = if let Some(target) = existing {
                let kind = if pending.iter().any(|(index, _)| *index == target) {
                    ReferenceKind::Cycle
                } else {
                    ReferenceKind::Repeat
                };
                (target, kind, None)
            } else {
                let target = graph.nodes.len();
                let edges = admit_node(
                    origin,
                    limits,
                    &mut graph,
                    &mut declared_edges,
                    &mut capture_node,
                )?;
                (target, ReferenceKind::First, Some(edges))
            };
            graph.nodes[source].edges.push(CapturedEdge {
                label,
                target,
                kind,
            });
            if let Some(edges) = next {
                pending.push((target, edges));
            }
        }
        Ok(graph)
    })();
    drop(capture_node);
    result
}

/// Checks both budgets before registering one node and returning its pending edges.
///
/// The node limit is checked before user code. The edge limit uses the callback's
/// declared edge count, so pending work cannot overbook the remaining budget.
/// Failure drops the rejected origin, leaves and edges without changing admitted counts.
fn admit_node<Origin, Leaves>(
    origin: Origin,
    limits: CaptureLimits,
    graph: &mut CapturedGraph<Origin, Leaves>,
    declared_edges: &mut usize,
    capture: &mut impl FnMut(&Origin) -> NodeCapture<Origin, Leaves>,
) -> Result<PendingEdges<Origin>, CaptureError> {
    if graph.nodes.len() == limits.nodes {
        return Err(CaptureError::new(
            LimitKind::Nodes,
            limits,
            graph.nodes.len(),
            *declared_edges,
        ));
    }
    let NodeCapture { leaves, edges } = capture(&origin);
    if edges.len() > limits.edges - *declared_edges {
        return Err(CaptureError::new(
            LimitKind::Edges,
            limits,
            graph.nodes.len(),
            *declared_edges,
        ));
    }
    *declared_edges += edges.len();
    graph.nodes.push(CapturedNode {
        origin,
        leaves,
        edges: Vec::new(),
    });
    Ok(edges.into_iter())
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use super::*;

    #[test]
    fn equality_ignores_local_numbering_and_encounter_markers() {
        let left = capture_graph(0, CaptureLimits::new(3, 4), |id| {
            NodeCapture::new(
                *id,
                match id {
                    0 => vec![("a".into(), 1), ("b".into(), 2)],
                    1 => vec![("repeat".into(), 2)],
                    _ => vec![("cycle".into(), 0)],
                },
            )
        })
        .unwrap();
        let mut right = left.clone();
        // Swap non-root registration indices while preserving every logical edge.
        right.nodes.swap(1, 2);
        for node in &mut right.nodes {
            for edge in &mut node.edges {
                edge.target = match edge.target {
                    1 => 2,
                    2 => 1,
                    other => other,
                };
                edge.kind = ReferenceKind::Repeat;
            }
        }
        assert_eq!(left, right);
        assert_eq!(right, left);
        right.nodes[1].leaves = 99;
        assert_ne!(left, right);
        right.nodes[1].leaves = 2;
        right.nodes[0].edges[0].label = "changed".into();
        assert_ne!(left, right);
    }
}
}
