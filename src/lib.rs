//! Kroma runtime library. The CLI and TUI are hosts of the same agent.
//! Keep modules in one Cargo package; no plugin runtime is required.
pub mod agent;
pub mod app;
pub(crate) mod attachments;
pub mod cli_events;
pub mod commands;
pub mod config;
pub mod context;
pub mod context_state;
pub mod convenience;
pub mod diagnostics;
pub mod events;
pub mod mcp;
pub mod model;
pub mod ports;
pub mod profiles;
pub mod prompt;
pub mod protocol;
pub mod provider;
pub mod session;
pub mod sessions;
pub mod shell;
pub mod skills;
pub mod steering;
pub mod tools;
pub mod tui;
pub mod usage;

mod atomic_file;
mod catalog_file;
mod private_file;
#[cfg(windows)]
mod winsys;
