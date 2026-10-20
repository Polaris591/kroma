//! C00-08: exposure and authorization are two different sets, and they are wired into the real
//! dispatch path.
//!
//! The defect this pins: `RequestSnapshot` existed and was tested in isolation while nothing in
//! production ever built one, so a dynamic refresh (`tools/list`, an extension mount) could change
//! the tool list under a request that was already running, and "the model was shown this" and "the
//! model may call this" were answered by the same lookup.
//!
//! Every test here goes through `ToolRuntime::begin_request` and `ToolRuntime::execute_call`: the
//! boundary the agent loop creates per request and the path a call actually takes.

use kroma_core::activation::ActivationRegistry;
use kroma_core::budget::BudgetConfig;
use kroma_core::env::LocalHostEnv;
use kroma_core::extensions::{
    CustomToolContext, ExtensionManifest, ExtensionTrustLevel, ScopedExtension,
    ScopedToolDefinition, EXTENSION_API_VERSION,
};
use kroma_core::messages::{ToolCall, ToolResultMessage, ToolResultMeta};
use kroma_core::tool::definition::{ToolDefinition, ToolMetadata};
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine, PolicyMode};
use kroma_core::tool::registry::ToolRegistry;
use kroma_core::tool::runtime::ToolRuntime;
use kroma_core::tools::ToolExecutionOutput;
use kroma_core::types::{EffectClass, ToolCallId};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// Bound for every awaited step: a hang must fail the test, not stall the suite.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);

fn workspace(tag: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("kroma-c0008-{tag}-{}", uuid::Uuid::new_v4()));
    let ws = root.join("ws");
    let state = root.join("state");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    (ws, state)
}

fn runtime_for(
    ws: &Path,
    state: &Path,
    filter: Option<Vec<String>>,
    capabilities: Arc<ActivationRegistry>,
) -> ToolRuntime {
    ToolRuntime::new(
        ToolRegistry::new(filter),
        PolicyEngine::new(
            PolicyMode::FullAutonomous,
            Arc::new(NonInteractiveApprovalHandler),
        ),
        Arc::new(LocalHostEnv::new(ws, state).unwrap()),
        BudgetConfig::default(),
    )
    .with_capabilities(capabilities)
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        call_id: ToolCallId::new(format!("call_{name}")),
        name: name.to_string(),
        raw_arguments: args.to_string(),
    }
}

async fn dispatch(
    runtime: &ToolRuntime,
    name: &str,
    args: serde_json::Value,
) -> (ToolResultMessage, String) {
    let (_tx, rx) = watch::channel(false);
    let (message, decision, _hooks) =
        tokio::time::timeout(STEP_TIMEOUT, runtime.execute_call(&call(name, args), 0, rx))
            .await
            .expect("dispatch must not hang");
    (message, decision)
}

fn tool_def(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: format!("extension tool {}", name),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "additionalProperties": false
        }),
    }
}

/// An extension tool that echoes its `value` argument, so a successful dispatch is distinguishable
/// from a refusal.
fn echo_tool(name: &str, prefix: &'static str) -> ScopedToolDefinition {
    ScopedToolDefinition::new(
        tool_def(name),
        ToolMetadata {
            effect_class: EffectClass::ReadOnly,
            default_timeout_sec: 30,
            inline_output_cap_bytes: 64 * 1024,
        },
        Arc::new(move |raw: &str, _ctx: &CustomToolContext<'_>| {
            let parsed: serde_json::Value = serde_json::from_str(raw).unwrap_or_default();
            let value = parsed
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("<missing>");
            ToolExecutionOutput::success(format!("{}{}", prefix, value), ToolResultMeta::default())
        }),
    )
}

