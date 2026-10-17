pub mod agent_loop;
pub mod ahe;
pub mod async_tasks;
pub mod attribution;
pub mod bench;
pub mod budget;
pub mod channel_governor;
pub mod completion_gate;
pub mod config_file;
pub mod context_economy;
pub mod context_projector;
pub mod env;
pub mod events;
pub mod experience;
pub mod experience_runtime;
pub mod extensions;
pub mod loop_guard;
pub mod mcp;
pub mod messages;
pub mod process_group;
pub mod process_launch;
pub mod process_supervisor;
pub mod prompt;
pub mod provenance;
pub mod provider;
pub mod repo_graph;
pub mod revision;
pub mod sandbox;
pub mod secure_fs;
pub mod self_repair_guard;
pub mod sensitive_paths;
pub mod session;
pub mod session_ownership;
pub mod subagents;
pub mod surface;
pub mod task_host;
pub mod theme;
pub mod tool;
pub mod tools;
pub mod types;
pub mod usage_cost;
pub mod workspace;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
