use crate::ready_future::{Fault, Point, Trace};

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        format!("{{\"allocated_blocks\":{},\"allocated_bytes\":{},\"live_blocks\":{},\"live_bytes\":{}}}", self.allocated_blocks, self.allocated_bytes, self.live_blocks, self.live_bytes)
    }
}

#[derive(Debug)]
struct Work {
    trace: Trace,
    counts: (usize, usize),
}

#[derive(Debug)]
struct Measurement {
    snapshots: [Snapshot; 5],
    work: Work,
}

fn capture(
    run: impl FnOnce(&mut dyn FnMut(Point)) -> Result<Work, &'static str>,
) -> Result<Measurement, &'static str> {
    let profiler = dhat::Profiler::builder().testing().build();
    let a = Snapshot::now();
    let mut middle = [a; 3];
    let mut next = 0;
    let mut invalid = false;
    let result = run(&mut |point| {
        if [Point::Construction, Point::Initial, Point::Checked].get(next) != Some(&point) {
            invalid = true;
        } else {
            middle[next] = Snapshot::now();
            next += 1;
        }
    });
    let e = Snapshot::now();
    drop(profiler);
    let work = result?;
    if invalid || next != 3 {
        return Err("invalid allocation snapshot schedule");
    }
    let [b, c, d] = middle;
    Ok(Measurement {
        snapshots: [a, b, c, d, e],
        work,
    })
}

fn phase(before: Snapshot, after: Snapshot, operations: usize) -> String {
    let blocks = after
        .allocated_blocks
        .checked_sub(before.allocated_blocks)
        .expect("cumulative allocation blocks cannot decrease");
    let bytes = after
        .allocated_bytes
        .checked_sub(before.allocated_bytes)
        .expect("cumulative allocation bytes cannot decrease");
    format!(
        "{{\"operations\":{operations},\"allocated_blocks\":{blocks},\"allocated_bytes\":{bytes}}}"
    )
}

