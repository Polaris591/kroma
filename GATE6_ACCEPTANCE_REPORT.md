# Harness v1.0.0 — Gate 6 Acceptance Report (Multi-Surface Runtime)

## 1. Executive Summary
- **Milestone**: Gate 6 — Multi-Surface Runtime (`v1.0.0`)
- **Workspace Version**: `1.0.0` (`/usr/local/bin/harness --version` -> `harness 1.0.0`)
- **Event Schema Version**: `1.0.0` (`EVENT_SCHEMA_VERSION = "1.0.0"`)
- **Workspace Test Suite**: **150 passed, 0 failed, 0 warnings** across 17 test binaries.

## 2. Architecture & Delivered Components

### 2.1 Unified Engine Runtime & JSON-RPC 2.0 Dispatcher (`crates/harness-core/src/surface.rs`)
- Decoupled the core execution kernel (`AgentRunner` + `SessionJournal` + `SnapshotStore` + `ToolRegistry` + `PolicyEngine` + `SubtreeContextTracker` + `MutationLedger` + `CompletionGate`) from terminal I/O via `RpcDispatcher` (`HarnessRuntimeServer`).
- Implemented complete JSON-RPC 2.0 request/response/notification framing (`JsonRpcRequest`, `JsonRpcResponse`, `JsonRpcError`, `JsonRpcNotification`) with a 4 MiB per-frame cap (`MAX_RPC_FRAME_BYTES`) and HTTP request-line rejection (`GET`/`POST`/`Origin:`/`Host:`) to prevent memory-exhaustion DoS and browser cross-origin socket abuse.
- Supported RPC methods across `harness acp` and `harness serve`:
  - `initialize` / `acp.initialize`
  - `session.new` / `session.start` / `session.prompt` / `acp.prompt` (with real-time `harness/event` notification streaming)
  - `approval.pending` & `approval.respond` (`approve_once` | `approve_session` | `deny`)
  - `session.cancel` & `session.status` & `session.fork`
  - `revision.list`, `revision.undo`, & `revision.restore`
  - `evidence.recall` (Gate 2 raw evidence inspection by `source_entry_id`, `call_id`, or `"latest"`)
  - `skills.list` & `skills.activate` (Gate 3 skill discovery and activation)
  - `shutdown`

### 2.2 Cross-Surface Approval Pause/Resume & Fail-Closed Disconnect (`RpcApprovalBroker`)
- Implemented `RpcApprovalBroker` (`ChannelApprovalHandler`) implementing `ApprovalHandler`.
- Under `PolicyMode::Ask`, mutating/shell/sensitive-path tools emit an `approval/requested` JSON-RPC notification (`{ request_id, tool_name, effect_class, is_sensitive_path, target_path, arguments_summary, risk_reason, surface }`), expose pending requests via `approval.pending`, and resume immediately when `approval.respond` arrives.
- Enforces fail-closed semantics on client EOF/disconnect (`broker.close()`), cancellation (`session.cancel`), or approval timeout: any pending approval immediately resolves to `ApprovalDecision::Deny` and logs `EventPayload::ApprovalResolved`.

### 2.3 Four Runtime Surfaces (`crates/harness-cli/src/main.rs`)
1. **Headless CLI** (`harness [run] --workspace <DIR> --task <TASK> [--output-format text|jsonl]`).
2. **Interactive REPL / TUI** (`harness repl` / `harness tui` / `--interactive`): stateless `InteractiveRepl` consumer backed 100% by `RpcDispatcher` (zero private session state per Blueprint Gate 6 Invariant #3), supporting `/help`, `/status`, `/revisions`, `/diff`, `/undo`, `/restore`, `/skills`, `/fork`, `/inspect`, `/clear`, `/quit` with terminal control-character/Bidi sanitization.
3. **ACP Stdio Server** (`harness acp` / `--acp`): newline-delimited JSON-RPC 2.0 server over `stdin`/`stdout` with `stdout` reserved exclusively for protocol frames.
4. **Local SDK / Daemon Server** (`harness serve [--listen 127.0.0.1:<PORT>] [--auth-token <TOKEN>]`): TCP JSON-RPC 2.0 daemon that strictly rejects non-loopback bind addresses without `--auth-token`, enforces constant-time token verification when configured, and bounds concurrent client connections (`MAX_DAEMON_CONNECTIONS = 16`).

## 3. Verification & Test Evidence (`crates/harness-core/tests/gate6_multi_surface_runtime.rs`)
- `test_acp_stdio_initialize_prompt_event_stream_and_evidence_recall`: passed.
- `test_rpc_approval_pause_and_resume_approve_and_deny_and_disconnect_fail_closed`: passed.
- `test_serve_tcp_daemon_loopback_security_and_cancel_and_fork`: passed.
- `test_interactive_repl_slash_commands_and_multiturn_session`: passed.
- `test_cross_surface_parity_headless_vs_rpc_produce_identical_wal_and_revisions`: passed.
