use crate::future_outcome::{Fault, Trace, OPERATIONS};

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Snapshot {
    allocated_blocks: u64,
    allocated_bytes: u64,
    live_blocks: usize,
    live_bytes: usize,
}

impl Snapshot {
    fn now() -> Self {
        let stats = dhat::HeapStats::get();
        Self {
            allocated_blocks: stats.total_blocks,
            allocated_bytes: stats.total_bytes,
            live_blocks: stats.curr_blocks,
            live_bytes: stats.curr_bytes,
        }
    }

    fn json(self) -> String {
        format!("{{\"allocated_blocks\":{},\"allocated_bytes\":{},\"live_blocks\":{},\"live_bytes\":{}}}",
            self.allocated_blocks, self.allocated_bytes, self.live_blocks, self.live_bytes)
    }
}

#[derive(Debug)]
struct Work {
    trace: Trace,
    counts: (usize, usize),
}

#[derive(Debug)]
struct Measurement {
    before: Snapshot,
    after: Snapshot,
    work: Work,
}

fn capture(run: impl FnOnce() -> Result<Work, &'static str>) -> Result<Measurement, &'static str> {
    let profiler = dhat::Profiler::builder().testing().build();
    let before = Snapshot::now();
    let result = run();
    let after = Snapshot::now();
    drop(profiler);
    Ok(Measurement {
        before,
        after,
        work: result?,
    })
}

pub fn run(engine: &str) -> Result<String, &'static str> {
    if !matches!(engine, "composable" | "rca") {
        return Err("invalid common-outcome engine");
    }
    let measurement = capture(|| {
        let (trace, probe) = crate::outcome::run(Fault::None)?;
        // Native extraction and host teardown finished. Release the last
        // caller-owned probe before E and retain only scalar results.
        let counts = probe.counts();
        drop(probe);
        Ok(Work { trace, counts })
    })?;
    let Work { trace, counts } = measurement.work;
    trace.verify_counts(counts)?;
    let blocks = measurement
        .after
        .allocated_blocks
        .checked_sub(measurement.before.allocated_blocks)
        .ok_or("allocation count decreased")?;
    let bytes = measurement
        .after
        .allocated_bytes
        .checked_sub(measurement.before.allocated_bytes)
        .ok_or("allocation bytes decreased")?;
    // Every string allocation is outside the measured lifetime.
    let work = format!("{{\"schema\":\"composable-common-outcome-v1\",\"engine\":\"{engine}\",\"operations_expected\":{OPERATIONS},\"operations_attempted\":{},\"operations_completed\":{},\"results\":{},\"checkpoints\":{},\"checksum\":{},\"future_polls\":{},\"future_drops\":{},\"errors\":{},\"state_extracted\":true,\"host_stopped\":true,\"completion\":\"result-drop-checkpoint\"}}", trace.attempted, trace.completed, trace.results, trace.checkpoints, trace.checksum, counts.0, counts.1, trace.errors);
    Ok(format!("{{\"schema\":\"composable-common-outcome-allocations-v1\",\"engine\":\"{engine}\",\"instrument\":\"dhat-0.3.3\",\"work\":{work},\"snapshots\":{{\"A\":{},\"E\":{}}},\"lifetime\":{{\"operations\":{OPERATIONS},\"allocated_blocks\":{blocks},\"allocated_bytes\":{bytes}}}}}", measurement.before.json(), measurement.after.json()))
}

