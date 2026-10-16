//! `RpcDispatcher` methods for the revision surface.

use super::*;

impl RpcDispatcher {
    pub(crate) async fn rpc_revision_list(&self) -> Result<Value, JsonRpcError> {
        // F10: read-only surface — a failed reconciliation is reported as degraded, not fatal.
        let mut recovery_error: Option<String> = None;
        if let Ok(_guard) = self.inner.op_lock.try_lock() {
            let path = self.session_path();
            match SessionJournal::open(&path) {
                Ok(mut journal) => match self.inner.snapshot_store.reconcile_with_wal(&mut journal)
                {
                    Ok(_) => self.update_snapshot_from_journal(&journal),
                    Err(e) => recovery_error = Some(format!("{}", e)),
                },
                Err(e) => recovery_error = Some(format!("failed to open session WAL: {}", e)),
            }
        }
        let snap = self.inner.status.lock().unwrap().clone();
        let current_idx = snap
            .revisions
            .iter()
            .rposition(|r| Some(&r.revision_id) == snap.latest_revision_id.as_ref());
        let revisions: Vec<Value> = snap
            .revisions
            .iter()
            .enumerate()
            .map(|(idx, r)| {
                json!({
                    "turn": r.turn,
                    "revision_id": r.revision_id,
                    "parent_revision_id": r.parent_revision_id,
                    "changed_paths": r.changed_paths,
                    "tracked_file_count": r.tracked_file_count,
                    "captured_at_ms": r.captured_at_ms,
                    "is_current": Some(idx) == current_idx,
                })
            })
            .collect();
        let store_revisions: Vec<WorkspaceRevision> = self
            .inner
            .snapshot_store
            .list_revisions()
            .unwrap_or_default();

        Ok(json!({
            "session_id": snap.session_id,
            "latest_revision_id": snap.latest_revision_id,
            "revisions": revisions,
            "store_revision_count": store_revisions.len(),
            "recovery": {
                "degraded": recovery_error.is_some(),
                "reason": recovery_error.unwrap_or_default(),
            },
        }))
    }
    pub(crate) async fn rpc_revision_undo(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let force = params
            .get("force")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if self.inner.in_flight.load(Ordering::SeqCst) {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot undo workspace revision while a prompt is in flight; cancel or await completion first",
            ));
        }
        let Ok(_guard) = self.inner.op_lock.try_lock() else {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot undo workspace revision while another session operation is in progress",
            ));
        };
        self.validate_session_param(params)?;
        let _writer_lease = self.acquire_writer_lease()?;
        let path = self.session_path();
        let mut journal = SessionJournal::open(&path).map_err(|e| {
            JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!("failed to open session WAL: {}", e),
            )
        })?;
        self.reconcile_journal(&mut journal)?;
        let records = journal.revision_records();
        let Some(current) = records.last().cloned() else {
            return Err(JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                "no workspace revision recorded for this session",
            ));
        };
        let Some(previous) = records
            .iter()
            .rev()
            .find(|r| r.revision_id != current.revision_id)
            .cloned()
        else {
            return Err(JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                format!(
                    "nothing to undo; workspace is unchanged since revision {}",
                    current.revision_id
                ),
            ));
        };
        let target = RevisionId::parse(&previous.revision_id)
            .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e.to_string()))?;
        let expected = RevisionId::parse(&current.revision_id)
            .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e.to_string()))?;

        self.apply_restore_operation(
            &mut journal,
            &target,
            Some(&expected),
            Some(previous.turn),
            force,
        )
    }
    pub(crate) async fn rpc_revision_restore(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let raw_rev = params
            .get("revision_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "missing 'revision_id' parameter for revision.restore",
                )
            })?;
        let force = params
            .get("force")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let target = RevisionId::parse(raw_rev)
            .map_err(|e| JsonRpcError::new(JSONRPC_INVALID_PARAMS, e.to_string()))?;

        if self.inner.in_flight.load(Ordering::SeqCst) {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot restore workspace revision while a prompt is in flight; cancel or await completion first",
            ));
        }
        let Ok(_guard) = self.inner.op_lock.try_lock() else {
            return Err(JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "cannot restore workspace revision while another session operation is in progress",
            ));
        };
        self.validate_session_param(params)?;
        let _writer_lease = self.acquire_writer_lease()?;
        let path = self.session_path();
        let mut journal = SessionJournal::open(&path).map_err(|e| {
            JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!("failed to open session WAL: {}", e),
            )
        })?;
        self.reconcile_journal(&mut journal)?;
        let expected = journal
            .latest_revision_id()
            .and_then(|id| RevisionId::parse(&id).ok());

        self.apply_restore_operation(&mut journal, &target, expected.as_ref(), None, force)
    }
    pub(crate) fn apply_restore_operation(
        &self,
        journal: &mut SessionJournal,
        target: &RevisionId,
        expected: Option<&RevisionId>,
        rewind_turn: Option<usize>,
        force: bool,
    ) -> Result<Value, JsonRpcError> {
        let workspace = self.inner.env.workspace();
        let store = &self.inner.snapshot_store;

        let restore_res = match store.pending_restore() {
            Ok(Some(intent)) if &intent.target_revision == target => {
                store.recover_pending_restore(workspace)
            }
            Ok(Some(intent)) => {
                return Err(JsonRpcError::with_data(
                    JSONRPC_RECOVERY_REQUIRED,
                    format!(
                        "an interrupted restore of {} must be completed before restoring {}",
                        intent.target_revision, target
                    ),
                    json!({
                        "pending_target_revision": intent.target_revision.as_str(),
                        "intent_journal": store.intent_path().display().to_string(),
                    }),
                ));
            }
            Ok(None) => store.restore_revision(workspace, target, expected, force),
            Err(e) => Err(e),
        }
        .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, format!("restore failed: {}", e)))?;

        match restore_res {
            RestoreResult::Applied {
                restored_revision,
                restored_files,
                removed_files,
            } => {
                if let Ok(mut logger) = EventLogger::new(
                    RunId::new(),
                    Some(self.inner.config.state_dir.clone()),
                    self.inner.config.strict_log,
                ) {
                    let session_id_for_sink = journal.session_id().to_string();
                    let inner_for_sink = Arc::clone(&self.inner);
                    logger.set_event_sink(Arc::new(move |ev: &DiagnosticEvent| {
                        let mut payload = serde_json::to_value(ev).unwrap_or(Value::Null);
                        if let Value::Object(ref mut map) = payload {
                            map.insert(
                                "session_id".to_string(),
                                Value::String(session_id_for_sink.clone()),
                            );
                        }
                        Self::broadcast_to_subscribers(
                            &inner_for_sink,
                            JsonRpcNotification::new("kroma/event", payload),
                        );
                    }));
                    let _ = logger.log(EventPayload::RevisionRestored {
                        revision_id: restored_revision.as_str().to_string(),
                        restored_files: restored_files.len(),
                        removed_files: removed_files.len(),
                    });
                }

                if let Some(turn) = rewind_turn {
                    journal.fork_at_turn(turn).map_err(|e| {
                        JsonRpcError::new(
                            JSONRPC_INTERNAL_ERROR,
                            format!(
                                "workspace restored but session leaf could not be rewound: {}",
                                e
                            ),
                        )
                    })?;
                    if journal.latest_revision_id().as_deref() != Some(restored_revision.as_str()) {
                        Self::record_restored_revision_in_wal(
                            journal,
                            store,
                            &restored_revision,
                            &restored_files,
                            &removed_files,
                            expected,
                        )
                        .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;
                    }
                } else {
                    Self::record_restored_revision_in_wal(
                        journal,
                        store,
                        &restored_revision,
                        &restored_files,
                        &removed_files,
                        expected,
                    )
                    .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;
                }

                self.update_snapshot_from_journal(journal);
                Ok(json!({
                    "status": "applied",
                    "restored_revision": restored_revision.as_str(),
                    "restored_files": restored_files,
                    "removed_files": removed_files,
                    "rewound_turn": rewind_turn,
                }))
            }
            RestoreResult::Conflict {
                target_revision,
                conflicting_paths,
            } => {
                let conflicts: Vec<Value> = conflicting_paths
                    .iter()
                    .map(|c| {
                        json!({
                            "path": c.path,
                            "kind": format!("{:?}", c.kind),
                        })
                    })
                    .collect();
                Err(JsonRpcError::with_data(
                    JSONRPC_RESTORE_CONFLICT,
                    format!(
                        "refusing to restore {}; {} path(s) modified externally (pass force=true to overwrite)",
                        target_revision,
                        conflicts.len()
                    ),
                    json!({
                        "target_revision": target_revision.as_str(),
                        "conflicts": conflicts,
                    }),
                ))
            }
            RestoreResult::RecoveryRequired {
                target_revision,
                journal_path,
                reason,
            } => Err(JsonRpcError::with_data(
                JSONRPC_RECOVERY_REQUIRED,
                format!(
                    "restore of {} requires recovery: {}",
                    target_revision, reason
                ),
                json!({
                    "target_revision": target_revision.as_str(),
                    "intent_journal": journal_path.display().to_string(),
                    "reason": reason,
                }),
            )),
        }
    }
    pub(crate) fn record_restored_revision_in_wal(
        journal: &mut SessionJournal,
        store: &SnapshotStore,
        restored_revision: &RevisionId,
        restored_files: &[String],
        removed_files: &[String],
        expected: Option<&RevisionId>,
    ) -> Result<(), String> {
        crate::revision::record_restored_revision(
            journal,
            store,
            restored_revision,
            restored_files,
            removed_files,
            expected,
        )
        .map_err(|e| e.to_string())
    }
    /// P1 (`v1.5.0`): carry the cumulative `subagents_spawned` count (and the merged revision, when
    /// one was produced) forward in a checkpoint on the active branch.
    ///
    /// Existing checkpoint state is preserved so `--resume` / `--fork` never lose the pinned
    /// baseline, the subtree directories, or the mutation ledger. A session that has never written a
    /// checkpoint gets a minimal one, which is harmless because there is no earlier state to keep.
    pub(crate) fn persist_spawn_count(
        journal: &mut SessionJournal,
        spawned: usize,
        merged_revision: Option<&RevisionId>,
    ) -> Result<(), String> {
        let (prev_turn, pinned, last_touched, reasoning_sha, prev_rev, subtree, mutations) =
            match journal.latest_active_checkpoint() {
                Some(cp) => (
                    cp.turn,
                    cp.pinned_baseline_end,
                    cp.last_touched.clone(),
                    cp.reasoning_blocks_sha256.clone(),
                    cp.workspace_revision_id.clone(),
                    cp.subtree_dirs.clone(),
                    cp.mutations.clone(),
                ),
                None => (
                    0,
                    0,
                    Vec::new(),
                    String::new(),
                    None,
                    Vec::new(),
                    Vec::new(),
                ),
            };
        let revision = merged_revision.map(|r| r.as_str().to_string()).or(prev_rev);
        let turn = journal.max_active_turn().max(prev_turn);
        journal
            .checkpoint_with_subagents(
                turn,
                pinned,
                last_touched,
                reasoning_sha,
                revision,
                subtree,
                mutations,
                spawned,
            )
            .map_err(|e| e.to_string())
    }
    pub(crate) async fn rpc_evidence_recall(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let path = self.session_path();
        let journal = SessionJournal::open(&path).map_err(|e| {
            JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                format!("failed to open session WAL: {}", e),
            )
        })?;

        let mut projector = ContextProjector::new();
        journal.seed_projector_evidence(&mut projector);

        // Also register uncompacted tool results from the active branch so IDE/ACP clients can
        // inspect any tool call's evidence (including `source_entry_id` pointers in `MutationRecord`
        // before the session crosses the compaction watermark).
        for (index, entry) in journal.active_entries().iter().enumerate() {
            let raw_item = journal.raw_entry_item(&entry.id).unwrap_or(&entry.item);
            if let TranscriptItem::ToolResult(tr) = raw_item {
                let sid = source_entry_id(index, tr.call_id.as_str());
                if projector.evidence_link(&sid).is_none() {
                    let link = EvidenceLink {
                        source_entry_id: sid,
                        canonical_index: index,
                        call_id: tr.call_id.0.clone(),
                        tool_name: tr.tool_name.clone(),
                        relative_path: tr.meta.relative_path.clone(),
                        original_sha256: sha256_hex64(tr.content.as_bytes()),
                        original_bytes: tr.content.len(),
                        original_lines: tr.content.lines().count(),
                        stub_bytes: tr.content.len(),
                        reason: CompactionReason::StepDownArchive,
                    };
                    projector.register_evidence(link, Arc::clone(&tr.content));
                }
            }
        }

        let available_ids: Vec<String> = projector
            .evidence_links()
            .iter()
            .map(|l| l.source_entry_id.clone())
            .collect();

        let requested_id = params
            .get("source_entry_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let resolved_id = match requested_id {
            Some("latest") => available_ids.last().cloned().ok_or_else(|| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "no tool evidence recorded in this session yet",
                )
            })?,
            Some(id) => id.to_string(),
            None => {
                return Err(JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "missing 'source_entry_id' parameter for evidence.recall",
                ))
            }
        };

        let start_line = params
            .get("start_line")
            .or_else(|| params.get("offset"))
            .and_then(Value::as_u64)
            .map(|v| (v as usize).max(1));
        let end_line = match (
            params.get("end_line").and_then(Value::as_u64),
            params.get("limit").and_then(Value::as_u64),
        ) {
            (Some(end), _) => Some(end as usize),
            (None, Some(limit)) => {
                let s = start_line.unwrap_or(1);
                Some(s.saturating_add((limit as usize).saturating_sub(1)))
            }
            (None, None) => None,
        };
        let pattern = params
            .get("pattern")
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        let mut call_args = serde_json::Map::new();
        call_args.insert(
            "source_entry_id".to_string(),
            Value::String(resolved_id.clone()),
        );
        if let Some(s) = start_line {
            call_args.insert("start_line".to_string(), json!(s));
        }
        if let Some(e) = end_line {
            call_args.insert("end_line".to_string(), json!(e));
        }
        if let Some(p) = pattern {
            call_args.insert("pattern".to_string(), Value::String(p));
        }

        let raw_args = Value::Object(call_args).to_string();
        let out = execute_recall_evidence(&projector, &raw_args);
        if !out.outcome.is_success() {
            return Err(JsonRpcError::with_data(
                JSONRPC_INVALID_PARAMS,
                out.content,
                json!({ "available_evidence_ids": available_ids }),
            ));
        }

        Ok(json!({
            "source_entry_id": resolved_id,
            "ok": true,
            "content": out.content,
            "truncated": out.meta.truncated,
            "available_evidence_ids": available_ids,
        }))
    }
    /// Gate 7: symbol dependency graph + Personalized PageRank navigation for non-terminal surfaces.
    pub(crate) async fn rpc_workspace_graph(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let max_nodes = params
            .get("max_nodes")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(crate::repo_graph::DEFAULT_MAX_NODES)
            .clamp(1, crate::repo_graph::MAX_MAX_NODES);
        let query = params
            .get("query")
            .and_then(Value::as_str)
            .map(str::to_string);
        let query_terms: Vec<String> = query
            .as_deref()
            .unwrap_or("")
            .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '.'))
            .map(str::trim)
            .filter(|t| t.len() >= 2)
            .map(str::to_string)
            .take(16)
            .collect();
        let mut focus_files: Vec<String> = Vec::new();
        if let Some(list) = params.get("focus_files").and_then(Value::as_array) {
            if list.len() > 32 {
                return Err(JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "focus_files accepts at most 32 entries",
                ));
            }
            for entry in list {
                let Some(raw) = entry.as_str() else {
                    continue;
                };
                let normalized = raw.trim().trim_start_matches("./").replace('\\', "/");
                if normalized.is_empty() {
                    continue;
                }
                WorkspaceCap::validate_relative_components(&normalized).map_err(|e| {
                    JsonRpcError::new(
                        JSONRPC_INVALID_PARAMS,
                        format!("invalid focus_files entry '{}': {}", raw, e),
                    )
                })?;
                focus_files.push(normalized);
            }
        }

        let graph = crate::repo_graph::RepoGraph::build_from_workspace(
            self.inner.env.workspace(),
            crate::repo_graph::MAX_GRAPH_FILES,
        );
        let report = graph.query(&query_terms, &focus_files, max_nodes);
        let ranked: Vec<Value> = report
            .ranked
            .iter()
            .map(|node| {
                json!({
                    "path": node.path,
                    "score": node.score,
                    "symbols": node
                        .symbols
                        .iter()
                        .map(|s| json!({
                            "name": s.name,
                            "kind": s.kind.as_str(),
                            "line": s.line,
                            "signature": s.signature,
                        }))
                        .collect::<Vec<Value>>(),
                    "called_by": node.called_by,
                    "calls_into": node.calls_into,
                })
            })
            .collect();

        Ok(json!({
            "query_terms": report.query_terms,
            "focus_files": report.focus_files,
            "total_files": report.total_files,
            "total_symbols": report.total_symbols,
            "total_edges": report.total_edges,
            "truncated": report.truncated,
            "skipped_files": graph.skipped_files,
            "ranked": ranked,
        }))
    }
    pub(crate) async fn rpc_skills_list(&self) -> Result<Value, JsonRpcError> {
        let discovered = discover_skills(self.inner.env.workspace());
        let activated = self.inner.status.lock().unwrap().activated_skills.clone();
        let skills: Vec<Value> = discovered
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "description": s.description,
                    "path": s.path,
                    "activated": activated.contains(&s.name),
                })
            })
            .collect();
        Ok(json!({ "skills": skills }))
    }
    pub(crate) async fn rpc_skills_activate(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    "missing 'name' parameter for skills.activate",
                )
            })?;
        let raw_args = json!({ "name": name }).to_string();
        let catalog = discover_skills(self.inner.env.workspace());
        let out = execute_activate_skill(&catalog, &raw_args);
        if !out.outcome.is_success() {
            return Err(JsonRpcError::new(JSONRPC_INVALID_PARAMS, out.content));
        }
        {
            let mut snap = self.inner.status.lock().unwrap();
            if !snap.activated_skills.iter().any(|s| s == name) {
                snap.activated_skills.push(name.to_string());
            }
        }
        Ok(json!({
            "name": name,
            "activated": true,
            "content": out.content,
        }))
    }

    // -----------------------------------------------------------------------------------------
    // v1.3.0 — persistent configuration and live model/protocol hot-swapping
    // -----------------------------------------------------------------------------------------
}
