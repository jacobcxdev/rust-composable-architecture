//! Lets a test poll a proxy owner independently of the store.

use std::pin::Pin;

use rca_example_mcmd_server::managed_proxy::Recovery;

/// Holds an owner until its external work ends.
pub(super) type Host = Pin<Box<dyn Future<Output = Recovery>>>;

/// Polls the owner once and verifies that it still has work.
pub(super) async fn step(host: &mut Host) {
    assert!(futures_util::poll!(host.as_mut()).is_pending());
}
