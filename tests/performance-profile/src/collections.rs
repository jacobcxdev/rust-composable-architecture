//! Direct, reserved keyed insertion and one-shot sorting; no Store or executor.
//!
//! Each iteration owns a prebuilt fixture. Work does not rebuild or reset it.
//! Payload lifetime counters and comparator calls remain part of the workload.
//! Order checks, lookup checks and result formatting stay outside measured work.

use std::{cell::Cell, hint::black_box, rc::Rc};

use rca_core::KeyedState;

use crate::measurement::Measurement;

/// Selects the collection operation or its no-operation driver control.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    /// Inserts before all existing rows.
    Head,
    /// Inserts at half the original row count, rounded down.
    Middle,
    /// Inserts after all existing rows.
    Tail,
    /// Sorts rows whose ranks already increase.
    Sorted,
    /// Sorts rows whose ranks initially decrease.
    Reversed,
    /// Sorts repeated ranks while checking the established equal-rank order.
    Ties,
    /// Visits insertion fixtures without consuming their prepared input.
    InsertControl,
    /// Visits sorting fixtures without invoking the sorter.
    SortControl,
}

/// A validated point in the fixed collection qualification matrix.
#[derive(Clone, Copy)]
pub(crate) struct Config {
    /// Chooses the operation and its independent expected results.
    mode: Mode,
    /// The canonical CLI name emitted in the report.
    name: &'static str,
    /// Original rows, excluding any prepared insertion input.
    size: usize,
    /// Independent prebuilt fixtures, each visited exactly once.
    iterations: usize,
}

impl Config {
    /// Parses one bounded size and fixture count for a known collection mode.
    ///
    /// # Errors
    /// Returns an error for missing, non-decimal, overflowing or unsupported
    /// dimensions, unknown modes, or extra arguments.
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let (mode, name) = match args.next().as_deref() {
            Some("collection-insert-head") => (Mode::Head, "collection-insert-head"),
            Some("collection-insert-middle") => (Mode::Middle, "collection-insert-middle"),
            Some("collection-insert-tail") => (Mode::Tail, "collection-insert-tail"),
            Some("collection-sort-sorted") => (Mode::Sorted, "collection-sort-sorted"),
            Some("collection-sort-reversed") => (Mode::Reversed, "collection-sort-reversed"),
            Some("collection-sort-ties") => (Mode::Ties, "collection-sort-ties"),
            Some("collection-insert-control") => (Mode::InsertControl, "collection-insert-control"),
            Some("collection-sort-control") => (Mode::SortControl, "collection-sort-control"),
            _ => return Err("unknown collection workload"),
        };
        let dimension = |text: Option<String>| {
            let text = text.ok_or("missing collection dimension")?;
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("invalid collection dimension");
            }
            text.parse::<usize>()
                .map_err(|_| "collection dimension overflow")
        };
        let size = dimension(args.next())?;
        let iterations = dimension(args.next())?;
        if args.next().is_some()
            || !matches!(size, 0 | 1 | 16 | 1024)
            || !matches!(iterations, 1 | 256)
        {
            return Err("collection workload requires size 0/1/16/1024 and iterations 1/256");
        }
        Ok(Self {
            mode,
            name,
            size,
            iterations,
        })
    }

    /// Includes the insertion control because it also needs a reserved input value.
    fn inserting(self) -> bool {
        matches!(
            self.mode,
            Mode::Head | Mode::Middle | Mode::Tail | Mode::InsertControl
        )
    }

    /// Identifies driver-only visits that must not count as collection operations.
    fn control(self) -> bool {
        matches!(self.mode, Mode::InsertControl | Mode::SortControl)
    }

    /// Selects the insertion index within the original rows.
    fn position(self) -> usize {
        match self.mode {
            Mode::Head => 0,
            Mode::Middle => self.size / 2,
            _ => self.size,
        }
    }

    /// Assigns a sort key to an ID from this configuration's fixture range.
    fn rank(self, id: usize) -> usize {
        match self.mode {
            Mode::Reversed => self.size - id,
            Mode::Ties => id % 3,
            _ => id,
        }
    }

    /// Returns literal comparator counts for the pinned sorter and finite inputs.
    fn comparisons(self) -> usize {
        // Literal qualification counts, not a second implementation of the sorter.
        match (self.mode, self.size) {
            (Mode::Sorted | Mode::Reversed, 16) => 32,
            (Mode::Sorted | Mode::Reversed, 1024) => 5120,
            (Mode::Ties, 16) => 47,
            (Mode::Ties, 1024) => 8108,
            _ => 0,
        }
    }
}

/// Counts move-only payload lifetimes on this synchronous driver thread.
#[derive(Default)]
struct Probe {
    /// Payloads constructed before work across all fixtures.
    created: Cell<usize>,
    /// Payload destructors observed so far.
    dropped: Cell<usize>,
}

