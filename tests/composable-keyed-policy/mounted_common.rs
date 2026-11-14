//! Shares workload data and allocation evidence between separate native-driver processes.
//!
//! Each process runs one workload. Static counters are not reset between calls.
//! Snapshots mark semantic boundaries, not quiescent states or qualified performance comparisons.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Counts completed actions independently of the stored output trace.
pub static OUTPUTS: AtomicUsize = AtomicUsize::new(0);
/// Counts released scalar children without retaining their owners.
pub static CHILD_DROPS: AtomicUsize = AtomicUsize::new(0);
/// Counts released root ledgers after native state extraction.
pub static ROOT_DROPS: AtomicUsize = AtomicUsize::new(0);

/// Changes per-child policy or carries one native child output.
#[derive(Clone, Copy)]
pub enum ChildAction {
    SetStep(u64),
    Step,
    Completed(u64),
}

/// Keeps identical scalar workload data in both native child representations.
pub struct ChildData {
    /// Next value emitted by a step.
    pub next: u64,
    /// Increment applied after each emitted value.
    pub step: u64,
    /// Number of completed actions reduced by this child.
    pub completed: u64,
    /// Most recent completed payload reduced by this child.
    pub last: Option<u64>,
}

impl ChildData {
    /// Creates fresh progress with an explicit starting value and policy.
    pub fn new(next: u64, step: u64) -> Self {
        Self {
            next,
            step,
            completed: 0,
            last: None,
        }
    }

    /// Creates the two active children or an untouched cardinality-control child.
    pub fn initial(id: usize) -> Self {
        match id {
            1 => Self::new(10, 2),
            2 => Self::new(100, 10),
            _ => Self::new(id as u64, 1),
        }
    }

    /// Applies one scalar transition and returns its optional completion action.
    pub fn reduce(&mut self, action: ChildAction) -> Option<ChildAction> {
        match action {
            ChildAction::SetStep(step) => self.step = step,
            ChildAction::Step => {
                let value = self.next;
                self.next += self.step;
                return Some(ChildAction::Completed(value));
            }
            ChildAction::Completed(value) => {
                self.completed += 1;
                self.last = Some(value);
            }
        }
        None
    }

    /// Exposes scalar evidence without allocating or cloning the child.
    pub fn row(&self) -> (u64, u64, u64, Option<u64>) {
        (self.next, self.step, self.completed, self.last)
    }
}

