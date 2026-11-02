//! Shared work checks and explicit measurement boundaries for native counter adapters.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A deadlock watchdog, not a performance threshold.
pub const PROOF_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(feature = "allocation-measurement")]
pub mod allocations;

/// Measures one cold lifecycle, ending only after the adapter releases its host.
pub fn run_lifecycle<Adapter>(
    engine: &str,
    create: impl FnOnce() -> Result<Adapter, &'static str>,
    send: impl FnOnce(&mut Adapter) -> Result<(), &'static str>,
    finish: impl FnOnce(Adapter) -> Result<u64, &'static str>,
) -> Result<String, &'static str> {
    run_lifecycle_with_clock(engine, create, send, finish, Instant::now)
}

// A clock input makes boundary tests independent of host scheduling delays.
fn run_lifecycle_with_clock<Adapter>(
    engine: &str,
    create: impl FnOnce() -> Result<Adapter, &'static str>,
    send: impl FnOnce(&mut Adapter) -> Result<(), &'static str>,
    finish: impl FnOnce(Adapter) -> Result<u64, &'static str>,
    mut now: impl FnMut() -> Instant,
) -> Result<String, &'static str> {
    let start = now();
    let mut adapter = create()?;
    send(&mut adapter)?;
    let final_state = finish(adapter)?;
    let elapsed_ns = now().duration_since(start).as_nanos();
    if final_state != 1 {
        return Err("counter lifecycle work was not completed");
    }
    Ok(format!(
        "{{\"schema\":\"composable-counter-lifecycle-v1\",\"clock\":\"std::time::Instant\",\"unit\":\"ns\",\"engine\":\"{engine}\",\"operations_expected\":1,\"operations_attempted\":1,\"operations_completed\":1,\"initial\":0,\"final\":{final_state},\"errors\":0,\"state_extracted\":true,\"host_stopped\":true,\"elapsed_ns\":{elapsed_ns}}}"
    ))
}

/// Selects one output contract. Measurement modes cannot be combined.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Correctness,
    Operations,
    Lifecycle,
    #[cfg(feature = "allocation-measurement")]
    Allocations,
}

/// Selects correctness-only output or one opt-in measurement mode.
pub fn mode(arguments: impl Iterator<Item = std::ffi::OsString>) -> Result<Mode, &'static str> {
    let arguments: Vec<_> = arguments.collect();
    match arguments.as_slice() {
        [] => Ok(Mode::Correctness),
        [argument] if argument == "--measure" => Ok(Mode::Operations),
        [argument] if argument == "--measure-lifecycle" => Ok(Mode::Lifecycle),
        #[cfg(feature = "allocation-measurement")]
        [argument] if argument == "--measure-allocations" => Ok(Mode::Allocations),
        #[cfg(feature = "allocation-measurement")]
        _ => Err("usage: counter [--measure | --measure-lifecycle | --measure-allocations]"),
        #[cfg(not(feature = "allocation-measurement"))]
        _ => Err("usage: counter [--measure | --measure-lifecycle]"),
    }
}

/// Runs the fixed workload through the same committed-operation boundary.
pub fn run_work(
    mut send: impl FnMut() -> Result<(), &'static str>,
    measured: bool,
) -> Result<(Work, Option<Timing>), &'static str> {
    let mut work = Work::new();
    for _ in 0..20 {
        work.attempt(Phase::Warmup);
        send()?;
        work.complete(Phase::Warmup);
    }
    // Allocate before timing; retain one raw latency for each checked operation.
    let mut samples = measured.then(|| Vec::with_capacity(10_000));
    let loop_start = measured.then(Instant::now);
    for _ in 0..10_000 {
        work.attempt(Phase::Checked);
        let start = measured.then(Instant::now);
        send()?;
        let elapsed = start.map(|start| start.elapsed().as_nanos());
        work.complete(Phase::Checked);
        if let (Some(samples), Some(elapsed)) = (&mut samples, elapsed) {
            samples.push(elapsed);
        }
    }
    let loop_ns = loop_start.map(|start| start.elapsed().as_nanos());
    let timing = samples.zip(loop_ns).map(|(samples_ns, loop_ns)| Timing {
        samples_ns,
        loop_ns,
    });
    Ok((work, timing))
}

