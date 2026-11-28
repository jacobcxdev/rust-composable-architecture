//! Gives the fixture a distinct crate identity without copying the real core.

/// Captures values using this fixture's trait, not the facade's trait.
pub trait Snapshot {
    /// Owned form returned by the capture.
    type Value;

    /// Captures the value.
    fn snapshot(&self) -> Self::Value;
}

impl Snapshot for u8 {
    type Value = [u8; 1];

    fn snapshot(&self) -> Self::Value {
        [*self]
    }
}
