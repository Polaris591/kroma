//! `RpcDispatcher` methods for the config surface.

use super::*;

impl RpcDispatcher {
    /// `config.get`: redacted layered configuration plus the active model/protocol.
    pub(crate) async fn rpc_config_get(&self, _params: &Value) -> Result<Value, JsonRpcError> {
        let resolved = self
            .inner
            .resolved_config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let active_model = self.active_model();
        let active_protocol = self.active_protocol();
        let hot_swap_available = self.inner.config.provider_factory.is_some();

        let (summary, rejected, user_path, workspace_path) = match &resolved {
            Some(cfg) => (
                cfg.redacted_summary(),
                cfg.rejected_workspace_keys.clone(),
                cfg.user_config_path
                    .as_ref()
                    .map(|p| p.display().to_string()),
                cfg.workspace_config_path
                    .as_ref()
                    .map(|p| p.display().to_string()),
            ),
            // Surfaces started without layered configuration still report the live state so
            // `/config show` is never blank.
            None => (
                format!(
                    "profile:          <none>\nmodel:            {}\nprotocol:         {}\nreasoning_effort: {}\nnote:             no ~/.kroma/config.toml or .kroma.toml was loaded\n",
                    active_model.clone().unwrap_or_else(|| "<unset>".to_string()),
                    active_protocol.clone().unwrap_or_else(|| "<unset>".to_string()),
                    self.active_reasoning_effort()
                        .unwrap_or_else(|| "off".to_string()),
                ),
                Vec::new(),
                None,
                None,
            ),
        };

        Ok(json!({
            "active_model": active_model,
            "active_protocol": active_protocol,
            "ui_theme": self.active_theme().as_str(),
            "hot_swap_available": hot_swap_available,
            "summary": summary,
            "rejected_workspace_keys": rejected,
            "workspace_guard_triggered": !rejected.is_empty(),
            "user_config_path": user_path,
            "workspace_config_path": workspace_path,
            "api_key": if resolved.as_ref().and_then(|c| c.api_key.as_ref()).is_some() {
                crate::config_file::REDACTED_API_KEY
            } else {
                "<unset>"
            },
        }))
    }
    /// `config.set` / `session.configure`: hot-swap the active model, wire protocol and/or profile
    /// without restarting the session, optionally persisting the change to `~/.kroma/config.toml`.
    pub(crate) async fn rpc_config_set(&self, params: &Value) -> Result<Value, JsonRpcError> {
        let model = params
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let protocol = params
            .get("protocol")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let profile = params
            .get("profile")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // `reasoning_effort` may be an explicit level, or `off`/`none`/`clear` to disable it.
        let reasoning_raw = params
            .get("reasoning_effort")
            .or_else(|| params.get("thinking"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // B2 (`v1.4.0`): `projection_profile` is a safe, hot-swappable key that does not require a
        // provider rebuild.
        let projection_profile_raw = params
            .get("projection_profile")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // Item #3 (`v1.4.0`): `adaptive_tournament` is also a safe, hot-swappable key.
        // P2 (`v1.5.0`): `/config set adaptive_tournament 3` arrives as a JSON string (the REPL
        // forwards raw text), so accept both a number and a numeric string.
        let adaptive_tournament_raw = params
            .get("adaptive_tournament")
            .and_then(|v| {
                v.as_u64().or_else(|| {
                    v.as_str()
                        .map(str::trim)
                        .and_then(|s| s.parse::<u64>().ok())
                })
            })
            .map(|n| n as usize);
        // `v1.5.0`: `ui_theme` is a safe, hot-swappable presentation key (`zen` | `classic`).
        let ui_theme_raw = params
            .get("ui_theme")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let save = params.get("save").and_then(Value::as_bool).unwrap_or(false);

        if model.is_none()
            && protocol.is_none()
            && profile.is_none()
            && reasoning_raw.is_none()
            && projection_profile_raw.is_none()
            && adaptive_tournament_raw.is_none()
            && ui_theme_raw.is_none()
        {
            return Err(JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                "config.set requires at least one of 'model', 'protocol', 'reasoning_effort', 'profile', 'projection_profile', 'adaptive_tournament' or 'ui_theme'"
                    .to_string(),
            ));
        }

        let new_theme = match &ui_theme_raw {
            Some(raw) => Some(
                Theme::parse(raw).map_err(|e| JsonRpcError::new(JSONRPC_INVALID_PARAMS, e))?,
            ),
            None => None,
        };

        let new_projection_profile = match &projection_profile_raw {
            Some(raw) => Some(
                crate::context_projector::ProjectionProfile::parse(raw)
                    .map_err(|e| JsonRpcError::new(JSONRPC_INVALID_PARAMS, e))?,
            ),
            None => self.active_projection_profile(),
        };

        let mut new_model = self.active_model();
        let mut new_protocol = self.active_protocol();
        let mut new_reasoning = self.active_reasoning_effort();

        if let Some(name) = &profile {
            let user = crate::config_file::KromaConfigFile::load_user_global()
                .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;
            let entry = user.profiles.get(name).ok_or_else(|| {
                let known: Vec<&str> = user.profiles.keys().map(String::as_str).collect();
                JsonRpcError::new(
                    JSONRPC_INVALID_PARAMS,
                    format!(
                        "unknown profile '{}'; available: {}",
                        name,
                        if known.is_empty() {
                            "<none>".to_string()
                        } else {
                            known.join(", ")
                        }
                    ),
                )
            })?;
            if let Some(m) = &entry.model {
                new_model = Some(m.clone());
            }
            if let Some(p) = &entry.protocol {
                new_protocol = Some(p.clone());
            }
            if let Some(r) = &entry.reasoning_effort {
                new_reasoning = Some(r.clone());
            }
        }
        if let Some(m) = &model {
            new_model = Some(m.clone());
        }
        if let Some(p) = &protocol {
            new_protocol = Some(p.clone());
        }
        if let Some(raw) = &reasoning_raw {
            new_reasoning = normalize_reasoning_effort(raw);
        }

        let needs_provider_rebuild = model.is_some()
            || protocol.is_some()
            || profile.is_some()
            || reasoning_raw.is_some();

        if !needs_provider_rebuild {
            // Only safe hot-swappable keys changed: apply them in place without touching the
            // provider.
            if projection_profile_raw.is_some() {
                *self
                    .inner
                    .active_projection_profile
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = new_projection_profile;
            }
            if let Some(n) = adaptive_tournament_raw {
                *self
                    .inner
                    .active_adaptive_tournament
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = n;
            }
            if let Some(theme) = new_theme {
                *self
                    .inner
                    .active_theme
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = theme;
            }
            {
                let mut guard = self
                    .inner
                    .resolved_config
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(cfg) = guard.as_mut() {
                    if projection_profile_raw.is_some() {
                        cfg.projection_profile = projection_profile_raw.clone();
                    }
                    if let Some(n) = adaptive_tournament_raw {
                        cfg.adaptive_tournament = Some(n);
                    }
                }
            }
            let mut persisted_to = None;
            if save {
                let mut file = crate::config_file::KromaConfigFile::load_user_global()
                    .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;
                if projection_profile_raw.is_some() {
                    file.projection_profile = projection_profile_raw.clone();
                }
                if let Some(n) = adaptive_tournament_raw {
                    file.adaptive_tournament = Some(n);
                }
                let path = file
                    .save_user_global()
                    .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;
                persisted_to = Some(path.display().to_string());
            }
            return Ok(json!({
                "projection_profile": new_projection_profile.map(|p| p.as_str()),
                "adaptive_tournament": adaptive_tournament_raw,
                "ui_theme": self.active_theme().as_str(),
                "persisted_to": persisted_to,
            }));
        }

        let factory = self.inner.config.provider_factory.as_ref().ok_or_else(|| {
            JsonRpcError::new(
                JSONRPC_INTERNAL_ERROR,
                "this surface was started without a provider factory; restart with --model/--protocol"
                    .to_string(),
            )
        })?;
        let requested_model = new_model.clone().unwrap_or_default();
        let effective_protocol = new_protocol.clone().unwrap_or_else(|| "auto".to_string());

        let (swap, provider) = factory
            .build(
                &requested_model,
                &effective_protocol,
                new_reasoning.as_deref(),
            )
            .map_err(|e| JsonRpcError::new(JSONRPC_INVALID_PARAMS, e))?;
        let resolved_protocol = swap.protocol.clone();
        let effective_model = swap.model.clone();
        if effective_model.trim().is_empty() {
            return Err(JsonRpcError::new(
                JSONRPC_INVALID_PARAMS,
                "no model is configured; pass 'model' or set model in ~/.kroma/config.toml"
                    .to_string(),
            ));
        }

        {
            let mut guard = self
                .inner
                .provider
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *guard = provider;
        }
        *self
            .inner
            .active_model
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(effective_model.clone());
        *self
            .inner
            .active_protocol
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(resolved_protocol.clone());
        *self
            .inner
            .active_reasoning_effort
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = new_reasoning.clone();
        *self
            .inner
            .active_projection_profile
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = new_projection_profile;
        if let Some(n) = adaptive_tournament_raw {
            *self
                .inner
                .active_adaptive_tournament
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = n;
        }
        if let Some(theme) = new_theme {
            *self
                .inner
                .active_theme
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = theme;
        }
        {
            let mut guard = self
                .inner
                .resolved_config
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(cfg) = guard.as_mut() {
                cfg.model = Some(effective_model.clone());
                cfg.protocol = Some(resolved_protocol.clone());
                cfg.reasoning_effort = new_reasoning.clone();
                if let Some(name) = &profile {
                    cfg.active_profile = Some(name.clone());
                }
            }
        }

        let mut persisted_to = None;
        if save {
            let mut file = crate::config_file::KromaConfigFile::load_user_global()
                .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;
            if model.is_some() || profile.is_some() {
                file.model = Some(effective_model.clone());
            }
            if protocol.is_some() {
                file.protocol = Some(resolved_protocol.clone());
            }
            if reasoning_raw.is_some() {
                file.reasoning_effort = new_reasoning.clone();
            }
            if let Some(name) = &profile {
                file.default_profile = Some(name.clone());
            }
            if let Some(raw) = &projection_profile_raw {
                file.projection_profile = Some(raw.clone());
            }
            if let Some(n) = adaptive_tournament_raw {
                file.adaptive_tournament = Some(n);
            }
            let path = file
                .save_user_global()
                .map_err(|e| JsonRpcError::new(JSONRPC_INTERNAL_ERROR, e))?;
            persisted_to = Some(path.display().to_string());
        }

        let note = JsonRpcNotification::new(
            "kroma/event",
            json!({
                "schema_version": crate::events::EVENT_SCHEMA_VERSION,
                "event": "config_updated",
                "model": effective_model,
                "protocol": resolved_protocol,
                "reasoning_effort": new_reasoning,
                "persisted_to": persisted_to,
            }),
        );
        Self::broadcast_to_subscribers(&self.inner, note);

        Ok(json!({
            "model": effective_model,
            "protocol": resolved_protocol,
            "reasoning_effort": new_reasoning,
            "profile": profile,
            "persisted_to": persisted_to,
        }))
    }
}