/// Stores raw operation samples and the duration of their enclosing loop.
#[derive(Debug)]
pub struct Timing {
    samples_ns: Vec<u128>,
    loop_ns: u128,
}

impl Timing {
    /// Embeds a previously validated ledger after extraction and host teardown.
    pub fn finish(self, work: &str) -> String {
        format!(
            "{{\"schema\":\"composable-counter-timing-v1\",\"clock\":\"std::time::Instant\",\"unit\":\"ns\",\"work\":{work},\"loop_ns\":{},\"samples_ns\":{:?}}}",
            self.loop_ns, self.samples_ns,
        )
    }
}

/// Separates host warm-up from checked actions.
#[derive(Clone, Copy)]
pub enum Phase {
    Warmup,
    Checked,
}

/// Counts attempted and completed sends before a ledger can be emitted.
pub struct Work {
    warmup_attempted: u64,
    warmup_completed: u64,
    operations_attempted: u64,
    operations_completed: u64,
    errors: u64,
}

impl Work {
    /// Starts empty counters before any warm-up action.
    pub const fn new() -> Self {
        Self {
            warmup_attempted: 0,
            warmup_completed: 0,
            operations_attempted: 0,
            operations_completed: 0,
            errors: 0,
        }
    }

    /// Records entry into a send, independently of its completion.
    pub fn attempt(&mut self, phase: Phase) {
        match phase {
            Phase::Warmup => self.warmup_attempted += 1,
            Phase::Checked => self.operations_attempted += 1,
        }
    }

    /// Records a send only after its committed-operation boundary returns.
    pub fn complete(&mut self, phase: Phase) {
        match phase {
            Phase::Warmup => self.warmup_completed += 1,
            Phase::Checked => self.operations_completed += 1,
        }
    }

    #[cfg(test)]
    pub fn error(&mut self) {
        self.errors += 1;
    }

    /// Rejects incomplete work or teardown before formatting the fixed output contract.
    pub fn finish(
        &self,
        engine: &str,
        final_state: u64,
        state_extracted: bool,
        host_stopped: bool,
    ) -> Result<String, &'static str> {
        if self.warmup_attempted != 20
            || self.warmup_completed != 20
            || self.operations_attempted != 10_000
            || self.operations_completed != 10_000
            || self.errors != 0
            || final_state != 10_020
            || !state_extracted
            || !host_stopped
        {
            return Err("counter work was not completed");
        }
        Ok(format!(
            "{{\"schema\":\"composable-counter-work-v1\",\"engine\":\"{engine}\",\"warmup_expected\":20,\"warmup_attempted\":{},\"warmup_completed\":{},\"operations_expected\":10000,\"operations_attempted\":{},\"operations_completed\":{},\"initial\":0,\"final\":{final_state},\"errors\":{},\"state_extracted\":{state_extracted},\"host_stopped\":{host_stopped}}}",
            self.warmup_attempted,
            self.warmup_completed,
            self.operations_attempted,
            self.operations_completed,
            self.errors,
        ))
    }
}

/// Blocks a reducer boundary while a separate controller observes completion.
pub struct Gate {
    entry: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    completions: Arc<AtomicU64>,
    timed_out: Arc<AtomicBool>,
    #[cfg(test)]
    on_drop: bool,
}

#[cfg(test)]
/// Observes and releases a reducer gate without relying on elapsed time.
pub struct GateControl {
    entry: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
    completions: Arc<AtomicU64>,
    timed_out: Arc<AtomicBool>,
}