/// A move-only payload whose lifetime is visible through its shared probe.
///
/// Only construction clones the probe. Work never clones this payload.
struct Value {
    /// Sort key, deliberately repeated in the equal-rank workload.
    rank: usize,
    /// Records this payload's destruction without retaining the payload itself.
    probe: Rc<Probe>,
}

impl Drop for Value {
    fn drop(&mut self) {
        self.probe.dropped.set(self.probe.dropped.get() + 1);
    }
}

/// Owns one prepared collection and the evidence from its single work visit.
struct Fixture {
    /// Rows whose reserved capacity includes any incoming insertion value.
    rows: KeyedState<usize, Value>,
    /// Value removed during setup and consumed only by a real insertion.
    input: Option<Value>,
    /// Distinguishes completed empty/singleton operations from skipped work.
    completed: bool,
    /// Comparator invocations made by this fixture's sort, excluding setup.
    comparisons: usize,
}

/// Prebuilds each fixture and reserves its incoming value without a throwaway payload.
///
/// # Errors
/// Returns an error if the prepared insertion input cannot be removed.
fn fixtures(config: Config, probe: &Rc<Probe>) -> Result<Vec<Fixture>, &'static str> {
    let count = config.size + usize::from(config.inserting());
    (0..config.iterations)
        .map(|_| {
            let mut rows = KeyedState::with_capacity(count);
            for id in 0..count {
                probe.created.set(probe.created.get() + 1);
                rows.insert(
                    id,
                    Value {
                        rank: config.rank(id),
                        probe: Rc::clone(probe),
                    },
                );
            }
            // Retain capacity and lineage even for N=0, without creating a throwaway value.
            let input = if config.inserting() {
                Some(
                    rows.remove(&config.size)
                        .ok_or("missing prepared insertion input")?,
                )
            } else {
                None
            };
            Ok(Fixture {
                rows,
                input,
                completed: false,
                comparisons: 0,
            })
        })
        .collect()
}

/// Measures one visit per fixture, including the selected insertion or driver control.
#[inline(never)]
fn profile_collection_insert_work(fixtures: &mut [Fixture], config: Config) -> usize {
    let mut visits = 0;
    for fixture in black_box(fixtures) {
        fixture.completed = if config.control() {
            true
        } else {
            fixture.input.take().is_some_and(|value| {
                fixture
                    .rows
                    .try_insert_at(config.position(), config.size, value)
                    .is_ok()
            })
        };
        black_box(&fixture.rows);
        visits += 1;
    }
    visits
}

/// Measures one sort or control visit without rebuilding any fixture.
#[inline(never)]
fn profile_collection_sort_work(fixtures: &mut [Fixture], config: Config) -> usize {
    let mut visits = 0;
    for fixture in black_box(fixtures) {
        fixture.completed = config.control()
            || fixture
                .rows
                .try_sort_by(|_, left, _, right| {
                    fixture.comparisons += 1;
                    Ok::<_, std::convert::Infallible>(left.rank.cmp(&right.rank))
                })
                .is_ok();
        black_box(&fixture.rows);
        visits += 1;
    }
    visits
}

/// Checks work counts, ordered rows, both lookup directions and retained input ownership.
///
/// # Errors
/// Returns an error for missing work, unexpected drops or comparisons, wrong
/// insertion ownership, or inconsistent row order and keyed lookups.
fn validate(
    config: Config,
    fixtures: &[Fixture],
    probe: &Rc<Probe>,
    visits: usize,
) -> Result<usize, &'static str> {
    let mut expected: Vec<_> = (0..config.size).map(|id| (id, id)).collect();
    match config.mode {
        Mode::Head => expected.insert(0, (config.size, config.size)),
        Mode::Middle => expected.insert(config.size / 2, (config.size, config.size)),
        Mode::Tail => expected.push((config.size, config.size)),
        Mode::Reversed => {
            expected = (0..config.size)
                .rev()
                .enumerate()
                .map(|(index, id)| (id, index + 1))
                .collect();
        }
        // Current tie order is a compatibility oracle, not a new public stability promise.
        Mode::Ties => {
            expected = (0..3)
                .flat_map(|rank| (rank..config.size).step_by(3).map(move |id| (id, rank)))
                .collect();
        }
        _ => {}
    }
    let created = (config.size + usize::from(config.inserting())) * config.iterations;
    if fixtures.len() != config.iterations
        || visits != config.iterations
        || probe.created.get() != created
        || probe.dropped.get() != 0
    {
        return Err("collection work or lifetime count mismatch");
    }
    let mut pairs = 0;
    for fixture in fixtures {
        if !fixture.completed
            || fixture.comparisons != config.comparisons()
            || fixture.rows.len() != expected.len()
            || fixture.input.is_some() != (config.mode == Mode::InsertControl)
            || fixture
                .input
                .as_ref()
                .is_some_and(|value| value.rank != config.size)
        {
            return Err("collection operation outcome mismatch");
        }
        for (index, (expected_id, expected_rank)) in expected.iter().enumerate() {
            let (id, value) = fixture
                .rows
                .get_index(index)
                .ok_or("missing collection pair")?;
            if id != expected_id
                || value.rank != *expected_rank
                || fixture.rows.index_of(id) != Some(index)
                || !fixture
                    .rows
                    .get(id)
                    .is_some_and(|found| std::ptr::eq(found, value))
            {
                return Err("collection order or lookup mismatch");
            }
            pairs += 1;
        }
    }
    Ok(pairs)
}

