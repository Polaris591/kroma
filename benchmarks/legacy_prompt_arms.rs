//! Archived 2026-11-10 prompt experiment arms from Kroma 0.4.0 (820b8bb).
//! Research-only: not linked into the production crate. Historical A/B
//! runners require a checkout/build of 820b8bb, whose env switches exist.

pub const BASELINE_EXECUTION_GUIDANCE: &str = "\nFor coding tasks, locate relevant symbols before reading large files and keep inspections focused on relevant files and ranges; prefer focused ranges and diagnostic excerpts over dumping entire files or logs unless full context is necessary; once the needed evidence is found, edit rather than re-inspecting unchanged files. When necessary inspections are independent, request them together as separate shell calls (or one concise command); keep dependent edits and checks in order. After implementing the requested changes, run the requested tests and relevant regression checks. If all requirements are met and those checks pass, finish with a concise result rather than running unrelated doctests, broad exploratory checks or repeated successful tests. If a check fails, investigate its actual cause and re-run after a meaningful fix. Do not omit required verification just to save calls.";

// Experimental legacy alternative to the first evidence-gathering sentence only.
// Its verification and completion rules match the historical baseline.
pub const COMPLETE_FUNCTION_GUIDANCE: &str = "\nFor coding tasks, locate relevant symbols before reading large files; once the relevant implementation is found, inspect its complete function and directly needed callers or types in as few useful requests as possible, rather than repeatedly reading tiny fragments. Do not dump unrelated large files or logs. Once the needed evidence is found, edit rather than re-inspecting unchanged files. When necessary inspections are independent, request them together as separate shell calls (or one concise command); keep dependent edits and checks in order. After implementing the requested changes, run the requested tests and relevant regression checks. If all requirements are met and those checks pass, finish with a concise result rather than running unrelated doctests, broad exploratory checks or repeated successful tests. If a check fails, investigate its actual cause and re-run after a meaningful fix. Do not omit required verification just to save calls.";

pub const FINISH_ON_EVIDENCE_GUIDANCE: &str = concat!(
    "\nFor coding tasks, locate relevant symbols before reading large files and keep inspections focused on relevant files and ranges; prefer focused ranges and diagnostic excerpts over dumping entire files or logs unless full context is necessary; once the needed evidence is found, edit rather than re-inspecting unchanged files. When necessary inspections are independent, request them together as separate shell calls (or one concise command); keep dependent edits and checks in order. ",
    "After repairing the failure, run the requested tests and relevant checks; if the requirements are met and those checks pass, finish concisely. Do not intentionally revert a passing fix just to recreate the old failure unless the user asks for a negative control or unresolved evidence makes it necessary. If a passing test leaves a concrete requirement unverified, check that requirement before finishing.",
    " If a check fails, investigate its actual cause and re-run after a meaningful fix. Do not omit required verification just to save calls."
);

// Evidence-based completion is the production default; baseline remains
// available for reproducible A/B experiments without extra prompt paragraphs.
pub const EXECUTION_GUIDANCE: &str = FINISH_ON_EVIDENCE_GUIDANCE;

// Multi-arm research only. The default uses EXECUTION_GUIDANCE.
// Never emit multiple competing execution paragraphs in the same request.
pub const LEAN_GUIDANCE: &str = "\nFor coding tasks, locate the responsible code, make a correct focused change, and run all requested checks plus relevant regression tests. Investigate concrete failures or unverified requirements. Once those checks pass and the requested behavior is covered, finish instead of exploring unrelated possibilities.";

pub const FOCUS_GUIDANCE: &str = concat!(
    "\nFor coding tasks, first locate the named failure and owning implementation. Inspect the complete relevant function and directly needed callers or types in one useful pass. If evidence supports a fix, edit; otherwise inspect only the missing evidence. ",
    "When necessary inspections are independent, request them together as separate shell calls (or one concise command); keep dependent edits and checks in order. After implementing the requested changes, run the requested tests and relevant regression checks. If all requirements are met and those checks pass, finish with a concise result rather than running unrelated doctests, broad exploratory checks or repeated successful tests. If a check fails, investigate its actual cause and re-run after a meaningful fix. Do not omit required verification just to save calls."
);

pub const FOCUS_FINISH_GUIDANCE: &str = concat!(
    "\nFor coding tasks, first locate the named failure and owning implementation. Inspect the complete relevant function and directly needed callers or types in one useful pass. If evidence supports a fix, edit; otherwise inspect only the missing evidence. ",
    "When necessary inspections are independent, request them together as separate shell calls (or one concise command); keep dependent edits and checks in order. ",
    "After repairing the failure, run the requested tests and relevant checks; if the requirements are met and those checks pass, finish concisely. Do not intentionally revert a passing fix just to recreate the old failure unless the user asks for a negative control or unresolved evidence makes it necessary. If a passing test leaves a concrete requirement unverified, check that requirement before finishing. ",
    "If a check fails, investigate its actual cause and re-run after a meaningful fix. Do not omit required verification just to save calls."
);

fn execution_guidance_for(mode: Option<&str>) -> &'static str {
    match mode {
        Some("baseline") => BASELINE_EXECUTION_GUIDANCE,
        Some("finish") => FINISH_ON_EVIDENCE_GUIDANCE,
        Some("complete") => COMPLETE_FUNCTION_GUIDANCE,
        Some("lean") => LEAN_GUIDANCE,
        Some("focus") => FOCUS_GUIDANCE,
        Some("focus_finish") => FOCUS_FINISH_GUIDANCE,
        _ => EXECUTION_GUIDANCE,
    }
}

fn selected_execution_guidance() -> &'static str {
    if let Ok(arm) = std::env::var("KROMA_EXPERIMENT_PROMPT_ARM") {
        return execution_guidance_for(Some(&arm));
    }
    if std::env::var("KROMA_EXPERIMENT_FINISH_ON_EVIDENCE").as_deref() == Ok("1") {
        return execution_guidance_for(Some("finish"));
    }
    if std::env::var("KROMA_EXPERIMENT_COMPLETE_FUNCTION_READ").as_deref() == Ok("1") {
        return execution_guidance_for(Some("complete"));
    }
    EXECUTION_GUIDANCE
}

// V7 (F+C+E) keeps its instruction order; the quality paragraph incorporates
// B (reuse-first) as the production default. C remains an experiment only. Do not repeat
// execution guidance here: the final response should be concise without a
// fixed sentence limit, so complex work can explain verification and risk.
pub const V7_GUIDANCE: &str = concat!(
    "\nGive a brief user-facing progress update when starting substantial work, making an important confirmed discovery or changing direction, and starting validation. Describe actions or verified results, not internal reasoning. Skip routine tool-by-tool narration and progress updates for simple questions; do not add model requests or split batchable operations just to report progress. When you finish, reply concisely and accurately about what changed and what was verified. Complex tasks may warrant a fuller explanation of the modifications, verification evidence, failure causes and remaining risks; report uncertainty rather than hiding it.",
    "\nPrefer the simplest correct implementation consistent with the existing codebase. Before adding code, prefer suitable project implementations, then standard-library or platform capabilities and existing dependencies, before writing new code. Avoid redundant comments, explanatory boilerplate, unnecessary helper layers, and unrelated refactoring. Preserve required APIs, edge cases, tests, and non-obvious invariants. Never sacrifice correctness just to reduce code length.",
    "\nInvestigate tooling internals only when the evidence points there."
);