#[cfg(test)]
/// Builds a gate that blocks before reduction starts.
pub fn gate() -> (Gate, GateControl) {
    let (entry_tx, entry_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let completions = Arc::new(AtomicU64::new(0));
    let timed_out = Arc::new(AtomicBool::new(false));
    (
        Gate {
            entry: entry_tx,
            release: release_rx,
            completions: Arc::clone(&completions),
            timed_out: Arc::clone(&timed_out),
            on_drop: false,
        },
        GateControl {
            entry: entry_rx,
            release: release_tx,
            completions,
            timed_out,
        },
    )
}

impl Gate {
    /// Blocks normal reduction gates, leaving drop-only gates untouched.
    pub fn before_reduce(&self) {
        #[cfg(test)]
        if self.on_drop {
            return;
        }
        self.wait_for_release();
    }

    fn wait_for_release(&self) {
        if self.entry.send(()).is_err() || self.release.recv_timeout(PROOF_TIMEOUT).is_err() {
            self.timed_out.store(true, Ordering::Release);
        }
    }

    /// Publishes a completion count only after the reducer changes state.
    pub fn reduced(&self) {
        self.completions.fetch_add(1, Ordering::Release);
    }
}

#[cfg(test)]
/// Builds a gate that blocks reducer destruction instead of reduction.
pub fn drop_gate() -> (Gate, GateControl) {
    let (mut gate, control) = gate();
    gate.on_drop = true;
    (gate, control)
}

#[cfg(test)]
impl Drop for Gate {
    fn drop(&mut self) {
        if self.on_drop {
            self.wait_for_release();
        }
    }
}

#[cfg(test)]
impl GateControl {
    pub fn wait_entered(&self) -> bool {
        self.entry.recv_timeout(PROOF_TIMEOUT).is_ok()
    }

    pub fn release(&self) {
        let _ = self.release.send(());
    }

    pub fn completions(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.completions)
    }

    pub fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::{Phase, Work};

    #[test]
    fn lifecycle_covers_construction_work_and_teardown() {
        let stages = std::cell::RefCell::new(Vec::new());
        let clock = std::cell::Cell::new(std::time::Instant::now());
        let step = std::time::Duration::from_millis(2);
        let output = super::run_lifecycle_with_clock(
            "rca",
            || {
                stages.borrow_mut().push("construct");
                clock.set(clock.get() + step);
                Ok(0_u64)
            },
            |state| {
                stages.borrow_mut().push("commit");
                clock.set(clock.get() + step);
                *state += 1;
                Ok(())
            },
            |state| {
                stages.borrow_mut().push("teardown");
                clock.set(clock.get() + step);
                Ok(state)
            },
            || clock.get(),
        )
        .expect("completed lifecycle");
        assert_eq!(*stages.borrow(), ["construct", "commit", "teardown"]);
        let elapsed = output
            .split_once("\"elapsed_ns\":")
            .unwrap()
            .1
            .strip_suffix('}')
            .unwrap()
            .parse::<u128>()
            .unwrap();
        assert_eq!(elapsed, 6_000_000);
    }

    #[test]
    fn lifecycle_rejects_failure_at_every_boundary_and_wrong_state() {
        for fail_at in 0..4 {
            let stages = std::cell::Cell::new(0);
            let output = super::run_lifecycle(
                "rca",
                || {
                    stages.set(1);
                    if fail_at == 0 {
                        Err("construct")
                    } else {
                        Ok(0_u64)
                    }
                },
                |state| {
                    stages.set(2);
                    *state += 1;
                    if fail_at == 1 {
                        Err("commit")
                    } else {
                        Ok(())
                    }
                },
                |_state| {
                    stages.set(3);
                    if fail_at == 2 {
                        Err("teardown")
                    } else {
                        Ok(0)
                    }
                },
            );
            assert!(output.is_err());
            assert_eq!(stages.get(), (fail_at + 1).min(3));
        }
    }

    #[test]
    fn measured_work_retains_every_completed_sample() {
        let mut calls = 0;
        let (work, timing) = super::run_work(
            || {
                calls += 1;
                Ok(())
            },
            true,
        )
        .unwrap();
        assert_eq!(calls, 10_020);
        let timing = timing.unwrap();
        assert_eq!(timing.samples_ns.len(), 10_000);
        assert!(timing.loop_ns > 0);
        assert!(timing.samples_ns.iter().sum::<u128>() <= timing.loop_ns);
        let ledger = work.finish("rca", calls, true, true).unwrap();
        assert!(timing
            .finish(&ledger)
            .contains("\"work\":{\"schema\":\"composable-counter-work-v1\""));
    }

    #[test]
    fn failed_operation_never_yields_measurement() {
        for fail_at in [1, 20, 21, 10_020] {
            let mut calls = 0;
            let result = super::run_work(
                || {
                    calls += 1;
                    if calls == fail_at {
                        Err("failed send")
                    } else {
                        Ok(())
                    }
                },
                true,
            );
            assert!(matches!(result, Err("failed send")));
            assert_eq!(calls, fail_at);
        }
    }

    #[test]
    fn latency_includes_time_inside_committed_operation() {
        let mut calls = 0;
        let (_, timing) = super::run_work(
            || {
                calls += 1;
                if calls == 21 {
                    // Deliberately slow the first checked operation, not warm-up.
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Ok(())
            },
            true,
        )
        .unwrap();
        assert!(timing.unwrap().samples_ns[0] >= 2_000_000);
    }

    #[test]
    fn correctness_mode_keeps_timing_disabled() {
        let (work, timing) = super::run_work(|| Ok(()), false).unwrap();
        assert!(timing.is_none());
        assert!(work.finish("rca", 10_020, true, true).is_ok());
    }

    #[test]
    fn arguments_reject_unknown_or_repeated_modes() {
        for arguments in [
            vec!["unknown"],
            vec!["--measure", "--measure"],
            vec!["--measure-lifecycle", "--measure-lifecycle"],
            vec!["--measure", "--measure-lifecycle"],
        ] {
            assert!(super::mode(arguments.into_iter().map(Into::into)).is_err());
        }
        assert_eq!(
            super::mode(std::iter::empty()).unwrap(),
            super::Mode::Correctness
        );
        assert_eq!(
            super::mode(["--measure"].into_iter().map(Into::into)).unwrap(),
            super::Mode::Operations,
        );
        assert_eq!(
            super::mode(["--measure-lifecycle"].into_iter().map(Into::into)).unwrap(),
            super::Mode::Lifecycle,
        );
    }

    #[test]
    fn exact_work_is_accepted() {
        let mut work = Work::new();
        for _ in 0..20 {
            work.attempt(Phase::Warmup);
            work.complete(Phase::Warmup);
        }
        for _ in 0..10_000 {
            work.attempt(Phase::Checked);
            work.complete(Phase::Checked);
        }
        assert!(work.finish("rca", 10_020, true, true).is_ok());
    }

    #[test]
    fn omitted_duplicate_and_wrong_phase_work_is_rejected() {
        for (warmup, checked) in [(19, 10_000), (21, 10_000), (20, 9_999), (20, 10_001)] {
            let mut work = Work::new();
            for _ in 0..warmup {
                work.attempt(Phase::Warmup);
                work.complete(Phase::Warmup);
            }
            for _ in 0..checked {
                work.attempt(Phase::Checked);
                work.complete(Phase::Checked);
            }
            assert!(work.finish("rca", 10_020, true, true).is_err());
        }
    }

    #[test]
    fn errors_or_missing_boundaries_are_rejected() {
        let mut work = Work::new();
        for _ in 0..20 {
            work.attempt(Phase::Warmup);
            work.complete(Phase::Warmup);
        }
        for _ in 0..10_000 {
            work.attempt(Phase::Checked);
            work.complete(Phase::Checked);
        }
        work.error();
        assert!(work.finish("rca", 10_020, true, true).is_err());
        work = Work::new();
        for _ in 0..20 {
            work.attempt(Phase::Warmup);
            work.complete(Phase::Warmup);
        }
        for _ in 0..10_000 {
            work.attempt(Phase::Checked);
            work.complete(Phase::Checked);
        }
        assert!(work.finish("rca", 10_020, false, true).is_err());
        assert!(work.finish("rca", 10_020, true, false).is_err());
    }
}
