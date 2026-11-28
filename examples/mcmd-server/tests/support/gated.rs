//! Controls when the proxy client finishes a start or stop request.

use async_channel::Sender;
use rca_example_mcmd_server::{
    managed_proxy::{Client, Error, Events, StartFuture, StopFuture},
    proxy::Config,
};

/// Records client entry and transfers the operation's completion gate.
pub(super) enum Call {
    /// Requests a start and waits for the event stream or its error.
    Start(Config, Sender<Result<Events, Error>>),
    /// Requests a stop and waits for its result.
    Stop(String, Sender<Result<(), Error>>),
}

/// Holds each client operation until its test supplies a result.
pub(super) struct Gated(pub(super) Sender<Call>);

impl Client for Gated {
    fn start(&self, config: Config) -> StartFuture {
        let calls = self.0.clone();
        Box::pin(async move {
            let (reply, receive) = async_channel::bounded(1);
            calls.send(Call::Start(config, reply)).await.unwrap();
            receive.recv().await.unwrap()
        })
    }

    fn stop(&self, server_id: String) -> StopFuture {
        let calls = self.0.clone();
        Box::pin(async move {
            let (reply, receive) = async_channel::bounded(1);
            calls.send(Call::Stop(server_id, reply)).await.unwrap();
            receive.recv().await.unwrap()
        })
    }
}

/// Sets the loopback address and both game ports for server `alpha`.
pub(super) fn config() -> Config {
    Config {
        server_id: "alpha".into(),
        host: "127.0.0.1".into(),
        java_ports: vec![25565],
        bedrock_ports: vec![19132],
    }
}
