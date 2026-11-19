//! Literal workload and lifecycle boundaries shared by all publication adapters.

use std::time::Duration;

/// Bounds missing publications, not acceptable runtime performance.
pub const WAIT: Duration = Duration::from_secs(2);
/// Includes a repeated value so deduplication cannot silently change the workload.
pub const UPDATES: [u8; 4] = [3, 1, 1, 4];
/// Checks initial registration separately from the four committed updates.
pub const EXPECTED: [u8; 5] = [0, 3, 1, 1, 4];

/// Marks completed adapter stages inside the allocation capture window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point {
    Construction,
    Registration,
    Initial,
    Checked,
    Unregistration,
}

/// Selects correct behaviour or one deliberate adapter error.
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Native,
    OmitRepeated,
    PreviousValue,
    KeepRegistered,
}

/// Keeps current value, previous value and domain updates independently observable.
#[derive(Default, Debug, PartialEq)]
pub struct State {
    pub value: u8,
    pub previous: u8,
    pub updates: usize,
}

impl State {
    pub fn update(&mut self, value: u8) {
        self.previous = self.value;
        self.value = value;
        self.updates += 1;
    }
}

/// Rejects stale or otherwise incorrect publications at the consuming boundary.
pub fn check_value(value: u8, expected: u8) -> Result<(), &'static str> {
    if value == expected {
        Ok(())
    } else {
        Err("wrong publication")
    }
}

#[cfg(test)]
/// Detects a stuck adapter; an outer process watchdog still bounds native joins.
pub fn bounded(
    exercise: fn(Mode) -> Result<(), &'static str>,
    mode: Mode,
) -> Result<(), &'static str> {
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = exercise(mode);
        send.send(result).unwrap();
    });
    let result = receive
        .recv_timeout(Duration::from_secs(10))
        .expect("publication host must finish");
    worker.join().unwrap();
    result
}