pub fn run(engine: &str, completion: &str) -> Result<String, &'static str> {
    let measurement = capture(|observe| {
        let (trace, probe) = crate::ready::run_observed(Fault::None, observe)?;
        // Native state extraction, host teardown and validation already finished.
        // Retain only scalars; release the last caller-owned probes before E.
        let counts = probe.counts();
        drop(probe);
        Ok(Work { trace, counts })
    })?;
    measurement
        .work
        .trace
        .verify_counts(measurement.work.counts)?;
    let work = measurement
        .work
        .trace
        .json(engine, completion, measurement.work.counts);
    let [a, b, c, d, e] = measurement.snapshots;
    let construction = phase(a, b, 1);
    let initial = phase(b, c, 20);
    let checked = phase(c, d, 44);
    let teardown = phase(d, e, 1);
    let setup = phase(a, c, 1);
    Ok(format!("{{\"schema\":\"composable-ready-future-allocations-v1\",\"engine\":\"{engine}\",\"instrument\":\"dhat-0.3.3\",\"work\":{work},\"snapshots\":{{\"A\":{},\"B\":{},\"C\":{},\"D\":{},\"E\":{}}},\"phases\":{{\"construction\":{construction},\"initial\":{initial},\"checked\":{checked},\"teardown\":{teardown},\"setup\":{setup}}}}}", a.json(),b.json(),c.json(),d.json(),e.json()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;

    #[expect(
        dead_code,
        reason = "this fixture uses a subset of the shared core child helper"
    )]
    mod child_process {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../child_process.rs"));
    }

    fn in_child(name: &str) -> bool {
        const CHILD: &str = "RCA_READY_ALLOCATION_CHILD";
        if std::env::var(CHILD).as_deref() == Ok(name) {
            return true;
        }
        child_process::run_test_child(
            &format!("ready_allocations::tests::{name}"),
            CHILD,
            name,
            &[],
        );
        false
    }

    #[test]
    fn child_report_requires_exactly_one_passed_test() {
        assert!(!child_process::passed_exactly_one(
            "test result: ok. 11 passed; 0 failed; 0 ignored;"
        ));
    }

    fn empty_work() -> Work {
        Work {
            trace: Trace::default(),
            counts: (0, 0),
        }
    }

    struct Fixture {
        construction: Box<[u8; 64]>,
        initial: Option<Box<[u8; 128]>>,
        checked: Option<Box<[u8; 256]>>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            black_box(&self.construction);
            drop(black_box(Box::new([0_u8; 512])));
        }
    }

    #[test]
    fn construction_initial_checked_and_destructor_are_separate() {
        if !in_child("construction_initial_checked_and_destructor_are_separate") {
            return;
        }
        let measurement = capture(|observe| {
            let mut fixture = Fixture {
                construction: black_box(Box::new([0; 64])),
                initial: None,
                checked: None,
            };
            observe(Point::Construction);
            fixture.initial = Some(black_box(Box::new([0; 128])));
            observe(Point::Initial);
            fixture.checked = Some(black_box(Box::new([0; 256])));
            observe(Point::Checked);
            drop(fixture);
            Ok(empty_work())
        })
        .expect("attribution capture");
        let [a, b, c, d, e] = measurement.snapshots;
        for (before, after, bytes) in [(a, b, 64), (b, c, 128), (c, d, 256), (d, e, 512)] {
            assert_eq!(after.allocated_blocks - before.allocated_blocks, 1);
            assert_eq!(after.allocated_bytes - before.allocated_bytes, bytes);
        }
        assert_eq!(d.live_blocks - a.live_blocks, 3);
        assert_eq!(d.live_bytes - a.live_bytes, 448);
        assert_eq!((e.live_blocks, e.live_bytes), (a.live_blocks, a.live_bytes));
    }

    #[test]
    fn snapshots_and_scalar_accounting_allocate_nothing() {
        if !in_child("snapshots_and_scalar_accounting_allocate_nothing") {
            return;
        }
        let measurement = capture(|observe| {
            for point in [Point::Construction, Point::Initial, Point::Checked] {
                observe(point);
            }
            Ok(empty_work())
        })
        .expect("empty capture");
        for snapshot in measurement.snapshots {
            assert_eq!(
                snapshot,
                Snapshot {
                    allocated_blocks: 0,
                    allocated_bytes: 0,
                    live_blocks: 0,
                    live_bytes: 0
                }
            );
        }
    }

    #[test]
    fn missing_duplicate_and_reordered_hooks_are_rejected() {
        if !in_child("missing_duplicate_and_reordered_hooks_are_rejected") {
            return;
        }
        for points in [
            vec![],
            vec![Point::Construction, Point::Initial],
            vec![Point::Initial, Point::Construction, Point::Checked],
            vec![
                Point::Construction,
                Point::Initial,
                Point::Checked,
                Point::Checked,
            ],
        ] {
            capture(|observe| {
                for point in points {
                    observe(point);
                }
                Ok(empty_work())
            })
            .expect_err("malformed snapshot schedule");
        }
    }

    #[test]
    fn worker_allocations_are_process_wide() {
        if !in_child("worker_allocations_are_process_wide") {
            return;
        }
        let measurement = capture(|observe| {
            let allocation = std::thread::spawn(|| black_box(Box::new([0_u8; 4096])))
                .join()
                .expect("worker joined");
            observe(Point::Construction);
            observe(Point::Initial);
            observe(Point::Checked);
            drop(black_box(allocation));
            Ok(empty_work())
        })
        .expect("worker capture");
        let [a, b, _, _, e] = measurement.snapshots;
        assert!(b.allocated_bytes - a.allocated_bytes >= 4096);
        assert_eq!(b.live_bytes - a.live_bytes, 4096);
        assert_eq!(e.live_bytes, a.live_bytes);
    }

    #[test]
    fn failed_work_returns_no_measurement_and_stops_profiler() {
        if !in_child("failed_work_returns_no_measurement_and_stops_profiler") {
            return;
        }
        capture(|observe| {
            let allocation = black_box(Box::new([0_u8; 64]));
            observe(Point::Construction);
            drop(allocation);
            Err("rejected work")
        })
        .expect_err("no result for failed work");
        // A subsequent isolated session would panic if the first profiler leaked.
        capture(|observe| {
            for point in [Point::Construction, Point::Initial, Point::Checked] {
                observe(point);
            }
            Ok(empty_work())
        })
        .expect("failed capture stopped its profiler");
    }
}
