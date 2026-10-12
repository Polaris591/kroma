pub mod agent_loop;
pub mod budget;
pub mod context_projector;
pub mod completion_gate;
pub mod env;
pub mod events;
pub mod loop_guard;
pub mod messages;
pub mod prompt;
pub mod provider;
pub mod revision;
pub mod sensitive_paths;
pub mod session;
pub mod tool;
pub mod tools;
pub mod types;
pub mod workspace;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
