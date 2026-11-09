//! Kroma runtime library. The CLI and TUI are hosts of the same agent.
//! Keep modules in one Cargo package; optional process plugins share this core.
pub mod agent;
pub mod app;
pub(crate) mod attachments;
pub mod checkpoints;
pub mod child_policy;
pub mod cli;
pub mod cli_events;
pub mod commands;
pub mod config;
pub mod context;
pub mod context_report;
pub mod context_state;
pub mod convenience;
pub mod desktop_host;
pub mod diagnostics;
pub mod diagnostics_bundle;
pub mod events;
pub mod external;
pub mod host_attachments;
pub mod mcp;
pub mod model;
pub mod model_catalog;
pub mod output;
pub mod plugins;
pub mod ports;
pub mod profiles;
pub mod prompt;
pub mod protocol;
pub mod provider;
pub mod session;
pub mod sessions;
pub mod setup;
pub mod shell;
pub mod skills;
pub mod steering;
pub mod task_control;
pub mod tools;
pub mod tui;
pub mod update;
pub mod usage;

mod atomic_file;
mod cache_trace;
mod catalog_file;
mod file_edit;
mod file_lock;
mod history_tools;
mod path_display;
mod private_file;
mod token_estimate;
#[cfg(windows)]
mod winsys;

#[cfg(test)]
mod performance;