#[cfg(not(test))]
pub fn main(engine: &str) {
    if std::env::args_os().len() != 1 {
        eprintln!("common-outcome-allocations expects no arguments");
        std::process::exit(1);
    }
    match run(engine) {
        Ok(ledger) => println!("{ledger}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::hint::black_box;

    /// Preserves the measured cases outside the threaded semantic test host.
    pub(crate) const CASES: &[(&str, fn())] = &[
        ("outcome_allocations::tests::whole_lifetime_includes_construction_and_destructor_allocations", whole_lifetime_includes_construction_and_destructor_allocations),
        ("outcome_allocations::tests::snapshots_and_scalar_results_allocate_nothing", snapshots_and_scalar_results_allocate_nothing),
        ("outcome_allocations::tests::worker_allocations_are_included_through_join", worker_allocations_are_included_through_join),
        ("outcome_allocations::tests::rejected_work_emits_no_measurement_and_stops_profiler", rejected_work_emits_no_measurement_and_stops_profiler),
        ("outcome_allocations::tests::native_capture_formats_only_after_successful_teardown", native_capture_formats_only_after_successful_teardown),
    ];

    #[test]
    fn allocation_case_roster_preserves_exact_names() {
        let expected = [
            "outcome_allocations::tests::whole_lifetime_includes_construction_and_destructor_allocations",
            "outcome_allocations::tests::snapshots_and_scalar_results_allocate_nothing",
            "outcome_allocations::tests::worker_allocations_are_included_through_join",
            "outcome_allocations::tests::rejected_work_emits_no_measurement_and_stops_profiler",
            "outcome_allocations::tests::native_capture_formats_only_after_successful_teardown",
        ];
        assert_eq!(
            CASES.iter().map(|case| case.0).collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn retired_engine_is_rejected_before_capture() {
        assert_eq!(run("rca-0.7"), Err("invalid common-outcome engine"));
    }

    fn empty_work() -> Work {
        Work {
            trace: Trace::default(),
            counts: (0, 0),
        }
    }

    struct Fixture(Box<[u8; 64]>);

    impl Drop for Fixture {
        fn drop(&mut self) {
            black_box(&self.0);
            drop(black_box(Box::new([0_u8; 128])));
        }
    }

    fn whole_lifetime_includes_construction_and_destructor_allocations() {
        if !crate::native::in_child("outcome_allocations::tests::whole_lifetime_includes_construction_and_destructor_allocations") { return; }
        let result = capture(|| {
            let fixture = Fixture(black_box(Box::new([0; 64])));
            drop(fixture);
            Ok(empty_work())
        })
        .unwrap();
        assert_eq!(
            result.after.allocated_blocks - result.before.allocated_blocks,
            2
        );
        assert_eq!(
            result.after.allocated_bytes - result.before.allocated_bytes,
            192
        );
        assert_eq!(
            (result.after.live_blocks, result.after.live_bytes),
            (result.before.live_blocks, result.before.live_bytes)
        );
    }

    fn snapshots_and_scalar_results_allocate_nothing() {
        if !crate::native::in_child(
            "outcome_allocations::tests::snapshots_and_scalar_results_allocate_nothing",
        ) {
            return;
        }
        let result = capture(|| Ok(empty_work())).unwrap();
        assert_eq!(result.before, Snapshot::default());
        assert_eq!(result.after, Snapshot::default());
    }

    fn worker_allocations_are_included_through_join() {
        if !crate::native::in_child(
            "outcome_allocations::tests::worker_allocations_are_included_through_join",
        ) {
            return;
        }
        let result = capture(|| {
            let allocation = std::thread::spawn(|| black_box(Box::new([0_u8; 4096])))
                .join()
                .unwrap();
            drop(black_box(allocation));
            Ok(empty_work())
        })
        .unwrap();
        assert!(result.after.allocated_bytes - result.before.allocated_bytes >= 4096);
        assert_eq!(result.after.live_bytes, result.before.live_bytes);
    }

    fn rejected_work_emits_no_measurement_and_stops_profiler() {
        if !crate::native::in_child(
            "outcome_allocations::tests::rejected_work_emits_no_measurement_and_stops_profiler",
        ) {
            return;
        }
        capture(|| {
            drop(black_box(Box::new([0_u8; 64])));
            Err("rejected work")
        })
        .expect_err("failed work has no measurement");
        capture(|| Ok(empty_work())).expect("failed capture released profiler");
    }

    fn native_capture_formats_only_after_successful_teardown() {
        if !crate::native::in_child(
            "outcome_allocations::tests::native_capture_formats_only_after_successful_teardown",
        ) {
            return;
        }
        let engine = match env!("CARGO_PKG_NAME") {
            "composable-stream-candidate" => "rca",
            "composable-stream-upstream" => "composable",
            _ => unreachable!(),
        };
        let output = run(engine).unwrap();
        assert!(output.starts_with("{\"schema\":\"composable-common-outcome-allocations-v1\""));
        assert!(output.contains("\"checkpoints\":64,\"checksum\":6496"));
        assert!(output.contains("\"state_extracted\":true,\"host_stopped\":true"));
        assert!(output.contains("\"completion\":\"result-drop-checkpoint\""));
    }
}
