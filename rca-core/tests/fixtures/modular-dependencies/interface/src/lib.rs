//! Defines a client interface without production providers.

use rca_core::DependencyKey;
use std::sync::Arc;

/// Owns callable client behaviour without choosing its provider.
#[derive(Clone)]
pub struct Client(Arc<dyn Fn() -> &'static str + Send + Sync>);

impl Client {
    /// Creates an explicitly supplied client.
    #[must_use]
    pub fn new(call: impl Fn() -> &'static str + Send + Sync + 'static) -> Self {
        Self(Arc::new(call))
    }

    /// Invokes the selected provider.
    ///
    /// # Panics
    /// Propagates any panic from the supplied callback.
    #[must_use]
    pub fn call(&self) -> &'static str {
        (self.0)()
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").finish_non_exhaustive()
    }
}

/// Declares identity and value type only, without production defaults.
#[derive(Debug)]
pub struct ClientKey;

impl DependencyKey for ClientKey {
    type Value = Client;
}
