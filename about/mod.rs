//! Crate metadata and “About” information.
//!
//! This module is included from the main crate via `#[path = "../../about/mod.rs"]` so it can live
//! at the workspace root (useful for packaging, docs generation, or sharing with other crates).
//!
//! Keep this module lightweight: it is intended to be safe to include in documentation builds.

#![doc = include_str!("Getting Started.md")]

pub mod changelog;