impl Drop for ChildData {
    fn drop(&mut self) {
        CHILD_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

/// Retains the complete output trace and independent lifecycle-check counts.
pub struct Ledger {
    /// Ordered child outputs observed by the native parent.
    pub trace: Vec<(usize, u64)>,
    /// External and emitted actions reduced by the parent.
    pub reductions: usize,
    /// Full child-state checks completed before removal and clearing.
    pub rows_verified: usize,
}

impl Ledger {
    /// Reserves the six outputs required by the nonempty workload.
    pub fn new() -> Self {
        Self {
            trace: Vec::with_capacity(6),
            reductions: 0,
            rows_verified: 0,
        }
    }

    /// Records one parent-observed output and advances independent completion evidence.
    pub fn output(&mut self, id: usize, value: u64) {
        self.trace.push((id, value));
        OUTPUTS.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for Ledger {
    fn drop(&mut self) {
        ROOT_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

/// Literal output oracle, independent of the shared scalar reducer.
pub const TRACE: [(usize, u64); 6] = [(1, 10), (2, 100), (1, 14), (1, 7), (2, 110), (1, 7)];

/// Checks exact child data for unique keys in the requested range.
///
/// Callers supply native keyed-container iterators, which guarantee unique keys.
/// Range and row-count checks then establish the complete key set without allocation.
pub fn verify_rows<'a>(rows: impl Iterator<Item = (&'a usize, &'a ChildData)>, count: usize) {
    let mut seen = 0;
    for (&id, child) in rows {
        assert!(
            (1..=count).contains(&id),
            "child key outside requested cardinality"
        );
        let expected = match id {
            1 => (10, 3, 1, Some(7)),
            2 => (120, 10, 2, Some(110)),
            _ => (id as u64, 1, 0, None),
        };
        assert_eq!(child.row(), expected, "per-child final state for {id}");
        seen += 1;
    }
    assert_eq!(seen, count);
}

/// Checks output completion immediately after the native completion boundary.
pub fn boundary(expected: usize) {
    assert_eq!(
        OUTPUTS.load(Ordering::SeqCst),
        expected,
        "completed output boundary"
    );
}

/// Records heap statistics and domain counters at one semantic boundary.
///
/// Separate reads do not establish quiescence or a single atomic snapshot.
#[derive(Clone, Copy)]
pub struct Snapshot {
    /// Fixed fixture label for this boundary.
    pub name: &'static str,
    /// Allocation count since profiling began.
    pub total_blocks: u64,
    /// Bytes allocated since profiling began.
    pub total_bytes: u64,
    /// Blocks still allocated at the heap-statistics read.
    pub curr_blocks: usize,
    /// Bytes still allocated at the heap-statistics read.
    pub curr_bytes: usize,
    /// Block count when allocated bytes reached their peak, not the block-count maximum.
    pub max_blocks: usize,
    /// Peak allocated bytes since profiling began.
    pub max_bytes: usize,
    /// Completed actions observed before this counter read.
    pub outputs: usize,
    /// Released children observed before this counter read.
    pub children_dropped: usize,
    /// Released root ledgers observed before this counter read.
    pub roots_dropped: usize,
}

impl Snapshot {
    /// Copies current profiling evidence into fixed-size storage.
    ///
    /// # Panics
    ///
    /// Panics if the DHAT heap profiler is not active.
    pub fn take(name: &'static str) -> Self {
        let stats = dhat::HeapStats::get();
        Self {
            name,
            total_blocks: stats.total_blocks,
            total_bytes: stats.total_bytes,
            curr_blocks: stats.curr_blocks,
            curr_bytes: stats.curr_bytes,
            max_blocks: stats.max_blocks,
            max_bytes: stats.max_bytes,
            outputs: OUTPUTS.load(Ordering::SeqCst),
            children_dropped: CHILD_DROPS.load(Ordering::SeqCst),
            roots_dropped: ROOT_DROPS.load(Ordering::SeqCst),
        }
    }

    /// Formats one snapshot after profiling stops, using only fixed fixture labels.
    fn json(self) -> String {
        format!(
            concat!(
                "{{\"name\":\"{name}\",",
                "\"total_blocks\":{total_blocks},",
                "\"total_bytes\":{total_bytes},",
                "\"curr_blocks\":{curr_blocks},",
                "\"curr_bytes\":{curr_bytes},",
                "\"max_blocks\":{max_blocks},",
                "\"max_bytes\":{max_bytes},",
                "\"outputs\":{outputs},",
                "\"children_dropped\":{children_dropped},",
                "\"roots_dropped\":{roots_dropped}}}",
            ),
            name = self.name,
            total_blocks = self.total_blocks,
            total_bytes = self.total_bytes,
            curr_blocks = self.curr_blocks,
            curr_bytes = self.curr_bytes,
            max_blocks = self.max_blocks,
            max_bytes = self.max_bytes,
            outputs = self.outputs,
            children_dropped = self.children_dropped,
            roots_dropped = self.roots_dropped,
        )
    }
}

/// Parses one supported cardinality before the driver starts profiling.
///
/// # Panics
///
/// Panics if the command has missing, invalid, unsupported, or extra arguments.
pub fn cardinality() -> usize {
    let mut args = std::env::args().skip(1);
    let count = args
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .expect("expected cardinality");
    assert!(
        matches!(count, 0 | 2 | 10 | 1000) && args.next().is_none(),
        "expected cardinality 0, 2, 10, or 1000"
    );
    count
}

/// Checks the full workload and child releases after native state extraction.
pub fn verify(ledger: &Ledger, count: usize) {
    assert_eq!(
        ledger.trace,
        if count == 0 { &[][..] } else { &TRACE[..] },
        "complete output trace"
    );
    assert_eq!(ledger.reductions, if count == 0 { 2 } else { 18 });
    assert_eq!(ledger.rows_verified, if count == 0 { 0 } else { 2 });
    boundary(if count == 0 { 0 } else { 6 });
    assert_eq!(
        CHILD_DROPS.load(Ordering::SeqCst),
        if count == 0 { 0 } else { count + 2 },
        "all children released"
    );
}

/// Emits fixed-schema diagnostics after state release and profiler shutdown.
pub fn report(
    engine: &str,
    count: usize,
    reductions: usize,
    trace: &[(usize, u64)],
    snapshots: [Snapshot; 10],
) {
    assert_eq!(ROOT_DROPS.load(Ordering::SeqCst), 1, "root released");
    // Formatting happens only after every measured snapshot and profiler teardown.
    let trace = trace
        .iter()
        .map(|(id, value)| format!("[{id},{value}]"))
        .collect::<Vec<_>>()
        .join(",");
    let snapshots = snapshots
        .into_iter()
        .map(Snapshot::json)
        .collect::<Vec<_>>()
        .join(",");
    let rows = if count == 0 {
        "[]"
    } else {
        "[[1,10,3,1,7],[2,120,10,2,110]]"
    };
    println!(
        concat!(
            "{{\"schema\":\"mounted-keyed-allocations-v1\",",
            "\"engine\":\"{engine}\",",
            "\"accounting\":\"semantic-boundary-not-quiescent\",",
            "\"comparison_qualified\":false,",
            "\"cardinality\":{count},",
            "\"outputs\":{outputs},",
            "\"reductions\":{reductions},",
            "\"children_dropped\":{children_dropped},",
            "\"root_released\":true,",
            "\"policy_work_skipped\":{policy_work_skipped},",
            "\"verified_child_rows\":{rows},",
            "\"trace\":[{trace}],",
            "\"snapshots\":[{snapshots}]}}",
        ),
        engine = engine,
        count = count,
        outputs = OUTPUTS.load(Ordering::SeqCst),
        reductions = reductions,
        children_dropped = CHILD_DROPS.load(Ordering::SeqCst),
        policy_work_skipped = count == 0,
        rows = rows,
        trace = trace,
        snapshots = snapshots,
    );
}

#[cfg(test)]
mod tests {
    use super::{verify_rows, ChildData};

    #[test]
    #[should_panic(expected = "child key outside requested cardinality")]
    fn rows_reject_out_of_range_key_even_when_cardinality_matches() {
        let rows = [
            (
                1,
                ChildData {
                    next: 10,
                    step: 3,
                    completed: 1,
                    last: Some(7),
                },
            ),
            (
                3,
                ChildData {
                    next: 3,
                    step: 1,
                    completed: 0,
                    last: None,
                },
            ),
        ];

        verify_rows(rows.iter().map(|(id, child)| (id, child)), 2);
    }
}
