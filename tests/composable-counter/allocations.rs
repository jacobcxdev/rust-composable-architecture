//! Isolated allocation windows for construction, committed work and joined teardown.

use super::{Phase, Work};

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Keeps cumulative allocations separate from current live allocations.
#[derive(Clone, Copy, Debug)]
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
        format!(
            "{{\"allocated_blocks\":{},\"allocated_bytes\":{},\"live_blocks\":{},\"live_bytes\":{}}}",
            self.allocated_blocks, self.allocated_bytes, self.live_blocks, self.live_bytes,
        )
    }
}

/// Retains stack-only observations until the process-wide profiler has stopped.
struct Measurement {
    snapshots: [Snapshot; 5],
    work: Work,
    final_state: u64,
}

/// Captures all five boundaries before any ledger allocation or formatting.
fn capture<Adapter>(
    create: impl FnOnce() -> Result<Adapter, &'static str>,
    mut send: impl FnMut(&mut Adapter) -> Result<(), &'static str>,
    finish: impl FnOnce(Adapter) -> Result<u64, &'static str>,
) -> Result<Measurement, &'static str> {
    let mut work = Work::new();
    // One process-wide session also tracks frees of construction allocations.
    let profiler = dhat::Profiler::builder().testing().build();
    let a = Snapshot::now();
    let mut adapter = create()?;
    let b = Snapshot::now();
    for _ in 0..20 {
        work.attempt(Phase::Warmup);
        send(&mut adapter)?;
        work.complete(Phase::Warmup);
    }
    let c = Snapshot::now();
    for _ in 0..10_000 {
        work.attempt(Phase::Checked);
        send(&mut adapter)?;
        work.complete(Phase::Checked);
    }
    let d = Snapshot::now();
    let final_state = finish(adapter)?;
    let e = Snapshot::now();
    drop(profiler);
    Ok(Measurement {
        snapshots: [a, b, c, d, e],
        work,
        final_state,
    })
}

