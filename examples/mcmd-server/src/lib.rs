//! Demonstrates selected `MCMaitreD` features with RCA's headless APIs.
//!
//! This library is not a runnable daemon. [`server`] composes backend, polling
//! and proxy state. [`servers`] gives each configured server a separate lifetime.
//! The client traits accept owned futures and do not select an async runtime.
//!
//! [`proxy`] requires resources that release when their owner is dropped.
//! [`managed_proxy`] instead retains an independent owner for asynchronous cleanup.
//! [`managed_servers`] composes those owners with a fixed server roster.
//! Test-only TCP adapters check selected socket-release paths. No production proxy
//! transport or complete daemon migration is implemented here.
//!
//! # Examples
//!
//! Construct matching child identities before building a Store:
//!
//! ```
//! use rca_example_mcmd_server::{proxy, server};
//!
//! let state = server::State::new(proxy::Config {
//!     server_id: "lobby".into(),
//!     host: "127.0.0.1".into(),
//!     java_ports: vec![25565],
//!     bedrock_ports: vec![],
//! });
//! let _reducer = server::reducer();
//! assert_eq!(state.backend.server_id, state.polling.server_id);
//! ```
//!
//! Client injection and a runtime are still required to run the feature.

pub mod backend;
pub mod config;
pub mod managed_proxy;
pub mod managed_servers;
pub mod polling;
pub mod proxy;
pub mod server;
pub mod servers;
