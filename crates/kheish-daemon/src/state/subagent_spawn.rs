//! Sidechain spawning methods implemented on [`DaemonState`].

use super::*;
use crate::api::{validate_input_attachment_requests, validate_submit_input_items};
use kheish_session::safe_storage_name;
use kheish_types::{CapabilityScope, CredentialScope, allow_list_allows_entry};
use sha2::{Digest, Sha256};

const INTERNAL_SIDECHAIN_SPAWN_RECEIPT_KEY_PREFIX: &str = "__kheish_internal_sidechain_spawn__";
const AGENT_WORKTREE_DIR: &str = ".kheish-agent-worktrees";
const GIT_WORKTREE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
struct SidechainSpawnReceiptKeyParts {
    parent: AgentId,
    spawn_request_id: Option<String>,
    internal: bool,
}

fn ensure_requested_allowlist_subset(
    label: &str,
    parent: &[String],
    requested: &[String],
) -> Result<()> {
    if parent.is_empty() || requested.is_empty() {
        return Ok(());
    }
    let disallowed = requested
        .iter()
        .filter(|entry| !allow_list_allows_entry(parent, entry))
        .cloned()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        disallowed.is_empty(),
        "{label} contains entries outside the parent scope: {}",
        disallowed.join(", ")
    );
    Ok(())
}

fn validate_requested_capability_scope(
    parent: &kheish_types::CapabilityScope,
    requested: &kheish_types::CapabilityScope,
) -> Result<()> {
    ensure_requested_allowlist_subset("skill_allow", &parent.skill_allow, &requested.skill_allow)?;
    ensure_requested_allowlist_subset(
        "mcp_server_allow",
        &parent.mcp_server_allow,
        &requested.mcp_server_allow,
    )?;
    ensure_requested_allowlist_subset(
        "mcp_tool_allow",
        &parent.mcp_tool_allow,
        &requested.mcp_tool_allow,
    )?;
    Ok(())
}

fn validate_requested_credential_scope(
    parent: &kheish_types::CredentialScope,
    requested: &kheish_types::CredentialScope,
) -> Result<()> {
    ensure_requested_allowlist_subset("route_allow", &parent.route_allow, &requested.route_allow)?;
    ensure_requested_allowlist_subset(
        "connector_allow",
        &parent.connector_allow,
        &requested.connector_allow,
    )?;
    if parent.connector_credential_allow.is_empty()
        && parent.connector_credential_deny.is_empty()
        && (!parent.connector_allow.is_empty() || !parent.connector_deny.is_empty())
    {
        anyhow::ensure!(
            requested.connector_credential_allow.is_empty(),
            "connector_credential_allow contains entries outside the parent scope: {}",
            requested.connector_credential_allow.join(", ")
        );
    } else {
        ensure_requested_allowlist_subset(
            "connector_credential_allow",
            &parent.connector_credential_allow,
            &requested.connector_credential_allow,
        )?;
    }
    ensure_requested_allowlist_subset(
        "mcp_server_allow",
        &parent.mcp_server_allow,
        &requested.mcp_server_allow,
    )?;
    Ok(())
}

fn validate_requested_tool_surface(
    parent: &kheish_types::ToolSurfaceFilter,
    requested: &kheish_types::ToolSurfaceFilter,
) -> Result<()> {
    if parent.allowlist.is_empty() || requested.allowlist.is_empty() {
        return Ok(());
    }
    let parent_has_dynamic_mcp = parent
        .allowlist
        .iter()
        .any(|entry| entry == kheish_types::DYNAMIC_MCP_TOOL_ALLOWLIST_SENTINEL);
    let disallowed = requested
        .allowlist
        .iter()
        .filter(|entry| {
            !parent.allowlist.iter().any(|candidate| candidate == *entry)
                && !(parent_has_dynamic_mcp && kheish_types::is_dynamic_mcp_tool_entry(entry))
        })
        .cloned()
        .collect::<Vec<_>>();
    anyhow::ensure!(
        disallowed.is_empty(),
        "allowed_tools contains entries outside the parent scope: {}",
        disallowed.join(", ")
    );
    Ok(())
}

fn child_permission_mode_is_not_wider(parent: &PermissionMode, child: &PermissionMode) -> bool {
    use PermissionMode::*;

    match parent {
        BypassPermissions => true,
        AcceptEdits => matches!(child, AcceptEdits | Default | DontAsk | Plan),
        Default => matches!(child, Default | DontAsk | Plan),
        DontAsk => matches!(child, DontAsk | Plan),
        Plan => matches!(child, Plan),
    }
}

fn permission_mode_name(mode: &PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::BypassPermissions => "bypassPermissions",
        PermissionMode::Plan => "plan",
        PermissionMode::DontAsk => "dontAsk",
    }
}

fn resolve_sidechain_permission_mode(
    parent_mode: &PermissionMode,
    requested_mode: Option<Option<PermissionMode>>,
) -> Result<Option<Option<PermissionMode>>> {
    let effective_child_mode = match requested_mode.as_ref() {
        Some(Some(mode)) => mode.clone(),
        Some(None) => PermissionMode::Default,
        None => parent_mode.clone(),
    };

    anyhow::ensure!(
        child_permission_mode_is_not_wider(parent_mode, &effective_child_mode),
        "sidechain permission mode {} would widen parent mode {}",
        permission_mode_name(&effective_child_mode),
        permission_mode_name(parent_mode),
    );

    Ok(match requested_mode {
        Some(Some(mode)) => Some(Some(mode)),
        Some(None) => Some(Some(PermissionMode::Default)),
        None => Some(Some(effective_child_mode)),
    })
}

fn summarize_inline_asset_upload(upload: &crate::InlineAssetUpload) -> Value {
    let mut value = serde_json::Map::new();
    value.insert(
        "type".to_string(),
        Value::String("inline_asset".to_string()),
    );
    value.insert(
        "file_name".to_string(),
        Value::String(upload.file_name.clone()),
    );
    if let Some(media_type) = upload.media_type.as_ref() {
        value.insert("media_type".to_string(), Value::String(media_type.clone()));
    }
    Value::Object(value)
}

#[cfg(test)]
mod permission_mode_tests {
    use super::*;

    #[test]
    fn sidechain_permission_modes_enforce_non_widening_matrix() {
        let cases = [
            (
                PermissionMode::Default,
                vec![
                    (PermissionMode::Default, true),
                    (PermissionMode::AcceptEdits, false),
                    (PermissionMode::BypassPermissions, false),
                    (PermissionMode::DontAsk, true),
                    (PermissionMode::Plan, true),
                ],
            ),
            (
                PermissionMode::AcceptEdits,
                vec![
                    (PermissionMode::Default, true),
                    (PermissionMode::AcceptEdits, true),
                    (PermissionMode::BypassPermissions, false),
                    (PermissionMode::DontAsk, true),
                    (PermissionMode::Plan, true),
                ],
            ),
            (
                PermissionMode::BypassPermissions,
                vec![
                    (PermissionMode::Default, true),
                    (PermissionMode::AcceptEdits, true),
                    (PermissionMode::BypassPermissions, true),
                    (PermissionMode::DontAsk, true),
                    (PermissionMode::Plan, true),
                ],
            ),
            (
                PermissionMode::DontAsk,
                vec![
                    (PermissionMode::Default, false),
                    (PermissionMode::AcceptEdits, false),
                    (PermissionMode::BypassPermissions, false),
                    (PermissionMode::DontAsk, true),
                    (PermissionMode::Plan, true),
                ],
            ),
            (
                PermissionMode::Plan,
                vec![
                    (PermissionMode::Default, false),
                    (PermissionMode::AcceptEdits, false),
                    (PermissionMode::BypassPermissions, false),
                    (PermissionMode::DontAsk, false),
                    (PermissionMode::Plan, true),
                ],
            ),
        ];

        for (parent, children) in cases {
            for (child, should_allow) in children {
                let result = resolve_sidechain_permission_mode(&parent, Some(Some(child.clone())));
                assert_eq!(
                    result.is_ok(),
                    should_allow,
                    "parent={parent:?} child={child:?}"
                );
            }
        }
    }

    #[test]
    fn sidechain_permission_modes_pin_omitted_child_mode_to_parent_effective_mode() {
        for parent in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::BypassPermissions,
            PermissionMode::DontAsk,
            PermissionMode::Plan,
        ] {
            assert_eq!(
                resolve_sidechain_permission_mode(&parent, None).expect("omitted mode inherits"),
                Some(Some(parent.clone())),
                "parent={parent:?}"
            );
        }
    }

    #[test]
    fn sidechain_permission_modes_treat_explicit_default_as_pinned_default() {
        assert_eq!(
            resolve_sidechain_permission_mode(&PermissionMode::AcceptEdits, Some(None))
                .expect("default narrows acceptEdits"),
            Some(Some(PermissionMode::Default))
        );
        assert_eq!(
            resolve_sidechain_permission_mode(&PermissionMode::BypassPermissions, Some(None))
                .expect("default narrows bypassPermissions"),
            Some(Some(PermissionMode::Default))
        );
        assert!(
            resolve_sidechain_permission_mode(&PermissionMode::DontAsk, Some(None)).is_err(),
            "default would escape dontAsk"
        );
        assert!(
            resolve_sidechain_permission_mode(&PermissionMode::Plan, Some(None)).is_err(),
            "default would escape plan mode"
        );
    }
}

fn summarize_attachment_request(attachment: &crate::InputAttachmentRequest) -> Value {
    match attachment {
        crate::InputAttachmentRequest::AssetReference { asset_id } => json!({
            "type": "asset_reference",
            "asset_id": asset_id,
        }),
        crate::InputAttachmentRequest::InlineAsset(upload) => summarize_inline_asset_upload(upload),
    }
}

fn summarize_submit_input_item(item: &crate::SubmitInputItemRequest) -> Value {
    match item {
        crate::SubmitInputItemRequest::Text { text } => json!({
            "type": "text",
            "text": text,
        }),
        crate::SubmitInputItemRequest::AssetReference { asset_id } => json!({
            "type": "asset_reference",
            "asset_id": asset_id,
        }),
        crate::SubmitInputItemRequest::BoardReference {
            board_id,
            revision_id,
        } => json!({
            "type": "board_reference",
            "board_id": board_id,
            "revision_id": revision_id,
        }),
        crate::SubmitInputItemRequest::InlineAsset(upload) => summarize_inline_asset_upload(upload),
    }
}

