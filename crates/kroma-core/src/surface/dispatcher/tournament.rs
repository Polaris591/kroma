//! `RpcDispatcher` methods for the tournament surface.

use super::*;

impl RpcDispatcher {
    /// F01 (`v1.4.0`): shared authorization + quota preflight for `session.tournament`.
    ///
    /// Mirrors the `spawn_subagent` tool path exactly: `max_subagents` quota, `PolicyEngine`
    /// evaluation with `EffectClass::Mutating`, the registered fail-closed `PolicyHook` gate, and an
    /// interactive approval round-trip for `Ask`. Before this fix the RPC path skipped all of these,
    /// so a `ReadOnly` session could write (and merge) files through `/race`.
    pub(crate) async fn tournament_preflight(&self, candidate_count: usize) -> Result<(), JsonRpcError> {
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
            raw_arguments: "{\"role\":\"worker\"}".to_string(),
        };
        if let Err((denied, _)) = runtime.pre_tool_hook_gate(&hook_call, 0).await {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!(
                    "policy hook denied session.tournament: {}",
                    denied.content
                ),
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
                    effect_class: EffectClass::Mutating,
                    is_sensitive_path: false,
                    target_path: None,
                    diff_preview: None,
                    command_preview: None,
                    workspace_root: self.inner.env.workspace().root().to_path_buf(),
                };
                let engine = self.inner.policy_engine.clone();
                let mut prompt_task =
                    tokio::task::spawn_blocking(move || engine.prompt_approval(&req));
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
                    ApprovalDecision::AllowOnce | ApprovalDecision::AllowCategoryForRun => Ok(()),
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
    pub(crate) async fn rpc_session_tournament(&self, params: &Value) -> Result<Value, JsonRpcError> {
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

        // F01 (`v1.4.0`): enforce the same authorization + quota as the `spawn_subagent` tool path
        // BEFORE any candidate worktree is materialized.
        self.tournament_preflight(count).await?;

        // P1 (`v1.5.0`): reserve the candidate budget in the live session counter immediately
        // after preflight (matching `execute_spawn_tournament`) so a second `/race` on the same
        // session is measured against what this one is about to spend.
        {
            let mut status = self
                .inner
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            status.subagents_spawned = status.subagents_spawned.saturating_add(count);
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

        // Candidate strategy hints: distinct by default so the candidates actually diverge.
        let default_strategies = [
            "Fix the root cause with the smallest possible change.",
            "Consider an alternative design and pick the most robust minimal fix.",
            "Re-derive the failure from first principles before editing.",
            "Prefer a defensive fix that also adds a regression guard.",
        ];

        let mut candidates: Vec<crate::subagents::TournamentCandidate> = Vec::with_capacity(count);
        for index in 0..count {
            let label = format!("c{}", index + 1);
            let strategy = strategies
                .get(index)
                .cloned()
                .unwrap_or_else(|| default_strategies[index % default_strategies.len()].to_string());
            let mut spec = crate::subagents::SubagentSpec::worker(format!(
                "{}\n\n[TOURNAMENT CANDIDATE {} STRATEGY] {}",
                task,
                label,
                strategy
            ));
            if let Some(globs) = &allowed_write_globs {
                spec = spec.with_allowed_write_globs(globs.clone());
            }
            if let Some(turns) = max_turns {
                spec = spec.with_max_turns(turns);
            }
            spec = spec.with_timeout_ms(timeout_ms);
            candidates.push(
                crate::subagents::TournamentCandidate {
                    label,
                    strategy: Some(strategy),
                    spec,
                },
            );
        }

        let workspace = self.inner.env.workspace().clone();
        let store = Arc::clone(&self.inner.snapshot_store);
        let provider = Arc::clone(&self.inner.provider.lock().unwrap_or_else(|e| e.into_inner()));
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
            budget: self.inner.config.budget.clone(),
            policy_mode: self.inner.policy_engine.mode(),
            workspace_skills: self.inner.registry.skills(),
            predictive_check: self.inner.config.predictive_check,
            cancel_rx: self.ensure_active_cancel_receiver(),
            timeout_ms,
            disallow_test_tampering,
            protected_paths: protected_paths.clone(),
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
        if let Ok(mut journal) = SessionJournal::open(&self.session_path()) {
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
