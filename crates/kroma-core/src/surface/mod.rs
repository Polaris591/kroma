//! Gate 6 — Multi-Surface Runtime (`v1.0.0`).
//!
//! Decouples the core execution engine (`AgentRunner` + `SessionJournal` + `SnapshotStore` +
//! `ToolRegistry` + `PolicyEngine`) from terminal I/O so all four runtime surfaces share the exact
//! same state machine, policy boundaries, workspace revision history, and diagnostic event stream:
//!
//! 1. **Headless CLI** (`kroma run` / `kroma --task ...` with optional `--output-format jsonl`).
//! 2. **Interactive REPL / TUI** (`kroma repl` / `kroma tui` / `--interactive`), implemented as a
//!    stateless consumer of [`RpcDispatcher`] with zero private session state.
//! 3. **ACP stdio server** (`kroma acp` / `--acp`), speaking newline-delimited JSON-RPC 2.0 on
//!    `stdin`/`stdout` with `stdout` reserved exclusively for protocol frames.
//! 4. **Local SDK / Daemon server** (`kroma serve --listen 127.0.0.1:<PORT>`), serving JSON-RPC 2.0
//!    over loopback TCP (or explicit `--auth-token` authenticated TCP) with bounded frame reading,
//!    HTTP/browser request rejection, and connection concurrency limits.


//!
//! Physical module split: one file per responsibility (no behaviour change).

mod approval;
mod daemon;
mod dispatcher;
mod repl;
mod rpc_types;
mod runtime;
mod terminal;

pub use approval::*;
pub use daemon::*;
pub use dispatcher::*;
pub use repl::*;
pub use rpc_types::*;
pub use runtime::*;
pub use terminal::*;

pub(crate) use crate::agent_loop::{build_runtime_with_registry, AgentRunConfig, AgentRunner};
pub(crate) use crate::attribution::{MutationLedger, MutationRecord};
pub(crate) use crate::budget::BudgetConfig;
pub(crate) use crate::completion_gate::WorkspaceCompletionGate;
pub(crate) use crate::context_projector::{source_entry_id, CompactionReason, ContextProjector, EvidenceLink};
pub(crate) use crate::env::{AllowAllGate, ExecutionEnv, LocalHostEnv, SharedCompletionGate};
pub(crate) use crate::events::{DiagnosticEvent, EventLogger, EventPayload};
pub(crate) use crate::extensions::{
    discover_skills, execute_activate_skill, ExtensionId, ScopedExtension, SkillEntry,
    SubtreeContextTracker,
};
pub(crate) use crate::messages::{ToolCall, TranscriptItem};
pub(crate) use crate::types::ToolCallId;
pub(crate) use crate::prompt::{sha256_hex64, PromptBuilder};
pub(crate) use crate::provider::ModelProvider;
pub(crate) use crate::revision::{MergeRecoveryOutcome, RestoreResult, RevisionId, SnapshotStore, WorkspaceRevision};
pub(crate) use crate::session::{RevisionRecord, SessionJournal};
pub(crate) use crate::theme::{StatusDot, Theme, ThemeRenderer};
pub(crate) use crate::tool::policy::{
    ApprovalDecision, ApprovalHandler, ApprovalRequest, PolicyCheckResult, PolicyEngine, PolicyMode,
};
pub(crate) use crate::tool::registry::ToolRegistry;
pub(crate) use crate::tools::recall_evidence::execute_recall_evidence;
pub(crate) use crate::types::{EffectClass, RunId, RunStatus};
pub(crate) use crate::workspace::WorkspaceCap;
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use serde_json::{json, Value};
pub(crate) use std::collections::{BTreeMap, BTreeSet};
pub(crate) use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::sync::atomic::{AtomicBool, Ordering};
pub(crate) use std::sync::{mpsc as std_mpsc, Arc, Mutex};
pub(crate) use std::time::{Duration, Instant};
pub(crate) use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
pub(crate) use tokio::net::{TcpListener, TcpStream};
pub(crate) use tokio::sync::{mpsc, watch, Mutex as AsyncMutex, Semaphore};

/// JSON-RPC 2.0 wire version string.
pub const JSONRPC_VERSION: &str = "2.0";
/// Agent Client Protocol (ACP) adapter version string.
pub const ACP_PROTOCOL_VERSION: &str = "1.0";
/// Hard cap on a single incoming JSON-RPC line (4 MiB) to prevent memory-exhaustion DoS.
pub const MAX_RPC_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// Maximum simultaneous client connections accepted by `kroma serve`.
pub const MAX_DAEMON_CONNECTIONS: usize = 16;
/// Default timeout for a pending operator approval before failing closed (`ApprovalDecision::Deny`).
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

pub const JSONRPC_PARSE_ERROR: i64 = -32700;
pub const JSONRPC_INVALID_REQUEST: i64 = -32600;
pub const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
pub const JSONRPC_INVALID_PARAMS: i64 = -32602;
pub const JSONRPC_INTERNAL_ERROR: i64 = -32603;
pub const JSONRPC_UNAUTHORIZED: i64 = -32001;
pub const JSONRPC_RESTORE_CONFLICT: i64 = -32003;
pub const JSONRPC_RECOVERY_REQUIRED: i64 = -32004;
