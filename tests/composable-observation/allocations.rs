//! Allocation traffic across completed publication stages, with reporting excluded.

use crate::adapter::common::Point;

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

const POINTS: [Point; 5] = [
    Point::Construction,
    Point::Registration,
    Point::Initial,
    Point::Checked,
    Point::Unregistration,
];

/// Separates cumulative allocation traffic from memory that remains live.
#[derive(Clone, Copy, Debug, Default)]
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

/// Rejects missing, repeated or out-of-order boundaries before producing a report.
fn capture(
    exercise: impl FnOnce(&mut dyn FnMut(Point)) -> Result<(), &'static str>,
) -> Result<[Snapshot; 7], &'static str> {
    let profiler = dhat::Profiler::builder().testing().build();
    let mut snapshots = [Snapshot::default(); 7];
    snapshots[0] = Snapshot::now();
    let mut count = 0;
    let mut invalid = false;
    exercise(&mut |point| {
        if POINTS.get(count) != Some(&point) {
            invalid = true;
        } else {
            snapshots[count + 1] = Snapshot::now();
            count += 1;
        }
    })?;
    snapshots[6] = Snapshot::now();
    drop(profiler);
    if invalid || count != POINTS.len() {
        return Err("invalid allocation boundaries");
    }
    Ok(snapshots)
}

/// Reports only after the adapter validates publications, final state and host teardown.
pub fn run(
    engine: &str,
    exercise: impl FnOnce(&mut dyn FnMut(Point)) -> Result<(), &'static str>,
) -> Result<String, &'static str> {
    let snapshots = capture(exercise)?;
    let names = [
        "construction",
        "registration",
        "initial",
        "checked",
        "unregistration",
        "teardown",
    ];
    let operations = [1, 1, 2, 2, 1, 1];
    let phases = snapshots
        .windows(2)
        .zip(names)
        .zip(operations)
        .map(|((pair, name), operations)| {
            let blocks = pair[1]
                .allocated_blocks
                .checked_sub(pair[0].allocated_blocks)
                .expect("cumulative blocks cannot decrease");
            let bytes = pair[1]
                .allocated_bytes
                .checked_sub(pair[0].allocated_bytes)
                .expect("cumulative bytes cannot decrease");
            format!("\"{name}\":{{\"operations\":{operations},\"allocated_blocks\":{blocks},\"allocated_bytes\":{bytes}}}")
        })
        .collect::<Vec<_>>()
        .join(",");
    let snapshots = snapshots
        .into_iter()
        .zip(['A', 'B', 'C', 'D', 'E', 'F', 'G'])
        .map(|(snapshot, name)| format!("\"{name}\":{}", snapshot.json()))
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!("{{\"schema\":\"composable-observation-allocations-v1\",\"engine\":\"{engine}\",\"instrument\":\"dhat-0.3.3\",\"work\":{{\"trace\":[0,3,1,1,4],\"updates\":5,\"publications\":5,\"final_value\":9,\"previous_value\":4,\"state_extracted\":true,\"host_stopped\":true}},\"snapshots\":{{{snapshots}}},\"phases\":{{{phases}}}}}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;

    // Keep profiler isolation, exact test selection and bounded capture in one helper.
    mod child {
        include!("../../rca-core/src/allocation_test/child.rs");
    }

    #[test]
    fn isolated_allocation_controls() {
        if child::run_in_child("allocations::tests::isolated_allocation_controls") {
            return;
        }
        let empty = capture(|observe| {
            for point in POINTS {
                observe(point);
            }
            Ok(())
        })
        .unwrap();
        assert!(empty.iter().all(|s| s.allocated_blocks == 0
            && s.allocated_bytes == 0
            && s.live_blocks == 0
            && s.live_bytes == 0));
        let measured = capture(|observe| {
            let held = black_box(Box::new([0_u8; 64]));
            observe(Point::Construction);
            drop(black_box(Box::new([0_u8; 128])));
            observe(Point::Registration);
            observe(Point::Initial);
            observe(Point::Checked);
            observe(Point::Unregistration);
            drop(held);
            drop(black_box(Box::new([0_u8; 256])));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            measured[1].allocated_bytes - measured[0].allocated_bytes,
            64
        );
        assert_eq!(
            measured[2].allocated_bytes - measured[1].allocated_bytes,
            128
        );
        assert_eq!(
            measured[6].allocated_bytes - measured[5].allocated_bytes,
            256
        );
        assert_eq!(measured[5].live_bytes - measured[0].live_bytes, 64);
        assert_eq!(measured[6].live_bytes, measured[0].live_bytes);
        capture(|observe| {
            std::thread::spawn(|| {
                let before = Snapshot::now();
                let held = black_box(Box::new([0_u8; 512]));
                let after = Snapshot::now();
                assert_eq!(after.allocated_bytes - before.allocated_bytes, 512);
                assert_eq!(after.live_bytes - before.live_bytes, 512);
                drop(held);
            })
            .join()
            .unwrap();
            for point in POINTS {
                observe(point);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            run("control", |_| Ok(())).unwrap_err(),
            "invalid allocation boundaries"
        );
        assert_eq!(
            run("control", |observe| {
                for point in POINTS.into_iter().rev() {
                    observe(point);
                }
                Ok(())
            })
            .unwrap_err(),
            "invalid allocation boundaries"
        );
        assert_eq!(
            run("control", |observe| {
                for point in POINTS {
                    observe(point);
                }
                observe(Point::Unregistration);
                Ok(())
            })
            .unwrap_err(),
            "invalid allocation boundaries"
        );
        assert_eq!(
            run("control", |observe| {
                for point in POINTS {
                    observe(point);
                }
                Err("failed semantics")
            })
            .unwrap_err(),
            "failed semantics"
        );
        assert_eq!(
            run("control", |observe| {
                crate::adapter::exercise_observed(
                    crate::adapter::common::Mode::KeepRegistered,
                    observe,
                )
            })
            .unwrap_err(),
            "publication remained active"
        );
    }
}
