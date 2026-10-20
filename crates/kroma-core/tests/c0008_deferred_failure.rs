//! C00-08 spec 3: a failed deferred discovery still leaves the base tools, and says what failed.
//!
//! Under on-demand schema mode only the core tools are pinned in the request; everything else needs
//! `discover_tools`. The failure this pins is the one that matters when discovery goes wrong: the
//! model must not be left with nothing to work with, and the failure must be named rather than
//! silently returning an empty result.

use std::sync::Arc;

use kroma_core::agent_loop::CORE_TOOL_NAMES;
use kroma_core::budget::BudgetConfig;
use kroma_core::extensions::execute_discover_tools;
use kroma_core::tool::definition::builtin_tool_registrations;
use kroma_core::tool::policy::PolicyMode;
use kroma_core::tool::policy::{NonInteractiveApprovalHandler, PolicyEngine};
use kroma_core::tool::registry::ToolRegistry;
use kroma_core::tool::runtime::ToolRuntime;

fn runtime(policy: PolicyMode) -> ToolRuntime {
    let registry = ToolRegistry::default();
    let state = std::env::temp_dir().join("kroma-c0008-state");
    ToolRuntime::new(
        registry,
        PolicyEngine::new(policy, Arc::new(NonInteractiveApprovalHandler)),
        Arc::new(
            kroma_core::env::LocalHostEnv::new(std::env::temp_dir(), state).expect("host env"),
        ),
        BudgetConfig::default(),
    )
}

/// C00-08-A1: a query that matches nothing answers "nothing matched" instead of returning silence.
#[test]
fn a_discovery_that_matches_nothing_says_so() {
    let catalog = builtin_tool_registrations();
    let output = execute_discover_tools(&catalog, r#"{"query":"definitely-not-a-tool-name"}"#);
    let text = output.content.clone();
    assert!(
        !text.trim().is_empty(),
        "an empty result would leave the model guessing"
    );
    assert!(
        text.to_ascii_lowercase().contains("no tool")
            || text.to_ascii_lowercase().contains("no match")
            || text.to_ascii_lowercase().contains("nothing"),
        "the diagnostic must say nothing matched, got: {text}"
    );
}

/// C00-08-A2: a failed activation leaves every core tool dispatchable and names the failure.
#[test]
fn a_failed_activation_keeps_the_base_tools() {
    let runtime = runtime(PolicyMode::FullAutonomous);
    let results = runtime.activate_tools(&["no_such_tool".to_string()]);
    assert_eq!(results.len(), 1);
    let failure = format!("{}", results[0].1.as_ref().expect_err("a refusal"));
    assert!(
        failure.contains("no_such_tool"),
        "the refusal must name the tool it refused: {failure}"
    );

    // The pinned base set is real, not a list of wishes: every name in it is a registered tool, so a
    // failed discovery cannot leave the model with nothing it is allowed to call.
    let catalog = builtin_tool_registrations();
    for name in CORE_TOOL_NAMES {
        assert!(
            catalog.iter().any(|tool| tool.definition.name == *name),
            "core tool {name} must exist for the base set to survive a failed discovery"
        );
    }
}

/// C00-08-A3: a failure does not roll back an activation that already succeeded.
#[test]
fn a_failure_does_not_undo_an_earlier_activation() {
    let runtime = runtime(PolicyMode::FullAutonomous);
    let ok = runtime.activate_tools(&["code_graph".to_string()]);
    assert!(ok[0].1.is_ok(), "code_graph activates: {:?}", ok[0].1);
    let before = runtime.active_tool_names();

    let failed = runtime.activate_tools(&["no_such_tool".to_string()]);
    assert!(failed[0].1.is_err());
    assert_eq!(
        runtime.active_tool_names(),
        before,
        "a failed activation must not disturb the active set"
    );
}
