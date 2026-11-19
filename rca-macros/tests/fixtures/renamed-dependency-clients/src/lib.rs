//! Exercises dependency derives through a renamed facade dependency.

use r#gen::{DependencyClient, DependencyKey};
use std::sync::Arc;

#[derive(DependencyClient)]
/// Preserves member values, explicit callback types and raw field names.
pub struct Client {
    /// Distinguishes this facade consumer from the direct-core consumer.
    #[dependency(member = String::from("renamed"))]
    pub label: String,
    /// Returns its input as an asynchronous error.
    #[dependency(test = Arc::new(|value| Box::pin(async move { Err(value) })))]
    #[expect(
        clippy::type_complexity,
        reason = "the fixture verifies parsing this explicit async callback type"
    )]
    pub async_error: Arc<
        dyn Fn(String) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send>>
            + Send
            + Sync,
    >,
    /// Requires an explicit override before use.
    pub r#type: Arc<dyn Fn(String) -> String + Send + Sync>,
    /// Returns the explicit test value instead of an unimplemented callback.
    #[dependency(test = Arc::new(|| 29))]
    pub count: Arc<dyn Fn() -> usize + Send + Sync>,
}

#[derive(DependencyKey)]
#[dependency(value = Client, live = Client::test_value(), test = Client::test_value())]
/// Uses generated client defaults for both dependency contexts.
pub struct Key;

/// Replaces the unimplemented raw-name callback with an identity callback.
pub fn configured() -> Client {
    Client {
        r#type: Arc::new(|value| value),
        ..Key::test_value()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_client_preserves_async_failure_and_overrides() {
        use std::task::{Context, Poll, Waker};

        let client = configured();
        assert_eq!(client.label, "renamed");
        assert_eq!((client.count)(), 29);
        assert_eq!((client.r#type)(String::from("sentinel")), "sentinel");
        let mut failure = (client.async_error)(String::from("failure"));
        let mut context = Context::from_waker(Waker::noop());
        assert_eq!(
            failure.as_mut().poll(&mut context),
            Poll::Ready(Err(String::from("failure")))
        );
        assert_eq!(Key::live_value().label, "renamed");
        assert_eq!(Key::test_value().label, "renamed");
    }
}