fn summarize_subtask_input_items(subtask: &crate::SidechainSubtaskRequest) -> Vec<Value> {
    if !subtask.input_items.is_empty() {
        return subtask
            .input_items
            .iter()
            .map(summarize_submit_input_item)
            .collect();
    }

    let mut items = Vec::with_capacity(subtask.attachments.len().saturating_add(1));
    if !subtask.content.trim().is_empty() {
        items.push(json!({
            "type": "text",
            "text": subtask.content,
        }));
    }
    items.extend(subtask.attachments.iter().map(summarize_attachment_request));
    items
}

/// Releases a reserved sidechain spawn slot when the spawn attempt exits early.
struct SpawnReservationGuard<'a, M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    daemon: &'a DaemonState<M>,
    reservation: Option<SpawnReservation>,
}

impl<'a, M> SpawnReservationGuard<'a, M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    fn new(daemon: &'a DaemonState<M>, reservation: SpawnReservation) -> Self {
        Self {
            daemon,
            reservation: Some(reservation),
        }
    }
}

impl<M> Drop for SpawnReservationGuard<'_, M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            self.daemon.release_subagent_spawn_reservation(reservation);
        }
    }
}

/// Releases a reserved spawn request id when the spawn attempt exits.
struct SpawnRequestReservationGuard<'a, M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    daemon: &'a DaemonState<M>,
    reservation: Option<SpawnRequestReservation>,
}

impl<'a, M> SpawnRequestReservationGuard<'a, M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    fn new(daemon: &'a DaemonState<M>, reservation: SpawnRequestReservation) -> Self {
        Self {
            daemon,
            reservation: Some(reservation),
        }
    }
}

impl<M> Drop for SpawnRequestReservationGuard<'_, M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            self.daemon
                .release_subagent_spawn_request_reservation(reservation);
        }
    }
}

