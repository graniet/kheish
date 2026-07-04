//! Runtime configuration and hook-dispatch methods implemented on [`DaemonState`].

use crate::problems::DaemonProblem;

use super::*;

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    pub(crate) async fn hook_settings_snapshot(&self) -> HookSettings {
        let _guard = self.runtime_config_service.snapshot_guard().await;
        self.hooks.settings()
    }

    pub(crate) async fn hook_dead_letters_snapshot(&self) -> Result<Vec<HookDeadLetterView>> {
        let _guard = self.runtime_config_service.snapshot_guard().await;
        self.hooks.dead_letter_views()
    }

    pub(crate) async fn resolve_hook_dead_letter(
        &self,
        dead_letter_id: &str,
        reason: &str,
    ) -> Result<Option<HookDeadLetterView>> {
        let _guard = self.runtime_config_service.snapshot_guard().await;
        self.hooks.resolve_dead_letter(dead_letter_id, reason)
    }

    pub(crate) async fn learning_policy_settings_snapshot(
        &self,
    ) -> crate::LearningAutomationPolicyConfig {
        let _guard = self.runtime_config_service.snapshot_guard().await;
        self.learning_policy_service.settings()
    }

    pub(crate) fn event_bus(&self) -> DaemonEventBus {
        self.events.clone()
    }

    pub(crate) async fn runtime_settings(&self) -> RuntimeSettingsView {
        let _guard = self.runtime_config_service.snapshot_guard().await;
        self.refresh_mcp_runtime_snapshot().await;
        self.runtime_settings_unlocked()
    }

    pub(crate) fn runtime_settings_unlocked(&self) -> RuntimeSettingsView {
        redacted_runtime_settings(self.runtime_settings_raw_unlocked())
    }

    fn runtime_settings_raw_unlocked(&self) -> RuntimeSettingsView {
        let active_route = self
            .model_control
            .as_ref()
            .and_then(|control| control.resolve_route(None, None).ok());
        RuntimeSettingsView {
            workspace_root: Some(self.workspace_root.display().to_string()),
            state_root: Some(self.state_root.display().to_string()),
            default_route: active_route.clone(),
            route_id: active_route.as_ref().map(|route| route.route_id.clone()),
            provider: active_route.as_ref().map(|route| route.provider.clone()),
            model: self
                .model_control
                .as_ref()
                .map(|control| control.current_model()),
            routes: self
                .model_control
                .as_ref()
                .map(|control| control.available_routes())
                .unwrap_or_default(),
            route_diagnostics: self
                .model_control
                .as_ref()
                .map(|control| control.route_diagnostics())
                .unwrap_or_default(),
            permission_mode: self.permissions.mode(),
            system_prompt: self.system_prompt.settings(),
            hooks: self.hooks.settings(),
            debug_level: self.debug.level(),
            debug_capture: self.run_service.debug_capture_policy_view(),
            mcp: self.mcp.lock().clone(),
            skills: crate::RuntimeSkillsView {
                loaded_count: self.skills.len(),
                roots: self
                    .skills
                    .roots()
                    .iter()
                    .map(|root| root.path.display().to_string())
                    .collect(),
                warnings: self
                    .skills
                    .warnings()
                    .iter()
                    .map(|warning| format!("{}: {}", warning.path.display(), warning.message))
                    .collect(),
            },
            learning_policy: self.learning_policy_service.settings(),
            run_memory_policy: self.run_memory.policy(),
            tool_runtime_limits: self.tools.limits(),
            subagent_policy: self.subagent_policy.clone(),
            scheduler_policy: self.schedule_service.scheduler_policy(),
            config: self.runtime_config_service.metadata(),
        }
    }

    async fn refresh_mcp_runtime_snapshot(&self) {
        let Some(manager) = self.mcp_manager.as_ref() else {
            return;
        };
        let snapshot = manager.runtime_snapshot().await;
        let surface = snapshot.runtime_surface();
        *self.mcp.lock() = snapshot;
        *self.mcp_surface.write() = surface;
    }

    pub(crate) async fn call_mcp_tool(
        &self,
        tool_name: &str,
        input: serde_json::Value,
    ) -> Result<crate::McpToolCallResponse> {
        let tool_name = tool_name.trim();
        if tool_name.is_empty() {
            return Err(DaemonProblem::bad_request(
                "mcp",
                "mcp_tool_name_empty",
                "MCP tool name cannot be empty",
            )
            .into());
        }
        if !input.is_object() {
            return Err(DaemonProblem::bad_request(
                "mcp",
                "mcp_tool_input_not_object",
                "MCP tool input must be a JSON object",
            )
            .into());
        }
        let Some(manager) = self.mcp_manager.as_ref() else {
            return Err(DaemonProblem::conflict(
                "mcp",
                "mcp_not_configured",
                "MCP is not configured for this daemon",
            )
            .into());
        };
        let known_discovered_tool = self
            .mcp
            .lock()
            .servers
            .iter()
            .any(|server| server.tools.iter().any(|candidate| candidate == tool_name));
        if !known_discovered_tool {
            return Err(DaemonProblem::not_found(
                "mcp",
                "mcp_tool_not_found",
                format!("unknown MCP tool `{tool_name}`"),
            )
            .into());
        }
        let output = manager.call_tool(tool_name, input).await;
        self.refresh_mcp_runtime_snapshot().await;
        let output = output.map_err(|_error| {
            DaemonProblem::bad_gateway(
                "mcp",
                "mcp_tool_call_failed",
                format!("MCP tool `{tool_name}` call failed"),
            )
        })?;
        Ok(crate::McpToolCallResponse {
            tool_name: tool_name.to_string(),
            output,
        })
    }

    /// Connects one MCP server while the daemon runs.
    ///
    /// The entry uses the exact Codex-compatible config-file shape; its
    /// secret refs resolve against the live auth store, so a slot stored a
    /// moment earlier through the secrets API works immediately. On success
    /// the entry persists in the state-root overlay and reconnects at boot.
    pub(crate) async fn add_mcp_server(
        &self,
        name: &str,
        entry: kheish_mcp::CodexServerConfig,
    ) -> Result<RuntimeSettingsView> {
        let name = name.trim();
        if name.is_empty()
            || name.len() > 64
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(DaemonProblem::bad_request(
                "mcp",
                "mcp_server_name_invalid",
                "MCP server names use 1-64 ascii alphanumerics, `-`, or `_`",
            )
            .into());
        }
        let Some(manager) = self.mcp_manager.as_ref() else {
            return Err(DaemonProblem::conflict(
                "mcp",
                "mcp_not_configured",
                "MCP is not configured for this daemon",
            )
            .into());
        };
        let _overlay_guard = self.mcp_overlay.lock().await;
        if manager.has_server(name).await {
            return Err(DaemonProblem::conflict(
                "mcp",
                "mcp_server_exists",
                format!("MCP server `{name}` is already configured"),
            )
            .into());
        }
        let resolved_secrets =
            crate::builders::mcp_resolved_secrets_from_auth_store(&self.auth_manager).await?;
        let options = kheish_mcp::CodexCompatOptions {
            config_path: None,
            credentials_path: None,
            resolved_secrets,
        };
        let config =
            kheish_mcp::codex_server_to_config(name.to_string(), entry.clone(), &options, None)?
                .ok_or_else(|| {
                    DaemonProblem::bad_request(
                        "mcp",
                        "mcp_server_config_inert",
                        "entry is disabled, declares no transport, or references a revoked secret",
                    )
                })?;
        manager
            .add_server(
                kheish_mcp::LoadedMcpServerConfig {
                    config,
                    source: kheish_mcp::McpServerSource::RuntimeApi,
                },
                Some(self.auth_manager.clone()),
                &self.tools,
            )
            .await
            .map_err(|error| {
                DaemonProblem::bad_gateway("mcp", "mcp_server_connect_failed", format!("{error:#}"))
            })?;
        let mut entries = self.mcp_overlay.entries();
        entries.insert(name.to_string(), entry);
        if let Err(error) = self.mcp_overlay.save(&entries) {
            // The server must not outlive a failed persist: an unrecorded
            // hot-add would silently vanish at the next boot.
            let _ = manager.remove_server(name, &self.tools).await;
            return Err(error);
        }
        self.refresh_mcp_runtime_snapshot().await;
        let runtime = self.runtime_settings_unlocked();
        self.events.publish(DaemonEvent::RuntimeUpdated {
            runtime: runtime.clone(),
        });
        Ok(runtime)
    }

    /// Disconnects one runtime-added MCP server and retires its tools.
    ///
    /// Servers from the operator's `--mcp-config` file or catalog profiles
    /// are startup-owned and refused here — edit the file and restart.
    pub(crate) async fn remove_mcp_server(&self, name: &str) -> Result<RuntimeSettingsView> {
        let Some(manager) = self.mcp_manager.as_ref() else {
            return Err(DaemonProblem::conflict(
                "mcp",
                "mcp_not_configured",
                "MCP is not configured for this daemon",
            )
            .into());
        };
        let _overlay_guard = self.mcp_overlay.lock().await;
        let mut entries = self.mcp_overlay.entries();
        if !entries.contains_key(name) {
            if manager.has_server(name).await {
                return Err(DaemonProblem::conflict(
                    "mcp",
                    "mcp_server_startup_owned",
                    format!(
                        "MCP server `{name}` comes from startup configuration; edit the config file and restart"
                    ),
                )
                .into());
            }
            return Err(DaemonProblem::not_found(
                "mcp",
                "mcp_server_not_found",
                format!("unknown MCP server `{name}`"),
            )
            .into());
        }
        manager.remove_server(name, &self.tools).await?;
        entries.remove(name);
        self.mcp_overlay.save(&entries)?;
        self.refresh_mcp_runtime_snapshot().await;
        let runtime = self.runtime_settings_unlocked();
        self.events.publish(DaemonEvent::RuntimeUpdated {
            runtime: runtime.clone(),
        });
        Ok(runtime)
    }

    pub(crate) async fn runtime_config_revisions(
        &self,
    ) -> crate::RuntimeConfigRevisionListResponse {
        let _guard = self.runtime_config_service.snapshot_guard().await;
        let mut response = self.runtime_config_service.list_revisions();
        for revision in &mut response.revisions {
            revision.hooks = crate::hooks::redacted_hook_settings(&revision.hooks);
        }
        response
    }

    pub(crate) async fn restore_runtime_learning_policy_from_config(
        &self,
        revision: &crate::RuntimeConfigRevisionView,
    ) -> Result<()> {
        let Some(policy) = revision.learning_policy.clone() else {
            return Ok(());
        };
        if self.learning_policy_service.settings() == policy {
            return Ok(());
        }
        self.learning_policy_service
            .activate_settings(policy)
            .await?;
        Ok(())
    }

    pub(super) async fn dispatch_daemon_hook(
        &self,
        event: HookEventName,
        subject: Option<String>,
        session_id: Option<String>,
        agent_id: Option<String>,
        run_id: Option<String>,
        payload: Value,
    ) -> Result<kheish_types::HookDispatchOutcome> {
        self.hooks
            .dispatch(HookInvocation {
                event,
                subject,
                session_id,
                agent_id,
                run_id,
                payload,
            })
            .await
    }

    pub(super) async fn dispatch_config_change(
        &self,
        setting: &'static str,
        source: &'static str,
        previous: Value,
        current: Value,
    ) -> Result<()> {
        let outcome = self
            .dispatch_daemon_hook(
                HookEventName::ConfigChange,
                Some(source.to_string()),
                None,
                None,
                None,
                json!({
                    "setting": setting,
                    "source": source,
                    "previous": previous,
                    "current": current,
                }),
            )
            .await?;
        if matches!(outcome.decision, Some(kheish_types::HookDecision::Block))
            || !outcome.continue_execution
        {
            let reason = outcome
                .stop_reason
                .unwrap_or_else(|| "hook requested stop".to_string());
            return Err(DaemonProblem::runtime_change_blocked(format!(
                "config change blocked by hook: {setting}: {reason}"
            ))
            .into());
        }
        Ok(())
    }

    fn live_runtime_config_revision(
        &self,
        setting: &str,
        source: &str,
        rollback_of_revision: Option<u64>,
    ) -> Result<crate::RuntimeConfigRevisionView> {
        let control = self
            .model_control
            .as_ref()
            .and_then(|control| control.resolve_route(None, None).ok());
        Ok(crate::RuntimeConfigRevisionView {
            revision: 0,
            updated_at_ms: 0,
            source: source.to_string(),
            setting: setting.to_string(),
            rollback_of_revision,
            route_id: control.as_ref().map(|route| route.route_id.clone()),
            provider: control.as_ref().map(|route| route.provider.clone()),
            model: control.as_ref().map(|route| route.model.clone()),
            permission_mode: self.permissions.mode(),
            system_prompt: self.system_prompt.settings(),
            hooks: self.hooks.settings(),
            debug_level: self.debug.level(),
            learning_policy: Some(self.learning_policy_service.settings()),
            run_memory_policy: self.run_memory.policy(),
            tool_runtime_limits: self.tools.limits(),
        })
    }

    async fn apply_runtime_config_revision(
        &self,
        revision: &crate::RuntimeConfigRevisionView,
    ) -> Result<()> {
        if let Some(control) = self.model_control.as_ref() {
            if let Some(model) = revision.model.clone() {
                let route_id = revision
                    .route_id
                    .as_deref()
                    .or(revision.provider.as_deref());
                control.set_route(route_id, model)?;
            }
        } else if revision.model.is_some() || revision.route_id.is_some() {
            anyhow::bail!("model reconfiguration is not supported by this daemon");
        }
        self.permissions.set_mode(revision.permission_mode.clone());
        self.system_prompt
            .set_settings(revision.system_prompt.clone());
        if self.hooks.settings() != revision.hooks {
            self.hooks.set_settings(revision.hooks.clone())?;
        }
        self.debug.set_level(revision.debug_level);
        if let Some(policy) = revision.learning_policy.clone() {
            if self.learning_policy_service.settings() != policy {
                self.learning_policy_service
                    .activate_settings(policy)
                    .await?;
            }
        }
        self.run_memory
            .set_policy(revision.run_memory_policy.clone())?;
        self.tools
            .set_limits(revision.tool_runtime_limits.clone())?;
        Ok(())
    }

    async fn commit_runtime_config_update<F>(
        &self,
        setting: &'static str,
        expected_revision: Option<u64>,
        skip_hooks: bool,
        update: F,
    ) -> Result<RuntimeSettingsView>
    where
        F: FnOnce(&mut crate::RuntimeConfigRevisionView) -> Result<()>,
    {
        let _guard = self.runtime_config_service.mutation_guard().await;
        self.runtime_config_service
            .require_expected_revision(expected_revision)?;
        let source = if skip_hooks {
            "runtime_api_force"
        } else {
            "runtime_api"
        };
        let previous_revision = self.live_runtime_config_revision(setting, source, None)?;
        let mut next_revision = previous_revision.clone();
        next_revision.revision = 0;
        next_revision.updated_at_ms = 0;
        next_revision.rollback_of_revision = None;
        update(&mut next_revision)?;
        if !skip_hooks {
            self.dispatch_config_change(
                setting,
                "runtime_api",
                serde_json::to_value(&previous_revision)?,
                serde_json::to_value(&next_revision)?,
            )
            .await?;
        }
        let _visibility_guard = self.runtime_config_service.visibility_guard().await;
        if let Err(error) = self.apply_runtime_config_revision(&next_revision).await {
            if let Err(rollback_error) =
                self.apply_runtime_config_revision(&previous_revision).await
            {
                anyhow::bail!(
                    "failed to apply runtime config revision; rollback also failed: {rollback_error}; original error: {error}"
                );
            }
            return Err(error).context("failed to apply runtime config revision");
        }
        if let Err(error) = self.runtime_config_service.append_revision(next_revision) {
            if let Err(rollback_error) =
                self.apply_runtime_config_revision(&previous_revision).await
            {
                anyhow::bail!(
                    "failed to persist runtime config revision; rollback also failed: {rollback_error}; original error: {error}"
                );
            }
            return Err(error);
        }
        let runtime = self.runtime_settings_unlocked();
        self.events.publish(DaemonEvent::RuntimeUpdated {
            runtime: runtime.clone(),
        });
        Ok(runtime)
    }

    pub(crate) async fn rollback_runtime_config(
        &self,
        request: crate::RuntimeRollbackRequest,
    ) -> Result<RuntimeSettingsView> {
        let _guard = self.runtime_config_service.mutation_guard().await;
        self.runtime_config_service
            .require_expected_revision(request.expected_revision)?;
        let target = match request.target_revision {
            Some(revision) => self.runtime_config_service.revision(revision)?,
            None => self.runtime_config_service.previous_revision()?,
        };
        let source = if request.skip_hooks {
            "runtime_api_force"
        } else {
            "runtime_api"
        };
        let previous_revision = self.live_runtime_config_revision("rollback", source, None)?;
        let mut next_revision =
            self.live_runtime_config_revision("rollback", source, Some(target.revision))?;
        next_revision.route_id = target.route_id.clone();
        next_revision.provider = target.provider.clone();
        next_revision.model = target.model.clone();
        next_revision.permission_mode = target.permission_mode.clone();
        next_revision.system_prompt = target.system_prompt.clone();
        next_revision.hooks = target.hooks.clone();
        next_revision.debug_level = target.debug_level;
        self.validate_debug_capture_enablement(next_revision.debug_level)?;
        next_revision.learning_policy = Some(
            target
                .learning_policy
                .clone()
                .unwrap_or_else(|| self.learning_policy_service.settings()),
        );
        next_revision.run_memory_policy = target.run_memory_policy.clone();
        next_revision.tool_runtime_limits = target.tool_runtime_limits.clone();
        if !request.skip_hooks {
            self.dispatch_config_change(
                "rollback",
                "runtime_api",
                serde_json::to_value(&previous_revision)?,
                serde_json::to_value(&next_revision)?,
            )
            .await?;
        }
        let _visibility_guard = self.runtime_config_service.visibility_guard().await;
        if let Err(error) = self.apply_runtime_config_revision(&next_revision).await {
            if let Err(rollback_error) =
                self.apply_runtime_config_revision(&previous_revision).await
            {
                anyhow::bail!(
                    "failed to apply runtime config rollback; rollback also failed: {rollback_error}; original error: {error}"
                );
            }
            return Err(error).context("failed to apply runtime config rollback");
        }
        if let Err(error) = self.runtime_config_service.append_revision(next_revision) {
            if let Err(rollback_error) =
                self.apply_runtime_config_revision(&previous_revision).await
            {
                anyhow::bail!(
                    "failed to persist runtime config rollback; rollback also failed: {rollback_error}; original error: {error}"
                );
            }
            return Err(error);
        }
        let runtime = self.runtime_settings_unlocked();
        self.events.publish(DaemonEvent::RuntimeUpdated {
            runtime: runtime.clone(),
        });
        self.enforce_run_memory_policy_after_runtime_config_commit(
            "rollback",
            &runtime.run_memory_policy,
        )
        .await;
        Ok(runtime)
    }

    pub(crate) async fn set_model(
        &self,
        provider: Option<String>,
        model: String,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeSettingsView> {
        self.commit_runtime_config_update("model", expected_revision, false, |next_revision| {
            let control = self
                .model_control
                .as_ref()
                .ok_or_else(|| anyhow!("model reconfiguration is not supported by this daemon"))?;
            let resolved = control.resolve_route(provider.as_deref(), Some(&model))?;
            next_revision.route_id = Some(resolved.route_id);
            next_revision.provider = Some(resolved.provider);
            next_revision.model = Some(resolved.model);
            Ok(())
        })
        .await
    }

    pub(crate) async fn set_permission_mode(
        &self,
        mode: PermissionMode,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeSettingsView> {
        self.commit_runtime_config_update(
            "permission_mode",
            expected_revision,
            false,
            |next_revision| {
                next_revision.permission_mode = mode;
                Ok(())
            },
        )
        .await
    }

    pub(crate) async fn check_permission(
        &self,
        request: CheckPermissionRequest,
    ) -> Result<PermissionExplanation> {
        let CheckPermissionRequest {
            tool_name,
            tool_call_id,
            session_id,
            mode_override,
            input,
        } = request;
        let tool_name = tool_name.trim();
        anyhow::ensure!(!tool_name.is_empty(), "tool_name is required");
        let call = ToolCallRecord {
            id: tool_call_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("dry-run")
                .to_string(),
            name: tool_name.to_string(),
            input,
            assistant_message_id: None,
            assistant_provider_response_id: None,
        };
        let evaluate = || {
            self.permissions
                .explain_with_mode(&call, mode_override.clone())
        };
        if let Some(session_id) = session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            self.agent_id_for_session(session_id).await?;
            let scope = ExecutionScope {
                session_id: session_id.to_string(),
                ..ExecutionScope::default()
            };
            return Ok(
                scope_execution(scope, CancellationToken::new(), async { evaluate() }).await,
            );
        }
        Ok(evaluate())
    }

    pub(crate) async fn check_permission_matrix(
        &self,
        request: CheckPermissionMatrixRequest,
    ) -> Result<PermissionMatrixView> {
        let session_id = request
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        if let Some(session_id) = session_id.as_deref() {
            self.agent_id_for_session(session_id).await?;
        }

        let mut tool_names = self
            .tools
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>();
        tool_names.sort();
        tool_names.dedup();
        let modes = [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::BypassPermissions,
            PermissionMode::Plan,
            PermissionMode::DontAsk,
        ];

        let evaluate = || {
            modes
                .into_iter()
                .map(|mode| {
                    let tools = tool_names
                        .iter()
                        .map(|tool_name| {
                            let call = ToolCallRecord {
                                id: format!("dry-run-{mode:?}-{tool_name}"),
                                name: tool_name.clone(),
                                input: Value::Object(Default::default()),
                                assistant_message_id: None,
                                assistant_provider_response_id: None,
                            };
                            self.permissions
                                .explain_with_mode(&call, Some(mode.clone()))
                        })
                        .collect::<Vec<_>>();
                    PermissionMatrixModeView { mode, tools }
                })
                .collect::<Vec<_>>()
        };

        let modes = if let Some(session_id) = session_id.as_deref() {
            let scope = ExecutionScope {
                session_id: session_id.to_string(),
                ..ExecutionScope::default()
            };
            scope_execution(scope, CancellationToken::new(), async { evaluate() }).await
        } else {
            evaluate()
        };

        Ok(PermissionMatrixView { session_id, modes })
    }

    pub(crate) async fn set_system_prompt(
        &self,
        settings: SystemPromptSettings,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeSettingsView> {
        self.commit_runtime_config_update(
            "system_prompt",
            expected_revision,
            false,
            |next_revision| {
                next_revision.system_prompt = settings;
                Ok(())
            },
        )
        .await
    }

    pub(crate) async fn set_hooks(
        &self,
        settings: HookSettings,
        expected_revision: Option<u64>,
        skip_hooks: bool,
    ) -> Result<RuntimeSettingsView> {
        crate::hooks::validate_hook_settings(&settings)?;
        self.commit_runtime_config_update("hooks", expected_revision, skip_hooks, |next_revision| {
            next_revision.hooks = settings;
            Ok(())
        })
        .await
    }

    pub(crate) async fn set_debug_level(
        &self,
        level: DebugCaptureLevel,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeSettingsView> {
        self.validate_debug_capture_enablement(level)?;
        self.commit_runtime_config_update(
            "debug_level",
            expected_revision,
            false,
            |next_revision| {
                next_revision.debug_level = level;
                Ok(())
            },
        )
        .await
    }

    pub(crate) async fn set_learning_policy(
        &self,
        settings: crate::LearningAutomationPolicyConfig,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeSettingsView> {
        self.learning_policy_service
            .validate_settings(&settings)
            .map_err(|error| DaemonProblem::invalid_learning_policy(error.to_string()))?;
        let runtime = self
            .commit_runtime_config_update(
                "learning_policy",
                expected_revision,
                false,
                |next_revision| {
                    next_revision.learning_policy = Some(settings);
                    Ok(())
                },
            )
            .await?;
        for candidate_id in self.learning_service.pending_candidate_ids().await {
            self.learning_policy_service
                .enqueue_candidate(candidate_id)
                .await;
        }
        Ok(runtime)
    }

    pub(crate) async fn set_run_memory_policy(
        &self,
        settings: RunMemoryPolicyConfig,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeSettingsView> {
        settings
            .validate()
            .map_err(|error| DaemonProblem::invalid_run_memory_policy(error.to_string()))?;
        let runtime = self
            .commit_runtime_config_update(
                "run_memory_policy",
                expected_revision,
                false,
                |next_revision| {
                    next_revision.run_memory_policy = settings;
                    Ok(())
                },
            )
            .await?;
        self.enforce_run_memory_policy_after_runtime_config_commit(
            "run_memory_policy",
            &runtime.run_memory_policy,
        )
        .await;
        Ok(runtime)
    }

    pub(crate) async fn set_tool_runtime_limits(
        &self,
        limits: kheish_runtime::ToolRuntimeLimits,
        expected_revision: Option<u64>,
    ) -> Result<RuntimeSettingsView> {
        limits
            .validate()
            .map_err(|error| DaemonProblem::invalid_tool_runtime_limits(error.to_string()))?;
        self.commit_runtime_config_update(
            "tool_runtime_limits",
            expected_revision,
            false,
            |next_revision| {
                next_revision.tool_runtime_limits = limits;
                Ok(())
            },
        )
        .await
    }

    async fn enforce_run_memory_policy_after_runtime_config_commit(
        &self,
        source: &'static str,
        policy: &RunMemoryPolicyConfig,
    ) {
        if let Err(error) = self
            .enforce_run_memory_policy_on_persisted_records(policy)
            .await
        {
            warn!(
                source,
                error = ?error,
                "runtime config committed but run-memory maintenance failed"
            );
        }
    }

    async fn enforce_run_memory_policy_on_persisted_records(
        &self,
        policy: &RunMemoryPolicyConfig,
    ) -> Result<()> {
        let runs = self.run_service.run_records_snapshot().await;
        let checked_at_ms = now_ms();
        let rebuilt = match crate::memory::rebuild_run_memory_index_with_policy(
            &runs,
            self.run_service.run_memory_store(),
            checked_at_ms,
            policy,
        ) {
            Ok(rebuilt) => rebuilt,
            Err(error) => {
                self.run_memory.record_maintenance(
                    crate::RunMemoryMaintenanceStatusView::scan_error(
                        "runtime_policy",
                        checked_at_ms,
                        error.to_string(),
                    ),
                );
                return Err(error);
            }
        };
        let mut maintenance = crate::RunMemoryMaintenanceStatusView::from_rebuild(
            "runtime_policy",
            checked_at_ms,
            true,
            &rebuilt,
        );
        for run_id in &rebuilt.pruned_run_ids {
            if let Err(error) = self
                .run_service
                .run_memory_store()
                .delete_run_memory(run_id)
            {
                maintenance.record_prune_error(
                    "delete_run_memory",
                    "delete_failed",
                    Some(run_id.clone()),
                    None,
                    error.to_string(),
                );
                self.run_memory.record_maintenance(maintenance);
                return Err(error);
            }
        }
        for path in &rebuilt.pruned_orphan_files {
            if let Err(error) = self
                .run_service
                .run_memory_store()
                .delete_run_memory_file(path)
            {
                maintenance.record_prune_error(
                    "delete_run_memory_file",
                    "delete_failed",
                    None,
                    Some(path.display().to_string()),
                    error.to_string(),
                );
                self.run_memory.record_maintenance(maintenance);
                return Err(error);
            }
        }
        self.run_memory
            .record_pruned_ttl(rebuilt.pruned_ttl_run_ids.len());
        self.run_memory
            .record_pruned_overflow(rebuilt.pruned_overflow_run_ids.len());
        self.run_memory
            .record_pruned_orphan(rebuilt.pruned_orphan_files.len());
        if let Err(error) = self
            .session_service
            .replace_run_memory_index(rebuilt.index)
            .await
        {
            maintenance.record_prune_error(
                "replace_run_memory_index",
                "index_replace_failed",
                None,
                None,
                error.to_string(),
            );
            self.run_memory.record_maintenance(maintenance);
            return Err(error);
        }
        self.run_memory.record_maintenance(maintenance);
        Ok(())
    }
    fn validate_debug_capture_enablement(&self, level: DebugCaptureLevel) -> Result<()> {
        if !level.is_enabled() {
            return Ok(());
        }
        if let Some(error) = self.run_service.debug_store_encryption_key_error() {
            return Err(
                DaemonProblem::bad_request("runtime", "debug_capture_key_invalid", error).into(),
            );
        }
        if level.captures_redacted_content()
            && let Some(error) = kheish_runtime::debug_redaction_config_error()
        {
            return Err(DaemonProblem::bad_request(
                "runtime",
                "debug_redaction_config_invalid",
                error,
            )
            .into());
        }
        Ok(())
    }
}

fn redacted_runtime_settings(mut runtime: RuntimeSettingsView) -> RuntimeSettingsView {
    runtime.hooks = crate::hooks::redacted_hook_settings(&runtime.hooks);
    runtime
}
