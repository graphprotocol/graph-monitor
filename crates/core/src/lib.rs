//! Core logic for the `graph-monitor` CLI: config, sources, reads, checks, and the engine
//! that runs them.
//!
//! This crate returns typed values and never formats output. Rendering lives in the binary,
//! so the console reporter, a metrics exporter, and a TUI are all consumers of the same
//! `CycleReport` rather than reimplementations of it.

pub mod activity;
pub mod config;
pub mod engine;
pub mod error;
pub mod read;
pub mod remediate;
pub mod rpc;
pub mod source;
pub mod subgraph;
pub mod units;
pub mod watch;
pub mod watches;

pub use error::{ConfigError, Error, Result, RpcError};