impl<M> DaemonState<M>
where
    M: kheish_core::ModelDriver + Send + Sync + 'static,
{
    fn sidechain_execution_identity(
        parent: &AgentId,
        child: &AgentId,
        spawn_request_id: Option<&str>,
        spawned_by_run_id: Option<&str>,
    ) -> kheish_types::SessionExecutionIdentity {
        let parent_principal_id = kheish_runtime::current_execution_scope()
            .and_then(|scope| scope.principal_id)
            .unwrap_or_else(|| format!("agent:{}", parent.0));
        let delegation_id = spawn_request_id
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .or_else(|| {
                spawned_by_run_id
                    .filter(|value| !value.trim().is_empty())
                    .map(|run_id| format!("{}:{run_id}", parent.0))
            });
        kheish_types::SessionExecutionIdentity {
            principal_id: Some(format!("agent:{}", child.0)),
            parent_principal_id: Some(parent_principal_id),
            delegation_id,
        }
    }

    fn reusable_sidechain_record(
        &self,
        parent: &AgentId,
        conversation: &ConversationKey,
    ) -> Result<Option<kheish_agent::AgentRecord>> {
        let matches = self
            .supervisor
            .list()
            .into_iter()
            .filter(|record| {
                record.parent.as_ref() == Some(parent)
                    && record.closed_at_ms.is_none()
                    && record.conversation.session_id == conversation.session_id
                    && record.conversation.thread_id == conversation.thread_id
            })
            .collect::<Vec<_>>();
        anyhow::ensure!(
            matches.len() <= 1,
            "multiple open sidechain sessions match parent {} and session {}",
            parent.0,
            conversation.session_id
        );
        Ok(matches.into_iter().next())
    }

    fn sidechain_spawn_receipt_key(
        parent: &AgentId,
        spawn_request_id: Option<&str>,
    ) -> Option<String> {
        spawn_request_id.map(|spawn_request_id| format!("{}:{spawn_request_id}", parent.0))
    }

    fn internal_sidechain_spawn_receipt_key(
        parent: &AgentId,
        conversation: &ConversationKey,
    ) -> String {
        let digest = Sha256::digest(
            serde_json::to_vec(&json!({
                "parent_agent_id": parent.0,
                "session_id": conversation.session_id,
                "thread_id": conversation.thread_id,
            }))
            .expect("sidechain receipt key payload should serialize")
            .as_slice(),
        );
        format!(
            "{INTERNAL_SIDECHAIN_SPAWN_RECEIPT_KEY_PREFIX}:{}:{}",
            parent.0,
            hex::encode(digest)
        )
    }

    fn parse_sidechain_spawn_receipt_key(
        receipt_key: &str,
    ) -> Option<SidechainSpawnReceiptKeyParts> {
        if let Some(rest) = receipt_key
            .strip_prefix(INTERNAL_SIDECHAIN_SPAWN_RECEIPT_KEY_PREFIX)
            .and_then(|rest| rest.strip_prefix(':'))
        {
            let (parent_agent_id, digest) = rest.split_once(':')?;
            if parent_agent_id.is_empty() || digest.is_empty() {
                return None;
            }
            return Some(SidechainSpawnReceiptKeyParts {
                parent: AgentId(parent_agent_id.to_string()),
                spawn_request_id: None,
                internal: true,
            });
        }

        let (parent_agent_id, spawn_request_id) = receipt_key.split_once(':')?;
        if parent_agent_id.is_empty() || spawn_request_id.is_empty() {
            return None;
        }
        Some(SidechainSpawnReceiptKeyParts {
            parent: AgentId(parent_agent_id.to_string()),
            spawn_request_id: Some(spawn_request_id.to_string()),
            internal: false,
        })
    }

    fn sidechain_record_matches_receipt(
        record: &kheish_agent::AgentRecord,
        parent: &AgentId,
        spawn_request_id: Option<&str>,
        session_id: &str,
        thread_id: Option<&str>,
        require_open: bool,
    ) -> bool {
        record.parent.as_ref() == Some(parent)
            && (!require_open || record.closed_at_ms.is_none())
            && record.spawn_request_id.as_deref() == spawn_request_id
            && record.conversation.session_id == session_id
            && record.conversation.thread_id.as_deref() == thread_id
    }

    fn clear_session_permission_memory(&self, session_id: &str) {
        self.permissions
            .set_session_mode(session_id.to_string(), None);
        self.permissions
            .replace_session_rule_updates(session_id.to_string(), &[]);
    }

    fn sidechain_spawn_request_fingerprint(
        request: &SpawnSidechainRequest,
        conversation: &ConversationKey,
    ) -> Result<String> {
        Ok(serde_json::to_string(&json!({
            "session_id": conversation.session_id,
            "thread_id": conversation.thread_id,
            "route_policy": request.route_policy,
            "provider": request.provider,
            "permission_mode": request.permission_mode,
            "retention": request.retention,
            "nickname": request.nickname,
            "capability_scope": request.capability_scope,
            "credential_scope": request.credential_scope,
            "fork_context": request.fork_context,
            "subtask": request.subtask,
        }))?)
    }

    async fn sidechain_spawn_policy_scope(
        &self,
        parent: &AgentId,
        parent_session_id: &str,
        profile: Option<String>,
    ) -> Result<crate::SubagentPolicyScopeView> {
        let root = self
            .supervisor
            .root_of(parent)
            .ok_or_else(|| anyhow!("unknown agent {}", parent.0))?;
        let project_ids = self
            .project_service
            .project_ids_for_session(parent_session_id)
            .await;
        Ok(crate::SubagentPolicyScopeView {
            parent_agent_id: parent.0.clone(),
            root_agent_id: root.id.0,
            session_id: parent_session_id.to_string(),
            profile,
            project_ids,
        })
    }

    fn sidechain_spawn_policy_estimate(
        request: &SpawnSidechainRequest,
        limits: &crate::SubagentPolicyLimits,
    ) -> crate::SubagentPolicyEstimateView {
        let mut serialized_size = 0usize;
        if let Some(subtask) = request.subtask.as_ref() {
            serialized_size += subtask.name.len();
            serialized_size += subtask.description.len();
            serialized_size += subtask.content.len();
            serialized_size += serde_json::to_string(&subtask.input_items)
                .map(|value| value.len())
                .unwrap_or(0);
            serialized_size += serde_json::to_string(&subtask.attachments)
                .map(|value| value.len())
                .unwrap_or(0);
        }
        serialized_size += request.fork_context.parent_assistant_message.len();
        serialized_size += request.fork_context.system_prompt.len();
        let input_tokens = ((serialized_size as u64).saturating_add(3) / 4).max(1);
        let output_tokens = request
            .fork_context
            .generation
            .as_ref()
            .and_then(|generation| generation.max_output_tokens)
            .unwrap_or(
                kheish_types::model_max_output_tokens(
                    request.provider.as_deref().unwrap_or("unknown-model"),
                )
                .default,
            ) as u64;
        crate::SubagentPolicyEstimateView {
            input_tokens,
            output_tokens,
            cost_microusd: limits.estimated_spawn_cost_microusd,
            cpu_ms: limits.estimated_spawn_cpu_ms,
        }
    }

    fn normalize_sidechain_legacy_provider(request: &mut SpawnSidechainRequest) -> Result<()> {
        if let (Some(provider), Some(fork_provider)) = (
            request.provider.as_deref(),
            request.fork_context.provider.as_deref(),
        ) {
            anyhow::ensure!(
                provider == fork_provider,
                "provider conflicts with fork_context.provider"
            );
        }
        if request.provider.is_none() {
            request.provider = request.fork_context.provider.clone();
        }
        Ok(())
    }

    fn ensure_reusable_sidechain_matches(
        request: &SpawnSidechainRequest,
        record: &kheish_agent::AgentRecord,
    ) -> Result<()> {
        let existing = record
            .fork_context
            .as_ref()
            .ok_or_else(|| anyhow!("existing sidechain has no fork context"))?;
        anyhow::ensure!(
            existing.provider == request.provider
                && existing.generation == request.fork_context.generation
                && existing.tool_surface == request.fork_context.tool_surface
                && existing.worktree_path == request.fork_context.worktree_path
                && existing.system_prompt == request.fork_context.system_prompt
                && existing.prompt_merge_mode == request.fork_context.prompt_merge_mode
                && existing.team_name == request.fork_context.team_name
                && existing.isolation == request.fork_context.isolation
                && existing.parent_assistant_message
                    == request.fork_context.parent_assistant_message
                && existing.inherited_tool_call_ids == request.fork_context.inherited_tool_call_ids,
            "existing sidechain session was created with a different route or fork context"
        );
        anyhow::ensure!(
            request.subtask.is_none(),
            "existing sidechain session cannot be reused for a new subtask without spawn_request_id"
        );
        Ok(())
    }

    async fn ensure_implicit_sidechain_reuse_is_read_only(
        self: &Arc<Self>,
        request: &SpawnSidechainRequest,
        record: &kheish_agent::AgentRecord,
        explicit_permission_mode_requested: bool,
        explicit_capability_scope_requested: bool,
        explicit_credential_scope_requested: bool,
        explicit_retention_requested: bool,
        explicit_nickname_requested: bool,
        explicit_spawned_by_run_id_requested: bool,
    ) -> Result<()> {
        let mut mutable_fields = Vec::new();
        if request.subtask.is_some() {
            mutable_fields.push("subtask");
        }
        if explicit_spawned_by_run_id_requested
            && request.spawned_by_run_id != record.spawned_by_run_id
        {
            mutable_fields.push("spawned_by_run_id");
        }
        if explicit_retention_requested
            && request
                .retention
                .as_ref()
                .unwrap_or(&ChildRetentionPolicy::Retain)
                != &record.retention
        {
            mutable_fields.push("retention");
        }
        if explicit_nickname_requested && request.nickname != record.nickname {
            mutable_fields.push("nickname");
        }
        if explicit_permission_mode_requested {
            let requested = request.permission_mode.as_deref();
            let existing = self
                .load_session_control_state(&record.conversation.session_id)
                .await?
                .session_permission_mode;
            if existing.as_deref() != requested {
                mutable_fields.push("permission_mode");
            }
        }
        if explicit_capability_scope_requested {
            let existing = self
                .load_session_capability_scope(&record.conversation.session_id)
                .await?
                .normalized();
            let requested = request
                .capability_scope
                .clone()
                .unwrap_or_default()
                .normalized();
            if existing != requested {
                mutable_fields.push("capability_scope");
            }
        }
        if explicit_credential_scope_requested {
            let existing = self
                .load_session_credential_scope(&record.conversation.session_id)
                .await?
                .normalized();
            let requested = request
                .credential_scope
                .clone()
                .unwrap_or_default()
                .normalized();
            if existing != requested {
                mutable_fields.push("credential_scope");
            }
        }
        anyhow::ensure!(
            mutable_fields.is_empty(),
            "existing sidechain session requires spawn_request_id for mutable reuse: {}",
            mutable_fields.join(", ")
        );
        Ok(())
    }

    fn sidechain_spawn_conversation_key(conversation: &ConversationKey) -> Result<String> {
        Ok(serde_json::to_string(&(
            conversation.session_id.as_str(),
            conversation.thread_id.as_deref(),
        ))?)
    }

    fn ensure_sidechain_spawn_binding(
        requested_session_id: Option<&str>,
        requested_thread_id: Option<&str>,
        request_fingerprint: &str,
        bound_session_id: &str,
        bound_thread_id: Option<&str>,
        bound_request_fingerprint: &str,
    ) -> Result<()> {
        if let Some(requested_session_id) = requested_session_id {
            anyhow::ensure!(
                requested_session_id == bound_session_id,
                "spawn_request_id is already bound to session {bound_session_id}"
            );
        }
        anyhow::ensure!(
            requested_thread_id == bound_thread_id,
            "spawn_request_id is already bound to thread {}",
            bound_thread_id.unwrap_or("<none>")
        );
        anyhow::ensure!(
            request_fingerprint == bound_request_fingerprint,
            "spawn_request_id is already bound to a different request payload"
        );
        Ok(())
    }

    fn combined_spawn_failure(
        error: &anyhow::Error,
        cleanup_error: anyhow::Error,
    ) -> anyhow::Error {
        anyhow!("sidechain spawn failed: {error}; rollback cleanup also failed: {cleanup_error}")
    }

    fn canonical_workspace_root(&self) -> Result<PathBuf> {
        std::fs::canonicalize(&self.workspace_root).with_context(|| {
            format!(
                "failed to resolve workspace root {}",
                self.workspace_root.display()
            )
        })
    }

    fn daemon_worktree_root(&self) -> Result<PathBuf> {
        Ok(self.canonical_workspace_root()?.join(AGENT_WORKTREE_DIR))
    }

    fn daemon_worktree_path(&self, parent: &AgentId, spawn_receipt_key: &str) -> Result<PathBuf> {
        Ok(self
            .daemon_worktree_root()?
            .join(safe_storage_name(&parent.0))
            .join(safe_storage_name(spawn_receipt_key)))
    }

    fn reserved_daemon_worktree_path(&self, path: &Path) -> Result<PathBuf> {
        let workspace_root = self.canonical_workspace_root()?;
        let resolved = bounded_workspace_root(&workspace_root, path)?;
        let root = workspace_root.join(AGENT_WORKTREE_DIR);
        anyhow::ensure!(
            resolved.starts_with(&root),
            "path {} is outside daemon-owned worktree root {}",
            resolved.display(),
            root.display()
        );
        Ok(resolved)
    }

    async fn run_git_command(&self, source_root: &Path, args: &[&str]) -> Result<Vec<u8>> {
        let output = tokio::time::timeout(
            GIT_WORKTREE_TIMEOUT,
            Command::new("git")
                .arg("-C")
                .arg(source_root)
                .args(args)
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| anyhow!("git command timed out after 30s"))??;
        if output.status.success() {
            return Ok(output.stdout);
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            source_root.display(),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        )
    }

    async fn current_git_head(&self, source_root: &Path) -> Result<String> {
        let output = self
            .run_git_command(source_root, &["rev-parse", "HEAD"])
            .await?;
        let head = String::from_utf8(output)?.trim().to_string();
        anyhow::ensure!(
            !head.is_empty(),
            "git rev-parse HEAD returned an empty commit"
        );
        Ok(head)
    }

    async fn prepare_daemon_owned_worktree(
        &self,
        parent: &AgentId,
        spawn_receipt_key: &str,
        request: &mut SpawnSidechainRequest,
    ) -> Result<Option<DaemonOwnedWorktree>> {
        if request.fork_context.isolation.as_deref() != Some("worktree")
            || request.fork_context.worktree_path.is_some()
        {
            return Ok(None);
        }
        let source_root = self.canonical_workspace_root()?;
        let path = self.daemon_worktree_path(parent, spawn_receipt_key)?;
        let base_commit = self.current_git_head(&source_root).await?;
        request.fork_context.worktree_path = Some(path.display().to_string());
        Ok(Some(DaemonOwnedWorktree {
            path: path.display().to_string(),
            source_root: source_root.display().to_string(),
            base_commit,
        }))
    }

    async fn ensure_daemon_owned_git_worktree(
        &self,
        ownership: &DaemonOwnedWorktree,
    ) -> Result<()> {
        let path = self.reserved_daemon_worktree_path(Path::new(&ownership.path))?;
        let source_root = Path::new(&ownership.source_root);
        if path.join(".git").exists() {
            self.verify_daemon_owned_worktree(ownership).await?;
            return Ok(());
        }
        anyhow::ensure!(
            !path.exists(),
            "refusing to create daemon-owned worktree over existing non-worktree path: {}",
            path.display()
        );
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::create_dir(&path).await.with_context(|| {
            format!(
                "failed to reserve daemon-owned worktree path {}",
                path.display()
            )
        })?;
        let output = tokio::time::timeout(
            GIT_WORKTREE_TIMEOUT,
            Command::new("git")
                .arg("-C")
                .arg(source_root)
                .args(["worktree", "add", "--detach"])
                .arg(&path)
                .arg(&ownership.base_commit)
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| anyhow!("git worktree add timed out after 30s"))??;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let error = anyhow!(
                "git worktree add failed for {}: {}",
                path.display(),
                if stderr.is_empty() {
                    output.status.to_string()
                } else {
                    stderr
                }
            );
            if let Err(cleanup_error) = self
                .remove_failed_daemon_worktree_creation(source_root, &path)
                .await
            {
                return Err(anyhow!(
                    "git worktree add failed: {error}; cleanup also failed: {cleanup_error}"
                ));
            }
            return Err(error);
        }
        self.verify_daemon_owned_worktree(ownership).await
    }

    async fn remove_failed_daemon_worktree_creation(
        &self,
        source_root: &Path,
        path: &Path,
    ) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        if path.join(".git").exists() {
            let output = tokio::time::timeout(
                GIT_WORKTREE_TIMEOUT,
                Command::new("git")
                    .arg("-C")
                    .arg(source_root)
                    .args(["worktree", "remove", "--force"])
                    .arg(path)
                    .stdin(Stdio::null())
                    .output(),
            )
            .await
            .map_err(|_| anyhow!("git worktree remove timed out after 30s"))??;
            anyhow::ensure!(
                output.status.success(),
                "git worktree remove failed for {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return Ok(());
        }
        tokio::fs::remove_dir_all(path).await.map_err(Into::into)
    }

    pub(super) async fn verify_daemon_owned_worktree(
        &self,
        ownership: &DaemonOwnedWorktree,
    ) -> Result<()> {
        let path = self.reserved_daemon_worktree_path(Path::new(&ownership.path))?;
        let source_root = Path::new(&ownership.source_root);
        let output = self
            .run_git_command(source_root, &["worktree", "list", "--porcelain"])
            .await?;
        let list = String::from_utf8(output)?;
        let registered = list
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .filter_map(|value| std::fs::canonicalize(value).ok())
            .any(|candidate| candidate == path);
        anyhow::ensure!(
            registered,
            "daemon-owned worktree {} is not registered in source repo {}",
            ownership.path,
            source_root.display()
        );
        Ok(())
    }

    pub(super) async fn remove_daemon_owned_git_worktree(
        &self,
        ownership: &DaemonOwnedWorktree,
    ) -> Result<()> {
        let requested_path = Path::new(&ownership.path);
        if !requested_path.exists() {
            return Ok(());
        }
        let path = self.reserved_daemon_worktree_path(requested_path)?;
        anyhow::ensure!(
            path.join(".git").exists(),
            "refusing to remove daemon-owned worktree without .git marker: {}",
            path.display()
        );
        self.verify_daemon_owned_worktree(ownership).await?;
        let source_root = Path::new(&ownership.source_root);
        let output = tokio::time::timeout(
            GIT_WORKTREE_TIMEOUT,
            Command::new("git")
                .arg("-C")
                .arg(source_root)
                .args(["worktree", "remove", "--force"])
                .arg(&path)
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| anyhow!("git worktree remove timed out after 30s"))??;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        anyhow::bail!(
            "git worktree remove failed for {}: {}",
            path.display(),
            if stderr.is_empty() {
                output.status.to_string()
            } else {
                stderr
            }
        )
    }

    /// Removes daemon-owned worktrees left behind by a crash between an
    /// agent settling and its end-of-session cleanup. Only worktrees whose
    /// agent record is already settled or closed are reclaimed — a live
    /// (interruptible, resumable) sidechain keeps its worktree so a later
    /// continuation can reuse it. Best-effort: failures are logged, never
    /// fatal to startup.
    pub(crate) async fn gc_orphaned_daemon_worktrees_on_boot(&self) -> Result<()> {
        let Ok(workspace_root) = self.canonical_workspace_root() else {
            return Ok(());
        };
        let worktree_root = workspace_root.join(AGENT_WORKTREE_DIR);
        if !worktree_root.exists() {
            return Ok(());
        }
        let mut removed = 0usize;
        let mut kept = 0usize;
        for agent in self.supervisor.list() {
            let Some(ownership) = agent.daemon_owned_worktree.clone() else {
                continue;
            };
            let path = Path::new(&ownership.path);
            if !path.exists() {
                continue;
            }
            let settled = agent.settled_at_ms.is_some() || agent.closed_at_ms.is_some();
            if !settled {
                kept += 1;
                continue;
            }
            let reclaimed = match self.remove_daemon_owned_git_worktree(&ownership).await {
                Ok(()) => true,
                Err(remove_error) => {
                    // The registration may already be gone (e.g. a manual
                    // `git worktree prune`); fall back to the creation-rollback
                    // path, which handles both registered and bare directories
                    // under the reserved root.
                    match self.reserved_daemon_worktree_path(path) {
                        Ok(reserved) => {
                            match self
                                .remove_failed_daemon_worktree_creation(
                                    Path::new(&ownership.source_root),
                                    &reserved,
                                )
                                .await
                            {
                                Ok(()) => true,
                                Err(fallback_error) => {
                                    warn!(
                                        agent_id = %agent.id.0,
                                        path = %ownership.path,
                                        error = %remove_error,
                                        fallback_error = %fallback_error,
                                        "failed to reclaim orphaned daemon-owned worktree"
                                    );
                                    false
                                }
                            }
                        }
                        Err(error) => {
                            warn!(
                                agent_id = %agent.id.0,
                                path = %ownership.path,
                                error = %error,
                                "orphaned worktree path failed reservation check; leaving it"
                            );
                            false
                        }
                    }
                }
            };
            if reclaimed {
                removed += 1;
            }
        }
        // Drop git's administrative records for worktree directories that no
        // longer exist, so the source repo's worktree list stays accurate.
        if workspace_root.join(".git").exists()
            && let Err(error) = self
                .run_git_command(&workspace_root, &["worktree", "prune"])
                .await
        {
            warn!(error = %error, "git worktree prune failed during startup GC");
        }
        if removed > 0 || kept > 0 {
            tracing::info!(
                removed,
                live = kept,
                "daemon-owned worktree GC finished at startup"
            );
        }
        Ok(())
    }

    async fn rollback_pre_spawn_receipt(
        &self,
        spawn_receipt_key: Option<&str>,
        _error: &anyhow::Error,
    ) -> Result<()> {
        if let Some(spawn_receipt_key) = spawn_receipt_key {
            self.session_service
                .forget_sidechain_spawn_receipt(spawn_receipt_key)
                .await?;
        }
        Ok(())
    }

    async fn prepare_sidechain_subtask(
        &self,
        request: &SpawnSidechainRequest,
        conversation: &ConversationKey,
        access_session_id: &str,
    ) -> Result<Option<(SubtaskSpec, SubmitInputRequest)>> {
        let Some(subtask) = request.subtask.clone() else {
            return Ok(None);
        };
        let reply = ReplyHandle {
            plugin: "daemon".to_string(),
            address: conversation.session_id.clone(),
        };
        let envelope_request = SubmitInputRequest {
            provider: request.provider.clone(),
            source_plugin: Some("daemon".to_string()),
            source_kind: Some("subtask".to_string()),
            actor_id: Some("supervisor".to_string()),
            content: subtask.content.clone(),
            input_items: subtask.input_items.clone(),
            attachments: subtask.attachments.clone(),
            generation: request
                .fork_context
                .generation
                .clone()
                .or(Some(ModelGenerationConfig::default())),
            completion_requirements: None,
            metadata: Some(Value::Null),
            binding_keys: Vec::new(),
            reply_targets: vec![reply.clone()],
            reply_plugin: Some(reply.plugin.clone()),
            reply_address: Some(reply.address.clone()),
        };
        let input = self
            .build_input_envelope_with_board_access(
                &conversation.session_id,
                access_session_id,
                &envelope_request,
            )
            .await?;
        let (normalized_content, normalized_items) = match &input.payload {
            InputPayload::Rich { items, .. } => (
                String::new(),
                items
                    .iter()
                    .map(|item| match item {
                        kheish_types::ContentPart::Text { text } => {
                            SubmitInputItemRequest::Text { text: text.clone() }
                        }
                        kheish_types::ContentPart::Attachment { attachment } => {
                            SubmitInputItemRequest::AssetReference {
                                asset_id: attachment.id.clone(),
                            }
                        }
                    })
                    .collect(),
            ),
            InputPayload::Text { content } => (content.clone(), Vec::new()),
            InputPayload::Json { .. }
            | InputPayload::Event { .. }
            | InputPayload::Command { .. } => (String::new(), Vec::new()),
        };
        Ok(Some((
            SubtaskSpec {
                name: subtask.name,
                description: subtask.description,
                input: input.clone(),
            },
            SubmitInputRequest {
                provider: request.provider.clone(),
                source_plugin: Some(input.source.plugin.clone()),
                source_kind: Some(input.source.kind.clone()),
                actor_id: Some(input.actor.id.clone()),
                content: normalized_content,
                input_items: normalized_items,
                attachments: Vec::new(),
                generation: request
                    .fork_context
                    .generation
                    .clone()
                    .or(Some(ModelGenerationConfig::default())),
                completion_requirements: None,
                metadata: Some(input.metadata.clone()),
                binding_keys: Vec::new(),
                reply_targets: input.reply_targets.clone(),
                reply_plugin: input.reply.as_ref().map(|reply| reply.plugin.clone()),
                reply_address: input.reply.as_ref().map(|reply| reply.address.clone()),
            },
        )))
    }

    async fn adopt_reusable_sidechain(
        self: &Arc<Self>,
        record: kheish_agent::AgentRecord,
        requested_permission_mode: Option<Option<PermissionMode>>,
        inherited_permission_updates: Vec<kheish_types::HookPermissionUpdate>,
        capability_scope: Option<CapabilityScope>,
        credential_scope: Option<CredentialScope>,
        execution_identity: Option<kheish_types::SessionExecutionIdentity>,
    ) -> Result<SessionView> {
        self.apply_requested_session_permission_mode(
            &record.conversation.session_id,
            requested_permission_mode,
        )
        .await?;
        self.replace_session_permission_updates(
            &record.conversation.session_id,
            inherited_permission_updates,
        )
        .await?;
        if let Some(capability_scope) = capability_scope {
            self.session_service
                .save_session_capability_scope(&record.conversation.session_id, &capability_scope)
                .await?;
        }
        if let Some(credential_scope) = credential_scope {
            self.session_service
                .save_session_credential_scope(&record.conversation.session_id, &credential_scope)
                .await?;
        }
        if let Some(execution_identity) = execution_identity {
            self.save_session_execution_identity(
                &record.conversation.session_id,
                execution_identity,
            )
            .await?;
        }
        self.store.save_supervisor(&self.supervisor.snapshot())?;
        self.session_service
            .remember_session(&record.conversation.session_id, &record.id.0)
            .await?;
        self.session_view(&record.conversation.session_id, &record.id)
            .await
    }

    async fn rollback_failed_spawn(
        self: &Arc<Self>,
        agent_id: &AgentId,
        session_id: &str,
        spawn_receipt_key: Option<&str>,
        error: &anyhow::Error,
    ) -> Result<()> {
        let mut cleanup_error: Option<anyhow::Error> = None;
        let remember_error = |slot: &mut Option<anyhow::Error>, error: anyhow::Error| {
            if slot.is_none() {
                *slot = Some(error);
            }
        };
        let daemon_owned_worktree = self
            .supervisor
            .get(agent_id)
            .and_then(|record| record.daemon_owned_worktree);

        let _ = self.orchestrator.interrupt(agent_id).await;
        let _ = self.orchestrator.close_runtime(agent_id);
        let _ = self.supervisor.remove_agent(agent_id);
        self.clear_session_permission_memory(session_id);
        if let Err(error) = self.session_service.forget_session(session_id).await {
            remember_error(&mut cleanup_error, error);
        }
        if let Err(error) = self
            .session_service
            .forget_session_persona(session_id)
            .await
        {
            remember_error(&mut cleanup_error, error);
        }
        if let Err(error) = self.session_service.delete_session_file(session_id) {
            remember_error(&mut cleanup_error, error);
        }
        if let Some(spawn_receipt_key) = spawn_receipt_key {
            if let Err(error) = self
                .session_service
                .forget_sidechain_spawn_receipt(spawn_receipt_key)
                .await
            {
                remember_error(&mut cleanup_error, error);
            }
        }
        if let Err(error) = self.persist_topology().await {
            remember_error(&mut cleanup_error, error);
        }
        if let Some(ownership) = daemon_owned_worktree
            && let Err(error) = self.remove_daemon_owned_git_worktree(&ownership).await
        {
            remember_error(&mut cleanup_error, error);
        }
        if let Some(cleanup_error) = cleanup_error {
            return Err(Self::combined_spawn_failure(error, cleanup_error));
        }
        Ok(())
    }

    async fn cleanup_abandoned_pending_sidechain_spawn_receipt(
        self: &Arc<Self>,
        parent: &AgentId,
        spawn_request_id: Option<&str>,
        spawn_receipt_key: &str,
        session_id: &str,
        thread_id: Option<&str>,
        reason: &anyhow::Error,
    ) -> Result<()> {
        if let Some(record) = self.supervisor.list().into_iter().find(|record| {
            Self::sidechain_record_matches_receipt(
                record,
                parent,
                spawn_request_id,
                session_id,
                thread_id,
                true,
            )
        }) {
            return self
                .rollback_failed_spawn(
                    &record.id,
                    &record.conversation.session_id,
                    Some(spawn_receipt_key),
                    reason,
                )
                .await;
        }

        let indexed_owner = self.session_service.session_agent_id(session_id).await;
        let supervisor_owner = self.supervisor.list().into_iter().find(|record| {
            record.closed_at_ms.is_none() && record.conversation.session_id == session_id
        });
        if indexed_owner.is_none()
            && supervisor_owner.is_none()
            && !self.has_session_runs(session_id).await
        {
            self.session_service.forget_session(session_id).await?;
            let _ = self.session_service.delete_session_file(session_id);
        } else {
            warn!(
                receipt_key = %spawn_receipt_key,
                session_id = %session_id,
                indexed_owner = indexed_owner.as_deref(),
                supervisor_owner = supervisor_owner.as_ref().map(|record| record.id.0.as_str()),
                "discarding pending sidechain receipt without deleting an owned session"
            );
        }
        self.session_service
            .forget_sidechain_spawn_receipt(spawn_receipt_key)
            .await?;
        Ok(())
    }

    async fn submit_sidechain_initial_run(
        self: &Arc<Self>,
        session_id: &str,
        agent_id: &AgentId,
        run_request: SubmitInputRequest,
    ) -> Result<()> {
        let before_run_ids = self
            .list_runs(Some(session_id))
            .await?
            .into_iter()
            .map(|run| run.run_id)
            .collect::<BTreeSet<_>>();
        if let Err(error) = self.submit_input_run(session_id, run_request).await {
            let persisted_new_run = self
                .list_runs(Some(session_id))
                .await?
                .into_iter()
                .any(|run| !before_run_ids.contains(&run.run_id));
            if persisted_new_run {
                warn!(
                    session_id = %session_id,
                    agent_id = %agent_id.0,
                    error = %error,
                    "sidechain initial run submission returned an error after a run was persisted"
                );
                return Ok(());
            }
            return Err(error);
        }
        Ok(())
    }

    async fn reconcile_pending_sidechain_spawn(
        self: &Arc<Self>,
        parent: &AgentId,
        spawn_request_id: &str,
        spawn_receipt_key: &str,
        session_id: &str,
        thread_id: Option<&str>,
        bound_request_fingerprint: &str,
        requested_session_id: Option<&str>,
        requested_thread_id: Option<&str>,
        request_fingerprint: &str,
    ) -> Result<ConversationKey> {
        Self::ensure_sidechain_spawn_binding(
            requested_session_id,
            requested_thread_id,
            request_fingerprint,
            session_id,
            thread_id,
            bound_request_fingerprint,
        )?;
        self.cleanup_abandoned_pending_sidechain_spawn_receipt(
            parent,
            Some(spawn_request_id),
            spawn_receipt_key,
            session_id,
            thread_id,
            &anyhow!("sidechain spawn retry replaced an incomplete child"),
        )
        .await?;
        Ok(ConversationKey {
            session_id: session_id.to_string(),
            thread_id: thread_id.map(str::to_string),
        })
    }

    pub(crate) async fn reconcile_pending_sidechain_spawn_receipts_on_boot(
        self: &Arc<Self>,
    ) -> Result<()> {
        let receipts = self.session_service.sidechain_spawn_receipts().await;
        for (receipt_key, receipt) in receipts {
            let Some(parts) = Self::parse_sidechain_spawn_receipt_key(&receipt_key) else {
                warn!(
                    receipt_key = %receipt_key,
                    "discarding malformed sidechain spawn receipt during boot reconciliation"
                );
                self.session_service
                    .forget_sidechain_spawn_receipt(&receipt_key)
                    .await?;
                continue;
            };
            if let SidechainSpawnReceiptState::Pending {
                session_id,
                thread_id,
                ..
            } = receipt
            {
                self.cleanup_abandoned_pending_sidechain_spawn_receipt(
                    &parts.parent,
                    parts.spawn_request_id.as_deref(),
                    &receipt_key,
                    &session_id,
                    thread_id.as_deref(),
                    &anyhow!("sidechain spawn boot reconciliation removed an incomplete child"),
                )
                .await?;
                info!(
                    receipt_key = %receipt_key,
                    session_id = %session_id,
                    "cleaned pending sidechain spawn receipt during boot reconciliation"
                );
            }
        }
        Ok(())
    }

    pub(crate) async fn replay_committed_sidechain_spawn_receipts_on_boot(
        self: &Arc<Self>,
    ) -> Result<()> {
        let receipts = self.session_service.sidechain_spawn_receipts().await;
        for (receipt_key, receipt) in receipts {
            let Some(parts) = Self::parse_sidechain_spawn_receipt_key(&receipt_key) else {
                warn!(
                    receipt_key = %receipt_key,
                    "discarding malformed sidechain spawn receipt during boot replay"
                );
                self.session_service
                    .forget_sidechain_spawn_receipt(&receipt_key)
                    .await?;
                continue;
            };
            let SidechainSpawnReceiptState::Committed {
                agent_id,
                session_id,
                thread_id,
                subtask_request_json,
                ..
            } = receipt
            else {
                continue;
            };
            let agent_id = AgentId(agent_id);
            let Some(record) = self.supervisor.get(&agent_id) else {
                let indexed_owner = self.session_service.session_agent_id(&session_id).await;
                let supervisor_owner = self.supervisor.list().into_iter().find(|record| {
                    record.closed_at_ms.is_none() && record.conversation.session_id == session_id
                });
                if !self.has_session_runs(&session_id).await
                    && supervisor_owner.is_none()
                    && indexed_owner
                        .as_deref()
                        .is_none_or(|owner| owner == agent_id.0)
                {
                    self.clear_session_permission_memory(&session_id);
                    self.session_service.forget_session(&session_id).await?;
                    let _ = self.session_service.delete_session_file(&session_id);
                }
                self.session_service
                    .forget_sidechain_spawn_receipt(&receipt_key)
                    .await?;
                warn!(
                    receipt_key = %receipt_key,
                    session_id = %session_id,
                    "discarded sidechain spawn receipt pointing at an unknown child during boot replay"
                );
                continue;
            };
            anyhow::ensure!(
                Self::sidechain_record_matches_receipt(
                    &record,
                    &parts.parent,
                    parts.spawn_request_id.as_deref(),
                    &session_id,
                    thread_id.as_deref(),
                    false,
                ),
                "sidechain spawn receipt {receipt_key} is out of sync with supervisor state"
            );
            if record.closed_at_ms.is_some() {
                if parts.internal {
                    self.session_service
                        .forget_sidechain_spawn_receipt(&receipt_key)
                        .await?;
                }
                continue;
            }
            if let Some(subtask_request_json) = subtask_request_json
                && !self.has_session_runs(&session_id).await
            {
                let conversation_key =
                    Self::sidechain_spawn_conversation_key(&record.conversation)?;
                let _reservation_guard = SpawnReservationGuard::new(
                    self.as_ref(),
                    self.reserve_subagent_spawn(
                        &parts.parent,
                        record.spawned_by_run_id.as_deref(),
                        Some(&receipt_key),
                        &conversation_key,
                        false,
                    )?,
                );
                let run_request =
                    serde_json::from_str::<SubmitInputRequest>(&subtask_request_json)?;
                self.submit_sidechain_initial_run(&session_id, &record.id, run_request)
                    .await?;
                info!(
                    receipt_key = %receipt_key,
                    session_id = %session_id,
                    agent_id = %record.id.0,
                    "replayed missing sidechain initial run during boot reconciliation"
                );
            }
            if parts.internal {
                self.session_service
                    .forget_sidechain_spawn_receipt(&receipt_key)
                    .await?;
            }
        }
        Ok(())
    }

    async fn sidechain_from_committed_receipt(
        self: &Arc<Self>,
        receipt_key: &str,
        request: &SpawnSidechainRequest,
        subtask_request_json: Option<&str>,
        request_fingerprint: &str,
        parent: &AgentId,
        requested_session_id: Option<&str>,
        requested_thread_id: Option<&str>,
        agent_id: &str,
        session_id: &str,
        thread_id: Option<&str>,
        bound_request_fingerprint: &str,
        requested_permission_mode: Option<Option<PermissionMode>>,
        inherited_permission_updates: Vec<kheish_types::HookPermissionUpdate>,
    ) -> Result<SessionView> {
        Self::ensure_sidechain_spawn_binding(
            requested_session_id,
            requested_thread_id,
            request_fingerprint,
            session_id,
            thread_id,
            bound_request_fingerprint,
        )?;
        let agent_id = AgentId(agent_id.to_string());
        let record = self.supervisor.get(&agent_id).ok_or_else(|| {
            anyhow!(
                "spawn receipt {receipt_key} points to unknown child {}",
                agent_id.0
            )
        })?;
        anyhow::ensure!(
            record.parent.as_ref() == Some(parent)
                && record.spawn_request_id.as_deref() == request.spawn_request_id.as_deref()
                && record.conversation.session_id == session_id
                && record.conversation.thread_id.as_deref() == thread_id,
            "spawn receipt {receipt_key} is out of sync with supervisor state"
        );
        if record.closed_at_ms.is_some() {
            return self.session_view(session_id, &record.id).await;
        }
        let had_runs = self.has_session_runs(session_id).await;
        let view = self
            .adopt_reusable_sidechain(
                record,
                requested_permission_mode,
                inherited_permission_updates,
                None,
                None,
                None,
            )
            .await?;
        if !had_runs {
            let replay_request = if let Some(subtask_request_json) = subtask_request_json {
                Some(serde_json::from_str::<SubmitInputRequest>(
                    subtask_request_json,
                )?)
            } else {
                self.prepare_sidechain_subtask(
                    request,
                    &view.snapshot.agent.conversation,
                    &view.snapshot.agent.conversation.session_id,
                )
                .await?
                .map(|(_, run_request)| run_request)
            };
            if let Some(run_request) = replay_request {
                self.submit_sidechain_initial_run(session_id, &view.snapshot.agent.id, run_request)
                    .await?;
            }
        }
        self.session_view(session_id, &view.snapshot.agent.id).await
    }

    pub(crate) async fn spawn_sidechain(
        self: &Arc<Self>,
        parent_agent_id: &str,
        mut request: SpawnSidechainRequest,
    ) -> Result<SessionView> {
        let explicit_permission_mode_requested = request.permission_mode.is_some();
        let explicit_capability_scope_requested = request.capability_scope.is_some();
        let explicit_credential_scope_requested = request.credential_scope.is_some();
        let explicit_retention_requested = request.retention.is_some();
        let explicit_nickname_requested = request.nickname.is_some();
        let explicit_spawned_by_run_id_requested = request.spawned_by_run_id.is_some();
        let parsed_permission_mode = request
            .permission_mode
            .as_deref()
            .map(|value| {
                parse_permission_mode(value)
                    .ok_or_else(|| anyhow!("unknown permission_mode {value}"))
                    .map(Some)
            })
            .transpose()?;
        Self::normalize_spawn_sidechain_route_policy(&mut request)?;
        if let Some(subtask) = request.subtask.as_ref() {
            validate_submit_input_items(&subtask.input_items)?;
            validate_input_attachment_requests(&subtask.attachments)?;
            if !subtask.input_items.is_empty()
                && (!subtask.content.trim().is_empty() || !subtask.attachments.is_empty())
            {
                anyhow::bail!("content cannot be combined with input_items or attachments");
            }
            if subtask.input_items.is_empty()
                && subtask.content.trim().is_empty()
                && subtask.attachments.is_empty()
            {
                anyhow::bail!("content or attachments or input_items is required");
            }
        }
        let requested_session_id = request.session_id.clone();
        let requested_thread_id = request.thread_id.clone();
        let parent = AgentId(parent_agent_id.to_string());
        let parent_record = self
            .supervisor
            .get(&parent)
            .ok_or_else(|| anyhow!("unknown parent agent {}", parent.0))?;
        let parent_permission_mode = self
            .permissions
            .session_mode(&parent_record.conversation.session_id)
            .unwrap_or_else(|| self.permissions.mode());
        let requested_permission_mode =
            resolve_sidechain_permission_mode(&parent_permission_mode, parsed_permission_mode)?;
        if request.permission_mode.is_none()
            && let Some(Some(mode)) = requested_permission_mode.as_ref()
        {
            request.permission_mode = Some(permission_mode_name(mode).to_string());
        }
        let inherited_permission_updates = self
            .load_session_control_state(&parent_record.conversation.session_id)
            .await?
            .session_permission_updates;
        let inherited_persona = self
            .load_session_persona_binding(&parent_record.conversation.session_id)
            .await?;
        let inherited_capability_scope = self
            .load_session_capability_scope(&parent_record.conversation.session_id)
            .await?;
        let inherited_credential_scope = self
            .load_session_credential_scope(&parent_record.conversation.session_id)
            .await?;
        let effective_run_id = self
            .effective_spawn_run_id(&parent, request.spawned_by_run_id.as_deref())
            .await?;
        request.spawned_by_run_id = effective_run_id.clone();
        let user_spawn_receipt_key =
            Self::sidechain_spawn_receipt_key(&parent, request.spawn_request_id.as_deref());
        let _request_reservation_guard =
            if let Some(spawn_receipt_key) = user_spawn_receipt_key.as_deref() {
                Some(SpawnRequestReservationGuard::new(
                    self.as_ref(),
                    self.reserve_subagent_spawn_request(spawn_receipt_key)?,
                ))
            } else {
                None
            };
        let inherited_capability_scope = inherited_capability_scope.normalized();
        let inherited_credential_scope = inherited_credential_scope.normalized();
        let requested_capability_scope = request
            .capability_scope
            .clone()
            .unwrap_or_default()
            .normalized();
        let requested_credential_scope = request
            .credential_scope
            .clone()
            .unwrap_or_else(CredentialScope::deny_delegated_non_route_credentials)
            .normalized();
        validate_requested_capability_scope(
            &inherited_capability_scope,
            &requested_capability_scope,
        )?;
        validate_requested_credential_scope(
            &inherited_credential_scope,
            &requested_credential_scope,
        )?;
        let effective_capability_scope =
            inherited_capability_scope.restrict_with(&requested_capability_scope);
        let effective_credential_scope =
            inherited_credential_scope.restrict_with(&requested_credential_scope);
        request.capability_scope =
            (!effective_capability_scope.is_empty()).then_some(effective_capability_scope.clone());
        request.credential_scope =
            (!effective_credential_scope.is_empty()).then_some(effective_credential_scope.clone());
        request.fork_context.generation = merge_generation_override(
            request.fork_context.generation.take(),
            request.generation.take(),
        );
        Self::normalize_sidechain_legacy_provider(&mut request)?;
        let inherited_tool_surface = parent_record
            .fork_context
            .as_ref()
            .map(|fork_context| fork_context.tool_surface.clone())
            .unwrap_or_default()
            .normalized();
        let requested_tool_surface = request
            .fork_context
            .tool_surface
            .normalized()
            .restrict_with(&request.tool_surface.take().unwrap_or_default().normalized());
        validate_requested_tool_surface(&inherited_tool_surface, &requested_tool_surface)?;
        request.fork_context.tool_surface =
            inherited_tool_surface.restrict_with(&requested_tool_surface);
        if let Some(worktree_path) = request.fork_context.worktree_path.clone() {
            let normalized_worktree_path = bounded_workspace_root(
                &self.system_prompt.environment().workspace_root,
                Path::new(&worktree_path),
            )?;
            request.fork_context.worktree_path =
                Some(normalized_worktree_path.display().to_string());
        }
        let (resolved_provider, resolved_generation) = self.resolve_generation_route(
            request.provider.as_deref(),
            request.fork_context.generation.take(),
        )?;
        request.provider = resolved_provider.or(request.provider.take());
        request.fork_context.provider = request.provider.clone();
        request.fork_context.generation = resolved_generation;
        let existing_receipt = if let Some(spawn_receipt_key) = user_spawn_receipt_key.as_deref() {
            self.session_service
                .sidechain_spawn_receipt(spawn_receipt_key)
                .await
        } else {
            None
        };
        let conversation = match &existing_receipt {
            Some(SidechainSpawnReceiptState::Committed {
                session_id,
                thread_id,
                ..
            })
            | Some(SidechainSpawnReceiptState::Pending {
                session_id,
                thread_id,
                ..
            }) => ConversationKey {
                session_id: session_id.clone(),
                thread_id: thread_id.clone(),
            },
            None => ConversationKey {
                session_id: requested_session_id
                    .clone()
                    .unwrap_or_else(|| self.session_service.next_session_id()),
                thread_id: requested_thread_id.clone(),
            },
        };
        let spawn_receipt_key = user_spawn_receipt_key
            .clone()
            .unwrap_or_else(|| Self::internal_sidechain_spawn_receipt_key(&parent, &conversation));
        let spawn_receipt_is_internal = user_spawn_receipt_key.is_none();
        let daemon_owned_worktree = self
            .prepare_daemon_owned_worktree(&parent, &spawn_receipt_key, &mut request)
            .await?;
        let request_fingerprint =
            Self::sidechain_spawn_request_fingerprint(&request, &conversation)?;
        let policy_scope = self
            .sidechain_spawn_policy_scope(
                &parent,
                &parent_record.conversation.session_id,
                request.fork_context.team_name.clone(),
            )
            .await?;
        let policy_limits = self.subagent_policy.effective_limits(&policy_scope);
        let policy_estimate = Self::sidechain_spawn_policy_estimate(&request, &policy_limits);
        if let Some(SidechainSpawnReceiptState::Committed {
            agent_id,
            session_id,
            thread_id,
            request_fingerprint: bound_request_fingerprint,
            subtask_request_json,
            ..
        }) = existing_receipt.as_ref()
        {
            let _replay_guard = if self.has_session_runs(session_id).await {
                None
            } else {
                let conversation_key = Self::sidechain_spawn_conversation_key(&conversation)?;
                Some(SpawnReservationGuard::new(
                    self.as_ref(),
                    self.reserve_subagent_spawn_with_held_request(
                        &parent,
                        effective_run_id.as_deref(),
                        &spawn_receipt_key,
                        &conversation_key,
                        false,
                    )?,
                ))
            };
            return self
                .sidechain_from_committed_receipt(
                    &spawn_receipt_key,
                    &request,
                    subtask_request_json.as_deref(),
                    &request_fingerprint,
                    &parent,
                    requested_session_id.as_deref(),
                    requested_thread_id.as_deref(),
                    agent_id,
                    session_id,
                    thread_id.as_deref(),
                    bound_request_fingerprint,
                    requested_permission_mode.clone(),
                    inherited_permission_updates.clone(),
                )
                .await;
        }
        let mut pending_reconcile_guard = None;
        if let Some(SidechainSpawnReceiptState::Pending {
            session_id,
            thread_id,
            request_fingerprint: bound_request_fingerprint,
            ..
        }) = existing_receipt.as_ref()
        {
            pending_reconcile_guard = Some(SpawnReservationGuard::new(
                self.as_ref(),
                self.reserve_subagent_spawn_with_held_request(
                    &parent,
                    effective_run_id.as_deref(),
                    &spawn_receipt_key,
                    &Self::sidechain_spawn_conversation_key(&conversation)?,
                    false,
                )?,
            ));
            let conversation = self
                .reconcile_pending_sidechain_spawn(
                    &parent,
                    request
                        .spawn_request_id
                        .as_deref()
                        .expect("receipt key requires spawn_request_id"),
                    &spawn_receipt_key,
                    session_id,
                    thread_id.as_deref(),
                    bound_request_fingerprint,
                    requested_session_id.as_deref(),
                    requested_thread_id.as_deref(),
                    &request_fingerprint,
                )
                .await?;
            debug_assert_eq!(conversation.session_id, *session_id);
        }
        if user_spawn_receipt_key.is_none() {
            if let Some(existing) = self.reusable_sidechain_record(&parent, &conversation)? {
                Self::ensure_reusable_sidechain_matches(&request, &existing)?;
                self.ensure_implicit_sidechain_reuse_is_read_only(
                    &request,
                    &existing,
                    explicit_permission_mode_requested,
                    explicit_capability_scope_requested,
                    explicit_credential_scope_requested,
                    explicit_retention_requested,
                    explicit_nickname_requested,
                    explicit_spawned_by_run_id_requested,
                )
                .await?;
                return self
                    .session_view(&existing.conversation.session_id, &existing.id)
                    .await;
            }
        }
        let reservation_guard = match pending_reconcile_guard.take() {
            Some(guard) => guard,
            None => {
                let conversation_key = Self::sidechain_spawn_conversation_key(&conversation)?;
                let reservation = if user_spawn_receipt_key.is_some() {
                    self.reserve_subagent_spawn_with_policy_and_held_request(
                        &parent,
                        effective_run_id.as_deref(),
                        &spawn_receipt_key,
                        &conversation_key,
                        existing_receipt.is_none(),
                        &policy_scope,
                        &policy_estimate,
                        &request_fingerprint,
                    )?
                } else {
                    self.reserve_subagent_spawn_with_policy(
                        &parent,
                        effective_run_id.as_deref(),
                        Some(&spawn_receipt_key),
                        &conversation_key,
                        existing_receipt.is_none(),
                        &policy_scope,
                        &policy_estimate,
                        &request_fingerprint,
                    )?
                };
                SpawnReservationGuard::new(self.as_ref(), reservation)
            }
        };
        if user_spawn_receipt_key.is_some()
            && self
                .reusable_sidechain_record(&parent, &conversation)?
                .is_some()
        {
            anyhow::bail!(
                "spawn_request_id requires a matching receipt for an existing child session"
            );
        }
        let prepared_subtask = self
            .prepare_sidechain_subtask(
                &request,
                &conversation,
                &parent_record.conversation.session_id,
            )
            .await?;
        let prepared_subtask_request_json = prepared_subtask
            .as_ref()
            .map(|(_, run_request)| serde_json::to_string(run_request))
            .transpose()?;
        debug!(
            parent_agent_id = %parent_agent_id,
            requested_session_id = request.session_id.as_deref(),
            requested_name = request.subtask.as_ref().map(|subtask| subtask.name.as_str()),
            retention = ?request.retention.clone().unwrap_or(ChildRetentionPolicy::Retain),
            spawn_request_id = request.spawn_request_id.as_deref(),
            spawned_by_run_id = effective_run_id.as_deref(),
            "processing sidechain spawn request"
        );
        self.session_service
            .remember_sidechain_spawn_receipt(
                &spawn_receipt_key,
                SidechainSpawnReceiptState::Pending {
                    session_id: conversation.session_id.clone(),
                    thread_id: conversation.thread_id.clone(),
                    request_fingerprint: request_fingerprint.clone(),
                    subtask_request_json: prepared_subtask_request_json.clone(),
                    recorded_at_ms: now_ms(),
                },
            )
            .await?;
        let start_hook = self
            .dispatch_daemon_hook(
                HookEventName::SubagentStart,
                Some(
                    request
                        .fork_context
                        .team_name
                        .clone()
                        .unwrap_or_else(|| "sidechain".to_string()),
                ),
                Some(conversation.session_id.clone()),
                Some(parent_agent_id.to_string()),
                None,
                json!({
                    "parent_agent_id": parent_agent_id,
                    "session_id": conversation.session_id,
                    "thread_id": conversation.thread_id,
                    "provider": request.provider,
                    "generation": request.fork_context.generation,
                    "tool_surface": request.fork_context.tool_surface,
                    "capability_scope": request.capability_scope,
                    "credential_scope": request.credential_scope,
                    "subtask": request.subtask.as_ref().map(|subtask| json!({
                        "name": subtask.name,
                        "description": subtask.description,
                        "content": subtask.content,
                        "input_items": summarize_subtask_input_items(subtask),
                    })),
                }),
            )
            .await;
        let start_hook = match start_hook {
            Ok(start_hook) => start_hook,
            Err(error) => {
                if let Err(cleanup_error) = self
                    .rollback_pre_spawn_receipt(Some(&spawn_receipt_key), &error)
                    .await
                {
                    return Err(Self::combined_spawn_failure(&error, cleanup_error));
                }
                return Err(error);
            }
        };
        if matches!(start_hook.decision, Some(kheish_types::HookDecision::Block))
            || !start_hook.continue_execution
        {
            let reason = start_hook
                .stop_reason
                .unwrap_or_else(|| "subagent start blocked by hook".to_string());
            let error: anyhow::Error =
                kheish_runtime::HookBlockedError::new(HookEventName::SubagentStart, reason).into();
            if let Err(cleanup_error) = self
                .rollback_pre_spawn_receipt(Some(&spawn_receipt_key), &error)
                .await
            {
                return Err(Self::combined_spawn_failure(&error, cleanup_error));
            }
            return Err(error);
        }
        if let Some(worktree_path) = request.fork_context.worktree_path.clone() {
            let worktree_hook = self
                .dispatch_daemon_hook(
                    HookEventName::WorktreeCreate,
                    Some(worktree_path),
                    Some(conversation.session_id.clone()),
                    Some(parent_agent_id.to_string()),
                    None,
                    json!({
                        "parent_agent_id": parent_agent_id,
                        "session_id": conversation.session_id,
                        "thread_id": conversation.thread_id,
                        "worktree_path": request.fork_context.worktree_path.clone(),
                    }),
                )
                .await;
            let worktree_hook = match worktree_hook {
                Ok(worktree_hook) => worktree_hook,
                Err(error) => {
                    if let Err(cleanup_error) = self
                        .rollback_pre_spawn_receipt(Some(&spawn_receipt_key), &error)
                        .await
                    {
                        return Err(Self::combined_spawn_failure(&error, cleanup_error));
                    }
                    return Err(error);
                }
            };
            if matches!(
                worktree_hook.decision,
                Some(kheish_types::HookDecision::Block)
            ) || !worktree_hook.continue_execution
            {
                let reason = worktree_hook
                    .stop_reason
                    .unwrap_or_else(|| "worktree creation blocked by hook".to_string());
                let error: anyhow::Error =
                    kheish_runtime::HookBlockedError::new(HookEventName::WorktreeCreate, reason)
                        .into();
                if let Err(cleanup_error) = self
                    .rollback_pre_spawn_receipt(Some(&spawn_receipt_key), &error)
                    .await
                {
                    return Err(Self::combined_spawn_failure(&error, cleanup_error));
                }
                return Err(error);
            }
        }
        if let Some(ownership) = daemon_owned_worktree.as_ref()
            && let Err(error) = self.ensure_daemon_owned_git_worktree(ownership).await
        {
            if let Err(cleanup_error) = self
                .rollback_pre_spawn_receipt(Some(&spawn_receipt_key), &error)
                .await
            {
                return Err(Self::combined_spawn_failure(&error, cleanup_error));
            }
            return Err(error);
        }
        let requested_name = prepared_subtask
            .as_ref()
            .map(|(subtask, _)| subtask.name.clone());
        let retention = request
            .retention
            .clone()
            .unwrap_or(ChildRetentionPolicy::Retain);
        let nickname = request.nickname.clone();
        let spawn_request_id = request.spawn_request_id.clone();
        let spawned_by_run_id = request.spawned_by_run_id.clone();
        let child_route_policy = SessionRoutePolicy {
            provider: request.provider.clone(),
            generation: request.fork_context.generation.clone(),
        };
        let snapshot = match self
            .orchestrator
            .spawn_sidechain(
                &parent,
                conversation.clone(),
                request.fork_context,
                requested_name,
                nickname,
                retention,
                spawned_by_run_id.clone(),
                spawn_request_id,
                daemon_owned_worktree.clone(),
                prepared_subtask
                    .as_ref()
                    .map(|(subtask, _)| subtask.clone()),
            )
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let receipt_cleanup = self
                    .session_service
                    .forget_sidechain_spawn_receipt(&spawn_receipt_key)
                    .await;
                let worktree_cleanup = if let Some(ownership) = daemon_owned_worktree.as_ref() {
                    self.remove_daemon_owned_git_worktree(ownership).await
                } else {
                    Ok(())
                };
                if let Err(cleanup_error) = receipt_cleanup {
                    return Err(Self::combined_spawn_failure(&error, cleanup_error));
                }
                if let Err(cleanup_error) = worktree_cleanup {
                    return Err(Self::combined_spawn_failure(&error, cleanup_error));
                }
                return Err(error);
            }
        };
        if let Err(error) = self.store.save_supervisor(&self.supervisor.snapshot()) {
            if let Err(cleanup_error) = self
                .rollback_failed_spawn(
                    &snapshot.agent.id,
                    &conversation.session_id,
                    Some(&spawn_receipt_key),
                    &error,
                )
                .await
            {
                return Err(Self::combined_spawn_failure(&error, cleanup_error));
            }
            return Err(error);
        }
        if !child_route_policy.is_empty() {
            if let Err(error) = self
                .save_session_route_policy(&conversation.session_id, child_route_policy)
                .await
            {
                if let Err(cleanup_error) = self
                    .rollback_failed_spawn(
                        &snapshot.agent.id,
                        &conversation.session_id,
                        Some(&spawn_receipt_key),
                        &error,
                    )
                    .await
                {
                    return Err(Self::combined_spawn_failure(&error, cleanup_error));
                }
                return Err(error);
            }
        }
        if let Err(error) = self
            .apply_requested_session_permission_mode(
                &conversation.session_id,
                requested_permission_mode.clone(),
            )
            .await
        {
            if let Err(cleanup_error) = self
                .rollback_failed_spawn(
                    &snapshot.agent.id,
                    &conversation.session_id,
                    Some(&spawn_receipt_key),
                    &error,
                )
                .await
            {
                return Err(Self::combined_spawn_failure(&error, cleanup_error));
            }
            return Err(error);
        }
        if let Err(error) = self
            .replace_session_permission_updates(
                &conversation.session_id,
                inherited_permission_updates.clone(),
            )
            .await
        {
            if let Err(cleanup_error) = self
                .rollback_failed_spawn(
                    &snapshot.agent.id,
                    &conversation.session_id,
                    Some(&spawn_receipt_key),
                    &error,
                )
                .await
            {
                return Err(Self::combined_spawn_failure(&error, cleanup_error));
            }
            return Err(error);
        }
        if let Err(error) = self
            .session_service
            .remember_session(&conversation.session_id, &snapshot.agent.id.0)
            .await
        {
            if let Err(cleanup_error) = self
                .rollback_failed_spawn(
                    &snapshot.agent.id,
                    &conversation.session_id,
                    Some(&spawn_receipt_key),
                    &error,
                )
                .await
            {
                return Err(Self::combined_spawn_failure(&error, cleanup_error));
            }
            return Err(error);
        }
        if let Some(binding) = inherited_persona.as_ref() {
            if let Err(error) = self
                .session_service
                .save_session_persona_binding(&conversation.session_id, Some(binding))
                .await
            {
                if let Err(cleanup_error) = self
                    .rollback_failed_spawn(
                        &snapshot.agent.id,
                        &conversation.session_id,
                        Some(&spawn_receipt_key),
                        &error,
                    )
                    .await
                {
                    return Err(Self::combined_spawn_failure(&error, cleanup_error));
                }
                return Err(error);
            }
            if let Err(error) = self
                .session_service
                .remember_session_persona(&conversation.session_id, &binding.persona_id)
                .await
            {
                if let Err(cleanup_error) = self
                    .rollback_failed_spawn(
                        &snapshot.agent.id,
                        &conversation.session_id,
                        Some(&spawn_receipt_key),
                        &error,
                    )
                    .await
                {
                    return Err(Self::combined_spawn_failure(&error, cleanup_error));
                }
                return Err(error);
            }
        }
        if !effective_capability_scope.is_empty()
            && let Err(error) = self
                .session_service
                .save_session_capability_scope(
                    &conversation.session_id,
                    &effective_capability_scope,
                )
                .await
        {
            if let Err(cleanup_error) = self
                .rollback_failed_spawn(
                    &snapshot.agent.id,
                    &conversation.session_id,
                    Some(&spawn_receipt_key),
                    &error,
                )
                .await
            {
                return Err(Self::combined_spawn_failure(&error, cleanup_error));
            }
            return Err(error);
        }
        if !effective_credential_scope.is_empty()
            && let Err(error) = self
                .session_service
                .save_session_credential_scope(
                    &conversation.session_id,
                    &effective_credential_scope,
                )
                .await
        {
            if let Err(cleanup_error) = self
                .rollback_failed_spawn(
                    &snapshot.agent.id,
                    &conversation.session_id,
                    Some(&spawn_receipt_key),
                    &error,
                )
                .await
            {
                return Err(Self::combined_spawn_failure(&error, cleanup_error));
            }
            return Err(error);
        }
        let execution_identity = Self::sidechain_execution_identity(
            &parent,
            &snapshot.agent.id,
            request.spawn_request_id.as_deref(),
            spawned_by_run_id.as_deref(),
        );
        if !execution_identity.is_empty()
            && let Err(error) = self
                .save_session_execution_identity(&conversation.session_id, execution_identity)
                .await
        {
            if let Err(cleanup_error) = self
                .rollback_failed_spawn(
                    &snapshot.agent.id,
                    &conversation.session_id,
                    Some(&spawn_receipt_key),
                    &error,
                )
                .await
            {
                return Err(Self::combined_spawn_failure(&error, cleanup_error));
            }
            return Err(error);
        }
        if let Err(error) = self
            .session_service
            .remember_sidechain_spawn_receipt(
                &spawn_receipt_key,
                SidechainSpawnReceiptState::Committed {
                    agent_id: snapshot.agent.id.0.clone(),
                    session_id: conversation.session_id.clone(),
                    thread_id: conversation.thread_id.clone(),
                    request_fingerprint: request_fingerprint.clone(),
                    subtask_request_json: prepared_subtask_request_json.clone(),
                    recorded_at_ms: now_ms(),
                },
            )
            .await
        {
            if let Err(cleanup_error) = self
                .rollback_failed_spawn(
                    &snapshot.agent.id,
                    &conversation.session_id,
                    Some(&spawn_receipt_key),
                    &error,
                )
                .await
            {
                return Err(Self::combined_spawn_failure(&error, cleanup_error));
            }
            return Err(error);
        }
        if let Some((_, run_request)) = prepared_subtask {
            if let Err(error) = self
                .submit_sidechain_initial_run(
                    &conversation.session_id,
                    &snapshot.agent.id,
                    run_request,
                )
                .await
            {
                if let Err(cleanup_error) = self
                    .rollback_failed_spawn(
                        &snapshot.agent.id,
                        &conversation.session_id,
                        Some(&spawn_receipt_key),
                        &error,
                    )
                    .await
                {
                    return Err(Self::combined_spawn_failure(&error, cleanup_error));
                }
                return Err(error);
            }
        }
        if spawn_receipt_is_internal
            && let Err(error) = self
                .session_service
                .forget_sidechain_spawn_receipt(&spawn_receipt_key)
                .await
        {
            warn!(
                receipt_key = %spawn_receipt_key,
                session_id = %conversation.session_id,
                error = %error,
                "failed to remove completed internal sidechain spawn receipt"
            );
        }
        drop(reservation_guard);
        let view = self
            .session_view(&conversation.session_id, &snapshot.agent.id)
            .await?;
        info!(
            parent_agent_id = %parent_agent_id,
            child_agent_id = %view.agent_id,
            child_session_id = %view.session_id,
            path = view.snapshot.agent.path.as_deref(),
            retention = ?view.snapshot.agent.retention,
            spawn_request_id = view.snapshot.agent.spawn_request_id.as_deref(),
            spawned_by_run_id = view.snapshot.agent.spawned_by_run_id.as_deref(),
            "spawned daemon sidechain"
        );
        self.bind_sidechain_to_channel_thread_from_run(
            &view.agent_id,
            view.snapshot.agent.spawned_by_run_id.as_deref(),
        )
        .await?;
        self.publish_snapshot(&view);
        Ok(view)
    }

    pub(crate) async fn explain_sidechain_spawn(
        self: &Arc<Self>,
        parent_agent_id: &str,
        mut request: SpawnSidechainRequest,
    ) -> Result<crate::SubagentPolicyDecisionView> {
        let parsed_permission_mode = request
            .permission_mode
            .as_deref()
            .map(|value| {
                parse_permission_mode(value)
                    .ok_or_else(|| anyhow!("unknown permission_mode {value}"))
                    .map(Some)
            })
            .transpose()?;
        Self::normalize_spawn_sidechain_route_policy(&mut request)?;
        if let Some(subtask) = request.subtask.as_ref() {
            validate_submit_input_items(&subtask.input_items)?;
            validate_input_attachment_requests(&subtask.attachments)?;
            if !subtask.input_items.is_empty()
                && (!subtask.content.trim().is_empty() || !subtask.attachments.is_empty())
            {
                anyhow::bail!("content cannot be combined with input_items or attachments");
            }
            if subtask.input_items.is_empty()
                && subtask.content.trim().is_empty()
                && subtask.attachments.is_empty()
            {
                anyhow::bail!("content or attachments or input_items is required");
            }
        }
        let requested_session_id = request.session_id.clone();
        let requested_thread_id = request.thread_id.clone();
        let parent = AgentId(parent_agent_id.to_string());
        let parent_record = self
            .supervisor
            .get(&parent)
            .ok_or_else(|| anyhow!("unknown parent agent {}", parent.0))?;
        let parent_permission_mode = self
            .permissions
            .session_mode(&parent_record.conversation.session_id)
            .unwrap_or_else(|| self.permissions.mode());
        let requested_permission_mode =
            resolve_sidechain_permission_mode(&parent_permission_mode, parsed_permission_mode)?;
        if request.permission_mode.is_none()
            && let Some(Some(mode)) = requested_permission_mode.as_ref()
        {
            request.permission_mode = Some(permission_mode_name(mode).to_string());
        }
        let effective_run_id = self
            .effective_spawn_run_id(&parent, request.spawned_by_run_id.as_deref())
            .await?;
        request.spawned_by_run_id = effective_run_id.clone();
        request.fork_context.generation = merge_generation_override(
            request.fork_context.generation.take(),
            request.generation.take(),
        );
        Self::normalize_sidechain_legacy_provider(&mut request)?;
        let (resolved_provider, resolved_generation) = self.resolve_generation_route(
            request.provider.as_deref(),
            request.fork_context.generation.take(),
        )?;
        request.provider = resolved_provider.or(request.provider.take());
        request.fork_context.provider = request.provider.clone();
        request.fork_context.generation = resolved_generation;
        let spawn_receipt_key =
            Self::sidechain_spawn_receipt_key(&parent, request.spawn_request_id.as_deref());
        let existing_receipt = if let Some(spawn_receipt_key) = spawn_receipt_key.as_deref() {
            self.session_service
                .sidechain_spawn_receipt(spawn_receipt_key)
                .await
        } else {
            None
        };
        let conversation = match &existing_receipt {
            Some(SidechainSpawnReceiptState::Committed {
                session_id,
                thread_id,
                ..
            })
            | Some(SidechainSpawnReceiptState::Pending {
                session_id,
                thread_id,
                ..
            }) => ConversationKey {
                session_id: session_id.clone(),
                thread_id: thread_id.clone(),
            },
            None => ConversationKey {
                session_id: requested_session_id
                    .clone()
                    .unwrap_or_else(|| format!("dry-run-sidechain:{}", parent.0)),
                thread_id: requested_thread_id.clone(),
            },
        };
        let request_fingerprint =
            Self::sidechain_spawn_request_fingerprint(&request, &conversation)?;
        let policy_scope = self
            .sidechain_spawn_policy_scope(
                &parent,
                &parent_record.conversation.session_id,
                request.fork_context.team_name.clone(),
            )
            .await?;
        let policy_limits = self.subagent_policy.effective_limits(&policy_scope);
        let policy_estimate = Self::sidechain_spawn_policy_estimate(&request, &policy_limits);
        Ok(self.subagent_service.explain_subagent_spawn_policy(
            &self.supervisor,
            &parent,
            effective_run_id.as_deref(),
            spawn_receipt_key.as_deref(),
            &Self::sidechain_spawn_conversation_key(&conversation)?,
            &self.subagent_policy,
            &policy_scope,
            &policy_estimate,
            &request_fingerprint,
            existing_receipt.is_some(),
            |agent_id| self.orchestrator.has_runtime(agent_id),
            |run_id| self.count_spawned_children_for_run(run_id),
        ))
    }
}
