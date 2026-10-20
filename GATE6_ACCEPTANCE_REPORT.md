# Harness v1.0.0 — Gate 6 Acceptance Report (Multi-Surface Runtime, Post-Gemini Review Sealed)

## 1. Executive Summary
- **Milestone**: Gate 6 — Multi-Surface Runtime (`v1.0.0` Production Release)
- **Workspace Version**: `1.0.0` (`/usr/local/bin/harness --version` -> `harness 1.0.0`)
- **Event Schema Version**: `1.0.0` (`EVENT_SCHEMA_VERSION = "1.0.0"`)
- **Workspace Test Suite**: **152 passed, 0 failed, 0 warnings** across 17 test binaries.

## 2. Architecture & Delivered Components

### 2.1 Unified Engine Runtime & JSON-RPC 2.0 Dispatcher (`crates/harness-core/src/surface.rs`)
- Decoupled the core execution kernel (`AgentRunner` + `SessionJournal` + `SnapshotStore` + `ToolRegistry` + `PolicyEngine` + `SubtreeContextTracker` + `MutationLedger` + `CompletionGate`) from terminal I/O via `RpcDispatcher` (`HarnessRuntimeServer`).
- Implemented complete JSON-RPC 2.0 request/response/notification framing (`JsonRpcRequest`, `JsonRpcResponse`, `JsonRpcError`, `JsonRpcNotification`) with a 4 MiB pre-allocation per-frame cap (`MAX_RPC_FRAME_BYTES`) and HTTP request-line rejection (`GET`/`POST`/`PUT`/`DELETE`/`OPTIONS`/`HEAD`/`CONNECT`/`Origin:`/`Host:`) to prevent memory-exhaustion DoS and browser cross-origin socket abuse.
- Supported RPC methods across `harness acp` and `harness serve`:
  - `initialize` / `acp.initialize`
  - `session.new` / `session.start` / `session.prompt` / `acp.prompt` (with real-time `harness/event` notification streaming and session-store/workspace-root validation on `resume`)
  - `approval.pending` & `approval.respond` (`approve_once` | `approve_session` | `deny`)
  - `session.cancel` & `session.status` & `session.fork`
  - `revision.list`, `revision.undo`, & `revision.restore`
  - `evidence.recall` (lock-free read-only Gate 2 raw evidence inspection by `source_entry_id`, `call_id`, or `"latest"`)
  - `skills.list` & `skills.activate` (Gate 3 skill discovery and activation)
  - `shutdown`

### 2.2 Cross-Surface Approval Pause/Resume & Fail-Closed Disconnect (`RpcApprovalBroker`)
- Implemented `RpcApprovalBroker` (`ChannelApprovalHandler`) implementing `ApprovalHandler` with `tokio::task::block_in_place` support and deduplicated `ApprovalRequested` / `ApprovalResolved` telemetry emission.
- Under `PolicyMode::Ask`, mutating/shell/sensitive-path tools and `role="worker"` `spawn_subagent` calls emit an `approval/requested` JSON-RPC notification, expose pending requests via `approval.pending`, and resume immediately when `approval.respond` arrives.
- Enforces fail-closed semantics on authenticated client EOF/disconnect (`broker.close()` + `rpc_session_cancel()`), explicit cancellation (`session.cancel`), or approval timeout (`deny_timeout`).

### 2.3 Four Runtime Surfaces (`crates/harness-cli/src/main.rs`)
1. **Headless CLI** (`harness [run] --workspace <DIR> --task <TASK> [--output-format text|jsonl]`), with `TtyApprovalUi::drain_events` recording `ApprovalRequested` / `ApprovalResolved`.
2. **Interactive REPL / TUI** (`harness repl` / `harness tui` / `--interactive`): stateless `InteractiveRepl` consumer backed 100% by `RpcDispatcher` (zero private session state per Blueprint Gate 6 Invariant #3), supporting optional initial `--prompt` turn execution plus `/help`, `/status`, `/revisions`, `/diff`, `/undo`, `/restore`, `/skills`, `/fork`, `/inspect`, `/clear`, `/quit` with terminal control-character/Bidi sanitization.
3. **ACP Stdio Server** (`harness acp` / `--acp`): newline-delimited JSON-RPC 2.0 server over `stdin`/`stdout` with `stdout` reserved exclusively for protocol frames.
4. **Local SDK / Daemon Server** (`harness serve [--listen 127.0.0.1:<PORT> | unix:///path/to/sock]`): serves JSON-RPC 2.0 over loopback/token-authenticated TCP or `0600`-permissioned Unix domain sockets (`tokio::net::UnixListener`), authenticating clients before subscribing them to live events or opening the approval broker, and cancelling in-flight runs on authenticated client disconnect.

## 3. Post-Gemini Review Findings Resolved
1. **[P0 Fixed] Authenticate before subscribing or opening `RpcApprovalBroker` (`surface.rs`)**: Unauthenticated TCP connections on a `--auth-token` daemon receive zero `harness/event` or `approval/requested` notifications and cannot close `approval_broker` or cancel an authenticated session on disconnect.
2. **[P1 Fixed] Cancel in-flight runs and close pending approvals on authenticated stream exit (`surface.rs`)**: EOF, oversized frames, or HTTP/CSRF aborts on an authenticated stream immediately close `approval_broker` and invoke `rpc_session_cancel()` before awaiting spawned prompt tasks.
3. **[P1 Fixed] Non-blocking `op_lock` & `block_in_place` approval waiting (`surface.rs`)**: `evidence.recall` is lock-free, mutating session/revision RPCs fail fast with a busy error if called while a prompt is in flight instead of deadlocking `serve_stream`, and `RpcApprovalBroker::request_approval` uses `tokio::task::block_in_place` on multi-threaded runtimes.
4. **[P1 Fixed] Unix Domain Socket transport (`0600` mode) (`surface.rs`, `main.rs`)**: `HarnessDaemon::bind` and `validate_daemon_listen_endpoint` support `unix:///path/to/socket` and `.sock` paths with `0600` permissions and automatic socket cleanup on shutdown.
5. **[P1 Fixed] Causal ordering of `ApprovalRequested` / `ApprovalResolved` (`agent_loop.rs`, `runtime.rs`, `approval_ui.rs`)**: `execute_spawn_subagent_inner` drains approval events immediately after `prompt_approval` (strictly before `SubagentSpawned`), awaits `prompt_task` on cancellation, and `TtyApprovalUi` implements `drain_events()`.
6. **[P1/P2 Fixed] CLI surface flag validation & initial REPL prompt (`main.rs`)**: Conflicting surface selectors and `--prompt` with `acp`/`serve` are rejected with exit code 2, while `harness repl --prompt <TASK>` executes the initial prompt as Turn 1 before entering the interactive loop.

## 4. Verification & Test Evidence (`crates/harness-core/tests/gate6_multi_surface_runtime.rs`)
- `test_acp_stdio_initialize_prompt_event_stream_and_evidence_recall`: passed.
- `test_rpc_approval_pause_and_resume_approve_and_deny_and_disconnect_fail_closed`: passed.
- `test_serve_tcp_daemon_loopback_security_and_cancel_and_fork`: passed.
- `test_interactive_repl_slash_commands_and_multiturn_session`: passed.
- `test_cross_surface_parity_headless_vs_rpc_produce_identical_wal_and_revisions`: passed.
- `test_unauthenticated_probe_isolation_and_disconnect_cancellation_and_protocol_guards`: passed.
- `test_unix_domain_socket_daemon_0600_permissions_and_worker_subagent_rpc_approval`: passed.
