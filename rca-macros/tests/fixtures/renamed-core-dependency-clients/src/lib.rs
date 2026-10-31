//! Exercises dependency derives with separately renamed core and macro dependencies.

use renamed_core::DependencyKey as _;
use renamed_macros::{DependencyClient, DependencyKey};
use std::sync::Arc;

#[derive(DependencyClient)]
/// Combines an ordinary member with explicit and unimplemented callbacks.
pub struct Client {
    /// Distinguishes this direct-core consumer from the facade consumer.
    #[dependency(member = String::from("renamed core"))]
    pub label: String,
    /// Returns its input as a typed error.
    #[dependency(test = Arc::new(Err))]
    pub error: Arc<dyn Fn(String) -> Result<(), String> + Send + Sync>,
    /// Requires an explicit override before use.
    pub call: Arc<dyn Fn() + Send + Sync>,
}

#[derive(DependencyKey)]
#[dependency(value = Client, live = Client::test_value(), test = Client::test_value())]
/// Uses generated client defaults for both dependency contexts.
pub struct Key;

/// Replaces the unimplemented callback with an empty callback.
pub fn configured() -> Client {
    Client {
        call: Arc::new(|| ()),
        ..Key::test_value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_client_preserves_typed_failure_and_defaults() {
        let client = configured();
        assert_eq!(client.label, "renamed core");
        assert_eq!(
            (client.error)(String::from("sentinel")),
            Err(String::from("sentinel"))
        );
        (client.call)();
        assert_eq!(Key::live_value().label, "renamed core");
        assert_eq!(Key::test_value().label, "renamed core");
    }
}