fn ext(name: &str, tools: Vec<ScopedToolDefinition>) -> ScopedExtension {
    let manifest = ExtensionManifest {
        name: name.to_string(),
        version: "1.0.0".to_string(),
        api_version: EXTENSION_API_VERSION.to_string(),
        capabilities: vec!["tools".to_string()],
        trust_level: ExtensionTrustLevel::OperatorTrusted,
        source: "test".to_string(),
        checksum: None,
    };
    tools
        .into_iter()
        .fold(ScopedExtension::new(manifest), |ext, tool| {
            ext.with_tool(tool)
        })
}

/// C00-08-①: discovery and exposure never stand in for authorization.
///
/// Both directions are checked against the wired path: a tool the request never declared is still
/// reachable because the gateway authorized it (declaration is not a grant), and a tool the registry
/// still holds stops being callable once the gateway withdraws it (the registry is not a grant
/// either).
#[tokio::test]
async fn discovery_and_exposure_never_stand_in_for_authorization() {
    let (ws, state) = workspace("authority");
    let capabilities = Arc::new(ActivationRegistry::default());
    capabilities.declare("write", &[], Arc::new(|| Ok(())));
    let lease = capabilities.activate("write").expect("write activates");
    let runtime = runtime_for(&ws, &state, None, Arc::clone(&capabilities));
    runtime.gate_tools(lease.claim(), &["write"]);

    // The request declares `read` only. `write` is deferred-but-authorized, so it is reachable
    // without ever being declared: declaration is not what grants the call.
    let first = runtime.begin_request(&["read".to_string()]);
    assert!(first.deferred_fallback().is_none());
    assert_eq!(first.snapshot().declared(), ["read".to_string()]);
    assert!(
        first.snapshot().may_call("write"),
        "a deferred but authorized tool is reachable without being declared"
    );
    let (written, decision) = dispatch(
        &runtime,
        "write",
        serde_json::json!({ "path": "note.txt", "content": "x\n" }),
    )
    .await;
    assert!(
        written.outcome.is_success(),
        "the wired path must allow a deferred-but-authorized call: {}",
        written.content
    );
    assert_ne!(decision, "deny_request_boundary");

    // The gateway withdraws the capability. The registry still holds the tool — discovery can still
    // find it — but it is no longer authorized, so the next boundary does not reach it.
    capabilities.revoke("write");
    assert!(
        runtime
            .registry()
            .all_registered()
            .iter()
            .any(|t| t.definition.name == "write"),
        "the tool is still registered: registration and discovery are not the authority"
    );
    let second = runtime.begin_request(&["read".to_string()]);
    assert!(
        !second.snapshot().may_call("write"),
        "a revoked capability is not reachable even though the registry still lists the tool"
    );
    assert!(second.snapshot().request_id() > first.snapshot().request_id());
    let (refused, decision) = dispatch(
        &runtime,
        "write",
        serde_json::json!({ "path": "note.txt", "content": "y\n" }),
    )
    .await;
    assert!(!refused.outcome.is_success());
    assert_eq!(decision, "deny_capability_revoked", "{}", refused.content);

    let _ = std::fs::remove_dir_all(ws.parent().unwrap());
}

