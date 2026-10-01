//! Shared Ferry application code.
//!
//! Binary targets should stay thin and call into this library so the CLI and
//! the desktop app (the `gui` crate) share the same behavior.

pub mod client;
pub mod config;
pub mod core;
pub mod daemon;
pub mod plugins;
pub mod protocol;
pub mod rpc;
pub mod store;
pub mod transport;
#[cfg(feature = "gui")]
pub mod ui;