/// Requires every payload to drop and release its shared probe owner.
///
/// # Errors
/// Returns an error if lifetime counts differ or another probe owner remains.
fn released(probe: &Rc<Probe>) -> Result<(), &'static str> {
    if probe.created.get() != probe.dropped.get() || Rc::strong_count(probe) != 1 {
        return Err("collection owners retained");
    }
    Ok(())
}

/// Checked scalar facts retained after every collection and payload is dropped.
struct Counts {
    /// Prebuilt fixtures visited by the work driver, including controls.
    visits: usize,
    /// Successful insertions or sorts, excluding driver-only controls.
    operations: usize,
    /// Comparator calls across all sort fixtures.
    comparisons: usize,
    /// Ordered key/value pairs checked after work.
    pairs: usize,
    /// Payloads constructed across all fixtures.
    created: usize,
    /// Payloads destroyed by the end of teardown.
    dropped: usize,
    /// Payload destructors observed at the end of work, before checking or teardown.
    work_drops: usize,
}

/// Executes the fixed phases and releases fixtures before returning any checking error.
///
/// # Errors
/// Returns an error if preparation, result checking or final ownership checking fails.
fn execute(config: Config, measurement: &mut Measurement) -> Result<Counts, &'static str> {
    let probe = Rc::new(Probe::default());
    let mut fixtures = fixtures(config, &probe)?;
    measurement.construction_finished();
    let visits = if config.inserting() {
        profile_collection_insert_work(&mut fixtures, config)
    } else {
        profile_collection_sort_work(&mut fixtures, config)
    };
    measurement.work_finished();
    let operations = fixtures
        .iter()
        .filter(|fixture| fixture.completed && !config.control())
        .count();
    let comparisons = fixtures.iter().map(|fixture| fixture.comparisons).sum();
    let work_drops = probe.dropped.get();
    let valid = validate(config, &fixtures, &probe, visits);
    measurement.validation_finished();
    drop(fixtures);
    let release = released(&probe);
    let (created, dropped) = (probe.created.get(), probe.dropped.get());
    drop(probe);
    release?;
    Ok(Counts {
        visits,
        operations,
        comparisons,
        pairs: valid?,
        created,
        dropped,
        work_drops,
    })
}

/// Profiles one validated matrix point and formats its record after profiler finalisation.
///
/// # Errors
/// Returns an error if the workload, owner checks or allocation checks fail.
pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    let mut measurement = Measurement::new(None);
    let result = execute(config, &mut measurement);
    let allocation_result = measurement.finish();
    let counts = result?;
    let allocations = allocation_result?.map_or_else(|| "null".to_owned(), |record| record.json());
    let kind = if config.control() {
        "driver-control"
    } else {
        "operation"
    };
    Ok(format!(
        "{{\"schema\":\"rca-collection-performance-profile-v1\",\"workload\":\"{}\",\"kind\":\"{kind}\",\"size\":{},\"iterations\":{},\"prebuilt_fixtures\":{},\"value_size\":{},\"driver_visits\":{},\"operations\":{},\"comparisons\":{},\"validated_pairs\":{},\"values_created\":{},\"values_dropped\":{},\"work_drops\":{},\"owners_released\":true,\"features\":{{\"allocations\":{},\"deterministic\":{},\"diagnostics\":{},\"persistence\":{}}},\"allocations\":{allocations}}}",
        config.name,
        config.size,
        config.iterations,
        config.iterations,
        size_of::<Value>(),
        counts.visits,
        counts.operations,
        counts.comparisons,
        counts.pairs,
        counts.created,
        counts.dropped,
        counts.work_drops,
        cfg!(feature = "allocations"),
        cfg!(feature = "deterministic"),
        cfg!(feature = "diagnostics"),
        cfg!(feature = "persistence"),
    ))
}

#[cfg(test)]
mod tests;
