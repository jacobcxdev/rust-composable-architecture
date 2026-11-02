//! Fixed inputs and independent expected results for both native keyed adapters.

use std::time::Duration;

/// Selects the native behaviour or one deliberately incorrect policy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Keeps policy and progress in the selected child.
    Native,
    /// Shares one mutable policy across otherwise independent children.
    SharedPolicy,
    /// Sends each completed value to the other child.
    WrongKey,
    /// Retains old policy when replacing a child with fresh state.
    RetainPolicy,
}

/// Changes a child or records the value sent back by its effect.
#[derive(Clone, Debug)]
pub enum ChildAction {
    SetStep(u64),
    Step,
    Completed { value: u64, wrong_key: bool },
}

/// Per-child policy, progress and output history; deliberately not `Clone`.
#[derive(Debug)]
pub struct ChildData {
    pub next: u64,
    pub step: u64,
    pub completed: Vec<u64>,
}

impl ChildData {
    /// Starts a child without carrying output history from an earlier lifetime.
    pub fn new(next: u64, step: u64) -> Self {
        Self {
            next,
            step,
            completed: Vec::new(),
        }
    }
}

/// An external command translated into each engine's native action type.
pub enum Input {
    SetStep(u8, u64),
    Step(u8),
    Replace(u8),
}

/// Seven commands whose effects must complete before the next command starts.
pub const INPUTS: &[Input] = &[
    Input::SetStep(1, 4),
    Input::Step(1),
    Input::Step(2),
    Input::Step(1),
    Input::Replace(1),
    Input::Step(1),
    Input::Step(2),
];
/// Expected keyed output order, written independently of either reducer.
pub const TRACE: &[(u8, u64)] = &[(1, 10), (2, 100), (1, 14), (1, 7), (2, 110)];
/// Completed child outputs required before each external operation returns.
pub const COMPLETED: [u64; 7] = [0, 1, 2, 3, 3, 4, 5];
/// Child ID, next value, step and completed values after host shutdown.
pub type Row = (u8, u64, u64, Vec<u64>);

/// Checks literal results while ignoring each engine's map iteration order.
pub fn verify(
    trace: &[(u8, u64)],
    mut rows: Vec<Row>,
    reductions: usize,
) -> Result<(), &'static str> {
    if trace != TRACE {
        return Err("wrong keyed trace");
    }
    rows.sort_by_key(|row| row.0);
    if rows != vec![(1, 10, 3, vec![7]), (2, 120, 10, vec![100, 110])] {
        return Err("wrong final children");
    }
    if reductions != 12 {
        return Err("wrong reduction count");
    }
    Ok(())
}

/// Bounds observation to ten seconds, then joins a successfully finished host.
///
/// A timeout cannot stop the worker thread. The owning test process must also
/// run under the external deadline described in the fixture README.
pub fn bounded(
    exercise: fn(Mode) -> Result<(), &'static str>,
    mode: Mode,
) -> Result<(), &'static str> {
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        send.send(exercise(mode)).unwrap();
    });
    let result = receive
        .recv_timeout(Duration::from_secs(10))
        .expect("keyed host must finish");
    worker.join().unwrap();
    result
}
