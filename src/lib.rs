//! systemd-ops: a capability-scoped operations engine for systemd.
//!
//! Direct CLI and optional MCP frontend share this crate. Nothing is
//! granted by default. Writes exist only through plan/apply.

#![forbid(unsafe_code)]

pub mod config;
pub mod json;
pub mod mcp;
pub mod operations;
pub mod scope;
pub mod sha256;
pub mod systemd;
pub mod token;
pub mod tui;
pub mod varlink;
pub mod write;