fn phase(before: Snapshot, after: Snapshot, operations: u64) -> String {
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

/// Captures allocation windows without including parsing or reporting.
pub fn run<Adapter>(
    engine: &str,
    create: impl FnOnce() -> Result<Adapter, &'static str>,
    send: impl FnMut(&mut Adapter) -> Result<(), &'static str>,
    finish: impl FnOnce(Adapter) -> Result<u64, &'static str>,
) -> Result<String, &'static str> {
    let measurement = capture(create, send, finish)?;
    let work = measurement
        .work
        .finish(engine, measurement.final_state, true, true)?;
    let [a, b, c, d, e] = measurement.snapshots;
    let construction = phase(a, b, 1);
    let warmup = phase(b, c, 20);
    let checked = phase(c, d, 10_000);
    let teardown = phase(d, e, 1);
    // One setup consists of construction followed by twenty warm-up actions.
    let setup = phase(a, c, 1);
    Ok(format!(
        "{{\"schema\":\"composable-counter-allocations-v1\",\"engine\":\"{engine}\",\"instrument\":\"dhat-0.3.3\",\"work\":{work},\"snapshots\":{{\"A\":{},\"B\":{},\"C\":{},\"D\":{},\"E\":{}}},\"phases\":{{\"construction\":{construction},\"warmup\":{warmup},\"checked\":{checked},\"teardown\":{teardown},\"setup\":{setup}}}}}",
        a.json(), b.json(), c.json(), d.json(), e.json(),
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::hint::black_box;

    use super::{capture, Measurement};

    // Reuse the isolated runner, including its exact one-test summary check.
    mod child {
        include!("../../rca-core/src/allocation_test/child.rs");
    }

    fn in_child(name: &str) -> bool {
        !child::run_in_child(&format!("common::allocations::tests::{name}"))
    }

    /// Keeps allocation controls separate from the threaded semantic tests.
    pub(crate) const CASES: &[(&str, fn())] = &[
        ("common::allocations::tests::construction_send_and_destructor_have_separate_windows", construction_send_and_destructor_have_separate_windows),
        ("common::allocations::tests::warmup_allocations_are_excluded_from_checked_work", warmup_allocations_are_excluded_from_checked_work),
        ("common::allocations::tests::teardown_releases_allocations_from_earlier_windows", teardown_releases_allocations_from_earlier_windows),
        ("common::allocations::tests::snapshots_and_empty_accounting_allocate_nothing", snapshots_and_empty_accounting_allocate_nothing),
        ("common::allocations::tests::retained_worker_allocation_is_counted", retained_worker_allocation_is_counted),
        ("common::allocations::tests::reallocation_counts_full_new_size_and_one_allocation_event", reallocation_counts_full_new_size_and_one_allocation_event),
        ("common::allocations::tests::failed_work_never_emits_allocation_measurements", failed_work_never_emits_allocation_measurements),
    ];

    #[test]
    fn allocation_case_roster_preserves_exact_names() {
        let expected = [
            "common::allocations::tests::construction_send_and_destructor_have_separate_windows",
            "common::allocations::tests::warmup_allocations_are_excluded_from_checked_work",
            "common::allocations::tests::teardown_releases_allocations_from_earlier_windows",
            "common::allocations::tests::snapshots_and_empty_accounting_allocate_nothing",
            "common::allocations::tests::retained_worker_allocation_is_counted",
            "common::allocations::tests::reallocation_counts_full_new_size_and_one_allocation_event",
            "common::allocations::tests::failed_work_never_emits_allocation_measurements",
        ];
        assert_eq!(
            CASES.iter().map(|case| case.0).collect::<Vec<_>>(),
            expected
        );
    }

    struct Fixture {
        value: u64,
        initial: Box<[u8; 64]>,
        warmup: Option<Box<[u8; 128]>>,
        checked: Option<Box<[u8; 256]>>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            drop(black_box(Box::new([0_u8; 512])));
            black_box(&self.initial);
        }
    }

    fn attributed() -> Measurement {
        capture(
            || {
                Ok(Fixture {
                    value: 0,
                    initial: black_box(Box::new([0; 64])),
                    warmup: None,
                    checked: None,
                })
            },
            |fixture| {
                fixture.value += 1;
                if fixture.value == 1 {
                    fixture.warmup = Some(black_box(Box::new([0; 128])));
                }
                if fixture.value == 21 {
                    fixture.checked = Some(black_box(Box::new([0; 256])));
                }
                Ok(())
            },
            |fixture| Ok(fixture.value),
        )
        .expect("allocation capture succeeds")
    }

    fn construction_send_and_destructor_have_separate_windows() {
        if !in_child("construction_send_and_destructor_have_separate_windows") {
            return;
        }
        let [a, b, c, d, e] = attributed().snapshots;
        assert_eq!(b.allocated_blocks - a.allocated_blocks, 1);
        assert_eq!(b.allocated_bytes - a.allocated_bytes, 64);
        assert_eq!(d.allocated_blocks - c.allocated_blocks, 1);
        assert_eq!(d.allocated_bytes - c.allocated_bytes, 256);
        assert_eq!(e.allocated_blocks - d.allocated_blocks, 1);
        assert_eq!(e.allocated_bytes - d.allocated_bytes, 512);
    }

    fn warmup_allocations_are_excluded_from_checked_work() {
        if !in_child("warmup_allocations_are_excluded_from_checked_work") {
            return;
        }
        let [_, b, c, d, _] = attributed().snapshots;
        assert_eq!(c.allocated_bytes - b.allocated_bytes, 128);
        assert_eq!(d.allocated_bytes - c.allocated_bytes, 256);
    }

    fn teardown_releases_allocations_from_earlier_windows() {
        if !in_child("teardown_releases_allocations_from_earlier_windows") {
            return;
        }
        let [a, _, _, d, e] = attributed().snapshots;
        assert_eq!(d.live_blocks - a.live_blocks, 3);
        assert_eq!(d.live_bytes - a.live_bytes, 448);
        assert_eq!(e.live_blocks, a.live_blocks);
        assert_eq!(e.live_bytes, a.live_bytes);
    }

    fn snapshots_and_empty_accounting_allocate_nothing() {
        if !in_child("snapshots_and_empty_accounting_allocate_nothing") {
            return;
        }
        let measurement = capture(
            || Ok(0_u64),
            |value| {
                *value += 1;
                Ok(())
            },
            Ok,
        )
        .expect("empty allocation capture succeeds");
        for snapshot in measurement.snapshots {
            assert_eq!(snapshot.allocated_blocks, 0);
            assert_eq!(snapshot.allocated_bytes, 0);
            assert_eq!(snapshot.live_blocks, 0);
            assert_eq!(snapshot.live_bytes, 0);
        }
    }

    fn retained_worker_allocation_is_counted() {
        if !in_child("retained_worker_allocation_is_counted") {
            return;
        }
        let measurement = capture(
            || {
                let allocation = std::thread::spawn(|| black_box(Box::new([0_u8; 131_072])))
                    .join()
                    .expect("worker joined");
                Ok((0_u64, allocation))
            },
            |(value, _)| {
                *value += 1;
                Ok(())
            },
            |(value, allocation)| {
                drop(black_box(allocation));
                Ok(value)
            },
        )
        .expect("worker allocation capture succeeds");
        let [a, b, _, _, e] = measurement.snapshots;
        assert!(b.allocated_bytes - a.allocated_bytes >= 131_072);
        assert_eq!(b.live_bytes - a.live_bytes, 131_072);
        assert_eq!(e.live_bytes, a.live_bytes);
    }

    fn reallocation_counts_full_new_size_and_one_allocation_event() {
        if !in_child("reallocation_counts_full_new_size_and_one_allocation_event") {
            return;
        }
        let measurement = capture(
            || Ok((0_u64, black_box(vec![0_u8; 8]))),
            |(value, bytes)| {
                *value += 1;
                if *value == 21 {
                    bytes.reserve_exact(8);
                    bytes.resize(16, 0);
                    black_box(&bytes);
                }
                Ok(())
            },
            |(value, bytes)| {
                drop(black_box(bytes));
                Ok(value)
            },
        )
        .expect("reallocation capture succeeds");
        let [a, b, c, d, e] = measurement.snapshots;
        assert_eq!(b.allocated_bytes - a.allocated_bytes, 8);
        assert_eq!(d.allocated_blocks - c.allocated_blocks, 1);
        assert_eq!(d.allocated_bytes - c.allocated_bytes, 16);
        assert_eq!(d.live_bytes - a.live_bytes, 16);
        assert_eq!(e.live_bytes, a.live_bytes);
    }

    fn failed_work_never_emits_allocation_measurements() {
        if !in_child("failed_work_never_emits_allocation_measurements") {
            return;
        }
        assert!(super::run("control", || Err::<u64, _>("create failed"), |_| Ok(()), Ok).is_err());
        assert!(super::run("control", || Ok(0_u64), |_| Err("send failed"), Ok).is_err());
        assert!(super::run(
            "control",
            || Ok(0_u64),
            |_| Ok(()),
            |_| Err("finish failed")
        )
        .is_err());
        assert!(super::run("control", || Ok(0_u64), |_| Ok(()), Ok).is_err());
    }
}
