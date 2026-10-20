//! `RpcDispatcher` methods for the tournament surface.

use super::*;
use crate::tool::policy::{canonical_arguments_digest, AuthEntry, AuthScope};

impl RpcDispatcher {
    /// F01 (`v1.4.0`): shared authorization + quota preflight for `session.tournament`.
    ///
    /// Mirrors the `spawn_subagent` tool path exactly: `max_subagents` quota, `PolicyEngine`
    /// evaluation with `EffectClass::Mutating`, the registered fail-closed `PolicyHook` gate, and an
    /// interactive approval round-trip for `Ask`. Before this fix the RPC path skipped all of these,
    /// so a `ReadOnly` session could write (and merge) files through `/race`.
    pub(crate) async fn tournament_preflight(
        &self,
        candidate_count: usize,
        approved_arguments: &str,
    ) -> Result<(), JsonRpcError> {
        // Quota + policy mode (shared helper, same as the tool path). P1 (`v1.5.0`): the live
        // session spawn count is passed in so repeated `/race` calls cannot bypass `max_subagents`.
        let already_spawned = self
            .inner
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subagents_spawned;
        let check = match crate::subagents::subagent_spawn_preflight(
            &self.inner.policy_engine,
            candidate_count,
            already_spawned,
            self.inner.config.max_subagents,
            EffectClass::Mutating,
        ) {
            Ok(check) => check,
            Err(reason) => {
                return Err(JsonRpcError::new(JSONRPC_INTERNAL_ERROR, reason));
            }
        };

        // `PolicyHook` gate: a registered fail-closed hook can veto the tournament before any
        // worktree is materialized.
        let runtime = build_runtime_with_registry(
            self.inner.registry.clone(),
            Arc::clone(&self.inner.env) as Arc<dyn ExecutionEnv>,
            self.inner.policy_engine.clone(),
            self.inner.config.budget.clone(),
        );
        let hook_call = ToolCall {
            call_id: ToolCallId::new("rpc-tournament"),
            name: "spawn_subagent".to_string(),
            // The hook and approval digest must see the *actual* normalized intent, not
            // an unrelated fixed placeholder whose approval could be replayed for any race.
            raw_arguments: approved_arguments.to_owned(),
        };
        if let Err((denied, _)) = runtime.pre_tool_hook_gate(&hook_call, 0).await {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!("policy hook denied session.tournament: {}", denied.content),
            ));
        }

        match check {
            PolicyCheckResult::Allow => Ok(()),
            PolicyCheckResult::Deny(reason) => Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!("{} (session.tournament requires write access)", reason),
            )),
            PolicyCheckResult::Ask => {
                let mut cancel_rx = self.ensure_active_cancel_receiver();
                if *cancel_rx.borrow() {
                    return Err(JsonRpcError::new(
                        JSONRPC_INTERNAL_ERROR,
                        "session.tournament cancelled before approval prompt",
                    ));
                }
                let req = ApprovalRequest {
                    tool_name: "spawn_subagent".to_string(),
                    call_id: None,
                    effect_class: EffectClass::Mutating,
                    is_sensitive_path: false,
                    target_path: None,
                    diff_preview: None,
                    command_preview: None,
                    workspace_root: self.inner.env.workspace().root().to_path_buf(),
                };
                // B11a: the RPC tournament gate is a distinct authority entry point from the
                // `spawn_subagent` tool, and the RPC carries no run identity, so a category answer
                // degrades to a single call rather than becoming a reusable cross-request grant.
                // B12a: bind it to the session's current revision too, so an approval cannot
                // survive a `revision.restore`/`--undo` that moved the workspace.
                let revision = self
                    .inner
                    .status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .latest_revision_id
                    .as_deref()
                    .and_then(|id| RevisionId::parse(id).ok());
                let auth_scope = AuthScope {
                    entry: AuthEntry::Tournament,
                    tool_name: "spawn_subagent".to_string(),
                    run_id: None,
                    workspace_root: self.inner.env.workspace().root().to_path_buf(),
                    revision,
                };
                let args_digest = canonical_arguments_digest(&hook_call.raw_arguments);
                let engine = self.inner.policy_engine.clone();
                let scope_for_prompt = auth_scope.clone();
                let digest_for_prompt = args_digest.clone();
                let mut prompt_task = tokio::task::spawn_blocking(move || {
                    engine.prompt_approval_scoped(
                        &scope_for_prompt,
                        &req,
                        digest_for_prompt.as_deref(),
                        None,
                    )
                });
                let decision = tokio::select! {
                    res = &mut prompt_task => res.unwrap_or(ApprovalDecision::Deny),
                    _ = async {
                        loop {
                            if *cancel_rx.borrow() {
                                break;
                            }
                            if cancel_rx.changed().await.is_err() {
                                std::future::pending::<()>().await;
                            }
                        }
                    } => {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_millis(200),
                            &mut prompt_task,
                        )
                        .await;
                        return Err(JsonRpcError::new(
                            JSONRPC_INTERNAL_ERROR,
                            "session.tournament approval cancelled",
                        ));
                    }
                };
                match decision {
                    ApprovalDecision::AllowOnce | ApprovalDecision::AllowCategoryForRun => {
                        // Ask must mint an exact, one-shot digest-bound grant. An absent grant
                        // is not an implicit Allow: cancellation or policy reconfiguration may
                        // have invalidated it during the operator's decision.
                        let digest = args_digest.as_deref().ok_or_else(|| {
                            JsonRpcError::new(
                                JSONRPC_INTERNAL_ERROR,
                                "approval intent has no digest",
                            )
                        })?;
                        self.inner
                            .policy_engine
                            .consume_grant(&auth_scope, digest)
                            .map_err(|rejection| {
                                JsonRpcError::new(
                                    JSONRPC_INTERNAL_ERROR,
                                    format!(
                                        "session.tournament approval grant rejected: {:?}",
                                        rejection
                                    ),
                                )
                            })?;
                        let current_revision = self
                            .inner
                            .status
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .latest_revision_id
                            .as_deref()
                            .and_then(|id| RevisionId::parse(id).ok());
                        if current_revision != auth_scope.revision {
                            return Err(JsonRpcError::new(
                                JSONRPC_INTERNAL_ERROR,
                                "session.tournament revision changed while approval was pending",
                            ));
                        }
                        // T02 (KR-30): the approved budget must still be the budget that runs. A
                        // `/config set max_tokens` (or any other live cap change) during the
                        // approval wait invalidates the decision instead of silently spending a
                        // different amount than the operator approved.
                        // T02 (KR-30): the approved budget must still be the budget that runs. A
                        // `/config set max_tokens` (or any other live cap change) during the
                        // approval wait invalidates the decision instead of silently spending a
                        // different amount than the operator approved.
                        let approved_max_tokens = serde_json::from_str::<Value>(approved_arguments)
                            .ok()
                            .and_then(|v| {
                                v.get("budget")
                                    .and_then(|b| b.get("max_total_tokens"))
                                    .and_then(Value::as_u64)
                            });
                        if self.active_max_tokens() != approved_max_tokens {
                            return Err(JsonRpcError::new(
                                JSONRPC_INTERNAL_ERROR,
                                "session.tournament token budget changed while approval was pending",
                            ));
                        }
                        Ok(())
                    }
                    ApprovalDecision::Deny => Err(JsonRpcError::new(
                        JSONRPC_INTERNAL_ERROR,
                        "session.tournament denied by approval policy",
                    )),
                }
            }
        }
    }
    /// `session.tournament`: run a Best-of-N worktree tournament and merge only the winner.
    ///
    /// Each candidate runs in its own isolated worktree, is verified inside that worktree, and is
    /// ranked deterministically. The parent workspace is only touched when a candidate passed
    /// verification, and the winning merge is recorded as a single WAL revision so `/undo` reverts
    /// the whole tournament in one step.
    pub(crate) async fn rpc_session_tournament(
        &self,
        params: &Value,
    ) -> Result<Value, JsonRpcError> {
        if self.inner.in_flight.load(Ordering::SeqCst) {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot run a tournament while a prompt is in flight; cancel or await completion first",
            ));
        }
        let Ok(_guard) = self.inner.op_lock.try_lock() else {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot run a tournament while another session operation is in progress",
            ));
        };
        // A tournament merges its winner into the parent workspace, so it is a writer: it must own
        // the workspace lease and refuse to run over a half-applied interrupted restore.
        self.validate_session_param(params)?;
        let _writer_lease = self.acquire_writer_lease()?;
        self.ensure_no_pending_restore()?;

        // B01: bind this tournament run to the connection that dispatched it before the approval
        // preflight, so a disconnect during that wait cancels it instead of leaking a writer lease.
        let conn = CONNECTION_CONTEXT.try_with(|c| c.clone()).ok();
        let request = RUN_REQUEST.try_with(|r| r.clone()).ok();
        let Some(claim) = self.claim_run(conn.as_ref(), request.as_ref()) else {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "the connection that started this tournament closed before it began",
            ));
        };
        let RunClaim {
            run_id,
            cancel_rx,
            acp_owner,
        } = claim;
        let _run_guard = RunGuard {
            inner: Arc::clone(&self.inner),
            run_id,
            in_flight: false,
            acp_owner,
        };

        // Fail closed on supplied but mistyped optional parameters. Silent coercion would
        // let the operator approve a different execution scope than the caller requested.
        for (field, valid) in [
            (
                "candidates",
                params.get("candidates").is_none_or(Value::is_u64),
            ),
            (
                "max_turns",
                params.get("max_turns").is_none_or(Value::is_u64),
            ),
            (
                "timeout_ms",
                params.get("timeout_ms").is_none_or(Value::is_u64),
            ),
            (
                "disallow_test_tampering",
                params
                    .get("disallow_test_tampering")
                    .is_none_or(Value::is_boolean),
            ),
            (
                "strategies",
                params
                    .get("strategies")
                    .is_none_or(|v| v.as_array().is_some_and(|a| a.iter().all(Value::is_string))),
            ),
            (
                "protected_paths",
                params
                    .get("protected_paths")
                    .is_none_or(|v| v.as_array().is_some_and(|a| a.iter().all(Value::is_string))),
            ),
            (
                "allowed_paths",
                params
                    .get("allowed_paths")
                    .is_none_or(|v| v.as_array().is_some_and(|a| a.iter().all(Value::is_string))),
            ),
            (
                "allowed_write_globs",
                params
                    .get("allowed_write_globs")
                    .is_none_or(|v| v.as_array().is_some_and(|a| a.iter().all(Value::is_string))),
            ),
        ] {
            if !valid {
                return Err(JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    format!("invalid type for session.tournament parameter '{field}'"),
                ));
            }
        }

        let task = params
            .get("task")
            .or_else(|| params.get("prompt"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "missing non-empty 'task' (or 'prompt') string parameter",
                )
            })?
            .to_string();

        let count = params
            .get("candidates")
            .and_then(Value::as_u64)
            .unwrap_or(3) as usize;
        if !(crate::subagents::MIN_TOURNAMENT_CANDIDATES
            ..=crate::subagents::MAX_TOURNAMENT_CANDIDATES)
            .contains(&count)
        {
            return Err(JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                format!(
                    "'candidates' must be between {} and {}",
                    crate::subagents::MIN_TOURNAMENT_CANDIDATES,
                    crate::subagents::MAX_TOURNAMENT_CANDIDATES
                ),
            ));
        }

        let strategies: Vec<String> = params
            .get("strategies")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let allowed_write_globs: Option<Vec<String>> = params
            .get("allowed_paths")
            .or_else(|| params.get("allowed_write_globs"))
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_string())
                    .collect()
            });
        let max_turns = params
            .get("max_turns")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let timeout_ms = params
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(crate::subagents::DEFAULT_SUBAGENT_TIMEOUT_MS);
        // Item #3 (`v1.4.0`): protected test/verification paths.
        let disallow_test_tampering = params
            .get("disallow_test_tampering")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let protected_paths: Vec<String> = params
            .get("protected_paths")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();

        // T01 (KR-01): the host decides the verification requirement and owns the base protection
        // set; RPC parameters can only tighten them. Resolved here, before the approval intent is
        // built, so the digest covers exactly what will run.
        let verification = crate::subagents::resolve_tournament_verification(
            self.inner.config.tournament_require_verified,
            None,
        );
        let protected_paths = crate::subagents::merge_protected_globs(
            &self.inner.config.tournament_protected_paths,
            &protected_paths,
        );
        let disallow_test_tampering = crate::subagents::effective_disallow_test_tampering(
            self.inner.config.tournament_disallow_test_tampering,
            disallow_test_tampering,
        );

        // Resolve every candidate strategy before authorization: the hook must inspect the
        // exact prompts used in isolated worktrees, including defaulted strategies.
        let default_strategies = [
            "Fix the root cause with the smallest possible change.",
            "Consider an alternative design and pick the most robust minimal fix.",
            "Re-derive the failure from first principles before editing.",
            "Prefer a defensive fix that also adds a regression guard.",
        ];
        let resolved_strategies: Vec<String> = (0..count)
            .map(|index| {
                strategies.get(index).cloned().unwrap_or_else(|| {
                    default_strategies[index % default_strategies.len()].to_string()
                })
            })
            .collect();

        // Build the host-executable intent before asking hooks or a human. It includes the
        // effective budget, all candidate-defining fields, frozen verification policy and
        // requested protected paths. This same budget is used when actually launching the race.
        let mut approved_budget = self.inner.config.budget.clone();
        approved_budget.max_total_tokens = self.active_max_tokens();
        // A missing/invalid field must not be silently coerced into a different authorized
        // scope. Validate the tool-compatible fields using the existing shared parser.
        let parse_only = serde_json::json!({
            "role": "worker",
            "mode": "tournament",
            "task": task,
            "candidates": count,
            "allowed_paths": allowed_write_globs,
            "max_turns": max_turns,
            "timeout_ms": timeout_ms,
            "protected_paths": protected_paths,
        })
        .to_string();
        let validated = crate::subagents::parse_tournament_request(&parse_only)
            .map_err(|msg| JsonRpcError::new(JSONRPC_INVALID_PARAMS, msg))?
            .ok_or_else(|| JsonRpcError::new(JSONRPC_INVALID_PARAMS, "expected tournament mode"))?;
        let allowed_write_globs = validated.allowed_write_globs;
        if max_turns == Some(0) {
            return Err(JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                "max_turns must be > 0",
            ));
        }
        // T02 (KR-30): one normalized intent feeds the hook, the approval digest and the run.
        let intent = crate::subagents::CanonicalTournamentIntent {
            task: task.clone(),
            candidates: count,
            strategies: resolved_strategies.clone(),
            allowed_paths: allowed_write_globs.clone(),
            max_turns,
            timeout_ms,
            disallow_test_tampering,
            protected_paths: protected_paths.clone(),
            require_verified: verification.require_verified,
            verification_policy_source: verification.source.as_str().to_string(),
            budget: crate::subagents::CanonicalTournamentBudget::from_config(&approved_budget),
            predictive_check: self.inner.config.predictive_check,
        };
        let approved_arguments = intent.canonical_json();
        // Always resolve and approve first; quota is committed only after successful approval.
        self.tournament_preflight(count, &approved_arguments)
            .await?;
        {
            let mut status = self.inner.status.lock().unwrap_or_else(|e| e.into_inner());
            status.subagents_spawned = status.subagents_spawned.saturating_add(count);
        }

        let mut candidates: Vec<crate::subagents::TournamentCandidate> = Vec::with_capacity(count);
        for (index, strategy) in resolved_strategies.iter().enumerate() {
            let label = format!("c{}", index + 1);
            let strategy = strategy.clone();
            let mut spec = crate::subagents::SubagentSpec::worker(format!(
                "{}\n\n[TOURNAMENT CANDIDATE {} STRATEGY] {}",
                task, label, strategy
            ));
            if let Some(globs) = &allowed_write_globs {
                spec = spec.with_allowed_write_globs(globs.clone());
            }
            if let Some(turns) = max_turns {
                spec = spec.with_max_turns(turns);
            }
            spec = spec.with_timeout_ms(timeout_ms);
            candidates.push(crate::subagents::TournamentCandidate {
                label,
                strategy: Some(strategy),
                spec,
            });
        }

        let workspace = self.inner.env.workspace().clone();
        let store = Arc::clone(&self.inner.snapshot_store);
        let provider = Arc::clone(
            &self
                .inner
                .provider
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        let backend: Arc<dyn crate::subagents::SubagentBackend> =
            Arc::new(crate::subagents::ProviderSubagentBackend::new(provider));

        let parent_revision_id: Option<RevisionId> = self
            .inner
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .latest_revision_id
            .clone()
            .map(RevisionId::new);
        let turn = self
            .inner
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .turn_count;

        // Capture a fresh baseline of the live parent workspace so every candidate forks from the
        // exact bytes on disk right now (not from a possibly stale earlier revision).
        let baseline = store
            .capture_revision(&workspace, parent_revision_id.as_ref(), None, turn)
            .map_err(|e| {
                JsonRpcError::new(
                    JSONRPC_INTERNAL_ERROR,
                    format!("failed to capture tournament baseline revision: {}", e),
                )
            })?;
        let base_revision = baseline.revision_id.clone();

        let spec = crate::subagents::TournamentSpec {
            candidates,
            worktree_base: self.inner.config.state_dir.join("worktrees"),
            state_base: self.inner.config.state_dir.join("subagents"),
            allowed_write_globs,
            // F05 (`v1.5.1`): honor the live `/config set max_tokens` cap for a standalone race,
            // exactly like `session.prompt` does.
            budget: approved_budget,
            // F05 (`v1.5.1`): the tournament derives its own shared ledger from the run budget's
            // `max_total_tokens` (a single RPC call has no parent runner to share with).
            token_budget: None,
            policy_mode: self.inner.policy_engine.mode(),
            // B11b: read the parent's fail-closed policy-hook constraints from the real session
            // registry, so a candidate is refused rather than run without the parent's constraints.
            parent_policy_hooks: crate::subagents::ParentPolicyHooks::from_registry(
                &self.inner.registry,
            ),
            workspace_skills: self.inner.registry.skills(),
            predictive_check: self.inner.config.predictive_check,
            // R01: candidates are the parent's children, so they run under the parent session's
            // host-resolved boundary (never a per-request or model-chosen one).
            execution_policy: self.inner.config.execution_policy.clone(),
            cancel_rx,
            timeout_ms,
            disallow_test_tampering,
            protected_paths: protected_paths.clone(),
            // Capability gap #5 (`v1.5.2`): an RPC repair race follows the same host policy as the
            // tool path (T01 parity) — strict unless the operator relaxed it.
            require_verified: verification.require_verified,
        };

        let outcome = crate::subagents::run_worktree_tournament(
            spec,
            &workspace,
            &store,
            backend,
            Some(&base_revision),
            &self.inner.config.completion_gate,
        )
        .await
        .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;

        // P1 (`v1.5.0`): persist the tournament outcome in the session WAL. The winning merge is
        // recorded as one `RevisionCaptured` record so `/undo` reverts it atomically (F04), and the
        // cumulative `subagents_spawned` count is carried forward in a checkpoint so later prompt
        // runs and `--resume` / `--fork` honour `max_subagents`.
        //
        // F04 (`v1.4.0`): a WAL failure is an explicit RPC error, never a silent success — when a
        // merge happened the revision must be durably recorded. When nothing merged the in-memory
        // count still enforces the quota, so a WAL failure is tolerated.
        if let Ok(mut journal) = SessionJournal::open(self.session_path()) {
            if let Some(rev) = outcome.merged_revision.clone() {
                match store.load_revision(&rev) {
                    Ok(manifest) => {
                        if let Err(e) = journal.record_revision_capture(
                            journal.max_active_turn(),
                            rev.as_str(),
                            Some(base_revision.as_str()),
                            manifest.changed_paths.clone(),
                            manifest.tracked_files.len(),
                        ) {
                            return Err(JsonRpcError::new(
                                JSONRPC_INTERNAL_ERROR,
                                format!(
                                    "tournament merged revision {} but appending it to the session WAL failed ({}); the merge is on disk but unrecorded",
                                    rev.as_str(),
                                    e
                                ),
                            ));
                        }
                    }
                    Err(e) => {
                        return Err(JsonRpcError::new(
                            JSONRPC_INTERNAL_ERROR,
                            format!(
                                "tournament merged revision {} but its manifest could not be loaded ({}); the merge is on disk but unrecorded",
                                rev.as_str(),
                                e
                            ),
                        ));
                    }
                }
            }
            let spawned = self
                .inner
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .subagents_spawned;
            if let Err(e) =
                Self::persist_spawn_count(&mut journal, spawned, outcome.merged_revision.as_ref())
            {
                if let Some(rev) = &outcome.merged_revision {
                    return Err(JsonRpcError::new(
                        JSONRPC_INTERNAL_ERROR,
                        format!(
                            "tournament merged revision {} but its spawn count could not be recorded ({}); the merge is on disk but unrecorded",
                            rev.as_str(),
                            e
                        ),
                    ));
                }
            }
            self.update_snapshot_from_journal(&journal);
        } else if let Some(rev) = &outcome.merged_revision {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!(
                    "tournament merged revision {} but the session WAL could not be opened; the merge is on disk but unrecorded",
                    rev.as_str()
                ),
            ));
        }

        let note = JsonRpcNotification::new(
            "kroma/event",
            json!({
                "schema_version": crate::events::EVENT_SCHEMA_VERSION,
                "event": "tournament_completed",
                "candidates": outcome.scores.len(),
                "merged_revision": outcome.merged_revision.as_ref().map(|r| r.as_str().to_string()),
                "winner": outcome.winner.map(|i| outcome.scores[i].label.clone()),
            }),
        );
        Self::broadcast_to_subscribers(&self.inner, note);

        Ok(json!({
            "session_id": self.session_id(),
            "candidates": outcome.scores.iter().map(|s| json!({
                "label": s.label,
                "status": s.status,
                "gate_passed": s.gate_passed,
                "gate_status": s.gate_status.as_str(),
                "gate_detail": s.gate_detail,
                "predictive_warnings": s.predictive_warnings,
                "conflict_free_with_parent": s.conflict_free_with_parent,
                "files_modified": s.files_modified,
                "total_changed_lines": s.total_changed_lines,
                "turns_used": s.turns_used,
                "winner": s.winner,
            })).collect::<Vec<Value>>(),
            "winner": outcome.winner.map(|i| outcome.scores[i].label.clone()),
            "merged_revision": outcome.merged_revision.as_ref().map(|r| r.as_str().to_string()),
            "parent_untouched": outcome.parent_untouched(),
            "all_worktrees_cleaned_up": outcome.all_worktrees_cleaned_up,
            "report": outcome.render_report(),
        }))
    }
}
