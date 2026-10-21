use crate::prompt::sha256_hex16;
use crate::types::ErrorCode;
use std::collections::{HashMap, VecDeque};

pub const LOOP_GUARD_WINDOW_SIZE: usize = 12;
pub const MAX_CONSECUTIVE_IDENTICAL_CALLS: usize = 3;
pub const MAX_PING_PONG_CYCLES: usize = 3;
pub const MAX_EDIT_FAILURE_STREAK: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopGuardVerdict {
    Allow {
        signature: String,
    },
    Block {
        code: ErrorCode,
        guard_kind: &'static str,
        signature: String,
        message: String,
    },
}

/// Loop and Ping-Pong Oscillation Guardrail (§8).
#[derive(Debug, Clone, Default)]
pub struct LoopGuard {
    recent_signatures: VecDeque<String>,
    per_file_edit_failures: HashMap<String, usize>,
    /// KR-N04: arm ablation. `false` disables stagnation detection entirely so a paired run can
    /// measure what the guard prevents (and what it costs in false stops).
    enabled: bool,
}

/// Normalize relative path arguments so `./src//a.rs` and `src/a.rs` map to the same canonical path.
pub fn clean_relative_path(raw: &str) -> String {
    let replaced = raw.trim().replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for seg in replaced.split('/') {
        match seg {
            "" | "." => continue,
            ".." => {
                if parts.last().is_some_and(|&p| p != "..") {
                    parts.pop();
                } else {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

impl LoopGuard {
    pub fn new() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// KR-N04: construct a guard that records nothing and never blocks.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// Compute canonical call signature: `SHA256(tool_name + "\0" + canonical_json(args))`.
    /// Note: `serde_json::Value` uses `BTreeMap` by default, guaranteeing sorted keys.
    /// Normalizes `"path"` string fields via `clean_relative_path` to prevent `./` bypass.
    pub fn compute_signature(tool_name: &str, raw_arguments: &str) -> String {
        let canonical_args = match serde_json::from_str::<serde_json::Value>(raw_arguments) {
            Ok(mut val) => {
                if let Some(obj) = val.as_object_mut() {
                    if let Some(serde_json::Value::String(ref mut p)) = obj.get_mut("path") {
                        *p = clean_relative_path(p);
                    }
                }
                serde_json::to_string(&val).unwrap_or_else(|_| raw_arguments.trim().to_string())
            }
            Err(_) => raw_arguments.trim().to_string(),
        };
        let mut input = String::with_capacity(tool_name.len() + 1 + canonical_args.len());
        input.push_str(tool_name);
        input.push('\0');
        input.push_str(&canonical_args);
        sha256_hex16(input.as_bytes())
    }

    /// Check before executing a tool call and record the call in the 12-item sliding window.
    pub fn check_and_record(
        &mut self,
        tool_name: &str,
        raw_arguments: &str,
        relative_path: Option<&str>,
    ) -> LoopGuardVerdict {
        if !self.enabled {
            return LoopGuardVerdict::Allow {
                signature: String::new(),
            };
        }
        let sig = Self::compute_signature(tool_name, raw_arguments);

        // 1. Check per-file edit failure circuit breaker if calling `edit` on a circuit-broken file
        if tool_name == "edit" {
            if let Some(raw_path) = relative_path {
                let norm_path = clean_relative_path(raw_path);
                if let Some(&streak) = self.per_file_edit_failures.get(&norm_path) {
                    if streak >= MAX_EDIT_FAILURE_STREAK {
                        return LoopGuardVerdict::Block {
                            code: ErrorCode::EditFailureStreakBlocked,
                            guard_kind: "per_file_edit_failure_streak",
                            signature: sig,
                            message: format!(
                                "EDIT_FAILURE_STREAK_BLOCKED: {} consecutive failed `edit` attempts on '{}'. \
                                You MUST call `read` on '{}' around the target lines to inspect current file content before attempting another `edit`.",
                                streak, norm_path, norm_path
                            ),
                        };
                    }
                }
            }
        }

        // 2. Check consecutive identical calls: if last 2 calls have the exact same signature, this is the 3rd.
        let len = self.recent_signatures.len();
        if len >= MAX_CONSECUTIVE_IDENTICAL_CALLS - 1 {
            let all_same = self
                .recent_signatures
                .iter()
                .rev()
                .take(MAX_CONSECUTIVE_IDENTICAL_CALLS - 1)
                .all(|prev| prev == &sig);
            if all_same {
                self.push_signature(sig.clone());
                return LoopGuardVerdict::Block {
                    code: ErrorCode::RepeatedCallBlocked,
                    guard_kind: "consecutive_identical_calls",
                    signature: sig.clone(),
                    message: format!(
                        "REPEATED_CALL_BLOCKED: Tool '{}' was called {} consecutive times with identical arguments (sig={}). \
                        Do not repeat the same call; change your arguments or use a different tool.",
                        tool_name, MAX_CONSECUTIVE_IDENTICAL_CALLS, sig
                    ),
                };
            }
        }

        // 3. Check Period-2 Ping-Pong Oscillation: (A, B) -> (A, B) -> (A, B)
        // Only trigger on completing the 3rd full cycle (6th call: [A, B, A, B, A] + current B).
        if len >= 5 {
            let s0 = &self.recent_signatures[len - 5];
            let s1 = &self.recent_signatures[len - 4];
            let s2 = &self.recent_signatures[len - 3];
            let s3 = &self.recent_signatures[len - 2];
            let s4 = &self.recent_signatures[len - 1];
            let is_sixth_ping_pong = s0 != s1 && s0 == s2 && s1 == s3 && s0 == s4 && &sig == s1;

            if is_sixth_ping_pong {
                self.push_signature(sig.clone());
                return LoopGuardVerdict::Block {
                    code: ErrorCode::PingPongCycleBlocked,
                    guard_kind: "ping_pong_cycle",
                    signature: sig.clone(),
                    message: format!(
                        "PING_PONG_CYCLE_BLOCKED: Detected period-2 ping-pong oscillation across {} cycles involving '{}' (sig={}). \
                        Stop oscillating between the same two calls; carefully inspect the previous error diagnostics and adjust your strategy.",
                        MAX_PING_PONG_CYCLES, tool_name, sig
                    ),
                };
            }
        }

        self.push_signature(sig.clone());
        LoopGuardVerdict::Allow { signature: sig }
    }

    fn push_signature(&mut self, sig: String) {
        if self.recent_signatures.len() >= LOOP_GUARD_WINDOW_SIZE {
            self.recent_signatures.pop_front();
        }
        self.recent_signatures.push_back(sig);
    }

    /// Undo the recording made by [`LoopGuard::check_and_record`] for `signature`.
    ///
    /// A `PreToolUse` policy hook may still deny a call after it was recorded. A call that never
    /// executed must not count toward the oscillation detectors, so its signature is removed from
    /// the sliding window.
    pub fn rollback_recorded(&mut self, signature: &str) {
        if let Some(pos) = self
            .recent_signatures
            .iter()
            .rposition(|recorded| recorded == signature)
        {
            self.recent_signatures.remove(pos);
        }
    }

    /// Record the outcome of a tool execution on a file path to maintain the per-file edit failure streak.
    pub fn record_file_tool_outcome(
        &mut self,
        tool_name: &str,
        path: &str,
        success: bool,
    ) -> usize {
        let norm_path = clean_relative_path(path);
        match (tool_name, success) {
            ("edit", false) => {
                let count = self.per_file_edit_failures.entry(norm_path).or_insert(0);
                *count += 1;
                *count
            }
            ("edit", true) | ("write", true) => {
                self.per_file_edit_failures.remove(&norm_path);
                0
            }
            ("read", true) => {
                let streak = self
                    .per_file_edit_failures
                    .get(&norm_path)
                    .copied()
                    .unwrap_or(0);
                if streak >= MAX_EDIT_FAILURE_STREAK {
                    self.per_file_edit_failures.remove(&norm_path);
                    0
                } else {
                    streak
                }
            }
            _ => self
                .per_file_edit_failures
                .get(&norm_path)
                .copied()
                .unwrap_or(0),
        }
    }

    pub fn edit_failure_streak(&self, path: &str) -> usize {
        let norm_path = clean_relative_path(path);
        self.per_file_edit_failures
            .get(&norm_path)
            .copied()
            .unwrap_or(0)
    }
}