/// C00-08-②: a refresh that lands while a request is in flight changes nothing for that request.
///
/// The refresh here is the real one: an extension mount on the shared registry, performed
/// concurrently with a call that belongs to the already-frozen request.
#[tokio::test]
async fn a_refresh_during_an_in_flight_request_cannot_change_it() {
    let (ws, state) = workspace("refresh");
    let runtime = runtime_for(&ws, &state, None, Arc::new(ActivationRegistry::default()));
    // The loop's tool list for this request: `read` only.
    let boundary = runtime.begin_request(&["read".to_string()]);
    let frozen = runtime.in_flight_snapshot().expect("a boundary freezes");

    // The refresh happens while the request is in flight: a new extension contributes a tool.
    let refreshing = {
        let registry = runtime.registry().clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            registry
                .register_extension(ext("late", vec![echo_tool("echo_note", "echo:")]))
                .expect("the mount succeeds")
        })
    };
    // A call from the in-flight request runs across the refresh.
    let (read_back, read_decision) =
        dispatch(&runtime, "read", serde_json::json!({"path": "missing.txt"})).await;
    let mount = tokio::time::timeout(STEP_TIMEOUT, refreshing)
        .await
        .expect("the refresh must not hang")
        .expect("the refresh task must not panic");
    assert_eq!(mount.as_str(), "late");
    assert_ne!(
        read_decision, "deny_request_boundary",
        "{}",
        read_back.content
    );

    // The refresh really landed...
    assert!(
        runtime
            .registry()
            .all_registered()
            .iter()
            .any(|t| t.definition.name == "echo_note"),
        "the registry refresh is visible to the registry"
    );
    // ...but the request in flight still sees the schema it was given.
    let in_flight = runtime.in_flight_snapshot().expect("still in flight");
    assert_eq!(
        in_flight, frozen,
        "the in-flight snapshot must not be mutated"
    );
    assert_eq!(in_flight.declared(), ["read".to_string()]);
    assert!(
        !in_flight.may_call("echo_note"),
        "a tool added mid-flight must not be reachable from this request"
    );

    let (denied, decision) =
        dispatch(&runtime, "echo_note", serde_json::json!({"value": "hi"})).await;
    assert_eq!(decision, "deny_request_boundary", "{}", denied.content);
    assert!(!denied.outcome.is_success());
    assert!(
        denied.content.contains("NOT_DISPATCHED_REQUEST_BOUNDARY")
            && denied.content.contains("next request boundary"),
        "the refusal must name itself and the boundary: {}",
        denied.content
    );

    // At the next boundary the refresh applies, and the tool is callable.
    let next = runtime.begin_request(&["read".to_string(), "echo_note".to_string()]);
    assert_ne!(
        next.snapshot().request_id(),
        boundary.snapshot().request_id()
    );
    assert!(next.snapshot().may_call("echo_note"));
    let (echoed, decision) =
        dispatch(&runtime, "echo_note", serde_json::json!({"value": "hi"})).await;
    assert_ne!(decision, "deny_request_boundary", "{}", echoed.content);
    assert!(echoed.outcome.is_success(), "{}", echoed.content);
    assert_eq!(echoed.content.as_ref(), "echo:hi");

    let _ = std::fs::remove_dir_all(ws.parent().unwrap());
}

/// C00-08-③: a deferred boundary that produced nothing still leaves the session with tools, and says
/// why instead of sending an empty declaration silently.
#[tokio::test]
async fn an_empty_deferred_boundary_falls_back_to_the_base_tools() {
    let (ws, state) = workspace("deferred");
    let runtime = runtime_for(&ws, &state, None, Arc::new(ActivationRegistry::default()));

    let boundary = runtime.begin_request(&[]);
    let fallback = boundary
        .deferred_fallback()
        .expect("an empty declaration is never silent");
    assert!(
        fallback.contains("authorized base tool(s)"),
        "the diagnostic must say what happened: {fallback}"
    );
    assert!(
        !boundary.snapshot().declared().is_empty(),
        "the session must still have tools: {:?}",
        boundary.snapshot().declared()
    );
    assert!(boundary.snapshot().may_call("read"));
    let (read_back, decision) =
        dispatch(&runtime, "read", serde_json::json!({"path": "x.txt"})).await;
    assert_ne!(decision, "deny_request_boundary", "{}", read_back.content);

    // Nothing authorized at all: the boundary still reports it rather than declaring nothing quietly.
    let empty = runtime_for(
        &ws,
        &state,
        Some(vec!["no_such_tool".to_string()]),
        Arc::new(ActivationRegistry::default()),
    );
    let boundary = empty.begin_request(&[]);
    let fallback = boundary.deferred_fallback().expect("still not silent");
    assert!(
        fallback.contains("no authorized tools"),
        "the diagnostic must explain the empty set: {fallback}"
    );
    assert!(boundary.snapshot().declared().is_empty());

    let _ = std::fs::remove_dir_all(ws.parent().unwrap());
}
