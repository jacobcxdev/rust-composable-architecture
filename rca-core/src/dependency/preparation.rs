//! Defines rejection of preparation after an owned environment is used.

use std::backtrace::Backtrace;
use std::fmt;

/// Reports preparation attempted after an environment's first lookup.
#[derive(Debug)]
pub struct DependencyPreparationError {
    /// Captures the rejected preparation call, subject to backtrace configuration.
    backtrace: Backtrace,
}

impl DependencyPreparationError {
    /// Captures evidence only after a lookup has closed preparation eligibility.
    pub(super) fn new() -> Self {
        Self {
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns the backtrace captured when preparation was rejected.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Display for DependencyPreparationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "dependency environment already used; preparation rejected\n{}",
            self.backtrace
        )
    }
}

impl std::error::Error for DependencyPreparationError {}
